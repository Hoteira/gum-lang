# gum language specification

This is the semantic reference for gum. It describes what a gum program
*means* (types, evaluation, storage, safety), not just what parses. Where
behavior is defined by matching Solidity, that is stated explicitly and is
enforced by the differential execution tests.

Grammar of record: [`gumc/src/parser/gum.pest`](gumc/src/parser/gum.pest).
Storage layout details live in [STORAGE.md](STORAGE.md).

---

## 1. Lexical structure

- **Comments** start with `//` and run to end of line.
- **Identifiers** match `[A-Za-z_][A-Za-z0-9_]*` and are not keywords.
- **Naming convention is load-bearing:** *types* start with an uppercase
  letter (`Account`, `Vec`, `TokenState`); *values* (locals, params, fields,
  functions) are lowercase/snake_case. The parser relies on this to
  distinguish a constructor call `new Counter(start)` from a generic
  instantiation `new Vec(u256)`. Do not name a value with a leading capital.
- **Number literals** are decimal (`42`) or hex (`0x2a`). All integer literals
  are typed `u256` until assigned/coerced to a narrower type.
- **String literals** `"..."` are `String` values (length-prefixed, ABI-encoded as `string`).
- **f-strings** `f"balance = {x}"` interpolate `{expr}` spans.

### Blocks (indentation)

gum uses Python-style layout. A line ending in `:` opens a block; the
following more-indented lines are its body. A preprocessor
([`indent.rs`](gumc/src/indent.rs)) converts this to the brace form the grammar
consumes, one output line per source line (so error line numbers are exact).

```python
contract C:
    export fn f(u256 x) -> u256:
        if x > 0:
            return x
        return 0
```

Keywords: `use export interface extern contract once payable fn class enum
error const mut var assert revert match for in if else return while unsafe
call`.

### Declarations and visibility

| Keyword | Meaning |
|---------|---------|
| `contract Name:` | storage singleton (§5) |
| `interface Name:` | external interface (§4); `extern class Name:` also accepted |
| `export fn` | public, externally-callable entry point (§6) |
| `fn` (bare) | internal function, no ABI selector, callable only from other gum functions |
| `class Name:` | struct / value type |

A bare `fn` is **not** reachable by a raw on-chain call; only `export`ed
functions get a dispatcher case and appear in the ABI.

### Type-inferred locals

| Form | Meaning |
|------|---------|
| `var x = expr` | inferred type, **immutable** (default) |
| `mut var x = expr` | inferred type, mutable |
| `const x = expr` | inferred type, immutable (explicit synonym of bare `var`) |

Inferred locals are immutable by default; mutability is opt-in via `mut`. All
inferred forms take the variable's type from the initializer, so they always
require one (`var x` with no `=` is a parse error). The type-first forms
(`u256 x = 5`, `mut u256 x`, `const u256 x = 5`) remain available.

---

## 2. Program structure

A program is a sequence of top-level declarations:

- `use`: module import (§11)
- `enum`: sum type (§9)
- `error`: custom error type (§8)
- `class`: struct value type (§4)
- `contract`: storage singleton (§4/§5)
- `interface` (or `extern class`): external interface (§4)
- `fn` / `export fn`: internal / public function (§6)

---

## 3. Types

| category | types |
|---|---|
| unsigned int | `u8 u16 u32 u64 u128 u256` |
| signed int | `i8 i16 i32 i64 i128 i256` |
| fixed-point | `f32 f64` (full-width WAD-style fixed values, **not** IEEE floats) |
| boolean | `bool` |
| address | `Account` (an EVM 160-bit address) |
| array | `[T]` (dynamic), `[T; N]` (fixed) |
| mapping | `HashMap(K, V)` |
| generic | `Vec(T)`, `HashMap(K, V)`, and user generics |
| user | any declared `class` (struct) or `enum` |

### The EVM reality of integer widths

