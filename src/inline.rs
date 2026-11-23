//! `inline c { }`, `inline python { }`, `inline node { }`: code of
//! another language, written in a `.zeno` file as that language writes
//! it (docs/inline.md, decisions chapter 61).
//!
//! ```zeno
//! import c
//!
//! let bits = inline c {
//!     int twice(int x) { return 2 * x; }
//! }
//! print(bits.twice(21))
//! ```
//!
//! A block gives one value. For C it is a library, as `c.library`
//! gives, whose functions are the ones the block writes out. For Python
//! and JavaScript it is a module, as `python.module` and `node.module`
//! give. Nothing new runs a block: each goes through the bridge its
//! language already has (`bridge_c.rs`, `bridge.rs`), which is why a
//! block needs that language's `import`.
//!
//! The lexer reads a block (`Lexer::inline_block`), the parser makes it
//! the value of a `let`, the checker types it (`Checker::inline`), and
//! this file is what `zeno run` does when it gets there. `zeno build`
//! writes a block of C into the program's C (`emit_c::c_libraries`).

use std::sync::Mutex;

use crate::bridge::{self, Lang};
use crate::error::Error;
use crate::token::Span;
use crate::value::*;

/// The languages a block can be written in: the word after `inline`,
/// which is also the module the file imports, and what the language is
/// called in a message.
pub const LANGUAGES: &[(&str, &str)] = &[("c", "C"), ("python", "Python"), ("node", "JavaScript")];

/// What to say of a word after `inline` that is no language of a block.
pub fn unknown(language: &str) -> String {
    let why = match language {
        "asm" | "assembly" | "s" => {
            " Assembly is written inside an `inline c` block, as the C compiler takes it (`__asm__`): \
             the C function around it is what says what the routine takes and gives"
        }
        "cpp" | "cxx" | "cc" => {
            " C++ has no block: a built program is one file of C. Functions of C++ marked `extern \"C\"`, \
             built into a library, are used with `c.library`"
        }
        "js" | "javascript" | "ts" | "typescript" => " JavaScript is `inline node`",
        "py" | "python3" => " Python is `inline python`",
        "rust" | "go" | "java" | "csharp" | "ruby" | "php" | "lua" => {
            " The libraries of this language are used through its `import`, or through `import c`"
        }
        _ => "",
    };
    format!("there is no `inline {language}`. A block is `inline c`, `inline python` or `inline node`.{why}")
}

/// The files of the program that was last read, by their numbers
/// (`Span::file`): what a block is told its file is. A program is read
/// on one thread and run on another, so this is not kept by thread.
static FILES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// File `number` of the program is at `path` (`modules::Loader::file`).
pub fn file_is(number: usize, path: &str) {
    if let Ok(mut files) = FILES.lock() {
        files.truncate(number);
        files.push(path.to_string());
    }
}

/// The file a place is in, and its line there.
pub fn place(span: Span) -> (String, usize) {
    let file = FILES.lock().ok().and_then(|files| files.get(span.file()).cloned()).unwrap_or_else(|| "program.zeno".to_string());
    (file, span.line % crate::token::LINES_A_FILE)
}

/// The block of C at `span`, for the C bridge: its text, and where its
/// first line is.
pub fn c_block(text: &str, span: Span) -> crate::bridge_c::Block {
    let (file, line) = place(span);
    crate::bridge_c::Block { text: text.to_string(), file, line: line + 1 }
}

