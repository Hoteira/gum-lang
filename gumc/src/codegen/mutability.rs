use crate::ast::*;
use crate::semantic::TypeChecker;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Mut {
    Pure,
    View,
    NonPayable,
}

pub type MutMap = HashMap<String, Mut>;

fn storage_root(tc: &TypeChecker, class: &ClassDecl, e: &Expr) -> bool {
    match e {
        Expr::Identifier(n) => {
            if n == "self" {
                return class.is_global;
            }
            tc.loaded_classes
                .get(n)
                .map(|c| c.is_global)
                .unwrap_or(false)
        }
        Expr::PropertyAccess { base, .. }
        | Expr::IndexAccess { base, .. }
        | Expr::MethodCall { base, .. } => storage_root(tc, class, base),
        _ => false,
    }
}

fn is_contract_type(tc: &TypeChecker, t: &Type) -> bool {
    match t {
        Type::Primitive(n) => tc
            .loaded_classes
            .get(n)
            .map(|c| c.is_global)
            .unwrap_or(false),
        _ => false,
    }
}

fn max_of(items: impl Iterator<Item = Mut>) -> Mut {
    items.max().unwrap_or(Mut::Pure)
}

fn expr_effect(tc: &TypeChecker, class: &ClassDecl, e: &Expr, map: &MutMap) -> Mut {
    let sub = |x: &Expr| expr_effect(tc, class, x, map);
    match e {
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Identifier(_) => Mut::Pure,
        Expr::Neg(x) | Expr::Not(x) => sub(x),
        Expr::BinaryOp { left, right, .. } => sub(left).max(sub(right)),
        Expr::ArrayLiteral(v) => max_of(v.iter().map(sub)),
        Expr::FString(segs) => max_of(segs.iter().filter_map(|s| match s {
            FStringSegment::Interp(x) => Some(sub(x)),
            _ => None,
        })),
        Expr::PropertyAccess { base, .. } => {
            let own = if storage_root(tc, class, base) {
                Mut::View
            } else {
                Mut::Pure
            };
            own.max(sub(base))
        }
        Expr::IndexAccess { base, index } => {
            let own = if storage_root(tc, class, base) {
                Mut::View
            } else {
                Mut::Pure
            };
            own.max(sub(base)).max(sub(index))
        }

        Expr::StaticCall { args, .. } => max_of(args.iter().map(sub)),
        Expr::Instantiation { type_def, args } => {
            let own = if is_contract_type(tc, type_def) {
                Mut::NonPayable
            } else {
                Mut::Pure
            };
            own.max(max_of(args.iter().map(sub)))
        }
        Expr::FnCall { name, args } => {
            let own = match name.as_str() {
                "log" => Mut::NonPayable,
                "keccak256" | "ecrecover" | "assert" => Mut::Pure,

                _ => map.get(name).copied().unwrap_or(Mut::NonPayable),
            };
            own.max(max_of(args.iter().map(sub)))
        }
        Expr::MethodCall { base, method, args } => {
            let a = max_of(args.iter().map(sub));

            if let Expr::FnCall { name, .. } = &**base {
                if tc.loaded_interfaces.contains(name) {
                    return Mut::NonPayable;
                }
            }
            if let Expr::Identifier(n) = &**base {
                if n == "Message" || n == "Block" {
                    return Mut::View.max(a);
                }
            }
            if storage_root(tc, class, base) {
                let own = match method.as_str() {
                    "length" | "len" | "get" => Mut::View,
                    _ => Mut::NonPayable,
                };
                return own.max(a);
            }
            let own = match method.as_str() {
                "balance" | "delegated_to" | "is_delegated" => Mut::View,

                "verify_p256" => Mut::View,
                "saturate" | "as_bytes" | "as_bits" | "serialize" | "concat" | "slice" => Mut::Pure,
                _ => map.get(method).copied().unwrap_or(Mut::NonPayable),
            };
            own.max(a).max(sub(base))
        }
    }
}

