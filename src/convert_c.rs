//! `zeno convert program.c`: the part that reads C (docs/convert.md,
//! decisions chapter 63).
//!
//! Three steps, each of which may give up on a piece without giving up
//! on the file:
//!
//! 1. `lex` makes tokens of the whole file. A line of the preprocessor
//!    (`#include`, `#define`) is one token of its own, and comments are
//!    kept beside the tokens, by line.
//! 2. `pieces` cuts the file into its top-level declarations: a line of
//!    the preprocessor, a function with its body, a `struct`, a global
//!    variable. This needs nothing of C but where a declaration ends.
//! 3. `Parser` reads one piece into a tree, for the part of C the
//!    converter knows. Anything else is `Err(Keep)`, with the line and
//!    what was met there, and the piece is kept as C.
//!
//! The converter reads no header. The types of `<stdint.h>` and
//! `<stdbool.h>` are known by name, and the functions of the standard
//! library it knows are in `convert_c_write.rs`, which writes the Zeno.

use std::collections::HashMap;

pub use crate::convert_python::{keep, Keep, Kept};

// ------------------------------------------------------------------ Tokens

#[derive(Debug, Clone, PartialEq)]
pub enum T {
    Name(String),
    /// A whole number: as written, its value, and what its letters at
    /// the end (`u`, `l`) and its base make of its type.
    Int { text: String, value: u64, ty: IntTy },
    /// A number with a point or an exponent, as written without its
    /// `f`, and whether it had one (a `float`, not a `double`).
    Float { text: String, single: bool },
    /// A character between single quotes: its value.
    Char(u32),
    /// A text between double quotes, its escapes read.
    Str(String),
    Punct(&'static str),
    /// A line of the preprocessor, from the `#` to its end, with the
    /// lines a `\` joins to it.
    Directive(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub kind: T,
    pub line: usize,
    /// The last line the token is on.
    pub end_line: usize,
}

/// A comment, without its `//` or `/* */`, one item a line.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub line: usize,
    pub lines: Vec<String>,
    /// Whether nothing but blanks is in front of it on its line.
    pub alone: bool,
}

pub struct Lexed {
    pub toks: Vec<Tok>,
    pub comments: Vec<Comment>,
}

const PUNCTS: &[&str] = &[
    "...", "<<=", ">>=", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+=", "-=", "*=", "/=", "%=", "&=", "^=", "|=", "##",
    "[", "]", "(", ")", "{", "}", ".", "&", "*", "+", "-", "~", "!", "/", "%", "<", ">", "^", "|", "?", ":", ";", "=", ",", "#",
];

/// A whole number type of C, on a machine where `int` has 32 bits and
/// `long` 64, as on every desktop that runs Zeno.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntTy {
    pub bits: u8,
    pub signed: bool,
    /// `char`, which is a whole number of 8 bits that is printed as a
    /// letter by `%c` and `putchar`.
    pub char: bool,
    /// `size_t` and the types like it, which count bytes and items: an
    /// unsigned number of 64 bits that Zeno writes as an `int`, since it
    /// is never past 2**63 and stands beside `int`s (63.2). Other 64-bit
    /// unsigned numbers are Zeno's `u64`.
    pub count: bool,
}

impl IntTy {
    pub const INT: IntTy = IntTy { bits: 32, signed: true, char: false, count: false };
    pub const UINT: IntTy = IntTy { bits: 32, signed: false, char: false, count: false };
    pub const LONG: IntTy = IntTy { bits: 64, signed: true, char: false, count: false };
    pub const ULONG: IntTy = IntTy { bits: 64, signed: false, char: false, count: false };
    pub const SIZE: IntTy = IntTy { bits: 64, signed: false, char: false, count: true };
    pub const CHAR: IntTy = IntTy { bits: 8, signed: true, char: true, count: false };

    /// An unsigned number of 64 bits that is not a count: Zeno's `u64`.
    pub fn is_u64(self) -> bool {
        self.bits == 64 && !self.signed && !self.count
    }

    pub fn max(self) -> u64 {
        if self.signed {
            (1u64 << (self.bits - 1)) - 1
        } else if self.bits == 64 {
            u64::MAX
        } else {
            (1u64 << self.bits) - 1
        }
    }

    pub fn min(self) -> i64 {
        if self.signed {
            if self.bits == 64 {
                i64::MIN
            } else {
                -(1i64 << (self.bits - 1))
            }
        } else {
            0
        }
    }

    /// What C makes of it in a sum: everything smaller than an `int` is
    /// an `int` first.
    pub fn promoted(self) -> IntTy {
        if self.bits < 32 {
            IntTy::INT
        } else {
            IntTy { char: false, ..self }
        }
    }
}

/// A type of C, as far as the converter reads types.
#[derive(Debug, Clone, PartialEq)]
pub enum CTy {
    Void,
    Int(IntTy),
    /// `float` when `true`, `double` when `false`.
    Float(bool),
    Bool,
    Struct(String),
    /// An array, with its length if it was written.
    Array(Box<CTy>, Option<usize>),
    /// A pointer, and whether what it points to is `const`.
    Ptr(Box<CTy>, bool),
}

impl CTy {
    /// The type as C writes it, for the report.
    pub fn c_name(&self) -> String {
        match self {
            CTy::Void => "void".into(),
            CTy::Int(int) => {
                let base = match (int.bits, int.char) {
                    (8, true) => "char",
                    (8, false) => "char",
                    (16, _) => "short",
                    (32, _) => "int",
                    _ => "long",
                };
                match (int.signed, int.char) {
                    (true, _) => base.to_string(),
                    (false, _) => format!("unsigned {base}"),
                }
            }
            CTy::Float(true) => "float".into(),
            CTy::Float(false) => "double".into(),
            CTy::Bool => "bool".into(),
            CTy::Struct(name) => format!("struct {name}"),
            CTy::Array(item, _) => format!("{}[]", item.c_name()),
            CTy::Ptr(item, constant) => format!("{}{} *", if *constant { "const " } else { "" }, item.c_name()),
        }
    }

    /// `const char *`, or `char *`: what the converter reads as a text.
    pub fn is_text(&self) -> bool {
        matches!(self, CTy::Ptr(item, _) if matches!(**item, CTy::Int(IntTy { char: true, .. })))
    }
}

