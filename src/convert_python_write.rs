//! `zeno convert program.py`: the part that writes Zeno from the tree of
//! Python that `convert_python.rs` reads (docs/convert.md, decisions
//! chapter 62).
//!
//! The file is a list of units, one for each piece. A unit is written as
//! Zeno, or it is kept: its lines go into one `inline python` block as
//! they were written, and the Zeno around it reaches what it defines
//! through the block (`py.load(...)`).
//!
//! What decides which:
//!
//! - A rule that is not there, or that cannot be sure, keeps the piece
//!   (`Err(Keep)`), with the line and the reason for the report.
//! - Python can only call Python. So a piece that a kept piece uses is
//!   kept too (`Units::spread`).
//! - Across the line between the two go numbers, text, `true`, `false`
//!   and `none`, and values of Python that Zeno only hands back. A list
//!   of Zeno given to Python would be a copy, so a piece that gives one
//!   is kept.
//! - What is written is then checked by Zeno's own checker, and a piece
//!   it finds a mistake in is kept.
//!
//! Types are worked out only as far as the rules need them: to know
//! whether `if xs:` is `if len(xs) > 0`, which `find` is meant, and what
//! `print` will show. They are worked out in rounds, each using what the
//! round before found, until nothing changes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::convert::{zeno_text, Converted, KeptPiece, Note, Report};
use crate::convert_python::*;

#[path = "convert_python_lines.rs"]
mod lines;
#[path = "convert_python_values.rs"]
mod values;

use values::{as_float, atom, error_kind, none_test, Val, ATOM};

/// The words of Zeno that are not words of Python: a name of Python
/// that is one of them cannot be a name in Zeno.
const ZENO_WORDS: &[&str] =
    &["let", "var", "const", "function", "unsafe", "private", "interface", "owned", "take", "strict", "match", "case", "enum", "raises", "true", "false", "none"];

/// The names Zeno has a meaning for before the program says anything,
/// and the modules that what is written may import.
const ZENO_OWN: &[&str] = &["print", "len", "range", "int", "float", "str", "abs", "min", "max", "sum", "allocate", "free", "char", "input", "math", "program", "python"];

// ------------------------------------------------------------------- Types

/// What a value is, as far as the converter knows.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    /// Not looked at yet: what a round begins with.
    Unset,
    Int,
    Float,
    Bool,
    Str,
    NoneT,
    /// An error that was caught.
    Error,
    List(Box<Ty>),
    Dict(Box<Ty>, Box<Ty>),
    Opt(Box<Ty>),
    /// An object of a class of the file that is written as Zeno.
    Object(String),
    /// What a function gives when it gives several values.
    Several(Vec<Ty>),
    /// A value of Zeno, of a type that is not known.
    Dyn,
    /// A value that may be Python's own: what a kept piece or a module
    /// of Python gave. Zeno hands it back and looks no further.
    Foreign,
}

fn join(a: &Ty, b: &Ty) -> Ty {
    match (a, b) {
        (Ty::Unset, other) | (other, Ty::Unset) => other.clone(),
        (a, b) if a == b => a.clone(),
        (Ty::Foreign, _) | (_, Ty::Foreign) => Ty::Foreign,
        (Ty::Dyn, _) | (_, Ty::Dyn) => Ty::Dyn,
        (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => Ty::Float,
        (Ty::NoneT, Ty::Opt(inner)) | (Ty::Opt(inner), Ty::NoneT) => Ty::Opt(inner.clone()),
        (Ty::NoneT, other) | (other, Ty::NoneT) => Ty::Opt(Box::new(other.clone())),
        (Ty::Opt(a), Ty::Opt(b)) => Ty::Opt(Box::new(join(a, b))),
        (Ty::Opt(a), b) | (b, Ty::Opt(a)) => Ty::Opt(Box::new(join(a, b))),
        (Ty::List(a), Ty::List(b)) => Ty::List(Box::new(join(a, b))),
        (Ty::Dict(ak, av), Ty::Dict(bk, bv)) => Ty::Dict(Box::new(join(ak, bk)), Box::new(join(av, bv))),
        (Ty::Several(a), Ty::Several(b)) if a.len() == b.len() => Ty::Several(a.iter().zip(b).map(|(a, b)| join(a, b)).collect()),
        _ => Ty::Dyn,
    }
}

/// The type as Zeno writes it, if every part of it is known.
fn zeno_type(ty: &Ty) -> Option<String> {
    Some(match ty {
        Ty::Int => "int".to_string(),
        Ty::Float => "float".to_string(),
        Ty::Bool => "bool".to_string(),
        Ty::Str => "str".to_string(),
        Ty::List(item) => format!("list[{}]", zeno_type(item)?),
        Ty::Dict(key, value) => format!("dict[{}, {}]", zeno_type(key)?, zeno_type(value)?),
        Ty::Opt(inner) if !matches!(**inner, Ty::Opt(_) | Ty::NoneT) => format!("{} or none", zeno_type(inner)?),
        Ty::Object(name) => name.clone(),
        _ => return None,
    })
}

fn described(ty: &Ty) -> &'static str {
    match ty {
        Ty::Int => "a whole number",
        Ty::Float => "a float",
        Ty::Bool => "a bool",
        Ty::Str => "a text",
        Ty::NoneT => "`None`",
        Ty::Error => "an error",
        Ty::List(_) => "a list",
        Ty::Dict(..) => "a dict",
        Ty::Opt(_) => "a value that may be `None`",
        Ty::Object(_) => "an object",
        Ty::Several(_) => "several values",
        Ty::Unset | Ty::Dyn => "a value whose type is not known",
        Ty::Foreign => "a value that only Python knows",
    }
}

/// Whether a value of this type is the same on both sides of the line
/// between Zeno and Python: a number, a text, a bool or `none`, of
/// which Python gets its own and loses nothing, or a value of Python
/// that Zeno only holds.
fn crosses(ty: &Ty) -> bool {
    match ty {
        Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::NoneT | Ty::Foreign | Ty::Unset => true,
        Ty::Opt(inner) => crosses(inner),
        _ => false,
    }
}

// ------------------------------------------------------- What the file has

