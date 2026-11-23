//! `import c`: a library of C, or of Go or Rust built with a C
//! interface, used from `zeno run` (docs/c.md, decisions chapter 44).
//!
//! A program says which functions it uses, as C declares them:
//!
//! ```zeno
//! let z = c.library("z", "unsigned long crc32(unsigned long crc, const unsigned char *buf, unsigned int len);")
//! ```
//!
//! Zeno reads those declarations, and writes a small library of C of
//! its own, the stubs: each declared function wrapped in one that takes
//! its arguments as an array of slots, and a table of what each slot
//! holds. The C compiler builds the stubs with the library they call,
//! and the C helper (`bridge.c`), a program beside Zeno as Python is
//! for `import python`, loads them. So the C compiler, which knows how
//! each machine passes an `unsigned long` or a `float`, writes every
//! call, and Zeno only says what was declared.
//!
//! What is built is kept in the folder this machine has for what can be
//! made again (`registry::cache`), under a number worked out from what
//! it was built from, and the next run uses it as it is.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::token::Span;
use crate::value::*;

type Result<T = Value> = std::result::Result<T, Control>;

const HELPER: &str = include_str!("bridge.c");

// ----------------------------------------------------------- Declarations

/// A type as a declaration wrote it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Type {
    /// As C writes it: `const unsigned char *`.
    pub(crate) text: String,
    /// What is left with no `*` and no `const`: `unsigned char`.
    pub(crate) base: String,
    /// How many `*`.
    pub(crate) stars: usize,
    /// Whether what it points to is `const`.
    pub(crate) konst: bool,
}

/// One function, as it was declared.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declared {
    pub(crate) name: String,
    /// A value and not a function: `int LIMIT;`, a constant or a
    /// variable that a header names (decision 44.11).
    pub(crate) value: bool,
    /// The declaration, with its spaces made single: how `print` shows
    /// the function.
    pub(crate) text: String,
    pub(crate) gives: Type,
    pub(crate) takes: Vec<Type>,
}

/// The words of C that are a type or part of one, and so are never the
/// name of a parameter.
const TYPE_WORDS: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "_Bool", "bool", "const", "volatile",
    "restrict", "struct", "enum", "union", "__restrict", "__restrict__",
];

/// Comments taken out: `/* ... */` and `// ...`.
fn without_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |end| &after[end + 2..]);
            out.push(' ');
        } else if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |end| &after[end..]);
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The words and stars of a type, in order: `const char *` is
/// `["const", "char", "*"]`.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        if c == '*' {
            out.push("*".to_string());
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// A type from its tokens.
fn type_of(tokens: &[String]) -> Type {
    let stars = tokens.iter().filter(|t| *t == "*").count();
    let last_star = tokens.iter().rposition(|t| t == "*");
    // `const char *` and `char const *` point to what is constant;
    // `char *const` is a pointer that is.
    let konst = match last_star {
        Some(last) => {
            let from = tokens[..last].iter().rposition(|t| t == "*").map_or(0, |at| at + 1);
            tokens[from..last].iter().any(|t| t == "const")
        }
        None => false,
    };
    let first_star = tokens.iter().position(|t| t == "*").unwrap_or(tokens.len());
    let base: Vec<&str> = tokens[..first_star]
        .iter()
        .map(String::as_str)
        .filter(|t| !matches!(*t, "const" | "volatile" | "restrict" | "register" | "__restrict" | "__restrict__"))
        .collect();
    let mut text = String::new();
    for token in tokens {
        if token == "*" {
            if !text.ends_with('*') {
                text.push(' ');
            }
            text.push('*');
        } else {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(token);
        }
    }
    Type { text, base: base.join(" "), stars, konst }
}

/// Is this word a name a program could give, and not a word of a type?
fn is_name(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && !TYPE_WORDS.contains(&word)
}

/// One parameter: its type, with its name taken off.
fn parameter(text: &str, function: &str) -> std::result::Result<Type, String> {
    let mut text = text.trim().to_string();
    if text.contains('(') {
        return Err(format!(
            "`{function}` takes a function (`{text}`), and a function of Zeno cannot be given to C yet"
        ));
    }
    // `int values[]` and `int values[8]` are pointers.
    let mut stars = 0;
    while let Some(open) = text.find('[') {
        let close = text[open..].find(']').map_or(text.len(), |at| open + at + 1);
        text.replace_range(open..close, "");
        stars += 1;
    }
    let mut tokens = tokens(&text);
    let words: Vec<usize> = (0..tokens.len()).filter(|&at| tokens[at] != "*").collect();
    if let [.., before, last] = words[..] {
        let follows_tag = matches!(tokens[before].as_str(), "struct" | "enum" | "union");
        if is_name(&tokens[last]) && !follows_tag && last == tokens.len() - 1 {
            tokens.pop();
        }
    }
    for _ in 0..stars {
        tokens.push("*".to_string());
    }
    if tokens.is_empty() {
        return Err(format!("a parameter of `{function}` has no type"));
    }
    Ok(type_of(&tokens))
}

/// One declaration, with no `;`.
fn declaration(text: &str) -> std::result::Result<Declared, String> {
    let shown = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.contains('{') {
        return Err(format!("`{shown}` has a body. `c.library` takes declarations, each ending with `;`"));
    }
    let Some(open) = text.find('(') else {
        return value(text, &shown);
    };
    let mut depth = 0;
    let mut close = None;
    for (at, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + at);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return Err(format!("`{shown}` has a `(` that is never closed")) };
    let head = tokens(&text[..open]);
    let Some(name) = head.last().filter(|word| is_name(word)).cloned() else {
        return Err(format!("`{shown}` has no name before its `(`"));
    };
    let mut gives: Vec<String> = head[..head.len() - 1].to_vec();
    gives.retain(|word| !matches!(word.as_str(), "extern" | "static" | "inline" | "__inline" | "__inline__"));
    if gives.is_empty() {
        return Err(format!("`{shown}` says nothing of what `{name}` gives. A function that gives nothing gives `void`"));
    }
    let inside = text[open + 1..close].trim();
    let mut takes = Vec::new();
    if !inside.is_empty() && inside != "void" {
        let mut depth = 0;
        let mut from = 0;
        let mut parts = Vec::new();
        for (at, c) in inside.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&inside[from..at]);
                    from = at + 1;
                }
                _ => {}
            }
        }
        parts.push(&inside[from..]);
        for part in parts {
            if part.trim() == "..." {
                return Err(format!(
                    "`{name}` takes any number of arguments (`...`), and Zeno cannot know their types. Declare it with the ones the program gives, under another name of C, or write a function of C that calls it"
                ));
            }
            takes.push(parameter(part, &name)?);
        }
    }
    let gives = type_of(&gives);
    for of in std::iter::once(&gives).chain(&takes) {
        if of.stars == 0 && (of.base.starts_with("struct ") || of.base.starts_with("union ")) {
            return Err(format!(
                "`{name}` takes or gives a whole `{}`, not a pointer to one, and Zeno cannot hand C a struct yet. Write a function of C that takes a pointer to it",
                of.base
            ));
        }
    }
    Ok(Declared { name, value: false, text: shown, gives, takes })
}

