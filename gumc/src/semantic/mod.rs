use crate::ast::*;
use crate::parser;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;

#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub is_const: bool,
    pub type_def: Type,
}

// The var (type-inferred) declaration sentinel emitted by the parser.
pub fn is_infer(t: &Type) -> bool {
    matches!(t, Type::Primitive(s) if s == "_infer")
}

// Replaces a generic class's own parameter names (T, K, V, ...) with the
// concrete types from one specific instantiation, mirrors codegen/mod.rs's
// substitute_type, used here just for resolving a method call's return type.
fn substitute_generic_type(t: &Type, subst: &HashMap<String, Type>) -> Type {
    match t {
        Type::Primitive(name) => subst.get(name).cloned().unwrap_or_else(|| t.clone()),
        Type::Array(inner) => Type::Array(Box::new(substitute_generic_type(inner, subst))),
        Type::FixedArray(inner, n) => Type::FixedArray(Box::new(substitute_generic_type(inner, subst)), *n),
        Type::Generic { name, args } => Type::Generic {
            name: name.clone(),
            args: args.iter().map(|a| substitute_generic_type(a, subst)).collect(),
        },
    }
}

pub struct TypeChecker {
    pub symbol_table: Vec<HashMap<String, SymbolInfo>>,
    pub loaded_classes: HashMap<String, ClassDecl>,
    pub loaded_enums: HashMap<String, EnumDecl>,
    pub loaded_errors: HashMap<String, ErrorDecl>,
    // Names registered via extern class rather than plain class.
    // Codegen needs to tell them apart: calling a method on an extern class
    // means emitting a real external CALL, whereas calling one on a
    // (locally-bodied) class means invoking a locally compiled function.
    pub loaded_interfaces: HashSet<String>,
    // Set to true if the contract contains any external calls (call statements
    // or method calls on extern classes). Used to optimize out reentrancy guards.
    pub has_external_calls: Cell<bool>,
    // Class names in first-registration order (declaration order for
    // locally-defined classes, then import order for use-loaded ones).
    // loaded_classes is a HashMap, whose iteration order Rust deliberately
    // randomizes per process, layout.rs must not iterate that map directly
    // when assigning storage slots, or the same source could get different
    // slot layouts on different compiler runs. See class_order's use in
    // LayoutEngine for why that matters (proxy upgrades, reproducible builds).
    pub class_order: Vec<String>,
    // Top-level function name -> declared return type. Without this,
    // eval_type(FnCall) has no way to know what calling a gum function
    // evaluates to (it falls back to "unknown"), which breaks anything
    // downstream that needs the result's real type, e.g. property access
    // on a function call's result (make_pair(a, b).x).
    pub function_return_types: HashMap<String, Type>,
    // Declared return type of the function/method currently being verified, so
    // a return statement can be checked against it. None means the enclosing
    // function returns nothing, and only a bare return is legal.
    current_return_type: Option<Type>,
}

impl TypeChecker {
    pub fn new() -> Self {
        TypeChecker {
            symbol_table: vec![HashMap::new()],
            loaded_classes: HashMap::new(),
            loaded_enums: HashMap::new(),
            loaded_errors: HashMap::new(),
            loaded_interfaces: HashSet::new(),
            has_external_calls: Cell::new(false),
            class_order: Vec::new(),
            function_return_types: HashMap::new(),
            current_return_type: None,
        }
    }

    fn register_class(&mut self, c: &ClassDecl) {
        if !self.loaded_classes.contains_key(&c.name) {
            self.class_order.push(c.name.clone());
        }
        let mut c = c.clone();
        if c.is_global {
            for f in &mut c.fields {
                if let Type::Generic { name, args } = &f.type_def {
                    if name == "Vec" && args.len() == 1 {
                        f.type_def = Type::Array(Box::new(args[0].clone()));
                    }
                }
            }
        }
        if c.is_extern {
            self.loaded_interfaces.insert(c.name.clone());
        }
        self.loaded_classes.insert(c.name.clone(), c);
    }

    // --- Inheritance ---
    //
    // class Child [Parent] gives Child a copy of Parent's fields and methods.
    // Resolution is a flattening pass, run once after every class is
    // registered: each class's fields/methods are rewritten in place to
    // include what it inherits, so nothing downstream needs an inheritance
    // concept, the layout engine packs the merged field list, and codegen
    // emits the merged method list.
    //
    // The rules:
    //   * A parent's fields come first, then the child's own, so a child that
    //     appends a field cannot move an inherited one to a different slot.
    //   * A method the child declares itself overrides the parent's.
    //   * new is inherited like any other method; declare one to override it.
    //   * Re-declaring an inherited *field* is an error, not shadowing.
    //   * An interface parent means "implements": it contributes nothing, but
    //     the child must define every method it declares, with a matching
    //     signature.
    //   * Ambiguity (two parents supplying the same method, child silent) and
    //     cycles are errors.
    fn flatten_inheritance(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        for name in self.class_order.clone() {
            let mut stack = Vec::new();
            if let Err(e) = self.flatten_class(&name, &mut done, &mut stack) {
                errors.push(e);
            }
        }
        errors
    }

    fn flatten_class(&mut self, name: &str, done: &mut HashSet<String>, stack: &mut Vec<String>) -> Result<(), String> {
        if done.contains(name) {
            return Ok(());
        }
        if stack.iter().any(|s| s == name) {
            stack.push(name.to_string());
            return Err(format!(
                "Semantic Error: inheritance cycle: {}. A class cannot be its own ancestor.",
                stack.join(" -> ")
            ));
        }
        let class = match self.loaded_classes.get(name) {
            Some(c) => c.clone(),
            None => return Ok(()),
        };
        if class.parents.is_empty() {
            done.insert(name.to_string());
            return Ok(());
        }
        stack.push(name.to_string());

        let mut fields: Vec<ClassField> = Vec::new();
        let mut methods: Vec<FnDecl> = Vec::new();
        let mut method_source: HashMap<String, String> = HashMap::new();
        let mut ancestors: Vec<String> = Vec::new();
        let mut required: Vec<(String, FnDecl)> = Vec::new();

        for p in &class.parents {
            let parent = match self.loaded_classes.get(p) {
                Some(c) => c.clone(),
                None => {
                    stack.pop();
                    return Err(format!(
                        "Semantic Error: class '{}' inherits from '{}', which is not a known class.",
                        name, p
                    ));
                }
            };
            self.flatten_class(p, done, stack)?;
            let parent = self.loaded_classes.get(p).cloned().unwrap_or(parent);

            if !parent.generic_params.is_empty() {
                stack.pop();
                return Err(format!(
                    "Semantic Error: class '{}' cannot inherit from generic class '{}', there is no syntax to supply its type arguments.",
                    name, p
                ));
            }
            if parent.is_global {
                stack.pop();
                return Err(format!(
                    "Semantic Error: class '{}' cannot inherit from '{}', which is a contract. A contract is a storage singleton, not a base class.",
                    name, p
                ));
            }

            ancestors.push(p.clone());
            ancestors.extend(parent.parents.iter().cloned());

            if parent.is_extern {
                for m in &parent.methods {
                    required.push((p.clone(), m.clone()));
                }
                continue;
            }

            for f in &parent.fields {
                if let Some(prev) = fields.iter().find(|e| e.name == f.name) {
                    let _ = prev;
                    stack.pop();
                    return Err(format!(
                        "Semantic Error: class '{}' inherits a field named '{}' from more than one parent. Rename one of them.",
                        name, f.name
                    ));
                }
                fields.push(f.clone());
            }
            for m in &parent.methods {
                if let Some(other) = method_source.get(&m.name) {
                    if !class.methods.iter().any(|own| own.name == m.name) {
                        stack.pop();
                        return Err(format!(
                            "Semantic Error: class '{}' inherits method '{}' from both '{}' and '{}'. Declare it on '{}' to say which one it should be.",
                            name, m.name, other, p, name
                        ));
                    }
                    continue;
                }
                method_source.insert(m.name.clone(), p.clone());
                methods.push(m.clone());
            }
        }

        for f in &class.fields {
            if fields.iter().any(|e| e.name == f.name) {
                stack.pop();
                return Err(format!(
                    "Semantic Error: class '{}' re-declares inherited field '{}'. Inherited fields already occupy their slots, rename this one.",
                    name, f.name
                ));
            }
            fields.push(f.clone());
        }
        let mut supers: Vec<FnDecl> = Vec::new();
        for m in &class.methods {
            match methods.iter().position(|e| e.name == m.name) {
                Some(i) => {
                    let mut parent_copy = methods[i].clone();
                    parent_copy.name = super_name(&m.name);
                    supers.push(parent_copy);
                    methods[i] = m.clone();
                }
                None => methods.push(m.clone()),
            }
        }
        methods.extend(supers);

        for (iface, want) in &required {
            match methods.iter().find(|m| m.name == want.name) {
                None => {
                    stack.pop();
                    return Err(format!(
                        "Semantic Error: class '{}' declares interface '{}' but never defines '{}'.",
                        name, iface, want.name
                    ));
                }
                Some(got) => {
                    if signature_key(got) != signature_key(want) {
                        stack.pop();
                        return Err(format!(
                            "Semantic Error: class '{}' defines '{}' with a signature that doesn't match interface '{}'.\n  interface: {}\n  class:     {}",
                            name, want.name, iface, signature_key(want), signature_key(got)
                        ));
                    }
                }
            }
        }

        ancestors.retain(|a| a != name);
        ancestors.dedup();
        let mut seen = HashSet::new();
        ancestors.retain(|a| seen.insert(a.clone()));

        if let Some(c) = self.loaded_classes.get_mut(name) {
            c.fields = fields;
            c.methods = methods;
            c.parents = ancestors;
        }
        stack.pop();
        done.insert(name.to_string());
        Ok(())
    }