/// Whether a value of type `value` goes into a place of Zeno that is
/// declared `declared` as it is, with nothing changed: the same type,
/// `none` or the inner type into a type that may be `none`, a list or
/// dict whose items do. A whole number into a float does not: Zeno
/// would have to make a float of it, and Python does not.
fn fits(value: &Ty, declared: &Ty) -> bool {
    match (value, declared) {
        (Ty::Unset, _) | (_, Ty::Unset) => true,
        (a, b) if a == b => true,
        (Ty::NoneT, Ty::Opt(_)) => true,
        (a, Ty::Opt(inner)) => fits(a, inner),
        (Ty::List(a), Ty::List(b)) => fits(a, b),
        (Ty::Dict(ak, av), Ty::Dict(bk, bv)) => fits(ak, bk) && fits(av, bv),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Sig {
    /// What every value given for each parameter is, with what the
    /// Python wrote of its type.
    params: Vec<Joined>,
    ret: Joined,
}

/// A type that is worked out from several places: what they join to,
/// and whether one of them is not that type as it stands.
#[derive(Debug, Clone, PartialEq)]
struct Joined {
    ty: Ty,
    loose: bool,
}

impl Default for Joined {
    fn default() -> Self {
        Joined { ty: Ty::Unset, loose: false }
    }
}

impl Joined {
    /// One more place gives a value of type `value`, when the round
    /// before found the type to be `before`.
    fn add(&mut self, value: &Ty, before: &Ty) {
        self.ty = join(&self.ty, value);
        if *before != Ty::Unset && !fits(value, before) {
            self.loose = true;
        }
    }

    /// What the rounds before found, with what this round found. Whether
    /// it is loose is this round's to say: it has seen more of the types.
    fn merged(&self, found: &Joined) -> Joined {
        Joined { ty: join(&self.ty, &found.ty), loose: found.loose }
    }
}

/// What is known of the file, from one round to the next.
#[derive(Debug, Clone, PartialEq, Default)]
struct Known {
    /// Each function by its name, and each method by `Class.name`.
    functions: BTreeMap<String, Sig>,
    /// Each field, by its class and its name.
    fields: BTreeMap<(String, String), Joined>,
    /// The type of each variable, by the function it is in and its name.
    vars: BTreeMap<(String, String), Ty>,
}

impl Known {
    /// What this round found, joined to what the rounds before found.
    fn merged(&self, found: &Known) -> Known {
        let mut all = self.clone();
        for (name, sig) in &found.functions {
            let into = all.functions.entry(name.clone()).or_default();
            for (at, par) in sig.params.iter().enumerate() {
                match into.params.get_mut(at) {
                    Some(old) => *old = old.merged(par),
                    None => into.params.push(par.clone()),
                }
            }
            into.ret = into.ret.merged(&sig.ret);
        }
        for (key, field) in &found.fields {
            let into = all.fields.entry(key.clone()).or_default();
            *into = into.merged(field);
        }
        for (key, ty) in &found.vars {
            let into = all.vars.entry(key.clone()).or_insert(Ty::Unset);
            *into = join(into, ty);
        }
        all
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Module {
    Math,
    Sys,
    /// A module whose names only say types, or ask for nothing.
    Quiet,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Def,
    Class,
    Import,
    Action,
}

struct Unit {
    piece: Piece,
    stmts: Option<Vec<Stmt>>,
    /// The line and the reason, if the unit is kept as Python.
    kept: Option<(usize, String)>,
    kind: Kind,
    label: String,
    /// The names it gives a meaning at the level of the file.
    defines: Vec<String>,
    /// Every name written in it.
    uses: HashSet<String>,
    /// `if __name__ == "__main__":`, whose block is the program itself.
    main_guard: bool,
    /// `LIMIT = 10`: a name given one fixed value, once in the whole
    /// file. It can be written on both sides of the line between Zeno
    /// and Python, since it is the same on both for ever.
    constant: bool,
    /// The names that Python looks up at the moment it makes this
    /// function or class, and not later when it is called.
    made_with: HashSet<String>,
    /// The names of a `for` at the left margin that are the loop's own:
    /// no other piece reads them, so that two loops with a variable of
    /// the same name have nothing to do with each other.
    own: HashSet<String>,
}

impl Unit {
    /// The statements the unit is, for Zeno: those in the block of an
    /// `if __name__ == "__main__":`, or its own.
    fn statements(&self) -> &[Stmt] {
        let Some(stmts) = &self.stmts else { return &[] };
        if self.main_guard {
            if let Some(Stmt { kind: S::If { branches, .. }, .. }) = stmts.first() {
                return &branches[0].1;
            }
        }
        stmts
    }
}

// --------------------------------------------------------- Walking a tree

fn each_expr(expr: &Expr, visit: &mut dyn FnMut(&Expr)) {
    visit(expr);
    match &expr.kind {
        E::List(items) | E::Tuple(items) => items.iter().for_each(|item| each_expr(item, visit)),
        E::Dict(pairs) => pairs.iter().for_each(|(key, value)| {
            each_expr(key, visit);
            each_expr(value, visit);
        }),
        E::Unary(_, of) | E::Attr(of, _) | E::Lambda(_, of) => each_expr(of, visit),
        E::Binary(_, left, right) | E::Logic(_, left, right) | E::Index(left, right) => {
            each_expr(left, visit);
            each_expr(right, visit);
        }
        E::Compare(first, rest) => {
            each_expr(first, visit);
            rest.iter().for_each(|(_, other)| each_expr(other, visit));
        }
        E::Call { function, args, named } => {
            each_expr(function, visit);
            args.iter().for_each(|arg| each_expr(arg, visit));
            named.iter().for_each(|(_, arg)| each_expr(arg, visit));
        }
        E::Slice { of, from, to } => {
            each_expr(of, visit);
            from.iter().for_each(|part| each_expr(part, visit));
            to.iter().for_each(|part| each_expr(part, visit));
        }
        E::IfElse { then, condition, otherwise } => {
            each_expr(then, visit);
            each_expr(condition, visit);
            each_expr(otherwise, visit);
        }
        E::ListOf { item, items, condition, .. } => {
            each_expr(item, visit);
            each_expr(items, visit);
            condition.iter().for_each(|part| each_expr(part, visit));
        }
        E::Name(_) | E::Number(_) | E::Str(_) | E::True | E::False | E::None => {}
    }
}

/// Every expression of some statements, those of the functions written
/// inside them too.
fn each_expr_of(stmts: &[Stmt], visit: &mut dyn FnMut(&Expr)) {
    for stmt in stmts {
        match &stmt.kind {
            S::Expr(expr) | S::Delete(expr) => each_expr(expr, visit),
            S::Assign { target, value } | S::Augmented { target, value, .. } => {
                each_expr(target, visit);
                each_expr(value, visit);
            }
            S::Annotated { target, value, .. } => {
                each_expr(target, visit);
                value.iter().for_each(|value| each_expr(value, visit));
            }
            S::If { branches, otherwise } => {
                for (condition, body, _) in branches {
                    each_expr(condition, visit);
                    each_expr_of(body, visit);
                }
                otherwise.iter().for_each(|body| each_expr_of(body, visit));
            }
            S::While { condition, body } => {
                each_expr(condition, visit);
                each_expr_of(body, visit);
            }
            S::For { items, body, .. } => {
                each_expr(items, visit);
                each_expr_of(body, visit);
            }
            S::Def(def) => {
                def.params.iter().for_each(|param| param.default.iter().for_each(|value| each_expr(value, visit)));
                each_expr_of(&def.body, visit);
            }
            S::Class { body, .. } => each_expr_of(body, visit),
            S::Return(value) | S::Raise(value) => value.iter().for_each(|value| each_expr(value, visit)),
            S::Try { body, handlers } => {
                each_expr_of(body, visit);
                handlers.iter().for_each(|handler| each_expr_of(&handler.body, visit));
            }
            S::Import { .. } | S::FromImport { .. } | S::Global(_) | S::Pass | S::Break | S::Continue => {}
        }
    }
}

/// A name, or fields of a name: `xs`, `self.rows`. Taking it twice does
/// nothing the first time did not.
fn is_plain_place(expr: &Expr) -> bool {
    match &expr.kind {
        E::Name(_) => true,
        E::Attr(of, _) => is_plain_place(of),
        _ => false,
    }
}

fn same_place(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (E::Name(a), E::Name(b)) => a == b,
        (E::Attr(a, x), E::Attr(b, y)) => x == y && same_place(a, b),
        _ => false,
    }
}

const CHANGES_A_LIST: &[&str] = &["append", "insert", "pop", "remove", "clear", "sort", "reverse", "extend", "update", "setdefault", "popitem"];

/// Whether these statements may change the list or dict at `place`
/// while a loop goes over it: by a method that changes it, by giving an
/// item a value, by giving the place another value, or by handing it to
/// a function, which may do any of those.
fn changes(stmts: &[Stmt], place: &Expr, values_only: bool) -> bool {
    let mut found = false;
    for stmt in stmts {
        match &stmt.kind {
            S::Assign { target, .. } | S::Augmented { target, .. } | S::Annotated { target, .. } => {
                found |= same_place(target, place);
                if let E::Index(of, _) = &target.kind {
                    found |= !values_only && same_place(of, place);
                }
            }
            S::Delete(target) => {
                if let E::Index(of, _) = &target.kind {
                    found |= same_place(of, place);
                }
            }
            S::If { branches, otherwise } => {
                found |= branches.iter().any(|(_, body, _)| changes(body, place, values_only));
                found |= otherwise.as_ref().is_some_and(|body| changes(body, place, values_only));
            }
            S::While { body, .. } | S::For { body, .. } => found |= changes(body, place, values_only),
            S::Try { body, handlers } => {
                found |= changes(body, place, values_only) || handlers.iter().any(|handler| changes(&handler.body, place, values_only));
            }
            _ => {}
        }
    }
    each_expr_of(stmts, &mut |expr| {
        if let E::Call { function, args, named } = &expr.kind {
            if let E::Attr(of, method) = &function.kind {
                found |= same_place(of, place) && CHANGES_A_LIST.contains(&method.as_str());
            }
            found |= args.iter().chain(named.iter().map(|(_, arg)| arg)).any(|arg| same_place(arg, place));
        }
    });
    found
}

/// Whether every way through these statements ends in a `return` or a
/// `raise`.
fn always_leaves(stmts: &[Stmt]) -> bool {
    match stmts.last().map(|stmt| &stmt.kind) {
        Some(S::Return(_) | S::Raise(_)) => true,
        Some(S::If { branches, otherwise: Some(otherwise) }) => branches.iter().all(|(_, body, _)| always_leaves(body)) && always_leaves(otherwise),
        Some(S::Try { body, handlers }) => always_leaves(body) && handlers.iter().all(|handler| always_leaves(&handler.body)),
        Some(S::While { condition: Expr { kind: E::True, .. }, body }) => !has_break(body),
        _ => false,
    }
}

/// Whether these statements never go on to the line after their block:
/// they return, raise, or go to the next time round a loop or out of it.
fn leaves_its_block(stmts: &[Stmt]) -> bool {
    match stmts.last().map(|stmt| &stmt.kind) {
        Some(S::Continue | S::Break) => true,
        Some(S::If { branches, otherwise: Some(otherwise) }) => branches.iter().all(|(_, body, _)| leaves_its_block(body)) && leaves_its_block(otherwise),
        _ => always_leaves(stmts),
    }
}

fn has_break(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        S::Break => true,
        S::If { branches, otherwise } => branches.iter().any(|(_, body, _)| has_break(body)) || otherwise.as_ref().is_some_and(|body| has_break(body)),
        S::Try { body, handlers } => has_break(body) || handlers.iter().any(|handler| has_break(&handler.body)),
        _ => false,
    })
}

// ------------------------------------------------- Where a name is declared

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ev {
    /// `name = value`.
    Bind,
    /// The same, where the line of Zeno gives the name a value twice.
    BindTwice,
    Aug,
    Read,
    /// The variable of a `for`, and the block it counts in.
    Loop(usize),
    /// The name of a caught error, and the block of its `except`.
    Caught(usize),
    /// A function written inside the function.
    Func,
}

struct Event {
    name: String,
    kind: Ev,
    /// The blocks the event is in, from the outermost in.
    path: Vec<usize>,
    /// The statement, at each of those levels, that the event is in.
    anchors: Vec<usize>,
    line: usize,
}

/// How each name of a function (or of the file) is declared in Zeno,
/// where Python declares none.
///
/// Python has one set of names for a whole function. Zeno has one for
/// each block, and a name is declared on a line (`let`, `var`) and can
/// be used from there to the end of its block. So a name is declared
/// where it gets its first value, if every other use of it is in that
/// block or further in. Otherwise it is declared, with the first value
/// of its type, before the outermost statement that holds all its
/// uses, and outside every loop, since a value that is not given anew
/// each time round must last from one time round to the next.
#[derive(Default)]
struct Plan {
    /// The statement and the name → whether it is `var`.
    declare: HashMap<(usize, String), bool>,
    /// The names declared before a statement, with a first value.
    hoist: HashMap<usize, Vec<String>>,
    locals: HashSet<String>,
    /// The `for` and its name → the name the loop of Zeno counts with,
    /// where the name of Python is a variable that outlasts the loop.
    renamed: HashMap<(usize, String), String>,
}

struct Planner {
    events: Vec<Event>,
    blocks: usize,
    /// The blocks that are the body of a loop.
    loops: HashSet<usize>,
    globals: HashSet<String>,
    /// The blocks that are the body of a `try` whose every `except`
    /// leaves (`continue`, `return`, ...) and reads nothing the body gives
    /// a value: past the `try`, every line of the body at its own level
    /// has run.
    whole_or_gone: HashSet<usize>,
}

fn address(stmt: &Stmt) -> usize {
    stmt as *const Stmt as usize
}

impl Planner {
    fn event(&mut self, name: &str, kind: Ev, path: &[usize], anchors: &[usize], line: usize) {
        if !self.globals.contains(name) {
            self.events.push(Event { name: name.to_string(), kind, path: path.to_vec(), anchors: anchors.to_vec(), line });
        }
    }

    fn reads(&mut self, expr: &Expr, path: &[usize], anchors: &[usize]) {
        // A name a `lambda` or a list made from a loop declares is its own.
        fn names(expr: &Expr, own: &mut Vec<String>, found: &mut Vec<(String, usize)>) {
            match &expr.kind {
                E::Name(name) => {
                    if !own.contains(name) {
                        found.push((name.clone(), expr.line));
                    }
                }
                E::Lambda(params, body) => {
                    let before = own.len();
                    own.extend(params.iter().cloned());
                    names(body, own, found);
                    own.truncate(before);
                }
                E::ListOf { item, name, items, condition } => {
                    names(items, own, found);
                    own.push(name.clone());
                    names(item, own, found);
                    condition.iter().for_each(|part| names(part, own, found));
                    own.pop();
                }
                E::List(items) | E::Tuple(items) => items.iter().for_each(|item| names(item, own, found)),
                E::Dict(pairs) => pairs.iter().for_each(|(key, value)| {
                    names(key, own, found);
                    names(value, own, found);
                }),
                E::Unary(_, of) | E::Attr(of, _) => names(of, own, found),
                E::Binary(_, left, right) | E::Logic(_, left, right) | E::Index(left, right) => {
                    names(left, own, found);
                    names(right, own, found);
                }
                E::Compare(first, rest) => {
                    names(first, own, found);
                    rest.iter().for_each(|(_, other)| names(other, own, found));
                }
                E::Call { function, args, named } => {
                    names(function, own, found);
                    args.iter().for_each(|arg| names(arg, own, found));
                    named.iter().for_each(|(_, arg)| names(arg, own, found));
                }
                E::Slice { of, from, to } => {
                    names(of, own, found);
                    from.iter().for_each(|part| names(part, own, found));
                    to.iter().for_each(|part| names(part, own, found));
                }
                E::IfElse { then, condition, otherwise } => {
                    names(condition, own, found);
                    names(then, own, found);
                    names(otherwise, own, found);
                }
                E::Number(_) | E::Str(_) | E::True | E::False | E::None => {}
            }
        }
        let mut found = Vec::new();
        names(expr, &mut Vec::new(), &mut found);
        for (name, line) in found {
            self.event(&name, Ev::Read, path, anchors, line);
        }
    }

    fn block(&mut self, stmts: &[Stmt], path: &mut Vec<usize>, anchors: &mut Vec<usize>) -> Kept<()> {
        for stmt in stmts {
            anchors.push(address(stmt));
            self.statement(stmt, path, anchors)?;
            anchors.pop();
        }
        Ok(())
    }

    fn inner(&mut self, stmts: &[Stmt], path: &mut Vec<usize>, anchors: &mut Vec<usize>, is_loop: bool) -> Kept<usize> {
        self.blocks += 1;
        let id = self.blocks;
        if is_loop {
            self.loops.insert(id);
        }
        path.push(id);
        self.block(stmts, path, anchors)?;
        path.pop();
        Ok(id)
    }

    fn statement(&mut self, stmt: &Stmt, path: &mut Vec<usize>, anchors: &mut Vec<usize>) -> Kept<()> {
        let line = stmt.line;
        match &stmt.kind {
            S::Expr(expr) => self.reads(expr, path, anchors),
            S::Assign { target, value } => {
                self.reads(value, path, anchors);
                let twice = matches!(value.kind, E::IfElse { .. });
                match &target.kind {
                    E::Name(name) => self.event(name, if twice { Ev::BindTwice } else { Ev::Bind }, path, anchors, line),
                    E::Tuple(names) => {
                        for name in names {
                            match &name.kind {
                                E::Name(name) => self.event(name, Ev::Bind, path, anchors, line),
                                _ => return keep(line, "several values given at once to something other than names"),
                            }
                        }
                    }
                    _ => self.reads(target, path, anchors),
                }
            }
            S::Augmented { target, value, .. } => {
                self.reads(value, path, anchors);
                match &target.kind {
                    E::Name(name) => self.event(name, Ev::Aug, path, anchors, line),
                    _ => self.reads(target, path, anchors),
                }
            }
            S::Annotated { target, value, .. } => {
                value.iter().for_each(|value| self.reads(value, path, anchors));
                match &target.kind {
                    E::Name(name) if value.is_some() => self.event(name, Ev::Bind, path, anchors, line),
                    E::Name(name) => return keep(line, format!("`{name}` is given a type and no value")),
                    _ => self.reads(target, path, anchors),
                }
            }
            S::If { branches, otherwise } => {
                for (condition, body, _) in branches {
                    self.reads(condition, path, anchors);
                    self.inner(body, path, anchors, false)?;
                }
                if let Some(body) = otherwise {
                    self.inner(body, path, anchors, false)?;
                }
            }
            S::While { condition, body } => {
                self.reads(condition, path, anchors);
                self.inner(body, path, anchors, true)?;
            }
            S::For { names, items, body } => {
                self.reads(items, path, anchors);
                let id = self.blocks + 1;
                for name in names {
                    self.event(name, Ev::Loop(id), path, anchors, line);
                }
                self.inner(body, path, anchors, true)?;
            }
            S::Def(def) => {
                self.event(&def.name, Ev::Func, path, anchors, line);
                // What the function inside uses of this one's names, it
                // uses from the line it is written on.
                let mut own: HashSet<String> = def.params.iter().map(|param| param.name.clone()).collect();
                bound_names(&def.body, &mut own);
                let mut used = Vec::new();
                each_expr_of(&def.body, &mut |expr| {
                    if let E::Name(name) = &expr.kind {
                        used.push((name.clone(), expr.line));
                    }
                });
                for (name, _) in used {
                    if !own.contains(&name) {
                        self.event(&name, Ev::Read, path, anchors, line);
                    }
                }
                for param in &def.params {
                    if let Some(default) = &param.default {
                        self.reads(default, path, anchors);
                    }
                }
            }
            S::Class { name, .. } => return keep(line, format!("the class `{name}` is written inside a function or a block")),
            S::Return(value) | S::Raise(value) => value.iter().for_each(|value| self.reads(value, path, anchors)),
            S::Try { body, handlers } => {
                let mut given = HashSet::new();
                bound_names(body, &mut given);
                let leaves = handlers.iter().all(|handler| {
                    let mut reads = false;
                    each_expr_of(&handler.body, &mut |expr| reads |= matches!(&expr.kind, E::Name(name) if given.contains(name)));
                    leaves_its_block(&handler.body) && !reads
                });
                let id = self.inner(body, path, anchors, false)?;
                if leaves {
                    self.whole_or_gone.insert(id);
                }
                for handler in handlers {
                    let id = self.blocks + 1;
                    if let Some(name) = &handler.name {
                        self.event(name, Ev::Caught(id), path, anchors, handler.line);
                    }
                    self.inner(&handler.body, path, anchors, false)?;
                }
            }
            S::Delete(target) => match &target.kind {
                E::Name(name) => return keep(line, format!("`del {name}` takes a name away, which Zeno does not do")),
                _ => self.reads(target, path, anchors),
            },
            S::Import { .. } | S::FromImport { .. } => return keep(line, "an `import` inside a function or a block"),
            S::Global(_) | S::Pass | S::Break | S::Continue => {}
        }
        Ok(())
    }
}

/// The names some statements give a value, as Python counts them: for
/// the whole function, whatever block they are in.
fn bound_names(stmts: &[Stmt], into: &mut HashSet<String>) {
    for stmt in stmts {
        match &stmt.kind {
            S::Assign { target, .. } | S::Augmented { target, .. } | S::Annotated { target, .. } => match &target.kind {
                E::Name(name) => {
                    into.insert(name.clone());
                }
                E::Tuple(names) => names.iter().for_each(|name| {
                    if let E::Name(name) = &name.kind {
                        into.insert(name.clone());
                    }
                }),
                _ => {}
            },
            S::If { branches, otherwise } => {
                branches.iter().for_each(|(_, body, _)| bound_names(body, into));
                otherwise.iter().for_each(|body| bound_names(body, into));
            }
            S::While { body, .. } => bound_names(body, into),
            S::For { names, body, .. } => {
                into.extend(names.iter().cloned());
                bound_names(body, into);
            }
            S::Def(def) => {
                into.insert(def.name.clone());
            }
            S::Class { name, .. } => {
                into.insert(name.clone());
            }
            S::Try { body, handlers } => {
                bound_names(body, into);
                for handler in handlers {
                    into.extend(handler.name.iter().cloned());
                    bound_names(&handler.body, into);
                }
            }
            S::Import { name, .. } => {
                into.insert(name.split('.').next().unwrap_or(name).to_string());
            }
            S::FromImport { names, .. } => into.extend(names.iter().map(|(_, known_as)| known_as.clone())),
            _ => {}
        }
    }
}

/// The names that `global` lines of some statements name.
fn global_names(stmts: &[Stmt], into: &mut HashSet<String>) {
    for stmt in stmts {
        match &stmt.kind {
            S::Global(names) => into.extend(names.iter().cloned()),
            S::If { branches, otherwise } => {
                branches.iter().for_each(|(_, body, _)| global_names(body, into));
                otherwise.iter().for_each(|body| global_names(body, into));
            }
            S::While { body, .. } | S::For { body, .. } => global_names(body, into),
            S::Try { body, handlers } => {
                global_names(body, into);
                handlers.iter().for_each(|handler| global_names(&handler.body, into));
            }
            _ => {}
        }
    }
}

/// Plan the names of one function, or of the file.
///
/// `blocks` are the lists of statements the function is made of, one
/// after another (a function has one; the file has one for each unit).
/// `given` are the names that have a value before the first statement:
/// the parameters. `changed_elsewhere` are names that some other
/// function gives a value, and `read_elsewhere` those another function
/// reads, which must then be declared outside every block.
///
/// `kills` go with `blocks`: before each block, the names that pieces
/// kept as Python gave a value there, each with the line of its piece.
/// That value is in Python only, so a line of Zeno that reads the name
/// after it must read a value that Zeno gave it after it.
fn plan(blocks: &[&[Stmt]], kills: &[HashMap<String, usize>], given: &[String], changed_elsewhere: &HashSet<String>, read_elsewhere: &HashSet<String>, known: &Known, scope: &str) -> Kept<Plan> {
    let mut planner = Planner { events: Vec::new(), blocks: 0, loops: HashSet::new(), globals: HashSet::new(), whole_or_gone: HashSet::new() };
    for stmts in blocks {
        global_names(stmts, &mut planner.globals);
    }
    let mut path = Vec::new();
    let mut anchors = Vec::new();
    let mut starts = Vec::new();
    for stmts in blocks {
        starts.push(planner.events.len());
        planner.block(stmts, &mut path, &mut anchors)?;
    }
    let mut made = Plan::default();
    outlasting(&mut planner, &mut made, given, changed_elsewhere, read_elsewhere, known);
    after_kept_loops(&planner, &starts, kills)?;
    for event in &planner.events {
        if given.contains(&event.name) && matches!(event.kind, Ev::Loop(_) | Ev::Caught(_) | Ev::Func) {
            return keep(event.line, format!("`{}` is a parameter, and the variable of a loop, an `except` or a function inside as well", event.name));
        }
    }
    let mut names: Vec<String> = Vec::new();
    for event in &planner.events {
        if !matches!(event.kind, Ev::Read) && !names.contains(&event.name) && !given.contains(&event.name) {
            names.push(event.name.clone());
        }
    }
    for name in names {
        let all: Vec<&Event> = planner.events.iter().filter(|event| event.name == name).collect();
        made.locals.insert(name.clone());
        let loops: Vec<usize> = all.iter().filter_map(|event| if let Ev::Loop(id) | Ev::Caught(id) = event.kind { Some(id) } else { None }).collect();
        // A name a `for` or an `except` declares is its block's own in
        // Zeno, and gone after it. The same name may be a variable as
        // well, if that variable begins after the last such block: then
        // nothing reads what the loop left in it.
        let of_a_loop = |event: &Event| matches!(event.kind, Ev::Loop(_) | Ev::Caught(_)) || event.path.iter().any(|id| loops.contains(id));
        let what = if all.iter().any(|event| matches!(event.kind, Ev::Loop(_))) { "the loop that counts with it" } else { "the `except` that names it" };
        if let Some(event) = all.iter().find(|event| of_a_loop(event) && !matches!(event.kind, Ev::Loop(_) | Ev::Caught(_) | Ev::Read)) {
            return keep(event.line, format!("`{name}` is given a value inside {what}"));
        }
        let events: Vec<&Event> = all.iter().copied().filter(|event| !of_a_loop(event)).collect();
        if events.is_empty() {
            continue;
        }
        let first = events[0];
        if !loops.is_empty() {
            if matches!(first.kind, Ev::Read | Ev::Aug) {
                return keep(first.line, format!("`{name}` is used outside {what}, where Zeno no longer has it"));
            }
            // A loop further down may count with the name, if it is
            // outside the block the variable is declared in.
            let begins = all.iter().position(|event| std::ptr::eq(*event, first)).unwrap_or(0);
            let outside = read_elsewhere.contains(&name) || changed_elsewhere.contains(&name);
            let in_its_block = !outside && events.iter().all(|event| event.path.starts_with(&first.path));
            if let Some(event) = all[begins..].iter().find(|event| matches!(event.kind, Ev::Loop(_) | Ev::Caught(_)) && (!in_its_block || event.path.starts_with(&first.path))) {
                return keep(event.line, format!("`{name}` is a variable, and further down the variable of a loop or of an `except` as well: Python has one variable for both"));
            }
        }
        if events.iter().any(|event| matches!(event.kind, Ev::Func)) {
            if let Some(other) = events.iter().find(|event| !matches!(event.kind, Ev::Func | Ev::Read)) {
                return keep(other.line, format!("`{name}` is a function written inside this one, and is given another value as well"));
            }
            if events.iter().filter(|event| matches!(event.kind, Ev::Func)).count() > 1 {
                return keep(first.line, format!("the function `{name}` is written twice"));
            }
            continue;
        }
        match first.kind {
            Ev::Bind | Ev::BindTwice => {}
            _ => return keep(first.line, format!("`{name}` is used here before a line above gives it a value")),
        }
        let binds = events.iter().filter(|event| matches!(event.kind, Ev::Bind | Ev::BindTwice)).count();
        let var = binds > 1 || events.iter().any(|event| matches!(event.kind, Ev::Aug | Ev::BindTwice)) || changed_elsewhere.contains(&name);
        let outside = read_elsewhere.contains(&name) || changed_elsewhere.contains(&name);
        let covers = events.iter().all(|event| event.path.starts_with(&first.path)) && !(outside && !first.path.is_empty());
        if covers {
            made.declare.insert((*first.anchors.last().unwrap_or(&0), name.clone()), var);
            continue;
        }
        if !loops.is_empty() {
            return keep(first.line, format!("`{name}` gets its first value inside a block, and is the variable of a loop above as well: what the loop left in it may be read"));
        }
        // The blocks every use is in, and of those the ones outside
        // every loop.
        let mut shared = first.path.len();
        if outside {
            shared = 0;
        }
        for event in &events {
            let same = event.path.iter().zip(&first.path).take_while(|(a, b)| a == b).count();
            shared = shared.min(same);
        }
        if let Some(in_loop) = first.path[..shared].iter().position(|id| planner.loops.contains(id)) {
            shared = in_loop;
        }
        let ty = known.vars.get(&(scope.to_string(), name.clone())).cloned().unwrap_or(Ty::Unset);
        if ty != Ty::Unset && first_value(&ty).is_none() {
            return keep(first.line, format!("`{name}` gets its first value inside a block and is used outside it, and Zeno has no first value for {}", described(&ty)));
        }
        made.hoist.entry(first.anchors[shared]).or_default().push(name.clone());
    }
    Ok(made)
}

/// The variable of a `for` is the loop's own in Zeno, and gone after it.
/// In Python it is a variable like any other: it can be read after the
/// loop, given another value inside it, and be a plain variable before.
/// Where a program does one of these, the loop of Zeno counts with a
/// name of its own, and its first line gives the value to the variable:
///
/// ```text
/// for line_item in lines {
///     line = line_item
/// ```
///
/// From there the name is planned as the plain variable it is.
fn outlasting(planner: &mut Planner, made: &mut Plan, given: &[String], changed_elsewhere: &HashSet<String>, read_elsewhere: &HashSet<String>, known: &Known) {
    let mut names: Vec<String> = Vec::new();
    for event in &planner.events {
        if matches!(event.kind, Ev::Loop(_)) && !names.contains(&event.name) && !given.contains(&event.name) {
            names.push(event.name.clone());
        }
    }
    let taken: HashSet<String> = planner.events.iter().map(|event| event.name.clone()).chain(given.iter().cloned()).chain(known.vars.keys().map(|(_, name)| name.clone())).chain(known.functions.keys().cloned()).collect();
    for name in names {
        let all: Vec<&Event> = planner.events.iter().filter(|event| event.name == name).collect();
        if all.iter().any(|event| matches!(event.kind, Ev::Caught(_) | Ev::Func)) {
            continue;
        }
        let loops: Vec<usize> = all.iter().filter_map(|event| if let Ev::Loop(id) = event.kind { Some(id) } else { None }).collect();
        let inside = |event: &Event| event.path.iter().any(|id| loops.contains(id));
        let changed_inside = all.iter().any(|event| inside(event) && !matches!(event.kind, Ev::Loop(_) | Ev::Read));
        let outside: Vec<usize> = (0..all.len()).filter(|&at| !inside(all[at]) && !matches!(all[at].kind, Ev::Loop(_))).collect();
        let outlasts = match outside.first() {
            None => changed_elsewhere.contains(&name) || read_elsewhere.contains(&name),
            Some(&first) => {
                // The plain variable is declared in the block of its first
                // value if every use is in there, and a loop further down
                // can have the name for its own only outside that block.
                let plain = all[first];
                let in_its_block = outside.iter().all(|&at| all[at].path.starts_with(&plain.path)) && !changed_elsewhere.contains(&name) && !read_elsewhere.contains(&name);
                matches!(plain.kind, Ev::Read | Ev::Aug)
                    || !in_its_block
                    || all[first..].iter().any(|event| matches!(event.kind, Ev::Loop(_)) && event.path.starts_with(&plain.path))
            }
        };
        if !(changed_inside || outlasts) {
            continue;
        }
        let mut counts_with = format!("{name}_item");
        let mut number = 2;
        while taken.contains(&counts_with) {
            counts_with = format!("{name}_item_{number}");
            number += 1;
        }
        for event in planner.events.iter_mut().filter(|event| event.name == name) {
            if let Ev::Loop(id) = event.kind {
                let of = event.anchors.last().copied().unwrap_or(0);
                made.renamed.insert((of, name.clone()), counts_with.clone());
                event.kind = Ev::Bind;
                event.path.push(id);
                event.anchors.push(of);
            }
        }
    }
}

/// A line of Zeno that reads a name after a piece kept as Python gave it
/// a value reads a value Zeno gave it after that piece: the first use
/// after it gives a value, and every other use runs after that.
fn after_kept_loops(planner: &Planner, starts: &[usize], kills: &[HashMap<String, usize>]) -> Kept<()> {
    for (at, killed) in kills.iter().enumerate() {
        for (name, &line) in killed {
            check_after_kept_loop(planner, starts, kills, name, starts[at], line)?;
        }
    }
    Ok(())
}

fn check_after_kept_loop(planner: &Planner, starts: &[usize], kills: &[HashMap<String, usize>], name: &str, from: usize, line: usize) -> Kept<()> {
    // Up to the next kept piece that gives the name a value.
    let to = starts.iter().zip(kills).filter(|(start, killed)| **start > from && killed.contains_key(name)).map(|(start, _)| *start).min().unwrap_or(planner.events.len());
    let all: Vec<&Event> = planner.events[from..to].iter().filter(|event| event.name == name).collect();
    // The loops of Zeno with the same variable have one of their own.
    let loops: Vec<usize> = all.iter().filter_map(|event| if let Ev::Loop(id) | Ev::Caught(id) = event.kind { Some(id) } else { None }).collect();
    let plain: Vec<&Event> = all.into_iter().filter(|event| !matches!(event.kind, Ev::Loop(_) | Ev::Caught(_)) && !event.path.iter().any(|id| loops.contains(id))).collect();
    // A use reads a value Zeno gave the name if a line above gave it
    // one for certain: it is in the same block as that line or further
    // in, a `try` that is run whole or left counting as no block.
    let solid = |event: &Event| event.path.iter().copied().filter(|id| !planner.whole_or_gone.contains(id)).collect::<Vec<usize>>();
    let given = |event: &Event| matches!(event.kind, Ev::Bind | Ev::BindTwice);
    let at = plain.iter().enumerate().find(|(at, event)| !given(event) && !plain[..*at].iter().any(|before| given(before) && solid(event).starts_with(&solid(before)))).map(|(_, event)| event.line);
    match at {
        Some(at) => keep(at, format!("`{name}` may still hold here the value that the piece at line {line} gave it, and that piece is kept as Python")),
        None => Ok(()),
    }
}

/// What a variable of this type holds before the program gives it a
/// value: how Zeno declares one that Python declares nowhere.
fn first_value(ty: &Ty) -> Option<String> {
    Some(match ty {
        Ty::Int => "0".to_string(),
        Ty::Float => "0.0".to_string(),
        Ty::Bool => "false".to_string(),
        Ty::Str => "\"\"".to_string(),
        Ty::List(_) => "[]".to_string(),
        Ty::Dict(..) => "[:]".to_string(),
        Ty::Opt(_) if zeno_type(ty).is_some() => "none".to_string(),
        _ => return None,
    })
}

// ------------------------------------------------------------ The writer

/// A field of a class, as `__init__` (or a `@dataclass`) makes it.
struct ShapeField<'a> {
    name: String,
    /// The parameter of `__init__` it takes its value from.
    param: String,
    /// Whether `Class(...)` takes a value for it.
    given: bool,
    default: Option<&'a Expr>,
    annotation: Option<&'a Expr>,
}

