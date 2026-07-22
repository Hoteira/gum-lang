use pest_derive::Parser;
use pest::Parser as PestParser;
use crate::ast::*;

#[derive(Parser)]
#[grammar = "parser/gum.pest"]
pub struct GumParser;

// Byte spans of the top-level declarations in preprocessed (brace-delimited)
// source. A declaration ends at the } that returns brace depth to 0, or at a
// ; at depth 0 (use, error, a bodyless class C;).
fn top_level_spans(src: &str) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !in_str && c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'"' {
            in_str = !in_str;
            start.get_or_insert(i);
            i += 1;
            continue;
        }
        if in_str || c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        start.get_or_insert(i);
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth <= 0 {
                    depth = 0;
                    spans.push((start.take().unwrap(), i + 1));
                }
            }
            b';' if depth == 0 => spans.push((start.take().unwrap(), i + 1)),
            _ => {}
        }
        i += 1;
    }
    if let Some(s) = start {
        spans.push((s, src.len()));
    }
    spans
}

// pest numbers lines from the start of whatever input it is handed, so a chunk
// is padded with the newlines that preceded it. That costs nothing and keeps
// every line number and rendered snippet absolute, with no renumbering.
fn padded(src: &str, start: usize, end: usize) -> String {
    let mut text = "\n".repeat(src[..start].matches('\n').count());
    text.push_str(&src[start..end]);
    text
}

// When a class/contract fails to parse, locate the failure per member.
//
// Entry points live inside a contract, so a whole contract, usually the
fn member_errors(src: &str, span: (usize, usize)) -> Option<Vec<String>> {
    let (start, end) = span;
    let chunk = &src[start..end];
    let head = chunk.trim_start();
    if !["class ", "contract ", "interface ", "extern "].iter().any(|k| head.starts_with(k)) {
        return None;
    }
    let open = chunk.find('{')?;
    let close = chunk.rfind('}')?;
    if close <= open {
        return None;
    }
    let body_start = start + open + 1;
    let body_end = start + close;
    let mut errors = Vec::new();
    for (s, e) in top_level_spans(&src[body_start..body_end]) {
        let text = padded(src, body_start + s, body_start + e);
        if let Err(err) = GumParser::parse(Rule::member_unit, &text) {
            // A member that fails to parse is usually a function whose body has a
            // bad statement. Split it one level deeper, into statements, so two
            // broken statements in the same function both surface. If that finds
            // nothing (a bad signature, or a malformed field with no body), fall
            // back to the member-level error, which points at the real spot.
            match statement_errors(src, (body_start + s, body_start + e)) {
                Some(stmt_errors) => errors.extend(stmt_errors),
                None => errors.push(format!("Syntax Error: {}", err)),
            }
        }
    }
    if errors.is_empty() {
        None
    } else {
        Some(errors)
    }
}

// When a function member fails to parse, locate the failure per statement. The
// third recovery level: file -> declaration -> member -> statement. A bodyless
// member has no brace, so find('{')? returns None and the member error stands.
fn statement_errors(src: &str, span: (usize, usize)) -> Option<Vec<String>> {
    let (start, end) = span;
    let chunk = &src[start..end];
    let open = chunk.find('{')?;
    let close = chunk.rfind('}')?;
    if close <= open {
        return None;
    }
    let body_start = start + open + 1;
    let body_end = start + close;
    let mut errors = Vec::new();
    for (s, e) in top_level_spans(&src[body_start..body_end]) {
        let text = padded(src, body_start + s, body_start + e);
        if let Err(err) = GumParser::parse(Rule::statement_unit, &text) {
            errors.push(format!("Syntax Error: {}", err));
        }
    }
    if errors.is_empty() {
        None
    } else {
        Some(errors)
    }
}

