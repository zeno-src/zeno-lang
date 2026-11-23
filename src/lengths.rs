//! Does a function change how many items a list it is given has?
//!
//! A list of a fixed length (`list[u8, 64]`, decision 36.4) may be given
//! to a function whose parameter is an ordinary `list[u8]` only if the
//! function never changes how long that list is: it reads it, gives its
//! items new values, walks it, or passes it on to a function that does
//! the same. This module reads the body of the function, as written, and
//! answers with the first thing in it that could change the length.
//!
//! It is careful in one direction only. Whatever it cannot be sure of
//! (the list kept in a variable, put in another list, returned, handed
//! to a function it cannot see) counts as a change, and the message
//! says which line it was.

use crate::ast::{Arg, Expr, ExprKind, FormatPart, Place, Stmt, StmtKind};
use crate::token::Span;

/// The methods that change how many items a list has.
pub const GROWS: &[&str] = &["append", "pop", "insert", "clear"];

/// What a call that is given the list does with it, as the checker can
/// tell from the function called.
pub enum Callee {
    /// A built-in function, which only looks.
    Looks,
    /// A function of the program, read in its turn: `None` if it keeps
    /// the length, or the reason it may not.
    Function(Option<String>),
    /// Something that cannot be read: a function held in a variable, a
    /// method of an object, a class being made.
    Unknown,
}

/// The first thing in `body` that could change how many items the list
/// named `param` has, as a reason a person can read, with where it is.
/// `callee` is asked about each call that is handed the list itself:
/// the call, and which of its arguments the list is.
pub fn changes_length(body: &[Stmt], param: &str, callee: &mut dyn FnMut(&Expr, &[Arg], usize) -> Callee) -> Option<String> {
    let mut reader = Reader { param, callee, found: None };
    reader.block(body);
    reader.found
}

struct Reader<'a> {
    param: &'a str,
    callee: &'a mut dyn FnMut(&Expr, &[Arg], usize) -> Callee,
    found: Option<String>,
}

