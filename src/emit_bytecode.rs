//! The bytecode compiler: turns the syntax tree into bytecode.
//!
//! It walks the tree the way the interpreter does, statement by statement
//! and expression by expression. Where the interpreter would *do* a thing,
//! this writes down the instructions that do it (see bytecode.rs for what
//! each one means). `1 + 2` is "work out the left side, work out the
//! right side, add", so it compiles to the code for `1`, the code for `2`,
//! and `Binary +`.
//!
//! Three things are settled here, once, that the interpreter settles
//! again every time a line runs:
//!
//! - **Where each variable lives.** A name becomes a slot number on the
//!   stack, or a cell if a function written inside also uses it (see
//!   `declare` and `resolve`).
//! - **Where control goes.** `if`, `while`, `for`, `match`, `break` and
//!   `try` all become jumps to instruction numbers.
//! - **How tall the stack is** at every instruction. The compiler counts
//!   as it writes (`height`), which is how it knows the slot a new
//!   variable will land in.
//!
//! It never refuses a program. A mistake it can see, such as a name that
//! does not exist, becomes a `Fatal` instruction at the place where the
//! interpreter would have found it, so that both stop at the same moment
//! with the same message. Finding mistakes before the program runs is the
//! checker's job.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::*;
use crate::builtins;
use crate::bytecode::{Capture, ClassCode, From, Op, Proto};
use crate::token::Span;
use crate::units;
use crate::value::Value;

/// Compile a whole program. The result is the function `main`, with every
/// function written in the program inside it.
pub fn compile(program: &Program) -> Rc<Proto> {
    let mut compiler = Compiler { functions: Vec::new(), builtins: builtins::all().into_iter().collect() };
    let start = program.statements.first().map_or(Span { line: 1, col: 1 }, |first| first.span);
    let end = program.statements.last().map_or(start, |last| last.span);

    let shared = shared_names(&[], &program.statements);
    compiler.functions.push(Function::new(Kind::Main, "main", None, Vec::new(), shared));
    compiler.open();
    compiler.statements(&program.statements);
    compiler.constant(Value::None, end);
    compiler.emit(Op::Return, end);
    Rc::new(compiler.finish())
}

struct Compiler {
    /// The functions being compiled: `main` first, then the function the
    /// compiler is inside, then the one inside that. The last is the one
    /// instructions are being written to.
    functions: Vec<Function>,
    /// `print`, `len`, `ValueError` and the rest. A use of one compiles to
    /// a constant.
    builtins: HashMap<&'static str, Value>,
}

#[derive(PartialEq)]
enum Kind {
    /// The program itself.
    Main,
    Function,
    /// The code that builds an object of a class.
    Constructor,
}

/// A function while it is being compiled: the `Proto` it will become,
/// and what the compiler needs to remember on the way.
struct Function {
    kind: Kind,
    name: String,
    decl: Option<Arc<crate::ast::Function>>,
    params: Vec<String>,
    code: Vec<Op>,
    spans: Vec<Span>,
    notes: Vec<String>,
    constants: Vec<Value>,
    names: Vec<Vec<String>>,
    /// `None` for a function that has a number but no code yet: one that
    /// is written further down the block.
    functions: Vec<Option<Rc<Proto>>>,
    classes: Vec<ClassCode>,
    enums: Vec<Enum>,
    cells: Vec<String>,
    captures: Vec<Capture>,

    /// The `{ }` blocks the compiler is inside, outermost first.
    blocks: Vec<Block>,
    /// How many values this function has on the stack at this point.
    height: u32,
    /// The loops the compiler is inside, for `break` and `continue`.
    loops: Vec<Loop>,
    /// How many `try` bodies the compiler is inside.
    tries: usize,
    /// The resources whose `Scoped` the compiler is inside, outermost
    /// first: each way out of one releases it (decision 59.1).
    scopeds: Vec<Scoped>,
    /// The names that functions written inside this one use. A variable
    /// with one of these names goes in a cell.
    shared: HashSet<String>,
    /// The number given to each function and class of the blocks being
    /// compiled, by where it is in the syntax tree.
    numbered: HashMap<usize, u32>,
}

impl Function {
    fn new(
        kind: Kind,
        name: &str,
        decl: Option<Arc<crate::ast::Function>>,
        params: Vec<String>,
        shared: HashSet<String>,
    ) -> Function {
        Function {
            kind,
            name: name.to_string(),
            decl,
            // The parameters are on the stack when the function starts.
            height: params.len() as u32,
            params,
            code: Vec::new(),
            spans: Vec::new(),
            notes: Vec::new(),
            constants: Vec::new(),
            names: Vec::new(),
            functions: Vec::new(),
            classes: Vec::new(),
            enums: Vec::new(),
            cells: Vec::new(),
            captures: Vec::new(),
            blocks: Vec::new(),
            loops: Vec::new(),
            tries: 0,
            scopeds: Vec::new(),
            shared,
            numbered: HashMap::new(),
        }
    }
}

/// One `{ }` block: the variables declared in it so far.
struct Block {
    variables: Vec<Variable>,
    /// How tall the stack was when the block began. Everything above that
    /// is dropped when it ends.
    height: u32,
    /// Cells made at the start of the block for variables that are
    /// declared further down it (see `prepare`).
    waiting: HashMap<String, u32>,
}

struct Variable {
    name: String,
    place: Place,
    /// True for `var`.
    mutable: bool,
    /// True for an enum case with no values, like `Idle`. In a `match`,
    /// `case Idle` compares with it, where any other name is a new
    /// variable.
    case: bool,
}

/// Where a variable is kept while the program runs.
#[derive(Clone, Copy)]
enum Place {
    /// A slot of this function's part of the stack.
    Slot(u32),
    /// A cell of this function.
    Cell(u32),
    /// A cell of a function around this one, which this one captured.
    Captured(u32),
}

/// The rest of a block after the `let` of a resource (decision 59.1).
struct Scoped {
    /// The variable that holds the resource.
    name: String,
    /// Where it is, found at its `let`: an inner block may declare the
    /// same name, and a `return` from there releases this one.
    place: Option<Place>,
    /// How many `try` bodies the compiler was inside before this one's
    /// own, which catches an error passing through.
    tries: usize,
    /// How many loops it was inside: a `break` or `continue` of a loop
    /// begun before it leaves it.
    loops: usize,
}

struct Loop {
    /// How tall the stack is at the top of the loop.
    height: u32,
    /// How many `try` bodies the loop itself is inside.
    tries: usize,
    /// Where `continue` jumps to.
    start: usize,
    /// The jumps written for each `break`, to be pointed at the end of
    /// the loop when the compiler gets there.
    breaks: Vec<usize>,
}

