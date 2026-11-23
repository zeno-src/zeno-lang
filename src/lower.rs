//! The portable tree: a checked program, made ready to be written out in
//! another language.
//!
//! `zeno build --target js` (and `py`, `go`, `java`, `cs`, `cpp`, `rust`)
//! writes the program as source code of another language. Each of those
//! languages is written by a small printer of its own (emit_js.rs and the
//! rest). What they have to print is the same program, so the hard part
//! is done once, here, for all of them:
//!
//! - **Every name is made unique and safe.** Zeno lets a block declare
//!   an `x` that hides an `x` further out, and a Zeno name may be a word
//!   that another language keeps for itself (`function`, `def`, `match`).
//!   Here each variable gets a name of its own that no target language
//!   minds: `x`, `x_2`, `class_`.
//! - **Every name is looked up once.** A name is a variable of this
//!   function, a variable of a function around it (which the inner one
//!   then *captures*), or something that comes with Zeno (`print`,
//!   `math.sqrt`). The printers never look a name up.
//! - **What is declared in a block is listed at its top.** Zeno lets a
//!   function be called above the line it is written on, and lets it use
//!   the variables of its block. So a block begins by declaring all of its
//!   variables, then makes its functions, classes and enums, and then
//!   runs its statements. Every target can say that.
//! - **Expressions that need statements become statements.** A list
//!   comprehension is a loop; `a < b < c` works out `b` once and may never
//!   work out `c`. Those become loops and `if`s over temporary variables,
//!   in front of the statement that uses them, keeping the order in which
//!   Zeno works things out.
//! - **`match`, `try`, a call with named arguments and an f-string become
//!   plain steps** that every language has: `if`, a native `try`, a call
//!   into the target's runtime.
//!
//! The values themselves stay what `zeno run` has: an int, a float, a str,
//! a list, an object, each one a `Value` of the target's runtime
//! (runtime.js and the rest), which does exactly what ops.rs and
//! builtins.rs do. That is the promise of every target: it prints what
//! `zeno run` prints (decisions chapter 24).
//!
//! Nothing here knows which language will be printed.

use std::collections::{HashMap, HashSet};

use crate::ast::{self, BinaryOp, ExprKind, FormatPart, Pattern, StmtKind, UnaryOp};
use crate::builtins;
use crate::error::Error;
use crate::token::Span;

/// A variable, by its place in `Program::vars`.
pub type VarId = usize;

/// The whole program, ready to print.
pub struct Program {
    pub vars: Vec<Var>,
    /// The program's own statements, as the body of a function with no
    /// parameters. Every function of the program is made inside it.
    pub main: Func,
    /// The files of the program, by number, for error messages: a line
    /// number `n` in this tree is line `n % LINES_A_FILE` of file
    /// `n / LINES_A_FILE` (see token.rs).
    pub files: Vec<String>,
}

