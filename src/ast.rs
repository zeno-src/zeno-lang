//! The syntax tree: what a Zeno program looks like once the parser has
//! worked out its structure.
//!
//! Tokens are flat ("let", "x", "=", "1", "+", "2"). The tree is nested:
//! a `Let` statement that holds a `Binary` expression that holds two numbers.
//! Every later stage (interpreter, type checker, code generator) walks this tree.
//!
//! There are two main kinds of node:
//! - a **statement** does something: declare a variable, loop, return.
//! - an **expression** produces a value: `1 + 2`, `area(shape)`, `user.name`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ints::Width;
use crate::token::Span;

/// A whole source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// True when the file begins with the line `strict`, which asks for
    /// every type to be known before the program runs.
    pub strict: bool,
    pub statements: Vec<Stmt>,
    /// The names the file declares with `private` in front: what it
    /// declares at its top and keeps to itself.
    pub private: Vec<String>,
    /// The other files of the program, when it has several: the files it
    /// imports, and the files those import. `modules.rs` fills this in,
    /// and puts their statements in front of this file's.
    pub modules: Vec<ModuleFile>,
    /// What a line `memory owned` or `memory collector` at the top of the
    /// file says, if it has one (decision 22.9).
    pub memory: Option<Memory>,
}

/// How a file asks for its memory to be freed: `memory owned` or `memory
/// collector` at its top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    Collector,
    Owned,
}

impl Program {
    /// A build leaves the tests of a program out. They are not in what
    /// is built, and they are not held to the rules of the build: a
    /// test may share a list in a program that is built with no
    /// collector (decision 10.20).
    pub fn without_tests(mut self) -> Program {
        self.statements.retain(|statement| !matches!(statement.kind, StmtKind::Test { .. }));
        self
    }

