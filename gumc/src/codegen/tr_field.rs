use crate::ast::*;
use crate::codegen::layout::{MemoryField, StorageField};
use crate::codegen::translator::Translator;
use crate::codegen::yul::*;

impl<'a> Translator<'a> {
    pub(crate) fn field_is_str(&self, class_name: &str, property: &str) -> bool {
        self.type_checker()
            .loaded_classes
            .get(class_name)
            .and_then(|c| c.fields.iter().find(|f| f.name == property))
            .map(|f| is_str_type(&f.type_def))
            .unwrap_or(false)
    }

    pub(crate) fn sign_extend_read(&self, ty: &Type, raw: String) -> String {
        if let Type::Primitive(n) = ty {
            if let Some(bits) = n.strip_prefix('i').and_then(|s| s.parse::<usize>().ok()) {
                if bits < 256 {
                    return format!("signextend({}, {})", bits / 8 - 1, raw);
                }
            }
        }
        raw
    }

    pub(crate) fn field_type(&self, class_name: &str, property: &str) -> Option<Type> {
        self.type_checker()
            .loaded_classes
            .get(class_name)
            .and_then(|c| c.fields.iter().find(|f| f.name == property))
            .map(|f| f.type_def.clone())
    }

    pub(crate) fn load_storage_field(
        &self,
        class_name: &str,
        property: &str,
        sf: &StorageField,
    ) -> String {
        let tr = sf.is_transient;
        if self.field_is_str(class_name, property) {
            self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
            self.ensure_helper(&format!("gum_sstr_load{}", kind_suffix(tr)), || {
                gum_sstr_load_helper_src(tr)
            });
            return format!("gum_sstr_load{}({})", kind_suffix(tr), sf.slot);
        }
        if sf.size <= 32 {
            let raw = read_slot_packed(
                &format!("{}({})", ld_op(tr), sf.slot),
                sf.offset_in_slot,
                sf.size,
            );
            return match self.field_type(class_name, property) {
                Some(t) => self.sign_extend_read(&t, raw),
                None => raw,
            };
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

    pub(crate) fn store_storage_field(
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

    pub(crate) fn load_memory_field(&self, base_ptr_expr: &str, mf: &MemoryField) -> String {
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

    pub(crate) fn store_memory_field(
        &self,
        base_ptr_expr: &str,
        mf: &MemoryField,
        val_expr: &str,
    ) -> String {
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
}
