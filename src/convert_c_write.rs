//! `zeno convert program.c`: the file as a whole (docs/convert.md,
//! decisions chapter 63). Which pieces are kept as C, the `inline c`
//! block that holds them, and the order of what is written.
//!
//! A piece of C is a top-level declaration, and the converter keeps
//! whole pieces: a function is Zeno or it is C. What one side may ask of
//! the other decides the rest:
//!
//! - C can call only C. A function that a kept function calls is kept.
//! - A variable lives on one side. A global that a kept function uses is
//!   kept, and a function of Zeno that uses a kept global is kept.
//! - Zeno calls a kept function through the block, with what can go
//!   across (docs/c.md): numbers, texts, lists for arrays. A call that
//!   gives it a struct is kept.
//!
//! Types, `#define`s and `#include`s are copied into the block, so the C
//! there means what it meant, and written as Zeno too, for the Zeno.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::convert::{Converted, KeptPiece, Note, Report};
use crate::convert_c::*;
use crate::convert_c_lines::*;

/// The words of Zeno, and the names that what is written uses: a name of
/// C that is one of them is written with a `_` after it.
const ZENO_NAMES: &[&str] = &[
    "let", "var", "const", "function", "return", "if", "else", "for", "in", "while", "unsafe", "private", "interface", "owned", "take", "strict",
    "break", "continue", "match", "case", "class", "enum", "try", "except", "raise", "raises", "assert", "as", "from", "import", "and", "or", "not",
    "is", "true", "false", "none", "self", "test", "inline", "print", "len", "range", "int", "float", "str", "abs", "min", "max", "sum", "allocate",
    "free", "char", "input", "list", "dict", "bool", "math", "program", "c", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "copy",
    "c_divide", "c_remainder", "times_u32", "letter_code", "letter", "write_text", "finish_text", "line_so_far", "exit_status",
];

struct Unit {
    piece: Piece,
    item: Kept<Item>,
    kept: Option<(usize, String)>,
    label: String,
    /// The first line of the comments right above it, which go with it.
    lead: usize,
}

/// What one round wrote for a piece.
struct Written {
    lines: Vec<String>,
    helpers: BTreeSet<Helper>,
    notes: Vec<(Note, usize)>,
    imports: BTreeSet<&'static str>,
    copied: BTreeSet<String>,
}

/// Whether `main` takes the program's arguments as C gives them:
/// `int argc, char *argv[]` (or `char **argv`).
fn takes_arguments(params: &[Param]) -> bool {
    let words = |ty: &CTy| matches!(ty, CTy::Ptr(item, _) if matches!(**item, CTy::Int(IntTy { char: true, .. })));
    params.len() == 2
        && matches!(params[0].ty, CTy::Int(_))
        && match &params[1].ty {
            CTy::Ptr(item, _) | CTy::Array(item, _) => words(item),
            _ => false,
        }
}

/// The C put at the end of the block when `main` reads the program's
/// arguments: the Zeno gives the words one at a time, as texts can go
/// across, and then calls `main` through `convert_main`.
const ARGUMENTS_C: &[&str] = &[
    "#include <stdlib.h>",
    "#include <string.h>",
    "",
    "static int convert_argc = 0;",
    "static char **convert_argv = NULL;",
    "",
    "void convert_argument(const char *word) {",
    "    size_t size = strlen(word) + 1;",
    "    convert_argv = realloc(convert_argv, (convert_argc + 2) * sizeof *convert_argv);",
    "    convert_argv[convert_argc] = memcpy(malloc(size), word, size);",
    "    convert_argc++;",
    "    convert_argv[convert_argc] = NULL;",
    "}",
    "",
    "int convert_main(void) {",
    "    return main(convert_argc, convert_argv);",
    "}",
];

/// The Zeno that gives `main` the program's arguments, the name of the
/// file first as `argv[0]`.
fn argument_lines(block: &str, file: &str) -> Vec<String> {
    let name = program_name(file);
    vec![
        format!("{block}.convert_argument(\"{name}\")"),
        "for word in program.arguments() {".to_string(),
        format!("    {block}.convert_argument(word)"),
        "}".to_string(),
    ]
}

/// The name `argv[0]` is given: the file's, without `.c`.
fn program_name(file: &str) -> String {
    let name = file.strip_suffix(".c").unwrap_or(file);
    name.chars().filter(|c| !matches!(c, '"' | '\\' | '{' | '}')).collect()
}

/// In a file kept whole, whether its `main` reads the program's
/// arguments, from the text of its first line.
fn whole_takes_arguments(source: &str) -> bool {
    source.lines().any(|line| {
        let Some(at) = line.find("main(") else { return false };
        let Some(end) = line[at..].find(')') else { return false };
        let inside = &line[at + 5..at + end];
        inside.split(',').count() == 2 && inside.trim_start().starts_with("int ") && inside.contains("char") && inside.contains('*')
    })
}

fn whole(source: &str, file: &str, why: String) -> Converted {
    let mut lines = vec!["import c".to_string()];
    let arguments = whole_takes_arguments(source);
    let has_main = arguments || source.contains("main(void)") || source.contains("main()");
    if has_main {
        lines.push("import program".to_string());
    }
    lines.push(String::new());
    let block = "c_part";
    lines.push(format!("let {block} = inline c {{"));
    for line in source.lines() {
        lines.push(if line.trim().is_empty() { String::new() } else { format!("    {line}") });
    }
    if arguments {
        lines.push(String::new());
        lines.extend(ARGUMENTS_C.iter().map(|line| if line.is_empty() { String::new() } else { format!("    {line}") }));
    }
    lines.push("}".to_string());
    let mut noted = Vec::new();
    if has_main {
        lines.push(String::new());
        if arguments {
            lines.extend(Note::ProgramName.lines(&program_name(file)).iter().map(|line| format!("# {line}")));
            lines.extend(argument_lines(block, file));
            noted.push((Note::ProgramName, 1));
        }
        let callee = if arguments { "convert_main" } else { "main" };
        lines.push(format!("let exit_status = {block}.{callee}()"));
        lines.push("if exit_status != 0 {".to_string());
        lines.push("    program.exit(exit_status)".to_string());
        lines.push("}".to_string());
    }
    let report = Report { pieces: 0, noted, kept: Vec::new(), whole: Some(why), warnings: Vec::new(), language: "C" };
    Converted { zeno: format!("{}\n", lines.join("\n")), report }
}

