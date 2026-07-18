use crate::ast::*;
use crate::codegen::abi::{AbiGenerator, AbiInput};
use crate::codegen::layout::{LayoutEngine, MemoryField, StorageField, immutable_key};
use crate::semantic::{TypeChecker, super_name};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use tiny_keccak::{Hasher, Keccak};

// The Yul local holding an immutable's value while fn new runs. The
// constructor returns these, and the deploy block feeds them to setimmutable
// once the runtime code is in memory. Prefixed to keep it out of the way of
pub fn immutable_local(field: &str) -> String {
    format!("_imm_{}", field)
}

// The deploy block's own binding for an immutable, distinct from
// immutable_local. Yul permits no shadowing whatsoever, not even a
// function's return variable over an outer let, and the constructor's
pub fn immutable_deploy_local(field: &str) -> String {
    format!("_immv_{}", field)
}

// All-1s bitmask covering exactly size bytes, right-justified, used to
fn mask_hex(size: usize) -> String {
    format!("0x{}", "f".repeat(size * 2))
}

// Copies len bytes from src to dst in 32-byte strides. May overwrite up
// to 31 bytes past len at the tail end, harmless here since every caller
// either immediately overwrites that tail with the next chunk, or it spills
fn gum_str_len_helper_src() -> String {
    "function gum_str_len(p) -> n {\n    n := and(shr(192, mload(p)), 0xffffffffffffffff)\n}\n"
        .to_string()
}

// Decimal itoa: an unsigned value to a gum String (header word holding the
// length in its top 8 bytes, ASCII digits from p+32). Digits are written from
// the least-significant end backwards, the same shape as OpenZeppelin's
fn gum_uint_to_str_helper_src() -> String {
    "function gum_uint_to_str(value) -> ptr {\n\
     \x20   let len := 1\n\
     \x20   let tmp := value\n\
     \x20   for { } gt(tmp, 9) { } { tmp := div(tmp, 10) len := add(len, 1) }\n\
     \x20   let padded := and(add(len, 31), not(31))\n\
     \x20   ptr := allocate_memory(add(32, padded))\n\
     \x20   mstore(ptr, shl(192, len))\n\
     \x20   switch value\n\
     \x20   case 0 { mstore8(add(ptr, 32), 48) }\n\
     \x20   default {\n\
     \x20       let cursor := add(add(ptr, 32), len)\n\
     \x20       let v := value\n\
     \x20       for { } gt(v, 0) { } {\n\
     \x20           cursor := sub(cursor, 1)\n\
     \x20           mstore8(cursor, add(48, mod(v, 10)))\n\
     \x20           v := div(v, 10)\n\
     \x20       }\n\
     \x20   }\n\
     }\n"
        .to_string()
}

// String and Bytes share one in-memory shape, so every operation below works
// on either.
// Whether t is a user struct for storage-addressing purposes: an aggregate
pub fn is_struct_type(tc: &TypeChecker, t: &Type) -> bool {
    match t {
        Type::Primitive(name) => {
            name != "Account" && !is_str_type(t) && tc.loaded_classes.contains_key(name)
        }
        _ => false,
    }
}

pub fn is_str_type(t: &Type) -> bool {
    matches!(t, Type::Primitive(n) if n == "String" || n == "Bytes")
}

// Whether t is an enum that crosses the ABI: payload-free, so it is a plain u8 tag.
pub fn is_enum_type(tc: &TypeChecker, t: &Type) -> bool {
    tc.is_scalar_enum(t)
}

// Materializes a compile-time-known string into ptr as a String value.
//
// The content is written a word at a time (one mstore per 32 chars),
fn str_literal_body_src(ptr: &str, s: &str) -> String {
    let bytes = s.as_bytes();
    let padded = (bytes.len() + 31) / 32 * 32;
    let mut out = String::new();
    out.push_str(&format!(
        "    {} := allocate_memory({})\n",
        ptr,
        32 + padded
    ));
    out.push_str(&format!("    mstore({}, shl(192, {}))\n", ptr, bytes.len()));
    for (i, chunk) in bytes.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        let hex: String = word.iter().map(|b| format!("{:02x}", b)).collect();
        out.push_str(&format!(
            "    mstore(add({}, {}), 0x{})\n",
            ptr,
            32 + i * 32,
            hex
        ));
    }
    out
}

// Content equality for String/Bytes. Compares length first, then whole 32-byte
// words, then the trailing partial word with the bytes past the length shifted
// off, so leftover allocator garbage after the last byte can never make two
fn gum_str_eq_helper_src() -> String {
    "function gum_str_eq(a, b) -> r {\n\
     \x20   r := 0\n\
     \x20   let la := gum_str_len(a)\n\
     \x20   if iszero(eq(la, gum_str_len(b))) { leave }\n\
     \x20   r := 1\n\
     \x20   let full := div(la, 32)\n\
     \x20   let i := 0\n\
     \x20   for {} lt(i, full) { i := add(i, 1) } {\n\
     \x20       let off := add(32, mul(i, 32))\n\
     \x20       if iszero(eq(mload(add(a, off)), mload(add(b, off)))) { r := 0 leave }\n\
     \x20   }\n\
     \x20   let rem := mod(la, 32)\n\
     \x20   if rem {\n\
     \x20       let off := add(32, mul(full, 32))\n\
     \x20       let sh := mul(8, sub(32, rem))\n\
     \x20       if iszero(eq(shr(sh, mload(add(a, off))), shr(sh, mload(add(b, off))))) { r := 0 leave }\n\
     \x20   }\n\
     }\n".to_string()
}

// Reverts with Selector(string), one 4-byte selector, a head holding the
// single tail offset (always 0x20), then the string's length word and its
// zero-padded bytes.
fn gum_revert_str_helper_src() -> String {
    "function gum_revert_str(sel, s) {\n\
     \x20   let p := mload(0x40)\n\
     \x20   mstore(p, sel)\n\
     \x20   mstore(add(p, 4), 0x20)\n\
     \x20   let len := gum_str_len(s)\n\
     \x20   mstore(add(p, 36), len)\n\
     \x20   let pad := and(add(len, 31), not(31))\n\
     \x20   if pad { mstore(add(p, add(36, pad)), 0) }\n\
     \x20   gum_memory_copy(add(s, 32), add(p, 68), len)\n\
     \x20   revert(p, add(68, pad))\n\
     }\n"
    .to_string()
}

// --- Solidity-compatible storage strings ---
//
// A String/Bytes contract field occupies exactly one slot, encoded the way

fn gum_sstr_base_helper_src() -> String {
    "function gum_sstr_base(slot) -> b {\n\
     \x20   mstore(0x00, slot)\n\
     \x20   b := keccak256(0x00, 0x20)\n\
     }\n"
    .to_string()
}

// Substitutes the storage kind into a helper template. format! would need
fn with_kind(template: &str, tr: bool) -> String {
    template
        .replace("{K}", kind_suffix(tr))
        .replace("{LD}", ld_op(tr))
        .replace("{ST}", st_op(tr))
}

// Loads a storage string into a fresh memory String.
fn gum_sstr_load_helper_src(tr: bool) -> String {
    with_kind(
        "function gum_sstr_load{K}(slot) -> ptr {\n\
     \x20   let s := {LD}(slot)\n\
     \x20   switch and(s, 1)\n\
     \x20   case 0 {\n\
     \x20       let n := shr(1, and(s, 0xff))\n\
     \x20       ptr := allocate_memory(64)\n\
     \x20       mstore(ptr, shl(192, n))\n\
     \x20       mstore(add(ptr, 32), and(s, not(0xff)))\n\
     \x20   }\n\
     \x20   default {\n\
     \x20       let n := shr(1, s)\n\
     \x20       let padded := and(add(n, 31), not(31))\n\
     \x20       ptr := allocate_memory(add(32, padded))\n\
     \x20       mstore(ptr, shl(192, n))\n\
     \x20       let base := gum_sstr_base(slot)\n\
     \x20       let i := 0\n\
     \x20       for {} lt(mul(i, 32), padded) { i := add(i, 1) } {\n\
     \x20           mstore(add(add(ptr, 32), mul(i, 32)), {LD}(add(base, i)))\n\
     \x20       }\n\
     \x20   }\n\
     }\n",
        tr,
    )
}

// delete on a storage string: release the long form's data slots (leaving
fn gum_sstr_clear_helper_src(tr: bool) -> String {
    with_kind(
        "function gum_sstr_clear{K}(slot) {\n\
     \x20   let old := {LD}(slot)\n\
     \x20   if and(old, 1) {\n\
     \x20       let oldslots := div(add(shr(1, old), 31), 32)\n\
     \x20       let ob := gum_sstr_base(slot)\n\
     \x20       for { let j := 0 } lt(j, oldslots) { j := add(j, 1) } { {ST}(add(ob, j), 0) }\n\
     \x20   }\n\
     \x20   {ST}(slot, 0)\n\
     }\n",
        tr,
    )
}

// Stores a memory String into a storage string slot.
//
// Old long-form data slots are zeroed first: Solidity releases them on
fn gum_sstr_store_helper_src(tr: bool) -> String {
    with_kind(
        "function gum_sstr_store{K}(slot, ptr) {\n\
     \x20   let old := {LD}(slot)\n\
     \x20   if and(old, 1) {\n\
     \x20       let oldn := shr(1, old)\n\
     \x20       let oldslots := div(add(oldn, 31), 32)\n\
     \x20       let ob := gum_sstr_base(slot)\n\
     \x20       let j := 0\n\
     \x20       for {} lt(j, oldslots) { j := add(j, 1) } { {ST}(add(ob, j), 0) }\n\
     \x20   }\n\
     \x20   let n := gum_str_len(ptr)\n\
     \x20   switch lt(n, 32)\n\
     \x20   case 1 {\n\
     \x20       let w := and(mload(add(ptr, 32)), not(shr(mul(8, n), not(0))))\n\
     \x20       {ST}(slot, or(w, mul(n, 2)))\n\
     \x20   }\n\
     \x20   default {\n\
     \x20       {ST}(slot, add(mul(n, 2), 1))\n\
     \x20       let base := gum_sstr_base(slot)\n\
     \x20       let full := div(n, 32)\n\
     \x20       let i := 0\n\
     \x20       for {} lt(i, full) { i := add(i, 1) } {\n\
     \x20           {ST}(add(base, i), mload(add(add(ptr, 32), mul(i, 32))))\n\
     \x20       }\n\
     \x20       let rem := mod(n, 32)\n\
     \x20       if rem {\n\
     \x20           let lw := and(mload(add(add(ptr, 32), mul(full, 32))), not(shr(mul(8, rem), not(0))))\n\
     \x20           {ST}(add(base, full), lw)\n\
     \x20       }\n\
     \x20   }\n\
     }\n",
        tr,
    )
}

// keccak256 over a dynamic byte value's contents.
//
// Yul's keccak256 takes (offset, length); gum's takes one value, so the length
fn gum_keccak_arr_helper_src() -> String {
    "function gum_keccak_arr(p) -> h {\n    h := keccak256(add(p, 32), mload(p))\n}\n".to_string()
}

fn gum_keccak_str_helper_src() -> String {
    "function gum_keccak_str(p) -> h {\n    h := keccak256(add(p, 32), gum_str_len(p))\n}\n"
        .to_string()
}

// secp256k1 public-key recovery via the precompile at address 1.
//
// Returns the zero address when recovery fails. The returndatasize check is
fn gum_ecrecover_helper_src() -> String {
    "function gum_ecrecover(h, v, r, s) -> a {\n\
     \x20   let p := mload(0x40)\n\
     \x20   mstore(p, h)\n\
     \x20   mstore(add(p, 32), v)\n\
     \x20   mstore(add(p, 64), r)\n\
     \x20   mstore(add(p, 96), s)\n\
     \x20   a := 0\n\
     \x20   if staticcall(gas(), 1, p, 128, add(p, 128), 32) {\n\
     \x20       if eq(returndatasize(), 32) { a := mload(add(p, 128)) }\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// to.pay(amount), send ETH.
//
// EIP-5920's PAY opcode would do this without handing control to the recipient
fn gum_pay_helper_src() -> String {
    "function gum_pay(to, amount) -> ok {\n\
     \x20   ok := call(gas(), to, amount, 0, 0, 0, 0)\n\
     }\n"
    .to_string()
}

// --- Storage kind ---
//
// A field is either persistent (SLOAD/SSTORE) or transient (TLOAD/TSTORE,
fn kind_suffix(tr: bool) -> &'static str {
    if tr { "_t" } else { "" }
}

fn ld_op(tr: bool) -> &'static str {
    if tr { "tload" } else { "sload" }
}

fn st_op(tr: bool) -> &'static str {
    if tr { "tstore" } else { "sstore" }
}

// Bounds-checked element address for a dynamic memory array.
//
// Word 0 holds the length in bytes, not elements, the convention the
fn gum_marr_addr_helper_src(rich: bool) -> String {
    format!(
        "function gum_marr_addr(ptr, i, esz) -> a {{\n\
     \x20   if iszero(lt(i, div(mload(ptr), esz))) {{ {} }}\n\
     \x20   a := add(add(ptr, 32), mul(i, esz))\n\
     }}\n",
        panic_revert(rich, PANIC_OOB)
    )
}

// Same for a fixed memory array, whose length is static and which has no length
fn gum_farr_addr_helper_src(rich: bool) -> String {
    format!(
        "function gum_farr_addr(ptr, i, n, esz) -> a {{\n\
     \x20   if iszero(lt(i, n)) {{ {} }}\n\
     \x20   a := add(ptr, mul(i, esz))\n\
     }}\n",
        panic_revert(rich, PANIC_OOB)
    )
}

// --- Dynamic-array ABI ---
//
// The wire format and gum's memory format are not the same shape, which is why
fn gum_abi_arr_cd_helper_src() -> String {
    "function gum_abi_arr_cd(off, esz) -> ptr {\n\
     \x20   if lt(calldatasize(), add(off, 32)) { revert(0, 0) }\n\
     \x20   let n := calldataload(off)\n\
     \x20   if gt(n, div(sub(calldatasize(), add(off, 32)), 32)) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(add(32, and(add(mul(n, esz), 31), not(31))))\n\
     \x20   mstore(ptr, mul(n, esz))\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := shl(sh, sub(shl(mul(esz, 8), 1), 1))\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       let a := add(add(ptr, 32), mul(i, esz))\n\
     \x20       let v := calldataload(add(add(off, 32), mul(i, 32)))\n\
     \x20       mstore(a, or(and(mload(a), not(m)), and(shl(sh, v), m)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// Same, reading from an ABI blob already in memory (the constructor's args,
fn gum_abi_arr_mem_helper_src() -> String {
    "function gum_abi_arr_mem(base, off, limit, esz) -> ptr {\n\
     \x20   if lt(limit, add(off, 32)) { revert(0, 0) }\n\
     \x20   let n := mload(add(base, off))\n\
     \x20   if gt(n, div(sub(limit, add(off, 32)), 32)) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(add(32, and(add(mul(n, esz), 31), not(31))))\n\
     \x20   mstore(ptr, mul(n, esz))\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := shl(sh, sub(shl(mul(esz, 8), 1), 1))\n\
     \x20   let src := add(add(base, off), 32)\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       let a := add(add(ptr, 32), mul(i, esz))\n\
     \x20       let v := mload(add(src, mul(i, 32)))\n\
     \x20       mstore(a, or(and(mload(a), not(m)), and(shl(sh, v), m)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// A fixed array is not a dynamic one on the wire: T[N] is N inline words in
// the head, with no offset and no count. It still needs converting rather than
// copying, for the same reason, memory packs at esz, the wire doesn't.
fn gum_abi_farr_cd_helper_src() -> String {
    "function gum_abi_farr_cd(off, n, esz) -> ptr {\n\
     \x20   if lt(calldatasize(), add(off, mul(n, 32))) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(and(add(mul(n, esz), 31), not(31)))\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := shl(sh, sub(shl(mul(esz, 8), 1), 1))\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       let a := add(ptr, mul(i, esz))\n\
     \x20       let v := calldataload(add(off, mul(i, 32)))\n\
     \x20       mstore(a, or(and(mload(a), not(m)), and(shl(sh, v), m)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

fn gum_abi_farr_mem_helper_src() -> String {
    "function gum_abi_farr_mem(base, off, limit, n, esz) -> ptr {\n\
     \x20   if lt(limit, add(off, mul(n, 32))) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(and(add(mul(n, esz), 31), not(31)))\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := shl(sh, sub(shl(mul(esz, 8), 1), 1))\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       let a := add(ptr, mul(i, esz))\n\
     \x20       let v := mload(add(add(base, off), mul(i, 32)))\n\
     \x20       mstore(a, or(and(mload(a), not(m)), and(shl(sh, v), m)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

fn gum_abi_farr_put_helper_src() -> String {
    "function gum_abi_farr_put(dst, ptr, n, esz) {\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := sub(shl(mul(esz, 8), 1), 1)\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       mstore(add(dst, mul(i, 32)), and(shr(sh, mload(add(ptr, mul(i, esz)))), m))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// Memory array -> ABI, written at dst. Returns the bytes written, so a caller
fn gum_abi_arr_put_helper_src() -> String {
    "function gum_abi_arr_put(dst, ptr, esz) -> written {\n\
     \x20   let n := div(mload(ptr), esz)\n\
     \x20   mstore(dst, n)\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := sub(shl(mul(esz, 8), 1), 1)\n\
     \x20   let src := add(ptr, 32)\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       mstore(add(add(dst, 32), mul(i, 32)), and(shr(sh, mload(add(src, mul(i, esz)))), m))\n\
     \x20   }\n\
     \x20   written := add(32, mul(n, 32))\n\
     }\n".to_string()
}

// The ABI size of a memory array: a count word plus one word per element,
fn gum_abi_arr_size_helper_src() -> String {
    "function gum_abi_arr_size(ptr, esz) -> sz {\n\
     \x20   sz := add(32, mul(div(mload(ptr), esz), 32))\n\
     }\n"
    .to_string()
}

// A dynamic element carries an offset rather than its bytes, so the wire grows a level: [count][off_0..off_n-1][tail_0][tail_1]...
fn abi_dynarr_cd_helper_src(fname: &str, inner_cd: &str) -> String {
    format!(
        "function {fname}(off) -> ptr {{\n\
         \x20   if lt(calldatasize(), add(off, 32)) {{ revert(0, 0) }}\n\
         \x20   let n := calldataload(off)\n\
         \x20   let base := add(off, 32)\n\
         \x20   if gt(n, div(sub(calldatasize(), base), 32)) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory(add(32, mul(n, 32)))\n\
         \x20   mstore(ptr, mul(n, 32))\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       let eo := calldataload(add(base, mul(i, 32)))\n\
         \x20       if gt(eo, calldatasize()) {{ revert(0, 0) }}\n\
         \x20       mstore(add(add(ptr, 32), mul(i, 32)), {inner_cd}(add(base, eo)))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_cd = inner_cd
    )
}

// Same shape from an ABI blob already in memory, which is how constructor args arrive.
fn abi_dynarr_mem_helper_src(fname: &str, inner_mem: &str) -> String {
    format!(
        "function {fname}(base, off, limit) -> ptr {{\n\
         \x20   if lt(limit, add(off, 32)) {{ revert(0, 0) }}\n\
         \x20   let n := mload(add(base, off))\n\
         \x20   let tbl := add(off, 32)\n\
         \x20   if gt(n, div(sub(limit, tbl), 32)) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory(add(32, mul(n, 32)))\n\
         \x20   mstore(ptr, mul(n, 32))\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       let eo := mload(add(base, add(tbl, mul(i, 32))))\n\
         \x20       if gt(eo, limit) {{ revert(0, 0) }}\n\
         \x20       mstore(add(add(ptr, 32), mul(i, 32)), {inner_mem}(base, add(tbl, eo), limit))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_mem = inner_mem
    )
}

// Memory -> ABI. The offset table is laid out first at a known width, then each tail is appended and its offset backfilled, so the cursor runs ahead of the loop rather than being computable up front.
fn abi_dynarr_put_helper_src(fname: &str, inner_put: &str) -> String {
    format!(
        "function {fname}(dst, ptr) -> written {{\n\
         \x20   let n := div(mload(ptr), 32)\n\
         \x20   mstore(dst, n)\n\
         \x20   let tbl := add(dst, 32)\n\
         \x20   let cur := mul(n, 32)\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       mstore(add(tbl, mul(i, 32)), cur)\n\
         \x20       let e := mload(add(add(ptr, 32), mul(i, 32)))\n\
         \x20       cur := add(cur, {inner_put}(add(tbl, cur), e))\n\
         \x20   }}\n\
         \x20   written := add(32, cur)\n\
         }}\n",
        fname = fname,
        inner_put = inner_put
    )
}

// The ABI size of a nested array: the count, the offset table, then each element's own size, which only the element codec can answer.
fn abi_dynarr_size_helper_src(fname: &str, inner_size: &str) -> String {
    format!(
        "function {fname}(ptr) -> sz {{\n\
         \x20   let n := div(mload(ptr), 32)\n\
         \x20   sz := add(32, mul(n, 32))\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       sz := add(sz, {inner_size}(mload(add(add(ptr, 32), mul(i, 32)))))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_size = inner_size
    )
}

// A fixed array of dynamic elements is itself dynamic, so it sits behind an offset like a dynamic array, but with no count word: the length is in the type.
fn abi_dynfarr_cd_helper_src(fname: &str, inner_cd: &str, n: usize) -> String {
    format!(
        "function {fname}(off) -> ptr {{\n\
         \x20   if lt(calldatasize(), add(off, {bytes})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory({bytes})\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       let eo := calldataload(add(off, mul(i, 32)))\n\
         \x20       if gt(eo, calldatasize()) {{ revert(0, 0) }}\n\
         \x20       mstore(add(ptr, mul(i, 32)), {inner_cd}(add(off, eo)))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_cd = inner_cd,
        n = n,
        bytes = n * 32
    )
}

fn abi_dynfarr_mem_helper_src(fname: &str, inner_mem: &str, n: usize) -> String {
    format!(
        "function {fname}(base, off, limit) -> ptr {{\n\
         \x20   if lt(limit, add(off, {bytes})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory({bytes})\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       let eo := mload(add(base, add(off, mul(i, 32))))\n\
         \x20       if gt(eo, limit) {{ revert(0, 0) }}\n\
         \x20       mstore(add(ptr, mul(i, 32)), {inner_mem}(base, add(off, eo), limit))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_mem = inner_mem,
        n = n,
        bytes = n * 32
    )
}

fn abi_dynfarr_put_helper_src(fname: &str, inner_put: &str, n: usize) -> String {
    format!(
        "function {fname}(dst, ptr) -> written {{\n\
         \x20   let cur := {bytes}\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       mstore(add(dst, mul(i, 32)), cur)\n\
         \x20       cur := add(cur, {inner_put}(add(dst, cur), mload(add(ptr, mul(i, 32)))))\n\
         \x20   }}\n\
         \x20   written := cur\n\
         }}\n",
        fname = fname,
        inner_put = inner_put,
        n = n,
        bytes = n * 32
    )
}

fn abi_dynfarr_size_helper_src(fname: &str, inner_size: &str, n: usize) -> String {
    format!(
        "function {fname}(ptr) -> sz {{\n\
         \x20   sz := {bytes}\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       sz := add(sz, {inner_size}(mload(add(ptr, mul(i, 32)))))\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_size = inner_size,
        n = n,
        bytes = n * 32
    )
}

// The put half of a dynamic array of static elements, for an element codec that reports what it wrote.
fn abi_statarr_put_helper_src(fname: &str, inner_put: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(dst, ptr) -> written {{\n\
         \x20   let n := div(mload(ptr), {packed})\n\
         \x20   mstore(dst, n)\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       pop({inner_put}(add(add(dst, 32), mul(i, {wire})), add(add(ptr, 32), mul(i, {packed}))))\n\
         \x20   }}\n\
         \x20   written := add(32, mul(n, {wire}))\n\
         }}\n",
        fname = fname,
        inner_put = inner_put,
        wire = wire,
        packed = packed
    )
}

// A fixed array of static elements: the elements are inline on the wire and inline in memory, at different strides, so this is the struct-array walk with the count coming from the type instead of the data.
fn abi_statfarr_cd_helper_src(fname: &str, inner_cd: &str, n: usize, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(off) -> ptr {{\n\
         \x20   if lt(calldatasize(), add(off, {total})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory({mtotal})\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       let e := {inner_cd}(add(off, mul(i, {wire})))\n\
         \x20       gum_memory_copy(e, add(ptr, mul(i, {packed})), {packed})\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_cd = inner_cd,
        n = n,
        wire = wire,
        packed = packed,
        total = n * wire,
        mtotal = n * packed
    )
}

fn abi_statfarr_mem_helper_src(fname: &str, inner_mem: &str, n: usize, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(base, off, limit) -> ptr {{\n\
         \x20   if lt(limit, add(off, {total})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory({mtotal})\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       let e := {inner_mem}(base, add(off, mul(i, {wire})), limit)\n\
         \x20       gum_memory_copy(e, add(ptr, mul(i, {packed})), {packed})\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        inner_mem = inner_mem,
        n = n,
        wire = wire,
        packed = packed,
        total = n * wire,
        mtotal = n * packed
    )
}

fn abi_statfarr_put_helper_src(fname: &str, inner_put: &str, n: usize, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(dst, ptr) -> written {{\n\
         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
         \x20       {inner_put}(add(dst, mul(i, {wire})), add(ptr, mul(i, {packed})))\n\
         \x20   }}\n\
         \x20   written := {total}\n\
         }}\n",
        fname = fname,
        inner_put = inner_put,
        n = n,
        wire = wire,
        packed = packed,
        total = n * wire
    )
}

// One field of a struct as it crosses the ABI: where it sits in the packed memory form, and how wide it is there.
#[derive(Clone, Copy)]
pub struct AbiStructField {
    mem_offset: usize,
    width: usize,
    is_addr: bool,
}

// A stable, unique suffix naming one type's codecs, so two functions taking the same type share a decoder and two different types never collide on one.
fn abi_mangle(t: &Type) -> String {
    match t {
        Type::Primitive(n) => n.clone(),
        Type::Array(i) => format!("arr_{}", abi_mangle(i)),
        Type::FixedArray(i, n) => format!("farr{}_{}", n, abi_mangle(i)),
        Type::Generic { name, args } => {
            let inner: Vec<String> = args.iter().map(abi_mangle).collect();
            format!("{}_{}", name, inner.join("_"))
        }
    }
}

// Whether a type is a scalar the ABI can put in exactly one word.
pub fn is_abi_scalar(t: &Type) -> bool {
    matches!(t, Type::Primitive(n) if matches!(n.as_str(),
        "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
        "i8" | "i16" | "i32" | "i64" | "i128" | "i256" |
        "bool" | "Account") || byte_width(n).is_some())
}

// Moves one wire word into its packed memory field. Split out because the calldata and memory decoders differ only in how they fetch the word.
fn abi_st_field_store(raw: &str, f: &AbiStructField) -> String {
    let v = if f.is_addr {
        format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", raw)
    } else {
        raw.to_string()
    };
    let addr = format!("add(ptr, {})", f.mem_offset);
    if f.width >= 32 {
        format!("    mstore({}, {})\n", addr, v)
    } else {
        let merged = write_packed(&format!("mload({})", addr), 0, f.width, &v);
        format!("    mstore({}, {})\n", addr, merged)
    }
}

// Calldata -> packed memory struct: one wire word per field in declaration order, scattered into a layout that is widest-first and tightly packed.
fn abi_st_cd_helper_src(fname: &str, fields: &[AbiStructField], packed: usize) -> String {
    let mut body = format!("function {}(off) -> ptr {{\n", fname);
    body.push_str(&format!(
        "    if lt(calldatasize(), add(off, {})) {{ revert(0, 0) }}\n",
        fields.len() * 32
    ));
    body.push_str(&format!("    ptr := allocate_memory({})\n", packed));
    for (i, f) in fields.iter().enumerate() {
        body.push_str(&abi_st_field_store(&format!("calldataload(add(off, {}))", i * 32), f));
    }
    body.push_str("}\n");
    body
}

// Same move, reading from an ABI blob already in memory, which is how constructor args arrive: appended to the creation code and codecopy'd in.
fn abi_st_mem_helper_src(fname: &str, fields: &[AbiStructField], packed: usize) -> String {
    let mut body = format!("function {}(base, off, limit) -> ptr {{\n", fname);
    body.push_str(&format!(
        "    if lt(limit, add(off, {})) {{ revert(0, 0) }}\n",
        fields.len() * 32
    ));
    body.push_str(&format!("    ptr := allocate_memory({})\n", packed));
    for (i, f) in fields.iter().enumerate() {
        body.push_str(&abi_st_field_store(
            &format!("mload(add(base, add(off, {})))", i * 32),
            f,
        ));
    }
    body.push_str("}\n");
    body
}

// Packed memory struct -> ABI, written at dst. No size is returned because a static struct's width is a compile-time constant the caller already has.
fn abi_st_put_helper_src(fname: &str, fields: &[AbiStructField]) -> String {
    let mut body = format!("function {}(dst, ptr) {{\n", fname);
    for (i, f) in fields.iter().enumerate() {
        let read = read_packed(&format!("mload(add(ptr, {}))", f.mem_offset), 0, f.width);
        let v = if f.is_addr {
            format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", read)
        } else {
            read
        };
        body.push_str(&format!("    mstore(add(dst, {}), {})\n", i * 32, v));
    }
    body.push_str("}\n");
    body
}

// An ABI string or bytes in memory -> gum's own shape, where the length lives in the top 8 bytes of the header word rather than in a word of its own.
fn gum_abi_str_mem_helper_src() -> String {
    "function gum_abi_str_mem(base, off, limit) -> ptr {\n\
     \x20   if gt(add(off, 32), limit) { revert(0, 0) }\n\
     \x20   let len := mload(add(base, off))\n\
     \x20   if gt(add(add(off, 32), len), limit) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(add(32, len))\n\
     \x20   mstore(ptr, shl(192, len))\n\
     \x20   gum_memory_copy(add(add(base, off), 32), add(ptr, 32), len)\n\
     }\n"
    .to_string()
}

// An array of static structs: [count][elem0 words][elem1 words]... with the elements inline, since a static tuple carries no offset of its own.
fn abi_starr_cd_helper_src(fname: &str, st_cd: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(off) -> ptr {{\n\
         \x20   if lt(calldatasize(), add(off, 32)) {{ revert(0, 0) }}\n\
         \x20   let n := calldataload(off)\n\
         \x20   if gt(n, div(sub(calldatasize(), add(off, 32)), {wire})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory(add(32, mul(n, {packed})))\n\
         \x20   mstore(ptr, mul(n, {packed}))\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       let e := {st_cd}(add(add(off, 32), mul(i, {wire})))\n\
         \x20       gum_memory_copy(e, add(add(ptr, 32), mul(i, {packed})), {packed})\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        st_cd = st_cd,
        wire = wire,
        packed = packed
    )
}

// Same, from an ABI blob already in memory, for constructor args.
fn abi_starr_mem_helper_src(fname: &str, st_mem: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(base, off, limit) -> ptr {{\n\
         \x20   if lt(limit, add(off, 32)) {{ revert(0, 0) }}\n\
         \x20   let n := mload(add(base, off))\n\
         \x20   if gt(n, div(sub(limit, add(off, 32)), {wire})) {{ revert(0, 0) }}\n\
         \x20   ptr := allocate_memory(add(32, mul(n, {packed})))\n\
         \x20   mstore(ptr, mul(n, {packed}))\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       let e := {st_mem}(base, add(add(off, 32), mul(i, {wire})), limit)\n\
         \x20       gum_memory_copy(e, add(add(ptr, 32), mul(i, {packed})), {packed})\n\
         \x20   }}\n\
         }}\n",
        fname = fname,
        st_mem = st_mem,
        wire = wire,
        packed = packed
    )
}

// Memory -> ABI. Returns bytes written so a caller laying out several tails can advance, matching gum_abi_arr_put.
fn abi_starr_put_helper_src(fname: &str, st_put: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(dst, ptr) -> written {{\n\
         \x20   let n := div(mload(ptr), {packed})\n\
         \x20   mstore(dst, n)\n\
         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
         \x20       {st_put}(add(add(dst, 32), mul(i, {wire})), add(add(ptr, 32), mul(i, {packed})))\n\
         \x20   }}\n\
         \x20   written := add(32, mul(n, {wire}))\n\
         }}\n",
        fname = fname,
        st_put = st_put,
        wire = wire,
        packed = packed
    )
}