/// A class of the file that can be a class of Zeno: fields that are all
/// made in `__init__`, and methods.
struct Shape<'a> {
    fields: Vec<ShapeField<'a>>,
    methods: HashMap<String, &'a Def>,
    /// The methods in the order they are written, and their lines.
    order: Vec<String>,
    lines: HashMap<String, usize>,
    /// A `@dataclass`: two objects of it are equal when their fields are.
    data: bool,
}

/// An empty list or dict, or one of fixed values: what a field may have
/// for its first value.
fn is_fixed_or_empty(expr: &Expr) -> bool {
    match &expr.kind {
        E::List(items) => items.iter().all(lines::is_fixed),
        E::Dict(pairs) => pairs.iter().all(|(key, value)| lines::is_fixed(key) && lines::is_fixed(value)),
        _ => lines::is_fixed(expr),
    }
}

fn shape_of<'a>(stmt: &'a Stmt, is_dataclass: &dyn Fn(&Expr) -> bool) -> Kept<Shape<'a>> {
    let S::Class { name, bases, decorators, body } = &stmt.kind else {
        return keep(stmt.line, "this is not a class");
    };
    if bases.iter().any(|base| !matches!(&base.kind, E::Name(base) if base == "object")) {
        return keep(stmt.line, format!("the class `{name}` is made from another class, which Zeno does not have"));
    }
    let mut shape = Shape { fields: Vec::new(), methods: HashMap::new(), order: Vec::new(), lines: HashMap::new(), data: false };
    for decorator in decorators {
        if !is_dataclass(decorator) {
            return keep(stmt.line, format!("a decorator on the class `{name}` other than a plain `@dataclass`"));
        }
        shape.data = true;
    }
    for part in body {
        match &part.kind {
            S::Pass => {}
            S::Expr(Expr { kind: E::Str(_), .. }) => {}
            S::Annotated { target: Expr { kind: E::Name(field), .. }, annotation, value } if shape.data => {
                if value.as_ref().is_some_and(|value| !is_fixed_or_empty(value)) {
                    return keep(part.line, format!("the first value of the field `{field}` is more than a fixed value"));
                }
                shape.fields.push(ShapeField { name: field.clone(), param: field.clone(), given: true, default: value.as_ref(), annotation: Some(annotation) });
            }
            S::Def(def) => {
                if def.params.first().map(|param| param.name.as_str()) != Some("self") {
                    return keep(part.line, format!("`{}` of `{name}` does not take `self` first", def.name));
                }
                if def.name == "__init__" && !shape.data {
                    shape.fields = fields_of(def, name, part.line)?;
                } else if def.name.starts_with("__") && def.name.ends_with("__") {
                    return keep(part.line, format!("`{}`: a method that Python calls by itself, which Zeno has no match for", def.name));
                } else {
                    shape.methods.insert(def.name.clone(), def);
                    shape.order.push(def.name.clone());
                    shape.lines.insert(def.name.clone(), part.line);
                }
            }
            _ => return keep(part.line, format!("a line in the class `{name}` that is not a method: Zeno has no values that belong to the class itself")),
        }
    }
    Ok(shape)
}

