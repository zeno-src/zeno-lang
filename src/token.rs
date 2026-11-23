//! Tokens: the "words" of a Zeno program.
//!
//! The lexer turns source text into a flat list of these. Each token records
//! what it is (`TokenKind`) and where it came from (`Span`), so later stages
//! can report errors at the right line and column.

/// Where a token starts in the source file. Both numbers start at 1.
///
/// A program can be several files (see `modules.rs`), and `line` says
/// which of them too. The lines of the program's first file are numbered
/// from 1, as the editor shows them. The second file's lines are numbered
/// from 1,000,001, the third file's from 2,000,001, and so on. One number
/// then says both the file and the line, and every stage that only passes
/// a position along needs to know nothing about files.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

/// How far apart the line numbers of two files are. A file has fewer
/// lines than this.
pub const LINES_A_FILE: usize = 1_000_000;

/// The most files a program can have. A compiled program keeps a line
/// number in a C `int`, and this is how many files fit in one.
pub const MOST_FILES: usize = 2_000;

impl Span {
    /// Which file of the program this is in: 0 for the first one.
    pub fn file(self) -> usize {
        self.line / LINES_A_FILE
    }

    /// The line as the editor numbers it, within its own file.
    pub fn line_in_file(self) -> usize {
        self.line % LINES_A_FILE
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // --- Values written directly in the source ---
    Int(u64),
    Float(f64),
    /// A string. `formatted` is true for f"..." strings; the `{...}` parts
    /// inside are left as plain text here and picked apart by the parser.
    Str { value: String, formatted: bool },
    /// A name chosen by the programmer: a variable, function, type, ...
    Ident(String),
    /// `inline c {` and every line after it, up to the `}` that ends the
    /// block: code of another language, kept as it was written (decision
    /// 61). `text` is the lines between, with the indentation they share
    /// taken off.
    Inline { language: String, text: String },

    // --- Keywords ---
    Let,
    Var,
    Const,
    Function,
    Return,
    If,
    Else,
    For,
    In,
    While,
    Unsafe,
    /// `private function helper()`: only the file it is in can use it.
    Private,
    /// `interface Shape { function area(self) -> float }`: what a class
    /// must have to be used as a `Shape`.
    Interface,
    /// `owned class`, `owned list[int]`: a value with one owner.
    Owned,
    /// `frame: take Frame`: a parameter that takes the value over.
    Take,
    /// `strict`, alone on the first line: every type must be known.
    Strict,
    Break,
    Continue,
    Match,
    Case,
    Class,
    Enum,
    Try,
    Except,
    Raise,
    Raises,
    /// `assert total == 6`, in a test.
    Assert,
    As,
    From,
    Import,
    And,
    Or,
    Not,
    Is,
    True,
    False,
    None,

    // --- Punctuation ---
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    LBrace,   // {
    RBrace,   // }
    Comma,    // ,
    Dot,      // .
    Colon,    // :
    Arrow,    // ->

    // --- Operators ---
    Plus,        // +
    Minus,       // -
    Star,        // *
    Slash,       // /
    SlashSlash,  // //
    Percent,     // %
    StarStar,    // **
    Assign,      // =
    PlusAssign,  // +=
    MinusAssign, // -=
    StarAssign,  // *=
    SlashAssign, // /=
    SlashSlashAssign, // //=
    PercentAssign,    // %=
    StarStarAssign,   // **=
    Amp,         // &   the bits both have
    Pipe,        // |   the bits either has
    Caret,       // ^   the bits one has and the other has not
    Tilde,       // ~   every bit turned over
    Shl,         // <<
    Shr,         // >>
    AmpAssign,   // &=
    PipeAssign,  // |=
    CaretAssign, // ^=
    ShlAssign,   // <<=
    ShrAssign,   // >>=
    Eq,          // ==
    NotEq,       // !=
    Lt,          // <
    LtEq,        // <=
    Gt,          // >
    GtEq,        // >=

    /// The end of a statement. Zeno has no semicolons, so a line break does this job.
    Newline,
    /// The end of the file. Always the last token.
    Eof,
}

/// If `word` is a keyword, return its token kind. Otherwise it is an identifier.
pub fn keyword(word: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match word {
        "let" => Let,
        "var" => Var,
        "const" => Const,
        "function" => Function,
        "return" => Return,
        "if" => If,
        "else" => Else,
        "for" => For,
        "in" => In,
        "while" => While,
        "unsafe" => Unsafe,
        "private" => Private,
        "interface" => Interface,
        "owned" => Owned,
        "take" => Take,
        "strict" => Strict,
        "break" => Break,
        "continue" => Continue,
        "match" => Match,
        "case" => Case,
        "class" => Class,
        "enum" => Enum,
        "try" => Try,
        "except" => Except,
        "raise" => Raise,
        "raises" => Raises,
        "assert" => Assert,
        "as" => As,
        "from" => From,
        "import" => Import,
        "and" => And,
        "or" => Or,
        "not" => Not,
        "is" => Is,
        "true" => True,
        "false" => False,
        "none" => None,
        _ => return Option::None,
    })
}