// Parses one declaration, reporting errors at their true position in the file.
fn parse_one_declaration(src: &str, span: (usize, usize), program: &mut Program) -> Result<(), Vec<String>> {
    let text = padded(src, span.0, span.1);
    match GumParser::parse(Rule::decl_unit, &text) {
        Ok(mut pairs) => {
            let unit = pairs.next().unwrap();
            let decl = unit.into_inner().next().unwrap();
            build_declaration(decl.into_inner().next().unwrap(), program);
            Ok(())
        }
        Err(e) => Err(member_errors(src, span).unwrap_or_else(|| vec![format!("Syntax Error: {}", e)])),
    }
}

// Parses a whole program, reporting every malformed top-level declaration
// rather than only the first.
//
pub fn parse_program(source: &str) -> Result<Program, Vec<String>> {
    let preprocessed = crate::indent::preprocess(source).map_err(|e| vec![e])?;
    if std::env::var("GUMC_DEBUG").is_ok() {
        eprintln!("{}", preprocessed);
    }

    let mut program = Program { declarations: Vec::new() };
    let mut errors = Vec::new();
    for span in top_level_spans(&preprocessed) {
        if let Err(errs) = parse_one_declaration(&preprocessed, span, &mut program) {
            errors.extend(errs);
        }
    }

    if errors.is_empty() {
        Ok(program)
    } else {
        Err(errors)
    }
}

