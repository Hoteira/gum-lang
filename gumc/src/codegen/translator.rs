use crate::ast::*;
use crate::codegen::abi::{AbiGenerator, AbiInput};
use crate::codegen::layout::{LayoutEngine, StorageField, immutable_key};
use crate::codegen::yul::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
pub struct SelfCtx {
    pub class_name: String,
    pub is_global: bool,
}

pub struct Ctx<'c> {
    pub self_ctx: Option<&'c SelfCtx>,

    pub is_entry: bool,
    pub try_ok_var: Option<String>,

    locals: RefCell<HashMap<String, Type>>,

    pub return_type: Option<Type>,

    pub lock_slot: Option<String>,

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

    pub fn in_constructor(mut self, yes: bool) -> Self {
        self.in_constructor = yes;
        self
    }

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

pub(crate) fn is_numeric_primitive(name: &str) -> bool {
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
    pub free_fns: &'a HashSet<String>,

    pub(crate) helper_thunks: RefCell<BTreeMap<String, String>>,

    pub(crate) literal_counter: RefCell<usize>,

    pub(crate) rich_reverts: bool,

    pub(crate) events: RefCell<BTreeMap<String, EventSchema>>,

    pub(crate) errors: RefCell<Vec<String>>,
}

#[derive(Clone, PartialEq)]
pub struct EventSchema {
    pub inputs: Vec<AbiInput>,

    pub signature: String,
}

fn abi_input_shape_eq(a: &AbiInput, b: &AbiInput) -> bool {
    a.type_name == b.type_name
        && a.indexed == b.indexed
        && a.components.len() == b.components.len()
        && a.components
            .iter()
            .zip(&b.components)
            .all(|(x, y)| abi_input_shape_eq(x, y))
}

fn event_shape_eq(a: &EventSchema, b: &EventSchema) -> bool {
    a.signature == b.signature
        && a.inputs.len() == b.inputs.len()
        && a.inputs
            .iter()
            .zip(&b.inputs)
            .all(|(x, y)| abi_input_shape_eq(x, y))
}

pub(crate) fn gum_exception_helpers_src() -> String {
    "function gum_set_exception() {
        tstore(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff, 1)
    }
    function gum_check_exception() -> has_err {
        has_err := tload(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff)
        if has_err { tstore(0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff, 0) }
    }\n"
    .to_string()
}