/// Where a pattern finds the value it is tested against.
#[derive(Clone, Copy)]
enum Source {
    /// In this slot.
    Slot(u32),
    /// The nth value carried by the enum value in this slot.
    Field(u32, u32),
}

impl Compiler {
    // ------------------------------------------------------- Writing code

    /// The function being written.
    fn f(&mut self) -> &mut Function {
        self.functions.last_mut().expect("a function is being compiled")
    }

    /// Write one instruction, and keep count of what it does to the stack.
    /// Returns where it was written.
    fn emit(&mut self, op: Op, span: Span) -> usize {
        let change: i64 = match op {
            Op::Const(_) | Op::GetLocal(_) | Op::GetCell(_) | Op::GetCaptured(_) => 1,
            Op::Closure(_) | Op::Class(_) | Op::Enum(_) | Op::ForNext(_) => 1,
            Op::ForStartPairs | Op::ForNextPair(_) | Op::ForSecond(_) => 1,
            Op::Pop(n) => -(n as i64),
            Op::SetLocal(_) | Op::DefineCell(_) | Op::SetCell(_) | Op::SetCaptured(_) => -1,
            Op::Binary(_) | Op::And(_) | Op::Or(_) | Op::Chain(..) => -1,
            Op::Append(_) | Op::GetIndex | Op::TakeIndex | Op::JumpIfFalse(_) => -1,
            Op::Return | Op::Raise | Op::Reraise | Op::Release => -1,
            Op::List(n) | Op::Text(n) | Op::Object(n) | Op::Several(n) => 1 - n as i64,
            Op::Unpack(n) => n as i64 - 1,
            Op::Dict(n) => 1 - 2 * n as i64,
            Op::Call(n) | Op::CallNamed(n, _) => -(n as i64),
            Op::SetField(_) => -2,
            Op::SetIndex => -3,
            // `PopTo` is written where the code after it cannot be reached
            // in the usual way, so its caller says what the height is.
            _ => 0,
        };
        let function = self.f();
        function.height = (function.height as i64 + change) as u32;
        function.code.push(op);
        function.spans.push(span);
        function.notes.push(String::new());
        function.code.len() - 1
    }

    /// The same, with a word for `zeno bytecode` to show beside it.
    fn emit_noted(&mut self, op: Op, span: Span, note: &str) -> usize {
        let at = self.emit(op, span);
        self.f().notes[at] = note.to_string();
        at
    }

    /// Point the jump written at `at` to the next instruction to be written.
    fn patch(&mut self, at: usize) {
        let function = self.f();
        let here = function.code.len() as u32;
        function.code[at] = match function.code[at] {
            Op::Jump(_) => Op::Jump(here),
            Op::JumpIfFalse(_) => Op::JumpIfFalse(here),
            Op::And(_) => Op::And(here),
            Op::Or(_) => Op::Or(here),
            Op::Chain(op, _) => Op::Chain(op, here),
            Op::ForNext(_) => Op::ForNext(here),
            Op::ForNextPair(_) => Op::ForNextPair(here),
            Op::Default(slot, _) => Op::Default(slot, here),
            Op::Try(_) => Op::Try(here),
            Op::Except(name, _) => Op::Except(name, here),
            other => unreachable!("{other:?} is not a jump"),
        };
    }

