//! The interpreter: runs a program by walking its syntax tree.
//!
//! There are two central functions, one for each kind of tree node:
//!
//! - `exec` runs a statement. It returns nothing, but changes things:
//!   declares a variable, prints, loops.
//! - `eval` works out an expression and returns its `Value`.
//!
//! Both are a `match` on the node kind, and both call themselves for the
//! nodes nested inside. That is all a "tree-walking" interpreter is.
//!
//! Types written in the source (`x: int`) are not checked here. The
//! interpreter finds mistakes only when it reaches them; the type checker,
//! the next stage, will find them before the program runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::board::Board;
use crate::page::Page;
use crate::builtins;
use crate::error::{counted, Error};
use crate::ops;
use crate::token::Span;
use crate::value::*;

/// How deep function calls may nest before we assume runaway recursion.
#[cfg(not(target_arch = "wasm32"))]
const MAX_DEPTH: usize = 1000;
/// In a browser the stack is the browser's, and far smaller. A program
/// that goes past the end of it is stopped by the browser, which cannot
/// say which line of Zeno did it, so the limit is set where Zeno can
/// still say so. Chromium was measured: a plain function that calls
/// itself got past 700 calls and not to 1000, and one that calls itself
/// from inside a loop, a `match` and a `map` got past 400 and not to
/// 500. 200 leaves room for a browser with half that stack.
#[cfg(target_arch = "wasm32")]
const MAX_DEPTH: usize = 200;
/// The interpreter calls itself for every nested node, so deep Zeno
/// recursion needs a deep Rust stack. It runs on its own thread to get one.
const STACK_SIZE: usize = 256 * 1024 * 1024;
/// In a browser, how long one turn of a program (its first run, or one
/// click, key or timer) may go on before it is stopped. A page does
/// nothing else while Zeno runs: not draw, not scroll, not let anyone
/// press a button to stop it. A loop that never ends would hold the
/// page, and the playground with it, for good (decisions 19.1).
#[cfg(target_arch = "wasm32")]
const TURN_MS: f64 = 5000.0;
/// The clock is asked once in this many loop rounds and calls: asking
/// it crosses into JavaScript, and that costs more than a round.
#[cfg(target_arch = "wasm32")]
const TICKS_PER_LOOK: u32 = 1 << 16;

/// Run a program. With `capture` the printed text is returned instead of
/// going to the screen, which is how the tests check output. `outside`
/// is what is typed at the program, and the words after the file on the
/// command line.
#[cfg(test)]
pub fn run_with(program: &Program, capture: bool, outside: Outside) -> (String, Result<(), Error>) {
    let ran = run_ending(program, capture, outside);
    (ran.output, ran.result)
}

/// The same, with what the program wrote on the error stream and the
/// number it ended with: what `zeno run` hands back to the shell.
pub fn run_ending(program: &Program, capture: bool, outside: Outside) -> Ran {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, || {
                let mut interpreter =
                    Interpreter { output: capture.then(String::new), warned: String::new(), depth: 0, board: Box::new(Board::new()), page: Page::simulated(outside.page.as_deref()), outside, ticks: 0, turn_began: 0.0, top: None };
                // Built-ins live in the outermost scope; the program gets its own inside it.
                let env = Scope::child(&builtins::globals());
                let finished = interpreter.run_statements(&program.statements, &env);
                // Another language that was started beside the program
                // (`import python`) is told the program is over.
                crate::bridge::close();
                Ran::ended(interpreter.output.unwrap_or_default(), interpreter.warned, finished)
            })
            .expect("could not start the interpreter thread")
            .join()
            .expect("the interpreter crashed")
    })
}

/// One test, after it was run: its name, and what went wrong if
/// anything did.
pub struct Tested {
    pub name: String,
    pub failure: Option<Error>,
}

/// `zeno test`: run the program, and then each test that is written in
/// its first file, in the order written. The tests of the files it
/// imports are those files' own, and are run when they are tested.
/// With `only`, the tests that have that text in their name are run
/// and the others are passed over.
/// Each test says `ok` or `FAILED` on a line as it ends. `Err` if the
/// program itself stopped before its tests could run.
pub fn run_tests(program: &Program, capture: bool, outside: Outside, only: Option<&str>) -> (String, Result<Vec<Tested>, Error>) {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, || {
                let mut interpreter =
                    Interpreter { output: capture.then(String::new), warned: String::new(), depth: 0, board: Box::new(Board::new()), page: Page::simulated(outside.page.as_deref()), outside, ticks: 0, turn_began: 0.0, top: None };
                let env = Scope::child(&builtins::globals());
                // The tests of the first file. They are run where the top
                // of the file ends: after its last line, and inside each
                // resource it holds, which is released after them (59.1).
                let mut tests = Vec::new();
                for statement in crate::ast::flat(&program.statements) {
                    let StmtKind::Test { name, body } = &statement.kind else {
                        continue;
                    };
                    // `zeno test --only "empty"`: the tests with that in
                    // their name, and no others.
                    if statement.span.file() == 0 && !only.is_some_and(|text| !name.contains(text)) {
                        tests.push((name.clone(), body.clone()));
                    }
                }
                interpreter.top = Some(Box::new(Top { env: env.clone(), tests, tested: None }));
                let started = interpreter.run_statements(&program.statements, &env).map_err(ops::escaped);
                // With nothing at the top to release, the top ends here.
                if started.is_ok() && interpreter.top.as_ref().is_some_and(|top| top.tested.is_none()) {
                    interpreter.run_tests_here();
                }
                let tested = interpreter.top.take().and_then(|top| top.tested).unwrap_or_default();
                crate::bridge::close();
                (interpreter.output.unwrap_or_default(), started.map(|()| tested))
            })
            .expect("could not start the interpreter thread")
            .join()
            .expect("the interpreter crashed")
    })
}

