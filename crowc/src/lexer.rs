//! Lexer for Crow source files.

use crate::ast::BinOp;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Literals and identifiers
    /// Integer literals are unsigned digits; a leading `-` is a separate
    /// token. The checker decides the type (and range-checks) in context.
    Int(u64),
    /// `b'X'` byte literal; always typed `u8`.
    Byte(u8),
    Float(f64),
    Str(String),
    Ident(String),
    // Keywords
    Fn,
    Struct,
    Let,
    If,
    Else,
    While,
    For,
    Return,
    Break,
    Continue,
    True,
    False,
    Nil,
    As,
    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    Dot,
    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Not,
    AndAnd,
    OrOr,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    /// Compound assignment (`+=`, `<<=`, ...), carrying the operator.
    OpAssign(BinOp),
    Eof,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tok::Int(v) => return write!(f, "integer literal {v}"),
            Tok::Byte(v) => return write!(f, "byte literal {v}"),
            Tok::Float(v) => return write!(f, "float literal {v}"),
            Tok::Str(_) => "string literal",
            Tok::Ident(name) => return write!(f, "identifier '{name}'"),
            Tok::Fn => "'fn'",
            Tok::Struct => "'struct'",
            Tok::Let => "'let'",
            Tok::If => "'if'",
            Tok::Else => "'else'",
            Tok::While => "'while'",
            Tok::For => "'for'",
            Tok::Return => "'return'",
            Tok::Break => "'break'",
            Tok::Continue => "'continue'",
            Tok::True => "'true'",
            Tok::False => "'false'",
            Tok::Nil => "'nil'",
            Tok::As => "'as'",
            Tok::LParen => "'('",
            Tok::RParen => "')'",
            Tok::LBrace => "'{'",
            Tok::RBrace => "'}'",
            Tok::LBracket => "'['",
            Tok::RBracket => "']'",
            Tok::Comma => "','",
            Tok::Semi => "';'",
            Tok::Colon => "':'",
            Tok::Dot => "'.'",
            Tok::Plus => "'+'",
            Tok::Minus => "'-'",
            Tok::Star => "'*'",
            Tok::Slash => "'/'",
            Tok::Percent => "'%'",
            Tok::Assign => "'='",
            Tok::Eq => "'=='",
            Tok::Ne => "'!='",
            Tok::Lt => "'<'",
            Tok::Le => "'<='",
            Tok::Gt => "'>'",
            Tok::Ge => "'>='",
            Tok::Not => "'!'",
            Tok::AndAnd => "'&&'",
            Tok::OrOr => "'||'",
            Tok::Amp => "'&'",
            Tok::Pipe => "'|'",
            Tok::Caret => "'^'",
            Tok::Tilde => "'~'",
            Tok::Shl => "'<<'",
            Tok::Shr => "'>>'",
            Tok::OpAssign(op) => return write!(f, "'{}='", op.sym()),
            Tok::Eof => "end of file",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
}

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let bytes = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;

    macro_rules! err {
        ($($arg:tt)*) => {
            return Err(format!("{}:{}: {}", line, i - line_start + 1, format!($($arg)*)))
        };
    }

    while i < bytes.len() {
        let c = bytes[i];
        let col = (i - line_start + 1) as u32;
        let mut push = |tok: Tok, len: usize| {
            toks.push(Token { tok, line, col });
            len
        };
        match c {
            b'\n' => {
                i += 1;
                line += 1;
                line_start = i;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth > 0 {
                    err!("unterminated block comment");
                }
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let is_float = i + 1 < bytes.len()
                    && bytes[i] == b'.'
                    && bytes[i + 1].is_ascii_digit();
                if is_float {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text = &src[start..i];
                if is_float {
                    match text.parse::<f64>() {
                        Ok(v) => toks.push(Token { tok: Tok::Float(v), line, col }),
                        Err(_) => err!("invalid float literal '{text}'"),
                    }
                } else {
                    match text.parse::<u64>() {
                        Ok(v) => toks.push(Token { tok: Tok::Int(v), line, col }),
                        Err(_) => err!("integer literal '{text}' out of range"),
                    }
                }
            }
            // `b'X'` byte literal: the ASCII value of one character, as u8.
            b'b' if bytes.get(i + 1) == Some(&b'\'') => {
                i += 2;
                let v: u8 = match bytes.get(i) {
                    None => err!("unterminated byte literal"),
                    Some(b'\'') => err!("empty byte literal"),
                    Some(b'\\') => {
                        i += 1;
                        let c = match bytes.get(i) {
                            Some(b'n') => b'\n',
                            Some(b't') => b'\t',
                            Some(b'r') => b'\r',
                            Some(b'\\') => b'\\',
                            Some(b'\'') => b'\'',
                            Some(b'0') => 0,
                            Some(b'x') => {
                                let hex = match (bytes.get(i + 1), bytes.get(i + 2)) {
                                    (Some(&h), Some(&l))
                                        if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() =>
                                    {
                                        u8::from_str_radix(&src[i + 1..i + 3], 16).unwrap()
                                    }
                                    _ => err!("\\x escape needs exactly 2 hex digits"),
                                };
                                i += 2;
                                hex
                            }
                            _ => err!("invalid escape sequence in byte literal"),
                        };
                        i += 1;
                        c
                    }
                    Some(&c) if (0x20..=0x7e).contains(&c) => {
                        i += 1;
                        c
                    }
                    Some(_) => err!(
                        "byte literal must be a printable ASCII character; \
                         use \\xNN for other byte values"
                    ),
                };
                if bytes.get(i) != Some(&b'\'') {
                    err!("byte literal must contain exactly one character");
                }
                i += 1;
                toks.push(Token { tok: Tok::Byte(v), line, col });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let word = &src[start..i];
                let tok = match word {
                    "fn" => Tok::Fn,
                    "struct" => Tok::Struct,
                    "let" => Tok::Let,
                    "if" => Tok::If,
                    "else" => Tok::Else,
                    "while" => Tok::While,
                    "for" => Tok::For,
                    "return" => Tok::Return,
                    "break" => Tok::Break,
                    "continue" => Tok::Continue,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "nil" => Tok::Nil,
                    "as" => Tok::As,
                    _ => Tok::Ident(word.to_string()),
                };
                toks.push(Token { tok, line, col });
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        err!("unterminated string literal");
                    }
                    match bytes[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\n' => {
                            // Strings may span lines; the newline is kept.
                            s.push('\n');
                            i += 1;
                            line += 1;
                            line_start = i;
                        }
                        b'\\' => {
                            i += 1;
                            match bytes.get(i) {
                                Some(b'n') => s.push('\n'),
                                Some(b't') => s.push('\t'),
                                Some(b'r') => s.push('\r'),
                                Some(b'\\') => s.push('\\'),
                                Some(b'"') => s.push('"'),
                                Some(b'0') => s.push('\0'),
                                Some(b'u') => {
                                    // \u{1..6 hex digits}, any Unicode scalar.
                                    i += 1;
                                    if bytes.get(i) != Some(&b'{') {
                                        err!("expected '{{' after \\u");
                                    }
                                    i += 1;
                                    let start = i;
                                    while i < bytes.len()
                                        && bytes[i].is_ascii_hexdigit()
                                    {
                                        i += 1;
                                    }
                                    let hex = &src[start..i];
                                    if bytes.get(i) != Some(&b'}') {
                                        err!("expected '}}' after \\u{{{hex}");
                                    }
                                    if hex.is_empty() || hex.len() > 6 {
                                        err!("\\u escape needs 1 to 6 hex digits");
                                    }
                                    let v = u32::from_str_radix(hex, 16).unwrap();
                                    match char::from_u32(v) {
                                        Some(c) => s.push(c),
                                        None => err!(
                                            "\\u{{{hex}}} is not a valid Unicode scalar"
                                        ),
                                    }
                                }
                                _ => err!("invalid escape sequence"),
                            }
                            i += 1;
                        }
                        _ => {
                            // Copy the full UTF-8 character.
                            let ch_len = utf8_len(bytes[i]);
                            s.push_str(&src[i..i + ch_len]);
                            i += ch_len;
                        }
                    }
                }
                toks.push(Token { tok: Tok::Str(s), line, col });
            }
            b'(' => i += push(Tok::LParen, 1),
            b')' => i += push(Tok::RParen, 1),
            b'{' => i += push(Tok::LBrace, 1),
            b'}' => i += push(Tok::RBrace, 1),
            b'[' => i += push(Tok::LBracket, 1),
            b']' => i += push(Tok::RBracket, 1),
            b',' => i += push(Tok::Comma, 1),
            b';' => i += push(Tok::Semi, 1),
            b':' => i += push(Tok::Colon, 1),
            b'.' => i += push(Tok::Dot, 1),
            b'+' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::Add), 2),
            b'+' => i += push(Tok::Plus, 1),
            b'-' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::Sub), 2),
            b'-' => i += push(Tok::Minus, 1),
            b'*' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::Mul), 2),
            b'*' => i += push(Tok::Star, 1),
            b'/' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::Div), 2),
            b'/' => i += push(Tok::Slash, 1),
            b'%' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::Rem), 2),
            b'%' => i += push(Tok::Percent, 1),
            b'=' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::Eq, 2),
            b'=' => i += push(Tok::Assign, 1),
            b'!' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::Ne, 2),
            b'!' => i += push(Tok::Not, 1),
            b'<' if bytes.get(i + 1) == Some(&b'<') && bytes.get(i + 2) == Some(&b'=') => {
                i += push(Tok::OpAssign(BinOp::Shl), 3)
            }
            b'<' if bytes.get(i + 1) == Some(&b'<') => i += push(Tok::Shl, 2),
            b'<' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::Le, 2),
            b'<' => i += push(Tok::Lt, 1),
            b'>' if bytes.get(i + 1) == Some(&b'>') && bytes.get(i + 2) == Some(&b'=') => {
                i += push(Tok::OpAssign(BinOp::Shr), 3)
            }
            // `>>` lexes as a shift; the parser splits it back into two `>`
            // when it closes nested type arguments (`Pair<Pair<int>>`).
            b'>' if bytes.get(i + 1) == Some(&b'>') => i += push(Tok::Shr, 2),
            b'>' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::Ge, 2),
            b'>' => i += push(Tok::Gt, 1),
            b'&' if bytes.get(i + 1) == Some(&b'&') => i += push(Tok::AndAnd, 2),
            b'&' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::BitAnd), 2),
            b'&' => i += push(Tok::Amp, 1),
            b'|' if bytes.get(i + 1) == Some(&b'|') => i += push(Tok::OrOr, 2),
            b'|' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::BitOr), 2),
            b'|' => i += push(Tok::Pipe, 1),
            b'^' if bytes.get(i + 1) == Some(&b'=') => i += push(Tok::OpAssign(BinOp::BitXor), 2),
            b'^' => i += push(Tok::Caret, 1),
            b'~' => i += push(Tok::Tilde, 1),
            _ => err!("unexpected character '{}'", src[i..].chars().next().unwrap()),
        }
    }
    toks.push(Token { tok: Tok::Eof, line, col: (i - line_start + 1) as u32 });
    Ok(toks)
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn keywords_and_identifiers() {
        assert_eq!(
            toks("fn struct let if else while for return break continue true false nil as foo _bar x9"),
            vec![
                Tok::Fn, Tok::Struct, Tok::Let, Tok::If, Tok::Else, Tok::While, Tok::For,
                Tok::Return, Tok::Break, Tok::Continue, Tok::True, Tok::False, Tok::Nil,
                Tok::As, Tok::Ident("foo".into()), Tok::Ident("_bar".into()),
                Tok::Ident("x9".into()), Tok::Eof,
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(toks("0 42 1.5 0.25"), vec![
            Tok::Int(0), Tok::Int(42), Tok::Float(1.5), Tok::Float(0.25), Tok::Eof,
        ]);
        // A dot not followed by a digit is not part of the number.
        assert_eq!(toks("1.x"), vec![Tok::Int(1), Tok::Dot, Tok::Ident("x".into()), Tok::Eof]);
        assert_eq!(toks("18446744073709551615"), vec![Tok::Int(u64::MAX), Tok::Eof]);
    }

    #[test]
    fn operators() {
        assert_eq!(toks("+ - * / % = == != < <= > >= ! && ||"), vec![
            Tok::Plus, Tok::Minus, Tok::Star, Tok::Slash, Tok::Percent, Tok::Assign,
            Tok::Eq, Tok::Ne, Tok::Lt, Tok::Le, Tok::Gt, Tok::Ge, Tok::Not,
            Tok::AndAnd, Tok::OrOr, Tok::Eof,
        ]);
        assert_eq!(toks("& | ^ ~ << >>"), vec![
            Tok::Amp, Tok::Pipe, Tok::Caret, Tok::Tilde, Tok::Shl, Tok::Shr, Tok::Eof,
        ]);
        assert_eq!(toks("+= -= *= /= %= &= |= ^= <<= >>="), vec![
            Tok::OpAssign(BinOp::Add), Tok::OpAssign(BinOp::Sub), Tok::OpAssign(BinOp::Mul),
            Tok::OpAssign(BinOp::Div), Tok::OpAssign(BinOp::Rem), Tok::OpAssign(BinOp::BitAnd),
            Tok::OpAssign(BinOp::BitOr), Tok::OpAssign(BinOp::BitXor),
            Tok::OpAssign(BinOp::Shl), Tok::OpAssign(BinOp::Shr), Tok::Eof,
        ]);
        // Maximal munch: `&&` beats `&`, `<<` beats `<`, `>>=` beats `>>`.
        assert_eq!(toks("a&&b"), vec![
            Tok::Ident("a".into()), Tok::AndAnd, Tok::Ident("b".into()), Tok::Eof,
        ]);
        assert_eq!(toks("a<<b"), vec![
            Tok::Ident("a".into()), Tok::Shl, Tok::Ident("b".into()), Tok::Eof,
        ]);
    }

    #[test]
    fn byte_literals() {
        assert_eq!(toks("b'a' b'Z' b'0' b' '"), vec![
            Tok::Byte(b'a'), Tok::Byte(b'Z'), Tok::Byte(b'0'), Tok::Byte(b' '), Tok::Eof,
        ]);
        assert_eq!(toks(r"b'\n' b'\t' b'\r' b'\\' b'\'' b'\0' b'\x00' b'\x7F' b'\xff'"), vec![
            Tok::Byte(b'\n'), Tok::Byte(b'\t'), Tok::Byte(b'\r'), Tok::Byte(b'\\'),
            Tok::Byte(b'\''), Tok::Byte(0), Tok::Byte(0x00), Tok::Byte(0x7f), Tok::Byte(0xff),
            Tok::Eof,
        ]);
        // Identifiers that merely start with 'b' are untouched.
        assert_eq!(toks("bar b"), vec![
            Tok::Ident("bar".into()), Tok::Ident("b".into()), Tok::Eof,
        ]);
        assert!(lex("b''").unwrap_err().contains("empty byte literal"));
        assert!(lex("b'ab'").unwrap_err().contains("exactly one character"));
        assert!(lex("b'é'").unwrap_err().contains("printable ASCII"));
        assert!(lex(r"b'\q'").unwrap_err().contains("invalid escape"));
        assert!(lex(r"b'\x1'").unwrap_err().contains("2 hex digits"));
        assert!(lex("b'a").unwrap_err().contains("exactly one character"));
        assert!(lex("b'").unwrap_err().contains("unterminated byte literal"));
    }

    #[test]
    fn punctuation() {
        assert_eq!(toks("(){}[],;:."), vec![
            Tok::LParen, Tok::RParen, Tok::LBrace, Tok::RBrace, Tok::LBracket,
            Tok::RBracket, Tok::Comma, Tok::Semi, Tok::Colon, Tok::Dot, Tok::Eof,
        ]);
    }

    #[test]
    fn string_escapes() {
        assert_eq!(toks(r#""a\nb\tc\\d\"e\r\0""#), vec![
            Tok::Str("a\nb\tc\\d\"e\r\0".into()), Tok::Eof,
        ]);
        assert_eq!(toks("\"héllo\""), vec![Tok::Str("héllo".into()), Tok::Eof]);
        assert_eq!(toks("\"\""), vec![Tok::Str("".into()), Tok::Eof]);
    }

    #[test]
    fn multiline_strings() {
        assert_eq!(toks("\"a\nb\""), vec![Tok::Str("a\nb".into()), Tok::Eof]);
        // Line counting stays correct after an embedded newline.
        let ts = lex("\"a\nb\" x").unwrap();
        assert_eq!((ts[1].line, ts[1].col), (2, 4)); // x
    }

    #[test]
    fn unicode_escapes() {
        assert_eq!(toks(r#""\u{48}\u{e9}\u{1F600}""#), vec![
            Tok::Str("Hé😀".into()), Tok::Eof,
        ]);
        assert!(lex(r#""\u48""#).unwrap_err().contains("expected '{' after \\u"));
        assert!(lex(r#""\u{}""#).unwrap_err().contains("1 to 6 hex digits"));
        assert!(lex(r#""\u{1234567}""#).unwrap_err().contains("1 to 6 hex digits"));
        assert!(lex(r#""\u{12x}""#).unwrap_err().contains("expected '}'"));
        assert!(lex(r#""\u{D800}""#).unwrap_err().contains("not a valid Unicode scalar"));
    }

    #[test]
    fn comments() {
        assert_eq!(toks("1 // two\n3"), vec![Tok::Int(1), Tok::Int(3), Tok::Eof]);
        assert_eq!(toks("1 /* x */ 2"), vec![Tok::Int(1), Tok::Int(2), Tok::Eof]);
        assert_eq!(toks("1 /* a /* nested */ b */ 2"), vec![Tok::Int(1), Tok::Int(2), Tok::Eof]);
    }

    #[test]
    fn positions() {
        let ts = lex("let x\n  = 1;").unwrap();
        assert_eq!((ts[0].line, ts[0].col), (1, 1)); // let
        assert_eq!((ts[1].line, ts[1].col), (1, 5)); // x
        assert_eq!((ts[2].line, ts[2].col), (2, 3)); // =
    }

    #[test]
    fn errors() {
        assert!(lex("\"abc").unwrap_err().contains("unterminated string"));
        assert!(lex("\"\\q\"").unwrap_err().contains("invalid escape"));
        assert!(lex("/* nope").unwrap_err().contains("unterminated block comment"));
        assert!(lex("let # = 1").unwrap_err().contains("unexpected character '#'"));
        assert!(lex("18446744073709551616").unwrap_err().contains("out of range"));
        assert!(lex("#").unwrap_err().contains("unexpected character"));
    }
}