/// The fields that `__init__` makes, if all it does is put each of its
/// parameters into a field and give other fields fixed values.
fn fields_of<'a>(init: &'a Def, class: &str, line: usize) -> Kept<Vec<ShapeField<'a>>> {
    let params = &init.params[1..];
    if let Some(param) = params.iter().find(|param| param.default.is_some()) {
        return keep(line, format!("`{}` of `__init__` of `{class}` has a default", param.name));
    }
    let mut given: Vec<Option<ShapeField>> = params.iter().map(|_| None).collect();
    let mut fixed: Vec<ShapeField> = Vec::new();
    let more = || keep(line, format!("`__init__` of `{class}` does more than put each of its parameters into a field and give other fields fixed values"));
    for stmt in &init.body {
        let (target, annotation, value) = match &stmt.kind {
            S::Pass | S::Expr(Expr { kind: E::Str(_), .. }) => continue,
            S::Assign { target, value } => (target, None, value),
            S::Annotated { target, annotation, value: Some(value) } => (target, Some(annotation), value),
            _ => return more(),
        };
        let E::Attr(of, field) = &target.kind else { return more() };
        if !matches!(&of.kind, E::Name(name) if name == "self") {
            return more();
        }
        if given.iter().flatten().chain(fixed.iter()).any(|known| &known.name == field) {
            return keep(stmt.line, format!("`__init__` of `{class}` gives `{field}` a value twice"));
        }
        match &value.kind {
            E::Name(param) if params.iter().any(|known| &known.name == param) => {
                let at = params.iter().position(|known| &known.name == param).unwrap_or(0);
                if given[at].is_some() {
                    return keep(stmt.line, format!("`__init__` of `{class}` puts `{param}` into two fields"));
                }
                given[at] = Some(ShapeField { name: field.clone(), param: param.clone(), given: true, default: None, annotation: annotation.or(params[at].annotation.as_ref()) });
            }
            _ if is_fixed_or_empty(value) => fixed.push(ShapeField { name: field.clone(), param: String::new(), given: false, default: Some(value), annotation }),
            _ => return more(),
        }
    }
    let mut fields = Vec::new();
    for (at, field) in given.into_iter().enumerate() {
        match field {
            Some(field) => fields.push(field),
            None => return keep(line, format!("`__init__` of `{class}` does not put `{}` into a field", params[at].name)),
        }
    }
    fields.extend(fixed);
    Ok(fields)
}

/// What stays the same through a round.
struct Env<'a> {
    known: &'a Known,
    /// The last round: types are as known as they will be, and what is
    /// written is what is kept.
    last: bool,
    /// `import math`, `import sys`, by the name the file knows each by.
    modules: &'a HashMap<String, Module>,
    /// `from math import sqrt`: the name here, the module, the name there.
    from_names: &'a HashMap<String, (Module, String)>,
    /// The names that live in Python, in the block, and what a call of
    /// each gives.
    kept: &'a HashMap<String, Ty>,
    /// The name of the block.
    block: &'a str,
    defs: &'a HashMap<String, &'a Def>,
    shapes: &'a HashMap<String, Shape<'a>>,
    /// The variables of the file that are written as Zeno.
    module_vars: &'a HashSet<String>,
    module_plan: &'a Plan,
    comments: &'a [Comment],
    /// What each function and method of the file may change: see
    /// `effects_of`.
    effects: &'a HashMap<String, HashSet<String>>,
    /// Every name written in the file of Python.
    file_names: &'a HashSet<String>,
}

struct Writer<'a> {
    env: &'a Env<'a>,
    /// What this round finds of the types.
    found: Known,
    notes: Vec<(Note, usize)>,
    /// The kinds of note the file has a comment for already.
    noted: HashSet<Note>,
    /// The variables that are made floats from their first value: those
    /// that have a comment already, and those the statement being
    /// written found.
    float_names: HashSet<String>,
    floats: Vec<String>,
    lines: Vec<(usize, String)>,
    indent: usize,
    /// The function being written: `name`, `Class.name`, or nothing for
    /// the file itself.
    scope: String,
    in_function: bool,
    class: Option<String>,
    plan: Plan,
    params: HashSet<String>,
    globals: HashSet<String>,
    /// The variables of lists made from loops, innermost last.
    temps: Vec<(String, Ty)>,
    /// The names that a test has shown not to be `none`, from there on.
    narrow: Vec<String>,
    /// The name each `except` around this line gives its error.
    caught: Vec<Option<String>>,
    /// The quote texts are written between: `'` inside an f-string.
    quote: char,
    /// How many times Python was asked something, so far.
    foreign_calls: usize,
    comment_at: usize,
    need_math: bool,
    need_program: bool,
    /// The functions the file gets written out for it (`Helper`).
    helpers: BTreeSet<Helper>,
}

impl<'a> Writer<'a> {
    fn new(env: &'a Env<'a>) -> Self {
        Writer {
            env,
            found: Known::default(),
            notes: Vec::new(),
            noted: HashSet::new(),
            float_names: HashSet::new(),
            floats: Vec::new(),
            lines: Vec::new(),
            indent: 0,
            scope: String::new(),
            in_function: false,
            class: None,
            plan: Plan::default(),
            params: HashSet::new(),
            globals: HashSet::new(),
            temps: Vec::new(),
            narrow: Vec::new(),
            caught: Vec::new(),
            quote: '"',
            foreign_calls: 0,
            comment_at: 0,
            need_math: false,
            need_program: false,
            helpers: BTreeSet::new(),
        }
    }
}

// ---------------------------------------------------------------- Pieces

const AUGMENTED: &[&str] = &["+=", "-=", "*=", "/=", "//=", "%=", "**=", "&=", "|=", "^=", "<<=", ">>=", "@="];

fn is_name(tok: &Tok) -> Option<&str> {
    match &tok.kind {
        T::Name(name) if !is_python_keyword(name) => Some(name),
        _ => None,
    }
}

fn is_word(tok: Option<&Tok>, word: &str) -> bool {
    matches!(tok.map(|tok| &tok.kind), Some(T::Name(name)) if name == word)
}

fn is_op(tok: Option<&Tok>, op: &str) -> bool {
    matches!(tok.map(|tok| &tok.kind), Some(T::Op(found)) if *found == op)
}