pub struct Var {
    /// The name it has in the printed program.
    pub ident: String,
    /// True when a function written inside the one that declares it uses
    /// it. Languages whose functions cannot share a variable (Java, C++,
    /// Rust) keep such a variable in a box, a *cell*, which both can hold.
    pub cell: bool,
    /// True when anything reads it. Go refuses a variable that nothing reads.
    pub read: bool,
    /// What it holds. For the typed targets still to come.
    #[allow(dead_code)]
    pub kind: VarKind,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VarKind {
    /// A Zeno value.
    Value,
    /// The error a `try` caught, as the target language holds it (an
    /// exception object). Only `Rt` calls named in `Expr::Caught` use it.
    Error,
}

#[derive(Clone)]
pub struct Func {
    /// The name the function prints by: `<function area>`. Empty for a
    /// function written as a value.
    pub name: String,
    pub params: Vec<Param>,
    pub body: Block,
    /// Variables of the functions around this one that this one gives a
    /// new value. A target whose closures copy what they use (C++, Java)
    /// must keep these in cells; the ones still to come read this.
    #[allow(dead_code)]
    pub nonlocals: Vec<VarId>,
    /// Variables of the functions around this one that this one uses, at
    /// any depth. C++ and Rust list them where the function is made.
    pub captures: Vec<VarId>,
}

#[derive(Clone)]
pub struct Param {
    pub var: VarId,
    /// As the program wrote it, for a call with named arguments.
    pub name: String,
    /// What works out its value when a call leaves it out. The block sets
    /// the parameter. A call that leaves out a parameter with no default
    /// is refused before the function runs.
    pub default: Option<Block>,
}

/// The statements of one `{ }`, with the variables they declare.
#[derive(Default, Clone)]
pub struct Block {
    pub decls: Vec<VarId>,
    pub stmts: Vec<Stmt>,
}

#[derive(Clone)]
pub enum Stmt {
    /// Give a variable a value: its first, or a new one.
    Set(VarId, Expr),
    /// `object.name = value`
    SetField { object: Expr, name: String, value: Expr, line: usize },
    /// `object[index] = value`
    SetIndex { object: Expr, index: Expr, value: Expr, line: usize },
    /// An expression worked out for what it does, as a call.
    Expr(Expr),
    If { condition: Cond, then: Block, otherwise: Option<Block> },
    /// A loop that goes on while the condition holds.
    While { condition: Cond, body: Block },
    /// A loop over a list of values from `Expr` (`items` or `pairs`).
    /// `var`, and `second` if there is one, are new variables in each round,
    /// declared by the loop and not by `body`.
    For { var: VarId, second: Option<VarId>, items: Expr, body: Block },
    Break,
    Continue,
    Return(Expr),
    /// Raise a Zeno value: an error, or an object of a class.
    Raise { value: Expr, line: usize },
    /// Run `body`. If it raises, `error` holds what it raised and
    /// `handler` runs. The handler ends with `Reraise` where nothing caught it.
    Try { body: Block, error: VarId, handler: Block },
    /// Raise again what `try` caught, from the line it was first raised on.
    Reraise(VarId),
    /// Stop the program with a mistake no `except` can catch.
    Fatal { message: String, line: usize },
    /// `match` found no case: stop, saying what the value was.
    NoMatch { value: Expr, line: usize },
    /// A block of its own, as one arm of a `match`.
    Block(Block),
}

/// A Zeno value.
#[derive(Clone)]
pub enum Expr {
    Var(VarId),
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    None,
    /// The value a call gives a parameter it leaves out, so that the
    /// function knows to work out the default.
    Missing,
    /// A bool, from a test.
    FromCond(Box<Cond>),
    /// Something that comes with Zeno: `print`, `math.sqrt`, `ValueError`,
    /// `u8.wrap`. See `builtin_names`.
    Builtin(String),
    /// A module that comes with Zeno, as a value: `math`.
    Module(String),
    /// A call of something that comes with Zeno: `print(x)`, `math.sqrt(2)`.
    CallBuiltin { name: String, args: Vec<Expr>, line: usize },
    /// A call of a value, with plain arguments.
    Call { callee: Box<Expr>, args: Vec<Expr>, line: usize },
    /// A call of a value with some named arguments, after the plain ones.
    CallNamed { callee: Box<Expr>, args: Vec<Expr>, names: Vec<String>, named: Vec<Expr>, line: usize },
    /// A call of a function of the program that is known where it is
    /// written: one declared with `function` and never replaced. The
    /// arguments are lined up with its parameters already, `Missing` for
    /// the ones left out.
    CallDirect {
        function: VarId,
        args: Vec<Expr>,
        #[allow(dead_code)]
        line: usize,
    },
    /// `object.name(args)`: a method of an object, a str or a list, or a
    /// function of a module of the program.
    CallMethod { object: Box<Expr>, name: String, args: Vec<Expr>, names: Vec<String>, named: Vec<Expr>, line: usize },
    /// A step done by the runtime, by name: `add`, `index`, `field`.
    /// Every runtime has each of these (see runtime.js for the list).
    Rt { name: &'static str, args: Vec<Expr>, line: usize },
    /// A piece of text as the target language holds it, not a Zeno str:
    /// the name of a field, a unit, a format.
    Text(String),
    /// A whole number as the target holds it: which case of an enum.
    Count(usize),
    /// Several values as the target holds them: a list of the target's own,
    /// handed to a step of the runtime that takes many (`list`, `concat`).
    Array(Vec<Expr>),
    /// What `try` caught, as a Zeno value: `Rt` steps `caught` and `kind_of`
    /// take it. (Kept apart because the target holds it differently.)
    Caught(VarId),
    Func(Box<Func>),
    Class(Box<Class>),
    Enum(Box<EnumDef>),
}

#[derive(Clone)]
pub struct Class {
    /// As the program declared it: `Point`, or `shapes.Point` for one of
    /// an imported file. It prints without the file's name.
    pub name: String,
    /// Each field, with what works out its default (a function of no
    /// arguments) if it has one.
    pub fields: Vec<(String, Option<Func>)>,
    pub methods: Vec<Func>,
}

#[derive(Clone)]
pub struct EnumDef {
    pub name: String,
    /// Each case, with the names of the values it carries.
    pub cases: Vec<(String, Vec<String>)>,
}

/// A test: a bool as the target holds it, ready for `if` and `while`.
#[derive(Clone)]
pub enum Cond {
    True,
    /// A Zeno value that must be a bool.
    Truth(Expr, usize),
    Not(Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    /// A test the runtime makes, by name: `eq`, `lt`, `is_case`, `catches`.
    Test { name: &'static str, args: Vec<Expr>, line: usize },
    /// Was this parameter left out of the call?
    #[allow(dead_code)]
    IsMissing(VarId),
}

/// What changes from one target to another in how the tree is made.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// The target works out the arguments of a call in an order of its
    /// own choosing (C++), or reads a variable at a time of its own
    /// choosing (Go). Then every argument before the last one that does
    /// anything, other than a constant, is worked out first, into a
    /// variable, in Zeno's order.
    pub unordered_arguments: bool,
}

/// The names that come with Zeno and need no `import`.
pub fn builtin_names() -> Vec<&'static str> {
    builtins::all().into_iter().map(|(name, _)| name).collect()
}

/// The functions and values of the modules that come with Zeno that a
/// target is asked for. `board` and `web` are left out: a program for
/// another language has no board, and only a page has the `web` module.
pub fn module_members(module: &str) -> Option<&'static [&'static str]> {
    Some(match module {
        "math" => &[
            "pi", "e", "sqrt", "floor", "ceil", "round", "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "exp",
            "log", "log2", "log10",
        ],
        "time" => &["sleep", "now"],
        "files" => &[
            "read", "write", "append", "exists", "remove", "read_bytes", "write_bytes", "list", "make_folder",
            "handle_open", "handle_line", "handle_bytes", "handle_write", "handle_write_bytes", "handle_close",
        ],
        "program" => &["arguments", "exit", "environment", "warn"],
        "json" => &["parse", "write"],
        _ => return None,
    })
}

/// What a module that comes with Zeno has and no target has yet. A
/// program that uses it is refused when it is built, with the place,
/// which is better than a program that stops when it gets there.
fn no_target_has(member: &str, span: Span) -> Result<()> {
    if member == "program.run" {
        return Err(Error::new(
            "a program built for another language cannot use `program.run` yet. `zeno run` can still run this program",
            span,
        ));
    }
    Ok(())
}

/// Make the portable tree of a checked program.
///
/// `files` are the paths of its files, by number. An `Err` says what the
/// program uses that no target can do yet, and where.
pub fn lower(program: &ast::Program, files: Vec<String>, options: Options) -> Result<Program> {
    let mut lowerer = Lowerer {
        vars: Vec::new(),
        info: Vec::new(),
        later: Vec::new(),
        scopes: Vec::new(),
        functions: Vec::new(),
        idents: HashSet::new(),
        options,
        builtins: builtin_names().into_iter().collect(),
    };
    lowerer.functions.push(FunctionContext::default());
    lowerer.scopes.push(Scope::default());
    let body = lowerer.statements(&program.statements)?;
    let decls = std::mem::take(&mut lowerer.scopes[0].decls);
    let context = lowerer.functions.pop().unwrap_or_default();
    let main = Func {
        name: "main".to_string(),
        params: Vec::new(),
        body: Block { decls, stmts: body },
        nonlocals: context.nonlocals,
        captures: context.captures,
    };
    Ok(Program { vars: lowerer.vars, main, files })
}

// ------------------------------------------------------------------ Names

/// Words that some target language keeps for itself, or that its
/// runtime uses. A Zeno name that is one of them gets a `_` after it.
const RESERVED: &[&str] = &[
    // JavaScript
    "arguments", "await", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
    "else", "enum", "eval", "export", "extends", "false", "finally", "for", "function", "if", "implements",
    "import", "in", "instanceof", "interface", "let", "new", "null", "package", "private", "protected", "public",
    "return", "static", "super", "switch", "this", "throw", "true", "try", "typeof", "var", "void", "while",
    "with", "yield", "undefined", "NaN", "Infinity", "Object", "Array", "String", "Number", "Math", "JSON",
    "console", "process", "require", "module", "exports", "globalThis", "window", "document", "Symbol", "Map",
    "Set", "Error", "BigInt", "Date", "Promise", "Boolean", "Function", "RegExp",
    // Python
    "and", "as", "assert", "async", "break", "def", "del", "elif", "except", "from", "global", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "None", "True", "False", "print", "len", "range", "int", "float",
    "str", "list", "dict", "object", "type", "self", "sys", "math", "os", "time", "json", "abs", "min", "max",
    "sum", "input", "open", "id", "iter", "next", "map", "filter", "any", "all", "bool", "chr", "ord",
    "isinstance", "super", "tuple", "set", "bytes", "format", "repr", "hash", "round", "sorted", "zip",
    "enumerate", "callable", "vars", "dir", "exec", "compile", "exit", "quit", "help",
    // Go
    "chan", "fallthrough", "func", "go", "goto", "map", "select", "struct", "defer", "nil", "iota", "append",
    "cap", "close", "copy", "make", "panic", "recover", "string", "byte", "rune", "error", "fmt", "strings",
    "strconv", "main", "init", "any", "uint", "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32",
    "uint64", "float32", "float64", "complex", "real", "imag", "uintptr", "complex64", "complex128",
    // Java and C#
    "abstract", "boolean", "char", "double", "final", "long", "native", "short", "synchronized", "throws",
    "transient", "volatile", "strictfp", "byte", "base", "checked", "decimal", "event", "explicit", "extern",
    "fixed", "foreach", "implicit", "internal", "lock", "namespace", "operator", "out", "override", "params",
    "readonly", "ref", "sbyte", "sealed", "sizeof", "stackalloc", "ulong", "unchecked", "unsafe", "ushort",
    "using", "virtual", "string", "System", "Math", "Console", "Value", "var", "dynamic", "record", "when",
    "where", "value", "get", "set", "add", "remove", "yield", "partial", "global", "nameof", "Exception",
    // C++
    "alignas", "alignof", "asm", "auto", "bool", "char8_t", "char16_t", "char32_t", "co_await", "co_return",
    "co_yield", "concept", "consteval", "constexpr", "constinit", "const_cast", "decltype", "dynamic_cast",
    "explicit", "export", "friend", "inline", "mutable", "noexcept", "nullptr", "register", "reinterpret_cast",
    "requires", "signed", "static_assert", "static_cast", "template", "thread_local", "typedef", "typeid",
    "typename", "union", "unsigned", "wchar_t", "and_eq", "bitand", "bitor", "compl", "not_eq", "or_eq",
    "xor", "xor_eq", "std", "NULL", "EOF", "errno", "stdin", "stdout", "stderr", "exit", "printf", "V", "NAN",
    "INFINITY", "M_PI", "M_E", "argc", "argv", "assert", "unix", "linux", "int64_t", "size_t", "INT64_MAX",
    "INT64_MIN", "EXIT_SUCCESS", "EXIT_FAILURE", "BUFSIZ", "RAND_MAX", "SEEK_SET", "SEEK_CUR", "SEEK_END",
    // Rust
    "crate", "dyn", "extern", "fn", "impl", "loop", "match", "mod", "move", "mut", "pub", "Self", "trait",
    "unsafe", "use", "where", "abstract", "become", "box", "final", "macro", "override", "priv", "typeof",
    "unsized", "virtual", "yield", "try", "Some", "Ok", "Err", "Box", "Rc", "Vec", "Option", "Result", "Cell",
    "RefCell", "z", "gen",
    // Found by the review: the `args` of Java's `main` and of a C#
    // program, and words the lists above had missed.
    "args", "delegate", "clear", "comparable", "println", "int8_t", "int16_t", "int32_t", "uint8_t", "uint16_t",
    "uint32_t", "uint64_t", "alloca", "offsetof",
    // PHP: the variables it has in every function, and the spare names
    // the Python and PHP printers use in a loop and a `catch`.
    "GLOBALS", "_SERVER", "_GET", "_POST", "_FILES", "_COOKIE", "_SESSION", "_REQUEST", "_ENV", "z_item",
    "z_second", "z_error",
    // Lua
    "elseif", "end", "local", "repeat", "then", "until",
    // Ruby
    "alias", "begin", "defined", "ensure", "elsif", "redo", "rescue", "retry", "undef", "unless", "_1", "_2", "_3",
    "_4", "_5", "_6", "_7", "_8", "_9",
    // Dart
    "covariant", "deferred", "extension", "external", "factory", "hide", "late", "library", "mixin", "on", "part",
    "required", "rethrow", "show", "sync", "List", "identical",
    // Kotlin
    "fun", "typealias", "val", "it", "arrayOf", "booleanArrayOf",
];

#[derive(Default)]
struct Scope {
    /// The variables declared here so far, by their Zeno name.
    names: HashMap<String, VarId>,
    /// The variables this block declares, in order: what its `decls` will be.
    decls: Vec<VarId>,
}

#[derive(Default)]
struct FunctionContext {
    nonlocals: Vec<VarId>,
    captures: Vec<VarId>,
    /// How many loops of this function the statement being lowered is in.
    loops: usize,
    /// The resources (decision 59.1) whose `Scoped` the statement being
    /// lowered is in, outermost first, each with how many loops it was in.
    /// A `return` releases them all, and a `break` or a `continue` the
    /// ones inside the loop it leaves.
    scoped: Vec<(VarId, usize)>,
}

/// What a variable is known to hold for as long as it exists, so that a
/// call of it can be made directly.
#[derive(Clone)]
enum Known {
    Nothing,
    /// A function declared with `function`: its parameters, each with
    /// whether it has a default.
    Function(Vec<(String, bool)>),
    /// A module that comes with Zeno: `import math`.
    Module(String),
    /// A function or value of such a module: `from math import sqrt`.
    Member(String),
    /// An enum case with no values (`Idle`), or one that carries values
    /// (`Circle`). Patterns tell the two apart.
    Case { carries: bool },
}


/// What the lowering knows of each variable while it works. Kept beside
/// `Program::vars`, one for each, so that the printers never see it.
struct VarInfo {
    /// The function (by depth) that declares it.
    function: usize,
    known: Known,
}

struct Lowerer {
    vars: Vec<Var>,
    info: Vec<VarInfo>,
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    idents: HashSet<String>,
    options: Options,
    builtins: HashSet<&'static str>,
    /// For each block being lowered, the variables made ahead for its
    /// `let`, `const` and `import` lines.
    later: Vec<Vec<(String, VarId)>>,
}

/// The line a node is on, as the tree keeps it: the line number with the
/// file's number in it (see `Program::files`).
fn line(span: Span) -> usize {
    span.line
}

/// The name of the runtime's step for an operator that works out a value.
fn operator(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "add",
        Sub => "sub",
        Mul => "mul",
        Div => "div",
        FloorDiv => "floordiv",
        Mod => "mod",
        Pow => "pow",
        BitAnd => "bitand",
        BitOr => "bitor",
        BitXor => "bitxor",
        Shl => "shl",
        Shr => "shr",
        Eq | Is => "eq",
        NotEq | IsNot => "ne",
        Lt => "lt",
        LtEq => "le",
        Gt => "gt",
        GtEq => "ge",
        In => "in",
        NotIn => "not_in",
        And => "and",
        Or => "or",
    }
}