fn build_declaration(inner: pest::iterators::Pair<Rule>, program: &mut Program) {
    {
        {
                match inner.as_rule() {
                    Rule::use_decl => {
                        let path = inner.into_inner().next().unwrap().as_str().to_string();
                        program.declarations.push(Declaration::Use(UseDecl { path }));
                    }
                    Rule::class_decl => {
                        let mut name = String::new();
                        let mut generic_params = Vec::new();
                        let mut parents = Vec::new();
                        let mut is_global = false;
                        let mut is_extern = false;
                        let mut fields = Vec::new();
                        let mut methods = Vec::new();
                        for rule in inner.into_inner() {
                            match rule.as_rule() {
                                Rule::ident => name = rule.as_str().to_string(),
                                Rule::is_global => is_global = true,
                                Rule::is_extern => is_extern = true,
                                Rule::generic_params => {
                                    for param_pair in rule.into_inner() {
                                        let mut inner_rules = param_pair.into_inner();
                                        let bound = inner_rules.next().unwrap().as_str().to_string();
                                        let name = inner_rules.next().unwrap().as_str().to_string();
                                        generic_params.push(GenericParam { bound, name });
                                    }
                                }
                                Rule::parents => {
                                    for parent in rule.into_inner() {
                                        parents.push(parent.as_str().to_string());
                                    }
                                }
                                Rule::class_body => {
                                    for body_pair in rule.into_inner() {
                                        match body_pair.as_rule() {
                                            Rule::class_field => {
                                                let mut is_const = false;
                                                let mut is_transient = false;
                                                let mut type_def = Type::Primitive("unknown".to_string());
                                                let mut field_name = String::new();
                                                for f in body_pair.into_inner() {
                                                    match f.as_rule() {
                                                        Rule::is_const => is_const = true,
                                                        Rule::is_transient => is_transient = true,
                                                        Rule::type_keyword => type_def = parse_type(f),
                                                        Rule::ident => field_name = f.as_str().to_string(),
                                                        _ => {}
                                                    }
                                                }
                                                fields.push(ClassField { is_const, is_transient, type_def, name: field_name });
                                            }
                                            Rule::fn_decl => {
                                                methods.push(parse_fn_decl(body_pair));
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                _ => {},
                            }
                        }
                        program.declarations.push(Declaration::Class(ClassDecl { is_global, is_extern, name, generic_params, parents, fields, methods }));
                    }
                    Rule::enum_decl => {
                        let mut variants = Vec::new();
                        let mut inner_rules = inner.into_inner();
                        let name = inner_rules.next().unwrap().as_str().to_string();
                        for variant_pair in inner_rules {
                            let mut v_rules = variant_pair.into_inner();
                            let v_name = v_rules.next().unwrap().as_str().to_string();
                            let mut parameters = Vec::new();
                            if let Some(params_pair) = v_rules.next() {
                                for param_pair in params_pair.into_inner() {
                                    let mut param_rules = param_pair.into_inner();
                                    let mut is_mut = false;
                                    let mut type_pair = param_rules.next().unwrap();
                                    if type_pair.as_rule() == Rule::is_mut {
                                        is_mut = true;
                                        type_pair = param_rules.next().unwrap();
                                    }
                                    let type_def = parse_type(type_pair);
                                    let param_name = param_rules.next().unwrap().as_str().to_string();
                                    parameters.push(Parameter { is_mut, type_def, name: param_name });
                                }
                            }
                            variants.push(EnumVariant { name: v_name, parameters });
                        }
                        program.declarations.push(Declaration::Enum(EnumDecl { name, variants }));
                    }
                    Rule::fn_decl => {
                        program.declarations.push(Declaration::Function(parse_fn_decl(inner)));
                    }
            _ => {}
        }
    }
    }
}

fn parse_fn_decl(rule: pest::iterators::Pair<Rule>) -> FnDecl {
    let mut name = String::new();
    let mut modifiers = Vec::new();
    let mut attributes = Vec::new();
    let mut parameters = Vec::new();
    let mut return_type = None;
    let mut body = Vec::new();

    for inner_rule in rule.into_inner() {
        match inner_rule.as_rule() {
            Rule::fn_attrs => {
                for a in inner_rule.into_inner() {
                    attributes.push(a.as_str().to_string());
                }
            }
            Rule::modifier => modifiers.push(inner_rule.as_str().to_string()),
            Rule::ident => name = inner_rule.as_str().to_string(),
            Rule::param_list => {
                for param_pair in inner_rule.into_inner() {
                    let mut is_mut = false;
                    let mut type_def = Type::Primitive("unknown".to_string());
                    let mut param_name = String::new();
                    for p in param_pair.into_inner() {
                        match p.as_rule() {
                            Rule::type_keyword => type_def = parse_type(p),
                            Rule::ident => param_name = p.as_str().to_string(),
                            Rule::is_mut => is_mut = true,
                            _ => {}
                        }
                    }
                    parameters.push(Parameter { is_mut, type_def, name: param_name });
                }
            }
            Rule::type_keyword => return_type = Some(parse_type(inner_rule)),
            Rule::fn_body => {
                for stmt_pair in inner_rule.into_inner() {
                    body.push(parse_statement(stmt_pair));
                }
            }
            _ => {}
        }
    }
    FnDecl { modifiers, attributes, name, parameters, return_type, body }
}

fn parse_type(pair: pest::iterators::Pair<Rule>) -> Type {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::primitive_type => Type::Primitive(inner.as_str().to_string()),
        Rule::type_ident => Type::Primitive(inner.as_str().to_string()),
        Rule::generic_type => {
            let mut inner_rules = inner.into_inner();
            let name = inner_rules.next().unwrap().as_str().to_string();
            let mut args = Vec::new();
            for type_rule in inner_rules {
                args.push(parse_type(type_rule));
            }
            Type::Generic { name, args }
        }
        Rule::array_type => {
            let mut inner_rules = inner.into_inner();
            let inner_type = parse_type(inner_rules.next().unwrap());
            if let Some(num_pair) = inner_rules.next() {
                let size = num_pair.as_str().parse().unwrap();
                Type::FixedArray(Box::new(inner_type), size)
            } else {
                Type::Array(Box::new(inner_type))
            }
        }
        _ => Type::Primitive("unknown".to_string())
    }
}

fn parse_statement(pair: pest::iterators::Pair<Rule>) -> Spanned<Statement> {
    let (line, col) = pair.as_span().start_pos().line_col();
    let inner = pair.into_inner().next().unwrap();
    
    let node = match inner.as_rule() {
        Rule::unsafe_block => {
            Statement::UnsafeBlock(inner.as_str().to_string())
        }
        Rule::assert_stmt => {
            let mut rules = inner.into_inner();
            let condition = parse_expr(rules.next().unwrap());
            let message = rules.next().map(parse_expr);
            Statement::Assert { condition, message }
        }
        Rule::return_stmt => {
            Statement::Return { value: inner.into_inner().next().map(parse_expr) }
        }
        Rule::revert_stmt => {
            let mut inner_rules = inner.into_inner();
            let error = parse_expr(inner_rules.next().unwrap());
            Statement::Revert { error }
        }
        Rule::delete_stmt => {
            Statement::Delete { target: parse_term(inner.into_inner().next().unwrap()) }
        }
        Rule::bitwise_flip => {
            let mut rules = inner.into_inner();
            let name = rules.next().unwrap().as_str().to_string();
            let index = parse_expr(rules.next().unwrap());
            let value = parse_expr(rules.next().unwrap());
            Statement::BitwiseFlip { name, index, value }
        }
        Rule::var_decl => {
            let mut is_mut = false;
            let mut is_const = false;
            let mut type_def = Type::Primitive("unknown".to_string());
            let mut saw_type = false;
            let mut is_var = false;
            let mut name = String::new();
            let mut value = None;
            for p in inner.into_inner() {
                match p.as_rule() {
                    Rule::var_kw => is_var = true,
                    Rule::is_mut => is_mut = true,
                    Rule::is_const => is_const = true,
                    Rule::type_keyword => { type_def = parse_type(p); saw_type = true; }
                    Rule::ident => name = p.as_str().to_string(),
                    Rule::expr => value = Some(parse_expr(p)),
                    _ => {}
                }
            }
            if !saw_type {
                type_def = Type::Primitive("_infer".to_string());
            }
            if (is_var || is_const) && !is_mut {
                is_const = true;
            }
            Statement::VarDecl { is_mut, is_const, type_def, name, value }
        }
        Rule::call_stmt => {
            let mut inner_rules = inner.into_inner();
            let target = inner_rules.next().unwrap().as_str().to_string();
            let args = inner_rules.map(parse_expr).collect();
            Statement::Call { target, args }
        }
        Rule::assignment => {
            let mut inner_rules = inner.into_inner();
            let target = parse_term(inner_rules.next().unwrap());
            let value = parse_expr(inner_rules.next().unwrap());
            Statement::Assignment { target, value }
        }
        Rule::compound_assignment => {
            let mut inner_rules = inner.into_inner();
            let target = parse_term(inner_rules.next().unwrap());
            let op_str = inner_rules.next().unwrap().as_str();
            let operator = op_str[..op_str.len() - 1].to_string();
            let rhs = parse_expr(inner_rules.next().unwrap());
            let value = Expr::BinaryOp { left: Box::new(target.clone()), operator, right: Box::new(rhs) };
            Statement::Assignment { target, value }
        }
        Rule::expr_stmt => {
            Statement::Expression(parse_expr(inner.into_inner().next().unwrap()))
        }
        Rule::if_stmt => {
            let mut inner_rules = inner.into_inner();
            let condition = parse_expr(inner_rules.next().unwrap());
            
            let mut if_body = Vec::new();
            let if_body_pair = inner_rules.next().unwrap();
            for stmt in if_body_pair.into_inner() {
                if_body.push(parse_statement(stmt));
            }

            let mut else_body = None;
            if let Some(else_pair) = inner_rules.next() {
                let mut eb = Vec::new();
                for stmt in else_pair.into_inner() {
                    eb.push(parse_statement(stmt));
                }
                else_body = Some(eb);
            }

            Statement::IfElse { condition, if_body, else_body }
        }
        Rule::for_stmt => {
            let mut inner_rules = inner.into_inner();
            let iterator = inner_rules.next().unwrap().as_str().to_string();
            let iterable = parse_expr(inner_rules.next().unwrap());
            let mut body = Vec::new();
            for stmt in inner_rules.next().unwrap().into_inner() {
                body.push(parse_statement(stmt));
            }
            Statement::ForLoop { iterator, iterable, body }
        }
        Rule::while_stmt => {
            let mut inner_rules = inner.into_inner();
            let condition = parse_expr(inner_rules.next().unwrap());
            let mut body = Vec::new();
            for stmt in inner_rules.next().unwrap().into_inner() {
                body.push(parse_statement(stmt));
            }
            Statement::WhileLoop { condition, body }
        }
        Rule::try_stmt => {
            let mut inner_rules = inner.into_inner();
            let mut try_body = Vec::new();
            for stmt in inner_rules.next().unwrap().into_inner() {
                try_body.push(parse_statement(stmt));
            }
            let mut catch_body = Vec::new();
            for stmt in inner_rules.next().unwrap().into_inner() {
                catch_body.push(parse_statement(stmt));
            }
            Statement::TryCatch { try_body, catch_body }
        }
        Rule::match_stmt => {
            let mut inner_rules = inner.into_inner();
            let expr = parse_expr(inner_rules.next().unwrap());
            let mut arms = Vec::new();
            for arm_pair in inner_rules {
                let mut arm_rules = arm_pair.into_inner();
                let variant = arm_rules.next().unwrap().as_str().to_string();
                
                let mut payload_var = None;
                let mut body_pair = arm_rules.next().unwrap();
                if body_pair.as_rule() == Rule::ident {
                    payload_var = Some(body_pair.as_str().to_string());
                    body_pair = arm_rules.next().unwrap();
                }
                
                let mut body = Vec::new();
                for stmt in body_pair.into_inner() {
                    body.push(parse_statement(stmt));
                }
                arms.push(MatchArm { variant, payload_var, body });
            }
            Statement::Match { expr, arms }
        }
        _ => panic!("Unknown statement rule"),
    };

    Spanned { node, line, col }
}

// Splits an f-string's inner text (between the quotes) into literal and
// {expr} segments, re-invoking the expr grammar rule on each
// interpolation span. gum's expr grammar never produces literal {/}
fn parse_fstring_segments(s: &str) -> Vec<FStringSegment> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '{' {
            literal.push(c);
            continue;
        }
        if !literal.is_empty() {
            segments.push(FStringSegment::Literal(std::mem::take(&mut literal)));
        }
        let mut expr_text = String::new();
        let mut depth = 1;
        while let Some(nc) = chars.next() {
            match nc {
                '{' => { depth += 1; expr_text.push(nc); }
                '}' => {
                    depth -= 1;
                    if depth == 0 { break; }
                    expr_text.push(nc);
                }
                _ => expr_text.push(nc),
            }
        }
        if let Ok(mut parsed) = GumParser::parse(Rule::expr, expr_text.trim()) {
            segments.push(FStringSegment::Interp(parse_expr(parsed.next().unwrap())));
        }
    }
    if !literal.is_empty() {
        segments.push(FStringSegment::Literal(literal));
    }
    segments
}

fn binop_prec(op: &str) -> u8 {
    match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => 0,
    }
}

