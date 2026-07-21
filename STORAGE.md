# gum storage model

This is the part that lets you migrate without holding your breath: gum's
storage layout is **byte-for-byte compatible with Solidity's**, verified by the
differential execution tests (they read raw storage slots from both gum's and
Solidity's deployed bytecode and assert equality). Your explorers, indexers, and
proxy patterns keep working because the bytes on chain land exactly where they'd
land in Solidity. This document specifies exactly how gum places state in
storage, and how the storage-layout lock keeps that placement safe across
upgrades.

Everything here concerns **`contract` fields**, the singleton contract
state. Non-`contract` classes are values (in memory or as mapping entries), not
independent storage.

---

## Slots

Contract storage is an array of 2²⁵⁶ slots, each 32 bytes. gum assigns each
`contract` field a slot (and, if it shares one, a byte offset within it).

### Field ordering and packing

Unlike Solidity, which lays fields out in **declaration order**, gum
**reorders fields largest-first** and greedily packs smaller fields into the
gaps. This is the manual optimization Solidity asks *you* to do; gum does it
automatically.

```
contract C:
    u128 a      # 16 bytes
    u256 big    # 32 bytes
    u128 b      # 16 bytes
```

| | layout | slots used |
|---|---|--:|
| Solidity (declaration order) | `a`→slot0, `big`→slot1, `b`→slot2 | 3 |
| gum (size-ordered) | `big`→slot0, `a`+`b`→slot1 | **2** |

Fewer slots means fewer cold `SSTORE`s (~20k gas each). For a
suboptimally-ordered struct this is a real, measurable win. A Solidity author
who hand-orders fields optimally gets the same layout; gum just makes the
optimal layout automatic and mistake-proof.

### Within-slot packing (endianness)

When several fields share a slot, gum packs them **low-order-first**, exactly
like Solidity: the first-placed field occupies the least-significant bytes.

```
slot = ( … | field2 | field1 | field0 )   // field0 in the lowest bytes
```

Reads mask and shift the field out; writes read-modify-write the slot to
preserve neighbors. (Note this is the *opposite* end from gum's *memory*/array
packing, which is big-endian because memory is byte-addressed, an internal
detail, invisible in behavior.)

### Field widths

| type | bytes |
|---|--:|
| `bool`, `u8`, `i8` | 1 |
| `u16`/`i16` … `u128`/`i128` | 2 … 16 |
| `u256`, `i256`, `f32`, `f64`, `Account` | 32 |
| `HashMap(...)`, `[T]` (dynamic), `Vec(T)` | 32 (a slot reserved as length / hash seed) |
| `String`, `Bytes` | 32 (owns a whole slot, see below) |
| `[T; N]` (fixed) | `N × size_of(T)`, **rounded up to whole slots** |
| user `class` (struct value) | sum of its packed field sizes |

A fixed array is *slot-aligned*: it is rounded up to whole slots so it never
shares one with a neighbouring field, exactly as Solidity requires ("array data
always starts a new slot"). Its elements still pack tightly *within* its own
slots. The rounding is what makes that safe: on the packed byte count alone,
two `[u8; 4]` fields both fit in one slot, and both would be handed that slot as
their base, silently aliasing every element.

`Account` occupies a full 32-byte slot even though it is a 160-bit address
(the high bytes are zero), matching Solidity's `address`.

### Storage strings, `String` / `Bytes`

A `String`/`Bytes` field owns one whole slot and uses **Solidity's exact
encoding**, so the slots are readable by anything that understands a Solidity
`string`:

| length | slot contents | data |
|---|---|---|
| ≤ 31 bytes (*short*) | the bytes, left-aligned in the high bytes, with `len × 2` in the **low byte** | in the slot itself |
| ≥ 32 bytes (*long*) | `len × 2 + 1` | consecutive slots from `keccak256(slot)` |

The low bit discriminates: even → short, odd → long. Overwriting a long value
zeroes its old data slots, exactly as Solidity does, so shrinking a string
leaves no stale words behind (and costs the same).

This is verified slot-for-slot against Solidity by
`storage_string_layout_matches_solidity`, which walks the 31/32-byte boundary,
multi-slot values, and the shrink path.

Note this differs from the *memory* representation of a `String` (a `u64 length`
in the high 8 bytes of word 0, then the bytes), codegen converts between them
on every load/store.

---

## Mappings

`HashMap(K, V)` reserves one slot `p` for itself. That slot is never written;
it is only a seed. The value for key `k` lives at:

```
slot(map[k]) = keccak256(k ‖ p)
```

where `k` and `p` are each left-padded to 32 bytes. This is identical to
Solidity's mapping layout. In gum's Yul it is the `gum_hash_slot(k, p)` helper.

### Nested mappings

`HashMap(K, HashMap(K, V))` composes the same rule. `m[a][b]` is:

```
slot(m[a][b]) = keccak256(b ‖ keccak256(a ‖ p))
```

Both the method form `m.get(a).set(b, v)` and the index form `m[a][b] = v`
compile to this, and match Solidity's `mapping(K => mapping(K => V))` exactly.

### Struct-valued mappings

When `V` is a user `class`, `map[k]` is a **struct base slot**
`keccak256(k ‖ p)`, and each field sits at `base + field's relative slot`:

```
class Stake:
    u256 amount    # relative slot 0
    u256 since     # relative slot 1

contract Vault:
    HashMap(Account, Stake) stakes   # slot p

# stakes[who].amount → keccak256(who ‖ p) + 0
# stakes[who].since  → keccak256(who ‖ p) + 1
```

This is exactly Solidity's `mapping(address => Stake)` storage.

### String / Bytes-valued mappings

When `V` is `String` or `Bytes`, `map[k]`'s value slot `keccak256(k ‖ p)`
doubles as the **base slot of a storage string** in its own right: the exact
short/long encoding described under [Storage strings](#storage-strings-string--bytes)
applies, with that value slot standing in for the field slot. A short value
(≤ 31 bytes) packs inline at `keccak256(k ‖ p)`; a long value keeps its length
there and its data at `keccak256(keccak256(k ‖ p))`.

```
contract Names:
    HashMap(Account, String) names   # slot p

# names[who]  short → packed inline at keccak256(who ‖ p)
# names[who]  long  → length at keccak256(who ‖ p),
#                     data from keccak256(keccak256(who ‖ p))
```

This is exactly Solidity's `mapping(address => string)` storage, and is verified
slot-for-slot against Solidity (header slot, data region, round-trip read, key
isolation, and `delete`) by `mapping_string_value_matches_solidity`. A dynamic
*array* value (`HashMap(K, [T])`) would likewise need a per-key region but is not
yet implemented.

---

## Arrays

### Element packing

Both array kinds place elements the same way, matching Solidity: an element
narrower than a word **shares a slot** with its neighbours, filled from the
least-significant end. Given an element of `esz` bytes:

```
per  = 32 / esz          elements per slot   (1 when esz >= 32)
es   = 1                 slots per group     (ceil(esz/32) when esz >= 32)

slot(arr[i])  = base + (i / per) × es
bitoffset(i)  = (i % per) × esz × 8
```

So a `[u8]` puts 32 elements in each slot, a `[u128]` two, a `[u256]` one. Both
cases fall out of the same formula: for a word-or-wider element `per` is 1, the
bit offset is always 0, and it degenerates to one plain `sload`/`sstore` per
slot. Writes are read-modify-write, since the slot-mates are live elements.

### Fixed arrays, `[T; N]`

Stored inline in the class's own (slot-aligned) slots, packed as above from
`base`. Indexing is **bounds-checked**: `i ≥ N` reverts (`Panic(0x32)` with
`--rich-reverts`).

### Dynamic arrays, `[T]`

The field's own slot `p` holds the **length**. Elements live in a separate
region starting at `keccak256(p)`, packed as above:

```
length       = sload(p)
slot(arr[i]) = keccak256(p) + (i / per) × es
```

Identical to Solidity's dynamic-array layout. Operations:

| op | behavior |
|---|---|
| `arr.push(v)` | write `v` at index `length` (read-modify-write, preserving slot-mates), then `length += 1` |
| `arr.pop()` | revert if empty (`Panic(0x31)`); **zero the popped element** (like Solidity, for the gas refund) without disturbing its slot-mates; `length -= 1` |
| `arr[i]` | bounds-checked against `length` (`Panic(0x32)`) |
| `arr.length` | `sload(p)` |
| `delete arr` | zero every occupied slot, then the length |

This is verified slot-for-slot against Solidity by
`packed_storage_array_layout_matches_solidity`, which walks a `uint8[]` across a
slot boundary, a `uint8[4]`, and the push/pop/out-of-bounds/empty-pop paths.

### Storage vectors, `Vec(T)`

A `Vec(T)` **contract field** is a storage vector: the compiler rewrites the
field's type to `[T]`, so it has exactly the layout and operations above, and
takes either spelling (`v[i]` or `v.get(i)`, `.length` or `.len()`).

std's `Vec` (the memory one) carries a capacity and reallocates on growth,
which is why it makes you write `v = v.push(x)`. Storage is sparse and
unbounded, so there is nothing to reallocate and no capacity to track: a storage
push mutates in place.

### Struct elements, `[Stake]`

An array whose element is a user `class` gives each element **whole slots**,
never packing two into one, exactly as Solidity specifies ("array elements of
struct type always occupy whole slots"). With `es = ceil(size_of(T)/32)` slots
per element:

```
base(arr[i])        = keccak256(p) + i × es      // dynamic
base(arr[i])        = p + i × es                 // fixed
slot(arr[i].field)  = base(arr[i]) + field's relative slot
```

which is the same rule a struct behind a mapping follows, only the base differs
(an index instead of a hash), so both go through the same field addressing.

```
class Stake:
    u256 amount    # relative slot 0
    u256 since     # relative slot 1

contract V:
    [Stake] stakes           # slot p; element i at keccak256(p) + i*2

    export fn add(u256 a, u256 s):
        V.stakes.push()      # append a zeroed element
        V.stakes[V.stakes.length - 1].amount = a
        V.stakes[V.stakes.length - 1].since = s
```

`push()` on a struct array **takes no argument**. gum has no struct copy: a
struct is either a memory value or a set of storage slots, and moving one to the
other is a field-by-field job `push` cannot do implicitly. So it appends a zeroed
element (the slots are already zero, never written, or cleared by a prior `pop`)
and you then set the fields in place, the same way a struct in a mapping is
written. `pop()` zeroes all `es` slots of the removed element before shrinking.

Verified slot-for-slot against Solidity by `struct_array_layout_matches_solidity`,
which diffs the raw storage of both after pushes, an overwrite, a pop, and the
out-of-bounds and empty-pop panics.

Note `[Account]` is *not* a struct array: `Account` is declared as a class but
the compiler treats it as the EVM address scalar throughout, so it packs and is
pushed by value like any other word-wide element.

---

## Transient storage, `transient`

A `contract` field marked `transient` lives in **transient storage**
(EIP-1153): `TSTORE`/`TLOAD` instead of `SSTORE`/`SLOAD`, cleared at the end of
the **transaction**.

```
contract Router:
    u256 total                      # persistent
    transient u256 depth            # transient
    transient [Account] touched     # so are collections
    transient HashMap(Account, bool) seen
    transient String note
```

**Every layout rule above carries over unchanged**, slot packing,
`keccak256(k ‖ p)` for a mapping, `keccak256(p) + i` for an array's data,
the short/long storage-string encoding. Transient storage is the same 256-bit
key/value space with the same addressing; only the opcode differs. So a
transient `[T]`, `HashMap`, or `String` works exactly like its persistent twin.

Two things that are **not** the same:

* **Its own keyspace.** Transient slot 0 and persistent slot 0 are different
  locations. Transient fields are therefore numbered from 0 independently, and
  a transient field and a persistent one routinely share a slot number without
  interacting.
* **Never in the storage lock.** The lock (below) exists to keep a proxy's
  *existing* storage readable across an upgrade. A transient field holds nothing
  across a transaction, so there is no inherited storage a moved slot could
  corrupt, they are omitted from the manifest entirely.

### The catch: it clears per *transaction*, not per call

Transient storage survives an external call and back. A transient value written
early in a transaction is still there after your contract calls out and is
re-entered, and is still there for the next call in the same transaction (a
multicall, a router batching two swaps). That is exactly what makes it right for
a reentrancy lock, and exactly what makes it wrong for a scratch buffer you
assume starts empty. If a later call in the same transaction must not see it,
clear it (`delete`) before returning.

### Cost

`transient` costs nothing to have. The storage kind is resolved at compile time
and each helper is emitted once per kind actually used, so a contract with no
transient fields produces byte-for-byte the bytecode it did before this existed,
and one using both pays only for the helpers it names.

Gas-wise `TSTORE`/`TLOAD` are ~100 gas against `SSTORE`'s 2,900–20,000, the
reason to reach for it.

> Note: Solidity's `transient` (0.8.28+) covers value types only, no transient
> mappings or dynamic arrays. gum's collections therefore have no Solidity twin
> to diff against, unlike everything else here; they are verified against the
> EVM's own behaviour under revm instead.

---

## Const fields, `const`

A `contract` field marked `const` never changes once the contract is deployed,
and is **not storage at all**. It is assigned exactly once, in `fn new()`, and
lives in the contract's own bytecode, so a read is a `PUSH32` (~3 gas), not a
2100-gas cold `SLOAD`. It occupies no slot.

```
contract Cfg:
    const Account owner    # from a constructor arg -> fixed at deploy
    const u256 cap         # from a literal         -> fixed at compile time
    u256 counter           # ordinary storage, slot 0

    fn new(Account o):
        Cfg.owner = o
        Cfg.cap = 100

    export fn get_owner() -> Account:
        return Cfg.owner   # ~3 gas, no storage access
```

### The compiler picks the mechanism

You state the intent, *this never changes after deploy*, and gum works out how
to keep it. There is no second keyword to choose, because the choice follows
from a question the compiler can answer for itself: **do I already know this
value?**

| the constructor assigns… | how it is carried | what it costs |
|---|---|---|
| a literal (`Cfg.cap = 100`) | inlined at every use | **nothing**, byte-for-byte the same code as writing `100` by hand |
| anything else (`Cfg.owner = o`) | written into the runtime bytecode by the deploy code (Yul `setimmutable`) | a `PUSH32` per read, patched once at deploy |

A constructor argument genuinely cannot be a compile-time constant: one compiled
contract is deployed many times, with a different value each time. That is the
case the deploy-time patch exists for, and it is verified by deploying identical
creation code twice with different arguments and reading back different values.

Measured, against the same contract with `owner` as an ordinary field:

| | deploy | a read |
|---|--:|--:|
| `const Account owner` | 69,817 | **21,160** |
| `Account owner` | 85,601 | 23,246 |

2,086 gas off the read (a cold `SLOAD` at 2100 traded for a `PUSH32` at 3) and
~15.8k off deploy, since the constructor no longer pays a cold `SSTORE`. It
compounds: a value read on every call pays that 2,100 forever.

Const fields **occupy no slot**, so declaring one never shifts a real field's
position (`counter` above is slot 0 despite being declared third), and they are
absent from the storage lock, having no storage an upgrade could orphan.

The fold is deliberately narrow, since a wrong value here is baked into a
contract forever. It applies only to a single unconditional assignment of a
literal that fits the field's type. A conditional assignment, a second write, or
a literal needing truncation all keep the deploy-time patch, which is always
correct, just larger.

### Rules, and why each exists

The bytecode is writable for exactly one moment: after the deploy block copies
it into memory and before it returns it. Everything below follows from that, and
each is a compile error rather than a silently-zero field:

* **Only `fn new()` may assign one.** Any other method runs after the code is
  final. (`'C.s' assigns const field 'a' … Drop `const` to make it an ordinary
  storage field you can write.`)
* **`fn new()` must assign every one, on every path.** See below.
* **`fn new()` may not *read* one.** Its value is only fixed once the
  constructor finishes, so there is nothing to read during it. Use the value
  being assigned, or a constructor parameter.
* **A contract with a `const` field needs a `fn new()`.** With no constructor
  nothing could ever set it.
* **Not on a plain `class`**, which is a memory value with no deployed code.
* **Not `transient` as well**, that is storage that clears each transaction;
  this is not storage.

### Definite assignment

"Assigned somewhere" is not enough: on any path that skips the assignment the
field is fixed at zero for the life of the contract. So the compiler checks that
every path reaching the end of `fn new()` has assigned it.

```
fn new(u256 x, bool c):
    if c:
        C.a = x            # error: not assigned on every path
```

A branch satisfies the rule by either assigning or **diverging**, a branch that
reverts never produces a contract, so it owes nothing:

```
fn new(u256 x, bool c):
    if c:
        C.a = x
    else:
        revert Bad()       # fine: the other path never deploys
```

A **loop never counts**, since it may run zero times:

```
fn new(u256 x, bool c):
    while c:
        C.a = x            # error: not assigned on every path
```

A `match` counts when every arm assigns or diverges, which is sound because a
non-exhaustive `match` is already rejected.

> Not for proxies. A `const` field is fixed in the *implementation's* code at
> its own deploy, so a proxy delegatecalling into it sees the implementation's
> value, not one of its own, and a value that arrives later, via a
> `once fn initialize`, cannot be `const` at all. This is the same constraint
> Solidity has, and the reason [`amm.gum`](amm.gum) keeps `token_a`/`token_b`
> as ordinary fields. Drop `const` whenever the value is set after deploy.

---

## Determinism

Compilation is a deterministic function of the source: the same input always
produces byte-identical output. That is what lets a deployed contract be
verified against the code it came from.

Three places decide an order, and none of them may be a hash map:

- **Field placement** walks classes in registration order (declaration, then
  import order), so the same source always gets the same slots. The upgrade
  lock below is built on this.
- **Helper functions** are emitted in name order.
- **Class methods** are emitted in that same registration order.

Rust randomizes hash-map iteration per process, so a hash map in any of these
paths means the same source compiles to different, though equivalent, bytecode
on every run. Field placement always avoided that; the two codegen paths did
not, and were fixed once a test caught it.
`compiling_the_same_source_twice_gives_the_same_output` compiles a reference
contract in four separate processes, since that is where the randomness lives,
and diffs the output.

---

## Upgrade safety: the storage lock

Automatic field reordering is great for gas but dangerous for **proxy
upgrades**: appending a field can change the *size-sorted order*, silently
moving existing fields to different slots and corrupting the storage a proxy
already holds. Determinism alone does not prevent this, you need *stability
under append*.

gum solves this with an opt-in **storage lock** (`--lock <file>`):

```sh
# At first deploy: writes the layout manifest. Commit it to version control.
gumc contract.gum --lock layout.json

# On every later compile: the committed layout is enforced.
gumc contract_v2.gum --lock layout.json
```

Under a lock:

1. **Committed fields keep their exact slot and offset**, never reordered,
   even if a new field would sort ahead of them.
2. **New fields are appended** into leftover gaps in committed slots, or into
   fresh slots beyond all committed storage.
3. **Unsafe changes are compile errors**:
   - removing a committed field →
     *"committed field 'x' was removed … existing storage would be orphaned"*
   - changing a committed field's byte width →
     *"field 'x' changed size (16 → 32 bytes) … would move or overlap committed storage"*

This turns the #1 upgrade footgun, *"did I break my proxy's storage?"*, into
a compile-time check, which is stronger than what Solidity gives you by
default. You get the aggressive-packing gas win on v1 *and* provable layout
stability forever after.

### Manifest format

```json
{
  "version": 1,
  "classes": {
    "Vault": {
      "fields": {
        "total":  { "slot": 0, "offset": 0, "size": 32, "type": "u256" },
        "stakes": { "slot": 1, "offset": 0, "size": 32, "type": "HashMapAccount_Stake" }
      }
    }
  }
}
```

Diff-friendly (stable key order); commit it alongside the source.
