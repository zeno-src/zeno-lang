//! `zeno convert program.c`: writing Zeno from the tree of one piece of C
//! (docs/convert.md, decisions chapter 63). Values, statements and
//! functions are here; what is kept, and the file as a whole, are in
//! `convert_c_write.rs`.
//!
//! Every value is written with the meaning C gives it, or not at all:
//!
//! - A whole number of C is a whole number of Zeno, which is exact. A
//!   signed one that would overflow in C is undefined there, so exact is
//!   one thing it may be.
//! - An unsigned one goes round, and Zeno's do not, so a value of type
//!   `unsigned` carries whether it is the value C has (`exact`) or only
//!   a number with the same low 32 bits. `u32.wrap( )` makes it exact,
//!   and is written only where the value is looked at: stored, compared,
//!   divided, printed.
//! - `/` and `%` take the quotient toward zero in C, and down in Zeno.
//!   They are the same when neither side is below zero, which is often
//!   known; otherwise `c_divide` and `c_remainder` are written.
//! - A comparison is a bool in Zeno and a whole number in C. Used as a
//!   number, it is not written.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::convert::{zeno_text, Note};
use crate::convert_c::*;

// Zeno's levels of precedence, loosest first (decisions 2.7).
pub const OR: u8 = 1;
pub const AND: u8 = 2;
pub const NOT: u8 = 3;
pub const CMP: u8 = 4;
pub const BOR: u8 = 5;
pub const BXOR: u8 = 6;
pub const BAND: u8 = 7;
pub const SHIFT: u8 = 8;
pub const ADD: u8 = 9;
pub const MUL: u8 = 10;
pub const UNARY: u8 = 11;
pub const POW: u8 = 12;
pub const ATOM: u8 = 13;

/// A function that C has and Zeno does not, written once in the file
/// when something uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Helper {
    Divide,
    Remainder,
    TimesU32,
    LetterCode,
    Letter,
    IsDigit,
    IsAlpha,
    IsAlnum,
    IsSpace,
    IsUpper,
    IsLower,
    ToUpper,
    ToLower,
    /// `printf` that writes part of a line: the line waits until it ends.
    Text,
}

impl Helper {
    pub fn name(self) -> &'static str {
        match self {
            Helper::Divide => "c_divide",
            Helper::Remainder => "c_remainder",
            Helper::TimesU32 => "times_u32",
            Helper::LetterCode => "letter_code",
            Helper::Letter => "letter",
            Helper::IsDigit => "isdigit",
            Helper::IsAlpha => "isalpha",
            Helper::IsAlnum => "isalnum",
            Helper::IsSpace => "isspace",
            Helper::IsUpper => "isupper",
            Helper::IsLower => "islower",
            Helper::ToUpper => "toupper",
            Helper::ToLower => "tolower",
            Helper::Text => "write_text",
        }
    }

    /// The helpers this one calls.
    pub fn needs(self) -> Vec<Helper> {
        match self {
            Helper::Remainder => vec![Helper::Divide],
            Helper::IsAlnum => vec![Helper::IsAlpha, Helper::IsDigit],
            Helper::IsAlpha => vec![Helper::IsUpper, Helper::IsLower],
            Helper::ToUpper => vec![Helper::IsLower],
            Helper::ToLower => vec![Helper::IsUpper],
            _ => Vec::new(),
        }
    }

    pub fn lines(self) -> Vec<&'static str> {
        match self {
            Helper::Divide => vec![
                "# C's `/` of two whole numbers takes the quotient toward zero, where Zeno's `//`",
                "# takes it down: -7 / 2 is -3 in C, and -7 // 2 is -4.",
                "function c_divide(a: int, b: int) -> int {",
                "    let down = a // b",
                "    if down < 0 and down * b != a {",
                "        return down + 1",
                "    }",
                "    return down",
                "}",
            ],
            Helper::Remainder => vec![
                "# C's `%` of two whole numbers has the sign of `a`, where Zeno's has the sign of",
                "# `b`: -7 % 2 is -1 in C, and 1 in Zeno.",
                "function c_remainder(a: int, b: int) -> int {",
                "    return a - b * c_divide(a, b)",
                "}",
            ],
            Helper::TimesU32 => vec![
                "# C's `*` of two `unsigned int`s keeps the low 32 bits of the product. The whole",
                "# product may not fit in 64 bits, so it is made in two halves.",
                "function times_u32(a: int, b: int) -> u32 {",
                "    let low = (a & 0xFFFF) * b",
                "    let high = ((a >> 16) * b) & 0xFFFF",
                "    return u32.wrap(low + (high << 16))",
                "}",
            ],
            Helper::LetterCode => vec![
                "# C's `text[i]`: the code of the letter at `i`, and 0 just past the end, where C",
                "# keeps a `\\0`.",
                "function letter_code(text: str, at: int) -> int {",
                "    if at == len(text) {",
                "        return 0",
                "    }",
                "    return text[at].code()",
                "}",
            ],
            Helper::Letter => vec![
                "# C's `%c` and `putchar`: the letter whose code this is. C writes one byte, so a",
                "# code past 127 is not a letter Zeno can write the same.",
                "function letter(code: int) -> str raises ValueError {",
                "    let byte = code & 255",
                "    if byte > 127 {",
                "        raise ValueError(f\"the byte {byte} is not a letter of ASCII\")",
                "    }",
                "    return char(byte)",
                "}",
            ],
            Helper::IsDigit => vec![
                "# The tests of <ctype.h>, on the letters of ASCII, as C has them.",
                "function isdigit(code: int) -> bool {",
                "    return code >= \"0\".code() and code <= \"9\".code()",
                "}",
            ],
            Helper::IsUpper => vec!["function isupper(code: int) -> bool {", "    return code >= \"A\".code() and code <= \"Z\".code()", "}"],
            Helper::IsLower => vec!["function islower(code: int) -> bool {", "    return code >= \"a\".code() and code <= \"z\".code()", "}"],
            Helper::IsAlpha => vec!["function isalpha(code: int) -> bool {", "    return isupper(code) or islower(code)", "}"],
            Helper::IsAlnum => vec!["function isalnum(code: int) -> bool {", "    return isalpha(code) or isdigit(code)", "}"],
            Helper::IsSpace => vec![
                "function isspace(code: int) -> bool {",
                "    return code == \" \".code() or (code >= \"\\t\".code() and code <= \"\\r\".code())",
                "}",
            ],
            Helper::ToUpper => vec![
                "function toupper(code: int) -> int {",
                "    if islower(code) {",
                "        return code - \"a\".code() + \"A\".code()",
                "    }",
                "    return code",
                "}",
            ],
            Helper::ToLower => vec![
                "function tolower(code: int) -> int {",
                "    if isupper(code) {",
                "        return code - \"A\".code() + \"a\".code()",
                "    }",
                "    return code",
                "}",
            ],
            Helper::Text => vec![
                "# C's `printf` may write part of a line; Zeno's `print` writes a whole one. So the",
                "# parts wait here until their line ends.",
                "var line_so_far = \"\"",
                "",
                "function write_text(text: str) {",
                "    let lines = (line_so_far + text).split(\"\\n\")",
                "    for at in range(0, len(lines) - 1) {",
                "        print(lines[at])",
                "    }",
                "    line_so_far = lines[len(lines) - 1]",
                "}",
                "",
                "# What is left of the last line, when the program ends.",
                "function finish_text() {",
                "    if line_so_far != \"\" {",
                "        print(line_so_far)",
                "        line_so_far = \"\"",
                "    }",
                "}",
            ],
        }
    }
}

/// How a parameter of a function is used, which says what Zeno gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A value, as C passes it.
    Value,
    /// A pointer that is used as an array, `p[i]`: a list.
    Array,
    /// A pointer to one number, used only as `*p`: a list of one, which
    /// the caller fills from its variable and empties back into it.
    Box,
    /// A pointer to a struct: the object itself, which Zeno shares.
    Ref,
    /// A `char *` that is read as a text.
    Text,
}

#[derive(Debug, Clone)]
pub struct FnInfo {
    pub unit: usize,
    pub ret: CTy,
    pub params: Vec<Param>,
    pub kinds: Vec<Kind>,
    /// The global variables it uses, and those of every function it
    /// calls.
    pub globals: HashSet<String>,
    /// For each parameter, whether the function changes it: a struct
    /// given to one that does is copied first, as C copies it.
    pub changes: Vec<bool>,
    /// Whether the body could be read: a function the converter could
    /// not read cannot be called from Zeno, even when it is kept.
    pub read: bool,
}

#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub unit: usize,
    pub ty: CTy,
    pub constant: bool,
}

/// What the whole file declares, and what has been decided about it so
/// far: the same for every piece.
#[derive(Debug, Default)]
pub struct World {
    pub functions: HashMap<String, FnInfo>,
    pub globals: HashMap<String, GlobalInfo>,
    pub defines: HashMap<String, (usize, Expr)>,
    pub macros: HashMap<String, usize>,
    pub names: Names,
    /// The pieces kept as C.
    pub kept: HashSet<usize>,
    /// Names of C that are words of Zeno, and what they are called here.
    pub renamed: HashMap<String, String>,
    pub block: String,
    /// Whether the program writes parts of lines, so that every line
    /// goes through `write_text`.
    pub buffered: bool,
    /// Global variables that some function changes.
    pub changed: HashSet<String>,
    /// Every name in the file, so that a new one is never one of them.
    pub used: HashSet<String>,
}

impl World {
    pub fn zeno_name(&self, name: &str) -> String {
        self.renamed.get(name).cloned().unwrap_or_else(|| name.to_string())
    }
}

/// A value as written in Zeno, with what C knows of it.
#[derive(Debug, Clone)]
pub struct Val {
    pub code: String,
    pub prec: u8,
    pub ty: CTy,
    /// For an unsigned value: whether `code` is the value C has, or only
    /// a number with the same low bits.
    pub exact: bool,
    /// For a value that is not exact: it is below 2 ** bits in size.
    pub bits: u32,
    /// Known not to be below zero.
    pub nonneg: bool,
    /// The type of `code` in Zeno is the sized type of `ty`: `u8`,
    /// `u16`, `u32` or `f32`, and not `int` or `float`.
    pub sized: bool,
    /// The value, when it is known before the program runs.
    pub value: Option<i128>,
    /// `code` is a number written out.
    pub literal: bool,
    /// `code` is a place, a variable, a field or an item: a struct read
    /// from one is shared, where C would copy it.
    pub place: bool,
    /// A number written with `f`, which is a `float` of 32 bits in C.
    pub single_literal: bool,
}

impl Val {
    fn new(code: impl Into<String>, prec: u8, ty: CTy) -> Val {
        Val { code: code.into(), prec, ty, exact: true, bits: 64, nonneg: false, sized: false, value: None, literal: false, place: false, single_literal: false }
    }

    fn int(&self) -> Option<IntTy> {
        match &self.ty {
            CTy::Int(int) => Some(*int),
            _ => None,
        }
    }

    fn is_bool(&self) -> bool {
        self.ty == CTy::Bool
    }
}

fn is_sized(ty: &CTy) -> bool {
    matches!(ty, CTy::Int(IntTy { signed: false, bits: 8 | 16 | 32, .. }) | CTy::Float(true)) || is_u64(ty)
}

/// A `uint64_t`, an `unsigned long`: Zeno's `u64`.
fn is_u64(ty: &CTy) -> bool {
    matches!(ty, CTy::Int(int) if int.is_u64())
}

/// `code` in brackets when it binds more loosely than `prec`.
fn paren(v: &Val, prec: u8) -> String {
    if v.prec < prec {
        format!("({})", v.code)
    } else {
        v.code.clone()
    }
}

/// The type of Zeno for a type of C, for a variable, a field or a value.
pub fn zeno_type(ty: &CTy, line: usize) -> Kept<String> {
    Ok(match ty {
        CTy::Int(IntTy { signed: false, bits: 8, .. }) => "u8".into(),
        CTy::Int(IntTy { signed: false, bits: 16, .. }) => "u16".into(),
        CTy::Int(IntTy { signed: false, bits: 32, .. }) => "u32".into(),
        CTy::Int(int) if int.is_u64() => "u64".into(),
        CTy::Int(_) => "int".into(),
        CTy::Float(true) => "f32".into(),
        CTy::Float(false) => "float".into(),
        CTy::Bool => "bool".into(),
        CTy::Struct(name) => name.clone(),
        CTy::Array(item, _) => format!("list[{}]", zeno_type(item, line)?),
        ty if ty.is_text() => "str".into(),
        CTy::Ptr(..) => return keep(line, format!("a value of type `{}`, a pointer", ty.c_name())),
        CTy::Void => return keep(line, "a value of type `void`"),
    })
}

/// The type of C a usual sum of two whole numbers has (C 6.3.1.8).
fn usual(a: IntTy, b: IntTy) -> IntTy {
    let (a, b) = (a.promoted(), b.promoted());
    // A `size_t` beside a `uint64_t` is the `uint64_t`: the one Zeno
    // holds in all 64 bits.
    if a.is_u64() || b.is_u64() {
        if a.bits == 64 && b.bits == 64 && !(a.signed && b.signed) || a.is_u64() && b.bits < 64 || b.is_u64() && a.bits < 64 {
            return IntTy::ULONG;
        }
    }
    if a.signed == b.signed {
        return if a.bits >= b.bits { a } else { b };
    }
    let (unsigned, signed) = if a.signed { (b, a) } else { (a, b) };
    if unsigned.bits >= signed.bits {
        unsigned
    } else {
        signed
    }
}

/// How many bits the size of a value can need, as far as is known.
fn bits_of(v: &Val) -> u32 {
    if let Some(value) = v.value {
        return (128 - value.unsigned_abs().leading_zeros()).max(1);
    }
    if !v.exact {
        return v.bits;
    }
    match &v.ty {
        CTy::Int(int) => int.bits as u32,
        _ => 64,
    }
}

/// The place a loop or a `switch` sets for `break` and `continue`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Breakable {
    /// A loop, and whether `continue` means the same in what is written:
    /// it does not in a `for` written as `while`, whose step `continue`
    /// would pass by.
    Loop(bool),
    Switch,
}

#[derive(Debug, Clone)]
struct Local {
    zeno: String,
    ty: CTy,
    kind: Kind,
    nonneg: bool,
}

/// Writes the Zeno for one piece.
pub struct W<'a> {
    pub world: &'a World,
    pub out: Vec<String>,
    depth: usize,
    scopes: Vec<HashMap<String, Local>>,
    ret: CTy,
    /// `main`, when every `return` in it gives 0: it is written without
    /// a value.
    main_void: bool,
    pub helpers: BTreeSet<Helper>,
    pub notes: Vec<(Note, usize)>,
    pub imports: BTreeSet<&'static str>,
    pub copied: BTreeSet<String>,
    breakables: Vec<Breakable>,
    /// The variables of the function that something gives a new value.
    assigned: HashSet<String>,
    taken: HashSet<String>,
    /// Lines to write before and after the statement being written: the
    /// lists of one that carry a variable to a function and back.
    pre: Vec<String>,
    post: Vec<String>,
    comments: Vec<Comment>,
    next_comment: usize,
    /// Notes met since the last line was written.
    pending: Vec<Note>,
    /// Struct parameters that every caller copies: the function's own.
    owned: HashSet<String>,
}

/// A line that stands for the `# convert:` comment of a note: the first
/// of each kind becomes the comment, and the rest go.
pub const NOTE_MARK: &str = "#@note ";

