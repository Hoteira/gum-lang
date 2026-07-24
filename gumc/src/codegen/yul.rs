use crate::ast::*;
use crate::semantic::TypeChecker;
use tiny_keccak::{Hasher, Keccak};

pub(crate) fn type_bound_literal(bits: usize, signed: bool, method: &str) -> Option<String> {
    let nibbles = bits / 4;
    match (method, signed) {
        ("min", false) => Some("0".to_string()),
        ("max", false) => Some(format!("0x{}", "f".repeat(nibbles))),
        ("max", true) => Some(format!("0x7{}", "f".repeat(nibbles - 1))),
        ("min", true) => Some(format!("sub(0, 0x8{})", "0".repeat(nibbles - 1))),
        _ => None,
    }
}

pub(crate) fn panic_revert(rich: bool, code: &str) -> String {
    if rich {
        format!(
            "mstore(0, shl(224, 0x4e487b71)) mstore(4, {}) revert(0, 0x24)",
            code
        )
    } else {
        "revert(0, 0)".to_string()
    }
}

pub(crate) const PANIC_OVERFLOW: &str = "0x11";
pub(crate) const PANIC_DIV_ZERO: &str = "0x12";
pub(crate) const PANIC_OOB: &str = "0x32";
pub(crate) const PANIC_EMPTY_POP: &str = "0x31";
pub(crate) const WAD: &str = "1000000000000000000";