    /// The number of a constant, adding it if the function does not have
    /// it yet.
    fn add_constant(&mut self, value: Value) -> u32 {
        let constants = &mut self.f().constants;
        let same = |other: &Value| match (&value, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::None, Value::None) => true,
            (Value::Builtin(a), Value::Builtin(b)) => a.name == b.name && a.call as usize == b.call as usize,
            _ => false,
        };
        match constants.iter().position(same) {
            Some(index) => index as u32,
            None => {
                constants.push(value);
                constants.len() as u32 - 1
            }
        }
    }

    /// The number of a piece of text among the constants.
    fn text(&mut self, text: &str) -> u32 {
        self.add_constant(Value::str(text))
    }

    /// Push a constant.
    fn constant(&mut self, value: Value, span: Span) {
        let index = self.add_constant(value);
        self.emit(Op::Const(index), span);
    }

    /// A mistake the compiler can see. The program stops here if it gets
    /// this far.
    fn mistake(&mut self, message: impl Into<String>, span: Span) {
        let index = self.text(&message.into());
        self.emit(Op::Fatal(index), span);
    }

    // ------------------------------------------------------------ Variables

    fn open(&mut self) {
        let height = self.f().height;
        self.f().blocks.push(Block { variables: Vec::new(), height, waiting: HashMap::new() });
    }

    /// End a block: its variables are dropped from the stack.
    fn close(&mut self, span: Span) {
        let block = self.f().blocks.pop().expect("a block is open");
        let extra = self.f().height - block.height;
        if extra > 0 {
            self.emit(Op::Pop(extra), span);
        }
    }

    fn block(&mut self) -> &mut Block {
        self.f().blocks.last_mut().expect("a block is open")
    }

    fn new_cell(&mut self, name: &str) -> u32 {
        self.f().cells.push(name.to_string());
        self.f().cells.len() as u32 - 1
    }

    /// The value on top of the stack becomes a new variable of the block
    /// being compiled.
    ///
    /// Usually that costs nothing: the value stays where it is, and the
    /// place it is in becomes the variable's slot. A variable that a
    /// function written inside uses is moved into a cell instead.
    ///
    /// Returns false if the block has the name already. The program stops
    /// there, as it does in the interpreter.
    fn declare(&mut self, name: &str, mutable: bool, case: bool, span: Span) -> bool {
        if self.block().variables.iter().any(|variable| variable.name == name) {
            self.mistake(format!("`{name}` is already declared in this block"), span);
            return false;
        }
        let place = if self.f().shared.contains(name) {
            let cell = match self.block().waiting.remove(name) {
                Some(cell) => cell,
                None => {
                    let cell = self.new_cell(name);
                    self.emit(Op::NewCell(cell), span);
                    cell
                }
            };
            self.emit(Op::DefineCell(cell), span);
            Place::Cell(cell)
        } else {
            Place::Slot(self.f().height - 1)
        };
        self.block().variables.push(Variable { name: name.to_string(), place, mutable, case });
        true
    }

    /// `declare`, for the value in `slot`, which need not be on top: one
    /// of several values received at once (decision 57). One that a
    /// function written inside uses is copied into its cell.
    fn declare_at(&mut self, name: &str, mutable: bool, slot: u32, span: Span) {
        if self.block().variables.iter().any(|variable| variable.name == name) {
            self.mistake(format!("`{name}` is already declared in this block"), span);
            return;
        }
        let place = if self.f().shared.contains(name) {
            self.emit(Op::GetLocal(slot), span);
            let cell = match self.block().waiting.remove(name) {
                Some(cell) => cell,
                None => {
                    let cell = self.new_cell(name);
                    self.emit(Op::NewCell(cell), span);
                    cell
                }
            };
            self.emit(Op::DefineCell(cell), span);
            Place::Cell(cell)
        } else {
            Place::Slot(slot)
        };
        self.block().variables.push(Variable { name: name.to_string(), place, mutable, case: false });
    }

    /// Find the variable a name means at this point of the program: the
    /// nearest one declared above, in this function or one around it.
    fn resolve(&mut self, name: &str) -> Option<(Place, bool, bool)> {
        self.resolve_in(self.functions.len() - 1, name)
    }

    fn resolve_in(&mut self, level: usize, name: &str) -> Option<(Place, bool, bool)> {
        let own = self.functions[level]
            .blocks
            .iter()
            .rev()
            .flat_map(|block| block.variables.iter().rev())
            .find(|variable| variable.name == name);
        if let Some(variable) = own {
            return Some((variable.place, variable.mutable, variable.case));
        }
        if level == 0 {
            return None;
        }

        // A variable of a function around this one. This function takes
        // the variable's cell with it when it is made (`Op::Closure`), and
        // the function around it may have to do the same in its turn.
        let (place, mutable, case) = self.resolve_in(level - 1, name)?;
        let from = match place {
            Place::Cell(cell) => From::Cell(cell),
            Place::Captured(index) => From::Captured(index),
            Place::Slot(_) => unreachable!("`{name}` is used by an inner function but is not in a cell"),
        };
        let captures = &mut self.functions[level].captures;
        let index = match captures.iter().position(|capture| capture.from == from) {
            Some(index) => index,
            None => {
                captures.push(Capture { name: name.to_string(), from });
                captures.len() - 1
            }
        };
        Some((Place::Captured(index as u32), mutable, case))
    }

    /// Push the value of a name.
    fn load(&mut self, name: &str, span: Span) {
        match self.resolve(name) {
            Some((Place::Slot(slot), ..)) => self.emit_noted(Op::GetLocal(slot), span, name),
            Some((Place::Cell(cell), ..)) => self.emit(Op::GetCell(cell), span),
            Some((Place::Captured(index), ..)) => self.emit(Op::GetCaptured(index), span),
            None => match self.builtins.get(name) {
                Some(builtin) => {
                    let builtin = builtin.clone();
                    return self.constant(builtin, span);
                }
                None => {
                    self.mistake(format!("`{name}` is not defined"), span);
                    return self.constant(Value::None, span);
                }
            },
        };
    }

    // ----------------------------------------------------------- Statements

    /// Compile the statements of a block that is already open.
    fn statements(&mut self, statements: &[Stmt]) {
        self.prepare(statements);
        self.hoist(statements);
        for statement in statements {
            self.statement(statement);
        }
    }

    /// Compile a `{ }` block in a scope of its own.
    fn scoped(&mut self, statements: &[Stmt], span: Span) {
        self.open();
        self.statements(statements);
        self.close(span);
    }

    /// Make the cells of the block's shared variables before anything
    /// else in it runs.
    ///
    /// A function is made at the top of its block, so that it can be
    /// called from above where it is written (`hoist`). When it is made
    /// it takes the cells of the variables it uses. A variable written
    /// between the top of the block and the function has no value yet at
    /// that moment, but its cell has to exist, so that the function and
    /// the block are looking at the same one.
    fn prepare(&mut self, statements: &[Stmt]) {
        // What follows the `let` of a resource is more of this block
        // (decision 59.1).
        for statement in crate::ast::flat(statements) {
            let names: Vec<&str> = match &statement.kind {
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => vec![name],
                StmtKind::Receive { places, .. } => {
                    places.iter().filter_map(crate::ast::Place::declared).map(String::as_str).collect()
                }
                StmtKind::Function(decl) => vec![&decl.name],
                StmtKind::Class(class) => vec![&class.name],
                StmtKind::Enum(decl) => std::iter::once(&decl.name)
                    .chain(decl.variants.iter().map(|case| &case.name))
                    .map(String::as_str)
                    .collect(),
                StmtKind::Import { module, names } => match builtins::module(&module.join(".")) {
                    Some(found) if names.is_empty() => vec![found.name],
                    Some(_) => names.iter().map(String::as_str).collect(),
                    None => Vec::new(),
                },
                _ => Vec::new(),
            };
            for name in names {
                if self.f().shared.contains(name) && !self.block().waiting.contains_key(name) {
                    let cell = self.new_cell(name);
                    self.emit(Op::NewCell(cell), statement.span);
                    self.block().waiting.insert(name.to_string(), cell);
                }
            }
        }
    }

    /// Declare the block's functions, classes and enums before the rest
    /// of it, so they can be used above the line where they are written.
    ///
    /// Only the *values* are made here. A function's body is compiled when
    /// the compiler reaches the line it is written on (`statement`), so
    /// that it sees the variables declared above it and no others. Until
    /// then the function has a number and no code.
    fn hoist(&mut self, statements: &[Stmt]) {
        for statement in crate::ast::flat(statements) {
            let span = statement.span;
            match &statement.kind {
                StmtKind::Function(decl) => {
                    let index = self.number_function();
                    self.f().numbered.insert(Arc::as_ptr(decl) as usize, index);
                    self.emit(Op::Closure(index), span);
                    self.declare(&decl.name, false, false, span);
                }
                StmtKind::Class(class) => {
                    let constructor = self.number_function();
                    let methods = class.methods.iter().map(|_| self.number_function()).collect();
                    self.f().classes.push(ClassCode {
                        name: class.name.clone(),
                        fields: class.fields.clone(),
                        constructor,
                        methods,
                    });
                    let index = self.f().classes.len() as u32 - 1;
                    self.f().numbered.insert(class as *const Class as usize, index);
                    self.emit(Op::Class(index), span);
                    self.declare(&class.name, false, false, span);
                }
                StmtKind::Enum(decl) => {
                    self.f().enums.push(decl.clone());
                    let index = self.f().enums.len() as u32 - 1;
                    self.emit(Op::Enum(index), span);
                    if !self.declare(&decl.name, false, false, span) {
                        continue;
                    }
                    // Each case is also a name of its own, so you can write
                    // `Circle(1.0)` as well as `Shape.Circle(1.0)`.
                    for (number, case) in decl.variants.iter().enumerate() {
                        self.load(&decl.name, span);
                        self.emit(Op::EnumCase(number as u32), span);
                        if !self.declare(&case.name, false, case.fields.is_empty(), span) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Give a function that is not compiled yet its number.
    fn number_function(&mut self) -> u32 {
        self.f().functions.push(None);
        self.f().functions.len() as u32 - 1
    }

    fn statement(&mut self, statement: &Stmt) {
        let span = statement.span;

        match &statement.kind {
            // The rest of the block after the `let` of a resource. Its body
            // runs in a `try` of its own: an error passing through releases
            // the resource and goes on as it was. Each other way out is
            // written below, and `return`, `break` and `continue` release
            // it on their way (`release_from`). Its body is more of the
            // same block, which `prepare` and `hoist` have already seen.
            StmtKind::Scoped { name, body } => {
                let height = self.f().height;
                let (tries, loops) = (self.f().tries, self.f().loops.len());
                let place = self.resolve(name).map(|(place, ..)| place);
                self.f().scopeds.push(Scoped { name: name.clone(), place, tries, loops });
                let to_handler = self.emit(Op::Try(0), span);
                self.f().tries += 1;
                for statement in body {
                    self.statement(statement);
                }
                self.f().tries -= 1;
                self.f().scopeds.pop();
                self.emit(Op::EndTry, span);
                self.release(name, span);
                let after = self.f().height;
                let to_end = self.emit(Op::Jump(0), span);

                // An error comes here, with the stack as it was at `Try`.
                self.patch(to_handler);
                self.f().height = height + 1;
                self.release(name, span);
                self.emit(Op::Reraise, span);
                self.patch(to_end);
                self.f().height = after;
            }
            StmtKind::Let { mutable, name, value, .. } => {
                self.expr(value);
                self.declare(name, *mutable, false, span);
            }
            StmtKind::Const { name, value, .. } => {
                self.expr(value);
                self.declare(name, false, false, span);
            }
            // All of the right, then each place left to right (decision 57).
            // The values lie on the stack, the first deepest. A new name
            // takes its value's slot as its own, as a `let` does; a place
            // that exists is given a copy, and the values are dropped after.
            StmtKind::Receive { declare, places, value } => {
                self.expr(value);
                let count = places.len() as u32;
                self.emit(Op::Unpack(count), span);
                let first = self.f().height - count;
                for (at, place) in places.iter().enumerate() {
                    let slot = first + at as u32;
                    match place {
                        crate::ast::Place::Name { name, .. } => {
                            self.declare_at(name, *declare == Some(true), slot, span);
                        }
                        // Left where it is, with no name, until the block ends.
                        crate::ast::Place::Drop => {}
                        crate::ast::Place::Target(target) => {
                            self.emit(Op::GetLocal(slot), span);
                            self.assign(target);
                        }
                    }
                }
                if declare.is_none() {
                    self.emit(Op::Pop(count), span);
                }
            }
            StmtKind::Assign { target, op, value } => {
                match op {
                    None => self.expr(value),
                    // `x += 1` is `x = x + 1`: what is there is read,
                    // and then the value is worked out.
                    Some(op) => {
                        self.expr(target);
                        self.expr(value);
                        self.emit(Op::Binary(*op), span);
                    }
                }
                self.assign(target);
            }
            StmtKind::Expr(expr) => {
                self.expr(expr);
                self.emit(Op::Pop(1), span);
            }

            // Declared by `hoist`. This is the line they are written on,
            // so this is where their code is compiled.
            StmtKind::Function(decl) => {
                if let Some(index) = self.f().numbered.get(&(Arc::as_ptr(decl) as usize)).copied() {
                    self.function(decl, index, span);
                }
            }
            StmtKind::Class(class) => {
                let Some(index) = self.f().numbered.get(&(class as *const Class as usize)).copied() else {
                    return;
                };
                let constructor = self.f().classes[index as usize].constructor;
                let methods = self.f().classes[index as usize].methods.clone();
                self.constructor(class, constructor, span);
                for (method, index) in class.methods.iter().zip(methods) {
                    self.function(method, index, span);
                }
            }
            StmtKind::Enum(_) => {}

            StmtKind::If { condition, then_block, else_block } => {
                self.expr(condition);
                let to_else = self.emit(Op::JumpIfFalse(0), condition.span);
                self.scoped(then_block, span);
                match else_block {
                    Some(else_block) => {
                        let to_end = self.emit(Op::Jump(0), span);
                        self.patch(to_else);
                        self.scoped(else_block, span);
                        self.patch(to_end);
                    }
                    None => self.patch(to_else),
                }
            }
            StmtKind::Unsafe(body) => self.scoped(body, span),
            // Tests are run by `zeno test`, which does not use the VM.
            StmtKind::Test { .. } | StmtKind::Assert(..) | StmtKind::AssertRaises { .. } => {}
            StmtKind::While { condition, body } => {
                let start = self.f().code.len();
                self.expr(condition);
                let to_end = self.emit(Op::JumpIfFalse(0), condition.span);
                self.begin_loop(start);
                self.scoped(body, span);
                self.emit(Op::Jump(start as u32), span);
                self.patch(to_end);
                self.end_loop();
            }
            StmtKind::For { variable, second: Some(second), iterable, body } => {
                // `for key, value in ages`. Three values the program
                // cannot see sit under the loop: what the second name is
                // given each round, the keys (or `none`, where the first
                // name is given positions), and how far the walk has got.
                self.expr(iterable);
                self.emit(Op::ForStartPairs, iterable.span);
                self.constant(Value::Int(0), span);
                let position = self.f().height - 1;

                let start = self.emit(Op::ForNextPair(0), span);
                self.f().height -= 1;
                self.begin_loop(start);
                self.f().height += 1;
                self.open();
                self.block().height -= 1;
                self.declare(variable, false, false, span);
                // The first name may have left the stack for a cell, so
                // how far down the position is, is worked out here.
                let below = self.f().height - 1 - position;
                self.emit(Op::ForSecond(below as u32), span);
                self.declare(second, false, false, span);
                self.statements(body);
                self.close(span);
                self.emit(Op::Jump(start as u32), span);

                self.patch(start);
                self.end_loop();
                self.emit(Op::Pop(3), span);
            }
            StmtKind::For { variable, second: None, iterable, body } => {
                // Two values the program cannot see sit under the loop:
                // the list being walked, and how far the walk has got.
                self.expr(iterable);
                self.emit(Op::ForStart, iterable.span);
                self.constant(Value::Int(0), span);

                let start = self.emit(Op::ForNext(0), span);
                // `continue` drops this round's item, so the loop's height
                // is taken before the item is counted.
                self.f().height -= 1;
                self.begin_loop(start);
                self.f().height += 1;
                // A fresh scope and a fresh loop variable for every round.
                self.open();
                self.block().height -= 1;
                self.declare(variable, false, false, span);
                self.statements(body);
                self.close(span);
                self.emit(Op::Jump(start as u32), span);

                self.patch(start);
                self.end_loop();
                self.emit(Op::Pop(2), span);
            }
            StmtKind::Match { subject, arms } => {
                self.expr(subject);
                let subject = self.f().height - 1;
                let mut ends = Vec::new();
                for arm in arms {
                    // The pattern's variables live in the arm's own scope.
                    self.open();
                    let mut fails = Vec::new();
                    self.pattern(&arm.pattern, Source::Slot(subject), span, &mut fails);
                    self.statements(&arm.body);
                    self.close(span);
                    ends.push(self.emit(Op::Jump(0), span));

                    // A pattern that did not fit comes here, with whatever
                    // it had put on the stack before it found out.
                    if !fails.is_empty() {
                        for fail in fails {
                            self.patch(fail);
                        }
                        self.emit(Op::PopTo(subject + 1), span);
                    }
                }
                self.emit(Op::NoMatch, span);
                for end in ends {
                    self.patch(end);
                }
                self.emit(Op::Pop(1), span);
            }
            StmtKind::Try { body, handlers } => {
                let height = self.f().height;
                let to_handlers = self.emit(Op::Try(0), span);
                self.f().tries += 1;
                self.scoped(body, span);
                self.f().tries -= 1;
                self.emit(Op::EndTry, span);
                let mut ends = vec![self.emit(Op::Jump(0), span)];

                // An error comes here. The VM has put the stack back to
                // the height it had at `Try`, and the error on top.
                self.patch(to_handlers);
                self.f().height = height + 1;
                let error = height;
                for handler in handlers {
                    // A bare `except` and `except Error` both catch everything.
                    let kind = handler.error.as_deref().filter(|kind| *kind != "Error");
                    let to_next = kind.map(|kind| {
                        let kind = self.text(kind);
                        self.emit(Op::Except(kind, 0), span)
                    });
                    self.open();
                    if let Some(binding) = &handler.binding {
                        self.emit_noted(Op::GetLocal(error), span, "the error");
                        if kind.is_some() {
                            self.emit(Op::Unwrap, span);
                        }
                        self.declare(binding, false, false, span);
                    }
                    self.statements(&handler.body);
                    self.close(span);
                    self.emit(Op::Pop(1), span);
                    ends.push(self.emit(Op::Jump(0), span));

                    self.f().height = height + 1;
                    if let Some(to_next) = to_next {
                        self.patch(to_next);
                    }
                }
                self.emit(Op::Reraise, span);
                for end in ends {
                    self.patch(end);
                }
            }

            StmtKind::Return(value) => {
                match value {
                    Some(value) => self.expr(value),
                    None => self.constant(Value::None, span),
                }
                if self.f().kind == Kind::Main {
                    self.mistake("`return` outside a function", span);
                    self.emit(Op::Pop(1), span);
                } else {
                    // The value first, and then the function's resources
                    // are released (decision 59.1).
                    self.release_from(0, span);
                    self.emit(Op::Return, span);
                }
            }
            StmtKind::Raise(value) => {
                self.expr(value);
                self.emit(Op::Raise, span);
            }
            StmtKind::Break => self.leave_loop(true, span),
            StmtKind::Continue => self.leave_loop(false, span),

            StmtKind::Import { module, names } => {
                let path = module.join(".");
                let Some(found) = builtins::module(&path) else {
                    return self.mistake(
                        format!("there is no module `{path}`. {}", builtins::MODULES),
                        span,
                    );
                };
                if names.is_empty() {
                    // `import math` makes the name `math`.
                    self.constant(Value::Module(found.clone()), span);
                    self.declare(found.name, false, false, span);
                    return;
                }
                for name in names {
                    let Some(member) = found.members.get(name.as_str()) else {
                        return self.mistake(format!("module `{path}` has no `{name}`"), span);
                    };
                    self.constant(member.clone(), span);
                    if !self.declare(name, false, false, span) {
                        return;
                    }
                }
            }
        }
    }

    fn begin_loop(&mut self, start: usize) {
        let (height, tries) = (self.f().height, self.f().tries);
        self.f().loops.push(Loop { height, tries, start, breaks: Vec::new() });
    }

    /// The end of a loop: every `break` in it jumps to here.
    fn end_loop(&mut self) {
        let finished = self.f().loops.pop().expect("a loop is open");
        for jump in finished.breaks {
            self.patch(jump);
        }
    }

    /// `break` or `continue`: put the stack back as it was at the top of
    /// the loop, and jump.
    fn leave_loop(&mut self, is_break: bool, span: Span) {
        let word = if is_break { "break" } else { "continue" };
        let Some(inside) = self.f().loops.last() else {
            return self.mistake(format!("`{word}` outside a loop"), span);
        };
        let (height, start, tries) = (inside.height, inside.start, inside.tries);
        // A resource whose `let` is inside the loop is released.
        let loops = self.f().loops.len();
        let first = self.f().scopeds.iter().position(|scoped| scoped.loops >= loops).unwrap_or(self.f().scopeds.len());
        let open = self.release_from(first, span);
        // Any `try` begun inside the loop is being left as well.
        for _ in tries..open {
            self.emit(Op::EndTry, span);
        }
        self.emit(Op::PopTo(height), span);
        if is_break {
            let jump = self.emit(Op::Jump(0), span);
            self.f().loops.last_mut().expect("a loop is open").breaks.push(jump);
        } else {
            self.emit(Op::Jump(start as u32), span);
        }
    }

    /// Release the resource in the variable `name`, if it still holds it
    /// (decision 59.1).
    fn release(&mut self, name: &str, span: Span) {
        self.load(name, span);
        self.emit(Op::Release, span);
    }

    /// Leave the `Scoped`s from number `first` on, innermost first: each
    /// `try` begun inside one is ended, its own included, so that its
    /// `release` is not caught by it, and then it is released. Returns
    /// how many `try` bodies are still open after them.
    fn release_from(&mut self, first: usize, span: Span) -> usize {
        let mut open = self.f().tries;
        let leaving: Vec<(String, Option<Place>, usize)> =
            self.f().scopeds[first..].iter().rev().map(|scoped| (scoped.name.clone(), scoped.place, scoped.tries)).collect();
        for (name, place, tries) in leaving {
            for _ in tries..open {
                self.emit(Op::EndTry, span);
            }
            open = tries;
            match place {
                Some(Place::Slot(slot)) => drop(self.emit_noted(Op::GetLocal(slot), span, &name)),
                Some(Place::Cell(cell)) => drop(self.emit(Op::GetCell(cell), span)),
                Some(Place::Captured(index)) => drop(self.emit(Op::GetCaptured(index), span)),
                None => self.constant(Value::None, span),
            }
            self.emit(Op::Release, span);
        }
        open
    }

    /// Store the value on top of the stack in `target`.
    fn assign(&mut self, target: &Expr) {
        let span = target.span;
        match &target.kind {
            ExprKind::Name(name) => {
                let place = match self.resolve(name) {
                    Some((place, true, _)) => Some(place),
                    Some(_) => None,
                    None if self.builtins.contains_key(name.as_str()) => None,
                    None => {
                        self.mistake(
                            format!("`{name}` is not declared. Create it with `let` or `var` first"),
                            span,
                        );
                        self.emit(Op::Pop(1), span);
                        return;
                    }
                };
                match place {
                    Some(Place::Slot(slot)) => self.emit_noted(Op::SetLocal(slot), span, name),
                    Some(Place::Cell(cell)) => self.emit(Op::SetCell(cell), span),
                    Some(Place::Captured(index)) => self.emit(Op::SetCaptured(index), span),
                    None => {
                        self.mistake(
                            format!("`{name}` cannot be changed. Declare it with `var` to allow that"),
                            span,
                        );
                        self.emit(Op::Pop(1), span)
                    }
                };
            }
            ExprKind::Field { object, name, .. } => {
                self.expr(object);
                let name = self.text(name);
                self.emit(Op::SetField(name), span);
            }
            ExprKind::Index { object, index } => {
                self.expr(object);
                self.expr(index);
                self.emit(Op::SetIndex, span);
            }
            // The parser only lets the three kinds above through.
            _ => {
                self.mistake("this cannot be assigned to", span);
                self.emit(Op::Pop(1), span);
            }
        }
    }

    /// Compile the test of one pattern against the value at `source`.
    ///
    /// A test that fails must jump to the next `case`. Those jumps are
    /// collected in `fails`, because where the next `case` begins is not
    /// known yet. A name in the pattern becomes a variable of the block
    /// being compiled, which is the arm's.
    fn pattern(&mut self, pattern: &Pattern, source: Source, span: Span, fails: &mut Vec<usize>) {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Literal(literal) => {
                self.source(source, span);
                self.expr(literal);
                self.emit(Op::Binary(BinaryOp::Eq), span);
                fails.push(self.emit(Op::JumpIfFalse(0), span));
            }
            Pattern::Name(name) => {
                // A name that is already a bare enum case, like `Idle`, is
                // compared. Any other name becomes a new variable holding
                // the value.
                if matches!(self.resolve(name), Some((_, _, true))) {
                    self.source(source, span);
                    self.load(name, span);
                    self.emit(Op::Binary(BinaryOp::Eq), span);
                    fails.push(self.emit(Op::JumpIfFalse(0), span));
                    return;
                }
                self.source(source, span);
                if self.block().variables.iter().any(|variable| variable.name == *name) {
                    // `Pair(a, a)`: the first `a` stands.
                    self.emit(Op::Pop(1), span);
                } else {
                    self.declare(name, false, false, span);
                }
            }
            Pattern::Variant { name, fields } => {
                // The value stays on the stack, out of sight, for as long
                // as the arm runs: its fields are read from there.
                self.source(source, span);
                let value = self.f().height - 1;
                if self.resolve(name).is_none() && !self.builtins.contains_key(name.as_str()) {
                    self.mistake(format!("`{name}` is not an enum case that carries values"), span);
                    self.constant(Value::None, span);
                } else {
                    self.load(name, span);
                }
                let name = self.text(name);
                self.emit(Op::IsCase(name, fields.len() as u32), span);
                fails.push(self.emit(Op::JumpIfFalse(0), span));
                for (index, field) in fields.iter().enumerate() {
                    self.pattern(field, Source::Field(value, index as u32), span, fails);
                }
            }
        }
    }

    /// Push the value a pattern is tested against.
    fn source(&mut self, source: Source, span: Span) {
        match source {
            Source::Slot(slot) => {
                self.emit_noted(Op::GetLocal(slot), span, "the value to match");
            }
            Source::Field(slot, index) => {
                self.emit_noted(Op::GetLocal(slot), span, "the value to match");
                self.emit(Op::CaseField(index), span);
            }
        }
    }

    // ------------------------------------------------------------ Functions

    /// Compile a function, as function number `index` of the one around it.
    fn function(&mut self, decl: &Arc<crate::ast::Function>, index: u32, span: Span) {
        let name = if decl.name.is_empty() { "function" } else { &decl.name };
        let params: Vec<String> = decl.params.iter().map(|param| param.name.clone()).collect();
        let defaults: Vec<&Expr> = decl.params.iter().filter_map(|param| param.default.as_deref()).collect();
        let shared = shared_names(&defaults, &decl.body);
        self.functions.push(Function::new(Kind::Function, name, Some(decl.clone()), params, shared));
        self.open();

        // The caller has put a value in the slot of every parameter it
        // gave. First the ones it left out are settled, in order, so that
        // a default can use the parameters before it.
        for (slot, param) in decl.params.iter().enumerate() {
            let slot = slot as u32;
            match &param.default {
                // Defaults are worked out fresh on every call.
                Some(default) => {
                    let given = self.emit(Op::Default(slot, 0), span);
                    self.expr(default);
                    self.emit_noted(Op::SetLocal(slot), span, &param.name);
                    self.patch(given);
                }
                None => {
                    let message = self.text(&format!("`{name}` needs a value for `{}`", param.name));
                    self.emit(Op::Required(slot, message), span);
                }
            }
            self.parameter(&param.name, slot, span);
        }
        self.emit(Op::Enter, span);

        self.statements(&decl.body);
        // Falling off the end without a `return` gives none.
        self.constant(Value::None, span);
        self.emit(Op::Return, span);

        let proto = self.finish();
        self.f().functions[index as usize] = Some(Rc::new(proto));
    }

    /// The parameter in `slot` becomes a variable.
    fn parameter(&mut self, name: &str, slot: u32, span: Span) {
        let place = if self.f().shared.contains(name) {
            // A function inside uses it, so it moves to a cell.
            let cell = self.new_cell(name);
            self.emit(Op::NewCell(cell), span);
            self.emit_noted(Op::GetLocal(slot), span, name);
            self.emit(Op::DefineCell(cell), span);
            Place::Cell(cell)
        } else {
            Place::Slot(slot)
        };
        self.block().variables.push(Variable { name: name.to_string(), place, mutable: false, case: false });
    }

    /// Compile the code that builds an object of a class. It is a function
    /// whose parameters are the fields: it works out the default of every
    /// field the caller left out, and makes the object.
    ///
    /// The fields are not given names here. A default is written in the
    /// class, outside any object, so it cannot use another field.
    fn constructor(&mut self, class: &Class, index: u32, span: Span) {
        let params: Vec<String> = class.fields.iter().map(|field| field.name.clone()).collect();
        let defaults: Vec<&Expr> = class.fields.iter().filter_map(|field| field.default.as_deref()).collect();
        let shared = shared_names(&defaults, &[]);
        self.functions.push(Function::new(Kind::Constructor, &class.name, None, params, shared));
        self.open();

        for (slot, field) in class.fields.iter().enumerate() {
            let slot = slot as u32;
            match &field.default {
                Some(default) => {
                    let given = self.emit(Op::Default(slot, 0), span);
                    self.expr(default);
                    self.emit_noted(Op::SetLocal(slot), span, &field.name);
                    self.patch(given);
                }
                None => {
                    let message =
                        self.text(&format!("`{}` needs a value for `{}`", class.name, field.name));
                    self.emit(Op::Required(slot, message), span);
                }
            }
        }
        self.emit(Op::Object(class.fields.len() as u32), span);
        self.emit(Op::Return, span);

        let proto = self.finish();
        self.f().functions[index as usize] = Some(Rc::new(proto));
    }

    /// The function being compiled is complete.
    fn finish(&mut self) -> Proto {
        let function = self.functions.pop().expect("a function is being compiled");
        Proto {
            name: function.name,
            decl: function.decl,
            params: function.params,
            fast: crate::bytecode::fuse(&function.code),
            code: function.code,
            spans: function.spans,
            notes: function.notes,
            constants: function.constants,
            names: function.names,
            functions: function
                .functions
                .into_iter()
                .map(|proto| proto.expect("a function was given a number and never compiled"))
                .collect(),
            classes: function.classes,
            enums: function.enums,
            cells: function.cells,
            captures: function.captures,
        }
    }

    // ---------------------------------------------------------- Expressions

    /// Compile an expression. The code it writes leaves exactly one more
    /// value on the stack: the expression's.
    fn expr(&mut self, expr: &Expr) {
        let span = expr.span;

        match &expr.kind {
            // A resource given away: its value, and `none` left in the
            // variable, so that its `Scoped` does not release it
            // (decision 59.1).
            ExprKind::Give(name) => {
                self.load(name, span);
                self.constant(Value::None, span);
                match self.resolve(name) {
                    Some((Place::Slot(slot), ..)) => drop(self.emit_noted(Op::SetLocal(slot), span, name)),
                    Some((Place::Cell(cell), ..)) => drop(self.emit(Op::SetCell(cell), span)),
                    Some((Place::Captured(index), ..)) => drop(self.emit(Op::SetCaptured(index), span)),
                    None => drop(self.emit(Op::Pop(1), span)),
                }
            }
            ExprKind::Int(n) => match i64::try_from(*n) {
                Ok(n) => self.constant(Value::Int(n), span),
                Err(_) => {
                    self.mistake("this number is too big for an int", span);
                    self.constant(Value::None, span);
                }
            },
            ExprKind::Float(x) => self.constant(Value::Float(*x), span),
            ExprKind::Str(text) => self.constant(Value::str(text), span),
            // A block of another language is a call of the function
            // that starts it, with the block's language and its text
            // (decision 61).
            ExprKind::Inline { language, text } => {
                self.constant(crate::builtins::inline_block(), span);
                self.constant(Value::str(language), span);
                self.constant(Value::str(text), span);
                self.emit(Op::Call(2), span);
            }
            ExprKind::Bool(b) => self.constant(Value::Bool(*b), span),
            ExprKind::None => self.constant(Value::None, span),

            ExprKind::FormatStr(parts) => {
                for part in parts {
                    match part {
                        FormatPart::Text(plain) => self.constant(Value::str(plain), span),
                        FormatPart::Value { value, format } => {
                            self.expr(value);
                            match format {
                                Some(format) => {
                                    let format = self.text(format);
                                    self.emit(Op::FormatWith(format), span);
                                }
                                None => {
                                    self.emit(Op::Format, span);
                                }
                            }
                        }
                    }
                }
                self.emit(Op::Text(parts.len() as u32), span);
            }

            ExprKind::Name(name) => self.load(name, span),

            ExprKind::Unit { value, unit } => {
                self.expr(value);
                if units::is_unit(unit) {
                    let unit = self.text(unit);
                    self.emit(Op::Unit(unit), span);
                } else {
                    self.mistake(units::unknown(unit), span);
                }
            }

            ExprKind::List(items) => {
                for item in items {
                    self.expr(item);
                }
                self.emit(Op::List(items.len() as u32), span);
            }
            ExprKind::Several(items) => {
                for item in items {
                    self.expr(item);
                }
                self.emit(Op::Several(items.len() as u32), span);
            }
            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key);
                    self.expr(value);
                }
                self.emit(Op::Dict(entries.len() as u32), span);
            }
            ExprKind::ListComp { element, variable, iterable, condition } => {
                // A `for` loop that adds to a list the program cannot see
                // until it is complete.
                self.emit(Op::List(0), span);
                let result = self.f().height - 1;
                self.expr(iterable);
                self.emit(Op::ForStart, iterable.span);
                self.constant(Value::Int(0), span);

                let start = self.emit(Op::ForNext(0), span);
                self.open();
                self.block().height -= 1;
                self.declare(variable, false, false, span);
                let skip = condition.as_ref().map(|condition| {
                    self.expr(condition);
                    self.emit(Op::JumpIfFalse(0), condition.span)
                });
                self.expr(element);
                self.emit(Op::Append(result), span);
                if let Some(skip) = skip {
                    self.patch(skip);
                }
                self.close(span);
                self.emit(Op::Jump(start as u32), span);

                self.patch(start);
                self.emit(Op::Pop(2), span);
            }

            ExprKind::Unary { op, operand } => {
                self.expr(operand);
                self.emit(Op::Unary(*op), span);
            }

            // `and` and `or` only work out their right side when they have to.
            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                self.expr(left);
                let decided = match op {
                    BinaryOp::And => self.emit(Op::And(0), left.span),
                    _ => self.emit(Op::Or(0), left.span),
                };
                self.expr(right);
                self.emit(Op::CheckBool(*op), right.span);
                self.patch(decided);
            }
            ExprKind::Binary { op, left, right } => {
                self.expr(left);
                self.expr(right);
                self.emit(Op::Binary(*op), span);
            }
            ExprKind::Compare { first, rest } => {
                // `a < b < c`: compare neighbours, stop at the first false,
                // and work out each operand once.
                let height = self.f().height;
                self.expr(first);
                let mut fails = Vec::new();
                for (index, (op, operand)) in rest.iter().enumerate() {
                    self.expr(operand);
                    if index + 1 < rest.len() {
                        fails.push(self.emit(Op::Chain(*op, 0), span));
                    } else {
                        self.emit(Op::Binary(*op), span);
                    }
                }
                let to_end = self.emit(Op::Jump(0), span);
                // A link that was false comes here with nothing on the stack.
                for fail in fails {
                    self.patch(fail);
                }
                self.f().height = height;
                self.constant(Value::Bool(false), span);
                self.patch(to_end);
            }

            // A function written as a value is compiled where it stands,
            // so it sees the variables declared above it.
            ExprKind::Function(decl) => {
                let index = self.number_function();
                self.function(decl, index, span);
                self.emit(Op::Closure(index), span);
            }

            ExprKind::Call { callee, args } => {
                self.expr(callee);
                let mut names = Vec::new();
                for arg in args {
                    self.expr(&arg.value);
                    match &arg.name {
                        Some(name) => names.push(name.clone()),
                        None if names.is_empty() => {}
                        None => self.mistake("plain arguments must come before named ones", arg.value.span),
                    }
                }
                if names.is_empty() {
                    self.emit(Op::Call(args.len() as u32), span);
                } else {
                    self.f().names.push(names);
                    let names = self.f().names.len() as u32 - 1;
                    self.emit(Op::CallNamed(args.len() as u32, names), span);
                }
            }

            ExprKind::Field { object, name, .. } => {
                self.expr(object);
                let name = self.text(name);
                self.emit(Op::GetField(name), span);
            }
            ExprKind::Index { object, index } => {
                self.expr(object);
                self.expr(index);
                self.emit(Op::GetIndex, span);
            }
            // `take slot.task`: what is there, leaving `none`. A failure
            // names the place, as reading it would.
            ExprKind::Take(place) => match &place.kind {
                ExprKind::Field { object, name, .. } => {
                    self.expr(object);
                    let name = self.text(name);
                    self.emit(Op::TakeField(name), place.span);
                }
                ExprKind::Index { object, index } => {
                    self.expr(object);
                    self.expr(index);
                    self.emit(Op::TakeIndex, place.span);
                }
                // The parser lets nothing else through.
                _ => {
                    self.mistake("`take` goes in front of a field or an item", span);
                }
            },
        }
    }
}