The EVM has exactly **one** native numeric width: 256 bits. gum's narrower
types (`u8`, `i32`, …) are a *compiler-enforced convention*, not a hardware
feature. A value is kept masked to its declared width at trust boundaries:

- **unsigned** narrowing masks with `and(v, 2^bits − 1)`;
- **signed** narrowing uses `SIGNEXTEND` (a plain bitmask would destroy the
  sign of a negative value).

Masking happens on function entry (decoding calldata), on assignment to a
narrower type, and on return.

`Account` is masked to its low 160 bits when it enters from calldata, so
equality tests, storage keys, and external-call targets all see a canonical
value.

---

## 4. Classes

`class` declares one of three things depending on modifiers:

### Struct (plain `class`)

A value type with fields. Lives in **memory** when used as a local/return, or
in **storage** when used as a `contract` field or a mapping value. Fields
are size-ordered and packed (see STORAGE.md).

```python
class Point:
    u256 x
    u256 y
```

### Storage singleton (`contract`)

The contract's persistent state. Its fields occupy storage slots. There is
exactly one instance; you refer to it by the class name:

```python
contract Bank:
    u256 total
    HashMap(Account, u256) balances

    export fn deposit(u256 v):
        Bank.total = Bank.total + v
```

A contract's **entry points are its `export fn` members**: they live inside the
`contract` block, alongside the state they act on. A top-level `export fn` is a
compile error: an entry point with no contract has no storage to belong to, and
no object to be dispatched from.

A bare top-level `fn` is still allowed: it is an internal helper, shared by any
contract in the file.

Because entry points are scoped to their contract, **a file may declare more
than one contract**; each compiles to its own object and bytecode, and each gets
its **own storage starting at slot 0**: two contracts in one file are two
deployments, no more related than two in different files.

### Deploying a contract (`new`)

`new` means different things for the two kinds of class, because they are
different kinds of thing:

| | `new` does | evaluates to |
|---|---|---|
| plain `class` | allocates it in memory | the value |
| `contract` | **deploys it** (`CREATE`) | its `Account` address |

```python
contract Child:
    u256 v

    fn new(u256 x):
        self.v = x

contract Factory:
    Account last

    export fn make(u256 x) -> Account:
        Factory.last = new Child(x)      # a real deployment
        return Factory.last
```

The child's creation code is embedded in the deployer as a Yul sub-object, so
the factory needs no help from its caller. The child's constructor runs with the
factory as `Message.sender()`, its arguments are ABI-encoded and appended to
that code, and if it reverts, its reason is bubbled up rather than surfacing as
an anonymous failure. (`CREATE` reports failure as address 0, not a revert; gum
checks that for you, so a failed deploy can never be mistaken for an address.)

Constructor arguments may be `String`/`Bytes` as well as scalars. They are
encoded head/tail exactly as the child's decoder expects, so
`new Token("Gum Token", supply, "GUM")` works:

```python
contract Token:
    String name
    u256 supply

    fn new(String n, u256 s):
        self.name = n
        self.supply = s
```

Constructor arguments may also be arrays, `[T]` or `[T; N]`, encoded the same
way, see §7a.

**Deployment cycles** (A deploys B, B deploys A) are a compile error: each one's
creation code contains the other's, so the bytecode would have no fixed point.
Deploy one from outside and pass its address in.

For a contract whose code you don't have at compile time (a proxy, an
EIP-1167 clone), use the raw-bytecode primitives instead:
`Account.create(code, value)`, `Account.create2(code, value, salt)`, and
`Account.create2_address(code, salt)` to know the address beforehand.

### External interface (`extern class`)

A class with method signatures but no bodies. They belong to *another*
deployed contract. Calling a method compiles to an EVM `CALL`:

```python
extern class IERC20:
    fn transfer(Account to, u256 amount) -> bool

# IERC20(token).transfer(to, amt)  →  external CALL with selector transfer(address,uint256)
```

