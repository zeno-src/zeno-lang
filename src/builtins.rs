//! Everything a Zeno program can use without defining it: functions like
//! `print` and `len`, methods on strings and lists, the built-in error
//! types, the modules (`math`, `time`, `board`, `files`, `program`), and
//! f-string formatting.
//!
//! These are written in Rust because they cannot be written in Zeno yet.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::BinaryOp;
use crate::board;
use crate::checker::Pointee;
use crate::error::{counted, were};
use crate::interpreter::{Env, Scope};
use crate::ints::{self, Width};
use crate::ops;
use crate::token::Span;
use crate::value::*;

type Result<T = Value> = std::result::Result<T, Control>;

pub const ERROR_TYPES: &[&str] =
    &["Error", "ValueError", "IndexError", "KeyError", "ZeroDivisionError", "OverflowError", "FileError"];

/// The end of the message for an `import` of a module there is not.
pub const MODULES: &str = "So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`";

/// True for the name of a module that comes with Zeno. Any other module
/// is a file of the program: see `modules.rs`.
pub fn comes_with_zeno(name: &str) -> bool {
    matches!(name, "math" | "time" | "board" | "files" | "program" | "json" | "web" | "python" | "node" | "java" | "random" | "ruby" | "php" | "lua" | "csharp" | "c")
}

/// Every built-in name and what it stands for.
pub fn all() -> Vec<(&'static str, Value)> {
    let functions: &[Builtin] = &[
        Builtin { name: "print", call: print },
        Builtin { name: "len", call: len },
        Builtin { name: "range", call: range },
        Builtin { name: "int", call: int },
        Builtin { name: "float", call: float },
        Builtin { name: "f32", call: f32_of },
        Builtin { name: "u64", call: u64_of },
        Builtin { name: "u64..of", call: u64_bits },
        Builtin { name: "u64..add", call: u64_wrapping::<0> },
        Builtin { name: "u64..sub", call: u64_wrapping::<1> },
        Builtin { name: "u64..mul", call: u64_wrapping::<2> },
        Builtin { name: "u64..shl", call: u64_wrapping::<3> },
        Builtin { name: "str", call: str },
        Builtin { name: "abs", call: abs },
        Builtin { name: "min", call: min },
        Builtin { name: "max", call: max },
        Builtin { name: "sum", call: sum },
        Builtin { name: "allocate", call: allocate },
        Builtin { name: "free", call: free },
        Builtin { name: "char", call: char_of },
        Builtin { name: "input", call: input },
    ];

    let mut all: Vec<(&'static str, Value)> =
        functions.iter().map(|builtin| (builtin.name, Value::Builtin(*builtin))).collect();
    // `u8(n)` and the other sized whole numbers.
    for (index, width) in ints::WIDTHS.iter().enumerate() {
        all.push((width.name, Value::Builtin(Builtin { name: width.name, call: FITS[index] })));
    }
    all.extend(ERROR_TYPES.iter().map(|kind| (*kind, Value::ErrorType(Rc::from(*kind)))));
    all
}

/// The outermost scope of the interpreter, holding every built-in name.
pub fn globals() -> Env {
    let env = Scope::root();
    for (name, value) in all() {
        env.declare(name, value, false);
    }
    env
}

/// Check how many arguments a built-in was given.
fn arity(name: &str, args: &[Value], min: usize, max: usize, span: Span) -> Result<()> {
    if (min..=max).contains(&args.len()) {
        return Ok(());
    }
    let wanted = if min == max { counted(min, "argument") } else { format!("{min} to {max} arguments") };
    fatal(format!("`{name}` takes {wanted}, but {} given", were(args.len())), span)
}

fn wrong_type<T>(name: &str, wanted: &str, got: &Value, span: Span) -> Result<T> {
    fatal(format!("`{name}` needs {wanted}, but this is {}", got.type_name()), span)
}

// ------------------------------------------------------------------ Functions

fn print(machine: &mut dyn Machine, args: Vec<Value>, _: Span) -> Result {
    let words: Vec<String> = args.iter().map(Value::to_string).collect();
    machine.write_line(&words.join(" "));
    Ok(Value::None)
}

/// `input()`, `input("Name: ")`: one line from the keyboard, without
/// its line end. `none` when there are no more lines, which is how a
/// program that reads to the end knows it is there.
fn input(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("input", &args, 0, 1, span)?;
    // A page has no keyboard to read a line from. The browser asks the
    // person in a small window of its own, with the question in it.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = machine;
        let question = match args.first() {
            Some(question) => text("input", question, span)?,
            None => "",
        };
        Ok(crate::web_host::input(question).map_or(Value::None, Value::str))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(question) = args.first() {
            machine.write(text("input", question, span)?);
        }
        Ok(match machine.outside().line() {
            Some(line) => Value::str(line),
            None => Value::None,
        })
    }
}

/// `program.arguments()`: the words the program was started with, after
/// its own name. A new list at each call.
fn program_arguments(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("arguments", &args, 0, 0, span)?;
    Ok(Value::list(machine.outside().arguments.iter().map(Value::str).collect()))
}

/// `program.exit(code)`: end the program here, and give `code` to what
/// started it: 0 says that all went well, and 1 to 255 that something
/// did not. It is not an error, so no `try` stops it (decision 34.1).
fn program_exit(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("exit", &args, 1, 1, span)?;
    let code = whole("exit", &args[0], span)?;
    match u8::try_from(code) {
        Ok(status) => Err(crate::value::exit_with(status, span)),
        Err(_) => raise("ValueError", format!("a program ends with a number from 0 to 255, not {code}"), span),
    }
}

/// `program.environment(name)`: what the environment the program was
/// started in says `name` is (`HOME`, `PATH`), or `none` if it does not
/// say (decision 34.2).
fn program_environment(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("environment", &args, 1, 1, span)?;
    let name = text("environment", &args[0], span)?;
    Ok(machine.outside().variable(name).map_or(Value::None, Value::str))
}

/// `program.warn(text)`: the text and a line end on the error stream,
/// which the shell keeps apart from what `print` prints (decision 34.3).
fn program_warn(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("warn", &args, 1, 1, span)?;
    machine.warn(text("warn", &args[0], span)?);
    Ok(Value::None)
}

/// `program.run(name, words)`: start another program with these words,
/// wait until it ends, and give the number it ended with (0 says that
/// all went well, by custom).
///
/// The words go to the other program one by one, exactly as they are:
/// no shell reads them, so a blank or a quote inside a word is part of
/// that word and nothing else. The other program uses this program's
/// keyboard and screen while it runs.
fn program_run(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("run", &args, 2, 2, span)?;
    let name = text("run", &args[0], span)?;
    let Value::List(items) = &args[1] else {
        return wrong_type("run", "a list of str", &args[1], span);
    };
    let mut words = Vec::new();
    for item in items.borrow().iter() {
        words.push(text("run", item, span)?.to_string());
    }
    // A page is inside a browser, which starts no programs.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = words;
        file_error(format!("cannot run `{name}`: a page cannot start a program"), span)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::io::Write;
        // What this program printed so far is on the screen before the
        // other one prints.
        let _ = std::io::stdout().flush();
        match std::process::Command::new(name).args(&words).status() {
            Ok(status) => Ok(Value::Int(ended_with(status))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                file_error(format!("there is no program `{name}` to run"), span)
            }
            Err(_) => file_error(format!("cannot run `{name}`"), span),
        }
    }
}

/// `python.module("yt_dlp")`: a module of Python, as `import yt_dlp`
/// gives it there. It stays in Python, which Zeno starts beside the
/// program the first time one is asked for (`bridge.rs`).
fn python_module(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("module", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Python, text("module", &args[0], span)?, span)
}

/// `python.copy(thing)`: a list, a tuple or a dict of Python as a list
/// or a dict of Zeno, with everything in it. A value that is one of
/// Zeno's already is given back as it is.
fn python_copy(_: &mut dyn Machine, mut args: Vec<Value>, span: Span) -> Result {
    arity("copy", &args, 1, 1, span)?;
    match args.remove(0) {
        Value::Foreign(foreign) => crate::bridge::copy(&foreign, span),
        other => Ok(other),
    }
}

/// `node.module("fs")`: a module of JavaScript, as `require("fs")`
/// gives it in Node. It stays in Node, which Zeno starts beside the
/// program the first time it is asked for (`bridge.rs`).
fn node_module(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("module", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Node, text("module", &args[0], span)?, span)
}

/// `node.global("JSON")`: what JavaScript has under that name with no
/// module asked for: `JSON`, `Math`, `Date`, `fetch`, `console`.
fn node_global(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("global", &args, 1, 1, span)?;
    crate::bridge::global(crate::bridge::Lang::Node, text("global", &args[0], span)?, span)
}

/// `node.new(Date, 2026, 10, 18)`: what `new Date(2026, 10, 18)` is in
/// JavaScript. The class comes first, then what it is made from.
fn node_new(_: &mut dyn Machine, mut args: Vec<Value>, span: Span) -> Result {
    if args.is_empty() {
        return fatal("`new` takes the class to make an object of, and then what it is made from, but nothing was given", span);
    }
    match args.remove(0) {
        Value::Foreign(class) if class.lang == crate::bridge::Lang::Node => crate::bridge::new(&class, &args, span),
        other => wrong_type("new", "a class of JavaScript", &other, span),
    }
}

/// `java.type("java.util.ArrayList")`: a class of Java, by its whole
/// name. Calling it makes an object, and a dot after it is something
/// `static`. It stays in Java, which Zeno starts beside the program the
/// first time it is asked for (`bridge.rs`).
fn java_type(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("type", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Java, text("type", &args[0], span)?, span)
}

/// `java.jar("lib/gson.jar")`: a jar where Java then finds classes, or
/// a folder, which is its classes and every jar in it.
fn java_jar(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("jar", &args, 1, 1, span)?;
    crate::bridge::jar(crate::bridge::Lang::Java, text("jar", &args[0], span)?, span)?;
    Ok(Value::None)
}

/// `ruby.require("date")`: a library of Ruby, or a gem, as `require`
/// loads it there. What it brings is then reached by `ruby.constant`.
/// Ruby is started beside the program the first time it is asked for
/// (`bridge.rs`).
fn ruby_require(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("require", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Ruby, text("require", &args[0], span)?, span)?;
    Ok(Value::None)
}

/// `ruby.constant("Date")`: a class, a module or another constant of
/// Ruby, by its name as Ruby writes it: `"Net::HTTP"`.
fn ruby_constant(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("constant", &args, 1, 1, span)?;
    crate::bridge::global(crate::bridge::Lang::Ruby, text("constant", &args[0], span)?, span)
}

/// `ruby.symbol("name")`: what `:name` is in Ruby.
fn ruby_symbol(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("symbol", &args, 1, 1, span)?;
    crate::bridge::symbol(crate::bridge::Lang::Ruby, text("symbol", &args[0], span)?, span)
}

/// `php.type("DateTime")`: a class of PHP, by its name with its
/// namespace. Calling it makes an object, and a dot after it is a
/// constant or something `static`. PHP is started beside the program
/// the first time it is asked for (`bridge.rs`).
fn php_type(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("type", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Php, text("type", &args[0], span)?, span)
}

/// `php.callable("strrev")`: a function of PHP, by its name.
fn php_callable(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("callable", &args, 1, 1, span)?;
    crate::bridge::named(crate::bridge::Lang::Php, "function", text("callable", &args[0], span)?, span)
}

/// `php.constant("PHP_VERSION")`: a constant of PHP, by its name.
fn php_constant(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("constant", &args, 1, 1, span)?;
    crate::bridge::named(crate::bridge::Lang::Php, "constant", text("constant", &args[0], span)?, span)
}

/// `php.require("helper.php")`: a file of PHP, read once, from the
/// folder of the program.
fn php_require(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("require", &args, 1, 1, span)?;
    crate::bridge::named(crate::bridge::Lang::Php, "require", text("require", &args[0], span)?, span)?;
    Ok(Value::None)
}

/// `php.variable(value)`: a variable of PHP that holds the value. A
/// function of PHP that changes what it is given changes this, and
/// `.value` reads it after.
fn php_variable(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("variable", &args, 0, 1, span)?;
    crate::bridge::variable(crate::bridge::Lang::Php, args.first().unwrap_or(&Value::None), span)
}

/// `lua.require("lpeg")`: a module of Lua, as `require` gives it
/// there. Lua is started beside the program the first time it is
/// asked for (`bridge.rs`).
fn lua_require(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("require", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::Lua, text("require", &args[0], span)?, span)
}

/// `lua.global("string")`: what Lua has under that name with no
/// module asked for: `string`, `table`, `math`, `os`, `io`.
fn lua_global(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("global", &args, 1, 1, span)?;
    crate::bridge::global(crate::bridge::Lang::Lua, text("global", &args[0], span)?, span)
}

/// `csharp.type("System.Text.StringBuilder")`: a type of .NET, by its
/// name with its namespace. Calling it makes an object, and a dot after
/// it is something `static`. .NET is started beside the program the
/// first time it is asked for (`bridge.rs`).
fn csharp_type(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("type", &args, 1, 1, span)?;
    crate::bridge::module(crate::bridge::Lang::CSharp, text("type", &args[0], span)?, span)
}

/// `csharp.assembly("lib/Humanizer.dll")`: a .dll, or a folder of them,
/// where .NET then finds types.
fn csharp_assembly(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("assembly", &args, 1, 1, span)?;
    crate::bridge::jar(crate::bridge::Lang::CSharp, text("assembly", &args[0], span)?, span)?;
    Ok(Value::None)
}

/// What an `inline` block is to the VM: a function called with the
/// block's language and its text, where the block is written (decision
/// 61). No program can name it.
pub fn inline_block() -> Value {
    fn start(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
        match (&args[0], &args[1]) {
            (Value::Str(language), Value::Str(text)) => crate::inline::run(language, text, span),
            _ => fatal("an `inline` block is a language and its text", span),
        }
    }
    Value::Builtin(Builtin { name: "inline", call: start })
}

