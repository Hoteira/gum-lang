// What a function does to chain state, which is what the ABI's stateMutability
// tells callers. Getting it wrong in one direction is only untidy: calling a
// view function "nonpayable" makes a wallet prompt for gas to read a balance.
// Getting it wrong the other way is dangerous: a caller told a function is
// "view" reaches it with eth_call, so a write silently never happens.
//
// So the analysis is a whitelist, not a blacklist. Anything it does not
// positively recognise as read-only counts as a write, and an unknown call is
// assumed to do the worst thing.

use crate::ast::*;
use crate::semantic::TypeChecker;
use std::collections::HashMap;

// Ordered weakest first, so combining two effects is a max().
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Mut {
    Pure,
    View,
    NonPayable,
}

pub type MutMap = HashMap<String, Mut>;

// Whether an lvalue or expression bottoms out at contract storage, `Vault.total` or `self.x` inside a contract, as opposed to a local or a memory value.
// Walks to the root of the chain, so `Vault.stakes[who].amount` counts.
fn storage_root(tc: &TypeChecker, class: &ClassDecl, e: &Expr) -> bool {
    match e {
        Expr::Identifier(n) => {
            if n == "self" {
                return class.is_global;
            }
            tc.loaded_classes.get(n).map(|c| c.is_global).unwrap_or(false)
        }
        Expr::PropertyAccess { base, .. }
        | Expr::IndexAccess { base, .. }
        | Expr::MethodCall { base, .. } => storage_root(tc, class, base),
        _ => false,
    }
}

fn is_contract_type(tc: &TypeChecker, t: &Type) -> bool {
    match t {
        Type::Primitive(n) => tc.loaded_classes.get(n).map(|c| c.is_global).unwrap_or(false),
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
            let own = if storage_root(tc, class, base) { Mut::View } else { Mut::Pure };
            own.max(sub(base))
        }
        Expr::IndexAccess { base, index } => {
            let own = if storage_root(tc, class, base) { Mut::View } else { Mut::Pure };
            own.max(sub(base)).max(sub(index))
        }
        // `new Child(...)` on a contract is a CREATE; on a plain class it is a memory allocation.
        Expr::Instantiation { type_def, args } => {
            let own = if is_contract_type(tc, type_def) { Mut::NonPayable } else { Mut::Pure };
            own.max(max_of(args.iter().map(sub)))
        }
        Expr::FnCall { name, args } => {
            let own = match name.as_str() {
                "log" => Mut::NonPayable,
                "keccak256" | "ecrecover" | "assert" => Mut::Pure,
                // A call to something this pass has not classified, a top-level fn or a builtin it does not know, is assumed to write.
                _ => map.get(name).copied().unwrap_or(Mut::NonPayable),
            };
            own.max(max_of(args.iter().map(sub)))
        }
        Expr::MethodCall { base, method, args } => {
            let a = max_of(args.iter().map(sub));
            // `IERC20(addr).transfer(...)`: a real CALL, and gum emits CALL rather than STATICCALL even for a getter, so it is never view.
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
                // A precompile reached by staticcall reads nothing of ours, but it is still a call out.
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
        Statement::Revert { args, .. } => max_of(args.iter().map(&sub)),
        Statement::Delete { target } => {
            if storage_root(tc, class, target) {
                Mut::NonPayable
            } else {
                sub(target)
            }
        }
        Statement::Return { value } => value.as_ref().map(&sub).unwrap_or(Mut::Pure),
        Statement::IfElse { condition, if_body, else_body } => sub(condition)
            .max(body(if_body))
            .max(else_body.as_ref().map(body).unwrap_or(Mut::Pure)),
        Statement::ForLoop { iterable, body: b, .. } => sub(iterable).max(body(b)),
        Statement::WhileLoop { condition, body: b } => sub(condition).max(body(b)),
        Statement::Match { expr, arms } => {
            sub(expr).max(max_of(arms.iter().map(|a| stmts_effect(tc, class, &a.body, map))))
        }
        Statement::Expression(e) => sub(e),
        // A raw `call target(payload)`, and raw Yul, can do anything.
        Statement::Call { .. } => Mut::NonPayable,
        Statement::UnsafeBlock(_) => Mut::NonPayable,
    }
}

fn stmts_effect(tc: &TypeChecker, class: &ClassDecl, b: &[Spanned<Statement>], map: &MutMap) -> Mut {
    max_of(b.iter().map(|s| stmt_effect(tc, class, &s.node, map)))
}