    // A storage array of scalars copies out to memory element by element, so it is a value.
    // An array of structs is not: its elements are slot groups with no memory form, and copying one would need a per struct memory layout gum does not have.
    fn reject_storage_array_copy(&self, e: &Expr, error_prefix: &str) -> Result<(), String> {
        let Expr::PropertyAccess { base, property } = e else {
            return Ok(());
        };
        let Expr::Identifier(base_name) = &**base else {
            return Ok(());
        };
        let class_name = if base_name == "self" {
            match self.lookup_symbol("self").map(|i| i.type_def.clone()) {
                Some(Type::Primitive(n)) => n,
                _ => return Ok(()),
            }
        } else {
            base_name.clone()
        };
        let Some(class) = self.loaded_classes.get(&class_name) else {
            return Ok(());
        };
        if !class.is_global {
            return Ok(());
        }
        if let Some(f) = class.fields.iter().find(|f| f.name == *property) {
            if let Type::Array(inner) = &f.type_def {
                if crate::codegen::translator::is_struct_type(self, inner) {
                    return Err(format!(
                        "{} '{}.{}' holds structs and cannot be copied into a value: a struct element is a group of slots with no memory form. Use it in place: '{}.{}[i].field', 'for x in {}.{}', '.length', '.push()', '.pop()'.",
                        error_prefix, class_name, property, class_name, property, class_name, property
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn push_scope(&mut self) {
        self.symbol_table.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.symbol_table.pop();
    }

    pub fn insert_symbol(&mut self, name: String, info: SymbolInfo) {
        if let Some(scope) = self.symbol_table.last_mut() {
            scope.insert(name, info);
        }
    }

    pub fn lookup_symbol(&self, name: &str) -> Option<&SymbolInfo> {
        for scope in self.symbol_table.iter().rev() {
            if let Some(info) = scope.get(name) {
                return Some(info);
            }
        }
        None
    }

    // Whether an enum has any variant carrying a payload.
    //
    // This is the whole distinction that matters for an enum's size. A
    // payload-free enum is just a tag, so it is a u8 exactly like Solidity's,
    // and every place that stores or encodes one can treat it as a scalar. A
    // payload-carrying one needs the [tag][payload] pair in memory, which has
    // no ABI form and no Solidity storage equivalent, so it is memory-only and
    // rejected anywhere a size would be needed.
    pub fn enum_has_payload(&self, name: &str) -> bool {
        self.loaded_enums
            .get(name)
            .map(|e| e.variants.iter().any(|v| v.payload.is_some()))
            .unwrap_or(false)
    }

    // Whether t is a payload-free enum, i.e. one that behaves as a plain u8 tag.
    pub fn is_scalar_enum(&self, t: &Type) -> bool {
        matches!(t, Type::Primitive(n) if self.loaded_enums.contains_key(n) && !self.enum_has_payload(n))
    }

    // Whether t is a payload-carrying enum: usable as a local, illegal anywhere that needs a size.
    pub fn is_payload_enum(&self, t: &Type) -> bool {
        matches!(t, Type::Primitive(n) if self.enum_has_payload(n))
    }

    // Reports as many independent semantic errors as it safely can in one
    // pass, rather than making you fix-recompile-fix to see the next one.
    // Scoped at the top-level-declaration granularity: two unrelated
    // functions (or methods) with bugs both get reported, but a second bug
    // *within the same function*, after an earlier statement in it already
    // failed, still won't surface until the first is fixed, that would
    // need verify_statement itself restructured to not bail out internally,
    // which is a bigger change than this pass makes.
    pub fn check(&mut self, mut program: Program, base_dir: &str) -> Result<(), Vec<String>> {
        println!("--> [Semantic Analyzer] Resolving Modules & Building Global Symbol Table...");

        let mut errors: Vec<String> = Vec::new();
        let mut new_declarations = Vec::new();

        let mut pending: Vec<String> = Vec::new();
        // Every `use x.y.Sym` seen, checked against its module once everything is loaded, since a module is only parsed the first time it is pulled in.
        let mut requested: Vec<(String, String, String)> = Vec::new();
        let mut module_symbols: HashMap<String, HashSet<String>> = HashMap::new();
        let mut module_asts: HashMap<String, Program> = HashMap::new();
        // Keyed "module::Decl", so a symbol reached from two different imports still merges once.
        let mut merged: HashSet<String> = HashSet::new();

        // A local import resolves against the source file's own directory; a gum.* import resolves against the table compiled into this binary and touches no filesystem at all.
        // The grammar's path rule is idents joined by dots, so a dot is always a directory separator here and there is no leading "./" form to strip.
        let local_path = |path: &str| -> String {
            format!("{}/{}.gum", base_dir, path.replace('.', "/"))
        };

        for decl in &program.declarations {
            match decl {
                Declaration::Use(u) => pending.push(u.path.clone()),
                Declaration::Class(c) => self.register_class(c),
                Declaration::Enum(e) => {
                    self.loaded_enums.insert(e.name.clone(), e.clone());
                }
                Declaration::Error(err) => {
                    self.loaded_errors.insert(err.name.clone(), err.clone());
                }
                _ => {}
            }
        }

        while let Some(use_path) = pending.pop() {
            // A path is a module plus the symbol wanted out of it, so gum.defaults.Account is the class Account inside module gum.defaults, not a file named after the class.
            // Local imports follow the same shape: a/b/C.gum if that file exists, otherwise the symbol C out of a/b.gum.
            let resolved = if crate::stdlib::is_std_path(&use_path) {
                match crate::stdlib::split_module(&use_path) {
                    Some((m, sym)) => {
                        let src = crate::stdlib::lookup(&m).unwrap_or_default().to_string();
                        Some((m, src, sym))
                    }
                    None => {
                        errors.push(format!(
                            "Module Error: '{}' does not name anything in the standard library. Modules: {}.",
                            use_path,
                            crate::stdlib::known_modules().join(", ")
                        ));
                        None
                    }
                }
            } else {
                let direct = local_path(&use_path);
                match fs::read_to_string(&direct) {
                    Ok(s) => Some((direct, s, None)),
                    Err(_) => match use_path.rsplit_once('.') {
                        Some((head, sym)) => {
                            let p = local_path(head);
                            match fs::read_to_string(&p) {
                                Ok(s) => Some((p, s, Some(sym.to_string()))),
                                Err(e) => {
                                    errors.push(format!("Module Error: cannot read module '{}', tried {} and {}: {}", use_path, direct, p, e));
                                    None
                                }
                            }
                        }
                        None => {
                            errors.push(format!("Module Error: cannot read module '{}' at {}", use_path, direct));
                            None
                        }
                    },
                }
            };
            let (mod_key, source, symbol) = match resolved {
                Some(t) => t,
                None => continue,
            };
            if let Some(sym) = &symbol {
                requested.push((use_path.clone(), mod_key.clone(), sym.clone()));
            }
            // The module is parsed once and cached; what gets merged is decided per symbol below, so two `use`s of different symbols out of one module each pull only their own.
            let ast = match module_asts.entry(mod_key.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    println!("    [Module Loader] Loading {}", mod_key);
                    match parser::parse_program(&source) {
                        Ok(a) => e.insert(a),
                        Err(errs) => {
                            errors.extend(errs.into_iter().map(|x| format!("{} ({})", x, mod_key)));
                            continue;
                        }
                    }
                }
            };

            let syms = module_symbols.entry(mod_key.clone()).or_default();
            for d in &ast.declarations {
                if let Some(n) = decl_name(d) {
                    syms.insert(n.to_ascii_lowercase());
                }
            }

            // Importing a symbol pulls that declaration and whatever its signature reaches, not the whole module.
            // Merging everything would drag every std class into every contract, and since an `unsafe` body anywhere marks the program as making external calls, that alone put a reentrancy guard on entry points that need none.
            let wanted: Vec<usize> = match &symbol {
                Some(sym) => symbol_closure(&ast.declarations, sym),
                None => (0..ast.declarations.len()).collect(),
            };

            let mut nested: Vec<String> = Vec::new();
            for i in wanted {
                let imp_decl = ast.declarations[i].clone();
                if let Declaration::Use(u) = &imp_decl {
                    nested.push(u.path.clone());
                    continue;
                }
                let key = match decl_name(&imp_decl) {
                    Some(n) => format!("{}::{}", mod_key, n),
                    None => continue,
                };
                if !merged.insert(key) {
                    continue;
                }
                match &imp_decl {
                    Declaration::Class(c) => self.register_class(c),
                    Declaration::Enum(e) => {
                        self.loaded_enums.insert(e.name.clone(), e.clone());
                    }
                    Declaration::Error(err) => {
                        self.loaded_errors.insert(err.name.clone(), err.clone());
                    }
                    _ => {}
                }
                new_declarations.push(imp_decl);
            }
            pending.extend(nested);
        }

        // Checked here rather than at the import, because a module is parsed only the first time it is pulled in and a later `use` of a different symbol out of it would otherwise go unchecked.
        for (use_path, mod_key, sym) in &requested {
            if let Some(syms) = module_symbols.get(mod_key) {
                if !syms.contains(&sym.to_ascii_lowercase()) {
                    let mut have: Vec<&str> = syms.iter().map(|s| s.as_str()).collect();
                    have.sort_unstable();
                    errors.push(format!(
                        "Module Error: '{}' has no '{}'. Module '{}' declares: {}.",
                        use_path,
                        sym,
                        mod_key,
                        have.join(", ")
                    ));
                }
            }
        }

        program.declarations.extend(new_declarations);

        errors.extend(self.flatten_inheritance());

        for decl in &program.declarations {
            if let Declaration::Function(f) = decl {
                if let Some(rt) = &f.return_type {
                    self.function_return_types.insert(f.name.clone(), rt.clone());
                }
            }
        }

        println!("--> [Semantic Analyzer] Running Validation (Generics, Fixed-Point, Scoping, Constructors)...");
        
        for decl in &program.declarations {
            if let Declaration::Function(f) = decl {
                if f.modifiers.iter().any(|m| m == "export") {
                    errors.push(format!(
                        "Semantic Error: top-level function '{}' cannot be marked export. Entry points must be declared inside a contract block.",
                        f.name
                    ));
                }
            }
        }

        for decl in &program.declarations {
            if let Declaration::Function(f) = decl {
                self.push_scope();
                self.current_return_type = f.return_type.clone();

                for param in &f.parameters {
                    self.insert_symbol(param.name.clone(), SymbolInfo {
                        is_const: !param.is_mut,
                        type_def: param.type_def.clone()
                    });
                }

                let mut body_ok = true;
                for stmt in &f.body {
                    if let Err(e) = self.verify_statement(stmt) {
                        errors.push(e);
                        body_ok = false;
                        break;
                    }
                }

                if body_ok {
                    if let Some(expected_return) = &f.return_type {
                        match self.check_returns(&f.body, expected_return) {
                            Ok(true) => {}
                            Ok(false) => errors.push(format!("Semantic Error: Function '{}' promises to return {:?}, but does not guarantee a return on all paths.", f.name, expected_return)),
                            Err(e) => errors.push(e),
                        }
                    }
                }

                self.pop_scope();
            }
        }

        let classes: Vec<(String, ClassDecl)> = self.loaded_classes.iter()
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
            
        // A payload-carrying enum is a [tag][payload] pair that only exists in memory: it has no ABI form, and no storage layout Solidity has an equivalent for.
        // Anywhere a size is needed it must be rejected, or the layout engine lays it out as 64 opaque bytes and every read of it returns a stale memory address.
        for (class_name, class_decl) in &classes {
            for field in &class_decl.fields {
                let payload_enum_in = |t: &Type| -> Option<String> {
                    match t {
                        Type::Primitive(n) if self.enum_has_payload(n) => Some(n.clone()),
                        Type::Array(inner) | Type::FixedArray(inner, _) => match inner.as_ref() {
                            Type::Primitive(n) if self.enum_has_payload(n) => Some(n.clone()),
                            _ => None,
                        },
                        Type::Generic { args, .. } => args.iter().find_map(|a| match a {
                            Type::Primitive(n) if self.enum_has_payload(n) => Some(n.clone()),
                            _ => None,
                        }),
                        _ => None,
                    }
                };
                if let Some(en) = payload_enum_in(&field.type_def) {
                    errors.push(format!(
                        "Semantic Error: enum '{}' has a variant carrying a payload, so it has no storage layout and cannot be the type of field '{}' in '{}'. Such an enum lives in memory only: use it as a local, and store its parts separately.",
                        en, field.name, class_name
                    ));
                }
            }
            if class_decl.is_global {
                for field in &class_decl.fields {
                    if let Type::Generic { name, args } = &field.type_def {
                        if name == "Vec" {
                            if args.len() != 1 {
                                errors.push(format!(
                                    "Semantic Error: Vec needs exactly one element type. Field '{}' in class '{}'.",
                                    field.name, class_name
                                ));
                            } else if matches!(&args[0], Type::Generic { .. } | Type::Array(_)) {
                                errors.push(format!(
                                    "Semantic Error: a Vec of {:?} has no storage layout. Field '{}' in class '{}' must hold a scalar element type.",
                                    args[0], field.name, class_name
                                ));
                            }
                        }
                    }
                }
            }
        }

        {
            // A payload-free enum belongs here: it is a u8 in every respect now, it maps to uint8 in the ABI, and it packs 32-to-a-slot in storage exactly like one. A payload-carrying enum does not, and is caught by the check below.
            let elem_ok = |t: &Type| {
                if self.is_scalar_enum(t) {
                    return true;
                }
                matches!(t, Type::Primitive(n) if matches!(n.as_str(),
                    "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
                    "i8" | "i16" | "i32" | "i64" | "i128" | "i256" |
                    "bool" | "Account"))
            };
            // The field types of a user struct, or None for anything that is not one.
            // Account, String and Bytes are Primitive-named classes the compiler treats as values, so they are not structs for this purpose and encode on their own.
            let struct_fields = |t: &Type| -> Option<Vec<Type>> {
                if let Type::Primitive(n) = t {
                    if n == "Account" || n == "String" || n == "Bytes" {
                        return None;
                    }
                    if let Some(c) = self.loaded_classes.get(n) {
                        return Some(c.fields.iter().map(|f| f.type_def.clone()).collect());
                    }
                }
                None
            };
            // An enum crosses as uint8, which carries the tag and nothing else, so a variant with a payload has nowhere to put it.
            let payload_enum = |t: &Type| -> bool {
                match t {
                    Type::Primitive(n) => self
                        .loaded_enums
                        .get(n)
                        .map(|e| e.variants.iter().any(|v| v.payload.is_some()))
                        .unwrap_or(false),
                    _ => false,
                }
            };
            let check = |t: &Type, what: String, errors: &mut Vec<String>| {
                if payload_enum(t) {
                    errors.push(format!(
                        "Semantic Error: {} has no ABI encoding: an enum crosses the ABI as a uint8 tag, and '{}' has a variant carrying a payload, which there is nowhere to put. Pass the payload as its own parameter.",
                        what,
                        type_name(t)
                    ));
                    return;
                }
                if let Some(fs) = struct_fields(t) {
                    if fs.is_empty() || !fs.iter().all(&elem_ok) {
                        errors.push(format!(
                            "Semantic Error: {} has no ABI encoding: a struct crossing the ABI boundary must have at least one field and hold only scalar fields, and '{}' does not. Pass its parts separately, or flatten it.",
                            what,
                            type_name(t)
                        ));
                    }
                    return;
                }
                // A dynamic array of static structs encodes: the elements are inline, so it needs no per-element offset.
                // A fixed array of them is still rejected, as is any array whose element is itself an array, because neither has a codec yet.
                if let Type::Array(inner) = t {
                    if let Some(fs) = struct_fields(inner) {
                        if fs.is_empty() || !fs.iter().all(&elem_ok) {
                            errors.push(format!(
                                "Semantic Error: {} has no ABI encoding: an array of structs must hold a struct of only scalar fields, and '{}' is not one. Pass its parts separately, or flatten it.",
                                what,
                                type_name(inner)
                            ));
                        }
                        return;
                    }
                }
                if let Type::Array(inner) | Type::FixedArray(inner, _) = t {
                    if !elem_ok(inner) {
                        errors.push(format!(
                            "Semantic Error: {} has no ABI encoding: an array crossing the ABI boundary must hold a scalar element, and {} is not one. Pass its parts separately, or flatten it.",
                            what,
                            type_name(inner)
                        ));
                    }
                }
            };
            // An interface's methods cross the ABI outbound: gum encodes the args and decodes the result, so its types are held to the same rules an export's are.
            for (class_name, class_decl) in &classes {
                for f in &class_decl.methods {
                    let crosses_abi = f.modifiers.iter().any(|m| m == "export")
                        || (class_decl.is_global && f.name == "new")
                        || class_decl.is_extern;
                    if !crosses_abi {
                        continue;
                    }
                    for p in &f.parameters {
                        check(
                            &p.type_def,
                            format!("parameter '{}' of '{}.{}'", p.name, class_name, f.name),
                            &mut errors,
                        );
                    }
                    if let Some(rt) = &f.return_type {
                        check(rt, format!("the return type of '{}.{}'", class_name, f.name), &mut errors);
                    }
                }
            }
        }

        for (class_name, class_decl) in &classes {
            if class_decl.is_global {
                continue;
            }
            for f in &class_decl.fields {
                if f.is_transient {
                    errors.push(format!(
                        "Semantic Error: field '{}' of '{}' cannot be transient: only a contract's fields live in storage, and a plain class is a memory value.",
                        f.name, class_name
                    ));
                }
                if f.is_const {
                    errors.push(format!(
                        "Semantic Error: field '{}' of '{}' cannot be const: a const contract field is carried in the contract's deployed bytecode, and a plain class is a memory value with no code of its own. For a value that never changes, just use a local const or a plain field.",
                        f.name, class_name
                    ));
                }
            }
        }

        for (class_name, class_decl) in &classes {
            if !class_decl.is_global {
                continue;
            }
            let immutables: Vec<&ClassField> =
                class_decl.fields.iter().filter(|f| f.is_const).collect();
            if immutables.is_empty() {
                continue;
            }

            for f in &immutables {
                if f.is_transient {
                    errors.push(format!(
                        "Semantic Error: field '{}' of '{}' cannot be both transient and const: transient is storage that clears each transaction, const is not storage at all.",
                        f.name, class_name
                    ));
                }
            }

            let ctor = class_decl.methods.iter().find(|m| m.name == "new");
            match ctor {
                None => {
                    errors.push(format!(
                        "Semantic Error: '{}' declares const field(s) {} but has no fn new(). A const field is assigned once, in the constructor, and fixed from deployment onwards, without one it could only ever read zero. Drop const if the value is set after deploy (e.g. from a once fn initialize), which makes it an ordinary storage field.",
                        class_name,
                        immutables.iter().map(|f| format!("'{}'", f.name)).collect::<Vec<_>>().join(", ")
                    ));
                }
                Some(ctor) => {
                    for f in &immutables {
                        if reads_field(&ctor.body, class_name, &f.name) {
                            errors.push(format!(
                                "Semantic Error: fn new() of '{}' reads const field '{}'. A const field's value is only fixed once the constructor has finished, so there is nothing to read during it. Use the value you are assigning (or a constructor parameter) instead.",
                                class_name, f.name
                            ));
                        }
                        if definitely_assigns(&ctor.body, class_name, &f.name) {
                            continue;
                        }
                        if assigns_field(&ctor.body, class_name, &f.name) {
                            errors.push(format!(
                                "Semantic Error: const field '{}' of '{}' is assigned in fn new(), but not on every path through it. On the paths that skip the assignment it would be fixed at zero for the life of the contract. Give every branch an assignment (an if needs its else), or assign it once unconditionally, note that an assignment inside a loop never counts, since the loop may run zero times.",
                                f.name, class_name
                            ));
                        } else {
                            errors.push(format!(
                                "Semantic Error: const field '{}' of '{}' is never assigned in fn new(). It would be fixed at zero for the life of the contract.",
                                f.name, class_name
                            ));
                        }
                    }
                }
            }

            for m in &class_decl.methods {
                if m.name == "new" {
                    continue;
                }
                for f in &immutables {
                    if assigns_field(&m.body, class_name, &f.name) {
                        errors.push(format!(
                            "Semantic Error: '{}.{}' assigns const field '{}'. A const field is fixed at construction and cannot be written afterwards, only fn new() may assign it. Drop const to make it an ordinary storage field you can write.",
                            class_name, m.name, f.name
                        ));
                    }
                }
            }
        }

        for (class_name, class_decl) in &classes {
            let mut seen: HashSet<&str> = HashSet::new();
            for m in &class_decl.methods {
                if !seen.insert(m.name.as_str()) {
                    errors.push(format!(
                        "Semantic Error: '{}' declares more than one method named '{}'. Each compiles to a function of that name, so the second would collide with the first.",
                        class_name, m.name
                    ));
                }
            }
        }

        for (class_name, class_decl) in &classes {
            if class_decl.is_global {
                for f in &class_decl.methods {
                    if f.name != "receive" && f.name != "fallback" {
                        continue;
                    }
                    let exported = f.modifiers.iter().any(|m| m == "export");
                    let payable = f.modifiers.iter().any(|m| m == "payable");
                    if !exported {
                        errors.push(format!(
                            "Semantic Error: '{}' in contract '{}' is a reserved entry point and must be declared export {}fn {}():, without export it would never be reachable.",
                            f.name, class_name, if f.name == "receive" { "payable " } else { "" }, f.name
                        ));
                        continue;
                    }
                    if !f.parameters.is_empty() {
                        errors.push(format!(
                            "Semantic Error: '{}' in contract '{}' takes no parameters, it is dispatched without a selector, so there is no ABI to decode arguments from.",
                            f.name, class_name
                        ));
                    }
                    if f.return_type.is_some() {
                        errors.push(format!(
                            "Semantic Error: '{}' in contract '{}' returns nothing, its caller is a plain ETH send or an unmatched call, which has no return value to decode.",
                            f.name, class_name
                        ));
                    }
                    if f.name == "receive" && !payable {
                        errors.push(format!(
                            "Semantic Error: 'receive' in contract '{}' must be payable, it exists to accept ETH, and a non-payable one would reject every call that reaches it.",
                            class_name
                        ));
                    }
                }
            }

            for method in &class_decl.methods {
                if method.body.is_empty() {
                    continue;
                }
                self.push_scope();
                self.current_return_type = method.return_type.clone();
                self.insert_symbol("self".to_string(), SymbolInfo {
                    is_const: true,
                    type_def: Type::Primitive(class_name.clone()),
                });
                for param in &method.parameters {
                    self.insert_symbol(param.name.clone(), SymbolInfo {
                        is_const: !param.is_mut,
                        type_def: param.type_def.clone()
                    });
                }

                let mut body_ok = true;
                for stmt in &method.body {
                    if let Err(e) = self.verify_statement(stmt) {
                        errors.push(e);
                        body_ok = false;
                        break;
                    }
                }

                if body_ok {
                    if let Some(expected_return) = &method.return_type {
                        match self.check_returns(&method.body, expected_return) {
                            Ok(true) => {}
                            Ok(false) => errors.push(format!("Semantic Error: Method '{}.{}' promises to return {:?}, but does not guarantee a return on all paths.", class_name, method.name, expected_return)),
                            Err(e) => errors.push(e),
                        }
                    }
                }

                self.pop_scope();
            }
        }

        if errors.is_empty() {
            println!("    [OK] Validation passed!");
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn check_returns(&mut self, body: &[Spanned<Statement>], expected: &Type) -> Result<bool, String> {
        for spanned_stmt in body {
            match &spanned_stmt.node {
                Statement::Return { value } => {
                    let loc = format!("Semantic Error at {}:{}:", spanned_stmt.line, spanned_stmt.col);
                    let value = value.as_ref().ok_or_else(|| {
                        format!("{} return needs a value: this function returns {:?}", loc, expected)
                    })?;
                    let evaluated_type = self.eval_type(value)
                        .map_err(|e| if e.starts_with("Semantic Error at") { e } else { format!("{} {}", loc, e) })?;
                    if !self.is_assignable(expected, &evaluated_type) {
                        return Err(format!("{} Return type mismatch. Expected {:?}, got {:?}", loc, expected, evaluated_type));
                    }
                    return Ok(true);
                }
                // A revert ends the frame, so the path never reaches the missing return: it owes one no more than a return does.
                // The const-assignment checker already gives this credit through `diverges`; this checker did not, so a function whose last statement was a revert was rejected for not returning.
                Statement::Revert { .. } => return Ok(true),
                Statement::IfElse { if_body, else_body, .. } => {
                    let if_returns = self.check_returns(if_body, expected)?;
                    if let Some(eb) = else_body {
                        let else_returns = self.check_returns(eb, expected)?;
                        if if_returns && else_returns {
                            return Ok(true);
                        }
                    }
                }
                Statement::Match { expr, arms } => {
                    if !arms.is_empty() {
                        let match_type = self.eval_type(expr).ok();
                        let mut all_return = true;
                        for arm in arms {
                            self.push_scope();
                            if let Some(payload_var) = &arm.payload_var {
                                if let Some(Type::Primitive(enum_name)) = &match_type {
                                    if let Some(variant) = self.loaded_enums.get(enum_name)
                                        .and_then(|e| e.variants.iter().find(|v| v.name == arm.variant).cloned())
                                    {
                                        if let Some(payload_type) = variant.payload {
                                            self.insert_symbol(payload_var.clone(), SymbolInfo { is_const: false, type_def: payload_type });
                                        }
                                    }
                                }
                            }
                            let arm_returns = self.check_returns(&arm.body, expected);
                            self.pop_scope();
                            if !arm_returns? {
                                all_return = false;
                                break;
                            }
                        }
                        if all_return {
                            return Ok(true);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn is_assignable(&self, expected: &Type, provided: &Type) -> bool {
        let exp_str = format!("{:?}", expected);
        let prov_str = format!("{:?}", provided);
        if exp_str == prov_str { return true; }

        if let Type::Primitive(exp_name) = expected {
            if exp_name == "All" { return true; }

            let is_known_type = matches!(exp_name.as_str(),
                "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
                "i8" | "i16" | "i32" | "i64" | "i128" | "i256" |
                "f32" | "f64" | "bool" | "Account")
                || self.loaded_classes.contains_key(exp_name)
                || self.loaded_enums.contains_key(exp_name);
            if !is_known_type { return true; }

            if let Type::Primitive(prov_name) = provided {
                if prov_name == "u256" && (exp_name == "f32" || exp_name == "f64") {
                    return true;
                }
                if exp_name == "u256" && (prov_name == "f32" || prov_name == "f64") {
                    return true;
                }
                if (exp_name == "u256" && self.loaded_classes.contains_key(prov_name))
                    || (prov_name == "u256" && self.loaded_classes.contains_key(exp_name)) {
                    return true;
                }
                let is_uint = |n: &str| matches!(n, "u8" | "u16" | "u32" | "u64" | "u128" | "u256");
                let is_sint = |n: &str| matches!(n, "i8" | "i16" | "i32" | "i64" | "i128" | "i256");
                if (is_uint(exp_name) && is_uint(prov_name)) || (is_sint(exp_name) && is_sint(prov_name)) {
                    return true;
                }
            }
        }

        if let Type::Primitive(prov_name) = provided {
            if let Some(class_decl) = self.loaded_classes.get(prov_name) {
                if let Type::Primitive(exp_name) = expected {
                    if class_decl.parents.contains(exp_name) {
                        return true;
                    }
                }
            }
        }
        
        if let Type::Array(exp_inner) = expected {
            if let Type::Array(prov_inner) = provided {
                return self.is_assignable(exp_inner, prov_inner);
            }
        }

        if let Type::Generic { name, args } = expected {
            if name == "Vec" && args.len() == 1 {
                if let Type::Array(prov_inner) = provided {
                    return self.is_assignable(&args[0], prov_inner);
                }
            }
        }

        if let Type::Generic { name, .. } = expected {
            if let Type::Primitive(prov_name) = provided {
                if prov_name == "u256" && self.loaded_classes.contains_key(name) {
                    return true;
                }
            }
        }
        if let Type::Generic { name, .. } = provided {
            if let Type::Primitive(exp_name) = expected {
                if exp_name == "u256" && self.loaded_classes.contains_key(name) {
                    return true;
                }
            }
        }

        if let Type::FixedArray(exp_inner, exp_size) = expected {
            if let Type::FixedArray(prov_inner, prov_size) = provided {
                if exp_size == prov_size {
                    return self.is_assignable(exp_inner, prov_inner);
                }
            }
        }

        false
    }

    // Errors are returned without a location prefix; verify_statement's
    // wrapper attaches the statement's line:col to anything unprefixed.
    fn verify_type_declaration(&self, type_def: &Type) -> Result<(), String> {
        match type_def {
            Type::Generic { name, args } => {
                if let Some(class_decl) = self.loaded_classes.get(name) {
                    if class_decl.generic_params.len() != args.len() {
                        return Err(format!("Generic type '{}' expects {} arguments, but {} were provided.", name, class_decl.generic_params.len(), args.len()));
                    }
                    
                    for (i, arg_type) in args.iter().enumerate() {
                        let param = &class_decl.generic_params[i];
                        let bound_str = &param.bound;
                        
                        let is_union = bound_str.contains("||");
                        let tokens = bound_str.split(if is_union { "||" } else { "&&" });
                        
                        let mut passes_bound = if is_union { false } else { true };
                        
                        for token in tokens {
                            let bound_name = token.trim();
                            let bound_type = Type::Primitive(bound_name.to_string());
                            let satisfies = self.is_assignable(&bound_type, arg_type);
                            
                            if is_union {
                                passes_bound = passes_bound || satisfies;
                            } else {
                                passes_bound = passes_bound && satisfies;
                            }
                        }
                        
                        if !passes_bound {
                            return Err(format!("Type {:?} does not satisfy the bound '{}' for generic parameter '{}' in class '{}'", arg_type, bound_str, param.name, name));
                        }
                    }

                    println!("    [Generics] Validated instantiation of bounded generic {} with {} args.", name, args.len());
                } else {
                    return Err(format!("Undeclared generic type '{}'", name));
                }
            }
            Type::Array(inner) => {
                self.verify_type_declaration(inner)?;
            }
            _ => {}
        }
        Ok(())
    }

    // A fixed-point value is a WAD-scaled integer: 1.0 is 10^18, not 1. So mixing one with a plain integer is almost always a bug, `price * 2` means "times 0.000000000000000002", and the result carries the fixed-point type as if it were fine.
    // The right operand's type was evaluated here and dropped, so nothing rejected the mix. Nothing else checks binary operand compatibility either, so this is the only place it can be caught.
    fn check_fixed_point_math(&self, expr: &Expr) -> Result<(), String> {
        if let Expr::BinaryOp { left, operator, right } = expr {
            let left_type = self.eval_type(left)?;
            let right_type = self.eval_type(right)?;
            let fixed = |t: &Type| matches!(t, Type::Primitive(p) if p == "f32" || p == "f64");
            let lf = fixed(&left_type);
            let rf = fixed(&right_type);
            if lf != rf {
                // A literal is typed u256 until it is coerced, so `x * 2` on a fixed-point x would trip this. Comparisons are fine either way, only arithmetic carries the scale.
                let arith = matches!(operator.as_str(), "+" | "-" | "*" | "/" | "%" | "**");
                if arith && !matches!(if lf { right } else { left }.as_ref(), Expr::Number(_)) {
                    return Err(format!(
                        "Fixed-point math must not mix scales: {:?} {} {:?}. A fixed-point value is WAD-scaled (1.0 is 10^18), so combining it with a plain integer silently reads that integer as a fraction. Convert one side first.",
                        left_type, operator, right_type
                    ));
                }
            }
            if lf {
                if let Type::Primitive(t1) = &left_type {
                    println!("    [Codegen Routing] Intercepted math on {}. Routing to WAD Fixed-Point Math Library to save gas.", t1);
                }
            }
        }
        Ok(())
    }

    fn verify_statement(&mut self, spanned_stmt: &Spanned<Statement>) -> Result<(), String> {
        let line = spanned_stmt.line;
        let col = spanned_stmt.col;
        let error_prefix = format!("Semantic Error at {}:{}:", line, col);

        self.verify_statement_inner(&spanned_stmt.node, &error_prefix)
            .map_err(|e| {
                if e.starts_with("Semantic Error at") {
                    e
                } else {
                    format!("{} {}", error_prefix, e)
                }
            })
    }

    // Checks a custom-error invocation (revert E(..) or assert(c, E(..)))
    // against its declaration: it must exist, and the argument count and types
    // must match.
    fn check_error_args(&mut self, error_name: &str, args: &[Expr], error_prefix: &str) -> Result<(), String> {
        let error_decl = self.loaded_errors.get(error_name)
            .ok_or_else(|| format!("{} Undeclared custom error '{}'", error_prefix, error_name))?.clone();
        if args.len() != error_decl.parameters.len() {
            return Err(format!("{} Custom error '{}' expects {} arguments, got {}", error_prefix, error_name, error_decl.parameters.len(), args.len()));
        }
        for (i, arg) in args.iter().enumerate() {
            let arg_type = self.eval_type(arg)?;
            if !self.is_assignable(&error_decl.parameters[i].type_def, &arg_type) {
                return Err(format!("{} Type mismatch in error '{}' argument {}: expected {:?}, got {:?}", error_prefix, error_name, i + 1, error_decl.parameters[i].type_def, arg_type));
            }
        }
        Ok(())
    }

    fn verify_statement_inner(&mut self, stmt: &Statement, error_prefix: &str) -> Result<(), String> {
        match stmt {
            Statement::VarDecl { name, type_def, is_const, value, .. } => {
                if let Some(v) = value {
                    self.reject_storage_array_copy(v, &error_prefix)?;
                }
                let resolved_type = if is_infer(type_def) {
                    match value {
                        Some(v) => self.eval_type(v)?,
                        None => return Err(format!("{} var declaration of '{}' needs an initializer to infer its type", error_prefix, name)),
                    }
                } else {
                    self.verify_type_declaration(type_def)?;
                    if let Some(value) = value {
                        let evaluated_type = self.eval_type(value)?;
                        if !self.is_assignable(type_def, &evaluated_type) {
                            return Err(format!("{} Type Mismatch. Cannot assign {:?} to {:?}", error_prefix, evaluated_type, type_def));
                        }
                    }
                    type_def.clone()
                };

                self.insert_symbol(name.clone(), SymbolInfo {
                    is_const: *is_const,
                    type_def: resolved_type,
                });
            }
            Statement::Assignment { target, value } => {
                self.reject_storage_array_copy(value, &error_prefix)?;
                let target_type = self.eval_type(target)?;
                let evaluated_type = self.eval_type(value)?;
                if !self.is_assignable(&target_type, &evaluated_type) {
                    return Err(format!("{} Type Mismatch on assignment. Expected {:?}, found {:?}", error_prefix, target_type, evaluated_type));
                }
                
                if let Expr::Identifier(name) = target {
                    if let Some(info) = self.lookup_symbol(name).cloned() {
                        if info.is_const {
                            return Err(format!("{} Cannot reassign to constant variable '{}'!", error_prefix, name));
                        }
                    }
                }
            }
            Statement::Delete { target } => {
                let ty = self.eval_type(target)?;
                match target {
                    Expr::Identifier(name) => {
                        if let Some(info) = self.lookup_symbol(name).cloned() {
                            if info.is_const {
                                return Err(format!(
                                    "{} Cannot delete '{}': it is immutable. Declare it mut to reset it.",
                                    error_prefix, name
                                ));
                            }
                        }
                    }
                    Expr::PropertyAccess { .. } | Expr::IndexAccess { .. } => {}
                    _ => {
                        return Err(format!(
                            "{} delete needs a variable, field, or element to reset, not an expression.",
                            error_prefix
                        ));
                    }
                }
                if let Type::Generic { name, .. } = &ty {
                    if name == "HashMap" {
                        return Err(format!(
                            "{} Cannot delete a whole HashMap, its keys aren't tracked, so there is nothing to clear. Delete a single entry instead (delete m[key]).",
                            error_prefix
                        ));
                    }
                }
            }
            Statement::IfElse { condition, if_body, else_body } => {
                let cond_type = self.eval_type(condition)?;
                if !self.is_assignable(&Type::Primitive("bool".to_string()), &cond_type) {
                    return Err(format!("{} If condition must be bool, found {:?}", error_prefix, cond_type));
                }
                self.push_scope();
                for inner_stmt in if_body { self.verify_statement(inner_stmt)?; }
                self.pop_scope();
                if let Some(eb) = else_body {
                    self.push_scope();
                    for inner_stmt in eb { self.verify_statement(inner_stmt)?; }
                    self.pop_scope();
                }
            }
            Statement::ForLoop { iterator, iterable, body } => {
                let iterable_type = self.eval_type(iterable)?;
                let elem_type = match iterable_type {
                    Type::Array(inner) => *inner,
                    Type::FixedArray(inner, _) => *inner,
                    _ => return Err(format!("{} for-loop requires an array type, found {:?}", error_prefix, iterable_type)),
                };
                self.push_scope();
                self.insert_symbol(iterator.clone(), SymbolInfo { is_const: false, type_def: elem_type });
                for inner_stmt in body { self.verify_statement(inner_stmt)?; }
                self.pop_scope();
            }
            Statement::WhileLoop { condition, body } => {
                let cond_type = self.eval_type(condition)?;
                if !self.is_assignable(&Type::Primitive("bool".to_string()), &cond_type) {
                    return Err(format!("{} While condition must be bool, found {:?}", error_prefix, cond_type));
                }
                self.push_scope();
                for inner_stmt in body { self.verify_statement(inner_stmt)?; }
                self.pop_scope();
            }
            Statement::Match { expr, arms } => {
                let match_type = self.eval_type(expr)?;
                if let Type::Primitive(enum_name) = &match_type {
                    if let Some(enum_decl) = self.loaded_enums.get(enum_name).cloned() {
                        let mut covered_variants = std::collections::HashSet::new();
                        
                        for arm in arms {
                            let variant = enum_decl.variants.iter().find(|v| v.name == arm.variant)
                                .ok_or_else(|| format!("{} Unknown variant '{}' for enum '{}'", error_prefix, arm.variant, enum_name))?;
                                
                            covered_variants.insert(arm.variant.clone());
                                
                            self.push_scope();
                            if let Some(payload_var) = &arm.payload_var {
                                if let Some(payload_type) = &variant.payload {
                                    self.insert_symbol(payload_var.clone(), SymbolInfo {
                                        is_const: false,
                                        type_def: payload_type.clone()
                                    });
                                } else {
                                    return Err(format!("{} Variant '{}' does not have a payload", error_prefix, arm.variant));
                                }
                            }
                            
                            for inner_stmt in &arm.body {
                                self.verify_statement(inner_stmt)?;
                            }
                            self.pop_scope();
                        }
                        
                        for variant in &enum_decl.variants {
                            if !covered_variants.contains(&variant.name) {
                                return Err(format!("{} Match is not exhaustive. Missing variant: '{}'", error_prefix, variant.name));
                            }
                        }
                    } else {
                        return Err(format!("{} Match expression must be an Enum type. Found: {:?}", error_prefix, match_type));
                    }
                } else {
                    return Err(format!("{} Match expression must be an Enum type. Found: {:?}", error_prefix, match_type));
                }
            }
            Statement::Expression(expr) => {
                self.check_fixed_point_math(expr)?;
                self.eval_type(expr)?;
            }
            Statement::Return { value } => {
                match value {
                    Some(value) => {
                        if self.current_return_type.is_none() {
                            return Err(format!(
                                "{} this function declares no return type, so return must not have a value",
                                error_prefix
                            ));
                        }
                        self.check_fixed_point_math(value)?;
                        self.eval_type(value)?;
                    }
                    None => {}
                }
            }
            Statement::Assert { condition, message } => {
                let cond_type = self.eval_type(condition)?;
                if !self.is_assignable(&Type::Primitive("bool".to_string()), &cond_type) {
                    return Err(format!("{} assert condition must be bool, found {:?}", error_prefix, cond_type));
                }
                if let Some(msg) = message {
                    if let Expr::FnCall { name, args } = msg {
                        if self.loaded_errors.contains_key(name) {
                            self.check_error_args(name, args, error_prefix)?;
                            return Ok(());
                        }
                    }
                    let msg_type = self.eval_type(msg)?;
                    if !matches!(&msg_type, Type::Primitive(n) if n == "String" || n == "Bytes") {
                        return Err(format!(
                            "{} assert message must be a String or a custom error call, found {:?}",
                            error_prefix, msg_type
                        ));
                    }
                }
            }
            Statement::Call { .. } => {
                self.has_external_calls.set(true);
            }
            Statement::UnsafeBlock(_) => {
                self.has_external_calls.set(true);
            }
            Statement::Revert { error_name, args } => {
                self.check_error_args(error_name, args, error_prefix)?;
            }
            Statement::BitwiseFlip { name, index, value } => {
                let symbol = self.lookup_symbol(name)
                    .ok_or_else(|| format!("{} Variable '{}' not found in scope for bitwise flip", error_prefix, name))?;
                if symbol.is_const {
                    return Err(format!("{} Cannot modify constant '{}' via bitwise flip", error_prefix, name));
                }
                match &symbol.type_def {
                    Type::Primitive(p) if p.starts_with('u') || p.starts_with('i') => {}
                    _ => return Err(format!("{} Bitwise flip is only supported on integer types, got {:?}", error_prefix, symbol.type_def)),
                }
                
                let idx_type = self.eval_type(index)?;
                match idx_type {
                    Type::Primitive(p) if p.starts_with('u') || p.starts_with('i') => {}
                    _ => return Err(format!("{} Bitwise flip index must be an integer, got {:?}", error_prefix, idx_type)),
                }
                
                let val_type = self.eval_type(value)?;
                match val_type {
                    Type::Primitive(p) if p.starts_with('u') || p.starts_with('i') || p == "bool" => {}
                    _ => return Err(format!("{} Bitwise flip value must be an integer or bool, got {:?}", error_prefix, val_type)),
                }
            }
        }
        Ok(())
    }
    
    // Return type of super.<method>(), and the errors for the two ways it can
    // be wrong: used outside a method, or naming something this class never
    // overrode.
    fn eval_super_call(&self, method: &str) -> Result<Type, String> {
        let class_name = match self.lookup_symbol("self").map(|i| i.type_def.clone()) {
            Some(Type::Primitive(n)) => n,
            _ => {
                return Err(format!(
                    "super.{}() is only available inside a method: there is no parent to call without a self.",
                    method
                ))
            }
        };
        let class = match self.loaded_classes.get(&class_name) {
            Some(c) => c,
            None => return Err(format!("Unknown class '{}'", class_name)),
        };
        match class.methods.iter().find(|m| m.name == super_name(method)) {
            Some(m) => Ok(m.return_type.clone().unwrap_or(Type::Primitive("unknown".to_string()))),
            None => Err(format!(
                "super.{}() has nothing to call: '{}' does not override an inherited '{}'. super reaches the version a method replaced, so it only means something inside that override.",
                method, class_name, method
            )),
        }
    }

    pub fn eval_type(&self, expr: &Expr) -> Result<Type, String> {
        match expr {
            Expr::Number(_) => Ok(Type::Primitive("u256".to_string())),
            Expr::StringLiteral(_) => Ok(Type::Primitive("String".to_string())),
            Expr::Identifier(name) => {
                if let Some(info) = self.lookup_symbol(name) {
                    Ok(info.type_def.clone())
                } else if name == "true" || name == "false" {
                    Ok(Type::Primitive("bool".to_string()))
                } else if self.loaded_enums.contains_key(name) {
                    Ok(Type::Primitive(name.to_string()))
                } else if self.loaded_classes.contains_key(name) {
                    Ok(Type::Primitive(name.to_string()))
                } else {
                    Err(format!("Undefined identifier: {}", name))
                }
            }
            Expr::FnCall { name, args } if args.len() == 1 && self.loaded_classes.contains_key(name) => {
                Ok(Type::Primitive(name.clone()))
            }
            Expr::FnCall { name, .. } if self.function_return_types.contains_key(name) => {
                Ok(self.function_return_types[name].clone())
            }
            Expr::Instantiation { type_def, args } => {
                if let Type::Primitive(class_name) = type_def {
                    if let Some(class_decl) = self.loaded_classes.get(class_name) {
                        let mut found_constructor = false;
                        for method in &class_decl.methods {
                            if method.name == "new" {
                                found_constructor = true;
                                if method.parameters.len() != args.len() {
                                    return Err(format!("Constructor for '{}' expects {} arguments, but {} were provided.", class_name, method.parameters.len(), args.len()));
                                }
                                println!("    [Constructor Engine] Successfully matched 'new {}()' constructor.", class_name);
                                break;
                            }
                        }
                        if !found_constructor && !args.is_empty() {
                            return Err(format!("Class '{}' has no 'fn new()' constructor, but arguments were provided.", class_name));
                        }
                        if class_decl.is_global {
                            return Ok(Type::Primitive("Account".to_string()));
                        }
                    }
                }
                Ok(type_def.clone())
            }
            Expr::MethodCall { base, method, args } => {
                if matches!(&**base, Expr::Identifier(b) if b == "super") {
                    return self.eval_super_call(method);
                }
                let base_type = self.eval_type(base)?;
                if let Type::Generic { name, args: type_args } = &base_type {
                    if name == "HashMap" && type_args.len() == 2 {
                        match method.as_str() {
                            "get" => return Ok(type_args[1].clone()),
                            "set" => return Ok(Type::Primitive("unknown".to_string())),
                            _ => {}
                        }
                    }
                    if let Some(class_decl) = self.loaded_classes.get(name) {
                        if let Some(m) = class_decl.methods.iter().find(|m| &m.name == method) {
                            return match &m.return_type {
                                Some(rt) => {
                                    let mut subst = HashMap::new();
                                    for (i, gp) in class_decl.generic_params.iter().enumerate() {
                                        if let Some(a) = type_args.get(i) {
                                            subst.insert(gp.name.clone(), a.clone());
                                        }
                                    }
                                    Ok(substitute_generic_type(rt, &subst))
                                }
                                None => Ok(Type::Primitive("unknown".to_string())),
                            };
                        }
                    }
                }
                if let Type::Primitive(name) = &base_type {
                    let is_numeric = matches!(name.as_str(),
                        "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
                        "i8" | "i16" | "i32" | "i64" | "i128" | "i256" | "f32" | "f64");
                    if is_numeric {
                        match method.as_str() {
                            "saturate" => return Ok(base_type.clone()),
                            "as_bytes" | "as_bits" => return Ok(Type::Array(Box::new(Type::Primitive("u8".to_string())))),
                            _ => {}
                        }
                    }
                }
                if let Type::Primitive(class_name) = &base_type {
                    if self.loaded_interfaces.contains(class_name) {
                        self.has_external_calls.set(true);
                    }
                    if class_name == "Account" {
                        match method.as_str() {
                            "balance" => return Ok(Type::Primitive("u256".to_string())),
                            "pay" => {
                                if args.len() != 1 {
                                    return Err("Account.pay() expects 1 argument (the amount)".to_string());
                                }
                                let amt = self.eval_type(&args[0])?;
                                if !matches!(&amt, Type::Primitive(n) if n.starts_with('u')) {
                                    return Err(format!("Account.pay() amount must be an unsigned integer, got {:?}", amt));
                                }
                                self.has_external_calls.set(true);
                                return Ok(Type::Primitive("bool".to_string()));
                            }
                            "transfer" => {
                                if args.len() != 1 {
                                    return Err("Account.transfer() expects 1 argument (the amount)".to_string());
                                }
                                let amt = self.eval_type(&args[0])?;
                                if !matches!(&amt, Type::Primitive(n) if n.starts_with('u')) {
                                    return Err(format!("Account.transfer() amount must be an unsigned integer, got {:?}", amt));
                                }
                                self.has_external_calls.set(true);
                                return Ok(Type::Primitive("unknown".to_string()));
                            }
                            "delegated_to" => return Ok(Type::Primitive("Account".to_string())),
                            "is_delegated" => return Ok(Type::Primitive("bool".to_string())),
                            _ => {}
                        }
                    }
                    let account_ns = matches!(base.as_ref(), Expr::Identifier(n) if n == "Account");
                    if account_ns && matches!(method.as_str(), "create" | "create2" | "create2_address") {
                        let want: usize = match method.as_str() {
                            "create" => 2,
                            "create2" => 3,
                            _ => 2,
                        };
                        let shape = match method.as_str() {
                            "create" => "(code, value)",
                            "create2" => "(code, value, salt)",
                            _ => "(code, salt)",
                        };
                        if args.len() != want {
                            return Err(format!(
                                "Account.{}() expects {} arguments {}, got {}",
                                method, want, shape, args.len()
                            ));
                        }
                        let code = self.eval_type(&args[0])?;
                        if !matches!(&code, Type::Primitive(n) if n == "Bytes" || n == "String") {
                            return Err(format!(
                                "Account.{}() needs the creation bytecode as Bytes, got {:?}",
                                method, code
                            ));
                        }
                        for a in &args[1..] {
                            let t = self.eval_type(a)?;
                            if !matches!(&t, Type::Primitive(n) if n.starts_with('u')) {
                                return Err(format!(
                                    "Account.{}() expects unsigned integers after the code, got {:?}",
                                    method, t
                                ));
                            }
                        }
                        if method != "create2_address" {
                            self.has_external_calls.set(true);
                        }
                        return Ok(Type::Primitive("Account".to_string()));
                    }
                    if class_name == "Crypto" && method == "verify_p256" {
                        if args.len() != 5 {
                            return Err(format!(
                                "Crypto.verify_p256() expects 5 arguments (hash, r, s, qx, qy), got {}",
                                args.len()
                            ));
                        }
                        for a in args {
                            let t = self.eval_type(a)?;
                            if !matches!(&t, Type::Primitive(n) if n.starts_with('u')) {
                                return Err(format!("Crypto.verify_p256() arguments must be unsigned integers, got {:?}", t));
                            }
                        }
                        return Ok(Type::Primitive("bool".to_string()));
                    }
                    if class_name == "String" || class_name == "Bytes" {
                        match method.as_str() {
                            "concat" => {
                                if args.len() != 1 {
                                    return Err(format!("{}.concat() expects 1 argument, got {}", class_name, args.len()));
                                }
                                let other = self.eval_type(&args[0])?;
                                if !matches!(&other, Type::Primitive(n) if n == "String" || n == "Bytes") {
                                    return Err(format!("{}.concat() expects a String or Bytes argument, got {:?}", class_name, other));
                                }
                                return Ok(base_type.clone());
                            }
                            "slice" => {
                                if args.len() != 2 {
                                    return Err(format!("{}.slice() expects 2 arguments (start, end), got {}", class_name, args.len()));
                                }
                                for a in args {
                                    let t = self.eval_type(a)?;
                                    if !matches!(&t, Type::Primitive(n) if n.starts_with('u')) {
                                        return Err(format!("{}.slice() bounds must be unsigned integers, got {:?}", class_name, t));
                                    }
                                }
                                return Ok(base_type.clone());
                            }
                            _ => {}
                        }
                    }
                    if let Some(class_decl) = self.loaded_classes.get(class_name) {
                        for class_method in &class_decl.methods {
                            if class_method.name == *method {
                                return Ok(class_method.return_type.clone().unwrap_or(Type::Primitive("unknown".to_string())));
                            }
                        }
                        if method == "serialize" && class_decl.parents.iter().any(|p| p == "Serializable") {
                            return Ok(Type::Array(Box::new(Type::Primitive("u8".to_string()))));
                        }
                        return Err(format!("Method '{}' not found on class '{}'", method, class_name));
                    } else if let Some(enum_decl) = self.loaded_enums.get(class_name) {
                        for variant in &enum_decl.variants {
                            if variant.name == *method {
                                if let Some(payload_type) = &variant.payload {
                                    if args.len() != 1 {
                                        return Err(format!("Enum variant '{}.{}' expects 1 payload argument", class_name, method));
                                    }
                                    let arg_type = self.eval_type(&args[0])?;
                                    if !self.is_assignable(payload_type, &arg_type) {
                                        return Err(format!("Type mismatch in enum payload for '{}.{}'. Expected {:?}, got {:?}", class_name, method, payload_type, arg_type));
                                    }
                                }
                                return Ok(Type::Primitive(class_name.clone()));
                            }
                        }
                        return Err(format!("Variant '{}' not found on enum '{}'", method, class_name));
                    }
                }
                if let Type::Array(inner) = &base_type {
                    match method.as_str() {
                        "push" => {
                            if crate::codegen::translator::is_struct_type(self, inner) {
                                if !args.is_empty() {
                                    return Err(format!(
                                        "push() on an array of struct '{}' takes no argument: gum has no struct copy. Append a zeroed element with arr.push(), then set its fields, arr[arr.length - 1].field = v.",
                                        type_name(inner)
                                    ));
                                }
                                return Ok(Type::Primitive("unknown".to_string()));
                            }
                            if args.len() != 1 {
                                return Err(format!("push() expects 1 argument (the value), got {}", args.len()));
                            }
                            let v = self.eval_type(&args[0])?;
                            if !self.is_assignable(inner, &v) {
                                return Err(format!("push() type mismatch: array holds {:?}, got {:?}", inner, v));
                            }
                            return Ok(Type::Primitive("unknown".to_string()));
                        }
                        "pop" => {
                            if !args.is_empty() {
                                return Err(format!("pop() takes no arguments, got {}", args.len()));
                            }
                            return Ok((**inner).clone());
                        }
                        "len" => {
                            if !args.is_empty() {
                                return Err(format!("len() takes no arguments, got {}", args.len()));
                            }
                            return Ok(Type::Primitive("u256".to_string()));
                        }
                        "get" => {
                            if args.len() != 1 {
                                return Err(format!("get() expects 1 argument (the index), got {}", args.len()));
                            }
                            let i = self.eval_type(&args[0])?;
                            if !matches!(&i, Type::Primitive(n) if n.starts_with('u')) {
                                return Err(format!("get() index must be an unsigned integer, got {:?}", i));
                            }
                            return Ok((**inner).clone());
                        }
                        _ => {}
                    }
                }
                Err(format!("Cannot call method '{}' on type {:?}", method, base_type))
            }
            Expr::PropertyAccess { base, property } => {
                let base_type = self.eval_type(base)?;
                if let Type::Primitive(class_name) = &base_type {
                    if let Some(class_decl) = self.loaded_classes.get(class_name) {
                        for field in &class_decl.fields {
                            if field.name == *property {
                                return Ok(field.type_def.clone());
                            }
                        }
                        return Err(format!("Property '{}' not found on class '{}'", property, class_name));
                    } else if let Some(enum_decl) = self.loaded_enums.get(class_name) {
                        for variant in &enum_decl.variants {
                            if variant.name == *property {
                                if variant.payload.is_some() {
                                    return Err(format!("Enum variant '{}.{}' requires a payload", class_name, property));
                                }
                                return Ok(Type::Primitive(class_name.clone()));
                            }
                        }
                        return Err(format!("Variant '{}' not found on enum '{}'", property, class_name));
                    }
                }
                if matches!(&base_type, Type::Array(_) | Type::FixedArray(..)) && property == "length" {
                    return Ok(Type::Primitive("u256".to_string()));
                }
                Err(format!("Cannot access property '{}' on type {:?}", property, base_type))
            }
            Expr::IndexAccess { base, index } => {
                let base_type = self.eval_type(base)?;
                if matches!(&base_type, Type::Primitive(n) if n == "String" || n == "Bytes") {
                    let idx = self.eval_type(index)?;
                    if !matches!(&idx, Type::Primitive(n) if n.starts_with('u')) {
                        return Err(format!("Index into a {:?} must be an unsigned integer, got {:?}", base_type, idx));
                    }
                    return Ok(Type::Primitive("u8".to_string()));
                }
                if let Type::Generic { name, args } = &base_type {
                    if name == "HashMap" && args.len() == 2 {
                        let key_type = self.eval_type(index)?;
                        if !self.is_assignable(&args[0], &key_type) {
                            return Err(format!("Invalid key type for HashMap. Expected {:?}, got {:?}", args[0], key_type));
                        }
                        return Ok(args[1].clone());
                    }
                }
                if let Type::Array(inner) = &base_type {
                    return Ok(*inner.clone());
                }
                if let Type::FixedArray(inner, _) = &base_type {
                    return Ok(*inner.clone());
                }
                Err(format!("Cannot index into type {:?}", base_type))
            }
            Expr::BinaryOp { left, operator, right } => {
                let left_type = self.eval_type(left)?;
                let right_type = self.eval_type(right)?;
                
                let is_numeric = |t: &Type| -> bool {
                    if let Type::Primitive(name) = t {
                        matches!(name.as_str(),
                            "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
                            "i8" | "i16" | "i32" | "i64" | "i128" | "i256" | "f32" | "f64")
                    } else {
                        false
                    }
                };

                let is_bool = |t: &Type| -> bool {
                    if let Type::Primitive(name) = t {
                        name == "bool"
                    } else {
                        false
                    }
                };

                match operator.as_str() {
                    "+" | "-" | "*" | "/" | "%" | "**" => {
                        if !is_numeric(&left_type) || !is_numeric(&right_type) {
                            return Err(format!("Binary operator '{}' requires numeric types. Found {:?} and {:?}", operator, left_type, right_type));
                        }
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if !self.is_assignable(&left_type, &right_type) && !self.is_assignable(&right_type, &left_type) {
                            return Err(format!("Cannot compare {:?} and {:?}", left_type, right_type));
                        }
                        return Ok(Type::Primitive("bool".to_string()));
                    }
                    "&&" | "||" => {
                        if !is_bool(&left_type) || !is_bool(&right_type) {
                            return Err(format!("Logical operator '{}' requires bool types. Found {:?} and {:?}", operator, left_type, right_type));
                        }
                        return Ok(Type::Primitive("bool".to_string()));
                    }
                    _ => {}
                }

                let _ = self.check_fixed_point_math(expr);
                Ok(left_type)
            }
            Expr::FString(segments) => {
                for seg in segments {
                    if let FStringSegment::Interp(e) = seg {
                        self.eval_type(e)?;
                    }
                }
                Ok(Type::Primitive("String".to_string()))
            }
            Expr::Neg(inner) => {
                match self.eval_type(inner)? {
                    Type::Primitive(name) if matches!(name.as_str(),
                        "u8" | "u16" | "u32" | "u64" | "u128" | "u256" |
                        "i8" | "i16" | "i32" | "i64" | "i128" | "i256") => {
                        Ok(Type::Primitive("i256".to_string()))
                    }
                    other => Ok(other),
                }
            }
            Expr::Not(inner) => {
                let t = self.eval_type(inner)?;
                if !matches!(&t, Type::Primitive(n) if n == "bool") {
                    return Err(format!("Logical NOT ('!') requires a bool operand, found {:?}", t));
                }
                Ok(Type::Primitive("bool".to_string()))
            }
            Expr::ArrayLiteral(elements) => {
                if elements.is_empty() {
                    return Ok(Type::FixedArray(Box::new(Type::Primitive("u256".to_string())), 0));
                }
                let elem_type = self.eval_type(&elements[0])?;
                for e in &elements[1..] {
                    let t = self.eval_type(e)?;
                    if !self.is_assignable(&elem_type, &t) {
                        return Err(format!("Array literal elements must share a type. Expected {:?}, found {:?}", elem_type, t));
                    }
                }
                Ok(Type::FixedArray(Box::new(elem_type), elements.len()))
            }
            _ => Ok(Type::Primitive("unknown".to_string()))
        }
    }
}

// A type as it is spelled in gum source. Structural, so it doubles as a
// comparison key for types (which have no PartialEq), while staying readable
// enough to print straight back at the user.
// Whether this expression names Class.field (or, inside a method, the
// equivalent self.field).
fn targets_field(e: &Expr, class_name: &str, field: &str) -> bool {
    match e {
        Expr::PropertyAccess { base, property } => {
            property == field
                && matches!(&**base, Expr::Identifier(b) if b == class_name || b == "self")
        }
        _ => false,
    }
}

// Whether body assigns Class.field *anywhere*, reachable or not.
//
// Only used to tell two errors apart: a field nobody ever mentions gets a
// different message from one assigned on some paths but not all.
fn assigns_field(body: &[Spanned<Statement>], class_name: &str, field: &str) -> bool {
    body.iter().any(|s| match &s.node {
        Statement::Assignment { target, .. } => targets_field(target, class_name, field),
        Statement::IfElse { if_body, else_body, .. } => {
            assigns_field(if_body, class_name, field)
                || else_body.as_ref().is_some_and(|b| assigns_field(b, class_name, field))
        }
        Statement::WhileLoop { body, .. } => assigns_field(body, class_name, field),
        Statement::ForLoop { body, .. } => assigns_field(body, class_name, field),
        Statement::Match { arms, .. } => {
            arms.iter().any(|a| assigns_field(&a.body, class_name, field))
        }
        _ => false,
    })
}

// Whether e reads Class.field anywhere inside it.
fn expr_reads_field(e: &Expr, class_name: &str, field: &str) -> bool {
    if targets_field(e, class_name, field) {
        return true;
    }
    let any = |es: &[Expr]| es.iter().any(|x| expr_reads_field(x, class_name, field));
    match e {
        Expr::PropertyAccess { base, .. } => expr_reads_field(base, class_name, field),
        Expr::FnCall { args, .. } | Expr::Instantiation { args, .. } | Expr::ArrayLiteral(args) => any(args),
        Expr::MethodCall { base, args, .. } => {
            expr_reads_field(base, class_name, field) || any(args)
        }
        Expr::IndexAccess { base, index } => {
            expr_reads_field(base, class_name, field) || expr_reads_field(index, class_name, field)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_reads_field(left, class_name, field) || expr_reads_field(right, class_name, field)
        }
        Expr::Neg(inner) | Expr::Not(inner) => expr_reads_field(inner, class_name, field),
        Expr::FString(segs) => segs.iter().any(|s| match s {
            FStringSegment::Interp(x) => expr_reads_field(x, class_name, field),
            FStringSegment::Literal(_) => false,
        }),
        _ => false,
    }
}

// Whether body *reads* Class.field, anywhere a value is consumed, as
// opposed to an assignment's target.
//
// Inside fn new this is what an immutable cannot survive: its value is not
// in the code yet, so the read has nothing to find. Left unchecked it reaches
// solc as Immutable "C_a" used before initialization against generated Yul
// the author never wrote.
fn reads_field(body: &[Spanned<Statement>], class_name: &str, field: &str) -> bool {
    let re = |e: &Expr| expr_reads_field(e, class_name, field);
    body.iter().any(|s| match &s.node {
        Statement::Assignment { value, .. } => re(value),
        Statement::VarDecl { value, .. } => value.as_ref().is_some_and(re),
        Statement::Assert { condition, message } => re(condition) || message.as_ref().is_some_and(re),
        Statement::Revert { args, .. } | Statement::Call { args, .. } => args.iter().any(re),
        Statement::Return { value } => value.as_ref().is_some_and(re),
        Statement::Expression(e) | Statement::Delete { target: e } => re(e),
        Statement::BitwiseFlip { index, value, .. } => re(index) || re(value),
        Statement::IfElse { condition, if_body, else_body } => {
            re(condition)
                || reads_field(if_body, class_name, field)
                || else_body.as_ref().is_some_and(|b| reads_field(b, class_name, field))
        }
        Statement::WhileLoop { condition, body } => re(condition) || reads_field(body, class_name, field),
        Statement::ForLoop { iterable, body, .. } => re(iterable) || reads_field(body, class_name, field),
        Statement::Match { expr, arms } => {
            re(expr) || arms.iter().any(|a| reads_field(&a.body, class_name, field))
        }
        Statement::UnsafeBlock(_) => false,
    })
}

// Whether body cannot complete normally, every path through it ends in a
// return or a revert.
//
// A branch that diverges never reaches the code after it, so it imposes no
// obligation to assign: if c: C.x = 1 else: revert(E) does define C.x on
// every path that survives.
fn diverges(body: &[Spanned<Statement>]) -> bool {
    body.iter().any(|s| match &s.node {
        Statement::Return { .. } | Statement::Revert { .. } => true,
        Statement::IfElse { if_body, else_body, .. } => {
            diverges(if_body) && else_body.as_ref().is_some_and(|b| diverges(b))
        }
        Statement::Match { arms, .. } => arms.iter().all(|a| diverges(&a.body)),
        _ => false,
    })
}

// Whether every path through body that completes normally has assigned
// Class.field, real definite-assignment analysis, not "is it mentioned".
//
// This is what makes an immutable trustworthy. A field assigned on only some
// paths is baked into the deployed code as zero on the others, permanently and
// silently, so "assigned somewhere" is not a strong enough question to ask.
//
// A branch either assigns or diverges. A loop never counts: while c: C.x = 1
// may run zero times. A match counts only if every arm does, which is sound
// because a non-exhaustive match is already rejected elsewhere.
fn definitely_assigns(body: &[Spanned<Statement>], class_name: &str, field: &str) -> bool {
    body.iter().any(|s| match &s.node {
        Statement::Assignment { target, .. } => targets_field(target, class_name, field),
        Statement::IfElse { if_body, else_body, .. } => {
            let covered = |b: &[Spanned<Statement>]| definitely_assigns(b, class_name, field) || diverges(b);
            covered(if_body) && else_body.as_ref().is_some_and(|b| covered(b))
        }
        Statement::Match { arms, .. } => arms.iter().all(|a| {
            definitely_assigns(&a.body, class_name, field) || diverges(&a.body)
        }),
        _ => false,
    })
}

// The name an overridden parent method is kept under so super.foo() can
// reach it. Not spellable in source: super_ is a legal identifier prefix, but
// a class declaring its own super_foo alongside an override of foo is
// rejected by the duplicate-method check.
pub fn super_name(method: &str) -> String {
    format!("super_{}", method)
}

fn type_name(t: &Type) -> String {
    match t {
        Type::Primitive(n) => n.clone(),
        Type::Array(inner) => format!("[{}]", type_name(inner)),
        Type::FixedArray(inner, n) => format!("[{}; {}]", type_name(inner), n),
        Type::Generic { name, args } => {
            format!("{}({})", name, args.iter().map(type_name).collect::<Vec<_>>().join(", "))
        }
    }
}

// A method's signature, for comparing a class against an interface.
fn signature_key(f: &FnDecl) -> String {
    let params: Vec<String> = f.parameters.iter().map(|p| type_name(&p.type_def)).collect();
    match &f.return_type {
        Some(t) => format!("fn {}({}) -> {}", f.name, params.join(", "), type_name(t)),
        None => format!("fn {}({})", f.name, params.join(", ")),
    }
}

// Every type name a declaration mentions: parents, generic bounds, field types, and each method signature.
// Bodies are not walked; a body can only name types its signature or fields already reach, and a free function it calls comes in on its own use line.
fn decl_refs(d: &Declaration) -> Vec<String> {
    let mut out = Vec::new();
    match d {
        Declaration::Class(c) => {
            out.extend(c.parents.iter().cloned());
            out.extend(c.generic_params.iter().map(|g| g.bound.clone()));
            for f in &c.fields {
                collect_type_names(&f.type_def, &mut out);
            }
            for m in &c.methods {
                for p in &m.parameters {
                    collect_type_names(&p.type_def, &mut out);
                }
                if let Some(rt) = &m.return_type {
                    collect_type_names(rt, &mut out);
                }
            }
        }
        Declaration::Function(f) => {
            for p in &f.parameters {
                collect_type_names(&p.type_def, &mut out);
            }
            if let Some(rt) = &f.return_type {
                collect_type_names(rt, &mut out);
            }
        }
        Declaration::Error(e) => {
            for p in &e.parameters {
                collect_type_names(&p.type_def, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn collect_type_names(t: &Type, out: &mut Vec<String>) {
    match t {
        Type::Primitive(n) => out.push(n.clone()),
        Type::Generic { name, args } => {
            out.push(name.clone());
            for a in args {
                collect_type_names(a, out);
            }
        }
        Type::Array(inner) => collect_type_names(inner, out),
        Type::FixedArray(inner, _) => collect_type_names(inner, out),
    }
}

fn decl_name(d: &Declaration) -> Option<String> {
    match d {
        Declaration::Class(c) => Some(c.name.clone()),
        Declaration::Enum(e) => Some(e.name.clone()),
        Declaration::Error(e) => Some(e.name.clone()),
        Declaration::Function(f) => Some(f.name.clone()),
        _ => None,
    }
}

// The declarations a `use module.Symbol` merges: the symbol itself plus everything its signature reaches inside the same module.
// Returns indices so the caller keeps the module AST cached and clones only what it takes. An unknown symbol yields nothing, and the caller reports that separately.
fn symbol_closure(decls: &[Declaration], symbol: &str) -> Vec<usize> {
    let index: HashMap<String, usize> = decls
        .iter()
        .enumerate()
        .filter_map(|(i, d)| decl_name(d).map(|n| (n.to_ascii_lowercase(), i)))
        .collect();
    let start = match index.get(&symbol.to_ascii_lowercase()) {
        Some(i) => *i,
        None => return Vec::new(),
    };
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack = vec![start];
    let mut out = Vec::new();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        out.push(i);
        for r in decl_refs(&decls[i]) {
            if let Some(j) = index.get(&r.to_ascii_lowercase()) {
                stack.push(*j);
            }
        }
    }
    // Declaration order, so a parent still lands before the child that names it.
    out.sort_unstable();
    out
}