/// `c.library(library, declarations)`: a library of C, and the
/// functions of it that the declarations name (decision 44).
fn c_library(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("library", &args, 2, 2, span)?;
    crate::bridge_c::library(&args[0], &args[1], span)
}

/// The number a program ended with. One that was stopped from outside
/// (Ctrl-C, `kill`) has no number of its own: it gets 128 plus the
/// number of the signal that stopped it, which is what a shell says.
#[cfg(not(target_arch = "wasm32"))]
fn ended_with(status: std::process::ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return code as i64;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal as i64;
        }
    }
    1
}

fn len(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("len", &args, 1, 1, span)?;
    match &args[0] {
        Value::Str(text) => Ok(Value::Int(text.chars().count() as i64)),
        Value::List(items) => Ok(Value::Int(items.borrow().len() as i64)),
        Value::Dict(dict) => Ok(Value::Int(dict.borrow().len() as i64)),
        Value::Foreign(foreign) => crate::bridge::len(foreign, span),
        other => wrong_type("len", "a str, a list or a dict", other, span),
    }
}

/// `range(stop)`, `range(start, stop)`, `range(start, stop, step)`
fn range(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("range", &args, 1, 3, span)?;
    let mut ints = Vec::new();
    for arg in &args {
        match arg {
            Value::Int(n) => ints.push(*n),
            other => return wrong_type("range", "ints", other, span),
        }
    }
    let (start, stop, step) = match ints[..] {
        [stop] => (0, stop, 1),
        [start, stop] => (start, stop, 1),
        [start, stop, step] => (start, stop, step),
        _ => unreachable!(),
    };
    if step == 0 {
        return raise("ValueError", "the step of a range cannot be 0", span);
    }

    let mut items = Vec::new();
    let mut i = start;
    while (step > 0 && i < stop) || (step < 0 && i > stop) {
        items.push(Value::Int(i));
        // A step past the last int there is ends the range.
        match i.checked_add(step) {
            Some(next) => i = next,
            None => break,
        }
    }
    Ok(Value::list(items))
}

fn int(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("int", &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Float(x) if x.is_finite() => Ok(Value::Int(x.trunc() as i64)),
        Value::Float(x) => raise("ValueError", format!("cannot turn {x} into an int"), span),
        Value::F32(x) if x.is_finite() => Ok(Value::Int(x.trunc() as i64)),
        Value::F32(x) => raise("ValueError", format!("cannot turn {x} into an int"), span),
        Value::U64(n) => match i64::try_from(*n) {
            Ok(n) => Ok(Value::Int(n)),
            Err(_) => raise("OverflowError", format!("{n} does not fit in an int ({} to {})", i64::MIN, i64::MAX), span),
        },
        Value::Str(text) => match strip(text).replace('_', "").parse() {
            Ok(n) => Ok(Value::Int(n)),
            Err(_) => raise("ValueError", format!("cannot turn {} into an int", crate::value::quoted(text)), span),
        },
        other => wrong_type("int", "a number or a str", other, span),
    }
}

fn float(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("float", &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(x) => Ok(Value::Float(*x)),
        Value::F32(x) => Ok(Value::Float(*x as f64)),
        Value::U64(n) => Ok(Value::Float(*n as f64)),
        Value::Str(text) => match strip(text).parse() {
            Ok(x) => Ok(Value::Float(x)),
            Err(_) => raise("ValueError", format!("cannot turn {} into a float", crate::value::quoted(text)), span),
        },
        other => wrong_type("float", "a number or a str", other, span),
    }
}

/// `f32(x)`: the nearest f32 to a number (decision 41.5). A float too
/// big for 32 bits is an OverflowError, as 300 is for `u8( )`.
fn f32_of(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("f32", &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::F32(*n as f64 as f32)),
        Value::F32(x) => Ok(Value::F32(*x)),
        Value::Float(x) => match crate::value::to_f32(*x) {
            Some(near) => Ok(Value::F32(near)),
            None => raise("OverflowError", crate::value::too_big_for_f32(*x), span),
        },
        other => wrong_type("f32", "a number", other, span),
    }
}

/// `u64(n)`: `n` itself if it is not below 0 (decision 42.4).
fn u64_of(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("u64", &args, 1, 1, span)?;
    match &args[0] {
        Value::U64(n) => Ok(Value::U64(*n)),
        Value::Int(n) => match u64::try_from(*n) {
            Ok(n) => Ok(Value::U64(n)),
            Err(_) => raise("OverflowError", format!("{n} does not fit in a u64 (0 to {})", u64::MAX), span),
        },
        other => wrong_type("u64", "a whole number", other, span),
    }
}

/// `u64.wrap(n)`, which the checker names `u64..of`: the 64 bits of an
/// int read as a u64, so -1 is the largest (decision 42.6). A u64 written
/// out past the largest int comes here as the int with its bits.
fn u64_bits(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("u64.wrap", &args, 1, 1, span)?;
    match &args[0] {
        Value::U64(n) => Ok(Value::U64(*n)),
        Value::Int(n) => Ok(Value::U64(*n as u64)),
        other => wrong_type("u64.wrap", "a whole number", other, span),
    }
}

/// `+`, `-`, `*` and `<<` of u64s written straight inside `u64.wrap( )`:
/// the answer cut to 64 bits, where it would have failed (42.6).
fn u64_wrapping<const N: usize>(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let name = ["u64.wrap", "u64.wrap", "u64.wrap", "u64.wrap"][N];
    arity(name, &args, 2, 2, span)?;
    // A shift is by a count, an int or a u64, as `<<` takes (42.6).
    if let (3, Some(a), Value::Int(_) | Value::U64(_)) = (N, args[0].unsigned(), &args[1]) {
        let count = match &args[1] {
            Value::Int(n) => *n as i128,
            Value::U64(n) => *n as i128,
            _ => 0,
        };
        if !(0..64).contains(&count) {
            return raise("ValueError", format!("cannot shift by {count}. A u64 has 64 bits, so a shift is 0 to 63"), span);
        }
        return Ok(Value::U64(a << count));
    }
    let (Some(a), Some(b)) = (args[0].unsigned(), args[1].unsigned()) else {
        return wrong_type(name, "u64s", if args[0].unsigned().is_none() { &args[0] } else { &args[1] }, span);
    };
    Ok(Value::U64(match N {
        0 => a.wrapping_add(b),
        1 => a.wrapping_sub(b),
        2 => a.wrapping_mul(b),
        _ => a << b.min(63),
    }))
}

fn str(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("str", &args, 1, 1, span)?;
    Ok(Value::str(args[0].to_string()))
}

fn abs(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("abs", &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) => match n.checked_abs() {
            Some(n) => Ok(Value::Int(n)),
            None => raise("OverflowError", "the result is too big for an int", span),
        },
        Value::Float(x) => Ok(Value::Float(x.abs())),
        Value::F32(x) => Ok(Value::F32(x.abs())),
        Value::U64(n) => Ok(Value::U64(*n)),
        Value::Quantity(x, unit) => Ok(Value::Quantity(x.abs(), unit.clone())),
        other => wrong_type("abs", "a number", other, span),
    }
}

/// What `sum` says to quantities, here and in the checker.
pub const SUM_OF_QUANTITIES: &str = "`sum` works on plain numbers, and these have a unit. Add quantities with `+`, \
                                     which keeps the unit: start from `var total = 0 V` and add each in a loop";

/// How much a number or a quantity is, without its unit.
fn amount(value: &Value) -> Option<f64> {
    match value {
        Value::Quantity(x, _) => Some(*x),
        other => other.number(),
    }
}

/// The numbers `min`, `max` and `sum` work on: one list, or several
/// arguments. They are all plain numbers, or all quantities in one
/// unit, and the unit is given back with them.
fn numbers(name: &str, args: Vec<Value>, span: Span) -> Result<(Vec<Value>, Option<Rc<str>>)> {
    let items = match args.as_slice() {
        [Value::List(items)] => items.borrow().clone(),
        _ => args,
    };
    let mut unit: Option<Rc<str>> = None;
    for (place, item) in items.iter().enumerate() {
        match item {
            Value::Quantity(_, its) => match &unit {
                Some(first) if first != its => {
                    return fatal(
                        format!(
                            "`{name}` needs quantities in one unit, but these are in {first} and in {its}. \
                             Turn one into the other first, as in `x.{first}`"
                        ),
                        span,
                    )
                }
                None if place > 0 => {
                    return fatal(format!("`{name}` needs all plain numbers or all quantities, but these are both"), span)
                }
                _ => unit = Some(its.clone()),
            },
            _ if item.number().is_none() => return wrong_type(name, "numbers", item, span),
            _ if unit.is_some() => {
                return fatal(format!("`{name}` needs all plain numbers or all quantities, but these are both"), span)
            }
            _ => {}
        }
    }
    Ok((items, unit))
}

/// Shared by `min` and `max`: keep the item that `better` prefers.
fn pick(name: &str, args: Vec<Value>, span: Span, better: fn(f64, f64) -> bool) -> Result {
    let items = numbers(name, args, span)?.0;
    // f32s with ints: the answer is an f32, whichever it is (41.2).
    let singles = items.iter().any(|item| matches!(item, Value::F32(_)))
        && items.iter().all(|item| matches!(item, Value::F32(_) | Value::Int(_)));
    let mut items = items.into_iter().map(|item| match (singles, item) {
        (true, Value::Int(n)) => Value::F32(n as f64 as f32),
        (_, item) => item,
    });
    let Some(mut best) = items.next() else {
        return raise("ValueError", format!("`{name}` of nothing"), span);
    };
    for item in items {
        // Two u64s are compared as they are: as floats, the large ones
        // would be rounded and could look equal (42.2).
        let wins = match (&item, &best) {
            (Value::U64(a), Value::U64(b)) if name == "min" => a < b,
            (Value::U64(a), Value::U64(b)) => a > b,
            _ => better(amount(&item).unwrap(), amount(&best).unwrap()),
        };
        if wins {
            best = item;
        }
    }
    Ok(best)
}

fn min(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    pick("min", args, span, |a, b| a < b)
}

fn max(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    pick("max", args, span, |a, b| a > b)
}

fn sum(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let (items, unit) = numbers("sum", args, span)?;
    // Not quantities: the sum of an empty list would have no unit to
    // be in (decision 17.3).
    if unit.is_some() {
        return fatal(SUM_OF_QUANTITIES, span);
    }
    // f32s, perhaps with ints, are added in 32 bits, one at a time.
    if items.iter().any(|item| matches!(item, Value::F32(_))) && items.iter().all(|item| matches!(item, Value::F32(_) | Value::Int(_))) {
        let mut total = 0.0f32;
        for item in &items {
            total += match item {
                Value::F32(x) => *x,
                Value::Int(n) => *n as f64 as f32,
                _ => 0.0,
            };
        }
        return Ok(Value::F32(total));
    }
    // u64s are added as u64s, and must fit (42.2).
    if items.iter().any(|item| matches!(item, Value::U64(_))) {
        let mut total = 0u64;
        for item in &items {
            let n = item.unsigned().unwrap_or(0);
            total = match total.checked_add(n) {
                Some(total) => total,
                None => return raise("OverflowError", crate::ops::TOO_BIG_FOR_U64, span),
            };
        }
        return Ok(Value::U64(total));
    }
    // Stay an int while every item is an int.
    let mut int_total: Option<i64> = Some(0);
    let mut float_total = 0.0;
    for item in &items {
        float_total += item.number().unwrap();
        int_total = match (int_total, item) {
            (Some(total), Value::Int(n)) => match total.checked_add(*n) {
                Some(total) => Some(total),
                None => return raise("OverflowError", "the result is too big for an int", span),
            },
            _ => None,
        };
    }
    Ok(int_total.map_or(Value::Float(float_total), Value::Int))
}

// ----------------------------------------------------------------- Sized ints

// ------------------------------------------------------------ Manual memory
//
// `allocate` and `free`, for the lines inside `unsafe { }`. See
// docs/unsafe.md. A compiled program gets real memory. Here a block is a
// list of values with a note of whether it has been freed, so that a
// mistake stops the program with its reason instead of damaging something.

/// The most items one block may have here. A compiled program has no
/// such limit. This one keeps a slip such as `allocate(u8, 1 << 40)`
/// from taking the whole computer along.
const MOST_ITEMS: i64 = 1 << 24;

/// The type a built-in such as `u8` or `float` names, where one is
/// written as the first argument of `allocate` or `register`.
fn named_type(name: &str, value: &Value, span: Span) -> Result<Pointee> {
    match value {
        Value::Builtin(builtin) if Pointee::named(builtin.name).is_some() => {
            Ok(Pointee::named(builtin.name).unwrap_or(Pointee::Int))
        }
        other => wrong_type(name, "a type first, such as u8", other, span),
    }
}

/// `allocate(u8, 16)`: a block of sixteen bytes, all zero.
fn allocate(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("allocate", &args, 2, 2, span)?;
    let pointee = named_type("allocate", &args[0], span)?;
    let count = match &args[1] {
        Value::Int(count) if (1..=MOST_ITEMS).contains(count) => *count as usize,
        Value::Int(count) if *count < 1 => {
            return fatal(format!("`allocate` needs at least one item, and this asks for {count}"), span)
        }
        Value::Int(count) => {
            return fatal(format!("`zeno run` allocates at most {MOST_ITEMS} items at once, and this asks for {count}"), span)
        }
        other => return wrong_type("allocate", "how many items, as an int", other, span),
    };
    let zero = if pointee == Pointee::Float { Value::Float(0.0) } else { Value::Int(0) };
    let memory = Memory { pointee, items: RefCell::new(vec![zero; count]), freed: std::cell::Cell::new(false) };
    Ok(Value::Pointer(Rc::new(memory), 0))
}

