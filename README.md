<div align="center">

<img src="assets/logo.svg" width="130" alt="gum" />

# gum

**Smart contracts that read like Python.**

Write Ethereum contracts in a language you already know how to read.
Get bytecode that behaves like Solidity's, and is usually smaller.

</div>

```python
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

That is a complete, deployable contract. No boilerplate, no braces, no
semicolons.

---

## Why you might want this

**You already know how to read it.** Colon and indentation, like Python. Types
come first, like C. `HashMap`, `Vec`, `String` mean what you would guess.

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

---

## How it works

`.gum` → [Yul](https://docs.soliditylang.org/en/latest/yul.html) → EVM bytecode,
assembled by `solc`. The compiler, `gumc`, is a Rust binary.

The load-bearing part is verification. A differential test harness deploys
gum's bytecode and the equivalent Solidity into an in-process EVM
([revm](https://github.com/bluealloy/revm)) and asserts they produce identical
storage, return data, logs, and reverts, on both fixed and fuzzed inputs. That
is what "behaves like Solidity" is measured against.

- Language reference: [SPEC.md](SPEC.md)
- Storage model and upgrade safety: [STORAGE.md](STORAGE.md)

---

## The syntax in one screen

```python
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

Everything else you would expect:

```python
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

mut u256 x = 0                        // mutable local (immutable is the default)
delete Vault.stakes[who]              // reset to zero, release the slots
revert Insufficient(amt)              // custom error
var c = new Child(arg)                // deploy another contract (CREATE)
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
| `-o, --output <file>` | with `--bytecode`, write the hex here instead of stdout |

The standard library ([`std/defaults.gum`](std/defaults.gum)) is compiled into
the binary, so `use gum.defaults.<Symbol>` resolves from any directory with no
search path and nothing to install. `gumc` is one file. A `use` path is a module
then a symbol: `gum.defaults` is the module, `Account` is a class in it.

---

## What works

Nearly everything here is exercised by a reference contract and diffed against
an equivalent Solidity twin by the execution tests. The one exception is noted
in its row: gum's transient collections have no Solidity equivalent to diff
against, so they are verified against the EVM's own behaviour instead.

| area | supported |
|---|---|
| Integers | `u8`-`u256`, `i8`-`i256`, checked `+ - * / % **`, signed ops, `.saturate()` |
| Other scalars | `bool`, `Account` (EVM address, masked to 160 bits at boundaries) |
| Mappings | `HashMap(K, V)`, scalar-valued, nested (`HashMap(K, HashMap(K, V))`), struct-valued |
| Structs | user `class`, in memory and in storage, as a mapping value or as an array element. `arr[i].field` and `m[k].field` compile to the same slot arithmetic; a struct array element occupies whole slots and never packs with its neighbours, exactly as Solidity lays it out |
| Inheritance | an `[Parent]` attribute above a class inherits its fields (ancestors first, as Solidity orders them) and methods, transitively; a child method overrides, and `super.method()` calls the parent's. An `interface` parent means *implements*, checked signature by signature |
| Storage strings | `String`/`Bytes` `contract` fields use Solidity's exact storage layout (short packed inline, long at `keccak256(slot)`), slot for slot |
| Storage arrays | fixed `[T; N]` and dynamic `[T]` with `.push()`, `.pop()`, `.length`, indexing, all bounds-checked, and packed like Solidity's (32 `u8`s to a slot) |
| Storage vectors | a `Vec(T)` `contract` field is a storage vector: the same layout as `[T]`, and it takes either spelling (`v[i]`/`v.get(i)`, `.length`/`.len()`) |
| `delete` | resets a scalar, packed field, mapping entry, array element, whole array, storage string, or struct, releasing the slots Solidity releases |
| Control flow | `if/else`, `while`, `for ... in <array>` (memory or storage), `match` over enums |
| Events | `log(Event, indexed(x), ...)` becomes a real `LOG1`-`LOG4` with canonical topic hashes, plus matching `"type": "event"` entries in the ABI JSON, so wallets, ethers/viem and Etherscan decode the logs. The schema is recorded at the `log()` site from the same values `topic0` is hashed from, so the ABI cannot describe an event the bytecode does not emit |
| External calls | `interface` types (compiled to `CALL`), low-level `call target(payload)`, `to.pay(amount)` (returns success) and `to.transfer(amount)` (reverts on failure) to send ETH. A failing call bubbles the callee's own revert reason, byte for byte as Solidity does |
| Deploying contracts | `new SomeContract(args)` becomes `CREATE`, with the child's creation code embedded in the deployer; or `Account.create`/`create2`/`create2_address` from raw bytecode, for proxies and EIP-1167 clones |
| Bare ETH | `export payable fn receive():` for a plain send, `export fn fallback():` for an unmatched selector |
| Const fields | `const` `contract` fields, assigned once in `fn new`, never storage. One keyword, and the compiler picks the mechanism: a value it can evaluate is inlined (byte for byte the same code as writing the literal); a value that only exists at deploy, like a constructor argument, is written into the runtime bytecode there (Yul `setimmutable`). Either way a read is ~3 gas, not a cold `SLOAD`: measured 21,160 vs 23,246, and ~15.8k cheaper to deploy. Assignment is checked on every path through the constructor. Not usable behind a proxy, see [STORAGE.md](STORAGE.md) |
| Transient storage | `transient` `contract` fields (EIP-1153): scalars, mappings, arrays and strings, with the same layout as their persistent twins in their own keyspace. ~100 gas against `SSTORE`'s 2,900-20,000. Solidity's `transient` is value-types-only, so the collections have no twin to diff against |
| Modern EVM | transient-storage reentrancy guards (EIP-1153), `MCOPY` for memory copies (EIP-5656), `keccak256(...)`, `ecrecover(...)`, and `Crypto.verify_p256(...)` via the secp256r1 precompile (EIP-7951 / RIP-7212), `a.delegated_to()` / `a.is_delegated()` for EIP-7702 accounts |
| Safety | checked arithmetic, reentrancy guards on by default (transient storage; `unsafe fn` opts out), nonpayable guard, calldata-length validation, address masking, returndata checks, array-bounds `Panic(0x32)` in memory and storage |
| Upgrades | storage-layout lockfile (`--lock`) pins committed fields and errors on unsafe changes |
| Reproducible builds | the same source always compiles to byte-identical bytecode, which is what lets a deployed contract be verified against its source. Emission order is stable everywhere (slots, helpers, class methods), never a randomized hash-map walk. Asserted by a test that compiles a reference contract in separate processes and diffs the output |
| ABI | standard 4-byte selectors; `address`/`uintN`/`bool`, `string`/`bytes`, `T[]`, a `class` of scalar fields as a `tuple`, and `T[]` of those tuples. Each works in every direction: arguments, returns, constructor arguments, `new Child(...)` arguments, and `interface` calls both out and back. A struct's fields cross in declaration order while memory packs them widest-first, so each field is moved individually rather than block-copied. `stateMutability` is inferred: a function that writes nothing is `view`, one that touches no chain state at all is `pure`, so wallets and explorers render getters as reads rather than as write buttons. The inference is a whitelist, so anything it cannot prove read-only stays `nonpayable` |

Reference contracts live in [`examples/`](examples/): [`token`](examples/token.gum),
[`amm`](examples/amm.gum), [`erc20`](examples/erc20.gum),
[`erc721`](examples/erc721.gum), [`vault`](examples/vault.gum) (struct in mapping).
Their Solidity twins are in [`examples/solidity/`](examples/solidity/).

---

## Measured against Solidity

Same solc, same optimizer settings.

| | runtime bytecode | deploy gas | runtime gas |
|---|--:|--:|--:|
| gum vs Solidity | 62-95% | 69-96% | 99-101% |

gum is cheaper to deploy, because the bytecode is smaller, and at parity on
runtime gas, which is dominated by storage, keccak and log operations that are
identical on both sides. Both rows come from tests rather than hand measurement
(`size_report` and `gas_report`), and `size_report` asserts the size band, so it
fails if the docs and the compiler ever drift apart.

Sizes are measured against `solc --no-cbor-metadata`. Solidity appends ~54 bytes
of CBOR metadata by default and gum emits none; counting that would credit gum
for bytes it never writes, which is not a codegen win.

| | gum | Solidity | |
|---|--:|--:|--:|
| `erc721` | 842 | 1337 | 62% |
| `erc20` | 900 | 1234 | 72% |
| `vault` | 487 | 602 | 80% |
| `amm` | 1646 | 1907 | 86% |
| `token` | 1100 | 1150 | 95% |

The size difference is codegen density, not omitted safety. gum's side does
strictly more than the Solidity twins: every entry point carries a reentrancy
guard the twins do not have, and `once` functions carry a replay guard. The
tighter the contract, the better gum does. `token` is closest because it is the
most revert-string-heavy, and a revert string is the same bytes in any language.

The one place gum is dearer at runtime is a `once` function, at ~125-134% of its
twin. The replay guard is a cold `SSTORE`, and ~22k gas is what that costs. It
buys a guarantee the twin does not have, and it is paid once. The 99-101% band
covers every function without a `once`.

---

## What does not work yet

The first of these is rejected at compile time rather than miscompiled. The
second is a diagnostics gap.

- **Nesting across the ABI past one level.** A struct or an array is fine, and
  so is one inside the other (`[P]`), but not two: `[[T]]`, `[P; N]`, and a
  struct that itself holds a struct, a `String`/`Bytes`, or an array are all
  rejected. Everything at one level (`[T]`, `[T; N]`, `P`, `[P]`) works as
  arguments, returns, constructor arguments, and `interface` calls.
- **Parser error recovery is per top-level declaration.** Every malformed
  declaration is reported, but only the first error within one is, and an
  indentation error stops the compile on its own, since indentation is what
  establishes the declarations to recover between.

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
  accounts) and a memory-allocator stress test. These need `solc`; they skip
  when it is absent.

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
  indent.rs          Python-style colon/indent to brace preprocessor
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