fn label(item: &Kept<Item>, piece: &Piece, toks: &[Tok]) -> String {
    match item {
        Ok(Item::Include(header)) => format!("`#include {header}`"),
        Ok(Item::Define(name, _)) | Ok(Item::Macro(name)) => format!("`#define {name}`"),
        Ok(Item::Function(f)) => format!("function `{}`", f.name),
        Ok(Item::Prototype(name, ..)) => format!("the declaration of `{name}`"),
        Ok(Item::Globals(vars)) => format!("`{}`", vars.iter().map(|var| var.name.as_str()).collect::<Vec<_>>().join("`, `")),
        Ok(Item::Struct(name, _)) => format!("struct `{name}`"),
        Ok(Item::Enum(values)) => format!("the enum of `{}`", values.first().map(|(name, _)| name.as_str()).unwrap_or("")),
        Ok(Item::Nothing) => "a declaration".to_string(),
        Err(_) if unread_function(&toks[piece.from..piece.to]).is_some() => {
            format!("function `{}`", unread_function(&toks[piece.from..piece.to]).unwrap_or_default())
        }
        Err(_) => {
            // The words up to the first bracket: `int read_all(`.
            let words: Vec<String> = toks[piece.from..piece.to]
                .iter()
                .take_while(|tok| !matches!(tok.kind, T::Punct("(") | T::Punct("{") | T::Punct("=") | T::Punct(";")))
                .take(4)
                .map(|tok| match &tok.kind {
                    T::Name(name) => name.clone(),
                    T::Punct(punct) => punct.to_string(),
                    T::Directive(text) => text.clone(),
                    _ => "...".to_string(),
                })
                .collect();
            format!("`{}`", words.join(" ").trim())
        }
    }
}

/// How each pointer parameter of a function is used.
fn kinds_of(f: &Function) -> Vec<Kind> {
    f.params
        .iter()
        .map(|param| {
            let CTy::Ptr(item, _) = &param.ty else { return Kind::Value };
            if matches!(**item, CTy::Int(IntTy { char: true, .. })) {
                return Kind::Text;
            }
            let mut indexed = false;
            for stmt in &f.body {
                each_expr(stmt, &mut |e| {
                    if let E::Index(base, _) = &e.kind {
                        if matches!(&base.kind, E::Name(name) if *name == param.name) {
                            indexed = true;
                        }
                    }
                });
            }
            match (&**item, indexed) {
                (_, true) => Kind::Array,
                (CTy::Struct(_), false) => Kind::Ref,
                _ => Kind::Box,
            }
        })
        .collect()
}

/// The variable at the root of a place: `p` of `p.x[2]`.
fn root(e: &Expr) -> Option<&str> {
    match &e.kind {
        E::Name(name) => Some(name),
        E::Member(base, _, false) | E::Index(base, _) => root(base),
        _ => None,
    }
}

fn changes_of(f: &Function) -> Vec<bool> {
    let mut touched = HashSet::new();
    for stmt in &f.body {
        each_expr(stmt, &mut |e| match &e.kind {
            E::Assign(_, target, _) | E::Pre(_, target) | E::Post(_, target) | E::Unary("&", target) => {
                if let Some(name) = root(target) {
                    touched.insert(name.to_string());
                }
            }
            _ => {}
        });
    }
    f.params.iter().map(|param| touched.contains(&param.name)).collect()
}

/// The names a function's body mentions, and the functions it calls.
fn mentions(f: &Function) -> (HashSet<String>, HashSet<String>) {
    let mut names = HashSet::new();
    let mut called = HashSet::new();
    for stmt in &f.body {
        each_expr(stmt, &mut |e| match &e.kind {
            E::Name(name) => {
                names.insert(name.clone());
            }
            E::Call(name, _) => {
                called.insert(name.clone());
            }
            _ => {}
        });
    }
    (names, called)
}

/// Whether a function writes part of a line: a `printf` whose text does
/// not end its line, a `putchar` of something other than `'\n'`.
fn writes_parts(f: &Function) -> bool {
    let mut parts = false;
    for stmt in &f.body {
        each_expr(stmt, &mut |e| {
            if let E::Call(name, args) = &e.kind {
                let format = match name.as_str() {
                    "printf" => args.first(),
                    "fprintf" if matches!(args.first().map(|arg| &arg.kind), Some(E::Name(stream)) if stream == "stdout") => args.get(1),
                    "putchar" => {
                        if !matches!(args.first().map(|arg| &arg.kind), Some(E::Char(10))) {
                            parts = true;
                        }
                        return;
                    }
                    "fputs" => {
                        parts = true;
                        return;
                    }
                    _ => return,
                };
                if let Some(Expr { kind: E::Str(text), .. }) = format {
                    if !text.ends_with('\n') {
                        parts = true;
                    }
                }
            }
        });
    }
    parts
}

/// Whether a function writes to the standard output at all.
fn writes(f: &Function) -> bool {
    let mut found = false;
    for stmt in &f.body {
        each_expr(stmt, &mut |e| {
            if let E::Call(name, args) = &e.kind {
                let to_stdout = match name.as_str() {
                    "printf" | "puts" | "putchar" | "putc" | "fputc" | "fwrite" => true,
                    "fprintf" | "fputs" => !args.iter().any(|arg| matches!(&arg.kind, E::Name(stream) if stream == "stderr")),
                    _ => false,
                };
                found |= to_stdout;
            }
        });
    }
    found
}