fn stmt_effect(tc: &TypeChecker, class: &ClassDecl, s: &Statement, map: &MutMap) -> Mut {
    let sub = |x: &Expr| expr_effect(tc, class, x, map);
    let body = |b: &Vec<Spanned<Statement>>| stmts_effect(tc, class, b, map);
    match s {
        Statement::VarDecl { value, .. } => value.as_ref().map(&sub).unwrap_or(Mut::Pure),
        Statement::Assignment { target, value } => {
            let t = if storage_root(tc, class, target) {
                Mut::NonPayable
            } else {
                sub(target)
            };
            t.max(sub(value))
        }
        Statement::BitwiseFlip { index, value, .. } => sub(index).max(sub(value)),
        Statement::Assert { condition, message } => {
            sub(condition).max(message.as_ref().map(&sub).unwrap_or(Mut::Pure))
        }
        Statement::Revert { error } => sub(error),
        Statement::Delete { target } => {
            if storage_root(tc, class, target) {
                Mut::NonPayable
            } else {
                sub(target)
            }
        }
        Statement::Return { value } => value.as_ref().map(&sub).unwrap_or(Mut::Pure),
        Statement::IfElse {
            condition,
            if_body,
            else_body,
        } => sub(condition)
            .max(body(if_body))
            .max(else_body.as_ref().map(body).unwrap_or(Mut::Pure)),
        Statement::ForLoop {
            iterable, body: b, ..
        } => sub(iterable).max(body(b)),
        Statement::WhileLoop { condition, body: b } => sub(condition).max(body(b)),
        Statement::Match { expr, arms } => sub(expr).max(max_of(
            arms.iter().map(|a| stmts_effect(tc, class, &a.body, map)),
        )),
        Statement::TryCatch {
            try_body,
            catch_body,
        } => body(try_body).max(body(catch_body)),

        Statement::ScopedTryCall { catch_body, .. } => Mut::NonPayable.max(body(catch_body)),
        Statement::ReturnCaptures(_) => Mut::NonPayable,
        Statement::Expression(e) => sub(e),

        Statement::Call { .. } => Mut::NonPayable,
        Statement::UnsafeBlock(_) => Mut::NonPayable,
    }
}

fn stmts_effect(
    tc: &TypeChecker,
    class: &ClassDecl,
    b: &[Spanned<Statement>],
    map: &MutMap,
) -> Mut {
    max_of(b.iter().map(|s| stmt_effect(tc, class, &s.node, map)))
}