    /// `count += 1` on a `u8` means `count = u8(count + 1)`: the sum is
    /// an int, and it is checked to fit where it is stored (decision
    /// 10.27). The checker finds each such statement and says where it
    /// is, with the size of what it stores into, and here each one is
    /// made the statement it means. Every engine then runs or compiles
    /// the long form, which it could do already, so there is nothing
    /// for the four of them to do alike.
    pub fn with_fits(mut self, fits: &HashMap<(usize, usize), Width>) -> Program {
        let mut visit = |node: Node| {
            let Node::Stmt(statement) = node else {
                return;
            };
            let span = statement.span;
            let StmtKind::Assign { target, op, value } = &mut statement.kind else {
                return;
            };
            let (Some(operator), Some(width)) = (*op, fits.get(&(span.line, span.col))) else {
                return;
            };
            let nothing = Expr { kind: ExprKind::None, span };
            let sum = ExprKind::Binary {
                op: operator,
                left: Box::new(target.clone()),
                right: Box::new(std::mem::replace(value, nothing)),
            };
            let convert = Expr { kind: ExprKind::Name(width.name.to_string()), span };
            let args = vec![Arg { name: None, value: Expr { kind: sum, span } }];
            *value = Expr { kind: ExprKind::Call { callee: Box::new(convert), args }, span };
            *op = None;
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }

    /// `let gain: f32 = 0.5` means `let gain: f32 = f32(0.5)`, and an f32
    /// that goes where a float is wanted is `float( )` of it (decision
    /// 41). The checker says where each such float and f32 begins, and
    /// here each is put in its call. Every engine then meets only `f32( )`
    /// and `float( )`, which it can do already.
    pub fn with_f32s(mut self, mut f32s: HashSet<(usize, usize)>, mut widened: HashSet<(usize, usize)>) -> Program {
        let mut visit = |node: Node| {
            let Node::Expr(expr) = node else { return };
            let span = expr.span;
            let name = if matches!(expr.kind, ExprKind::Float(_)) && f32s.remove(&(span.line, span.col)) {
                "f32"
            } else if widened.remove(&(span.line, span.col)) {
                "float"
            } else {
                return;
            };
            let value = std::mem::replace(expr, Expr { kind: ExprKind::None, span });
            let callee = Box::new(Expr { kind: ExprKind::Name(name.to_string()), span });
            expr.kind = ExprKind::Call { callee, args: vec![Arg { name: None, value }] };
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }

    /// The places where a u64 is written or worked out in a way that
    /// every engine should meet as a plain call (decision 42):
    ///
    /// - a whole number written where a u64 is wanted becomes
    ///   `u64..of(n)`, and one past the largest int is written as the int
    ///   with the same 64 bits: `18446744073709551615` is `u64..of(-1)`;
    /// - `a * b` straight inside `u64.wrap( )` becomes `u64..mul(a, b)`,
    ///   and so do `+`, `-` and `<<`, found by where their right side is;
    /// - `u8.max` becomes `u8(255)`, `u64.wrap` becomes `u64..of`.
    ///
    /// No name a program writes has two dots in a row.
    pub fn with_u64s(
        mut self,
        mut wholes: HashSet<(usize, usize)>,
        mut wrapping: HashMap<(usize, usize), &'static str>,
        mut limits: HashMap<(usize, usize), Expr>,
    ) -> Program {
        let mut visit = |node: Node| {
            let Node::Expr(expr) = node else { return };
            let span = expr.span;
            let key = (span.line, span.col);
            match &mut expr.kind {
                ExprKind::Int(n) if wholes.remove(&key) => {
                    let n = *n;
                    let number = |n: u64| Expr { kind: ExprKind::Int(n), span };
                    let negative = |n: u64| Expr { kind: ExprKind::Unary { op: UnaryOp::Neg, operand: Box::new(number(n)) }, span };
                    let bits = if n <= i64::MAX as u64 {
                        number(n)
                    } else if n > 1 << 63 {
                        negative(n.wrapping_neg())
                    } else {
                        // 2 to the 63rd: the smallest int, written as the
                        // largest, negated, less one.
                        let left = Box::new(negative(i64::MAX as u64));
                        Expr { kind: ExprKind::Binary { op: BinaryOp::Sub, left, right: Box::new(number(1)) }, span }
                    };
                    let callee = Box::new(Expr { kind: ExprKind::Name("u64..of".to_string()), span });
                    expr.kind = ExprKind::Call { callee, args: vec![Arg { name: None, value: bits }] };
                }
                ExprKind::Binary { right, .. } if wrapping.contains_key(&(right.span.line, right.span.col)) => {
                    let function = wrapping.remove(&(right.span.line, right.span.col)).unwrap_or("u64..add");
                    let ExprKind::Binary { left, right, .. } = std::mem::replace(&mut expr.kind, ExprKind::None) else {
                        return;
                    };
                    let callee = Box::new(Expr { kind: ExprKind::Name(function.to_string()), span });
                    let args = vec![Arg { name: None, value: *left }, Arg { name: None, value: *right }];
                    expr.kind = ExprKind::Call { callee, args };
                }
                ExprKind::Field { .. } => {
                    if let Some(value) = limits.remove(&key) {
                        *expr = value;
                    }
                }
                _ => {}
            }
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }

    /// The `Resource` that comes with Zeno (decision 59.1), joined in
    /// ahead of everything when a class of the program says it is one and
    /// the program has no `Resource` of its own. It is written at line 0,
    /// which no file has, and that is how the checker knows it. A program
    /// that does not say `is Resource` is left as it was.
    pub fn with_resource(mut self) -> Program {
        let mut says = false;
        let mut has_own = false;
        for statement in &self.statements {
            match &statement.kind {
                StmtKind::Class(class) => {
                    says |= class.is.iter().any(|is| matches!(is, Type::Named { name, args } if name == "Resource" && args.is_empty()));
                    has_own |= class.name == "Resource";
                }
                StmtKind::Function(function) => has_own |= function.name == "Resource",
                StmtKind::Enum(decl) => has_own |= decl.name == "Resource",
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => has_own |= name == "Resource",
                _ => {}
            }
        }
        if !says || has_own {
            return self;
        }
        let span = Span { line: 0, col: 0 };
        let release = Function {
            span,
            name: "release".to_string(),
            type_params: Vec::new(),
            params: vec![Param { name: "self".to_string(), ty: None, default: None }],
            return_type: None,
            raises: Vec::new(),
            body: Vec::new(),
        };
        let resource = Class {
            name: "Resource".to_string(),
            type_params: Vec::new(),
            owned: true,
            fields: Vec::new(),
            methods: vec![Arc::new(release)],
            private: Vec::new(),
            is: Vec::new(),
            interface: true,
        };
        self.statements.insert(0, Stmt { kind: StmtKind::Class(resource), span });
        self
    }

    /// What the checker found of resources (decision 59.1), made into
    /// what it means. After the `let` of a resource, the rest of its block
    /// goes inside a `Scoped` that releases it; so does the body of a
    /// function that takes one as a parameter, the first such parameter
    /// outermost; and each name read to give one away becomes a `Give`.
    /// Places are told by where they are written.
    pub fn with_resources(
        mut self,
        lets: HashSet<(usize, usize)>,
        params: HashSet<(usize, usize, String)>,
        gives: HashSet<(usize, usize)>,
    ) -> Program {
        let taken = |function: &mut Arc<Function>| {
            let names: Vec<String> = function
                .params
                .iter()
                .filter(|param| params.contains(&(function.span.line, function.span.col, param.name.clone())))
                .map(|param| param.name.clone())
                .collect();
            if names.is_empty() {
                return;
            }
            let function = Arc::make_mut(function);
            for name in names.into_iter().rev() {
                let body = std::mem::take(&mut function.body);
                function.body = vec![Stmt { kind: StmtKind::Scoped { name, body }, span: function.span }];
            }
        };
        let mut visit = |node: Node| match node {
            Node::Block(block) => {
                let at = block.iter().position(|statement| {
                    matches!(statement.kind, StmtKind::Let { .. }) && lets.contains(&(statement.span.line, statement.span.col))
                });
                if let Some(at) = at {
                    let StmtKind::Let { name, .. } = &block[at].kind else { return };
                    let (name, span) = (name.clone(), block[at].span);
                    let body = block.split_off(at + 1);
                    block.push(Stmt { kind: StmtKind::Scoped { name, body }, span });
                }
            }
            Node::Stmt(Stmt { kind: StmtKind::Function(function), .. }) => taken(function),
            Node::Stmt(Stmt { kind: StmtKind::Class(class), .. }) => class.methods.iter_mut().for_each(|method| taken(method)),
            Node::Expr(Expr { kind: ExprKind::Function(function), .. }) => taken(function),
            Node::Expr(expr) => {
                if let ExprKind::Name(name) = &expr.kind {
                    if gives.contains(&(expr.span.line, expr.span.col)) {
                        expr.kind = ExprKind::Give(name.clone());
                    }
                }
            }
            Node::Stmt(_) => {}
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }

    /// `for line in source` over a `files.Reader` (decision 59.7), at the
    /// places the checker found, made into the loop it means:
    ///
    /// ```text
    /// while true {
    ///     let line..next = source.read_line()
    ///     if line..next is none { break }
    ///     let line = line..next
    ///     ...
    /// }
    /// ```
    ///
    /// No name a program writes has two dots in a row, so `line..next`
    /// is no one else's. `break` and `continue` mean what they mean in
    /// any `for`, and every engine, build and target runs a `while`, a
    /// method call and an `if`, which they all could already.
    pub fn with_readers(mut self, readers: &HashSet<(usize, usize)>) -> Program {
        if readers.is_empty() {
            return self;
        }
        let mut visit = |node: Node| {
            let Node::Stmt(statement) = node else { return };
            if !matches!(statement.kind, StmtKind::For { .. }) || !readers.contains(&(statement.span.line, statement.span.col)) {
                return;
            }
            let span = statement.span;
            let StmtKind::For { variable, iterable, body, .. } = std::mem::replace(&mut statement.kind, StmtKind::Break) else {
                return;
            };
            let next = format!("{variable}..next");
            let at = |kind| Expr { kind, span };
            let method = at(ExprKind::Field { object: Box::new(iterable), name: "read_line".to_string(), at: span });
            let read = at(ExprKind::Call { callee: Box::new(method), args: Vec::new() });
            let ended = at(ExprKind::Binary {
                op: BinaryOp::Is,
                left: Box::new(at(ExprKind::Name(next.clone()))),
                right: Box::new(at(ExprKind::None)),
            });
            let mut statements = vec![
                Stmt { kind: StmtKind::Let { mutable: false, name: next.clone(), ty: None, value: read }, span },
                Stmt { kind: StmtKind::If { condition: ended, then_block: vec![Stmt { kind: StmtKind::Break, span }], else_block: None }, span },
                Stmt { kind: StmtKind::Let { mutable: false, name: variable, ty: None, value: at(ExprKind::Name(next)) }, span },
            ];
            statements.extend(body);
            statement.kind = StmtKind::While { condition: at(ExprKind::Bool(true)), body: statements };
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }

    /// `int(Ack)` and `Reply(6)`, for an enum whose cases have numbers
    /// (decision 35.2). The checker says where each such call is, and
    /// which enums they use. Each of those enums gets two functions,
    /// written beside it in the same block:
    ///
    /// ```text
    /// function Reply..number(value: Reply) -> int {
    ///     match value { case Ack { return 6 }  case Nak { return 21 } }
    /// }
    /// function Reply..case(number: int) -> Reply or none {
    ///     if number == 6 { return Ack }
    ///     if number == 21 { return Nak }
    ///     return none
    /// }
    /// ```
    ///
    /// and `int(Ack)` becomes `int(Reply..number(Ack))`, `Reply(6)`
    /// becomes `Reply..case(6)`. No name a program writes has two dots in
    /// a row, so these can be no one else's. Every engine, build and
    /// target then runs plain functions, a `match` and an `if`, which
    /// they all could already.
    pub fn with_numbers(mut self, mut calls: HashMap<(usize, usize), Numbered>, enums: &HashSet<(usize, usize)>) -> Program {
        let mut visit = |node: Node| match node {
            Node::Block(block) => {
                let mut at = 0;
                while at < block.len() {
                    let statement = &block[at];
                    at += 1;
                    let StmtKind::Enum(decl) = &statement.kind else { continue };
                    if !enums.contains(&(statement.span.line, statement.span.col)) {
                        continue;
                    }
                    let written = number_functions(decl, statement.span);
                    let count = written.len();
                    block.splice(at..at, written);
                    at += count;
                }
            }
            Node::Expr(expr) => {
                let span = expr.span;
                let ExprKind::Call { callee, args } = &mut expr.kind else { return };
                match calls.remove(&(span.line, span.col)) {
                    Some(Numbered::Case(name)) => {
                        callee.kind = ExprKind::Name(format!("{name}..case"));
                    }
                    Some(Numbered::Number(name)) => {
                        let Some(arg) = args.first_mut() else { return };
                        let at = arg.value.span;
                        let value = std::mem::replace(&mut arg.value, Expr { kind: ExprKind::None, span: at });
                        let number = Box::new(Expr { kind: ExprKind::Name(format!("{name}..number")), span: at });
                        let args = vec![Arg { name: None, value }];
                        arg.value = Expr { kind: ExprKind::Call { callee: number, args }, span: at };
                    }
                    None => {}
                }
            }
            Node::Stmt(_) => {}
        };
        fresh_block(&mut self.statements, &mut visit);
        self
    }
}

/// A call the checker found that `Program::with_numbers` rewrites: the
/// enum's name, as the program's tree has it.
#[derive(Debug, Clone, PartialEq)]
pub enum Numbered {
    /// `int(Ack)`, `u8(Ack)`: the case's number.
    Number(String),
    /// `Reply(6)`: the case with that number, or `none`.
    Case(String),
}

/// The two functions of `Program::with_numbers`, for one enum, placed
/// where the enum is written.
fn number_functions(decl: &Enum, span: Span) -> Vec<Stmt> {
    let expr = |kind: ExprKind| Expr { kind, span };
    let named = |name: &str| Type::Named { name: name.to_string(), args: Vec::new() };
    let number = |n: i128| match n < 0 {
        true => expr(ExprKind::Unary { op: UnaryOp::Neg, operand: Box::new(expr(ExprKind::Int(n.unsigned_abs() as u64))) }),
        false => expr(ExprKind::Int(n as u64)),
    };
    let give = |value: Expr| Stmt { kind: StmtKind::Return(Some(value)), span };
    let function = |name: String, param: (&str, Type), returns: Type, body: Block| Stmt {
        kind: StmtKind::Function(Arc::new(Function {
            span,
            name,
            type_params: Vec::new(),
            params: vec![Param { name: param.0.to_string(), ty: Some(param.1), default: None }],
            return_type: Some(returns),
            raises: Vec::new(),
            body,
        })),
        span,
    };
    let cases = decl.variants.iter().filter_map(|case| case.number.map(|(n, _)| (case.name.as_str(), n)));
    let arms = cases
        .clone()
        .map(|(case, n)| Arm { pattern: Pattern::Name(case.to_string()), body: vec![give(number(n))] })
        .collect();
    let match_ = Stmt { kind: StmtKind::Match { subject: expr(ExprKind::Name("value".to_string())), arms }, span };
    let mut tests: Block = cases
        .map(|(case, n)| Stmt {
            kind: StmtKind::If {
                condition: expr(ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(expr(ExprKind::Name("number".to_string()))),
                    right: Box::new(number(n)),
                }),
                then_block: vec![give(expr(ExprKind::Name(case.to_string())))],
                else_block: None,
            },
            span,
        })
        .collect();
    tests.push(give(expr(ExprKind::None)));
    vec![
        function(format!("{}..number", decl.name), ("value", named(&decl.name)), named("int"), vec![match_]),
        function(
            format!("{}..case", decl.name),
            ("number", named("int")),
            Type::Optional(Box::new(named(&decl.name))),
            tests,
        ),
    ]
}

/// A file that the program imports.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleFile {
    /// Where the file is, as an error message names it.
    pub path: String,
    /// True when this file begins with the line `strict`.
    pub strict: bool,
    /// What its line `memory ...` says, if it has one.
    pub memory: Option<Memory>,
}

/// The name a class, an enum or a case is printed by.
///
/// Inside the compiler a name from an imported file has the file's module
/// name in front, so that two files can each have a `Point`: the one in
/// `shapes.zeno` is `shapes.Point`. A copy of a generic class has its
/// types behind it: `Stack[int]`. A running program prints neither. It
/// prints `Point(x=1, y=2)` and `Stack(items=[1])`, the name as the
/// file that declares it wrote it.
pub fn shown_name(name: &str) -> &str {
    let name = name.split('[').next().unwrap_or(name);
    name.rsplit('.').next().unwrap_or(name)
}

/// The statements between `{` and `}`.
pub type Block = Vec<Stmt>;

// ---------------------------------------------------------------- Statements

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    /// Where the statement starts, for error messages.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// `let x = 1` (mutable: false) or `var x = 1` (mutable: true)
    Let { mutable: bool, name: String, ty: Option<Type>, value: Expr },
    /// `let q, r = divide(7, 2)`, `var q, r = ...`, or `low, high = high,
    /// low` for places that exist (decision 57). `declare` is `Some(false)`
    /// for `let`, `Some(true)` for `var`, and `None` for neither. The
    /// right is one call that gives as many values as there are places,
    /// or that many values written out (`ExprKind::Several`).
    Receive { declare: Option<bool>, places: Vec<Place>, value: Expr },
    /// `const PI = 3.14`
    Const { name: String, ty: Option<Type>, value: Expr },
    /// `x = 2`, or with an operator: `x += 2` has `op: Some(Add)`
    Assign { target: Expr, op: Option<BinaryOp>, value: Expr },
    /// An expression used as a statement, usually a call: `print("hi")`
    Expr(Expr),

    Function(Arc<Function>),
    Class(Class),
    Enum(Enum),

    /// `else if` is stored as an else block holding one `If` statement.
    If { condition: Expr, then_block: Block, else_block: Option<Block> },
    While { condition: Expr, body: Block },
    /// `for variable in iterable { body }`. With two names, `for key,
    /// value in ages`, `variable` is the key (or the position, in a
    /// list) and `second` is what is there.
    For { variable: String, second: Option<String>, iterable: Expr, body: Block },
    Match { subject: Expr, arms: Vec<Arm> },
    Try { body: Block, handlers: Vec<Handler> },
    /// `unsafe { ... }`: the only place where a pointer may be read
    /// through, written through or freed. See docs/unsafe.md.
    Unsafe(Block),

    /// `test "a new ring is empty" { ... }`, at the top of a file. Run
    /// by `zeno test`, and left out of a build. See docs/testing.md.
    Test { name: String, body: Block },
    /// `assert total == 6`: what must be so, in a test. With
    /// `assert total == 6, "the total"`, the text that a failure prints
    /// above what was false; it is worked out only when the `assert`
    /// fails (decision 60.1).
    Assert(Expr, Option<Expr>),
    /// `assert raises ValueError { ... }`: the block must raise that.
    AssertRaises { error: String, body: Block },

    Return(Option<Expr>),
    Raise(Expr),
    Break,
    Continue,

    /// `import a.b` has no names. `from a.b import x, y` has names x and y.
    Import { module: Vec<String>, names: Vec<String> },

    /// What follows the `let` of a resource, to the end of its block:
    /// `body` runs, and however it is left, if `name` still holds its
    /// value, that value's `release` runs, and then the leaving goes on
    /// as it was (decision 59.1). Only the checker makes it, and no
    /// program can write it. It opens no scope of its own: a name its
    /// body declares belongs to the block around it.
    Scoped { name: String, body: Block },
}

/// One place of a `Receive`.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    /// A new name, after `let` or `var`, with the type written beside it
    /// if there is one.
    Name { name: String, ty: Option<Type> },
    /// `_`: the value is received and dropped.
    Drop,
    /// A place that exists: a variable, a field or an item.
    Target(Expr),
}

impl Place {
    /// The name a `let a, b = ...` declares here, if it declares one.
    pub fn declared(&self) -> Option<&String> {
        match self {
            Place::Name { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The variable a `a, b = ...` gives a new value here, if it is one.
    pub fn assigned(&self) -> Option<&String> {
        match self {
            Place::Target(Expr { kind: ExprKind::Name(name), .. }) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// Where it is written: the word `function`, or the `(` of one that
    /// has no such word in front (a function of an interface, or one
    /// written as a value). A mistake in the first line of a method is
    /// shown here.
    pub span: Span,
    /// Empty for a function written as a value: `function(n) { return n * 2 }`
    pub name: String,
    /// `function first[T](items: list[T]) -> T` has the one type
    /// parameter `T`: a type that each call fills in.
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    /// The errors named after `raises`.
    pub raises: Vec<String>,
    pub body: Block,
}

/// One type parameter: `T`, or `T is Ordered`, which asks that what
/// `T` stands for is that interface (decision 58.3).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    /// The interface after `is`.
    pub bound: Option<Type>,
}

impl TypeParam {
    /// A type parameter with no bound.
    #[cfg(test)]
    pub fn plain(name: &str) -> TypeParam {
        TypeParam { name: name.to_string(), bound: None }
    }
}

/// The names of these type parameters, in order.
pub fn param_names(params: &[TypeParam]) -> Vec<String> {
    params.iter().map(|param| param.name.clone()).collect()
}

/// One parameter: `who`, `who: str`, or `excited: bool = false`.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
    /// Shared, so that a compiler that copies the parameter list keeps
    /// the node the checker saw, and what the checker noted about it.
    pub default: Option<Arc<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: String,
    /// `class Stack[T]` has the one type parameter `T`.
    pub type_params: Vec<TypeParam>,
    /// `owned class`: every object of it has exactly one owner.
    pub owned: bool,
    pub fields: Vec<Field>,
    pub methods: Vec<Arc<Function>>,
    /// The fields and methods with `private` in front: only the file
    /// the class is written in can use them.
    pub private: Vec<String>,
    /// `class Circle is Shape, Named`: the interfaces the class says it
    /// is. It must have every function they list. Each is a type, since
    /// an interface may have types of its own: `is Source[int]` (58.6).
    pub is: Vec<Type>,
    /// True for `interface Shape { ... }`. An interface is kept as a
    /// class with no fields, whose functions have no bodies: it is a
    /// type that objects of other classes are seen as.
    pub interface: bool,
}

impl Function {
    /// A copy of this function that shares nothing with it.
    ///
    /// The stages after the parser keep notes about a function by where
    /// its nodes are in memory. A generic function is checked and compiled
    /// once for each set of types it is used with, and each of those needs
    /// notes of its own, so each gets a copy of the tree. `clone` alone is
    /// not enough: a function written inside this one is behind an `Arc`,
    /// which `clone` shares instead of copying.
    pub fn fresh(&self) -> Function {
        self.fresh_with(&mut |_| {})
    }

    /// `fresh`, with every node of the copy shown to `visit` on the
    /// way, which may change it.
    fn fresh_with(&self, visit: &mut dyn FnMut(Node)) -> Function {
        let mut copy = self.clone();
        fresh_block(&mut copy.body, visit);
        copy.params.iter_mut().filter_map(|param| param.default.as_mut()).for_each(|value| fresh_expr(Arc::make_mut(value), visit));
        copy
    }
}

impl Enum {
    /// A copy of this enum that shares nothing with it, for a copy of a
    /// generic enum. See `Function::fresh`.
    pub fn fresh(&self) -> Enum {
        let mut copy = self.clone();
        let fields = copy.variants.iter_mut().flat_map(|case| &mut case.fields);
        fields.filter_map(|field| field.default.as_mut()).for_each(|value| fresh_expr(Arc::make_mut(value), &mut |_| {}));
        copy
    }
}

impl Class {
    /// A copy of this class that shares nothing with it. See `Function::fresh`.
    pub fn fresh(&self) -> Class {
        self.fresh_with(&mut |_| {})
    }

    fn fresh_with(&self, visit: &mut dyn FnMut(Node)) -> Class {
        let mut copy = self.clone();
        copy.fields.iter_mut().filter_map(|field| field.default.as_mut()).for_each(|value| fresh_expr(Arc::make_mut(value), visit));
        for method in &mut copy.methods {
            *method = Arc::new(method.fresh_with(visit));
        }
        copy
    }
}

/// What `fresh_block` shows its visitor on the way: each block before
/// its statements, each statement, and each expression before what is
/// inside it. Each may be changed.
pub enum Node<'a> {
    Block(&'a mut Block),
    Stmt(&'a mut Stmt),
    Expr(&'a mut Expr),
}

/// The statements of a block, with the body of a `Scoped` at its end
/// read as more statements of the same block, as many deep as there
/// are. A `Scoped` opens no scope (decision 59.1), so whatever lists
/// what a block declares lists these.
pub fn flat(statements: &[Stmt]) -> Vec<&Stmt> {
    let mut out = Vec::with_capacity(statements.len());
    let mut statements = statements;
    loop {
        match statements.split_last() {
            Some((Stmt { kind: StmtKind::Scoped { body, .. }, .. }, before)) => {
                out.extend(before);
                statements = body;
            }
            _ => {
                out.extend(statements);
                return out;
            }
        }
    }
}

/// Show `visit` each block, statement and expression of these
/// statements, those of the functions written inside them included. The
/// statements are given functions of their own on the way (`fresh_block`),
/// so this is for statements that are a copy, or that are about to be
/// changed anyway.
pub fn visit(block: &mut Block, visit: &mut dyn FnMut(Node)) {
    fresh_block(block, visit)
}

/// Give every function written inside these statements a copy of its own,
/// and show `visit` each block, statement and expression on the way.
fn fresh_block(block: &mut Block, visit: &mut dyn FnMut(Node)) {
    visit(Node::Block(block));
    for statement in block {
        visit(Node::Stmt(statement));
        match &mut statement.kind {
            StmtKind::Let { value, .. } | StmtKind::Const { value, .. } => fresh_expr(value, visit),
            StmtKind::Assign { target, value, .. } => {
                fresh_expr(target, visit);
                fresh_expr(value, visit);
            }
            StmtKind::Receive { places, value, .. } => {
                for place in places {
                    if let Place::Target(target) = place {
                        fresh_expr(target, visit);
                    }
                }
                fresh_expr(value, visit);
            }
            StmtKind::Expr(expr) | StmtKind::Raise(expr) => fresh_expr(expr, visit),
            StmtKind::Return(value) => value.iter_mut().for_each(|value| fresh_expr(value, visit)),
            StmtKind::Function(function) => *function = Arc::new(function.fresh_with(visit)),
            StmtKind::Class(class) => *class = class.fresh_with(visit),
            StmtKind::Enum(decl) => {
                let fields = decl.variants.iter_mut().flat_map(|case| &mut case.fields);
                fields.filter_map(|field| field.default.as_mut()).for_each(|value| fresh_expr(Arc::make_mut(value), visit));
            }
            StmtKind::If { condition, then_block, else_block } => {
                fresh_expr(condition, visit);
                fresh_block(then_block, visit);
                else_block.iter_mut().for_each(|block| fresh_block(block, visit));
            }
            StmtKind::While { condition, body } => {
                fresh_expr(condition, visit);
                fresh_block(body, visit);
            }
            StmtKind::For { iterable, body, .. } => {
                fresh_expr(iterable, visit);
                fresh_block(body, visit);
            }
            StmtKind::Match { subject, arms } => {
                fresh_expr(subject, visit);
                arms.iter_mut().for_each(|arm| fresh_block(&mut arm.body, visit));
            }
            StmtKind::Try { body, handlers } => {
                fresh_block(body, visit);
                handlers.iter_mut().for_each(|handler| fresh_block(&mut handler.body, visit));
            }
            StmtKind::Unsafe(body) | StmtKind::Scoped { body, .. } => fresh_block(body, visit),
            StmtKind::Test { body, .. } | StmtKind::AssertRaises { body, .. } => fresh_block(body, visit),
            StmtKind::Assert(condition, message) => {
                fresh_expr(condition, visit);
                message.iter_mut().for_each(|message| fresh_expr(message, visit));
            }
            StmtKind::Break | StmtKind::Continue | StmtKind::Import { .. } => {}
        }
    }
}

fn fresh_expr(expr: &mut Expr, visit: &mut dyn FnMut(Node)) {
    visit(Node::Expr(expr));
    match &mut expr.kind {
        ExprKind::Function(function) => *function = Arc::new(function.fresh_with(visit)),
        ExprKind::FormatStr(parts) => {
            for part in parts {
                if let FormatPart::Value { value, .. } = part {
                    fresh_expr(value, visit);
                }
            }
        }
        ExprKind::Unit { value, .. } => fresh_expr(value, visit),
        ExprKind::List(items) | ExprKind::Several(items) => items.iter_mut().for_each(|value| fresh_expr(value, visit)),
        ExprKind::Dict(entries) => {
            for (key, value) in entries {
                fresh_expr(key, visit);
                fresh_expr(value, visit);
            }
        }
        ExprKind::ListComp { element, iterable, condition, .. } => {
            fresh_expr(element, visit);
            fresh_expr(iterable, visit);
            condition.iter_mut().for_each(|condition| fresh_expr(condition, visit));
        }
        ExprKind::Unary { operand, .. } => fresh_expr(operand, visit),
        ExprKind::Binary { left, right, .. } => {
            fresh_expr(left, visit);
            fresh_expr(right, visit);
        }
        ExprKind::Compare { first, rest } => {
            fresh_expr(first, visit);
            rest.iter_mut().for_each(|(_, operand)| fresh_expr(operand, visit));
        }
        ExprKind::Call { callee, args } => {
            fresh_expr(callee, visit);
            args.iter_mut().for_each(|arg| fresh_expr(&mut arg.value, visit));
        }
        ExprKind::Field { object, .. } => fresh_expr(object, visit),
        ExprKind::Index { object, index } => {
            fresh_expr(object, visit);
            fresh_expr(index, visit);
        }
        ExprKind::Take(place) => fresh_expr(place, visit),
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_) | ExprKind::Inline { .. }
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Name(_)
        | ExprKind::Give(_) => {}
    }
}

/// A named slot: a class field, or one value carried by an enum case.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Option<Type>,
    /// Shared, so that a compiler that copies the parameter list keeps
    /// the node the checker saw, and what the checker noted about it.
    pub default: Option<Arc<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    pub name: String,
    /// `enum Maybe[T]` has the one type parameter `T` (decision 58.1).
    /// Like a generic class, it is not an enum yet: each set of types it
    /// is used with gets a copy that is one.
    pub type_params: Vec<TypeParam>,
    pub variants: Vec<Variant>,
}

/// One case of an enum: `Idle`, or `Circle(radius: float)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Field>,
    /// `Ack = 6`: the number written for the case, and where it is
    /// (decision 35.1). An enum has one for every case, or for none.
    pub number: Option<(i128, Span)>,
}

/// One `case pattern { body }` inside a match.
#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Block,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_` matches anything.
    Wildcard,
    /// A bare name. Whether it is an enum case (`Idle`) or a new variable
    /// that captures the value is decided later, when names are resolved.
    Name(String),
    /// `0`, `"yes"`, `true`, `none`
    Literal(Expr),
    /// `Circle(r)`, `Rect(w, h)`
    Variant { name: String, fields: Vec<Pattern> },
}

/// One `except Error as name { body }` after a try.
#[derive(Debug, Clone, PartialEq)]
pub struct Handler {
    /// `None` catches every error.
    pub error: Option<String>,
    pub binding: Option<String>,
    pub body: Block,
}

// --------------------------------------------------------------------- Types

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// `int`, `Point`, or with arguments: `list[int]`
    Named { name: String, args: Vec<Type> },
    /// `str or none`
    Optional(Box<Type>),
    /// The type of a function: `function(int, int) -> int`, and the
    /// kind of error a call of it may raise: `function(str) -> int
    /// raises ValueError` (decision 56.1).
    /// The kind is boxed so that a `Type` is no bigger than it was before
    /// it had one: every statement that holds a type would grow, and the
    /// interpreter, which matches on statements, would be slower.
    Function { params: Vec<Type>, returns: Option<Box<Type>>, raises: Option<Box<String>> },
    /// `owned list[int]`: a value with exactly one owner.
    Owned(Box<Type>),
    /// `take Packet`, on a parameter: the function takes the value over,
    /// and the caller's variable holds nothing afterwards. A parameter
    /// without the word only looks.
    Take(Box<Type>),
    /// A number written among the types in `[ ]`: the 64 of
    /// `list[u8, 64]`, a list of a fixed length (decision 36.4).
    Length(u64),
    /// `(int, str)` after `->`: a function that gives two values or more
    /// (decision 57). Nowhere else is it a type.
    Several(Vec<Type>),
}

// --------------------------------------------------------------- Expressions

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(u64),
    Float(f64),
    Str(String),
    /// `f"Hello, {who}!"`, split into its text and `{...}` parts.
    FormatStr(Vec<FormatPart>),
    Bool(bool),
    None,
    /// A variable, function or type, used by name.
    Name(String),
    /// A resource read from the variable `name` and given away: the
    /// variable holds nothing after it, so the `Scoped` it ends does not
    /// release it (decision 59.1). Only the checker makes it.
    Give(String),
    /// A number with a unit: `500 ms`
    Unit { value: Box<Expr>, unit: String },
    /// `[1, 2, 3]`
    List(Vec<Expr>),
    /// `["a": 1, "b": 2]`, and `[:]` for one with nothing in it.
    Dict(Vec<(Expr, Expr)>),
    /// `[element for variable in iterable if condition]`
    ListComp {
        element: Box<Expr>,
        variable: String,
        iterable: Box<Expr>,
        condition: Option<Box<Expr>>,
    },
    /// `-x`, `not ready`
    Unary { op: UnaryOp, operand: Box<Expr> },
    /// `a + b`, `x == y`, `p and q`
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    /// A chain of two or more comparisons: `0 <= i < len`.
    /// It means `0 <= i and i < len`, with `i` worked out only once.
    Compare { first: Box<Expr>, rest: Vec<(BinaryOp, Expr)> },
    /// A function with no name, used as a value: `function(n) { return n * 2 }`
    Function(Arc<Function>),
    /// `callee(args)`
    Call { callee: Box<Expr>, args: Vec<Arg> },
    /// `object.name`. `at` is where the name is written, which is what
    /// an editor asks about when the mouse rests on a field.
    Field { object: Box<Expr>, name: String, at: Span },
    /// `object[index]`
    Index { object: Box<Expr>, index: Box<Expr> },
    /// `take slot.task`, `take tasks[i]`: what the place holds, which is
    /// left holding `none` (decision 56.10).
    Take(Box<Expr>),
    /// `a // b, a % b`: several values written out, after `return` in a
    /// function that gives that many, or on the right of a `Receive`
    /// (decision 57). They are never a value of their own.
    Several(Vec<Expr>),
    /// `inline c { ... }`: code of another language, as it was written,
    /// which gives a library of C or a module of Python or JavaScript
    /// (decision 61). Only a `let` has one as its value.
    Inline { language: String, text: String },
}