/// A copy of a value of this type, as C copies it.
fn copy_of(code: &str, ty: &CTy, copied: &mut BTreeSet<String>, depth: usize) -> String {
    match ty {
        CTy::Struct(name) => {
            copied.insert(name.clone());
            format!("{code}.copy()")
        }
        CTy::Array(item, _) => match &**item {
            CTy::Struct(_) | CTy::Array(..) => {
                let each = if depth == 0 { "item".to_string() } else { format!("item_{depth}") };
                format!("[{} for {each} in {code}]", copy_of(&each, item, copied, depth + 1))
            }
            _ => format!("{code}.slice(0)"),
        },
        _ => code.to_string(),
    }
}

fn class_lines(world: &World, name: &str, fields: &[Param], copied: &mut BTreeSet<String>, line: usize) -> Kept<Vec<String>> {
    let zname = world.zeno_name(name);
    let mut lines = vec![format!("class {zname} {{")];
    for field in fields {
        lines.push(format!("    {}: {}", world.zeno_name(&field.name), zeno_type(&field.ty, line)?));
    }
    if copied.contains(name) {
        let mut values = Vec::new();
        for field in fields {
            values.push(copy_of(&format!("self.{}", world.zeno_name(&field.name)), &field.ty, copied, 0));
        }
        lines.push(String::new());
        lines.push("    # C copies a struct where it is given or stored; Zeno shares an object.".to_string());
        lines.push(format!("    function copy(self) -> {zname} {{"));
        lines.push(format!("        return {zname}({})", values.join(", ")));
        lines.push("    }".to_string());
    }
    lines.push("}".to_string());
    Ok(lines)
}