/// Is this statement a type for the declarations that follow, and not a
/// function: `typedef struct counter counter`, `struct point { int x; }`,
/// `enum mode { OFF, ON }`? It is given to C as it is.
fn is_a_type(statement: &str) -> bool {
    let words = tokens(statement);
    match words.first().map(String::as_str) {
        Some("typedef") => true,
        Some("struct" | "union" | "enum") => match (statement.find('{'), statement.find('(')) {
            (_, None) => true,
            (Some(brace), Some(open)) => brace < open,
            (None, Some(_)) => false,
        },
        _ => false,
    }
}

/// A value the declarations name, with no `(`: `int LIMIT;`,
/// `GPIO_TypeDef *GPIOA;`. It may be a variable of the library, a
/// constant, or a macro of a header that is included: C reads it by its
/// name in each case (decision 44.11).
fn value(text: &str, shown: &str) -> std::result::Result<Declared, String> {
    let mut words = tokens(text);
    words.retain(|word| !matches!(word.as_str(), "extern" | "static"));
    let Some(name) = words.last().filter(|word| is_name(word)).cloned() else {
        return Err(format!("`{shown}` is neither a function, as in `int abs(int n);`, nor a value, as in `int LIMIT;`"));
    };
    let of = &words[..words.len() - 1];
    if of.is_empty() || text.contains('=') || text.contains('[') {
        return Err(format!(
            "`{shown}` is neither a function, as in `int abs(int n);`, nor a value, as in `int LIMIT;`, with its type and its name and nothing more"
        ));
    }
    let gives = type_of(of);
    if gives.stars == 0 && (gives.base.starts_with("struct ") || gives.base.starts_with("union ") || gives.base == "void") {
        return Err(format!(
            "`{name}` is a whole `{}`, and Zeno cannot hold a struct yet. Write a function of C that gives a pointer to it",
            gives.base
        ));
    }
    Ok(Declared { name, value: true, text: shown.to_string(), gives, takes: Vec::new() })
}

/// All that a program gave `c.library`: the lines that begin with `#`
/// and the types, as they are, and each function.
pub(crate) fn declarations(text: &str) -> std::result::Result<(Vec<String>, Vec<Declared>), String> {
    let text = without_comments(text);
    let mut lines = Vec::new();
    let mut rest = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            lines.push(line.trim().to_string());
        } else {
            rest.push_str(line);
            rest.push('\n');
        }
    }
    let mut found: Vec<Declared> = Vec::new();
    let mut depth = 0;
    let mut from = 0;
    for (at, c) in rest.char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' => depth -= 1,
            // A function with its body ends at its `}`, and is refused
            // for it; a struct ends at the `;` after its `}`.
            '}' => {
                depth -= 1;
                let one = rest[from..=at].trim();
                if depth == 0 && !is_a_type(one) {
                    declaration(one)?;
                }
            }
            ';' if depth == 0 => {
                let one = rest[from..at].trim();
                from = at + 1;
                if one.is_empty() {
                    continue;
                }
                if is_a_type(one) {
                    lines.push(format!("{};", one.split_whitespace().collect::<Vec<_>>().join(" ")));
                    continue;
                }
                let declared = declaration(one)?;
                if found.iter().any(|other| other.name == declared.name) {
                    return Err(format!("`{}` is declared twice", declared.name));
                }
                found.push(declared);
            }
            _ => {}
        }
    }
    let left = rest[from..].trim();
    if !left.is_empty() {
        let shown = left.split_whitespace().collect::<Vec<_>>().join(" ");
        return Err(format!("`{shown}` has no `;` after it. Each declaration ends with one"));
    }
    if found.is_empty() {
        return Err("`c.library` was given no declarations. It takes the functions the program uses, as C declares them: \"int abs(int n);\"".to_string());
    }
    Ok((lines, found))
}

// ----------------------------------------------------- What a type is

/// What a type of C is to Zeno, worked out from its words alone, so that
/// the checker knows it before anything is built (decision 44.10).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Class {
    /// `void`, given back: `none`.
    Nothing,
    /// A whole number: an `int` of Zeno, checked to fit when it is given.
    Whole,
    /// `uint64_t` and `unsigned long long`: a `u64` of Zeno.
    U64,
    /// `float` and `double`.
    Float,
    /// `bool` and `_Bool`.
    Bool,
    /// A pointer to `char`: text.
    Text { konst: bool },
    /// Any other pointer. `to` is what it points to, with no `const`:
    /// `int`, `struct tm`, `counter`, or `void`, and `item` what an item
    /// there is, when a list can be lent for it.
    Pointer { to: String, konst: bool, item: Option<Box<Class>> },
}

/// The names of floats that headers often give: CMSIS has `float32_t`.
const FLOAT_NAMES: &[&str] = &["float32_t", "float64_t", "float_t", "double_t"];

/// The names of whole numbers of 64 bits with no sign.
const U64_NAMES: &[&str] = &["uint64_t", "uint_least64_t", "uint_fast64_t"];

/// What each name a `typedef` of the declarations gives stands for: the
/// type it names, or `None` for a struct, a union or a function.
pub(crate) fn typedefs(lines: &[String]) -> Vec<(String, Option<Type>)> {
    let mut out = Vec::new();
    for line in lines {
        let words = tokens(line.trim_end_matches(';'));
        if words.first().map(String::as_str) != Some("typedef") {
            continue;
        }
        // `typedef void (*handler)(int);`: the name is the one after `*`.
        if line.contains('(') {
            if let Some(name) = line.split('(').nth(1).map(|part| part.trim_start_matches(|c: char| c == '*' || c.is_whitespace())) {
                let name: String = name.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                out.push((name, None));
            }
            continue;
        }
        let Some(name) = words.last().cloned() else { continue };
        let named = &words[1..words.len() - 1];
        let opaque = line.contains('{') || matches!(named.first().map(String::as_str), Some("struct" | "union"));
        let enumeration = named.first().map(String::as_str) == Some("enum");
        let of = if enumeration {
            Some(type_of(&["int".to_string()]))
        } else if opaque {
            None
        } else {
            Some(type_of(named))
        };
        out.push((name, of));
    }
    out
}

/// What a type of the declarations is to Zeno, or why Zeno cannot take
/// it. A name that only a header gives is a whole number (nearly every
/// one is: `uLong`, `HAL_StatusTypeDef`), unless it is a float's name
/// headers often use. The C compiler sees the real type, and says so if
/// the two disagree.
pub(crate) fn class(of: &Type, typedefs: &[(String, Option<Type>)]) -> std::result::Result<Class, String> {
    class_in(of, typedefs, 0)
}

