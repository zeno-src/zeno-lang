//! The type checker: finds mistakes before the program runs.
//!
//! It walks the syntax tree the same way the interpreter does, but instead of
//! working out *values* it works out *types*. Where the interpreter's `eval`
//! turns `1 + 2` into `3`, the checker's `expr` turns it into `int`.
//!
//! Zeno's types are optional, so the checker is "gradual":
//!
//! 1. A type that is written down is checked.
//! 2. A type that is left out is worked out from the value, when possible.
//! 3. Otherwise the value is `Dynamic`: the checker lets it through, and any
//!    mistake is found when the program runs.
//!
//! In strict mode step 3 is an error. Everything must be known in advance.
//!
//! A parameter or field with no type written is worked out from how it is
//! used across the whole program. That needs several passes, because one
//! answer can unlock another; see `check` and `Seen`.
//!
//! Unlike the parser, the checker does not stop at the first error. After
//! reporting one it treats the expression as `Invalid`, which is accepted
//! everywhere, so one mistake does not set off a string of others.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::ast::{self, *};
use crate::error::{counted, were, Error};
use crate::ints::{self, Width};
use crate::lengths;
use crate::token::Span;
use crate::units;

/// How many passes to allow for working out untyped parameters and fields.
const MAX_ROUNDS: usize = 8;

/// Check a whole program. Returns every error found, in source order.
///
/// Strict mode is on if it is asked for here or by the file itself.
#[cfg(test)]
pub fn check(program: &Program, strict: bool) -> Vec<Error> {
    check_with_types(program, strict).0
}

/// How a program is checked.
#[derive(Clone, Copy, Default)]
pub struct Rules {
    /// Every type must be known before the program runs.
    pub strict: bool,
    /// Every list and every object has exactly one owner, whether or not
    /// the program says `owned`. A build with no collector checks this
    /// way: with nothing to collect a shared value, nothing can be shared.
    pub one_owner: bool,
    /// Errors travel as values, as in `zeno build --firmware`. There a
    /// division by zero, a bad index or a number that does not fit stops
    /// the program where it happens, and cannot be raised to a caller.
    pub firmware: bool,
}

/// A type, as the stages after the checker see it. The checker's own `Ty`
/// is full of links back into the checker; this is the plain answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Int,
    Float,
    /// A float of 32 bits (decision 41). C keeps it in a `float`.
    F32,
    /// A whole number of 64 bits with no sign (decision 42). C keeps it
    /// in a `uint64_t`.
    U64,
    Str,
    Bool,
    None,
    /// A number with a unit; the string is the unit, such as "ms".
    Quantity(String),
    /// A `u8` as an item of a list, which a compiled program stores in
    /// one byte (decision 36.2). Only ever inside `List`: the item, once
    /// read, is an `Int`.
    Byte,
    List(Box<Kind>),
    /// `dict[str, int]`: the kind of its keys, and of its values.
    Dict(Box<Kind>, Box<Kind>),
    /// An object of the class with this name.
    Class(String),
    /// A value of the enum with this name.
    Enum(String),
    /// `T or none`: a value of the kind inside, or nothing.
    Optional(Box<Kind>),
    /// A pin of the board: `led`, `pin(7)`. It is the pin's number.
    Pin,
    /// A pin the program reads: `input_pin(2)`. It is the pin's number
    /// too (decision 45).
    InputPin,
    /// A serial port of the board: `serial(1, 115200 baud)`. It is the
    /// port's number (decision 46).
    Serial,
    /// A pin whose voltage the program reads: `analog_pin(0)`. It is the
    /// pin's number (decision 47).
    AnalogPin,
    /// A pin that is switched on and off many times a second:
    /// `pwm_pin(9, 1 kHz)`. It is the pin's number (decision 48).
    PwmPin,
    /// A beat the program keeps time by: `ticker(10 ms)`. It is which
    /// ticker of the program this is, counted from 0 (decision 49).
    Ticker,
    /// The edges of an input pin, counted by the board:
    /// `button.interrupt(falling)`. It is the pin's number (decision 50).
    Interrupt,
    /// A raw pointer: `pointer[u8]`. See docs/unsafe.md.
    Pointer(Pointee),
    /// A register of the board at a fixed address. It is the address.
    Register(Width),
    /// An error value, such as what `ValueError("bad")` makes.
    Error,
    /// A function held as a value.
    Function(Box<FunctionKind>),
    /// A list or an object with one owner, which frees it. Seen only on a
    /// parameter, a field or a return type; a value that is merely looked at
    /// has the plain kind inside.
    Owned(Box<Kind>),
    /// A library of C that the program declared, by its number in
    /// `Types::c_libraries` (decision 44.10). It is known while
    /// building, and holds nothing while the program runs.
    CLibrary(usize),
    /// A pointer that C gave, to what it names (`counter`, `void`).
    CPointer(String),
    /// Anything else, described in words for an error message.
    Other(String),
    /// What a function gives that gives two values or more: `-> (int,
    /// str)` (decision 57). Only ever what a function returns.
    Several(Vec<Kind>),
}

/// What a `pointer[T]` points at: numbers of one size, side by side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pointee {
    Int,
    Float,
    Sized(Width),
}

impl Pointee {
    /// The pointee with this name, if it is one: `u8`, `int`, `float`.
    pub fn named(name: &str) -> Option<Pointee> {
        match name {
            "int" => Some(Pointee::Int),
            "float" => Some(Pointee::Float),
            _ => ints::width(name).map(Pointee::Sized),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Pointee::Int => "int",
            Pointee::Float => "float",
            Pointee::Sized(width) => width.name,
        }
    }

    /// How many bytes one item takes.
    pub fn bytes(self) -> u32 {
        match self {
            Pointee::Int | Pointee::Float => 8,
            Pointee::Sized(width) => width.bits / 8,
        }
    }

    /// The C type of one item.
    pub fn c_type(self) -> String {
        match self {
            Pointee::Int => "int64_t".to_string(),
            Pointee::Float => "double".to_string(),
            Pointee::Sized(width) => width.c_type(),
        }
    }

    /// The LLVM IR type of one item.
    pub fn ir_type(self) -> &'static str {
        match self {
            Pointee::Int => "i64",
            Pointee::Float => "double",
            Pointee::Sized(width) => width.ir_type(),
        }
    }

    /// The kind of a value read through the pointer. A sized number
    /// travels as an ordinary int, as everywhere.
    pub fn kind(self) -> Kind {
        match self {
            Pointee::Float => Kind::Float,
            Pointee::Int | Pointee::Sized(_) => Kind::Int,
        }
    }

    fn ty(self) -> Ty {
        match self {
            Pointee::Int => Ty::Int,
            Pointee::Float => Ty::Float,
            Pointee::Sized(width) => Ty::Sized(width),
        }
    }
}

/// What a function takes and gives back.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionKind {
    pub params: Vec<Kind>,
    pub returns: Kind,
    /// The kinds of error a call may raise: what the function says after
    /// `raises`, or what its type says (decision 56.1). Empty if it raises
    /// none. A firmware build calls such a function another way (5.5l);
    /// every other build calls both alike, and does not look at this.
    pub raises: Vec<String>,
}

/// What the checker worked out that the compiler cannot work out for
/// itself: the signature of every function, including the parameter and
/// return types that were never written down.
///
/// A function is looked up by its node in the syntax tree (its address in
/// memory), so the tree that was checked must be the tree that is compiled.
#[derive(Default)]
pub struct Types {
    /// Where each variable, function, class, enum and case was declared,
    /// by the number the checker gave it (`Variable::id`).
    placed: HashMap<usize, Span>,
    /// Each place a name is used: where, the name, its type there, and
    /// the number of what it means. An editor asks about these.
    used: Vec<(Span, String, String, usize)>,
    functions: HashMap<usize, FunctionKind>,
    /// For each function, which parameters only look at a value that
    /// somebody else owns. Such a parameter may be handed what the
    /// statement owns, and never frees what it was given.
    looks: HashMap<usize, Vec<bool>>,
    /// The type of each field of each class, in the order they are listed.
    classes: HashMap<usize, Vec<Kind>>,
    /// For each class, which of its fields are sized whole numbers. Such a
    /// field is stored in that many bits; everywhere else a sized number
    /// travels as an ordinary int.
    widths: HashMap<usize, Vec<Option<Width>>>,
    /// For each enum, the types of the values each case carries.
    enums: HashMap<usize, Vec<Vec<Kind>>>,
    /// Each place a variable is read after a check such as `is not none`
    /// has narrowed it, with the narrower type it has there.
    narrowed: HashMap<usize, Kind>,
    /// Each place a variable is read in order to give its owned value
    /// away. The variable holds nothing afterwards.
    moves: HashSet<usize>,
    /// Resources (decision 59.1), by where they are written: each `let`
    /// that holds one, each parameter that takes one (the function's
    /// place and the parameter's name), and each name read to give one
    /// away. `checked` makes the program what they mean.
    resource_lets: HashSet<(usize, usize)>,
    resource_params: HashSet<(usize, usize, String)>,
    gives: HashSet<(usize, usize)>,
    /// Each `for line in source` over a `files.Reader`, by where it is
    /// written: `checked` makes it a loop of `read_line` (decision 59.7).
    readers: HashSet<(usize, usize)>,
    /// Each `let`, `var` or `const` whose variable owns its value, and so
    /// frees it when its block ends.
    owners: HashSet<usize>,
    /// Each statement that makes an owned value and does nothing with it.
    discarded: HashSet<usize>,
    /// Each list that begins life as an owned list: written out, built
    /// by a comprehension, or made by `split`, `map`, `filter` or `+`,
    /// where an owned list is wanted.
    fresh: HashSet<usize>,
    /// Each call of `split`, `map` or `filter`, and each `+` on two
    /// lists: an expression that makes a new list, which is nobody's yet.
    made: HashSet<usize>,
    /// Each owned value that is made in the middle of a statement and
    /// only looked at there, as in `print(size(make()))`. The statement
    /// owns it, and frees it when it is over.
    lent: HashSet<usize>,
    /// Was the program checked with every list and object owned? Then
    /// every class is an owned class, whatever its declaration says.
    one_owner: bool,
    /// For each generic function, the copies made of it: one for each set
    /// of types it is called with. A copy is an ordinary function with a
    /// name such as `first[int]`, and the compilers compile it as one.
    function_copies: HashMap<usize, Vec<Arc<ast::Function>>>,
    /// The same for each generic class: `Stack[int]`, `Stack[str]`.
    class_copies: HashMap<usize, Vec<Arc<ast::Class>>>,
    /// The same for each generic enum: `Maybe[int]` (decision 58.1).
    enum_copies: HashMap<usize, Vec<Arc<ast::Enum>>>,
    /// Each call of a generic function or class, with the name of the
    /// copy that the call reaches. Also each case of a generic enum where
    /// it is used, with the copy of the enum it is a case of there: the
    /// call in `Just(3)`, and the name in a bare `Nothing`.
    copy_called: HashMap<usize, String>,
    /// What each type written in the program stands for. Inside a copy,
    /// `T` is whatever that copy was made for.
    annotations: HashMap<usize, Kind>,
    /// Each list written out where the place says more about the items
    /// than they say themselves, with the kind of its items:
    /// `[Circle(1.0), Square(2.0)]` is a list of `Shape`, and `[1, 2]` a
    /// list of `int or none`, where the place wants one.
    items: HashMap<usize, Kind>,
    /// Each list written out, list comprehension and call that makes a
    /// new `list[u8]`, which a compiled program stores a byte an item
    /// (decision 36.2).
    bytes: HashSet<usize>,
    /// Each `count += 1` whose place holds a sized number, by the line
    /// and column it starts at, with the size. It means `count =
    /// u8(count + 1)`, and `checked` makes it that.
    fits: HashMap<(usize, usize), Width>,
    /// Each call that turns a case of an enum with numbers into its
    /// number (`int(Ack)`, `u8(Ack)`) or a number into a case
    /// (`Reply(6)`), by where the call starts (decision 35.2). `checked`
    /// makes each a call of a function it writes beside the enum.
    numbered: HashMap<(usize, usize), ast::Numbered>,
    /// Where each enum is written that such a call uses: those are the
    /// enums that get the two functions.
    numbered_enums: HashSet<(usize, usize)>,
    /// Each float written out where an f32 is wanted, by where it starts:
    /// `0.5` in `let gain: f32 = 0.5`. `checked` makes each `f32(0.5)`
    /// (decision 41.4), so every engine meets an f32 there.
    f32s: HashSet<(usize, usize)>,
    /// Each f32 that goes where a float is wanted, by where it starts.
    /// `checked` makes each `float(...)`, so that a float holds a float
    /// in every engine, and prints as one (decision 41.3).
    widened: HashSet<(usize, usize)>,
    /// Each whole number written out where a u64 is wanted, by where it
    /// starts. `checked` makes each `u64..of(n)` (decision 42.5): a u64
    /// past the largest int is written as the int with the same bits.
    pub u64s: HashSet<(usize, usize)>,
    /// Each `+`, `-`, `*` and `<<` of u64s written straight inside
    /// `u64.wrap( )`, by where its right side starts, and the function
    /// `checked` makes it a call of, which cuts the answer to 64 bits
    /// where it would have failed (decision 42.6).
    pub wrapping: HashMap<(usize, usize), &'static str>,
    /// Each `u8.max`, `int.min` and the like by where it starts, and the
    /// value `checked` puts there; and `u64.wrap`, which becomes
    /// `u64..of` (decision 42.7).
    pub limits: HashMap<(usize, usize), Expr>,
    /// Each library of C a program declares with `c.library`, written
    /// out, in the order they are met (decision 44.10).
    pub c_libraries: Vec<Rc<crate::bridge_c::Library>>,
    /// In a firmware build: each function that raises nothing, named or
    /// written where a value goes, that arrives where a function type
    /// says `raises`, by where it starts. Such a function is made as one
    /// that can raise and never does, so that it is called as the type
    /// says (decision 56.5). With each, the kind the type says it raises.
    pub made_raising: HashMap<Spot, String>,
    /// Each function written where a value goes that is kept, and so
    /// takes the values with one owner that it uses, by where it starts:
    /// the names of what it takes, in order (decision 56.8). The function
    /// owns them from then on, and frees them when it is freed.
    pub taken: HashMap<Spot, Vec<String>>,
    /// Each method of an object used as a value, `counter.bump`, by the
    /// address of that expression: the function written where a value
    /// goes that it stands for, `function(by: int) -> int { return
    /// counter.bump(by) }` (decision 56.9). The compilers compile that.
    pub method_values: HashMap<usize, Arc<Expr>>,
}

/// Where something stands, told apart in each copy of a generic: the
/// declaration of the named function it is written in (its address, or 0
/// for the top of the file), then its line and column. The copies of a
/// generic share every line and column, and each has a declaration of
/// its own.
pub type Spot = (usize, usize, usize);

/// The spot of `span`, inside the function whose declaration is at
/// `within`.
pub fn spot(within: usize, span: Span) -> Spot {
    (within, span.line, span.col)
}

/// One place where a name is used, for an editor that asks "what is
/// this, and where is it declared?".
#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    /// Where the name is written.
    pub at: Span,
    pub name: String,
    /// Its type at this place, as a message would show it.
    pub ty: String,
    /// Where it was declared. `None` for what comes with Zeno.
    pub declared: Option<Span>,
}

impl Types {
    /// Every place a name is used, in the order the checker met them.
    pub fn uses(&self) -> Vec<Use> {
        self.used
            .iter()
            .map(|(at, name, ty, id)| Use { at: *at, name: name.clone(), ty: ty.clone(), declared: self.placed.get(id).copied() })
            .collect()
    }

    /// Does reading the variable `expr` here give its owned value away?
    pub fn moves(&self, expr: &ast::Expr) -> bool {
        self.moves.contains(&(expr as *const ast::Expr as usize))
    }

    /// Does each value of this enum have one owner? It does where
    /// everything has one, if some case carries values: such a value is
    /// kept in a block of its own, which somebody has to free.
    pub fn owned_enum(&self, decl: &ast::Enum) -> bool {
        self.one_owner && decl.variants.iter().any(|case| !case.fields.is_empty())
    }

    /// Does the variable this statement declares own its value?
    pub fn owner(&self, statement: &ast::Stmt) -> bool {
        self.owners.contains(&(statement as *const ast::Stmt as usize))
    }

    /// Does the variable a `let a, b = ...` declares at `place` own its
    /// value (decision 57)?
    pub fn owner_at(&self, place: &ast::Place) -> bool {
        self.owners.contains(&(place as *const ast::Place as usize))
    }

    /// Is the value a `_` of a `let a, b = ...` receives an owned value,
    /// which is freed there (decision 57)?
    pub fn discarded_at(&self, place: &ast::Place) -> bool {
        self.discarded.contains(&(place as *const ast::Place as usize))
    }

    /// Is `expr`, written as a statement, an owned value nobody keeps?
    pub fn discarded(&self, expr: &ast::Expr) -> bool {
        self.discarded.contains(&(expr as *const ast::Expr as usize))
    }

    /// Does the list written at `expr` start out owned?
    pub fn fresh(&self, expr: &ast::Expr) -> bool {
        self.fresh.contains(&(expr as *const ast::Expr as usize))
    }

    /// Is `expr` an owned value that the statement around it owns?
    pub fn lent(&self, expr: &ast::Expr) -> bool {
        self.lent.contains(&(expr as *const ast::Expr as usize))
    }

    /// Do the objects of this class have one owner each? They do if it
    /// is an `owned class`, and in a build where everything is owned.
    pub fn owned_class(&self, decl: &ast::Class) -> bool {
        decl.owned || self.one_owner
    }

    pub fn class(&self, decl: &ast::Class) -> Option<&Vec<Kind>> {
        self.classes.get(&(decl as *const ast::Class as usize))
    }

    /// The size of each field of the class, for the ones that have one:
    /// `level: u8` is stored in a byte.
    pub fn widths(&self, decl: &ast::Class) -> Option<&Vec<Option<Width>>> {
        self.widths.get(&(decl as *const ast::Class as usize))
    }

    pub fn enumeration(&self, decl: &ast::Enum) -> Option<&Vec<Vec<Kind>>> {
        self.enums.get(&(decl as *const ast::Enum as usize))
    }

    /// The copies of a generic function, in the order they were first needed.
    pub fn function_copies(&self, decl: &Arc<ast::Function>) -> &[Arc<ast::Function>] {
        self.function_copies.get(&(Arc::as_ptr(decl) as usize)).map_or(&[], Vec::as_slice)
    }

    /// The copies of a generic class.
    pub fn class_copies(&self, decl: &ast::Class) -> &[Arc<ast::Class>] {
        self.class_copies.get(&(decl as *const ast::Class as usize)).map_or(&[], Vec::as_slice)
    }

    /// The copies of a generic enum.
    pub fn enum_copies(&self, decl: &ast::Enum) -> &[Arc<ast::Enum>] {
        self.enum_copies.get(&(decl as *const ast::Enum as usize)).map_or(&[], Vec::as_slice)
    }

    /// If `call` calls a generic function or builds an object of a generic
    /// class, the name of the copy it means: `first[int]`. If it is a case
    /// of a generic enum, called or bare, the copy of the enum: `Maybe[int]`.
    pub fn copy_called(&self, call: &ast::Expr) -> Option<&str> {
        self.copy_called.get(&(call as *const ast::Expr as usize)).map(String::as_str)
    }

    /// What the type written at `ty` stands for, if the checker got as far
    /// as reading it.
    pub fn annotation(&self, ty: &ast::Type) -> Option<&Kind> {
        self.annotations.get(&(ty as *const ast::Type as usize))
    }

    /// The name of the interface written after `is` at `ty`: `Shape`, or
    /// for a copy of a generic interface, `Source[int]` (decision 58.6).
    pub fn interface_name(&self, ty: &ast::Type) -> String {
        let mut found = self.annotation(ty);
        while let Some(Kind::Owned(inner)) = found {
            found = Some(inner);
        }
        match (found, ty) {
            (Some(Kind::Class(name)), _) => name.clone(),
            (_, ast::Type::Named { name, .. }) => name.clone(),
            (_, other) => crate::printer::type_(other),
        }
    }

    pub fn function(&self, decl: &Arc<ast::Function>) -> Option<&FunctionKind> {
        self.functions.get(&(Arc::as_ptr(decl) as usize))
    }

    /// Which parameters of the function only look (see `looks`).
    pub fn looks(&self, decl: &Arc<ast::Function>) -> Vec<bool> {
        self.looks.get(&(Arc::as_ptr(decl) as usize)).cloned().unwrap_or_default()
    }

    /// Whether `expr` makes a new `list[u8]`: a list written out, a list
    /// comprehension, or a call such as `map` whose list is new.
    pub fn makes_bytes(&self, expr: &ast::Expr) -> bool {
        self.bytes.contains(&(expr as *const ast::Expr as usize))
    }

    /// If `expr` is a list written out where a list of an interface, or
    /// of values that may be `none`, is wanted: the kind of its items.
    pub fn items(&self, expr: &ast::Expr) -> Option<&Kind> {
        self.items.get(&(expr as *const ast::Expr as usize))
    }

    /// If `expr` is a variable read where a check has narrowed it (`user`
    /// inside `if user is not none { ... }`), the type it has there.
    pub fn narrowed(&self, expr: &ast::Expr) -> Option<&Kind> {
        self.narrowed.get(&(expr as *const ast::Expr as usize))
    }
}

fn kind(ty: &Ty) -> Kind {
    kind_within(ty, 0)
}

/// `kind`, counting how many function types deep it has gone. A function
/// can return itself, and its type would then never end.
fn kind_within(ty: &Ty, depth: usize) -> Kind {
    let function = |params: Vec<&Ty>, returns: &Ty, raises: Vec<String>| {
        Kind::Function(Box::new(FunctionKind {
            params: params.into_iter().map(|param| kind_within(param, depth + 1)).collect(),
            returns: kind_within(returns, depth + 1),
            raises,
        }))
    };
    match ty {
        // The mark that says "this has one owner" goes at the front,
        // where the compilers look for it.
        Ty::Optional(inner) => match kind_within(inner, depth) {
            Kind::Owned(inner) => Kind::Owned(Box::new(Kind::Optional(inner))),
            inner => Kind::Optional(Box::new(inner)),
        },
        list @ (Ty::List(_) | Ty::Fixed(..)) if is_bytes(list) => Kind::List(Box::new(Kind::Byte)),
        // To the compilers a list of a fixed length is a list.
        Ty::Fixed(item, _) => Kind::List(Box::new(kind_within(item, depth))),
        Ty::List(item) => Kind::List(Box::new(kind_within(item, depth))),
        Ty::Dict(key, value) => Kind::Dict(Box::new(kind_within(key, depth)), Box::new(kind_within(value, depth))),
        Ty::Owned(inner) => Kind::Owned(Box::new(kind_within(inner, depth))),
        Ty::Instance(class) if class.owned => Kind::Owned(Box::new(Kind::Class(class.name.clone()))),
        Ty::EnumValue(info) if info.owned => Kind::Owned(Box::new(Kind::Enum(info.name.clone()))),
        // Looking at a value is not owning it.
        Ty::Borrowed(inner) => match kind_within(inner, depth) {
            Kind::Owned(inner) => *inner,
            inner => inner,
        },
        Ty::Error(_) => Kind::Error,
        Ty::Several(types) => Kind::Several(types.iter().map(|ty| kind_within(ty, depth)).collect()),
        Ty::Callable(callable) if depth < 4 => {
            let kind = function(callable.params.iter().collect(), &callable.returns, callable.raises.iter().cloned().collect());
            match callable.owned {
                true => Kind::Owned(Box::new(kind)),
                false => kind,
            }
        }
        Ty::Function(info) if depth < 4 && info.self_type.is_none() => match &*info.state.borrow() {
            State::Checked(signature) | State::Checking(signature) => {
                function(signature.params.iter().map(|slot| &slot.ty).collect(), &signature.returns, signature.raises.clone())
            }
            State::NotChecked => Kind::Other(ty.to_string()),
        },
        other => plain_kind(other),
    }
}

/// `list[u8]`, as it is or with one owner.
/// Is a value of this type one that two places can hold at once, so that
/// a change made through one is seen through the other? A list, a dict
/// and an object are. A number, a text and an enum value are not.
fn is_shared(ty: &Ty) -> bool {
    match ty {
        Ty::List(_) | Ty::Fixed(..) | Ty::Dict(..) | Ty::Instance(_) | Ty::Owned(_) | Ty::Borrowed(_) | Ty::Element | Ty::Timer => {
            true
        }
        Ty::Optional(inner) => is_shared(inner),
        _ => false,
    }
}

fn is_bytes(ty: &Ty) -> bool {
    match ty {
        Ty::Owned(inner) | Ty::Borrowed(inner) => is_bytes(inner),
        Ty::List(item) | Ty::Fixed(item, _) => matches!(**item, Ty::Sized(width) if width.bits == 8 && !width.signed),
        _ => false,
    }
}

fn plain_kind(ty: &Ty) -> Kind {
    match ty {
        // A sized number is an ordinary int to the stages after this one.
        // Only a field of an object stores it small: see `Types::widths`.
        Ty::Int | Ty::Sized(_) => Kind::Int,
        Ty::Float => Kind::Float,
        Ty::F32 => Kind::F32,
        Ty::U64 => Kind::U64,
        Ty::Str => Kind::Str,
        Ty::Bool => Kind::Bool,
        Ty::Pin => Kind::Pin,
        Ty::InputPin => Kind::InputPin,
        Ty::Serial => Kind::Serial,
        Ty::AnalogPin => Kind::AnalogPin,
        Ty::PwmPin => Kind::PwmPin,
        Ty::Ticker => Kind::Ticker,
        Ty::Interrupt => Kind::Interrupt,
        // `pull_up` and `pull_down` are the strs of their names, so
        // printing one, comparing two and keeping them in a list work
        // as they do for any str. The checker keeps them apart.
        Ty::Pull => Kind::Str,
        // `rising`, `falling` and `either` are the strs of their names too.
        Ty::Edge => Kind::Str,
        Ty::Pointer(pointee) => Kind::Pointer(*pointee),
        Ty::Register(width) => Kind::Register(*width),
        Ty::None => Kind::None,
        Ty::Quantity(unit) => Kind::Quantity(unit.to_string()),
        Ty::Instance(class) => Kind::Class(class.name.clone()),
        Ty::EnumValue(info) => Kind::Enum(info.name.clone()),
        Ty::CLibrary(library) => Kind::CLibrary(library.number),
        Ty::CPointer(to) => Kind::CPointer(to.to_string()),
        other => Kind::Other(other.to_string()),
    }
}

/// `check`, also returning what was learned about the program's types.
#[cfg(test)]
pub fn check_with_types(program: &Program, strict: bool) -> (Vec<Error>, Types) {
    check_by(program, Rules { strict, one_owner: false, firmware: false })
}

/// Check a program, and give back the program to run or to build.
///
/// It is the program that was given, but for one thing: `count += 1`
/// where `count` is a sized number has become what it means, `count =
/// u8(count + 1)` (decision 10.27). The checker is the one stage that
/// knows the type of `count`, so it says which statements those are,
/// `Program::with_fits` rewrites them, and the program is checked once
/// more, because the checker's notes are kept by where each node is in
/// memory and the rewrite made new nodes. A program with no such
/// statement is checked once.
pub fn checked(program: Program, rules: Rules) -> (Program, Vec<Error>, Types) {
    let (errors, types) = check_by(&program, rules);
    if !errors.is_empty()
        || (types.fits.is_empty()
            && types.numbered.is_empty()
            && types.f32s.is_empty()
            && types.widened.is_empty()
            && types.u64s.is_empty()
            && types.wrapping.is_empty()
            && types.limits.is_empty()
            && types.resource_lets.is_empty()
            && types.resource_params.is_empty()
            && types.readers.is_empty())
    {
        return (program, errors, types);
    }
    let Types {
        fits, numbered, numbered_enums, f32s, widened, u64s, wrapping, limits, resource_lets, resource_params, gives, readers, ..
    } =
        types;
    let program = program
        .with_u64s(u64s, wrapping, limits)
        .with_fits(&fits)
        .with_numbers(numbered, &numbered_enums)
        .with_f32s(f32s, widened)
        .with_readers(&readers)
        .with_resources(resource_lets, resource_params, gives);
    let (errors, types) = check_by(&program, rules);
    (program, errors, types)
}

/// `check_with_types`, by the rules given.
pub fn check_by(program: &Program, rules: Rules) -> (Vec<Error>, Types) {
    let strict = rules.strict;
    // A file that begins with the line `strict` asks for every type in
    // that file to be known. `--strict` and `zeno build` ask it of all
    // the files of the program.
    let files_part = program.modules.iter().position(|file| file.path == "library/files.zeno").map(|place| place + 1);
    let strict_files: Vec<bool> =
        std::iter::once(program.strict).chain(program.modules.iter().map(|module| module.strict)).collect();
    let one_owner = rules.one_owner;

    // Parameters and fields with no type written are worked out from how
    // they are used: `add(1, 2)` says both parameters of `add` are ints.
    // One answer can lead to another (if `f` passes its parameter on to
    // `g`, then `g` is only known once `f` is), so the program is checked
    // repeatedly, each pass starting from what the last one learned, until
    // a pass learns nothing new. Only the final pass reports errors.
    let mut known: HashMap<SlotId, Key> = HashMap::new();
    let mut open: HashSet<SlotId> = HashSet::new();
    let mut origins: HashMap<SlotId, usize> = HashMap::new();
    // The functions of interfaces that some class's function takes out
    // for. A round can find more than the round before knew, so they are
    // carried to the next round, as what is worked out is (58.21).
    let mut taken: HashMap<(DeclId, String), String> = HashMap::new();
    let mut settled = false;
    for _ in 0..MAX_ROUNDS {
        let mut checker = Checker::new(strict, false, known.clone(), open.clone());
        checker.strict_files = strict_files.clone();
        checker.files_part = files_part;
        checker.one_owner = one_owner;
        checker.firmware = rules.firmware;
        checker.taken = taken.clone();
        checker.run(program);
        origins = std::mem::take(&mut checker.origins);
        let grew = checker.taking.keys().any(|key| !taken.contains_key(key));
        taken.extend(std::mem::take(&mut checker.taking));

        let mut next_known = HashMap::new();
        let mut next_open = open.clone(); // once open, always open
        for (id, seen) in checker.seen {
            match seen {
                Seen::Known(key) if !next_open.contains(&id) => {
                    next_known.insert(id, key);
                }
                Seen::Known(_) => {}
                Seen::Unknowable => {
                    next_open.insert(id);
                }
            }
        }
        next_known.retain(|id, _| !next_open.contains(id));

        settled = next_known == known && next_open == open && !grew;
        known = next_known;
        open = next_open;
        if settled {
            break;
        }
    }
    if !settled {
        // Still changing after that many passes. Rather than trust a
        // half-finished answer, treat everything worked out as unknown:
        // unknown is always safe, a wrong guess is not.
        open.extend(known.drain().map(|(id, _)| id));
    }

    // What is reported comes from a round in which no function of an
    // interface was found to take out that the round did not know of
    // before it began. So a call read before the class that takes out is
    // held to the same guards as one read after it.
    let checker = loop {
        let mut checker = Checker::new(strict, true, known.clone(), open.clone());
        checker.strict_files = strict_files.clone();
        checker.files_part = files_part;
        checker.one_owner = one_owner;
        checker.firmware = rules.firmware;
        checker.types.one_owner = one_owner;
        checker.known_origins = origins.clone();
        checker.taken = taken.clone();
        checker.run(program);
        if checker.taking.keys().all(|key| taken.contains_key(key)) {
            break checker;
        }
        taken.extend(std::mem::take(&mut checker.taking));
    };
    let mut errors = checker.errors;
    // A number past the largest int, written where no u64 is wanted.
    for (span, n) in &checker.big_numbers {
        if !checker.types.u64s.contains(&(span.line, span.col)) {
            errors.push(Error::new(
                format!("{n} is too big for an int ({} to {}). Only a u64 holds it: write it where a u64 is wanted, as in `let x: u64 = {n}`", i64::MIN, i64::MAX),
                *span,
            ));
        }
    }
    // A file that asks for the collector, in a program built with none.
    if one_owner {
        let files = std::iter::once(program.memory).chain(program.modules.iter().map(|module| module.memory));
        for (file, memory) in files.enumerate() {
            if memory == Some(crate::ast::Memory::Collector) {
                errors.push(Error::new(
                    "this file says `memory collector`, and the program is built with no collector. \
                     Take the line out, or build the program with `--collector`",
                    Span { line: file * crate::token::LINES_A_FILE + 1, col: 1 },
                ));
            }
        }
    }
    // Line numbers count on from one file to the next (see `Span`), so
    // this puts the mistakes of each file together, the first file's first.
    errors.sort_by_key(|error| (error.span.line, error.span.col));
    // Two copies of a generic can find the same mistake. Once is enough.
    errors.dedup_by(|a, b| a.span == b.span && a.message == b.message);
    (errors, checker.types)
}

// ---------------------------------------------------------------------- Types

/// The type of an expression, as far as the checker can tell.
#[derive(Clone)]
enum Ty {
    /// Not known until the program runs. Accepted everywhere.
    Dynamic,
    /// The type of something that already has an error reported against it.
    /// Accepted everywhere, so the same mistake is not reported twice.
    Invalid,

    Int,
    Float,
    /// A float of 32 bits: `f32` (decision 41). Its sums are done in 32
    /// bits, and it goes where a float is wanted.
    F32,
    /// A whole number of 64 bits with no sign: `u64` (decision 42). It
    /// meets only another u64 or a number written out.
    U64,
    Str,
    Bool,
    None,
    /// A number with a unit, such as `500 ms`.
    Quantity(Rc<str>),
    /// A pin of the board, from the `board` module.
    Pin,
    /// A pin of the board that the program reads: `input_pin(2)`.
    InputPin,
    /// How an input pin is held when nothing drives it: `pull_up` or
    /// `pull_down`, from the `board` module.
    Pull,
    /// A serial port of the board: `serial(1, 115200 baud)`.
    Serial,
    /// A pin whose voltage the program reads: `analog_pin(0)`.
    AnalogPin,
    /// A pin that is switched on and off many times a second:
    /// `pwm_pin(9, 1 kHz)`.
    PwmPin,
    /// A beat the program keeps time by: `ticker(10 ms)`.
    Ticker,
    /// Which change of a pin an interrupt counts: `rising`, `falling`
    /// or `either`, from the `board` module.
    Edge,
    /// The edges of an input pin, counted by the board:
    /// `button.interrupt(falling)`.
    Interrupt,
    /// An element of the page, from the `web` module.
    Element,
    /// What happened on the page: what a function given to `on` may ask for.
    Event,
    /// A call that waits: what `web.after` and `web.every` give.
    Timer,
    /// A whole number with a size: `u8`, `i32`. See src/ints.rs.
    Sized(Width),
    /// A raw pointer to numbers of one size: `pointer[u8]`. Reading
    /// through it, writing through it and freeing it need `unsafe`.
    Pointer(Pointee),
    /// A register of the board at a fixed address: `Register[u32]`.
    Register(Width),
    List(Box<Ty>),
    /// `list[u8, 64]`: a list that always has this many items (decision
    /// 36.4). Inside is the type of an item. Anything that only reads a
    /// list reads one of these; nothing can change how long it is.
    Fixed(Box<Ty>, u64),
    /// `dict[str, int]`: the type of its keys, and of its values.
    Dict(Box<Ty>, Box<Ty>),
    /// `T or none`
    Optional(Box<Ty>),
    /// An object of a class.
    Instance(Rc<ClassInfo>),
    /// A value of an enum.
    EnumValue(Rc<EnumInfo>),
    /// A built-in error value, such as a `ValueError`.
    Error(Rc<str>),

    /// `owned list[int]`: a list with exactly one owner. (An object of an
    /// `owned class` is an `Instance` whose class says so.)
    Owned(Box<Ty>),
    /// A look at a value that something else owns: what reading an owner
    /// gives, and the type of a parameter that only looks. Inside is the plain
    /// type. It fits nowhere that would keep the value, which is the whole
    /// point: only the places that just look take the wrapper off.
    Borrowed(Box<Ty>),

    /// A function known only by its written type: `function(int) -> int`
    Callable(Rc<CallableType>),

    // Things that can be called.
    Function(Rc<FunctionInfo>),
    /// A method read from an object: `self` is already supplied.
    BoundMethod(Rc<FunctionInfo>),
    Builtin(&'static str),
    /// A method of a str, a list or a dict, with the type it was read from.
    BuiltinMethod(Box<Ty>, Rc<str>),
    Class(Rc<ClassInfo>),
    Enum(Rc<EnumInfo>),
    /// An enum case that still needs its values: `Circle`
    CaseConstructor(Rc<EnumInfo>, usize),
    /// A case of a generic enum, by its place in it: `Just`, `Nothing` of
    /// `enum Maybe[T]` (decision 58.1). Which copy of the enum it is a
    /// case of is worked out where it is used: from the values it is
    /// given, or from the place it goes.
    GenericCase(Rc<Generic>, usize),
    ErrorType(Rc<str>),

    Module(&'static str),

    /// What `c.library(...)` gives when the library and its declarations
    /// are written out in the program (decision 44.10).
    CLibrary(Rc<crate::bridge_c::Library>),
    /// One function of such a library, by its place in it.
    CFunction(Rc<crate::bridge_c::Library>, usize),
    /// A pointer that C gave, to what it names: `counter`, `struct tm`,
    /// `void`. Zeno keeps it, gives it back, and never looks inside.
    CPointer(Rc<str>),

    // Generics. See the section "Generic functions and classes".
    /// A function or a class with type parameters, by its name: `first`,
    /// `Stack`. It is not a type yet, and not something that can run:
    /// each use of it picks a copy.
    Generic(Rc<Generic>),
    /// A type parameter that has not been filled in: the `T` of
    /// `items: list[T]`, as the generic itself is written. Only seen
    /// while working out which copy a call means.
    Param(Rc<str>),
    /// A generic class given types that are not all filled in yet:
    /// `Stack[T]`, written in the parameters of a generic function.
    Applied(Rc<Generic>, Vec<Ty>),
    /// The name of a type parameter, inside a copy: there `T` is another
    /// name for the type the copy was made for.
    Alias(Box<Ty>),
    /// What a function gives that gives two values or more: `-> (int,
    /// str)` (decision 57). It is never the type of a value: only of a
    /// call, received by as many places, or given by a `return`.
    Several(Vec<Ty>),
}

/// A function or a class with type parameters: `function first[T](...)`,
/// `class Stack[T] { ... }`.
struct Generic {
    /// Where its declaration sits in the syntax tree. The compilers ask
    /// for its copies by this.
    at: usize,
    id: DeclId,
    name: String,
    /// The names of its type parameters: `T`.
    params: Vec<String>,
    /// What each type parameter asks of its type, as written after `is`:
    /// `[T is Ordered]` (decision 58.3). Usually none.
    bounds: Vec<Option<ast::Type>>,
    /// The same, worked out the first time it is needed.
    asks: RefCell<Option<Rc<[Option<Bound>]>>>,
    template: Template,
    env: Env,
    span: Span,
    above: Rc<[String]>,
    /// Its parameters and what it returns, with the type parameters still
    /// in them. Worked out the first time it is called.
    outline: RefCell<Option<Rc<Outline>>>,
}

/// What a type parameter asks of the type it stands for (decision 58.3).
#[derive(Clone)]
enum Bound {
    /// `[T is Ordered]`: that `<` works on two of it. Every kind of
    /// number, a quantity and text are; no class is yet.
    Ordered,
    /// `[S is Shape]`: a class that says it is the interface, or the
    /// interface itself.
    Interface(Rc<ClassInfo>),
}

/// What a copy of a generic is copied from.
enum Template {
    Function(Arc<ast::Function>),
    Class(Rc<ast::Class>),
    Enum(Rc<ast::Enum>),
}

/// The name written at `expr`, if it is one.
fn ty_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Name(name) => name.clone(),
        _ => String::new(),
    }
}

/// Is this a value of a copy of a generic enum, as `Maybe[int]`?
fn is_enum_copy(ty: &Ty) -> bool {
    matches!(ty, Ty::EnumValue(info) if info.origin.is_some())
}

/// Is `expr` a case of a generic enum that carries nothing, as `Nothing`?
fn bare_generic_case(expr: &Expr, env: &Env) -> bool {
    match &expr.kind {
        ExprKind::Name(name) => {
            matches!(env.get(name), Some(Variable { ty: Ty::GenericCase(..), case: Some(_), .. }))
        }
        _ => false,
    }
}

/// A bare case of a generic enum given to a generic, whose type waits
/// until the other arguments have settled the generic's types: the `End`
/// of `Link(3, End)` (decision 58.1).
struct Later {
    /// Its place among the arguments.
    at: usize,
    /// The generic enum it is a case of, and its name.
    generic: Rc<Generic>,
    name: String,
    /// Does the parameter keep what it is given? If not, it looks, as an
    /// argument that is read at once is lent (decision 56.11).
    keeps: bool,
}

/// A generic as a call sees it before the types are known: `items:
/// list[T]` and `-> T`.
struct Outline {
    slots: Vec<Slot>,
    returns: Option<Ty>,
}

/// One copy of a generic.
#[derive(Clone)]
enum Made {
    Function(Rc<FunctionInfo>),
    Class(Rc<ClassInfo>),
    Enum(Rc<EnumInfo>),
}

struct CallableType {
    params: Vec<Ty>,
    returns: Ty,
    /// `function(str) -> int raises ValueError`: the one kind of error a
    /// call through such a value may raise (decision 56.1).
    raises: Option<String>,
    /// Does a value of this type have one owner? It does where
    /// everything has: a function written where a value goes is kept in
    /// a block of its own, and somebody has to free it.
    owned: bool,
}

/// Names a declaration: where it starts in the file, and what it is called.
/// Types from one checking pass are carried to the next by this, since the
/// passes do not share any of their working data.
type DeclId = (usize, usize, String);

/// Names one parameter or field: the declaration it belongs to, and its name.
type SlotId = (DeclId, String);

struct ClassInfo {
    id: DeclId,
    name: String,
    /// Declared `owned class`: every object of it has exactly one owner.
    owned: bool,
    fields: RefCell<Vec<Slot>>,
    methods: RefCell<HashMap<String, Rc<FunctionInfo>>>,
    /// The fields and methods declared `private`, and the file the class
    /// is written in: the only file that can use them.
    private: Vec<String>,
    file: usize,
    /// Is this an `interface`? Then it has no fields, its functions have
    /// no bodies, and no object is ever built of it.
    interface: bool,
    /// The interfaces the class says it is: `class Circle is Shape`.
    is: RefCell<Vec<Rc<ClassInfo>>>,
    /// As for a function (`FunctionInfo::above`): the defaults of the
    /// fields are worked out when an object is built, and may use these.
    above: Rc<[String]>,
    /// The scope the class is declared in.
    env: Env,
    /// Each field that has a default, with the names the default mentions.
    defaults: Vec<(String, Vec<String>)>,
    /// For a copy of a generic class, the generic and the types this copy
    /// was made for: `Stack` and `[int]`.
    origin: Option<(Rc<Generic>, Vec<Ty>)>,
    /// For a copy, the words that say so under an error found inside it.
    copy_note: Option<String>,
}

struct EnumInfo {
    id: DeclId,
    name: String,
    cases: RefCell<Vec<CaseInfo>>,
    /// Does some case carry values? Such a value is kept in a block of
    /// its own, like an object, where a bare case is only a number.
    carries: bool,
    /// Does each value have one owner, which frees it? It does where
    /// everything has (a build with no collector), if some case carries
    /// values: those are what there is to free.
    owned: bool,
    /// `Ack = 6`: the number of each case, in order, when the enum has
    /// them (decision 35.1).
    numbers: Option<Vec<i64>>,
    /// For a copy of a generic enum, the generic and the types this copy
    /// was made for: `Maybe` and `[int]` (decision 58.1).
    origin: Option<(Rc<Generic>, Vec<Ty>)>,
}

struct CaseInfo {
    name: String,
    fields: Vec<Slot>,
}

/// A named place for an argument: a parameter, a class field, an enum case field.
#[derive(Clone)]
struct Slot {
    name: String,
    ty: Ty,
    has_default: bool,
    /// Set when no type was written, so that its type is being worked out
    /// from the values that arrive here.
    id: Option<SlotId>,
}

/// A type in a form that outlives one checking pass. It is `Ty` without
/// the things that only exist during a pass.
#[derive(Clone, PartialEq, Debug)]
enum Key {
    Int,
    Float,
    Str,
    Bool,
    Pin,
    InputPin,
    Pull,
    Serial,
    AnalogPin,
    PwmPin,
    Ticker,
    Edge,
    Interrupt,
    Element,
    Event,
    Timer,
    /// A sized whole number, by its name: `u8`.
    Sized(&'static str),
    /// A pointer, by the name of what it points at.
    Pointer(&'static str),
    /// A register, by the name of its size.
    Register(&'static str),
    None,
    Quantity(String),
    List(Box<Key>),
    Dict(Box<Key>, Box<Key>),
    Optional(Box<Key>),
    Class(DeclId),
    /// A copy of a generic class: the generic, and the types it was made for.
    Copy(DeclId, Vec<Key>),
    Enum(DeclId),
    Error(String),
    /// What a function gives that gives several values (decision 57).
    Several(Vec<Key>),
}

/// What has been learned so far about a parameter or field with no type.
enum Seen {
    /// Every value that arrives here has this type.
    Known(Key),
    /// The values differ, or one of them is itself unknown until run time.
    Unknowable,
}

/// The one key that covers both, if there is one.
fn join_keys(a: &Key, b: &Key) -> Option<Key> {
    match (a, b) {
        _ if a == b => Some(a.clone()),
        // A parameter that is handed a u8 by one call and an int by
        // another is an int.
        (Key::Sized(_), Key::Int | Key::Sized(_)) | (Key::Int, Key::Sized(_)) => Some(Key::Int),
        (Key::None, Key::Optional(_)) => Some(b.clone()),
        (Key::Optional(_), Key::None) => Some(a.clone()),
        (Key::None, other) | (other, Key::None) => Some(Key::Optional(Box::new(other.clone()))),
        (Key::Optional(inner), other) | (other, Key::Optional(inner)) if **inner == *other => {
            Some(Key::Optional(inner.clone()))
        }
        _ => None,
    }
}

/// What a caller needs to know about a function.
struct Signature {
    name: String,
    params: Vec<Slot>,
    returns: Ty,
    raises: Vec<String>,
    /// For a method: it takes an item out of something `self` holds, or
    /// puts a new value in the place of one. Then a call of it is held
    /// to the rule for doing that by hand (see `takes_out`).
    takes_out: bool,
}

/// A function and where it was defined. Its signature is worked out the
/// first time it is needed, because a return type that is not written down
/// has to be found by checking the body.
struct FunctionInfo {
    decl: Arc<ast::Function>,
    env: Env,
    span: Span,
    /// For a method, the type of `self`.
    self_type: Option<Ty>,
    state: RefCell<State>,
    /// The variables its block declares above the line it is written on.
    /// The function may use them, so it must not run before they exist
    /// (see `Early`).
    above: Rc<[String]>,
    /// For a copy of a generic function, or a method of a copy of a
    /// generic class, the words that say so under an error found inside it.
    copy_note: Option<String>,
}

/// A function whose body is being checked, for catching a call that
/// comes too early.
///
/// A function can be called from above the line it is written on, and
/// its body is checked at the first place it is used. If that is above a
/// variable the function uses, the variable is not declared yet, so a
/// name in the body would be taken for a variable of the same name
/// further out. When the program ran, the name would mean the variable
/// of the function's own block, which has no value at that moment
/// (decision 3.6d).
struct Early {
    name: String,
    /// The function body being checked, and the one it is written in.
    body: usize,
    outer: usize,
    /// The variables above the function that are not declared yet.
    missing: Vec<String>,
    /// The ones of those that have been reported, so that each is
    /// reported once.
    told: Vec<String>,
    /// Is this a function written where a value goes? Such a function
    /// may look at the values of the function around it.
    literal: bool,
    /// The values with one owner that it looks at, from outside itself.
    captures: Vec<Capture>,
}

/// A value from outside a function written where a value goes, which
/// the function looks at: the variable's name, which variable it is, and
/// the function body that declares it.
#[derive(Clone)]
struct Capture {
    name: String,
    id: usize,
    function: usize,
}

#[derive(Clone)]
enum State {
    NotChecked,
    /// The body is being checked right now. A call that arrives in this
    /// state is recursion, and sees only what the signature declares.
    Checking(Rc<Signature>),
    Checked(Rc<Signature>),
}

impl Ty {
    fn list(item: Ty) -> Ty {
        Ty::List(Box::new(item))
    }

    fn dict(key: Ty, value: Ty) -> Ty {
        Ty::Dict(Box::new(key), Box::new(value))
    }

    /// A list or a dict: the two types that hold other values. Where
    /// everything has one owner, each of them has one.
    fn is_collection(&self) -> bool {
        matches!(self, Ty::List(_) | Ty::Fixed(..) | Ty::Dict(..))
    }

    /// True for the two types that are accepted everywhere.
    fn is_loose(&self) -> bool {
        matches!(self, Ty::Dynamic | Ty::Invalid)
    }

    fn is_number(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    /// Does this type have a `Dynamic` anywhere inside it?
    fn has_dynamic(&self) -> bool {
        match self {
            Ty::Dynamic => true,
            Ty::List(inner) | Ty::Fixed(inner, _) | Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner) => {
                inner.has_dynamic()
            }
            Ty::Dict(key, value) => key.has_dynamic() || value.has_dynamic(),
            _ => false,
        }
    }

    /// Is this a value with exactly one owner?
    fn is_owned(&self) -> bool {
        match self {
            Ty::Owned(_) => true,
            Ty::Instance(class) => class.owned,
            Ty::EnumValue(info) => info.owned,
            Ty::Callable(callable) => callable.owned,
            // What may be `none` has an owner if what is there has.
            Ty::Optional(inner) => inner.is_owned(),
            // `Box[T]`, in the parameters of a generic, where `Box` is an
            // `owned class`.
            Ty::Applied(generic, _) => matches!(&generic.template, Template::Class(class) if class.owned),
            _ => false,
        }
    }

    /// Is this a look at a value that somebody else owns? `or none` is
    /// always written outermost: a look at a `Task or none` is "a
    /// Task that is only looked at, or none".
    fn is_look(&self) -> bool {
        match self {
            Ty::Borrowed(_) => true,
            Ty::Optional(inner) => inner.is_look(),
            _ => false,
        }
    }

    /// Owned, or a look at something owned: a value that cannot be shared.
    fn is_tied(&self) -> bool {
        self.is_owned() || self.is_look()
    }

    /// The type as it is to somebody who only looks: an `owned list[int]`
    /// is read, indexed and printed exactly like a `list[int]`.
    fn looked_at(self) -> Ty {
        match self {
            Ty::Owned(inner) | Ty::Borrowed(inner) => *inner,
            Ty::Optional(inner) => Ty::Optional(Box::new(inner.looked_at())),
            other => other,
        }
    }

    /// The type as arithmetic sees it. Sums, comparisons, indexes and the
    /// built-in functions all work on plain ints, so a `u8` that takes
    /// part in one counts as an `int`, and the answer is an `int`.
    fn widened(&self) -> Ty {
        match self {
            Ty::Sized(_) => Ty::Int,
            other => other.clone(),
        }
    }

    /// `widened`, all the way down: a `list[u8]` holds the same values
    /// as a `list[int]`, which is what `==` and `in` ask about.
    fn as_ints(&self) -> Ty {
        match self {
            Ty::Sized(_) => Ty::Int,
            Ty::List(inner) => Ty::list(inner.as_ints()),
            Ty::Optional(inner) => Ty::Optional(Box::new(inner.as_ints())),
            other => other.clone(),
        }
    }

    /// Can a value of this type be `none`?
    fn accepts_none(&self) -> bool {
        matches!(self, Ty::None | Ty::Optional(_)) || self.is_loose()
    }
}

/// How a type is written in an error message.
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Ty::Dynamic | Ty::Invalid => write!(f, "any"),
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::F32 => write!(f, "f32"),
            Ty::U64 => write!(f, "u64"),
            Ty::Str => write!(f, "str"),
            Ty::Bool => write!(f, "bool"),
            Ty::Pin => write!(f, "Pin"),
            Ty::InputPin => write!(f, "InputPin"),
            Ty::Pull => write!(f, "Pull"),
            Ty::Serial => write!(f, "Serial"),
            Ty::AnalogPin => write!(f, "AnalogPin"),
            Ty::PwmPin => write!(f, "PwmPin"),
            Ty::Ticker => write!(f, "Ticker"),
            Ty::Edge => write!(f, "Edge"),
            Ty::Interrupt => write!(f, "Interrupt"),
            Ty::Element => write!(f, "Element"),
            Ty::Event => write!(f, "Event"),
            Ty::Timer => write!(f, "Timer"),
            Ty::Pointer(pointee) => write!(f, "pointer[{}]", pointee.name()),
            Ty::Register(width) => write!(f, "Register[{}]", width.name),
            Ty::Sized(width) => write!(f, "{}", width.name),
            Ty::None => write!(f, "none"),
            Ty::Quantity(unit) => write!(f, "a quantity in {unit}"),
            Ty::List(item) => write!(f, "list[{item}]"),
            Ty::Fixed(item, length) => write!(f, "list[{item}, {length}]"),
            Ty::Dict(key, value) => write!(f, "dict[{key}, {value}]"),
            // A function that may itself be `none` is written in brackets,
            // as it is in a program (decision 56.2).
            Ty::Optional(inner) if matches!(**inner, Ty::Callable(_)) => write!(f, "({inner}) or none"),
            Ty::Optional(inner) => write!(f, "{inner} or none"),
            Ty::Owned(inner) => write!(f, "owned {inner}"),
            // `a function`, `a quantity in V`: one "a" is enough.
            Ty::Borrowed(inner) => {
                let inner = inner.to_string();
                write!(f, "a {} that is only looked at", inner.strip_prefix("a ").unwrap_or(&inner))
            }
            Ty::Instance(class) => write!(f, "{}", class.name),
            Ty::EnumValue(info) => write!(f, "{}", info.name),
            Ty::Error(kind) => write!(f, "{kind}"),
            Ty::Callable(callable) => {
                let params: Vec<String> = callable.params.iter().map(as_parameter).collect();
                write!(f, "function({})", params.join(", "))?;
                match &callable.returns {
                    Ty::None => {}
                    // A `raises` after it would be read as this one's.
                    Ty::Callable(returned) if returned.raises.is_some() => write!(f, " -> ({})", callable.returns)?,
                    returns => write!(f, " -> {returns}")?,
                }
                match &callable.raises {
                    Some(kind) => write!(f, " raises {kind}"),
                    None => Ok(()),
                }
            }
            Ty::Function(_) | Ty::BoundMethod(_) | Ty::Builtin(_) | Ty::BuiltinMethod(..) => {
                write!(f, "a function")
            }
            Ty::Class(class) if class.interface => write!(f, "the interface {}", class.name),
            Ty::Class(class) => write!(f, "the class {}", class.name),
            Ty::Enum(info) => write!(f, "the enum {}", info.name),
            Ty::CaseConstructor(info, index) => {
                write!(f, "the enum case {}", info.cases.borrow()[*index].name)
            }
            Ty::ErrorType(kind) => write!(f, "the error type {kind}"),
            Ty::Module(name) => write!(f, "the module {name}"),
            Ty::CLibrary(library) => write!(f, "the C library {}", library.label),
            Ty::CFunction(library, at) => write!(f, "the C function {}", library.functions[*at].name),
            Ty::CPointer(to) => write!(f, "a C pointer to {to}"),
            Ty::Generic(generic) => match generic.template {
                Template::Function(_) => write!(f, "the generic function {}", generic.name),
                Template::Class(_) => write!(f, "the generic class {}", generic.name),
                Template::Enum(_) => write!(f, "the generic enum {}", generic.name),
            },
            Ty::GenericCase(generic, index) => match &generic.template {
                Template::Enum(decl) => write!(f, "the enum case {}", decl.variants[*index].name),
                _ => write!(f, "the enum case"),
            },
            Ty::Param(name) => write!(f, "{name}"),
            Ty::Applied(generic, types) => {
                let types: Vec<String> = types.iter().map(spelled).collect();
                write!(f, "{}[{}]", generic.name, types.join(", "))
            }
            Ty::Alias(inner) => write!(f, "the type {inner}"),
            Ty::Several(types) => {
                let types: Vec<String> = types.iter().map(Ty::to_string).collect();
                write!(f, "({})", types.join(", "))
            }
        }
    }
}

/// A type as it is spelled inside the name of a copy: `Stack[ms]`,
/// `first[list[int]]`. The same as in a message, but for a quantity,
/// which a message calls "a quantity in ms".
fn spelled(ty: &Ty) -> String {
    match ty {
        Ty::Quantity(unit) => unit.to_string(),
        Ty::List(item) => format!("list[{}]", spelled(item)),
        Ty::Dict(key, value) => format!("dict[{}, {}]", spelled(key), spelled(value)),
        Ty::Optional(inner) => format!("{} or none", spelled(inner)),
        other => other.to_string(),
    }
}

/// Is there a type parameter in `ty` that has not been filled in?
fn has_param(ty: &Ty) -> bool {
    mentions(ty, None)
}

/// Is the type parameter `name` in `ty`? With no name, is any?
fn mentions(ty: &Ty, name: Option<&str>) -> bool {
    match ty {
        Ty::Param(param) => name.is_none_or(|name| name == param.as_ref()),
        Ty::List(inner) | Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner) => mentions(inner, name),
        Ty::Dict(key, value) => mentions(key, name) || mentions(value, name),
        Ty::Applied(_, types) | Ty::Several(types) => types.iter().any(|ty| mentions(ty, name)),
        Ty::Callable(callable) => {
            callable.params.iter().any(|ty| mentions(ty, name)) || mentions(&callable.returns, name)
        }
        _ => false,
    }
}

/// Is `ty` a type parameter and nothing around it, give or take a look
/// and an `or none`? A `T`, and not a `list[T]`.
fn straight(ty: &Ty) -> bool {
    match ty {
        Ty::Param(_) => true,
        Ty::Optional(inner) | Ty::Borrowed(inner) => straight(inner),
        _ => false,
    }
}

/// A type as it is written on a parameter. One that looks at a value
/// with an owner is written as the plain type, and one that takes the
/// value over says `take`.
fn as_parameter(ty: &Ty) -> String {
    match ty {
        ty if ty.is_look() => ty.clone().looked_at().to_string(),
        ty if ty.is_owned() => format!("take {}", ty.clone().looked_at()),
        ty => ty.to_string(),
    }
}

/// Can a number that is written out, such as `1`, be of this type? It
/// can be an int, a float, a sized number or a quantity, and any of
/// those `or none`. What is not known yet is not ruled out.
fn takes_a_number(ty: &Ty) -> bool {
    match ty {
        Ty::Int | Ty::Float | Ty::F32 | Ty::U64 | Ty::Sized(_) | Ty::Quantity(_) | Ty::Dynamic | Ty::Invalid | Ty::Param(_) => true,
        Ty::Optional(inner) => takes_a_number(inner),
        _ => false,
    }
}

/// Are these the same type?
fn same(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (Ty::Int, Ty::Int)
        | (Ty::Float, Ty::Float)
        | (Ty::F32, Ty::F32)
        | (Ty::U64, Ty::U64)
        | (Ty::Str, Ty::Str)
        | (Ty::Bool, Ty::Bool)
        | (Ty::Pin, Ty::Pin)
        | (Ty::InputPin, Ty::InputPin)
        | (Ty::Pull, Ty::Pull)
        | (Ty::Serial, Ty::Serial)
        | (Ty::AnalogPin, Ty::AnalogPin)
        | (Ty::PwmPin, Ty::PwmPin)
        | (Ty::Ticker, Ty::Ticker)
        | (Ty::Edge, Ty::Edge)
        | (Ty::Interrupt, Ty::Interrupt)
        | (Ty::Element, Ty::Element)
        | (Ty::Event, Ty::Event)
        | (Ty::Timer, Ty::Timer)
        | (Ty::None, Ty::None) => true,
        (Ty::Quantity(a), Ty::Quantity(b)) => a == b,
        (Ty::Sized(a), Ty::Sized(b)) => a == b,
        (Ty::Pointer(a), Ty::Pointer(b)) => a == b,
        (Ty::CPointer(a), Ty::CPointer(b)) => a == b,
        (Ty::Register(a), Ty::Register(b)) => a == b,
        (Ty::Param(a), Ty::Param(b)) => a == b,
        (Ty::List(a), Ty::List(b))
        | (Ty::Optional(a), Ty::Optional(b))
        | (Ty::Owned(a), Ty::Owned(b))
        | (Ty::Borrowed(a), Ty::Borrowed(b)) => same(a, b),
        (Ty::Dict(a_key, a_value), Ty::Dict(b_key, b_value)) => same(a_key, b_key) && same(a_value, b_value),
        (Ty::Fixed(a, a_length), Ty::Fixed(b, b_length)) => a_length == b_length && same(a, b),
        (Ty::Instance(a), Ty::Instance(b)) => Rc::ptr_eq(a, b),
        (Ty::EnumValue(a), Ty::EnumValue(b)) => Rc::ptr_eq(a, b),
        (Ty::Error(a), Ty::Error(b)) => a == b,
        (Ty::Callable(a), Ty::Callable(b)) => a.raises == b.raises && same_shape(a, b),
        (Ty::Several(a), Ty::Several(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)),
        _ => false,
    }
}

/// Do two function types take and return the same, whatever they say
/// they raise?
fn same_shape(a: &CallableType, b: &CallableType) -> bool {
    a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(a, b)| same(a, b))
        && same(&a.returns, &b.returns)
}

/// May a function that raises `kind` go where a function type that says
/// `raises wanted` is wanted? It may if `except wanted` would catch it,
/// which is the test `except` makes (decision 56.4).
fn caught_by(kind: &str, wanted: &str) -> bool {
    wanted == "Error" || wanted == kind
}

/// The type of what C gives back, or of a value it names (44.10).
fn c_given(class: &crate::bridge_c::Class) -> Ty {
    use crate::bridge_c::Class;
    match class {
        Class::Nothing => Ty::None,
        Class::Whole => Ty::Int,
        Class::U64 => Ty::U64,
        Class::Float => Ty::Float,
        Class::Bool => Ty::Bool,
        // NULL is `none`.
        Class::Text { .. } => Ty::Optional(Box::new(Ty::Str)),
        Class::Pointer { to, .. } => Ty::Optional(Box::new(Ty::CPointer(to.as_str().into()))),
    }
}

/// May a value of type `from` go where a `to` is wanted?
fn fits(from: &Ty, to: &Ty) -> bool {
    if from.is_loose() || to.is_loose() || same(from, to) {
        return true;
    }
    match (from, to) {
        // Lending. A parameter that looks takes a look at an owned value, or
        // a shared value, which anyone may look at. It does not take an
        // owned value that has no owner yet: nobody would ever free it.
        (Ty::Borrowed(from), Ty::Borrowed(to)) => fits(from, to),
        (from, Ty::Borrowed(to)) => !from.is_owned() && fits(from, to),
        // A look that may be `none` goes where such a look is wanted.
        (Ty::Optional(from), Ty::Optional(to)) if from.is_look() || to.is_look() => fits(from, to),
        (Ty::Borrowed(_), Ty::Optional(to)) if to.is_look() => fits(from, to),
        (Ty::Borrowed(_), _) => false,
        (from, _) if from.is_look() => false,
        (Ty::Owned(from), Ty::Owned(to)) => fits(from, to),
        // A `Circle or none` goes where a `Shape or none` is wanted.
        (Ty::Optional(from), Ty::Optional(to)) if seen_as_interface(from, to) => fits(from, to),
        // A pointer of C that may be NULL, where another may be.
        (Ty::Optional(from), Ty::Optional(to)) if matches!(**from, Ty::CPointer(_)) => fits(from, to),
        // `none` and a plain T both fit `T or none`.
        (Ty::None, Ty::Optional(_)) => true,
        (from, Ty::Optional(inner)) => fits(from, inner),
        // A `list[u8]` is not a `list[int]`: whoever holds it as a list of
        // ints could append 300, and the first holder would then find a
        // u8 that is not one.
        (Ty::List(from), Ty::List(to)) => !sized_into_int(from, to) && !seen_as_interface(from, to) && fits(from, to),
        // A dict goes where a dict with the same keys and the same values
        // is wanted. `[:]` holds nothing yet, and goes where any dict is.
        (Ty::Dict(from_key, from_value), Ty::Dict(to_key, to_value)) => {
            let agree = |from: &Ty, to: &Ty| from.is_loose() || to.is_loose() || same(from, to);
            agree(from_key, to_key) && agree(from_value, to_value)
        }
        // Every u8 is an int, so a sized number goes where an int is
        // wanted and nothing has to be written. The other way round needs
        // a conversion, because not every int is a u8.
        (Ty::Sized(_), Ty::Int) => true,
        // Every f32 is a float, so an f32 goes where a float is wanted
        // (decision 41.3). A float going where an f32 is wanted needs
        // `f32( )`, since most floats are not one.
        (Ty::F32, Ty::Float) => true,
        // A `void *` of C goes where any pointer of C does, and any
        // goes where a `void *` does, as in C.
        (Ty::CPointer(from), Ty::CPointer(to)) => &**from == "void" || &**to == "void",
        // Any error fits the catch-all `Error`.
        (Ty::Error(_), Ty::Error(to)) => &**to == "Error",
        // A function type into another of the same shape. What each says
        // it raises is compared by `Checker::arrives`, which knows the
        // build: an ordinary build does not hold it against a type that
        // says nothing (decision 56.4).
        (Ty::Callable(from), Ty::Callable(to)) => same_shape(from, to),
        // A function going where a function type is wanted is compared in
        // detail by `Checker::expect`, which can look inside it.
        (
            Ty::Function(_) | Ty::BoundMethod(_) | Ty::Builtin(_) | Ty::BuiltinMethod(..),
            Ty::Callable(_),
        ) => true,
        // A function written where a value goes, which has one owner
        // where everything has.
        (Ty::Owned(from), Ty::Callable(_)) => matches!(**from, Ty::Function(_)),
        // `class Circle is Shape`: a Circle goes where a Shape is wanted.
        (Ty::Instance(from), Ty::Instance(to)) => from.is.borrow().iter().any(|interface| Rc::ptr_eq(interface, to)),
        // Several values go where as many are wanted, each where its own
        // is (decision 57).
        (Ty::Several(from), Ty::Several(to)) => from.len() == to.len() && from.iter().zip(to).all(|(from, to)| fits(from, to)),
        _ => false,
    }
}

/// Would this take an object of a class for one of an interface the
/// class is, somewhere inside the type? A Circle goes where a Shape is
/// wanted. A `list[Circle]` does not go where a `list[Shape]` is: whoever
/// holds it as a list of shapes could put a Square in it.
fn seen_as_interface(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        (Ty::Instance(from), Ty::Instance(to)) => !Rc::ptr_eq(from, to),
        (Ty::List(from), Ty::List(to))
        | (Ty::Optional(from), Ty::Optional(to))
        | (Ty::Owned(from), Ty::Owned(to))
        | (Ty::Borrowed(from), Ty::Borrowed(to)) => seen_as_interface(from, to),
        (from, Ty::Optional(to) | Ty::Borrowed(to)) => seen_as_interface(from, to),
        _ => false,
    }
}

/// Is `class` a class that says it is the interface `interface`?
fn is_one(class: &Ty, interface: &Ty) -> bool {
    match (class, interface) {
        (Ty::Instance(class), Ty::Instance(interface)) => class.is.borrow().iter().any(|is| Rc::ptr_eq(is, interface)),
        _ => false,
    }
}

/// Is this an object seen as an interface, or one that may be `none`?
fn of_an_interface(ty: &Ty) -> bool {
    match ty {
        Ty::Instance(class) => class.interface,
        Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner) => of_an_interface(inner),
        _ => false,
    }
}

/// Does a list of `ty` hold its items another way than the items say by
/// themselves? A `Circle` kept as a `Shape`, a `1` kept as an `int or
/// none`. A list written out for such a place is built item by item, as
/// a list of what the place wants.
fn asks_more(ty: &Ty) -> bool {
    match ty {
        Ty::Optional(_) => true,
        Ty::List(item) | Ty::Dict(_, item) => asks_more(item),
        Ty::Owned(inner) | Ty::Borrowed(inner) => asks_more(inner),
        ty => of_an_interface(ty),
    }
}

/// A function as its first line is written:
/// `function scale(self, by: float) -> float`.
fn first_line(signature: &Signature) -> String {
    let params: Vec<String> = signature
        .params
        .iter()
        .enumerate()
        .map(|(index, slot)| match (index, slot.name.as_str()) {
            (0, "self") => "self".to_string(),
            _ => format!("{}: {}", slot.name, as_parameter(&slot.ty)),
        })
        .collect();
    let mut line = format!("function {}({})", signature.name, params.join(", "));
    if !matches!(signature.returns, Ty::None) {
        line += &format!(" -> {}", signature.returns);
    }
    line
}

/// Is this a sized number (perhaps inside lists) going where the same
/// shape of `int` is wanted?
fn sized_into_int(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        (Ty::Sized(_), Ty::Int) | (Ty::F32, Ty::Float) => true,
        (Ty::List(from), Ty::List(to)) | (Ty::Optional(from), Ty::Optional(to)) => sized_into_int(from, to),
        (from, Ty::Optional(to)) => sized_into_int(from, to),
        _ => false,
    }
}

/// A whole number, or a list of them, as it is written in the program:
/// `7`, `-1`, `[0x10, 0x20]`. Its value is known while checking, so it
/// can go where a sized number is wanted without a conversion: `let b: u8
/// = 7` is checked here, once, and costs nothing when the program runs.
#[derive(Clone)]
enum Written {
    /// A whole number written out, and where the number itself starts.
    Number(i128, Span),
    /// A float written out, and where the number itself starts. It can
    /// be an f32 where one is wanted (decision 41.4).
    Real(f64, Span),
    List(Vec<Written>),
    /// `[0] * 64`: a list written out, and how many times it is repeated
    /// when that is written as a number. `place` is the list written out.
    Repeat { list: Box<Written>, times: Option<i128>, place: usize },
}

/// A default as a message shows it: `-0.5` and `-5 ms`, and `2 * 3`
/// with no brackets around the whole.
fn shown(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Unary { op: UnaryOp::Neg, operand } => format!("-{}", shown(operand)),
        ExprKind::Unit { value, unit } => format!("{} {unit}", shown(value)),
        ExprKind::Unary { .. } | ExprKind::Binary { .. } | ExprKind::Compare { .. } => {
            let text = crate::printer::expr(expr);
            match text.strip_prefix('(').and_then(|text| text.strip_suffix(')')) {
                Some(inner) => inner.to_string(),
                None => text,
            }
        }
        _ => crate::printer::expr(expr),
    }
}

/// A default of a function of an interface. It is a plain value, so
/// that "the class writes the same one" has one meaning: the same value,
/// however it is spelled (decision 58.20).
#[derive(Debug, Clone, PartialEq)]
enum Plain {
    Number(i128),
    Real(f64),
    Text(String),
    Truth(bool),
    None,
    /// `5 ms`: the number and its unit.
    Quantity(f64, String),
    /// A case of an enum that carries nothing, by its name.
    Case(String),
}

impl Plain {
    /// `1_000` and `1000` are the same value, and so are `5 ms` and
    /// `5000 us`.
    fn same(&self, other: &Plain) -> bool {
        match (self, other) {
            (Plain::Quantity(x, from), Plain::Quantity(y, to)) => {
                (from == to && x == y) || crate::units::common(from, to).is_some_and(|(_, a, b)| a.apply(*x) == b.apply(*y))
            }
            _ => self == other,
        }
    }
}

fn written(expr: &Expr) -> Option<Written> {
    match &expr.kind {
        ExprKind::Int(n) => Some(Written::Number(*n as i128, expr.span)),
        ExprKind::Float(x) => Some(Written::Real(*x, expr.span)),
        ExprKind::Unary { op: UnaryOp::Neg, operand } => match written(operand)? {
            Written::Number(n, at) => Some(Written::Number(-n, at)),
            Written::Real(x, at) => Some(Written::Real(-x, at)),
            Written::List(_) | Written::Repeat { .. } => None,
        },
        ExprKind::Binary { op: BinaryOp::Mul, left, right } => {
            let list = written(left).filter(|list| matches!(list, Written::List(_)))?;
            let times = match &right.kind {
                ExprKind::Int(n) => Some(*n as i128),
                _ => None,
            };
            Some(Written::Repeat { list: Box::new(list), times, place: &**left as *const Expr as usize })
        }
        ExprKind::List(items) if !items.is_empty() => {
            items.iter().map(written).collect::<Option<Vec<_>>>().map(Written::List)
        }
        _ => None,
    }
}

/// Does what is written fit where a `to` is wanted? `None` if `to` asks
/// for no sized number here, and the ordinary rules decide. Otherwise
/// yes, or the message for the first number that is too big.
fn written_fits(written: &Written, to: &Ty) -> Option<Result<(), String>> {
    match (written, to) {
        (Written::Number(n, _), Ty::Sized(width)) => {
            let fits = i64::try_from(*n).is_ok_and(|n| width.holds(n));
            Some(if fits { Ok(()) } else { Err(width.does_not_fit(n)) })
        }
        (Written::Number(n, _), Ty::U64) => Some(u64_holds(*n)),
        (Written::Real(x, _), Ty::F32) => Some(f32_holds(*x)),
        (_, Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner)) => written_fits(written, inner),
        (Written::Repeat { list, .. }, _) => written_fits(list, to),
        (Written::List(items), Ty::List(item) | Ty::Fixed(item, _)) => {
            for written in items {
                if let Err(message) = written_fits(written, item)? {
                    return Some(Err(message));
                }
            }
            Some(Ok(()))
        }
        _ => None,
    }
}

/// Can a float written out be an f32? Any can, rounded to the nearest
/// f32, but one too big for 32 bits would be infinity.
fn f32_holds(x: f64) -> Result<(), String> {
    if x.is_finite() && (x as f32).is_infinite() {
        return Err(crate::value::too_big_for_f32(x));
    }
    Ok(())
}

/// Can a whole number written out be a u64 (decision 42.5)?
fn u64_holds(n: i128) -> Result<(), String> {
    if (0..=u64::MAX as i128).contains(&n) {
        return Ok(());
    }
    Err(format!("{n} does not fit in a u64 (0 to {})", u64::MAX))
}

/// Where each whole number written out begins that becomes a u64 where a
/// `to` is wanted: `7`, or the items of `[1, 2]` for a `list[u64]`.
fn numbers_into_u64(written: &Written, to: &Ty, out: &mut Vec<Span>) {
    match (written, to) {
        (Written::Number(_, at), Ty::U64) => out.push(*at),
        (_, Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner)) => numbers_into_u64(written, inner, out),
        (Written::Repeat { list, .. }, _) => numbers_into_u64(list, to, out),
        (Written::List(items), Ty::List(item) | Ty::Fixed(item, _)) => {
            items.iter().for_each(|written| numbers_into_u64(written, item, out))
        }
        _ => {}
    }
}

/// `u8.max`, `int.min`, `u64.max`: the number, written as a value of
/// that type, which `checked` puts in its place (decision 42.7).
fn limit_of(type_name: &str, name: &str, span: Span) -> Option<Expr> {
    let number = |n: u64| Expr { kind: ExprKind::Int(n), span };
    let negative = |n: u64| Expr { kind: ExprKind::Unary { op: UnaryOp::Neg, operand: Box::new(number(n)) }, span };
    let call = |callee: &str, value: Expr| Expr {
        kind: ExprKind::Call {
            callee: Box::new(Expr { kind: ExprKind::Name(callee.to_string()), span }),
            args: vec![ast::Arg { name: None, value }],
        },
        span,
    };
    let max = name == "max";
    if !max && name != "min" {
        return None;
    }
    Some(match type_name {
        "int" if max => number(i64::MAX as u64),
        // -9223372036854775808 is past the largest int written out, so
        // it is written as the int it is: the largest, negated, less one.
        "int" => Expr {
            kind: ExprKind::Binary { op: BinaryOp::Sub, left: Box::new(negative(i64::MAX as u64)), right: Box::new(number(1)) },
            span,
        },
        "u64" if max => call("u64..of", negative(1)),
        "u64" => call("u64..of", number(0)),
        _ => {
            let width = ints::width(type_name)?;
            let n = if max { width.max() } else { width.min() };
            let value = if n < 0 { negative(n.unsigned_abs()) } else { number(n as u64) };
            call(width.name, value)
        }
    })
}

/// Where each float written out begins that becomes an f32 where a `to`
/// is wanted: `0.5`, or the items of `[0.5, 1.5]` for a `list[f32]`.
fn reals_into_f32(written: &Written, to: &Ty, out: &mut Vec<Span>) {
    match (written, to) {
        (Written::Real(_, at), Ty::F32) => out.push(*at),
        (_, Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner)) => reals_into_f32(written, inner, out),
        (Written::Repeat { list, .. }, _) => reals_into_f32(list, to, out),
        (Written::List(items), Ty::List(item) | Ty::Fixed(item, _)) => {
            items.iter().for_each(|written| reals_into_f32(written, item, out))
        }
        _ => {}
    }
}

/// Does a value of type `from`, going where a `to` is wanted, go as an
/// f32 where a float is wanted? Then it is made a float on the way.
fn widens_f32(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        (Ty::F32, Ty::Float) => true,
        (Ty::F32, Ty::Optional(to) | Ty::Borrowed(to)) => widens_f32(from, to),
        _ => false,
    }
}

/// The first call in the place an assignment names, if it has one:
/// `next()` in `xs[next()]`, `pick()` in `pick().count`.
/// `first`, `second`: which of several values, for a message.
fn ordinal(at: usize) -> String {
    match at {
        0 => "first".to_string(),
        1 => "second".to_string(),
        2 => "third".to_string(),
        3 => "fourth".to_string(),
        _ => format!("number {}", at + 1),
    }
}

/// `a, b, c`: as many names as there are values, for a message.
fn letters(count: usize) -> String {
    (0..count).map(|at| ((b'a' + (at % 26) as u8) as char).to_string()).collect::<Vec<_>>().join(", ")
}

/// The refusal of a resource among several values (59.1).
const RESOURCE_SEVERAL: &str = "a resource cannot be one of several values: its owner, which releases it, is a `let` \
     of its own. Give it back alone, and receive it alone: `let held = ...`";

/// The message for a call of several values used as one (decision 57,
/// rule 3): "`divide` gives 2 values. Receive each: ...".
fn several_not_one(call: &Expr, count: usize) -> String {
    let name = match &call.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Name(name) => Some(name.clone()),
            ExprKind::Field { name, .. } => Some(name.clone()),
            _ => None,
        },
        _ => None,
    };
    let (who, called) = match &name {
        Some(name) => (format!("`{name}`"), name.as_str()),
        None => ("this call".to_string(), "f"),
    };
    format!(
        "{who} gives {count} values. Receive each: `let {} = {called}(...)`. To keep them together, declare a class",
        letters(count)
    )
}

fn call_in(place: &Expr) -> Option<&Expr> {
    match &place.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_) | ExprKind::Inline { .. }
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Name(_)
        | ExprKind::Give(_) => None,
        ExprKind::Field { object, .. } | ExprKind::Unit { value: object, .. } | ExprKind::Unary { operand: object, .. } => {
            call_in(object)
        }
        ExprKind::Index { object: left, index: right } | ExprKind::Binary { left, right, .. } => {
            call_in(left).or_else(|| call_in(right))
        }
        ExprKind::Compare { first, rest } => call_in(first).or_else(|| rest.iter().find_map(|(_, operand)| call_in(operand))),
        ExprKind::List(items) | ExprKind::Several(items) => items.iter().find_map(call_in),
        ExprKind::FormatStr(parts) => parts.iter().find_map(|part| match part {
            FormatPart::Value { value, .. } => call_in(value),
            FormatPart::Text(_) => None,
        }),
        // A call, and the few things nobody writes in a place. `take`
        // changes what it reads, as a call may.
        ExprKind::Call { .. } | ExprKind::Dict(_) | ExprKind::ListComp { .. } | ExprKind::Function(_) | ExprKind::Take(_) => {
            Some(place)
        }
    }
}

/// The type `from` with its ints read as the sized numbers `to` wants:
/// what `[1, 2]` is, written where a `list[u8]` goes.
fn sized_as(from: &Ty, to: &Ty) -> Ty {
    match (from, to) {
        (Ty::Int, Ty::Sized(_)) => to.clone(),
        (Ty::Float, Ty::F32) => Ty::F32,
        (Ty::Int, Ty::U64) => Ty::U64,
        (Ty::List(from), Ty::List(to) | Ty::Fixed(to, _)) => Ty::list(sized_as(from, to)),
        (Ty::Owned(from), Ty::Owned(to)) => Ty::Owned(Box::new(sized_as(from, to))),
        (from, Ty::Optional(to) | Ty::Borrowed(to) | Ty::Owned(to)) => sized_as(from, to),
        _ => from.clone(),
    }
}

/// The one type that covers both `a` and `b`: used for the items of a list
/// and the different `return`s of a function.
fn join(a: &Ty, b: &Ty) -> Ty {
    if same(a, b) {
        return a.clone();
    }
    match (a, b) {
        (Ty::Invalid, _) | (_, Ty::Invalid) => Ty::Invalid,
        (Ty::None, other) | (other, Ty::None) => match other {
            Ty::Optional(_) => other.clone(),
            _ => Ty::Optional(Box::new(other.clone())),
        },
        (Ty::Optional(inner), other) | (other, Ty::Optional(inner)) => {
            Ty::Optional(Box::new(join(inner, other)))
        }
        (Ty::List(a), Ty::List(b)) => Ty::list(join(a, b)),
        (Ty::Owned(a), Ty::Owned(b)) => Ty::Owned(Box::new(join(a, b))),
        (Ty::Several(a), Ty::Several(b)) if a.len() == b.len() => {
            Ty::Several(a.iter().zip(b).map(|(a, b)| join(a, b)).collect())
        }
        // A u8 beside an int, or beside an i16: what covers both is an int.
        (Ty::Sized(_), Ty::Int | Ty::Sized(_)) | (Ty::Int, Ty::Sized(_)) => Ty::Int,
        // An f32 beside a float: every f32 is a float.
        (Ty::F32, Ty::Float) | (Ty::Float, Ty::F32) => Ty::Float,
        // Unrelated types: only known when the program runs.
        _ => Ty::Dynamic,
    }
}

/// The names in `above` that `env` has not declared yet.
fn not_yet(above: &[String], env: &Env) -> Vec<String> {
    above.iter().filter(|name| env.get_here(name).is_none()).cloned().collect()
}

/// Every name an expression mentions, for `ClassInfo::defaults`. It goes
/// by name alone and does not look inside a function written as a value.
fn names_in(expr: &Expr, names: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
        ExprKind::Function(_) => {}
        ExprKind::Name(name) | ExprKind::Give(name) => names.push(name.clone()),
        ExprKind::FormatStr(parts) => {
            for part in parts {
                if let FormatPart::Value { value, .. } = part {
                    names_in(value, names);
                }
            }
        }
        ExprKind::Unit { value, .. } => names_in(value, names),
        ExprKind::List(items) | ExprKind::Several(items) => items.iter().for_each(|item| names_in(item, names)),
        ExprKind::Dict(entries) => {
            for (key, value) in entries {
                names_in(key, names);
                names_in(value, names);
            }
        }
        ExprKind::ListComp { element, iterable, condition, .. } => {
            names_in(element, names);
            names_in(iterable, names);
            if let Some(condition) = condition {
                names_in(condition, names);
            }
        }
        ExprKind::Unary { operand, .. } => names_in(operand, names),
        ExprKind::Binary { left, right, .. } => {
            names_in(left, names);
            names_in(right, names);
        }
        ExprKind::Compare { first, rest } => {
            names_in(first, names);
            rest.iter().for_each(|(_, operand)| names_in(operand, names));
        }
        ExprKind::Call { callee, args } => {
            names_in(callee, names);
            args.iter().for_each(|arg| names_in(&arg.value, names));
        }
        ExprKind::Field { object, .. } | ExprKind::Take(object) => names_in(object, names),
        ExprKind::Index { object, index } => {
            names_in(object, names);
            names_in(index, names);
        }
    }
}

// --------------------------------------------------------------------- Scopes

/// The checker's scopes mirror the interpreter's, holding types in place of values.
struct Scope {
    variables: RefCell<HashMap<String, Variable>>,
    parent: Option<Env>,
    /// Which function body this scope is part of. An owned value may only
    /// be used by the function whose block it lives in.
    function: usize,
    /// Is this the top of the file, outside every function and block?
    /// A variable declared there is the program's, and every function
    /// can reach it.
    top: bool,
}

/// A number nothing else has: for telling variables, and function bodies,
/// apart. Later ones are larger, which says what was declared before what.
fn fresh_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

type Env = Rc<Scope>;

#[derive(Clone)]
struct Variable {
    ty: Ty,
    mutable: bool,
    /// Set for a bare enum case such as `Idle`: which case it is.
    case: Option<usize>,
    /// Set when `ty` is narrower than the type the variable was declared
    /// with, after a check such as `is not none`. This is the declared type.
    declared: Option<Ty>,
    /// A scope inside the one that declares a variable can hold its own
    /// view of it, to record what is known about it in there. This marks
    /// such a view, as opposed to the declaration itself.
    view: bool,
    /// Which variable this is, whatever it is called. See `fresh_id`.
    id: usize,
    /// The function body it was declared in.
    function: usize,
    /// Was it declared at the top of the file? See `Scope::top`.
    top: bool,
}

impl Scope {
    fn child(parent: &Env) -> Env {
        Rc::new(Scope {
            variables: RefCell::new(HashMap::new()),
            parent: Some(parent.clone()),
            function: parent.function,
            top: false,
        })
    }

    /// The scope of the statements at the top of the file.
    fn program(parent: &Env) -> Env {
        Rc::new(Scope {
            variables: RefCell::new(HashMap::new()),
            parent: Some(parent.clone()),
            function: parent.function,
            top: true,
        })
    }

    /// The outermost scope of a function's body.
    fn body(parent: &Env) -> Env {
        Rc::new(Scope {
            variables: RefCell::new(HashMap::new()),
            parent: Some(parent.clone()),
            function: fresh_id(),
            top: false,
        })
    }

    fn declare(&self, name: &str, ty: Ty, mutable: bool) -> bool {
        let mut variables = self.variables.borrow_mut();
        if variables.contains_key(name) {
            return false;
        }
        let variable =
            Variable { ty, mutable, case: None, declared: None, view: false, id: fresh_id(), function: self.function, top: self.top };
        variables.insert(name.to_string(), variable);
        true
    }

    fn get(&self, name: &str) -> Option<Variable> {
        if let Some(variable) = self.variables.borrow().get(name) {
            return Some(variable.clone());
        }
        self.parent.as_ref()?.get(name)
    }

    fn get_here(&self, name: &str) -> Option<Variable> {
        self.variables.borrow().get(name).cloned()
    }

    /// Record that, from here on in this scope, `name` is known to have a
    /// narrower type: for example `str` instead of `str or none` after a check.
    fn narrow(&self, name: &str, ty: Ty) {
        if let Some(mut variable) = self.get(name) {
            variable.view = self.get_here(name).map_or(true, |here| here.view);
            if variable.declared.is_none() {
                variable.declared = Some(variable.ty.clone());
            }
            variable.ty = ty;
            self.variables.borrow_mut().insert(name.to_string(), variable);
        }
    }

    /// Give this scope a view of `name` that knows nothing beyond its
    /// declared type, whatever the scopes around it have worked out.
    fn forget(&self, name: &str) {
        if let Some(mut variable) = self.get(name) {
            if let Some(declared) = variable.declared.take() {
                variable.ty = declared;
                variable.view = true;
                self.variables.borrow_mut().insert(name.to_string(), variable);
            }
        }
    }

    /// `name` has just been given a value that may be `none`. Drop what
    /// checks have said about it, in this scope and in every scope around
    /// it, out to the one that declares it.
    fn widen(&self, name: &str) {
        let mut view = true;
        if let Some(variable) = self.variables.borrow_mut().get_mut(name) {
            if let Some(declared) = variable.declared.take() {
                variable.ty = declared;
            }
            view = variable.view;
        }
        if let (true, Some(parent)) = (view, &self.parent) {
            parent.widen(name);
        }
    }
}

/// The outermost scope: the built-in functions and error types.
fn globals() -> Env {
    let env = Rc::new(Scope { variables: RefCell::new(HashMap::new()), parent: None, function: 0, top: false });
    for name in ["print", "len", "range", "int", "float", "str", "abs", "min", "max", "sum", "allocate", "free", "char", "input"] {
        env.declare(name, Ty::Builtin(name), false);
    }
    // The sized whole numbers: `u8(n)` converts, and `u8` is also a type.
    for width in ints::WIDTHS {
        env.declare(width.name, Ty::Builtin(width.name), false);
    }
    // `f32(x)` converts, and `f32` is also a type (decision 41).
    env.declare("f32", Ty::Builtin("f32"), false);
    // `u64(n)` converts, and `u64` is a type (decision 42). The names
    // with two dots are the ones `checked` writes for `u64.wrap` and a
    // u64 written out (42.5, 42.6); no program can write them.
    for name in ["u64", "u64..of", "u64..add", "u64..sub", "u64..mul", "u64..shl"] {
        env.declare(name, Ty::Builtin(name), false);
    }
    for kind in crate::builtins::ERROR_TYPES {
        env.declare(kind, Ty::ErrorType(Rc::from(*kind)), false);
    }
    env
}

/// The type of `module.name`, for the built-in modules.
fn module_member(module: &str, name: &str) -> Option<Ty> {
    Some(match (module, name) {
        ("math", "pi") => Ty::Float,
        ("math", "sqrt") => Ty::Builtin("sqrt"),
        ("math", "floor") => Ty::Builtin("floor"),
        ("math", "ceil") => Ty::Builtin("ceil"),
        ("math", "e") => Ty::Float,
        ("math", "round") => Ty::Builtin("round"),
        ("math", "sin") => Ty::Builtin("sin"),
        ("math", "cos") => Ty::Builtin("cos"),
        ("math", "tan") => Ty::Builtin("tan"),
        ("math", "asin") => Ty::Builtin("asin"),
        ("math", "acos") => Ty::Builtin("acos"),
        ("math", "atan") => Ty::Builtin("atan"),
        ("math", "atan2") => Ty::Builtin("atan2"),
        ("math", "exp") => Ty::Builtin("exp"),
        ("math", "log") => Ty::Builtin("log"),
        ("math", "log2") => Ty::Builtin("log2"),
        ("math", "log10") => Ty::Builtin("log10"),
        ("time", "sleep") => Ty::Builtin("sleep"),
        ("time", "now") => Ty::Builtin("now"),
        ("files", "read") => Ty::Builtin("files.read"),
        ("files", "write") => Ty::Builtin("files.write"),
        ("files", "read_bytes") => Ty::Builtin("files.read_bytes"),
        ("files", "write_bytes") => Ty::Builtin("files.write_bytes"),
        ("files", "append") => Ty::Builtin("files.append"),
        ("files", "exists") => Ty::Builtin("files.exists"),
        ("files", "remove") => Ty::Builtin("files.remove"),
        ("files", "list") => Ty::Builtin("files.list"),
        ("files", "make_folder") => Ty::Builtin("files.make_folder"),
        // What a `Reader` and a `Writer` of library/files.zeno are made
        // of, and no program can name (decision 59.6).
        ("files", "handle_open") => Ty::Builtin("files.handle_open"),
        ("files", "handle_line") => Ty::Builtin("files.handle_line"),
        ("files", "handle_bytes") => Ty::Builtin("files.handle_bytes"),
        ("files", "handle_write") => Ty::Builtin("files.handle_write"),
        ("files", "handle_write_bytes") => Ty::Builtin("files.handle_write_bytes"),
        ("files", "handle_close") => Ty::Builtin("files.handle_close"),
        ("program", "arguments") => Ty::Builtin("program.arguments"),
        ("program", "run") => Ty::Builtin("program.run"),
        ("program", "exit") => Ty::Builtin("program.exit"),
        ("program", "environment") => Ty::Builtin("program.environment"),
        ("program", "warn") => Ty::Builtin("program.warn"),
        ("python", "module") => Ty::Builtin("python.module"),
        ("python", "copy") => Ty::Builtin("python.copy"),
        ("node", "module") => Ty::Builtin("node.module"),
        ("node", "global") => Ty::Builtin("node.global"),
        ("node", "new") => Ty::Builtin("node.new"),
        ("node", "copy") => Ty::Builtin("python.copy"),
        ("java", "type") => Ty::Builtin("java.type"),
        ("java", "jar") => Ty::Builtin("java.jar"),
        ("java", "copy") => Ty::Builtin("python.copy"),
        ("ruby", "require") => Ty::Builtin("ruby.require"),
        ("ruby", "constant") => Ty::Builtin("ruby.constant"),
        ("ruby", "symbol") => Ty::Builtin("ruby.symbol"),
        ("ruby", "copy") => Ty::Builtin("python.copy"),
        ("php", "type") => Ty::Builtin("php.type"),
        ("php", "callable") => Ty::Builtin("php.callable"),
        ("php", "constant") => Ty::Builtin("php.constant"),
        ("php", "require") => Ty::Builtin("php.require"),
        ("php", "variable") => Ty::Builtin("php.variable"),
        ("php", "copy") => Ty::Builtin("python.copy"),
        ("lua", "require") => Ty::Builtin("lua.require"),
        ("lua", "global") => Ty::Builtin("lua.global"),
        ("lua", "copy") => Ty::Builtin("python.copy"),
        ("csharp", "type") => Ty::Builtin("csharp.type"),
        ("csharp", "assembly") => Ty::Builtin("csharp.assembly"),
        ("csharp", "copy") => Ty::Builtin("python.copy"),
        ("c", "library") => Ty::Builtin("c.library"),
        ("json", "parse") => Ty::Builtin("json.parse"),
        ("json", "write") => Ty::Builtin("json.write"),
        ("web", "element") => Ty::Builtin("web.element"),
        ("web", "elements") => Ty::Builtin("web.elements"),
        ("web", "create") => Ty::Builtin("web.create"),
        ("web", "after") => Ty::Builtin("web.after"),
        ("web", "on_key") => Ty::Builtin("web.on_key"),
        ("web", "every") => Ty::Builtin("web.every"),
        ("web", "save") => Ty::Builtin("web.save"),
        ("web", "load") => Ty::Builtin("web.load"),
        ("web", "forget") => Ty::Builtin("web.forget"),
        ("web", "fetch") => Ty::Builtin("web.fetch"),
        ("web", "post") => Ty::Builtin("web.post"),
        ("board", "led") => Ty::Pin,
        ("board", "pin") => Ty::Builtin("pin"),
        ("board", "register") => Ty::Builtin("register"),
        ("board", "input_pin") => Ty::Builtin("input_pin"),
        ("board", "pull_up") | ("board", "pull_down") => Ty::Pull,
        ("board", "serial") => Ty::Builtin("serial"),
        ("board", "analog_pin") => Ty::Builtin("analog_pin"),
        ("board", "pwm_pin") => Ty::Builtin("pwm_pin"),
        ("board", "ticker") => Ty::Builtin("ticker"),
        ("board", "rising") | ("board", "falling") | ("board", "either") => Ty::Edge,
        _ => return None,
    })
}

/// The message for `math.nothing`. A name of the part of a module that
/// is written in Zeno (`library.rs`) only gets as far as this when the
/// module was imported inside a function: `modules.rs` joins that part
/// to a file that imports the module at its top.
fn no_member(module: &str, name: &str) -> String {
    match crate::library::part(module).is_some_and(|part| part.names.contains(&name)) {
        true => format!(
            "`{module}.{name}` is written in Zeno, and is joined to a file that has `import {module}` at its top. \
             Move the `import` there from inside the function"
        ),
        false => format!("module `{module}` has no `{name}`"),
    }
}

// -------------------------------------------------------------------- Checker

impl Checker {
    /// Is this a `files.Reader`, or a look at one? A `for` over it is
    /// given its lines (decision 59.7).
    fn is_reader(&self, ty: &Ty) -> bool {
        let mut bare = ty;
        while let Ty::Owned(inner) | Ty::Borrowed(inner) = bare {
            bare = inner;
        }
        matches!(bare, Ty::Instance(class) if class.name == "files.Reader" && Some(class.file) == self.files_part)
    }

    /// `module_member`, but for the built-ins a `files.Reader` and a
    /// `files.Writer` are made of, which only library/files.zeno can
    /// call: to any other file they are names the module does not have.
    fn member(&self, module: &str, name: &str, span: Span) -> Option<Ty> {
        if module == "files" && name.starts_with("handle_") && Some(span.file()) != self.files_part {
            return None;
        }
        module_member(module, name)
    }
}

/// What the checker tracks for the function body it is currently inside.
/// The bottom frame stands for the top level of the file.
struct Frame {
    declared_return: Option<Ty>,
    /// The type of every `return` seen so far.
    returns: Vec<Ty>,
    /// How many loops we are inside, for `break` and `continue`.
    loops: usize,
    /// How many `unsafe { }` blocks we are inside. A function written
    /// inside one starts at none again: the word covers the lines
    /// between its braces, in the function that has them.
    unsafe_blocks: usize,
    /// For the body of a method: has it taken something out of what
    /// `self` holds? `None` in any other function.
    takes_self: Option<bool>,
}

/// One argument at a call, with where it is written.
struct Argument {
    name: Option<String>,
    ty: Ty,
    span: Span,
    /// The number, if the argument is one written out: `f(7)`.
    written: Option<Written>,
    /// Where the argument's expression is, to note it when it turns out
    /// to be a list of bytes written out (`f([1, 2])`, 36.2). 0 for none.
    place: usize,
}

struct Checker {
    /// Set just before the right of a `let a, b = ...` or what a
    /// `return` gives is looked at: the one expression that may give
    /// several values (decision 57). The next `expr` takes it down.
    several: bool,
    /// The calls refused because they give several values where one is
    /// wanted, so that the call they are an argument of does not also
    /// say that an argument is missing: `divide(divide(9, 2))`.
    not_one: HashSet<usize>,
    /// Set while the argument of `u64.wrap( )` is about to be looked
    /// at, and kept down through `+`, `-`, `*` and `<<`: those sums cut
    /// their answer to 64 bits (decision 42.6).
    wrap_mode: bool,
    /// Each whole number written out past the largest int. It is only
    /// right where a u64 is wanted, which is known once it is checked.
    big_numbers: Vec<(Span, u64)>,
    /// The uses of a name that mean a function: the place in
    /// `Types::used`, and the function. What a function takes and gives
    /// back may not be known yet where it is used, so it is written into
    /// the record when the whole program has been checked.
    functions_used: Vec<(usize, Rc<FunctionInfo>)>,
    /// What the function literal that was checked last looks at from
    /// outside itself. See `Capture`.
    captured: Vec<Capture>,
    /// The function written where a value goes that `take` is about to
    /// check: it is kept, and takes what it uses (decision 56.8).
    keeping: Option<usize>,
    /// The expressions `take` checks: a resource that a call gives may be
    /// kept there, and nowhere else (decision 59.1).
    receiving: HashSet<usize>,
    /// The interface `Resource` that comes with Zeno, if the program
    /// uses it (decision 59.1).
    resource: Option<Rc<ClassInfo>>,
    /// Is the program a page, which stays open after its last line?
    page: bool,
    /// The type parameters of generics, by variable id: the `T` of
    /// `Stack[T]` in its outline and in each copy. `take T` is read by
    /// them (decision 56.11).
    type_params: HashSet<usize>,
    /// The `d[key]` that look at a value a dict owns, for the message
    /// when one is kept (decision 56.12).
    dict_values: HashSet<usize>,
    /// The functions of interfaces that the function of some class that
    /// is the interface takes out for, with the name of such a class: what
    /// the rounds before this one found. A call through the interface is
    /// held to the guards of 5.15e when its function is here (58.21).
    taken: HashMap<(DeclId, String), String>,
    /// The same, as this round finds them. It only grows.
    taking: HashMap<(DeclId, String), String>,
    /// The declarations of the named functions whose bodies are being
    /// checked, the innermost last. See `Spot`.
    within: Vec<usize>,
    strict: bool,
    /// For each file of the program, whether it begins with the line
    /// `strict`. See `strict_at`.
    strict_files: Vec<bool>,
    /// The number of the file library/files.zeno, if it is joined to the
    /// program: the one file that may call `files.handle_open` and the
    /// other built-ins under a `Reader` and a `Writer` (decision 59.6).
    files_part: Option<usize>,
    /// Does every list and object have one owner, said or not? True for
    /// a build with no collector. See `Rules`.
    one_owner: bool,
    /// Do errors travel as values, as in a firmware build? See `Rules`.
    firmware: bool,
    /// False while types are still being worked out over several passes.
    last_round: bool,
    /// The types already reported as not having what a type parameter
    /// asks, by generic and line, so that one line says it once.
    unmet: HashSet<(usize, String, usize)>,
    /// Each list written out whose items are not all one type, with the
    /// types they are, as `[200 ms, 1 s]`: a generic called with one is
    /// told why it cannot tell what `T` is (58.8).
    mixed: HashMap<usize, Vec<Ty>>,
    /// What earlier passes worked out for untyped parameters and fields.
    known: HashMap<SlotId, Key>,
    /// The untyped parameters and fields that cannot be worked out.
    open: HashSet<SlotId>,
    /// What this pass has seen arrive at each untyped parameter and field.
    seen: HashMap<SlotId, Seen>,
    /// The line where each of them was first given a value: by this pass
    /// (`origins`), and by the pass before (`known_origins`).
    origins: HashMap<SlotId, usize>,
    known_origins: HashMap<SlotId, usize>,
    /// For each function body being checked, innermost last: its parameters
    /// whose types were worked out, with the type and the line it came from.
    worked_out: Vec<HashMap<String, (String, usize)>>,
    /// Which of those the statement being checked has mentioned so far.
    touched: Vec<String>,
    /// The variables that were declared with `const`, by `Variable::id`.
    /// Another `const` may be written from them.
    consts: std::collections::HashSet<usize>,
    /// Where a field that may be `none` was read (line and column), and
    /// the field's name, for `none_first`.
    maybe_fields: HashMap<(usize, usize), String>,
    /// How many items each list written out has, by where it is: `[a, b]`
    /// has two, and `[x] * 4` has four. A list of a fixed length is
    /// checked against it (decision 36.4).
    counted: HashMap<usize, i128>,
    /// This pass's classes and enums, to turn a `Key` back into a `Ty`.
    classes: HashMap<DeclId, Rc<ClassInfo>>,
    enums: HashMap<DeclId, Rc<EnumInfo>>,
    /// What this pass worked out, kept for the compiler.
    types: Types,
    errors: Vec<Error>,
    frames: Vec<Frame>,
    /// The function bodies being checked right now, outermost first.
    early: Vec<Early>,
    /// The errors that can be raised by the code checked so far, innermost
    /// `try` body or function body last. Each entry is an error kind and
    /// where it comes from.
    raised: Vec<Vec<(String, Span)>>,
    /// Which owned values have been given away, in the function body being
    /// checked. See the section "Owned values".
    owned: Ownership,
    /// The `let`s at the top of the file that some function looks at,
    /// and the line of the first look. Such a value stays where it is
    /// until the program ends (decision 5.19i).
    reached: HashMap<usize, usize>,
    /// The `let`s at the top of the file that are given away, and where.
    /// No function may look at one of these.
    given_top: HashMap<usize, usize>,
    /// Every name the top of the file declares with `let`, `var` or
    /// `const`, and the line. Only for the message when a name is used
    /// above its declaration.
    later: HashMap<String, usize>,
    /// Every generic function and class the pass has met.
    generics: Vec<Rc<Generic>>,
    /// The copies made so far, by the generic and the types: `first` and
    /// `int`. Asking twice gives the same copy.
    copies: HashMap<(usize, String), Made>,
    /// The generics that something in the program uses.
    used: HashSet<usize>,
    /// Copies of classes whose bodies are still to be checked. A copy can
    /// be asked for in the middle of anything, so its body waits here
    /// until the checker is between statements again.
    pending: Vec<(Arc<ast::Class>, Rc<ClassInfo>)>,
    /// A call that is about to be checked, and the type wanted where its
    /// result goes: in `let s: Stack[int] = Stack()`, the call `Stack()`
    /// and `Stack[int]`. A generic that cannot tell its types from its
    /// arguments looks here.
    expected: Option<(usize, Ty)>,
    /// The same for a bare case of a generic enum, `Nothing`, which says
    /// nothing about its types by itself (decision 58.1).
    expected_case: Option<(usize, Ty)>,
    /// The values each case of a generic enum carries, with the type
    /// parameters left in: by the generic and the place of the case.
    case_outlines: HashMap<(usize, usize), Rc<Outline>>,
    /// The same for a list that is written out where the place says more
    /// about the items than they say themselves: the list, and what each
    /// item is kept as. `[Circle(1.0), Square(2.0)]` for a `list[Shape]`.
    expected_items: Option<(usize, Ty)>,
    /// For each function body being checked, innermost last: if it is a
    /// copy of a generic, the words that say which.
    copy_notes: Vec<Option<String>>,
}

/// What the checker tracks about owned values in one function body.
#[derive(Default)]
struct Ownership {
    /// The owners that hold nothing at this point in the code, because
    /// their value was given away: each one's name, and where it went.
    moved: Moved,
    /// The owned and looked-at variables the statement being checked has
    /// looked at so far. None of them may be given away by the same
    /// statement: the look might still be in use.
    looking: Vec<usize>,
    /// For each loop around this point, what had been given away at each
    /// `break` and at each `continue` in it.
    loops: Vec<(Moved, Moved)>,
    /// For each `try` around this point, everything its body has given
    /// away so far. An error can leave the body after any of them.
    tries: Vec<Moved>,
    /// The variables of `for` loops that are a look at an item which has
    /// one owner, the list being walked.
    items: HashSet<usize>,
    /// The owners that the statement being checked has taken an item
    /// out of. It may not use them for anything else: see `takes_out`.
    emptied: Vec<usize>,
    /// For each `for` loop around this point that walks a list whose
    /// items have one owner each: the owner the list is reached through,
    /// and the line of the `for`. The loop is looking at the items.
    walked: Vec<(usize, usize, Looking)>,
    /// The names a `case` gave to a value that has one owner, or to a
    /// part of one. Each is a look.
    parts: HashSet<usize>,
}

type Moved = HashMap<usize, (String, Span)>;

impl Checker {
    fn new(strict: bool, last_round: bool, known: HashMap<SlotId, Key>, open: HashSet<SlotId>) -> Checker {
        Checker {
            wrap_mode: false,
            several: false,
            not_one: HashSet::new(),
            big_numbers: Vec::new(),
            strict,
            strict_files: Vec::new(),
            files_part: None,
            one_owner: false,
            firmware: false,
            last_round,
            unmet: HashSet::new(),
            mixed: HashMap::new(),
            known,
            open,
            seen: HashMap::new(),
            origins: HashMap::new(),
            known_origins: HashMap::new(),
            worked_out: Vec::new(),
            touched: Vec::new(),
            consts: std::collections::HashSet::new(),
            maybe_fields: HashMap::new(),
            counted: HashMap::new(),
            classes: HashMap::new(),
            enums: HashMap::new(),
            types: Types::default(),
            errors: Vec::new(),
            frames: vec![Frame { declared_return: None, returns: Vec::new(), loops: 0, unsafe_blocks: 0, takes_self: None }],
            raised: vec![Vec::new()],
            owned: Ownership::default(),
            reached: HashMap::new(),
            given_top: HashMap::new(),
            later: HashMap::new(),
            early: Vec::new(),
            generics: Vec::new(),
            copies: HashMap::new(),
            expected_case: None,
            case_outlines: HashMap::new(),
            used: HashSet::new(),
            pending: Vec::new(),
            expected: None,
            expected_items: None,
            copy_notes: Vec::new(),
            functions_used: Vec::new(),
            captured: Vec::new(),
            keeping: None,
            receiving: HashSet::new(),
            resource: None,
            page: false,
            type_params: HashSet::new(),
            dict_values: HashSet::new(),
            taken: HashMap::new(),
            taking: HashMap::new(),
            within: Vec::new(),
        }
    }

    /// True if every type has to be known at this place before the
    /// program runs: the whole program is checked that way, or the file
    /// this place is in begins with the line `strict`.
    fn strict_at(&self, span: Span) -> bool {
        self.strict || self.strict_files.get(span.file()).copied().unwrap_or(false)
    }

    fn run(&mut self, program: &Program) {
        let env = Scope::program(&globals());
        for statement in ast::flat(&program.statements) {
            // A page stays open after the program's last line (59.1).
            if matches!(&statement.kind, StmtKind::Import { module, .. } if module.len() == 1 && module[0] == "web") {
                self.page = true;
            }
            if let StmtKind::Let { name, .. } | StmtKind::Const { name, .. } = &statement.kind {
                self.later.entry(name.clone()).or_insert(statement.span.line);
            }
            if let StmtKind::Receive { places, .. } = &statement.kind {
                for name in places.iter().filter_map(Place::declared) {
                    self.later.entry(name.clone()).or_insert(statement.span.line);
                }
            }
        }
        self.statements(&program.statements, &env);
        self.finish_generics();
        // Say what each function that was used takes and gives back, as
        // its first line says it: `function(n: int) -> int`.
        for (place, function) in std::mem::take(&mut self.functions_used) {
            if let State::Checked(signature) = &*function.state.borrow() {
                let params: Vec<String> =
                    signature.params.iter().map(|slot| format!("{}: {}", slot.name, as_parameter(&slot.ty))).collect();
                let mut shown = format!("function({})", params.join(", "));
                if !matches!(signature.returns, Ty::None) {
                    shown += &format!(" -> {}", signature.returns);
                }
                self.types.used[place].2 = shown;
            }
        }
    }

    fn error(&mut self, message: impl Into<String>, span: Span) -> Ty {
        let mut error = Error::new(message, span);
        // If this statement used a parameter whose type was worked out from
        // a call, the real mistake may be that call. Say where it is.
        if let Some(params) = self.worked_out.last() {
            let mut notes = Vec::new();
            for name in &self.touched {
                if let Some((ty, line)) = params.get(name) {
                    let note = format!(
                        "`{name}` was worked out to be {ty} from line {line}. \
                         Give `{name}` a type to have each call checked against it instead"
                    );
                    if !notes.contains(&note) {
                        notes.push(note);
                    }
                }
            }
            if !notes.is_empty() {
                error.note = Some(notes.join("\n"));
            }
        }
        // An error inside a copy of a generic depends on the types the
        // copy was made for. Say which copy this is.
        if let Some(Some(copy)) = self.copy_notes.last() {
            error.note = Some(match error.note.take() {
                Some(note) => format!("{note}\n{copy}"),
                None => copy.clone(),
            });
        }
        self.errors.push(error);
        Ty::Invalid
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("there is always a top-level frame")
    }

    /// `what` is one of the few things that nothing can check: reading
    /// or writing through a pointer, freeing one, touching a register.
    /// It has to be written inside `unsafe { }`.
    fn needs_unsafe(&mut self, what: &str, span: Span) {
        if self.frame().unsafe_blocks == 0 {
            self.error(
                format!(
                    "{what} goes inside `unsafe {{ }}`. Zeno cannot check that it is right, \
                     so the block marks the lines where a mistake can damage memory"
                ),
                span,
            );
        }
    }

    fn can_raise(&mut self, kind: &str, span: Span) {
        self.raised.last_mut().expect("there is always a top-level list").push((kind.to_string(), span));
    }

    /// What to say to somebody who used a value that may be `none` as
    /// if it were there. `usual` is the advice for a variable: test it.
    ///
    /// For a field that advice is no help, and reads as if the checker
    /// had not seen the test two lines up. A field is never known to be
    /// there: it belongs to an object that any call may reach and
    /// change. So the advice is to copy it into a variable, which
    /// nothing else can change, and test that.
    fn none_first(&self, span: Span, usual: &str) -> String {
        match self.maybe_fields.get(&(span.line, span.col)) {
            Some(name) => format!(
                "`{name}` is a field, and a field can be given a new value between a test and its use, \
                 so a test of it is not remembered. Copy it into a variable with `let`, and test that for `none`"
            ),
            None => usual.to_string(),
        }
    }

    fn declare(&mut self, env: &Env, name: &str, ty: Ty, mutable: bool, span: Span) {
        if !env.declare(name, ty, mutable) {
            self.error(format!("`{name}` is already declared in this block"), span);
        } else {
            self.placed(env, name, span);
        }
    }

    /// Note where the name that `scope` has just declared was declared.
    fn placed(&mut self, scope: &Env, name: &str, span: Span) {
        if let Some(variable) = scope.get_here(name) {
            self.types.placed.insert(variable.id, span);
        }
    }

    /// Note that `name` is used at `span`, and means `variable`.
    fn used(&mut self, name: &str, variable: &Variable, span: Span) {
        if let Ty::Function(function) = &variable.ty {
            self.functions_used.push((self.types.used.len(), function.clone()));
        }
        self.types.used.push((span, name.to_string(), variable.ty.to_string(), variable.id));
    }

    /// Note that a field or a method is read here, and what its type is,
    /// for an editor that asks (`Types::uses`). Where it was declared is
    /// not noted: a field has no place of its own in the tree yet.
    fn field_used(&mut self, name: &str, ty: &Ty, at: Span) {
        if !matches!(ty, Ty::Invalid) {
            self.types.used.push((at, name.to_string(), ty.to_string(), usize::MAX));
        }
    }

    /// Report an error unless a `from` can go where a `to` is wanted.
    fn expect(&mut self, from: &Ty, to: &Ty, what: &str, span: Span) {
        if !fits(from, to) {
            let mut message = format!("{what} should be {to}, but this is {from}");
            match (from, to) {
                (from, Ty::Borrowed(_)) if from.is_owned() => {
                    message = format!(
                        "this {from} has no owner yet, so it cannot be lent. \
                         Give it a name with `let` first, and pass the name"
                    )
                }
                // An ordinary build, and a list with one owner handed to
                // a parameter written `list[...]`, which is a shared list
                // that the function may keep.
                (Ty::Borrowed(inner) | Ty::Owned(inner), Ty::List(_)) if matches!(**inner, Ty::List(_)) => {
                    message = format!(
                        "{what} is written `{to}`, which is a shared list that the function may keep, \
                         and this list has one owner. Write the parameter as `owned {to}`: \
                         then it only looks, and any list can be passed to it"
                    )
                }
                (Ty::Borrowed(inner) | Ty::Owned(inner), Ty::Dict(..)) if matches!(**inner, Ty::Dict(..)) => {
                    message = format!(
                        "{what} is written `{to}`, which is a shared dict that the function may keep, \
                         and this dict has one owner. Write the parameter as `owned {to}`: \
                         then it only looks, and any dict can be passed to it"
                    )
                }
                (Ty::Borrowed(_), _) => {
                    message += ". Something else owns it, so it can go to a parameter that looks, \
                                and not to one that says `take` or to anything else that keeps it"
                }
                // A list of a fixed length kept as one that may grow.
                (Ty::Fixed(item, length), Ty::List(_)) => {
                    message += &format!(
                        ". This one always has {length} items, and a list[{item}] may be made longer. \
                         Write `list[{item}, {length}]` there, or copy the items into a list of its own with `.slice(0)`"
                    )
                }
                (Ty::List(_), Ty::Fixed(item, length)) => {
                    message += &format!(
                        ". A list[{item}, {length}] always has {length} items, and how many this one has is only \
                         known when the program runs. Write the {length} items, or `[0] * {length}`"
                    )
                }
                (Ty::Dict(..), Ty::Owned(_)) => {
                    message += ". Only a dict made right here can become an owned one: one written out, \
                                as in `[\"a\": 1]` or `[:]`"
                }
                (Ty::List(_), Ty::Owned(_)) => {
                    message += ". Only a list made right here can become an owned one: one written out, \
                                as in `[1, 2]` or `[x for x in xs]`, or what `split`, `map`, `filter` or `+` gives"
                }
                (Ty::Int, Ty::Float) => {
                    message += ". Write a number with a decimal point, as in `1.0`, and convert a value with `float( )`"
                }
                (Ty::Int | Ty::Sized(_), Ty::F32) => {
                    message += ". Write a number with a decimal point, as in `1.0`, and convert a value with `f32( )`"
                }
                (Ty::Sized(width), Ty::U64) if !width.signed => {
                    message += &format!(". Convert it with `u64( )`, which never fails for a {from}")
                }
                (Ty::Int | Ty::Sized(_), Ty::U64) => {
                    message += &format!(". An {from} can be below 0, and a u64 never is. Convert it with `u64( )`, which checks that it fits")
                }
                (Ty::U64, Ty::Sized(_)) => message += &format!(". Convert it with `{to}(int( ))`, which checks that it fits"),
                (Ty::U64, Ty::Int) => {
                    message += ". Half the numbers a u64 holds are past the largest int. \
                                Convert it with `int( )`, which checks that it fits"
                }
                (Ty::U64, Ty::Float) => message += ". Convert it with `float( )`",
                (Ty::Float, Ty::F32) => {
                    message += ". An f32 holds about 7 digits and a float about 16, so most floats are not one. \
                                Convert it with `f32( )`, which rounds it to the nearest f32"
                }
                (Ty::List(from), Ty::List(to)) if matches!((&**from, &**to), (Ty::F32, Ty::Float)) => {
                    message += ". The list could then be given a float that is not an f32. \
                                Make a list of floats from it with `[float(x) for x in ...]`"
                }
                (_, Ty::Function(_)) => {
                    message += ". A variable that was given a function of the program and no type holds that one \
                                function only. Write its type to let it hold others, as in \
                                `var f: function(int) -> int = double`"
                }
                (Ty::List(from), Ty::List(to)) if sized_into_int(from, to) => {
                    message += ". The list could then be given a number too big for it. \
                                Make a list of ints from it with `[int(n) for n in ...]`"
                }
                (Ty::List(from), Ty::List(to)) if seen_as_interface(from, to) && fits(from, to) => {
                    message += &format!(
                        ". Whoever holds it as a list of {to} could append another class to it. \
                         Say what the list holds where it is made, as in `let items: list[{to}] = [...]`"
                    )
                }
                // There is no way back from an interface to a class.
                (from, to) if is_one(&to.clone().looked_at(), &from.clone().looked_at()) => {
                    let interface = from.clone().looked_at();
                    message += &format!(
                        ". A {interface} can be an object of any class that is one. \
                         Keep the object in a variable of its own class as well, or give {interface} a function for what is needed"
                    )
                }
                (Ty::Int | Ty::Sized(_), Ty::Sized(width)) => {
                    message += &format!(". Convert it with `{}(...)`, which checks that it fits", width.name)
                }
                (Ty::Optional(_), _) => message += &format!(". {}", self.none_first(span, "Check it with `is not none` first")),
                (Ty::Quantity(from), Ty::Quantity(to)) if units::convert(from, to).is_some() => {
                    message += &format!(". Convert it with `.{to}`")
                }
                _ => {}
            }
            self.error(message, span);
            return;
        }
        if widens_f32(from, to) {
            self.types.widened.insert((span.line, span.col));
        }
        self.arrives(from, to, what, span);
    }

    /// A number that is written out adapts to the sized type it is put in:
    /// `let b: u8 = 7` needs no `u8(7)`. This gives the type the value has
    /// there, after checking that the number fits. Anything that is not a
    /// written number keeps the type it was given.
    fn adapted(&mut self, written: Option<Written>, place: usize, given: Ty, wanted: &Ty, span: Span) -> Ty {
        let Some(written) = written else {
            // A list written out with names in it, as in `[start, command]`:
            // only its length is known.
            if let (Some(length), Some(count)) = (fixed_length(wanted), self.counted.get(&place).copied()) {
                if count != length as i128 {
                    return self.error(too_many_or_few(count, wanted, length), span);
                }
                return fixed_as(given, length);
            }
            return given;
        };
        let mut ty = match written_fits(&written, wanted) {
            None => given,
            Some(Ok(())) => {
                let mut reals = Vec::new();
                reals_into_f32(&written, wanted, &mut reals);
                self.types.f32s.extend(reals.iter().map(|at| (at.line, at.col)));
                let mut wholes = Vec::new();
                numbers_into_u64(&written, wanted, &mut wholes);
                self.types.u64s.extend(wholes.iter().map(|at| (at.line, at.col)));
                sized_as(&given, wanted)
            }
            Some(Err(message)) => self.error(message, span),
        };
        // `[0] * 64` where a `list[u8, 64]` is wanted: the length is
        // known before the program runs, and must be the one asked for.
        let count = written_length(&written).or_else(|| self.counted.get(&place).copied());
        if let (Some(length), Some(count)) = (fixed_length(wanted), count) {
            if count != length as i128 {
                return self.error(too_many_or_few(count, wanted, length), span);
            }
            ty = fixed_as(ty, length);
        }
        // `[1, 2]` where a `list[u8]` is wanted is made as a list of bytes.
        if matches!(written, Written::List(_) | Written::Repeat { .. }) && place != 0 && is_bytes(&ty) {
            self.types.bytes.insert(place);
            if let Written::Repeat { place, .. } = written {
                self.types.bytes.insert(place);
            }
        }
        ty
    }

    /// A value of type `from` goes where a `to` is wanted, and the two fit.
    /// What is left concerns functions, which may sit inside a list or an
    /// `or none`: one going where a function type is wanted is compared with
    /// it in detail, and one going where its type is not tracked `escapes`.
    fn arrives(&mut self, from: &Ty, to: &Ty, what: &str, span: Span) {
        match (from, to) {
            (_, to) if to.is_loose() => self.escapes(from),
            (Ty::List(from), Ty::List(to)) | (Ty::Optional(from), Ty::Optional(to)) => {
                self.arrives(from, to, what, span)
            }
            (from, Ty::Optional(to)) => self.arrives(from, to, what, span),
            (Ty::Owned(from) | Ty::Borrowed(from), to) => self.arrives(from, to, what, span),
            (from, Ty::Borrowed(to)) => self.arrives(from, to, what, span),
            (Ty::Callable(given), Ty::Callable(wanted)) => {
                let raises: Vec<String> = given.raises.iter().cloned().collect();
                self.raises_fit(&raises, false, wanted, to, what, span);
            }
            (from, Ty::Callable(wanted)) => {
                if let Some(given) = self.callable(from) {
                    self.expect_function(&given, wanted, to, what, span);
                    if let Ty::Function(function) | Ty::BoundMethod(function) = from {
                        let raises = self.signature(function).raises.clone();
                        self.raises_fit(&raises, true, wanted, to, what, span);
                    }
                }
            }
            _ => {}
        }
    }

    /// A function that raises `raises` goes where the function type `to`
    /// is wanted, and the two have the same shape. Does what it raises
    /// fit what the type says? `made` is true for a function of the
    /// program, named or written right here, and false for a value of a
    /// function type (decisions 56.4, 56.5).
    fn raises_fit(&mut self, raises: &[String], made: bool, wanted: &CallableType, to: &Ty, what: &str, span: Span) {
        match &wanted.raises {
            Some(said) => {
                if let Some(kind) = raises.iter().find(|kind| !caught_by(kind, said)) {
                    self.error(
                        format!(
                            "{what} should be {to}, but this function can raise {kind}, and the type says it raises \
                             only {said}. Write `raises Error` on the type to take a function that raises any kind"
                        ),
                        span,
                    );
                    return;
                }
                // A firmware build calls a function that can raise in
                // another way from one that cannot (5.5l). A function of
                // the program is made the way the type is called. A value
                // that is already of a plain function type cannot change.
                if self.firmware && raises.is_empty() {
                    match made {
                        true => {
                            let at = spot(self.within(), span);
                            self.types.made_raising.insert(at, said.clone());
                        }
                        false => {
                            self.error(
                                format!(
                                    "{what} should be {to}, and this is a function value that raises nothing. In a \
                                     firmware build a function that can raise is called in another way from one \
                                     that cannot, so one kept as the first cannot become the second. Give it the \
                                     type `{to}` where it is made, or build without `--firmware`"
                                ),
                                span,
                            );
                        }
                    }
                }
            }
            // An ordinary build does not hold what a function raises
            // against a type that says nothing, as it never has. A
            // firmware build calls the two in different ways, so it has
            // to (decision 56.4).
            None => {
                if let (true, Some(kind)) = (self.firmware, raises.first()) {
                    let kinds = if raises.len() == 1 { kind.clone() } else { "Error".to_string() };
                    self.error(
                        format!(
                            "{what} should be {to}, which raises nothing, but this function can raise {kind}. \
                             In a firmware build a call through a function type raises only what the type \
                             says: write `raises {kinds}` after what it returns, or build without `--firmware`"
                        ),
                        span,
                    );
                }
            }
        }
    }

    /// The kind of error after `raises` in a function type, which must be
    /// a kind of error. A firmware build holds a function that says it
    /// raises a fault to raising one itself (5.19m), so a type that says
    /// so needs no rule of its own: only such a function, or one that
    /// raises nothing, can be put in it (decision 56.1).
    fn kind_in_type(&mut self, kind: &str, env: &Env, span: Span) -> String {
        let known = env.get(kind).map(|variable| variable.ty);
        if !matches!(known, Some(Ty::ErrorType(_) | Ty::Class(_))) {
            self.error(format!("`{kind}` is not a kind of error"), span);
        }
        kind.to_string()
    }

    /// What a function type says it raises, for a function that raises
    /// `kinds` and is given the type of a value: `var f = parse`. An
    /// ordinary build says nothing, as it always has (5.19l). A firmware
    /// build calls through the value as the type says, so it says what
    /// the function raises, or `Error` for more than one kind (56.4).
    fn raises_in_type(&self, kinds: &[String]) -> Option<String> {
        match (self.firmware, kinds) {
            (false, _) | (true, []) => None,
            (true, [kind]) => Some(kind.clone()),
            (true, _) => Some("Error".to_string()),
        }
    }

    // ----------------------------------------------- Working out left-out types

    /// The type of a parameter or field: what is written, or failing that,
    /// what has been worked out from how it is used.
    fn slot_type(
        &mut self,
        annotation: &Option<ast::Type>,
        id: &SlotId,
        what: &str,
        env: &Env,
        span: Span,
        parameter: bool,
    ) -> Ty {
        if let Some(annotation) = annotation {
            return self.resolve_for(annotation, env, span, parameter);
        }
        if let Some(key) = self.known.get(id).cloned() {
            return self.from_key(&key);
        }
        if !self.last_round && !self.open.contains(id) {
            // Not settled yet. `Invalid` is quiet and is not counted as
            // evidence about anything it is passed on to.
            return Ty::Invalid;
        }
        if self.strict_at(span) {
            return self.error(
                format!("strict mode: {what} needs a type. It could not be worked out from how it is used"),
                span,
            );
        }
        Ty::Dynamic
    }

    /// Note that a value of type `ty` arrives at `slot`, and where.
    fn sees(&mut self, slot: &Slot, ty: &Ty, span: Span) {
        let Some(id) = &slot.id else {
            return;
        };
        // Remember the first place that says anything about this slot, so
        // that an error caused by what was worked out can point back to it.
        if !matches!(ty, Ty::Invalid) {
            self.origins.entry(id.clone()).or_insert(span.line_in_file());
        }
        let seen = match (self.key(ty), self.seen.remove(id)) {
            (None, previous) => previous, // no evidence either way
            (Some(Ok(key)), None) => Some(Seen::Known(key)),
            (Some(Ok(key)), Some(Seen::Known(old))) => {
                Some(join_keys(&old, &key).map_or(Seen::Unknowable, Seen::Known))
            }
            _ => Some(Seen::Unknowable),
        };
        if let Some(seen) = seen {
            self.seen.insert(id.clone(), seen);
        }
    }

    /// `ty` as a key. `None` means the type is not settled yet and says
    /// nothing; `Some(Err(()))` means it can never be pinned down.
    fn key(&self, ty: &Ty) -> Option<Result<Key, ()>> {
        Some(Ok(match ty {
            Ty::Invalid => return None,
            Ty::Int => Key::Int,
            Ty::Float => Key::Float,
            Ty::Str => Key::Str,
            Ty::Bool => Key::Bool,
            Ty::Pin => Key::Pin,
            Ty::InputPin => Key::InputPin,
            Ty::Pull => Key::Pull,
            Ty::Serial => Key::Serial,
            Ty::AnalogPin => Key::AnalogPin,
            Ty::PwmPin => Key::PwmPin,
            Ty::Ticker => Key::Ticker,
            Ty::Edge => Key::Edge,
            Ty::Interrupt => Key::Interrupt,
            Ty::Element => Key::Element,
            Ty::Event => Key::Event,
            Ty::Timer => Key::Timer,
            Ty::Sized(width) => Key::Sized(width.name),
            Ty::Pointer(pointee) => Key::Pointer(pointee.name()),
            Ty::Register(width) => Key::Register(width.name),
            Ty::None => Key::None,
            Ty::Quantity(unit) => Key::Quantity(unit.to_string()),
            Ty::Error(kind) => Key::Error(kind.to_string()),
            // Who owns a value is never guessed: it has to be written down.
            Ty::Instance(class) if class.owned => return Some(Err(())),
            // A copy of a generic class is named by what it is a copy of,
            // so that the next pass can make it again.
            Ty::Instance(class) if class.origin.is_some() => {
                let (generic, types) = class.origin.as_ref()?;
                let mut keys = Vec::new();
                for ty in types {
                    match self.key(ty)? {
                        Ok(key) => keys.push(key),
                        Err(()) => return Some(Err(())),
                    }
                }
                Key::Copy(generic.id.clone(), keys)
            }
            Ty::Instance(class) => Key::Class(class.id.clone()),
            // The same for a copy of a generic enum (decision 58.1).
            Ty::EnumValue(info) if info.origin.is_some() => {
                let (generic, types) = info.origin.as_ref()?;
                let mut keys = Vec::new();
                for ty in types {
                    match self.key(ty)? {
                        Ok(key) => keys.push(key),
                        Err(()) => return Some(Err(())),
                    }
                }
                Key::Copy(generic.id.clone(), keys)
            }
            Ty::EnumValue(info) => Key::Enum(info.id.clone()),
            Ty::List(inner) => match self.key(inner)? {
                Ok(inner) => Key::List(Box::new(inner)),
                Err(()) => return Some(Err(())),
            },
            Ty::Dict(key, value) => match (self.key(key)?, self.key(value)?) {
                (Ok(key), Ok(value)) => Key::Dict(Box::new(key), Box::new(value)),
                _ => return Some(Err(())),
            },
            Ty::Optional(inner) => match self.key(inner)? {
                Ok(inner) => Key::Optional(Box::new(inner)),
                Err(()) => return Some(Err(())),
            },
            Ty::Several(types) => {
                let mut keys = Vec::new();
                for ty in types {
                    match self.key(ty)? {
                        Ok(key) => keys.push(key),
                        Err(()) => return Some(Err(())),
                    }
                }
                Key::Several(keys)
            }
            _ => return Some(Err(())),
        }))
    }

    fn from_key(&mut self, key: &Key) -> Ty {
        match key {
            Key::Int => Ty::Int,
            Key::Float => Ty::Float,
            Key::Str => Ty::Str,
            Key::Bool => Ty::Bool,
            Key::Pin => Ty::Pin,
            Key::InputPin => Ty::InputPin,
            Key::Pull => Ty::Pull,
            Key::Serial => Ty::Serial,
            Key::AnalogPin => Ty::AnalogPin,
            Key::PwmPin => Ty::PwmPin,
            Key::Ticker => Ty::Ticker,
            Key::Edge => Ty::Edge,
            Key::Interrupt => Ty::Interrupt,
            Key::Element => Ty::Element,
            Key::Event => Ty::Event,
            Key::Timer => Ty::Timer,
            Key::Sized(name) => ints::width(name).map_or(Ty::Int, Ty::Sized),
            Key::Pointer(name) => Pointee::named(name).map_or(Ty::Invalid, Ty::Pointer),
            Key::Register(name) => ints::width(name).map_or(Ty::Invalid, Ty::Register),
            Key::None => Ty::None,
            Key::Quantity(unit) => Ty::Quantity(Rc::from(unit.as_str())),
            Key::Error(kind) => Ty::Error(Rc::from(kind.as_str())),
            Key::List(inner) => Ty::list(self.from_key(inner)),
            Key::Dict(key, value) => Ty::dict(self.from_key(key), self.from_key(value)),
            Key::Optional(inner) => Ty::Optional(Box::new(self.from_key(inner))),
            Key::Class(id) => self.classes.get(id).map_or(Ty::Dynamic, |class| Ty::Instance(class.clone())),
            Key::Copy(id, keys) => {
                let types: Vec<Ty> = keys.iter().map(|key| self.from_key(key)).collect();
                let generic = self.generics.iter().find(|generic| generic.id == *id).cloned();
                let copy = generic.and_then(|generic| match &generic.template {
                    Template::Enum(_) => self.copy_enum(&generic, types, None).map(Ty::EnumValue),
                    _ => self.copy_class(&generic, types, None).map(Ty::Instance),
                });
                copy.unwrap_or(Ty::Dynamic)
            }
            Key::Enum(id) => self.enums.get(id).map_or(Ty::Dynamic, |info| Ty::EnumValue(info.clone())),
            Key::Several(keys) => Ty::Several(keys.iter().map(|key| self.from_key(key)).collect()),
        }
    }

    /// A function value has gone somewhere its type is not tracked, so it
    /// may be called from there with anything. Its untyped parameters can
    /// no longer be worked out from the calls that are visible.
    fn escapes(&mut self, ty: &Ty) {
        let function = match ty {
            Ty::Function(function) | Ty::BoundMethod(function) => function.clone(),
            Ty::List(inner) | Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner) => {
                return self.escapes(inner)
            }
            _ => return,
        };
        for slot in &self.signature(&function).params {
            if let Some(id) = &slot.id {
                self.seen.insert(id.clone(), Seen::Unknowable);
            }
        }
    }

    /// `join`, for places where a function might be one of the things joined.
    fn join(&mut self, a: &Ty, b: &Ty) -> Ty {
        let joined = join(a, b);
        if joined.is_loose() {
            self.escapes(a);
            self.escapes(b);
        }
        joined
    }

    fn join_all(&mut self, types: &[Ty]) -> Option<Ty> {
        let (first, rest) = types.split_first()?;
        let mut joined = first.clone();
        for ty in rest {
            joined = self.join(&joined, ty);
        }
        Some(joined)
    }

    /// If every one of `types` is a function, and all have the same
    /// parameters, result and error, the function type they share.
    fn one_function_type(&mut self, types: &[Ty]) -> Option<Ty> {
        let mut shared: Option<Rc<CallableType>> = None;
        for ty in types {
            let callable = match ty.clone().looked_at() {
                Ty::Callable(callable) => callable,
                Ty::Function(function) if function.self_type.is_none() => {
                    let signature = self.signature(&function);
                    Rc::new(CallableType {
                        params: signature.params.iter().map(|slot| slot.ty.clone()).collect(),
                        returns: signature.returns.clone(),
                        raises: self.raises_in_type(&signature.raises),
                        owned: self.one_owner,
                    })
                }
                _ => return None,
            };
            if callable.params.iter().any(Ty::is_loose) || callable.returns.is_loose() {
                return None;
            }
            match &shared {
                Some(first) if !same(&Ty::Callable(first.clone()), &Ty::Callable(callable.clone())) => return None,
                Some(_) => {}
                None => shared = Some(callable),
            }
        }
        shared.map(Ty::Callable)
    }

    /// If `ty` is a function whose details are known, its parameters and
    /// what it returns.
    fn callable(&mut self, ty: &Ty) -> Option<(Vec<Slot>, Ty)> {
        match ty {
            Ty::Function(function) => {
                let signature = self.signature(function);
                Some((signature.params.clone(), signature.returns.clone()))
            }
            Ty::BoundMethod(function) => {
                let signature = self.signature(function);
                Some((signature.params.get(1..).unwrap_or_default().to_vec(), signature.returns.clone()))
            }
            _ => None,
        }
    }

    /// Check a function against a written function type.
    fn expect_function(
        &mut self,
        (params, returns): &(Vec<Slot>, Ty),
        wanted: &CallableType,
        wanted_type: &Ty,
        what: &str,
        span: Span,
    ) {
        let needed = params.iter().filter(|slot| !slot.has_default).count();
        if !(needed..=params.len()).contains(&wanted.params.len()) {
            self.error(
                format!(
                    "{what} should be {wanted_type}, but this function takes {}",
                    counted(params.len(), "argument")
                ),
                span,
            );
            return;
        }
        for (slot, given) in params.iter().zip(&wanted.params) {
            // It will be called with these types, which is evidence for an
            // untyped parameter just as a direct call would be.
            self.sees(slot, given, span);
            if !fits(given, &slot.ty) {
                self.error(
                    format!(
                        "{what} should be {wanted_type}, but the parameter `{}` of this function {}",
                        slot.name,
                        match &slot.ty {
                            ty if ty.is_look() => format!("only looks at what it is given (`{}`)", as_parameter(ty)),
                            ty if ty.is_owned() => format!("takes over what it is given (`{}`)", as_parameter(ty)),
                            ty => format!("is {ty}"),
                        }
                    ),
                    span,
                );
                return;
            }
        }
        if !fits(returns, &wanted.returns) {
            self.error(format!("{what} should be {wanted_type}, but this function returns {returns}"), span);
        }
    }

    // ------------------------------------------------------- Type annotations

    /// Turn a type written in the source into a `Ty`.
    fn resolve(&mut self, ty: &ast::Type, env: &Env, span: Span) -> Ty {
        self.resolve_for(ty, env, span, false)
    }

    /// `resolve`, for what a function gives: after `->` alone, `(int,
    /// str)` says it gives two values (decision 57).
    fn resolve_returns(&mut self, ty: &ast::Type, env: &Env, span: Span) -> Ty {
        match ty {
            ast::Type::Several(types) => Ty::Several(types.iter().map(|ty| self.resolve(ty, env, span)).collect()),
            ty => self.resolve(ty, env, span),
        }
    }

    /// `resolve`, knowing whether the type is a parameter's. `owned` and
    /// `take` are words about the whole value, so they are only looked
    /// for here, at the front, and `take` only on a parameter.
    ///
    /// Is this written type a type parameter of a generic, such as the
    /// `T` of `take T`, or one that may be `none`?
    fn a_type_parameter(&self, ty: &ast::Type, env: &Env) -> bool {
        match ty {
            ast::Type::Named { name, args } if args.is_empty() => {
                env.get(name).is_some_and(|variable| self.type_params.contains(&variable.id))
            }
            ast::Type::Optional(inner) => self.a_type_parameter(inner, env),
            _ => false,
        }
    }

    /// A parameter looks at what it is given. If the value has one
    /// owner, the caller goes on owning it, and the parameter's type is
    /// a look at it. `take` is the word for a parameter that takes the
    /// value over: its type is then what a variable's would be.
    fn resolve_for(&mut self, ty: &ast::Type, env: &Env, span: Span, parameter: bool) -> Ty {
        if parameter {
            return match ty {
                // `take T` is right for whatever `T` stands for: where it
                // is a number, the word changes nothing (decision 56.11).
                ast::Type::Take(inner) => match self.resolve_for(inner, env, span, false) {
                    ty if borrowable(&ty) || ty.is_loose() || self.a_type_parameter(inner, env) => ty,
                    ty => self.error(
                        format!(
                            "`take` is for a value that has an owner: a list, an object, a function, an error \
                             or an enum value that carries values. This is {ty}, which is copied. Write the type alone"
                        ),
                        span,
                    ),
                },
                _ => match self.resolve_for(ty, env, span, false) {
                    ty if ty.is_owned() && borrowable(&ty) => lent(&ty),
                    ty => ty,
                },
            };
        }
        match ty {
            ast::Type::Owned(inner) => match self.resolve_inside(inner, env, span, Inside::Front) {
                list if list.is_collection() => Ty::Owned(Box::new(list)),
                // `owned list[int] or none`: the list has an owner when
                // there is one.
                Ty::Optional(list) if list.is_collection() => Ty::Optional(Box::new(Ty::Owned(list))),
                // An object of an `owned class` is owned without saying so again.
                ty if ty.is_owned() || ty.is_loose() => ty,
                ty => self.error(
                    format!(
                        "`owned` goes in front of a list or a dict type, as in `owned list[int]`, and this is {ty}. \
                         A class is made owned where it is declared: `owned class`"
                    ),
                    span,
                ),
            },
            ast::Type::Take(_) => self.error(
                "`take` can only be written on a parameter, where it says that the function takes the value over. \
                 A variable, a field and what a function gives back always own what they hold",
                span,
            ),
            _ => match self.resolve_inside(ty, env, span, Inside::Front) {
                // Where everything has one owner, a list does too, and a dict.
                list if list.is_collection() && self.one_owner => Ty::Owned(Box::new(list)),
                // A list is the owner of items that have one owner each,
                // so somebody has to be the owner of the list.
                // A list of resources is refused where it is used, in the
                // words of 59.1, with a collector or without.
                list if self.holds_resource(&list) => list,
                Ty::Optional(list) if owns_items(&list) => self.error(
                    "each item of this list has one owner: the list. So the list needs an owner too. \
                     Write `owned list[...] or none`",
                    span,
                ),
                Ty::List(item) if item.is_owned() => self.error(
                    format!(
                        "each item of this list is {}, which has one owner: the list. So the list needs an \
                         owner too. Write `owned list[...]`",
                        an_owned(&item)
                    ),
                    span,
                ),
                ty => ty,
            },
        }
    }

    /// The part of a type after any `owned` or `take`. `inside` says
    /// what the type is written in. A list can hold values that have one
    /// owner: it is their owner. So can an `or none` and the `[ ]` of a
    /// generic (decision 56.11), where the copy is checked as if written
    /// by hand.
    fn resolve_inside(&mut self, ty: &ast::Type, env: &Env, span: Span, inside: Inside) -> Ty {
        let written = ty as *const ast::Type as usize;
        let ty = match ty {
            ast::Type::Owned(_) | ast::Type::Take(_) => {
                return self.error(
                    "`owned` and `take` describe a whole value, so they go at the very front of a type, \
                     and not inside `list[ ]` or `or none`",
                    span,
                )
            }
            ast::Type::Several(_) => {
                return self.error(
                    "`(int, str)` is written only after `->`, for a function that gives two values or more. \
                     A value that holds several things is an object: declare a class",
                    span,
                )
            }
            ast::Type::Length(length) => {
                return self.error(
                    format!("{length} is a number, and a type is wanted here. Only a list takes a length: `list[u8, {length}]`"),
                    span,
                )
            }
            // What may be `none` can have one owner: `none` is what is
            // left when it is given a new value.
            ast::Type::Optional(inner) => {
                let within = match inside {
                    Inside::Other => Inside::Other,
                    _ => Inside::Optional,
                };
                let inner = self.resolve_inside(inner, env, span, within);
                if !self.one_owner && owns_items(&inner) && !matches!(inside, Inside::Front) {
                    return self.error(
                        "the items of this list have one owner each, so the list needs one too, and \
                         `owned` cannot be written inside another type yet",
                        span,
                    );
                }
                Ty::Optional(Box::new(inner))
            }
            // Each parameter of a function type is a parameter: it looks
            // unless it says `take`.
            ast::Type::Function { params, returns, raises } => {
                let params = params.iter().map(|param| self.resolve_for(param, env, span, true)).collect();
                let returns = match returns {
                    Some(returns) => self.resolve_returns(returns, env, span),
                    None => Ty::None,
                };
                let raises = raises.as_ref().map(|kind| self.kind_in_type(kind, env, span));
                Ty::Callable(Rc::new(CallableType { params, returns, raises, owned: self.one_owner }))
            }
            ast::Type::Named { name, args } => {
                // `list[function() raises A, B]`: the type ends at the
                // comma, so `B` reads as one more type in `[ ]`. If it is
                // the name of a kind of error, a second kind was meant
                // (decision 56.3).
                let second_kind = args.windows(2).any(|pair| {
                    let kind = match &pair[1] {
                        ast::Type::Named { name, args } if args.is_empty() => env.get(name).map(|variable| variable.ty),
                        _ => None,
                    };
                    crate::parser::ends_in_raises(&pair[0]) && matches!(kind, Some(Ty::ErrorType(_) | Ty::Class(_)))
                });
                if second_kind {
                    return self.error(crate::parser::ONE_KIND, span);
                }
                let ty = match (name.as_str(), args.as_slice()) {
                    ("int", []) => Ty::Int,
                    ("float", []) => Ty::Float,
                    ("f32", []) => Ty::F32,
                    ("u64", []) => Ty::U64,
                    ("str", []) => Ty::Str,
                    ("bool", []) => Ty::Bool,
                    ("list", [item]) => {
                        let item = self.resolve_inside(item, env, span, Inside::List);
                        let owns = item.is_owned();
                        let list = Ty::list(item);
                        match inside {
                            // `list[int] or none`, where everything has
                            // one owner.
                            Inside::Optional if self.one_owner => Ty::Owned(Box::new(list)),
                            Inside::Front | Inside::Optional => list,
                            // Where everything has one owner, a list in
                            // a list has too: the list it is in.
                            _ if self.one_owner => Ty::Owned(Box::new(list)),
                            _ if owns => {
                                return self.error(
                                    "the items of this list have one owner each, so the list needs one too, and \
                                     `owned` cannot be written inside another type yet",
                                    span,
                                )
                            }
                            _ => list,
                        }
                    }
                    // `list[u8, 64]`: always 64 items (decision 36.4).
                    ("list", [item, ast::Type::Length(length)]) => {
                        if *length == 0 {
                            return self.error("a list of a fixed length has at least one item: `list[u8, 1]` or more", span);
                        }
                        let item = self.resolve_inside(item, env, span, Inside::List);
                        if item.is_owned() {
                            return self.error(
                                "a list of a fixed length holds values that are not owned for now: numbers, text, \
                                 bools, enum values and shared objects",
                                span,
                            );
                        }
                        let list = Ty::Fixed(Box::new(item), *length);
                        match inside {
                            Inside::Optional if self.one_owner => Ty::Owned(Box::new(list)),
                            Inside::Front | Inside::Optional => list,
                            _ if self.one_owner => Ty::Owned(Box::new(list)),
                            _ => list,
                        }
                    }
                    ("list", [_, _]) => {
                        return self.error(
                            "a list takes the type of its items, and then may take how many there are, as in `list[u8, 64]`",
                            span,
                        )
                    }
                    ("list", _) => {
                        return self.error("`list` takes one type, as in `list[int]`", span)
                    }
                    // `dict[str, int]`: what the keys are, then what the
                    // values are. A value is anything a list can hold
                    // (decision 10.11).
                    ("dict", [key, value]) => {
                        let key = self.resolve_inside(key, env, span, Inside::Other);
                        let value = self.resolve_inside(value, env, span, Inside::List);
                        // A `K` of a generic is checked when it is known
                        // what `K` is, in each copy.
                        if !matches!(key, Ty::Str | Ty::Int | Ty::Param(_)) && !key.is_loose() {
                            return self.error(
                                format!(
                                    "a key of a dict is a str or an int, and this says {}. Write `dict[str, ...]` \
                                     or `dict[int, ...]`",
                                    key.clone().looked_at()
                                ),
                                span,
                            );
                        }
                        if let Some(why) = self.no_such_value(&value) {
                            return self.error(why, span);
                        }
                        let dict = Ty::dict(key, value);
                        match inside {
                            Inside::Front => dict,
                            // Where everything has one owner, a dict has too.
                            _ if self.one_owner => Ty::Owned(Box::new(dict)),
                            _ => dict,
                        }
                    }
                    ("dict", _) => {
                        return self.error(
                            "`dict` takes two types, the key's and then the value's, as in `dict[str, int]`",
                            span,
                        )
                    }
                    // `pointer[u8]`: a raw pointer to numbers of one size.
                    ("pointer", [ast::Type::Named { name: item, args }]) if args.is_empty() && Pointee::named(item).is_some() => {
                        Pointee::named(item).map_or(Ty::Invalid, Ty::Pointer)
                    }
                    ("pointer", _) => {
                        return self.error(
                            "a pointer points at numbers of one size: `pointer[u8]`, `pointer[int]`, `pointer[float]`, \
                             or any of u8, u16, u32, i8, i16 and i32",
                            span,
                        )
                    }
                    // `any` asks for a dynamic value on purpose.
                    ("any", []) if self.strict_at(span) => {
                        return self.error("`any` is not allowed in strict mode", span)
                    }
                    ("any", []) => Ty::Dynamic,
                    // `Stack[int]`: the copy of a generic class for those types.
                    (_, written_types) if matches!(env.get(name), Some(Variable { ty: Ty::Generic(_), .. })) => {
                        let Some(Variable { ty: Ty::Generic(generic), .. }) = env.get(name) else {
                            return Ty::Invalid;
                        };
                        let example = format!("{name}[{}]", vec!["int"; generic.params.len()].join(", "));
                        if matches!(generic.template, Template::Function(_)) {
                            return self.error(format!("`{name}` is a function, and this place needs a type"), span);
                        }
                        if written_types.len() != generic.params.len() {
                            return self.error(
                                format!(
                                    "`{name}` takes {} in `[ ]`, as in `{example}`, but {} written",
                                    counted(generic.params.len(), "type"),
                                    were(written_types.len())
                                ),
                                span,
                            );
                        }
                        let types: Vec<Ty> =
                            written_types.iter().map(|ty| self.resolve_inside(ty, env, span, Inside::Other)).collect();
                        if types.iter().any(has_param) {
                            // Still a pattern: the `Stack[T]` of a generic
                            // function's own parameters.
                            Ty::Applied(generic, types)
                        } else if types.iter().any(|ty| matches!(ty, Ty::Invalid)) {
                            Ty::Invalid
                        } else {
                            self.copy_type(&generic, types, span).unwrap_or(Ty::Invalid)
                        }
                    }
                    (_, []) => match env.get(name).map(|variable| variable.ty) {
                        // Inside a copy, `T` is the type it was made for.
                        Some(Ty::Alias(ty)) => *ty,
                        Some(Ty::Class(class)) => Ty::Instance(class),
                        Some(Ty::Enum(info)) => Ty::EnumValue(info),
                        Some(Ty::ErrorType(kind)) => self.an_error(kind),
                        // `u8` the type shares its name with `u8( )` the
                        // conversion, which is how it is found here.
                        Some(Ty::Builtin(name)) if ints::width(name).is_some() => {
                            ints::width(name).map_or(Ty::Int, Ty::Sized)
                        }
                        // A unit is the type of a quantity measured in it.
                        _ if units::is_unit(name) => Ty::Quantity(Rc::from(name.as_str())),
                        // A pin of the board. A class of the program's own
                        // called `Pin` was found above, and comes first.
                        _ if name == "Pin" => Ty::Pin,
                        _ if name == "InputPin" => Ty::InputPin,
                        _ if name == "Pull" => Ty::Pull,
                        _ if name == "Serial" => Ty::Serial,
                        _ if name == "AnalogPin" => Ty::AnalogPin,
                        _ if name == "PwmPin" => Ty::PwmPin,
                        _ if name == "Ticker" => Ty::Ticker,
                        _ if name == "Edge" => Ty::Edge,
                        _ if name == "Interrupt" => Ty::Interrupt,
                        // An element of the page, by the same rule.
                        _ if name == "Element" => Ty::Element,
                        _ if name == "Event" => Ty::Event,
                        _ if name == "Timer" => Ty::Timer,
                        _ if name == "Register" => {
                            return self.error("`Register` needs the size of the register, as in `Register[u32]`", span)
                        }
                        // 42.1: `int` is the one name for it.
                        _ if name == "i64" => {
                            return self.error(
                                "there is no type called `i64`. An `int` is 64 bits with a sign, so write `int`. \
                                 The sized whole numbers are u8, u16, u32, u64, i8, i16 and i32",
                                span,
                            )
                        }
                        // 58.3: a question asked of a type parameter.
                        _ if name == "Ordered" => {
                            return self.error(
                                "`Ordered` is not a type: it is what a type parameter may ask of its type, that `<` \
                                 works on two of it, as in `function largest[T is Ordered](items: list[T]) -> T`",
                                span,
                            )
                        }
                        _ => return self.error(format!("there is no type called `{name}`"), span),
                    },
                    // A register of the board. A class of the program's
                    // own called `Register` was found above.
                    ("Register", [ast::Type::Named { name: size, args }])
                        if args.is_empty() && ints::width(size).is_some_and(|width| !width.signed) =>
                    {
                        ints::width(size).map_or(Ty::Invalid, Ty::Register)
                    }
                    ("Register", _) => {
                        return self.error(
                            "a register holds a whole number of one size: `Register[u8]`, `Register[u16]` or `Register[u32]`",
                            span,
                        )
                    }
                    _ => return self.error(format!("`{name}` does not take types in `[ ]`"), span),
                };
                ty
            }
        };
        // The compilers ask what a written type stands for by where it is
        // written. Inside a copy of a generic only the checker knows.
        if !has_param(&ty) {
            self.types.annotations.insert(written, kind(&ty));
        }
        ty
    }

    // ------------------------------------------------------------ Statements

    /// Check a `{ }` block. Returns true if control can never reach the end
    /// of it, because every path returns, raises, breaks or continues.
    fn block(&mut self, statements: &[Stmt], env: &Env) -> bool {
        self.statements(statements, &Scope::child(env))
    }

    fn statements(&mut self, statements: &[Stmt], env: &Env) -> bool {
        self.hoist(statements, env);
        let mut exits = false;
        for statement in statements {
            exits |= self.stmt(statement, env);
        }
        exits
    }

    /// Declare the block's functions, classes and enums before checking any
    /// of it, exactly as the interpreter does before running it.
    fn hoist(&mut self, statements: &[Stmt], env: &Env) {
        let statements = ast::flat(statements);
        // What each statement has above it in the block: the variables
        // that a function or class written there is allowed to use.
        let mut lets: Vec<String> = Vec::new();
        let mut above: Vec<Rc<[String]>> = Vec::new();
        for statement in &statements {
            above.push(Rc::from(lets.as_slice()));
            match &statement.kind {
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => lets.push(name.clone()),
                StmtKind::Receive { places, .. } => lets.extend(places.iter().filter_map(Place::declared).cloned()),
                StmtKind::Import { module, names } if names.is_empty() => lets.extend(module.last().cloned()),
                StmtKind::Import { names, .. } => lets.extend(names.iter().cloned()),
                _ => {}
            }
        }

        // First the names of the new types, so that they can mention each other.
        let mut classes = Vec::new();
        let mut enums = Vec::new();
        for (statement, above) in statements.iter().zip(&above) {
            match &statement.kind {
                // An enum with type parameters is not an enum yet, as a
                // generic class is not a class (decision 58.1).
                StmtKind::Enum(decl) if !decl.type_params.is_empty() => {
                    self.type_params(&decl.type_params, statement.span);
                    if let Some(case) = decl.variants.iter().find(|case| case.number.is_some()) {
                        self.error(
                            format!(
                                "`{}` has type parameters, so its cases carry values, and a case that carries values \
                                 cannot stand for a number. Take `= {}` away",
                                decl.name,
                                case.number.map_or(0, |(number, _)| number)
                            ),
                            statement.span,
                        );
                    }
                    let generic = Rc::new(Generic {
                        at: decl as *const ast::Enum as usize,
                        id: decl_id(statement.span, &decl.name),
                        name: decl.name.clone(),
                        params: ast::param_names(&decl.type_params),
                        bounds: decl.type_params.iter().map(|param| param.bound.clone()).collect(),
                        asks: RefCell::new(None),
                        template: Template::Enum(Rc::new(decl.clone())),
                        env: env.clone(),
                        span: statement.span,
                        above: above.clone(),
                        outline: RefCell::new(None),
                    });
                    self.declare(env, &decl.name, Ty::Generic(generic.clone()), false, statement.span);
                    self.generics.push(generic.clone());
                    enums.push((decl, Err(generic), statement.span));
                }
                // A class with type parameters is not a class yet: each
                // set of types it is used with gets a copy that is one.
                StmtKind::Class(decl) if !decl.type_params.is_empty() => {
                    self.type_params(&decl.type_params, statement.span);
                    let generic = Rc::new(Generic {
                        at: decl as *const ast::Class as usize,
                        id: decl_id(statement.span, &decl.name),
                        name: decl.name.clone(),
                        params: ast::param_names(&decl.type_params),
                        bounds: decl.type_params.iter().map(|param| param.bound.clone()).collect(),
                        asks: RefCell::new(None),
                        template: Template::Class(Rc::new(decl.clone())),
                        env: env.clone(),
                        span: statement.span,
                        above: above.clone(),
                        outline: RefCell::new(None),
                    });
                    self.declare(env, &decl.name, Ty::Generic(generic.clone()), false, statement.span);
                    self.generics.push(generic);
                }
                StmtKind::Class(decl) => {
                    let info = class_info(decl, self.one_owner, env, above, statement.span, None, None);
                    // The `Resource` that comes with Zeno is written at no
                    // line of any file (`Program::with_resource`).
                    if statement.span.line == 0 && decl.interface && decl.name == "Resource" {
                        self.resource = Some(info.clone());
                    }
                    self.declare(env, &decl.name, Ty::Class(info.clone()), false, statement.span);
                    self.classes.insert(info.id.clone(), info.clone());
                    classes.push((decl, info, statement.span));
                }
                StmtKind::Enum(decl) => {
                    let carries = decl.variants.iter().any(|case| !case.fields.is_empty());
                    let numbers = self.case_numbers(decl, statement.span);
                    let info = Rc::new(EnumInfo {
                        id: decl_id(statement.span, &decl.name),
                        name: decl.name.clone(),
                        cases: RefCell::new(Vec::new()),
                        carries,
                        owned: carries && self.one_owner,
                        numbers,
                        origin: None,
                    });
                    self.declare(env, &decl.name, Ty::Enum(info.clone()), false, statement.span);
                    self.enums.insert(info.id.clone(), info.clone());
                    enums.push((decl, Ok(info), statement.span));
                }
                _ => {}
            }
        }

        // Then what is inside them.
        for (decl, info, span) in classes {
            self.fill_class(decl, &info, env, span);
        }
        for (decl, info, span) in enums {
            // The cases of a generic enum are names in the file as any
            // case is, and the same rule keeps them apart from every
            // other case. What they carry is worked out for each copy.
            let info = match info {
                Ok(info) => info,
                Err(generic) => {
                    for (index, case) in decl.variants.iter().enumerate() {
                        if self.case_name_free(env, &case.name, &decl.name, span) {
                            self.declare(env, &case.name, Ty::GenericCase(generic.clone(), index), false, span);
                            if let (Some(variable), true) =
                                (env.variables.borrow_mut().get_mut(&case.name), case.fields.is_empty())
                            {
                                variable.case = Some(index);
                            }
                        }
                    }
                    continue;
                }
            };
            let cases: Vec<CaseInfo> = decl
                .variants
                .iter()
                .map(|case| CaseInfo {
                    name: case.name.clone(),
                    fields: case.fields.iter().map(|f| self.slot(f, &case.name, env, span)).collect(),
                })
                .collect();
            // A case that carries a resource (rule 7 of 59.1), and a value
            // that is shared, which cannot hold one that has one owner
            // (rule 6 of docs/ownership.md).
            if let Some(case) = cases.iter().find(|case| case.fields.iter().any(|field| self.holds_resource(&field.ty))) {
                self.resource_inside(&format!("the case `{}` of the enum `{}`", case.name, decl.name), span);
            } else if !info.owned && cases.iter().any(|case| case.fields.iter().any(|field| field.ty.is_owned())) {
                self.error(format!("a case of the enum `{}` cannot carry an owned value yet", decl.name), span);
            }
            for (index, case) in cases.iter().enumerate() {
                if !self.case_name_free(env, &case.name, &info.name, span) {
                    continue;
                }
                if case.fields.is_empty() {
                    // A bare case is a value of the enum.
                    self.declare(env, &case.name, Ty::EnumValue(info.clone()), false, span);
                    if let Some(variable) = env.variables.borrow_mut().get_mut(&case.name) {
                        variable.case = Some(index);
                    }
                } else {
                    self.declare(env, &case.name, Ty::CaseConstructor(info.clone(), index), false, span);
                }
            }
            self.types.enums.insert(
                decl as *const ast::Enum as usize,
                cases.iter().map(|case| case.fields.iter().map(|slot| kind(&slot.ty)).collect()).collect(),
            );
            *info.cases.borrow_mut() = cases;
        }

        for (statement, above) in statements.iter().zip(&above) {
            if let StmtKind::Function(decl) = &statement.kind {
                if !decl.type_params.is_empty() {
                    self.type_params(&decl.type_params, statement.span);
                    let generic = Rc::new(Generic {
                        at: Arc::as_ptr(decl) as usize,
                        id: decl_id(statement.span, &decl.name),
                        name: decl.name.clone(),
                        params: ast::param_names(&decl.type_params),
                        bounds: decl.type_params.iter().map(|param| param.bound.clone()).collect(),
                        asks: RefCell::new(None),
                        template: Template::Function(decl.clone()),
                        env: env.clone(),
                        span: statement.span,
                        above: above.clone(),
                        outline: RefCell::new(None),
                    });
                    self.declare(env, &decl.name, Ty::Generic(generic.clone()), false, statement.span);
                    self.generics.push(generic);
                    continue;
                }
                let function = FunctionInfo {
                    decl: decl.clone(),
                    env: env.clone(),
                    span: statement.span,
                    self_type: None,
                    state: RefCell::new(State::NotChecked),
                    above: above.clone(),
                    copy_note: None,
                };
                self.declare(env, &decl.name, Ty::Function(Rc::new(function)), false, statement.span);
            }
        }
    }

    /// May a case called `name`, of the enum called `owner`, be declared
    /// in `env`? A case is written by its name alone, `Dot` and not
    /// `Shape.Dot`, so the name can stand for one case only. If it is
    /// taken, this says so and gives false.
    fn case_name_free(&mut self, env: &Env, name: &str, owner: &str, span: Span) -> bool {
        let taken = env.variables.borrow().get(name).map(|variable| variable.ty.clone());
        let other = match &taken {
            Some(Ty::EnumValue(other) | Ty::CaseConstructor(other, _)) => other.name.clone(),
            Some(Ty::GenericCase(other, _)) => other.name.clone(),
            _ => return true,
        };
        let message = match other == owner {
            true => format!("the enum `{owner}` has two cases called `{name}`. Give one of them another name"),
            false => format!(
                "`{name}` is already a case of the enum `{other}`. A case is written by its name alone, \
                 so two enums in one file cannot both have a `{name}`. Give one of them another name"
            ),
        };
        self.error(message, span);
        false
    }

    /// Work out the fields of a class and note its methods. For a class
    /// written in the program this is part of `hoist`. For a copy of a
    /// generic class it happens when the copy is first asked for.
    fn fill_class(&mut self, decl: &ast::Class, info: &Rc<ClassInfo>, env: &Env, span: Span) {
        {
            let fields: Vec<Slot> =
                decl.fields.iter().map(|field| self.slot(field, &decl.name, env, span)).collect();
            // A field that holds a resource is refused in the words of 59.1
            // (`resource_class`).
            if let Some(field) = fields.iter().find(|field| field.ty.is_owned() && !info.owned && !self.holds_resource(&field.ty)) {
                // Objects of an ordinary class are shared. Each holder would
                // believe the owned value inside was still there.
                self.error(
                    format!(
                        "`{}` holds an owned value, so `{}` must be an `owned class` itself. \
                         A shared object cannot own anything",
                        field.name, decl.name
                    ),
                    span,
                );
            }
            self.types
                .classes
                .insert(decl as *const ast::Class as usize, fields.iter().map(|slot| kind(&slot.ty)).collect());
            self.types.widths.insert(
                decl as *const ast::Class as usize,
                fields
                    .iter()
                    .map(|slot| match &slot.ty {
                        Ty::Sized(width) => Some(*width),
                        _ => None,
                    })
                    .collect(),
            );
            *info.fields.borrow_mut() = fields;
            for method in &decl.methods {
                if !method.type_params.is_empty() {
                    // 58.7: it stays refused, and a function of the file
                    // that takes the object first does the same work.
                    // A copy of a generic class, or the generic checked on
                    // its own, is named after the generic, with its `[T]`.
                    let generic = match &info.origin {
                        Some((generic, _)) => Some(generic.clone()),
                        None => match env.get(&decl.name).map(|variable| variable.ty) {
                            Some(Ty::Generic(generic)) => Some(generic),
                            _ => None,
                        },
                    };
                    let (class_name, class_params) = match &generic {
                        Some(generic) => (generic.name.clone(), generic.params.clone()),
                        None => (decl.name.clone(), Vec::new()),
                    };
                    let mut all = class_params.clone();
                    all.extend(ast::param_names(&method.type_params));
                    let object = match class_params.is_empty() {
                        true => class_name.clone(),
                        false => format!("{class_name}[{}]", class_params.join(", ")),
                    };
                    self.error(
                        format!(
                            "the method `{0}` cannot have type parameters of its own yet. Write it as a function \
                             of the file that takes the object first: `function {0}[{1}]({2}: {object}, ...)`",
                            method.name,
                            all.join(", "),
                            class_name.to_lowercase()
                        ),
                        // On the method's own line, not the class's.
                        method.span,
                    );
                }
                // So that the one mistake is not reported again for each
                // `T` in the method, its names stand for "anything".
                let env = if method.type_params.is_empty() {
                    env.clone()
                } else {
                    let inner = Scope::child(env);
                    for param in &method.type_params {
                        inner.declare(&param.name, Ty::Alias(Box::new(Ty::Invalid)), false);
                    }
                    inner
                };
                let function = FunctionInfo {
                    decl: method.clone(),
                    env,
                    // A mistake in its first line is shown on that line,
                    // not on the line of the class.
                    span: method.span,
                    // A method of an owned class looks at the object it is
                    // called on: the caller goes on owning it.
                    self_type: Some(match info.owned {
                        true => Ty::Borrowed(Box::new(Ty::Instance(info.clone()))),
                        false => Ty::Instance(info.clone()),
                    }),
                    state: RefCell::new(State::NotChecked),
                    above: info.above.clone(),
                    copy_note: info.copy_note.clone(),
                };
                info.methods.borrow_mut().insert(method.name.clone(), Rc::new(function));
            }
        }
        // `class Circle is Shape`: which interfaces those are. Whether
        // the class has what they list is checked with its body.
        for written in &decl.is {
            let ast::Type::Named { name: interface_name, args } = written else {
                continue;
            };
            // What is written, `Shape` or `Source[int]`, is what each
            // message names.
            let name = &crate::printer::type_(written);
            let looked = env.get(interface_name).map(|variable| variable.ty);
            let found = match looked {
                // `is Source[int]`: a copy of a generic interface, made as
                // a copy of a generic class is (decision 58.6).
                Some(Ty::Generic(generic)) if matches!(&generic.template, Template::Class(template) if template.interface) => {
                    if args.is_empty() {
                        let example = vec!["int"; generic.params.len()].join(", ");
                        self.error(
                            format!(
                                "`{name}` has type parameters, so a class says which `{name}` it is: \
                                 `class {} is {name}[{example}]`",
                                decl.name
                            ),
                            span,
                        );
                        continue;
                    }
                    match self.resolve(written, env, span) {
                        Ty::Instance(copy) => Some(Ty::Class(copy)),
                        // A mistake in the types was reported, or the class
                        // is a generic checked on its own.
                        _ => continue,
                    }
                }
                Some(Ty::Class(interface)) if interface.interface && !args.is_empty() => {
                    self.error(
                        format!(
                            "`{interface_name}` has no type parameters, so it is written without `[ ]`: \
                             `class {} is {interface_name}`",
                            decl.name
                        ),
                        span,
                    );
                    continue;
                }
                looked => looked,
            };
            match found {
                Some(Ty::Class(interface)) if interface.interface => {
                    // An object with one owner cannot be where a shared one
                    // is wanted, nor the other way round: an `owned class`
                    // is an `owned interface`, and only it is (58.22). With
                    // no collector both are owned, as everything is.
                    if info.owned && !interface.owned {
                        self.error(
                            format!(
                                "`{0}` is an owned class, and `{name}` is not an `owned interface`: an object with one \
                                 owner cannot be where a shared one is wanted. Write `owned interface {interface_name}`, \
                                 or take `owned` off `{0}`",
                                decl.name
                            ),
                            span,
                        );
                    } else if !info.owned && interface.owned && !self.resource.as_ref().is_some_and(|resource| Rc::ptr_eq(resource, &interface)) {
                        self.error(
                            format!(
                                "`{0}` is not an owned class, and `{name}` is an `owned interface`, whose objects each \
                                 have one owner. Write `owned class {0}`, or take `owned` off `{interface_name}`",
                                decl.name
                            ),
                            span,
                        );
                    } else if !info.is.borrow().iter().any(|known| Rc::ptr_eq(known, &interface)) {
                        // A class that is the `Resource` that comes with
                        // Zeno and is not owned is told so by
                        // `resource_class`, in the words of 59.1.
                        info.is.borrow_mut().push(interface);
                    }
                }
                Some(Ty::Class(_) | Ty::Generic(_)) => {
                    self.error(
                        format!(
                            "`{name}` is a class, not an interface, so `{}` cannot be one. A class has no parent class \
                             in Zeno. Declare what the two share as an `interface`, and let both be it",
                            decl.name
                        ),
                        span,
                    );
                }
                Some(_) => {
                    self.error(format!("`{name}` is not an interface"), span);
                }
                // 58.3: a class cannot be `Ordered` until a class can say
                // what `<` means for its objects.
                None if name == "Ordered" => {
                    self.error(
                        format!(
                            "`{}` cannot be `Ordered` yet: `Ordered` is what `<` works on, numbers, quantities and \
                             text, and `<` between objects of a class comes with operators for a class",
                            decl.name
                        ),
                        span,
                    );
                }
                None => {
                    self.error(
                        format!("`{name}` is not declared. Declare it with `interface {name} {{ ... }}`"),
                        span,
                    );
                }
            }
        }
        self.resource_class(decl, info, span);
    }

    /// A parameter that takes a resource releases it when the function
    /// ends (59.1). One that only looks leaves it to its owner. Nothing
    /// else in the first line of a function may hold one.
    fn resource_signature(&mut self, decl: &ast::Function, params: &[Slot], returns: Option<&Ty>, span: Span) {
        for slot in params {
            if self.is_resource(&slot.ty) {
                self.types.resource_params.insert((decl.span.line, decl.span.col, slot.name.clone()));
            } else if self.holds_resource(&slot.ty) && !self.is_resource(&slot.ty.clone().looked_at()) {
                self.resource_inside(&format!("the parameter `{}`", slot.name), span);
            }
        }
        match returns {
            Some(Ty::Several(types)) if types.iter().any(|ty| self.holds_resource(ty)) => drop(self.error(RESOURCE_SEVERAL, span)),
            Some(ty) if self.holds_resource(ty) && !self.is_resource(ty) => {
                let name = if decl.name.is_empty() { "this function".to_string() } else { format!("`{}`", decl.name) };
                self.resource_inside(&format!("what {name} gives"), span);
            }
            _ => {}
        }
    }

    /// Where a value that holds a resource may stand (59.1): the
    /// refusal, if this is not such a place.
    fn resource_here(&mut self, expr: &Expr, ty: &Ty) -> Option<Ty> {
        if let Ty::Several(types) = ty {
            return types.iter().any(|ty| self.holds_resource(ty)).then(|| self.error(RESOURCE_SEVERAL, expr.span));
        }
        if self.holds_resource(ty) && !self.is_resource(&ty.clone().looked_at()) {
            return Some(self.resource_inside("this", expr.span));
        }
        let kept = self.receiving.contains(&(expr as *const Expr as usize));
        if self.is_resource(ty) && matches!(expr.kind, ExprKind::Call { .. }) && !kept {
            return Some(self.error(
                "this call gives a resource, and nothing keeps it, so it would be released at once. Keep it in a \
                 `let`, which releases it when its block ends: `let held = ...`",
                expr.span,
            ));
        }
        None
    }

    /// What 59.1 asks of a class that is `Resource`: one owner, and no
    /// field that holds one. (That its `release` raises nothing is asked
    /// where a class is held to its interfaces, `stands_for`.)
    fn resource_class(&mut self, decl: &ast::Class, info: &Rc<ClassInfo>, span: Span) {
        if let Some(field) = info.fields.borrow().iter().find(|field| self.holds_resource(&field.ty)).map(|field| field.name.clone()) {
            self.resource_inside(&format!("the field `{field}` of `{}`", decl.name), span);
        }
        if decl.interface || !self.is_resource(&Ty::Instance(info.clone())) {
            return;
        }
        if !decl.owned {
            self.error(
                format!(
                    "`{0}` is a `Resource`, so each of its objects has one owner, whose end releases it. \
                     Declare it `owned class {0} is Resource`",
                    decl.name
                ),
                span,
            );
        }
    }

    fn slot(&mut self, field: &ast::Field, owner: &str, env: &Env, span: Span) -> Slot {
        let what = format!("`{}` in `{owner}`", field.name);
        let id = (decl_id(span, owner), field.name.clone());
        Slot {
            name: field.name.clone(),
            ty: self.slot_type(&field.ty, &id, &what, env, span, false),
            has_default: field.default.is_some(),
            id: field.ty.is_none().then_some(id),
        }
    }

    /// The first part of `value` that the compiler cannot work out by
    /// itself, if there is one. What it can: numbers, text, a `const`,
    /// a case of an enum that carries nothing, a constant of a module
    /// (`math.pi`), operators on those, a list or a dict of them, and
    /// the conversions between kinds of number (`u8(n)`, `float(n)`).
    fn not_constant<'a>(&self, value: &'a Expr, env: &Env) -> Option<&'a Expr> {
        let all = |parts: &mut dyn Iterator<Item = &'a Expr>| {
            for part in parts {
                if let Some(found) = self.not_constant(part, env) {
                    return Some(found);
                }
            }
            None
        };
        match &value.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => None,
            ExprKind::Name(name) => match env.get(name) {
                Some(variable) if self.consts.contains(&variable.id) || variable.case.is_some() => None,
                _ => Some(value),
            },
            ExprKind::Give(_) => Some(value),
            ExprKind::Unit { value: inner, .. } | ExprKind::Unary { operand: inner, .. } => self.not_constant(inner, env),
            ExprKind::Binary { left, right, .. } | ExprKind::Index { object: left, index: right } => {
                all(&mut [&**left, &**right].into_iter())
            }
            ExprKind::Compare { first, rest } => all(&mut std::iter::once(&**first).chain(rest.iter().map(|(_, operand)| operand))),
            ExprKind::List(items) => all(&mut items.iter()),
            ExprKind::Dict(pairs) => all(&mut pairs.iter().flat_map(|(key, item)| [key, item])),
            ExprKind::FormatStr(parts) => all(&mut parts.iter().filter_map(|part| match part {
                FormatPart::Value { value, .. } => Some(value),
                FormatPart::Text(_) => None,
            })),
            // It changes the place it reads.
            ExprKind::Take(_) => Some(value),
            // `math.pi` and `Mode.Idle`: a name of a module or an enum.
            ExprKind::Field { object, .. } => match &object.kind {
                ExprKind::Name(name) if matches!(env.get(name), Some(Variable { ty: Ty::Module(_) | Ty::Enum(_), .. })) => None,
                _ => Some(value),
            },
            // `u8(n)`, `u8.wrap(n)`, `int(x)`, `float(n)`.
            ExprKind::Call { callee, args } => {
                let converts = match &callee.kind {
                    ExprKind::Name(name) => crate::ints::width(name).is_some() || matches!(name.as_str(), "int" | "float"),
                    ExprKind::Field { object, name, .. } => {
                        name == "wrap" && matches!(&object.kind, ExprKind::Name(of) if crate::ints::width(of).is_some())
                    }
                    _ => false,
                };
                match converts {
                    true => all(&mut args.iter().map(|arg| &arg.value)),
                    false => Some(value),
                }
            }
            ExprKind::ListComp { .. } | ExprKind::Function(_) | ExprKind::Several(_) => Some(value),
        }
    }

    /// Check one statement. Returns true if it always leaves the block.
    fn stmt(&mut self, statement: &Stmt, env: &Env) -> bool {
        let span = statement.span;
        self.touched.clear();
        self.owned.looking.clear();
        self.owned.emptied.clear();

        match &statement.kind {
            StmtKind::Let { name, ty, value, mutable } => {
                self.declaration(name, ty, value, *mutable, env, statement);
                false
            }
            StmtKind::Receive { declare, places, value } => {
                self.receive(*declare, places, value, env, span);
                false
            }
            StmtKind::Const { name, ty, value } => {
                // A `const` is fixed when the program is built (1.7), so
                // it is written from what the compiler can work out.
                if let Some(part) = self.not_constant(value, env) {
                    self.error(
                        "this is only known when the program runs, and a `const` is fixed when the program is built. \
                         A `const` is written from numbers, text, other `const`s and operators. \
                         For a value that is worked out once while the program runs, use `let`",
                        part.span,
                    );
                }
                self.declaration(name, ty, value, false, env, statement);
                if let Some(variable) = env.get_here(name) {
                    self.consts.insert(variable.id);
                }
                false
            }
            StmtKind::Assign { target, op, value } => {
                let (wanted, holds, field) = self.assignment_target(target, env);
                let given = match op {
                    // `x = v` keeps `v`. `x += v` only reads it.
                    None => {
                        let given = self.kept(value, &wanted, env);
                        self.adapted(written(value), value as *const Expr as usize, given, &wanted, value.span)
                    }
                    Some(op) => {
                        // `xs[next()] += 1` is `xs[next()] = xs[next()] + 1`,
                        // which would call `next` twice and could read one
                        // place and write another.
                        if let Some(call) = call_in(target) {
                            self.error(
                                format!(
                                    "`{0}=` reads this place and then writes it, so a call in the place would run twice. \
                                     Keep what the call gives in a `let` first, and use that name here",
                                    op.text()
                                ),
                                call.span,
                            );
                        }
                        let given = self.look(value, env);
                        let given = self.real_beside(given, &holds.clone().looked_at(), value);
                        // `xs += [4]` is `xs = xs + [4]`. On an owned list,
                        // the list that `+` makes takes the old one's place.
                        match (&wanted, self.binary(*op, &holds.looked_at(), &given, span)) {
                            // `count += 1` on a `u8` is `count = u8(count + 1)`:
                            // the sum is an int, and it is checked to fit as
                            // it is stored. `checked` rewrites the statement.
                            (Ty::Sized(width), Ty::Int) => {
                                self.types.fits.insert((span.line, span.col), *width);
                                wanted.clone()
                            }
                            (Ty::Owned(_), made) if owns_items(&made) => self.error(
                                "`+=` would put each item of the list on the right in a second list, and each has \
                                 one owner. Add the items one at a time with `append`",
                                span,
                            ),
                            (Ty::Owned(_), made @ Ty::List(_)) => Ty::Owned(Box::new(made)),
                            (_, made) => made,
                        }
                    }
                };
                if let Some(field) = field {
                    // A value stored in an untyped field is evidence of its type.
                    self.sees(&field, &given, value.span);
                }
                self.expect(&given, &wanted, "the new value", value.span);
                // What a variable holds from here on is what was just put
                // in it. If that is certainly a value, the variable is
                // known to be present; if it may be `none`, whatever an
                // earlier check said about it no longer holds.
                if let (ExprKind::Name(name), Ty::Optional(inner)) = (&target.kind, &wanted) {
                    if !given.is_loose() && fits(&given, inner) {
                        env.narrow(name, (**inner).clone());
                    } else {
                        env.widen(name);
                    }
                }
                // An owner that gave its value away holds one again.
                if let (ExprKind::Name(name), true) = (&target.kind, wanted.is_owned()) {
                    if let Some(variable) = env.get(name) {
                        self.owned.moved.remove(&variable.id);
                    }
                }
                false
            }
            StmtKind::Expr(expr) => {
                // An owned value made here and kept by nobody is freed on
                // the spot. The compiler needs to know to do that.
                if self.expr(expr, env).is_owned() {
                    self.types.discarded.insert(expr as *const Expr as usize);
                }
                false
            }

            // Declared by `hoist`. Their bodies are checked here, so that a
            // function nobody calls is still checked.
            StmtKind::Function(decl) => {
                if let Some(Variable { ty: Ty::Function(function), .. }) = env.get_here(&decl.name) {
                    self.signature(&function);
                }
                false
            }
            StmtKind::Class(decl) => {
                if let Some(Variable { ty: Ty::Class(class), .. }) = env.get_here(&decl.name) {
                    self.class_body(decl, &class, env);
                }
                false
            }
            StmtKind::Enum(_) => false,

            StmtKind::If { condition, then_block, else_block } => {
                self.condition(condition, env);
                let (when_true, when_false) = self.narrowings(condition, env);

                let then_scope = Scope::child(env);
                narrow_all(&then_scope, &when_true);
                let before = self.owned.moved.clone();
                let then_exits = self.statements(then_block, &then_scope);
                let then_moved = std::mem::replace(&mut self.owned.moved, before);

                let else_scope = Scope::child(env);
                narrow_all(&else_scope, &when_false);
                let else_exits = match else_block {
                    Some(block) => self.statements(block, &else_scope),
                    None => false,
                };
                // What is given away after the `if` is what either branch
                // gave away, leaving out a branch that never gets there.
                match (then_exits, else_exits) {
                    (true, false) => {}
                    (false, true) => self.owned.moved = then_moved,
                    _ => self.owned.moved.extend(then_moved),
                }

                // If one branch always leaves, what follows the `if` only
                // runs when the other branch's condition held:
                //     if x is none { return }
                //     ... x is not none from here on ...
                //
                // The branch that carries on may itself have changed the
                // variable, so what is passed on is what that branch still
                // knows at its end.
                if then_exits && !else_exits {
                    keep_known(env, &else_scope, &when_false);
                } else if else_exits && !then_exits {
                    keep_known(env, &then_scope, &when_true);
                } else if !then_exits {
                    // Both branches carry on. What they agree on at their
                    // ends is known afterwards:
                    //     if name is none { name = "anon" }
                    //     ... name is a str from here on, either way ...
                    for (name, _) in when_true.iter().chain(&when_false) {
                        if let (Some(then), Some(other)) = (then_scope.get(name), else_scope.get(name)) {
                            if then.declared.is_some() && other.declared.is_some() && same(&then.ty, &other.ty) {
                                env.narrow(name, then.ty);
                            }
                        }
                    }
                }
                then_exits && else_exits
            }
            // The rest of a block, after the `let` of a resource: more of
            // the same block, whose declarations its `hoist` has made.
            StmtKind::Scoped { body, .. } => {
                let mut exits = false;
                for statement in body {
                    exits |= self.stmt(statement, env);
                }
                exits
            }
            StmtKind::Unsafe(body) => {
                self.frame().unsafe_blocks += 1;
                let exits = self.block(body, env);
                self.frame().unsafe_blocks -= 1;
                exits
            }
            StmtKind::While { condition, body } => {
                // The condition runs again on every trip, so it is part of
                // the loop as far as owned values go.
                let entry = self.enter_loop();
                self.condition(condition, env);
                // Each trip through the body starts just after the
                // condition held, so what it says is known in there:
                // `while node is not none { ... node.next ... }`.
                let scope = Scope::child(env);
                forget_assigned(body, &scope);
                narrow_all(&scope, &self.narrowings(condition, &scope).0);
                self.frame().loops += 1;
                let exits = self.block(body, &scope);
                self.leave_loop(entry, exits);
                self.frame().loops -= 1;
                // `while true` with no `break` never finishes, so nothing follows it.
                condition.kind == ExprKind::Bool(true) && !has_break(body)
            }
            StmtKind::For { variable, second, iterable, body } => {
                // A reader that a call gives has no owner to close it at
                // the end of the loop: said below, in a `for`'s words.
                let made = matches!(iterable.kind, ExprKind::Call { .. });
                if made {
                    self.receiving.insert(iterable as *const Expr as usize);
                }
                let iterable_type = self.look(iterable, env);
                let reader = self.is_reader(&iterable_type);
                if made && self.is_resource(&iterable_type) {
                    self.error(
                        "a `for` cannot read from a reader that nothing keeps: it would be closed at once. \
                         Give it a name first, `let source = files.read_from(path)`, and then write `for line in source`",
                        iterable.span,
                    );
                } else if reader && second.is_some() {
                    self.error("a `for` over a `files.Reader` gives one line at a time, so it takes one name: `for line in source`", span);
                } else if reader {
                    self.types.readers.insert((span.line, span.col));
                }
                // With two names the first is the key, or the position,
                // and the second is what is there.
                let (first, item) = match second {
                    None if reader => (None, Ty::Str),
                    None => (None, self.item_type(&iterable_type, iterable.span)),
                    Some(_) => {
                        let (first, item) = self.pair_types(&iterable_type, iterable.span);
                        (Some(first), item)
                    }
                };
                let walks = self.walk(iterable, &item, env);
                let scope = Scope::child(env);
                forget_assigned(body, &scope);
                match (first, second) {
                    (Some(first), Some(second)) => {
                        self.loop_variable(&scope, variable, first, span);
                        self.loop_variable(&scope, second, item, span);
                    }
                    _ => self.loop_variable(&scope, variable, item, span),
                }
                self.frame().loops += 1;
                let entry = self.enter_loop();
                let exits = self.statements(body, &scope);
                self.leave_loop(entry, exits);
                self.frame().loops -= 1;
                self.owned.walked.truncate(walks);
                false
            }
            StmtKind::Match { subject, arms } => self.match_statement(subject, arms, env, span),
            StmtKind::Try { body, handlers } => self.try_statement(body, handlers, env, span),

            // A test is checked like any block at the top of a file. An
            // error that leaves it fails that test, and is nobody
            // else's to catch or to name (decision 10.16).
            StmtKind::Test { body, .. } => {
                self.raised.push(Vec::new());
                self.block(body, env);
                self.raised.pop();
                false
            }
            StmtKind::Assert(condition, message) => {
                self.condition(condition, env);
                if let Some(message) = message {
                    let ty = self.look(message, env);
                    if !fits(&ty, &Ty::Str) {
                        self.error(
                            format!(
                                "what an `assert` prints when it fails must be text, but this is {ty}. \
                                 Write `assert total == 6, f\"the total was {{total}}\"`"
                            ),
                            message.span,
                        );
                    }
                }
                false
            }
            // The block must raise that kind of error: a `try` with one
            // `except` that has nothing in it.
            StmtKind::AssertRaises { error, body } => {
                let handler = Handler { error: Some(error.clone()), binding: None, body: Vec::new() };
                self.try_statement(body, std::slice::from_ref(&handler), env, span);
                false
            }

            StmtKind::Return(value) => {
                let declared = self.frame().declared_return.clone();
                let ty = match value {
                    // `return a // b, a % b`: each is checked against its
                    // own place in what the function gives (decision 57).
                    Some(Expr { kind: ExprKind::Several(values), .. }) => {
                        let wanted = match &declared {
                            Some(Ty::Several(types)) if types.len() == values.len() => types.clone(),
                            Some(Ty::Several(types)) => {
                                self.error(
                                    format!(
                                        "this function gives {} values, and this `return` gives {}",
                                        types.len(),
                                        values.len()
                                    ),
                                    span,
                                );
                                vec![Ty::Invalid; values.len()]
                            }
                            Some(declared) if !declared.is_loose() => {
                                self.error(
                                    format!(
                                        "this function gives one value, {declared}, and this `return` gives {}. \
                                         A function that gives {} says so in its first line, as in `-> ({})`",
                                        values.len(),
                                        values.len(),
                                        vec!["int"; values.len()].join(", ")
                                    ),
                                    span,
                                );
                                vec![Ty::Invalid; values.len()]
                            }
                            _ => vec![Ty::Invalid; values.len()],
                        };
                        let mut types = Vec::new();
                        for (at, (value, wanted)) in values.iter().zip(&wanted).enumerate() {
                            let given = self.kept(value, wanted, env);
                            let given = self.adapted(written(value), value as *const Expr as usize, given, wanted, value.span);
                            self.expect(&given, wanted, &format!("the {} value returned", ordinal(at)), value.span);
                            types.push(given);
                        }
                        Ty::Several(types)
                    }
                    // The caller keeps what is returned.
                    Some(value) => {
                        let wanted = declared.as_ref().unwrap_or(&Ty::Invalid);
                        // `return divide(a, b)` passes on both values, in a
                        // function that gives as many, or says nothing.
                        self.several = matches!(declared, None | Some(Ty::Several(_)));
                        let given = self.kept(value, wanted, env);
                        self.several = false;
                        let given = self.adapted(written(value), value as *const Expr as usize, given, wanted, value.span);
                        match (wanted, &given) {
                            (Ty::Several(types), given) if !matches!(given, Ty::Several(_)) && !given.is_loose() => self.error(
                                format!(
                                    "this function gives {} values, and this is one. Give each, as in `return {}`",
                                    types.len(),
                                    letters(types.len())
                                ),
                                value.span,
                            ),
                            _ => {
                                self.expect(&given, wanted, "the returned value", value.span);
                                given
                            }
                        }
                    }
                    None => {
                        if let Some(declared) = &declared {
                            self.expect(&Ty::None, declared, "the returned value", span);
                        }
                        Ty::None
                    }
                };
                if self.frames.len() == 1 {
                    self.error("`return` outside a function", span);
                    return true;
                }
                self.frame().returns.push(ty);
                true
            }
            StmtKind::Raise(value) => {
                // Where everything has one owner, `raise` takes the
                // error, and the `except` that catches it receives it.
                let raised = match self.one_owner {
                    true => match self.take(value, env) {
                        Ty::Owned(inner) if matches!(*inner, Ty::Error(_)) => *inner,
                        ty => ty,
                    },
                    false => self.expr(value, env),
                };
                match raised {
                    // An error travels to wherever it is caught, and can be
                    // kept there for any length of time.
                    ty if ty.is_tied() && !self.one_owner => {
                        self.error(format!("a value that has one owner cannot be raised, and this is {ty}"), span);
                    }
                    Ty::Error(kind) => self.can_raise(&kind, span),
                    // `raises` and `except` name a class, and `Problem[int]`
                    // is not a name that can be written there.
                    Ty::Instance(class) if class.origin.is_some() => {
                        self.error(
                            format!("an object of a generic class cannot be raised yet, and this is a {}", class.name),
                            span,
                        );
                    }
                    Ty::Instance(class) => self.can_raise(&class.name, span),
                    // A mistake that has been reported already.
                    Ty::Invalid => {}
                    ty if ty.is_loose() => self.can_raise("Error", span),
                    ty => {
                        self.error(format!("only an error can be raised, and this is {ty}"), span);
                    }
                }
                true
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.frame().loops == 0 {
                    let word = if statement.kind == StmtKind::Break { "break" } else { "continue" };
                    self.error(format!("`{word}` outside a loop"), span);
                }
                // Remember what is given away by now: it still is, where
                // this jump lands.
                let moved = self.owned.moved.clone();
                if let Some((breaks, continues)) = self.owned.loops.last_mut() {
                    if statement.kind == StmtKind::Break { breaks.extend(moved) } else { continues.extend(moved) }
                }
                true
            }

            StmtKind::Import { module, names } => {
                let path = module.join(".");
                let module = match path.as_str() {
                    "math" => "math",
                    "time" => "time",
                    "board" => "board",
                    "files" => "files",
                    "program" => "program",
                    "json" => "json",
                    "web" => "web",
                    "python" => "python",
                    "node" => "node",
                    "java" => "java",
                    "ruby" => "ruby",
                    "php" => "php",
                    "lua" => "lua",
                    "csharp" => "csharp",
                    "c" => "c",
                    _ => {
                        self.error(
                            format!("there is no module `{path}`. {}", crate::builtins::MODULES),
                            span,
                        );
                        // Declare the names anyway, so each use is not a second error.
                        for name in names.iter().chain(module.last().filter(|_| names.is_empty())) {
                            env.declare(name, Ty::Invalid, false);
                        }
                        return false;
                    }
                };
                if names.is_empty() {
                    self.declare(env, module, Ty::Module(module), false, span);
                }
                for name in names {
                    let ty = match self.member(module, name, span) {
                        Some(ty) => ty,
                        None => self.error(no_member(module, name), span),
                    };
                    self.declare(env, name, ty, false, span);
                }
                false
            }
        }
    }

    /// `let`, `var` and `const`.
    fn declaration(
        &mut self,
        name: &str,
        annotation: &Option<ast::Type>,
        value: &Expr,
        mutable: bool,
        env: &Env,
        statement: &Stmt,
    ) {
        let span = statement.span;
        // The variable keeps the value. If the value is an owned one held
        // by another variable, that one gives it up.
        let wanted = annotation.as_ref().map(|annotation| self.resolve(annotation, env, span));
        if let Some(wanted) = &wanted {
            self.expecting(value, wanted);
        }
        let wanted_item = match &wanted {
            Some(Ty::List(item) | Ty::Fixed(item, _)) => Some((**item).clone()),
            Some(Ty::Owned(inner)) => match &**inner {
                Ty::List(item) | Ty::Fixed(item, _) => Some((**item).clone()),
                _ => None,
            },
            _ => None,
        };
        // A list written out in full, for a place that says what it
        // holds, is checked item by item further down. Each item is
        // looked at once: one that names an owner gives its value away.
        let item_by_item = matches!(value.kind, ExprKind::List(_)) && wanted_item.is_some();
        let given = if item_by_item { Ty::Invalid } else { self.take(value, env) };
        // `var f = double`: a `var` is there to be given other values, so
        // it gets the type of the function, `function(int) -> int`, and
        // not "the function `double`", which a `let` gets
        // (decision 5.19l).
        let wanted = match (wanted, &given) {
            (None, Ty::Function(function)) if mutable && function.self_type.is_none() && !function.decl.name.is_empty() => {
                let signature = self.signature(function);
                Some(Ty::Callable(Rc::new(CallableType {
                    params: signature.params.iter().map(|slot| slot.ty.clone()).collect(),
                    returns: signature.returns.clone(),
                    raises: self.raises_in_type(&signature.raises),
                    owned: self.one_owner,
                })))
            }
            (wanted, _) => wanted,
        };
        let ty = match wanted {
            Some(wanted) => {
                let wanted_item = wanted_item.as_ref();
                match (&value.kind, wanted_item) {
                    // A list written out in full is checked item by item, so
                    // that `[1, "two"]` is caught rather than passing as a
                    // list of mixed, unknown things.
                    (ExprKind::List(items), Some(wanted_item)) => {
                        if matches!(wanted, Ty::Owned(_)) {
                            self.types.fresh.insert(value as *const Expr as usize);
                        }
                        if asks_more(wanted_item) {
                            self.types.items.insert(value as *const Expr as usize, kind(wanted_item));
                        }
                        if is_bytes(&wanted) {
                            self.types.bytes.insert(value as *const Expr as usize);
                        }
                        if let Some(length) = fixed_length(&wanted) {
                            if items.len() as u64 != length {
                                self.error(too_many_or_few(items.len() as i128, &wanted, length), value.span);
                            }
                        }
                        for item in items {
                            // The list keeps the item.
                            let item_type = self.kept(item, wanted_item, env);
                            let item_type = self.adapted(written(item), item as *const Expr as usize, item_type, wanted_item, item.span);
                            if !fits(&item_type, wanted_item) {
                                self.error(
                                    format!("`{name}` should be {wanted}, but this item is {item_type}"),
                                    item.span,
                                );
                            } else {
                                self.arrives(&item_type, wanted_item, &format!("`{name}`"), item.span);
                            }
                        }
                    }
                    _ => {
                        // `[none] * 4` where a `list[int or none]` is wanted:
                        // the list written out is made as that list.
                        if let (ExprKind::Binary { op: BinaryOp::Mul, left, .. }, Some(wanted_item)) = (&value.kind, wanted_item) {
                            if matches!(left.kind, ExprKind::List(_)) && asks_more(wanted_item) {
                                self.types.items.insert(&**left as *const Expr as usize, kind(wanted_item));
                            }
                        }
                        let given = self.fresh_list(value, given, &wanted);
                        let given = self.adapted(written(value), value as *const Expr as usize, given, &wanted, value.span);
                        self.expect(&given, &wanted, &format!("`{name}`"), value.span)
                    }
                }
                wanted
            }
            None if self.strict_at(span) && given.has_dynamic() => self.error(
                format!(
                    "strict mode: the type of `{name}` cannot be worked out. Give it one, as in `{name}: {}`",
                    match given.clone().looked_at() {
                        Ty::Dict(..) => "dict[str, int]",
                        _ => "list[int]",
                    }
                ),
                span,
            ),
            None => given,
        };
        if ty.is_owned() {
            self.types.owners.insert(statement as *const Stmt as usize);
        }
        // The `let` of a resource releases it when its block ends (59.1).
        if self.is_resource(&ty) {
            match mutable {
                true => drop(self.error(
                    format!(
                        "`{name}` holds a resource, which is released when its owner ends, so the variable cannot \
                         be given another value. Declare it with `let`"
                    ),
                    span,
                )),
                false if self.page && env.top => drop(self.error(
                    format!(
                        "a page stays open after the program's last line, so the top of its file never ends, and \
                         `{name}` would never be released. Make it inside the function that uses it"
                    ),
                    span,
                )),
                false => drop(self.types.resource_lets.insert((span.line, span.col))),
            }
        } else if self.holds_resource(&ty) && !ty.is_look() {
            self.resource_inside(&format!("`{name}`"), span);
        }
        self.declare(env, name, ty, mutable, span);
    }

    /// `let q, r = divide(7, 2)`, `var a, b = 1, 2`, `low, high = high,
    /// low` (decision 57). The places that exist are looked at first, for
    /// what each may hold; then all of the right, left to right; then the
    /// places are filled, left to right.
    fn receive(&mut self, declare: Option<bool>, places: &[Place], value: &Expr, env: &Env, span: Span) {
        // `_` drops a value only where no variable is called `_`, which
        // 0.7.0 allowed: there it would be the variable, and say nothing.
        if places.iter().any(|place| matches!(place, Place::Drop)) && env.get("_").is_some() {
            self.error(
                "`_` on the left of `=` drops a value, but here `_` is a variable, which would be given it instead. \
                 Give the variable another name",
                span,
            );
        }
        // A plain name given two values at once: which one it keeps would
        // be a rule nobody can see. (Items, `xs[i], xs[j]`, are told apart
        // only as the program runs, and are filled left to right.)
        if declare.is_none() {
            let mut seen = HashSet::new();
            for place in places {
                if let Place::Target(Expr { kind: ExprKind::Name(name), span: at }) = place {
                    if !seen.insert(name.as_str()) {
                        self.error(format!("`{name}` is on the left of `=` twice. Each place receives one value"), *at);
                    }
                }
            }
        }
        let mut wanted = Vec::new();
        let mut fields = Vec::new();
        for place in places {
            let (ty, field) = match place {
                Place::Name { ty: Some(ty), .. } => (Some(self.resolve(ty, env, span)), None),
                Place::Name { ty: None, .. } | Place::Drop => (None, None),
                Place::Target(target) => {
                    let (ty, _, field) = self.assignment_target(target, env);
                    (Some(ty), field)
                }
            };
            wanted.push(ty);
            fields.push(field);
        }
        let count = places.len();
        let given: Vec<Ty> = match &value.kind {
            ExprKind::Several(values) => {
                if values.len() != count {
                    self.error(
                        format!(
                            "there are {} on the left of `=` and {} on the right. Each place receives one",
                            counted(count, "place"),
                            counted(values.len(), "value")
                        ),
                        span,
                    );
                }
                let mut given = Vec::new();
                for (at, value) in values.iter().enumerate() {
                    let ty = match wanted.get(at).cloned().flatten() {
                        Some(wanted) => {
                            let ty = self.kept(value, &wanted, env);
                            self.adapted(written(value), value as *const Expr as usize, ty, &wanted, value.span)
                        }
                        None => self.take(value, env),
                    };
                    // `let a, b = hold("x"), 1`: the resource would have
                    // no `let` of its own to release it (59.2).
                    if self.holds_resource(&ty) {
                        self.error(RESOURCE_SEVERAL, value.span);
                    }
                    given.push(ty);
                }
                given.resize(count, Ty::Invalid);
                given
            }
            _ => {
                self.several = true;
                let ty = self.take(value, env);
                self.several = false;
                match ty {
                    Ty::Several(types) if types.len() == count => types,
                    Ty::Several(types) => {
                        let who = match &value.kind {
                            ExprKind::Call { callee, .. } => match &callee.kind {
                                ExprKind::Name(name) | ExprKind::Field { name, .. } => format!("`{name}`"),
                                _ => "this call".to_string(),
                            },
                            _ => "this".to_string(),
                        };
                        self.error(
                            format!(
                                "{who} gives {} values, and there are {} on the left of `=`. \
                                 Receive each, and write `_` for one that is not wanted",
                                types.len(),
                                counted(count, "place")
                            ),
                            span,
                        );
                        vec![Ty::Invalid; count]
                    }
                    ty if ty.is_loose() => vec![ty; count],
                    ty => {
                        self.error(
                            format!(
                                "there are {} on the left of `=`, and the right gives one value, {ty}. \
                                 Give {count}, as in `{} = {}`, or call a function that gives {count}",
                                counted(count, "place"),
                                letters(count),
                                (1..=count).map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
                            ),
                            value.span,
                        );
                        vec![Ty::Invalid; count]
                    }
                }
            }
        };
        for (((place, wanted), field), given) in places.iter().zip(wanted).zip(fields).zip(given) {
            match place {
                Place::Drop => {
                    if given.is_owned() {
                        self.types.discarded.insert(place as *const Place as usize);
                    }
                }
                Place::Name { name, .. } => {
                    let ty = match wanted {
                        Some(wanted) => {
                            self.expect(&given, &wanted, &format!("`{name}`"), span);
                            wanted
                        }
                        None if self.strict_at(span) && given.has_dynamic() => self.error(
                            format!("strict mode: the type of `{name}` cannot be worked out. Give it one, as in `{name}: int`"),
                            span,
                        ),
                        None => given,
                    };
                    if ty.is_owned() {
                        self.types.owners.insert(place as *const Place as usize);
                    }
                    self.declare(env, name, ty, declare == Some(true), span);
                }
                Place::Target(target) => {
                    let wanted = wanted.unwrap_or(Ty::Invalid);
                    if let Some(field) = field {
                        self.sees(&field, &given, target.span);
                    }
                    self.expect(&given, &wanted, "the new value", target.span);
                    if let (ExprKind::Name(name), Ty::Optional(inner)) = (&target.kind, &wanted) {
                        if !given.is_loose() && fits(&given, inner) {
                            env.narrow(name, (**inner).clone());
                        } else {
                            env.widen(name);
                        }
                    }
                    if let (ExprKind::Name(name), true) = (&target.kind, wanted.is_owned()) {
                        if let Some(variable) = env.get(name) {
                            self.owned.moved.remove(&variable.id);
                        }
                    }
                }
            }
        }
    }

    /// The type that may be assigned to `target`, after checking that it can
    /// be assigned to at all. For a field of an object, the field is
    /// returned as well.
    /// What can be stored in `target`, what it holds at this point (which
    /// is narrower after a check such as `is not none`), and the field it
    /// is, if it is one.
    fn assignment_target(&mut self, target: &Expr, env: &Env) -> (Ty, Ty, Option<Slot>) {
        let span = target.span;
        if let ExprKind::Name(name) = &target.kind {
            if let Some(variable) = env.get(name) {
                self.used(name, &variable, span);
            }
        }
        let ty = match &target.kind {
            ExprKind::Name(name) => match (env.get(name), self.too_early(name, env.get(name).as_ref(), env, span)) {
                (_, Some(mistake)) => mistake,
                (None, None) => self.error(
                    format!("`{name}` is not declared. Create it with `let` or `var` first"),
                    span,
                ),
                (Some(variable), None) if !variable.mutable => self.error(
                    format!("`{name}` cannot be changed. Declare it with `var` to allow that"),
                    span,
                ),
                (Some(variable), None) if self.one_owner && variable.function != env.function && !variable.top => {
                    self.error(shared(name), span)
                }
                (Some(variable), None) if variable.ty.is_owned() && variable.function != env.function => {
                    self.error(elsewhere(name, false), span)
                }
                // A new value frees the old one, items and all, and the
                // loop is still looking at one of those items.
                (Some(variable), None) if variable.ty.is_owned() && self.being_walked(target, env).is_some() => {
                    let (_, who, until) = self.being_walked(target, env).expect("it was there a line ago");
                    self.error(format!("{who} `{name}` cannot be given a new value {until}"), span)
                }
                (Some(variable), None) => {
                    let Some(declared) = variable.declared else {
                        return (variable.ty.clone(), variable.ty, None);
                    };
                    // `x += 1` reads `x` as well as writing it.
                    self.types.narrowed.insert(target as *const Expr as usize, kind(&variable.ty));
                    return (declared, variable.ty, None);
                }
            },
            ExprKind::Field { object, name, at } => match self.look(object, env) {
                Ty::Instance(class) => {
                    let field = class.fields.borrow().iter().find(|f| f.name == *name).cloned();
                    if let Some(field) = &field {
                        self.field_used(name, &field.ty, *at);
                    }
                    if self.kept_private(&class, name, span) {
                        return (Ty::Invalid, Ty::Invalid, None);
                    }
                    match field {
                        // Putting a new owned value in a field frees the
                        // old one. Somebody who was lent the object may be
                        // looking at the old one right now, so only the
                        // owner, who knows nobody is, may do it.
                        Some(field) if field.ty.is_owned() => {
                            let does = format!("replace what `{name}` holds");
                            match root(object).is_none() || self.takes_out(object, 0, &does, env, span).is_some() {
                                true => return (field.ty.clone(), field.ty.clone(), Some(field)),
                                false => Ty::Invalid,
                            }
                        }
                        Some(field) => return (field.ty.clone(), field.ty.clone(), Some(field)),
                        None => self.error(format!("`{}` has no field `{name}`", class.name), span),
                    }
                }
                // `title.text = "Hello"`: a change to the page.
                Ty::Element if crate::builtins::ELEMENT_FIELDS.contains(&name.as_str()) => return (Ty::Str, Ty::Str, None),
                Ty::Element => self.error(
                    format!("an Element has no `{name}` to give a new value. It has `text` and `value`"),
                    span,
                ),
                Ty::Event => self.error(
                    format!("an Event says what happened, so `{name}` cannot be given a new value"),
                    span,
                ),
                ty if ty.is_loose() => ty,
                ty => self.error(format!("a field cannot be set on {ty}"), span),
            },
            ExprKind::Index { object: list, index } => {
                let (object, length) = self.look_fixed(list, env);
                self.index_inside(index, length);
                let index_type = self.look(index, env);
                if let Ty::Dict(key, value) = &object {
                    // `ages["Ada"] = 36` puts the key in, or gives it a
                    // new value if it is there already.
                    self.expect(&index_type.widened(), key, "a key of this dict", index.span);
                    // A value that was there under the key is freed, so
                    // this is held to the rules of taking out (56.12).
                    if self.one_owner && value.is_owned() && root(list).is_some() && self.takes_out(list, 0, "replace a value of this dict", env, span).is_none() {
                        return (Ty::Invalid, Ty::Invalid, None);
                    }
                    return ((**value).clone(), (**value).clone(), None);
                }
                // What is not known may be a dict, whose keys can be text.
                if !object.is_loose() {
                    self.expect(&index_type, &Ty::Int, "an index", index.span);
                }
                match object {
                    // The item that was there is freed. As for a field.
                    Ty::List(item) if item.is_owned() => {
                        match root(list).is_none() || self.takes_out(list, 0, "replace an item of this list", env, span).is_some() {
                            true => *item,
                            false => Ty::Invalid,
                        }
                    }
                    Ty::List(item) => *item,
                    Ty::Pointer(pointee) => {
                        self.needs_unsafe("writing through a pointer", span);
                        pointee.ty()
                    }
                    ty if ty.is_loose() => ty,
                    ty => self.error(format!("an item cannot be set on {ty}"), span),
                }
            }
            _ => self.error("this cannot be assigned to", span),
        };
        (ty.clone(), ty, None)
    }

    fn condition(&mut self, condition: &Expr, env: &Env) {
        let ty = self.look(condition, env);
        if !fits(&ty, &Ty::Bool) {
            self.error(format!("a condition must be true or false, but this is {ty}"), condition.span);
        }
    }

    /// The type of each item when looping over a value of type `ty`.
    fn item_type(&mut self, ty: &Ty, span: Span) -> Ty {
        match ty {
            // An item that has one owner stays in its list. The loop is
            // lent it.
            Ty::List(item) if item.is_owned() => lent(item),
            Ty::List(item) => (**item).clone(),
            // A loop over a dict is given its keys, in the order they
            // were put in.
            Ty::Dict(key, _) => (**key).clone(),
            Ty::Str => Ty::Str,
            ty if ty.is_loose() => ty.clone(),
            Ty::Pointer(_) => self.error(
                "a pointer does not know how many items it points at, so it cannot be looped over. \
                 Loop over `range(count)` and read item `i` inside `unsafe { }`",
                span,
            ),
            ty => self.error(format!("{ty} cannot be looped over"), span),
        }
    }

    /// What the two names of `for key, value in ages` are given, when
    /// looping over a value of type `ty`: each key of a dict and what is
    /// under it, or each position of a list or a str and what is there.
    /// Either way the second is what `ages[key]` would be.
    fn pair_types(&mut self, ty: &Ty, span: Span) -> (Ty, Ty) {
        match ty {
            Ty::Dict(key, value) if value.is_owned() => ((**key).clone(), lent(value)),
            Ty::Dict(key, value) => ((**key).clone(), (**value).clone()),
            ty if ty.is_loose() => (ty.clone(), ty.clone()),
            ty => (Ty::Int, self.item_type(ty, span)),
        }
    }

    /// What a condition tells us about the variables in it. Returns the
    /// narrower types that hold when it is true, and when it is false.
    ///
    /// `x is not none` means `x` is a plain `T` when true and `none` when false.
    fn narrowings(&self, condition: &Expr, env: &Env) -> (Vec<(String, Ty)>, Vec<(String, Ty)>) {
        match &condition.kind {
            ExprKind::Binary { op: op @ (BinaryOp::Is | BinaryOp::IsNot), left, .. } => {
                let ExprKind::Name(name) = &left.kind else {
                    return (Vec::new(), Vec::new());
                };
                let Some(Variable { ty: Ty::Optional(inner), .. }) = env.get(name) else {
                    return (Vec::new(), Vec::new());
                };
                let present = vec![(name.clone(), *inner)];
                let absent = vec![(name.clone(), Ty::None)];
                if *op == BinaryOp::Is {
                    (absent, present)
                } else {
                    (present, absent)
                }
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let (when_true, when_false) = self.narrowings(operand, env);
                (when_false, when_true)
            }
            // `a and b` being true means both were true.
            ExprKind::Binary { op: BinaryOp::And, left, right } => {
                let mut when_true = self.narrowings(left, env).0;
                when_true.extend(self.narrowings(right, env).0);
                (when_true, Vec::new())
            }
            // `a or b` being false means both were false.
            ExprKind::Binary { op: BinaryOp::Or, left, right } => {
                let mut when_false = self.narrowings(left, env).1;
                when_false.extend(self.narrowings(right, env).1);
                (Vec::new(), when_false)
            }
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn class_body(&mut self, decl: &ast::Class, class: &Rc<ClassInfo>, env: &Env) {
        let fields = class.fields.borrow().clone();
        for (field, slot) in decl.fields.iter().zip(&fields) {
            if let Some(default) = &field.default {
                let given = self.kept(default, &slot.ty, env);
                let given = self.adapted(written(default), Arc::as_ptr(default) as usize, given, &slot.ty, default.span);
                self.sees(slot, &given, default.span);
                self.expect(&given, &slot.ty, &format!("`{}`", field.name), default.span);
            }
        }
        let methods: Vec<Rc<FunctionInfo>> = class.methods.borrow().values().cloned().collect();
        for method in methods {
            if method.decl.params.is_empty() {
                self.error(
                    format!("the method `{}` needs `self` as its first parameter", method.decl.name),
                    method.span,
                );
            }
            self.signature(&method);
        }
        self.stands_for(class);
    }

    /// `class Circle is Shape`: the class must have every function that
    /// `Shape` lists, written the way `Shape` writes it. Whoever calls
    /// `shape.area()` has only the interface to go by.
    fn stands_for(&mut self, class: &Rc<ClassInfo>) {
        let span = Span { line: class.id.0, col: class.id.1 };
        let interfaces = class.is.borrow().clone();
        for interface in interfaces {
            let mut asked: Vec<Rc<FunctionInfo>> = interface.methods.borrow().values().cloned().collect();
            asked.sort_by(|a, b| a.decl.name.cmp(&b.decl.name));
            for asked in asked {
                let want = self.signature(&asked);
                let name = &asked.decl.name;
                let method = class.methods.borrow().get(name).cloned();
                let Some(method) = method else {
                    self.error(
                        format!(
                            "`{0}` says it is a `{1}`, and a `{1}` has `{2}`. Give `{0}` that function",
                            class.name,
                            interface.name,
                            first_line(&want)
                        ),
                        span,
                    );
                    continue;
                };
                if class.private.contains(name) {
                    self.error(
                        format!(
                            "`{name}` of `{}` is private, and `{}` lets every file call it. Take the word `private` off it",
                            class.name, interface.name
                        ),
                        span,
                    );
                    continue;
                }
                let got = self.signature(&method);
                let agree = got.params.len() == want.params.len()
                    && got.params.iter().zip(&want.params).skip(1).all(|(got, want)| got.name == want.name && same(&got.ty, &want.ty))
                    && same(&got.returns, &want.returns);
                if !agree {
                    self.error(
                        format!(
                            "`{name}` of `{}` is `{}`, and `{}` asks for `{}`. Write the two the same",
                            class.name,
                            first_line(&got),
                            interface.name,
                            first_line(&want)
                        ),
                        span,
                    );
                    continue;
                }
                self.same_defaults(class, &interface, &method, &asked);
                // A `release` has nowhere to send an error (59.1).
                if !got.raises.is_empty() && self.resource.as_ref().is_some_and(|resource| Rc::ptr_eq(resource, &interface)) {
                    self.error(
                        "`release` runs when the owner of its object ends, where an error has nowhere to go, so it \
                         cannot raise. Catch the error inside `release`",
                        span,
                    );
                    continue;
                }
                for kind in &got.raises {
                    if !want.raises.iter().any(|said| said == kind || said == "Error") {
                        self.error(
                            format!(
                                "`{name}` of `{0}` can raise {kind}, and `{name}` of `{1}` does not say so. Whoever calls \
                                 it through a `{1}` would not know. Add `raises {kind}` to the function in `{1}`, \
                                 or catch the error in `{0}`",
                                class.name, interface.name
                            ),
                            span,
                        );
                    }
                }
                // A call through the interface is held to the guards of
                // taking out from here on (58.21).
                if got.takes_out {
                    self.taking.entry((interface.id.clone(), name.clone())).or_insert_with(|| class.name.clone());
                }
            }
        }
    }

    /// Where the function of the interface writes a default, the class
    /// writes the same value (decision 58.20). Where it writes none, the
    /// class may have its own, as before: a call through the interface
    /// then gives every argument.
    fn same_defaults(&mut self, class: &ClassInfo, interface: &ClassInfo, method: &FunctionInfo, asked: &FunctionInfo) {
        let span = Span { line: class.id.0, col: class.id.1 };
        let name = &asked.decl.name;
        let line = |at: Span| match at.file() == span.file() {
            true => format!("line {}", at.line_in_file()),
            false => format!("line {} of the file that declares `{}`", at.line_in_file(), interface.name),
        };
        for (got, want) in method.decl.params.iter().zip(&asked.decl.params).skip(1) {
            let Some(wanted) = &want.default else { continue };
            // One that is not a plain value was refused where it is written.
            let Some(value) = self.plain(wanted, &asked.env) else { continue };
            let message = match &got.default {
                None => format!(
                    "`{}` gives `{}` of `{name}` the default `{}`, on {}, and `{name}` of `{}` gives it none. \
                     Write the same default in `{}`, so that a call through a `{}` and a call on a `{}` mean the same",
                    interface.name,
                    want.name,
                    shown(wanted),
                    line(wanted.span),
                    class.name,
                    class.name,
                    interface.name,
                    class.name
                ),
                Some(given) if self.plain(given, &method.env).is_some_and(|given| given.same(&value)) => continue,
                Some(given) => format!(
                    "`{name}` of `{}` gives `{}` the default `{}`, on line {}, and `{}` gives it `{}`, on {}. \
                     Write the same default in both, so that a call through a `{}` and a call on a `{}` mean the same",
                    class.name,
                    want.name,
                    shown(given),
                    given.span.line_in_file(),
                    interface.name,
                    shown(wanted),
                    line(wanted.span),
                    interface.name,
                    class.name
                ),
            };
            self.error(message, span);
        }
    }

    /// For a function of an interface that some class that is the
    /// interface takes out for: that class, and the interface (58.21).
    fn taken_through(&self, function: &FunctionInfo) -> Option<(String, String)> {
        let Some(Ty::Instance(interface)) = function.self_type.clone().map(Ty::looked_at) else {
            return None;
        };
        if !interface.interface {
            return None;
        }
        let key = (interface.id.clone(), function.decl.name.clone());
        let class = self.taken.get(&key).or_else(|| self.taking.get(&key))?;
        Some((class.clone(), interface.name.clone()))
    }

    /// The plain value a default of an interface's function is, if it is
    /// one (decision 58.20).
    fn plain(&self, value: &Expr, env: &Env) -> Option<Plain> {
        Some(match &value.kind {
            ExprKind::Int(n) => Plain::Number(*n as i128),
            ExprKind::Float(x) => Plain::Real(*x),
            ExprKind::Unary { op: UnaryOp::Neg, operand } => match self.plain(operand, env)? {
                Plain::Number(n) => Plain::Number(-n),
                Plain::Real(x) => Plain::Real(-x),
                Plain::Quantity(x, unit) => Plain::Quantity(-x, unit),
                _ => return None,
            },
            ExprKind::Str(text) => Plain::Text(text.clone()),
            ExprKind::Bool(truth) => Plain::Truth(*truth),
            ExprKind::None => Plain::None,
            ExprKind::Unit { value, unit } => match self.plain(value, env)? {
                Plain::Number(n) => Plain::Quantity(n as f64, unit.clone()),
                Plain::Real(x) => Plain::Quantity(x, unit.clone()),
                _ => return None,
            },
            // `Idle`, a case that carries nothing.
            ExprKind::Name(name) => match env.get(name)? {
                Variable { case: Some(_), ty: Ty::EnumValue(_), .. } => Plain::Case(name.clone()),
                _ => return None,
            },
            // `Mode.Idle`.
            ExprKind::Field { object, name, .. } => match &object.kind {
                ExprKind::Name(of) => match env.get(of)?.ty {
                    Ty::Enum(info) if info.cases.borrow().iter().any(|case| case.name == *name && case.fields.is_empty()) => {
                        Plain::Case(name.clone())
                    }
                    _ => return None,
                },
                _ => return None,
            },
            _ => return None,
        })
    }

    fn match_statement(&mut self, subject: &Expr, arms: &[Arm], env: &Env, span: Span) -> bool {
        // A `match` looks at its value where it is. If the value has one
        // owner, a name that a `case` gives to it, or to a part of it
        // that has one owner, is a look too.
        let subject_type = self.expr(subject, env);
        let look = subject_type.is_tied();
        let subject_type = self.looked(subject_type, subject);
        // The owner whose value the names look into, if a variable is.
        // (In a method, `self` counts: the method may take out what its
        // object holds, so its loops and cases have to be known too.)
        let in_method = self.frame().takes_self.is_some();
        let owner = root(subject)
            .and_then(|name| env.get(name).map(|variable| (name, variable)))
            .filter(|(name, variable)| (variable.ty.is_owned() || (in_method && *name == "self")) && variable.case.is_none())
            .map(|(_, variable)| variable.id);
        let mut all_exit = true;
        // What is given away after the `match` is what any case that gets
        // to its end gave away.
        let before = self.owned.moved.clone();
        let mut after: Option<Moved> = None;
        // Does some case accept anything at all?
        let mut catch_all = false;
        // For an enum, the cases that are fully handled.
        let mut covered = Vec::new();
        // Once a `case none` has gone by, the cases after it know that
        // the value is there, as the `else` of `if x is none` does
        // (decision 5.19h). `remaining` is what is left to match, and
        // `variable` is the one being matched, if a variable is.
        let mut remaining = subject_type.clone();
        let variable = match &subject.kind {
            ExprKind::Name(name) => match env.get(name) {
                Some(Variable { ty: Ty::Optional(inner), .. }) => Some((name.clone(), *inner)),
                _ => None,
            },
            _ => None,
        };
        let mut none_gone = false;
        // After the `match` the variable is known to hold something if
        // every `case none` left, and no case that carries on changed that.
        let mut known_after = true;

        for arm in arms {
            let scope = Scope::child(env);
            if let (true, Some((name, inner))) = (none_gone, &variable) {
                scope.narrow(name, inner.clone());
            }
            let is_none = matches!(&arm.pattern, Pattern::Literal(literal) if matches!(literal.kind, ExprKind::None));
            let mut looks = false;
            // A name for the whole value is declared as what the value
            // is, and then narrowed, exactly as the variable itself is.
            // The compilers store `T or none` and `T` differently, and
            // this is how they are told which one each use reads.
            let named = match &arm.pattern {
                Pattern::Name(name) if none_gone => Some(name),
                _ => None,
            };
            let against = if named.is_some() { &subject_type } else { &remaining };
            match self.pattern(&arm.pattern, against, look, &mut looks, &scope, span) {
                Covers::Everything => catch_all = true,
                Covers::Case(index) => covered.push(index),
                Covers::Some => {}
            }
            if let Some(Variable { ty: Ty::Optional(inner), .. }) = named.and_then(|name| scope.get_here(name)) {
                scope.narrow(named.expect("a name"), *inner);
            }
            // While a name looks into the owner's value, the owner must
            // leave that value where it is.
            let walks = self.owned.walked.len();
            if let (true, Some(owner)) = (looks, owner) {
                self.owned.walked.push((owner, subject.span.line_in_file(), Looking::Case));
            }
            let exits = self.statements(&arm.body, &scope);
            self.owned.walked.truncate(walks);
            all_exit &= exits;
            let moved = std::mem::replace(&mut self.owned.moved, before.clone());
            if !exits {
                after.get_or_insert_with(HashMap::new).extend(moved);
            }
            match (&remaining, is_none) {
                (Ty::Optional(inner), true) => {
                    remaining = (**inner).clone();
                    none_gone = true;
                    known_after &= exits;
                }
                _ if exits => {}
                // A case that carries on: does it still know, at its
                // end, that the variable holds something?
                _ => {
                    known_after &= none_gone
                        && variable.as_ref().is_some_and(|(name, _)| {
                            matches!(scope.get(name), Some(Variable { ty, declared: Some(_), .. }) if !matches!(ty, Ty::Optional(_) | Ty::None))
                        });
                }
            }
        }
        self.owned.moved.extend(after.unwrap_or_default());
        if let (true, true, Some((name, inner))) = (none_gone, known_after, variable) {
            env.narrow(&name, inner);
        }

        // An enum that may be `none` is complete once `none` and every
        // case of the enum have a `case`.
        let (of_enum, none_left) = match &remaining {
            Ty::Optional(inner) => (&**inner, true),
            other => (other, false),
        };
        // A generic checked on its own, with `T` standing for anything,
        // matches a value whose type is not known, and every copy of it
        // is checked for its cases left out (decision 58.1).
        let of_generic_enum = arms.iter().any(|arm| {
            let name = match &arm.pattern {
                Pattern::Name(name) | Pattern::Variant { name, .. } => name,
                _ => return false,
            };
            matches!(env.get(name), Some(Variable { ty: Ty::GenericCase(..), .. }))
        });
        let complete = match of_enum {
            _ if catch_all => true,
            Ty::Invalid if of_generic_enum => true,
            Ty::EnumValue(info) => {
                let mut missing: Vec<String> = info
                    .cases
                    .borrow()
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !covered.contains(index))
                    .map(|(_, case)| case.name.clone())
                    .collect();
                if none_left {
                    missing.insert(0, "`none`".to_string());
                }
                if !missing.is_empty() {
                    self.error(
                        format!(
                            "this `match` does not handle {}. Add a `case` for it, or a final `case _`",
                            missing.join(", ")
                        ),
                        span,
                    );
                }
                // Already reported. Treat it as complete so that the same
                // gap is not reported again as a missing `return`.
                true
            }
            ty if ty.is_loose() => false,
            _ => {
                self.error("this `match` needs a final `case _` to handle every other value", span);
                false
            }
        };
        all_exit && complete
    }

    /// Check a pattern against the type being matched, declaring the names
    /// it introduces. Reports how much of the type the pattern accepts.
    ///
    /// `look` says the value has one owner, so a name for it is a look at
    /// it. `looks` is set if the pattern gave such a name.
    fn pattern(&mut self, pattern: &Pattern, subject: &Ty, look: bool, looks: &mut bool, scope: &Env, span: Span) -> Covers {
        match pattern {
            Pattern::Wildcard => Covers::Everything,
            Pattern::Literal(literal) => {
                let ty = self.expr(literal, scope);
                if !fits(&ty, subject) && !fits(subject, &ty) {
                    self.error(format!("this case is {ty}, which can never match {subject}"), span);
                }
                Covers::Some
            }
            Pattern::Name(name) => {
                // A bare enum case is compared; any other name takes the value.
                // A case of a generic enum is one of the copy matched.
                if let Some(Variable { ty: Ty::GenericCase(generic, index), case: Some(_), .. }) = scope.get(name) {
                    self.copy_matched(&generic, name, subject, span);
                    return Covers::Case(index);
                }
                if let Some(Variable { ty: Ty::EnumValue(info), case: Some(index), .. }) = scope.get(name) {
                    if !fits(&Ty::EnumValue(info.clone()), subject) {
                        self.error(
                            format!("`{name}` is a case of {}, which can never match {subject}", info.name),
                            span,
                        );
                    }
                    return Covers::Case(index);
                }
                if look {
                    *looks = true;
                    scope.declare(name, lent(subject), false);
                    if let Some(variable) = scope.get_here(name) {
                        self.owned.parts.insert(variable.id);
                    }
                } else {
                    scope.declare(name, subject.clone(), false);
                }
                self.placed(scope, name, span);
                Covers::Everything
            }
            Pattern::Variant { name, fields } => {
                let (info, index) = match scope.get(name) {
                    Some(Variable { ty: Ty::CaseConstructor(info, index), .. }) => (info, index),
                    Some(Variable { ty: Ty::GenericCase(generic, index), case: None, .. }) => {
                        match self.copy_matched(&generic, name, subject, span) {
                            Some(info) => (info, index),
                            // Matched against something that is not a
                            // copy, which was said, or is only known
                            // when the program runs.
                            None => {
                                // Only known when the program runs, or a
                                // mistake that has been reported.
                                let part = match subject.has_dynamic() {
                                    true => Ty::Dynamic,
                                    false => Ty::Invalid,
                                };
                                for pattern in fields {
                                    self.pattern(pattern, &part, false, looks, scope, span);
                                }
                                return Covers::Some;
                            }
                        }
                    }
                    _ => {
                        self.error(format!("`{name}` is not an enum case that carries values"), span);
                        return Covers::Some;
                    }
                };
                if !fits(&Ty::EnumValue(info.clone()), subject) {
                    self.error(
                        format!("`{name}` is a case of {}, which can never match {subject}", info.name),
                        span,
                    );
                }
                let slots = info.cases.borrow()[index].fields.clone();
                if slots.len() != fields.len() {
                    self.error(
                        format!(
                            "`{name}` carries {}, but this pattern has {}",
                            counted(slots.len(), "value"),
                            fields.len()
                        ),
                        span,
                    );
                    return Covers::Some;
                }
                // The case is fully handled only if every inner pattern accepts anything.
                let mut whole_case = true;
                for (pattern, slot) in fields.iter().zip(&slots) {
                    // A part that has one owner belongs to the value it
                    // is in. A number or a text is copied out.
                    let (part, look) = (slot.ty.clone().looked_at(), slot.ty.is_owned());
                    whole_case &= matches!(self.pattern(pattern, &part, look, looks, scope, span), Covers::Everything);
                }
                if whole_case {
                    Covers::Case(index)
                } else {
                    Covers::Some
                }
            }
        }
    }

    /// The copy of the generic enum that a `match` with a case of it is
    /// on: in `match m { case Just(v) ... }` with `m` a `Maybe[int]`,
    /// `Maybe[int]` (decision 58.1). `None` if the value is not one, which
    /// is an error unless it is only known when the program runs.
    fn copy_matched(&mut self, generic: &Rc<Generic>, name: &str, subject: &Ty, span: Span) -> Option<Rc<EnumInfo>> {
        let inner = match subject.clone().looked_at() {
            Ty::Optional(inner) => inner.looked_at(),
            other => other,
        };
        match inner {
            Ty::EnumValue(info) if info.origin.as_ref().is_some_and(|(origin, _)| Rc::ptr_eq(origin, generic)) => Some(info),
            ty if ty.is_loose() => None,
            _ => {
                self.error(format!("`{name}` is a case of {}, which can never match {subject}", generic.name), span);
                None
            }
        }
    }

    fn try_statement(&mut self, body: &[Stmt], handlers: &[Handler], env: &Env, span: Span) -> bool {
        // Collect what the body can raise separately, so the handlers can
        // take out what they catch.
        self.raised.push(Vec::new());
        let before = self.owned.moved.clone();
        self.owned.tries.push(HashMap::new());
        let mut all_exit = self.block(body, env);
        let raised = self.raised.pop().unwrap_or_default();
        // An error can leave the body after any of the things it gives
        // away, so an `except` block starts with all of them gone.
        let in_body = self.owned.tries.pop().unwrap_or_default();
        if let Some(outer) = self.owned.tries.last_mut() {
            outer.extend(in_body.clone());
        }
        let mut after = if all_exit { Moved::new() } else { self.owned.moved.clone() };
        let mut at_except = before;
        at_except.extend(in_body);

        let mut caught: Vec<&str> = Vec::new();
        for handler in handlers {
            let scope = Scope::child(env);
            // Where everything has one owner, the `except` that catches
            // an error receives it, so the name is its owner: the error
            // is freed when the block ends, unless it was given on.
            let error_type = match &handler.error {
                None => self.an_error(Rc::from("Error")),
                Some(name) => match env.get(name).map(|variable| variable.ty) {
                    Some(Ty::ErrorType(kind)) => self.an_error(kind),
                    Some(Ty::Class(class)) if self.one_owner => Ty::Instance(class),
                    Some(Ty::Class(class)) if class.owned => {
                        self.error(format!("`{name}` is an owned class. Its objects cannot be raised, so none can be caught"), span)
                    }
                    Some(Ty::Class(class)) => Ty::Instance(class),
                    _ => self.error(format!("`{name}` is not a kind of error"), span),
                },
            };
            caught.push(handler.error.as_deref().unwrap_or("Error"));
            if let Some(binding) = &handler.binding {
                scope.declare(binding, error_type, false);
                self.placed(&scope, binding, span);
            }
            self.owned.moved = at_except.clone();
            let exits = self.statements(&handler.body, &scope);
            all_exit &= exits;
            if !exits {
                after.extend(std::mem::take(&mut self.owned.moved));
            }
        }
        self.owned.moved = after;

        // Whatever no handler catches carries on outward.
        for (kind, at) in raised {
            if !caught.iter().any(|name| *name == "Error" || *name == kind) {
                self.can_raise(&kind, at);
            }
        }
        all_exit
    }

    // ------------------------------------------------------------- Resources
    //
    // An object of an `owned class` that is `Resource` (decision 59.1) is
    // released when its owner ends. In this version its owner is a `let`
    // or a parameter, and nothing else: each other place it could go is
    // refused here, so that every engine has only "the end of a block" to
    // make into something that runs.

    /// Is a value of this type a resource? A look at one is not: it is
    /// the owner's to release.
    fn is_resource(&self, ty: &Ty) -> bool {
        let Some(resource) = &self.resource else { return false };
        match ty {
            Ty::Instance(class) => Rc::ptr_eq(class, resource) || class.is.borrow().iter().any(|is| Rc::ptr_eq(is, resource)),
            _ => false,
        }
    }

    /// Does this type hold a resource anywhere inside, a look at one
    /// included?
    fn holds_resource(&self, ty: &Ty) -> bool {
        if self.resource.is_none() {
            return false;
        }
        match ty {
            Ty::List(inner) | Ty::Fixed(inner, _) | Ty::Optional(inner) | Ty::Owned(inner) | Ty::Borrowed(inner) | Ty::Alias(inner) => {
                self.holds_resource(inner)
            }
            Ty::Dict(key, value) => self.holds_resource(key) || self.holds_resource(value),
            Ty::Several(types) | Ty::Applied(_, types) => types.iter().any(|ty| self.holds_resource(ty)),
            other => self.is_resource(other),
        }
    }

    /// The refusal of rule 7 of 59.1, for a place that would hold a
    /// resource inside something else.
    fn resource_inside(&mut self, what: &str, span: Span) -> Ty {
        self.error(
            format!(
                "{what} would hold a resource. A resource is released when its owner ends, and its owner is a \
                 `let` or a parameter, never a list, a dict, a field, a case of an enum, `or none` or a type \
                 parameter. Keep it in a `let` of its own"
            ),
            span,
        )
    }

    // ----------------------------------------------------------- Owned values
    //
    // Most values in Zeno are shared: `let b = a` makes a second name for
    // the same list, and the collector frees it when no name is left. An
    // owned value (an `owned list[T]`, or an object of an `owned class`)
    // has exactly one owner instead, and is freed when its owner's block
    // ends. For that to be safe the checker has to know, at every line,
    // who the owner is. Three ways of using a value keep that simple:
    //
    // - `take`: the value is about to be kept somewhere (a variable, a
    //   field, a parameter that takes it, a `return`). If it comes from a
    //   variable, that variable has given it away and may not be used again.
    // - `look`: the value is read where it stands (printed, indexed,
    //   compared, lent to a parameter that looks). Nothing changes hands.
    // - `expr`, used anywhere else, gives an owner's value the type
    //   `Borrowed`, which fits nowhere. So a place this section forgot is
    //   an error message, never a second owner.

    /// Is `name` being used in a function that was called too early, from
    /// above the line that declares the variable it means? If so this
    /// reports it, once for each name, and returns the type of a mistake.
    /// `variable` is what the name was taken for, if anything.
    fn too_early(&mut self, name: &str, variable: Option<&Variable>, env: &Env, span: Span) -> Option<Ty> {
        // Start at the function the name is written in and work outward
        // through the functions it is written inside.
        let mut body = env.function;
        let at = loop {
            // A variable of this very function is what the name means.
            if variable.is_some_and(|variable| variable.function == body) {
                return None;
            }
            let at = self.early.iter().rposition(|early| early.body == body)?;
            if self.early[at].missing.iter().any(|missing| missing == name) {
                break at;
            }
            body = self.early[at].outer;
        };
        let checking = &mut self.early[at];
        if checking.told.iter().any(|told| told == name) {
            return Some(Ty::Invalid);
        }
        checking.told.push(name.to_string());
        let function = checking.name.clone();
        Some(self.error(
            format!(
                "`{name}` is declared further down than the first use of `{function}`, which needs it. \
                 Use `{function}` below the line that declares `{name}`"
            ),
            span,
        ))
    }

    /// What reading the variable `name` gives. For an owner, or a looking
    /// parameter, that is a look at the value and no more.
    fn read(&mut self, name: &str, variable: Variable, env: &Env, span: Span) -> Ty {
        // A bare case of an enum, such as `Stop`, is not a variable that
        // holds a value: each use of the name is the value, new.
        let outside = variable.function != env.function && variable.case.is_none();
        if outside && self.one_owner && variable.mutable && !variable.top {
            return self.error(shared(name), span);
        }
        if !variable.ty.is_tied() || variable.case.is_some() {
            return variable.ty;
        }
        // A `let` at the top of the file holds its value until the
        // program ends, as long as the top of the file never gives it
        // away. So any function may look at it, and may be kept while it
        // does. A `var` could be given a new value while a function is
        // still looking (decision 5.19i).
        if outside && variable.top && !variable.mutable {
            return match self.given_top.get(&variable.id) {
                Some(line) => self.error(
                    format!(
                        "`{name}` is given away on line {line}, so a function cannot use it: the function could \
                         be called when `{name}` holds nothing. Keep `{name}` where it is, or pass it to the function"
                    ),
                    span,
                ),
                None => {
                    self.reached.entry(variable.id).or_insert(span.line_in_file());
                    lent(&variable.ty)
                }
            };
        }
        if outside {
            // A function written where a value goes may look at a value
            // of the function around it. That makes the function a look
            // too: see `ExprKind::Function`.
            let literal = self.early.iter_mut().rev().find(|early| early.body == env.function);
            return match literal.filter(|early| early.literal) {
                Some(early) => {
                    if !early.captures.iter().any(|capture| capture.id == variable.id) {
                        early.captures.push(Capture { name: name.to_string(), id: variable.id, function: variable.function });
                    }
                    lent(&variable.ty)
                }
                None => self.error(elsewhere(name, variable.top), span),
            };
        }
        if let Some((_, at)) = self.owned.moved.get(&variable.id) {
            return self.error(
                format!(
                    "`{name}` was given away on line {} and holds nothing now. \
                     Use it before it goes, or give it only to what looks: a parameter without `take`",
                    at.line_in_file()
                ),
                span,
            );
        }
        if self.owned.emptied.contains(&variable.id) {
            return self.error(
                format!(
                    "this line takes an item out of `{name}`, so it cannot use `{name}` for anything else. \
                     Do the other thing on a line of its own"
                ),
                span,
            );
        }
        self.owned.looking.push(variable.id);
        lent(&variable.ty)
    }

    /// The owner that `inside` reaches into, if a `for` loop around this
    /// point is walking a list of its: the name, and the loop's line.
    ///
    /// A `case` of a `match` that gave a name to a part of the owner's
    /// value counts too. What comes back is the start of an error
    /// message that says who is looking, and the words for when it ends.
    fn being_walked(&self, inside: &Expr, env: &Env) -> Option<(String, String, &'static str)> {
        let name = root(inside)?;
        self.walking(name, env.get(name)?.id)
    }

    /// The same, for the variable `name` whose number is `id`.
    fn walking(&self, name: &String, id: usize) -> Option<(String, String, &'static str)> {
        let (_, line, looking) = self.owned.walked.iter().find(|(walked, _, _)| *walked == id)?;
        Some(match looking {
            Looking::Loop => (
                name.clone(),
                format!(
                    "the `for` on line {line} is walking a list that `{name}` owns, and is still looking at its items."
                ),
                "until the loop is over",
            ),
            Looking::DictLoop => (
                name.clone(),
                format!("the `for` on line {line} is walking a dict that `{name}` owns, and is still looking at its values."),
                "until the loop is over",
            ),
            Looking::Case => (
                name.clone(),
                format!(
                    "the `match` on line {line} gave a name to a part of what `{name}` holds, and this `case` is still looking at it."
                ),
                "until the `case` is over. If the name is not needed, write `_` in its place",
            ),
        })
    }

    /// A part that has one owner is about to leave what `inside` reaches
    /// into: an item is taken out of a list, or a new value takes the
    /// place of an item or of a field, and the old one is freed. Anybody
    /// who was lent that part would be left looking at nothing. So:
    ///
    /// - Only the owner may do it. A function that was lent the list or
    ///   the object cannot know who else was lent a part of it.
    /// - Not inside a `for` loop that walks it: the loop was lent each item.
    /// - Not on a line that has already looked at it: the look might be
    ///   in use still. `looked` is how many looks the line had made
    ///   before it reached `inside`.
    ///
    /// `does` is what is being done, for the message: "take it out".
    /// Returns the owner, if there is one and it may.
    fn takes_out(&mut self, inside: &Expr, looked: usize, does: &str, env: &Env, span: Span) -> Option<usize> {
        // Something made on this line, as in `make().pop()`: the
        // statement owns it, and nobody else has seen it.
        let name = root(inside)?;
        let variable = env.get(name)?;
        if variable.function != env.function {
            // It belongs to the function around this one. A function
            // written where a value goes only looks at it. (Any other
            // function has been told already that it cannot reach it.)
            if self.early.iter().rev().find(|early| early.body == env.function).is_some_and(|early| early.literal) {
                self.error(
                    format!(
                        "only the owner can {does}: somebody else may have been lent it. \
                         Here `{name}` belongs to the function around this one, which is only looked at from here"
                    ),
                    span,
                );
            } else if variable.top && !variable.mutable {
                // A `let` at the top of the file, which every function
                // may look at.
                self.error(
                    format!(
                        "only the owner can {does}: somebody else may have been lent it. \
                         Here `{name}` belongs to the top of the file, and a function only looks at it"
                    ),
                    span,
                );
            }
            return None;
        }
        // A method may take out what its own object holds. Whoever calls
        // the method is then held to this rule in its place: see
        // `ExprKind::Call` (decision 5.19k).
        let method = name == "self" && self.frame().takes_self.is_some();
        if method {
            self.frame().takes_self = Some(true);
        }
        if !variable.ty.is_owned() && !method {
            let what = match &inside.kind {
                ExprKind::Name(_) => format!("`{name}` only looks at it"),
                _ => format!("what `{name}` holds is only looked at"),
            };
            self.error(
                format!(
                    "only the owner can {does}: somebody else may have been lent it. \
                     Here {what}"
                ),
                span,
            );
            return None;
        }
        if let Some((_, who, until)) = self.being_walked(inside, env) {
            self.error(format!("{who} Nothing can {does} {until}"), span);
            return None;
        }
        if self.owned.looking[..looked.min(self.owned.looking.len())].contains(&variable.id) {
            self.error(
                format!(
                    "this line has already looked at `{name}`, and the look might still be in use. \
                     Do the other thing on a line of its own first"
                ),
                span,
            );
            return None;
        }
        Some(variable.id)
    }

    /// The named function whose body is being checked. See `Spot`.
    fn within(&self) -> usize {
        self.within.last().copied().unwrap_or(0)
    }

    /// Why the owner `name` cannot give its value away on this line, if
    /// it cannot: a loop or a `case` is looking at it, or the line has
    /// already used it. `giving` says what the line does with it.
    fn cannot_give_away(&self, name: &String, variable: &Variable, giving: &str) -> Option<String> {
        if let Some((_, who, until)) = self.walking(name, variable.id) {
            return Some(format!("{who} `{name}` cannot be given away {until}"));
        }
        if self.owned.looking.contains(&variable.id) {
            return Some(format!("{giving}, on a line that also uses it. Do the other thing on a line of its own first"));
        }
        None
    }

    /// The owner `name` gives its value away at `span`, and holds nothing
    /// after it. False, with a message, if it cannot.
    fn give_away(&mut self, name: &String, variable: &Variable, span: Span) -> bool {
        // A `let` at the top of the file that a function looks at stays
        // where it is.
        if variable.top && !variable.mutable {
            if let Some(line) = self.reached.get(&variable.id) {
                self.error(
                    format!(
                        "`{name}` is used by a function, on line {line}, so it cannot be given away: the function \
                         could be called when `{name}` holds nothing. Pass `{name}` to that function instead, \
                         or give away a copy"
                    ),
                    span,
                );
                return false;
            }
            self.given_top.insert(variable.id, span.line_in_file());
        }
        let record = (name.clone(), span);
        for body in &mut self.owned.tries {
            body.insert(variable.id, record.clone());
        }
        self.owned.moved.insert(variable.id, record);
        // The `Scoped` the resource's `let` began does not release what
        // was given away (59.1).
        if self.is_resource(&variable.ty) {
            self.types.gives.insert((span.line, span.col));
        }
        true
    }

    /// The type of an expression whose value is about to be kept. An owner
    /// named here gives its value away.
    fn take(&mut self, expr: &Expr, env: &Env) -> Ty {
        // A resource that a call gives is kept here (59.1).
        self.receiving.insert(expr as *const Expr as usize);
        let owner = match &expr.kind {
            ExprKind::Name(name) | ExprKind::Give(name) => {
                // A function written inside another one only looks at
                // what the outer one owns.
                env.get(name)
                    .filter(|variable| variable.ty.is_owned() && variable.case.is_none())
                    .filter(|variable| variable.function == env.function)
                    .map(|v| (name, v))
            }
            _ => None,
        };
        if let Some((name, variable)) = owner {
            if let Some(message) = self.cannot_give_away(name, &variable, &format!("`{name}` is given away here")) {
                return self.error(message, expr.span);
            }
            // The usual checks: that it still holds something, and that
            // this is the function it belongs to.
            if matches!(self.expr(expr, env), Ty::Invalid) {
                return Ty::Invalid;
            }
            if !self.give_away(name, &variable, expr.span) {
                return Ty::Invalid;
            }
            self.types.moves.insert(expr as *const Expr as usize);
            return variable.ty;
        }
        // A function written here is kept, so it takes what it uses
        // (decision 56.8). So is a method used as a value (56.9).
        if matches!(expr.kind, ExprKind::Function(_) | ExprKind::Field { .. }) {
            self.keeping = Some(expr as *const Expr as usize);
        }
        let ty = self.expr(expr, env);
        self.keeping = None;
        match ty {
            look if look.is_look() => {
                let inner = look.clone().looked_at();
                let item = |name: &String| env.get(name).is_some_and(|variable| self.owned.items.contains(&variable.id));
                let part = |name: &String| env.get(name).is_some_and(|variable| self.owned.parts.contains(&variable.id));
                let message = match &expr.kind {
                    ExprKind::Name(name) if item(name) => format!(
                        "`{name}` is a look at an item of the list that the loop is walking. The list owns the item, \
                         so it cannot be kept, returned or given away. Use it here, or pass it to a parameter that looks"
                    ),
                    ExprKind::Name(name) if part(name) => format!(
                        "`{name}` is a look at the value that the `match` is looking at, or at a part of it. \
                         Its owner still has it, so it cannot be kept, returned or given away. \
                         Use it here, or pass it to a parameter that looks"
                    ),
                    ExprKind::Name(name) if name == "self" => "`self` is the object the method was called on. \
                         Its owner still has it, so it cannot be kept, returned or given away"
                        .to_string(),
                    ExprKind::Name(name) => format!(
                        "`{name}` is only looked at here: its owner still has it. So it cannot be kept, \
                         returned or given away. If it is a parameter, write `take` in front of its type \
                         to take the value over"
                    ),
                    // One that may be `none` can be taken out, leaving
                    // `none` (decision 56.10).
                    ExprKind::Field { name, .. } if matches!(inner, Ty::Optional(_)) => format!(
                        "`{name}` holds an owned value, which cannot be taken out of the object it is in. \
                         Use it where it is, or pass it to a parameter that looks. To move it out and leave `none` \
                         in the field, write `take {}`",
                        crate::printer::expr(expr)
                    ),
                    ExprKind::Field { name, .. } => format!(
                        "`{name}` holds an owned value, which cannot be taken out of the object it is in. \
                         Use it where it is, or pass it to a parameter that looks"
                    ),
                    // `d.get(key)` of a dict that owns its values (56.12).
                    ExprKind::Call { callee, .. }
                        if matches!(&callee.kind, ExprKind::Field { name, .. } if name == "get") && self.one_owner =>
                    {
                        format!(
                            "`get` gives a look at {} that the dict owns, or `none`, and a look cannot be kept. \
                             Test it where it is, or pass it to a parameter that looks. To take the value out \
                             of the dict, use `pop(key)`",
                            an_owned(&match inner.clone() {
                                Ty::Optional(value) => *value,
                                other => other,
                            }
                            .looked_at())
                        )
                    }
                    ExprKind::Index { .. } if self.dict_values.contains(&(expr as *const Expr as usize)) => format!(
                        "this value is {}, which has one owner: the dict. `[ ]` only looks at it. \
                         Use it where it is, pass it to a parameter that looks, or take it out with `pop(key)`",
                        an_owned(&inner)
                    ),
                    ExprKind::Index { .. } if matches!(inner, Ty::Optional(_)) => format!(
                        "this item is {}, which has one owner: the list. `[ ]` only looks at it. \
                         Use it where it is, or pass it to a parameter that looks. To move it out and leave `none` \
                         in its place, write `take {}`",
                        an_owned(&inner),
                        crate::printer::expr(expr)
                    ),
                    ExprKind::Index { .. } => format!(
                        "this item is {}, which has one owner: the list. `[ ]` only looks at it. \
                         Use it where it is, pass it to a parameter that looks, or take it out with `pop()`",
                        an_owned(&inner)
                    ),
                    ExprKind::Function(_) => "this function uses a value from outside it that has one owner, \
                         so it must be gone before that value is, and cannot be kept, returned or given away. \
                         Pass it straight to `map`, `filter`, `reduce` or a parameter that looks, \
                         or give the function what it needs as a parameter of its own"
                        .to_string(),
                    _ => format!("this is {look}, which cannot be kept"),
                };
                self.error(message, expr.span)
            }
            // Where everything has one owner, whoever keeps a list that
            // was just made is its owner. So is whoever keeps a list of
            // items that have one owner each, in any build.
            list @ Ty::List(_) if self.one_owner || owns_items(&list) => {
                self.fresh_list(expr, list.clone(), &Ty::Owned(Box::new(list)))
            }
            dict @ Ty::Dict(..) if self.one_owner => self.fresh_list(expr, dict.clone(), &Ty::Owned(Box::new(dict))),
            ty => ty,
        }
    }

    /// The type of a dict that is written out: `["Ada": 36, "Alan": 41]`.
    /// The keys are all of one type and so are the values, which is what
    /// lets a line further down say what `ages["Ada"]` is.
    fn dict_written(&mut self, expr: &Expr, entries: &[(Expr, Expr)], env: &Env) -> Ty {
        let (mut keys, mut values): (Option<Ty>, Option<Ty>) = (None, None);
        // A place that says more about the values than they say
        // themselves, as for a list: `["a": Circle(1.0), "b": Square(2.0)]`
        // where a `dict[str, Shape]` is wanted.
        let told = self.expected_items.take().filter(|(place, _)| *place == expr as *const Expr as usize);
        if let Some((_, wanted)) = &told {
            self.types.items.insert(expr as *const Expr as usize, kind(wanted));
            values = Some(wanted.clone());
        }
        for (index, (key, value)) in entries.iter().enumerate() {
            let key_type = self.look(key, env).looked_at().widened();
            let value_type = match &told {
                Some((_, wanted)) => {
                    let given = self.kept(value, wanted, env);
                    let given = self.adapted(written(value), value as *const Expr as usize, given, wanted, value.span);
                    if fits(&given, wanted) {
                        self.arrives(&given, wanted, "a value of this dict", value.span);
                        wanted.clone()
                    } else {
                        let advice = match (&given, wanted) {
                            (Ty::Int, Ty::Float) => {
                                ". Write a number with a decimal point, as in `1.0`, and convert a value with `float( )`"
                            }
                            _ => "",
                        };
                        return self.error(
                            format!("the values of this dict are {wanted} here, but this one is {given}{advice}"),
                            value.span,
                        );
                    }
                }
                // The dict keeps the value. With no collector it owns one
                // that has one owner, as a list owns its items (56.12).
                None => match self.item(value, env) {
                    owned if self.one_owner && owned.is_owned() => owned,
                    ty => ty.looked_at().widened(),
                },
            };
            if !matches!(key_type, Ty::Str | Ty::Int) && !key_type.is_loose() {
                return self.error(format!("a key of a dict is a str or an int, and this is {key_type}"), key.span);
            }
            if let Some(why) = self.no_such_value(&value_type) {
                return self.error(why, value.span);
            }
            // The same key written twice: the second would quietly
            // replace the first.
            let twice = entries[..index].iter().any(|(earlier, _)| match (&earlier.kind, &key.kind) {
                (ExprKind::Str(a), ExprKind::Str(b)) => a == b,
                (ExprKind::Int(a), ExprKind::Int(b)) => a == b,
                _ => false,
            });
            if twice {
                return self.error(
                    "this key is written twice in the dict. A dict has each key once, so the second \
                     would replace the first. Take one of them out",
                    key.span,
                );
            }
            for (known, found, what, at) in
                [(&mut keys, key_type, "keys", key.span), (&mut values, value_type, "values", value.span)]
            {
                match known {
                    Some(known) if known.is_loose() || found.is_loose() || same(known, &found) => {}
                    // `[:]` beside a dict that says what it holds, a u8
                    // beside an int: the one that says more stands.
                    Some(known) if fits(&found, known) => {}
                    Some(known) if fits(known, &found) => *known = found,
                    Some(known) => {
                        return self.error(
                            format!(
                                "the {what} of a dict are all of one type. This one is {found}, \
                                 and the ones before it are {known}"
                            ),
                            at,
                        )
                    }
                    None => *known = Some(found),
                }
            }
        }
        // `[:]` holds nothing yet, so what it will hold is open.
        Ty::dict(keys.unwrap_or(Ty::Dynamic), values.unwrap_or(Ty::Dynamic))
    }

    /// Why a dict cannot have values of type `value`, if it cannot.
    fn no_such_value(&self, value: &Ty) -> Option<String> {
        let plain = value.clone().looked_at();
        match &plain {
            // `get` says `none` for a key that is not there.
            Ty::Optional(inner) => Some(format!(
                "a value of a dict cannot be `{inner} or none`: `get` gives `none` for a key that is not there, and \
                 that could not be told from a value that is `none`. Make it a `dict[..., {inner}]`, and leave \
                 the key out where there is no value"
            )),
            Ty::None => Some(
                "a value of a dict cannot be `none`: `get` gives `none` for a key that is not there. \
                 Leave the key out instead"
                    .to_string(),
            ),
            _ => None,
        }
    }

    /// The type of an error of one of the kinds Zeno knows, such as a
    /// `ValueError`. Where everything has one owner, an error has one
    /// too: it is kept in a block of its own.
    fn an_error(&self, kind: Rc<str>) -> Ty {
        match self.one_owner {
            true => Ty::Owned(Box::new(Ty::Error(kind))),
            false => Ty::Error(kind),
        }
    }

    /// `take`, for a value going somewhere whose type is known.
    fn kept(&mut self, value: &Expr, wanted: &Ty, env: &Env) -> Ty {
        // `[Circle(1.0), Square(2.0)]` where a `list[Shape]` is wanted, or
        // `[1, 2]` where a `list[int or none]` is. The items do not say
        // that they are shapes, or that one may be `none` later, so each
        // is checked against what the place wants, and the list is a
        // list of that.
        self.expecting(value, wanted);
        let given = self.take(value, env);
        self.fresh_list(value, given, wanted)
    }

    /// A list that was made right here is nobody's yet: one written out
    /// in full, one built by a comprehension, or what `split`, `map`,
    /// `filter` or `+` gives. So it can begin life as an owned list where
    /// one is wanted. The compiler is told, because a build with no
    /// collector has to know who frees each list.
    fn fresh_list(&mut self, value: &Expr, given: Ty, wanted: &Ty) -> Ty {
        let made = matches!(value.kind, ExprKind::List(_) | ExprKind::ListComp { .. } | ExprKind::Dict(_))
            || self.types.made.contains(&(value as *const Expr as usize));
        // An `owned list[int] or none` wants an owned list, when it
        // gets a list at all.
        let wanted = match wanted {
            Ty::Optional(inner) => &**inner,
            wanted => wanted,
        };
        match (wanted, given) {
            (Ty::Owned(_), given) if made && given.is_collection() => {
                self.types.fresh.insert(value as *const Expr as usize);
                Ty::Owned(Box::new(given))
            }
            (_, given) => given,
        }
    }

    /// The type of an expression that is only looked at.
    fn look(&mut self, expr: &Expr, env: &Env) -> Ty {
        let ty = self.expr(expr, env);
        self.looked(ty, expr)
    }

    /// `look`, and the length of the list if it is one of a fixed length.
    fn look_fixed(&mut self, expr: &Expr, env: &Env) -> (Ty, Option<u64>) {
        let ty = self.expr(expr, env);
        let length = fixed_length(&ty.clone().looked_at());
        (self.looked(ty, expr), length)
    }

    /// `append`, `pop`, `insert` and `clear` change how long a list is,
    /// so a list of a fixed length has none of them (decision 36.4).
    fn keeps_length(&mut self, name: &str, length: Option<u64>, span: Span) -> bool {
        let Some(length) = length else {
            return true;
        };
        if !matches!(name, "append" | "pop" | "insert" | "clear") {
            return true;
        }
        self.error(
            format!(
                "`{name}` would change how many items this list has, and a list of a fixed length always has {length}. \
                 Give an item a new value instead, as in `list[0] = 7`"
            ),
            span,
        );
        false
    }

    /// `buffer[64]` on a `list[u8, 64]`: a position written as a number
    /// is checked before the program runs.
    fn index_inside(&mut self, index: &Expr, length: Option<u64>) {
        if let (Some(length), Some(Written::Number(at, _))) = (length, written(index)) {
            if at >= length as i128 || at < 0 {
                self.error(
                    format!("{at} is outside a list of {length} items, whose positions are 0 to {}", length - 1),
                    index.span,
                );
            }
        }
    }

    /// What somebody who only looks sees of the value of `expr`, whose
    /// type is `ty`.
    fn looked(&mut self, ty: Ty, expr: &Expr) -> Ty {
        if ty.is_owned() || owns_items(&ty) {
            // Made right here, and given to nobody, as in
            // `print(size(make()))`. The statement it is in owns it, and
            // frees it when the statement is over.
            self.types.lent.insert(expr as *const Expr as usize);
        }
        // Whatever only looks reads a list of a fixed length as a list.
        unfixed(ty.looked_at())
    }

    /// The type of an expression whose value goes into a list. The list
    /// keeps it, so an owner named here gives its value to the list.
    fn item(&mut self, expr: &Expr, env: &Env) -> Ty {
        self.take(expr, env)
    }

    /// A loop is about to walk `iterable`, and each trip is given `item`.
    /// If that is a look at an item that has one owner, the owner the
    /// list is reached through must leave the items where they are until
    /// the loop is over: this notes it. Returns what to hand
    /// `walked.truncate` when the loop is.
    fn walk(&mut self, iterable: &Expr, item: &Ty, env: &Env) -> usize {
        let before = self.owned.walked.len();
        if let (true, Some(name)) = (item.is_look(), root(iterable)) {
            let in_method = name == "self" && self.frame().takes_self.is_some();
            if let Some(variable) = env.get(name).filter(|variable| variable.ty.is_owned() || in_method) {
                let looking = match variable.ty.clone().looked_at() {
                    Ty::Dict(..) => Looking::DictLoop,
                    _ => Looking::Loop,
                };
                self.owned.walked.push((variable.id, iterable.span.line_in_file(), looking));
            }
        }
        before
    }

    /// Declare the variable of a `for` loop, which holds `item` on each
    /// trip.
    fn loop_variable(&mut self, scope: &Env, variable: &str, item: Ty, span: Span) {
        let looks = item.is_look();
        scope.declare(variable, item, false);
        self.placed(scope, variable, span);
        if let (true, Some(variable)) = (looks, scope.get_here(variable)) {
            self.owned.items.insert(variable.id);
        }
    }

    /// The parameters a call's arguments are going to, when the thing
    /// called has parameters with names.
    fn slots(&mut self, callee: &Ty) -> Option<Vec<Slot>> {
        match callee {
            Ty::Function(function) => Some(self.signature(function).params.clone()),
            Ty::BoundMethod(function) => Some(self.signature(function).params.get(1..).unwrap_or_default().to_vec()),
            Ty::Class(class) => Some(class.fields.borrow().clone()),
            Ty::CaseConstructor(info, index) => Some(info.cases.borrow()[*index].fields.clone()),
            Ty::Generic(generic) => Some(self.outline(generic).slots.clone()),
            Ty::GenericCase(generic, index) => Some(self.case_outline(generic, *index).slots.clone()),
            // A function held in a variable: its type says what each
            // parameter does with what it is given.
            Ty::Callable(callable) => Some(
                callable
                    .params
                    .iter()
                    .map(|ty| Slot { name: String::new(), ty: ty.clone(), has_default: false, id: None })
                    .collect(),
            ),
            // `map`, `filter` and `reduce` call the function they are
            // given and do not keep it.
            Ty::BuiltinMethod(_, name) if matches!(&**name, "map" | "filter" | "reduce" | "sort") => Some(vec![Slot {
                name: String::new(),
                ty: Ty::Borrowed(Box::new(Ty::Dynamic)),
                has_default: false,
                id: None,
            }]),
            // `append` keeps what it is given. For an item that has one
            // owner that matters: the list becomes the owner. So it does
            // for a list written out as the item, where the items of
            // that list are kept another way than they say themselves.
            Ty::BuiltinMethod(receiver, name) if &**name == "append" => match &**receiver {
                // A float written out goes into a list of f32s as one,
                // and `Nothing` into a list of `Maybe[int]` as one (58.1).
                Ty::List(item) if item.is_owned() || asks_more(item) || matches!(**item, Ty::F32 | Ty::U64) || is_enum_copy(item) => {
                    Some(vec![Slot { name: "item".to_string(), ty: (**item).clone(), has_default: false, id: None }])
                }
                _ => None,
            },
            // `find` only looks at what it is given. An item made in the
            // argument itself belongs to the statement, which frees it.
            Ty::BuiltinMethod(receiver, name) if &**name == "find" => match &**receiver {
                Ty::List(item) if item.is_owned() => {
                    Some(vec![Slot { name: "item".to_string(), ty: lent(item), has_default: false, id: None }])
                }
                Ty::List(item) | Ty::Fixed(item, _) if matches!(**item, Ty::F32 | Ty::U64) || is_enum_copy(item) => {
                    Some(vec![Slot { name: "item".to_string(), ty: (**item).clone(), has_default: false, id: None }])
                }
                _ => None,
            },
            // So does `insert`, after the position.
            Ty::BuiltinMethod(receiver, name) if &**name == "insert" => match &**receiver {
                Ty::List(item) if item.is_owned() || asks_more(item) || matches!(**item, Ty::F32 | Ty::U64) || is_enum_copy(item) => Some(vec![
                    Slot { name: "position".to_string(), ty: Ty::Int, has_default: false, id: None },
                    Slot { name: "item".to_string(), ty: (**item).clone(), has_default: false, id: None },
                ]),
                _ => None,
            },
            _ => None,
        }
    }

    /// A loop begins. Returns what is needed to check it when it ends.
    fn enter_loop(&mut self) -> (Moved, usize) {
        self.owned.loops.push((Moved::new(), Moved::new()));
        (self.owned.moved.clone(), fresh_id())
    }

    /// A loop has ended. A value given away on one trip round is not
    /// there on the next, so an owner from outside the loop may only be
    /// given away on a path that leaves the loop.
    fn leave_loop(&mut self, (before, first_inside): (Moved, usize), body_exits: bool) {
        let (breaks, continues) = self.owned.loops.pop().unwrap_or_default();
        let mut again = continues;
        if !body_exits {
            again.extend(std::mem::take(&mut self.owned.moved));
        }
        let mut lost: Vec<(String, Span)> = again
            .iter()
            .filter(|(id, _)| **id < first_inside && !before.contains_key(id))
            .map(|(_, record)| record.clone())
            .collect();
        lost.sort_by_key(|(_, at)| (at.line, at.col));
        for (name, at) in lost {
            self.error(
                format!(
                    "`{name}` is given away inside a loop, and the loop may come round again with nothing in it. \
                     Give it away after the loop, or just before a `break`"
                ),
                at,
            );
        }
        // After the loop: whatever was gone before it, during it, or at a
        // `break`. The loop's own variables no longer exist.
        self.owned.moved = before;
        self.owned.moved.extend(again);
        self.owned.moved.extend(breaks);
        self.owned.moved.retain(|id, _| *id < first_inside);
    }

    /// `counter.bump`, a method of an object named by a variable, used as
    /// a value: the function `function(by: int) -> int { return
    /// counter.bump(by) }`, written out here and checked as if it were in
    /// the program (decision 56.9). So it looks at `counter` when it is
    /// passed along, and takes it when it is kept, by the rule of 56.8,
    /// and every compiler builds it as the function it is. None if
    /// `expr` is something else.
    fn method_value(&mut self, expr: &Expr, object: &Expr, name: &str, at: Span, env: &Env) -> Option<Ty> {
        let ExprKind::Name(object_name) = &object.kind else {
            return None;
        };
        let variable = env.get(object_name)?;
        let Ty::Instance(class) = variable.ty.clone().looked_at() else {
            return None;
        };
        // A method of a copy of a generic class is written with the
        // class's own `T`, which means nothing here.
        if class.origin.is_some() || class.fields.borrow().iter().any(|field| field.name == name) {
            return None;
        }
        let method = class.methods.borrow().get(name).cloned()?;
        // A method with types of its own is written for each call.
        if !method.decl.type_params.is_empty() {
            return None;
        }
        if self.kept_private(&class, name, at) {
            return Some(Ty::Invalid);
        }
        let span = expr.span;
        let decl = &method.decl;
        // Names for its parameters that cannot be the object's.
        let mut params = Vec::new();
        for (index, param) in decl.params.iter().skip(1).enumerate() {
            let mut fresh = format!("a{index}");
            while fresh == *object_name {
                fresh.insert(0, 'a');
            }
            params.push(Param { name: fresh, ty: param.ty.clone(), default: None });
        }
        let args = params
            .iter()
            .map(|param| Arg { name: None, value: Expr { kind: ExprKind::Name(param.name.clone()), span } })
            .collect();
        let callee = Expr {
            kind: ExprKind::Field { object: Box::new(Expr { kind: ExprKind::Name(object_name.clone()), span: object.span }), name: name.to_string(), at },
            span,
        };
        let call = Expr { kind: ExprKind::Call { callee: Box::new(callee), args }, span };
        let statement = match decl.return_type {
            Some(_) => StmtKind::Return(Some(call)),
            None => StmtKind::Expr(call),
        };
        // No name: a function with one is a function of the program,
        // which cannot reach what a block owns. The compilers give the
        // value the method's name, which `print` shows.
        let function = Function {
            span,
            name: String::new(),
            type_params: Vec::new(),
            params,
            return_type: decl.return_type.clone(),
            raises: decl.raises.clone(),
            body: vec![Stmt { kind: statement, span }],
        };
        let written = Arc::new(Expr { kind: ExprKind::Function(Arc::new(function)), span });
        // Kept, so it takes what it uses.
        if self.keeping == Some(expr as *const Expr as usize) {
            self.keeping = Some(&*written as *const Expr as usize);
        }
        // An editor sees the object and the method, as they are written,
        // and not the function written out for them.
        let (used, functions_used) = (self.types.used.len(), self.functions_used.len());
        let ty = self.expr(&written, env);
        // Passed along, it is the statement's, and goes when the line is
        // over: the compilers know it by what is written.
        if self.types.lent.remove(&(&*written as *const Expr as usize)) {
            self.types.lent.insert(expr as *const Expr as usize);
        }
        self.types.used.truncate(used);
        self.functions_used.truncate(functions_used);
        self.used(object_name, &variable, object.span);
        self.field_used(name, &Ty::BoundMethod(method), at);
        self.types.method_values.insert(expr as *const Expr as usize, written);
        Some(ty)
    }

    /// A function written where a value goes is kept: returned, put in a
    /// variable or a field, or given to a parameter that says `take`. So
    /// each value with one owner that it uses is given to it, as `let b
    /// = a` gives it, and the function frees them when it is freed
    /// (decision 56.8). What it takes must be the function around it's
    /// to give: a value it owns, or a parameter that says `take`.
    fn takes_what_it_uses(&mut self, function: Rc<FunctionInfo>, captures: Vec<Capture>, env: &Env, span: Span) -> Ty {
        let mut taken = Vec::new();
        let mut wrong = false;
        for capture in captures {
            let name = capture.name.clone();
            // The function in the middle may be called again, and would
            // have nothing left to give the second time.
            if capture.function != env.function {
                self.error(
                    format!(
                        "this function is kept, so it takes `{name}`, but `{name}` belongs to a function further \
                         out, and the function in between may be called again with nothing left to give. Hand \
                         `{name}` to the function in between as a parameter that says `take`"
                    ),
                    span,
                );
                wrong = true;
                continue;
            }
            let Some(variable) = env.get(&name) else {
                continue;
            };
            let message = if let Some((_, at)) = self.owned.moved.get(&variable.id) {
                Some(format!(
                    "`{name}` was given away on line {} and holds nothing now, and this function uses it. \
                     Use it before it goes",
                    at.line_in_file()
                ))
            } else if variable.mutable {
                // The function around could give it a new value after this
                // one took it, and where values are shared the two would
                // then hold different ones (plan, Changes 4).
                Some(format!(
                    "this function is kept, so it takes `{name}`, but `{name}` is a `var`: the function around it \
                     could give it a new value after this one took it. Declare it with `let`"
                ))
            } else if self.is_resource(&variable.ty) {
                Some(format!(
                    "this function is kept, so it would take `{name}`, a resource, and nothing would release it \
                     when its block ends. Pass `{name}` to the function as a parameter instead"
                ))
            } else if name == "self" {
                Some(
                    "this function is kept, so it takes `self`, but `self` is the object the method was called on, \
                     and its owner still has it. Put what the function needs in a `let` first, and use that"
                        .to_string(),
                )
            } else if !variable.ty.is_owned() {
                Some(format!(
                    "this function is kept, so it takes `{name}`, but `{name}` is only looked at here: its owner \
                     still has it. If it is a parameter, write `take` in front of its type to take the value over"
                ))
            } else {
                self.cannot_give_away(&name, &variable, &format!("this function is kept, so it takes `{name}`"))
            };
            if let Some(message) = message {
                self.error(message, span);
                wrong = true;
                continue;
            }
            if !self.give_away(&name, &variable, span) {
                wrong = true;
                continue;
            }
            taken.push(name);
        }
        if wrong {
            return Ty::Invalid;
        }
        let at = spot(self.within(), span);
        self.types.taken.insert(at, taken);
        let function = Ty::Function(function);
        match self.one_owner {
            true => Ty::Owned(Box::new(function)),
            false => function,
        }
    }

    // -------------------------------------------------------------- Functions

    /// The signature of a function, checking its body first if that has not
    /// been done yet.
    fn signature(&mut self, function: &Rc<FunctionInfo>) -> Rc<Signature> {
        match function.state.borrow().clone() {
            State::Checked(signature) | State::Checking(signature) => return signature,
            State::NotChecked => {}
        }
        let decl = &function.decl;
        let span = function.span;
        let name = if decl.name.is_empty() { "function".to_string() } else { decl.name.clone() };
        // A mistake in a copy of a generic, in its first line as much
        // as in its body, says which copy it is in.
        self.copy_notes.push(function.copy_note.clone());

        let mut params = Vec::new();
        for (index, param) in decl.params.iter().enumerate() {
            let ty = match &function.self_type {
                // The first parameter of a method is the object itself.
                Some(self_type) if index == 0 && param.ty.is_none() => self_type.clone(),
                _ => {
                    let what = format!("the parameter `{}` of `{name}`", param.name);
                    let id = (decl_id(span, &decl.name), param.name.clone());
                    let ty = self.slot_type(&param.ty, &id, &what, &function.env, span, true);
                    let id = param.ty.is_none().then_some(id);
                    params.push(Slot { name: param.name.clone(), ty, has_default: param.default.is_some(), id });
                    continue;
                }
            };
            params.push(Slot { name: param.name.clone(), ty, has_default: param.default.is_some(), id: None });
        }
        // `function f(g: function() raises A, B)`: the type ends at the
        // comma, so `B` is a parameter with no type. If it has the name
        // of a kind of error, somebody meant a second kind (56.3).
        for pair in decl.params.windows(2) {
            let after_raises = pair[0].ty.as_ref().is_some_and(crate::parser::ends_in_raises);
            let known = function.env.get(&pair[1].name).map(|variable| variable.ty);
            if after_raises && pair[1].ty.is_none() && matches!(known, Some(Ty::ErrorType(_) | Ty::Class(_))) {
                self.error(crate::parser::ONE_KIND, span);
            }
        }
        let declared_return = decl.return_type.as_ref().map(|ty| self.resolve_returns(ty, &function.env, span));
        self.resource_signature(decl, &params, declared_return.as_ref(), span);
        for kind in &decl.raises {
            let known = function.env.get(kind).map(|variable| variable.ty);
            if !matches!(known, Some(Ty::ErrorType(_) | Ty::Class(_))) {
                self.error(format!("`{kind}` is not a kind of error"), span);
            }
        }

        // A function of an interface is all first line: there is no
        // body to check, and nothing to work out.
        let of_interface =
            matches!(function.self_type.clone().map(Ty::looked_at), Some(Ty::Instance(class)) if class.interface);
        if of_interface {
            // A default is a plain value, of the parameter's type
            // (decision 58.20). What a class writes is compared with it
            // in `stands_for`.
            for (param, slot) in decl.params.iter().zip(&params) {
                let Some(default) = &param.default else { continue };
                if self.plain(default, &function.env).is_none() {
                    self.error(
                        format!(
                            "the default of `{}` in `{name}` is `{}`. A default in an interface is a plain value: a number, \
                             text, `true`, `false`, `none`, a quantity such as `5 ms`, or a case of an enum that carries \
                             nothing. Each class that is the interface writes the same one",
                            param.name,
                            shown(default)
                        ),
                        default.span,
                    );
                    continue;
                }
                let given = self.expr(default, &function.env);
                let given = self.adapted(written(default), Arc::as_ptr(default) as usize, given, &slot.ty, default.span);
                self.expect(&given, &slot.ty, &format!("`{}`", param.name), default.span);
            }
            let returns = declared_return.unwrap_or(Ty::None);
            self.types.functions.insert(
                Arc::as_ptr(&function.decl) as usize,
                FunctionKind {
                    params: params.iter().map(|slot| kind(&slot.ty)).collect(),
                    returns: kind(&returns),
                    raises: decl.raises.clone(),
                },
            );
            self.types
                .looks
                .insert(Arc::as_ptr(&function.decl) as usize, params.iter().map(|slot| slot.ty.is_look()).collect());
            let signature = Rc::new(Signature { name, params, returns, raises: decl.raises.clone(), takes_out: false });
            *function.state.borrow_mut() = State::Checked(signature.clone());
            self.copy_notes.pop();
            return signature;
        }

        // Until the body is checked, callers (that is, recursive calls) see
        // what is written down. If no return type is written, they see what
        // the last pass worked out, the same way untyped parameters do.
        let return_id: SlotId = (decl_id(span, &decl.name), "->".to_string());
        let returns_for_now = match &declared_return {
            Some(declared) => declared.clone(),
            None => match self.known.get(&return_id).cloned() {
                Some(key) => self.from_key(&key),
                None if !self.last_round && !self.open.contains(&return_id) => Ty::Invalid,
                None => Ty::Dynamic,
            },
        };
        let declared = Rc::new(Signature {
            name: name.clone(),
            params: params.clone(),
            returns: returns_for_now,
            raises: decl.raises.clone(),
            // A method that calls itself is not known yet. Say it does,
            // which can only refuse.
            takes_out: function.self_type.is_some(),
        });
        *function.state.borrow_mut() = State::Checking(declared);

        let mut worked_out = HashMap::new();
        for slot in &params {
            if let Some(line) = slot.id.as_ref().and_then(|id| self.known_origins.get(id)) {
                if !slot.ty.is_loose() {
                    worked_out.insert(slot.name.clone(), (slot.ty.to_string(), *line));
                }
            }
        }
        let touched = std::mem::take(&mut self.touched);
        // Owned values belong to one function body, so what the body
        // around this one was tracking is set aside.
        let owned = std::mem::take(&mut self.owned);

        let scope = Scope::body(&function.env);
        self.early.push(Early {
            name: name.clone(),
            body: scope.function,
            outer: function.env.function,
            missing: not_yet(&function.above, &function.env),
            told: Vec::new(),
            literal: decl.name.is_empty(),
            captures: Vec::new(),
        });
        for (param, slot) in decl.params.iter().zip(&params) {
            if let Some(default) = &param.default {
                // A resource made by a default would be made at each call
                // that leaves it out, where no `let` owns it (59.2).
                if self.is_resource(&slot.ty.clone().looked_at()) && matches!(default.kind, ExprKind::Call { .. }) {
                    self.error(
                        format!(
                            "a default cannot make a resource: `{}` would be made at each call that leaves it \
                             out, where no `let` owns it. Pass one at the call instead",
                            param.name
                        ),
                        default.span,
                    );
                    self.receiving.insert(Arc::as_ptr(default) as usize);
                }
                let given = match slot.ty.is_look() {
                    // `xs: list[int] = [1, 2]`, where the parameter only
                    // looks: as for an argument, the list is made at each
                    // call that leaves it out, and the statement of that
                    // call owns it.
                    true => {
                        self.expecting(default, &slot.ty);
                        match self.expr(default, &scope) {
                            ty if ty.is_owned() => {
                                self.types.lent.insert(Arc::as_ptr(default) as usize);
                                lent(&ty)
                            }
                            ty => ty,
                        }
                    }
                    false => self.kept(default, &slot.ty, &scope),
                };
                let given = self.adapted(written(default), Arc::as_ptr(default) as usize, given, &slot.ty, default.span);
                self.sees(slot, &given, default.span);
                self.expect(&given, &slot.ty, &format!("`{}`", param.name), default.span);
            }
            self.declare(&scope, &param.name, slot.ty.clone(), false, span);
        }

        self.frames.push(Frame {
            declared_return: declared_return.clone(),
            returns: Vec::new(),
            loops: 0,
            unsafe_blocks: 0,
            takes_self: function.self_type.as_ref().filter(|_| decl.params.first().is_some_and(|p| p.name == "self")).map(|_| false),
        });
        self.raised.push(Vec::new());
        self.worked_out.push(worked_out);
        // How many mistakes there were before the body was read.
        let mistakes = self.errors.len();
        let named = !decl.name.is_empty();
        if named {
            self.within.push(Arc::as_ptr(decl) as usize);
        }
        let exits = self.statements(&decl.body, &scope);
        if named {
            self.within.pop();
        }
        // A body with a mistake in it may have a `raise` that was not
        // understood. It gets no second complaint about what it raises.
        let sound = self.errors.len() == mistakes;
        self.copy_notes.pop();
        self.captured = self.early.pop().map(|early| early.captures).unwrap_or_default();
        self.worked_out.pop();
        self.touched = touched;
        self.owned = owned;
        let raised = self.raised.pop().unwrap_or_default();
        let frame = self.frames.pop().expect("the frame pushed above");

        let returns = match declared_return {
            Some(declared) => {
                if !exits && !declared.accepts_none() {
                    self.error(
                        format!("`{name}` can reach its end without returning the {declared} it promises"),
                        span,
                    );
                }
                declared
            }
            // Not written down: it is whatever the `return`s give, plus
            // `none` if the end of the body can be reached.
            None => {
                let mut types = frame.returns;
                // `return a, b`: every `return` gives as many, and the end
                // of the body is not reached (decision 57).
                let counts: Vec<usize> = types
                    .iter()
                    .filter(|ty| !ty.is_loose())
                    .map(|ty| match ty {
                        Ty::Several(types) => types.len(),
                        _ => 1,
                    })
                    .collect();
                let most = counts.iter().copied().max().unwrap_or(1);
                if most > 1 && counts.iter().any(|&count| count != most) {
                    let fewest = counts.iter().copied().min().unwrap_or(1);
                    types = vec![self.error(
                        format!(
                            "`{name}` gives {most} values in one `return` and {fewest} in another. Every `return` gives as many"
                        ),
                        span,
                    )];
                } else if most > 1 && !exits {
                    types.push(self.error(format!("`{name}` can reach its end without giving its {most} values"), span));
                }
                if !exits {
                    types.push(Ty::None);
                }
                if !self.last_round {
                    // A `return` whose type is not settled yet (it hangs on
                    // a recursive call, say) is left out of this pass.
                    let unsettled = types.len();
                    types.retain(|ty| !matches!(ty, Ty::Invalid));
                    if types.is_empty() && unsettled > 0 {
                        types.push(Ty::Invalid);
                    }
                }
                let mut returns = self.join_all(&types).unwrap_or(Ty::None);
                if self.strict_at(span) && matches!(returns, Ty::Dynamic) && !types.iter().any(Ty::is_loose) {
                    // Each `return` is known, but they do not agree.
                    let mut kinds: Vec<String> = types.iter().map(Ty::to_string).collect();
                    kinds.dedup();
                    returns = self.error(
                        format!(
                            "strict mode: `{name}` returns different types ({}). Make them one type, as in `float(n)` or `1.0`",
                            kinds.join(", ")
                        ),
                        span,
                    );
                }
                match self.key(&returns) {
                    Some(Ok(key)) => {
                        self.seen.insert(return_id, Seen::Known(key));
                    }
                    Some(Err(())) => {
                        self.seen.insert(return_id, Seen::Unknowable);
                    }
                    None => {}
                }
                returns
            }
        };

        // In strict mode, or once a function says what it raises, the list
        // must be complete. Otherwise it is worked out, like a return type.
        //
        // Where errors travel as values, a fault stops the program on the
        // spot. A function that says it raises one, and has nothing in it
        // that can, would have an `except` waiting for ever in its caller
        // while `zeno run` catches the same fault (decision 5.19m).
        if self.firmware && sound {
            for declared in &decl.raises {
                let fault = match declared.as_str() {
                    "ZeroDivisionError" => "a division by zero",
                    "IndexError" => "a bad index",
                    "KeyError" => "a key that is not in the dict",
                    "OverflowError" => "a number that does not fit",
                    // `int("x")` travels as a value, and is in `raised`.
                    // What is left is a built-in that was handed a value
                    // it has no answer for.
                    "ValueError" => "a value that a built-in function has no answer for, such as `char(0)`,",
                    _ => continue,
                };
                if !raised.iter().any(|(kind, _)| kind == declared || kind == "Error") {
                    self.error(
                        format!(
                            "`{name}` says `raises {declared}`, but nothing in it raises one itself. In a firmware \
                             build {fault} stops the program where it happens, and cannot be caught by a caller. \
                             Test for it before it happens and take `raises {declared}` off, or build without `--firmware`"
                        ),
                        span,
                    );
                }
            }
        }
        let raises = if self.strict_at(span) || !decl.raises.is_empty() {
            for (kind, at) in &raised {
                if !decl.raises.iter().any(|declared| declared == "Error" || declared == kind) {
                    self.error(
                        format!(
                            "`{name}` can raise {kind} here but does not say so. Add `raises {kind}` to it, or catch the error"
                        ),
                        *at,
                    );
                }
            }
            decl.raises.clone()
        } else {
            let mut kinds: Vec<String> = raised.into_iter().map(|(kind, _)| kind).collect();
            kinds.sort();
            kinds.dedup();
            kinds
        };

        self.types.functions.insert(
            Arc::as_ptr(&function.decl) as usize,
            FunctionKind {
                params: params.iter().map(|slot| kind(&slot.ty)).collect(),
                returns: kind(&returns),
                raises: raises.clone(),
            },
        );
        self.types.looks.insert(Arc::as_ptr(&function.decl) as usize, params.iter().map(|slot| slot.ty.is_look()).collect());
        let signature = Rc::new(Signature { name, params, returns, raises, takes_out: frame.takes_self == Some(true) });
        *function.state.borrow_mut() = State::Checked(signature.clone());
        signature
    }

    // ----------------------------------------------------------- Expressions

    /// The type of an expression.
    fn expr(&mut self, expr: &Expr, env: &Env) -> Ty {
        let several = std::mem::take(&mut self.several);
        let ty = self.expr_inner(expr, env);
        // Several values are not a value (decision 57, rule 3).
        if let (Ty::Several(types), false) = (&ty, several) {
            self.not_one.insert(expr as *const Expr as usize);
            return self.error(several_not_one(expr, types.len()), expr.span);
        }
        if self.resource.is_some() {
            if let Some(refused) = self.resource_here(expr, &ty) {
                return refused;
            }
        }
        match &expr.kind {
            ExprKind::List(items) => {
                self.counted.insert(expr as *const Expr as usize, items.len() as i128);
            }
            ExprKind::Binary { op: BinaryOp::Mul, left, right } => {
                if let (Some(count), ExprKind::Int(times)) = (self.counted.get(&(&**left as *const Expr as usize)), &right.kind) {
                    let count = count * *times as i128;
                    self.counted.insert(expr as *const Expr as usize, count);
                }
            }
            _ => {}
        }
        if matches!(expr.kind, ExprKind::List(_) | ExprKind::ListComp { .. } | ExprKind::Call { .. } | ExprKind::Binary { .. })
            && is_bytes(&ty)
        {
            self.types.bytes.insert(expr as *const Expr as usize);
        }
        if let (ExprKind::Field { name, .. }, Ty::Optional(_)) = (&expr.kind, &ty) {
            // Remembered for the message, should this be used as if it
            // were there (see `none_first`).
            self.maybe_fields.insert((expr.span.line, expr.span.col), name.clone());
        }
        if let Ty::GenericCase(generic, _) = &ty {
            // The same for `let f = Just`.
            return self.error(
                format!(
                    "`{0}` is a case of the generic enum `{1}`. It can be called, as in `{0}(...)`, \
                     but it cannot be kept as a value or passed along yet",
                    ty_name(expr),
                    generic.name
                ),
                expr.span,
            );
        }
        if let Ty::Generic(generic) = &ty {
            // A generic interface is a type and nothing more (58.6).
            if matches!(&generic.template, Template::Class(decl) if decl.interface) {
                return self.error(
                    format!(
                        "`{0}` is an interface, and is written where a type goes, with its types: `{0}[{1}]`",
                        generic.name,
                        vec!["int"; generic.params.len()].join(", ")
                    ),
                    expr.span,
                );
            }
            // `let f = first` would need `f` to be every copy at once.
            return self.error(
                format!(
                    "`{0}` is generic. It can be called, as in `{0}(...)`, but it cannot be kept as a value or passed along yet",
                    generic.name
                ),
                expr.span,
            );
        }
        if self.strict_at(expr.span) && matches!(ty, Ty::Dynamic) {
            return self.error(
                "strict mode: the type of this cannot be worked out before the program runs",
                expr.span,
            );
        }
        ty
    }

    fn expr_inner(&mut self, expr: &Expr, env: &Env) -> Ty {
        let span = expr.span;
        let wrap_mode = std::mem::take(&mut self.wrap_mode);

        match &expr.kind {
            // Read by `return` and `Receive` themselves, never as one value.
            ExprKind::Several(values) => self.error(
                format!(
                    "{} values written with commas are given by `return`, or received with `let {} = ...`. \
                     A value that holds several things is an object: declare a class",
                    values.len(),
                    letters(values.len())
                ),
                span,
            ),
            ExprKind::Int(n) => {
                if *n > i64::MAX as u64 {
                    self.big_numbers.push((span, *n));
                }
                Ty::Int
            }
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Inline { language, text } => self.inline(language, text, env, span),
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::None => Ty::None,

            ExprKind::FormatStr(parts) => {
                for part in parts {
                    if let FormatPart::Value { value, .. } = part {
                        self.look(value, env);
                    }
                }
                Ty::Str
            }

            // A resource given away, which the checker wrote in place of
            // its name (59.1): read as the name is, by `take`.
            ExprKind::Give(name) => match env.get(name) {
                Some(variable) => self.read(name, variable, env, span),
                None => self.error(format!("`{name}` is not declared"), span),
            },
            ExprKind::Name(name) => match env.get(name) {
                Some(variable) => {
                    self.used(name, &variable, span);
                    if self.worked_out.last().is_some_and(|params| params.contains_key(name)) {
                        self.touched.push(name.clone());
                    }
                    if variable.declared.is_some() {
                        // The compiler needs to know that here the value
                        // is the narrower type: it stores the two differently.
                        self.types.narrowed.insert(expr as *const Expr as usize, kind(&variable.ty));
                    }
                    if let Some(mistake) = self.too_early(name, Some(&variable), env, span) {
                        return mistake;
                    }
                    if let (Ty::GenericCase(generic, _), Some(_)) = (&variable.ty, variable.case) {
                        return self.generic_bare_case(generic, name, expr, span);
                    }
                    self.read(name, variable, env, span)
                }
                None => match self.too_early(name, None, env, span) {
                    Some(mistake) => mistake,
                    // `allocate(bool, 4)`, `bool(1)`: the one type name
                    // that is not also a function that converts to it.
                    None if name == "bool" => self.error("`bool` is a type and not a value, so it cannot be used here", span),
                    None => match self.later.get(name) {
                        // Declared, but below this line. A name means
                        // what is above it, in a function too.
                        Some(line) if *line > span.line => self.error(
                            format!(
                                "`{name}` is declared on line {}, which is further down. A line can use only \
                                 what is declared above it, also inside a function. Move the declaration of `{name}` up",
                                *line % crate::token::LINES_A_FILE
                            ),
                            span,
                        ),
                        // The same thing under the name another
                        // language gives it.
                        _ => {
                            let zeno = match name.as_str() {
                                "null" | "nil" | "None" | "NULL" | "nullptr" | "undefined" => Some("none"),
                                "True" | "TRUE" => Some("true"),
                                "False" | "FALSE" => Some("false"),
                                _ => None,
                            };
                            match zeno {
                                Some(zeno) => self.error(format!("`{name}` is not defined. Zeno spells it `{zeno}`"), span),
                                None => self.error(format!("`{name}` is not defined"), span),
                            }
                        }
                    },
                },
            },

            ExprKind::Unit { value, unit } => {
                self.look(value, env);
                if !units::is_unit(unit) {
                    return self.error(units::unknown(unit), span);
                }
                Ty::Quantity(Rc::from(unit.as_str()))
            }

            ExprKind::List(items) => {
                // A place that says more about the items than they say
                // themselves: each is checked against it, and the list
                // is a list of what the place wants.
                let told = self.expected_items.take().filter(|(place, _)| *place == expr as *const Expr as usize);
                if let Some((_, wanted_item)) = told {
                    for item in items {
                        let item_type = self.kept(item, &wanted_item, env);
                        let item_type = self.adapted(written(item), item as *const Expr as usize, item_type, &wanted_item, item.span);
                        if !fits(&item_type, &wanted_item) {
                            self.error(
                                format!("this is a list of {wanted_item} here, but this item is {item_type}"),
                                item.span,
                            );
                        } else {
                            self.arrives(&item_type, &wanted_item, "an item of this list", item.span);
                        }
                    }
                    self.types.items.insert(expr as *const Expr as usize, kind(&wanted_item));
                    Ty::list(wanted_item)
                } else {
                    let types: Vec<Ty> = items.iter().map(|item| self.item(item, env)).collect();
                    // `[big, 3, 2]`: whole numbers written out beside a
                    // u64 are u64s too (decision 42.3), so the list is a
                    // list of u64s.
                    let beside_u64 = types.iter().any(|ty| matches!(ty.widened(), Ty::U64))
                        && types.iter().zip(items).all(|(ty, item)| {
                            matches!(ty.widened(), Ty::U64) || matches!(written(item), Some(Written::Number(..)))
                        });
                    if beside_u64 {
                        for (ty, item) in types.iter().zip(items) {
                            if matches!(ty.widened(), Ty::U64) {
                                continue;
                            }
                            if let Some(Written::Number(n, at)) = written(item) {
                                match u64_holds(n) {
                                    Ok(()) => {
                                        self.types.u64s.insert((at.line, at.col));
                                    }
                                    Err(message) => {
                                        self.error(message, item.span);
                                    }
                                }
                            }
                        }
                        Ty::list(Ty::U64)
                    } else {
                        // An empty list holds nothing yet, so its item type is open.
                        let mut joined = self.join_all(&types).unwrap_or(Ty::Dynamic);
                        if matches!(joined, Ty::Dynamic) && !types.iter().any(Ty::has_dynamic) {
                            let mut kinds: Vec<Ty> = Vec::new();
                            for ty in &types {
                                if !kinds.iter().any(|kind| same(kind, ty)) {
                                    kinds.push(ty.clone());
                                }
                            }
                            // `[]` says nothing, and is not of two types.
                            if kinds.len() > 1 {
                                self.mixed.insert(expr as *const Expr as usize, kinds);
                            }
                        }
                        // `[[1, none], []]`: an empty list beside other
                        // lists holds what they hold.
                        let empty = |item: &Expr| matches!(&item.kind, ExprKind::List(inner) if inner.is_empty());
                        let full: Vec<Ty> = types.iter().zip(items).filter(|(_, item)| !empty(item)).map(|(ty, _)| ty.clone()).collect();
                        if joined.has_dynamic() && !full.is_empty() && full.len() < types.len() {
                            let without = join(&full[0], &full[0]);
                            let without = full[1..].iter().fold(without, |sum, ty| join(&sum, ty));
                            if matches!(without.clone().looked_at(), Ty::List(_)) && !without.has_dynamic() {
                                self.types.items.insert(expr as *const Expr as usize, kind(&without));
                                joined = without;
                            }
                        }
                        // Two functions of one type, as `[add, function(n:
                        // int) -> int { return n * 2 }]`, are a list of that
                        // function type, as if it were written.
                        if joined.is_loose() {
                            if let Some(callable) = self.one_function_type(&types) {
                                self.types.items.insert(expr as *const Expr as usize, kind(&callable));
                                joined = callable;
                            }
                        }
                        // `[["a", none], [none]]`: the items beside it say
                        // what `[none]` holds, so a compiler is told.
                        if let Ty::List(item) = joined.clone().looked_at() {
                            for (ty, written) in types.iter().zip(items) {
                                if matches!(written.kind, ExprKind::List(_)) && asks_more(&item) && !same(ty, &joined) {
                                    self.types.items.insert(written as *const Expr as usize, kind(&item));
                                }
                            }
                        }
                        Ty::list(joined)
                    }
                }
            }
            ExprKind::Dict(entries) => self.dict_written(expr, entries, env),
            ExprKind::ListComp { element, variable, iterable, condition } => {
                let iterable_type = self.look(iterable, env);
                let item = self.item_type(&iterable_type, iterable.span);
                let walks = self.walk(iterable, &item, env);
                let scope = Scope::child(env);
                self.loop_variable(&scope, variable, item, span);
                // The element is worked out once for every item: a loop.
                let entry = self.enter_loop();
                if let Some(condition) = condition {
                    self.condition(condition, &scope);
                    narrow_all(&scope, &self.narrowings(condition, &scope).0);
                }
                let element = self.item(element, &scope);
                self.leave_loop(entry, false);
                self.owned.walked.truncate(walks);
                Ty::list(element)
            }

            ExprKind::Unary { op, operand } => {
                let ty = self.look(operand, env).widened();
                match (op, &ty) {
                    (_, ty) if ty.is_loose() => ty.clone(),
                    (UnaryOp::Neg, Ty::Int | Ty::Float | Ty::F32 | Ty::Quantity(_)) => ty,
                    (UnaryOp::Not, Ty::Bool) => Ty::Bool,
                    (UnaryOp::BitNot, Ty::Int) => Ty::Int,
                    (UnaryOp::BitNot, Ty::U64) => Ty::U64,
                    (UnaryOp::Neg, Ty::U64) => self.error(
                        "`-` does not work on a u64, which is never below 0. Make it an int first with `int( )`",
                        span,
                    ),
                    (UnaryOp::BitNot, _) => {
                        self.error(format!("`~` works on the bits of a whole number, and this is {ty}"), span)
                    }
                    (UnaryOp::Neg, _) => self.error(format!("`-` does not work on {ty}"), span),
                    (UnaryOp::Not, _) => self.error(format!("`not` needs a bool, but this is {ty}"), span),
                }
            }

            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                let left_type = self.look(left, env);
                // The right side only runs when the left side did not settle
                // it, so it can rely on what the left side established:
                //     x is not none and x.upper() == "A"
                let scope = Scope::child(env);
                let (when_true, when_false) = self.narrowings(left, env);
                narrow_all(&scope, if *op == BinaryOp::And { &when_true } else { &when_false });
                let right_type = self.look(right, &scope);

                for (ty, operand) in [(left_type, left), (right_type, right)] {
                    if !fits(&ty, &Ty::Bool) {
                        self.error(format!("`{}` needs bools, but this is {ty}", op.text()), operand.span);
                    }
                }
                Ty::Bool
            }
            ExprKind::Binary { op, left, right } => {
                let (tested, right_side) = (left, right);
                // Straight inside `u64.wrap( )`, these four cut their
                // answer to 64 bits, and so do the same four inside them.
                let wraps = wrap_mode && matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Shl);
                // `m == Nothing`, `Nothing in found`: a bare case of a
                // generic enum is the copy the other side says (58.1).
                let compares = matches!(op, BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::In | BinaryOp::NotIn);
                let (left_type, right_type) = if compares && bare_generic_case(left, env) && !bare_generic_case(right, env) {
                    let right_type = self.look(right, env);
                    let beside = match (op, right_type.clone().looked_at()) {
                        (BinaryOp::In | BinaryOp::NotIn, Ty::List(item) | Ty::Fixed(item, _)) => *item,
                        (_, other) => other,
                    };
                    self.expecting(left, &beside);
                    (self.look(left, env), right_type)
                } else {
                    self.wrap_mode = wraps;
                    let left_type = self.look(left, env);
                    if compares && matches!(op, BinaryOp::Eq | BinaryOp::NotEq) && bare_generic_case(right, env) {
                        self.expecting(right, &left_type.clone().looked_at());
                    }
                    self.wrap_mode = wraps;
                    (left_type, self.look(right, env))
                };
                self.wrap_mode = false;
                // `0.5 in readings`: the 0.5 is beside the items.
                let beside_left = match (op, right_type.clone().widened().looked_at()) {
                    (BinaryOp::In | BinaryOp::NotIn, Ty::List(item) | Ty::Fixed(item, _)) => *item,
                    _ => right_type.clone(),
                };
                // `[none] in grid`: the items of `grid` say what the list
                // written on the left holds, so a compiler is told.
                if let (BinaryOp::In | BinaryOp::NotIn, ExprKind::List(_), Ty::List(element)) =
                    (op, &tested.kind, beside_left.clone().looked_at())
                {
                    if asks_more(&element) && !same(&left_type.clone().looked_at(), &beside_left.clone().looked_at()) {
                        self.types.items.insert(&**tested as *const Expr as usize, kind(&element));
                    }
                }
                let left = self.real_beside(left_type.clone(), &beside_left, tested);
                let right = self.real_beside(right_type, &left_type, right_side);
                // A name that may be `none` is known to be there after a
                // test or a new value. Testing it again is not a mistake,
                // so `is none` looks at the type it was declared with.
                let left = match (op, &tested.kind) {
                    (BinaryOp::Is | BinaryOp::IsNot, ExprKind::Name(name)) => {
                        env.get(name).and_then(|variable| variable.declared).unwrap_or(left)
                    }
                    _ => left,
                };
                // If it is the right of the two that is a field which may
                // be `none`, the message is about that field (`none_first`).
                if let Some(field) = self.maybe_fields.get(&(right_side.span.line, right_side.span.col)).cloned() {
                    self.maybe_fields.entry((span.line, span.col)).or_insert(field);
                }
                let ty = self.binary(*op, &left, &right, span);
                if wraps && matches!(ty, Ty::U64) {
                    let function = match op {
                        BinaryOp::Add => "u64..add",
                        BinaryOp::Sub => "u64..sub",
                        BinaryOp::Mul => "u64..mul",
                        _ => "u64..shl",
                    };
                    self.types.wrapping.insert((right_side.span.line, right_side.span.col), function);
                }
                if *op == BinaryOp::Add && owns_items(&ty) {
                    return self.error(
                        "`+` would put each item of these lists in a second list, and each has one owner. \
                         Move the items across one at a time with `pop()` and `append`",
                        span,
                    );
                }
                if matches!(op, BinaryOp::Add | BinaryOp::Mul) && matches!(ty, Ty::List(_)) {
                    // `a + b` on two lists makes a third, and `a * 3` a second.
                    self.types.made.insert(expr as *const Expr as usize);
                }
                ty
            }
            ExprKind::Compare { first, rest } => {
                let mut left = self.look(first, env);
                let mut left_expr: &Expr = first;
                for (op, operand) in rest {
                    let right = self.look(operand, env);
                    let (left_type, right_type) =
                        (self.real_beside(left.clone(), &right, left_expr), self.real_beside(right.clone(), &left, operand));
                    self.binary(*op, &left_type, &right_type, span);
                    left = right_type;
                    left_expr = operand;
                }
                Ty::Bool
            }

            ExprKind::Function(decl) => {
                let function = Rc::new(FunctionInfo {
                    decl: decl.clone(),
                    env: env.clone(),
                    span,
                    self_type: None,
                    state: RefCell::new(State::NotChecked),
                    // It is made where it is written, so nothing it can
                    // see is still to come.
                    above: Rc::from(Vec::new()),
                    copy_note: None,
                });
                let kept = self.keeping.take() == Some(expr as *const Expr as usize);
                self.signature(&function);
                let captures = std::mem::take(&mut self.captured);
                if kept && !captures.is_empty() {
                    return self.takes_what_it_uses(function, captures, env, span);
                }
                let mut looks = false;
                for capture in captures {
                    looks = true;
                    if capture.function != env.function {
                        // It belongs to a function further out, so the
                        // function this one is written in looks at it
                        // too, if it may.
                        let around = self.early.iter_mut().rev().find(|early| early.body == env.function);
                        match around.filter(|early| early.literal) {
                            Some(early) => {
                                if !early.captures.iter().any(|known| known.id == capture.id) {
                                    early.captures.push(capture);
                                }
                            }
                            None => {
                                self.error(elsewhere(&capture.name, false), span);
                            }
                        }
                        continue;
                    }
                    if let Some((_, at)) = self.owned.moved.get(&capture.id) {
                        self.error(
                            format!(
                                "`{}` was given away on line {} and holds nothing now, and this function uses it. \
                                 Use it before it goes, or give it only to what looks: a parameter without `take`",
                                capture.name,
                                at.line_in_file()
                            ),
                            span,
                        );
                    }
                    // This line is looking at it from here on.
                    self.owned.looking.push(capture.id);
                }
                let function = Ty::Function(function);
                if looks {
                    // A function that looks at a value with one owner
                    // must be gone before the value is. So it is a look
                    // itself: it can be called or lent on this line, and
                    // not kept. The statement owns it.
                    if self.one_owner {
                        self.types.lent.insert(expr as *const Expr as usize);
                    }
                    Ty::Borrowed(Box::new(function))
                } else if self.one_owner {
                    // It is kept in a block of its own, and whoever
                    // receives it owns that.
                    Ty::Owned(Box::new(function))
                } else {
                    function
                }
            }

            ExprKind::Call { callee, args } => {
                // If the place this call's result goes was noted, take the
                // note now: the arguments may leave notes of their own.
                let wanted = match self.expected.take() {
                    Some((call, wanted)) if call == expr as *const Expr as usize => Some(wanted),
                    _ => None,
                };
                // The owner a method takes something out of, and how many
                // looks the line had made by then.
                let mut taker = None;
                let callee = match &callee.kind {
                    // `object.method(...)`: the call looks at the object.
                    ExprKind::Field { object: receiver, name, .. } => {
                        let looked = self.owned.looking.len();
                        let (object, length) = self.look_fixed(receiver, env);
                        if !self.keeps_length(name, length, callee.span) {
                            return Ty::Invalid;
                        }
                        if matches!(name.as_str(), "pop" | "clear") && owns_items(&object) {
                            // The item leaves the list, and whoever
                            // receives it is its owner from here. `clear`
                            // takes every item out, and frees it.
                            if let Some(owner) = self.takes_out(receiver, looked, "take an item out of this list", env, span) {
                                self.owned.emptied.push(owner);
                            }
                        }
                        // The same for a dict that owns its values (56.12).
                        if self.one_owner && matches!(name.as_str(), "pop" | "clear") && owns_values(&object) {
                            if let Some(owner) = self.takes_out(receiver, looked, "take a value out of this dict", env, span) {
                                self.owned.emptied.push(owner);
                            }
                        }
                        let method = self.field(&object, name, callee.span);
                        // A method that takes out what its object owns:
                        // calling it is taking out, for whoever calls.
                        if let Ty::BoundMethod(function) = &method {
                            if object.is_tied() {
                                let does = match self.taken_through(function) {
                                    // Through an interface: some class that
                                    // is it takes out (58.21).
                                    Some((class, interface)) => Some(format!(
                                        "call `{name}`, which takes out or replaces something the object owns \
                                         (`{class}` is a `{interface}`, and its `{name}` does)"
                                    )),
                                    None if self.signature(function).takes_out => {
                                        Some(format!("call `{name}`, which takes out or replaces something the object owns"))
                                    }
                                    None => None,
                                };
                                if let Some(does) = does {
                                    taker = self.takes_out(receiver, looked, &does, env, span).map(|owner| (owner, self.owned.looking.len()));
                                }
                            }
                        }
                        match method {
                            Ty::Dynamic if self.strict_at(span) => self.error(
                                "strict mode: the type of this cannot be worked out before the program runs",
                                callee.span,
                            ),
                            ty => ty,
                        }
                    }
                    // A generic is only ever met here, by its own name.
                    ExprKind::Name(_) => match self.expr_inner(callee, env) {
                        Ty::Dynamic if self.strict_at(span) => self.expr(callee, env),
                        ty => ty,
                    },
                    _ => self.expr(callee, env),
                };
                // Calling a function that has an owner only looks at it.
                // One that a call has just made, as in `make(2)("5")`, is
                // the statement's, which frees it when it is over.
                let callee = match callee {
                    Ty::Borrowed(_) => self.looked(callee, &**args_callee(expr)),
                    callee if callee.is_owned() => self.looked(callee, &**args_callee(expr)),
                    callee => callee,
                };
                // Where each argument is going decides what passing it
                // means, so find that out before looking at any of them.
                let slots = self.slots(&callee);
                let only_looks = matches!(callee, Ty::Builtin(_));
                let mut arguments = Vec::new();
                // Bare cases of a generic enum given to a generic, settled last.
                let mut later = Vec::new();
                let (mut named_seen, mut misplaced) = (false, false);
                for (position, arg) in args.iter().enumerate() {
                    if arg.name.is_some() {
                        named_seen = true;
                    } else if named_seen && !misplaced {
                        misplaced = true;
                        self.error("plain arguments must come before named ones", arg.value.span);
                    }
                    let slot = slots.as_ref().and_then(|slots| match &arg.name {
                        None => slots.get(position),
                        Some(name) => slots.iter().find(|slot| slot.name == *name),
                    });
                    // `u64.wrap(h * prime)`: the product is cut to 64 bits.
                    self.wrap_mode = matches!(callee, Ty::Builtin("u64..of"));
                    // What a type parameter stands for is known only once
                    // every argument has been read. So a generic's field,
                    // and a parameter written `take T`, take the value, and
                    // a parameter written `T` looks at it (decision 56.11).
                    let generic_keeps = match &callee {
                        Ty::GenericCase(..) => Some(true),
                        Ty::Generic(generic) => match &generic.template {
                            Template::Class(_) | Template::Enum(_) => Some(true),
                            Template::Function(decl) => Some(
                                match &arg.name {
                                    None => decl.params.get(position),
                                    Some(name) => decl.params.iter().find(|param| param.name == *name),
                                }
                                .is_some_and(|param| matches!(param.ty, Some(ast::Type::Take(_)))),
                            ),
                        },
                        _ => None,
                    };
                    // `Link(3, End)`, `or_else(Nothing, 5)`: a bare case of
                    // a generic enum, given to a generic, says nothing about
                    // `T` by itself. It is settled once the other arguments
                    // have settled `T` (decision 58.1).
                    if let (Some(keeps), ExprKind::Name(name)) = (generic_keeps, &arg.value.kind) {
                        if let Some(variable) = env.get(name).filter(|variable| variable.case.is_some()) {
                            if let Ty::GenericCase(of, _) = &variable.ty {
                                self.used(name, &variable, arg.value.span);
                                later.push(Later { at: arguments.len(), generic: of.clone(), name: name.clone(), keeps });
                                arguments.push(Argument {
                                    name: arg.name.clone(),
                                    ty: Ty::Invalid,
                                    span: arg.value.span,
                                    written: None,
                                    place: &arg.value as *const Expr as usize,
                                });
                                continue;
                            }
                        }
                    }
                    let ty = match slot {
                        // A parameter that takes an owned value keeps it:
                        // the caller's variable gives it up.
                        Some(slot) if slot.ty.is_owned() => self.kept(&arg.value, &slot.ty, env),
                        // `print`, `len` and the rest only look.
                        _ if only_looks => self.look(&arg.value, env),
                        // So does a parameter without `take`. An owned
                        // value made in the argument itself is lent for
                        // the length of the statement, which owns it.
                        Some(slot) if slot.ty.is_look() => {
                            self.expecting(&arg.value, &slot.ty);
                            match self.expr(&arg.value, env) {
                                ty if ty.is_owned() => {
                                    self.types.lent.insert(&arg.value as *const Expr as usize);
                                    lent(&ty)
                                }
                                ty => ty,
                            }
                        }
                        _ if generic_keeps == Some(true) => self.take(&arg.value, env),
                        _ if generic_keeps == Some(false) => match self.expr(&arg.value, env) {
                            ty if ty.is_owned() => {
                                self.types.lent.insert(&arg.value as *const Expr as usize);
                                lent(&ty)
                            }
                            ty => ty,
                        },
                        // Anything else is checked against the parameter
                        // by `call`. A look at an owned value fits there
                        // only if the parameter looks.
                        _ => {
                            if let Some(slot) = slot {
                                self.expecting(&arg.value, &slot.ty);
                            }
                            self.expr(&arg.value, env)
                        }
                    };
                    arguments.push(Argument {
                        name: arg.name.clone(),
                        ty,
                        span: arg.value.span,
                        written: written(&arg.value),
                        place: &arg.value as *const Expr as usize,
                    });
                }
                // Which parameter a plain argument is for is counted from
                // the left, so after one out of order nothing more can be
                // said about the call. One mistake gets one message.
                if misplaced {
                    return Ty::Invalid;
                }
                if let Ty::Generic(generic) = &callee {
                    return self.call_generic(generic, arguments, &later, expr, wanted, span);
                }
                if let Ty::GenericCase(generic, index) = &callee {
                    return self.call_generic_case(generic, *index, arguments, &later, expr, wanted, span);
                }
                // An argument that looks at the same object would be
                // looking while the method takes something out of it.
                if let Some((owner, from)) = taker {
                    if self.owned.looking[from.min(self.owned.looking.len())..].contains(&owner) {
                        self.error(
                            "this method takes out or replaces something its object owns, and an argument on this line \
                             looks at the same object. Work the argument out on a line of its own first",
                            span,
                        );
                    }
                }
                let ty = match self.numbered(&callee, &mut arguments, span) {
                    Some(ty) => ty,
                    None => self.call(&callee, arguments, span),
                };
                // `c.library("m", "double cbrt(double x);")`, written out:
                // its functions are known from here (decision 44.10).
                if matches!(callee, Ty::Builtin("c.library")) && !matches!(ty, Ty::Invalid) {
                    if let Some(library) = self.c_library(args, span) {
                        return Ty::CLibrary(library);
                    }
                }
                if matches!(callee, Ty::Builtin("program.arguments" | "files.read_bytes" | "files.handle_bytes" | "files.list" | "range")) {
                    // The list of arguments, the bytes of a file, the
                    // names in a folder and the numbers of a range, are
                    // new at each call.
                    self.types.made.insert(expr as *const Expr as usize);
                }
                if let (Ty::BuiltinMethod(_, name), Ty::List(_)) = (&callee, &ty) {
                    if matches!(&**name, "split" | "lines" | "map" | "filter" | "slice" | "keys" | "values") {
                        // Each of these makes a new list.
                        self.types.made.insert(expr as *const Expr as usize);
                    }
                }
                ty
            }

            ExprKind::Field { object, name, at } => {
                if let Some(ty) = self.method_value(expr, object, name, *at, env) {
                    return ty;
                }
                let object_type = self.expr(object, env);
                let tied = object_type.is_tied();
                let length = fixed_length(&object_type.clone().looked_at());
                let object = self.looked(object_type, object);
                if !self.keeps_length(name, length, span) {
                    return Ty::Invalid;
                }
                let found = self.field(&object, name, span);
                self.field_used(name, &found, *at);
                match found {
                    // The method would carry the object away inside it.
                    Ty::BoundMethod(_) | Ty::BuiltinMethod(..) if tied => self.error(
                        format!("a method of a value that has one owner cannot be kept as a value. Call it here: `.{name}(...)`"),
                        span,
                    ),
                    // An owned value in a field stays where it is. This
                    // is a look at it.
                    ty if ty.is_owned() => lent(&ty),
                    ty => ty,
                }
            }
            ExprKind::Take(place) => self.take_out(place, env),
            ExprKind::Index { object, index } => {
                let (object, length) = self.look_fixed(object, env);
                self.index_inside(index, length);
                let index_type = self.look(index, env);
                if let Ty::Dict(key, value) = &object {
                    // `ages["Ada"]`: the value kept under that key. One
                    // that has one owner stays in the dict, which owns
                    // it, and this is a look at it (decision 56.12).
                    self.expect(&index_type.widened(), key, "a key of this dict", index.span);
                    if self.one_owner && value.is_owned() {
                        self.dict_values.insert(expr as *const Expr as usize);
                        return lent(value);
                    }
                    return (**value).clone();
                }
                // What is not known may be a dict, whose keys can be text.
                if !object.is_loose() {
                    self.expect(&index_type, &Ty::Int, "an index", index.span);
                }
                match object {
                    // An item that has one owner stays where it is. This
                    // is a look at it.
                    Ty::List(item) if item.is_owned() => lent(&item),
                    Ty::List(item) => *item,
                    Ty::Str => Ty::Str,
                    Ty::Pointer(pointee) => {
                        self.needs_unsafe("reading through a pointer", span);
                        pointee.ty()
                    }
                    ty if ty.is_loose() => ty,
                    Ty::Optional(_) => self.error(
                        format!("this is {object}. {}", self.none_first(span, "Check it with `is not none` before taking an item")),
                        span,
                    ),
                    ty => self.error(format!("an item cannot be taken from {ty}"), span),
                }
            }
        }
    }

    /// `take slot.task`, `take tasks[i]`: what the place holds, which is
    /// left holding `none` (decision 56.10). So the place must be one
    /// that may hold `none`. A value that has one owner leaves the object
    /// or the list it was in, and the guards of `takes_out` hold for it
    /// as for `pop()`: only the owner may, not while a loop walks it, and
    /// not on a line that has already looked at it. Whoever receives it
    /// owns it from here.
    fn take_out(&mut self, place: &Expr, env: &Env) -> Ty {
        let looked = self.owned.looking.len();
        let (inside, declared) = match &place.kind {
            ExprKind::Field { object, name, at } => {
                let object_type = self.expr(object, env);
                let object_type = self.looked(object_type, object);
                let found = self.field(&object_type, name, place.span);
                self.field_used(name, &found, *at);
                if !matches!(found, Ty::Optional(_) | Ty::Invalid) && !found.is_loose() {
                    let message = match &found {
                        Ty::BoundMethod(_) | Ty::BuiltinMethod(..) => {
                            format!("`{name}` is a method. `take` goes in front of a field or an item, and leaves `none` there")
                        }
                        ty => format!(
                            "`take` leaves `none` in `{name}`, and `{name}` is written `{ty}`, which does not allow \
                             `none`. If it may be empty, declare it as `{name}: {ty} or none`"
                        ),
                    };
                    return self.error(message, place.span);
                }
                (object, found)
            }
            ExprKind::Index { object, index } => {
                let (object_type, length) = self.look_fixed(object, env);
                self.index_inside(index, length);
                let index_type = self.look(index, env);
                let item = match object_type {
                    Ty::List(item) => *item,
                    ty if ty.is_loose() => return ty,
                    Ty::Dict(..) => {
                        return self.error(
                            "`take` takes an item out of a list or a field out of an object, and not a value out of \
                             a dict yet",
                            place.span,
                        )
                    }
                    Ty::Str => {
                        return self.error(
                            "`take` leaves `none` in the place of an item, and text is never changed. Read the \
                             character with `[ ]`",
                            place.span,
                        )
                    }
                    ty => return self.error(format!("an item cannot be taken from {ty}"), place.span),
                };
                self.expect(&index_type, &Ty::Int, "an index", index.span);
                if !matches!(item, Ty::Optional(_) | Ty::Invalid) && !item.is_loose() {
                    return self.error(
                        format!(
                            "`take` leaves `none` in the place of the item, and the items of this list are written \
                             `{item}`, which does not allow `none`. To take the item out, use `pop()`. To leave \
                             `none` in its place, make it a `list[{item} or none]`"
                        ),
                        place.span,
                    );
                }
                (object, item)
            }
            // The parser lets nothing else through.
            _ => return Ty::Invalid,
        };
        if declared.is_owned() {
            self.takes_out(inside, looked, "take out what is there", env, place.span);
        }
        declared
    }

    /// Is `name` a field or a method that `class` keeps to the file it
    /// is written in, and is `span` in another file? Then this says so.
    fn kept_private(&mut self, class: &ClassInfo, name: &str, span: Span) -> bool {
        let kept = span.file() != class.file && class.private.iter().any(|private| private == name);
        if kept {
            self.error(
                format!(
                    "`{name}` is private in the class `{}`: only the file that declares the class can use it. \
                     To use it from another file, take the word `private` off where it is declared",
                    class.name
                ),
                span,
            );
        }
        kept
    }

    /// The type of `object.name`.
    fn field(&mut self, object: &Ty, name: &str, span: Span) -> Ty {
        match object {
            Ty::CLibrary(library) => match library.functions.iter().position(|function| function.name == name) {
                // `hal.GPIO_PIN_5`: a value, read where it is named.
                Some(at) if library.functions[at].value => c_given(&library.functions[at].gives.1),
                Some(at) => Ty::CFunction(library.clone(), at),
                None => {
                    let declared: Vec<&str> = library.functions.iter().map(|function| function.name.as_str()).collect();
                    if library.block.is_some() {
                        // An `inline c` block declares nothing: it has
                        // the functions it writes out.
                        return self.error(
                            format!(
                                "this `inline c` block has no function `{name}` that Zeno can call. It has: {}. \
                                 A function marked `static` is the block's own",
                                declared.join(", ")
                            ),
                            span,
                        );
                    }
                    self.error(
                        format!(
                            "the library `{}` has no `{name}` among what is declared for it. Declared: {}",
                            library.label,
                            declared.join(", ")
                        ),
                        span,
                    )
                }
            },
            Ty::Instance(class) => {
                if self.kept_private(class, name, span) {
                    return Ty::Invalid;
                }
                if let Some(field) = class.fields.borrow().iter().find(|f| f.name == name) {
                    return field.ty.clone();
                }
                // It runs once, by itself, when the owner ends (59.1).
                if name == "release" && self.is_resource(object) {
                    return self.error(
                        format!(
                            "`release` runs by itself, once, when the owner of this `{0}` ends, so it cannot be \
                             called. To end it sooner, give it to a function that takes it: `function done(held: \
                             take {0}) {{ }}`",
                            class.name
                        ),
                        span,
                    );
                }
                if let Some(method) = class.methods.borrow().get(name) {
                    return Ty::BoundMethod(method.clone());
                }
                if class.interface {
                    // The object has more than its interface says, and
                    // what that is depends on its class.
                    let mut functions: Vec<String> = class.methods.borrow().keys().cloned().collect();
                    functions.sort();
                    let has = match functions.is_empty() {
                        true => "no functions".to_string(),
                        false => crate::modules::listed(&functions),
                    };
                    return self.error(
                        format!(
                            "`{name}` is not a function of the interface `{0}`, which has {has}. \
                             What else this object has depends on its class, and that is only known when \
                             the program runs. Give `{0}` a function for it",
                            class.name
                        ),
                        span,
                    );
                }
                self.error(format!("`{}` has no field or method `{name}`", class.name), span)
            }
            // Which case the value is, is only known when the program
            // runs, and a case that is not the one asked has no such
            // field. `match` is the place where the case is known.
            Ty::EnumValue(info) => {
                let cases = info.cases.borrow();
                match cases.iter().find(|case| case.fields.iter().any(|f| f.name == name)) {
                    Some(case) => {
                        let places: Vec<&str> =
                            case.fields.iter().map(|f| if f.name == name { name } else { "_" }).collect();
                        let message = format!(
                            "`{name}` belongs to the case `{0}`, and which case a `{1}` is, is only known inside a `match`. \
                             Take it out there: `match ... {{ case {0}({2}) {{ ... }} }}`",
                            case.name,
                            info.name,
                            places.join(", ")
                        );
                        drop(cases);
                        self.error(message, span)
                    }
                    None => {
                        drop(cases);
                        self.error(format!("no case of `{}` has a field `{name}`", info.name), span)
                    }
                }
            }
            Ty::Enum(info) => {
                let index = info.cases.borrow().iter().position(|case| case.name == name);
                match index {
                    Some(index) if info.cases.borrow()[index].fields.is_empty() => Ty::EnumValue(info.clone()),
                    Some(index) => Ty::CaseConstructor(info.clone(), index),
                    None => self.error(format!("enum `{}` has no case `{name}`", info.name), span),
                }
            }
            Ty::Module(module) => match self.member(module, name, span) {
                Some(ty) => ty,
                None => self.error(no_member(module, name), span),
            },
            // `u8.max`, `int.min`, `u64.max`: the number itself (42.7).
            Ty::Builtin(of) if matches!(name, "max" | "min") && limit_of(of, name, span).is_some() => {
                let value = limit_of(of, name, span).unwrap_or(Expr { kind: ExprKind::None, span });
                self.types.limits.insert((span.line, span.col), value);
                match *of {
                    "int" => Ty::Int,
                    "u64" => Ty::U64,
                    sized => ints::width(sized).map_or(Ty::Invalid, Ty::Sized),
                }
            }
            // `u64.wrap`: the 64 bits of an int, or a u64 sum cut to 64
            // bits (42.6). It becomes `u64..of`, a name only `checked`
            // writes, so that no engine needs a field of a function.
            Ty::Builtin("u64") if name == "wrap" => {
                let callee = Expr { kind: ExprKind::Name("u64..of".to_string()), span };
                self.types.limits.insert((span.line, span.col), callee);
                Ty::Builtin("u64..of")
            }
            // `u8.wrap`: the conversion that keeps the low bits.
            Ty::Builtin(sized) if name == "wrap" && ints::width(sized).is_some() => {
                ints::width(sized).map_or(Ty::Invalid, |width| Ty::Builtin(width.wrap))
            }
            Ty::Error(_) if name == "message" || name == "kind" => Ty::Str,
            // `(3 s).ms`: the same quantity in another unit.
            Ty::Quantity(unit) if units::is_unit(name) => match units::convert(unit, name) {
                Some(_) => Ty::Quantity(Rc::from(name)),
                None => self.error(
                    format!("a quantity in {unit} cannot be turned into {name}. They measure different things"),
                    span,
                ),
            },
            Ty::Str if STR_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::Str), Rc::from(name)),
            Ty::List(_) if LIST_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(object.clone()), Rc::from(name))
            }
            Ty::Dict(..) if DICT_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(object.clone()), Rc::from(name))
            }
            Ty::Pin if PIN_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::Pin), Rc::from(name)),
            Ty::Register(_) if REGISTER_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(object.clone()), Rc::from(name))
            }
            Ty::Register(_) if name == "address" => Ty::Int,
            // `led.number`: which pin this is.
            Ty::Pin if name == "number" => Ty::Int,
            Ty::InputPin if INPUT_PIN_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(Ty::InputPin), Rc::from(name))
            }
            Ty::InputPin if name == "number" => Ty::Int,
            Ty::PwmPin if PWM_PIN_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::PwmPin), Rc::from(name)),
            Ty::PwmPin if name == "number" => Ty::Int,
            Ty::PwmPin => self.error(
                format!(
                    "a PwmPin has no `{name}`. It has `duty(share)`, `pulse(time)` and `number`. \
                     All the time on is `duty(1.0)`, and off is `duty(0.0)`"
                ),
                span,
            ),
            Ty::Interrupt if INTERRUPT_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::Interrupt), Rc::from(name)),
            Ty::Interrupt if name == "number" => Ty::Int,
            Ty::Interrupt => self.error(
                format!(
                    "an Interrupt has no `{name}`. It has `wait()`, `edges()` and `number`. \
                     The level of its pin is read from the InputPin, with `is_high()` or `is_low()`"
                ),
                span,
            ),
            Ty::Ticker if TICKER_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::Ticker), Rc::from(name)),
            // `beat.period`: the time from one tick to the next, in the
            // unit `now()` counts in (49.2).
            Ty::Ticker if name == "period" => Ty::Quantity(Rc::from("ms")),
            Ty::Ticker => self.error(
                format!("a Ticker has no `{name}`. It has `wait()`, `ticks()`, `restart()` and `period`"),
                span,
            ),
            Ty::AnalogPin if ANALOG_PIN_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(Ty::AnalogPin), Rc::from(name))
            }
            // `sensor.number`, and `sensor.max_count`: the count that
            // stands for the whole reference voltage (47.2).
            Ty::AnalogPin if name == "number" || name == "max_count" => Ty::Int,
            Ty::AnalogPin => self.error(
                format!(
                    "an AnalogPin has no `{name}`. It has `voltage()`, `count()`, `max_count` and `number`. \
                     A pin read as high or low is made with `input_pin( )`"
                ),
                span,
            ),
            Ty::Serial if SERIAL_METHODS.contains(&name) => Ty::BuiltinMethod(Box::new(Ty::Serial), Rc::from(name)),
            Ty::Serial if name == "number" => Ty::Int,
            Ty::Serial => self.error(
                format!(
                    "a Serial has no `{name}`. It has `write(text)`, `write_bytes(bytes)`, `read_byte()`, `has_byte()` and `number`"
                ),
                span,
            ),
            // `ticking.stop()`: the one thing a timer does.
            Ty::Timer if name == "stop" => Ty::BuiltinMethod(Box::new(Ty::Timer), Rc::from(name)),
            Ty::Timer => self.error(format!("a Timer has no `{name}`. It has `stop()`, which calls off what is waiting"), span),
            // An input is read and an output is driven (45.1, 45.2).
            Ty::InputPin if PIN_METHODS.contains(&name) => self.error(
                format!(
                    "an InputPin is read and not driven, so it has no `{name}`. Read it with `is_high()` or `is_low()`. \
                     A pin the program drives is made with `pin( )`"
                ),
                span,
            ),
            Ty::Pin if INPUT_PIN_METHODS.contains(&name) => self.error(
                format!(
                    "`{name}` reads an input, and this Pin is driven by the program. Ask it what it was driven to with \
                     `is_on()`. A pin the program reads is made with `input_pin( )`"
                ),
                span,
            ),
            Ty::Element if crate::builtins::ELEMENT_METHODS.contains(&name) => {
                Ty::BuiltinMethod(Box::new(Ty::Element), Rc::from(name))
            }
            // `title.text`, `name.value`: what an element shows, and
            // what has been typed into it.
            Ty::Element if crate::builtins::ELEMENT_FIELDS.contains(&name) => Ty::Str,
            Ty::Element => self.error(
                format!(
                    "an Element has no field or method `{name}`. It has `text` and `value`, and the methods `{}`",
                    crate::builtins::ELEMENT_METHODS.join("`, `")
                ),
                span,
            ),
            Ty::Event => match name {
                "kind" | "key" => Ty::Str,
                "target" => Ty::Element,
                "x" | "y" => Ty::Float,
                _ => self.error(
                    format!(
                        "an Event has no field `{name}`. It has `{}`",
                        crate::value::EVENT_FIELDS.join("`, `")
                    ),
                    span,
                ),
            },
            Ty::Optional(_) => self.error(
                format!(
                    "this is {object}. {}",
                    self.none_first(span, &format!("Check it with `is not none` before using `.{name}`"))
                ),
                span,
            ),
            ty if ty.is_loose() => ty.clone(),
            // Somebody who knows lists reaches for `append`.
            ty @ Ty::Dict(..) => self.error(
                format!(
                    "{ty} has no method `{name}`. A dict has `get`, `pop`, `keys`, `values` and `clear`, \
                     and a value is put in with `d[key] = value`"
                ),
                span,
            ),
            ty => self.error(format!("{ty} has no field or method `{name}`"), span),
        }
    }

    // ----------------------------------------------------------------- Calls

    /// `function` was given to something that will call it later with
    /// values of these types, as the page calls what `on` was given.
    /// Check that it can be called so.
    ///
    /// The check is the one a call written in the program gets. What it
    /// would say is about a call nobody wrote ("`pressed` takes 1
    /// argument, but 0 were given"), so one message takes its place, and
    /// says what `by` will call the function with.
    fn calls_back(&mut self, by: &str, function: &Argument, types: Vec<Ty>) {
        let with = match types.as_slice() {
            [] => "with nothing, so it takes no parameters: `function() { ... }`",
            [Ty::Event] => {
                "with nothing, or with what happened if it asks: `function() { ... }` or `function(event: Event) { ... }`"
            }
            [Ty::Int, Ty::Str] => {
                "with the status and the text of the answer, so it takes an int and a str: \
                 `function(status: int, body: str) { ... }`"
            }
            _ => "with the key that was pressed, so it takes one str: `function(key: str) { ... }`",
        };
        let found = self.errors.len();
        let callee = function.ty.clone().looked_at();
        let arguments = |types: Vec<Ty>| -> Vec<Argument> {
            types.into_iter().map(|ty| Argument { name: None, ty, span: function.span, written: None, place: 0 }).collect()
        };
        // `on` hands the event to a function that asks for it, and
        // nothing to one that does not.
        if matches!(types.as_slice(), [Ty::Event]) {
            self.call(&callee, Vec::new(), function.span);
            if self.errors.len() == found {
                return;
            }
            self.errors.truncate(found);
        }
        self.call(&callee, arguments(types), function.span);
        if self.errors.len() > found {
            self.errors.truncate(found);
            let message = match function.ty.clone().looked_at() {
                Ty::Function(_) | Ty::BoundMethod(_) => format!("`{by}` will call this function {with}"),
                other => format!("`{by}` needs a function to call, but this is {other}"),
            };
            self.error(message, function.span);
        }
    }

    /// The numbers written for the cases of an enum (`Ack = 6`), if it
    /// has them, after the rules of decision 35.1: a number for every
    /// case or for none, only on a case that carries nothing, each one
    /// an int, and no two the same.
    fn case_numbers(&mut self, decl: &ast::Enum, span: Span) -> Option<Vec<i64>> {
        let numbered: Vec<&ast::Variant> = decl.variants.iter().filter(|case| case.number.is_some()).collect();
        if numbered.is_empty() {
            return None;
        }
        let mut numbers = Vec::new();
        let mut sound = true;
        for case in &decl.variants {
            let Some((number, at)) = case.number else {
                self.error(
                    format!(
                        "`{}` has no number, and the other cases of `{}` have one. Give every case a number, or none",
                        case.name, decl.name
                    ),
                    span,
                );
                sound = false;
                continue;
            };
            if !case.fields.is_empty() {
                self.error(
                    format!(
                        "`{}` carries values, so it cannot also stand for a number. Only an enum whose cases carry \
                         nothing can have numbers",
                        case.name
                    ),
                    at,
                );
                sound = false;
                continue;
            }
            let Ok(number) = i64::try_from(number) else {
                self.error(format!("{number} is too big for an int, and the number of a case is an int"), at);
                sound = false;
                continue;
            };
            if let Some(index) = numbers.iter().position(|&taken| taken == number) {
                self.error(
                    format!(
                        "`{}` and `{}` are both {number}. Each case needs a number of its own, so that a number \
                         gives back one case",
                        decl.variants[index].name, case.name
                    ),
                    at,
                );
                sound = false;
                continue;
            }
            numbers.push(number);
        }
        sound.then_some(numbers)
    }

    /// `int(Ack)` or `u8(Ack)`: a case of an enum with numbers, as its
    /// number. `Reply(6)`: the case whose number that is, or `none`
    /// (decision 35.2). Each is noted, for `checked` to turn into a call
    /// of a function it writes, and its type is given here. `None` for
    /// any other call, which `call` checks.
    fn numbered(&mut self, callee: &Ty, arguments: &mut [Argument], span: Span) -> Option<Ty> {
        match callee {
            Ty::Enum(info) if info.numbers.is_some() => {
                if self.arity(&info.name, arguments, 1, 1, span) && arguments[0].name.is_none() {
                    let what = format!("the number given to `{}`", ast::shown_name(&info.name));
                    let given = arguments[0].ty.clone();
                    self.expect(&given, &Ty::Int, &what, arguments[0].span);
                }
                self.types.numbered.insert((span.line, span.col), ast::Numbered::Case(info.name.clone()));
                self.types.numbered_enums.insert((info.id.0, info.id.1));
                Some(Ty::Optional(Box::new(Ty::EnumValue(info.clone()))))
            }
            Ty::Enum(info) => Some(self.error(
                format!(
                    "`{0}` is an enum, so there is nothing to build: a value of it is one of its cases, written by \
                     its name. If the cases stand for numbers, write them: `Ack = 6`, and `{0}(6)` gives the case \
                     that is 6",
                    ast::shown_name(&info.name)
                ),
                span,
            )),
            Ty::Builtin(name) if *name == "int" || ints::width(name).is_some() => {
                let [argument] = arguments else { return None };
                let Ty::EnumValue(info) = &argument.ty else { return None };
                if argument.name.is_some() {
                    return None;
                }
                if info.numbers.is_none() {
                    return Some(self.error(
                        format!(
                            "the cases of `{}` stand for no number. To turn a case into one, write a number for \
                             each case: `Ack = 6`",
                            ast::shown_name(&info.name)
                        ),
                        argument.span,
                    ));
                }
                self.types.numbered.insert((span.line, span.col), ast::Numbered::Number(info.name.clone()));
                self.types.numbered_enums.insert((info.id.0, info.id.1));
                argument.ty = Ty::Int;
                argument.written = None;
                None
            }
            _ => None,
        }
    }

    fn call(&mut self, callee: &Ty, mut arguments: Vec<Argument>, span: Span) -> Ty {
        match callee {
            Ty::CFunction(library, at) => self.c_call(library, *at, &arguments, span),
            Ty::Function(function) => {
                let signature = self.signature(function);
                self.fixed_arguments(&function.decl, &function.env, &signature.params, &mut arguments);
                self.arguments(&signature.name, &signature.params, &arguments, span);
                for kind in &signature.raises {
                    self.can_raise(kind, span);
                }
                signature.returns.clone()
            }
            Ty::BoundMethod(function) => {
                let signature = self.signature(function);
                // `self` is already supplied by the object.
                let params = signature.params.get(1..).unwrap_or_default();
                self.fixed_arguments(&function.decl, &function.env, params, &mut arguments);
                self.arguments(&signature.name, params, &arguments, span);
                for kind in &signature.raises {
                    self.can_raise(kind, span);
                }
                signature.returns.clone()
            }
            Ty::Class(class) if class.interface => self.error(
                format!(
                    "`{0}` is an interface, so there is nothing to build. Build an object of a class that is a `{0}`",
                    class.name
                ),
                span,
            ),
            Ty::Class(class) => {
                let fields = class.fields.borrow().clone();
                // Whoever builds an object fills in its fields, and
                // another file cannot see the private ones.
                let kept = fields.iter().find(|field| class.private.contains(&field.name));
                if let (true, Some(field)) = (span.file() != class.file, kept) {
                    return self.error(
                        format!(
                            "`{}` has the private field `{}`, so only the file that declares the class can build one. \
                             Write a function in that file that builds one and returns it, and call that function here",
                            class.name, field.name
                        ),
                        span,
                    );
                }
                self.arguments(&class.name, &fields, &arguments, span);
                // The defaults of the fields are worked out here, where
                // the object is built. One that uses a variable declared
                // further down has nothing to use yet.
                let missing = not_yet(&class.above, &class.env);
                for (field, names) in &class.defaults {
                    if let Some(name) = names.iter().find(|name| missing.contains(name)) {
                        self.error(
                            format!(
                                "`{}` cannot be built here yet. The default of its field `{field}` uses `{name}`, \
                                 which is declared further down. Build it below the line that declares `{name}`",
                                class.name
                            ),
                            span,
                        );
                        break;
                    }
                }
                Ty::Instance(class.clone())
            }
            Ty::CaseConstructor(info, index) => {
                let case_name = info.cases.borrow()[*index].name.clone();
                let fields = info.cases.borrow()[*index].fields.clone();
                self.arguments(&case_name, &fields, &arguments, span);
                Ty::EnumValue(info.clone())
            }
            Ty::ErrorType(kind) => {
                if arguments.len() > 1 || arguments.iter().any(|a| a.name.is_some()) {
                    self.error(format!("`{kind}` takes one message"), span);
                }
                self.an_error(kind.clone())
            }
            Ty::Builtin(name) => {
                self.no_named(name, &arguments, span);
                self.builtin(name, &arguments, span)
            }
            Ty::BuiltinMethod(receiver, name) => {
                self.no_named(name, &arguments, span);
                self.method(receiver, name, &arguments, span)
            }
            Ty::Callable(callable) => {
                self.no_named("this function", &arguments, span);
                if arguments.len() != callable.params.len() {
                    return self.error(
                        format!(
                            "this function takes {}, but {} given",
                            counted(callable.params.len(), "argument"),
                            were(arguments.len())
                        ),
                        span,
                    );
                }
                for (argument, wanted) in arguments.iter().zip(&callable.params) {
                    let given = self.adapted(argument.written.clone(), argument.place, argument.ty.clone(), wanted, argument.span);
                    self.expect(&given, wanted, "an argument", argument.span);
                }
                // `convert(text)`, where `convert` is a `function(str) ->
                // int raises ValueError`, is checked as a call of a
                // function that says so (decision 56.1).
                if let Some(kind) = &callable.raises {
                    self.can_raise(kind, span);
                }
                callable.returns.clone()
            }
            ty if ty.is_loose() => {
                // Whatever is being called, it receives these untracked.
                for argument in &arguments {
                    self.escapes(&argument.ty);
                }
                ty.clone()
            }
            Ty::Generic(generic) => self.error(
                format!("`{}` is generic, so it is called by its own name and cannot be kept as a value yet", generic.name),
                span,
            ),
            ty => self.error(format!("{ty} cannot be called"), span),
        }
    }

    /// A list of a fixed length given where a plain list is wanted goes
    /// there if the function never changes how many items that list has
    /// (decision 36.4). Then the argument is read as the plain list it
    /// is to the function; otherwise the message says what in the
    /// function could change it.
    fn fixed_arguments(&mut self, decl: &Arc<ast::Function>, env: &Env, slots: &[Slot], arguments: &mut [Argument]) {
        let mut next = 0;
        for argument in arguments.iter_mut() {
            let index = match &argument.name {
                None => {
                    next += 1;
                    next - 1
                }
                Some(name) => match slots.iter().position(|slot| slot.name == *name) {
                    Some(index) => index,
                    None => continue,
                },
            };
            let Some(slot) = slots.get(index) else {
                continue;
            };
            let Some(length) = fixed_length(&argument.ty.clone().looked_at()) else {
                continue;
            };
            let plain = unfixed(argument.ty.clone());
            if !matches!(slot.ty.clone().looked_at(), Ty::List(_)) || !fits(&plain, &slot.ty) {
                continue;
            }
            let mut seen = HashSet::new();
            match length_changed(decl, env, &slot.name, &mut seen) {
                None => argument.ty = plain,
                Some(why) => {
                    self.error(
                        format!(
                            "this list always has {length} items, and `{}` may change how many its `{}` has: {why}. \
                             A function that only reads a list, or gives its items new values, can be given one of a \
                             fixed length",
                            decl.name, slot.name
                        ),
                        argument.span,
                    );
                    argument.ty = Ty::Invalid;
                }
            }
        }
    }

    fn no_named(&mut self, what: &str, arguments: &[Argument], span: Span) {
        if let Some(name) = arguments.iter().find_map(|a| a.name.as_ref()) {
            self.error(format!("`{what}` does not take a named argument `{name}`"), span);
        }
    }

    /// Check the arguments of a call against the slots they are for. This
    /// follows the interpreter's `bind`, with types added.
    fn arguments(&mut self, what: &str, slots: &[Slot], arguments: &[Argument], span: Span) {
        let positional = arguments.iter().filter(|a| a.name.is_none()).count();
        if positional > slots.len() {
            self.error(
                format!("`{what}` takes {}, but {} given", counted(slots.len(), "argument"), were(positional)),
                span,
            );
            return;
        }

        let mut filled = vec![false; slots.len()];
        let mut next = 0;
        for argument in arguments {
            let index = match &argument.name {
                None => {
                    next += 1;
                    next - 1
                }
                Some(name) => match slots.iter().position(|slot| slot.name == *name) {
                    Some(index) => index,
                    None => {
                        self.error(format!("`{what}` has no argument named `{name}`"), argument.span);
                        continue;
                    }
                },
            };
            if filled[index] {
                self.error(format!("`{what}` was given `{}` twice", slots[index].name), argument.span);
                continue;
            }
            filled[index] = true;
            // Where the type is not known, a resource may not go: no
            // engine should have to find out as the program runs whether
            // a value is one (59.1).
            if self.holds_resource(&argument.ty) && slots[index].ty.has_dynamic() {
                self.error(
                    format!(
                        "the type of `{}` of `{what}` is not written, so a resource cannot go to it. Write its type: \
                         `{}: {}`",
                        slots[index].name,
                        slots[index].name,
                        argument.ty.clone().looked_at()
                    ),
                    argument.span,
                );
                continue;
            }
            self.sees(&slots[index], &argument.ty, argument.span);
            let label = format!("`{}` of `{what}`", slots[index].name);
            let given =
                self.adapted(argument.written.clone(), argument.place, argument.ty.clone(), &slots[index].ty, argument.span);
            self.expect(&given, &slots[index].ty, &label, argument.span);
        }

        // One argument that gives several values was meant as several: rule
        // 3 has said so, and a missing argument would only repeat it.
        if arguments.iter().any(|argument| argument.place != 0 && self.not_one.contains(&argument.place)) {
            return;
        }
        for (slot, filled) in slots.iter().zip(filled) {
            if !filled && !slot.has_default {
                self.error(format!("`{what}` needs a value for `{}`", slot.name), span);
            }
        }
    }

    /// `c.library(...)` with the library and the declarations written
    /// out: the library, with what each of its functions takes and gives
    /// (decision 44.10). `None` when either is worked out only as the
    /// program runs; then its functions are known only then.
    fn c_library(&mut self, args: &[ast::Arg], span: Span) -> Option<Rc<crate::bridge_c::Library>> {
        let [library, declared] = args else { return None };
        if library.name.is_some() || declared.name.is_some() {
            return None;
        }
        fn texts(expr: &Expr, none_too: bool) -> Option<Vec<String>> {
            match &expr.kind {
                ExprKind::Str(text) => Some(vec![text.clone()]),
                ExprKind::None if none_too => Some(Vec::new()),
                ExprKind::List(items) => items
                    .iter()
                    .map(|item| match &item.kind {
                        ExprKind::Str(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            }
        }
        let words = texts(&library.value, true)?;
        let declared = texts(&declared.value, false)?.join("\n");
        if let Some(known) = self.types.c_libraries.iter().find(|known| known.span == span) {
            return Some(known.clone());
        }
        let number = self.types.c_libraries.len();
        match crate::bridge_c::library_of(words, &declared, number, span) {
            Ok(library) => {
                let library = Rc::new(library);
                self.types.c_libraries.push(library.clone());
                Some(library)
            }
            Err(why) => {
                self.error(why, args[1].value.span);
                None
            }
        }
    }

    /// `inline c { ... }`, `inline python { ... }`, `inline node { ... }`:
    /// what a block of another language gives (decision 61).
    ///
    /// A block is asked for twice: by the word `inline`, and by the
    /// `import` of its language, which is what starts that language for
    /// every other use of it. A block of C is a library whose functions
    /// are the ones it writes out, each typed from its own first line,
    /// so a call of one is checked here as a call of a function of Zeno
    /// is. A block of Python or of JavaScript is a module, and what it
    /// has is known when the program runs, as for `python.module`.
    fn inline(&mut self, language: &str, text: &str, env: &Env, span: Span) -> Ty {
        if !crate::inline::LANGUAGES.iter().any(|(word, _)| *word == language) {
            return self.error(crate::inline::unknown(language), span);
        }
        if !matches!(env.get(language), Some(Variable { ty: Ty::Module(module), .. }) if module == language) {
            return self.error(
                format!(
                    "an `inline {language}` block needs `import {language}` at the top of the file. \
                     The import is what asks for the other language, and a file with no such line uses none"
                ),
                span,
            );
        }
        if language != "c" {
            if self.strict_at(span) {
                let name = crate::inline::LANGUAGES.iter().find(|(word, _)| *word == language).map_or(language, |(_, name)| name);
                return self.error(
                    format!(
                        "strict mode: what an `inline {language}` block gives is known only when the program runs, by {name}. \
                         `zeno build` and `zeno check --strict` do not take it, and `zeno run` does"
                    ),
                    span,
                );
            }
            return Ty::Dynamic;
        }
        if let Some(known) = self.types.c_libraries.iter().find(|known| known.span == span) {
            return Ty::CLibrary(known.clone());
        }
        let number = self.types.c_libraries.len();
        match crate::bridge_c::library_of_block(crate::inline::c_block(text, span), number, span) {
            Ok(library) => {
                let library = Rc::new(library);
                self.types.c_libraries.push(library.clone());
                Ty::CLibrary(library)
            }
            Err(why) => self.error(why, span),
        }
    }

    /// A call of a function of a library of C: each argument held to
    /// what the declaration says the function takes.
    fn c_call(&mut self, library: &crate::bridge_c::Library, at: usize, arguments: &[Argument], span: Span) -> Ty {
        use crate::bridge_c::Class;
        let function = &library.functions[at];
        if let Some(named) = arguments.iter().find(|argument| argument.name.is_some()) {
            return self.error(format!("a function of C takes no named arguments, and `{}` was given one", function.name), named.span);
        }
        if arguments.len() != function.takes.len() {
            return self.error(
                format!("`{}` takes {}, but {} given", function.name, counted(function.takes.len(), "argument"), were(arguments.len())),
                span,
            );
        }
        let numbers = |item: &Ty| matches!(item, Ty::Int | Ty::Sized(_) | Ty::Dynamic | Ty::Invalid);
        let mut checks = false;
        for (index, (argument, (of, class))) in arguments.iter().zip(&function.takes).enumerate() {
            let label = format!("the {} parameter of `{}` (`{}`)", crate::bridge_c::ordinal(index + 1), function.name, of.text);
            let wanted = match class {
                Class::Whole => Some(Ty::Int),
                Class::U64 => Some(Ty::U64),
                Class::Float => Some(Ty::Float),
                Class::Bool => Some(Ty::Bool),
                _ => None,
            };
            if let Some(wanted) = wanted {
                checks |= matches!(class, Class::Whole);
                let given = self.adapted(argument.written.clone(), argument.place, argument.ty.clone(), &wanted, argument.span);
                self.expect(&given, &wanted, &label, argument.span);
                continue;
            }
            let given = argument.ty.clone().looked_at();
            let given = match given {
                Ty::Optional(inner) => *inner,
                other => other,
            };
            if given.is_loose() || matches!(given, Ty::None) {
                continue;
            }
            let takes = match class {
                Class::Text { .. } => match &given {
                    Ty::Str => true,
                    Ty::List(item) | Ty::Fixed(item, _) => {
                        checks = true;
                        numbers(item)
                    }
                    _ => false,
                },
                Class::Pointer { to, item, .. } => match (&given, item.as_deref()) {
                    (Ty::CPointer(from), _) => &**from == to || &**from == "void" || to == "void",
                    // Bytes, for a pointer to anything that is not a
                    // number of a known size larger than one.
                    (Ty::Str, Some(Class::Whole)) => !crate::bridge_c::wider_than_a_byte(to),
                    (Ty::Str, None) => to == "void",
                    (Ty::List(list) | Ty::Fixed(list, _), item) => {
                        checks = true;
                        match item {
                            Some(Class::Whole) => numbers(list),
                            Some(Class::U64) => matches!(**list, Ty::U64) || numbers(list),
                            Some(Class::Float) => matches!(**list, Ty::Float | Ty::F32 | Ty::Dynamic | Ty::Invalid),
                            Some(Class::Bool) => matches!(**list, Ty::Bool | Ty::Dynamic | Ty::Invalid),
                            // `void *` takes bytes; a pointer to a
                            // pointer takes a list for C to fill.
                            None if to == "void" => numbers(list),
                            None => to.ends_with('*'),
                            Some(_) => false,
                        }
                    }
                    _ => false,
                },
                _ => false,
            };
            if !takes {
                let what = match class {
                    Class::Text { .. } => "takes a str, a list of bytes or none".to_string(),
                    Class::Pointer { to, item, .. } => {
                        let list = match item.as_deref() {
                            Some(Class::Float) => ", a list of floats".to_string(),
                            Some(Class::Bool) => ", a list of bools".to_string(),
                            Some(_) => ", a list of whole numbers".to_string(),
                            None => String::new(),
                        };
                        format!("takes a pointer to {to} that C gave{list} or none")
                    }
                    _ => "takes something else".to_string(),
                };
                self.error(format!("{label} {what}, and this is {}", argument.ty), argument.span);
            }
        }
        // A number that does not fit is an OverflowError, as `u8(300)`
        // is: a check the program does not have to declare.
        let _ = checks;
        c_given(&function.gives.1)
    }

    /// Check the number of arguments given to a built-in.
    fn arity(&mut self, name: &str, arguments: &[Argument], min: usize, max: usize, span: Span) -> bool {
        if (min..=max).contains(&arguments.len()) {
            return true;
        }
        let wanted = if min == max { counted(min, "argument") } else { format!("{min} to {max} arguments") };
        self.error(
            format!("`{name}` takes {wanted}, but {} given", were(arguments.len())),
            span,
        );
        false
    }

    /// The result of calling a built-in function.
    fn builtin(&mut self, name: &str, arguments: &[Argument], span: Span) -> Ty {
        // The built-ins work on plain ints: `abs` of an i8 is an int.
        let first = arguments.first().map(|a| a.ty.widened()).unwrap_or(Ty::Invalid);
        let wrong = |checker: &mut Checker, wanted: &str| {
            checker.error(format!("`{name}` needs {wanted}, but this is {first}"), span)
        };

        match name {
            "print" => Ty::None,
            "str" => {
                self.arity(name, arguments, 1, 1, span);
                Ty::Str
            }
            // `allocate(u8, 16)`: memory for sixteen bytes, all zero, and
            // a pointer to the first. Making it harms nothing. Using it
            // is what needs `unsafe`.
            "allocate" => {
                if !self.arity(name, arguments, 2, 2, span) {
                    return Ty::Invalid;
                }
                self.expect(&arguments[1].ty, &Ty::Int, "the number of items", arguments[1].span);
                match &arguments[0].ty {
                    Ty::Builtin(item) if Pointee::named(item).is_some() => {
                        Pointee::named(item).map_or(Ty::Invalid, Ty::Pointer)
                    }
                    ty if ty.is_loose() => Ty::Invalid,
                    _ => self.error(
                        "`allocate` needs the type of one item first, and then how many: `allocate(u8, 16)`. \
                         The type is int, float, or one of u8, u16, u32, i8, i16 and i32",
                        arguments[0].span,
                    ),
                }
            }
            "free" => {
                if self.arity(name, arguments, 1, 1, span) && !matches!(first, Ty::Pointer(_)) && !first.is_loose() {
                    wrong(self, "a pointer that `allocate` gave");
                }
                self.needs_unsafe("`free`", span);
                Ty::None
            }
            // `register(u32, 0x4002_0014)`: the register at that address.
            "register" => {
                if !self.arity(name, arguments, 2, 2, span) {
                    return Ty::Invalid;
                }
                self.expect(&arguments[1].ty, &Ty::Int, "the address of a register", arguments[1].span);
                match &arguments[0].ty {
                    Ty::Builtin(size) if ints::width(size).is_some_and(|width| !width.signed) => {
                        ints::width(size).map_or(Ty::Invalid, Ty::Register)
                    }
                    ty if ty.is_loose() => Ty::Invalid,
                    _ => self.error(
                        "`register` needs the size of the register first, and then its address: \
                         `register(u32, 0x4002_0014)`. The size is u8, u16 or u32",
                        arguments[0].span,
                    ),
                }
            }
            "len" => {
                if self.arity(name, arguments, 1, 1, span) && !matches!(first, Ty::Str | Ty::List(_) | Ty::Dict(..)) && !first.is_loose() {
                    if matches!(first, Ty::Pointer(_)) {
                        self.error(
                            "a pointer does not know how many items it points at, so `len` cannot tell. \
                             Keep the number you gave `allocate` next to the pointer",
                            span,
                        );
                    } else {
                        wrong(self, "a str, a list or a dict");
                    }
                }
                Ty::Int
            }
            "range" => {
                self.arity(name, arguments, 1, 3, span);
                for argument in arguments {
                    self.expect(&argument.ty, &Ty::Int, "an argument of `range`", argument.span);
                }
                Ty::list(Ty::Int)
            }
            "int" | "float" => {
                if self.arity(name, arguments, 1, 1, span) {
                    match first {
                        Ty::Int | Ty::Float | Ty::F32 => {}
                        // Half the u64s are past the largest int.
                        Ty::U64 if name == "int" => self.can_raise("OverflowError", span),
                        Ty::U64 => {}
                        // Text from outside may not be a number at all.
                        Ty::Str | Ty::Dynamic => self.can_raise("ValueError", span),
                        Ty::Invalid => {}
                        _ => {
                            wrong(self, "a number or a str");
                        }
                    }
                }
                if name == "int" { Ty::Int } else { Ty::Float }
            }
            // The `files` module. Every argument is text, and all but
            // `exists` can fail in a way no program can rule out: the
            // file may be gone by the time it is opened. So they raise
            // FileError as a value, in every kind of build.
            "files.read" | "files.write" | "files.append" | "files.exists" | "files.remove" => {
                let short = &name["files.".len()..];
                let count = if matches!(short, "write" | "append") { 2 } else { 1 };
                self.arity(short, arguments, count, count, span);
                for argument in arguments {
                    let what = format!("an argument of `files.{short}`");
                    self.expect(&argument.ty, &Ty::Str, &what, argument.span);
                }
                if short != "exists" {
                    self.can_raise("FileError", span);
                }
                match short {
                    "read" => Ty::Str,
                    "exists" => Ty::Bool,
                    _ => Ty::None,
                }
            }
            // `files.list(folder)`: the names in it, sorted, as a new list
            // of text. `files.make_folder(path)`: the folder and those
            // above it (decisions 59.20 and 59.21).
            "files.list" | "files.make_folder" => {
                let short = &name["files.".len()..];
                if self.arity(short, arguments, 1, 1, span) {
                    let what = if short == "list" { "the folder given to `files.list`" } else { "the path given to `files.make_folder`" };
                    self.expect(&first, &Ty::Str, what, arguments[0].span);
                }
                self.can_raise("FileError", span);
                if short == "list" { Ty::List(Box::new(Ty::Str)) } else { Ty::None }
            }
            // `files.read_bytes(path)` and `files.write_bytes(path, bytes)`:
            // a file as the bytes it holds, one `u8` an item (36.1).
            "files.read_bytes" | "files.write_bytes" => {
                let short = &name["files.".len()..];
                let bytes = Ty::List(Box::new(Ty::Sized(ints::WIDTHS[0])));
                let count = if short == "write_bytes" { 2 } else { 1 };
                if self.arity(short, arguments, count, count, span) {
                    self.expect(&first, &Ty::Str, &format!("the path given to `files.{short}`"), arguments[0].span);
                    if count == 2 {
                        let given = &arguments[1];
                        let ty = self.adapted(given.written.clone(), given.place, given.ty.clone(), &bytes, given.span);
                        self.expect(&ty, &bytes, "the bytes given to `files.write_bytes`", given.span);
                    }
                }
                self.can_raise("FileError", span);
                if short == "read_bytes" { bytes } else { Ty::None }
            }
            // The open file under a `Reader` or a `Writer` (decision
            // 59.6): `handle_open(path, how)` gives its number (`how` is 0
            // to read, 1 to write, 2 to add to the end); the rest take
            // that number and the path, which their messages name.
            "files.handle_open" => {
                if self.arity("handle_open", arguments, 2, 2, span) {
                    self.expect(&first, &Ty::Str, "the path given to `files.handle_open`", arguments[0].span);
                    self.expect(&arguments[1].ty, &Ty::Int, "how `files.handle_open` opens", arguments[1].span);
                }
                self.can_raise("FileError", span);
                Ty::Int
            }
            "files.handle_line" | "files.handle_bytes" | "files.handle_write" | "files.handle_write_bytes"
            | "files.handle_close" => {
                let short = &name["files.".len()..];
                let bytes = Ty::List(Box::new(Ty::Sized(ints::WIDTHS[0])));
                let count = if short == "handle_close" { 1 } else if short == "handle_line" { 2 } else { 3 };
                if self.arity(short, arguments, count, count, span) {
                    self.expect(&first, &Ty::Int, &format!("the file given to `files.{short}`"), arguments[0].span);
                    if count > 1 {
                        self.expect(&arguments[1].ty, &Ty::Str, &format!("the path given to `files.{short}`"), arguments[1].span);
                    }
                    if count > 2 {
                        let given = &arguments[2];
                        let wanted = match short {
                            "handle_bytes" => Ty::Int,
                            "handle_write" => Ty::Str,
                            _ => bytes.clone(),
                        };
                        let ty = self.adapted(given.written.clone(), given.place, given.ty.clone(), &wanted, given.span);
                        self.expect(&ty, &wanted, &format!("what is given to `files.{short}`"), given.span);
                    }
                }
                if short != "handle_close" {
                    self.can_raise("FileError", span);
                }
                match short {
                    "handle_line" => Ty::Optional(Box::new(Ty::Str)),
                    "handle_bytes" => bytes,
                    _ => Ty::None,
                }
            }
            // `input()`, `input("Name: ")`: a line from the keyboard, or
            // `none` when there are no more. The answer has to be tested
            // before it is used as text.
            "input" => {
                if self.arity(name, arguments, 0, 1, span) && !arguments.is_empty() {
                    self.expect(&first, &Ty::Str, "the question `input` asks", arguments[0].span);
                }
                Ty::Optional(Box::new(Ty::Str))
            }
            // `json.parse(text)`: what it gives is known when the program
            // runs, which is what a type left out means (18.1).
            "json.parse" => {
                if self.arity("parse", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the text given to `json.parse`", arguments[0].span);
                }
                Ty::Dynamic
            }
            "json.write" => {
                self.arity("write", arguments, 1, 1, span);
                Ty::Str
            }
            // `python.module("yt_dlp")`: a module of Python. What it has
            // is known when the program runs, as for `json.parse` (18.1).
            // Python may not be installed, or may not have the module.
            "python.module" => {
                if self.arity("module", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the name given to `python.module`", arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::Dynamic
            }
            "python.copy" => {
                self.arity("copy", arguments, 1, 1, span);
                Ty::Dynamic
            }
            // `node.module("fs")`, `node.global("JSON")`: the same for
            // JavaScript. Node may not be installed, or may not have
            // the module.
            // `java.type("java.util.ArrayList")` is the same for Java,
            // and `java.jar("lib")` says where its classes are found.
            // `csharp.assembly("lib")` says where the types of a
            // library of .NET are found.
            "csharp.assembly" => {
                if self.arity("assembly", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the .dll or folder given to `csharp.assembly`", arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::None
            }
            // `c.library("m", ["double cbrt(double x);"])`: a library of
            // C, and the declarations of its functions. Either may be one
            // text or a list of them. The library may not be there, and
            // the declarations are read only when the program runs.
            "c.library" => {
                if self.arity("library", arguments, 2, 2, span) {
                    // The library may be `none`: the C library every
                    // program has.
                    for (at, what) in [(0, "the library given to `c.library`"), (1, "the declarations given to `c.library`")] {
                        let ty = &arguments[at].ty;
                        if !fits(ty, &Ty::Str) && !fits(ty, &Ty::list(Ty::Str)) && !(at == 0 && matches!(ty, Ty::None)) {
                            self.expect(ty, &Ty::Str, what, arguments[at].span);
                        }
                    }
                }
                self.can_raise("FileError", span);
                Ty::Dynamic
            }
            "java.jar" => {
                if self.arity("jar", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the jar or folder given to `java.jar`", arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::None
            }
            // `ruby.require("date")` loads a library of Ruby, which may
            // not be there. `ruby.constant("Date")` then names what it
            // brought, and `ruby.symbol("name")` is `:name`.
            "ruby.require" => {
                if self.arity("require", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the library given to `ruby.require`", arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::None
            }
            "ruby.symbol" => {
                if self.arity("symbol", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the name given to `ruby.symbol`", arguments[0].span);
                }
                Ty::Dynamic
            }
            // `php.require("helper.php")` reads a file of PHP, which may
            // not be there. `php.callable("strrev")` and
            // `php.constant("M_PI")` name what PHP has, and
            // `php.variable(value)` is a variable of PHP.
            "php.require" => {
                if self.arity("require", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the file given to `php.require`", arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::None
            }
            "php.variable" => {
                self.arity("variable", arguments, 0, 1, span);
                Ty::Dynamic
            }
            "node.module" | "node.global" | "java.type" | "ruby.constant" | "php.type" | "php.callable" | "php.constant" | "lua.require" | "lua.global" | "csharp.type" => {
                let short = name.split('.').nth(1).unwrap_or(name);
                if self.arity(short, arguments, 1, 1, span) {
                    let what = format!("the name given to `{name}`");
                    self.expect(&first, &Ty::Str, &what, arguments[0].span);
                }
                self.can_raise("FileError", span);
                Ty::Dynamic
            }
            // `node.new(Class, ...)`: the class, then what the object
            // is made from.
            "node.new" => {
                if arguments.is_empty() {
                    self.error("`new` takes the class to make an object of, and then what it is made from, but nothing was given", span);
                }
                Ty::Dynamic
            }
            // `program.arguments()`: the words the program was started
            // with, as a new list.
            "program.arguments" => {
                self.arity("arguments", arguments, 0, 0, span);
                Ty::list(Ty::Str)
            }
            // `program.run(name, words)`: start another program and give
            // the number it ended with. That there is such a program is
            // known only when it is started, so this raises FileError as
            // a value, as the `files` functions do.
            "program.run" => {
                if self.arity("run", arguments, 2, 2, span) {
                    self.expect(&first, &Ty::Str, "the name of the program `program.run` starts", arguments[0].span);
                    let words = Ty::list(Ty::Str);
                    self.expect(&arguments[1].ty, &words, "the words `program.run` gives the program", arguments[1].span);
                }
                self.can_raise("FileError", span);
                Ty::Int
            }
            // `program.exit(code)`: the program ends here, with this
            // number for what started it (34.1). A number written down
            // that no program can end with is a mistake found now.
            "program.exit" => {
                if self.arity("exit", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Int, "the number `program.exit` ends the program with", arguments[0].span);
                    if let Some(Written::Number(code, _)) = &arguments[0].written {
                        if !(0..=255).contains(code) {
                            self.error(
                                format!("a program ends with a number from 0 to 255, not {code}"),
                                arguments[0].span,
                            );
                        }
                    }
                }
                Ty::None
            }
            // `program.environment("HOME")`: what the environment says,
            // or `none` (34.2). A board has no environment.
            "program.environment" => {
                if self.arity("environment", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the name `program.environment` looks up", arguments[0].span);
                }
                if self.firmware {
                    self.error(
                        "a firmware build cannot use `program.environment`: a board has no environment to look in. \
                         Give the program what it needs as a `const`",
                        span,
                    );
                }
                Ty::Optional(Box::new(Ty::Str))
            }
            // `program.warn(text)`: one line on the error stream (34.3).
            "program.warn" => {
                if self.arity("warn", arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, "the text `program.warn` writes", arguments[0].span);
                }
                Ty::None
            }
            // `char(65)`: the character with that code, as a str.
            "char" => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Int, "the code of a character", arguments[0].span);
                }
                Ty::Str
            }
            // `f32(x)`: the nearest f32 to a number (decision 41.5).
            "f32" => {
                if !self.arity(name, arguments, 1, 1, span) {
                    return Ty::Invalid;
                }
                match &first {
                    Ty::Int | Ty::Float | Ty::F32 => {}
                    ty if ty.is_loose() => {}
                    _ => return wrong(self, "a number"),
                }
                if let Some(Written::Real(x, _)) = &arguments[0].written {
                    if let Err(message) = f32_holds(*x) {
                        return self.error(message, arguments[0].span);
                    }
                } else if matches!(first, Ty::Float) || first.is_loose() {
                    // A float too big for 32 bits.
                    self.can_raise("OverflowError", span);
                }
                Ty::F32
            }
            "abs" => {
                if !self.arity(name, arguments, 1, 1, span) {
                    return Ty::Invalid;
                }
                match first {
                    Ty::Int | Ty::Float | Ty::F32 | Ty::U64 | Ty::Quantity(_) => first,
                    ty if ty.is_loose() => ty,
                    _ => wrong(self, "a number"),
                }
            }
            "min" | "max" | "sum" => {
                // Either one list, or the numbers themselves.
                let types: Vec<Ty> = match arguments {
                    [Argument { ty: Ty::List(item), .. }] => vec![item.widened()],
                    _ => arguments.iter().map(|a| a.ty.widened()).collect(),
                };
                // Quantities, if they are all in one unit: the answer is
                // in that unit too (17.3).
                let units: Vec<&Rc<str>> = types.iter().filter_map(|ty| if let Ty::Quantity(unit) = ty { Some(unit) } else { None }).collect();
                if let Some(first) = units.first() {
                    // `sum` of an empty list of volts would have to be
                    // `0 V`, and a running list does not know its unit.
                    if name == "sum" {
                        return self.error(crate::builtins::SUM_OF_QUANTITIES, span);
                    }
                    let plain = types.iter().find(|ty| !matches!(ty, Ty::Quantity(_)) && !ty.is_loose());
                    if let Some(plain) = plain {
                        return self.error(
                            format!("`{name}` needs all plain numbers or all quantities, but one of these is {plain} and one is in {first}"),
                            span,
                        );
                    }
                    if let Some(other) = units.iter().find(|unit| *unit != first) {
                        return self.error(
                            format!(
                                "`{name}` needs quantities in one unit, but these are in {first} and in {other}. \
                                 Turn one into the other first, as in `x.{first}`"
                            ),
                            span,
                        );
                    }
                    return if types.iter().any(|ty| ty.is_loose()) { Ty::Dynamic } else { Ty::Quantity((*first).clone()) };
                }
                // f32s, perhaps with ints: the answer is an f32.
                if types.iter().any(|ty| matches!(ty, Ty::F32)) && types.iter().all(|ty| matches!(ty, Ty::F32 | Ty::Int)) {
                    return Ty::F32;
                }
                // u64s: the answer is a u64, and `sum` checks that it fits.
                if types.iter().any(|ty| matches!(ty, Ty::U64)) {
                    if let Some(other) = types.iter().find(|ty| !matches!(ty, Ty::U64) && !ty.is_loose()) {
                        return self.error(
                            format!("`{name}` of u64s and {other} would have to choose between them. Make them all u64s with `u64( )`"),
                            span,
                        );
                    }
                    if name == "sum" {
                        self.can_raise("OverflowError", span);
                    }
                    return Ty::U64;
                }
                let joined = self.join_all(&types).unwrap_or(Ty::Int);
                if !joined.is_number() && !joined.is_loose() {
                    return self.error(format!("`{name}` needs numbers, but this is {joined}"), span);
                }
                joined
            }
            "sqrt" | "floor" | "ceil" | "round" | "exp" | "log" | "log2" | "log10" => {
                self.math_widens(arguments);
                let first = if matches!(first, Ty::F32) { Ty::Float } else { first.clone() };
                if self.arity(name, arguments, 1, 1, span) && !first.is_number() && !first.is_loose() {
                    wrong(self, "a number");
                }
                if matches!(name, "floor" | "ceil" | "round") { Ty::Int } else { Ty::Float }
            }
            // An angle goes in: a plain number is radians, as in
            // mathematics, and `30 deg` or `0.5 rad` says which it is.
            "sin" | "cos" | "tan" => {
                self.math_widens(arguments);
                let first = if matches!(first, Ty::F32) { Ty::Float } else { first.clone() };
                let angle = matches!(&first, Ty::Quantity(unit) if units::convert(unit, "rad").is_some());
                if self.arity(name, arguments, 1, 1, span) && !first.is_number() && !angle && !first.is_loose() {
                    wrong(self, "a number or an angle, such as `30 deg`");
                }
                Ty::Float
            }
            // An angle comes out, and says that it is in radians.
            "asin" | "acos" | "atan" => {
                self.math_widens(arguments);
                let first = if matches!(first, Ty::F32) { Ty::Float } else { first.clone() };
                if self.arity(name, arguments, 1, 1, span) && !first.is_number() && !first.is_loose() {
                    wrong(self, "a number");
                }
                Ty::Quantity(Rc::from("rad"))
            }
            "atan2" => {
                self.math_widens(arguments);
                if self.arity(name, arguments, 2, 2, span) {
                    for argument in arguments {
                        let ty = argument.ty.widened();
                        let ty = if matches!(ty, Ty::F32) { Ty::Float } else { ty };
                        if !ty.is_number() && !ty.is_loose() {
                            self.error(format!("`atan2` needs two numbers, but this is {ty}"), argument.span);
                        }
                    }
                }
                Ty::Quantity(Rc::from("rad"))
            }
            "sleep" => {
                // A time, and not a voltage: `sleep(3 V)` is refused here
                // and not when the program reaches it.
                let time = matches!(&first, Ty::Quantity(unit) if units::convert(unit, "s").is_some());
                if self.arity(name, arguments, 1, 1, span) && !time && !first.is_loose() {
                    wrong(self, "a time with a unit, such as `500 ms`");
                }
                Ty::None
            }
            "now" => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Quantity(Rc::from("ms"))
            }
            // `u64(n)` checks that `n` is not below 0 (42.4); `u64.wrap(n)`,
            // which `checked` names `u64..of`, keeps its 64 bits (42.6).
            "u64" | "u64..of" => {
                let shown = if name == "u64" { "u64" } else { "u64.wrap" };
                if !self.arity(shown, arguments, 1, 1, span) {
                    return Ty::Invalid;
                }
                if !matches!(first, Ty::Int | Ty::U64) && !first.is_loose() {
                    let hint = if matches!(first, Ty::Float) { ". Make it one with `int( )` first" } else { "" };
                    return self.error(format!("`{shown}` needs a whole number, but this is {first}{hint}"), span);
                }
                if let Some(Written::Number(n, at)) = &arguments[0].written {
                    if name == "u64" {
                        if let Err(message) = u64_holds(*n) {
                            return self.error(message, arguments[0].span);
                        }
                    }
                    // `u64(18446744073709551615)`: past the largest int,
                    // so it is written as a u64 straight away.
                    if *n > i64::MAX as i128 {
                        self.types.u64s.insert((at.line, at.col));
                    }
                } else if name == "u64" && !matches!(first, Ty::U64) {
                    // A u8, a u16 or a u32 is never below 0.
                    let unsigned = matches!(arguments[0].ty.clone().looked_at(), Ty::Sized(width) if !width.signed);
                    if !unsigned {
                        self.can_raise("OverflowError", span);
                    }
                }
                Ty::U64
            }
            "u64..add" | "u64..sub" | "u64..mul" | "u64..shl" => {
                self.arity("u64.wrap", arguments, 2, 2, span);
                Ty::U64
            }
            // `u8(n)` checks that `n` fits when the program runs, and
            // `u8.wrap(n)` keeps its low bits. Either way the answer is a u8.
            _ if ints::width(name).or(ints::wrapping(name)).is_some() => {
                let width = ints::width(name).or(ints::wrapping(name)).unwrap_or(ints::WIDTHS[0]);
                if !self.arity(name, arguments, 1, 1, span) {
                    return Ty::Invalid;
                }
                if matches!(first, Ty::U64) {
                    // 42.10: not yet straight from a u64.
                    return self.error(format!("`{name}` takes an int, and this is a u64. Make it an int first, as in `{name}(int(x))`"), span);
                }
                if !matches!(first, Ty::Int) && !first.is_loose() {
                    let hint = if matches!(first, Ty::Float) { ". Make it one with `int( )` first" } else { "" };
                    return self.error(format!("`{name}` needs a whole number, but this is {first}{hint}"), span);
                }
                // A number written out is checked now. Only `u8( )`
                // refuses one: `u8.wrap(300)` is asking for the 44.
                if let (Some(Written::Number(n, _)), true) = (&arguments[0].written, ints::width(name).is_some()) {
                    if !i64::try_from(*n).is_ok_and(|n| width.holds(n)) {
                        return self.error(width.does_not_fit(n), arguments[0].span);
                    }
                }
                Ty::Sized(width)
            }
            "pin" => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Int, "the number of a pin", arguments[0].span);
                }
                Ty::Pin
            }
            // `ticker(10 ms)`: a beat with that long from one tick to
            // the next (49.1).
            "ticker" => {
                if self.arity(name, arguments, 1, 1, span) {
                    let time = matches!(&first, Ty::Quantity(unit) if units::convert(unit, "s").is_some());
                    if !time && !first.is_loose() {
                        self.error(
                            format!("the period of a ticker has a unit, as in `ticker(10 ms)`, and this is {first}"),
                            arguments[0].span,
                        );
                    }
                }
                Ty::Ticker
            }
            // `pwm_pin(9, 1 kHz)`: a pin switched on and off that many
            // times a second (48.1).
            "pwm_pin" => {
                if self.arity(name, arguments, 2, 2, span) {
                    self.expect(&first, &Ty::Int, "the number of a pin", arguments[0].span);
                    let rate = &arguments[1];
                    let frequency = matches!(&rate.ty, Ty::Quantity(unit) if units::convert(unit, "Hz").is_some());
                    if !frequency && !rate.ty.is_loose() {
                        self.error(
                            format!(
                                "the frequency of a PWM pin has a unit, as in `pwm_pin(9, 1 kHz)`, and this is {}",
                                rate.ty
                            ),
                            rate.span,
                        );
                    }
                }
                Ty::PwmPin
            }
            // `analog_pin(0)`: a pin whose voltage the program reads (47.1).
            "analog_pin" => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Int, "the number of a pin", arguments[0].span);
                }
                Ty::AnalogPin
            }
            // `serial(1, 115200 baud)`: a serial port of the board, at
            // that speed (46.1, 46.2).
            "serial" => {
                if self.arity(name, arguments, 2, 2, span) {
                    self.expect(&first, &Ty::Int, "the number of a serial port", arguments[0].span);
                    let speed = &arguments[1];
                    if !fits(&speed.ty, &Ty::Quantity(Rc::from("baud"))) {
                        self.error(
                            format!(
                                "the speed of a serial port is in baud, as in `serial(1, 115200 baud)`, and this is {}",
                                speed.ty
                            ),
                            speed.span,
                        );
                    }
                }
                Ty::Serial
            }
            // `input_pin(2)`, `input_pin(2, pull_up)`: a pin the program
            // reads, and what holds it when nothing drives it (45.3).
            "input_pin" => {
                if self.arity(name, arguments, 1, 2, span) {
                    self.expect(&first, &Ty::Int, "the number of a pin", arguments[0].span);
                    if let Some(pull) = arguments.get(1).filter(|pull| !fits(&pull.ty, &Ty::Pull)) {
                        self.error(
                            format!(
                                "what holds an input pin is `pull_up` or `pull_down`, from the `board` module, \
                                 and this is {}. With nothing there, nothing pulls the pin",
                                pull.ty
                            ),
                            pull.span,
                        );
                    }
                }
                Ty::InputPin
            }
            "web.element" | "web.elements" => {
                let short = &name["web.".len()..];
                if self.arity(short, arguments, 1, 1, span) {
                    self.expect(&first, &Ty::Str, &format!("the selector given to `web.{short}`"), arguments[0].span);
                }
                if short == "element" { Ty::Element } else { Ty::list(Ty::Element) }
            }
            "web.create" => {
                self.arity("create", arguments, 1, 2, span);
                for argument in arguments {
                    self.expect(&argument.ty, &Ty::Str, "an argument of `web.create`", argument.span);
                }
                Ty::Element
            }
            "web.after" | "web.every" => {
                let short = &name["web.".len()..];
                let time = matches!(&first, Ty::Quantity(unit) if units::convert(unit, "ms").is_some());
                if self.arity(short, arguments, 2, 2, span) {
                    if !time && !first.is_loose() {
                        self.error(
                            format!("`{short}` needs a time with a unit, such as `500 ms`, but this is {first}"),
                            arguments[0].span,
                        );
                    }
                    self.calls_back(short, &arguments[1], Vec::new());
                }
                Ty::Timer
            }
            "web.save" | "web.load" | "web.forget" => {
                let short = &name["web.".len()..];
                let count = if short == "save" { 2 } else { 1 };
                self.arity(short, arguments, count, count, span);
                for argument in arguments {
                    self.expect(&argument.ty, &Ty::Str, &format!("an argument of `web.{short}`"), argument.span);
                }
                // Nothing may have been saved under the name.
                if short == "load" { Ty::Optional(Box::new(Ty::Str)) } else { Ty::None }
            }
            "web.fetch" | "web.post" => {
                let short = &name["web.".len()..];
                let texts = if short == "fetch" { 1 } else { 2 };
                if self.arity(short, arguments, texts + 1, texts + 1, span) {
                    for argument in &arguments[..texts] {
                        self.expect(&argument.ty, &Ty::Str, &format!("an argument of `web.{short}`"), argument.span);
                    }
                    self.calls_back(short, &arguments[texts], vec![Ty::Int, Ty::Str]);
                }
                Ty::None
            }
            "web.on_key" => {
                if self.arity("on_key", arguments, 1, 1, span) {
                    self.calls_back("on_key", &arguments[0], vec![Ty::Str]);
                }
                Ty::None
            }
            _ => Ty::Dynamic,
        }
    }

    /// The result of calling a built-in method on a str, a list or a dict.
    fn method(&mut self, receiver: &Ty, name: &str, arguments: &[Argument], span: Span) -> Ty {
        let strs = |checker: &mut Checker| {
            for argument in arguments {
                checker.expect(&argument.ty, &Ty::Str, &format!("an argument of `{name}`"), argument.span);
            }
        };
        // `slice(from, to)` and `pop(at)` take positions.
        let positions = |checker: &mut Checker| {
            for argument in arguments {
                checker.expect(&argument.ty, &Ty::Int, &format!("a position given to `{name}`"), argument.span);
            }
        };
        // Call a function argument with the given argument types.
        let apply = |checker: &mut Checker, function: &Argument, types: Vec<Ty>| {
            let arguments = types
                .into_iter()
                .map(|ty| Argument { name: None, ty, span: function.span, written: None, place: 0 })
                .collect();
            // Several values are not a value (decision 57): `map` would
            // have nothing to put in its list.
            match checker.call(&function.ty.clone().looked_at(), arguments, function.span) {
                Ty::Several(values) => checker.error(
                    format!(
                        "the function given to `{name}` gives {} values, and `{name}` needs one value from each call. \
                         To keep them together, declare a class and return one object",
                        values.len()
                    ),
                    function.span,
                ),
                result => result,
            }
        };
        // What `map` and `reduce` hand the function for each item. An
        // item that has one owner stays in its list, and is lent.
        let each = |item: &Ty| if item.is_owned() { lent(item) } else { item.clone() };

        match (receiver, name) {
            (Ty::Pin, _) => {
                self.arity(name, arguments, 0, 0, span);
                if name == "is_on" { Ty::Bool } else { Ty::None }
            }
            // `beat.wait()` waits for the next tick and `beat.ticks()`
            // does not; each gives how many ticks have come since the
            // program last took them. `beat.restart()` starts the period
            // again from now (49.2, 49.3).
            (Ty::Ticker, _) => {
                self.arity(name, arguments, 0, 0, span);
                if name == "restart" { Ty::None } else { Ty::Int }
            }
            // `lamp.duty(0.25)`: on for that share of the time, a float
            // from 0.0 to 1.0 (48.2).
            (Ty::PwmPin, "duty") => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&arguments[0].ty, &Ty::Float, "the share of the time given to `duty`", arguments[0].span);
                }
                Ty::None
            }
            // `servo.pulse(1500 us)`: on for that long in each period (48.3).
            (Ty::PwmPin, _) => {
                if self.arity(name, arguments, 1, 1, span) {
                    let given = &arguments[0];
                    let time = matches!(&given.ty, Ty::Quantity(unit) if units::convert(unit, "s").is_some());
                    if !time && !given.ty.is_loose() {
                        self.error(
                            format!("`pulse` needs a time with a unit, such as `1500 us`, and this is {}", given.ty),
                            given.span,
                        );
                    }
                }
                Ty::None
            }
            // `sensor.voltage()`: what the pin is at now, in volts.
            // `sensor.count()`: the same as the converter counts it.
            (Ty::AnalogPin, _) => {
                self.arity(name, arguments, 0, 0, span);
                if name == "voltage" { Ty::Quantity(Rc::from("V")) } else { Ty::Int }
            }
            // `button.interrupt(falling)`: the board counts those edges
            // of the pin from now on (50.1).
            (Ty::InputPin, "interrupt") => {
                if self.arity(name, arguments, 1, 1, span) {
                    let edge = &arguments[0];
                    if !fits(&edge.ty, &Ty::Edge) {
                        self.error(
                            format!(
                                "an interrupt counts `rising`, `falling` or `either` edges, from the `board` module, \
                                 and this is {}",
                                edge.ty
                            ),
                            edge.span,
                        );
                    }
                }
                Ty::Interrupt
            }
            // `press.wait()` waits for an edge and `press.edges()` does
            // not; each gives how many have come since the program last
            // took them (50.2).
            (Ty::Interrupt, _) => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Int
            }
            // `button.is_low()`: the level the pin is at now.
            (Ty::InputPin, _) => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Bool
            }
            // `port.write("hello\n")`, `port.write_bytes(frame)`: sent as
            // they are. `port.read_byte()`: the next byte, waiting for
            // it, or `none` when no more can come. `port.has_byte()`:
            // has one come, without waiting (46.3, 46.4).
            (Ty::Serial, "has_byte") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Bool
            }
            (Ty::Serial, "write") => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&arguments[0].ty, &Ty::Str, "the text given to `write`", arguments[0].span);
                }
                Ty::None
            }
            (Ty::Serial, "write_bytes") => {
                if self.arity(name, arguments, 1, 1, span) {
                    let bytes = Ty::List(Box::new(Ty::Sized(ints::WIDTHS[0])));
                    let given = &arguments[0];
                    let ty = self.adapted(given.written.clone(), given.place, given.ty.clone(), &bytes, given.span);
                    // The port only looks at the list, so a list with one
                    // owner is lent to it for the call.
                    self.expect(&ty.looked_at(), &bytes, "the bytes given to `write_bytes`", given.span);
                }
                Ty::None
            }
            (Ty::Serial, _) => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Optional(Box::new(Ty::Sized(ints::WIDTHS[0])))
            }
            // `button.on("click", pressed)`: the page calls `pressed`
            // with nothing.
            (Ty::Timer, "stop") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::None
            }
            (Ty::Element, "on") => {
                if self.arity(name, arguments, 2, 2, span) {
                    self.expect(&arguments[0].ty, &Ty::Str, "the name of the event", arguments[0].span);
                    self.calls_back("on", &arguments[1], vec![Ty::Event]);
                }
                Ty::None
            }
            (Ty::Element, "fire") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                Ty::None
            }
            (Ty::Element, "get_style") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                Ty::Optional(Box::new(Ty::Str))
            }
            (Ty::Element, "set_style") => {
                self.arity(name, arguments, 2, 2, span);
                strs(self);
                Ty::None
            }
            // Places and sizes, then the text for `draw_text`, then the
            // colour. `erase` takes nothing.
            (Ty::Element, "draw_rect" | "draw_circle" | "draw_line" | "draw_text" | "erase") => {
                let (numbers, words) = match name {
                    "draw_rect" | "draw_line" => (4, 1),
                    "draw_circle" => (3, 1),
                    "draw_text" => (2, 2),
                    _ => (0, 0),
                };
                if self.arity(name, arguments, numbers + words, numbers + words, span) {
                    for argument in &arguments[..numbers] {
                        if !matches!(argument.ty, Ty::Int | Ty::Float) && !argument.ty.is_loose() {
                            self.error(
                                format!("a place or a size given to `{name}` is a number, but this is {}", argument.ty),
                                argument.span,
                            );
                        }
                    }
                    for argument in &arguments[numbers..] {
                        let what = if words == 2 && argument.span == arguments[numbers].span { "the text" } else { "the colour" };
                        self.expect(&argument.ty, &Ty::Str, &format!("{what} given to `{name}`"), argument.span);
                    }
                }
                Ty::None
            }
            (Ty::Element, "append") => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&arguments[0].ty, &Ty::Element, "what is appended to an element", arguments[0].span);
                }
                Ty::None
            }
            (Ty::Element, "remove" | "clear" | "show" | "hide") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::None
            }
            // An attribute the element does not have is `none`.
            (Ty::Element, "get") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                Ty::Optional(Box::new(Ty::Str))
            }
            (Ty::Element, "set") => {
                self.arity(name, arguments, 2, 2, span);
                strs(self);
                Ty::None
            }
            (Ty::Element, "add_class" | "remove_class" | "has_class") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                if name == "has_class" { Ty::Bool } else { Ty::None }
            }
            (Ty::Register(width), "read") => {
                self.arity(name, arguments, 0, 0, span);
                self.needs_unsafe("reading a register", span);
                Ty::Sized(*width)
            }
            (Ty::Register(width), _) => {
                self.needs_unsafe("writing a register", span);
                if self.arity(name, arguments, 1, 1, span) {
                    let value = &arguments[0];
                    let wanted = Ty::Sized(*width);
                    let given = self.adapted(value.written.clone(), value.place, value.ty.clone(), &wanted, value.span);
                    self.expect(&given, &wanted, "the value written to this register", value.span);
                }
                Ty::None
            }
            (Ty::Str, "upper" | "lower" | "strip") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Str
            }
            (Ty::Str, "split") => {
                self.arity(name, arguments, 0, 1, span);
                strs(self);
                Ty::list(Ty::Str)
            }
            (Ty::Str, "lines") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::list(Ty::Str)
            }
            (Ty::Str, "replace") => {
                self.arity(name, arguments, 2, 2, span);
                strs(self);
                Ty::Str
            }
            // Where the piece begins, or `none` when it is not there:
            // the answer has to be tested before it is used as a number.
            (Ty::Str, "find") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                Ty::Optional(Box::new(Ty::Int))
            }
            (Ty::Str, "slice") => {
                self.arity(name, arguments, 1, 2, span);
                positions(self);
                Ty::Str
            }
            (Ty::Str, "code") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::Int
            }
            (Ty::Str, _) => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                Ty::Bool
            }

            // `get` is the way to ask for a key that may not be there.
            // With one argument the answer may be `none`. With two, the
            // second is what to give instead.
            (Ty::Dict(key, value), "get") => {
                if !self.arity(name, arguments, 1, 2, span) {
                    return Ty::Invalid;
                }
                self.expect(&arguments[0].ty.widened(), key, "the key given to `get`", arguments[0].span);
                match arguments.get(1) {
                    // Who would own what it gives: the dict, or the caller?
                    Some(instead) if self.one_owner && value.is_owned() => self.error(
                        format!(
                            "a value of this dict is {}, which has one owner: the dict. `get` with a second \
                             value would give either a look at the dict's or the value given, and nobody could \
                             tell who frees it. Write `get(key)`, which gives a look or `none`, and test it \
                             with `is not none`",
                            an_owned(value)
                        ),
                        instead.span,
                    ),
                    Some(instead) => {
                        self.expect(
                            &instead.ty.widened(),
                            value,
                            "what `get` gives when the key is not there",
                            instead.span,
                        );
                        (**value).clone()
                    }
                    // A value that has one owner stays in the dict.
                    None if self.one_owner && value.is_owned() => Ty::Optional(Box::new(lent(value))),
                    None => Ty::Optional(value.clone()),
                }
            }
            (Ty::Dict(key, value), "pop") => {
                if self.arity(name, arguments, 1, 1, span) {
                    self.expect(&arguments[0].ty.widened(), key, "the key given to `pop`", arguments[0].span);
                }
                (**value).clone()
            }
            (Ty::Dict(key, _), "keys") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::list((**key).clone())
            }
            (Ty::Dict(_, value), "values") if self.one_owner && value.is_owned() => self.error(
                format!(
                    "a value of this dict is {}, which has one owner: the dict. A list of the values would be \
                     a second owner of each. Loop over the dict with `for key, value in` instead, which lends \
                     each value in turn",
                    an_owned(value)
                ),
                span,
            ),
            (Ty::Dict(_, value), "values") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::list((**value).clone())
            }
            (Ty::Dict(..), "clear") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::None
            }

            (Ty::List(item), "append") => {
                if self.arity(name, arguments, 1, 1, span) {
                    let added = &arguments[0];
                    let given = self.adapted(added.written.clone(), added.place, added.ty.clone(), item, added.span);
                    self.expect(&given, item, "an item added to this list", added.span);
                }
                Ty::None
            }
            (Ty::List(item), "pop") => {
                self.arity(name, arguments, 0, 1, span);
                positions(self);
                (**item).clone()
            }
            (Ty::List(item), "insert") => {
                if self.arity(name, arguments, 2, 2, span) {
                    let at = &arguments[0];
                    self.expect(&at.ty, &Ty::Int, "the position given to `insert`", at.span);
                    let added = &arguments[1];
                    let given = self.adapted(added.written.clone(), added.place, added.ty.clone(), item, added.span);
                    self.expect(&given, item, "an item put in this list", added.span);
                }
                Ty::None
            }
            (Ty::List(_), "reverse" | "clear") => {
                self.arity(name, arguments, 0, 0, span);
                Ty::None
            }
            // `sort` puts the smallest first, with `<`. So the items must
            // be something `<` has an answer for.
            // `people.sort(age_of)`: the function gives what each item
            // is ordered by, and that must be something `<` has an
            // answer for.
            (Ty::List(item), "sort") if arguments.len() == 1 => {
                // A function of two items, as other languages sort by.
                let takes = match arguments[0].ty.clone().looked_at() {
                    Ty::Function(function) => Some(function.decl.params.len()),
                    Ty::Callable(callable) => Some(callable.params.len()),
                    _ => None,
                };
                if takes == Some(2) {
                    return self.error(
                        "the function given to `sort` takes one item and returns what it is ordered by, \
                         a number or text. It is not asked which of two items comes first",
                        arguments[0].span,
                    );
                }
                let key = apply(self, &arguments[0], vec![each(item)]);
                if !matches!(key, Ty::Int | Ty::Float | Ty::F32 | Ty::U64 | Ty::Sized(_) | Ty::Str | Ty::Invalid) && !key.is_loose() {
                    return self.error(
                        format!(
                            "the function given to `sort` says what each item is ordered by, a number or text, \
                             and this one returns {key}"
                        ),
                        arguments[0].span,
                    );
                }
                Ty::None
            }
            (Ty::List(item), "sort") => {
                self.arity(name, arguments, 0, 1, span);
                if !matches!(**item, Ty::Int | Ty::Float | Ty::F32 | Ty::U64 | Ty::Sized(_) | Ty::Str) && !item.is_loose() {
                    return self.error(
                        format!(
                            "`sort` puts numbers or text in order, smallest first, and this is {receiver}. \
                             There is no one order for those items, so give `sort` a function that says \
                             what each is ordered by, as in `people.sort(age_of)`"
                        ),
                        span,
                    );
                }
                Ty::None
            }
            (Ty::List(item), "find") => {
                if self.arity(name, arguments, 1, 1, span) {
                    let wanted = &arguments[0];
                    let given = self.adapted(wanted.written.clone(), wanted.place, wanted.ty.clone(), item, wanted.span);
                    // It is compared and not kept, so a look will do.
                    let (given, item) = (given.looked_at(), (**item).clone().looked_at());
                    self.expect(&given, &item, "the item `find` looks for", wanted.span);
                }
                Ty::Optional(Box::new(Ty::Int))
            }
            (Ty::List(item), "slice") if item.is_owned() => self.error(
                "`slice` would put the items it takes in a second list, and each has one owner. \
                 Use a `for` loop that makes what the new list needs",
                span,
            ),
            (Ty::List(_), "slice") => {
                self.arity(name, arguments, 1, 2, span);
                positions(self);
                receiver.clone()
            }
            (Ty::List(item), "join") => {
                self.arity(name, arguments, 1, 1, span);
                strs(self);
                if !matches!(**item, Ty::Str) && !item.is_loose() {
                    return self.error(
                        format!(
                            "`join` puts text together, and this is {receiver}. \
                             Make each item text first, with `str( )`"
                        ),
                        span,
                    );
                }
                Ty::Str
            }
            (Ty::List(item), "map") => {
                if !self.arity(name, arguments, 1, 1, span) {
                    return Ty::Invalid;
                }
                Ty::list(apply(self, &arguments[0], vec![each(item)]))
            }
            (Ty::List(item), "filter") if item.is_owned() => self.error(
                "`filter` would put the items it keeps in a second list, and each has one owner. \
                 Use a `for` loop that makes what the new list needs",
                span,
            ),
            (Ty::List(item), "filter") => {
                if self.arity(name, arguments, 1, 1, span) {
                    let result = apply(self, &arguments[0], vec![(**item).clone()]);
                    self.expect(&result, &Ty::Bool, "what the function given to `filter` returns", arguments[0].span);
                }
                receiver.clone()
            }
            (Ty::List(item), "reduce") => {
                if !self.arity(name, arguments, 2, 2, span) {
                    return Ty::Invalid;
                }
                let start = arguments[1].ty.clone();
                let result = apply(self, &arguments[0], vec![start.clone(), each(item)]);
                self.join(&start, &result)
            }
            _ => Ty::Dynamic,
        }
    }

    // ------------------------------------------- Generic functions and classes
    //
    // `function first[T](items: list[T]) -> T` is not one function. It is
    // a pattern for as many as the program needs: one for lists of ints,
    // one for lists of strs. Each is a **copy**: the function's own
    // syntax tree, copied, with `T` standing for one type, under a name
    // such as `first[int]`. A copy is an ordinary function. It is checked
    // like one, and the compilers compile it like one, which is why none
    // of the stages after this one needed to learn what `T` is.
    //
    // The work here is in three steps:
    //
    // 1. `infer`: at a call, find what each type parameter must be, by
    //    laying the written parameter types over the types of the
    //    arguments (`bind`).
    // 2. `copy_function` / `copy_class`: make the copy for those types,
    //    or find the one made earlier.
    // 3. Carry on as if the copy had been called by name.
    //
    // A generic class works the same way, with its fields as the
    // parameters. `Stack[int]` written as a type asks for a copy too.

    /// Check the names in `[T, U]` after a function's or class's name.
    /// The capital letter keeps them apart from `int`, `str` and `list`.
    /// Inside the generic the name wins over anything else called the
    /// same, so `A` and `V` are type parameters there and not amperes
    /// and volts.
    fn type_params(&mut self, params: &[ast::TypeParam], span: Span) {
        let names = ast::param_names(params);
        for (index, name) in names.iter().enumerate() {
            if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
                self.error(
                    format!("a type parameter starts with a capital letter, as in `T` or `Item`, and this is `{name}`"),
                    span,
                );
            } else if names[..index].contains(name) {
                self.error(format!("the type parameter `{name}` is written twice"), span);
            }
        }
    }

    /// Note that `value` is going where a `wanted` is wanted, in case it
    /// is a call that needs to know: `let s: Stack[int] = Stack()`.
    fn expecting(&mut self, value: &Expr, wanted: &Ty) {
        let says_something = !matches!(wanted, Ty::Dynamic) && !has_param(wanted);
        if matches!(value.kind, ExprKind::Name(_)) && says_something {
            self.expected_case = Some((value as *const Expr as usize, wanted.clone()));
        }
        if matches!(value.kind, ExprKind::Call { .. }) && says_something {
            // `Invalid` is noted too: a mistake in the written type was
            // reported there, and the call should not complain as well.
            self.expected = Some((value as *const Expr as usize, wanted.clone()));
        }
        // `[Circle(1.0), Square(2.0)]` where a `list[Shape]` is wanted, or
        // `[1, 2]` where a `list[int or none]` is. The items do not say
        // that they are shapes, or that one may be `none` later, so the
        // list is told what it is a list of.
        if matches!(value.kind, ExprKind::List(_)) && says_something {
            let wanted_item = match wanted.clone().looked_at() {
                Ty::List(item) | Ty::Fixed(item, _) => Some(*item),
                Ty::Optional(inner) => match inner.looked_at() {
                    Ty::List(item) | Ty::Fixed(item, _) => Some(*item),
                    _ => None,
                },
                _ => None,
            };
            // `[0x7E, command, 0]` where a `list[u8]` is wanted: a number
            // written out is a u8 if it fits, and `command` must be one.
            if let Some(item) = wanted_item.filter(|item| asks_more(item) || matches!(item, Ty::Sized(_))) {
                self.expected_items = Some((value as *const Expr as usize, item));
            }
        }
        // The same for the values of a dict that is written out, whatever
        // they are: a number written where a `dict[str, u8]` goes is a u8,
        // and a function is held to the function type the place wants.
        if matches!(value.kind, ExprKind::Dict(_)) && says_something {
            let wanted_value = match wanted.clone().looked_at() {
                Ty::Dict(_, value) => Some(*value),
                Ty::Optional(inner) => match inner.looked_at() {
                    Ty::Dict(_, value) => Some(*value),
                    _ => None,
                },
                _ => None,
            };
            if let Some(value_type) = wanted_value.filter(|ty| !ty.is_loose() && !has_param(ty)) {
                self.expected_items = Some((value as *const Expr as usize, value_type));
            }
        }
    }

    /// The parameters of a generic function (or the fields of a generic
    /// class) and what it returns, with the type parameters left in.
    fn outline(&mut self, generic: &Rc<Generic>) -> Rc<Outline> {
        if let Some(outline) = generic.outline.borrow().clone() {
            return outline;
        }
        // While this is worked out, `T` means "the type parameter T".
        let env = Scope::child(&generic.env);
        for name in &generic.params {
            env.declare(name, Ty::Alias(Box::new(Ty::Param(Rc::from(name.as_str())))), false);
            self.type_params.extend(env.get(name).map(|variable| variable.id));
        }
        let span = generic.span;
        let mut slots = Vec::new();
        let returns = match &generic.template {
            Template::Function(decl) => {
                for param in &decl.params {
                    let ty = match &param.ty {
                        Some(ty) => self.resolve_for(ty, &env, span, true),
                        None => Ty::Invalid,
                    };
                    slots.push(Slot { name: param.name.clone(), ty, has_default: param.default.is_some(), id: None });
                }
                decl.return_type.as_ref().map(|ty| self.resolve_returns(ty, &env, span))
            }
            Template::Class(decl) => {
                for field in &decl.fields {
                    let ty = match &field.ty {
                        Some(ty) => self.resolve(ty, &env, span),
                        None => Ty::Invalid,
                    };
                    slots.push(Slot { name: field.name.clone(), ty, has_default: field.default.is_some(), id: None });
                }
                // Building one gives the class with every parameter in place.
                let params = generic.params.iter().map(|name| Ty::Param(Rc::from(name.as_str()))).collect();
                Some(Ty::Applied(generic.clone(), params))
            }
            // An enum is reached through its cases, each with values of
            // its own (see `case_outline`), and never called itself.
            Template::Enum(_) => None,
        };
        let outline = Rc::new(Outline { slots, returns });
        *generic.outline.borrow_mut() = Some(outline.clone());
        outline
    }

    /// A call of a generic function, or the building of an object of a
    /// generic class. `wanted` is the type of the place the result goes,
    /// if that is known.
    fn call_generic(
        &mut self,
        generic: &Rc<Generic>,
        mut arguments: Vec<Argument>,
        later: &[Later],
        call: &Expr,
        wanted: Option<Ty>,
        span: Span,
    ) -> Ty {
        // A value of a generic enum is one of its cases.
        if let Template::Enum(decl) = &generic.template {
            let case = decl.variants.iter().find(|case| !case.fields.is_empty()).unwrap_or(&decl.variants[0]);
            return self.error(
                format!(
                    "`{}` is an enum, and a value of it is one of its cases, as in `{}`",
                    generic.name,
                    match case.fields.is_empty() {
                        true => case.name.clone(),
                        false => format!("{}(...)", case.name),
                    }
                ),
                span,
            );
        }
        // An interface has no objects of its own, generic or not (58.6).
        if let Template::Class(decl) = &generic.template {
            if decl.interface {
                return self.error(
                    format!(
                        "`{0}` is an interface, so there is nothing to build. Build an object of a class that is a `{0}`",
                        generic.name
                    ),
                    span,
                );
            }
        }
        // `first(buffer)`, where `first` takes a `list[T]`: as for any
        // function, a list of a fixed length goes there if the function
        // never changes how long it is (decision 36.4).
        if let Template::Function(decl) = &generic.template {
            let slots: Vec<Slot> = decl
                .params
                .iter()
                .map(|param| Slot {
                    name: param.name.clone(),
                    ty: match &param.ty {
                        Some(ast::Type::Named { name, args }) if name == "list" && args.len() == 1 => Ty::list(Ty::Dynamic),
                        _ => Ty::Invalid,
                    },
                    has_default: false,
                    id: None,
                })
                .collect();
            self.fixed_arguments(decl, &generic.env, &slots, &mut arguments);
        }
        // The compilers find the copy by the call. `expr` has seen to it
        // that a generic is only ever called by its own name.
        let outline = self.outline(generic);
        let Some(types) = self.infer_from(generic, &outline, None, &arguments, later, wanted, span) else {
            return Ty::Invalid;
        };
        if !self.settle_later(generic, &outline, &types, &mut arguments, later) {
            return Ty::Invalid;
        }
        let copy = match &generic.template {
            Template::Function(_) => self.copy_function(generic, types, Some(span)).map(|copy| {
                self.types.copy_called.insert(call as *const Expr as usize, copy.decl.name.clone());
                Ty::Function(copy)
            }),
            Template::Class(_) => self.copy_class(generic, types, Some(span)).map(|copy| {
                self.types.copy_called.insert(call as *const Expr as usize, copy.name.clone());
                Ty::Class(copy)
            }),
            Template::Enum(_) => None,
        };
        match copy {
            // From here on it is an ordinary call of an ordinary function.
            Some(copy) => self.call(&copy, arguments, span),
            None => Ty::Invalid,
        }
    }

    /// The values one case of a generic enum carries, with the type
    /// parameters left in, as the outline of a function: `Just(value: T)`
    /// takes a `T` and gives a `Maybe[T]` (decision 58.1).
    fn case_outline(&mut self, generic: &Rc<Generic>, index: usize) -> Rc<Outline> {
        if let Some(outline) = self.case_outlines.get(&(generic.at, index)) {
            return outline.clone();
        }
        let Template::Enum(decl) = &generic.template else {
            return Rc::new(Outline { slots: Vec::new(), returns: None });
        };
        let env = Scope::child(&generic.env);
        for name in &generic.params {
            env.declare(name, Ty::Alias(Box::new(Ty::Param(Rc::from(name.as_str())))), false);
            self.type_params.extend(env.get(name).map(|variable| variable.id));
        }
        let span = generic.span;
        let slots = decl.variants[index]
            .fields
            .iter()
            .map(|field| Slot {
                name: field.name.clone(),
                ty: field.ty.as_ref().map_or(Ty::Invalid, |ty| self.resolve(ty, &env, span)),
                has_default: field.default.is_some(),
                id: None,
            })
            .collect();
        let params = generic.params.iter().map(|name| Ty::Param(Rc::from(name.as_str()))).collect();
        let outline = Rc::new(Outline { slots, returns: Some(Ty::Applied(generic.clone(), params)) });
        self.case_outlines.insert((generic.at, index), outline.clone());
        outline
    }

    /// The type that a generic class or enum given these types is: an
    /// object of the copy `Stack[int]`, or a value of the copy `Maybe[int]`.
    fn copy_type(&mut self, generic: &Rc<Generic>, types: Vec<Ty>, span: Span) -> Option<Ty> {
        match &generic.template {
            Template::Enum(_) => self.copy_enum(generic, types, Some(span)).map(Ty::EnumValue),
            _ => self.copy_class(generic, types, Some(span)).map(Ty::Instance),
        }
    }

    /// A case of a generic enum called with the values it carries:
    /// `Just(3)`. Which copy of the enum it makes is worked out as for a
    /// call of a generic function, from the values and then from the
    /// place the result goes, and the call is then a call of that copy's
    /// case. The compilers find the copy by the call.
    fn call_generic_case(
        &mut self,
        generic: &Rc<Generic>,
        index: usize,
        mut arguments: Vec<Argument>,
        later: &[Later],
        call: &Expr,
        wanted: Option<Ty>,
        span: Span,
    ) -> Ty {
        let outline = self.case_outline(generic, index);
        let Some(types) = self.infer_from(generic, &outline, Some(index), &arguments, later, wanted, span) else {
            return Ty::Invalid;
        };
        if !self.settle_later(generic, &outline, &types, &mut arguments, later) {
            return Ty::Invalid;
        }
        match self.copy_enum(generic, types, Some(span)) {
            Some(copy) => {
                self.types.copy_called.insert(call as *const Expr as usize, copy.name.clone());
                self.call(&Ty::CaseConstructor(copy, index), arguments, span)
            }
            None => Ty::Invalid,
        }
    }

    /// A case of a generic enum that carries nothing, used as a value:
    /// `Nothing`. It says nothing about `T`, so the place it goes must
    /// (decision 58.1): the type of the variable, of the parameter, of
    /// what the function returns, or of the value it is compared with.
    fn generic_bare_case(&mut self, generic: &Rc<Generic>, name: &str, expr: &Expr, span: Span) -> Ty {
        let wanted = match self.expected_case.take() {
            Some((place, wanted)) if place == expr as *const Expr as usize => Some(wanted),
            _ => None,
        };
        self.bare_case_as(generic, name, expr as *const Expr as usize, wanted, span)
    }

    /// `generic_bare_case`, with the place's type given. `at` is the
    /// address of the name, by which the compilers find the copy.
    fn bare_case_as(&mut self, generic: &Rc<Generic>, name: &str, at: usize, wanted: Option<Ty>, span: Span) -> Ty {
        // `Maybe[int] or none`, and a parameter that only looks.
        let copy = match wanted.as_ref().map(|wanted| wanted.clone().looked_at()) {
            Some(Ty::Invalid) => return Ty::Invalid,
            Some(Ty::Optional(inner)) => match inner.looked_at() {
                Ty::EnumValue(info) => Some(info),
                _ => None,
            },
            Some(Ty::EnumValue(info)) => Some(info),
            _ => None,
        };
        match copy {
            Some(copy) if copy.origin.as_ref().is_some_and(|(origin, _)| Rc::ptr_eq(origin, generic)) => {
                self.types.copy_called.insert(at, copy.name.clone());
                Ty::EnumValue(copy)
            }
            Some(copy) => self.error(format!("`{name}` is a case of `{}`, and a {} is wanted here", generic.name, copy.name), span),
            None if !self.last_round => Ty::Invalid,
            None => {
                let example = vec!["int"; generic.params.len()].join(", ");
                self.error(
                    format!(
                        "`{name}` is a case of the generic enum `{0}`, and nothing here says which `{0}` it is. \
                         Put it where its type is written, as in `let x: {0}[{example}] = {name}`",
                        generic.name
                    ),
                    span,
                )
            }
        }
    }

    /// Give each bare case of a generic enum that waited (`later`) the
    /// type its parameter has now that `types` are known: the `End` of
    /// `Link(3, End)` is a `Chain[int]`. False if one cannot be told.
    fn settle_later(&mut self, generic: &Rc<Generic>, outline: &Outline, types: &[Ty], arguments: &mut [Argument], later: &[Later]) -> bool {
        let found: HashMap<String, Ty> = generic.params.iter().cloned().zip(types.iter().cloned()).collect();
        for waiting in later {
            let argument = &arguments[waiting.at];
            let slot = match &argument.name {
                None => outline.slots.get(arguments[..waiting.at].iter().filter(|a| a.name.is_none()).count()),
                Some(name) => outline.slots.iter().find(|slot| slot.name == *name),
            };
            let (place, span) = (argument.place, argument.span);
            let wanted = match slot {
                Some(slot) => self.filled(&slot.ty, &found, span),
                None => None,
            };
            let ty = match self.bare_case_as(&waiting.generic, &waiting.name, place, wanted, span) {
                Ty::Invalid => return false,
                ty if ty.is_owned() && !waiting.keeps => {
                    self.types.lent.insert(place);
                    lent(&ty)
                }
                ty => ty,
            };
            arguments[waiting.at].ty = ty;
        }
        true
    }

    /// What each type parameter of `generic` is at this call, in the order
    /// they are declared, from the parameters and the result in `outline`:
    /// those of the generic, or of one case of a generic enum, whose
    /// place is `case`. The arguments in `later` say nothing yet. `None`
    /// if they cannot all be told, which is an error unless a later pass
    /// may still settle it.
    #[allow(clippy::too_many_arguments)]
    fn infer_from(
        &mut self,
        generic: &Rc<Generic>,
        outline: &Outline,
        case: Option<usize>,
        arguments: &[Argument],
        later: &[Later],
        wanted: Option<Ty>,
        span: Span,
    ) -> Option<Vec<Ty>> {
        // Which parameter each argument is for, as `arguments` works it out.
        let mut pairs: Vec<(&Slot, &Argument)> = Vec::new();
        let mut next = 0;
        let waits = |at: usize| later.iter().any(|waiting| waiting.at == at);
        for (at, argument) in arguments.iter().enumerate() {
            let slot = match &argument.name {
                None => {
                    next += 1;
                    outline.slots.get(next - 1)
                }
                Some(name) => outline.slots.iter().find(|slot| slot.name == *name),
            };
            if !waits(at) {
                pairs.extend(slot.map(|slot| (slot, argument)));
            }
        }

        // First, what the arguments say. A number that is written out,
        // such as the `0` of `ring(4, 0)`, waits until last: it can be
        // an int or a u8, and something else may say which.
        let mut found: HashMap<String, Ty> = HashMap::new();
        for (slot, argument) in pairs.iter().filter(|(_, argument)| argument.written.is_none()) {
            self.bind(&slot.ty, &argument.ty, &mut found);
        }
        // An argument that is a function says what it returns only once it
        // is known what it will be called with: in `apply(5, double)`
        // with `f: function(T) -> U`, the 5 gives `T`, and `double` called
        // with a `T` gives `U`.
        for (slot, argument) in &pairs {
            let Ty::Callable(pattern) = &slot.ty else {
                continue;
            };
            let takes: Option<Vec<Ty>> = pattern.params.iter().map(|ty| self.filled(ty, &found, span)).collect();
            let (Some(takes), true) = (takes, self.filled(&pattern.returns, &found, span).is_none()) else {
                continue;
            };
            // A built-in such as `len` says what it returns when called too.
            let fits = matches!(argument.ty, Ty::Builtin(_))
                || self.callable(&argument.ty).is_some_and(|(slots, _)| slots.len() == takes.len());
            if fits {
                let takes =
                    takes.into_iter().map(|ty| Argument { name: None, ty, span: argument.span, written: None, place: 0 }).collect();
                let returns = self.call(&argument.ty, takes, argument.span);
                self.bind(&pattern.returns, &returns, &mut found);
            }
        }
        // Then, for what no argument has settled, the place the result
        // goes: `let bytes: Ring[u8] = ring(4, 0)`.
        // What the written arguments say by themselves: the `[1]` of
        // `first([1])` says a list of ints.
        let mut from_written = HashMap::new();
        for (slot, argument) in pairs.iter().filter(|(_, argument)| argument.written.is_some()) {
            self.bind(&slot.ty, &argument.ty, &mut from_written);
        }
        if let (Some(returns), Some(wanted)) = (&outline.returns, &wanted) {
            let mut from_wanted = HashMap::new();
            self.bind(returns, wanted, &mut from_wanted);
            for (name, ty) in from_wanted {
                // The place can say which kind of number a written one
                // is (`let bytes: list[u8] = twice([1, 2])`). It cannot
                // make it something it could never be: in
                // `let x: str = first([1])` the list is a list of ints
                // whatever `x` wants, and the mistake is that an int is
                // put in a str, which is then said at `x` (17.1).
                let never = from_written.get(&name).is_some_and(|said: &Ty| {
                    let a_number = matches!(said, Ty::Int | Ty::Float) && takes_a_number(&ty);
                    !found.contains_key(&name) && !fits(said, &ty) && !a_number
                });
                if never {
                    continue;
                }
                // The place can ask for more than the arguments say:
                // `let pair: Pair[Shape] = Pair(circle, square)` is a pair
                // of shapes, and `let box: Box[int or none] = Box(count)`
                // can hold `none` later. That is so when each argument goes
                // straight into a `T`. A `list[Circle]` given for a
                // `list[T]` stays what it is: it is not a list of shapes.
                // A list that is written out says what it holds itself:
                // `first([1, 2])` is a list of ints, also where the
                // result goes into an `int or none`.
                let in_a_list = pairs.iter().any(|(slot, argument)| {
                    matches!(argument.written, Some(Written::List(_))) && mentions(&slot.ty, Some(&name))
                });
                if in_a_list && asks_more(&ty) {
                    continue;
                }
                let wider = found.get(&name).is_some_and(|said| {
                    !same(said, &ty)
                        && fits(said, &ty)
                        && pairs.iter().all(|(slot, _)| !mentions(&slot.ty, Some(&name)) || straight(&slot.ty))
                });
                if wider {
                    found.insert(name, ty);
                } else {
                    found.entry(name).or_insert(ty);
                }
            }
        }
        // Last, the written numbers, as the ints they are if nothing
        // else has spoken.
        for (name, ty) in from_written {
            found.entry(name).or_insert(ty);
        }

        let unsettled = !self.last_round
            || matches!(wanted, Some(Ty::Invalid))
            || arguments.iter().enumerate().any(|(at, argument)| matches!(argument.ty, Ty::Invalid) && !waits(at));
        let loose = arguments.iter().any(|argument| argument.ty.has_dynamic());
        let mut types = Vec::new();
        for name in &generic.params {
            let ty = match found.get(name) {
                Some(ty) => ty.clone(),
                // In a program with types left out, an argument whose type
                // is only known at run time makes `T` the same.
                None if loose && !self.strict_at(span) => Ty::Dynamic,
                None if unsettled => return None,
                None => {
                    // An argument that was meant to say, and is the wrong
                    // shape to: a 5 where a `list[T]` goes. (An empty
                    // list is the right shape, and says nothing.)
                    let wrong = pairs
                        .iter()
                        .find(|(slot, argument)| mentions(&slot.ty, Some(name)) && !argument.ty.has_dynamic());
                    if let Some((slot, argument)) = wrong {
                        self.error(
                            format!(
                                "`{}` of `{}` should be {}, but this is {}",
                                slot.name, generic.name, slot.ty, argument.ty
                            ),
                            argument.span,
                        );
                        return None;
                    }
                    // `first([200 ms, 1 s])`: the list says nothing, since
                    // its items are not all one type, and a build gives a
                    // list one type (58.8).
                    let mixed = arguments.iter().find_map(|argument| self.mixed.get(&argument.place).map(|kinds| (kinds, argument.span)));
                    if let Some((kinds, at)) = mixed {
                        let mut shown: Vec<String> = kinds.iter().map(|kind| kind.to_string()).collect();
                        let last = shown.pop().unwrap_or_default();
                        let shown = match shown.is_empty() {
                            true => last,
                            false => format!("{} and {last}", shown.join(", ")),
                        };
                        let units: Vec<&str> = kinds.iter().filter_map(|kind| match kind { Ty::Quantity(unit) => Some(&**unit), _ => None }).collect();
                        let numbers = kinds.iter().all(|kind| matches!(kind, Ty::Int | Ty::Float));
                        let hint = if units.len() == kinds.len() {
                            let finest = units[1..].iter().fold(units[0].to_string(), |finest, unit| {
                                units::common(&finest, unit).map_or(finest.clone(), |(common, _, _)| common.to_string())
                            });
                            format!("Write them all in one unit, such as `{finest}`")
                        } else if numbers {
                            "Write each as a float, as `1.0` for `1`".to_string()
                        } else {
                            "Make them one type, or an enum with a case for each".to_string()
                        };
                        self.error(
                            format!(
                                "`{}` cannot tell what `{name}` is from this list: its items are not all one type, \
                                 but {}, and a build gives a list one type. {hint}",
                                generic.name, shown
                            ),
                            at,
                        );
                        return None;
                    }
                    let types = vec!["int"; generic.params.len()].join(", ");
                    let (who, example) = match (&generic.template, case) {
                        (Template::Class(_), _) => (generic.name.clone(), format!("let x: {0}[{types}] = {0}(...)", generic.name)),
                        (Template::Function(_), _) => (generic.name.clone(), format!("let x: list[int] = {}(...)", generic.name)),
                        (Template::Enum(decl), index) => {
                            let case = &decl.variants[index.unwrap_or(0)].name;
                            (case.clone(), format!("let x: {}[{types}] = {case}(...)", generic.name))
                        }
                    };
                    self.error(
                        format!(
                            "`{who}` cannot tell what `{name}` is from this call: no argument says. \
                             Put the result where its type is written, as in `{example}`"
                        ),
                        span,
                    );
                    return None;
                }
            };
            // A copy is checked as if it were written by hand with this
            // type in the place of `T` (decision 56.11). What fills `T` is
            // the value itself, never a look at it: in the copy a parameter
            // written `T` looks, and one written `take T` takes.
            let ty = match unlent(ty) {
                // With one owner for everything, a list is an owned value
                // too, whatever it was called when the argument was read.
                list if self.one_owner && list.is_collection() => Ty::Owned(Box::new(list)),
                Ty::Optional(list) if self.one_owner && list.is_collection() => Ty::Optional(Box::new(Ty::Owned(list))),
                ty => ty,
            };
            types.push(ty);
        }
        Some(types)
    }

    /// Lay the pattern `pattern` over the type `actual`, and note what each
    /// type parameter in it has to be: `list[T]` over `list[int]` says
    /// that `T` is `int`. Where the two do not line up, nothing is noted
    /// and nothing is reported: the ordinary check of the arguments
    /// against the copy will say what is wrong, in terms of real types.
    fn bind(&mut self, pattern: &Ty, actual: &Ty, found: &mut HashMap<String, Ty>) {
        match (pattern, actual) {
            // Not settled, or only known at run time: it says nothing.
            (_, Ty::Invalid | Ty::Dynamic) => {}
            (Ty::Param(name), _) => {
                // A function is known by what it takes and returns.
                let actual = match actual {
                    Ty::Function(_) | Ty::BoundMethod(_) => match self.callable(actual) {
                        Some((slots, returns)) if !slots.iter().any(|slot| slot.ty.is_loose()) && !returns.is_loose() => {
                            let params = slots.into_iter().map(|slot| slot.ty).collect();
                            let raises = match actual {
                                Ty::Function(function) | Ty::BoundMethod(function) => {
                                    let signature = self.signature(function);
                                    self.raises_in_type(&signature.raises)
                                }
                                _ => None,
                            };
                            Ty::Callable(Rc::new(CallableType { params, returns, raises, owned: self.one_owner }))
                        }
                        _ => return,
                    },
                    other => other.clone(),
                };
                let merged = match found.get(name.as_ref()) {
                    None => actual,
                    // Two arguments, one `T`. A u8 and an int make it an
                    // int; `none` and an int make it `int or none`. Two
                    // types with nothing in common leave the first, and
                    // the check of the arguments reports the second.
                    Some(old) if fits(&actual, old) => return,
                    Some(old) if fits(old, &actual) => actual,
                    Some(old) if matches!(old, Ty::None) || matches!(actual, Ty::None) => join(old, &actual),
                    Some(_) => return,
                };
                found.insert(name.to_string(), merged);
            }
            (Ty::List(pattern), Ty::List(actual))
            | (Ty::Optional(pattern), Ty::Optional(actual))
            | (Ty::Owned(pattern), Ty::Owned(actual))
            | (Ty::Borrowed(pattern), Ty::Borrowed(actual)) => self.bind(pattern, actual, found),
            // `dict[str, T]` over `dict[str, int]`.
            (Ty::Dict(key, value), Ty::Dict(actual_key, actual_value)) => {
                self.bind(key, actual_key, found);
                self.bind(value, actual_value, found);
            }
            // A plain value where `T or none` is wanted, a value lent to a
            // parameter that looks, and a result that goes into an `or none`.
            (Ty::Optional(pattern), actual) if !matches!(actual, Ty::None) => self.bind(pattern, actual, found),
            (Ty::Borrowed(pattern), actual) => self.bind(pattern, actual, found),
            (pattern, Ty::Optional(actual)) => self.bind(pattern, actual, found),
            // A look at a `Stack[int]` or a `Maybe[int]` that somebody
            // owns, given to a parameter written `Stack[T]`, which looks
            // (decision 58.2).
            (Ty::Applied(..), Ty::Borrowed(actual)) => self.bind(pattern, actual, found),
            // `Stack[T]` over `Stack[int]`, and `Maybe[T]` over `Maybe[int]`.
            (Ty::Applied(generic, patterns), Ty::Instance(_) | Ty::EnumValue(_)) => {
                let origin = match actual {
                    Ty::Instance(class) => &class.origin,
                    Ty::EnumValue(info) => &info.origin,
                    _ => &None,
                };
                if let Some((origin, types)) = origin {
                    if Rc::ptr_eq(origin, generic) {
                        for (pattern, actual) in patterns.iter().zip(types) {
                            self.bind(pattern, actual, found);
                        }
                    }
                }
            }
            (Ty::Callable(pattern), Ty::Callable(actual)) => {
                for (pattern, actual) in pattern.params.iter().zip(&actual.params) {
                    self.bind(pattern, actual, found);
                }
                self.bind(&pattern.returns, &actual.returns, found);
            }
            (Ty::Several(patterns), Ty::Several(actuals)) => {
                for (pattern, actual) in patterns.iter().zip(actuals) {
                    self.bind(pattern, actual, found);
                }
            }
            (Ty::Callable(pattern), Ty::Function(_) | Ty::BoundMethod(_)) => {
                if let Some((slots, returns)) = self.callable(actual) {
                    for (pattern, slot) in pattern.params.iter().zip(&slots) {
                        self.bind(pattern, &slot.ty, found);
                    }
                    self.bind(&pattern.returns, &returns, found);
                }
            }
            _ => {}
        }
    }

    /// `pattern` with each type parameter replaced by what was found for
    /// it. `None` if one of them has not been found.
    fn filled(&mut self, pattern: &Ty, found: &HashMap<String, Ty>, span: Span) -> Option<Ty> {
        Some(match pattern {
            Ty::Param(name) => found.get(name.as_ref())?.clone(),
            Ty::List(inner) => Ty::list(self.filled(inner, found, span)?),
            Ty::Dict(key, value) => {
                let key = self.filled(key, found, span)?;
                let value = self.filled(value, found, span)?;
                // A `dict[K, V]` is a dict like any other: its keys are
                // text or ints, and no value of it is `none`.
                if !matches!(key.clone().looked_at(), Ty::Str | Ty::Int) && !key.is_loose() {
                    self.error(format!("a key of a dict is a str or an int, and this is {key}"), span);
                    return None;
                }
                if let Some(why) = self.no_such_value(&value) {
                    self.error(why, span);
                    return None;
                }
                Ty::dict(key, value)
            }
            Ty::Optional(inner) => Ty::Optional(Box::new(self.filled(inner, found, span)?)),
            Ty::Owned(inner) => Ty::Owned(Box::new(self.filled(inner, found, span)?)),
            Ty::Borrowed(inner) => Ty::Borrowed(Box::new(self.filled(inner, found, span)?)),
            Ty::Callable(callable) => {
                let params: Option<Vec<Ty>> = callable.params.iter().map(|ty| self.filled(ty, found, span)).collect();
                let returns = self.filled(&callable.returns, found, span)?;
                Ty::Callable(Rc::new(CallableType {
                    params: params?,
                    returns,
                    raises: callable.raises.clone(),
                    owned: callable.owned,
                }))
            }
            Ty::Applied(generic, types) => {
                let types: Option<Vec<Ty>> = types.iter().map(|ty| self.filled(ty, found, span)).collect();
                self.copy_type(generic, types?, span)?
            }
            Ty::Several(types) => {
                let types: Option<Vec<Ty>> = types.iter().map(|ty| self.filled(ty, found, span)).collect();
                Ty::Several(types?)
            }
            other => other.clone(),
        })
    }

    /// The name of the copy of `generic` for `types`, the scope in which
    /// its type parameters mean those types, and the note that goes under
    /// an error found inside it. `None` if the copies have got out of hand.
    /// A type as it is spelled in the name of a copy. With no collector
    /// every list and dict has one owner, so `owned` would say nothing:
    /// the copy of `Box` for a list of ints is `Box[list[int]]`.
    fn spelled(&self, ty: &Ty) -> String {
        fn unowned(ty: &Ty) -> Ty {
            match ty {
                Ty::Owned(inner) => unowned(inner),
                Ty::List(item) => Ty::List(Box::new(unowned(item))),
                Ty::Fixed(item, length) => Ty::Fixed(Box::new(unowned(item)), *length),
                Ty::Dict(key, value) => Ty::Dict(Box::new(unowned(key)), Box::new(unowned(value))),
                Ty::Optional(inner) => Ty::Optional(Box::new(unowned(inner))),
                other => other.clone(),
            }
        }
        match self.one_owner {
            true => spelled(&unowned(ty)),
            false => spelled(ty),
        }
    }

    /// What each type parameter of `generic` asks of its type, worked out
    /// once from what is written after `is` (decision 58.3). A mistake in
    /// a bound is reported here, on the line of the generic.
    fn asks(&mut self, generic: &Rc<Generic>) -> Rc<[Option<Bound>]> {
        if let Some(asks) = generic.asks.borrow().as_ref() {
            return asks.clone();
        }
        let mut asks = Vec::new();
        for (index, (param, bound)) in generic.params.iter().zip(&generic.bounds).enumerate() {
            let Some(bound) = bound else {
                // `[T is Ordered, Shape]` makes `Shape` a second type
                // parameter, not a second thing `T` asks (58.4).
                if generic.bounds[..index].iter().any(Option::is_some) {
                    if let Some(Ty::Class(interface)) = generic.env.get(param).map(|variable| variable.ty) {
                        if interface.interface {
                            let first = &generic.params[index - 1];
                            self.error(
                                format!(
                                    "`{param}` after a comma is a second type parameter, which hides the interface \
                                     `{param}`. A type parameter asks for one interface: `[{first} is ...]`. \
                                     If `{param}` is meant to be a type parameter, give it another name"
                                ),
                                generic.span,
                            );
                        }
                    }
                }
                asks.push(None);
                continue;
            };
            let is_ordered = matches!(bound, ast::Type::Named { name, args } if name == "Ordered" && args.is_empty());
            // `Ordered` comes with Zeno, unless the program has its own.
            if is_ordered && generic.env.get("Ordered").is_none() {
                asks.push(Some(Bound::Ordered));
                continue;
            }
            let written = crate::printer::type_(bound);
            let ask = match self.resolve(bound, &generic.env, generic.span) {
                Ty::Instance(interface) if interface.interface => Some(Bound::Interface(interface)),
                Ty::Invalid => None,
                Ty::Instance(_) => {
                    self.error(
                        format!(
                            "after `is` comes an interface, and `{written}` is a class. Write `{written}` itself \
                             where `{param}` is used, or declare an interface that `{written}` is"
                        ),
                        generic.span,
                    );
                    None
                }
                _ => {
                    self.error(
                        format!(
                            "after `is` comes an interface, such as `Ordered` or one the program declares, \
                             and `{written}` is not one"
                        ),
                        generic.span,
                    );
                    None
                }
            };
            asks.push(ask);
        }
        let asks: Rc<[Option<Bound>]> = Rc::from(asks);
        *generic.asks.borrow_mut() = Some(asks.clone());
        asks
    }

    /// Does each type given to `generic` have what its parameter asks?
    /// If not, the error is here, where it is asked for: "`Task` is not
    /// `Ordered`, which `largest` asks of `T`" (decision 58.3).
    fn meets_bounds(&mut self, generic: &Rc<Generic>, types: &[Ty], asked: Option<Span>) -> bool {
        if generic.bounds.iter().all(Option::is_none) {
            return true;
        }
        let asks = self.asks(generic);
        let mut meets = true;
        for (index, ty) in types.iter().enumerate() {
            let Some(Some(ask)) = asks.get(index) else {
                continue;
            };
            let mut bare = ty;
            while let Ty::Owned(inner) | Ty::Borrowed(inner) = bare {
                bare = inner;
            }
            let param = &generic.params[index];
            let shown = self.spelled(bare);
            let message = match ask {
                Bound::Ordered => match bare {
                    Ty::Int | Ty::Float | Ty::F32 | Ty::U64 | Ty::Sized(_) | Ty::Quantity(_) | Ty::Str => None,
                    Ty::Invalid | Ty::Dynamic => None,
                    Ty::Instance(_) => Some(format!(
                        "`{shown}` is not `Ordered`, which `{}` asks of `{param}`. `Ordered` is what `<` works \
                         on: a number, a quantity or text. A class cannot be `Ordered` yet, since `<` between \
                         its objects comes with operators for a class",
                        generic.name
                    )),
                    _ => Some(format!(
                        "`{shown}` is not `Ordered`, which `{}` asks of `{param}`. `Ordered` is what `<` works \
                         on: a number, a quantity or text",
                        generic.name
                    )),
                },
                Bound::Interface(interface) => match bare {
                    Ty::Invalid | Ty::Dynamic => None,
                    Ty::Instance(class) if Rc::ptr_eq(class, interface) || is_one(bare, &Ty::Instance(interface.clone())) => None,
                    Ty::Instance(class) if !class.interface => Some(format!(
                        "`{shown}` is not `{1}`, which `{0}` asks of `{param}`. It is one if it says so: \
                         `class {shown} is {1}`, with what `{1}` lists",
                        generic.name, interface.name
                    )),
                    _ => Some(format!(
                        "`{shown}` is not `{}`, which `{}` asks of `{param}`",
                        interface.name, generic.name
                    )),
                },
            };
            if let Some(message) = message {
                // `let b: Box[Task] = Box(Task(1))` asks twice on one line.
                let at = asked.unwrap_or(generic.span);
                if self.unmet.insert((generic.at, shown, at.line)) {
                    self.error(message, at);
                }
                meets = false;
            }
        }
        meets
    }

    fn copy_setting(&mut self, generic: &Rc<Generic>, types: &[Ty], asked: Option<Span>) -> Option<(String, Env, String)> {
        let spelled: Vec<String> = types.iter().map(|ty| self.spelled(ty)).collect();
        let name = format!("{}[{}]", generic.name, spelled.join(", "));
        // `class Node[T] { next: Node[list[T]] or none }` asks for
        // Node[int], Node[list[int]], Node[list[list[int]]], with no end.
        if name.len() > 300 || self.copies.len() > 2000 {
            let start: String = name.chars().take(60).collect();
            self.error(
                format!(
                    "`{}` needs a copy of itself for a bigger type every time, and that never ends. \
                     It got as far as `{start}...`",
                    generic.name
                ),
                asked.unwrap_or(generic.span),
            );
            return None;
        }
        // A type parameter is no owner of a resource (rule 7 of 59.1).
        if let Some((param, _)) = generic.params.iter().zip(types).find(|(_, ty)| self.holds_resource(ty)) {
            self.resource_inside(&format!("`{param}` of `{}`", generic.name), asked.unwrap_or(generic.span));
            return None;
        }
        let env = Scope::child(&generic.env);
        let mut means = Vec::new();
        // Checked on its own, a generic whose parameter says `is Shape`
        // is checked with that parameter as a `Shape`, so a call of a
        // function `Shape` does not list is found though nothing uses
        // the generic (58.5). Any other parameter stands for "anything".
        let alone = types.iter().all(|ty| matches!(ty, Ty::Invalid));
        let asks = if alone { self.asks(generic) } else { Rc::from(Vec::new()) };
        for (index, (param, ty)) in generic.params.iter().zip(types).enumerate() {
            let ty = match asks.get(index) {
                Some(Some(Bound::Interface(interface))) => Ty::Instance(interface.clone()),
                _ => ty.clone(),
            };
            env.declare(param, Ty::Alias(Box::new(ty)), false);
            self.type_params.extend(env.get(param).map(|variable| variable.id));
            means.push(format!("{param} = {}", spelled.get(means.len()).cloned().unwrap_or_default()));
        }
        let mut note = format!("this is inside `{name}`: `{}` with {}", generic.name, means.join(", "));
        if let Some(asked) = asked {
            note += &format!(", which line {} asks for", asked.line_in_file());
        }
        Some((name, env, note))
    }

    /// The copy of a generic function for these types: an ordinary
    /// function called `first[int]`. `asked` is where the program asks
    /// for it. With every type `Invalid` it is the generic checked on its
    /// own (see `finish_generics`), which keeps its plain name.
    fn copy_function(&mut self, generic: &Rc<Generic>, types: Vec<Ty>, asked: Option<Span>) -> Option<Rc<FunctionInfo>> {
        let Template::Function(template) = &generic.template else {
            return None;
        };
        let alone = types.iter().all(|ty| matches!(ty, Ty::Invalid));
        if !alone && !self.meets_bounds(generic, &types, asked) {
            return None;
        }
        let key = (generic.at, if alone { String::new() } else { types.iter().map(|ty| self.spelled(ty)).collect::<Vec<_>>().join(", ") });
        if let Some(Made::Function(copy)) = self.copies.get(&key) {
            return Some(copy.clone());
        }
        let (name, env, note) = self.copy_setting(generic, &types, asked)?;
        let mut decl = template.fresh();
        decl.type_params.clear();
        if !alone {
            decl.name = name;
        }
        let copy = Rc::new(FunctionInfo {
            decl: Arc::new(decl),
            env,
            span: generic.span,
            self_type: None,
            state: RefCell::new(State::NotChecked),
            above: generic.above.clone(),
            copy_note: (!alone).then_some(note),
        });
        self.copies.insert(key, Made::Function(copy.clone()));
        if !alone {
            self.used.insert(generic.at);
            self.types.function_copies.entry(generic.at).or_default().push(copy.decl.clone());
        }
        Some(copy)
    }

    /// The copy of a generic class for these types: an ordinary class
    /// called `Stack[int]`.
    fn copy_class(&mut self, generic: &Rc<Generic>, types: Vec<Ty>, asked: Option<Span>) -> Option<Rc<ClassInfo>> {
        let Template::Class(template) = &generic.template else {
            return None;
        };
        // Checking the generic on its own fills every parameter with
        // `Invalid`, the type that is accepted everywhere.
        let alone = types.iter().all(|ty| matches!(ty, Ty::Invalid));
        if !alone && !self.meets_bounds(generic, &types, asked) {
            return None;
        }
        let key = (generic.at, if alone { String::new() } else { types.iter().map(|ty| self.spelled(ty)).collect::<Vec<_>>().join(", ") });
        if let Some(Made::Class(copy)) = self.copies.get(&key) {
            return Some(copy.clone());
        }
        let (name, env, note) = self.copy_setting(generic, &types, asked)?;
        let mut decl = template.fresh();
        decl.type_params.clear();
        if !alone {
            decl.name = name;
        }
        let decl = Arc::new(decl);
        let origin = (!alone).then(|| (generic.clone(), types));
        let copy =
            class_info(&decl, self.one_owner, &env, &generic.above, generic.span, origin, (!alone).then_some(note));
        // Noted before its fields are worked out: a class may mention itself.
        self.copies.insert(key, Made::Class(copy.clone()));
        if !alone {
            self.used.insert(generic.at);
            self.classes.insert(copy.id.clone(), copy.clone());
            self.types.class_copies.entry(generic.at).or_default().push(decl.clone());
        }
        // A mistake in the fields of a copy says which copy it is in, as
        // one in its methods does.
        self.copy_notes.push(copy.copy_note.clone());
        self.fill_class(&decl, &copy, &env, generic.span);
        self.copy_notes.pop();
        self.pending.push((decl, copy.clone()));
        Some(copy)
    }

    /// The copy of a generic enum for these types: an ordinary enum
    /// called `Maybe[int]`, whose cases carry ints where the generic's
    /// carry a `T` (decision 58.1).
    fn copy_enum(&mut self, generic: &Rc<Generic>, types: Vec<Ty>, asked: Option<Span>) -> Option<Rc<EnumInfo>> {
        let Template::Enum(template) = &generic.template else {
            return None;
        };
        let alone = types.iter().all(|ty| matches!(ty, Ty::Invalid));
        if !alone && !self.meets_bounds(generic, &types, asked) {
            return None;
        }
        let key = (generic.at, if alone { String::new() } else { types.iter().map(|ty| self.spelled(ty)).collect::<Vec<_>>().join(", ") });
        if let Some(Made::Enum(copy)) = self.copies.get(&key) {
            return Some(copy.clone());
        }
        let (name, env, note) = self.copy_setting(generic, &types, asked)?;
        let mut decl = template.fresh();
        decl.type_params.clear();
        if !alone {
            decl.name = name;
        }
        let decl = Arc::new(decl);
        let carries = decl.variants.iter().any(|case| !case.fields.is_empty());
        let copy = Rc::new(EnumInfo {
            id: decl_id(generic.span, &decl.name),
            name: decl.name.clone(),
            cases: RefCell::new(Vec::new()),
            carries,
            owned: carries && self.one_owner,
            numbers: None,
            origin: (!alone).then(|| (generic.clone(), types)),
        });
        // Noted before its cases are worked out: an enum may mention
        // itself, as `enum Chain[T] { End; Link(item: T, rest: Chain[T]) }`.
        self.copies.insert(key, Made::Enum(copy.clone()));
        self.copy_notes.push((!alone).then_some(note));
        let span = generic.span;
        let cases: Vec<CaseInfo> = decl
            .variants
            .iter()
            .map(|case| CaseInfo {
                name: case.name.clone(),
                fields: case.fields.iter().map(|field| self.slot(field, &case.name, &env, span)).collect(),
            })
            .collect();
        // As for any enum: a value that is shared cannot hold one that
        // has one owner (rule 6 of docs/ownership.md).
        if !copy.owned && cases.iter().any(|case| case.fields.iter().any(|field| field.ty.is_owned())) {
            self.error(format!("a case of the enum `{}` cannot carry an owned value yet", decl.name), asked.unwrap_or(span));
        }
        self.copy_notes.pop();
        if !alone {
            self.used.insert(generic.at);
            self.enums.insert(copy.id.clone(), copy.clone());
            self.types.enums.insert(
                Arc::as_ptr(&decl) as usize,
                cases.iter().map(|case| case.fields.iter().map(|slot| kind(&slot.ty)).collect()).collect(),
            );
            self.types.enum_copies.entry(generic.at).or_default().push(decl);
        }
        *copy.cases.borrow_mut() = cases;
        Some(copy)
    }

    /// The end of a pass: check the bodies of the class copies that were
    /// made along the way, and every generic that nothing used.
    fn finish_generics(&mut self) {
        let mut alone = 0;
        loop {
            if let Some((decl, class)) = self.pending.pop() {
                self.copy_notes.push(class.copy_note.clone());
                let env = class.env.clone();
                self.class_body(&decl, &class, &env);
                self.copy_notes.pop();
                continue;
            }
            // A generic nobody uses has no copy to check. So that a
            // mistake in it is still found, it is checked once with its
            // type parameters standing for "anything". That finds a
            // misspelt name or a wrong number of arguments, though not a
            // `+` that only some types have.
            let Some(generic) = self.generics.get(alone).cloned() else {
                break;
            };
            alone += 1;
            // A mistake after `is` is found though nothing uses it.
            self.asks(&generic);
            if !self.used.contains(&generic.at) {
                let anything = vec![Ty::Invalid; generic.params.len()];
                match &generic.template {
                    Template::Function(_) => {
                        if let Some(copy) = self.copy_function(&generic, anything, None) {
                            self.signature(&copy);
                        }
                    }
                    Template::Class(_) => {
                        self.copy_class(&generic, anything, None);
                    }
                    Template::Enum(_) => {
                        self.copy_enum(&generic, anything, None);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------- Operators

    /// The functions of `math` work in floats: an f32 given to one is
    /// made a float on the way in (decision 41.6).
    fn math_widens(&mut self, arguments: &[Argument]) {
        for argument in arguments {
            if matches!(argument.ty.widened(), Ty::F32) {
                self.types.widened.insert((argument.span.line, argument.span.col));
            }
        }
    }

    /// A float written out beside an f32, as in `gain * 0.5`, is an f32
    /// too (decision 41.4): this gives its type there, and notes it.
    /// So is a list of them beside a list of f32s: `readings == [0.5, 1.5]`.
    fn real_beside(&mut self, ty: Ty, other: &Ty, expr: &Expr) -> Ty {
        let other = other.widened().looked_at();
        let wanted = match &other {
            Ty::F32 | Ty::U64 => other.clone(),
            Ty::List(item) | Ty::Fixed(item, _) if matches!(**item, Ty::F32 | Ty::U64) => Ty::list((**item).clone()),
            _ => return ty,
        };
        let Some(written) = written(expr) else { return ty };
        match written_fits(&written, &wanted) {
            Some(Ok(())) => {}
            Some(Err(message)) => return self.error(message, expr.span),
            None => return ty,
        }
        let mut reals = Vec::new();
        reals_into_f32(&written, &wanted, &mut reals);
        numbers_into_u64(&written, &wanted, &mut reals);
        if reals.is_empty() {
            return ty;
        }
        if matches!(wanted, Ty::U64) || matches!(&wanted, Ty::List(item) if matches!(**item, Ty::U64)) {
            self.types.u64s.extend(reals.iter().map(|at| (at.line, at.col)));
        } else {
            self.types.f32s.extend(reals.iter().map(|at| (at.line, at.col)));
        }
        sized_as(&ty, &wanted)
    }

    /// The type of `left op right`. The rules are the interpreter's, applied
    /// to types.
    fn binary(&mut self, op: BinaryOp, left: &Ty, right: &Ty, span: Span) -> Ty {
        use BinaryOp::*;
        let comparison = matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq | In | NotIn | Is | IsNot);
        // A sized number takes part as the int it is, and the answer is an
        // int: `a + b` on two u8s is never cut down to eight bits.
        let (left, right) = (&left.widened(), &right.widened());

        // With a side that is only known at run time, so is the result,
        // except that a comparison is always a bool.
        if left.is_loose() || right.is_loose() {
            return match (left, right) {
                _ if comparison => Ty::Bool,
                (Ty::Invalid, _) | (_, Ty::Invalid) => Ty::Invalid,
                _ => Ty::Dynamic,
            };
        }

        // An f32 with an f32 or an int is worked out in 32 bits, and the
        // answer is an f32. With a float, which of the two is meant is
        // not clear, and the program says (decision 41.2).
        if matches!(left, Ty::F32) || matches!(right, Ty::F32) {
            let other = if matches!(left, Ty::F32) { right } else { left };
            let sum = matches!(op, Add | Sub | Mul | Div | FloorDiv | Mod | Pow);
            let order = matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq);
            match other {
                Ty::F32 | Ty::Int if sum => return Ty::F32,
                Ty::F32 | Ty::Int if order => return Ty::Bool,
                Ty::Float if sum || order => {
                    self.error(
                        format!(
                            "`{}` between {left} and {right} would have to choose between 32 bits and 64. \
                             Write `f32( )` around the float to work in 32, or `float( )` around the f32 to work in 64",
                            op.text()
                        ),
                        span,
                    );
                    return if order { Ty::Bool } else { Ty::Invalid };
                }
                _ => {}
            }
        }

        // A u64 meets another u64, or a number written out, which has
        // become one by now. Next to an int, which of the two is meant is
        // not clear, and the program says (decision 42.3). A shift is by
        // a count, which may be either.
        if matches!(left, Ty::U64) || matches!(right, Ty::U64) {
            let other = if matches!(left, Ty::U64) { right } else { left };
            let sum = matches!(op, Add | Sub | Mul | FloorDiv | Mod | Pow | BitAnd | BitOr | BitXor);
            let order = matches!(op, Eq | NotEq | Lt | LtEq | Gt | GtEq);
            match (op, left, other) {
                (Shl | Shr, Ty::U64, Ty::U64 | Ty::Int) => return Ty::U64,
                (_, _, Ty::U64) if sum => return Ty::U64,
                (Div, _, Ty::U64) => return Ty::Float,
                (_, _, Ty::U64) if order => return Ty::Bool,
                (_, _, Ty::Int | Ty::Float | Ty::F32) if sum || order || matches!(op, Div | Shl | Shr) => {
                    let message = match other {
                        Ty::Int => format!(
                            "`{}` between {left} and {right} would have to choose between a u64 and an int. \
                             Write `u64( )` around the int to work in u64s, or `int( )` around the u64",
                            op.text()
                        ),
                        _ => format!(
                            "`{}` between {left} and {right} would have to choose between a u64 and a float. \
                             Write `float( )` around the u64 to work in floats",
                            op.text()
                        ),
                    };
                    self.error(message, span);
                    return if order { Ty::Bool } else { Ty::Invalid };
                }
                _ => {}
            }
        }

        let numbers = left.is_number() && right.is_number();
        let ints = matches!((left, right), (Ty::Int, Ty::Int));
        // Two quantities that measure the same thing, such as `s` and `ms`,
        // work together. The answer is in the finer unit of the two.
        let shared_unit = match (left, right) {
            (Ty::Quantity(a), Ty::Quantity(b)) => units::common(a, b).map(|(unit, _, _)| unit),
            _ => None,
        };
        let same_unit = shared_unit.is_some();

        let result = match op {
            Add | Sub if ints => Some(Ty::Int),
            Add | Sub if numbers => Some(Ty::Float),
            // `p + 4`: four items further on. Only looking at where a
            // pointer points needs `unsafe`, not working out where.
            Add | Sub if matches!(left, Ty::Pointer(_)) && fits(right, &Ty::Int) => Some(left.clone()),
            Add | Sub if same_unit => shared_unit.map(|unit| Ty::Quantity(Rc::from(unit))),
            Add => match (left, right) {
                (Ty::Str, Ty::Str) => Some(Ty::Str),
                // Two lists of known, unrelated things would make a list
                // of nothing in particular, which `append` already
                // refuses one item at a time (decision 5.19g).
                (Ty::List(a), Ty::List(b)) if !a.has_dynamic() && !b.has_dynamic() && join(a, b).has_dynamic() => {
                    return self.error(
                        format!(
                            "`+` joins two lists that hold the same kind of thing, and these are {left} and {right}"
                        ),
                        span,
                    );
                }
                (Ty::List(a), Ty::List(b)) => Some(Ty::list(join(a, b))),
                _ => None,
            },

            Mul if ints => Some(Ty::Int),
            Mul if numbers => Some(Ty::Float),
            Mul => match (left, right) {
                // `[0] * 64`: the items again and again (decision 36.3).
                // Items that are shared, such as lists, would be the same
                // one many times over, which nobody means.
                (Ty::List(item), Ty::Int) if is_shared(item) => {
                    return self.error(
                        format!(
                            "`*` would put the same {item} in the list again and again, and a change to one \
                             would be a change to all. Make each one: `[[0] * 4 for i in range(3)]`"
                        ),
                        span,
                    );
                }
                (Ty::List(_), Ty::Int) => Some(left.clone()),
                (Ty::Int, Ty::List(_)) => {
                    return self.error("`*` repeats a list when the list comes first: write `[0] * 64`", span);
                }
                (Ty::Quantity(_), other) if other.is_number() => Some(left.clone()),
                (other, Ty::Quantity(_)) if other.is_number() => Some(right.clone()),
                // `3.3 V * 20 mA` is a power (decision 43).
                (Ty::Quantity(a), Ty::Quantity(b)) => units::made("*", Some(a), b).map(made_ty),
                _ => None,
            },

            Div if numbers || same_unit => Some(Ty::Float),
            Div => match (left, right) {
                (Ty::Quantity(_), other) if other.is_number() => Some(left.clone()),
                (Ty::Quantity(a), Ty::Quantity(b)) => units::made("/", Some(a), b).map(made_ty),
                (other, Ty::Quantity(b)) if other.is_number() => units::made("/", None, b).map(made_ty),
                _ => None,
            },

            FloorDiv | Mod | Pow if ints => Some(Ty::Int),
            FloorDiv | Mod | Pow if numbers => Some(Ty::Float),

            BitAnd | BitOr | BitXor | Shl | Shr if ints => Some(Ty::Int),
            // Anything else has no bits to speak of. Two bools are the
            // likely slip, by someone who writes C.
            BitAnd | BitOr | BitXor | Shl | Shr => {
                let hint = match (op, left, right) {
                    (BitAnd, Ty::Bool, Ty::Bool) => ". For two bools Zeno spells this `and`",
                    (BitOr, Ty::Bool, Ty::Bool) => ". For two bools Zeno spells this `or`",
                    _ => "",
                };
                return self.error(
                    format!("`{}` works on the bits of whole numbers, and these are {left} and {right}{hint}", op.text()),
                    span,
                );
            }

            // `none` is tested for with `is`, the one word for it. `==`
            // compares two values, and `none` is the lack of one.
            Eq | NotEq if matches!(left, Ty::None) || matches!(right, Ty::None) => {
                let is = if op == Eq { "is none" } else { "is not none" };
                return self.error(format!("`none` is tested for with `is`: write `{is}`, not `{} none`", op.text()), span);
            }
            Eq | NotEq => {
                // Two types that can never hold the same value: the answer
                // is already known, which means the comparison is a mistake.
                let (left, right) = (&left.as_ints(), &right.as_ints());
                if !numbers && !same_unit && !fits(left, right) && !fits(right, left) {
                    let answer = if op == Eq { "false" } else { "true" };
                    self.error(
                        format!("`{}` between {left} and {right} is always {answer}", op.text()),
                        span,
                    );
                }
                Some(Ty::Bool)
            }
            Lt | LtEq | Gt | GtEq => {
                let both_strs = matches!((left, right), (Ty::Str, Ty::Str));
                (numbers || both_strs || same_unit).then_some(Ty::Bool)
            }
            In | NotIn => match (left, &right.as_ints()) {
                // `[3] in rows` only looks at the items, so an item with
                // one owner is compared as the list it is.
                (item, Ty::List(items)) if {
                    let (item, items) = (item.clone().looked_at(), items.as_ref().clone().looked_at());
                    fits(&item, &items) || fits(&items, &item)
                } =>
                {
                    Some(Ty::Bool)
                }
                // `"Ada" in ages`: is there such a key?
                (item, Ty::Dict(key, _)) if fits(item, key) => Some(Ty::Bool),
                (Ty::Str, Ty::Str) => Some(Ty::Bool),
                _ => None,
            },
            // A value whose type has no `or none` in it is never `none`,
            // so the answer is known, and the test is a mistake.
            Is | IsNot if !left.is_loose() && !matches!(left, Ty::Optional(_) | Ty::None) => {
                let answer = if op == Is { "false" } else { "true" };
                return self.error(
                    format!(
                        "`{} none` is always {answer}: this is {left}, which is never `none`. \
                         Only a value whose type says `or none` can be",
                        op.text()
                    ),
                    span,
                );
            }
            Is | IsNot => Some(Ty::Bool),
            _ => None,
        };

        match result {
            Some(ty) => ty,
            None => {
                let mut message = format!("`{}` does not work between {left} and {right}", op.text());
                if matches!(left, Ty::Optional(_)) || matches!(right, Ty::Optional(_)) {
                    message += &format!(". {}", self.none_first(span, "Check for `none` first"));
                }
                // Degrees and radians meet only when one is changed.
                if let (Ty::Quantity(a), Ty::Quantity(b)) = (&left, &right) {
                    if units::convert(a, b).is_some() && units::common(a, b).is_none() {
                        message += &format!(". Change one of them first, with `.{a}` or `.{b}`");
                    }
                }
                // The units that make other units (decision 43).
                let quantity = |ty: &Ty| matches!(ty, Ty::Quantity(_));
                if matches!(op, Mul | Div) && (quantity(&left) && quantity(&right) || matches!(op, Div) && left.is_number() && quantity(&right)) {
                    message += &format!(". The units Zeno makes from others are {}", units::made_list());
                }
                // One way to write it: the pointer first, then how far.
                if matches!(op, Add) && fits(&left, &Ty::Int) && matches!(right, Ty::Pointer(_)) {
                    message += ". Write the pointer first, as in `p + 1`";
                }
                let ty = self.error(message, span);
                if comparison { Ty::Bool } else { ty }
            }
        }
    }
}

/// The type of what `*` or `/` made from two quantities (decision 43).
fn made_ty(made: units::Made) -> Ty {
    match made {
        units::Made::Unit(unit) => Ty::Quantity(Rc::from(unit)),
        units::Made::Number => Ty::Float,
    }
}

const STR_METHODS: &[&str] =
    &["upper", "lower", "strip", "split", "lines", "replace", "starts_with", "ends_with", "find", "slice", "code"];
const LIST_METHODS: &[&str] = &[
    "append", "pop", "map", "filter", "reduce", "insert", "reverse", "sort", "clear", "find", "slice", "join",
];
const DICT_METHODS: &[&str] = &["get", "pop", "keys", "values", "clear"];
const PIN_METHODS: &[&str] = &["on", "off", "toggle", "is_on"];
const INPUT_PIN_METHODS: &[&str] = &["is_high", "is_low", "interrupt"];
const INTERRUPT_METHODS: &[&str] = &["wait", "edges"];
const ANALOG_PIN_METHODS: &[&str] = &["voltage", "count"];
const PWM_PIN_METHODS: &[&str] = &["duty", "pulse"];
const TICKER_METHODS: &[&str] = &["wait", "ticks", "restart"];
const SERIAL_METHODS: &[&str] = &["write", "write_bytes", "read_byte", "has_byte"];
const REGISTER_METHODS: &[&str] = &["read", "write"];

/// What is looking at a part of an owner's value, so that the part must
/// stay where it is.
#[derive(Clone, Copy, PartialEq)]
enum Looking {
    /// A `for` loop, or a comprehension, walking a list.
    Loop,
    /// A `for` loop walking a dict that owns its values (decision 56.12).
    DictLoop,
    /// A `case` of a `match`, which gave names to parts of the value.
    Case,
}

/// How much of the matched type a pattern accepts.
enum Covers {
    /// Any value: `_`, or a new name.
    Everything,
    /// The whole of one enum case.
    Case(usize),
    /// Only some values.
    Some,
}

/// What a type is written inside of. See `Checker::resolve_inside`.
#[derive(Clone, Copy, PartialEq)]
enum Inside {
    /// Nothing: it is the whole type, or follows `owned` or `take`.
    Front,
    /// The `[ ]` of a list.
    List,
    /// An `or none` that is itself at the front or in a list.
    Optional,
    /// The `[ ]` of a generic.
    Other,
}

/// "an owned Frame", "an owned list[int]".
fn an_owned(ty: &Ty) -> String {
    match ty {
        Ty::Owned(_) => format!("an {ty}"),
        _ => format!("an owned {ty}"),
    }
}

/// The value that a look is a look at: what a variable that owns it
/// would hold. The opposite of `lent`.
fn unlent(ty: Ty) -> Ty {
    match ty {
        Ty::Borrowed(inner) if inner.is_collection() => Ty::Owned(inner),
        Ty::Borrowed(inner) => *inner,
        Ty::Optional(inner) => Ty::Optional(Box::new(unlent(*inner))),
        other => other,
    }
}

/// A look at a value of type `ty`, which has one owner.
fn lent(ty: &Ty) -> Ty {
    match ty {
        Ty::Optional(inner) => Ty::Optional(Box::new(lent(inner))),
        _ => Ty::Borrowed(Box::new(ty.clone().looked_at())),
    }
}

/// Does a value of this type have an owner, so that a parameter can
/// look at it or `take` it? A list, an
/// object, an enum value that is kept in a block of its own, or one of
/// those that may be `none`.
fn borrowable(ty: &Ty) -> bool {
    match ty {
        Ty::List(_) | Ty::Dict(..) | Ty::Instance(_) | Ty::Applied(..) | Ty::Error(_) | Ty::Callable(_) => true,
        Ty::Owned(inner) | Ty::Optional(inner) => borrowable(inner),
        Ty::EnumValue(info) => info.carries,
        _ => false,
    }
}

/// Is this a list whose items have one owner each? Such a list is their
/// owner, and frees them when it is freed.
fn owns_items(ty: &Ty) -> bool {
    matches!(ty, Ty::List(item) | Ty::Fixed(item, _) if item.is_owned())
}

/// Is this a dict whose values have one owner each? It is their owner,
/// and frees them when it is freed (decision 56.12).
fn owns_values(ty: &Ty) -> bool {
    matches!(ty, Ty::Dict(_, value) if value.is_owned())
}

/// A list of a fixed length, as anything that only reads it sees it: a
/// list. `owned` and `or none` around it stay.
fn unfixed(ty: Ty) -> Ty {
    match ty {
        Ty::Fixed(item, _) => Ty::List(item),
        Ty::Owned(inner) => Ty::Owned(Box::new(unfixed(*inner))),
        Ty::Borrowed(inner) => Ty::Borrowed(Box::new(unfixed(*inner))),
        Ty::Optional(inner) => Ty::Optional(Box::new(unfixed(*inner))),
        other => other,
    }
}

/// How many items a list written out has, when that is known before the
/// program runs: `[1, 2, 3]` has three, and `[0] * 64` has 64.
fn written_length(written: &Written) -> Option<i128> {
    match written {
        Written::List(items) => Some(items.len() as i128),
        Written::Repeat { list, times: Some(times), .. } => match &**list {
            Written::List(items) => Some(items.len() as i128 * times.max(&0)),
            _ => None,
        },
        _ => None,
    }
}

/// A list whose length is known, as the list of a fixed length it fits.
fn fixed_as(ty: Ty, length: u64) -> Ty {
    match ty {
        Ty::List(item) => Ty::Fixed(item, length),
        Ty::Owned(inner) => Ty::Owned(Box::new(fixed_as(*inner, length))),
        other => other,
    }
}

/// The message for a list written out with the wrong number of items.
fn too_many_or_few(count: i128, wanted: &Ty, length: u64) -> String {
    let wanted = unowned_text(wanted);
    let items = if count == 1 { "1 item".to_string() } else { format!("{count} items") };
    format!("this list has {items}, and a {wanted} always has {length}")
}

/// A type as it is written, without the `owned` in front.
fn unowned_text(ty: &Ty) -> String {
    match ty {
        Ty::Owned(inner) | Ty::Borrowed(inner) => unowned_text(inner),
        Ty::Optional(inner) => format!("{} or none", unowned_text(inner)),
        other => other.to_string(),
    }
}

/// What in `decl` could change how many items its parameter `param` has,
/// reading the functions it hands the list on to as well. `seen` holds
/// the ones being read already: a function that calls itself is taken
/// to keep the length there, and anything that changes it is found
/// where it is written.
fn length_changed(decl: &Arc<ast::Function>, env: &Env, param: &str, seen: &mut HashSet<(usize, String)>) -> Option<String> {
    if !seen.insert((Arc::as_ptr(decl) as usize, param.to_string())) {
        return None;
    }
    lengths::changes_length(&decl.body, param, &mut |callee, args, index| {
        let found = match &callee.kind {
            ExprKind::Name(name) => env.get(name).map(|variable| variable.ty),
            // `files.write_bytes(path, data)`: a module's functions are
            // built in, and only look.
            ExprKind::Field { object, .. } => match &object.kind {
                ExprKind::Name(name) => match env.get(name).map(|variable| variable.ty) {
                    Some(Ty::Module(_)) => return lengths::Callee::Looks,
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        let (called, called_env) = match found {
            Some(Ty::Builtin(_)) => return lengths::Callee::Looks,
            Some(Ty::Function(info)) if info.self_type.is_none() => (info.decl.clone(), info.env.clone()),
            Some(Ty::Generic(generic)) => match &generic.template {
                Template::Function(function) => (function.clone(), generic.env.clone()),
                Template::Class(_) | Template::Enum(_) => return lengths::Callee::Unknown,
            },
            _ => return lengths::Callee::Unknown,
        };
        // Which parameter the list arrives at.
        let at = match &args[index].name {
            Some(name) => called.params.iter().position(|param| param.name == *name),
            None => Some(args[..index].iter().filter(|arg| arg.name.is_none()).count()),
        };
        let Some(param) = at.and_then(|at| called.params.get(at)) else {
            return lengths::Callee::Unknown;
        };
        lengths::Callee::Function(length_changed(&called, &called_env, &param.name, seen))
    })
}

/// The length of a list of a fixed length, through `owned` or a look.
fn fixed_length(ty: &Ty) -> Option<u64> {
    match ty {
        Ty::Fixed(_, length) => Some(*length),
        Ty::Owned(inner) | Ty::Borrowed(inner) => fixed_length(inner),
        _ => None,
    }
}

/// What a call calls: the `f` of `f(x)`.
fn args_callee(call: &Expr) -> &Box<Expr> {
    match &call.kind {
        ExprKind::Call { callee, .. } => callee,
        _ => unreachable!("only asked of a call"),
    }
}

/// The variable an expression reaches into: `a` for `a`, for `a.items`
/// and for `a.rows[2].cells`.
fn root(expr: &Expr) -> Option<&String> {
    match &expr.kind {
        ExprKind::Name(name) | ExprKind::Give(name) => Some(name),
        ExprKind::Field { object, .. } | ExprKind::Index { object, .. } => root(object),
        _ => None,
    }
}

/// The error for a `var` of one function used by a function written
/// inside it, where everything has one owner.
fn shared(name: &str) -> String {
    format!(
        "`{name}` is a `var` of the function around this one, so the two functions would share it, \
         and with no collector nothing is shared. If it does not change, declare it with `let`. \
         If it does, keep it in an object, which a function written here can look at and change"
    )
}

/// The error for an owned variable used from another function's body.
/// `top` says it is a `var` at the top of the file: a `let` there could
/// be reached.
fn elsewhere(name: &str, top: bool) -> String {
    let instead = match top {
        true => ". Or declare it with `let`, if it is never given a new value: every function can look at a `let` \
                 at the top of the file",
        false => "",
    };
    format!(
        "`{name}` holds an owned value, which belongs to the block that declares it. \
         A function written elsewhere cannot reach it. Pass it to the function: a parameter looks at what it is given{instead}"
    )
}

/// What the checker keeps about a class, before its fields are worked out.
fn class_info(
    decl: &ast::Class,
    one_owner: bool,
    env: &Env,
    above: &Rc<[String]>,
    span: Span,
    origin: Option<(Rc<Generic>, Vec<Ty>)>,
    copy_note: Option<String>,
) -> Rc<ClassInfo> {
    let defaults = decl
        .fields
        .iter()
        .filter_map(|field| {
            let mut names = Vec::new();
            names_in(field.default.as_ref()?, &mut names);
            Some((field.name.clone(), names))
        })
        .collect();
    Rc::new(ClassInfo {
        id: decl_id(span, &decl.name),
        name: decl.name.clone(),
        owned: decl.owned || one_owner,
        fields: RefCell::new(Vec::new()),
        methods: RefCell::new(HashMap::new()),
        private: decl.private.clone(),
        file: span.file(),
        interface: decl.interface,
        is: RefCell::new(Vec::new()),
        above: above.clone(),
        env: env.clone(),
        defaults,
        origin,
        copy_note,
    })
}

fn decl_id(span: Span, name: &str) -> DeclId {
    (span.line, span.col, name.to_string())
}

/// A loop body runs more than once, so what was known about a variable on
/// the way into the loop is not known on the second trip if the body gives
/// the variable a new value. Start the body knowing nothing about those.
fn forget_assigned(body: &[Stmt], scope: &Env) {
    for statement in body {
        match &statement.kind {
            StmtKind::Assign { target: Expr { kind: ExprKind::Name(name), .. }, .. } => scope.forget(name),
            StmtKind::Receive { places, .. } => places.iter().filter_map(Place::assigned).for_each(|name| scope.forget(name)),
            StmtKind::If { then_block, else_block, .. } => {
                forget_assigned(then_block, scope);
                forget_assigned(else_block.as_deref().unwrap_or_default(), scope);
            }
            StmtKind::While { body, .. } | StmtKind::For { body, .. } | StmtKind::Unsafe(body) | StmtKind::Scoped { body, .. } => {
                forget_assigned(body, scope)
            }
            StmtKind::Match { arms, .. } => arms.iter().for_each(|arm| forget_assigned(&arm.body, scope)),
            StmtKind::Try { body, handlers } => {
                forget_assigned(body, scope);
                handlers.iter().for_each(|handler| forget_assigned(&handler.body, scope));
            }
            _ => {}
        }
    }
}

/// Carry out to `env` what the scope of a branch still knows, at its end,
/// about the variables its condition narrowed.
fn keep_known(env: &Env, branch: &Env, narrowings: &[(String, Ty)]) {
    for (name, _) in narrowings {
        if let Some(Variable { ty, declared: Some(_), .. }) = branch.get(name) {
            env.narrow(name, ty);
        }
    }
}

fn narrow_all(scope: &Env, narrowings: &[(String, Ty)]) {
    for (name, ty) in narrowings {
        scope.narrow(name, ty.clone());
    }
}

/// Does this loop body contain a `break` that belongs to it? Breaks inside
/// an inner loop belong to that loop instead.
fn has_break(statements: &[Stmt]) -> bool {
    statements.iter().any(|statement| match &statement.kind {
        StmtKind::Break => true,
        StmtKind::If { then_block, else_block, .. } => {
            has_break(then_block) || else_block.as_deref().is_some_and(has_break)
        }
        StmtKind::Match { arms, .. } => arms.iter().any(|arm| has_break(&arm.body)),
        StmtKind::Unsafe(body) | StmtKind::Scoped { body, .. } => has_break(body),
        StmtKind::Try { body, handlers } => {
            has_break(body) || handlers.iter().any(|handler| has_break(&handler.body))
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use crate::{lexer, parser};

    fn check(source: &str, strict: bool) -> Vec<String> {
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
        super::check(&program, strict).into_iter().map(|error| error.message).collect()
    }

    /// The messages the checker gives for a program.
    fn errors(source: &str) -> Vec<String> {
        check(source, false)
    }

    /// The single message expected from a program with one mistake.
    fn error(source: &str) -> String {
        let mut found = errors(source);
        assert_eq!(found.len(), 1, "expected exactly one error, got {found:?}");
        found.remove(0)
    }

    fn strict_error(source: &str) -> String {
        let mut found = check(source, true);
        assert_eq!(found.len(), 1, "expected exactly one error, got {found:?}");
        found.remove(0)
    }

    fn ok(source: &str) {
        assert_eq!(errors(source), Vec::<String>::new(), "\n{source}");
    }

    #[test]
    fn a_sized_number_takes_an_operator_and_an_equals() {
        // `count += 1` on a `u8` is `count = u8(count + 1)`, for every
        // operator that gives a whole number, wherever the `u8` is kept.
        strict_ok(
            "class Reg {\n    value: u8\n}\n\
             var a: u8 = 1\nvar b: i16 = -1\nlet r = Reg(1)\nlet xs: list[u8] = [1, 2]\nlet i = 0\n\
             a += 1\na -= 1\na *= 2\na //= 2\na %= 7\na **= 2\na &= 0x0F\na |= 0x80\na ^= 1\na <<= 1\na >>= 1\n\
             b -= a\nr.value += a\nxs[i] += 1\nxs[i + 1] |= r.value\n\
             function bump(n: u8) -> u8 {\n    var m = n\n    m += 1\n    return m\n}\nprint(a, b, r, xs, bump(a))",
        );
        // `/` gives a float, on a sized number as on an int.
        assert_eq!(error("var a: u8 = 1\na /= 2"), "the new value should be u8, but this is float");
        assert_eq!(error("var a = 1\na /= 2"), "the new value should be int, but this is float");
        assert_eq!(error("var a: u8 = 1\na += 1.5"), "the new value should be u8, but this is float");

        // The program that is run has the long form in it, and the
        // statements that store into an int are as they were written.
        let source = "var count: u8 = 250\nvar total = 0\nfunction more() {\n    count += 5\n    total += 5\n}\nmore()\n";
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let (program, errors, _) = super::checked(program, super::Rules { strict: true, ..super::Rules::default() });
        assert!(errors.is_empty(), "{:?}", errors[0].message);
        let printed = crate::printer::print(&program);
        assert!(printed.contains("count = u8((count + 5))") && printed.contains("total += 5"), "{printed}");
        // A program the checker refuses comes back as it was.
        let wrong = parser::parse(lexer::lex("var count: u8 = 250\ncount += 5\nprint(nothing)").unwrap()).unwrap();
        let (back, errors, _) = super::checked(wrong.clone(), super::Rules::default());
        assert_eq!((back, errors.len()), (wrong, 1));
    }

    #[test]
    fn the_place_of_a_compound_assignment_has_no_call_in_it() {
        let twice = |op: &str| {
            format!(
                "`{op}` reads this place and then writes it, so a call in the place would run twice. \
                 Keep what the call gives in a `let` first, and use that name here"
            )
        };
        let setup = "class Box {\n    n: int\n}\nfunction pick() -> Box {\n    return Box(1)\n}\nfunction next() -> int {\n    return 1\n}\n\
                     let xs = [1, 2, 3]\nlet counts: dict[str, int] = [\"a\": 1]\n";
        assert_eq!(error(&format!("{setup}xs[next()] += 5")), twice("+="));
        assert_eq!(error(&format!("{setup}xs[len(xs) - 1] *= 2")), twice("*="));
        assert_eq!(error(&format!("{setup}counts[\"A\".lower()] -= 1")), twice("-="));
        assert_eq!(error(&format!("{setup}pick().n //= 2")), twice("//="));
        // A call in the value is what a value is for, and so is a
        // call in the place of a plain `=`, which reads nothing.
        ok(&format!("{setup}let last = len(xs) - 1\nxs[last] += next()\nxs[next()] = 5\npick().n = 2\ncounts[\"a\"] += len(xs)"));
    }

    fn strict_ok(source: &str) {
        assert_eq!(check(source, true), Vec::<String>::new(), "\n{source}");
    }

    const SHAPES: &str = "interface Shape {\n    function area(self) -> float\n    function scale(self, by: float)\n}\n\
        class Circle is Shape {\n    radius: float\n\n    function area(self) -> float {\n        return 3.0 * self.radius * self.radius\n    }\n\n    \
        function scale(self, by: float) {\n        self.radius = self.radius * by\n    }\n}\n\
        class Square is Shape {\n    side: float\n\n    function area(self) -> float {\n        return self.side * self.side\n    }\n\n    \
        function scale(self, by: float) {\n        self.side = self.side * by\n    }\n}\n\
        class Other {\n    x: int\n}\n";

    #[test]
    fn a_class_keeps_what_its_interface_asks() {
        let shape = "interface Shape {\n    function area(self) -> float\n    function scale(self, by: float)\n}\n";
        let area = "    function area(self) -> float {\n        return 0.0\n    }\n";
        let scale = "    function scale(self, by: float) {\n    }\n";
        let dot = |body: &str| format!("{shape}class Dot is Shape {{\n{body}}}\n");
        // Every function of the interface, written the same way.
        strict_ok(&dot(&format!("{area}{scale}")));
        // More than the interface asks is the class's own business.
        strict_ok(&dot(&format!("    size: int = 1\n{area}{scale}    function grow(self) {{\n        self.size += 1\n    }}\n")));
        assert_eq!(
            error(&dot(area)),
            "`Dot` says it is a `Shape`, and a `Shape` has `function scale(self, by: float)`. Give `Dot` that function"
        );
        // The names of the parameters count: `shape.scale(by=2.0)` has
        // to mean the same for every class.
        assert_eq!(
            errors(&dot("    function area(self) -> int {\n        return 0\n    }\n    function scale(self, factor: float) {\n    }\n")),
            vec![
                "`area` of `Dot` is `function area(self) -> int`, and `Shape` asks for `function area(self) -> float`. \
                 Write the two the same",
                "`scale` of `Dot` is `function scale(self, factor: float)`, and `Shape` asks for \
                 `function scale(self, by: float)`. Write the two the same",
            ]
        );
        assert_eq!(
            error(&dot(&format!("{area}    private function scale(self, by: float) {{\n    }}\n"))),
            "`scale` of `Dot` is private, and `Shape` lets every file call it. Take the word `private` off it"
        );
        // A class may raise less than its interface says, never more:
        // whoever calls through the interface knows only what it says.
        let port = |says: &str, does: &str| {
            format!(
                "interface Port {{\n    function send(self, text: str) -> int{says}\n}}\n\
                 class Uart is Port {{\n    function send(self, text: str) -> int{does} {{\n        return int(text)\n    }}\n}}\n\
                 class Quiet is Port {{\n    function send(self, text: str) -> int {{\n        return 0\n    }}\n}}\n"
            )
        };
        let unsaid = "`send` of `Uart` can raise ValueError, and `send` of `Port` does not say so. Whoever calls \
                      it through a `Port` would not know. Add `raises ValueError` to the function in `Port`, \
                      or catch the error in `Uart`";
        strict_ok(&port(" raises ValueError", " raises ValueError"));
        strict_ok(&port(" raises Error", " raises ValueError"));
        ok(&port(" raises ValueError", ""));
        assert_eq!(error(&port("", " raises ValueError")), unsaid);
        assert_eq!(error(&port("", "")), unsaid);
        // A call through the interface can raise what the interface
        // says, whatever the classes do.
        assert_eq!(
            strict_error(&format!(
                "{}function try_it(port: Port) -> int {{\n    return port.send(\"1\")\n}}",
                port(" raises ValueError", " raises ValueError")
            )),
            "`try_it` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        // What comes after `is` is an interface that exists.
        assert_eq!(
            error("class A {\n}\nclass B is A {\n}"),
            "`A` is a class, not an interface, so `B` cannot be one. A class has no parent class in Zeno. \
             Declare what the two share as an `interface`, and let both be it"
        );
        assert_eq!(error("class C is Nothing {\n}"), "`Nothing` is not declared. Declare it with `interface Nothing { ... }`");
        // An `owned class` is an `owned interface`, and only it is (58.22).
        let owned_shape = format!("owned {shape}");
        assert_eq!(
            error(&format!("{shape}owned class Dot is Shape {{\n{area}{scale}}}\n")),
            "`Dot` is an owned class, and `Shape` is not an `owned interface`: an object with one owner cannot be \
             where a shared one is wanted. Write `owned interface Shape`, or take `owned` off `Dot`"
        );
        assert_eq!(
            error(&format!("{owned_shape}class Dot is Shape {{\n{area}{scale}}}\n")),
            "`Dot` is not an owned class, and `Shape` is an `owned interface`, whose objects each have one owner. \
             Write `owned class Dot`, or take `owned` off `Shape`"
        );
        ok(&format!("{owned_shape}owned class Dot is Shape {{\n{area}{scale}}}\n"));
        // With no collector everything is owned, said or not, and in a
        // file that says `memory owned` every class and interface is.
        one_owner_ok(&format!("{shape}owned class Dot is Shape {{\n{area}{scale}}}\n"));
        one_owner_ok(&format!("{owned_shape}class Dot is Shape {{\n{area}{scale}}}\n"));
        ok(&format!("memory owned\n{shape}class Dot is Shape {{\n{area}{scale}}}\nlet d: Shape = Dot()\nprint(d.area())\n"));
        // An interface is nothing that can be built.
        assert_eq!(
            error(&format!("{shape}let s = Shape()")),
            "`Shape` is an interface, so there is nothing to build. Build an object of a class that is a `Shape`"
        );
    }

    #[test]
    fn a_default_in_an_interface_is_a_plain_value_that_each_class_writes() {
        // Every kind of plain value, written the same in another
        // spelling: `1_000` and `1000`, `Fast` and `Mode.Fast` (58.20).
        let motor = |defaults: &str, written: &str| {
            format!(
                "enum Mode {{\n    Slow\n    Fast\n}}\nenum Command {{\n    Stop\n    Go(speed: int)\n}}\n\
                 interface Motor {{\n    function spin(self{defaults})\n}}\n\
                 class Fan is Motor {{\n    function spin(self{written}) {{\n    }}\n}}\n"
            )
        };
        strict_ok(&motor(
            ", speed: int = 1_000, wait: ms = 5 ms, mode: Mode = Fast, label: str = \"m\", back: bool = false, \
             trim: float = -0.5, low: ms = -2 ms, name: str or none = none, how: Command = Stop",
            ", speed: int = 1000, wait: ms = 5 ms, mode: Mode = Mode.Fast, label: str = \"m\", back: bool = false, \
             trim: float = -0.5, low: ms = -2 ms, name: str or none = none, how: Command = Command.Stop",
        ));
        // A call through the interface may leave out what has a
        // default there, and is filled in as a call on the object is.
        strict_ok(&format!("{}function go(m: Motor) {{\n    m.spin()\n    m.spin(2)\n}}\ngo(Fan())\nFan().spin()", motor(", speed: int = 1", ", speed: int = 1")));
        // Where the interface writes none, the class may write its own,
        // as before. A call through the interface gives every argument.
        let own = motor(", speed: int", ", speed: int = 4");
        strict_ok(&format!("{own}Fan().spin()\nfunction go(m: Motor) {{\n    m.spin(2)\n}}\ngo(Fan())"));
        assert_eq!(error(&format!("{own}function go(m: Motor) {{\n    m.spin()\n}}")), "`spin` needs a value for `speed`");
        // A default that is not the same value, or none where the
        // interface has one, shows both.
        assert_eq!(
            error(&motor(", speed: int = 1", ", speed: int = 2")),
            "`spin` of `Fan` gives `speed` the default `2`, on line 13, and `Motor` gives it `1`, on line 10. \
             Write the same default in both, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        assert_eq!(
            error(&motor(", mode: Mode = Fast", ", mode: Mode = Slow")),
            "`spin` of `Fan` gives `mode` the default `Slow`, on line 13, and `Motor` gives it `Fast`, on line 10. \
             Write the same default in both, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        assert_eq!(
            error(&motor(", wait: ms = 5 ms", ", wait: ms = 6 ms")),
            "`spin` of `Fan` gives `wait` the default `6 ms`, on line 13, and `Motor` gives it `5 ms`, on line 10. \
             Write the same default in both, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        assert_eq!(
            error(&motor(", trim: float = -0.5", ", trim: float = 0.5")),
            "`spin` of `Fan` gives `trim` the default `0.5`, on line 13, and `Motor` gives it `-0.5`, on line 10. \
             Write the same default in both, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        // What the class writes is a plain value too, to be the same.
        assert_eq!(
            error(&motor(", speed: int = 6", ", speed: int = 2 * 3")),
            "`spin` of `Fan` gives `speed` the default `2 * 3`, on line 13, and `Motor` gives it `6`, on line 10. \
             Write the same default in both, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        assert_eq!(
            error(&motor(", label: str = \"m\"", ", label: str")),
            "`Motor` gives `label` of `spin` the default `\"m\"`, on line 10, and `spin` of `Fan` gives it none. \
             Write the same default in `Fan`, so that a call through a `Motor` and a call on a `Fan` mean the same"
        );
        // In the interface it is a plain value, of the parameter's type.
        let plain = |shown: &str| {
            format!(
                "the default of `speed` in `spin` is `{shown}`. A default in an interface is a plain value: a number, \
                 text, `true`, `false`, `none`, a quantity such as `5 ms`, or a case of an enum that carries nothing. \
                 Each class that is the interface writes the same one"
            )
        };
        assert_eq!(error(&motor(", speed: int = 2 * 3", ", speed: int = 6")), plain("2 * 3"));
        assert_eq!(error(&motor(", speed: int = len(\"ab\")", ", speed: int = 2")), plain("len(\"ab\")"));
        assert_eq!(error(&motor(", speed: list[int] = [1]", ", speed: list[int] = [1]")), plain("[1]"));
        assert_eq!(error(&motor(", speed: str = f\"{1}\"", ", speed: str = \"1\"")), plain("f\"{1}\""));
        assert_eq!(error(&motor(", speed: Command = Go", ", speed: Command = Stop")), plain("Go"));
        let wrong = "`speed` should be int, but this is str";
        assert_eq!(errors(&motor(", speed: int = \"1\"", ", speed: int = \"1\"")), vec![wrong, wrong]);
    }

    #[test]
    fn a_class_goes_where_its_interface_is_wanted() {
        let ok = |source: &str| strict_ok(&format!("{SHAPES}{source}"));
        let error = |source: &str| error(&format!("{SHAPES}{source}"));
        // A variable, a parameter, a result, a field, and each with `or none`.
        ok("let one: Shape = Circle(1.0)\none.scale(by=2.0)\nprint(one.area(), one)");
        ok("function double(shape: Shape) {\n    shape.scale(2.0)\n}\ndouble(Square(1.0))\nlet c = Circle(1.0)\ndouble(c)");
        ok("function pick(round: bool) -> Shape {\n    if round {\n        return Circle(1.0)\n    }\n    return Square(1.0)\n}\nprint(pick(true))");
        ok("class Frame {\n    inside: Shape\n    spare: Shape or none = none\n}\nlet f = Frame(Circle(1.0), Square(2.0))\nf.inside = Square(3.0)\nprint(f)");
        ok("var maybe: Shape or none = none\nmaybe = Circle(1.0)\nlet c: Circle or none = Circle(2.0)\nmaybe = c\n\
            if maybe is not none {\n    print(maybe.area())\n}");
        // Two of them are equal when they are the same class and that
        // class says they are.
        ok("let a: Shape = Circle(1.0)\nlet b: Shape = Square(1.0)\nprint(a == b, a != b)");
        // A list that is written out is a list of the interface where
        // one is wanted: in a `let`, an argument, a result, a field, an
        // assignment, an `append`.
        ok("let shapes: list[Shape] = [Circle(1.0), Square(2.0)]\nfor shape in shapes {\n    print(shape.area())\n}");
        ok("function total(shapes: list[Shape]) -> int {\n    return len(shapes)\n}\nprint(total([Circle(1.0), Square(2.0)]))");
        ok("function both() -> list[Shape] {\n    return [Circle(1.0), Square(2.0)]\n}\nprint(both())");
        ok("class Drawing {\n    shapes: list[Shape]\n}\nlet d = Drawing([Circle(1.0)])\nd.shapes = [Square(1.0), Circle(2.0)]\nd.shapes.append(Circle(3.0))");
        ok("var rows: list[list[Shape]] = []\nrows.append([Circle(1.0), Square(1.0)])\nrows.insert(0, [Square(2.0)])");
        assert_eq!(
            error("let shapes: list[Shape] = [Circle(1.0), Other(1)]"),
            "`shapes` should be list[Shape], but this item is Other"
        );
        assert_eq!(
            error("function total(shapes: list[Shape]) -> int {\n    return len(shapes)\n}\nprint(total([Circle(1.0), 5]))"),
            "this is a list of Shape here, but this item is int"
        );
        // A list of circles is not a list of shapes: a square could be
        // appended to it by whoever holds it as shapes.
        assert_eq!(
            error("let circles = [Circle(1.0)]\nlet shapes: list[Shape] = circles"),
            "`shapes` should be list[Shape], but this is list[Circle]. Whoever holds it as a list of Shape could \
             append another class to it. Say what the list holds where it is made, as in `let items: list[Shape] = [...]`"
        );
        assert_eq!(
            error("let circles = [Circle(1.0)]\nlet others: list[Other] = circles"),
            "`others` should be list[Other], but this is list[Circle]"
        );
        // There is no way back, and nothing but the interface's functions.
        assert_eq!(
            error("let s: Shape = Circle(1.0)\nlet c: Circle = s"),
            "`c` should be Circle, but this is Shape. A Shape can be an object of any class that is one. \
             Keep the object in a variable of its own class as well, or give Shape a function for what is needed"
        );
        assert_eq!(error("let s: Shape = Circle(1.0)\nlet o: Other = s"), "`o` should be Other, but this is Shape");
        assert_eq!(error("let s: Shape = Other(1)"), "`s` should be Shape, but this is Other");
        assert_eq!(
            error("let s: Shape = Circle(1.0)\nprint(s.radius)"),
            "`radius` is not a function of the interface `Shape`, which has `area` and `scale`. What else this object \
             has depends on its class, and that is only known when the program runs. Give `Shape` a function for it"
        );
        // A generic takes the interface from the place its result goes,
        // when each argument goes straight into a `T`.
        ok("class Pair[T] {\n    first: T\n    second: T\n}\nlet pair: Pair[Shape] = Pair(Circle(1.0), Square(2.0))\nprint(pair.second.area())");
        ok("function same[T](item: T) -> T {\n    return item\n}\nlet s: Shape = same(Circle(1.0))\nlet n: int or none = same(len(\"abc\"))\nprint(s, n)");
        ok("function first[T](items: list[T]) -> T {\n    return items[0]\n}\nlet s: Shape = first([Circle(1.0)])\nlet n: int or none = first([1, 2])\nprint(s, n)");
    }

    #[test]
    fn a_private_field_or_method_is_for_the_file_of_its_class() {
        let counter = "==> counter.zeno\n\
            class Counter {\n    private count: int = 0\n    step: int = 1\n\n    \
            function bump(self) {\n        self.count = self.next()\n    }\n\n    \
            private function next(self) -> int {\n        return self.count + self.step\n    }\n}\n\n\
            function counter() -> Counter {\n    return Counter()\n}\n\n\
            class Open {\n    step: int = 1\n\n    private function inner(self) -> int {\n        return self.step\n    }\n}\n\n\
            class Stack[T] {\n    private items: list[T]\n\n    function size(self) -> int {\n        return len(self.items)\n    }\n}\n";
        let check = |main: &str| -> Vec<String> {
            let program = crate::modules::testing::program(&format!("import counter\n{main}\n{counter}"));
            super::check(&program, false).into_iter().map(|error| error.message).collect()
        };
        let private = |name: &str, class: &str| {
            vec![format!(
                "`{name}` is private in the class `{class}`: only the file that declares the class can use it. \
                 To use it from another file, take the word `private` off where it is declared"
            )]
        };
        let build = |class: &str, field: &str| {
            vec![format!(
                "`{class}` has the private field `{field}`, so only the file that declares the class can build one. \
                 Write a function in that file that builds one and returns it, and call that function here"
            )]
        };
        // What is not private is used as ever.
        assert_eq!(check("let c = counter.counter()\nc.bump()\nc.step = 2\nprint(counter.Open().step)"), Vec::<String>::new());
        // A private field cannot be read, given a value, or filled in.
        assert_eq!(check("let c = counter.counter()\nprint(c.count)"), private("count", "counter.Counter"));
        assert_eq!(check("let c = counter.counter()\nc.count = 4"), private("count", "counter.Counter"));
        assert_eq!(check("let c = counter.counter()\nc.count += 4"), private("count", "counter.Counter"));
        assert_eq!(check("let c = counter.Counter()"), build("counter.Counter", "count"));
        assert_eq!(check("let s = counter.Stack([1])"), build("counter.Stack[int]", "items"));
        // A private method cannot be called or kept. A class that has
        // one can still be built: there is nothing to fill in.
        assert_eq!(check("let o = counter.Open()\nprint(o.inner())"), private("inner", "counter.Open"));
        assert_eq!(check("let o = counter.Open()\nlet f = o.inner"), private("inner", "counter.Open"));
        // A function with no types written is checked with what it is
        // given, so it does not get round this.
        assert_eq!(
            check("function peek(c) {\n    return c.count\n}\nprint(peek(counter.counter()))"),
            private("count", "counter.Counter")
        );
        // In the file of the class nothing is hidden.
        ok("class Counter {\n    private count: int = 0\n\n    private function next(self) -> int {\n        return self.count + 1\n    }\n}\n\
            let c = Counter(3)\nc.count = c.next()\nprint(c.count)");
    }

    // ----- Written types are checked -----

    #[test]
    fn declared_types_must_match() {
        ok("let a: int = 1\nlet b: float = 1.5\nlet c: str = \"x\"\nlet d: bool = true\nlet e: list[int] = [1, 2]\nlet f: list[str] = []");
        assert_eq!(error("let x: int = \"hello\""), "`x` should be int, but this is str");
        assert_eq!(error("let xs: list[int] = [1, \"two\"]"), "`xs` should be list[int], but this item is str");
        assert_eq!(error("let xs: list[int] = [\"a\"]"), "`xs` should be list[int], but this item is str");
        assert_eq!(error("let ys = [\"a\"]\nlet xs: list[int] = ys"), "`xs` should be list[int], but this is list[str]");
        assert_eq!(
            error("let x: float = 1"),
            "`x` should be float, but this is int. Write a number with a decimal point, as in `1.0`, and convert a value with `float( )`"
        );
    }

    #[test]
    fn unknown_type_names() {
        assert_eq!(error("let x: Strng = \"a\""), "there is no type called `Strng`");
        assert_eq!(error("let x: list = []"), "`list` takes one type, as in `list[int]`");
        assert_eq!(error("let x: int[str] = 1"), "`int` does not take types in `[ ]`");
    }

    // ----- Types that are left out are worked out -----

    #[test]
    fn types_are_inferred_from_values() {
        assert_eq!(error("let n = 3\nlet s: str = n"), "`s` should be str, but this is int");
        assert_eq!(error("let xs = [1, 2]\nlet s: str = xs[0]"), "`s` should be str, but this is int");
        assert_eq!(error("let xs = [n * 2 for n in range(3)]\nlet s: str = xs"), "`s` should be str, but this is list[int]");
        assert_eq!(error("let q = 500 ms\nlet s: str = q"), "`s` should be str, but this is a quantity in ms");
        assert_eq!(error("let x = 7 / 2\nlet n: int = x"), "`n` should be int, but this is float");
        ok("let n: int = 7 // 2\nlet m: int = 7 % 2\nlet p: int = 2 ** 8\nlet f: float = 1 + 0.5");
    }

    #[test]
    fn return_types_are_inferred() {
        assert_eq!(error("function double(n: int) { return n * 2 }\nlet s: str = double(2)"), "`s` should be str, but this is int");
        assert_eq!(
            error("function find(n: int) {\n if n > 0 { return \"yes\" }\n}\nlet s: str = find(1)"),
            "`s` should be str, but this is str or none. Check it with `is not none` first"
        );
        ok("function nothing() { }\nlet x = nothing()");
        // A function used above where it is written still has its type worked out.
        assert_eq!(error("let s: str = later()\nfunction later() { return 1 }"), "`s` should be str, but this is int");
    }

    #[test]
    fn untyped_code_is_left_alone() {
        ok("function add(a, b) { return a + b }\nprint(add(1, 2), add(\"a\", \"b\"))\nlet x: int = add(1, 2)");
        ok("class Box {\n item\n}\nlet b = Box(1)\nb.item = \"now a str\"\nprint(b.item.upper())");
        ok("let x: any = 1\nprint(x.whatever)");
    }

    // ----- Names -----

    #[test]
    fn names_must_exist() {
        assert_eq!(error("print(missing)"), "`missing` is not defined");
        assert_eq!(error("x = 1"), "`x` is not declared. Create it with `let` or `var` first");
        assert_eq!(error("let x = 1\nlet x = 2"), "`x` is already declared in this block");
        assert_eq!(error("if true {\n let inner = 1\n}\nprint(inner)"), "`inner` is not defined");
        // One mistake, one message: the bad name does not set off more errors.
        assert_eq!(error("let y = missing + 1\nprint(y.upper())"), "`missing` is not defined");
    }

    #[test]
    fn let_cannot_be_reassigned() {
        ok("var x = 1\nx = 2\nx += 3");
        assert_eq!(error("let x = 1\nx = 2"), "`x` cannot be changed. Declare it with `var` to allow that");
        assert_eq!(error("function f(n: int) { n = 2 }"), "`n` cannot be changed. Declare it with `var` to allow that");
        assert_eq!(error("for i in range(3) { i = 0 }"), "`i` cannot be changed. Declare it with `var` to allow that");
        assert_eq!(error("var x = 1\nx = \"one\""), "the new value should be int, but this is str");
        assert_eq!(error("var x = 1\nx += 0.5"), "the new value should be int, but this is float");
    }

    // ----- Operators -----

    #[test]
    fn operators_need_matching_types() {
        assert_eq!(error("print(\"abc\" - 1)"), "`-` does not work between str and int");
        assert_eq!(error("print(1 + \"1\")"), "`+` does not work between int and str");
        assert_eq!(error("print(500 ms + 3 V)"), "`+` does not work between a quantity in ms and a quantity in V");
        assert_eq!(error("print(1 < \"2\")"), "`<` does not work between int and str");
        assert_eq!(error("print(-\"x\")"), "`-` does not work on str");
        assert_eq!(error("print(not 1)"), "`not` needs a bool, but this is int");
        assert_eq!(error("print(1 and true)"), "`and` needs bools, but this is int");
        assert_eq!(error("print(1 in 5)"), "`in` does not work between int and int");
        ok("print(1 + 2.5, \"a\" + \"b\", [1] + [2], 2 * 500 ms, 1 < 2 < 3, \"a\" in \"abc\", 1 in [1, 2])");
    }

    #[test]
    fn comparisons_that_can_never_be_true() {
        assert_eq!(error("print(1 == \"1\")"), "`==` between int and str is always false");
        assert_eq!(error("print([1] != \"x\")"), "`!=` between list[int] and str is always true");
        ok("print(1 == 1.0)\nlet x: int or none = 1\nprint(x == 1, x is none)");
    }

    #[test]
    fn none_is_tested_for_with_is() {
        let maybe = "let x: int or none = 1\n";
        assert_eq!(error(&format!("{maybe}print(x == none)")), "`none` is tested for with `is`: write `is none`, not `== none`");
        assert_eq!(error(&format!("{maybe}print(none != x)")), "`none` is tested for with `is`: write `is not none`, not `!= none`");
        // Two values that may each be `none` are compared as values.
        ok(&format!("{maybe}let y: int or none = none\nprint(x == y, x != y)"));
        // A test whose answer is known is a mistake.
        assert_eq!(
            error("let n = 3\nprint(n is none)"),
            "`is none` is always false: this is int, which is never `none`. Only a value whose type says `or none` can be"
        );
        assert_eq!(
            error("let names = [\"a\"]\nif names is not none { }"),
            "`is not none` is always true: this is list[str], which is never `none`. Only a value whose type says `or none` can be"
        );
        // After a test or a new value the name is known to be there, and
        // testing it again is not a mistake. Nor is it for a parameter
        // with no type written that is given a `none` somewhere.
        ok(&format!("{maybe}if x is not none {{\n if x is none {{ }}\n}}"));
        ok("var x: int or none = none\nx = 3\nprint(x is none)");
        ok("function f(x) {\n return x is none\n}\nprint(f(1), f(none))");
    }

    #[test]
    fn one_mistake_in_a_call_gets_one_message() {
        // Before, this said three things: the order, `a` twice, no `b`.
        let f = "function f(a: int, b: int = 2) -> int {\n return a + b\n}\n";
        assert_eq!(errors(&format!("{f}print(f(b=5, 1))")), vec!["plain arguments must come before named ones".to_string()]);
        ok(&format!("{f}print(f(1), f(1, b=5), f(a=1, b=5))"));
    }

    #[test]
    fn a_const_is_written_from_what_the_compiler_can_work_out() {
        ok("import math\nenum Mode {\n Idle\n Run\n}\nconst A = 4\nconst B: u8 = 0xA5\nconst C = A * 2 + 1\nconst TAU = 2 * math.pi\n\
            const T = [A, C, 10]\nconst W = 500 ms\nconst S = f\"v{A}\"\nconst U = u8(A + 1)\nconst X = u8.wrap(300)\n\
            const M = Idle\nconst N = Mode.Run\nconst F = float(A)\nprint(A, B, C, TAU, T, W, S, U, X, M, N, F, T[1])");
        let not = "this is only known when the program runs, and a `const` is fixed when the program is built. \
                   A `const` is written from numbers, text, other `const`s and operators. \
                   For a value that is worked out once while the program runs, use `let`";
        assert_eq!(error("const N = len(\"abc\")"), not);
        assert_eq!(error("var k = 3\nconst P = k + 1"), not);
        assert_eq!(error("let k = 3\nconst P = [1, k]"), not);
        assert_eq!(error("function f() -> int {\n return 1\n}\nconst P = f()"), not);
        // Inside a function too.
        ok("function f() -> int {\n const Q = 2\n const R = Q * 2\n return R\n}\nprint(f())");
        assert_eq!(error("function f(z: int) -> int {\n const BAD = z\n return BAD\n}"), not);
    }

    #[test]
    fn a_field_of_a_case_is_taken_out_in_a_match() {
        let shape = "enum Shape {\n Dot\n Circle(radius: float)\n Rect(w: float, h: float)\n}\n";
        assert_eq!(
            error(&format!("{shape}let c = Circle(2.0)\nprint(c.radius)")),
            "`radius` belongs to the case `Circle`, and which case a `Shape` is, is only known inside a `match`. \
             Take it out there: `match ... { case Circle(radius) { ... } }`"
        );
        assert_eq!(
            error(&format!("{shape}let r = Rect(1.0, 2.0)\nprint(r.h)")),
            "`h` belongs to the case `Rect`, and which case a `Shape` is, is only known inside a `match`. \
             Take it out there: `match ... { case Rect(_, h) { ... } }`"
        );
        assert_eq!(error(&format!("{shape}print(Dot.side)")), "no case of `Shape` has a field `side`");
        ok(&format!("{shape}match Circle(2.0) {{\n case Circle(radius) {{ print(radius) }}\n case Rect(_, h) {{ print(h) }}\n case Dot {{ }}\n}}"));
    }

    #[test]
    fn a_field_that_may_be_none_says_to_copy_it() {
        // A field is never known to be there, so "check it first" would
        // be advice the program has already taken.
        let copy = "`name` is a field, and a field can be given a new value between a test and its use, \
                    so a test of it is not remembered. Copy it into a variable with `let`, and test that for `none`";
        let user = "class User {\n    name: str or none\n}\nfunction shout(text: str) -> str {\n    return text\n}\nlet user = User(none)\n";
        let inside = |line: &str| format!("{user}if user.name is not none {{\n    {line}\n}}\n");
        assert_eq!(error(&inside("print(user.name + \"!\")")), format!("`+` does not work between str or none and str. {copy}"));
        assert_eq!(error(&inside("print(\"!\" + user.name)")), format!("`+` does not work between str and str or none. {copy}"));
        assert_eq!(error(&inside("print(shout(user.name))")), format!("`text` of `shout` should be str, but this is str or none. {copy}"));
        assert_eq!(error(&inside("print(user.name.upper())")), format!("this is str or none. {copy}"));
        assert_eq!(error(&inside("let known: str = user.name")), format!("`known` should be str, but this is str or none. {copy}"));
        // Copied into a variable, the test is remembered.
        assert!(errors(&format!("{user}let name = user.name\nif name is not none {{\n    print(shout(name))\n}}\n")).is_empty());
        // A variable still gets the advice for a variable.
        assert_eq!(
            error(&format!("{user}let name = user.name\nprint(shout(name))\n")),
            "`text` of `shout` should be str, but this is str or none. Check it with `is not none` first"
        );
    }

    #[test]
    fn a_case_has_one_enum() {
        let shape = "enum Shape {\n    Dot\n    Square(side: int)\n}\n";
        assert_eq!(
            error(&format!("{shape}enum Mark {{\n    Dot\n    Dash\n}}\n")),
            "`Dot` is already a case of the enum `Shape`. A case is written by its name alone, \
             so two enums in one file cannot both have a `Dot`. Give one of them another name"
        );
        assert_eq!(
            error(&format!("{shape}enum Mark {{\n    Square\n    Dash\n}}\n")),
            "`Square` is already a case of the enum `Shape`. A case is written by its name alone, \
             so two enums in one file cannot both have a `Square`. Give one of them another name"
        );
        assert_eq!(
            error("enum Shape {\n    Dot\n    Dot\n}\n"),
            "the enum `Shape` has two cases called `Dot`. Give one of them another name"
        );
        // A name that is something else keeps the message for that.
        assert_eq!(error(&format!("let Dash = 1\n{shape}enum Mark {{\n    Dash\n}}\n")), "`Dash` is already declared in this block");
    }

    #[test]
    fn a_mistake_in_the_first_line_of_a_method_is_shown_there() {
        let program = parser::parse(
            lexer::lex("class Counter {\n    count: int\n\n    function bump() {\n        print(1)\n    }\n}\n").expect("lexing failed"),
        )
        .expect("parsing failed");
        let found = super::check(&program, false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message, "the method `bump` needs `self` as its first parameter");
        assert_eq!((found[0].span.line, found[0].span.col), (4, 5));
    }

    #[test]
    fn a_mistake_from_another_language_says_what_zeno_writes() {
        assert_eq!(error("let a = None"), "`None` is not defined. Zeno spells it `none`");
        assert_eq!(error("let a = null"), "`null` is not defined. Zeno spells it `none`");
        assert_eq!(error("print(nil)"), "`nil` is not defined. Zeno spells it `none`");
        assert_eq!(error("let a = True"), "`True` is not defined. Zeno spells it `true`");
        assert_eq!(error("let a = False"), "`False` is not defined. Zeno spells it `false`");
        // A program may still use such a name for something of its own.
        ok("let nil = 0\nprint(nil)");
        // `min` and `max` take quantities in one unit (17.3).
        ok("let low: V = min(3 V, 200 V)\nlet high: ms = max([1 ms, 2 ms])\nprint(low, high)");
        assert_eq!(
            error("print(min(3 V, 200 mV))"),
            "`min` needs quantities in one unit, but these are in V and in mV. Turn one into the other first, as in `x.V`"
        );
        assert_eq!(
            error("print(max(3 V, 2))"),
            "`max` needs all plain numbers or all quantities, but one of these is int and one is in V"
        );
        assert_eq!(
            error("print(sum([3 V, 5 V]))"),
            "`sum` works on plain numbers, and these have a unit. Add quantities with `+`, which keeps the unit: \
             start from `var total = 0 V` and add each in a loop"
        );
    }

    #[test]
    fn conditions_must_be_bools() {
        assert_eq!(error("if 1 { }"), "a condition must be true or false, but this is int");
        assert_eq!(error("while \"yes\" { }"), "a condition must be true or false, but this is str");
        assert_eq!(error("let xs = [n for n in range(3) if n]"), "a condition must be true or false, but this is int");
    }

    // ----- Calls -----

    #[test]
    fn arguments_are_checked() {
        let f = "function f(a: int, b: str = \"x\") -> int { return a }\n";
        ok(&format!("{f}f(1)\nf(1, \"y\")\nf(a=1, b=\"y\")\nf(1, b=\"y\")"));
        assert_eq!(error(&format!("{f}f(\"one\")")), "`a` of `f` should be int, but this is str");
        assert_eq!(error(&format!("{f}f(1, 2)")), "`b` of `f` should be str, but this is int");
        assert_eq!(error(&format!("{f}f()")), "`f` needs a value for `a`");
        assert_eq!(error(&format!("{f}f(1, \"y\", 3)")), "`f` takes 2 arguments, but 3 were given");
        assert_eq!(error(&format!("{f}f(1, c=3)")), "`f` has no argument named `c`");
        assert_eq!(error(&format!("{f}f(1, a=3)")), "`f` was given `a` twice");
        assert_eq!(error(&format!("{f}f(b=\"y\", 1)")), "plain arguments must come before named ones");
        assert_eq!(error("let x = 5\nx()"), "int cannot be called");
        assert_eq!(error("function f(a: int = \"no\") { }"), "`a` should be int, but this is str");
    }

    #[test]
    fn an_inline_block_is_asked_for_twice_and_typed_from_its_own_lines() {
        let block = "import c\nlet lib = inline c {\n    #include <stdint.h>\n    static int helper(int x) { return x; }\n    \
                     uint8_t low(uint32_t x) { return (uint8_t) helper((int) x); }\n    double half(double x) { return x / 2; }\n}\n";
        // A function of the block is called as a function of Zeno is,
        // and a wrong call is found before the program runs.
        ok(&format!("{block}let a: int = lib.low(300)\nlet b: float = lib.half(3.0)\nprint(a, b, lib)"));
        strict_ok(&format!("{block}print(lib.low(300))"));
        assert_eq!(error(&format!("{block}lib.half(\"x\")")), "the 1st parameter of `half` (`double`) should be float, but this is str");
        assert_eq!(error(&format!("{block}lib.low(1, 2)")), "`low` takes 1 argument, but 2 were given");
        assert_eq!(
            error(&format!("{block}lib.helper(1)")),
            "this `inline c` block has no function `helper` that Zeno can call. It has: low, half. A function marked `static` is the block's own"
        );
        // The import is the first asking, the word `inline` the second.
        assert_eq!(
            error("let lib = inline c {\n    int one(void) { return 1; }\n}\n"),
            "an `inline c` block needs `import c` at the top of the file. The import is what asks for the other language, and a file with no such line uses none"
        );
        assert_eq!(
            error("import c\nlet lib = inline python {\n    x = 1\n}\n"),
            "an `inline python` block needs `import python` at the top of the file. The import is what asks for the other language, and a file with no such line uses none"
        );
        // A language that has no block is told what it has.
        assert!(error("import c\nlet a = inline asm {\n    nop\n}\n").starts_with("there is no `inline asm`. A block is `inline c`, `inline python` or `inline node`. Assembly is written inside an `inline c` block"));
        assert!(error("import c\nlet a = inline cpp {\n    int x;\n}\n").contains("C++ has no block"));
        assert!(error("import node\nlet a = inline js {\n    exports.x = 1\n}\n").ends_with("JavaScript is `inline node`"));
        // A block with nothing in it for Zeno says so.
        assert!(error("import c\nlet lib = inline c {\n    static int one(void) { return 1; }\n}\n").starts_with("this `inline c` block has no function that Zeno can call."));
        // What a block of Python or JavaScript gives is known when the
        // program runs, which strict mode, and so a build, does not take.
        ok("import python\nlet m = inline python {\n    def f(x):\n        return x\n}\nprint(m.f(1), m.anything)");
        ok("import node\nlet m = inline node {\n    exports.f = (x) => x\n}\nprint(m.f(1))");
        assert_eq!(
            strict_error("import python\nlet m = inline python {\n    x = 1\n}\n"),
            "strict mode: what an `inline python` block gives is known only when the program runs, by Python. `zeno build` and `zeno check --strict` do not take it, and `zeno run` does"
        );
        // `inline` is still a name.
        ok("let inline = 3\nprint(inline + 1)");
    }

    #[test]
    fn a_function_of_c_is_checked_before_the_program_runs() {
        // The declarations say what each function takes and gives, so a
        // wrong argument is found here, as for a function of Zeno
        // (decision 44.10). Nothing is built to find it.
        let lib = "import c\nlet m = c.library(\"m\", [\"double cbrt(double x);\", \"int abs(int n);\", \"char *getenv(const char *name);\", \"typedef struct thing thing;\", \"thing *make(void);\", \"void use(thing *t, uint8_t *bytes, size_t n);\", \"uint64_t big(void);\", \"int LIMIT;\"])\n";
        ok(&format!("{lib}let x: float = m.cbrt(8.0)\nlet n: int = m.abs(-2) + m.LIMIT\nlet home = m.getenv(\"HOME\")\nif home is not none {{\n    print(home.upper())\n}}\nlet t = m.make()\nlet bytes: list[u8] = [1, 2]\nm.use(t, bytes, len(bytes))\nm.use(none, [1, 2], 2)\nlet b: u64 = m.big() + 1\n"));
        assert_eq!(
            error(&format!("{lib}m.cbrt(\"8\")")),
            "the 1st parameter of `cbrt` (`double`) should be float, but this is str"
        );
        assert_eq!(error(&format!("{lib}m.abs(1, 2)")), "`abs` takes 1 argument, but 2 were given");
        assert_eq!(error(&format!("{lib}m.abs(n=1)")), "a function of C takes no named arguments, and `abs` was given one");
        assert_eq!(
            error(&format!("{lib}m.cube(1.0)")),
            "the library `m` has no `cube` among what is declared for it. Declared: cbrt, abs, getenv, make, use, big, LIMIT"
        );
        assert_eq!(
            error(&format!("{lib}m.use(m.getenv(\"x\"), [1], 1)")),
            "the 1st parameter of `use` (`thing *`) takes a pointer to thing that C gave or none, and this is str or none"
        );
        assert_eq!(
            error(&format!("{lib}m.use(none, [1.5], 1)")),
            "the 2nd parameter of `use` (`uint8_t *`) takes a pointer to uint8_t that C gave, a list of whole numbers or none, and this is list[float]"
        );
        assert_eq!(error(&format!("{lib}let s: str = m.getenv(\"x\")")), "`s` should be str, but this is str or none. Check it with `is not none` first");
        // A declaration that is not one is found here too.
        assert_eq!(
            error("import c\nc.library(\"m\", \"int printf(const char *format, ...);\")"),
            "`printf` takes any number of arguments (`...`), and Zeno cannot know their types. Declare it with the ones the program gives, under another name of C, or write a function of C that calls it"
        );
        // What is worked out as the program runs is known only then.
        ok("import c\nlet name = \"m\"\nlet m = c.library(name, \"double cbrt(double x);\")\nprint(m.anything(1, 2))");
        assert_eq!(
            strict_error("import c\nlet name = \"m\"\nlet m = c.library(name, \"double cbrt(double x);\")\nprint(m.cbrt(1.0))"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
    }

    #[test]
    fn built_ins_are_checked() {
        assert_eq!(error("print(len(5))"), "`len` needs a str, a list or a dict, but this is int");
        assert_eq!(error("print(len(1, 2))"), "`len` takes 1 argument, but 2 were given");
        assert_eq!(error("let s: str = len(\"abc\")"), "`s` should be str, but this is int");
        assert_eq!(error("range(\"3\")"), "an argument of `range` should be int, but this is str");
        assert_eq!(error("from time import sleep\nsleep(500)"), "`sleep` needs a time with a unit, such as `500 ms`, but this is int");
        assert_eq!(error("print(\"x\".upper(1))"), "`upper` takes 0 arguments, but 1 was given");
        assert_eq!(error("print(\"x\".nope())"), "str has no field or method `nope`");
        assert_eq!(error("let xs = [1]\nxs.append(\"two\")"), "an item added to this list should be int, but this is str");
        // `+` is held to the same rule as `append`: two lists of known,
        // unrelated things do not join, with or without `--strict`.
        let unrelated = "`+` joins two lists that hold the same kind of thing, and these are list[int] and list[str]";
        assert_eq!(error("let xs = [1] + [\"two\"]"), unrelated);
        assert_eq!(error("var xs = [1]\nxs += [\"two\"]"), unrelated);
        assert_eq!(strict_error("let xs: list[int] = [1] + [\"two\"]"), unrelated);
        // A list that may hold `none` joins one that does not, and a
        // list that is not known yet joins anything.
        ok("let a: list[int or none] = [none]\nlet b = [1] + a\nvar c = []\nc += [1]\nlet d = [1, \"x\"] + [2]\nprint(b, c, d)");
        assert_eq!(
            error("import radio"),
            "there is no module `radio`. So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`"
        );
        assert_eq!(error("from math import nope"), "module `math` has no `nope`");
        // A name of the part of `math` that is written in Zeno, which
        // `modules.rs` joins to a file that imports `math` at its top.
        assert_eq!(
            error("function far() -> float {\n    import math\n    return math.hypot(3.0, 4.0)\n}"),
            "`math.hypot` is written in Zeno, and is joined to a file that has `import math` at its top. \
             Move the `import` there from inside the function"
        );
        ok("from math import sqrt\nimport math\nlet x: float = sqrt(2) + math.pi\nlet n: int = math.floor(x)\nlet parts: list[str] = \"a b\".split()");
    }

    #[test]
    fn functions_passed_to_map_filter_reduce_are_checked() {
        ok("let xs = [1, 2, 3]\nlet ys: list[str] = xs.map(str)\nlet total: int = xs.reduce(function(a: int, b: int) -> int { return a + b }, 0)\nlet evens: list[int] = xs.filter(function(n: int) -> bool { return n % 2 == 0 })");
        assert_eq!(
            error("let xs = [1, 2]\nxs.filter(function(n: int) -> int { return n })"),
            "what the function given to `filter` returns should be bool, but this is int"
        );
        assert_eq!(
            error("let xs = [1, 2]\nxs.map(function(s: str) -> str { return s })"),
            "`s` of `function` should be str, but this is int"
        );
        assert_eq!(error("[1].map(function() { return 1 })"), "`function` takes 0 arguments, but 1 was given");
    }

    // ----- Functions -----

    #[test]
    fn returns_must_match_the_declared_type() {
        assert_eq!(error("function f() -> int { return \"x\" }"), "the returned value should be int, but this is str");
        assert_eq!(error("function f() -> int { return }"), "the returned value should be int, but this is none");
        assert_eq!(
            error("function f(n: int) -> int {\n if n > 0 { return 1 }\n}"),
            "`f` can reach its end without returning the int it promises"
        );
        ok("function f(n: int) -> int {\n if n > 0 { return 1 } else { return 2 }\n}");
        ok("function f(n: int) -> int {\n while true {\n  if n > 0 { return 1 }\n }\n}");
        ok("function f(n: int) -> int or none {\n if n > 0 { return 1 }\n}");
        ok("function f() -> int { raise ValueError(\"no\") }");
        assert_eq!(error("return 1"), "`return` outside a function");
        assert_eq!(error("break"), "`break` outside a loop");
        assert_eq!(error("while true {\n function f() { continue }\n}"), "`continue` outside a loop");
    }

    #[test]
    fn recursion_is_checked() {
        ok("function fact(n: int) -> int {\n if n <= 1 { return 1 }\n return n * fact(n - 1)\n}\nlet x: int = fact(5)");
        ok("function even(n: int) -> bool {\n if n == 0 { return true }\n return odd(n - 1)\n}\nfunction odd(n: int) -> bool {\n if n == 0 { return false }\n return even(n - 1)\n}");
        assert_eq!(error("function fact(n: int) -> int {\n return fact(\"x\")\n}"), "`n` of `fact` should be int, but this is str");
    }

    #[test]
    fn a_function_nobody_calls_is_still_checked() {
        assert_eq!(error("function unused() { print(missing) }"), "`missing` is not defined");
    }

    // ----- The bits of whole numbers -----

    #[test]
    fn bit_operators_need_whole_numbers() {
        ok("let a: int = 6 & 3\nlet b: int = 1 << 4 | 1\nlet c: int = ~a ^ b >> 1\nvar d = 0\nd |= 4\nd <<= 1");
        assert_eq!(
            error("print(true & false)"),
            "`&` works on the bits of whole numbers, and these are bool and bool. For two bools Zeno spells this `and`"
        );
        assert_eq!(
            error("print(true | false)"),
            "`|` works on the bits of whole numbers, and these are bool and bool. For two bools Zeno spells this `or`"
        );
        assert_eq!(error("print(1.5 << 2)"), "`<<` works on the bits of whole numbers, and these are float and int");
        assert_eq!(error("print(~1.5)"), "`~` works on the bits of a whole number, and this is float");
        assert_eq!(error("var x = 1.0\nx &= 3"), "`&` works on the bits of whole numbers, and these are float and int");
    }

    // ----- Sized whole numbers -----

    #[test]
    fn sized_ints_are_types() {
        strict_ok(include_str!("../examples/packet.zeno"));
        strict_ok(
            "let a: u8 = 200\nlet b: u8 = u8(100)\nlet c: i8 = i8.wrap(a)\nlet d: u16 = u16(a + b)\n\
             let e: i32 = -2147483648\nlet f: u32 = 4294967295\nlet g: i16 = -32768\n\
             let added: int = a + b\nlet bits: int = a << 8 | b\nlet same: bool = a == b or a < 3 or a in [1, 2]\n\
             let wide: int = a\nlet text: str = f\"{a:02x}\"\nlet biggest: int = max(a, b)\nlet size: int = abs(c)\n\
             function double(n: int) -> int { return n * 2 }\nlet twice: int = double(a)\n\
             class Reg {\n    value: u8\n    mask: u8 = 0xFF\n}\nlet r = Reg(value=1)\nr.value = u8(r.value | 4)\nr.mask = 15\n\
             let bytes: list[u8] = [1, 2, 255]\nbytes.append(a)\nbytes.append(9)\nbytes[0] = 7\nlet first: u8 = bytes[0]\n\
             let total: int = sum(bytes)\nvar maybe: u8 or none = none\nmaybe = 5\nmaybe = a\n\
             for i in range(a) { print(bytes[b]) }",
        );
        // A class of the program's own may take the name.
        ok("class u8 {\n    n: int\n}\nlet x: u8 = u8(n=300)");
    }

    #[test]
    fn a_written_number_must_fit_its_size() {
        assert_eq!(error("let a: u8 = 256"), "256 does not fit in a u8 (0 to 255)");
        assert_eq!(error("let a: u8 = -1"), "-1 does not fit in a u8 (0 to 255)");
        assert_eq!(error("let a: i8 = -129"), "-129 does not fit in an i8 (-128 to 127)");
        assert_eq!(error("let a: u16 or none = 65536"), "65536 does not fit in a u16 (0 to 65535)");
        assert_eq!(error("let a: list[u8] = [1, 2, 300]"), "300 does not fit in a u8 (0 to 255)");
        assert_eq!(error("var a: u8 = 0\na = 999"), "999 does not fit in a u8 (0 to 255)");
        assert_eq!(error("function f(n: u8) { print(n) }\nf(256)"), "256 does not fit in a u8 (0 to 255)");
        assert_eq!(error("function f(n: i8 = 128) { print(n) }"), "128 does not fit in an i8 (-128 to 127)");
        assert_eq!(error("function f() -> i8 { return 128 }"), "128 does not fit in an i8 (-128 to 127)");
        assert_eq!(error("class P {\n    v: u8 = 256\n}"), "256 does not fit in a u8 (0 to 255)");
        assert_eq!(error("class P {\n    v: u8\n}\nlet p = P(v=-1)"), "-1 does not fit in a u8 (0 to 255)");
        assert_eq!(error("let xs: list[u8] = [1]\nxs.append(256)"), "256 does not fit in a u8 (0 to 255)");
        assert_eq!(error("let xs: list[u8] = [1]\nxs[0] = 256"), "256 does not fit in a u8 (0 to 255)");
        // `u8( )` of a written number is checked here too. `wrap` is not:
        // it is asked for the low bits.
        assert_eq!(error("let a = u8(1000)"), "1000 does not fit in a u8 (0 to 255)");
        ok("let a: u8 = u8.wrap(1000)\nlet b: i32 = i32.wrap(-99999999999)");
    }

    #[test]
    fn an_int_does_not_become_sized_by_itself() {
        let hint = "Convert it with `u8(...)`, which checks that it fits";
        assert_eq!(error("let n = 5\nlet a: u8 = n"), format!("`a` should be u8, but this is int. {hint}"));
        // The answer of an operator is an int, whatever went in.
        assert_eq!(error("var a: u8 = 1\na = a + 1"), format!("the new value should be u8, but this is int. {hint}"));
        assert_eq!(error("let a: u8 = 1\nlet b: u8 = ~a"), format!("`b` should be u8, but this is int. {hint}"));
        assert_eq!(
            error("let a: u8 = 1\nlet b: u16 = a"),
            "`b` should be u16, but this is u8. Convert it with `u16(...)`, which checks that it fits"
        );
        assert_eq!(
            error("function f(n: u8) { print(n) }\nlet n = 5\nf(n)"),
            format!("`n` of `f` should be u8, but this is int. {hint}")
        );
        assert_eq!(
            error("let xs: list[u8] = [1]\nlet n = 5\nxs.append(n)"),
            format!("an item added to this list should be u8, but this is int. {hint}")
        );
        assert_eq!(
            error("let xs: list[u8] = [1]\nlet ys: list[int] = xs"),
            "`ys` should be list[int], but this is list[u8]. The list could then be given a number too big for it. \
             Make a list of ints from it with `[int(n) for n in ...]`"
        );
        assert_eq!(error("let a = u8(2.5)"), "`u8` needs a whole number, but this is float. Make it one with `int( )` first");
        assert_eq!(error("let a = u8.wrap(\"x\")"), "`u8.wrap` needs a whole number, but this is str");
        assert_eq!(error("let a = u8(1, 2)"), "`u8` takes 1 argument, but 2 were given");
        assert_eq!(
            error("let a: i64 = 1"),
            "there is no type called `i64`. An `int` is 64 bits with a sign, so write `int`. \
             The sized whole numbers are u8, u16, u32, u64, i8, i16 and i32"
        );
    }

    #[test]
    fn a_size_left_out_is_worked_out() {
        // From the value, and for a parameter from the calls.
        strict_ok("let a = u8(5)\nlet b: u8 = a\nfunction same(n) { return n }\nlet c: u8 = same(a)");
        // A u8 from one call and an int from another: the parameter is an int.
        assert_eq!(
            strict_error("function same(n) { return n }\nlet a = same(u8(5))\nlet b: u8 = same(7)"),
            "`b` should be u8, but this is int. Convert it with `u8(...)`, which checks that it fits"
        );
        // A list of a u8 and an int is a list of ints.
        strict_ok("let a: u8 = 1\nlet xs: list[int] = [a, 300]\nlet ys = [a, u16(2)]\nlet zs: list[int] = ys");
    }

    // ----- Generics -----

    const FIRST: &str = "function first[T](items: list[T]) -> T {\n    return items[0]\n}\n";
    const STACK: &str = "class Stack[T] {\n    items: list[T]\n    function push(self, item: T) {\n        self.items.append(item)\n    }\n}\n";
    const SAME: &str = "function same[T](a: T, b: T) -> T {\n    return a\n}\n";

    #[test]
    fn generics_are_checked() {
        strict_ok(include_str!("../examples/generics.zeno"));
        // What comes back is the type that went in.
        strict_ok(&format!("{FIRST}let a: int = first([1, 2])\nlet b: str = first([\"a\"])\nlet c: list[float] = first([[1.5]])"));
        strict_ok(&format!(
            "{STACK}let a = Stack([1])\na.push(2)\nlet b: Stack[str] = Stack(items=[])\nb.push(\"x\")\n\
             let both: list[Stack[int]] = [a, Stack([3])]\nlet maybe: Stack[str] or none = b\n\
             function total(s: Stack[int]) -> int {{\n    return sum(s.items)\n}}\nlet n: int = total(a)"
        ));
        // Inside a generic the type parameter is a type like any other,
        // and it wins over a unit of the same name: `A` is not amperes.
        strict_ok(
            "class Pair[A, B] {\n    left: A\n    right: B\n}\n\
             function swap[A, B](p: Pair[A, B]) -> Pair[B, A] {\n    let left: A = p.left\n    return Pair(p.right, left)\n}\n\
             let q: Pair[str, int] = swap(Pair(1, \"x\"))\nlet amps: A = 2 A",
        );
        // A function argument is called with what the others gave.
        strict_ok(
            "function apply[T, U](value: T, f: function(T) -> U) -> U {\n    return f(value)\n}\n\
             let a: int = apply(5, function(n) { return n * 2 })\nlet b: str = apply(\"hi\", function(s) { return s.upper() })\n\
             let c: int = apply(\"abc\", len)",
        );
        // An owned class can be generic, and its copies are owned.
        strict_ok(
            "owned class Box[T] {\n    value: T\n}\nfunction look[T](box: Box[T]) -> T {\n    return box.value\n}\n\
             function open[T](box: Box[T]) -> T {\n    return box.value\n}\nlet b = Box(5)\nlet n: int = look(b)\nlet m: int = open(b)",
        );
    }

    #[test]
    fn the_types_of_a_generic_come_from_the_call() {
        assert_eq!(error(&format!("{FIRST}let x: str = first([1, 2])")), "`x` should be str, but this is int");
        assert_eq!(error(&format!("{FIRST}let n = 5\nlet x: str = first([n])")), "`x` should be str, but this is int");
        assert_eq!(error(&format!("{FIRST}print(first(5))")), "`items` of `first` should be list[T], but this is int");
        assert_eq!(error(&format!("{STACK}let a: Stack[int] = Stack([1])\nlet b: Stack[str] = a")), "`b` should be Stack[str], but this is Stack[int]");
        assert_eq!(error(&format!("{STACK}let a = Stack([1])\na.push(\"x\")")), "`item` of `push` should be int, but this is str");
        assert_eq!(error(&format!("{SAME}let n = 1\nprint(same(n, \"x\"))")), "`b` of `same[int]` should be int, but this is str");
        // Nothing says what `T` is, until the place the result goes does.
        let make = "function make[T]() -> list[T] {\n    return []\n}\n";
        assert_eq!(
            error(&format!("{make}let a = make()")),
            "`make` cannot tell what `T` is from this call: no argument says. \
             Put the result where its type is written, as in `let x: list[int] = make(...)`"
        );
        assert_eq!(
            strict_error(&format!("{STACK}let s = Stack(items=[])")),
            "`Stack` cannot tell what `T` is from this call: no argument says. \
             Put the result where its type is written, as in `let x: Stack[int] = Stack(...)`"
        );
        strict_ok(&format!("{make}let a: list[int] = make()\nfunction f() -> list[str] {{\n    return make()\n}}"));
        // `none` and a value make an `or none`. A wider number wins.
        strict_ok(&format!("{SAME}let n = 3\nlet a: int or none = same(none, n)\nlet small: u8 = 4\nlet wide: int = same(small, n)"));
    }

    #[test]
    fn a_written_number_in_a_generic_call_takes_the_size_that_is_wanted() {
        let ring = "class Ring[T] {\n    slots: list[T]\n}\nfunction ring[T](size: int, empty: T) -> Ring[T] {\n    return Ring([empty])\n}\n";
        strict_ok(&format!(
            "{ring}{SAME}{FIRST}let bytes: Ring[u8] = ring(4, 0)\nlet ints: Ring[int] = ring(4, 0)\nlet small: u8 = 4\n\
             let x: u8 = same(small, 9)\nlet y: u8 = first([1, 2, 3])\nlet z: int = same(1, 2)"
        ));
        assert_eq!(error(&format!("{SAME}let small: u8 = 4\nprint(same(small, 300))")), "300 does not fit in a u8 (0 to 255)");
        assert_eq!(error(&format!("{ring}let bytes: Ring[u8] = ring(4, 256)")), "256 does not fit in a u8 (0 to 255)");
    }

    #[test]
    fn a_generic_is_checked_once_for_each_set_of_types() {
        // The body is checked with `T` as each type it is used with, so
        // a `*` that a str does not have is found, and the error says
        // which copy it is in.
        let source = "class Box[T] {\n    value: T\n    function double(self) -> T {\n        return self.value * 2\n    }\n}\n\
                      print(Box(2).double())\nprint(Box(2.5).double())\nprint(Box(\"ab\").double())";
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let found = super::check(&program, false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].message, "`*` does not work between str and int");
        assert_eq!((found[0].span.line, found[0].span.col), (4, 16));
        assert_eq!(found[0].note.as_deref(), Some("this is inside `Box[str]`: `Box` with T = str, which line 9 asks for"));

        // A generic nothing uses is still checked, with `T` as anything.
        assert_eq!(error("function unused[T](x: T) -> T {\n    return y\n}"), "`y` is not defined");
        ok("function unused[T](x: T) -> T {\n    return x * 2 + len(x)\n}");
        // A class that needs a bigger copy of itself each time never ends.
        assert_eq!(
            error("class Node[T] {\n    value: T\n    next: Node[list[T]] or none\n}\nlet n = Node(1, none)"),
            "`Node` needs a copy of itself for a bigger type every time, and that never ends. \
             It got as far as `Node[list[list[list[list[list[list[list[list[list[list[list[...`"
        );
    }

    #[test]
    fn a_value_with_one_owner_can_fill_a_type_parameter() {
        // A copy of a generic is checked as if it were written by hand,
        // with the type in the place of `T` (decision 56.11).
        const GENERICS: &str = "class Frame {\n id: int\n data: list[int]\n}\n\
                                enum Shape {\n Dot\n Circle(r: int)\n}\n\
                                class Box[T] {\n value: T\n}\n\
                                class Stack[T] {\n items: list[T]\n\
                                function push(self, x: take T) { self.items.append(x) }\n\
                                function pop(self) -> T { return self.items.pop() }\n}\n\
                                function same[T](x: take T) -> T { return x }\n\
                                function count[T](xs: list[T]) -> int { return len(xs) }\n";
        let good = |source: &str| one_owner_ok(&format!("{GENERICS}{source}"));
        let bad = |source: &str| one_owner_error(&format!("{GENERICS}{source}"));
        good("let xs = same([1, 2])\nlet f = same(Frame(1, [1]))\nlet s = same(Circle(2))\nprint(xs, f.id, s)");
        good("let a = Box([1])\nlet b = Box(Frame(1, []))\nlet c: Box[Shape] = Box(Dot)\nprint(a.value, b.value.id, c.value)");
        good("let s: Stack[Frame] = Stack([])\ns.push(Frame(1, []))\nlet f = s.pop()\nprint(f.id)");
        good("let ls: Stack[list[int]] = Stack([[1]])\nls.push([2])\nprint(count(ls.items))");
        // A parameter written `T` looks, so what it is given is still there.
        good("let fs = [Frame(1, [])]\nprint(count(fs))\nprint(len(fs))");
        // One written `take T` takes it, as any `take` does.
        let f = "let f = Frame(1, [])\n";
        assert!(bad(&format!("{f}let g = same(f)\nprint(f.id)")).contains(GONE));
        assert!(bad(&format!("{f}let b = Box(f)\nprint(f.id)")).contains(GONE));
        assert_eq!(
            bad("function f(p: Frame) -> Frame { return same(p) }"),
            "`p` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // `take T` is right for a number too, in every build: there the
        // word changes nothing.
        let numbers = "function same[T](x: take T) -> T { return x }\nprint(same(5), same(\"a\"))";
        one_owner_ok(numbers);
        strict_ok(numbers);
        // `take` on a type that is copied, and is no type parameter, is
        // still a mistake.
        assert!(error("function f(x: take int) { }").contains("This is int, which is copied"));
        // A generic that returns what it only looked at is wrong for a
        // value with one owner. The error is inside the copy, and says so.
        let source = format!("{GENERICS}function keep[T](x: T) -> T {{\n return x\n}}\nprint(keep(Frame(1, [])).id)");
        let program = parser::parse(lexer::lex(&source).unwrap()).unwrap();
        let (found, _) = super::check_by(&program, super::Rules { strict: true, one_owner: true, firmware: true });
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.starts_with("`x` is only looked at here"), "{}", found[0].message);
        assert_eq!(found[0].span.line, 20);
        assert_eq!(found[0].note.as_deref(), Some("this is inside `keep[Frame]`: `keep` with T = Frame, which line 22 asks for"));
        // With a collector, an object of an `owned class` fills `T` as
        // well, and a copy that holds one is held to the rules of a class
        // written for it.
        owned_ok("function same[T](x: take T) -> T { return x }\nlet p = same(make())\nprint(p.id)");
        assert_eq!(
            owned_error("class Box[T] {\n value: T\n}\nlet b = Box(make())"),
            "`value` holds an owned value, so `Box[Packet]` must be an `owned class` itself. A shared object cannot own anything"
        );
    }

    #[test]
    fn what_a_generic_cannot_do_yet() {
        assert_eq!(
            error("function first[t](items: list[t]) -> t {\n    return items[0]\n}"),
            "a type parameter starts with a capital letter, as in `T` or `Item`, and this is `t`"
        );
        assert_eq!(error("function two[T, T](a: T) -> T {\n    return a\n}"), "the type parameter `T` is written twice");
        assert_eq!(
            error(&format!("{STACK}let s: Stack[int, str] = Stack(items=[])")),
            "`Stack` takes 1 type in `[ ]`, as in `Stack[int]`, but 2 were written"
        );
        assert_eq!(
            error(&format!("{STACK}function f(s: Stack) {{ }}")),
            "`Stack` takes 1 type in `[ ]`, as in `Stack[int]`, but 0 were written"
        );
        assert_eq!(error(&format!("{FIRST}let n: first = 4")), "`first` is a function, and this place needs a type");
        assert_eq!(
            error(&format!("{FIRST}let f = first")),
            "`first` is generic. It can be called, as in `first(...)`, but it cannot be kept as a value or passed along yet"
        );
        assert_eq!(
            error("class Box {\n    n: int\n    function get[T](self, x: T) -> T {\n        return x\n    }\n}"),
            "the method `get` cannot have type parameters of its own yet. Write it as a function of the file that \
             takes the object first: `function get[T](box: Box, ...)`"
        );
        // In a generic class the class's own come first (58.7).
        assert_eq!(
            error(
                "class Box[T] {\n    item: T\n    function convert[U](self, change: function(T) -> U) -> Box[U] {\n        \
                 return Box(change(self.item))\n    }\n}"
            ),
            "the method `convert` cannot have type parameters of its own yet. Write it as a function of the file that \
             takes the object first: `function convert[T, U](box: Box[T], ...)`"
        );
        // The error is on the line of the method, not of the class.
        let source = "class Box[T] {\n    item: T\n\n    function convert[U](self, change: function(T) -> U) -> U {\n        \
                      return change(self.item)\n    }\n}\nprint(Box(1).item)";
        let program = parser::parse(lexer::lex(source).unwrap()).unwrap();
        let found = super::check(&program, false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].span.line, 4);
        assert_eq!(
            error("class Problem[T] {\n    what: T\n}\nfunction f() {\n    raise Problem(4)\n}"),
            "an object of a generic class cannot be raised yet, and this is a Problem[int]"
        );
    }

    const MAYBE: &str = "enum Maybe[T] {\n    Nothing\n    Just(value: T)\n}\n";

    #[test]
    fn a_generic_enum_takes_its_types_from_where_it_is_used() {
        // From what a case carries, and for a case that carries nothing
        // from the place it goes: a variable, a parameter, a result, a
        // field, a list, a dict, the other side of `==`, an argument of
        // a generic beside one that says (decision 58.1).
        ok(&format!(
            "{MAYBE}let a = Just(3)\nlet b: Maybe[str] = Nothing\nfunction f(m: Maybe[int]) -> Maybe[int] {{\n    return Nothing\n}}\n\
             f(Nothing)\nclass Holder {{\n    slot: Maybe[int] = Nothing\n}}\nlet xs: list[Maybe[int]] = [Just(1), Nothing]\n\
             let d: dict[str, Maybe[int]] = [\"a\": Nothing]\nlet same: bool = a == Nothing\nlet c: Maybe[int] or none = Nothing\n\
             function or_else[T](m: Maybe[T], other: T) -> T {{\n    match m {{\n        case Just(v) {{ return v }}\n        case Nothing {{ return other }}\n    }}\n}}\n\
             let n: int = or_else(Nothing, 4)\nlet s: str = or_else(Just(\"a\"), \"b\")\n"
        ));
        // A copy is a type of its own: a `Maybe[str]` is no `Maybe[int]`.
        assert_eq!(
            error(&format!("{MAYBE}let b: Maybe[str] = Nothing\nlet c: Maybe[int] = b")),
            "`c` should be Maybe[int], but this is Maybe[str]"
        );
        // An enum that holds itself.
        ok("enum Chain[T] {\n    End\n    Link(item: T, rest: Chain[T])\n}\nlet c: Chain[int] = Link(1, Link(2, End))\n");
    }

    #[test]
    fn what_a_generic_enum_says_when_it_cannot() {
        assert_eq!(
            error(&format!("{MAYBE}let x = Nothing")),
            "`Nothing` is a case of the generic enum `Maybe`, and nothing here says which `Maybe` it is. \
             Put it where its type is written, as in `let x: Maybe[int] = Nothing`"
        );
        assert_eq!(
            error("enum Result[T, E] {\n    Ok(value: T)\n    Failed(error: E)\n}\nlet r = Ok(1)"),
            "`Ok` cannot tell what `E` is from this call: no argument says. \
             Put the result where its type is written, as in `let x: Result[int, int] = Ok(...)`"
        );
        // A case left out is named as for any enum.
        assert_eq!(
            error(&format!("{MAYBE}let m: Maybe[int] = Just(3)\nmatch m {{\n    case Just(v) {{ print(v) }}\n}}")),
            "this `match` does not handle Nothing. Add a `case` for it, or a final `case _`"
        );
        assert_eq!(
            error(&format!("{MAYBE}let m: Maybe[int] = Just(3)\nmatch m {{\n    case Nothing {{ print(0) }}\n}}")),
            "this `match` does not handle Just. Add a `case` for it, or a final `case _`"
        );
        assert_eq!(
            error(&format!("{MAYBE}let n: int = 3\nmatch n {{\n    case Just(v) {{ print(v) }}\n    case _ {{ }}\n}}")),
            "`Just` is a case of Maybe, which can never match int"
        );
        assert_eq!(error(&format!("{MAYBE}print(Maybe(3))")), "`Maybe` is an enum, and a value of it is one of its cases, as in `Just(...)`");
        assert_eq!(
            error(&format!("{MAYBE}let f = Just")),
            "`Just` is a case of the generic enum `Maybe`. It can be called, as in `Just(...)`, but it cannot be kept as a value or passed along yet"
        );
        assert_eq!(
            error(&format!("{MAYBE}let m: Maybe = Just(3)")),
            "`Maybe` takes 1 type in `[ ]`, as in `Maybe[int]`, but 0 were written"
        );
        // A case is written by its name alone, so it is one enum's only.
        assert_eq!(
            error(&format!("{MAYBE}enum Other {{\n    Nothing\n}}")),
            "`Nothing` is already a case of the enum `Maybe`. A case is written by its name alone, \
             so two enums in one file cannot both have a `Nothing`. Give one of them another name"
        );
        assert_eq!(
            error(&format!("enum Other {{\n    Nothing\n}}\n{MAYBE}")),
            "`Nothing` is already a case of the enum `Other`. A case is written by its name alone, \
             so two enums in one file cannot both have a `Nothing`. Give one of them another name"
        );
        assert_eq!(
            error("enum Code[T] {\n    A = 1\n    B = 2\n}"),
            "`Code` has type parameters, so its cases carry values, and a case that carries values cannot stand for a number. \
             Take `= 1` away"
        );
        assert_eq!(
            error("enum Maybe[t] {\n    Nothing\n}"),
            "a type parameter starts with a capital letter, as in `T` or `Item`, and this is `t`"
        );
    }

    /// A generic function and a generic class that ask something of `T`.
    const BOUNDS: &str = "interface Shape {\n    function area(self) -> float\n}\n\
        class Circle is Shape {\n    r: float\n    function area(self) -> float { return 3.0 * self.r * self.r }\n}\n\
        class Task {\n    n: int\n}\n\
        function largest[T is Ordered](items: list[T]) -> T {\n    var best = items[0]\n    \
        for item in items {\n        if item > best { best = item }\n    }\n    return best\n}\n\
        function total[S is Shape](shapes: list[S]) -> float {\n    var sum = 0.0\n    \
        for s in shapes { sum += s.area() }\n    return sum\n}\n\
        class Range[T is Ordered] {\n    low: T\n    high: T\n}\n";

    #[test]
    fn a_type_parameter_may_ask_for_an_interface() {
        // `Ordered` is every kind of number, a quantity and text; a bound
        // of the program's own takes its classes and itself (58.3).
        ok(&format!(
            "{BOUNDS}let a: int = largest([3, 9, 2])\nlet b: float = largest([1.5, 0.5])\nlet c: str = largest([\"a\", \"b\"])\n\
             let d = largest([3 V, 12 V])\nlet bytes: list[u8] = [3, 200]\nlet e: u8 = largest(bytes)\n\
             let halves: list[f32] = [0.5]\nlet f: f32 = largest(halves)\n\
             let g: float = total([Circle(1.0)])\nlet shapes: list[Shape] = [Circle(2.0)]\nlet h: float = total(shapes)\n\
             let r = Range(1 ms, 5 ms)\nenum Span[T is Ordered] {{\n    Empty\n    Between(low: T, high: T)\n}}\n\
             let s: Span[int] = Between(1, 2)\n"
        ));
        // A program's own `Ordered` is the program's.
        ok("interface Ordered {\n    function rank(self) -> int\n}\nclass Task is Ordered {\n    n: int\n    \
            function rank(self) -> int { return self.n }\n}\n\
            function best[T is Ordered](items: list[T]) -> int { return items[0].rank() }\nprint(best([Task(4)]))");
        // The body is still checked for each copy: a bound does not have
        // to name everything the body does.
        ok("function smaller[T is Ordered](a: T, b: T) -> T {\n    if a < b { return a }\n    return b\n}\nprint(smaller(2, 1))");
    }

    #[test]
    fn a_type_that_lacks_what_is_asked_is_refused_at_the_call() {
        assert_eq!(
            error(&format!("{BOUNDS}print(largest([Task(1)]))")),
            "`Task` is not `Ordered`, which `largest` asks of `T`. `Ordered` is what `<` works on: a number, a quantity \
             or text. A class cannot be `Ordered` yet, since `<` between its objects comes with operators for a class"
        );
        assert_eq!(
            error(&format!("{BOUNDS}print(largest([true]))")),
            "`bool` is not `Ordered`, which `largest` asks of `T`. `Ordered` is what `<` works on: a number, a quantity or text"
        );
        assert_eq!(
            error(&format!("{BOUNDS}print(total([Task(1)]))")),
            "`Task` is not `Shape`, which `total` asks of `S`. It is one if it says so: `class Task is Shape`, with what `Shape` lists"
        );
        // For a class too, and once though a line asks twice.
        assert_eq!(
            error(&format!("{BOUNDS}let r: Range[bool] = Range(true, false)")),
            "`bool` is not `Ordered`, which `Range` asks of `T`. `Ordered` is what `<` works on: a number, a quantity or text"
        );
        // Checked on its own, a generic sees `S` as a `Shape` (58.5).
        assert_eq!(
            error("interface Shape {\n    function area(self) -> float\n}\n\
                   function f[S is Shape](s: S) -> float { return s.perimeter() }"),
            "`perimeter` is not a function of the interface `Shape`, which has `area`. What else this object has depends on \
             its class, and that is only known when the program runs. Give `Shape` a function for it"
        );
    }

    #[test]
    fn a_generic_given_a_list_of_two_types_says_why_in_a_build() {
        // `zeno run` takes it, and keeps each item as it is; a build gives
        // a list one type, so the list says nothing about `T` (58.8).
        let first = "function first[T](xs: list[T]) -> T {\n    return xs[0]\n}\n";
        ok(&format!("{first}print(first([200 ms, 1 s]))"));
        assert_eq!(
            strict_error(&format!("{first}print(first([200 ms, 1 s]))")),
            "`first` cannot tell what `T` is from this list: its items are not all one type, but a quantity in ms and \
             a quantity in s, and a build gives a list one type. Write them all in one unit, such as `ms`"
        );
        assert_eq!(
            strict_error(&format!("{first}print(first([1, 2.5]))")),
            "`first` cannot tell what `T` is from this list: its items are not all one type, but int and float, and \
             a build gives a list one type. Write each as a float, as `1.0` for `1`"
        );
        assert_eq!(
            strict_error(&format!("{first}print(first([1, \"a\", 2]))")),
            "`first` cannot tell what `T` is from this list: its items are not all one type, but int and str, and \
             a build gives a list one type. Make them one type, or an enum with a case for each"
        );
        strict_ok(&format!("{first}print(first([200 ms, 1000 ms]))"));
    }

    #[test]
    fn what_a_bound_says_when_it_is_written_wrong() {
        assert_eq!(
            error("function f[T is int](x: T) -> T { return x }"),
            "after `is` comes an interface, such as `Ordered` or one the program declares, and `int` is not one"
        );
        assert_eq!(
            error("class Task {\n    n: int\n}\nfunction f[T is Task](x: T) -> T { return x }"),
            "after `is` comes an interface, and `Task` is a class. Write `Task` itself where `T` is used, \
             or declare an interface that `Task` is"
        );
        assert_eq!(
            error("interface Shape {\n    function area(self) -> float\n}\nfunction f[T is Ordered, Shape](x: T) -> T { return x }"),
            "`Shape` after a comma is a second type parameter, which hides the interface `Shape`. A type parameter asks \
             for one interface: `[T is ...]`. If `Shape` is meant to be a type parameter, give it another name"
        );
        assert_eq!(
            error("class Task is Ordered {\n    n: int\n}"),
            "`Task` cannot be `Ordered` yet: `Ordered` is what `<` works on, numbers, quantities and text, \
             and `<` between objects of a class comes with operators for a class"
        );
        assert_eq!(
            error("let x: Ordered = 3"),
            "`Ordered` is not a type: it is what a type parameter may ask of its type, that `<` works on two of it, \
             as in `function largest[T is Ordered](items: list[T]) -> T`"
        );
    }

    /// A generic interface and two classes that are copies of it.
    const SOURCE: &str = "interface Source[T] {\n    function next(self) -> T or none\n}\n\
        class Count is Source[int] {\n    left: int\n    function next(self) -> int or none { return none }\n}\n\
        class Words is Source[str] {\n    function next(self) -> str or none { return none }\n}\n\
        class Again[T] is Source[T] {\n    value: T\n    function next(self) -> T or none { return self.value }\n}\n";

    #[test]
    fn an_interface_may_have_types_of_its_own() {
        // A class says which copy it is, and fits that copy and no other
        // (decision 58.6).
        ok(&format!(
            "{SOURCE}let a: Source[int] = Count(3)\nlet b: Source[str] = Words()\nlet c: Source[int] = Again(4)\n\
             let d: Source[str] = Again(\"x\")\nlet all: list[Source[int]] = [Count(1), Again(2)]\n\
             function drain(s: Source[int]) -> int {{\n    let n = s.next()\n    if n is none {{ return 0 }}\n    return n\n}}\n\
             print(drain(Count(1)), drain(Again(5)))\n\
             function first[S is Source[str]](s: S) -> str or none {{ return s.next() }}\nprint(first(Words()))\n"
        ));
        assert_eq!(
            error(&format!("{SOURCE}let a: Source[int] = Words()")),
            "`a` should be Source[int], but this is Words"
        );
        assert_eq!(
            error(&format!("{SOURCE}let b: Source[str] = Words()\nlet a: Source[int] = b")),
            "`a` should be Source[int], but this is Source[str]"
        );
        assert_eq!(
            error(&format!("{SOURCE}function first[S is Source[str]](s: S) {{ }}\nfirst(Count(1))")),
            "`Count` is not `Source[str]`, which `first` asks of `S`. It is one if it says so: `class Count is Source[str]`, \
             with what `Source[str]` lists"
        );
    }

    #[test]
    fn each_copy_of_a_generic_interface_is_held_to_taking_out_by_itself() {
        // `Slot`, a `Holder[list[int]]`, takes out; `Name`, a
        // `Holder[str]`, does not. A call through a `Holder[str]` is for
        // anybody, and one through a `Holder[list[int]]` for the owner
        // only (58.6 with 58.21 and 58.22).
        let holder = "owned interface Holder[T] {\n    function give(self) -> T or none\n}\n\
                      owned class Slot is Holder[list[int]] {\n    items: list[int] or none\n    \
                      function give(self) -> list[int] or none { return take self.items }\n}\n\
                      owned class Name is Holder[str] {\n    text: str\n    \
                      function give(self) -> str or none { return self.text }\n}\n";
        one_owner_ok(&format!(
            "{holder}function peek(h: Holder[str]) -> str or none {{ return h.give() }}\nprint(peek(Name(\"a\")))\n"
        ));
        assert_eq!(
            one_owner_error(&format!("{holder}function peek(h: Holder[list[int]]) -> list[int] or none {{ return h.give() }}\n")),
            "only the owner can call `give`, which takes out or replaces something the object owns (`Slot` is a \
             `Holder[list[int]]`, and its `give` does): somebody else may have been lent it. Here `h` only looks at it"
        );
        // An `owned interface` copy takes only an `owned class`.
        assert_eq!(
            error("owned interface Holder[T] {\n    function give(self) -> T or none\n}\n\
                   class Name is Holder[str] {\n    function give(self) -> str or none { return none }\n}"),
            "`Name` is not an owned class, and `Holder[str]` is an `owned interface`, whose objects each have one owner. \
             Write `owned class Name`, or take `owned` off `Holder`"
        );
    }

    #[test]
    fn what_a_generic_interface_says_when_it_cannot() {
        let source = "interface Source[T] {\n    function next(self) -> T or none\n}\n";
        assert_eq!(
            error(&format!("{source}class Words is Source[str] {{\n    function next(self) -> int or none {{ return 1 }}\n}}")),
            "`next` of `Words` is `function next(self) -> int or none`, and `Source[str]` asks for \
             `function next(self) -> str or none`. Write the two the same"
        );
        assert_eq!(
            error(&format!("{source}class Words is Source {{\n    function next(self) -> int or none {{ return 1 }}\n}}")),
            "`Source` has type parameters, so a class says which `Source` it is: `class Words is Source[int]`"
        );
        assert_eq!(
            error("interface Shape {\n    function area(self) -> float\n}\nclass C is Shape[int] {\n    function area(self) -> float { return 1.0 }\n}"),
            "`Shape` has no type parameters, so it is written without `[ ]`: `class C is Shape`"
        );
        assert_eq!(
            error(&format!("{source}class W is Source[int, str] {{\n    function next(self) -> str or none {{ return none }}\n}}")),
            "`Source` takes 1 type in `[ ]`, as in `Source[int]`, but 2 were written"
        );
        assert_eq!(
            error(&format!("{source}let s = Source()")),
            "`Source` is an interface, so there is nothing to build. Build an object of a class that is a `Source`"
        );
        assert_eq!(
            error(&format!("{source}let s: Source[int] = Source()")),
            "`Source` is an interface, so there is nothing to build. Build an object of a class that is a `Source`"
        );
        assert_eq!(
            error(&format!("{source}let s = Source")),
            "`Source` is an interface, and is written where a type goes, with its types: `Source[int]`"
        );
        assert_eq!(
            error(&format!("{source}function f(s: Source) {{ }}")),
            "`Source` takes 1 type in `[ ]`, as in `Source[int]`, but 0 were written"
        );
    }

    // ----- The board and the clock -----

    #[test]
    fn pins_are_checked() {
        ok("from board import led, pin\nled.on()\nlet relay: Pin = pin(7)\nlet lit: bool = relay.is_on()\nlet n: int = led.number");
        ok("import board\nfunction flash(light: Pin) { light.toggle() }\nflash(board.led)\nlet pins: list[Pin] = [board.pin(1), board.led]");
        strict_ok(include_str!("../examples/blink.zeno"));
        strict_ok(include_str!("../examples/signal.zeno"));
        assert_eq!(error("from board import pin\npin(\"a\")"), "the number of a pin should be int, but this is str");
        assert_eq!(error("from board import led\nled.blink()"), "Pin has no field or method `blink`");
        assert_eq!(error("from board import led\nled.on(1)"), "`on` takes 0 arguments, but 1 was given");
        assert_eq!(error("from board import led\nlet n: int = led"), "`n` should be int, but this is Pin");
        assert_eq!(error("from board import led\nprint(led == 13)"), "`==` between Pin and int is always false");
        assert_eq!(error("from board import button"), "module `board` has no `button`");
    }

    #[test]
    fn an_input_pin_is_read_and_an_output_is_driven() {
        ok("from board import input_pin, pull_up, pull_down\nlet a: InputPin = input_pin(2, pull_up)\n\
            let b = input_pin(3, pull_down)\nlet c = input_pin(4)\nlet high: bool = a.is_high() and b.is_low()\n\
            let n: int = c.number\nlet how: Pull = pull_up\nprint(how == pull_down, a == b, how)");
        ok("import board\nfunction made(n: int, how: Pull) -> InputPin { return board.input_pin(n, how) }\n\
            let pins: list[InputPin] = [made(1, board.pull_up)]\nlet spare: InputPin or none = none");
        assert_eq!(
            error("from board import input_pin\ninput_pin(2).on()"),
            "an InputPin is read and not driven, so it has no `on`. Read it with `is_high()` or `is_low()`. \
             A pin the program drives is made with `pin( )`"
        );
        assert_eq!(
            error("from board import led\nprint(led.is_low())"),
            "`is_low` reads an input, and this Pin is driven by the program. Ask it what it was driven to with \
             `is_on()`. A pin the program reads is made with `input_pin( )`"
        );
        assert_eq!(
            error("from board import input_pin\nlet b = input_pin(2, \"up\")"),
            "what holds an input pin is `pull_up` or `pull_down`, from the `board` module, and this is str. \
             With nothing there, nothing pulls the pin"
        );
        assert_eq!(error("from board import input_pin, pin\nlet b: Pin = input_pin(2)"), "`b` should be Pin, but this is InputPin");
        assert_eq!(error("from board import input_pin, pin\nprint(input_pin(2) == pin(2))"), "`==` between InputPin and Pin is always false");
        assert_eq!(error("from board import pull_up\nlet s: str = pull_up"), "`s` should be str, but this is Pull");
        assert_eq!(error("from board import input_pin\ninput_pin(2, 3, 4)"), "`input_pin` takes 1 to 2 arguments, but 3 were given");
        assert_eq!(error("from board import input_pin, pull_up\ninput_pin(pull_up)"), "the number of a pin should be int, but this is Pull");
        assert_eq!(error("from board import input_pin\ninput_pin(2).is_high(1)"), "`is_high` takes 0 arguments, but 1 was given");
    }

    #[test]
    fn an_analog_pin_reads_a_voltage() {
        ok("from board import analog_pin\nlet sensor: AnalogPin = analog_pin(0)\nlet level: V = sensor.voltage()\n\
            let count: int = sensor.count()\nlet most: int = sensor.max_count\nlet n: int = sensor.number\n\
            let low: bool = level < 1200 mV");
        ok("import board\nfunction read(of: AnalogPin) -> mV { return of.voltage().mV }\nread(board.analog_pin(1))");
        assert_eq!(
            error("from board import analog_pin\nanalog_pin(0).is_high()"),
            "an AnalogPin has no `is_high`. It has `voltage()`, `count()`, `max_count` and `number`. \
             A pin read as high or low is made with `input_pin( )`"
        );
        assert_eq!(
            error("from board import analog_pin\nlet v: float = analog_pin(0).voltage()"),
            "`v` should be float, but this is a quantity in V"
        );
        assert_eq!(
            error("from board import analog_pin\nlet v: mV = analog_pin(0).voltage()"),
            "`v` should be a quantity in mV, but this is a quantity in V. Convert it with `.mV`"
        );
        assert_eq!(error("from board import analog_pin\nanalog_pin(0).count(1)"), "`count` takes 0 arguments, but 1 was given");
        assert_eq!(error("from board import analog_pin\nanalog_pin(0.5)"), "the number of a pin should be int, but this is float");
        assert_eq!(error("from board import analog_pin, pin\nlet p: Pin = analog_pin(0)"), "`p` should be Pin, but this is AnalogPin");
    }

    #[test]
    fn a_pwm_pin_has_a_frequency_and_a_duty() {
        ok("from board import pwm_pin\nlet lamp: PwmPin = pwm_pin(9, 1 kHz)\nlamp.duty(0.25)\nlamp.pulse(1500 us)\n\
            let n: int = lamp.number\nlet rate: Hz = 1 / 20 ms\nlet servo = pwm_pin(10, rate)\nservo.pulse(0.002 s)");
        ok("import board\nfunction dim(light: PwmPin, share: float) { light.duty(share) }\ndim(board.pwm_pin(3, 500 Hz), 0.1)");
        assert_eq!(
            error("from board import pwm_pin\npwm_pin(9, 1000)"),
            "the frequency of a PWM pin has a unit, as in `pwm_pin(9, 1 kHz)`, and this is int"
        );
        assert_eq!(
            error("from board import pwm_pin\npwm_pin(9, 1 ms)"),
            "the frequency of a PWM pin has a unit, as in `pwm_pin(9, 1 kHz)`, and this is a quantity in ms"
        );
        assert_eq!(error("from board import pwm_pin\npwm_pin(9)"), "`pwm_pin` takes 2 arguments, but 1 was given");
        assert_eq!(
            error("from board import pwm_pin\npwm_pin(9, 1 kHz).pulse(1500)"),
            "`pulse` needs a time with a unit, such as `1500 us`, and this is int"
        );
        assert_eq!(
            error("from board import pwm_pin\npwm_pin(9, 1 kHz).pulse(50 Hz)"),
            "`pulse` needs a time with a unit, such as `1500 us`, and this is a quantity in Hz"
        );
        assert_eq!(
            error("from board import pwm_pin\npwm_pin(9, 1 kHz).on()"),
            "a PwmPin has no `on`. It has `duty(share)`, `pulse(time)` and `number`. \
             All the time on is `duty(1.0)`, and off is `duty(0.0)`"
        );
        assert_eq!(error("from board import pwm_pin\npwm_pin(9, 1 kHz).duty()"), "`duty` takes 1 argument, but 0 were given");
        assert_eq!(error("from board import pwm_pin, pin\nlet p: Pin = pwm_pin(9, 1 kHz)"), "`p` should be Pin, but this is PwmPin");
    }

    #[test]
    fn a_ticker_has_a_period_with_a_unit() {
        ok("from board import ticker\nlet beat: Ticker = ticker(10 ms)\nlet came: int = beat.wait()\nlet more: int = beat.ticks()\n\
            beat.restart()\nlet period: ms = beat.period\nlet long = ticker(1 h)\nlet short = ticker(250 us)");
        ok("import board\nfunction keep(beat: Ticker) -> int { return beat.wait() }\nkeep(board.ticker(1 s / 50))");
        assert_eq!(
            error("from board import ticker\nticker(10)"),
            "the period of a ticker has a unit, as in `ticker(10 ms)`, and this is int"
        );
        assert_eq!(
            error("from board import ticker\nticker(100 Hz)"),
            "the period of a ticker has a unit, as in `ticker(10 ms)`, and this is a quantity in Hz"
        );
        assert_eq!(error("from board import ticker\nticker()"), "`ticker` takes 1 argument, but 0 were given");
        assert_eq!(
            error("from board import ticker\nticker(1 s).stop()"),
            "a Ticker has no `stop`. It has `wait()`, `ticks()`, `restart()` and `period`"
        );
        assert_eq!(error("from board import ticker\nticker(1 s).wait(5 ms)"), "`wait` takes 0 arguments, but 1 was given");
        assert_eq!(
            error("from board import ticker\nif ticker(1 s).ticks() { print(1) }"),
            "a condition must be true or false, but this is int"
        );
        assert_eq!(
            error("from board import ticker\nlet period: s = ticker(1 s).period"),
            "`period` should be a quantity in s, but this is a quantity in ms. Convert it with `.s`"
        );
        assert_eq!(error("from board import ticker\nlet t: Timer = ticker(1 s)"), "`t` should be Timer, but this is Ticker");
    }

    #[test]
    fn an_interrupt_counts_the_edges_of_an_input() {
        ok("from board import input_pin, pull_up, falling, rising, either\nlet button = input_pin(2, pull_up)\n\
            let press: Interrupt = button.interrupt(falling)\nlet came: int = press.wait()\nlet more: int = press.edges()\n\
            let n: int = press.number\nlet which: Edge = rising\nlet kinds: list[Edge] = [rising, falling, either]");
        ok("import board\nfunction watch(what: InputPin, which: Edge) -> Interrupt { return what.interrupt(which) }\n\
            watch(board.input_pin(3), board.rising)");
        assert_eq!(
            error("from board import input_pin\ninput_pin(2).interrupt(\"falling\")"),
            "an interrupt counts `rising`, `falling` or `either` edges, from the `board` module, and this is str"
        );
        assert_eq!(
            error("from board import input_pin, pull_up\ninput_pin(2).interrupt(pull_up)"),
            "an interrupt counts `rising`, `falling` or `either` edges, from the `board` module, and this is Pull"
        );
        assert_eq!(error("from board import input_pin\ninput_pin(2).interrupt()"), "`interrupt` takes 1 argument, but 0 were given");
        assert_eq!(
            error("from board import led, falling\nled.interrupt(falling)"),
            "`interrupt` reads an input, and this Pin is driven by the program. Ask it what it was driven to with \
             `is_on()`. A pin the program reads is made with `input_pin( )`"
        );
        assert_eq!(
            error("from board import input_pin, falling\ninput_pin(2).interrupt(falling).is_low()"),
            "an Interrupt has no `is_low`. It has `wait()`, `edges()` and `number`. \
             The level of its pin is read from the InputPin, with `is_high()` or `is_low()`"
        );
        assert_eq!(
            error("from board import input_pin, falling\ninput_pin(2).interrupt(falling).wait(5 ms)"),
            "`wait` takes 0 arguments, but 1 was given"
        );
        assert_eq!(error("from board import falling\nlet s: str = falling"), "`s` should be str, but this is Edge");
        assert_eq!(error("from board import rising, pull_up\nprint(rising == pull_up)"), "`==` between Edge and Pull is always false");
        assert_eq!(
            error("from board import input_pin, rising\nlet p: InputPin = input_pin(2).interrupt(rising)"),
            "`p` should be InputPin, but this is Interrupt"
        );
    }

    #[test]
    fn a_serial_port_has_a_speed_in_baud() {
        ok("from board import serial\nlet port: Serial = serial(1, 115200 baud)\nport.write(\"hi\\n\")\n\
            let frame: list[u8] = [1, 2]\nport.write_bytes(frame)\nport.write_bytes([3, 4])\n\
            let byte: u8 or none = port.read_byte()\nlet ready: bool = port.has_byte()\nlet n: int = port.number");
        ok("import board\nfunction talk(to: Serial) { to.write(\"x\") }\ntalk(board.serial(0, 9600 baud))");
        assert_eq!(
            error("from board import serial\nlet p = serial(1, 9600)"),
            "the speed of a serial port is in baud, as in `serial(1, 115200 baud)`, and this is int"
        );
        assert_eq!(
            error("from board import serial\nlet p = serial(1, 9600 Hz)"),
            "the speed of a serial port is in baud, as in `serial(1, 115200 baud)`, and this is a quantity in Hz"
        );
        assert_eq!(
            error("from board import serial\nserial(1, 9600 baud).on()"),
            "a Serial has no `on`. It has `write(text)`, `write_bytes(bytes)`, `read_byte()`, `has_byte()` and `number`"
        );
        assert_eq!(error("from board import serial\nserial(1, 9600 baud).write(65)"), "the text given to `write` should be str, but this is int");
        assert_eq!(
            error("from board import serial\nserial(1, 9600 baud).write_bytes(\"hi\")"),
            "the bytes given to `write_bytes` should be list[u8], but this is str"
        );
        assert_eq!(
            error("from board import serial\nlet b: u8 = serial(1, 9600 baud).read_byte()"),
            "`b` should be u8, but this is u8 or none. Check it with `is not none` first"
        );
        // A class of the program's own called `Pin` comes first.
        ok("class Pin {\n n: int\n}\nlet p: Pin = Pin(1)\nprint(p.n)");
    }

    #[test]
    fn the_clock_is_checked() {
        ok("from time import sleep, now\nlet start: ms = now()\nsleep(1 s)\nsleep(2 min)\nlet took: ms = now() - start");
        assert_eq!(error("from time import sleep\nsleep(3 V)"), "`sleep` needs a time with a unit, such as `500 ms`, but this is a quantity in V");
        assert_eq!(error("from time import now\nnow(1)"), "`now` takes 0 arguments, but 1 was given");
        assert_eq!(error("from time import now\nlet t: s = now()"), "`t` should be a quantity in s, but this is a quantity in ms. Convert it with `.s`");
    }

    // ----- Classes -----

    #[test]
    fn classes_are_checked() {
        let point = "class Point {\n x: float\n y: float = 0.0\n function scaled(self, by: float) -> Point {\n  return Point(self.x * by, self.y * by)\n }\n}\n";
        ok(&format!("{point}let p = Point(1.0)\nlet q: Point = p.scaled(2.0)\np.x = 3.0\nlet x: float = q.x"));
        assert_eq!(error(&format!("{point}Point(\"one\")")), "`x` of `Point` should be float, but this is str");
        assert_eq!(error(&format!("{point}Point()")), "`Point` needs a value for `x`");
        assert_eq!(error(&format!("{point}print(Point(1.0).z)")), "`Point` has no field or method `z`");
        assert_eq!(error(&format!("{point}let p = Point(1.0)\np.z = 2.0")), "`Point` has no field `z`");
        assert_eq!(error(&format!("{point}let p = Point(1.0)\np.x = \"far\"")), "the new value should be float, but this is str");
        assert_eq!(error(&format!("{point}Point(1.0).scaled(\"big\")")), "`by` of `scaled` should be float, but this is str");
        assert_eq!(error(&format!("{point}Point(1.0).scaled()")), "`scaled` needs a value for `by`");
        assert_eq!(error(&format!("{point}let n: int = Point(1.0)")), "`n` should be int, but this is Point");
    }

    #[test]
    fn mistakes_inside_a_class() {
        assert_eq!(error("class A {\n n: int = \"no\"\n}"), "`n` should be int, but this is str");
        assert_eq!(error("class A {\n n: int\n function get(self) -> str { return self.n }\n}"), "the returned value should be str, but this is int");
        assert_eq!(error("class A {\n function lost() { }\n}"), "the method `lost` needs `self` as its first parameter");
        assert_eq!(error("class A {\n other: Missing\n}"), "there is no type called `Missing`");
        // Two classes can mention each other, whichever comes first.
        ok("class A {\n b: B or none\n}\nclass B {\n a: A or none\n}");
    }

    // ----- Enums and match -----

    const SHAPE: &str = "enum Shape {\n Dot\n Circle(radius: float)\n Rect(w: float, h: float)\n}\n";

    #[test]
    fn enums_are_checked() {
        ok(&format!("{SHAPE}let a: Shape = Circle(1.0)\nlet b: Shape = Dot\nlet c: Shape = Shape.Rect(1.0, h=2.0)\nlet d: list[Shape] = [Dot, Circle(2.0)]"));
        assert_eq!(error(&format!("{SHAPE}Circle(\"big\")")), "`radius` of `Circle` should be float, but this is str");
        assert_eq!(error(&format!("{SHAPE}Circle()")), "`Circle` needs a value for `radius`");
        assert_eq!(error(&format!("{SHAPE}print(Shape.Square)")), "enum `Shape` has no case `Square`");
        assert_eq!(error(&format!("{SHAPE}let n: int = Dot")), "`n` should be int, but this is Shape");
    }

    #[test]
    fn match_must_cover_every_case() {
        let area = "function area(s: Shape) -> float {\n match s {\n  case Circle(r) { return r }\n  case Rect(w, h) { return w * h }\n";
        ok(&format!("{SHAPE}{area}  case Dot {{ return 0.0 }}\n }}\n}}"));
        ok(&format!("{SHAPE}{area}  case _ {{ return 0.0 }}\n }}\n}}"));
        ok(&format!("{SHAPE}{area}  case other {{ return 0.0 }}\n }}\n}}"));
        assert_eq!(
            errors(&format!("{SHAPE}{area} }}\n}}")),
            vec!["this `match` does not handle Dot. Add a `case` for it, or a final `case _`"]
        );
        assert_eq!(
            error(&format!("{SHAPE}match Circle(1.0) {{\n case Dot {{ }}\n}}")),
            "this `match` does not handle Circle, Rect. Add a `case` for it, or a final `case _`"
        );
        // A case that only matches some values does not cover the whole case.
        assert_eq!(
            error(&format!("{SHAPE}match Dot {{\n case Dot {{ }}\n case Rect(w, h) {{ }}\n case Circle(1.0) {{ }}\n}}")),
            "this `match` does not handle Circle. Add a `case` for it, or a final `case _`"
        );
    }

    #[test]
    fn patterns_are_checked() {
        assert_eq!(error(&format!("{SHAPE}match Dot {{\n case Circle(a, b) {{ }}\n case _ {{ }}\n}}")), "`Circle` carries 1 value, but this pattern has 2");
        assert_eq!(error(&format!("{SHAPE}match Dot {{\n case Nope(x) {{ }}\n case _ {{ }}\n}}")), "`Nope` is not an enum case that carries values");
        assert_eq!(error(&format!("{SHAPE}match 5 {{\n case Dot {{ }}\n case _ {{ }}\n}}")), "`Dot` is a case of Shape, which can never match int");
        assert_eq!(error("match 5 {\n case \"five\" { }\n case _ { }\n}"), "this case is str, which can never match int");
        assert_eq!(error("match 5 {\n case 1 { }\n}"), "this `match` needs a final `case _` to handle every other value");
        // An enum that may be `none` needs a `case none` as well.
        let maybe = format!("{SHAPE}function pick(n: int) -> Shape or none {{\n if n > 0 {{ return Dot }}\n return none\n}}\n");
        assert_eq!(
            error(&format!("{maybe}match pick(1) {{\n case Dot {{ }}\n case Circle(r) {{ }}\n case Rect(w, h) {{ }}\n}}")),
            "this `match` does not handle `none`. Add a `case` for it, or a final `case _`"
        );
        ok(&format!("{maybe}match pick(1) {{\n case none {{ }}\n case Dot {{ }}\n case Circle(r) {{ }}\n case Rect(w, h) {{ }}\n}}"));
        // The values a case carries have the types the enum gave them.
        assert_eq!(
            error(&format!("{SHAPE}match Dot {{\n case Circle(r) {{ let s: str = r }}\n case _ {{ }}\n}}")),
            "`s` should be str, but this is float"
        );
    }

    // ----- none -----

    #[test]
    fn none_must_be_checked_before_use() {
        let find = "function find(id: int) -> str or none {\n if id == 1 { return \"partha\" }\n return none\n}\nlet user = find(1)\n";
        assert_eq!(
            error(&format!("{find}print(user.upper())")),
            "this is str or none. Check it with `is not none` before using `.upper`"
        );
        assert_eq!(
            error(&format!("{find}let name: str = user")),
            "`name` should be str, but this is str or none. Check it with `is not none` first"
        );
        assert_eq!(error(&format!("{find}print(user + \"!\")")), "`+` does not work between str or none and str. Check for `none` first");
        ok(&format!("{find}if user is not none {{\n print(user.upper())\n}}"));
        ok(&format!("{find}if user is none {{\n print(\"nobody\")\n}} else {{\n print(user.upper())\n}}"));
        ok(&format!("{find}print(user is not none and user.upper() == \"PARTHA\")"));
        ok(&format!("{find}print(user is none or user.upper() == \"PARTHA\")"));
        ok(&format!("{find}if not user is none {{\n print(user.upper())\n}}"));
    }

    #[test]
    fn an_early_exit_settles_the_rest_of_the_block() {
        let source = "function shout(text: str or none) -> str {\n if text is none {\n  return \"\"\n }\n return text.upper()\n}";
        ok(source);
        ok("function first(xs: list[str or none]) -> list[str] {\n return [x for x in xs if x is not none]\n}");
        assert_eq!(
            error("function shout(text: str or none) -> str {\n if text is none {\n  print(\"nothing\")\n }\n return text.upper()\n}"),
            "this is str or none. Check it with `is not none` before using `.upper`"
        );
    }

    #[test]
    fn a_new_value_changes_what_is_known() {
        let node = "class Node {\n value: int\n next: Node or none = none\n}\n";
        // Walking a linked list: `node` is checked by the loop, then moved on.
        ok(&format!(
            "{node}function total(head: Node or none) -> int {{\n var node = head\n var sum = 0\n while node is not none {{\n  sum += node.value\n  node = node.next\n }}\n return sum\n}}"
        ));
        // Filling in a missing value: both ways out of the `if` leave a str.
        ok("function f(given: str or none) -> str {\n var name = given\n if name is none { name = \"anon\" }\n return name\n}");
        ok("function f() -> int {\n var x: int or none = none\n x = 5\n return x + 1\n}");

        let gone = "`+` does not work between int or none and int. Check for `none` first";
        // Set back to none in a block inside the check.
        assert_eq!(error("var x: int or none = 5\nif x is not none {\n if x > 3 { x = none }\n print(x + 1)\n}"), gone);
        // Set back to none by an earlier trip round a loop.
        assert_eq!(error("var x: int or none = 5\nif x is not none {\n while true {\n  print(x + 1)\n  x = none\n }\n}"), gone);
        assert_eq!(
            error("function f(xs: list[int]) {\n var x: int or none = 1\n x = 2\n for y in xs {\n  print(x + y)\n  x = none\n }\n}"),
            gone
        );
        // Set back to none in a `try`, which may stop part way.
        assert_eq!(error("function f() -> int {\n var x: int or none = 1\n x = 2\n try { x = none } except { }\n return x + 1\n}"), gone);
        // The branch that carries on undid the check.
        assert_eq!(
            error("function f(x: int or none) -> int {\n var y = x\n if y is none { return 0 } else { y = none }\n return y + 1\n}"),
            gone
        );
    }

    #[test]
    fn after_case_none_the_value_is_known_to_be_there() {
        let find = "function find(id: int) -> str or none {\n if id == 1 { return \"partha\" }\n return none\n}\nlet user = find(1)\n";
        // The cases after `case none` know, as the `else` of
        // `if user is none` does: by a new name, and by the old one.
        strict_ok(&format!("{find}match user {{\n case none {{ print(0) }}\n case name {{ print(name.upper()) }}\n}}"));
        strict_ok(&format!("{find}match user {{\n case none {{ print(0) }}\n case _ {{ print(user.upper()) }}\n}}"));
        // A case in front of it does not.
        assert_eq!(
            error(&format!("{find}match user {{\n case \"x\" {{ print(user.upper()) }}\n case none {{ }}\n case _ {{ }}\n}}")),
            "this is str or none. Check it with `is not none` before using `.upper`"
        );
        // If `case none` always leaves, what follows the `match` knows too.
        let f = "function shout(user: str or none) -> str {\n match user {\n  case none { return \"\" }\n  case _ { }\n }\n return user.upper()\n}";
        strict_ok(f);
        // It does not if `case none` carries on, or if a later case gave
        // the variable a new value that may be `none`.
        assert_eq!(
            error(&format!("{find}match user {{\n case none {{ print(0) }}\n case _ {{ }}\n}}\nprint(user.upper())")),
            "this is str or none. Check it with `is not none` before using `.upper`"
        );
        assert_eq!(
            error(&format!(
                "{find}var who = find(2)\nfunction f() -> int {{\n match who {{\n  case none {{ return 0 }}\n  case _ {{ who = find(3) }}\n }}\n return len(who)\n}}"
            )),
            "`len` needs a str, a list or a dict, but this is str or none"
        );
        // A second `case none` can never match.
        assert_eq!(
            error(&format!("{find}match user {{\n case none {{ }}\n case none {{ }}\n case _ {{ }}\n}}")),
            "this case is none, which can never match str"
        );
    }

    #[test]
    fn none_only_goes_where_it_is_allowed() {
        ok("let a: int or none = none\nlet b: int or none = 5\nvar c: int or none = 1\nc = none");
        assert_eq!(error("let a: int = none"), "`a` should be int, but this is none");
        assert_eq!(error("function f(n: int) { }\nf(none)"), "`n` of `f` should be int, but this is none");
    }

    // ----- Errors and raises -----

    #[test]
    fn declared_raises_must_be_complete() {
        ok("function f(text: str) -> int raises ValueError { return int(text) }");
        ok("function f() raises ValueError { raise ValueError(\"no\") }");
        ok("function f() raises Error { raise IndexError(\"no\") }");
        assert_eq!(
            error("function f(n: int) raises ValueError {\n if n > 0 { raise ValueError(\"a\") }\n raise IndexError(\"b\")\n}"),
            "`f` can raise IndexError here but does not say so. Add `raises IndexError` to it, or catch the error"
        );
        let parse = "function parse(text: str) -> int raises ValueError { return int(text) }\n";
        assert_eq!(
            error(&format!("{parse}function load() -> int raises IndexError {{ return parse(\"1\") }}")),
            "`load` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        ok(&format!("{parse}function load() -> int raises IndexError {{\n try {{\n  return parse(\"1\")\n }} except ValueError {{\n  return 0\n }}\n}}"));
        assert_eq!(error("function f() raises Oops { }"), "`Oops` is not a kind of error");
        assert_eq!(error("try { } except Oops { }"), "`Oops` is not a kind of error");
        assert_eq!(error("raise 5"), "only an error can be raised, and this is int");
    }

    #[test]
    fn raises_left_out_is_worked_out() {
        // `parse` does not say what it raises, so that is inferred, and
        // `load`, which does say, must account for it.
        let source = "function parse(text: str) -> int { return int(text) }\nfunction load() -> int raises IndexError { return parse(\"1\") }";
        assert_eq!(
            error(source),
            "`load` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        ok("function parse(text: str) -> int { return int(text) }\nprint(parse(\"1\"))");
    }

    #[test]
    fn caught_errors_have_fields() {
        ok("try {\n raise ValueError(\"bad\")\n} except ValueError as e {\n let m: str = e.message\n} except Error as e {\n print(e.kind)\n}");
        ok("class Timeout {\n after: int\n}\ntry {\n raise Timeout(3)\n} except Timeout as t {\n let n: int = t.after\n}");
    }

    // ----- Strict mode -----

    #[test]
    fn strict_mode_needs_every_type_to_be_known() {
        assert_eq!(strict_error("function add(a, b: int) -> int { return b }"), "strict mode: the parameter `a` of `add` needs a type. It could not be worked out from how it is used"
        );
        assert_eq!(
            strict_error("class Box {\n item\n}"),
            "strict mode: `item` in `Box` needs a type. It could not be worked out from how it is used"
        );
        assert_eq!(
            strict_error("let xs = []"),
            "strict mode: the type of `xs` cannot be worked out. Give it one, as in `xs: list[int]`"
        );
        assert_eq!(strict_error("let x: any = 1"), "`any` is not allowed in strict mode");
        assert_eq!(
            strict_error("let mixed = [1, \"two\"]"),
            "strict mode: the type of `mixed` cannot be worked out. Give it one, as in `mixed: list[int]`"
        );
        strict_ok("let xs: list[int] = []\nlet n = 3\nlet names = [\"a\", \"b\"]\nfunction double(n: int) { return n * 2 }\nlet d: int = double(n)");
    }

    #[test]
    fn strict_mode_needs_raises_to_be_written() {
        assert_eq!(
            strict_error("function parse(text: str) -> int { return int(text) }"),
            "`parse` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        strict_ok("function parse(text: str) -> int raises ValueError { return int(text) }");
    }

    // ----- Function types -----

    #[test]
    fn function_types_can_be_written() {
        let twice = "function twice(f: function(int) -> int, x: int) -> int { return f(f(x)) }\n";
        ok(&format!("{twice}function inc(n: int) -> int {{ return n + 1 }}\nlet r: int = twice(inc, 1)"));
        ok(&format!("{twice}let r: int = twice(function(k) {{ return k * 2 }}, 5)"));
        assert_eq!(
            error(&format!("{twice}function shout(s: str) -> str {{ return s.upper() }}\ntwice(shout, 1)")),
            "`f` of `twice` should be function(int) -> int, but the parameter `s` of this function is str"
        );
        assert_eq!(
            error("let g: function(int) -> str = function(k: int) -> int { return k }"),
            "`g` should be function(int) -> str, but this function returns int"
        );
        assert_eq!(
            error(&format!("{twice}twice(5, 1)")),
            "`f` of `twice` should be function(int) -> int, but this is int"
        );
        // Calling through a function type is checked like any other call.
        assert_eq!(
            error("function apply(f: function(int) -> int) -> int { return f(\"x\") }"),
            "an argument should be int, but this is str"
        );
    }

    // ----- Units -----

    #[test]
    fn units_are_types() {
        ok("function wait(d: ms) { }\nwait(500 ms)\nlet v: mV = 20 mV");
        assert_eq!(
            error("function wait(d: ms) { }\nwait(3 s)"),
            "`d` of `wait` should be a quantity in ms, but this is a quantity in s. Convert it with `.ms`"
        );
        ok("function wait(d: ms) { }\nlet t = 3 s\nwait(t.ms)\nwait((2 min).ms)");
        // Units of one kind work together, and the finer one is the answer.
        ok("let total: ms = 1 s + 500 ms\nlet late: bool = 1500 ms > 1 s\nlet same: bool = 1 s == 1000 ms\nlet ratio: float = 1 h / 1 min");
        assert_eq!(error("let x = 1 s + 1 V"), "`+` does not work between a quantity in s and a quantity in V");
        assert_eq!(
            error("let v = (1 s).V"),
            "a quantity in s cannot be turned into V. They measure different things"
        );
        assert_eq!(
            error("function wait(d: ms) { }\nwait(3)"),
            "`d` of `wait` should be a quantity in ms, but this is int"
        );
        assert_eq!(
            error("let t = 5 sec"),
            "`sec` is not a unit Zeno knows. Some that it does: s, ms, us, Hz, V, mV, A, mA, W, degC"
        );
    }

    #[test]
    fn units_make_units_from_a_closed_list() {
        // Decision 43: the answer is in the unit of its kind with no prefix.
        ok("let p: W = 3.3 V * 20 mA\nlet r: ohm = 5 V / 2 mA\nlet i: A = 12 V / (4.7 kohm)\nlet v: V = 2 mA * 1 kohm");
        ok("let e: J = 5 W * 2 h\nlet p: W = 1 kWh / 1 h\nlet t: s = 1 kWh / 5 W\nlet f: Hz = 1 / 2 ms\nlet t2: s = 1 / 1 kHz");
        ok("let cycles: float = 10 ms * 1 kHz\nlet mw: mW = (3.3 V * 20 mA).mW");
        let list = "V * A is W, A * ohm is V, W * s is J, V / A is ohm, V / ohm is A, W / V is A, J / s is W, 1 / s is Hz";
        assert_eq!(
            error("let a = 2 m * 3 m"),
            format!("`*` does not work between a quantity in m and a quantity in m. The units Zeno makes from others are {list}")
        );
        assert_eq!(
            error("let a = 1 / 2 V"),
            format!("`/` does not work between int and a quantity in V. The units Zeno makes from others are {list}")
        );
        assert_eq!(
            error("let p: mW = 3.3 V * 20 mA"),
            "`p` should be a quantity in mW, but this is a quantity in W. Convert it with `.mW`"
        );
    }

    // ----- Working out parameters and fields -----

    #[test]
    fn untyped_parameters_are_worked_out_from_the_calls() {
        assert_eq!(
            error("function add(a, b) { return a + b }\nlet s: str = add(1, 2)"),
            "`s` should be str, but this is int"
        );
        // What one function learns is passed on to the next.
        assert_eq!(
            error("function a(x) { return b(x) }\nfunction b(y) { return y + 1 }\nlet s: str = a(1)"),
            "`s` should be str, but this is int"
        );
        // A mistake shows up inside the function, where the value is used.
        assert_eq!(error("function f(a) { return a.upper() }\nf(1)"), "int has no field or method `upper`");
        // `none` at one call and an int at another makes `int or none`.
        strict_ok("function h(a) {\n if a is none { return 0 }\n return a + 1\n}\nprint(h(1), h(none))");
        // A default value counts as a use.
        assert_eq!(error("function f(a = 3) { return a }\nlet s: str = f()"), "`s` should be str, but this is int");
    }

    #[test]
    fn an_error_on_a_worked_out_parameter_names_the_call() {
        let program = |source: &str| {
            let tokens = crate::lexer::lex(source).expect("lexes");
            crate::parser::parse(tokens).expect("parses")
        };
        let found = super::check(&program("function f(a) {\n let n: str = 1\n return a.upper()\n}\n\nf(1)"), false);
        assert_eq!(found.len(), 2);
        // The mistake that has nothing to do with `a` says nothing about it.
        assert_eq!(found[0].note, None);
        assert_eq!(
            found[1].note.as_deref(),
            Some("`a` was worked out to be int from line 6. Give `a` a type to have each call checked against it instead")
        );
        // With the type written, the call is where the error is.
        let found = super::check(&program("function f(a: str) {\n return a.upper()\n}\n\nf(1)"), false);
        assert_eq!((found[0].span.line, &found[0].note), (5, &None));
    }

    #[test]
    fn calls_that_disagree_leave_a_parameter_dynamic() {
        ok("function id(x) { return x }\nlet a: int = id(1)\nlet b: str = id(\"s\")");
        assert_eq!(
            strict_error("function id(x) { return x }\nprint(id(1), id(\"s\"))"),
            "strict mode: the parameter `x` of `id` needs a type. It could not be worked out from how it is used"
        );
        // Never called, so there is nothing to go on.
        ok("function f(x) { return x.anything }");
        // Stored in a variable of unknown type, it may be called with anything.
        ok("function f(x) { return x.upper() }\nlet fs: list[any] = [f]\nprint(f(1))");
    }

    #[test]
    fn untyped_fields_are_worked_out_from_what_is_stored() {
        assert_eq!(
            error("class P {\n x\n}\nlet p = P(1)\np.x = 2\nlet s: str = p.x"),
            "`s` should be str, but this is int"
        );
        ok("class P {\n x\n}\nlet p = P(1)\np.x = \"s\"\nlet n: int = p.x");
        strict_ok("class C {\n total\n function add(self, n) { self.total = self.total + n }\n}\nlet c = C(0)\nc.add(5)\nlet t: int = c.total");
    }

    #[test]
    fn recursive_functions_have_their_return_type_worked_out() {
        let fact = "function fact(n) {\n if n <= 1 { return 1 }\n return n * fact(n - 1)\n}\n";
        strict_ok(&format!("{fact}let x: int = fact(5)"));
        assert_eq!(error(&format!("{fact}let s: str = fact(5)")), "`s` should be str, but this is int");
        let even = "function ev(n) {\n if n == 0 { return true }\n return od(n - 1)\n}\nfunction od(n) {\n if n == 0 { return false }\n return ev(n - 1)\n}\n";
        assert_eq!(error(&format!("{even}let s: str = ev(4)")), "`s` should be str, but this is bool");
    }

    #[test]
    fn an_int_is_not_quietly_made_a_float() {
        // Arithmetic mixes the two, but a function that returns an int on
        // one path and a float on another does not have one return type.
        strict_ok("function half(n: int) { return n / 2 }\nlet h: float = half(3) + 1");
        assert_eq!(
            strict_error("function pick(n: int) {\n if n > 0 { return 1 }\n return 2.5\n}\nlet p = pick(1)"),
            "strict mode: `pick` returns different types (int, float). Make them one type, as in `float(n)` or `1.0`"
        );
        strict_ok("function pick(n: int) {\n if n > 0 { return 1.0 }\n return 2.5\n}\nlet p: float = pick(1)");
    }

    // ----- Strict in the file -----

    #[test]
    fn a_file_can_ask_for_strict_mode() {
        ok("let xs = []");
        assert_eq!(
            error("strict\nlet xs = []"),
            "strict mode: the type of `xs` cannot be worked out. Give it one, as in `xs: list[int]`"
        );
    }

    // ----- Owned values -----

    /// A few declarations the tests below all use.
    const OWNED: &str = "owned class Packet {
    id: int
    data: owned list[int]
    function size(self) -> int { return len(self.data) }
}
function make() -> Packet { return Packet(1, [1, 2]) }
function send(p: take Packet) -> int { return p.id }
function look(p: Packet) -> int { return p.id }
function total(xs: owned list[int]) -> int { return len(xs) }
";

    fn owned_ok(source: &str) {
        strict_ok(&format!("{OWNED}{source}"));
    }

    fn owned_error(source: &str) -> String {
        strict_error(&format!("{OWNED}{source}"))
    }

    const GONE: &str = "holds nothing now. Use it before it goes, or give it only to what looks: a parameter without `take`";

    #[test]
    fn an_owned_value_can_be_made_lent_and_given_away() {
        owned_ok("let p = make()\nprint(look(p), p.size(), p.id, p.data[0], p)\nprint(send(p))");
        owned_ok("let xs: owned list[int] = [1, 2]\nprint(total(xs), len(xs), xs)\nxs.append(3)\nfor x in xs { print(x) }");
        // A shared list can be lent too: anyone may look at it.
        owned_ok("let xs = [1, 2]\nprint(total(xs), total([3]))");
        owned_ok("let p = make()\nlet q = p\nprint(send(q))");
        // An owned value nobody keeps is simply freed.
        owned_ok("make()\nPacket(2, [])");
        owned_ok("function build(n: int) -> owned list[int] {\n let xs: owned list[int] = [i for i in range(n)]\n return xs\n}\nlet ys = build(3)\nprint(ys)");
    }

    #[test]
    fn a_value_that_was_given_away_cannot_be_used() {
        assert_eq!(owned_error("let p = make()\nsend(p)\nprint(p.id)"), format!("`p` was given away on line 11 and {GONE}"));
        assert_eq!(owned_error("let p = make()\nlet q = p\nlet r = p"), format!("`p` was given away on line 11 and {GONE}"));
        // Given away on one path is not there afterwards on any path.
        assert_eq!(
            owned_error("let p = make()\nif p.id > 0 { send(p) }\nprint(look(p))"),
            format!("`p` was given away on line 11 and {GONE}")
        );
        // Unless that path never reaches what follows.
        owned_ok("function f(p: take Packet) -> int {\n if p.id > 0 { return send(p) }\n return look(p)\n}");
        // A `var` can be given a new value, and holds something again.
        owned_ok("var p = make()\nsend(p)\np = make()\nprint(p.id)");
        // An error can leave a `try` after the value has gone.
        assert_eq!(
            owned_error("class Bad { }\nfunction risky(p: take Packet) raises Bad { raise Bad() }\nlet p = make()\ntry { risky(p) } except Bad { print(p.id) }"),
            format!("`p` was given away on line 13 and {GONE}")
        );
    }

    #[test]
    fn giving_away_inside_a_loop() {
        let again = "is given away inside a loop, and the loop may come round again with nothing in it. \
                     Give it away after the loop, or just before a `break`";
        assert_eq!(owned_error("let p = make()\nfor i in range(3) { send(p) }"), format!("`p` {again}"));
        assert_eq!(owned_error("let p = make()\nwhile send(p) > 0 { }"), format!("`p` {again}"));
        assert_eq!(owned_error("let p = make()\nlet xs = [send(p) for i in range(3)]"), format!("`p` {again}"));
        assert_eq!(
            owned_error("let p = make()\nfor i in range(3) {\n if i == 1 {\n  send(p)\n  continue\n }\n}"),
            format!("`p` {again}")
        );
        // Just before a `break` is fine: the loop does not come round again.
        owned_ok("let p = make()\nwhile true {\n if p.id > 0 {\n  send(p)\n  break\n }\n}");
        // But it is gone after the loop.
        assert_eq!(
            owned_error("let p = make()\nwhile true {\n if p.id > 0 {\n  send(p)\n  break\n }\n}\nprint(p.id)"),
            format!("`p` was given away on line 13 and {GONE}")
        );
        // A value made inside the loop is a new one every time round.
        owned_ok("for i in range(3) {\n let p = make()\n send(p)\n}");
        owned_ok("var p = make()\nfor i in range(3) {\n send(p)\n p = make()\n}");
    }

    #[test]
    fn a_value_that_is_only_looked_at_cannot_be_kept() {
        assert_eq!(
            owned_error("function keep(p: Packet) -> Packet { return p }"),
            "`p` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. If it is a parameter, write `take` in front of its type to take the value over"
        );
        assert_eq!(
            owned_error("function pass_on(p: Packet) -> int { return send(p) }"),
            "`p` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. If it is a parameter, write `take` in front of its type to take the value over"
        );
        assert_eq!(
            owned_error("let p = make()\nlet d = p.data"),
            "`data` holds an owned value, which cannot be taken out of the object it is in. Use it where it is, or pass it to a parameter that looks"
        );
        assert_eq!(
            owned_error("function f(xs: list[int]) -> int { return len(xs) }\nlet ys: owned list[int] = [1]\nprint(f(ys))"),
            "`xs` of `f` is written `list[int]`, which is a shared list that the function may keep, and this list has one owner. \
             Write the parameter as `owned list[int]`: then it only looks, and any list can be passed to it"
        );
        // A method used as a value is a function that calls it, and
        // looks at the object or takes it as any such function does
        // (decision 56.9). A `let` at the top of the file is only looked
        // at.
        owned_ok("let p = make()\nlet m = p.size\nprint(m(), p.id)");
        owned_ok("function f() -> function() -> int {\n let p = make()\n return p.size\n}\nprint(f()())");
        assert!(owned_error("function f() -> int {\n let p = make()\n let m = p.size\n return m() + p.id\n}").contains(GONE));
        assert_eq!(
            owned_error("function f(p: Packet) -> function() -> int {\n return p.size\n}"),
            "this function is kept, so it takes `p`, but `p` is only looked at here: its owner still has it. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // One that is not a variable's is refused as before.
        assert_eq!(
            owned_error("let m = make().size"),
            "a method of a value that has one owner cannot be kept as a value. Call it here: `.size(...)`"
        );
        assert_eq!(
            owned_error("function reset(p: Packet) { p.data = [] }"),
            "only the owner can replace what `data` holds: somebody else may have been lent it. Here `p` only looks at it"
        );
        owned_ok("let p = make()\np.data = [5, 6]\nprint(p)");
    }

    #[test]
    fn a_line_cannot_use_a_value_and_give_it_away() {
        assert_eq!(
            owned_error("let p = make()\nprint(look(p) + send(p))"),
            "`p` is given away here, on a line that also uses it. Do the other thing on a line of its own first"
        );
        assert_eq!(owned_error("let p = make()\nprint(send(p) + look(p))"), format!("`p` was given away on line 11 and {GONE}"));
    }

    #[test]
    fn an_owned_value_with_no_name_belongs_to_its_statement() {
        // Made in the middle of a statement and only looked at: the
        // statement owns it. (These were refused once: "has no owner yet".)
        owned_ok("print(look(make()))");
        owned_ok("print(make().size(), make().id, len(make().data))");
        owned_ok("if look(make()) > 0 { print(1) }");
        owned_ok("for x in make().data { print(x) }");
        // It still cannot be kept by something that is not an owner.
        assert_eq!(
            owned_error("let xs = [1, 2]\nlet ys: owned list[int] = xs"),
            "`ys` should be owned list[int], but this is list[int]. Only a list made right here can become an owned one: \
             one written out, as in `[1, 2]` or `[x for x in xs]`, or what `split`, `map`, `filter` or `+` gives"
        );
        assert_eq!(
            owned_error("let data = make().data"),
            "`data` holds an owned value, which cannot be taken out of the object it is in. \
             Use it where it is, or pass it to a parameter that looks"
        );
    }

    #[test]
    fn a_list_that_was_just_made_can_start_out_owned() {
        owned_ok("let words: owned list[str] = \"a b\".split()\nprint(words)");
        owned_ok("let parts: owned list[str] = \"a,b\".split(\",\")\nprint(total([len(p) for p in parts]))");
        owned_ok(
            "function double(n: int) -> int { return n * 2 }\nfunction big(n: int) -> bool { return n > 1 }\n\
             let xs: owned list[int] = [1, 2]\nlet ys: owned list[int] = xs.map(double)\n\
             let zs: owned list[int] = xs.filter(big)\nlet all: owned list[int] = xs + ys + zs\nprint(total(all), total(xs + ys))",
        );
        owned_ok("function words(text: str) -> owned list[str] { return text.split() }\nprint(len(words(\"a b\")))");
        // `xs += ys` is `xs = xs + ys`: the list that `+` makes takes the
        // old one's place.
        owned_ok("var xs: owned list[int] = [1]\nxs += [2]\nxs += xs\nprint(xs)");
        assert_eq!(
            owned_error("var xs: owned list[int] = [1]\nxs += 2"),
            "`+` does not work between list[int] and int"
        );
    }

    #[test]
    fn an_owned_value_stays_in_the_function_that_holds_it() {
        let elsewhere = "holds an owned value, which belongs to the block that declares it. \
                         A function written elsewhere cannot reach it. Pass it to the function: a parameter looks at what it is given";
        assert_eq!(
            owned_error("function outer() -> int {\n let p = make()\n function peek() -> int { return p.id }\n return peek()\n}"),
            format!("`p` {elsewhere}")
        );
        assert_eq!(owned_error("var p = make()\nfunction swap() { p = make() }"), format!("`p` {elsewhere}"));
        // A `let` at the top of the file is the exception: it is there
        // until the program ends, so every function may look at it.
        owned_ok("let p = make()\nfunction peek() -> int { return p.id }\nprint(peek())");
        // A function written where a value goes may look at it when it
        // is passed along. Kept, it takes it, and a parameter that only
        // looks has nothing to give (decision 56.8).
        assert_eq!(
            owned_error("function f(p: Packet) -> function() -> int {\n return function() -> int { return p.id }\n}"),
            "this function is kept, so it takes `p`, but `p` is only looked at here: its owner still has it. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        owned_ok("function f(p: take Packet) -> function() -> int {\n return function() -> int { return p.id }\n}");
    }

    #[test]
    fn shared_things_cannot_hold_owned_ones() {
        // A function type can mention one: each of its parameters is a
        // parameter, and says what it does with what it is given.
        owned_ok("function f(g: function(take Packet) -> int) -> int { return g(make()) }\nprint(f(send))");
        owned_ok("function f(g: function(Packet) -> int) -> int {\n let p = make()\n return g(p) + g(p)\n}\nprint(f(look))");
        owned_ok("function f(g: function(Packet or none) -> int) { }");
        owned_ok("function f(g: function(int) -> Packet) -> int { return send(g(1)) }");
        assert_eq!(
            owned_error("function f(g: function(take Packet) -> int) { }\nf(look)"),
            "`g` of `f` should be function(take Packet) -> int, but the parameter `p` of this function \
             only looks at what it is given (`Packet`)"
        );
        assert_eq!(
            owned_error("function f(g: function(Packet) -> int) { }\nf(send)"),
            "`g` of `f` should be function(Packet) -> int, but the parameter `p` of this function \
             takes over what it is given (`take Packet`)"
        );
        assert_eq!(
            owned_error("function f(g: function(take Packet) -> int) -> int {\n let p = make()\n return g(p) + g(p)\n}"),
            "`p` is given away here, on a line that also uses it. Do the other thing on a line of its own first"
        );
        // A copy of a generic is checked as if it were written by hand,
        // so `Stack[Packet]` holds its packets as a class written for them
        // would (decision 56.11).
        assert_eq!(
            owned_error("class Stack[T] {\n items: list[T]\n}\nfunction f(s: Stack[Packet]) { }"),
            "each item of this list is an owned Packet, which has one owner: the list. So the list needs an \
             owner too. Write `owned list[...]`"
        );
        owned_ok(
            "owned class Stack[T] {\n items: owned list[T]\n}\nfunction f(s: Stack[Packet]) -> int { return len(s.items) }\n\
             let s: Stack[Packet] = Stack([make()])\nprint(f(s), len(Stack([1]).items))",
        );
        // A list may hold them, and is then their owner. So it has to
        // have an owner itself.
        assert_eq!(
            owned_error("let xs: list[Packet] = []"),
            "each item of this list is an owned Packet, which has one owner: the list. So the list needs an owner too. \
             Write `owned list[...]`"
        );
        assert_eq!(
            owned_error("function f(xs: list[Packet]) { }"),
            "each item of this list is an owned Packet, which has one owner: the list. So the list needs an owner too. \
             Write `owned list[...]`"
        );
        assert_eq!(
            owned_error("let x: owned list[list[Packet]] = []"),
            "the items of this list have one owner each, so the list needs one too, and `owned` cannot be written \
             inside another type yet"
        );
        assert_eq!(
            owned_error("class Box { inside: Packet }"),
            "`inside` holds an owned value, so `Box` must be an `owned class` itself. A shared object cannot own anything"
        );
        assert_eq!(owned_error("enum E { A(p: Packet) }"), "a case of the enum `E` cannot carry an owned value yet");
        assert_eq!(
            owned_error("let p = make()\nraise p"),
            "a value that has one owner cannot be raised, and this is a Packet that is only looked at"
        );
        assert_eq!(
            owned_error("let xs: list[Packet] or none = none"),
            "each item of this list has one owner: the list. So the list needs an owner too. \
             Write `owned list[...] or none`"
        );
        assert_eq!(
            owned_error("class Box { inside: Packet or none }"),
            "`inside` holds an owned value, so `Box` must be an `owned class` itself. A shared object cannot own anything"
        );
    }

    // ----- What may be `none`, and has one owner when it is there -----

    #[test]
    fn what_may_be_none_can_have_one_owner() {
        const MAYBE: &str = "function find(n: int) -> Packet or none {\n if n > 0 { return make() }\n return none\n}\n\
                             function peek(p: Packet or none) -> bool { return p is none }\n\
                             function eat(p: take Packet or none) -> bool { return p is none }\n";
        let ok = |source: &str| owned_ok(&format!("{MAYBE}{source}"));
        let error = |source: &str| owned_error(&format!("{MAYBE}{source}"));

        // A variable, a parameter, a result and a field can each hold one.
        ok("var last: Packet or none = none\nprint(peek(last), last is none)\nlast = find(1)\nlast = none\nprint(eat(last))");
        ok("owned class Slot { held: Packet or none }\nlet s = Slot(find(1))\ns.held = none\ns.held = make()\nprint(peek(s.held), s)");
        ok("let xs: owned list[int] or none = [1, 2]\nif xs is not none { xs.append(3)\nprint(total(xs)) }");
        // Once it is known to be there, it is an owned value like any other.
        ok("let p = find(1)\nif p is not none { print(look(p), p.id)\nprint(send(p)) }");
        ok("print(peek(find(1)), peek(none), eat(find(0)), eat(none), find(2) is none)");
        // A list can hold them, and owns the ones that are there.
        ok("let ps: owned list[Packet or none] = [find(1), none]\nfor p in ps { print(peek(p)) }\nps[1] = make()\nprint(eat(ps.pop()))");

        // It has one owner, so it moves.
        assert!(error("let a = find(1)\nlet b = a\nprint(peek(a))").contains(GONE));
        assert!(error("let a = find(1)\nprint(eat(a))\nprint(peek(a))").contains(GONE));
        assert!(error("let a = find(1)\nif a is not none { print(send(a)) }\nprint(peek(a))").contains(GONE));
        // Giving it a new value fills it again.
        ok("var a = find(1)\nif a is not none { print(send(a))\na = find(2) }\nprint(peek(a))");
        // A look at one cannot be kept, and a field does not give its
        // value up by being read. `take` takes it out (decision 56.10).
        assert_eq!(
            error("function keep(p: Packet or none) -> Packet or none { return p }"),
            "`p` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        assert_eq!(
            error("owned class Slot { held: Packet or none }\nlet s = Slot(find(1))\nlet p = s.held"),
            "`held` holds an owned value, which cannot be taken out of the object it is in. Use it where it is, \
             or pass it to a parameter that looks. To move it out and leave `none` in the field, write `take s.held`"
        );
        ok("owned class Slot { held: Packet or none }\nlet s = Slot(find(1))\nlet p = take s.held\nprint(p is none, s.held is none)");
        // Only the owner may put something else there: that frees what
        // was there.
        assert_eq!(
            error("owned class Slot { held: Packet or none }\nfunction clear(s: Slot) { s.held = none }"),
            "only the owner can replace what `held` holds: somebody else may have been lent it. Here `s` only looks at it"
        );
        // `take` is for what has, or could have, one owner.
        assert_eq!(
            error("function f(n: take int or none) { }"),
            "`take` is for a value that has an owner: a list, an object, a function, an error or an enum value \
             that carries values. This is int or none, which is copied. Write the type alone"
        );
    }

    #[test]
    fn a_match_looks_at_a_value_that_has_one_owner() {
        // The value stays with its owner, and a name a `case` gives it is
        // a look.
        owned_ok("let p = make()\nmatch p { case _ { print(1) } }\nprint(send(p))");
        owned_ok("let p = make()\nmatch p { case other { print(look(other), other.id) } }\nprint(send(p))");
        owned_ok("match make() { case made { print(look(made)) } }");
        assert_eq!(
            owned_error("let p = make()\nmatch p { case other { print(send(other)) } }"),
            "`other` is a look at the value that the `match` is looking at, or at a part of it. \
             Its owner still has it, so it cannot be kept, returned or given away. \
             Use it here, or pass it to a parameter that looks"
        );
        // While the name is in use the owner keeps the value where it is.
        let looking = "the `match` on line 11 gave a name to a part of what `p` holds, and this `case` is still looking at it.";
        let until = "until the `case` is over. If the name is not needed, write `_` in its place";
        assert_eq!(
            owned_error("var p = make()\nmatch p { case other { p = make() } }"),
            format!("{looking} `p` cannot be given a new value {until}")
        );
        assert_eq!(
            owned_error("let p = make()\nmatch p { case other { print(send(p)) } }"),
            format!("{looking} `p` cannot be given away {until}")
        );
        assert_eq!(
            owned_error("let p = make()\nmatch p { case other { p.data = [] } }"),
            format!("{looking} Nothing can replace what `data` holds {until}")
        );
        // A case that gives no name looks at nothing once it is chosen.
        owned_ok("var p = make()\nmatch p { case _ { p = make() } }\nprint(send(p))");
    }

    // ----- A list that owns its items -----

    #[test]
    fn an_owned_list_can_hold_values_that_have_one_owner() {
        // Written out, built by a comprehension, added one at a time.
        owned_ok("let ps: owned list[Packet] = [make(), make()]\nprint(len(ps), ps)");
        owned_ok("let ps: owned list[Packet] = [Packet(n, []) for n in range(3)]\nprint(ps)");
        owned_ok("let ps: owned list[Packet] = []\nps.append(make())\nlet p = make()\nps.append(p)");
        // With no type written, a list of them is an owned list: it
        // could not be anything else.
        owned_ok("let ps = [make(), make()]\nlet qs = ps\nprint(qs)");
        assert!(owned_error("let ps = [make()]\nlet qs = ps\nprint(ps)").contains(GONE));
        // A variable that is put in the list has given its value away.
        assert!(owned_error("let p = make()\nlet ps = [p]\nprint(p.id)").contains(GONE));
        assert!(owned_error("let p = make()\nlet ps: owned list[Packet] = []\nps.append(p)\nprint(p.id)").contains(GONE));
        // `[ ]` and `for` look at an item where it is.
        owned_ok("let ps = [make()]\nprint(ps[0].id, ps[0].size(), look(ps[0]), ps[0])\nps[0].id = 9\nps[0].data.append(1)");
        owned_ok("let ps = [make()]\nvar n = 0\nfor p in ps { n += look(p) + p.size() }\nprint([p.id for p in ps])");
        // `pop()` takes one out, and whoever receives it owns it.
        owned_ok("let ps = [make()]\nlet p = ps.pop()\nprint(send(p))");
        owned_ok("let ps = [make(), make()]\nprint(send(ps.pop()))\nprint(ps.pop().id)\nps.pop()");
        // A new item takes the place of an old one, which is freed.
        owned_ok("let ps = [make()]\nps[0] = make()\nps[0] = Packet(3, [])");
        // `map` and `reduce` lend each item to the function.
        owned_ok("let ps = [make()]\nprint(ps.map(look))");
        owned_ok("function add(n: int, p: Packet) -> int { return n + p.id }\nlet ps = [make()]\nprint(ps.reduce(add, 0))");
        // Made and only looked at: the statement owns it.
        owned_ok("print(len([make(), make()]), [make()][0].id)\nfor p in [make()] { print(p.id) }");
        // It can be lent, given away, returned and kept in an object.
        owned_ok(
            "owned class Queue { waiting: owned list[Packet] }\n\
             function count(ps: owned list[Packet]) -> int { return len(ps) }\n\
             function all() -> owned list[Packet] { return [make()] }\n\
             function keep(ps: take owned list[Packet]) -> Queue { return Queue(ps) }\n\
             let q = keep(all())\nprint(count(q.waiting), count(all()))\nq.waiting.append(make())\nq.waiting = all()",
        );
    }

    #[test]
    fn an_item_that_has_one_owner_stays_in_its_list() {
        let ps = "let ps = [make(), make()]\n";
        let item = |source: &str| owned_error(&format!("{ps}{source}"));
        assert_eq!(
            item("let first = ps[0]"),
            "this item is an owned Packet, which has one owner: the list. `[ ]` only looks at it. \
             Use it where it is, pass it to a parameter that looks, or take it out with `pop()`"
        );
        assert_eq!(item("print(send(ps[0]))"), item("let first = ps[0]"));
        assert_eq!(
            item("for p in ps { print(send(p)) }"),
            "`p` is a look at an item of the list that the loop is walking. The list owns the item, \
             so it cannot be kept, returned or given away. Use it here, or pass it to a parameter that looks"
        );
        assert_eq!(item("let again = [p for p in ps]"), item("for p in ps { print(send(p)) }"));
        // What would put an item in two lists.
        assert_eq!(
            item("let both = ps + ps"),
            "`+` would put each item of these lists in a second list, and each has one owner. \
             Move the items across one at a time with `pop()` and `append`"
        );
        assert_eq!(
            item("var qs = [make()]\nqs += ps"),
            "`+=` would put each item of the list on the right in a second list, and each has one owner. \
             Add the items one at a time with `append`"
        );
        assert_eq!(
            item("function big(p: Packet) -> bool { return p.id > 1 }\nlet some = ps.filter(big)"),
            "`filter` would put the items it keeps in a second list, and each has one owner. \
             Use a `for` loop that makes what the new list needs"
        );
        assert_eq!(
            item("let p = make()\nlet two = [p, p]"),
            "`p` is given away here, on a line that also uses it. Do the other thing on a line of its own first"
        );
    }

    #[test]
    fn only_the_owner_takes_out_what_has_one_owner() {
        // Somebody who was lent the list cannot know who else was lent an item.
        let looking = "somebody else may have been lent it. Here `ps` only looks at it";
        assert_eq!(
            owned_error("function last(ps: owned list[Packet]) -> Packet { return ps.pop() }"),
            format!("only the owner can take an item out of this list: {looking}")
        );
        assert_eq!(
            owned_error("function reset(ps: owned list[Packet]) { ps[0] = make() }"),
            format!("only the owner can replace an item of this list: {looking}")
        );
        // A method may, for what its own object holds. Then whoever
        // calls the method is held to the rule in its place.
        const QUEUE: &str = "owned class Queue {\n waiting: owned list[Packet]\n last: Packet or none\n \
            function next(self) -> Packet { return self.waiting.pop() }\n \
            function keep(self, p: take Packet) { self.last = p }\n \
            function swap(self, p: take Packet) { self.waiting[0] = p }\n \
            function move_on(self) {\n  let p = self.next()\n  self.keep(p)\n }\n \
            function drop_like(self, p: Packet) { self.waiting.pop() }\n \
            function size(self) -> int { return len(self.waiting) }\n}\n\
            let q = Queue([make()], none)\n";
        owned_ok(&format!("{QUEUE}let p = q.next()\nq.keep(p)\nq.swap(make())\nq.move_on()\nprint(q.size())"));
        let calls = |name: &str| format!("call `{name}`, which takes out or replaces something the object owns");
        assert_eq!(
            owned_error(&format!("{QUEUE}function drain(queue: Queue) {{ queue.move_on() }}")),
            format!("only the owner can {}: somebody else may have been lent it. Here `queue` only looks at it", calls("move_on"))
        );
        assert_eq!(
            owned_error(&format!("{QUEUE}for p in q.waiting {{ q.next() }}")),
            format!(
                "the `for` on line 24 is walking a list that `q` owns, and is still looking at its items. \
                 Nothing can {} until the loop is over",
                calls("next")
            )
        );
        assert_eq!(
            owned_error(&format!("{QUEUE}print(q.size(), q.next().id)")),
            "this line has already looked at `q`, and the look might still be in use. Do the other thing on a line of its own first"
        );
        assert_eq!(
            owned_error(&format!("{QUEUE}q.drop_like(q.waiting[0])")),
            "this method takes out or replaces something its object owns, and an argument on this line looks at \
             the same object. Work the argument out on a line of its own first"
        );
        // Inside the method the other two rules hold for `self`.
        assert_eq!(
            owned_error(
                "owned class Queue {\n waiting: owned list[Packet]\n function clear(self) {\n  for p in self.waiting { self.waiting.pop() }\n }\n}"
            ),
            "the `for` on line 13 is walking a list that `self` owns, and is still looking at its items. \
             Nothing can take an item out of this list until the loop is over"
        );
        // A method that only looks can be called by anyone.
        owned_ok(&format!("{QUEUE}function count(queue: Queue) -> int {{ return queue.size() }}\nfor p in q.waiting {{ print(q.size()) }}"));
        // It can add to it, and change what is inside an item.
        owned_ok("function more(ps: owned list[Packet]) { ps.append(make())\nps[0].id = 4 }");
        // The owner can reach in as far as it likes.
        owned_ok(
            "owned class Queue { waiting: owned list[Packet] }\n\
             let q = Queue([make()])\nlet p = q.waiting.pop()\nq.waiting.append(p)\nq.waiting[0] = make()\nq.waiting[0].data = [7]",
        );
        // A list of numbers has nothing with an owner in it.
        owned_ok("function last(xs: owned list[int]) -> int { return xs.pop() }");

        // Not while a `for` loop is looking at the items.
        let ps = "let ps = [make(), make()]\n";
        let walking = "the `for` on line 11 is walking a list that `ps` owns, and is still looking at its items.";
        assert_eq!(
            owned_error(&format!("{ps}for p in ps {{ ps.pop() }}")),
            format!("{walking} Nothing can take an item out of this list until the loop is over")
        );
        assert_eq!(
            owned_error(&format!("{ps}for p in ps {{ ps[0] = make() }}")),
            format!("{walking} Nothing can replace an item of this list until the loop is over")
        );
        assert_eq!(
            owned_error(&format!("{ps}for p in ps {{ let qs = ps\nbreak }}")),
            format!("{walking} `ps` cannot be given away until the loop is over")
        );
        assert_eq!(
            owned_error(&format!("{ps}let ids = [ps.pop().id for p in ps]")),
            format!("{walking} Nothing can take an item out of this list until the loop is over")
        );
        // Nor may the owner be given a new value: that frees the old one,
        // items and all.
        assert_eq!(
            owned_error("var ps: owned list[Packet] = [make(), make()]\nfor p in ps { ps = []\nbreak }"),
            format!("{walking} `ps` cannot be given a new value until the loop is over")
        );
        assert_eq!(
            owned_error(
                "owned class Queue { waiting: owned list[Packet] }\n\
                 var q = Queue([make()])\nfor p in q.waiting { q = Queue([])\nbreak }"
            ),
            "the `for` on line 12 is walking a list that `q` owns, and is still looking at its items. \
             `q` cannot be given a new value until the loop is over"
        );
        // Adding to it is fine: the loop walks the list as it was.
        owned_ok(&format!("{ps}for p in ps {{ if p.id > 5 {{ ps.append(make()) }} }}"));
        // A loop over numbers looks at nothing that can be freed.
        owned_ok("var xs: owned list[int] = [1, 2]\nfor x in xs { xs.pop() }");

        // Not on a line that has looked at the list, either way round.
        assert_eq!(
            owned_error(&format!("function two(a: Packet, b: take Packet) {{ }}\n{ps}two(ps[0], ps.pop())")),
            "this line has already looked at `ps`, and the look might still be in use. \
             Do the other thing on a line of its own first"
        );
        assert_eq!(
            owned_error(&format!("function two(a: take Packet, b: Packet) {{ }}\n{ps}two(ps.pop(), ps[0])")),
            "this line takes an item out of `ps`, so it cannot use `ps` for anything else. \
             Do the other thing on a line of its own"
        );
        owned_ok(&format!("{ps}ps[len(ps) - 1] = make()\nps[0] = Packet(ps[0].id + 1, [])"));
    }

    // ----- Where every list and object has one owner -----

    /// The messages for a program checked as `zeno build --no-collector`
    /// checks it.
    fn one_owner(source: &str) -> Vec<String> {
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
        let rules = super::Rules { strict: true, one_owner: true, firmware: true };
        super::check_by(&program, rules).0.into_iter().map(|error| error.message).collect()
    }

    fn one_owner_ok(source: &str) {
        assert_eq!(one_owner(source), Vec::<String>::new(), "\n{source}");
    }

    fn one_owner_error(source: &str) -> String {
        let mut found = one_owner(source);
        assert_eq!(found.len(), 1, "expected exactly one error, got {found:?}\n{source}");
        found.remove(0)
    }

    #[test]
    fn with_one_owner_a_list_and_an_object_need_no_word() {
        const POINT: &str = "class Point {\n x: int\n y: int\n}\n\
                             function look(p: Point) -> int { return p.x }\n\
                             function consume(p: take Point) -> int { return p.y }\n\
                             function total(xs: list[int]) -> int { return len(xs) }\n\
                             function eat(xs: take list[int]) -> int { return len(xs) }\n\
                             function make(n: int) -> list[int] { return [i for i in range(n)] }\n";
        let ok = |source: &str| one_owner_ok(&format!("{POINT}{source}"));
        let error = |source: &str| one_owner_error(&format!("{POINT}{source}"));
        // The same programs are fine where values are shared.
        let shared = |source: &str| strict_ok(&format!("{POINT}{source}"));

        // A list that is made belongs to whoever receives it.
        ok("let xs = [1, 2]\nprint(total(xs), xs)\nlet ys = xs\nprint(eat(ys), eat([3]), eat(make(2)))");
        ok("var xs = make(3)\nxs = make(4)\nxs.append(1)\nxs += [2]\nlet words = \"a b\".split()\nprint(xs, words)");
        // Giving it to another name, or to a parameter that says `take`,
        // gives it away.
        let gone = "let xs = [1, 2]\nlet ys = xs\nprint(xs)";
        assert!(error(gone).contains(GONE));
        shared(gone);
        let eaten = "let xs = [1, 2]\nprint(eat(xs))\nprint(xs)";
        assert!(error(eaten).contains(GONE));
        shared(eaten);
        // So is every object, of every class.
        ok("let p = Point(1, 2)\nprint(look(p), p.x, p)\nprint(consume(p))");
        let taken = "let p = Point(1, 2)\nprint(consume(p))\nprint(p.x)";
        assert!(error(taken).contains(GONE));
        shared(taken);
        // A list in a list, and an object in a list, belong to that list.
        ok("var grid = [[1, 2], [3]]\ngrid.append([4])\ngrid[0] = make(2)\ngrid[1].append(5)\nlet row = grid.pop()\nprint(grid, row, grid[0][1])");
        ok("let ps = [Point(1, 2)]\nps.append(Point(3, 4))\nfor p in ps { print(look(p)) }\nprint(consume(ps.pop()))");
        ok("let table: list[list[str]] = [\"a b\".split()]\nfor row in table { for word in row { print(word) } }");
        assert_eq!(
            error("let grid = [[1], [2]]\nlet row = grid[0]"),
            "this item is an owned list[int], which has one owner: the list. `[ ]` only looks at it. \
             Use it where it is, pass it to a parameter that looks, or take it out with `pop()`"
        );
        // A field that holds one belongs to the object.
        ok("class Bag {\n items: list[int]\n spare: Point\n}\nlet b = Bag([1], Point(0, 0))\nb.items = [2]\nb.spare = Point(1, 1)\nprint(b)");
        // A method may replace what its own object holds, and then only
        // the owner of the object may call it.
        ok("class Bag {\n items: list[int]\n function empty(self) { self.items = [] }\n}\nlet b = Bag([1])\nb.empty()\nprint(b)");
        assert_eq!(
            error("class Bag {\n items: list[int]\n function empty(self) { self.items = [] }\n}\nfunction f(b: Bag) { b.empty() }"),
            "only the owner can call `empty`, which takes out or replaces something the object owns: somebody else may \
             have been lent it. Here `b` only looks at it"
        );
        // What may be `none` has one owner too, when it is there.
        ok("var p: Point or none = none\np = Point(1, 2)\nif p is not none { print(look(p))\nprint(consume(p)) }");
        ok("var xs: list[int] or none = none\nxs = make(2)\nif xs is not none { print(total(xs))\nprint(eat(xs)) }");
        let maybe = "var xs: list[int] or none = make(2)\nlet ys = xs\nprint(xs is none)";
        assert!(error(maybe).contains(GONE));
        shared(maybe);
        // A function type says what each parameter does with a list or
        // an object, as a function does.
        ok("function f(g: function(take Point) -> int) -> int { return g(Point(1, 2)) }\nprint(f(consume))");
        ok("function f(g: function(list[int]) -> int) -> int {\n let xs = make(3)\n return g(xs) + g(xs)\n}\nprint(f(total))");
        ok("function f(g: function(int) -> list[int]) -> int { return eat(g(2)) }\nprint(f(make))");
        assert!(error("function f(g: function(take list[int]) -> int) -> int {\n let xs = make(3)\n print(g(xs))\n return g(xs)\n}").contains(GONE));
        assert_eq!(
            error("function f(g: function(take list[int]) -> int) { }\nf(total)"),
            "`g` of `f` should be function(take list[int]) -> int, but the parameter `xs` of this function \
             only looks at what it is given (`list[int]`)"
        );
        // Nor as what a type parameter stands for. An item type is fine.
        ok("function first[T](items: list[T]) -> T { return items[0] }\nlet xs = [1, 2]\nprint(first(xs))");
        // It can, and the copy is checked as if it were written by hand
        // (decision 56.11): this `same` returns what it only looks at.
        let looks = "`x` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
                     If it is a parameter, write `take` in front of its type to take the value over";
        assert_eq!(error("function same[T](x: T) -> T { return x }\nlet xs = same([1, 2])"), looks);
        assert_eq!(error("function same[T](x: T) -> T { return x }\nlet p = same(Point(1, 2))"), looks);
        ok("function same[T](x: take T) -> T { return x }\nlet xs = same([1, 2])\nlet p = same(Point(1, 2))\nprint(xs, p.x)");
    }

    #[test]
    fn with_one_owner_an_enum_value_that_carries_values_has_one() {
        const SHAPE: &str = "enum Shape {\n Dot\n Circle(r: float)\n Path(points: list[int])\n Named(name: str, inner: Shape)\n}\n\
                             enum Color {\n Red\n Green\n}\n\
                             function area(s: Shape) -> float { return 1.0 }\n\
                             function eat(s: take Shape) -> float { return 2.0 }\n\
                             function pick(n: int) -> Shape {\n if n > 0 { return Dot }\n return Circle(1.0)\n}\n";
        let ok = |source: &str| one_owner_ok(&format!("{SHAPE}{source}"));
        let error = |source: &str| one_owner_error(&format!("{SHAPE}{source}"));
        let shared = |source: &str| strict_ok(&format!("{SHAPE}{source}"));

        // It is made, lent, compared, given away and kept in a list like
        // an object. A case may carry what has one owner.
        ok("let s = Circle(1.0)\nprint(s, area(s), s == Dot, area(pick(0)), eat(pick(1)))\nprint(eat(s))");
        ok("let n = Named(\"a\", Path([1, 2]))\nprint(n)\nlet shapes = [Dot, pick(0), n]\nfor s in shapes { print(area(s)) }\nprint(eat(shapes.pop()))");
        let gone = "let a = Circle(1.0)\nlet b = a\nprint(area(a))";
        assert!(error(gone).contains(GONE));
        shared(gone);
        // Every value of such an enum has one owner, bare cases too:
        // which case a variable holds is not known until the program runs.
        let bare = "let a = Dot\nlet b = a\nprint(area(a))";
        assert!(error(bare).contains(GONE));
        shared(bare);
        // The name of a bare case is the value, new each time.
        ok("let a = Dot\nlet b = Dot\nprint(a == b, eat(Dot), eat(Dot))\nfunction f() -> Shape { return Dot }");
        // An enum whose cases carry nothing is a number, and is copied.
        ok("var c = Red\nlet d = c\nc = Green\nprint(c, d, c == d)\nfunction flip() { c = Red }");
        assert_eq!(
            error("function f(c: take Color) { }"),
            "`take` is for a value that has an owner: a list, an object, a function, an error or an enum value \
             that carries values. This is Color, which is copied. Write the type alone"
        );

        // `match` looks. A number or a text that a case carries is copied
        // out, so the owner is free to change.
        ok("var s = pick(0)\nmatch s {\n case Dot { s = Circle(0.0) }\n case Circle(r) { s = Circle(r + 1.0) }\n case Path(_) { s = Dot }\n case Named(name, _) { s = Named(name, Dot) }\n}\nprint(s)");
        ok("function sum(s: Shape) -> int {\n match s {\n  case Path(points) { return len(points) }\n  case Named(_, inner) { return sum(inner) }\n  case _ { return 0 }\n }\n}");
        // A part that has one owner stays in the value: its name is a look.
        assert_eq!(
            error("let s = pick(0)\nmatch s {\n case Path(points) { let mine = points }\n case _ { }\n}"),
            "`points` is a look at the value that the `match` is looking at, or at a part of it. \
             Its owner still has it, so it cannot be kept, returned or given away. \
             Use it here, or pass it to a parameter that looks"
        );
        assert_eq!(
            error("var s = pick(0)\nmatch s {\n case Path(points) { s = Dot }\n case _ { }\n}"),
            "the `match` on line 18 gave a name to a part of what `s` holds, and this `case` is still looking at it. \
             `s` cannot be given a new value until the `case` is over. If the name is not needed, write `_` in its place"
        );
        // An item of a list stays in the list, as an object does.
        assert_eq!(
            error("let shapes = [Dot, Circle(2.0)]\nlet first = shapes[0]"),
            "this item is an owned Shape, which has one owner: the list. `[ ]` only looks at it. \
             Use it where it is, pass it to a parameter that looks, or take it out with `pop()`"
        );
    }

    #[test]
    fn with_one_owner_an_except_owns_the_error_it_catches() {
        const BAD: &str = "class Bad {\n id: int\n}\n\
                           function risky(n: int) -> int raises Bad {\n if n > 2 { raise Bad(n) }\n return n\n}\n\
                           function parse(text: str) -> int raises ValueError {\n if text == \"\" { raise ValueError(\"empty\") }\n return 1\n}\n\
                           function look(b: Bad) -> int { return b.id }\n\
                           function why(e: ValueError) -> str { return e.message }\n\
                           function consume(b: take Bad) -> int { return b.id }\n";
        let ok = |source: &str| one_owner_ok(&format!("{BAD}{source}"));
        let error = |source: &str| one_owner_error(&format!("{BAD}{source}"));
        let caught = "try { print(risky(5)) } except Bad as e";
        // The name owns the error. It can be read, lent, kept, given
        // away and raised again.
        ok(&format!("{caught} {{ print(e.id, look(e), e) }}"));
        ok(&format!("{caught} {{ let again = e\nprint(again.id) }}"));
        ok(&format!("{caught} {{ print(consume(e)) }}"));
        ok(&format!("var last: Bad or none = none\n{caught} {{ last = e }}\nprint(last)"));
        ok(&format!("let seen: list[Bad] = []\n{caught} {{ seen.append(e) }}\nprint(len(seen))"));
        ok(&format!("function again() -> int raises Bad {{\n {caught} {{ raise e }}\n return 0\n}}"));
        ok(&format!("function found() -> Bad or none {{\n {caught} {{ return e }}\n return none\n}}"));
        // Given away is gone, as for any owner.
        assert!(error(&format!("{caught} {{ let again = e\nprint(e.id) }}")).contains(GONE));
        assert!(error(&format!("{caught} {{ print(consume(e))\nraise e }}")).contains(GONE));
        // An error of a kind Zeno knows has one owner too.
        let fault = "try { print(parse(\"\")) } except ValueError as e";
        ok(&format!("{fault} {{ print(e.message, e.kind, why(e), e) }}"));
        ok(&format!("var last: ValueError or none = none\n{fault} {{ last = e }}\nprint(last)"));
        ok(&format!("let faults: list[ValueError] = []\n{fault} {{ faults.append(e) }}"));
        ok(&format!("function again() -> int raises ValueError {{\n {fault} {{ raise e }}\n return 0\n}}"));
        ok("let ready = ValueError(\"x\")\nprint(ready.message, why(ready))\nlet other = ready\nprint(other)");
        ok("function fail(e: take ValueError) raises ValueError { raise e }");
        ok("try { print(parse(\"\")) } except Error as e { var any: Error or none = none\nany = e\nprint(any) }");
        assert!(error(&format!("{fault} {{ let again = e\nprint(e.message) }}")).contains(GONE));
        assert!(error("let ready = ValueError(\"x\")\nlet other = ready\nprint(ready)").contains(GONE));
        // `raise` takes the error: a look cannot be raised.
        assert_eq!(
            error("function fail(e: ValueError) raises ValueError { raise e }"),
            "`e` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // Where values are shared, an error can be kept like any other.
        strict_ok(&format!("{BAD}{caught} {{ let again = e\nprint(consume(e)) }}"));
        strict_ok(&format!("{BAD}{fault} {{ let again = e\nprint(again, e) }}"));
    }

    #[test]
    fn with_one_owner_a_function_value_has_one() {
        const ADD: &str = "class Task {\n id: int\n}\n\
                           function double(n: int) -> int { return n * 2 }\n\
                           function make_adder(n: int) -> function(int) -> int {\n return function(x: int) -> int { return x + n }\n}\n\
                           function apply(f: function(int) -> int, x: int) -> int { return f(x) }\n\
                           function keep(f: take function(int) -> int) -> int { return f(1) }\n\
                           function send(xs: take list[int]) -> int { return len(xs) }\n";
        let ok = |source: &str| one_owner_ok(&format!("{ADD}{source}"));
        let error = |source: &str| one_owner_error(&format!("{ADD}{source}"));
        let shared = |source: &str| strict_ok(&format!("{ADD}{source}"));
        // Made, called, lent, given away.
        ok("let add = make_adder(2)\nprint(add(1), apply(add, 2), apply(add, 3))\nprint(keep(add))");
        ok("print(apply(double, 1), keep(double), keep(double), keep(make_adder(1)), make_adder(1)(2))");
        ok("let scale = 3\nlet triple = function(n: int) -> int { return n * scale }\nprint(triple(1), apply(triple, 2))");
        let gone = "let add = make_adder(2)\nlet other = add\nprint(add(1))";
        assert!(error(gone).contains(GONE));
        shared(gone);
        let kept = "let add = make_adder(2)\nprint(keep(add))\nprint(add(1))";
        assert!(error(kept).contains(GONE));
        shared(kept);
        // A function of the program is a fixed value, like a case of an
        // enum that carries nothing: its name is never given away.
        ok("let f = double\nlet g = f\nprint(f(1), g(1), keep(double), keep(double))");
        // A variable, a field and a list can hold one, and own it.
        ok("var f: function(int) -> int = double\nf = make_adder(1)\nf = double\nprint(f(1))");
        ok("class Button {\n on: function(int) -> int\n}\nlet b = Button(make_adder(1))\nb.on = double\nprint(b.on(2))");
        ok("let fs: list[function(int) -> int] = [double, make_adder(1)]\nfs.append(make_adder(2))\nfor f in fs { print(f(1)) }\nlet f = fs.pop()\nprint(f(1), fs[0](1))");
        assert_eq!(
            error("let fs: list[function(int) -> int] = [double]\nlet f = fs[0]"),
            "this item is an owned function(int) -> int, which has one owner: the list. `[ ]` only looks at it. \
             Use it where it is, pass it to a parameter that looks, or take it out with `pop()`"
        );
// A `var` that is given a function of the program can be given
        // another: it has the function's type. A `let` holds the one.
        ok("var f = double\nf = make_adder(1)\nf = double\nprint(f(1))");
        shared("var f = double\nf = make_adder(1)\nf = double\nprint(f(1))");
        assert_eq!(
            error("var f = double\nlet g = f\nprint(f(1))"),
            "`f` was given away on line 12 and holds nothing now. Use it before it goes, or give it only to what looks: \
             a parameter without `take`"
        );

        // A function that uses a list or an object from outside looks
        // at it when it is only passed along: it is lent on the line that
        // writes it.
        ok("let xs = [1, 2]\nprint(apply(function(n: int) -> int { return n + len(xs) }, 1), xs)");
        ok("let xs = [1, 2]\nprint(xs.map(function(n: int) -> int { return n + xs[0] }), xs.filter(function(n: int) -> bool { return n > len(xs) }))");
        ok("let t = Task(1)\nprint(apply(function(n: int) -> int {\n t.id += n\n return t.id\n}, 1), t)");
        ok("let xs = [1, 2]\nprint(apply(function(n: int) -> int { return apply(function(m: int) -> int { return m + len(xs) }, n) }, 1))");
        ok("for t in [Task(1), Task(2)] { print(apply(function(n: int) -> int { return n + t.id }, 1)) }");
        ok("let add = make_adder(1)\nprint(apply(function(n: int) -> int { return add(n) }, 1), add(2))");
        // A function that is kept takes what it uses (decision 56.8),
        // and the owner holds nothing after that line.
        ok("function main() {\n let xs = [1, 2]\n let f = function(n: int) -> int { return n + len(xs) }\n print(f(1))\n}\nmain()");
        ok("function main() {\n let xs = [1, 2]\n print(keep(function(n: int) -> int { return n + len(xs) }))\n}\nmain()");
        assert!(error("function main() {\n let xs = [1, 2]\n let f = function(n: int) -> int { return n + len(xs) }\n print(len(xs))\n}")
            .contains(GONE));
        // A `var` cannot be taken: the function around could give it a
        // new value after the kept one took it.
        const VAR: &str = "this function is kept, so it takes `xs`, but `xs` is a `var`: the function around it could \
                           give it a new value after this one took it. Declare it with `let`";
        assert_eq!(error("var xs = [1, 2]\nlet f = function(n: int) -> int { return n + len(xs) }\nprint(f(1))"), VAR);
        assert_eq!(error("var xs = [1, 2]\nprint(keep(function(n: int) -> int { return n + len(xs) }))"), VAR);
        // Unless what it uses is a `let` at the top of the file, which
        // is there until the program ends, and which it only looks at.
        ok("let xs = [1, 2]\nlet f = function(n: int) -> int { return n + len(xs) }\nprint(keep(f), len(xs))");
        // A parameter that only looks has nothing to give.
        assert_eq!(
            error("function f(xs: list[int]) -> function(int) -> int {\n return function(n: int) -> int { return n + len(xs) }\n}"),
            "this function is kept, so it takes `xs`, but `xs` is only looked at here: its owner still has it. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        ok("function f(xs: take list[int]) -> function(int) -> int {\n return function(n: int) -> int { return n + len(xs) }\n}\nprint(f([1])(2))");
        // The line that lends it is looking at what it uses.
        assert_eq!(
            error("var xs = [1, 2]\nprint(apply(function(n: int) -> int { return n + len(xs) }, send(xs)))"),
            "`xs` is given away here, on a line that also uses it. Do the other thing on a line of its own first"
        );
        assert_eq!(
            error("var xs = [1, 2]\nprint(send(xs))\nprint(apply(function(n: int) -> int { return n + len(xs) }, 1))"),
            format!("`xs` was given away on line 12 and holds nothing now, and this function uses it. \
                     Use it before it goes, or give it only to what looks: a parameter without `take`")
        );
        // It only looks: it cannot take an item out, or give the value away.
        assert_eq!(
            error("var ts = [Task(1)]\nprint(apply(function(n: int) -> int { return n + ts.pop().id }, 1))"),
            "only the owner can take an item out of this list: somebody else may have been lent it. \
             Here `ts` belongs to the function around this one, which is only looked at from here"
        );
        assert_eq!(
            error("var xs = [1, 2]\nprint(apply(function(n: int) -> int { return n + send(xs) }, 1))"),
            "`xs` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // A function of the program still cannot reach a `var`.
        assert_eq!(
            error("var xs = [1, 2]\nfunction f() -> int { return apply(function(n: int) -> int { return n + len(xs) }, 1) }"),
            "`xs` holds an owned value, which belongs to the block that declares it. \
             A function written elsewhere cannot reach it. Pass it to the function: a parameter looks at what it is given"
        );

        // Nothing is shared, so a `var` of the function around cannot
        // be used. A `let` is copied in, and a `var` at the top of the
        // file is the program's.
        let var = "function counter() -> function() -> int {\n var count = 0\n return function() -> int { return count }\n}";
        assert_eq!(
            error(var),
            "`count` is a `var` of the function around this one, so the two functions would share it, and with no \
             collector nothing is shared. If it does not change, declare it with `let`. If it does, keep it in an \
             object, which a function written here can look at and change"
        );
        shared(var);
        ok("var total = 0\nprint(apply(function(n: int) -> int {\n total += n\n return total\n}, 1), total)");
        ok("function counter() -> function() -> int {\n let count = 0\n return function() -> int { return count }\n}");
    }

    #[test]
    fn where_owned_and_take_can_be_written() {
        let only_parameters = "`take` can only be written on a parameter, where it says that the function takes the value over. \
                               A variable, a field and what a function gives back always own what they hold";
        assert_eq!(owned_error("let x: take list[int] = [1]"), only_parameters);
        assert_eq!(owned_error("function f() -> take Packet { return make() }"), only_parameters);
        assert_eq!(owned_error("owned class Box { inside: take Packet }"), only_parameters);
        assert_eq!(
            owned_error("let x: owned int = 1"),
            "`owned` goes in front of a list or a dict type, as in `owned list[int]`, and this is int. A class is made owned where it is declared: `owned class`"
        );
        // `take` is for what has, or could have, one owner. A number is
        // copied, and there is one way to write that.
        assert_eq!(
            owned_error("function f(n: take int) { }"),
            "`take` is for a value that has an owner: a list, an object, a function, an error or an enum value \
             that carries values. This is int, which is copied. Write the type alone"
        );
        // On a shared value it changes nothing, so that one program can
        // be written for both kinds of build.
        owned_ok("class Point { x: int }\nfunction f(p: take Point, xs: take list[int]) { }\nlet p = Point(1)\nf(p, [1])\nprint(p)");
        assert_eq!(
            owned_error("let x: list[owned list[int]] = []"),
            "`owned` and `take` describe a whole value, so they go at the very front of a type, and not inside `list[ ]` or `or none`"
        );
    }

    #[test]
    fn a_plain_parameter_looks_and_take_takes() {
        const FRAME: &str = "class Frame {\n    id: int\n    bytes: list[int]\n}\n\
            function size(frame: Frame) -> int { return len(frame.bytes) }\n\
            function send(frame: take Frame) -> int { return frame.id }\n\
            let frame = Frame(7, [1, 2])\n";
        // A parameter that is written with its type alone looks: the
        // caller still has the value, as often as it likes.
        one_owner_ok(&format!("{FRAME}print(size(frame))\nprint(size(frame))\nprint(send(frame))"));
        // `take` takes the value over, and the caller's variable is empty.
        assert_eq!(
            one_owner_error(&format!("{FRAME}print(send(frame))\nprint(frame.id)")),
            "`frame` was given away on line 8 and holds nothing now. Use it before it goes, or give it only to what \
             looks: a parameter without `take`"
        );
        // What only looks cannot keep, and the message names the word.
        assert_eq!(
            one_owner_error("class Frame {\n    id: int\n}\nfunction keep(frame: Frame) -> Frame { return frame }"),
            "`frame` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // Where values are shared, the same program means the same
        // thing: `take` on a shared value changes nothing.
        strict_ok(&format!("{FRAME}print(size(frame))\nprint(send(frame))\nprint(frame.id)"));
        // The word is for what has an owner. A number is copied.
        assert_eq!(
            error("function f(n: take int) -> int { return n }"),
            "`take` is for a value that has an owner: a list, an object, a function, an error or an enum value that \
             carries values. This is int, which is copied. Write the type alone"
        );
    }

    #[test]
    fn every_function_can_look_at_a_let_at_the_top_of_the_file() {
        const TABLE: &str = "class Task {\n    id: int\n}\nlet table = [10, 20]\nlet tasks = [Task(1)]\n";
        // The value is there until the program ends, so a function, a
        // method and a function that is kept may all look at it, and
        // change what is inside it.
        one_owner_ok(&format!(
            "{TABLE}function first() -> int {{ return table[0] + tasks[0].id }}\n\
             function bump() {{\n    table[0] += 1\n    tasks[0].id = 5\n    tasks.append(Task(2))\n}}\n\
             let pick = function(i: int) -> int {{ return table[i] }}\n\
             bump()\nprint(first(), pick(1), len(table))"
        ));
        // It stays where it is: it cannot be given away, before or after.
        assert_eq!(
            one_owner_error(&format!("{TABLE}function first() -> int {{ return table[0] }}\nlet other = table")),
            "`table` is used by a function, on line 6, so it cannot be given away: the function could be called \
             when `table` holds nothing. Pass `table` to that function instead, or give away a copy"
        );
        assert_eq!(
            one_owner_error(&format!("{TABLE}let other = table\nfunction first() -> int {{ return table[0] }}")),
            "`table` is given away on line 6, so a function cannot use it: the function could be called when \
             `table` holds nothing. Keep `table` where it is, or pass it to the function"
        );
        // A function only looks. The top of the file is the owner.
        assert_eq!(
            one_owner_error(&format!("{TABLE}function all() -> list[int] {{ return table }}")),
            "`table` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        assert_eq!(
            one_owner_error(&format!("{TABLE}function last() -> Task {{ return tasks.pop() }}")),
            "only the owner can take an item out of this list: somebody else may have been lent it. \
             Here `tasks` belongs to the top of the file, and a function only looks at it"
        );
        // A `var` could be given a new value while a function looks.
        assert_eq!(
            one_owner_error("var table = [1, 2]\nfunction first() -> int { return table[0] }"),
            "`table` holds an owned value, which belongs to the block that declares it. A function written \
             elsewhere cannot reach it. Pass it to the function: a parameter looks at what it is given. Or declare \
             it with `let`, if it is never given a new value: every function can look at a `let` at the top of the file"
        );
        // The same holds for an `owned` value where there is a collector.
        owned_ok("let window: owned list[int] = [1, 2]\nfunction first() -> int { return window[0] }\nprint(first())");
        // A name is still only known below its declaration, and the
        // message says where that is.
        assert_eq!(
            error("function first() -> int { return table[0] }\nlet table = [1, 2]\nprint(first())"),
            "`table` is declared on line 2, which is further down. A line can use only what is declared above it, \
             also inside a function. Move the declaration of `table` up"
        );
    }

    #[test]
    fn text_and_list_methods_are_checked() {
        strict_ok(
            "let text = \"a b\"\nlet at = text.find(\" \")\nif at is not none {\n print(text.slice(0, at), text.slice(at))\n}\n\
             let n: int = text.slice(0, 1).code()\nlet c: str = char(n)\n\
             var xs = [3, 1, 2]\nxs.insert(0, 4)\nxs.sort()\nxs.reverse()\nlet x: int = xs.pop(1)\n\
             let part: list[int] = xs.slice(1)\nlet where_: int or none = xs.find(2)\nxs.clear()\n\
             let joined: str = [\"a\", \"b\"].join(\", \")\nlet bytes: list[u8] = [u8(3), u8(1)]\nbytes.sort()",
        );
        // `find` may find nothing, so its answer is not a number yet.
        assert_eq!(
            error("let n: int = \"abc\".find(\"b\")"),
            "`n` should be int, but this is int or none. Check it with `is not none` first"
        );
        assert_eq!(error("let n: int = [1, 2].find(2)").len(), error("let n: int = \"abc\".find(\"b\")").len());
        assert_eq!(error("print(\"abc\".find(1))"), "an argument of `find` should be str, but this is int");
        assert_eq!(error("print(\"abc\".slice(\"a\"))"), "a position given to `slice` should be int, but this is str");
        assert_eq!(error("print([1].pop(\"a\"))"), "a position given to `pop` should be int, but this is str");
        assert_eq!(error("print([1, 2].find(\"a\"))"), "the item `find` looks for should be int, but this is str");
        assert_eq!(
            error("let xs = [1]\nxs.insert(\"a\", 1)"),
            "the position given to `insert` should be int, but this is str"
        );
        assert_eq!(error("let xs = [1]\nxs.insert(0, \"a\")"), "an item put in this list should be int, but this is str");
        assert_eq!(error("print(char(\"a\"))"), "the code of a character should be int, but this is str");
        assert_eq!(error("print(\"a\".code(1))"), "`code` takes 0 arguments, but 1 was given");
        // Only numbers and text have an order of their own.
        assert_eq!(
            error("let flags = [true, false]\nflags.sort()"),
            "`sort` puts numbers or text in order, smallest first, and this is list[bool]. There is no one order \
             for those items, so give `sort` a function that says what each is ordered by, as in `people.sort(age_of)`"
        );
        // With a function, the items go in the order of what it gives.
        ok("let flags = [true, false]\nflags.sort(function(flag: bool) -> int { return 1 })\n\
            let words = [\"b\", \"a\"]\nwords.sort(len)\nwords.sort(function(word) { return word })");
        assert_eq!(
            error("let xs = [1, 2]\nxs.sort(function(n: int) -> bool { return n > 1 })"),
            "the function given to `sort` says what each item is ordered by, a number or text, and this one returns bool"
        );
        assert_eq!(
            error("let xs = [1, 2]\nxs.sort(function(a: int, b: int) -> bool { return a < b })"),
            "the function given to `sort` takes one item and returns what it is ordered by, a number or text. \
             It is not asked which of two items comes first"
        );
        assert_eq!(error("let xs = [1, 2]\nxs.sort(1, 2)"), "`sort` takes 0 to 1 arguments, but 2 were given");
        assert_eq!(
            error("print([1, 2].join(\", \"))"),
            "`join` puts text together, and this is list[int]. Make each item text first, with `str( )`"
        );
    }

    #[test]
    fn a_dict_is_checked() {
        ok("let ages = [\"Ada\": 36]\nages[\"Bob\"] = 5\nlet n: int = ages[\"Ada\"]\nprint(len(ages), \"Ada\" in ages, ages == ages)");
        strict_ok(
            "let d: dict[str, int] = [:]\nd[\"a\"] = 1\nlet m: int or none = d.get(\"b\")\nlet n: int = d.get(\"b\", 0)\n\
             let p: int = d.pop(\"a\")\nlet ks: list[str] = d.keys()\nlet vs: list[int] = d.values()\nd.clear()\n\
             for k in d {\n print(k.upper())\n}\nfunction f(x: dict[int, bool]) -> dict[int, bool] {\n return x\n}",
        );
        // What it holds: keys that are text or ints, plain values, each
        // all of one type.
        assert_eq!(error("let d = [1.5: \"x\"]"), "a key of a dict is a str or an int, and this is float");
        assert_eq!(
            error("let d = [\"a\": 1, 2: 3]"),
            "the keys of a dict are all of one type. This one is int, and the ones before it are str"
        );
        assert_eq!(
            error("let d = [\"a\": 1, \"b\": 2.5]"),
            "the values of a dict are all of one type. This one is float, and the ones before it are int"
        );
        assert_eq!(
            error("let d = [\"a\": 1, \"a\": 2]"),
            "this key is written twice in the dict. A dict has each key once, so the second would replace the first. \
             Take one of them out"
        );
        // The written type.
        assert_eq!(
            error("let d: dict[str] = [:]"),
            "`dict` takes two types, the key's and then the value's, as in `dict[str, int]`"
        );
        assert_eq!(
            error("let d: dict[float, int] = [:]"),
            "a key of a dict is a str or an int, and this says float. Write `dict[str, ...]` or `dict[int, ...]`"
        );
        assert_eq!(
            error("let d: dict[str, float] = [\"a\": 1]"),
            "the values of this dict are float here, but this one is int. Write a number with a decimal point, as in `1.0`, and convert a value with `float( )`"
        );
        assert_eq!(
            strict_error("let d = [:]"),
            "strict mode: the type of `d` cannot be worked out. Give it one, as in `d: dict[str, int]`"
        );
        // Keys and values where they are used.
        assert_eq!(error("let d = [\"a\": 1]\nprint(d[3])"), "a key of this dict should be str, but this is int");
        assert_eq!(error("let d = [\"a\": 1]\nd[\"b\"] = \"two\""), "the new value should be int, but this is str");
        assert_eq!(error("let d = [\"a\": 1]\nprint(1 in d)"), "`in` does not work between int and dict[str, int]");
        assert_eq!(error("let d = [\"a\": 1]\nprint(d.get(1))"), "the key given to `get` should be str, but this is int");
        assert_eq!(
            error("let d = [\"a\": 1]\nprint(d.get(\"a\", \"none\"))"),
            "what `get` gives when the key is not there should be int, but this is str"
        );
        assert_eq!(
            error("let d = [\"a\": 1]\nlet x: int = d.get(\"a\")"),
            "`x` should be int, but this is int or none. Check it with `is not none` first"
        );
        assert_eq!(error("let d = [\"a\": 1]\nd.pop()"), "`pop` takes 1 argument, but 0 were given");
        assert_eq!(error("let d = [1: \"a\"]\nfor k in d {\n print(k + \"x\")\n}"), "`+` does not work between int and str");
        assert_eq!(
            error("let d = [\"a\": 1]\nd.append(3)"),
            "dict[str, int] has no method `append`. A dict has `get`, `pop`, `keys`, `values` and `clear`, \
             and a value is put in with `d[key] = value`"
        );
        assert_eq!(
            error("print([\"a\": 1] == [\"a\": \"b\"])"),
            "`==` between dict[str, int] and dict[str, str] is always false"
        );
    }

    /// Two classes that are one interface, for the tests of what a dict
    /// can hold.
    const TWO_SHAPES: &str = "interface Shape {\n function area(self) -> float\n}\n\
        class Circle is Shape {\n r: float\n function area(self) -> float {\n  return self.r\n }\n}\n\
        class Square is Shape {\n s: float\n function area(self) -> float {\n  return self.s\n }\n}\n";

    #[test]
    fn a_test_is_checked() {
        // A test is checked with the program, in strict mode too. What
        // it declares is its own, and what it raises is nobody's to name.
        strict_ok(
            "function half(n: int) -> int raises ValueError {\n if n % 2 == 1 {\n  raise ValueError(\"odd\")\n }\n return n // 2\n}\n\
             test \"half of an even number\" {\n let got = half(4)\n assert got == 2\n assert half(8) > got\n}\n\
             test \"half of an odd number\" {\n assert raises ValueError {\n  half(3)\n }\n}\n\
             let got = \"outside\"\nprint(got)",
        );
        assert_eq!(error("test \"x\" {\n let n: int = \"a\"\n}"), "`n` should be int, but this is str");
        assert_eq!(error("test \"x\" {\n assert 5\n}"), "a condition must be true or false, but this is int");
        // What a failed `assert` prints is text (decision 60.1).
        strict_ok("test \"x\" {\n let n = 3\n assert n == 3, f\"n is {n}\"\n assert true, \"plain\"\n}");
        assert_eq!(
            error("test \"x\" {\n assert true, 5\n}"),
            "what an `assert` prints when it fails must be text, but this is int. Write `assert total == 6, f\"the \
             total was {total}\"`"
        );
        assert_eq!(error("test \"x\" {\n assert true, nothing\n}"), "`nothing` is not defined");
        assert_eq!(error("test \"x\" {\n assert raises Nothing {\n }\n}"), "`Nothing` is not a kind of error");
        assert_eq!(error("test \"x\" {\n return\n}"), "`return` outside a function");
        assert_eq!(error("test \"x\" {\n let inside = 1\n}\nprint(inside)"), "`inside` is not defined");
    }

    #[test]
    fn a_dict_holds_what_a_list_can() {
        let with = |rest: &str| format!("{TWO_SHAPES}{rest}");
        // Lists, objects, enums, dicts, quantities, functions and sized
        // numbers, each read back as what it is.
        strict_ok(&with(
            "enum Level {\n Low\n High(limit: int)\n}\n\
             let lists: dict[str, list[int]] = [\"odd\": [1, 3], \"none\": []]\n\
             lists[\"odd\"].append(5)\nlet numbers: list[int] = lists[\"odd\"]\nlet all: list[list[int]] = lists.values()\n\
             let circles = [\"a\": Circle(1.0)]\nlet c: Circle = circles[\"a\"]\nlet maybe: Circle or none = circles.get(\"b\")\n\
             let other: Circle = circles.get(\"b\", Circle(2.0))\nlet gone: Circle = circles.pop(\"a\")\n\
             let levels: dict[int, Level] = [1: Low, 2: High(7)]\nlet level: Level = levels[2]\n\
             let nested: dict[str, dict[str, int]] = [\"a\": [\"x\": 1], \"b\": [:]]\nlet inner: dict[str, int] = nested[\"a\"]\n\
             let times = [\"blink\": 500 ms]\nlet wait: ms = times[\"blink\"]\n\
             let steps: dict[str, function(int) -> int] = [\"double\": function(n: int) -> int {\n return n * 2\n}]\n\
             let n: int = steps[\"double\"](21)\n\
             let bytes: dict[str, u8] = [\"a\": 200]\nlet byte: u8 = bytes[\"a\"]\n\
             print(numbers, all, c, maybe, other, gone, level, inner, wait, n, byte)",
        ));
        // A dict that is written out is a dict of what the place wants:
        // here, of shapes. A dict of circles that is made already stays
        // what it is, as a list of circles does.
        strict_ok(&with(
            "function total(shapes: dict[str, Shape]) -> float {\n var sum = 0.0\n for name, shape in shapes {\n  sum += shape.area()\n }\n return sum\n}\n\
             let shapes: dict[str, Shape] = [\"a\": Circle(1.0), \"b\": Square(2.0)]\nshapes[\"c\"] = Square(3.0)\n\
             print(total(shapes), total([\"a\": Circle(1.0)]), shapes.get(\"z\", Circle(0.0)).area())",
        ));
        assert_eq!(
            error(&with("let circles = [\"a\": Circle(1.0)]\nlet shapes: dict[str, Shape] = circles")),
            "`shapes` should be dict[str, Shape], but this is dict[str, Circle]"
        );
        assert_eq!(
            error(&with("let shapes: dict[str, Shape] = [\"a\": Circle(1.0), \"b\": 5]")),
            "the values of this dict are Shape here, but this one is int"
        );
        assert_eq!(
            error(&with("let shapes = [\"a\": Circle(1.0), \"b\": Square(2.0)]")),
            "the values of a dict are all of one type. This one is Square, and the ones before it are Circle"
        );
        assert_eq!(
            error("let d = [\"a\": [1], \"b\": [\"x\"]]"),
            "the values of a dict are all of one type. This one is list[str], and the ones before it are list[int]"
        );
        assert_eq!(error("let d: dict[str, u8] = [\"a\": 300]"), "300 does not fit in a u8 (0 to 255)");
        // An empty list or dict beside one that says what it holds.
        ok("let d = [\"a\": [1], \"b\": []]\nlet e = [\"a\": [:], \"b\": [\"x\": 1]]\nprint(d, e)");
        // `get` says `none` for a key that is not there, so no value of
        // a dict is `none`.
        assert_eq!(
            error("let d: dict[str, int or none] = [:]"),
            "a value of a dict cannot be `int or none`: `get` gives `none` for a key that is not there, and that could \
             not be told from a value that is `none`. Make it a `dict[..., int]`, and leave the key out where there is no value"
        );
        assert_eq!(
            error("let d = [\"a\": none]"),
            "a value of a dict cannot be `none`: `get` gives `none` for a key that is not there. Leave the key out instead"
        );
        // Inside a value it is as anywhere.
        ok("let d: dict[str, list[int or none]] = [\"a\": [1, none]]\nprint(d)");
    }

    #[test]
    fn a_generic_can_have_a_dict_of_its_type_parameter() {
        strict_ok(
            "function first[T](d: dict[str, T], key: str) -> T {\n return d[key]\n}\n\
             function flip[K, V](d: dict[K, V]) -> dict[V, K] {\n var out: dict[V, K] = [:]\n for key, value in d {\n  out[value] = key\n }\n return out\n}\n\
             class Table[T] {\n rows: dict[str, T]\n function put(self, key: str, row: T) {\n  self.rows[key] = row\n }\n}\n\
             let n: int = first([\"a\": 1], \"a\")\nlet x: list[float] = first([\"b\": [1.5]], \"b\")\n\
             let back: dict[int, str] = flip([\"one\": 1])\n\
             let table: Table[list[str]] = Table([:])\ntable.put(\"k\", [\"v\"])\nprint(n, x, back, table.rows)",
        );
        // What `K` and `V` are is checked in each copy, which says which
        // copy it is.
        let wrong = errors(
            "function flip[K, V](d: dict[K, V]) -> dict[V, K] {\n var out: dict[V, K] = [:]\n return out\n}\nprint(flip([\"a\": 1.5]))",
        );
        assert_eq!(
            wrong[0],
            "a key of a dict is a str or an int, and this says float. Write `dict[str, ...]` or `dict[int, ...]`"
        );
        assert!(
            error("function wrap[T](x: T) -> dict[str, T] {\n return [:]\n}\nlet n: int or none = none\nprint(wrap(n))")
                .starts_with("a value of a dict cannot be `int or none`")
        );
    }

    #[test]
    fn a_loop_with_two_names_is_checked() {
        // A dict gives each key and its value. A list, text and a range
        // give the position, from 0, and what is there.
        strict_ok(
            "let ages = [\"Ada\": 36]\nfor name, age in ages {\n let n: str = name\n let a: int = age\n print(n, a)\n}\n\
             for at, word in [\"a\", \"b\"] {\n let i: int = at\n let w: str = word\n print(i, w)\n}\n\
             for at, letter in \"abc\" {\n let i: int = at\n let c: str = letter\n print(i, c)\n}\n\
             for trip, n in range(5, 8) {\n let i: int = trip\n let m: int = n\n print(i, m)\n}\n\
             for name, numbers in [\"odd\": [1, 3]] {\n let l: list[int] = numbers\n print(name, l)\n}",
        );
        assert_eq!(error("let ages = [\"Ada\": 36]\nfor name, age in ages {\n let n: int = name\n}"), "`n` should be int, but this is str");
        assert_eq!(error("for at, letter in \"abc\" {\n let n: str = at\n}"), "`n` should be str, but this is int");
        assert_eq!(error("for a, b in 5 {\n print(a)\n}"), "int cannot be looped over");
        // The two names are the loop's own, and gone after it.
        assert_eq!(error("for k, v in [\"a\": 1] {\n print(k)\n}\nprint(v)"), "`v` is not defined");
        assert_eq!(
            error("for k, v in [\"a\": 1] {\n v = 2\n}"),
            "`v` cannot be changed. Declare it with `var` to allow that"
        );
        // The dict can be changed inside: the loop goes over the pairs
        // as they were when it began.
        ok("let seen = [1: 1]\nfor n, count in seen {\n seen[n + 100] = count\n}\nprint(seen)");
        // With no collector the value is looked at, and the dict is whole
        // again after the loop.
        one_owner_ok("let ages = [\"Ada\": 36]\nfor name, age in ages {\n print(name, age)\n}\nprint(ages)");
    }

    #[test]
    fn with_one_owner_a_dict_has_one_too() {
        // Made, lent to a parameter, changed through the look, given back.
        one_owner_ok(
            "function fill(d: dict[str, int]) {\n d[\"k\"] = 1\n}\n\
             function make() -> dict[str, int] {\n let d: dict[str, int] = [:]\n fill(d)\n return d\n}\n\
             function drain(d: take dict[str, int]) -> int {\n return len(d)\n}\n\
             let d = make()\nprint(d)\nprint(drain(d))\nprint(len(make()), drain([\"a\": 1]))",
        );
        // A second name takes the dict over, as with a list.
        assert_eq!(
            one_owner_error("let a = [\"x\": 1]\nlet b = a\nprint(a)"),
            format!("`a` was given away on line 2 and {GONE}")
        );
        // A parameter only looks, so what it looks at cannot be given back.
        assert_eq!(
            one_owner_error("function keep(d: dict[str, int]) -> dict[str, int] {\n return d\n}"),
            "`d` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
             If it is a parameter, write `take` in front of its type to take the value over"
        );
        // In an ordinary build `owned` in front makes it so by request.
        ok("let d: owned dict[str, int] = [\"a\": 1]\nd[\"b\"] = 2\nprint(d)");
        // A dict of numbers or text frees what it holds in one step.
        one_owner_ok("let times = [\"blink\": 500 ms]\nlet bytes: dict[str, u8] = [\"a\": 200]\nprint(times, bytes)");
        // One of lists or objects owns them, as a list owns its items
        // (decision 56.12).
        one_owner_ok("let d = [\"a\": [1, 2]]\nprint(d)");
        one_owner_ok("class P {\n n: int\n}\nlet d: dict[str, P] = [:]\nd[\"a\"] = P(1)\nprint(d)");
    }

    #[test]
    fn a_dict_owns_its_values() {
        // With no collector a dict owns its lists and objects, as a list
        // owns its items (decision 56.12).
        const BANK: &str = "class Frame {\n id: int\n}\nlet d: dict[str, Frame] = [\"a\": Frame(1)]\n";
        let good = |source: &str| one_owner_ok(&format!("{BANK}{source}"));
        let bad = |source: &str| one_owner_error(&format!("{BANK}{source}"));
        // `d[key]` lends, `d[key] = value` takes, `pop` hands over, `clear`
        // frees, a loop lends each value, and `get(key)` lends or is `none`.
        good("print(d[\"a\"].id)\nd[\"b\"] = Frame(2)\nlet f = d.pop(\"a\")\nprint(f.id)\nd.clear()");
        good("for key, frame in d {\n print(key, frame.id)\n}\nprint(d.get(\"a\") is none, d.keys())");
        good("let f = Frame(3)\nd[\"c\"] = f\nlet e = [\"x\": [1], \"y\": [2, 3]]\ne[\"x\"].append(4)\nprint(e)");
        // What a look cannot do, with the dict's words.
        assert_eq!(
            bad("let f = d[\"a\"]"),
            "this value is an owned Frame, which has one owner: the dict. `[ ]` only looks at it. \
             Use it where it is, pass it to a parameter that looks, or take it out with `pop(key)`"
        );
        assert_eq!(
            bad("let g = d.get(\"a\")"),
            "`get` gives a look at an owned Frame that the dict owns, or `none`, and a look cannot be kept. \
             Test it where it is, or pass it to a parameter that looks. To take the value out of the dict, use `pop(key)`"
        );
        assert_eq!(
            bad("print(d.get(\"a\", Frame(2)).id)"),
            "a value of this dict is an owned Frame, which has one owner: the dict. `get` with a second value would \
             give either a look at the dict's or the value given, and nobody could tell who frees it. Write \
             `get(key)`, which gives a look or `none`, and test it with `is not none`"
        );
        assert_eq!(
            bad("let vs = d.values()"),
            "a value of this dict is an owned Frame, which has one owner: the dict. A list of the values would be a \
             second owner of each. Loop over the dict with `for key, value in` instead, which lends each value in turn"
        );
        // The three guards of `pop()`.
        assert_eq!(
            bad("for k, f in d {\n d[\"b\"] = Frame(2)\n}"),
            "the `for` on line 5 is walking a dict that `d` owns, and is still looking at its values. \
             Nothing can replace a value of this dict until the loop is over"
        );
        assert_eq!(
            bad("function f(e: dict[str, Frame]) -> Frame {\n return e.pop(\"a\")\n}"),
            "only the owner can take a value out of this dict: somebody else may have been lent it. Here `e` only looks at it"
        );
        assert_eq!(
            bad("print(d[\"a\"].id, d.pop(\"a\").id)"),
            "this line has already looked at `d`, and the look might still be in use. Do the other thing on a line of its own first"
        );
        assert!(bad("let f = Frame(1)\nd[\"b\"] = f\nprint(f.id)").contains(GONE));
        // With a collector a dict is shared, and these rules are not
        // asked for: what built before builds the same.
        ok("owned class P {\n n: int\n}\nlet d: dict[str, P] = [:]\nd[\"a\"] = P(1)\nlet vs = d.values()\nprint(d.get(\"a\", P(2)).n)");
    }

    #[test]
    fn more_of_math_is_checked() {
        strict_ok(
            "import math\nfrom math import atan2, e\n\
             let n: int = math.round(2.5)\n\
             let x: float = math.log(2) + math.log2(2) + math.log10(2) + math.exp(1) + math.e + e\n\
             let s: float = math.sin(30 deg) + math.cos(0.5 rad) + math.tan(1) + math.sin(0.5)\n\
             let a: rad = math.asin(0.5)\n\
             let b: rad = math.acos(0.5) + math.atan(1) + atan2(1, 2.5)\n\
             let d: deg = a.deg\n\
             let back: rad = d.rad\n\
             function turn(angle: rad) -> deg {\n    return (angle * 2).deg\n}\n\
             print(turn(b), math.sin(a), d > 20 deg, back)",
        );
        // An angle, and not a time or a voltage.
        assert_eq!(
            error("import math\nprint(math.sin(3 s))"),
            "`sin` needs a number or an angle, such as `30 deg`, but this is a quantity in s"
        );
        assert_eq!(
            error("import math\nprint(math.tan(\"a\"))"),
            "`tan` needs a number or an angle, such as `30 deg`, but this is str"
        );
        // What comes out is an angle in radians, not a plain number.
        assert_eq!(
            error("import math\nlet a: float = math.atan(1)"),
            "`a` should be float, but this is a quantity in rad"
        );
        assert_eq!(error("import math\nprint(math.asin(30 deg))"), "`asin` needs a number, but this is a quantity in deg");
        assert_eq!(error("import math\nprint(math.atan2(1))"), "`atan2` takes 2 arguments, but 1 was given");
        assert_eq!(error("import math\nprint(math.atan2(1, \"x\"))"), "`atan2` needs two numbers, but this is str");
        assert_eq!(error("import math\nprint(math.round(\"x\"))"), "`round` needs a number, but this is str");
        assert_eq!(error("import math\nprint(math.log())"), "`log` takes 1 argument, but 0 were given");
        assert_eq!(error("import math\nprint(math.tau)"), "module `math` has no `tau`");
        // Degrees and radians never meet by themselves, and the message
        // says how to bring them together.
        assert_eq!(
            error("import math\nprint(math.atan(1) + 90 deg)"),
            "`+` does not work between a quantity in rad and a quantity in deg. Change one of them first, with `.rad` or `.deg`"
        );
        assert_eq!(
            error("print(90 deg < 1 rad)"),
            "`<` does not work between a quantity in deg and a quantity in rad. Change one of them first, with `.deg` or `.rad`"
        );
        assert_eq!(
            error("function f(a: rad) -> rad { return a }\nprint(f(90 deg))"),
            "`a` of `f` should be a quantity in rad, but this is a quantity in deg. Convert it with `.rad`"
        );
        // Other pairs are as far apart as they were.
        assert_eq!(error("print((3 s).deg)"), "a quantity in s cannot be turned into deg. They measure different things");
        assert_eq!(error("print((3 degC).K)"), "a quantity in degC cannot be turned into K. They measure different things");
    }

    #[test]
    fn reading_and_writing_is_checked() {
        strict_ok(
            "import files\nimport program\n\
             function first(path: str) -> str raises FileError {\n    return files.read(path).lines()[0]\n}\n\
             function safe(path: str) -> int {\n\
             \x20   try {\n        files.append(path, \"x\")\n        files.remove(path)\n        return 1\n\
             \x20   } except FileError {\n        return 0\n    }\n}\n\
             let there: bool = files.exists(\"a\")\n\
             let words: list[str] = program.arguments()\n\
             let line: str or none = input(\"? \")\n\
             let lines: list[str] = \"a\\nb\".lines()\n\
             if line is not none { print(line.upper()) }\n\
             files.write(\"a\", \"b\")",
        );
        // A function that reads a file can fail, and has to say so.
        assert_eq!(
            strict_error("import files\nfunction first(path: str) -> str {\n    return files.read(path)\n}"),
            "`first` can raise FileError here but does not say so. Add `raises FileError` to it, or catch the error"
        );
        assert_eq!(
            strict_error("import files\nfunction clear(path: str) {\n    files.write(path, \"\")\n}"),
            "`clear` can raise FileError here but does not say so. Add `raises FileError` to it, or catch the error"
        );
        // `exists` is a question, and cannot fail.
        strict_ok("import files\nfunction there(path: str) -> bool {\n    return files.exists(path)\n}");
        assert_eq!(error("import files\nfiles.read(5)"), "an argument of `files.read` should be str, but this is int");
        assert_eq!(error("import files\nfiles.write(\"a\")"), "`write` takes 2 arguments, but 1 was given");
        assert_eq!(error("import files\nlet n: int = files.read(\"a\")"), "`n` should be int, but this is str");
        assert_eq!(error("import files\nfiles.open(\"a\")"), "module `files` has no `open`");
        // `program.run` gives the number the other program ended with,
        // and can fail: the program may not be there.
        strict_ok(
            "import program\n\
             function list_folder(folder: str) -> int raises FileError {\n    return program.run(\"ls\", [\"-l\", folder])\n}\n\
             function there(name: str) -> bool {\n\
             \x20   try {\n        return program.run(name, []) == 0\n    } except FileError {\n        return false\n    }\n}",
        );
        assert_eq!(
            strict_error("import program\nfunction go() -> int {\n    return program.run(\"ls\", [])\n}"),
            "`go` can raise FileError here but does not say so. Add `raises FileError` to it, or catch the error"
        );
        assert_eq!(error("import program\nprogram.run(\"ls\")"), "`run` takes 2 arguments, but 1 was given");
        assert_eq!(
            error("import program\nprogram.run(5, [])"),
            "the name of the program `program.run` starts should be str, but this is int"
        );
        assert_eq!(
            error("import program\nprogram.run(\"ls\", \"-l\")"),
            "the words `program.run` gives the program should be list[str], but this is str"
        );
        assert_eq!(
            error("import program\nprogram.run(\"ls\", [1])"),
            "the words `program.run` gives the program should be list[str], but this is list[int]"
        );
        assert_eq!(error("import program\nlet text: str = program.run(\"ls\", [])"), "`text` should be str, but this is int");
    }

    #[test]
    fn a_module_of_python_is_checked_when_the_program_runs() {
        // What Python has is known when it is asked, so everything is
        // let through: a field, a call with a named argument, an item.
        ok("import python\n\
            let yt = python.module(\"yt_dlp\")\n\
            let downloader = yt.YoutubeDL(yt.parse_options()[1], auto_init=true)\n\
            downloader.params[\"quiet\"] = true\n\
            let title: str = downloader.extract_info(\"x\", download=false)[\"title\"]\n\
            for entry in python.copy(downloader.cache) {\n    print(entry, title, len(downloader.params))\n}");
        ok("from python import module, copy\nprint(copy(module(\"math\").pi))");
        assert_eq!(error("import python\npython.module(5)"), "the name given to `python.module` should be str, but this is int");
        assert_eq!(error("import python\npython.module()"), "`module` takes 1 argument, but 0 were given");
        assert_eq!(error("import python\npython.copy()"), "`copy` takes 1 argument, but 0 were given");
        assert_eq!(error("import python\npython.eval(\"1\")"), "module `python` has no `eval`");
        // JavaScript's is let through in the same way.
        ok("import node\n\
            let fs = node.module(\"fs\")\n\
            let watcher = node.new(node.global(\"Map\"), [[1, fs.constants]])\n\
            watcher[\"k\"] = fs.readFileSync(\"x\", encoding=\"utf8\")\n\
            let size: int = fs.statSync(\"x\").size\n\
            for name in node.copy(fs.readdirSync(\".\")) {\n    print(name, size, len(watcher))\n}");
        ok("from node import module, global, new, copy\nprint(copy(new(global(\"Set\"))), module(\"os\").EOL)");
        assert_eq!(error("import node\nnode.module(5)"), "the name given to `node.module` should be str, but this is int");
        assert_eq!(error("import node\nnode.global()"), "`global` takes 1 argument, but 0 were given");
        assert_eq!(
            error("import node\nnode.new()"),
            "`new` takes the class to make an object of, and then what it is made from, but nothing was given"
        );
        assert_eq!(error("import node\nnode.require(\"fs\")"), "module `node` has no `require`");
        assert_eq!(
            strict_error("import node\nlet os = node.module(\"os\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // And Java's.
        ok("import java\n\
            java.jar(\"lib\")\n\
            let ArrayList = java.type(\"java.util.ArrayList\")\n\
            let names = ArrayList(16)\n\
            names.add(java.type(\"java.lang.Math\").PI)\n\
            let size: int = names.size()\n\
            for name in java.copy(names) {\n    print(name, size, len(names), names[0])\n}");
        ok("from java import type, jar, copy\njar(\"lib\")\nprint(copy(type(\"java.util.ArrayList\")()))");
        assert_eq!(error("import java\njava.type(5)"), "the name given to `java.type` should be str, but this is int");
        assert_eq!(error("import java\njava.jar(5)"), "the jar or folder given to `java.jar` should be str, but this is int");
        assert_eq!(error("import java\njava.type()"), "`type` takes 1 argument, but 0 were given");
        assert_eq!(error("import java\nlet nothing: int = java.jar(\"lib\")"), "`nothing` should be int, but this is none");
        assert_eq!(error("import java\njava.new(\"x\")"), "module `java` has no `new`");
        assert_eq!(
            strict_error("import java\nlet list = java.type(\"java.util.ArrayList\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // And Ruby's.
        ok("import ruby\n\
            ruby.require(\"date\")\n\
            let Date = ruby.constant(\"Date\")\n\
            let day = Date.new(2026, 11, 18)\n\
            let year: int = day.year()\n\
            let days = ruby.constant(\"Array\").new(2, day)\n\
            days[0] = ruby.symbol(\"none\")\n\
            for one in ruby.copy(days) {\n    print(one, year, len(days), days[0], day.send(\"leap?\"), Date.today(start=1))\n}");
        ok("from ruby import require, constant, symbol, copy\nrequire(\"set\")\nprint(copy(constant(\"Set\").new([symbol(\"a\")])))");
        assert_eq!(error("import ruby\nruby.constant(5)"), "the name given to `ruby.constant` should be str, but this is int");
        assert_eq!(error("import ruby\nruby.symbol(5)"), "the name given to `ruby.symbol` should be str, but this is int");
        assert_eq!(error("import ruby\nruby.require(5)"), "the library given to `ruby.require` should be str, but this is int");
        assert_eq!(error("import ruby\nruby.constant()"), "`constant` takes 1 argument, but 0 were given");
        assert_eq!(error("import ruby\nlet nothing: int = ruby.require(\"date\")"), "`nothing` should be int, but this is none");
        assert_eq!(error("import ruby\nruby.module(\"date\")"), "module `ruby` has no `module`");
        assert_eq!(
            strict_error("import ruby\nlet date = ruby.constant(\"Date\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // And PHP's.
        ok("import php\n\
            php.require(\"helper.php\")\n\
            let DateTime = php.type(\"DateTime\")\n\
            let day = DateTime(\"2026-11-18\")\n\
            let year: str = day.format(\"Y\")\n\
            let found = php.variable()\n\
            php.callable(\"preg_match\")(\"/a/\", \"a\", found, flags=php.constant(\"PREG_OFFSET_CAPTURE\"))\n\
            for one in php.copy(found) {\n    print(one, year, len(found.value), found.value[0], php.variable(1))\n}");
        ok("from php import type, callable, constant, require, variable, copy\nrequire(\"a.php\")\nprint(copy(type(\"ArrayObject\")([constant(\"M_PI\"), callable(\"strrev\"), variable()])))");
        assert_eq!(error("import php\nphp.type(5)"), "the name given to `php.type` should be str, but this is int");
        assert_eq!(error("import php\nphp.callable(5)"), "the name given to `php.callable` should be str, but this is int");
        assert_eq!(error("import php\nphp.constant(5)"), "the name given to `php.constant` should be str, but this is int");
        assert_eq!(error("import php\nphp.require(5)"), "the file given to `php.require` should be str, but this is int");
        assert_eq!(error("import php\nphp.type()"), "`type` takes 1 argument, but 0 were given");
        assert_eq!(error("import php\nphp.variable(1, 2)"), "`variable` takes 0 to 1 arguments, but 2 were given");
        assert_eq!(error("import php\nlet nothing: int = php.require(\"a.php\")"), "`nothing` should be int, but this is none");
        assert_eq!(error("import php\nphp.module(\"date\")"), "module `php` has no `module`");
        assert_eq!(
            strict_error("import php\nlet date = php.type(\"DateTime\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // And Lua's.
        ok("import lua\n\
            let string = lua.global(\"string\")\n\
            let json = lua.require(\"dkjson\")\n\
            let size: int = string.len(\"abc\")\n\
            let parts = string.byte(\"abc\", 1, 3)\n\
            for one in lua.copy(json.decode(\"[1]\")) {\n    print(one, size, len(parts), parts[0], json.encode([1], indent=true))\n}");
        ok("from lua import require, global, copy\nprint(copy(global(\"table\")), require(\"lpeg\"))");
        assert_eq!(error("import lua\nlua.require(5)"), "the name given to `lua.require` should be str, but this is int");
        assert_eq!(error("import lua\nlua.global(5)"), "the name given to `lua.global` should be str, but this is int");
        assert_eq!(error("import lua\nlua.global()"), "`global` takes 1 argument, but 0 were given");
        assert_eq!(error("import lua\nlua.module(\"lpeg\")"), "module `lua` has no `module`");
        assert_eq!(
            strict_error("import lua\nlet string = lua.global(\"string\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // And those of .NET.
        ok("import csharp\n\
            csharp.assembly(\"lib\")\n\
            let builder = csharp.type(\"System.Text.StringBuilder\")()\n\
            builder.Append(\"abc\")\n\
            let size: int = builder.Length\n\
            for one in csharp.copy(csharp.type(\"System.IO.Directory\").GetFiles(\".\")) {\n    print(one, size, builder.ToString())\n}");
        ok("from csharp import type, assembly, copy\nassembly(\"lib/Humanizer.dll\")\nprint(copy(type(\"System.Math\").Max(1, 2)))");
        assert_eq!(error("import csharp\ncsharp.type(5)"), "the name given to `csharp.type` should be str, but this is int");
        assert_eq!(error("import csharp\ncsharp.assembly(5)"), "the .dll or folder given to `csharp.assembly` should be str, but this is int");
        assert_eq!(error("import csharp\ncsharp.type()"), "`type` takes 1 argument, but 0 were given");
        assert_eq!(error("import csharp\nlet nothing: int = csharp.assembly(\"lib\")"), "`nothing` should be int, but this is none");
        assert_eq!(error("import csharp\ncsharp.module(\"System\")"), "module `csharp` has no `module`");
        assert_eq!(
            strict_error("import csharp\nlet math = csharp.type(\"System.Math\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // Strict mode wants every type before the program runs, and
        // Python's are not known then: the same as for `json.parse`.
        assert_eq!(
            strict_error("import python\nlet math = python.module(\"math\")"),
            "strict mode: the type of this cannot be worked out before the program runs"
        );
        // A line may not be there, so it is tested before it is used.
        assert_eq!(
            error("print(input().upper())"),
            "this is str or none. Check it with `is not none` before using `.upper`"
        );
        assert_eq!(
            error("let name: str = input()"),
            "`name` should be str, but this is str or none. Check it with `is not none` first"
        );
        assert_eq!(error("input(5)"), "the question `input` asks should be str, but this is int");
        assert_eq!(error("input(\"a\", \"b\")"), "`input` takes 0 to 1 arguments, but 2 were given");
        assert_eq!(error("import program\nprogram.arguments(1)"), "`arguments` takes 0 arguments, but 1 was given");
        assert_eq!(error("import program\nprint(program.name)"), "module `program` has no `name`");
        assert_eq!(error("print(\"a\".lines(1))"), "`lines` takes 0 arguments, but 1 was given");
        // With one owner, each of these lists is new and can be owned.
        one_owner_ok(
            "import program\n\
             let words: owned list[str] = program.arguments()\n\
             let lines: owned list[str] = \"a\\nb\".lines()\n\
             print(len(words), len(lines))",
        );
    }

    #[test]
    fn with_one_owner_a_list_method_that_takes_out_is_for_the_owner() {
        let task = "class Task {\n name: str\n}\n";
        one_owner_ok(&format!(
            "{task}let tasks = [Task(\"a\"), Task(\"b\")]\ntasks.insert(0, Task(\"c\"))\nlet first = tasks.pop(0)\n\
             print(first.name)\nprint(tasks.find(Task(\"b\")))\ntasks.reverse()\ntasks.clear()"
        ));
        // `clear` frees every item, and `pop(0)` takes one out: neither
        // is for a parameter that looks, or for a loop that is walking.
        assert_eq!(
            one_owner_error(&format!("{task}function empty(tasks: list[Task]) {{\n tasks.clear()\n}}")),
            "only the owner can take an item out of this list: somebody else may have been lent it. \
             Here `tasks` only looks at it"
        );
        assert_eq!(
            one_owner_error(&format!("{task}function first(tasks: list[Task]) -> Task {{\n return tasks.pop(0)\n}}")),
            "only the owner can take an item out of this list: somebody else may have been lent it. \
             Here `tasks` only looks at it"
        );
        assert_eq!(
            one_owner_error(&format!("{task}let tasks = [Task(\"a\")]\nfor task in tasks {{\n tasks.clear()\n}}")),
            "the `for` on line 5 is walking a list that `tasks` owns, and is still looking at its items. \
             Nothing can take an item out of this list until the loop is over"
        );
        // A slice would hold the same items as the list it came from.
        assert_eq!(
            one_owner_error(&format!("{task}let tasks = [Task(\"a\")]\nlet some = tasks.slice(0, 1)\nprint(len(some))")),
            "`slice` would put the items it takes in a second list, and each has one owner. \
             Use a `for` loop that makes what the new list needs"
        );
        // Numbers and text are copied, so any list of them can be sliced.
        one_owner_ok("function tail(xs: list[int]) -> list[int] {\n return xs.slice(1)\n}\nprint(tail([1, 2, 3]))");
    }

    #[test]
    fn a_function_that_takes_out_can_stand_for_one_of_an_interface() {
        // 0.7.0 refused `Stack` here. Now a call through a `Pile` is held
        // to the guards of taking out, as a call on a `Stack` is (58.21).
        let pile = "class Task {\n name: str\n}\ninterface Pile {\n function take_last(self) -> Task\n function size(self) -> int\n}\n";
        let stack = "class Stack is Pile {\n tasks: list[Task]\n function take_last(self) -> Task {\n  return self.tasks.pop()\n }\n \
                     function size(self) -> int {\n  return len(self.tasks)\n }\n}\n";
        let only_the_owner = "only the owner can call `take_last`, which takes out or replaces something the object owns \
                              (`Stack` is a `Pile`, and its `take_last` does): somebody else may have been lent it. \
                              Here `pile` only looks at it";
        let drain = "function drain(pile: take Pile) {\n while pile.size() > 0 {\n  print(pile.take_last().name)\n }\n}\n\
                     drain(Stack([Task(\"a\"), Task(\"b\")]))\n";
        one_owner_ok(&format!("{pile}{stack}{drain}"));
        let look = "function look(pile: Pile) -> str {\n return pile.take_last().name\n}\n";
        assert_eq!(one_owner_error(&format!("{pile}{stack}{look}")), only_the_owner);
        // A call read before the class that takes out is held to the
        // same guards as one read after it.
        assert_eq!(one_owner_error(&format!("{pile}{look}{stack}")), only_the_owner);
        // A function that only looks, through the same interface, is
        // for anybody, as before.
        one_owner_ok(&format!("{pile}{stack}function count(pile: Pile) -> int {{\n return pile.size()\n}}\n"));
        // Not in a loop that walks the list the object is in, and not on
        // a line that has already looked at the same object.
        assert_eq!(
            one_owner_error(&format!(
                "{pile}function all(piles: take list[Pile]) {{\n for one in piles {{\n  print(piles[0].take_last().name)\n }}\n}}\n{stack}"
            )),
            "the `for` on line 9 is walking a list that `piles` owns, and is still looking at its items. Nothing can \
             call `take_last`, which takes out or replaces something the object owns (`Stack` is a `Pile`, and its \
             `take_last` does) until the loop is over"
        );
        assert_eq!(
            one_owner_error(&format!(
                "{pile}{stack}function twice(pile: take Pile) {{\n \
                 print(pile.size(), pile.take_last().name)\n}}\n"
            )),
            "this line has already looked at `pile`, and the look might still be in use. Do the other thing on a line of its own first"
        );
        // An interface whose classes take nothing out is as it was.
        let quiet = "class Quiet is Pile {\n function take_last(self) -> Task {\n  return Task(\"q\")\n }\n \
                     function size(self) -> int {\n  return 0\n }\n}\n";
        one_owner_ok(&format!("{pile}{quiet}{look}"));
        // The class in a second file.
        let loaded = crate::modules::testing::load(&format!(
            "import piles\nfunction look(pile: piles.Pile) -> str {{\n return pile.take_last().name\n}}\n==> piles.zeno\n{pile}{stack}"
        ));
        let rules = super::Rules { strict: true, one_owner: true, firmware: true };
        let found: Vec<String> = super::check_by(&loaded.program.unwrap(), rules).0.into_iter().map(|error| error.message).collect();
        assert_eq!(found, vec![only_the_owner.replace("`Stack` is a `Pile`", "`piles.Stack` is a `piles.Pile`")]);
        // A copy of a generic class, made after the call.
        let boxed = "class Box[T] is Pile {\n tasks: list[Task]\n extra: T\n function take_last(self) -> Task {\n  return self.tasks.pop()\n }\n \
                     function size(self) -> int {\n  return 0\n }\n}\nlet b: Pile = Box([Task(\"a\")], 1)\n";
        assert_eq!(one_owner_error(&format!("{pile}{look}{boxed}")), only_the_owner.replace("`Stack`", "`Box[int]`"));
        // A class that calls through its own interface: `count` takes
        // nothing out, and a call of it on a loop's item is for anybody;
        // `strip` does, and one on a loop's item is not.
        let tree = |strip: &str| {
            format!(
                "interface Tree {{\n function count(self) -> int\n function strip(self)\n}}\n\
                 class Node is Tree {{\n kids: list[Tree]\n function count(self) -> int {{\n  var n = 1\n  for kid in self.kids {{\n   \
                 n += kid.count()\n  }}\n  return n\n }}\n function strip(self) {{\n{strip} }}\n}}\n\
                 class Leaf is Tree {{\n function count(self) -> int {{\n  return 1\n }}\n function strip(self) {{\n }}\n}}\n\
                 let t = Node([Node([Leaf()]), Leaf()])\nt.strip()\nprint(t.count())\n"
            )
        };
        one_owner_ok(&tree("  while len(self.kids) > 0 {\n   var kid = self.kids.pop()\n   kid.strip()\n  }\n"));
        one_owner_ok(&tree("  for kid in self.kids {\n   kid.strip()\n  }\n"));
        assert_eq!(
            one_owner_error(&tree("  for kid in self.kids {\n   kid.strip()\n  }\n  self.kids.clear()\n")),
            "only the owner can call `strip`, which takes out or replaces something the object owns (`Node` is a \
             `Tree`, and its `strip` does): somebody else may have been lent it. Here `kid` only looks at it"
        );
    }

    #[test]
    fn an_owned_interface_has_one_owner_with_a_collector_too() {
        // Each object that is an `owned interface` has one owner, in a
        // build with a collector as with none (58.22).
        let shapes = "owned class Point {\n x: int\n}\nowned interface Shape {\n function area(self) -> float\n \
                      function move_to(self, at: take Point)\n}\nowned class Circle is Shape {\n radius: float\n centre: Point\n \
                      function area(self) -> float {\n  return self.radius\n }\n function move_to(self, at: take Point) {\n  \
                      self.centre = at\n }\n}\nowned class Frame {\n inside: Shape\n spare: Shape or none = none\n}\n";
        let uses = "function total(shapes: owned list[Shape]) -> float {\n var sum = 0.0\n for shape in shapes {\n  sum += shape.area()\n }\n return sum\n}\n\
                    function frame(shape: take Shape) -> Frame {\n return Frame(shape)\n}\n\
                    function look(shape: Shape) -> float {\n return shape.area()\n}\n\
                    var all: owned list[Shape] = [Circle(1.0, Point(0))]\nprint(total(all), look(all[0]))\nall[0].move_to(Point(2))\n\
                    let f = frame(all.pop())\nf.spare = Circle(2.0, Point(1))\nf.inside.move_to(Point(3))\nprint(f)\n";
        strict_ok(&format!("{shapes}{uses}"));
        one_owner_ok(&format!("{shapes}{uses}"));
        // Given away, it is gone, as an object of an `owned class` is.
        assert_eq!(
            error(&format!("{shapes}let c: Shape = Circle(1.0, Point(0))\nlet f = Frame(c)\nprint(c.area())")),
            "`c` was given away on line 23 and holds nothing now. Use it before it goes, or give it only to what looks: \
             a parameter without `take`"
        );
        // A list of them needs an owner, as a list of objects of an
        // `owned class` does.
        assert_eq!(
            error(&format!("{shapes}let all: list[Shape] = [Circle(1.0, Point(0))]")),
            "each item of this list is an owned Shape, which has one owner: the list. So the list needs an owner too. \
             Write `owned list[...]`"
        );
        // A function that replaces what its object owns is for the owner,
        // through the interface as on the object (58.21).
        assert_eq!(
            error(&format!("{shapes}function far(shape: Shape) {{\n shape.move_to(Point(9))\n}}")),
            "only the owner can call `move_to`, which takes out or replaces something the object owns (`Circle` is a \
             `Shape`, and its `move_to` does): somebody else may have been lent it. Here `shape` only looks at it"
        );
    }

    /// The messages of a firmware build's check, which is strict.
    fn firmware_errors(source: &str) -> Vec<String> {
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
        let rules = super::Rules { strict: true, one_owner: false, firmware: true };
        super::check_by(&program, rules).0.into_iter().map(|error| error.message).collect()
    }

    const PARSE: &str = "function parse(text: str) -> int raises ValueError {\n    return int(text)\n}\n\
                         function size(text: str) -> int {\n    return len(text)\n}\n";

    #[test]
    fn a_call_through_a_function_type_that_raises_is_checked_as_raising() {
        // It must be caught, or said by the function around it
        // (decision 56.1), as a call of `parse` by its name must.
        let apply = "function apply(convert: function(str) -> int raises ValueError, text: str) -> int";
        assert_eq!(
            strict_error(&format!("{PARSE}{apply} {{\n    return convert(text)\n}}\nprint(apply(parse, \"1\"))")),
            "`apply` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        strict_ok(&format!("{PARSE}{apply} raises ValueError {{\n    return convert(text)\n}}\nprint(apply(parse, \"1\"))"));
        strict_ok(&format!(
            "{PARSE}{apply} {{\n    try {{\n        return convert(text)\n    }} except ValueError {{\n        return -1\n    }}\n}}\n\
             print(apply(parse, \"x\"), apply(size, \"abc\"))"
        ));
        // A type with no `raises` is checked as it always was.
        strict_ok(&format!("{PARSE}function apply(convert: function(str) -> int, text: str) -> int {{\n    return convert(text)\n}}\nprint(apply(size, \"1\"))"));
        // What the type says is part of it, in a message.
        assert_eq!(
            error(&format!("{PARSE}let f: function(str) -> int raises ValueError = parse\nlet n: int = f")),
            "`n` should be int, but this is function(str) -> int raises ValueError"
        );
        assert_eq!(error("function f(g: function() raises Nope) {\n}\n"), "`Nope` is not a kind of error");
    }

    #[test]
    fn what_a_function_raises_fits_a_type_that_catches_it() {
        // A function that raises the kind, or nothing, fits; so does any
        // kind where the type says `raises Error` (decision 56.4).
        let wants = |raises: &str| format!("function apply(convert: function(str) -> int {raises}, text: str) {{\n    print(1)\n}}\n");
        strict_ok(&format!("{PARSE}{}apply(parse, \"1\")\napply(size, \"1\")", wants("raises ValueError")));
        strict_ok(&format!("{PARSE}{}apply(parse, \"1\")\napply(size, \"1\")", wants("raises Error")));
        let message = "`convert` of `apply` should be function(str) -> int raises FileError, but this function can raise \
                       ValueError, and the type says it raises only FileError. Write `raises Error` on the type to take \
                       a function that raises any kind";
        assert_eq!(strict_error(&format!("{PARSE}{}apply(parse, \"1\")", wants("raises FileError"))), message);
        // A value of one function type into another.
        let message = "`g` should be function(str) -> int raises FileError, but this function can raise \
                       ValueError, and the type says it raises only FileError. Write `raises Error` on the type to take \
                       a function that raises any kind";
        assert_eq!(
            strict_error(&format!(
                "{PARSE}let f: function(str) -> int raises ValueError = parse\nlet g: function(str) -> int raises FileError = f"
            )),
            message
        );
        // A function written where a value goes is held to it as well.
        strict_ok(&format!("{}apply(function(t: str) -> int raises ValueError {{ return int(t) }}, \"1\")", wants("raises ValueError")));
    }

    #[test]
    fn an_ordinary_build_does_not_hold_what_a_function_raises_against_a_plain_type() {
        // As it never has (decision 56.4): a raising function, or a value
        // of a raising type, goes where the type says nothing. A firmware
        // build calls the two in different ways, so there it is refused.
        let plain = "function apply(convert: function(str) -> int, text: str) -> int {\n    return convert(text)\n}\n";
        let both = [
            format!("{PARSE}{plain}print(apply(parse, \"1\"))"),
            format!("{PARSE}{plain}let f: function(str) -> int raises ValueError = parse\nprint(apply(f, \"1\"))"),
        ];
        let message = "`convert` of `apply` should be function(str) -> int, which raises nothing, but this function can \
                       raise ValueError. In a firmware build a call through a function type raises only what the type \
                       says: write `raises ValueError` after what it returns, or build without `--firmware`";
        for source in &both {
            strict_ok(source);
            assert_eq!(firmware_errors(source), vec![message], "\n{source}");
        }
        // `var f = parse` gets a type that says it raises in a firmware
        // build, and none in any other, as before (5.19l).
        let kept = format!("{PARSE}function run() -> int raises ValueError {{\n    var f = parse\n    return f(\"1\")\n}}\nprint(run())");
        strict_ok(&kept);
        assert_eq!(firmware_errors(&kept), Vec::<String>::new());
        let unsaid = format!("{PARSE}function run() -> int {{\n    var f = parse\n    return f(\"1\")\n}}\nprint(run())");
        strict_ok(&unsaid);
        assert_eq!(
            firmware_errors(&unsaid),
            vec!["`run` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"]
        );
    }

    #[test]
    fn a_firmware_build_cannot_make_a_plain_function_value_raise() {
        // A function of the program that raises nothing is made as one
        // that can, where a raising type wants it (decision 56.5). A
        // value already kept as a plain function cannot change how it is
        // called.
        let apply = "function apply(convert: function(str) -> int raises ValueError, text: str) -> int raises ValueError {\n\
                     return convert(text)\n}\n";
        assert_eq!(firmware_errors(&format!("{PARSE}{apply}print(apply(size, \"1\"))")), Vec::<String>::new());
        assert_eq!(
            firmware_errors(&format!("{PARSE}{apply}print(apply(function(t: str) -> int {{ return 2 }}, \"1\"))")),
            Vec::<String>::new()
        );
        let kept = format!("{PARSE}{apply}let f: function(str) -> int = size\nprint(apply(f, \"1\"))");
        strict_ok(&kept);
        assert_eq!(
            firmware_errors(&kept),
            vec![
                "`convert` of `apply` should be function(str) -> int raises ValueError, and this is a function value \
                 that raises nothing. In a firmware build a function that can raise is called in another way from \
                 one that cannot, so one kept as the first cannot become the second. Give it the type \
                 `function(str) -> int raises ValueError` where it is made, or build without `--firmware`"
            ]
        );
    }

    #[test]
    fn a_second_kind_after_raises_in_a_type_is_named_as_one() {
        // In a list of parameters the type ends at the comma, so a second
        // kind reads as a parameter with no type (decision 56.3).
        assert_eq!(error("function f(g: function() raises ValueError, FileError) {\n}\n"), crate::parser::ONE_KIND);
        // A parameter with any other name is one.
        ok("function f(g: function() raises ValueError, text) {\n    print(text)\n}\nf(function() { }, \"a\")\n");
    }

    #[test]
    fn a_function_type_in_a_message_is_written_as_in_a_program() {
        // Brackets where `raises` or `or none` would be read otherwise.
        let make = "function make() -> (function(int) -> int raises ValueError) {\n\
                    return function(n: int) -> int raises ValueError { return n }\n}\n";
        assert_eq!(
            error(&format!("{make}let n: int = make")),
            "`n` should be int, but this is a function"
        );
        assert_eq!(
            error(&format!("{make}let g: function() -> (function(int) -> int raises ValueError) = make\nlet n: int = g")),
            "`n` should be int, but this is function() -> (function(int) -> int raises ValueError)"
        );
        assert_eq!(
            error("let f: (function(int) -> int) or none = none\nlet n: int = f"),
            "`n` should be int, but this is (function(int) -> int) or none. Check it with `is not none` first"
        );
    }

    #[test]
    fn a_function_that_is_kept_takes_what_it_uses() {
        // Returned, kept in a variable, or given to a parameter that says
        // `take`, a function written where a value goes takes each value
        // with one owner that it uses (decision 56.8).
        let compose = "function compose(f: take function(int) -> int, g: take function(int) -> int) -> function(int) -> int {\n\
                       return function(n: int) -> int { return f(g(n)) }\n}\n\
                       function add(k: int) -> function(int) -> int {\n    return function(n: int) -> int { return n + k }\n}\n";
        one_owner_ok(&format!("{compose}let h = compose(add(1), add(10))\nprint(h(5), h(6))"));
        let inside = |body: &str| one_owner(&format!("function f() {{\n{body}\n}}\nf()\n"));
        // What it took is gone from the function around it.
        assert_eq!(
            inside("    let xs = [1, 2]\n    let g = function() -> int { return len(xs) }\n    print(len(xs), g())"),
            vec!["`xs` was given away on line 3 and holds nothing now. Use it before it goes, or give it only to what \
                  looks: a parameter without `take`"]
        );
        // A value of a function further out: the one in between may be
        // called again.
        assert_eq!(
            inside("    let xs = [1, 2]\n    let h = function() -> int {\n        let g = function() -> int { return len(xs) }\n\
                    return g()\n    }\n    print(h())"),
            vec!["this function is kept, so it takes `xs`, but `xs` belongs to a function further out, and the function \
                  in between may be called again with nothing left to give. Hand `xs` to the function in between as a \
                  parameter that says `take`"]
        );
        // The line uses it too.
        let both = "function both(a: list[int], g: take function() -> int) -> int {\n    return len(a) + g()\n}\n";
        assert_eq!(
            one_owner(&format!("{both}function f() {{\n    let xs = [1, 2]\n    print(both(xs, function() -> int {{ return len(xs) }}))\n}}\nf()\n")),
            vec!["this function is kept, so it takes `xs`, on a line that also uses it. Do the other thing on a line of its own first"]
        );
        // Each trip round a loop would take it again.
        assert_eq!(
            inside("    let xs = [1, 2]\n    for i in range(2) {\n        let g = function() -> int { return len(xs) }\n        print(g())\n    }"),
            vec!["`xs` is given away inside a loop, and the loop may come round again with nothing in it. Give it away \
                  after the loop, or just before a `break`"]
        );
        // One made inside the loop is the loop's to give.
        assert_eq!(inside("    for i in range(2) {\n        let xs = [i]\n        let g = function() -> int { return len(xs) }\n        print(g())\n    }"), Vec::<String>::new());
        // A list a loop is walking.
        let walked = "class P {\n    n: int\n}\nfunction f() {\n    let ps = [P(1)]\n    for p in ps {\n\
                      let g = function() -> int { return len(ps) }\n        print(g())\n        break\n    }\n}\nf()\n";
        assert_eq!(
            one_owner(walked),
            vec!["the `for` on line 6 is walking a list that `ps` owns, and is still looking at its items. `ps` cannot be \
                  given away until the loop is over"]
        );
        // A `var` of the function around stays refused: nothing is shared.
        assert_eq!(
            one_owner("function f() -> function() -> int {\n    var n = 1\n    return function() -> int { return n }\n}\nprint(f()())\n"),
            vec![super::shared("n")]
        );
        // Passed along, it still only looks.
        assert_eq!(inside("    let xs = [1, 2]\n    print([1].map(function(n: int) -> int { return n + len(xs) }), len(xs))"), Vec::<String>::new());
        // `self` cannot be taken, and `take` cannot be written on it.
        assert_eq!(
            one_owner("class Box {\n    xs: list[int]\n    function size(self) -> function() -> int {\n\
                       return function() -> int { return len(self.xs) }\n    }\n}\nprint(Box([1]).size()())\n"),
            vec!["this function is kept, so it takes `self`, but `self` is the object the method was called on, and its \
                  owner still has it. Put what the function needs in a `let` first, and use that"]
        );

        // With a collector too, a `var` that says `owned` is refused:
        // it could be given a new value after the function took it.
        const VAR: &str = "this function is kept, so it takes `xs`, but `xs` is a `var`: the function around it could \
                           give it a new value after this one took it. Declare it with `let`";
        let var = "function f() -> int {\n    var xs: owned list[int] = [1, 2]\n    let g = function() -> int { return len(xs) }\n\
                   return g()\n}\nprint(f())\n";
        let again = "function f() -> int {\n    var xs: owned list[int] = [1, 2]\n    let g = function() -> int { return len(xs) }\n\
                     xs = [3, 4, 5]\n    return g()\n}\nprint(f())\n";
        for source in [var, again] {
            assert_eq!(errors(source), vec![VAR]);
            assert_eq!(check(source, true), vec![VAR]);
            assert_eq!(firmware_errors(source), vec![VAR]);
        }
        // A `let` that says `owned`, and a parameter that says `take`,
        // are taken in every build.
        let owned = "function f() -> function() -> int {\n    let xs: owned list[int] = [1, 2]\n\
                     return function() -> int { return len(xs) }\n}\n\
                     function g(ys: take owned list[int]) -> function() -> int {\n\
                     return function() -> int { return len(ys) }\n}\nprint(f()(), g([1, 2, 3])())\n";
        assert_eq!(check(owned, true), Vec::<String>::new());
        assert_eq!(firmware_errors(owned), Vec::<String>::new());
        assert_eq!(one_owner(owned), Vec::<String>::new());
    }

    #[test]
    fn a_method_used_as_a_value_is_a_function_that_calls_it() {
        // `c.bump` is `function(n: int) -> int { return c.bump(n) }`,
        // written out by the checker for the compilers (decision 56.9).
        let counter = "class Counter {\n    count: int\n    function bump(self, by: int) -> int {\n\
                       self.count += by\n        return self.count\n    }\n\
                       function check(self, text: str) -> int raises ValueError {\n        return int(text) + self.count\n    }\n}\n\
                       function apply(f: function(int) -> int, n: int) -> int {\n    return f(n)\n}\n";
        let with = |rest: &str| format!("{counter}{rest}");
        // It has the type of a function, so it goes where one goes.
        ok(&with("let c = Counter(0)\nlet f: function(int) -> int = c.bump\nprint(f(1), apply(c.bump, 2))"));
        strict_ok(&with("let c = Counter(0)\nlet fs: list[function(int) -> int] = [c.bump]\nprint(fs[0](1))"));
        // What it raises is part of its type.
        assert_eq!(firmware_errors(&with("let c = Counter(0)\nlet p: function(str) -> int raises ValueError = c.check\n\
                                         try {\n    print(p(\"1\"))\n} except ValueError {\n    print(0)\n}\n")), Vec::<String>::new());
        assert_eq!(
            strict_error(&with("function f(c: Counter) {\n    let p = c.check\n    print(p(\"1\"))\n}\n")),
            "`f` can raise ValueError here but does not say so. Add `raises ValueError` to it, or catch the error"
        );
        // With one owner, passed along it looks at the object...
        let inside = |body: &str| one_owner(&with(&format!("function f() {{\n{body}\n}}\nf()\n")));
        assert_eq!(inside("    let c = Counter(0)\n    print(apply(c.bump, 1), [1, 2].map(c.bump), c.count)"), Vec::<String>::new());
        // ...and kept it takes it.
        assert_eq!(
            inside("    let c = Counter(0)\n    let m = c.bump\n    print(m(1), c.count)"),
            vec!["`c` was given away on line 16 and holds nothing now. Use it before it goes, or give it only to what \
                  looks: a parameter without `take`"]
        );
        one_owner_ok(&with("function keep(c: take Counter) -> function(int) -> int {\n    return c.bump\n}\nprint(keep(Counter(1))(2))\n"));
        // A `var`, and `self`, are refused as for any function that is kept.
        assert_eq!(inside("    var c = Counter(0)\n    let m = c.bump\n    print(m(1))"), vec![super::shared("c")]);
        let own = "class Box {\n    n: int\n    function get(self) -> int {\n        return self.n\n    }\n\
                   function getter(self) -> function() -> int {\n        return self.get\n    }\n}\nprint(Box(1).getter()())\n";
        assert_eq!(
            one_owner(own),
            vec!["this function is kept, so it takes `self`, but `self` is the object the method was called on, and its \
                  owner still has it. Put what the function needs in a `let` first, and use that"]
        );
        // With a collector the object is shared, as it is in `zeno run`.
        strict_ok(own);
        // Through an interface, the method of the object's own class.
        strict_ok("interface Shape {\n    function area(self) -> int\n}\nclass Square is Shape {\n    side: int\n\
                   function area(self) -> int {\n        return self.side * self.side\n    }\n}\n\
                   let s: Shape = Square(2)\nlet f = s.area\nprint(f())\n");
    }

    #[test]
    fn take_leaves_none_where_a_value_was() {
        // `take slot.task` is what the field holds, and leaves `none`
        // there (decision 56.10).
        let classes = "class Task {\n    id: int\n}\nclass Slot {\n    task: Task or none\n    id: int\n}\n\
                       function look(a: Task or none, b: Task or none) -> int {\n    return 1\n}\n";
        let with = |rest: &str| format!("{classes}{rest}");
        // Its type is the place's, in every build.
        for source in [
            with("let s = Slot(Task(1), 1)\nlet t: Task or none = take s.task\nprint(t is none, s.task)"),
            with("let ts: list[Task or none] = [Task(1), none]\nlet t = take ts[0]\nprint(t, ts)"),
            with("let ts: list[str or none, 2] = [\"a\", none]\nlet t: str or none = take ts[0]\nprint(t)"),
        ] {
            assert_eq!(errors(&source), Vec::<String>::new(), "{source}");
            assert_eq!(check(&source, true), Vec::<String>::new(), "{source}");
            assert_eq!(firmware_errors(&source), Vec::<String>::new(), "{source}");
            assert_eq!(one_owner(&source), Vec::<String>::new(), "{source}");
        }
        // The place must allow `none`.
        assert_eq!(
            error(&with("let s = Slot(none, 1)\nlet n = take s.id")),
            "`take` leaves `none` in `id`, and `id` is written `int`, which does not allow `none`. If it may be \
             empty, declare it as `id: int or none`"
        );
        assert_eq!(
            error(&with("let ts = [Task(1)]\nlet t = take ts[0]")),
            "`take` leaves `none` in the place of the item, and the items of this list are written `Task`, which \
             does not allow `none`. To take the item out, use `pop()`. To leave `none` in its place, make it a \
             `list[Task or none]`"
        );
        assert_eq!(
            error(&with("let t = take \"abc\"[0]")),
            "`take` leaves `none` in the place of an item, and text is never changed. Read the character with `[ ]`"
        );
        // With one owner, the guards of `pop()` hold.
        let inside = |body: &str| one_owner(&with(&format!("function f() {{\n{body}\n}}\nf()\n")));
        assert_eq!(
            one_owner(&with("function f(s: Slot) -> Task or none {\n    return take s.task\n}\n")),
            vec!["only the owner can take out what is there: somebody else may have been lent it. Here `s` only looks at it"]
        );
        one_owner_ok(&with("function f(s: take Slot) -> Task or none {\n    return take s.task\n}\nprint(f(Slot(Task(1), 2)))\n"));
        assert_eq!(
            inside("    let ts: list[Task or none] = [Task(1)]\n    for t in ts {\n        let x = take ts[0]\n    }"),
            vec!["the `for` on line 13 is walking a list that `ts` owns, and is still looking at its items. Nothing can \
                  take out what is there until the loop is over"]
        );
        assert_eq!(
            inside("    let s = Slot(Task(1), 1)\n    print(look(s.task, take s.task))"),
            vec!["this line has already looked at `s`, and the look might still be in use. Do the other thing on a \
                  line of its own first"]
        );
        // After it, on the same line, the field is `none`: nothing to guard.
        assert_eq!(inside("    let s = Slot(Task(1), 1)\n    print(look(take s.task, s.task))"), Vec::<String>::new());
        // A method may take out what its own object holds, and whoever
        // calls it is held to the rule in its place.
        let method = "class Holder {\n    task: Task or none\n    function hand_on(self) -> Task or none {\n\
                      return take self.task\n    }\n}\n";
        one_owner_ok(&with(&format!("{method}let h = Holder(Task(1))\nprint(h.hand_on(), h.task)\n")));
        assert_eq!(
            one_owner(&with(&format!("{method}function f(h: Holder) -> Task or none {{\n    return h.hand_on()\n}}\n"))),
            vec!["only the owner can call `hand_on`, which takes out or replaces something the object owns: somebody \
                  else may have been lent it. Here `h` only looks at it"]
        );
        // Reading an owned field that may be `none` says to take it.
        assert_eq!(
            one_owner(&with("function f() -> Task or none {\n    let s = Slot(Task(1), 1)\n    let t = s.task\n    return t\n}\n")),
            vec!["`task` holds an owned value, which cannot be taken out of the object it is in. Use it where it is, \
                  or pass it to a parameter that looks. To move it out and leave `none` in the field, write `take s.task`"]
        );
        // With a collector the value is shared, and the place still holds
        // `none` afterwards, as in every build.
        strict_ok(&with("function f(s: Slot) -> Task or none {\n    return take s.task\n}\nprint(f(Slot(Task(1), 1)))\n"));
    }

    #[test]
    fn a_second_kind_inside_brackets_is_named_as_one() {
        // Without the brackets the parser refuses it (its tests have the
        // words), since it could read as "raises ValueError, or nothing".
        ok("let f: (function() raises ValueError) or none = none\nprint(f is none)\n");
        // A second kind inside `[ ]` is named as one (decision 56.3).
        assert_eq!(error("let fs: list[function() raises ValueError, FileError] = []\n"), crate::parser::ONE_KIND);
        assert_eq!(error("let fs: dict[str, function() raises ValueError, FileError] = []\n"), crate::parser::ONE_KIND);
    }

    #[test]
    fn a_fault_after_raises_in_a_type_is_one_raised_by_hand() {
        // A function that says `raises ZeroDivisionError` must raise it
        // itself in a firmware build (5.19m), so only such a function, or
        // one that raises nothing, can be put in the type, and the type
        // needs no rule of its own (decision 56.1).
        let apply = "function apply(f: function(int, int) -> int raises ZeroDivisionError) -> int {\n\
                     try {\n        return f(1, 0)\n    } except ZeroDivisionError {\n        return -1\n    }\n}\n";
        let by_hand = "function half(a: int, b: int) -> int raises ZeroDivisionError {\n    if b == 0 {\n\
                       raise ZeroDivisionError(\"nothing to halve\")\n    }\n    return a // b\n}\n";
        assert_eq!(firmware_errors(&format!("{by_hand}{apply}print(apply(half))")), Vec::<String>::new());
        let divides = "function half(a: int, b: int) -> int raises ZeroDivisionError {\n    return a // b\n}\n";
        let refused = firmware_errors(&format!("{divides}{apply}print(apply(half))"));
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].starts_with("`half` says `raises ZeroDivisionError`, but nothing in it raises one itself"), "{refused:?}");
    }

    #[test]
    fn a_function_of_an_interface_says_what_its_function_types_raise() {
        // `raises` is part of a function type, so a class that leaves it
        // off a parameter has written another first line (decision 56.1).
        let port = "interface Port {\n    function each(self, convert: function(str) -> int raises ValueError) -> int raises ValueError\n}\n";
        let class = |ty: &str| {
            format!("{port}class Uart is Port {{\n    function each(self, convert: {ty}) -> int raises ValueError {{\n\
                     return convert(\"1\")\n    }}\n}}\n")
        };
        strict_ok(&class("function(str) -> int raises ValueError"));
        assert_eq!(
            strict_error(&class("function(str) -> int")),
            "`each` of `Uart` is `function each(self, convert: function(str) -> int) -> int`, and `Port` asks for \
             `function each(self, convert: function(str) -> int raises ValueError) -> int`. Write the two the same"
        );
    }

    #[test]
    fn a_firmware_build_cannot_raise_a_fault_to_a_caller() {
        let firmware = |source: &str| -> Vec<String> {
            let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
            let rules = super::Rules { strict: true, one_owner: false, firmware: true };
            super::check_by(&program, rules).0.into_iter().map(|error| error.message).collect()
        };
        // `zeno run` and an ordinary build catch this division by zero in
        // the caller. A firmware build stops at the division. So there
        // the `raises` is refused, and the two never disagree in silence.
        let half = "function half(a: int, b: int) -> int raises ZeroDivisionError { return a // b }\n\
                    function safe() -> int {\n try { return half(1, 0) } except ZeroDivisionError { return -1 }\n}";
        assert_eq!(
            firmware(half),
            vec![
                "`half` says `raises ZeroDivisionError`, but nothing in it raises one itself. In a firmware build \
                 a division by zero stops the program where it happens, and cannot be caught by a caller. Test for \
                 it before it happens and take `raises ZeroDivisionError` off, or build without `--firmware`"
            ]
        );
        strict_ok(half);
        assert_eq!(
            firmware("function narrow(n: int) -> u8 raises OverflowError { return u8(n) }").len(),
            1
        );
        // Raised by the program itself, it travels as a value, and so
        // does one passed on from a function that raises it.
        let raised = "function half(a: int, b: int) -> int raises ZeroDivisionError {\n \
                      if b == 0 { raise ZeroDivisionError(\"nothing to halve\") }\n return a // b\n}\n\
                      function twice(a: int, b: int) -> int raises ZeroDivisionError { return half(a, b) * 2 }";
        assert_eq!(firmware(raised), Vec::<String>::new());
        // The same for a built-in that is handed a value it has no
        // answer for. `int` of text is not one of those: it travels.
        assert_eq!(firmware("function letter(n: int) -> str raises ValueError { return char(n) }").len(), 1);
        assert_eq!(
            firmware("function number(text: str) -> int raises ValueError { return int(text) }"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn who_owns_a_value_is_never_guessed() {
        assert_eq!(
            owned_error("function f(q) { print(q) }\nlet p = make()\nf(p)"),
            "strict mode: the parameter `q` of `f` needs a type. It could not be worked out from how it is used"
        );
    }

    // ----- A function is not used before what it needs exists -----

    #[test]
    fn a_function_is_not_called_above_a_variable_it_uses() {
        // `x` in `inner` means the `x` of `outer`, which has no value yet
        // at the call. It must not be taken for the `x` further out.
        assert_eq!(
            error(
                "let x = 1\nfunction outer() {\n    print(inner())\n    let x = 5\n    function inner() { return x }\n}\nouter()"
            ),
            "`x` is declared further down than the first use of `inner`, which needs it. \
             Use `inner` below the line that declares `x`"
        );
        // The same with nothing of that name further out.
        assert_eq!(
            error("print(show())\nlet name = \"zeno\"\nfunction show() { return name }"),
            "`name` is declared further down than the first use of `show`, which needs it. \
             Use `show` below the line that declares `name`"
        );
        // Reached through another function, and by an assignment. Each
        // name is reported once.
        assert_eq!(
            error(
                "var n = 1\nfunction outer() {\n    step()\n    var n = 5\n    function add() { n = n + 1 }\n    function step() { add() }\n}\nouter()"
            ),
            "`n` is declared further down than the first use of `add`, which needs it. \
             Use `add` below the line that declares `n`"
        );
        // A method is a function of the block its class is in.
        assert_eq!(
            error(
                "let size = 1\nfunction make() {\n    let b = Box(2)\n    print(b.grow())\n    let size = 9\n    class Box {\n        side: int\n        function grow(self) { return self.side + size }\n    }\n}\nmake()"
            ),
            "`size` is declared further down than the first use of `grow`, which needs it. \
             Use `grow` below the line that declares `size`"
        );
    }

    #[test]
    fn a_class_is_not_built_above_a_variable_its_defaults_use() {
        assert_eq!(
            error(
                "let size = 1\nfunction make() {\n    let b = Box()\n    let size = 9\n    class Box {\n        side: int = size\n    }\n    return b.side\n}\nprint(make())"
            ),
            "`Box` cannot be built here yet. The default of its field `side` uses `size`, which is declared \
             further down. Build it below the line that declares `size`"
        );
        ok("let size = 9\nclass Box {\n    side: int = size\n}\nprint(Box().side)");
    }

    #[test]
    fn a_function_may_be_used_once_what_it_needs_exists() {
        // Called below the variable, though written above the call.
        ok("let x = 1\nfunction outer() {\n    let x = 5\n    print(inner())\n    function inner() { return x }\n}\nouter()");
        // Named above the variable but called below it.
        ok("let x = 1\nfunction outer() {\n    let f = inner\n    let x = 5\n    function inner() { return x }\n    print(f())\n}\nouter()");
        // A variable of the function's own, and one further out.
        ok("let x = 1\nfunction outer() {\n    print(inner())\n    function inner() {\n        let y = x\n        return y\n    }\n}\nouter()");
        // Two functions that call each other.
        ok("print(even(4))\nfunction even(n) {\n    if n == 0 { return true }\n    return odd(n - 1)\n}\nfunction odd(n) {\n    if n == 0 { return false }\n    return even(n - 1)\n}");
    }

    // ----- Reporting -----

    #[test]
    fn every_error_is_reported_in_order() {
        assert_eq!(
            errors("let a: int = \"x\"\nprint(missing)\nlet b = 1\nb = 2"),
            vec![
                "`a` should be int, but this is str",
                "`missing` is not defined",
                "`b` cannot be changed. Declare it with `var` to allow that",
            ]
        );
    }

    #[test]
    fn the_page_is_checked_before_the_program_runs() {
        let page = "import web\nlet count = web.element(\"#count\")\n";
        let with = |line: &str| format!("{page}{line}");
        ok(&with(
            "function mark(item: Element) { item.add_class(\"done\") }\nmark(count)\n\
             let all: list[Element] = web.elements(\"li\")\nfor item in all { mark(item) }\n\
             let lang: str or none = count.get(\"lang\")\ncount.text += \"!\"\ncount.value = count.text.upper()\n\
             var other = count\nother = web.create(\"p\")\nprint(count == other, count.has_class(\"a\") and true)",
        ));
        ok("from web import element, after\nlet e = element(\"p\")\nafter(1 s, function() { e.hide() })");
        check(&with("print(count.text)"), true).iter().for_each(|error| panic!("{error}"));

        assert_eq!(error(&with("count.text = 3")), "the new value should be str, but this is int");
        assert_eq!(error(&with("let n: int = count.get(\"href\")")), "`n` should be int, but this is str or none. Check it with `is not none` first");
        assert_eq!(error(&with("count.append(\"text\")")), "what is appended to an element should be Element, but this is str");
        assert_eq!(error(&with("count.set(\"href\")")), "`set` takes 2 arguments, but 1 was given");
        assert_eq!(error(&with("web.element(5)")), "the selector given to `web.element` should be str, but this is int");
        assert_eq!(error(&with("web.shout()")), "module `web` has no `shout`");
        assert_eq!(
            error(&with("count.shout()")),
            "an Element has no field or method `shout`. It has `text` and `value`, and the methods `on`, `fire`, `append`, \
             `remove`, `clear`, `get`, `set`, `add_class`, `remove_class`, `has_class`, `show`, `hide`, `get_style`, \
             `set_style`, `draw_rect`, `draw_circle`, `draw_line`, `draw_text`, `erase`"
        );
        assert_eq!(error(&with("count.html = \"x\"")), "an Element has no `html` to give a new value. It has `text` and `value`");
        assert_eq!(
            error(&with("web.after(500, function() { print(1) })")),
            "`after` needs a time with a unit, such as `500 ms`, but this is int"
        );
    }

    #[test]
    fn json_gives_a_value_whose_type_is_known_when_the_program_runs() {
        ok("import json\nlet data = json.parse(\"{}\")\nlet name: str = data[\"name\"]\nlet text: str = json.write(data)\nprint(name, text)");
        ok("from json import parse, write\nprint(write(parse(\"1\")))");
        assert_eq!(error("import json\njson.parse(5)"), "the text given to `json.parse` should be str, but this is int");
        assert_eq!(error("import json\nlet n: int = json.write(5)"), "`n` should be int, but this is str");
        assert_eq!(error("import json\njson.write()"), "`write` takes 1 argument, but 0 were given");
        assert_eq!(error("import json\njson.read(\"1\")"), "module `json` has no `read`");
        // Strict mode wants every type before the program runs, and
        // what JSON holds is not known then.
        assert_eq!(
            check("import json\nlet data = json.parse(\"1\")", true)[0],
            "strict mode: the type of this cannot be worked out before the program runs"
        );
    }

    #[test]
    fn a_function_the_page_will_call_takes_what_the_page_gives() {
        let page = "import web\nlet count = web.element(\"#count\")\nfunction key(name: str, twice: bool = false) { print(name) }\n";
        let with = |line: &str| format!("{page}{line}");
        ok(&with("web.on_key(key)\nweb.on_key(print)\ncount.on(\"click\", function() { count.hide() })"));
        let nothing = "will call this function with nothing, so it takes no parameters: `function() { ... }`";
        let or_event = "`on` will call this function with nothing, or with what happened if it asks: \
                        `function() { ... }` or `function(event: Event) { ... }`";
        assert_eq!(error(&with("count.on(\"click\", function(x: int) { print(x) })")), or_event);
        assert_eq!(error(&with("count.on(\"click\", function(a: Event, b: Event) { print(a) })")), or_event);
        ok(&with(
            "count.on(\"click\", function(event: Event) {\n let where: float = event.x + event.y\n \
             let what: str = event.kind + event.key\n let which: Element = event.target\n print(where, what, which)\n})\n\
             function seen(event: Event) { print(event) }\ncount.on(\"input\", seen)\ncount.fire(\"input\")\n\
             let ticking: Timer = web.every(1 s, function() { count.show() })\nticking.stop()\n\
             web.after(1 s, function() { count.show() }).stop()\n\
             web.fetch(\"/a\", function(status: int, body: str) { print(status, body) })\n\
             web.post(\"/a\", \"b\", function(status: int, body: str) { print(status, body) })\n\
             web.save(\"a\", \"b\")\nlet kept: str or none = web.load(\"a\")\nweb.forget(\"a\")\n\
             count.set_style(\"color\", \"red\")\nlet colour: str or none = count.get_style(\"color\")\n\
             count.draw_rect(0, 0, 10.5, 2, \"red\")\ncount.draw_circle(5, 5, 2, \"red\")\ncount.draw_line(0, 0, 1, 1, \"red\")\n\
             count.draw_text(0, 0, \"hi\", \"red\")\ncount.erase()",
        ));
        assert_eq!(
            error(&with("count.on(\"click\", function(event: Event) { event.x = 1.0 })")),
            "an Event says what happened, so `x` cannot be given a new value"
        );
        assert_eq!(
            error(&with("count.on(\"click\", function(event: Event) { print(event.button) })")),
            "an Event has no field `button`. It has `kind`, `target`, `x`, `y`, `key`"
        );
        assert_eq!(
            error(&with("web.fetch(\"/a\", function(body: str) { print(body) })")),
            "`fetch` will call this function with the status and the text of the answer, so it takes an int and a str: \
             `function(status: int, body: str) { ... }`"
        );
        assert_eq!(error(&with("web.every(1, function() { count.hide() })")), "`every` needs a time with a unit, such as `500 ms`, but this is int");
        assert_eq!(
            error(&with("web.every(1 s, function() { count.hide() }).pause()")),
            "a Timer has no `pause`. It has `stop()`, which calls off what is waiting"
        );
        assert_eq!(error(&with("web.every(1 s, function() { count.hide() }).stop(1)")), "`stop` takes 0 arguments, but 1 was given");
        assert_eq!(error(&with("let kept: str = web.load(\"a\")")), "`kept` should be str, but this is str or none. Check it with `is not none` first");
        assert_eq!(
            error(&with("count.draw_rect(0, 0, \"wide\", 2, \"red\")")),
            "a place or a size given to `draw_rect` is a number, but this is str"
        );
        assert_eq!(error(&with("count.draw_text(0, 0, \"hi\", 5)")), "the colour given to `draw_text` should be str, but this is int");
        assert_eq!(error(&with("count.draw_text(0, 0, 5, \"red\")")), "the text given to `draw_text` should be str, but this is int");
        assert_eq!(error(&with("count.erase(1)")), "`erase` takes 0 arguments, but 1 was given");
        assert_eq!(error(&with("web.after(1 s, key)")), format!("`after` {nothing}"));
        let a_key = "`on_key` will call this function with the key that was pressed, so it takes one str: `function(key: str) { ... }`";
        assert_eq!(error(&with("web.on_key(function() { print(1) })")), a_key);
        assert_eq!(error(&with("web.on_key(function(code: int) { print(code) })")), a_key);
        assert_eq!(error(&with("count.on(\"click\", 5)")), "`on` needs a function to call, but this is int");
    }

    #[test]
    fn the_examples_pass() {
        ok(include_str!("../examples/hello.zeno"));
        ok(include_str!("../examples/tour.zeno"));
        ok(include_str!("../examples/styles.zeno"));
        // The tour is fully typed, so it passes strict mode as well.
        strict_ok(include_str!("../examples/tour.zeno"));
        strict_ok(include_str!("../examples/buffer.zeno"));
        strict_ok(include_str!("../examples/labels.zeno"));
        strict_ok(include_str!("../examples/words.zeno"));
    }

    // ----- Memory by hand (docs/unsafe.md) -----

    const POINTER: &str = "let p = allocate(u8, 4)\n";
    const REGISTER: &str = "from board import register\nlet r = register(u8, 0x4002_0000)\n";

    #[test]
    fn what_zeno_cannot_check_goes_inside_unsafe() {
        // Five things can damage memory, and those five need the block.
        for (source, what) in [
            (format!("{POINTER}print(p[0])"), "reading through a pointer"),
            (format!("{POINTER}p[0] = 1"), "writing through a pointer"),
            (format!("{POINTER}free(p)"), "`free`"),
            (format!("{REGISTER}print(r.read())"), "reading a register"),
            (format!("{REGISTER}r.write(1)"), "writing a register"),
            // A parameter with no type written is covered too: its type
            // is worked out from the call.
            ("function peek(p) { return p[0] }\nprint(peek(allocate(u8, 2)))".to_string(), "reading through a pointer"),
            // A function written inside the block is a new function. It
            // may be called from anywhere, so it starts safe again.
            (format!("{POINTER}unsafe {{\n    function first() -> int {{ return p[0] }}\n}}"), "reading through a pointer"),
            (
                format!("{POINTER}unsafe {{\n    let f = function() -> int {{ return p[0] }}\n    print(f())\n}}"),
                "reading through a pointer",
            ),
        ] {
            assert_eq!(
                error(&source),
                format!(
                    "{what} goes inside `unsafe {{ }}`. Zeno cannot check that it is right, \
                     so the block marks the lines where a mistake can damage memory"
                ),
                "\n{source}"
            );
        }

        // Everything else is safe: making a pointer or a register,
        // passing it, keeping it, working out an address, comparing.
        strict_ok(
            "from board import register\n\
             class Buffer {\n    bytes: pointer[u8]\n    size: int\n}\n\
             function second(p: pointer[u8]) -> pointer[u8] { return p + 1 }\n\
             let p = allocate(u8, 4)\n\
             let buffer = Buffer(p, 4)\n\
             let q: pointer[u8] or none = second(p) - 1\n\
             let all: list[pointer[u8]] = [p, buffer.bytes]\n\
             let r: Register[u32] = register(u32, 0x4002_0014)\n\
             print(p == all[1], q == p, p, r, r.address)\n\
             unsafe {\n\
             \x20   p[0] = 255\n\
             \x20   p[1] = u8(p[0] - 1)\n\
             \x20   if p[1] > 0 {\n\
             \x20       r.write(u32(r.read() | 1 << 5))\n\
             \x20   }\n\
             \x20   free(p)\n\
             }",
        );
        // The block is an ordinary block: what is declared inside ends
        // with it, and a `return` inside it counts.
        assert_eq!(error("unsafe {\n    let inside = 1\n}\nprint(inside)"), "`inside` is not defined");
        ok("function first(p: pointer[int]) -> int {\n    unsafe { return p[0] }\n}");
        strict_ok(include_str!("../examples/driver.zeno"));
        one_owner_ok(include_str!("../examples/driver.zeno"));
    }

    #[test]
    fn pointers_and_registers_have_types() {
        let p = POINTER;
        let r = REGISTER;
        for (source, message) in [
            // What a pointer may point at, and what `allocate` is given.
            (
                "let p = allocate(str, 4)".to_string(),
                "`allocate` needs the type of one item first, and then how many: `allocate(u8, 16)`. \
                 The type is int, float, or one of u8, u16, u32, i8, i16 and i32",
            ),
            ("let p = allocate(bool, 4)".to_string(), "`bool` is a type and not a value, so it cannot be used here"),
            ("let p = allocate(u8)".to_string(), "`allocate` takes 2 arguments, but 1 was given"),
            ("let p = allocate(u8, 2.5)".to_string(), "the number of items should be int, but this is float"),
            (
                "let p: pointer[str] = allocate(u8, 4)".to_string(),
                "a pointer points at numbers of one size: `pointer[u8]`, `pointer[int]`, `pointer[float]`, \
                 or any of u8, u16, u32, i8, i16 and i32",
            ),
            (
                "let p: pointer = allocate(u8, 4)".to_string(),
                "a pointer points at numbers of one size: `pointer[u8]`, `pointer[int]`, `pointer[float]`, \
                 or any of u8, u16, u32, i8, i16 and i32",
            ),
            // A pointer to bytes is not a pointer to anything else.
            ("let p: pointer[u16] = allocate(u8, 4)".to_string(), "`p` should be pointer[u16], but this is pointer[u8]"),
            // What is written through a pointer follows the rules of the
            // sized numbers (docs/sized-ints.md).
            (format!("{p}unsafe {{ p[0] = 300 }}"), "300 does not fit in a u8 (0 to 255)"),
            (
                format!("{p}unsafe {{ p[0] = p[1] + 1 }}"),
                "the new value should be u8, but this is int. Convert it with `u8(...)`, which checks that it fits",
            ),
            (
                "let p = allocate(float, 4)\nunsafe { p[0] = 2 }".to_string(),
                "the new value should be float, but this is int. Write a number with a decimal point, as in `1.0`, and convert a value with `float( )`",
            ),
            (format!("{p}unsafe {{ print(p[1.0]) }}"), "an index should be int, but this is float"),
            // `p + n` and `p - n`, and nothing else.
            (format!("{p}print(p + 1.5)"), "`+` does not work between pointer[u8] and float"),
            (format!("{p}print(p + p)"), "`+` does not work between pointer[u8] and pointer[u8]"),
            (
                format!("{p}print(1 + p)"),
                "`+` does not work between int and pointer[u8]. Write the pointer first, as in `p + 1`",
            ),
            (format!("{p}print(p * 2)"), "`*` does not work between pointer[u8] and int"),
            (format!("{p}print(p < p + 1)"), "`<` does not work between pointer[u8] and pointer[u8]"),
            // A pointer is an address and nothing more.
            (
                format!("{p}for byte in p {{ print(byte) }}"),
                "a pointer does not know how many items it points at, so it cannot be looped over. \
                 Loop over `range(count)` and read item `i` inside `unsafe { }`",
            ),
            (
                format!("{p}print(len(p))"),
                "a pointer does not know how many items it points at, so `len` cannot tell. \
                 Keep the number you gave `allocate` next to the pointer",
            ),
            ("unsafe { free(3) }".to_string(), "`free` needs a pointer that `allocate` gave, but this is int"),
            // Registers.
            (
                "from board import register\nlet r = register(i16, 0x10)".to_string(),
                "`register` needs the size of the register first, and then its address: `register(u32, 0x4002_0014)`. \
                 The size is u8, u16 or u32",
            ),
            (
                "from board import register\nlet r = register(u8, \"a\")".to_string(),
                "the address of a register should be int, but this is str",
            ),
            (
                "from board import register\nlet r: Register[int] = register(u8, 1)".to_string(),
                "a register holds a whole number of one size: `Register[u8]`, `Register[u16]` or `Register[u32]`",
            ),
            (
                "from board import register\nlet r: Register = register(u8, 1)".to_string(),
                "`Register` needs the size of the register, as in `Register[u32]`",
            ),
            (format!("{r}unsafe {{ r.write(256) }}"), "256 does not fit in a u8 (0 to 255)"),
            (format!("{r}unsafe {{ r.toggle() }}"), "Register[u8] has no field or method `toggle`"),
            (format!("{r}r.address = 5"), "a field cannot be set on Register[u8]"),
        ] {
            assert_eq!(error(&source), message, "\n{source}");
        }
    }

    #[test]
    fn an_f32_is_never_mixed_with_a_float_by_guess() {
        // Decision 41: sums with an int stay f32, a float literal fits
        // where an f32 is wanted, and an f32 fits where a float is.
        ok("let a: f32 = 0.5\nlet b = a + 1\nlet c: f32 = b * 2.0\nlet d: float = c\nprint(d, a < 1, abs(a), min(a, 2), sum([a, a]))");
        ok("let a: f32 = f32(3) / 2\nimport math\nprint(math.sqrt(a), float(a), int(a))");
        for (source, message) in [
            (
                "let a: f32 = 0.5\nlet b = 1.5\nprint(a + b)",
                "`+` between f32 and float would have to choose between 32 bits and 64. Write `f32( )` around the float to work in 32, or `float( )` around the f32 to work in 64",
            ),
            (
                "let a: f32 = 0.5\nlet b = 2.0\nprint(a < b)",
                "`<` between f32 and float would have to choose between 32 bits and 64. Write `f32( )` around the float to work in 32, or `float( )` around the f32 to work in 64",
            ),
            ("let a: f32 = 1e39", "1e39 does not fit in an f32, whose largest is about 3.4e38"),
            (
                "let n = 3\nlet a: f32 = n",
                "`a` should be f32, but this is int. Write a number with a decimal point, as in `1.0`, and convert a value with `f32( )`",
            ),
            (
                "let x = 1.5\nlet a: f32 = x",
                "`a` should be f32, but this is float. An f32 holds about 7 digits and a float about 16, so most floats are not one. Convert it with `f32( )`, which rounds it to the nearest f32",
            ),
            (
                "let a: list[f32] = [0.5]\nlet b: list[float] = a",
                "`b` should be list[float], but this is list[f32]. The list could then be given a float that is not an f32. Make a list of floats from it with `[float(x) for x in ...]`",
            ),
        ] {
            assert_eq!(error(source), message, "\n{source}");
        }
    }

    #[test]
    fn a_u64_meets_only_a_u64_or_a_written_number() {
        // Decision 42: a written number fits a u64 when it is 0 or more,
        // a list of them beside a u64 is a list[u64], and the limits of
        // every whole type are names.
        ok("let a: u64 = 18446744073709551615\nlet b = a - 5\nlet c = a > 3 and b == 7\nlet xs = [a, 7, 8]\nlet ys: list[u64] = xs\nprint(c, ys)");
        ok("let a = u8.max + i8.min + u16.max + i32.min + u32.max\nlet b: u64 = u64.max\nlet c: int = int.min\nprint(a, b, c, u64.wrap(-1), u64.wrap(b * 3))");
        for (source, message) in [
            (
                "let a: u64 = 5\nlet b = -3\nprint(a + b)",
                "`+` between u64 and int would have to choose between a u64 and an int. Write `u64( )` around the int to work in u64s, or `int( )` around the u64",
            ),
            (
                "let a: u64 = 5\nprint(a + 1.5)",
                "`+` between u64 and float would have to choose between a u64 and a float. Write `float( )` around the u64 to work in floats",
            ),
            ("let a: u64 = -1", "-1 does not fit in a u64 (0 to 18446744073709551615)"),
            ("let a: u64 = 5\nprint(-a)", "`-` does not work on a u64, which is never below 0. Make it an int first with `int( )`"),
            (
                "let n = 3\nlet a: u64 = n",
                "`a` should be u64, but this is int. An int can be below 0, and a u64 never is. Convert it with `u64( )`, which checks that it fits",
            ),
            (
                "let a: u64 = 5\nlet b: int = a",
                "`b` should be int, but this is u64. Half the numbers a u64 holds are past the largest int. Convert it with `int( )`, which checks that it fits",
            ),
            ("print(u64.wrap(1.5))", "`u64.wrap` needs a whole number, but this is float. Make it one with `int( )` first"),
            ("let a: u8 = 5\nlet b: u64 = a", "`b` should be u64, but this is u8. Convert it with `u64( )`, which never fails for a u8"),
            (
                "let a: i16 = 5\nlet b: u64 = a",
                "`b` should be u64, but this is i16. An i16 can be below 0, and a u64 never is. Convert it with `u64( )`, which checks that it fits",
            ),
            ("let a: u64 = 5\nlet b: u8 = a", "`b` should be u8, but this is u64. Convert it with `u8(int( ))`, which checks that it fits"),
            ("let a: u64 = 5\nlet b = u8(a)", "`u8` takes an int, and this is a u64. Make it an int first, as in `u8(int(x))`"),
        ] {
            assert_eq!(error(source), message, "\n{source}");
        }
    }

    #[test]
    fn several_values_are_received_one_to_each_place() {
        // Decision 57: the count and the type on both sides, `_`, a field
        // and an item as places, a generic, an interface and a function
        // type that give two.
        let divide = "function divide(a: int, b: int) -> (int, int) {\n    return a // b, a % b\n}\n";
        let with = |rest: &str| format!("{divide}{rest}");
        ok(&with("let q, r = divide(7, 2)\nvar low, high = 1, 2\nlow, high = high, low\nlet _, _ = divide(1, 2)\nprint(q + r, low, high)"));
        ok(&with("class B {\n    a: int = 0\n}\nvar xs = [0]\nlet bx = B()\nbx.a, xs[0] = divide(5, 2)\nprint(bx.a, xs)"));
        ok(&with("function passes() -> (int, int) {\n    return divide(1, 2)\n}\nlet a, b = passes()\nprint(a + b)"));
        ok("function swap[A, B](a: A, b: B) -> (B, A) {\n    return b, a\n}\nlet s, n = swap(1, \"x\")\nlet k: int = n\nprint(s + \"\", k)");
        ok("interface Reader {\n    function read(self) -> (int, bool)\n}\nclass Port is Reader {\n    function read(self) -> (int, bool) { return 1, true }\n}\n\
            function use(r: Reader) -> int {\n    let v, ok = r.read()\n    return v\n}\nprint(use(Port()))");
        ok(&with("let f: function(int, int) -> (int, int) = divide\nlet a, b = f(1, 2)\nprint(a, b)"));

        assert_eq!(
            error(&with("let a, b, c = divide(1, 2)")),
            "`divide` gives 2 values, and there are 3 places on the left of `=`. Receive each, and write `_` for one that is not wanted"
        );
        assert_eq!(error(&with("let a: str, b = divide(1, 2)")), "`a` should be str, but this is int");
        assert_eq!(
            error("let a, b = 1"),
            "there are 2 places on the left of `=`, and the right gives one value, int. Give 2, as in `a, b = 1, 2`, or call a function that gives 2"
        );
        assert_eq!(error("let a, b = 1, 2, 3"), "there are 2 places on the left of `=` and 3 values on the right. Each place receives one");
        assert_eq!(
            error("function h() -> (int, str) {\n    return 1\n}"),
            "this function gives 2 values, and this is one. Give each, as in `return a, b`"
        );
        assert_eq!(error("function h() -> (int, str) {\n    return 1, 2\n}"), "the second value returned should be str, but this is int");
        assert_eq!(
            error("function h() -> int {\n    return 1, 2\n}"),
            "this function gives one value, int, and this `return` gives 2. A function that gives 2 says so in its first line, as in `-> (int, int)`"
        );
        assert_eq!(
            error(&with("let f: function(int, int) -> int = divide")),
            "`f` should be function(int, int) -> int, but this function returns (int, int)"
        );
        assert_eq!(
            error("interface Reader {\n    function read(self) -> (int, bool)\n}\nclass Bad is Reader {\n    function read(self) -> int { return 1 }\n}"),
            "`read` of `Bad` is `function read(self) -> int`, and `Reader` asks for `function read(self) -> (int, bool)`. Write the two the same"
        );
        assert_eq!(error("var a = 1\nlet b = 2\na, b = 3, 4"), "`b` cannot be changed. Declare it with `var` to allow that");

        // Rule 3: several values are not a value, wherever one is wanted.
        let not_one = "`divide` gives 2 values. Receive each: `let a, b = divide(...)`. To keep them together, declare a class";
        for place in [
            "let a = divide(1, 2)",
            "let xs = [divide(1, 2)]",
            "print(divide(1, 2))",
            "divide(1, 2)",
            "let a = divide(1, 2) + 1",
            "class B {\n    a: int = 0\n}\nlet b = B()\nb.a = divide(1, 2)",
            "function g(n: int) -> int { return n }\nprint(g(divide(1, 2)))",
        ] {
            assert_eq!(error(&with(place)), not_one, "{place}");
        }
        // The same name twice on the left, and `_` where it is a variable.
        assert_eq!(error(&with("var a = 0\na, a = divide(7, 2)")), "`a` is on the left of `=` twice. Each place receives one value");
        ok(&with("var xs = [1, 2]\nxs[0], xs[1] = divide(7, 2)\nprint(xs)"));
        let shadowed = "`_` on the left of `=` drops a value, but here `_` is a variable, which would be given it instead. \
                        Give the variable another name";
        assert_eq!(error(&with("var _ = 9\nvar b = 0\n_, b = divide(7, 2)")), shadowed);
        assert_eq!(error(&with("var _ = 9\nlet _, c = divide(7, 2)")), shadowed);
        // A call of several given as the one argument of another says rule
        // 3, and not that an argument is missing.
        assert_eq!(error(&with("let x, y = divide(divide(9, 2))")), not_one);
        // A function given to `map` and the others gives one value a call.
        assert_eq!(
            error("function two(n: int) -> (int, int) {\n    return n, n\n}\nprint([1].map(two))"),
            "the function given to `map` gives 2 values, and `map` needs one value from each call. To keep them together, declare a class and return one object"
        );
    }

    /// The messages for a program that uses the `Resource` that comes
    /// with Zeno: joined in as `modules.rs` joins it, and checked as `zeno
    /// run` checks it. A build with no collector must say the same, but
    /// for the strict mode of a build, which asks first for a type that
    /// is not written.
    fn resource_errors(source: &str) -> Vec<String> {
        let source = format!("{HELD}{source}");
        let mut found = Vec::new();
        for one_owner in [false, true] {
            let program = parser::parse(lexer::lex(&source).expect("lexing failed")).expect("parsing failed").with_resource();
            // As `zeno run` checks it, and as a build with no collector.
            let rules = super::Rules { strict: one_owner, one_owner, firmware: false };
            let (_, errors, _) = super::checked(program, rules);
            let messages: Vec<String> = errors.into_iter().map(|error| error.message).collect();
            match one_owner {
                false => found = messages,
                true if found.iter().any(|message| message.contains("is not written")) => {}
                true => assert_eq!(messages, found, "with no collector\n{source}"),
            }
        }
        found
    }

    const HELD: &str = "owned class Held is Resource {\n    name: str\n\n    function release(self) {\n        print(\"released\", self.name)\n    }\n}\n";

    #[test]
    fn a_resource_goes_only_where_it_is_released() {
        // Rule 7 of 59.1: a `let` or a parameter, and nothing else, each
        // refused in words that say what to write.
        let inside = |what: &str| {
            format!(
                "{what} would hold a resource. A resource is released when its owner ends, and its owner is a `let` or \
                 a parameter, never a list, a dict, a field, a case of an enum, `or none` or a type parameter. Keep it \
                 in a `let` of its own"
            )
        };
        let kept = "this call gives a resource, and nothing keeps it, so it would be released at once. Keep it in a \
                    `let`, which releases it when its block ends: `let held = ...`";
        let refused: Vec<(&str, String)> = vec![
            ("var h = Held(\"a\")", "`h` holds a resource, which is released when its owner ends, so the variable cannot be given another value. Declare it with `let`".to_string()),
            ("let hs = [Held(\"a\")]", inside("this")),
            ("let h: Held or none = none", inside("`h`")),
            ("class Box {\n    h: Held\n}", inside("the field `h` of `Box`")),
            ("owned class Box {\n    h: Held\n}", inside("the field `h` of `Box`")),
            ("enum Some {\n    One(h: Held)\n}", inside("the case `One` of the enum `Some`")),
            ("function make() -> list[Held] {\n    return []\n}", inside("what `make` gives")),
            ("function holder[T](x: take T) -> int {\n    return 1\n}\nlet h = Held(\"a\")\nprint(holder(h))", inside("`T` of `holder`")),
            // Where items 11 and 12 meet: a bound, a generic class and a
            // generic enum are each a type parameter.
            ("function named[T is Resource](x: T) -> int {\n    return 1\n}\nlet h = Held(\"a\")\nprint(named(h))", inside("`T` of `named`")),
            ("class Box[T is Resource] {\n    count: int\n}\nlet b: Box[Held] = Box(1)", inside("`T` of `Box`")),
            ("enum Maybe[T] {\n    Just(value: T)\n    Nothing\n}\nlet m = Just(Held(\"a\"))", inside("`T` of `Maybe`")),
            ("interface Source[T] {\n    function next(self) -> T\n}\nclass Maker is Source[Held] {\n    function next(self) -> Held {\n        return Held(\"made\")\n    }\n}", inside("`T` of `Source`")),
            ("print(Held(\"a\").name)", kept.to_string()),
            ("Held(\"a\")", kept.to_string()),
            ("let h = Held(\"a\")\nh.release()", "`release` runs by itself, once, when the owner of this `Held` ends, so it cannot be called. To end it sooner, give it to a function that takes it: `function done(held: take Held) { }`".to_string()),
            ("function outer() {\n    let h = Held(\"a\")\n    let f = function() -> str { return h.name }\n    print(f())\n}", "this function is kept, so it would take `h`, a resource, and nothing would release it when its block ends. Pass `h` to the function as a parameter instead".to_string()),
            ("function two() -> (Held, int) {\n    return Held(\"a\"), 1\n}", super::RESOURCE_SEVERAL.to_string()),
            ("let a, b = Held(\"received\"), 1", super::RESOURCE_SEVERAL.to_string()),
            ("let n, h = 1, Held(\"second\")", super::RESOURCE_SEVERAL.to_string()),
            ("function f(h: Held = Held(\"d\")) {\n}", "a default cannot make a resource: `h` would be made at each call that leaves it out, where no `let` owns it. Pass one at the call instead".to_string()),
            ("function f(h: take Held = Held(\"d\")) {\n}", "a default cannot make a resource: `h` would be made at each call that leaves it out, where no `let` owns it. Pass one at the call instead".to_string()),
            ("function show(x) {\n    print(x)\n}\nlet h = Held(\"a\")\nshow(h)", "the type of `x` of `show` is not written, so a resource cannot go to it. Write its type: `x: Held`".to_string()),
            ("class Plain is Resource {\n    function release(self) {\n    }\n}", "`Plain` is a `Resource`, so each of its objects has one owner, whose end releases it. Declare it `owned class Plain is Resource`".to_string()),
            ("owned class Bad is Resource {\n    function release(self) raises ValueError {\n        raise ValueError(\"no\")\n    }\n}", "`release` runs when the owner of its object ends, where an error has nowhere to go, so it cannot raise. Catch the error inside `release`".to_string()),
            ("import web\nlet h = Held(\"a\")", "a page stays open after the program's last line, so the top of its file never ends, and `h` would never be released. Make it inside the function that uses it".to_string()),
        ];
        for (source, message) in refused {
            assert_eq!(resource_errors(source), vec![message], "\n{source}");
        }
        // The plain form has no place that can hold a resource, so it is
        // refused for that too; the resource on either side is refused.
        for source in ["var a: int = 0\nvar b: int = 0\na, b = Held(\"x\"), 1", "var a: int = 0\nvar b: int = 0\na, b = 1, Held(\"x\")"] {
            assert!(resource_errors(source).contains(&super::RESOURCE_SEVERAL.to_string()), "\n{source}");
        }
        // What is taken: a `let`, a parameter that takes or looks, given
        // back, given away, through the interface, and inside a function
        // of a page.
        let taken = [
            "let h = Held(\"a\")\nprint(h.name)",
            "function done(held: take Held) {\n    print(held.name)\n}\nlet h = Held(\"a\")\ndone(h)",
            "function look(held: Held) -> str {\n    return held.name\n}\nlet h = Held(\"a\")\nprint(look(h))",
            "function make() -> Held {\n    return Held(\"made\")\n}\nlet h = make()\nlet g = h",
            "function done(held: take Held) {\n}\ndone(Held(\"passed\"))",
            "let h = Held(\"a\")\nlet r: Resource = h",
            "import web\nfunction pressed() {\n    let h = Held(\"a\")\n}",
        ];
        for source in taken {
            assert_eq!(resource_errors(source), Vec::<String>::new(), "\n{source}");
        }
    }

    #[test]
    fn a_resource_of_the_program_s_own_is_the_program_s() {
        // With a `Resource` of its own, the program means that, and
        // nothing is released: it is an interface like any other.
        let source = "interface Resource {\n    function release(self)\n}\nclass Lock is Resource {\n    function release(self) {\n    }\n}\nvar locks = [Lock(), Lock()]\nlocks[0].release()\n";
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed").with_resource();
        let (checked, errors, _) = super::checked(program, super::Rules { strict: true, one_owner: false, firmware: false });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(checked.statements.len(), 4, "a `Resource` was joined in, or something was rewritten");
        assert!(!format!("{:?}", checked.statements).contains("Scoped"));
    }

    #[test]
    fn a_program_with_no_resource_is_not_rewritten() {
        let source = "class Box {\n    n: int\n}\nlet b = Box(1)\nprint(b.n)\n";
        let program = parser::parse(lexer::lex(source).expect("lexing failed")).expect("parsing failed");
        let joined = program.clone().with_resource();
        assert_eq!(joined, program);
    }

    #[test]
    fn the_rest_of_a_block_after_a_resource_is_scoped() {
        // The statements after the `let`, to the end of the block, go in a
        // `Scoped`, the second inside the first; a taken parameter wraps
        // the body; a name given away is a `Give`.
        let source = format!("{HELD}function done(held: take Held) {{\n    print(held.name)\n}}\nlet a = Held(\"a\")\nlet b = Held(\"b\")\ndone(a)\n");
        let program = parser::parse(lexer::lex(&source).expect("lexing failed")).expect("parsing failed").with_resource();
        let (checked, errors, _) = super::checked(program, super::Rules { strict: true, one_owner: false, firmware: false });
        assert!(errors.is_empty(), "{errors:?}");
        let shown = crate::printer::print(&checked);
        assert!(shown.contains("let a = Held(\"a\")\nreleased at the end: a\n    let b = Held(\"b\")\n    released at the end: b\n        done(a)"), "{shown}");
        assert!(shown.contains("function done(held: take Held)\n    released at the end: held\n"), "{shown}");
        let crate::ast::StmtKind::Scoped { body, .. } = &checked.statements.last().expect("statements").kind else { panic!("{shown}") };
        let crate::ast::StmtKind::Scoped { body, .. } = &body.last().expect("statements").kind else { panic!("{shown}") };
        let crate::ast::StmtKind::Expr(crate::ast::Expr { kind: crate::ast::ExprKind::Call { args, .. }, .. }) = &body[0].kind else { panic!("{shown}") };
        assert_eq!(args[0].value.kind, crate::ast::ExprKind::Give("a".to_string()));
    }
}
