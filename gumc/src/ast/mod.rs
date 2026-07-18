#[derive(Debug, Clone)]
pub struct Program {
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub enum Declaration {
    Use(UseDecl),
    Class(ClassDecl),
    Enum(EnumDecl),
    Function(FnDecl),
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub parameters: Vec<Parameter>,
}

#[derive(Debug, Clone)]
pub struct UseDecl {
    pub path: String,
}

#[derive(Debug, Clone)]
pub enum Type {
    Primitive(String),
    Generic { name: String, args: Vec<Type> },
    Array(Box<Type>),
    FixedArray(Box<Type>, usize),
}

#[derive(Debug, Clone)]
pub struct GenericParam {
    pub bound: String,
    pub name: String,
}

// is_const: fixed at deploy, no storage slot. Assigned once in fn new.
// is_transient: transient storage (EIP-1153), cleared each transaction.
#[derive(Debug, Clone)]
pub struct ClassField {
    pub is_const: bool,
    pub is_transient: bool,
    pub type_def: Type,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub is_global: bool,
    pub is_extern: bool,
    pub name: String,
    pub generic_params: Vec<GenericParam>,
    pub parents: Vec<String>,
    pub fields: Vec<ClassField>,
    pub methods: Vec<FnDecl>,
}

#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub node: T,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub modifiers: Vec<String>,
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<Type>,
    pub body: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone)]
pub struct Parameter {
    pub is_mut: bool,
    pub type_def: Type,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum Statement {
    VarDecl { is_mut: bool, is_const: bool, type_def: Type, name: String, value: Option<Expr> },
    Assignment { target: Expr, value: Expr },
    BitwiseFlip { name: String, index: Expr, value: Expr }, // Syntax sugar
    // assert(cond) or assert(cond, msg), where msg is a string (→ the
    // standard Error(string) revert) or a custom-error call (→ that error).
    Assert { condition: Expr, message: Option<Expr> },
    Revert { error: Expr },
    // delete x, reset an lvalue to its type's zero value. For most types
    // that is a plain store of 0; a dynamic array clears its elements and
    // length, a storage string releases its data slots, a struct zeroes every
    // field.
    Delete { target: Expr },
    // return expr in a value-returning function; bare return (value: None)
    // early-exits a function that declares no return type.
    Return { value: Option<Expr> },
    IfElse { condition: Expr, if_body: Vec<Spanned<Statement>>, else_body: Option<Vec<Spanned<Statement>>> },
    ForLoop { iterator: String, iterable: Expr, body: Vec<Spanned<Statement>> },
    WhileLoop { condition: Expr, body: Vec<Spanned<Statement>> },
    TryCatch { try_body: Vec<Spanned<Statement>>, catch_body: Vec<Spanned<Statement>> },
    Match { expr: Expr, arms: Vec<MatchArm> },
    Expression(Expr),
    Call { target: String, args: Vec<Expr> }, // Low-level external contract call, e.g. call target(args)
    UnsafeBlock(String),
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub variant: String,
    pub payload_var: Option<String>,
    pub body: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Number(String),
    StringLiteral(String),
    Identifier(String),
    FnCall { name: String, args: Vec<Expr> },
    Instantiation { type_def: Type, args: Vec<Expr> },
    PropertyAccess { base: Box<Expr>, property: String },
    MethodCall { base: Box<Expr>, method: String, args: Vec<Expr> },
    IndexAccess { base: Box<Expr>, index: Box<Expr> },
    BinaryOp { left: Box<Expr>, operator: String, right: Box<Expr> },
    FString(Vec<FStringSegment>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    ArrayLiteral(Vec<Expr>),
}

#[derive(Debug, Clone)]
pub enum FStringSegment {
    Literal(String),
    Interp(Expr),
}