fn is_right_assoc(op: &str) -> bool {
    op == "**"
}

fn parse_expr(pair: pest::iterators::Pair<Rule>) -> Expr {
    let mut expr_rules = pair.into_inner();
    let mut terms = vec![parse_term(expr_rules.next().unwrap())];
    let mut ops: Vec<String> = Vec::new();
    while let Some(op_pair) = expr_rules.next() {
        ops.push(op_pair.as_str().to_string());
        terms.push(parse_term(expr_rules.next().unwrap()));
    }

    let mut cursor = 0usize;
    climb_prec(&terms, &ops, &mut cursor, 0)
}

fn climb_prec(terms: &[Expr], ops: &[String], cursor: &mut usize, min_prec: u8) -> Expr {
    let mut left = terms[*cursor].clone();
    while *cursor < ops.len() {
        let op = ops[*cursor].clone();
        let prec = binop_prec(&op);
        if prec < min_prec {
            break;
        }
        // consume operator; right operand starts at the new cursor
        *cursor += 1;
        let next_min = if is_right_assoc(&op) { prec } else { prec + 1 };
        let right = climb_prec(terms, ops, cursor, next_min);
        left = Expr::BinaryOp {
            left: Box::new(left),
            operator: op,
            right: Box::new(right),
        };
    }
    left
}