/// `free(p)`: give the block back. Every pointer into it is dead from
/// here on, and `zeno run` remembers that.
fn free(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("free", &args, 1, 1, span)?;
    match &args[0] {
        Value::Pointer(memory, _) if memory.freed.get() => fatal(
            "this memory was freed already. `zeno run` checks every `free`. \
             A compiled program does not: freeing twice damages the memory it hands out next",
            span,
        ),
        Value::Pointer(_, at) if *at != 0 => fatal(
            format!(
                "`free` needs the pointer that `allocate` gave, and this one points {} further on. \
                 `zeno run` checks every `free`. A compiled program does not",
                counted(*at as usize, "item")
            ),
            span,
        ),
        Value::Pointer(memory, _) => {
            memory.freed.set(true);
            memory.items.borrow_mut().clear();
            Ok(Value::None)
        }
        other => wrong_type("free", "a pointer that `allocate` gave", other, span),
    }
}

/// `register(u32, 0x4002_0014)`: the register of the board at that address.
fn register(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("register", &args, 2, 2, span)?;
    let width = match named_type("register", &args[0], span)? {
        Pointee::Sized(width) if !width.signed => width,
        _ => return fatal("a register is a u8, a u16 or a u32", span),
    };
    match &args[1] {
        Value::Int(address) => Ok(Value::Register(*address, width)),
        other => wrong_type("register", "an address, as an int", other, span),
    }
}

// A sized whole number is an ordinary `Value::Int` that is known to be in
// range: nothing about it is different once it exists. So all the work is
// in the two ways of making one.

/// `u8(n)`: `n` itself if a u8 can hold it, and an OverflowError if not.
fn fit_in(width: Width, args: Vec<Value>, span: Span) -> Result {
    arity(width.name, &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) if width.holds(*n) => Ok(Value::Int(*n)),
        Value::Int(n) => raise("OverflowError", width.does_not_fit(n), span),
        other => wrong_type(width.name, "a whole number", other, span),
    }
}

/// `u8.wrap(n)`: the low eight bits of `n`. Never an error.
fn wrap_to(width: Width, args: Vec<Value>, span: Span) -> Result {
    arity(width.wrap, &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(width.wrap(*n))),
        other => wrong_type(width.wrap, "a whole number", other, span),
    }
}

// A built-in is a plain function pointer, which cannot remember which size
// it is for. So there is one function for each size, stamped out from
// these two by the number in `::<N>`: `fit::<0>` is `u8( )`, because `u8`
// is first in `ints::WIDTHS`.

fn fit<const N: usize>(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    fit_in(ints::WIDTHS[N], args, span)
}

fn wrap<const N: usize>(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    wrap_to(ints::WIDTHS[N], args, span)
}

type Call = fn(&mut dyn Machine, Vec<Value>, Span) -> Result;
const FITS: [Call; 6] = [fit::<0>, fit::<1>, fit::<2>, fit::<3>, fit::<4>, fit::<5>];
const WRAPS: [Call; 6] = [wrap::<0>, wrap::<1>, wrap::<2>, wrap::<3>, wrap::<4>, wrap::<5>];

/// `u8.wrap`, given the built-in `u8`. `None` for any other built-in.
pub fn wrapping(sized: &Builtin) -> Option<Value> {
    let index = ints::WIDTHS.iter().position(|width| width.name == sized.name)?;
    // `u8` could also be a function of the program's own. Only the real
    // one has a `.wrap`.
    (sized.call as usize == FITS[index] as usize)
        .then(|| Value::Builtin(Builtin { name: ints::WIDTHS[index].wrap, call: WRAPS[index] }))
}

// -------------------------------------------------------------------- Methods

/// The blank characters that `strip`, `split`, `int` and `float` skip:
/// space, tab, the two line endings and the form feed. Only these, so that
/// the compiled program needs no table of the world's other blanks.
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

/// The lines of a text. A line ends at "\n", and a "\r" just before
/// that "\n" goes with it. A "\r" anywhere else stays in its line. The
/// last line need not end, and an end at the very end starts no line.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(at) => {
                let line = &rest[..at];
                out.push(line.strip_suffix('\r').unwrap_or(line));
                rest = &rest[at + 1..];
            }
            None => {
                out.push(rest);
                rest = "";
            }
        }
    }
    out
}

/// `text` without the blanks at either end.
fn strip(text: &str) -> &str {
    text.trim_matches(is_blank)
}

const STR_METHODS: &[&str] =
    &["upper", "lower", "strip", "split", "lines", "replace", "starts_with", "ends_with", "find", "slice", "code"];
const LIST_METHODS: &[&str] = &[
    "append", "pop", "map", "filter", "reduce", "insert", "reverse", "sort", "clear", "find", "slice", "join",
];
const DICT_METHODS: &[&str] = &["get", "pop", "keys", "values", "clear"];
const PIN_METHODS: &[&str] = &["on", "off", "toggle", "is_on"];
const REGISTER_METHODS: &[&str] = &["read", "write"];
pub const ELEMENT_METHODS: &[&str] = &[
    "on", "fire", "append", "remove", "clear", "get", "set", "add_class", "remove_class", "has_class", "show", "hide",
    "get_style", "set_style", "draw_rect", "draw_circle", "draw_line", "draw_text", "erase",
];
/// What an element has that is read and given a new value like a field
/// of an object: `title.text = "Hello"`.
pub const ELEMENT_FIELDS: &[&str] = &["text", "value"];

pub fn has_method(receiver: &Value, name: &str) -> bool {
    match receiver {
        Value::Str(_) => STR_METHODS.contains(&name),
        Value::List(_) => LIST_METHODS.contains(&name),
        Value::Dict(_) => DICT_METHODS.contains(&name),
        Value::Pin(_) => PIN_METHODS.contains(&name),
        Value::InputPin(_) => matches!(name, "is_high" | "is_low" | "interrupt"),
        Value::Interrupt(_) => name == "wait" || name == "edges",
        Value::Serial(_) => matches!(name, "write" | "write_bytes" | "read_byte" | "has_byte"),
        Value::AnalogPin(_) => name == "voltage" || name == "count",
        Value::PwmPin(_) => name == "duty" || name == "pulse",
        Value::Ticker(..) => matches!(name, "wait" | "ticks" | "restart"),
        Value::Register(..) => REGISTER_METHODS.contains(&name),
        Value::Element(..) => ELEMENT_METHODS.contains(&name),
        Value::Timer(_) => name == "stop",
        _ => false,
    }
}

/// The text inside a str argument of a string method.
fn text<'a>(method: &str, arg: &'a Value, span: Span) -> Result<&'a str> {
    match arg {
        Value::Str(text) => Ok(text),
        other => wrong_type(method, "a str", other, span),
    }
}

/// A whole number given to a method.
fn whole(method: &str, arg: &Value, span: Span) -> Result<i64> {
    match arg {
        Value::Int(n) => Ok(*n),
        other => wrong_type(method, "a whole number", other, span),
    }
}

/// Where `slice(from)` or `slice(from, to)` begins and ends, in something
/// `length` long. The end is not part of the slice, as in `range`.
fn ends(args: &[Value], length: usize, what: &str, span: Span) -> Result<(usize, usize)> {
    let from = whole("slice", &args[0], span)?;
    let to = match args.get(1) {
        Some(to) => whole("slice", to, span)?,
        None => length as i64,
    };
    if from < 0 || to < from || to > length as i64 {
        return raise(
            "IndexError",
            format!("slice {from} to {to} is out of range for a {what} of length {length}"),
            span,
        );
    }
    Ok((from as usize, to as usize))
}

/// Put `items` in order, smallest first, with `<`.
///
/// This is heapsort, and it is the same heapsort as `zeno_list_sort` in
/// runtime.c, step for step. Any way of sorting gives the same answer
/// for numbers that can all be told apart. Writing the same steps twice
/// means the two also agree on the ones that cannot (`0.0` and `-0.0`),
/// so a compiled program prints what `zeno run` prints.
///
/// Heapsort needs no second list and never calls itself, which is what
/// a firmware build wants (see docs/library.md).
fn sort(items: &mut [Value], span: Span) -> Result<()> {
    let before = |a: &Value, b: &Value| -> Result<bool> {
        match ops::binary(BinaryOp::Lt, a.clone(), b.clone(), span)? {
            Value::Bool(before) => Ok(before),
            _ => Ok(false),
        }
    };
    // Move the item at `root` down until both items below it are smaller.
    let sift = |items: &mut [Value], mut root: usize, end: usize| -> Result<()> {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return Ok(());
            }
            if child + 1 < end && before(&items[child], &items[child + 1])? {
                child += 1;
            }
            if !before(&items[root], &items[child])? {
                return Ok(());
            }
            items.swap(root, child);
            root = child;
        }
    };
    let len = items.len();
    for root in (0..len / 2).rev() {
        sift(items, root, len)?;
    }
    for end in (1..len).rev() {
        items.swap(0, end);
        sift(items, 0, end)?;
    }
    Ok(())
}

/// What `sort` says when the function it was given changed how many
/// items the list has. runtime.c says the same.
pub const SORT_CHANGED: &str = "the function given to `sort` changed how many items the list has";

/// Put `items` in the order of `keys`, smallest key first: `sort` with
/// a function, which gave one key for each item. Items with keys that
/// are neither before the other stay in the order they were in, which
/// is what `places` is for: where each item was.
///
/// The same heapsort as `sort` above, and as `zeno_list_sort_by` in
/// runtime.c, step for step.
fn sort_by(items: &mut [Value], keys: &mut [Value], span: Span) -> Result<()> {
    let mut places: Vec<usize> = (0..items.len()).collect();
    let less = |a: &Value, b: &Value| -> Result<bool> {
        match ops::binary(BinaryOp::Lt, a.clone(), b.clone(), span)? {
            Value::Bool(less) => Ok(less),
            _ => Ok(false),
        }
    };
    let before = |keys: &[Value], places: &[usize], a: usize, b: usize| -> Result<bool> {
        if less(&keys[a], &keys[b])? {
            return Ok(true);
        }
        if less(&keys[b], &keys[a])? {
            return Ok(false);
        }
        Ok(places[a] < places[b])
    };
    let sift = |items: &mut [Value], keys: &mut [Value], places: &mut [usize], mut root: usize, end: usize| -> Result<()> {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return Ok(());
            }
            if child + 1 < end && before(keys, places, child, child + 1)? {
                child += 1;
            }
            if !before(keys, places, root, child)? {
                return Ok(());
            }
            items.swap(root, child);
            keys.swap(root, child);
            places.swap(root, child);
            root = child;
        }
    };
    let len = items.len();
    for root in (0..len / 2).rev() {
        sift(items, keys, &mut places, root, len)?;
    }
    for end in (1..len).rev() {
        items.swap(0, end);
        keys.swap(0, end);
        places.swap(0, end);
        sift(items, keys, &mut places, 0, end)?;
    }
    Ok(())
}