// The ABI size of a memory array of static structs: a count word plus each element's full wire width.
fn abi_starr_size_helper_src(fname: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(ptr) -> sz {{\n\
         \x20   sz := add(32, mul(div(mload(ptr), {packed}), {wire}))\n\
         }}\n",
        fname = fname,
        wire = wire,
        packed = packed
    )
}

// Re-revert with the callee's own revert data, verbatim.
//
// A sub-call that fails has a reason, "ERC20: transfer amount exceeds
fn gum_bubble_revert_helper_src() -> String {
    "function gum_bubble_revert() {\n\
     \x20   returndatacopy(0, 0, returndatasize())\n\
     \x20   revert(0, returndatasize())\n\
     }\n"
    .to_string()
}

// Account.create(code, value) / Account.create2(code, value, salt), deploy
// a contract from creation bytecode held in memory.
//
fn gum_create_helper_src() -> String {
    "function gum_create(codeptr, value) -> addr {\n\
     \x20   addr := create(value, add(codeptr, 32), gum_str_len(codeptr))\n\
     \x20   if iszero(addr) { gum_bubble_revert() }\n\
     }\n"
    .to_string()
}

// CREATE2 (EIP-1014): the address is keccak256(0xff ++ this ++ salt ++
fn gum_create2_helper_src() -> String {
    "function gum_create2(codeptr, value, salt) -> addr {\n\
     \x20   addr := create2(value, add(codeptr, 32), gum_str_len(codeptr), salt)\n\
     \x20   if iszero(addr) { gum_bubble_revert() }\n\
     }\n"
    .to_string()
}

// The address create2 would produce, without deploying. Lets a factory hand
fn gum_create2_address_helper_src() -> String {
    "function gum_create2_address(codeptr, salt) -> addr {\n\
     \x20   let h := keccak256(add(codeptr, 32), gum_str_len(codeptr))\n\
     \x20   let p := mload(0x40)\n\
     \x20   mstore8(p, 0xff)\n\
     \x20   mstore(add(p, 1), shl(96, address()))\n\
     \x20   mstore(add(p, 21), salt)\n\
     \x20   mstore(add(p, 53), h)\n\
     \x20   addr := and(keccak256(p, 85), 0xffffffffffffffffffffffffffffffffffffffff)\n\
     }\n"
    .to_string()
}

// to.transfer(amount), send ETH, revert if it fails.
//
// The checked companion to pay: same CALL, but an unsuccessful send aborts
fn gum_transfer_helper_src() -> String {
    "function gum_transfer(to, amount) {\n\
     \x20   if iszero(call(gas(), to, amount, 0, 0, 0, 0)) {\n\
     \x20       returndatacopy(0, 0, returndatasize())\n\
     \x20       revert(0, returndatasize())\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// secp256r1 (P-256) signature verification via the precompile at 0x100
// EIP-7951 on L1, interface-identical to RIP-7212 already live on L2s, so the
// same bytecode works on both. Enables Apple Secure Enclave / Android Keystore
fn gum_p256_verify_helper_src() -> String {
    "function gum_p256_verify(h, r, s, qx, qy) -> ok {\n\
     \x20   let p := mload(0x40)\n\
     \x20   mstore(p, h)\n\
     \x20   mstore(add(p, 32), r)\n\
     \x20   mstore(add(p, 64), s)\n\
     \x20   mstore(add(p, 96), qx)\n\
     \x20   mstore(add(p, 128), qy)\n\
     \x20   ok := 0\n\
     \x20   if staticcall(gas(), 0x100, p, 160, add(p, 160), 32) {\n\
     \x20       if eq(returndatasize(), 32) { ok := eq(mload(add(p, 160)), 1) }\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// EIP-7702 delegation target, or 0 if the account isn't delegated.
//
// A 7702-delegated EOA's code is exactly 23 bytes: the 0xef0100 marker followed
fn gum_delegate_of_helper_src() -> String {
    "function gum_delegate_of(a) -> target {\n\
     \x20   target := 0\n\
     \x20   if eq(extcodesize(a), 23) {\n\
     \x20       let p := mload(0x40)\n\
     \x20       mstore(p, 0)\n\
     \x20       extcodecopy(a, p, 0, 23)\n\
     \x20       if eq(shr(232, mload(p)), 0xef0100) {\n\
     \x20           target := and(shr(72, mload(p)), 0xffffffffffffffffffffffffffffffffffffffff)\n\
     \x20       }\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// Concatenation: allocates a fresh String holding a's bytes then b's.
fn gum_str_concat_helper_src() -> String {
    "function gum_str_concat(a, b) -> ptr {\n\
     \x20   let la := gum_str_len(a)\n\
     \x20   let lb := gum_str_len(b)\n\
     \x20   let total := add(la, lb)\n\
     \x20   ptr := allocate_memory(add(32, total))\n\
     \x20   mstore(ptr, shl(192, total))\n\
     \x20   gum_memory_copy(add(a, 32), add(ptr, 32), la)\n\
     \x20   gum_memory_copy(add(b, 32), add(add(ptr, 32), la), lb)\n\
     }\n"
    .to_string()
}

// Byte at index, bounds-checked like any other array access (Panic 0x32).
fn gum_str_at_helper_src(rich: bool) -> String {
    format!(
        "function gum_str_at(p, i) -> b {{\n\
         \x20   if iszero(lt(i, gum_str_len(p))) {{ {} }}\n\
         \x20   b := shr(248, mload(add(add(p, 32), i)))\n\
         }}\n",
        panic_revert(rich, "0x32")
    )
}

// Half-open slice [s, e). Reverts (Panic 0x32) if e is past the end or the
fn gum_str_slice_helper_src(rich: bool) -> String {
    format!(
        "function gum_str_slice(p, s, e) -> ptr {{\n\
         \x20   if or(gt(e, gum_str_len(p)), gt(s, e)) {{ {} }}\n\
         \x20   let l := sub(e, s)\n\
         \x20   ptr := allocate_memory(add(32, l))\n\
         \x20   mstore(ptr, shl(192, l))\n\
         \x20   gum_memory_copy(add(add(p, 32), s), add(ptr, 32), l)\n\
         }}\n",
        panic_revert(rich, "0x32")
    )
}

fn bytes_copy_helper_src() -> String {
    "function bytes_copy(dst, src, len) {\n\
     \x20   let i := 0\n\
     \x20   for {} lt(i, len) { i := add(i, 32) } {\n\
     \x20       mstore(add(dst, i), mload(add(src, i)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

// Reads a packed field: shift the containing word right so the field's
// bytes land at the bottom, then mask off anything to their left that
// belongs to a neighboring field. When a field fills its whole 32-byte
fn read_slot_packed(container_expr: &str, offset: usize, size: usize) -> String {
    if size >= 32 && offset == 0 {
        return container_expr.to_string();
    }
    let shift = offset * 8;
    if shift == 0 {
        format!("and({}, {})", container_expr, mask_hex(size))
    } else {
        format!(
            "and(shr({}, {}), {})",
            shift,
            container_expr,
            mask_hex(size)
        )
    }
}

fn write_slot_packed(container_expr: &str, offset: usize, size: usize, val_expr: &str) -> String {
    if size >= 32 && offset == 0 {
        return val_expr.to_string();
    }
    let shift = offset * 8;
    let mask = mask_hex(size);
    format!(
        "or(and({}, not(shl({}, {}))), shl({}, and({}, {})))",
        container_expr, shift, mask, shift, val_expr, mask
    )
}

fn read_packed(container_expr: &str, offset_in_container: usize, size: usize) -> String {
    if size >= 32 && offset_in_container == 0 {
        return container_expr.to_string();
    }
    let shift = (32 - offset_in_container - size) * 8;
    format!(
        "and(shr({}, {}), {})",
        shift,
        container_expr,
        mask_hex(size)
    )
}

// Read-modify-write for a packed field: preserve every byte in the
// container except this field's own range, then OR in the new value shifted
// into position. Degenerates to a plain overwrite when the field fills its
fn write_packed(
    container_expr: &str,
    offset_in_container: usize,
    size: usize,
    val_expr: &str,
) -> String {
    if size >= 32 && offset_in_container == 0 {
        return val_expr.to_string();
    }
    let shift = (32 - offset_in_container - size) * 8;
    let mask = mask_hex(size);
    format!(
        "or(and({}, not(shl({}, {}))), shl({}, and({}, {})))",
        container_expr, shift, mask, shift, val_expr, mask
    )
}

// Bit width and signedness of a scalar integer type, or None for anything
// else (bool, classes, arrays, ...). Used to pick the right overflow bound
// and signed/unsigned opcode variant for a binary operation.
fn literal_u128(expr: &Expr) -> Option<u128> {
    if let Expr::Number(n) = expr {
        n.parse::<u128>().ok()
    } else {
        None
    }
}

// Solidity/ABI type name for a gum type, used to build canonical event
// signatures for topic0 hashing. Account is the EVM address type; the uN/iN
// families map straight to uintN/intN. Anything unrecognized falls back to

fn numeric_meta(name: &str) -> Option<(usize, bool)> {
    match name {
        "u8" => Some((8, false)),
        "i8" => Some((8, true)),
        "u16" => Some((16, false)),
        "i16" => Some((16, true)),
        "u32" => Some((32, false)),
        "i32" => Some((32, true)),
        "u64" => Some((64, false)),
        "i64" => Some((64, true)),
        "u128" => Some((128, false)),
        "i128" => Some((128, true)),
        "u256" => Some((256, false)),
        "i256" => Some((256, true)),
        // A fixed-point value is a signed full-width word carrying a WAD scale, so it is signed for every purpose the scale does not touch: + and - are ordinary signed ops, and comparison and % follow the signed opcodes.
        "f32" | "f64" => Some((256, true)),
        _ => None,
    }
}

// Whether a type is WAD-scaled fixed point, whose  and / must correct the scale.
fn is_fixed_point(t: &Type) -> bool {
    matches!(t, Type::Primitive(p) if p == "f32" || p == "f64")
}

// Truncates a value down to a narrower integer width. Unsigned types just
// mask off the high bits; signed types must use SIGNEXTEND instead of a
// plain AND-mask, or a negative value (all-1s in its upper bits under two's
fn mask_to_width(val_expr: &str, bits: usize, signed: bool) -> String {
    if bits >= 256 {
        return val_expr.to_string();
    }
    if signed {
        format!("signextend({}, {})", bits / 8 - 1, val_expr)
    } else {
        format!("and({}, {})", val_expr, mask_hex(bits / 8))
    }
}

// Like mask_to_width, but takes the gum Type directly and is a no-op for
// The byte count of a fixed-bytes type: byte_width("b4") == Some(4). Valid
// b1..b32; anything else (bool, u256, a class name) is None.
pub fn byte_width(name: &str) -> Option<usize> {
    let n = name.strip_prefix('b')?.parse::<usize>().ok()?;
    (1..=32).contains(&n).then_some(n)
}

fn mask_for_type(val_expr: &str, type_def: &Type) -> String {
    if let Type::Primitive(name) = type_def {
        if let Some((bits, signed)) = numeric_meta(name) {
            return mask_to_width(val_expr, bits, signed);
        }
        // A fixed-bytes value rides the wire left-aligned (its bytes in the high
        // end of the word, like Solidity). For a sub-word width we shift it down
        // to a clean right-aligned value so it compares against a plain literal
        // like 0x01ffc9a7; a full b32 already fills the word, so it is left as
        // is. Sub-word values are re-aligned on the way out (see Return).
        if let Some(w) = byte_width(name) {
            return if w == 32 {
                val_expr.to_string()
            } else {
                format!("shr({}, {})", (32 - w) * 8, val_expr)
            };
        }
    }
    val_expr.to_string()
}

// The revert body used inside the checked helpers. With rich reverts off it's
// a bare revert(0, 0) (smallest possible, zero return data). With it on it
// encodes Solidity's Panic(uint256): selector 0x4e487b71 followed by a code
fn panic_revert(rich: bool, code: &str) -> String {
    if rich {
        format!(
            "mstore(0, shl(224, 0x4e487b71)) mstore(4, {}) revert(0, 0x24)",
            code
        )
    } else {
        "revert(0, 0)".to_string()
    }
}

const PANIC_OVERFLOW: &str = "0x11";
const PANIC_DIV_ZERO: &str = "0x12";
const PANIC_OOB: &str = "0x32";
const PANIC_EMPTY_POP: &str = "0x31";

// (elements per slot, slots per group) for an element of esz bytes, see
fn pack_params(esz: usize) -> (usize, usize) {
    let esz = esz.max(1);
    if esz >= 32 {
        (1, (esz + 31) / 32)
    } else {
        (32 / esz, 1)
    }
}

// The data region of a dynamic storage array: Solidity stores the length at
fn arr_data_base_helper_src() -> String {
    "function arr_data_base(len_slot) -> b {\n\
     \x20   mstore(0, len_slot)\n\
     \x20   b := keccak256(0, 32)\n\
     }\n"
    .to_string()
}

// Element addressing for a storage array, packed exactly as Solidity packs it.
//
// Every element width is handled by one scheme, parameterized by:
fn pk_read_helper_src(tr: bool) -> String {
    format!(
        "function pk_read{k}(base, i, per, es, esz) -> v {{\n\
     \x20   let s := add(base, mul(div(i, per), es))\n\
     \x20   v := and(shr(mul(mul(mod(i, per), esz), 8), {ld}(s)), sub(shl(mul(esz, 8), 1), 1))\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr)
    )
}

// Read-modify-write: the neighbours sharing this slot are live elements, so
fn pk_write_helper_src(tr: bool) -> String {
    format!(
        "function pk_write{k}(base, i, per, es, esz, v) {{\n\
     \x20   let s := add(base, mul(div(i, per), es))\n\
     \x20   let sh := mul(mul(mod(i, per), esz), 8)\n\
     \x20   let m := sub(shl(mul(esz, 8), 1), 1)\n\
     \x20   {st}(s, or(and({ld}(s), not(shl(sh, m))), shl(sh, and(v, m))))\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr),
        st = st_op(tr)
    )
}

// Copies a whole dynamic storage array out into a fresh memory array.
fn sarr_to_mem_helper_src(name: &str, esz: usize, per: usize, es: usize, tr: bool) -> String {
    let write = if esz >= 32 {
        "v".to_string()
    } else {
        write_packed("mload(a)", 0, esz, "v")
    };
    format!(
        "function {name}(len_slot) -> ptr {{\n\
     \x20   let n := {ld}(len_slot)\n\
     \x20   ptr := allocate_memory(add(32, mul(n, {esz})))\n\
     \x20   mstore(ptr, mul(n, {esz}))\n\
     \x20   let base := arr_data_base(len_slot)\n\
     \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
     \x20       let a := add(add(ptr, 32), mul(i, {esz}))\n\
     \x20       let v := pk_read{k}(base, i, {per}, {es}, {esz})\n\
     \x20       mstore(a, {write})\n\
     \x20   }}\n\
     }}\n",
        name = name,
        ld = ld_op(tr),
        k = kind_suffix(tr),
        esz = esz,
        per = per,
        es = es,
        write = write
    )
}

