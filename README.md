<div align="center">

<img src="assets/logo.svg" width="130" alt="gum" />

# gum

[![CI](https://github.com/Hoteira/gum-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/Hoteira/gum-lang/actions/workflows/ci.yml)

**A smart-contract language that reads clean, ships like Solidity, and
protects you like Rust.**

Built by web2 devs, for web2 devs. Write Ethereum contracts without the
footguns — gum compiles to ordinary EVM bytecode that's *provably* identical to
Solidity's behavior, and usually smaller.

</div>

```
use gum.defaults.Account
use gum.defaults.Message

contract Wallet:
    HashMap(Account, u256) balances

    export fn deposit(u256 amount):
        var me = Message.sender()
        Wallet.balances[me] = Wallet.balances[me] + amount

    export fn balance_of(Account who) -> u256:
        return Wallet.balances[who]
```

That's a complete, deployable contract. No boilerplate, no braces, no
semicolons — and every safety guard you'd have to remember in Solidity is
already switched on.

---

## The pitch, in five lines

- **Familiar on day one.** Clean, low-ceremony syntax: types first,
  colon-and-indent blocks, `HashMap`/`Vec`/`String` that mean exactly what
  you'd guess.
- **Safe by default, not by discipline.** Overflow checks, bounds checks,
  address masking, and reentrancy guards are *on automatically*. The #1
  exploited bug class in the ecosystem is off by default in Solidity — in gum
  it's off by default *for attackers*.
- **Zero ecosystem risk.** It's not a new chain. Ordinary EVM bytecode,
  ordinary ABI. Etherscan decodes it, ethers and viem call it, your indexers
  don't notice the difference.
- **Cheaper to ship.** 80–94% of Solidity's bytecode, 83–96% of the deploy gas,
  at parity on runtime — while doing *strictly more* safety work. Every number
  is asserted by a test, not measured by hand.
- **Proven, not promised.** A differential test harness runs gum and equivalent
  Solidity in the same EVM and asserts identical storage, return data, logs, and
  reverts — on fixed *and* fuzzed inputs. "Behaves like Solidity" is a passing
  test, not a tagline.

---

## Why teams pick it up

**Low ceremony.** Colon-and-indent blocks, types first, no braces or
semicolons. `HashMap`, `Vec`, `String` mean what you would guess. The syntax
stays out of the way, so what you read is the logic, not the plumbing.

**Safe by default, not by discipline.** Overflow checks, array bounds checks,
address masking, and reentrancy guards are on automatically. Reentrancy is the
single most exploited bug class in the ecosystem and is off by default in
Solidity. Locals are immutable unless you write `mut`, an idea borrowed from
Rust.

**It is not a new blockchain.** gum compiles to ordinary EVM bytecode with an
ordinary ABI. Etherscan decodes it. ethers and viem call it. Storage layout
matches Solidity's, so existing block explorers and indexers work unchanged.
You are changing the syntax, not leaving the ecosystem.

**It is smaller.** 62-95% of the equivalent Solidity's bytecode, 69-96% of the
deploy gas, at parity on runtime gas. Those numbers come from tests, not from
hand measurement.

**It tells you when you are wrong.** Most mistakes are compile errors with an
explanation rather than a revert at 2am: a `const` field assigned on only one
branch, a struct array pushed the wrong way, an event logged with two different
shapes.

**`try`/`catch` that actually catches.** In Solidity, `try` only wraps an
external call or a `new` — an `assert` or an overflow deeper in the block still
takes the whole transaction down. gum's `try:` / `catch:` wraps an *arbitrary
block* and catches **any** revert inside it, internal or external: the body runs
in a self-call frame whose state rolls back cleanly on failure, then `catch`
runs, with locals the body mutated written back out. Recovery you can actually
scope, not just call-site error handling.

---

## How it works (the honest version)

`.gum` → [Yul](https://docs.soliditylang.org/en/latest/yul.html) → EVM bytecode,
assembled by `solc`. The compiler, `gumc`, is a single Rust binary. No runtime,
no VM changes, no trust-us magic.

The load-bearing part — and the thing that makes the claims above safe to
believe — is verification. A differential test harness deploys
gum's bytecode and the equivalent Solidity into an in-process EVM
([revm](https://github.com/bluealloy/revm)) and asserts they produce identical
storage, return data, logs, and reverts, on both fixed and fuzzed inputs. That
is what "behaves like Solidity" is measured against.

- Language reference: [SPEC.md](SPEC.md)
- Storage model and upgrade safety: [STORAGE.md](STORAGE.md)

---

## The syntax in one screen

```
use gum.defaults.Account
use gum.defaults.Message

enum VaultLogs:                       // events are enum variants
    Deposited

class Stake:                          // a plain struct, a value, no storage
    u256 amount
    u256 since

interface IERC20:                     // an external contract; calls become CALL
    fn transfer(Account to, u256 amount) -> bool

contract Vault:                       // the on-chain state singleton
    u256 total                        // storage
    const Account owner               // fixed at deploy, no slot, ~3 gas to read
    transient u256 depth              // scratch state, cleared each transaction
    HashMap(Account, Stake) stakes    // nested and struct values both work
    [Stake] history                   // dynamic array

    fn new(Account o):                // constructor, runs once, at deploy
        Vault.owner = o

    export payable fn deposit(u256 amt):      // public entry point, takes ETH
        var who = Message.sender()            // immutable local, type inferred
        assert(amt > 0, "zero deposit")
        Vault.stakes[who].amount = Vault.stakes[who].amount + amt
        Vault.history.push()                  // append a zeroed element
        Vault.history[Vault.history.length - 1].amount = amt
        Vault.total = Vault.total + amt
        log(VaultLogs.Deposited, indexed(who), amt)

    fn internal_helper() -> u256:     // no `export` means no ABI selector
        return Vault.total

    export fn read(Account who) -> u256:
        return Vault.stakes[who].amount
```

A plain `class` (no `contract`) is a value that lives in memory — a struct with
methods, told apart the way Rust does it: a function that takes `self` is an
instance method, called on a value; a function without `self` is an associated
function, called on the type. A **constructor is not special** — it is any
`self`-taking function called on the type, so a class can have several, named
whatever you like, and there is no built-in `new`:

```
class Rational:                       // a value type, lives in memory
    u256 num
    u256 den

    fn new(self, u256 n, u256 d):     // a constructor — Rational.new(1, 2)
        self.num = n
        self.den = d

    fn whole(self, u256 n):           // another constructor — Rational.whole(3)
        self.num = n
        self.den = 1

    fn scale(self, u256 k) -> u256:   // instance method — r.scale(10)
        return self.num * k / self.den

    fn half() -> Rational:            // associated fn (no self): a factory
        return Rational.new(1, 2)

contract C:
    export fn f() -> u256:
        var r = Rational.whole(3)     // construct via a named constructor
        return r.scale(10)            // 30
```

Everything else you would expect:

```
if x > 0:
    ...
else:
    ...

while i < n:
    i = i + 1

for s in Vault.history:               // works on memory or storage
    ...

match status:                         // over enums, exhaustiveness checked
    Active:                           // arms are bare variant names
        ...
    Closed:
        ...

try:                                  // catches ANY revert in the block,
    Vault.total = Vault.total + amt   //   internal or external — this overflow,
    IReceiver(to).onReceived(amt)     //   or this external call reverting
catch:                                // on failure, the block's state rolled
    log(VaultLogs.Deposited, amt)     //   back; recover here instead

mut u256 x = 0                        // mutable local (immutable is the default)
delete Vault.stakes[who]              // reset to zero, release the slots
revert Insufficient(amt)              // custom error
var c = Child.new(arg)                // deploy another contract (CREATE)
to.transfer(amount)                   // send ETH; reverts on failure
f"balance: {amt}"                     // interpolation, produces a String
```

### Keywords at a glance

| Keyword | Means |
|---------|-------|
| `contract Name:` | the persistent on-chain state singleton: fields sit at fixed storage slots, `export fn`s are its entry points. A file may declare several |
| `class Name:` | a plain struct, a value with no storage slot of its own |
| `[Parent]` *(above a class)* | inherits Parent's fields and methods, written as an attribute above the declaration like Rust's `#[derive(...)]`. Several parents go in one list, `[A, B]`. An `interface` parent instead means *implements*: the child must define its methods |
| `interface Name:` | an external contract's ABI; method calls compile to `CALL` (`extern class` also accepted) |
| `export fn` | a public entry point, declared inside a `contract`. A bare `fn` is internal, with no ABI selector |
| `export payable fn` | a public entry point that may receive ETH. Without `payable`, any value-bearing call reverts |
| `var x = expr` | an immutable local, type inferred (reassignment is a compile error) |
| `mut var x = expr` | a mutable inferred local |
| `const T x` *(field)* | never changes after deploy: assigned once in `fn new`, then read for ~3 gas instead of a 2100-gas `SLOAD`. Owns no slot. The compiler picks how to carry it: inlined if it can work the value out, otherwise baked into the bytecode at deploy |
| `transient T x` *(field)* | transient storage (EIP-1153): same layout, `TSTORE`/`TLOAD`, cleared at the end of the transaction |
| `delete x` | reset a field, element, or local to its type's zero value, releasing whatever storage it owned |

`String` and `Bytes` are first-class: they pass as arguments, return values, and
custom-error fields, ABI-encoded identically to Solidity's `string`/`bytes`.
String literals and `f"..."` interpolation are `String` values.

---

## Getting started

```sh
cd gumc
cargo build              # binary at gumc/target/debug/gumc
```

Bytecode output needs `solc` to assemble the Yul. Drop one at `tools/solc.exe`
or point at it with `--solc`; see [tools/README.md](tools/README.md). Yul and ABI
output do not need it.

```sh
# Emit Yul + ABI JSON
gumc contract.gum

# Assemble all the way to EVM bytecode
gumc contract.gum --bytecode --solc tools/solc.exe

# Freeze / enforce storage layout for upgrade safety (see STORAGE.md)
gumc contract.gum --lock layout.json
```

| flag | meaning |
|---|---|
| `--bytecode` | assemble the Yul to EVM bytecode (needs `solc`) |
| `--solc <path>` | path to the `solc` binary (default: `solc` on PATH) |
| `--lock <file>` | storage-layout lockfile; created on first use, enforced after |
| `--rich-reverts` | emit `Panic(uint256)` reason data (larger, decodable) |
| `--test` | run every `[Test]` function in an in-process EVM (needs `solc`) |
| `-o, --output <file>` | with `--bytecode`, write the hex here instead of stdout |

The standard library ([`std/defaults.gum`](std/defaults.gum)) is compiled into
the binary, so `use gum.defaults.<Symbol>` resolves from any directory with no
search path and nothing to install. `gumc` is one file. A `use` path is a module
then a symbol: `gum.defaults` is the module, `Account` is a class in it.

---

## Testing contracts

Mark a no-argument function with `[Test]` and run it with `--test`. A test
**passes** if it returns and **fails** if it reverts; the revert reason (an
`assert` string, a `Panic` code, a custom error) is shown, and any failure makes
`gumc` exit non-zero so CI can gate on it. Each test runs against its own fresh
deployment, so they never leak state into each other. A plain `fn` beside the
tests is an ordinary helper, not run and callable from the tests.

```
# demo_test.gum
use gum.defaults.hashable

contract DemoTests:
    u256 counter

    fn fresh_counter() -> u256:          # a helper, not a test
        return DemoTests.counter

    [Test]
    fn storage_roundtrips():
        DemoTests.counter = 7
        assert(self.fresh_counter() == 7, "storage broke")

    [Test]
    fn itoa_works():
        var n = 42
        assert(n.to_string() == "42", "itoa wrong")
```

```sh
gumc demo_test.gum --test --solc tools/solc.exe
```

```
contract DemoTests
  ok    storage_roundtrips
  ok    itoa_works

2 tests, 2 passed, 0 failed
```

`[Test]` makes a function a runnable entry point on its own, so it needs no
`export`. The runner deploys with no constructor arguments, so a test contract
needs either no `fn new` or a no-argument one. It runs the same in-process EVM
(`revm`) the compiler's own differential suite uses.

**Cheatcodes.** Inside a test, set `Vm.sender` to make the calls that follow
come from an address you choose, so you can test access control without
deploying from many keys:

```
[Test]
fn only_owner_can_pause():
    var v = Vault.new()
    Vm.sender = 0x00000000000000000000000000000000000000AA   # not the owner
    IVault(v).pause()                                        # this call is from 0xAA
```

The sender stays set until you change it. Setting `Vm.sender` compiles to a
plain call to the cheatcode address, which the runner's EVM inspector
intercepts; off the test path it hits an address with no code and is a harmless
no-op, so a stray cheatcode never affects production behavior.

---

## What works (and how we know)

Nearly everything here is exercised by a reference contract and diffed against
an equivalent Solidity twin by the execution tests. The one exception is noted
in its row: gum's transient collections have no Solidity equivalent to diff
against, so they are verified against the EVM's own behaviour instead.

| area | supported |
|---|---|
| Integers | `u8`-`u256`, `i8`-`i256`, checked `+ - * / % **`, signed ops, `.saturate()`, and `.to_string()` on unsigned ints (decimal itoa to a `String`) |
| Other scalars | `bool`, `Account` (EVM address, masked to 160 bits at boundaries), fixed bytes `b1`-`b32` (ABI `bytes1`-`bytes32`; `b32` is a full word, sub-word values ride the wire left-aligned like Solidity) |
| Mappings | `HashMap(K, V)`, scalar-valued, nested (`HashMap(K, HashMap(K, V))`), struct-valued, `String`/`Bytes`-valued, and dynamic-array-valued (`HashMap(K, [T])`). A `String`/`Bytes` value gets its own slot region at `keccak256(key ‖ p)` — the value slot doubles as the string's base slot, exactly as `mapping(K => string)` (short packed inline, long at `keccak256(valueSlot)`). A `[T]` value holds its length at `keccak256(key ‖ p)` with its elements packed from `keccak256(that slot)`, exactly as `mapping(K => T[])` — `push`/`pop`/`[i]`/`.length`/`delete` all work. Both are verified slot-for-slot against Solidity |
| Structs | user `class`, in memory and in storage, as a mapping value or as an array element. `arr[i].field` and `m[k].field` compile to the same slot arithmetic; a struct array element occupies whole slots and never packs with its neighbours, exactly as Solidity lays it out |
| Inheritance | an `[Parent]` attribute above a class inherits its fields (ancestors first, as Solidity orders them) and methods, transitively; a child method overrides, and `super.method()` calls the parent's. An `interface` parent means *implements*, checked signature by signature |
| Storage strings | `String`/`Bytes` `contract` fields use Solidity's exact storage layout (short packed inline, long at `keccak256(slot)`), slot for slot |
| Storage arrays | fixed `[T; N]` and dynamic `[T]` with `.push()`, `.pop()`, `.length`, indexing, all bounds-checked, and packed like Solidity's (32 `u8`s to a slot) |
| Storage vectors | a `Vec(T)` `contract` field is a storage vector: the same layout as `[T]`, and it takes either spelling (`v[i]`/`v.get(i)`, `.length`/`.len()`) |
| `delete` | resets a scalar, packed field, mapping entry, array element, whole array, storage string, or struct, releasing the slots Solidity releases |
| Control flow | `if/else`, `while`, `for ... in <array>` (memory or storage), `match` over enums |
| Events | `log(Event, indexed(x), ...)` becomes a real `LOG1`-`LOG4` with canonical topic hashes, plus matching `"type": "event"` entries in the ABI JSON, so wallets, ethers/viem and Etherscan decode the logs. The schema is recorded at the `log()` site from the same values `topic0` is hashed from, so the ABI cannot describe an event the bytecode does not emit. The data area shares the ABI encoder the `interface` and `Child.new(...)` paths use, so a string, an array or a tuple field is encoded head/tail rather than as a pointer. An indexed field must be one word, since a topic is 32 bytes |
| External calls | `interface` types (compiled to `CALL`), low-level `call target(payload)`, `to.pay(amount)` (returns success) and `to.transfer(amount)` (reverts on failure) to send ETH. A failing call bubbles the callee's own revert reason, byte for byte as Solidity does. A `try:` / `catch:` block recovers from a revert instead of bubbling it — and unlike Solidity's (external-calls-only) `try`, gum's wraps an **arbitrary block** and catches *any* revert inside it, internal or external (a failed `assert`, an overflow, an internal call, as well as an external one): the body runs in a self-call frame that rolls its state back on failure, then `catch` runs. `addr.code.len()` gives the callee's code size (`EXTCODESIZE`), e.g. to skip the `onERC721Received` hook for a plain wallet |
| Deploying contracts | `SomeContract.new(args)` becomes `CREATE`, with the child's creation code embedded in the deployer; or `Account.create`/`create2`/`create2_address` from raw bytecode, for proxies and EIP-1167 clones |
| Bare ETH | `export payable fn receive():` for a plain send, `export fn fallback():` for an unmatched selector |
| Const fields | `const` `contract` fields, assigned once in `fn new`, never storage. One keyword, and the compiler picks the mechanism: a value it can evaluate is inlined (byte for byte the same code as writing the literal); a value that only exists at deploy, like a constructor argument, is written into the runtime bytecode there (Yul `setimmutable`). Either way a read is ~3 gas, not a cold `SLOAD`: measured 21,160 vs 23,246, and ~15.8k cheaper to deploy. Assignment is checked on every path through the constructor. Not usable behind a proxy, see [STORAGE.md](STORAGE.md) |
| Transient storage | `transient` `contract` fields (EIP-1153): scalars, mappings, arrays and strings, with the same layout as their persistent twins in their own keyspace. ~100 gas against `SSTORE`'s 2,900-20,000. Solidity's `transient` is value-types-only, so the collections have no twin to diff against |
| Hashing & encoding | `keccak256(...)`, `ecrecover(...)`, and `Abi.encode(...)` / `Abi.encode_packed(...)` returning a `Bytes` for the ABI-standard and tightly-packed forms, byte-for-byte Solidity's `abi.encode` / `abi.encodePacked`. `keccak256(Abi.encode(...))` is the building block for EIP-712 digests, merkle leaves, and signature verification |
| Modern EVM | transient-storage reentrancy guards (EIP-1153), `MCOPY` for memory copies (EIP-5656), `Crypto.verify_p256(...)` via the secp256r1 precompile (EIP-7951 / RIP-7212), `a.delegated_to()` / `a.is_delegated()` for EIP-7702 accounts |
| Safety | checked arithmetic, reentrancy guards on by default (transient storage; `unsafe fn` opts out), nonpayable guard, calldata-length validation, address masking, returndata checks, array-bounds `Panic(0x32)` in memory and storage |
| Upgrades | storage-layout lockfile (`--lock`) pins committed fields and errors on unsafe changes |
| Reproducible builds | the same source always compiles to byte-identical bytecode, which is what lets a deployed contract be verified against its source. Emission order is stable everywhere (slots, helpers, class methods), never a randomized hash-map walk. Asserted by a test that compiles a reference contract in separate processes and diffs the output |
| ABI | standard 4-byte selectors; `address`/`uintN`/`bytesN`/`bool`, `string`/`bytes`, `T[]`, `T[N]`, an `enum` as `uint8`, a `class` as a `tuple` — of scalar fields (a static tuple) or with `string`/`bytes`/array fields (a dynamic tuple, head/tail encoded) — and arrays of any of those nested to any depth (`T[][]`, `T[][2]`, `T[3][]`, `tuple[2]`, `tuple[][]`), including arrays of dynamic elements (`string[]`, `string[N]`, `string[][]`). Each works in every direction: arguments, returns, constructor arguments, `Child.new(...)` arguments, and `interface` calls both out and back. A struct's fields cross in declaration order while memory packs them widest-first, so each field is moved individually rather than block-copied. `stateMutability` is inferred: a function that writes nothing is `view`, one that touches no chain state at all is `pure`, so wallets and explorers render getters as reads rather than as write buttons. The inference is a whitelist, so anything it cannot prove read-only stays `nonpayable` |

Reference contracts live in [`examples/`](examples/): [`token`](examples/token.gum),
[`amm`](examples/amm.gum), [`erc20`](examples/erc20.gum),
[`erc721`](examples/erc721.gum), [`vault`](examples/vault.gum) (struct in mapping).
Their Solidity twins are in [`examples/solidity/`](examples/solidity/).

---

## The numbers (measured against Solidity)

Same solc, same optimizer settings. Every number comes from a test
(`size_report`, `gas_report`), not hand measurement, and `size_report` asserts
the size band so the docs cannot silently drift from the compiler.

| | runtime bytecode | deploy gas | runtime gas |
|---|--:|--:|--:|
| gum vs Solidity | 80-94% | 83-96% | ~100% |

gum is smaller and cheaper to deploy, and at parity on runtime gas, which is
dominated by storage, keccak and log operations that are identical on both
sides. gum's side does strictly more: every entry point carries a reentrancy
guard the twins do not have.

Sizes are measured against `solc --no-cbor-metadata`. Solidity appends ~54 bytes
of CBOR metadata by default and gum emits none; counting that would credit gum
for bytes it never writes, which is not a codegen win.

| | gum | Solidity | |
|---|--:|--:|--:|
| `amm` | 1617 | 1999 | 80% |
| `vault` | 487 | 602 | 80% |
| `token` | 1018 | 1244 | 81% |
| `erc721` | 3338 | 3853 | 86% |
| `erc20` | 1732 | 1834 | 94% |

The size difference is codegen density, not omitted safety, and the leaner
contracts (`amm`, `vault`, `token`) show it best. `erc20` and `erc721` are full,
faithful ports of OpenZeppelin's audited contracts, differential-tested against
the real OpenZeppelin v5.1 source, so they carry the same custom errors, events,
and (for `erc721`) the ERC165 / `tokenURI` / receiver-callback surface, and land
closer to parity. The tighter the contract, the further gum pulls ahead. On
runtime gas the small overhead is where gum does strictly more work, e.g.
`erc721`'s `approve` reads the token owner to match OpenZeppelin exactly.

---

## What does not work yet

The first of these is rejected at compile time rather than miscompiled. The
second is a diagnostics gap.

- **A few ABI nesting cases.** Across the ABI, arrays nest to any depth
  (`[[T]]`, `[[T]; N]`, `[[T; N]]`, `[P; N]`, `[[P]]`), carry a `String`/`Bytes`
  element (`[String]`, `[String; N]`, `[[String]]`), and a struct may now hold a
  `String`/`Bytes` or array field and cross as a dynamic tuple (`class { u256 id;
  String uri }`) — head/tail encoded, verified against Solidity. Still rejected:
  a struct that itself *nests another struct*, and a *dynamic struct as an array
  element* (`[P]` where `P` has a dynamic field) — each would need the tuple
  codec applied recursively, which is not built yet. Everything accepted works as
  arguments, returns, constructor arguments, and `interface` calls (both
  directions).
- **Dynamic values inside storage.** A `String`/`Bytes` mapping value
  (`HashMap(K, String)`) and a dynamic-array mapping value (`HashMap(K, [T])`)
  now work, laid out exactly as Solidity's `mapping(K => string)` /
  `mapping(K => T[])`. Still unsupported: a `String`/dynamic array as a *storage
  array element* or a *`Vec` element*, and a nested storage array (`[[T]]`) —
  each would need a slot region of its own per element. This is narrower than the
  ABI: `[[T]]` is fine as an argument, just not as a field.
- **Parser error recovery stops at statement granularity.** Recovery now nests
  three levels — file → declaration → member → statement — so every malformed
  declaration, every broken member of a contract, and every broken *statement*
  within one function body is reported in a single run, not just the first.
  Remaining limits: a bad statement inside a *nested* block (an `if`/`for` body)
  is localized to that block rather than the inner line, and an indentation
  error still stops the compile on its own, since indentation is what
  establishes the structure to recover between.

---

## Testing

```sh
cd gumc
cargo test                 # compile tests + execution/differential tests
```

- Compile tests (`tests/compile_tests.rs`) assert that snippets compile, or fail
  with the right error, and that the emitted Yul contains the expected
  constructs.
- Execution tests (`tests/execution_tests.rs`) are the real safety net. They
  deploy gum's bytecode and the Solidity twin into revm, pinned to the Osaka
  hardfork so that EIP-7702 delegation and the secp256r1 precompile are actually
  present, run identical calls, and assert identical storage, return data, logs
  and reverts. They include fuzzers (random ERC20/AMM/Store sequences across
  accounts), a memory-allocator stress test, and a **structural** fuzzer that
  generates random *contracts* — a random mix of field types, so a random storage
  packing — with setters, getters and checked-add mutators, and diffs each against
  a generated Solidity twin. That last one probes the codegen surface the fixed
  contracts can't reach (arbitrary field packing, narrow-type masking, signed
  arithmetic); it is what caught a real signed-underflow bug where a negative
  narrow int was read unsigned and its underflow slipped past the checked add.
  These need `solc`; they skip when it is absent.

Reporting tests, run with `-- --nocapture`:

```sh
cargo test --test execution_tests size_report   -- --nocapture   # bytecode, gum vs solc
cargo test --test execution_tests gas_report    -- --nocapture   # gas, gum vs solc
cargo test --test execution_tests timing_report -- --nocapture   # wall-clock, gum vs solc
```

---

## Layout

```
examples/            reference contracts, and their Solidity twins
std/defaults.gum     the standard library, compiled into the binary
scripts/             formatting helpers
tools/               where you put solc (not committed)
gumc/src/
  indent.rs          colon/indent to brace preprocessor
  stdlib.rs          the embedded standard library and module resolution
  parser/gum.pest    grammar (pest)
  parser/mod.rs      AST building + operator-precedence climbing
  ast/mod.rs         AST types
  semantic/mod.rs    type checking, scoping, return/exhaustiveness checks
  codegen/
    layout.rs        storage/memory layout + the storage-lock manifest
    abi.rs           selectors + ABI JSON
    translator.rs    expression/statement to Yul
    mod.rs           dispatcher, helpers, Yul object assembly
```

Pipeline: `.gum` → indent preprocess → pest parse → AST → semantic check →
Yul codegen → (`solc --strict-assembly`) → EVM bytecode.
