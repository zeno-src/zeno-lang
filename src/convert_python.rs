//! `zeno convert program.py`: the part that reads Python (docs/convert.md,
//! decisions chapter 62).
//!
//! Three steps, each of which may give up on a piece without giving up
//! on the file:
//!
//! 1. `lex` makes tokens of the whole file, as Python does: indentation
//!    becomes `Indent` and `Dedent`, and a line that goes on inside
//!    brackets is one line.
//! 2. `pieces` cuts the file where a statement begins at the left
//!    margin. This needs nothing of Python but where a statement ends.
//! 3. `Parser` reads one piece into a tree, for the part of Python the
//!    converter knows. Anything else is `Err(Keep)`, with the line and
//!    what was met there, and the piece is kept as Python.
//!
//! What is done with the tree is in `convert_python_write.rs`.

/// Why a piece is not translated: the line, and what was met there, in
/// words that go into the report as they are.
#[derive(Debug, Clone, PartialEq)]
pub struct Keep {
    pub line: usize,
    pub why: String,
}

pub type Kept<T> = Result<T, Keep>;

pub fn keep<T>(line: usize, why: impl Into<String>) -> Kept<T> {
    Err(Keep { line, why: why.into() })
}

// ------------------------------------------------------------------ Tokens

#[derive(Debug, Clone, PartialEq)]
pub enum T {
    Name(String),
    /// A number, as it was written.
    Number(String),
    Str(StrLit),
    Op(&'static str),
    Newline,
    Indent,
    Dedent,
    End,
}

/// A text of Python, as it was written between its quotes.
#[derive(Debug, Clone, PartialEq)]
pub struct StrLit {
    /// The letters in front of the quote, in small letters: `f`, `r`, `b`.
    pub prefix: String,
    pub body: String,
    pub triple: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub kind: T,
    pub line: usize,
    /// The line the token ends on: another one only for a text between
    /// three quotes.
    pub end_line: usize,
}

/// A comment, without its `#`.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub line: usize,
    pub text: String,
    /// Whether nothing but blanks is in front of it on its line.
    pub alone: bool,
}

pub struct Lexed {
    pub toks: Vec<Tok>,
    pub comments: Vec<Comment>,
}

const OPERATORS: &[&str] = &[
    "**=", "//=", ">>=", "<<=", "...", "!=", "==", "<=", ">=", "->", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "@=", ":=", "**",
    "//", "<<", ">>", "+", "-", "*", "/", "%", "@", "&", "|", "^", "~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":", ".", ";", "=",
];

/// The tokens of a file of Python. An `Err` is a file that cannot be cut
/// into lines at all: a text with no end, a tab in the indentation.
pub fn lex(source: &str) -> Result<Lexed, Keep> {
    let chars: Vec<char> = source.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut comments = Vec::new();
    let mut indents: Vec<usize> = vec![0];
    let mut depth = 0usize;
    let mut i = 0usize;
    let mut line = 1usize;
    let mut at_line_start = true;
    // Whether the line so far holds a token, so that an empty line gives
    // no `Newline`.
    let mut line_has_token = false;

    while i < chars.len() {
        if at_line_start && depth == 0 {
            // The indentation of a line that holds something.
            let mut width = 0;
            let mut j = i;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\u{c}') {
                if chars[j] == '\t' {
                    return keep(line, "a tab in the indentation: how far in a line is cannot be told");
                }
                if chars[j] == ' ' {
                    width += 1;
                }
                j += 1;
            }
            if j >= chars.len() {
                break;
            }
            match chars[j] {
                '\n' => {
                    i = j + 1;
                    line += 1;
                    continue;
                }
                '\r' => {
                    i = j + 1;
                    continue;
                }
                '#' => {
                    let end = (j..chars.len()).find(|&k| chars[k] == '\n').unwrap_or(chars.len());
                    let text: String = chars[j + 1..end].iter().collect();
                    comments.push(Comment { line, text: text.trim_end().to_string(), alone: true });
                    i = end;
                    continue;
                }
                _ => {}
            }
            let top = *indents.last().unwrap_or(&0);
            if width > top {
                indents.push(width);
                toks.push(Tok { kind: T::Indent, line, end_line: line });
            } else {
                while width < *indents.last().unwrap_or(&0) {
                    indents.pop();
                    toks.push(Tok { kind: T::Dedent, line, end_line: line });
                }
                if width != *indents.last().unwrap_or(&0) {
                    return keep(line, "this line is not as far in as any line above it");
                }
            }
            i = j;
            at_line_start = false;
            line_has_token = false;
        }
        let c = chars[i];
        match c {
            '\n' => {
                if depth == 0 && line_has_token {
                    toks.push(Tok { kind: T::Newline, line, end_line: line });
                }
                if depth == 0 {
                    at_line_start = true;
                }
                line += 1;
                i += 1;
            }
            ' ' | '\t' | '\r' | '\u{c}' => i += 1,
            '\\' if chars.get(i + 1) == Some(&'\n') => {
                line += 1;
                i += 2;
            }
            '\\' if chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n') => {
                line += 1;
                i += 3;
            }
            '#' => {
                let end = (i..chars.len()).find(|&k| chars[k] == '\n').unwrap_or(chars.len());
                let text: String = chars[i + 1..end].iter().collect();
                comments.push(Comment { line, text: text.trim_end().to_string(), alone: !line_has_token && depth == 0 });
                i = end;
            }
            '"' | '\'' => {
                let (lit, next, lines) = string(&chars, i, String::new(), line)?;
                toks.push(Tok { kind: T::Str(lit), line, end_line: line + lines });
                line += lines;
                i = next;
                line_has_token = true;
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                let quote = chars.get(j).copied();
                let is_prefix = word.len() <= 2 && word.chars().all(|c| "rRbBuUfF".contains(c));
                if is_prefix && matches!(quote, Some('"' | '\'')) {
                    let (lit, next, lines) = string(&chars, j, word.to_lowercase(), line)?;
                    toks.push(Tok { kind: T::Str(lit), line, end_line: line + lines });
                    line += lines;
                    i = next;
                } else {
                    toks.push(Tok { kind: T::Name(word), line, end_line: line });
                    i = j;
                }
                line_has_token = true;
            }
            c if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) => {
                let mut j = i;
                let hex = c == '0' && matches!(chars.get(i + 1), Some('x' | 'X'));
                while j < chars.len() {
                    let d = chars[j];
                    let sign_of_exponent = (d == '+' || d == '-') && !hex && matches!(chars[j - 1], 'e' | 'E') && j > i;
                    if d.is_alphanumeric() || d == '_' || d == '.' || sign_of_exponent {
                        j += 1;
                    } else {
                        break;
                    }
                }
                toks.push(Tok { kind: T::Number(chars[i..j].iter().collect()), line, end_line: line });
                i = j;
                line_has_token = true;
            }
            _ => {
                let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
                let Some(op) = OPERATORS.iter().find(|op| rest.starts_with(**op)) else {
                    return keep(line, format!("the character `{c}`, which begins nothing the converter knows"));
                };
                match *op {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    _ => {}
                }
                toks.push(Tok { kind: T::Op(op), line, end_line: line });
                i += op.len();
                line_has_token = true;
            }
        }
    }
    if depth > 0 {
        return keep(line, "a bracket that is never closed");
    }
    if line_has_token && !matches!(toks.last().map(|tok| &tok.kind), Some(T::Newline)) {
        toks.push(Tok { kind: T::Newline, line, end_line: line });
    }
    while indents.len() > 1 {
        indents.pop();
        toks.push(Tok { kind: T::Dedent, line, end_line: line });
    }
    toks.push(Tok { kind: T::End, line, end_line: line });
    Ok(Lexed { toks, comments })
}