/// A program that stays: one in a page.
///
/// `zeno run` runs a program from its first line to its last, and that
/// is the end of it. A page is not like that. The program runs to its
/// last line, and then the page is still there, and somebody presses a
/// button: the function the program gave to `on` has to be called, with
/// every variable as the program left it. So the interpreter and the
/// program's scope are kept, here, for as long as the page is open.
///
/// There is no thread with a deep stack here, as `run_with` has: a
/// browser gives a program one thread and one stack.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub struct Session {
    interpreter: Interpreter,
    env: Env,
    program: Program,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl Session {
    /// Run the program from its first line to its last, on this page.
    /// What it printed is given back if `capture` says to keep it.
    pub fn start(program: Program, page: Page, capture: bool) -> (Session, Result<(), Error>) {
        let interpreter = Interpreter {
            output: capture.then(String::new),
            warned: String::new(),
            depth: 0,
            board: Box::new(Board::new()),
            page,
            outside: Outside::default(),
            ticks: 0,
            turn_began: 0.0,
            top: None,
        };
        let env = Scope::child(&builtins::globals());
        let mut session = Session { interpreter, env, program };
        session.interpreter.begin_turn();
        let result = session.interpreter.run_statements(&session.program.statements, &session.env);
        (session, in_page(result))
    }

    /// The page calls one of the functions it was given: the one that
    /// `on`, `after` or `on_key` kept under this number.
    pub fn call(&mut self, handler: u32, arguments: Vec<Value>) -> Result<(), Error> {
        let Some(handler) = self.interpreter.page.handler(handler) else {
            return Ok(());
        };
        // The event goes to a function that asked for it.
        let arguments = arguments.into_iter().filter(|argument| handler.takes_event || !matches!(argument, Value::Event(_))).collect();
        let function = handler.function;
        // An error may have left calls open the last time.
        self.interpreter.depth = 0;
        self.interpreter.begin_turn();
        let span = Span { line: 1, col: 1 };
        in_page(self.interpreter.call(function, arguments, Vec::new(), span).map(|_| ()))
    }

    /// What has been printed and kept since the last time this was asked.
    #[cfg(test)]
    pub fn printed(&mut self) -> String {
        self.interpreter.output.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

/// How a turn of a program in a page finished. A page cannot end:
/// `program.exit` ends the turn that is running, and the page stays as
/// it is, with its buttons (decision 34.1).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn in_page(finished: Result<(), Control>) -> Result<(), Error> {
    match (finished, crate::value::exited()) {
        (_, Some(_)) => Ok(()),
        (other, None) => other.map_err(ops::escaped),
    }
}

// --------------------------------------------------------------------- Scopes

/// A scope is one set of variables. Scopes nest: each `{ }` block gets a new
/// one whose `parent` is the scope around it. Looking up a name walks outward
/// until it is found.
pub struct Scope {
    variables: RefCell<HashMap<String, Rc<Variable>>>,
    parent: Option<Env>,
    /// Set for a scope that is a *view* of another (see `seen`). A view
    /// has no variables of its own.
    view: Option<View>,
}

/// What a function can see of the block it is written in.
struct View {
    /// The block's scope.
    of: Env,
    /// How many of that scope's variables can be seen: the ones declared
    /// first.
    count: usize,
    /// The names of the variables among those that the block declares
    /// with `let`, `var`, `const` or `import`. When the function is called
    /// from above the line that declares one of them, the variable does
    /// not exist yet. The name still means that variable, and not one of
    /// the same name further out.
    lets: Rc<[String]>,
}

/// Scopes are shared (`Rc`) because a function keeps hold of the scope it
/// was defined in for as long as the function exists.
pub type Env = Rc<Scope>;

/// One variable. It is shared (`Rc`) because a function written inside
/// another keeps hold of the variables it uses: both must see the same
/// variable, so that a change made by one is seen by the other.
struct Variable {
    value: RefCell<Value>,
    /// True for `var`, false for `let`, `const`, parameters and loop variables.
    mutable: bool,
    /// How many variables its scope had before this one: 0 for the first
    /// to be declared there, 1 for the second.
    number: usize,
}

enum AssignError {
    NotDeclared,
    Immutable,
}

impl Scope {
    pub fn root() -> Env {
        Rc::new(Scope { variables: RefCell::new(HashMap::new()), parent: None, view: None })
    }

    pub fn child(parent: &Env) -> Env {
        Rc::new(Scope { variables: RefCell::new(HashMap::new()), parent: Some(parent.clone()), view: None })
    }

    /// Create a variable in this scope. Returns false if the name is taken here.
    pub fn declare(&self, name: &str, value: Value, mutable: bool) -> bool {
        let mut variables = self.variables.borrow_mut();
        if variables.contains_key(name) {
            return false;
        }
        let number = variables.len();
        variables.insert(name.to_string(), Rc::new(Variable { value: RefCell::new(value), mutable, number }));
        true
    }

    /// How many variables have been declared in this scope so far.
    fn count(&self) -> usize {
        self.variables.borrow().len()
    }

    /// The scope a function keeps: what can be seen from where it is
    /// written, and nothing that is declared later.
    ///
    /// Keeping `env` itself would be wrong. A variable declared further
    /// down the block would then be visible inside the function, and could
    /// hide an outer variable of the same name that the function was
    /// written to use.
    ///
    /// So the function gets a **view** of `env`: a scope with no variables
    /// of its own, which looks into `env` and sees only the first `count`
    /// variables declared there. They are the same variables, not copies,
    /// so changes still pass both ways. The scopes around `env` are treated
    /// the same way, each seen as it is at this moment.
    fn seen(env: &Env, count: usize, lets: &[String]) -> Env {
        // The built-ins never gain a variable, and a view never changes.
        let Some(parent) = &env.parent else {
            return env.clone();
        };
        if env.view.is_some() {
            return env.clone();
        }
        let parent = Scope::seen(parent, parent.count(), &[]);
        Rc::new(Scope {
            variables: RefCell::new(HashMap::new()),
            parent: Some(parent),
            view: Some(View { of: env.clone(), count, lets: Rc::from(lets) }),
        })
    }

    /// Find a variable by name, here or in a scope around this one.
    fn find(&self, name: &str) -> Option<Rc<Variable>> {
        let here = match &self.view {
            Some(view) => {
                let seen =
                    view.of.variables.borrow().get(name).filter(|variable| variable.number < view.count).cloned();
                if seen.is_none() && view.lets.iter().any(|declared| declared == name) {
                    // The block declares it, on a line that has not run yet.
                    return None;
                }
                seen
            }
            None => self.variables.borrow().get(name).cloned(),
        };
        here.or_else(|| self.parent.as_ref()?.find(name))
    }

    fn get(&self, name: &str) -> Option<Value> {
        Some(self.find(name)?.value.borrow().clone())
    }

    /// The value of `name`, which is given away: the variable holds
    /// `none` after it, which a resource never is (decision 59.1).
    fn give(&self, name: &str) -> Option<Value> {
        let variable = self.find(name)?;
        let value = std::mem::replace(&mut *variable.value.borrow_mut(), Value::None);
        Some(value)
    }

    fn assign(&self, name: &str, value: Value) -> Result<(), AssignError> {
        let variable = self.find(name).ok_or(AssignError::NotDeclared)?;
        if !variable.mutable {
            return Err(AssignError::Immutable);
        }
        *variable.value.borrow_mut() = value;
        Ok(())
    }
}

// ---------------------------------------------------------------- Interpreter

pub struct Interpreter {
    /// `Some` collects printed text; `None` prints straight to the screen.
    output: Option<String>,
    /// What `program.warn` wrote, when printed text is collected.
    warned: String,
    /// The simulated board, for the `board` module. In a box of its
    /// own, so that what the board comes to hold cannot move the fields
    /// around it (decisions 34.7 and 50.7).
    board: Box<Board>,
    /// The page, for the `web` module: simulated, unless the program is
    /// in a browser.
    page: Page,
    /// The keyboard and the command line, for `input` and `program`.
    outside: Outside,
    /// How many function calls are currently in progress.
    depth: usize,
    /// Loop rounds and calls since the clock was last looked at, and
    /// when this turn began (only looked at in a browser: see `tick`).
    ticks: u32,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    turn_began: f64,
    /// For `zeno test`: the tests, which run where the top of the file
    /// ends, after its last line and before what the top holds is
    /// released (decision 59.1).
    top: Option<Box<Top>>,
}

/// The top of the file being tested: its scope, its tests, and what
/// they came to once they have run.
struct Top {
    env: Env,
    tests: Vec<(String, Vec<Stmt>)>,
    tested: Option<Vec<Tested>>,
}

/// What the built-ins ask of the interpreter (see `Machine` in value.rs).
impl Machine for Interpreter {
    fn write_line(&mut self, text: &str) {
        match &mut self.output {
            Some(buffer) => {
                buffer.push_str(text);
                buffer.push('\n');
            }
            None => print_line(text),
        }
    }

    fn write(&mut self, text: &str) {
        match &mut self.output {
            Some(buffer) => buffer.push_str(text),
            None => print_text(text),
        }
    }

    fn warn(&mut self, text: &str) {
        match &mut self.output {
            // Kept apart from what is printed, as the shell keeps it.
            Some(_) => {
                self.warned.push_str(text);
                self.warned.push('\n');
            }
            None => warn_line(text),
        }
    }

    fn outside(&mut self) -> &mut Outside {
        &mut self.outside
    }

    fn board(&mut self) -> &mut Board {
        &mut self.board
    }

    fn page(&mut self) -> &mut Page {
        &mut self.page
    }

    fn call(
        &mut self,
        callee: Value,
        positional: Vec<Value>,
        named: Vec<(String, Value)>,
        span: Span,
    ) -> Result<Value, Control> {
        Interpreter::call(self, callee, positional, named, span)
    }
}

/// The scope a function or class made by the interpreter was defined in.
fn scope_of(home: &Home) -> Env {
    match home {
        Home::Scope(scope) => scope.clone(),
        Home::Code(..) => unreachable!("the interpreter met a function that the VM made"),
    }
}

impl Interpreter {

    /// A turn of the program begins: its first run, or a call from the
    /// page. The time it may take is counted from here.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn begin_turn(&mut self) {
        self.ticks = 0;
        #[cfg(target_arch = "wasm32")]
        {
            self.turn_began = crate::web_host::now();
        }
    }

    /// One more round of a loop, or one more call. In a browser, a turn
    /// that has gone on for longer than `TURN_MS` is stopped, with the
    /// line it was on. Anywhere else a program runs for as long as it
    /// likes, as programs do, and this does nothing.
    #[inline]
    fn tick(&mut self, span: Span) -> Result<(), Control> {
        #[cfg(target_arch = "wasm32")]
        {
            self.ticks = self.ticks.wrapping_add(1);
            if self.ticks % TICKS_PER_LOOK == 0 && crate::web_host::now() - self.turn_began > TURN_MS {
                let seconds = TURN_MS / 1000.0;
                return fatal(
                    format!(
                        "the program ran for {seconds} seconds without stopping, so it was stopped. \
                         A page can do nothing else while it runs. Is a loop going on forever?"
                    ),
                    span,
                );
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = span;
        Ok(())
    }

    fn declare(
        &self,
        env: &Env,
        name: &str,
        value: Value,
        mutable: bool,
        span: Span,
    ) -> Result<(), Control> {
        if env.declare(name, value, mutable) {
            Ok(())
        } else {
            fatal(format!("`{name}` is already declared in this block"), span)
        }
    }

    // ------------------------------------------------------------ Statements

    /// `assert total == 6`. A failure is not an error of the program
    /// that an `except` could catch: it ends the test at once, and says
    /// what was on each side of a comparison.
    fn assert(&mut self, condition: &Expr, message: Option<&Expr>, env: &Env) -> Result<(), Control> {
        let span = condition.span;
        let compares = |op: &BinaryOp| {
            use BinaryOp::*;
            matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq | Is | IsNot | In | NotIn)
        };
        let false_because = match &condition.kind {
            ExprKind::Binary { op, left, right } if compares(op) => {
                let left = self.eval(left, env)?;
                let right = self.eval(right, env)?;
                let (was_left, was_right) = (left.repr(), right.repr());
                match ops::binary(*op, left, right, span)? {
                    Value::Bool(true) => return Ok(()),
                    _ => format!("this is false. The left of `{}` is {was_left}, and the right is {was_right}", op.text()),
                }
            }
            _ => match self.eval(condition, env)? {
                Value::Bool(true) => return Ok(()),
                _ => "this is false".to_string(),
            },
        };
        // The text of `assert c, "text"` is worked out only now that the
        // `assert` has failed, and goes on a line of its own above what
        // was false (decision 60.1).
        match message {
            Some(message) => match self.eval(message, env)? {
                Value::Str(text) => fatal(format!("{text}\n{false_because}"), span),
                other => fatal(format!("{other}\n{false_because}"), span),
            },
            None => fatal(false_because, span),
        }
    }

    /// Run a `{ }` block in a fresh scope of its own.
    fn exec_block(&mut self, statements: &[Stmt], env: &Env) -> Result<(), Control> {
        self.run_statements(statements, &Scope::child(env))
    }

    /// Run statements in the given scope, one after another.
    fn run_statements(&mut self, statements: &[Stmt], env: &Env) -> Result<(), Control> {
        self.hoist(statements, env)?;
        for statement in statements {
            self.exec(statement, env)?;
        }
        Ok(())
    }

    /// Declare the block's functions, classes and enums before running any
    /// of it, so they can be used above the line where they are written.
    ///
    /// Each sees the block as it will be when the program reaches the line
    /// it is written on: everything declared here, and the variables
    /// declared above that line. Those variables do not exist yet, so what
    /// is worked out is how many there will be (`above`) and what they are
    /// called (`lets`).
    fn hoist(&mut self, statements: &[Stmt], env: &Env) -> Result<(), Control> {
        // What follows the `let` of a resource is more of this block
        // (decision 59.1).
        let statements = crate::ast::flat(statements);
        let hoisted: usize = statements
            .iter()
            .map(|statement| match &statement.kind {
                StmtKind::Function(_) | StmtKind::Class(_) => 1,
                StmtKind::Enum(decl) => 1 + decl.variants.len(),
                _ => 0,
            })
            .sum();
        let mut above = env.count() + hoisted;
        let mut lets: Vec<String> = Vec::new();

        for statement in statements {
            let span = statement.span;
            match &statement.kind {
                StmtKind::Function(decl) => {
                    let home = Scope::seen(env, above, &lets);
                    let closure = Closure { decl: decl.clone(), home: Home::Scope(home) };
                    self.declare(env, &decl.name, Value::Function(Rc::new(closure)), false, span)?;
                }
                StmtKind::Class(class) => {
                    let home = Scope::seen(env, above, &lets);
                    let methods = class
                        .methods
                        .iter()
                        .map(|decl| {
                            let closure = Closure { decl: decl.clone(), home: Home::Scope(home.clone()) };
                            (decl.name.clone(), Rc::new(closure))
                        })
                        .collect();
                    let def = ClassDef {
                        name: class.name.clone(),
                        fields: class.fields.clone(),
                        methods,
                        home: Home::Scope(home),
                    };
                    self.declare(env, &class.name, Value::Class(Rc::new(def)), false, span)?;
                }
                StmtKind::Enum(decl) => {
                    let def = Rc::new(EnumDef { name: decl.name.clone(), cases: decl.variants.clone() });
                    self.declare(env, &decl.name, Value::Enum(def.clone()), false, span)?;
                    // Each case is also a name of its own, so you can write
                    // `Circle(1.0)` as well as `Shape.Circle(1.0)`.
                    for (index, case) in decl.variants.iter().enumerate() {
                        self.declare(env, &case.name, ops::case_value(&def, index), false, span)?;
                    }
                }
                // Each of these adds to what the functions below it can see.
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => {
                    above += 1;
                    lets.push(name.clone());
                }
                StmtKind::Receive { places, .. } => {
                    for name in places.iter().filter_map(Place::declared) {
                        above += 1;
                        lets.push(name.clone());
                    }
                }
                StmtKind::Import { module, names } => {
                    // `import a.b` declares one name, the module's.
                    above += names.len().max(1);
                    lets.extend(names.iter().cloned());
                    if names.is_empty() {
                        lets.extend(module.last().cloned());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn exec(&mut self, statement: &Stmt, env: &Env) -> Result<(), Control> {
        let span = statement.span;

        match &statement.kind {
            StmtKind::Let { mutable, name, value, .. } => {
                let value = self.eval(value, env)?;
                self.declare(env, name, value, *mutable, span)
            }
            StmtKind::Const { name, value, .. } => {
                let value = self.eval(value, env)?;
                self.declare(env, name, value, false, span)
            }
            // All of the right first, then each place, left to right
            // (decision 57).
            StmtKind::Receive { declare, places, value } => {
                let values = match &value.kind {
                    ExprKind::Several(values) => {
                        let mut given = Vec::with_capacity(values.len());
                        for value in values {
                            given.push(self.eval(value, env)?);
                        }
                        given
                    }
                    _ => crate::value::received(self.eval(value, env)?, places.len(), span)?,
                };
                for (place, value) in places.iter().zip(values) {
                    match place {
                        Place::Drop => {}
                        Place::Name { name, .. } => self.declare(env, name, value, *declare == Some(true), span)?,
                        Place::Target(target) => self.assign(target, value, env)?,
                    }
                }
                Ok(())
            }
            StmtKind::Assign { target, op, value } => {
                let new = match op {
                    None => self.eval(value, env)?,
                    // `x += 1` is `x = x + 1`: what is there is read,
                    // and then the value is worked out.
                    Some(op) => {
                        let old = self.eval(target, env)?;
                        let new = self.eval(value, env)?;
                        ops::binary(*op, old, new, span)?
                    }
                };
                self.assign(target, new, env)
            }
            StmtKind::Expr(expr) => {
                self.eval(expr, env)?;
                Ok(())
            }

            // Already declared by `hoist`.
            StmtKind::Function(_) | StmtKind::Class(_) | StmtKind::Enum(_) => Ok(()),

            StmtKind::If { condition, then_block, else_block } => {
                if self.condition(condition, env)? {
                    self.exec_block(then_block, env)
                } else if let Some(else_block) = else_block {
                    self.exec_block(else_block, env)
                } else {
                    Ok(())
                }
            }
            // The checker has seen to what may be written inside. To
            // run, it is a block like any other.
            StmtKind::Unsafe(body) => self.exec_block(body, env),
            // The rest of the block after the `let` of a resource: however
            // it is left, the resource is released if it is still there,
            // and the leaving goes on as it was (decision 59.1).
            StmtKind::Scoped { name, body } => {
                let mut left = Ok(());
                for statement in body {
                    left = self.exec(statement, env);
                    if left.is_err() {
                        break;
                    }
                }
                // `program.exit` ends no block (rule 6).
                if matches!(left, Err(Control::Fatal(_))) && crate::value::exiting() {
                    return left;
                }
                // Under `zeno test`, the top of the file ends after its
                // tests: they run here, in the innermost `Scoped` of the
                // top, before anything it holds is released.
                let innermost = !matches!(body.last(), Some(Stmt { kind: StmtKind::Scoped { .. }, .. }));
                let at_top = self.top.as_ref().is_some_and(|top| top.tested.is_none() && Rc::ptr_eq(&top.env, env));
                if left.is_ok() && innermost && at_top {
                    self.run_tests_here();
                }
                self.release(name, env, span)?;
                left
            }
            StmtKind::While { condition, body } => {
                while self.condition(condition, env)? {
                    self.tick(span)?;
                    match self.exec_block(body, env) {
                        Ok(()) | Err(Control::Continue(_)) => {}
                        Err(Control::Break(_)) => break,
                        Err(other) => return Err(other),
                    }
                }
                Ok(())
            }
            StmtKind::For { variable, second, iterable, body } => {
                let iterable_span = iterable.span;
                let iterable = self.eval(iterable, env)?;
                // With one name each round is given an item. With two it
                // is given a key, or a position, and what is there.
                let rounds: Vec<(Value, Option<Value>)> = match second {
                    None => ops::items(&iterable, iterable_span)?.into_iter().map(|item| (item, None)).collect(),
                    Some(_) => {
                        ops::pairs(&iterable, iterable_span)?.into_iter().map(|(key, value)| (key, Some(value))).collect()
                    }
                };
                for (item, value) in rounds {
                    self.tick(span)?;
                    // A fresh scope and a fresh loop variable for every round.
                    let scope = Scope::child(env);
                    scope.declare(variable, item, false);
                    if let (Some(second), Some(value)) = (second, value) {
                        scope.declare(second, value, false);
                    }
                    match self.run_statements(body, &scope) {
                        Ok(()) | Err(Control::Continue(_)) => {}
                        Err(Control::Break(_)) => break,
                        Err(other) => return Err(other),
                    }
                }
                Ok(())
            }
            StmtKind::Match { subject, arms } => {
                let value = self.eval(subject, env)?;
                for arm in arms {
                    // The pattern's variables live in the arm's own scope.
                    let scope = Scope::child(env);
                    if self.matches(&arm.pattern, &value, &scope, span)? {
                        return self.run_statements(&arm.body, &scope);
                    }
                }
                fatal(format!("no `case` matches {}", value.repr()), span)
            }
            StmtKind::Try { body, handlers } => {
                let (error, raised_at) = match self.exec_block(body, env) {
                    Err(Control::Raise(error, span)) => (error, span),
                    other => return other,
                };
                let kind = ops::error_kind(&error);
                for handler in handlers {
                    if ops::catches(handler.error.as_deref(), &kind) {
                        let scope = Scope::child(env);
                        if let Some(binding) = &handler.binding {
                            scope.declare(binding, ops::caught(handler.error.as_deref(), &error), false);
                        }
                        return self.run_statements(&handler.body, &scope);
                    }
                }
                Err(Control::Raise(error, raised_at))
            }

            // The tests of a program are run by `zeno test`, after the
            // program (see `run_tests`). `zeno run` goes past them.
            StmtKind::Test { .. } => Ok(()),
            StmtKind::Assert(condition, message) => self.assert(condition, message.as_ref(), env),
            StmtKind::AssertRaises { error, body } => match self.exec_block(body, env) {
                Ok(()) => fatal(format!("this block was expected to raise {error}, and it raised nothing"), span),
                Err(Control::Raise(raised, at)) => {
                    let kind = ops::error_kind(&raised);
                    if ops::catches(Some(error), &kind) {
                        return Ok(());
                    }
                    let mut wrong = Error::new(
                        format!(
                            "this block was expected to raise {error}, and it raised {}: {raised}",
                            crate::ast::shown_name(&kind)
                        ),
                        span,
                    );
                    wrong.note = Some(format!("that error was raised on line {}", at.line_in_file()));
                    Err(Control::Fatal(wrong))
                }
                other => other,
            },

            StmtKind::Return(value) => {
                let value = match value {
                    Some(value) => self.eval(value, env)?,
                    None => Value::None,
                };
                Err(Control::Return(value, span))
            }
            StmtKind::Raise(value) => {
                let error = self.eval(value, env)?;
                Err(ops::raised(error, span))
            }
            StmtKind::Break => Err(Control::Break(span)),
            StmtKind::Continue => Err(Control::Continue(span)),

            StmtKind::Import { module, names } => {
                let path = module.join(".");
                let Some(found) = builtins::module(&path) else {
                    return fatal(
                        format!("there is no module `{path}`. {}", builtins::MODULES),
                        span,
                    );
                };
                if names.is_empty() {
                    // `import math` makes the name `math`.
                    return self.declare(env, found.name, Value::Module(found.clone()), false, span);
                }
                for name in names {
                    let Some(member) = found.members.get(name.as_str()) else {
                        return fatal(format!("module `{path}` has no `{name}`"), span);
                    };
                    self.declare(env, name, member.clone(), false, span)?;
                }
                Ok(())
            }
        }
    }

    /// Work out a condition. It has to be a real bool; see docs/decisions.md.
    fn condition(&mut self, condition: &Expr, env: &Env) -> Result<bool, Control> {
        let value = self.eval(condition, env)?;
        ops::condition(value, condition.span)
    }

    fn assign(&mut self, target: &Expr, value: Value, env: &Env) -> Result<(), Control> {
        let span = target.span;
        match &target.kind {
            ExprKind::Name(name) => match env.assign(name, value) {
                Ok(()) => Ok(()),
                Err(AssignError::NotDeclared) => fatal(
                    format!("`{name}` is not declared. Create it with `let` or `var` first"),
                    span,
                ),
                Err(AssignError::Immutable) => fatal(
                    format!("`{name}` cannot be changed. Declare it with `var` to allow that"),
                    span,
                ),
            },
            ExprKind::Field { object, name, .. } => {
                let object = self.eval(object, env)?;
                // `title.text = "Hello"` is a change to the page.
                match builtins::set_on_page(self, &object, name, &value, span) {
                    Some(done) => done,
                    None => ops::set_field(object, name, value, span),
                }
            }
            ExprKind::Index { object, index } => {
                let object = self.eval(object, env)?;
                let index = self.eval(index, env)?;
                ops::set_item(object, &index, value, span)
            }
            // The parser only lets the three kinds above through.
            _ => fatal("this cannot be assigned to", span),
        }
    }

    /// Does `value` fit `pattern`? Names in the pattern are declared in `scope`.
    fn matches(
        &mut self,
        pattern: &Pattern,
        value: &Value,
        scope: &Env,
        span: Span,
    ) -> Result<bool, Control> {
        match pattern {
            Pattern::Wildcard => Ok(true),
            Pattern::Literal(literal) => {
                let literal = self.eval(literal, scope)?;
                Ok(equal(&literal, value))
            }
            Pattern::Name(name) => {
                // A name that is already a bare enum case, like `Idle`, is
                // compared. Any other name becomes a new variable holding the value.
                if let Some(Value::Case(case)) = scope.get(name) {
                    if case.fields.is_empty() && case.name() == name {
                        return Ok(equal(&Value::Case(case), value));
                    }
                }
                scope.declare(name, value.clone(), false);
                Ok(true)
            }
            Pattern::Variant { name, fields } => {
                let Some(Value::CaseConstructor(def, index)) = scope.get(name) else {
                    return fatal(format!("`{name}` is not an enum case that carries values"), span);
                };
                let Value::Case(case) = value else {
                    return Ok(false);
                };
                if !Rc::ptr_eq(&case.def, &def) || case.index != index {
                    return Ok(false);
                }
                if fields.len() != case.fields.len() {
                    return fatal(
                        format!(
                            "`{name}` carries {}, but this pattern has {}",
                            counted(case.fields.len(), "value"),
                            fields.len()
                        ),
                        span,
                    );
                }
                for (pattern, value) in fields.iter().zip(&case.fields) {
                    if !self.matches(pattern, value, scope, span)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    // ----------------------------------------------------------- Expressions

    fn eval(&mut self, expr: &Expr, env: &Env) -> Result<Value, Control> {
        let span = expr.span;

        match &expr.kind {
            ExprKind::Int(n) => match i64::try_from(*n) {
                Ok(n) => Ok(Value::Int(n)),
                Err(_) => fatal("this number is too big for an int", span),
            },
            ExprKind::Float(x) => Ok(Value::Float(*x)),
            ExprKind::Str(text) => Ok(Value::str(text)),
            // A block of another language: its bridge makes it a
            // library or a module (decision 61).
            ExprKind::Inline { language, text } => crate::inline::run(language, text, span),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::None => Ok(Value::None),

            ExprKind::FormatStr(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        FormatPart::Text(plain) => text += plain,
                        FormatPart::Value { value, format } => {
                            let value = self.eval(value, env)?;
                            text += &builtins::format(&value, format.as_deref(), span)?;
                        }
                    }
                }
                Ok(Value::str(text))
            }

            ExprKind::Name(name) => match env.get(name) {
                Some(value) => Ok(value),
                None => fatal(format!("`{name}` is not defined"), span),
            },
            ExprKind::Give(name) => match env.give(name) {
                Some(value) => Ok(value),
                None => fatal(format!("`{name}` is not defined"), span),
            },

            ExprKind::Unit { value, unit } => {
                let value = self.eval(value, env)?;
                ops::quantity(value, unit, span)
            }

            ExprKind::List(items) => {
                let mut values = Vec::new();
                for item in items {
                    values.push(self.eval(item, env)?);
                }
                Ok(Value::list(values))
            }
            // `return a, b` (decision 57).
            ExprKind::Several(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item, env)?);
                }
                Ok(Value::Several(values.into()))
            }
            // A key written twice keeps its first place and its last value.
            ExprKind::Dict(entries) => {
                let mut dict = Dict::default();
                for (key, value) in entries {
                    let at = key.span;
                    let key = self.eval(key, env)?;
                    let value = self.eval(value, env)?;
                    dict.set(ops::key(&key, at)?, value);
                }
                Ok(Value::dict(dict))
            }
            ExprKind::ListComp { element, variable, iterable, condition } => {
                let source = self.eval(iterable, env)?;
                let mut values = Vec::new();
                for item in ops::items(&source, iterable.span)? {
                    let scope = Scope::child(env);
                    scope.declare(variable, item, false);
                    let keep = match condition {
                        Some(condition) => self.condition(condition, &scope)?,
                        None => true,
                    };
                    if keep {
                        values.push(self.eval(element, &scope)?);
                    }
                }
                Ok(Value::list(values))
            }

            ExprKind::Unary { op, operand } => {
                let value = self.eval(operand, env)?;
                ops::unary(*op, value, span)
            }

            // `and` and `or` only work out their right side when they have to.
            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                let left = self.logic_operand(*op, left, env)?;
                let decided = match op {
                    BinaryOp::And => !left, // false and ... is false
                    _ => left,              // true or ... is true
                };
                if decided {
                    return Ok(Value::Bool(left));
                }
                Ok(Value::Bool(self.logic_operand(*op, right, env)?))
            }
            ExprKind::Binary { op, left, right } => {
                let left = self.eval(left, env)?;
                let right = self.eval(right, env)?;
                ops::binary(*op, left, right, span)
            }
            ExprKind::Compare { first, rest } => {
                // `a < b < c`: compare neighbours, stop at the first false,
                // and work out each operand once.
                let mut left = self.eval(first, env)?;
                for (op, operand) in rest {
                    let right = self.eval(operand, env)?;
                    match ops::binary(*op, left, right.clone(), span)? {
                        Value::Bool(true) => left = right,
                        _ => return Ok(Value::Bool(false)),
                    }
                }
                Ok(Value::Bool(true))
            }

            // A function written as a value remembers the scope it was written in.
            ExprKind::Function(decl) => {
                let closure =
                    Closure { decl: decl.clone(), home: Home::Scope(Scope::seen(env, env.count(), &[])) };
                Ok(Value::Function(Rc::new(closure)))
            }

            ExprKind::Call { callee, args } => {
                let callee = self.eval(callee, env)?;
                let mut positional = Vec::new();
                let mut named = Vec::new();
                for arg in args {
                    let value = self.eval(&arg.value, env)?;
                    match &arg.name {
                        Some(name) => named.push((name.clone(), value)),
                        None if named.is_empty() => positional.push(value),
                        None => {
                            return fatal("plain arguments must come before named ones", arg.value.span)
                        }
                    }
                }
                self.call(callee, positional, named, span)
            }

            ExprKind::Field { object, name, .. } => {
                let object = self.eval(object, env)?;
                // `title.text` is asked of the page.
                match builtins::on_page(self, &object, name) {
                    Some(value) => Ok(value),
                    None => ops::field(object, name, span),
                }
            }
            ExprKind::Index { object, index } => {
                let object = self.eval(object, env)?;
                let index = self.eval(index, env)?;
                ops::item(&object, &index, span)
            }
            // `take slot.task`: read the place, then leave `none` there.
            ExprKind::Take(place) => self.take_out(place, env, span),
        }
    }

    /// `take` in front of a place (decision 56.10). Kept out of `eval`, so
    /// that a rare arm does not grow the frame of the hot one.
    #[inline(never)]
    #[cold]
    fn take_out(&mut self, place: &Expr, env: &Env, span: Span) -> Result<Value, Control> {
        match &place.kind {
            ExprKind::Field { object, name, .. } => {
                let object = self.eval(object, env)?;
                let value = ops::field(object.clone(), name, place.span)?;
                ops::set_field(object, name, Value::None, place.span)?;
                Ok(value)
            }
            ExprKind::Index { object, index } => {
                let object = self.eval(object, env)?;
                let index = self.eval(index, env)?;
                let value = ops::item(&object, &index, place.span)?;
                ops::set_item(object, &index, Value::None, place.span)?;
                Ok(value)
            }
            // The parser lets nothing else through.
            _ => fatal("`take` goes in front of a field or an item", span),
        }
    }

    fn logic_operand(&mut self, op: BinaryOp, operand: &Expr, env: &Env) -> Result<bool, Control> {
        let value = self.eval(operand, env)?;
        ops::logic_operand(op, value, operand.span)
    }


    /// Run the tests of the file being tested, at the end of its top.
    fn run_tests_here(&mut self) {
        let Some(top) = self.top.as_mut() else { return };
        let (env, tests) = (top.env.clone(), std::mem::take(&mut top.tests));
        let mut tested = Vec::new();
        for (name, body) in tests {
            // A test that stopped half way may have left calls open.
            self.depth = 0;
            let failure = self.exec_block(&body, &env).map_err(ops::escaped).err();
            let word = if failure.is_some() { "FAILED" } else { "ok    " };
            self.write_line(&format!("  {word}  {name}"));
            tested.push(Tested { name, failure });
        }
        if let Some(top) = self.top.as_mut() {
            top.tested = Some(tested);
        }
    }

    /// Call `release` on what the resource `name` holds, unless it was
    /// given away (decision 59.1).
    fn release(&mut self, name: &str, env: &Env, span: Span) -> Result<(), Control> {
        let Some(Value::Instance(instance)) = env.give(name) else { return Ok(()) };
        let Some(release) = instance.class.methods.get("release").cloned() else {
            return fatal(format!("`{}` has no `release`", instance.class.name), span);
        };
        self.call_function(&release, vec![Value::Instance(instance)], Vec::new(), span)?;
        Ok(())
    }

    // ----------------------------------------------------------------- Calls

    fn call(
        &mut self,
        callee: Value,
        mut positional: Vec<Value>,
        named: Vec<(String, Value)>,
        span: Span,
    ) -> Result<Value, Control> {
        match callee {
            Value::Function(closure) => self.call_function(&closure, positional, named, span),
            Value::BoundMethod(instance, closure) => {
                // `p.distance_to(q)` passes `p` as the first argument, `self`.
                positional.insert(0, Value::Instance(instance));
                self.call_function(&closure, positional, named, span)
            }
            Value::Builtin(builtin) => {
                ops::no_named(builtin.name, &named, span)?;
                (builtin.call)(self, positional, span)
            }
            Value::Method(receiver, name) => {
                ops::no_named(&name, &named, span)?;
                builtins::call_method(self, &receiver, &name, positional, span)
            }
            Value::Class(class) => {
                let names: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
                let slots = ops::bind(&class.name, &names, positional, named, span)?;
                let scope = scope_of(&class.home);
                let mut fields = Vec::with_capacity(class.fields.len());
                for (field, slot) in class.fields.iter().zip(slots) {
                    let value = match (slot, &field.default) {
                        (Some(value), _) => value,
                        (None, Some(default)) => self.eval(default, &scope)?,
                        (None, None) => {
                            return fatal(
                                format!("`{}` needs a value for `{}`", class.name, field.name),
                                span,
                            )
                        }
                    };
                    fields.push(value);
                }
                Ok(Value::Instance(Rc::new(Instance { class, fields: RefCell::new(fields) })))
            }
            Value::CaseConstructor(def, index) => ops::new_case(&def, index, positional, named, span),
            Value::ErrorType(kind) => ops::new_error(&kind, positional, named, span),
            // A function, a class or a method of Python: called there,
            // and a named argument is one of its keyword arguments.
            Value::Foreign(foreign) => crate::bridge::call(&foreign, &positional, &named, span),
            other => fatal(format!("{} cannot be called", other.type_name()), span),
        }
    }

    fn call_function(
        &mut self,
        closure: &Closure,
        positional: Vec<Value>,
        named: Vec<(String, Value)>,
        span: Span,
    ) -> Result<Value, Control> {
        let decl = &closure.decl;
        let name = if decl.name.is_empty() { "function" } else { &decl.name };

        if self.depth >= MAX_DEPTH {
            return fatal(
                format!("more than {MAX_DEPTH} calls are nested. Is `{name}` calling itself forever?"),
                span,
            );
        }

        // The body runs in a new scope inside the scope where the function
        // was *defined*, not the one it was called from.
        let scope = Scope::child(&scope_of(&closure.home));
        let names: Vec<&str> = decl.params.iter().map(|p| p.name.as_str()).collect();
        let slots = ops::bind(name, &names, positional, named, span)?;
        for (param, slot) in decl.params.iter().zip(slots) {
            let value = match (slot, &param.default) {
                (Some(value), _) => value,
                // Defaults are worked out fresh on every call.
                (None, Some(default)) => self.eval(default, &scope)?,
                (None, None) => {
                    return fatal(format!("`{name}` needs a value for `{}`", param.name), span)
                }
            };
            self.declare(&scope, &param.name, value, false, span)?;
        }

        self.tick(span)?;
        self.depth += 1;
        let result = self.run_statements(&decl.body, &scope);
        self.depth -= 1;

        match result {
            Ok(()) => Ok(Value::None), // fell off the end without a `return`
            Err(Control::Return(value, _)) => Ok(value),
            Err(Control::Break(span)) => fatal("`break` outside a loop", span),
            Err(Control::Continue(span)) => fatal("`continue` outside a loop", span),
            Err(other) => Err(other),
        }
    }

}


#[cfg(test)]
mod tests {
    use crate::{lexer, parser};

    /// Run a program and return what it printed and how it ended.
    fn run(source: &str) -> (String, Result<(), String>) {
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
        // Both engines run it, and must agree.
        let (output, result) = crate::vm::run_both(&program, source);
        (output, result.map_err(|error| error.message))
    }

    /// What a working program prints. A program that runs correctly must
    /// also get through the type checker, so that is tested here too.
    fn output(source: &str) -> String {
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let complaints = crate::checker::check(&program, false);
        assert!(complaints.is_empty(), "the checker rejected a working program: {complaints:?}\n{source}");

        let (output, result) = run(source);
        if let Err(message) = result {
            panic!("program failed: {message}\noutput so far:\n{output}");
        }
        output
    }

    /// The first thing the checker says about a program.
    fn check_error(source: &str) -> String {
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let complaints = crate::checker::check(&program, false);
        complaints.first().map(|complaint| complaint.message.clone()).unwrap_or_default()
    }

    /// The message a failing program stops with.
    fn error(source: &str) -> String {
        run(source).1.expect_err("the program should have failed")
    }

    // ----- Values and operators -----

    #[test]
    fn arithmetic() {
        assert_eq!(output("print(1 + 2 * 3, 10 - 4, 2 ** 10, -3 + 1)"), "7 6 1024 -2\n");
        assert_eq!(output("print(1.5 + 1, 2 * 0.5, 2.0 ** -1, 2.0 ** 3)"), "2.5 1.0 0.5 8.0\n");
        assert_eq!(
            error("print(2 ** -1)"),
            "uncaught ValueError: a whole number cannot be raised to a negative power. Use a float, as in `2.0 ** -1`"
        );
    }

    #[test]
    fn division_always_gives_a_float_and_floor_division_rounds_down() {
        assert_eq!(output("print(7 / 2, 6 / 2, 7 // 2, -7 // 2, 7.5 // 2)"), "3.5 3.0 3 -4 3.0\n");
        assert_eq!(output("print(7 % 3, -7 % 3, 7 % -3, 7.5 % 2)"), "1 2 -2 1.5\n");
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(output("print(1 < 2, 2 <= 2, 3 > 4, \"a\" < \"b\", 1 == 1.0, 1 != 2)"), "true true false true true true\n");
        assert_eq!(output("print([1, 2] == [1, 2], [1] == [2])"), "true false\n");
        // The checker refuses to compare an int with a str, but a value of
        // unknown type can still meet one when the program runs.
        assert_eq!(output("function same(a, b) { return a == b }\nprint(same(1, \"1\"), same(1, 1.0))"), "false true\n");
    }

    #[test]
    fn chained_comparisons() {
        assert_eq!(output("print(1 < 2 < 3, 1 < 5 < 3, 3 > 2 > 1 == 1)"), "true false true\n");
        // The middle operand runs once, and the chain stops at the first false.
        let source = "function mid() -> int {\n print(\"mid\")\n return 5\n}\nfunction last() -> int {\n print(\"last\")\n return 9\n}\nprint(1 < mid() < last())\nprint(9 < mid() < last())";
        assert_eq!(output(source), "mid\nlast\ntrue\nmid\nfalse\n");
    }

    #[test]
    fn logic_short_circuits() {
        let source = "function loud(x: bool) -> bool {\n print(\"called\")\n return x\n}\nprint(false and loud(true))\nprint(true or loud(true))\nprint(true and loud(false))\nprint(not true)";
        assert_eq!(output(source), "false\ntrue\ncalled\nfalse\nfalse\n");
    }

    #[test]
    fn strings() {
        assert_eq!(output("print(\"ab\" + \"cd\", len(\"héllo\"), \"hello\"[1], \"ell\" in \"hello\")"), "abcd 5 e true\n");
        assert_eq!(output("print(\" Hi \".strip().upper(), \"a,b\".split(\",\"), \"x\".replace(\"x\", \"y\"))"), "HI [\"a\", \"b\"] y\n");
        assert_eq!(output("print(\"zeno\".starts_with(\"ze\"), \"zeno\".ends_with(\"x\"))"), "true false\n");
    }

    #[test]
    fn text_and_list_methods_of_the_library() {
        // Positions count characters, not bytes.
        assert_eq!(
            output("let t = \"héllo wörld\"\nprint(t.find(\"w\"), t.find(\"z\"), t.slice(6), t.slice(1, 3), t.slice(11) == \"\")"),
            "6 none wörld él true\n"
        );
        assert_eq!(output("print(\"A\".code(), \"é\".code(), char(97), char(233), char(128512))"), "65 233 a é 😀\n");
        assert_eq!(
            output("let xs = [3, 1, 2]\nxs.insert(1, 9)\nprint(xs)\nprint(xs.pop(0), xs)\nxs.sort()\nprint(xs)\nxs.reverse()\nprint(xs, xs.find(2), xs.find(7), xs.slice(1), xs.slice(0, 1))\nxs.clear()\nprint(xs)"),
            "[3, 9, 1, 2]\n3 [9, 1, 2]\n[1, 2, 9]\n[9, 2, 1] 1 none [2, 1] [9]\n[]\n"
        );
        assert_eq!(output("print([\"b\", \"a\"].join(\", \"), [].join(\"-\") == \"\", [\"x\"].join(\"-\"))"), "b, a true x\n");
        // Ints and floats sort together, as `<` compares them.
        // With a function: by what it gives, and the same stay as they were.
        assert_eq!(
            output("let words = [\"pear\", \"fig\", \"kiwi\", \"apple\", \"plum\"]\nwords.sort(len)\nprint(words)\nlet xs = [3, -7, 1]\nxs.sort(abs)\nprint(xs)"),
            "[\"fig\", \"pear\", \"kiwi\", \"plum\", \"apple\"]\n[1, 3, -7]\n"
        );
        assert_eq!(
            error("let xs = [2, 1]\nxs.sort(function(n) { return [n] })"),
            "the function given to `sort` must return a number or text to order by, but it returned a list"
        );
        // A function given to `map` may change the list it is walking.
        assert_eq!(
            output("var xs = [1, 2]\nprint(xs.map(function(n: int) -> int {\n xs.append(n)\n return n * 2\n}), xs)"),
            "[2, 4] [1, 2, 1, 2]\n"
        );
        assert_eq!(output("function order(xs) {\n xs.sort()\n return xs\n}\nprint(order([2, 1.5, -3]), order([\"b\", \"B\", \"a\"]))"), "[-3, 1.5, 2] [\"B\", \"a\", \"b\"]\n");
        assert_eq!(error("print(\"abc\".slice(2, 9))"), "uncaught IndexError: slice 2 to 9 is out of range for a str of length 3");
        assert_eq!(error("print(\"abc\".slice(2, 1))"), "uncaught IndexError: slice 2 to 1 is out of range for a str of length 3");
        assert_eq!(error("print([1].slice(-1))"), "uncaught IndexError: slice -1 to 1 is out of range for a list of length 1");
        assert_eq!(error("let xs = [1]\nxs.insert(2, 0)"), "uncaught IndexError: cannot insert at 2 in a list of length 1");
        assert_eq!(error("print([1].pop(1))"), "uncaught IndexError: index 1 is out of range for a list of length 1");
        assert_eq!(error("print(\"ab\".code())"), "uncaught ValueError: `code` needs a str of exactly one character, and this one has 2");
        assert_eq!(error("print(char(0))"), "uncaught ValueError: 0 is not the code of a character");
        assert_eq!(error("print(char(55296))"), "uncaught ValueError: 55296 is not the code of a character");
    }

    // ----- Reading and writing -----

    /// A place for a test's file that no other test uses.
    fn scratch(name: &str) -> String {
        crate::modules::shown(&std::env::temp_dir().join(format!("zeno-test-{}-{name}", std::process::id())))
    }

    #[test]
    fn files_are_written_read_and_removed() {
        let path = scratch("files.txt");
        let source = format!(
            "import files\nlet path = \"{path}\"\n\
             print(files.exists(path))\n\
             files.write(path, \"one\\ntwo\\n\")\n\
             print(files.exists(path), len(files.read(path)))\n\
             files.append(path, \"three\")\n\
             print(files.read(path).lines())\n\
             files.write(path, \"again\")\n\
             print(files.read(path))\n\
             files.remove(path)\n\
             print(files.exists(path))\n"
        );
        assert_eq!(output(&source), "false\ntrue 8\n[\"one\", \"two\", \"three\"]\nagain\nfalse\n");
        // `from files import ...` gives the same functions.
        let source = format!(
            "from files import write, read, remove\nwrite(\"{path}\", \"x\")\nprint(read(\"{path}\"))\nremove(\"{path}\")"
        );
        assert_eq!(output(&source), "x\n");
    }

    #[test]
    fn a_list_of_a_fixed_length_keeps_its_length() {
        let buffer = "var buffer: list[u8, 8] = [0] * 8\n";
        assert_eq!(output(&format!("{buffer}buffer[7] = 255\nprint(buffer, len(buffer))")), "[0, 0, 0, 0, 0, 0, 0, 255] 8\n");
        let grows = "`append` would change how many items this list has, and a list of a fixed length always has 8. \
                     Give an item a new value instead, as in `list[0] = 7`";
        assert_eq!(check_error(&format!("{buffer}buffer.append(1)")), grows);
        assert_eq!(
            check_error(&format!("{buffer}print(buffer[8])")),
            "8 is outside a list of 8 items, whose positions are 0 to 7"
        );
        assert_eq!(check_error("let b: list[u8, 8] = [1, 2]"), "this list has 2 items, and a list[u8, 8] always has 8");
        assert_eq!(check_error("let b: list[u8, 8] = [0] * 7"), "this list has 7 items, and a list[u8, 8] always has 8");
        assert_eq!(
            check_error("let n = 8\nlet b: list[u8, 8] = [0] * n"),
            "`b` should be list[u8, 8], but this is list[u8]. A list[u8, 8] always has 8 items, and how many this one \
             has is only known when the program runs. Write the 8 items, or `[0] * 8`"
        );
        assert_eq!(
            check_error(&format!("{buffer}let grow: list[u8] = buffer")),
            "`grow` should be list[u8], but this is list[u8, 8]. This one always has 8 items, and a list[u8] may be \
             made longer. Write `list[u8, 8]` there, or copy the items into a list of its own with `.slice(0)`"
        );
        assert_eq!(check_error("let b: list[u8, 0] = []"), "a list of a fixed length has at least one item: `list[u8, 1]` or more");
        // A function that never changes the length can be given one.
        let functions = "function total(data: list[u8]) -> int {\n    var sum = 0\n    for b in data {\n        sum = sum + b\n    }\n    return sum\n}\n\
                         function twice(data: list[u8]) -> int {\n    return total(data) * 2\n}\n\
                         function pad(data: list[u8]) {\n    data.append(0)\n}\n\
                         function keep(data: list[u8]) -> list[u8] {\n    return data\n}\n";
        assert_eq!(output(&format!("{functions}{buffer}buffer[0] = 3\nprint(total(buffer), twice(buffer))")), "3 6\n");
        assert_eq!(
            check_error(&format!("{functions}{buffer}pad(buffer)")),
            "this list always has 8 items, and `pad` may change how many its `data` has: it calls `data.append` (line 12). \
             A function that only reads a list, or gives its items new values, can be given one of a fixed length"
        );
        assert_eq!(
            check_error(&format!("{functions}{buffer}keep(buffer)")),
            "this list always has 8 items, and `keep` may change how many its `data` has: it returns `data` (line 15). \
             A function that only reads a list, or gives its items new values, can be given one of a fixed length"
        );
    }

    #[test]
    fn a_list_is_repeated_with_a_star() {
        assert_eq!(
            output("print([0] * 4, [1, 2] * 2, [\"a\"] * 0)\nlet b: list[u8] = [255] * 3\nprint(b)\nlet n = 2\nprint([true] * n)"),
            "[0, 0, 0, 0] [1, 2, 1, 2] []\n[255, 255, 255]\n[true, true]\n"
        );
        assert_eq!(
            error("let n = -1\nprint([0] * n)"),
            "uncaught ValueError: `*` repeats a list 0 or more times, and this is -1"
        );
        assert_eq!(check_error("let b: list[u8] = [256] * 2"), "256 does not fit in a u8 (0 to 255)");
        assert_eq!(check_error("let x = 3 * [0]"), "`*` repeats a list when the list comes first: write `[0] * 64`");
        assert!(check_error("let grid = [[0] * 3] * 3").starts_with("`*` would put the same list[int] in the list again"));
    }

    #[test]
    fn a_file_is_read_and_written_as_bytes() {
        let path = scratch("bytes.bin");
        let source = format!(
            "import files\nlet path = \"{path}\"\n\
             files.write_bytes(path, [0, 1, 127, 128, 255])\n\
             print(files.read_bytes(path))\n\
             files.write(path, \"é\")\n\
             print(files.read_bytes(path))\n\
             let every: list[u8] = [u8(n) for n in range(256)]\n\
             files.write_bytes(path, every)\n\
             print(files.read_bytes(path) == every)\n\
             files.remove(path)\n"
        );
        assert_eq!(output(&source), "[0, 1, 127, 128, 255]\n[195, 169]\ntrue\n");
        assert_eq!(
            error(&format!("import files\nprint(files.read_bytes(\"{path}\"))")),
            format!("uncaught FileError: there is no file `{path}`")
        );
        // A list that says what it holds only when the program runs is
        // looked at item by item.
        assert_eq!(
            error(&format!("import files\nimport json\nfiles.write_bytes(\"{path}\", json.parse(\"[1, 300]\"))")),
            "uncaught ValueError: 300 is not a byte: a byte is a number from 0 to 255"
        );
        // A byte is a u8: a number written out must fit, and a list of
        // ints is not a list of bytes.
        assert_eq!(
            check_error("import files\nfiles.write_bytes(\"x\", [1, 256])"),
            "256 does not fit in a u8 (0 to 255)"
        );
        assert_eq!(
            check_error("import files\nlet xs = [1, 2]\nfiles.write_bytes(\"x\", xs)"),
            "the bytes given to `files.write_bytes` should be list[u8], but this is list[int]"
        );
    }

    /// `files.list` and `files.make_folder` (decisions 59.20 and 59.21).
    #[test]
    fn a_folder_is_made_and_listed() {
        let folder = scratch("folder");
        let _ = std::fs::remove_dir_all(&folder);
        let source = format!(
            "import files\nlet top = \"{folder}\"\n\
             files.make_folder(top + \"/deep/er\")\n\
             files.make_folder(top + \"/deep/er\")\n\
             files.make_folder(top)\n\
             print(files.list(top + \"/deep\"), files.list(top + \"/deep/er\"))\n\
             for name in [\"b\", \"C\", \"a b\", \"10\", \"9\", \"é\", \"ｚ\", \"😀\"] {{\n    files.write(top + \"/\" + name, name)\n}}\n\
             let names = files.list(top)\n\
             print(names)\n\
             names.append(\"x\")\n\
             print(len(files.list(top)))\n\
             try {{\n    print(files.list(top + \"/b\"))\n}} except FileError as e {{\n    print(e.message == \"cannot list the folder `\" + top + \"/b`\")\n}}\n\
             try {{\n    files.make_folder(top + \"/b/c\")\n}} except FileError as e {{\n    print(e.message == \"cannot make the folder `\" + top + \"/b/c`\")\n}}\n"
        );
        assert_eq!(
            output(&source),
            "[\"er\"] []\n[\"10\", \"9\", \"C\", \"a b\", \"b\", \"deep\", \"é\", \"ｚ\", \"😀\"]\n9\ntrue\ntrue\n"
        );
        assert_eq!(
            error(&format!("import files\nprint(files.list(\"{folder}/none\"))")),
            format!("uncaught FileError: there is no folder `{folder}/none`")
        );
        assert_eq!(error("import files\nprint(files.list(\"\"))"), "uncaught FileError: there is no folder ``");
        assert_eq!(error("import files\nfiles.make_folder(\"\")"), "uncaught FileError: cannot make the folder ``");
        assert_eq!(
            error(&format!("import files\nfiles.make_folder(\"{folder}/b\")")),
            format!("uncaught FileError: cannot make the folder `{folder}/b`")
        );
        // A name that is not UTF-8 is no text, and is said to be so,
        // rather than changed into some other name.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = std::path::Path::new(&folder).join(std::ffi::OsStr::from_bytes(b"bad\xff"));
            // A disk that keeps only names that are UTF-8, as macOS's
            // does, refuses to make one, and there is nothing to list.
            if std::fs::write(bad, "").is_ok() {
                assert_eq!(
                    error(&format!("import files\nprint(files.list(\"{folder}\"))")),
                    format!("uncaught FileError: the folder `{folder}` holds a name that is not text")
                );
            }
        }
        std::fs::remove_dir_all(&folder).unwrap();
        // The checker knows them both.
        assert_eq!(check_error("import files\nprint(files.list())"), "`list` takes 1 argument, but 0 were given");
        assert_eq!(
            check_error("import files\nfiles.make_folder(3)"),
            "the path given to `files.make_folder` should be str, but this is int"
        );
        assert_eq!(
            check_error("import files\nfiles.list([\"a\"])"),
            "the folder given to `files.list` should be str, but this is list[str]"
        );
        assert_eq!(check_error("import files\nlet n: int = files.list(\"x\")"), "`n` should be int, but this is list[str]");
        assert_eq!(
            check_error("strict\nimport files\nfunction names() -> list[str] {\n    return files.list(\"x\")\n}\nprint(names())\n"),
            "`names` can raise FileError here but does not say so. Add `raises FileError` to it, or catch the error"
        );
        assert_eq!(
            check_error("strict\nimport files\nfunction make() {\n    files.make_folder(\"x\")\n}\nmake()\n"),
            "`make` can raise FileError here but does not say so. Add `raises FileError` to it, or catch the error"
        );
    }

    #[test]
    fn a_file_that_cannot_be_used_is_a_file_error() {
        let path = scratch("missing.txt");
        let folder = crate::modules::shown(&std::env::temp_dir());
        assert_eq!(
            error(&format!("import files\nprint(files.read(\"{path}\"))")),
            format!("uncaught FileError: there is no file `{path}`")
        );
        assert_eq!(
            error(&format!("import files\nfiles.remove(\"{path}\")")),
            format!("uncaught FileError: there is no file `{path}`")
        );
        assert_eq!(
            error(&format!("import files\nfiles.write(\"{path}/x/y.txt\", \"\")")),
            format!("uncaught FileError: cannot write `{path}/x/y.txt`")
        );
        // A folder is not a file: it is there, and cannot be read.
        assert_eq!(
            output(&format!(
                "import files\nprint(files.exists(\"{folder}\"))\n\
                 try {{ print(files.read(\"{folder}\")) }} except FileError as error {{ print(error.message) }}"
            )),
            format!("false\ncannot read `{folder}`\n")
        );
        // A file that is not text: bytes that are not UTF-8, or a zero.
        for (name, bytes) in [("not-utf8.bin", &[b'a', 0xFF][..]), ("zero.bin", &[b'a', 0, b'b'][..])] {
            let path = scratch(name);
            std::fs::write(&path, bytes).expect("the file is written");
            let source = format!(
                "import files\ntry {{ print(files.read(\"{path}\")) }} except FileError as error {{ print(error.message) }}"
            );
            assert_eq!(output(&source), format!("`{path}` does not hold text\n"));
            std::fs::remove_file(&path).expect("the file is removed");
        }
        assert_eq!(error("import files\nfiles.read(5)"), "`read` needs a str, but this is an int");
    }

    #[test]
    fn text_in_quotes_is_shown_by_zenos_own_rule() {
        // Every character as it is, whatever script it is in (24.20).
        assert_eq!(output("print([\"தமிழ்\", \"e\" + char(769), char(160), char(8232), \"😀\"])"), "[\"தமிழ்\", \"e\u{301}\", \"\u{a0}\", \"\u{2028}\", \"😀\"]\n");
        // But for the five a program writes with `\\`, and the control characters.
        assert_eq!(
            output("print([\"q\\\"b\\\\n\\nr\\rt\\t\", char(1) + char(27) + char(127) + char(133) + char(159)])"),
            "[\"q\\\"b\\\\n\\nr\\rt\\t\", \"\\u{1}\\u{1b}\\u{7f}\\u{85}\\u{9f}\"]\n"
        );
        assert_eq!(error("int(\"நூறு\")"), "uncaught ValueError: cannot turn \"நூறு\" into an int");
    }

    #[test]
    fn a_float_between_two_shortest_texts_takes_the_even_one() {
        // 1e15 + 0.25 is exactly between ...0.2 and ...0.3 (24.21).
        assert_eq!(
            output("print(1e15 + 0.25, 1e15 + 0.75, 8388608.0 + 1.0 / 1024.0, 70368744177664.0 + 0.125, -(1e15 + 0.25))"),
            "1000000000000000.2 1000000000000000.8 8388608.000976562 70368744177664.12 -1000000000000000.2\n"
        );
        // Every other float is as it was.
        assert_eq!(output("print(0.1 + 0.2, 1e16, 1.5e-7, 5e-324, 1.7976931348623157e308, 123456789012345680.0)"), "0.30000000000000004 1e16 1.5e-7 5e-324 1.7976931348623157e308 1.2345678901234568e17\n");
    }

    #[test]
    fn a_range_ends_at_the_last_int() {
        assert_eq!(
            output("print(range(9223372036854775806, 9223372036854775807, 5), range(-9223372036854775807, -9223372036854775807 - 1, -5))"),
            "[9223372036854775806] [-9223372036854775807]\n"
        );
    }

    #[test]
    fn the_hex_of_a_json_escape_is_four_digits() {
        // A sign is not a digit, the low half of a pair has its own
        // range, and the blanks of JSON are four: a form feed is not one.
        let source = r#"import json
let bs = "\\"
for text in ["\"" + bs + "u+041\"", "\"" + bs + "ud83d" + bs + "ue000\"", char(12) + "1"] {
    try {
        print(json.parse(text))
    } except ValueError as e {
        print(e.message)
    }
}
print(json.parse("\"" + bs + "u0041" + bs + "ud83d" + bs + "ude00\""))"#;
        assert_eq!(
            output(source),
            "this text is not JSON: `\"\\u+041\"`\nthis text is not JSON: `\"\\ud83d\\ue000\"`\nthis text is not JSON: `\u{c}1`\nA😀\n"
        );
    }

    #[test]
    fn the_lines_of_a_text() {
        assert_eq!(
            output(
                "print(\"\".lines(), \"one\".lines(), \"one\\n\".lines(), \"one\\ntwo\".lines())\n\
                 print(\"one\\r\\ntwo\\r\\n\".lines(), \"\\n\".lines(), \"a\\n\\nb\".lines())\n\
                 print(len(\"a\\r\".lines()[0]), len(\"a\\rb\\n\".lines()[0]))"
            ),
            "[] [\"one\"] [\"one\"] [\"one\", \"two\"]\n[\"one\", \"two\"] [\"\"] [\"a\", \"\", \"b\"]\n2 3\n"
        );
    }

    #[test]
    fn input_reads_a_line_or_none() {
        use crate::value::testing::with_outside;
        let source = "let name = input(\"Name: \")\n\
                      if name is none { print(\"nobody\") } else { print(f\"Hello, {name}!\") }\n\
                      var count = 0\n\
                      while true {\n\
                      \x20   let line = input()\n\
                      \x20   if line is none { break }\n\
                      \x20   count += 1\n\
                      \x20   print(count, len(line), line)\n\
                      }\n\
                      print(input() is none)";
        with_outside("Partha\none\r\n\nlast, with no end", &[], || {
            assert_eq!(output(source), "Name: Hello, Partha!\n1 3 one\n2 0 \n3 17 last, with no end\ntrue\n");
        });
        // Nothing typed at all: the first `input` is `none` already.
        assert_eq!(output(source), "Name: nobody\ntrue\n");
        assert_eq!(error("input(5)"), "`input` needs a str, but this is an int");
    }

    #[test]
    fn a_dict_keeps_one_value_under_each_key() {
        // Written out, read, stored into. A key that is there keeps its
        // place; a new one goes at the end.
        assert_eq!(
            output("let ages = [\"Ada\": 36, \"Alan\": 41]\nages[\"Grace\"] = 45\nages[\"Ada\"] = 37\nprint(ages, len(ages), ages[\"Alan\"])"),
            "[\"Ada\": 37, \"Alan\": 41, \"Grace\": 45] 3 41\n"
        );
        assert_eq!(output("let empty: dict[int, str] = [:]\nprint(empty, len(empty))\nempty[2] = \"two\"\nprint(empty)"), "[:] 0\n[2: \"two\"]\n");
        // `in` asks about keys, and a loop is given them in order.
        assert_eq!(
            output("let d = [\"b\": 1, \"a\": 2]\nprint(\"a\" in d, \"c\" in d, \"c\" not in d)\nfor k in d {\n print(k, d[k])\n}"),
            "true false true\nb 1\na 2\n"
        );
        // The loop walks the keys as they were when it began.
        assert_eq!(output("let d = [1: 1]\nfor k in d {\n d[k + 1] = 0\n d[k + 2] = 0\n}\nprint(d)"), "[1: 1, 2: 0, 3: 0]\n");
        // With two names it is given each key and what is under it, also
        // as they were when it began.
        assert_eq!(
            output("let d = [\"b\": 1, \"a\": 2]\nfor k, v in d {\n d[k] = v * 10\n d[k + k] = 0\n print(k, v)\n}\nprint(d)"),
            "b 1\na 2\n[\"b\": 10, \"a\": 20, \"bb\": 0, \"aa\": 0]\n"
        );
        // Over a list, text and a range the first name is the position,
        // from 0, and the second is what is there.
        assert_eq!(
            output(
                "for i, word in [\"zero\", \"one\"] {\n print(i, word)\n}\nfor i, c in \"h\u{e9}y\" {\n print(i, c)\n}\n\
                 for i, n in range(10, 0, -4) {\n if i == 2 {\n  break\n }\n print(i, n)\n}"
            ),
            "0 zero\n1 one\n0 h\n1 \u{e9}\n2 y\n0 10\n1 6\n"
        );
        // A value is anything a list can hold, and it is the thing
        // itself that is under the key, not a copy.
        assert_eq!(
            output(
                "class Point {\n x: int\n}\nlet lists = [\"a\": [1]]\nlists[\"a\"].append(2)\nlists[\"b\"] = []\n\
                 let points = [1: Point(5)]\npoints[1].x = 6\nlet nested = [\"in\": [\"x\": 1]]\nnested[\"in\"][\"y\"] = 2\n\
                 print(lists, points, nested, lists.values(), points.get(2))\nprint(points.pop(1), points)"
            ),
            "[\"a\": [1, 2], \"b\": []] [1: Point(x=6)] [\"in\": [\"x\": 1, \"y\": 2]] [[1, 2], []] none\nPoint(x=6) [:]\n"
        );
        // `get`: `none`, or what the second argument says, for a key
        // that is not there.
        assert_eq!(
            output("let d = [\"a\": 1.5]\nprint(d.get(\"a\"), d.get(\"z\"), d.get(\"z\", 0.0), d.get(\"a\", 0.0))"),
            "1.5 none 0.0 1.5\n"
        );
        assert_eq!(
            output("let d = [1: true, 2: false, 3: true]\nprint(d.pop(2), d, d.keys(), d.values())\nd.clear()\nprint(d, len(d))"),
            "false [1: true, 3: true] [1, 3] [true, true]\n[:] 0\n"
        );
        // Equal when the same keys have the same values, in any order.
        assert_eq!(
            output("let a = [\"x\": 1, \"y\": 2]\nlet b = [\"y\": 2, \"x\": 1]\nprint(a == b, a != b, a == [\"x\": 1], a == [\"x\": 1, \"y\": 3])"),
            "true false false false\n"
        );
        // A dict is shared, like a list: two names for one dict.
        assert_eq!(output("let a = [\"x\": 1]\nlet b = a\nb[\"y\"] = 2\nprint(a)"), "[\"x\": 1, \"y\": 2]\n");
        assert_eq!(output("print(str([\"k\": \"v\"]), f\"{[1: 2]}\")"), "[\"k\": \"v\"] [1: 2]\n");
        // A key that is not there is a KeyError, which can be caught.
        assert_eq!(error("let d = [\"a\": 1]\nprint(d[\"b\"])"), "uncaught KeyError: this dict has no key \"b\"");
        assert_eq!(error("let d = [1: 2]\nd.pop(5)"), "uncaught KeyError: this dict has no key 5");
        assert_eq!(
            output("let d = [1: 2]\ntry {\n print(d[3])\n} except KeyError as e {\n print(e.kind, e.message)\n}"),
            "KeyError this dict has no key 3\n"
        );
    }

    #[test]
    fn more_of_math() {
        // Angles in, as a plain number (radians) or with a unit.
        assert_eq!(
            output("import math\nprint(math.sin(90 deg), math.cos(180 deg), math.sin(0), math.tan(0 rad), math.cos(0.0 rad))"),
            "1.0 -1.0 0.0 0.0 1.0\n"
        );
        // Angles out, in radians, which `.deg` changes.
        assert_eq!(
            output(
                "import math\nfrom math import atan2\n\
                 print(math.asin(1).deg, math.acos(1), math.atan(1).deg, math.acos(-1) / 1 rad == math.pi)\n\
                 print(atan2(1, 1).deg, atan2(1, -1).deg, atan2(-1, -1).deg, atan2(-1, 1).deg, atan2(0, -1).deg)\n\
                 print((180 deg).rad / 1 rad == math.pi, (math.pi * 1 rad).deg, (90 deg).rad == math.asin(1))"
            ),
            "90 deg 0 rad 45 deg true\n45 deg 135 deg -135 deg -45 deg 180 deg\ntrue 180 deg true\n"
        );
        assert_eq!(
            output(
                "import math\nprint(math.exp(0), math.log(1), math.log(math.e), math.log2(8), math.log2(0.5), math.e > 2.718)\n\
                 print(math.round(2.5), math.round(-2.5), math.round(2.4), math.round(-2.6), math.round(0.5), math.round(7))"
            ),
            "1.0 0.0 1.0 3.0 -1.0 true\n3 -3 2 -3 1 7\n"
        );
        assert_eq!(error("import math\nprint(math.log(0))"), "uncaught ValueError: cannot take the logarithm of 0.0");
        assert_eq!(error("import math\nprint(math.log2(-8))"), "uncaught ValueError: cannot take the logarithm of -8.0");
        assert_eq!(
            error("import math\nprint(math.asin(1.5))"),
            "uncaught ValueError: `asin` needs a number from -1 to 1, and this is 1.5"
        );
        assert_eq!(
            error("import math\nprint(math.acos(-2))"),
            "uncaught ValueError: `acos` needs a number from -1 to 1, and this is -2.0"
        );
        // What the checker would have caught, met while running.
        assert_eq!(
            error("import math\nprint(math.sin(3 s))"),
            "`sin` needs a number or an angle, such as `30 deg`, not a quantity in s"
        );
        assert_eq!(error("import math\nprint(math.atan2(1, \"x\"))"), "`atan2` needs two numbers, but this is a str");
        assert_eq!(
            error("import math\nprint(math.atan(1) + 90 deg)"),
            "`+` does not work between a quantity in rad and a quantity in deg"
        );
    }

    #[test]
    fn the_arguments_of_the_program() {
        use crate::value::testing::with_outside;
        let source = "import program\nlet words = program.arguments()\nprint(len(words), words)\n\
                      words.append(\"mine\")\nprint(len(program.arguments()))";
        with_outside("", &["a.txt", "two words", "--strict"], || {
            assert_eq!(output(source), "3 [\"a.txt\", \"two words\", \"--strict\"]\n3\n");
        });
        assert_eq!(output(source), "0 []\n0\n");
    }

    /// Run a program in both engines, for everything a run gives.
    fn ending(source: &str) -> crate::value::Ran {
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let complaints = crate::checker::check(&program, false);
        assert!(complaints.is_empty(), "the checker rejected a working program: {complaints:?}\n{source}");
        crate::vm::run_both_ending(&program, source)
    }

    #[test]
    fn a_program_can_end_with_a_number_of_its_choosing() {
        let ran = ending("import program\nprint(\"one\")\nprogram.exit(7)\nprint(\"two\")");
        assert_eq!((ran.output.as_str(), ran.status, ran.result), ("one\n", 7, Ok(())));
        // From inside calls, a loop, a `try` and a function that `map`
        // calls: no `except` sees it, not even one that names no kind.
        let ran = ending(
            "import program\n\
             function look(n: int) -> int {\n    if n == 3 {\n        try { program.exit(0) } except { print(\"caught\") }\n    }\n    return n\n}\n\
             for i in range(2) {\n    try {\n        print([1, 2, 3, 4].map(look))\n    } except ValueError {\n        print(\"no\")\n    }\n}\n\
             print(\"never\")",
        );
        assert_eq!((ran.output.as_str(), ran.status, ran.result), ("", 0, Ok(())));
        // A number no program can end with is a ValueError, which can be
        // caught, as `char(0)` is.
        assert_eq!(
            error("import program\nlet code = 256\nprogram.exit(code)"),
            "uncaught ValueError: a program ends with a number from 0 to 255, not 256"
        );
        assert_eq!(
            output("import program\nlet codes = [-1, 300]\nfor code in codes {\n    try { program.exit(code) } except ValueError as e { print(e) }\n}"),
            "a program ends with a number from 0 to 255, not -1\na program ends with a number from 0 to 255, not 300\n"
        );
        // A number written down that no program can end with is found
        // before the program runs.
        let program = parser::parse(lexer::lex("import program\nprogram.exit(-1)").unwrap()).unwrap();
        let complaints = crate::checker::check(&program, false);
        assert_eq!(complaints.first().map(|error| error.message.as_str()), Some("a program ends with a number from 0 to 255, not -1"));
    }

    #[test]
    fn a_warning_goes_on_the_error_stream() {
        let ran = ending("import program\nprint(\"out\")\nprogram.warn(\"careful\")\nprogram.warn(\"\")\nprint(\"out again\")");
        assert_eq!(ran.output, "out\nout again\n");
        assert_eq!(ran.warned, "careful\n\n");
        assert_eq!(ran.status, 0);
        // An error ends the program with 1, as it always did.
        let ran = ending("import program\nprogram.warn(\"about to fail\")\nprint([1][3])");
        assert_eq!((ran.warned.as_str(), ran.status), ("about to fail\n", 1));
    }

    #[test]
    fn the_environment_of_the_program() {
        use crate::value::testing::with_environment;
        let source = "import program\n\
                      let home = program.environment(\"ZENO_HOME\")\n\
                      if home is none { print(\"none\") } else { print(home.upper()) }\n\
                      print(program.environment(\"ZENO_EMPTY\") == \"\")\n\
                      print(program.environment(\"ZENO_NOT_SET\"))";
        with_environment(&[("ZENO_HOME", "/home/partha"), ("ZENO_EMPTY", "")], || {
            assert_eq!(output(source), "/HOME/PARTHA\ntrue\nnone\n");
        });
        with_environment(&[], || {
            assert_eq!(output(source), "none\nfalse\nnone\n");
        });
    }

    #[test]
    fn a_test_that_ends_the_program_fails() {
        let program = parser::parse(
            lexer::lex("import program\ntest \"ends\" {\n    program.exit(2)\n}\ntest \"goes on\" {\n    assert 1 == 1\n}").unwrap(),
        )
        .unwrap();
        let (output, tested) = super::run_tests(&program, true, crate::value::testing::outside(), None);
        let tested = tested.expect("the tests ran");
        assert_eq!(output, "  FAILED  ends\n  ok      goes on\n");
        assert_eq!(
            tested[0].failure.as_ref().map(|error| error.message.as_str()),
            Some("the program ended here, with `program.exit(2)`")
        );
    }

    #[test]
    fn a_program_that_is_not_there_is_a_file_error() {
        assert_eq!(
            error("import program\nprogram.run(\"zeno-no-such-program\", [])"),
            "uncaught FileError: there is no program `zeno-no-such-program` to run"
        );
        // It is an error that can be caught, and the program goes on.
        assert_eq!(
            output(
                "from program import run\n\
                 try { print(run(\"zeno-no-such-program\", [\"a\"])) } except FileError as error { print(error.message) }\n\
                 print(\"still here\")"
            ),
            "there is no program `zeno-no-such-program` to run\nstill here\n"
        );
        assert_eq!(error("import program\nprogram.run(\"sh\")"), "`run` takes 2 arguments, but 1 was given");
        assert_eq!(
            error("import program\nlet words = [1]\nprogram.run(\"sh\", words)"),
            "`run` needs a str, but this is an int"
        );
        assert_eq!(error("import program\nprogram.run(\"sh\", \"-c\")"), "`run` needs a list of str, but this is a str");
    }

    /// `sh` is on every Unix. Windows has no program that is sure to be
    /// there and takes its words the same way, so this is not run there.
    #[cfg(unix)]
    #[test]
    fn another_program_is_run_with_the_words_as_they_are() {
        let path = scratch("run.txt");
        // The other program writes each word it was given, with a `|`
        // after it, into the file. No shell reads the words on the way:
        // a blank, a quote, a `$` and a `*` arrive as they were written,
        // and so does a word that is empty.
        let source = format!(
            "import files\nimport program\n\
             let script = \"printf '%s|' \\\"$@\\\" > \\\"$0\\\"; exit 3\"\n\
             let code = program.run(\"sh\", [\"-c\", script, \"{path}\", \"two words\", \"a\\\"b\", \"$HOME\", \"*\", \"\"])\n\
             print(code, files.read(\"{path}\"))\n\
             files.remove(\"{path}\")\n\
             print(program.run(\"sh\", [\"-c\", \"exit 0\"]))\n\
             let none_given: list[str] = []\n\
             print(program.run(\"true\", none_given), program.run(\"false\", []))"
        );
        assert_eq!(output(&source), "3 two words|a\"b|$HOME|*||\n0\n0 1\n");
        // A program that was stopped from outside has no number of its
        // own: 128 and the number of the signal, as a shell says it.
        assert_eq!(output("import program\nprint(program.run(\"sh\", [\"-c\", \"kill -9 $$\"]))"), "137\n");
    }

    /// Is there a Python on this machine for `import python` to start?
    /// A test that needs one says nothing where there is none, as the
    /// tests of the C backend do where there is no C compiler.
    fn python_is_here() -> bool {
        let names: &[&str] = if cfg!(windows) { &["python", "python3", "py"] } else { &["python3", "python"] };
        names.iter().any(|name| {
            std::process::Command::new(name)
                .args(["-c", "import sys; sys.exit(0 if sys.version_info >= (3, 6) else 1)"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    #[test]
    fn a_module_of_python_is_used_from_zeno() {
        if !python_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own, and can be used
        // as such at once. A named argument is a keyword argument.
        assert_eq!(
            output(
                "import python\n\
                 let math = python.module(\"math\")\n\
                 print(math.sqrt(16.0) + 1, math.floor(2.5) + 1, math.pi > 3.14, math.isnan(math.nan))\n\
                 let builtins = python.module(\"builtins\")\n\
                 print(builtins.int(\"ff\", base=16), builtins.str(2.5) + \"!\", builtins.round(2.567, ndigits=1))\n\
                 print(builtins.len(\"h\u{e9}llo \u{1F600}\"), builtins.str.upper(\"tab\\there \\\"q\\\"\"), builtins.max([3, 9, 4]))\n\
                 print(builtins.abs(-1), builtins.divmod(7, 2), 2.0 == builtins.float(2), builtins.print is none)"
            ),
            "5.0 3 true true\n255 2.5! 2.6\n7 TAB\tHERE \"Q\" 9\n1 (3, 1) true false\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_python() {
        if !python_is_here() {
            return;
        }
        // A list of Python is the one list there is: Zeno adds to it,
        // reads it, loops over it and asks what is in it, and Python
        // sees every change. `python.copy` makes a list of Zeno of it.
        assert_eq!(
            output(
                "import python\n\
                 let builtins = python.module(\"builtins\")\n\
                 let items = builtins.list(builtins.range(3))\n\
                 items.append(10)\n\
                 items[0] = \"zero\"\n\
                 print(items, len(items), items[3] + 1, 10 in items, 11 not in items)\n\
                 var seen: list[str] = []\n\
                 for item in items {\n    seen.append(str(item))\n}\n\
                 print(seen.join(\"-\"), [items], f\"{items[0]:>6}|\")\n\
                 let copy = python.copy(items)\n\
                 copy.append(11)\n\
                 print(copy, len(items), python.copy(3), items == items, items == builtins.list(items), items == 3)\n\
                 let ages = builtins.dict(ann=31, bob=40)\n\
                 ages[\"cy\"] = [1.5, none, true]\n\
                 for name, age in python.copy(ages) {\n    print(name, age)\n}\n\
                 print(\"ann\" in ages, len(ages), ages.get(\"dee\", 0), ages)\n\
                 let fractions = python.module(\"fractions\")\n\
                 let half = fractions.Fraction(1, 2)\n\
                 let box = python.module(\"types\").SimpleNamespace(size=2)\n\
                 box.note = \"kept in Python\"\n\
                 box.size += 1\n\
                 print(half, half.numerator, half == fractions.Fraction(2, 4), half == 0.5, box.note, box.size)\n\
                 let given = builtins.list([1, [2.5, \"x\", none], [\"k\": [true], \"j\": [false]], half])\n\
                 print(given, builtins.type(given[2]).__name__)"
            ),
            "['zero', 1, 2, 10] 4 11 true true\n\
             zero-1-2-10 [['zero', 1, 2, 10]]   zero|\n\
             [\"zero\", 1, 2, 10, 11] 4 3 true true false\n\
             ann 31\nbob 40\ncy [1.5, none, true]\n\
             true 3 0 {'ann': 31, 'bob': 40, 'cy': [1.5, None, True]}\n\
             1/2 1 true true kept in Python 3\n\
             [1, [2.5, 'x', None], {'k': [True], 'j': [False]}, Fraction(1, 2)] dict\n"
        );
    }

    #[test]
    fn an_error_of_python_is_an_error_of_zeno() {
        if !python_is_here() {
            return;
        }
        // Python's error has Zeno's kind where the two have one name, a
        // file that cannot be used is a FileError, and every other is an
        // `Error` that says what Python called it.
        assert_eq!(
            output(
                "import python\n\
                 let builtins = python.module(\"builtins\")\n\
                 try { builtins.int(\"x\") } except ValueError as error { print(1, error.message) }\n\
                 try { builtins.list()[2] } except IndexError as error { print(2, error.message) }\n\
                 try { builtins.dict()[\"k\"] } except KeyError as error { print(3, error.message) }\n\
                 try { builtins.divmod(1, 0) } except ZeroDivisionError as error { print(4, error.message.ends_with(\"by zero\")) }\n\
                 try { python.module(\"math\").exp(1000.0) } except OverflowError as error { print(5, error.message) }\n\
                 try { builtins.open(\"/zeno/no/such/file\") } except FileError as error { print(6, error.message.starts_with(\"FileNotFoundError: \")) }\n\
                 try { builtins.len(5) } except Error as error { print(7, error.kind, error.message) }\n\
                 try { python.module(\"zeno_no_such_module\") } except FileError as error { print(8, error.message.starts_with(\"Python has no module `zeno_no_such_module`\")) }\n\
                 try { python.module(\"json\").loads(\"{\") } except ValueError as error { print(9, error.message.starts_with(\"JSONDecodeError: \")) }\n\
                 try { python.copy(builtins.dict([[1.5, 2]])) } except ValueError as error { print(10, error.message) }\n\
                 print(builtins.pow(2, 70), \"still here\")"
            ),
            "1 invalid literal for int() with base 10: 'x'\n\
             2 list index out of range\n\
             3 there is no key 'k'\n\
             4 true\n\
             5 math range error\n\
             6 true\n\
             7 Error TypeError: object of type 'int' has no len()\n\
             8 true\n\
             9 true\n\
             10 a dict in Zeno has keys that are str or int, and this one has the key 1.5\n\
             1180591620717411303424 still here\n"
        );
        assert_eq!(
            error("import python\npython.module(\"builtins\").int(\"x\")"),
            "uncaught ValueError: invalid literal for int() with base 10: 'x'"
        );
        // What Zeno cannot hand over is said before anything is sent.
        assert_eq!(
            error("import python\nfunction double(n) { return n * 2 }\npython.module(\"builtins\").map(double, [1])"),
            "a function of Zeno cannot be given to Python yet"
        );
        assert_eq!(
            error("import python\npython.module(\"builtins\").str(3 s)"),
            "a quantity in s cannot be given to Python"
        );
        assert_eq!(
            error("import python\nlet xs = python.module(\"builtins\").list()\nif xs { print(1) }"),
            "a condition must be true or false, but this is a Python value"
        );
        assert_eq!(error("import python\npython.module()"), "`module` takes 1 argument, but 0 were given");
        assert_eq!(error("import python\npython.module(3)"), "`module` needs a str, but this is an int");
    }

    /// Is there a Node on this machine for `import node` to start?
    fn node_is_here() -> bool {
        std::process::Command::new("node")
            .args(["-e", "process.exit(0)"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_module_of_node_is_used_from_zeno() {
        if !node_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own. A number of
        // JavaScript that is whole is an int. Named arguments are the
        // one object of options that goes last, and a promise is
        // waited for.
        assert_eq!(
            output(
                "import node\n\
                 let path = node.module(\"path\")\n\
                 print(path.posix.join(\"a\", \"b\", \"c.txt\"), path.extname(\"x.tar.gz\"), len(path.sep))\n\
                 let Math = node.global(\"Math\")\n\
                 print(Math.max(3, 9, 4) + 1, Math.sqrt(16.0) + 1, Math.PI > 3.14, Math.sqrt(2.0) < 1.5, Math.nothing is none)\n\
                 let Number = node.global(\"Number\")\n\
                 print(Number(\"0x10\"), Number(\"2.5\") + 1, Number.isNaN(Number(\"x\")), Number.MAX_SAFE_INTEGER + 1, Number(\"1e21\"))\n\
                 print(node.global(\"String\")(9007199254740993), node.global(\"BigInt\")(\"9007199254740993\") + 1, node.global(\"undefined\") is none)\n\
                 let util = node.module(\"util\")\n\
                 print(util.inspect(\"h\u{e9}llo \u{1F600}\\t\\\"q\\\"\"), util.inspect([1, [2, [3]]], depth=0, compact=true))\n\
                 let waited = node.module(\"timers/promises\").setTimeout(1, \"waited for\")\n\
                 print(waited, node.global(\"Promise\").resolve(41) + 1)\n\
                 let day = node.new(node.global(\"Date\"), 0)\n\
                 let year = day.getUTCFullYear\n\
                 print(day.toISOString(), year() + 1, node.new(node.global(\"URL\"), \"https://zeno-lang.org/a?b=1\").searchParams.get(\"b\"))"
            ),
            "a/b/c.txt .gz 1\n\
             10 5 true true true\n\
             16 3.5 true 9007199254740992 1e21\n\
             9007199254740993 9007199254740994 true\n\
             'h\u{e9}llo \u{1F600}\\t\"q\"' [ 1, [Array] ]\n\
             waited for 42\n\
             1970-01-01T00:00:00.000Z 1971 1\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_node() {
        if !node_is_here() {
            return;
        }
        // An array of JavaScript is the one array there is. `node.copy`
        // makes a list of Zeno of it, and a dict of a plain object.
        assert_eq!(
            output(
                "import node\n\
                 let items = node.global(\"Array\").of(0, 1, 2)\n\
                 items.push(10)\n\
                 items[0] = \"zero\"\n\
                 print(items, len(items), items[3] + 1, 10 in items, 11 not in items, items[9] is none)\n\
                 var seen: list[str] = []\n\
                 for item in items {\n    seen.append(str(item))\n}\n\
                 print(seen.join(\"-\"), [items], f\"{items[0]:>6}|\")\n\
                 let copy = node.copy(items)\n\
                 copy.append(11)\n\
                 print(copy, len(items), node.copy(3), items == items, items == node.global(\"Array\").of(), items == 3)\n\
                 let ages = node.global(\"Object\")(ann=31, bob=40)\n\
                 ages[\"cy\"] = [1.5, none, true]\n\
                 ages.dee = 5\n\
                 for name, age in node.copy(ages) {\n    print(name, age)\n}\n\
                 for name in ages {\n    print(name)\n}\n\
                 print(\"ann\" in ages, len(ages), ages.ann + ages[\"bob\"], ages)\n\
                 let seen_by = node.new(node.global(\"Map\"))\n\
                 seen_by.set(\"k\", [1, 2])\n\
                 seen_by[7] = \"seven\"\n\
                 print(len(seen_by), \"k\" in seen_by, seen_by[7], seen_by[\"k\"], node.copy(seen_by))\n\
                 let bytes = node.global(\"Buffer\").alloc(2)\n\
                 bytes[1] = 255\n\
                 print(len(bytes), node.copy(bytes), node.copy(node.new(node.global(\"Set\"), [3, 3, 4])))\n\
                 let given = node.global(\"Array\").of([2.5, \"x\", none], [\"k\": [true]], 2 ** 62)\n\
                 print(given, node.global(\"Array\").isArray(given[0]), given[2] == 2 ** 62)"
            ),
            "[ 'zero', 1, 2, 10 ] 4 11 true true true\n\
             zero-1-2-10 [[ 'zero', 1, 2, 10 ]]   zero|\n\
             [\"zero\", 1, 2, 10, 11] 4 3 true false false\n\
             ann 31\nbob 40\ncy [1.5, none, true]\ndee 5\n\
             ann\nbob\ncy\ndee\n\
             true 4 71 { ann: 31, bob: 40, cy: [ 1.5, null, true ], dee: 5 }\n\
             2 true seven [ 1, 2 ] [\"k\": [1, 2], 7: \"seven\"]\n\
             2 [0, 255] [3, 4]\n\
             [ [ 2.5, 'x', null ], { k: [ true ] }, 4611686018427387904n ] true true\n"
        );
    }

    #[test]
    fn an_error_of_node_is_an_error_of_zeno() {
        if !node_is_here() {
            return;
        }
        // What JavaScript throws is an `Error` that says what it was
        // called there. What the system refused is a FileError.
        assert_eq!(
            output(
                "import node\n\
                 let JSON = node.global(\"JSON\")\n\
                 try { JSON.parse(\"{oops\") } except Error as error { print(1, error.kind, error.message.starts_with(\"SyntaxError: \")) }\n\
                 try { node.module(\"fs\").readFileSync(\"/zeno/no/such/file\") } except FileError as error { print(2, error.message.starts_with(\"ENOENT: \")) }\n\
                 try { node.module(\"fs/promises\").readFile(\"/zeno/no/such/file\") } except FileError as error { print(3, error.message.starts_with(\"ENOENT: \")) }\n\
                 try { node.module(\"zeno-no-such-module\") } except FileError as error { print(4, error.message.starts_with(\"Node has no module `zeno-no-such-module`\")) }\n\
                 try { node.global(\"zeno_no_such_thing\") } except Error as error { print(5, error.message) }\n\
                 try { node.global(\"eval\")(\"throw 5\") } except Error as error { print(6, error.message) }\n\
                 try { node.new(node.global(\"Map\"), 5) } except Error as error { print(7, error.message.starts_with(\"TypeError: \")) }\n\
                 try { node.new(node.global(\"Math\")) } except Error as error { print(8, error.message) }\n\
                 try { node.copy(node.new(node.global(\"Map\"), [[1.5, 2]])) } except ValueError as error { print(9, error.message) }\n\
                 try { node.global(\"Promise\").reject(node.new(node.global(\"Error\"), \"no\")) } except Error as error { print(10, error.message) }\n\
                 print(\"still here\")"
            ),
            "1 Error true\n2 true\n3 true\n4 true\n\
             5 JavaScript has nothing called `zeno_no_such_thing`\n\
             6 thrown: 5\n7 true\n\
             8 TypeError: this is not a function, and cannot be called\n\
             9 a dict in Zeno has keys that are str or int, and this one has the key 1.5\n\
             10 no\nstill here\n"
        );
        assert_eq!(
            error("import node\nfunction double(n) { return n * 2 }\nnode.global(\"Array\").of(1).map(double)"),
            "a function of Zeno cannot be given to JavaScript yet"
        );
        assert_eq!(
            error("import node\nif node.global(\"process\") { print(1) }"),
            "a condition must be true or false, but this is a JavaScript value"
        );
        assert_eq!(error("import node\nnode.new()"), "`new` takes the class to make an object of, and then what it is made from, but nothing was given");
        assert_eq!(error("import node\nnode.new(3)"), "`new` needs a class of JavaScript, but this is an int");
        assert_eq!(error("import node\nnode.global(3)"), "`global` needs a str, but this is an int");
        // Each stays where it was made.
        if python_is_here() {
            assert_eq!(
                error("import node\nimport python\npython.module(\"builtins\").len(node.global(\"process\"))"),
                "a JavaScript value cannot be given to Python. Each stays where it was made"
            );
            assert_eq!(
                output("import node\nimport python\nprint(python.module(\"sys\") == node.global(\"process\"), python.module(\"math\").floor(node.global(\"Math\").PI))"),
                "false 3\n"
            );
        }
    }

    /// Is there a Java on this machine for `import java` to start?
    fn java_is_here() -> bool {
        std::process::Command::new("java")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_class_of_java_is_used_from_zeno() {
        if !java_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own. Calling a class
        // makes an object, a dot after a class is what is `static`
        // there, and of several methods with one name the one the
        // arguments fit is called.
        assert_eq!(
            output(
                "import java\n\
                 let Math = java.type(\"java.lang.Math\")\n\
                 print(Math.max(3, 9) + 1, Math.max(3, 2.5), Math.sqrt(16.0), Math.PI > 3.14, Math.abs(-7), Math.floorMod(-7, 3))\n\
                 let Integer = java.type(\"java.lang.Integer\")\n\
                 print(Integer.parseInt(\"ff\", 16), Integer.MAX_VALUE + 1, Integer.toBinaryString(5) + \"!\", java.type(\"java.lang.Long\").MAX_VALUE)\n\
                 let String = java.type(\"java.lang.String\")\n\
                 print(String.format(\"%s has %d and %.1f\", \"it\", 3, 2.5), String.valueOf(true), String.join(\"-\", [\"a\", \"b\"]), String.valueOf(2.5))\n\
                 let text = java.type(\"java.lang.StringBuilder\")(\"h\u{e9}llo \u{1F600}\\t\\\"q\\\"\")\n\
                 text.append(1).append(2.5).append(true).append('c')\n\
                 print(text, len(text), text.toString().upper(), text.charAt(0), text.indexOf(\"llo\"))\n\
                 let day = java.type(\"java.time.LocalDate\").of(2026, 11, 18)\n\
                 let later = day.plusMonths\n\
                 print(day, day.getDayOfWeek(), later(1), day.getYear() + 1, day.isLeapYear(), day == java.type(\"java.time.LocalDate\").parse(\"2026-11-18\"))\n\
                 let big = java.type(\"java.math.BigInteger\")(\"123456789012345678901234567890\")\n\
                 print(big.add(big), big.bitLength(), big.signum(), java.type(\"java.lang.Thread\").State.NEW, java.type(\"java.util.Map.Entry\"), big[\"not\"]().signum(), java.type(\"java.lang.System\")[\"in\"] is not none)\n\
                 let point = java.type(\"java.awt.Point\")(1, 2)\n\
                 point.x = 10\n\
                 point.translate(1, 1)\n\
                 print(point.x + point.y, point == java.type(\"java.awt.Point\")(11, 3), java.type(\"java.lang.Float\").valueOf(\"0.1\"), java.type(\"java.lang.Double\").NaN)"
            ),
            "10 3.0 4.0 true 7 2\n\
             255 2147483648 101! 9223372036854775807\n\
             it has 3 and 2.5 true a-b 2.5\n\
             h\u{e9}llo \u{1F600}\t\"q\"12.5truec 21 H\u{e9}LLO \u{1F600}\t\"Q\"12.5TRUEC h 2\n\
             2026-11-18 WEDNESDAY 2026-12-18 2027 false true\n\
             246913578024691357802469135780 97 1 NEW class java.util.Map$Entry -1 true\n\
             14 true 0.1 NaN\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_java() {
        if !java_is_here() {
            return;
        }
        // A list of Java is the one list there is. `java.copy` makes a
        // list of Zeno of it, and a dict of a map.
        assert_eq!(
            output(
                "import java\n\
                 let items = java.type(\"java.util.ArrayList\")([0, 1, 2])\n\
                 items.add(10)\n\
                 items[0] = \"zero\"\n\
                 print(items, len(items), items[3] + 1, 10 in items, 11 not in items, items.size())\n\
                 var seen: list[str] = []\n\
                 for item in items {\n    seen.append(str(item))\n}\n\
                 print(seen.join(\"-\"), [items], f\"{items[0]:>6}|\")\n\
                 let copy = java.copy(items)\n\
                 copy.append(11)\n\
                 print(copy, len(items), java.copy(3), items == items, items == java.type(\"java.util.ArrayList\")(items), items == 3)\n\
                 let ages = java.type(\"java.util.LinkedHashMap\")([\"ann\": 31, \"bob\": 40])\n\
                 ages[\"cy\"] = [1.5, none, true]\n\
                 ages.put(7, \"seven\")\n\
                 for name, age in java.copy(ages) {\n    print(name, age)\n}\n\
                 for name in ages {\n    print(name)\n}\n\
                 print(\"ann\" in ages, len(ages), ages[\"ann\"] + ages.get(\"bob\"), ages.get(\"dee\") is none, ages)\n\
                 let fixed = java.type(\"java.util.List\").of(3, 1, 2)\n\
                 print(fixed, fixed.size(), fixed.contains(2), fixed.indexOf(2), java.copy(java.type(\"java.util.TreeSet\")(fixed)))\n\
                 let numbers = java.type(\"java.lang.reflect.Array\").newInstance(java.type(\"int\"), 3)\n\
                 numbers[1] = 5\n\
                 print(numbers, len(numbers), numbers.length, numbers[1] + 1, 5 in numbers, java.copy(numbers))\n\
                 let Arrays = java.type(\"java.util.Arrays\")\n\
                 print(Arrays.asList(1, \"two\", 3.5), Arrays.stream([3, 1, 2]).sorted().toArray(), Arrays.toString([\"a\", \"b\"]))\n\
                 for part in java.type(\"java.util.StringTokenizer\")(\"a b\") {\n    print(part)\n}\n\
                 let given = java.type(\"java.util.ArrayList\")([1, [2.5, \"x\", none], [\"k\": [true]], 2 ** 62])\n\
                 print(given, given[1].getClass().getSimpleName(), given[3] == 2 ** 62)"
            ),
            "[zero, 1, 2, 10] 4 11 true true 4\n\
             zero-1-2-10 [[zero, 1, 2, 10]]   zero|\n\
             [\"zero\", 1, 2, 10, 11] 4 3 true true false\n\
             ann 31\nbob 40\ncy [1.5, none, true]\n7 seven\n\
             ann\nbob\ncy\n7\n\
             true 4 71 true {ann=31, bob=40, cy=[1.5, null, true], 7=seven}\n\
             [3, 1, 2] 3 true 2 [1, 2, 3]\n\
             [0, 5, 0] 3 3 6 true [0, 5, 0]\n\
             [1, two, 3.5] [1, 2, 3] [a, b]\n\
             a\nb\n\
             [1, [2.5, x, null], {k=[true]}, 4611686018427387904] ArrayList true\n"
        );
    }

    #[test]
    fn an_error_of_java_is_an_error_of_zeno() {
        if !java_is_here() {
            return;
        }
        // An exception has Zeno's kind where the two mean one thing, a
        // file that cannot be used is a FileError, and every other is an
        // `Error` that says what Java called it.
        assert_eq!(
            output(
                "import java\n\
                 let ArrayList = java.type(\"java.util.ArrayList\")\n\
                 try { java.type(\"java.lang.Integer\").parseInt(\"x\") } except ValueError as error { print(1, error.message.starts_with(\"NumberFormatException: \")) }\n\
                 try { ArrayList().get(2) } except IndexError as error { print(2, error.message.starts_with(\"IndexOutOfBoundsException: \")) }\n\
                 try { ArrayList()[2] } except IndexError as error { print(3, error.message) }\n\
                 try { java.type(\"java.nio.file.Files\").readString(java.type(\"java.nio.file.Path\").of(\"/zeno/no/such/file\")) } except FileError as error { print(4, error.message.starts_with(\"NoSuchFileException: \")) }\n\
                 try { java.type(\"java.util.List\").of(1).add(2) } except Error as error { print(5, error.kind, error.message) }\n\
                 try { java.type(\"zeno.no.such.Class\") } except FileError as error { print(6, error.message.starts_with(\"Java has no class `zeno.no.such.Class`\")) }\n\
                 try { java.jar(\"zeno-no-such.jar\") } except FileError as error { print(7, error.message.starts_with(\"there is no jar or folder `\"), error.message.ends_with(\"zeno-no-such.jar`\")) }\n\
                 try { ArrayList().add(1, 2, 3) } except Error as error { print(8, error.message) }\n\
                 try { ArrayList().size(most=1) } except Error as error { print(9, error.message) }\n\
                 try { ArrayList().nothing } except Error as error { print(10, error.message) }\n\
                 try { java.type(\"java.lang.Math\").nothing } except Error as error { print(11, error.message) }\n\
                 try { java.type(\"java.lang.Runnable\")() } except Error as error { print(12, error.message) }\n\
                 try { ArrayList()() } except Error as error { print(13, error.message) }\n\
                 try { java.copy(java.type(\"java.util.HashMap\")([1: 2]).entrySet().iterator().next()) } except Error as error { print(14) }\n\
                 try { java.copy(java.type(\"java.util.Map\").of(1.5, 2)) } except ValueError as error { print(15, error.message) }\n\
                 print(\"still here\")"
            ),
            "1 true\n2 true\n\
             3 index 2 is out of range for 0 items\n\
             4 true\n\
             5 Error UnsupportedOperationException\n\
             6 true\n\
             7 true true\n\
             8 Java has no `add` that takes an int, an int, an int. There is add(Object), add(int, Object)\n\
             9 Java has no named arguments, and `most` was given as one\n\
             10 a java.util.ArrayList has no field or method `nothing`\n\
             11 the class java.lang.Math has no static field or method `nothing`\n\
             12 `Runnable` is an interface, and no object can be made of it by calling it\n\
             13 a java.util.ArrayList is not a method or a class, and cannot be called\n\
             15 a dict in Zeno has keys that are str or int, and this one has the key 1.5\n\
             still here\n"
        );
        assert_eq!(
            error("import java\nfunction double(n) { return n * 2 }\njava.type(\"java.util.ArrayList\")().forEach(double)"),
            "a function of Zeno cannot be given to Java yet"
        );
        assert_eq!(
            error("import java\nif java.type(\"java.util.ArrayList\")() { print(1) }"),
            "a condition must be true or false, but this is a Java value"
        );
        assert_eq!(error("import java\njava.type()"), "`type` takes 1 argument, but 0 were given");
        assert_eq!(error("import java\njava.jar(3)"), "`jar` needs a str, but this is an int");
        // Each stays where it was made.
        if node_is_here() {
            assert_eq!(
                error("import java\nimport node\nnode.global(\"String\")(java.type(\"java.util.ArrayList\")())"),
                "a Java value cannot be given to JavaScript. Each stays where it was made"
            );
        }
    }

    /// Is there a Ruby on this machine for `import ruby` to start?
    fn ruby_is_here() -> bool {
        std::process::Command::new("ruby")
            .args(["-e", "exit 0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_library_of_ruby_is_used_from_zeno() {
        if !ruby_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own. A constant is named
        // as Ruby writes it, a call has brackets, a name Zeno cannot write
        // after a dot goes through `send`, and a named argument is a keyword
        // argument.
        assert_eq!(
            output(
                "import ruby\n\
                 ruby.require(\"date\")\n\
                 let Date = ruby.constant(\"Date\")\n\
                 let day = Date.new(2026, 11, 18)\n\
                 print(day, day.year() + 1, day.strftime(\"%A\"), day.send(\"leap?\"), day.next_day(30), day == Date.parse(\"2026-11-18\"), day.send(\"-\", Date.new(2026, 1, 1)).to_i())\n\
                 let Math = ruby.constant(\"Math\")\n\
                 print(Math.PI > 3.14, Math.sqrt(16), Math.hypot(3, 4), ruby.constant(\"Float::INFINITY\"), ruby.constant(\"Float\").NAN, ruby.constant(\"Integer\").sqrt(99))\n\
                 let Kernel = ruby.constant(\"Kernel\")\n\
                 let text = Kernel.format(\"%05.1f|%s|%d\", 3.14159, \"h\u{e9}llo \u{1f600}\\t\\\"q\\\"\", 7)\n\
                 print(text, len(text), text.upper(), Kernel.send(\"Integer\", \"ff\", 16), Kernel.send(\"Integer\", \"x\", exception=false), 2 ** 62)\n\
                 let big = Kernel.send(\"Integer\", \"123456789012345678901234567890\")\n\
                 print(big, big.send(\"+\", big), big.bit_length(), big.send(\"even?\"), big.send(\"class\"), big == big.send(\"+\", 0))\n\
                 let Point = ruby.constant(\"Struct\").new(ruby.symbol(\"x\"), ruby.symbol(\"y\"), keyword_init=true)\n\
                 let point = Point.new(x=1, y=2.5)\n\
                 point.x = point.x() + 10\n\
                 let read = point.y\n\
                 print(point.x(), read(), point.to_a(), point == Point.new(x=11, y=2.5), point == Point.new(x=0, y=0), ruby.symbol(\"x\"), ruby.symbol(\"x\") == ruby.symbol(\"x\"), ruby.symbol(\"x\") == \"x\")\n\
                 ruby.require(\"net/http\")\n\
                 ruby.require(\"digest\")\n\
                 print(ruby.constant(\"Net::HTTP\"), ruby.constant(\"Net\").HTTP == ruby.constant(\"Net::HTTP\"), ruby.constant(\"URI\").parse(\"https://zeno-lang.org/docs?x=1\").host(), ruby.constant(\"Digest::SHA256\").hexdigest(\"zeno\"))\n\
                 print(ruby.require(\"set\"), read)"
            ),
            "2026-11-18 2027 Wednesday false 2026-12-18 true 321\n\
             true 4.0 5.0 inf NaN 9\n\
             003.1|h\u{e9}llo \u{1f600}\t\"q\"|7 19 003.1|H\u{e9}LLO \u{1f600}\t\"Q\"|7 255 none 4611686018427387904\n\
             123456789012345678901234567890 246913578024691357802469135780 97 true Integer true\n\
             11 2.5 [11, 2.5] true false x true false\n\
             Net::HTTP true zeno-lang.org 988465d28db4d902b53d9e902ea5d7bba65489f4aafdb2df4c271a9e4f6364a4\n\
             none method `y` of a Struct. In Zeno a call has brackets: `.y()`\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_ruby() {
        if !ruby_is_here() {
            return;
        }
        // An Array of Ruby is the one Array there is. `ruby.copy` makes a
        // list of Zeno of it, with everything in it, and a symbol in it is
        // copied as its name. Text is a str whatever its encoding was, and
        // bytes that are not text stay in Ruby.
        assert_eq!(
            output(
                "import ruby\n\
                 let items = ruby.constant(\"Array\").new(3, 0)\n\
                 items.push(5).push(\"x\")\n\
                 items[0] = [1, 2.5, none, true, [\"k\": \"v\"]]\n\
                 print(len(items), items[1], items[3] + 1, items[4] + \"!\", 5 in items, 6 in items, items.size(), items.last(), items.index(5))\n\
                 var total = 0\n\
                 for item in items.last(3) { total = total + len(str(item)) }\n\
                 print(total, items == items, items == items.dup(), items == 3, items[0][4][\"k\"], len(items[0]), items[0].length())\n\
                 let copy = ruby.copy(items)\n\
                 copy[1] = 9\n\
                 print(copy, items[1], ruby.copy(3), ruby.copy(\"as it is\"), ruby.copy(items.first(2)))\n\
                 let ages = ruby.constant(\"Hash\").new()\n\
                 ages[\"ann\"] = 31\n\
                 ages[ruby.symbol(\"bob\")] = 40\n\
                 ages[7] = [ruby.symbol(\"seven\"), 7.0]\n\
                 print(len(ages), ages[\"ann\"], ages[ruby.symbol(\"bob\")], ages[\"bob\"], \"ann\" in ages, \"bob\" in ages, ages.key(31), ages.fetch(\"zed\", 0))\n\
                 for key in ages { print(key) }\n\
                 for name, age in ruby.copy(ages) { print(name, age) }\n\
                 for n in ruby.constant(\"Range\").new(1, 3) { print(n * 2) }\n\
                 for pair in ages.first(2) { print(ruby.copy(pair)) }\n\
                 ruby.require(\"set\")\n\
                 ruby.require(\"stringio\")\n\
                 let seen = ruby.constant(\"Set\").new([3, 1, 3])\n\
                 print(len(seen), 3 in seen, ruby.copy(seen), ruby.copy(ruby.constant(\"Range\").new(1, 4).step(2).to_a()), len(ruby.constant(\"Range\").new(1, 10)))\n\
                 for line in ruby.constant(\"StringIO\").new(\"a\\nb\\n\") { print(line.strip()) }\n\
                 let bytes = ruby.constant(\"Array\").new(2, 255).pack(\"C*\")\n\
                 let latin = ruby.constant(\"Array\").new(1, 233).pack(\"C*\").force_encoding(\"ISO-8859-1\")\n\
                 print(len(bytes), bytes.bytes(), bytes == bytes.dup(), bytes.unpack1(\"H*\"), ruby.constant(\"Array\").new(2, 104).pack(\"C*\") + \"i\", latin, len(latin))\n\
                 let word = ruby.constant(\"String\").new(\"zeno\")\n\
                 print(word.upper(), ruby.symbol(\"zeno\").length(), ruby.symbol(\"upcase\").to_proc().call(\"x\" + word) == \"XZENO\")"
            ),
            "5 0 6 x! true false 5 x 3\n\
             3 true true false v 5 5\n\
             [[1, 2.5, none, true, [\"k\": \"v\"]], 9, 0, 5, \"x\"] 0 3 as it is [[1, 2.5, none, true, [\"k\": \"v\"]], 0]\n\
             3 31 40 none true false ann 0\n\
             ann\n\
             bob\n\
             7\n\
             ann 31\n\
             bob 40\n\
             7 [\"seven\", 7.0]\n\
             2\n\
             4\n\
             6\n\
             [\"ann\", 31]\n\
             [\"bob\", 40]\n\
             2 true [3, 1] [1, 3] 10\n\
             a\n\
             b\n\
             2 [255, 255] true ffff hhi \u{e9} 1\n\
             ZENO 4 true\n"
        );
    }

    #[test]
    fn an_error_of_ruby_is_an_error_of_zeno() {
        if !ruby_is_here() {
            return;
        }
        // An exception has Zeno's kind where the two mean one thing, a
        // file that cannot be used is a FileError, and every other is an
        // `Error` that says what Ruby called it. What Ruby words is not
        // compared, since each Ruby words it differently.
        assert_eq!(
            output(
                "import ruby\n\
                 let Kernel = ruby.constant(\"Kernel\")\n\
                 let items = ruby.constant(\"Array\").new(2, 0)\n\
                 try { Kernel.send(\"Integer\", \"x\") } except ValueError as error { print(1, error.message.starts_with(\"ArgumentError: \")) }\n\
                 try { items.fetch(2) } except IndexError as error { print(2, error.message != \"\") }\n\
                 try { ruby.constant(\"Hash\").new().fetch(\"k\") } except KeyError as error { print(3, error.message != \"\") }\n\
                 try { Kernel.send(\"Rational\", 1, 0) } except ZeroDivisionError as error { print(4, error.message != \"\") }\n\
                 try { ruby.constant(\"File\").read(\"/zeno/no/such/file\") } except FileError as error { print(5, error.message.starts_with(\"Errno::ENOENT: \")) }\n\
                 try { items.send(\"+\", 1) } except Error as error { print(6, error.kind, error.message.starts_with(\"TypeError: \")) }\n\
                 try { Kernel.send(\"raise\", \"it broke\") } except Error as error { print(7, error.kind, error.message) }\n\
                 try { ruby.require(\"zeno_no_such_library\") } except FileError as error { print(8, error.message.starts_with(\"Ruby has no library `zeno_no_such_library` (\")) }\n\
                 try { ruby.constant(\"ZenoNoSuch\") } except Error as error { print(9, error.message) }\n\
                 try { items.nothing() } except Error as error { print(10, error.message) }\n\
                 try { items.frozen } except Error as error { print(11, error.message) }\n\
                 try { ruby.constant(\"Math\").nothing } except Error as error { print(12, error.message) }\n\
                 try { ruby.constant(\"Array\")(1) } except Error as error { print(13, error.message) }\n\
                 try { items() } except Error as error { print(14, error.message) }\n\
                 try { items.push(1, most=2).fetch(9, 9, 9) } except ValueError as error { print(16, error.message.starts_with(\"ArgumentError: \")) }\n\
                 try { ruby.copy(ruby.constant(\"Hash\").send(\"[]\", 1.5, 2)) } except ValueError as error { print(17, error.message) }\n\
                 try { for n in ruby.constant(\"Range\").new(1, none) { print(n) } } except ValueError as error { print(18, error.message) }\n\
                 try { for n in ruby.constant(\"Object\").new() { print(n) } } except Error as error { print(19, error.message) }\n\
                 try { len(ruby.constant(\"Object\").new()) } except Error as error { print(20, error.message) }\n\
                 try { print(1 in ruby.constant(\"Object\").new()) } except Error as error { print(21, error.message) }\n\
                 try { ruby.constant(\"Object\").new()[0] } except Error as error { print(22, error.message.starts_with(\"NoMethodError: \")) }\n\
                 print(items.size, \"still here\")"
            ),
            "1 true\n\
             2 true\n\
             3 true\n\
             4 true\n\
             5 true\n\
             6 Error true\n\
             7 Error it broke\n\
             8 true\n\
             9 Ruby has no constant `ZenoNoSuch`. One that a library brings is there after `ruby.require` of the library\n\
             10 an Array has no method `nothing`\n\
             11 an Array has no method `frozen`. It has `frozen?`, which is written `.send(\"frozen?\")` in Zeno\n\
             12 the module Math has no method `nothing`\n\
             13 the class Array is not called in Ruby. An object of it is made with `.new(...)`\n\
             14 an Array is not a method, and cannot be called\n\
             16 true\n\
             17 a dict in Zeno has keys that are str or int, and this one has the key 1.5\n\
             18 a Range that has no end cannot be gone through with `for`. `.first(n)` takes the first n of it\n\
             19 an Object cannot be gone through with `for`\n\
             20 an Object has no length\n\
             21 an Object cannot be asked what is `in` it\n\
             22 true\n\
             method `size` of an Array. In Zeno a call has brackets: `.size()` still here\n"
        );
        assert_eq!(
            error("import ruby\nfunction double(n) { return n * 2 }\nruby.constant(\"Array\").new(2, 0).map(double)"),
            "a function of Zeno cannot be given to Ruby yet"
        );
        assert_eq!(
            error("import ruby\nif ruby.constant(\"Array\").new() { print(1) }"),
            "a condition must be true or false, but this is a Ruby value"
        );
        assert_eq!(error("import ruby\nruby.constant()"), "`constant` takes 1 argument, but 0 were given");
        assert_eq!(error("import ruby\nruby.require(3)"), "`require` needs a str, but this is an int");
        assert_eq!(error("import ruby\nruby.symbol(3)"), "`symbol` needs a str, but this is an int");
        // Each stays where it was made.
        if python_is_here() {
            assert_eq!(
                error("import ruby\nimport python\npython.module(\"json\").dumps(ruby.constant(\"Array\").new())"),
                "a Ruby value cannot be given to Python. Each stays where it was made"
            );
        }
    }

    /// Is there a PHP on this machine for `import php` to start? The
    /// tests use named arguments, which PHP has from 8.
    fn php_is_here() -> bool {
        std::process::Command::new("php")
            .args(["-r", "exit(PHP_MAJOR_VERSION >= 8 ? 0 : 1);"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_class_of_php_is_used_from_zeno() {
        if !php_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own, and so does an array,
        // as a list or a dict: in PHP an array is a value. Calling a class
        // makes an object, a dot after a class is a constant or what is
        // `static` there, a named argument is a named argument, and a
        // function that changes what it is given is given a variable.
        assert_eq!(
            output(
                "import php\n\
                 let strrev = php.callable(\"strrev\")\n\
                 print(strrev(\"abc\"), php.callable(\"str_repeat\")(\"ab\", 3), php.callable(\"intdiv\")(7, 2), php.callable(\"sqrt\")(16), php.callable(\"strlen\")(\"h\u{e9}llo\"), php.callable(\"ucfirst\")(\"zeno \u{1f600}\\t\\\"q\\\"\"))\n\
                 print(php.constant(\"PHP_INT_MAX\"), php.constant(\"M_PI\") > 3.14, php.constant(\"PHP_VERSION\") != \"\", php.constant(\"DateTime::ATOM\"), php.constant(\"NAN\"), php.constant(\"INF\"), php.callable(\"is_nan\")(php.constant(\"NAN\")))\n\
                 let DateTime = php.type(\"DateTime\")\n\
                 let utc = php.type(\"DateTimeZone\")(\"UTC\")\n\
                 let day = DateTime(\"2026-11-18\", utc)\n\
                 print(day.format(\"l\"), day.format(\"Y\") + \"!\", DateTime.ATOM, day.getTimestamp() + 1, day == DateTime(\"2026-11-18\", utc), day == DateTime(\"2026-11-19\", utc), day == 3)\n\
                 day.modify(\"+1 month\")\n\
                 let later = day.format\n\
                 print(later(\"Y-m-d\"), DateTime.createFromFormat(\"Y-m-d\", \"2026-01-02\", utc).format(\"D\"), php.callable(\"DateTime::createFromFormat\")(\"Y\", \"2027\", utc).format(\"Y\"), DateTime[\"createFromFormat\"](\"Y\", \"2028\", utc).format(\"Y\"))\n\
                 print(php.callable(\"htmlspecialchars\")(\"<a>&amp;\", double_encode=false), php.callable(\"number_format\")(1234.5, 2, thousands_separator=\" \"), php.callable(\"round\")(2.5), php.callable(\"floor\")(7), php.callable(\"max\")(1, 2.5), php.callable(\"str_pad\")(\"7\", 3, \"0\", php.constant(\"STR_PAD_LEFT\")))\n\
                 let parts = php.callable(\"explode\")(\",\", \"a,b,c\")\n\
                 parts.append(\"d\")\n\
                 print(parts, len(parts), php.callable(\"implode\")(\"-\", parts), php.callable(\"array_flip\")([\"a\", \"b\"]), php.callable(\"array_sum\")([1, 2, 3.5]), php.callable(\"array_keys\")([\"x\": 1, \"y\": 2]), php.callable(\"range\")(1, 3))\n\
                 print(php.callable(\"json_decode\")(\"{\\\"a\\\": [1, 2.0, null, true, {\\\"k\\\": \\\"v\\\"}], \\\"e\\\": []}\", true), php.callable(\"json_encode\")([\"a\": [1, 2.5], \"b\": [3]]))\n\
                 let found = php.variable()\n\
                 print(php.callable(\"preg_match\")(\"/(\\\\d+)-(\\\\d+)/\", \"tel 12-34\", found), found.value, found.value[2] + \"!\")\n\
                 let numbers = php.variable([3, 1, 2])\n\
                 php.callable(\"sort\")(numbers)\n\
                 numbers.value = numbers.value + [0]\n\
                 print(numbers.value, php.copy(numbers), php.callable(\"count\")(numbers), php.callable(\"array_reverse\")(numbers))"
            ),
            "cba ababab 3 4.0 6 Zeno \u{1f600}\t\"q\"\n\
             9223372036854775807 true true Y-m-d\\TH:i:sP NaN inf true\n\
             Wednesday 2026! Y-m-d\\TH:i:sP 1794960001 true false false\n\
             2026-12-18 Fri 2027 2028\n\
             &lt;a&gt;&amp; 1 234.50 3.0 7.0 2.5 007\n\
             [\"a\", \"b\", \"c\", \"d\"] 4 a-b-c-d [\"a\": 0, \"b\": 1] 6.5 [\"x\", \"y\"] [1, 2, 3]\n\
             [\"a\": [1, 2.0, none, true, [\"k\": \"v\"]], \"e\": []] {\"a\":[1,2.5],\"b\":[3]}\n\
             1 [\"12-34\", \"12\", \"34\"] 34!\n\
             [1, 2, 3, 0] [1, 2, 3, 0] 4 [0, 3, 2, 1]\n"
        );
    }

    #[test]
    fn what_is_not_a_value_stays_in_php() {
        if !php_is_here() {
            return;
        }
        // An object of PHP is the one object there is. `php.copy` makes a
        // list or a dict of what can be gone through, and of an object that
        // only holds values. A string that is not text stays in PHP as bytes.
        assert_eq!(
            output(
                "import php\n\
                 let list = php.type(\"ArrayObject\")([3, 1, 2])\n\
                 list[0] = 9\n\
                 list.append(7)\n\
                 print(len(list), list[0] + 1, 1 in list, 9 in list, php.copy(list), list.count(), list == list, list == php.type(\"ArrayObject\")([9, 1, 2, 7]), list == 3)\n\
                 var total = 0\n\
                 for item in list { total = total + item }\n\
                 print(total)\n\
                 let ages = php.type(\"ArrayObject\")([\"ann\": 31, \"bob\": 40])\n\
                 ages[\"cy\"] = [1, [2.5, \"x\", none], [\"k\": [true]]]\n\
                 print(len(ages), ages[\"ann\"], \"ann\" in ages, \"zed\" in ages, ages[\"cy\"][1][1], ages[\"cy\"][2][\"k\"])\n\
                 for name, age in php.copy(ages) { print(name, age) }\n\
                 for value in php.type(\"ArrayIterator\")([\"a\": 1, \"b\": 2]) { print(value) }\n\
                 let thing = php.callable(\"json_decode\")(\"{\\\"a\\\": 1, \\\"b\\\": {\\\"c\\\": [1]}, \\\"for\\\": 2}\")\n\
                 thing.a = thing.a + 4\n\
                 thing.z = \"new\"\n\
                 thing[\"for\"] = thing[\"for\"] * 2\n\
                 print(thing.a, thing.b.c, thing[\"z\"], php.copy(thing), php.copy(3), php.copy(\"as it is\"))\n\
                 let back = php.type(\"Closure\").fromCallable(\"strrev\")\n\
                 print(back(\"abc\"), php.callable(\"call_user_func\")([php.type(\"DateTime\")(\"2026-11-18\"), \"format\"], \"Y\"), php.callable(\"is_callable\")(back))\n\
                 let bytes = php.callable(\"hex2bin\")(\"ff006162\")\n\
                 var sum = 0\n\
                 for byte in bytes { sum = sum + byte }\n\
                 print(len(bytes), bytes[0], bytes[3], sum, php.callable(\"bin2hex\")(bytes), bytes == php.callable(\"hex2bin\")(\"ff006162\"), bytes, php.callable(\"hex2bin\")(\"6869\") + \"!\")\n\
                 let heap = php.type(\"SplMinHeap\")()\n\
                 heap.insert(3)\n\
                 heap.insert(1)\n\
                 print(heap.extract(), len(heap), heap.isEmpty(), php.type(\"SplMinHeap\"), heap.top, php.variable(5))"
            ),
            "4 10 true false [9, 1, 2, 7] 4 true true false\n\
             19\n\
             3 31 true false x [true]\n\
             ann 31\n\
             bob 40\n\
             cy [1, [2.5, \"x\", none], [\"k\": [true]]]\n\
             1\n\
             2\n\
             5 [1] new [\"a\": 5, \"b\": [\"c\": [1]], \"for\": 4, \"z\": \"new\"] 3 as it is\n\
             cba 2026 true\n\
             4 255 98 450 ff006162 true \"\\xFF\\x00ab\" hi!\n\
             1 1 false SplMinHeap the method `top` of a SplMinHeap a variable of PHP that holds an int\n"
        );
    }

    #[test]
    fn an_error_of_php_is_an_error_of_zeno() {
        if !php_is_here() {
            return;
        }
        // An exception has Zeno's kind where the two mean one thing, a
        // warning is raised, a file that cannot be used is a FileError, and
        // every other is an `Error` that says what PHP called it. What PHP
        // words is not compared.
        assert_eq!(
            output(
                "import php\n\
                 let list = php.type(\"ArrayObject\")([1])\n\
                 try { php.callable(\"intdiv\")(1, 0) } except ZeroDivisionError as error { print(1, error.message != \"\") }\n\
                 try { php.callable(\"str_repeat\")(\"x\", -1) } except ValueError as error { print(2, error.message != \"\") }\n\
                 try { php.type(\"ArrayIterator\")([1]).seek(5) } except IndexError as error { print(3, error.message.starts_with(\"OutOfBoundsException: \")) }\n\
                 try { php.callable(\"file_get_contents\")(\"/zeno/no/such/file\") } except FileError as error { print(4, error.message.starts_with(\"file_get_contents(/zeno/no/such/file): \")) }\n\
                 try { php.type(\"SplFileObject\")(\"/zeno/no/such/file\") } except FileError as error { print(5, error.message.starts_with(\"RuntimeException: \")) }\n\
                 try { php.callable(\"strlen\")([1]) } except Error as error { print(6, error.kind, error.message.starts_with(\"TypeError: \")) }\n\
                 try { php.callable(\"json_decode\")(\"{oops\", flags=php.constant(\"JSON_THROW_ON_ERROR\")) } except ValueError as error { print(7, error.message.starts_with(\"JsonException: \")) }\n\
                 try { php.callable(\"trigger_error\")(\"careful\", php.constant(\"E_USER_WARNING\")) } except Error as error { print(8, error.kind, error.message) }\n\
                 try { php.type(\"Zeno\\\\NoSuch\") } except FileError as error { print(9, error.message.starts_with(\"PHP has no class `Zeno\\\\NoSuch`. Is its package installed in this folder (`composer require`)? This folder is \")) }\n\
                 try { php.callable(\"zeno_no_such\") } except Error as error { print(10, error.message) }\n\
                 try { php.callable(\"echo\") } except Error as error { print(11, error.message) }\n\
                 try { php.constant(\"ZENO_NO_SUCH\") } except Error as error { print(12, error.message) }\n\
                 try { php.require(\"zeno-no-such.php\") } except FileError as error { print(13, error.message.starts_with(\"there is no file `\"), error.message.ends_with(\"zeno-no-such.php`\")) }\n\
                 try { php.callable(\"sort\")([3, 1, 2]) } except Error as error { print(14, error.message) }\n\
                 try { list.nothing() } except Error as error { print(15, error.message) }\n\
                 try { php.type(\"ArrayObject\").nothing } except Error as error { print(16, error.message) }\n\
                 try { php.type(\"Countable\")() } except Error as error { print(17, error.message) }\n\
                 try { php.type(\"Closure\")() } except Error as error { print(18, error.message) }\n\
                 try { list() } except Error as error { print(19, error.message) }\n\
                 try { php.type(\"DateTime\")()[0] } except Error as error { print(20, error.message) }\n\
                 try { php.type(\"DateTime\")()[\"zone\"] } except Error as error { print(21, error.message) }\n\
                 try { for x in php.type(\"DateTime\")() { print(x) } } except Error as error { print(22, error.message) }\n\
                 try { len(php.type(\"DateTime\")()) } except Error as error { print(23, error.message) }\n\
                 try { print(1 in php.type(\"DateTime\")()) } except Error as error { print(24, error.message) }\n\
                 try { php.callable(\"hex2bin\")(\"ff\")[3] } except IndexError as error { print(25, error.message) }\n\
                 print(php.callable(\"error_reporting\")() > 0, \"still here\")"
            ),
            "1 true\n\
             2 true\n\
             3 true\n\
             4 true\n\
             5 true\n\
             6 Error true\n\
             7 true\n\
             8 Error careful\n\
             9 true\n\
             10 PHP has no function `zeno_no_such`. One that an extension or a package brings is there when that is installed\n\
             11 `echo` is part of the language of PHP and not a function, so it cannot be called from Zeno\n\
             12 PHP has no constant `ZENO_NO_SUCH`\n\
             13 true true\n\
             14 `sort` changes its `array` (PHP writes `&$array`), so it needs a variable of PHP to change: `php.variable(value)`\n\
             15 an ArrayObject has no property or method `nothing`\n\
             16 the class ArrayObject has no constant, static property or static method `nothing`\n\
             17 `Countable` is an interface, and no object can be made of it by calling it\n\
             18 `Closure` is a class that only PHP itself makes objects of, and no object can be made of it by calling it\n\
             19 an ArrayObject is not a function, and cannot be called\n\
             20 a DateTime has no items\n\
             21 a DateTime has no property or method `zone`\n\
             22 a DateTime cannot be gone through with `for`\n\
             23 a DateTime has no length\n\
             24 a DateTime cannot be asked what is `in` it\n\
             25 there is no byte 3 in 1 bytes\n\
             true still here\n"
        );
        assert_eq!(
            error("import php\nfunction double(n) { return n * 2 }\nphp.callable(\"array_map\")(double, [1])"),
            "a function of Zeno cannot be given to PHP yet"
        );
        assert_eq!(
            error("import php\nif php.type(\"ArrayObject\")() { print(1) }"),
            "a condition must be true or false, but this is a PHP value"
        );
        assert_eq!(error("import php\nphp.type()"), "`type` takes 1 argument, but 0 were given");
        assert_eq!(error("import php\nphp.callable(3)"), "`callable` needs a str, but this is an int");
        assert_eq!(error("import php\nphp.variable(1, 2)"), "`variable` takes 0 to 1 arguments, but 2 were given");
        // Each stays where it was made.
        if python_is_here() {
            assert_eq!(
                error("import php\nimport python\npython.module(\"json\").dumps(php.type(\"ArrayObject\")())"),
                "a PHP value cannot be given to Python. Each stays where it was made"
            );
        }
    }

    /// Is there a Lua on this machine for `import lua` to start? The
    /// first of the names Zeno tries is the one the tests use.
    fn lua_is_here() -> bool {
        std::process::Command::new("lua")
            .args(["-e", "os.exit(0)"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// A program that uses `helper.lua`, a module of Lua written for the
    /// tests, which is put in a folder of its own.
    fn with_lua_helper(source: &str) -> String {
        let folder = std::env::temp_dir().join(format!("zeno-lua-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&folder);
        let helper = "local M = {}\n\
              M.version = \"1.0\"\n\
              local Point = {}\n\
              Point.__index = Point\n\
              function M.Point(x, y) return setmetatable({ x = x, y = y }, Point) end\n\
              function Point:length() return math.sqrt(self.x * self.x + self.y * self.y) end\n\
              function Point:moved(dx, dy) return M.Point(self.x + dx, self.y + dy) end\n\
              Point.__tostring = function (p) return \"Point(\" .. p.x .. \", \" .. p.y .. \")\" end\n\
              Point.__eq = function (a, b) return a.x == b.x and a.y == b.y end\n\
              function M.greet(name, options)\n\
                options = options or {}\n\
                local parts = {}\n\
                for i = 1, options.times or 1 do parts[i] = \"hello \" .. name end\n\
                local text = table.concat(parts, \", \")\n\
                if options.loud then text = text:upper() end\n\
                return text\n\
              end\n\
              function M.fail() error(\"it broke\") end\n\
              function M.fail_table() error({ code = 7 }) end\n\
              function M.nothing() end\n\
              function M.many() return 1, \"two\", nil end\n\
              function M.bytes() return \"\\255\\0ab\" end\n\
              function M.sum(list) local s = 0 for _, v in ipairs(list) do s = s + v end return s end\n\
              function M.list() return { 3, 1, 2 } end\n\
              function M.ages() return { ann = 31, bob = 40 } end\n\
              function M.echo(t) return t end\n\
              function M.odd() return { [1.5] = 2 } end\n\
              return M\n";
        std::fs::write(folder.join("helper.lua"), helper).expect("the folder of the tests can be written");
        source.replace("{folder}", &folder.to_string_lossy().replace('\\', "/"))
    }

    #[test]
    fn a_module_of_lua_is_used_from_zeno() {
        if !lua_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own. A function read from
        // an object is called with the object as `self`, as `object:name()`
        // is in Lua, and one read from a module is not. Several values
        // given back are a list, and named arguments are the table of
        // options that Lua takes last.
        assert_eq!(
            output(&with_lua_helper(
                "import lua\n\
                 let package = lua.global(\"package\")\n\
                 package.path = \"{folder}/?.lua;\" + package.path\n\
                 let string = lua.global(\"string\")\n\
                 let math = lua.global(\"math\")\n\
                 print(string.format(\"%5.1f|%s|%d\", 3.14159, \"h\u{e9}llo \u{1f600}\\t\\\"q\\\"\", 42), string.rep(\"ab\", 3), string.upper(\"zeno\"), string.len(\"h\u{e9}llo\"))\n\
                 print(math.pi > 3.14, math.floor(2.5), math.sqrt(2.0) > 1.41, math.huge, -math.huge, math.max(1, 7, 3), math.abs(-2 ** 40))\n\
                 print(string.byte(\"abc\", 1, 3), string.find(\"hello\", \"l\"), string.gsub(\"hello\", \"l\", \"L\")[0], string[\"match\"](\"key=value\", \"(%w+)=(%w+)\"), string.find(\"hello\", \"z\"))\n\
                 let helper = lua.require(\"helper\")\n\
                 print(helper.version, helper.greet(\"ann\"), helper.greet(\"bob\", times=2, loud=true), helper.sum([1, 2, 3.5]), helper.nothing(), helper.many())\n\
                 let p = helper.Point(3, 4)\n\
                 print(p, p.length() == 5, p.x + p.y, p.moved(1, 1), p == helper.Point(3, 4), p == helper.Point(0, 0), p == 3, p[\"length\"](p) == 5)\n\
                 p.x = 6\n\
                 p[\"y\"] = 8\n\
                 print(p.length() == 10, p.x, p[\"y\"], lua.copy(p)[\"x\"])\n\
                 let later = p.moved\n\
                 print(later(1, 2), later)\n\
                 let os = lua.global(\"os\")\n\
                 print(os.time() > 0, os.getenv(\"HOME\") != none, lua.global(\"_VERSION\").starts_with(\"Lua\"), lua.global(\"type\")(p), lua.global(\"tostring\")(12))"
            )),
            "  3.1|h\u{e9}llo \u{1f600}\t\"q\"|42 ababab ZENO 6\n\
             true 2 true inf -inf 7 1099511627776\n\
             [97, 98, 99] [3, 3] heLLo [\"key\", \"value\"] none\n\
             1.0 hello ann HELLO BOB, HELLO BOB 6.5 none [1, \"two\"]\n\
             Point(3, 4) true 7 Point(4, 5) true false false true\n\
             true 6 8 6\n\
             Point(7, 10) method `moved` of a table\n\
             true true true table 12\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_lua() {
        if !lua_is_here() {
            return;
        }
        // A table of Lua is the one table there is: its items are counted
        // from 1, as Lua counts them. `lua.copy` makes a list of a table
        // whose keys are 1, 2, ... n, and a dict of any other. Bytes that are
        // not text stay in Lua.
        assert_eq!(
            output(&with_lua_helper(
                "import lua\n\
                 let package = lua.global(\"package\")\n\
                 package.path = \"{folder}/?.lua;\" + package.path\n\
                 let table = lua.global(\"table\")\n\
                 let helper = lua.require(\"helper\")\n\
                 let list = helper.list()\n\
                 table.insert(list, 9)\n\
                 table.sort(list)\n\
                 print(len(list), list[1], list[4], 9 in list, 5 in list, lua.copy(list), table.concat(list, \"-\"), list)\n\
                 var total = 0\n\
                 for item in list { total = total + item }\n\
                 print(total)\n\
                 list[1] = \"x\"\n\
                 print(list[1], lua.copy(list))\n\
                 let ages = helper.ages()\n\
                 ages[\"cy\"] = [1, [2.5, \"x\"], [\"k\": [true]]]\n\
                 print(len(ages), ages[\"ann\"], \"ann\" in ages, \"zed\" in ages, ages[\"zed\"], ages[\"cy\"][2][2], ages.cy[3][\"k\"][1])\n\
                 var keys = 0\n\
                 for key in ages { keys = keys + 1 }\n\
                 print(keys)\n\
                 for name, age in lua.copy(ages) { if name == \"ann\" { print(name, age) } }\n\
                 let given = helper.echo\n\
                 print(lua.copy(given([1, none, 3])), lua.copy(given([\"a\": 1])), given([]), lua.copy(given([])), lua.copy(3), lua.copy(\"as it is\"))\n\
                 let bytes = helper.bytes()\n\
                 var sum = 0\n\
                 for byte in bytes { sum = sum + byte }\n\
                 print(len(bytes), sum, bytes, lua.global(\"string\").byte(bytes, 1), bytes == helper.bytes())\n\
                 print(lua.global(\"math\").pow(2, 63) > 9.2e18, 2 ** 62, helper.sum([2 ** 52, 1]))"
            )),
            "4 1 9 true false [1, 2, 3, 9] 1-2-3-9 {1, 2, 3, 9}\n\
             15\n\
             x [\"x\", 2, 3, 9]\n\
             0 31 true false none x true\n\
             3\n\
             ann 31\n\
             [1] [\"a\": 1] {} [] 3 as it is\n\
             4 450 \"\\xFF\\x00ab\" 255 true\n\
             true 4611686018427387904 4503599627370497\n"
        );
    }

    #[test]
    fn an_error_of_lua_is_an_error_of_zeno() {
        if !lua_is_here() {
            return;
        }
        // Lua has no kinds of error, so an error of Lua is an `Error` that says
        // what Lua said. `nil` and a message, which is how most functions of
        // Lua say they failed, is one too, and a `FileError` where a number of
        // the system comes with them.
        assert_eq!(
            output(&with_lua_helper(
                "import lua\n\
                 let package = lua.global(\"package\")\n\
                 package.path = \"{folder}/?.lua;\" + package.path\n\
                 let helper = lua.require(\"helper\")\n\
                 try { helper.fail() } except Error as error { print(1, error.kind, error.message.ends_with(\"it broke\")) }\n\
                 try { helper.fail_table() } except Error as error { print(2, error.message.starts_with(\"table\")) }\n\
                 try { lua.global(\"io\").open(\"/zeno/no/such/file\") } except FileError as error { print(3, error.message.starts_with(\"/zeno/no/such/file: \")) }\n\
                 try { lua.global(\"os\").rename(\"/zeno/no/such/a\", \"/zeno/no/such/b\") } except FileError as error { print(4, error.message != \"\") }\n\
                 try { lua.global(\"error\")(\"plain\") } except Error as error { print(5, error.message) }\n\
                 try { lua.require(\"zeno_no_such\") } except FileError as error { print(6, error.message.starts_with(\"Lua has no module `zeno_no_such`. Is it installed for this Lua (`luarocks install`)? It is Lua\")) }\n\
                 try { lua.global(\"zeno_no_such\") } except Error as error { print(7, error.message) }\n\
                 try { helper.nothing.x } except Error as error { print(8, error.message) }\n\
                 try { helper() } except Error as error { print(10, error.message) }\n\
                 try { len(helper.fail) } except Error as error { print(11, error.message) }\n\
                 try { for x in lua.global(\"io\").stdout { print(x) } } except Error as error { print(12, error.message) }\n\
                 try { print(1 in helper.fail) } except Error as error { print(13, error.message) }\n\
                 try { lua.copy(helper.odd()) } except ValueError as error { print(14, error.message) }\n\
                 try { lua.global(\"string\").rep() } except Error as error { print(15, error.message.starts_with(\"bad argument #1 to \")) }\n\
                 print(helper.nothing() == none, \"still here\")"
            )),
            "1 Error true\n\
             2 true\n\
             3 true\n\
             4 true\n\
             5 plain\n\
             6 true\n\
             7 Lua has no global `zeno_no_such`\n\
             8 a function has no field `x`\n\
             10 a table is not a function, and cannot be called\n\
             11 a function has no length\n\
             12 a file cannot be gone through with `for`\n\
             13 a function cannot be asked what is `in` it\n\
             14 a dict in Zeno has keys that are str or int, and this one has the key 1.5\n\
             15 true\n\
             true still here\n"
        );
        assert_eq!(
            error("import lua\nfunction double(n) { return n * 2 }\nlua.global(\"table\").sort(lua.global(\"package\").loaded, double)"),
            "a function of Zeno cannot be given to Lua yet"
        );
        assert_eq!(
            error("import lua\nif lua.global(\"table\") { print(1) }"),
            "a condition must be true or false, but this is a Lua value"
        );
        assert_eq!(error("import lua\nlua.require()"), "`require` takes 1 argument, but 0 were given");
        assert_eq!(error("import lua\nlua.global(3)"), "`global` needs a str, but this is an int");
        // Each stays where it was made.
        if python_is_here() {
            assert_eq!(
                error("import lua\nimport python\npython.module(\"json\").dumps(lua.global(\"table\"))"),
                "a Lua value cannot be given to Python. Each stays where it was made"
            );
        }
    }

    /// Is there a .NET on this machine that can build the helper of
    /// `import csharp`? That takes an SDK, not only the runtime.
    fn dotnet_is_here() -> bool {
        std::process::Command::new("dotnet")
            .arg("--list-sdks")
            .output()
            .is_ok_and(|said| said.status.success() && !said.stdout.is_empty())
    }

    #[test]
    fn a_type_of_dotnet_is_used_from_zeno() {
        if !dotnet_is_here() {
            return;
        }
        // Numbers and text come across as Zeno's own. A type is named with its
        // namespace, or as C# writes it (`string`, `List<int>`); calling it makes
        // an object, and a dot after it is what is `static`. Of the methods of one
        // name, the one the arguments fit is called, a named argument is given to
        // the parameter of that name, and a `Task` is waited for.
        assert_eq!(
            output(
                "import csharp\n\
                 let Builder = csharp.type(\"System.Text.StringBuilder\")\n\
                 let builder = Builder(\"ab\")\n\
                 builder.Append(\"c\").Append(42).Append(1.5).Append(true)\n\
                 print(builder, builder.Length, builder.ToString().upper(), builder.Capacity >= 9)\n\
                 let math = csharp.type(\"System.Math\")\n\
                 print(math.PI, math.Max(3, 7), math.Max(2, 3.5), math.Round(2.567, 2), math.Round(2.567, digits=1), math.Abs(-4), math.Sqrt(16))\n\
                 let text = csharp.type(\"string\")\n\
                 print(text.Join(\"-\", [\"x\", \"y\"]), text.Join(\"+\", \"a\", \"b\", \"c\"), text.Join(separator=\", \", values=[\"p\", \"q\"]), text.Empty == \"\", text.Concat(\"h\u{e9}\", \"\u{1f600}\"))\n\
                 let day = csharp.type(\"System.DateTime\")(2026, 11, 18)\n\
                 print(day.DayOfWeek, day.AddDays(10).ToString(\"yyyy-MM-dd\"), day.Year + 1, day == csharp.type(\"System.DateTime\")(2026, 11, 18), day.DayOfWeek == csharp.type(\"System.DayOfWeek\").Wednesday)\n\
                 let bytes = csharp.type(\"System.Text.Encoding\").UTF8.GetBytes(\"zeno\")\n\
                 print(csharp.type(\"System.Convert\").ToBase64String(bytes), len(bytes), bytes[0], csharp.copy(bytes))\n\
                 print(csharp.type(\"long\").MaxValue, csharp.type(\"System.Int32\").Parse(\"12\") + 1, csharp.type(\"double\").NaN, csharp.type(\"double\").PositiveInfinity, csharp.type(\"System.Guid\").Empty)\n\
                 print(csharp.type(\"System.Threading.Tasks.Task\").Delay(1), csharp.type(\"System.Threading.Tasks.Task\").FromResult(7))\n\
                 let builder2 = Builder()\n\
                 builder2.Capacity = 100\n\
                 print(builder2.Capacity, builder2.Length, csharp.type(\"System.Text.StringBuilder\")[\"Equals\"](builder2, builder2))"
            ),
            "abc421.5True 12 ABC421.5TRUE true\n\
             3.141592653589793 7 3.5 2.57 2.6 4 4.0\n\
             x-y a+b+c p, q true h\u{e9}\u{1f600}\n\
             Wednesday 2026-11-28 2027 true true\n\
             emVubw== 4 122 [122, 101, 110, 111]\n\
             9223372036854775807 13 NaN inf 00000000-0000-0000-0000-000000000000\n\
             none 7\n\
             100 0 true\n"
        );
    }

    #[test]
    fn what_is_not_a_number_or_text_stays_in_dotnet() {
        if !dotnet_is_here() {
            return;
        }
        // A collection of .NET is the one collection there is: its items are
        // reached from Zeno, and a list or a dict that Zeno gives is made into
        // the one the method wants. `csharp.copy` makes a list or a dict of Zeno.
        assert_eq!(
            output(
                "import csharp\n\
                 let list = csharp.type(\"System.Collections.Generic.List<int>\")()\n\
                 list.Add(3)\n\
                 list.Add(1)\n\
                 list.AddRange([5, 2])\n\
                 list.Sort()\n\
                 list[0] = 9\n\
                 print(list, len(list), list[0] + 1, 2 in list, 4 in list, list.Count, list.Contains(5), csharp.copy(list))\n\
                 var total = 0\n\
                 for item in list {\n\
                     total = total + item\n\
                 }\n\
                 print(total)\n\
                 let ages = csharp.type(\"System.Collections.Generic.Dictionary<string, System.Collections.Generic.List<int>>\")()\n\
                 ages[\"ann\"] = [31, 2]\n\
                 ages.Add(\"bob\", [])\n\
                 print(ages, len(ages), ages[\"ann\"][1], \"ann\" in ages, \"zed\" in ages, ages.ContainsKey(\"bob\"), csharp.copy(ages))\n\
                 for name in ages {\n\
                     print(name)\n\
                 }\n\
                 for name, numbers in csharp.copy(ages) {\n\
                     print(name, numbers)\n\
                 }\n\
                 let array = csharp.type(\"int[]\")(3)\n\
                 let made = csharp.type(\"System.Array\").CreateInstance(csharp.type(\"string\"), 2)\n\
                 made[1] = \"b\"\n\
                 print(array, made, len(made), made[1], made[0], csharp.copy(made))\n\
                 let linq = csharp.type(\"System.Linq.Enumerable\")\n\
                 print(linq.Sum([1, 2, 3]), linq.Max([1.5, 0.5]), linq.Range(1, 4), csharp.copy(linq.Range(1, 4)), linq.Repeat(\"ab\", 2), linq.Count(list))\n\
                 let set = csharp.type(\"System.Collections.Generic.HashSet<string>\")([\"a\", \"b\", \"a\"])\n\
                 print(len(set), \"a\" in set, set.Add(\"a\"), set.Add(\"c\"), len(set))\n\
                 let queue = csharp.type(\"System.Collections.Generic.Queue<double>\")()\n\
                 queue.Enqueue(1.5)\n\
                 queue.Enqueue(2)\n\
                 print(queue.Dequeue(), queue.Peek(), len(queue), list == list, list == csharp.type(\"System.Collections.Generic.List<int>\")(), list == 3)\n\
                 let pair = csharp.type(\"System.Collections.Generic.KeyValuePair<string, int>\")(\"k\", 1)\n\
                 print(pair.Key, pair.Value, csharp.type(\"System.Tuple\").Create(1, \"a\").Item2)"
            ),
            "[9, 2, 3, 5] 4 10 true false 4 true [9, 2, 3, 5]\n\
             19\n\
             {\"ann\": [31, 2], \"bob\": []} 2 2 true false true [\"ann\": [31, 2], \"bob\": []]\n\
             ann\n\
             bob\n\
             ann [31, 2]\n\
             bob []\n\
             [0, 0, 0] [null, \"b\"] 2 b none [none, \"b\"]\n\
             6 1.5 [1, 2, 3, 4] [1, 2, 3, 4] [\"ab\", \"ab\"] 4\n\
             2 true false true 3\n\
             1.5 2.0 1 true false false\n\
             k 1 a\n"
        );
    }

    #[test]
    fn an_error_of_dotnet_is_an_error_of_zeno() {
        if !dotnet_is_here() {
            return;
        }
        // An exception has Zeno's kind where the two mean one thing, and every
        // other is an `Error` that says what .NET called it. What .NET words is
        // not compared, since each version words it differently.
        assert_eq!(
            output(
                "import csharp\n\
                 let numbers = csharp.type(\"System.Collections.Generic.Dictionary<string, int>\")()\n\
                 try { numbers[\"no\"] } except KeyError as error { print(1, error.message.starts_with(\"KeyNotFoundException: \")) }\n\
                 try { csharp.type(\"System.Convert\").ToInt32(\"abc\") } except ValueError as error { print(2, error.message.starts_with(\"FormatException: \")) }\n\
                 try { csharp.type(\"System.IO.File\").ReadAllText(\"/zeno/no/such\") } except FileError as error { print(3, error.message.starts_with(\"DirectoryNotFoundException\")) }\n\
                 try { csharp.type(\"Zeno.No.Such\") } except FileError as error { print(4, error.message) }\n\
                 try { csharp.type(\"System.Math\").Max(\"a\", 2) } except Error as error { print(5, error.message.starts_with(\"`Max` has none that takes a str, an int. There is Max(byte val1, byte val2), \")) }\n\
                 try { csharp.type(\"System.Math\").Nope } except Error as error { print(6, error.message) }\n\
                 try { csharp.type(\"System.Collections.Generic.List<int>\")()[3] } except IndexError as error { print(7, error.message.starts_with(\"ArgumentOutOfRangeException\")) }\n\
                 try { csharp.type(\"System.Int32\").Parse(\"99999999999\") } except OverflowError as error { print(8, error.message.starts_with(\"OverflowException\")) }\n\
                 try { csharp.type(\"System.Decimal\").Divide(1, 0) } except ZeroDivisionError as error { print(9, error.message.starts_with(\"DivideByZeroException\")) }\n\
                 try { csharp.type(\"System.Text.StringBuilder\")().Length = \"x\" } except Error as error { print(10, error.message) }\n\
                 try { csharp.type(\"System.IO.Stream\")() } except Error as error { print(11, error.message) }\n\
                 try { csharp.type(\"System.Math\").Round(2.5, places=1) } except Error as error { print(12, error.message.starts_with(\"`Round` has none that takes a float and `places`. \")) }\n\
                 try { csharp.assembly(\"zeno-no-such.dll\") } except FileError as error { print(13, error.message.starts_with(\"there is no .dll or folder `\"), error.message.ends_with(\"zeno-no-such.dll`\")) }\n\
                 try { csharp.type(\"List<int, int>\") } except Error as error { print(14, error.message) }\n\
                 print(csharp.type(\"System.Math\").Max)"
            ),
            "1 true\n\
             2 true\n\
             3 true\n\
             4 .NET has no type `Zeno.No.Such`. A type of a package is there once the project beside the program has the package (`dotnet add package`), and one of a .dll after `csharp.assembly`\n\
             5 true\n\
             6 the class System.Math has no static property, field or method `Nope`\n\
             7 true\n\
             8 true\n\
             9 true\n\
             10 `Length` is an int, and this is a str\n\
             11 `Stream` is abstract, and no object can be made of it by calling it\n\
             12 true\n\
             13 true true\n\
             14 .NET has no type `List<int, int>`. A type of a package is there once the project beside the program has the package (`dotnet add package`), and one of a .dll after `csharp.assembly`\n\
             method `Max` of the type System.Math. In Zeno a call has brackets: `.Max()`\n"
        );
        assert_eq!(
            error("import csharp\nfunction f(x: int) -> int {\n    return x\n}\ncsharp.type(\"System.Collections.Generic.List<int>\")().ForEach(f)"),
            "a function of Zeno cannot be given to C# yet"
        );
        assert_eq!(
            error("import csharp\nif csharp.type(\"System.Text.StringBuilder\")() {\n    print(1)\n}"),
            "a condition must be true or false, but this is a C# value"
        );
    }

    /// Is there a C compiler for `import c` to build with?
    fn cc_is_here() -> bool {
        crate::bridge_c::helper().is_ok()
    }

    /// A program that uses `helper.c`, a file of C written for the
    /// tests, which is put in a folder of its own.
    fn with_c_helper(source: &str) -> String {
        let folder = std::env::temp_dir().join(format!("zeno-c-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&folder);
        let helper = "#include <stdint.h>\n\
              #include <stdlib.h>\n\
              #include <string.h>\n\
              typedef struct counter { int count; } counter;\n\
              counter *counter_new(void) { counter *c = calloc(1, sizeof *c); return c; }\n\
              void counter_add(counter *c, int by) { c->count += by; }\n\
              int counter_get(const counter *c) { return c->count; }\n\
              void counter_free(counter *c) { free(c); }\n\
              uint16_t crc16(const uint8_t *data, size_t length) {\n\
                  uint16_t crc = 0xFFFF;\n\
                  for (size_t i = 0; i < length; i++) {\n\
                      crc ^= data[i];\n\
                      for (int b = 0; b < 8; b++) crc = (crc & 1) ? (crc >> 1) ^ 0xA001 : crc >> 1;\n\
                  }\n\
                  return crc;\n\
              }\n\
              const char *greet(const char *name) { static char out[64]; strcpy(out, \"hello, \"); strncat(out, name, 50); return out; }\n\
              float half(float x) { return x / 2; }\n\
              uint64_t biggest(void) { return UINT64_MAX; }\n\
              int8_t same8(int8_t x) { return x; }\n\
              _Bool positive(int x) { return x > 0; }\n\
              void fill(double *out, int n) { for (int i = 0; i < n; i++) out[i] = i * 1.5; }\n\
              void bump(uint8_t *bytes, size_t n) { for (size_t i = 0; i < n; i++) bytes[i] += STEP; }\n\
              void *nothing(void) { return NULL; }\n\
              const char *odd(void) { return \"\\377ab\"; }\n";
        std::fs::write(folder.join("helper.c"), helper).expect("the folder of the tests can be written");
        source.replace("{folder}", &folder.to_string_lossy().replace('\\', "/"))
    }

    #[test]
    fn a_library_of_c_is_used_from_zeno() {
        if !cc_is_here() {
            return;
        }
        // The functions are the ones declared, as C declares them. A
        // number goes in as the type the function takes, and comes back
        // as Zeno's own.
        assert_eq!(
            output(&with_c_helper(
                "import c\n\
                 let m = c.library(\"m\", [\"double sqrt(double x);\", \"double frexp(double x, int *exponent);\", \"int abs(int);\"])\n\
                 let e = [0]\n\
                 print(m.sqrt(2.0), m.frexp(8.0, e), e, m.abs(-5), m.sqrt(16.0))\n\
                 let mine = c.library([\"{folder}/helper.c\", \"-DSTEP=3\"], [\n\
                     \"/* An opaque struct: Zeno keeps the pointer and never looks inside. */ typedef struct counter counter;\",\n\
                     \"counter *counter_new(void);\",\n\
                     \"void counter_add(counter *c, int by);\",\n\
                     \"int counter_get(const counter *c);\",\n\
                     \"void counter_free(counter *c);\",\n\
                     \"uint16_t crc16(const uint8_t *data, size_t length);\",\n\
                     \"const char *greet(const char *name);\",\n\
                     \"float half(float x);\",\n\
                     \"uint64_t biggest(void);\",\n\
                     \"int8_t same8(int8_t x);\",\n\
                     \"bool positive(int x);\",\n\
                     \"void fill(double *out, int n);\",\n\
                     \"void bump(uint8_t *bytes, size_t n);\",\n\
                     \"void *nothing(void);\",\n\
                     \"const char *odd(void);\",\n\
                 ])\n\
                 let k = mine.counter_new()\n\
                 mine.counter_add(k, 5)\n\
                 mine.counter_add(k, 7)\n\
                 print(mine.counter_get(k), k == k, k == mine.nothing(), mine.nothing())\n\
                 mine.counter_free(k)\n\
                 let data: list[u8] = [49, 50, 51, 52, 53, 54, 55, 56, 57]\n\
                 print(mine.crc16(data, len(data)), mine.crc16(\"123456789\", 9), mine.greet(\"Partha\"))\n\
                 print(mine.half(3.0), mine.biggest(), mine.same8(-3), mine.positive(2), mine.positive(-1), mine.odd())\n\
                 let out = [0.0, 0.0, 0.0]\n\
                 mine.fill(out, 3)\n\
                 let bytes: list[u8] = [1, 2, 255]\n\
                 mine.bump(bytes, 3)\n\
                 print(out, bytes, mine.bump(bytes, 0))\n\
                 print(m, mine.crc16)"
            )),
            "1.4142135623730951 0.5 [4] 5 4.0\n\
             12 true false none\n\
             19255 19255 hello, Partha\n\
             1.5 18446744073709551615 -3 true false \u{fffd}ab\n\
             [0.0, 1.5, 3.0] [4, 5, 2] none\n\
             <C library m> uint16_t crc16(const uint8_t *data, size_t length)\n"
        );
    }

    /// A block of C that holds what a real one does: a header, a type,
    /// a table and a function of its own, and the functions it gives.
    const C_BLOCK: &str = "import c\n\
         let lib = inline c {\n    \
             #include <stdint.h>\n    \
             #include <string.h>\n\n    \
             typedef struct { int count; } counter;\n    \
             static counter only;\n    \
             static const uint16_t STEP = 0xA001;\n\n    \
             static uint16_t step(uint16_t crc) { return (crc & 1) ? (crc >> 1) ^ STEP : crc >> 1; }\n\n    \
             /* The checksum of Modbus: a } here ends nothing. */\n    \
             uint16_t crc16(const uint8_t *data, size_t length) {\n        \
                 uint16_t crc = 0xFFFF;\n        \
                 for (size_t i = 0; i < length; i++) {\n            \
                     crc ^= data[i];\n            \
                     for (int b = 0; b < 8; b++) { crc = step(crc); }\n        \
                 }\n        \
                 return crc;\n    \
             }\n\n    \
             counter *counter_get(void) { return &only; }\n    \
             int counter_add(counter *c, int by) { c->count += by; return c->count; }\n    \
             void fill(double *out, int n) { for (int i = 0; i < n; i++) { out[i] = i * 1.5; } }\n    \
             const char *closing(void) { return \"}\"; }\n\
         }\n";

    #[test]
    fn a_block_of_c_is_a_library_of_c() {
        if !cc_is_here() {
            return;
        }
        // The functions of the block are called as the functions of a
        // library are: a number checked to fit, a list lent for a
        // pointer, a pointer kept as it is, text copied.
        assert_eq!(
            output(&format!(
                "{C_BLOCK}\
                 let frame: list[u8] = [0x01, 0x03, 0x00, 0x6B, 0x00, 0x03]\n\
                 print(lib.crc16(frame, len(frame)))\n\
                 let counter = lib.counter_get()\n\
                 lib.counter_add(counter, 2)\n\
                 print(lib.counter_add(counter, 3), lib.closing(), lib)\n\
                 let out = [0.0, 0.0, 0.0]\n\
                 lib.fill(out, 3)\n\
                 print(out)\n"
            )),
            "6004\n5 } <C library inline c>\n[0.0, 1.5, 3.0]\n"
        );
        // Two blocks in one file are two libraries, and a block in a
        // function is built once however often the function runs.
        assert_eq!(
            output(
                "import c\n\
                 let a = inline c {\n    int value(void) { return 1; }\n}\n\
                 function twice(n: int) -> int {\n    \
                     let b = inline c {\n        int value(int n) { return 2 * n; }\n    }\n    \
                     return b.value(n)\n\
                 }\n\
                 print(a.value(), twice(4), twice(5))\n"
            ),
            "1 8 10\n"
        );
        // A number that does not fit what the function takes is the
        // error it is for a library.
        assert_eq!(
            error(&format!("{C_BLOCK}lib.crc16([300], 1)")),
            "uncaught OverflowError: item 0 of the list given for the 1st parameter of `crc16` (`const uint8_t *`) holds 0 to 255, and this is 300"
        );
    }

    #[test]
    fn a_block_of_c_that_does_not_compile_stops_the_program_at_its_line() {
        if !cc_is_here() {
            return;
        }
        // What the C compiler says names the line of the `.zeno` file,
        // and no `try` catches it: the program is wrong, not unlucky.
        let said = error(
            "import c\n\
             try {\n    \
                 let lib = inline c {\n        int twice(int x) { return 2 * y; }\n    }\n    \
                 print(lib.twice(2))\n\
             } except Error as wrong {\n    print(\"caught\")\n}\n",
        );
        assert!(said.starts_with("C could not build this `inline c` block. The C compiler said:"), "{said}");
        assert!(said.contains(".zeno:4:"), "{said}");
        assert!(said.contains("y"), "{said}");
    }

    #[test]
    fn a_block_of_python_is_a_module_of_python() {
        if !python_is_here() {
            return;
        }
        assert_eq!(
            output(
                "import python\n\
                 let stats = inline python {\n    \
                     import statistics\n\n    \
                     LIMIT = {\"low\": 1}\n\n    \
                     def spread(readings):\n        \
                         # a comment of Python, and a brace: }\n        \
                         return max(readings) - min(readings)\n\n    \
                     class Counter:\n        \
                         def __init__(self):\n            \
                             self.count = 0\n\n        \
                         def add(self, by):\n            \
                             self.count += by\n            \
                             return self.count\n\
                 }\n\
                 let counter = stats.Counter()\n\
                 counter.add(2)\n\
                 print(stats.spread([21.5, 22.0, 23.5]), counter.add(3), stats.LIMIT[\"low\"], stats.statistics.mean([1, 3]))\n"
            ),
            "2.0 5 1 2\n"
        );
        // A block Python cannot read stops the program, and Python's
        // own line is the line of the `.zeno` file.
        let said = error("import python\nprint(\"before\")\nlet m = inline python {\n    def f(:\n        return 1\n}\n");
        assert!(said.starts_with("this `inline python` block could not be started. Python said: SyntaxError:"), "{said}");
        assert!(said.contains("line 4"), "{said}");
    }

    #[test]
    fn a_block_of_javascript_is_a_module_of_node() {
        if !node_is_here() {
            return;
        }
        assert_eq!(
            output(
                "import node\n\
                 let text = inline node {\n    \
                     const path = require(\"path\")\n\n    \
                     // a comment of JavaScript, and a brace: }\n    \
                     exports.shout = (word) => word.toUpperCase() + \"!\"\n    \
                     exports.sum = function (items) {\n        \
                         return items.reduce((a, b) => a + b, 0)\n    \
                     }\n    \
                     exports.base = (file) => path.basename(file)\n\
                 }\n\
                 print(text.shout(\"zeno\"), text.sum([1, 2, 3]), text.base(\"/a/b.txt\"))\n"
            ),
            "ZENO! 6 b.txt\n"
        );
        let said = error("import node\nlet m = inline node {\n    const a = require(\"no-such-package-of-zeno\")\n}\n");
        assert!(said.starts_with("this `inline node` block could not be started. JavaScript said: Cannot find module"), "{said}");
    }

    #[test]
    fn a_mistake_with_a_library_of_c_is_an_error_of_zeno() {
        if !cc_is_here() {
            return;
        }
        let sqrt = "import c\nlet m = c.library(\"m\", \"double sqrt(double);\")\n";
        assert_eq!(
            error(&format!("{sqrt}print(m.cbrt(8.0))")),
            "uncaught Error: the library `m` has no `cbrt` among what is declared for it. Declared: sqrt"
        );
        assert_eq!(error(&format!("{sqrt}m.sqrt(\"x\")")), "uncaught Error: the 1st parameter of `sqrt` (`double`) takes a number, and this is a str");
        assert_eq!(error(&format!("{sqrt}m.sqrt()")), "uncaught Error: `sqrt` takes 1 argument, but 0 were given");
        assert_eq!(
            error(&with_c_helper("import c\nlet h = c.library([\"{folder}/helper.c\", \"-DSTEP=1\"], \"int8_t same8(int8_t x);\")\nh.same8(300)")),
            "uncaught OverflowError: the 1st parameter of `same8` (`int8_t`) holds -128 to 127, and this is 300"
        );
        assert_eq!(
            error("import c\nc.library(\"zeno_has_no_such_library\", \"int f(void);\")"),
            "uncaught FileError: C has no library `zeno_has_no_such_library`. Is it installed, with what building with it needs (on Debian and Ubuntu, its package that ends in `-dev`)?"
        );
        assert_eq!(
            error("import c\nc.library(\"m\", \"int zeno_has_no_such_function(int x);\")"),
            "uncaught Error: the library `m` has no `zeno_has_no_such_function`, which was declared"
        );
        assert_eq!(
            error("import c\nc.library(\"m\", \"int printf(const char *format, ...);\")"),
            "uncaught ValueError: `printf` takes any number of arguments (`...`), and Zeno cannot know their types. Declare it with the ones the program gives, under another name of C, or write a function of C that calls it"
        );
        assert_eq!(
            error("import c\nc.library(\"m\", [])"),
            "uncaught ValueError: `c.library` was given no declarations. It takes the functions the program uses, as C declares them: \"int abs(int n);\""
        );
        assert_eq!(
            check_error("import c\nc.library(5, \"double sqrt(double);\")"),
            "the library given to `c.library` should be str, but this is int"
        );
    }

    #[test]
    fn format_strings() {
        assert_eq!(output("let n = 3\nprint(f\"{n} + {n} = {n + n}\")"), "3 + 3 = 6\n");
        assert_eq!(output("print(f\"{3.14159:.2f}|{42:5}|{'ab':5}|{'ab':>5}|{'a:}':^6}|\")"), "3.14|   42|ab   |   ab| a:}  |\n");
        assert_eq!(output("print(f\"{255:x} {255:X} {5:08b} {7:03} {-7:04} {2:.1f}\")"), "ff FF 00000101 007 -007 2.0\n");
        assert_eq!(output("print(f\"{{literal}} {1}\")"), "{literal} 1\n");
    }

    #[test]
    fn lists() {
        assert_eq!(output("let xs = [1, 2]\nxs.append(3)\nxs[0] = 9\nprint(xs, len(xs), xs.pop(), xs + [7], 2 in xs, 5 not in xs)"), "[9, 2] 3 3 [9, 2, 7] true true\n");
        assert_eq!(output("let a = [1]\nlet b = a\nb.append(2)\nprint(a)"), "[1, 2]\n");
        assert_eq!(output("print([n * n for n in range(6) if n % 2 == 0], range(2, 5), range(5, 0, -2))"), "[0, 4, 16] [2, 3, 4] [5, 3, 1]\n");
    }

    #[test]
    fn quantities() {
        assert_eq!(output("print(500 ms, 500 ms + 250 ms, 2 * 500 ms, 1.5 V / 3, 1 s / 250 s, 3 V > 2 V)"), "500 ms 750 ms 1000 ms 0.5 V 0.004 true\n");
        assert_eq!(error("500 ms + 3 V"), "`+` does not work between a quantity in ms and a quantity in V");
        assert_eq!(error("500 ms + 3"), "`+` does not work between a quantity in ms and an int");
        assert_eq!(
            error("print(500 mss)"),
            "`mss` is not a unit Zeno knows. Some that it does: s, ms, us, Hz, V, mV, A, mA, W, degC"
        );
    }

    #[test]
    fn built_in_functions() {
        assert_eq!(output("print(int(\"42\"), int(3.9), float(\"1.5\"), str(12) + \"!\", abs(-3), min(3, 1, 2), max([1, 5.5]), sum([1, 2, 3]), sum([1, 0.5]))"), "42 3 1.5 12! 3 1 5.5 6 1.5\n");
    }

    #[test]
    fn modules() {
        assert_eq!(output("import math\nfrom math import sqrt, floor\nprint(sqrt(16), floor(2.7), math.ceil(2.1), math.pi > 3.14)"), "4.0 2 3 true\n");
        assert_eq!(
            error("import radio"),
            "there is no module `radio`. So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`"
        );
        assert_eq!(error("from math import nope"), "module `math` has no `nope`");
        assert_eq!(error("from time import sleep\nsleep(500)"), "`sleep` needs a time with a unit, such as `500 ms`, but this is an int");
    }

    // ----- Variables and scopes -----

    #[test]
    fn let_cannot_change_and_var_can() {
        assert_eq!(output("var x = 1\nx = 2\nx += 3\nprint(x)"), "5\n");
        assert_eq!(error("let x = 1\nx = 2"), "`x` cannot be changed. Declare it with `var` to allow that");
        assert_eq!(error("const N = 1\nN += 1"), "`N` cannot be changed. Declare it with `var` to allow that");
        assert_eq!(error("x = 2"), "`x` is not declared. Create it with `let` or `var` first");
        assert_eq!(error("print(y)"), "`y` is not defined");
    }

    #[test]
    fn blocks_have_their_own_scope() {
        assert_eq!(output("let x = 1\nif true {\n let x = 2\n print(x)\n}\nprint(x)"), "2\n1\n");
        assert_eq!(error("if true {\n let inner = 1\n}\nprint(inner)"), "`inner` is not defined");
        assert_eq!(error("let x = 1\nlet x = 2"), "`x` is already declared in this block");
        assert_eq!(output("var total = 0\nfor i in range(3) {\n let double = i * 2\n total += double\n}\nprint(total)"), "6\n");
    }

    // ----- Control flow -----

    #[test]
    fn if_else() {
        let source = "function size(n: int) -> str {\n if n < 10 { return \"small\" } else if n < 100 { return \"medium\" } else { return \"large\" }\n}\nprint(size(5), size(50), size(500))";
        assert_eq!(output(source), "small medium large\n");
        assert_eq!(error("if 1 { }"), "a condition must be true or false, but this is an int");
        assert_eq!(error("print(1 and true)"), "`and` needs bools, but this is an int");
    }

    #[test]
    fn loops_with_break_and_continue() {
        let source = "var i = 0\nwhile true {\n i += 1\n if i == 2 { continue }\n if i > 4 { break }\n print(i)\n}";
        assert_eq!(output(source), "1\n3\n4\n");
        assert_eq!(output("for c in \"ab\" {\n print(c)\n}\nfor x in [1, 2, 3] {\n if x == 2 { break }\n print(x)\n}"), "a\nb\n1\n");
        assert_eq!(error("break"), "`break` outside a loop");
        assert_eq!(error("function f() { continue }\nf()"), "`continue` outside a loop");
        assert_eq!(error("for x in 5 { }"), "cannot loop over an int");
    }

    // ----- Functions -----

    #[test]
    fn functions_and_arguments() {
        let source = "function greet(who: str, excited: bool = false) -> str {\n if excited { return f\"Hello, {who}!\" }\n return f\"Hello, {who}.\"\n}\nprint(greet(\"a\"), greet(\"b\", true), greet(who=\"c\", excited=true), greet(\"d\", excited=true))";
        assert_eq!(output(source), "Hello, a. Hello, b! Hello, c! Hello, d!\n");
        assert_eq!(output("function nothing() { }\nprint(nothing())"), "none\n");
    }

    #[test]
    fn argument_mistakes() {
        let f = "function f(a: int, b: int = 2) -> int { return a + b }\n";
        assert_eq!(error(&format!("{f}f()")), "`f` needs a value for `a`");
        assert_eq!(error(&format!("{f}f(1, 2, 3)")), "`f` takes 2 arguments, but 3 were given");
        assert_eq!(error(&format!("{f}f(1, c=3)")), "`f` has no argument named `c`");
        assert_eq!(error(&format!("{f}f(1, a=3)")), "`f` was given `a` twice");
        assert_eq!(error(&format!("{f}f(a=1, 2)")), "plain arguments must come before named ones");
        assert_eq!(error("let x = 5\nx()"), "an int cannot be called");
        assert_eq!(error("print(len(1, 2))"), "`len` takes 1 argument, but 2 were given");
    }

    #[test]
    fn functions_can_be_used_before_they_are_written() {
        assert_eq!(output("print(double(4))\nfunction double(n: int) -> int { return n * 2 }"), "8\n");
        let source = "function even(n: int) -> bool {\n if n == 0 { return true }\n return odd(n - 1)\n}\nfunction odd(n: int) -> bool {\n if n == 0 { return false }\n return even(n - 1)\n}\nprint(even(10), odd(7))";
        assert_eq!(output(source), "true true\n");
    }

    #[test]
    fn recursion_and_its_limit() {
        assert_eq!(output("function fact(n: int) -> int {\n if n <= 1 { return 1 }\n return n * fact(n - 1)\n}\nprint(fact(20))"), "2432902008176640000\n");
        assert_eq!(output("function down(n: int) -> int {\n if n == 0 { return 0 }\n return down(n - 1)\n}\nprint(down(900))"), "0\n");
        assert_eq!(error("function forever() { forever() }\nforever()"), "more than 1000 calls are nested. Is `forever` calling itself forever?");
    }

    #[test]
    fn closures_remember_where_they_were_defined() {
        let source = "function counter() {\n var count = 0\n function next() -> int {\n  count += 1\n  return count\n }\n return next\n}\nlet a = counter()\nlet b = counter()\nprint(a(), a(), b())";
        assert_eq!(output(source), "1 2 1\n");
    }

    #[test]
    fn defaults_are_fresh_on_every_call() {
        let source = "function add(item: int, to: list[int] = []) -> list[int] {\n to.append(item)\n return to\n}\nprint(add(1), add(2))";
        assert_eq!(output(source), "[1] [2]\n");
    }

    #[test]
    fn parameters_cannot_be_reassigned() {
        assert_eq!(error("function f(n: int) { n = 2 }\nf(1)"), "`n` cannot be changed. Declare it with `var` to allow that");
    }

    #[test]
    fn functions_are_values() {
        let source = "function twice(f, x) { return f(f(x)) }\nfunction add_one(n) { return n + 1 }\nlet double = function(n) { return n * 2 }\nprint(twice(add_one, 5), twice(double, 5), function(a, b) { return a + b }(1, 2))";
        assert_eq!(output(source), "7 20 3\n");
        assert_eq!(output("function f() { }\nprint(f, function() { }, len)"), "<function f> <function> <function len>\n");
    }

    #[test]
    fn map_filter_reduce() {
        let source = "let xs = [1, 2, 3, 4, 5, 6]\nfunction even(n) { return n % 2 == 0 }\nprint(xs.filter(even).map(function(n) { return n * n }).reduce(function(total, n) { return total + n }, 0))\nprint(xs.map(str), [].map(even), [].reduce(function(a, b) { return a }, 7))";
        assert_eq!(output(source), "56\n[\"1\", \"2\", \"3\", \"4\", \"5\", \"6\"] [] 7\n");
        assert_eq!(
            error("[1].filter(function(n) { return n })"),
            "the function given to `filter` must return true or false, but it returned an int"
        );
        assert_eq!(error("[1].map(5)"), "an int cannot be called");
        assert_eq!(error("[1].map(function() { return 1 })"), "`function` takes 0 arguments, but 1 was given");
    }

    #[test]
    fn a_function_written_inside_a_call_can_span_lines() {
        let source = "let total = [1, 2, 3].reduce(function(sum, n) {\n    let bigger = n * 10\n    return sum + bigger\n}, 0)\nprint(total)";
        assert_eq!(output(source), "60\n");
    }

    #[test]
    fn types_are_optional() {
        let typed = "class Point {\n x: float\n y: float\n}\nfunction length(p: Point) -> float {\n let squared: float = p.x ** 2 + p.y ** 2\n return squared ** 0.5\n}\nprint(length(Point(3.0, 4.0)))";
        let untyped = "class Point {\n x\n y\n}\nfunction length(p) {\n let squared = p.x ** 2 + p.y ** 2\n return squared ** 0.5\n}\nprint(length(Point(3.0, 4.0)))";
        assert_eq!(output(typed), "5.0\n");
        assert_eq!(output(untyped), "5.0\n");
    }

    // ----- Classes -----

    #[test]
    fn classes() {
        let source = "class Point {\n x: float\n y: float = 0.0\n function shifted(self, dx: float) -> Point {\n  return Point(self.x + dx, self.y)\n }\n}\nlet p = Point(1.0)\nlet q = Point(y=5.0, x=2.0)\np.x = 3.0\nprint(p, q, p.shifted(1.0).x, p == Point(3.0, 0.0), p == q)";
        assert_eq!(output(source), "Point(x=3.0, y=0.0) Point(x=2.0, y=5.0) 4.0 true false\n");
    }

    #[test]
    fn class_mistakes() {
        let point = "class Point {\n x: int\n}\n";
        assert_eq!(error(&format!("{point}Point()")), "`Point` needs a value for `x`");
        assert_eq!(error(&format!("{point}Point(1).z")), "`Point` has no field or method `z`");
        assert_eq!(error(&format!("{point}let p = Point(1)\np.z = 2")), "`Point` has no field `z`");
        assert_eq!(error("let s = \"x\"\ns.nope()"), "a str has no field or method `nope`");
    }

    #[test]
    fn objects_are_shared_not_copied() {
        assert_eq!(output("class Box {\n n: int\n}\nlet a = Box(1)\nlet b = a\nb.n = 2\nprint(a.n)"), "2\n");
    }

    // ----- Enums and match -----

    #[test]
    fn enums_and_match() {
        let source = "enum Shape {\n Dot\n Circle(radius: float)\n Rect(w: float, h: float)\n}\nfunction area(s: Shape) -> float {\n match s {\n  case Circle(r) { return 3.0 * r * r }\n  case Rect(w, h) { return w * h }\n  case Dot { return 0.0 }\n }\n}\nprint(area(Circle(2.0)), area(Rect(w=2.0, h=3.0)), area(Dot), area(Shape.Dot))\nprint(Circle(1.5), Dot, Circle(1.0) == Circle(1.0), Dot == Circle(1.0))";
        assert_eq!(output(source), "12.0 6.0 0.0 0.0\nCircle(1.5) Dot true false\n");
    }

    #[test]
    fn match_on_plain_values() {
        let source = "function name(n: int) -> str {\n match n {\n  case 0 { return \"zero\" }\n  case 1 { return \"one\" }\n  case other { return f\"many ({other})\" }\n }\n}\nprint(name(0), name(1), name(7))\nmatch \"x\" {\n case \"y\" { print(\"no\") }\n case _ { print(\"fallback\") }\n}";
        assert_eq!(output(source), "zero one many (7)\nfallback\n");
    }

    #[test]
    fn match_mistakes() {
        assert_eq!(error("match 3 {\n case 1 { }\n}"), "no `case` matches 3");
        assert_eq!(error("enum E {\n A(x: int)\n}\nmatch A(1) {\n case A(x, y) { }\n}"), "`A` carries 1 value, but this pattern has 2");
        assert_eq!(error("match 1 {\n case Nope(x) { }\n}"), "`Nope` is not an enum case that carries values");
    }

    // ----- Errors -----

    #[test]
    fn raise_and_except() {
        let source = "function parse_port(text: str) -> int raises ValueError {\n let port = int(text)\n if not 1 <= port <= 65535 { raise ValueError(\"port out of range\") }\n return port\n}\nfor text in [\"8080\", \"0\", \"abc\"] {\n try {\n  print(parse_port(text))\n } except ValueError as err {\n  print(f\"error: {err}\")\n }\n}";
        assert_eq!(output(source), "8080\nerror: port out of range\nerror: cannot turn \"abc\" into an int\n");
    }

    #[test]
    fn except_picks_the_matching_handler() {
        let source = "function risky(n: int) {\n if n == 0 { raise ValueError(\"v\") }\n if n == 1 { print(1 // 0) }\n if n == 2 { print([1][5]) }\n}\nfor n in range(3) {\n try {\n  risky(n)\n } except ValueError {\n  print(\"value\")\n } except ZeroDivisionError as e {\n  print(\"zero:\", e.message)\n } except Error as e {\n  print(e.kind)\n }\n}";
        assert_eq!(output(source), "value\nzero: division by zero\nIndexError\n");
    }

    #[test]
    fn a_class_can_be_raised() {
        let source = "class Timeout {\n after: int\n}\ntry {\n raise Timeout(30)\n} except Timeout as t {\n print(t.after)\n}";
        assert_eq!(output(source), "30\n");
    }

    #[test]
    fn a_raised_object_is_an_error_to_those_who_ask_for_any_error() {
        // `except Timeout as t` gets the object. `except Error as e` gets
        // an error named after the class, and raising that again passes
        // the object on.
        let source = "class Timeout {\n after: int\n}\n\
                      try {\n try {\n  raise Timeout(30)\n } except Error as e {\n  print(e.kind, e.message, e)\n  raise e\n }\n\
                      } except Timeout as t {\n print(t.after)\n}";
        assert_eq!(output(source), "Timeout Timeout(after=30) Timeout(after=30)\n30\n");
    }

    #[test]
    fn text_methods_know_only_plain_letters_and_blanks() {
        // The compiled program has to do the same on a microcontroller,
        // with no table of every alphabet to look letters up in.
        assert_eq!(output("print(\"Stra\u{df}e \u{e9}\".upper(), \"\u{c9}A\".lower())"), "STRA\u{df}E \u{e9} \u{c9}a\n");
        assert_eq!(output("print(\"\\t a b \\n\".strip(), \" a  b\\tc \".split(), int(\" 1_000\\n\"))"), "a b [\"a\", \"b\", \"c\"] 1000\n");
    }

    #[test]
    fn a_function_value_keeps_the_variables_it_was_written_with() {
        // The `y` declared after `f` must not get into `f`, which was
        // written when `y` meant the one at the top.
        let source = "let y = 1\nfunction g() -> int {\n let f = function() { return y }\n let y = 2\n return f() + y\n}\nprint(g())";
        assert_eq!(output(source), "3\n");
        // But it does see changes to the variables it kept.
        let source = "function g() -> int {\n var n = 1\n let f = function() { return n }\n n = 5\n return f()\n}\nprint(g())";
        assert_eq!(output(source), "5\n");
    }

    #[test]
    fn a_named_function_sees_what_is_declared_above_it_and_no_more() {
        // The same rule for a function with a name. It is made at the top
        // of its block, so that it can be called from above, but the `x`
        // declared below it is still not the one it was written to use.
        let source = "let x = 1\nfunction outer() -> int {\n function inner() -> int { return x }\n let x = 5\n return inner() + x\n}\nprint(outer())";
        assert_eq!(output(source), "6\n");
        // That holds at the top level of a file too, where the name
        // hidden is a built-in.
        let source = "function size(items: list[int]) -> int { return len(items) }\nlet len = 5\nprint(size([1, 2]), len)";
        assert_eq!(output(source), "2 5\n");
        // What is declared above it is seen, wherever it is called from...
        assert_eq!(output("let a = 1\nprint(f())\nfunction f() -> int { return a }"), "1\n");
        // ...once the line that declares it has run.
        assert_eq!(error("print(f())\nlet a = 1\nfunction f() { return a }"), "`a` is not defined");
        // A name means one variable, whenever the function is called. Here
        // `inner` uses the `x` of its own block, which has no value at
        // the first call. It does not fall back on the `x` outside.
        let source = "let x = 1\nfunction outer() {\n print(inner())\n let x = 5\n function inner() { return x }\n}\nouter()";
        assert_eq!(error(source), "`x` is not defined");
        // A method follows the same rule.
        let source = "let k = 2\nfunction make() -> int {\n class Scale {\n  by: int = k\n  function apply(self, n: int) -> int { return n * self.by * k }\n }\n let k = 10\n return Scale().apply(k)\n}\nprint(make())";
        assert_eq!(output(source), "40\n");
    }

    #[test]
    fn uncaught_errors_stop_the_program() {
        assert_eq!(error("raise ValueError(\"bad\")"), "uncaught ValueError: bad");
        assert_eq!(error("try {\n raise ValueError(\"bad\")\n} except IndexError {\n}"), "uncaught ValueError: bad");
        assert_eq!(error("print(1 / 0)"), "uncaught ZeroDivisionError: division by zero");
        assert_eq!(error("print([1, 2][2])"), "uncaught IndexError: index 2 is out of range for a list of length 2");
        assert_eq!(error("print([1, 2][-1])"), "uncaught IndexError: index -1 is out of range for a list of length 2");
        assert_eq!(error("print(9223372036854775807 + 1)"), "uncaught OverflowError: the result is too big for an int");
        assert_eq!(error("raise 5"), "only an error can be raised, and this is an int");
    }

    #[test]
    fn bugs_cannot_be_caught() {
        assert_eq!(error("try {\n print(\"abc\" - 1)\n} except {\n print(\"caught\")\n}"), "`-` does not work between a str and an int");
        assert_eq!(error("try {\n print(missing)\n} except Error {\n}"), "`missing` is not defined");
    }

    /// What `zeno test` prints for a program, and how each test went:
    /// its name, and the message it failed with.
    fn tested(source: &str) -> (String, Vec<(String, Option<String>)>) {
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let complaints = crate::checker::check(&program, false);
        assert!(complaints.is_empty(), "the checker rejected the program: {complaints:?}\n{source}");
        let (output, tested) = super::run_tests(&program, true, crate::value::testing::outside(), None);
        let tested = tested.expect("the program ran");
        (output, tested.into_iter().map(|test| (test.name, test.failure.map(|failure| failure.message))).collect())
    }

    #[test]
    fn tests_are_chosen_by_what_is_in_their_name() {
        let source = "print(\"the program\")\ntest \"an empty box\" {\n assert true\n}\n\
                      test \"a full box\" {\n assert false\n}\ntest \"empty again\" {\n assert true\n}";
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let run = |only| super::run_tests(&program, true, crate::value::testing::outside(), only).0;
        // The program still runs whole: its tests may need what it made.
        assert_eq!(run(Some("empty")), "the program\n  ok      an empty box\n  ok      empty again\n");
        assert_eq!(run(Some("full box")), "the program\n  FAILED  a full box\n");
        assert_eq!(run(Some("Empty")), "the program\n");
    }

    #[test]
    fn the_tests_of_a_program_are_run_after_it() {
        let source = "var calls = 0\n\
             function half(n: int) -> int raises ValueError {\n calls += 1\n if n % 2 == 1 {\n  raise ValueError(\"odd\")\n }\n return n // 2\n}\n\
             test \"even\" {\n assert half(4) == 2\n print(\"in the test\")\n}\n\
             test \"wrong\" {\n assert half(4) == 3\n print(\"never\")\n}\n\
             print(\"the program\")\n\
             test \"odd\" {\n assert raises ValueError {\n  half(3)\n }\n}\n\
             test \"nothing raised\" {\n assert raises ValueError {\n  half(2)\n }\n}\n\
             test \"another kind\" {\n assert raises KeyError {\n  half(3)\n }\n}\n\
             test \"an error\" {\n half(5)\n}\n\
             test \"not caught\" {\n try {\n  assert calls == 0\n } except Error {\n  print(\"caught\")\n }\n}\n\
             test \"plain\" {\n assert calls > 0 and not false\n assert \"a\" in [\"b\"]\n}\n\
             test \"chain\" {\n assert 1 < 2 < 2\n}";
        let (output, tests) = tested(source);
        // The program runs first, whole, and then its tests in order.
        // Each says how it went as it ends.
        assert_eq!(
            output,
            "the program\nin the test\n  ok      even\n  FAILED  wrong\n  ok      odd\n  FAILED  nothing raised\n  \
             FAILED  another kind\n  FAILED  an error\n  FAILED  not caught\n  FAILED  plain\n  FAILED  chain\n"
        );
        let failures: Vec<&str> = tests.iter().map(|(_, failure)| failure.as_deref().unwrap_or("ok")).collect();
        assert_eq!(
            failures,
            [
                "ok",
                // A comparison says what was on each side.
                "this is false. The left of `==` is 2, and the right is 3",
                "ok",
                "this block was expected to raise ValueError, and it raised nothing",
                "this block was expected to raise KeyError, and it raised ValueError: odd",
                // An error that leaves a test fails it.
                "uncaught ValueError: odd",
                // A failed `assert` is not an error an `except` can catch.
                "this is false. The left of `==` is 6, and the right is 0",
                "this is false. The left of `in` is \"a\", and the right is [\"b\"]",
                "this is false",
            ]
        );
        // `zeno run` goes past the tests, in both engines.
        assert_eq!(output_of_run(source), "the program\n");
    }

    #[test]
    fn an_assert_says_what_it_was_given() {
        // The text goes on a line of its own above what was false, and is
        // worked out only when the `assert` fails (decision 60.1).
        let source = "let xs = [1, 2]\n\
             test \"compared\" {\n let reply = len(xs)\n assert reply == 3, f\"the board answered {reply}\"\n}\n\
             test \"plain\" {\n assert xs[0] == 2 or false, \"the first is not two\"\n}\n\
             test \"passes\" {\n assert len(xs) == 2, f\"never worked out: {xs[9]}\"\n}\n\
             test \"the text fails\" {\n assert false, f\"{xs[9]}\"\n}";
        let (output, tests) = tested(source);
        assert_eq!(output, "  FAILED  compared\n  FAILED  plain\n  ok      passes\n  FAILED  the text fails\n");
        let failures: Vec<&str> = tests.iter().map(|(_, failure)| failure.as_deref().unwrap_or("ok")).collect();
        assert_eq!(
            failures,
            [
                "the board answered 2\nthis is false. The left of `==` is 2, and the right is 3",
                "the first is not two\nthis is false",
                "ok",
                // An error in working out the text fails the test with that error.
                "uncaught IndexError: index 9 is out of range for a list of length 2",
            ]
        );
    }

    fn output_of_run(source: &str) -> String {
        output(source)
    }

    #[test]
    fn a_program_that_stops_runs_no_test() {
        let program = parser::parse(lexer::lex("print(1 // 0)\ntest \"x\" {\n assert true\n}").unwrap()).unwrap();
        let (output, tested) = super::run_tests(&program, true, crate::value::testing::outside(), None);
        assert_eq!(output, "");
        assert_eq!(tested.err().map(|error| error.message).as_deref(), Some("uncaught ZeroDivisionError: division by zero"));
    }

    #[test]
    fn output_before_an_error_is_kept() {
        let (output, result) = run("print(\"one\")\nprint(1 / 0)\nprint(\"two\")");
        assert_eq!(output, "one\n");
        assert!(result.is_err());
    }

    // ----- The bits of whole numbers -----

    #[test]
    fn bit_operators() {
        // Checked against Python, which gives the same answers.
        assert_eq!(output("print(0b1100 & 0b1010, 0b1100 | 0b1010, 0b1100 ^ 0b1010, ~0b1100, ~0, ~-1)"), "8 14 6 -13 -1 0\n");
        assert_eq!(output("print(1 << 5, 1 << 62, 0xFF >> 4, -8 >> 1, -7 >> 1, -1 >> 63, -1 << 63)"), "32 4611686018427387904 15 -4 -4 -1 -9223372036854775808\n");
        assert_eq!(output("print(-1 & 0xFF, ~5 & 0xFF, 6 | 1 ^ 3 & 2, 1 | 2 == 3, 1 << 2 + 1)"), "255 250 7 true 8\n");
        assert_eq!(output("var r = 0\nr |= 1 << 3\nr |= 0x81\nr &= ~1\nr ^= 0xFF\nr <<= 4\nr >>= 2\nprint(r)"), "476\n");
    }

    #[test]
    fn a_shift_must_fit() {
        assert_eq!(error("print(3 << 62)"), "uncaught OverflowError: the result is too big for an int");
        assert_eq!(error("let n = 64\nprint(1 << n)"), "uncaught ValueError: cannot shift by 64. An int has 64 bits, so a shift is 0 to 63");
        assert_eq!(error("let n = -1\nprint(1 >> n)"), "uncaught ValueError: cannot shift by -1. An int has 64 bits, so a shift is 0 to 63");
    }

    // ----- Sized whole numbers -----

    #[test]
    fn a_compound_assignment_is_the_long_form() {
        // What is there is read first, and then the value is worked out,
        // as in `count = count + bump()`.
        let source = "var count = 1\nfunction bump() -> int {\n    count = 10\n    return 1\n}\n\
                      count += bump()\nprint(count)\ncount = 1\ncount = count + bump()\nprint(count)\n\
                      let xs = [1, 2]\nfunction grow() -> int {\n    xs[0] = 10\n    return 1\n}\nxs[0] += grow()\nprint(xs)";
        assert_eq!(output(source), "2\n2\n[2, 2]\n");
        // Every operator on two numbers has an `=` form.
        let source = "var n = 17\nn //= 2\nprint(n)\nn %= 5\nprint(n)\nn **= 4\nprint(n)\nvar x = 7.5\nx //= 2\nx %= 2\nx **= 2\nprint(x)";
        assert_eq!(output(source), "8\n3\n81\n1.0\n");
    }

    #[test]
    fn a_quantity_takes_the_formats_of_a_float() {
        // The decimals are the number's, the unit follows, and the
        // width counts both.
        let source = "let angle = 30.456 deg\nlet supply = 3.3 V\nlet wait = 500 ms\n\
                      print(f\"{angle:.1f}|{supply:.2f}|{supply}|{wait:.0f}|{wait:f}|{wait:.3}\")\n\
                      print(f\"[{supply:10.2f}] [{supply:<10.2f}] [{supply:^10.2f}] [{supply:010.2f}] [{-supply:8.1f}]\")\n\
                      print(f\"[{wait:10}] [{wait:<10}] [{wait:2}]\")";
        assert_eq!(
            output(source),
            "30.5 deg|3.30 V|3.3 V|500 ms|500.000000 ms|500.000 ms\n\
             [    3.30 V] [3.30 V    ] [  3.30 V  ] [00003.30 V] [  -3.3 V]\n\
             [    500 ms] [500 ms    ] [500 ms]\n"
        );
        // It is not a whole number.
        let (_, result) = run("let supply = 3.3 V\nprint(f\"{supply:x}\")");
        assert_eq!(result.unwrap_err(), "`x` needs an int, but this is a quantity in V");
    }

    #[test]
    fn sized_ints() {
        // A sized number is an ordinary int that is known to be small:
        // arithmetic on two of them is never cut down to their size.
        assert_eq!(output("let a: u8 = 200\nlet b: u8 = 100\nprint(a + b, a * b, b - a, ~a, a << 8)"), "300 20000 -100 -201 51200\n");
        assert_eq!(output("print(u8(255), i8(-128), u16(65535), i16(-32768), u32(4294967295), i32(-2147483648))"), "255 -128 65535 -32768 4294967295 -2147483648\n");
        assert_eq!(output("print(u8.wrap(300), u8.wrap(-1), i8.wrap(200), i16.wrap(0xFFFF), u32.wrap(-1), i32.wrap(0x80000000))"), "44 255 -56 -1 4294967295 -2147483648\n");
        assert_eq!(output("var count: u8 = 254\nfor i in range(3) {\n    count = u8.wrap(count + 1)\n    print(count)\n}"), "255\n0\n1\n");
        assert_eq!(output("class Reg {\n    value: u8\n}\nlet r = Reg(value=1)\nr.value = u8(r.value | 0x80)\nprint(r, r.value >> 7)"), "Reg(value=129) 1\n");
        assert_eq!(output("let bytes: list[u8] = [1, 2, 255]\nbytes.append(7)\nprint(bytes, sum(bytes), bytes[2] << 8 | bytes[3])"), "[1, 2, 255, 7] 265 65287\n");
        // `u8` is a value like any built-in, and `wrap` belongs to it.
        assert_eq!(output("let make = u8\nlet cut = u8.wrap\nprint(make(7), cut(263))"), "7 7\n");
    }

    #[test]
    fn a_sized_int_must_fit() {
        assert_eq!(error("let n = 300\nprint(u8(n))"), "uncaught OverflowError: 300 does not fit in a u8 (0 to 255)");
        assert_eq!(error("let n = -1\nprint(u16(n))"), "uncaught OverflowError: -1 does not fit in a u16 (0 to 65535)");
        assert_eq!(error("let n = 128\nprint(i8(n))"), "uncaught OverflowError: 128 does not fit in an i8 (-128 to 127)");
        assert_eq!(
            output("let a: u8 = 200\ntry { print(u8(a + a)) } except OverflowError as e { print(e.message) }"),
            "400 does not fit in a u8 (0 to 255)\n"
        );
        assert_eq!(error("let x = \"7\"\nlet f = u8\nprint(f(x))"), "`u8` needs a whole number, but this is a str");
    }

    // ----- Generics -----

    #[test]
    fn generics() {
        // When a program runs, a generic is one function: values carry
        // their own types, so there is nothing to fill in.
        let first = "function first[T](items: list[T]) -> T {\n    return items[0]\n}\n";
        assert_eq!(output(&format!("{first}print(first([1, 2]), first([\"a\"]), first([[2.5]]))")), "1 a [2.5]\n");
        let stack = "class Stack[T] {\n    items: list[T]\n    function push(self, item: T) {\n        self.items.append(item)\n    }\n\
                     \x20   function pop(self) -> T {\n        return self.items.pop()\n    }\n}\n";
        assert_eq!(
            output(&format!(
                "{stack}let a: Stack[int] = Stack(items=[])\na.push(1)\na.push(2)\nlet b = Stack([\"x\"])\nb.push(\"y\")\n\
                 print(a.pop(), b.pop(), a, b)"
            )),
            "2 y Stack(items=[1]) Stack(items=[\"x\"])\n"
        );
        // Two type parameters, one of them worked out from a function.
        assert_eq!(
            output(
                "function convert[T, U](items: list[T], change: function(T) -> U) -> list[U] {\n\
                 \x20   var out: list[U] = []\n    for item in items {\n        out.append(change(item))\n    }\n    return out\n}\n\
                 print(convert([1, 2], function(n: int) -> str { return f\"<{n}>\" }), convert([\"ab\"], len))"
            ),
            "[\"<1>\", \"<2>\"] [2]\n"
        );
        // A generic that calls itself, and one that calls another.
        assert_eq!(
            output(
                "function count[T](items: list[T], at: int) -> int {\n    if at >= len(items) {\n        return 0\n    }\n\
                 \x20   return 1 + count(items, at + 1)\n}\n\
                 function wrap[T](x: T) -> list[T] {\n    return [x]\n}\n\
                 function twice[T](x: T) -> list[list[T]] {\n    return wrap(wrap(x))\n}\n\
                 print(count([1, 2, 3], 0), count([\"a\"], 0), twice(5), twice(\"x\"))"
            ),
            "3 1 [[5]] [[\"x\"]]\n"
        );
        // A class that mentions itself.
        assert_eq!(
            output(
                "class Node[T] {\n    value: T\n    next: Node[T] or none\n}\n\
                 function length[T](node: Node[T] or none) -> int {\n    var n = 0\n    var at = node\n\
                 \x20   while at is not none {\n        n += 1\n        at = at.next\n    }\n    return n\n}\n\
                 let chain = Node(1, Node(2, none))\nprint(length(chain), chain.next)"
            ),
            "2 Node(value=2, next=none)\n"
        );
        // With the types left out it still runs.
        assert_eq!(
            output("function id[T](x: T) -> T {\n    return x\n}\nfunction loose(x) {\n    return id(x)\n}\nprint(loose(3), loose(\"a\"))"),
            "3 a\n"
        );
    }

    #[test]
    fn the_generics_example() {
        assert_eq!(
            output(include_str!("../examples/generics.zeno")),
            "bytes: [108, 108, 111, 33] newest 33 dropped 2\n\
             above 0x60: 3\n\
             as hex: [\"6c\", \"6c\", \"6f\", \"21\"]\n\
             no reading yet: none\n\
             volts: [3.3 V, 2.9 V, 3.2 V] highest 3.3 V\n\
             low: 1\n\
             log: [\"sensor ready\", \"first reading\"] last in the alphabet: sensor ready\n\
             lengths: [12, 13]\n\
             Ring(slots=[\"first reading\", \"sensor ready\"], next=1, count=2, dropped=1)\n"
        );
    }

    // ----- The board and the clock -----

    #[test]
    fn a_pin_shows_a_line_when_its_level_changes() {
        let source = "from board import led, pin\nled.on()\nled.on()\nled.toggle()\npin(7).toggle()\nled.off()\n\
                      print(led, pin(7), led.is_on(), pin(7).is_on(), led.number, led == pin(13))";
        assert_eq!(output(source), "led on\nled off\npin 7 on\nPin(13) Pin(7) false true 13 true\n");
    }

    #[test]
    fn the_board_has_64_pins() {
        assert_eq!(output("from board import pin\npin(0).on()\npin(63).on()"), "pin 0 on\npin 63 on\n");
        assert_eq!(error("from board import pin\npin(64)"), "uncaught ValueError: this board has no pin 64. Its pins are 0 to 63");
        assert_eq!(output("from board import pin\ntry { pin(-1) } except ValueError as e { print(e.message) }"), "this board has no pin -1. Its pins are 0 to 63\n");
    }

    #[test]
    fn the_clock_only_goes_forward() {
        let source = "from time import sleep, now\nlet start = now()\nsleep(2 ms)\nsleep(0 s)\nsleep(-1 s)\n\
                      let took = now() - start\nprint(took >= 2 ms, took < 5 s)";
        assert_eq!(output(source), "true true\n");
    }

    #[test]
    fn return_outside_a_function() {
        assert_eq!(error("return 1"), "`return` outside a function");
    }

    // ----- The page -----
    //
    // These run on the simulated page (`page.rs`). The real one needs a
    // browser: docs/web.md says how it was tried in one.

    #[test]
    fn the_page_shows_a_line_for_each_change_that_could_be_seen() {
        let source = "import web\nlet title = web.element(\"#title\")\n\
                      title.text = \"Hello\"\ntitle.text = \"Hello\"\n\
                      title.add_class(\"big\")\ntitle.add_class(\"big\")\ntitle.remove_class(\"big\")\ntitle.remove_class(\"big\")\n\
                      title.set(\"lang\", \"ta\")\ntitle.set(\"lang\", \"ta\")\n\
                      title.hide()\ntitle.hide()\ntitle.show()\n\
                      print(title, title.text, title.get(\"lang\"), title.get(\"href\"), title.has_class(\"big\"))\n\
                      print(title == web.element(\"#title\"), title == web.element(\"#other\"))";
        assert_eq!(
            output(source),
            "#title text: Hello\n#title class +big\n#title class -big\n#title lang: ta\n#title hidden\n#title shown\n\
             Element(#title) Hello ta none false\ntrue false\n"
        );
    }

    #[test]
    fn an_element_is_seen_once_it_is_on_the_page() {
        let source = "import web\nlet list = web.element(\"#list\")\n\
                      let item = web.create(\"li\", \"milk\")\nitem.add_class(\"new\")\nitem.text = \"Milk\"\n\
                      print(item, item.text, len(web.elements(\"li\")))\n\
                      list.append(item)\nitem.text = \"Oat milk\"\n\
                      let more = web.create(\"li\")\nmore.append(web.create(\"b\", \"bread\"))\nlist.append(more)\n\
                      print(len(web.elements(\"li\")), len(web.elements(\".new\")), len(web.elements(\"b\")), list.text)\n\
                      item.remove()\nitem.text = \"gone\"\n\
                      print(len(web.elements(\"li\")), len(web.elements(\".new\")))\n\
                      list.clear()\nlist.clear()\nprint(len(web.elements(\"li\")), len(web.elements(\"b\")))";
        assert_eq!(
            output(source),
            "Element(<li>) Milk 0\n#list + <li> Milk\n<li> text: Oat milk\n#list + <li> bread\n2 1 1 Oat milkbread\n\
             <li> removed\n1 0\n#list cleared\n0 0\n"
        );
    }

    #[test]
    fn an_element_is_in_one_place() {
        // Appended somewhere else, it leaves where it was.
        let source = "import web\nlet a = web.element(\"#a\")\nlet b = web.element(\"#b\")\n\
                      let p = web.create(\"p\", \"one\")\na.append(p)\nb.append(p)\nprint(a.text == \"\", b.text)\n\
                      let box = web.create(\"div\")\nbox.append(p)\nprint(len(web.elements(\"p\")))\n\
                      a.append(box)\nprint(len(web.elements(\"p\")))";
        assert_eq!(output(source), "#a + <p> one\n#b + <p> one\ntrue one\n<p> removed\n0\n#a + <div> one\n1\n");
        let inside_itself = "uncaught ValueError: an element cannot be put inside itself, or inside something it holds";
        assert_eq!(error("import web\nlet a = web.element(\"#a\")\na.append(a)"), inside_itself);
        assert_eq!(
            error("import web\nlet a = web.element(\"#a\")\nlet p = web.create(\"p\")\na.append(p)\np.append(a)"),
            inside_itself
        );
    }

    #[test]
    fn the_simulated_page_reads_plain_selectors() {
        let source = "import web\nlet list = web.element(\"#list\")\nweb.element(\"#title\").add_class(\"big\")\n\
                      for word in [\"milk\", \"bread\", \"tea\"] {\n let item = web.create(\"li\", word)\n\
                      if word != \"bread\" { item.add_class(\"done\") }\n list.append(item)\n}\n\
                      let other = web.create(\"li\", \"outside\")\nother.add_class(\"done\")\nother.set(\"id\", \"last\")\n\
                      web.element(\"#other\").append(other)\n\
                      function count(selector: str) -> int { return len(web.elements(selector)) }\n\
                      print(count(\"li\"), count(\"li.done\"), count(\"#list li.done\"), count(\"#list .done\"))\n\
                      print(count(\"#other li\"), count(\"#list #last\"), count(\"li#last.done\"), count(\".big\"), count(\"p.big\"))\n\
                      print(count(\"#list\"), count(\"#nothing li\"), count(\"#list #list\"))";
        let printed = output(source);
        assert!(printed.ends_with("4 3 2 2\n1 0 1 1 0\n1 0 0\n"), "{printed}");
        // What only a browser can read is not answered with a guess.
        assert_eq!(
            error("import web\nweb.elements(\"ul > li\")"),
            "uncaught ValueError: the simulated page cannot read `ul > li`. It reads a tag, `.class` and `#id`, together \
             (`li.done`) and one inside another (`#list li`). A browser reads the rest, so the page that is built can use it"
        );
    }

    #[test]
    fn what_is_typed_into_an_element_is_its_value() {
        let source = "import web\nlet box = web.element(\"input\")\nbox.value = \"typed\"\nbox.value = \"typed\"\n\
                      print(box.value, box.text == \"\")\nbox.value += \"!\"\nprint(box.value.upper())";
        assert_eq!(output(source), "input value: typed\ntyped true\ninput value: typed!\nTYPED!\n");
    }

    #[test]
    fn the_simulated_page_calls_nothing() {
        // Nobody is there to press a button, and no time passes.
        let source = "import web\nfunction tick() { print(\"tick\") }\n\
                      web.after(500 ms, tick)\nweb.after(2 s, tick)\n\
                      web.on_key(function(key: str) { print(key) })\n\
                      web.element(\"#go\").on(\"click\", tick)\nprint(\"nothing was called\")";
        assert_eq!(output(source), "nothing was called\n");
    }

    #[test]
    fn the_page_says_what_it_cannot_do() {
        assert_eq!(
            error("import web\nweb.create(\"<li>\", \"x\")"),
            "uncaught ValueError: `<li>` is not the name of a kind of element. Write one such as `li` or `button`"
        );
        assert_eq!(
            error("import web\nweb.after(500, print)"),
            "`after` needs a time with a unit, such as `500 ms`, but this is an int"
        );
        assert_eq!(error("import web\nweb.after(1 V, print)"), "`after` needs a time, such as `500 ms`, not a quantity in V");
        assert_eq!(error("import web\nweb.on_key(5)"), "`on_key` needs a function to call, but this is an int");
        assert_eq!(
            error("import web\nweb.element(\"p\").shout()"),
            "an element has no field or method `shout`"
        );
    }

    #[test]
    fn a_page_calls_a_function_of_the_program_later() {
        // What a browser does when a button is pressed, done by hand:
        // the program has run to its last line, and is then called.
        let source = "import web\nlet count = web.element(\"#count\")\nvar presses = 0\n\
                      function pressed() {\n presses += 1\n count.text = f\"{presses} presses\"\n}\n\
                      web.element(\"#press\").on(\"click\", pressed)\n\
                      web.on_key(function(key: str) {\n if key == \"Enter\" { pressed() }\n print(key)\n})\n\
                      web.after(1 s, function() { print([1][presses]) })\n\
                      print(\"ready\")";
        let program = crate::modules::testing::program(source);
        let (mut session, result) = super::Session::start(program, crate::page::Page::simulated(None), true);
        assert!(result.is_ok());
        assert_eq!(session.printed(), "ready\n");
        // The first function it gave to the page was `pressed`.
        session.call(0, Vec::new()).expect("a press");
        session.call(0, Vec::new()).expect("a press");
        assert_eq!(session.printed(), "#count text: 1 presses\n#count text: 2 presses\n");
        // The second takes the key.
        session.call(1, vec![super::Value::str("a")]).expect("a key");
        session.call(1, vec![super::Value::str("Enter")]).expect("a key");
        assert_eq!(session.printed(), "a\n#count text: 3 presses\nEnter\n");
        // An error in one is the program's error, with its line.
        let error = session.call(2, Vec::new()).expect_err("out of range");
        assert_eq!(error.message, "uncaught IndexError: index 3 is out of range for a list of length 1");
        assert_eq!(error.span.line, 13);
        // A number the page was never given calls nothing.
        session.call(9, Vec::new()).expect("nothing to call");
        assert_eq!(session.printed(), "");
    }

    #[test]
    fn fire_calls_what_on_was_given() {
        // How a test presses a button: at once, in the order given, and
        // with the event for a function that asks for it.
        let source = "import web\nlet button = web.element(\"#press\")\nvar presses = 0\n\
                      button.on(\"click\", function() { presses += 1 })\n\
                      button.on(\"click\", function(event: Event) {\n print(event, event.kind, event.target, event.x, event.y, event.key == \"\")\n print(event.target == button)\n})\n\
                      button.on(\"input\", function() { print(\"typed\") })\n\
                      button.fire(\"click\")\nbutton.fire(\"click\")\nbutton.fire(\"keydown\")\nprint(presses)\n\
                      class Counter {\n n: int\n function heard(self, event: Event) { self.n += 1 }\n function plain(self) { self.n += 10 }\n}\n\
                      let counter = Counter(0)\nbutton.on(\"wheel\", counter.heard)\nbutton.on(\"wheel\", counter.plain)\nbutton.fire(\"wheel\")\nprint(counter.n)";
        assert_eq!(
            output(source),
            "Event(click) click Element(#press) 0.0 0.0 true\ntrue\nEvent(click) click Element(#press) 0.0 0.0 true\ntrue\n2\n11\n"
        );
        assert_eq!(
            error("import web\nweb.element(\"#a\").on(\"click\", function() { print([1][2]) })\nweb.element(\"#a\").fire(\"click\")"),
            "uncaught IndexError: index 2 is out of range for a list of length 1"
        );
    }

    #[test]
    fn styles_and_what_is_saved_and_what_is_drawn() {
        let source = "import web\nlet box = web.element(\"#box\")\n\
                      print(box.get_style(\"color\"))\nbox.set_style(\"color\", \"red\")\nbox.set_style(\"color\", \"red\")\n\
                      box.set_style(\"color\", \"blue\")\nprint(box.get_style(\"color\"))\n\
                      print(web.load(\"name\"))\nweb.save(\"name\", \"Partha\")\nweb.save(\"name\", \"Zeno\")\nprint(web.load(\"name\"))\n\
                      web.forget(\"name\")\nweb.forget(\"name\")\nprint(web.load(\"name\"))\n\
                      box.draw_rect(10, 10, 50.5, 20, \"red\")\nbox.draw_circle(5, 5, 2, \"#00f\")\nbox.draw_line(0, 0, 9, 9, \"black\")\n\
                      box.draw_text(1, 2, \"two words\", \"green\")\nbox.erase()\n\
                      web.every(1 s, function() { print(\"never on a desk\") })\n\
                      web.fetch(\"/prices\", function(status: int, body: str) { print(status) })";
        assert_eq!(
            output(source),
            "none\n#box style color: red\n#box style color: blue\nblue\nnone\nZeno\nnone\n\
             #box draw_rect 10 10 50.5 20 red\n#box draw_circle 5 5 2 #00f\n#box draw_line 0 0 9 9 black\n\
             #box draw_text 1 2 two words green\n#box erase\n"
        );
        assert_eq!(
            error("import web\nweb.create(\"p\", \"x\").draw_rect(0, 0, 1, 1, \"red\")"),
            "uncaught ValueError: `draw_rect` draws on a `<canvas>`, and this element is not one"
        );
        assert_eq!(error("import web\nweb.every(0 ms, function() { print(1) })"), "uncaught ValueError: `every` needs a time of at least 1 ms");
    }

    #[test]
    fn a_timer_can_be_stopped() {
        let source = "import web\nlet ticking = web.every(1 s, function() { print(\"tick\") })\n\
                      let once: Timer = web.after(500 ms, function() { print(\"late\") })\n\
                      print(ticking, ticking == ticking, ticking == once)\n\
                      ticking.stop()\nticking.stop()\nonce.stop()\nprint(\"stopped\")";
        assert_eq!(output(source), "Timer true false\nstopped\n");
    }

    #[test]
    fn a_page_that_was_written_has_the_elements_it_has() {
        let page = "<body><h1 id=title class=\"big\">Hello</h1>\n<form id=add><input id=what value=\"milk\"><textarea id=note>a note</textarea></form>\n\
                    <ul id=list>\n  <li>one<li class=done>two</ul>\n<p id=left style=\"color: red; width: 10px\" hidden></p>\n<canvas id=board></canvas></body>";
        crate::value::testing::on_page(page, || {
            let source = "import web\nlet title = web.element(\"#title\")\n\
                          print(title.text, title.has_class(\"big\"), title.get(\"class\"), web.element(\"h1\") == title)\n\
                          print(web.element(\"#what\").value, web.element(\"#note\").value, len(web.elements(\"li\")), len(web.elements(\"#list li.done\")))\n\
                          let list = web.element(\"#list\")\nprint(list.text)\nlist.append(web.create(\"li\", \"three\"))\n\
                          let left = web.element(\"#left\")\nprint(left.get_style(\"width\"), left.get_style(\"height\"))\nleft.show()\nleft.text = \"2 left\"\n\
                          web.element(\"#board\").draw_rect(0, 0, 1, 1, \"red\")\n\
                          print(len(web.elements(\"em\")))\nlist.clear()\nprint(len(web.elements(\"li\")))";
            assert_eq!(
                output(source),
                "Hello true big true\nmilk a note 2 1\nonetwo\n#list + <li> three\n10px none\n#left shown\n#left text: 2 left\n\
                 #board draw_rect 0 0 1 1 red\n0\n#list cleared\n0\n"
            );
            // The reason the page is read: a name spelt wrong is found on a desk.
            assert_eq!(
                error("import web\nlet count = web.element(\"#cuont\")"),
                "uncaught ValueError: the page has no element that answers to `#cuont`. `web.elements` gives a list, which may be empty"
            );
            assert_eq!(
                error("import web\nweb.element(\"#title\").draw_rect(0, 0, 1, 1, \"red\")"),
                "uncaught ValueError: `draw_rect` draws on a `<canvas>`, and this element is not one"
            );
        });
        // The to-do list says the same with its page as without.
        let without = output(include_str!("../examples/todo.zeno"));
        crate::value::testing::on_page(include_str!("../examples/todo.html"), || {
            assert_eq!(output(include_str!("../examples/todo.zeno")), without);
        });
    }

    #[test]
    fn the_page_hands_over_the_event_and_the_answer() {
        let source = "import web\nlet board = web.element(\"#board\")\n\
                      board.on(\"click\", function() { print(\"pressed\") })\n\
                      board.on(\"click\", function(event: Event) { print(event.kind, event.x, event.y, event.target) })\n\
                      web.fetch(\"/prices\", function(status: int, body: str) { print(status, body) })";
        let program = crate::modules::testing::program(source);
        let (mut session, result) = super::Session::start(program, crate::page::Page::simulated(None), true);
        assert!(result.is_ok());
        let event = || {
            super::Value::Event(std::rc::Rc::new(crate::value::Event {
                kind: std::rc::Rc::from("click"),
                target: super::Value::Element(0, std::rc::Rc::from("<b>")),
                x: 12.0,
                y: 3.5,
                key: std::rc::Rc::from(""),
            }))
        };
        // A function that did not ask for the event is not given it.
        session.call(0, vec![event()]).expect("a click");
        session.call(1, vec![event()]).expect("a click");
        session.call(2, vec![super::Value::Int(404), super::Value::str("not here")]).expect("an answer");
        assert_eq!(session.printed(), "pressed\nclick 12.0 3.5 Element(<b>)\n404 not here\n");
    }

    // ----- JSON (docs/json.md) -----

    #[test]
    fn json_is_read_into_values_and_written_from_them() {
        let source = "import json\n\
                      let data = json.parse(\"{\\\"name\\\": \\\"Ada\\\", \\\"age\\\": 36, \\\"tags\\\": [\\\"a\\\", \\\"b\\\"], \\\"score\\\": 9.5, \\\"gone\\\": null, \\\"ok\\\": true, \\\"in\\\": {\\\"n\\\": 2.0}}\")\n\
                      print(data)\n\
                      let name: str = data[\"name\"]\nlet age: int = data[\"age\"]\n\
                      print(name, age + 1, data[\"score\"] * 2, data.get(\"gone\"), \"gone\" in data, data[\"in\"][\"n\"])\n\
                      for tag in data[\"tags\"] {\n    print(tag.upper())\n}\n\
                      print(json.write(data))\n\
                      print(json.parse(\"[1, null, \\\"x\\\", -2.5e3, 1e300]\"), json.parse(\"null\"), json.parse(\" 12 \"), json.parse(\"\\\"t\\\\u00e9\\\"\"))\n\
                      class Line {\n    item: str\n    count: int\n}\n\
                      class Order {\n    who: str\n    lines: list[Line]\n    note: str or none\n    total: float\n}\n\
                      let order = Order(\"Ada \\\"A\\\"\\n\", [Line(\"tea\", 2), Line(\"milk\", 1)], none, 7.0)\n\
                      let sent = json.write(order)\nprint(sent)\nprint(json.parse(sent)[\"lines\"][1][\"item\"])\n\
                      print(json.write([\"a\": [1, 2], \"b\": [3]]), json.write([1, 2.5, none, true]), json.write(\"x\"), json.write([7: \"seven\"]))";
        assert_eq!(
            output(source),
            "[\"name\": \"Ada\", \"age\": 36, \"tags\": [\"a\", \"b\"], \"score\": 9.5, \"ok\": true, \"in\": [\"n\": 2]]\n\
             Ada 37 19.0 none false 2\nA\nB\n\
             {\"name\":\"Ada\",\"age\":36,\"tags\":[\"a\",\"b\"],\"score\":9.5,\"ok\":true,\"in\":{\"n\":2}}\n\
             [1, none, \"x\", -2500, 1e300] none 12 t\u{e9}\n\
             {\"who\":\"Ada \\\"A\\\"\\n\",\"lines\":[{\"item\":\"tea\",\"count\":2},{\"item\":\"milk\",\"count\":1}],\"note\":null,\"total\":7.0}\nmilk\n\
             {\"a\":[1,2],\"b\":[3]} [1,2.5,null,true] \"x\" {\"7\":\"seven\"}\n"
        );
        assert_eq!(error("import json\njson.parse(\"{oops\")"), "uncaught ValueError: this text is not JSON: `{oops`");
        assert_eq!(
            error("import json\njson.parse(\"  [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12\")"),
            "uncaught ValueError: this text is not JSON: `[1, 2, 3, 4, 5, 6, 7, 8, 9, 10...`"
        );
        assert_eq!(
            error("import json\njson.write([1 V])"),
            "uncaught ValueError: JSON has no units, and this is a quantity in V. Write its number, and put the unit in the name: `[\"V\": ...]`"
        );
        assert_eq!(
            error("import json\njson.write(print)"),
            "uncaught ValueError: JSON has objects, lists, text, numbers, bools and none, and this is a function"
        );
        assert_eq!(error("import json\njson.write(1.0 / 0.0)"), "uncaught ZeroDivisionError: division by zero");
        // An error that is caught, as any other.
        assert_eq!(
            output("import json\ntry {\n    print(json.parse(\"nope\"))\n} except ValueError as mistake {\n    print(mistake.message)\n}"),
            "this text is not JSON: `nope`\n"
        );
    }

    // ----- The examples -----

    #[test]
    fn hello_runs() {
        assert_eq!(output(include_str!("../examples/hello.zeno")), "Hello, Zeno!\n");
    }

    #[test]
    fn the_styles_example_gives_the_same_answer_four_ways() {
        assert_eq!(
            output(include_str!("../examples/styles.zeno")),
            "typed, with a class:   21.5\n\
             untyped, with a class: 21.5\n\
             functional:            21.5\n\
             comprehension:         21.5\n"
        );
    }

    #[test]
    fn units_of_one_kind_work_together() {
        assert_eq!(
            output("let t = 3 s\nlet d = 250 ms\nprint(t + d, t - d, t.ms, d.s, (90 min).h, (1 kWh).J)"),
            "3250 ms 2750 ms 3000 ms 0.25 s 1.5 h 3600000 J\n"
        );
        assert_eq!(output("print(1 s == 1000 ms, 3 s > 250 ms, 999 ms < 1 s, 1 h / 1 min)"), "true true true 60.0\n");
    }

    #[test]
    fn the_tour_runs() {
        assert_eq!(
            output(include_str!("../examples/tour.zeno")),
            "listening on 8080\n\
             area = 3.14\n\
             area = 6.00\n\
             Hello, Zeno! 4 [0, 4, 16, 36, 64] 5.0\n18 waiting 500 ms\n"
        );
    }

    // ----- Memory by hand (docs/unsafe.md) -----

    #[test]
    fn memory_by_hand() {
        assert_eq!(
            output(
                "let bytes = allocate(u8, 4)\n\
                 let wide = allocate(int, 2)\n\
                 let small = allocate(i8, 2)\n\
                 let samples = allocate(float, 3)\n\
                 unsafe {\n\
                 \x20   print(bytes[0], wide[1], samples[2])\n\
                 \x20   bytes[0] = 0x41\n\
                 \x20   bytes[3] = u8(bytes[0] + 1)\n\
                 \x20   wide[1] = -7000000000\n\
                 \x20   wide[1] += 5\n\
                 \x20   small[0] = -128\n\
                 \x20   samples[1] = 2.5\n\
                 \x20   print(bytes[0], bytes[3], wide[1], small[0], samples[1])\n\
                 \x20   let last = bytes + 3\n\
                 \x20   print(last[0], (last - 3)[0], last == bytes + 3, last == bytes)\n\
                 \x20   free(wide)\n\
                 \x20   free(small)\n\
                 \x20   free(samples)\n\
                 }\n\
                 print(bytes, [bytes, bytes + 1] == [bytes, bytes + 1])\n\
                 unsafe { free(bytes) }"
            ),
            "0 0 0.0\n65 66 -6999999995 -128 2.5\n66 65 true false\n<pointer[u8]> true\n"
        );
    }

    #[test]
    fn zeno_run_checks_every_use_of_a_pointer() {
        let p = "let p = allocate(u8, 4)\n";
        for (source, message) in [
            (
                format!("{p}unsafe {{ print(p[4]) }}"),
                "this reads item 4 of a block that has 4 (0 to 3). `zeno run` checks every use of a pointer. \
                 A compiled program does not: it reads whatever is beside the block",
            ),
            (
                format!("{p}unsafe {{ p[-1] = 1 }}"),
                "this writes over item -1 of a block that has 4 (0 to 3). `zeno run` checks every use of a pointer. \
                 A compiled program does not: it writes over whatever is beside the block",
            ),
            // Working out an address past the end is allowed. Using it is
            // what is caught.
            (
                format!("{p}let q = p + 6\nunsafe {{\n    print((q - 3)[0])\n    print(q[0])\n}}"),
                "this reads item 6 of a block that has 4 (0 to 3). `zeno run` checks every use of a pointer. \
                 A compiled program does not: it reads whatever is beside the block",
            ),
            (
                format!("{p}unsafe {{\n    free(p)\n    print(p[0])\n}}"),
                "this reads memory that was freed. `zeno run` checks every use of a pointer. \
                 A compiled program does not: it reads whatever is there now, which may be somebody else's",
            ),
            (
                format!("{p}unsafe {{\n    free(p)\n    p[0] = 1\n}}"),
                "this writes over memory that was freed. `zeno run` checks every use of a pointer. \
                 A compiled program does not: it writes over whatever is there now, which may be somebody else's",
            ),
            (
                format!("{p}unsafe {{\n    free(p)\n    free(p)\n}}"),
                "this memory was freed already. `zeno run` checks every `free`. \
                 A compiled program does not: freeing twice damages the memory it hands out next",
            ),
            (
                format!("{p}unsafe {{ free(p + 1) }}"),
                "`free` needs the pointer that `allocate` gave, and this one points 1 item further on. \
                 `zeno run` checks every `free`. A compiled program does not",
            ),
            ("let n = 0\nlet p = allocate(u8, n)".to_string(), "`allocate` needs at least one item, and this asks for 0"),
        ] {
            assert_eq!(error(&source), message, "\n{source}");
        }
        // A value that does not fit is an ordinary error, as it is for
        // any sized number, and can be caught.
        assert_eq!(
            output(
                "let p = allocate(u8, 2)\nlet n = 256\n\
                 try {\n    unsafe { p[0] = u8(n) }\n} except OverflowError as e {\n    print(e.message)\n}\n\
                 unsafe {\n    print(p[0])\n    free(p)\n}"
            ),
            "256 does not fit in a u8 (0 to 255)\n0\n"
        );
    }

    #[test]
    fn registers_on_the_simulated_board() {
        // A register nobody wrote to reads as 0. Every write prints a
        // line, also one that changes nothing.
        assert_eq!(
            output(
                "from board import register\n\
                 let mode = register(u32, 0x4002_0000)\n\
                 let data = register(u8, 0x4002_0014)\n\
                 unsafe {\n\
                 \x20   print(mode.read())\n\
                 \x20   mode.write(u32(mode.read() | 1 << 10))\n\
                 \x20   data.write(0x55)\n\
                 \x20   data.write(0x55)\n\
                 \x20   mode.write(u32(mode.read() & ~(1 << 10)))\n\
                 \x20   print(mode.read(), data.read())\n\
                 }\n\
                 print(mode, data.address, mode == register(u32, 0x4002_0004), mode == register(u32, 0x4002_0000))"
            ),
            "0\nregister 0x40020000 = 0x00000400\nregister 0x40020014 = 0x55\nregister 0x40020014 = 0x55\n\
             register 0x40020000 = 0x00000000\n0 85\nRegister(0x40020000) 1073872916 false true\n"
        );
        // The module can be used by its name as well.
        assert_eq!(
            output("import board\nlet r = board.register(u16, 0x10)\nunsafe { r.write(0xBEEF) }"),
            "register 0x00000010 = 0xbeef\n"
        );
    }

    #[test]
    fn the_driver_example_runs() {
        assert_eq!(
            output(include_str!("../examples/driver.zeno")),
            "register 0x40004408 = 0x0068\n\
             register 0x40004400 = 0x00000009\n\
             waiting: 4 refused: 2\n\
             checksum of all four: 156\n\
             checksum of the last three: 66\n\
             register 0x4000440c = 0x5a\n\
             register 0x4000440c = 0x65\n\
             register 0x4000440c = 0x6e\n\
             register 0x4000440c = 0x6f\n\
             sent: 4\n\
             register 0x4000440c = 0x21\n\
             register 0x4000440c = 0x0a\n\
             sent: 2\n\
             register 0x40004400 = 0x00000000\n\
             Register(0x40004400) Register(0x4000440c) <pointer[u8]>\n"
        );
    }

    // ----- Resources (decision 59.1) -----

    const HELD: &str = "owned class Held is Resource {\n    name: str\n\n    function release(self) {\n        print(\"released\", self.name)\n    }\n}\n";

    /// How a program with resources ends, on both engines. The checker
    /// is the one that says where each resource's owner ends.
    fn resourced(source: &str) -> crate::value::Ran {
        let source = format!("{HELD}{source}");
        let program = parser::parse(lexer::lex(&source).unwrap()).unwrap().with_resource();
        let (program, complaints, _) = crate::checker::checked(program, crate::checker::Rules::default());
        assert!(complaints.is_empty(), "the checker rejected a working program: {complaints:?}\n{source}");
        crate::vm::run_both_ending(&program, &source)
    }

    #[test]
    fn an_error_that_ends_the_program_releases_on_its_way_out() {
        // The top of the file ends too, so its resources are released
        // before the error is reported; the report is as it always was.
        let ran = resourced(
            "function fail() {\n    let inner = Held(\"inner\")\n    raise ValueError(\"bad\")\n}\n\
             let a = Held(\"a\")\nprint(\"before\")\nfail()\nprint(\"after\")\n",
        );
        assert_eq!(ran.output, "before\nreleased inner\nreleased a\n");
        assert_eq!(ran.result.map_err(|error| error.message), Err("uncaught ValueError: bad".to_string()));
    }

    #[test]
    fn under_test_the_top_of_a_file_ends_after_its_last_test() {
        // The tests use what the top made, so it is released after them;
        // a test that fails releases what its own blocks held.
        let source = format!(
            "{HELD}let top = Held(\"top\")\n\
             test \"one\" {{\n    let inner = Held(\"inner\")\n    assert 1 == 2\n    print(\"not here\")\n}}\n\
             test \"two\" {{\n    print(\"using\", top.name)\n}}\nprint(\"program\")\n"
        );
        let program = parser::parse(lexer::lex(&source).unwrap()).unwrap().with_resource();
        let (program, complaints, _) = crate::checker::checked(program, crate::checker::Rules::default());
        assert!(complaints.is_empty(), "{complaints:?}");
        let (output, tested) = super::run_tests(&program, true, crate::value::testing::outside(), None);
        assert_eq!(output, "program\nreleased inner\n  FAILED  one\nusing top\n  ok      two\nreleased top\n");
        let tested = tested.expect("the program ran");
        assert_eq!(tested.iter().map(|test| test.failure.is_some()).collect::<Vec<_>>(), [true, false]);
        // `zeno run` runs no test, and the top is released at its end.
        assert_eq!(resourced("let top = Held(\"top\")\ntest \"one\" {\n    print(\"no\")\n}\nprint(\"program\")\n").output, "program\nreleased top\n");
    }

    // ----- A file in pieces (decisions 59.6 and 59.7) -----

    /// How a program that imports a part of the library written in Zeno
    /// ends, on both engines: the program as `zeno run` gets it.
    fn joined(source: &str) -> crate::value::Ran {
        let program = crate::modules::testing::program(source);
        crate::vm::run_both_ending(&program, source)
    }

    /// The messages of the checker for such a program.
    fn joined_errors(source: &str) -> Vec<String> {
        let program = crate::modules::testing::load(source).program.expect("it reads");
        let (_, errors, _) = crate::checker::checked(program, crate::checker::Rules::default());
        errors.into_iter().map(|error| error.message).collect()
    }

    #[test]
    fn a_file_is_read_and_written_in_pieces() {
        let path = scratch("pieces.txt");
        let source = format!(
            "import files\nlet path = \"{path}\"\n\
             if true {{\n    let out = files.write_to(path)\n    out.write(\"one\\r\\ntwo\\n\")\n    out.write_bytes([116, 104, 114, 101, 101])\n}}\n\
             if true {{\n    let more = files.append_to(path)\n    more.write(\"\\nfour\")\n}}\n\
             let source = files.read_from(path)\n\
             print(source.read_line())\n\
             for line in source {{\n    if line == \"three\" {{\n        continue\n    }}\n    print(\"for:\", line)\n}}\n\
             print(source.read_line(), source.read_bytes(3))\n\
             let again = files.read_from(path)\n\
             print(again.read_bytes(5), again.read_bytes(0), again.read_line())\n\
             try {{\n    print(again.read_bytes(-2))\n}} except ValueError as e {{\n    print(e.message)\n}}\n\
             try {{\n    let missing = files.read_from(path + \".none\")\n}} except FileError as e {{\n    print(e.message == \"there is no file `\" + path + \".none`\")\n}}\n"
        );
        let ran = joined(&source);
        assert_eq!(ran.result.map_err(|error| error.message), Ok(()));
        assert_eq!(
            ran.output,
            "one\nfor: two\nfor: four\nnone []\n[111, 110, 101, 13, 10] [] two\n\
             `read_bytes` was asked for -2 bytes, and a count of bytes is never below 0\ntrue\n"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `write` hands its text to the system before it gives back, so a
    /// full disk is an error at that `write`, which can be caught.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_write_that_meets_a_full_disk_raises_there() {
        let ran = joined(
            "import files\nlet full = files.write_to(\"/dev/full\")\n\
             try {\n    full.write(\"x\")\n} except FileError as e {\n    print(\"at the write:\", e.message)\n}\n\
             print(\"goes on\")\n",
        );
        assert_eq!(ran.output, "at the write: cannot write `/dev/full`\ngoes on\n");
    }

    #[test]
    fn a_reader_is_made_and_read_only_as_the_library_says() {
        // The number of the open file is the class's own, and the
        // built-ins under it are no name a program has (59.6).
        let refused = [
            ("import files\nlet r = files.Reader(3, \"x\")\n", "`files.Reader` has the private field `file`, so only the file that declares the class can build one. Write a function in that file that builds one and returns it, and call that function here"),
            ("import files\nlet r = files.read_from(\"x\")\nprint(r.file)\n", "`file` is private in the class `files.Reader`: only the file that declares the class can use it. To use it from another file, take the word `private` off where it is declared"),
            ("import files\nprint(files.handle_open(\"x\", 0))\n", "module `files` has no `handle_open`"),
            ("import files\nlet r = files.read_from(\"x\")\nprint(files.handle_line(1, \"x\"))\n", "module `files` has no `handle_line`"),
            ("from files import handle_close\n", "module `files` has no `handle_close`"),
            ("import files\nfor line in files.read_from(\"x\") {\n    print(line)\n}\n", "a `for` cannot read from a reader that nothing keeps: it would be closed at once. Give it a name first, `let source = files.read_from(path)`, and then write `for line in source`"),
            ("import files\nlet source = files.read_from(\"x\")\nfor i, line in source {\n    print(line)\n}\n", "a `for` over a `files.Reader` gives one line at a time, so it takes one name: `for line in source`"),
            ("import files\nlet out = files.write_to(\"x\")\nfor line in out {\n    print(line)\n}\n", "files.Writer cannot be looped over"),
        ];
        for (source, message) in refused {
            let errors = joined_errors(source);
            assert!(errors.iter().any(|error| error.starts_with(message)), "{errors:?}\n{source}");
        }
    }
}
