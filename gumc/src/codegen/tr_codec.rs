use crate::ast::*;
use crate::codegen::translator::Translator;
use crate::codegen::yul::*;

impl<'a> Translator<'a> {
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

    pub fn abi_struct_layout(&self, name: &str) -> Option<Vec<AbiStructField>> {
        let class = self.type_checker().loaded_classes.get(name)?;
        let mut out = Vec::new();
        for f in &class.fields {
            let is_enum = self.type_checker().is_scalar_enum(&f.type_def);
            if !is_abi_scalar(&f.type_def) && !is_enum {
                return None;
            }
            let mf = self.layout_engine.memory_field(name, &f.name)?;
            out.push(AbiStructField {
                mem_offset: mf.offset,
                width: mf.size,
                is_addr: crate::codegen::is_address_type(&f.type_def),
                enum_variants: if is_enum {
                    Some(self.enum_variant_count(&f.type_def))
                } else {
                    None
                },
            });
        }
        if out.is_empty() {
            return None;
        }
        Some(out)
    }

    pub(crate) fn abi_dyn_struct_layout(&self, name: &str) -> Option<Vec<AbiDynField>> {
        let class = self.type_checker().loaded_classes.get(name)?;
        let mut out = Vec::new();
        let mut any_dynamic = false;
        for f in &class.fields {
            let mf = self.layout_engine.memory_field(name, &f.name)?;
            let is_dynamic = self.abi_is_dynamic(&f.type_def);
            if is_dynamic {
                let ok = is_str_type(&f.type_def)
                    || matches!(&f.type_def, Type::Array(_) | Type::FixedArray(..));
                if !ok {
                    return None;
                }
                any_dynamic = true;
            } else if !is_abi_scalar(&f.type_def)
                && !self.type_checker().is_scalar_enum(&f.type_def)
            {
                return None;
            }
            out.push(AbiDynField {
                ty: f.type_def.clone(),
                mem_offset: mf.offset,
                width: mf.size,
                is_addr: crate::codegen::is_address_type(&f.type_def),
                is_dynamic,
                enum_variants: if self.type_checker().is_scalar_enum(&f.type_def) {
                    Some(self.enum_variant_count(&f.type_def))
                } else {
                    None
                },
            });
        }
        if !any_dynamic || out.is_empty() {
            return None;
        }
        Some(out)
    }