/// Write a file of C as Zeno.
pub fn convert(source: &str, file: &str) -> Converted {
    let lexed = match lex(source) {
        Ok(lexed) => lexed,
        Err(keep) => return whole(source, file, format!("line {}: {}", keep.line, keep.why)),
    };
    let source_lines: Vec<&str> = source.lines().collect();
    let mut names = Names::default();
    let mut units: Vec<Unit> = Vec::new();
    for piece in pieces(&lexed) {
        let toks = &lexed.toks[piece.from..piece.to];
        let item = Parser::new(toks, &mut names).item();
        if let (Err(keep), T::Directive(_)) = (&item, &toks[0].kind) {
            return whole(source, file, format!("line {}: {}", keep.line, keep.why));
        }
        let label = label(&item, &piece, &lexed.toks);
        units.push(Unit { lead: piece.line, piece, item, kept: None, label });
    }
    if units.is_empty() {
        return whole(source, file, "the file has no declarations".to_string());
    }
    // Comments: those right above a piece go with it; the rest where
    // they are.
    let mut previous_end = 0;
    for unit in units.iter_mut() {
        let above: Vec<&Comment> = lexed.comments.iter().filter(|comment| comment.alone && comment.line > previous_end && comment.line < unit.piece.line).collect();
        if let Some(first) = above.first() {
            unit.lead = first.line;
        }
        previous_end = unit.piece.end_line;
    }
    let header_end = if lexed.comments.first().is_some_and(|comment| comment.line == 1 || source_lines[..comment_line_index(&lexed.comments[0])].iter().all(|line| line.trim().is_empty())) {
        // The comment at the top of the file, before anything: it is the
        // file's, and stays at the top.
        let first_code = units[0].piece.line;
        let mut end = 0;
        for comment in &lexed.comments {
            if comment.line < first_code && comment.line == end + 1 + blank_run(&source_lines, end) {
                end = comment.line + comment.lines.len() - 1;
            } else {
                break;
            }
        }
        end
    } else {
        0
    };
    if units[0].lead <= header_end {
        units[0].lead = units[0].piece.line;
    }

    let mut world = World { names: names.clone(), ..World::default() };
    let file_names: HashSet<String> = lexed.toks.iter().filter_map(|tok| if let T::Name(name) = &tok.kind { Some(name.clone()) } else { None }).collect();
    world.block = ["c_part", "kept", "kept_c"].into_iter().find(|name| !file_names.contains(*name)).unwrap_or("c_kept").to_string();
    for name in &file_names {
        if ZENO_NAMES.contains(&name.as_str()) {
            let mut renamed = format!("{name}_");
            while file_names.contains(&renamed) {
                renamed.push('_');
            }
            world.renamed.insert(name.clone(), renamed);
        }
    }
    world.used = file_names.clone();
    world.used.extend(ZENO_NAMES.iter().map(|name| name.to_string()));
    world.used.insert(world.block.clone());
    let mut calls: HashMap<String, HashSet<String>> = HashMap::new();
    let mut direct_globals: HashMap<String, HashSet<String>> = HashMap::new();
    for (at, unit) in units.iter().enumerate() {
        match &unit.item {
            Ok(Item::Function(f)) => {
                let (mentioned, called) = mentions(f);
                calls.insert(f.name.clone(), called);
                direct_globals.insert(f.name.clone(), mentioned);
                world.functions.insert(
                    f.name.clone(),
                    FnInfo { unit: at, ret: f.ret.clone(), params: f.params.clone(), kinds: kinds_of(f), globals: HashSet::new(), changes: changes_of(f), read: true },
                );
            }
            Ok(Item::Globals(vars)) => {
                for var in vars {
                    world.globals.insert(var.name.clone(), GlobalInfo { unit: at, ty: var.ty.clone(), constant: var.constant });
                }
            }
            Ok(Item::Define(name, value)) => {
                world.defines.insert(name.clone(), (at, value.clone()));
            }
            Ok(Item::Macro(name)) => {
                world.macros.insert(name.clone(), at);
            }
            Err(_) => {
                // What it calls and the names it uses, from its tokens.
                let toks = &lexed.toks[unit.piece.from..unit.piece.to];
                if let Some(name) = unread_function(toks) {
                    let mut called = HashSet::new();
                    let mut mentioned = HashSet::new();
                    for (at, tok) in toks.iter().enumerate().skip(1) {
                        if let T::Name(word) = &tok.kind {
                            if toks.get(at + 1).is_some_and(|next| next.kind == T::Punct("(")) && *word != name {
                                called.insert(word.clone());
                            }
                            mentioned.insert(word.clone());
                        }
                    }
                    calls.insert(name.clone(), called);
                    direct_globals.insert(name, mentioned);
                }
                // A function the converter could not read is still a
                // function: a call of it is a call into the block, with
                // what its first line says it takes.
                let toks = &lexed.toks[unit.piece.from..unit.piece.to];
                if let Some(name) = unread_function(toks) {
                    let found = signature(toks, &mut names.clone());
                    let read = found.as_ref().is_some_and(|(found, _, _)| *found == name);
                    let (ret, params) = found.map(|(_, ret, params)| (ret, params)).unwrap_or((CTy::Void, Vec::new()));
                    let kinds = params
                        .iter()
                        .map(|param| match &param.ty {
                            CTy::Ptr(item, _) if matches!(**item, CTy::Int(IntTy { char: true, .. })) => Kind::Text,
                            CTy::Ptr(item, _) if matches!(**item, CTy::Struct(_)) => Kind::Ref,
                            CTy::Ptr(..) => Kind::Box,
                            _ => Kind::Value,
                        })
                        .collect();
                    let changes = vec![true; params.len()];
                    world.functions.entry(name).or_insert(FnInfo { unit: at, ret, params, kinds, globals: HashSet::new(), changes, read });
                }
            }
            _ => {}
        }
    }
    // The globals each function uses, through the functions it calls.
    let global_names: HashSet<String> = world.globals.keys().cloned().collect();
    let function_names: Vec<String> = world.functions.keys().cloned().collect();
    for name in &function_names {
        let mut seen = HashSet::new();
        let mut todo = vec![name.clone()];
        let mut found = HashSet::new();
        while let Some(next) = todo.pop() {
            if !seen.insert(next.clone()) {
                continue;
            }
            if let Some(mentioned) = direct_globals.get(&next) {
                found.extend(mentioned.intersection(&global_names).cloned());
            }
            if let Some(called) = calls.get(&next) {
                todo.extend(called.iter().filter(|callee| world.functions.contains_key(*callee)).cloned());
            }
        }
        world.functions.get_mut(name).unwrap().globals = found;
    }
    for unit in &units {
        if let Ok(Item::Function(f)) = &unit.item {
            world.changed.extend(assigned_names(&f.body).intersection(&global_names).cloned());
        }
    }
    // Macros are C's own: they stay in the block.
    for unit in units.iter_mut() {
        if let Ok(Item::Macro(name)) = &unit.item {
            unit.kept = Some((unit.piece.line, format!("`{name}` is a macro of C, which Zeno does not have; it stays for the C in the block")));
        }
        if let Err(keep) = &unit.item {
            unit.kept = Some((keep.line, keep.why.clone()));
        }
    }

    loop {
        spread(&mut units, &world, &calls, &direct_globals);
        world.kept = units.iter().enumerate().filter(|(_, unit)| unit.kept.is_some()).map(|(at, _)| at).collect();
        // Lines in parts wait in Zeno until they end, so C that is kept
        // must not write between.
        let zeno_parts: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.kept.is_none() && matches!(&unit.item, Ok(Item::Function(f)) if writes_parts(f)))
            .map(|(at, _)| at)
            .collect();
        let c_writes = units.iter().any(|unit| unit.kept.is_some() && matches!(&unit.item, Ok(Item::Function(f)) if writes(f)))
            || units.iter().any(|unit| unit.kept.is_some() && unit.item.is_err() && piece_text(&source_lines, &unit.piece).contains("printf"));
        if !zeno_parts.is_empty() && c_writes {
            for at in zeno_parts {
                let line = units[at].piece.line;
                units[at].kept = Some((line, "it writes parts of lines, which wait in Zeno until the line ends, and C that is kept writes too: the two would not stay in order".into()));
            }
            continue;
        }
        world.buffered = !zeno_parts.is_empty();
        // `c` is a name in Zeno only where the file imports `c`, for a block.
        let block_needed = units.iter().any(|unit| unit.kept.is_some() && matches!(unit.item, Ok(Item::Function(_)) | Ok(Item::Globals(_)) | Err(_)));
        if file_names.contains("c") {
            if block_needed {
                world.renamed.insert("c".to_string(), "c_".to_string());
            } else {
                world.renamed.remove("c");
            }
        }

        let mut written: Vec<Option<Written>> = Vec::new();
        let mut failed: Vec<(usize, Keep)> = Vec::new();
        for (at, unit) in units.iter().enumerate() {
            if unit.kept.is_some() {
                written.push(None);
                continue;
            }
            let inside: Vec<Comment> = lexed.comments.iter().filter(|comment| comment.line >= unit.piece.line && comment.line <= unit.piece.end_line).cloned().collect();
            match write_unit(&world, unit, inside) {
                Ok(done) => written.push(Some(done)),
                Err(keep) => {
                    failed.push((at, keep));
                    written.push(None);
                }
            }
        }
        if !failed.is_empty() {
            for (at, keep) in failed {
                units[at].kept = Some((keep.line, keep.why));
            }
            continue;
        }
        let mut copied: BTreeSet<String> = written.iter().flatten().flat_map(|done| done.copied.iter().cloned()).collect();
        let (text, owners, notes) = match assemble(&units, &written, &world, &lexed, &source_lines, header_end, &mut copied, file) {
            Ok(done) => done,
            Err((at, keep)) => {
                units[at].kept = Some((keep.line, keep.why));
                continue;
            }
        };
        let joined = format!("{}\n", text.join("\n"));
        let found = mistakes(&joined);
        if !found.is_empty() {
            let mut failed: Vec<(usize, String)> = Vec::new();
            for (line, message) in &found {
                if let Some(Some(at)) = owners.get(line.saturating_sub(1)) {
                    if units[*at].kept.is_none() && !failed.iter().any(|(other, _)| other == at) {
                        failed.push((*at, format!("Zeno's checker does not take what the converter wrote for it: {message}")));
                    }
                }
            }
            if failed.is_empty() {
                let (line, message) = &found[0];
                return whole(source, file, format!("what the converter wrote was not sound Zeno (line {line} of it: {message})"));
            }
            for (at, why) in failed {
                let line = units[at].piece.line;
                units[at].kept = Some((line, why));
            }
            continue;
        }
        let zeno = crate::format::format(&joined).unwrap_or(joined);
        let mut noted = notes;
        noted.sort_by_key(|(note, line)| (*line, *note));
        noted.dedup();
        let kept = units
            .iter()
            .filter(|unit| !matches!(unit.item, Ok(Item::Macro(_))) || unit.kept.is_some())
            .filter_map(|unit| unit.kept.as_ref().map(|(at, why)| KeptPiece { line: unit.piece.line, what: unit.label.clone(), at: *at, why: why.clone() }))
            .collect();
        let mut warnings = Vec::new();
        if let Some(info) = world.functions.get("main") {
            if !((info.params.is_empty() || takes_arguments(&info.params)) && info.read) {
                warnings.push("`main` is not called, since the converter could not read how it is declared: the program does nothing until it is called.".to_string());
            }
        }
        let report = Report { pieces: units.len(), noted, kept, whole: None, warnings, language: "C" };
        return Converted { zeno, report };
    }
}