fn class_in(of: &Type, typedefs: &[(String, Option<Type>)], depth: usize) -> std::result::Result<Class, String> {
    let words: Vec<&str> = of.base.split_whitespace().filter(|w| !matches!(*w, "extern" | "static" | "inline")).collect();
    let base = words.join(" ");
    // A name a typedef gives is what it stands for.
    if depth < 16 {
        if let Some((_, named)) = typedefs.iter().find(|(name, _)| *name == base) {
            return match named {
                Some(named) => {
                    let mut whole = named.clone();
                    whole.stars += of.stars;
                    whole.konst = if of.stars > 0 { of.konst } else { named.konst };
                    class_in(&whole, typedefs, depth + 1)
                }
                None if of.stars > 0 => Ok(Class::Pointer { to: base, konst: of.konst, item: None }),
                None => Err(format!("a whole `{base}`, not a pointer to one, and Zeno cannot hand C a struct or a function yet")),
            };
        }
    }
    let plain = |_: usize| -> std::result::Result<Class, String> {
        if base == "void" {
            return Ok(Class::Nothing);
        }
        if matches!(base.as_str(), "bool" | "_Bool") {
            return Ok(Class::Bool);
        }
        if matches!(base.as_str(), "float" | "double" | "long double") || FLOAT_NAMES.contains(&base.as_str()) {
            return Ok(Class::Float);
        }
        if U64_NAMES.contains(&base.as_str())
            || (words.contains(&"unsigned") && words.iter().filter(|w| **w == "long").count() == 2)
        {
            return Ok(Class::U64);
        }
        if words.first().is_some_and(|w| matches!(*w, "struct" | "union")) {
            return Err(format!("a whole `{base}`, not a pointer to one, and Zeno cannot hand C a struct yet"));
        }
        Ok(Class::Whole)
    };
    if of.stars == 0 {
        return plain(0);
    }
    let to = if base.is_empty() { "void".to_string() } else { base.clone() };
    if of.stars == 1 && base == "char" {
        return Ok(Class::Text { konst: of.konst });
    }
    // What an item is, where a list can be lent: a pointer to numbers or
    // bools, one level down.
    let item = match plain(0) {
        Ok(Class::Nothing) | Err(_) => None,
        Ok(class) if of.stars == 1 => Some(Box::new(class)),
        Ok(_) => None,
    };
    let to = if of.stars > 1 { format!("{to} {}", "*".repeat(of.stars - 1)) } else { to };
    Ok(Class::Pointer { to, konst: of.konst, item })
}

/// A library of C as a program declared it, with each type worked out:
/// what the checker and `zeno build` know of it (decision 44.10).
#[derive(Debug)]
pub(crate) struct Library {
    /// Its place among the libraries of the program.
    pub(crate) number: usize,
    /// Where `c.library` is called: the same call is one library.
    pub(crate) span: Span,
    /// What it is called in messages: the first word naming it.
    pub(crate) label: String,
    /// The words that name it: `"m"`, `"crc.c"`, `"-DSTEP=3"`.
    pub(crate) words: Vec<String>,
    /// The lines given to C as they are: `#include`, `typedef`.
    pub(crate) lines: Vec<String>,
    pub(crate) functions: Vec<Function>,
    /// The C itself, when the library is an `inline c` block of the
    /// program and not something `c.library` names (decision 61).
    pub(crate) block: Option<Block>,
}

/// An `inline c` block: its C as it was written, and where.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub(crate) text: String,
    /// The file the block is in, as an error message shows it.
    pub(crate) file: String,
    /// The line of that file that the first line of `text` is.
    pub(crate) line: usize,
}

impl Block {
    /// The block for a C compiler, which is told where each line of it
    /// was written, so that what it says of a line names the `.zeno`
    /// file and the line there. `after` is the line of the C file that
    /// comes after the block, and `c_file` that file.
    pub(crate) fn c(&self, after: usize, c_file: &str) -> String {
        let quoted = |name: &str| name.replace('\\', "/").replace('"', "");
        format!("#line {} \"{}\"\n{}#line {} \"{}\"\n", self.line, quoted(&self.file), self.text, after, quoted(c_file))
    }
}

/// One declared function, and what each of its types is to Zeno.
#[derive(Debug)]
pub(crate) struct Function {
    pub(crate) name: String,
    /// A value, read by its name, and not a function (decision 44.11).
    pub(crate) value: bool,
    /// How C is told of it: `crate::bridge_c::declared_c`.
    pub(crate) c: String,
    pub(crate) gives: (Type, Class),
    pub(crate) takes: Vec<(Type, Class)>,
}

/// Is this the name of a whole number known to take more than a byte?
/// A str can be lent for a pointer to anything else.
pub(crate) fn wider_than_a_byte(base: &str) -> bool {
    let words: Vec<&str> = base.split_whitespace().collect();
    words.iter().any(|word| matches!(*word, "short" | "int" | "long" | "wchar_t" | "size_t" | "ssize_t" | "ptrdiff_t"))
        || (base.contains("int") && !base.contains("int8") && base.ends_with("_t") && (base.starts_with("int") || base.starts_with("uint")))
}

/// `1st`, `2nd`, `3rd`, `4th`, as the messages of the helper say it.
pub(crate) fn ordinal(n: usize) -> String {
    let end = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{end}")
}

/// A library from what a program wrote: the words that name it, and its
/// declarations. The message, if it cannot be one.
pub(crate) fn library_of(words: Vec<String>, declared: &str, number: usize, span: Span) -> std::result::Result<Library, String> {
    let (lines, declared) = declarations(declared)?;
    let named = typedefs(&lines);
    let mut functions = Vec::new();
    for function in declared {
        let gives = class(&function.gives, &named).map_err(|why| format!("`{}` gives {why}", function.name))?;
        let mut takes = Vec::new();
        for (at, of) in function.takes.iter().enumerate() {
            let what = class(of, &named).map_err(|why| format!("the {} parameter of `{}` is {why}", ordinal(at + 1), function.name))?;
            if what == Class::Nothing {
                return Err(format!("a parameter of `{}` is `void`, which only a function that takes nothing has, alone: `{}(void)`", function.name, function.name));
            }
            takes.push((of.clone(), what));
        }
        if function.value && gives == Class::Nothing {
            return Err(format!("`{}` is `void`, which no value is", function.name));
        }
        let c = declared_c(&function);
        functions.push(Function { name: function.name, value: function.value, c, gives: (function.gives, gives), takes });
    }
    let label = words.iter().find(|word| !word.starts_with('-')).cloned().unwrap_or_else(|| "c".to_string());
    Ok(Library { number, span, label, words, lines, functions, block: None })
}

// ------------------------------------------------------ An inline block

/// `text` with every `__attribute__((...))` and `__declspec(...)` taken
/// out: they say how a function is built, and nothing of what it takes.
fn without_attributes(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let found = ["__attribute__", "__declspec"].iter().filter_map(|word| rest.find(word).map(|at| (at, word.len()))).min();
        let Some((at, length)) = found else { break };
        out.push_str(&rest[..at]);
        rest = &rest[at + length..];
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('(') {
            continue;
        }
        let mut depth = 0;
        let mut end = trimmed.len();
        for (i, c) in trimmed.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(' ');
        rest = &trimmed[end..];
    }
    out.push_str(rest);
    out
}