/// One piece of an f-string.
#[derive(Debug, Clone, PartialEq)]
pub enum FormatPart {
    Text(String),
    /// `{value}` or `{value:format}`, e.g. `{area:.2f}`
    Value { value: Expr, format: Option<String> },
}

/// One argument in a call: `3.0`, or named: `x=3.0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
    /// `~x`: every bit of a whole number turned over.
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    /// `&`, `|`, `^`, `<<`, `>>`: the bits of whole numbers.
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Is,
    IsNot,
    In,
    NotIn,
}

impl BinaryOp {
    /// The operator as it is written in source.
    pub fn text(self) -> &'static str {
        use BinaryOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            FloorDiv => "//",
            Mod => "%",
            Pow => "**",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Shl => "<<",
            Shr => ">>",
            Eq => "==",
            NotEq => "!=",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            And => "and",
            Or => "or",
            Is => "is",
            IsNot => "is not",
            In => "in",
            NotIn => "not in",
        }
    }
}

// ------------------------------------------------- Loops that need no copy

/// Can `for item in iterable { body }` walk the list where it is, with no
/// copy made for the loop?
///
/// A loop walks the list as it was when the loop began (decision 5.5c),
/// so the compilers give it a copy of its own: the body may add to the
/// list, take from it, or give its variable another list. The copy costs
/// a block of memory and a pass over the list on every loop, which in a
/// short loop run often (the planets of bench/nbody.zeno, five of them,
/// moved ten million times) is a large part of the time.
///
/// When the body cannot change the list, the copy and the list are the
/// same, and the copy can go. This answers yes only when that is plain
/// from the words of the body, whatever the types:
///
/// - the name the list is reached from (`bodies` in `bodies` or in
///   `bodies.inner`, `self` in `self.tasks`) is not written anywhere in
///   the body, so the body neither changes it nor hands it to anything;
/// - nothing is called but the built-in functions that only look at
///   their arguments (`print`, `len`, `str`, `math.sqrt`, ...), which
///   `built_in` says, so no function can reach the list another way;
/// - no item of any list is replaced (`xs[i] = v`), which with no
///   collector frees the item that was there, and there is no function
///   literal, which could be kept and called later.
///
/// Changing a field of an item (`b.x += dt * b.vx`) is allowed: the list
/// is the same list, holding the same items, after it.
pub fn walks_in_place(iterable: &Expr, body: &[Stmt], built_in: &dyn Fn(&str) -> bool) -> bool {
    let mut root = iterable;
    loop {
        match &root.kind {
            ExprKind::Field { object, .. } => root = object,
            ExprKind::Name(name) => return body.iter().all(|stmt| stmt_leaves(stmt, name, built_in)),
            _ => return false,
        }
    }
}