// ---------------------------------------------------------------- Shared names

/// The names used inside the functions and classes written in a piece of
/// code: in a function's body, say, or the whole program.
///
/// A variable with one of these names may be used by a function written
/// inside, and outlive the call that made it, so it is kept in a cell.
/// This goes by name alone, which is cruder than it could be: a variable
/// `i` is put in a cell if any inner function mentions an `i`, even a
/// different one. That costs a little speed and is never wrong.
fn shared_names(defaults: &[&Expr], body: &[Stmt]) -> HashSet<String> {
    let mut names = Names { found: HashSet::new(), inside: false };
    for default in defaults {
        names.expr(default);
    }
    names.block(body);
    names.found
}

struct Names {
    found: HashSet<String>,
    /// True while walking through a function or class written inside.
    inside: bool,
}

impl Names {
    fn add(&mut self, name: &str) {
        if self.inside {
            self.found.insert(name.to_string());
        }
    }

    fn block(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn function(&mut self, decl: &crate::ast::Function) {
        let was_inside = std::mem::replace(&mut self.inside, true);
        for param in &decl.params {
            if let Some(default) = &param.default {
                self.expr(default);
            }
        }
        self.block(&decl.body);
        self.inside = was_inside;
    }

    fn statement(&mut self, statement: &Stmt) {
        match &statement.kind {
            // Its release reads the resource by name.
            StmtKind::Scoped { name, body } => {
                self.add(name);
                self.block(body);
            }
            StmtKind::Let { value, .. } | StmtKind::Const { value, .. } => self.expr(value),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Receive { places, value, .. } => {
                for place in places {
                    if let crate::ast::Place::Target(target) = place {
                        self.expr(target);
                    }
                }
                self.expr(value);
            }
            StmtKind::Expr(expr) | StmtKind::Raise(expr) => self.expr(expr),
            StmtKind::Function(decl) => self.function(decl),
            StmtKind::Class(class) => {
                // The defaults of the fields are worked out by the code
                // that builds an object, which is a function of its own.
                let was_inside = std::mem::replace(&mut self.inside, true);
                for field in &class.fields {
                    if let Some(default) = &field.default {
                        self.expr(default);
                    }
                }
                self.inside = was_inside;
                for method in &class.methods {
                    self.function(method);
                }
            }
            StmtKind::If { condition, then_block, else_block } => {
                self.expr(condition);
                self.block(then_block);
                if let Some(else_block) = else_block {
                    self.block(else_block);
                }
            }
            StmtKind::Unsafe(body) => self.block(body),
            StmtKind::Test { .. } | StmtKind::Assert(..) | StmtKind::AssertRaises { .. } => {}
            StmtKind::While { condition, body } => {
                self.expr(condition);
                self.block(body);
            }
            StmtKind::For { iterable, body, .. } => {
                self.expr(iterable);
                self.block(body);
            }
            StmtKind::Match { subject, arms } => {
                self.expr(subject);
                for arm in arms {
                    self.pattern(&arm.pattern);
                    self.block(&arm.body);
                }
            }
            StmtKind::Try { body, handlers } => {
                self.block(body);
                for handler in handlers {
                    self.block(&handler.body);
                }
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            StmtKind::Enum(_) | StmtKind::Break | StmtKind::Continue | StmtKind::Import { .. } => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Name(name) => self.add(name),
            Pattern::Literal(literal) => self.expr(literal),
            Pattern::Variant { name, fields } => {
                self.add(name);
                for field in fields {
                    self.pattern(field);
                }
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
            ExprKind::Name(name) | ExprKind::Give(name) => self.add(name),
            ExprKind::FormatStr(parts) => {
                for part in parts {
                    if let FormatPart::Value { value, .. } = part {
                        self.expr(value);
                    }
                }
            }
            ExprKind::Unit { value, .. } => self.expr(value),
            ExprKind::List(items) | ExprKind::Several(items) => {
                for item in items {
                    self.expr(item);
                }
            }
            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key);
                    self.expr(value);
                }
            }
            ExprKind::ListComp { element, iterable, condition, .. } => {
                self.expr(iterable);
                if let Some(condition) = condition {
                    self.expr(condition);
                }
                self.expr(element);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Binary { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ExprKind::Compare { first, rest } => {
                self.expr(first);
                for (_, operand) in rest {
                    self.expr(operand);
                }
            }
            ExprKind::Function(decl) => self.function(decl),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for arg in args {
                    self.expr(&arg.value);
                }
            }
            ExprKind::Field { object, .. } | ExprKind::Take(object) => self.expr(object),
            ExprKind::Index { object, index } => {
                self.expr(object);
                self.expr(index);
            }
        }
    }
}