pub fn call_method(
    machine: &mut dyn Machine,
    receiver: &Value,
    name: &str,
    mut args: Vec<Value>,
    span: Span,
) -> Result {
    match receiver {
        Value::Str(this) => match name {
            "upper" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::str(this.to_ascii_uppercase()))
            }
            "lower" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::str(this.to_ascii_lowercase()))
            }
            "strip" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::str(strip(this)))
            }
            "split" => {
                arity(name, &args, 0, 1, span)?;
                let parts: Vec<Value> = match args.first() {
                    // With no separator, split on any run of spaces.
                    None => this.split(is_blank).filter(|word| !word.is_empty()).map(Value::str).collect(),
                    Some(separator) => match text(name, separator, span)? {
                        "" => return raise("ValueError", "cannot split on an empty str", span),
                        separator => this.split(separator).map(Value::str).collect(),
                    },
                };
                Ok(Value::list(parts))
            }
            // The lines of a text: what a file holds, read one at a time.
            // A line ends at "\n", and a "\r" just before it (a file
            // from Windows) goes with it. The last line need not end.
            "lines" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::list(lines(this).into_iter().map(Value::str).collect()))
            }
            "replace" => {
                arity(name, &args, 2, 2, span)?;
                let (old, new) = (text(name, &args[0], span)?, text(name, &args[1], span)?);
                Ok(Value::str(this.replace(old, new)))
            }
            "starts_with" => {
                arity(name, &args, 1, 1, span)?;
                Ok(Value::Bool(this.starts_with(text(name, &args[0], span)?)))
            }
            "ends_with" => {
                arity(name, &args, 1, 1, span)?;
                Ok(Value::Bool(this.ends_with(text(name, &args[0], span)?)))
            }
            // Where `part` first begins, counted in characters, or
            // `none` if it is not there.
            "find" => {
                arity(name, &args, 1, 1, span)?;
                Ok(match this.find(text(name, &args[0], span)?) {
                    Some(byte) => Value::Int(this[..byte].chars().count() as i64),
                    None => Value::None,
                })
            }
            "slice" => {
                arity(name, &args, 1, 2, span)?;
                let (from, to) = ends(&args, this.chars().count(), "str", span)?;
                Ok(Value::str(this.chars().skip(from).take(to - from).collect::<String>()))
            }
            // The number Unicode gives to a character: 65 for "A".
            "code" => {
                arity(name, &args, 0, 0, span)?;
                let mut chars = this.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Ok(Value::Int(c as i64)),
                    _ => raise(
                        "ValueError",
                        format!(
                            "`code` needs a str of exactly one character, and this one has {}",
                            this.chars().count()
                        ),
                        span,
                    ),
                }
            }
            _ => fatal(format!("a str has no method `{name}`"), span),
        },
        Value::Dict(dict) => match name {
            // `get(key)` is the value, or `none` for a key that is not
            // there. `get(key, 0)` is the value, or the 0.
            "get" => {
                arity(name, &args, 1, 2, span)?;
                let found = dict.borrow().get(&ops::key(&args[0], span)?).cloned();
                Ok(found.or_else(|| args.get(1).cloned()).unwrap_or(Value::None))
            }
            // `pop(key)` takes the key out, and gives its value.
            "pop" => {
                arity(name, &args, 1, 1, span)?;
                let key = ops::key(&args[0], span)?;
                match dict.borrow_mut().remove(&key) {
                    Some(value) => Ok(value),
                    None => ops::no_key(&args[0], span),
                }
            }
            // New lists, in the order the keys were put in.
            "keys" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::list(dict.borrow().entries().iter().map(|(key, _)| key.clone()).collect()))
            }
            "values" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::list(dict.borrow().entries().iter().map(|(_, value)| value.clone()).collect()))
            }
            "clear" => {
                arity(name, &args, 0, 0, span)?;
                dict.borrow_mut().clear();
                Ok(Value::None)
            }
            _ => fatal(format!("a dict has no method `{name}`"), span),
        },
        Value::List(items) => match name {
            "append" => {
                arity(name, &args, 1, 1, span)?;
                items.borrow_mut().push(args.remove(0));
                Ok(Value::None)
            }
            "pop" => {
                arity(name, &args, 0, 1, span)?;
                // `pop(2)` takes out the item at position 2.
                if let Some(at) = args.first() {
                    let len = items.borrow().len();
                    let at = ops::position(at, len, "list", span)?;
                    return Ok(items.borrow_mut().remove(at));
                }
                match items.borrow_mut().pop() {
                    Some(item) => Ok(item),
                    None => raise("IndexError", "cannot pop from an empty list", span),
                }
            }
            // `insert(0, x)` puts `x` in front. The position may be one
            // past the last item, which is what `append` does.
            "insert" => {
                arity(name, &args, 2, 2, span)?;
                let (at, len) = (whole(name, &args[0], span)?, items.borrow().len());
                if at < 0 || at > len as i64 {
                    return raise(
                        "IndexError",
                        format!("cannot insert at {at} in a list of length {len}"),
                        span,
                    );
                }
                items.borrow_mut().insert(at as usize, args.remove(1));
                Ok(Value::None)
            }
            "reverse" => {
                arity(name, &args, 0, 0, span)?;
                items.borrow_mut().reverse();
                Ok(Value::None)
            }
            "clear" => {
                arity(name, &args, 0, 0, span)?;
                items.borrow_mut().clear();
                Ok(Value::None)
            }
            // `people.sort(age_of)`: the function gives a key for each
            // item, and the items go in the order of their keys.
            "sort" if args.len() == 1 => {
                let function = args.remove(0);
                let mut keys = Vec::new();
                // The list is let go of before the function is called, which
                // may change it.
                let walked = items.borrow().clone();
                for item in walked {
                    let key = machine.call(function.clone(), vec![item], Vec::new(), span)?;
                    if !matches!(key, Value::Int(_) | Value::Float(_) | Value::F32(_) | Value::U64(_) | Value::Str(_)) {
                        return fatal(
                            format!(
                                "the function given to `sort` must return a number or text to order by, but it returned {}",
                                key.type_name()
                            ),
                            span,
                        );
                    }
                    keys.push(key);
                }
                let mut sorted = items.borrow().clone();
                if sorted.len() != keys.len() {
                    return raise("ValueError", SORT_CHANGED, span);
                }
                sort_by(&mut sorted, &mut keys, span)?;
                *items.borrow_mut() = sorted;
                Ok(Value::None)
            }
            "sort" => {
                arity(name, &args, 0, 1, span)?;
                // Sorted on the side, so that a `<` which fails leaves
                // the list as it was.
                let mut sorted = items.borrow().clone();
                sort(&mut sorted, span)?;
                *items.borrow_mut() = sorted;
                Ok(Value::None)
            }
            // The position of the first item equal to the one given, or
            // `none`.
            "find" => {
                arity(name, &args, 1, 1, span)?;
                Ok(match items.borrow().iter().position(|item| equal(item, &args[0])) {
                    Some(at) => Value::Int(at as i64),
                    None => Value::None,
                })
            }
            "slice" => {
                arity(name, &args, 1, 2, span)?;
                let (from, to) = ends(&args, items.borrow().len(), "list", span)?;
                Ok(Value::list(items.borrow()[from..to].to_vec()))
            }
            // `words.join(", ")`: the items, with the separator between.
            "join" => {
                arity(name, &args, 1, 1, span)?;
                let separator = text(name, &args[0], span)?;
                let mut pieces = Vec::new();
                for item in items.borrow().iter() {
                    match item {
                        Value::Str(piece) => pieces.push(piece.to_string()),
                        other => {
                            return fatal(
                                format!(
                                    "`join` puts text together, and this list holds {}. \
                                     Make each item text first, with `str( )`",
                                    other.type_name()
                                ),
                                span,
                            )
                        }
                    }
                }
                Ok(Value::str(pieces.join(separator)))
            }

            // The three below take a function and call it on the items.
            // They work on a copy, so the function may safely change the list.
            "map" => {
                arity(name, &args, 1, 1, span)?;
                let function = args.remove(0);
                let mut mapped = Vec::new();
                // The list is let go of before the function is called, which
                // may change it.
                let walked = items.borrow().clone();
                for item in walked {
                    mapped.push(machine.call(function.clone(), vec![item], Vec::new(), span)?);
                }
                Ok(Value::list(mapped))
            }
            "filter" => {
                arity(name, &args, 1, 1, span)?;
                let function = args.remove(0);
                let mut kept = Vec::new();
                // The list is let go of before the function is called, which
                // may change it.
                let walked = items.borrow().clone();
                for item in walked {
                    match machine.call(function.clone(), vec![item.clone()], Vec::new(), span)? {
                        Value::Bool(true) => kept.push(item),
                        Value::Bool(false) => {}
                        other => {
                            return fatal(
                                format!(
                                    "the function given to `filter` must return true or false, but it returned {}",
                                    other.type_name()
                                ),
                                span,
                            )
                        }
                    }
                }
                Ok(Value::list(kept))
            }
            // `items.reduce(function(total, item) { ... }, start)` folds the
            // list into one value, beginning from `start`.
            "reduce" => {
                arity(name, &args, 2, 2, span)?;
                let function = args.remove(0);
                let mut total = args.remove(0);
                // The list is let go of before the function is called, which
                // may change it.
                let walked = items.borrow().clone();
                for item in walked {
                    total = machine.call(function.clone(), vec![total, item], Vec::new(), span)?;
                }
                Ok(total)
            }
            _ => fatal(format!("a list has no method `{name}`"), span),
        },
        // A pin of the board. Driving it goes through `drive`, so that
        // the line the simulated board prints comes out with the rest of
        // the program's output.
        Value::Pin(pin) => {
            arity(name, &args, 0, 0, span)?;
            match name {
                "on" => drive(machine, *pin, true),
                "off" => drive(machine, *pin, false),
                "toggle" => {
                    let level = !machine.board().read(*pin);
                    drive(machine, *pin, level)
                }
                "is_on" => Ok(Value::Bool(machine.board().read(*pin))),
                _ => fatal(format!("a pin has no method `{name}`"), span),
            }
        }
        // A serial port of the simulated board is the terminal: what is
        // sent is written out as it is, and what is read is typed.
        Value::Serial(_) => match name {
            "write" => {
                arity(name, &args, 1, 1, span)?;
                let sent = text("write", &args[0], span)?.to_string();
                machine.write(&sent);
                Ok(Value::None)
            }
            "write_bytes" => {
                arity(name, &args, 1, 1, span)?;
                let bytes = byte_list("write_bytes", &args[0], span)?;
                machine.write(&String::from_utf8_lossy(&bytes));
                Ok(Value::None)
            }
            "read_byte" => {
                arity(name, &args, 0, 0, span)?;
                Ok(machine.outside().byte().map_or(Value::None, |byte| Value::Int(i64::from(byte))))
            }
            "has_byte" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::Bool(machine.outside().has_byte()))
            }
            _ => fatal(format!("a serial port has no method `{name}`"), span),
        },
        // `beat.wait()`, `beat.ticks()` and `beat.restart()`, on the
        // clock `now()` reads.
        Value::Ticker(ticker, _) => {
            arity(name, &args, 0, 0, span)?;
            match name {
                "wait" => {
                    // A sleep may end a little early, so the clock is
                    // asked again until the tick has come.
                    loop {
                        let left = machine.board().ticker_left(*ticker, board::now());
                        if !(left > 0.0) {
                            break;
                        }
                        // A page cannot stop and wait (see `sleep`).
                        #[cfg(target_arch = "wasm32")]
                        return fatal(
                            "a program in a page cannot `wait` for a tick: the page would stand still until it came. \
                             Ask with `ticks()`, or say what is to happen later with `web.every(10 ms, ...)`",
                            span,
                        );
                        #[cfg(not(target_arch = "wasm32"))]
                        sleep(machine, vec![Value::Quantity(left, Rc::from("ms"))], span)?;
                    }
                    Ok(Value::Int(machine.board().ticker_ticks(*ticker, board::now())))
                }
                "ticks" => Ok(Value::Int(machine.board().ticker_ticks(*ticker, board::now()))),
                "restart" => {
                    machine.board().ticker_restart(*ticker, board::now());
                    Ok(Value::None)
                }
                _ => fatal(format!("a ticker has no method `{name}`"), span),
            }
        }
        // `lamp.duty(0.25)` and `servo.pulse(1500 us)`: the share of the
        // time the pin is on, said as a share or as a time in each
        // period.
        Value::PwmPin(pin) => {
            arity(name, &args, 1, 1, span)?;
            let hertz = machine.board().pwm_hertz(*pin);
            let share = match (name, &args[0]) {
                ("duty", Value::Float(share)) => {
                    if !(*share >= 0.0 && *share <= 1.0) {
                        return raise(
                            "ValueError",
                            format!(
                                "a duty is from 0.0 to 1.0, and this is {}. For a quarter of the time on, write 0.25",
                                Value::Float(*share)
                            ),
                            span,
                        );
                    }
                    *share
                }
                ("duty", other) => return wrong_type("duty", "a float from 0.0 to 1.0", other, span),
                ("pulse", Value::Quantity(time, unit)) => {
                    let Some(scale) = crate::units::convert(unit, "s") else {
                        return fatal(format!("`pulse` needs a time, such as `1500 us`, not a quantity in {unit}"), span);
                    };
                    let seconds = scale.apply(*time);
                    let mut share = seconds * hertz;
                    // A pulse of one whole period, give or take the last
                    // digit of the sum, is all the time on.
                    if share > 1.0 && share <= 1.0 + 1e-9 {
                        share = 1.0;
                    }
                    if !(share >= 0.0 && share <= 1.0) {
                        return raise(
                            "ValueError",
                            format!(
                                "a pulse lasts from no time to one whole period, which is {} at this frequency, and this is {}",
                                Value::Quantity(1.0 / hertz, Rc::from("s")),
                                Value::Quantity(seconds, Rc::from("s"))
                            ),
                            span,
                        );
                    }
                    share
                }
                ("pulse", other) => return wrong_type("pulse", "a time with a unit, such as `1500 us`", other, span),
                _ => return fatal(format!("a PWM pin has no method `{name}`"), span),
            };
            pwm(machine, *pin, hertz, share);
            Ok(Value::None)
        }
        // `sensor.voltage()` and `sensor.count()`: what the converter
        // reads on the pin now. The voltage is the count's share of the
        // largest count, times the reference, worked out in that order
        // in every engine and build.
        Value::AnalogPin(pin) => {
            arity(name, &args, 0, 0, span)?;
            let count = machine.board().analog_read(*pin);
            match name {
                "count" => Ok(Value::Int(count)),
                "voltage" => {
                    let volts = count as f64 / board::analog_max() as f64 * board::ANALOG_REFERENCE;
                    Ok(Value::Quantity(volts, Rc::from("V")))
                }
                _ => fatal(format!("an analog pin has no method `{name}`"), span),
            }
        }
        // `button.interrupt(falling)`: the board counts those edges of
        // the pin from now on.
        Value::InputPin(pin) if name == "interrupt" => {
            arity(name, &args, 1, 1, span)?;
            let edge = match &args[0] {
                Value::Str(edge) if &**edge == "rising" => board::RISING,
                Value::Str(edge) if &**edge == "falling" => board::FALLING,
                Value::Str(edge) if &**edge == "either" => board::RISING | board::FALLING,
                other => return wrong_type("interrupt", "`rising`, `falling` or `either`", other, span),
            };
            machine.board().interrupt(*pin, edge);
            Ok(Value::Interrupt(*pin))
        }
        // `press.edges()`: how many edges have come since the program
        // last took them. `press.wait()`: the same, after waiting for
        // one. Nothing is wired to the simulated board, and nothing can
        // drive a pin while the program waits, so a wait with no edge
        // counted would never end: it is an error here.
        Value::Interrupt(pin) => {
            arity(name, &args, 0, 0, span)?;
            let edges = machine.board().edges(*pin);
            match name {
                "edges" => Ok(Value::Int(edges)),
                "wait" if edges > 0 => Ok(Value::Int(edges)),
                "wait" => raise("ValueError", no_edge_can_come(*pin), span),
                _ => fatal(format!("an interrupt has no method `{name}`"), span),
            }
        }
        // `button.is_low()`: what the pin reads now.
        Value::InputPin(pin) => {
            arity(name, &args, 0, 0, span)?;
            match name {
                "is_high" => Ok(Value::Bool(machine.board().read(*pin))),
                "is_low" => Ok(Value::Bool(!machine.board().read(*pin))),
                _ => fatal(format!("an input pin has no method `{name}`"), span),
            }
        }
        Value::Element(id, _) => element_method(machine, *id, name, args, span),
        // `ticking.stop()`: the call that waits is called off. Stopping
        // what has stopped, or what has been called already, is nothing.
        Value::Timer(id) => match name {
            "stop" => {
                arity("stop", &args, 0, 0, span)?;
                machine.page().stop(*id);
                Ok(Value::None)
            }
            _ => fatal(format!("a Timer has no method `{name}`"), span),
        },
        // A register of the simulated board holds what was last written
        // to it, and the board prints a line when that changes.
        Value::Register(address, width) => match name {
            "read" => {
                arity(name, &args, 0, 0, span)?;
                Ok(Value::Int(machine.board().register_read(*address)))
            }
            "write" => {
                arity(name, &args, 1, 1, span)?;
                let value = match &args[0] {
                    Value::Int(value) if width.holds(*value) => *value,
                    Value::Int(value) => return raise("OverflowError", width.does_not_fit(value), span),
                    other => return wrong_type("write", "a whole number", other, span),
                };
                let line = machine.board().register_write(*address, value, (width.bits / 4) as usize);
                machine.write_line(&line);
                Ok(Value::None)
            }
            _ => fatal(format!("a register has no method `{name}`"), span),
        },
        other => fatal(format!("{} has no method `{name}`", other.type_name()), span),
    }
}