fn compares(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Eq | Is | NotEq | IsNot | Lt | LtEq | Gt | GtEq | In | NotIn)
}

/// A value that can be worked out at any moment with the same result and
/// no effect, so it need not be kept in a variable to keep Zeno's order.
/// A value that is the same whenever it is worked out.
fn constant(expr: &Expr) -> bool {
    trivial(expr) && !matches!(expr, Expr::Var(_))
}

fn trivial(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Var(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::None
            | Expr::Missing
            | Expr::Builtin(_)
            | Expr::Module(_)
            | Expr::Text(_)
            | Expr::Count(_)
            | Expr::Caught(_)
    )
}

type Result<T> = std::result::Result<T, Error>;

impl Lowerer {
    // ----------------------------------------------------------- Variables

    /// A name for a new variable that no other variable of the program
    /// has, and that no target language minds.
    fn ident(&mut self, wanted: &str) -> String {
        let mut base: String =
            wanted.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
        // `_` alone is Go's, Java's, C#'s and Rust's: a name that is
        // nothing but `_` becomes `v_`.
        if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) || base.chars().all(|c| c == '_') {
            base = format!("v{base}");
        }
        // A name of a runtime begins with `z_`, `Z` or `__`; a name of
        // the program never does.
        if base.starts_with("z_") || base.starts_with('Z') || base.starts_with("__") {
            base = format!("v_{base}");
        }
        if RESERVED.contains(&base.as_str()) {
            base.push('_');
        }
        let mut ident = base.clone();
        let mut n = 2;
        while self.idents.contains(&ident) || RESERVED.contains(&ident.as_str()) {
            ident = format!("{base}_{n}");
            n += 1;
        }
        self.idents.insert(ident.clone());
        ident
    }

    fn new_var(&mut self, wanted: &str, kind: VarKind, known: Known) -> VarId {
        let ident = self.ident(wanted);
        self.vars.push(Var { ident, cell: false, read: false, kind });
        self.info.push(VarInfo { function: self.functions.len() - 1, known });
        self.vars.len() - 1
    }

    fn scope(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("there is always a scope")
    }

    /// Declare a variable of the program in the innermost block.
    fn declare(&mut self, name: &str, known: Known) -> VarId {
        let id = self.new_var(name, VarKind::Value, known);
        let scope = self.scope();
        scope.names.insert(name.to_string(), id);
        scope.decls.push(id);
        id
    }

    /// Declare a variable that something else declares in the printed
    /// program (a parameter, the variable of a `for`).
    fn declare_apart(&mut self, name: &str) -> VarId {
        let id = self.new_var(name, VarKind::Value, Known::Nothing);
        self.scope().names.insert(name.to_string(), id);
        id
    }

    /// A variable of the lowering's own, to hold a value for a moment.
    fn temp(&mut self, kind: VarKind) -> VarId {
        let id = self.new_var("t", kind, Known::Nothing);
        self.scope().decls.push(id);
        id
    }

    fn lookup(&self, name: &str) -> Option<VarId> {
        self.scopes.iter().rev().find_map(|scope| scope.names.get(name).copied())
    }

    /// Note that the current function uses a variable, and gives it a
    /// new value if `assigns`.
    fn uses(&mut self, id: VarId, assigns: bool) {
        let here = self.functions.len() - 1;
        let home = self.info[id].function;
        if !assigns {
            self.vars[id].read = true;
        }
        if home == here {
            return;
        }
        self.vars[id].cell = true;
        for depth in home + 1..=here {
            let captures = &mut self.functions[depth].captures;
            if !captures.contains(&id) {
                captures.push(id);
            }
        }
        if assigns && !self.functions[here].nonlocals.contains(&id) {
            self.functions[here].nonlocals.push(id);
        }
    }

    fn read(&mut self, id: VarId) -> Expr {
        self.uses(id, false);
        Expr::Var(id)
    }

    fn set(&mut self, id: VarId, value: Expr) -> Stmt {
        self.uses(id, true);
        Stmt::Set(id, value)
    }

    /// Keep a value in a temporary variable, unless it is trivial already.
    fn spill(&mut self, value: Expr, out: &mut Vec<Stmt>) -> Expr {
        if trivial(&value) {
            return value;
        }
        let t = self.temp(VarKind::Value);
        out.push(self.set(t, value));
        self.read(t)
    }

    /// Keep a value in a temporary variable unless it is a constant. A
    /// variable is kept too: what is worked out next may give it a new
    /// value, and Zeno has read it by then.
    fn hold(&mut self, value: Expr, out: &mut Vec<Stmt>) -> Expr {
        if constant(&value) {
            return value;
        }
        let t = self.temp(VarKind::Value);
        out.push(self.set(t, value));
        self.read(t)
    }

    // ---------------------------------------------------------- Statements

    /// Statements in a new block of their own.
    fn block(&mut self, statements: &[ast::Stmt]) -> Result<Block> {
        self.scopes.push(Scope::default());
        let stmts = self.statements(statements);
        let scope = self.scopes.pop().expect("the scope just pushed");
        Ok(Block { decls: scope.decls, stmts: stmts? })
    }

    /// Run `inside` in a new block, and give back the block it made.
    fn in_block(&mut self, inside: impl FnOnce(&mut Self, &mut Vec<Stmt>) -> Result<()>) -> Result<Block> {
        self.scopes.push(Scope::default());
        let mut stmts = Vec::new();
        let done = inside(self, &mut stmts);
        let scope = self.scopes.pop().expect("the scope just pushed");
        done?;
        Ok(Block { decls: scope.decls, stmts })
    }

    /// Statements in the current block: its functions, classes and enums
    /// first, then the rest in order.
    fn statements(&mut self, statements: &[ast::Stmt]) -> Result<Vec<Stmt>> {
        let mut out = Vec::new();
        // What follows the `let` of a resource is more of this block
        // (decision 59.1): its names are the block's, and declared here.
        let every = ast::flat(statements);
        // Every name first, so that each function can see the others.
        let mut hoisted = Vec::new();
        for &statement in &every {
            match &statement.kind {
                StmtKind::Function(decl) => {
                    let params = decl.params.iter().map(|p| (p.name.clone(), p.default.is_some())).collect();
                    hoisted.push((statement, vec![self.declare(&decl.name, Known::Function(params))]));
                }
                StmtKind::Class(class) => hoisted.push((statement, vec![self.declare(&class.name, Known::Nothing)])),
                StmtKind::Enum(decl) => {
                    let mut ids = vec![self.declare(&decl.name, Known::Nothing)];
                    for case in &decl.variants {
                        ids.push(self.declare(&case.name, Known::Case { carries: !case.fields.is_empty() }));
                    }
                    hoisted.push((statement, ids));
                }
                _ => {}
            }
        }
        // The variables the block declares further down. A function of
        // the block sees them (it may be called once they have a value),
        // and the statements of the block see each from its own line on.
        let mut later: Vec<(String, VarId)> = Vec::new();
        for &statement in &every {
            match &statement.kind {
                StmtKind::Let { name, mutable, value, .. } => {
                    let known = match (&value.kind, mutable) {
                        (ExprKind::Function(decl), false) => {
                            Known::Function(decl.params.iter().map(|p| (p.name.clone(), p.default.is_some())).collect())
                        }
                        _ => Known::Nothing,
                    };
                    let id = self.new_var(name, VarKind::Value, known);
                    later.push((name.clone(), id));
                }
                StmtKind::Const { name, .. } => {
                    let id = self.new_var(name, VarKind::Value, Known::Nothing);
                    later.push((name.clone(), id));
                }
                StmtKind::Receive { places, .. } => {
                    for name in places.iter().filter_map(ast::Place::declared) {
                        let id = self.new_var(name, VarKind::Value, Known::Nothing);
                        later.push((name.clone(), id));
                    }
                }
                StmtKind::Import { module, names } if module_members(&module.join(".")).is_some() => {
                    let path = module.join(".");
                    if names.is_empty() {
                        let name = module.last().cloned().unwrap_or_default();
                        let id = self.new_var(&name, VarKind::Value, Known::Module(path.clone()));
                        later.push((name, id));
                    }
                    for name in names {
                        let id = self.new_var(name, VarKind::Value, Known::Member(format!("{path}.{name}")));
                        later.push((name.clone(), id));
                    }
                }
                _ => {}
            }
        }
        for (name, id) in &later {
            let scope = self.scope();
            scope.decls.push(*id);
            scope.names.insert(name.clone(), *id);
        }
        for (statement, ids) in hoisted {
            match &statement.kind {
                StmtKind::Function(decl) => {
                    let function = self.function(decl)?;
                    out.push(self.set(ids[0], Expr::Func(Box::new(function))));
                }
                StmtKind::Class(class) => {
                    let class = self.class(class)?;
                    out.push(self.set(ids[0], Expr::Class(Box::new(class))));
                }
                StmtKind::Enum(decl) => {
                    let cases =
                        decl.variants.iter().map(|case| (case.name.clone(), case.fields.iter().map(|f| f.name.clone()).collect())).collect();
                    out.push(self.set(ids[0], Expr::Enum(Box::new(EnumDef { name: decl.name.clone(), cases }))));
                    for (index, id) in ids[1..].iter().enumerate() {
                        let def = self.read(ids[0]);
                        let value = Expr::Rt { name: "case", args: vec![def, Expr::Count(index)], line: line(statement.span) };
                        out.push(self.set(*id, value));
                    }
                }
                _ => unreachable!("only these were hoisted"),
            }
        }
        // From here on, each is seen from its own line.
        for (name, _) in &later {
            self.scope().names.remove(name);
        }
        self.later.push(later);
        let mut result = Ok(());
        for statement in statements {
            result = self.statement(statement, &mut out);
            if result.is_err() {
                break;
            }
        }
        self.later.pop();
        result.map(|()| out)
    }

    /// The variable made ahead for the `let`, `const` or `import` of the
    /// current block called `name`, now seen from here on.
    fn declare_later(&mut self, name: &str) -> VarId {
        let id = self.later.last().and_then(|later| later.iter().find(|(n, _)| n == name).map(|(_, id)| *id));
        match id {
            Some(id) => {
                self.scope().names.insert(name.to_string(), id);
                id
            }
            None => self.declare(name, Known::Nothing),
        }
    }

    fn statement(&mut self, statement: &ast::Stmt, out: &mut Vec<Stmt>) -> Result<()> {
        let at = line(statement.span);
        match &statement.kind {
            StmtKind::Scoped { name, body } => self.scoped(name, body, at, out)?,
            StmtKind::Let { mutable, name, value, .. } => {
                let _ = mutable;
                let value = self.expr(value, out)?;
                let id = self.declare_later(name);
                out.push(self.set(id, value));
            }
            StmtKind::Const { name, value, .. } => {
                let value = self.expr(value, out)?;
                let id = self.declare_later(name);
                out.push(self.set(id, value));
            }
            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, at, out)?,
            StmtKind::Receive { places, value, .. } => self.receive(places, value, at, out)?,
            StmtKind::Expr(expr) => {
                let expr = self.expr(expr, out)?;
                out.push(Stmt::Expr(expr));
            }
            StmtKind::Function(_) | StmtKind::Class(_) | StmtKind::Enum(_) => {}
            StmtKind::If { condition, then_block, else_block } => {
                let condition = self.cond(condition, out)?;
                let then = self.block(then_block)?;
                let otherwise = match else_block {
                    Some(block) => Some(self.block(block)?),
                    None => None,
                };
                out.push(Stmt::If { condition, then, otherwise });
            }
            StmtKind::While { condition, body } => {
                let mut condition_out = None;
                let body = self.in_block(|this, stmts| {
                    let mut pre = Vec::new();
                    let test = this.cond(condition, &mut pre)?;
                    if pre.is_empty() {
                        condition_out = Some(test);
                    } else {
                        // Worked out again at the top of every round.
                        stmts.extend(pre);
                        let stop = Block { decls: Vec::new(), stmts: vec![Stmt::Break] };
                        stmts.push(Stmt::If { condition: Cond::Not(Box::new(test)), then: stop, otherwise: None });
                    }
                    let rest = this.in_loop(|this| this.statements(body))?;
                    stmts.extend(rest);
                    Ok(())
                })?;
                out.push(Stmt::While { condition: condition_out.unwrap_or(Cond::True), body });
            }
            StmtKind::For { variable, second, iterable, body } => {
                let items = self.expr(iterable, out)?;
                let step = if second.is_some() { "pairs" } else { "items" };
                let items = Expr::Rt { name: step, args: vec![items], line: line(iterable.span) };
                let mut vars = (0, None);
                let body = self.in_block(|this, stmts| {
                    vars.0 = this.declare_apart(variable);
                    vars.1 = second.as_ref().map(|second| this.declare_apart(second));
                    let rest = this.in_loop(|this| this.statements(body))?;
                    stmts.extend(rest);
                    Ok(())
                })?;
                out.push(Stmt::For { var: vars.0, second: vars.1, items, body });
            }
            StmtKind::Match { subject, arms } => {
                let value = self.expr(subject, out)?;
                let subject_var = match value {
                    Expr::Var(id) => id,
                    value => {
                        let t = self.temp(VarKind::Value);
                        out.push(self.set(t, value));
                        t
                    }
                };
                // From the last arm back: each `else` holds the arms after it.
                let mut otherwise =
                    Block { decls: Vec::new(), stmts: vec![Stmt::NoMatch { value: self.read(subject_var), line: at }] };
                for arm in arms.iter().rev() {
                    let mut test = Cond::True;
                    let then = self.in_block(|this, stmts| {
                        let subject = this.read(subject_var);
                        let mut bindings = Vec::new();
                        test = this.pattern(&arm.pattern, subject, &mut bindings, at)?;
                        for (id, value) in bindings {
                            stmts.push(this.set(id, value));
                        }
                        let rest = this.statements(&arm.body)?;
                        stmts.extend(rest);
                        Ok(())
                    })?;
                    otherwise = match test {
                        // A pattern that always matches: the arms after it are never reached.
                        Cond::True => then,
                        test => Block { decls: Vec::new(), stmts: vec![Stmt::If { condition: test, then, otherwise: Some(otherwise) }] },
                    };
                }
                out.push(Stmt::Block(otherwise));
            }
            StmtKind::Try { body, handlers } => {
                let body = self.block(body)?;
                let error = self.temp(VarKind::Error);
                let mut otherwise = Block { decls: Vec::new(), stmts: vec![Stmt::Reraise(error)] };
                for handler in handlers.iter().rev() {
                    let kind = handler.error.clone().unwrap_or_default();
                    let then = self.in_block(|this, stmts| {
                        if let Some(binding) = &handler.binding {
                            this.uses(error, false);
                            let caught = Expr::Rt { name: "caught", args: vec![Expr::Caught(error), Expr::Text(kind.clone())], line: at };
                            let id = this.declare(binding, Known::Nothing);
                            stmts.push(this.set(id, caught));
                        }
                        let rest = this.statements(&handler.body)?;
                        stmts.extend(rest);
                        Ok(())
                    })?;
                    otherwise = match &handler.error {
                        None => then,
                        Some(kind) => {
                            self.uses(error, false);
                            let test = Cond::Test { name: "catches", args: vec![Expr::Caught(error), Expr::Text(kind.clone())], line: at };
                            Block { decls: Vec::new(), stmts: vec![Stmt::If { condition: test, then, otherwise: Some(otherwise) }] }
                        }
                    };
                }
                self.uses(error, false);
                out.push(Stmt::Try { body, error, handler: otherwise });
            }
            StmtKind::Unsafe(_) => {
                return Err(Error::new(
                    "manual memory (`unsafe`) is only for a program compiled to machine code. \
                     Build this program without `--target`, or with `--llvm`",
                    statement.span,
                ))
            }
            // A build leaves the tests out; one met here is not run.
            StmtKind::Test { .. } => {}
            // An `assert` stands only in a test, so this is not met; its
            // text, if it has one, would go with the tests.
            StmtKind::Assert(condition, _) => {
                let test = self.cond(condition, out)?;
                let fail = Block { decls: Vec::new(), stmts: vec![Stmt::Fatal { message: "this is false".to_string(), line: at }] };
                out.push(Stmt::If { condition: Cond::Not(Box::new(test)), then: fail, otherwise: None });
            }
            StmtKind::AssertRaises { error, body } => {
                // As `try`, with a mistake when the body raises nothing or
                // something else.
                let body = self.block(body)?;
                let caught = self.temp(VarKind::Error);
                let mut body = body;
                body.stmts.push(Stmt::Fatal {
                    message: format!("this block was expected to raise {error}, and it raised nothing"),
                    line: at,
                });
                self.uses(caught, false);
                let test = Cond::Test { name: "catches", args: vec![Expr::Caught(caught), Expr::Text(error.clone())], line: at };
                let handler = Block {
                    decls: Vec::new(),
                    stmts: vec![Stmt::If {
                        condition: test,
                        then: Block::default(),
                        otherwise: Some(Block { decls: Vec::new(), stmts: vec![Stmt::Reraise(caught)] }),
                    }],
                };
                out.push(Stmt::Try { body, error: caught, handler });
            }
            StmtKind::Return(value) => {
                let value = match value {
                    Some(value) => self.expr(value, out)?,
                    None => Expr::None,
                };
                // The value is worked out first, and then each resource
                // of the function is released, last made first (59.1).
                let leaving = self.leaving(None);
                let value = if leaving.is_empty() { value } else { self.hold(value, out) };
                for id in leaving {
                    self.release(id, at, out);
                }
                out.push(Stmt::Return(value));
            }
            StmtKind::Raise(value) => {
                let value = self.expr(value, out)?;
                out.push(Stmt::Raise { value, line: at });
            }
            StmtKind::Break | StmtKind::Continue => {
                // The resources made inside the loop being left (59.1).
                let loops = self.functions.last().map_or(0, |context| context.loops);
                for id in self.leaving(Some(loops)) {
                    self.release(id, at, out);
                }
                out.push(if matches!(statement.kind, StmtKind::Break) { Stmt::Break } else { Stmt::Continue });
            }
            StmtKind::Import { module, names } => {
                let path = module.join(".");
                if module_members(&path).is_none() {
                    let why = match path.as_str() {
                        "web" => "The `web` module is for a page: build it with `--web`, or with `--target html`",
                        "board" => "The `board` module is for a program on a board: build it without `--target`",
                        "python" => "The `python` module needs Python beside the program while it runs, and `zeno run` is what starts it",
                        "node" => "The `node` module needs Node beside the program while it runs, and `zeno run` is what starts it",
                        "java" => "The `java` module needs Java beside the program while it runs, and `zeno run` is what starts it",
                        "ruby" => "The `ruby` module needs Ruby beside the program while it runs, and `zeno run` is what starts it",
                        "php" => "The `php` module needs PHP beside the program while it runs, and `zeno run` is what starts it",
                        "lua" => "The `lua` module needs Lua beside the program while it runs, and `zeno run` is what starts it",
                        "csharp" => "The `csharp` module needs .NET beside the program while it runs, and `zeno run` is what starts it",
                        "c" => "The `c` module calls a library of C, which `zeno run` does, and `zeno build`, which writes C",
                        _ => "It is not a module that comes with Zeno",
                    };
                    return Err(Error::new(format!("a program built for another language cannot use `{path}`. {why}"), statement.span));
                }
                if names.is_empty() {
                    let id = self.declare_later(module.last().map_or(path.as_str(), |m| m.as_str()));
                    out.push(self.set(id, Expr::Module(path)));
                    return Ok(());
                }
                for name in names {
                    no_target_has(&format!("{path}.{name}"), statement.span)?;
                    let members = module_members(&path).unwrap_or(&[]);
                    if !members.contains(&name.as_str()) {
                        return Err(Error::new(format!("module `{path}` has no `{name}`"), statement.span));
                    }
                    let member = format!("{path}.{name}");
                    let id = self.declare_later(name);
                    out.push(self.set(id, Expr::Builtin(member)));
                }
            }
        }
        Ok(())
    }

    /// The rest of a block after the `let` of the resource `name`
    /// (decision 59.1). Its statements are lowered in the block they are
    /// in, which declared their names already. They run in a `try` that
    /// catches whatever the program raises, releases the resource and
    /// raises it again as it was; a `return`, `break` or `continue` that
    /// leaves them releases it on its way (`leaving`); and the end of
    /// them releases it. A mistake of the program, and `program.exit`, go
    /// past the `try` of every target and release nothing (rule 6).
    fn scoped(&mut self, name: &str, body: &[ast::Stmt], at: usize, out: &mut Vec<Stmt>) -> Result<()> {
        let Some(id) = self.lookup(name) else {
            return Err(Error::new(format!("`{name}` is not declared"), Span { line: at, col: 1 }));
        };
        let loops = self.functions.last().map_or(0, |context| context.loops);
        self.functions.last_mut().expect("there is always a function").scoped.push((id, loops));
        let mut stmts = Vec::new();
        let mut done = Ok(());
        for statement in body {
            done = self.statement(statement, &mut stmts);
            if done.is_err() {
                break;
            }
        }
        self.functions.last_mut().expect("there is always a function").scoped.pop();
        done?;
        let error = self.temp(VarKind::Error);
        let mut handler = Vec::new();
        self.release(id, at, &mut handler);
        self.uses(error, false);
        handler.push(Stmt::Reraise(error));
        out.push(Stmt::Try {
            body: Block { decls: Vec::new(), stmts },
            error,
            handler: Block { decls: Vec::new(), stmts: handler },
        });
        self.release(id, at, out);
        Ok(())
    }

    /// The resources a `return` leaves (`None`), or a `break` or a
    /// `continue` of the loop that is this many loops deep: the last made
    /// first.
    fn leaving(&self, loop_at: Option<usize>) -> Vec<VarId> {
        let Some(context) = self.functions.last() else { return Vec::new() };
        context.scoped.iter().rev().filter(|(_, loops)| loop_at.is_none_or(|at| *loops == at)).map(|(id, _)| *id).collect()
    }

    /// Release the resource in `id` unless it was given away: it is taken
    /// out of the variable first, as `Interpreter::release` does, so that
    /// whatever happens in `release` it is released once.
    fn release(&mut self, id: VarId, at: usize, out: &mut Vec<Stmt>) {
        let held = self.read(id);
        let condition = Cond::Test { name: "ne", args: vec![held, Expr::None], line: at };
        let t = self.temp(VarKind::Value);
        let mut then = Vec::new();
        let value = self.read(id);
        then.push(self.set(t, value));
        then.push(self.set(id, Expr::None));
        let object = Box::new(self.read(t));
        let call = Expr::CallMethod { object, name: "release".to_string(), args: Vec::new(), names: Vec::new(), named: Vec::new(), line: at };
        then.push(Stmt::Expr(call));
        out.push(Stmt::If { condition, then: Block { decls: Vec::new(), stmts: then }, otherwise: None });
    }

    /// Lower what is inside a loop, one loop deeper.
    fn in_loop<T>(&mut self, inside: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.functions.last_mut().expect("there is always a function").loops += 1;
        let done = inside(self);
        self.functions.last_mut().expect("there is always a function").loops -= 1;
        done
    }

    /// `target = value`, or `target op= value`.
    fn assign(&mut self, target: &ast::Expr, op: Option<BinaryOp>, value: &ast::Expr, at: usize, out: &mut Vec<Stmt>) -> Result<()> {
        match &target.kind {
            ExprKind::Name(name) => {
                let Some(id) = self.lookup(name) else {
                    out.push(Stmt::Fatal { message: format!("`{name}` is not declared. Create it with `let` or `var` first"), line: at });
                    return Ok(());
                };
                let new = match op {
                    // `x += 1` reads `x`, then works out the value.
                    Some(op) => {
                        let old = self.read(id);
                        let mut pre = Vec::new();
                        let value = self.ordered(&[value], &mut pre, |args| args)?;
                        // The value may be worked out by statements, or
                        // (in Go and C++) before the variable is read:
                        // then what the variable holds now is kept first.
                        let late = !pre.is_empty() || (self.options.unordered_arguments && !value.iter().all(trivial));
                        let old = if late { self.hold(old, out) } else { old };
                        out.extend(pre);
                        let mut args = vec![old];
                        args.extend(value);
                        Expr::Rt { name: operator(op), args, line: at }
                    }
                    None => self.expr(value, out)?,
                };
                out.push(self.set(id, new));
                Ok(())
            }
            ExprKind::Field { .. } | ExprKind::Index { .. } => {
                let new = match op {
                    // What is there is read, then the value is worked
                    // out. The object is worked out again to store into,
                    // as `zeno run` does.
                    Some(op) => {
                        let old = self.expr(target, out)?;
                        let old = self.spill(old, out);
                        let value = self.expr(value, out)?;
                        Expr::Rt { name: operator(op), args: vec![old, value], line: at }
                    }
                    None => self.expr(value, out)?,
                };
                self.store(target, new, at, out)
            }
            _ => {
                out.push(Stmt::Fatal { message: "this cannot be assigned to".to_string(), line: at });
                Ok(())
            }
        }
    }

    /// `let a, b = f()`, `a, b = b, a` (decision 57). All of the right is
    /// worked out and kept first. Values written out are each kept in a
    /// temporary; what a call gives is kept in one, and each place reads
    /// its own value from it with the runtime's `received`, which also
    /// says when a program with no types gave something else.
    fn receive(&mut self, places: &[ast::Place], value: &ast::Expr, at: usize, out: &mut Vec<Stmt>) -> Result<()> {
        let count = places.len();
        let values: Vec<Expr> = match &value.kind {
            ExprKind::Several(items) => {
                let items: Vec<&ast::Expr> = items.iter().collect();
                let values = self.ordered(&items, out, |values| values)?;
                values.into_iter().map(|value| self.hold(value, out)).collect()
            }
            _ => {
                let given = self.expr(value, out)?;
                let given = self.hold(given, out);
                (0..count)
                    .map(|place| Expr::Rt {
                        name: "received",
                        args: vec![given.clone(), Expr::Count(count), Expr::Count(place)],
                        line: at,
                    })
                    .collect()
            }
        };
        for (place, value) in places.iter().zip(values) {
            match place {
                ast::Place::Name { name, .. } => {
                    let id = self.declare_later(name);
                    out.push(self.set(id, value));
                }
                // Read all the same, so that a wrong count is told.
                ast::Place::Drop => {
                    if matches!(value, Expr::Rt { .. }) {
                        out.push(Stmt::Expr(value));
                    }
                }
                ast::Place::Target(target) => self.store(target, value, at, out)?,
            }
        }
        Ok(())
    }

    /// Put `new`, worked out already, in the place `target`.
    fn store(&mut self, target: &ast::Expr, new: Expr, at: usize, out: &mut Vec<Stmt>) -> Result<()> {
        match &target.kind {
            ExprKind::Name(name) => match self.lookup(name) {
                Some(id) => out.push(self.set(id, new)),
                None => out.push(Stmt::Fatal { message: format!("`{name}` is not declared. Create it with `let` or `var` first"), line: at }),
            },
            ExprKind::Field { object, name, .. } => {
                // The value is worked out before the object it goes into.
                let mut pre = Vec::new();
                let object = self.expr(object, &mut pre)?;
                let new = if pre.is_empty() && trivial(&object) { self.spill(new, out) } else { self.hold(new, out) };
                out.extend(pre);
                out.push(Stmt::SetField { object, name: name.clone(), value: new, line: at });
            }
            ExprKind::Index { object, index } => {
                // The value is worked out before the list and the place in it.
                let mut pre = Vec::new();
                let parts = self.ordered(&[object, index], &mut pre, |args| args)?;
                let new = if pre.is_empty() && parts.iter().all(trivial) { self.spill(new, out) } else { self.hold(new, out) };
                out.extend(pre);
                let mut parts = parts.into_iter();
                let (object, index) = (parts.next().unwrap_or(Expr::None), parts.next().unwrap_or(Expr::None));
                out.push(Stmt::SetIndex { object, index, value: new, line: at });
            }
            _ => out.push(Stmt::Fatal { message: "this cannot be assigned to".to_string(), line: at }),
        }
        Ok(())
    }

    /// A `match` pattern, tried on `value`: the test, with the variables
    /// it gives a value to when it matches added to `bindings`.
    fn pattern(&mut self, pattern: &Pattern, value: Expr, bindings: &mut Vec<(VarId, Expr)>, at: usize) -> Result<Cond> {
        Ok(match pattern {
            Pattern::Wildcard => Cond::True,
            Pattern::Literal(literal) => {
                let mut pre = Vec::new();
                let literal = self.expr(literal, &mut pre)?;
                Cond::Test { name: "eq", args: vec![value, literal], line: at }
            }
            Pattern::Name(name) => {
                // A name that is a case with no values is compared; any
                // other name is a new variable that holds the value.
                if let Some(id) = self.lookup(name) {
                    if matches!(self.info[id].known, Known::Case { carries: false }) {
                        let case = self.read(id);
                        return Ok(Cond::Test { name: "eq", args: vec![value, case], line: at });
                    }
                }
                let id = self.declare(name, Known::Nothing);
                bindings.push((id, value));
                Cond::True
            }
            Pattern::Variant { name, fields } => {
                let case = match self.lookup(name) {
                    Some(id) => self.read(id),
                    None => {
                        return Err(Error::new(format!("`{name}` is not an enum case that carries values"), Span { line: at, col: 1 }))
                    }
                };
                let mut test = Cond::Test { name: "is_case", args: vec![self.copy(&value), case, Expr::Count(fields.len())], line: at };
                for (index, field) in fields.iter().enumerate() {
                    let inner = Expr::Rt { name: "case_field", args: vec![self.copy(&value), Expr::Count(index)], line: at };
                    match self.pattern(field, inner, bindings, at)? {
                        Cond::True => {}
                        more => test = Cond::And(Box::new(test), Box::new(more)),
                    }
                }
                test
            }
        })
    }

    /// The same value again, for a pattern that looks at it twice. Only
    /// what a pattern is tried on is ever copied: a variable, and the
    /// values inside it.
    fn copy(&mut self, value: &Expr) -> Expr {
        value.clone()
    }

    // ---------------------------------------------------------- Functions

    fn function(&mut self, decl: &ast::Function) -> Result<Func> {
        self.functions.push(FunctionContext::default());
        self.scopes.push(Scope::default());
        let made = (|| -> Result<(Vec<Param>, Vec<Stmt>)> {
            let mut params = Vec::new();
            for param in &decl.params {
                let id = self.declare_apart(&param.name);
                let default = match &param.default {
                    Some(default) => Some(self.in_block(|this, stmts| {
                        let value = this.expr(default, stmts)?;
                        stmts.push(this.set(id, value));
                        Ok(())
                    })?),
                    None => None,
                };
                params.push(Param { var: id, name: param.name.clone(), default });
            }
            let body = self.statements(&decl.body)?;
            Ok((params, body))
        })();
        let scope = self.scopes.pop().expect("the function's scope");
        let context = self.functions.pop().unwrap_or_default();
        let (params, stmts) = made?;
        Ok(Func {
            name: decl.name.clone(),
            params,
            body: Block { decls: scope.decls, stmts },
            nonlocals: context.nonlocals,
            captures: context.captures,
        })
    }

    fn class(&mut self, class: &ast::Class) -> Result<Class> {
        let mut fields = Vec::new();
        for field in &class.fields {
            let default = match &field.default {
                // A default is worked out at each call of the class, where
                // the class was written: a function of no arguments.
                Some(default) => {
                    let body = vec![ast::Stmt { kind: StmtKind::Return(Some((**default).clone())), span: default.span }];
                    let decl = ast::Function {
                        span: default.span,
                        name: String::new(),
                        type_params: Vec::new(),
                        params: Vec::new(),
                        return_type: None,
                        raises: Vec::new(),
                        body,
                    };
                    Some(self.function(&decl)?)
                }
                None => None,
            };
            fields.push((field.name.clone(), default));
        }
        let mut methods = Vec::new();
        // An interface has functions with no bodies: nothing to call.
        if !class.interface {
            for method in &class.methods {
                methods.push(self.function(method)?);
            }
        }
        Ok(Class { name: class.name.clone(), fields, methods })
    }

    // -------------------------------------------------------- Expressions

    /// Several expressions, worked out in the order written, and given to
    /// `make`. A later one that needs statements first has the earlier
    /// ones kept in variables, so that they are still worked out first.
    fn ordered<T>(&mut self, exprs: &[&ast::Expr], out: &mut Vec<Stmt>, make: impl FnOnce(Vec<Expr>) -> T) -> Result<T> {
        let mut done: Vec<Expr> = Vec::new();
        for expr in exprs {
            let mut pre = Vec::new();
            let value = self.expr(expr, &mut pre)?;
            if !pre.is_empty() {
                // The statements of this one run before the call they
                // are all given to: the earlier ones are worked out
                // before them, a variable among them read.
                for earlier in done.iter_mut() {
                    let value = std::mem::replace(earlier, Expr::None);
                    *earlier = self.hold(value, out);
                }
                out.extend(pre);
            }
            done.push(value);
        }
        // In Go and C++ a variable may be read after a call written to its
        // right, which may change it: everything before the last argument
        // that does anything, other than a constant, is worked out first.
        if self.options.unordered_arguments {
            if let Some(last) = done.iter().rposition(|value| !trivial(value)) {
                for earlier in done[..last].iter_mut() {
                    if !constant(earlier) {
                        let value = std::mem::replace(earlier, Expr::None);
                        // A variable too: its value now, not after the call.
                        let t = self.temp(VarKind::Value);
                        out.push(self.set(t, value));
                        *earlier = self.read(t);
                    }
                }
            }
        }
        Ok(make(done))
    }

    fn rt(&mut self, name: &'static str, exprs: &[&ast::Expr], at: usize, out: &mut Vec<Stmt>) -> Result<Expr> {
        self.ordered(exprs, out, |args| Expr::Rt { name, args, line: at })
    }

    fn expr(&mut self, expr: &ast::Expr, out: &mut Vec<Stmt>) -> Result<Expr> {
        let at = line(expr.span);
        Ok(match &expr.kind {
            // A resource given away (59.1): its value, after which the
            // variable holds `none`, so that its `Scoped` does not
            // release it.
            ExprKind::Give(name) => {
                let Some(id) = self.lookup(name) else {
                    return Err(Error::new(format!("`{name}` is not declared"), expr.span));
                };
                let t = self.temp(VarKind::Value);
                let value = self.read(id);
                out.push(self.set(t, value));
                out.push(self.set(id, Expr::None));
                self.read(t)
            }
            ExprKind::Int(n) => match i64::try_from(*n) {
                Ok(n) => Expr::Int(n),
                Err(_) => {
                    out.push(Stmt::Fatal { message: "this number is too big for an int".to_string(), line: at });
                    Expr::None
                }
            },
            ExprKind::Float(x) => Expr::Float(*x),
            ExprKind::Str(text) => Expr::Str(text.clone()),
            // A block of another language is run by that language,
            // beside `zeno run`, or is C in a build that writes C. A
            // program written out in another language has neither
            // (decision 61.7).
            ExprKind::Inline { language, .. } => {
                return Err(Error::new(
                    format!(
                        "`--target` cannot take an `inline {language}` block: the program it writes is in one other language, \
                         and the block is in {}. `zeno run` takes it{}",
                        crate::inline::LANGUAGES.iter().find(|(word, _)| word == language).map_or("another", |(_, name)| name),
                        if language == "c" { ", and so does a `zeno build` that writes C" } else { "" }
                    ),
                    expr.span,
                ));
            }
            ExprKind::Bool(b) => Expr::Bool(*b),
            ExprKind::None => Expr::None,
            ExprKind::FormatStr(parts) => {
                let values: Vec<&ast::Expr> = parts
                    .iter()
                    .filter_map(|part| match part {
                        FormatPart::Value { value, .. } => Some(value),
                        FormatPart::Text(_) => None,
                    })
                    .collect();
                let mut values = self.ordered(&values, out, |values| values)?.into_iter();
                let mut pieces = Vec::new();
                for part in parts {
                    match part {
                        FormatPart::Text(text) => pieces.push(Expr::Str(text.clone())),
                        FormatPart::Value { format, .. } => {
                            let value = values.next().unwrap_or(Expr::None);
                            let format = Expr::Text(format.clone().unwrap_or_default());
                            pieces.push(Expr::Rt { name: "format", args: vec![value, format], line: at });
                        }
                    }
                }
                Expr::Rt { name: "concat", args: vec![Expr::Array(pieces)], line: at }
            }
            ExprKind::Name(name) => self.name(name, at),
            ExprKind::Unit { value, unit } => {
                let value = self.expr(value, out)?;
                Expr::Rt { name: "unit", args: vec![value, Expr::Text(unit.clone())], line: at }
            }
            ExprKind::List(items) => {
                let items: Vec<&ast::Expr> = items.iter().collect();
                self.ordered(&items, out, |items| Expr::Rt { name: "list", args: vec![Expr::Array(items)], line: at })?
            }
            // `return a, b` (decision 57): the runtime packs them.
            ExprKind::Several(items) => {
                let items: Vec<&ast::Expr> = items.iter().collect();
                self.ordered(&items, out, |items| Expr::Rt { name: "several", args: vec![Expr::Array(items)], line: at })?
            }
            ExprKind::Dict(entries) => {
                let flat: Vec<&ast::Expr> = entries.iter().flat_map(|(key, value)| [key, value]).collect();
                self.ordered(&flat, out, |flat| Expr::Rt { name: "dict", args: vec![Expr::Array(flat)], line: at })?
            }
            ExprKind::ListComp { element, variable, iterable, condition } => {
                let source = self.expr(iterable, out)?;
                let items = Expr::Rt { name: "items", args: vec![source], line: line(iterable.span) };
                let result = self.temp(VarKind::Value);
                out.push(self.set(result, Expr::Rt { name: "list", args: vec![Expr::Array(Vec::new())], line: at }));
                let mut var = 0;
                let body = self.in_block(|this, stmts| {
                    var = this.declare_apart(variable);
                    let keep = |this: &mut Self, stmts: &mut Vec<Stmt>| -> Result<()> {
                        let value = this.expr(element, stmts)?;
                        let list = this.read(result);
                        stmts.push(Stmt::Expr(Expr::Rt { name: "push", args: vec![list, value], line: at }));
                        Ok(())
                    };
                    match condition {
                        None => keep(this, stmts),
                        Some(condition) => {
                            let test = this.cond(condition, stmts)?;
                            let then = this.in_block(keep)?;
                            stmts.push(Stmt::If { condition: test, then, otherwise: None });
                            Ok(())
                        }
                    }
                })?;
                out.push(Stmt::For { var, second: None, items, body });
                self.read(result)
            }
            ExprKind::Unary { op: UnaryOp::Not, .. } | ExprKind::Compare { .. } => {
                let test = self.cond(expr, out)?;
                Expr::FromCond(Box::new(test))
            }
            ExprKind::Unary { op, operand } => {
                let value = self.expr(operand, out)?;
                let name = if *op == UnaryOp::Neg { "neg" } else { "bitnot" };
                Expr::Rt { name, args: vec![value], line: at }
            }
            ExprKind::Binary { op, .. } if compares(*op) || matches!(op, BinaryOp::And | BinaryOp::Or) => {
                let test = self.cond(expr, out)?;
                Expr::FromCond(Box::new(test))
            }
            ExprKind::Binary { op, left, right } => self.rt(operator(*op), &[left, right], at, out)?,
            ExprKind::Function(decl) => Expr::Func(Box::new(self.function(decl)?)),
            ExprKind::Call { callee, args } => self.call(callee, args, at, out)?,
            ExprKind::Field { object, name, .. } => {
                if let ExprKind::Name(module) = &object.kind {
                    if let Some(member) = self.member(module, name) {
                        no_target_has(&member, expr.span)?;
                        return Ok(Expr::Builtin(member));
                    }
                }
                let object = self.expr(object, out)?;
                Expr::Rt { name: "field", args: vec![object, Expr::Text(name.clone())], line: at }
            }
            ExprKind::Index { object, index } => self.rt("index", &[object, index], at, out)?,
            // `take slot.task` is two statements: keep what is there, then
            // leave `none` in its place. No target's runtime changes.
            ExprKind::Take(place) => {
                let at = line(place.span);
                let (read, store) = match &place.kind {
                    ExprKind::Field { object, name, .. } => {
                        let object = self.expr(object, out)?;
                        let object = self.spill(object, out);
                        let read = Expr::Rt { name: "field", args: vec![object.clone(), Expr::Text(name.clone())], line: at };
                        (read, Stmt::SetField { object, name: name.clone(), value: Expr::None, line: at })
                    }
                    ExprKind::Index { object, index } => {
                        let parts = self.ordered(&[object, index], out, |args| args)?;
                        let mut parts = parts.into_iter().map(|part| self.spill(part, out)).collect::<Vec<_>>().into_iter();
                        let (object, index) = (parts.next().unwrap_or(Expr::None), parts.next().unwrap_or(Expr::None));
                        let read = Expr::Rt { name: "index", args: vec![object.clone(), index.clone()], line: at };
                        (read, Stmt::SetIndex { object, index, value: Expr::None, line: at })
                    }
                    // The parser lets nothing else through.
                    _ => {
                        out.push(Stmt::Fatal { message: "`take` goes in front of a field or an item".to_string(), line: at });
                        return Ok(Expr::None);
                    }
                };
                let t = self.temp(VarKind::Value);
                out.push(self.set(t, read));
                out.push(store);
                self.read(t)
            }
        })
    }

    /// A name used as a value.
    fn name(&mut self, name: &str, at: usize) -> Expr {
        match self.lookup(name) {
            Some(id) => self.read(id),
            None if self.builtins.contains(name) => Expr::Builtin(name.to_string()),
            None => Expr::Rt { name: "undefined", args: vec![Expr::Text(name.to_string())], line: at },
        }
    }

    /// `module.name`, when `module` is a module that comes with Zeno (or
    /// `u8.wrap`): its qualified name, `math.sqrt`.
    fn member(&mut self, module: &str, name: &str) -> Option<String> {
        match self.lookup(module) {
            Some(id) => match &self.info[id].known {
                Known::Module(path) => {
                    let path = path.clone();
                    self.uses(id, false);
                    Some(format!("{path}.{name}"))
                }
                _ => None,
            },
            None if name == "wrap" && crate::ints::width(module).is_some() => Some(format!("{module}.wrap")),
            None => None,
        }
    }

    fn call(&mut self, callee: &ast::Expr, args: &[ast::Arg], at: usize, out: &mut Vec<Stmt>) -> Result<Expr> {
        let plain: Vec<&ast::Expr> = args.iter().filter(|arg| arg.name.is_none()).map(|arg| &arg.value).collect();
        let named: Vec<(&String, &ast::Expr)> = args.iter().filter_map(|arg| Some((arg.name.as_ref()?, &arg.value))).collect();
        if args.iter().skip_while(|arg| arg.name.is_none()).any(|arg| arg.name.is_none()) {
            out.push(Stmt::Fatal { message: "plain arguments must come before named ones".to_string(), line: at });
            return Ok(Expr::None);
        }
        let values: Vec<&ast::Expr> = args.iter().map(|arg| &arg.value).collect();
        let names: Vec<String> = named.iter().map(|(name, _)| (*name).clone()).collect();

        // Something that comes with Zeno, called by name.
        let builtin = match &callee.kind {
            ExprKind::Name(name) => match self.lookup(name) {
                Some(id) => match &self.info[id].known {
                    Known::Member(member) => {
                        let member = member.clone();
                        self.uses(id, false);
                        Some(member)
                    }
                    _ => None,
                },
                None if self.builtins.contains(name.as_str()) => Some(name.clone()),
                None => None,
            },
            ExprKind::Field { object, name, .. } => match &object.kind {
                ExprKind::Name(module) => self.member(module, name),
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = builtin {
            no_target_has(&name, callee.span)?;
            if !named.is_empty() {
                out.push(Stmt::Fatal { message: format!("`{name}` does not take a named argument `{}`", names[0]), line: at });
                return Ok(Expr::None);
            }
            return self.ordered(&values, out, |args| Expr::CallBuiltin { name, args, line: at });
        }

        // A function of the program, known where it is written.
        if let ExprKind::Name(name) = &callee.kind {
            if let Some(id) = self.lookup(name) {
                if let Known::Function(params) = self.info[id].known.clone() {
                    if let Some(slots) = line_up(&params, plain.len(), &names) {
                        let values = self.ordered(&values, out, |values| values)?;
                        // Named arguments may be written in another order
                        // than the parameters. They are worked out in the
                        // order written, so each is kept before they are
                        // put in the order of the parameters.
                        let written: Vec<usize> = slots.iter().flatten().copied().collect();
                        let values: Vec<Expr> = if written.windows(2).all(|pair| pair[0] < pair[1]) {
                            values
                        } else {
                            values.into_iter().map(|value| self.hold(value, out)).collect()
                        };
                        let mut values: Vec<Option<Expr>> = values.into_iter().map(Some).collect();
                        let args = slots.into_iter().map(|slot| slot.and_then(|at| values[at].take()).unwrap_or(Expr::Missing)).collect();
                        self.uses(id, false);
                        return Ok(Expr::CallDirect { function: id, args, line: at });
                    }
                }
            }
        }

        // `object.name(...)`: a method.
        if let ExprKind::Field { object, name, .. } = &callee.kind {
            let mut all = vec![&**object];
            all.extend(values.iter().copied());
            let count = plain.len();
            return self.ordered(&all, out, |mut all| {
                let object = all.remove(0);
                let named = all.split_off(count);
                Expr::CallMethod { object: Box::new(object), name: name.clone(), args: all, names, named, line: at }
            });
        }

        let mut all = vec![callee];
        all.extend(values.iter().copied());
        let count = plain.len();
        self.ordered(&all, out, |mut all| {
            let callee = Box::new(all.remove(0));
            if names.is_empty() {
                Expr::Call { callee, args: all, line: at }
            } else {
                let named = all.split_off(count);
                Expr::CallNamed { callee, args: all, names, named, line: at }
            }
        })
    }

    // --------------------------------------------------------- Conditions

    /// An expression used as a test.
    fn cond(&mut self, expr: &ast::Expr, out: &mut Vec<Stmt>) -> Result<Cond> {
        let at = line(expr.span);
        Ok(match &expr.kind {
            ExprKind::Bool(true) => Cond::True,
            ExprKind::Unary { op: UnaryOp::Not, operand } => Cond::Not(Box::new(self.cond(operand, out)?)),
            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                let left = self.cond(left, out)?;
                let mut pre = Vec::new();
                let right_test = self.cond(right, &mut pre)?;
                if pre.is_empty() {
                    match op {
                        BinaryOp::And => Cond::And(Box::new(left), Box::new(right_test)),
                        _ => Cond::Or(Box::new(left), Box::new(right_test)),
                    }
                } else {
                    // The right side needs statements, which run only
                    // when the left side has not decided already.
                    let t = self.temp(VarKind::Value);
                    out.push(self.set(t, Expr::FromCond(Box::new(left))));
                    let decided = Cond::Truth(self.read(t), at);
                    let undecided = if *op == BinaryOp::And { decided } else { Cond::Not(Box::new(decided)) };
                    pre.push(self.set(t, Expr::FromCond(Box::new(right_test))));
                    out.push(Stmt::If { condition: undecided, then: Block { decls: Vec::new(), stmts: pre }, otherwise: None });
                    Cond::Truth(self.read(t), at)
                }
            }
            ExprKind::Binary { op, left, right } if compares(*op) => {
                let name = operator(*op);
                self.ordered(&[left, right], out, |args| Cond::Test { name, args, line: at })?
            }
            ExprKind::Compare { first, rest } => {
                // `a < b < c`: each operand once, and none after the
                // first link that is false.
                let first = self.expr(first, out)?;
                let result = self.temp(VarKind::Value);
                let mut links = rest.iter();
                let Some((op, operand)) = links.next() else {
                    return Ok(Cond::True);
                };
                let mut pre = Vec::new();
                let right = self.expr(operand, &mut pre)?;
                // The left side is worked out first. If it is a variable
                // and the right side does anything, it is read first.
                let mut left = if pre.is_empty() && trivial(&right) { self.spill(first, out) } else { self.hold(first, out) };
                out.extend(pre);
                let right = self.spill(right, out);
                let test = Cond::Test { name: operator(*op), args: vec![left, self.copy(&right)], line: at };
                out.push(self.set(result, Expr::FromCond(Box::new(test))));
                left = right;
                self.links(links.collect(), left, result, at, out)?;
                Cond::Truth(self.read(result), at)
            }
            _ => {
                let value = self.expr(expr, out)?;
                Cond::Truth(value, at)
            }
        })
    }

    /// The links of a chain of comparisons after the first, each worked
    /// out only when the ones before it held.
    fn links(&mut self, links: Vec<&(BinaryOp, ast::Expr)>, left: Expr, result: VarId, at: usize, out: &mut Vec<Stmt>) -> Result<()> {
        let Some(((op, operand), rest)) = links.split_first() else {
            return Ok(());
        };
        let held = Cond::Truth(self.read(result), at);
        let then = self.in_block(|this, stmts| {
            let mut pre = Vec::new();
            let right = this.expr(operand, &mut pre)?;
            let left = if pre.is_empty() && trivial(&right) { left } else { this.hold(left, stmts) };
            stmts.extend(pre);
            let right = this.spill(right, stmts);
            let test = Cond::Test { name: operator(*op), args: vec![left, this.copy(&right)], line: at };
            stmts.push(this.set(result, Expr::FromCond(Box::new(test))));
            this.links(rest.to_vec(), right, result, at, stmts)
        })?;
        out.push(Stmt::If { condition: held, then, otherwise: None });
        Ok(())
    }
}