/// Read the whole file into tokens. A file whose tokens cannot be read,
/// a character C has no use for, a text that is never closed, is kept
/// whole.
pub fn lex(source: &str) -> Kept<Lexed> {
    let chars: Vec<char> = source.chars().collect();
    let mut toks = Vec::new();
    let mut comments: Vec<Comment> = Vec::new();
    let mut i = 0;
    let mut line = 1;
    // Whether only blanks have been met on this line so far.
    let mut fresh = true;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            fresh = true;
            i += 1;
            continue;
        }
        if c == ' ' || c == '\t' || c == '\r' || c == '\x0c' {
            i += 1;
            continue;
        }
        if c == '\\' && chars.get(i + 1) == Some(&'\n') {
            i += 2;
            line += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            let start = i + 2;
            let mut end = start;
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            let text: String = chars[start..end].iter().collect();
            comments.push(Comment { line, lines: vec![text.trim_end().to_string()], alone: fresh });
            i = end;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let start_line = line;
            let start = i + 2;
            let mut end = start;
            while end + 1 < chars.len() && !(chars[end] == '*' && chars[end + 1] == '/') {
                end += 1;
            }
            if end + 1 >= chars.len() {
                return keep(start_line, "a comment `/*` that is never closed");
            }
            let text: String = chars[start..end].iter().collect();
            line += text.matches('\n').count();
            comments.push(Comment { line: start_line, lines: comment_lines(&text), alone: fresh });
            i = end + 2;
            continue;
        }
        let was_fresh = fresh;
        fresh = false;
        if c == '#' && was_fresh {
            // A line of the preprocessor: to the end of the line, with
            // the lines a `\` at the end joins to it, and without a
            // comment at its end.
            let start_line = line;
            let mut text = String::new();
            let mut j = i;
            while j < chars.len() && chars[j] != '\n' {
                if chars[j] == '\\' && chars.get(j + 1) == Some(&'\n') {
                    text.push(' ');
                    j += 2;
                    line += 1;
                    continue;
                }
                if chars[j] == '/' && chars.get(j + 1) == Some(&'/') {
                    let mut end = j + 2;
                    while end < chars.len() && chars[end] != '\n' {
                        end += 1;
                    }
                    let comment: String = chars[j + 2..end].iter().collect();
                    comments.push(Comment { line, lines: vec![comment.trim_end().to_string()], alone: false });
                    j = end;
                    break;
                }
                if chars[j] == '/' && chars.get(j + 1) == Some(&'*') {
                    let mut end = j + 2;
                    while end + 1 < chars.len() && !(chars[end] == '*' && chars[end + 1] == '/') {
                        end += 1;
                    }
                    let comment: String = chars[j + 2..end.min(chars.len())].iter().collect();
                    comments.push(Comment { line, lines: comment_lines(&comment), alone: false });
                    line += comment.matches('\n').count();
                    j = (end + 2).min(chars.len());
                    text.push(' ');
                    continue;
                }
                text.push(chars[j]);
                j += 1;
            }
            toks.push(Tok { kind: T::Directive(text.trim().to_string()), line: start_line, end_line: line });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // A text or a character with a prefix: `L"wide"`, `u8"..."`.
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') && matches!(word.as_str(), "L" | "u" | "U" | "u8") {
                return keep(line, format!("a text or character of C with the prefix `{word}`"));
            }
            toks.push(Tok { kind: T::Name(word), line, end_line: line });
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let (kind, end) = number(&chars, i, line)?;
            toks.push(Tok { kind, line, end_line: line });
            i = end;
            continue;
        }
        if c == '"' {
            let (value, end) = quoted(&chars, i, '"', line)?;
            toks.push(Tok { kind: T::Str(value), line, end_line: line });
            i = end;
            continue;
        }
        if c == '\'' {
            let (value, end) = quoted(&chars, i, '\'', line)?;
            let mut letters = value.chars();
            let code = match (letters.next(), letters.next()) {
                (Some(letter), None) => letter as u32,
                _ => return keep(line, "a character constant of C that holds more than one character"),
            };
            toks.push(Tok { kind: T::Char(code), line, end_line: line });
            i = end;
            continue;
        }
        let rest: String = chars[i..(i + 3).min(chars.len())].iter().collect();
        match PUNCTS.iter().find(|punct| rest.starts_with(**punct)) {
            Some(punct) => {
                toks.push(Tok { kind: T::Punct(punct), line, end_line: line });
                i += punct.len();
            }
            None => return keep(line, format!("the character `{c}`, which C has no use for")),
        }
    }
    Ok(Lexed { toks, comments })
}

/// The lines of a `/* */` comment, without the `*` that begins each
/// line of a comment laid out the usual way.
fn comment_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let trimmed = trimmed.strip_prefix('*').map(|rest| rest.strip_prefix(' ').unwrap_or(rest)).unwrap_or(line.trim());
            trimmed.trim_end().to_string()
        })
        .collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    // `/* a comment */` reads as ` a comment `: one space in front, as
    // `#` and its text are written.
    lines.iter_mut().for_each(|line| *line = format!(" {}", line.trim_start()));
    lines
}

fn number(chars: &[char], start: usize, line: usize) -> Kept<(T, usize)> {
    let mut i = start;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_' || ((chars[i] == '+' || chars[i] == '-') && matches!(chars[i - 1], 'e' | 'E' | 'p' | 'P') && !is_hex(chars, start))) {
        i += 1;
    }
    let text: String = chars[start..i].iter().collect();
    let lower = text.to_ascii_lowercase();
    let hex = lower.starts_with("0x");
    let is_float = if hex { lower.contains('p') || lower.contains('.') } else { lower.contains('.') || lower.contains('e') };
    if is_float {
        if hex {
            return keep(line, format!("the number `{text}`, a float written in hexadecimal"));
        }
        let (body, single) = match lower.strip_suffix('f') {
            Some(body) => (body.to_string(), true),
            None if lower.ends_with('l') => return keep(line, format!("the number `{text}`, a `long double`")),
            None => (lower.clone(), false),
        };
        if body.parse::<f64>().is_err() {
            return keep(line, format!("the number `{text}`"));
        }
        return Ok((T::Float { text: body, single }, i));
    }
    // `u` and `l` are never digits, in hexadecimal either.
    let digits_end = lower.find(['u', 'l']).unwrap_or(lower.len());
    let (digits, suffix) = lower.split_at(digits_end);
    let (radix, body) = if let Some(body) = digits.strip_prefix("0x") {
        (16, body)
    } else if let Some(body) = digits.strip_prefix("0b") {
        (2, body)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    let Ok(value) = u64::from_str_radix(body, radix) else {
        return keep(line, format!("the number `{text}`"));
    };
    let unsigned = suffix.contains('u');
    let long = suffix.contains('l');
    if !suffix.chars().all(|c| c == 'u' || c == 'l') || suffix.matches('u').count() > 1 {
        return keep(line, format!("the number `{text}`"));
    }
    // C's rule for the type of a number: the first of these it fits.
    let decimal = radix == 10;
    let mut order: Vec<IntTy> = Vec::new();
    match (unsigned, long) {
        (false, false) if decimal => order.extend([IntTy::INT, IntTy::LONG]),
        (false, false) => order.extend([IntTy::INT, IntTy::UINT, IntTy::LONG, IntTy::ULONG]),
        (true, false) => order.extend([IntTy::UINT, IntTy::ULONG]),
        (false, true) if decimal => order.push(IntTy::LONG),
        (false, true) => order.extend([IntTy::LONG, IntTy::ULONG]),
        (true, true) => order.push(IntTy::ULONG),
    }
    let Some(ty) = order.into_iter().find(|ty| value <= ty.max()) else {
        return keep(line, format!("the number `{text}`, too big for any whole number of C"));
    };
    Ok((T::Int { text, value, ty }, i))
}