// -------------------------------------------------------------------- Modules

/// Find a built-in module by name, for `import`.
pub fn module(name: &str) -> Option<Rc<Module>> {
    let (name, members): (&'static str, Vec<(&'static str, Value)>) = match name {
        "math" => (
            "math",
            vec![
                ("pi", Value::Float(std::f64::consts::PI)),
                ("e", Value::Float(std::f64::consts::E)),
                ("sqrt", Value::Builtin(Builtin { name: "sqrt", call: sqrt })),
                ("floor", Value::Builtin(Builtin { name: "floor", call: floor })),
                ("ceil", Value::Builtin(Builtin { name: "ceil", call: ceil })),
                ("round", Value::Builtin(Builtin { name: "round", call: round })),
                ("sin", Value::Builtin(Builtin { name: "sin", call: sin })),
                ("cos", Value::Builtin(Builtin { name: "cos", call: cos })),
                ("tan", Value::Builtin(Builtin { name: "tan", call: tan })),
                ("asin", Value::Builtin(Builtin { name: "asin", call: asin })),
                ("acos", Value::Builtin(Builtin { name: "acos", call: acos })),
                ("atan", Value::Builtin(Builtin { name: "atan", call: atan })),
                ("atan2", Value::Builtin(Builtin { name: "atan2", call: atan2 })),
                ("exp", Value::Builtin(Builtin { name: "exp", call: exp })),
                ("log", Value::Builtin(Builtin { name: "log", call: log })),
                ("log2", Value::Builtin(Builtin { name: "log2", call: log2 })),
                ("log10", Value::Builtin(Builtin { name: "log10", call: log10 })),
            ],
        ),
        "time" => (
            "time",
            vec![
                ("sleep", Value::Builtin(Builtin { name: "sleep", call: sleep })),
                ("now", Value::Builtin(Builtin { name: "now", call: now })),
            ],
        ),
        "files" => (
            "files",
            vec![
                ("read", Value::Builtin(Builtin { name: "read", call: files_read })),
                ("write", Value::Builtin(Builtin { name: "write", call: files_write })),
                ("read_bytes", Value::Builtin(Builtin { name: "read_bytes", call: files_read_bytes })),
                ("write_bytes", Value::Builtin(Builtin { name: "write_bytes", call: files_write_bytes })),
                ("append", Value::Builtin(Builtin { name: "append", call: files_append })),
                ("exists", Value::Builtin(Builtin { name: "exists", call: files_exists })),
                ("remove", Value::Builtin(Builtin { name: "remove", call: files_remove })),
                ("list", Value::Builtin(Builtin { name: "list", call: files_list })),
                ("make_folder", Value::Builtin(Builtin { name: "make_folder", call: files_make_folder })),
                ("handle_open", Value::Builtin(Builtin { name: "handle_open", call: files_handle_open })),
                ("handle_line", Value::Builtin(Builtin { name: "handle_line", call: files_handle_line })),
                ("handle_bytes", Value::Builtin(Builtin { name: "handle_bytes", call: files_handle_bytes })),
                ("handle_write", Value::Builtin(Builtin { name: "handle_write", call: files_handle_write })),
                ("handle_write_bytes", Value::Builtin(Builtin { name: "handle_write_bytes", call: files_handle_write_bytes })),
                ("handle_close", Value::Builtin(Builtin { name: "handle_close", call: files_handle_close })),
            ],
        ),
        "program" => (
            "program",
            vec![
                ("arguments", Value::Builtin(Builtin { name: "arguments", call: program_arguments })),
                ("run", Value::Builtin(Builtin { name: "run", call: program_run })),
                ("exit", Value::Builtin(Builtin { name: "exit", call: program_exit })),
                ("environment", Value::Builtin(Builtin { name: "environment", call: program_environment })),
                ("warn", Value::Builtin(Builtin { name: "warn", call: program_warn })),
            ],
        ),
        "python" => (
            "python",
            vec![
                ("module", Value::Builtin(Builtin { name: "module", call: python_module })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "node" => (
            "node",
            vec![
                ("module", Value::Builtin(Builtin { name: "module", call: node_module })),
                ("global", Value::Builtin(Builtin { name: "global", call: node_global })),
                ("new", Value::Builtin(Builtin { name: "new", call: node_new })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "java" => (
            "java",
            vec![
                ("type", Value::Builtin(Builtin { name: "type", call: java_type })),
                ("jar", Value::Builtin(Builtin { name: "jar", call: java_jar })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "ruby" => (
            "ruby",
            vec![
                ("require", Value::Builtin(Builtin { name: "require", call: ruby_require })),
                ("constant", Value::Builtin(Builtin { name: "constant", call: ruby_constant })),
                ("symbol", Value::Builtin(Builtin { name: "symbol", call: ruby_symbol })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "php" => (
            "php",
            vec![
                ("type", Value::Builtin(Builtin { name: "type", call: php_type })),
                ("callable", Value::Builtin(Builtin { name: "callable", call: php_callable })),
                ("constant", Value::Builtin(Builtin { name: "constant", call: php_constant })),
                ("require", Value::Builtin(Builtin { name: "require", call: php_require })),
                ("variable", Value::Builtin(Builtin { name: "variable", call: php_variable })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "lua" => (
            "lua",
            vec![
                ("require", Value::Builtin(Builtin { name: "require", call: lua_require })),
                ("global", Value::Builtin(Builtin { name: "global", call: lua_global })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "csharp" => (
            "csharp",
            vec![
                ("type", Value::Builtin(Builtin { name: "type", call: csharp_type })),
                ("assembly", Value::Builtin(Builtin { name: "assembly", call: csharp_assembly })),
                ("copy", Value::Builtin(Builtin { name: "copy", call: python_copy })),
            ],
        ),
        "c" => ("c", vec![("library", Value::Builtin(Builtin { name: "library", call: c_library }))]),
        "json" => (
            "json",
            vec![
                ("parse", Value::Builtin(Builtin { name: "parse", call: json_parse })),
                ("write", Value::Builtin(Builtin { name: "write", call: json_write })),
            ],
        ),
        "web" => (
            "web",
            vec![
                ("element", Value::Builtin(Builtin { name: "element", call: web_element })),
                ("elements", Value::Builtin(Builtin { name: "elements", call: web_elements })),
                ("create", Value::Builtin(Builtin { name: "create", call: web_create })),
                ("after", Value::Builtin(Builtin { name: "after", call: web_after })),
                ("every", Value::Builtin(Builtin { name: "every", call: web_every })),
                ("on_key", Value::Builtin(Builtin { name: "on_key", call: web_on_key })),
                ("save", Value::Builtin(Builtin { name: "save", call: web_save })),
                ("load", Value::Builtin(Builtin { name: "load", call: web_load })),
                ("forget", Value::Builtin(Builtin { name: "forget", call: web_forget })),
                ("fetch", Value::Builtin(Builtin { name: "fetch", call: web_fetch })),
                ("post", Value::Builtin(Builtin { name: "post", call: web_post })),
            ],
        ),
        "board" => (
            "board",
            vec![
                ("led", Value::Pin(board::LED)),
                ("pin", Value::Builtin(Builtin { name: "pin", call: pin })),
                ("register", Value::Builtin(Builtin { name: "register", call: register })),
                ("input_pin", Value::Builtin(Builtin { name: "input_pin", call: input_pin })),
                ("serial", Value::Builtin(Builtin { name: "serial", call: serial })),
                ("analog_pin", Value::Builtin(Builtin { name: "analog_pin", call: analog_pin })),
                ("pwm_pin", Value::Builtin(Builtin { name: "pwm_pin", call: pwm_pin })),
                ("ticker", Value::Builtin(Builtin { name: "ticker", call: ticker })),
                // How an input is held: the str of its name, as in a
                // compiled program (decision 45.3).
                ("pull_up", Value::Str(Rc::from("pull_up"))),
                ("pull_down", Value::Str(Rc::from("pull_down"))),
                // Which edges an interrupt counts, by the same rule
                // (decision 50.1).
                ("rising", Value::Str(Rc::from("rising"))),
                ("falling", Value::Str(Rc::from("falling"))),
                ("either", Value::Str(Rc::from("either"))),
            ],
        ),
        _ => return None,
    };
    Some(Rc::new(Module { name, members: HashMap::from_iter(members) }))
}

/// `char(65)`: the character Unicode gives that number to, as a str.
/// The opposite of `"A".code()`.
fn char_of(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("char", &args, 1, 1, span)?;
    let code = whole("char", &args[0], span)?;
    // 0 is left out: it could not be told from the end of the text in
    // a compiled program, where text ends at the first zero byte.
    match u32::try_from(code).ok().filter(|code| *code != 0).and_then(char::from_u32) {
        Some(c) => Ok(Value::str(c.to_string())),
        None => raise("ValueError", format!("{code} is not the code of a character"), span),
    }
}

// ---------------------------------------------------------------- Files
//
// The `files` module: a whole file read as text, text written to a file,
// and two questions about a path. Each function here has a twin in
// runtime.c (`zeno_files_read` and the rest), and the two are written
// to give the same answer and the same message in every case, so that
// a compiled program prints what `zeno run` prints.

/// What is at a path.
#[derive(PartialEq)]
enum At {
    /// Nothing.
    Nothing,
    /// A file that can be read.
    File,
    /// Something else: a folder, or a file this program may not read.
    Other,
}

/// Open the path and try to read one byte. A folder can be opened on
/// some systems, and only fails when it is read.
fn at(path: &str) -> At {
    use std::io::Read;
    match std::fs::File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => At::Nothing,
        Err(_) => At::Other,
        Ok(mut file) => match file.read(&mut [0u8; 1]) {
            Ok(_) => At::File,
            Err(_) => At::Other,
        },
    }
}

/// The text arguments of a `files` function.
fn paths<'a>(name: &str, args: &'a [Value], count: usize, span: Span) -> Result<Vec<&'a str>> {
    arity(name, args, count, count, span)?;
    args.iter().map(|arg| text(name, arg, span)).collect()
}

fn file_error<T>(message: String, span: Span) -> Result<T> {
    raise("FileError", message, span)
}

/// `files.read(path)`: everything in the file, as one str.
fn files_read(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let path = paths("read", &args, 1, span)?[0];
    match at(path) {
        At::Nothing => return file_error(format!("there is no file `{path}`"), span),
        At::Other => return file_error(format!("cannot read `{path}`"), span),
        At::File => {}
    }
    let Ok(bytes) = std::fs::read(path) else {
        return file_error(format!("cannot read `{path}`"), span);
    };
    // Text is UTF-8 with no zero byte in it: a compiled program ends
    // text at the first zero.
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(Value::str(text)),
        _ => file_error(format!("`{path}` does not hold text"), span),
    }
}

/// `files.read_bytes(path)`: everything in the file, as a `list[u8]`,
/// one item a byte, whatever the file holds (decision 36.1).
fn files_read_bytes(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let path = paths("read_bytes", &args, 1, span)?[0];
    match at(path) {
        At::Nothing => return file_error(format!("there is no file `{path}`"), span),
        At::Other => return file_error(format!("cannot read `{path}`"), span),
        At::File => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(Value::list(bytes.into_iter().map(|byte| Value::Int(byte as i64)).collect())),
        Err(_) => file_error(format!("cannot read `{path}`"), span),
    }
}

/// `files.write_bytes(path, bytes)` makes the file, or replaces what was
/// in it, with one byte for each item of the list.
fn files_write_bytes(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    use std::io::Write;
    arity("write_bytes", &args, 2, 2, span)?;
    let path = text("write_bytes", &args[0], span)?;
    let Value::List(items) = &args[1] else {
        return wrong_type("write_bytes", "a list[u8]", &args[1], span);
    };
    let mut bytes = Vec::with_capacity(items.borrow().len());
    for item in items.borrow().iter() {
        match item {
            Value::Int(n) if (0..=255).contains(n) => bytes.push(*n as u8),
            Value::Int(n) => {
                return raise("ValueError", format!("{n} is not a byte: a byte is a number from 0 to 255"), span)
            }
            other => return wrong_type("write_bytes", "a list[u8]", other, span),
        }
    }
    let file = std::fs::OpenOptions::new().write(true).truncate(true).create(true).open(path);
    match file.and_then(|mut file| file.write_all(&bytes)) {
        Ok(()) => Ok(Value::None),
        Err(_) => file_error(format!("cannot write `{path}`"), span),
    }
}

/// `files.write(path, text)` makes the file, or replaces what was in
/// it. `files.append(path, text)` adds to the end.
fn put(name: &str, args: Vec<Value>, append: bool, span: Span) -> Result {
    use std::io::Write;
    let texts = paths(name, &args, 2, span)?;
    let (path, text) = (texts[0], texts[1]);
    let mut options = std::fs::OpenOptions::new();
    if append { options.append(true) } else { options.write(true).truncate(true) };
    match options.create(true).open(path).and_then(|mut file| file.write_all(text.as_bytes())) {
        Ok(()) => Ok(Value::None),
        Err(_) => file_error(format!("cannot write `{path}`"), span),
    }
}

fn files_write(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    put("write", args, false, span)
}

fn files_append(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    put("append", args, true, span)
}

// ------------------------------------------------- A file in pieces
//
// What a `Reader` and a `Writer` of library/files.zeno are made of
// (decision 59.6). Only that file can name these: the classes keep the
// number of the open file to themselves, and close it in `release`.

/// `files.handle_open(path, how)`: open the file to read (0), to write
/// from empty (1) or to add to its end (2), as bytes on every system,
/// and give its number.
fn files_handle_open(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("handle_open", &args, 2, 2, span)?;
    let path = text("handle_open", &args[0], span)?;
    let Value::Int(how) = args[1] else { return wrong_type("handle_open", "an int", &args[1], span) };
    let file = match how {
        0 => {
            match at(path) {
                At::Nothing => return file_error(format!("there is no file `{path}`"), span),
                At::Other => return file_error(format!("cannot read `{path}`"), span),
                At::File => {}
            }
            match std::fs::File::open(path) {
                Ok(file) => crate::value::OpenFile::Reading(std::io::BufReader::new(file)),
                Err(_) => return file_error(format!("cannot read `{path}`"), span),
            }
        }
        _ => {
            let mut options = std::fs::OpenOptions::new();
            if how == 2 { options.append(true) } else { options.write(true).truncate(true) };
            match options.create(true).open(path) {
                Ok(file) => crate::value::OpenFile::Writing(file),
                Err(_) => return file_error(format!("cannot write `{path}`"), span),
            }
        }
    };
    let open = &mut machine.outside().files.open;
    let place = match open.iter().position(Option::is_none) {
        Some(place) => {
            open[place] = Some(file);
            place
        }
        None => {
            open.push(Some(file));
            open.len() - 1
        }
    };
    Ok(Value::Int(place as i64 + 1))
}

/// The open file numbered by the first argument. Only library/files.zeno
/// gives these numbers, and only while the file is open.
fn handle<'a>(machine: &'a mut dyn Machine, name: &str, args: &[Value], span: Span) -> Result<&'a mut crate::value::OpenFile> {
    let Value::Int(number) = args[0] else { return wrong_type(name, "an int", &args[0], span) };
    let open = &mut machine.outside().files.open;
    match usize::try_from(number - 1).ok().and_then(|place| open.get_mut(place)).and_then(Option::as_mut) {
        Some(file) => Ok(file),
        None => fatal(format!("there is no open file numbered {number}"), span),
    }
}

/// `files.handle_line(file, path)`: the next line without its line end
/// (a `\r` before the `\n` goes with it, 6.17), or `none` at the end.
fn files_handle_line(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    use std::io::BufRead;
    arity("handle_line", &args, 2, 2, span)?;
    let path = text("handle_line", &args[1], span)?.to_string();
    let crate::value::OpenFile::Reading(reader) = handle(machine, "handle_line", &args, span)? else {
        return file_error(format!("cannot read `{path}`"), span);
    };
    let mut line = Vec::new();
    match reader.read_until(b'\n', &mut line) {
        Ok(0) => return Ok(Value::None),
        Ok(_) => {}
        Err(_) => return file_error(format!("cannot read `{path}`"), span),
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    match String::from_utf8(line) {
        Ok(text) if !text.contains('\0') => Ok(Value::str(text)),
        _ => file_error(format!("`{path}` does not hold text"), span),
    }
}

/// `files.handle_bytes(file, path, count)`: up to `count` bytes, fewer
/// at the end, none after it. `count` is never below 0: `read_bytes`
/// sees to that.
fn files_handle_bytes(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    use std::io::Read;
    arity("handle_bytes", &args, 3, 3, span)?;
    let path = text("handle_bytes", &args[1], span)?.to_string();
    let Value::Int(count) = args[2] else { return wrong_type("handle_bytes", "an int", &args[2], span) };
    let crate::value::OpenFile::Reading(reader) = handle(machine, "handle_bytes", &args, span)? else {
        return file_error(format!("cannot read `{path}`"), span);
    };
    let mut bytes = Vec::new();
    match reader.take(count.max(0) as u64).read_to_end(&mut bytes) {
        Ok(_) => Ok(Value::list(bytes.into_iter().map(|byte| Value::Int(byte as i64)).collect())),
        Err(_) => file_error(format!("cannot read `{path}`"), span),
    }
}

/// Hand `bytes` to the system before giving back, so that a full disk
/// is an error at the `write` that met it (decision 59.6).
fn handle_put(machine: &mut dyn Machine, name: &str, args: &[Value], path: &str, bytes: &[u8], span: Span) -> Result {
    use std::io::Write;
    let crate::value::OpenFile::Writing(file) = handle(machine, name, args, span)? else {
        return file_error(format!("cannot write `{path}`"), span);
    };
    match file.write_all(bytes).and_then(|()| file.flush()) {
        Ok(()) => Ok(Value::None),
        Err(_) => file_error(format!("cannot write `{path}`"), span),
    }
}

/// `files.handle_write(file, path, text)`: the text, and no line end.
fn files_handle_write(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("handle_write", &args, 3, 3, span)?;
    let path = text("handle_write", &args[1], span)?.to_string();
    let written = text("handle_write", &args[2], span)?.to_string();
    handle_put(machine, "handle_write", &args, &path, written.as_bytes(), span)
}

/// `files.handle_write_bytes(file, path, bytes)`.
fn files_handle_write_bytes(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("handle_write_bytes", &args, 3, 3, span)?;
    let path = text("handle_write_bytes", &args[1], span)?.to_string();
    let Value::List(items) = &args[2] else {
        return wrong_type("handle_write_bytes", "a list[u8]", &args[2], span);
    };
    let mut bytes = Vec::with_capacity(items.borrow().len());
    for item in items.borrow().iter() {
        match item {
            Value::Int(n) if (0..=255).contains(n) => bytes.push(*n as u8),
            Value::Int(n) => {
                return raise("ValueError", format!("{n} is not a byte: a byte is a number from 0 to 255"), span)
            }
            other => return wrong_type("handle_write_bytes", "a list[u8]", other, span),
        }
    }
    handle_put(machine, "handle_write_bytes", &args, &path, &bytes, span)
}

/// `files.handle_close(file)`: the file is closed, and its number free.
fn files_handle_close(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("handle_close", &args, 1, 1, span)?;
    handle(machine, "handle_close", &args, span)?;
    let Value::Int(number) = args[0] else { unreachable!("`handle` looked") };
    machine.outside().files.open[number as usize - 1] = None;
    Ok(Value::None)
}

/// `files.exists(path)`: is there a file there that can be read?
fn files_exists(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let path = paths("exists", &args, 1, span)?[0];
    Ok(Value::Bool(at(path) == At::File))
}

/// `files.remove(path)`: the file is gone. A folder is left alone.
fn files_remove(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let path = paths("remove", &args, 1, span)?[0];
    match at(path) {
        At::Nothing => file_error(format!("there is no file `{path}`"), span),
        At::File if std::fs::remove_file(path).is_ok() => Ok(Value::None),
        _ => file_error(format!("cannot remove `{path}`"), span),
    }
}

/// `files.list(folder)`: the names in a folder, files and folders alike,
/// sorted by the numbers of their characters, so that every engine and
/// every system gives the same list (decision 59.20). `.` and `..` are
/// not names in it.
fn files_list(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let folder = paths("list", &args, 1, span)?[0];
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return file_error(format!("there is no folder `{folder}`"), span)
        }
        Err(_) => return file_error(format!("cannot list the folder `{folder}`"), span),
    };
    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return file_error(format!("cannot list the folder `{folder}`"), span);
        };
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(_) => return file_error(format!("the folder `{folder}` holds a name that is not text"), span),
        }
    }
    // Text in Rust is UTF-8, and its bytes sort as its characters do.
    names.sort();
    Ok(Value::list(names.into_iter().map(Value::str).collect()))
}

/// `files.make_folder(path)`: the folder is there afterwards, with every
/// folder above it. One that is there already is left as it is. It
/// worked if the path is a folder at the end, whatever was in the way
/// (decision 59.21).
fn files_make_folder(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let path = paths("make_folder", &args, 1, span)?[0];
    let _ = std::fs::create_dir_all(path);
    match std::path::Path::new(path).is_dir() {
        true => Ok(Value::None),
        false => file_error(format!("cannot make the folder `{path}`"), span),
    }
}

/// The one number a math function takes.
fn one_number(name: &str, args: &[Value], span: Span) -> Result<f64> {
    arity(name, args, 1, 1, span)?;
    match args[0].number() {
        Some(x) => Ok(x),
        None => wrong_type(name, "a number", &args[0], span),
    }
}

fn sqrt(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    let x = one_number("sqrt", &args, span)?;
    if x < 0.0 {
        return raise("ValueError", format!("cannot take the square root of {}", crate::value::float_text(x)), span);
    }
    Ok(Value::Float(x.sqrt()))
}

fn floor(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Int(one_number("floor", &args, span)?.floor() as i64))
}

fn ceil(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Int(one_number("ceil", &args, span)?.ceil() as i64))
}

/// `round(2.5)` is 3 and `round(-2.5)` is -3: a half goes away from
/// zero, as it is taught at school.
fn round(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Int(one_number("round", &args, span)?.round() as i64))
}

/// The angle given to `sin`, `cos` or `tan`, in radians. A plain number
/// is radians already, as in mathematics. An angle with a unit says
/// which it is: `sin(30 deg)`.
fn angle(name: &str, args: &[Value], span: Span) -> Result<f64> {
    arity(name, args, 1, 1, span)?;
    match &args[0] {
        Value::Quantity(x, unit) => match crate::units::convert(unit, "rad") {
            Some(scale) => Ok(scale.apply(*x)),
            None => fatal(
                format!("`{name}` needs a number or an angle, such as `30 deg`, not a quantity in {unit}"),
                span,
            ),
        },
        other => match other.number() {
            Some(x) => Ok(x),
            None => wrong_type(name, "a number or an angle, such as `30 deg`", other, span),
        },
    }
}

/// An angle that a function worked out. It is in radians, and says so.
fn radians(x: f64) -> Result {
    Ok(Value::Quantity(x, Rc::from("rad")))
}

fn sin(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(angle("sin", &args, span)?.sin()))
}

fn cos(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(angle("cos", &args, span)?.cos()))
}

fn tan(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(angle("tan", &args, span)?.tan()))
}

/// The number given to `asin` or `acos`, which is the sine or cosine
/// of some angle, and so lies from -1 to 1.
fn ratio(name: &str, args: &[Value], span: Span) -> Result<f64> {
    let x = one_number(name, args, span)?;
    if x < -1.0 || x > 1.0 {
        return raise("ValueError", format!("`{name}` needs a number from -1 to 1, and this is {}", crate::value::float_text(x)), span);
    }
    Ok(x)
}

fn asin(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    radians(ratio("asin", &args, span)?.asin())
}

fn acos(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    radians(ratio("acos", &args, span)?.acos())
}

fn atan(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    radians(one_number("atan", &args, span)?.atan())
}

/// `atan2(y, x)`: the angle of the point (x, y) seen from (0, 0), all
/// the way round, which `atan(y / x)` cannot tell.
fn atan2(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("atan2", &args, 2, 2, span)?;
    match (args[0].number(), args[1].number()) {
        (Some(y), Some(x)) => radians(y.atan2(x)),
        (None, _) => wrong_type("atan2", "two numbers", &args[0], span),
        (_, None) => wrong_type("atan2", "two numbers", &args[1], span),
    }
}

fn exp(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(one_number("exp", &args, span)?.exp()))
}