Arguments are encoded, and the result decoded, from the *declared* signature,
so a `String`, `Bytes`, `T[]`, `P`, or `[P]` crosses head/tail exactly as the
callee expects rather than as a bare memory pointer, in either direction:

```python
extern class IERC20:
    fn name() -> String        # decoded back into a real String, not its offset word
    fn transfer(Account to, u256 amount) -> bool
```

Arguments share the encoder `new Child(...)` uses, and results share the
decoders that read constructor arguments, so neither pair can drift apart. A
scalar return stays on a one-word fast path and costs nothing extra.

Methods (`fn` inside a class) take an implicit `self`. A `fn new(...)` is a
constructor invoked by `new ClassName(args)`.

### Inheritance (`[Parent]`)

Parents are an attribute **above** the declaration, the way Rust writes
`#[derive(...)]`:

```python
[Parent]
class Child:
```

That gives Child a copy of Parent's fields and methods. It is resolved by
flattening at compile time: there is no vtable, no dynamic dispatch, and no
runtime cost. There is no trailing form; `class Child [Parent]:` is a syntax
error.

```python
class Ledger:
    u256 total

    fn credit(u256 v):
        self.total = self.total + v

    fn cap() -> u256:
        return 100

[Ledger]
contract Bank:
    u256 fee

    fn cap() -> u256:      # overrides Ledger.cap()
        return 250
```

* **Fields**: the parent's come first, then the child's own. Appending a field
  to a child can therefore never move an inherited one to a different slot.
  This is also the order Solidity lays out inherited state, so an inheriting
  `contract` matches its Solidity equivalent slot-for-slot.
* **Methods**: inherited unless the child declares one of the same name, which
  overrides it. `new` is inherited like any other method.
* **Transitive**: a parent is fully resolved before its children, so a chain
  (`[Mid] contract C`, `[Base] class Mid`) inherits from the whole chain.
* **Multiple parents** are allowed: `[A, B]`.

Errors, all at compile time: an unknown parent; an inheritance cycle;
re-declaring an inherited *field* (shadowing would silently give it a second
slot); inheriting the same method from two parents without the child declaring
which; and inheriting from a `contract` or from a generic class.

`super.method()` calls the parent's version from an override, so a child can
extend rather than only replace. It is a compile error where there is nothing to
call: outside a method, or on a method the parent does not declare.

### Interfaces as parents (`implements`)

When the parent is an `interface`, `[...]` means **implements** rather than
inherits. The interface contributes nothing; it only obliges the child to
define every method it declares, with a matching signature:

```python
interface IThing:
    fn ping(u256 x) -> bool

[IThing]
class Impl:               # compile error unless Impl defines ping(u256) -> bool
    fn ping(u256 x) -> bool:
        return true
```

Marker parents (`Serializable`, `Hashable`) are just classes with no fields or
methods, so they inherit nothing and oblige nothing. They propagate down a
chain: if A has `[Serializable]` and B has `[A]`, then B is serializable too.

---

## 5. Declarations, modifiers, mutability

| modifier | on | meaning |
|---|---|---|
| `export` | `fn` | a **public** external entry point (has an ABI selector, dispatched) |
| `payable` | `fn` | may receive ETH; read the amount with `Message.value()` |
| `once` | `fn` | one-time function: reverts on any call after the first |
| `const` | var/field | immutable binding; reassignment is a compile error |
| `mut` | var/param/field | explicitly mutable (default for locals) |

(`contract` and `interface`/`extern class` are declaration keywords, not `fn`
modifiers, see §4.)

- **Visibility is binary and enforced.** An `export fn` is external (selector +
  ABI entry). A bare `fn` is **internal**: no selector, unreachable by a raw
  on-chain call, callable only from other gum functions. There is no separate
  `internal`/`private` keyword: the absence of `export` *is* internal.
