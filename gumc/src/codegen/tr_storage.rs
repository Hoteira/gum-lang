use crate::ast::*;
use crate::codegen::layout::StorageField;
use crate::codegen::translator::{Ctx, Translator};
use crate::codegen::yul::*;

impl<'a> Translator<'a> {
    pub(crate) fn hashmap_field_slot(&self, base: &Expr, ctx: &Ctx) -> Option<usize> {
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

    pub(crate) fn hashmap_transient(&self, base: &Expr, ctx: &Ctx) -> bool {
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

    pub(crate) fn hashmap_base_slot_expr(&self, base: &Expr, ctx: &Ctx) -> Option<String> {
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

    pub(crate) fn hashmap_entry_slot(
        &self,
        base: &Expr,
        ctx: &Ctx,
    ) -> Option<(String, Type, bool)> {
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
                let base_slot = self.hashmap_base_slot_expr(map, ctx)?;
                let key_expr = self.translate_expr(key, ctx);
                let tr = self.hashmap_transient(map, ctx);
                return Some((
                    format!("gum_hash_slot({}, {})", key_expr, base_slot),
                    targs[1].clone(),
                    tr,
                ));
            }
        }
        None
    }

    pub(crate) fn resolve_storage_field(&self, base: &Expr, ctx: &Ctx) -> Option<StorageField> {
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

    pub(crate) fn storage_array_to_memory(&self, e: &Expr, ctx: &Ctx) -> Option<String> {
        let (slot, esz, tr) = self.dyn_storage_array(e, ctx)?;
        if self.storage_struct_array(e, ctx).is_some() {
            return None;
        }
        let esz = esz.max(1);
        let (per, es) = pack_params(esz);

        let name = format!("sarr_to_mem_{}{}", esz, kind_suffix(tr));
        self.ensure_helper("arr_data_base", arr_data_base_helper_src);
        self.ensure_pack_read(tr);
        let n = name.clone();
        self.ensure_helper(&name, || sarr_to_mem_helper_src(&n, esz, per, es, tr));
        Some(format!("{}({})", name, slot))
    }

    pub(crate) fn dyn_storage_array(
        &self,
        base: &Expr,
        ctx: &Ctx,
    ) -> Option<(String, usize, bool)> {
        if let Some(sf) = self.resolve_storage_field(base, ctx) {
            if let Type::Array(inner) = self.static_type(base, ctx) {
                return Some((
                    sf.slot.to_string(),
                    self.layout_engine.size_of(&inner),
                    sf.is_transient,
                ));
            }
        }
        if let Some((slot, Type::Array(inner), tr)) = self.hashmap_entry_slot(base, ctx) {
            return Some((slot, self.layout_engine.size_of(&inner), tr));
        }
        None
    }

    pub(crate) fn storage_array_info(
        &self,
        base: &Expr,
        ctx: &Ctx,
    ) -> Option<(usize, usize, usize, bool)> {
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

    pub(crate) fn storage_array_get(
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

    pub(crate) fn storage_array_set(
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

    pub(crate) fn dyn_array_get(
        &self,
        len_slot: &str,
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

    pub(crate) fn dyn_array_set(
        &self,
        len_slot: &str,
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

    pub(crate) fn ensure_pack_read(&self, tr: bool) {
        let rich = self.rich_reverts;
        self.ensure_helper(&format!("pk_read{}", kind_suffix(tr)), || {
            pk_read_helper_src(tr)
        });
        self.ensure_helper(&format!("pk_get{}", kind_suffix(tr)), || {
            pk_get_helper_src(rich, tr)
        });
    }

    pub(crate) fn ensure_pack_write(&self, tr: bool) {
        let rich = self.rich_reverts;
        self.ensure_helper(&format!("pk_write{}", kind_suffix(tr)), || {
            pk_write_helper_src(tr)
        });
        self.ensure_helper(&format!("pk_set{}", kind_suffix(tr)), || {
            pk_set_helper_src(rich, tr)
        });
    }

    pub(crate) fn struct_base_transient(&self, base: &Expr, ctx: &Ctx) -> bool {
        if let Expr::PropertyAccess {
            base: owner,
            property,
        } = base
        {
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

    pub(crate) fn storage_struct_array(
        &self,
        arr: &Expr,
        ctx: &Ctx,
    ) -> Option<(String, String, String, usize, bool)> {
        let sf = self.resolve_storage_field(arr, ctx)?;

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

    pub(crate) fn field_owner(&self, base: &Expr, ctx: &Ctx) -> Option<String> {
        match base {
            Expr::Identifier(n) if n == "self" => ctx.self_ctx.map(|s| s.class_name.clone()),
            Expr::Identifier(n) => Some(n.clone()),
            _ => None,
        }
    }

    pub(crate) fn struct_storage_base(&self, base: &Expr, ctx: &Ctx) -> Option<(String, String)> {
        if let Expr::PropertyAccess {
            base: owner,
            property,
        } = base
        {
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

        let indexed: Option<(&Expr, &Expr)> = match base {
            Expr::IndexAccess { base: arr, index } => Some((arr, index)),
            Expr::MethodCall {
                base: arr,
                method,
                args,
            } if method == "get" && !args.is_empty() => Some((arr, &args[0])),
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

    pub(crate) fn struct_field_slot(
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
}