impl<'a> W<'a> {
    pub fn new(world: &'a World, comments: Vec<Comment>) -> W<'a> {
        W {
            world,
            out: Vec::new(),
            depth: 0,
            scopes: vec![HashMap::new()],
            ret: CTy::Void,
            main_void: false,
            helpers: BTreeSet::new(),
            notes: Vec::new(),
            imports: BTreeSet::new(),
            copied: BTreeSet::new(),
            breakables: Vec::new(),
            assigned: HashSet::new(),
            taken: HashSet::new(),
            pre: Vec::new(),
            post: Vec::new(),
            comments,
            next_comment: 0,
            pending: Vec::new(),
            owned: HashSet::new(),
        }
    }

    pub fn line(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref();
        for note in std::mem::take(&mut self.pending) {
            self.out.push(format!("{}{NOTE_MARK}{}", "    ".repeat(self.depth), note as u8));
        }
        if text.is_empty() {
            self.out.push(String::new());
        } else {
            self.out.push(format!("{}{}", "    ".repeat(self.depth), text));
        }
    }

    /// The comments that come before `line`, written where the code is.
    fn comments_before(&mut self, line: usize) {
        while self.next_comment < self.comments.len() && self.comments[self.next_comment].line < line {
            let lines = self.comments[self.next_comment].lines.clone();
            for text in lines {
                self.line(format!("#{text}"));
            }
            self.next_comment += 1;
        }
    }

    pub fn rest_of_comments(&mut self) {
        self.comments_before(usize::MAX);
    }

    fn fresh(&mut self, base: &str) -> String {
        let mut name = base.to_string();
        let mut n = 2;
        while self.world.used.contains(&name) || self.taken.contains(&name) || self.lookup(&name).is_some() {
            name = format!("{base}_{n}");
            n += 1;
        }
        self.taken.insert(name.clone());
        name
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn declare(&mut self, name: &str, local: Local) {
        self.scopes.last_mut().expect("a scope").insert(name.to_string(), local);
    }

    /// A place where Zeno does otherwise: counted for the report, and
    /// marked where the next line is written, for its `# convert:` comment.
    fn note(&mut self, note: Note, line: usize) {
        self.notes.push((note, line));
        if !self.pending.contains(&note) {
            self.pending.push(note);
        }
    }

    fn helper(&mut self, helper: Helper) {
        self.helpers.insert(helper);
        for needed in helper.needs() {
            self.helper(needed);
        }
    }

    // ------------------------------------------------------------- Values

    /// The value of C as one that is exact: an unsigned number that only
    /// has the right low bits is cut to them.
    fn exact(&mut self, v: Val) -> Val {
        if v.exact {
            return v;
        }
        let bits = v.int().map(|int| int.bits).unwrap_or(32);
        Val { code: format!("u{bits}.wrap({})", v.code), prec: ATOM, exact: true, nonneg: true, sized: true, bits: bits as u32, ..v }
    }

    /// An int of the type `to`, which is unsigned and of 32 bits at most:
    /// the value C converts `v` into, or a number with its low bits.
    fn as_unsigned(&mut self, v: Val, to: IntTy) -> Val {
        let from = v.int().unwrap_or(IntTy::INT);
        if from.signed && !v.nonneg {
            // A signed number below zero has the low bits C converts it to.
            return Val { ty: CTy::Int(to), exact: false, bits: bits_of(&v).max(from.bits as u32) + 1, sized: false, ..v };
        }
        Val { ty: CTy::Int(to), ..v }
    }

    /// `v` as a `u64` of Zeno, as C converts a whole number to a
    /// `uint64_t`. A sum of u64s need not be cut yet: `u64.wrap` around
    /// the whole of it, where it is looked at, cuts it (42.6).
    fn to_u64(&mut self, v: Val) -> Val {
        let from = v.int().unwrap_or(IntTy::INT);
        if from.is_u64() {
            return v;
        }
        if v.literal {
            return Val { ty: CTy::Int(IntTy::ULONG), sized: true, ..v };
        }
        let v = self.exact(v);
        let code = if v.nonneg || !from.signed { format!("u64({})", v.code) } else { format!("u64.wrap({})", v.code) };
        Val { exact: true, nonneg: true, sized: true, literal: false, ..Val::new(code, ATOM, CTy::Int(IntTy::ULONG)) }
    }

    /// A sum of two whole numbers that C works out as `uint64_t`s.
    fn arith_u64(&mut self, op: &'static str, l: Val, r: Val) -> Val {
        let prec = prec_of(op);
        let zop = if op == "/" { "//" } else { op };
        let (mut l, r, exact) = match op {
            "<<" | ">>" => {
                let l = self.to_u64(l);
                let l = if op == ">>" { self.exact(l) } else { l };
                let r = self.exact(r);
                // The count of a shift is an int.
                let r = if r.int().is_some_and(|int| int.is_u64()) { Val { value: r.value, ..Val::new(format!("int({})", r.code), ATOM, CTy::Int(IntTy::INT)) } } else { r };
                (l, r, op == ">>")
            }
            "/" | "%" => {
                let (l, r) = (self.to_u64(l), self.to_u64(r));
                (self.exact(l), self.exact(r), true)
            }
            // `u64.wrap` reaches through `+ - * <<` inside it, and not
            // through the operators on bits: their sides are cut first.
            "&" | "|" | "^" => {
                let (l, r) = (self.to_u64(l), self.to_u64(r));
                (self.exact(l), self.exact(r), true)
            }
            _ => (self.to_u64(l), self.to_u64(r), false),
        };
        // Two numbers written out are an int in Zeno: one is made a u64.
        if l.literal && (r.literal || op == "<<" || op == ">>") {
            l = Val { literal: false, ..Val::new(format!("u64({})", l.code), ATOM, CTy::Int(IntTy::ULONG)) };
        }
        let code = format!("{} {zop} {}", paren(&l, prec), paren(&r, prec + 1));
        Val { exact, nonneg: true, sized: true, ..Val::new(code, prec, CTy::Int(IntTy::ULONG)) }
    }

    pub fn expr(&mut self, e: &Expr) -> Kept<Val> {
        let line = e.line;
        match &e.kind {
            E::Int { text, value, ty } => {
                let digits = text.trim_end_matches(['u', 'l', 'U', 'L']);
                let lower = digits.to_ascii_lowercase();
                let code = if digits.len() > 1 && digits.starts_with('0') && !lower.starts_with("0x") && !lower.starts_with("0b") {
                    format!("0o{}", &digits[1..])
                } else {
                    digits.to_string()
                };
                Ok(Val { nonneg: true, value: Some(*value as i128), literal: true, ..Val::new(code, ATOM, CTy::Int(*ty)) })
            }
            E::Float { text, single } => {
                let mut code = text.clone();
                if code.starts_with('.') {
                    code.insert(0, '0');
                }
                if let Some(point) = code.find('.') {
                    if !code[point + 1..].starts_with(|c: char| c.is_ascii_digit()) {
                        code.insert(point + 1, '0');
                    }
                }
                Ok(Val { single_literal: *single, sized: false, ..Val::new(code, ATOM, CTy::Float(*single)) })
            }
            E::Char(code) => {
                let ty = CTy::Int(IntTy::INT);
                let base = Val { nonneg: true, value: Some(*code as i128), ..Val::new("", ATOM, ty) };
                if *code == 0 {
                    return Ok(Val { code: "0".into(), literal: true, ..base });
                }
                let letter = char::from_u32(*code).unwrap_or('?');
                match zeno_text(&letter.to_string(), '"') {
                    Some(text) => Ok(Val { code: format!("{text}.code()"), ..base }),
                    None => Ok(Val { code: code.to_string(), literal: true, ..base }),
                }
            }
            E::Str(text) => {
                if !text.is_ascii() {
                    return keep(line, "a text with letters outside ASCII, whose length C counts in bytes");
                }
                match zeno_text(text, '"') {
                    Some(code) => Ok(Val::new(code, ATOM, CTy::Ptr(Box::new(CTy::Int(IntTy::CHAR)), true))),
                    None => keep(line, "a text with a character that Zeno writes otherwise"),
                }
            }
            E::Name(name) => self.name(name, line),
            E::Unary(op, operand) => self.unary(op, operand, line),
            E::Binary(op, left, right) => self.binary(op, left, right, line),
            E::Assign(..) => keep(line, "an assignment used as a value"),
            E::Pre(inc, _) | E::Post(inc, _) => keep(line, format!("`{}` inside an expression", if *inc { "++" } else { "--" })),
            E::Cond(..) => keep(line, "`? :` inside an expression, which Zeno writes as an `if`"),
            E::Call(name, args) => self.call(name, args, line),
            E::Index(base, index) => self.index(base, index, line),
            E::Member(base, field, arrow) => self.member(base, field, *arrow, line),
            E::Cast(ty, operand) => self.cast(ty, operand, line),
            E::SizeofType(ty) => match size_of(ty) {
                Some(size) => Ok(Val { nonneg: true, value: Some(size as i128), literal: true, ..Val::new(size.to_string(), ATOM, CTy::Int(IntTy::SIZE)) }),
                None => keep(line, format!("`sizeof` of `{}`", ty.c_name())),
            },
            E::SizeofExpr(operand) => {
                let v = self.expr(operand)?;
                match size_of(&v.ty) {
                    Some(size) => Ok(Val { nonneg: true, value: Some(size as i128), literal: true, ..Val::new(size.to_string(), ATOM, CTy::Int(IntTy::SIZE)) }),
                    None => keep(line, "`sizeof` of a value whose size the converter does not know"),
                }
            }
            E::Comma(..) => keep(line, "the operator `,`, which Zeno does not have"),
            E::List(_) | E::Field(..) => keep(line, "`{ }` where only a value is taken"),
        }
    }

    fn name(&mut self, name: &str, line: usize) -> Kept<Val> {
        if let Some(local) = self.lookup(name).cloned() {
            return match local.kind {
                Kind::Value | Kind::Text => Ok(Val { nonneg: local.nonneg || is_unsigned(&local.ty), place: true, sized: is_sized(&local.ty), ..Val::new(local.zeno, ATOM, local.ty) }),
                Kind::Array => Ok(Val { place: true, ..Val::new(local.zeno, ATOM, local.ty) }),
                Kind::Box | Kind::Ref => keep(line, format!("the pointer `{name}` used as a value")),
            };
        }
        let world = self.world;
        if let Some(global) = world.globals.get(name) {
            if world.kept.contains(&global.unit) && !global.constant {
                return keep(line, format!("it uses `{name}`, a variable that is kept as C"));
            }
            if global.ty.is_text() || matches!(global.ty, CTy::Int(_) | CTy::Float(_) | CTy::Bool | CTy::Struct(_) | CTy::Array(..)) {
                return Ok(Val { nonneg: is_unsigned(&global.ty), place: true, sized: is_sized(&global.ty), ..Val::new(world.zeno_name(name), ATOM, global.ty.clone()) });
            }
            return keep(line, format!("the global `{name}`, a pointer"));
        }
        if let Some((_, value)) = world.defines.get(name) {
            if world.kept.contains(&world.defines[name].0) {
                return keep(line, format!("it uses `{name}`, which is kept as C"));
            }
            let value = value.clone();
            // The type of the name is the type of what it stands for.
            let saved = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
            let typed = self.expr(&value);
            self.scopes = saved;
            let typed = typed?;
            let typed = self.exact(typed);
            return Ok(Val { code: world.zeno_name(name), prec: ATOM, literal: false, place: false, sized: is_sized(&typed.ty), single_literal: false, ..typed });
        }
        if world.macros.contains_key(name) {
            return keep(line, format!("`{name}`, a macro of C"));
        }
        if let Some(value) = world.names.enums.get(name) {
            return Ok(Val { nonneg: *value >= 0, value: Some(*value as i128), ..Val::new(world.zeno_name(name), ATOM, CTy::Int(IntTy::INT)) });
        }
        if world.names.stdbool && (name == "true" || name == "false") {
            return Ok(Val::new(name, ATOM, CTy::Bool));
        }
        if let Some((value, ty)) = known_constant(name) {
            return Ok(Val { nonneg: value >= 0, value: Some(value), literal: true, ..Val::new(value.to_string(), if value < 0 { UNARY } else { ATOM }, CTy::Int(ty)) });
        }
        match name {
            "M_PI" => {
                self.imports.insert("math");
                Ok(Val::new("math.pi", ATOM, CTy::Float(false)))
            }
            "NULL" => keep(line, "`NULL`, a pointer"),
            _ if world.functions.contains_key(name) => keep(line, format!("the function `{name}` used as a value")),
            _ => keep(line, format!("`{name}`, which is not declared in this file")),
        }
    }

    fn unary(&mut self, op: &str, operand: &Expr, line: usize) -> Kept<Val> {
        match op {
            "!" => {
                if let E::Call(name, args) = &operand.kind {
                    if name == "strcmp" {
                        return self.strcmp("==", args, line);
                    }
                }
                let test = self.cond(operand)?;
                Ok(Val::new(format!("not {}", paren(&test, NOT)), NOT, CTy::Bool))
            }
            "&" => keep(line, "`&`, the address of a value, which Zeno does not have"),
            "*" => {
                if let E::Name(name) = &operand.kind {
                    if let Some(local) = self.lookup(name).cloned() {
                        match local.kind {
                            Kind::Box | Kind::Array => {
                                let item = pointee(&local.ty);
                                return Ok(Val { place: true, nonneg: is_unsigned(&item), sized: is_sized(&item), ..Val::new(format!("{}[0]", local.zeno), ATOM, item) });
                            }
                            Kind::Ref => return Ok(Val { place: true, ..Val::new(local.zeno, ATOM, pointee(&local.ty)) }),
                            _ => {}
                        }
                    }
                }
                keep(line, "`*`, what a pointer points to")
            }
            _ => {
                let v = self.expr(operand)?;
                if v.is_bool() {
                    return keep(line, "a truth value used as a number");
                }
                match (&v.ty, op) {
                    (CTy::Float(single), "-") => {
                        let single = *single;
                        Ok(Val { sized: v.sized, single_literal: v.single_literal, ..Val::new(format!("-{}", paren(&v, UNARY)), UNARY, CTy::Float(single)) })
                    }
                    (CTy::Float(_), "+") => Ok(v),
                    (CTy::Int(int), _) => {
                        let ty = int.promoted();
                        if op == "+" {
                            return Ok(Val { ty: CTy::Int(ty), ..v });
                        }
                        if ty.is_u64() {
                            // A number written out is an int in Zeno by itself.
                            let v = if v.literal { Val { literal: false, ..Val::new(format!("u64({})", v.code), ATOM, CTy::Int(ty)) } } else { v };
                            // `-x` is `0 - x`, cut where it is looked at.
                            let (code, prec, exact) = if op == "-" {
                                (format!("0 - {}", paren(&v, ADD + 1)), ADD, false)
                            } else {
                                let v = self.exact(v);
                                (format!("~{}", paren(&v, UNARY)), UNARY, true)
                            };
                            return Ok(Val { exact, nonneg: true, sized: true, ..Val::new(code, prec, CTy::Int(ty)) });
                        }
                        if !ty.signed && ty.bits == 64 {
                            return keep(line, format!("`{op}` of a `size_t`, which goes round at 2**64 in C"));
                        }
                        let bits = bits_of(&v) + 1;
                        let v = if ty.signed { self.exact(v) } else { v };
                        let value = v.value.map(|value| if op == "-" { -value } else { !value });
                        let code = format!("{op}{}", paren(&v, UNARY));
                        if ty.signed {
                            Ok(Val { value, nonneg: value.is_some_and(|value| value >= 0), ..Val::new(code, UNARY, CTy::Int(ty)) })
                        } else {
                            Ok(Val { exact: false, bits, ..Val::new(code, UNARY, CTy::Int(ty)) })
                        }
                    }
                    _ => keep(line, format!("`{op}` of a value that is not a number")),
                }
            }
        }
    }

    /// A value used as true or false: C's test of not being zero.
    pub fn cond(&mut self, e: &Expr) -> Kept<Val> {
        if let E::Call(name, args) = &e.kind {
            if name == "strcmp" {
                return self.strcmp("!=", args, e.line);
            }
        }
        let v = self.expr(e)?;
        self.test(v, e.line)
    }

    fn test(&mut self, v: Val, line: usize) -> Kept<Val> {
        match &v.ty {
            CTy::Bool => Ok(v),
            CTy::Int(_) => {
                let v = self.exact(v);
                Ok(Val::new(format!("{} != 0", paren(&v, CMP + 1)), CMP, CTy::Bool))
            }
            CTy::Float(_) => Ok(Val::new(format!("{} != 0.0", paren(&v, CMP + 1)), CMP, CTy::Bool)),
            _ => keep(line, "a pointer used as true or false"),
        }
    }

    fn strcmp(&mut self, op: &str, args: &[Expr], line: usize) -> Kept<Val> {
        if args.len() != 2 {
            return keep(line, "`strcmp` with other than two arguments");
        }
        let a = self.expr(&args[0])?;
        let b = self.expr(&args[1])?;
        if !a.ty.is_text() || !b.ty.is_text() {
            return keep(line, "`strcmp` of values that are not texts");
        }
        Ok(Val::new(format!("{} {op} {}", paren(&a, CMP + 1), paren(&b, CMP + 1)), CMP, CTy::Bool))
    }

    fn binary(&mut self, op: &'static str, left: &Expr, right: &Expr, line: usize) -> Kept<Val> {
        // `strcmp(a, b) == 0` compares the texts.
        if matches!(op, "==" | "!=" | "<" | ">" | "<=" | ">=") {
            let zero = |e: &Expr| matches!(e.kind, E::Int { value: 0, .. });
            if let E::Call(name, args) = &left.kind {
                if name == "strcmp" && zero(right) {
                    return self.strcmp(op, args, line);
                }
            }
            if let E::Call(name, args) = &right.kind {
                if name == "strcmp" && zero(left) {
                    let flipped = match op {
                        "<" => ">",
                        ">" => "<",
                        "<=" => ">=",
                        ">=" => "<=",
                        other => other,
                    };
                    return self.strcmp(flipped, args, line);
                }
            }
        }
        // `sizeof values / sizeof values[0]` is how many items it has.
        if op == "/" {
            if let Some((array, count)) = self.length_of(left, right) {
                return Ok(Val { nonneg: true, value: Some(count as i128), ..Val::new(format!("len({array})"), ATOM, CTy::Int(IntTy::SIZE)) });
            }
        }
        if op == "&&" || op == "||" {
            let a = self.cond(left)?;
            let b = self.cond(right)?;
            let (word, prec) = if op == "&&" { ("and", AND) } else { ("or", OR) };
            return Ok(Val::new(format!("{} {word} {}", paren(&a, prec), paren(&b, prec + 1)), prec, CTy::Bool));
        }
        let l = self.expr(left)?;
        let r = self.expr(right)?;
        if matches!(op, "==" | "!=" | "<" | ">" | "<=" | ">=") {
            return self.compare(op, l, r, line);
        }
        self.arith(op, l, r, line)
    }

    /// `sizeof a / sizeof a[0]`, `sizeof a / sizeof *a` or
    /// `sizeof a / sizeof(int)` for an array `a` the function can see.
    fn length_of(&self, left: &Expr, right: &Expr) -> Option<(String, usize)> {
        let E::SizeofExpr(of) = &left.kind else { return None };
        let E::Name(name) = &of.kind else { return None };
        let (zeno, ty) = match self.lookup(name) {
            Some(local) if matches!(local.ty, CTy::Array(..)) && local.kind == Kind::Array => (local.zeno.clone(), local.ty.clone()),
            Some(_) => return None,
            None => match self.world.globals.get(name) {
                Some(global) if matches!(global.ty, CTy::Array(..)) && !self.world.kept.contains(&global.unit) => (self.world.zeno_name(name), global.ty.clone()),
                _ => return None,
            },
        };
        let CTy::Array(item, Some(count)) = &ty else { return None };
        let same = match &right.kind {
            E::SizeofExpr(of) => match &of.kind {
                E::Index(base, _) => matches!(&base.kind, E::Name(found) if found == name),
                E::Unary("*", base) => matches!(&base.kind, E::Name(found) if found == name),
                _ => false,
            },
            E::SizeofType(ty) => ty == &**item,
            _ => false,
        };
        same.then_some((zeno, *count))
    }

    fn compare(&mut self, op: &str, l: Val, r: Val, line: usize) -> Kept<Val> {
        let both = |l: &Val, r: &Val| Val::new(format!("{} {op} {}", paren(l, CMP + 1), paren(r, CMP + 1)), CMP, CTy::Bool);
        match (&l.ty, &r.ty) {
            (CTy::Bool, CTy::Bool) if op == "==" || op == "!=" => Ok(both(&l, &r)),
            (CTy::Bool, _) | (_, CTy::Bool) => keep(line, "a truth value compared with a number"),
            (CTy::Float(_), _) | (_, CTy::Float(_)) => {
                let double = matches!(l.ty, CTy::Float(false)) || matches!(r.ty, CTy::Float(false));
                if !matches!(l.ty, CTy::Int(_) | CTy::Float(_)) || !matches!(r.ty, CTy::Int(_) | CTy::Float(_)) {
                    return keep(line, "a number compared with a value that is not one");
                }
                if !double && (is_u64(&l.ty) || is_u64(&r.ty)) {
                    return keep(line, "a 64-bit unsigned number beside a `float` of 32 bits");
                }
                let l = self.float_side(l, double);
                let r = self.float_side(r, double);
                Ok(both(&l, &r))
            }
            (CTy::Int(a), CTy::Int(b)) => {
                let ty = usual(*a, *b);
                let (l, r) = self.int_sides(l, r, ty, line)?;
                Ok(both(&l, &r))
            }
            _ => keep(line, "a comparison of pointers, or of texts with `==`, which compares where they are"),
        }
    }

    /// Both sides of a comparison or a division made the exact values C
    /// compares, in the type `ty`.
    fn int_sides(&mut self, l: Val, r: Val, ty: IntTy, line: usize) -> Kept<(Val, Val)> {
        if ty.signed {
            return Ok((self.exact(l), self.exact(r)));
        }
        if ty.is_u64() {
            let (l, r) = (self.to_u64(l), self.to_u64(r));
            return Ok((self.exact(l), self.exact(r)));
        }
        if ty.bits == 64 {
            for side in [&l, &r] {
                if side.int().is_some_and(|int| int.signed) && !side.nonneg {
                    return keep(line, "a number that may be below zero beside a 64-bit unsigned one, which C would make a very large number");
                }
            }
            return Ok((self.exact(l), self.exact(r)));
        }
        let l = self.as_unsigned(l, ty);
        let r = self.as_unsigned(r, ty);
        Ok((self.exact(l), self.exact(r)))
    }

    /// A number beside a float: as the float type C works in.
    fn float_side(&mut self, v: Val, double: bool) -> Val {
        match (&v.ty, double) {
            (CTy::Float(true), true) => {
                let inner = if v.sized { v.code.clone() } else { format!("f32({})", v.code) };
                Val { sized: false, single_literal: false, ..Val::new(format!("float({inner})"), ATOM, CTy::Float(false)) }
            }
            (CTy::Int(int), _) if int.is_u64() => {
                let v = self.exact(v);
                Val::new(format!("float({})", v.code), ATOM, CTy::Float(false))
            }
            (CTy::Int(_), _) => self.exact(v),
            _ => v,
        }
    }

    pub fn arith(&mut self, op: &'static str, l: Val, r: Val, line: usize) -> Kept<Val> {
        if l.is_bool() || r.is_bool() {
            return keep(line, "the value of a comparison, or a bool, used as a number");
        }
        let numeric = |v: &Val| matches!(v.ty, CTy::Int(_) | CTy::Float(_));
        if !numeric(&l) || !numeric(&r) {
            return keep(line, format!("`{op}` of a value that is not a number: arithmetic on a pointer, a text or a struct"));
        }
        if matches!(l.ty, CTy::Float(_)) || matches!(r.ty, CTy::Float(_)) {
            return self.float_arith(op, l, r, line);
        }
        let (a, b) = (l.int().unwrap(), r.int().unwrap());
        let shift = op == "<<" || op == ">>";
        let ty = if shift { a.promoted() } else { usual(a, b) };
        let zop = if op == "/" { "//" } else { op };
        let prec = prec_of(op);
        if ty.signed {
            let l = self.exact(l);
            let r = self.exact(r);
            let value = fold(op, l.value, r.value);
            let both_nonneg = l.nonneg && r.nonneg;
            if (op == "/" || op == "%") && !both_nonneg {
                let helper = if op == "/" { Helper::Divide } else { Helper::Remainder };
                self.helper(helper);
                return Ok(Val { value, ..Val::new(format!("{}({}, {})", helper.name(), l.code, r.code), ATOM, CTy::Int(ty)) });
            }
            let nonneg = match op {
                "+" | "*" | "/" | "%" | "|" | "^" => both_nonneg,
                "&" => l.nonneg || r.nonneg,
                ">>" => l.nonneg,
                "<<" => l.nonneg,
                _ => false,
            } || value.is_some_and(|value| value >= 0);
            let code = format!("{} {zop} {}", paren(&l, prec), paren(&r, prec + 1));
            return Ok(Val { value, nonneg, ..Val::new(code, prec, CTy::Int(ty)) });
        }
        if ty.is_u64() {
            return Ok(self.arith_u64(op, l, r));
        }
        if ty.bits == 64 {
            if op == "-" {
                return keep(line, "`-` of `size_t`s, which go round at 2**64 in C; Zeno's `int` does not");
            }
            let (l, r) = if shift { (self.exact(l), self.exact(r)) } else { self.int_sides(l, r, ty, line)? };
            let code = format!("{} {zop} {}", paren(&l, prec), paren(&r, prec + 1));
            return Ok(Val { nonneg: true, value: fold(op, l.value, r.value), ..Val::new(code, prec, CTy::Int(ty)) });
        }
        // Unsigned, of 32 bits.
        match op {
            "/" | "%" | ">>" => {
                let l = self.as_unsigned(l, ty);
                let l = self.exact(l);
                let r = if shift { self.exact(r) } else { let r = self.as_unsigned(r, ty); self.exact(r) };
                let code = format!("{} {zop} {}", paren(&l, prec), paren(&r, prec + 1));
                Ok(Val { nonneg: true, value: fold(op, l.value, r.value), ..Val::new(code, prec, CTy::Int(ty)) })
            }
            "<<" => {
                let r = self.exact(r);
                let count = r.value.filter(|count| (0..32).contains(count));
                let mut l = self.as_unsigned(l, ty);
                let mut bits = bits_of(&l) + count.map(|count| count as u32).unwrap_or(31);
                if bits > 62 {
                    l = self.exact(l);
                    bits = bits_of(&l) + count.map(|count| count as u32).unwrap_or(31);
                }
                if bits > 62 {
                    let Some(count) = count else {
                        return keep(line, "`<<` of an `unsigned int` by an amount that is not written out, which may not fit in 64 bits before it is cut to 32");
                    };
                    // Only the bits that stay are shifted.
                    let mask = (1u64 << (32 - count)) - 1;
                    let code = format!("({} & {mask:#x}) << {}", paren(&l, BAND), r.code);
                    return Ok(Val { nonneg: true, exact: true, ..Val::new(code, SHIFT, CTy::Int(ty)) });
                }
                let code = format!("{} << {}", paren(&l, SHIFT), paren(&r, SHIFT + 1));
                Ok(Val { exact: false, bits, ..Val::new(code, SHIFT, CTy::Int(ty)) })
            }
            _ => {
                let mut l = self.as_unsigned(l, ty);
                let mut r = self.as_unsigned(r, ty);
                let need = |l: &Val, r: &Val| match op {
                    "+" | "-" => bits_of(l).max(bits_of(r)) + 1,
                    "*" => bits_of(l) + bits_of(r),
                    _ => bits_of(l).max(bits_of(r)),
                };
                if need(&l, &r) > 62 {
                    l = self.exact(l);
                    r = self.exact(r);
                }
                if need(&l, &r) > 62 {
                    self.helper(Helper::TimesU32);
                    return Ok(Val { nonneg: true, sized: true, ..Val::new(format!("times_u32({}, {})", l.code, r.code), ATOM, CTy::Int(ty)) });
                }
                let bits = need(&l, &r);
                let exact = matches!(op, "&" | "|" | "^") && l.exact && r.exact;
                let code = format!("{} {zop} {}", paren(&l, prec), paren(&r, prec + 1));
                Ok(Val { exact, bits, nonneg: exact, value: if exact { fold(op, l.value, r.value) } else { None }, ..Val::new(code, prec, CTy::Int(ty)) })
            }
        }
    }

    fn float_arith(&mut self, op: &'static str, l: Val, r: Val, line: usize) -> Kept<Val> {
        if !matches!(op, "+" | "-" | "*" | "/") {
            return keep(line, format!("`{op}` of a float"));
        }
        let double = matches!(l.ty, CTy::Float(false)) || matches!(r.ty, CTy::Float(false));
        let prec = prec_of(op);
        if !double && (is_u64(&l.ty) || is_u64(&r.ty)) {
            return keep(line, "a 64-bit unsigned number in a sum of `float`s of 32 bits");
        }
        if double {
            let l = self.float_side(l, true);
            let r = self.float_side(r, true);
            return Ok(Val::new(format!("{} {op} {}", paren(&l, prec), paren(&r, prec + 1)), prec, CTy::Float(false)));
        }
        // Sums of `float`s are done in 32 bits: one side at least must be
        // an f32 in Zeno for that.
        let mut l = if matches!(l.ty, CTy::Int(_)) { self.exact(l) } else { l };
        let r = if matches!(r.ty, CTy::Int(_)) { self.exact(r) } else { r };
        if !l.sized && !r.sized {
            l = Val { sized: true, single_literal: false, ..Val::new(format!("f32({})", l.code), ATOM, CTy::Float(true)) };
        }
        Ok(Val { sized: true, ..Val::new(format!("{} {op} {}", paren(&l, prec), paren(&r, prec + 1)), prec, CTy::Float(true)) })
    }

    fn index(&mut self, base: &Expr, index: &Expr, line: usize) -> Kept<Val> {
        let at = self.expr(index)?;
        let Some(at_ty) = at.int() else {
            return keep(line, "an index that is not a whole number");
        };
        let at = if at_ty.signed {
            self.exact(at)
        } else if at_ty.is_u64() {
            let at = self.exact(at);
            Val::new(format!("int({})", at.code), ATOM, CTy::Int(IntTy::LONG))
        } else {
            let at = self.as_unsigned(at, at_ty.promoted());
            self.exact(at)
        };
        // `text[i]` of a text is the code of a letter.
        if let E::Name(name) = &base.kind {
            if let Some(local) = self.lookup(name).cloned() {
                if local.kind == Kind::Box || local.kind == Kind::Array {
                    let item = pointee(&local.ty);
                    return Ok(Val { place: true, nonneg: is_unsigned(&item), sized: is_sized(&item), ..Val::new(format!("{}[{}]", local.zeno, at.code), ATOM, item) });
                }
            }
        }
        let b = self.expr(base)?;
        if b.ty.is_text() {
            self.helper(Helper::LetterCode);
            return Ok(Val { nonneg: true, ..Val::new(format!("letter_code({}, {})", b.code, at.code), ATOM, CTy::Int(IntTy::CHAR)) });
        }
        match &b.ty {
            CTy::Array(item, _) => {
                let item = (**item).clone();
                Ok(Val { place: true, nonneg: is_unsigned(&item), sized: is_sized(&item), ..Val::new(format!("{}[{}]", paren(&b, ATOM), at.code), ATOM, item) })
            }
            _ => keep(line, "`[ ]` of a value that is not an array"),
        }
    }

    fn member(&mut self, base: &Expr, field: &str, arrow: bool, line: usize) -> Kept<Val> {
        let b = if arrow {
            match &base.kind {
                E::Name(name) if self.lookup(name).is_some_and(|local| local.kind == Kind::Ref) => {
                    let local = self.lookup(name).cloned().unwrap();
                    Val { place: true, ..Val::new(local.zeno, ATOM, pointee(&local.ty)) }
                }
                _ => return keep(line, "`->` of a pointer that is not a parameter the function only reads fields through"),
            }
        } else {
            self.expr(base)?
        };
        let CTy::Struct(name) = &b.ty else {
            return keep(line, format!("`.{field}` of a value that is not a struct"));
        };
        let Some(fields) = self.world.names.structs.get(name) else {
            return keep(line, format!("the struct `{name}`, whose fields the converter did not read"));
        };
        let Some(found) = fields.iter().find(|param| param.name == field) else {
            return keep(line, format!("the struct `{name}` has no field `{field}`"));
        };
        let ty = found.ty.clone();
        Ok(Val { place: true, nonneg: is_unsigned(&ty), sized: is_sized(&ty), ..Val::new(format!("{}.{}", paren(&b, ATOM), self.world.zeno_name(field)), ATOM, ty) })
    }

    fn cast(&mut self, ty: &CTy, operand: &Expr, line: usize) -> Kept<Val> {
        let v = self.expr(operand)?;
        match ty {
            CTy::Float(_) | CTy::Bool => {
                let code = self.store(v, ty, line)?;
                Ok(Val { sized: is_sized(ty), ..Val::new(code, ATOM, ty.clone()) })
            }
            CTy::Int(to) => {
                let to = *to;
                if v.is_bool() {
                    return keep(line, "a truth value made a whole number");
                }
                if !to.signed && to.bits <= 32 {
                    // C converts to an unsigned type by keeping the low bits:
                    // the value need not be exact until it is looked at.
                    if let CTy::Int(from) = v.ty {
                        if to.bits == 32 && !from.is_u64() {
                            return Ok(self.as_unsigned(v, to));
                        }
                    }
                }
                let code = self.store(v, ty, line)?;
                Ok(Val { sized: is_sized(ty), nonneg: !to.signed, ..Val::new(code, ATOM, ty.clone()) })
            }
            CTy::Void => keep(line, "`(void)` inside an expression"),
            _ => keep(line, format!("a cast to `{}`", ty.c_name())),
        }
    }

    /// `v` written as a value of type `to`, as C converts it when it
    /// stores one (C 6.3.1).
    pub fn store(&mut self, v: Val, to: &CTy, line: usize) -> Kept<String> {
        Ok(self.store_val(v, to, line)?.code)
    }

    fn store_val(&mut self, v: Val, to: &CTy, line: usize) -> Kept<Val> {
        match to {
            CTy::Int(t) => {
                let t = *t;
                let v = match &v.ty {
                    CTy::Bool => return keep(line, "a truth value stored as a whole number"),
                    CTy::Float(_) => {
                        let inner = if matches!(v.ty, CTy::Float(true)) && !v.sized { format!("f32({})", v.code) } else { v.code.clone() };
                        Val::new(format!("int({inner})"), ATOM, CTy::Int(IntTy::LONG))
                    }
                    CTy::Int(_) => v,
                    _ => return keep(line, format!("a value that is not a number stored in a `{}`", to.c_name())),
                };
                let from = v.int().unwrap();
                let fits_value = v.value.is_some_and(|value| value >= t.min() as i128 && value <= t.max() as i128);
                if !t.signed && t.bits <= 32 && from.is_u64() {
                    // A u64 is cut to the low bits first: Zeno's `u32.wrap`
                    // takes an int, and a u64 may be past `int.max`.
                    let v = self.exact(v);
                    let mask = t.max();
                    return Ok(Val { exact: true, sized: true, nonneg: true, ..Val::new(format!("u{}(int({} & {mask:#x}))", t.bits, paren(&v, BAND + 1)), ATOM, to.clone()) });
                }
                if !t.signed && t.bits <= 32 {
                    let width = format!("u{}", t.bits);
                    let same = !from.signed && from.bits == t.bits;
                    if same && v.exact && v.sized {
                        return Ok(v);
                    }
                    if v.literal && fits_value {
                        return Ok(Val { sized: true, ..v });
                    }
                    let fits = v.exact && (fits_value || (!from.signed && from.bits <= t.bits) || (v.nonneg && from.max() <= t.max()));
                    let code = if fits { format!("{width}({})", v.code) } else { format!("{width}.wrap({})", v.code) };
                    return Ok(Val { exact: true, sized: true, nonneg: true, ..Val::new(code, ATOM, to.clone()) });
                }
                if t.is_u64() {
                    let v = self.to_u64(v);
                    let v = self.exact(v);
                    return Ok(Val { ty: to.clone(), sized: true, nonneg: true, ..v });
                }
                let v = self.exact(v);
                // A u64 stored in an int of Zeno: `int( )` stops the program
                // past `int.max`, where C would keep the low bits.
                let (v, from) = if from.is_u64() {
                    (Val { nonneg: true, ..Val::new(format!("int({})", v.code), ATOM, CTy::Int(IntTy::LONG)) }, IntTy::LONG)
                } else {
                    (v, from)
                };
                if !t.signed {
                    // 64 bits, which Zeno holds in an int.
                    if from.signed && !v.nonneg && !fits_value {
                        return keep(line, format!("a number that may be below zero stored in a `{}`, where C would make it a very large one", to.c_name()));
                    }
                    return Ok(Val { ty: to.clone(), nonneg: true, sized: false, ..v });
                }
                let fits = fits_value || (from.signed && from.bits <= t.bits) || (!from.signed && from.bits < t.bits);
                if fits || t.bits == 64 {
                    return Ok(Val { ty: to.clone(), sized: false, ..v });
                }
                self.note(Note::Narrow, line);
                Ok(Val { sized: false, ..Val::new(format!("i{}({})", t.bits, v.code), ATOM, to.clone()) })
            }
            CTy::Float(single) => {
                let single = *single;
                match &v.ty {
                    CTy::Int(from) if from.is_u64() => {
                        if single {
                            return keep(line, "a 64-bit unsigned number stored in a `float` of 32 bits");
                        }
                        let v = self.exact(v);
                        Ok(Val::new(format!("float({})", v.code), ATOM, to.clone()))
                    }
                    CTy::Int(_) => {
                        let v = self.exact(v);
                        if v.literal && !v.code.starts_with("0x") && !v.code.starts_with("0o") && !v.code.starts_with("0b") {
                            return Ok(Val { sized: single, ..Val::new(format!("{}.0", v.code), ATOM, to.clone()) });
                        }
                        let word = if single { "f32" } else { "float" };
                        Ok(Val { sized: single, ..Val::new(format!("{word}({})", v.code), ATOM, to.clone()) })
                    }
                    CTy::Float(true) if single => {
                        if v.sized || v.single_literal {
                            Ok(Val { sized: true, ..v })
                        } else {
                            Ok(Val { sized: true, ..Val::new(format!("f32({})", v.code), ATOM, to.clone()) })
                        }
                    }
                    CTy::Float(true) => Ok(self.float_side(v, true)),
                    CTy::Float(false) if single => Ok(Val { sized: true, ..Val::new(format!("f32({})", v.code), ATOM, to.clone()) }),
                    CTy::Float(false) => Ok(v),
                    _ => keep(line, format!("a value that is not a number stored in a `{}`", to.c_name())),
                }
            }
            CTy::Bool => match &v.ty {
                CTy::Bool => Ok(v),
                CTy::Int(_) | CTy::Float(_) => self.test(v, line),
                _ => keep(line, "a pointer stored in a `bool`"),
            },
            CTy::Struct(name) => match &v.ty {
                CTy::Struct(found) if found == name => {
                    if v.place {
                        self.copied.insert(name.clone());
                        Ok(Val { place: false, ..Val::new(format!("{}.copy()", paren(&v, ATOM)), ATOM, v.ty.clone()) })
                    } else {
                        Ok(v)
                    }
                }
                _ => keep(line, format!("a value stored in a `struct {name}` that is not one")),
            },
            ty if ty.is_text() => {
                if v.ty.is_text() {
                    Ok(v)
                } else {
                    keep(line, "a value stored in a `char *` that is not a text")
                }
            }
            _ => keep(line, format!("a value of type `{}`", to.c_name())),
        }
    }

    fn call(&mut self, name: &str, args: &[Expr], line: usize) -> Kept<Val> {
        if let Some(found) = self.library(name, args, line) {
            return found;
        }
        let world = self.world;
        if world.macros.contains_key(name) {
            return keep(line, format!("`{name}`, a macro of C, which Zeno does not have"));
        }
        let Some(info) = world.functions.get(name) else {
            return keep(line, format!("`{name}`, which is not a function of this file, and not one of C's library that the converter writes"));
        };
        if !info.read {
            return keep(line, format!("it calls `{name}`, which the converter could not read"));
        }
        if args.len() != info.params.len() {
            return keep(line, format!("it calls `{name}` with {} arguments, and it takes {}", args.len(), info.params.len()));
        }
        let kept = world.kept.contains(&info.unit);
        let mut codes = Vec::new();
        let mut boxed: Vec<String> = Vec::new();
        for (at, ((arg, param), kind)) in args.iter().zip(&info.params).zip(&info.kinds).enumerate() {
            let code = match kind {
                Kind::Value => {
                    if kept && matches!(param.ty, CTy::Struct(_)) {
                        return keep(line, format!("it gives a struct to `{name}`, which is kept as C"));
                    }
                    let v = self.expr(arg)?;
                    match (&param.ty, &v.ty) {
                        (CTy::Struct(want), CTy::Struct(found)) if want == found && !info.changes[at] => v.code,
                        _ => self.store(v, &param.ty, arg.line)?,
                    }
                }
                Kind::Text => {
                    let v = self.expr(arg)?;
                    if !v.ty.is_text() {
                        return keep(line, format!("it gives `{name}` a value that is not a text"));
                    }
                    if kept && !matches!(param.ty, CTy::Ptr(_, true)) {
                        return keep(line, format!("it gives a text to `{name}`, which is kept as C and may change it (`char *`, not `const char *`)"));
                    }
                    v.code
                }
                Kind::Array => {
                    let item = pointee(&param.ty);
                    if kept && !matches!(item, CTy::Int(_) | CTy::Float(_) | CTy::Bool) {
                        return keep(line, format!("it gives an array of `{}` to `{name}`, which is kept as C", item.c_name()));
                    }
                    let list = self.list_argument(arg, &item, name)?;
                    if boxed.iter().any(|other| other.split(['[', '.']).next() == Some(list.as_str())) {
                        return keep(line, format!("it gives `{name}` both `{list}` and a place in it"));
                    }
                    boxed.push(list.clone());
                    list
                }
                Kind::Box => {
                    let item = pointee(&param.ty);
                    match &arg.kind {
                        E::Unary("&", inner) if !matches!(inner.kind, E::Name(_)) => {
                            // The address of an item or a field: carried there
                            // and back like a variable's value.
                            if has_call(inner) {
                                return keep(line, format!("it gives `{name}` the address of a place with a call in it"));
                            }
                            let v = self.expr(inner)?;
                            if !v.place || v.ty != item {
                                return keep(line, format!("it gives `{name}` the address of something that is not a `{}`", item.c_name()));
                            }
                            let whole = v.code.split(['[', '.']).next().unwrap_or("").to_string();
                            if boxed.iter().any(|other| other.split(['[', '.']).next() == Some(whole.as_str())) {
                                return keep(line, format!("it gives `{name}` two places in `{whole}`, which may be the same place"));
                            }
                            boxed.push(v.code.clone());
                            let carrier = self.fresh(&format!("{}_box", whole.trim_start_matches(|c: char| !c.is_alphanumeric())));
                            let annotation = if is_sized(&item) { format!(": list[{}]", zeno_type(&item, line)?) } else { String::new() };
                            self.pre.push(format!("let {carrier}{annotation} = [{}]", v.code));
                            self.post.push(format!("{} = {carrier}[0]", v.code));
                            carrier
                        }
                        E::Unary("&", inner) => {
                            let E::Name(var) = &inner.kind else { unreachable!() };
                            let (zeno, ty) = self.variable(var, line)?;
                            if ty != item {
                                return keep(line, format!("it gives `{name}` the address of a `{}` where it takes a `{}`", ty.c_name(), param.ty.c_name()));
                            }
                            if boxed.contains(&zeno) {
                                return keep(line, format!("it gives `{name}` the address of `{var}` twice"));
                            }
                            if info.globals.contains(var) {
                                return keep(line, format!("it gives `{name}` the address of `{var}`, which `{name}` also uses by its name"));
                            }
                            boxed.push(zeno.clone());
                            let carrier = self.fresh(&format!("{zeno}_box"));
                            let annotation = if is_sized(&item) { format!(": list[{}]", zeno_type(&item, line)?) } else { String::new() };
                            self.pre.push(format!("let {carrier}{annotation} = [{zeno}]"));
                            self.post.push(format!("{zeno} = {carrier}[0]"));
                            carrier
                        }
                        _ => self.list_argument(arg, &item, name)?,
                    }
                }
                Kind::Ref => {
                    if kept {
                        return keep(line, format!("it gives a struct to `{name}`, which is kept as C"));
                    }
                    let want = pointee(&param.ty);
                    match &arg.kind {
                        E::Unary("&", inner) => {
                            let v = self.expr(inner)?;
                            if v.ty != want || !v.place {
                                return keep(line, format!("it gives `{name}` the address of something that is not a `{}`", want.c_name()));
                            }
                            v.code
                        }
                        E::Name(other) if self.lookup(other).is_some_and(|local| local.kind == Kind::Ref && pointee(&local.ty) == want) => self.lookup(other).unwrap().zeno.clone(),
                        _ => return keep(line, format!("it gives `{name}` a pointer the converter cannot follow")),
                    }
                }
            };
            codes.push(code);
        }
        let callee = if kept { format!("{}.{name}", world.block) } else { world.zeno_name(name) };
        let code = format!("{callee}({})", codes.join(", "));
        let ret = info.ret.clone();
        if kept {
            return match &ret {
                CTy::Int(_) => Ok(Val { nonneg: is_unsigned(&ret), ..Val::new(code, ATOM, ret) }),
                CTy::Float(true) => Ok(Val { sized: true, ..Val::new(format!("f32({code})"), ATOM, ret) }),
                CTy::Float(false) | CTy::Bool | CTy::Void => Ok(Val::new(code, ATOM, ret)),
                _ => keep(line, format!("`{name}` is kept as C, and gives back a `{}`", ret.c_name())),
            };
        }
        Ok(Val { nonneg: is_unsigned(&ret), sized: is_sized(&ret), ..Val::new(code, ATOM, ret) })
    }

    /// An array given where a function takes a pointer to its items.
    fn list_argument(&mut self, arg: &Expr, item: &CTy, callee: &str) -> Kept<String> {
        let line = arg.line;
        if let E::Name(name) = &arg.kind {
            if let Some(local) = self.lookup(name).cloned() {
                if matches!(local.kind, Kind::Array | Kind::Box) && pointee(&local.ty) == *item {
                    return Ok(local.zeno);
                }
            } else if let Some(global) = self.world.globals.get(name) {
                if !self.world.kept.contains(&global.unit) && matches!(&global.ty, CTy::Array(found, _) if **found == *item) {
                    return Ok(self.world.zeno_name(name));
                }
            }
        }
        keep(line, format!("it gives `{callee}` a pointer that is not an array of `{}` it can see", item.c_name()))
    }

    /// A variable that a statement gives a value: its name in Zeno and
    /// its type.
    fn variable(&mut self, name: &str, line: usize) -> Kept<(String, CTy)> {
        if let Some(local) = self.lookup(name).cloned() {
            if local.kind == Kind::Value {
                return Ok((local.zeno, local.ty));
            }
            return keep(line, format!("it changes where the pointer `{name}` points"));
        }
        if let Some(global) = self.world.globals.get(name) {
            if self.world.kept.contains(&global.unit) {
                return keep(line, format!("it changes `{name}`, a variable that is kept as C"));
            }
            return Ok((self.world.zeno_name(name), global.ty.clone()));
        }
        keep(line, format!("`{name}`, which is not declared in this file"))
    }

    /// The functions of C's own library that the converter writes.
    fn library(&mut self, name: &str, args: &[Expr], line: usize) -> Option<Kept<Val>> {
        let defined = self.world.functions.contains_key(name);
        if defined {
            return None;
        }
        let one = |w: &mut W, args: &[Expr]| -> Kept<Val> {
            if args.len() != 1 {
                return keep(line, format!("`{name}` with {} arguments", args.len()));
            }
            w.expr(&args[0])
        };
        let double = |w: &mut W, v: Val| -> Kept<Val> {
            match &v.ty {
                CTy::Float(_) => Ok(w.float_side(v, true)),
                CTy::Int(_) => {
                    let v = w.exact(v);
                    Ok(Val::new(format!("float({})", v.code), ATOM, CTy::Float(false)))
                }
                _ => keep(line, format!("`{name}` of a value that is not a number")),
            }
        };
        let result = (|| -> Kept<Val> {
            match name {
                "sqrt" | "sin" | "cos" | "tan" | "exp" | "log" | "log10" | "log2" => {
                    let v = one(self, args)?;
                    let v = double(self, v)?;
                    self.imports.insert("math");
                    Ok(Val::new(format!("math.{name}({})", v.code), ATOM, CTy::Float(false)))
                }
                "floor" | "ceil" | "round" => {
                    let v = one(self, args)?;
                    let v = double(self, v)?;
                    self.imports.insert("math");
                    Ok(Val::new(format!("float(math.{name}({}))", v.code), ATOM, CTy::Float(false)))
                }
                "trunc" => {
                    let v = one(self, args)?;
                    let v = double(self, v)?;
                    Ok(Val::new(format!("float(int({}))", v.code), ATOM, CTy::Float(false)))
                }
                "fabs" => {
                    let v = one(self, args)?;
                    let v = double(self, v)?;
                    Ok(Val::new(format!("abs({})", v.code), ATOM, CTy::Float(false)))
                }
                "pow" => {
                    if args.len() != 2 {
                        return keep(line, "`pow` with other than two arguments");
                    }
                    let a = self.expr(&args[0])?;
                    let a = double(self, a)?;
                    let b = self.expr(&args[1])?;
                    let b = double(self, b)?;
                    Ok(Val::new(format!("{} ** {}", paren(&a, POW + 1), paren(&b, POW)), POW, CTy::Float(false)))
                }
                "abs" | "labs" | "llabs" => {
                    let v = one(self, args)?;
                    let Some(int) = v.int() else {
                        return keep(line, format!("`{name}` of a value that is not a whole number"));
                    };
                    let v = self.exact(v);
                    Ok(Val { nonneg: true, ..Val::new(format!("abs({})", v.code), ATOM, CTy::Int(int.promoted())) })
                }
                "strlen" => {
                    let v = one(self, args)?;
                    if !v.ty.is_text() {
                        return keep(line, "`strlen` of a value that is not a text");
                    }
                    Ok(Val { nonneg: true, ..Val::new(format!("len({})", v.code), ATOM, CTy::Int(IntTy::SIZE)) })
                }
                "isdigit" | "isalpha" | "isalnum" | "isspace" | "isupper" | "islower" | "toupper" | "tolower" => {
                    let v = one(self, args)?;
                    if v.int().is_none() {
                        return keep(line, format!("`{name}` of a value that is not a whole number"));
                    }
                    let v = self.exact(v);
                    let helper = match name {
                        "isdigit" => Helper::IsDigit,
                        "isalpha" => Helper::IsAlpha,
                        "isalnum" => Helper::IsAlnum,
                        "isspace" => Helper::IsSpace,
                        "isupper" => Helper::IsUpper,
                        "islower" => Helper::IsLower,
                        "toupper" => Helper::ToUpper,
                        _ => Helper::ToLower,
                    };
                    self.helper(helper);
                    // A letter changed is still a letter: what was a `char` fits one.
                    let small = v.int().is_some_and(|int| int.bits == 8 && int.signed) || v.value.is_some_and(|value| (0..128).contains(&value));
                    let ty = match (name.starts_with("to"), small) {
                        (true, true) => CTy::Int(IntTy::CHAR),
                        (true, false) => CTy::Int(IntTy::INT),
                        _ => CTy::Bool,
                    };
                    Ok(Val::new(format!("{name}({})", v.code), ATOM, ty))
                }
                "printf" | "puts" | "putchar" | "fprintf" | "fputs" | "exit" => keep(line, format!("the value that `{name}` gives back")),
                _ => Err(Keep { line: 0, why: String::new() }),
            }
        })();
        match result {
            Err(keep) if keep.line == 0 => None,
            other => Some(other),
        }
    }

    // --------------------------------------------------------- Statements

    fn flush_pre(&mut self) {
        for line in std::mem::take(&mut self.pre) {
            self.line(line);
        }
    }

    fn flush_post(&mut self) {
        for line in std::mem::take(&mut self.post) {
            self.line(line);
        }
    }

    /// A value in a place where nothing can be written before or after
    /// it: a test of an `if`, of a loop.
    fn alone(&mut self, line: usize) -> Kept<()> {
        if !self.pre.is_empty() || !self.post.is_empty() {
            self.pre.clear();
            self.post.clear();
            return keep(line, "a call that changes a variable through a pointer, in a test");
        }
        Ok(())
    }

    fn statements(&mut self, body: &[Stmt]) -> Kept<()> {
        for stmt in body {
            self.statement(stmt)?;
        }
        Ok(())
    }

    /// The body of an `if` or a loop, one step in.
    fn body(&mut self, stmt: &Stmt) -> Kept<()> {
        self.depth += 1;
        self.scopes.push(HashMap::new());
        let done = match &stmt.kind {
            S::Block(body) => self.statements(body),
            _ => self.statement(stmt),
        };
        self.scopes.pop();
        self.depth -= 1;
        done
    }

    pub fn statement(&mut self, stmt: &Stmt) -> Kept<()> {
        self.one_statement(stmt)?;
        // A variable given to a call through a pointer has its value put
        // back before the next statement, whatever this one was.
        self.flush_post();
        Ok(())
    }

    fn one_statement(&mut self, stmt: &Stmt) -> Kept<()> {
        let line = stmt.line;
        self.comments_before(line);
        match &stmt.kind {
            S::Decl(vars) => {
                for var in vars {
                    self.declaration(var)?;
                }
            }
            S::Expr(expr) => self.expression_statement(expr)?,
            S::If(test, then, other) => {
                let test = self.cond(test)?;
                // A test that carries a variable to a function and back is
                // worked out on the lines before.
                let code = if self.pre.is_empty() && self.post.is_empty() {
                    test.code
                } else {
                    let name = self.fresh("tested");
                    self.flush_pre();
                    self.line(format!("let {name} = {}", test.code));
                    self.flush_post();
                    name
                };
                self.line(format!("if {code} {{"));
                self.body(then)?;
                let mut other = other.as_deref();
                while let Some(next) = other {
                    if let S::If(test, then, rest) = &next.kind {
                        let test = self.cond(test)?;
                        self.alone(next.line)?;
                        self.line(format!("}} else if {} {{", test.code));
                        self.body(then)?;
                        other = rest.as_deref();
                    } else {
                        self.line("} else {");
                        self.body(next)?;
                        other = None;
                    }
                }
                self.line("}");
            }
            S::While(test, body) => {
                let test = self.cond(test)?;
                if self.pre.is_empty() && self.post.is_empty() {
                    self.line(format!("while {} {{", test.code));
                } else {
                    // The test carries a variable to a function and back: it is
                    // worked out at the top of the loop, each time round.
                    let name = self.fresh("tested");
                    self.line("while true {");
                    self.depth += 1;
                    self.flush_pre();
                    self.line(format!("let {name} = {}", test.code));
                    self.flush_post();
                    self.line(format!("if not {name} {{"));
                    self.line("    break");
                    self.line("}");
                    self.depth -= 1;
                }
                self.breakables.push(Breakable::Loop(true));
                let done = self.body(body);
                self.breakables.pop();
                done?;
                self.line("}");
            }
            S::Do(body, test) => {
                if continues(body) {
                    return keep(line, "`continue` inside `do ... while`, which would pass by its test in Zeno");
                }
                self.line("while true {");
                self.breakables.push(Breakable::Loop(true));
                let done = self.body(body);
                self.breakables.pop();
                done?;
                let test = self.cond(test)?;
                self.alone(line)?;
                self.depth += 1;
                self.line(format!("if not {} {{", paren(&test, NOT + 1)));
                self.line("    break");
                self.line("}");
                self.depth -= 1;
                self.line("}");
            }
            S::For(init, test, step, body) => self.for_loop(init.as_deref(), test.as_ref(), step.as_ref(), body, line)?,
            S::Switch(value, body) => self.switch(value, body, line)?,
            S::Case(_) | S::Default => return keep(line, "a `case` that the converter does not follow"),
            S::Break => match self.breakables.last() {
                Some(Breakable::Loop(_)) => self.line("break"),
                Some(Breakable::Switch) => return keep(line, "a `break` out of a `switch` from inside an `if`, which Zeno writes otherwise"),
                None => return keep(line, "a `break` outside a loop"),
            },
            S::Continue => match self.breakables.iter().rev().find(|breakable| matches!(breakable, Breakable::Loop(_))) {
                Some(Breakable::Loop(true)) => self.line("continue"),
                Some(_) => return keep(line, "`continue` in a `for` loop that Zeno writes as `while`, where it would pass by the step"),
                None => return keep(line, "a `continue` outside a loop"),
            },
            S::Return(value) => self.return_statement(value.as_ref(), line)?,
            S::Block(body) => self.statements(body)?,
            S::Empty => {}
        }
        Ok(())
    }

    fn return_statement(&mut self, value: Option<&Expr>, line: usize) -> Kept<()> {
        if self.main_void || self.ret == CTy::Void {
            if self.ret == CTy::Void && value.is_some() {
                return keep(line, "a `return` with a value in a function that gives none");
            }
            self.line("return");
            return Ok(());
        }
        let Some(value) = value else {
            return keep(line, "a `return` without the value the function gives");
        };
        if let E::Cond(test, yes, no) = &value.kind {
            let test = self.cond(test)?;
            self.alone(line)?;
            let ret = self.ret.clone();
            let yes_v = self.expr(yes)?;
            let yes = self.returned(yes_v, &ret, line)?;
            let no_v = self.expr(no)?;
            let no = self.returned(no_v, &ret, line)?;
            self.alone(line)?;
            self.line(format!("if {} {{", test.code));
            self.line(format!("    return {yes}"));
            self.line("}");
            self.line(format!("return {no}"));
            return Ok(());
        }
        let v = self.expr(value)?;
        let ret = self.ret.clone();
        let code = self.returned(v, &ret, line)?;
        if !self.post.is_empty() {
            self.pre.clear();
            self.post.clear();
            return keep(line, "a `return` of a call that changes a variable through a pointer");
        }
        self.flush_pre();
        self.line(format!("return {code}"));
        Ok(())
    }

    /// A value given back: a struct in a local variable is the
    /// function's own, and is given back without a copy.
    fn returned(&mut self, v: Val, ret: &CTy, line: usize) -> Kept<String> {
        if let (CTy::Struct(_), true) = (&v.ty, v.place) {
            let own = self.owned.contains(&v.code) || self.scopes.iter().skip(1).any(|scope| scope.values().any(|local| local.zeno == v.code && local.kind == Kind::Value));
            if own && v.ty == *ret {
                return Ok(v.code);
            }
        }
        self.store(v, ret, line)
    }

    /// A global variable, which some function may change.
    pub fn global(&mut self, var: &Var) -> Kept<()> {
        if !var.constant {
            self.assigned.insert(var.name.clone());
        }
        self.declaration(var)
    }

    fn declaration(&mut self, var: &Var) -> Kept<()> {
        let line = var.line;
        if var.stat {
            return keep(line, format!("`static {}`, a variable inside a function that keeps its value from one call to the next", var.name));
        }
        let zeno = self.world.zeno_name(&var.name);
        let changed = self.assigned.contains(&var.name);
        let word = if var.constant || !changed { "let" } else { "var" };
        match &var.ty {
            CTy::Array(item, size) => {
                if matches!(**item, CTy::Int(IntTy { char: true, .. })) {
                    return keep(line, format!("`{}`, an array of `char`: a text that C changes in place", var.name));
                }
                let code = match &var.init {
                    Some(Expr { kind: E::List(items), .. }) => self.list_literal(items, item, *size, line)?,
                    Some(_) => return keep(line, "an array given a value that is not `{ }`"),
                    None => zero(&var.ty, &self.world.names, line)?,
                };
                let ty = match (size, &var.init) {
                    (None, Some(Expr { kind: E::List(items), .. })) => CTy::Array(item.clone(), Some(items.len())),
                    _ => var.ty.clone(),
                };
                let annotation = if contains_sized(&ty) { format!(": {}", zeno_type(&ty, line)?) } else { String::new() };
                self.flush_pre();
                self.line(format!("let {zeno}{annotation} = {code}"));
                self.declare(&var.name, Local { zeno, ty, kind: Kind::Array, nonneg: false });
            }
            CTy::Struct(name) => {
                let code = match &var.init {
                    Some(Expr { kind: E::List(items), .. }) => self.construct(name, items, line)?,
                    Some(value) => {
                        let v = self.expr(value)?;
                        self.store(v, &var.ty, line)?
                    }
                    None => zero(&var.ty, &self.world.names, line)?,
                };
                self.flush_pre();
                self.line(format!("{word} {zeno} = {code}"));
                self.flush_post();
                self.declare(&var.name, Local { zeno, ty: var.ty.clone(), kind: Kind::Value, nonneg: false });
            }
            CTy::Ptr(..) if !var.ty.is_text() => return keep(line, format!("`{}`, a pointer variable", var.name)),
            ty => {
                let ty = ty.clone();
                let zty = zeno_type(&ty, line)?;
                if let Some(Expr { kind: E::Cond(test, yes, no), .. }) = &var.init {
                    let test = self.cond(test)?;
                    self.alone(line)?;
                    let yes_v = self.expr(yes)?;
                    let yes = self.store(yes_v, &ty, line)?;
                    let no_v = self.expr(no)?;
                    let no = self.store(no_v, &ty, line)?;
                    self.alone(line)?;
                    self.line(format!("var {zeno}: {zty} = {no}"));
                    self.line(format!("if {} {{", test.code));
                    self.line(format!("    {zeno} = {yes}"));
                    self.line("}");
                } else {
                    let (code, plain) = match &var.init {
                        Some(value) => {
                            let v = self.expr(value)?;
                            let stored = self.store_val(v, &ty, line)?;
                            let plain = stored.literal || (stored.sized == is_sized(&ty) && !matches!(ty, CTy::Int(_)) && !matches!(stored.ty, CTy::Int(_)));
                            (stored.code, plain || (matches!(ty, CTy::Int(IntTy { signed: true, .. }) | CTy::Int(IntTy { bits: 64, .. })) && !stored.sized))
                        }
                        None => (zero(&ty, &self.world.names, line)?, !is_sized(&ty)),
                    };
                    let annotation = if is_sized(&ty) || !plain { format!(": {zty}") } else { String::new() };
                    // A variable declared again in the same block, as two
                    // loops of C each declare their `i`, is given a value.
                    let again = self.scopes.last().and_then(|scope| scope.get(&var.name)).is_some_and(|local| local.ty == ty);
                    self.flush_pre();
                    if again {
                        self.line(format!("{zeno} = {code}"));
                    } else {
                        self.line(format!("{word} {zeno}{annotation} = {code}"));
                    }
                    self.flush_post();
                }
                let kind = if ty.is_text() { Kind::Text } else { Kind::Value };
                self.declare(&var.name, Local { zeno, ty, kind, nonneg: false });
            }
        }
        Ok(())
    }

    fn list_literal(&mut self, items: &[Expr], item: &CTy, size: Option<usize>, line: usize) -> Kept<String> {
        let count = size.unwrap_or(items.len());
        if items.len() > count {
            return keep(line, "more values than the array has room for");
        }
        let all_zero = items.iter().all(|item| matches!(item.kind, E::Int { value: 0, .. }));
        if all_zero && matches!(item, CTy::Int(_) | CTy::Float(_) | CTy::Bool) {
            return zero(&CTy::Array(Box::new(item.clone()), Some(count)), &self.world.names, line);
        }
        let mut codes = Vec::new();
        for value in items {
            let code = match (item, &value.kind) {
                (CTy::Array(inner, size), E::List(inner_items)) => self.list_literal(inner_items, inner, *size, value.line)?,
                (CTy::Struct(name), E::List(fields)) => self.construct(name, fields, value.line)?,
                (CTy::Array(..), _) => return keep(value.line, "an array of arrays given a value that is not `{ }`"),
                _ => {
                    let v = self.expr(value)?;
                    self.store(v, item, value.line)?
                }
            };
            codes.push(code);
        }
        let missing = count - items.len();
        let filler = zero(item, &self.world.names, line)?;
        if missing <= 8 || !matches!(item, CTy::Int(_) | CTy::Float(_) | CTy::Bool) {
            for _ in 0..missing {
                codes.push(filler.clone());
            }
            return Ok(format!("[{}]", codes.join(", ")));
        }
        Ok(format!("[{}] + [{filler}] * {missing}", codes.join(", ")))
    }

    fn construct(&mut self, name: &str, items: &[Expr], line: usize) -> Kept<String> {
        let Some(fields) = self.world.names.structs.get(name).cloned() else {
            return keep(line, format!("the struct `{name}`, whose fields the converter did not read"));
        };
        let mut given: Vec<Option<String>> = vec![None; fields.len()];
        let mut next = 0;
        for item in items {
            let (at, value) = match &item.kind {
                E::Field(field, value) => match fields.iter().position(|param| param.name == *field) {
                    Some(at) => (at, &**value),
                    None => return keep(item.line, format!("the struct `{name}` has no field `{field}`")),
                },
                _ => (next, item),
            };
            if at >= fields.len() {
                return keep(item.line, format!("more values than the struct `{name}` has fields"));
            }
            let field_ty = fields[at].ty.clone();
            let code = match (&field_ty, &value.kind) {
                (CTy::Array(inner, size), E::List(inner_items)) => self.list_literal(inner_items, inner, *size, value.line)?,
                (CTy::Struct(inner), E::List(inner_items)) => self.construct(inner, inner_items, value.line)?,
                _ => {
                    let v = self.expr(value)?;
                    self.store(v, &field_ty, value.line)?
                }
            };
            given[at] = Some(code);
            next = at + 1;
        }
        let mut codes = Vec::new();
        for (at, code) in given.into_iter().enumerate() {
            codes.push(match code {
                Some(code) => code,
                None => zero(&fields[at].ty, &self.world.names, line)?,
            });
        }
        Ok(format!("{}({})", self.world.zeno_name(name), codes.join(", ")))
    }

    fn expression_statement(&mut self, expr: &Expr) -> Kept<()> {
        if let Some((before, plain, after)) = lift_steps(expr) {
            for (name, inc) in before {
                self.step(&name, inc, expr.line)?;
            }
            self.expression_statement(&plain)?;
            for (name, inc) in after {
                self.step(&name, inc, expr.line)?;
            }
            return Ok(());
        }
        let line = expr.line;
        match &expr.kind {
            E::Assign(op, target, value) => self.assign(op, target, value, line),
            E::Pre(inc, target) | E::Post(inc, target) => {
                let one = Expr { kind: E::Int { text: "1".into(), value: 1, ty: IntTy::INT }, line };
                self.assign(if *inc { "+" } else { "-" }, target, &one, line)
            }
            E::Comma(first, second) => {
                self.expression_statement(first)?;
                self.expression_statement(second)
            }
            E::Cast(CTy::Void, inner) => self.expression_statement(inner),
            E::Cond(test, yes, no) => {
                let test = self.cond(test)?;
                self.alone(line)?;
                self.line(format!("if {} {{", test.code));
                self.depth += 1;
                self.expression_statement(yes)?;
                self.depth -= 1;
                self.line("} else {");
                self.depth += 1;
                self.expression_statement(no)?;
                self.depth -= 1;
                self.line("}");
                Ok(())
            }
            E::Call(name, args) if !self.world.functions.contains_key(name) && matches!(name.as_str(), "printf" | "puts" | "putchar" | "fprintf" | "fputs" | "exit") => {
                self.output(name, args, line)
            }
            E::Call(..) => {
                let v = self.expr(expr)?;
                self.flush_pre();
                self.line(v.code);
                self.flush_post();
                Ok(())
            }
            _ => keep(line, "a statement that only works out a value"),
        }
    }

    /// `x += 1` or `x -= 1`, for a `++` or `--` taken out of a statement.
    fn step(&mut self, name: &str, inc: bool, line: usize) -> Kept<()> {
        let target = Expr { kind: E::Name(name.to_string()), line };
        let one = Expr { kind: E::Int { text: "1".into(), value: 1, ty: IntTy::INT }, line };
        self.assign(if inc { "+" } else { "-" }, &target, &one, line)
    }

    fn assign(&mut self, op: &'static str, target: &Expr, value: &Expr, line: usize) -> Kept<()> {
        let (place, ty) = self.place(target, line)?;
        if op == "=" {
            if let E::Cond(test, yes, no) = &value.kind {
                let test = self.cond(test)?;
                self.alone(line)?;
                let yes_v = self.expr(yes)?;
                let yes = self.store(yes_v, &ty, line)?;
                let no_v = self.expr(no)?;
                let no = self.store(no_v, &ty, line)?;
                self.alone(line)?;
                self.line(format!("if {} {{", test.code));
                self.line(format!("    {place} = {yes}"));
                self.line("} else {");
                self.line(format!("    {place} = {no}"));
                self.line("}");
                return Ok(());
            }
            let v = self.expr(value)?;
            let code = self.store(v, &ty, line)?;
            self.flush_pre();
            self.line(format!("{place} = {code}"));
            self.flush_post();
            return Ok(());
        }
        if has_call(target) {
            return keep(line, format!("`{op}=` of a place with a call in it, which would be worked out twice"));
        }
        let current = self.expr(target)?;
        let v = self.expr(value)?;
        let result = self.arith(op, current, v, line)?;
        let code = self.store(result, &ty, line)?;
        self.flush_pre();
        let zop = if op == "/" { "//" } else { op };
        let prefix = format!("{place} {zop} ");
        // `u32( )` checks that the value fits, as `^=` on a u32 does.
        let checked = match &ty {
            CTy::Int(int) if !int.signed && int.bits <= 32 => code.strip_prefix(&format!("u{}(", int.bits)).and_then(|rest| rest.strip_suffix(')')).filter(|inner| balanced(inner)),
            _ => None,
        };
        match code.strip_prefix(&prefix).or_else(|| checked.and_then(|inner| inner.strip_prefix(&prefix))) {
            Some(rest) if !rest.is_empty() => self.line(format!("{place} {zop}= {}", outer_parens_off(rest))),
            _ => self.line(format!("{place} = {code}")),
        }
        self.flush_post();
        Ok(())
    }

    /// The place an assignment writes: its code and type.
    fn place(&mut self, target: &Expr, line: usize) -> Kept<(String, CTy)> {
        match &target.kind {
            E::Name(name) => {
                let (zeno, ty) = self.variable(name, line)?;
                if matches!(ty, CTy::Array(..)) {
                    return keep(line, "an array given a value as a whole");
                }
                Ok((zeno, ty))
            }
            E::Index(base, _) | E::Member(base, _, _) => {
                if let E::Name(name) = &base.kind {
                    if self.lookup(name).is_some_and(|local| local.kind == Kind::Text) || self.world.globals.get(name).is_some_and(|global| global.ty.is_text()) {
                        return keep(line, "it changes a letter of a text");
                    }
                }
                let v = self.expr(target)?;
                if matches!(v.ty, CTy::Array(..)) {
                    return keep(line, "an array given a value as a whole");
                }
                if !v.place {
                    return keep(line, "a value given a value");
                }
                Ok((v.code, v.ty))
            }
            E::Unary("*", inner) => {
                if let E::Name(name) = &inner.kind {
                    if self.lookup(name).is_some_and(|local| local.kind == Kind::Ref) {
                        return keep(line, "a whole struct given a value through a pointer");
                    }
                }
                let v = self.expr(target)?;
                Ok((v.code, v.ty))
            }
            _ => keep(line, "a place the converter does not write to"),
        }
    }

    /// `printf` and the other functions that write.
    fn output(&mut self, name: &str, args: &[Expr], line: usize) -> Kept<()> {
        let buffered = self.world.buffered;
        let write = |w: &mut W, text: String| {
            w.flush_pre();
            if buffered {
                w.helper(Helper::Text);
                w.line(format!("write_text({text})"));
            } else {
                w.line(format!("print({text})"));
            }
            // What a call gave back through a pointer is put back before
            // the next statement reads it.
            w.flush_post();
        };
        match name {
            "printf" | "fprintf" => {
                let (stream, args) = if name == "fprintf" {
                    let Some(first) = args.first() else { return keep(line, "`fprintf` without arguments") };
                    match &first.kind {
                        E::Name(stream) if stream == "stdout" || stream == "stderr" => (stream.as_str(), &args[1..]),
                        _ => return keep(line, "`fprintf` to a file"),
                    }
                } else {
                    ("stdout", args)
                };
                let Some(Expr { kind: E::Str(format), .. }) = args.first() else {
                    return keep(line, format!("`{name}` whose format is not a text written out"));
                };
                let mut parts = self.format(format, &args[1..], line)?;
                if stream == "stderr" {
                    let ends = parts.iter().filter(|part| matches!(part, Part::Text(text) if text.contains('\n'))).count();
                    let Some(Part::Text(last)) = parts.last_mut() else {
                        return keep(line, "`fprintf(stderr, ...)` that does not end its line");
                    };
                    if !last.ends_with('\n') || ends > 1 || last[..last.len() - 1].contains('\n') {
                        return keep(line, "`fprintf(stderr, ...)` that writes other than one whole line");
                    }
                    last.pop();
                    let text = render(&parts, line)?;
                    self.imports.insert("program");
                    self.flush_pre();
                    self.line(format!("program.warn({text})"));
                    self.flush_post();
                    return Ok(());
                }
                if !buffered {
                    match parts.last_mut() {
                        Some(Part::Text(last)) if last.ends_with('\n') => {
                            last.pop();
                        }
                        _ => return keep(line, "a `printf` that writes part of a line"),
                    }
                }
                let text = render(&parts, line)?;
                write(self, text);
                Ok(())
            }
            "puts" => {
                let [arg] = args else { return keep(line, "`puts` with other than one argument") };
                let v = self.expr(arg)?;
                if !v.ty.is_text() {
                    return keep(line, "`puts` of a value that is not a text");
                }
                let text = if buffered { render(&[Part::Field(v.code, String::new()), Part::Text("\n".into())], line)? } else { v.code };
                write(self, text);
                Ok(())
            }
            "putchar" => {
                let [arg] = args else { return keep(line, "`putchar` with other than one argument") };
                let part = match &arg.kind {
                    E::Char(code) if *code < 0x80 => Part::Text(char::from_u32(*code).unwrap_or('?').to_string()),
                    _ => {
                        let v = self.expr(arg)?;
                        if v.int().is_none() {
                            return keep(line, "`putchar` of a value that is not a whole number");
                        }
                        let v = self.exact(v);
                        self.helper(Helper::Letter);
                        Part::Field(format!("letter({})", v.code), String::new())
                    }
                };
                if !buffered {
                    if part == Part::Text("\n".into()) {
                        write(self, "\"\"".to_string());
                        return Ok(());
                    }
                    return keep(line, "a `putchar` that writes part of a line");
                }
                let text = match part {
                    Part::Field(code, _) => code,
                    text => render(&[text], line)?,
                };
                write(self, text);
                Ok(())
            }
            "fputs" => {
                let [text, stream] = args else { return keep(line, "`fputs` with other than two arguments") };
                if !matches!(&stream.kind, E::Name(name) if name == "stdout") || !buffered {
                    return keep(line, "`fputs` to other than `stdout`");
                }
                let v = self.expr(text)?;
                if !v.ty.is_text() {
                    return keep(line, "`fputs` of a value that is not a text");
                }
                write(self, v.code);
                Ok(())
            }
            _ => {
                // exit
                let [arg] = args else { return keep(line, "`exit` with other than one argument") };
                let v = self.expr(arg)?;
                let code = self.store(v, &CTy::Int(IntTy::INT), line)?;
                self.flush_pre();
                if buffered {
                    self.helper(Helper::Text);
                    self.line("finish_text()");
                }
                self.imports.insert("program");
                self.line(format!("program.exit({code})"));
                Ok(())
            }
        }
    }

    /// The parts of what `printf` writes: text, and values with how they
    /// are shown (C 7.21.6.1).
    fn format(&mut self, format: &str, args: &[Expr], line: usize) -> Kept<Vec<Part>> {
        let mut parts: Vec<Part> = Vec::new();
        let mut text = String::new();
        let mut chars = format.chars().peekable();
        let mut next = 0;
        while let Some(c) = chars.next() {
            if c != '%' {
                text.push(c);
                continue;
            }
            let mut flags = String::new();
            while let Some(&flag) = chars.peek() {
                if "-+ #0".contains(flag) {
                    flags.push(flag);
                    chars.next();
                } else {
                    break;
                }
            }
            let mut width = String::new();
            while let Some(&digit) = chars.peek() {
                if digit.is_ascii_digit() {
                    width.push(digit);
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&'*') {
                return keep(line, "a width given as `*` in a format");
            }
            let mut precision = None;
            if chars.peek() == Some(&'.') {
                chars.next();
                let mut digits = String::new();
                while let Some(&digit) = chars.peek() {
                    if digit.is_ascii_digit() {
                        digits.push(digit);
                        chars.next();
                    } else {
                        break;
                    }
                }
                precision = Some(digits.parse::<usize>().unwrap_or(0));
            }
            let mut length = String::new();
            while let Some(&letter) = chars.peek() {
                if "hlLqjzt".contains(letter) {
                    length.push(letter);
                    chars.next();
                } else {
                    break;
                }
            }
            let Some(conversion) = chars.next() else {
                return keep(line, "a format that ends in `%`");
            };
            if conversion == '%' {
                text.push('%');
                continue;
            }
            if flags.contains(['+', ' ', '#']) {
                return keep(line, format!("the flag `{}` in a format, which Zeno does not have", flags.chars().find(|c| "+ #".contains(*c)).unwrap()));
            }
            let Some(arg) = args.get(next) else {
                return keep(line, "a format with more `%` than values");
            };
            next += 1;
            let left = flags.contains('-');
            // Numbers line up on the right in Zeno as in C; texts on the
            // left in Zeno, and on the right in C.
            let number = matches!(conversion, 'd' | 'i' | 'u' | 'x' | 'X' | 'f' | 'F');
            let zeros = flags.contains('0') && !left && number;
            let mut spec = String::new();
            if left {
                spec.push('<');
            } else if !number && !width.is_empty() {
                spec.push('>');
            }
            if zeros && !width.is_empty() {
                spec.push('0');
            }
            spec.push_str(&width);
            let v = self.expr(arg)?;
            let long = length == "l" || length == "ll" || length == "z" || length == "j" || length == "t";
            let field = match conversion {
                'd' | 'i' | 'u' | 'x' | 'X' => {
                    if precision.is_some() {
                        return keep(line, "a whole number shown with a precision (`%.3d`)");
                    }
                    let Some(int) = v.int() else {
                        return keep(line, format!("`%{conversion}` of a value that is not a whole number"));
                    };
                    let promoted = int.promoted();
                    if long != (promoted.bits == 64) {
                        return keep(line, format!("`%{length}{conversion}` of a `{}`, which C reads as another size", v.ty.c_name()));
                    }
                    let signed_conversion = conversion == 'd' || conversion == 'i';
                    let v = if signed_conversion {
                        if promoted.is_u64() {
                            return keep(line, format!("a `{}` shown with `%l{conversion}`, which shows a large one below zero", v.ty.c_name()));
                        }
                        if !promoted.signed && promoted.bits == 32 && !v.nonneg {
                            return keep(line, "an `unsigned int` shown with `%d`, which shows a large one below zero");
                        }
                        self.exact(v)
                    } else if promoted.signed {
                        if promoted.bits == 64 && !v.nonneg {
                            return keep(line, format!("a `long` that may be below zero shown with `%l{conversion}`"));
                        }
                        let v = self.as_unsigned(v, IntTy::UINT);
                        self.exact(v)
                    } else {
                        self.exact(v)
                    };
                    if conversion == 'x' || conversion == 'X' {
                        spec.push(conversion);
                    }
                    Part::Field(v.code, spec)
                }
                'c' => {
                    if precision.is_some() {
                        return keep(line, "`%c` with a precision");
                    }
                    if let E::Char(code) = &arg.kind {
                        if *code < 0x80 && width.is_empty() {
                            text.push(char::from_u32(*code).unwrap_or('?'));
                            continue;
                        }
                    }
                    if v.int().is_none() {
                        return keep(line, "`%c` of a value that is not a whole number");
                    }
                    let v = self.exact(v);
                    self.helper(Helper::Letter);
                    Part::Field(format!("letter({})", v.code), spec)
                }
                's' => {
                    if precision.is_some() {
                        return keep(line, "`%s` with a precision, which cuts the text");
                    }
                    if !v.ty.is_text() {
                        return keep(line, "`%s` of a value that is not a text");
                    }
                    if let E::Str(literal) = &arg.kind {
                        if width.is_empty() {
                            text.push_str(literal);
                            continue;
                        }
                    }
                    Part::Field(v.code, spec)
                }
                'f' | 'F' => {
                    if !matches!(v.ty, CTy::Float(_)) {
                        return keep(line, "`%f` of a value that is not a float");
                    }
                    let v = self.float_side(v, true);
                    spec.push_str(&format!(".{}f", precision.unwrap_or(6)));
                    Part::Field(v.code, spec)
                }
                other => return keep(line, format!("`%{other}` in a format, which Zeno does not show the same way")),
            };
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(field);
        }
        if next < args.len() {
            return keep(line, "a format with fewer `%` than values");
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        // A value with a text in it cannot go inside an f-string: it is
        // worked out on a line before.
        for part in parts.iter_mut() {
            if let Part::Field(code, _) = part {
                if code.contains('"') || code.contains('{') || code.contains('}') {
                    let name = self.fresh("shown");
                    self.pre.push(format!("let {name} = {code}"));
                    *code = name;
                }
            }
        }
        Ok(parts)
    }

    fn for_loop(&mut self, init: Option<&Stmt>, test: Option<&Expr>, step: Option<&Expr>, body: &Stmt, line: usize) -> Kept<()> {
        if let Some(range) = self.range(init, test, step, body)? {
            let (name, zeno, ty, nonneg, code) = range;
            self.line(format!("for {zeno} in {code} {{"));
            self.scopes.push(HashMap::new());
            self.declare(&name, Local { zeno, ty, kind: Kind::Value, nonneg });
            self.breakables.push(Breakable::Loop(true));
            let done = self.body(body);
            self.breakables.pop();
            self.scopes.pop();
            done?;
            self.line("}");
            return Ok(());
        }
        if let Some(init) = init {
            match &init.kind {
                S::Decl(vars) => {
                    for var in vars {
                        self.declaration(var)?;
                    }
                }
                S::Expr(expr) => self.expression_statement(expr)?,
                _ => return keep(line, "a `for` whose first part the converter does not read"),
            }
        }
        let test = match test {
            Some(test) => {
                let test = self.cond(test)?;
                self.alone(line)?;
                test.code
            }
            None => "true".to_string(),
        };
        self.line(format!("while {test} {{"));
        self.breakables.push(Breakable::Loop(step.is_none()));
        let done = self.body(body);
        self.breakables.pop();
        done?;
        if let Some(step) = step {
            self.depth += 1;
            self.expression_statement(step)?;
            self.depth -= 1;
        }
        self.line("}");
        Ok(())
    }

    /// `for (int i = a; i < b; i++)` with nothing in the body that
    /// changes `i` or `b`: `for i in range(a, b)`.
    #[allow(clippy::type_complexity)]
    fn range(&mut self, init: Option<&Stmt>, test: Option<&Expr>, step: Option<&Expr>, body: &Stmt) -> Kept<Option<(String, String, CTy, bool, String)>> {
        let Some(Stmt { kind: S::Decl(vars), .. }) = init else { return Ok(None) };
        let [var] = vars.as_slice() else { return Ok(None) };
        let CTy::Int(int) = var.ty else { return Ok(None) };
        if !int.signed || int.bits < 32 || var.stat {
            return Ok(None);
        }
        let Some(start) = &var.init else { return Ok(None) };
        let name = &var.name;
        let is_var = |e: &Expr| matches!(&e.kind, E::Name(found) if found == name);
        let Some(Expr { kind: E::Binary(op, left, right), .. }) = test else { return Ok(None) };
        let (op, bound) = if is_var(left) {
            (*op, &**right)
        } else if is_var(right) {
            let flipped = match *op {
                "<" => ">",
                ">" => "<",
                "<=" => ">=",
                ">=" => "<=",
                other => other,
            };
            (flipped, &**left)
        } else {
            return Ok(None);
        };
        let Some(step) = step else { return Ok(None) };
        let by: i64 = match &step.kind {
            E::Post(inc, target) | E::Pre(inc, target) if is_var(target) => {
                if *inc {
                    1
                } else {
                    -1
                }
            }
            E::Assign(how @ ("+" | "-"), target, amount) if is_var(target) => match amount.kind {
                E::Int { value, .. } if value > 0 && value < 1 << 31 => {
                    if *how == "+" {
                        value as i64
                    } else {
                        -(value as i64)
                    }
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let up = match op {
            "<" | "<=" => true,
            ">" | ">=" => false,
            _ => return Ok(None),
        };
        if up != (by > 0) || assigns(body, name) || !self.steady(bound, body) {
            return Ok(None);
        }
        let start_v = self.expr(start)?;
        let bound_v = self.expr(bound)?;
        let (Some(start_int), Some(bound_int)) = (start_v.int(), bound_v.int()) else { return Ok(None) };
        let common = usual(usual(start_int, bound_int), int);
        let start_v = self.exact(start_v);
        if !common.signed {
            // C compares `i` as unsigned: the same while `i` is not below zero.
            if !up || !start_v.nonneg {
                return Ok(None);
            }
        }
        let bound_v = if bound_int.signed { self.exact(bound_v) } else { let v = self.as_unsigned(bound_v, bound_int.promoted()); self.exact(v) };
        let shifted = |v: &Val, by: i128| -> String {
            match v.value.filter(|_| v.literal) {
                Some(value) => (value + by).to_string(),
                None => format!("{} {} 1", paren(v, ADD), if by > 0 { "+" } else { "-" }),
            }
        };
        let end = match op {
            "<" | ">" => bound_v.code.clone(),
            "<=" => shifted(&bound_v, 1),
            _ => shifted(&bound_v, -1),
        };
        let code = if by == 1 { format!("range({}, {end})", start_v.code) } else { format!("range({}, {end}, {by})", start_v.code) };
        let nonneg = if up { start_v.nonneg } else { bound_v.nonneg };
        let zeno = self.world.zeno_name(name);
        Ok(Some((name.clone(), zeno, var.ty.clone(), nonneg, code)))
    }

    /// Whether a value is the same each time round a loop with this body.
    fn steady(&self, value: &Expr, body: &Stmt) -> bool {
        match &value.kind {
            E::Int { .. } | E::Char(_) | E::Float { .. } => true,
            E::Name(name) => {
                if self.world.defines.contains_key(name) || self.world.names.enums.contains_key(name) || known_constant(name).is_some() {
                    return true;
                }
                if assigns(body, name) {
                    return false;
                }
                if self.lookup(name).is_some() {
                    return true;
                }
                // A global: no call in the body may change it.
                self.world.globals.get(name).is_some_and(|global| global.constant) || !calls(body)
            }
            E::Unary(op, operand) => matches!(*op, "-" | "+" | "~") && self.steady(operand, body),
            E::Binary(op, left, right) => {
                if *op == "/" && self.length_of(left, right).is_some() {
                    return true;
                }
                self.steady(left, body) && self.steady(right, body)
            }
            E::Cast(_, operand) => self.steady(operand, body),
            E::Call(name, args) if name == "strlen" && !self.world.functions.contains_key(name) => args.iter().all(|arg| self.steady(arg, body)),
            E::SizeofExpr(_) | E::SizeofType(_) => true,
            _ => false,
        }
    }

    fn switch(&mut self, value: &Expr, body: &[Stmt], line: usize) -> Kept<()> {
        let v = self.expr(value)?;
        if v.int().is_none() {
            return keep(line, "a `switch` on a value that is not a whole number");
        }
        let v = self.exact(v);
        let chosen = if v.prec == ATOM && !v.code.contains('(') {
            v.code.clone()
        } else {
            let name = self.fresh("chosen");
            self.flush_pre();
            self.line(format!("let {name} = {}", v.code));
            name
        };
        // The groups: the labels, then the statements under them.
        let mut groups: Vec<(Vec<Option<&Expr>>, Vec<&Stmt>)> = Vec::new();
        for stmt in body {
            match &stmt.kind {
                S::Case(value) => {
                    if groups.last().is_none_or(|(_, stmts)| !stmts.is_empty()) {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    groups.last_mut().unwrap().0.push(Some(value));
                }
                S::Default => {
                    if groups.last().is_none_or(|(_, stmts)| !stmts.is_empty()) {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    groups.last_mut().unwrap().0.push(None);
                }
                _ => match groups.last_mut() {
                    Some((_, stmts)) => stmts.push(stmt),
                    None => return keep(stmt.line, "a statement in a `switch` before its first `case`"),
                },
            }
        }
        let count = groups.len();
        let mut written: Vec<(Option<String>, Vec<&Stmt>)> = Vec::new();
        let mut default: Option<Vec<&Stmt>> = None;
        for (at, (labels, mut stmts)) in groups.into_iter().enumerate() {
            match stmts.last().map(|stmt| &stmt.kind) {
                Some(S::Break) => {
                    stmts.pop();
                }
                Some(S::Return(_)) | Some(S::Continue) => {}
                _ if at + 1 == count => {}
                _ => return keep(stmts.last().map(|stmt| stmt.line).unwrap_or(line), "a `case` that runs on into the next one"),
            }
            if labels.contains(&None) {
                default = Some(stmts);
                continue;
            }
            let mut tests = Vec::new();
            for label in labels.into_iter().flatten() {
                let case = self.expr(label)?;
                if case.int().is_none() {
                    return keep(label.line, "a `case` that is not a whole number");
                }
                tests.push(format!("{chosen} == {}", paren(&case, CMP + 1)));
            }
            written.push((Some(tests.join(" or ")), stmts));
        }
        if let Some(stmts) = default {
            written.push((None, stmts));
        }
        self.breakables.push(Breakable::Switch);
        let done = (|| -> Kept<()> {
            if let [(None, stmts)] = written.as_slice() {
                for stmt in stmts {
                    self.statement(stmt)?;
                }
                return Ok(());
            }
            for (at, (test, stmts)) in written.iter().enumerate() {
                match (at, test) {
                    (0, Some(test)) => self.line(format!("if {test} {{")),
                    (_, Some(test)) => self.line(format!("}} else if {test} {{")),
                    (_, None) => self.line("} else {"),
                }
                self.depth += 1;
                self.scopes.push(HashMap::new());
                for stmt in stmts {
                    self.statement(stmt)?;
                }
                self.scopes.pop();
                self.depth -= 1;
            }
            if !written.is_empty() {
                self.line("}");
            }
            Ok(())
        })();
        self.breakables.pop();
        done
    }

    // ----------------------------------------------------------- Functions

    /// A function of C as a function of Zeno.
    pub fn function(&mut self, f: &Function, info: &FnInfo) -> Kept<()> {
        let line = f.line;
        self.comments_before(line + 1);
        let is_main = f.name == "main";
        if is_main && !f.params.is_empty() {
            return keep(line, "`main` reads the program's arguments (`argc`, `argv`)");
        }
        if is_main && !matches!(f.ret, CTy::Int(IntTy { bits: 32, signed: true, .. })) {
            return keep(line, "a `main` that does not give back an `int`");
        }
        self.assigned = assigned_names(&f.body);
        let mut params = Vec::new();
        let mut copies = Vec::new();
        for (param, kind) in f.params.iter().zip(&info.kinds) {
            let zty = match kind {
                Kind::Value | Kind::Text => zeno_type(&param.ty, line)?,
                Kind::Array | Kind::Box => format!("list[{}]", zeno_type(&pointee(&param.ty), line)?),
                Kind::Ref => zeno_type(&pointee(&param.ty), line)?,
            };
            let zeno = self.world.zeno_name(&param.name);
            // A parameter of C is a variable the function may change.
            let given = if matches!(kind, Kind::Value | Kind::Text) && self.assigned.contains(&param.name) {
                let given = self.fresh(&format!("{zeno}_given"));
                copies.push(format!("var {zeno} = {given}"));
                given
            } else {
                zeno.clone()
            };
            if matches!(param.ty, CTy::Struct(_)) && info.changes.get(params.len()).copied().unwrap_or(false) {
                self.owned.insert(zeno.clone());
            }
            params.push(format!("{given}: {zty}"));
            self.declare(&param.name, Local { zeno, ty: param.ty.clone(), kind: *kind, nonneg: false });
        }
        self.main_void = is_main && returns_only_zero(&f.body);
        self.ret = f.ret.clone();
        let ret = if self.main_void || f.ret == CTy::Void { String::new() } else { format!(" -> {}", zeno_type(&f.ret, line)?) };
        self.line(format!("function {}({}){ret} {{", self.world.zeno_name(&f.name), params.join(", ")));
        self.depth += 1;
        for copy in copies {
            self.line(copy);
        }
        self.scopes.push(HashMap::new());
        let mut body: &[Stmt] = &f.body;
        if self.main_void {
            if let Some(Stmt { kind: S::Return(_), .. }) = body.last() {
                body = &body[..body.len() - 1];
            }
        }
        self.statements(body)?;
        self.comments_before(f.end_line);
        self.scopes.pop();
        self.depth -= 1;
        self.line("}");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    /// A value, and what follows its `:` in an f-string.
    Field(String, String),
}

/// What `printf` writes, as a text of Zeno: an f-string when it shows
/// values.
fn render(parts: &[Part], line: usize) -> Kept<String> {
    let fields = parts.iter().any(|part| matches!(part, Part::Field(..)));
    let mut out = String::new();
    if fields {
        out.push('f');
    }
    out.push('"');
    for part in parts {
        match part {
            Part::Text(text) => {
                let Some(written) = zeno_text(text, '"') else {
                    return keep(line, "a text with a character that Zeno writes otherwise");
                };
                let inner = &written[1..written.len() - 1];
                if fields {
                    out.push_str(&inner.replace('{', "{{").replace('}', "}}"));
                } else {
                    out.push_str(inner);
                }
            }
            Part::Field(code, spec) => {
                if spec.is_empty() {
                    out.push_str(&format!("{{{code}}}"));
                } else {
                    out.push_str(&format!("{{{code}:{spec}}}"));
                }
            }
        }
    }
    out.push('"');
    Ok(out)
}

/// Whether the brackets in a piece of code close as they open.
fn balanced(code: &str) -> bool {
    let mut depth = 0i32;
    for c in code.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// `(a - b)` as `a - b`, when the brackets hold the whole of it.
fn outer_parens_off(code: &str) -> &str {
    match code.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')) {
        Some(inner) if balanced(inner) => inner,
        _ => code,
    }
}

fn prec_of(op: &str) -> u8 {
    match op {
        "|" => BOR,
        "^" => BXOR,
        "&" => BAND,
        "<<" | ">>" => SHIFT,
        "+" | "-" => ADD,
        _ => MUL,
    }
}

fn fold(op: &str, a: Option<i128>, b: Option<i128>) -> Option<i128> {
    let (a, b) = (a?, b?);
    match op {
        "+" => Some(a + b),
        "-" => Some(a - b),
        "*" => a.checked_mul(b),
        "&" => Some(a & b),
        "|" => Some(a | b),
        "^" => Some(a ^ b),
        _ => None,
    }
}

pub fn is_unsigned(ty: &CTy) -> bool {
    matches!(ty, CTy::Int(IntTy { signed: false, .. }))
}

fn contains_sized(ty: &CTy) -> bool {
    match ty {
        CTy::Array(item, _) => contains_sized(item),
        other => is_sized(other),
    }
}

/// What a pointer or an array holds.
pub fn pointee(ty: &CTy) -> CTy {
    match ty {
        CTy::Ptr(item, _) | CTy::Array(item, _) => (**item).clone(),
        other => other.clone(),
    }
}

/// The value C gives a variable of static storage that is given none,
/// and that the converter gives one inside a function too.
pub fn zero(ty: &CTy, names: &Names, line: usize) -> Kept<String> {
    Ok(match ty {
        CTy::Int(_) => "0".into(),
        CTy::Float(_) => "0.0".into(),
        CTy::Bool => "false".into(),
        ty if ty.is_text() => "\"\"".into(),
        CTy::Struct(name) => {
            let Some(fields) = names.structs.get(name) else {
                return keep(line, format!("the struct `{name}`, whose fields the converter did not read"));
            };
            let mut codes = Vec::new();
            for field in fields {
                codes.push(zero(&field.ty, names, line)?);
            }
            format!("{name}({})", codes.join(", "))
        }
        CTy::Array(item, Some(size)) => match &**item {
            CTy::Int(_) | CTy::Float(_) | CTy::Bool => format!("[{}] * {size}", zero(item, names, line)?),
            other => format!("[{} for _ in range({size})]", zero(other, names, line)?),
        },
        CTy::Array(_, None) => return keep(line, "an array without a length or values"),
        other => return keep(line, format!("a value of type `{}`", other.c_name())),
    })
}

/// The size C gives a type on a machine where `int` has 32 bits and
/// pointers 64.
fn size_of(ty: &CTy) -> Option<u64> {
    Some(match ty {
        CTy::Int(int) => int.bits as u64 / 8,
        CTy::Float(true) => 4,
        CTy::Float(false) => 8,
        CTy::Bool => 1,
        CTy::Array(item, Some(count)) => size_of(item)? * *count as u64,
        _ => return None,
    })
}

/// The values of `<limits.h>` and `<stdlib.h>` that a file uses by name.
fn known_constant(name: &str) -> Option<(i128, IntTy)> {
    Some(match name {
        "INT_MAX" => (i32::MAX as i128, IntTy::INT),
        "INT_MIN" => (i32::MIN as i128, IntTy::INT),
        "UINT_MAX" => (u32::MAX as i128, IntTy::UINT),
        "LONG_MAX" => (i64::MAX as i128, IntTy::LONG),
        "LONG_MIN" => (i64::MIN as i128, IntTy::LONG),
        "SHRT_MAX" => (i16::MAX as i128, IntTy::INT),
        "SHRT_MIN" => (i16::MIN as i128, IntTy::INT),
        "CHAR_BIT" => (8, IntTy::INT),
        "CHAR_MAX" | "SCHAR_MAX" => (127, IntTy::INT),
        "CHAR_MIN" | "SCHAR_MIN" => (-128, IntTy::INT),
        "UCHAR_MAX" => (255, IntTy::INT),
        "USHRT_MAX" => (65535, IntTy::INT),
        "EXIT_SUCCESS" => (0, IntTy::INT),
        "EXIT_FAILURE" => (1, IntTy::INT),
        _ => return None,
    })
}

// ------------------------------------------------------- Looking at trees

/// Calls `found` for every expression in a statement, and those in it.
pub fn each_expr(stmt: &Stmt, found: &mut dyn FnMut(&Expr)) {
    let expr = |e: &Expr, found: &mut dyn FnMut(&Expr)| walk_expr(e, found);
    match &stmt.kind {
        S::Decl(vars) => {
            for var in vars {
                if let Some(init) = &var.init {
                    expr(init, found);
                }
            }
        }
        S::Expr(e) | S::Case(e) => expr(e, found),
        S::If(test, then, other) => {
            expr(test, found);
            each_expr(then, found);
            if let Some(other) = other {
                each_expr(other, found);
            }
        }
        S::While(test, body) | S::Do(body, test) => {
            expr(test, found);
            each_expr(body, found);
        }
        S::For(init, test, step, body) => {
            if let Some(init) = init {
                each_expr(init, found);
            }
            if let Some(test) = test {
                expr(test, found);
            }
            if let Some(step) = step {
                expr(step, found);
            }
            each_expr(body, found);
        }
        S::Switch(value, body) => {
            expr(value, found);
            for stmt in body {
                each_expr(stmt, found);
            }
        }
        S::Return(Some(e)) => expr(e, found),
        S::Block(body) => {
            for stmt in body {
                each_expr(stmt, found);
            }
        }
        _ => {}
    }
}

pub fn walk_expr(e: &Expr, found: &mut dyn FnMut(&Expr)) {
    found(e);
    match &e.kind {
        E::Unary(_, a) | E::Pre(_, a) | E::Post(_, a) | E::Cast(_, a) | E::SizeofExpr(a) | E::Member(a, _, _) | E::Field(_, a) => walk_expr(a, found),
        E::Binary(_, a, b) | E::Assign(_, a, b) | E::Index(a, b) | E::Comma(a, b) => {
            walk_expr(a, found);
            walk_expr(b, found);
        }
        E::Cond(a, b, c) => {
            walk_expr(a, found);
            walk_expr(b, found);
            walk_expr(c, found);
        }
        E::Call(_, args) | E::List(args) => {
            for arg in args {
                walk_expr(arg, found);
            }
        }
        _ => {}
    }
}

/// `values[count++] = n`: the `++` of a variable that the statement
/// names nowhere else is a step of its own, after the statement (or before
/// it, for `++count`). The statement without them, and the steps.
#[allow(clippy::type_complexity)]
fn lift_steps(stmt: &Expr) -> Option<(Vec<(String, bool)>, Expr, Vec<(String, bool)>)> {
    // A statement that is only a step is written as one already.
    if matches!(stmt.kind, E::Pre(..) | E::Post(..)) {
        return None;
    }
    let mut steps: Vec<(String, bool, bool)> = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();
    let mut other = false;
    walk_expr(stmt, &mut |e| match &e.kind {
        E::Pre(inc, target) | E::Post(inc, target) => match &target.kind {
            E::Name(name) => steps.push((name.clone(), *inc, matches!(e.kind, E::Pre(..)))),
            _ => other = true,
        },
        E::Name(name) => *names.entry(name.clone()).or_insert(0) += 1,
        _ => {}
    });
    if steps.is_empty() || other {
        return None;
    }
    // Each stepped variable is named once, by its step.
    if steps.iter().any(|(name, _, _)| names.get(name).copied().unwrap_or(0) != 1) {
        return None;
    }
    fn plain(e: &Expr) -> Expr {
        let kind = match &e.kind {
            E::Pre(_, target) | E::Post(_, target) => return (**target).clone(),
            E::Unary(op, a) => E::Unary(op, Box::new(plain(a))),
            E::Binary(op, a, b) => E::Binary(op, Box::new(plain(a)), Box::new(plain(b))),
            E::Assign(op, a, b) => E::Assign(op, Box::new(plain(a)), Box::new(plain(b))),
            E::Index(a, b) => E::Index(Box::new(plain(a)), Box::new(plain(b))),
            E::Member(a, field, arrow) => E::Member(Box::new(plain(a)), field.clone(), *arrow),
            E::Cast(ty, a) => E::Cast(ty.clone(), Box::new(plain(a))),
            E::Call(name, args) => E::Call(name.clone(), args.iter().map(plain).collect()),
            E::Cond(a, b, c) => E::Cond(Box::new(plain(a)), Box::new(plain(b)), Box::new(plain(c))),
            other => other.clone(),
        };
        Expr { kind, line: e.line }
    }
    // A step inside `? :`, `&&` or `||` happens only sometimes.
    let mut sometimes = false;
    walk_expr(stmt, &mut |e| {
        if let E::Cond(_, a, b) | E::Binary("&&" | "||", a, b) = &e.kind {
            let mut inner = |e: &Expr| walk_expr(e, &mut |e| sometimes |= matches!(e.kind, E::Pre(..) | E::Post(..)));
            inner(a);
            inner(b);
        }
    });
    if sometimes {
        return None;
    }
    let before = steps.iter().filter(|(_, _, pre)| *pre).map(|(name, inc, _)| (name.clone(), *inc)).collect();
    let after = steps.iter().filter(|(_, _, pre)| !*pre).map(|(name, inc, _)| (name.clone(), *inc)).collect();
    Some((before, plain(stmt), after))
}

/// The variable an assignment changes, when it is a plain name.
fn root_name(e: &Expr) -> Option<&str> {
    match &e.kind {
        E::Name(name) => Some(name),
        _ => None,
    }
}

/// The names that some statement gives a new value: by `=`, `++`, or by
/// giving its address to a function.
pub fn assigned_names(body: &[Stmt]) -> HashSet<String> {
    let mut names = HashSet::new();
    for stmt in body {
        each_expr(stmt, &mut |e| match &e.kind {
            E::Assign(_, target, _) | E::Pre(_, target) | E::Post(_, target) => {
                if let Some(name) = root_name(target) {
                    names.insert(name.to_string());
                }
            }
            E::Unary("&", target) => {
                if let Some(name) = root_name(target) {
                    names.insert(name.to_string());
                }
            }
            _ => {}
        });
    }
    names
}

fn assigns(body: &Stmt, name: &str) -> bool {
    assigned_names(std::slice::from_ref(body)).contains(name)
}

fn calls(body: &Stmt) -> bool {
    let mut found = false;
    each_expr(body, &mut |e| {
        if matches!(e.kind, E::Call(..)) {
            found = true;
        }
    });
    found
}

fn has_call(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |e| {
        if matches!(e.kind, E::Call(..) | E::Assign(..) | E::Pre(..) | E::Post(..)) {
            found = true;
        }
    });
    found
}

/// Whether a `continue` in this body belongs to the loop around it.
fn continues(stmt: &Stmt) -> bool {
    match &stmt.kind {
        S::Continue => true,
        S::If(_, then, other) => continues(then) || other.as_deref().is_some_and(continues),
        S::Block(body) | S::Switch(_, body) => body.iter().any(continues),
        _ => false,
    }
}

/// Whether every `return` in `main` gives 0.
pub fn returns_only_zero(body: &[Stmt]) -> bool {
    fn all(stmt: &Stmt) -> bool {
        match &stmt.kind {
            S::Return(Some(Expr { kind: E::Int { value: 0, .. }, .. })) => true,
            S::Return(Some(Expr { kind: E::Name(name), .. })) => name == "EXIT_SUCCESS",
            S::Return(_) => false,
            S::If(_, then, other) => all(then) && other.as_deref().is_none_or(all),
            S::While(_, body) | S::Do(body, _) | S::For(_, _, _, body) => all(body),
            S::Block(body) | S::Switch(_, body) => body.iter().all(all),
            _ => true,
        }
    }
    body.iter().all(all)
}