- **`once`** emits a re-entry guard: the function stores a flag in a
  keccak-derived storage slot on first call and reverts on every call
  thereafter. Useful for a one-time `initialize`.
- **`payable` is opt-in.** Any value-bearing call to a non-`payable` entry
  point reverts, so ETH can't be trapped by accident. When *no* function is
  payable the check is hoisted to the dispatcher entry (one copy); as soon as
  one is, each non-payable case carries its own guard; the hoist would
  otherwise drop the check for every other function.

### Variable declarations

Type-first, optional initializer:

```python
u256 x = 5          # typed, initialized
mut u256 y = 0      # explicitly mutable
const Account owner = Message.sender()
```

---

## 6. Functions and dispatch

```
fn_decl = modifier* "fn" ident "(" params? ")" ("->" type)? body
```

Each parameter is `type name` (type-first). A contract's `export fn`s become
ABI entry points. At runtime the dispatcher:

1. rejects calldata shorter than 4 bytes;
2. rejects non-zero `callvalue` for any function that isn't `payable`;
3. switches on the 4-byte selector;
4. for the matched case, checks calldata is long enough for the declared
   arguments, decodes and masks them, and calls the implementation;
5. unmatched selector → revert.

Selectors are the standard `keccak256("name(argType,...)")[:4]`, with gum types
mapped to ABI types (`Account`→`address`, `uN`→`uintN`, etc.).

---

## 7. Expressions and operators

### Precedence (highest first)

| level | operators | assoc |
|--:|---|---|
| 11 | `**` | right |
| 10 | `*` `/` `%` | left |
| 9 | `+` `-` | left |
| 8 | `<<` `>>` | left |
| 7 | `<` `<=` `>` `>=` | left |
| 6 | `==` `!=` | left |
| 5 | `&` | left |
| 4 | `^` | left |
| 3 | `\|` | left |
| 2 | `&&` | left |
| 1 | `\|\|` | left |

So `a == b || c == d` groups as `(a == b) || (c == d)`. Unary `-` and `!` bind
tighter than any binary operator and apply to a whole postfix term (`-x.field`
is `-(x.field)`). Parenthesize with `( … )`.

### Postfix

- `x.field`: property/field access
- `x.method(args)`: method call
- `x[i]`: index (mapping key or array index)

### Arithmetic semantics

Default arithmetic is **checked** (Solidity 0.8+ style):

| op | behavior |
|---|---|
| `+` `-` `*` | revert on overflow/underflow |
| `/` `%` | revert on divide-by-zero; signed variants for `iN` |
| `**` | native `EXP` (not overflow-checked, like unchecked pow) |
| comparisons | signed opcodes (`slt`/`sgt`) for signed operands |

With `--rich-reverts`, arithmetic reverts carry `Panic(uint256)` reason data
(`0x11` overflow, `0x12` divide-by-zero); by default they revert with no data
(smaller bytecode). `x.saturate()` clamps to the type max instead of reverting.

Literal-only expressions that provably fit are constant-folded at compile time
(the runtime check is elided when it cannot fail).

---

## 8. Statements

| statement | form |
|---|---|
| declaration | `T x = e` / `mut T x` / `const T x = e` |
| assignment | `lvalue = e` |
| compound | `x += e`, `x -= e` (desugar to checked `x = x + e` / `x - e`) |
| assert | `assert(cond)` / `assert(cond, "msg")`, reverts if false |
| return | `return e`, or bare `return` in a function with no return type |
| delete | `delete lvalue`, reset to the type's zero value |
| if / else | `if cond:` … `else:` … |
| while | `while cond:` … |
| for-in | `for x in arr:` …, iterates an array's elements, in memory or in storage |
| match | `match e:` with variant arms (§9) |
| log | `log(Event, args…)` (§10) |
| call | `call target(payload)`, low-level `CALL`, fire-and-forget |
| unsafe | `unsafe: <raw Yul>` (§12) |
| expression | a bare expression (e.g. a method call) |

