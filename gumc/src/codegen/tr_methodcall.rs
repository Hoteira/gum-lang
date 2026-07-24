use crate::ast::*;
use crate::codegen::translator::{Ctx, Translator, is_numeric_primitive};
use crate::codegen::yul::*;
use crate::semantic::super_name;

impl<'a> Translator<'a> {
    pub(crate) fn translate_super_call(&self, method: &str, args: &[Expr], ctx: &Ctx) -> String {
        let class_name = match ctx.self_ctx {
            Some(s) => s.class_name.clone(),
            None => return String::new(),
        };
        let name = format!("{}_{}", class_name, super_name(method));
        let class_decl = self.type_checker().loaded_classes.get(&class_name);
        let is_global = class_decl.map(|c| c.is_global).unwrap_or(false);
        let has_self = class_decl
            .and_then(|c| c.methods.iter().find(|m| m.name == super_name(method)))
            .map(|m| m.has_self)
            .unwrap_or(true);
        let mut all: Vec<String> = Vec::new();
        if !is_global && has_self {
            all.push("self".to_string());
        }
        all.extend(args.iter().map(|a| self.translate_expr(a, ctx)));
        format!("{}({})", name, all.join(", "))
    }

    pub(crate) fn translate_method_call(
        &self,
        base: &Expr,
        method: &str,
        args: &[Expr],
        ctx: &Ctx,
    ) -> String {
        if matches!(base, Expr::Identifier(b) if b == "super") {
            return self.translate_super_call(method, args, ctx);
        }

        if let Expr::PropertyAccess {
            base: inner,
            property: ancestor,
        } = base
        {
            if let Type::Primitive(owner) = self.static_type(inner, ctx) {
                let is_ancestor = self
                    .type_checker()
                    .loaded_classes
                    .get(&owner)
                    .map(|c| c.parents.iter().any(|p| p == ancestor))
                    .unwrap_or(false);
                if is_ancestor && self.type_checker().loaded_classes.contains_key(ancestor) {
                    let name = format!(
                        "{}_{}",
                        owner,
                        crate::semantic::qualified_method_name(ancestor, method)
                    );
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
                    "to_string" if name.starts_with('u') => {
                        return self.translate_to_string(base, ctx);
                    }
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
            if ns == "Abi" && matches!(method, "encode" | "encode_packed") {
                return self.translate_abi_encode(method == "encode_packed", args, ctx);
            }
        }

        if matches!(method, "len" | "get") {
            if let Some((slot, elem_size, tr)) = self.dyn_storage_array(base, ctx) {
                if method == "len" && args.is_empty() {
                    return format!("{}({})", ld_op(tr), slot);
                }
                if method == "get" && args.len() == 1 {
                    let idx = self.translate_expr(&args[0], ctx);
                    return self.dyn_array_get(&slot, elem_size, &idx, tr);
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
                        let value_is_str = is_str_type(&type_args[1]);
                        if method == "get" {
                            if value_is_str {
                                self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
                                self.ensure_helper(
                                    &format!("gum_sstr_load{}", kind_suffix(tr)),
                                    || gum_sstr_load_helper_src(tr),
                                );
                                return format!("gum_sstr_load{}({})", kind_suffix(tr), slot);
                            }
                            return if value_is_map {
                                slot
                            } else {
                                format!("{}({})", ld_op(tr), slot)
                            };
                        } else if let Some(value_arg) = args.get(1) {
                            let val_expr = self.translate_expr(value_arg, ctx);
                            if value_is_str {
                                self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                                self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
                                self.ensure_helper(
                                    &format!("gum_sstr_store{}", kind_suffix(tr)),
                                    || gum_sstr_store_helper_src(tr),
                                );
                                return format!(
                                    "gum_sstr_store{}({}, {})",
                                    kind_suffix(tr),
                                    slot,
                                    val_expr
                                );
                            }
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
                let has_self = class_decl
                    .methods
                    .iter()
                    .find(|m| m.name == method)
                    .map(|m| m.has_self)
                    .unwrap_or(false);
                if class_decl.is_global || !has_self {
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
                return self.extcall_wrapper_src(
                    iface_name,
                    method,
                    &std::iter::once(cast_args[0].clone())
                        .chain(args.iter().cloned())
                        .collect::<Vec<_>>(),
                    ctx,
                );
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
                let method_decl = class_decl.methods.iter().find(|m| m.name == method);
                let has_self = method_decl.map(|m| m.has_self).unwrap_or(false);
                if is_global {
                    return format!("{}_{}({})", class_name, method, arg_strs.join(", "));
                }

                if bare_class && (class_name == "Message" || class_name == "Block") {
                    return format!("{}_{}({})", class_name, method, arg_strs.join(", "));
                }

                if !has_self {
                    if method_decl.is_none() {
                        self.errors.borrow_mut().push(format!(
                            "'{}.{}()' is not a method of '{}'.",
                            class_name, method, class_name
                        ));
                        return String::new();
                    }
                    return format!("{}_{}({})", class_name, method, arg_strs.join(", "));
                }

                if bare_class {
                    return self.translate_construction(
                        &Type::Primitive(class_name.clone()),
                        args,
                        ctx,
                        method,
                    );
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
}