/// The number given to a logarithm, which has to be above zero.
fn above_zero(name: &str, args: &[Value], span: Span) -> Result<f64> {
    let x = one_number(name, args, span)?;
    if x <= 0.0 {
        return raise("ValueError", format!("cannot take the logarithm of {}", crate::value::float_text(x)), span);
    }
    Ok(x)
}

fn log(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(above_zero("log", &args, span)?.ln()))
}

fn log2(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(above_zero("log2", &args, span)?.log2()))
}

fn log10(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    Ok(Value::Float(above_zero("log10", &args, span)?.log10()))
}

/// `sleep(500 ms)`. A time without a unit is refused, because nobody can
/// tell whether `sleep(500)` means seconds or milliseconds.
fn sleep(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("sleep", &args, 1, 1, span)?;
    let seconds = match &args[0] {
        Value::Quantity(x, unit) => match crate::units::convert(unit, "s") {
            Some(scale) => scale.apply(*x),
            None => return fatal(format!("`sleep` needs a time, such as `500 ms`, not a quantity in {unit}"), span),
        },
        other => return wrong_type("sleep", "a time with a unit, such as `500 ms`", other, span),
    };
    // Zero, a negative time and "not a number" wait for nothing. A time
    // too long to count is cut to about thirty years.
    #[cfg(not(target_arch = "wasm32"))]
    if seconds > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds.min(1e9)));
    }
    // A page cannot stop and wait. While a program is running nothing
    // on the page moves and no button answers, so a program that slept
    // would freeze the page for that long.
    #[cfg(target_arch = "wasm32")]
    if seconds > 0.0 {
        return fatal(
            "a program in a page cannot `sleep`: the page would stand still until it woke. \
             Say what is to happen later with `web.after(500 ms, ...)`",
            span,
        );
    }
    Ok(Value::None)
}

