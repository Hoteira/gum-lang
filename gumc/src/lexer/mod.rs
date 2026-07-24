use logos::Logos;

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(skip r"[ \t\n\f]+")]
#[logos(skip(r"//.*", allow_greedy = true))]
pub enum Token {
    #[token("use")]
    Use,
    #[token("global")]
    Global,
    #[token("once")]
    Once,
    #[token("fn")]
    Fn,
    #[token("class")]
    Class,
    #[token("enum")]
    Enum,
    #[token("interface")]
    Interface,
    #[token("const")]
    Const,
    #[token("mut")]
    Mut,
    #[token("assert")]
    Assert,
    #[token("match")]
    Match,
    #[token("for")]
    For,
    #[token("in")]
    In,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("return")]
    Return,

    #[token("u8")]
    #[token("u16")]
    #[token("u32")]
    #[token("u64")]
    #[token("u128")]
    #[token("u256")]
    #[token("i8")]
    #[token("i16")]
    #[token("i32")]
    #[token("i64")]
    #[token("i128")]
    #[token("i256")]
    #[token("f32")]
    #[token("f64")]
    #[token("bool")]
    #[token("Account")]
    TypeKeyword,

    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(":")]
    Colon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,

    #[token("=>")]
    FatArrow,
    #[token("->")]
    Arrow,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token(">=")]
    GtEq,
    #[token("<=")]
    LtEq,
    #[token(">")]
    Gt,
    #[token("<")]
    Lt,
    #[token("=")]
    Assign,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("||")]
    Or,
    #[token("&&")]
    And,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Identifier(String),

    #[regex(r"[0-9]+", |lex| lex.slice().parse::<u64>().unwrap_or(0))]
    Integer(u64),

    #[regex(r#""([^"\\]|\\t|\\u|\\n|\\")*""#, |lex| lex.slice().to_string())]
    StringLiteral(String),
}