/// What an `inline c` block declares to Zeno: the lines `c.library`
/// would be given for it (decision 61.4). Each function of the block
/// that is not `static` is declared by its own first line; a line that
/// starts with `#`, a `typedef`, a `struct`, a `union` and an `enum`
/// are kept, since a function's types may be named there. A `static`
/// function and a variable are the block's own, and are left out.
///
/// The block is C, and Zeno does not check it: this reads only as far
/// as finding where each thing at the top of the file begins and ends.
pub(crate) fn block_declarations(text: &str) -> std::result::Result<String, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut item = String::new();
    let mut functions = 0;
    let mut at = 0;
    // Braces open in the thing being read: a struct, or what a variable
    // starts as. A function's body is skipped whole.
    let mut depth = 0;
    let mut line_start = true;

    // Where what begins at `from` ends: text in quotes, or a comment.
    let skip = |from: usize| -> Option<usize> {
        match (chars[from], chars.get(from + 1)) {
            ('/', Some('/')) => Some(chars[from..].iter().position(|c| *c == '\n').map_or(chars.len(), |end| from + end)),
            ('/', Some('*')) => {
                let mut end = from + 2;
                while end < chars.len() && !(chars[end] == '/' && chars[end - 1] == '*' && end > from + 2) {
                    end += 1;
                }
                Some((end + 1).min(chars.len()))
            }
            (quote @ ('"' | '\''), _) => {
                let mut end = from + 1;
                while end < chars.len() && chars[end] != quote && chars[end] != '\n' {
                    end += if chars[end] == '\\' { 2 } else { 1 };
                }
                Some((end + 1).min(chars.len()))
            }
            _ => None,
        }
    };

    while at < chars.len() {
        let c = chars[at];
        // A line for the preprocessor, with the lines it goes on to.
        if line_start && c == '#' {
            let mut end = at;
            while end < chars.len() && (chars[end] != '\n' || chars[end - 1] == '\\') {
                end += 1;
            }
            if depth == 0 {
                // A line that goes on to the next is made one line,
                // which is how the declarations are read.
                let line: String = chars[at..end].iter().collect();
                let joined: Vec<&str> = line.split("\\\n").map(str::trim).collect();
                out.push_str(joined.join(" ").trim_end());
                out.push('\n');
            }
            at = end;
            continue;
        }
        if let Some(end) = skip(at) {
            // Text in quotes is kept where it is part of a thing, and a
            // comment is one space.
            if c == '/' {
                item.push(' ');
            } else {
                item.extend(&chars[at..end]);
            }
            line_start = false;
            at = end;
            continue;
        }
        if c == '\n' {
            line_start = true;
            item.push(' ');
            at += 1;
            continue;
        }
        if !c.is_whitespace() {
            line_start = false;
        }
        match c {
            '{' if depth == 0 && is_function_head(&item) => {
                // The body, to the `}` that closes it.
                let mut open = 0;
                while at < chars.len() {
                    if let Some(end) = skip(at) {
                        at = end;
                        continue;
                    }
                    match chars[at] {
                        '{' => open += 1,
                        '}' => {
                            open -= 1;
                            if open == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                at += 1;
                let head = without_attributes(item.trim());
                item.clear();
                if tokens(&head).iter().any(|word| word == "static") {
                    continue;
                }
                let shown = head.split_whitespace().collect::<Vec<_>>().join(" ");
                if let Err(why) = declaration(&shown) {
                    return Err(format!("{why}. In an `inline c` block, a function that only the block calls is marked `static`, and Zeno leaves it alone"));
                }
                out.push_str(&shown);
                out.push_str(";\n");
                functions += 1;
                continue;
            }
            '{' => depth += 1,
            '}' => depth -= 1,
            ';' if depth == 0 => {
                let one = item.trim().to_string();
                item.clear();
                at += 1;
                // A type is kept, for the functions that name it. A
                // variable, and a function that is only declared, are
                // the block's own.
                if !one.is_empty() && is_a_type(&one) {
                    out.push_str(&one.split_whitespace().collect::<Vec<_>>().join(" "));
                    out.push_str(";\n");
                }
                continue;
            }
            _ => {}
        }
        item.push(c);
        at += 1;
    }
    if functions == 0 {
        return Err("this `inline c` block has no function that Zeno can call. A function is one when it is written out in the block, \
                    with its body, and is not `static`: `int twice(int x) { return 2 * x; }`"
            .to_string());
    }
    Ok(out)
}

/// Is what was read so far the first line of a function, whose body
/// the `{` that follows begins: `uint16_t crc16(const uint8_t *data,
/// size_t length)`? Not a struct, and not a variable with what it
/// starts as.
fn is_function_head(head: &str) -> bool {
    let head = without_attributes(head);
    let head = head.trim();
    head.ends_with(')') && !head.contains('=') && !is_a_type(head) && head.contains('(')
}

/// The library an `inline c` block is: its functions, each with what it
/// takes and gives, as `library_of` works them out for `c.library`.
pub(crate) fn library_of_block(block: Block, number: usize, span: Span) -> std::result::Result<Library, String> {
    let declared = block_declarations(&block.text)?;
    let mut library = library_of(Vec::new(), &declared, number, span)?;
    library.label = "inline c".to_string();
    library.block = Some(block);
    Ok(library)
}

// ----------------------------------------------------------- The stubs

/// How C is told of a declared function or value, before it is used.
/// `extern int total;` is a variable of the library, declared as it is
/// written. `int LIMIT;` is a value a header has (a macro, a constant
/// of an enum, a variable), which is read by its name and not declared
/// again, since C cannot declare a macro (decision 44.11).
pub(crate) fn declared_c(function: &Declared) -> String {
    if function.value && !function.text.starts_with("extern ") {
        return format!("/* {}: read by its name */\n", function.text);
    }
    format!("{};\n", function.text)
}

/// A string of C that holds this text.
fn c_text(text: &str) -> String {
    let mut out = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b' '..=b'~' => out.push(byte as char),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
    out.push('"');
    out
}

/// What a slot of this type holds, as the table of the stubs says it.
fn kind(of: &Type, named: &[(String, Option<Type>)]) -> String {
    if of.stars == 0 {
        if of.base == "void" {
            return "{'v', 0, 0, 0, 0}".to_string();
        }
        // A u64 of Zeno comes back as one whatever its value, as the
        // checker said it would (decision 44.10).
        let u64 = if class(of, named) == Ok(Class::U64) { "'U'" } else { "0" };
        return format!("{{ZCODE({0}), ZSIZE({0}), 0, {u64}, 0}}", of.base);
    }
    let konst = u8::from(of.konst);
    if of.stars == 1 && of.base == "char" {
        return format!("{{'s', sizeof(void *), {konst}, 'u', 1}}");
    }
    if of.stars > 1 {
        return format!("{{'p', sizeof(void *), {konst}, 'p', sizeof(void *)}}");
    }
    format!("{{'p', sizeof(void *), {konst}, ZCODE({0}), ZSIZE({0})}}", of.base)
}

/// The argument in slot `at`, as the type the function takes.
fn argument(of: &Type, at: usize) -> String {
    if of.stars > 0 {
        format!("({}) a[{at}].p", of.text)
    } else {
        format!("ZA({}, a[{at}])", of.text)
    }
}

/// The library of C that Zeno writes for a `c.library`.
fn stubs(lines: &[String], declared: &[Declared], block: Option<&Block>) -> String {
    let named = typedefs(lines);
    let mut out = String::from(
        "/* Written by Zeno for c.library (docs/c.md). */\n\
         #include <stdbool.h>\n#include <stddef.h>\n#include <stdint.h>\n",
    );
    if let Some(block) = block {
        // An `inline c` block is the library itself: its C comes first,
        // as it was written, and declares what the stubs call.
        out.push_str("\n/* The block, as the program wrote it. */\n");
        let after = out.lines().count() + block.text.lines().count() + 3;
        out.push_str(&block.c(after, "zeno-library.c"));
    } else {
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("\n/* As the program declared them. */\n");
        for function in declared {
            out.push_str(&declared_c(function));
        }
    }
    out.push_str(STUB_TOP);
    for (number, function) in declared.iter().enumerate() {
        let args: Vec<String> = function.takes.iter().enumerate().map(|(at, of)| argument(of, at)).collect();
        let call = if function.value { function.name.clone() } else { format!("{}({})", function.name, args.join(", ")) };
        let body = if function.gives.stars > 0 {
            format!("r->p = (void *) {call};")
        } else if function.gives.base == "void" {
            format!("(void) a; (void) r; {call};")
        } else {
            format!("ZR({}, r, {call});", function.gives.text)
        };
        out.push_str(&format!("static void zeno_call_{number}(zeno_slot *a, zeno_slot *r) {{ (void) a; {body} }}\n"));
        let kinds: Vec<String> = std::iter::once(&function.gives).chain(&function.takes).map(|of| kind(of, &named)).collect();
        out.push_str(&format!("static const zeno_kind zeno_kinds_{number}[] = {{{}}};\n", kinds.join(", ")));
        let types: Vec<String> = std::iter::once(&function.gives).chain(&function.takes).map(|of| c_text(&of.text)).collect();
        out.push_str(&format!("static const char *const zeno_types_{number}[] = {{{}}};\n", types.join(", ")));
    }
    out.push_str("static const zeno_function zeno_functions[] = {\n");
    for (number, function) in declared.iter().enumerate() {
        out.push_str(&format!(
            "    {{{}, {}, {}, zeno_kinds_{number}, zeno_types_{number}, zeno_call_{number}}},\n",
            c_text(&function.name),
            c_text(&function.text),
            if function.value { -1 } else { function.takes.len() as i64 }
        ));
    }
    out.push_str(&format!(
        "}};\nZENO_EXPORT const zeno_table *zeno_library(void) {{\n    static const zeno_table table = {{{}, zeno_functions}};\n    return &table;\n}}\n",
        declared.len()
    ));
    out
}

/// What every set of stubs begins with. The four types are the ones
/// `bridge.c` reads. What a type is (a float, a whole number with a
/// sign or without, its size) is worked out by the C compiler, from the
/// type the declaration names, so that a name a header gives a type
/// (`uLong`, `size_t`) is known as well as `int` is.
const STUB_TOP: &str = r#"
typedef union { int64_t i; uint64_t u; double d; void *p; } zeno_slot;
typedef struct { char code; unsigned char size; unsigned char konst; char pointee; unsigned char pointee_size; } zeno_kind;
typedef struct {
    const char *name;
    const char *declared;
    int arity;
    const zeno_kind *kinds;
    const char *const *types;
    void (*call)(zeno_slot *, zeno_slot *);
} zeno_function;
typedef struct { int count; const zeno_function *functions; } zeno_table;

#ifdef _WIN32
#define ZENO_EXPORT __declspec(dllexport)
#else
#define ZENO_EXPORT __attribute__((visibility("default")))
#endif

#define ZCODE(T) _Generic((T *) 0, \
    _Bool *: 'b', char *: ((char) -1 < 0 ? 'i' : 'u'), signed char *: 'i', unsigned char *: 'u', \
    short *: 'i', unsigned short *: 'u', int *: 'i', unsigned int *: 'u', long *: 'i', unsigned long *: 'u', \
    long long *: 'i', unsigned long long *: 'u', float *: 'f', double *: 'f', long double *: 'f', default: 'o')
#define ZSIZE(T) _Generic((T *) 0, \
    _Bool *: sizeof(_Bool), char *: 1, signed char *: 1, unsigned char *: 1, \
    short *: sizeof(short), unsigned short *: sizeof(short), int *: sizeof(int), unsigned int *: sizeof(int), \
    long *: sizeof(long), unsigned long *: sizeof(long), long long *: sizeof(long long), \
    unsigned long long *: sizeof(long long), float *: 4, double *: 8, long double *: 8, default: 0)