pub(crate) fn checked_add_helper_src(rich: bool) -> String {
    format!(
        "function checked_add(a, b, max) -> r {{\n\
     \x20   r := add(a, b)\n\
     \x20   if or(lt(r, a), gt(r, max)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

pub(crate) fn checked_sub_helper_src(rich: bool) -> String {
    format!(
        "function checked_sub(a, b) -> r {{\n\
     \x20   if lt(a, b) {{ {} }}\n\
     \x20   r := sub(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

pub(crate) fn checked_mul_helper_src(rich: bool) -> String {
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

pub(crate) fn checked_sadd_helper_src(rich: bool) -> String {
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

pub(crate) fn checked_ssub_helper_src(rich: bool) -> String {
    format!(
        "function checked_ssub(a, b) -> r {{\n\
     \x20   r := sub(a, b)\n\
     \x20   if slt(and(xor(a, b), xor(a, r)), 0) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

pub(crate) fn checked_ssub_n_helper_src(rich: bool) -> String {
    format!(
        "function checked_ssub_n(a, b, minv, maxv) -> r {{\n\
     \x20   r := checked_ssub(a, b)\n\
     \x20   if or(slt(r, minv), sgt(r, maxv)) {{ {} }}\n\
     }}\n",
        panic_revert(rich, PANIC_OVERFLOW)
    )
}

pub(crate) fn checked_smul_helper_src(rich: bool) -> String {
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

pub(crate) fn gum_muldiv_helper_src(rich: bool) -> String {
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

pub(crate) fn gum_wad_signed(fname: &str, num: &str, den: &str, rich: bool) -> String {
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

pub(crate) fn gum_wad_mul_helper_src(rich: bool) -> String {
    let den = WAD.to_string();
    gum_wad_signed("gum_wad_mul", "x, y", &den, rich)
}

pub(crate) fn gum_wad_div_helper_src(rich: bool) -> String {
    let num = format!("x, {}", WAD);
    gum_wad_signed("gum_wad_div", &num, "y", rich)
}

pub(crate) fn checked_div_helper_src(rich: bool) -> String {
    format!(
        "function checked_div(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := div(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

pub(crate) fn checked_sdiv_helper_src(rich: bool) -> String {
    format!(
        "function checked_sdiv(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := sdiv(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

pub(crate) fn checked_mod_helper_src(rich: bool) -> String {
    format!(
        "function checked_mod(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := mod(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

pub(crate) fn checked_smod_helper_src(rich: bool) -> String {
    format!(
        "function checked_smod(a, b) -> r {{\n\
     \x20   if iszero(b) {{ {} }}\n\
     \x20   r := smod(a, b)\n\
     }}\n",
        panic_revert(rich, PANIC_DIV_ZERO)
    )
}

pub fn immutable_local(field: &str) -> String {
    format!("_imm_{}", field)
}

pub fn immutable_deploy_local(field: &str) -> String {
    format!("_immv_{}", field)
}

pub(crate) fn mask_hex(size: usize) -> String {
    format!("0x{}", "f".repeat(size * 2))
}

pub(crate) fn gum_str_len_helper_src() -> String {
    "function gum_str_len(p) -> n {\n    n := and(shr(192, mload(p)), 0xffffffffffffffff)\n}\n"
        .to_string()
}

pub(crate) fn gum_uint_to_str_helper_src() -> String {
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

pub fn is_enum_type(tc: &TypeChecker, t: &Type) -> bool {
    tc.is_scalar_enum(t)
}

pub(crate) fn str_literal_body_src(ptr: &str, s: &str) -> String {
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

pub(crate) fn gum_str_eq_helper_src() -> String {
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

pub(crate) fn gum_revert_str_helper_src() -> String {
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

pub(crate) fn gum_sstr_base_helper_src() -> String {
    "function gum_sstr_base(slot) -> b {\n\
     \x20   mstore(0x00, slot)\n\
     \x20   b := keccak256(0x00, 0x20)\n\
     }\n"
    .to_string()
}

pub(crate) fn with_kind(template: &str, tr: bool) -> String {
    template
        .replace("{K}", kind_suffix(tr))
        .replace("{LD}", ld_op(tr))
        .replace("{ST}", st_op(tr))
}

pub(crate) fn gum_sstr_load_helper_src(tr: bool) -> String {
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

pub(crate) fn gum_sstr_clear_helper_src(tr: bool) -> String {
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

pub(crate) fn gum_sstr_store_helper_src(tr: bool) -> String {
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

pub(crate) fn gum_keccak_arr_helper_src() -> String {
    "function gum_keccak_arr(p) -> h {\n    h := keccak256(add(p, 32), mload(p))\n}\n".to_string()
}

pub(crate) fn gum_keccak_str_helper_src() -> String {
    "function gum_keccak_str(p) -> h {\n    h := keccak256(add(p, 32), gum_str_len(p))\n}\n"
        .to_string()
}

pub(crate) fn gum_ecrecover_helper_src() -> String {
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

pub(crate) fn gum_pay_helper_src() -> String {
    "function gum_pay(to, amount) -> ok {\n\
     \x20   ok := call(gas(), to, amount, 0, 0, 0, 0)\n\
     }\n"
    .to_string()
}

pub(crate) fn kind_suffix(tr: bool) -> &'static str {
    if tr { "_t" } else { "" }
}

pub(crate) fn ld_op(tr: bool) -> &'static str {
    if tr { "tload" } else { "sload" }
}

pub(crate) fn st_op(tr: bool) -> &'static str {
    if tr { "tstore" } else { "sstore" }
}

pub(crate) fn gum_marr_addr_helper_src(rich: bool) -> String {
    format!(
        "function gum_marr_addr(ptr, i, esz) -> a {{\n\
     \x20   if iszero(lt(i, div(mload(ptr), esz))) {{ {} }}\n\
     \x20   a := add(add(ptr, 32), mul(i, esz))\n\
     }}\n",
        panic_revert(rich, PANIC_OOB)
    )
}

pub(crate) fn gum_farr_addr_helper_src(rich: bool) -> String {
    format!(
        "function gum_farr_addr(ptr, i, n, esz) -> a {{\n\
     \x20   if iszero(lt(i, n)) {{ {} }}\n\
     \x20   a := add(ptr, mul(i, esz))\n\
     }}\n",
        panic_revert(rich, PANIC_OOB)
    )
}

pub(crate) fn gum_abi_arr_cd_helper_src() -> String {
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

pub(crate) fn gum_abi_arr_mem_helper_src() -> String {
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

pub(crate) fn gum_abi_farr_cd_helper_src() -> String {
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

pub(crate) fn gum_abi_farr_mem_helper_src() -> String {
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

pub(crate) fn gum_abi_farr_put_helper_src() -> String {
    "function gum_abi_farr_put(dst, ptr, n, esz) {\n\
     \x20   let sh := mul(sub(32, esz), 8)\n\
     \x20   let m := sub(shl(mul(esz, 8), 1), 1)\n\
     \x20   for { let i := 0 } lt(i, n) { i := add(i, 1) } {\n\
     \x20       mstore(add(dst, mul(i, 32)), and(shr(sh, mload(add(ptr, mul(i, esz)))), m))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_abi_arr_put_helper_src() -> String {
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

pub(crate) fn gum_abi_arr_size_helper_src() -> String {
    "function gum_abi_arr_size(ptr, esz) -> sz {\n\
     \x20   sz := add(32, mul(div(mload(ptr), esz), 32))\n\
     }\n"
    .to_string()
}

pub(crate) fn abi_dynarr_cd_helper_src(fname: &str, inner_cd: &str) -> String {
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

pub(crate) fn abi_dynarr_mem_helper_src(fname: &str, inner_mem: &str) -> String {
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

pub(crate) fn abi_dynarr_put_helper_src(fname: &str, inner_put: &str) -> String {
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

pub(crate) fn abi_dynarr_size_helper_src(fname: &str, inner_size: &str) -> String {
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

pub(crate) fn abi_dynfarr_cd_helper_src(fname: &str, inner_cd: &str, n: usize) -> String {
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

pub(crate) fn abi_dynfarr_mem_helper_src(fname: &str, inner_mem: &str, n: usize) -> String {
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

pub(crate) fn abi_dynfarr_put_helper_src(fname: &str, inner_put: &str, n: usize) -> String {
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

pub(crate) fn abi_dynfarr_size_helper_src(fname: &str, inner_size: &str, n: usize) -> String {
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

pub(crate) fn abi_statarr_put_helper_src(
    fname: &str,
    inner_put: &str,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_statfarr_cd_helper_src(
    fname: &str,
    inner_cd: &str,
    n: usize,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_statfarr_mem_helper_src(
    fname: &str,
    inner_mem: &str,
    n: usize,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_statfarr_put_helper_src(
    fname: &str,
    inner_put: &str,
    n: usize,
    wire: usize,
    packed: usize,
) -> String {
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

#[derive(Clone, Copy)]
pub struct AbiStructField {
    pub(crate) mem_offset: usize,
    pub(crate) width: usize,
    pub(crate) is_addr: bool,
    pub(crate) enum_variants: Option<usize>,
}

#[derive(Clone)]
pub(crate) struct AbiDynField {
    pub(crate) ty: Type,
    pub(crate) mem_offset: usize,
    pub(crate) width: usize,
    pub(crate) is_addr: bool,
    pub(crate) is_dynamic: bool,
    pub(crate) enum_variants: Option<usize>,
}

pub(crate) fn abi_mangle(t: &Type) -> String {
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

pub fn is_abi_scalar(t: &Type) -> bool {
    matches!(t, Type::Primitive(n) if matches!(n.as_str(),
        "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
        "i8" | "i16" | "i32" | "i64" | "i128" | "i256" |
        "bool" | "Account") || byte_width(n).is_some())
}

pub(crate) fn abi_st_field_store(raw: &str, f: &AbiStructField) -> String {
    let mut out = String::new();

    if let Some(nvar) = f.enum_variants {
        out.push_str(&format!(
            "    if iszero(lt({}, {})) {{ revert(0, 0) }}\n",
            raw, nvar
        ));
    }
    let v = if f.is_addr {
        format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", raw)
    } else {
        raw.to_string()
    };
    let addr = format!("add(ptr, {})", f.mem_offset);
    if f.width >= 32 {
        out.push_str(&format!("    mstore({}, {})\n", addr, v));
    } else {
        let merged = write_packed(&format!("mload({})", addr), 0, f.width, &v);
        out.push_str(&format!("    mstore({}, {})\n", addr, merged));
    }
    out
}

pub(crate) fn abi_st_cd_helper_src(
    fname: &str,
    fields: &[AbiStructField],
    packed: usize,
) -> String {
    let mut body = format!("function {}(off) -> ptr {{\n", fname);
    body.push_str(&format!(
        "    if lt(calldatasize(), add(off, {})) {{ revert(0, 0) }}\n",
        fields.len() * 32
    ));
    body.push_str(&format!("    ptr := allocate_memory({})\n", packed));
    for (i, f) in fields.iter().enumerate() {
        body.push_str(&abi_st_field_store(
            &format!("calldataload(add(off, {}))", i * 32),
            f,
        ));
    }
    body.push_str("}\n");
    body
}

pub(crate) fn abi_st_mem_helper_src(
    fname: &str,
    fields: &[AbiStructField],
    packed: usize,
) -> String {
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

pub(crate) fn abi_st_put_helper_src(fname: &str, fields: &[AbiStructField]) -> String {
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

pub(crate) fn build_dyn_struct_cd(
    fname: &str,
    fields: &[AbiDynField],
    sub: &[Option<String>],
    head: usize,
    packed: usize,
) -> String {
    let mut b = format!("function {}(off) -> ptr {{\n", fname);
    b.push_str(&format!(
        "    if lt(calldatasize(), add(off, {})) {{ revert(0, 0) }}\n",
        head
    ));
    b.push_str(&format!("    ptr := allocate_memory({})\n", packed));
    for (i, f) in fields.iter().enumerate() {
        if f.is_dynamic {
            let cd = sub[i].as_ref().unwrap();
            b.push_str(&format!(
                "    let eo{i} := calldataload(add(off, {ho}))\n",
                i = i,
                ho = i * 32
            ));
            b.push_str(&format!(
                "    if gt(eo{i}, calldatasize()) {{ revert(0, 0) }}\n",
                i = i
            ));
            b.push_str(&format!(
                "    mstore(add(ptr, {mo}), {cd}(add(off, eo{i})))\n",
                mo = f.mem_offset,
                cd = cd,
                i = i
            ));
        } else {
            let asf = AbiStructField {
                mem_offset: f.mem_offset,
                width: f.width,
                is_addr: f.is_addr,
                enum_variants: f.enum_variants,
            };
            b.push_str(&abi_st_field_store(
                &format!("calldataload(add(off, {}))", i * 32),
                &asf,
            ));
        }
    }
    b.push_str("}\n");
    b
}

pub(crate) fn build_dyn_struct_mem(
    fname: &str,
    fields: &[AbiDynField],
    sub: &[Option<String>],
    head: usize,
    packed: usize,
) -> String {
    let mut b = format!("function {}(base, off, limit) -> ptr {{\n", fname);
    b.push_str(&format!(
        "    if lt(limit, add(off, {})) {{ revert(0, 0) }}\n",
        head
    ));
    b.push_str(&format!("    ptr := allocate_memory({})\n", packed));
    for (i, f) in fields.iter().enumerate() {
        if f.is_dynamic {
            let mem = sub[i].as_ref().unwrap();
            b.push_str(&format!(
                "    let eo{i} := mload(add(base, add(off, {ho})))\n",
                i = i,
                ho = i * 32
            ));
            b.push_str(&format!(
                "    if gt(eo{i}, limit) {{ revert(0, 0) }}\n",
                i = i
            ));
            b.push_str(&format!(
                "    mstore(add(ptr, {mo}), {mem}(base, add(off, eo{i}), limit))\n",
                mo = f.mem_offset,
                mem = mem,
                i = i
            ));
        } else {
            let asf = AbiStructField {
                mem_offset: f.mem_offset,
                width: f.width,
                is_addr: f.is_addr,
                enum_variants: f.enum_variants,
            };
            b.push_str(&abi_st_field_store(
                &format!("mload(add(base, add(off, {})))", i * 32),
                &asf,
            ));
        }
    }
    b.push_str("}\n");
    b
}

pub(crate) fn build_dyn_struct_put(
    fname: &str,
    fields: &[AbiDynField],
    sub_put: &[Option<String>],
    head: usize,
) -> String {
    let mut b = format!("function {}(dst, ptr) -> written {{\n", fname);
    b.push_str(&format!("    let cur := {}\n", head));
    for (i, f) in fields.iter().enumerate() {
        if f.is_dynamic {
            let put = sub_put[i].as_ref().unwrap();
            b.push_str(&format!("    mstore(add(dst, {ho}), cur)\n", ho = i * 32));
            b.push_str(&format!(
                "    cur := add(cur, {put}(add(dst, cur), mload(add(ptr, {mo}))))\n",
                put = put,
                mo = f.mem_offset
            ));
        } else {
            let read = read_packed(&format!("mload(add(ptr, {}))", f.mem_offset), 0, f.width);
            let v = if f.is_addr {
                format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", read)
            } else {
                read
            };
            b.push_str(&format!(
                "    mstore(add(dst, {ho}), {v})\n",
                ho = i * 32,
                v = v
            ));
        }
    }
    b.push_str("    written := cur\n");
    b.push_str("}\n");
    b
}

pub(crate) fn build_dyn_struct_size(
    fname: &str,
    fields: &[AbiDynField],
    sub_size: &[Option<String>],
    head: usize,
) -> String {
    let mut b = format!("function {}(ptr) -> sz {{\n", fname);
    b.push_str(&format!("    sz := {}\n", head));
    for (i, f) in fields.iter().enumerate() {
        if f.is_dynamic {
            let sz = sub_size[i].as_ref().unwrap();
            b.push_str(&format!(
                "    sz := add(sz, {sz}(mload(add(ptr, {mo}))))\n",
                sz = sz,
                mo = f.mem_offset
            ));
        }
    }
    b.push_str("}\n");
    b
}

pub(crate) fn gum_abi_str_mem_helper_src() -> String {
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

pub(crate) fn gum_abi_str_cd_helper_src() -> String {
    "function gum_abi_str_cd(off) -> ptr {\n\
     \x20   if gt(add(off, 32), calldatasize()) { revert(0, 0) }\n\
     \x20   let len := calldataload(off)\n\
     \x20   if gt(add(add(off, 32), len), calldatasize()) { revert(0, 0) }\n\
     \x20   ptr := allocate_memory(add(32, len))\n\
     \x20   mstore(ptr, shl(192, len))\n\
     \x20   calldatacopy(add(ptr, 32), add(off, 32), len)\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_abi_str_put_helper_src() -> String {
    "function gum_abi_str_put(dst, ptr) -> written {\n\
     \x20   let len := shr(192, mload(ptr))\n\
     \x20   mstore(dst, len)\n\
     \x20   let full := div(len, 32)\n\
     \x20   let i := 0\n\
     \x20   for {} lt(i, full) { i := add(i, 1) } {\n\
     \x20       mstore(add(add(dst, 32), mul(i, 32)), mload(add(add(ptr, 32), mul(i, 32))))\n\
     \x20   }\n\
     \x20   let rem := mod(len, 32)\n\
     \x20   if rem {\n\
     \x20       let lw := and(mload(add(add(ptr, 32), mul(full, 32))), not(shr(mul(8, rem), not(0))))\n\
     \x20       mstore(add(add(dst, 32), mul(full, 32)), lw)\n\
     \x20   }\n\
     \x20   written := add(32, and(add(len, 31), not(31)))\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_abi_str_size_helper_src() -> String {
    "function gum_abi_str_size(ptr) -> sz {\n\
     \x20   sz := add(32, and(add(shr(192, mload(ptr)), 31), not(31)))\n\
     }\n"
    .to_string()
}

pub(crate) fn abi_starr_cd_helper_src(
    fname: &str,
    st_cd: &str,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_starr_mem_helper_src(
    fname: &str,
    st_mem: &str,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_starr_put_helper_src(
    fname: &str,
    st_put: &str,
    wire: usize,
    packed: usize,
) -> String {
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

pub(crate) fn abi_starr_size_helper_src(fname: &str, wire: usize, packed: usize) -> String {
    format!(
        "function {fname}(ptr) -> sz {{\n\
         \x20   sz := add(32, mul(div(mload(ptr), {packed}), {wire}))\n\
         }}\n",
        fname = fname,
        wire = wire,
        packed = packed
    )
}

pub(crate) fn gum_bubble_revert_helper_src() -> String {
    "function gum_bubble_revert() {\n\
     \x20   returndatacopy(0, 0, returndatasize())\n\
     \x20   revert(0, returndatasize())\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_create_helper_src() -> String {
    "function gum_create(codeptr, value) -> addr {\n\
     \x20   addr := create(value, add(codeptr, 32), gum_str_len(codeptr))\n\
     \x20   if iszero(addr) { gum_bubble_revert() }\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_create2_helper_src() -> String {
    "function gum_create2(codeptr, value, salt) -> addr {\n\
     \x20   addr := create2(value, add(codeptr, 32), gum_str_len(codeptr), salt)\n\
     \x20   if iszero(addr) { gum_bubble_revert() }\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_create2_address_helper_src() -> String {
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

pub(crate) fn gum_transfer_helper_src() -> String {
    "function gum_transfer(to, amount) {\n\
     \x20   if iszero(call(gas(), to, amount, 0, 0, 0, 0)) {\n\
     \x20       returndatacopy(0, 0, returndatasize())\n\
     \x20       revert(0, returndatasize())\n\
     \x20   }\n\
     }\n"
    .to_string()
}

pub(crate) fn gum_p256_verify_helper_src() -> String {
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

pub(crate) fn gum_delegate_of_helper_src() -> String {
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

pub(crate) fn gum_str_concat_helper_src() -> String {
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

pub(crate) fn gum_str_at_helper_src(rich: bool) -> String {
    format!(
        "function gum_str_at(p, i) -> b {{\n\
         \x20   if iszero(lt(i, gum_str_len(p))) {{ {} }}\n\
         \x20   b := shr(248, mload(add(add(p, 32), i)))\n\
         }}\n",
        panic_revert(rich, "0x32")
    )
}

pub(crate) fn gum_str_slice_helper_src(rich: bool) -> String {
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

pub(crate) fn bytes_copy_helper_src() -> String {
    "function bytes_copy(dst, src, len) {\n\
     \x20   let i := 0\n\
     \x20   for {} lt(i, len) { i := add(i, 32) } {\n\
     \x20       mstore(add(dst, i), mload(add(src, i)))\n\
     \x20   }\n\
     }\n"
    .to_string()
}

pub(crate) fn read_slot_packed(container_expr: &str, offset: usize, size: usize) -> String {
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

pub(crate) fn write_slot_packed(
    container_expr: &str,
    offset: usize,
    size: usize,
    val_expr: &str,
) -> String {
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

pub(crate) fn read_packed(container_expr: &str, offset_in_container: usize, size: usize) -> String {
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

pub(crate) fn write_packed(
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

pub(crate) fn literal_u128(expr: &Expr) -> Option<u128> {
    if let Expr::Number(n) = expr {
        n.parse::<u128>().ok()
    } else {
        None
    }
}

pub(crate) fn numeric_meta(name: &str) -> Option<(usize, bool)> {
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

        "f32" | "f64" => Some((256, true)),
        _ => None,
    }
}

pub(crate) fn is_fixed_point(t: &Type) -> bool {
    matches!(t, Type::Primitive(p) if p == "f32" || p == "f64")
}

pub(crate) fn mask_to_width(val_expr: &str, bits: usize, signed: bool) -> String {
    if bits >= 256 {
        return val_expr.to_string();
    }
    if signed {
        format!("signextend({}, {})", bits / 8 - 1, val_expr)
    } else {
        format!("and({}, {})", val_expr, mask_hex(bits / 8))
    }
}

pub fn byte_width(name: &str) -> Option<usize> {
    let n = name.strip_prefix('b')?.parse::<usize>().ok()?;
    (1..=32).contains(&n).then_some(n)
}

pub(crate) fn packed_width(t: &Type) -> Option<usize> {
    if let Type::Primitive(n) = t {
        if let Some((bits, _)) = numeric_meta(n) {
            return Some(bits / 8);
        }
        if n == "bool" {
            return Some(1);
        }
        if n == "Account" {
            return Some(20);
        }
        if let Some(w) = byte_width(n) {
            return Some(w);
        }
    }
    None
}

pub(crate) fn mask_for_type(val_expr: &str, type_def: &Type) -> String {
    if let Type::Primitive(name) = type_def {
        if let Some((bits, signed)) = numeric_meta(name) {
            return mask_to_width(val_expr, bits, signed);
        }

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

pub(crate) fn pack_params(esz: usize) -> (usize, usize) {
    let esz = esz.max(1);
    if esz >= 32 {
        (1, (esz + 31) / 32)
    } else {
        (32 / esz, 1)
    }
}

pub(crate) fn arr_data_base_helper_src() -> String {
    "function arr_data_base(len_slot) -> b {\n\
     \x20   mstore(0, len_slot)\n\
     \x20   b := keccak256(0, 32)\n\
     }\n"
    .to_string()
}

pub(crate) fn pk_read_helper_src(tr: bool) -> String {
    format!(
        "function pk_read{k}(base, i, per, es, esz) -> v {{\n\
     \x20   let s := add(base, mul(div(i, per), es))\n\
     \x20   v := and(shr(mul(mul(mod(i, per), esz), 8), {ld}(s)), sub(shl(mul(esz, 8), 1), 1))\n\
     }}\n",
        k = kind_suffix(tr),
        ld = ld_op(tr)
    )
}

pub(crate) fn pk_write_helper_src(tr: bool) -> String {
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

pub(crate) fn sarr_to_mem_helper_src(
    name: &str,
    esz: usize,
    per: usize,
    es: usize,
    tr: bool,
) -> String {
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

pub(crate) fn struct_elem_slots(size: usize) -> usize {
    size.div_ceil(32).max(1)
}

pub(crate) fn sarr_base_helper_src(rich: bool) -> String {
    format!(
        "function sarr_base(base, i, len, es) -> b {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   b := add(base, mul(i, es))\n\
     }}\n",
        p = panic_revert(rich, PANIC_OOB)
    )
}

pub(crate) fn dsarr_pop_helper_src(rich: bool, tr: bool) -> String {
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

pub(crate) fn pk_get_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function pk_get{k}(base, i, len, per, es, esz) -> v {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   v := pk_read{k}(base, i, per, es, esz)\n\
     }}\n",
        k = kind_suffix(tr),
        p = panic_revert(rich, PANIC_OOB)
    )
}

pub(crate) fn pk_set_helper_src(rich: bool, tr: bool) -> String {
    format!(
        "function pk_set{k}(base, i, len, per, es, esz, v) {{\n\
     \x20   if iszero(lt(i, len)) {{ {p} }}\n\
     \x20   pk_write{k}(base, i, per, es, esz, v)\n\
     }}\n",
        k = kind_suffix(tr),
        p = panic_revert(rich, PANIC_OOB)
    )
}

pub(crate) fn dpk_push_helper_src(tr: bool) -> String {
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

pub(crate) fn dpk_pop_helper_src(rich: bool, tr: bool) -> String {
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

pub(crate) fn dpk_clear_helper_src(tr: bool) -> String {
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

pub(crate) fn make_enum_helper_src() -> String {
    "function make_enum(tag, payload) -> ptr {\n\
     \x20   ptr := allocate_memory(64)\n\
     \x20   mstore(ptr, tag)\n\
     \x20   mstore(add(ptr, 32), payload)\n\
     }\n"
    .to_string()
}

pub(crate) fn keccak256_hex(data: &str) -> String {
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