impl TokenKind {
    /// How to name this token in an error message, e.g. "`{`" or "the end of the line".
    pub fn describe(&self) -> String {
        use TokenKind::*;
        let text = match self {
            Int(_) | Float(_) => return "a number".to_string(),
            Str { .. } => return "a string".to_string(),
            Ident(name) => return format!("the name `{name}`"),
            Inline { language, .. } => return format!("an `inline {language}` block"),
            Newline => return "the end of the line".to_string(),
            Eof => return "the end of the file".to_string(),

            Let => "let",
            Var => "var",
            Const => "const",
            Function => "function",
            Return => "return",
            If => "if",
            Else => "else",
            For => "for",
            In => "in",
            While => "while",
            Unsafe => "unsafe",
            Private => "private",
            Interface => "interface",
            Owned => "owned",
            Take => "take",
            Strict => "strict",
            Break => "break",
            Continue => "continue",
            Match => "match",
            Case => "case",
            Class => "class",
            Enum => "enum",
            Try => "try",
            Except => "except",
            Raise => "raise",
            Raises => "raises",
            Assert => "assert",
            As => "as",
            From => "from",
            Import => "import",
            And => "and",
            Or => "or",
            Not => "not",
            Is => "is",
            True => "true",
            False => "false",
            None => "none",

            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            Comma => ",",
            Dot => ".",
            Colon => ":",
            Arrow => "->",

            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            SlashSlash => "//",
            Percent => "%",
            StarStar => "**",
            Assign => "=",
            PlusAssign => "+=",
            MinusAssign => "-=",
            StarAssign => "*=",
            SlashAssign => "/=",
            SlashSlashAssign => "//=",
            PercentAssign => "%=",
            StarStarAssign => "**=",
            Amp => "&",
            Pipe => "|",
            Caret => "^",
            Tilde => "~",
            Shl => "<<",
            Shr => ">>",
            AmpAssign => "&=",
            PipeAssign => "|=",
            CaretAssign => "^=",
            ShlAssign => "<<=",
            ShrAssign => ">>=",
            Eq => "==",
            NotEq => "!=",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
        };
        format!("`{text}`")
    }

    /// True if a statement cannot possibly end on this token, so a line break
    /// right after it means "the statement continues on the next line":
    ///
    /// ```text
    /// let total = price +
    ///     tax
    /// ```
    pub fn continues_line(&self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Comma | Dot | Arrow
                | Plus | Minus | Star | Slash | SlashSlash | Percent | StarStar
                | Assign | PlusAssign | MinusAssign | StarAssign | SlashAssign
                | SlashSlashAssign | PercentAssign | StarStarAssign
                | Amp | Pipe | Caret | Tilde | Shl | Shr
                | AmpAssign | PipeAssign | CaretAssign | ShlAssign | ShrAssign
                | Eq | NotEq | Lt | LtEq | Gt | GtEq
                | And | Or | Not | Is | In
        )
    }
}