`lvalue` is a local, a storage field (`Class.field`), a mapping entry
(`m[k]`, `m[a][b]`), a struct field of a mapping entry (`m[k].field`), or an
array element (`arr[i]`).

### 7a. Arrays across the ABI

An array argument, return, or constructor argument is converted between the wire
format and gum's memory format, never copied flat, because the two disagree:

| | ABI (the wire) | gum memory |
|---|---|---|
| `[T]` | offset → count → **one 32-byte word per element** | byte-length word, then elements packed at `size_of(T)` |
| `[T; N]` | **N words inline**, no offset, no count | elements packed at `size_of(T)`, no header |

The difference is invisible for `[u256]`, whose stride already *is* a word, and
load-bearing for everything narrower: a `[u8]` carries one byte of payload per 32
on the wire, and one byte per byte in memory.

The element must be a **scalar** (`uN`, `iN`, `bool`, `Account`) or a static
struct (§7b). An array *of arrays* is a compile error rather than a wrong
answer: it would decode each element as a 32-byte scalar, which for `[[T]]`
means reading the inner arrays' offsets as if they were values.

Indexing is bounds-checked in memory as well as in storage (`Panic(0x32)` under
`--rich-reverts`), and `.length` is an element count in both.

### 7b. Structs across the ABI

A `class` whose fields are all scalars crosses the ABI as a `tuple`, as an
argument, a return, or a constructor argument, including one passed to
`new Child(...)`. Like an array, it is converted rather than copied, and for two
independent reasons:

