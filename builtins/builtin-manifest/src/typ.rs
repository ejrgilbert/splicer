//! Tiny hand-rolled parser for the subset of WIT type expressions the
//! manifest schema accepts. Produces a [`TypeAst`] that the codegen
//! translates into a `wasm_wave::value::Type` builder chain at build
//! time, and the splicer-side validator pattern-matches against to
//! type-check TOML/YAML inputs.
//!
//! Grammar (informally):
//!
//! ```text
//! type      = primitive
//!           | "list<" type ">"
//!           | "option<" type ">"
//!           | "tuple<" type ("," type)+ ">"
//!           | "enum" "{" ident ("," ident)* "}"
//!
//! primitive = "bool" | "u8" | "u16" | "u32" | "u64"
//!           | "s8" | "s16" | "s32" | "s64"
//!           | "f32" | "f64" | "char" | "string"
//!
//! ident     = [a-zA-Z_-][a-zA-Z0-9_-]*
//! ```
//!
//! Records / variants / flags / results are intentionally absent —
//! they'd be additive when a real consumer appears. Errors carry the
//! offending byte offset so manifest-author mistakes have a chance of
//! being legible.

use core::fmt;

/// Parsed WIT type expression. Variants line up 1:1 with the
/// `wasm_wave::value::Type` constructors the codegen emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeAst {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    F32,
    F64,
    Char,
    String,
    List(Box<TypeAst>),
    Option(Box<TypeAst>),
    Tuple(Vec<TypeAst>),
    Enum { cases: Vec<String> },
}

impl TypeAst {
    /// Render the AST back as canonical WIT type-expr text. Used by
    /// CLI output and by codegen for stable string keys.
    pub fn display(&self) -> String {
        match self {
            Self::Bool => "bool".into(),
            Self::U8 => "u8".into(),
            Self::U16 => "u16".into(),
            Self::U32 => "u32".into(),
            Self::U64 => "u64".into(),
            Self::S8 => "s8".into(),
            Self::S16 => "s16".into(),
            Self::S32 => "s32".into(),
            Self::S64 => "s64".into(),
            Self::F32 => "f32".into(),
            Self::F64 => "f64".into(),
            Self::Char => "char".into(),
            Self::String => "string".into(),
            Self::List(t) => format!("list<{}>", t.display()),
            Self::Option(t) => format!("option<{}>", t.display()),
            Self::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(Self::display).collect();
                format!("tuple<{}>", parts.join(", "))
            }
            Self::Enum { cases } => format!("enum {{ {} }}", cases.join(", ")),
        }
    }
}