#define ZA(T, s) _Generic((T *) 0, float *: (T) (s).d, double *: (T) (s).d, long double *: (T) (s).d, default: (T) (s).u)
#define ZR(T, r, e) _Generic((T *) 0, \
    float *: (void) ((r)->d = (e)), double *: (void) ((r)->d = (e)), long double *: (void) ((r)->d = (double) (e)), \
    default: (void) ((r)->u = (uint64_t) (e)))

"#;

// ----------------------------------------------------------- Building

/// The C compilers to try: `CC`, as for `zeno build`, or else the first
/// of these that is installed.
fn compilers() -> Vec<String> {
    match std::env::var("CC") {
        Ok(named) if !named.is_empty() => vec![named],
        _ => vec!["cc".to_string(), "gcc".to_string(), "clang".to_string()],
    }
}

fn number_of(parts: &[&[u8]]) -> u64 {
    let mut number: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.iter().chain(&[0]) {
            number = (number ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    number
}

fn cache() -> PathBuf {
    crate::registry::cache().unwrap_or_else(|_| std::env::temp_dir().join("zeno-cache"))
}

/// A folder for one build, with a name no other run has.
fn making(base: &Path, name: &str) -> PathBuf {
    use std::hash::{BuildHasher, Hasher};
    let word = std::collections::hash_map::RandomState::new().build_hasher().finish();
    base.join(format!("{name}.{}.{word:016x}", std::process::id()))
}

/// Run a compiler, quietly. What it said, if it failed; `None` if there
/// is no such program.
fn compile(compiler: &str, args: &[String]) -> Option<std::result::Result<(), String>> {
    let output = Command::new(compiler).args(args).stdin(Stdio::null()).output().ok()?;
    if output.status.success() {
        Some(Ok(()))
    } else {
        Some(Err(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}

/// Make `from` the folder `to`, which another run may have made first.
fn settle(from: &Path, to: &Path, file: &str) -> PathBuf {
    if std::fs::rename(from, to).is_ok() {
        return to.join(file);
    }
    if to.join(file).is_file() {
        let _ = std::fs::remove_dir_all(from);
        return to.join(file);
    }
    from.join(file)
}

/// The C helper, built once with the C compiler and kept.
pub fn helper() -> std::result::Result<String, String> {
    let file = if cfg!(windows) { "zeno-c-helper.exe" } else { "zeno-c-helper" };
    let compilers = compilers();
    let base = cache();
    for compiler in &compilers {
        let name = format!("c-bridge-{:016x}", number_of(&[HELPER.as_bytes(), compiler.as_bytes()]));
        let built = base.join(&name);
        if built.join(file).is_file() {
            return Ok(built.join(file).to_string_lossy().into_owned());
        }
        let folder = making(&base, &name);
        if std::fs::create_dir_all(&folder).is_err() || std::fs::write(folder.join("bridge.c"), HELPER).is_err() {
            return Err(format!("cannot write the C helper in `{}`", base.display()));
        }
        let mut args = vec!["-O2".to_string(), "-o".to_string(), folder.join(file).to_string_lossy().into_owned()];
        args.push(folder.join("bridge.c").to_string_lossy().into_owned());
        let libraries: Vec<Vec<String>> = if cfg!(windows) {
            vec![vec!["-lws2_32".to_string()]]
        } else {
            // `dlopen` is in `libdl` on older systems, and in the C
            // library itself on newer ones.
            vec![vec!["-ldl".to_string()], vec![]]
        };
        let mut said = String::new();
        for extra in libraries {
            let all: Vec<String> = args.iter().cloned().chain(extra).collect();
            match compile(compiler, &all) {
                // There is no such compiler: the next is tried.
                None => break,
                Some(Ok(())) => {
                    let _ = std::fs::remove_file(folder.join("bridge.c"));
                    return Ok(settle(&folder, &built, file).to_string_lossy().into_owned());
                }
                Some(Err(text)) => said = text,
            }
        }
        let _ = std::fs::remove_dir_all(&folder);
        if !said.is_empty() {
            return Err(format!("`{compiler}` could not build the C helper. That is a bug in Zeno. It said:\n{}", first_lines(&said, 12)));
        }
    }
    Err(format!(
        "there is no C compiler to build with. Zeno looked for `{}`. `CC` can name one",
        compilers.join("`, then `")
    ))
}

fn first_lines(text: &str, count: usize) -> String {
    text.lines().take(count).collect::<Vec<_>>().join("\n")
}

/// What a word given as the library is: a library by its name, a file,
/// or a word for the compiler.
enum Part {
    /// `"z"`: `-lz`.
    Named(String),
    /// A library, an object file or C source, by its path.
    File(PathBuf),
    /// `"-lpthread"`, as it is.
    Flag(String),
}

/// A path as a C compiler can be given it. On Windows `canonicalize`
/// gives a path that begins `\\?\`, which gcc of MinGW does not read: it
/// took `\\?\D:\a\crc.c` for `\\crc.c`. The same path without it is
/// the same file.
pub(crate) fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// How the name of a file ends, among the words of a library: C, the
/// assembly a C compiler takes (`.s`, and `.S`, which it reads as C
/// reads a file first), and what either was built into. Any other word
/// with no `/` in it is a library of the system.
const FILE_ENDINGS: &[&str] = &[".c", ".s", ".so", ".a", ".o", ".dylib", ".dll", ".lib", ".obj"];

fn parts(words: &[String], home: &Path, span: Span) -> Result<Vec<Part>> {
    let mut out = Vec::new();
    for word in words {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        if word.starts_with('-') {
            out.push(Part::Flag(word.to_string()));
            continue;
        }
        let lower = word.to_ascii_lowercase();
        let is_file = word.contains('/')
            || word.contains('\\')
            || FILE_ENDINGS.iter().any(|end| lower.ends_with(end))
            || lower.contains(".so.");
        if !is_file {
            out.push(Part::Named(word.to_string()));
            continue;
        }
        let path = home.join(word);
        if !path.is_file() {
            return raise("FileError", format!("there is no file `{word}`, from the folder `{}`", home.display()), span);
        }
        out.push(Part::File(plain(path.canonicalize().unwrap_or(path))));
    }
    Ok(out)
}

/// What the C compiler is given, besides the program's own C, to build a
/// program that uses these libraries (`zeno build`, decision 44.10): a
/// file of C, or a library, found from `home`, the program's folder; a
/// library of the system as `-l`; and anything that starts with `-`.
pub(crate) fn build_args(libraries: &[std::rc::Rc<Library>], home: &Path) -> std::result::Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut folders: Vec<PathBuf> = vec![home.to_path_buf()];
    for library in libraries {
        for word in &library.words {
            let word = word.trim();
            if word.is_empty() {
                continue;
            }
            if word.starts_with('-') {
                args.push(word.to_string());
                continue;
            }
            let lower = word.to_ascii_lowercase();
            let is_file = word.contains('/')
                || word.contains('\\')
                || FILE_ENDINGS.iter().any(|end| lower.ends_with(end))
                || lower.contains(".so.");
            if !is_file {
                args.push(format!("-l{word}"));
                continue;
            }
            let path = home.join(word);
            if !path.is_file() {
                return Err(format!("there is no file `{word}`, from the folder `{}`, which `c.library` names", home.display()));
            }
            if let Some(folder) = path.parent() {
                if !folders.iter().any(|known| known == folder) {
                    folders.push(folder.to_path_buf());
                }
            }
            args.push(path.to_string_lossy().into_owned());
        }
    }
    let mut out: Vec<String> = folders.iter().map(|folder| format!("-I{}", folder.display())).collect();
    out.extend(args);
    if !cfg!(windows) {
        for folder in &folders {
            out.push(format!("-Wl,-rpath,{}", folder.display()));
        }
    }
    Ok(out)
}

/// What a value given to `c.library` says, as words: a str, or a list
/// of them.
fn words(value: &Value, what: &str, span: Span) -> Result<Vec<String>> {
    match value {
        Value::Str(text) => Ok(vec![text.to_string()]),
        Value::List(items) => items
            .borrow()
            .iter()
            .map(|item| match item {
                Value::Str(text) => Ok(text.to_string()),
                other => raise("Error", format!("{what} is a str or a list of str, and this list holds {}", other.type_name()), span),
            })
            .collect(),
        Value::None if what.starts_with("the library") => Ok(Vec::new()),
        other => raise("Error", format!("{what} is a str or a list of str, and this is {}", other.type_name()), span),
    }
}

/// `c.library(library, declarations)`: the stubs built, and loaded.
#[cold]
#[inline(never)]
pub fn library(library: &Value, declared: &Value, span: Span) -> Result {
    let words_given = words(library, "the library given to `c.library`", span)?;
    let text = words(declared, "what `c.library` is given to declare", span)?.join("\n");
    let (lines, functions) = match declarations(&text) {
        Ok(found) => found,
        Err(why) => return raise("ValueError", why, span),
    };
    let label = words_given.first().cloned().unwrap_or_else(|| "c".to_string());
    built(&words_given, &lines, &functions, None, &label, span)
}

/// An `inline c` block of the program, built and loaded as a library is
/// (decision 61.5): its C is the library, and the stubs go in the same
/// file of C.
#[cold]
#[inline(never)]
pub fn block(block: &Block, span: Span) -> Result {
    let found = block_declarations(&block.text).and_then(|declared| declarations(&declared));
    let (lines, functions) = match found {
        Ok(found) => found,
        Err(why) => return raise("ValueError", why, span),
    };
    built(&[], &lines, &functions, Some(block), "inline c", span)
}

/// The stubs for these functions, built with the library they call, or
/// taken from where an earlier run kept them, and loaded by the helper.
fn built(words_given: &[String], lines: &[String], functions: &[Declared], block: Option<&Block>, label: &str, span: Span) -> Result {
    let home = crate::bridge::home().or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."));
    let parts = parts(words_given, &home, span)?;
    let source = stubs(lines, functions, block);

    let file = if cfg!(windows) { "zeno-library.dll" } else { "zeno-library.so" };
    let mut args: Vec<String> = Vec::new();
    let mut folders: Vec<PathBuf> = vec![home.clone()];
    let mut said: Vec<Vec<u8>> = vec![source.clone().into_bytes()];
    for part in &parts {
        match part {
            Part::Named(name) => args.push(format!("-l{name}")),
            Part::Flag(flag) => args.push(flag.clone()),
            Part::File(path) => {
                args.push(path.to_string_lossy().into_owned());
                if let Some(folder) = path.parent() {
                    if !folders.iter().any(|f| f == folder) {
                        folders.push(folder.to_path_buf());
                    }
                }
                // A file that changed is built again.
                let changed = std::fs::metadata(path)
                    .ok()
                    .and_then(|data| data.modified().ok().map(|when| (data.len(), when)))
                    .map(|(size, when)| format!("{size} {when:?}"))
                    .unwrap_or_default();
                said.push(changed.into_bytes());
            }
        }
    }
    for arg in &args {
        said.push(arg.clone().into_bytes());
    }
    for folder in &folders {
        said.push(folder.to_string_lossy().into_owned().into_bytes());
    }

    let base = cache();
    let mut tried = Vec::new();
    for compiler in compilers() {
        let mut all: Vec<&[u8]> = said.iter().map(Vec::as_slice).collect();
        all.push(compiler.as_bytes());
        let name = format!("c-library-{:016x}", number_of(&all));
        let built = base.join(&name);
        if built.join(file).is_file() {
            return crate::bridge::c_library(&built.join(file).to_string_lossy(), label, span);
        }
        let folder = making(&base, &name);
        if std::fs::create_dir_all(&folder).is_err() || std::fs::write(folder.join("zeno-library.c"), &source).is_err() {
            return raise("FileError", format!("cannot write what `c.library` builds, in `{}`", base.display()), span);
        }
        let mut command: Vec<String> = vec!["-shared".to_string()];
        if !cfg!(windows) {
            command.push("-fPIC".to_string());
        }
        command.extend(["-O1".to_string(), "-o".to_string(), folder.join(file).to_string_lossy().into_owned()]);
        command.push(folder.join("zeno-library.c").to_string_lossy().into_owned());
        // A header, or a library, beside the program or beside a file
        // it names, is found there.
        for folder in &folders {
            command.push(format!("-I{}", folder.display()));
            command.push(format!("-L{}", folder.display()));
        }
        command.extend(args.iter().cloned());
        if !cfg!(windows) {
            // Where a library that is a file of its own is found, when
            // the helper loads the stubs.
            for folder in &folders {
                command.push(format!("-Wl,-rpath,{}", folder.display()));
            }
        }
        match compile(&compiler, &command) {
            None => {
                let _ = std::fs::remove_dir_all(&folder);
                tried.push(compiler);
                continue;
            }
            Some(Ok(())) => {
                let path = settle(&folder, &built, file);
                return crate::bridge::c_library(&path.to_string_lossy(), label, span);
            }
            Some(Err(text)) => {
                let _ = std::fs::remove_dir_all(&folder);
                let names: Vec<&str> = functions.iter().map(|function| function.name.as_str()).collect();
                if block.is_some() {
                    // Each line the compiler names is a line of the
                    // `.zeno` file already.
                    return raise(
                        "Error",
                        format!("C could not build this `inline c` block. The C compiler said:\n{}", first_lines(text.trim(), 15)),
                        span,
                    );
                }
                return raise_build_error(&text, &folder, label, &names, span);
            }
        }
    }
    raise(
        "FileError",
        format!("there is no C compiler to build with. Zeno looked for `{}`. `CC` can name one", tried.join("`, then `")),
        span,
    )
}

/// The name a linker says nothing defines, if that is what a line of
/// its says. GNU ld: "undefined reference to `name'"; lld: "undefined
/// symbol: name"; the linker of macOS lists each as `"_name", referenced
/// from:`, with the underscore it puts before every name of C.
fn missing_name(line: &str) -> Option<String> {
    let name_of = |rest: &str| -> String { rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect() };
    for sign in ["undefined reference to `", "undefined reference to '", "undefined symbol: "] {
        if let Some(at) = line.find(sign) {
            let name = name_of(&line[at + sign.len()..]);
            return (!name.is_empty()).then_some(name);
        }
    }
    let line = line.trim();
    if line.ends_with(", referenced from:") {
        if let Some(rest) = line.strip_prefix("\"_") {
            let name = name_of(rest);
            return (!name.is_empty()).then_some(name);
        }
    }
    None
}

/// What the C compiler said, as an error of Zeno.
fn raise_build_error(said: &str, folder: &Path, label: &str, declared: &[&str], span: Span) -> Result {
    // The linkers of Windows and macOS want every declared name to be
    // there when the stubs are built, where the linker of Linux leaves
    // it to the loading: what they say is said as the loading says it.
    if let Some(name) = said.lines().find_map(missing_name).filter(|name| declared.contains(&name.as_str())) {
        return raise("Error", format!("the library `{label}` has no `{name}`, which was declared"), span);
    }
    for line in said.lines() {
        // The newer linker of macOS: "ld: library 'foo' not found".
        if let Some(at) = line.find("library '") {
            let rest = &line[at + "library '".len()..];
            if let Some(end) = rest.find("' not found") {
                let name = &rest[..end];
                return raise(
                    "FileError",
                    format!(
                        "C has no library `{name}`. Is it installed, with what building with it needs (on Debian and Ubuntu, its package that ends in `-dev`)?"
                    ),
                    span,
                );
            }
        }
        // GNU ld: "cannot find -lfoo"; the older linker of macOS: "library
        // not found for -lfoo".
        for sign in ["cannot find -l", "library not found for -l"] {
            if let Some(at) = line.find(sign) {
                let name: String = line[at + sign.len()..].chars().take_while(|c| !c.is_whitespace() && *c != ':' && *c != '\'').collect();
                return raise(
                    "FileError",
                    format!(
                        "C has no library `{name}`. Is it installed, with what building with it needs (on Debian and Ubuntu, its package that ends in `-dev`)?"
                    ),
                    span,
                );
            }
        }
    }
    let shown = folder.join("zeno-library.c").to_string_lossy().into_owned();
    let said = said.replace(&shown, "the declarations");
    raise("Error", format!("C could not build what was declared for `{label}`. The C compiler said:\n{}", first_lines(said.trim(), 15)), span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_is_read_as_c_reads_it() {
        let (lines, found) = declarations(
            "#include <zlib.h>\n\
             /* the sum */ unsigned long crc32(unsigned long crc, const unsigned char *buf, unsigned int len);\n\
             int abs(int);   double frexp(double x, int *exponent); // two\n\
             void free(void *); char *getenv(const char *name); size_t strlen(const char*);\n\
             int main2(int argc, char *const argv[]); struct tm *gmtime(const time_t *t); const char *zlibVersion(void);\n\
             typedef struct counter counter; struct point { int x; int y; };\n\
             enum mode { OFF, ON }; typedef void (*handler)(int);",
        )
        .expect("they are declarations");
        // A type is given to C as it is, after the lines of `#`.
        assert_eq!(
            lines,
            [
                "#include <zlib.h>",
                "typedef struct counter counter;",
                "struct point { int x; int y; };",
                "enum mode { OFF, ON };",
                "typedef void (*handler)(int);"
            ]
        );
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["crc32", "abs", "frexp", "free", "getenv", "strlen", "main2", "gmtime", "zlibVersion"]);
        let crc = &found[0];
        assert_eq!(crc.gives.text, "unsigned long");
        assert_eq!(crc.takes.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["unsigned long", "const unsigned char *", "unsigned int"]);
        assert!(crc.takes[1].konst && crc.takes[1].stars == 1 && crc.takes[1].base == "unsigned char");
        assert_eq!(crc.text, "unsigned long crc32(unsigned long crc, const unsigned char *buf, unsigned int len)");
        assert_eq!(found[1].takes[0].text, "int");
        assert!(!found[2].takes[1].konst && found[2].takes[1].base == "int");
        assert_eq!(found[4].gives.text, "char *");
        assert_eq!(found[5].takes[0].text, "const char *");
        // `char *const argv[]`: the pointers are constant, and are what
        // the parameter points to.
        assert_eq!(found[6].takes[1].stars, 2);
        assert!(found[6].takes[1].konst);
        assert_eq!(found[7].gives.base, "struct tm");
        assert_eq!(found[7].takes[0].base, "time_t");
        assert!(found[8].takes.is_empty());
        assert!(found[8].gives.konst && found[8].gives.base == "char");

        // What each linker says of a name nothing defines.
        for (line, name) in [
            ("ld.exe: C:\\Temp\\ccqnKXGm.o:zeno-library.c:(.text+0xb): undefined reference to `no_such'", Some("no_such")),
            ("  \"_no_such\", referenced from:", Some("no_such")),
            ("ld.lld: error: undefined symbol: no_such", Some("no_such")),
            ("ld: symbol(s) not found for architecture arm64", None),
        ] {
            assert_eq!(missing_name(line).as_deref(), name, "{line}");
        }

        // A declaration with no `(` is a value, read by its name.
        let (_, found) = declarations("int LIMIT;").unwrap();
        assert!(found[0].value && found[0].name == "LIMIT" && found[0].gives.base == "int");

        for (wrong, says) in [
            ("int printf(const char *format, ...);", "any number of arguments"),
            ("void qsort(void *base, size_t n, size_t size, int (*compare)(const void *, const void *));", "a function of Zeno cannot be given"),
            ("int f(int x) { return x; }", "has a body"),
            ("int f(int x)", "has no `;`"),
            ("abs(int x);", "says nothing of what `abs` gives"),
            ("int x = 3;", "neither a function"),
            ("struct point { int x; int y; }; struct point middle(struct point a);", "a whole `struct point`"),
            ("int f(void); long f(int);", "declared twice"),
            ("", "no declarations"),
        ] {
            let error = declarations(wrong).expect_err(wrong);
            assert!(error.contains(says), "{wrong}: {error}");
        }
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    #[test]
    fn a_block_declares_the_functions_it_writes_out() {
        let block = "#include <stdint.h>\n\
             #define LIMIT 7\n\
             #define TWICE(x) \\\n    ((x) * 2)\n\
             typedef struct { int x; int y; } point;\n\
             struct pair { int a; int b; };\n\
             enum mode { OFF, ON = 4 };\n\
             static const int table[3] = {1, 2, 3};\n\
             int counter = 0;\n\
             int declared_only(int x);\n\
             /* A comment with a { in it. */\n\
             static int helper(int x) { return x + '}'; }\n\
             int px(const point *p)\n{\n    // }\n    return p ? p->x : table[1] + LIMIT;\n}\n\
             const char *name(void) { return \"}{ \\\" block\"; }\n\
             __attribute__((noinline)) double half(double x) { return x / 2; }\n\
             struct pair *both(void) { static struct pair p; return &p; }\n";
        assert_eq!(
            block_declarations(block).unwrap(),
            "#include <stdint.h>\n\
             #define LIMIT 7\n\
             #define TWICE(x) ((x) * 2)\n\
             typedef struct { int x; int y; } point;\n\
             struct pair { int a; int b; };\n\
             enum mode { OFF, ON = 4 };\n\
             int px(const point *p);\n\
             const char *name(void);\n\
             double half(double x);\n\
             struct pair *both(void);\n"
        );
        let library = library_of_block(Block { text: block.to_string(), file: "a.zeno".to_string(), line: 3 }, 0, Span { line: 2, col: 9 }).unwrap();
        let names: Vec<&str> = library.functions.iter().map(|function| function.name.as_str()).collect();
        assert_eq!(names, ["px", "name", "half", "both"]);
        assert_eq!(library.label, "inline c");
        assert!(library.words.is_empty());
    }

    #[test]
    fn a_block_with_nothing_to_call_and_one_zeno_cannot_call_are_refused() {
        assert_eq!(
            block_declarations("static int twice(int x) { return 2 * x; }\nint limit = 3;\n").unwrap_err(),
            "this `inline c` block has no function that Zeno can call. A function is one when it is written out in the block, \
             with its body, and is not `static`: `int twice(int x) { return 2 * x; }`"
        );
        let said = block_declarations("int say(const char *format, ...) { return 0; }\n").unwrap_err();
        assert!(said.starts_with("`say` takes any number of arguments (`...`)"), "{said}");
        assert!(said.ends_with("In an `inline c` block, a function that only the block calls is marked `static`, and Zeno leaves it alone"), "{said}");
    }

    #[test]
    fn a_block_is_told_where_it_was_written() {
        let block = Block { text: "int one(void) { return 1; }\n".to_string(), file: "src\\main.zeno".to_string(), line: 12 };
        assert_eq!(block.c(40, "main.c"), "#line 12 \"src/main.zeno\"\nint one(void) { return 1; }\n#line 40 \"main.c\"\n");
    }
}

#[cfg(test)]
mod parts_tests {
    use super::*;

    #[test]
    fn a_file_of_assembly_is_a_file_and_not_a_library_of_the_system() {
        let folder = std::env::temp_dir().join(format!("zeno-assembly-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["start.s", "vectors.S"] {
            std::fs::write(folder.join(name), "").unwrap();
        }
        let words = ["start.s".to_string(), "vectors.S".to_string(), "m".to_string()];
        let parts = parts(&words, &folder, Span { line: 1, col: 1 }).unwrap_or_else(|_| panic!("the files are there"));
        assert!(matches!(&parts[0], Part::File(path) if path.ends_with("start.s")));
        assert!(matches!(&parts[1], Part::File(path) if path.ends_with("vectors.S")));
        assert!(matches!(&parts[2], Part::Named(name) if name == "m"));
        let _ = std::fs::remove_dir_all(&folder);
    }
}

#[cfg(test)]
mod plain_tests {
    use super::plain;
    use std::path::PathBuf;

    #[test]
    fn a_path_of_windows_is_given_to_c_without_its_long_prefix() {
        assert_eq!(plain(PathBuf::from(r"\\?\D:\a\zeno\examples\crc.c")), PathBuf::from(r"D:\a\zeno\examples\crc.c"));
        assert_eq!(plain(PathBuf::from(r"\\?\UNC\server\share\crc.c")), PathBuf::from(r"\\server\share\crc.c"));
        assert_eq!(plain(PathBuf::from("/home/me/crc.c")), PathBuf::from("/home/me/crc.c"));
    }
}