| | ABI (the wire) | gum memory |
|---|---|---|
| field order | **declaration order** | **widest first** (the packer's order) |
| field width | one 32-byte word each | packed at `size_of(T)`, no padding |

So `class P: u128 a; u256 b` puts `a` first on the wire but `b` first in memory.
Each field is moved on its own; a block copy would come back *transposed*, not
merely shifted. Being all-scalar, such a tuple is **static**: it rides inline in
the head with no offset and no count, and the dispatcher's `calldatasize` guard
covers its full width, so a short call reverts instead of reading zeros.

A struct that holds another struct, a `String`/`Bytes`, or an array is a compile
error rather than a wrong answer, those are multi-word or dynamic on the wire
and have no codec yet.

#### `[P]`, an array of structs

Because a static tuple is inline, `[P]` needs no per-element offset: the wire is
an offset word, then a count, then the elements back to back at the tuple's full
width. Memory holds them back to back too, but *packed*, so the two strides
differ (`P{u128,u256,Account}` is 96 bytes on the wire and 80 in memory) and each
element is converted through the same per-struct codec above.

Indexing a memory `[P]` yields the element's **address**, not a copy, elements
are inline, so the address already is the struct pointer that field access wants.
`xs[i].a` reads and `xs[i].a = v` writes in place, and `for x in xs` binds `x` to
each element's address the same way. A `[P; N]` is *not* supported and is a
compile error.

Where it works: arguments, returns, constructor arguments, `new Child(...)`
arguments, and `interface` calls, the last three share one encoder, so they
cannot drift apart.

### `delete`

`delete lvalue` resets an lvalue to its type's zero value, and **releases**
whatever storage it owned, the same slots Solidity's `delete` releases, so the
gas refund matches too.

| target | effect |
|---|---|
| scalar, `Account`, `bool` | set to 0 |
| a packed field | read-modify-write, so the fields sharing its slot are untouched |
| `m[k]` | zero that entry's value |
| `m[k]` where the value is a struct | zero every slot the struct's fields occupy |
| `arr[i]` | zero that element, leaving its slot-mates alone |
| `arr` (dynamic) | zero every occupied element slot, then the length |
| `arr` (fixed) | zero every slot it owns |
| `String` / `Bytes` field | release the long form's data slots, then the header |

`delete m` on a whole mapping is a **compile error**: a mapping's keys aren't
tracked, so there is nothing to clear. (Solidity accepts it and silently does
nothing.) Deleting an immutable local, or any computed expression, is also an
error.

`assert` takes an optional failure value:

| form | revert data |
|---|---|
| `assert(cond)` | none (a blank `revert(0, 0)`) |
| `assert(cond, "msg")` | `Error(string)`, the exact `0x08c379a0` payload Solidity's `require(cond, "msg")` produces, so existing tooling decodes the reason |
| `assert(cond, MyErr(a, b))` | that custom error's own ABI encoding (§8) |

The message must be a `String` or a declared custom-error call; anything else
is a compile error. `--rich-reverts` is independent of this, it governs
whether *arithmetic* and *bounds* failures carry `Panic(uint256)` data.

### Returns and exhaustiveness

A function that declares a return type must return on every path; the semantic
checker rejects functions that can fall through, and `match` must cover every
enum variant.

A function that declares **no** return type may `return` bare to exit early:

```python
contract S:
    u256 t

    export fn set_unless_zero(u256 x):
        if x == 0:
            return
        S.t = x
```

The value is required exactly when the function promises one, a bare `return`
in a value-returning function is a compile error, and so is `return expr` where
no return type was declared.

---

## 9. Enums and match

```python
enum Status:
    Active
    Pending(u256)      # a variant may carry one payload value

contract C:
    export fn check(Status s) -> u256:
        match s:
            Active:
                return 0
            Pending(amount):
                return amount
```

An enum value is a pointer to `[tag, payload]` in memory; the tag is the
variant's 0-based declaration index. `match` switches on the tag and, for a
payload variant, binds the payload in the arm's scope. Match must be
exhaustive.

---

## 10. Events

```python
enum TokenLogs:
    Transfer
    Mint

log(TokenLogs.Transfer, indexed(from), indexed(to), amount)
```

- The event **name** is the variant identifier.
- `indexed(x)` marks a topic (searchable); other args go into the data area.
- `topic0` is `keccak256("Name(type,type,…)")`, the canonical ABI event
  signature, built from the arg types, so wallets/indexers recognize it. For
  example the standard ERC20 `Transfer(address,address,uint256)` topic
  `0xddf252ad…` is produced automatically.
- Emits `LOG1`–`LOG4` by indexed count (topic0 + up to 3 indexed).

---

## 11. Modules

```python
use gum.defaults.Account
use gum.defaults.Message
```

A `use` path is a **module, then the symbol you want out of it**. In
`use gum.defaults.Account`, `gum.defaults` is the module and `Account` is a
class inside it, not a file named after the class.

Importing a symbol brings in **that declaration and whatever its signature
reaches**, not the whole module: `use gum.defaults.Account` also gets
`Serializable`, because `Account` inherits it, but not `Vec`. Import each symbol
you name:

```python
use gum.defaults.Account
use gum.defaults.keccak256      # a free function is a symbol too
```

`gum.*` resolves against the **standard library compiled into `gumc`**, no
search path, no flag, nothing to install, so `gumc contract.gum` works from any
directory. Any other path is a local import: `use a.b.C` reads `a/b/C.gum`
relative to the **source file's own directory**, or symbol `C` out of `a/b.gum`.

Names are case-insensitive, so `gum.defaults.String` and `gum.defaults.string`
are the same import.

A `use` that resolves to nothing is a **compile error**, not a silent skip. The
error names the import and lists what the module does declare, rather than
surfacing later as a missing type.

### Standard library: module `gum.defaults`

One module, one file ([`std/defaults.gum`](std/defaults.gum)), compiled into the
binary. Each row is a symbol you import by name.

| symbol | provides |
|---|---|
| `Account` | the `Account` (address) type, and its intrinsics: `.balance()`, `.pay(v)`, `.transfer(v)`, `.delegated_to()`, `.is_delegated()` |
| `Message` | this call's frame: `.sender()` → `caller()`, `.value()` → `callvalue()`, `.address()` → `address()` |
| `Block` | this block: `.timestamp()` → `timestamp()`, `.number()` → `number()` |
| `String`, `Bytes` | the dynamic byte-string types |
| `HashMap` | `HashMap(K, V)` |
| `Vec` | `Vec(T)` with `.push`/`.get`/`.len` (memory). As a `contract` field it is a **storage vector**, see STORAGE.md |
| `Serializable`, `Hashable` | markers: `serialize()` synthesis, and mapping-key bounds |
| `keccak256`, `ecrecover` | free functions, compiled to the opcode and the precompile |
| `Crypto` | `Crypto.verify_p256(...)`, secp256r1 via the precompile at `0x100` |

Importing a symbol brings in what its signature depends on, so
`use gum.defaults.Account` also brings `Serializable`. It does **not** bring the
rest of the module: a contract that never names `Vec` does not carry it. A
symbol reached from two imports is merged once.

Sending ETH comes in two forms, so neither can be misused silently:

| form | on failure |
|---|---|
| `to.pay(amount) -> bool` | returns `false`, you check it |
| `to.transfer(amount)` | reverts, bubbling up the recipient's own revert data |

Both forward all remaining gas rather than a 2300-gas stipend (the stipend
breaks recipients whose `receive()` does real work, and silently changes meaning
whenever gas is repriced); the reentrancy guard, not a gas limit, is what makes
that safe. Both hand control to the recipient, so both arm the guard.

`Message` and `Block` are compiler intrinsics: their methods compile directly to
EVM opcodes (`caller()`, `callvalue()`, `address()`, `timestamp()`, `number()`),
not to real storage or function calls.

The split is **execution frame** vs **block**, which is the boundary the EVM
actually has:

- **`Message`** is what is true of *this call*. Every `CALL` makes a new one;
  a `DELEGATECALL` inherits the caller's. `address()` belongs here, not with
  the block: under `DELEGATECALL` it is the *calling* contract's address.
- **`Block`** is what is true of the whole block, and is identical in every
  frame of the transaction.

`Message.sender()` is the **immediate caller**, not whoever signed the
transaction, if a contract calls you, it is that contract. That is why the
class is not called `Transaction`: a transaction has one origin, while
`CALLER` differs in every frame. gum has no `tx.origin` equivalent.

---

## 12. `unsafe`, raw Yul

```python
unsafe:
    sstore(0, add(sload(0), 1))
```

The indented body is passed through to the output Yul verbatim (with correct
brace-balancing for nested Yul blocks). This is the escape hatch for anything
the language does not yet express. No safety checks apply inside it.

---

## 13. Safety summary

gum inserts these checks so contracts are safe by default:

| check | where |
|---|---|
| overflow / underflow / divide-by-zero | every `+ - * / %` on integers |
| `callvalue == 0` | once, at dispatch entry (no payable functions) |
| `calldatasize ≥ 4` | before selector decode |
| calldata long enough for arguments | per function, before decode |
| `Account` masked to 160 bits | on calldata decode |
| external-call returndata ≥ 32 bytes | before decoding a declared return |
| array index in bounds | fixed and dynamic array access (`Panic(0x32)`) |
| `pop` on non-empty | dynamic array `pop` (`Panic(0x31)`) |

Reverts are blank by default (smallest bytecode); `--rich-reverts` makes the
arithmetic and array Panics carry Solidity-identical `Panic(uint256)` data.

---

## 14. Compilation pipeline

```
.gum → indent preprocess → pest parse → AST → semantic check → Yul → (solc --strict-assembly) → EVM bytecode
```

gum emits standard Yul; `solc`'s battle-tested Yul→EVM backend (optimizer +
assembler) produces the final bytecode. The ABI JSON is emitted alongside.
