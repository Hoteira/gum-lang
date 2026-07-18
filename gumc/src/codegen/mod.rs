pub mod layout;
pub mod mutability;
pub mod translator;
pub mod abi;

use crate::ast::*;
use crate::semantic::TypeChecker;
use layout::{immutable_key, LayoutEngine};
use translator::{immutable_deploy_local, immutable_local, is_enum_type, is_str_type, is_struct_type, Ctx, SelfCtx, Translator};
use abi::AbiGenerator;
use std::collections::{HashMap, HashSet};

pub trait Backend {
    fn generate(&mut self, program: &Program, type_checker: &TypeChecker) -> Result<Vec<(String, String, String)>, String>;
}

// Whether a type is an EVM address (160-bit) for masking purposes. Account is
// the stdlib's address type; its single u256 address field is a storage
// convenience, but on-chain it names a 20-byte account, so it's masked to 160
pub(crate) fn is_address_type(t: &Type) -> bool {
    matches!(t, Type::Primitive(name) if name == "Account")
}

fn hash_slot(key: &str) -> String {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(key.as_bytes());
    k.finalize(&mut out);
    let mut s = String::from("0x");
    for b in out {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// Storage slot for a once function's has-run flag. Derived from keccak of the
// function name so it can never collide with a normal field's slot (0, 1, 2, …)
// the same trick namespaced storage uses.
fn once_flag_slot(fn_name: &str) -> String {
    hash_slot(&format!("gum.once:{}", fn_name))
}

fn reentrancy_lock_slot() -> String {
    hash_slot("gum.reentrancy")
}

// A short, Yul-identifier-safe name for a concrete type, used to build
// specialized function names for monomorphized generic classes
// (e.g. Vec(u256)'s push becomes Vec_u256_push).
pub fn type_suffix(t: &Type) -> String {
    match t {
        Type::Primitive(name) => name.clone(),
        Type::Array(inner) => format!("Arr{}", type_suffix(inner)),
        Type::FixedArray(inner, n) => format!("FArr{}x{}", type_suffix(inner), n),
        Type::Generic { name, args } => format!("{}{}", name, generic_suffix(args)),
    }
}

pub fn generic_suffix(args: &[Type]) -> String {
    args.iter().map(type_suffix).collect::<Vec<_>>().join("_")
}

// Replaces occurrences of a generic class's own parameter names (e.g. Vec's
// T, HashMap's K/V) with the concrete types from one specific
// instantiation. Only applied to a method's parameter/return types, method
fn substitute_type(t: &Type, subst: &HashMap<String, Type>) -> Type {
    match t {
        Type::Primitive(name) => subst.get(name).cloned().unwrap_or_else(|| t.clone()),
        Type::Array(inner) => Type::Array(Box::new(substitute_type(inner, subst))),
        Type::FixedArray(inner, n) => Type::FixedArray(Box::new(substitute_type(inner, subst)), *n),
        Type::Generic { name, args } => Type::Generic {
            name: name.clone(),
            args: args.iter().map(|a| substitute_type(a, subst)).collect(),
        },
    }
}

fn substitute_method(m: &FnDecl, subst: &HashMap<String, Type>) -> FnDecl {
    FnDecl {
        modifiers: m.modifiers.clone(),
        name: m.name.clone(),
        parameters: m.parameters.iter().map(|p| Parameter {
            is_mut: p.is_mut,
            type_def: substitute_type(&p.type_def, subst),
            name: p.name.clone(),
        }).collect(),
        return_type: m.return_type.as_ref().map(|t| substitute_type(t, subst)),
        body: m.body.clone(),
    }
}

fn note_generic(t: &Type, found: &mut HashMap<String, Vec<Vec<Type>>>) {
    match t {
        Type::Generic { name, args } => {
            let entry = found.entry(name.clone()).or_default();
            let key = format!("{:?}", args);
            if !entry.iter().any(|a| format!("{:?}", a) == key) {
                entry.push(args.clone());
            }
            for a in args {
                note_generic(a, found);
            }
        }
        Type::Array(inner) | Type::FixedArray(inner, _) => note_generic(inner, found),
        _ => {}
    }
}

fn scan_stmts_for_generics(stmts: &[Spanned<Statement>], found: &mut HashMap<String, Vec<Vec<Type>>>) {
    for s in stmts {
        match &s.node {
            Statement::VarDecl { type_def, .. } => note_generic(type_def, found),
            Statement::IfElse { if_body, else_body, .. } => {
                scan_stmts_for_generics(if_body, found);
                if let Some(eb) = else_body {
                    scan_stmts_for_generics(eb, found);
                }
            }
            Statement::WhileLoop { body, .. } => scan_stmts_for_generics(body, found),
            Statement::ForLoop { body, .. } => scan_stmts_for_generics(body, found),
            Statement::Match { arms, .. } => {
                for arm in arms {
                    scan_stmts_for_generics(&arm.body, found);
                }
            }
            _ => {}
        }
    }
}

// Finds every concrete instantiation of every generic class actually used in
// the program (class fields, function/method signatures, and var_decls in
// any body, including nested blocks) so each one can get its own compiled
fn collect_generic_instantiations(program: &Program, type_checker: &TypeChecker) -> HashMap<String, Vec<Vec<Type>>> {
    let mut found: HashMap<String, Vec<Vec<Type>>> = HashMap::new();
    for class_decl in type_checker.loaded_classes.values() {
        for f in &class_decl.fields {
            note_generic(&f.type_def, &mut found);
        }
        // Deliberately not scanning method parameter/return types here: a
        // Deliberately not scanning method parameter/return types here: a
        for m in &class_decl.methods {
            scan_stmts_for_generics(&m.body, &mut found);
        }
    }
    for decl in &program.declarations {
        if let Declaration::Function(f) = decl {
            for p in &f.parameters {
                note_generic(&p.type_def, &mut found);
            }
            if let Some(rt) = &f.return_type {
                note_generic(rt, &mut found);
            }
            scan_stmts_for_generics(&f.body, &mut found);
        }
    }
    found
}

// Every contract these method bodies deploy with new X(...), in first-seen
// order. Each one's creation code has to be embedded as a sub-object of the
// deployer, so codegen needs to know the set before it emits the parent.
fn collect_deployed_contracts(methods: &[FnDecl], tc: &TypeChecker, out: &mut Vec<String>) {
    fn expr(e: &Expr, tc: &TypeChecker, out: &mut Vec<String>) {
        match e {
            Expr::Instantiation { type_def, args } => {
                if let Type::Primitive(n) = type_def {
                    let is_contract = tc.loaded_classes.get(n).map(|c| c.is_global).unwrap_or(false);
                    if is_contract && !out.iter().any(|x| x == n) {
                        out.push(n.clone());
                    }
                }
                for a in args {
                    expr(a, tc, out);
                }
            }
            Expr::FnCall { args, .. } => args.iter().for_each(|a| expr(a, tc, out)),
            Expr::MethodCall { base, args, .. } => {
                expr(base, tc, out);
                args.iter().for_each(|a| expr(a, tc, out));
            }
            Expr::PropertyAccess { base, .. } => expr(base, tc, out),
            Expr::IndexAccess { base, index } => {
                expr(base, tc, out);
                expr(index, tc, out);
            }
            Expr::BinaryOp { left, right, .. } => {
                expr(left, tc, out);
                expr(right, tc, out);
            }
            Expr::Neg(i) | Expr::Not(i) => expr(i, tc, out),
            Expr::ArrayLiteral(xs) => xs.iter().for_each(|x| expr(x, tc, out)),
            Expr::FString(segs) => segs.iter().for_each(|s| {
                if let FStringSegment::Interp(x) = s {
                    expr(x, tc, out)
                }
            }),
            Expr::Number(_) | Expr::StringLiteral(_) | Expr::Identifier(_) => {}
        }
    }
    fn stmt(s: &Statement, tc: &TypeChecker, out: &mut Vec<String>) {
        let body = |b: &[Spanned<Statement>], out: &mut Vec<String>| {
            for s in b {
                stmt(&s.node, tc, out);
            }
        };
        match s {
            Statement::VarDecl { value, .. } => {
                if let Some(v) = value {
                    expr(v, tc, out)
                }
            }
            Statement::Assignment { target, value } => {
                expr(target, tc, out);
                expr(value, tc, out);
            }
            Statement::Delete { target } => expr(target, tc, out),
            Statement::Assert { condition, message } => {
                expr(condition, tc, out);
                if let Some(m) = message {
                    expr(m, tc, out)
                }
            }
            Statement::Revert { error } => expr(error, tc, out),
            Statement::Return { value } => {
                if let Some(v) = value {
                    expr(v, tc, out)
                }
            }
            Statement::IfElse { condition, if_body, else_body } => {
                expr(condition, tc, out);
                body(if_body, out);
                if let Some(eb) = else_body {
                    body(eb, out)
                }
            }
            Statement::ForLoop { iterable, body: b, .. } => {
                expr(iterable, tc, out);
                body(b, out);
            }
            Statement::WhileLoop { condition, body: b } => {
                expr(condition, tc, out);
                body(b, out);
            }
            Statement::Match { expr: e, arms } => {
                expr(e, tc, out);
                for a in arms {
                    body(&a.body, out)
                }
            }
            Statement::Expression(e) => expr(e, tc, out),
            Statement::Call { args, .. } => args.iter().for_each(|a| expr(a, tc, out)),
            Statement::BitwiseFlip { index, value, .. } => {
                expr(index, tc, out);
                expr(value, tc, out);
            }
            Statement::UnsafeBlock(_) => {}
            Statement::TryCatch { try_body, catch_body } => {
                body(try_body, out);
                body(catch_body, out);
            }
        }
    }
    for m in methods {
        for s in &m.body {
            stmt(&s.node, tc, out);
        }
    }
}

// A top-level function is externally callable iff it is marked export.
// Everything else is an internal helper.
fn is_exported(f: &FnDecl) -> bool {
    f.modifiers.iter().any(|m| m == "export")
}

// A payable function may receive ETH; every other entry point rejects any
// value-bearing call.
fn is_payable(f: &FnDecl) -> bool {
    f.modifiers.iter().any(|m| m == "payable")
}

fn is_unsafe(f: &FnDecl) -> bool {
    f.modifiers.iter().any(|m| m == "unsafe")
}

// receive and fallback are entry points reached by shape of the call
// rather than by selector, so they never appear in the dispatcher's switch and
// have no ABI selector of their own:
fn is_receive(f: &FnDecl) -> bool {
    is_exported(f) && f.name == "receive"
}

fn is_fallback(f: &FnDecl) -> bool {
    is_exported(f) && f.name == "fallback"
}

fn is_selector_entry(f: &FnDecl) -> bool {
    is_exported(f) && !is_receive(f) && !is_fallback(f)
}

// Message/Block params are synthesized from opcodes at the call
// boundary rather than decoded from calldata (or from constructor args), so
// they never occupy a slot in the ABI head.
fn is_context_param(t: &Type) -> bool {
    matches!(t, Type::Primitive(n) if n == "Message" || n == "Block")
}


fn compile_class_methods(
    yul: &mut String,
    translator: &Translator,
    class_name: &str,
    suffix: &str,
    is_global: bool,
    methods: &[FnDecl],
) {
    for method in methods {
        let self_ctx = SelfCtx { class_name: class_name.to_string(), is_global };
        let is_ctor = is_global && method.name == "new";
        let method_ctx = Ctx::helper(Some(&self_ctx))
            .with_return_type(method.return_type.clone())
            .in_constructor(is_ctor);
        for p in &method.parameters {
            method_ctx.declare(&p.name, &p.type_def);
        }

        let mut params: Vec<String> = Vec::new();
        if !is_global {
            params.push("self".to_string());
        }
        params.extend(method.parameters.iter().map(|p| p.name.clone()));

        let fn_name = if suffix.is_empty() {
            format!("{}_{}", class_name, method.name)
        } else {
            format!("{}_{}_{}", class_name, suffix, method.name)
        };

        let immutables: Vec<String> = if is_ctor {
            translator.layout_engine.patched_immutables(class_name)
        } else {
            Vec::new()
        };

        let mut rets: Vec<String> = Vec::new();
        if method.return_type.is_some() {
            rets.push("ret".to_string());
        }
        rets.extend(immutables.iter().map(|f| immutable_local(f)));

        let signature = if rets.is_empty() {
            format!("function {}({}) {{\n", fn_name, params.join(", "))
        } else {
            format!("function {}({}) -> {} {{\n", fn_name, params.join(", "), rets.join(", "))
        };
        yul.push_str(&format!("      {}", signature));

        for stmt in &method.body {
            let stmt_code = translator.translate_statement(&stmt.node, &method_ctx);
            for line in stmt_code.lines() {
                yul.push_str(&format!("          {}\n", line));
            }
        }
        yul.push_str("      }\n\n");
    }
}

pub struct EvmYulBackend {
    pub rich_reverts: bool,
    pub lock_path: Option<String>,
}

impl EvmYulBackend {
    pub fn new(rich_reverts: bool) -> Self {
        Self { rich_reverts, lock_path: None }
    }

    pub fn with_lock(rich_reverts: bool, lock_path: Option<String>) -> Self {
        Self { rich_reverts, lock_path }
    }
}

impl Backend for EvmYulBackend {
    fn generate(&mut self, program: &Program, type_checker: &TypeChecker) -> Result<Vec<(String, String, String)>, String> {
        println!("--> [Codegen] Generating EVM Yul...");
        let lock_in = match &self.lock_path {
            Some(p) => layout::StorageManifest::load(p)?,
            None => None,
        };
        let had_lock = lock_in.is_some();
        let layout_engine = LayoutEngine::with_lock(type_checker, lock_in)?;
        if let Some(p) = &self.lock_path {
            layout_engine.manifest_out.save(p)?;
            if had_lock {
                println!("    [Storage Lock] Honored committed layout from {} (existing fields pinned).", p);
            } else {
                println!("    [Storage Lock] Wrote new storage lock to {}, commit it; future compiles will keep this layout.", p);
            }
        }
        let abi_gen = AbiGenerator::new(type_checker);

        let global_classes: Vec<ClassDecl> = program.declarations.iter().filter_map(|d| {
            if let Declaration::Class(c) = d {
                if c.is_global && c.name != "Message" && c.name != "Block" {
                    return Some(type_checker.loaded_classes.get(&c.name).unwrap_or(c).clone());
                }
            }
            None
        }).collect();

        let mut compiled_contracts = Vec::new();

        for global_class in &global_classes {
            let mut stack: Vec<String> = Vec::new();
            let (yul, abi) = self.build_contract_object(
                global_class,
                program,
                type_checker,
                &layout_engine,
                &abi_gen,
                &mut stack,
            )?;
            compiled_contracts.push((global_class.name.clone(), yul, abi));
        }

        Ok(compiled_contracts)
    }
}

impl EvmYulBackend {
    fn build_contract_object(
        &self,
        global_class: &ClassDecl,
        program: &Program,
        type_checker: &TypeChecker,
        layout_engine: &LayoutEngine,
        abi_gen: &AbiGenerator,
        stack: &mut Vec<String>,
    ) -> Result<(String, String), String> {
        {
            let global_class_name = global_class.name.clone();

            if stack.iter().any(|s| *s == global_class_name) {
                stack.push(global_class_name.clone());
                return Err(format!(
                    "Deployment cycle: {}. A contract's creation code is embedded in whatever deploys it, so this would be infinitely large. Deploy one of them from outside, and pass its address in.",
                    stack.join(" -> ")
                ));
            }
            stack.push(global_class_name.clone());

            let mut deployed: Vec<String> = Vec::new();
            collect_deployed_contracts(&global_class.methods, type_checker, &mut deployed);
            let mut nested_objects = String::new();
            for child_name in &deployed {
                let child = type_checker
                    .loaded_classes
                    .get(child_name)
                    .ok_or_else(|| format!("new {}(...): no such contract", child_name))?
                    .clone();
                let (child_yul, _) =
                    self.build_contract_object(&child, program, type_checker, layout_engine, abi_gen, stack)?;
                for line in child_yul.lines() {
                    if line.is_empty() {
                        nested_objects.push('\n');
                    } else {
                        nested_objects.push_str("    ");
                        nested_objects.push_str(line);
                        nested_objects.push('\n');
                    }
                }
            }
            
            let top_level_fns: HashSet<String> = global_class.methods.iter()
                .map(|f| f.name.clone())
                .collect();

            let mut yul = format!("object \"{}\" {{
", global_class_name);
            yul.push_str("  code {
");
            yul.push_str("    // --- Deployment Code ---
");
            
            let translator = Translator::new(&layout_engine, &abi_gen, &top_level_fns, self.rich_reverts);
            
            let constructor_decl = type_checker
                .loaded_classes
                .get(&global_class_name)
                .unwrap_or(&global_class)
                .methods
                .iter()
                .find(|m| m.name == "new")
                .cloned();

        if let Some(constructor) = &constructor_decl {
            let head_bytes: usize = constructor.parameters.iter()
                .filter(|p| !is_context_param(&p.type_def))
                .map(|p| translator.abi_head_bytes(&p.type_def))
                .sum();

            if head_bytes > 0 {
                yul.push_str("    // --- Constructor Arguments ---\n");
                yul.push_str(&format!("    let _prog := datasize(\"{}\")\n", global_class_name));
                yul.push_str("    let _args_len := sub(codesize(), _prog)\n");
                yul.push_str(&format!("    if lt(_args_len, {}) {{ revert(0, 0) }}\n", head_bytes));
                yul.push_str("    let args_mem := allocate_memory(_args_len)\n");
                yul.push_str("    codecopy(args_mem, _prog, _args_len)\n");
            }

            let mut arg_names = Vec::new();
            let mut offset = 0;
            for p in &constructor.parameters {
                let mut is_context = false;
                let arg_name = format!("param_{}", p.name);
                arg_names.push(arg_name.clone());

                if let Type::Primitive(name) = &p.type_def {
                    if name == "Message" {
                        yul.push_str(&format!("    let {} := allocate_memory(64)\n", arg_name));
                        yul.push_str(&format!("    mstore({}, caller())\n", arg_name));
                        yul.push_str(&format!("    mstore(add({}, 32), callvalue())\n", arg_name));
                        is_context = true;
                    } else if name == "Block" {
                        yul.push_str(&format!("    let {} := allocate_memory(64)\n", arg_name));
                        yul.push_str(&format!("    mstore({}, timestamp())\n", arg_name));
                        yul.push_str(&format!("    mstore(add({}, 32), number())\n", arg_name));
                        is_context = true;
                    }
                }

                if !is_context {
                    if is_str_type(&p.type_def) {
                        yul.push_str(&format!("    let {}_head := mload(add(args_mem, {}))\n", arg_name, offset));
                        yul.push_str(&format!("    if gt(add({}_head, 32), _args_len) {{ revert(0, 0) }}\n", arg_name));
                        yul.push_str(&format!("    let {}_len := mload(add(args_mem, {}_head))\n", arg_name, arg_name));
                        yul.push_str(&format!("    if gt(add(add({}_head, 32), {}_len), _args_len) {{ revert(0, 0) }}\n", arg_name, arg_name));
                        yul.push_str(&format!("    let {} := allocate_memory(add(32, {}_len))\n", arg_name, arg_name));
                        yul.push_str(&format!("    mstore({}, shl(192, {}_len))\n", arg_name, arg_name));
                        yul.push_str(&format!(
                            "    gum_memory_copy(add(add(args_mem, {}_head), 32), add({}, 32), {}_len)\n",
                            arg_name, arg_name, arg_name
                        ));
                        offset += 32;
                        continue;
                    }
                    // Same as the dispatcher: one wire word holding the tag, rebuilt into the [tag][payload] pair the body expects.
                    if is_enum_type(layout_engine.type_checker, &p.type_def) {
                        yul.push_str(&format!(
                            "    let {} := and(mload(add(args_mem, {})), 0xff)\n",
                            arg_name, offset
                        ));
                        offset += 32;
                        continue;
                    }
                    if let Type::Primitive(name) = &p.type_def {
                        if is_struct_type(layout_engine.type_checker, &p.type_def) {
                            if let Some((helper, wire)) = translator.ensure_abi_struct_mem(name) {
                                yul.push_str(&format!(
                                    "    let {} := {}(args_mem, {}, _args_len)\n",
                                    arg_name, helper, offset
                                ));
                                offset += wire;
                                continue;
                            }
                        }
                    }
                    // The dispatcher's array path, reading the blob the creation code appended instead of calldata.
                    if matches!(&p.type_def, Type::Array(_) | Type::FixedArray(..)) {
                        if let Some(helper) = translator.ensure_abi_mem(&p.type_def) {
                            if translator.abi_is_dynamic(&p.type_def) {
                                yul.push_str(&format!(
                                    "    let {} := {}(args_mem, mload(add(args_mem, {})), _args_len)\n",
                                    arg_name, helper, offset
                                ));
                                offset += 32;
                            } else {
                                yul.push_str(&format!(
                                    "    let {} := {}(args_mem, {}, _args_len)\n",
                                    arg_name, helper, offset
                                ));
                                offset += translator.abi_head_bytes(&p.type_def);
                            }
                            continue;
                        }
                    }
                    let size = layout_engine.size_of(&p.type_def);
                    if size <= 32 {
                        let loaded = format!("mload(add(args_mem, {}))", offset);
                        let mut masked = translator.mask_for_type(&loaded, &p.type_def);
                        if is_address_type(&p.type_def) {
                            masked = format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", masked);
                        }
                        yul.push_str(&format!("    let {} := {}\n", arg_name, masked));
                        offset += 32;
                    } else {
                        yul.push_str(&format!("    let {} := add(args_mem, {})\n", arg_name, offset));
                        offset += size;
                    }
                }
            }

            let imm_names: Vec<String> = layout_engine
                .patched_immutables(&global_class_name)
                .iter()
                .map(|f| immutable_deploy_local(f))
                .collect();
            let binding = if imm_names.is_empty() {
                String::new()
            } else {
                format!("let {} := ", imm_names.join(", "))
            };
            yul.push_str(&format!(
                "    {}{}_new({})\n",
                binding,
                global_class_name,
                arg_names.join(", ")
            ));
        }

        let runtime_obj = format!("{}_runtime", global_class_name);

        yul.push_str("    // Copy runtime code to memory and return it\n");
        yul.push_str(&format!(
            "    datacopy(0, dataoffset(\"{r}\"), datasize(\"{r}\"))\n",
            r = runtime_obj
        ));
        for f in layout_engine.patched_immutables(&global_class_name) {
            yul.push_str(&format!(
                "    setimmutable(0, \"{}\", {})\n",
                immutable_key(&global_class_name, &f),
                immutable_deploy_local(&f)
            ));
        }
        yul.push_str(&format!("    return(0, datasize(\"{r}\"))\n", r = runtime_obj));

        yul.push_str("  }\n"); // End of deployment block

        yul.push_str(&format!("  object \"{}\" {{\n", runtime_obj));
        yul.push_str("    code {\n");

        yul.push_str("      // --- Function Dispatcher ---\n");

        let has_ext = type_checker.has_external_calls.get();
        let muts = mutability::analyze_class(type_checker, global_class);
        // Per-function refinement of has_ext: a state-changing entry point that never hands control away cannot be re-entered, so it needs no lock. Gated behind the contract-wide flag, so a guard is only ever dropped, never added.
        let ext = mutability::analyze_external_calls(type_checker, global_class);

        let find_fn = |pred: fn(&FnDecl) -> bool| -> Option<&FnDecl> {
            global_class.methods.iter().find(|f| pred(f))
        };
        let receive_fn = find_fn(is_receive);
        let fallback_fn = find_fn(is_fallback);

        let any_payable = global_class.methods.iter().any(|f| is_exported(f) && is_payable(f));

        let invoke_bare = |yul: &mut String, f: &FnDecl, indent: &str| {
            let guarded = has_ext
                && mutability::makes_external_call(f, &ext)
                && !is_unsafe(f)
                && !mutability::is_read_only(f, &muts);
            if guarded {
                yul.push_str(&format!("{}if tload({}) {{ revert(0, 0) }}\n", indent, reentrancy_lock_slot()));
                yul.push_str(&format!("{}tstore({}, 1)\n", indent, reentrancy_lock_slot()));
            }
            if any_payable && !is_payable(f) {
                yul.push_str(&format!("{}if callvalue() {{ revert(0, 0) }}\n", indent));
            }
            yul.push_str(&format!("{}{}_impl()\n", indent, f.name));
            if guarded {
                yul.push_str(&format!("{}tstore({}, 0)\n", indent, reentrancy_lock_slot()));
            }
            yul.push_str(&format!("{}return(0, 0)\n", indent));
        };

        let empty_target = receive_fn.or(fallback_fn);
        if let Some(f) = empty_target {
            yul.push_str("      if iszero(calldatasize()) {\n");
            invoke_bare(&mut yul, f, "          ");
            yul.push_str("      }\n");
        }

        match fallback_fn {
            Some(f) => {
                yul.push_str("      if lt(calldatasize(), 4) {\n");
                invoke_bare(&mut yul, f, "          ");
                yul.push_str("      }\n");
            }
            None => yul.push_str("      if lt(calldatasize(), 4) { revert(0, 0) }\n"),
        }

        yul.push_str("      let selector := shr(224, calldataload(0))\n");

        if !any_payable {
            yul.push_str("      if callvalue() { revert(0, 0) }\n");
        }

        yul.push_str("      switch selector\n");

        for f in &global_class.methods {
            if true {
                if !is_selector_entry(f) { continue; }
                let selector = abi_gen.calculate_selector(f);
                yul.push_str(&format!("      case {} /* {} */ {{\n", selector, f.name));

                // A read-only function gets no guard, for two reasons that agree.
                // It cannot be harmed by reentrancy, since it writes nothing. And the ABI calls it view, which invites callers to use eth_call: the guard's tstore would revert inside that STATICCALL, making the getter uncallable.
                let requires_guard = has_ext
                    && mutability::makes_external_call(f, &ext)
                    && !is_unsafe(f)
                    && !mutability::is_read_only(f, &muts);
                if requires_guard {
                    yul.push_str(&format!("          if tload({}) {{ revert(0, 0) }}\n", reentrancy_lock_slot()));
                    yul.push_str(&format!("          tstore({}, 1)\n", reentrancy_lock_slot()));
                }

                if any_payable && !is_payable(f) {
                    yul.push_str("          if callvalue() { revert(0, 0) }\n");
                }

                let mut expected_cd: usize = 4;
                for p in &f.parameters {
                    if is_context_param(&p.type_def) {
                        continue;
                    }
                    expected_cd += translator.abi_head_bytes(&p.type_def);
                }
                if expected_cd > 4 {
                    yul.push_str(&format!("          if lt(calldatasize(), {}) {{ revert(0, 0) }}\n", expected_cd));
                }

                let mut arg_names = Vec::new();
                let mut offset = 4;
                for p in &f.parameters {
                    let mut is_context = false;
                    let arg_name = format!("param_{}", p.name);
                    arg_names.push(arg_name.clone());

                    if let Type::Primitive(name) = &p.type_def {
                        if name == "Message" {
                            yul.push_str(&format!("          let {} := allocate_memory(64)\n", arg_name));
                            yul.push_str(&format!("          mstore({}, caller())\n", arg_name));
                            yul.push_str(&format!("          mstore(add({}, 32), callvalue())\n", arg_name));
                            is_context = true;
                        } else if name == "Block" {
                            yul.push_str(&format!("          let {} := allocate_memory(64)\n", arg_name));
                            yul.push_str(&format!("          mstore({}, timestamp())\n", arg_name));
                            yul.push_str(&format!("          mstore(add({}, 32), number())\n", arg_name));
                            is_context = true;
                        }
                    }

                    if !is_context {
                        if let Type::Primitive(name) = &p.type_def {
                            if name == "String" || name == "Bytes" {
                                yul.push_str(&format!("          let {}_head := calldataload({})\n", arg_name, offset));
                                yul.push_str(&format!("          let {}_data_offset := add(4, {}_head)\n", arg_name, arg_name));
                                yul.push_str(&format!("          let {}_len := calldataload({}_data_offset)\n", arg_name, arg_name));
                                yul.push_str(&format!("          let {} := allocate_memory(add(32, {}_len))\n", arg_name, arg_name));
                                yul.push_str(&format!("          mstore({}, shl(192, {}_len))\n", arg_name, arg_name));
                                yul.push_str(&format!("          calldatacopy(add({}, 32), add({}_data_offset, 32), {}_len)\n", arg_name, arg_name, arg_name));
                                offset += 32;
                                continue;
                            }
                        }

                        // An enum is one uint8 word on the wire holding the tag, but a pointer to [tag][payload] in memory, so it is rebuilt rather than copied.
                        // Copying size_of(enum) = 64 bytes instead read the next argument as the payload and then read every later one past the end of calldata as zero.
                        if is_enum_type(layout_engine.type_checker, &p.type_def) {
                            yul.push_str(&format!(
                                "          let {} := and(calldataload({}), 0xff)\n",
                                arg_name, offset
                            ));
                            offset += 32;
                            continue;
                        }

                        // A static struct is inline in the head, so it advances the cursor by its whole wire width rather than the one word an offset would take.
                        if let Type::Primitive(name) = &p.type_def {
                            if is_struct_type(layout_engine.type_checker, &p.type_def) {
                                if let Some((helper, wire)) = translator.ensure_abi_struct_cd(name) {
                                    yul.push_str(&format!("          let {} := {}({})\n", arg_name, helper, offset));
                                    offset += wire;
                                    continue;
                                }
                            }
                        }

                        // Every array shape resolves through one codec lookup, which recurses into its element, so a nested array decodes by the same path a flat one does.
                        // A dynamic value sits behind an offset word and is decoded at add(4, that offset); a static one is inline and is decoded where the cursor already points.
                        if matches!(&p.type_def, Type::Array(_) | Type::FixedArray(..)) {
                            if let Some(helper) = translator.ensure_abi_cd(&p.type_def) {
                                if translator.abi_is_dynamic(&p.type_def) {
                                    yul.push_str(&format!(
                                        "          let {} := {}(add(4, calldataload({})))\n",
                                        arg_name, helper, offset
                                    ));
                                    offset += 32;
                                } else {
                                    yul.push_str(&format!("          let {} := {}({})\n", arg_name, helper, offset));
                                    offset += translator.abi_head_bytes(&p.type_def);
                                }
                                continue;
                            }
                        }

                        let size = layout_engine.size_of(&p.type_def);
                        if size <= 32 {
                            let loaded = format!("calldataload({})", offset);
                            let mut masked = translator.mask_for_type(&loaded, &p.type_def);
                            if is_address_type(&p.type_def) {
                                masked = format!("and({}, 0xffffffffffffffffffffffffffffffffffffffff)", masked);
                            }
                            yul.push_str(&format!("          let {} := {}\n", arg_name, masked));
                            offset += 32;
                        } else {
                            yul.push_str(&format!("          let {} := allocate_memory({})\n", arg_name, size));
                            yul.push_str(&format!("          calldatacopy({}, {}, {})\n", arg_name, offset, size));
                            offset += size;
                        }
                    }
                }

                if arg_names.is_empty() {
                    yul.push_str(&format!("          {}_impl()\n", f.name));
                } else {
                    yul.push_str(&format!("          {}_impl({})\n", f.name, arg_names.join(", ")));
                }

                if requires_guard {
                    yul.push_str(&format!("          tstore({}, 0)\n", reentrancy_lock_slot()));
                }
                yul.push_str("          return(0, 0)\n");

                yul.push_str("      }\n");
            }
        }

        match fallback_fn {
            Some(f) => {
                yul.push_str("      default {\n");
                invoke_bare(&mut yul, f, "          ");
                yul.push_str("      }\n\n");
            }
            None => yul.push_str("      default { revert(0, 0) }\n\n"),
        }

        yul.push_str("    }\n");
        yul.push_str("  }\n"); // End of runtime object
        
        
        let mut shared_functions = String::new();

        let entry_self = SelfCtx { class_name: global_class_name.clone(), is_global: true };

        for f in &global_class.methods {
            if f.name == "new" {
                continue;
            }
            {
                // Must agree with the dispatcher's guard decision above: this is the other half of the same lock, the clear on the return path.
                // A read-only function takes neither, or its body would still TSTORE and revert under the STATICCALL its view invites.
                let requires_guard = has_ext
                    && mutability::makes_external_call(f, &ext)
                    && !is_unsafe(f)
                    && is_exported(f)
                    && !mutability::is_read_only(f, &muts);
                let lock_slot = if requires_guard { Some(reentrancy_lock_slot()) } else { None };

                let entry_ctx = Ctx::entry(lock_slot)
                    .with_self(Some(&entry_self))
                    .with_return_type(f.return_type.clone());
                for p in &f.parameters {
                    entry_ctx.declare(&p.name, &p.type_def);
                }

                let param_names: Vec<String> = f.parameters.iter().map(|p| p.name.clone()).collect();
                if param_names.is_empty() {
                    shared_functions.push_str(&format!("      function {}_impl() {{\n", f.name));
                } else {
                    shared_functions.push_str(&format!("      function {}_impl({}) {{\n", f.name, param_names.join(", ")));
                }

                if f.modifiers.iter().any(|m| m == "once") {
                    let slot = once_flag_slot(&f.name);
                    shared_functions.push_str(&format!("          if sload({}) {{ revert(0, 0) }}\n", slot));
                    shared_functions.push_str(&format!("          sstore({}, 1)\n", slot));
                }

                for stmt in &f.body {
                    let stmt_code = translator.translate_statement(&stmt.node, &entry_ctx);
                    for line in stmt_code.lines() {
                        shared_functions.push_str(&format!("          {}\n", line));
                    }
                }
                shared_functions.push_str("      }\n\n");
            }
        }

        shared_functions.push_str("      function Message_sender() -> ret {\n          ret := caller()\n      }\n\n");
        shared_functions.push_str("      function Message_value() -> ret {\n          ret := callvalue()\n      }\n\n");
        shared_functions.push_str("      function Message_address() -> ret {\n          ret := address()\n      }\n\n");
        shared_functions.push_str("      function Block_timestamp() -> ret {\n          ret := timestamp()\n      }\n\n");
        shared_functions.push_str("      function Block_number() -> ret {\n          ret := number()\n      }\n\n");

        let instantiations = collect_generic_instantiations(program, type_checker);
        // Walk class_order, the order classes were registered in, rather than loaded_classes directly.
        // loaded_classes is a HashMap and Rust randomizes its iteration per process, so iterating it here emitted these functions in a different order on every run: same source, different (equivalent) bytecode, and no way to verify a deployed contract against its source.
        // layout.rs already walks class_order for the same reason; this is the other half of it.
        let ordered: Vec<(&String, &ClassDecl)> = type_checker
            .class_order
            .iter()
            .filter_map(|n| type_checker.loaded_classes.get(n).map(|c| (n, c)))
            .collect();
        for (class_name, class_decl) in ordered {
            if type_checker.loaded_interfaces.contains(class_name) || class_name == "Message" || class_name == "Block" {
                continue;
            }
            if class_decl.is_global && class_name != &global_class_name {
                continue;
            }

            if class_decl.generic_params.is_empty() {
                compile_class_methods(&mut shared_functions, &translator, class_name, "", class_decl.is_global, &class_decl.methods);
            } else if let Some(insts) = instantiations.get(class_name) {
                for args in insts {
                    let mut subst = HashMap::new();
                    for (i, gp) in class_decl.generic_params.iter().enumerate() {
                        if let Some(a) = args.get(i) {
                            subst.insert(gp.name.clone(), a.clone());
                        }
                    }
                    let specialized: Vec<FnDecl> = class_decl.methods.iter().map(|m| substitute_method(m, &subst)).collect();
                    let suffix = generic_suffix(args);
                    compile_class_methods(&mut shared_functions, &translator, class_name, &suffix, class_decl.is_global, &specialized);
                }
            }

            if class_decl.parents.iter().any(|p| p == "Serializable") {
                let total_size = layout_engine.size_of(&Type::Primitive(class_name.clone()));
                translator.require_bytes_copy();
                shared_functions.push_str(&format!("      function {}_serialize(self) -> ptr {{\n", class_name));
                shared_functions.push_str(&format!("          ptr := allocate_memory({})\n", 32 + total_size));
                shared_functions.push_str(&format!("          mstore(ptr, {})\n", total_size));
                shared_functions.push_str(&format!("          bytes_copy(add(ptr, 32), self, {})\n", total_size));
                shared_functions.push_str("      }\n\n");
            }
        }

        for thunk in translator.drain_helper_thunks() {
            for line in thunk.lines() {
                shared_functions.push_str(&format!("      {}\n", line));
            }
        }

        shared_functions.push_str("      function allocate_memory(size) -> ptr {\n");
        shared_functions.push_str("          ptr := mload(0x40)\n");
        shared_functions.push_str("          if iszero(ptr) { ptr := 0x80 }\n");
        shared_functions.push_str("          mstore(0x40, add(ptr, size))\n");
        shared_functions.push_str("      }\n\n");
        shared_functions.push_str("      function gum_hash_slot(key, slot) -> hash_ptr {\n");
        shared_functions.push_str("          mstore(0x00, key)\n");
        shared_functions.push_str("          mstore(0x20, slot)\n");
        shared_functions.push_str("          hash_ptr := keccak256(0x00, 0x40)\n");
        shared_functions.push_str("      }\n\n");
        shared_functions.push_str("      function gum_memory_copy(src, dst, size) {\n");
        shared_functions.push_str("          mcopy(dst, src, size)\n");
        shared_functions.push_str("      }\n");
        
        let end_of_deployment = yul.find("    // Copy runtime code").unwrap();
        let mut ultra_final_yul = yul[..end_of_deployment].to_string();
        ultra_final_yul.push_str(&shared_functions);
        
        let runtime_part = &yul[end_of_deployment..];
        let stripped_runtime = runtime_part.strip_suffix("    }\n  }\n").unwrap();
        
        let runtime_marker = format!("  object \"{}\" {{\n", runtime_obj);
        match stripped_runtime.find(&runtime_marker) {
            Some(i) => {
                ultra_final_yul.push_str(&stripped_runtime[..i]);
                ultra_final_yul.push_str(&nested_objects);
                ultra_final_yul.push_str(&stripped_runtime[i..]);
            }
            None => ultra_final_yul.push_str(stripped_runtime),
        }
        ultra_final_yul.push_str(&shared_functions);
        ultra_final_yul.push_str("    }\n");
        ultra_final_yul.push_str(&nested_objects);
        ultra_final_yul.push_str("  }\n}\n");

        let errors = translator.take_errors();
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }

        let mut abi_entries = abi_gen.generate_abi(program, global_class);
        for (name, schema) in translator.recorded_events() {
            abi_entries.push(AbiGenerator::event_entry(&name, schema.inputs));
        }
        let abi_json = serde_json::to_string_pretty(&abi_entries).unwrap();

        stack.pop();
        Ok((ultra_final_yul, abi_json))
        }
    }
}