fn comment_line_index(comment: &Comment) -> usize {
    comment.line.saturating_sub(1)
}

/// How many blank lines follow line `after` (counted from 1).
fn blank_run(lines: &[&str], after: usize) -> usize {
    lines.iter().skip(after).take_while(|line| line.trim().is_empty()).count()
}

/// The name of a function whose piece could not be read: the name in
/// front of the first `(` of a piece that ends with a body.
fn unread_function(toks: &[Tok]) -> Option<String> {
    if !matches!(toks.last().map(|tok| &tok.kind), Some(T::Punct("}"))) {
        return None;
    }
    let open = toks.iter().position(|tok| tok.kind == T::Punct("("))?;
    match &toks.get(open.checked_sub(1)?)?.kind {
        T::Name(name) if !is_c_keyword(name) => Some(name.clone()),
        _ => None,
    }
}

fn piece_text(lines: &[&str], piece: &Piece) -> String {
    lines[piece.line - 1..piece.end_line.min(lines.len())].join("\n")
}

/// Keep what a kept piece needs on its side: the functions it calls and
/// the variables it uses.
fn spread(units: &mut [Unit], world: &World, calls: &HashMap<String, HashSet<String>>, mentioned: &HashMap<String, HashSet<String>>) {
    let by_unit: HashMap<usize, &String> = world.functions.iter().map(|(name, info)| (info.unit, name)).collect();
    loop {
        let mut more: Vec<(usize, usize, String)> = Vec::new();
        for (at, unit) in units.iter().enumerate() {
            if unit.kept.is_none() {
                continue;
            }
            let Some(name) = by_unit.get(&at) else { continue };
            for callee in calls.get(*name).into_iter().flatten() {
                if let Some(info) = world.functions.get(callee) {
                    if units[info.unit].kept.is_none() && !more.iter().any(|(at, _, _)| *at == info.unit) {
                        more.push((info.unit, units[info.unit].piece.line, format!("the function `{name}`, which is kept as C, calls it, and C can call only C")));
                    }
                }
            }
            for used in mentioned.get(*name).into_iter().flatten() {
                if let Some(global) = world.globals.get(used) {
                    if global.constant {
                        continue;
                    }
                    if units[global.unit].kept.is_none() && !more.iter().any(|(at, _, _)| *at == global.unit) {
                        more.push((global.unit, units[global.unit].piece.line, format!("the function `{name}`, which is kept as C, uses it, and a variable lives on one side")));
                    }
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

fn write_unit(world: &World, unit: &Unit, comments: Vec<Comment>) -> Kept<Written> {
    let item = unit.item.as_ref().map_err(|keep| keep.clone())?;
    let line = unit.piece.line;
    let mut w = W::new(world, comments);
    match item {
        Item::Include(_) | Item::Prototype(..) | Item::Nothing | Item::Macro(_) => {}
        Item::Define(name, value) => {
            let v = w.expr(value)?;
            let ty = v.ty.clone();
            let code = w.store(v, &ty, line)?;
            let annotation = match &ty {
                CTy::Int(IntTy { signed: false, bits: 8 | 16 | 32, .. }) | CTy::Float(true) => format!(": {}", zeno_type(&ty, line)?),
                _ => String::new(),
            };
            w.line(format!("let {}{annotation} = {code}", world.zeno_name(name)));
        }
        Item::Enum(values) => {
            for (name, value) in values {
                w.line(format!("let {} = {value}", world.zeno_name(name)));
            }
        }
        Item::Struct(..) => {
            // Written with what is copied, once everything is written.
        }
        Item::Globals(vars) => {
            for var in vars {
                let var = Var { stat: false, constant: var.constant || !world.changed.contains(&var.name), ..var.clone() };
                w.global(&var)?;
            }
        }
        Item::Function(f) => {
            let info = &world.functions[&f.name];
            w.function(f, info)?;
        }
    }
    w.rest_of_comments();
    Ok(Written { lines: w.out, helpers: w.helpers, notes: w.notes, imports: w.imports, copied: w.copied })
}

type Assembled = (Vec<String>, Vec<Option<usize>>, Vec<(Note, usize)>);

#[allow(clippy::too_many_arguments)]
fn assemble(
    units: &[Unit],
    written: &[Option<Written>],
    world: &World,
    lexed: &Lexed,
    source_lines: &[&str],
    header_end: usize,
    copied: &mut BTreeSet<String>,
    file: &str,
) -> Result<Assembled, (usize, Keep)> {
    let mut text: Vec<String> = Vec::new();
    let mut owners: Vec<Option<usize>> = Vec::new();
    let push = |text: &mut Vec<String>, owners: &mut Vec<Option<usize>>, line: String, owner: Option<usize>| {
        text.push(line);
        owners.push(owner);
    };
    let mut helpers: BTreeSet<Helper> = BTreeSet::new();
    let mut imports: BTreeSet<&'static str> = BTreeSet::new();
    let mut notes: Vec<(Note, usize)> = Vec::new();
    for done in written.iter().flatten() {
        helpers.extend(done.helpers.iter().copied());
        imports.extend(done.imports.iter().copied());
        notes.extend(done.notes.iter().copied());
    }
    // `main`: called at the end, as C calls it. One that may give back
    // something other than 0 ends the program with it.
    // A `main` that reads the program's arguments is kept, and is given
    // them through `convert_main`.
    let arguments = world.functions.get("main").is_some_and(|info| takes_arguments(&info.params));
    let main = world.functions.get("main").map(|info| {
        let unit = &units[info.unit];
        let only_zero = match &unit.item {
            Ok(Item::Function(f)) => returns_only_zero(&f.body),
            _ => false,
        };
        (info.unit, (info.params.is_empty() || arguments) && info.read, only_zero)
    });
    let main_kept = main.is_some_and(|(at, _, _)| units[at].kept.is_some());
    let main_status = main.is_some_and(|(_, callable, only_zero)| callable && !only_zero);
    if main_status || arguments {
        imports.insert("program");
    }
    // `argv[0]` is the file's name: a note where `main` looks at `argv`.
    let reads_argv = arguments
        && main.is_some_and(|(at, _, _)| {
            let argv = &world.functions["main"].params[1].name;
            let piece = &units[at].piece;
            lexed.toks[piece.from..piece.to].iter().filter(|tok| matches!(&tok.kind, T::Name(name) if name == argv)).count() > 1
        });
    if reads_argv {
        if let Some((at, _, _)) = main {
            notes.push((Note::ProgramName, units[at].piece.line));
        }
    }
    if world.buffered {
        helpers.insert(Helper::Text);
        if let Some((at, _, _)) = main {
            notes.push((Note::LastLine, units[at].piece.line));
        }
    }
    let block_needed = units.iter().any(|unit| unit.kept.is_some() && matches!(unit.item, Ok(Item::Function(_)) | Ok(Item::Globals(_)) | Err(_)));
    if block_needed {
        imports.insert("c");
    }

    // The comment at the top of the file.
    let header: Vec<&Comment> = lexed.comments.iter().filter(|comment| comment.line <= header_end).collect();
    for comment in &header {
        for line in &comment.lines {
            push(&mut text, &mut owners, format!("#{line}"), None);
        }
    }
    if !header.is_empty() {
        push(&mut text, &mut owners, String::new(), None);
    }
    for name in ["c", "math", "program"] {
        if imports.contains(name) {
            push(&mut text, &mut owners, format!("import {name}"), None);
        }
    }
    if !imports.is_empty() {
        push(&mut text, &mut owners, String::new(), None);
    }

    if block_needed {
        push(&mut text, &mut owners, format!("let {} = inline c {{", world.block), None);
        let mut done_lines: HashSet<usize> = HashSet::new();
        let mut first = true;
        for unit in units {
            let kept = unit.kept.is_some();
            let copy = match &unit.item {
                Ok(Item::Include(_)) | Ok(Item::Define(..)) | Ok(Item::Macro(_)) | Ok(Item::Struct(..)) | Ok(Item::Enum(_)) | Ok(Item::Nothing) => true,
                Ok(Item::Prototype(name, ..)) => world.functions.get(name).is_none_or(|info| units[info.unit].kept.is_some()),
                Ok(Item::Globals(vars)) => kept || vars.iter().all(|var| var.constant),
                Ok(Item::Function(_)) | Err(_) => kept,
            };
            if !copy {
                continue;
            }
            let from = if kept && !matches!(unit.item, Ok(Item::Include(_)) | Ok(Item::Define(..)) | Ok(Item::Macro(_))) { unit.lead } else { unit.piece.line };
            let is_function = matches!(unit.item, Ok(Item::Function(_))) || (unit.item.is_err() && unread_function(&lexed.toks[unit.piece.from..unit.piece.to]).is_some());
            if is_function && !first && text.last().is_some_and(|line| !line.is_empty()) {
                push(&mut text, &mut owners, String::new(), None);
            }
            let mut stripped = false;
            for number in from..=unit.piece.end_line.min(source_lines.len()) {
                if !done_lines.insert(number) {
                    continue;
                }
                let mut line = source_lines[number - 1].to_string();
                // A function Zeno calls cannot be the block's own.
                if is_function && !stripped && number >= unit.piece.line {
                    if let Some(rest) = line.strip_prefix("static ") {
                        line = rest.to_string();
                    }
                    stripped = true;
                }
                push(&mut text, &mut owners, if line.trim().is_empty() { String::new() } else { format!("    {line}") }, None);
            }
            first = false;
            if is_function && text.last().is_some_and(|line| !line.is_empty()) {
                push(&mut text, &mut owners, String::new(), None);
            }
        }
        while text.last().is_some_and(|line| line.is_empty()) {
            text.pop();
            owners.pop();
        }
        if arguments {
            push(&mut text, &mut owners, String::new(), None);
            for line in ARGUMENTS_C {
                push(&mut text, &mut owners, if line.is_empty() { String::new() } else { format!("    {line}") }, None);
            }
        }
        push(&mut text, &mut owners, "}".to_string(), None);
        push(&mut text, &mut owners, String::new(), None);
    }

    for helper in &helpers {
        for line in helper.lines() {
            push(&mut text, &mut owners, line.to_string(), None);
        }
        push(&mut text, &mut owners, String::new(), None);
    }

    let mut previous_blank = true;
    for (at, unit) in units.iter().enumerate() {
        let mut lines: Vec<String> = Vec::new();
        // The comments right above it, and those between its lines.
        if unit.kept.is_none() || matches!(unit.item, Ok(Item::Include(_)) | Ok(Item::Define(..)) | Ok(Item::Struct(..)) | Ok(Item::Enum(_)) | Ok(Item::Nothing) | Ok(Item::Prototype(..)) | Ok(Item::Macro(_))) {
            for comment in lexed.comments.iter().filter(|comment| comment.line >= unit.lead && comment.line < unit.piece.line && comment.line > header_end) {
                for line in &comment.lines {
                    lines.push(format!("#{line}"));
                }
            }
        }
        if let Some((_, _why)) = &unit.kept {
            match &unit.item {
                Ok(Item::Function(f)) => lines.push(format!("# `{}` is kept as C, in the block above.", f.name)),
                Ok(Item::Globals(vars)) if vars.iter().all(|var| var.constant) => {}
                Ok(Item::Globals(vars)) => {
                    lines.push(format!("# {} kept as C, in the block above.", unit.label.clone() + if vars.len() == 1 { " is" } else { " are" }))
                }
                Err(_) => lines.push(format!("# {} is kept as C, in the block above.", unit.label)),
                _ => {}
            }
        }
        if let Ok(Item::Struct(name, fields)) = &unit.item {
            if unit.kept.is_none() {
                match class_lines(world, name, fields, copied, unit.piece.line) {
                    Ok(class) => lines.extend(class),
                    Err(keep) => return Err((at, keep)),
                }
            }
        }
        if let Some(Some(done)) = written.get(at) {
            lines.extend(done.lines.iter().cloned());
        }
        if lines.is_empty() {
            continue;
        }
        let is_block = matches!(unit.item, Ok(Item::Function(_)) | Ok(Item::Struct(..)));
        if (is_block || unit.lead < unit.piece.line || gap_before(units, at, source_lines)) && !previous_blank {
            push(&mut text, &mut owners, String::new(), None);
        }
        for line in lines {
            push(&mut text, &mut owners, line, Some(at));
        }
        previous_blank = false;
        if is_block {
            push(&mut text, &mut owners, String::new(), None);
            previous_blank = true;
        }
    }

    if let Some((at, callable, _)) = main {
        if !previous_blank {
            push(&mut text, &mut owners, String::new(), None);
        }
        let callee = if arguments {
            format!("{}.convert_main", world.block)
        } else if main_kept {
            format!("{}.main", world.block)
        } else {
            world.zeno_name("main")
        };
        if arguments && callable {
            if reads_argv {
                push(&mut text, &mut owners, format!("{NOTE_MARK}{}", Note::ProgramName as u8), Some(at));
            }
            for line in argument_lines(&world.block, file) {
                push(&mut text, &mut owners, line, Some(at));
            }
        }
        if !callable {
            push(&mut text, &mut owners, "# `main` is not called: the converter could not read how it is declared.".to_string(), Some(at));
        } else if main_status {
            push(&mut text, &mut owners, format!("let exit_status = {callee}()"), Some(at));
            if world.buffered {
                push(&mut text, &mut owners, format!("{NOTE_MARK}{}", Note::LastLine as u8), Some(at));
                push(&mut text, &mut owners, "finish_text()".to_string(), Some(at));
            }
            push(&mut text, &mut owners, "if exit_status != 0 {".to_string(), Some(at));
            push(&mut text, &mut owners, "    program.exit(exit_status)".to_string(), Some(at));
            push(&mut text, &mut owners, "}".to_string(), Some(at));
        } else {
            push(&mut text, &mut owners, format!("{callee}()"), Some(at));
            if world.buffered {
                push(&mut text, &mut owners, format!("{NOTE_MARK}{}", Note::LastLine as u8), Some(at));
                push(&mut text, &mut owners, "finish_text()".to_string(), Some(at));
            }
        }
    }
    let last_line = units.last().map(|unit| unit.piece.end_line).unwrap_or(0);
    let tail: Vec<&Comment> = lexed.comments.iter().filter(|comment| comment.line > last_line).collect();
    if !tail.is_empty() {
        push(&mut text, &mut owners, String::new(), None);
        for comment in tail {
            for line in &comment.lines {
                push(&mut text, &mut owners, format!("#{line}"), None);
            }
        }
    }
    while text.last().is_some_and(|line| line.is_empty()) {
        text.pop();
        owners.pop();
    }
    // The first mark of each note becomes its comment; the rest go.
    const NOTES: [Note; 10] = [Note::Shown, Note::Exponent, Note::ErrorWords, Note::Catches, Note::Letters, Note::Half, Note::FloatStart, Note::Narrow, Note::LastLine, Note::ProgramName];
    let mut seen: HashSet<Note> = HashSet::new();
    let mut out_text = Vec::new();
    let mut out_owners = Vec::new();
    for (line, owner) in text.into_iter().zip(owners) {
        let trimmed = line.trim_start();
        let Some(kind) = trimmed.strip_prefix(NOTE_MARK) else {
            out_text.push(line);
            out_owners.push(owner);
            continue;
        };
        let indent = &line[..line.len() - trimmed.len()];
        let Some(note) = NOTES.iter().find(|note| (**note as u8).to_string() == kind) else { continue };
        if seen.insert(*note) {
            for comment in note.lines(&program_name(file)) {
                out_text.push(format!("{indent}# {comment}"));
                out_owners.push(owner);
            }
        }
    }
    Ok((out_text, out_owners, notes))
}

/// Whether the C had a blank line in front of this piece.
fn gap_before(units: &[Unit], at: usize, lines: &[&str]) -> bool {
    if at == 0 {
        return false;
    }
    let start = units[at].lead;
    start >= 2 && lines.get(start - 2).is_some_and(|line| line.trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn converted(source: &str) -> Converted {
        let done = convert(source, "test.c");
        assert!(done.report.whole.is_none(), "the whole file was kept: {:?}\n{}", done.report.whole, done.zeno);
        assert!(mistakes(&done.zeno).is_empty(), "the converter wrote what Zeno's checker refuses:\n{}", done.zeno);
        done
    }

    fn kept_lines(done: &Converted) -> Vec<usize> {
        done.report.kept.iter().map(|piece| piece.line).collect()
    }

    #[test]
    fn a_program_zeno_has_words_for_is_zeno_through_and_through() {
        let done = converted("#include <stdio.h>\n\nint twice(int n) {\n    return n * 2;\n}\n\nint main(void) {\n    int total = 0;\n    for (int i = 0; i < 4; i++) {\n        total += twice(i);\n    }\n    printf(\"%d\\n\", total);\n    return 0;\n}\n");
        assert!(done.report.kept.is_empty(), "{:?}", done.report.kept);
        assert!(done.zeno.contains("function twice(n: int) -> int {"), "{}", done.zeno);
        assert!(done.zeno.contains("for i in range(0, 4) {"), "{}", done.zeno);
        assert!(done.zeno.contains("print(f\"{total}\")"), "{}", done.zeno);
        assert!(done.zeno.trim_end().ends_with("main()"), "{}", done.zeno);
    }

    #[test]
    fn what_cannot_be_read_is_kept_and_called() {
        let done = converted("#include <stdio.h>\n\nint step(int x) {\n    goto done;\ndone:\n    return x + 1;\n}\n\nint main(void) {\n    printf(\"%d\\n\", step(2));\n    return 0;\n}\n");
        assert_eq!(kept_lines(&done), vec![3]);
        assert!(done.zeno.contains("let c_part = inline c {"), "{}", done.zeno);
        assert!(done.zeno.contains("c_part.step(2)"), "{}", done.zeno);
    }

    #[test]
    fn division_takes_the_quotient_toward_zero() {
        let done = converted("int f(int a, int b) {\n    return a / b + a % b;\n}\nint g(unsigned a) {\n    return (int)(a / 2u);\n}\n");
        assert!(done.zeno.contains("c_divide(a, b) + c_remainder(a, b)"), "{}", done.zeno);
        assert!(done.zeno.contains("function c_divide"), "{}", done.zeno);
    }

    #[test]
    fn an_unsigned_sum_goes_round() {
        let done = converted("unsigned hash(const char *text) {\n    unsigned h = 5381;\n    for (int i = 0; text[i] != 0; i++) {\n        h = h * 33 + text[i];\n    }\n    return h;\n}\n");
        assert!(done.zeno.contains("h = u32.wrap(h * 33 + letter_code(text, i))"), "{}", done.zeno);
    }

    #[test]
    fn a_pointer_to_one_number_is_a_list_of_one() {
        let done = converted("void swap(int *a, int *b) {\n    int t = *a;\n    *a = *b;\n    *b = t;\n}\nint main(void) {\n    int x = 1, y = 2;\n    swap(&x, &y);\n    return x;\n}\n");
        assert!(done.report.kept.is_empty(), "{:?}", done.report.kept);
        assert!(done.zeno.contains("function swap(a: list[int], b: list[int]) {"), "{}", done.zeno);
        assert!(done.zeno.contains("let x_box = [x]"), "{}", done.zeno);
        assert!(done.zeno.contains("x = x_box[0]"), "{}", done.zeno);
    }

    #[test]
    fn a_struct_is_a_class_and_is_copied_as_c_copies_it() {
        let done = converted("struct Point { int x; int y; };\nint main(void) {\n    struct Point a = {1, 2};\n    struct Point b = a;\n    b.x = 5;\n    return a.x;\n}\n");
        assert!(done.zeno.contains("class Point {"), "{}", done.zeno);
        assert!(done.zeno.contains("var b = a.copy()") || done.zeno.contains("let b = a.copy()"), "{}", done.zeno);
        assert!(done.zeno.contains("function copy(self) -> Point {"), "{}", done.zeno);
    }

    #[test]
    fn a_kept_function_keeps_what_it_calls_and_the_variables_it_uses() {
        let done = converted("int count = 0;\nint bump(void) {\n    count++;\n    return count;\n}\nint twice(void) {\n    int *p = &count;\n    return bump() + *p;\n}\nint main(void) {\n    return twice();\n}\n");
        assert_eq!(kept_lines(&done), vec![1, 2, 6]);
    }

    #[test]
    fn a_line_written_in_parts_waits_for_its_end() {
        let done = converted("#include <stdio.h>\nint main(void) {\n    for (int i = 0; i < 3; i++) {\n        printf(\"%d \", i);\n    }\n    printf(\"\\n\");\n    return 0;\n}\n");
        assert!(done.zeno.contains("write_text(f\"{i} \")"), "{}", done.zeno);
        assert!(done.zeno.contains("finish_text()"), "{}", done.zeno);
    }

    #[test]
    fn a_file_the_preprocessor_changes_is_kept_whole() {
        let done = convert("#ifdef DEBUG\nint x;\n#endif\nint main(void) { return 0; }\n", "test.c");
        assert!(done.report.whole.is_some());
        assert!(done.zeno.contains("c_part.main()"));
    }

    #[test]
    fn a_main_that_reads_its_arguments_is_given_them() {
        let source = "#include <stdio.h>\nint main(int argc, char **argv) {\n    printf(\"%s %d\\n\", argv[0], argc);\n    return 0;\n}\n";
        let done = convert(source, "tool.c");
        assert!(done.zeno.contains("int convert_main(void) {"), "{}", done.zeno);
        assert!(done.zeno.contains("c_part.convert_argument(\"tool\")"), "{}", done.zeno);
        assert!(done.zeno.contains("for word in program.arguments() {"), "{}", done.zeno);
        assert!(done.zeno.contains("c_part.convert_main()"), "{}", done.zeno);
        assert!(done.report.noted.iter().any(|(note, _)| *note == Note::ProgramName));
        assert!(done.report.warnings.is_empty());
    }
}