#[derive(Debug)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_wit_type(src: &str) -> Result<TypeAst, ParseError> {
    let mut p = Parser::new(src);
    let ty = p.parse_type()?;
    p.skip_ws();
    if p.pos < p.src.len() {
        return Err(p.err(format!("unexpected trailing input: {:?}", &p.src[p.pos..])));
    }
    Ok(ty)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            offset: self.pos,
            message: message.into(),
        }
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn eat_char(&mut self, expected: char) -> Result<(), ParseError> {
        self.skip_ws();
        match self.peek_char() {
            Some(c) if c == expected => {
                self.pos += c.len_utf8();
                Ok(())
            }
            Some(c) => Err(self.err(format!("expected {expected:?}, got {c:?}"))),
            None => Err(self.err(format!("expected {expected:?}, got end of input"))),
        }
    }

    fn eat_ident(&mut self) -> Result<&'a str, ParseError> {
        self.skip_ws();
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        if self.pos == start {
            Err(self.err("expected identifier"))
        } else {
            Ok(&self.src[start..self.pos])
        }
    }

    fn parse_type(&mut self) -> Result<TypeAst, ParseError> {
        self.skip_ws();
        let ident_start = self.pos;
        let ident = self.eat_ident()?;
        match ident {
            "bool" => Ok(TypeAst::Bool),
            "u8" => Ok(TypeAst::U8),
            "u16" => Ok(TypeAst::U16),
            "u32" => Ok(TypeAst::U32),
            "u64" => Ok(TypeAst::U64),
            "s8" => Ok(TypeAst::S8),
            "s16" => Ok(TypeAst::S16),
            "s32" => Ok(TypeAst::S32),
            "s64" => Ok(TypeAst::S64),
            "f32" => Ok(TypeAst::F32),
            "f64" => Ok(TypeAst::F64),
            "char" => Ok(TypeAst::Char),
            "string" => Ok(TypeAst::String),
            "list" => {
                self.eat_char('<')?;
                let inner = self.parse_type()?;
                self.eat_char('>')?;
                Ok(TypeAst::List(Box::new(inner)))
            }
            "option" => {
                self.eat_char('<')?;
                let inner = self.parse_type()?;
                self.eat_char('>')?;
                Ok(TypeAst::Option(Box::new(inner)))
            }
            "tuple" => {
                self.eat_char('<')?;
                let mut elems = vec![self.parse_type()?];
                loop {
                    self.skip_ws();
                    match self.peek_char() {
                        Some(',') => {
                            self.pos += 1;
                            elems.push(self.parse_type()?);
                        }
                        Some('>') => break,
                        _ => {
                            return Err(self.err("expected ',' or '>' inside tuple<...>"));
                        }
                    }
                }
                self.eat_char('>')?;
                if elems.len() < 2 {
                    return Err(ParseError {
                        offset: ident_start,
                        message: "tuple must have at least two element types".into(),
                    });
                }
                Ok(TypeAst::Tuple(elems))
            }
            "enum" => {
                self.eat_char('{')?;
                let mut cases: Vec<String> = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek_char() == Some('}') {
                        break;
                    }
                    let case = self.eat_ident()?.to_string();
                    if cases.iter().any(|c| c == &case) {
                        return Err(self.err(format!("duplicate enum case '{case}'")));
                    }
                    cases.push(case);
                    self.skip_ws();
                    match self.peek_char() {
                        Some(',') => {
                            self.pos += 1;
                        }
                        Some('}') => break,
                        _ => {
                            return Err(self.err("expected ',' or '}' inside enum body"));
                        }
                    }
                }
                self.eat_char('}')?;
                if cases.is_empty() {
                    return Err(ParseError {
                        offset: ident_start,
                        message: "enum must have at least one case".into(),
                    });
                }
                Ok(TypeAst::Enum { cases })
            }
            other => Err(ParseError {
                offset: ident_start,
                message: format!("unknown type '{other}'"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives() {
        assert_eq!(parse_wit_type("u32").unwrap(), TypeAst::U32);
        assert_eq!(parse_wit_type("  bool  ").unwrap(), TypeAst::Bool);
        assert_eq!(parse_wit_type("f64").unwrap(), TypeAst::F64);
    }

    #[test]
    fn list_of_string() {
        let ty = parse_wit_type("list<string>").unwrap();
        assert_eq!(ty, TypeAst::List(Box::new(TypeAst::String)));
    }

    #[test]
    fn enum_with_cases() {
        let ty = parse_wit_type("enum { trace, debug, info }").unwrap();
        assert_eq!(
            ty,
            TypeAst::Enum {
                cases: vec!["trace".into(), "debug".into(), "info".into()],
            }
        );
    }

    #[test]
    fn nested() {
        let ty = parse_wit_type("option<list<u8>>").unwrap();
        assert_eq!(
            ty,
            TypeAst::Option(Box::new(TypeAst::List(Box::new(TypeAst::U8))))
        );
    }

    #[test]
    fn tuple_arity_check() {
        let err = parse_wit_type("tuple<u32>").unwrap_err();
        assert!(err.message.contains("at least two"), "{err}");
    }

    #[test]
    fn duplicate_enum_case_rejected() {
        let err = parse_wit_type("enum { a, b, a }").unwrap_err();
        assert!(err.message.contains("duplicate"), "{err}");
    }

    #[test]
    fn trailing_garbage_rejected() {
        let err = parse_wit_type("u32 xyz").unwrap_err();
        assert!(err.message.contains("trailing"), "{err}");
    }

    #[test]
    fn unknown_primitive() {
        let err = parse_wit_type("u128").unwrap_err();
        assert!(err.message.contains("unknown type"), "{err}");
    }

    #[test]
    fn display_round_trip() {
        let inputs = [
            "u32",
            "list<string>",
            "option<u8>",
            "tuple<u32, string>",
            "enum { a, b, c }",
        ];
        for input in inputs {
            let ty = parse_wit_type(input).unwrap();
            let re = parse_wit_type(&ty.display()).unwrap();
            assert_eq!(ty, re, "round-trip mismatch for {input}");
        }
    }
}