/// The text that begins with the quote at `start`: what it is, where the
/// next token begins, and how many line ends are inside it.
fn string(chars: &[char], start: usize, prefix: String, line: usize) -> Result<(StrLit, usize, usize), Keep> {
    let quote = chars[start];
    let triple = chars.get(start + 1) == Some(&quote) && chars.get(start + 2) == Some(&quote);
    let mut i = start + if triple { 3 } else { 1 };
    let mut body = String::new();
    let mut lines = 0;
    loop {
        let Some(&c) = chars.get(i) else {
            return keep(line, "a text that is never closed");
        };
        if c == '\\' {
            // A `\` takes the next character with it, in a raw text too:
            // there it stays in the text, and still does not end it.
            body.push(c);
            if let Some(&next) = chars.get(i + 1) {
                body.push(next);
                if next == '\n' {
                    lines += 1;
                }
            }
            i += 2;
            continue;
        }
        if c == quote {
            if !triple {
                return Ok((StrLit { prefix, body, triple }, i + 1, lines));
            }
            if chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote) {
                return Ok((StrLit { prefix, body, triple }, i + 3, lines));
            }
        }
        if c == '\n' {
            if !triple {
                return keep(line, "a text that is never closed");
            }
            lines += 1;
        }
        body.push(c);
        i += 1;
    }
}

// ------------------------------------------------------------------ Pieces

/// A statement that begins at the left margin, with the comments right
/// above it: the tokens it is made of and the lines it is written on.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    /// The tokens, from the first of the statement up to the first of the
    /// next piece.
    pub toks: std::ops::Range<usize>,
    /// The first line of the statement itself.
    pub line: usize,
    /// The first line that belongs to the piece: the comments above it.
    pub first_line: usize,
    pub last_line: usize,
}

/// Where the file is cut: at each line that begins a statement at the
/// left margin. `else`, `except` and their like go on with the statement
/// above, and a line that begins with `@` belongs to the one below.
pub fn pieces(lexed: &Lexed) -> Vec<Piece> {
    let toks = &lexed.toks;
    let mut starts = Vec::new();
    let mut level = 0usize;
    let mut line_start = true;
    let mut after_decorator = false;
    for (at, tok) in toks.iter().enumerate() {
        match &tok.kind {
            T::Indent => level += 1,
            T::Dedent => level = level.saturating_sub(1),
            T::Newline => line_start = true,
            T::End => {}
            kind => {
                if line_start && level == 0 {
                    let goes_on = matches!(kind, T::Name(word) if matches!(word.as_str(), "elif" | "else" | "except" | "finally"));
                    if !goes_on && !after_decorator {
                        starts.push(at);
                    }
                    after_decorator = matches!(kind, T::Op("@"));
                }
                line_start = false;
            }
        }
    }
    let end = toks.len().saturating_sub(1);
    let mut found = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let stop = starts.get(n + 1).copied().unwrap_or(end);
        let last = toks[start..stop].iter().rev().find(|tok| !matches!(tok.kind, T::Newline | T::Indent | T::Dedent | T::End));
        let line = toks[start].line;
        let last_line = last.map_or(line, |tok| tok.end_line);
        found.push(Piece { toks: start..stop, line, first_line: line, last_line });
    }
    // The comments right above a piece are its own: those with no empty
    // line and no other piece between them and it. A comment further in
    // than the margin, after the last line of a piece, is still inside
    // that piece.
    for n in 0..found.len() {
        let above = if n == 0 { 0 } else { found[n - 1].last_line };
        let mut first = found[n].line;
        while first > above + 1 && lexed.comments.iter().any(|comment| comment.alone && comment.line == first - 1) {
            first -= 1;
        }
        found[n].first_line = first;
    }
    found
}