/// `now()`: the reading of a clock that only goes forward, in
/// milliseconds. Take two readings and subtract.
fn now(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("now", &args, 0, 0, span)?;
    Ok(Value::Quantity(board::now(), Rc::from("ms")))
}

/// `pin(7)`: the pin with that number.
fn pin(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("pin", &args, 1, 1, span)?;
    match &args[0] {
        Value::Int(number) if (0..board::PINS).contains(number) => Ok(Value::Pin(*number)),
        Value::Int(number) => raise(
            "ValueError",
            format!("this board has no pin {number}. Its pins are 0 to {}", board::PINS - 1),
            span,
        ),
        other => wrong_type("pin", "an int", other, span),
    }
}

/// Set a pin high or low, and print what the simulated board shows.
/// What `press.wait()` says when the board answers that no edge can
/// ever come. The runtime of a built program says the same.
fn no_edge_can_come(pin: i64) -> String {
    format!(
        "the board says no edge can ever come on pin {pin}, so the program would wait for ever. \
         On a computer nothing is wired to a pin: drive it with `pin({pin})` before the wait"
    )
}

/// `ticker(10 ms)`: a beat with that long from one tick to the next.
/// Its first tick is one period from now.
fn ticker(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("ticker", &args, 1, 1, span)?;
    let period = match &args[0] {
        Value::Quantity(time, unit) => match crate::units::convert(unit, "ms") {
            Some(scale) => scale.apply(*time),
            None => return wrong_type("ticker", "a time with a unit, such as `10 ms`", &args[0], span),
        },
        other => return wrong_type("ticker", "a time with a unit, such as `10 ms`", other, span),
    };
    if !(period > 0.0) {
        return raise("ValueError", "the period of a ticker must be above 0 s".to_string(), span);
    }
    match machine.board().ticker(period, board::now()) {
        Some(ticker) => Ok(Value::Ticker(ticker, period)),
        None => raise(
            "ValueError",
            format!(
                "this board keeps {} tickers, and this would be one more. Make each ticker once, outside any loop",
                board::TICKERS
            ),
            span,
        ),
    }
}

/// `pwm_pin(9, 1 kHz)`: the pin with that number, switched on and off
/// that many times a second. It starts off all the time.
fn pwm_pin(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("pwm_pin", &args, 2, 2, span)?;
    let Value::Pin(number) = pin(machine, vec![args[0].clone()], span)? else { unreachable!("`pin` gives a pin") };
    let hertz = match &args[1] {
        Value::Quantity(rate, unit) => match crate::units::convert(unit, "Hz") {
            Some(scale) => scale.apply(*rate),
            None => return wrong_type("pwm_pin", "a frequency, such as `1 kHz`", &args[1], span),
        },
        other => return wrong_type("pwm_pin", "a frequency, such as `1 kHz`", other, span),
    };
    if !(hertz > 0.0) {
        return raise("ValueError", "the frequency of a PWM signal must be above 0 Hz".to_string(), span);
    }
    pwm(machine, number, hertz, 0.0);
    Ok(Value::PwmPin(number))
}

/// Set a pin's PWM signal, and print the lines the simulated board
/// shows for it: the pin going low if it was driven high, and the signal
/// when it changes.
fn pwm(machine: &mut dyn Machine, pin: i64, hertz: f64, duty: f64) {
    let (low, changed) = machine.board().pwm(pin, hertz, duty);
    if let Some(line) = low {
        machine.write_line(&line);
    }
    if changed {
        let signal = format!("{} pwm {} at {}", board::name(pin), Value::Float(duty), Value::Quantity(hertz, Rc::from("Hz")));
        machine.write_line(&signal);
    }
}

/// `analog_pin(0)`: the pin with that number, for reading its voltage.
/// Every pin of the simulated board can be read this way.
fn analog_pin(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("analog_pin", &args, 1, 1, span)?;
    let Value::Pin(number) = pin(machine, vec![args[0].clone()], span)? else { unreachable!("`pin` gives a pin") };
    Ok(Value::AnalogPin(number))
}

/// `serial(1, 115200 baud)`: the serial port with that number.
fn serial(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("serial", &args, 2, 2, span)?;
    let number = match &args[0] {
        Value::Int(number) if (0..board::SERIALS).contains(number) => *number,
        Value::Int(number) => {
            return raise(
                "ValueError",
                format!("this board has no serial port {number}. Its ports are 0 to {}", board::SERIALS - 1),
                span,
            )
        }
        other => return wrong_type("serial", "an int", other, span),
    };
    match &args[1] {
        Value::Quantity(speed, unit) if &**unit == "baud" => {
            if !(*speed > 0.0) {
                return raise("ValueError", "the speed of a serial port must be above 0 baud".to_string(), span);
            }
        }
        other => return wrong_type("serial", "a speed in baud", other, span),
    }
    Ok(Value::Serial(number))
}

/// The bytes of a `list[u8]`.
fn byte_list(name: &str, list: &Value, span: Span) -> Result<Vec<u8>> {
    let Value::List(items) = list else {
        return wrong_type(name, "a list[u8]", list, span);
    };
    let mut bytes = Vec::with_capacity(items.borrow().len());
    for item in items.borrow().iter() {
        match item {
            Value::Int(n) if (0..=255).contains(n) => bytes.push(*n as u8),
            Value::Int(n) => {
                return raise("ValueError", format!("{n} is not a byte: a byte is a number from 0 to 255"), span)
            }
            other => return wrong_type(name, "a list[u8]", other, span),
        }
    }
    Ok(bytes)
}

/// `input_pin(2, pull_up)`: the pin with that number, made an input.
fn input_pin(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("input_pin", &args, 1, 2, span)?;
    let Value::Pin(number) = pin(machine, vec![args[0].clone()], span)? else { unreachable!("`pin` gives a pin") };
    let pull = match args.get(1) {
        None => board::Pull::Nothing,
        Some(Value::Str(name)) if &**name == "pull_up" => board::Pull::Up,
        Some(Value::Str(name)) if &**name == "pull_down" => board::Pull::Down,
        Some(other) => return wrong_type("input_pin", "`pull_up` or `pull_down`", other, span),
    };
    machine.board().input(number, pull);
    Ok(Value::InputPin(number))
}

fn drive(machine: &mut dyn Machine, pin: i64, level: bool) -> Result {
    if let Some(line) = machine.board().write(pin, level) {
        machine.write_line(&line);
    }
    Ok(Value::None)
}

// ------------------------------------------------------------------- The page
//
// The `web` module. Every function here asks the page, or changes it,
// through `machine.page()`. Under `zeno run` that is a simulated page,
// which answers with the line to print for a change that could be seen
// (`page.rs`). In a browser it is the real page, and nothing is printed.

// ------------------------------------------------------------------- JSON

/// `json.parse(text)`: the value that JSON text describes (decision 18.1).
///
/// ```text
/// {"name": "Ada", "age": 36}   a dict
/// [1, 2, 3]                    a list
/// "text"                       a str
/// 12     12.5                  an int, a float
/// true   false                 a bool
/// null                         none
/// ```
///
/// A member of an object that is `null` is left out of the dict, since
/// a dict holds no `none` and gives `none` for a key that is not there
/// (10.11).
fn json_parse(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("parse", &args, 1, 1, span)?;
    let written = text("parse", &args[0], span)?;
    match crate::json::Json::parse(written) {
        Some(json) => Ok(from_json(&json)),
        None => {
            // Enough of the text to know it by, and no more.
            // The blanks before it are the four that JSON has.
            let seen = written.trim_start_matches([' ', '\t', '\n', '\r']);
            let start: String = seen.chars().take(30).collect();
            let more = if seen.chars().count() > 30 { "..." } else { "" };
            raise("ValueError", format!("this text is not JSON: `{start}{more}`"), span)
        }
    }
}

fn from_json(json: &crate::json::Json) -> Value {
    use crate::json::Json;
    match json {
        Json::Null => Value::None,
        Json::Bool(value) => Value::Bool(*value),
        // JSON has one kind of number. One with nothing after the point,
        // and small enough to be exact, is an int.
        Json::Number(number) if number.fract() == 0.0 && number.abs() < 9.0e15 => Value::Int(*number as i64),
        Json::Number(number) => Value::Float(*number),
        Json::Text(text) => Value::str(text),
        Json::List(items) => Value::list(items.iter().map(from_json).collect()),
        Json::Object(members) => {
            let mut dict = crate::value::Dict::default();
            for (name, value) in members.iter().filter(|(_, value)| *value != Json::Null) {
                dict.set(crate::value::Key::Str(Rc::from(name.as_str())), from_json(value));
            }
            Value::dict(dict)
        }
    }
}

/// `json.write(value)`: the value as JSON text, on one line. It takes
/// what `json.parse` gives (dicts, lists, text, numbers, bools and
/// `none`) and objects, which are written field by field. Anything
/// else has no JSON to be, and is a `ValueError`.
fn json_write(_: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("write", &args, 1, 1, span)?;
    let mut out = String::new();
    to_json(&args[0], &mut out, span)?;
    Ok(Value::str(out))
}

fn to_json(value: &Value, out: &mut String, span: Span) -> Result<()> {
    match value {
        Value::None => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(x) if x.is_finite() => out.push_str(&Value::Float(*x).to_string()),
        Value::Float(x) => return raise("ValueError", format!("JSON has no way to write {x}"), span),
        Value::Str(text) => crate::json::write_text(text, out),
        Value::List(items) => {
            out.push('[');
            for (nth, item) in items.borrow().iter().enumerate() {
                if nth > 0 {
                    out.push(',');
                }
                to_json(item, out, span)?;
            }
            out.push(']');
        }
        Value::Dict(dict) => {
            out.push('{');
            for (nth, (key, value)) in dict.borrow().entries().iter().enumerate() {
                if nth > 0 {
                    out.push(',');
                }
                // A key of JSON is text. A whole number is written as one.
                match key {
                    Value::Str(text) => crate::json::write_text(text, out),
                    other => crate::json::write_text(&other.to_string(), out),
                }
                out.push(':');
                to_json(value, out, span)?;
            }
            out.push('}');
        }
        // An object is written as JSON calls an object: each field by
        // its name, in the order the class declares them. It is how
        // values of several types go under one roof, which a dict does
        // not allow (18.2).
        Value::Instance(instance) => {
            out.push('{');
            let fields = instance.fields.borrow();
            let mut written = 0;
            for (field, value) in instance.class.fields.iter().zip(fields.iter()) {
                if written > 0 {
                    out.push(',');
                }
                written += 1;
                crate::json::write_text(&field.name, out);
                out.push(':');
                to_json(value, out, span)?;
            }
            out.push('}');
        }
        Value::Quantity(_, unit) => {
            return raise(
                "ValueError",
                format!("JSON has no units, and this is a quantity in {unit}. Write its number, and put the unit in the name: `[\"{unit}\": ...]`"),
                span,
            )
        }
        other => {
            return raise(
                "ValueError",
                format!("JSON has objects, lists, text, numbers, bools and none, and this is {}", other.type_name()),
                span,
            )
        }
    }
    Ok(())
}

/// Print what the simulated page shows for a change, if anything.
fn shown(machine: &mut dyn Machine, line: Option<String>) -> Result {
    if let Some(line) = line {
        machine.write_line(&line);
    }
    Ok(Value::None)
}

/// Is this something the page can call: a function, with or without a
/// name, or a method of an object?
fn can_be_called(value: &Value) -> bool {
    matches!(value, Value::Function(_) | Value::BoundMethod(..) | Value::Builtin(_) | Value::Method(..))
}

/// `web.element("#count")`: the first element of the page that answers
/// to the selector. A page that has none is a mistake in the program or
/// in the page, and the program stops there and says so.
fn web_element(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("element", &args, 1, 1, span)?;
    let selector = text("element", &args[0], span)?;
    match machine.page().element(selector) {
        Some(id) => Ok(Value::Element(id, Rc::from(selector))),
        None => raise(
            "ValueError",
            format!("the page has no element that answers to `{selector}`. `web.elements` gives a list, which may be empty"),
            span,
        ),
    }
}

/// `web.elements("li")`: every element that answers to the selector.
fn web_elements(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("elements", &args, 1, 1, span)?;
    let selector = text("elements", &args[0], span)?;
    let name: Rc<str> = Rc::from(selector);
    match machine.page().elements(selector) {
        Ok(found) => Ok(Value::list(found.into_iter().map(|id| Value::Element(id, name.clone())).collect())),
        Err(why) => raise("ValueError", why, span),
    }
}

/// `web.create("li", "Buy milk")`: a new element with that text in it.
/// It is not on the page until it is appended to one that is.
fn web_create(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("create", &args, 1, 2, span)?;
    let tag = text("create", &args[0], span)?;
    let inside = match args.get(1) {
        Some(inside) => text("create", inside, span)?,
        None => "",
    };
    // A tag is a word: letters, then letters, digits and `-`.
    let word = tag.starts_with(|c: char| c.is_ascii_alphabetic()) && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !word {
        return raise("ValueError", format!("`{tag}` is not the name of a kind of element. Write one such as `li` or `button`"), span);
    }
    let id = machine.page().create(tag, inside);
    Ok(Value::Element(id, Rc::from(format!("<{tag}>"))))
}

/// `web.after(500 ms, tick)`: call the function once, that much later.
fn web_after(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    timer(machine, "after", args, span)
}

/// `web.every(1 s, tick)`: call the function that often, for as long as
/// the page is open.
fn web_every(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    timer(machine, "every", args, span)
}

