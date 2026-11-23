//! The lexer: turns source text into tokens.
//!
//! It reads the source one character at a time, left to right, and never
//! looks more than two characters ahead. The whole thing is one loop
//! (`Lexer::run`) that looks at the next character and decides which kind of
//! token starts there.

use crate::error::Error;
use crate::token::{keyword, Span, Token, TokenKind};

/// Turn a whole source file into tokens.
///
/// On success the list always ends with `Eof`. On failure you get every error
/// found, not just the first, so the programmer can fix them in one go.
pub fn lex(source: &str) -> Result<Vec<Token>, Vec<Error>> {
    lex_in(source, 0)
}

/// The same for one file of a program that has several. `file` is the
/// file's number, counted from 0, and it goes into every line number:
/// see `Span`.
pub fn lex_in(source: &str, file: usize) -> Result<Vec<Token>, Vec<Error>> {
    let mut lexer = Lexer::new(source);
    lexer.line += file * crate::token::LINES_A_FILE;
    lexer.run();
    if lexer.errors.is_empty() {
        Ok(lexer.tokens)
    } else {
        Err(lexer.errors)
    }
}

/// A program as it was written: the tokens, and beside them what the
/// tokens alone do not say. `zeno format` works from this.
pub struct Written {
    pub tokens: Vec<Token>,
    /// The text of each token, as it stands in the source: `0x4000_4400`
    /// for the number, the quotes and all for a string. One for each of
    /// `tokens`, and empty for a line break and the end of the file.
    pub texts: Vec<String>,
    /// Every comment, from its `#` to the end of its line, and where it is.
    pub comments: Vec<(Span, String)>,
}

/// Turn a source file into tokens, and keep how each was written.
pub fn lex_written(source: &str) -> Result<Written, Vec<Error>> {
    let mut lexer = Lexer::new(source);
    lexer.run();
    if !lexer.errors.is_empty() {
        return Err(lexer.errors);
    }
    let texts = lexer.extents.iter().map(|&(from, to)| lexer.chars[from..to].iter().collect()).collect();
    Ok(Written { tokens: lexer.tokens, texts, comments: lexer.comments })
}

struct Lexer {
    chars: Vec<char>,
    /// Index into `chars` of the next unread character.
    pos: usize,
    /// Line and column of the next unread character (both start at 1).
    line: usize,
    col: usize,
    /// How many `(` or `[` are currently open. Line breaks inside them are
    /// ignored, so a long call or list can be split over several lines.
    depth: usize,
    /// The value of `depth` outside each `{` that is currently open. Inside
    /// braces line breaks matter again, even if the braces are themselves
    /// inside brackets, as with a function written as an argument.
    outer_depths: Vec<usize>,
    tokens: Vec<Token>,
    /// Where in `chars` the token being read began.
    started: usize,
    /// Where in `chars` each token began and ended: one for each of `tokens`.
    extents: Vec<(usize, usize)>,
    comments: Vec<(Span, String)>,
    errors: Vec<Error>,
}

/// A whole number past the largest a u64 holds (decision 42.5).
const TOO_BIG: &str = "this number is too big. The largest whole number Zeno holds is 18446744073709551615, the largest u64";