fn is_hex(chars: &[char], start: usize) -> bool {
    chars.get(start) == Some(&'0') && matches!(chars.get(start + 1), Some('x') | Some('X'))
}

/// A text or a character between its quotes, its escapes read.
fn quoted(chars: &[char], start: usize, quote: char, line: usize) -> Kept<(String, usize)> {
    let mut i = start + 1;
    let mut out = String::new();
    loop {
        let Some(&c) = chars.get(i) else {
            return keep(line, "a text that is never closed");
        };
        if c == '\n' {
            return keep(line, "a text that is never closed on its line");
        }
        if c == quote {
            return Ok((out, i + 1));
        }
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&e) = chars.get(i + 1) else {
            return keep(line, "a text that is never closed");
        };
        i += 2;
        let simple = match e {
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            '\\' => Some('\\'),
            '\'' => Some('\''),
            '"' => Some('"'),
            '?' => Some('?'),
            'a' => Some('\x07'),
            'b' => Some('\x08'),
            'f' => Some('\x0c'),
            'v' => Some('\x0b'),
            _ => None,
        };
        if let Some(letter) = simple {
            out.push(letter);
            continue;
        }
        if e == 'x' {
            let begin = i;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            let digits: String = chars[begin..i].iter().collect();
            match u32::from_str_radix(&digits, 16).ok().filter(|code| *code < 0x80) {
                Some(code) => out.push(char::from_u32(code).unwrap_or('\0')),
                None => return keep(line, format!("the escape `\\x{digits}`, a byte that is not a letter of ASCII")),
            }
            continue;
        }
        if e.is_digit(8) {
            let begin = i - 1;
            while i < chars.len() && i < begin + 3 && chars[i].is_digit(8) {
                i += 1;
            }
            let digits: String = chars[begin..i].iter().collect();
            match u32::from_str_radix(&digits, 8).ok().filter(|code| *code < 0x80) {
                Some(code) => out.push(char::from_u32(code).unwrap_or('\0')),
                None => return keep(line, format!("the escape `\\{digits}`, a byte that is not a letter of ASCII")),
            }
            continue;
        }
        return keep(line, format!("the escape `\\{e}`"));
    }
}

// ------------------------------------------------------------------ Pieces

/// A top-level declaration of the file, as tokens.
#[derive(Debug, Clone)]
pub struct Piece {
    pub line: usize,
    pub end_line: usize,
    /// Where its tokens are in `Lexed::toks`.
    pub from: usize,
    pub to: usize,
}

/// Cut the file into its top-level declarations: a `;` or a function's
/// closing `}` at the left margin ends one, and a line of the
/// preprocessor is one by itself.
pub fn pieces(lexed: &Lexed) -> Vec<Piece> {
    let toks = &lexed.toks;
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    // The depth at which a `{` came after a `)`: a function's body.
    let mut body_open = false;
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        let mut end = false;
        match &tok.kind {
            T::Directive(_) if depth == 0 && i == start => end = true,
            T::Punct("{") => {
                if depth == 0 {
                    body_open = i > 0 && toks[i - 1].kind == T::Punct(")");
                }
                depth += 1;
            }
            T::Punct("(") | T::Punct("[") => depth += 1,
            T::Punct(")") | T::Punct("]") => depth = depth.saturating_sub(1),
            T::Punct("}") => {
                depth = depth.saturating_sub(1);
                if depth == 0 && body_open {
                    end = true;
                }
            }
            T::Punct(";") if depth == 0 => end = true,
            _ => {}
        }
        if end {
            out.push(Piece { line: toks[start].line, end_line: tok.end_line, from: start, to: i + 1 });
            start = i + 1;
            body_open = false;
        }
        i += 1;
    }
    if start < toks.len() {
        out.push(Piece { line: toks[start].line, end_line: toks[toks.len() - 1].end_line, from: start, to: toks.len() });
    }
    out
}