// How many whole slots one struct element of an array occupies.
//
// Unlike a scalar element, a struct element is never packed with its
fn struct_elem_slots(size: usize) -> usize {
    size.div_ceil(32).max(1)
}

// The base slot of struct element i, bounds-checked.
//
// The check runs before the multiply: mul(i, es) on a huge index wraps, and
fn sarr_base_helper_src(rich: bool) -> String {
    format!(
        "function sarr_base(base, i, len, es) -> b {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   b := add(base, mul(i, es))\n\
     }}\n",
        p = panic_revert(rich, PANIC_OOB)
    )
}

// arr.pop() for a struct array: zero every slot of the removed element (as
// Solidity does, for the refund) then shrink. Struct elements own their slots
// outright, so unlike the packed scalar pop there are no neighbours to preserve
fn dsarr_pop_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function dsarr_pop{k}(len_slot, es) {{\n\
     \x20   let n := {ld}(len_slot)\n\
     \x20   if iszero(n) {{ {p} }}\n\
     \x20   n := sub(n, 1)\n\
     \x20   let b := add(arr_data_base(len_slot), mul(n, es))\n\
     \x20   for {{ let j := 0 }} lt(j, es) {{ j := add(j, 1) }} {{ {st}(add(b, j), 0) }}\n\
     \x20   {st}(len_slot, n)\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr),
        st = st_op(tr),
        p = panic_revert(rich, PANIC_EMPTY_POP)
    )
}

// Bounds-checked read/write. Reverts (Panic 0x32 under --rich-reverts, blank
fn pk_get_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function pk_get{k}(base, i, len, per, es, esz) -> v {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   v := pk_read{k}(base, i, per, es, esz)\n\
     }}\n",
        k = kind_suffix(tr),
        p = panic_revert(rich, PANIC_OOB)
    )
}

fn pk_set_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function pk_set{k}(base, i, len, per, es, esz, v) {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   pk_write{k}(base, i, per, es, esz, v)\n\
     }}\n",
        k = kind_suffix(tr),
        p = panic_revert(rich, PANIC_OOB)
    )
}

fn dpk_push_helper_src(tr: bool) -> String {
    format!(
        "function dpk_push{k}(len_slot, per, es, esz, v) {{\n\
     \x20   let n := {ld}(len_slot)\n\
     \x20   pk_write{k}(arr_data_base(len_slot), n, per, es, esz, v)\n\
     \x20   {st}(len_slot, add(n, 1))\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr),
        st = st_op(tr)
    )
}

// pop zeroes the removed element (Solidity does, for the gas refund) without
fn dpk_pop_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function dpk_pop{k}(len_slot, per, es, esz) {{\n\
     \x20   let n := {ld}(len_slot)\n\
     \x20   if iszero(n) {{ {p} }}\n\
     \x20   n := sub(n, 1)\n\
     \x20   pk_write{k}(arr_data_base(len_slot), n, per, es, esz, 0)\n\
     \x20   if gt(es, 1) {{\n\
     \x20       let s := add(arr_data_base(len_slot), mul(n, es))\n\
     \x20       for {{ let j := 1 }} lt(j, es) {{ j := add(j, 1) }} {{ {st}(add(s, j), 0) }}\n\
     \x20   }}\n\
     \x20   {st}(len_slot, n)\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr),
        st = st_op(tr),
        p = panic_revert(rich, PANIC_EMPTY_POP)
    )
}

// delete arr on a dynamic storage array: zero every occupied slot, then the
fn dpk_clear_helper_src(tr: bool) -> String {
    format!(
        "function dpk_clear{k}(len_slot, per, es) {{\n\
     \x20   let n := {ld}(len_slot)\n\
     \x20   let base := arr_data_base(len_slot)\n\
     \x20   let slots := mul(div(add(n, sub(per, 1)), per), es)\n\
     \x20   for {{ let i := 0 }} lt(i, slots) {{ i := add(i, 1) }} {{ {st}(add(base, i), 0) }}\n\
     \x20   {st}(len_slot, 0)\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr),
        st = st_op(tr)
    )
}