/// `after` and `every`: a time with a unit, and a function to call.
fn timer(machine: &mut dyn Machine, name: &str, mut args: Vec<Value>, span: Span) -> Result {
    arity(name, &args, 2, 2, span)?;
    let milliseconds = match &args[0] {
        Value::Quantity(x, unit) => match crate::units::convert(unit, "ms") {
            Some(scale) => scale.apply(*x),
            None => return fatal(format!("`{name}` needs a time, such as `500 ms`, not a quantity in {unit}"), span),
        },
        other => return wrong_type(name, "a time with a unit, such as `500 ms`", other, span),
    };
    // A page that calls a function every 0 ms does nothing else.
    if name == "every" && milliseconds < 1.0 {
        return raise("ValueError", "`every` needs a time of at least 1 ms", span);
    }
    let handler = args.remove(1);
    if !can_be_called(&handler) {
        return wrong_type(name, "a function to call", &handler, span);
    }
    Ok(Value::Timer(machine.page().after(milliseconds, handler, name == "every")))
}

/// `web.save("name", "Partha")`: keep text under a name. It is there
/// the next time the page is opened in the same browser.
fn web_save(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("save", &args, 2, 2, span)?;
    let (name, value) = (text("save", &args[0], span)?, text("save", &args[1], span)?);
    machine.page().save(name, value);
    Ok(Value::None)
}

/// `web.load("name")`: the text saved under the name, or `none`.
fn web_load(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("load", &args, 1, 1, span)?;
    let name = text("load", &args[0], span)?;
    Ok(machine.page().load(name).map_or(Value::None, Value::str))
}

/// `web.forget("name")`: nothing is saved under the name any more.
fn web_forget(machine: &mut dyn Machine, args: Vec<Value>, span: Span) -> Result {
    arity("forget", &args, 1, 1, span)?;
    let name = text("forget", &args[0], span)?;
    machine.page().forget(name);
    Ok(Value::None)
}

/// `web.fetch("/prices.txt", done)`: ask the network for what is at
/// that address, and call `done(status, body)` when the answer comes.
fn web_fetch(machine: &mut dyn Machine, mut args: Vec<Value>, span: Span) -> Result {
    arity("fetch", &args, 2, 2, span)?;
    let handler = args.remove(1);
    let address = text("fetch", &args[0], span)?;
    if !can_be_called(&handler) {
        return wrong_type("fetch", "a function to call", &handler, span);
    }
    machine.page().fetch(address, None, handler);
    Ok(Value::None)
}

/// `web.post("/orders", body, done)`: send `body` to that address, and
/// call `done(status, body)` with what comes back.
fn web_post(machine: &mut dyn Machine, mut args: Vec<Value>, span: Span) -> Result {
    arity("post", &args, 3, 3, span)?;
    let handler = args.remove(2);
    let (address, body) = (text("post", &args[0], span)?, text("post", &args[1], span)?);
    if !can_be_called(&handler) {
        return wrong_type("post", "a function to call", &handler, span);
    }
    machine.page().fetch(address, Some(body), handler);
    Ok(Value::None)
}

/// Does this function ask for the `Event`? One that takes a parameter
/// does: `function(event: Event) { ... }`. The checker has seen to it
/// that it takes nothing else.
fn takes_event(handler: &Value) -> bool {
    match handler {
        Value::Function(closure) => !closure.decl.params.is_empty(),
        // The first parameter of a method is `self`.
        Value::BoundMethod(_, closure) => closure.decl.params.len() > 1,
        _ => false,
    }
}

/// A number given to one of the `draw_` methods: an int or a float.
fn place(method: &str, arg: &Value, span: Span) -> Result<f64> {
    match arg {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => wrong_type(method, "a number", other, span),
    }
}

/// `web.on_key(pressed)`: call the function with the name of each key
/// that is pressed, as a str: `"a"`, `"Enter"`, `"ArrowLeft"`.
fn web_on_key(machine: &mut dyn Machine, mut args: Vec<Value>, span: Span) -> Result {
    arity("on_key", &args, 1, 1, span)?;
    let handler = args.remove(0);
    if !can_be_called(&handler) {
        return wrong_type("on_key", "a function to call", &handler, span);
    }
    machine.page().on_key(handler);
    Ok(Value::None)
}

/// `title.text`, `name.value`: what an element shows, and what has been
/// typed into one. `None` for anything that is not asked of the page.
pub fn on_page(machine: &mut dyn Machine, object: &Value, name: &str) -> Option<Value> {
    match object {
        Value::Element(id, _) if ELEMENT_FIELDS.contains(&name) => Some(Value::str(machine.page().read(*id, name))),
        _ => None,
    }
}

/// `title.text = "Hello"`, `name.value = ""`. `None` for anything that
/// is not a change to the page.
pub fn set_on_page(machine: &mut dyn Machine, object: &Value, name: &str, value: &Value, span: Span) -> Option<Result<()>> {
    let Value::Element(id, _) = object else { return None };
    if !ELEMENT_FIELDS.contains(&name) {
        return Some(fatal(format!("an element has no `{name}` to give a new value. It has `text` and `value`"), span));
    }
    Some(match value {
        Value::Str(to) => {
            let line = machine.page().write(*id, name, to);
            shown(machine, line).map(|_| ())
        }
        other => fatal(format!("the `{name}` of an element is a str, but this is {}", other.type_name()), span),
    })
}

/// A method of an element: `button.on("click", pressed)`.
fn element_method(machine: &mut dyn Machine, id: u32, name: &str, mut args: Vec<Value>, span: Span) -> Result {
    match name {
        "on" => {
            arity(name, &args, 2, 2, span)?;
            let handler = args.remove(1);
            let event = text(name, &args[0], span)?;
            if !can_be_called(&handler) {
                return wrong_type(name, "a function to call", &handler, span);
            }
            let takes_event = takes_event(&handler);
            machine.page().on(id, event, handler, takes_event);
            Ok(Value::None)
        }
        // `button.fire("click")`: call what `on` was given for that
        // event, now, in the order given. It is how a test presses a
        // button (decision 16.3).
        "fire" => {
            arity(name, &args, 1, 1, span)?;
            let kind = text(name, &args[0], span)?;
            let target = Value::Element(id, Rc::from(machine.page().name(id)));
            let event = Value::Event(Rc::new(crate::value::Event {
                kind: Rc::from(kind),
                target,
                x: 0.0,
                y: 0.0,
                key: Rc::from(""),
            }));
            for handler in machine.page().listeners(id, kind) {
                let arguments = if handler.takes_event { vec![event.clone()] } else { Vec::new() };
                machine.call(handler.function, arguments, Vec::new(), span)?;
            }
            Ok(Value::None)
        }
        "get_style" => {
            arity(name, &args, 1, 1, span)?;
            let style = text(name, &args[0], span)?;
            Ok(machine.page().style(id, style).map_or(Value::None, Value::str))
        }
        "set_style" => {
            arity(name, &args, 2, 2, span)?;
            let (style, value) = (text(name, &args[0], span)?, text(name, &args[1], span)?);
            match machine.page().set_style(id, style, value) {
                Ok(line) => shown(machine, line),
                Err(()) => raise(
                    "ValueError",
                    format!("there is no style called `{style}`. A style is named as CSS names it: `color`, `background-color`, `font-size`"),
                    span,
                ),
            }
        }
        "draw_rect" | "draw_circle" | "draw_line" | "draw_text" | "erase" => {
            // How many numbers each takes, and whether text follows them.
            let (what, numbers, has_text) = match name {
                "draw_rect" => ("rect", 4, false),
                "draw_circle" => ("circle", 3, false),
                "draw_line" => ("line", 4, false),
                "draw_text" => ("text", 2, true),
                _ => ("erase", 0, false),
            };
            // The colour comes last, for all but `erase`.
            let words = if what == "erase" { 0 } else { 1 + has_text as usize };
            arity(name, &args, numbers + words, numbers + words, span)?;
            let places = args[..numbers].iter().map(|arg| place(name, arg, span)).collect::<Result<Vec<f64>>>()?;
            let words = args[numbers..].iter().map(|arg| text(name, arg, span)).collect::<Result<Vec<&str>>>()?;
            match machine.page().draw(id, what, &places, &words) {
                Ok(line) => shown(machine, line),
                Err(()) => raise("ValueError", format!("`{name}` draws on a `<canvas>`, and this element is not one"), span),
            }
        }
        "append" => {
            arity(name, &args, 1, 1, span)?;
            let Value::Element(child, _) = &args[0] else {
                return wrong_type(name, "an element", &args[0], span);
            };
            if machine.page().is_inside(id, *child) {
                return raise("ValueError", "an element cannot be put inside itself, or inside something it holds", span);
            }
            let line = machine.page().append(id, *child);
            shown(machine, line)
        }
        "remove" => {
            arity(name, &args, 0, 0, span)?;
            let line = machine.page().remove(id);
            shown(machine, line)
        }
        "clear" => {
            arity(name, &args, 0, 0, span)?;
            let line = machine.page().clear(id);
            shown(machine, line)
        }
        "get" => {
            arity(name, &args, 1, 1, span)?;
            let attribute = text(name, &args[0], span)?;
            Ok(machine.page().attribute(id, attribute).map_or(Value::None, Value::str))
        }
        "set" => {
            arity(name, &args, 2, 2, span)?;
            let (attribute, value) = (text(name, &args[0], span)?, text(name, &args[1], span)?);
            let line = machine.page().set_attribute(id, attribute, value);
            shown(machine, line)
        }
        "add_class" | "remove_class" => {
            arity(name, &args, 1, 1, span)?;
            let class = text(name, &args[0], span)?;
            let line = machine.page().class(id, class, name == "add_class");
            shown(machine, line)
        }
        "has_class" => {
            arity(name, &args, 1, 1, span)?;
            let class = text(name, &args[0], span)?;
            Ok(Value::Bool(machine.page().has_class(id, class)))
        }
        "show" | "hide" => {
            arity(name, &args, 0, 0, span)?;
            let line = machine.page().hide(id, name == "hide");
            shown(machine, line)
        }
        _ => fatal(format!("an element has no method `{name}`"), span),
    }
}

// ----------------------------------------------------------------- Formatting

/// Turn a value into text for an f-string. `spec` is what follows the `:` in
/// `{value:spec}`, and follows Python's layout:
///
/// ```text
/// [align][0][width][.precision][type]
///
/// align      <  left    >  right    ^  centre
/// 0          pad numbers with zeros instead of spaces
/// width      the least number of characters to fill
/// precision  digits after the decimal point
/// type       f  fixed decimals    d  int    x X  hex    b  binary
/// ```
///
/// So `{x:.2f}` is two decimals, `{n:5}` is width 5, `{reg:08b}` is a byte in binary.
pub fn format(value: &Value, spec: Option<&str>, span: Span) -> Result<String> {
    let Some(spec) = spec else {
        return Ok(value.to_string());
    };
    let bad = || fatal(format!("`{spec}` is not a format Zeno understands"), span);
    let mut rest = spec.chars().peekable();

    let align = rest.next_if(|c| matches!(c, '<' | '>' | '^'));
    let zero = rest.next_if_eq(&'0').is_some();
    let mut width = 0;
    while let Some(digit) = rest.peek().and_then(|c| c.to_digit(10)) {
        width = width * 10 + digit as usize;
        rest.next();
    }
    let mut precision = None;
    if rest.next_if_eq(&'.').is_some() {
        let mut digits = 0;
        while let Some(digit) = rest.peek().and_then(|c| c.to_digit(10)) {
            digits = digits * 10 + digit as usize;
            rest.next();
        }
        precision = Some(digits);
    }
    let kind = rest.next();
    if rest.next().is_some() {
        return bad();
    }

    // A float type or a precision asks for fixed decimals.
    let body = match (kind, precision, value) {
        // `{angle:.1f}` is `30.5 deg`: the decimals are the number's,
        // and the unit follows it as it always does.
        (Some('f'), _, Value::Quantity(x, unit)) | (None, Some(_), Value::Quantity(x, unit)) => {
            format!("{x:.*} {unit}", precision.unwrap_or(6))
        }
        (Some('f'), _, _) | (None, Some(_), Value::Float(_)) => match value.number() {
            Some(x) => format!("{x:.*}", precision.unwrap_or(6)),
            None => return wrong_type(spec, "a number", value, span),
        },
        (None, None, _) | (Some('d'), None, Value::Int(_) | Value::U64(_)) => value.to_string(),
        (Some(kind @ ('x' | 'X' | 'b')), None, Value::U64(n)) => match kind {
            'x' => format!("{n:x}"),
            'X' => format!("{n:X}"),
            _ => format!("{n:b}"),
        },
        (Some(kind @ ('x' | 'X' | 'b')), None, Value::Int(n)) => {
            let sign = if *n < 0 { "-" } else { "" };
            let digits = match kind {
                'x' => format!("{:x}", n.unsigned_abs()),
                'X' => format!("{:X}", n.unsigned_abs()),
                _ => format!("{:b}", n.unsigned_abs()),
            };
            format!("{sign}{digits}")
        }
        (Some('d' | 'x' | 'X' | 'b'), None, other) => {
            return wrong_type(spec, "an int", other, span)
        }
        _ => return bad(),
    };

    let padding = width.saturating_sub(body.chars().count());
    // A quantity is a number with its unit, and lines up as numbers do.
    let is_number = value.number().is_some() || matches!(value, Value::Quantity(..));
    Ok(if zero && is_number {
        // Zeros go between the sign and the digits: -0042.
        let (sign, digits) = match body.strip_prefix('-') {
            Some(digits) => ("-", digits),
            None => ("", body.as_str()),
        };
        format!("{sign}{}{digits}", "0".repeat(padding))
    } else {
        // Numbers line up on the right by default, everything else on the left.
        match align.unwrap_or(if is_number { '>' } else { '<' }) {
            '<' => format!("{body}{}", " ".repeat(padding)),
            '>' => format!("{}{body}", " ".repeat(padding)),
            _ => {
                let left = padding / 2;
                format!("{}{body}{}", " ".repeat(left), " ".repeat(padding - left))
            }
        }
    })
}