/// What a block gives, when `zeno run` gets to it.
///
/// A block that cannot be started is a mistake in the program, as a
/// misspelt word of Zeno is, and not something for a `try`: the program
/// stops, with what the other language said (decision 61.6).
#[cold]
#[inline(never)]
pub fn run(language: &str, text: &str, span: Span) -> Result<Value, Control> {
    let started = match language {
        "c" => crate::bridge_c::block(&c_block(text, span), span),
        "python" => python(text, span),
        "node" => node(text, span),
        _ => return fatal(unknown(language), span),
    };
    match started {
        Err(Control::Raise(Value::Error(error), _)) => {
            let said = match language {
                // What C says already begins with what could not be done.
                "c" => error.message.clone(),
                _ => {
                    let name = LANGUAGES.iter().find(|(word, _)| *word == language).map_or(language, |(_, name)| name);
                    // An error of the other language is an `Error` whose
                    // message begins with the name it has there.
                    let kind = if error.kind == "Error" { String::new() } else { format!("{}: ", error.kind) };
                    format!("this `inline {language}` block could not be started. {name} said: {kind}{}", error.message)
                }
            };
            Err(Control::Fatal(Error::new(said, span)))
        }
        other => other,
    }
}

/// A value of the other language, to ask more of.
fn foreign(value: &Value, span: Span) -> Result<std::rc::Rc<bridge::Foreign>, Control> {
    match value {
        Value::Foreign(foreign) => Ok(foreign.clone()),
        other => fatal(format!("an `inline` block was given {}, where a value of the other language was expected", other.type_name()), span),
    }
}

/// The file a block is in, with its whole path, for the other
/// language's own messages.
fn path_of(file: &str) -> String {
    let path = std::path::Path::new(file);
    let whole = std::fs::canonicalize(path).map(crate::bridge_c::plain).unwrap_or_else(|_| path.to_path_buf());
    whole.to_string_lossy().into_owned()
}

/// `inline python { }`: a module of Python made from the block's text.
/// It is compiled as part of the `.zeno` file, at the lines it is
/// written on, so that an error of Python names that file and line.
fn python(text: &str, span: Span) -> Result<Value, Control> {
    let (file, line) = place(span);
    let path = path_of(&file);
    let builtins = foreign(&bridge::module(Lang::Python, "builtins", span)?, span)?;
    let types = foreign(&bridge::module(Lang::Python, "types", span)?, span)?;
    let make = foreign(&bridge::field(&types, "ModuleType", span)?, span)?;
    let module = bridge::call(&make, &[Value::str(format!("inline_python_{line}"))], &[], span)?;
    let made = foreign(&module, span)?;
    bridge::set_field(&made, "__file__", &Value::str(&path), span)?;
    let compile = foreign(&bridge::field(&builtins, "compile", span)?, span)?;
    let source = format!("{}{text}", "\n".repeat(line));
    let code = bridge::call(&compile, &[Value::str(source), Value::str(&path), Value::str("exec")], &[], span)?;
    let exec = foreign(&bridge::field(&builtins, "exec", span)?, span)?;
    let names = bridge::field(&made, "__dict__", span)?;
    bridge::call(&exec, &[code, names], &[], span)?;
    Ok(module)
}

/// `inline node { }`: a module of JavaScript made from the block's
/// text, as Node makes one from a file: what the block puts in
/// `exports` (or `module.exports`) is what the block gives, and
/// `require` finds packages from the program's folder.
fn node(text: &str, span: Span) -> Result<Value, Control> {
    let (file, line) = place(span);
    let path = path_of(&file);
    let folder = std::path::Path::new(&path).parent().map(|folder| folder.to_string_lossy().into_owned()).unwrap_or_else(|| ".".to_string());
    let class = foreign(&bridge::module(Lang::Node, "module", span)?, span)?;
    let module = foreign(&bridge::new(&class, &[Value::str(&path)], span)?, span)?;
    bridge::set_field(&module, "filename", &Value::str(&path), span)?;
    let paths = foreign(&bridge::field(&class, "_nodeModulePaths", span)?, span)?;
    let found = bridge::call(&paths, &[Value::str(&folder)], &[], span)?;
    bridge::set_field(&module, "paths", &found, span)?;
    let compile = foreign(&bridge::field(&module, "_compile", span)?, span)?;
    let source = format!("{}{text}", "\n".repeat(line));
    bridge::call(&compile, &[Value::str(source), Value::str(&path)], &[], span)?;
    bridge::field(&module, "exports", span)
}