fn checked_add_helper_src(rich: bool) -> String {
    format!(
        "function checked_add(a, b, max) -> r {{\n\
     \x20   r := add(a, b)\n\
     \x20   if or(lt(r, a), gt(r, max)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

fn checked_sub_helper_src(rich: bool) -> String {
    format!(
        "function checked_sub(a, b) -> r {{\n\
     \x20   if lt(a, b) {{ {} }}\n\
     \x20   r := sub(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

fn checked_mul_helper_src(rich: bool) -> String {
    format!(
        "function checked_mul(a, b, max) -> r {{\n\
     \x20   r := mul(a, b)\n\
     \x20   if and(iszero(iszero(a)), iszero(eq(div(r, a), b))) {{ {} }}\n\
     \x20   if gt(r, max) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW),
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

// Signed add. checked_add guards with lt and a max, both unsigned, so on a signed type it read a negative operand as a huge number and reverted: 5 + (-3) never got a chance to be 2.
fn checked_sadd_helper_src(rich: bool) -> String {
    format!(
        "function checked_sadd(a, b, minv, maxv) -> r {{\n\
     \x20   r := add(a, b)\n\
     \x20   if slt(and(xor(a, r), xor(b, r)), 0) {{ {} }}\n\
     \x20   if or(slt(r, minv), sgt(r, maxv)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW),
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

// Signed subtract. checked_sub reverted whenever lt(a, b) unsigned, which for a signed type is exactly the ordinary case of going negative: 1 - 2 reverted instead of giving -1.
fn checked_ssub_helper_src(rich: bool) -> String {
    format!(
        "function checked_ssub(a, b) -> r {{\n\
     \x20   r := sub(a, b)\n\
     \x20   if slt(and(xor(a, b), xor(a, r)), 0) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

// Signed subtract with a narrowing range check, for i8..i128 whose result must land back inside the declared width.
fn checked_ssub_n_helper_src(rich: bool) -> String {
    format!(
        "function checked_ssub_n(a, b, minv, maxv) -> r {{\n\
     \x20   r := checked_ssub(a, b)\n\
     \x20   if or(slt(r, minv), sgt(r, maxv)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

// Signed multiply. sdiv(r, a) == b is the same inverse test checked_mul uses, in its signed form.
fn checked_smul_helper_src(rich: bool) -> String {
    format!(
        "function checked_smul(a, b, minv, maxv) -> r {{\n\
     \x20   r := mul(a, b)\n\
     \x20   if iszero(iszero(a)) {{\n\
     \x20       if and(eq(a, not(0)), eq(b, 0x8000000000000000000000000000000000000000000000000000000000000000)) {{ {} }}\n\
     \x20       if iszero(eq(sdiv(r, a), b)) {{ {} }}\n\
     \x20   }}\n\
     \x20   if or(slt(r, minv), sgt(r, maxv)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW),
        panic_revert(rich, PANIC_OVERFLOW),
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

// One WAD, the scale of every f32 and f64 value: 1.0 is 10^18.
const WAD: &str = "1000000000000000000";

// Full-precision unsigned floor((xy)/d), the Remco Bloemen / OpenZeppelin Math.mulDiv.
fn gum_muldiv_helper_src(rich: bool) -> String {
    format!(
        "function gum_muldiv(x, y, d) -> r {{\n\
     \x20   let mm := mulmod(x, y, not(0))\n\
     \x20   let p0 := mul(x, y)\n\
     \x20   let p1 := sub(sub(mm, p0), lt(mm, p0))\n\
     \x20   if iszero(p1) {{\n\
     \x20       if iszero(d) {{ {divzero} }}\n\
     \x20       r := div(p0, d)\n\
     \x20       leave\n\
     \x20   }}\n\
     \x20   if iszero(gt(d, p1)) {{ {overflow} }}\n\
     \x20   let rem := mulmod(x, y, d)\n\
     \x20   p1 := sub(p1, gt(rem, p0))\n\
     \x20   p0 := sub(p0, rem)\n\
     \x20   let twos := and(sub(0, d), d)\n\
     \x20   d := div(d, twos)\n\
     \x20   p0 := div(p0, twos)\n\
     \x20   twos := add(div(sub(0, twos), twos), 1)\n\
     \x20   p0 := or(p0, mul(p1, twos))\n\
     \x20   let inv := xor(mul(3, d), 2)\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   inv := mul(inv, sub(2, mul(d, inv)))\n\
     \x20   r := mul(p0, inv)\n\
     }}\n",
        divzero = panic_revert(rich, PANIC_DIV_ZERO),
        overflow = panic_revert(rich, PANIC_OVERFLOW)
    )
}

// A signed WAD op reduces to an unsigned mulDiv on the magnitudes, with the sign reapplied at the end.
fn gum_wad_signed(fname: &str, num: &str, den: &str, rich: bool) -> String {
    format!(
        "function {fname}(a, b, minv, maxv) -> r {{\n\
     \x20   let neg := xor(slt(a, 0), slt(b, 0))\n\
     \x20   let x := a\n\
     \x20   if slt(a, 0) {{ x := sub(0, a) }}\n\
     \x20   let y := b\n\
     \x20   if slt(b, 0) {{ y := sub(0, b) }}\n\
     \x20   let mag := gum_muldiv({num}, {den})\n\
     \x20   switch neg\n\
     \x20   case 0 {{ if gt(mag, maxv) {{ {overflow} }} r := mag }}\n\
     \x20   default {{ if gt(mag, minv) {{ {overflow} }} r := sub(0, mag) }}\n\
     }}\n",
        fname = fname,
        num = num,
        den = den,
        overflow = panic_revert(rich, PANIC_OVERFLOW)
    )
}

// Multiplying two WAD values gives a WAD-squared product, so it comes back down by one WAD: floor(|a||b| / 10^18) on the magnitudes.
fn gum_wad_mul_helper_src(rich: bool) -> String {
    let den = WAD.to_string();
    gum_wad_signed("gum_wad_mul", "x, y", &den, rich)
}

// Dividing two WAD values cancels the scale, so the numerator goes up by one WAD first: floor(|a|10^18 / |b|).
fn gum_wad_div_helper_src(rich: bool) -> String {
    let num = format!("x, {}", WAD);
    // b == 0 surfaces inside gum_muldiv as a divide-by-zero once the magnitude denominator is zero.
    gum_wad_signed("gum_wad_div", &num, "y", rich)
}

fn checked_div_helper_src(rich: bool) -> String {
    format!(
        "function checked_div(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := div(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

fn checked_sdiv_helper_src(rich: bool) -> String {
    format!(
        "function checked_sdiv(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := sdiv(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

fn checked_mod_helper_src(rich: bool) -> String {
    format!(
        "function checked_mod(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := mod(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

fn checked_smod_helper_src(rich: bool) -> String {
    format!(
        "function checked_smod(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := smod(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

// Runtime shape of every enum value in this compiler: a pointer to
fn make_enum_helper_src() -> String {
    "function make_enum(tag, payload) -> ptr {\n\
     \x20   ptr := allocate_memory(64)\n\
     \x20   mstore(ptr, tag)\n\
     \x20   mstore(add(ptr, 32), payload)\n\
     }\n"
    .to_string()
}

fn keccak256_hex(data: &str) -> String {
    let mut keccak = Keccak::v256();
    let mut output = [0u8; 32];
    keccak.update(data.as_bytes());
    keccak.finalize(&mut output);
    let mut s = String::from("0x");
    for b in output {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// Identifies which class a method body's implicit self belongs to, and
pub struct SelfCtx {
    pub class_name: String,
    pub is_global: bool,
}

// Threaded through every translate_ call so a single Translator can compile
pub struct Ctx<'c> {
    pub self_ctx: Option<&'c SelfCtx>,
    // Entry points (the _impl functions the dispatcher calls) end with the
    pub is_entry: bool,
    pub try_ok_var: Option<String>,
    // Declared types of parameters and locals for the function/method
    locals: RefCell<HashMap<String, Type>>,
    // The enclosing function/method's declared return type, if any, used
    pub return_type: Option<Type>,
    // The designated reentrancy lock slot if this context requires guarding.
    pub lock_slot: Option<String>,
    // True while translating fn new. An immutable field is a plain Yul local
    pub in_constructor: bool,
}

impl<'c> Ctx<'c> {
    pub fn entry(lock_slot: Option<String>) -> Self {
        Ctx {
            self_ctx: None,
            is_entry: true,
            try_ok_var: None,
            locals: RefCell::new(HashMap::new()),
            return_type: None,
            lock_slot,
            in_constructor: false,
        }
    }
    pub fn helper(self_ctx: Option<&'c SelfCtx>) -> Self {
        Ctx {
            self_ctx,
            is_entry: false,
            try_ok_var: None,
            locals: RefCell::new(HashMap::new()),
            return_type: None,
            lock_slot: None,
            in_constructor: false,
        }
    }

    // Marks this context as the body of fn new.
    pub fn in_constructor(mut self, yes: bool) -> Self {
        self.in_constructor = yes;
        self
    }

    // Binds the class an entry point belongs to. Entry points are declared
    // inside a contract, so their bodies can say self.field, without a
    // self context that resolves as a memory field on an unbound self
    pub fn with_self(mut self, self_ctx: Option<&'c SelfCtx>) -> Self {
        self.self_ctx = self_ctx;
        self
    }
    pub fn with_return_type(mut self, ty: Option<Type>) -> Self {
        self.return_type = ty;
        self
    }
    pub fn declare(&self, name: &str, type_def: &Type) {
        self.locals
            .borrow_mut()
            .insert(name.to_string(), type_def.clone());
    }
}

fn is_numeric_primitive(name: &str) -> bool {
    matches!(
        name,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "u256"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "i256"
            | "f32"
            | "f64"
    )
}

pub struct Translator<'a> {
    pub layout_engine: &'a LayoutEngine<'a>,
    pub abi_gen: &'a AbiGenerator<'a>,
    pub top_level_fns: &'a HashSet<String>,
    // Helper Yul functions synthesized on demand while translating (external
    // call thunks, constructor thunks), keyed by function name so repeated
    // call sites share one definition. Emitted once, after translation, by
    // whoever drains them.
    //
    // BTreeMap, not HashMap: the drain order is the emission order, and Rust
    // randomizes HashMap iteration per process, so a HashMap here made the same
    // source compile to different (equivalent) bytecode on every run. Ordering
    // by name costs nothing and is what makes a build reproducible.
    helper_thunks: RefCell<BTreeMap<String, String>>,
    // Each f-string/plain-string literal gets its own uniquely-named
    // assembly thunk (its content is fixed at compile time), so this just
    // hands out unique suffixes.
    literal_counter: RefCell<usize>,
    // When true, checked-arithmetic reverts carry Solidity's Panic(uint256)
    // reason data instead of reverting blank. Off by default (smaller code);
    // toggled by the CLI's --rich-reverts.
    rich_reverts: bool,
    // Event schemas, recorded as their log() sites are translated.
    //
    // An event has no declaration to read a schema from: enum TokenLogs:
    // Transfer names a variant and nothing else, so the field types, names
    // and indexed-ness exist only at the call site. This registry is therefore
    // the only place the ABI can come from, and populating it from the same
    // walk that computes topic0 is what keeps the two from drifting.
    //
    // BTreeMap, not HashMap: ABI JSON order must be deterministic across runs.
    events: RefCell<BTreeMap<String, EventSchema>>,
    // Errors raised during translation. translate_ return plain Strings of
    // Yul with nowhere to put a failure, so they are collected here and
    // drained by codegen once the walk is done.
    errors: RefCell<Vec<String>>,
}

// One event's ABI shape, as recorded at a log() call site.
#[derive(Clone, PartialEq)]
pub struct EventSchema {
    pub inputs: Vec<AbiInput>,
    // The canonical Name(type,…) string topic0 was hashed from. Two log()
    // sites that disagree on it are two different events wearing one name.
    pub signature: String,
}

fn gum_exception_helpers_src() -> String {
    "function gum_set_exception() {
        tstore(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff, 1)
    }
    function gum_check_exception() -> has_err {
        has_err := tload(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff)
        if has_err { tstore(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff, 0) }
    }\n".to_string()
}

impl<'a> Translator<'a> {
    pub fn new(
        layout_engine: &'a LayoutEngine<'a>,
        abi_gen: &'a AbiGenerator<'a>,
        top_level_fns: &'a HashSet<String>,
        rich_reverts: bool,
    ) -> Self {
        Self {
            layout_engine,
            abi_gen,
            top_level_fns,
            helper_thunks: RefCell::new(BTreeMap::new()),
            literal_counter: RefCell::new(0),
            rich_reverts,
            events: RefCell::new(BTreeMap::new()),
            errors: RefCell::new(Vec::new()),
        }
    }

    // Errors raised while translating. Non-empty means the emitted Yul must
    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.borrow_mut())
    }

    // Event schemas gathered from every log() translated so far, name-sorted.
    pub fn recorded_events(&self) -> Vec<(String, EventSchema)> {
        self.events
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // Files one log() site's schema, or reports the name as ambiguous.
    //
    // Two sites may legitimately log the same event repeatedly, but they must
    fn record_event(&self, name: &str, schema: EventSchema) -> Result<(), String> {
        let mut events = self.events.borrow_mut();
        match events.get(name) {
            Some(prev) if *prev == schema => Ok(()),
            Some(prev) => Err(format!(
                "event '{}' is logged with two different shapes: {} and {}. \
                 An event name maps to exactly one ABI entry, so every log() of \
                 it must pass the same field types and mark the same fields indexed.",
                name, prev.signature, schema.signature
            )),
            None => {
                events.insert(name.to_string(), schema);
                Ok(())
            }
        }
    }

    fn next_literal_id(&self) -> usize {
        let mut c = self.literal_counter.borrow_mut();
        *c += 1;
        *c
    }

    // Drained in name order, which BTreeMap gives for free, so the emitted helpers land in the same order on every run.
    pub fn drain_helper_thunks(&self) -> Vec<String> {
        std::mem::take(&mut *self.helper_thunks.borrow_mut())
            .into_values()
            .collect()
    }

    // The array ABI decoders, for the dispatcher and constructor arg decoding
    pub fn ensure_abi_arr_cd(&self) {
        self.ensure_helper("gum_abi_arr_cd", gum_abi_arr_cd_helper_src);
    }

    pub fn ensure_abi_arr_mem(&self) {
        self.ensure_helper("gum_abi_arr_mem", gum_abi_arr_mem_helper_src);
    }

    pub fn ensure_abi_arr_put(&self) {
        self.ensure_helper("gum_abi_arr_put", gum_abi_arr_put_helper_src);
        self.ensure_helper("gum_abi_arr_size", gum_abi_arr_size_helper_src);
    }

    // A struct's fields in declaration order, which is ABI order. Memory order is widest-first, so this deliberately does not follow the memory layout.
    pub fn abi_struct_layout(&self, name: &str) -> Option<Vec<AbiStructField>> {
        let class = self.type_checker().loaded_classes.get(name)?;
        let mut out = Vec::new();
        for f in &class.fields {
            if !is_abi_scalar(&f.type_def) {
                return None;
            }
            let mf = self.layout_engine.memory_field(name, &f.name)?;
            out.push(AbiStructField {
                mem_offset: mf.offset,
                width: mf.size,
                is_addr: crate::codegen::is_address_type(&f.type_def),
            });
        }
        if out.is_empty() {
            return None;
        }
        Some(out)
    }

    // The wire width of a static struct: one word per field, however tightly memory packs them.
    pub fn abi_struct_wire_size(&self, name: &str) -> Option<usize> {
        self.abi_struct_layout(name).map(|f| f.len() * 32)
    }

    fn abi_struct_packed(&self, name: &str) -> usize {
        self.layout_engine.size_of(&Type::Primitive(name.to_string()))
    }

    // The three ensure_ methods below register one codec per struct type and hand back its Yul name, so a contract with two functions taking the same struct emits the decoder once.
    pub fn ensure_abi_struct_cd(&self, name: &str) -> Option<(String, usize)> {
        let fields = self.abi_struct_layout(name)?;
        let packed = self.abi_struct_packed(name);
        let fname = format!("gum_abi_st_{}_cd", name);
        let (f2, n2) = (fields.clone(), fname.clone());
        self.ensure_helper(&fname, || abi_st_cd_helper_src(&n2, &f2, packed));
        Some((fname, fields.len() * 32))
    }

    pub fn ensure_abi_struct_mem(&self, name: &str) -> Option<(String, usize)> {
        let fields = self.abi_struct_layout(name)?;
        let packed = self.abi_struct_packed(name);
        let fname = format!("gum_abi_st_{}_mem", name);
        let (f2, n2) = (fields.clone(), fname.clone());
        self.ensure_helper(&fname, || abi_st_mem_helper_src(&n2, &f2, packed));
        Some((fname, fields.len() * 32))
    }

    pub fn ensure_abi_struct_put(&self, name: &str) -> Option<(String, usize)> {
        let fields = self.abi_struct_layout(name)?;
        let fname = format!("gum_abi_st_{}_put", name);
        let (f2, n2) = (fields.clone(), fname.clone());
        self.ensure_helper(&fname, || abi_st_put_helper_src(&n2, &f2));
        Some((fname, fields.len() * 32))
    }

    // The struct-array codecs, each built on the single-struct codec above so the two can never disagree about a field's place.
    pub fn ensure_abi_struct_arr_cd(&self, name: &str) -> Option<String> {
        let (st, wire) = self.ensure_abi_struct_cd(name)?;
        let packed = self.abi_struct_packed(name);
        let fname = format!("gum_abi_starr_{}_cd", name);
        let (n2, s2) = (fname.clone(), st.clone());
        self.ensure_helper(&fname, || abi_starr_cd_helper_src(&n2, &s2, wire, packed));
        Some(fname)
    }

    pub fn ensure_abi_struct_arr_mem(&self, name: &str) -> Option<String> {
        let (st, wire) = self.ensure_abi_struct_mem(name)?;
        let packed = self.abi_struct_packed(name);
        let fname = format!("gum_abi_starr_{}_mem", name);
        let (n2, s2) = (fname.clone(), st.clone());
        self.ensure_helper(&fname, || abi_starr_mem_helper_src(&n2, &s2, wire, packed));
        Some(fname)
    }

    pub fn ensure_abi_struct_arr_put(&self, name: &str) -> Option<(String, String)> {
        let (st, wire) = self.ensure_abi_struct_put(name)?;
        let packed = self.abi_struct_packed(name);
        let fname = format!("gum_abi_starr_{}_put", name);
        let sname = format!("gum_abi_starr_{}_size", name);
        let (n2, s2) = (fname.clone(), st.clone());
        self.ensure_helper(&fname, || abi_starr_put_helper_src(&n2, &s2, wire, packed));
        let n3 = sname.clone();
        self.ensure_helper(&sname, || abi_starr_size_helper_src(&n3, wire, packed));
        Some((fname, sname))
    }

    // The element struct of an array type, or None if the element is not a struct with a static wire form.
    pub fn abi_struct_elem(&self, t: &Type) -> Option<String> {
        let inner = match t {
            Type::Array(inner) => inner,
            _ => return None,
        };
        match inner.as_ref() {
            Type::Primitive(n) if is_struct_type(self.type_checker(), inner) => {
                self.abi_struct_layout(n).map(|_| n.clone())
            }
            _ => None,
        }
    }

    pub fn ensure_abi_farr_cd(&self) {
        self.ensure_helper("gum_abi_farr_cd", gum_abi_farr_cd_helper_src);
    }

    pub fn ensure_abi_farr_mem(&self) {
        self.ensure_helper("gum_abi_farr_mem", gum_abi_farr_mem_helper_src);
    }

    pub fn ensure_abi_farr_put(&self) {
        self.ensure_helper("gum_abi_farr_put", gum_abi_farr_put_helper_src);
    }

    // Whether the ABI keeps this type behind an offset word instead of inline in the head.
    pub fn abi_is_dynamic(&self, t: &Type) -> bool {
        match t {
            Type::Array(_) => true,
            Type::FixedArray(inner, _) => self.abi_is_dynamic(inner),
            Type::Primitive(n) => n == "String" || n == "Bytes",
            _ => false,
        }
    }

    // The struct name of t if t is a struct with a static wire form, else None.
    fn abi_static_struct(&self, t: &Type) -> Option<String> {
        match t {
            Type::Primitive(n) if is_struct_type(self.type_checker(), t) => {
                self.abi_struct_layout(n).map(|_| n.clone())
            }
            _ => None,
        }
    }

    // The four ensure_abi_ below give every ABI type the same four codec signatures: cd(off), mem(base, off, limit), put(dst, ptr) -> written, size(ptr).
    pub fn ensure_abi_cd(&self, t: &Type) -> Option<String> {
        if let Some(sn) = self.abi_struct_elem(t) {
            return self.ensure_abi_struct_arr_cd(&sn);
        }
        let fname = format!("gum_abi_{}_cd", abi_mangle(t));
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some(fname);
        }
        let src = match t {
            Type::Array(inner) => {
                if self.abi_is_dynamic(inner) {
                    let ic = self.ensure_abi_cd(inner)?;
                    abi_dynarr_cd_helper_src(&fname, &ic)
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_arr_cd();
                    let esz = self.layout_engine.size_of(inner);
                    format!(
                        "function {}(off) -> ptr {{\n    ptr := gum_abi_arr_cd(off, {})\n}}\n",
                        fname, esz
                    )
                } else {
                    // A static element that is not a scalar, so it is inline on the wire but at its own width rather than one word: [u256; 3] rows, say.
                    let ic = self.ensure_abi_cd(inner)?;
                    let wire = self.abi_head_bytes(inner);
                    let packed = self.layout_engine.size_of(inner);
                    abi_starr_cd_helper_src(&fname, &ic, wire, packed)
                }
            }
            Type::FixedArray(inner, n) => {
                if self.abi_is_dynamic(inner) {
                    let ic = self.ensure_abi_cd(inner)?;
                    abi_dynfarr_cd_helper_src(&fname, &ic, *n)
                } else if let Some(sn) = self.abi_static_struct(inner) {
                    let (st, wire) = self.ensure_abi_struct_cd(&sn)?;
                    let packed = self.abi_struct_packed(&sn);
                    abi_statfarr_cd_helper_src(&fname, &st, *n, wire, packed)
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_farr_cd();
                    let esz = self.layout_engine.size_of(inner);
                    format!(
                        "function {}(off) -> ptr {{\n    ptr := gum_abi_farr_cd(off, {}, {})\n}}\n",
                        fname, n, esz
                    )
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        self.ensure_helper(&fname, || src);
        Some(fname)
    }

    pub fn ensure_abi_mem(&self, t: &Type) -> Option<String> {
        if let Some(sn) = self.abi_struct_elem(t) {
            return self.ensure_abi_struct_arr_mem(&sn);
        }
        let fname = format!("gum_abi_{}_mem", abi_mangle(t));
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some(fname);
        }
        let src = match t {
            Type::Array(inner) => {
                if self.abi_is_dynamic(inner) {
                    let ic = self.ensure_abi_mem(inner)?;
                    abi_dynarr_mem_helper_src(&fname, &ic)
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_arr_mem();
                    let esz = self.layout_engine.size_of(inner);
                    format!(
                        "function {}(base, off, limit) -> ptr {{\n    ptr := gum_abi_arr_mem(base, off, limit, {})\n}}\n",
                        fname, esz
                    )
                } else {
                    let ic = self.ensure_abi_mem(inner)?;
                    let wire = self.abi_head_bytes(inner);
                    let packed = self.layout_engine.size_of(inner);
                    abi_starr_mem_helper_src(&fname, &ic, wire, packed)
                }
            }
            Type::FixedArray(inner, n) => {
                if self.abi_is_dynamic(inner) {
                    let ic = self.ensure_abi_mem(inner)?;
                    abi_dynfarr_mem_helper_src(&fname, &ic, *n)
                } else if let Some(sn) = self.abi_static_struct(inner) {
                    let (st, wire) = self.ensure_abi_struct_mem(&sn)?;
                    let packed = self.abi_struct_packed(&sn);
                    abi_statfarr_mem_helper_src(&fname, &st, *n, wire, packed)
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_farr_mem();
                    let esz = self.layout_engine.size_of(inner);
                    format!(
                        "function {}(base, off, limit) -> ptr {{\n    ptr := gum_abi_farr_mem(base, off, limit, {}, {})\n}}\n",
                        fname, n, esz
                    )
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        self.ensure_helper(&fname, || src);
        Some(fname)
    }

    // Returns the put and size helper names together, since a caller that encodes a value always has to measure it first to lay out the head.
    pub fn ensure_abi_put(&self, t: &Type) -> Option<(String, String)> {
        if let Some(sn) = self.abi_struct_elem(t) {
            return self.ensure_abi_struct_arr_put(&sn);
        }
        let fname = format!("gum_abi_{}_put", abi_mangle(t));
        let sname = format!("gum_abi_{}_size", abi_mangle(t));
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some((fname, sname));
        }
        let (psrc, ssrc) = match t {
            Type::Array(inner) => {
                if self.abi_is_dynamic(inner) {
                    let (ip, is) = self.ensure_abi_put(inner)?;
                    (
                        abi_dynarr_put_helper_src(&fname, &ip),
                        abi_dynarr_size_helper_src(&sname, &is),
                    )
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_arr_put();
                    let esz = self.layout_engine.size_of(inner);
                    (
                        format!(
                            "function {}(dst, ptr) -> written {{\n    written := gum_abi_arr_put(dst, ptr, {})\n}}\n",
                            fname, esz
                        ),
                        format!(
                            "function {}(ptr) -> sz {{\n    sz := gum_abi_arr_size(ptr, {})\n}}\n",
                            sname, esz
                        ),
                    )
                } else {
                    let (ip, _) = self.ensure_abi_put(inner)?;
                    let wire = self.abi_head_bytes(inner);
                    let packed = self.layout_engine.size_of(inner);
                    (
                        abi_statarr_put_helper_src(&fname, &ip, wire, packed),
                        abi_starr_size_helper_src(&sname, wire, packed),
                    )
                }
            }
            Type::FixedArray(inner, n) => {
                if self.abi_is_dynamic(inner) {
                    let (ip, is) = self.ensure_abi_put(inner)?;
                    (
                        abi_dynfarr_put_helper_src(&fname, &ip, *n),
                        abi_dynfarr_size_helper_src(&sname, &is, *n),
                    )
                } else if let Some(sn) = self.abi_static_struct(inner) {
                    let (st, wire) = self.ensure_abi_struct_put(&sn)?;
                    let packed = self.abi_struct_packed(&sn);
                    (
                        abi_statfarr_put_helper_src(&fname, &st, *n, wire, packed),
                        format!("function {}(ptr) -> sz {{\n    ptr := ptr\n    sz := {}\n}}\n", sname, n * wire),
                    )
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_farr_put();
                    let esz = self.layout_engine.size_of(inner);
                    (
                        format!(
                            "function {}(dst, ptr) -> written {{\n    gum_abi_farr_put(dst, ptr, {}, {})\n    written := {}\n}}\n",
                            fname, n, esz, n * 32
                        ),
                        format!("function {}(ptr) -> sz {{\n    ptr := ptr\n    sz := {}\n}}\n", sname, n * 32),
                    )
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        self.ensure_helper(&fname, || psrc);
        self.ensure_helper(&sname, || ssrc);
        Some((fname, sname))
    }

    // How many bytes this type occupies in an ABI head: one word for a
    // scalar or a dynamic type's offset, N words inline for a fixed array.
    // Shared so the calldata-length guard, the head layout, and the decoders
    pub fn abi_head_bytes(&self, t: &Type) -> usize {
        if self.abi_is_dynamic(t) {
            return 32;
        }
        match t {
            // A fixed array of statics is its element repeated, and the element may itself be a struct or another fixed array, so this recurses rather than assuming one word each.
            Type::FixedArray(inner, n) => n * self.abi_head_bytes(inner),
            Type::Primitive(name) if is_struct_type(self.type_checker(), t) => {
                self.abi_struct_wire_size(name).unwrap_or(32)
            }
            _ => 32,
        }
    }

    // For codegen/mod.rs's synthesized serialize() functions, which need
    pub fn require_bytes_copy(&self) {
        self.ensure_helper("bytes_copy", bytes_copy_helper_src);
    }

    // Exposed for codegen/mod.rs's dispatcher parameter-loading loop, which
    pub fn mask_for_type(&self, val_expr: &str, type_def: &Type) -> String {
        mask_for_type(val_expr, type_def)
    }

    fn type_checker(&self) -> &crate::semantic::TypeChecker {
        self.layout_engine.type_checker
    }

    // Resolves an expression's static type without relying on the semantic
    // checker's symbol table, which is already stale by codegen time (its
    // per-function scopes were pushed and popped during the earlier
    fn static_type(&self, expr: &Expr, ctx: &Ctx) -> Type {
        match expr {
            Expr::Number(_) => Type::Primitive("u256".to_string()),
            Expr::StringLiteral(_) => Type::Primitive("String".to_string()),
            Expr::Identifier(name) => {
                if name == "self" {
                    if let Some(sc) = ctx.self_ctx {
                        return Type::Primitive(sc.class_name.clone());
                    }
                }
                if let Some(t) = ctx.locals.borrow().get(name) {
                    return t.clone();
                }
                if name == "true" || name == "false" {
                    return Type::Primitive("bool".to_string());
                }
                if self.type_checker().loaded_classes.contains_key(name)
                    || self.type_checker().loaded_enums.contains_key(name)
                {
                    return Type::Primitive(name.clone());
                }
                Type::Primitive("unknown".to_string())
            }
            Expr::PropertyAccess { base, property } => {
                if let Type::Primitive(class_name) = self.static_type(base, ctx) {
                    if class_name == "Account" && property == "code" {
                        return Type::Primitive("AccountCode".to_string());
                    }
                    if let Some(cd) = self.type_checker().loaded_classes.get(&class_name) {
                        if let Some(f) = cd.fields.iter().find(|f| &f.name == property) {
                            return f.type_def.clone();
                        }
                        // Child.Ancestor names the ancestor slice, matching the semantic pass, so a following .method() resolves against the ancestor.
                        if cd.parents.iter().any(|p| p == property)
                            && self.type_checker().loaded_classes.contains_key(property)
                        {
                            return Type::Primitive(property.clone());
                        }
                    }
                    if self.type_checker().loaded_enums.contains_key(&class_name) {
                        return Type::Primitive(class_name);
                    }
                }
                Type::Primitive("unknown".to_string())
            }
            Expr::IndexAccess { base, .. } => match self.static_type(base, ctx) {
                Type::Generic { name, args } if name == "HashMap" && args.len() == 2 => {
                    args[1].clone()
                }
                Type::Array(inner) => *inner,
                Type::FixedArray(inner, _) => *inner,
                _ => Type::Primitive("unknown".to_string()),
            },
            Expr::BinaryOp { left, operator, .. } => match operator.as_str() {
                "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||" => {
                    Type::Primitive("bool".to_string())
                }
                _ => self.static_type(left, ctx),
            },
            Expr::MethodCall { base, method, .. } => {
                let base_ty = self.static_type(base, ctx);
                if let Type::Generic { name, args } = &base_ty {
                    if name == "HashMap" && args.len() == 2 && method == "get" {
                        return args[1].clone();
                    }
                }
                if let Type::Primitive(class_name) = &base_ty {
                    // The builtin String/Bytes methods that yield the same kind back. The type checker already knows these; codegen has to agree, or a.concat(b).length types the intermediate as unknown and the .length read falls through to the offset catch-all.
                    if (class_name == "String" || class_name == "Bytes")
                        && matches!(method.as_str(), "concat" | "slice")
                    {
                        return base_ty.clone();
                    }
                    if let Some(cd) = self.type_checker().loaded_classes.get(class_name) {
                        if let Some(m) = cd.methods.iter().find(|m| &m.name == method) {
                            return m
                                .return_type
                                .clone()
                                .unwrap_or(Type::Primitive("unknown".to_string()));
                        }
                    }
                }
                Type::Primitive("unknown".to_string())
            }
            Expr::FnCall { name, args }
                if args.len() == 1 && self.type_checker().loaded_classes.contains_key(name) =>
            {
                Type::Primitive(name.clone())
            }
            Expr::FnCall { name, .. }
                if self.type_checker().function_return_types.contains_key(name) =>
            {
                self.type_checker().function_return_types[name].clone()
            }
            Expr::Instantiation { type_def, .. } => type_def.clone(),
            Expr::FString(_) => Type::Primitive("String".to_string()),
            Expr::Neg(inner) => match self.static_type(inner, ctx) {
                Type::Primitive(name) if numeric_meta(&name).is_some() => {
                    Type::Primitive("i256".to_string())
                }
                other => other,
            },
            Expr::Not(_) => Type::Primitive("bool".to_string()),
            Expr::ArrayLiteral(elements) => {
                let elem_type = elements
                    .first()
                    .map(|e| self.static_type(e, ctx))
                    .unwrap_or(Type::Primitive("u256".to_string()));
                Type::FixedArray(Box::new(elem_type), elements.len())
            }
            _ => Type::Primitive("unknown".to_string()),
        }
    }

    pub fn translate_statement(&self, stmt: &Statement, ctx: &Ctx) -> String {
        match stmt {
            Statement::VarDecl {
                name,
                type_def,
                value,
                ..
            } => {
                let inferred;
                let type_def: &Type = if matches!(type_def, Type::Primitive(s) if s == "_infer") {
                    inferred = match value {
                        Some(v) => self.static_type(v, ctx),
                        None => Type::Primitive("u256".to_string()),
                    };
                    &inferred
                } else {
                    type_def
                };
                ctx.declare(name, type_def);
                let val_expr = match value {
                    Some(Expr::ArrayLiteral(elements)) => {
                        let hint = if let Type::FixedArray(inner, _) = type_def {
                            Some(inner.as_ref())
                        } else {
                            None
                        };
                        mask_for_type(&self.translate_array_literal(elements, hint, ctx), type_def)
                    }
                    Some(v) => mask_for_type(&self.translate_expr(v, ctx), type_def),
                    None => match self.fresh_local_bytes(type_def) {
                        Some(bytes) => format!("allocate_memory({})", bytes),
                        None => "0".to_string(),
                    },
                };
                format!("let {} := {}\n", name, val_expr)
            }
            Statement::Assignment { target, value } => {
                let val_expr = self.translate_expr(value, ctx);
                match target {
                    Expr::Identifier(name) => {
                        format!("{} := {}\n", name, val_expr)
                    }
                    Expr::PropertyAccess { base, property } => {
                        self.translate_property_store(base, property, &val_expr, ctx)
                    }
                    Expr::IndexAccess { base, index } => {
                        if let Type::Generic { name, args: targs } = self.static_type(base, ctx) {
                            if name == "HashMap" && targs.len() == 2 {
                                if let Some(base_slot) = self.hashmap_base_slot_expr(base, ctx) {
                                    let idx = self.translate_expr(index, ctx);
                                    let tr = self.hashmap_transient(base, ctx);
                                    return format!(
                                        "{}(gum_hash_slot({}, {}), {})\n",
                                        st_op(tr),
                                        idx,
                                        base_slot,
                                        val_expr
                                    );
                                }
                            }
                        }
                        if let Some((base_slot, elem_size, len, tr)) =
                            self.storage_array_info(base, ctx)
                        {
                            let idx = self.translate_expr(index, ctx);
                            return self
                                .storage_array_set(base_slot, elem_size, len, &idx, &val_expr, tr);
                        }
                        if let Some((base_slot, elem_size, tr)) = self.dyn_storage_array(base, ctx)
                        {
                            let idx = self.translate_expr(index, ctx);
                            return self.dyn_array_set(base_slot, elem_size, &idx, &val_expr, tr);
                        }
                        let i = self.translate_expr(index, ctx);
                        let (addr_expr, stride) = self.mem_array_addr(base, &i, ctx);
                        let av = format!("__ma_{}", self.next_literal_id());
                        let mut out = format!("let {} := {}\n", av, addr_expr);
                        let elem_ty = match self.static_type(base, ctx) {
                            Type::Array(inner) | Type::FixedArray(inner, _) => Some(*inner),
                            _ => None,
                        };
                        // An inline element is the bytes themselves, so assigning one copies them; mstore would write the source pointer into the element's first word and drop the rest.
                        if elem_ty.as_ref().map(|t| self.elem_is_inline(t)).unwrap_or(false) {
                            out.push_str(&format!(
                                "gum_memory_copy({}, {}, {})\n",
                                val_expr, av, stride
                            ));
                        } else if stride >= 32 {
                            out.push_str(&format!("mstore({}, {})\n", av, val_expr));
                        } else {
                            let merged =
                                write_packed(&format!("mload({})", av), 0, stride, &val_expr);
                            out.push_str(&format!("mstore({}, {})\n", av, merged));
                        }
                        out
                    }
                    _ => format!("/* invalid assignment target */\n"),
                }
            }
            Statement::Delete { target } => self.translate_delete(target, ctx),
            Statement::Return { value: None } => {
                if ctx.is_entry {
                    if let Some(lock) = &ctx.lock_slot {
                        format!("tstore({}, 0)\nreturn(0, 0)\n", lock)
                    } else {
                        "return(0, 0)\n".to_string()
                    }
                } else {
                    "leave\n".to_string()
                }
            }
            Statement::Return { value: Some(value) } => {
                let mut val_expr = self.translate_expr(value, ctx);
                let mut is_dynamic = false;
                let mut struct_ret: Option<(String, usize)> = None;
                // The put and size helpers for an array return of any shape, plus whether the ABI puts it behind an offset word.
                let mut arr_ret: Option<(String, String, bool, usize)> = None;
                if let Some(ret_ty) = &ctx.return_type {
                    // A fixed-bytes value keeps its internal (right-aligned) form
                    // for gum-to-gum returns; the entry return below re-aligns it
                    // to the wire. mask_for_type here would shr it (decode direction).
                    if !matches!(ret_ty, Type::Primitive(n) if byte_width(n).is_some()) {
                        val_expr = mask_for_type(&val_expr, ret_ty);
                    }
                    if let Type::Primitive(name) = ret_ty {
                        if name == "String" || name == "Bytes" {
                            is_dynamic = true;
                        }
                        if ctx.is_entry && is_struct_type(self.type_checker(), ret_ty) {
                            struct_ret = self.ensure_abi_struct_put(name);
                        }
                    }
                    if ctx.is_entry && matches!(ret_ty, Type::Array(_) | Type::FixedArray(..)) {
                        if let Some((put, size_fn)) = self.ensure_abi_put(ret_ty) {
                            arr_ret = Some((
                                put,
                                size_fn,
                                self.abi_is_dynamic(ret_ty),
                                self.abi_head_bytes(ret_ty),
                            ));
                        }
                    }
                }
                if ctx.is_entry {
                    // tstore, not sstore: the lock lives in transient storage.
                    let lock_clear = match &ctx.lock_slot {
                        Some(lock) => format!("tstore({}, 0)\n", lock),
                        None => String::new(),
                    };
                    if let Some((put, size_fn, dynamic, head)) = arr_ret {
                        if dynamic {
                            format!(
                                "let _p := {val}\n\
                                 let _out := allocate_memory(add(32, {size_fn}(_p)))\n\
                                 mstore(_out, 32)\n\
                                 let _w := {put}(add(_out, 32), _p)\n\
                                 {lock_clear}\
                                 return(_out, add(32, _w))\n",
                                val = val_expr,
                                size_fn = size_fn,
                                put = put,
                                lock_clear = lock_clear
                            )
                        } else {
                            format!(
                                "let _p := {val}\n\
                                 let _out := allocate_memory({head})\n\
                                 pop({put}(_out, _p))\n\
                                 {lock_clear}\
                                 return(_out, {head})\n",
                                val = val_expr,
                                head = head,
                                put = put,
                                lock_clear = lock_clear
                            )
                        }
                    } else if let Some((helper, wire)) = struct_ret {
                        format!(
                            "let _p := {val}\n\
                             let _out := allocate_memory({wire})\n\
                             {helper}(_out, _p)\n\
                             {lock_clear}\
                             return(_out, {wire})\n",
                            val = val_expr,
                            wire = wire,
                            helper = helper,
                            lock_clear = lock_clear
                        )
                    } else if is_dynamic {
                        // Bound to a local first, like every other branch here: the expression was substituted twice, once for the length and once for the copy, so return I(t).name() made the external call twice.
                        format!(
                            "let _p := {val}\n\
                             let _len := and(shr(192, mload(_p)), 0xffffffffffffffff)\n\
                             let _padded_len := and(add(_len, 31), not(31))\n\
                             let _out := allocate_memory(add(64, _padded_len))\n\
                             mstore(_out, 32)\n\
                             mstore(add(_out, 32), _len)\n\
                             gum_memory_copy(add(_p, 32), add(_out, 64), _len)\n\
                             {lock_clear}\
                             return(_out, add(64, _padded_len))\n",
                            val = val_expr,
                            lock_clear = lock_clear
                        )
                    } else {
                        // A sub-word fixed-bytes return is laid out left-aligned
                        // on the wire; b32 (and non-byte scalars) go out as is.
                        let enc = match &ctx.return_type {
                            Some(Type::Primitive(rn)) => match byte_width(rn) {
                                Some(w) if w < 32 => format!("shl({}, {})", (32 - w) * 8, val_expr),
                                _ => val_expr.clone(),
                            },
                            _ => val_expr.clone(),
                        };
                        format!("mstore(0, {})\n{}return(0, 32)\n", enc, lock_clear)
                    }
                } else {
                    format!("ret := {}\nleave\n", val_expr)
                }
            }
            Statement::Assert { condition, message } => {
                let cond_expr = self.translate_expr(condition, ctx);
                match message {
                    None => format!("if iszero({}) {{ revert(0, 0) }}\n", cond_expr),
                    Some(msg) => {
                        let body = self.assert_failure_data(msg, ctx);
                        let mut out = format!("if iszero({}) {{\n", cond_expr);
                        for line in body.lines() {
                            out.push_str(&format!("    {}\n", line));
                        }
                        out.push_str("}\n");
                        out
                    }
                }
            }
            Statement::Revert { error } => {
                let (base, method, args) = match error {
                    Expr::MethodCall { base, method, args } => (base, method.as_str(), args.as_slice()),
                    Expr::PropertyAccess { base, property } => (base, property.as_str(), &[] as &[Expr]),
                    _ => unreachable!(),
                };
                let Expr::Identifier(enum_name) = &**base else { unreachable!() };
                let enum_decl = self.type_checker().loaded_enums.get(enum_name).unwrap();
                let variant = enum_decl.variants.iter().find(|v| v.name == method).unwrap();
                let abi_gen = AbiGenerator::new(self.type_checker());
                let selector = abi_gen.calculate_error_selector(variant);
                let types: Vec<Type> =
                    variant.parameters.iter().map(|p| p.type_def.clone()).collect();
                self.emit_revert_data(
                    &format!("Revert {}", method),
                    &selector,
                    args,
                    &types,
                    ctx,
                )
            }

            Statement::IfElse {
                condition,
                if_body,
                else_body,
            } => {
                let cond_expr = self.translate_expr(condition, ctx);
                let mut out = format!("if {} {{\n", cond_expr);
                for s in if_body {
                    out.push_str(&self.translate_statement(&s.node, ctx));
                }
                out.push_str("}\n");
                if let Some(eb) = else_body {
                    out.push_str("if iszero(");
                    out.push_str(&cond_expr);
                    out.push_str(") {\n");
                    for s in eb {
                        out.push_str(&self.translate_statement(&s.node, ctx));
                    }
                    out.push_str("}\n");
                }
                out
            }
            Statement::WhileLoop { condition, body } => {
                let cond_expr = self.translate_expr(condition, ctx);
                let mut out = format!("for {{}} {} {{}} {{\n", cond_expr);
                for s in body {
                    out.push_str(&self.translate_statement(&s.node, ctx));
                }
                out.push_str("}\n");
                out
            }
            Statement::TryCatch { try_body, catch_body } => {
                self.ensure_helper("gum_exception_helpers", gum_exception_helpers_src);
                let try_id = self.next_literal_id();
                let try_ok_var = format!("__try_ok_{}", try_id);
                let mut out = format!("let {} := 1\n", try_ok_var);
                out.push_str("for {} 1 {} {\n");
                
                let inner_ctx = Ctx {
                    self_ctx: ctx.self_ctx,
                    is_entry: ctx.is_entry,
                    try_ok_var: Some(try_ok_var.clone()),
                    locals: ctx.locals.clone(),
                    return_type: ctx.return_type.clone(),
                    lock_slot: ctx.lock_slot.clone(),
                    in_constructor: ctx.in_constructor,
                };
                
                for s in try_body {
                    out.push_str(&self.translate_statement(&s.node, &inner_ctx));
                    out.push_str(&format!("    if gum_check_exception() {{\n        {} := 0\n        break\n    }}\n", try_ok_var));
                }
                out.push_str("    break\n}\n");
                
                out.push_str(&format!("if iszero({}) {{\n", try_ok_var));
                for s in catch_body {
                    out.push_str(&self.translate_statement(&s.node, ctx));
                }
                out.push_str("}\n");
                out
            }
            Statement::ForLoop {
                iterator,
                iterable,
                body,
            } => self.translate_for_loop(iterator, iterable, body, ctx),
            Statement::BitwiseFlip { name, index, value } => {
                let idx = self.translate_expr(index, ctx);
                let val = self.translate_expr(value, ctx);
                format!(
                    "{} := or(and({}, not(shl({}, 1))), shl({}, and({}, 1)))\n",
                    name, name, idx, idx, val
                )
            }
            Statement::UnsafeBlock(code) => {
                let start = code.find('{').map(|i| i + 1).unwrap_or(0);
                let end = code.rfind('}').unwrap_or(code.len());
                format!("{}\n", code[start..end].trim())
            }
            Statement::Match { expr, arms } => {
                let match_expr = self.translate_expr(expr, ctx);
                let mv = format!("__match_{}", self.next_literal_id());
                let mut out = format!("let {} := {}\n", mv, match_expr);
                // A payload-free enum is the tag; a payload-carrying one is a pointer to [tag][payload], so only that one dereferences.
                let scalar_enum = self.type_checker().is_scalar_enum(&self.static_type(expr, ctx));
                if scalar_enum {
                    out.push_str(&format!("switch {}\n", mv));
                } else {
                    out.push_str(&format!("switch mload({})\n", mv));
                }
                for (i, arm) in arms.iter().enumerate() {
                    out.push_str(&format!("case {} {{\n", i));
                    if let Some(payload_var) = &arm.payload_var {
                        out.push_str(&format!(
                            "    let {} := mload(add({}, 32))\n",
                            payload_var, mv
                        ));
                    }
                    for s in &arm.body {
                        let stmt_out = self.translate_statement(&s.node, ctx);
                        for line in stmt_out.lines() {
                            out.push_str(&format!("    {}\n", line));
                        }
                    }
                    out.push_str("}\n");
                }
                out
            }
            Statement::Call { target, args } => {
                return self.extcall_wrapper_src("Interface", target, args, ctx);
            }
            Statement::Expression(expr) => {
                if let Expr::FnCall { name, args } = expr {
                    if name == "log" {
                        return self.translate_log_stmt(args, ctx);
                    }
                }
                let code = self.translate_expr(expr, ctx);
                // A call used as a statement discards its result. Yul rejects a
                let discards_value = matches!(expr, Expr::MethodCall { .. } | Expr::FnCall { .. })
                    && !matches!(self.static_type(expr, ctx), Type::Primitive(ref n) if n == "unknown");
                if discards_value {
                    format!("pop({})\n", code)
                } else {
                    format!("{}\n", code)
                }
            }
        }
    }

    fn translate_log_stmt(&self, args: &[Expr], ctx: &Ctx) -> String {
        if args.is_empty() {
            return "/* log() requires an event argument */\n".to_string();
        }
        let event_name = match &args[0] {
            Expr::PropertyAccess { property, .. } => property.clone(),
            Expr::Identifier(name) => name.clone(),
            _ => "UnknownEvent".to_string(),
        };

        let fields: Vec<(bool, &Expr)> = args[1..]
            .iter()
            .map(|a| {
                if let Expr::FnCall { name, args: inner } = a {
                    if name == "indexed" && inner.len() == 1 {
                        return (true, &inner[0]);
                    }
                }
                (false, a)
            })
            .collect();

        let field_types: Vec<Type> = fields.iter().map(|(_, e)| self.static_type(e, ctx)).collect();
        let sig_types: Vec<String> = field_types
            .iter()
            .map(|t| self.abi_gen.signature_type(t))
            .collect();
        let signature = format!("{}({})", event_name, sig_types.join(","));
        let topic0 = keccak256_hex(&signature);

        // The JSON names a struct "tuple" and carries its fields in components, while topic0 is hashed from the expanded "(uint128,uint256)" form. They are the same type spelled two ways, and using the signature spelling in the JSON emitted a "type": "(uint256)" no decoder accepts.
        let inputs: Vec<AbiInput> = fields
            .iter()
            .zip(&field_types)
            .map(|((indexed, e), t)| AbiInput {
                name: match e {
                    Expr::Identifier(n) => n.clone(),
                    _ => String::new(),
                },
                type_name: self.abi_gen.map_type(t),
                components: self.abi_gen.generate_components(t),
                indexed: Some(*indexed),
            })
            .collect();
        if let Err(e) = self.record_event(&event_name, EventSchema { inputs, signature }) {
            self.errors.borrow_mut().push(e);
        }

        // Solidity puts a hash of the value in the topic for anything that is not one word, since a topic is exactly 32 bytes. Rejecting is better than emitting the pointer, which is what this did.
        let mut topics = vec![topic0];
        for ((indexed, e), t) in fields.iter().zip(&field_types) {
            if *indexed {
                if !is_abi_scalar(t) && !self.type_checker().is_scalar_enum(t) {
                    self.errors.borrow_mut().push(format!(
                        "Semantic Error: an indexed field of event '{}' must be one word, and '{}' is not. A topic is 32 bytes, so a longer value would have to be hashed to fit. Log it unindexed.",
                        event_name,
                        self.abi_gen.signature_type(t)
                    ));
                }
                topics.push(self.translate_expr(e, ctx));
            }
        }
        let log_op = format!("log{}", topics.len()); // topics.len() is 1..=4

        let data: Vec<(String, Type)> = fields
            .iter()
            .zip(&field_types)
            .filter(|((indexed, _), _)| !indexed)
            .map(|((_, e), t)| (self.translate_expr(e, ctx), t.clone()))
            .collect();

        if data.is_empty() {
            return format!("{}(0, 0, {})\n", log_op, topics.join(", "));
        }

        // The event data is an ABI argument list like any other, so it goes through the encoder the CREATE and interface-call paths use rather than one word per field.
        let types: Vec<Type> = data.iter().map(|(_, t)| t.clone()).collect();
        let (size_src, write_src) = self.abi_arg_blob_src(&types);
        // A block, so the a0..aN the encoder emits are scoped to this log and a second log in the same function does not redeclare them.
        let mut out = String::from("{\n");
        for (i, (e, _)) in data.iter().enumerate() {
            out.push_str(&format!("let a{} := {}\n", i, e));
        }
        out.push_str(&size_src);
        out.push_str("let blob := allocate_memory(alen)\n");
        out.push_str(&write_src);
        out.push_str(&format!("{}(blob, alen, {})\n", log_op, topics.join(", ")));
        out.push_str("}\n");
        out
    }

    fn translate_property_store(
        &self,
        base: &Expr,
        property: &str,
        val_expr: &str,
        ctx: &Ctx,
    ) -> String {
        if let Expr::Identifier(base_name) = base {
            let owner = if base_name == "self" {
                ctx.self_ctx
                    .filter(|s| s.is_global)
                    .map(|s| s.class_name.clone())
            } else {
                Some(base_name.clone())
            };
            if let Some(owner) = owner {
                if self
                    .layout_engine
                    .immutable_field(&owner, property)
                    .is_some()
                {
                    if self
                        .layout_engine
                        .const_field_value(&owner, property)
                        .is_some()
                    {
                        return format!("// const {}.{} folded at compile time\n", owner, property);
                    }
                    return format!("{} := {}\n", immutable_local(property), val_expr);
                }
            }
        }
        if let Expr::Identifier(base_name) = base {
            if base_name == "self" {
                if let Some(self_ctx) = ctx.self_ctx {
                    if self_ctx.is_global {
                        if let Some(sf) = self
                            .layout_engine
                            .storage_field(&self_ctx.class_name, property)
                        {
                            return self.store_storage_field(
                                &self_ctx.class_name,
                                property,
                                &sf,
                                val_expr,
                            );
                        }
                    } else if let Some(mf) = self
                        .layout_engine
                        .memory_field(&self_ctx.class_name, property)
                    {
                        return self.store_memory_field("self", &mf, val_expr);
                    }
                }
            }
            if let Some(sf) = self.layout_engine.storage_field(base_name, property) {
                return self.store_storage_field(base_name, property, &sf, val_expr);
            }
        }
        if let Some((base_slot, struct_name)) = self.struct_storage_base(base, ctx) {
            if let Some((slot, off, size)) =
                self.struct_field_slot(&base_slot, &struct_name, property)
            {
                let tr = self.struct_base_transient(base, ctx);
                if off == 0 && size >= 32 {
                    return format!("{}({}, {})\n", st_op(tr), slot, val_expr);
                }
                let merged =
                    write_slot_packed(&format!("{}({})", ld_op(tr), slot), off, size, val_expr);
                return format!("{}({}, {})\n", st_op(tr), slot, merged);
            }
        }
        if let Type::Primitive(class_name) = self.static_type(base, ctx) {
            if let Some(mf) = self.layout_engine.memory_field(&class_name, property) {
                let b = self.translate_expr(base, ctx);
                return self.store_memory_field(&b, &mf, val_expr);
            }
        }
        // No guess here. Every resolver above knows an offset; falling past them means the property has no known place, and assuming 0 wrote the value over whatever sits at the base instead.
        self.errors.borrow_mut().push(format!(
            "no known storage or memory offset for '{}' on a {}, so it cannot be assigned. This is a compiler gap rather than a mistake in your code: please report it.",
            property,
            self.abi_gen.signature_type(&self.static_type(base, ctx))
        ));
        String::new()
    }

    fn field_is_str(&self, class_name: &str, property: &str) -> bool {
        self.type_checker()
            .loaded_classes
            .get(class_name)
            .and_then(|c| c.fields.iter().find(|f| f.name == property))
            .map(|f| is_str_type(&f.type_def))
            .unwrap_or(false)
    }

    fn load_storage_field(&self, class_name: &str, property: &str, sf: &StorageField) -> String {
        let tr = sf.is_transient;
        if self.field_is_str(class_name, property) {
            self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
            self.ensure_helper(&format!("gum_sstr_load{}", kind_suffix(tr)), || {
                gum_sstr_load_helper_src(tr)
            });
            return format!("gum_sstr_load{}({})", kind_suffix(tr), sf.slot);
        }
        if sf.size <= 32 {
            return read_slot_packed(
                &format!("{}({})", ld_op(tr), sf.slot),
                sf.offset_in_slot,
                sf.size,
            );
        }
        let fn_name = format!("__load_{}_{}_{}", class_name, sf.slot, tr);
        let (slot, size) = (sf.slot, sf.size);
        self.ensure_helper(&fn_name, || {
            let n = (size + 31) / 32;
            let mut body = format!("function {}() -> ptr {{\n", fn_name);
            body.push_str(&format!("    ptr := allocate_memory({})\n", size));
            for i in 0..n {
                body.push_str(&format!(
                    "    mstore(add(ptr, {}), {}({}))\n",
                    i * 32,
                    ld_op(tr),
                    slot + i
                ));
            }
            body.push_str("}\n");
            body
        });
        format!("{}()", fn_name)
    }

    fn store_storage_field(
        &self,
        class_name: &str,
        property: &str,
        sf: &StorageField,
        val_expr: &str,
    ) -> String {
        let tr = sf.is_transient;
        if self.field_is_str(class_name, property) {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
            self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
            self.ensure_helper(&format!("gum_sstr_store{}", kind_suffix(tr)), || {
                gum_sstr_store_helper_src(tr)
            });
            return format!(
                "gum_sstr_store{}({}, {})\n",
                kind_suffix(tr),
                sf.slot,
                val_expr
            );
        }
        if sf.size > 32 {
            let tmp = format!("__src_{}", self.next_literal_id());
            let n = (sf.size + 31) / 32;
            let mut out = format!("let {} := {}\n", tmp, val_expr);
            for i in 0..n {
                out.push_str(&format!(
                    "{}({}, mload(add({}, {})))\n",
                    st_op(tr),
                    sf.slot + i,
                    tmp,
                    i * 32
                ));
            }
            return out;
        }
        if sf.offset_in_slot == 0 && sf.size == 32 {
            format!("{}({}, {})\n", st_op(tr), sf.slot, val_expr)
        } else {
            let merged = write_slot_packed(
                &format!("{}({})", ld_op(tr), sf.slot),
                sf.offset_in_slot,
                sf.size,
                val_expr,
            );
            format!("{}({}, {})\n", st_op(tr), sf.slot, merged)
        }
    }

    fn load_memory_field(&self, base_ptr_expr: &str, mf: &MemoryField) -> String {
        if mf.size > 32 {
            format!("add({}, {})", base_ptr_expr, mf.offset)
        } else {
            read_packed(
                &format!("mload(add({}, {}))", base_ptr_expr, mf.offset),
                0,
                mf.size,
            )
        }
    }

    fn store_memory_field(&self, base_ptr_expr: &str, mf: &MemoryField, val_expr: &str) -> String {
        let addr = format!("add({}, {})", base_ptr_expr, mf.offset);
        if mf.size > 32 {
            self.ensure_helper("bytes_copy", bytes_copy_helper_src);
            return format!("bytes_copy({}, {}, {})\n", addr, val_expr, mf.size);
        }
        if mf.size == 32 {
            format!("mstore({}, {})\n", addr, val_expr)
        } else {
            let merged = write_packed(&format!("mload({})", addr), 0, mf.size, val_expr);
            format!("mstore({}, {})\n", addr, merged)
        }
    }

    pub fn translate_expr(&self, expr: &Expr, ctx: &Ctx) -> String {
        match expr {
            Expr::Number(n) => n.clone(),
            Expr::StringLiteral(s) => self.translate_string_literal(s),
            Expr::Identifier(name) => {
                if name == "true" {
                    "1".to_string()
                } else if name == "false" {
                    "0".to_string()
                } else {
                    name.clone()
                }
            }
            Expr::BinaryOp {
                left,
                operator,
                right,
            } => self.translate_binary_op(left, operator, right, ctx),
            Expr::Instantiation { type_def, args } => {
                self.translate_instantiation(type_def, args, ctx)
            }
            Expr::MethodCall { base, method, args } => {
                self.translate_method_call(base, method, args, ctx)
            }
            Expr::FString(segments) => self.translate_fstring(segments, ctx),
            Expr::Neg(inner) => format!("sub(0, {})", self.translate_expr(inner, ctx)),
            Expr::Not(inner) => format!("iszero({})", self.translate_expr(inner, ctx)),
            Expr::ArrayLiteral(elements) => self.translate_array_literal(elements, None, ctx),
            Expr::PropertyAccess { base, property } => {
                if let Type::Primitive(class_name) = self.static_type(base, ctx) {
                    if class_name == "Account" && property == "code" {
                        return self.translate_expr(base, ctx);
                    }
                }
                if let Expr::Identifier(base_name) = &**base {
                    let owner = if base_name == "self" {
                        ctx.self_ctx
                            .filter(|s| s.is_global)
                            .map(|s| s.class_name.clone())
                    } else {
                        Some(base_name.clone())
                    };
                    if let Some(owner) = owner {
                        if self
                            .layout_engine
                            .immutable_field(&owner, property)
                            .is_some()
                        {
                            if let Some(v) = self.layout_engine.const_field_value(&owner, property)
                            {
                                return v;
                            }
                            return format!(
                                "loadimmutable(\"{}\")",
                                immutable_key(&owner, property)
                            );
                        }
                    }
                    if base_name == "self" {
                        if let Some(self_ctx) = ctx.self_ctx {
                            if self_ctx.is_global {
                                if let Some(sf) = self
                                    .layout_engine
                                    .storage_field(&self_ctx.class_name, property)
                                {
                                    return self.load_storage_field(
                                        &self_ctx.class_name,
                                        property,
                                        &sf,
                                    );
                                }
                            } else if let Some(mf) = self
                                .layout_engine
                                .memory_field(&self_ctx.class_name, property)
                            {
                                return self.load_memory_field("self", &mf);
                            }
                        }
                    }
                    if let Some(sf) = self.layout_engine.storage_field(base_name, property) {
                        if let Some(copy) = self.storage_array_to_memory(expr, ctx) {
                            return copy;
                        }
                        return self.load_storage_field(base_name, property, &sf);
                    }
                    if let Some(enum_decl) = self.type_checker().loaded_enums.get(base_name) {
                        if let Some(idx) =
                            enum_decl.variants.iter().position(|v| &v.name == property)
                        {
                            // A payload-free enum is its tag, full stop: no allocation, no pointer, and it stores, logs and encodes as the u8 it is.
                            if !self.type_checker().enum_has_payload(base_name) {
                                return idx.to_string();
                            }
                            self.ensure_helper("make_enum", make_enum_helper_src);
                            return format!("make_enum({}, 0)", idx);
                        }
                    }
                }
                if property == "length" {
                    if let Some((slot, _, tr)) = self.dyn_storage_array(base, ctx) {
                        return format!("{}({})", ld_op(tr), slot);
                    }
                    if let Type::FixedArray(_, n) = self.static_type(base, ctx) {
                        return n.to_string();
                    }
                    if let Type::Array(inner) = self.static_type(base, ctx) {
                        let esz = self.layout_engine.size_of(&inner).max(1);
                        let b = self.translate_expr(base, ctx);
                        return format!("div(mload({}), {})", b, esz);
                    }
                }
                if let Some((base_slot, struct_name)) = self.struct_storage_base(base, ctx) {
                    if let Some((slot, off, size)) =
                        self.struct_field_slot(&base_slot, &struct_name, property)
                    {
                        let tr = self.struct_base_transient(base, ctx);
                        return read_slot_packed(&format!("{}({})", ld_op(tr), slot), off, size);
                    }
                }
                if let Type::Primitive(class_name) = self.static_type(base, ctx) {
                    if let Some(mf) = self.layout_engine.memory_field(&class_name, property) {
                        return self.load_memory_field(&self.translate_expr(base, ctx), &mf);
                    }
                }
                // The read half of the same rule: offset 0 is not a default, it is the first field, so an unresolved property silently read the wrong one.
                self.errors.borrow_mut().push(format!(
                    "no known storage or memory offset for '{}' on a {}, so it cannot be read. This is a compiler gap rather than a mistake in your code: please report it.",
                    property,
                    self.abi_gen.signature_type(&self.static_type(base, ctx))
                ));
                "0".to_string()
            }
            Expr::IndexAccess { base, index } => {
                if is_str_type(&self.static_type(base, ctx)) {
                    let rich = self.rich_reverts;
                    self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                    self.ensure_helper("gum_str_at", || gum_str_at_helper_src(rich));
                    let b = self.translate_expr(base, ctx);
                    let i = self.translate_expr(index, ctx);
                    return format!("gum_str_at({}, {})", b, i);
                }
                if let Type::Generic { name, args: targs } = self.static_type(base, ctx) {
                    if name == "HashMap" && targs.len() == 2 {
                        if let Some(base_slot) = self.hashmap_base_slot_expr(base, ctx) {
                            let idx = self.translate_expr(index, ctx);
                            let slot = format!("gum_hash_slot({}, {})", idx, base_slot);
                            let value_is_map = matches!(&targs[1], Type::Generic { name, .. } if name == "HashMap");
                            if value_is_map {
                                return slot;
                            }
                            let tr = self.hashmap_transient(base, ctx);
                            return format!("{}({})", ld_op(tr), slot);
                        }
                    }
                }
                if let Some((base_slot, elem_size, len, tr)) = self.storage_array_info(base, ctx) {
                    let idx = self.translate_expr(index, ctx);
                    return self.storage_array_get(base_slot, elem_size, len, &idx, tr);
                }
                if let Some((slot, elem_size, tr)) = self.dyn_storage_array(base, ctx) {
                    let idx = self.translate_expr(index, ctx);
                    return self.dyn_array_get(slot, elem_size, &idx, tr);
                }
                let i = self.translate_expr(index, ctx);
                let (addr, stride) = self.mem_array_addr(base, &i, ctx);
                if self.elem_is_inline(&self.static_type(expr, ctx)) {
                    return addr;
                }
                read_packed(&format!("mload({})", addr), 0, stride)
            }
            Expr::FnCall { name, args } => {
                if name == "keccak256" && args.len() == 1 {
                    let p = self.translate_expr(&args[0], ctx);
                    return if is_str_type(&self.static_type(&args[0], ctx)) {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_keccak_str", gum_keccak_str_helper_src);
                        format!("gum_keccak_str({})", p)
                    } else {
                        self.ensure_helper("gum_keccak_arr", gum_keccak_arr_helper_src);
                        format!("gum_keccak_arr({})", p)
                    };
                }
                if name == "ecrecover" && args.len() == 4 {
                    self.ensure_helper("gum_ecrecover", gum_ecrecover_helper_src);
                    let a: Vec<String> = args.iter().map(|x| self.translate_expr(x, ctx)).collect();
                    return format!("gum_ecrecover({})", a.join(", "));
                }

                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                let callee = if self.top_level_fns.contains(name) {
                    format!("{}_impl", name)
                } else {
                    name.clone()
                };
                if arg_strs.is_empty() {
                    format!("{}()", callee)
                } else {
                    format!("{}({})", callee, arg_strs.join(", "))
                }
            }
        }
    }

    // Lays out an ABI argument list for a helper whose parameters are a0..aN-1, shared by the CREATE encoder and the interface-call encoder because the wire format does not care which one is writing.
    fn abi_arg_blob_src(&self, types: &[Type]) -> (String, String) {
        let head_bytes: usize = types.iter().map(|t| self.abi_head_bytes(t)).sum();
        let head_at: Vec<usize> = types
            .iter()
            .scan(0usize, |acc, t| {
                let at = *acc;
                *acc += self.abi_head_bytes(t);
                Some(at)
            })
            .collect();
        let any_dynamic = types.iter().any(|t| is_str_type(t) || self.abi_is_dynamic(t));

        if types.iter().any(is_str_type) {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
        }
        let struct_put: Vec<Option<String>> = types
            .iter()
            .map(|t| match t {
                Type::Primitive(n) if is_struct_type(self.type_checker(), t) => {
                    self.ensure_abi_struct_put(n).map(|(h, _)| h)
                }
                _ => None,
            })
            .collect();
        // One codec per array argument whatever its shape, so an outbound nested array encodes by the same path a flat one does.
        let arr_put: Vec<Option<(String, String, bool)>> = types
            .iter()
            .map(|t| {
                if matches!(t, Type::Array(_) | Type::FixedArray(..)) {
                    self.ensure_abi_put(t)
                        .map(|(p, s)| (p, s, self.abi_is_dynamic(t)))
                } else {
                    None
                }
            })
            .collect();

        // Only a dynamic value adds to alen; a static array is already counted, since abi_head_bytes gives its whole inline width rather than one offset word.
        let mut size_src = format!("    let alen := {}\n", head_bytes);
        for (i, t) in types.iter().enumerate() {
            if is_str_type(t) {
                size_src.push_str(&format!("    let a{i}_len := gum_str_len(a{i})\n", i = i));
                size_src.push_str(&format!("    let a{i}_pad := and(add(a{i}_len, 31), not(31))\n", i = i));
                size_src.push_str(&format!("    alen := add(alen, add(32, a{}_pad))\n", i));
            } else if let Some((_, size_fn, true)) = &arr_put[i] {
                size_src.push_str(&format!("    let a{i}_abi := {s}(a{i})\n", i = i, s = size_fn));
                size_src.push_str(&format!("    alen := add(alen, a{}_abi)\n", i));
            }
        }

        let mut write_src = String::new();
        if any_dynamic {
            write_src.push_str(&format!("    let tail := {}\n", head_bytes));
        }
        for (i, t) in types.iter().enumerate() {
            let at = head_at[i];
            if is_str_type(t) {
                write_src.push_str(&format!("    mstore(add(blob, {}), tail)\n", at));
                write_src.push_str(&format!("    mstore(add(blob, tail), a{}_len)\n", i));
                write_src.push_str(&format!(
                    "    gum_memory_copy(add(a{i}, 32), add(add(blob, tail), 32), a{i}_len)\n",
                    i = i
                ));
                write_src.push_str(&format!("    tail := add(tail, add(32, a{}_pad))\n", i));
            } else if let Some((put, _, dynamic)) = &arr_put[i] {
                if *dynamic {
                    write_src.push_str(&format!("    mstore(add(blob, {}), tail)\n", at));
                    write_src.push_str(&format!(
                        "    tail := add(tail, {}(add(blob, tail), a{}))\n",
                        put, i
                    ));
                } else {
                    write_src.push_str(&format!("    pop({}(add(blob, {}), a{}))\n", put, at, i));
                }
            } else if let Some(helper) = &struct_put[i] {
                write_src.push_str(&format!("    {}(add(blob, {}), a{})\n", helper, at, i));
            } else {
                write_src.push_str(&format!("    mstore(add(blob, {}), a{})\n", at, i));
            }
        }
        (size_src, write_src)
    }

    fn translate_contract_deploy(&self, name: &str, args: &[Expr], ctx: &Ctx) -> String {
        let is_ctx_param =
            |t: &Type| matches!(t, Type::Primitive(n) if n == "Message" || n == "Block");

        let ctor_params: Vec<Type> = self
            .type_checker()
            .loaded_classes
            .get(name)
            .and_then(|c| c.methods.iter().find(|m| m.name == "new"))
            .map(|m| m.parameters.iter().map(|p| p.type_def.clone()).collect())
            .unwrap_or_default();

        let passed: Vec<(String, Type)> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                ctor_params
                    .get(*i)
                    .map(|t| !is_ctx_param(t))
                    .unwrap_or(true)
            })
            .map(|(i, a)| {
                let t = ctor_params
                    .get(i)
                    .cloned()
                    .unwrap_or(Type::Primitive("u256".to_string()));
                (self.translate_expr(a, ctx), t)
            })
            .collect();

        let types: Vec<Type> = passed.iter().map(|(_, t)| t.clone()).collect();
        let (size_src, write_src) = self.abi_arg_blob_src(&types);
        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);

        let key = format!("__deploy_{}", name);
        let nm = name.to_string();
        self.ensure_helper(&key, || {
            let names: Vec<String> = (0..types.len()).map(|i| format!("a{}", i)).collect();
            let mut b = String::new();
            b.push_str(&format!(
                "function {}({}) -> addr {{
",
                key,
                names.join(", ")
            ));
            b.push_str(&format!("    let size := datasize(\"{}\")
", nm));
            b.push_str(&size_src);
            b.push_str("    let ptr := allocate_memory(add(size, alen))
");
            b.push_str(&format!(
                "    datacopy(ptr, dataoffset(\"{}\"), size)
",
                nm
            ));
            b.push_str("    let blob := add(ptr, size)
");
            b.push_str(&write_src);
            b.push_str("    addr := create(0, ptr, add(size, alen))
");
            b.push_str("    if iszero(addr) { gum_bubble_revert() }
");
            b.push_str("}
");
            b
        });

        let call_args: Vec<String> = passed.into_iter().map(|(e, _)| e).collect();
        format!("{}({})", key, call_args.join(", "))
    }

    fn translate_instantiation(&self, type_def: &Type, args: &[Expr], ctx: &Ctx) -> String {
        if let Type::Primitive(name) = type_def {
            if self
                .type_checker()
                .loaded_classes
                .get(name)
                .map(|c| c.is_global)
                .unwrap_or(false)
            {
                return self.translate_contract_deploy(name, args, ctx);
            }
        }

        let (class_name, suffix, own_size) = match type_def {
            Type::Primitive(name) => (
                Some(name.clone()),
                String::new(),
                self.layout_engine.size_of(type_def),
            ),
            Type::Generic {
                name,
                args: type_args,
            } => (
                Some(name.clone()),
                super::generic_suffix(type_args),
                self.layout_engine.size_of(&Type::Primitive(name.clone())),
            ),
            _ => (None, String::new(), self.layout_engine.size_of(type_def)),
        };

        if let Some(class_name) = class_name {
            let tc = self.type_checker();
            if !tc.loaded_interfaces.contains(&class_name) {
                if let Some(ctor) = tc
                    .loaded_classes
                    .get(&class_name)
                    .and_then(|c| c.methods.iter().find(|m| m.name == "new"))
                {
                    let arg_exprs: Vec<String> =
                        args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                    let thunk_key = if suffix.is_empty() {
                        format!("__new_{}", class_name)
                    } else {
                        format!("__new_{}_{}", class_name, suffix)
                    };
                    let ctor_fn_name = if suffix.is_empty() {
                        format!("{}_new", class_name)
                    } else {
                        format!("{}_{}_new", class_name, suffix)
                    };
                    let ctor_params: Vec<String> =
                        ctor.parameters.iter().map(|p| p.name.clone()).collect();
                    self.ensure_helper(&thunk_key, || {
                        let mut body = String::new();
                        body.push_str(&format!(
                            "function {}({}) -> selfp {{\n",
                            thunk_key,
                            ctor_params.join(", ")
                        ));
                        body.push_str(&format!("    selfp := allocate_memory({})\n", own_size));
                        let mut ctor_call_args = vec!["selfp".to_string()];
                        ctor_call_args.extend(ctor_params.iter().cloned());
                        body.push_str(&format!(
                            "    {}({})\n",
                            ctor_fn_name,
                            ctor_call_args.join(", ")
                        ));
                        body.push_str("}\n");
                        body
                    });
                    return format!("{}({})", thunk_key, arg_exprs.join(", "));
                }
            }
        }
        format!("allocate_memory({})", self.layout_engine.size_of(type_def))
    }

    fn binary_op_meta(&self, left: &Expr, right: &Expr, ctx: &Ctx) -> Option<(usize, bool)> {
        if let Type::Primitive(name) = self.static_type(left, ctx) {
            if let Some(m) = numeric_meta(&name) {
                return Some(m);
            }
        }
        if let Type::Primitive(name) = self.static_type(right, ctx) {
            if let Some(m) = numeric_meta(&name) {
                return Some(m);
            }
        }
        None
    }

    fn max_value_hex(bits: usize) -> String {
        if bits == 256 {
            "not(0)".to_string()
        } else {
            mask_hex(bits / 8)
        }
    }

    // The largest value of a signed type, as a full word: 0x7f..ff for i256, 0x00..007f for i8.
    fn max_signed_hex(bits: usize) -> String {
        let n = bits / 8;
        format!("0x{}{}{}", "00".repeat(32 - n), "7f", "ff".repeat(n - 1))
    }

    // The smallest value of a signed type, as its two's complement in a full word: 0x80..00 for i256, 0xff..ff80 for i8.
    fn min_signed_hex(bits: usize) -> String {
        let n = bits / 8;
        format!("0x{}{}{}", "ff".repeat(32 - n), "80", "00".repeat(n - 1))
    }

    fn const_fold(
        &self,
        left: &Expr,
        right: &Expr,
        op: &str,
        meta: Option<(usize, bool)>,
    ) -> Option<String> {
        let (bits, signed) = meta?;
        if signed {
            return None;
        }
        let a = literal_u128(left)?;
        let b = literal_u128(right)?;

        let result: u128 = match op {
            "+" => a.checked_add(b)?,
            "-" => a.checked_sub(b)?, // None on underflow → keep checked_sub (which reverts)
            "*" => a.checked_mul(b)?,
            _ => return None,
        };

        let fits = if bits >= 128 {
            true
        } else {
            result <= ((1u128 << bits) - 1)
        };
        if !fits {
            return None;
        }

        Some(result.to_string())
    }

    fn translate_binary_op(&self, left: &Expr, operator: &str, right: &Expr, ctx: &Ctx) -> String {
        let l = self.translate_expr(left, ctx);
        let r = self.translate_expr(right, ctx);

        if matches!(operator, "==" | "!=")
            && is_str_type(&self.static_type(left, ctx))
            && is_str_type(&self.static_type(right, ctx))
        {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
            self.ensure_helper("gum_str_eq", gum_str_eq_helper_src);
            let eq = format!("gum_str_eq({}, {})", l, r);
            return if operator == "==" {
                eq
            } else {
                format!("iszero({})", eq)
            };
        }

        let meta = self.binary_op_meta(left, right, ctx);
        let signed = meta.map(|(_, s)| s).unwrap_or(false);

        if matches!(operator, "+" | "-" | "*") {
            if let Some(folded) = self.const_fold(left, right, operator, meta) {
                return folded;
            }
        }

        let rich = self.rich_reverts;

        // Only  and / move a WAD scale, and only when both sides carry one: two WAD values multiplied are WAD-squared, and divided they are unscaled.
        if matches!(operator, "*" | "/")
            && is_fixed_point(&self.static_type(left, ctx))
            && is_fixed_point(&self.static_type(right, ctx))
        {
            self.ensure_helper("gum_muldiv", || gum_muldiv_helper_src(rich));
            let (lo, hi) = (Self::min_signed_hex(256), Self::max_signed_hex(256));
            if operator == "*" {
                self.ensure_helper("gum_wad_mul", || gum_wad_mul_helper_src(rich));
                return format!("gum_wad_mul({}, {}, {}, {})", l, r, lo, hi);
            }
            self.ensure_helper("gum_wad_div", || gum_wad_div_helper_src(rich));
            return format!("gum_wad_div({}, {}, {}, {})", l, r, lo, hi);
        }
        match operator {
            "+" => match meta {
                Some((bits, true)) => {
                    self.ensure_helper("checked_sadd", || checked_sadd_helper_src(rich));
                    format!(
                        "checked_sadd({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                Some((bits, false)) => {
                    self.ensure_helper("checked_add", || checked_add_helper_src(rich));
                    format!("checked_add({}, {}, {})", l, r, Self::max_value_hex(bits))
                }
                None => format!("add({}, {})", l, r),
            },
            "-" => match meta {
                Some((256, true)) => {
                    self.ensure_helper("checked_ssub", || checked_ssub_helper_src(rich));
                    format!("checked_ssub({}, {})", l, r)
                }
                Some((bits, true)) => {
                    self.ensure_helper("checked_ssub", || checked_ssub_helper_src(rich));
                    self.ensure_helper("checked_ssub_n", || checked_ssub_n_helper_src(rich));
                    format!(
                        "checked_ssub_n({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                _ => {
                    self.ensure_helper("checked_sub", || checked_sub_helper_src(rich));
                    format!("checked_sub({}, {})", l, r)
                }
            },
            "*" => match meta {
                Some((bits, true)) => {
                    self.ensure_helper("checked_smul", || checked_smul_helper_src(rich));
                    format!(
                        "checked_smul({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                Some((bits, false)) => {
                    self.ensure_helper("checked_mul", || checked_mul_helper_src(rich));
                    format!("checked_mul({}, {}, {})", l, r, Self::max_value_hex(bits))
                }
                None => format!("mul({}, {})", l, r),
            },
            "/" => {
                if signed {
                    self.ensure_helper("checked_sdiv", || checked_sdiv_helper_src(rich));
                    format!("checked_sdiv({}, {})", l, r)
                } else {
                    self.ensure_helper("checked_div", || checked_div_helper_src(rich));
                    format!("checked_div({}, {})", l, r)
                }
            }
            "%" => {
                if signed {
                    self.ensure_helper("checked_smod", || checked_smod_helper_src(rich));
                    format!("checked_smod({}, {})", l, r)
                } else {
                    self.ensure_helper("checked_mod", || checked_mod_helper_src(rich));
                    format!("checked_mod({}, {})", l, r)
                }
            }
            "**" => format!("exp({}, {})", l, r),
            "==" => format!("eq({}, {})", l, r),
            "!=" => format!("iszero(eq({}, {}))", l, r),
            "<" => {
                if signed {
                    format!("slt({}, {})", l, r)
                } else {
                    format!("lt({}, {})", l, r)
                }
            }
            ">" => {
                if signed {
                    format!("sgt({}, {})", l, r)
                } else {
                    format!("gt({}, {})", l, r)
                }
            }
            "<=" => {
                if signed {
                    format!("iszero(sgt({}, {}))", l, r)
                } else {
                    format!("iszero(gt({}, {}))", l, r)
                }
            }
            ">=" => {
                if signed {
                    format!("iszero(slt({}, {}))", l, r)
                } else {
                    format!("iszero(lt({}, {}))", l, r)
                }
            }
            "&&" => format!("and({}, {})", l, r),
            "||" => format!("or({}, {})", l, r),
            _ => format!("/* unsupported op {} */", operator),
        }
    }

    fn hashmap_field_slot(&self, base: &Expr, ctx: &Ctx) -> Option<usize> {
        if let Expr::PropertyAccess {
            base: inner,
            property,
        } = base
        {
            if let Expr::Identifier(name) = &**inner {
                if name == "self" {
                    let self_ctx = ctx.self_ctx?;
                    if self_ctx.is_global {
                        return self
                            .layout_engine
                            .storage_field(&self_ctx.class_name, property)
                            .map(|sf| sf.slot);
                    }
                    return None;
                }
                return self
                    .layout_engine
                    .storage_field(name, property)
                    .map(|sf| sf.slot);
            }
        }
        None
    }

    fn hashmap_transient(&self, base: &Expr, ctx: &Ctx) -> bool {
        if let Expr::PropertyAccess {
            base: inner,
            property,
        } = base
        {
            if let Expr::Identifier(name) = &**inner {
                let class = if name == "self" {
                    match ctx.self_ctx {
                        Some(sc) if sc.is_global => sc.class_name.clone(),
                        _ => return false,
                    }
                } else {
                    name.clone()
                };
                return self
                    .layout_engine
                    .storage_field(&class, property)
                    .map(|sf| sf.is_transient)
                    .unwrap_or(false);
            }
        }
        match base {
            Expr::MethodCall {
                base: inner,
                method,
                args,
            } if method == "get" && !args.is_empty() => self.hashmap_transient(inner, ctx),
            Expr::IndexAccess { base: inner, .. } => self.hashmap_transient(inner, ctx),
            _ => false,
        }
    }

    fn hashmap_base_slot_expr(&self, base: &Expr, ctx: &Ctx) -> Option<String> {
        if let Some(slot) = self.hashmap_field_slot(base, ctx) {
            return Some(slot.to_string());
        }
        let (inner, key): (&Expr, &Expr) = match base {
            Expr::MethodCall {
                base: inner,
                method,
                args,
            } if method == "get" && !args.is_empty() => (inner, &args[0]),
            Expr::IndexAccess { base: inner, index } => (inner, index),
            _ => return None,
        };
        if let Type::Generic { name, args: targs } = self.static_type(inner, ctx) {
            if name == "HashMap" && targs.len() == 2 {
                let inner_slot = self.hashmap_base_slot_expr(inner, ctx)?;
                let key_expr = self.translate_expr(key, ctx);
                return Some(format!("gum_hash_slot({}, {})", key_expr, inner_slot));
            }
        }
        None
    }

    fn resolve_storage_field(&self, base: &Expr, ctx: &Ctx) -> Option<StorageField> {
        if let Expr::PropertyAccess {
            base: inner,
            property,
        } = base
        {
            if let Expr::Identifier(name) = &**inner {
                if name == "self" {
                    let sc = ctx.self_ctx?;
                    return if sc.is_global {
                        self.layout_engine.storage_field(&sc.class_name, property)
                    } else {
                        None
                    };
                }
                return self.layout_engine.storage_field(name, property);
            }
        }
        None
    }

    // A dynamic storage array read as a value copies out to memory, since a storage array cannot be held outside storage.
    fn storage_array_to_memory(&self, e: &Expr, ctx: &Ctx) -> Option<String> {
        let (slot, esz, tr) = self.dyn_storage_array(e, ctx)?;
        if self.storage_struct_array(e, ctx).is_some() {
            return None;
        }
        let esz = esz.max(1);
        let (per, es) = pack_params(esz);
        let name = format!("sarr_to_mem_{}_{}{}", esz, slot, kind_suffix(tr));
        self.ensure_helper("arr_data_base", arr_data_base_helper_src);
        self.ensure_pack_read(tr);
        let n = name.clone();
        self.ensure_helper(&name, || sarr_to_mem_helper_src(&n, esz, per, es, tr));
        Some(format!("{}({})", name, slot))
    }

    fn dyn_storage_array(&self, base: &Expr, ctx: &Ctx) -> Option<(usize, usize, bool)> {
        let sf = self.resolve_storage_field(base, ctx)?;
        if let Type::Array(inner) = self.static_type(base, ctx) {
            return Some((sf.slot, self.layout_engine.size_of(&inner), sf.is_transient));
        }
        None
    }

    fn storage_array_info(&self, base: &Expr, ctx: &Ctx) -> Option<(usize, usize, usize, bool)> {
        let sf = self.resolve_storage_field(base, ctx)?;
        if let Type::FixedArray(inner, n) = self.static_type(base, ctx) {
            return Some((
                sf.slot,
                self.layout_engine.size_of(&inner),
                n,
                sf.is_transient,
            ));
        }
        None
    }

    fn storage_array_get(
        &self,
        base_slot: usize,
        elem_size: usize,
        len: usize,
        index_expr: &str,
        tr: bool,
    ) -> String {
        let (per, es) = pack_params(elem_size);
        self.ensure_pack_read(tr);
        format!(
            "pk_get{}({}, {}, {}, {}, {}, {})",
            kind_suffix(tr),
            base_slot,
            index_expr,
            len,
            per,
            es,
            elem_size.max(1)
        )
    }

    fn storage_array_set(
        &self,
        base_slot: usize,
        elem_size: usize,
        len: usize,
        index_expr: &str,
        val: &str,
        tr: bool,
    ) -> String {
        let (per, es) = pack_params(elem_size);
        self.ensure_pack_write(tr);
        format!(
            "pk_set{}({}, {}, {}, {}, {}, {}, {})\n",
            kind_suffix(tr),
            base_slot,
            index_expr,
            len,
            per,
            es,
            elem_size.max(1),
            val
        )
    }

    fn dyn_array_get(
        &self,
        len_slot: usize,
        elem_size: usize,
        index_expr: &str,
        tr: bool,
    ) -> String {
        let (per, es) = pack_params(elem_size);
        self.ensure_helper("arr_data_base", arr_data_base_helper_src);
        self.ensure_pack_read(tr);
        format!(
            "pk_get{}(arr_data_base({}), {}, {}({}), {}, {}, {})",
            kind_suffix(tr),
            len_slot,
            index_expr,
            ld_op(tr),
            len_slot,
            per,
            es,
            elem_size.max(1)
        )
    }

    fn dyn_array_set(
        &self,
        len_slot: usize,
        elem_size: usize,
        index_expr: &str,
        val: &str,
        tr: bool,
    ) -> String {
        let (per, es) = pack_params(elem_size);
        self.ensure_helper("arr_data_base", arr_data_base_helper_src);
        self.ensure_pack_write(tr);
        format!(
            "pk_set{}(arr_data_base({}), {}, {}({}), {}, {}, {}, {})\n",
            kind_suffix(tr),
            len_slot,
            index_expr,
            ld_op(tr),
            len_slot,
            per,
            es,
            elem_size.max(1),
            val
        )
    }

    fn ensure_pack_read(&self, tr: bool) {
        let rich = self.rich_reverts;
        self.ensure_helper(&format!("pk_read{}", kind_suffix(tr)), || {
            pk_read_helper_src(tr)
        });
        self.ensure_helper(&format!("pk_get{}", kind_suffix(tr)), || {
            pk_get_helper_src(rich, tr)
        });
    }

    fn ensure_pack_write(&self, tr: bool) {
        let rich = self.rich_reverts;
        self.ensure_helper(&format!("pk_write{}", kind_suffix(tr)), || {
            pk_write_helper_src(tr)
        });
        self.ensure_helper(&format!("pk_set{}", kind_suffix(tr)), || {
            pk_set_helper_src(rich, tr)
        });
    }

    fn struct_base_transient(&self, base: &Expr, ctx: &Ctx) -> bool {
        // A struct held directly as a field carries its own transient flag, so it has to be asked here too or a transient struct would read and write persistent slots.
        if let Expr::PropertyAccess { base: owner, property } = base {
            let sf = self
                .field_owner(owner, ctx)
                .and_then(|c| self.layout_engine.storage_field(&c, property));
            if let Some(sf) = sf {
                return sf.is_transient;
            }
        }
        let map: &Expr = match base {
            Expr::IndexAccess { base: m, .. } => m,
            Expr::MethodCall {
                base: m,
                method,
                args,
            } if method == "get" && !args.is_empty() => m,
            _ => return false,
        };
        if let Some((.., tr)) = self.storage_struct_array(map, ctx) {
            return tr;
        }
        self.hashmap_transient(map, ctx)
    }

    fn storage_struct_array(
        &self,
        arr: &Expr,
        ctx: &Ctx,
    ) -> Option<(String, String, String, usize, bool)> {
        let sf = self.resolve_storage_field(arr, ctx)?;
        // No Vec case here on purpose: a contract's Vec(T) field is rewritten to [T] when the class is loaded, so codegen only ever sees the array form.
        let (inner, len_expr, data_base) = match self.static_type(arr, ctx) {
            Type::Array(inner) => {
                self.ensure_helper("arr_data_base", arr_data_base_helper_src);
                (
                    inner,
                    format!("{}({})", ld_op(sf.is_transient), sf.slot),
                    format!("arr_data_base({})", sf.slot),
                )
            }
            Type::FixedArray(inner, n) => (inner, n.to_string(), sf.slot.to_string()),
            _ => return None,
        };
        let struct_name = match &*inner {
            Type::Primitive(name) if is_struct_type(self.type_checker(), &inner) => name.clone(),
            _ => return None,
        };
        let es = struct_elem_slots(self.layout_engine.size_of(&inner));
        Some((data_base, len_expr, struct_name, es, sf.is_transient))
    }

    // The class owning a bare field reference, resolving self against the enclosing class.
    fn field_owner(&self, base: &Expr, ctx: &Ctx) -> Option<String> {
        match base {
            Expr::Identifier(n) if n == "self" => ctx.self_ctx.map(|s| s.class_name.clone()),
            Expr::Identifier(n) => Some(n.clone()),
            _ => None,
        }
    }

    fn struct_storage_base(&self, base: &Expr, ctx: &Ctx) -> Option<(String, String)> {
        // A struct held directly as a contract field, C.p, so C.p.b resolves to p's base slot plus b's offset within the struct.
        if let Expr::PropertyAccess { base: owner, property } = base {
            let sf = self
                .field_owner(owner, ctx)
                .and_then(|c| self.layout_engine.storage_field(&c, property));
            let ty = self.static_type(base, ctx);
            if let (Some(sf), Type::Primitive(struct_name)) = (sf, &ty) {
                if is_struct_type(self.type_checker(), &ty) {
                    return Some((sf.slot.to_string(), struct_name.clone()));
                }
            }
        }
        // xs[i] and v.get(i) are the same access spelled two ways, so both reach the struct-array path; only the mapping form used to accept .get.
        let indexed: Option<(&Expr, &Expr)> = match base {
            Expr::IndexAccess { base: arr, index } => Some((arr, index)),
            Expr::MethodCall { base: arr, method, args } if method == "get" && !args.is_empty() => {
                Some((arr, &args[0]))
            }
            _ => None,
        };
        if let Some((arr, index)) = indexed {
            if let Some((data_base, len, struct_name, es, tr)) = self.storage_struct_array(arr, ctx)
            {
                self.ensure_helper("sarr_base", || sarr_base_helper_src(self.rich_reverts));
                let _ = tr;
                let idx = self.translate_expr(index, ctx);
                return Some((
                    format!("sarr_base({}, {}, {}, {})", data_base, idx, len, es),
                    struct_name,
                ));
            }
        }
        let (map, key): (&Expr, &Expr) = match base {
            Expr::IndexAccess { base: m, index } => (m, index),
            Expr::MethodCall {
                base: m,
                method,
                args,
            } if method == "get" && !args.is_empty() => (m, &args[0]),
            _ => return None,
        };
        if let Type::Generic { name, args: targs } = self.static_type(map, ctx) {
            if name == "HashMap" && targs.len() == 2 {
                if let Type::Primitive(struct_name) = &targs[1] {
                    if self.type_checker().loaded_classes.contains_key(struct_name) {
                        let map_base = self.hashmap_base_slot_expr(map, ctx)?;
                        let key_expr = self.translate_expr(key, ctx);
                        return Some((
                            format!("gum_hash_slot({}, {})", key_expr, map_base),
                            struct_name.clone(),
                        ));
                    }
                }
            }
        }
        None
    }

    fn struct_field_slot(
        &self,
        base_slot: &str,
        struct_name: &str,
        property: &str,
    ) -> Option<(String, usize, usize)> {
        let sf = self
            .layout_engine
            .struct_storage_field(struct_name, property)?;
        let slot = if sf.slot == 0 {
            base_slot.to_string()
        } else {
            format!("add({}, {})", base_slot, sf.slot)
        };
        Some((slot, sf.offset_in_slot, sf.size))
    }

    // The revert data of a custom error is an ABI argument list like any other, so it shares the CREATE and interface-call encoder rather than laying out its own head and tail.
    fn emit_revert_data(
        &self,
        label: &str,
        selector: &str,
        args: &[Expr],
        types: &[Type],
        ctx: &Ctx,
    ) -> String {
        let mut out = format!("// {}\n", label);

        // One string keeps its own helper: it is the assert(msg) shape and by far the most common revert, so it is worth the bytes it saves.
        if args.len() == 1 && types.first().map(is_str_type).unwrap_or(false) {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
            self.ensure_helper("gum_revert_str", gum_revert_str_helper_src);
            let arg = self.translate_expr(&args[0], ctx);
            out.push_str(&format!(
                "gum_revert_str(shl(224, {}), {})\n",
                selector, arg
            ));
            return out;
        }

        if args.is_empty() {
            out.push_str("{\n");
            out.push_str("let _p := mload(0x40)\n");
            out.push_str(&format!("mstore(_p, shl(224, {}))\n", selector));
            out.push_str("revert(_p, 4)\n");
            out.push_str("}\n");
            return out;
        }

        let (size_src, write_src) = self.abi_arg_blob_src(types);
        out.push_str("{\n");
        for (i, arg) in args.iter().enumerate() {
            let arg_expr = self.translate_expr(arg, ctx);
            out.push_str(&format!("let a{} := {}\n", i, arg_expr));
        }
        out.push_str(&size_src);
        // The selector sits immediately before the blob, so one allocation covers both and revert can hand back a contiguous range.
        out.push_str("let _p := allocate_memory(add(4, alen))\n");
        out.push_str(&format!("mstore(_p, shl(224, {}))\n", selector));
        out.push_str("let blob := add(_p, 4)\n");
        out.push_str(&write_src);
        out.push_str("revert(_p, add(4, alen))\n");
        out.push_str("}\n");
        out
    }

    fn assert_failure_data(&self, msg: &Expr, ctx: &Ctx) -> String {
        let enum_call = match msg {
            Expr::MethodCall { base, method, args } => Some((base, method.as_str(), args.as_slice())),
            Expr::PropertyAccess { base, property } => Some((base, property.as_str(), &[] as &[Expr])),
            _ => None,
        };
        if let Some((base, method, args)) = enum_call {
            if let Expr::Identifier(enum_name) = &**base {
                if let Some(enum_decl) = self.type_checker().loaded_enums.get(enum_name) {
                    if let Some(variant) = enum_decl.variants.iter().find(|v| v.name == method) {
                        let abi_gen = AbiGenerator::new(self.type_checker());
                        let selector = abi_gen.calculate_error_selector(variant);
                        let types: Vec<Type> = variant.parameters.iter().map(|p| p.type_def.clone()).collect();
                        return self.emit_revert_data(
                            &format!("assert failed: {}", method),
                            &selector,
                            args,
                            &types,
                            ctx,
                        );
                    }
                }
            }
        }
        // A bare assert message is Error(string), the selector every tool already knows.
        self.emit_revert_data(
            "assert failed",
            "0x08c379a0",
            std::slice::from_ref(msg),
            &[Type::Primitive("String".to_string())],
            ctx,
        )
    }

    fn translate_super_call(&self, method: &str, args: &[Expr], ctx: &Ctx) -> String {
        let class_name = match ctx.self_ctx {
            Some(s) => s.class_name.clone(),
            None => return String::new(),
        };
        let name = format!("{}_{}", class_name, super_name(method));
        let mut all: Vec<String> = Vec::new();
        if !self
            .type_checker()
            .loaded_classes
            .get(&class_name)
            .map(|c| c.is_global)
            .unwrap_or(false)
        {
            all.push("self".to_string());
        }
        all.extend(args.iter().map(|a| self.translate_expr(a, ctx)));
        format!("{}({})", name, all.join(", "))
    }

    fn translate_method_call(&self, base: &Expr, method: &str, args: &[Expr], ctx: &Ctx) -> String {
        if matches!(base, Expr::Identifier(b) if b == "super") {
            return self.translate_super_call(method, args, ctx);
        }
        // Child.Ancestor.method(): call the ancestor's retained copy, which the flattening emitted in the owner's namespace and bound to the owner's storage.
        if let Expr::PropertyAccess { base: inner, property: ancestor } = base {
            if let Type::Primitive(owner) = self.static_type(inner, ctx) {
                let is_ancestor = self
                    .type_checker()
                    .loaded_classes
                    .get(&owner)
                    .map(|c| c.parents.iter().any(|p| p == ancestor))
                    .unwrap_or(false);
                if is_ancestor && self.type_checker().loaded_classes.contains_key(ancestor) {
                    let name = format!("{}_{}", owner, crate::semantic::qualified_method_name(ancestor, method));
                    let mut all: Vec<String> = Vec::new();
                    if !self
                        .type_checker()
                        .loaded_classes
                        .get(&owner)
                        .map(|c| c.is_global)
                        .unwrap_or(false)
                    {
                        all.push(self.translate_expr(inner, ctx));
                    }
                    all.extend(args.iter().map(|a| self.translate_expr(a, ctx)));
                    return format!("{}({})", name, all.join(", "));
                }
            }
        }
        if let Type::Primitive(name) = self.static_type(base, ctx) {
            if is_numeric_primitive(&name) {
                match method {
                    "saturate" => return self.translate_saturate(base, ctx),
                    "as_bytes" | "as_bits" => return self.translate_as_bytes(base, ctx),
                    "to_string" if name.starts_with('u') => return self.translate_to_string(base, ctx),
                    _ => {}
                }
            }
        }

        if is_str_type(&self.static_type(base, ctx)) {
            let self_expr = self.translate_expr(base, ctx);
            match method {
                "concat" if args.len() == 1 => {
                    self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                    self.ensure_helper("gum_str_concat", gum_str_concat_helper_src);
                    let other = self.translate_expr(&args[0], ctx);
                    return format!("gum_str_concat({}, {})", self_expr, other);
                }
                "slice" if args.len() == 2 => {
                    let rich = self.rich_reverts;
                    self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                    self.ensure_helper("gum_str_slice", || gum_str_slice_helper_src(rich));
                    let s = self.translate_expr(&args[0], ctx);
                    let e = self.translate_expr(&args[1], ctx);
                    return format!("gum_str_slice({}, {}, {})", self_expr, s, e);
                }
                _ => {}
            }
        }

        if let Type::Primitive(class_name) = self.static_type(base, ctx) {
            if class_name == "AccountCode" {
                if method == "len" {
                    let self_expr = self.translate_expr(base, ctx);
                    return format!("extcodesize({})", self_expr);
                }
            }
            if class_name == "Account" {
                let self_expr = self.translate_expr(base, ctx);
                match method {
                    "balance" => return format!("balance({})", self_expr),

                    "pay" if args.len() == 1 => {
                        self.ensure_helper("gum_pay", gum_pay_helper_src);
                        let amt = self.translate_expr(&args[0], ctx);
                        return format!("gum_pay({}, {})", self_expr, amt);
                    }
                    "transfer" if args.len() == 1 => {
                        self.ensure_helper("gum_transfer", gum_transfer_helper_src);
                        let amt = self.translate_expr(&args[0], ctx);
                        return format!("gum_transfer({}, {})", self_expr, amt);
                    }
                    "delegated_to" if args.is_empty() => {
                        self.ensure_helper("gum_delegate_of", gum_delegate_of_helper_src);
                        return format!("gum_delegate_of({})", self_expr);
                    }
                    "is_delegated" if args.is_empty() => {
                        self.ensure_helper("gum_delegate_of", gum_delegate_of_helper_src);
                        return format!("iszero(iszero(gum_delegate_of({})))", self_expr);
                    }
                    _ => {}
                }
            }
        }

        if let Expr::Identifier(ns) = base {
            if ns == "Account" {
                let a: Vec<String> = args.iter().map(|x| self.translate_expr(x, ctx)).collect();
                match method {
                    "create" if a.len() == 2 => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);
                        self.ensure_helper("gum_create", gum_create_helper_src);
                        return format!("gum_create({}, {})", a[0], a[1]);
                    }
                    "create2" if a.len() == 3 => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);
                        self.ensure_helper("gum_create2", gum_create2_helper_src);
                        return format!("gum_create2({}, {}, {})", a[0], a[1], a[2]);
                    }
                    "create2_address" if a.len() == 2 => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_create2_address", gum_create2_address_helper_src);
                        return format!("gum_create2_address({}, {})", a[0], a[1]);
                    }
                    _ => {}
                }
            }
        }

        if let Expr::Identifier(ns) = base {
            if ns == "Crypto" && method == "verify_p256" && args.len() == 5 {
                self.ensure_helper("gum_p256_verify", gum_p256_verify_helper_src);
                let a: Vec<String> = args.iter().map(|x| self.translate_expr(x, ctx)).collect();
                return format!("gum_p256_verify({})", a.join(", "));
            }
        }

        if matches!(method, "len" | "get") {
            if let Some((slot, elem_size, tr)) = self.dyn_storage_array(base, ctx) {
                if method == "len" && args.is_empty() {
                    return format!("{}({})", ld_op(tr), slot);
                }
                if method == "get" && args.len() == 1 {
                    let idx = self.translate_expr(&args[0], ctx);
                    return self.dyn_array_get(slot, elem_size, &idx, tr);
                }
            }
        }

        if matches!(method, "push" | "pop") {
            if let Some((_, _, struct_name, es, tr)) = self
                .dyn_storage_array(base, ctx)
                .and_then(|_| self.storage_struct_array(base, ctx))
            {
                let k = kind_suffix(tr);
                self.ensure_helper("arr_data_base", arr_data_base_helper_src);
                if method == "push" {
                    if !args.is_empty() {
                        self.errors.borrow_mut().push(format!(
                            "Semantic Error: push on an array of struct '{}' takes no argument. gum has no struct-copy: a struct lives either in memory or in storage, and moving one between them field by field is not something push can do implicitly. Use arr.push() to append a zeroed element, then set its fields (arr[arr.length - 1].field = v) the same way a struct in a mapping is written.",
                            struct_name
                        ));
                        return String::new();
                    }
                    let ld = ld_op(tr);
                    let st = st_op(tr);
                    let slot = self
                        .resolve_storage_field(base, ctx)
                        .map(|f| f.slot)
                        .unwrap_or(0);
                    return format!("{}({}, add({}({}), 1))\n", st, slot, ld, slot);
                }
                let rich = self.rich_reverts;
                self.ensure_helper(&format!("dsarr_pop{}", k), || {
                    dsarr_pop_helper_src(rich, tr)
                });
                let slot = self
                    .resolve_storage_field(base, ctx)
                    .map(|f| f.slot)
                    .unwrap_or(0);
                return format!("dsarr_pop{}({}, {})\n", k, slot, es);
            }
            if let Some((slot, elem_size, tr)) = self.dyn_storage_array(base, ctx) {
                let (per, es) = pack_params(elem_size);
                let esz = elem_size.max(1);
                let rich = self.rich_reverts;
                self.ensure_helper("arr_data_base", arr_data_base_helper_src);
                self.ensure_helper(&format!("pk_write{}", kind_suffix(tr)), || {
                    pk_write_helper_src(tr)
                });
                if method == "push" {
                    let v = self.translate_expr(&args[0], ctx);
                    self.ensure_helper(&format!("dpk_push{}", kind_suffix(tr)), || {
                        dpk_push_helper_src(tr)
                    });
                    return format!(
                        "dpk_push{}({}, {}, {}, {}, {})\n",
                        kind_suffix(tr),
                        slot,
                        per,
                        es,
                        esz,
                        v
                    );
                } else {
                    self.ensure_helper(&format!("dpk_pop{}", kind_suffix(tr)), || {
                        dpk_pop_helper_src(rich, tr)
                    });
                    return format!(
                        "dpk_pop{}({}, {}, {}, {})\n",
                        kind_suffix(tr),
                        slot,
                        per,
                        es,
                        esz
                    );
                }
            }
        }

        if matches!(method, "get" | "set") {
            if let Type::Generic {
                name,
                args: type_args,
            } = self.static_type(base, ctx)
            {
                if name == "HashMap" && type_args.len() == 2 {
                    if let Some(base_slot) = self.hashmap_base_slot_expr(base, ctx) {
                        let key_expr = self.translate_expr(&args[0], ctx);
                        let slot = format!("gum_hash_slot({}, {})", key_expr, base_slot);
                        let value_is_map = matches!(&type_args[1], Type::Generic { name, .. } if name == "HashMap");
                        let tr = self.hashmap_transient(base, ctx);
                        if method == "get" {
                            return if value_is_map {
                                slot
                            } else {
                                format!("{}({})", ld_op(tr), slot)
                            };
                        } else if let Some(value_arg) = args.get(1) {
                            let val_expr = self.translate_expr(value_arg, ctx);
                            return format!("{}({}, {})", st_op(tr), slot, val_expr);
                        }
                    }
                }
            }
        }

        if let Type::Generic {
            name: class_name,
            args: type_args,
        } = self.static_type(base, ctx)
        {
            if let Some(class_decl) = self.type_checker().loaded_classes.get(&class_name) {
                let suffix = super::generic_suffix(&type_args);
                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                if class_decl.is_global {
                    return format!(
                        "{}_{}_{}({})",
                        class_name,
                        suffix,
                        method,
                        arg_strs.join(", ")
                    );
                } else {
                    let self_expr = self.translate_expr(base, ctx);
                    let mut all = vec![self_expr];
                    all.extend(arg_strs);
                    return format!("{}_{}_{}({})", class_name, suffix, method, all.join(", "));
                }
            }
        }

        if let Expr::Identifier(enum_name) = base {
            if let Some(enum_decl) = self.type_checker().loaded_enums.get(enum_name) {
                if let Some(idx) = enum_decl.variants.iter().position(|v| v.name == method) {
                    // Same rule as the bare S.A form above: a payload-free enum is its tag, so it needs no allocation and no pointer.
                    if !self.type_checker().enum_has_payload(enum_name) {
                        return idx.to_string();
                    }
                    let payload_expr = args
                        .first()
                        .map(|a| self.translate_expr(a, ctx))
                        .unwrap_or_else(|| "0".to_string());
                    self.ensure_helper("make_enum", make_enum_helper_src);
                    return format!("make_enum({}, {})", idx, payload_expr);
                }
            }
        }

        if let Expr::FnCall {
            name: iface_name,
            args: cast_args,
        } = base
        {
            if self.type_checker().loaded_interfaces.contains(iface_name) && cast_args.len() == 1 {
                // The interface target (cast_args[0]) must lead the arg list:
                return self.extcall_wrapper_src(iface_name, method, &std::iter::once(cast_args[0].clone()).chain(args.iter().cloned()).collect::<Vec<_>>(), ctx);
            }
        }

        if let Type::Primitive(class_name) = self.static_type(base, ctx) {
            if self.type_checker().loaded_classes.contains_key(&class_name)
                && !self.type_checker().loaded_interfaces.contains(&class_name)
            {
                let class_decl = &self.type_checker().loaded_classes[&class_name];
                if method == "serialize" && class_decl.parents.iter().any(|p| p == "Serializable") {
                    let self_expr = self.translate_expr(base, ctx);
                    return format!("{}_serialize({})", class_name, self_expr);
                }
                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                let is_global = class_decl.is_global;
                let bare_class = matches!(base, Expr::Identifier(n) if self.type_checker().loaded_classes.contains_key(n));
                if is_global {
                    return format!("{}_{}({})", class_name, method, arg_strs.join(", "));
                }
                // Message and Block are frame namespaces whose methods compile to opcodes (caller, timestamp) and take no self, so a bare-name call is exactly right for them.
                if bare_class && (class_name == "Message" || class_name == "Block") {
                    return format!("{}_{}({})", class_name, method, arg_strs.join(", "));
                }
                // Any other bare class name is not an instance, so its method has no self to run on. This used to emit a call with the self argument dropped, which solc rejected.
                if bare_class {
                    self.errors.borrow_mut().push(format!(
                        "'{}.{}()' has no receiver. Call a parent's version as Child.{}.{}(), or call the method on an instance.",
                        class_name, method, class_name, method
                    ));
                    return String::new();
                }
                let self_expr = self.translate_expr(base, ctx);
                let mut all = vec![self_expr];
                all.extend(arg_strs);
                return format!("{}_{}({})", class_name, method, all.join(", "));
            }
        }

        let b = self.translate_expr(base, ctx);
        let arg_strs: Vec<String> = args.iter().map(|a| self.translate_expr(a, ctx)).collect();
        if arg_strs.is_empty() {
            format!("{}_{}()", b, method)
        } else {
            format!("{}_{}({})", b, method, arg_strs.join(", "))
        }
    }

    // How many bytes a fresh local of this type needs, or None for a scalar, which is just a value.
    fn fresh_local_bytes(&self, t: &Type) -> Option<usize> {
        match t {
            Type::FixedArray(..) | Type::Array(_) => Some(self.layout_engine.size_of(t)),
            Type::Primitive(n) if n == "String" || n == "Bytes" => Some(32),
            Type::Primitive(_)
                if is_struct_type(self.type_checker(), t)
                    || self.type_checker().is_payload_enum(t) =>
            {
                Some(self.layout_engine.size_of(t))
            }
            _ => None,
        }
    }

    // Whether a value of this type sits inline where it is stored rather than being a pointer to somewhere else.
    fn elem_is_inline(&self, t: &Type) -> bool {
        matches!(t, Type::FixedArray(..)) || is_struct_type(self.type_checker(), t)
    }

    fn array_elem_info(&self, base: &Expr, ctx: &Ctx) -> (bool, usize) {
        match self.static_type(base, ctx) {
            Type::Array(inner) => (true, self.layout_engine.size_of(&inner)),
            Type::FixedArray(inner, _) => (false, self.layout_engine.size_of(&inner)),
            _ => (false, 32),
        }
    }

    fn mem_array_addr(&self, base: &Expr, index_expr: &str, ctx: &Ctx) -> (String, usize) {
        let (is_dynamic, stride) = self.array_elem_info(base, ctx);
        let b = self.translate_expr(base, ctx);
        let rich = self.rich_reverts;
        if is_dynamic {
            self.ensure_helper("gum_marr_addr", || gum_marr_addr_helper_src(rich));
            return (
                format!("gum_marr_addr({}, {}, {})", b, index_expr, stride),
                stride,
            );
        }
        if let Type::FixedArray(_, n) = self.static_type(base, ctx) {
            self.ensure_helper("gum_farr_addr", || gum_farr_addr_helper_src(rich));
            return (
                format!("gum_farr_addr({}, {}, {}, {})", b, index_expr, n, stride),
                stride,
            );
        }
        (
            format!("add({}, mul({}, {}))", b, index_expr, stride),
            stride,
        )
    }

    fn translate_for_loop(
        &self,
        iterator: &str,
        iterable: &Expr,
        body: &[Spanned<Statement>],
        ctx: &Ctx,
    ) -> String {
        if let Some((len_slot, elem_size, tr)) = self.dyn_storage_array(iterable, ctx) {
            self.ensure_helper("arr_data_base", arr_data_base_helper_src);
            return self.storage_for_loop(
                iterator,
                &format!("arr_data_base({})", len_slot),
                &format!("{}({})", ld_op(tr), len_slot),
                elem_size,
                self.elem_type_of(iterable, ctx),
                body,
                tr,
                ctx,
            );
        }
        if let Some((base_slot, elem_size, n, tr)) = self.storage_array_info(iterable, ctx) {
            return self.storage_for_loop(
                iterator,
                &base_slot.to_string(),
                &n.to_string(),
                elem_size,
                self.elem_type_of(iterable, ctx),
                body,
                tr,
                ctx,
            );
        }

        let iter_expr = self.translate_expr(iterable, ctx);
        let ty = self.static_type(iterable, ctx);
        let id = self.next_literal_id();
        let ptr_var = format!("__iter_ptr_{}", id);
        let len_var = format!("__iter_len_{}", id);
        let i_var = format!("__iter_i_{}", id);

        let (data_base, len_src, stride, elem_type) = match ty {
            Type::FixedArray(inner, n) => {
                let stride = self.layout_engine.size_of(&inner);
                (ptr_var.clone(), (n * stride).to_string(), stride, *inner)
            }
            Type::Array(inner) => {
                let stride = self.layout_engine.size_of(&inner);
                (
                    format!("add({}, 32)", ptr_var),
                    format!("mload({})", ptr_var),
                    stride,
                    *inner,
                )
            }
            _ => (
                ptr_var.clone(),
                "0".to_string(),
                32,
                Type::Primitive("u256".to_string()),
            ),
        };

        ctx.declare(iterator, &elem_type);

        let mut out = format!("let {} := {}\n", ptr_var, iter_expr);
        out.push_str(&format!("let {} := {}\n", len_var, len_src));
        out.push_str(&format!("let {} := 0\n", i_var));
        out.push_str(&format!(
            "for {{}} lt({}, {}) {{ {} := add({}, {}) }} {{\n",
            i_var, len_var, i_var, i_var, stride
        ));
        let elem_addr = format!("add({}, {})", data_base, i_var);
        // A struct element binds the loop variable to its address, matching how indexing yields one, since field access needs a pointer and not the element's first word.
        let read = if is_struct_type(self.type_checker(), &elem_type) {
            elem_addr.clone()
        } else {
            read_packed(&format!("mload({})", elem_addr), 0, stride)
        };
        out.push_str(&format!("    let {} := {}\n", iterator, read));
        for s in body {
            let stmt_out = self.translate_statement(&s.node, ctx);
            for line in stmt_out.lines() {
                out.push_str(&format!("    {}\n", line));
            }
        }
        out.push_str("}\n");
        out
    }

    fn resolve_storage_field_named(
        &self,
        e: &Expr,
        ctx: &Ctx,
    ) -> Option<(String, String, StorageField)> {
        if let Expr::PropertyAccess { base, property } = e {
            if let Expr::Identifier(name) = &**base {
                let class = if name == "self" {
                    let sc = ctx.self_ctx?;
                    if !sc.is_global {
                        return None;
                    }
                    sc.class_name.clone()
                } else {
                    name.clone()
                };
                let sf = self.layout_engine.storage_field(&class, property)?;
                return Some((class, property.clone(), sf));
            }
        }
        None
    }

    fn translate_delete(&self, target: &Expr, ctx: &Ctx) -> String {
        if let Some((len_slot, elem_size, tr)) = self.dyn_storage_array(target, ctx) {
            let (per, es) = pack_params(elem_size);
            self.ensure_helper("arr_data_base", arr_data_base_helper_src);
            self.ensure_helper(&format!("dpk_clear{}", kind_suffix(tr)), || {
                dpk_clear_helper_src(tr)
            });
            return format!(
                "dpk_clear{}({}, {}, {})\n",
                kind_suffix(tr),
                len_slot,
                per,
                es
            );
        }

        if let Some((class, property, sf)) = self.resolve_storage_field_named(target, ctx) {
            if self.field_is_str(&class, &property) {
                self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
                self.ensure_helper(
                    &format!("gum_sstr_clear{}", kind_suffix(sf.is_transient)),
                    || gum_sstr_clear_helper_src(sf.is_transient),
                );
                return format!(
                    "gum_sstr_clear{}({})\n",
                    kind_suffix(sf.is_transient),
                    sf.slot
                );
            }
        }

        if let Some((base_slot, elem_size, n, tr)) = self.storage_array_info(target, ctx) {
            let (per, es) = pack_params(elem_size);
            let slots = ((n + per - 1) / per) * es;
            let mut out = String::new();
            for i in 0..slots {
                out.push_str(&format!("{}({}, 0)\n", st_op(tr), base_slot + i));
            }
            return out;
        }

        if let Some((base_slot, struct_name)) = self.struct_storage_base(target, ctx) {
            if let Some(class) = self.type_checker().loaded_classes.get(&struct_name) {
                let mut slots: Vec<usize> = class
                    .fields
                    .iter()
                    .filter_map(|f| {
                        self.layout_engine
                            .struct_storage_field(&struct_name, &f.name)
                    })
                    .flat_map(|sf| {
                        let span = ((sf.offset_in_slot + sf.size + 31) / 32).max(1);
                        (0..span).map(move |i| sf.slot + i)
                    })
                    .collect();
                slots.sort_unstable();
                slots.dedup();
                if !slots.is_empty() {
                    let tr = self.struct_base_transient(target, ctx);
                    let bv = format!("__del_{}", self.next_literal_id());
                    let mut out = format!("let {} := {}\n", bv, base_slot);
                    for s in slots {
                        if s == 0 {
                            out.push_str(&format!("{}({}, 0)\n", st_op(tr), bv));
                        } else {
                            out.push_str(&format!("{}(add({}, {}), 0)\n", st_op(tr), bv, s));
                        }
                    }
                    return out;
                }
            }
        }

        // A memory-backed local is a pointer to a block, so deleting it clears the block. Assigning 0 nulled the pointer instead, and every later read of it went to scratch memory at address 0.
        if let Expr::Identifier(_) = target {
            let t = self.static_type(target, ctx);
            if let Some(bytes) = self.fresh_local_bytes(&t) {
                let p = self.translate_expr(target, ctx);
                let pv = format!("__delp_{}", self.next_literal_id());
                let mut out = format!("let {} := {}\n", pv, p);
                for i in 0..(bytes / 32) {
                    out.push_str(&format!("mstore(add({}, {}), 0)\n", pv, i * 32));
                }
                // The block need not end on a word boundary, and the bytes after it belong to the next allocation, so the tail word is a masked read-modify-write rather than a store.
                let rem = bytes % 32;
                if rem > 0 {
                    let addr = format!("add({}, {})", pv, (bytes / 32) * 32);
                    let merged = write_packed(&format!("mload({})", addr), 0, rem, "0");
                    out.push_str(&format!("mstore({}, {})\n", addr, merged));
                }
                return out;
            }
        }

        self.translate_statement(
            &Statement::Assignment {
                target: target.clone(),
                value: Expr::Number("0".to_string()),
            },
            ctx,
        )
    }

    fn elem_type_of(&self, iterable: &Expr, ctx: &Ctx) -> Type {
        match self.static_type(iterable, ctx) {
            Type::Array(inner) | Type::FixedArray(inner, _) => *inner,
            _ => Type::Primitive("u256".to_string()),
        }
    }

    fn storage_for_loop(
        &self,
        iterator: &str,
        base_expr: &str,
        len_expr: &str,
        elem_size: usize,
        elem_type: Type,
        body: &[Spanned<Statement>],
        tr: bool,
        ctx: &Ctx,
    ) -> String {
        let (per, es) = pack_params(elem_size);
        self.ensure_helper(&format!("pk_read{}", kind_suffix(tr)), || {
            pk_read_helper_src(tr)
        });
        ctx.declare(iterator, &elem_type);

        let id = self.next_literal_id();
        let base_var = format!("__iter_base_{}", id);
        let len_var = format!("__iter_len_{}", id);
        let i_var = format!("__iter_i_{}", id);

        let mut out = format!("let {} := {}\n", base_var, base_expr);
        out.push_str(&format!("let {} := {}\n", len_var, len_expr));
        out.push_str(&format!(
            "for {{ let {i} := 0 }} lt({i}, {len}) {{ {i} := add({i}, 1) }} {{\n",
            i = i_var,
            len = len_var
        ));
        out.push_str(&format!(
            "    let {} := pk_read{}({}, {}, {}, {}, {})\n",
            iterator,
            kind_suffix(tr),
            base_var,
            i_var,
            per,
            es,
            elem_size.max(1)
        ));
        for s in body {
            for line in self.translate_statement(&s.node, ctx).lines() {
                out.push_str(&format!("    {}\n", line));
            }
        }
        out.push_str("}\n");
        out
    }

    fn translate_saturate(&self, base: &Expr, ctx: &Ctx) -> String {
        if let Expr::BinaryOp {
            left,
            operator,
            right,
        } = base
        {
            if operator == "+" {
                let l = self.translate_expr(left, ctx);
                let r = self.translate_expr(right, ctx);
                self.ensure_helper("sat_add", || {
                    "function sat_add(a, b) -> r {\n    r := add(a, b)\n    if lt(r, a) { r := not(0) }\n}\n".to_string()
                });
                return format!("sat_add({}, {})", l, r);
            }
        }
        self.translate_expr(base, ctx)
    }

    fn translate_as_bytes(&self, base: &Expr, ctx: &Ctx) -> String {
        let val = self.translate_expr(base, ctx);
        self.ensure_helper("as_bytes_u256", || {
            "function as_bytes_u256(val) -> ptr {\n    ptr := allocate_memory(64)\n    mstore(ptr, 32)\n    mstore(add(ptr, 32), val)\n}\n".to_string()
        });
        format!("as_bytes_u256({})", val)
    }

    fn translate_to_string(&self, base: &Expr, ctx: &Ctx) -> String {
        let val = self.translate_expr(base, ctx);
        self.ensure_helper("gum_uint_to_str", gum_uint_to_str_helper_src);
        format!("gum_uint_to_str({})", val)
    }

    fn translate_string_literal(&self, s: &str) -> String {
        let fn_name = format!("__strlit_{}", self.next_literal_id());
        let mut body = String::new();
        body.push_str(&format!("function {}() -> ptr {{\n", fn_name));
        body.push_str(&str_literal_body_src("ptr", s));
        body.push_str("}\n");
        self.ensure_helper(&fn_name, || body);
        format!("{}()", fn_name)
    }

    fn translate_array_literal(
        &self,
        elements: &[Expr],
        elem_type_hint: Option<&Type>,
        ctx: &Ctx,
    ) -> String {
        if elements.is_empty() {
            return "allocate_memory(0)".to_string();
        }
        let elem_type = elem_type_hint
            .cloned()
            .unwrap_or_else(|| self.static_type(&elements[0], ctx));
        let stride = self.layout_engine.size_of(&elem_type);
        let elem_exprs: Vec<String> = elements
            .iter()
            .map(|e| self.translate_expr(e, ctx))
            .collect();
        let fn_name = format!("__arrlit_{}", self.next_literal_id());
        let params: Vec<String> = (0..elements.len()).map(|i| format!("v{}", i)).collect();

        let mut body = format!("function {}({}) -> ptr {{\n", fn_name, params.join(", "));
        body.push_str(&format!(
            "    ptr := allocate_memory({})\n",
            elements.len() * stride
        ));
        for (i, p) in params.iter().enumerate() {
            let addr = format!("add(ptr, {})", i * stride);
            if stride >= 32 {
                body.push_str(&format!("    mstore({}, {})\n", addr, p));
            } else {
                body.push_str(&format!(
                    "    mstore({}, {})\n",
                    addr,
                    write_packed(&format!("mload({})", addr), 0, stride, p)
                ));
            }
        }
        body.push_str("}\n");
        self.ensure_helper(&fn_name, || body);
        format!("{}({})", fn_name, elem_exprs.join(", "))
    }

    fn translate_fstring(&self, segments: &[FStringSegment], ctx: &Ctx) -> String {
        self.ensure_helper("u256_to_string", || {
            "function u256_to_string(val) -> ptr {\n\
             \x20   let count := 1\n\
             \x20   let tmp := val\n\
             \x20   for {} gt(tmp, 9) {} {\n\
             \x20       tmp := div(tmp, 10)\n\
             \x20       count := add(count, 1)\n\
             \x20   }\n\
             \x20   ptr := allocate_memory(add(32, count))\n\
             \x20   mstore(ptr, shl(192, count))\n\
             \x20   let i := count\n\
             \x20   let v := val\n\
             \x20   for {} gt(i, 0) {} {\n\
             \x20       i := sub(i, 1)\n\
             \x20       mstore8(add(add(ptr, 32), i), add(0x30, mod(v, 10)))\n\
             \x20       v := div(v, 10)\n\
             \x20   }\n\
             }\n"
            .to_string()
        });
        self.ensure_helper("bytes_copy", bytes_copy_helper_src);
        self.ensure_helper("gum_str_len", gum_str_len_helper_src);

        let interp_exprs: Vec<&Expr> = segments
            .iter()
            .filter_map(|s| match s {
                FStringSegment::Interp(e) => Some(e),
                FStringSegment::Literal(_) => None,
            })
            .collect();
        let arg_exprs: Vec<String> = interp_exprs
            .iter()
            .map(|e| self.translate_expr(e, ctx))
            .collect();
        let arg_is_bytes: Vec<bool> = interp_exprs
            .iter()
            .map(|e| {
                matches!(
                    self.static_type(e, ctx),
                    Type::Array(_) | Type::FixedArray(_, _)
                )
            })
            .collect();

        let fn_name = format!("__fstr_{}", self.next_literal_id());
        let params: Vec<String> = (0..arg_exprs.len()).map(|i| format!("v{}", i)).collect();

        let mut body = String::new();
        body.push_str(&format!(
            "function {}({}) -> ptr {{\n",
            fn_name,
            params.join(", ")
        ));

        let mut chunks: Vec<(String, bool)> = Vec::new();
        let mut interp_i = 0;
        for (i, seg) in segments.iter().enumerate() {
            match seg {
                FStringSegment::Literal(text) => {
                    let cv = format!("lit{}", i);
                    body.push_str(&format!("    let {} := 0\n", cv));
                    body.push_str(&str_literal_body_src(&cv, text));
                    chunks.push((cv, false));
                }
                FStringSegment::Interp(_) => {
                    let cv = format!("chunk{}", i);
                    let param = &params[interp_i];
                    let is_raw = arg_is_bytes[interp_i];
                    if is_raw {
                        body.push_str(&format!("    let {} := {}\n", cv, param));
                    } else {
                        body.push_str(&format!("    let {} := u256_to_string({})\n", cv, param));
                    }
                    chunks.push((cv, is_raw));
                    interp_i += 1;
                }
            }
        }

        let len_of = |cv: &str, is_raw: bool| {
            if is_raw {
                format!("mload({})", cv)
            } else {
                format!("gum_str_len({})", cv)
            }
        };

        body.push_str("    let total := 0\n");
        for (cv, is_raw) in &chunks {
            body.push_str(&format!(
                "    total := add(total, {})\n",
                len_of(cv, *is_raw)
            ));
        }
        body.push_str("    ptr := allocate_memory(add(32, total))\n");
        body.push_str("    mstore(ptr, shl(192, total))\n");
        body.push_str("    let off := 0\n");
        for (cv, is_raw) in &chunks {
            let l = len_of(cv, *is_raw);
            body.push_str(&format!(
                "    bytes_copy(add(add(ptr, 32), off), add({}, 32), {})\n",
                cv, l
            ));
            body.push_str(&format!("    off := add(off, {})\n", l));
        }
        body.push_str("}\n");

        self.ensure_helper(&fn_name, || body);
        format!("{}({})", fn_name, arg_exprs.join(", "))
    }

    fn ensure_helper(&self, name: &str, build: impl FnOnce() -> String) {
        let mut thunks = self.helper_thunks.borrow_mut();
        if !thunks.contains_key(name) {
            thunks.insert(name.to_string(), build());
        }
    }

    // An interface call is the same ABI layout problem as a CREATE, only the prefix differs: a 4-byte selector instead of the child's creation code, so it shares abi_arg_blob_src.
    // The parameter types come from the interface declaration rather than the argument expressions, because only the declaration says whether a value is a tuple, a string, or a scalar.
    // Output is taken via returndatacopy rather than a call-supplied buffer, since with dynamic args the arg blob is no longer conveniently at least a word wide.
    fn extcall_return_src(&self, ret_ty: &Option<Type>) -> String {
        let t = match ret_ty {
            Some(t) => t,
            None => return String::new(),
        };
        let scalar = format!(
            "    if lt(returndatasize(), 32) {{ revert(0, 0) }}\n    returndatacopy(0, 0, 32)\n    result := mload(0)\n"
        );
        let decode = |min: String, expr: String| -> String {
            let mut o = format!("    if lt(returndatasize(), {}) {{ revert(0, 0) }}\n", min);
            o.push_str("    let rd := allocate_memory(returndatasize())\n");
            o.push_str("    returndatacopy(rd, 0, returndatasize())\n");
            o.push_str(&format!("    result := {}\n", expr));
            o
        };

        // A returned enum arrives as its uint8 tag and has to be rebuilt into the [tag][payload] pair, or the caller would use the tag itself as a memory address.
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_mem", gum_abi_str_mem_helper_src);
            return decode(
                "32".to_string(),
                "gum_abi_str_mem(rd, mload(rd), returndatasize())".to_string(),
            );
        }
        // An array of any shape decodes through its own codec, reading returndata as the memory blob it already is.
        if matches!(t, Type::Array(_) | Type::FixedArray(..)) {
            if let Some(h) = self.ensure_abi_mem(t) {
                return if self.abi_is_dynamic(t) {
                    decode("32".to_string(), format!("{}(rd, mload(rd), returndatasize())", h))
                } else {
                    decode(
                        self.abi_head_bytes(t).to_string(),
                        format!("{}(rd, 0, returndatasize())", h),
                    )
                };
            }
        }
        match t {
            Type::Primitive(nm) if is_struct_type(self.type_checker(), t) => {
                if let Some((h, wire)) = self.ensure_abi_struct_mem(nm) {
                    return decode(wire.to_string(), format!("{}(rd, 0, returndatasize())", h));
                }
                scalar
            }
            _ => scalar,
        }
    }

    fn extcall_wrapper_src(
        &self,
        iface_name: &str,
        method: &str,
        args: &[Expr],
        ctx: &Ctx,
    ) -> String {
        let decl = self
            .type_checker()
            .loaded_classes
            .get(iface_name)
            .and_then(|c| c.methods.iter().find(|m| m.name == method));

        let arg_exprs: Vec<String> = args
            .iter()
            .map(|a| self.translate_expr(a, ctx))
            .collect();
        let target_expr = arg_exprs[0].clone();
        let arg_exprs = &arg_exprs[1..];

        let selector = decl
            .as_ref()
            .map(|m| AbiGenerator::new(self.type_checker()).calculate_selector(m))
            .unwrap_or_else(|| "0x00000000".to_string());

        let n = arg_exprs.len();
        // Falling back to plain words when the arity does not line up keeps a malformed call from panicking here; the type checker reports the real error.
        let types: Vec<Type> = decl
            .as_ref()
            .map(|m| m.parameters.iter().map(|p| p.type_def.clone()).collect::<Vec<_>>())
            .filter(|t: &Vec<Type>| t.len() == n)
            .unwrap_or_else(|| vec![Type::Primitive("u256".to_string()); n]);

        let ret_src = self.extcall_return_src(&decl.and_then(|m| m.return_type.clone()));
        let (size_src, write_src) = self.abi_arg_blob_src(&types);
        
        let is_try = ctx.try_ok_var.is_some();
        let fn_name = format!("__extcall_{}{}_{}", if is_try { "try_" } else { "" }, iface_name, method);
        
        if is_try {
            self.ensure_helper("gum_exception_helpers", gum_exception_helpers_src);
        }
        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);
        self.ensure_helper(&fn_name, || {
            let mut body = String::new();
            let params: Vec<String> = (0..n).map(|i| format!("a{}", i)).collect();
            let has_return = decl.and_then(|m| m.return_type.clone()).is_some();
            body.push_str(&format!(
                "function {}(target{}{}){} {{\n",
                fn_name,
                if params.is_empty() { "" } else { ", " },
                params.join(", "),
                if has_return { " -> result" } else { "" }
            ));
            body.push_str(&size_src);
            body.push_str("    let ptr := allocate_memory(add(4, alen))\n");
            body.push_str(&format!("    mstore(ptr, shl(224, {}))\n", selector));
            body.push_str("    let blob := add(ptr, 4)\n");
            body.push_str(&write_src);
            body.push_str("    let ok := call(gas(), target, 0, ptr, add(4, alen), 0, 0)\n");
            if is_try {
                body.push_str("    if iszero(ok) { gum_set_exception() leave }\n");
            } else {
                body.push_str("    if iszero(ok) { gum_bubble_revert() }\n");
            }
            body.push_str(&ret_src);
            body.push_str("}\n");
            body
        });

        let mut call = format!("{}({}", fn_name, target_expr);
        for a in arg_exprs {
            call.push_str(", ");
            call.push_str(a);
        }
        call.push(')');
        call
    }
}