impl<'a> Translator<'a> {
    pub fn new(
        layout_engine: &'a LayoutEngine<'a>,
        abi_gen: &'a AbiGenerator<'a>,
        top_level_fns: &'a HashSet<String>,
        free_fns: &'a HashSet<String>,
        rich_reverts: bool,
    ) -> Self {
        Self {
            layout_engine,
            abi_gen,
            top_level_fns,
            free_fns,
            helper_thunks: RefCell::new(BTreeMap::new()),
            literal_counter: RefCell::new(0),
            rich_reverts,
            events: RefCell::new(BTreeMap::new()),
            errors: RefCell::new(Vec::new()),
        }
    }

    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.borrow_mut())
    }

    pub fn recorded_events(&self) -> Vec<(String, EventSchema)> {
        self.events
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub(crate) fn record_event(&self, name: &str, schema: EventSchema) -> Result<(), String> {
        let mut events = self.events.borrow_mut();
        match events.get(name) {
            Some(prev) if event_shape_eq(prev, &schema) => Ok(()),
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

    pub(crate) fn next_literal_id(&self) -> usize {
        let mut c = self.literal_counter.borrow_mut();
        *c += 1;
        *c
    }

    pub fn drain_helper_thunks(&self) -> Vec<String> {
        std::mem::take(&mut *self.helper_thunks.borrow_mut())
            .into_values()
            .collect()
    }

    pub fn require_bytes_copy(&self) {
        self.ensure_helper("bytes_copy", bytes_copy_helper_src);
    }

    pub fn mask_for_type(&self, val_expr: &str, type_def: &Type) -> String {
        mask_for_type(val_expr, type_def)
    }

    pub(crate) fn type_checker(&self) -> &crate::semantic::TypeChecker {
        self.layout_engine.type_checker
    }

    pub(crate) fn static_type(&self, expr: &Expr, ctx: &Ctx) -> Type {
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
                if let Expr::Identifier(ns) = &**base {
                    if ns == "Abi" && matches!(method.as_str(), "encode" | "encode_packed") {
                        return Type::Primitive("Bytes".to_string());
                    }
                }
                let base_ty = self.static_type(base, ctx);
                if let Type::Generic { name, args } = &base_ty {
                    if name == "HashMap" && args.len() == 2 && method == "get" {
                        return args[1].clone();
                    }
                }
                if let Type::Primitive(class_name) = &base_ty {
                    if (class_name == "String" || class_name == "Bytes")
                        && matches!(method.as_str(), "concat" | "slice")
                    {
                        return base_ty.clone();
                    }

                    if method == "to_string" && is_numeric_primitive(class_name) {
                        return Type::Primitive("String".to_string());
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
            Expr::StaticCall { type_def, .. } => type_def.clone(),
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
                    Expr::PropertyAccess { base, property }
                        if matches!(base.as_ref(), Expr::Identifier(ns) if ns == "Vm")
                            && property == "sender" =>
                    {
                        self.translate_set_sender(&val_expr)
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
                                    let slot = format!("gum_hash_slot({}, {})", idx, base_slot);

                                    if is_str_type(&targs[1]) {
                                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                                        self.ensure_helper(
                                            "gum_sstr_base",
                                            gum_sstr_base_helper_src,
                                        );
                                        self.ensure_helper(
                                            &format!("gum_sstr_store{}", kind_suffix(tr)),
                                            || gum_sstr_store_helper_src(tr),
                                        );
                                        return format!(
                                            "gum_sstr_store{}({}, {})\n",
                                            kind_suffix(tr),
                                            slot,
                                            val_expr
                                        );
                                    }
                                    return format!("{}({}, {})\n", st_op(tr), slot, val_expr);
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
                            return self.dyn_array_set(&base_slot, elem_size, &idx, &val_expr, tr);
                        }
                        let i = self.translate_expr(index, ctx);
                        let (addr_expr, stride) = self.mem_array_addr(base, &i, ctx);
                        let av = format!("__ma_{}", self.next_literal_id());
                        let mut out = format!("let {} := {}\n", av, addr_expr);
                        let elem_ty = match self.static_type(base, ctx) {
                            Type::Array(inner) | Type::FixedArray(inner, _) => Some(*inner),
                            _ => None,
                        };

                        if elem_ty
                            .as_ref()
                            .map(|t| self.elem_is_inline(t))
                            .unwrap_or(false)
                        {
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

                let mut arr_ret: Option<(String, String, bool, usize)> = None;
                if let Some(ret_ty) = &ctx.return_type {
                    if !matches!(ret_ty, Type::Primitive(n) if byte_width(n).is_some()) {
                        val_expr = mask_for_type(&val_expr, ret_ty);
                    }
                    if let Type::Primitive(name) = ret_ty {
                        if name == "String" || name == "Bytes" {
                            is_dynamic = true;
                        }
                        if ctx.is_entry && is_struct_type(self.type_checker(), ret_ty) {
                            if self.abi_is_dynamic(ret_ty) {
                                if let Some((put, size_fn)) = self.ensure_abi_dyn_struct_put(name) {
                                    arr_ret = Some((put, size_fn, true, 32));
                                }
                            } else {
                                struct_ret = self.ensure_abi_struct_put(name);
                            }
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
                    Expr::MethodCall { base, method, args } => {
                        (base, method.as_str(), args.as_slice())
                    }
                    Expr::PropertyAccess { base, property } => {
                        (base, property.as_str(), &[] as &[Expr])
                    }
                    _ => unreachable!(),
                };
                let Expr::Identifier(enum_name) = &**base else {
                    unreachable!()
                };
                let enum_decl = self.type_checker().loaded_enums.get(enum_name).unwrap();
                let variant = enum_decl
                    .variants
                    .iter()
                    .find(|v| v.name == method)
                    .unwrap();
                let abi_gen = AbiGenerator::new(self.type_checker());
                let selector = abi_gen.calculate_error_selector(variant);
                let types: Vec<Type> = variant
                    .parameters
                    .iter()
                    .map(|p| p.type_def.clone())
                    .collect();
                self.emit_revert_data(&format!("Revert {}", method), &selector, args, &types, ctx)
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
            Statement::TryCatch {
                try_body,
                catch_body,
            } => {
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
                    out.push_str(&format!(
                        "    if gum_check_exception() {{\n        {} := 0\n        break\n    }}\n",
                        try_ok_var
                    ));
                }
                out.push_str("    break\n}\n");

                out.push_str(&format!("if iszero({}) {{\n", try_ok_var));
                for s in catch_body {
                    out.push_str(&self.translate_statement(&s.node, ctx));
                }
                out.push_str("}\n");
                out
            }
            Statement::ScopedTryCall {
                thunk,
                args,
                propagate_return,
                writeback,
                catch_body,
            } => self.translate_scoped_try(
                thunk,
                args,
                *propagate_return,
                writeback,
                catch_body,
                ctx,
            ),
            Statement::ReturnCaptures(fields) => {
                let types: Vec<Type> = fields.iter().map(|(_, t)| t.clone()).collect();
                let (size_src, write_src) = self.abi_arg_blob_src(&types);
                let mut b = String::from("{\n");
                for (i, (name, _)) in fields.iter().enumerate() {
                    b.push_str(&format!("    let a{} := {}\n", i, name));
                }
                b.push_str(&size_src);
                b.push_str("    let blob := allocate_memory(alen)\n");
                b.push_str(&write_src);
                b.push_str("    return(blob, alen)\n");
                b.push_str("}\n");
                b
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

                let scalar_enum = self
                    .type_checker()
                    .is_scalar_enum(&self.static_type(expr, ctx));
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

    pub(crate) fn translate_log_stmt(&self, args: &[Expr], ctx: &Ctx) -> String {
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

        let field_types: Vec<Type> = fields
            .iter()
            .map(|(_, e)| self.static_type(e, ctx))
            .collect();
        let sig_types: Vec<String> = field_types
            .iter()
            .map(|t| self.abi_gen.signature_type(t))
            .collect();
        let signature = format!("{}({})", event_name, sig_types.join(","));
        let topic0 = keccak256_hex(&signature);

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

        let log_op = format!("log{}", topics.len());

        let data: Vec<(String, Type)> = fields
            .iter()
            .zip(&field_types)
            .filter(|((indexed, _), _)| !indexed)
            .map(|((_, e), t)| (self.translate_expr(e, ctx), t.clone()))
            .collect();

        if data.is_empty() {
            return format!("{}(0, 0, {})\n", log_op, topics.join(", "));
        }

        let types: Vec<Type> = data.iter().map(|(_, t)| t.clone()).collect();
        let (size_src, write_src) = self.abi_arg_blob_src(&types);

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

    pub(crate) fn translate_property_store(
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

        self.errors.borrow_mut().push(format!(
            "no known storage or memory offset for '{}' on a {}, so it cannot be assigned. This is a compiler gap rather than a mistake in your code: please report it.",
            property,
            self.abi_gen.signature_type(&self.static_type(base, ctx))
        ));
        String::new()
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
            Expr::StaticCall {
                type_def,
                method,
                args,
            } => self.translate_static_call(type_def, method, args, ctx),
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
                                return self.sign_extend_read(
                                    &self.static_type(expr, ctx),
                                    self.load_memory_field("self", &mf),
                                );
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
                        let esz = self.mem_elem_stride(&inner).max(1);
                        let b = self.translate_expr(base, ctx);
                        return format!("div(mload({}), {})", b, esz);
                    }
                }
                if let Some((base_slot, struct_name)) = self.struct_storage_base(base, ctx) {
                    if let Some((slot, off, size)) =
                        self.struct_field_slot(&base_slot, &struct_name, property)
                    {
                        let tr = self.struct_base_transient(base, ctx);
                        return self.sign_extend_read(
                            &self.static_type(expr, ctx),
                            read_slot_packed(&format!("{}({})", ld_op(tr), slot), off, size),
                        );
                    }
                }
                if let Type::Primitive(class_name) = self.static_type(base, ctx) {
                    if let Some(mf) = self.layout_engine.memory_field(&class_name, property) {
                        return self.sign_extend_read(
                            &self.static_type(expr, ctx),
                            self.load_memory_field(&self.translate_expr(base, ctx), &mf),
                        );
                    }
                }

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

                            if is_str_type(&targs[1]) {
                                self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
                                self.ensure_helper(
                                    &format!("gum_sstr_load{}", kind_suffix(tr)),
                                    || gum_sstr_load_helper_src(tr),
                                );
                                return format!("gum_sstr_load{}({})", kind_suffix(tr), slot);
                            }
                            return format!("{}({})", ld_op(tr), slot);
                        }
                    }
                }

                let elem_ty = self.static_type(expr, ctx);
                if let Some((base_slot, elem_size, len, tr)) = self.storage_array_info(base, ctx) {
                    let idx = self.translate_expr(index, ctx);
                    return self.sign_extend_read(
                        &elem_ty,
                        self.storage_array_get(base_slot, elem_size, len, &idx, tr),
                    );
                }
                if let Some((slot, elem_size, tr)) = self.dyn_storage_array(base, ctx) {
                    let idx = self.translate_expr(index, ctx);
                    return self.sign_extend_read(
                        &elem_ty,
                        self.dyn_array_get(&slot, elem_size, &idx, tr),
                    );
                }
                let i = self.translate_expr(index, ctx);
                let (addr, stride) = self.mem_array_addr(base, &i, ctx);
                if self.elem_is_inline(&elem_ty) {
                    return addr;
                }
                self.sign_extend_read(
                    &elem_ty,
                    read_packed(&format!("mload({})", addr), 0, stride),
                )
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

                if args.len() == 1 && self.type_checker().loaded_classes.contains_key(name) {
                    // A one-argument call on a class name is a reinterpret cast,
                    // e.g. Account(0) for the zero address. The wrapper is transparent.
                    return self.translate_expr(&args[0], ctx);
                }

                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                let callee = if self.free_fns.contains(name) {
                    format!("gumfn_{}", name)
                } else if self.top_level_fns.contains(name) {
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

    pub(crate) fn abi_arg_blob_src(&self, types: &[Type]) -> (String, String) {
        let head_bytes: usize = types.iter().map(|t| self.abi_head_bytes(t)).sum();
        let head_at: Vec<usize> = types
            .iter()
            .scan(0usize, |acc, t| {
                let at = *acc;
                *acc += self.abi_head_bytes(t);
                Some(at)
            })
            .collect();
        let any_dynamic = types
            .iter()
            .any(|t| is_str_type(t) || self.abi_is_dynamic(t));

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

        let dyn_struct_put: Vec<Option<(String, String)>> = types
            .iter()
            .map(|t| match t {
                Type::Primitive(n)
                    if is_struct_type(self.type_checker(), t) && self.abi_is_dynamic(t) =>
                {
                    self.ensure_abi_dyn_struct_put(n)
                }
                _ => None,
            })
            .collect();

        let mut size_src = format!("    let alen := {}\n", head_bytes);
        for (i, t) in types.iter().enumerate() {
            if is_str_type(t) {
                size_src.push_str(&format!("    let a{i}_len := gum_str_len(a{i})\n", i = i));
                size_src.push_str(&format!(
                    "    let a{i}_pad := and(add(a{i}_len, 31), not(31))\n",
                    i = i
                ));
                size_src.push_str(&format!("    alen := add(alen, add(32, a{}_pad))\n", i));
            } else if let Some((_, size_fn, true)) = &arr_put[i] {
                size_src.push_str(&format!(
                    "    let a{i}_abi := {s}(a{i})\n",
                    i = i,
                    s = size_fn
                ));
                size_src.push_str(&format!("    alen := add(alen, a{}_abi)\n", i));
            } else if let Some((_, size_fn)) = &dyn_struct_put[i] {
                size_src.push_str(&format!(
                    "    let a{i}_abi := {s}(a{i})\n",
                    i = i,
                    s = size_fn
                ));
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
            } else if let Some((put, _)) = &dyn_struct_put[i] {
                write_src.push_str(&format!("    mstore(add(blob, {}), tail)\n", at));
                write_src.push_str(&format!(
                    "    tail := add(tail, {}(add(blob, tail), a{}))\n",
                    put, i
                ));
            } else if let Some(helper) = &struct_put[i] {
                write_src.push_str(&format!("    {}(add(blob, {}), a{})\n", helper, at, i));
            } else {
                write_src.push_str(&format!("    mstore(add(blob, {}), a{})\n", at, i));
            }
        }
        (size_src, write_src)
    }

    pub(crate) fn translate_contract_deploy(&self, name: &str, args: &[Expr], ctx: &Ctx) -> String {
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
            b.push_str(&format!(
                "    let size := datasize(\"{}\")
",
                nm
            ));
            b.push_str(&size_src);
            b.push_str(
                "    let ptr := allocate_memory(add(size, alen))
",
            );
            b.push_str(&format!(
                "    datacopy(ptr, dataoffset(\"{}\"), size)
",
                nm
            ));
            b.push_str(
                "    let blob := add(ptr, size)
",
            );
            b.push_str(&write_src);
            b.push_str(
                "    addr := create(0, ptr, add(size, alen))
",
            );
            b.push_str(
                "    if iszero(addr) { gum_bubble_revert() }
",
            );
            b.push_str(
                "}
",
            );
            b
        });

        let call_args: Vec<String> = passed.into_iter().map(|(e, _)| e).collect();
        format!("{}({})", key, call_args.join(", "))
    }

    pub(crate) fn translate_static_call(
        &self,
        type_def: &Type,
        method: &str,
        args: &[Expr],
        ctx: &Ctx,
    ) -> String {
        if let Type::Primitive(name) = type_def {
            if name == "Account" {
                let a: Vec<String> = args.iter().map(|x| self.translate_expr(x, ctx)).collect();
                match (method, a.len()) {
                    ("create", 2) => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);
                        self.ensure_helper("gum_create", gum_create_helper_src);
                        return format!("gum_create({}, {})", a[0], a[1]);
                    }
                    ("create2", 3) => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_bubble_revert", gum_bubble_revert_helper_src);
                        self.ensure_helper("gum_create2", gum_create2_helper_src);
                        return format!("gum_create2({}, {}, {})", a[0], a[1], a[2]);
                    }
                    ("create2_address", 2) => {
                        self.ensure_helper("gum_str_len", gum_str_len_helper_src);
                        self.ensure_helper("gum_create2_address", gum_create2_address_helper_src);
                        return format!("gum_create2_address({}, {})", a[0], a[1]);
                    }
                    _ => {}
                }
            }
            if args.is_empty() {
                if let Some((bits, signed)) = numeric_meta(name) {
                    if let Some(v) = type_bound_literal(bits, signed, method) {
                        return v;
                    }
                }
            }
        }

        let (class_name, suffix) = match type_def {
            Type::Primitive(n) => (Some(n.clone()), String::new()),
            Type::Generic {
                name,
                args: type_args,
            } => (Some(name.clone()), super::generic_suffix(type_args)),
            _ => (None, String::new()),
        };
        if let Some(class_name) = class_name {
            if let Some(class_decl) = self.type_checker().loaded_classes.get(&class_name) {
                let has_self = class_decl
                    .methods
                    .iter()
                    .find(|m| m.name == method)
                    .map(|m| m.has_self)
                    .unwrap_or(false);
                if has_self {
                    return self.translate_construction(type_def, args, ctx, method);
                }
                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                let fname = if suffix.is_empty() {
                    format!("{}_{}", class_name, method)
                } else {
                    format!("{}_{}_{}", class_name, suffix, method)
                };
                return format!("{}({})", fname, arg_strs.join(", "));
            }
        }
        "revert(0, 0)".to_string()
    }

    pub(crate) fn translate_instantiation(
        &self,
        type_def: &Type,
        args: &[Expr],
        ctx: &Ctx,
    ) -> String {
        self.translate_construction(type_def, args, ctx, "new")
    }

    pub(crate) fn translate_construction(
        &self,
        type_def: &Type,
        args: &[Expr],
        ctx: &Ctx,
        ctor_name: &str,
    ) -> String {
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
                    .and_then(|c| c.methods.iter().find(|m| m.name == ctor_name))
                {
                    let arg_exprs: Vec<String> =
                        args.iter().map(|a| self.translate_expr(a, ctx)).collect();
                    let thunk_key = if suffix.is_empty() {
                        format!("__ctor_{}_{}", ctor_name, class_name)
                    } else {
                        format!("__ctor_{}_{}_{}", ctor_name, class_name, suffix)
                    };
                    let ctor_fn_name = if suffix.is_empty() {
                        format!("{}_{}", class_name, ctor_name)
                    } else {
                        format!("{}_{}_{}", class_name, suffix, ctor_name)
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

    pub(crate) fn emit_revert_data(
        &self,
        label: &str,
        selector: &str,
        args: &[Expr],
        types: &[Type],
        ctx: &Ctx,
    ) -> String {
        let mut out = format!("// {}\n", label);

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

        out.push_str("let _p := allocate_memory(add(4, alen))\n");
        out.push_str(&format!("mstore(_p, shl(224, {}))\n", selector));
        out.push_str("let blob := add(_p, 4)\n");
        out.push_str(&write_src);
        out.push_str("revert(_p, add(4, alen))\n");
        out.push_str("}\n");
        out
    }

    pub(crate) fn assert_failure_data(&self, msg: &Expr, ctx: &Ctx) -> String {
        let enum_call = match msg {
            Expr::MethodCall { base, method, args } => {
                Some((base, method.as_str(), args.as_slice()))
            }
            Expr::PropertyAccess { base, property } => {
                Some((base, property.as_str(), &[] as &[Expr]))
            }
            _ => None,
        };
        if let Some((base, method, args)) = enum_call {
            if let Expr::Identifier(enum_name) = &**base {
                if let Some(enum_decl) = self.type_checker().loaded_enums.get(enum_name) {
                    if let Some(variant) = enum_decl.variants.iter().find(|v| v.name == method) {
                        let abi_gen = AbiGenerator::new(self.type_checker());
                        let selector = abi_gen.calculate_error_selector(variant);
                        let types: Vec<Type> = variant
                            .parameters
                            .iter()
                            .map(|p| p.type_def.clone())
                            .collect();
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

        self.emit_revert_data(
            "assert failed",
            "0x08c379a0",
            std::slice::from_ref(msg),
            &[Type::Primitive("String".to_string())],
            ctx,
        )
    }

    pub(crate) fn fresh_local_bytes(&self, t: &Type) -> Option<usize> {
        match t {
            Type::FixedArray(..) | Type::Array(_) => Some(self.layout_engine.size_of(t)),
            Type::Primitive(n) if n == "String" || n == "Bytes" => Some(32),
            Type::Primitive(_)
                if is_struct_type(self.type_checker(), t)
                    || self.type_checker().is_payload_enum(t) =>
            {
                Some(self.layout_engine.size_of(t))
            }
            Type::Generic { name, .. } if self.type_checker().loaded_classes.contains_key(name) => {
                Some(self.layout_engine.size_of(&Type::Primitive(name.clone())))
            }
            _ => None,
        }
    }

    pub(crate) fn elem_is_inline(&self, t: &Type) -> bool {
        matches!(t, Type::FixedArray(..)) || is_struct_type(self.type_checker(), t)
    }

    pub(crate) fn mem_elem_stride(&self, inner: &Type) -> usize {
        if is_str_type(inner) {
            32
        } else {
            self.layout_engine.size_of(inner)
        }
    }

    pub(crate) fn array_elem_info(&self, base: &Expr, ctx: &Ctx) -> (bool, usize) {
        match self.static_type(base, ctx) {
            Type::Array(inner) => (true, self.mem_elem_stride(&inner)),
            Type::FixedArray(inner, _) => (false, self.mem_elem_stride(&inner)),
            _ => (false, 32),
        }
    }

    pub(crate) fn mem_array_addr(
        &self,
        base: &Expr,
        index_expr: &str,
        ctx: &Ctx,
    ) -> (String, usize) {
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

    pub(crate) fn translate_for_loop(
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

    pub(crate) fn resolve_storage_field_named(
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

    pub(crate) fn translate_delete(&self, target: &Expr, ctx: &Ctx) -> String {
        if let Some((slot, vty, tr)) = self.hashmap_entry_slot(target, ctx) {
            if is_str_type(&vty) {
                self.ensure_helper("gum_sstr_base", gum_sstr_base_helper_src);
                self.ensure_helper(&format!("gum_sstr_clear{}", kind_suffix(tr)), || {
                    gum_sstr_clear_helper_src(tr)
                });
                return format!("gum_sstr_clear{}({})\n", kind_suffix(tr), slot);
            }
        }
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

        if let Expr::Identifier(_) = target {
            let t = self.static_type(target, ctx);
            if let Some(bytes) = self.fresh_local_bytes(&t) {
                let p = self.translate_expr(target, ctx);
                let pv = format!("__delp_{}", self.next_literal_id());
                let mut out = format!("let {} := {}\n", pv, p);
                for i in 0..(bytes / 32) {
                    out.push_str(&format!("mstore(add({}, {}), 0)\n", pv, i * 32));
                }

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

    pub(crate) fn elem_type_of(&self, iterable: &Expr, ctx: &Ctx) -> Type {
        match self.static_type(iterable, ctx) {
            Type::Array(inner) | Type::FixedArray(inner, _) => *inner,
            _ => Type::Primitive("u256".to_string()),
        }
    }

    pub(crate) fn storage_for_loop(
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

    pub(crate) fn translate_saturate(&self, base: &Expr, ctx: &Ctx) -> String {
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

    pub(crate) fn translate_as_bytes(&self, base: &Expr, ctx: &Ctx) -> String {
        let val = self.translate_expr(base, ctx);
        self.ensure_helper("as_bytes_u256", || {
            "function as_bytes_u256(val) -> ptr {\n    ptr := allocate_memory(64)\n    mstore(ptr, 32)\n    mstore(add(ptr, 32), val)\n}\n".to_string()
        });
        format!("as_bytes_u256({})", val)
    }

    pub(crate) fn translate_to_string(&self, base: &Expr, ctx: &Ctx) -> String {
        let val = self.translate_expr(base, ctx);
        self.ensure_helper("gum_uint_to_str", gum_uint_to_str_helper_src);
        format!("gum_uint_to_str({})", val)
    }

    pub(crate) fn translate_scoped_try(
        &self,
        thunk: &str,
        args: &[(String, Type)],
        propagate_return: bool,
        writeback: &[(String, Type)],
        catch_body: &[Spanned<Statement>],
        ctx: &Ctx,
    ) -> String {
        let slot = crate::codegen::TRY_CAPABILITY_SLOT;

        let sig_decl = FnDecl {
            modifiers: Vec::new(),
            attributes: Vec::new(),
            name: thunk.to_string(),
            has_self: false,
            parameters: args
                .iter()
                .map(|(n, t)| Parameter {
                    is_mut: false,
                    type_def: t.clone(),
                    name: n.clone(),
                })
                .collect(),
            return_type: None,
            body: Vec::new(),
        };
        let selector = self.abi_gen.calculate_selector(&sig_decl);
        let types: Vec<Type> = args.iter().map(|(_, t)| t.clone()).collect();
        let arg_vals: Vec<String> = args.iter().map(|(n, _)| n.clone()).collect();

        let helper = format!("{}_call", thunk);
        let (size_src, write_src) = self.abi_arg_blob_src(&types);
        let (h, sel, sz, wr, slot_s) = (
            helper.clone(),
            selector.clone(),
            size_src,
            write_src,
            slot.to_string(),
        );
        let n = args.len();
        self.ensure_helper(&helper, move || {
            let params: Vec<String> = (0..n).map(|i| format!("a{}", i)).collect();
            let mut b = format!("function {}({}) -> ok {{\n", h, params.join(", "));
            b.push_str(&sz);
            b.push_str("    let ptr := allocate_memory(add(4, alen))\n");
            b.push_str(&format!("    mstore(ptr, shl(224, {}))\n", sel));
            b.push_str("    let blob := add(ptr, 4)\n");
            b.push_str(&wr);
            b.push_str(&format!("    tstore({}, 1)\n", slot_s));
            b.push_str("    ok := call(gas(), address(), 0, ptr, add(4, alen), 0, 0)\n");
            b.push_str(&format!("    tstore({}, 0)\n", slot_s));
            b.push_str("}\n");
            b
        });

        let id = self.next_literal_id();
        let ok = format!("__try_ok_{}", id);
        let mut out = format!("let {} := {}({})\n", ok, helper, arg_vals.join(", "));

        let mut propagate = String::new();
        propagate.push_str("    if gt(returndatasize(), 0) {\n");
        propagate.push_str(&format!(
            "        tstore({}, 1)\n",
            crate::codegen::TRY_RETURNED_SLOT
        ));
        if ctx.is_entry {
            if let Some(lock) = &ctx.lock_slot {
                propagate.push_str(&format!("        tstore({}, 0)\n", lock));
            }
            propagate.push_str("        let __rb := allocate_memory(returndatasize())\n");
            propagate.push_str("        returndatacopy(__rb, 0, returndatasize())\n");
            propagate.push_str("        return(__rb, returndatasize())\n");
        } else if let Some(rt) = &ctx.return_type {
            propagate.push_str(&self.returndata_to_var(rt, "ret"));
            propagate.push_str("        leave\n");
        }
        propagate.push_str("    }\n");

        let writeback_src = self.returndata_tuple_to_vars(writeback);

        if propagate_return && !writeback.is_empty() {
            out.push_str(&format!("if {} {{\n", ok));
            out.push_str(&format!(
                "switch tload({})\n",
                crate::codegen::TRY_RETURNED_SLOT
            ));
            out.push_str(&format!("case 1 {{\n{}}}\n", propagate));
            out.push_str(&format!("default {{\n{}}}\n", writeback_src));
            out.push_str("}\n");
        } else if propagate_return {
            out.push_str(&format!("if {} {{\n{}}}\n", ok, propagate));
        } else if !writeback.is_empty() {
            out.push_str(&format!("if {} {{\n{}}}\n", ok, writeback_src));
        }

        out.push_str(&format!("if iszero({}) {{\n", ok));
        for s in catch_body {
            out.push_str(&self.translate_statement(&s.node, ctx));
        }
        out.push_str("}\n");
        out
    }

    pub(crate) fn translate_set_sender(&self, addr: &str) -> String {
        const VM: &str = "0x7109709ECfa91a80626fF3989D68f67F5b1DD12D";
        self.ensure_helper("__vm_set_sender", move || {
            let mut b = String::from("function __vm_set_sender(a0) {\n");
            b.push_str("    let p := allocate_memory(36)\n");

            b.push_str("    mstore(p, shl(224, 0x06447d56))\n");
            b.push_str("    mstore(add(p, 4), a0)\n");
            b.push_str(&format!("    pop(call(gas(), {}, 0, p, 36, 0, 0))\n", VM));
            b.push_str("}\n");
            b
        });
        format!("__vm_set_sender({})\n", addr)
    }

    pub(crate) fn translate_abi_encode(&self, packed: bool, args: &[Expr], ctx: &Ctx) -> String {
        let types: Vec<Type> = args.iter().map(|a| self.static_type(a, ctx)).collect();
        let arg_strs: Vec<String> = args.iter().map(|a| self.translate_expr(a, ctx)).collect();
        let params: Vec<String> = (0..args.len()).map(|i| format!("a{}", i)).collect();
        let fn_name = format!(
            "__abi_{}_{}",
            if packed { "encode_packed" } else { "encode" },
            self.next_literal_id()
        );

        if packed {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
            let (types, params, name) = (types.clone(), params.clone(), fn_name.clone());
            self.ensure_helper(&fn_name, move || {
                let mut b = format!("function {}({}) -> ptr {{\n", name, params.join(", "));
                b.push_str("    let plen := 0\n");
                for (i, t) in types.iter().enumerate() {
                    match packed_width(t) {
                        Some(w) => b.push_str(&format!("    plen := add(plen, {})\n", w)),
                        None => {
                            b.push_str(&format!("    plen := add(plen, gum_str_len(a{}))\n", i))
                        }
                    }
                }

                b.push_str("    ptr := allocate_memory(add(64, plen))\n");
                b.push_str("    mstore(ptr, shl(192, plen))\n");
                b.push_str("    let cur := add(ptr, 32)\n");
                for (i, t) in types.iter().enumerate() {
                    match packed_width(t) {
                        Some(w) => {
                            let shift = (32 - w) * 8;
                            if shift == 0 {
                                b.push_str(&format!("    mstore(cur, a{})\n", i));
                            } else {
                                b.push_str(&format!("    mstore(cur, shl({}, a{}))\n", shift, i));
                            }
                            b.push_str(&format!("    cur := add(cur, {})\n", w));
                        }
                        None => {
                            b.push_str(&format!("    let n{i} := gum_str_len(a{i})\n", i = i));
                            b.push_str(&format!(
                                "    gum_memory_copy(add(a{i}, 32), cur, n{i})\n",
                                i = i
                            ));
                            b.push_str(&format!("    cur := add(cur, n{})\n", i));
                        }
                    }
                }
                b.push_str("}\n");
                b
            });
        } else {
            let (size_src, write_src) = self.abi_arg_blob_src(&types);
            let (params, name) = (params.clone(), fn_name.clone());
            self.ensure_helper(&fn_name, move || {
                let mut b = format!("function {}({}) -> ptr {{\n", name, params.join(", "));
                b.push_str(&size_src);
                b.push_str("    ptr := allocate_memory(add(32, alen))\n");
                b.push_str("    mstore(ptr, shl(192, alen))\n");
                b.push_str("    let blob := add(ptr, 32)\n");
                b.push_str(&write_src);
                b.push_str("}\n");
                b
            });
        }
        format!("{}({})", fn_name, arg_strs.join(", "))
    }

    pub(crate) fn translate_string_literal(&self, s: &str) -> String {
        let fn_name = format!("__strlit_{}", self.next_literal_id());
        let mut body = String::new();
        body.push_str(&format!("function {}() -> ptr {{\n", fn_name));
        body.push_str(&str_literal_body_src("ptr", s));
        body.push_str("}\n");
        self.ensure_helper(&fn_name, || body);
        format!("{}()", fn_name)
    }

    pub(crate) fn translate_array_literal(
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

    pub(crate) fn translate_fstring(&self, segments: &[FStringSegment], ctx: &Ctx) -> String {
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
}