/// Line up the arguments of a call with the parameters of a function
/// known where it is written: for each parameter, which argument (by its
/// place among all the arguments) it gets, or `None` for one left out
/// that has a default. `None` overall when the call does not fit, which
/// the runtime then reports as `zeno run` would.
fn line_up(params: &[(String, bool)], plain: usize, names: &[String]) -> Option<Vec<Option<usize>>> {
    if plain > params.len() {
        return None;
    }
    let mut slots: Vec<Option<usize>> = (0..params.len()).map(|at| (at < plain).then_some(at)).collect();
    for (nth, name) in names.iter().enumerate() {
        let at = params.iter().position(|(param, _)| param == name)?;
        if slots[at].is_some() {
            return None;
        }
        slots[at] = Some(plain + nth);
    }
    for (slot, (_, has_default)) in slots.iter().zip(params) {
        if slot.is_none() && !has_default {
            return None;
        }
    }
    Some(slots)
}

/// Every step of a runtime that `Expr::Rt` names, with whether it is
/// given the line it is on (to say where, if it stops the program).
/// Each runtime has all of them.
pub const STEPS: &[(&str, bool)] = &[
    ("case", false),       // (enum, count): the value of a case, or what makes one
    ("items", true),       // (value): what a one-name `for` steps through, as a copy
    ("pairs", true),       // (value): the same for a two-name `for`, each a pair
    ("caught", false),     // (caught, text): what `except Kind as e` calls `e`
    ("unit", true),        // (value, text): `500 ms`
    ("list", false),       // (array): a new list
    ("dict", true),        // (array): a new dict from key, value, key, value...
    ("push", false),       // (list, value): add to the end of a list
    ("format", true),      // (value, text): `{value:format}` as a str
    ("concat", false),     // (array of str): one str
    ("field", true),       // (value, text): `value.name`
    ("index", true),       // (value, value): `value[index]`
    ("neg", true),         // (value): `-x`
    ("bitnot", true),      // (value): `~x`
    ("add", true),         // (value, value): `+`, and the same for each below
    ("sub", true),
    ("mul", true),
    ("div", true),
    ("floordiv", true),
    ("mod", true),
    ("pow", true),
    ("bitand", true),
    ("bitor", true),
    ("bitxor", true),
    ("shl", true),
    ("shr", true),
    ("case_field", false), // (value, count): the nth value an enum case carries
    ("several", false),    // (array): what `return a, b` gives (decision 57)
    ("received", true),    // (value, count, count): place n of so many on the left of `=`
    ("undefined", true),   // (text): a name that is not defined, met while running
];