/// The names one line of Python gives a meaning, read from its tokens
/// alone: this must work for a piece the parser gave up on.
fn line_defines(line: &[Tok], into: &mut Vec<String>) {
    let mut add = |name: &str| {
        if !into.iter().any(|known| known == name) {
            into.push(name.to_string());
        }
    };
    let first = match line.first().map(|tok| &tok.kind) {
        Some(T::Name(word)) => word.as_str(),
        _ => "",
    };
    // `NAME :=` gives a name a value wherever it is.
    for (at, tok) in line.iter().enumerate() {
        if let (Some(name), true) = (is_name(tok), is_op(line.get(at + 1), ":=")) {
            add(name);
        }
    }
    // Where the header of a block ends, if the block is on the same line.
    let colon = {
        let mut depth = 0usize;
        line.iter().position(|tok| {
            match &tok.kind {
                T::Op("(" | "[" | "{") => depth += 1,
                T::Op(")" | "]" | "}") => depth = depth.saturating_sub(1),
                T::Op(":") if depth == 0 => return true,
                _ => {}
            }
            false
        })
    };
    match first {
        "for" | "async" | "with" | "except" | "if" | "elif" | "else" | "while" | "try" | "finally" => {
            let header = &line[..colon.unwrap_or(line.len())];
            match first {
                "for" | "async" => {
                    let from = header.iter().position(|tok| is_word(Some(tok), "for")).map_or(0, |at| at + 1);
                    let to = header.iter().position(|tok| is_word(Some(tok), "in")).unwrap_or(header.len());
                    header.get(from..to).unwrap_or(&[]).iter().filter_map(is_name).for_each(&mut add);
                }
                "with" | "except" => {
                    for (at, tok) in header.iter().enumerate() {
                        if is_word(Some(tok), "as") {
                            header.get(at + 1).and_then(is_name).into_iter().for_each(&mut add);
                        }
                    }
                }
                _ => {}
            }
            if let Some(colon) = colon {
                if colon + 1 < line.len() {
                    line_defines(&line[colon + 1..], into);
                }
            }
        }
        "import" => {
            let mut at = 1;
            while at < line.len() {
                let start = at;
                while at < line.len() && !is_op(line.get(at), ",") {
                    at += 1;
                }
                let part = &line[start..at];
                match part.iter().position(|tok| is_word(Some(tok), "as")) {
                    Some(as_at) => part.get(as_at + 1).and_then(is_name).into_iter().for_each(&mut add),
                    None => part.first().and_then(is_name).into_iter().for_each(&mut add),
                }
                at += 1;
            }
        }
        "from" => {
            let Some(import) = line.iter().position(|tok| is_word(Some(tok), "import")) else { return };
            let mut at = import + 1;
            while at < line.len() {
                let start = at;
                while at < line.len() && !is_op(line.get(at), ",") {
                    at += 1;
                }
                let part: Vec<&Tok> = line[start..at].iter().filter(|tok| !matches!(tok.kind, T::Op("(" | ")"))).collect();
                match part.iter().position(|tok| is_word(Some(tok), "as")) {
                    Some(as_at) => part.get(as_at + 1).and_then(|tok| is_name(tok)).into_iter().for_each(&mut add),
                    None => part.first().and_then(|tok| is_name(tok)).into_iter().for_each(&mut add),
                }
                at += 1;
            }
        }
        "def" | "class" => line.get(1).and_then(is_name).into_iter().for_each(&mut add),
        "global" | "nonlocal" | "del" => {
            for (at, tok) in line.iter().enumerate().skip(1) {
                let alone = (at == 1 || is_op(line.get(at - 1), ",")) && (at + 1 == line.len() || is_op(line.get(at + 1), ","));
                if let (Some(name), true) = (is_name(tok), alone) {
                    add(name);
                }
            }
        }
        _ => {
            // `a = 1`, `a, b = c`, `a = b = 0`, `a += 1`, `a: int = 1`:
            // the names in front of the last `=`, outside every bracket.
            let mut depth = 0usize;
            let mut last = None;
            for (at, tok) in line.iter().enumerate() {
                match &tok.kind {
                    T::Op("(" | "[" | "{") => depth += 1,
                    T::Op(")" | "]" | "}") => depth = depth.saturating_sub(1),
                    T::Op(op) if depth == 0 && (*op == "=" || AUGMENTED.contains(op)) => last = Some(at),
                    _ => {}
                }
            }
            let Some(last) = last else { return };
            let mut depth = 0usize;
            let mut in_type = false;
            for (at, tok) in line[..last].iter().enumerate() {
                match &tok.kind {
                    T::Op("(" | "[" | "{") => depth += 1,
                    T::Op(")" | "]" | "}") => depth = depth.saturating_sub(1),
                    T::Op(":") if depth == 0 => in_type = true,
                    T::Op("=") if depth == 0 => in_type = false,
                    _ => {}
                }
                let Some(name) = is_name(tok) else { continue };
                let part_of_more = (at > 0 && is_op(line.get(at - 1), ".")) || matches!(line.get(at + 1).map(|tok| &tok.kind), Some(T::Op("." | "(" | "[")));
                if depth == 0 && !in_type && !part_of_more {
                    add(name);
                }
            }
        }
    }
}

/// The names a line takes from around it: not what comes after a dot,
/// and not the variable of a list made from a loop, inside the brackets
/// of that list.
fn names_on(line: &[Tok], into: &mut HashSet<String>) {
    // Each `[x ... for x in ...]`: where its brackets are, and the name.
    let mut own: Vec<(usize, usize, &str)> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut waiting: Vec<(usize, Vec<&str>)> = Vec::new();
    for (at, tok) in line.iter().enumerate() {
        match &tok.kind {
            T::Op("(" | "[" | "{") => open.push(at),
            T::Op(")" | "]" | "}") => {
                if let Some(opened) = open.pop() {
                    while waiting.last().is_some_and(|(from, _)| *from == opened) {
                        for name in waiting.pop().map(|(_, names)| names).unwrap_or_default() {
                            own.push((opened, at, name));
                        }
                    }
                }
            }
            T::Name(word) if word == "for" => {
                if let Some(&opened) = open.last() {
                    let names = line[at + 1..].iter().take_while(|tok| !is_word(Some(tok), "in")).filter_map(is_name).collect();
                    waiting.push((opened, names));
                }
            }
            _ => {}
        }
    }
    for (at, tok) in line.iter().enumerate() {
        let Some(name) = is_name(tok) else { continue };
        let after_dot = at > 0 && is_op(line.get(at - 1), ".");
        let of_a_loop = own.iter().any(|(from, to, own)| *from < at && at < *to && *own == name);
        if !after_dot && !of_a_loop {
            into.insert(name.to_string());
        }
    }
}

/// The header of a `def` cut in three: its parameters, the other names
/// in it (defaults and types, which belong to whatever is around the
/// function), and what comes after its `:` on the same line.
fn header_of<'a>(line: &'a [Tok], outside: &mut HashSet<String>) -> (HashSet<String>, &'a [Tok]) {
    let mut params = HashSet::new();
    let mut depth = 0usize;
    let mut end = line.len();
    let start = line.iter().position(|tok| is_word(Some(tok), "def")).map_or(0, |at| at + 2);
    for (at, tok) in line.iter().enumerate().skip(start) {
        match &tok.kind {
            T::Op("(" | "[" | "{") => depth += 1,
            T::Op(")" | "]" | "}") => depth = depth.saturating_sub(1),
            T::Op(":") if depth == 0 => {
                end = at;
                break;
            }
            _ => {}
        }
        let Some(name) = is_name(tok) else { continue };
        let is_param = depth == 1 && matches!(line.get(at - 1).map(|tok| &tok.kind), Some(T::Op("(" | "," | "*" | "**")));
        if is_param {
            params.insert(name.to_string());
        } else if !is_op(line.get(at - 1), ".") {
            outside.insert(name.to_string());
        }
    }
    (params, line.get(end + 1..).unwrap_or(&[]))
}

/// The names that some lines of Python take from outside themselves.
/// Each line comes with how far in it is. A function has names of its
/// own, its parameters and what it gives a value, and those are not
/// taken from outside: so a kept function with a parameter `name` does
/// not use the `name` of the file.
fn free_in(lines: &[(usize, &[Tok])]) -> HashSet<String> {
    let mut uses = HashSet::new();
    let mut at = 0;
    while at < lines.len() {
        let (level, line) = lines[at];
        let after = (at + 1..lines.len()).find(|&next| lines[next].0 <= level).unwrap_or(lines.len());
        let first = line.iter().find(|tok| !is_word(Some(tok), "async"));
        if is_word(first, "def") {
            let (params, same_line) = header_of(line, &mut uses);
            let mut body: Vec<(usize, &[Tok])> = Vec::new();
            if !same_line.is_empty() {
                body.push((level + 1, same_line));
            }
            body.extend_from_slice(&lines[at + 1..after]);
            // Its own names: the parameters, and what its lines give a
            // value, but not the lines of the functions inside it.
            let mut own: Vec<String> = Vec::new();
            let mut not_own: Vec<String> = Vec::new();
            let mut skip_below: Option<usize> = None;
            for (inner_level, inner) in &body {
                if skip_below.is_some_and(|below| *inner_level > below) {
                    continue;
                }
                skip_below = None;
                if is_word(inner.first(), "global") || is_word(inner.first(), "nonlocal") {
                    line_defines(inner, &mut not_own);
                    continue;
                }
                let begins = inner.iter().find(|tok| !is_word(Some(tok), "async"));
                if is_word(begins, "def") || is_word(begins, "class") {
                    let name_at = inner.iter().position(|tok| is_word(Some(tok), "def") || is_word(Some(tok), "class")).map_or(0, |at| at + 1);
                    inner.get(name_at).and_then(is_name).into_iter().for_each(|name| own.push(name.to_string()));
                    skip_below = Some(*inner_level);
                    continue;
                }
                line_defines(inner, &mut own);
            }
            for name in free_in(&body) {
                if not_own.contains(&name) || !(params.contains(&name) || own.contains(&name)) {
                    uses.insert(name);
                }
            }
            at = after;
        } else if is_word(first, "class") {
            // What the lines of a class give a value is not seen by its
            // methods, so nothing else is taken away; but the name that
            // such a line gives a value (`title: str`, `count = 0`) is
            // the class's own, and not read from the file.
            names_on(line.get(2..).unwrap_or(&[]), &mut uses);
            let body: Vec<(usize, &[Tok])> = lines[at + 1..after]
                .iter()
                .map(|&(inner_level, inner)| {
                    let gives = inner_level == level + 1 && is_name(&inner[0]).is_some() && (is_op(inner.get(1), ":") || is_op(inner.get(1), "="));
                    (inner_level, if gives { &inner[1..] } else { inner })
                })
                .collect();
            uses.extend(free_in(&body));
            at = after;
        } else {
            names_on(line, &mut uses);
            at += 1;
        }
    }
    uses
}

/// What a piece gives a meaning at the level of the file, every name
/// it takes from the rest of the file, and the names it needs at the
/// moment it is made.
fn names_of(toks: &[Tok], kind: Kind) -> (Vec<String>, HashSet<String>, HashSet<String>, HashSet<String>) {
    let mut made_with = HashSet::new();
    let mut level = 0usize;
    let mut lines: Vec<(usize, &[Tok])> = Vec::new();
    let mut start = 0;
    for (at, tok) in toks.iter().enumerate() {
        let ends_line = matches!(tok.kind, T::Newline | T::Indent | T::Dedent | T::End);
        if ends_line {
            if start < at {
                lines.push((level, &toks[start..at]));
            }
            start = at + 1;
        }
        match &tok.kind {
            T::Indent => level += 1,
            T::Dedent => level = level.saturating_sub(1),
            _ => {}
        }
        let Some(name) = is_name(tok) else { continue };
        if at > 0 && is_op(toks.get(at - 1), ".") {
            continue;
        }
        let at_making = match kind {
            Kind::Def => level == 0,
            Kind::Class => level <= 1,
            _ => false,
        };
        if at_making {
            made_with.insert(name.to_string());
        }
    }
    if start < toks.len() {
        lines.push((level, &toks[start..]));
    }
    // A parameter is not looked up when the function is made: only what
    // its header says beside the parameters, a first value or a type.
    if matches!(kind, Kind::Def | Kind::Class) {
        let header_level = if kind == Kind::Def { 0 } else { 1 };
        let mut params: HashSet<String> = HashSet::new();
        let mut outside: HashSet<String> = HashSet::new();
        for (level, line) in &lines {
            if *level > header_level {
                continue;
            }
            let first = line.iter().find(|tok| !is_word(Some(tok), "async"));
            if is_word(first, "def") {
                params.extend(header_of(line, &mut outside).0);
            } else {
                names_on(line, &mut outside);
            }
        }
        made_with.retain(|name| outside.contains(name) || !params.contains(name));
    }
    let uses = free_in(&lines);
    let mut defines = Vec::new();
    match kind {
        Kind::Def | Kind::Class => {
            // The name after `def` or `class`, and what `global` names.
            let word = if kind == Kind::Def { "def" } else { "class" };
            if let Some(at) = toks.iter().position(|tok| is_word(Some(tok), word)) {
                if let Some(name) = toks.get(at + 1).and_then(is_name) {
                    defines.push(name.to_string());
                    made_with.remove(name);
                }
            }
            for (_, line) in &lines {
                if is_word(line.first(), "global") {
                    line_defines(line, &mut defines);
                }
            }
        }
        _ => {
            for (_, line) in &lines {
                line_defines(line, &mut defines);
            }
        }
    }
    let own = if kind == Kind::Action { loop_names(&lines) } else { HashSet::new() };
    (defines, uses, made_with, own)
}

/// The variables of a `for` at the left margin, if every line that reads
/// one comes after the loop has given it a value: the loop has no
/// `else`, the names are not in what it goes over, and nothing in it
/// could read them later (a function, a `lambda`) or by another way.
fn loop_names(lines: &[(usize, &[Tok])]) -> HashSet<String> {
    const READS_LATER: &[&str] = &["lambda", "def", "class", "global", "nonlocal", "del", "exec", "eval", "globals", "locals", "vars"];
    let mut own = HashSet::new();
    let Some((_, first)) = lines.first() else { return own };
    if !is_word(first.first(), "for") || lines.iter().skip(1).any(|(level, _)| *level == 0) {
        return own;
    }
    let Some(end) = first.iter().position(|tok| is_word(Some(tok), "in")) else { return own };
    // What the loop goes over runs once, before the loop: a `lambda` there
    // cannot keep the loop's variable for later. Past the `:`, it can.
    let mut depth = 0usize;
    let colon = first.iter().enumerate().skip(end).find_map(|(at, tok)| {
        match &tok.kind {
            T::Op("(" | "[" | "{") => depth += 1,
            T::Op(")" | "]" | "}") => depth = depth.saturating_sub(1),
            T::Op(":") if depth == 0 => return Some(at),
            _ => {}
        }
        None
    });
    let body = first[colon.map_or(first.len(), |at| at + 1)..].iter().chain(lines.iter().skip(1).flat_map(|(_, line)| line.iter()));
    if first[..end].iter().chain(body).any(|tok| matches!(&tok.kind, T::Name(word) if READS_LATER.contains(&word.as_str()))) {
        return own;
    }
    for tok in &first[1..end] {
        match (&tok.kind, is_name(tok)) {
            (_, Some(name)) => {
                own.insert(name.to_string());
            }
            (T::Op("," | "(" | ")"), _) => {}
            _ => return HashSet::new(),
        }
    }
    for tok in &first[end + 1..] {
        if let Some(name) = is_name(tok) {
            own.remove(name);
        }
    }
    own
}