pub fn analyze_class(tc: &TypeChecker, class: &ClassDecl) -> MutMap {
    let mut map: MutMap = class
        .methods
        .iter()
        .map(|m| (m.name.clone(), Mut::Pure))
        .collect();
    for _ in 0..class.methods.len() + 2 {
        let mut changed = false;
        for m in &class.methods {
            let e = stmts_effect(tc, class, &m.body, &map);
            if e > map[&m.name] {
                map.insert(m.name.clone(), e);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    map
}

pub fn state_mutability(f: &FnDecl, map: &MutMap) -> &'static str {
    if f.modifiers.iter().any(|m| m == "payable") {
        return "payable";
    }
    if f.modifiers.iter().any(|m| m == "once") {
        return "nonpayable";
    }
    match map.get(&f.name).copied().unwrap_or(Mut::NonPayable) {
        Mut::Pure => "pure",
        Mut::View => "view",
        Mut::NonPayable => "nonpayable",
    }
}

pub fn is_read_only(f: &FnDecl, map: &MutMap) -> bool {
    matches!(state_mutability(f, map), "view" | "pure")
}

fn expr_calls_out(
    tc: &TypeChecker,
    class: &ClassDecl,
    e: &Expr,
    map: &HashMap<String, bool>,
) -> bool {
    let sub = |x: &Expr| expr_calls_out(tc, class, x, map);
    match e {
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Identifier(_) => false,
        Expr::Neg(x) | Expr::Not(x) => sub(x),
        Expr::BinaryOp { left, right, .. } => sub(left) || sub(right),
        Expr::ArrayLiteral(v) => v.iter().any(sub),
        Expr::FString(segs) => segs
            .iter()
            .any(|s| matches!(s, FStringSegment::Interp(x) if sub(x))),
        Expr::PropertyAccess { base, .. } => sub(base),
        Expr::IndexAccess { base, index } => sub(base) || sub(index),

        Expr::StaticCall { args, .. } => args.iter().any(sub),
        Expr::Instantiation { type_def, args } => {
            is_contract_type(tc, type_def) || args.iter().any(sub)
        }
        Expr::FnCall { name, args } => {
            let own = match name.as_str() {
                "log" | "indexed" | "keccak256" | "ecrecover" | "assert" => false,

                _ => map.get(name).copied().unwrap_or(true),
            };
            own || args.iter().any(sub)
        }
        Expr::MethodCall { base, method, args } => {
            let a = args.iter().any(sub);
            if let Expr::FnCall { name, .. } = &**base {
                if tc.loaded_interfaces.contains(name) {
                    return true;
                }
            }
            if let Expr::Identifier(n) = &**base {
                if n == "Message" || n == "Block" {
                    return a;
                }
            }
            if storage_root(tc, class, base) {
                let own = match method.as_str() {
                    "length" | "len" | "get" | "push" | "pop" => false,
                    _ => map.get(method).copied().unwrap_or(true),
                };
                return own || a;
            }
            let own = match method.as_str() {
                "transfer" | "pay" | "create" | "create2" => true,
                "balance" | "delegated_to" | "is_delegated" | "verify_p256" => false,
                "saturate" | "as_bytes" | "as_bits" | "serialize" | "concat" | "slice" => false,
                _ => map.get(method).copied().unwrap_or(true),
            };
            own || a || sub(base)
        }
    }
}

fn stmt_calls_out(
    tc: &TypeChecker,
    class: &ClassDecl,
    s: &Statement,
    map: &HashMap<String, bool>,
) -> bool {
    let sub = |x: &Expr| expr_calls_out(tc, class, x, map);
    let body =
        |b: &Vec<Spanned<Statement>>| b.iter().any(|s| stmt_calls_out(tc, class, &s.node, map));
    match s {
        Statement::VarDecl { value, .. } => value.as_ref().map(&sub).unwrap_or(false),
        Statement::Assignment { target, value } => sub(target) || sub(value),
        Statement::BitwiseFlip { index, value, .. } => sub(index) || sub(value),
        Statement::Assert { condition, message } => {
            sub(condition) || message.as_ref().map(&sub).unwrap_or(false)
        }
        Statement::Revert { error } => sub(error),
        Statement::Delete { target } => sub(target),
        Statement::Return { value } => value.as_ref().map(&sub).unwrap_or(false),
        Statement::IfElse {
            condition,
            if_body,
            else_body,
        } => sub(condition) || body(if_body) || else_body.as_ref().map(body).unwrap_or(false),
        Statement::ForLoop {
            iterable, body: b, ..
        } => sub(iterable) || body(b),
        Statement::WhileLoop { condition, body: b } => sub(condition) || body(b),
        Statement::Match { expr, arms } => {
            sub(expr)
                || arms.iter().any(|a| {
                    a.body
                        .iter()
                        .any(|s| stmt_calls_out(tc, class, &s.node, map))
                })
        }
        Statement::TryCatch {
            try_body,
            catch_body,
        } => body(try_body) || body(catch_body),

        Statement::ScopedTryCall { .. } => true,
        Statement::ReturnCaptures(_) => false,
        Statement::Expression(e) => sub(e),

        Statement::Call { .. } => true,
        Statement::UnsafeBlock(_) => true,
    }
}

pub fn analyze_external_calls(tc: &TypeChecker, class: &ClassDecl) -> HashMap<String, bool> {
    let mut map: HashMap<String, bool> = class
        .methods
        .iter()
        .map(|m| (m.name.clone(), false))
        .collect();
    for _ in 0..class.methods.len() + 2 {
        let mut changed = false;
        for m in &class.methods {
            if !map[&m.name]
                && m.body
                    .iter()
                    .any(|s| stmt_calls_out(tc, class, &s.node, &map))
            {
                map.insert(m.name.clone(), true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    map
}

pub fn makes_external_call(f: &FnDecl, map: &HashMap<String, bool>) -> bool {
    map.get(&f.name).copied().unwrap_or(true)
}