// -------------------------------------------------------------------- Tree

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: E,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum E {
    Int { text: String, value: u64, ty: IntTy },
    Float { text: String, single: bool },
    Char(u32),
    Str(String),
    Name(String),
    /// `-x`, `+x`, `!x`, `~x`, `&x`, `*x`.
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    /// `a = b`, `a += b`: the operator without its `=`, or `=` itself.
    Assign(&'static str, Box<Expr>, Box<Expr>),
    /// `++x` and `--x` (`true` for the first), `x++` and `x--`.
    Pre(bool, Box<Expr>),
    Post(bool, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// A call of a name: C calls through a pointer too, which is kept.
    Call(String, Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// `a.b`, and `a->b` when the flag is set.
    Member(Box<Expr>, String, bool),
    Cast(CTy, Box<Expr>),
    SizeofType(CTy),
    SizeofExpr(Box<Expr>),
    Comma(Box<Expr>, Box<Expr>),
    /// `{1, 2, 3}`, which is only an initializer.
    List(Vec<Expr>),
    /// `.x = 1` in an initializer of a struct.
    Field(String, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub ty: CTy,
    pub init: Option<Expr>,
    pub constant: bool,
    pub stat: bool,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: S,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum S {
    Decl(Vec<Var>),
    Expr(Expr),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    Do(Box<Stmt>, Expr),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Switch(Expr, Vec<Stmt>),
    /// `case 3:`; its value. Only inside the body of a `switch`.
    Case(Expr),
    Default,
    Break,
    Continue,
    Return(Option<Expr>),
    Block(Vec<Stmt>),
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: CTy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub ret: CTy,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub stat: bool,
    pub line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `#include <stdio.h>`: the header, as written between its marks.
    Include(String),
    /// `#define SIZE 16`: a name for a value.
    Define(String, Expr),
    /// Any other line of the preprocessor that the C keeps as it is: a
    /// `#define` with parameters, or one whose value is not a value.
    Macro(String),
    Function(Function),
    /// A function declared without its body, as `int twice(int);`: its
    /// name, its type and its parameters.
    Prototype(String, CTy, Vec<Param>),
    Globals(Vec<Var>),
    Struct(String, Vec<Param>),
    /// The names an `enum` gives, with their values.
    Enum(Vec<(String, i64)>),
    /// A `typedef` of a type the converter already knows by its own
    /// name, or a declaration of a struct without its fields.
    Nothing,
}

/// What the file has declared so far: the parser needs it to tell a
/// cast from a value in brackets, and the types behind `typedef` names.
#[derive(Debug, Clone, Default)]
pub struct Names {
    pub typedefs: HashMap<String, CTy>,
    pub structs: HashMap<String, Vec<Param>>,
    pub enums: HashMap<String, i64>,
    /// The whole numbers that a `#define` gives a name, for the lengths
    /// of arrays.
    pub defines: HashMap<String, i64>,
    /// Whether the file includes `<stdbool.h>`, which makes `bool`,
    /// `true` and `false` words.
    pub stdbool: bool,
}

/// The type names of `<stdint.h>` and `<stddef.h>`, which a file uses
/// without a declaration the converter can see.
fn known_type(name: &str) -> Option<CTy> {
    let int = |bits, signed| Some(CTy::Int(IntTy { bits, signed, char: false, count: false }));
    match name {
        "int8_t" => int(8, true),
        "int16_t" => int(16, true),
        "int32_t" => int(32, true),
        "int64_t" | "ssize_t" | "ptrdiff_t" | "intptr_t" | "intmax_t" => int(64, true),
        "uint8_t" => int(8, false),
        "uint16_t" => int(16, false),
        "uint32_t" => int(32, false),
        "uint64_t" | "uintmax_t" => int(64, false),
        "size_t" | "uintptr_t" => Some(CTy::Int(IntTy::SIZE)),
        _ => None,
    }
}

pub struct Parser<'a> {
    toks: &'a [Tok],
    at: usize,
    pub names: &'a mut Names,
    /// The line of the last token, for a piece that ends too soon.
    last_line: usize,
}

const TYPE_WORDS: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "_Bool", "struct", "union", "enum", "const", "volatile",
    "static", "extern", "register", "inline", "typedef", "auto", "restrict",
];

/// Words of C the converter does not translate, and what they are, for
/// the report.
fn not_read(word: &str) -> Option<&'static str> {
    Some(match word {
        "goto" => "`goto`, which Zeno does not have",
        "union" => "a `union`, which Zeno does not have",
        "volatile" => "`volatile`, which Zeno writes otherwise",
        "extern" => "`extern`, a variable of another file",
        "register" | "auto" | "restrict" => "a word of C that Zeno has no use for",
        "_Generic" | "_Static_assert" | "_Atomic" | "_Thread_local" | "_Alignas" | "_Alignof" | "asm" | "__asm__" | "__attribute__" => {
            "a word of C that Zeno has no use for"
        }
        _ => return None,
    })
}

struct Specs {
    ty: CTy,
    constant: bool,
    stat: bool,
    typedef: bool,
    /// Set when the specifiers declared a struct or an enum with its body.
    declared: Option<Item>,
}

impl<'a> Parser<'a> {
    pub fn new(toks: &'a [Tok], names: &'a mut Names) -> Parser<'a> {
        let last_line = toks.last().map(|tok| tok.end_line).unwrap_or(1);
        Parser { toks, at: 0, names, last_line }
    }

    fn peek(&self) -> Option<&T> {
        self.toks.get(self.at).map(|tok| &tok.kind)
    }

    fn peek_at(&self, ahead: usize) -> Option<&T> {
        self.toks.get(self.at + ahead).map(|tok| &tok.kind)
    }

    fn line(&self) -> usize {
        self.toks.get(self.at).map(|tok| tok.line).unwrap_or(self.last_line)
    }

    fn is(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(T::Punct(found)) if *found == punct)
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(T::Name(found)) if found == word)
    }

    fn eat(&mut self, punct: &str) -> bool {
        if self.is(punct) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, punct: &str) -> Kept<()> {
        if self.eat(punct) {
            return Ok(());
        }
        let found = self.peek().map(describe).unwrap_or_else(|| "the end of the declaration".into());
        keep(self.line(), format!("`{punct}` was looked for, and {found} is there"))
    }

    fn name(&mut self) -> Kept<String> {
        match self.peek() {
            Some(T::Name(name)) if !is_c_keyword(name) => {
                let name = name.clone();
                self.at += 1;
                Ok(name)
            }
            other => {
                let found = other.map(describe).unwrap_or_else(|| "the end of the declaration".into());
                keep(self.line(), format!("a name was looked for, and {found} is there"))
            }
        }
    }

    /// Whether a type begins here: a word of a type, or a name that a
    /// `typedef` made.
    fn starts_type(&self) -> bool {
        match self.peek() {
            Some(T::Name(word)) => {
                TYPE_WORDS.contains(&word.as_str())
                    || self.names.typedefs.contains_key(word)
                    || known_type(word).is_some()
                    || (word == "bool" && self.names.stdbool)
            }
            _ => false,
        }
    }

    /// The words in front of a declaration: `static const unsigned int`.
    fn specs(&mut self) -> Kept<Specs> {
        let line = self.line();
        let mut constant = false;
        let mut stat = false;
        let mut typedef = false;
        let mut signed = None;
        let mut short = false;
        let mut longs = 0;
        let mut base: Option<&str> = None;
        let mut named: Option<CTy> = None;
        let mut declared = None;
        while let Some(T::Name(word)) = self.peek().cloned() {
            if let Some(why) = not_read(&word) {
                return keep(self.line(), why);
            }
            match word.as_str() {
                "const" => constant = true,
                "static" => stat = true,
                "inline" => {}
                "typedef" => typedef = true,
                "signed" => signed = Some(true),
                "unsigned" => signed = Some(false),
                "short" => short = true,
                "long" => longs += 1,
                "void" | "char" | "int" | "float" | "double" | "_Bool" => {
                    if base.is_some() {
                        return keep(self.line(), "two types in one declaration");
                    }
                    base = Some(match word.as_str() {
                        "void" => "void",
                        "char" => "char",
                        "int" => "int",
                        "float" => "float",
                        "double" => "double",
                        _ => "bool",
                    });
                }
                "bool" if self.names.stdbool && base.is_none() && named.is_none() => base = Some("bool"),
                "struct" => {
                    self.at += 1;
                    let (ty, item) = self.struct_spec()?;
                    named = Some(ty);
                    declared = item;
                    continue;
                }
                "enum" => {
                    self.at += 1;
                    let item = self.enum_spec()?;
                    named = Some(CTy::Int(IntTy::INT));
                    declared = item;
                    continue;
                }
                other if base.is_none() && named.is_none() && signed.is_none() && !short && longs == 0 => {
                    if let Some(ty) = self.names.typedefs.get(other).cloned().or_else(|| known_type(other)) {
                        named = Some(ty);
                    } else {
                        break;
                    }
                }
                _ => break,
            }
            self.at += 1;
        }
        let ty = if let Some(ty) = named {
            if base.is_some() || signed.is_some() || short || longs > 0 {
                return keep(line, "a declaration whose type is written in a way the converter does not read");
            }
            ty
        } else {
            let int = |bits| CTy::Int(IntTy { bits, signed: signed.unwrap_or(true), char: false, count: false });
            match base {
                Some("void") => CTy::Void,
                Some("bool") => CTy::Bool,
                Some("float") => CTy::Float(true),
                Some("double") if longs > 0 => return keep(line, "a `long double`, which has more bits than any float of Zeno"),
                Some("double") => CTy::Float(false),
                Some("char") => CTy::Int(IntTy { bits: 8, signed: signed.unwrap_or(true), char: signed.is_none(), count: false }),
                _ if short => int(16),
                _ if longs > 0 => int(64),
                Some("int") => int(32),
                None if signed.is_some() => int(32),
                None => return keep(line, "a declaration without a type"),
                Some(_) => int(32),
            }
        };
        Ok(Specs { ty, constant, stat, typedef, declared })
    }

    /// After `struct`: a name, a body, or both.
    fn struct_spec(&mut self) -> Kept<(CTy, Option<Item>)> {
        let line = self.line();
        let name = if matches!(self.peek(), Some(T::Name(_))) { Some(self.name()?) } else { None };
        if !self.eat("{") {
            let Some(name) = name else {
                return keep(line, "a `struct` without a name or fields");
            };
            return Ok((CTy::Struct(name), None));
        }
        let mut fields = Vec::new();
        while !self.eat("}") {
            let specs = self.specs()?;
            if specs.stat || specs.typedef || specs.declared.is_some() {
                return keep(self.line(), "a field of a struct that the converter does not read");
            }
            loop {
                let (field, ty, _) = self.declarator(specs.ty.clone(), specs.constant)?;
                if (matches!(ty, CTy::Ptr(..)) && !ty.is_text()) || self.is(":") {
                    let why = if self.is(":") { "a field of bits, which Zeno writes otherwise" } else { "a field that is a pointer" };
                    return keep(self.line(), why);
                }
                fields.push(Param { name: field, ty });
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(";")?;
        }
        let name = match name {
            Some(name) => name,
            None => match self.peek() {
                // `typedef struct { ... } Point;` names it after its typedef.
                Some(T::Name(alias)) => alias.clone(),
                _ => return keep(line, "a `struct` without a name"),
            },
        };
        self.names.structs.insert(name.clone(), fields.clone());
        Ok((CTy::Struct(name.clone()), Some(Item::Struct(name, fields))))
    }

    /// After `enum`: the names it gives, which are whole numbers.
    fn enum_spec(&mut self) -> Kept<Option<Item>> {
        if matches!(self.peek(), Some(T::Name(_))) {
            self.name()?;
        }
        if !self.eat("{") {
            return Ok(None);
        }
        let mut next = 0i64;
        let mut values = Vec::new();
        while !self.eat("}") {
            let name = self.name()?;
            if self.eat("=") {
                let expr = self.conditional()?;
                next = match const_value(&expr, self.names) {
                    Some(value) => value,
                    None => return keep(expr.line, "a value of an `enum` that is not a whole number written out"),
                };
            }
            self.names.enums.insert(name.clone(), next);
            values.push((name, next));
            next += 1;
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(Some(Item::Enum(values)))
    }

    /// A declarator after the specifiers: `*p`, `values[8]`, `grid[3][3]`.
    /// `constant` is whether the specifiers said `const`; what comes
    /// back says whether the variable itself is `const`, which for a
    /// pointer is a `const` after its `*`.
    fn declarator(&mut self, base: CTy, constant: bool) -> Kept<(String, CTy, bool)> {
        let mut ty = base;
        let mut fixed = constant;
        let mut pointee_const = constant;
        while self.eat("*") {
            ty = CTy::Ptr(Box::new(ty), pointee_const);
            fixed = false;
            while self.eat_word("const") {
                fixed = true;
            }
            pointee_const = fixed;
        }
        if self.is("(") {
            return keep(self.line(), "a pointer to a function, which Zeno writes otherwise");
        }
        let name = self.name()?;
        let mut sizes = Vec::new();
        while self.eat("[") {
            if self.eat("]") {
                sizes.push(None);
                continue;
            }
            let expr = self.conditional()?;
            match const_value(&expr, self.names) {
                Some(size) if size > 0 => sizes.push(Some(size as usize)),
                _ => return keep(expr.line, "an array whose length is not a number written out, or the name of one"),
            }
            self.expect("]")?;
        }
        for size in sizes.into_iter().rev() {
            ty = CTy::Array(Box::new(ty), size);
        }
        Ok((name, ty, fixed))
    }

    /// A type in a cast or in `sizeof( )`: specifiers and stars.
    fn type_name(&mut self) -> Kept<CTy> {
        let specs = self.specs()?;
        let mut ty = specs.ty;
        while self.eat("*") {
            ty = CTy::Ptr(Box::new(ty), specs.constant);
            while self.eat_word("const") {}
        }
        if self.is("[") || self.is("(") {
            return keep(self.line(), "a type the converter does not read");
        }
        Ok(ty)
    }

    /// One top-level declaration.
    pub fn item(&mut self) -> Kept<Item> {
        if let Some(T::Directive(text)) = self.peek().cloned() {
            self.at += 1;
            return directive(&text, self.line(), self.names);
        }
        let line = self.line();
        if !self.starts_type() {
            if let Some(T::Name(word)) = self.peek() {
                if let Some(why) = not_read(word) {
                    return keep(line, why);
                }
                // `main() { }`: a function of old C, whose type is `int`.
            }
            return keep(line, format!("a declaration the converter does not read, beginning with {}", self.peek().map(describe).unwrap_or_default()));
        }
        let specs = self.specs()?;
        if specs.typedef {
            if let Some(Item::Struct(name, fields)) = &specs.declared {
                // `typedef struct { } Name;` and `typedef struct Name { } Name;`
                let alias = self.name()?;
                self.expect(";")?;
                self.names.typedefs.insert(alias.clone(), CTy::Struct(name.clone()));
                if alias != *name {
                    self.names.structs.insert(alias.clone(), fields.clone());
                    self.names.typedefs.insert(alias.clone(), CTy::Struct(name.clone()));
                }
                return Ok(Item::Struct(name.clone(), fields.clone()));
            }
            let (alias, ty, _) = self.declarator(specs.ty, specs.constant)?;
            self.expect(";")?;
            self.names.typedefs.insert(alias, ty);
            return Ok(specs.declared.unwrap_or(Item::Nothing));
        }
        if self.eat(";") {
            return Ok(specs.declared.unwrap_or(Item::Nothing));
        }
        if specs.declared.is_some() {
            return keep(line, "a variable declared together with its struct or enum");
        }
        let (name, ty, fixed) = self.declarator(specs.ty.clone(), specs.constant)?;
        if self.is("(") {
            return self.function(name, ty, specs.stat, line);
        }
        let mut vars = Vec::new();
        let mut current = (name, ty, fixed);
        loop {
            let init = if self.eat("=") { Some(self.initializer()?) } else { None };
            vars.push(Var { name: current.0, ty: current.1, init, constant: current.2, stat: specs.stat, line });
            if !self.eat(",") {
                break;
            }
            current = self.declarator(specs.ty.clone(), specs.constant)?;
        }
        self.expect(";")?;
        Ok(Item::Globals(vars))
    }

    fn function(&mut self, name: String, ret: CTy, stat: bool, line: usize) -> Kept<Item> {
        self.expect("(")?;
        let mut params = Vec::new();
        let no_params = self.is_word("void") && matches!(self.peek_at(1), Some(T::Punct(")")));
        if no_params {
            self.at += 1;
        }
        while !self.eat(")") {
            if self.is("...") {
                return keep(self.line(), "a function that takes any number of arguments (`...`)");
            }
            let specs = self.specs()?;
            let (param, ty, _) = self.declarator(specs.ty.clone(), specs.constant)?;
            // `int values[]` is a pointer, as C reads it.
            let ty = match ty {
                CTy::Array(item, _) => CTy::Ptr(item, specs.constant),
                other => other,
            };
            params.push(Param { name: param, ty });
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        if self.eat(";") {
            return Ok(Item::Prototype(name, ret, params));
        }
        if !self.is("{") {
            return keep(self.line(), "a function written in the old way of C, with its parameters' types after it");
        }
        let body = self.block()?;
        let end_line = self.toks.get(self.at.saturating_sub(1)).map(|tok| tok.end_line).unwrap_or(line);
        if self.at < self.toks.len() {
            return keep(self.line(), "more after the end of a function");
        }
        Ok(Item::Function(Function { name, ret, params, body, stat, line, end_line }))
    }

    fn block(&mut self) -> Kept<Vec<Stmt>> {
        self.expect("{")?;
        let mut body = Vec::new();
        while !self.eat("}") {
            if self.at >= self.toks.len() {
                return keep(self.last_line, "a `{` that is never closed");
            }
            body.push(self.statement()?);
        }
        Ok(body)
    }

    fn statement(&mut self) -> Kept<Stmt> {
        let line = self.line();
        let stmt = |kind| Ok(Stmt { kind, line });
        if let Some(T::Directive(_)) = self.peek() {
            return keep(line, "a line of the preprocessor inside a function");
        }
        if self.is("{") {
            return stmt(S::Block(self.block()?));
        }
        if self.eat(";") {
            return stmt(S::Empty);
        }
        if let Some(T::Name(word)) = self.peek().cloned() {
            if let Some(why) = not_read(&word) {
                return keep(line, why);
            }
            if matches!(self.peek_at(1), Some(T::Punct(":"))) && !is_c_keyword(&word) {
                return keep(line, "a label, which is for `goto`");
            }
            match word.as_str() {
                "if" => {
                    self.at += 1;
                    self.expect("(")?;
                    let test = self.expression()?;
                    self.expect(")")?;
                    let then = self.statement()?;
                    let other = if self.eat_word("else") { Some(Box::new(self.statement()?)) } else { None };
                    return stmt(S::If(test, Box::new(then), other));
                }
                "while" => {
                    self.at += 1;
                    self.expect("(")?;
                    let test = self.expression()?;
                    self.expect(")")?;
                    return stmt(S::While(test, Box::new(self.statement()?)));
                }
                "do" => {
                    self.at += 1;
                    let body = self.statement()?;
                    if !self.eat_word("while") {
                        return keep(self.line(), "a `do` without its `while`");
                    }
                    self.expect("(")?;
                    let test = self.expression()?;
                    self.expect(")")?;
                    self.expect(";")?;
                    return stmt(S::Do(Box::new(body), test));
                }
                "for" => {
                    self.at += 1;
                    self.expect("(")?;
                    let init = if self.eat(";") {
                        None
                    } else if self.starts_type() {
                        Some(Box::new(self.declaration()?))
                    } else {
                        let line = self.line();
                        let expr = self.expression()?;
                        self.expect(";")?;
                        Some(Box::new(Stmt { kind: S::Expr(expr), line }))
                    };
                    let test = if self.is(";") { None } else { Some(self.expression()?) };
                    self.expect(";")?;
                    let step = if self.is(")") { None } else { Some(self.expression()?) };
                    self.expect(")")?;
                    let body = self.statement()?;
                    return stmt(S::For(init, test, step, Box::new(body)));
                }
                "switch" => {
                    self.at += 1;
                    self.expect("(")?;
                    let value = self.expression()?;
                    self.expect(")")?;
                    if !self.is("{") {
                        return keep(self.line(), "a `switch` without braces");
                    }
                    let body = self.block()?;
                    return stmt(S::Switch(value, body));
                }
                "case" => {
                    self.at += 1;
                    let value = self.conditional()?;
                    self.expect(":")?;
                    return stmt(S::Case(value));
                }
                "default" => {
                    self.at += 1;
                    self.expect(":")?;
                    return stmt(S::Default);
                }
                "break" => {
                    self.at += 1;
                    self.expect(";")?;
                    return stmt(S::Break);
                }
                "continue" => {
                    self.at += 1;
                    self.expect(";")?;
                    return stmt(S::Continue);
                }
                "return" => {
                    self.at += 1;
                    let value = if self.is(";") { None } else { Some(self.expression()?) };
                    self.expect(";")?;
                    return stmt(S::Return(value));
                }
                _ => {}
            }
        }
        if self.starts_type() {
            return self.declaration();
        }
        let expr = self.expression()?;
        self.expect(";")?;
        stmt(S::Expr(expr))
    }

    /// A declaration of local variables, with its `;`.
    fn declaration(&mut self) -> Kept<Stmt> {
        let line = self.line();
        let specs = self.specs()?;
        if specs.typedef || specs.declared.is_some() {
            return keep(line, "a type declared inside a function");
        }
        let mut vars = Vec::new();
        loop {
            let (name, ty, fixed) = self.declarator(specs.ty.clone(), specs.constant)?;
            if self.is("(") {
                return keep(line, "a function declared inside a function");
            }
            let init = if self.eat("=") { Some(self.initializer()?) } else { None };
            vars.push(Var { name, ty, init, constant: fixed, stat: specs.stat, line });
            if !self.eat(",") {
                break;
            }
        }
        self.expect(";")?;
        Ok(Stmt { kind: S::Decl(vars), line })
    }

    /// The value after the `=` of a declaration: a value, or `{ }`.
    fn initializer(&mut self) -> Kept<Expr> {
        let line = self.line();
        if !self.eat("{") {
            return self.assignment();
        }
        let mut items = Vec::new();
        while !self.eat("}") {
            if self.eat(".") {
                let field = self.name()?;
                self.expect("=")?;
                items.push(Expr { kind: E::Field(field, Box::new(self.initializer()?)), line: self.line() });
            } else if self.is("[") {
                return keep(self.line(), "an initializer that names the place of an item (`[2] = ...`)");
            } else {
                items.push(self.initializer()?);
            }
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(Expr { kind: E::List(items), line })
    }

    pub fn expression(&mut self) -> Kept<Expr> {
        let mut left = self.assignment()?;
        while self.is(",") {
            let line = self.line();
            self.at += 1;
            let right = self.assignment()?;
            left = Expr { kind: E::Comma(Box::new(left), Box::new(right)), line };
        }
        Ok(left)
    }

    fn assignment(&mut self) -> Kept<Expr> {
        let left = self.conditional()?;
        let op = match self.peek() {
            Some(T::Punct(op)) if matches!(*op, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=") => *op,
            _ => return Ok(left),
        };
        let line = self.line();
        self.at += 1;
        let right = self.assignment()?;
        let op: &'static str = if op == "=" { "=" } else { &op[..op.len() - 1] };
        Ok(Expr { kind: E::Assign(op, Box::new(left), Box::new(right)), line })
    }

    fn conditional(&mut self) -> Kept<Expr> {
        let test = self.binary(0)?;
        if !self.is("?") {
            return Ok(test);
        }
        let line = self.line();
        self.at += 1;
        let yes = self.expression()?;
        self.expect(":")?;
        let no = self.conditional()?;
        Ok(Expr { kind: E::Cond(Box::new(test), Box::new(yes), Box::new(no)), line })
    }

    fn binary(&mut self, level: usize) -> Kept<Expr> {
        const LEVELS: &[&[&str]] = &[&["||"], &["&&"], &["|"], &["^"], &["&"], &["==", "!="], &["<", ">", "<=", ">="], &["<<", ">>"], &["+", "-"], &["*", "/", "%"]];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        loop {
            let op = match self.peek() {
                Some(T::Punct(op)) if LEVELS[level].contains(op) => *op,
                _ => return Ok(left),
            };
            let line = self.line();
            self.at += 1;
            let right = self.binary(level + 1)?;
            left = Expr { kind: E::Binary(op, Box::new(left), Box::new(right)), line };
        }
    }

    fn unary(&mut self) -> Kept<Expr> {
        let line = self.line();
        let expr = |kind| Ok(Expr { kind, line });
        if let Some(T::Punct(op)) = self.peek().cloned() {
            match op {
                "-" | "+" | "!" | "~" | "&" | "*" => {
                    self.at += 1;
                    let operand = self.unary()?;
                    return expr(E::Unary(op, Box::new(operand)));
                }
                "++" | "--" => {
                    self.at += 1;
                    let operand = self.unary()?;
                    return expr(E::Pre(op == "++", Box::new(operand)));
                }
                "(" => {
                    // A cast: a type between the brackets.
                    let save = self.at;
                    self.at += 1;
                    if self.starts_type() {
                        let ty = self.type_name()?;
                        self.expect(")")?;
                        if self.is("{") {
                            return keep(line, "a value of a struct written as `(type){ }`");
                        }
                        let operand = self.unary()?;
                        return expr(E::Cast(ty, Box::new(operand)));
                    }
                    self.at = save;
                }
                _ => {}
            }
        }
        if self.eat_word("sizeof") {
            if self.is("(") {
                let save = self.at;
                self.at += 1;
                if self.starts_type() {
                    let ty = self.type_name()?;
                    self.expect(")")?;
                    return expr(E::SizeofType(ty));
                }
                self.at = save;
            }
            let operand = self.unary()?;
            return expr(E::SizeofExpr(Box::new(operand)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Kept<Expr> {
        let mut value = self.primary()?;
        loop {
            let line = self.line();
            if self.eat("[") {
                let index = self.expression()?;
                self.expect("]")?;
                value = Expr { kind: E::Index(Box::new(value), Box::new(index)), line };
            } else if self.is("(") {
                let E::Name(callee) = &value.kind else {
                    return keep(line, "a call of a function through a pointer");
                };
                let callee = callee.clone();
                self.at += 1;
                let mut args = Vec::new();
                while !self.eat(")") {
                    args.push(self.assignment()?);
                    if !self.eat(",") {
                        self.expect(")")?;
                        break;
                    }
                }
                value = Expr { kind: E::Call(callee, args), line: value.line };
            } else if self.eat(".") {
                let field = self.name()?;
                value = Expr { kind: E::Member(Box::new(value), field, false), line };
            } else if self.eat("->") {
                let field = self.name()?;
                value = Expr { kind: E::Member(Box::new(value), field, true), line };
            } else if self.eat("++") {
                value = Expr { kind: E::Post(true, Box::new(value)), line };
            } else if self.eat("--") {
                value = Expr { kind: E::Post(false, Box::new(value)), line };
            } else {
                return Ok(value);
            }
        }
    }

    fn primary(&mut self) -> Kept<Expr> {
        let line = self.line();
        let expr = |kind| Ok(Expr { kind, line });
        let Some(tok) = self.peek().cloned() else {
            return keep(line, "a value was looked for, and the declaration ends there");
        };
        match tok {
            T::Int { text, value, ty } => {
                self.at += 1;
                expr(E::Int { text, value, ty })
            }
            T::Float { text, single } => {
                self.at += 1;
                expr(E::Float { text, single })
            }
            T::Char(code) => {
                self.at += 1;
                expr(E::Char(code))
            }
            T::Str(text) => {
                self.at += 1;
                let mut text = text;
                // "two " "parts" is one text, as C joins them.
                while let Some(T::Str(more)) = self.peek().cloned() {
                    text.push_str(&more);
                    self.at += 1;
                }
                expr(E::Str(text))
            }
            T::Name(name) => {
                if let Some(why) = not_read(&name) {
                    return keep(line, why);
                }
                if is_c_keyword(&name) {
                    return keep(line, format!("`{name}` where a value was looked for"));
                }
                self.at += 1;
                expr(E::Name(name))
            }
            T::Punct("(") => {
                self.at += 1;
                if self.is("{") {
                    return keep(line, "a block of statements used as a value, which only GCC has");
                }
                let inner = self.expression()?;
                self.expect(")")?;
                Ok(inner)
            }
            other => keep(line, format!("{} where a value was looked for", describe(&other))),
        }
    }

    pub fn done(&self) -> bool {
        self.at >= self.toks.len()
    }
}

/// The name, type and parameters of a function whose body could not be
/// read: what its first line says.
pub fn signature(toks: &[Tok], names: &mut Names) -> Option<(String, CTy, Vec<Param>)> {
    let open = toks.iter().position(|tok| tok.kind == T::Punct("{"))?;
    let mut head: Vec<Tok> = toks[..open].to_vec();
    let line = head.last()?.end_line;
    head.push(Tok { kind: T::Punct(";"), line, end_line: line });
    match Parser::new(&head, names).item() {
        Ok(Item::Prototype(name, ret, params)) => Some((name, ret, params)),
        _ => None,
    }
}

/// A line of the preprocessor: what it is to the converter.
fn directive(text: &str, line: usize, names: &mut Names) -> Kept<Item> {
    let body = text.trim_start_matches('#').trim_start();
    let (word, rest) = body.split_once(|c: char| c.is_whitespace()).unwrap_or((body, ""));
    let rest = rest.trim();
    match word {
        "include" => {
            if rest == "<stdbool.h>" {
                names.stdbool = true;
            }
            Ok(Item::Include(rest.to_string()))
        }
        "define" => {
            let name_end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
            let (name, value) = rest.split_at(name_end);
            if value.starts_with('(') {
                return Ok(Item::Macro(name.to_string()));
            }
            let value = value.trim();
            if value.is_empty() {
                return Ok(Item::Macro(name.to_string()));
            }
            // The value of a `#define` is a value of C when it reads as one.
            let Ok(lexed) = lex(value) else {
                return Ok(Item::Macro(name.to_string()));
            };
            let toks: Vec<Tok> = lexed.toks.into_iter().map(|tok| Tok { line, end_line: line, ..tok }).collect();
            let mut parser = Parser::new(&toks, names);
            match parser.expression() {
                Ok(expr) if parser.done() && simple_value(&expr) => {
                    if let Some(value) = const_value(&expr, names) {
                        names.defines.insert(name.to_string(), value);
                    }
                    Ok(Item::Define(name.to_string(), relined(expr, line)))
                }
                _ => Ok(Item::Macro(name.to_string())),
            }
        }
        "pragma" if rest == "once" => Ok(Item::Nothing),
        _ => keep(line, format!("`#{word}`, a line of the preprocessor that changes what the file is")),
    }
}

/// A value that a `#define` may give a name for: numbers, texts, names
/// and the operators between them, nothing that does something.
fn simple_value(expr: &Expr) -> bool {
    match &expr.kind {
        E::Int { .. } | E::Float { .. } | E::Char(_) | E::Str(_) | E::Name(_) => true,
        E::Unary(op, operand) => matches!(*op, "-" | "+" | "~" | "!") && simple_value(operand),
        E::Binary(_, left, right) => simple_value(left) && simple_value(right),
        E::Cast(_, operand) => simple_value(operand),
        E::Cond(test, yes, no) => simple_value(test) && simple_value(yes) && simple_value(no),
        _ => false,
    }
}

fn relined(mut expr: Expr, line: usize) -> Expr {
    expr.line = line;
    expr.kind = match expr.kind {
        E::Unary(op, operand) => E::Unary(op, Box::new(relined(*operand, line))),
        E::Binary(op, left, right) => E::Binary(op, Box::new(relined(*left, line)), Box::new(relined(*right, line))),
        E::Cast(ty, operand) => E::Cast(ty, Box::new(relined(*operand, line))),
        E::Cond(a, b, c) => E::Cond(Box::new(relined(*a, line)), Box::new(relined(*b, line)), Box::new(relined(*c, line))),
        other => other,
    };
    expr
}

/// The value of a whole number written out, or of the name of one that
/// an `enum` gave, with the operators between them.
pub fn const_value(expr: &Expr, names: &Names) -> Option<i64> {
    match &expr.kind {
        E::Int { value, .. } => i64::try_from(*value).ok(),
        E::Char(code) => Some(*code as i64),
        E::Name(name) => names.enums.get(name).or_else(|| names.defines.get(name)).copied(),
        E::Unary("-", operand) => const_value(operand, names).and_then(i64::checked_neg),
        E::Unary("+", operand) => const_value(operand, names),
        E::Binary(op, left, right) => {
            let (a, b) = (const_value(left, names)?, const_value(right, names)?);
            match *op {
                "+" => a.checked_add(b),
                "-" => a.checked_sub(b),
                "*" => a.checked_mul(b),
                "/" if b != 0 => a.checked_div(b),
                "%" if b != 0 => a.checked_rem(b),
                "<<" if (0..31).contains(&b) => a.checked_shl(b as u32).filter(|v| *v <= i32::MAX as i64),
                "|" => Some(a | b),
                "&" => Some(a & b),
                _ => None,
            }
        }
        _ => None,
    }
}

pub fn is_c_keyword(word: &str) -> bool {
    matches!(
        word,
        "auto" | "break" | "case" | "char" | "const" | "continue" | "default" | "do" | "double" | "else" | "enum" | "extern" | "float" | "for" | "goto"
            | "if" | "inline" | "int" | "long" | "register" | "restrict" | "return" | "short" | "signed" | "sizeof" | "static" | "struct" | "switch"
            | "typedef" | "union" | "unsigned" | "void" | "volatile" | "while" | "_Bool"
    )
}

pub fn describe(kind: &T) -> String {
    match kind {
        T::Name(name) => format!("`{name}`"),
        T::Int { text, .. } => format!("the number `{text}`"),
        T::Float { text, .. } => format!("the number `{text}`"),
        T::Char(_) => "a character".into(),
        T::Str(_) => "a text".into(),
        T::Punct(punct) => format!("`{punct}`"),
        T::Directive(text) => format!("`{text}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(source: &str) -> Vec<Kept<Item>> {
        let lexed = lex(source).unwrap();
        let mut names = Names::default();
        pieces(&lexed)
            .iter()
            .map(|piece| {
                let mut parser = Parser::new(&lexed.toks[piece.from..piece.to], &mut names);
                parser.item()
            })
            .collect()
    }

    #[test]
    fn a_file_is_cut_where_its_declarations_end() {
        let source = "#include <stdio.h>\n#define SIZE 4\n\nstruct P { int x; int y; };\nint total = 0;\n\nint twice(int n) {\n    return n * 2;\n}\n\nint main(void) {\n    printf(\"%d\\n\", twice(SIZE));\n    return 0;\n}\n";
        let lexed = lex(source).unwrap();
        let lines: Vec<usize> = pieces(&lexed).iter().map(|piece| piece.line).collect();
        assert_eq!(lines, vec![1, 2, 4, 5, 7, 11]);
        let read = items(source);
        assert!(matches!(&read[0], Ok(Item::Include(header)) if header == "<stdio.h>"));
        assert!(matches!(&read[1], Ok(Item::Define(name, _)) if name == "SIZE"));
        assert!(matches!(&read[2], Ok(Item::Struct(name, fields)) if name == "P" && fields.len() == 2));
        assert!(matches!(&read[4], Ok(Item::Function(f)) if f.name == "twice" && f.params.len() == 1));
    }

    #[test]
    fn numbers_have_the_type_c_gives_them() {
        let lexed = lex("1 4000000000 0xFFFFFFFF 10u 7L 017 1.5f 2.0").unwrap();
        let kinds: Vec<T> = lexed.toks.into_iter().map(|tok| tok.kind).collect();
        assert!(matches!(&kinds[0], T::Int { ty, .. } if *ty == IntTy::INT));
        assert!(matches!(&kinds[1], T::Int { ty, .. } if *ty == IntTy::LONG));
        assert!(matches!(&kinds[2], T::Int { ty, .. } if *ty == IntTy::UINT));
        assert!(matches!(&kinds[3], T::Int { ty, .. } if *ty == IntTy::UINT));
        assert!(matches!(&kinds[4], T::Int { ty, .. } if *ty == IntTy::LONG));
        assert!(matches!(&kinds[5], T::Int { value: 15, .. }));
        assert!(matches!(&kinds[6], T::Float { single: true, .. }));
        assert!(matches!(&kinds[7], T::Float { single: false, .. }));
    }

    #[test]
    fn what_is_not_read_says_why() {
        let read = items("int f(int *p) { goto end; end: return 0; }\n#ifdef DEBUG\n");
        assert!(matches!(&read[0], Err(keep) if keep.why.contains("goto")));
        assert!(matches!(&read[1], Err(keep) if keep.why.contains("#ifdef")));
    }

    #[test]
    fn comments_are_kept_by_line() {
        let lexed = lex("/* A file.\n * Two lines. */\nint x; // the count\n").unwrap();
        assert_eq!(lexed.comments.len(), 2);
        assert_eq!(lexed.comments[0].lines, vec![" A file.".to_string(), " Two lines.".to_string()]);
        assert!(!lexed.comments[1].alone);
    }
}