/// Whether any statement of `body` names the variable `name`: reads it,
/// changes it, gives it away or captures it. A loop over text uses this
/// to know that the text stays where it is while the loop walks it.
///
/// It reads the printed form of the tree, where a use of a variable shows
/// as `Name("text")`, so it misses nothing a new kind of expression adds.
/// A field called `text` prints differently and does not count.
pub fn mentions(body: &[Stmt], name: &str) -> bool {
    format!("{body:?}").contains(&format!("Name({name:?})"))
}

/// The fields a function may read once, at its start, and keep in a
/// local instead of reading them from the object every time: decision
/// 22.8. Returns the names of the parameters whose fields qualify, and
/// the names of the fields that are written somewhere in the body (which
/// do not), or `None` when nothing qualifies.
///
/// A field `p.f` qualifies when `p` is a parameter that no `let`, `const`
/// or `for` in the body takes the name of, no statement anywhere in the
/// body writes a field called `f`, and the body calls nothing but the
/// built-ins that only look (`print`, `len`, `math.sqrt`, ...): any other
/// function could change the field. Then the field holds the same value
/// from the first line to the last, and C (or LLVM) need not read it
/// again after each write to another field, in case the two objects are
/// the same. (The caller checks that `p` holds an object and `f` a
/// number.)
pub fn fields_read_once<'a>(
    params: impl Iterator<Item = &'a str>,
    body: &[Stmt],
    built_in: &dyn Fn(&str) -> bool,
) -> Option<(Vec<String>, std::collections::HashSet<String>)> {
    // `stmt_leaves` with a name no variable has: only the calls and the
    // kinds of statement it allows.
    if !body.iter().all(|stmt| stmt_leaves(stmt, "", built_in)) {
        return None;
    }
    let mut bound = std::collections::HashSet::new();
    let mut written = std::collections::HashSet::new();
    fn walk(block: &[Stmt], bound: &mut std::collections::HashSet<String>, written: &mut std::collections::HashSet<String>) {
        for stmt in block {
            match &stmt.kind {
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => {
                    bound.insert(name.clone());
                }
                StmtKind::Assign { target, .. } => match &target.kind {
                    ExprKind::Field { name, .. } => {
                        written.insert(name.clone());
                    }
                    ExprKind::Name(name) => {
                        bound.insert(name.clone());
                    }
                    _ => {}
                },
                StmtKind::If { then_block, else_block, .. } => {
                    walk(then_block, bound, written);
                    if let Some(block) = else_block {
                        walk(block, bound, written);
                    }
                }
                StmtKind::While { body, .. } => walk(body, bound, written),
                StmtKind::For { variable, second, body, .. } => {
                    bound.insert(variable.clone());
                    bound.extend(second.clone());
                    walk(body, bound, written);
                }
                _ => {}
            }
        }
    }
    walk(body, &mut bound, &mut written);
    let params: Vec<String> = params.filter(|name| !bound.contains(*name)).map(str::to_string).collect();
    (!params.is_empty()).then_some((params, written))
}