/// Every test that `Cond::Test` names, with whether it is given the line.
pub const TESTS: &[(&str, bool)] = &[
    ("eq", false),
    ("ne", false),
    ("lt", true),
    ("le", true),
    ("gt", true),
    ("ge", true),
    ("in", true),
    ("not_in", true),
    ("is_case", true),  // (value, case, count): is the value that case, carrying count values?
    ("catches", false), // (caught, text): does `except text` catch it?
];

/// Does this step or test take the line it is on?
pub fn takes_line(name: &str) -> bool {
    STEPS.iter().chain(TESTS).find(|(step, _)| *step == name).is_some_and(|(_, line)| *line)
}

/// A float written as Rust writes it with `{:?}`, which every target
/// reads as the same number: `1.5`, `1e16`, `2.0`. Not for infinity or NaN.
pub fn float_literal(x: f64) -> String {
    format!("{x:?}")
}

/// Text as a string literal in double quotes, in the escapes that
/// JavaScript, Java, C#, Go, Rust, C++ and Python all read the same way:
/// `\\`, `\"`, `\n`, `\r`, `\t`, and `\u` with four hex digits for any
/// other character below a space. `wide` writes the characters above
/// U+FFFF as two `\u` escapes (JavaScript, Java, C#); otherwise they stay
/// as they are, in UTF-8.
pub fn quoted(text: &str, escape: fn(u32) -> String) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&escape(c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    /// `program.run` starts another program, and no target does that
    /// yet. The build says so, with the place, however it is written.
    #[test]
    fn what_no_target_has_is_refused_when_built() {
        let refused = |source: &str| {
            let program = crate::modules::testing::built(source);
            super::lower(&program, vec!["test.zeno".to_string()], super::Options::default()).err().map(|error| error.message)
        };
        let message = "a program built for another language cannot use `program.run` yet. `zeno run` can still run this program";
        assert_eq!(refused("import program\nprint(program.run(\"ls\", []))").as_deref(), Some(message));
        assert_eq!(refused("from program import run\nprint(run(\"ls\", []))").as_deref(), Some(message));
        assert_eq!(refused("import program\nlet start = program.run\nprint(start(\"ls\", []))").as_deref(), Some(message));
        assert_eq!(
            refused("import python\nprint(python.module(\"math\").pi)").as_deref(),
            Some(
                "a program built for another language cannot use `python`. The `python` module needs Python beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import node\nprint(node.module(\"os\").EOL)").as_deref(),
            Some(
                "a program built for another language cannot use `node`. The `node` module needs Node beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import java\nprint(java.type(\"java.lang.Math\").PI)").as_deref(),
            Some(
                "a program built for another language cannot use `java`. The `java` module needs Java beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import ruby\nprint(ruby.constant(\"Math\").PI)").as_deref(),
            Some(
                "a program built for another language cannot use `ruby`. The `ruby` module needs Ruby beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import php\nprint(php.constant(\"M_PI\"))").as_deref(),
            Some(
                "a program built for another language cannot use `php`. The `php` module needs PHP beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import lua\nprint(lua.global(\"math\").pi)").as_deref(),
            Some(
                "a program built for another language cannot use `lua`. The `lua` module needs Lua beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        assert_eq!(
            refused("import csharp\nprint(csharp.type(\"System.Math\").PI)").as_deref(),
            Some(
                "a program built for another language cannot use `csharp`. The `csharp` module needs .NET beside \
                 the program while it runs, and `zeno run` is what starts it"
            )
        );
        // The rest of the module is there as before.
        assert_eq!(refused("import program\nprint(program.arguments())"), None);
    }
}