impl Reader<'_> {
    fn is_param(&self, expr: &Expr) -> bool {
        matches!(&expr.kind, ExprKind::Name(name) if name == self.param)
    }

    fn report(&mut self, what: String, span: Span) {
        if self.found.is_none() {
            self.found = Some(format!("{what} (line {})", span.line));
        }
    }

    /// The list goes somewhere it could be kept and grown later.
    fn kept(&mut self, expr: &Expr, how: &str) {
        if self.is_param(expr) {
            self.report(format!("it {how} `{}`", self.param), expr.span);
        } else {
            self.expr(expr);
        }
    }

    fn block(&mut self, body: &[Stmt]) {
        for stmt in body {
            if self.found.is_some() {
                return;
            }
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { value, .. } | StmtKind::Const { value, .. } => self.kept(value, "keeps in a variable"),
            StmtKind::Assign { target, op, value } => {
                match &target.kind {
                    // `data[i] = 7` gives an item a new value.
                    ExprKind::Index { object, index } if self.is_param(object) => self.expr(index),
                    _ if self.is_param(target) => {
                        let how = if op.is_some() { "changes" } else { "gives a new value to" };
                        self.report(format!("it {how} `{}`", self.param), target.span);
                    }
                    _ => self.expr(target),
                }
                self.kept(value, "keeps");
            }
            // `a, data = data, a`: each value is kept by its place.
            StmtKind::Receive { places, value, .. } => {
                for place in places {
                    if let Place::Target(target) = place {
                        match &target.kind {
                            ExprKind::Index { object, index } if self.is_param(object) => self.expr(index),
                            _ if self.is_param(target) => {
                                self.report(format!("it gives a new value to `{}`", self.param), target.span)
                            }
                            _ => self.expr(target),
                        }
                    }
                }
                match &value.kind {
                    ExprKind::Several(values) => values.iter().for_each(|value| self.kept(value, "keeps")),
                    _ => self.kept(value, "keeps"),
                }
            }
            StmtKind::Expr(expr) => {
                if !self.is_param(expr) {
                    self.expr(expr);
                }
            }
            StmtKind::Return(Some(value)) => self.kept(value, "returns"),
            StmtKind::Raise(value) => self.expr(value),
            StmtKind::Function(function) => self.block(&function.body),
            StmtKind::If { condition, then_block, else_block } => {
                self.look(condition);
                self.block(then_block);
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            StmtKind::While { condition, body } => {
                self.look(condition);
                self.block(body);
            }
            StmtKind::For { iterable, body, .. } => {
                self.look(iterable);
                self.block(body);
            }
            // A pattern can give the whole list a name of its own.
            StmtKind::Match { subject, arms } => {
                self.kept(subject, "matches");
                for arm in arms {
                    self.block(&arm.body);
                }
            }
            StmtKind::Try { body, handlers } => {
                self.block(body);
                for handler in handlers {
                    self.block(&handler.body);
                }
            }
            StmtKind::Unsafe(body)
            | StmtKind::Test { body, .. }
            | StmtKind::AssertRaises { body, .. }
            | StmtKind::Scoped { body, .. } => self.block(body),
            StmtKind::Assert(condition, message) => {
                self.look(condition);
                if let Some(message) = message {
                    self.look(message);
                }
            }
            StmtKind::Return(None)
            | StmtKind::Class(_)
            | StmtKind::Enum(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import { .. } => {}
        }
    }

    /// An expression that reads the list, if it is the list.
    fn look(&mut self, expr: &Expr) {
        if !self.is_param(expr) {
            self.expr(expr);
        }
    }

    fn expr(&mut self, expr: &Expr) {
        if self.found.is_some() {
            return;
        }
        match &expr.kind {
            // Named anywhere not listed below, it could be kept.
            ExprKind::Name(name) | ExprKind::Give(name) if name == self.param => self.report(format!("it keeps `{name}`"), expr.span),
            ExprKind::Name(_) | ExprKind::Give(_) => {}
            ExprKind::Index { object, index } => {
                self.look(object);
                self.expr(index);
            }
            ExprKind::Field { object, name, .. } if self.is_param(object) => {
                if GROWS.contains(&name.as_str()) {
                    self.report(format!("it calls `{}.{name}`", self.param), expr.span);
                }
            }
            ExprKind::Field { object, .. } => self.expr(object),
            // It leaves `none` in an item: the list is no longer.
            ExprKind::Take(place) => self.expr(place),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for (index, arg) in args.iter().enumerate() {
                    if !self.is_param(&arg.value) {
                        self.expr(&arg.value);
                        continue;
                    }
                    let shown = callee_text(callee);
                    match (self.callee)(callee, args, index) {
                        Callee::Looks | Callee::Function(None) => {}
                        Callee::Function(Some(why)) => {
                            self.report(format!("it gives `{}` to `{shown}`, and {why}", self.param), arg.value.span)
                        }
                        Callee::Unknown => self.report(
                            format!("it gives `{}` to `{shown}`, which may keep it", self.param),
                            arg.value.span,
                        ),
                    }
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.look(left);
                self.look(right);
            }
            ExprKind::Compare { first, rest } => {
                self.look(first);
                for (_, operand) in rest {
                    self.look(operand);
                }
            }
            ExprKind::Unary { operand, .. } => self.look(operand),
            ExprKind::ListComp { element, iterable, condition, .. } => {
                self.look(iterable);
                self.look(element);
                if let Some(condition) = condition {
                    self.look(condition);
                }
            }
            ExprKind::FormatStr(parts) => {
                for part in parts {
                    if let FormatPart::Value { value, .. } = part {
                        self.look(value);
                    }
                }
            }
            ExprKind::Unit { value, .. } => self.look(value),
            ExprKind::List(items) => {
                for item in items {
                    self.kept(item, "puts in a list");
                }
            }
            ExprKind::Several(items) => {
                for item in items {
                    self.kept(item, "returns");
                }
            }
            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key);
                    self.kept(value, "puts in a dict");
                }
            }
            // A function written inside sees the list too.
            ExprKind::Function(function) => self.block(&function.body),
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
        }
    }
}

/// How a callee is shown in a message: `pad`, or `frame.fill`.
fn callee_text(callee: &Expr) -> String {
    match &callee.kind {
        ExprKind::Name(name) => name.clone(),
        ExprKind::Field { object, name, .. } => format!("{}.{name}", callee_text(object)),
        _ => "a function".to_string(),
    }
}