/// The built-in functions that only look at what they are given.
const LOOKING: &[&str] = &["print", "len", "str", "int", "float", "bool", "abs", "min", "max", "round", "chr", "ord"];

fn stmt_leaves(stmt: &Stmt, root: &str, built_in: &dyn Fn(&str) -> bool) -> bool {
    let expr = |e: &Expr| expr_leaves(e, root, built_in);
    let block = |b: &Block| b.iter().all(|s| stmt_leaves(s, root, built_in));
    match &stmt.kind {
        StmtKind::Let { name, value, .. } | StmtKind::Const { name, value, .. } => name != root && expr(value),
        StmtKind::Assign { target, value, .. } => {
            let target_ok = match &target.kind {
                ExprKind::Name(name) => name != root,
                ExprKind::Field { object, .. } => expr(object),
                _ => false,
            };
            target_ok && expr(value)
        }
        StmtKind::Expr(e) | StmtKind::Raise(e) => expr(e),
        StmtKind::Assert(e, message) => expr(e) && message.as_ref().is_none_or(expr),
        StmtKind::Return(value) => value.as_ref().is_none_or(expr),
        StmtKind::If { condition, then_block, else_block } => {
            expr(condition) && block(then_block) && else_block.as_ref().is_none_or(block)
        }
        StmtKind::While { condition, body } => expr(condition) && block(body),
        StmtKind::For { variable, second, iterable, body } => {
            variable != root && second.as_deref() != Some(root) && expr(iterable) && block(body)
        }
        StmtKind::Break | StmtKind::Continue => true,
        _ => false,
    }
}