fn is_main_guard(stmts: &[Stmt]) -> bool {
    let [Stmt { kind: S::If { branches, otherwise: None }, .. }] = stmts else { return false };
    let [(condition, _, _)] = branches.as_slice() else { return false };
    let E::Compare(first, rest) = &condition.kind else { return false };
    let [(op, other)] = rest.as_slice() else { return false };
    *op == "==" && matches!(&first.kind, E::Name(name) if name == "__name__") && matches!(&other.kind, E::Str(lit) if lit.body == "__main__" && lit.prefix.is_empty())
}

/// The modules whose names Zeno has its own words for, or that only say
/// types.
fn known_module(module: &str, from: bool) -> Option<Module> {
    match (module, from) {
        ("math", _) => Some(Module::Math),
        ("sys", _) => Some(Module::Sys),
        ("typing" | "__future__" | "dataclasses", true) => Some(Module::Quiet),
        _ => None,
    }
}

fn units_of(lexed: &Lexed, source: &[&str]) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    for piece in pieces(lexed) {
        let toks = &lexed.toks[piece.toks.clone()];
        let (stmts, mut kept) = match Parser::new(toks).piece() {
            Ok(stmts) => (Some(stmts), None),
            Err(keep) => (None, Some((keep.line, keep.why))),
        };
        let kind = match &stmts {
            Some(stmts) => match stmts.as_slice() {
                [Stmt { kind: S::Def(_), .. }] => Kind::Def,
                [Stmt { kind: S::Class { .. }, .. }] => Kind::Class,
                all if !all.is_empty() && all.iter().all(|stmt| matches!(stmt.kind, S::Import { .. } | S::FromImport { .. })) => Kind::Import,
                _ => Kind::Action,
            },
            None => {
                // The first word of the statement, past any decorators.
                let mut at_start = true;
                let mut found = Kind::Action;
                for tok in toks {
                    match &tok.kind {
                        T::Newline => at_start = true,
                        T::Name(word) if at_start => {
                            found = match word.as_str() {
                                "def" | "async" => Kind::Def,
                                "class" => Kind::Class,
                                "import" | "from" => Kind::Import,
                                _ => Kind::Action,
                            };
                            break;
                        }
                        T::Op("@") if at_start => at_start = false,
                        _ if at_start => break,
                        _ => {}
                    }
                }
                found
            }
        };
        let (defines, uses, made_with, own) = names_of(toks, kind);
        let first_line = source.get(piece.line.saturating_sub(1)).map_or("", |line| line.trim());
        let label = match (kind, defines.first()) {
            (Kind::Def, Some(name)) => format!("`def {name}`"),
            (Kind::Class, Some(name)) => format!("`class {name}`"),
            _ if first_line.chars().count() > 48 => format!("`{}...`", first_line.chars().take(45).collect::<String>().trim_end()),
            _ => format!("`{first_line}`"),
        };
        // A module that Zeno has no words for stays an `import` of
        // Python, and what uses it reaches it through the block.
        if let (Kind::Import, Some(stmts), None) = (kind, &stmts, &kept) {
            for stmt in stmts {
                let (module, from) = match &stmt.kind {
                    S::Import { module, .. } => (module, false),
                    S::FromImport { module, .. } => (module, true),
                    _ => continue,
                };
                if known_module(module, from).is_none() {
                    kept = Some((stmt.line, format!("`{module}` is a module of Python, and what uses it reaches it through the block")));
                }
            }
        }
        let main_guard = stmts.as_ref().is_some_and(|stmts| is_main_guard(stmts));
        units.push(Unit { piece, stmts, kept, kind, label, defines, uses, main_guard, constant: false, made_with, own });
    }
    // What a function or a class reads of the file is read when it is
    // called, which can be after any loop: a name it reads is no loop's own.
    let read_by_functions: HashSet<String> = units.iter().filter(|unit| matches!(unit.kind, Kind::Def | Kind::Class)).flat_map(|unit| unit.uses.iter().cloned()).collect();
    for unit in &mut units {
        unit.own.retain(|name| !read_by_functions.contains(name));
    }
    // A name with one meaning in the whole file.
    let mut count: HashMap<&str, usize> = HashMap::new();
    for unit in &units {
        for name in &unit.defines {
            *count.entry(name).or_default() += 1;
        }
    }
    let once: HashSet<String> = count.into_iter().filter(|(_, count)| *count == 1).map(|(name, _)| name.to_string()).collect();
    for unit in &mut units {
        if let Some([Stmt { kind: S::Assign { target: Expr { kind: E::Name(name), .. }, value }, .. }]) = unit.stmts.as_deref() {
            unit.constant = lines::is_fixed(value) && once.contains(name);
        }
        if matches!(unit.kind, Kind::Def | Kind::Class) && unit.kept.is_none() {
            if let Some(name) = unit.defines.first() {
                if !once.contains(name) {
                    unit.kept = Some((unit.piece.line, format!("`{name}` is given a meaning in more than one place in the file")));
                }
            }
        }
    }
    units
}

/// The variable of a loop that is kept as Python, and that no piece of
/// Python reads from it: what the loop leaves in it is not used by the
/// Python, and the plan of the file sees to it that no line of Zeno
/// reads it either (`plan`, `kills`).
fn kept_own(unit: &Unit, name: &str) -> bool {
    unit.kept.is_some() && unit.own.contains(name)
}

/// Python can only call Python, and a variable lives on one side. So:
///
/// - A piece that gives a name a value that a kept piece reads is kept
///   too, if the kept piece can run after it: it is further down, or it
///   is a function or a class, which run when they are called.
/// - A piece that gives a value to a name that a kept function or class
///   gives a meaning, or to a name that a kept piece gives a value and a
///   function of Zeno reads, is kept too.
///
/// A name that a kept piece and a piece of Zeno both give a value is
/// otherwise a variable on each side: the plan of the file sees to it
/// that a line of Zeno past the kept piece reads only what Zeno gave it
/// there (`plan`, `kills`). An `import` and a fixed value are written on
/// both sides, and the variable of a kept loop is that loop's own
/// (`kept_own`).
fn spread(units: &mut [Unit]) {
    loop {
        let read_by_functions: HashSet<&String> = units.iter().filter(|unit| unit.kept.is_none() && matches!(unit.kind, Kind::Def | Kind::Class)).flat_map(|unit| &unit.uses).collect();
        let mut more: Vec<(usize, usize, String)> = Vec::new();
        for (at, unit) in units.iter().enumerate() {
            if unit.kept.is_some() || unit.kind == Kind::Import || unit.constant {
                continue;
            }
            for (kept_at, kept) in units.iter().enumerate().filter(|(_, other)| other.kept.is_some()) {
                let line = kept.piece.line;
                let runs_after = kept_at > at || matches!(kept.kind, Kind::Def | Kind::Class);
                if let Some(name) = unit.defines.iter().find(|name| runs_after && kept.uses.contains(*name) && !kept_own(kept, name)) {
                    more.push((at, unit.piece.line, format!("`{name}` is used by the piece at line {line}, which is kept as Python, and Python can call only Python")));
                    break;
                }
                let one_side = |name: &String| matches!(kept.kind, Kind::Def | Kind::Class) || read_by_functions.contains(name);
                if let Some(name) = unit.defines.iter().find(|name| kept.kind != Kind::Import && kept.defines.contains(name) && !kept_own(kept, name) && one_side(name)) {
                    more.push((at, unit.piece.line, format!("`{name}` is also given a value by the piece at line {line}, which is kept as Python")));
                    break;
                }
            }
        }
        if more.is_empty() {
            return;
        }
        for (at, line, why) in more {
            units[at].kept = Some((line, why));
        }
    }
}

// ----------------------------------------------------------------- Rounds

/// The tables that do not change from round to round.
struct Tables<'a> {
    modules: HashMap<String, Module>,
    from_names: HashMap<String, (Module, String)>,
    kept: HashMap<String, Ty>,
    defs: HashMap<String, &'a Def>,
    shapes: HashMap<String, Shape<'a>>,
    module_vars: HashSet<String>,
    /// The names that some function gives a value with `global`.
    changed_in_functions: HashSet<String>,
    /// The names that some function reads and does not declare.
    read_in_functions: HashSet<String>,
    effects: HashMap<String, HashSet<String>>,
}

/// What a function may change that is not its own: the variables of the
/// file by their names, and fields as `.name`, whatever object they are
/// of. A loop needs this: what it goes over must stay as it is while
/// the loop runs, also when the block calls a function.
fn changed_by(def: &Def) -> (HashSet<String>, HashSet<String>) {
    let mut own: HashSet<String> = def.params.iter().map(|param| param.name.clone()).collect();
    bound_names(&def.body, &mut own);
    let mut globals = HashSet::new();
    global_names(&def.body, &mut globals);
    let mut changed: HashSet<String> = globals.clone();
    let mut calls = HashSet::new();
    let touch = |place: &Expr, changed: &mut HashSet<String>| match &place.kind {
        E::Name(name) if !own.contains(name) || globals.contains(name) => {
            changed.insert(name.clone());
        }
        E::Attr(_, field) => {
            changed.insert(format!(".{field}"));
        }
        _ => {}
    };
    fn targets(stmts: &[Stmt], visit: &mut dyn FnMut(&Expr)) {
        for stmt in stmts {
            match &stmt.kind {
                S::Assign { target, .. } | S::Augmented { target, .. } | S::Annotated { target, .. } | S::Delete(target) => visit(target),
                S::If { branches, otherwise } => {
                    branches.iter().for_each(|(_, body, _)| targets(body, visit));
                    otherwise.iter().for_each(|body| targets(body, visit));
                }
                S::While { body, .. } | S::For { body, .. } => targets(body, visit),
                S::Try { body, handlers } => {
                    targets(body, visit);
                    handlers.iter().for_each(|handler| targets(&handler.body, visit));
                }
                _ => {}
            }
        }
    }
    targets(&def.body, &mut |target| match &target.kind {
        E::Index(of, _) => touch(of, &mut changed),
        E::Attr(..) => touch(target, &mut changed),
        _ => {}
    });
    each_expr_of(&def.body, &mut |expr| {
        if let E::Call { function, .. } = &expr.kind {
            match &function.kind {
                E::Name(name) => {
                    calls.insert(name.clone());
                }
                E::Attr(of, method) => {
                    calls.insert(method.clone());
                    if CHANGES_A_LIST.contains(&method.as_str()) {
                        touch(of, &mut changed);
                    }
                }
                _ => {}
            }
        }
    });
    (changed, calls)
}

/// For each function and method, by its name, what it may change,
/// itself or through what it calls.
fn effects_of(defs: &[&Def]) -> HashMap<String, HashSet<String>> {
    let mut effects: HashMap<String, HashSet<String>> = HashMap::new();
    let mut calls: HashMap<String, HashSet<String>> = HashMap::new();
    for def in defs {
        let (changed, called) = changed_by(def);
        effects.entry(def.name.clone()).or_default().extend(changed);
        calls.entry(def.name.clone()).or_default().extend(called);
    }
    loop {
        let mut more: Vec<(String, String)> = Vec::new();
        for (name, called) in &calls {
            for other in called {
                for place in effects.get(other).into_iter().flatten() {
                    if !effects[name].contains(place) {
                        more.push((name.clone(), place.clone()));
                    }
                }
            }
        }
        if more.is_empty() {
            return effects;
        }
        for (name, place) in more {
            effects.entry(name).or_default().insert(place);
        }
    }
}

/// The names a function reads that are not its own.
fn free_names(def: &Def, into: &mut HashSet<String>) {
    let mut own: HashSet<String> = def.params.iter().map(|param| param.name.clone()).collect();
    bound_names(&def.body, &mut own);
    let mut globals = HashSet::new();
    global_names(&def.body, &mut globals);
    each_expr_of(&def.body, &mut |expr| {
        if let E::Name(name) = &expr.kind {
            if !own.contains(name) || globals.contains(name) {
                into.insert(name.clone());
            }
        }
    });
}

