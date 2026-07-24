use crate::ast::*;
use crate::codegen::abi::AbiGenerator;
use crate::codegen::translator::{Ctx, Translator, gum_exception_helpers_src};
use crate::codegen::yul::*;

impl<'a> Translator<'a> {
    pub(crate) fn ensure_helper(&self, name: &str, build: impl FnOnce() -> String) {
        let mut thunks = self.helper_thunks.borrow_mut();
        if !thunks.contains_key(name) {
            thunks.insert(name.to_string(), build());
        }
    }

    pub(crate) fn returndata_to_var(&self, t: &Type, dst: &str) -> String {
        let decode = |min: String, expr: String| -> String {
            let mut o = format!("    if lt(returndatasize(), {}) {{ revert(0, 0) }}\n", min);
            o.push_str("    let rd := allocate_memory(returndatasize())\n");
            o.push_str("    returndatacopy(rd, 0, returndatasize())\n");
            o.push_str(&format!("    {} := {}\n", dst, expr));
            o
        };
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_mem", gum_abi_str_mem_helper_src);
            return decode(
                "32".to_string(),
                "gum_abi_str_mem(rd, mload(rd), returndatasize())".to_string(),
            );
        }
        if matches!(t, Type::Array(_) | Type::FixedArray(..)) {
            if let Some(h) = self.ensure_abi_mem(t) {
                return if self.abi_is_dynamic(t) {
                    decode(
                        "32".to_string(),
                        format!("{}(rd, mload(rd), returndatasize())", h),
                    )
                } else {
                    decode(
                        self.abi_head_bytes(t).to_string(),
                        format!("{}(rd, 0, returndatasize())", h),
                    )
                };
            }
        }
        if let Type::Primitive(nm) = t {
            if is_struct_type(self.type_checker(), t) {
                if self.abi_is_dynamic(t) {
                    if let Some(h) = self.ensure_abi_dyn_struct_mem(nm) {
                        return decode(
                            "32".to_string(),
                            format!("{}(rd, mload(rd), returndatasize())", h),
                        );
                    }
                }
                if let Some((h, wire)) = self.ensure_abi_struct_mem(nm) {
                    return decode(wire.to_string(), format!("{}(rd, 0, returndatasize())", h));
                }
            }
        }

        format!(
            "    if lt(returndatasize(), 32) {{ revert(0, 0) }}\n    returndatacopy(0, 0, 32)\n    {} := {}\n",
            dst,
            self.mask_for_type("mload(0)", t)
        )
    }

    pub(crate) fn returndata_tuple_to_vars(&self, fields: &[(String, Type)]) -> String {
        if fields.is_empty() {
            return String::new();
        }
        if fields.len() == 1 {
            return self.returndata_to_var(&fields[0].1, &fields[0].0);
        }
        let head_bytes: usize = fields.iter().map(|(_, t)| self.abi_head_bytes(t)).sum();
        let mut o = format!(
            "    if lt(returndatasize(), {}) {{ revert(0, 0) }}\n",
            head_bytes
        );
        o.push_str("    let __wb := allocate_memory(returndatasize())\n");
        o.push_str("    returndatacopy(__wb, 0, returndatasize())\n");
        let mut at = 0usize;
        for (name, t) in fields {
            let ho = at;
            at += self.abi_head_bytes(t);
            o.push_str(&self.decode_tuple_field(name, t, ho));
        }
        o
    }

    pub(crate) fn decode_tuple_field(&self, name: &str, t: &Type, ho: usize) -> String {
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_mem", gum_abi_str_mem_helper_src);
            return format!(
                "    {} := gum_abi_str_mem(__wb, mload(add(__wb, {ho})), returndatasize())\n",
                name,
                ho = ho
            );
        }
        if matches!(t, Type::Array(_) | Type::FixedArray(..)) {
            if let Some(h) = self.ensure_abi_mem(t) {
                return if self.abi_is_dynamic(t) {
                    format!(
                        "    {} := {h}(__wb, mload(add(__wb, {ho})), returndatasize())\n",
                        name,
                        h = h,
                        ho = ho
                    )
                } else {
                    format!(
                        "    {} := {h}(__wb, {ho}, returndatasize())\n",
                        name,
                        h = h,
                        ho = ho
                    )
                };
            }
        }
        if let Type::Primitive(nm) = t {
            if is_struct_type(self.type_checker(), t) {
                if self.abi_is_dynamic(t) {
                    if let Some(h) = self.ensure_abi_dyn_struct_mem(nm) {
                        return format!(
                            "    {} := {h}(__wb, mload(add(__wb, {ho})), returndatasize())\n",
                            name,
                            h = h,
                            ho = ho
                        );
                    }
                }
                if let Some((h, _)) = self.ensure_abi_struct_mem(nm) {
                    return format!(
                        "    {} := {h}(__wb, {ho}, returndatasize())\n",
                        name,
                        h = h,
                        ho = ho
                    );
                }
            }
        }
        format!(
            "    {} := {}\n",
            name,
            self.mask_for_type(&format!("mload(add(__wb, {}))", ho), t)
        )
    }

    pub(crate) fn extcall_return_src(&self, ret_ty: &Option<Type>) -> String {
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

        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_mem", gum_abi_str_mem_helper_src);
            return decode(
                "32".to_string(),
                "gum_abi_str_mem(rd, mload(rd), returndatasize())".to_string(),
            );
        }

        if matches!(t, Type::Array(_) | Type::FixedArray(..)) {
            if let Some(h) = self.ensure_abi_mem(t) {
                return if self.abi_is_dynamic(t) {
                    decode(
                        "32".to_string(),
                        format!("{}(rd, mload(rd), returndatasize())", h),
                    )
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
                if self.abi_is_dynamic(t) {
                    if let Some(h) = self.ensure_abi_dyn_struct_mem(nm) {
                        return decode(
                            "32".to_string(),
                            format!("{}(rd, mload(rd), returndatasize())", h),
                        );
                    }
                }
                if let Some((h, wire)) = self.ensure_abi_struct_mem(nm) {
                    return decode(wire.to_string(), format!("{}(rd, 0, returndatasize())", h));
                }
                scalar
            }
            _ => scalar,
        }
    }

    pub(crate) fn extcall_wrapper_src(
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

        let arg_exprs: Vec<String> = args.iter().map(|a| self.translate_expr(a, ctx)).collect();
        let target_expr = arg_exprs[0].clone();
        let arg_exprs = &arg_exprs[1..];

        let selector = decl
            .as_ref()
            .map(|m| AbiGenerator::new(self.type_checker()).calculate_selector(m))
            .unwrap_or_else(|| "0x00000000".to_string());

        let n = arg_exprs.len();

        let types: Vec<Type> = decl
            .as_ref()
            .map(|m| {
                m.parameters
                    .iter()
                    .map(|p| p.type_def.clone())
                    .collect::<Vec<_>>()
            })
            .filter(|t: &Vec<Type>| t.len() == n)
            .unwrap_or_else(|| vec![Type::Primitive("u256".to_string()); n]);

        let ret_src = self.extcall_return_src(&decl.and_then(|m| m.return_type.clone()));
        let (size_src, write_src) = self.abi_arg_blob_src(&types);

        let is_try = ctx.try_ok_var.is_some();
        let fn_name = format!(
            "__extcall_{}{}_{}",
            if is_try { "try_" } else { "" },
            iface_name,
            method
        );

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