fn parse_term(pair: pest::iterators::Pair<Rule>) -> Expr {
    let mut inner_rules = pair.into_inner().peekable();
    let unary = inner_rules.peek()
        .filter(|p| p.as_rule() == Rule::unary_op)
        .map(|p| p.as_str().to_string());
    if unary.is_some() {
        inner_rules.next();
    }
    let atom = inner_rules.next().unwrap().into_inner().next().unwrap();

    let mut base_expr = match atom.as_rule() {
        Rule::number => Expr::Number(atom.as_str().to_string()),
        Rule::string_literal => {
            let s = atom.as_str();
            Expr::StringLiteral(s[1..s.len()-1].to_string())
        }
        Rule::ident => Expr::Identifier(atom.as_str().to_string()),
        Rule::paren_expr => {
            parse_expr(atom.into_inner().next().unwrap())
        }
        Rule::array_literal => {
            Expr::ArrayLiteral(atom.into_inner().map(parse_expr).collect())
        }
        Rule::fstring_literal => {
            let raw = atom.as_str();
            // strip leading f" and trailing "
            let inner = &raw[2..raw.len() - 1];
            Expr::FString(parse_fstring_segments(inner))
        }
        Rule::instantiation => {
            let mut i_rules = atom.into_inner();
            let type_def = parse_type(i_rules.next().unwrap());
            let mut args = Vec::new();
            for p in i_rules {
                args.push(parse_expr(p));
            }
            Expr::Instantiation { type_def, args }
        }
        Rule::fn_call => {
            let mut args = Vec::new();
            let mut name = String::new();
            for (i, p) in atom.into_inner().enumerate() {
                if i == 0 { name = p.as_str().to_string(); }
                else { args.push(parse_expr(p)); }
            }
            Expr::FnCall { name, args }
        }
        _ => Expr::Identifier("unknown_term".to_string())
    };

    while let Some(suffix_pair) = inner_rules.next() {
        let next_pair = if suffix_pair.as_rule() == Rule::suffix {
            suffix_pair.into_inner().next().unwrap()
        } else {
            suffix_pair
        };

        match next_pair.as_rule() {
            Rule::property_access => {
                let prop = next_pair.into_inner().next().unwrap().as_str().to_string();
                base_expr = Expr::PropertyAccess { base: Box::new(base_expr), property: prop };
            }
            Rule::index_access => {
                let idx = parse_expr(next_pair.into_inner().next().unwrap());
                base_expr = Expr::IndexAccess { base: Box::new(base_expr), index: Box::new(idx) };
            }
            Rule::method_access => {
                let fn_call_pair = next_pair.into_inner().next().unwrap();
                let mut args = Vec::new();
                let mut method = String::new();
                for (i, p) in fn_call_pair.into_inner().enumerate() {
                    if i == 0 { method = p.as_str().to_string(); }
                    else { args.push(parse_expr(p)); }
                }
                base_expr = Expr::MethodCall { base: Box::new(base_expr), method, args };
            }
            _ => {}
        }
    }
    match unary.as_deref() {
        Some("-") => Expr::Neg(Box::new(base_expr)),
        Some("!") => Expr::Not(Box::new(base_expr)),
        _ => base_expr
    }
}