fn tables_of(units: &[Unit]) -> Result<Tables<'_>, Vec<(usize, Keep)>> {
    let mut tables = Tables {
        modules: HashMap::new(),
        from_names: HashMap::new(),
        kept: HashMap::new(),
        defs: HashMap::new(),
        shapes: HashMap::new(),
        module_vars: HashSet::new(),
        changed_in_functions: HashSet::new(),
        read_in_functions: HashSet::new(),
        effects: HashMap::new(),
    };
    let mut failed = Vec::new();
    for unit in units {
        if unit.kept.is_some() {
            for name in unit.defines.iter().filter(|name| !kept_own(unit, name)) {
                // What a kept function says it gives is believed, if it
                // is a kind of value that is the same on both sides.
                let ty = match unit.stmts.as_deref() {
                    Some([Stmt { kind: S::Def(def), .. }]) => match def.returns.as_ref().map(|returns| &returns.kind) {
                        Some(E::Name(said)) => match said.as_str() {
                            "int" => Ty::Int,
                            "float" => Ty::Float,
                            "bool" => Ty::Bool,
                            "str" => Ty::Str,
                            _ => Ty::Foreign,
                        },
                        _ => Ty::Foreign,
                    },
                    _ => Ty::Foreign,
                };
                tables.kept.insert(name.clone(), ty);
            }
            continue;
        }
        let Some(stmts) = &unit.stmts else { continue };
        if unit.kind == Kind::Import {
            for stmt in stmts {
                match &stmt.kind {
                    S::Import { module, name } => {
                        if let Some(known) = known_module(module, false) {
                            tables.modules.insert(name.clone(), known);
                        }
                    }
                    S::FromImport { module, names } => {
                        if let Some(known) = known_module(module, true) {
                            for (real, known_as) in names {
                                tables.from_names.insert(known_as.clone(), (known, real.clone()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for (at, unit) in units.iter().enumerate() {
        if unit.kept.is_some() {
            continue;
        }
        let Some(stmts) = &unit.stmts else { continue };
        match (unit.kind, stmts.as_slice()) {
            (Kind::Def, [Stmt { kind: S::Def(def), .. }]) => {
                tables.defs.insert(def.name.clone(), def);
                global_names(&def.body, &mut tables.changed_in_functions);
                free_names(def, &mut tables.read_in_functions);
            }
            (Kind::Class, [stmt]) => {
                let from_names = &tables.from_names;
                let is_dataclass = |decorator: &Expr| matches!(&decorator.kind, E::Name(name) if matches!(from_names.get(name), Some((Module::Quiet, real)) if real == "dataclass"));
                match shape_of(stmt, &is_dataclass) {
                    Ok(shape) => {
                        for def in shape.methods.values() {
                            global_names(&def.body, &mut tables.changed_in_functions);
                            free_names(def, &mut tables.read_in_functions);
                        }
                        if let S::Class { name, .. } = &stmt.kind {
                            tables.shapes.insert(name.clone(), shape);
                        }
                    }
                    Err(keep) => failed.push((at, keep)),
                }
            }
            (Kind::Action, _) => bound_names(unit.statements(), &mut tables.module_vars),
            _ => {}
        }
    }
    let mut all: Vec<&Def> = tables.defs.values().copied().collect();
    all.extend(tables.shapes.values().flat_map(|shape| shape.methods.values().copied()));
    tables.effects = effects_of(&all);
    // A name of the file that is a function or a class is not a variable.
    tables.module_vars.retain(|name| !tables.defs.contains_key(name) && !tables.shapes.contains_key(name));
    if failed.is_empty() {
        Ok(tables)
    } else {
        Err(failed)
    }
}

/// A function of Python that Zeno has no word for, and that a file which
/// uses it gets written out in Zeno, once, below the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Helper {
    /// `sorted(xs)`: a list of Zeno is sorted where it is.
    Sorted,
    /// `xs.remove(x)`.
    Remove,
    /// `text.split(by, most)`.
    SplitAtMost,
    /// `text * count`.
    Repeated,
}

impl Helper {
    pub(super) fn name(self) -> &'static str {
        match self {
            Helper::Sorted => "sorted",
            Helper::Remove => "remove_item",
            Helper::SplitAtMost => "split_at_most",
            Helper::Repeated => "repeated",
        }
    }

    fn lines(self) -> &'static [&'static str] {
        match self {
            Helper::Sorted => &[
                "# convert: Python's `sorted`, a copy of a list with the smallest first.",
                "function sorted[T is Ordered](items: list[T]) -> list[T] {",
                "    let copy = items.slice(0)",
                "    copy.sort()",
                "    return copy",
                "}",
            ],
            Helper::Remove => &[
                "# convert: Python's `remove`, which takes out the first item equal to the one given.",
                "function remove_item[T](items: list[T], item: T) raises ValueError {",
                "    let at = items.find(item)",
                "    if at is none {",
                "        raise ValueError(\"list.remove(x): x not in list\")",
                "    }",
                "    items.pop(at)",
                "}",
            ],
            Helper::SplitAtMost => &[
                "# convert: Python's `split` with a number: the text is cut at the first so many places only.",
                "function split_at_most(text: str, by: str, most: int) -> list[str] {",
                "    let parts = text.split(by)",
                "    if most < 0 or len(parts) <= most + 1 {",
                "        return parts",
                "    }",
                "    let out = parts.slice(0, most)",
                "    out.append(parts.slice(most).join(by))",
                "    return out",
                "}",
            ],
            Helper::Repeated => &[
                "# convert: Python's `text * count`, the text that many times over.",
                "function repeated(text: str, count: int) -> str {",
                "    var out = \"\"",
                "    for time in range(count) {",
                "        out += text",
                "    }",
                "    return out",
                "}",
            ],
        }
    }
}

/// One round over every piece that is to be Zeno.
struct Round {
    found: Known,
    /// For each unit, its lines of Zeno, or why it cannot be written.
    written: Vec<Option<Kept<Vec<(usize, String)>>>>,
    notes: Vec<(Note, usize)>,
    need_math: bool,
    need_program: bool,
    helpers: BTreeSet<Helper>,
}

fn round(units: &[Unit], tables: &Tables, comments: &[Comment], block: &str, file_names: &HashSet<String>, known: &Known, last: bool) -> Round {
    let mut blocks: Vec<&[Stmt]> = Vec::new();
    let mut kills: Vec<HashMap<String, usize>> = Vec::new();
    let mut killed: HashMap<String, usize> = HashMap::new();
    for unit in units {
        if unit.kept.is_some() && unit.kind == Kind::Action {
            for name in unit.defines.iter().filter(|name| unit.own.contains(*name) || tables.module_vars.contains(*name)) {
                killed.insert(name.clone(), unit.piece.line);
            }
        } else if unit.kept.is_none() && unit.kind == Kind::Action {
            blocks.push(unit.statements());
            kills.push(std::mem::take(&mut killed));
        }
    }
    let (module_plan, plan_failed) = match plan(&blocks, &kills, &[], &tables.changed_in_functions, &tables.read_in_functions, known, "") {
        Ok(plan) => (plan, None),
        Err(keep) => (Plan::default(), Some(keep)),
    };
    let env = Env {
        known,
        last,
        modules: &tables.modules,
        from_names: &tables.from_names,
        kept: &tables.kept,
        block,
        defs: &tables.defs,
        shapes: &tables.shapes,
        module_vars: &tables.module_vars,
        module_plan: &module_plan,
        comments,
        effects: &tables.effects,
        file_names,
    };
    let mut writer = Writer::new(&env);
    let mut written = Vec::new();
    for unit in units {
        let (true, Some(stmts)) = (unit.kept.is_none(), &unit.stmts) else {
            written.push(None);
            continue;
        };
        // The plan of the file could not be made: the piece whose line
        // it stopped at is the one that cannot be written.
        if let Some(keep) = &plan_failed {
            if last && (unit.piece.line..=unit.piece.last_line).contains(&keep.line) {
                written.push(Some(Err(keep.clone())));
                continue;
            }
        }
        writer.comment_at = comments.iter().position(|comment| comment.line >= unit.piece.first_line).unwrap_or(comments.len());
        writer.lines.clear();
        writer.indent = 0;
        let done = match (unit.kind, stmts.as_slice()) {
            (Kind::Def, [Stmt { kind: S::Def(def), line, .. }]) => writer.function(def, None, *line),
            (Kind::Class, [Stmt { kind: S::Class { name, .. }, line, .. }]) => match tables.shapes.get(name) {
                Some(shape) => writer.class(name, shape, *line),
                None => keep(*line, format!("the class `{name}`")),
            },
            (Kind::Import, _) => Ok(()),
            _ => writer.block(unit.statements()),
        };
        if done.is_ok() {
            writer.indent = 0;
            writer.comments_before(unit.piece.last_line + 1);
        }
        written.push(Some(done.map(|()| std::mem::take(&mut writer.lines))));
    }
    Round { found: writer.found, written, notes: writer.notes, need_math: writer.need_math, need_program: writer.need_program, helpers: writer.helpers }
}

// ------------------------------------------------------------ The result

/// `if __name__ == "__main__":` at the left margin, as it must be
/// written inside a block: there the file is not `__main__`.
fn without_main_guard(line: &str) -> Option<String> {
    let close: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if !(close.starts_with("if__name__==\"__main__\":") || close.starts_with("if__name__=='__main__':")) {
        return None;
    }
    let after_name = line.find("__main__")? + "__main__".len();
    let colon = after_name + line[after_name..].find(':')?;
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = line[colon + 1..].trim();
    Some(if rest.is_empty() { format!("{indent}if True:  # convert: this was `if __name__ == \"__main__\":`") } else { format!("{indent}if True: {rest}") })
}

/// The whole file kept as Python, in one block.
fn whole(source: &str, why: String) -> Converted {
    let mut zeno = String::from("import python\n\nlet py = inline python {\n");
    for line in source.lines() {
        let line = without_main_guard(line).unwrap_or_else(|| line.to_string());
        if line.trim().is_empty() {
            zeno.push('\n');
        } else {
            zeno.push_str(&format!("    {line}\n"));
        }
    }
    zeno.push_str("}\n");
    Converted { zeno, report: Report { whole: Some(why), ..Report::default() } }
}

/// How a kept piece is written in the block.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// As it is, at the top of the block: it is made once, before the
    /// first line of Zeno runs.
    Top,
    /// As the body of a function of the block with this number, which
    /// the Zeno calls where the piece was.
    Step(usize),
}

struct Text {
    lines: Vec<String>,
    /// The unit each line was written for.
    owners: Vec<Option<usize>>,
}

impl Text {
    fn push(&mut self, line: String, owner: Option<usize>) {
        self.lines.push(line);
        self.owners.push(owner);
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.push(String::new(), None);
        }
    }
}

/// The file of Zeno: what it imports, the block of kept Python, then
/// every piece where it was.
fn assemble(units: &[Unit], lexed: &Lexed, source: &[&str], tables: &Tables, last: &Round, block: &str, file: &str) -> Text {
    let mut text = Text { lines: Vec::new(), owners: Vec::new() };
    let any_kept = units.iter().any(|unit| unit.kept.is_some());
    text.push(format!("# Written from {file} by `zeno convert`."), None);
    text.blank();
    if any_kept {
        text.push("import python".to_string(), None);
    }
    if last.need_math {
        text.push("import math".to_string(), None);
    }
    if last.need_program {
        text.push("import program".to_string(), None);
    }

    // Where each kept piece goes in the block.
    let late: HashSet<&String> = units.iter().filter(|unit| unit.kept.is_some() && unit.kind == Kind::Action).flat_map(|unit| &unit.defines).collect();
    let mut places: Vec<Option<Place>> = Vec::new();
    let mut steps = 0;
    for (at, unit) in units.iter().enumerate() {
        places.push(match (&unit.kept, unit.kind) {
            (None, _) => None,
            (Some(_), Kind::Import) => Some(Place::Top),
            (Some(_), Kind::Def | Kind::Class) => {
                let again = unit.defines.first().is_some_and(|name| units.iter().enumerate().any(|(other, unit)| other != at && unit.kept.is_some() && unit.defines.contains(name)));
                if again || unit.made_with.iter().any(|name| late.contains(name)) {
                    steps += 1;
                    Some(Place::Step(steps))
                } else {
                    Some(Place::Top)
                }
            }
            (Some(_), Kind::Action) => {
                steps += 1;
                Some(Place::Step(steps))
            }
        });
    }

    if any_kept {
        text.blank();
        text.push(format!("let {block} = inline python {{"), None);
        let kept_uses: HashSet<&String> = units.iter().filter(|unit| unit.kept.is_some()).flat_map(|unit| &unit.uses).collect();
        let mut first = true;
        for (at, unit) in units.iter().enumerate() {
            let on_both_sides = unit.kept.is_none() && (unit.kind == Kind::Import || unit.constant) && unit.defines.iter().any(|name| kept_uses.contains(name));
            let from = if on_both_sides || unit.kind == Kind::Import { unit.piece.line } else { unit.piece.first_line };
            let lines: Vec<&str> = (from..=unit.piece.last_line).map(|line| source.get(line - 1).copied().unwrap_or("")).collect();
            // A line that is inside a text of several lines belongs to
            // the text, and is not moved further in.
            let toks = &lexed.toks[unit.piece.toks.clone()];
            let in_a_text = |line: usize| toks.iter().any(|tok| matches!(tok.kind, T::Str(_)) && tok.line < line && line <= tok.end_line);
            match (places[at], on_both_sides) {
                (Some(Place::Top), _) | (None, true) => {
                    if !first && unit.kind != Kind::Import && !unit.constant {
                        text.push(String::new(), None);
                    }
                    for line in lines {
                        text.push(if line.trim().is_empty() { String::new() } else { format!("    {line}") }, None);
                    }
                }
                (Some(Place::Step(number)), _) => {
                    if !first {
                        text.push(String::new(), None);
                    }
                    text.push(format!("    def convert_step_{number}():"), None);
                    if !unit.defines.is_empty() {
                        text.push(format!("        global {}", unit.defines.join(", ")), None);
                    }
                    for (offset, line) in lines.iter().enumerate() {
                        let number = from + offset;
                        let line = if number == unit.piece.line { without_main_guard(line).unwrap_or_else(|| line.to_string()) } else { line.to_string() };
                        text.push(
                            if line.trim().is_empty() {
                                String::new()
                            } else if in_a_text(number) {
                                format!("    {line}")
                            } else {
                                format!("        {line}")
                            },
                            None,
                        );
                    }
                }
                (None, false) => continue,
            }
            first = false;
        }
        text.push("}".to_string(), None);
    }
    text.blank();
    for helper in &last.helpers {
        for line in helper.lines() {
            text.push(line.to_string(), None);
        }
        text.blank();
    }

    // A function that reads a variable of the file goes below the line
    // that declares the variable: in Zeno a line can use only what is
    // declared above it, inside a function too.
    let declared_by = |name: &str| {
        units.iter().position(|unit| {
            let mut bound = HashSet::new();
            if unit.kept.is_none() && unit.kind == Kind::Action {
                bound_names(unit.statements(), &mut bound);
            }
            bound.contains(name)
        })
    };
    let mut below: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut sunk: HashSet<usize> = HashSet::new();
    for (at, unit) in units.iter().enumerate() {
        if unit.kept.is_some() || !matches!(unit.kind, Kind::Def | Kind::Class) {
            continue;
        }
        let mut reads = HashSet::new();
        match unit.stmts.as_deref() {
            Some([Stmt { kind: S::Def(def), .. }]) => {
                free_names(def, &mut reads);
                global_names(&def.body, &mut reads);
            }
            Some([Stmt { kind: S::Class { name, .. }, .. }]) => {
                for def in tables.shapes.get(name).into_iter().flat_map(|shape| shape.methods.values()) {
                    free_names(def, &mut reads);
                    global_names(&def.body, &mut reads);
                }
            }
            _ => {}
        }
        let after = reads.iter().filter(|name| tables.module_vars.contains(*name)).filter_map(|name| declared_by(name)).max();
        if let Some(after) = after.filter(|after| *after > at) {
            below.entry(after).or_default().push(at);
            sunk.insert(at);
        }
    }

    let mut comment_at = 0;
    let mut last_line = 0;
    let comments = &lexed.comments;
    let write_unit = |text: &mut Text, at: usize| {
        if let Some(Some(Ok(lines))) = last.written.get(at) {
            for (indent, line) in lines {
                text.push(format!("{}{line}", "    ".repeat(*indent)), Some(at));
            }
        }
    };
    for (at, unit) in units.iter().enumerate() {
        // The comments between the pieces, where they were.
        // (Those above a kept `import` too: they are often about the file.)
        let own_from = if unit.kept.is_some() && unit.kind == Kind::Import { unit.piece.line } else { unit.piece.first_line };
        while let Some(comment) = comments.get(comment_at) {
            if comment.line >= own_from {
                break;
            }
            comment_at += 1;
            if comment.line > last_line + 1 {
                text.blank();
            }
            text.push(format!("#{}", comment.text), None);
            last_line = comment.line;
        }
        while comments.get(comment_at).is_some_and(|comment| comment.line <= unit.piece.last_line) {
            comment_at += 1;
        }
        if unit.piece.first_line > last_line + 1 {
            text.blank();
        }
        last_line = unit.piece.last_line;
        match places[at] {
            Some(Place::Step(number)) => text.push(format!("{block}.convert_step_{number}()"), Some(at)),
            Some(Place::Top) if unit.kind != Kind::Import => {
                text.push(format!("# {} is kept as Python, in the block above.", unit.label), None);
            }
            Some(Place::Top) => {}
            None if sunk.contains(&at) => {}
            None => write_unit(&mut text, at),
        }
        for moved in below.get(&at).into_iter().flatten() {
            text.blank();
            write_unit(&mut text, *moved);
            text.blank();
        }
    }
    for comment in &comments[comment_at.min(comments.len())..] {
        if comment.line > last_line + 1 {
            text.blank();
        }
        text.push(format!("#{}", comment.text), None);
        last_line = comment.line;
    }
    while text.lines.last().is_some_and(|line| line.is_empty()) {
        text.lines.pop();
        text.owners.pop();
    }
    text
}

/// What Zeno's own checker says of what was written: the line and the
/// message of each mistake.
fn mistakes(text: &str) -> Vec<(usize, String)> {
    let loaded = crate::modules::load("converted.zeno", text.to_string(), &|_| None);
    let program = match loaded.program {
        Ok(program) => program,
        Err(errors) => return errors.iter().map(|error| (error.span.line, error.message.clone())).collect(),
    };
    let (_, errors, _) = crate::checker::checked(program, crate::checker::Rules::default());
    errors.iter().map(|error| (error.span.line, error.message.clone())).collect()
}

/// A name for the block that the file does not use.
fn block_name(lexed: &Lexed) -> String {
    let used = |name: &str| lexed.toks.iter().any(|tok| matches!(&tok.kind, T::Name(found) if found == name));
    ["py", "kept", "python_part", "kept_python_part"].into_iter().find(|name| !used(name)).unwrap_or("kept_as_python").to_string()
}

/// Write a file of Python as Zeno.
pub fn convert(source: &str, file: &str) -> Converted {
    let mut lexed = match lex(source) {
        Ok(lexed) => lexed,
        Err(keep) => return whole(source, format!("line {}: {}", keep.line, keep.why)),
    };
    // `#!/usr/bin/env python3` says how to run the file of Python.
    lexed.comments.retain(|comment| !(comment.line == 1 && comment.text.starts_with('!')));
    let source_lines: Vec<&str> = source.lines().collect();
    let mut units = units_of(&lexed, &source_lines);
    let block = block_name(&lexed);
    let file_names: HashSet<String> = lexed.toks.iter().filter_map(|tok| if let T::Name(name) = &tok.kind { Some(name.clone()) } else { None }).collect();
    // Each time round, at least one more piece is kept, so this ends.
    loop {
        spread(&mut units);
        let tables = match tables_of(&units) {
            Ok(tables) => tables,
            Err(failed) => {
                for (at, keep) in failed {
                    units[at].kept = Some((keep.line, keep.why));
                }
                continue;
            }
        };
        let mut known = Known::default();
        for _ in 0..12 {
            let found = round(&units, &tables, &lexed.comments, &block, &file_names, &known, false).found;
            let merged = known.merged(&found);
            if merged == known {
                break;
            }
            known = merged;
        }
        let last = round(&units, &tables, &lexed.comments, &block, &file_names, &known, true);
        let failed: Vec<(usize, Keep)> = last.written.iter().enumerate().filter_map(|(at, written)| match written {
            Some(Err(keep)) => Some((at, keep.clone())),
            _ => None,
        }).collect();
        if !failed.is_empty() {
            drop(tables);
            // What one piece cannot be written for is often what another
            // piece became: a type that is not known, a name that holds
            // a function because the line that gives it one cannot be
            // written. So one piece is kept at a time, the first that has
            // a reason of its own, and the rest are asked again.
            let unknown = |keep: &Keep| keep.why.contains("is not known");
            let first = failed.iter().find(|(_, keep)| !unknown(keep)).or(failed.first()).cloned();
            let now: Vec<(usize, Keep)> = first.into_iter().collect();
            for (at, keep) in now {
                units[at].kept = Some((keep.line, keep.why));
            }
            continue;
        }
        let text = assemble(&units, &lexed, &source_lines, &tables, &last, &block, file);
        let joined = format!("{}\n", text.lines.join("\n"));
        let found = mistakes(&joined);
        if !found.is_empty() {
            let mut failed: Vec<(usize, String)> = Vec::new();
            for (line, message) in &found {
                match text.owners.get(line.saturating_sub(1)).copied().flatten() {
                    Some(at) if units[at].kept.is_none() => {
                        if !failed.iter().any(|(other, _)| *other == at) {
                            failed.push((at, format!("Zeno's checker does not take what the converter wrote for it: {message}")));
                        }
                    }
                    _ => {}
                }
            }
            if failed.is_empty() {
                let (line, message) = &found[0];
                return whole(source, format!("what the converter wrote was not sound Zeno (line {line} of it: {message})"));
            }
            drop(tables);
            for (at, why) in failed {
                let line = units[at].piece.line;
                units[at].kept = Some((line, why));
            }
            continue;
        }
        let zeno = crate::format::format(&joined).unwrap_or(joined);
        let mut noted = last.notes.clone();
        noted.sort_by_key(|(note, line)| (*line, *note));
        noted.dedup();
        let kept = units
            .iter()
            .filter_map(|unit| unit.kept.as_ref().map(|(at, why)| KeptPiece { line: unit.piece.line, what: unit.label.clone(), at: *at, why: why.clone() }))
            .collect();
        let report = Report { pieces: units.len(), noted, kept, whole: None, warnings: Vec::new() };
        return Converted { zeno, report };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converted(source: &str) -> Converted {
        let done = convert(source, "test.py");
        assert!(mistakes(&done.zeno).is_empty(), "the converter wrote what Zeno's checker refuses:\n{}", done.zeno);
        done
    }

    fn kept_lines(done: &Converted) -> Vec<usize> {
        done.report.kept.iter().map(|piece| piece.line).collect()
    }

    #[test]
    fn a_program_zeno_has_words_for_is_zeno_through_and_through() {
        let done = converted("def twice(n):\n    return n * 2\n\n\ntotal = 0\nfor n in range(4):\n    total += twice(n)\nprint(total)\n");
        assert!(done.report.kept.is_empty());
        assert!(!done.zeno.contains("inline python"));
        assert!(done.zeno.contains("function twice(n: int) -> int {"));
        assert!(done.zeno.contains("var total = 0"));
    }

    #[test]
    fn what_cannot_be_read_is_kept_and_called() {
        let done = converted("square = lambda x: x * x\nprint(square(3))\n");
        assert_eq!(kept_lines(&done), vec![1]);
        assert!(done.report.kept[0].why.contains("lambda"));
        assert!(done.zeno.contains("let py = inline python {"));
        assert!(done.zeno.contains("py.square(3)"));
    }

    #[test]
    fn a_kept_piece_does_not_pull_the_pieces_above_it_into_python() {
        // The loop at line 3 is kept; the loop above it, with a variable of
        // the same name, stays Zeno, and so does the one below.
        let done = converted("for item in [1, 2]:\n    print(item)\nfor item in (lambda: [3])():\n    print(item)\nfor item in [4]:\n    print(item)\n");
        assert_eq!(kept_lines(&done), vec![3]);
    }

    #[test]
    fn a_line_of_zeno_never_reads_what_a_kept_piece_left_in_a_name() {
        // A name only the kept piece gives a value is read from Python.
        let done = converted("n = (lambda: 5)()\nm = 1\nprint(n, m)\n");
        assert_eq!(kept_lines(&done), vec![1]);
        assert!(done.zeno.contains("print(py.n, m)"));
        // Zeno gives `n` a value too, but not for certain before the line
        // that reads it: that line is kept, and so is the piece above it.
        let done = converted("n = (lambda: 5)()\nif len(\"ab\") > 1:\n    n = 2\nprint(n)\n");
        assert_eq!(kept_lines(&done), vec![1, 2, 4]);
        assert!(done.report.kept.iter().any(|piece| piece.why.contains("may still hold here the value that the piece at line 1 gave it")));
        // Zeno gives it a value for certain: the rest is Zeno.
        let done = converted("n = (lambda: 5)()\nn = 2\nprint(n)\n");
        assert_eq!(kept_lines(&done), vec![1]);
    }

    #[test]
    fn a_loop_variable_read_after_the_loop_is_a_variable() {
        let done = converted("for word in [\"a\", \"b\"]:\n    pass\nprint(word)\n");
        assert!(done.report.kept.is_empty());
        assert!(done.zeno.contains("for word_item in [\"a\", \"b\"] {"));
        assert!(done.zeno.contains("word = word_item"));
    }

    #[test]
    fn rows_are_gone_over_by_place_and_mixed_numbers_are_kept() {
        let done = converted("for name, age in [(\"ann\", 30), (\"bob\", 41)]:\n    print(name, age)\n");
        assert!(done.report.kept.is_empty());
        assert!(done.zeno.contains("let age = [30, 41][row_1]"));
        let done = converted("for name, price in [(\"tea\", 2), (\"jam\", 1.5)]:\n    print(name, price)\n");
        assert_eq!(kept_lines(&done), vec![1]);
    }

    #[test]
    fn what_python_has_and_zeno_has_not_is_written_once_and_only_if_used() {
        let done = converted("print(sorted([3, 1]), \"-\" * 3)\n");
        assert!(done.zeno.contains("function sorted[T is Ordered]"));
        assert!(done.zeno.contains("function repeated("));
        assert!(!done.zeno.contains("function remove_item"));
        // A name the file has already is not taken for one.
        let done = converted("repeated = 2\nprint(\"-\" * repeated)\n");
        assert_eq!(kept_lines(&done), vec![2]);
    }

    #[test]
    fn a_part_of_a_list_is_cut_as_python_cuts_it() {
        let done = converted("xs = [1, 2, 3]\nprint(xs[1:10], xs[-2:])\n");
        assert!(done.zeno.contains("xs.slice(min(1, len(xs)), min(10, len(xs)))"));
        assert!(done.zeno.contains("xs.slice(max(len(xs) - 2, 0))"));
    }

    #[test]
    fn a_loop_whose_list_a_called_function_changes_is_kept() {
        let done = converted("queue = [1]\n\n\ndef grow(n):\n    if n < 3:\n        queue.append(n + 1)\n\n\nfor n in queue:\n    grow(n)\nprint(queue)\n");
        assert!(kept_lines(&done).contains(&9));
        assert!(done.report.kept.iter().any(|piece| piece.why.contains("`grow`, which the loop calls")));
    }
}