impl Lexer {
    fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            depth: 0,
            outer_depths: Vec::new(),
            tokens: Vec::new(),
            started: 0,
            extents: Vec::new(),
            comments: Vec::new(),
            errors: Vec::new(),
        }
    }

    // ----- Reading characters -----

    /// The next character, without consuming it.
    fn peek(&self) -> Option<char> {
        self.peek_at(0)
    }

    /// The character `offset` places ahead, without consuming anything.
    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Consume one character and keep the line/column counters in step.
    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    /// Consume the next character only if it is `expected`.
    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    // ----- Producing output -----

    fn push(&mut self, kind: TokenKind, span: Span) {
        // A token is pushed when its last character has been read.
        let written = match kind {
            TokenKind::Newline | TokenKind::Eof => (self.pos, self.pos),
            _ => (self.started, self.pos),
        };
        self.extents.push(written);
        self.tokens.push(Token { kind, span });
    }

    fn error(&mut self, message: impl Into<String>, span: Span) {
        self.errors.push(Error::new(message, span));
    }

    // ----- The main loop -----

    fn run(&mut self) {
        while let Some(c) = self.peek() {
            // Remember where this token starts before consuming anything.
            let span = Span { line: self.line, col: self.col };
            self.started = self.pos;

            match c {
                ' ' | '\t' | '\r' => {
                    self.advance();
                }
                '\n' => {
                    self.advance();
                    self.end_statement(span);
                }
                '#' => self.comment(span),
                '"' | '\'' => self.string(false, span),
                'f' if matches!(self.peek_at(1), Some('"' | '\'')) => {
                    self.advance(); // the f
                    self.string(true, span);
                }
                c if c.is_ascii_digit() => self.number(span),
                c if c.is_alphabetic() || c == '_' => self.word(span),
                _ => self.symbol(span),
            }
        }

        // A file that doesn't end in a line break still ends its last statement.
        let end = Span { line: self.line, col: self.col };
        self.end_statement(end);
        self.push(TokenKind::Eof, end);
    }

    /// Called at every line break. Emits a `Newline` token only when the line
    /// break really ends a statement.
    fn end_statement(&mut self, span: Span) {
        if self.depth > 0 {
            return; // inside ( ) or [ ]
        }
        match self.tokens.last() {
            None => {}                                         // nothing before it
            Some(t) if t.kind == TokenKind::Newline => {}      // blank line
            Some(t) if t.kind.continues_line() => {}           // `a +` <newline> `b`
            Some(_) => self.push(TokenKind::Newline, span),
        }
    }

    /// `# ...` runs to the end of the line. The line break itself is left for
    /// the main loop, because it may still end a statement.
    fn comment(&mut self, span: Span) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.advance();
        }
        // The program does not need the comment. `zeno format` does.
        let text: String = self.chars[self.started..self.pos].iter().collect();
        self.comments.push((span, text.trim_end().to_string()));
    }

    /// A keyword or an identifier: letters, digits and underscores.
    fn word(&mut self, span: Span) {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if text == "inline" && self.inline_block(span) {
            return;
        }
        let kind = keyword(&text).unwrap_or(TokenKind::Ident(text));
        self.push(kind, span);
    }

    /// `inline c {` at the end of a line begins a block of another
    /// language (decision 61). `inline` is not a word of Zeno: it is a
    /// name like any other, unless a name and a `{` that ends the line
    /// come after it, which nothing else in Zeno can be. This is the one
    /// place where the lexer looks further ahead than two characters,
    /// and it never looks past the end of the line.
    ///
    /// What follows is not Zeno, and is not read as Zeno: every line is
    /// taken as it is, up to a line that holds only `}`, as far in as
    /// the line that opened the block. The lines between are further in
    /// than that, so no `}` of the other language can be taken for the
    /// end, whatever its strings and comments look like.
    fn inline_block(&mut self, span: Span) -> bool {
        // Is it `inline name {` and then the end of the line?
        let blank = |c: char| c == ' ' || c == '\t';
        let mut at = self.pos;
        while self.chars.get(at).copied().is_some_and(blank) {
            at += 1;
        }
        let name_from = at;
        while self.chars.get(at).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
            at += 1;
        }
        if at == name_from || self.chars[name_from].is_ascii_digit() {
            return false;
        }
        let language: String = self.chars[name_from..at].iter().collect();
        while self.chars.get(at).copied().is_some_and(blank) {
            at += 1;
        }
        if self.chars.get(at) != Some(&'{') {
            return false;
        }
        at += 1;
        while self.chars.get(at).is_some_and(|c| blank(*c) || *c == '\r') {
            at += 1;
        }
        if !matches!(self.chars.get(at), None | Some('\n')) {
            return false;
        }

        // How far in the line that opens the block is.
        let mut line_from = self.started;
        while line_from > 0 && self.chars[line_from - 1] != '\n' {
            line_from -= 1;
        }
        let indent: String = self.chars[line_from..].iter().take_while(|c| blank(**c)).collect();

        // The rest of the opening line.
        while self.pos < at {
            self.advance();
        }
        self.advance(); // its line break, if the file goes on

        let mut lines: Vec<String> = Vec::new();
        let mut closed = false;
        let mut said = false;
        while self.pos < self.chars.len() {
            let here = Span { line: self.line, col: self.col };
            let mut end = self.pos;
            while end < self.chars.len() && self.chars[end] != '\n' {
                end += 1;
            }
            let line: String = self.chars[self.pos..end].iter().collect();
            let line = line.trim_end();
            if line.strip_prefix(indent.as_str()) == Some("}") {
                // The `}` itself is read, and what is left of its line
                // is Zeno again.
                for _ in 0..indent.chars().count() + 1 {
                    self.advance();
                }
                closed = true;
                break;
            }
            let further = line.strip_prefix(indent.as_str()).is_some_and(|rest| rest.starts_with(blank));
            if !line.is_empty() && !further && !said {
                said = true;
                self.error(
                    format!(
                        "this line of the `inline {language}` block is not further in than the line that opens the block. \
                         Everything in the block is indented, and the block ends at a line that holds only `}}`, \
                         as far in as the line that opened it"
                    ),
                    Span { line: here.line, col: line.chars().take_while(|c| blank(*c)).count() + 1 },
                );
            }
            lines.push(line.to_string());
            while self.pos < end {
                self.advance();
            }
            self.advance(); // the line break
        }
        if !closed && !said {
            self.error(
                format!(
                    "this `inline {language}` block has no end. It ends at a line that holds only `}}`, \
                     as far in as the line that opened it"
                ),
                span,
            );
        }

        // The indentation every line of the block shares is not part of
        // what the block says.
        let shared = lines
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| line.chars().take_while(|c| blank(*c)).collect::<String>())
            .reduce(|shared, own| shared.chars().zip(own.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect())
            .unwrap_or_default();
        let mut text = String::new();
        for line in &lines {
            text.push_str(line.strip_prefix(shared.as_str()).unwrap_or(line));
            text.push('\n');
        }
        self.push(TokenKind::Inline { language, text }, span);
        true
    }

    /// A number: `42`, `1_000`, `3.14`, `1e-3`, `0xFF`, `0b1010`, `0o17`.
    fn number(&mut self, span: Span) {
        // Hex, binary and octal start with 0x, 0b, 0o.
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some('0'), Some('x' | 'X')) => Some((16, "hexadecimal")),
            (Some('0'), Some('b' | 'B')) => Some((2, "binary")),
            (Some('0'), Some('o' | 'O')) => Some((8, "octal")),
            _ => None,
        };
        if let Some((radix, name)) = radix {
            self.advance(); // 0
            self.advance(); // x, b or o
            let digits = self.digits(|c| c.is_ascii_alphanumeric());
            match u64::from_str_radix(&digits, radix) {
                Ok(n) => self.push(TokenKind::Int(n), span),
                Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => self.error(TOO_BIG, span),
                Err(_) => self.error(format!("invalid {name} number"), span),
            }
            return;
        }

        let mut text = self.digits(|c| c.is_ascii_digit());
        let mut is_float = false;

        // A fraction needs a digit after the dot, so `x.0` is a float
        // but `list.len` after a number is not swallowed.
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            self.advance();
            text.push('.');
            text += &self.digits(|c| c.is_ascii_digit());
        }

        // An exponent: e or E, an optional sign, then at least one digit.
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign = matches!(self.peek_at(1), Some('+' | '-'));
            let first_digit = self.peek_at(if sign { 2 } else { 1 });
            if first_digit.is_some_and(|c| c.is_ascii_digit()) {
                is_float = true;
                text.push('e');
                self.advance();
                if sign {
                    text.push(self.advance().unwrap());
                }
                text += &self.digits(|c| c.is_ascii_digit());
            }
        }

        if is_float {
            match text.parse::<f64>() {
                Ok(x) => self.push(TokenKind::Float(x), span),
                Err(_) => self.error("invalid number", span),
            }
        } else {
            match text.parse::<u64>() {
                Ok(n) => self.push(TokenKind::Int(n), span),
                Err(_) => self.error(TOO_BIG, span),
            }
        }
    }

    /// Consume characters while `accept` says yes, dropping `_` separators
    /// so that `1_000_000` reads as 1000000.
    fn digits(&mut self, accept: impl Fn(char) -> bool) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c == '_' {
                self.advance();
            } else if accept(c) {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }
        text
    }

    /// A string in single or double quotes. It must close on the same line.
    fn string(&mut self, formatted: bool, span: Span) {
        let quote = self.advance().unwrap();
        let mut value = String::new();

        loop {
            let here = Span { line: self.line, col: self.col };
            match self.peek() {
                None | Some('\n') => {
                    self.error("string is missing its closing quote", span);
                    return;
                }
                Some(c) if c == quote => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.advance() {
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('r') => value.push('\r'),
                        Some('0') => value.push('\0'),
                        Some(c @ ('\\' | '"' | '\'')) => value.push(c),
                        Some(c) => self.error(format!("unknown escape `\\{c}`"), here),
                        None => {} // reported as a missing quote on the next turn
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.advance();
                }
            }
        }

        self.push(TokenKind::Str { value, formatted }, span);
    }

    /// Punctuation and operators. Two-character ones (`==`, `->`, `**`, ...)
    /// are found by checking the character after the first.
    fn symbol(&mut self, span: Span) {
        use TokenKind::*;
        let c = self.advance().unwrap();

        let kind = match c {
            '(' => {
                self.depth += 1;
                LParen
            }
            ')' => {
                self.depth = self.depth.saturating_sub(1);
                RParen
            }
            '[' => {
                self.depth += 1;
                LBracket
            }
            ']' => {
                self.depth = self.depth.saturating_sub(1);
                RBracket
            }
            '{' => {
                self.outer_depths.push(self.depth);
                self.depth = 0;
                LBrace
            }
            '}' => {
                self.depth = self.outer_depths.pop().unwrap_or(0);
                RBrace
            }
            ',' => Comma,
            '.' => Dot,
            ':' => Colon,
            '%' => if self.eat('=') { PercentAssign } else { Percent },
            '+' => if self.eat('=') { PlusAssign } else { Plus },
            '-' => {
                if self.eat('>') {
                    Arrow
                } else if self.eat('=') {
                    MinusAssign
                } else {
                    Minus
                }
            }
            '*' => {
                if self.eat('*') {
                    if self.eat('=') { StarStarAssign } else { StarStar }
                } else if self.eat('=') {
                    StarAssign
                } else {
                    Star
                }
            }
            '/' => {
                if self.eat('/') {
                    if self.eat('=') { SlashSlashAssign } else { SlashSlash }
                } else if self.eat('=') {
                    SlashAssign
                } else {
                    Slash
                }
            }
            '=' => if self.eat('=') { Eq } else { Assign },
            '<' => {
                if self.eat('<') {
                    if self.eat('=') { ShlAssign } else { Shl }
                } else if self.eat('=') {
                    LtEq
                } else {
                    Lt
                }
            }
            '>' => {
                if self.eat('>') {
                    if self.eat('=') { ShrAssign } else { Shr }
                } else if self.eat('=') {
                    GtEq
                } else {
                    Gt
                }
            }
            '!' => {
                if self.eat('=') {
                    NotEq
                } else {
                    return self.error("unexpected `!`. Zeno spells this `not`", span);
                }
            }

            // One `&` or `|` works on the bits of a whole number. Two is a
            // habit from other languages, and gets a hint.
            '&' => {
                if self.eat('&') {
                    return self.error("Zeno spells this `and`", span);
                }
                if self.eat('=') { AmpAssign } else { Amp }
            }
            '|' => {
                if self.eat('|') {
                    return self.error("Zeno spells this `or`", span);
                }
                if self.eat('=') { PipeAssign } else { Pipe }
            }
            '^' => if self.eat('=') { CaretAssign } else { Caret },
            '~' => Tilde,
            ';' => {
                return self.error("Zeno has no semicolons. A line break ends a statement", span);
            }
            other => return self.error(format!("unexpected character `{other}`"), span),
        };

        self.push(kind, span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    /// Lex `source` and return just the kinds, without the trailing Eof.
    fn kinds(source: &str) -> Vec<TokenKind> {
        let mut kinds: Vec<_> = lex(source).unwrap().into_iter().map(|t| t.kind).collect();
        assert_eq!(kinds.pop(), Some(Eof));
        kinds
    }

    fn ident(name: &str) -> TokenKind {
        Ident(name.to_string())
    }

    fn string(value: &str) -> TokenKind {
        Str { value: value.to_string(), formatted: false }
    }

    #[test]
    fn empty_source_is_just_eof() {
        assert_eq!(kinds(""), vec![]);
        assert_eq!(kinds("\n\n   \n# only a comment\n"), vec![]);
    }

    #[test]
    fn let_statement() {
        assert_eq!(
            kinds("let count = 3"),
            vec![Let, ident("count"), Assign, Int(3), Newline]
        );
    }

    #[test]
    fn keywords_and_identifiers() {
        assert_eq!(
            kinds("function if_ready not nothing none"),
            vec![Function, ident("if_ready"), Not, ident("nothing"), None, Newline]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("42 1_000 3.14 1e3 2.5e-3 0xFF 0b1010 0o17"),
            vec![
                Int(42), Int(1000), Float(3.14), Float(1000.0), Float(0.0025),
                Int(255), Int(10), Int(15), Newline
            ]
        );
    }

    #[test]
    fn number_followed_by_a_unit_or_method() {
        assert_eq!(kinds("500 ms"), vec![Int(500), ident("ms"), Newline]);
        assert_eq!(kinds("500ms"), vec![Int(500), ident("ms"), Newline]);
        assert_eq!(kinds("5.abs"), vec![Int(5), Dot, ident("abs"), Newline]);
    }

    #[test]
    fn strings() {
        assert_eq!(kinds(r#""hi" 'there'"#), vec![string("hi"), string("there"), Newline]);
        assert_eq!(kinds(r#""a\tb\n\"q\"""#), vec![string("a\tb\n\"q\""), Newline]);
        assert_eq!(kinds(r#""it's""#), vec![string("it's"), Newline]);
    }

    #[test]
    fn formatted_strings() {
        assert_eq!(
            kinds(r#"f"Hello, {who}!""#),
            vec![Str { value: "Hello, {who}!".to_string(), formatted: true }, Newline]
        );
        // A name that merely starts with f is still a name.
        assert_eq!(kinds("f foo"), vec![ident("f"), ident("foo"), Newline]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("a + b - c * d / e % f ** g // h"),
            vec![
                ident("a"), Plus, ident("b"), Minus, ident("c"), Star, ident("d"),
                Slash, ident("e"), Percent, ident("f"), StarStar, ident("g"),
                SlashSlash, ident("h"), Newline
            ]
        );
        assert_eq!(
            kinds("== != < <= > >= = += -= *= /= ->  x"),
            vec![
                Eq, NotEq, Lt, LtEq, Gt, GtEq, Assign, PlusAssign, MinusAssign,
                StarAssign, SlashAssign, Arrow, ident("x"), Newline
            ]
        );
        // Every operator on two numbers has an `=` form.
        assert_eq!(
            kinds("a //= b %= c **= d ** e // f % g"),
            vec![
                ident("a"), SlashSlashAssign, ident("b"), PercentAssign, ident("c"), StarStarAssign, ident("d"),
                StarStar, ident("e"), SlashSlash, ident("f"), Percent, ident("g"), Newline
            ]
        );
    }

    #[test]
    fn function_signature() {
        assert_eq!(
            kinds("function area(shape: Shape) -> float {"),
            vec![
                Function, ident("area"), LParen, ident("shape"), Colon, ident("Shape"),
                RParen, Arrow, ident("float"), LBrace, Newline
            ]
        );
    }

    #[test]
    fn a_line_break_ends_a_statement() {
        assert_eq!(
            kinds("let a = 1\nlet b = 2\n"),
            vec![Let, ident("a"), Assign, Int(1), Newline, Let, ident("b"), Assign, Int(2), Newline]
        );
    }

    #[test]
    fn blank_lines_and_comments_do_not_add_newlines() {
        assert_eq!(
            kinds("\n\na  # trailing comment\n\n# full line\n\nb"),
            vec![ident("a"), Newline, ident("b"), Newline]
        );
    }

    #[test]
    fn no_newline_inside_parentheses_or_brackets() {
        assert_eq!(
            kinds("f(\n  1,\n  2\n)"),
            vec![ident("f"), LParen, Int(1), Comma, Int(2), RParen, Newline]
        );
        assert_eq!(
            kinds("[\n1,\n2\n]"),
            vec![LBracket, Int(1), Comma, Int(2), RBracket, Newline]
        );
    }

    #[test]
    fn line_breaks_count_again_inside_braces_inside_parentheses() {
        assert_eq!(
            kinds("f(function() {\n  a\n  b\n},\n  1)"),
            vec![
                ident("f"), LParen, Function, LParen, RParen, LBrace, Newline,
                ident("a"), Newline, ident("b"), Newline, RBrace, Comma, Int(1), RParen, Newline
            ]
        );
    }

    #[test]
    fn an_operator_at_the_end_of_a_line_continues_the_statement() {
        assert_eq!(
            kinds("let total = price +\n    tax"),
            vec![Let, ident("total"), Assign, ident("price"), Plus, ident("tax"), Newline]
        );
        assert_eq!(
            kinds("ready and\nwaiting"),
            vec![ident("ready"), And, ident("waiting"), Newline]
        );
    }

    #[test]
    fn positions_are_tracked() {
        let tokens = lex("let x\n  = 10").unwrap();
        let at: Vec<_> = tokens.iter().map(|t| (t.span.line, t.span.col)).collect();
        //             let     x       \n      =       10      \n(end)  eof
        assert_eq!(at, vec![(1, 1), (1, 5), (1, 6), (2, 3), (2, 5), (2, 7), (2, 7)]);
    }

    fn errors(source: &str) -> Vec<(String, usize, usize)> {
        lex(source)
            .unwrap_err()
            .into_iter()
            .map(|e| (e.message, e.span.line, e.span.col))
            .collect()
    }

    #[test]
    fn unterminated_string() {
        assert_eq!(
            errors("let s = \"oops\nlet t = 1"),
            vec![("string is missing its closing quote".to_string(), 1, 9)]
        );
    }

    #[test]
    fn bit_operators() {
        assert_eq!(
            kinds("a & b | c ^ ~d << 1 >> 2"),
            vec![ident("a"), Amp, ident("b"), Pipe, ident("c"), Caret, Tilde, ident("d"), Shl, Int(1), Shr, Int(2), Newline]
        );
        assert_eq!(kinds("&= |= ^= <<= >>= x"), vec![AmpAssign, PipeAssign, CaretAssign, ShlAssign, ShrAssign, ident("x"), Newline]);
        // The comparisons that begin the same way are still themselves.
        assert_eq!(kinds("< <= << > >= >> x"), vec![Lt, LtEq, Shl, Gt, GtEq, Shr, ident("x"), Newline]);
    }

    #[test]
    fn hints_for_habits_from_other_languages() {
        let found = errors("let x = 1;\nif a && b || !c {");
        let messages: Vec<_> = found.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            messages,
            vec![
                "Zeno has no semicolons. A line break ends a statement",
                "Zeno spells this `and`",
                "Zeno spells this `or`",
                "unexpected `!`. Zeno spells this `not`",
            ]
        );
        assert_eq!((found[0].1, found[0].2), (1, 10));
        assert_eq!((found[1].1, found[1].2), (2, 6));
    }

    #[test]
    fn bad_numbers_and_characters() {
        assert_eq!(errors("0xZZ")[0].0, "invalid hexadecimal number");
        assert_eq!(errors("0b102")[0].0, "invalid binary number");
        assert_eq!(errors("99999999999999999999999")[0].0, TOO_BIG);
        assert_eq!(errors("18446744073709551616")[0].0, TOO_BIG);
        assert_eq!(errors("0x1_0000_0000_0000_0000")[0].0, TOO_BIG);
        assert!(lex("18446744073709551615").is_ok());
        assert_eq!(errors("a $ b"), vec![("unexpected character `$`".to_string(), 1, 3)]);
        assert_eq!(errors(r#""bad \q""#)[0].0, "unknown escape `\\q`");
    }

    #[test]
    fn all_reported_errors_not_just_the_first() {
        assert_eq!(errors("$\n@\n`").len(), 3);
    }

    #[test]
    fn an_inline_block_is_taken_as_it_was_written() {
        // Everything between the first line and the `}` under it is
        // one token: the text, with the indentation its lines share
        // taken off, and nothing in it read as Zeno.
        let source = "let lib = inline c {\n    int f(void) {\n        return '}' + \"}\"[0]; // }\n    }\n\n    # not a comment of Zeno\n}\nprint(1)\n";
        let tokens = kinds(source);
        assert_eq!(
            tokens[..5],
            [
                TokenKind::Let,
                ident("lib"),
                TokenKind::Assign,
                TokenKind::Inline {
                    language: "c".to_string(),
                    text: "int f(void) {\n    return '}' + \"}\"[0]; // }\n}\n\n# not a comment of Zeno\n".to_string()
                },
                TokenKind::Newline,
            ]
        );
        assert_eq!(tokens[5], ident("print"));
        // The lines after it are counted as they are in the file.
        let all = lex(source).unwrap();
        assert_eq!((all[5].span.line, all[5].span.col), (8, 1));
        // A block further in ends at a `}` as far in as its own line.
        let nested = "function f() {\n    let m = inline python {\n        def g():\n            return {\n            }\n    }\n}\n";
        assert!(kinds(nested).contains(&TokenKind::Inline { language: "python".to_string(), text: "def g():\n    return {\n    }\n".to_string() }));
        // An empty block is a block.
        assert!(kinds("let m = inline c {\n}\n").contains(&TokenKind::Inline { language: "c".to_string(), text: String::new() }));
    }

    #[test]
    fn inline_is_a_name_wherever_it_does_not_open_a_block() {
        // `inline` is not a word of Zeno. It begins a block only with a
        // name and a `{` that ends the line after it, which nothing
        // else can be.
        assert_eq!(kinds("let inline = 3")[1], ident("inline"));
        assert_eq!(kinds("inline(c)")[0], ident("inline"));
        assert_eq!(kinds("if inline {\n}")[1], ident("inline"));
        assert_eq!(kinds("inline c { }")[0], ident("inline"));
        assert_eq!(kinds("for inline in c {\n}")[1], ident("inline"));
    }

    #[test]
    fn an_inline_block_that_does_not_end_says_how_one_ends() {
        let said = errors("let m = inline c {\n    int f(void);\n");
        assert_eq!(said.len(), 1);
        assert_eq!(
            said[0],
            (
                "this `inline c` block has no end. It ends at a line that holds only `}`, as far in as the line that opened it".to_string(),
                1,
                9
            )
        );
        // A line that is not further in than the first is where the
        // mistake is: a `}` in the wrong place, most often.
        let said = errors("function f() {\n    let m = inline c {\n        int f(void);\n  }\n}\n");
        assert_eq!(said.len(), 1);
        assert!(said[0].0.starts_with("this line of the `inline c` block is not further in than the line that opens the block."), "{said:?}");
        assert_eq!((said[0].1, said[0].2), (4, 3));
    }

    #[test]
    fn the_example_programs_lex_cleanly() {
        for (name, source) in [
            ("hello", include_str!("../examples/hello.zeno")),
            ("tour", include_str!("../examples/tour.zeno")),
            ("blink", include_str!("../examples/blink.zeno")),
            ("packet", include_str!("../examples/packet.zeno")),
            ("generics", include_str!("../examples/generics.zeno")),
        ] {
            if let Err(errors) = lex(source) {
                panic!("examples/{name}.zeno: {errors:?}");
            }
        }
    }
}