fn expr_leaves(e: &Expr, root: &str, built_in: &dyn Fn(&str) -> bool) -> bool {
    let go = |e: &Expr| expr_leaves(e, root, built_in);
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::None => true,
        ExprKind::Name(name) => name != root,
        ExprKind::Unit { value, .. } => go(value),
        ExprKind::FormatStr(parts) => parts.iter().all(|part| match part {
            FormatPart::Text(_) => true,
            FormatPart::Value { value, .. } => go(value),
        }),
        ExprKind::Unary { operand, .. } => go(operand),
        ExprKind::Binary { left, right, .. } => go(left) && go(right),
        ExprKind::Compare { first, rest } => go(first) && rest.iter().all(|(_, e)| go(e)),
        ExprKind::Field { object, .. } => go(object),
        ExprKind::Index { object, index } => go(object) && go(index),
        ExprKind::Call { callee, args } => {
            let looks = match &callee.kind {
                ExprKind::Name(name) => LOOKING.contains(&name.as_str()) && built_in(name),
                // `math.sqrt(x)`: a function of the math module.
                ExprKind::Field { object, .. } => matches!(&object.kind, ExprKind::Name(module) if module == "math" && built_in(module)),
                _ => false,
            };
            looks && args.iter().all(|arg| go(&arg.value))
        }
        _ => false,
    }
}