    pub fn ensure_abi_dyn_struct_cd(&self, name: &str) -> Option<String> {
        let fields = self.abi_dyn_struct_layout(name)?;
        let fname = format!("gum_abi_dst_{}_cd", name);
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some(fname);
        }
        let mut sub = Vec::new();
        for f in &fields {
            sub.push(if f.is_dynamic {
                Some(self.ensure_abi_cd(&f.ty)?)
            } else {
                None
            });
        }
        let head = fields.len() * 32;
        let packed = self.abi_struct_packed(name);
        let n2 = fname.clone();
        self.ensure_helper(&fname, move || {
            build_dyn_struct_cd(&n2, &fields, &sub, head, packed)
        });
        Some(fname)
    }

    pub fn ensure_abi_dyn_struct_mem(&self, name: &str) -> Option<String> {
        let fields = self.abi_dyn_struct_layout(name)?;
        let fname = format!("gum_abi_dst_{}_mem", name);
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some(fname);
        }
        let mut sub = Vec::new();
        for f in &fields {
            sub.push(if f.is_dynamic {
                Some(self.ensure_abi_mem(&f.ty)?)
            } else {
                None
            });
        }
        let head = fields.len() * 32;
        let packed = self.abi_struct_packed(name);
        let n2 = fname.clone();
        self.ensure_helper(&fname, move || {
            build_dyn_struct_mem(&n2, &fields, &sub, head, packed)
        });
        Some(fname)
    }

    pub fn ensure_abi_dyn_struct_put(&self, name: &str) -> Option<(String, String)> {
        let fields = self.abi_dyn_struct_layout(name)?;
        let fname = format!("gum_abi_dst_{}_put", name);
        let sname = format!("gum_abi_dst_{}_size", name);
        if self.helper_thunks.borrow().contains_key(&fname) {
            return Some((fname, sname));
        }
        let mut sub_put = Vec::new();
        let mut sub_size = Vec::new();
        for f in &fields {
            if f.is_dynamic {
                let (p, s) = self.ensure_abi_put(&f.ty)?;
                sub_put.push(Some(p));
                sub_size.push(Some(s));
            } else {
                sub_put.push(None);
                sub_size.push(None);
            }
        }
        let head = fields.len() * 32;
        let (fields2, n2, s2) = (fields.clone(), fname.clone(), sname.clone());
        self.ensure_helper(&fname, move || {
            build_dyn_struct_put(&n2, &fields2, &sub_put, head)
        });
        self.ensure_helper(&sname, move || {
            build_dyn_struct_size(&s2, &fields, &sub_size, head)
        });
        Some((fname, sname))
    }

    pub fn abi_struct_wire_size(&self, name: &str) -> Option<usize> {
        self.abi_struct_layout(name).map(|f| f.len() * 32)
    }

    pub(crate) fn abi_struct_packed(&self, name: &str) -> usize {
        self.layout_engine
            .size_of(&Type::Primitive(name.to_string()))
    }

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

    pub fn abi_is_dynamic(&self, t: &Type) -> bool {
        match t {
            Type::Array(_) => true,
            Type::FixedArray(inner, _) => self.abi_is_dynamic(inner),
            Type::Primitive(n) => {
                if n == "String" || n == "Bytes" {
                    return true;
                }

                if is_struct_type(self.type_checker(), t) {
                    if let Some(c) = self.type_checker().loaded_classes.get(n) {
                        return c.fields.iter().any(|f| self.abi_is_dynamic(&f.type_def));
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub(crate) fn abi_static_struct(&self, t: &Type) -> Option<String> {
        match t {
            Type::Primitive(n) if is_struct_type(self.type_checker(), t) => {
                self.abi_struct_layout(n).map(|_| n.clone())
            }
            _ => None,
        }
    }

    pub(crate) fn enum_variant_count(&self, t: &Type) -> usize {
        if let Type::Primitive(name) = t {
            if let Some(e) = self.type_checker().loaded_enums.get(name) {
                return e.variants.len();
            }
        }
        0
    }

    pub fn ensure_abi_cd(&self, t: &Type) -> Option<String> {
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_cd", gum_abi_str_cd_helper_src);
            return Some("gum_abi_str_cd".to_string());
        }
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
                } else if self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_arr_cd();
                    let esz = self.layout_engine.size_of(inner);
                    let nvar = self.enum_variant_count(inner);
                    format!(
                        "function {f}(off) -> ptr {{\n\
                         \x20   if lt(calldatasize(), add(off, 32)) {{ revert(0, 0) }}\n\
                         \x20   let n := calldataload(off)\n\
                         \x20   if gt(n, div(sub(calldatasize(), add(off, 32)), 32)) {{ revert(0, 0) }}\n\
                         \x20   for {{ let i := 0 }} lt(i, n) {{ i := add(i, 1) }} {{\n\
                         \x20       if iszero(lt(calldataload(add(add(off, 32), mul(i, 32))), {nv})) {{ revert(0, 0) }}\n\
                         \x20   }}\n\
                         \x20   ptr := gum_abi_arr_cd(off, {esz})\n\
                         }}\n",
                        f = fname,
                        nv = nvar,
                        esz = esz
                    )
                } else if is_abi_scalar(inner) {
                    self.ensure_abi_arr_cd();
                    let esz = self.layout_engine.size_of(inner);
                    format!(
                        "function {}(off) -> ptr {{\n    ptr := gum_abi_arr_cd(off, {})\n}}\n",
                        fname, esz
                    )
                } else {
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
                } else if self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_farr_cd();
                    let esz = self.layout_engine.size_of(inner);
                    let nvar = self.enum_variant_count(inner);
                    format!(
                        "function {f}(off) -> ptr {{\n\
                         \x20   for {{ let i := 0 }} lt(i, {n}) {{ i := add(i, 1) }} {{\n\
                         \x20       if iszero(lt(calldataload(add(off, mul(i, 32))), {nv})) {{ revert(0, 0) }}\n\
                         \x20   }}\n\
                         \x20   ptr := gum_abi_farr_cd(off, {n}, {esz})\n\
                         }}\n",
                        f = fname,
                        n = n,
                        nv = nvar,
                        esz = esz
                    )
                } else if is_abi_scalar(inner) {
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
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_mem", gum_abi_str_mem_helper_src);
            return Some("gum_abi_str_mem".to_string());
        }
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

    pub fn ensure_abi_put(&self, t: &Type) -> Option<(String, String)> {
        if is_str_type(t) {
            self.ensure_helper("gum_abi_str_put", gum_abi_str_put_helper_src);
            self.ensure_helper("gum_abi_str_size", gum_abi_str_size_helper_src);
            return Some((
                "gum_abi_str_put".to_string(),
                "gum_abi_str_size".to_string(),
            ));
        }
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
                        format!(
                            "function {}(ptr) -> sz {{\n    ptr := ptr\n    sz := {}\n}}\n",
                            sname,
                            n * wire
                        ),
                    )
                } else if is_abi_scalar(inner) || self.type_checker().is_scalar_enum(inner) {
                    self.ensure_abi_farr_put();
                    let esz = self.layout_engine.size_of(inner);
                    (
                        format!(
                            "function {}(dst, ptr) -> written {{\n    gum_abi_farr_put(dst, ptr, {}, {})\n    written := {}\n}}\n",
                            fname,
                            n,
                            esz,
                            n * 32
                        ),
                        format!(
                            "function {}(ptr) -> sz {{\n    ptr := ptr\n    sz := {}\n}}\n",
                            sname,
                            n * 32
                        ),
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

    pub fn abi_head_bytes(&self, t: &Type) -> usize {
        if self.abi_is_dynamic(t) {
            return 32;
        }
        match t {
            Type::FixedArray(inner, n) => n * self.abi_head_bytes(inner),
            Type::Primitive(name) if is_struct_type(self.type_checker(), t) => {
                self.abi_struct_wire_size(name).unwrap_or(32)
            }
            _ => 32,
        }
    }
}