// -------------------------------------------------------------------- Tree

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: E,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum E {
    Name(String),
    Number(String),
    Str(StrLit),
    True,
    False,
    None,
    List(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    Tuple(Vec<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    /// `and` or `or`.
    Logic(&'static str, Box<Expr>, Box<Expr>),
    /// `a < b <= c`: the first operand, then each operator with the one
    /// after it. `is not` and `not in` are one operator each.
    Compare(Box<Expr>, Vec<(&'static str, Expr)>),
    Call { function: Box<Expr>, args: Vec<Expr>, named: Vec<(String, Expr)> },
    Attr(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Slice { of: Box<Expr>, from: Option<Box<Expr>>, to: Option<Box<Expr>> },
    Lambda(Vec<String>, Box<Expr>),
    IfElse { then: Box<Expr>, condition: Box<Expr>, otherwise: Box<Expr> },
    ListOf { item: Box<Expr>, name: String, items: Box<Expr>, condition: Option<Box<Expr>> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub annotation: Option<Expr>,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Handler {
    /// The kind of error caught, or `None` for a bare `except:`.
    pub kind: Option<String>,
    pub name: Option<String>,
    pub body: Vec<Stmt>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: S,
    pub line: usize,
    pub last_line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum S {
    Expr(Expr),
    /// `target = value`. The target is a name, a field, an item, or a
    /// tuple of names.
    Assign { target: Expr, value: Expr },
    Augmented { target: Expr, op: &'static str, value: Expr },
    Annotated { target: Expr, annotation: Expr, value: Option<Expr> },
    If { branches: Vec<(Expr, Vec<Stmt>, usize)>, otherwise: Option<Vec<Stmt>> },
    While { condition: Expr, body: Vec<Stmt> },
    For { names: Vec<String>, items: Expr, body: Vec<Stmt> },
    Def(Def),
    Class { name: String, bases: Vec<Expr>, decorators: Vec<Expr>, body: Vec<Stmt> },
    Return(Option<Expr>),
    Raise(Option<Expr>),
    Try { body: Vec<Stmt>, handlers: Vec<Handler> },
    Import { module: String, name: String },
    FromImport { module: String, names: Vec<(String, String)> },
    Global(Vec<String>),
    Delete(Expr),
    Pass,
    Break,
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub params: Vec<Param>,
    pub returns: Option<Expr>,
    pub body: Vec<Stmt>,
}

// ------------------------------------------------------------------ Parser

pub struct Parser<'a> {
    toks: &'a [Tok],
    at: usize,
}

impl<'a> Parser<'a> {
    pub fn new(toks: &'a [Tok]) -> Self {
        Parser { toks, at: 0 }
    }

    fn peek(&self) -> &T {
        self.toks.get(self.at).map_or(&T::End, |tok| &tok.kind)
    }

    fn line(&self) -> usize {
        self.toks.get(self.at).or(self.toks.last()).map_or(1, |tok| tok.line)
    }

    /// The line that what was read so far ends on: the end of the last
    /// token that is not the end of a line or a change of indentation.
    fn line_before(&self) -> usize {
        let read = &self.toks[..self.at.min(self.toks.len())];
        read.iter().rev().find(|tok| !matches!(tok.kind, T::Newline | T::Indent | T::Dedent | T::End)).map_or(1, |tok| tok.end_line)
    }

    fn next(&mut self) -> T {
        let kind = self.peek().clone();
        self.at += 1;
        kind
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), T::Op(found) if *found == op)
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), T::Name(found) if found == word)
    }

    fn take_op(&mut self, op: &str) -> bool {
        let found = self.is_op(op);
        if found {
            self.at += 1;
        }
        found
    }

    fn take_word(&mut self, word: &str) -> bool {
        let found = self.is_word(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_op(&mut self, op: &str) -> Kept<()> {
        if self.take_op(op) {
            return Ok(());
        }
        keep(self.line(), format!("{} where `{op}` was expected", describe(self.peek())))
    }

    fn name(&mut self) -> Kept<String> {
        match self.peek().clone() {
            T::Name(name) if !is_python_keyword(&name) => {
                self.at += 1;
                Ok(name)
            }
            other => keep(self.line(), format!("{} where a name was expected", describe(&other))),
        }
    }

    /// Every statement of a piece: one, or several on a line with `;`.
    pub fn piece(&mut self) -> Kept<Vec<Stmt>> {
        let mut statements = Vec::new();
        while !matches!(self.peek(), T::End) && self.at < self.toks.len() {
            if matches!(self.peek(), T::Newline | T::Dedent | T::Indent) {
                self.at += 1;
                continue;
            }
            self.statement(&mut statements)?;
        }
        Ok(statements)
    }

    /// The statements of a block: after the `:`, on the same line or on
    /// the lines below, further in.
    fn block(&mut self) -> Kept<Vec<Stmt>> {
        self.expect_op(":")?;
        let mut statements = Vec::new();
        if !matches!(self.peek(), T::Newline) {
            self.simple_line(&mut statements)?;
            return Ok(statements);
        }
        self.at += 1;
        if !matches!(self.peek(), T::Indent) {
            return keep(self.line(), "a block with nothing in it");
        }
        self.at += 1;
        while !matches!(self.peek(), T::Dedent | T::End) {
            self.statement(&mut statements)?;
        }
        if matches!(self.peek(), T::Dedent) {
            self.at += 1;
        }
        Ok(statements)
    }

    fn statement(&mut self, into: &mut Vec<Stmt>) -> Kept<()> {
        let line = self.line();
        let word = match self.peek() {
            T::Name(word) => word.clone(),
            T::Op("@") => return self.decorated(into),
            _ => String::new(),
        };
        let kind = match word.as_str() {
            "if" => {
                self.at += 1;
                let mut branches = Vec::new();
                let condition = self.expression()?;
                branches.push((condition, self.block()?, line));
                let mut otherwise = None;
                loop {
                    let at = self.line();
                    if self.take_word("elif") {
                        let condition = self.expression()?;
                        branches.push((condition, self.block()?, at));
                    } else if self.take_word("else") {
                        otherwise = Some(self.block()?);
                        break;
                    } else {
                        break;
                    }
                }
                S::If { branches, otherwise }
            }
            "while" => {
                self.at += 1;
                let condition = self.expression()?;
                let body = self.block()?;
                if self.is_word("else") {
                    return keep(self.line(), "`else` after a `while` has no words in Zeno");
                }
                S::While { condition, body }
            }
            "for" => {
                self.at += 1;
                let mut names = vec![self.for_name()?];
                while self.take_op(",") {
                    names.push(self.for_name()?);
                }
                if !self.take_word("in") {
                    return keep(self.line(), "a `for` whose variable is not a name");
                }
                let items = self.expressions()?;
                let body = self.block()?;
                if self.is_word("else") {
                    return keep(self.line(), "`else` after a `for` has no words in Zeno");
                }
                S::For { names, items, body }
            }
            "def" => S::Def(self.def()?),
            "class" => self.class(Vec::new())?,
            "try" => {
                self.at += 1;
                let body = self.block()?;
                let mut handlers = Vec::new();
                while self.is_word("except") {
                    let line = self.line();
                    self.at += 1;
                    let mut kind = None;
                    let mut name = None;
                    if !self.is_op(":") {
                        if self.is_op("(") {
                            return keep(line, "an `except` for several kinds of error at once");
                        }
                        if self.is_op("*") {
                            return keep(line, "`except*` has no words in Zeno");
                        }
                        let mut dotted = self.name()?;
                        while self.take_op(".") {
                            dotted = format!("{dotted}.{}", self.name()?);
                        }
                        kind = Some(dotted);
                        if self.take_word("as") {
                            name = Some(self.name()?);
                        }
                    }
                    handlers.push(Handler { kind, name, body: self.block()?, line });
                }
                if self.is_word("else") {
                    return keep(self.line(), "`else` after a `try` has no words in Zeno");
                }
                if self.is_word("finally") {
                    return keep(self.line(), "`finally` has no words in Zeno yet");
                }
                if handlers.is_empty() {
                    return keep(line, "a `try` with no `except`");
                }
                S::Try { body, handlers }
            }
            "with" => return keep(line, "`with` has no words in Zeno yet"),
            "async" => return keep(line, "`async` has no words in Zeno yet"),
            "match" if matches!(self.toks.get(self.at + 1).map(|tok| &tok.kind), Some(T::Name(_) | T::Number(_) | T::Str(_) | T::Op("("))) => {
                return keep(line, "Python's `match` is not translated yet")
            }
            _ => return self.simple_line(into),
        };
        into.push(Stmt { kind, line, last_line: self.line_before() });
        Ok(())
    }

    fn for_name(&mut self) -> Kept<String> {
        if self.is_op("(") || self.is_op("[") || self.is_op("*") {
            return keep(self.line(), "a `for` that takes each item apart in brackets");
        }
        self.name()
    }

    fn decorated(&mut self, into: &mut Vec<Stmt>) -> Kept<()> {
        let line = self.line();
        let mut decorators = Vec::new();
        while self.take_op("@") {
            decorators.push(self.expression()?);
            if !matches!(self.next(), T::Newline) {
                return keep(line, "a decorator the converter cannot read");
            }
        }
        if self.is_word("class") {
            let kind = self.class(decorators)?;
            into.push(Stmt { kind, line, last_line: self.line_before() });
            return Ok(());
        }
        let name = match &decorators[0].kind {
            E::Name(name) => format!("`@{name}`"),
            E::Attr(_, name) => format!("`@...{name}`"),
            _ => "a decorator".to_string(),
        };
        keep(line, format!("{name} changes the function below it, in a way only Python knows"))
    }

    fn def(&mut self) -> Kept<Def> {
        self.at += 1;
        let name = self.name()?;
        self.expect_op("(")?;
        let mut params = Vec::new();
        while !self.is_op(")") {
            let line = self.line();
            if self.is_op("*") || self.is_op("**") {
                return keep(line, "a parameter that takes any number of arguments (`*args`, `**kwargs`)");
            }
            if self.is_op("/") {
                return keep(line, "a `/` among the parameters");
            }
            let name = self.name()?;
            let annotation = if self.take_op(":") { Some(self.expression()?) } else { None };
            let default = if self.take_op("=") { Some(self.expression()?) } else { None };
            params.push(Param { name, annotation, default });
            if !self.take_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        let returns = if self.take_op("->") { Some(self.expression()?) } else { None };
        let body = self.block()?;
        Ok(Def { name, params, returns, body })
    }

    fn class(&mut self, decorators: Vec<Expr>) -> Kept<S> {
        self.at += 1;
        let name = self.name()?;
        let mut bases = Vec::new();
        if self.take_op("(") {
            while !self.is_op(")") {
                if matches!(self.toks.get(self.at + 1).map(|tok| &tok.kind), Some(T::Op("="))) {
                    return keep(self.line(), "a class with `metaclass=` or another named thing in its brackets");
                }
                bases.push(self.expression()?);
                if !self.take_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let body = self.block()?;
        Ok(S::Class { name, bases, decorators, body })
    }

    /// The statements of one line that has no block: one, or several
    /// with `;` between them.
    fn simple_line(&mut self, into: &mut Vec<Stmt>) -> Kept<()> {
        loop {
            let line = self.line();
            let kind = self.simple()?;
            into.push(Stmt { kind, line, last_line: self.line_before() });
            if !self.take_op(";") || matches!(self.peek(), T::Newline) {
                break;
            }
        }
        match self.next() {
            T::Newline | T::End => Ok(()),
            other => keep(self.line_before(), format!("{} where the line was expected to end", describe(&other))),
        }
    }

    fn simple(&mut self) -> Kept<S> {
        let line = self.line();
        let word = match self.peek() {
            T::Name(word) => word.clone(),
            _ => String::new(),
        };
        match word.as_str() {
            "pass" => {
                self.at += 1;
                return Ok(S::Pass);
            }
            "break" => {
                self.at += 1;
                return Ok(S::Break);
            }
            "continue" => {
                self.at += 1;
                return Ok(S::Continue);
            }
            "return" => {
                self.at += 1;
                if matches!(self.peek(), T::Newline | T::End | T::Op(";")) {
                    return Ok(S::Return(None));
                }
                return Ok(S::Return(Some(self.expressions()?)));
            }
            "raise" => {
                self.at += 1;
                if matches!(self.peek(), T::Newline | T::End | T::Op(";")) {
                    return Ok(S::Raise(None));
                }
                let error = self.expression()?;
                if self.is_word("from") {
                    return keep(line, "`raise ... from ...` has no words in Zeno");
                }
                return Ok(S::Raise(Some(error)));
            }
            "global" => {
                self.at += 1;
                let mut names = vec![self.name()?];
                while self.take_op(",") {
                    names.push(self.name()?);
                }
                return Ok(S::Global(names));
            }
            "nonlocal" => return keep(line, "`nonlocal` is not translated yet"),
            "del" => {
                self.at += 1;
                let target = self.expression()?;
                if self.is_op(",") {
                    return keep(line, "a `del` of several things at once");
                }
                return Ok(S::Delete(target));
            }
            "assert" => return keep(line, "`assert` outside a test has no words in Zeno"),
            "yield" => return keep(line, "`yield` makes a generator, which Zeno does not have"),
            "await" => return keep(line, "`await` has no words in Zeno yet"),
            "import" => {
                self.at += 1;
                let mut module = self.name()?;
                while self.take_op(".") {
                    module = format!("{module}.{}", self.name()?);
                }
                let name = if self.take_word("as") { self.name()? } else { module.clone() };
                if self.is_op(",") {
                    return keep(line, "several modules imported on one line");
                }
                return Ok(S::Import { module, name });
            }
            "from" => {
                self.at += 1;
                if self.is_op(".") || self.is_op("...") {
                    return keep(line, "an import from the package the file is in");
                }
                let mut module = self.name()?;
                while self.take_op(".") {
                    module = format!("{module}.{}", self.name()?);
                }
                if !self.take_word("import") {
                    return keep(line, "a `from` with no `import`");
                }
                if self.is_op("*") {
                    return keep(line, "`import *`: the names it brings are not written anywhere");
                }
                let bracket = self.take_op("(");
                let mut names = Vec::new();
                loop {
                    let name = self.name()?;
                    let known_as = if self.take_word("as") { self.name()? } else { name.clone() };
                    names.push((name, known_as));
                    if !self.take_op(",") || self.is_op(")") {
                        break;
                    }
                }
                if bracket {
                    self.expect_op(")")?;
                }
                return Ok(S::FromImport { module, names });
            }
            _ => {}
        }
        let first = self.expressions()?;
        if self.take_op(":") {
            let annotation = self.expression()?;
            let value = if self.take_op("=") { Some(self.expressions()?) } else { None };
            return Ok(S::Annotated { target: first, annotation, value });
        }
        for op in ["+=", "-=", "*=", "/=", "//=", "%=", "**=", "&=", "|=", "^=", "<<=", ">>="] {
            if self.take_op(op) {
                let value = self.expressions()?;
                let op = OPERATORS.iter().find(|known| **known == &op[..op.len() - 1]).copied().unwrap_or("+");
                return Ok(S::Augmented { target: first, op, value });
            }
        }
        if self.is_op("@=") {
            return keep(line, "`@=` has no words in Zeno");
        }
        if self.take_op("=") {
            let value = self.expressions()?;
            if self.is_op("=") {
                return keep(line, "one value given to several names at once (`a = b = 0`)");
            }
            return Ok(S::Assign { target: first, value });
        }
        Ok(S::Expr(first))
    }

    /// One expression, or several with commas between them, which is a
    /// tuple.
    fn expressions(&mut self) -> Kept<Expr> {
        let line = self.line();
        if self.is_op("*") {
            return keep(line, "a `*` that spreads a list out");
        }
        let first = self.expression()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.take_op(",") {
            if matches!(self.peek(), T::Newline | T::End | T::Op("=" | ")" | ":" | ";")) {
                break;
            }
            if self.is_op("*") {
                return keep(line, "a `*` that spreads a list out");
            }
            items.push(self.expression()?);
        }
        Ok(Expr { kind: E::Tuple(items), line })
    }

    pub fn expression(&mut self) -> Kept<Expr> {
        let line = self.line();
        if self.is_word("lambda") {
            self.at += 1;
            let mut names = Vec::new();
            while !self.is_op(":") {
                if self.is_op("*") || self.is_op("**") {
                    return keep(line, "a `lambda` that takes any number of arguments");
                }
                names.push(self.name()?);
                if self.is_op("=") {
                    return keep(line, "a `lambda` with a default for a parameter");
                }
                if !self.take_op(",") {
                    break;
                }
            }
            self.expect_op(":")?;
            let body = self.expression()?;
            return Ok(Expr { kind: E::Lambda(names, Box::new(body)), line });
        }
        let then = self.or()?;
        if self.is_op(":=") {
            return keep(line, "`:=` gives a name a value in the middle of a line, which Zeno does not do");
        }
        if !self.take_word("if") {
            return Ok(then);
        }
        let condition = self.or()?;
        if !self.take_word("else") {
            return keep(line, "an `if` in the middle of a line with no `else`");
        }
        let otherwise = self.expression()?;
        Ok(Expr { kind: E::IfElse { then: Box::new(then), condition: Box::new(condition), otherwise: Box::new(otherwise) }, line })
    }

    fn or(&mut self) -> Kept<Expr> {
        let mut left = self.and()?;
        while self.take_word("or") {
            let line = left.line;
            left = Expr { kind: E::Logic("or", Box::new(left), Box::new(self.and()?)), line };
        }
        Ok(left)
    }

    fn and(&mut self) -> Kept<Expr> {
        let mut left = self.not()?;
        while self.take_word("and") {
            let line = left.line;
            left = Expr { kind: E::Logic("and", Box::new(left), Box::new(self.not()?)), line };
        }
        Ok(left)
    }

    fn not(&mut self) -> Kept<Expr> {
        let line = self.line();
        if self.take_word("not") {
            return Ok(Expr { kind: E::Unary("not", Box::new(self.not()?)), line });
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Kept<Expr> {
        let first = self.binary(0)?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek() {
                T::Op(op @ ("==" | "!=" | "<" | "<=" | ">" | ">=")) => {
                    let op = *op;
                    self.at += 1;
                    op
                }
                T::Name(word) if word == "in" => {
                    self.at += 1;
                    "in"
                }
                T::Name(word) if word == "is" => {
                    self.at += 1;
                    if self.take_word("not") {
                        "is not"
                    } else {
                        "is"
                    }
                }
                T::Name(word) if word == "not" && matches!(self.toks.get(self.at + 1).map(|tok| &tok.kind), Some(T::Name(next)) if next == "in") => {
                    self.at += 2;
                    "not in"
                }
                _ => break,
            };
            rest.push((op, self.binary(0)?));
        }
        if rest.is_empty() {
            return Ok(first);
        }
        let line = first.line;
        Ok(Expr { kind: E::Compare(Box::new(first), rest), line })
    }

    /// The operators that take two numbers, from the loosest (`|`) to the
    /// tightest (`*`), each level joining to the left.
    fn binary(&mut self, level: usize) -> Kept<Expr> {
        const LEVELS: &[&[&str]] = &[&["|"], &["^"], &["&"], &["<<", ">>"], &["+", "-"], &["*", "/", "//", "%", "@"]];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        loop {
            let Some(op) = LEVELS[level].iter().find(|op| self.is_op(op)).copied() else {
                return Ok(left);
            };
            let line = self.line();
            self.at += 1;
            if op == "@" {
                return keep(line, "`@` between two values has no words in Zeno");
            }
            let right = self.binary(level + 1)?;
            let line = left.line;
            left = Expr { kind: E::Binary(op, Box::new(left), Box::new(right)), line };
        }
    }

    fn unary(&mut self) -> Kept<Expr> {
        let line = self.line();
        for op in ["-", "+", "~"] {
            if self.take_op(op) {
                return Ok(Expr { kind: E::Unary(OPERATORS.iter().find(|known| **known == op).copied().unwrap_or("-"), Box::new(self.unary()?)), line });
            }
        }
        let base = self.postfix()?;
        if self.take_op("**") {
            // `**` joins to the right, and takes a sign on its right.
            let power = self.unary()?;
            return Ok(Expr { kind: E::Binary("**", Box::new(base), Box::new(power)), line });
        }
        Ok(base)
    }

    /// `item for name in items if condition`, the `for` read already.
    fn list_from_loop(&mut self, item: Expr, line: usize) -> Kept<Expr> {
        let name = self.for_name()?;
        if self.is_op(",") {
            return keep(line, "a list made from a loop over pairs");
        }
        if !self.take_word("in") {
            return keep(line, "a list made from a loop the converter cannot read");
        }
        let items = self.or()?;
        let condition = if self.take_word("if") { Some(Box::new(self.or()?)) } else { None };
        if self.is_word("for") || self.is_word("if") {
            return keep(line, "a list made from two loops, or with two conditions");
        }
        Ok(Expr { kind: E::ListOf { item: Box::new(item), name, items: Box::new(items), condition }, line })
    }

    fn postfix(&mut self) -> Kept<Expr> {
        let mut value = self.atom()?;
        loop {
            let line = self.line();
            if self.take_op(".") {
                let name = self.name()?;
                let at = value.line;
                value = Expr { kind: E::Attr(Box::new(value), name), line: at };
            } else if self.take_op("(") {
                let mut args = Vec::new();
                let mut named = Vec::new();
                while !self.is_op(")") {
                    if self.is_op("*") || self.is_op("**") {
                        return keep(line, "a call that spreads a list or a dict out into arguments");
                    }
                    let is_named = matches!(self.peek(), T::Name(_)) && matches!(self.toks.get(self.at + 1).map(|tok| &tok.kind), Some(T::Op("=")));
                    if is_named {
                        let name = self.name()?;
                        self.at += 1;
                        named.push((name, self.expression()?));
                    } else {
                        if !named.is_empty() {
                            return keep(line, "an argument with no name after one with a name");
                        }
                        let mut arg = self.expression()?;
                        if self.is_word("for") {
                            // `sum(x for x in xs)`: these take every item
                            // and give nothing back until they have, so a
                            // list of the items is the same to them.
                            let takes_all = match &value.kind {
                                E::Name(name) => matches!(name.as_str(), "sum" | "min" | "max" | "sorted" | "list"),
                                E::Attr(_, method) => method == "join",
                                _ => false,
                            };
                            if !takes_all || !args.is_empty() {
                                return keep(line, "a generator in the brackets of a call, other than of `sum`, `min`, `max`, `sorted`, `list` and `join`");
                            }
                            self.at += 1;
                            arg = self.list_from_loop(arg, line)?;
                            if !self.is_op(")") {
                                return keep(line, "a generator in the brackets of a call, with more after it");
                            }
                        }
                        args.push(arg);
                    }
                    if !self.take_op(",") {
                        break;
                    }
                }
                self.expect_op(")")?;
                let at = value.line;
                value = Expr { kind: E::Call { function: Box::new(value), args, named }, line: at };
            } else if self.take_op("[") {
                let from = if self.is_op(":") { None } else { Some(Box::new(self.expressions()?)) };
                if self.take_op(":") {
                    let to = if self.is_op("]") || self.is_op(":") { None } else { Some(Box::new(self.expression()?)) };
                    if self.is_op(":") {
                        return keep(line, "a slice with a step (`xs[::2]`)");
                    }
                    self.expect_op("]")?;
                    let at = value.line;
                    value = Expr { kind: E::Slice { of: Box::new(value), from, to }, line: at };
                } else {
                    self.expect_op("]")?;
                    let Some(index) = from else {
                        return keep(line, "empty square brackets");
                    };
                    let at = value.line;
                    value = Expr { kind: E::Index(Box::new(value), index), line: at };
                }
            } else {
                return Ok(value);
            }
        }
    }

    fn atom(&mut self) -> Kept<Expr> {
        let line = self.line();
        let kind = match self.next() {
            T::Name(name) => match name.as_str() {
                "True" => E::True,
                "False" => E::False,
                "None" => E::None,
                "await" => return keep(line, "`await` has no words in Zeno yet"),
                "yield" => return keep(line, "`yield` makes a generator, which Zeno does not have"),
                word if is_python_keyword(word) => return keep(line, format!("`{word}` where a value was expected")),
                _ => E::Name(name),
            },
            T::Number(text) => E::Number(text),
            T::Str(lit) => {
                if matches!(self.peek(), T::Str(_)) {
                    return keep(line, "two texts side by side, which Python joins into one");
                }
                E::Str(lit)
            }
            T::Op("(") => {
                if self.take_op(")") {
                    return Ok(Expr { kind: E::Tuple(Vec::new()), line });
                }
                if self.is_op("*") {
                    return keep(line, "a `*` that spreads a list out");
                }
                let first = self.expression()?;
                if self.is_word("for") {
                    return keep(line, "a generator (`(x for x in xs)`), which Zeno does not have");
                }
                if self.take_op(")") {
                    return Ok(first);
                }
                let mut items = vec![first];
                while self.take_op(",") {
                    if self.is_op(")") {
                        break;
                    }
                    items.push(self.expression()?);
                }
                self.expect_op(")")?;
                E::Tuple(items)
            }
            T::Op("[") => {
                let mut items = Vec::new();
                if self.take_op("]") {
                    return Ok(Expr { kind: E::List(items), line });
                }
                if self.is_op("*") {
                    return keep(line, "a `*` that spreads a list out");
                }
                let first = self.expression()?;
                if self.take_word("for") {
                    let made = self.list_from_loop(first, line)?;
                    self.expect_op("]")?;
                    return Ok(made);
                }
                items.push(first);
                while self.take_op(",") {
                    if self.is_op("]") {
                        break;
                    }
                    if self.is_op("*") {
                        return keep(line, "a `*` that spreads a list out");
                    }
                    items.push(self.expression()?);
                }
                self.expect_op("]")?;
                E::List(items)
            }
            T::Op("{") => {
                let mut pairs = Vec::new();
                if self.take_op("}") {
                    return Ok(Expr { kind: E::Dict(pairs), line });
                }
                loop {
                    if self.is_op("**") {
                        return keep(line, "a `**` that spreads a dict out");
                    }
                    let key = self.expression()?;
                    if !self.take_op(":") {
                        return keep(line, "a set (`{1, 2}`), which Zeno does not have");
                    }
                    let value = self.expression()?;
                    if self.is_word("for") {
                        return keep(line, "a dict made from a loop");
                    }
                    pairs.push((key, value));
                    if !self.take_op(",") || self.is_op("}") {
                        break;
                    }
                }
                self.expect_op("}")?;
                E::Dict(pairs)
            }
            T::Op("...") => return keep(line, "`...`, which stands for nothing yet"),
            other => return keep(line, format!("{} where a value was expected", describe(&other))),
        };
        Ok(Expr { kind, line })
    }
}

fn describe(kind: &T) -> String {
    match kind {
        T::Name(name) => format!("`{name}`"),
        T::Number(text) => format!("`{text}`"),
        T::Str(_) => "a text".to_string(),
        T::Op(op) => format!("`{op}`"),
        T::Newline | T::End => "the end of the line".to_string(),
        T::Indent => "a line further in".to_string(),
        T::Dedent => "a line further out".to_string(),
    }
}

pub fn is_python_keyword(word: &str) -> bool {
    matches!(
        word,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

/// The expression written in the braces of an f-string, or anywhere
/// else a piece of text holds one.
pub fn expression_of(text: &str, line: usize) -> Kept<Expr> {
    let lexed = lex(text).map_err(|keep| Keep { line, why: keep.why })?;
    let mut parser = Parser::new(&lexed.toks);
    let mut expr = parser.expressions().map_err(|keep| Keep { line, why: keep.why })?;
    if !matches!(parser.peek(), T::Newline | T::End) {
        return keep(line, format!("{} in the braces of an f-string", describe(parser.peek())));
    }
    relined(&mut expr, line);
    Ok(expr)
}

/// Give every part of an expression one line: the line of the text it
/// was written in.
fn relined(expr: &mut Expr, line: usize) {
    expr.line = line;
    match &mut expr.kind {
        E::List(items) | E::Tuple(items) => items.iter_mut().for_each(|item| relined(item, line)),
        E::Dict(pairs) => pairs.iter_mut().for_each(|(key, value)| {
            relined(key, line);
            relined(value, line);
        }),
        E::Unary(_, of) | E::Attr(of, _) | E::Lambda(_, of) => relined(of, line),
        E::Binary(_, left, right) | E::Logic(_, left, right) | E::Index(left, right) => {
            relined(left, line);
            relined(right, line);
        }
        E::Compare(first, rest) => {
            relined(first, line);
            rest.iter_mut().for_each(|(_, other)| relined(other, line));
        }
        E::Call { function, args, named } => {
            relined(function, line);
            args.iter_mut().for_each(|arg| relined(arg, line));
            named.iter_mut().for_each(|(_, arg)| relined(arg, line));
        }
        E::Slice { of, from, to } => {
            relined(of, line);
            from.iter_mut().for_each(|part| relined(part, line));
            to.iter_mut().for_each(|part| relined(part, line));
        }
        E::IfElse { then, condition, otherwise } => {
            relined(then, line);
            relined(condition, line);
            relined(otherwise, line);
        }
        E::ListOf { item, items, condition, .. } => {
            relined(item, line);
            relined(items, line);
            condition.iter_mut().for_each(|part| relined(part, line));
        }
        E::Name(_) | E::Number(_) | E::Str(_) | E::True | E::False | E::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<T> {
        lex(source).unwrap_or_else(|keep| panic!("{}", keep.why)).toks.into_iter().map(|tok| tok.kind).collect()
    }

    fn tree(source: &str) -> Kept<Vec<Stmt>> {
        let lexed = lex(source).unwrap_or_else(|keep| panic!("{}", keep.why));
        Parser::new(&lexed.toks).piece()
    }

    #[test]
    fn indentation_becomes_tokens_and_brackets_join_lines() {
        let found = kinds("if x:\n    y = [1,\n  2]\n\n    # a comment\nz = 3\n");
        let name = |name: &str| T::Name(name.to_string());
        let number = |text: &str| T::Number(text.to_string());
        assert_eq!(
            found,
            vec![
                name("if"),
                name("x"),
                T::Op(":"),
                T::Newline,
                T::Indent,
                name("y"),
                T::Op("="),
                T::Op("["),
                number("1"),
                T::Op(","),
                number("2"),
                T::Op("]"),
                T::Newline,
                T::Dedent,
                name("z"),
                T::Op("="),
                number("3"),
                T::Newline,
                T::End
            ]
        );
    }

    #[test]
    fn a_text_keeps_what_was_written_and_counts_its_lines() {
        let lexed = lex("a = '''one\ntwo'''\nb = rb'\\d' + f\"{x!r}\"\n# last\n").unwrap();
        let texts: Vec<&StrLit> = lexed.toks.iter().filter_map(|tok| if let T::Str(lit) = &tok.kind { Some(lit) } else { None }).collect();
        assert_eq!(texts[0], &StrLit { prefix: String::new(), body: "one\ntwo".to_string(), triple: true });
        assert_eq!(texts[1], &StrLit { prefix: "rb".to_string(), body: "\\d".to_string(), triple: false });
        assert_eq!(texts[2].prefix, "f");
        let b = lexed.toks.iter().find(|tok| tok.kind == T::Name("b".to_string())).unwrap();
        assert_eq!(b.line, 3);
        assert_eq!(lexed.comments, vec![Comment { line: 4, text: " last".to_string(), alone: true }]);
        assert!(lex("a = 'no end\n").is_err());
        assert!(lex("if x:\n\ty = 1\n").is_err());
    }

    #[test]
    fn a_file_is_cut_at_each_statement_at_the_margin() {
        let source = "import os\n\n# about f\n# more\n@decorated\ndef f():\n    return 1\n    # inside\n\nif a:\n    pass\nelse:\n    pass\nx = 1; y = 2\n";
        let lexed = lex(source).unwrap();
        let found: Vec<(usize, usize, usize)> = pieces(&lexed).iter().map(|piece| (piece.first_line, piece.line, piece.last_line)).collect();
        assert_eq!(found, vec![(1, 1, 1), (3, 5, 7), (10, 10, 13), (14, 14, 14)]);
    }

    #[test]
    fn the_tree_follows_what_binds_tighter_in_python() {
        let Ok(found) = tree("x = -a ** 2 + b * c < d and not e or f\n") else { panic!() };
        let S::Assign { value, .. } = &found[0].kind else { panic!() };
        let E::Logic("or", left, _) = &value.kind else { panic!("{value:?}") };
        let E::Logic("and", compare, not) = &left.kind else { panic!() };
        assert!(matches!(&not.kind, E::Unary("not", _)));
        let E::Compare(sum, rest) = &compare.kind else { panic!() };
        assert_eq!(rest[0].0, "<");
        let E::Binary("+", minus, _) = &sum.kind else { panic!() };
        let E::Unary("-", power) = &minus.kind else { panic!() };
        assert!(matches!(&power.kind, E::Binary("**", _, _)));
    }

    #[test]
    fn what_the_converter_does_not_know_is_kept_with_its_line_and_a_reason() {
        for (source, line, says) in [
            ("def f():\n    yield 1\n", 2, "generator"),
            ("with open(p) as f:\n    pass\n", 1, "`with`"),
            ("def f(*args):\n    pass\n", 1, "any number of arguments"),
            ("x = {1, 2}\n", 1, "a set"),
            ("try:\n    pass\nfinally:\n    pass\n", 3, "`finally`"),
            ("a = b = 0\n", 1, "several names"),
            ("x = [a for a in b for c in d]\n", 1, "two loops"),
            ("@cache\ndef f():\n    pass\n", 1, "`@cache`"),
            ("print(x for x in y)\n", 1, "generator"),
            ("sum((x for x in y), 1)\n", 1, "generator"),
            ("xs[::2]\n", 1, "step"),
        ] {
            let Err(keep) = tree(source) else { panic!("{source} was read") };
            assert_eq!(keep.line, line, "{source}: {}", keep.why);
            assert!(keep.why.contains(says), "{source}: {}", keep.why);
        }
    }

    #[test]
    fn statements_are_read_with_their_lines() {
        let source = "def f(a, b: int = 2) -> str:\n    for i, x in enumerate(a):\n        if x is not None and i not in b:\n            return 'y'\n    try:\n        a.b[0] += 1\n    except ValueError as e:\n        raise\n    return a if b else None\n";
        let found = tree(source).unwrap_or_else(|keep| panic!("line {}: {}", keep.line, keep.why));
        let S::Def(def) = &found[0].kind else { panic!() };
        assert_eq!(def.params.len(), 2);
        assert!(def.params[1].annotation.is_some() && def.params[1].default.is_some());
        assert_eq!(found[0].last_line, 9);
        let S::For { names, body, .. } = &def.body[0].kind else { panic!() };
        assert_eq!(names, &["i".to_string(), "x".to_string()]);
        let S::If { branches, .. } = &body[0].kind else { panic!() };
        let E::Logic("and", left, right) = &branches[0].0.kind else { panic!() };
        assert!(matches!(&left.kind, E::Compare(_, rest) if rest[0].0 == "is not"));
        assert!(matches!(&right.kind, E::Compare(_, rest) if rest[0].0 == "not in"));
        let S::Try { handlers, .. } = &def.body[1].kind else { panic!() };
        assert_eq!(handlers[0].kind.as_deref(), Some("ValueError"));
        assert!(matches!(&def.body[2].kind, S::Return(Some(Expr { kind: E::IfElse { .. }, .. }))));
    }
}