// Every method's effect, resolved to a fixpoint so a caller inherits what its callees do.
// Starts at Pure and only ever climbs, so the loop terminates and recursion settles rather than spinning.
pub fn analyze_class(tc: &TypeChecker, class: &ClassDecl) -> MutMap {
    let mut map: MutMap = class.methods.iter().map(|m| (m.name.clone(), Mut::Pure)).collect();
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

// The ABI's stateMutability for one function.
// `payable` is a declaration, not something to infer. A `once` function writes its has-run flag, so it is never view however clean its body looks.
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

// Whether a function only reads. Such a function cannot be harmed by reentrancy, so it needs no guard, and it must not carry one: a guard is a TSTORE, which reverts inside the STATICCALL that `view` invites callers to use.
pub fn is_read_only(f: &FnDecl, map: &MutMap) -> bool {
    matches!(state_mutability(f, map), "view" | "pure")
}

// Whether an expression can hand control to another contract, which is the only way reentrancy happens: an interface call, Account.transfer/pay, a CREATE, or a call to a function that itself does one.
// Conservative on the unknown side: a call this pass cannot resolve is assumed to reach out, so the guard is only ever dropped from a function proven not to call anywhere.
fn expr_calls_out(tc: &TypeChecker, class: &ClassDecl, e: &Expr, map: &HashMap<String, bool>) -> bool {
    let sub = |x: &Expr| expr_calls_out(tc, class, x, map);
    match e {
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Identifier(_) => false,
        Expr::Neg(x) | Expr::Not(x) => sub(x),
        Expr::BinaryOp { left, right, .. } => sub(left) || sub(right),
        Expr::ArrayLiteral(v) => v.iter().any(sub),
        Expr::FString(segs) => segs.iter().any(|s| matches!(s, FStringSegment::Interp(x) if sub(x))),
        Expr::PropertyAccess { base, .. } => sub(base),
        Expr::IndexAccess { base, index } => sub(base) || sub(index),
        // new Child(...) on a contract is a CREATE, which runs the child's code and can call back.
        Expr::Instantiation { type_def, args } => {
            is_contract_type(tc, type_def) || args.iter().any(sub)
        }
        Expr::FnCall { name, args } => {
            let own = match name.as_str() {
                // Builtins and markers that emit no call: log writes a topic, indexed just tags a log field, the rest are pure.
                "log" | "indexed" | "keccak256" | "ecrecover" | "assert" => false,
                // An unresolved name is assumed to call out; a known contract method is what the map says.
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
                // Value transfers and deploys hand over control; the rest are local reads or staticcalls that cannot re-enter and write.
                "transfer" | "pay" | "create" | "create2" => true,
                "balance" | "delegated_to" | "is_delegated" | "verify_p256" => false,
                "saturate" | "as_bytes" | "as_bits" | "serialize" | "concat" | "slice" => false,
                _ => map.get(method).copied().unwrap_or(true),
            };
            own || a || sub(base)
        }
    }
}

fn stmt_calls_out(tc: &TypeChecker, class: &ClassDecl, s: &Statement, map: &HashMap<String, bool>) -> bool {
    let sub = |x: &Expr| expr_calls_out(tc, class, x, map);
    let body = |b: &Vec<Spanned<Statement>>| b.iter().any(|s| stmt_calls_out(tc, class, &s.node, map));
    match s {
        Statement::VarDecl { value, .. } => value.as_ref().map(&sub).unwrap_or(false),
        Statement::Assignment { target, value } => sub(target) || sub(value),
        Statement::BitwiseFlip { index, value, .. } => sub(index) || sub(value),
        Statement::Assert { condition, message } => {
            sub(condition) || message.as_ref().map(&sub).unwrap_or(false)
        }
        Statement::Revert { args, .. } => args.iter().any(&sub),
        Statement::Delete { target } => sub(target),
        Statement::Return { value } => value.as_ref().map(&sub).unwrap_or(false),
        Statement::IfElse { condition, if_body, else_body } => {
            sub(condition) || body(if_body) || else_body.as_ref().map(body).unwrap_or(false)
        }
        Statement::ForLoop { iterable, body: b, .. } => sub(iterable) || body(b),
        Statement::WhileLoop { condition, body: b } => sub(condition) || body(b),
        Statement::Match { expr, arms } => {
            sub(expr) || arms.iter().any(|a| a.body.iter().any(|s| stmt_calls_out(tc, class, &s.node, map)))
        }
        Statement::Expression(e) => sub(e),
        // A raw low-level call and raw Yul can reach anywhere.
        Statement::Call { .. } => true,
        Statement::UnsafeBlock(_) => true,
    }
}

// Per-method: does it, transitively, hand control to another contract? Resolved to a fixpoint so a caller inherits its callees, exactly like the mutability pass.
pub fn analyze_external_calls(tc: &TypeChecker, class: &ClassDecl) -> HashMap<String, bool> {
    let mut map: HashMap<String, bool> = class.methods.iter().map(|m| (m.name.clone(), false)).collect();
    for _ in 0..class.methods.len() + 2 {
        let mut changed = false;
        for m in &class.methods {
            if !map[&m.name] && m.body.iter().any(|s| stmt_calls_out(tc, class, &s.node, &map)) {
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

// Whether a function needs a reentrancy guard: only one that can actually hand control away can be re-entered.
pub fn makes_external_call(f: &FnDecl, map: &HashMap<String, bool>) -> bool {
    map.get(&f.name).copied().unwrap_or(true)
}
