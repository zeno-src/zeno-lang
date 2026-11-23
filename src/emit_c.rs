//! Stage 5: the C backend. It turns a checked program into C source.
//!
//! ```text
//! tokens -> parser -> tree -> checker -> [emit_c] -> C source -> C compiler -> program
//! ```
//!
//! The interpreter walks the tree and *does* what each node says. This stage
//! walks the same tree and *writes down*, in C, what each node says. The two
//! must agree: a program has to print the same thing whether it goes through
//! `zeno run` or `zeno build`. The tests at the bottom hold them to that.
//!
//! Three things make this more than copying the program out with different
//! spelling:
//!
//! 1. **C needs every type; Zeno lets you leave them out.** The checker has
//!    already worked out what every function takes and returns, and hands
//!    that over in `Types`. From there the type of everything else follows:
//!    a literal knows its own type, a variable has the type of its first
//!    value, and `a + b` has the type its two sides give it.
//! 2. **C is looser than Zeno.** Overflow, division by zero and the like are
//!    errors in Zeno and undefined in C. So arithmetic goes through small
//!    checked functions in `runtime.c`.
//! 3. **C does not say which argument of `f(a(), b())` runs first.** Zeno
//!    does: left to right. Where the order can matter, the pieces are worked
//!    out one at a time into temporaries. See `sequence`.
//!
//! It compiles numbers, bools, strings, quantities, functions, control flow,
//! classes, lists, enums and `match`, values that may be `none`, errors
//! (`try`, `except`, `raise`), and functions used as values. The few things
//! it does not compile yet are refused with a message that says so.
//!
//! `zeno build --firmware` compiles errors a second way, as values that
//! each caller tests, with no `setjmp`. Look for "Errors as values".

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::ast::*;
use crate::checker::{spot, FunctionKind, Kind, Pointee, Types};
use crate::error::Error;
use crate::ints::{self, Width};
use crate::test_build::TestBuild;
use crate::token::Span;
use crate::units;

/// The runtime library, pasted in front of every compiled program.
const RUNTIME: &str = include_str!("runtime.c");

/// The three ways `zeno build` can compile a program.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Build {
    /// `zeno build`: errors jump (`setjmp`), and a collector frees memory.
    Ordinary,
    /// `zeno build --firmware`: errors travel as values that each caller
    /// tests, with no `setjmp`. The collector is still there.
    Firmware,
    /// `zeno build --no-collector`: a firmware build with no collector in
    /// it. Every list and object has one owner, whoever received it. See
    /// docs/no-collector.md and docs/one-owner.md.
    NoCollector,
}

impl Build {
    /// How the checker checks a program for this kind of build. Compiled
    /// code has no room for a value of unknown type, so every build is
    /// strict. With no collector, every list and object has one owner.
    pub fn rules(self) -> crate::checker::Rules {
        crate::checker::Rules { strict: true, one_owner: self == Build::NoCollector, firmware: self != Build::Ordinary }
    }
}

/// The runtime as one kind of build needs it. The runtime marks what only
/// some builds use with `#ifdef ZENO_FIRMWARE` and `#ifdef
/// ZENO_NO_COLLECTOR`. The choice is made here, so that the C file holds
/// what the program uses and nothing else. (That matters to a reviewer,
/// who must not find `setjmp` anywhere in a firmware build, nor a
/// collector in a build that says it has none.)
#[cfg(test)]
pub fn runtime(firmware: bool, collector: bool) -> String {
    runtime_for(firmware, collector, false)
}

/// The same, saying also whether the pool and the text room of a build
/// with no collector grow (`memory owned` on a computer, marked `#ifdef
/// ZENO_HEAP`) or are fixed arrays (`--no-collector`, for a chip).
pub fn runtime_for(firmware: bool, collector: bool, grows: bool) -> String {
    runtime_of(firmware, collector, grows, false)
}

/// The parts of the runtime that call a function of the board layer
/// beyond the first four, each marked `#ifdef` its name: input pins
/// (decision 45.4) and serial ports (46.5).
pub const BOARD_PARTS: &[&str] = &["ZENO_INPUT_PINS", "ZENO_SERIAL_PORTS", "ZENO_ANALOG_PINS", "ZENO_PWM_PINS", "ZENO_TICKING", "ZENO_INTERRUPTS"];

/// `runtime` with each part of `BOARD_PARTS` kept when the program uses
/// it (`used`) and left out when it does not. Only those parts call the
/// functions a board header did not always have to write, so a header
/// written before them still builds every program that uses none, and
/// the C of such a program is what it was before.
pub fn with_board_parts(runtime: &str, used: &[&str]) -> String {
    with_marked(runtime, BOARD_PARTS, used)
}

/// The pieces of the runtime that are written only for a program that
/// uses them, each marked `#ifdef` its name in runtime.c, and the function
/// of the runtime by which a program uses it. A program that uses none
/// is given the runtime that 0.7.0 wrote, to the byte, so `tools/same.py`
/// against the tag sees every program that uses nothing new as it was,
/// and a board's flash holds nothing it was not asked for, whatever the C
/// compiler drops (decision 56.19). Whatever is added to the runtime from
/// now on goes in a piece: a line here, `#ifdef` and `#endif /* name */`
/// around it in runtime.c, and `uses_piece` where the C is written. LLVM
/// finds the pieces by the functions its IR declares (`pieces_in_ir`).
pub const PIECES: &[(&str, &str)] = &[
    ("ZENO_RANGE_LIST", "zeno_range_list"),
    ("ZENO_FILES_LIST", "zeno_files_list"),
    ("ZENO_FILES_MAKE_FOLDER", "zeno_files_make_folder"),
    ("ZENO_FILES_PIECES", "zeno_files_handle_open"),
];

/// Are these the kinds of what `files.{name}` is given, for one of the
/// built-ins under a `files.Reader` or a `files.Writer` (decision 59.6)?
pub fn handle_arguments(name: &str, kinds: &[&Kind]) -> bool {
    let bytes = Kind::List(Box::new(Kind::Byte));
    match name {
        "handle_open" => matches!(kinds[..], [Kind::Str, Kind::Int]),
        "handle_line" => matches!(kinds[..], [Kind::Int, Kind::Str]),
        "handle_bytes" => matches!(kinds[..], [Kind::Int, Kind::Str, Kind::Int]),
        "handle_write" => matches!(kinds[..], [Kind::Int, Kind::Str, Kind::Str]),
        "handle_write_bytes" => matches!(kinds[..], [Kind::Int, Kind::Str, written] if *written == bytes),
        _ => false,
    }
}

/// `runtime` with each piece of `PIECES` that `used` names, and no other.
pub fn with_pieces(runtime: &str, used: &[&str]) -> String {
    let names: Vec<&str> = PIECES.iter().map(|(name, _)| *name).collect();
    with_marked(runtime, &names, used)
}

/// The pieces of `PIECES` that a program of LLVM IR uses: those whose
/// function it declares.
pub fn pieces_in_ir(ir: &str) -> Vec<&'static str> {
    PIECES.iter().filter(|(_, function)| ir.contains(&format!("@{function}("))).map(|(name, _)| *name).collect()
}

/// `runtime` with each part marked `#ifdef` one of `names` ... `#endif /*
/// name */` kept, without the two lines, when `used` names it, and left
/// out when it does not.
fn with_marked(runtime: &str, names: &[&str], used: &[&str]) -> String {
    let mut text = String::with_capacity(runtime.len());
    let mut inside: Option<&str> = None;
    for line in runtime.lines() {
        let trimmed = line.trim();
        let part = |prefix: &str, suffix: &str| {
            names.iter().copied().find(|part| trimmed.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) == Some(part))
        };
        if let Some(part) = part("#ifdef ", "") {
            inside = Some(part);
        } else if part("#endif /* ", " */").is_some() {
            inside = None;
        } else if inside.is_none_or(|part| used.contains(&part)) {
            text += line;
            text.push('\n');
        }
    }
    text
}

/// The parts of `BOARD_PARTS` that a program of LLVM IR uses, by the
/// functions of the runtime it calls. (What shows a port or a pin,
/// `zeno_str_serial`, is in no part, and begins differently.)
pub fn board_parts_in_ir(ir: &str) -> Vec<&'static str> {
    let mut used = Vec::new();
    if ir.contains("@zeno_input_pin(") {
        used.push("ZENO_INPUT_PINS");
    }
    if ir.contains("@zeno_serial") {
        used.push("ZENO_SERIAL_PORTS");
    }
    if ir.contains("@zeno_analog_") {
        used.push("ZENO_ANALOG_PINS");
    }
    if ir.contains("@zeno_pwm_") {
        used.push("ZENO_PWM_PINS");
    }
    if ir.contains("@zeno_ticker") {
        used.push("ZENO_TICKING");
    }
    if ir.contains("@zeno_interrupt") {
        used.push("ZENO_INTERRUPTS");
    }
    used
}

/// The runtime of a build with no collector, made to look for a write to
/// a block after it was given back (decision 56.15). `zeno build
/// --memory` asks for it. A block that is given back is filled with one
/// byte past the link to the next spare block, and when it is handed out
/// again the byte is looked for, and the link must lead to a spare block
/// of the same size in the pool. A write that changed either stops the
/// program. All of it is behind `#ifdef ZENO_MEMORY_REPORT`, so a build
/// without `--memory` has not a byte of it; and it is written only into
/// a build that asks, so the C of every other build is what it was.
pub fn with_spares_checked(runtime: &str) -> String {
    const PROTOTYPES: &str = "#ifdef ZENO_MEMORY_REPORT
/* `zeno build --memory`: spare blocks are filled, and looked at when they
 * are handed out again (zeno_spare_check, below). */
static void zeno_spare_fill(zeno_spare *block);
static void zeno_spare_check(zeno_spare *block);
#endif
";
    const DEFINITIONS: &str = "#ifdef ZENO_MEMORY_REPORT
/* A block that is given back is filled with this byte, after the link to
 * the next spare block, so that a write to it after it was given back can
 * be seen when it is handed out again. */
#define ZENO_SPARE_BYTE 0xA5

static void zeno_spare_fill(zeno_spare *block) {
    unsigned char *end = (unsigned char *)block + sizeof(zeno_head) + ((size_t)16 << block->head.step);
    for (unsigned char *byte = (unsigned char *)(block + 1); byte < end; byte++) *byte = ZENO_SPARE_BYTE;
}

/* The block is about to be handed out again. Every byte after the link is
 * as zeno_spare_fill left it, and the link is either the end of the list
 * or a spare block of the same size in the pool. Anything else was
 * written by a line that still had the block's address after it was
 * given back. */
static void zeno_spare_check(zeno_spare *block) {
    unsigned char *end = (unsigned char *)block + sizeof(zeno_head) + ((size_t)16 << block->head.step);
    bool untouched = true;
    for (unsigned char *byte = (unsigned char *)(block + 1); byte < end; byte++) {
        if (*byte != ZENO_SPARE_BYTE) untouched = false;
    }
    zeno_spare *next = block->next;
    if (next != NULL && !(zeno_in_pool((const char *)next) && next->head.step == block->head.step && next->head.asked == 0)) {
        untouched = false;
    }
    if (!untouched) {
        zeno_sayf(\"error: a block of memory was written to after it was given back\\n\");
        zeno_stop(1);
    }
}
#endif

";
    let places = [
        ("} zeno_spare;\n", "} zeno_spare;\n".to_string() + PROTOTYPES),
        (
            "    if (block != NULL) {\n        zeno_pool_spare[step] = block->next;\n",
            "    if (block != NULL) {\n#ifdef ZENO_MEMORY_REPORT\n        zeno_spare_check(block);\n#endif\n        zeno_pool_spare[step] = block->next;\n"
                .to_string(),
        ),
        (
            "    block->head.asked = 0;\n    block->next = zeno_pool_spare[block->head.step];\n",
            "    block->head.asked = 0;\n#ifdef ZENO_MEMORY_REPORT\n    zeno_spare_fill(block);\n#endif\n    block->next = zeno_pool_spare[block->head.step];\n"
                .to_string(),
        ),
        ("/* `text` is about to be kept in a variable, a field or a list.", DEFINITIONS.to_string() + "/* `text` is about to be kept in a variable, a field or a list."),
    ];
    let mut text = runtime.to_string();
    for (place, with) in places {
        assert_eq!(text.matches(place).count(), 1, "the runtime of a build with no collector has one `{place}`");
        text = text.replace(place, &with);
    }
    text
}

/// The same, saying also whether it is for `zeno build --minimal`, which
/// leaves the checks out (marked `#ifndef ZENO_MINIMAL`).
fn runtime_of(firmware: bool, collector: bool, grows: bool, minimal: bool) -> String {
    let mut text = String::new();
    // One entry for each marked part the line is inside: is it being kept?
    let mut keeping: Vec<bool> = Vec::new();
    for line in RUNTIME.lines() {
        match line.trim() {
            "#ifdef ZENO_FIRMWARE" => keeping.push(firmware),
            "#ifndef ZENO_FIRMWARE" => keeping.push(!firmware),
            "#ifdef ZENO_NO_COLLECTOR" => keeping.push(!collector),
            "#ifndef ZENO_NO_COLLECTOR" => keeping.push(collector),
            "#ifdef ZENO_HEAP" => keeping.push(grows),
            "#ifndef ZENO_HEAP" => keeping.push(!grows),
            // What only `zeno build --minimal` leaves out or changes
            // (src/minimal.rs). Here it is kept as it always was.
            "#ifdef ZENO_MINIMAL" => keeping.push(minimal),
            "#ifndef ZENO_MINIMAL" => keeping.push(!minimal),
            "#else /* ZENO_FIRMWARE */" | "#else /* ZENO_NO_COLLECTOR */" | "#else /* ZENO_HEAP */" | "#else /* ZENO_MINIMAL */" => {
                if let Some(kept) = keeping.last_mut() {
                    *kept = !*kept;
                }
            }
            "#endif /* ZENO_FIRMWARE */"
            | "#endif /* ZENO_NO_COLLECTOR */"
            | "#endif /* ZENO_HEAP */"
            | "#endif /* ZENO_MINIMAL */" => {
                keeping.pop();
            }
            _ if keeping.iter().all(|kept| *kept) => {
                text += line;
                text.push('\n');
            }
            _ => {}
        }
    }
    text
}

/// What the C compiler is asked to link beside the C library.
///
/// The runtime uses `pow`, `floor` and the rest of C's maths. On Linux
/// they are in a library of their own, asked for with `-lm`, and macOS
/// accepts the same. On Windows they are part of the C library, and
/// Visual Studio's linker, asked for `-lm`, looks for a file `m.lib` and
/// stops when there is none.
pub const LIBRARIES: &[&str] = if cfg!(windows) { &[] } else { &["-lm"] };

/// The file a build makes: `hello`, and on Windows `hello.exe`, which is
/// the only name Windows will run. (gcc adds the `.exe` by itself, and
/// Visual Studio's clang does not, so it is written out for both.)
pub fn program_file(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Turn a checked program into the text of one C file. `file` is the name of
/// the Zeno source, which the compiled program uses in its error messages.
/// `build` says which of the three kinds of build this is.
pub fn emit(program: &Program, types: &Types, file: &str, build: Build) -> Result<String, Error> {
    emit_with(program, types, file, build, Options::default())
}

/// What a build may do beyond its kind (docs/memory-modes.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// `--release`: `+`, `-` and `*` on whole numbers wrap round on
    /// overflow, as the processor does, instead of stopping the program.
    pub release: bool,
    /// With no collector: the pool and the text room grow as the program
    /// needs (`memory owned`), instead of being fixed arrays for a chip
    /// (`--no-collector`).
    pub grows: bool,
    /// `--fast`: `--release`, and the checks a correct program never
    /// fails are left out too: list indexes, a float divided by zero, the
    /// square root of a number below zero (decision 23.1). The C says so
    /// in its first line, `#define ZENO_UNCHECKED 1`, so the file builds
    /// the same by hand. Whole-number `+`, `-` and `*` are C's own signed
    /// ones, which tell the C compiler that they do not overflow
    /// (decision 23.6).
    pub fast: bool,
    /// `--minimal`: the least C that runs the program (decisions chapter
    /// 30, src/minimal.rs). No check of any kind is written, nothing is
    /// told where in the source it is, only the parts of the runtime
    /// that the program uses are in the file, and `print` is C's own
    /// `printf` where it can be.
    pub minimal: bool,
    /// `--memory`, which a minimal build has to know of when the C is
    /// written: it leaves out the lines that count memory unless they
    /// are asked for. Every other build writes them behind
    /// `#ifdef ZENO_MEMORY_REPORT` and lets the C compiler choose.
    pub memory_report: bool,
    /// `--memory` with no collector: a block that is given back is
    /// filled, and looked at when it is handed out again
    /// (`with_spares_checked`, decision 56.15). Not in a minimal build,
    /// which writes no check of any kind.
    pub spares_checked: bool,
    /// `--board`: `print` goes through the runtime in a minimal build
    /// too, and not to `printf`, which a board may not have and which
    /// costs a chip 20 KB of flash where it does (decision 52.4).
    pub board_prints: bool,
    /// `--board`, for a board with a header of its own: the header says
    /// whether the board or the C library prints, reads and stops, so a
    /// minimal build writes both halves and the header with them.
    pub board_header: bool,
}

/// `emit`, with the options of `Options`.
pub fn emit_with(program: &Program, types: &Types, file: &str, build: Build, options: Options) -> Result<String, Error> {
    emit_all(program, types, file, build, options, None)
}

/// `emit_with`, for `zeno test --build`: the program, with each of its
/// tests run after it and reported as `zeno test` reports it (decision
/// 60.2). The tests are functions of the program by now (see
/// `test_build::with_tests`).
pub fn emit_tests(program: &Program, types: &Types, file: &str, build: Build, options: Options, tests: &TestBuild) -> Result<String, Error> {
    emit_all(program, types, file, build, options, Some(tests))
}

fn emit_all(program: &Program, types: &Types, file: &str, build: Build, options: Options, tests: Option<&TestBuild>) -> Result<String, Error> {
    let c = emit_whole(program, types, file, build, options, tests)?;
    // The file of C this becomes: `sensor.c` for `sensor.zeno`, and
    // `sensor-tests.c` for its tests.
    let name = std::path::Path::new(file).file_stem().and_then(|name| name.to_str()).unwrap_or("program");
    let c_file = format!("{name}{}.c", if tests.is_some() { "-tests" } else { "" });
    Ok(with_blocks_placed(c, types, &c_file))
}

fn emit_whole(program: &Program, types: &Types, file: &str, build: Build, options: Options, tests: Option<&TestBuild>) -> Result<String, Error> {
    let firmware = build != Build::Ordinary;
    let collector = build != Build::NoCollector;
    let mut emitter = Emitter {
        release: options.release || options.fast || options.minimal,
        fast: options.fast || options.minimal,
        minimal: options.minimal,
        board_prints: options.board_prints,
        board_parts: Vec::new(),
        pieces: Vec::new(),
        in_place: false,
        text_stays: false,
        once: None,
        drops: Drops::of(program, types, collector),
        drops_written: 0,
        firmware,
        collector,
        built: false,
        makes_bytes: false,
        marks: 0,
        holds: 0,
        walked: None,
        passing: Vec::new(),
        fallible: false,
        arriving: Vec::new(),
        jumped: HashSet::new(),
        labels: 0,
        types,
        functions: HashMap::new(),
        classes: HashMap::new(),
        enums: HashMap::new(),
        is_a: HashMap::new(),
        numbered: 0,
        cases: HashMap::new(),
        copied_enums: HashSet::new(),
        imported: HashMap::new(),
        scopes: vec![HashMap::new()],
        used: HashMap::new(),
        lines: Vec::new(),
        indent: 1,
        temps: 0,
        typedefs: Vec::new(),
        structs: Vec::new(),
        globals: Vec::new(),
        roots: Vec::new(),
        arguments: false,
        prototypes: Vec::new(),
        definitions: Vec::new(),
        adapters: HashSet::new(),
        returns: None,
        tries: Vec::new(),
        loops: Vec::new(),
        base: 0,
        loop_scopes: Vec::new(),
        try_scopes: Vec::new(),
        has_try: false,
        volatile: HashSet::new(),
        shared: HashSet::new(),
        closures: 0,
        within: 0,
        below: HashSet::new(),
        type_names: HashSet::new(),
        inner_jobs: Vec::new(),
        inner_count: 0,
        inner_frames: Vec::new(),
        inner_seen: Vec::new(),
        callee: None,
        outs: None,
        top_functions: HashMap::new(),
        tests: tests.cloned(),
        tests_run: false,
        resources: Vec::new(),
    };

    // What the top of the file declares. What follows the `let` of a
    // resource there is more of the top of the file (decision 59.1).
    let declared = crate::ast::flat(&program.statements);

    // The names of the types first of all: the C of one class may have to
    // step round the name of a class that is written below it.
    for statement in &declared {
        match &statement.kind {
            StmtKind::Class(decl) if !decl.type_params.is_empty() => {
                for copy in types.class_copies(decl) {
                    emitter.type_names.insert(format!("z_{}", ident(&copy.name)));
                }
            }
            StmtKind::Class(decl) => {
                emitter.type_names.insert(format!("z_{}", ident(&decl.name)));
            }
            StmtKind::Enum(decl) if !decl.type_params.is_empty() => {
                for copy in types.enum_copies(decl) {
                    emitter.type_names.insert(format!("z_{}", ident(&copy.name)));
                }
            }
            StmtKind::Enum(decl) => {
                emitter.type_names.insert(format!("z_{}", ident(&decl.name)));
            }
            _ => {}
        }
    }

    // Classes, enums and functions can be used above where they are written,
    // so learn about all of them before compiling anything. Types go first,
    // because functions mention them.
    for statement in &declared {
        match &statement.kind {
            // A generic class is not compiled. Its copies are, one for
            // each set of types the program uses it with: `Stack[int]`
            // is a class of its own, with its own struct. The checker
            // made the copies, and each is an ordinary class.
            StmtKind::Class(decl) if !decl.type_params.is_empty() => {
                for copy in types.class_copies(decl) {
                    emitter.register_class(copy, statement.span)?;
                }
            }
            StmtKind::Class(decl) => emitter.register_class(decl, statement.span)?,
            // The same for a generic enum: `Maybe[int]` (decision 58.1).
            StmtKind::Enum(decl) if !decl.type_params.is_empty() => {
                for copy in types.enum_copies(decl) {
                    emitter.copied_enums.insert(copy.name.clone());
                    emitter.register_enum(copy, statement.span)?;
                }
            }
            StmtKind::Enum(decl) => emitter.register_enum(decl, statement.span)?,
            _ => {}
        }
    }
    for statement in &declared {
        match &statement.kind {
            // The same for a generic function: `first[int]`, `first[str]`.
            StmtKind::Function(decl) if !decl.type_params.is_empty() => {
                for copy in types.function_copies(decl) {
                    emitter.register(copy, statement.span)?;
                }
            }
            StmtKind::Function(decl) => emitter.register(decl, statement.span)?,
            _ => {}
        }
    }
    emitter.top_functions = emitter.functions.clone();

    // The statements at the top of the file become the body of C's `main`.
    let top: Vec<&Stmt> = program
        .statements
        .iter()
        .filter(|statement| !matches!(statement.kind, StmtKind::Function(_) | StmtKind::Class(_) | StmtKind::Enum(_)))
        .collect();
    for statement in &top {
        if !firmware {
            changed_in_try(statement, false, &mut emitter.volatile);
        }
        names_in_functions(statement, false, &mut emitter.shared);
    }
    emitter.has_try = has_try(&program.statements);
    for statement in &program.statements {
        emitter.stmt(statement)?;
    }
    // The tests run after the program, and see what it made. (With a
    // resource at the top of the file, they ran before it was released.)
    if let (Some(tests), false) = (tests, emitter.tests_run) {
        emitter.run_tests(tests)?;
    }
    // The end of the file is the end of the outermost block.
    emitter.free_owned(0);
    let main = std::mem::take(&mut emitter.lines);
    emitter.inner_functions_now()?;

    // Then each function and method. They are compiled after `main` so that
    // every top-level variable is known by the time a body mentions one.
    for (index, statement) in declared.iter().enumerate() {
        // A function sees the top-level variables declared above it, and
        // not the ones below.
        emitter.below.clear();
        for later in &declared[index + 1..] {
            match &later.kind {
                StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => {
                    emitter.below.insert(name.clone());
                }
                StmtKind::Import { names, .. } => emitter.below.extend(names.iter().cloned()),
                _ => {}
            }
        }
        // What there is to compile here: the function or class itself,
        // or, if it is generic, each of its copies.
        let (functions, classes): (Vec<&Arc<Function>>, Vec<&Class>) = match &statement.kind {
            StmtKind::Function(decl) if !decl.type_params.is_empty() => {
                (types.function_copies(decl).iter().collect(), Vec::new())
            }
            StmtKind::Function(decl) => (vec![decl], Vec::new()),
            StmtKind::Class(class) if !class.type_params.is_empty() => {
                (Vec::new(), types.class_copies(class).iter().map(|copy| &**copy).collect())
            }
            StmtKind::Class(class) => (Vec::new(), vec![class]),
            _ => continue,
        };
        for decl in functions {
            let entry = &emitter.functions[&decl.name];
            let (c_name, kind, owned) = (entry.c_name.clone(), entry.kind.clone(), entry.owned.clone());
            emitter.function(decl, &c_name, &kind, &owned, statement.span)?;
            emitter.inner_functions_now()?;
        }
        for class in classes {
            if class.interface {
                // Nothing to compile: a function of an interface is
                // one that passes the call on to the object's class.
                emitter.pass_on(class, statement.span)?;
                continue;
            }
            for method in &class.methods {
                let entry = &emitter.classes[&class.name].methods[&method.name];
                let (c_name, kind, owned) = (entry.c_name.clone(), entry.kind.clone(), entry.owned.clone());
                emitter.function(method, &c_name, &kind, &owned, statement.span)?;
                emitter.inner_functions_now()?;
            }
        }
    }

    emitter.write_drops(Span { line: 0, col: 0 })?;

    // The libraries of C it declares come after the runtime, and before
    // anything of the program's own: their lines and declarations as they
    // are, and the few functions that pass values to C.
    let (declared, helpers) = c_libraries(types, options.minimal);
    if !options.minimal && !declared.is_empty() {
        emitter.typedefs.insert(0, format!("{declared}{helpers}"));
    }

    // A minimal build is put together from the same pieces another way:
    // only what the program uses is written (src/minimal.rs).
    if options.minimal {
        // The mark that stood for a place in the source goes, with the
        // comma before it. See `Emitter::at`.
        let bare = |text: &String| text.replace(&format!(", {NO_PLACE}"), "");
        // And each statement is written the short way. See `tidy`.
        let tidied = |text: &String| bare(text).lines().map(|line| tidy(line) + "\n").collect::<String>();
        let helpers = bare(&helpers);
        let declarations: Vec<String> =
            std::iter::once(&helpers).chain(&emitter.typedefs).chain(&emitter.structs).chain(&emitter.globals).map(bare).collect();
        let definitions: Vec<String> = emitter.definitions.iter().map(tidied).collect();
        let program = tidied(&render(&main));
        let source = std::path::Path::new(file).file_name().and_then(|name| name.to_str()).unwrap_or(file);
        let runtime = with_pieces(&runtime_of(firmware, collector, options.grows && !collector, true), &emitter.pieces);
        let mut runtime = with_board_parts(&runtime, &emitter.board_parts);
        if !options.board_header {
            // With no header of a board there is nobody but the C library
            // to print, read and stop, and only its half is written. With
            // one, the header says which half it is (decision 52.4).
            runtime = crate::minimal::resolved(&runtime, &[("ZENO_BOARD_PRINTS", false)]);
        }
        return Ok(crate::minimal::assemble(crate::minimal::Parts {
            source,
            runtime: &runtime,
            declarations: declarations.iter().map(String::as_str).collect(),
            functions: definitions.iter().map(String::as_str).collect(),
            program: &program,
            collector,
            roots: &emitter.roots,
            arguments: emitter.arguments,
            memory_report: options.memory_report,
            verbatim: &declared,
        }));
    }

    // One C file: the runtime, then the program's types, its top-level
    // variables, a first line for every function (so that they can call each
    // other in any order), the functions themselves, and `main`.
    let mut runtime = runtime_for(firmware, collector, options.grows && !collector);
    if options.spares_checked && !collector {
        runtime = with_spares_checked(&runtime);
    }
    let mut c = with_board_parts(&with_pieces(&runtime, &emitter.pieces), &emitter.board_parts);
    if options.fast {
        c.insert_str(0, "#define ZENO_UNCHECKED 1\n");
    }
    if let Some(tests) = tests {
        c += &tests.runtime(collector);
    }
    for section in [&emitter.typedefs, &emitter.structs, &emitter.globals, &emitter.prototypes, &emitter.definitions] {
        if !section.is_empty() {
            c.push('\n');
        }
        for item in section {
            c += item;
            c.push('\n');
        }
    }
    // The top-level code. It is kept out of `main` so that every variable
    // of the program lies below `main`'s on the stack, where the collector
    // looks. GCC and Clang are told so in their own word, as they always
    // were: with it they write for a program what they wrote before. A
    // compiler that has no such word calls it by its address, read from
    // a variable it is told it cannot see through (`volatile`), so that
    // it cannot put the code inside `main` either (decision 55.4).
    c += "\nZENO_NOINLINE static void zeno_program(void) {\n";
    c += &render(&main);
    c += "}\n";

    // `main` takes the command line only in a program that reads it.
    let main_line = if emitter.arguments { "\nint main(int argc, char **argv) {\n" } else { "\nint main(void) {\n" };
    c += main_line;
    if collector {
        // The collector starts from the variables. Those on the stack it
        // finds by itself; the top-level ones, and an error on its way to
        // an `except`, are listed here.
        let mut roots = "\nstatic void zeno_roots(void) {\n".to_string();
        roots += "    zeno_mark_words(&zeno_raised, sizeof zeno_raised);\n";
        for root in &emitter.roots {
            roots += &format!("    zeno_mark_words(&{root}, sizeof {root});\n");
        }
        roots += "}\n";
        c.insert_str(c.len() - main_line.len(), &roots);
        c += "    void *base = NULL;\n";
        c += "    zeno_stack_base = &base; /* the stack starts here */\n";
    }
    c += "#ifdef ZENO_MEMORY_REPORT\n    atexit(zeno_memory_report);\n#endif\n";
    c += &format!("    zeno_file = {};\n", c_string(file));
    if !program.modules.is_empty() {
        // The files the program imports, for an error that stops the
        // program in one of them.
        let names: Vec<String> = program.modules.iter().map(|module| c_string(&module.path)).collect();
        c += &format!("    static const char *const modules[] = {{{}}};\n", names.join(", "));
        c += "    zeno_modules = modules;\n";
    }
    if emitter.arguments {
        c += "    zeno_program_start(argc, argv);\n";
    }
    // `zeno test` names the file before it runs it.
    if tests.is_some() {
        c += &format!("    zeno_test_out({});\n", c_string(&format!("{file}\n")));
    }
    if collector {
        c += "#ifndef ZENO_PLAIN_C\n";
        c += "    zeno_program();\n";
        c += "#else\n";
        c += "    void (*volatile program)(void) = zeno_program; /* called so, it stays out of `main` */\n";
        c += "    program();\n";
        c += "#endif\n";
    } else {
        c += "    zeno_program();\n";
    }
    c += match tests {
        Some(_) => "    return zeno_tests_end();\n}\n",
        None => "    return 0;\n}\n",
    };
    Ok(c)
}

/// The line that stands where an `inline c` block begins, and the one
/// where it ends, until the file is whole.
fn block_mark(number: usize, ends: bool) -> String {
    format!("/* An inline c block of the program ({number}){} */\n", if ends { ": its end" } else { "" })
}

/// The C of a program, with each `inline c` block marked by where it
/// was written: a `#line` before it that names the `.zeno` file and the
/// line, so that what the C compiler says of the block, and what a
/// debugger shows, is the line the programmer wrote, and a `#line`
/// after it that goes back to this file of C. A program with no block
/// is given back as it is.
fn with_blocks_placed(c: String, types: &Types, c_file: &str) -> String {
    if !types.c_libraries.iter().any(|library| library.block.is_some()) {
        return c;
    }
    let mut out = String::with_capacity(c.len() + 256);
    let mut written = 0;
    for line in c.split_inclusive('\n') {
        written += 1;
        let block = types.c_libraries.iter().find_map(|library| {
            let block = library.block.as_ref()?;
            if line == block_mark(library.number, false) {
                Some(format!("#line {} \"{}\"\n", block.line, block.file.replace('\\', "/").replace('"', "")))
            } else if line == block_mark(library.number, true) {
                Some(format!("#line {} \"{}\"\n", written + 1, c_file.replace('\\', "/").replace('"', "")))
            } else {
                None
            }
        });
        out.push_str(block.as_deref().unwrap_or(line));
    }
    out
}

/// What a program that says `import c` needs in its C: each library's
/// lines (`#include`, `typedef`) and declarations, as the program wrote
/// them, and the few functions that check a number for C, lend a list
/// and copy text (decision 44.10). Nothing for any other program.
fn c_libraries(types: &Types, minimal: bool) -> (String, String) {
    if types.c_libraries.is_empty() {
        return (String::new(), String::new());
    }
    let mut c = String::from("/* The libraries of C that the program declares (docs/c.md). */\n");
    for library in &types.c_libraries {
        if let Some(block) = &library.block {
            // An `inline c` block is its own C, as the program wrote it.
            // The two marks become `#line`s when the whole file is put
            // together (`with_blocks_placed`), which is when the line
            // each is on is known.
            c += &format!("{}{}{}", block_mark(library.number, false), block.text, block_mark(library.number, true));
            continue;
        }
        c += &format!("/* {} */\n", library.words.join(" ").replace("*/", "* /"));
        for line in &library.lines {
            c += line;
            c.push('\n');
        }
        for function in &library.functions {
            c += &function.c;
        }
    }
    let helpers = format!("{C_LIBRARY_HELPERS}{}", if minimal { C_LIBRARY_MINIMAL } else { C_LIBRARY_CHECKED });
    (c, helpers)
}

/// What kind of number an item of type `T` is: 'f', 'b', or a whole
/// number with a sign ('i') or without ('u'); and the copy of text a
/// function of C may change, and the text C gives back, copied.
const C_LIBRARY_HELPERS: &str = r#"#define ZENO_C_CODE(T) _Generic((T *)0, float *: 'f', double *: 'f', long double *: 'f', _Bool *: 'b', \
    default: ((T)-1 < (T)0 ? 'i' : 'u'))
static const char *zeno_c_text(const char *text) {
    return text ? zeno_copy(text, strlen(text)) : NULL;
}
static char *zeno_c_text_copy(const char *text) {
    return text ? (char *)zeno_copy(text, strlen(text)) : NULL;
}
static void *zeno_c_lend_bytes(int64_t count, int size) {
    void *memory = malloc((size_t)(count > 0 ? count : 1) * (size_t)size);
    if (!memory) {
        zeno_sayf("error: out of memory\n");
        zeno_stop(1);
    }
    return memory;
}
static void zeno_c_store(unsigned char *at, int size, char code, int64_t whole, double real) {
    if (code == 'f' && size == 4) { float x = (float)real; memcpy(at, &x, 4); }
    else if (code == 'f' && size == 8) { memcpy(at, &real, 8); }
    else if (code == 'f') { long double x = real; memcpy(at, &x, sizeof x); }
    else if (size == 1) { uint8_t x = (uint8_t)whole; memcpy(at, &x, 1); }
    else if (size == 2) { uint16_t x = (uint16_t)whole; memcpy(at, &x, 2); }
    else if (size == 4) { uint32_t x = (uint32_t)whole; memcpy(at, &x, 4); }
    else { memcpy(at, &whole, 8); }
}
static void zeno_c_load(const unsigned char *at, int size, char code, int64_t *whole, double *real) {
    if (code == 'f' && size == 4) { float x; memcpy(&x, at, 4); *real = x; }
    else if (code == 'f' && size == 8) { memcpy(real, at, 8); }
    else if (code == 'f') { long double x; memcpy(&x, at, sizeof x); *real = (double)x; }
    else if (code == 'b') { *whole = at[0] != 0; }
    else if (size == 1) { *whole = code == 'i' ? (int64_t)(int8_t)at[0] : (int64_t)at[0]; }
    else if (size == 2) { uint16_t x; memcpy(&x, at, 2); *whole = code == 'i' ? (int64_t)(int16_t)x : (int64_t)x; }
    else if (size == 4) { uint32_t x; memcpy(&x, at, 4); *whole = code == 'i' ? (int64_t)(int32_t)x : (int64_t)x; }
    else { memcpy(whole, at, 8); }
}
static void zeno_c_give_back(zeno_list *list, void *memory, bool bytes, bool floats, int size, char code, bool back) {
    for (int64_t i = 0; back && i < list->len; i++) {
        int64_t whole = 0;
        double real = 0;
        zeno_c_load((unsigned char *)memory + i * size, size, code, &whole, &real);
        if (bytes) ZENO_BYTES(list)[i] = (uint8_t)whole;
        else if (floats) list->items[i].f = code == 'f' ? real : (double)whole;
        else if (code == 'b') list->items[i].b = whole != 0;
        else list->items[i].i = whole;
    }
    free(memory);
}
static const char *zeno_c_pointer_str(const void *pointer) {
    char text[64];
    snprintf(text, sizeof text, "<C pointer %p>", pointer);
    return zeno_copy(text, strlen(text));
}
"#;

/// A number for C, checked to fit the type the function takes, as
/// `zeno run` checks it; and a list lent, each item checked.
const C_LIBRARY_CHECKED: &str = r#"static int64_t zeno_c_fit(int64_t n, bool fits, bool is_signed, int size, const char *what, int line, int col) {
    if (!fits) {
        int bits = size * 8;
        int64_t low = !is_signed ? 0 : bits >= 64 ? INT64_MIN : -((int64_t)1 << (bits - 1));
        uint64_t high = bits >= 64 ? (is_signed ? (uint64_t)INT64_MAX : UINT64_MAX) : ((uint64_t)1 << (bits - (is_signed ? 1 : 0))) - 1;
        zeno_fail("OverflowError", zeno_join(6, what, " holds ", zeno_str_int(low), " to ", zeno_str_u64(high),
                  zeno_join(2, ", and this is ", zeno_str_int(n))), line, col);
    }
    return n;
}
#define ZENO_C_FIT(T, v, what, line, col) ((T)zeno_c_fit((v), (int64_t)(T)(v) == (v) && (((T)(v) < (T)0) == ((v) < 0)), \
    (T)-1 < (T)0, (int)sizeof(T), what, line, col))
static void *zeno_c_lend(zeno_list *list, bool bytes, bool floats, int size, char code, const char *what, int line, int col) {
    unsigned char *memory = zeno_c_lend_bytes(list->len, size);
    for (int64_t i = 0; i < list->len; i++) {
        int64_t whole = bytes ? (int64_t)ZENO_BYTES(list)[i] : code == 'b' ? (int64_t)list->items[i].b : list->items[i].i;
        double real = floats ? list->items[i].f : (double)whole;
        if (code == 'i' || code == 'u') {
            bool is_signed = code == 'i';
            int64_t moved = size >= 8 ? whole : is_signed ? (whole << (64 - size * 8)) >> (64 - size * 8) : (int64_t)((uint64_t)whole & ((UINT64_C(1) << (size * 8)) - 1));
            if (moved != whole || (!is_signed && whole < 0)) {
                const char *item = zeno_join(4, "item ", zeno_str_int(i), " of the list given for ", what);
                zeno_c_fit(whole, false, is_signed, size, item, line, col);
            }
        }
        zeno_c_store(memory + i * size, size, code, whole, real);
    }
    return memory;
}
"#;

/// The same with no checks, for `zeno build --minimal`.
const C_LIBRARY_MINIMAL: &str = r#"#define ZENO_C_FIT(T, v, what) ((T)(v))
static void *zeno_c_lend(zeno_list *list, bool bytes, bool floats, int size, char code, const char *what) {
    unsigned char *memory = zeno_c_lend_bytes(list->len, size);
    (void)what;
    for (int64_t i = 0; i < list->len; i++) {
        int64_t whole = bytes ? (int64_t)ZENO_BYTES(list)[i] : code == 'b' ? (int64_t)list->items[i].b : list->items[i].i;
        double real = floats ? list->items[i].f : (double)whole;
        zeno_c_store(memory + i * size, size, code, whole, real);
    }
    return memory;
}
"#;

/// Lines of C, each with how deeply it is indented.
type Lines = Vec<(usize, String)>;

fn render(lines: &Lines) -> String {
    let mut text = String::new();
    for (indent, line) in lines {
        text += &"    ".repeat(*indent);
        text += line;
        text.push('\n');
    }
    text
}

/// A Zeno expression, compiled.
///
/// `code` is a C expression that gives its value. Some expressions cannot be
/// said in one C expression; the statements they needed first have already
/// been written out by the time a `Val` is returned.
///
/// The flags say what running `code` could do, which is what `sequence`
/// needs to know to keep things in order.
#[derive(Clone)]
struct Val {
    code: String,
    kind: Kind,
    /// It looks at a variable, so it must run before anything changes one.
    reads: bool,
    /// It can stop the program with an error.
    fails: bool,
    /// It calls a function, which may print or change variables.
    calls: bool,
    /// The same two questions, about the statements already written out.
    pre_fails: bool,
    pre_calls: bool,
    /// For text: it was there before this statement began (a literal, or
    /// what an unchanging variable holds), so it will still be there
    /// afterwards. Text that is not `fixed` is in the text room: this
    /// statement built it, or a function gave it back, or it was lent
    /// (see `lend`). Only a build with no collector cares: there, such
    /// text is gone when the statement is over, and is copied to be kept.
    fixed: bool,
    /// For a list: this very expression made it, and nobody has it yet.
    /// `[1, 2]` does, and a comprehension, `split`, `map`, `filter` and
    /// `+` on two lists.
    made: bool,
    /// The statement being compiled owns it and frees it when the
    /// statement is over. So it can be looked at, and it cannot be kept.
    /// See `belongs`.
    passing: bool,
}

/// A value that belongs to the statement being compiled. See
/// `Emitter::belongs`.
struct Passing {
    /// What it is held under, so that leaving early frees it. See `hold`.
    key: String,
    /// The C that declares the variable it is in, holding nothing yet.
    declare: String,
    /// The C that frees it.
    free: String,
}

impl Val {
    /// A value that does nothing and depends on nothing: a literal, or a
    /// temporary that already holds its answer.
    fn plain(code: impl Into<String>, kind: Kind) -> Val {
        Val {
            code: code.into(),
            kind,
            reads: false,
            fails: false,
            calls: false,
            pre_fails: false,
            pre_calls: false,
            fixed: false,
            made: false,
            passing: false,
        }
    }

    /// A value built out of other values, inheriting what they can do.
    fn from(code: String, kind: Kind, parts: &[&Val]) -> Val {
        let mut val = Val::plain(code, kind);
        for part in parts {
            val.reads |= part.reads;
            val.fails |= part.fails;
            val.calls |= part.calls;
            val.pre_fails |= part.pre_fails;
            val.pre_calls |= part.pre_calls;
        }
        val
    }

    fn failing(mut self) -> Val {
        self.fails = true;
        self
    }

    /// `failing`, for a value that only one of the runtime's own checks
    /// can stop the program at: an index, a division. A minimal build has
    /// none of those checks (`checked` is false there), so nothing has to
    /// be worked out ahead of the value to keep two errors in order, and
    /// the C stays one expression.
    fn failing_if(mut self, checked: bool) -> Val {
        self.fails |= checked;
        self
    }

    fn fixed(mut self) -> Val {
        self.fixed = true;
        self
    }

    /// The value as a C double, for arithmetic that mixes ints and floats.
    fn double(&self) -> String {
        match self.kind {
            Kind::Int if self.code.bytes().all(|b| b.is_ascii_digit()) => format!("{}.0", self.code),
            Kind::Int | Kind::U64 => format!("(double){}", self.code),
            _ => self.code.clone(),
        }
    }

    fn is_number(&self) -> bool {
        matches!(self.kind, Kind::Int | Kind::Float)
    }

    /// The value as a C float, for arithmetic on an f32: an int becomes
    /// a float and then the nearest f32, as in every engine (decision 41.2).
    fn single(&self) -> String {
        match self.kind {
            Kind::F32 => self.code.clone(),
            Kind::Int => format!("((float)(double){})", self.code),
            _ => format!("((float){})", self.code),
        }
    }
}

struct Variable {
    /// The C that reads or writes it. Usually a name, such as `z_count`.
    c_name: String,
    kind: Kind,
    /// Declared with `var`, so it can be given a new value.
    mutable: bool,
    /// A `var` that a function written inside this one also uses does not
    /// live in a C variable: it lives in a box on the heap that both of
    /// them point to, so that a change made by one is seen by the other.
    /// This is the C for the pointer to that box.
    boxed: Option<String>,
    /// Whether anything has looked the variable up. That is how a function
    /// written inside another finds out which outside variables it needs.
    used: Cell<bool>,
    /// It owns the list or object it holds, and frees it when its block
    /// ends. In C it is NULL once the value has been given away.
    owner: bool,
    /// It holds the copy of a list that a loop walks, whose items have
    /// an owner elsewhere: only the copy itself is freed.
    looks: bool,
    /// It holds a function with a name, written inside a function, that
    /// uses variables of the function around it (decision 56.16): the
    /// declaration, for a call with names or defaults, and whether it may
    /// be used here. With no collector a function written where a value
    /// goes cannot use it, since it could outlive it.
    inner: Option<(Arc<Function>, bool)>,
}

#[derive(Clone)]
struct FunctionEntry {
    c_name: String,
    decl: Arc<Function>,
    kind: FunctionKind,
    /// For each parameter: does the function take over an owned value?
    owned: Vec<bool>,
    /// For each parameter: is it without `take`? Then the function only
    /// looks, and may be lent a value that is gone when the statement is.
    lends: Vec<bool>,
    /// Where its defaults are read: the declaration of the named function
    /// it is written in (0 for the top of the file), and the functions
    /// with names written inside functions that it sees (decision 56.16).
    within: usize,
    sight: Vec<(String, FunctionEntry)>,
}

/// A function with a name, written inside a function, that uses nothing
/// of the function around it: it is compiled as a function of the file,
/// once the function it is written in is done (decision 56.16).
struct InnerJob {
    entry: FunctionEntry,
    span: Span,
}

/// `kind` without the checker's note that the value is owned, and whether
/// the note was there. C holds an owned value exactly as it holds a shared
/// one, so everything past this point works with the plain kind. What
/// differs is who frees it, and that is kept beside the kind: see
/// `Variable::owner`.
fn disown(kind: Kind) -> (Kind, bool) {
    match kind {
        Kind::Owned(inner) => (disowned(*inner), true),
        kind => (disowned(kind), false),
    }
}

/// The same for everything a function takes and gives back.
/// `kind` with every "owned" mark taken off, however deep. Who owns a
/// value does not change how C holds it.
pub fn disowned(kind: Kind) -> Kind {
    match kind {
        Kind::Owned(inner) => disowned(*inner),
        Kind::List(item) => Kind::List(Box::new(disowned(*item))),
        Kind::Optional(inner) => Kind::Optional(Box::new(disowned(*inner))),
        Kind::Dict(key, value) => Kind::Dict(Box::new(disowned(*key)), Box::new(disowned(*value))),
        Kind::Several(kinds) => Kind::Several(kinds.into_iter().map(disowned).collect()),
        Kind::Function(function) => Kind::Function(Box::new(FunctionKind {
            params: function.params.into_iter().map(disowned).collect(),
            returns: disowned(function.returns),
            raises: function.raises,
        })),
        other => other,
    }
}

fn disown_all(kind: FunctionKind) -> (FunctionKind, Vec<bool>) {
    let (params, owned) = kind.params.into_iter().map(disown).unzip();
    (FunctionKind { params, returns: disown(kind.returns).0, raises: kind.raises }, owned)
}

/// The kind that is there when a value that may be `none` is not.
pub fn some(kind: &Kind) -> &Kind {
    match kind {
        Kind::Optional(inner) => inner,
        kind => kind,
    }
}

/// What it takes to write the C that frees a value.
pub struct Drops {
    /// Is text kept by copy (a build with no collector, docs/text.md)?
    /// Then a str has something to free, and a list of them frees its
    /// items as well as itself.
    pub texts: bool,
    /// Does a list inside a list have one owner, the list it is in? It
    /// does where every list has one: a build with no collector.
    pub nested: bool,
    /// The classes whose objects have one owner each, and the enums
    /// whose values have.
    pub owned: HashSet<String>,
    /// The functions that free a list together with its items, which
    /// some line has called and nobody has written yet: the name of
    /// each, and the kind of its items.
    wanted: RefCell<Vec<(String, Kind)>>,
}

impl Drops {
    /// Which classes have one owner for each object is asked wherever
    /// something is freed, so it is found before anything is compiled.
    pub fn of(program: &Program, types: &Types, collector: bool) -> Drops {
        let mut owned = HashSet::new();
        for statement in crate::ast::flat(&program.statements) {
            if let StmtKind::Class(decl) = &statement.kind {
                let copies = types.class_copies(decl).iter().map(|copy| &**copy);
                for class in std::iter::once(decl).chain(copies) {
                    if types.owned_class(class) {
                        owned.insert(class.name.clone());
                    }
                }
            }
            // An enum and a class cannot have the same name, so one set
            // holds both.
            if let StmtKind::Enum(decl) = &statement.kind {
                let copies = types.enum_copies(decl).iter().map(|copy| &**copy);
                for decl in std::iter::once(decl).chain(copies) {
                    if types.owned_enum(decl) {
                        owned.insert(decl.name.clone());
                    }
                }
            }
        }
        Drops { texts: !collector, nested: !collector, owned, wanted: RefCell::new(Vec::new()) }
    }

    /// Does an item of this kind have one owner, the list it is in? Then
    /// the end of the list is the end of the item.
    pub fn owns(&self, item: &Kind) -> bool {
        match item {
            Kind::Class(name) | Kind::Enum(name) => self.owned.contains(name),
            // An error is a block too, where there is no collector, and
            // so is a function written where a value goes.
            Kind::List(_) | Kind::Dict(..) | Kind::Error | Kind::Function(_) => self.nested,
            // What may be `none` is a pointer that may be NULL, and
            // every function that frees does nothing with NULL.
            Kind::Optional(inner) => is_pointer(inner) && self.owns(inner),
            _ => false,
        }
    }

    /// The name of the function that frees a value of this kind, and
    /// does nothing when handed NULL.
    pub fn name(&self, kind: &Kind) -> String {
        match kind {
            Kind::Class(name) | Kind::Enum(name) => format!("zeno_drop_{}", ident(name)),
            Kind::Optional(inner) if is_pointer(inner) => self.name(inner),
            // An error that an `except` caught, in a build with no collector.
            Kind::Error => "zeno_error_drop".to_string(),
            // A function that was written where a value goes frees the
            // block it is kept in. A function of the program has none,
            // and is left alone.
            Kind::Function(_) => "zeno_closure_drop".to_string(),
            kind if is_text(kind) => "zeno_text_drop".to_string(),
            // A dict frees its two lists, its index and, with no
            // collector, its copies of any text. One that owns its values
            // frees each of them first (decision 56.12).
            Kind::Dict(_, value) if self.owns(value) => self.dict_helper("drop", value),
            Kind::Dict(..) => "zeno_dict_drop".to_string(),
            Kind::List(item) if self.texts && is_text(item) => "zeno_list_drop_text".to_string(),
            // With no collector a number that may be `none` sits in a
            // list as a box that the list owns (decision 56.14).
            Kind::List(item) if self.texts && is_boxed(item) => {
                let name = "zeno_list_drop_boxes".to_string();
                let mut wanted = self.wanted.borrow_mut();
                if !wanted.iter().any(|(known, _)| *known == name) {
                    wanted.push((name.clone(), (**item).clone()));
                }
                name
            }
            Kind::List(item) if self.owns(item) => {
                // How many lists deep the items that are not lists lie,
                // and what frees one of those.
                let (mut depth, mut inner) = (1, some(item));
                while let Kind::List(next) = inner {
                    if !self.owns(next) {
                        break;
                    }
                    (depth, inner) = (depth + 1, some(next));
                }
                // `zeno_list_drop_2_Point` frees a list of lists of
                // Points. No class can be given a name that makes one
                // of the other two.
                let name = match inner {
                    Kind::Class(name) | Kind::Enum(name) => format!("zeno_list_drop_{depth}_{}", ident(name)),
                    Kind::Error => format!("zeno_list_drop_errors{depth}"),
                    Kind::Function(_) => format!("zeno_list_drop_functions{depth}"),
                    // A list of dicts, by what the dicts hold.
                    Kind::Dict(_, value) if self.owns(value) => {
                        format!("zeno_list_drop_dicts{depth}_values_{}", self.values_word(value))
                    }
                    Kind::Dict(..) => format!("zeno_list_drop_dicts{depth}"),
                    Kind::List(text) if self.texts && is_text(text) => format!("zeno_list_drop_texts{depth}"),
                    Kind::List(boxed) if self.texts && is_boxed(boxed) => format!("zeno_list_drop_boxes{depth}"),
                    _ => format!("zeno_list_drop_lists{depth}"),
                };
                let mut wanted = self.wanted.borrow_mut();
                if !wanted.iter().any(|(known, _)| *known == name) {
                    wanted.push((name.clone(), (**item).clone()));
                }
                name
            }
            _ => "zeno_list_drop".to_string(),
        }
    }

    /// What the name of a function for a dict that owns values of this
    /// kind ends with, from the function that frees one value: `Point`,
    /// `lists`, `lists_1_Point`, `dicts`, `errors`, `functions`.
    fn values_word(&self, value: &Kind) -> String {
        match self.name(value).as_str() {
            "zeno_error_drop" => "errors".to_string(),
            "zeno_closure_drop" => "functions".to_string(),
            other if other.starts_with("zeno_list_drop") => format!("lists{}", &other["zeno_list_drop".len()..]),
            other if other.starts_with("zeno_dict_drop") => format!("dicts{}", &other["zeno_dict_drop".len()..]),
            other => other.trim_start_matches("zeno_drop_").to_string(),
        }
    }

    /// The name of a function that works on a dict which owns its values
    /// of this kind: `drop` frees the dict and each value, `clear` frees
    /// each value and empties the dict, and `put` stores a value under a
    /// key and frees the one that was there (decision 56.12). As for the
    /// lists, each is asked for here and written once.
    pub fn dict_helper(&self, what: &str, value: &Kind) -> String {
        let name = format!("zeno_dict_{what}_values_{}", self.values_word(value));
        let mut wanted = self.wanted.borrow_mut();
        if !wanted.iter().any(|(known, _)| *known == name) {
            wanted.push((name.clone(), Kind::Dict(Box::new(Kind::None), Box::new(value.clone()))));
        }
        name
    }

    /// The name of a function for the boxes in which, with no collector, a
    /// list keeps its numbers that may be `none` (decision 56.14), for
    /// `int`, `float` or `bool`: `box_keep` puts a number into a box of
    /// the list's own, `box_take` takes it out and frees the box, and
    /// `list_keep_boxes` gives a list that was just made a box of its own
    /// for each item. Each is asked for here and written once.
    pub fn box_helper(&self, what: &str, scalar: &str) -> String {
        let name = format!("zeno_{what}_{scalar}");
        let kind = match scalar {
            "float" => Kind::Float,
            "bool" => Kind::Bool,
            _ => Kind::Int,
        };
        let mut wanted = self.wanted.borrow_mut();
        if !wanted.iter().any(|(known, _)| *known == name) {
            wanted.push((name.clone(), Kind::Optional(Box::new(kind))));
        }
        name
    }

    /// The statement that frees the owned value in `place`, if `place`
    /// still holds one.
    pub fn code(&self, kind: &Kind, place: &str) -> String {
        format!("{}({place});", self.name(kind))
    }

    /// The statement that frees a copy of a list which a loop walked. The
    /// items in the copy are still the first list's.
    pub fn code_for_copy(&self, item: &Kind, place: &str) -> String {
        match self.owns(item) {
            true => format!("zeno_list_drop({place});"),
            false => self.code(&Kind::List(Box::new(item.clone())), place),
        }
    }

    /// The functions that were asked for since the last call, to write:
    /// the name of each and the kind of the items its list holds.
    pub fn take_wanted(&self, written: usize) -> Vec<(String, Kind)> {
        self.wanted.borrow()[written..].to_vec()
    }
}

/// If `code` is exactly `zeno_text_lend(x)`, the `x`.
fn lent(code: &str) -> Option<&str> {
    let inside = code.strip_prefix("zeno_text_lend(")?.strip_suffix(')')?;
    // The bracket that was taken off the end must be the one that closes
    // the call, and not the end of something written after it.
    let mut depth = 0;
    for byte in inside.bytes() {
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return None,
            b')' => depth -= 1,
            _ => {}
        }
    }
    Some(inside)
}

/// Text, or text that may be `none`.
pub fn is_text(kind: &Kind) -> bool {
    match kind {
        Kind::Str => true,
        Kind::Optional(inner) => **inner == Kind::Str,
        _ => false,
    }
}

/// Is this text written in the program, or `none`? Either can be kept as
/// it is: there is nothing to copy and nothing to free.
pub fn is_written(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Str(_) | ExprKind::None)
}

/// Is there a `try` anywhere in these statements?
pub fn has_try(statements: &[Stmt]) -> bool {
    statements.iter().any(|statement| match &statement.kind {
        // `assert raises` is a `try`, in a program built for its tests.
        StmtKind::Try { .. } | StmtKind::AssertRaises { .. } => true,
        StmtKind::If { then_block, else_block, .. } => {
            has_try(then_block) || else_block.as_deref().is_some_and(has_try)
        }
        StmtKind::While { body, .. } | StmtKind::For { body, .. } | StmtKind::Unsafe(body) => has_try(body),
        StmtKind::Match { arms, .. } => arms.iter().any(|arm| has_try(&arm.body)),
        // A resource's block is a `try` in an ordinary build, that
        // releases it when an error passes through (decision 59.1).
        StmtKind::Scoped { .. } => true,
        _ => false,
    })
}

/// Does this statement always end the function it is in?
fn ends(statement: Option<&Stmt>) -> bool {
    match statement.map(|statement| &statement.kind) {
        Some(StmtKind::Return(_) | StmtKind::Raise(_)) => true,
        Some(StmtKind::Unsafe(body) | StmtKind::Scoped { body, .. }) => ends(body.last()),
        _ => false,
    }
}

/// Does this statement always leave the block it is in?
fn leaves(statement: Option<&Stmt>) -> bool {
    match statement.map(|statement| &statement.kind) {
        Some(StmtKind::Return(_) | StmtKind::Break | StmtKind::Continue | StmtKind::Raise(_)) => true,
        // `unsafe { return p[0] }` leaves as surely as the `return` does.
        Some(StmtKind::Unsafe(body)) => leaves(body.last()),
        // The rest of the block after the `let` of a resource.
        Some(StmtKind::Scoped { body, .. }) => leaves(body.last()),
        _ => false,
    }
}

/// A class: its fields in order, each with its type and default value, and
/// its methods.
struct ClassEntry {
    fields: Vec<(String, Kind, Option<Arc<Expr>>)>,
    /// The fields that are sized whole numbers, such as `level: u8`. The
    /// struct stores each in that many bits. Everywhere else it is an
    /// `int64_t` like any int, so it is made small on the way in and
    /// wide again on the way out.
    sized: HashMap<String, Width>,
    methods: HashMap<String, FunctionEntry>,
    /// The fields that own what they hold: an object of an `owned class`
    /// frees those along with itself.
    owned_fields: Vec<String>,
}

/// An enum: each case with the names and types of the values it carries.
struct EnumEntry {
    cases: Vec<(String, Vec<(String, Kind)>)>,
    /// For each case, which of the values it carries have one owner,
    /// which is the enum value from then on.
    takes: Vec<Vec<bool>>,
}

/// The fields of a function's parameters that it reads once, at its
/// start, and keeps in locals: see `ast::fields_read_once`.
struct Once {
    params: HashSet<String>,
    written: HashSet<String>,
    /// The locals made so far: (parameter, field) to the local and its kind.
    made: HashMap<(String, String), (String, Kind)>,
}

/// `Some(3)` for the code of the number 8: which power of two a whole
/// number written in the program is, if it is one. `x // 8` and `x % 8`
/// are then a shift and a mask (decision 23.5).
pub(crate) fn power_of_two(code: &str) -> Option<u32> {
    let digits = code.strip_prefix("INT64_C(").and_then(|rest| rest.strip_suffix(')')).unwrap_or(code);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    (n.is_power_of_two() && n <= 1 << 62).then(|| n.trailing_zeros())
}

struct Emitter<'a> {
    types: &'a Types,
    /// `--release`: whole-number `+`, `-` and `*` wrap round, unchecked.
    release: bool,
    /// `--fast`: they are C's signed `+`, `-` and `*`, which the C
    /// compiler may take never to overflow (decision 23.6).
    fast: bool,
    /// `--minimal`: no checks at all, and C as a person would write it
    /// where that can be done (decisions chapter 30).
    minimal: bool,
    /// `print` is the runtime's also in a minimal build (`Options`).
    board_prints: bool,
    /// The parts of `BOARD_PARTS` the program uses, such as
    /// `ZENO_INPUT_PINS` when it makes an input pin (decision 45.4).
    /// The pieces of the runtime (`PIECES`) the program uses.
    pieces: Vec<&'static str>,
    board_parts: Vec<&'static str>,
    /// The `for` loop being opened has a body that cannot change its
    /// list, so it needs no copy of it (`ast::walks_in_place`).
    in_place: bool,
    /// For a `for` loop over a variable holding text: the body never names
    /// the variable, so the text stays where it is while the loop walks it.
    text_stays: bool,
    /// The fields this function reads once, at its start (decision 22.8).
    once: Option<Once>,
    /// How each kind of value is freed. See `Drops`.
    drops: Drops,
    /// How many of the functions `drops` has asked for are written.
    drops_written: usize,
    /// The functions, classes and enums of the program, by their Zeno names.
    functions: HashMap<String, FunctionEntry>,
    classes: HashMap<String, ClassEntry>,
    enums: HashMap<String, EnumEntry>,
    /// For each interface, the classes that are one: each with the
    /// number its objects carry. See `register_class`.
    is_a: HashMap<String, Vec<(usize, String)>>,
    /// How many classes have been given such a number.
    numbered: usize,
    /// Each enum case, by its own name: which enum it belongs to, and its
    /// number within it.
    cases: HashMap<String, (String, usize)>,
    /// The copies of generic enums, `Maybe[int]`, whose cases are not in
    /// `cases`: each copy has a case of the same name (decision 58.1).
    copied_enums: HashSet<String>,
    /// Names brought in by `from math import sqrt`: the name, and its module.
    imported: HashMap<String, String>,
    /// The variables in sight, innermost block last. The first entry holds
    /// the top-level variables, which become C globals.
    scopes: Vec<HashMap<String, Variable>>,
    /// How many times each Zeno name has been declared, to give each
    /// declaration its own C name.
    used: HashMap<String, usize>,
    /// The C statements written so far for the body being compiled.
    lines: Lines,
    indent: usize,
    temps: usize,
    /// The pieces of the C file, in the order they are written out.
    typedefs: Vec<String>,
    structs: Vec<String>,
    globals: Vec<String>,
    /// The C names of the top-level variables that can hold a pointer: the
    /// places, besides the stack, where the collector starts looking.
    roots: Vec<String>,
    /// Does the program ask for `program.arguments()`? Then `main`
    /// takes them from the system and hands them to the runtime.
    arguments: bool,
    prototypes: Vec<String>,
    definitions: Vec<String>,
    /// The small show-one-item and compare-two-items functions written so
    /// far for lists, so that each is written once.
    adapters: HashSet<String>,
    /// Inside a function: what it returns.
    returns: Option<Kind>,
    /// The `try` blocks being compiled around this point, outermost first,
    /// by the name of the C variable each one keeps its place in.
    tries: Vec<String>,
    /// For each loop around this point, how many `try` blocks were open
    /// when it began. `break` has to close the ones opened since.
    loops: Vec<usize>,
    /// Which of `scopes` is the first that belongs to the function being
    /// compiled. A `return` frees what the owners from there inward hold.
    base: usize,
    /// For each loop around this point, how many scopes there were when it
    /// began. `break` frees what the owners in the newer ones hold.
    loop_scopes: Vec<usize>,
    /// The same for each `try`: an error raised inside one leaves the
    /// scopes opened since it began.
    try_scopes: Vec<usize>,
    /// The function being compiled has a `try` in it. See `stmt`, on `let`.
    has_try: bool,
    /// In the function being compiled: the variables given a new value
    /// inside a `try`. See `changed_in_try`.
    volatile: HashSet<String>,
    /// In the function being compiled: the names that functions written
    /// inside it mention. A `var` among them is kept in a box.
    shared: HashSet<String>,
    /// How many functions with no name have been compiled, to number them.
    closures: usize,
    /// The declaration of the named function being compiled, 0 at the
    /// top of the file: with a line and a column, where the checker's
    /// notes are kept (`checker::Spot`).
    within: usize,
    /// While a function is being compiled: the top-level variables that
    /// are declared below it in the file, which it must not see.
    below: HashSet<String>,
    /// The C name of every class and enum of the program, known before
    /// any of them is written. A field may have the name of a type, and
    /// the function that makes an object has a parameter for each field:
    /// such a parameter must not take the type's name (decision 31.1).
    type_names: HashSet<String>,
    /// Functions with names written inside functions that use nothing of
    /// the function around them, waiting to be compiled; how many there
    /// have been, for their names in C; and, for each block being
    /// compiled, the ones it declares, with what each name meant before
    /// (decision 56.16). `inner_seen` is the ones in sight of a function
    /// of that kind being compiled now, from the blocks around it.
    inner_jobs: Vec<InnerJob>,
    inner_count: usize,
    inner_frames: Vec<Vec<(String, Option<FunctionEntry>)>>,
    inner_seen: Vec<(String, FunctionEntry)>,
    /// The function being called while its arguments are compiled: where
    /// its defaults are read (see `FunctionEntry::within`).
    callee: Option<(usize, Vec<(String, FunctionEntry)>)>,
    /// Where the second value and the ones after it go, for the call of a
    /// function that gives several that is being compiled (decision 57):
    /// `&t2` in a `let a, b = f()`, or this function's own pointers in a
    /// `return f()`.
    outs: Option<Vec<String>>,
    /// The functions of the top of the file, as a default reads them.
    top_functions: HashMap<String, FunctionEntry>,
    /// Built by `zeno test --build`: the tests to run after the program,
    /// and an `assert` is compiled (decision 60.2).
    tests: Option<TestBuild>,
    /// The tests have been run already: at the end of the innermost
    /// resource at the top of the file, before it is released (rulings
    /// of decision 59.1).
    tests_run: bool,
    /// The resources of the function being compiled whose blocks are
    /// still open, first made first (decision 59.1): the scope each
    /// one's variable is in, and the C that releases it. Each way out
    /// of a block releases them, last made first, before it frees.
    resources: Vec<(usize, String)>,

    // For a build with no collector (`zeno build --no-collector`).
    /// False in a build with no collector.
    collector: bool,
    /// The statement being compiled has built text, which lasts until the
    /// statement is over. See `text_begin`.
    built: bool,
    /// The call about to be compiled makes a new `list[u8]` (36.2).
    makes_bytes: bool,
    /// How many places in the text room have been noted, to number them.
    marks: usize,
    /// How many values are being held for a moment, to number them. See
    /// `hold`.
    holds: usize,
    /// What a `for` loop is about to walk. If that is a list made right
    /// there, as in `for pin in [2, 3, 5]`, the loop frees it.
    walked: Option<usize>,
    /// The values that belong to the statement being compiled, which
    /// frees them when it is over. See `belongs`.
    passing: Vec<Passing>,

    // The rest is for a firmware build, where errors travel as values.
    /// `zeno build --firmware`, and `--no-collector`, which is one too.
    firmware: bool,
    /// The function being compiled can raise: it returns `true` or `false`
    /// and gives its answer through `zeno_result`.
    fallible: bool,
    /// For each `try` around this point, the kinds of error that can
    /// arrive at its `except` blocks as values.
    arriving: Vec<BTreeSet<String>>,
    /// The `except` labels that some `goto` leads to.
    jumped: HashSet<String>,
    /// How many `except` labels there have been, to number them.
    labels: usize,
}

/// The error for something a firmware build has to refuse.
fn firmware_cannot(what: impl AsRef<str>, span: Span) -> Error {
    Error::new(format!("{}, or build without `--firmware`", what.as_ref()), span)
}

/// The error for something a build with no collector has to refuse:
/// whatever would make memory that no line of the program gives back.
/// A function with a name written inside a function that uses its
/// variables, used as a value in a build with no collector (56.16).
pub(crate) fn inner_as_value(name: &str, span: Span) -> Error {
    without_collector(
        format!(
            "`{name}` is a function inside a function that uses variables of the function around it. With no \
             collector it can be called, and not used as a value: the block owns it. Write it as `let {name} = \
             function(...) {{ ... }}`, which has one owner"
        ),
        span,
    )
}

/// The same, called from a function written where a value goes (56.16).
pub(crate) fn inner_outlives(name: &str, span: Span) -> Error {
    without_collector(
        format!(
            "`{name}` is a function inside a function that uses variables of the function around it, and a \
             function written where a value goes may outlive the block that owns it. Give it what it needs as \
             parameters, or call `{name}` outside it"
        ),
        span,
    )
}

pub fn without_collector(why: impl AsRef<str>, span: Span) -> Error {
    Error::new(
        format!(
            "{}. `zeno build --no-collector` leaves the collector out, so nothing would free it. \
             docs/no-collector.md lists what such a build accepts",
            why.as_ref()
        ),
        span,
    )
}

/// The error for a list of numbers that may be `none` in a build with no
/// collector. It says what to write instead: an enum value fits a slot,
/// and the list owns it.
pub fn none_in_a_slot(span: Span) -> Error {
    Error::new(
        "a list of numbers that may be `none` keeps each item in a small block of its own, and \
         `zeno build --no-collector` leaves the collector out, so nothing would free them. \
         Make it a list of an enum that has one case for a missing value and one case that \
         carries the number, say `Missing` and `Value(n: int)`. docs/no-collector.md lists what \
         such a build accepts",
        span,
    )
}

/// The error for two classes or enums that C would know by one name.
fn same_in_c(zeno_name: &str, c_name: &str, span: Span) -> Error {
    Error::new(
        format!(
            "`{zeno_name}` and another class or enum of this program would both be called `z_{c_name}` in C. \
             A copy of a generic class gets such a name: `Stack[int]` becomes `z_Stack_int`. So does a class \
             of an imported file: `Point` of the module `shapes` becomes `z_shapes_Point`. \
             Rename one of them, or build with `--llvm`"
        ),
        span,
    )
}

/// The error for a part of Zeno this stage does not handle yet.
/// A Zeno name as it can be written inside a C name. Two kinds of name
/// need something done. The copy of a generic: `Stack[int]` becomes
/// `Stack_int`, and `Pair[int, list[str]]` becomes `Pair_int_list_str`.
/// And a name from an imported file: `shapes.area` becomes `shapes_area`.
/// Each run of characters that C does not allow in a name turns into
/// one `_`.
pub fn ident(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}


fn cannot(what: impl AsRef<str>, span: Span) -> Error {
    cannot_in("`zeno build`", what.as_ref(), span)
}

/// A list whose items have no type: `[]` or `[none]` where nothing
/// around it gives one. A build needs one (decision 56.16).
pub(crate) fn untyped_list(build: &str, span: Span) -> Error {
    Error::new(
        format!(
            "{build} needs to know what a list holds, and nothing says it here: `[]` and `[none]` alone could \
             hold anything. Give it a type first, as in `let items: list[int] = []`"
        ),
        span,
    )
}

/// A dict of objects of an `owned class` where there is a collector: the
/// dict is shared, and cannot be the one owner of each (decision 56.12).
pub(crate) fn owned_in_shared_dict(build: &str, span: Span) -> Error {
    Error::new(
        format!(
            "{build} keeps a dict where anything may share it, as there is a collector, and an object of an \
             `owned class` has one owner. Make it a plain `class`, or build with `--no-collector`, where a dict \
             owns its values"
        ),
        span,
    )
}

/// A variable that only ever holds `none`: a build needs a type for it.
pub(crate) fn only_none(build: &str, span: Span) -> Error {
    Error::new(
        format!("{build} needs a type for a variable that only ever holds `none`. Give it one, as in `x: int or none`"),
        span,
    )
}

/// The same, for the build `build` names (`zeno build --llvm` uses the
/// parts of this file that both compilers share).
pub(crate) fn cannot_in(build: &str, what: &str, span: Span) -> Error {
    Error::new(format!("{build} cannot compile {what} yet. `zeno run` can still run this program"), span)
}

/// A program that imports `web` is for a browser, and a browser does not
/// run what a C compiler makes. There is another build for it.
pub fn for_a_page(span: Span) -> Error {
    Error::new(
        "the module `web` is for a program in a page, and this build makes a program for a desktop or a board. \
         `zeno build --web` builds the page",
        span,
    )
}

/// The C type that holds a Zeno value of this kind.
///
/// Objects, enum values and lists live on the heap and are handled through
/// pointers, because in Zeno they are shared: `let b = a` makes a second
/// name for the same object, not a copy.
///
/// The text ends ready for a name to follow: `int64_t ` has a space, and
/// `zeno_list *` does not need one.
fn c_type(kind: &Kind, span: Span) -> Result<String, Error> {
    Ok(match kind {
        // A pin is its number.
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt => "int64_t ".to_string(),
        // Only ever an item of a list; read, it is an int.
        Kind::Byte => "uint8_t ".to_string(),
        // A register is its address.
        Kind::Register(_) => "int64_t ".to_string(),
        // A raw pointer is exactly C's: `uint8_t *`.
        Kind::Pointer(pointee) => format!("{} *", pointee.c_type()),
        // A pointer that C gave is kept as C keeps any pointer, and goes
        // back to C as the pointer the function takes (decision 44.10).
        Kind::CPointer(_) => "void *".to_string(),
        // A library of C is known while building, and holds nothing.
        Kind::CLibrary(_) => "int ".to_string(),
        Kind::Float | Kind::Quantity(_) => "double ".to_string(),
        // An f32 is C's own `float` (decision 41.7).
        Kind::F32 => "float ".to_string(),
        Kind::U64 => "uint64_t ".to_string(),
        Kind::Bool => "bool ".to_string(),
        Kind::Str => "const char *".to_string(),
        Kind::List(_) => "zeno_list *".to_string(),
        Kind::Dict(..) => "zeno_dict *".to_string(),
        Kind::Class(name) | Kind::Enum(name) => format!("z_{} *", ident(name)),
        Kind::Error => "zeno_error *".to_string(),
        Kind::Function(_) => "zeno_closure *".to_string(),
        // A pointer can already say "nothing": it is NULL. A number or a
        // bool cannot, so it is paired with a flag.
        Kind::Optional(inner) => match &**inner {
            inner if is_pointer(inner) => c_type(inner, span)?,
            Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "zeno_opt_int ".to_string(),
            Kind::Float | Kind::F32 | Kind::Quantity(_) => "zeno_opt_float ".to_string(),
            Kind::Bool => "zeno_opt_bool ".to_string(),
            _ => return Err(cannot(optional_named(inner), span)),
        },
        Kind::None => {
            return Err(only_none("`zeno build`", span))
        }
        Kind::Other(name) => return Err(cannot(format!("a value of type {name}"), span)),
        // In C an owned value is the same pointer a shared one would be.
        Kind::Owned(inner) => c_type(inner, span)?,
        // A function that gives several values gives the first as its
        // answer, and the rest through pointers (decision 57).
        Kind::Several(kinds) => c_type(&kinds[0], span)?,
    })
}

/// What a `T or none` is called whose T no build can hold yet. A u64
/// that may be `none` would need a fourth flagged kind (decision 56.19).
pub(crate) fn optional_named(inner: &Kind) -> &'static str {
    match inner {
        Kind::U64 => "a u64 that may be `none`",
        _ => "this kind of value that may be `none`",
    }
}

/// Is this a kind that C holds through a pointer?
/// A number or a bool that may be `none`. It does not fit in a slot of a
/// list with its flag, so a list keeps it in a box (`zeno_box_int`).
pub fn is_boxed(kind: &Kind) -> bool {
    matches!(kind, Kind::Optional(inner) if !is_pointer(inner))
}

fn is_pointer(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Str
            | Kind::List(_)
            | Kind::Dict(..)
            | Kind::Class(_)
            | Kind::Enum(_)
            | Kind::Error
            | Kind::Function(_)
            | Kind::Pointer(_)
            | Kind::CPointer(_)
    )
}

/// The C for "no value" where a `T or none` is wanted, `inner` being the T.
fn nothing(inner: &Kind, span: Span) -> Result<String, Error> {
    Ok(match inner {
        inner if is_pointer(inner) => "NULL".to_string(),
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "(zeno_opt_int){false, 0}".to_string(),
        Kind::Float | Kind::F32 | Kind::Quantity(_) => "(zeno_opt_float){false, 0.0}".to_string(),
        Kind::Bool => "(zeno_opt_bool){false, false}".to_string(),
        _ => return Err(cannot(optional_named(inner), span)),
    })
}

/// The one kind that values of kinds `a` and `b` can both be stored as, if
/// there is one: `int` and `none` meet in `int or none`.
fn joined(a: &Kind, b: &Kind) -> Option<Kind> {
    match (a, b) {
        _ if a == b => Some(a.clone()),
        // `[[], [1]]`: the empty list goes along with the other one.
        (empty, Kind::List(_)) if is_empty_list(empty) => Some(b.clone()),
        (Kind::List(_), empty) if is_empty_list(empty) => Some(a.clone()),
        // `[:]` beside a dict that says what it holds.
        (empty, Kind::Dict(..)) if is_empty_dict(empty) => Some(b.clone()),
        (Kind::Dict(..), empty) if is_empty_dict(empty) => Some(a.clone()),
        (Kind::None, Kind::Optional(_)) => Some(b.clone()),
        (Kind::Optional(_), Kind::None) => Some(a.clone()),
        (Kind::None, other) | (other, Kind::None) => Some(Kind::Optional(Box::new(other.clone()))),
        (Kind::Optional(inner), other) | (other, Kind::Optional(inner)) if **inner == *other => {
            Some(Kind::Optional(inner.clone()))
        }
        // Two functions that take and give back the same things in C.
        (Kind::Function(x), Kind::Function(y)) => {
            let nowhere = Span { line: 0, col: 0 };
            // What each raises is the checker's to hold, by the build.
            let (x, y) = (code_type(x, false, nowhere).ok()?, code_type(y, false, nowhere).ok()?);
            (x == y).then(|| a.clone())
        }
        _ => None,
    }
}

/// Can a value of this kind be, or contain, a pointer to memory that the
/// collector looks after? Numbers and bools cannot.
fn holds_pointer(kind: &Kind) -> bool {
    match kind {
        Kind::Int | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Quantity(_) | Kind::None => false,
        // Memory from `allocate` is not the collector's.
        Kind::Pointer(_) | Kind::Register(_) => false,
        // Nor is what C gave.
        Kind::CPointer(_) | Kind::CLibrary(_) => false,
        Kind::Optional(inner) => holds_pointer(inner),
        _ => true,
    }
}

/// The kind of `[]` before the place it arrives at has said what it holds.
fn is_empty_list(kind: &Kind) -> bool {
    matches!(kind, Kind::List(item) if matches!(**item, Kind::Other(_)))
}

/// The kind of `[:]` before the place it arrives at has said what it holds.
fn is_empty_dict(kind: &Kind) -> bool {
    matches!(kind, Kind::Dict(key, _) if matches!(**key, Kind::Other(_)))
}

/// What a dict is told about its keys and its values when it is made, so
/// that the runtime can compare and show them: 's' for text, 'i' for a
/// whole number, 'f' for a float, 'b' for a bool. Any other value is
/// 'p': the runtime keeps it, and the compiler writes the function that
/// shows one and the function that compares two (`plain_value`).
pub fn dict_kinds(key: &Kind, value: &Kind, span: Span) -> Result<(char, char), Error> {
    let letter = |kind: &Kind| match kind {
        Kind::Str => Some('s'),
        Kind::Int => Some('i'),
        Kind::Float => Some('f'),
        Kind::Bool => Some('b'),
        _ => None,
    };
    match (letter(key), letter(value)) {
        (Some(key @ ('s' | 'i')), Some(value)) => Ok((key, value)),
        (Some(key @ ('s' | 'i')), None) if !matches!(value, Kind::None | Kind::Other(_) | Kind::Owned(_)) => Ok((key, 'p')),
        _ => Err(cannot("a dict with these keys and values", span)),
    }
}

/// Can the runtime show and compare a value of this kind in a dict by
/// itself, from the letter the dict was made with?
pub fn plain_value(value: &Kind) -> bool {
    matches!(value, Kind::Str | Kind::Int | Kind::Float | Kind::Bool)
}

/// The C return type of a function that gives back a value of this kind.
fn return_type(kind: &Kind, span: Span) -> Result<String, Error> {
    match kind {
        Kind::None => Ok("void ".to_string()),
        other => c_type(other, span),
    }
}

/// What a function gives as its answer: all it gives, or the first of
/// several (decision 57).
pub(crate) fn first_value(kind: &Kind) -> &Kind {
    match kind {
        Kind::Several(kinds) => &kinds[0],
        kind => kind,
    }
}

/// The values a function gives after the first, which go out through
/// pointers the caller passes, after the parameters (decision 57).
pub(crate) fn later_values(kind: &Kind) -> &[Kind] {
    match kind {
        Kind::Several(kinds) => &kinds[1..],
        _ => &[],
    }
}

/// The name of the pointer that the value after the first `at` goes
/// out through: `zeno_gives_2` for the second value.
pub(crate) fn gives(at: usize) -> String {
    format!("zeno_gives_{}", at + 2)
}

/// The parameters those pointers are, as a function declares them:
/// `int64_t *zeno_gives_2`.
fn out_params(kind: &Kind, span: Span) -> Result<Vec<String>, Error> {
    let mut params = Vec::new();
    for (at, kind) in later_values(kind).iter().enumerate() {
        params.push(format!("{}*{}", c_type(kind, span)?, gives(at)));
    }
    Ok(params)
}

/// `call` with more arguments at its end, before the `)`.
fn with_args(call: &str, more: &[String]) -> String {
    if more.is_empty() {
        return call.to_string();
    }
    let open = call.strip_suffix(')').expect("a call ends with `)`");
    let comma = if open.ends_with('(') { "" } else { ", " };
    format!("{open}{comma}{})", more.join(", "))
}

/// The C type of a pointer to the code of a function held as a value. Its
/// first argument is always the function value itself, which is where a
/// function written inside another finds the variables it uses.
///
/// `raising` is for a firmware build, and a function type that says
/// `raises`: such a function gives back `true` or `false`, and its answer
/// through a pointer that comes last, as a function that raises does
/// when it is called by its name (5.5l, 56.5).
fn code_type(kind: &FunctionKind, raising: bool, span: Span) -> Result<String, Error> {
    let mut params = vec!["zeno_closure *".to_string()];
    for param in &kind.params {
        params.push(c_type(param, span)?.trim_end().to_string());
    }
    for later in later_values(&kind.returns) {
        params.push(format!("{}*", c_type(later, span)?));
    }
    if !raising {
        return Ok(format!("{}(*)({})", return_type(&kind.returns, span)?, params.join(", ")));
    }
    if kind.returns != Kind::None {
        params.push(format!("{}*", c_type(&kind.returns, span)?));
    }
    Ok(format!("bool (*)({})", params.join(", ")))
}

/// C that calls a function held as a value. `function` is mentioned twice,
/// so it must be a plain name. For a function that can raise, in a
/// firmware build, the last of `args` is where the answer goes.
fn invoke(function: &str, kind: &FunctionKind, raising: bool, args: &[String], span: Span) -> Result<String, Error> {
    let mut all = vec![function.to_string()];
    all.extend(args.iter().cloned());
    Ok(format!("(({}){function}->code)({})", code_type(kind, raising, span)?, all.join(", ")))
}

/// A short name for a kind, to build C function names from.
fn key(kind: &Kind) -> String {
    match kind {
        Kind::Int => "int".to_string(),
        Kind::Byte => "u8".to_string(),
        Kind::Float => "float".to_string(),
        Kind::F32 => "f32".to_string(),
        Kind::U64 => "u64".to_string(),
        Kind::Bool => "bool".to_string(),
        Kind::Pin => "pin".to_string(),
        Kind::InputPin => "input_pin".to_string(),
        Kind::Serial => "serial".to_string(),
        Kind::AnalogPin => "analog_pin".to_string(),
        Kind::PwmPin => "pwm_pin".to_string(),
        Kind::Ticker => "ticker".to_string(),
        Kind::Interrupt => "interrupt".to_string(),
        Kind::Pointer(pointee) => format!("pointer_{}", pointee.name()),
        Kind::Register(width) => format!("register_{}", width.name),
        Kind::Str => "str".to_string(),
        Kind::None => "none".to_string(),
        Kind::Quantity(unit) => format!("in_{unit}"),
        Kind::List(item) => format!("list_{}", key(item)),
        Kind::Dict(keys, values) => format!("dict_{}_{}", key(keys), key(values)),
        Kind::Class(name) | Kind::Enum(name) => ident(name),
        Kind::Optional(inner) => format!("opt_{}", key(inner)),
        Kind::Error => "error".to_string(),
        Kind::Function(_) => "function".to_string(),
        Kind::Other(_) => "other".to_string(),
        Kind::CPointer(_) => "c_pointer".to_string(),
        Kind::CLibrary(number) => format!("c_library_{number}"),
        Kind::Owned(inner) => key(inner),
        Kind::Several(kinds) => format!("several_{}", kinds.iter().map(key).collect::<Vec<_>>().join("_")),
    }
}

/// C that puts a value into a list slot.
fn to_slot(kind: &Kind, code: &str, span: Span) -> Result<String, Error> {
    let member = match kind {
        // A list of bytes holds bytes, not slots (decision 36.2).
        Kind::Byte => return Ok(format!("((int64_t)({code}))")),
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "i",
        // A u64 is kept as the int with the same bits.
        Kind::U64 => return Ok(format!("(zeno_slot){{.i = (int64_t)({code})}}")),
        // In a slot an f32 is kept as the float it is exactly.
        Kind::Float | Kind::F32 | Kind::Quantity(_) => "f",
        Kind::Bool => "b",
        Kind::Str => "s",
        Kind::List(_)
        | Kind::Dict(..)
        | Kind::Class(_)
        | Kind::Enum(_)
        | Kind::Error
        | Kind::Function(_)
        | Kind::Pointer(_)
        | Kind::CPointer(_) => "p",
        Kind::Optional(inner) if is_pointer(inner) => return to_slot(inner, code, span),
        // A number and its flag do not fit in a slot; see `zeno_box_int`.
        Kind::Optional(inner) => return Ok(format!("(zeno_slot){{.p = zeno_box_{}({code})}}", scalar(inner, span)?)),
        // `[]` or `[none]` where nothing says what it holds.
        Kind::None | Kind::Other(_) => return Err(untyped_list("`zeno build`", span)),
        Kind::Owned(_) | Kind::CLibrary(_) | Kind::Several(_) => return Err(cannot("a list of these", span)),
    };
    Ok(format!("(zeno_slot){{.{member} = {code}}}"))
}

/// C for the item of `list` at `index`, unchecked: the slot, or the byte.
fn item_at(list: &str, index: &str, item: &Kind) -> String {
    match item {
        Kind::Byte => format!("ZENO_BYTES({list})[{index}]"),
        _ => format!("{list}->items[{index}]"),
    }
}

/// The runtime function that does `what` to a list of `item`s:
/// `zeno_list_append`, or `zeno_bytes_append` for a list of bytes.
fn list_fn(item: &Kind, what: &str) -> String {
    match item {
        Kind::Byte => format!("zeno_bytes_{what}"),
        _ => format!("zeno_list_{what}"),
    }
}

/// The kind of an item once it is read out of a list: a byte is an int.
fn read_kind(item: &Kind) -> Kind {
    match item {
        Kind::Byte => Kind::Int,
        item => item.clone(),
    }
}

/// C that takes a value back out of a list slot.
fn from_slot(kind: &Kind, slot: &str, span: Span) -> Result<String, Error> {
    Ok(match kind {
        Kind::Byte => format!("((int64_t){slot})"),
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => format!("{slot}.i"),
        Kind::Float | Kind::Quantity(_) => format!("{slot}.f"),
        Kind::F32 => format!("((float){slot}.f)"),
        Kind::U64 => format!("((uint64_t){slot}.i)"),
        Kind::Bool => format!("{slot}.b"),
        Kind::Str => format!("{slot}.s"),
        Kind::List(_)
        | Kind::Dict(..)
        | Kind::Class(_)
        | Kind::Enum(_)
        | Kind::Error
        | Kind::Function(_)
        | Kind::Pointer(_)
        | Kind::CPointer(_) => {
            format!("(({}){slot}.p)", c_type(kind, span)?.trim_end())
        }
        Kind::Optional(inner) if is_pointer(inner) => return from_slot(inner, slot, span),
        Kind::Optional(inner) => format!("zeno_unbox_{}({slot}.p)", scalar(inner, span)?),
        // `[]` or `[none]` where nothing says what it holds.
        Kind::None | Kind::Other(_) => return Err(untyped_list("`zeno build`", span)),
        Kind::Owned(_) | Kind::CLibrary(_) | Kind::Several(_) => return Err(cannot("a list of these", span)),
    })
}

/// Which of the runtime's three flagged kinds holds an optional `inner`.
fn scalar(inner: &Kind, span: Span) -> Result<&'static str, Error> {
    match inner {
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => Ok("int"),
        Kind::Float | Kind::F32 | Kind::Quantity(_) => Ok("float"),
        Kind::Bool => Ok("bool"),
        _ => Err(cannot(optional_named(inner), span)),
    }
}

/// What stands for the place in the source in a minimal build, until
/// the C is put together. See `Emitter::at`.
const NO_PLACE: &str = "\u{1}";

/// A Zeno string as a C string literal.
fn c_string(text: &str) -> String {
    let mut out = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'"' => out += "\\\"",
            b'\\' => out += "\\\\",
            b'\n' => out += "\\n",
            b'\t' => out += "\\t",
            // `??` followed by some characters means something else in old C.
            b'?' => out += "\\?",
            // Printable ASCII goes in as it is. Everything else, including
            // the bytes of non-English letters, is written as its number.
            0x20..=0x7e => out.push(byte as char),
            _ => out += &format!("\\{byte:03o}"),
        }
    }
    out.push('"');
    out
}

impl Emitter<'_> {
    // ------------------------------------------------------------- Plumbing

    fn line(&mut self, text: impl Into<String>) {
        self.lines.push((self.indent, text.into()));
    }

    /// `line, col` as the last two arguments of a runtime call that can
    /// fail, for its error message.
    ///
    /// A minimal build has no error messages, and its runtime takes no
    /// such arguments. There this gives a mark that no C has in it, and
    /// when the file is put together the mark goes, with the comma
    /// before it: `zeno_mod(a, b, 16, 12)` is written `zeno_mod(a, b)`.
    fn at(&self, span: Span) -> String {
        match self.minimal {
            true => NO_PLACE.to_string(),
            false => format!("{}, {}", span.line, span.col),
        }
    }

    /// C for the slot of a list at an index: something that can be read,
    /// assigned to, or have its address taken.
    ///
    /// The runtime's `zeno_list_at` checks the index first. A minimal
    /// build has no check, so there it is C's own `[ ]`.
    fn slot_at(&self, list: &str, index: &str, item: &Kind, span: Span) -> String {
        match (self.minimal, item) {
            (true, Kind::Byte) => format!("ZENO_BYTES({list})[{index}]"),
            (false, Kind::Byte) => format!("*zeno_bytes_at({list}, {index}, {})", self.at(span)),
            (true, _) => format!("{list}->items[{index}]"),
            (false, _) => format!("*zeno_list_at({list}, {index}, {})", self.at(span)),
        }
    }

    /// Where that slot is, as a C pointer to it.
    fn slot_address(&self, list: &str, index: &str, span: Span) -> String {
        match self.minimal {
            true => format!("&{list}->items[{index}]"),
            false => format!("zeno_list_at({list}, {index}, {})", self.at(span)),
        }
    }

    /// C for one float divided by another. The runtime's `zeno_div`
    /// refuses to divide by zero; a minimal build divides as C does.
    fn divided(&self, left: &str, right: &str, span: Span) -> String {
        match self.minimal {
            true => format!("({left} / {right})"),
            false => format!("zeno_div({left}, {right}, {})", self.at(span)),
        }
    }

    fn new_temp(&mut self) -> String {
        self.temps += 1;
        format!("t{}", self.temps)
    }

    /// Put `val` into a C variable of its own and return that variable.
    fn temp(&mut self, val: Val, span: Span) -> Result<Val, Error> {
        let name = self.new_temp();
        self.line(format!("{}{name} = {};", c_type(&val.kind, span)?, val.code));
        let mut temp = Val::plain(name, val.kind);
        temp.pre_fails = val.pre_fails || val.fails;
        temp.pre_calls = val.pre_calls || val.calls;
        temp.fixed = val.fixed;
        temp.made = val.made;
        temp.passing = val.passing;
        Ok(temp)
    }

    /// A C name for a new Zeno variable or function. Every Zeno name gets
    /// `z_` in front, so it can never collide with a C keyword or a name
    /// from the C library. A second `x`, in another block, becomes `z2_x`:
    /// that keeps `let x = x + 1` in an inner block from reading itself.
    fn c_name(&mut self, name: &str) -> String {
        let name = &ident(name);
        let count = self.used.entry(name.to_string()).or_insert(0);
        *count += 1;
        match *count {
            1 => format!("z_{name}"),
            n => format!("z{n}_{name}"),
        }
    }

    /// The parameter that brings the value of a field to the function
    /// that makes the object. It is `z_` and the field's name, as the
    /// field is in the struct, unless a class or an enum of the program
    /// has that C name: a parameter called `z_point` would hide the type
    /// `z_point` from the rest of the function, and `z_point *self` would
    /// be read as a multiplication. Then it is `z2_point`, which no type
    /// is called, because every type begins with `z_` (decision 31.1).
    fn given(&self, field: &str) -> String {
        let name = format!("z_{field}");
        if self.type_names.contains(&name) { format!("z2_{field}") } else { name }
    }

    fn declare(&mut self, name: &str, kind: Kind) -> String {
        let c_name = self.c_name(name);
        let variable =
            Variable { c_name: c_name.clone(), kind, mutable: false, boxed: None, used: Cell::new(false), owner: false, looks: false, inner: None };
        self.scopes.last_mut().expect("there is always a scope").insert(name.to_string(), variable);
        c_name
    }

    // ------------------------------------------- With no collector to help
    //
    // `zeno build --no-collector` (docs/no-collector.md). Three things
    // need care that a collector would otherwise take: a value that
    // belongs to nobody for a moment, text built while the program runs,
    // and values that must not be kept.

    /// `place` holds an owned value that has no owner just now: it is on
    /// its way into a call, or it is a list still being filled. Until
    /// `let_go`, whatever leaves the block early (a `return`, a failed
    /// call) frees it, as it frees what the block's owners hold.
    fn hold(&mut self, kind: Kind, place: &str) -> String {
        self.holds += 1;
        // No Zeno name has a space in it, so this cannot hide a variable.
        let key = format!("held {}", self.holds);
        let variable =
            Variable { c_name: place.to_string(), kind, mutable: false, boxed: None, used: Cell::new(false), owner: true, looks: false, inner: None };
        self.scopes.last_mut().expect("there is always a scope").insert(key.clone(), variable);
        key
    }

    /// `hold`, for the copy of a list that a loop walks. The copy is the
    /// loop's. The items in it still belong to the list it was made
    /// from, so leaving early frees the copy and not the items.
    fn hold_copy(&mut self, item: Kind, place: &str) -> String {
        let key = self.hold(Kind::List(Box::new(item)), place);
        if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(&key)) {
            variable.looks = self.drops.owns(match &variable.kind {
                Kind::List(item) => item,
                kind => kind,
            });
        }
        key
    }

    /// Write the functions that free a list together with its items,
    /// for each kind of list the program frees that way:
    ///
    ///     static void zeno_list_drop_1_Point(zeno_list *list) {
    ///         if (list == NULL) return;
    ///         for (int64_t i = 0; i < list->len; i++) zeno_drop_Point(((z_Point *)list->items[i].p));
    ///         zeno_list_drop(list);
    ///     }
    ///
    /// Writing one can ask for another: a list of lists of Points frees
    /// each of its items with the function above.
    fn write_drops(&mut self, span: Span) -> Result<(), Error> {
        loop {
            let wanted = self.drops.take_wanted(self.drops_written);
            if wanted.is_empty() {
                return Ok(());
            }
            self.drops_written += wanted.len();
            for (name, item) in wanted {
                if let Some((head, body)) = self.box_function(&name, &item, span)? {
                    self.helper(head, body);
                    continue;
                }
                // The functions for a dict that owns its values:
                //
                //     static void zeno_dict_drop_values_Point(zeno_dict *dict) {
                //         if (dict == NULL) return;
                //         zeno_dict_clear_values_Point(dict);
                //         zeno_dict_drop(dict);
                //     }
                if let (true, Kind::Dict(_, value)) = (name.starts_with("zeno_dict_"), &item) {
                    let each = self.drops.code(value, &from_slot(value, "dict->values->items[i]", span)?);
                    let clear = self.drops.dict_helper("clear", value);
                    let (head, body) = if name.starts_with("zeno_dict_drop_") {
                        (
                            format!("static void {name}(zeno_dict *dict)"),
                            format!("    if (dict == NULL) return;\n    {clear}(dict);\n    zeno_dict_drop(dict);\n"),
                        )
                    } else if name.starts_with("zeno_dict_clear_") {
                        (
                            format!("static void {name}(zeno_dict *dict)"),
                            format!("    for (int64_t i = 0; i < dict->values->len; i++) {each}\n    zeno_dict_clear(dict);\n"),
                        )
                    } else {
                        let old = self.drops.code(value, &from_slot(value, "zeno_dict_value(dict, zeno_dict_find(dict, key))", span)?);
                        (
                            format!("static void {name}(zeno_dict *dict, zeno_slot key, zeno_slot value)"),
                            format!("    {old}\n    zeno_dict_set(dict, key, value);\n"),
                        )
                    };
                    self.helper(head, body);
                    continue;
                }
                let each = self.drops.code(&item, &from_slot(&item, "list->items[i]", span)?);
                self.helper(
                    format!("static void {name}(zeno_list *list)"),
                    format!(
                        "    if (list == NULL) return;\n    for (int64_t i = 0; i < list->len; i++) {each}\n    zeno_list_drop(list);\n"
                    ),
                );
            }
        }
    }

    /// The functions for the boxes of a list with no collector
    /// (decision 56.14), or `None` if `name` is not one of them:
    ///
    ///     static void *zeno_box_keep_int(zeno_opt_int value) {
    ///         if (!value.present) return NULL;
    ///         int64_t *box = (int64_t *)zeno_alloc_raw(sizeof(int64_t));
    ///         *box = value.value;
    ///         return box;
    ///     }
    ///
    /// `zeno_box_int` of the runtime makes a box in the text room, which
    /// is given back when the statement is over. These make one in the
    /// pool, which the list frees.
    fn box_function(&self, name: &str, item: &Kind, span: Span) -> Result<Option<(String, String)>, Error> {
        let scalar = match item {
            Kind::Optional(inner) if !is_pointer(inner) => scalar(inner, span)?,
            _ => return Ok(None),
        };
        let c = match scalar {
            "float" => "double",
            "bool" => "bool",
            _ => "int64_t",
        };
        Ok(Some(if name == "zeno_list_drop_boxes" {
            (
                format!("static void {name}(zeno_list *list)"),
                "    if (list == NULL) return;\n    for (int64_t i = 0; i < list->len; i++) zeno_free(list->items[i].p);\n    zeno_list_drop(list);\n".to_string(),
            )
        } else if name.starts_with("zeno_box_keep_") {
            (
                format!("static void *{name}(zeno_opt_{scalar} value)"),
                format!(
                    "    if (!value.present) return NULL;\n    {c} *box = ({c} *)zeno_alloc_raw(sizeof({c}));\n    *box = value.value;\n    return box;\n"
                ),
            )
        } else if name.starts_with("zeno_box_take_") {
            (
                format!("static zeno_opt_{scalar} {name}(void *box)"),
                format!("    zeno_opt_{scalar} value = zeno_unbox_{scalar}(box);\n    zeno_free(box);\n    return value;\n"),
            )
        } else if name.starts_with("zeno_list_keep_boxes_") {
            let keep = self.drops.box_helper("box_keep", scalar);
            (
                format!("static zeno_list *{name}(zeno_list *list)"),
                format!(
                    "    for (int64_t i = 0; i < list->len; i++) list->items[i].p = {keep}(zeno_unbox_{scalar}(list->items[i].p));\n    return list;\n"
                ),
            )
        } else {
            return Ok(None);
        }))
    }

    /// With no collector a list keeps its own copy of each item that is
    /// text, and its own box for each number that may be `none`
    /// (decision 56.14). `code` is a list just made whose items are still
    /// another's; this gives it copies of its own.
    fn keep_items(&self, item: &Kind, code: &str, span: Span) -> Result<String, Error> {
        Ok(match item {
            item if is_text(item) => format!("zeno_list_keep_text({code})"),
            Kind::Optional(inner) => format!("{}({code})", self.drops.box_helper("list_keep_boxes", scalar(inner, span)?)),
            _ => code.to_string(),
        })
    }

    /// Does a list of these keep a copy of its own of each item? Then
    /// a list just made from another one needs `keep_items`.
    fn keeps_copies(&self, item: &Kind) -> bool {
        !self.collector && (is_text(item) || is_boxed(item))
    }

    /// C that puts a value into a slot of a list that keeps it: with no
    /// collector a number that may be `none` goes into a box of the
    /// list's own.
    fn kept_slot(&self, kind: &Kind, code: &str, span: Span) -> Result<String, Error> {
        match kind {
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => {
                Ok(format!("(zeno_slot){{.p = {}({code})}}", self.drops.box_helper("box_keep", scalar(inner, span)?)))
            }
            _ => to_slot(kind, code, span),
        }
    }

    /// C that puts a value that is only looked for (`find`, `in`) into a
    /// slot. A number that may be `none` goes into a box in the text room,
    /// which, with no collector, is given back when the statement is over.
    fn looked_for(&mut self, kind: &Kind, code: &str, span: Span) -> Result<String, Error> {
        if !self.collector && is_boxed(kind) {
            self.built = true;
        }
        to_slot(kind, code, span)
    }

    /// C for an item that leaves the list for good (`pop`, `take`), read
    /// from `slot`: the list's copy of text is freed once the statement
    /// has its own, and the list's box once the number is out.
    fn taken(&self, item: &Kind, slot: &str, span: Span) -> Result<String, Error> {
        Ok(match item {
            item if !self.collector && is_text(item) => format!("zeno_text_take({})", from_slot(item, slot, span)?),
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => {
                format!("{}({slot}.p)", self.drops.box_helper("box_take", scalar(inner, span)?))
            }
            item => from_slot(item, slot, span)?,
        })
    }

    /// `dict_kinds`, for a dict this program makes. A dict that owns its
    /// values is built where there is no collector (decision 56.12).
    /// With one, a dict is shared, and could not be the one owner of an
    /// object of an `owned class`, so it is refused as before.
    fn dict_letters(&self, key: &Kind, value: &Kind, span: Span) -> Result<(char, char), Error> {
        if self.collector && self.drops.owns(value) {
            return Err(owned_in_shared_dict("`zeno build`", span));
        }
        dict_kinds(key, value, span)
    }

    /// The value has arrived where it was going, and has an owner again.
    fn let_go(&mut self, key: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.remove(key).is_some() {
                return;
            }
        }
    }

    /// A statement is about to be compiled. In a build with no collector,
    /// text that the statement builds is written into the text room (see
    /// runtime.c) and stays there until the statement is over. Call
    /// `text_end` with what this returns when it is.
    fn text_begin(&mut self) -> (usize, usize, bool) {
        (self.lines.len(), self.indent, std::mem::replace(&mut self.built, false))
    }

    /// The statement begun at `text_begin` has been compiled. If it built
    /// text, note where the text room's top was before it:
    ///
    /// ```c
    /// size_t m1 = zeno_text_top;
    /// ```
    ///
    /// and return the C statement that puts the top back there, for the
    /// caller to write once nothing needs the text any more.
    fn text_end(&mut self, begin: (usize, usize, bool)) -> Option<String> {
        self.text_mark(begin).map(|mark| format!("zeno_text_top = {mark};"))
    }

    /// `text_end`, giving the name of the C variable that holds the mark
    /// in place of the statement that goes back to it.
    fn text_mark(&mut self, (start, indent, outer): (usize, usize, bool)) -> Option<String> {
        if !std::mem::replace(&mut self.built, outer) {
            return None;
        }
        self.marks += 1;
        let mark = format!("m{}", self.marks);
        self.lines.insert(start, (indent, format!("size_t {mark} = zeno_text_top;")));
        Some(mark)
    }

    // ------------------------------------------------------------ Kept text
    //
    // In a build with no collector, text is kept by copy (docs/text.md).
    // Whatever stores text (a variable, a field, a list) has a copy of
    // its own in the pool and frees it when it is done. Text is never
    // changed once made, so a copy cannot be told from the original, and
    // the program means what it means in every other build.

    /// `val` is about to be stored. If it is text, store a copy that the
    /// place can call its own. `written` says the text is written in the
    /// program, which needs no copy.
    fn keep(&self, mut val: Val, written: bool) -> Val {
        if !self.collector && is_text(&val.kind) && !written {
            // Text that was lent for the statement is copied straight
            // from where it is kept, not from the statement's copy.
            let from = lent(&val.code).unwrap_or(&val.code);
            val.code = format!("zeno_text_keep({from})");
        }
        val
    }

    /// `val` was read from a place that a function could change while
    /// this statement is still using what it read. If it is text, the
    /// statement gets a copy of its own, in the text room.
    fn lend(&self, mut val: Val) -> Val {
        if self.collector || !is_text(&val.kind) {
            return val.fixed();
        }
        val.code = format!("zeno_text_lend({})", val.code);
        val.fixed = false;
        val
    }

    /// The condition of an `if` or a `while`. If working it out builds
    /// text, or makes a value that the condition alone owns, the answer
    /// is taken first and those are given back, before the body runs.
    fn condition(&mut self, expr: &Expr) -> Result<Val, Error> {
        let begin = self.text_begin();
        let own = self.own_begin();
        let mut val = self.expr(expr)?;
        let release = self.text_end(begin);
        if release.is_some() || self.passing.len() > own.2 {
            val = self.temp(val, expr.span)?;
        }
        self.own_end(own, true);
        if let Some(release) = release {
            self.line(release);
        }
        Ok(val)
    }

    // ------------------------------------------------ What a statement owns
    //
    // A value that is made in the middle of a statement and given to
    // nobody belongs to the statement (docs/temporaries.md):
    //
    //     print(len(text.split()))      the list of words
    //     print(size(make()))           the owned value `make` gives
    //
    // The statement puts it in a C variable of its own and frees it when
    // it is over:
    //
    //     zeno_list *t1 = NULL;
    //     t1 = zeno_split_blanks(z_text);
    //     zeno_print(1, zeno_int_str(t1->len));
    //     zeno_list_drop_text(t1);
    //
    // The variable is declared where the statement begins, holding NULL,
    // because the value may be made inside an `if` that C needs for `and`
    // or `or`, and the line that frees it is outside. Freeing NULL does
    // nothing.

    /// A statement begins, or a part of one that is over before the
    /// statement is: a condition, one item of a comprehension.
    fn own_begin(&mut self) -> (usize, usize, usize) {
        (self.lines.len(), self.indent, self.passing.len())
    }

    /// It is over. Declare the variables its values were put in, where it
    /// began. If `free`, free the values here. A statement that leaves
    /// (`return`, `raise`) has freed them on its way out, as it frees
    /// what the owners hold, and passes `false`.
    fn own_end(&mut self, (start, indent, from): (usize, usize, usize), free: bool) {
        let ended: Vec<Passing> = self.passing.drain(from..).collect();
        for (position, passing) in ended.iter().enumerate() {
            self.lines.insert(start + position, (indent, passing.declare.clone()));
        }
        // The last one made may be looking at an earlier one.
        for passing in ended.iter().rev() {
            if free {
                self.line(passing.free.clone());
            }
            self.let_go(&passing.key);
        }
    }

    /// `val` is what `expr` gave. If it was made right here and nobody is
    /// getting it, the statement owns it from now on.
    ///
    /// Two kinds of value are in that position. An owned value that is
    /// only looked at, which the checker has noted (`Types::lent`), in
    /// every build. And, in a build with no collector, a list that was
    /// just made and does not start life owned: every other build leaves
    /// such a list to the collector.
    fn belongs(&mut self, mut val: Val, expr: &Expr) -> Result<Val, Error> {
        let lent = self.types.lent(expr);
        if self.walked == Some(expr as *const Expr as usize) {
            // The `for` loop that walks it is the one that frees it.
            val.made |= lent;
            return Ok(val);
        }
        let nobodys = val.made && !self.collector && !self.types.fresh(expr);
        if !lent && !nobodys {
            return Ok(val);
        }
        let name = self.new_temp();
        self.line(format!("{name} = {};", val.code));
        let key = self.hold(val.kind.clone(), &name);
        self.passing.push(Passing {
            key,
            declare: format!("{}{name} = NULL;", c_type(&val.kind, expr.span)?),
            free: self.drops.code(&val.kind, &name),
        });
        let mut own = Val::plain(name, val.kind);
        own.pre_fails = val.pre_fails || val.fails;
        own.pre_calls = val.pre_calls || val.calls;
        own.passing = true;
        Ok(own)
    }

    /// The error for a list that would outlive the statement that owns
    /// it: one that is put somewhere that is not an owner.
    fn no_owner(span: Span) -> Error {
        without_collector("this list has no owner. Give it one, as in `let items: owned list[int] = [1, 2, 3]`", span)
    }

    /// `val` is about to be kept: put in a variable, a field or a list,
    /// given to a function, or returned. In a build with no collector a
    /// value that the statement owns cannot be (see `belongs`). (Text
    /// can: see `keep`. An error can: whoever keeps it owns it.)
    /// Several values written out (`return make(n), fail(n)`, `let xs, s
    /// = make(n), fail(n)`) are all worked out before any reaches its
    /// place. In a firmware build a later one may fail, and the failure
    /// leaves at once. Hold each value that has to be freed until then,
    /// so that such a failure frees it, as `call_c` holds an argument
    /// (decision 57). `copied` says a text is a copy of its own.
    fn hold_for_later(&mut self, val: &Val, copied: bool, later: &[Expr]) -> Option<String> {
        let frees = self.drops.owns(&val.kind) || (copied && is_text(&val.kind));
        let later_work = later.iter().any(|value| !is_plain(value));
        (self.firmware && !self.collector && frees && later_work).then(|| self.hold(val.kind.clone(), &val.code))
    }

    fn stays(&self, val: &Val, span: Span) -> Result<(), Error> {
        if !self.collector && val.passing {
            // It is freed when this statement is over.
            return Err(Self::no_owner(span));
        }
        Ok(())
    }

    /// Each item of a list sits in one slot. A number that may be `none`
    /// does not fit, and is kept in a small block of its own, which
    /// nothing would free.
    fn fits_a_slot(&self, item: &Kind, span: Span) -> Result<(), Error> {
        match item {
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => Err(none_in_a_slot(span)),
            _ => Ok(()),
        }
    }

    // ---------------------------------------------------------- Owned values
    //
    // An owned value is freed by its owner, at the end of the owner's block
    // (docs/ownership.md). In the C, an owner is an ordinary pointer
    // variable, with one extra rule: when its value is given away, the
    // variable is set to NULL. So "free whatever the owners still hold" is
    // one call per owner, and the call does nothing for NULL. The checker
    // has already made sure a variable is never read after it gave its
    // value away.

    /// The C statements that free what the owners in `scopes[from..]`
    /// still hold, innermost block first. The resources of a block are
    /// released, last made first, before its owners are freed (decision
    /// 59.1).
    fn frees(&self, from: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for (depth, scope) in self.scopes.iter().enumerate().skip(from).rev() {
            lines.extend(self.resources.iter().rev().filter(|(at, _)| *at == depth).map(|(_, release)| release.clone()));
            let mut owners: Vec<&Variable> = scope.values().filter(|variable| variable.owner).collect();
            owners.sort_by(|a, b| a.c_name.cmp(&b.c_name));
            lines.extend(owners.into_iter().map(|owner| match owner.looks {
                true => format!("zeno_list_drop({});", owner.c_name),
                false => self.drops.code(&owner.kind, &owner.c_name),
            }));
        }
        lines
    }

    /// Write those statements out: a block is ending, or being left early.
    fn free_owned(&mut self, from: usize) {
        for line in self.frees(from) {
            self.line(line);
        }
    }

    /// The innermost block has reached its last statement. Unless that
    /// statement left the block by itself (and freed on its way out), free
    /// what the block's owners hold.
    fn end_of_block(&mut self, statements: &[Stmt]) {
        if !leaves(statements.last()) {
            self.free_owned(self.scopes.len() - 1);
        }
    }

    /// Mark the variable just declared as an owner.
    fn owns(&mut self, name: &str) {
        if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(name)) {
            variable.owner = true;
        }
    }

    /// Which scope `name` is found in: 0 is the top level.
    fn lookup_depth(&self, name: &str) -> Option<usize> {
        let depth = self.scopes.iter().rposition(|scope| scope.contains_key(name))?;
        // A top-level variable declared below the function being compiled
        // is not in sight from inside it (decision 3.6c).
        (depth > 0 || !self.below.contains(name)).then_some(depth)
    }

    fn lookup(&self, name: &str) -> Option<&Variable> {
        let depth = self.lookup_depth(name)?;
        let variable = self.scopes[depth].get(name)?;
        variable.used.set(true);
        Some(variable)
    }

    // ------------------------------------------------------------ Functions

    fn entry(&mut self, decl: &Arc<Function>, c_name: &str, span: Span) -> Result<FunctionEntry, Error> {
        let Some(kind) = self.types.function(decl).cloned() else {
            return Err(cannot(format!("the function `{}`", decl.name), span));
        };
        let (kind, owned) = disown_all(kind);
        let c_name = self.c_name(c_name);
        let lends = self.types.looks(decl);
        Ok(FunctionEntry { c_name, decl: decl.clone(), kind, owned, lends, within: self.within, sight: Vec::new() })
    }

    fn register(&mut self, decl: &Arc<Function>, span: Span) -> Result<(), Error> {
        let entry = self.entry(decl, &decl.name, span)?;
        self.functions.insert(decl.name.clone(), entry);
        Ok(())
    }

    /// Add a C function to the output: its first line, and its body.
    fn helper(&mut self, first_line: String, body: String) {
        self.prototypes.push(format!("{first_line};"));
        self.definitions.push(format!("{first_line} {{\n{body}}}\n"));
    }

    /// A class becomes a C struct, plus three functions the compiler writes
    /// for it: one to make an object, one to show it as text, and one to
    /// compare two of them. Its methods become ordinary C functions that
    /// take the object as their first argument.
    fn register_class(&mut self, decl: &Class, span: Span) -> Result<(), Error> {
        let Some(kinds) = self.types.class(decl).cloned() else {
            return Err(cannot(format!("the class `{}`", decl.name), span));
        };
        let (kinds, owned): (Vec<Kind>, Vec<bool>) = kinds.into_iter().map(disown).unzip();
        let widths = self.types.widths(decl).cloned().unwrap_or_default();
        let mut sized = HashMap::new();
        // `name` goes into C names, `shown` into what the program prints.
        // They differ only for a copy of a generic class.
        let (name, shown_as) = (&ident(&decl.name), shown_name(&decl.name));
        if self.used.contains_key(name) {
            // `Stack[int]` and a class called `Stack_int`, in one program.
            return Err(same_in_c(&decl.name, name, span));
        }
        let c_name = self.c_name(name);

        self.typedefs.push(format!("typedef struct {c_name} {c_name};"));
        let mut body = format!("struct {c_name} {{\n");
        let mut params = Vec::new();
        let mut fill = String::new();
        if decl.interface {
            return self.register_interface(decl, &c_name, span);
        }
        if !decl.is.is_empty() {
            // An object that is seen as a `Shape` has to say which class
            // it really is, so that `shape.area()` can find the right
            // `area`. It carries its class's number in front of its
            // fields, where every such object has it.
            self.numbered += 1;
            body += "    int32_t zeno_class;\n";
            fill += &format!("    self->zeno_class = {};\n", self.numbered);
            for interface in &decl.is {
                let interface = self.types.interface_name(interface);
                self.is_a.entry(interface).or_default().push((self.numbered, decl.name.clone()));
            }
        }
        let mut shown = Vec::new();
        let mut tests = Vec::new();
        for (index, (field, kind)) in decl.fields.iter().zip(&kinds).enumerate() {
            let field_type = c_type(kind, span)?;
            let field_name = format!("z_{}", field.name);
            let given = self.given(&field.name);
            params.push(format!("{field_type}{given}"));
            match widths.get(index).copied().flatten() {
                // `level: u8` is one byte of the struct. The number was
                // checked when it became a u8, so the cast loses nothing.
                Some(width) => {
                    body += &format!("    {} {field_name};\n", width.c_type());
                    fill += &format!("    self->{field_name} = ({}){given};\n", width.c_type());
                    sized.insert(field.name.clone(), width);
                }
                None if !self.collector && is_text(kind) => {
                    // With no collector the object has a copy of its own.
                    body += &format!("    {field_type}{field_name};\n");
                    fill += &format!("    self->{field_name} = zeno_text_keep({given});\n");
                }
                None => {
                    body += &format!("    {field_type}{field_name};\n");
                    fill += &format!("    self->{field_name} = {given};\n");
                }
            }
            let label = if index == 0 { format!("{shown_as}({}=", field.name) } else { format!(", {}=", field.name) };
            shown.push(c_string(&label));
            shown.push(self.repr(&Val::plain(format!("self->{field_name}"), kind.clone()), span)?);
            tests.push(self.equal(kind, &format!("a->{field_name}"), &format!("b->{field_name}"), span)?);
        }
        if decl.fields.is_empty() {
            if decl.is.is_empty() {
                body += "    char nothing; /* C does not allow an empty struct */\n";
            }
            shown.push(c_string(&format!("{shown_as}(")));
            tests.push("true".to_string());
        }
        shown.push(c_string(")"));
        body += "};";
        self.structs.push(body);

        let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
        self.helper(
            format!("static {c_name} *zeno_new_{name}({params})"),
            format!("    {c_name} *self = ({c_name} *)zeno_alloc(sizeof({c_name}));\n{fill}    return self;\n"),
        );
        self.helper(
            format!("static const char *zeno_show_{name}({c_name} *self)"),
            format!("    return zeno_join({}, {});\n", shown.len(), shown.join(", ")),
        );
        self.helper(
            format!("static bool zeno_same_{name}({c_name} *a, {c_name} *b)"),
            format!("    return {};\n", tests.join(" && ")),
        );
        let mut owned_fields = Vec::new();
        // With no collector every object is freed by a line of the
        // program, and has its own copies of the text in its fields.
        let texts = !self.collector;
        let owned_class = self.types.owned_class(decl);
        if owned_class || texts {
            // The end of an object of an owned class: free what its fields
            // own, then the object. NULL means it was given away.
            let mut body = String::from("    if (self == NULL) return;\n");
            for ((field, kind), owned) in decl.fields.iter().zip(&kinds).zip(&owned) {
                if *owned || (texts && is_text(kind)) {
                    body += &format!("    {}\n", self.drops.code(kind, &format!("self->z_{}", field.name)));
                }
                if *owned {
                    owned_fields.push(field.name.clone());
                }
            }
            body += "    zeno_free(self);\n";
            self.helper(format!("static void zeno_drop_{name}({c_name} *self)"), body);
            if texts {
                // An object that is raised travels inside an error, and
                // the `except` that catches it frees it through this.
                self.helper(
                    format!("static void zeno_drop_any_{name}(void *object)"),
                    format!("    zeno_drop_{name}(({c_name} *)object);\n"),
                );
            }
        }

        let mut methods = HashMap::new();
        for method in &decl.methods {
            let entry = self.entry(method, &format!("{name}_{}", method.name), span)?;
            methods.insert(method.name.clone(), entry);
        }
        let fields = decl
            .fields
            .iter()
            .zip(kinds)
            .map(|(field, kind)| (field.name.clone(), kind, field.default.clone()))
            .collect();
        self.classes.insert(decl.name.clone(), ClassEntry { fields, sized, methods, owned_fields });
        Ok(())
    }

    /// An interface becomes a C struct with one field, the number of the
    /// class an object really is. Every class that is this interface
    /// begins with the same field, so a pointer to one can be read as a
    /// pointer to this.
    fn register_interface(&mut self, decl: &Class, c_name: &str, span: Span) -> Result<(), Error> {
        self.structs.push(format!("struct {c_name} {{\n    int32_t zeno_class;\n}};"));
        let name = ident(&decl.name);
        let mut methods = HashMap::new();
        for method in &decl.methods {
            let entry = self.entry(method, &format!("{name}_{}", method.name), span)?;
            methods.insert(method.name.clone(), entry);
        }
        let entry = ClassEntry { fields: Vec::new(), sized: HashMap::new(), methods, owned_fields: Vec::new() };
        self.classes.insert(decl.name.clone(), entry);
        Ok(())
    }

    /// The C functions of an interface. Each looks at the number the
    /// object carries and passes the call on to the function of that
    /// class:
    ///
    /// ```c
    /// static double z_Shape_area(z_Shape *a0) {
    ///     switch (a0->zeno_class) {
    ///     case 1: return z_Circle_area((z_Circle *)a0);
    ///     default: return z_Square_area((z_Square *)a0);
    ///     }
    /// }
    /// ```
    ///
    /// The same is written for showing an object as text, comparing two,
    /// and freeing one.
    fn pass_on(&mut self, decl: &Class, span: Span) -> Result<(), Error> {
        let name = ident(&decl.name);
        let c_name = format!("z_{name}");
        let classes = self.is_a.get(&decl.name).cloned().unwrap_or_default();
        // `switch` over the classes, the last one as `default`, so that
        // C sees every path return.
        let switch = |on: &str, each: &dyn Fn(&str) -> String, none: &str| -> String {
            if classes.is_empty() {
                return format!("    (void){on};\n{none}");
            }
            let mut body = format!("    switch ({on}->zeno_class) {{\n");
            for (index, (number, class)) in classes.iter().enumerate() {
                let label = if index + 1 == classes.len() { "default".to_string() } else { format!("case {number}") };
                body += &format!("    {label}: {}\n", each(class));
            }
            body + "    }\n"
        };

        for method in &decl.methods {
            let entry = &self.classes[&decl.name].methods[&method.name];
            let (function, kind) = (entry.c_name.clone(), entry.kind.clone());
            // In a firmware build a function that can raise gives back
            // `true` or `false`, and its answer goes out through one
            // more parameter: see `function`.
            let fallible = self.firmware && !method.raises.is_empty();
            let gives = kind.returns != Kind::None;
            let mut params = Vec::new();
            for (index, param) in kind.params.iter().enumerate() {
                params.push(format!("{}a{index}", c_type(param, span)?));
            }
            // The later of several values go out where the caller said.
            let outs: Vec<String> = (0..later_values(&kind.returns).len()).map(crate::emit_c::gives).collect();
            params.extend(out_params(&kind.returns, span)?);
            if fallible && gives {
                params.push(format!("{}*zeno_result", c_type(&kind.returns, span)?));
            }
            let mut calls = HashMap::new();
            for (_, class) in &classes {
                let Some(own) = self.classes.get(class).and_then(|entry| entry.methods.get(&method.name)) else {
                    return Err(cannot(format!("the class `{class}` as a `{}`", decl.name), span));
                };
                let mut args = vec![format!("(z_{} *)a0", ident(class))];
                args.extend((1..kind.params.len()).map(|index| format!("a{index}")));
                args.extend(outs.iter().cloned());
                let own_fallible = self.firmware && !own.decl.raises.is_empty();
                if own_fallible && gives {
                    args.push("zeno_result".to_string());
                }
                let call = format!("{}({})", own.c_name, args.join(", "));
                let line = match (fallible, own_fallible, gives) {
                    (false, _, true) | (true, true, _) => format!("return {call};"),
                    (false, _, false) => format!("{call}; return;"),
                    (true, false, true) => format!("*zeno_result = {call}; return true;"),
                    (true, false, false) => format!("{call}; return true;"),
                };
                calls.insert(class.clone(), line);
            }
            let none = match (fallible, gives) {
                (true, _) => "    return true;\n".to_string(),
                (false, false) => String::new(),
                (false, true) => {
                    let returns = c_type(&kind.returns, span)?;
                    format!("    static {returns}nothing;\n    return nothing;\n")
                }
            };
            let returns = if fallible { "bool ".to_string() } else { return_type(&kind.returns, span)? };
            let body = switch("a0", &|class| calls[class].clone(), &none);
            self.helper(format!("static {returns}{function}({})", params.join(", ")), body);
        }

        self.helper(
            format!("static const char *zeno_show_{name}({c_name} *self)"),
            switch(
                "self",
                &|class| format!("return zeno_show_{0}((z_{0} *)self);", ident(class)),
                "    return \"\";\n",
            ),
        );
        self.helper(
            format!("static bool zeno_same_{name}({c_name} *a, {c_name} *b)"),
            format!(
                "    if (a->zeno_class != b->zeno_class) return false;\n{}",
                switch(
                    "a",
                    &|class| format!("return zeno_same_{0}((z_{0} *)a, (z_{0} *)b);", ident(class)),
                    "    return true;\n"
                )
            ),
        );
        let texts = !self.collector;
        if self.types.owned_class(decl) || texts {
            self.helper(
                format!("static void zeno_drop_{name}({c_name} *self)"),
                format!(
                    "    if (self == NULL) return;\n{}",
                    switch("self", &|class| format!("zeno_drop_{0}((z_{0} *)self); return;", ident(class)), "")
                ),
            );
            if texts {
                self.helper(
                    format!("static void zeno_drop_any_{name}(void *object)"),
                    format!("    zeno_drop_{name}(({c_name} *)object);\n"),
                );
            }
        }
        Ok(())
    }

    /// Is an object of the class `from` also a `to`, an interface?
    fn is_a(&self, from: &str, to: &str) -> bool {
        self.is_a.get(to).is_some_and(|classes| classes.iter().any(|(_, class)| class == from))
    }

    /// An enum becomes a C struct holding a `tag` that says which case the
    /// value is, and a union with room for whichever case carries the most.
    fn register_enum(&mut self, decl: &Enum, span: Span) -> Result<(), Error> {
        let Some(kinds) = self.types.enumeration(decl).cloned() else {
            return Err(cannot(format!("the enum `{}`", decl.name), span));
        };
        let name = &decl.name;
        // The enum's name as part of a C name. An enum of an imported
        // file is `shapes.Shape` to Zeno and `shapes_Shape` to C.
        let id = &ident(name);
        if self.used.contains_key(id) {
            return Err(same_in_c(name, id, span));
        }
        let c_name = self.c_name(name);
        self.typedefs.push(format!("typedef struct {c_name} {c_name};"));

        let mut members = String::new();
        let mut bare_cases = Vec::new();
        let mut show = String::from("    switch (self->tag) {\n");
        let mut same = String::from("    if (a->tag != b->tag) return false;\n    switch (a->tag) {\n");
        let mut cases = Vec::new();
        // With no collector a value that carries something is freed by
        // its owner, and has its own copies of the text it carries.
        let texts = !self.collector;
        let mut drop = String::from("    if (self == NULL) return;\n    switch (self->tag) {\n");
        let mut takes = Vec::new();
        for (index, (case, case_kinds)) in decl.variants.iter().zip(&kinds).enumerate() {
            // A case of a copy of a generic enum is found through the
            // copy the checker noted, since each copy has a `Just`.
            if !self.copied_enums.contains(name) {
                self.cases.insert(case.name.clone(), (name.clone(), index));
            }
            let (case_kinds, owned): (Vec<Kind>, Vec<bool>) = case_kinds.iter().cloned().map(disown).unzip();
            let fields: Vec<(String, Kind)> =
                case.fields.iter().map(|field| field.name.clone()).zip(case_kinds).collect();

            if fields.is_empty() {
                // A case that carries nothing needs no memory of its own:
                // one fixed value stands for every use of it.
                bare_cases.push(format!("static {c_name} zeno_case_{id}_{} = {{.tag = {index}}};", ident(&case.name)));
                show += &format!("    case {index}: return {};\n", c_string(shown_name(&case.name)));
            } else {
                let mut inside = String::new();
                let mut params = Vec::new();
                let mut fill = String::new();
                let mut shown = Vec::new();
                let mut tests = Vec::new();
                let mut frees = String::new();
                for (position, (field, kind)) in fields.iter().enumerate() {
                    let field_type = c_type(kind, span)?;
                    let place = format!("as.c{index}.z_{field}");
                    let given = self.given(field);
                    inside += &format!("{field_type}z_{field}; ");
                    params.push(format!("{field_type}{given}"));
                    if texts && is_text(kind) {
                        fill += &format!("    self->{place} = zeno_text_keep({given});\n");
                    } else {
                        fill += &format!("    self->{place} = {given};\n");
                    }
                    if owned[position] || (texts && is_text(kind)) {
                        frees += &format!(" {}", self.drops.code(kind, &format!("self->{place}")));
                    }
                    shown.push(if position == 0 { c_string(&format!("{}(", shown_name(&case.name))) } else { c_string(", ") });
                    shown.push(self.repr(&Val::plain(format!("self->{place}"), kind.clone()), span)?);
                    tests.push(self.equal(kind, &format!("a->{place}"), &format!("b->{place}"), span)?);
                }
                members += &format!("        struct {{ {inside}}} c{index};\n");
                self.helper(
                    format!("static {c_name} *zeno_new_{id}_{}({})", ident(&case.name), params.join(", ")),
                    format!(
                        // Only as much as this case carries: the
                        // union is as big as the biggest case, and most
                        // values are not that one.
                        "    {c_name} *self = ({c_name} *)zeno_alloc(offsetof({c_name}, as) + sizeof((({c_name} *)0)->as.c{index}));\n    self->tag = {index};\n{fill}    return self;\n"
                    ),
                );
                show += &format!(
                    "    case {index}: return zeno_join({}, {}, \")\");\n",
                    shown.len() + 1,
                    shown.join(", ")
                );
                same += &format!("    case {index}: return {};\n", tests.join(" && "));
                // A bare case is one fixed value, which is never freed.
                // Only a case that carries something has a block.
                drop += &format!("    case {index}:{frees} zeno_free(self); break;\n");
            }
            cases.push((case.name.clone(), fields));
            takes.push(owned);
        }
        if self.drops.owned.contains(name) {
            self.helper(
                format!("static void zeno_drop_{id}({c_name} *self)"),
                format!("{drop}    default: break;\n    }}\n"),
            );
        }
        let mut body = format!("struct {c_name} {{\n    int tag;\n");
        if !members.is_empty() {
            body += &format!("    union {{\n{members}    }} as;\n");
        }
        body += "};";
        self.structs.push(body);
        self.structs.extend(bare_cases);

        self.helper(
            format!("static const char *zeno_show_{id}({c_name} *self)"),
            format!("{show}    }}\n    return \"\";\n"),
        );
        self.helper(
            format!("static bool zeno_same_{id}({c_name} *a, {c_name} *b)"),
            format!("{same}    }}\n    return true;\n"),
        );
        self.enums.insert(name.clone(), EnumEntry { cases, takes });
        Ok(())
    }

    /// The enum and the place of the case `name`, in a pattern matched
    /// against a value of the kind `there`. A case of a copy of a generic
    /// enum is a case of the copy matched (decision 58.1); any other is
    /// found by its name alone.
    fn case_in(&self, name: &str, there: &Kind) -> Option<(String, usize)> {
        match there {
            Kind::Enum(subject) if self.copied_enums.contains(subject) => {
                let index = self.enums[subject].cases.iter().position(|(case, _)| case == name)?;
                Some((subject.clone(), index))
            }
            _ => self.cases.get(name).cloned(),
        }
    }

    /// The place of the case named at `expr` (a name, or the callee of a
    /// call) in the copy `owner` of a generic enum.
    fn copied_case(&self, owner: &str, expr: &Expr, span: Span) -> Result<usize, Error> {
        let ExprKind::Name(name) = &expr.kind else {
            return Err(cannot("a case of a generic enum written this way", span));
        };
        self.enums[owner]
            .cases
            .iter()
            .position(|(case, _)| case == name)
            .ok_or_else(|| cannot(format!("the case `{name}` of `{owner}`"), span))
    }

    /// Compile one function or method, and add it to the output.
    fn function(
        &mut self,
        decl: &Function,
        c_name: &str,
        kind: &FunctionKind,
        owned: &[bool],
        span: Span,
    ) -> Result<(), Error> {
        // A function sees the top-level variables and its own, nothing else.
        self.scopes.truncate(1);
        self.scopes.push(HashMap::new());
        self.base = 1;
        self.lines.clear();
        self.indent = 1;
        // Names only have to be told apart within one C function, so the
        // count of names used starts again from the top-level ones.
        let used = self.used.clone();

        // In a firmware build a function that can raise gives back `true`
        // or `false`, and its answer goes out through one more parameter.
        let fallible = self.firmware && !decl.raises.is_empty();
        let mut params = self.parameters(decl, kind, owned, span)?;
        params.extend(out_params(&kind.returns, span)?);
        if fallible && kind.returns != Kind::None {
            params.push(format!("{}*zeno_result", c_type(&kind.returns, span)?));
        }
        let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
        let within = std::mem::replace(&mut self.within, decl as *const Function as usize);
        let done = self.body(decl, kind, fallible, span);
        self.within = within;
        done?;
        self.scopes.pop();
        self.base = 0;
        self.used = used;

        let body = render(&std::mem::take(&mut self.lines));
        let returns = if fallible { "bool ".to_string() } else { return_type(&kind.returns, span)? };
        self.helper(format!("static {returns}{c_name}({params})"), body);
        Ok(())
    }

    /// Declare the parameters of a function in the innermost scope, and
    /// return them as they are written in C.
    ///
    /// A parameter that takes over an owned value is an owner like any
    /// other: the function frees the value when it ends, unless it has
    /// passed it on by then.
    fn parameters(
        &mut self,
        decl: &Function,
        kind: &FunctionKind,
        owned: &[bool],
        span: Span,
    ) -> Result<Vec<String>, Error> {
        // See `stmt`, on `let`, for what `volatile` is doing here.
        let careful = if !self.firmware && has_try(&decl.body) { "volatile " } else { "" };
        let mut params = Vec::new();
        for (index, (param, param_kind)) in decl.params.iter().zip(&kind.params).enumerate() {
            let param_type = c_type(param_kind, span)?;
            let param_name = self.declare(&param.name, param_kind.clone());
            if owned.get(index) == Some(&true) {
                self.owns(&param.name);
                params.push(format!("{param_type}{careful}{param_name}"));
            } else {
                params.push(format!("{param_type}{param_name}"));
            }
        }
        Ok(params)
    }

    /// Compile the statements of a function into `self.lines`.
    fn body(&mut self, decl: &Function, kind: &FunctionKind, fallible: bool, span: Span) -> Result<(), Error> {
        let mut volatile = HashSet::new();
        let mut shared = HashSet::new();
        for statement in &decl.body {
            if !self.firmware {
                changed_in_try(statement, false, &mut volatile);
            }
            names_in_functions(statement, false, &mut shared);
        }
        // What the function around this one was in the middle of is set
        // aside, and put back afterwards.
        let volatile = std::mem::replace(&mut self.volatile, volatile);
        let shared = std::mem::replace(&mut self.shared, shared);
        let tries = std::mem::take(&mut self.tries);
        let loops = std::mem::take(&mut self.loops);
        let loop_scopes = std::mem::take(&mut self.loop_scopes);
        let try_scopes = std::mem::take(&mut self.try_scopes);
        let resources = std::mem::take(&mut self.resources);
        let has_try = std::mem::replace(&mut self.has_try, has_try(&decl.body));
        let returns = self.returns.replace(kind.returns.clone());
        let arriving = std::mem::take(&mut self.arriving);
        let fallible = std::mem::replace(&mut self.fallible, fallible);
        let built_in = |name: &str| self.lookup_depth(name).is_none() && !self.functions.contains_key(name);
        // A parameter that may be `none` is read only after a test that it
        // is not, so its fields cannot be read at the start.
        let may_be_none =
            |name: &str| matches!(self.scopes.iter().rev().find_map(|scope| scope.get(name)), Some(Variable { kind: Kind::Optional(_), .. }));
        let params = decl.params.iter().map(|param| param.name.as_str()).filter(|name| !may_be_none(name));
        let once = crate::ast::fields_read_once(params, &decl.body, &built_in)
            .map(|(params, written)| Once { params: params.into_iter().collect(), written, made: HashMap::new() });
        let once = std::mem::replace(&mut self.once, once);

        let mut result = self.statements(&decl.body);
        self.once = once;
        let ends = ends(decl.body.last());
        // Reaching the end of the body: free what its owners still hold.
        if result.is_ok() && !ends {
            self.free_owned(self.base);
        }
        // A function that may give back `none` does so if it reaches its
        // end without a `return`.
        if let (Ok(()), Kind::Optional(inner), false) = (&result, &kind.returns, ends) {
            result = nothing(inner, span).map(|none| {
                if self.fallible {
                    self.line(format!("*zeno_result = {none};"));
                    self.line("return true;");
                } else {
                    self.line(format!("return {none};"));
                }
            });
        }
        // Reaching the end of a function that can raise means it did not.
        if result.is_ok() && self.fallible && kind.returns == Kind::None && !ends {
            self.line("return true;");
        }
        self.arriving = arriving;
        self.fallible = fallible;

        self.volatile = volatile;
        self.shared = shared;
        self.tries = tries;
        self.loops = loops;
        self.loop_scopes = loop_scopes;
        self.try_scopes = try_scopes;
        self.resources = resources;
        self.has_try = has_try;
        self.returns = returns;
        result
    }

    // ----------------------------------------------------------- Statements

    fn block(&mut self, statements: &[Stmt]) -> Result<(), Error> {
        self.scopes.push(HashMap::new());
        self.indent += 1;
        let result = self.statements(statements);
        if result.is_ok() {
            self.end_of_block(statements);
        }
        self.indent -= 1;
        self.scopes.pop();
        result
    }

    /// The statements of a block, in order, with the functions with names
    /// that the block declares (decision 56.16).
    ///
    /// What follows the `let` of a resource is more of the same block
    /// (decision 59.1), so the block's functions are looked for there too,
    /// and the places they are made at are counted through it.
    fn statements(&mut self, statements: &[Stmt]) -> Result<(), Error> {
        let flat = crate::ast::flat(statements);
        let made = self.inner_functions(&flat)?;
        let result = self.statements_from(statements, &made, 0, flat.len());
        self.inner_end();
        result
    }

    /// The statements of `statements`, the first of them at `first` in
    /// the block read flat, and the rest of a resource's block inside
    /// what releases it. `total` is how many the block has, read flat.
    fn statements_from(
        &mut self,
        statements: &[Stmt],
        made: &[(usize, Arc<Function>, Span)],
        first: usize,
        total: usize,
    ) -> Result<(), Error> {
        for (offset, statement) in statements.iter().enumerate() {
            let index = first + offset;
            if let StmtKind::Scoped { name, body } = &statement.kind {
                // Its first statement is the one at `index`, read flat.
                return self.scoped(name, body, statement.span, &mut |this| this.statements_from(body, made, index, total));
            }
            for (at, decl, span) in made {
                if *at == index {
                    self.inner_closure(decl, *span)?;
                }
            }
            self.stmt(statement)?;
        }
        // One that nothing uses: made last, so that its mistakes are
        // still found.
        for (at, decl, span) in made {
            if *at == total {
                self.inner_closure(decl, *span)?;
            }
        }
        Ok(())
    }

    /// The rest of a block after the `let` of a resource, or the body of
    /// a function that takes one (decision 59.1). However it is left, the
    /// resource is released if its variable still holds it, and the
    /// leaving goes on as it was. It opens no scope: what is declared in
    /// it is the block's.
    ///
    /// The end of it, and `return`, `break` and `continue`, and an error
    /// as a value (`failed`), release it through `frees`, before the
    /// block's owners are freed. In an ordinary build an error jumps, so
    /// the rest is a `try` too:
    ///
    /// ```c
    /// zeno_try t1;
    /// t1.outer = zeno_trying;
    /// zeno_trying = &t1;
    /// if (zeno_setjmp(t1.place) != 0) {
    ///     ...release...                 // an error passing through
    ///     zeno_raise_again();
    /// }
    /// ...the rest of the block...
    /// zeno_trying = t1.outer;
    /// ...release...
    /// ```
    ///
    /// `program.exit` and a fault of a firmware build stop the program
    /// where they are, and release nothing.
    fn scoped(
        &mut self,
        name: &str,
        body: &[Stmt],
        span: Span,
        compile: &mut dyn FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let top_level = self.scopes.len() == 1 && self.returns.is_none();
        let release = self.release_of(name, span)?;
        let depth = self.lookup_depth(name).ok_or_else(|| cannot(format!("the resource `{name}`"), span))?;
        let place = (!self.firmware).then(|| self.new_temp());
        if let Some(place) = &place {
            self.line(format!("zeno_try {place};"));
            self.line(format!("{place}.outer = zeno_trying;"));
            self.line(format!("zeno_trying = &{place};"));
            self.line(format!("if (zeno_setjmp({place}.place) != 0) {{"));
            self.indent += 1;
            for line in self.keep_raised(vec![release.clone()]) {
                self.line(line);
            }
            self.line("zeno_raise_again();");
            self.indent -= 1;
            self.line("}");
            self.tries.push(place.clone());
            // A `raise` inside frees nothing of the resource's block:
            // the release above still needs what it holds.
            self.try_scopes.push(self.scopes.len());
        }
        self.resources.push((depth, release.clone()));
        let result = compile(self);
        self.resources.pop();
        if place.is_some() {
            self.try_scopes.pop();
            self.tries.pop();
        }
        result?;
        // Left by its last line, which released on its way out.
        if leaves(body.last()) {
            return Ok(());
        }
        if let Some(place) = &place {
            self.line(format!("zeno_trying = {place}.outer;"));
        }
        // Under `zeno test --build` the top of the file ends after its
        // last test, so the tests run before its last resource is
        // released (rulings of decision 59.1).
        let innermost = !matches!(body.last().map(|statement| &statement.kind), Some(StmtKind::Scoped { .. }));
        if top_level && innermost && !self.tests_run {
            if let Some(tests) = self.tests.clone() {
                self.tests_run = true;
                self.run_tests(&tests)?;
            }
        }
        self.line(release);
        Ok(())
    }

    /// The C that releases the resource `name` if its variable still
    /// holds it: the variable is left holding nothing first, so that
    /// nothing releases it twice, then `release` is called, and the
    /// object is freed after, unless `release` took it.
    fn release_of(&mut self, name: &str, span: Span) -> Result<String, Error> {
        let Some(variable) = self.lookup(name) else {
            return Err(cannot(format!("the resource `{name}`"), span));
        };
        let (c_name, kind) = (variable.c_name.clone(), variable.kind.clone());
        let entry = match &kind {
            Kind::Class(class) => self.classes.get(class).and_then(|entry| entry.methods.get("release")),
            _ => None,
        };
        let Some(entry) = entry else {
            return Err(cannot(format!("the resource `{name}`, with no `release`"), span));
        };
        let (function, takes) = (entry.c_name.clone(), entry.owned.first() == Some(&true));
        let c_type = c_type(&kind, span)?;
        let free = match takes {
            true => String::new(),
            false => format!(" {}", self.drops.code(&kind, "zeno_held")),
        };
        Ok(format!("if ({c_name} != NULL) {{ {c_type}zeno_held = {c_name}; {c_name} = NULL; {function}(zeno_held);{free} }}"))
    }

    /// The functions with names that a block of a function declares
    /// (decision 56.16), sorted by `inner_plan`. One that uses nothing of
    /// the function around it becomes a function of the file, which can
    /// be called anywhere in the block, above its line too: it is in sight
    /// from here, and compiled later (`inner_jobs`). One that does use
    /// something is a closure, given back with the statement it is made
    /// before.
    fn inner_functions(&mut self, statements: &[&Stmt]) -> Result<Vec<(usize, Arc<Function>, Span)>, Error> {
        let around: HashSet<String> = self.scopes[1..].iter().flat_map(|scope| scope.keys().cloned()).collect();
        let plan = inner_plan(statements, &around, "`zeno build`")?;
        self.inner_frames.push(Vec::new());
        // The first kind: in sight from here, under a name in C of its own.
        let mut jobs = Vec::new();
        for (decl, span) in &plan.plain {
            let Some(kind) = self.types.function(decl).cloned() else {
                return Err(cannot(format!("the function `{}`", decl.name), *span));
            };
            let (kind, owned) = disown_all(kind);
            self.inner_count += 1;
            let c_name = format!("zeno_inner_{}_{}", self.inner_count, ident(&decl.name));
            let lends = self.types.looks(decl);
            let entry = FunctionEntry { c_name, decl: decl.clone(), kind, owned, lends, within: self.within, sight: Vec::new() };
            let before = self.functions.insert(decl.name.clone(), entry);
            self.inner_frames.last_mut().expect("pushed above").push((decl.name.clone(), before));
            jobs.push((decl.name.clone(), *span));
        }
        // Each sees every function of this kind around it and beside it.
        let mut seen = self.inner_seen.clone();
        for frame in &self.inner_frames {
            for (name, _) in frame {
                seen.retain(|(other, _)| other != name);
                seen.push((name.clone(), self.functions[name].clone()));
            }
        }
        for (name, span) in jobs {
            let mut entry = self.functions[&name].clone();
            entry.sight = seen.clone();
            self.functions.insert(name, entry.clone());
            self.inner_jobs.push(InnerJob { entry, span });
        }
        Ok(plan.closures)
    }

    /// The end of a block that `inner_functions` began: each name of a
    /// function of the first kind means again what it meant before.
    fn inner_end(&mut self) {
        let frame = self.inner_frames.pop().unwrap_or_default();
        for (name, before) in frame.into_iter().rev() {
            match before {
                Some(entry) => self.functions.insert(name, entry),
                None => self.functions.remove(&name),
            };
        }
    }

    /// A function with a name written inside a function, that uses
    /// variables of the function around it: a closure, held under its
    /// name, which the block owns (decision 56.16).
    fn inner_closure(&mut self, decl: &Arc<Function>, span: Span) -> Result<(), Error> {
        let val = self.closure_made(decl, &decl.name, Some(decl), span)?;
        let c_type = c_type(&val.kind, span)?;
        let c_name = self.declare(&decl.name, val.kind);
        self.line(format!("{c_type}{c_name} = {};", val.code));
        if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(&decl.name)) {
            variable.owner = !self.collector;
            variable.inner = Some((decl.clone(), true));
        }
        Ok(())
    }

    /// Compile the functions of the first kind that `inner_functions`
    /// found, each as a function of the file, seeing what it saw where it
    /// was written (decision 56.16).
    fn inner_functions_now(&mut self) -> Result<(), Error> {
        while !self.inner_jobs.is_empty() {
            let job = self.inner_jobs.remove(0);
            let entry = job.entry;
            let mut before = Vec::new();
            for (name, seen) in &entry.sight {
                before.push((name.clone(), self.functions.insert(name.clone(), seen.clone())));
            }
            let frames = std::mem::take(&mut self.inner_frames);
            let seen = std::mem::replace(&mut self.inner_seen, entry.sight.clone());
            let done = self.function(&entry.decl, &entry.c_name, &entry.kind, &entry.owned, job.span);
            self.inner_frames = frames;
            self.inner_seen = seen;
            for (name, was) in before.into_iter().rev() {
                match was {
                    Some(entry) => self.functions.insert(name, entry),
                    None => self.functions.remove(&name),
                };
            }
            done?;
        }
        Ok(())
    }

    fn stmt(&mut self, statement: &Stmt) -> Result<(), Error> {
        // The statements that work out one value and are done. In a build
        // with no collector, text they build is given back as they end.
        // (The others give it back at a point of their own choosing.)
        let simple = matches!(
            statement.kind,
            StmtKind::Let { .. } | StmtKind::Const { .. } | StmtKind::Assign { .. } | StmtKind::Expr(_) | StmtKind::Receive { .. }
        );
        let begin = (!self.collector && simple).then(|| self.text_begin());
        // What the statement makes and gives to nobody is freed as it
        // ends, in every build (see `belongs`). A `for` or a `match` keeps
        // such a value until its last block is over.
        let own = self.own_begin();
        let result = self.statement(statement);
        if result.is_ok() {
            self.own_end(own, !matches!(statement.kind, StmtKind::Return(_) | StmtKind::Raise(_)));
        }
        if let Some(release) = begin.and_then(|begin| self.text_end(begin)) {
            self.line(release);
        }
        result
    }

    fn statement(&mut self, statement: &Stmt) -> Result<(), Error> {
        let span = statement.span;
        // At the top of the file, outside any function or block.
        let top_level = self.scopes.len() == 1 && self.returns.is_none();
        match &statement.kind {
            // At the top of the file, where each statement is compiled
            // by itself, or in a block of statements that `statements`
            // did not read.
            StmtKind::Scoped { name, body } => {
                self.scoped(name, body, span, &mut |this| body.iter().try_for_each(|statement| this.stmt(statement)))?
            }
            StmtKind::Let { name, ty, value, .. } | StmtKind::Const { name, ty, value } => {
                let mutable = matches!(statement.kind, StmtKind::Let { mutable: true, .. });
                // The checker knows whether this variable owns its value.
                let owner = self.types.owner(statement);
                let expected = ty.as_ref().and_then(|ty| self.kind_of_type(ty));
                let mut val = self.expr_as(value, expected.as_ref())?;
                self.stays(&val, value.span)?;
                val = self.keep(val, is_written(value));
                if let Some(expected) = &expected {
                    val = self.coerce(val, expected, value.span)?;
                }
                // With no collector a variable has its own copy of its
                // text, and frees it. Text written in the program is not a
                // copy, but a `var` may be given one later.
                let owner = owner || (!self.collector && is_text(&val.kind) && (mutable || !is_written(value)));
                self.bind(name, val, mutable, owner, top_level, value.span)?;
            }

            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, span)?,
            StmtKind::Receive { declare, places, value } => self.receive(*declare, places, value, top_level, span)?,

            StmtKind::Expr(expr) => {
                let val = self.expr(expr)?;
                if self.types.discarded(expr) {
                    // An owned value that nobody keeps is freed on the spot.
                    self.line(self.drops.code(&val.kind, &val.code));
                } else if !val.code.is_empty() && (val.fails || val.calls) {
                    // What is left may still need to run, for what it does.
                    self.line(format!("{};", val.code));
                }
            }

            StmtKind::If { condition, then_block, else_block } => {
                let condition = self.condition(condition)?;
                self.line(format!("if ({}) {{", bare(&condition.code)));
                self.block(then_block)?;
                self.else_part(else_block)?;
            }

            // The checker has seen to what is written inside. In C it is
            // a block like any other: C is unsafe all the way through.
            // A build leaves the tests of a program out (decision 10.20).
            // Built for its tests, each test is a function by now, and
            // its asserts are compiled (decision 60.2).
            StmtKind::Assert(condition, message) if self.tests.is_some() => {
                self.test_assert(condition, message.as_ref(), span)?
            }
            StmtKind::AssertRaises { error, body } if self.tests.is_some() => self.test_assert_raises(error, body, span)?,
            StmtKind::Test { .. } | StmtKind::Assert(..) | StmtKind::AssertRaises { .. } => {}
            StmtKind::Unsafe(body) => {
                self.line("{");
                self.block(body)?;
                self.line("}");
            }

            StmtKind::While { condition, body } => {
                let mark = self.lines.len();
                let condition = self.condition(condition)?;
                let before = self.lines.split_off(mark);
                if before.is_empty() {
                    self.line(format!("while ({}) {{", bare(&condition.code)));
                } else {
                    // The condition needs statements of its own, and they
                    // must run again on every trip round the loop.
                    self.line("while (true) {");
                    self.lines.extend(before.into_iter().map(|(indent, line)| (indent + 1, line)));
                    self.indent += 1;
                    self.line(format!("if (!({})) break;", condition.code));
                    self.indent -= 1;
                }
                self.loop_body(body)?;
                self.line("}");
            }

            StmtKind::For { variable, second, iterable, body } => {
                let begin = self.text_begin();
                // A body that cannot change the list walks it in place.
                let built_in = |name: &str| self.lookup_depth(name).is_none() && !self.functions.contains_key(name);
                self.in_place = crate::ast::walks_in_place(iterable, body, &built_in);
                self.text_stays = matches!(&iterable.kind, ExprKind::Name(name) if !crate::ast::mentions(body, name));
                let walked = self.open_loop(variable, second.as_deref(), iterable);
                self.in_place = false;
                self.text_stays = false;
                let walked = walked?;
                if let Some(release) = self.text_end(begin) {
                    // Text built to find what to loop over. The loop has
                    // its list or its numbers in hand by now, so the text
                    // is given back before the first trip.
                    let at = self.lines.iter().rposition(|(_, line)| line.starts_with("for (")).expect("a loop");
                    let indent = self.lines[at].0;
                    self.lines.insert(at, (indent, release));
                }
                self.loop_body(body)?;
                self.line("}");
                self.scopes.pop();
                self.close_loop(walked);
            }

            StmtKind::Match { subject, arms } => self.match_statement(subject, arms, span)?,

            StmtKind::Return(value) => {
                let Some(returns) = self.returns.clone() else {
                    return Err(cannot("`return` outside a function", span));
                };
                match value {
                    Some(value) if returns != Kind::None => {
                        let begin = self.text_begin();
                        // `return a, b`: each is worked out in its turn and
                        // held; the later ones go out through the pointers
                        // the caller passed, each kept by copy where text is
                        // (decision 57). `return f()` hands f the same
                        // pointers.
                        let (val, later, written) = match (&value.kind, &returns) {
                            (ExprKind::Several(values), Kind::Several(kinds)) => {
                                let (mut vals, mut held) = (Vec::new(), Vec::new());
                                for (at, (value, kind)) in values.iter().zip(kinds).enumerate() {
                                    let val = self.expr_as(value, Some(kind))?;
                                    self.stays(&val, value.span)?;
                                    let mut val = self.coerce(val, kind, span)?;
                                    if at > 0 {
                                        val = self.keep(val, is_written(value));
                                    }
                                    let val = self.temp(val, span)?;
                                    held.extend(self.hold_for_later(&val, at > 0 && !is_written(value), &values[at + 1..]));
                                    vals.push(val);
                                }
                                for key in &held {
                                    self.let_go(key);
                                }
                                let first = vals.remove(0);
                                (first, vals, is_written(&values[0]))
                            }
                            _ => {
                                // What was there is put back after: a function written among the
                                // arguments has `return`s of its own (decision 57).
                                let outer = self.outs.replace((0..later_values(&returns).len()).map(gives).collect());
                                let val = self.expr_as(value, Some(&returns));
                                self.outs = outer;
                                let val = val?;
                                self.stays(&val, value.span)?;
                                let mut val = self.coerce(val, &returns, span)?;
                                val.kind = first_value(&val.kind).clone();
                                (val, Vec::new(), is_written(value))
                            }
                        };
                        let returns = first_value(&returns).clone();
                        let mut val = val;
                        // With no collector, text that a function gives
                        // back travels in the text room (docs/text.md).
                        let text = !self.collector && is_text(&returns) && !written;
                        self.built |= text;
                        if let Some(mark) = self.text_mark(begin) {
                            // Take the answer before the text it was
                            // worked out from is given back.
                            if text {
                                val.code = format!("zeno_text_return({mark}, {})", val.code);
                                val = self.temp(val, span)?;
                            } else {
                                val = self.temp(val, span)?;
                                self.line(format!("zeno_text_top = {mark};"));
                            }
                        }
                        // Every block of the function ends here. Work the
                        // value out first: it may read what is freed next.
                        let frees = self.frees(self.base);
                        if !frees.is_empty() {
                            if !text {
                                val = self.temp(val, span)?;
                            }
                            frees.into_iter().for_each(|line| self.line(line));
                        }
                        for (at, later) in later.iter().enumerate() {
                            self.line(format!("*{} = {};", gives(at), later.code));
                        }
                        if self.fallible {
                            self.line(format!("*zeno_result = {};", val.code));
                            self.line("return true;");
                        } else if self.tries.is_empty() || self.firmware {
                            self.line(format!("return {};", val.code));
                        } else {
                            // Work the value out while the `try` is still
                            // watching, then leave the `try`, then return.
                            let val = self.temp(val, span)?;
                            self.leave_tries(0);
                            self.line(format!("return {};", val.code));
                        }
                    }
                    Some(value) if !matches!(value.kind, ExprKind::None) => {
                        return Err(cannot("returning a value from a function that returns nothing", span))
                    }
                    _ => {
                        self.free_owned(self.base);
                        self.leave_tries(0);
                        self.line(if self.fallible { "return true;" } else { "return;" });
                    }
                }
            }
            StmtKind::Break | StmtKind::Continue => {
                let Some(&opened_before) = self.loops.last() else {
                    return Err(cannot("`break` or `continue` outside a loop", span));
                };
                // The blocks opened since the loop began end here.
                let outside = self.loop_scopes.last().copied().unwrap_or(self.scopes.len());
                self.free_owned(outside);
                self.leave_tries(opened_before);
                self.line(if statement.kind == StmtKind::Break { "break;" } else { "continue;" });
            }

            // Compiled separately; see `emit`.
            StmtKind::Function(_) | StmtKind::Class(_) | StmtKind::Enum(_) if top_level => {}
            // Made by `statements`, where it is first needed.
            StmtKind::Function(_) => {}
            StmtKind::Class(_) => return Err(cannot("a class inside a function or block", span)),
            StmtKind::Enum(_) => return Err(cannot("an enum inside a function or block", span)),

            StmtKind::Import { module, names } => {
                let module = module.join(".");
                if module == "web" {
                    return Err(for_a_page(span));
                }
                if !matches!(module.as_str(), "math" | "time" | "board" | "files" | "program" | "c") {
                    return Err(cannot(format!("the module `{module}`"), span));
                }
                for name in names {
                    self.imported.insert(name.clone(), module.clone());
                }
            }

            StmtKind::Try { body, handlers } => self.try_statement(body, handlers, span)?,

            StmtKind::Raise(value) if self.firmware => {
                let begin = self.text_begin();
                // Note the error, then go where a failed call would go.
                // With no collector `raise` takes the error: whoever
                // held it holds nothing now, and the `except` that
                // catches it is its next owner.
                let val = self.expr(value)?;
                self.stays(&val, value.span)?;
                let (error, kind) = match &val.kind {
                    Kind::Error => match &value.kind {
                        // `raise ValueError("...")` says its kind. An error
                        // held in a variable could be any kind.
                        ExprKind::Call { callee, .. } => match &callee.kind {
                            ExprKind::Name(name) if ERROR_KINDS.contains(&name.as_str()) => (val.code, name.clone()),
                            _ => (val.code, "Error".to_string()),
                        },
                        _ => (val.code, "Error".to_string()),
                    },
                    Kind::Class(name) => {
                        let (name, id) = (name.clone(), ident(name));
                        let object = self.temp(val, span)?.code;
                        // Showing the object builds text.
                        self.built = !self.collector;
                        let error = match self.collector {
                            true => format!("zeno_error_new({}, zeno_show_{id}({object}), {object})", c_string(&name)),
                            // The `except` that catches it frees the
                            // object, and the text the object keeps.
                            false => format!(
                                "zeno_error_of({}, zeno_show_{id}({object}), {object}, zeno_drop_any_{id})",
                                c_string(&name)
                            ),
                        };
                        (error, name)
                    }
                    _ => return Err(cannot("`raise` with anything but an error or an object", span)),
                };
                let leave = self.failed(&[kind], span)?;
                self.line(format!("zeno_note({error}, {});", self.at(span)));
                // The error has its own copy of its message by now.
                if let Some(release) = self.text_end(begin) {
                    self.line(release);
                }
                self.line(leave);
            }

            StmtKind::Raise(value) => {
                let val = self.expr(value)?;
                // The jump leaves at least the blocks opened since the
                // nearest `try` in this function, or all of the function's
                // if there is none. Free what their owners hold first.
                // (The functions the jump passes through on its way are
                // not asked: what they own is left to the collector.)
                let inside = self.try_scopes.last().copied().unwrap_or(self.base);
                let frees = self.frees(inside);
                let error = match &val.kind {
                    Kind::Error if frees.is_empty() => val.code,
                    // Making the error may read what is about to be freed.
                    Kind::Error => self.temp(val, span)?.code,
                    // An object of a class can be raised too. It travels
                    // inside an error value whose kind is the class's name.
                    Kind::Class(name) => {
                        let (name, id) = (name.clone(), ident(name));
                        let object = self.temp(val, span)?.code;
                        format!("zeno_error_new({}, zeno_show_{id}({object}), {object})", c_string(&name))
                    }
                    _ => return Err(cannot("`raise` with anything but an error or an object", span)),
                };
                frees.into_iter().for_each(|line| self.line(line));
                self.line(format!("zeno_raise({error}, {});", self.at(span)));
            }
        }
        Ok(())
    }

    /// The body of a loop. It notes how many `try` blocks were open when
    /// the loop began, for `break` and `continue` to compare against.
    fn loop_body(&mut self, body: &[Stmt]) -> Result<(), Error> {
        self.loops.push(self.tries.len());
        self.loop_scopes.push(self.scopes.len());
        let result = self.block(body);
        self.loop_scopes.pop();
        self.loops.pop();
        result
    }

    /// Before a `return`, `break` or `continue` jumps out of `try` blocks,
    /// tell the runtime that those blocks are over. `keep` is how many of
    /// the open ones stay open.
    fn leave_tries(&mut self, keep: usize) {
        // In a firmware build a `try` leaves nothing behind to undo.
        if self.firmware {
            return;
        }
        if let Some(outermost) = self.tries.get(keep) {
            self.line(format!("zeno_trying = {outermost}.outer;"));
        }
    }

    /// `try { ... } except Kind as name { ... }`
    ///
    /// ```c
    /// zeno_try t1;                      // a place to come back to
    /// t1.outer = zeno_trying;
    /// zeno_trying = &t1;                // from here on, errors come to t1
    /// if (zeno_setjmp(t1.place) == 0) {
    ///     ...the body...
    ///     zeno_trying = t1.outer;       // finished with no error
    /// } else if (zeno_caught("ValueError")) {
    ///     ...the except block...        // an error jumped back here
    /// } else {
    ///     zeno_raise_again();           // not ours: pass it outward
    /// }
    /// ```
    ///
    /// `setjmp` gives 0 when it is first called. If anything inside the
    /// body raises, however deep in function calls, the runtime jumps back
    /// to that same `setjmp`, which this time gives 1.
    fn try_statement(&mut self, body: &[Stmt], handlers: &[Handler], span: Span) -> Result<(), Error> {
        if self.firmware {
            return self.try_with_values(body, handlers, span);
        }
        let place = self.new_temp();
        self.line(format!("zeno_try {place};"));
        self.line(format!("{place}.outer = zeno_trying;"));
        self.line(format!("zeno_trying = &{place};"));
        self.line(format!("if (zeno_setjmp({place}.place) == 0) {{"));
        self.tries.push(place.clone());
        self.try_scopes.push(self.scopes.len());
        let result = self.block(body);
        self.try_scopes.pop();
        self.tries.pop();
        result?;
        // Reaching the end of the body means nothing was raised. (If the
        // body always leaves by its last line, that end is never reached.)
        let leaves = leaves(body.last());
        if !leaves {
            self.indent += 1;
            self.line(format!("zeno_trying = {place}.outer;"));
            self.indent -= 1;
        }

        let mut catches_everything = false;
        for handler in handlers {
            // What the `except` asks for, and what `as name` then holds.
            let (wanted, holds) = match handler.error.as_deref() {
                None | Some("Error") => (None, Kind::Error),
                Some(class) if self.classes.contains_key(class) => (Some(class), Kind::Class(class.to_string())),
                Some(kind) if ERROR_KINDS.contains(&kind) => (Some(kind), Kind::Error),
                Some(other) => return Err(cannot(format!("`except {other}`"), span)),
            };
            self.line(match wanted {
                Some(kind) => format!("}} else if (zeno_caught({})) {{", c_string(kind)),
                None => "} else {".to_string(),
            });
            self.scopes.push(HashMap::new());
            self.indent += 1;
            if let Some(name) = &handler.binding {
                let c_type = c_type(&holds, span)?;
                let value = match holds {
                    Kind::Class(_) => format!("({})zeno_raised->object", c_type.trim_end()),
                    _ => "zeno_raised".to_string(),
                };
                let c_name = self.declare(name, holds);
                self.line(format!("{c_type}{c_name} = {value};"));
            }
            let result = handler.body.iter().try_for_each(|statement| self.stmt(statement));
            if result.is_ok() {
                self.end_of_block(&handler.body);
            }
            self.indent -= 1;
            self.scopes.pop();
            result?;
            if wanted.is_none() {
                catches_everything = true;
                break; // nothing gets past an `except` that takes every error
            }
        }
        if !catches_everything {
            self.line("} else {");
            self.indent += 1;
            self.line("zeno_raise_again();");
            self.indent -= 1;
        }
        self.line("}");
        Ok(())
    }

    // ------------------------------------------------- Tests (decision 60.2)
    //
    // Only in a program built by `zeno test --build`. Each test is a
    // function by now, that may raise any error (`test_build::with_tests`).

    /// After the program's own statements: each test in turn, with what
    /// it raises caught and reported, and a line for each as it ends.
    fn run_tests(&mut self, tests: &TestBuild) -> Result<(), Error> {
        self.line("zeno_tests_begin();");
        for (function, name) in &tests.tests {
            let c_name = self.functions[function].c_name.clone();
            self.line(format!("zeno_test_begin({});", c_string(name)));
            if self.firmware {
                self.line(format!("if (!{c_name}()) zeno_test_uncaught();"));
            } else {
                let place = self.new_temp();
                self.line("{");
                self.indent += 1;
                self.line(format!("zeno_try {place};"));
                self.line(format!("{place}.outer = zeno_trying;"));
                self.line(format!("zeno_trying = &{place};"));
                self.line(format!("if (zeno_setjmp({place}.place) == 0) {{"));
                self.line(format!("    {c_name}();"));
                self.line(format!("    zeno_trying = {place}.outer;"));
                self.line("} else {");
                self.line("    zeno_test_uncaught();");
                self.line("}");
                self.indent -= 1;
                self.line("}");
            }
            self.line("zeno_test_end();");
        }
        Ok(())
    }

    /// `assert`, in a test. The condition is worked out once. If it is
    /// false, the runtime is given the report: the text after the comma,
    /// worked out only now, and for a comparison what was on each side.
    /// Then the test is left as `return` leaves it: through every `try`,
    /// which does not catch it, and freeing what the test owns.
    fn test_assert(&mut self, condition: &Expr, message: Option<&Expr>, span: Span) -> Result<(), Error> {
        let failed = self.new_temp();
        self.line(format!("bool {failed} = false;"));
        let begin = self.text_begin();
        let own = self.own_begin();
        let at = self.at(condition.span);
        match &condition.kind {
            ExprKind::Binary { op, left, right }
                if matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Lt
                        | BinaryOp::LtEq
                        | BinaryOp::Gt
                        | BinaryOp::GtEq
                        | BinaryOp::Is
                        | BinaryOp::IsNot
                        | BinaryOp::In
                        | BinaryOp::NotIn
                ) =>
            {
                let (left, right, holds) = if matches!(op, BinaryOp::Is | BinaryOp::IsNot) {
                    // `x is none`: the right is `none`, as written.
                    let left = self.expr(left)?;
                    let left = self.twice(left, span)?;
                    let right = self.expr(right)?;
                    let is = *op == BinaryOp::Is;
                    let holds = match &left.kind {
                        Kind::Optional(inner) if is_pointer(inner) => {
                            format!("({} {} NULL)", left.code, if is { "==" } else { "!=" })
                        }
                        Kind::Optional(_) if is => format!("(!{}.present)", left.code),
                        Kind::Optional(_) => format!("({}.present)", left.code),
                        other => ((*other == Kind::None) == is).to_string(),
                    };
                    (left, right, holds)
                } else {
                    let mut vals = self.sequence(&[left, right])?;
                    let right = vals.pop().expect("two values");
                    let left = vals.pop().expect("two values");
                    let left = self.twice(left, span)?;
                    let right = self.twice(right, span)?;
                    let holds = match op {
                        BinaryOp::In | BinaryOp::NotIn => self.contains(*op, left.clone(), right.clone(), span)?.code,
                        _ => self.compare(*op, &left, &right, span)?,
                    };
                    (left, right, holds)
                };
                self.line(format!("if (!({})) {{", bare(&holds)));
                self.indent += 1;
                let text = self.assert_text(message)?;
                let (left, right) = (self.shown(&left, span)?, self.shown(&right, span)?);
                self.line(format!("zeno_test_false({text}, {}, {left}, {right}, {at});", c_string(op.text())));
            }
            _ => {
                let holds = self.expr(condition)?;
                self.line(format!("if (!({})) {{", bare(&holds.code)));
                self.indent += 1;
                let text = self.assert_text(message)?;
                self.line(format!("zeno_test_false({text}, NULL, NULL, NULL, {at});"));
            }
        }
        self.line(format!("{failed} = true;"));
        self.indent -= 1;
        self.line("}");
        let release = self.text_end(begin);
        self.own_end(own, true);
        if let Some(release) = release {
            self.line(release);
        }
        self.leave_test(&failed, span)
    }

    /// A value that a failed `assert` shows as well as compares: in a
    /// variable of its own, so that it is worked out once.
    fn twice(&mut self, val: Val, span: Span) -> Result<Val, Error> {
        let named = val.code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if named || c_type(&val.kind, span).is_err() {
            return Ok(val);
        }
        self.temp(val, span)
    }

    /// One side of a comparison, as a failed `assert` shows it: as `print`
    /// shows it inside a list, so that text has its quotes.
    fn shown(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        match val.kind {
            Kind::None => Ok(c_string("none")),
            _ => self.repr(val, span),
        }
    }

    /// The text after the comma of an `assert`, as C, or `NULL`.
    fn assert_text(&mut self, message: Option<&Expr>) -> Result<String, Error> {
        match message {
            Some(message) => Ok(self.expr(message)?.code),
            None => Ok("NULL".to_string()),
        }
    }

    /// After a failed `assert`: leave the test as `return` would.
    fn leave_test(&mut self, failed: &str, span: Span) -> Result<(), Error> {
        self.line(format!("if ({failed}) {{"));
        self.indent += 1;
        self.stmt(&Stmt { kind: StmtKind::Return(None), span })?;
        self.indent -= 1;
        self.line("}");
        Ok(())
    }

    /// `assert raises Kind { ... }`, in a test: a `try` whose body must
    /// raise that kind. Raising nothing, or another kind, is reported as
    /// `zeno test` reports it, and leaves the test.
    fn test_assert_raises(&mut self, kind: &str, body: &[Stmt], span: Span) -> Result<(), Error> {
        // `except Error` takes every kind.
        let wanted = match kind {
            "Error" => None,
            class if self.classes.contains_key(class) => Some(class),
            kind if ERROR_KINDS.contains(&kind) => Some(kind),
            other => return Err(cannot(format!("`assert raises {other}`"), span)),
        };
        let (failed, at, shown) = (self.new_temp(), self.at(span), c_string(kind));
        if !self.firmware {
            let place = self.new_temp();
            self.line(format!("volatile bool {failed} = false;"));
            self.line(format!("zeno_try {place};"));
            self.line(format!("{place}.outer = zeno_trying;"));
            self.line(format!("zeno_trying = &{place};"));
            self.line(format!("if (zeno_setjmp({place}.place) == 0) {{"));
            self.tries.push(place.clone());
            self.try_scopes.push(self.scopes.len());
            let result = self.block(body);
            self.try_scopes.pop();
            self.tries.pop();
            result?;
            self.indent += 1;
            self.line(format!("zeno_trying = {place}.outer;"));
            self.line(format!("zeno_test_raised_nothing({shown}, {at});"));
            self.line(format!("{failed} = true;"));
            self.indent -= 1;
            if let Some(wanted) = wanted {
                self.line(format!("}} else if (!zeno_caught({})) {{", c_string(wanted)));
                self.line(format!("    zeno_test_raised_other({shown}, zeno_raised, {at});"));
                self.line(format!("    {failed} = true;"));
            }
            self.line("}");
            return self.leave_test(&failed, span);
        }

        // A firmware build: the error travels as a value, and a failure in
        // the body comes forward to the label (see `try_with_values`).
        self.labels += 1;
        let (label, caught) = (format!("zeno_except_{}", self.labels), format!("zeno_caught_{}", self.labels));
        self.line(format!("bool {failed} = false;"));
        self.tries.push(label.clone());
        self.try_scopes.push(self.scopes.len());
        self.arriving.push(BTreeSet::new());
        let text_top = (!self.collector).then(|| {
            self.marks += 1;
            format!("m{}", self.marks)
        });
        if let Some(mark) = &text_top {
            self.line(format!("size_t {mark} = zeno_text_top;"));
        }
        self.line("{");
        let result = self.block(body);
        self.line("}");
        self.try_scopes.pop();
        self.tries.pop();
        self.arriving.pop();
        result?;
        self.jumped.insert(label.clone());
        self.line(format!("zeno_test_raised_nothing({shown}, {at});"));
        self.line(format!("{failed} = true;"));
        self.line(format!("{label}: ;"));
        if let Some(mark) = &text_top {
            self.line(format!("zeno_text_top = {mark};"));
        }
        self.line("if (zeno_raised != NULL) {");
        self.indent += 1;
        self.line(format!("zeno_error *{caught} = zeno_raised;"));
        self.line("zeno_raised = NULL;");
        if let Some(wanted) = wanted {
            self.line(format!("if (!zeno_is({caught}, {})) {{", c_string(wanted)));
            self.line(format!("    zeno_test_raised_other({shown}, {caught}, {at});"));
            self.line(format!("    {failed} = true;"));
            self.line("}");
        }
        if !self.collector {
            self.line(format!("zeno_error_drop({caught});"));
        } else {
            self.line(format!("(void){caught};"));
        }
        self.indent -= 1;
        self.line("}");
        self.leave_test(&failed, span)
    }

    // ------------------------------------------- Errors as values (firmware)

    /// Something here can fail in a firmware build: a `raise`, or a call to
    /// a function that can raise, which has just given back `false`. The
    /// error is already noted in `zeno_raised`. Returns the C statement
    /// that sends control where the error goes next:
    ///
    /// - inside a `try`, to its `except` blocks, further down this function;
    /// - in a function that can raise, back to its caller, as `false`;
    /// - at the top level, nobody is left to catch it.
    ///
    /// `kinds` is what may have been raised, so that the `try` around this
    /// point knows what can reach it.
    fn failed(&mut self, kinds: &[String], span: Span) -> Result<String, Error> {
        if let Some(arriving) = self.arriving.last_mut() {
            arriving.extend(kinds.iter().cloned());
        }
        // Leaving this way ends blocks early, like `break` and `return`
        // do: free what their owners hold on the way out.
        let leave = |frees: Vec<String>, jump: String| match frees.is_empty() {
            true => jump,
            false => format!("{{ {} {jump} }}", frees.join(" ")),
        };
        if let Some(label) = self.tries.last().cloned() {
            self.jumped.insert(label.clone());
            let inside = self.try_scopes.last().copied().unwrap_or(self.scopes.len());
            return Ok(leave(self.raised_kept(inside, self.frees(inside)), format!("goto {label};")));
        }
        if self.fallible {
            return Ok(leave(self.raised_kept(self.base, self.frees(self.base)), "return false;".to_string()));
        }
        if self.returns.is_none() {
            // Nobody catches it. The resources of the top of the file are
            // released on its way out, as `zeno run` releases them; the
            // rest is left as it is (decision 59.1).
            let releases = self.resources.iter().rev().map(|(_, release)| release.clone()).collect();
            return Ok(leave(self.raised_kept(0, releases), "zeno_uncaught();".to_string()));
        }
        // The checker makes a function say what it raises, and a function
        // written where a value goes that raises is made as one that can.
        Err(cannot("an error raised in a function that does not say it raises", span))
    }

    /// The error on its way travels in `zeno_raised`. A `release` that
    /// these lines call may catch an error of its own inside, which is
    /// noted there too: so when they release a resource of `scopes[from..]`,
    /// the error is kept aside while they run and put back after.
    fn raised_kept(&self, from: usize, lines: Vec<String>) -> Vec<String> {
        match self.resources.iter().any(|(at, _)| *at >= from) {
            true => self.keep_raised(lines),
            false => lines,
        }
    }

    /// These lines, with the error on its way kept aside while they run.
    fn keep_raised(&self, lines: Vec<String>) -> Vec<String> {
        let mut kept = vec!["zeno_error *zeno_kept = zeno_raised;".to_string()];
        if !self.minimal {
            kept.push("int zeno_kept_line = zeno_raised_line, zeno_kept_col = zeno_raised_col;".to_string());
        }
        kept.extend(lines);
        kept.push("zeno_raised = zeno_kept;".to_string());
        if !self.minimal {
            kept.push("zeno_raised_line = zeno_kept_line; zeno_raised_col = zeno_kept_col;".to_string());
        }
        kept
    }

    /// Is this the type of a function that is called as one that can
    /// raise? Only in a firmware build, where it gives back `true` or
    /// `false` (5.5l). Every other build calls both kinds alike (56.5).
    fn raising(&self, kind: &FunctionKind) -> bool {
        self.firmware && !kind.raises.is_empty()
    }

    /// A call through the function value in `function` (a plain name),
    /// in a loop that `over_list` writes. For a function that can raise,
    /// in a firmware build, the call is tested on a line of its own, and
    /// a failure leaves as any failure does, freeing what is held.
    /// Gives those lines, and the C of what the call gives back.
    fn through(
        &mut self,
        function: &str,
        kind: &FunctionKind,
        mut args: Vec<String>,
        span: Span,
    ) -> Result<(Vec<String>, String), Error> {
        if !self.raising(kind) {
            return Ok((Vec::new(), invoke(function, kind, false, &args, span)?));
        }
        let leave = self.failed(&kind.raises, span)?;
        let mut lines = Vec::new();
        let answer = match kind.returns {
            Kind::None => String::new(),
            _ => {
                let answer = self.new_temp();
                lines.push(format!("{}{answer};", c_type(&kind.returns, span)?));
                args.push(format!("&{answer}"));
                answer
            }
        };
        lines.push(format!("if (!{}) {leave}", invoke(function, kind, true, &args, span)?));
        Ok((lines, answer))
    }

    /// A call, in a firmware build, to something that gives back `true` or
    /// `false` and puts its answer where the last argument points:
    ///
    /// ```c
    /// double t3;
    /// if (!z_parse(z_text, &t3)) goto zeno_except_1;   // or: return false;
    /// ```
    ///
    /// `call` is the C call without the answer's pointer.
    fn checked_call(&mut self, call: String, returns: Kind, kinds: &[String], span: Span) -> Result<Val, Error> {
        let leave = self.failed(kinds, span)?;
        let open = call.strip_suffix(')').expect("a call ends with `)`");
        let mut val = if returns == Kind::None {
            self.line(format!("if (!{call}) {leave}"));
            Val::plain(String::new(), Kind::None)
        } else {
            let answer = self.new_temp();
            self.line(format!("{}{answer};", c_type(&returns, span)?));
            let comma = if open.ends_with('(') { "" } else { ", " };
            self.line(format!("if (!{open}{comma}&{answer})) {leave}"));
            // With a collector, text that a function gives back will
            // last. With none it is in the text room, like text this
            // statement built itself.
            let val = Val::plain(answer, returns);
            if !self.collector && is_text(first_value(&val.kind)) { val } else { val.fixed() }
        };
        val.pre_fails = true;
        val.pre_calls = true;
        Ok(val)
    }

    /// `try` / `except` in a firmware build. Nothing jumps between
    /// functions. Each thing in the body that can fail is followed by a
    /// test, and a failure goes forward to the `except` blocks:
    ///
    /// ```c
    /// {
    ///     double t1;
    ///     if (!z_parse(z_line, &t1)) goto zeno_except_1;
    ///     ...the rest of the body...
    /// }
    /// zeno_except_1: ;
    /// if (zeno_raised != NULL) {                 // did the body fail?
    ///     zeno_error *zeno_caught_1 = zeno_raised;
    ///     zeno_raised = NULL;                    // it is being dealt with
    ///     if (zeno_is(zeno_caught_1, "ValueError")) {
    ///         ...the except block...
    ///     } else {
    ///         zeno_raised = zeno_caught_1;       // not ours: pass it on
    ///         return false;
    ///     }
    /// }
    /// ```
    ///
    /// The `goto` only ever goes forward, to a label in a block around it.
    fn try_with_values(&mut self, body: &[Stmt], handlers: &[Handler], span: Span) -> Result<(), Error> {
        self.labels += 1;
        let (label, caught) = (format!("zeno_except_{}", self.labels), format!("zeno_caught_{}", self.labels));
        self.tries.push(label.clone());
        self.try_scopes.push(self.scopes.len());
        self.arriving.push(BTreeSet::new());
        // With no collector: an error can leave a statement half way
        // through, with its text still in the text room. Whatever the
        // body leaves there is given back where the error is caught.
        let text_top = (!self.collector).then(|| {
            self.marks += 1;
            format!("m{}", self.marks)
        });
        if let Some(mark) = &text_top {
            self.line(format!("size_t {mark} = zeno_text_top;"));
        }
        self.line("{");
        let result = self.block(body);
        self.line("}");
        self.try_scopes.pop();
        self.tries.pop();
        let arrived = self.arriving.pop().unwrap_or_default();
        result?;

        // Only an error that travels as a value can reach an `except`. A
        // division by zero or a bad index stops a firmware build instead,
        // and an `except` waiting for one would wait for ever.
        const WHY: &str = "In a firmware build, an error the program raises itself (and `int` or `float` of text) travels as a value and can be caught. A division by zero, a bad index, a key that is not in a dict or an overflow stops the program. Test for it before it happens";
        if arrived.is_empty() {
            return Err(firmware_cannot(format!("nothing in this `try` can be caught. {WHY}"), span));
        }
        let anything = arrived.contains("Error");

        self.line(format!("{label}: ;"));
        if let Some(mark) = &text_top {
            self.line(format!("zeno_text_top = {mark};"));
        }
        // If the body always leaves by its last line, the only way to get
        // here is the `goto`, and there is nothing to ask.
        let leaves = leaves(body.last());
        self.line(if leaves { "{" } else { "if (zeno_raised != NULL) {" });
        self.indent += 1;
        self.line(format!("zeno_error *{caught} = zeno_raised;"));
        self.line("zeno_raised = NULL;");
        self.line(format!("(void){caught};"));
        let mut handled: Vec<&str> = Vec::new();
        let mut catches_everything = false;
        for (index, handler) in handlers.iter().enumerate() {
            let (wanted, holds) = match handler.error.as_deref() {
                None | Some("Error") => (None, Kind::Error),
                Some(class) if self.classes.contains_key(class) => (Some(class), Kind::Class(class.to_string())),
                Some(kind) if ERROR_KINDS.contains(&kind) => (Some(kind), Kind::Error),
                Some(other) => return Err(cannot(format!("`except {other}`"), span)),
            };
            if let Some(kind) = wanted {
                // An `except` for a class that nothing here raises is idle
                // in every kind of build. One for a built-in kind is most
                // likely waiting for the runtime to raise it.
                if !anything && !arrived.contains(kind) && !self.classes.contains_key(kind) {
                    return Err(firmware_cannot(format!("nothing in this `try` can reach `except {kind}`. {WHY}"), span));
                }
                handled.push(kind);
            }
            let test = wanted.map(|kind| format!("if (zeno_is({caught}, {})) ", c_string(kind))).unwrap_or_default();
            self.line(if index == 0 { format!("{test}{{") } else { format!("}} else {test}{{") });
            self.scopes.push(HashMap::new());
            self.indent += 1;
            // With no collector this block has received the error and
            // owns it. However the block is left, the error is freed on
            // the way out, unless the block gave it on: kept it, returned
            // it or raised it again.
            let named = handler.binding.is_some();
            if !self.collector && !(named && holds == Kind::Error) {
                self.hold(Kind::Error, &caught);
            }
            if let Some(name) = &handler.binding {
                let c_type = c_type(&holds, span)?;
                let value = match holds {
                    Kind::Class(_) => format!("({}){caught}->object", c_type.trim_end()),
                    _ => caught.clone(),
                };
                let object = matches!(holds, Kind::Class(_));
                let c_name = self.declare(name, holds);
                self.line(format!("{c_type}{c_name} = {value};"));
                self.line(format!("(void){c_name};"));
                if !self.collector {
                    // The name is the owner. Of an object that was
                    // raised, it owns the object, which is taken out of
                    // the error it travelled in.
                    if object {
                        self.line(format!("{caught}->object = NULL;"));
                    }
                    if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(name)) {
                        variable.owner = true;
                    }
                }
            }
            let result = handler.body.iter().try_for_each(|statement| self.stmt(statement));
            if result.is_ok() {
                self.end_of_block(&handler.body);
            }
            self.indent -= 1;
            self.scopes.pop();
            result?;
            if wanted.is_none() {
                catches_everything = true;
                break; // nothing gets past an `except` that takes every error
            }
        }
        // What no `except` here takes carries on outward.
        let passed_on: Vec<String> = arrived.iter().filter(|kind| !handled.contains(&kind.as_str())).cloned().collect();
        if !catches_everything {
            let leave = if passed_on.is_empty() {
                // Every kind the body can raise has an `except` above, so
                // this cannot happen. It is written out all the same: C
                // cannot see that, and coding standards want every chain
                // of `else if` to end in an `else`.
                "zeno_uncaught();".to_string()
            } else {
                self.failed(&passed_on, span)?
            };
            self.line("} else {");
            self.indent += 1;
            self.line(format!("zeno_raised = {caught};"));
            self.line(leave);
            self.indent -= 1;
        }
        self.line("}");
        self.indent -= 1;
        self.line("}");
        Ok(())
    }

    /// The kind a written type stands for, where the compiler needs to know
    /// it before it has a value to look at: `let xs: list[int] = []`.
    fn kind_of_type(&self, ty: &Type) -> Option<Kind> {
        // Inside a copy of a generic, `T` is whatever the copy was made
        // for, and `Stack[int]` is a copy itself. Only the checker knows
        // those, so its answer is used where it has one.
        if let Some(kind) = self.types.annotation(ty) {
            return Some(disowned(kind.clone()));
        }
        let (name, args) = match ty {
            Type::Named { name, args } => (name, args),
            // Who owns a value does not change how C holds it.
            Type::Owned(inner) | Type::Take(inner) => return self.kind_of_type(inner),
            Type::Length(_) => return None,
            Type::Optional(inner) => return Some(Kind::Optional(Box::new(self.kind_of_type(inner)?))),
            Type::Several(types) => {
                return Some(Kind::Several(types.iter().map(|ty| self.kind_of_type(ty)).collect::<Option<Vec<Kind>>>()?))
            }
            Type::Function { params, returns, raises } => {
                let params = params.iter().map(|param| self.kind_of_type(param)).collect::<Option<Vec<Kind>>>()?;
                let returns = match returns {
                    Some(returns) => self.kind_of_type(returns)?,
                    None => Kind::None,
                };
                let raises = raises.iter().map(|kind| kind.to_string()).collect();
                return Some(Kind::Function(Box::new(FunctionKind { params, returns, raises })));
            }
        };
        Some(match (name.as_str(), args.as_slice()) {
            ("int", []) => Kind::Int,
            ("float", []) => Kind::Float,
            ("str", []) => Kind::Str,
            ("bool", []) => Kind::Bool,
            ("list", [item]) | ("list", [item, Type::Length(_)]) => match self.kind_of_type(item)? {
                // A list of bytes keeps one byte to an item (36.2).
                Kind::Int if matches!(item, Type::Named { name, .. } if name == "u8") => Kind::List(Box::new(Kind::Byte)),
                item => Kind::List(Box::new(item)),
            },
            ("dict", [key, value]) => {
                Kind::Dict(Box::new(self.kind_of_type(key)?), Box::new(self.kind_of_type(value)?))
            }
            (name, []) if self.classes.contains_key(name) => Kind::Class(name.to_string()),
            (name, []) if self.enums.contains_key(name) => Kind::Enum(name.to_string()),
            (name, []) if units::is_unit(name) => Kind::Quantity(name.to_string()),
            ("Pin", []) => Kind::Pin,
            ("InputPin", []) => Kind::InputPin,
            ("Serial", []) => Kind::Serial,
            ("AnalogPin", []) => Kind::AnalogPin,
            ("PwmPin", []) => Kind::PwmPin,
            ("Ticker", []) => Kind::Ticker,
            ("Interrupt", []) => Kind::Interrupt,
            // `rising`, `falling` and `either` are the strs of their names.
            ("Edge", []) => Kind::Str,
            // `pull_up` and `pull_down` are the strs of their names.
            ("Pull", []) => Kind::Str,
            ("pointer", [Type::Named { name, args }]) if args.is_empty() => Kind::Pointer(Pointee::named(name)?),
            ("Register", [Type::Named { name, args }]) if args.is_empty() => Kind::Register(ints::width(name)?),
            // `u8` and the rest: an int that is known to be small.
            (name, []) if ints::width(name).is_some() => Kind::Int,
            _ => return None,
        })
    }

    /// `expr`, for a place that knows what kind it wants. Only an empty
    /// list needs the help: `[]` alone does not say what it will hold.
    fn expr_as(&mut self, expr: &Expr, expected: Option<&Kind>) -> Result<Val, Error> {
        match (&expr.kind, expected) {
            (ExprKind::List(items), Some(kind @ Kind::List(_))) if items.is_empty() => {
                let mut val = Val::plain("zeno_list_new(0)", kind.clone());
                val.made = true;
                self.belongs(val, expr)
            }
            (ExprKind::Dict(entries), Some(kind @ Kind::Dict(key, value))) if entries.is_empty() => {
                let (key, value) = self.dict_letters(key, value, expr.span)?;
                let mut val = Val::plain(format!("zeno_dict_new('{key}', '{value}')"), kind.clone());
                val.made = true;
                self.belongs(val, expr)
            }
            _ => self.expr(expr),
        }
    }

    /// `x = v`, `point.x = v`, `items[i] = v`, and the same with `+=`.
    /// `let q, r = divide(7, 2)`, `a, b = b, a` (decision 57). All of the
    /// right is worked out first, each value held in a temporary and kept
    /// as a `let` keeps it; then each place is filled, left to right. A
    /// call gives its first value as its answer, and the others where the
    /// pointers after its arguments say:
    ///
    /// ```c
    /// int64_t t2;
    /// int64_t t1 = z_divide(7, 2, &t2);
    /// int64_t z_q = t1;
    /// int64_t z_r = t2;
    /// ```
    fn receive(&mut self, declare: Option<bool>, places: &[Place], value: &Expr, top_level: bool, span: Span) -> Result<(), Error> {
        let expected: Vec<Option<Kind>> = places
            .iter()
            .map(|place| match place {
                Place::Name { ty: Some(ty), .. } => self.kind_of_type(ty),
                _ => None,
            })
            .collect();
        let dropped = |at: usize| matches!(places.get(at), Some(Place::Drop));
        let mut vals = Vec::new();
        match &value.kind {
            ExprKind::Several(values) => {
                let mut held = Vec::new();
                for (at, value) in values.iter().enumerate() {
                    let val = self.expr_as(value, expected.get(at).and_then(Option::as_ref))?;
                    let val = match dropped(at) {
                        true => val,
                        false => {
                            self.stays(&val, value.span)?;
                            self.keep(val, is_written(value))
                        }
                    };
                    let val = if val.kind == Kind::None { val } else { self.temp(val, value.span)? };
                    held.extend(self.hold_for_later(&val, !dropped(at) && !is_written(value), &values[at + 1..]));
                    vals.push((val, false));
                }
                for key in &held {
                    self.let_go(key);
                }
            }
            _ => {
                let (start, indent) = (self.lines.len(), self.indent);
                let outs: Vec<String> = (1..places.len()).map(|_| self.new_temp()).collect();
                // What was there is put back after: a function written among the
                // arguments has `return`s of its own (decision 57).
                let outer = self.outs.replace(outs.iter().map(|out| format!("&{out}")).collect());
                let val = self.expr(value);
                self.outs = outer;
                let mut val = val?;
                let Kind::Several(kinds) = val.kind.clone() else {
                    return Err(cannot("this on the right of several places", value.span));
                };
                for (out, kind) in outs.iter().zip(&kinds[1..]).rev() {
                    self.lines.insert(start, (indent, format!("{}{out};", c_type(kind, span)?)));
                }
                val.kind = kinds[0].clone();
                if !dropped(0) {
                    self.stays(&val, value.span)?;
                    val = self.keep(val, false);
                }
                vals.push((self.temp(val, value.span)?, false));
                // The function kept a copy of each later text for whoever
                // receives it.
                for (out, kind) in outs.into_iter().zip(&kinds[1..]) {
                    vals.push((Val::plain(out, kind.clone()).fixed(), true));
                }
            }
        }
        for ((place, (val, copied)), expected) in places.iter().zip(vals).zip(expected) {
            match place {
                Place::Name { name, .. } => {
                    let val = match &expected {
                        Some(expected) => self.coerce(val, expected, span)?,
                        None => val,
                    };
                    let owner = self.types.owner_at(place) || (!self.collector && is_text(&val.kind));
                    self.bind(name, val, declare == Some(true), owner, top_level, span)?;
                }
                Place::Target(target) => self.store(target, val, span, span)?,
                // What nobody receives is given back: an owned value, and
                // with no collector a text the function kept a copy of.
                Place::Drop => {
                    if self.types.discarded_at(place) {
                        self.line(self.drops.code(&val.kind, &val.code));
                    } else if !self.collector && is_text(&val.kind) && copied {
                        self.line(format!("zeno_text_drop({});", val.code));
                    }
                }
            }
        }
        Ok(())
    }

    /// Declare the variable `name` with `val`, worked out and kept
    /// already: what a `let` does once it has its value. `at` is where
    /// the value is written.
    fn bind(&mut self, name: &str, val: Val, mutable: bool, owner: bool, top_level: bool, at: Span) -> Result<(), Error> {
        if is_empty_list(&val.kind) {
            return Err(cannot(
                "an empty list with no type beside it. Write `let items: list[int] = []`",
                at,
            ));
        }
        if is_empty_dict(&val.kind) {
            return Err(cannot(
                "an empty dict with no type beside it. Write `let ages: dict[str, int] = [:]`",
                at,
            ));
        }
        let c_type = c_type(&val.kind, at)?;
        if top_level {
            // A top-level variable is one that functions can see: a
            // C global, given its value when `main` gets this far.
            let is_root = holds_pointer(&val.kind);
            let c_name = self.declare(name, val.kind);
            self.globals.push(format!("static {c_type}{c_name};"));
            if is_root {
                self.roots.push(c_name.clone());
            }
            self.line(format!("{c_name} = {};", val.code));
        } else if mutable && self.shared.contains(name) && self.collector {
            // A function written inside this one uses the variable
            // too, and either may change it. So it lives in a box on
            // the heap, and both hold a pointer to the box.
            let pointer = self.c_name(name);
            let size = c_type.trim_end();
            self.line(format!("{c_type}*{pointer} = ({c_type}*)zeno_alloc(sizeof({size}));"));
            self.line(format!("*{pointer} = {};", val.code));
            let variable = Variable {
                c_name: format!("(*{pointer})"),
                kind: val.kind,
                mutable,
                boxed: Some(pointer),
                used: Cell::new(false),
                owner: false,
                looks: false,
                inner: None,
            };
            self.scopes.last_mut().expect("there is always a scope").insert(name.to_string(), variable);
        } else {
            // See `changed_in_try` for what `volatile` is doing here.
            //
            // An owner needs the same care for a different reason.
            // Giving its value away sets it to NULL. If that
            // happens inside a `try` and an error then jumps back
            // here, C does not promise the NULL is still there
            // unless the variable is `volatile`, and the block's
            // end would free a value that is no longer its own.
            let changes = mutable && self.volatile.contains(name);
            let gives = owner && self.has_try && !self.firmware;
            let careful = if changes || gives { "volatile " } else { "" };
            let c_name = self.declare(name, val.kind);
            self.line(format!("{c_type}{careful}{c_name} = {};", val.code));
        }
        if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(name)) {
            variable.mutable = mutable;
            variable.owner = owner;
        }
        Ok(())
    }

    fn assign(&mut self, target: &Expr, op: Option<BinaryOp>, value: &Expr, span: Span) -> Result<(), Error> {
        let val = match op {
            None => self.expr(value)?,
            // `x += 1` is `x = x + 1`.
            Some(op) => {
                let mut vals = self.sequence(&[target, value])?;
                let right = vals.pop().expect("two values");
                let left = vals.pop().expect("two values");
                self.arithmetic(op, left, right, span)?
            }
        };
        self.stays(&val, value.span)?;
        let val = self.keep(val, op.is_none() && is_written(value));
        self.store(target, val, value.span, span)
    }

    /// Put `val`, worked out and kept already, in the place `target`.
    /// `at` is where the value is written.
    fn store(&mut self, target: &Expr, val: Val, at: Span, span: Span) -> Result<(), Error> {
        // With no collector, a list that was just made (`xs += [4]` makes
        // one) has to arrive at an owner, which frees the list it had.
        let needs_owner = val.made && !self.collector;
        match &target.kind {
            ExprKind::Name(name) => {
                let Some(variable) = self.lookup(name) else {
                    return Err(cannot(format!("an assignment to `{name}`"), target.span));
                };
                let (c_name, kind, owner) = (variable.c_name.clone(), variable.kind.clone(), variable.owner);
                if needs_owner && !owner {
                    return Err(Self::no_owner(at));
                }
                let mut val = self.coerce(val, &kind, span)?;
                if owner {
                    // The new value is made first, since making it may
                    // read the old one. Then the old one is freed.
                    val = self.temp(val, span)?;
                    self.line(self.drops.code(&kind, &c_name));
                }
                self.line(format!("{c_name} = {};", val.code));
            }
            // For the other two, Zeno works the new value out first and then
            // looks up where it goes. If working the value out calls a
            // function, pin it down before touching the place.
            // The object may be reached through a list, as in
            // `points[1].x = 30`. Finding it can fail, so it is found
            // once, after the value, and pinned down as well.
            ExprKind::Field { object: reached, name, .. } => {
                let val = if val.calls || !is_place(reached) { self.temp(val, span)? } else { val };
                let object = self.expr(reached)?;
                let object = if is_place(reached) { object } else { self.temp(object, span)? };
                let field = match &object.kind {
                    Kind::Class(class) => self.classes[class].fields.iter().find(|(field, _, _)| field == name),
                    _ => None,
                };
                let Some((_, kind, _)) = field else {
                    return Err(cannot("an assignment to this", target.span));
                };
                let kind = kind.clone();
                let mut val = self.coerce(val, &kind, span)?;
                let owned = matches!(&object.kind, Kind::Class(class) if self.classes[class].owned_fields.contains(name))
                    || (!self.collector && is_text(&kind));
                if needs_owner && !owned {
                    return Err(Self::no_owner(at));
                }
                if owned {
                    // As for an owner variable: new value first, then free
                    // the one the field held.
                    val = self.temp(val, span)?;
                    self.line(self.drops.code(&kind, &format!("{}->z_{name}", object.code)));
                }
                let sized = match &object.kind {
                    Kind::Class(class) => self.classes[class].sized.get(name).copied(),
                    _ => None,
                };
                match sized {
                    Some(width) => {
                        self.line(format!("{}->z_{name} = ({}){};", object.code, width.c_type(), val.code))
                    }
                    None => self.line(format!("{}->z_{name} = {};", object.code, val.code)),
                }
            }
            ExprKind::Index { object, index } => {
                // A bare `none` is nothing to pin down, and has no C type
                // until it is known what else the list holds.
                let val = if val.kind == Kind::None { val } else { self.temp(val, span)? };
                // The list may itself be an item of a list, as in
                // `grid[1][0] = 5`: then finding it can fail too.
                let mut vals = self.sequence(&[object, index])?;
                let index = vals.pop().expect("two values");
                let object = vals.pop().expect("two values");
                // Through a pointer: the item is stored at its own size,
                // and nothing is checked. That is what `unsafe` is for.
                if let Kind::Pointer(pointee) = &object.kind {
                    let val = self.coerce(val, &pointee.kind(), span)?;
                    self.line(format!("{}[{}] = ({}){};", object.code, index.code, pointee.c_type(), val.code));
                    return Ok(());
                }
                // `ages["Ada"] = 36`. With no collector the value is
                // already a copy that the dict can call its own (`keep`,
                // above), and the dict frees the one it had.
                if let Kind::Dict(key, item) = &object.kind {
                    let val = self.coerce(val, item, span)?;
                    let (key, slot) = (to_slot(key, &index.code, span)?, to_slot(item, &val.code, span)?);
                    // A dict that owns its values frees the one that was
                    // under the key (decision 56.12).
                    let set = match self.drops.owns(item) {
                        true => self.drops.dict_helper("put", item),
                        false => "zeno_dict_set".to_string(),
                    };
                    self.line(format!("{set}({}, {key}, {slot});", object.code));
                    return Ok(());
                }
                let Kind::List(item) = &object.kind else {
                    return Err(cannot("an assignment to this", target.span));
                };
                let item = (**item).clone();
                // The list owns an item that has one owner. Any other
                // list has no use for one that was just made.
                let owns = self.drops.owns(&item);
                if needs_owner && !owns {
                    return Err(Self::no_owner(at));
                }
                let val = self.coerce(val, &item, span)?;
                let slot = self.kept_slot(&val.kind, &val.code, span)?;
                if owns {
                    // The item that was there is freed, as a field's
                    // old value is, and the new one takes its place.
                    let place = self.new_temp();
                    self.line(format!("zeno_slot *{place} = {};", self.slot_address(&object.code, &index.code, span)));
                    self.line(self.drops.code(&item, &from_slot(&item, &format!("(*{place})"), span)?));
                    self.line(format!("*{place} = {slot};"));
                } else if !self.collector && is_text(&val.kind) {
                    // The list is done with the text it held there.
                    let place = self.new_temp();
                    self.line(format!("zeno_slot *{place} = {};", self.slot_address(&object.code, &index.code, span)));
                    self.line(format!("zeno_text_drop({place}->s);"));
                    self.line(format!("*{place} = {slot};"));
                } else if !self.collector && is_boxed(&val.kind) {
                    // And with the box it kept the number in (56.14). The
                    // new box is made first, as the LLVM build makes it,
                    // so that both use the pool alike.
                    let (boxed, place) = (self.new_temp(), self.new_temp());
                    self.line(format!("zeno_slot {boxed} = {slot};"));
                    self.line(format!("zeno_slot *{place} = {};", self.slot_address(&object.code, &index.code, span)));
                    self.line(format!("zeno_free({place}->p);"));
                    self.line(format!("*{place} = {boxed};"));
                } else {
                    self.line(format!("{} = {slot};", self.slot_at(&object.code, &index.code, &item, span)));
                }
            }
            _ => return Err(cannot("an assignment to this", target.span)),
        }
        Ok(())
    }

    /// `match`: each `case` becomes one branch of an `if ... else if` chain.
    /// A pattern gives two things: tests the value must pass (is it this
    /// case?), and new variables for the parts the pattern names.
    fn match_statement(&mut self, subject: &Expr, arms: &[Arm], span: Span) -> Result<(), Error> {
        // With no collector, text built to find the value is needed until
        // a case has been chosen, and is given back as that case begins.
        let begin = self.text_begin();
        let subject = self.expr(subject)?;
        let release = self.text_end(begin);
        let subject = self.temp(subject, span)?;
        let mut chosen = arms.is_empty();
        for (index, arm) in arms.iter().enumerate() {
            let mut tests = Vec::new();
            let mut names = Vec::new();
            self.pattern(&arm.pattern, &subject.code, &subject.kind, &mut tests, &mut names, span)?;
            // The checker has made sure the cases of an enum are all
            // covered. So if the last `case` only asks "is it this case?",
            // the answer is already yes, and it can be a plain `else`.
            let last = index + 1 == arms.len();
            if last && matches!(subject.kind, Kind::Enum(_)) && tests.len() == 1 {
                tests.clear();
            }
            let test = tests.join(" && ");
            self.line(match (index, tests.is_empty()) {
                (0, true) => "if (true) {".to_string(),
                (0, false) => format!("if ({test}) {{"),
                (_, true) => "} else {".to_string(),
                (_, false) => format!("}} else if ({test}) {{"),
            });
            self.scopes.push(HashMap::new());
            self.indent += 1;
            for (name, code, kind) in names {
                let c_type = c_type(&kind, span)?;
                let kept = !self.collector && is_text(&kind);
                let c_name = self.declare(&name, kind);
                if kept {
                    // The name outlives the text room's copy.
                    self.line(format!("{c_type}{c_name} = zeno_text_keep({code});"));
                    self.owns(&name);
                } else {
                    self.line(format!("{c_type}{c_name} = {code};"));
                }
            }
            if let Some(release) = &release {
                self.line(release.clone());
            }
            let result = arm.body.iter().try_for_each(|statement| self.stmt(statement));
            if result.is_ok() {
                self.end_of_block(&arm.body);
            }
            self.indent -= 1;
            self.scopes.pop();
            result?;
            if tests.is_empty() {
                chosen = true;
                break; // nothing after a pattern that takes everything can run
            }
        }
        match release {
            // No case was chosen: the text is given back all the same.
            Some(release) if !chosen => {
                self.line("} else {");
                self.indent += 1;
                self.line(release);
                self.indent -= 1;
                self.line("}");
            }
            Some(release) if arms.is_empty() => self.line(release),
            _ if !arms.is_empty() => self.line("}"),
            _ => {}
        }
        Ok(())
    }

    fn pattern(
        &mut self,
        pattern: &Pattern,
        code: &str,
        kind: &Kind,
        tests: &mut Vec<String>,
        names: &mut Vec<(String, String, Kind)>,
        span: Span,
    ) -> Result<(), Error> {
        // A value that may be `none` is a pointer that may be NULL, and a
        // case of an enum matches only a value that is there.
        let there = some(kind);
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Name(name) => match (self.case_in(name, there), there) {
                // A bare enum case, such as `Idle`, is compared.
                (Some((owner, index)), Kind::Enum(subject)) if owner == *subject && self.lookup(name).is_none() => {
                    if there != kind {
                        tests.push(format!("{code} != NULL"));
                    }
                    tests.push(format!("{code}->tag == {index}"));
                }
                // Any other name is a new variable holding the value.
                _ => names.push((name.clone(), code.to_string(), kind.clone())),
            },
            Pattern::Literal(literal) => {
                let literal = self.expr(literal)?;
                let Some(common) = joined(kind, &literal.kind) else {
                    return Err(cannot("a pattern of a different type from the value", span));
                };
                let subject = self.coerce(Val::plain(code, kind.clone()), &common, span)?;
                let literal = self.coerce(literal, &common, span)?;
                tests.push(self.equal(&common, &subject.code, &literal.code, span)?);
            }
            Pattern::Variant { name, fields } => {
                let (owner, index) = match (self.case_in(name, there), there) {
                    (Some((owner, index)), Kind::Enum(subject)) if owner == *subject => (owner, index),
                    _ => return Err(cannot(format!("the pattern `{name}(...)` here"), span)),
                };
                let carried = self.enums[&owner].cases[index].1.clone();
                if carried.len() != fields.len() {
                    return Err(cannot(format!("the pattern `{name}(...)` with this many parts"), span));
                }
                if there != kind {
                    tests.push(format!("{code} != NULL"));
                }
                tests.push(format!("{code}->tag == {index}"));
                for (field, (field_name, field_kind)) in fields.iter().zip(&carried) {
                    let place = format!("{code}->as.c{index}.z_{field_name}");
                    self.pattern(field, &place, field_kind, tests, names, span)?;
                }
            }
        }
        Ok(())
    }

    /// What follows the `}` of an `if`: nothing, `else`, or `else if`.
    fn else_part(&mut self, else_block: &Option<Block>) -> Result<(), Error> {
        let Some(block) = else_block else {
            self.line("}");
            return Ok(());
        };
        // The parser stores `else if` as an else block holding one `if`.
        if let [Stmt { kind: StmtKind::If { condition, then_block, else_block }, .. }] = block.as_slice() {
            let mark = self.lines.len();
            let condition = self.condition(condition)?;
            let before = self.lines.split_off(mark);
            if before.is_empty() {
                self.line(format!("}} else if ({}) {{", bare(&condition.code)));
                self.block(then_block)?;
                return self.else_part(else_block);
            }
            // The second condition needs statements of its own. They may
            // only run if the first condition was false, so they go inside
            // the `else`.
            self.line("} else {");
            self.lines.extend(before.into_iter().map(|(indent, line)| (indent + 1, line)));
            self.indent += 1;
            self.line(format!("if ({}) {{", bare(&condition.code)));
            self.block(then_block)?;
            self.else_part(else_block)?;
            self.indent -= 1;
            self.line("}");
            return Ok(());
        }
        self.line("} else {");
        self.block(block)?;
        self.line("}");
        Ok(())
    }

    /// The first line of a `for` loop: `for i in range(...)` or `for x in
    /// items`. It declares the loop variable in a scope of its own; the
    /// caller writes the body, the closing `}`, drops the scope, and gives
    /// what this returns to `close_loop`.
    ///
    /// With two names, `for key, value in ages`, `second` is the second:
    /// the first is then the key of a dict, or the position in a list, a
    /// str or a range, and the second is what is there.
    fn open_loop(&mut self, variable: &str, second: Option<&str>, iterable: &Expr) -> Result<Option<String>, Error> {
        let span = iterable.span;
        let range = match &iterable.kind {
            ExprKind::Call { callee, args }
                if matches!(&callee.kind, ExprKind::Name(name) if name == "range" && self.lookup(name).is_none())
                    && args.iter().all(|arg| arg.name.is_none()) =>
            {
                Some(args)
            }
            _ => None,
        };
        let Some(args) = range else {
            return self.open_list_loop(variable, second, iterable);
        };
        // A range becomes a plain counting loop: no list of numbers is built.
        let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        let mut bounds = Vec::new();
        for val in self.sequence(&exprs)? {
            // Worked out once, before the loop, as `range` itself would.
            let is_literal = val.code.bytes().all(|b| b.is_ascii_digit());
            bounds.push(if is_literal { val.code } else { self.temp(val, span)?.code });
        }

        self.scopes.push(HashMap::new());
        // With two names the second is the number, and the first counts
        // the trips from 0.
        let c_name = self.declare(second.unwrap_or(variable), Kind::Int);
        let trips = second.map(|_| self.declare(variable, Kind::Int));
        let counted = |emitter: &mut Self, from: String| {
            if let Some(trips) = &trips {
                emitter.indent += 1;
                emitter.line(format!("int64_t {trips} = {from};"));
                emitter.indent -= 1;
            }
        };
        match bounds.as_slice() {
            [stop] => {
                self.line(format!("for (int64_t {c_name} = 0; {c_name} < {stop}; {c_name}++) {{"));
                counted(self, c_name.clone());
            }
            [start, stop] => {
                self.line(format!("for (int64_t {c_name} = {start}; {c_name} < {stop}; {c_name}++) {{"));
                counted(self, format!("{c_name} - {start}"));
            }
            // With a step, count the trips first. Counting up or down to
            // `stop` directly could run past the end of what an int holds.
            [start, stop, step] => {
                let count = self.new_temp();
                let trip = self.new_temp();
                self.line(format!(
                    "int64_t {count} = zeno_range_count({start}, {stop}, {step}, {});",
                    self.at(span)
                ));
                self.line(format!("for (int64_t {trip} = 0; {trip} < {count}; {trip}++) {{"));
                self.indent += 1;
                self.line(format!("int64_t {c_name} = {start} + {trip} * {step};"));
                self.indent -= 1;
                counted(self, trip);
            }
            _ => return Err(cannot("this `range`", span)),
        }
        Ok(None)
    }

    /// After the `}` of a `for` loop. A loop over a list walks a copy made
    /// for it alone (see `open_list_loop`), and `walked` is that copy.
    /// Nothing else ever saw it, so it can be freed here and now instead
    /// of being left for the collector. (A `return` from inside the loop
    /// skips this line, and frees the copy itself: see `open_list_loop`.)
    fn close_loop(&mut self, walked: Option<String>) {
        // A loop over the pairs of a dict walked two lists.
        for drop in walked.iter().flat_map(|drops| drops.lines()) {
            self.line(drop);
        }
    }

    /// `for x in items`. The loop walks the list as it was when the loop
    /// began, so a body that adds to the list or removes from it cannot
    /// confuse the loop. That is what `zeno run` does too.
    ///
    /// `for key, value in ages` walks two lists side by side, the keys
    /// and the values, both made for the loop. `for i, item in items`
    /// walks the one list, and `i` is how far the walk has got.
    fn open_list_loop(&mut self, variable: &str, second: Option<&str>, iterable: &Expr) -> Result<Option<String>, Error> {
        let span = iterable.span;
        // The values of a dict, for a loop with two names: the list of
        // them, and what kind they are.
        let mut values: Option<(String, Kind)> = None;
        // Whether the list of values owns them: it does when the dict was
        // made right here and owns its values (decision 56.12).
        let mut values_owned = false;
        self.walked = Some(iterable as *const Expr as usize);
        let list = self.expr(iterable);
        self.walked = None;
        let list = list?;
        // A loop over a dict walks a list of its keys, made for the loop:
        //
        //     zeno_list *t1 = zeno_dict_keys(z_ages);
        //
        // A dict that was made right here (`for key in ["a": 1]`) is
        // nobody's, and has done its work once the keys are out of it.
        let list = match &list.kind {
            Kind::Dict(key, value) => {
                let keys_kind = Kind::List(key.clone());
                let key = (**key).clone();
                let value = (**value).clone();
                // The dict is asked twice, so it is worked out once.
                let list = match (second, list.made) {
                    (Some(_), false) => self.temp(list, span)?,
                    _ => list,
                };
                let mut keys = match list.made {
                    true => {
                        let dict = self.temp(list, span)?;
                        let keys = Val::from(format!("zeno_dict_keys({})", dict.code), keys_kind, &[&dict]);
                        let keys = self.temp(keys, span)?;
                        match second.is_some() {
                            // The values go over to their list, and the dict
                            // is freed without them.
                            true => {
                                let name = self.new_temp();
                                self.line(format!("zeno_list *{name} = zeno_dict_values({});", dict.code));
                                values_owned = self.drops.owns(&value);
                                values = Some((name, value));
                                self.line(format!("zeno_dict_drop({});", dict.code));
                            }
                            false => {
                                let kind = Kind::Dict(Box::new(key), Box::new(value));
                                let drop = self.drops.code(&kind, &dict.code);
                                self.line(drop);
                            }
                        }
                        keys
                    }
                    false if second.is_some() => {
                        let keys = Val::from(format!("zeno_dict_keys({})", list.code), keys_kind, &[&list]);
                        let keys = self.temp(keys, span)?;
                        let name = self.new_temp();
                        self.line(format!("zeno_list *{name} = zeno_dict_values({});", list.code));
                        values = Some((name, value));
                        keys
                    }
                    false => Val::from(format!("zeno_dict_keys({})", list.code), keys_kind, &[&list]),
                };
                keys.made = true;
                keys.passing = false;
                keys
            }
            _ => list,
        };
        // `for ch in text` walks the text where it is, one character at a
        // time, and builds no list of them. With no collector that needs
        // the text to stay where it is until the loop is over: a variable
        // the body leaves alone, or a literal.
        if list.kind == Kind::Str && second.is_none() {
            let stays = match &iterable.kind {
                ExprKind::Name(_) => self.text_stays,
                ExprKind::Str(_) => true,
                _ => false,
            };
            if self.collector || stays {
                return self.open_text_loop(variable, list, span);
            }
        }
        // A list made right here is nobody else's, so it needs no copy:
        // one written out, or what `split`, `map`, `filter` or `+` gave,
        // or an owned list that a function gave back.
        let fresh = list.made;
        let item = match &list.kind {
            Kind::List(item) => (**item).clone(),
            // A str is looped over one character at a time: the loop
            // walks a list of them, which is its own.
            Kind::Str => Kind::Str,
            _ => return Err(cannot("a `for` loop over anything but a list, a str or `range(...)`", span)),
        };
        // Nor does a list that the statement owns: nothing the body does
        // can reach it. The statement frees it, after the loop.
        //
        // Nor does a list the body cannot change: the copy would be the
        // list itself. The loop walks it where it is, and frees nothing.
        let own = (list.passing || (self.in_place && !fresh)) && list.kind != Kind::Str;
        let items = self.new_temp();
        let source = match (&list.kind, fresh || own) {
            (Kind::Str, _) => format!("zeno_chars({})", list.code),
            (_, true) => list.code,
            // With no collector a list of text has its own copies of its
            // items, and so does the copy the loop walks: the body may
            // give the list new items while the loop looks at the old.
            (_, false) if !self.collector && is_text(&item) => format!("zeno_list_copy_text({})", list.code),
            // The same for the boxes of numbers that may be `none` (56.14).
            (_, false) if self.keeps_copies(&item) => self.keep_items(&item, &format!("zeno_list_copy({})", list.code), span)?,
            (_, false) => format!("{}({})", list_fn(&item, "copy"), list.code),
        };
        self.line(format!("zeno_list *{items} = {source};"));

        let position = self.new_temp();
        self.scopes.push(HashMap::new());
        // The list being walked is the loop's own. A `return` or a failed
        // call inside the loop frees it on the way out; `close_loop` does
        // when the loop ends by itself.
        let copied = !(fresh || own) && list.kind != Kind::Str;
        match (own, copied) {
            (true, _) => {}
            (false, true) => {
                self.hold_copy(item.clone(), &items);
            }
            (false, false) => {
                self.hold(Kind::List(Box::new(item.clone())), &items);
            }
        }
        // The list of values is the loop's own as well. What is in it
        // still belongs to the dict.
        if let Some((list, value)) = &values {
            match values_owned {
                true => drop(self.hold(Kind::List(Box::new(value.clone())), list)),
                false => drop(self.hold_copy(value.clone(), list)),
            }
        }
        let c_type_of_item = c_type(&read_kind(&item), span)?;
        // With two names over a list, the second is the item and the
        // first is the position. Over a dict the first is the key, which
        // is the item of the list of keys.
        let c_name = match (second, &values) {
            (Some(second), None) => self.declare(second, read_kind(&item)),
            _ => self.declare(variable, read_kind(&item)),
        };
        self.line(format!("for (int64_t {position} = 0; {position} < {items}->len; {position}++) {{"));
        self.indent += 1;
        let value = from_slot(&item, &item_at(&items, &position, &item), span)?;
        self.line(format!("{c_type_of_item}{c_name} = {value};"));
        match (second, &values) {
            (Some(second), Some((list, kind))) => {
                let c_type = c_type(kind, span)?;
                let c_name = self.declare(second, kind.clone());
                let value = from_slot(kind, &item_at(list, &position, kind), span)?;
                self.line(format!("{c_type}{c_name} = {value};"));
            }
            (Some(_), None) => {
                let c_name = self.declare(variable, Kind::Int);
                self.line(format!("int64_t {c_name} = {position};"));
            }
            _ => {}
        }
        self.indent -= 1;
        let mut drops = Vec::new();
        if !own {
            drops.push(match copied {
                true => self.drops.code_for_copy(&item, &items),
                false => self.drops.code(&Kind::List(Box::new(item)), &items),
            });
        }
        if let Some((list, value)) = &values {
            drops.push(match values_owned {
                true => self.drops.code(&Kind::List(Box::new(value.clone())), list),
                false => self.drops.code_for_copy(value, list),
            });
        }
        Ok((!drops.is_empty()).then(|| drops.join("\n")))
    }

    /// `for ch in text`, walking the text itself:
    ///
    ///     const char *t1 = z_text;
    ///     const char *t3 = NULL;
    ///     for (const char *t2 = t1; *t2 != '\0'; t2 += zeno_char_size(t2)) {
    ///         const char *z_ch = zeno_char_here(t2, &t3);
    ///
    /// `t3` holds the last character that was not ASCII, with no
    /// collector, and is freed when the loop is over or left.
    fn open_text_loop(&mut self, variable: &str, text: Val, span: Span) -> Result<Option<String>, Error> {
        let text = self.temp(text, span)?;
        let at = self.new_temp();
        let held = self.new_temp();
        self.line(format!("const char *{held} = NULL;"));
        self.scopes.push(HashMap::new());
        let drop = (!self.collector).then(|| {
            self.hold(Kind::Str, &held);
            self.drops.code(&Kind::Str, &held)
        });
        let c_name = self.declare(variable, Kind::Str);
        self.line(format!("for (const char *{at} = {}; *{at} != '\\0'; {at} += zeno_char_size({at})) {{", text.code));
        self.indent += 1;
        self.line(format!("{}{c_name} = zeno_char_here({at}, &{held});", c_type(&Kind::Str, span)?));
        self.indent -= 1;
        Ok(drop)
    }

    // ---------------------------------------------------------- Expressions

    /// Compile several expressions that Zeno works out left to right, and
    /// make sure C does too.
    ///
    /// C leaves the order of `f(a(), b())` and of `a() + b()` up to the
    /// compiler. That only shows when the order matters: when one piece
    /// calls a function (which may print, or change a variable) and the
    /// other reads a variable or does anything else that shows, or when two
    /// pieces can both fail (which error is reported?). In those cases the
    /// earlier piece is worked out into a temporary first. In the common case, such as `a + b * 2`, nothing is
    /// disturbed and the C looks like the Zeno.
    fn sequence(&mut self, exprs: &[&Expr]) -> Result<Vec<Val>, Error> {
        self.sequence_after(Vec::new(), exprs)
    }

    /// `sequence`, continuing from values that are already compiled.
    fn sequence_after(&mut self, mut done: Vec<Val>, exprs: &[&Expr]) -> Result<Vec<Val>, Error> {
        for expr in exprs {
            let mark = self.lines.len();
            let next = self.expr(expr)?;
            self.settle(&mut done, &next, mark, expr.span)?;
            done.push(next);
        }
        Ok(done)
    }

    /// `next` has just been compiled, after the values in `done`. Move into
    /// a temporary every earlier value that `next` could disturb, or that
    /// could disturb `next`. `mark` is how many statements there were
    /// before `next` wrote out its own: the temporaries go there.
    fn settle(&mut self, done: &mut [Val], next: &Val, mark: usize, span: Span) -> Result<(), Error> {
        let fails = next.fails || next.pre_fails;
        let calls = next.calls || next.pre_calls;
        let mut position = mark;
        for earlier in done {
            let disturbed = (calls && (earlier.reads || earlier.fails || earlier.calls))
                || (fails && (earlier.fails || earlier.calls))
                || (next.reads && earlier.calls);
            if disturbed {
                self.temps += 1;
                let name = format!("t{}", self.temps);
                let c_type = c_type(&earlier.kind, span)?;
                self.lines.insert(position, (self.indent, format!("{c_type}{name} = {};", earlier.code)));
                position += 1;
                earlier.pre_fails |= earlier.fails;
                earlier.pre_calls |= earlier.calls;
                earlier.code = name;
                earlier.reads = false;
                earlier.fails = false;
                earlier.calls = false;
            }
        }
        Ok(())
    }

    fn expr(&mut self, expr: &Expr) -> Result<Val, Error> {
        let val = self.value(expr)?;
        let val = self.belongs(val, expr)?;
        // For a build with no collector: text that was not already there
        // has just been built, and the statement will have to give it back.
        // Of several values, the first comes back as one value does, and
        // the others by copy (decision 57).
        let text = match &val.kind {
            Kind::Several(kinds) => is_text(&kinds[0]),
            Kind::Optional(inner) => **inner == Kind::Str,
            kind => *kind == Kind::Str,
        };
        if text && !val.fixed && !self.collector {
            self.built = true;
        }
        Ok(val)
    }

    fn value(&mut self, expr: &Expr) -> Result<Val, Error> {
        let span = expr.span;
        match &expr.kind {
            // A resource given away (decision 59.1): as an owner gives
            // its value away, below.
            ExprKind::Give(name) => match self.lookup(name) {
                Some(variable) => {
                    let (c_name, kind) = (variable.c_name.clone(), variable.kind.clone());
                    let taken = self.new_temp();
                    self.line(format!("{}{taken} = {c_name};", c_type(&kind, span)?));
                    self.line(format!("{c_name} = NULL;"));
                    Ok(Val::plain(taken, kind))
                }
                None => Err(cannot(format!("`{name}` used this way"), span)),
            },
            // Read by `return` and `receive` themselves (decision 57).
            ExprKind::Several(_) => Err(cannot("several values written anywhere but after `return` or `=`", span)),
            ExprKind::Int(n) => match i64::try_from(*n) {
                Ok(n) if n <= i32::MAX as i64 => Ok(Val::plain(n.to_string(), Kind::Int)),
                Ok(n) => Ok(Val::plain(format!("INT64_C({n})"), Kind::Int)),
                Err(_) => Err(cannot("a whole number this big", span)),
            },
            ExprKind::Float(x) => Ok(Val::plain(format!("{x:?}"), Kind::Float)),
            ExprKind::Str(text) => Ok(Val::plain(c_string(text), Kind::Str).fixed()),
            // A block of C is part of this file of C already (see
            // `c_libraries`), and nothing while the program runs: its
            // functions are called by their names (decision 61.5).
            ExprKind::Inline { language, .. } => match self.types.c_libraries.iter().find(|library| library.span == span) {
                Some(library) if language == "c" => Ok(Val::plain("0", Kind::CLibrary(library.number))),
                _ => Err(Error::new(
                    format!(
                        "`zeno build` cannot take an `inline {language}` block: a built program is C, and has no {} beside it. \
                         `zeno run` takes it",
                        crate::inline::LANGUAGES.iter().find(|(word, _)| word == language).map_or("other language", |(_, name)| name)
                    ),
                    span,
                )),
            },
            ExprKind::Bool(b) => Ok(Val::plain(b.to_string(), Kind::Bool)),
            // What `none` is in C depends on where it is going, so it is
            // settled there. See `coerce`.
            ExprKind::None => Ok(Val::plain("NULL", Kind::None)),

            // `Nothing` of a generic enum: the case of the copy the
            // checker found for this place (decision 58.1).
            ExprKind::Name(name) if self.lookup(name).is_none() && self.types.copy_called(expr).is_some() => {
                let owner = self.types.copy_called(expr).unwrap_or_default().to_string();
                let index = self.copied_case(&owner, expr, span)?;
                if !self.enums[&owner].cases[index].1.is_empty() {
                    return Err(cannot("an enum case that carries values, used without them", span));
                }
                Ok(Val::plain(format!("(&zeno_case_{}_{})", ident(&owner), ident(name)), Kind::Enum(owner)))
            }
            ExprKind::Name(name) if self.lookup(name).is_none() && self.cases.contains_key(name) => {
                // A bare enum case, such as `Idle`.
                let (owner, index) = self.cases[name].clone();
                if !self.enums[&owner].cases[index].1.is_empty() {
                    return Err(cannot("an enum case that carries values, used without them", span));
                }
                Ok(Val::plain(format!("(&zeno_case_{}_{})", ident(&owner), ident(name)), Kind::Enum(owner)))
            }
            ExprKind::Name(name)
                if self.lookup(name).is_none()
                    && math_constant(name).is_some()
                    && self.imported.get(name).is_some_and(|m| m == "math") =>
            {
                Ok(Val::plain(format!("{:?}", math_constant(name).unwrap_or_default()), Kind::Float))
            }
            // `from board import led`
            ExprKind::Name(name)
                if self.lookup(name).is_none() && name == "led" && self.imported.get(name).is_some_and(|m| m == "board") =>
            {
                Ok(Val::plain("ZENO_LED", Kind::Pin))
            }
            // `from board import pull_up`: the str of its name (45.3).
            ExprKind::Name(name)
                if self.lookup(name).is_none()
                    && matches!(name.as_str(), "pull_up" | "pull_down" | "rising" | "falling" | "either")
                    && self.imported.get(name).is_some_and(|m| m == "board") =>
            {
                Ok(Val::plain(c_string(name), Kind::Str).fixed())
            }
            ExprKind::Name(name) => match self.lookup(name) {
                // The owner gives its value away here. Take the pointer
                // out and leave NULL behind, so that the end of the owner's
                // block finds nothing to free.
                Some(variable) if self.types.moves(expr) => {
                    let (c_name, kind) = (variable.c_name.clone(), variable.kind.clone());
                    let taken = self.new_temp();
                    self.line(format!("{}{taken} = {c_name};", c_type(&kind, span)?));
                    self.line(format!("{c_name} = NULL;"));
                    // Nothing else on this line can be looking at the
                    // variable (the checker refuses a line that uses an
                    // owner and gives it away), and no other function can
                    // reach it. So no order has to be kept: the temporary
                    // is as plain as a literal.
                    Ok(Val::plain(taken, kind))
                }
                Some(Variable { inner: Some((decl, _)), .. }) if !self.collector => Err(inner_as_value(&decl.name, span)),
                Some(variable) => {
                    let mut val = Val::plain(variable.c_name.clone(), variable.kind.clone());
                    // Only a top-level variable, or one shared with a
                    // function written inside this one, can be changed by a
                    // function that is called. Any other is out of every
                    // other function's reach, so nothing can disturb it.
                    let top_level = self.lookup_depth(name) == Some(0);
                    let reachable = top_level || variable.boxed.is_some();
                    let changeable = reachable && variable.mutable;
                    val.reads = reachable;
                    // Whatever a variable holds was there before this
                    // statement began. If a function could give the
                    // variable new text, what it holds now is lent.
                    let val = self.present(val, expr, changeable)?;
                    Ok(if changeable { self.lend(val) } else { val.fixed() })
                }
                None if self.functions.contains_key(name) => self.function_value(name, span),
                None => Err(cannot(format!("`{name}` used this way"), span)),
            },

            // A quantity is a C double. Its unit has done its work in the
            // checker and costs nothing in the compiled program.
            ExprKind::Unit { value, unit } => {
                let val = self.expr(value)?;
                Ok(Val::from(val.double(), Kind::Quantity(unit.clone()), &[&val]))
            }

            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let val = self.condition(operand)?;
                Ok(Val::from(format!("(!{})", val.code), Kind::Bool, &[&val]))
            }
            // Every bit turned over. C's `~` on an `int64_t` is the same.
            ExprKind::Unary { op: UnaryOp::BitNot, operand } => {
                let val = self.expr(operand)?;
                if !matches!(val.kind, Kind::Int | Kind::U64) {
                    return Err(cannot("`~` on this", span));
                }
                Ok(Val::from(format!("(~{})", val.code), val.kind.clone(), &[&val]))
            }
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                let val = self.expr(operand)?;
                match val.kind {
                    // `-5` written out is just the number minus five.
                    Kind::Int if matches!(operand.kind, ExprKind::Int(_)) => {
                        Ok(Val::plain(format!("-{}", val.code), Kind::Int))
                    }
                    Kind::Int if self.fast => Ok(Val::from(format!("(-({}))", val.code), Kind::Int, &[&val])),
                    Kind::Int if self.release => {
                        Ok(Val::from(format!("((int64_t)(0 - (uint64_t)({})))", val.code), Kind::Int, &[&val]))
                    }
                    Kind::Int => {
                        Ok(Val::from(format!("zeno_neg({}, {})", val.code, self.at(span)), Kind::Int, &[&val]).failing_if(!self.minimal))
                    }
                    Kind::Float | Kind::F32 | Kind::Quantity(_) => {
                        Ok(Val::from(format!("(-{})", val.code), val.kind.clone(), &[&val]))
                    }
                    _ => Err(cannot("`-` on this", span)),
                }
            }

            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                self.logic(*op, left, right, span)
            }
            // `x is none`, `x is not none`
            ExprKind::Binary { op: op @ (BinaryOp::Is | BinaryOp::IsNot), left, .. } => {
                let val = self.expr(left)?;
                let is = *op == BinaryOp::Is;
                let code = match &val.kind {
                    Kind::Optional(inner) if is_pointer(inner) => {
                        format!("({} {} NULL)", val.code, if is { "==" } else { "!=" })
                    }
                    Kind::Optional(_) if is => format!("(!{}.present)", val.code),
                    Kind::Optional(_) => format!("({}.present)", val.code),
                    // Anything else is either always `none` or never. It
                    // may still have work to do, such as a call.
                    other => {
                        let always_none = *other == Kind::None;
                        if (val.fails || val.calls) && !val.code.is_empty() {
                            self.line(format!("(void){};", val.code));
                        }
                        let mut answer = Val::plain((always_none == is).to_string(), Kind::Bool);
                        answer.pre_fails = val.pre_fails || val.fails;
                        answer.pre_calls = val.pre_calls || val.calls;
                        return Ok(answer);
                    }
                };
                Ok(Val::from(code, Kind::Bool, &[&val]))
            }
            ExprKind::Binary { op, left, right } => {
                let mut vals = self.sequence(&[left, right])?;
                let right = vals.pop().expect("two values");
                let left = vals.pop().expect("two values");
                if matches!(op, BinaryOp::In | BinaryOp::NotIn) {
                    return self.contains(*op, left, right, span);
                }
                if matches!(op, BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq)
                {
                    let code = self.compare(*op, &left, &right, span)?;
                    let mut val = Val::from(code, Kind::Bool, &[&left, &right]);
                    // Comparing lists or objects looks inside them.
                    val.reads |= !matches!(left.kind, Kind::Int | Kind::Float | Kind::Bool | Kind::Str | Kind::Quantity(_));
                    return Ok(val);
                }
                self.arithmetic(*op, left, right, span)
            }
            ExprKind::Compare { first, rest } => self.chain(first, rest, span),

            ExprKind::FormatStr(parts) => self.format_string(parts, span),
            // A call of a generic goes to one of its copies, and the
            // checker noted which: `first([1, 2])` calls `first[int]`.
            ExprKind::Call { callee, args } if self.types.copy_called(expr).is_some() => {
                let copy = self.types.copy_called(expr).unwrap_or_default().to_string();
                match self.functions.contains_key(&copy) {
                    true => self.call_function(&copy, args, span),
                    false if self.classes.contains_key(&copy) => self.construct(&copy, args, span),
                    // `Just(3)`: the case of the copy `Maybe[int]`.
                    false if self.copied_enums.contains(&copy) => {
                        let index = self.copied_case(&copy, callee, span)?;
                        self.construct_case(&copy, index, args, span)
                    }
                    false => Err(cannot(format!("`{copy}`, a generic that is not at the top of the file,"), span)),
                }
            }
            ExprKind::Call { callee, args } => {
                // `map` that makes a list of bytes is told so (36.2).
                self.makes_bytes = self.types.makes_bytes(expr);
                self.call(callee, args, span)
            }

            // `counter.bump` as a value: the function the checker wrote
            // for it (decision 56.9).
            ExprKind::Field { object, name, .. } if self.types.method_values.contains_key(&(expr as *const Expr as usize)) => {
                let written = self.types.method_values[&(expr as *const Expr as usize)].clone();
                self.method_value(object, name, &written)
            }
            ExprKind::Field { object, name, .. } => self.field(object, name, span),
            ExprKind::Take(place) => self.take_out(place),
            ExprKind::Index { object, index } => {
                let mut vals = self.sequence(&[object, index])?;
                let index = vals.pop().expect("two values");
                let list = vals.pop().expect("two values");
                if list.kind == Kind::Str {
                    let code = format!("zeno_char_at({}, {}, {})", list.code, index.code, self.at(span));
                    return Ok(Val::from(code, Kind::Str, &[&list, &index]).failing_if(!self.minimal));
                }
                // Through a pointer it is C's own `[ ]`, with no check. A
                // sized item is made wide again, like a field.
                if let Kind::Pointer(pointee) = &list.kind {
                    let code = match pointee {
                        Pointee::Sized(_) => format!("((int64_t){}[{}])", list.code, index.code),
                        _ => format!("({}[{}])", list.code, index.code),
                    };
                    let mut val = Val::from(code, pointee.kind(), &[&list, &index]);
                    val.reads = true;
                    return Ok(val);
                }
                // `ages["Ada"]`: the runtime finds the key, or fails
                // with a KeyError.
                if let Kind::Dict(key, value) = &list.kind {
                    let slot = format!(
                        "zeno_dict_at({}, {}, {})",
                        list.code,
                        to_slot(key, &index.code, span)?,
                        self.at(span)
                    );
                    let mut val = Val::from(from_slot(value, &slot, span)?, (**value).clone(), &[&list, &index]).failing_if(!self.minimal);
                    val.reads = true;
                    return Ok(self.lend(val));
                }
                let Kind::List(item) = &list.kind else {
                    return Err(cannot("`[ ]` on anything but a list, a dict or a str", span));
                };
                let slot = match self.minimal {
                    true => self.slot_at(&list.code, &index.code, item, span),
                    false => format!("({})", self.slot_at(&list.code, &index.code, item, span)),
                };
                let mut val =
                    Val::from(from_slot(item, &slot, span)?, read_kind(item), &[&list, &index]).failing_if(!self.minimal);
                val.reads = true;
                Ok(self.lend(val))
            }
            ExprKind::List(items) => {
                // The items, left to right, as `sequence` would work
                // them out. One that has an owner of its own has left
                // that owner and not yet reached the list. In a firmware
                // build a later item may fail, and the failure leaves
                // this function at once: hold the item until the list
                // is made, so that such a failure frees it.
                let mut vals: Vec<Val> = Vec::new();
                let mut held = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    let mark = self.lines.len();
                    let mut next = self.expr(item)?;
                    self.settle(&mut vals, &next, mark, item.span)?;
                    let later_work = items[index + 1..].iter().any(|item| !is_plain(item));
                    if self.drops.owns(&next.kind) && !next.passing && later_work && self.firmware {
                        if !self.types.moves(item) {
                            next = self.temp(next, item.span)?;
                        }
                        held.push(self.hold(next.kind.clone(), &next.code));
                    }
                    vals.push(next);
                }
                for key in &held {
                    self.let_go(key);
                }
                for (val, item) in vals.iter_mut().zip(items) {
                    self.stays(val, item.span)?;
                    if !self.collector && is_text(&val.kind) && !is_written(item) {
                        // Every item has been worked out by now, so each
                        // is copied from what the statement holds.
                        val.code = format!("zeno_text_keep({})", val.code);
                    }
                }
                let Some(first) = vals.first() else {
                    // `[]` alone does not say what it will hold. The place
                    // it arrives at does (`coerce`), and a `let` with no
                    // type beside it says it cannot tell.
                    let mut val = Val::plain("zeno_list_new(0)", Kind::List(Box::new(Kind::Other("nothing yet".to_string()))));
                    val.made = true;
                    return Ok(val);
                };
                // `[1, none]` is a list of `int or none`: find the one kind
                // that every item can be stored as. Where a list of an
                // interface is wanted, the checker has said so: the
                // items are of different classes.
                let mut item = Some(first.kind.clone());
                for val in &vals {
                    item = item.and_then(|item| joined(&item, &val.kind));
                }
                if let Some(wanted) = self.types.items(expr) {
                    item = Some(disown(wanted.clone()).0);
                }
                if self.types.makes_bytes(expr) {
                    item = Some(Kind::Byte);
                }
                let Some(item) = item else {
                    return Err(cannot("a list whose items are not all one type", span));
                };
                let mut slots = vec![vals.len().to_string()];
                for val in &vals {
                    let val = self.coerce(val.clone(), &item, span)?;
                    slots.push(self.kept_slot(&item, &val.code, span)?);
                }
                let parts: Vec<&Val> = vals.iter().collect();
                let made = list_fn(&item, "of");
                let mut val = Val::from(format!("{made}({})", slots.join(", ")), Kind::List(Box::new(item)), &parts);
                val.made = true;
                Ok(val)
            }
            // `["Ada": 36, "Alan": 41]`: one call that is handed each key
            // and then its value, left to right.
            ExprKind::Dict(entries) => {
                let exprs: Vec<&Expr> = entries.iter().flat_map(|(key, value)| [key, value]).collect();
                let mut vals = self.sequence(&exprs)?;
                if vals.is_empty() {
                    // `[:]` alone does not say what it will hold. The
                    // place it arrives at does (`coerce`).
                    let nothing_yet = || Box::new(Kind::Other("nothing yet".to_string()));
                    let mut val = Val::plain("zeno_dict_new(0, 0)", Kind::Dict(nothing_yet(), nothing_yet()));
                    val.made = true;
                    return Ok(val);
                }
                // The values are all of one kind. Where the place says
                // more than they do (a `dict[str, Shape]`), the checker
                // has said what that kind is.
                let key = vals[0].kind.clone();
                let mut value = Some(vals[1].kind.clone());
                for pair in vals.chunks(2) {
                    value = value.and_then(|value| joined(&value, &pair[1].kind));
                }
                if let Some(wanted) = self.types.items(expr) {
                    value = Some(disown(wanted.clone()).0);
                }
                let Some(value) = value else {
                    return Err(cannot("a dict whose values are not all one type", span));
                };
                self.fits_a_slot(&value, span)?;
                let (key_letter, value_letter) = self.dict_letters(&key, &value, span)?;
                let mut slots = Vec::new();
                for (pair, (_, written)) in vals.chunks_mut(2).zip(entries) {
                    if pair[0].kind != key {
                        return Err(cannot("a dict whose keys are not all one type", span));
                    }
                    pair[1] = self.coerce(pair[1].clone(), &value, span)?;
                    if !self.collector && is_text(&value) && !is_written(written) {
                        // The dict keeps a copy of its own of each value
                        // that is text, as a list does of each item.
                        let from = lent(&pair[1].code).unwrap_or(&pair[1].code).to_string();
                        pair[1].code = format!("zeno_text_keep({from})");
                    }
                    slots.push(to_slot(&key, &pair[0].code, span)?);
                    slots.push(to_slot(&value, &pair[1].code, span)?);
                }
                let parts: Vec<&Val> = vals.iter().collect();
                let code = format!("zeno_dict_of('{key_letter}', '{value_letter}', {}, {})", entries.len(), slots.join(", "));
                let mut val = Val::from(code, Kind::Dict(Box::new(key), Box::new(value)), &parts);
                val.made = true;
                Ok(val)
            }
            // `[n * n for n in items if n > 0]` is a loop that fills a list.
            ExprKind::ListComp { element, variable, iterable, condition } => {
                let list = self.new_temp();
                // A list of `u8`s is a list of bytes (36.2).
                let bytes = self.types.makes_bytes(expr);
                let new = if bytes { "zeno_bytes_new" } else { "zeno_list_new" };
                self.line(format!("zeno_list *{list} = {new}(0);"));
                // Until it is full, the list is nobody's. If working out
                // an item fails, the failure frees what there is of it.
                let filling = self.hold(Kind::List(Box::new(Kind::None)), &list);
                let filled_from = self.lines.len();
                let walked = self.open_loop(variable, None, iterable)?;
                self.indent += 1;
                // Text built for one item is given back before the next.
                let begin = self.text_begin();
                if let Some(condition) = condition {
                    let condition = self.condition(condition)?;
                    self.line(format!("if ({}) {{", bare(&condition.code)));
                    self.indent += 1;
                }
                // So is what working out one item makes and gives to nobody.
                let own = self.own_begin();
                let item = self.expr(element)?;
                self.stays(&item, element.span)?;
                let item = self.keep(item, is_written(element));
                let item = if bytes { self.coerce(item, &Kind::Byte, span)? } else { item };
                let slot = self.kept_slot(&item.kind, &item.code, span)?;
                self.line(format!("{}({list}, {slot});", list_fn(&item.kind, "append")));
                self.own_end(own, true);
                if self.keeps_copies(&item.kind) || self.drops.owns(&item.kind) {
                    // The list has copies of its items, or is the owner
                    // of them, which was not known when the lines above
                    // were written: a failure half way has to free those
                    // as well.
                    let plain = format!("zeno_list_drop({list});");
                    let whole = self.drops.code(&Kind::List(Box::new(item.kind.clone())), &list);
                    for (_, line) in &mut self.lines[filled_from..] {
                        *line = line.replace(&plain, &whole);
                    }
                }
                if condition.is_some() {
                    self.indent -= 1;
                    self.line("}");
                }
                if let Some(release) = self.text_end(begin) {
                    self.line(release);
                }
                self.indent -= 1;
                self.line("}");
                self.scopes.pop();
                self.close_loop(walked);
                self.let_go(&filling);
                let mut val = Val::plain(list, Kind::List(Box::new(item.kind)));
                val.pre_fails = true;
                val.pre_calls = true;
                val.made = true;
                Ok(val)
            }
            ExprKind::Function(decl) => self.closure(decl, span),
        }
    }

    // ------------------------------------------------- Values that may be none

    /// Make `val` fit a place that holds values of kind `to`.
    ///
    /// Nearly always there is nothing to do. The work is at the border of
    /// `T or none`: a plain number going into an `int or none` has a flag
    /// put beside it, and `none` becomes whatever "nothing" is for that
    /// kind.
    fn coerce(&mut self, val: Val, to: &Kind, span: Span) -> Result<Val, Error> {
        if val.kind == *to {
            return Ok(val);
        }
        // An empty list becomes a list of whatever this place holds.
        let mut val = val;
        if is_empty_list(&val.kind) {
            match to {
                Kind::List(_) => return Ok(Val { kind: to.clone(), ..val }),
                Kind::Optional(inner) if matches!(**inner, Kind::List(_)) => val.kind = (**inner).clone(),
                _ => {}
            }
        }
        // `[:]` becomes a dict of whatever this place holds, and is told so.
        if is_empty_dict(&val.kind) {
            let wanted = match to {
                Kind::Optional(inner) => &**inner,
                to => to,
            };
            if let Kind::Dict(key, value) = wanted {
                let (key, value) = self.dict_letters(key, value, span)?;
                val.code = format!("zeno_dict_kinds({}, '{key}', '{value}')", val.code);
                val.kind = wanted.clone();
                if val.kind == *to {
                    return Ok(val);
                }
            }
        }
        // A Circle arriving where a Shape is wanted: the same pointer,
        // which C has to be told is one to a `z_Shape` now.
        let seen_as = match (some(&val.kind), some(to)) {
            (Kind::Class(from), Kind::Class(interface)) if from != interface && self.is_a(from, interface) => {
                Some(interface.clone())
            }
            _ => None,
        };
        if let Some(interface) = seen_as {
            return Ok(Val { code: format!("((z_{} *){})", ident(&interface), val.code), kind: to.clone(), ..val });
        }
        let code = match (&val.kind, to) {
            (Kind::None, Kind::Optional(inner)) => nothing(inner, span)?,
            (from, Kind::Optional(inner)) if from == &**inner => match from {
                from if is_pointer(from) => val.code.clone(),
                Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt => format!("(zeno_opt_int){{true, {}}}", val.code),
                Kind::Float | Kind::F32 | Kind::Quantity(_) => format!("(zeno_opt_float){{true, {}}}", val.code),
                Kind::Bool => format!("(zeno_opt_bool){{true, {}}}", val.code),
                _ => return Err(cannot(optional_named(inner), span)),
            },
            // In a firmware build the checker has made a function that
            // raises nothing as one that can, where a raising type is
            // wanted (decision 56.5). One that was not would be called
            // with the wrong arguments.
            (Kind::Function(from), Kind::Function(wanted)) if self.raising(from) != self.raising(wanted) => {
                return Err(cannot("a function value that raises where one that does not is wanted, or the other way round", span));
            }
            (Kind::Function(_), Kind::Function(_)) => match joined(&val.kind, to) {
                Some(_) => val.code.clone(),
                None => {
                    return Err(cannot("a function used where a function of a different shape is wanted", span))
                }
            },
            // A number going into a list of bytes: the checker has seen
            // that it fits.
            (Kind::Int, Kind::Byte) => val.code.clone(),
            // A list of bytes is a list of `u8` that the checker knows
            // about. One made where it did not say so is a mistake here.
            (Kind::List(from), Kind::List(item)) if (**from == Kind::Byte) != (**item == Kind::Byte) => {
                return Err(cannot("a list of bytes where another list is wanted, or the other way round", span));
            }
            // Anything else the checker has already agreed to as it is.
            _ => return Ok(val),
        };
        Ok(Val { code, kind: to.clone(), ..val })
    }

    /// A variable has just been read. If the checker has ruled `none` out
    /// at this spot (it is inside `if x is not none { ... }`, say), give
    /// the value inside instead of the `T or none` around it.
    ///
    /// `changeable` is for a variable that other functions can reach and
    /// change. One of them may have set it back to `none` since the check,
    /// so it is looked at once more before it is trusted.
    fn present(&mut self, val: Val, expr: &Expr, changeable: bool) -> Result<Val, Error> {
        let Kind::Optional(inner) = &val.kind else {
            return Ok(val);
        };
        if matches!(self.types.narrowed(expr), None | Some(Kind::None)) {
            return Ok(val);
        }
        let inner = (**inner).clone();
        // Looking again is a check, and a minimal build has none.
        let changeable = changeable && !self.minimal;
        let again = self.at(expr.span);
        let code = match (&inner, changeable) {
            (inner, false) if is_pointer(inner) => val.code.clone(),
            (_, false) => format!("{}.value", val.code),
            (Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt, true) => format!("zeno_still_int({}, {again})", val.code),
            (Kind::Float | Kind::F32 | Kind::Quantity(_), true) => format!("zeno_still_float({}, {again})", val.code),
            (Kind::Bool, true) => format!("zeno_still_bool({}, {again})", val.code),
            (inner, true) => {
                format!("(({})zeno_still({}, {again}))", c_type(inner, expr.span)?.trim_end(), val.code)
            }
        };
        Ok(Val { code, kind: inner, fails: val.fails || changeable, ..val })
    }

    /// The name of a C function that shows a `T or none` as text. One is
    /// written for each kind, the first time it is needed.
    fn optional_text(&mut self, inner: &Kind, quoted: bool, span: Span) -> Result<String, Error> {
        let quoted = quoted && *inner == Kind::Str;
        let kind = Kind::Optional(Box::new(inner.clone()));
        let name = format!("zeno_{}_{}", if quoted { "repr" } else { "text" }, key(&kind));
        if self.adapters.insert(name.clone()) {
            let (present, value) = if is_pointer(inner) { ("x != NULL", "x") } else { ("x.present", "x.value") };
            let value = Val::plain(value, inner.clone());
            let shown = if quoted { self.repr(&value, span)? } else { self.text(&value, span)? };
            self.helper(
                format!("static const char *{name}({}x)", c_type(&kind, span)?),
                format!("    return {present} ? {shown} : \"none\";\n"),
            );
        }
        Ok(name)
    }

    /// The same, for comparing two of them: equal when both are `none`, or
    /// when neither is and the values inside are equal.
    fn optional_same(&mut self, inner: &Kind, span: Span) -> Result<String, Error> {
        let kind = Kind::Optional(Box::new(inner.clone()));
        let name = format!("zeno_same_{}", key(&kind));
        if self.adapters.insert(name.clone()) {
            let (a, b, a_value, b_value) = if is_pointer(inner) {
                ("a != NULL", "b != NULL", "a", "b")
            } else {
                ("a.present", "b.present", "a.value", "b.value")
            };
            let same = self.equal(inner, a_value, b_value, span)?;
            let c_type = c_type(&kind, span)?;
            self.helper(
                format!("static bool {name}({c_type}a, {c_type}b)"),
                format!("    if (!({a}) || !({b})) return ({a}) == ({b});\n    return {same};\n"),
            );
        }
        Ok(name)
    }

    // ------------------------------------------------------ Functions as values

    /// A function of the program used as a value: `apply(double, 3)`.
    ///
    /// Every function value is called the same way, with itself as a hidden
    /// first argument. An ordinary function does not take that argument, so
    /// it gets a small wrapper that does, and one fixed value that points
    /// at the wrapper.
    ///
    /// In a firmware build the wrapper of a function that can raise gives
    /// back `true` or `false`, as the function does. A function that
    /// raises nothing, put where a function type says `raises`, gets a
    /// second wrapper of that kind, which always gives back `true`
    /// (decision 56.5).
    fn function_value(&mut self, name: &str, span: Span) -> Result<Val, Error> {
        let entry = &self.functions[name];
        let (target, mut kind) = (entry.c_name.clone(), entry.kind.clone());
        kind.raises = match self.firmware {
            true => entry.decl.raises.clone(),
            false => Vec::new(),
        };
        // A raising function that raises nothing.
        let made = match (kind.raises.is_empty(), self.types.made_raising.get(&spot(self.within, span))) {
            (true, Some(said)) if self.firmware => {
                kind.raises = vec![said.clone()];
                true
            }
            _ => false,
        };
        // A function of an imported file has a dot in its name,
        // `shapes.area`, and a name in C cannot. Its name in C can
        // stand in, since no two functions have the same.
        let shown = name;
        // So can the name in C of one written inside a function (decision
        // 56.16), which two functions may each have one of.
        let name = if name.contains('.') || target.starts_with("zeno_inner_") { target.as_str() } else { name };
        // The second wrapper has a prefix of its own, which no name of
        // the program can make.
        let (value, wrap) = match made {
            true => (format!("zeno_raising_value_{name}"), format!("zeno_raising_wrap_{name}")),
            false => (format!("zeno_value_{name}"), format!("zeno_wrap_{name}")),
        };
        if self.adapters.insert(value.clone()) {
            let mut params = vec!["zeno_closure *zeno_self".to_string()];
            let mut args = Vec::new();
            for (index, param) in kind.params.iter().enumerate() {
                params.push(format!("{}a{index}", c_type(param, span)?));
                args.push(format!("a{index}"));
            }
            params.extend(out_params(&kind.returns, span)?);
            args.extend((0..later_values(&kind.returns).len()).map(gives));
            let raising = self.raising(&kind);
            if raising && kind.returns != Kind::None {
                params.push(format!("{}*zeno_result", c_type(&kind.returns, span)?));
                if !made {
                    args.push("zeno_result".to_string());
                }
            }
            let call = format!("{target}({})", args.join(", "));
            let (returns, call) = match (raising, made, &kind.returns) {
                (false, _, Kind::None) => (return_type(&kind.returns, span)?, call),
                (false, _, _) => (return_type(&kind.returns, span)?, format!("return {call}")),
                (true, false, _) => ("bool ".to_string(), format!("return {call}")),
                (true, true, Kind::None) => ("bool ".to_string(), format!("{call};\n    return true")),
                (true, true, _) => ("bool ".to_string(), format!("*zeno_result = {call};\n    return true")),
            };
            self.helper(
                format!("static {returns}{wrap}({})", params.join(", ")),
                format!("    (void)zeno_self;\n    {call};\n"),
            );
            self.definitions.push(format!("static zeno_closure {value} = {{(zeno_code){wrap}, {}}};\n", c_string(shown)));
        }
        Ok(Val::plain(format!("(&{value})"), Kind::Function(Box::new(kind))))
    }

    /// A function written where a value goes: `function(n) { return n * 2 }`.
    ///
    /// It becomes a C function of its own. If it uses variables from the
    /// function around it, they are packed into a struct made on the heap
    /// at this spot, and the C function receives that struct. That pair, a
    /// function and the variables it took with it, is called a closure.
    ///
    /// ```c
    /// struct zeno_env_1 { zeno_closure base; int64_t z_k; };
    /// static int64_t zeno_fn_1(zeno_closure *zeno_self, int64_t z_n) {
    ///     struct zeno_env_1 *env = (struct zeno_env_1 *)zeno_self;
    ///     return zeno_mul(z_n, env->z_k, 3, 40);
    /// }
    /// ```
    fn closure(&mut self, decl: &Arc<Function>, span: Span) -> Result<Val, Error> {
        self.closure_named(decl, &decl.name, span)
    }

    /// A function written where a value goes, under the name `print`
    /// shows: none, or the method's for one used as a value.
    fn closure_named(&mut self, decl: &Arc<Function>, shown: &str, span: Span) -> Result<Val, Error> {
        self.closure_made(decl, shown, None, span)
    }

    /// The same, for a function with a name written inside a function
    /// (`own`, decision 56.16): inside its body its name is the function
    /// itself, so that it can call itself, and the checker's notes for
    /// its body are under its own declaration.
    fn closure_made(&mut self, decl: &Arc<Function>, shown: &str, own: Option<&Arc<Function>>, span: Span) -> Result<Val, Error> {
        let Some(kind) = self.types.function(decl).cloned() else {
            return Err(cannot("this function", span));
        };
        let (mut kind, owned) = disown_all(kind);
        // In a firmware build one that can raise gives back `true` or
        // `false`, as a function of the program that can raise does. So
        // does one that raises nothing, put where a function type says
        // `raises` (decision 56.5).
        kind.raises = match (self.firmware, self.types.made_raising.get(&spot(self.within, span))) {
            (false, _) => Vec::new(),
            (true, Some(said)) if decl.raises.is_empty() => vec![said.clone()],
            (true, _) => decl.raises.clone(),
        };
        let fallible = self.raising(&kind);
        // Where its answer goes, if it can raise and gives one back.
        let outs = out_params(&kind.returns, span)?;
        let answer = match (fallible, &kind.returns) {
            (true, returns) if *returns != Kind::None => Some(format!("{}*zeno_result", c_type(returns, span)?)),
            _ => None,
        };
        self.closures += 1;
        let id = self.closures;

        // Set aside the function being compiled, and start a new one. Every
        // variable in sight of this spot is offered to the new function as
        // `env->name`; the ones it looks up are the ones it needs.
        let mut outside = std::mem::take(&mut self.scopes);
        let globals = outside.remove(0);
        let mut offered: HashMap<String, Variable> = HashMap::new();
        for scope in &outside {
            for (name, variable) in scope {
                let (c_name, boxed) = match variable.boxed {
                    Some(_) => (format!("(*env->z_{name})"), Some(format!("env->z_{name}"))),
                    None => (format!("env->z_{name}"), None),
                };
                let variable = Variable {
                    c_name,
                    kind: variable.kind.clone(),
                    mutable: variable.mutable,
                    boxed,
                    used: Cell::new(false),
                    // The function around this one goes on owning it.
                    owner: false,
                    looks: false,
                    // A function written where a value goes may outlive
                    // the block that owns a function with a name it would
                    // take, unless there is a collector.
                    inner: variable.inner.clone().map(|(decl, usable)| (decl, usable && (self.collector || own.is_some()))),
                };
                offered.insert(name.clone(), variable);
            }
        }
        let mut own_scope = HashMap::new();
        if let Some(own) = own {
            let itself = Variable {
                c_name: "zeno_self".to_string(),
                kind: Kind::Function(Box::new(kind.clone())),
                mutable: false,
                boxed: None,
                used: Cell::new(false),
                owner: false,
                looks: false,
                inner: Some((own.clone(), true)),
            };
            own_scope.insert(decl.name.clone(), itself);
        }
        self.scopes = vec![globals, offered, own_scope];
        let base = std::mem::replace(&mut self.base, 2);
        let lines = std::mem::take(&mut self.lines);
        let indent = std::mem::replace(&mut self.indent, 1);
        let used = self.used.clone();
        let within = match own {
            Some(own) => std::mem::replace(&mut self.within, Arc::as_ptr(own) as usize),
            None => self.within,
        };

        let result = match self.parameters(decl, &kind, &owned, span) {
            Ok(mut params) => {
                params.extend(outs);
                params.extend(answer);
                self.body(decl, &kind, fallible, span).map(|()| params)
            }
            Err(error) => Err(error),
        };
        self.within = within;
        self.base = base;

        // Put everything back as it was.
        let body = render(&std::mem::replace(&mut self.lines, lines));
        self.indent = indent;
        self.used = used;
        let mut inside = std::mem::take(&mut self.scopes);
        outside.insert(0, inside.remove(0));
        let offered = inside.remove(0);
        self.scopes = outside;
        let params = result?;

        let mut taken: Vec<(&String, &Variable)> = offered.iter().filter(|(_, variable)| variable.used.get()).collect();
        taken.sort_by(|a, b| a.0.cmp(b.0));
        let mut all_params = vec!["zeno_closure *zeno_self".to_string()];
        all_params.extend(params);
        let returns = if fallible { "bool ".to_string() } else { return_type(&kind.returns, span)? };
        let first_line = format!("static {returns}zeno_fn_{id}({})", all_params.join(", "));
        let name = c_string(shown);
        let kind = Kind::Function(Box::new(kind));

        // The value itself is made here, on the heap, each time this spot
        // is reached: two function values are equal only if they are the
        // very same one, as in `zeno run`.
        //
        // With no collector the block has one owner, which frees it
        // (docs/functions-and-errors.md). A number it uses is copied in.
        // Text is copied in too, as a copy of its own that goes when the
        // block goes. A list or an object is looked at where it is: the
        // checker has made sure such a function is gone before they are.
        // Unless the function is kept: then it takes them (decision
        // 56.8), the variable out here holds nothing from this line on,
        // and the block frees them when it goes.
        let takes = self.types.taken.get(&spot(self.within, span)).cloned().unwrap_or_default();
        let made = self.new_temp();
        let mut fields = String::new();
        let mut fill = Vec::new();
        let mut frees = Vec::new();
        for (zeno_name, variable) in &taken {
            let field_type = c_type(&variable.kind, span)?;
            let star = if variable.boxed.is_some() { "*" } else { "" };
            fields += &format!("    {field_type}{star}z_{zeno_name};\n");
            // What the variable is called out here, where the function is
            // being made. A boxed one hands over its box, not its value.
            let Some(source) = self.lookup(zeno_name) else {
                return Err(cannot(format!("a function that uses `{zeno_name}` from outside it"), span));
            };
            // A value that is taken is moved out of its variable, which a
            // box shared with another function cannot allow. The checker
            // takes only what a `let` or a parameter holds (56.8).
            if takes.contains(zeno_name) && source.boxed.is_some() {
                return Err(cannot(format!("a function that takes `{zeno_name}`, which is shared, from outside it"), span));
            }
            let source = source.boxed.clone().unwrap_or_else(|| source.c_name.clone());
            if takes.contains(zeno_name) {
                fill.push(format!("{made}->z_{zeno_name} = {source};"));
                fill.push(format!("{source} = NULL;"));
                if !self.collector {
                    frees.push(self.drops.code(&variable.kind, &format!("env->z_{zeno_name}")));
                }
            } else if !self.collector && is_text(&variable.kind) {
                fill.push(format!("{made}->z_{zeno_name} = zeno_text_keep({source});"));
                frees.push(format!("zeno_text_drop(env->z_{zeno_name});"));
            } else {
                fill.push(format!("{made}->z_{zeno_name} = {source};"));
            }
        }
        if taken.is_empty() {
            // It uses nothing from outside: the plain header is all of it.
            self.helper(first_line, format!("    (void)zeno_self;\n{body}"));
            self.line(format!("zeno_closure *{made} = (zeno_closure *)zeno_alloc(sizeof(zeno_closure));"));
            self.line(format!("{made}->code = (zeno_code)zeno_fn_{id};"));
            self.line(format!("{made}->name = {name};"));
            if !self.collector {
                self.line(format!("{made}->drop = zeno_closure_release;"));
            }
            return Ok(Val::plain(made, kind));
        }
        let env = format!("struct zeno_env_{id}");
        self.structs.push(format!("{env} {{\n    zeno_closure base;\n{fields}}};"));
        self.helper(first_line, format!("    {env} *env = ({env} *)zeno_self;\n{body}"));
        self.line(format!("{env} *{made} = ({env} *)zeno_alloc(sizeof({env}));"));
        self.line(format!("{made}->base.code = (zeno_code)zeno_fn_{id};"));
        self.line(format!("{made}->base.name = {name};"));
        if !self.collector && frees.is_empty() {
            self.line(format!("{made}->base.drop = zeno_closure_release;"));
        } else if !self.collector {
            // The text it took copies of goes with it, and so does what
            // it took.
            let mut body = format!("    {env} *env = ({env} *)zeno_self;\n");
            for free in &frees {
                body += &format!("    {free}\n");
            }
            body += "    zeno_free(env);\n";
            self.helper(format!("static void zeno_fn_drop_{id}(zeno_closure *zeno_self)"), body);
            self.line(format!("{made}->base.drop = zeno_fn_drop_{id};"));
        }
        for line in fill {
            self.line(line);
        }
        Ok(Val::plain(format!("(&{made}->base)"), kind))
    }

    /// `take slot.task`, `take tasks[i]`: what the place holds, which is
    /// left holding `none` (decision 56.10). The value is read into a
    /// variable of its own, and `none` written where it was, so nothing is
    /// freed: whoever receives the value owns it, and if nobody does, the
    /// statement does (`belongs`). Text that a build with no collector
    /// kept there leaves its keeper as `pop()` makes it leave a list.
    fn take_out(&mut self, place: &Expr) -> Result<Val, Error> {
        let span = place.span;
        match &place.kind {
            ExprKind::Field { object: reached, name, .. } => {
                let object = self.expr(reached)?;
                let object = if is_c_place(&object.code) { object } else { self.temp(object, span)? };
                let field = match &object.kind {
                    Kind::Class(class) if !self.classes[class].sized.contains_key(name) => {
                        self.classes[class].fields.iter().find(|(field, _, _)| field == name)
                    }
                    _ => None,
                };
                let Some((_, kind @ Kind::Optional(inner), _)) = field else {
                    return Err(cannot("`take` in front of this", span));
                };
                let (kind, empty) = (kind.clone(), nothing(inner, span)?);
                let at = format!("{}->z_{name}", object.code);
                let code = match !self.collector && is_text(&kind) {
                    true => format!("zeno_text_take({at})"),
                    false => at.clone(),
                };
                let mut val = Val::from(code, kind, &[&object]);
                val.calls = true;
                let val = self.temp(val, span)?;
                self.line(format!("{at} = {empty};"));
                Ok(val)
            }
            ExprKind::Index { object, index } => {
                let mut vals = self.sequence(&[object, index])?;
                let index = vals.pop().expect("two values");
                let list = vals.pop().expect("two values");
                let Kind::List(item) = &list.kind else {
                    return Err(cannot("`take` in front of this", span));
                };
                let item = (**item).clone();
                let Kind::Optional(inner) = &item else {
                    return Err(cannot("`take` in front of this", span));
                };
                let empty = to_slot(&item, &nothing(inner, span)?, span)?;
                let at = self.new_temp();
                self.line(format!("zeno_slot *{at} = {};", self.slot_address(&list.code, &index.code, span)));
                let code = self.taken(&item, &format!("(*{at})"), span)?;
                let mut val = Val::from(code, read_kind(&item), &[&list, &index]);
                val.calls = true;
                let val = self.temp(val, span)?;
                self.line(format!("*{at} = {empty};"));
                Ok(val)
            }
            // The parser lets nothing else through.
            _ => Err(cannot("`take` in front of this", span)),
        }
    }

    /// `counter.bump` used as a value is the function `function(n: int)
    /// -> int { return counter.bump(n) }`, which the checker wrote out
    /// (decision 56.9). The method is bound to the object `counter`
    /// holds now, as in `zeno run`: a `var` given another object after
    /// this line, or a variable at the top of the file, which the
    /// function would otherwise read where it is, does not change it. So
    /// the function is offered a copy of the variable made here, unless
    /// it takes the object, which moves it out of the variable itself.
    fn method_value(&mut self, object: &Expr, method: &str, written: &Arc<Expr>) -> Result<Val, Error> {
        let (ExprKind::Function(decl), ExprKind::Name(name)) = (&written.kind, &object.kind) else {
            return Err(cannot("this method used as a value", written.span));
        };
        let takes = self.types.taken.get(&spot(self.within, written.span)).is_some_and(|taken| taken.contains(name));
        if takes {
            return self.closure_named(decl, method, written.span);
        }
        let now = self.expr(object)?;
        let now = self.temp(now, object.span)?;
        let copy = Variable {
            c_name: now.code,
            kind: now.kind,
            mutable: false,
            boxed: None,
            used: Cell::new(false),
            owner: false,
            looks: false,
            inner: None,
        };
        self.scopes.push(HashMap::from([(name.clone(), copy)]));
        let made = self.closure_named(decl, method, written.span);
        self.scopes.pop();
        made
    }

    /// A call to a function held in a variable, or to anything else that
    /// is a function value: `f(3)`, `make_adder(2)(3)`, `handlers[0]()`.
    fn call_value(&mut self, function: Val, args: &[Arg], span: Span) -> Result<Val, Error> {
        let Kind::Function(kind) = function.kind.clone() else {
            return Err(cannot("a call to something that is not a function", span));
        };
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(cannot("named arguments to a function held in a variable", span));
        }
        if args.len() != kind.params.len() {
            return Err(cannot("a call with this many arguments", span));
        }
        // The C mentions the function twice, so give it a name first.
        let function = if is_c_place(&function.code) { function } else { self.temp(function, span)? };
        let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        let vals = self.sequence_after(vec![function], &exprs)?;
        let mut codes = Vec::new();
        // What may be given to each parameter, and what it does with
        // it, is the checker's to say: the function's type tells it.
        for (val, param) in vals[1..].iter().zip(&kind.params) {
            codes.push(self.coerce(val.clone(), param, span)?.code);
        }
        codes.extend(self.outs_for(&kind.returns, span)?);
        // In a firmware build a function that can raise gives back
        // `true` or `false`, and is tested as a call of one by its name.
        if self.raising(&kind) {
            let call = invoke(&vals[0].code, &kind, true, &codes, span)?;
            return self.checked_call(call, kind.returns.clone(), &kind.raises, span);
        }
        let call = invoke(&vals[0].code, &kind, false, &codes, span)?;
        Ok(self.called(call, &vals, kind.returns.clone()))
    }

    /// `items.map(f)`, `items.filter(f)`, `items.reduce(f, start)`: each is
    /// a loop that calls `f` on every item. `vals` is the list, the
    /// function, and for `reduce` the starting value.
    ///
    /// They walk the list as it was when they began, as `zeno run` does, so
    /// `f` may change the list without confusing the loop.
    fn over_list(&mut self, name: &str, mut vals: Vec<Val>, item: &Kind, bytes: bool, span: Span) -> Result<Val, Error> {
        let takes = if name == "reduce" { 2 } else { 1 };
        let kind = match vals.get(1).map(|function| &function.kind) {
            Some(Kind::Function(kind)) if kind.params.len() == takes && vals.len() == takes + 1 => (**kind).clone(),
            _ => return Err(cannot(format!("the list method `{name}` called this way"), span)),
        };
        if !is_c_place(&vals[1].code) {
            vals[1] = self.temp(vals[1].clone(), span)?;
        }
        let function = vals[1].code.clone();
        // `sort` reads the list twice: to walk it and to put it in order.
        if name == "sort" && !vals[0].passing && !is_c_place(&vals[0].code) {
            vals[0] = self.temp(vals[0].clone(), span)?;
        }

        // `reduce` starts from its second argument.
        let total = match vals.get(2) {
            Some(start) => {
                let start = self.coerce(start.clone(), &kind.returns, span)?;
                let total = self.new_temp();
                self.line(format!("{}{total} = {};", c_type(&kind.returns, span)?, start.code));
                total
            }
            None => String::new(),
        };
        let items = self.new_temp();
        let position = self.new_temp();
        // With no collector a list of text is walked as a `for` loop
        // walks one: a copy with copies of the items, which the function
        // that is called cannot pull away.
        let texts = !self.collector && is_text(item);
        let copy = if texts { "zeno_list_copy_text".to_string() } else { list_fn(item, "copy") };
        // And a list of numbers that may be `none` with boxes of its own
        // (56.14).
        let boxes = !self.collector && is_boxed(item);
        // A list that the statement owns is out of every function's
        // reach, so it is walked as it is.
        let own = vals[0].passing;
        match own {
            true => self.line(format!("zeno_list *{items} = {};", vals[0].code)),
            false if boxes => {
                let copy = self.keep_items(item, &format!("zeno_list_copy({})", vals[0].code), span)?;
                self.line(format!("zeno_list *{items} = {copy};"))
            }
            false => self.line(format!("zeno_list *{items} = {copy}({});", vals[0].code)),
        }
        let slot = item_at(&items, &position, item);
        let each = Val::plain(from_slot(item, &slot, span)?, read_kind(item));
        // In a firmware build with no collector a function that can
        // raise may fail on any item. Until the loop is over, the copy
        // that is walked and the list being made are held, so that a
        // failure frees them on its way out.
        let holding = self.raising(&kind) && !self.collector;
        let mut held = Vec::new();
        if holding && !own {
            held.push(self.hold_copy(item.clone(), &items));
        }

        // What `sort` does when every key is known.
        let mut after = Vec::new();
        let (before, inside, mut result) = match name {
            // `people.sort(age_of)`: a list of keys is made as `map`
            // makes a list, the runtime puts the items in the order of
            // their keys, and the keys are done with.
            "sort" => {
                let by = match &kind.returns {
                    Kind::Int => 0,
                    Kind::Float | Kind::F32 => 1,
                    Kind::Str => 2,
                    Kind::U64 => 3,
                    _ => return Err(cannot("`sort` with a function that returns anything but a number or text", span)),
                };
                let out = self.new_temp();
                let arg = self.coerce(each, &kind.params[0], span)?.code;
                if holding {
                    held.push(self.hold(Kind::List(Box::new(kind.returns.clone())), &out));
                }
                let (mut inside, call) = self.through(&function, &kind, vec![arg], span)?;
                let mut before = vec![format!("zeno_list *{out} = zeno_list_new({items}->len);")];
                let sorted = if own { items.clone() } else { vals[0].code.clone() };
                after.push(format!("{}({sorted}, {out}, {by}, {});", list_fn(item, "sort_by"), self.at(span)));
                if !self.collector && is_text(&kind.returns) {
                    self.marks += 1;
                    let mark = format!("m{}", self.marks);
                    before.push(format!("size_t {mark} = zeno_text_top;"));
                    let kept = format!("zeno_text_keep({call})");
                    inside.push(format!("zeno_list_append({out}, {});", to_slot(&kind.returns, &kept, span)?));
                    inside.push(format!("zeno_text_top = {mark};"));
                    after.push(format!("zeno_list_drop_text({out});"));
                } else {
                    inside.push(format!("zeno_list_append({out}, {});", to_slot(&kind.returns, &call, span)?));
                    after.push(format!("zeno_list_drop({out});"));
                }
                (before, inside, Val::plain(String::new(), Kind::None))
            }
            "map" => {
                let out = self.new_temp();
                let arg = self.coerce(each, &kind.params[0], span)?.code;
                // A function that gives a `u8` makes a list of bytes.
                let made = if bytes { Kind::Byte } else { kind.returns.clone() };
                if holding {
                    held.push(self.hold(Kind::List(Box::new(made.clone())), &out));
                }
                let (mut inside, call) = self.through(&function, &kind, vec![arg], span)?;
                let new = list_fn(&made, "new");
                let append = list_fn(&made, "append");
                let mut before = vec![format!("zeno_list *{out} = {new}({items}->len);")];
                if !self.collector && is_text(&kind.returns) {
                    // With no collector the text each call gives back is
                    // in the text room. The new list takes a copy of its
                    // own, and the room is given back before the next.
                    self.marks += 1;
                    let mark = format!("m{}", self.marks);
                    before.push(format!("size_t {mark} = zeno_text_top;"));
                    let kept = format!("zeno_text_keep({call})");
                    inside.push(format!("zeno_list_append({out}, {});", to_slot(&kind.returns, &kept, span)?));
                    inside.push(format!("zeno_text_top = {mark};"));
                } else {
                    inside.push(format!("{append}({out}, {});", self.kept_slot(&made, &call, span)?));
                }
                let mut result = Val::plain(out, Kind::List(Box::new(made)));
                result.made = true;
                (before, inside, result)
            }
            "filter" => {
                if kind.returns != Kind::Bool {
                    return Err(cannot("`filter` with a function that does not return a bool", span));
                }
                let out = self.new_temp();
                let arg = self.coerce(each.clone(), &kind.params[0], span)?.code;
                if holding {
                    held.push(self.hold(Kind::List(Box::new(item.clone())), &out));
                }
                let (mut inside, call) = self.through(&function, &kind, vec![arg], span)?;
                // With no collector the new list has its own copy of each
                // text it takes.
                let taken = match texts {
                    true => to_slot(item, &format!("zeno_text_keep({})", each.code), span)?,
                    false if boxes => self.kept_slot(item, &each.code, span)?,
                    false => slot.clone(),
                };
                let mut result = Val::plain(out.clone(), Kind::List(Box::new(item.clone())));
                result.made = true;
                let (new, append) = (list_fn(item, "new"), list_fn(item, "append"));
                inside.push(format!("if ({call}) {append}({out}, {taken});"));
                (vec![format!("zeno_list *{out} = {new}(0);")], inside, result)
            }
            _ => {
                let so_far = self.coerce(Val::plain(total.clone(), kind.returns.clone()), &kind.params[0], span)?.code;
                let arg = self.coerce(each, &kind.params[1], span)?.code;
                let (mut inside, call) = self.through(&function, &kind, vec![so_far, arg], span)?;
                inside.push(format!("{total} = {call};"));
                (Vec::new(), inside, Val::plain(total, kind.returns.clone()))
            }
        };
        for line in before {
            self.line(line);
        }
        self.line(format!("for (int64_t {position} = 0; {position} < {items}->len; {position}++) {{"));
        self.indent += 1;
        for line in inside {
            self.line(line);
        }
        self.indent -= 1;
        self.line("}");
        for key in &held {
            self.let_go(key);
        }
        for line in after {
            self.line(line);
        }
        if !own {
            // The copy that was walked was nobody else's.
            self.line(self.drops.code_for_copy(item, &items));
        }
        result.pre_fails = true;
        result.pre_calls = true;
        Ok(result)
    }

    /// `+ - * / // % **`, on values that are already in order.
    fn arithmetic(&mut self, op: BinaryOp, left: Val, right: Val, span: Span) -> Result<Val, Error> {
        use BinaryOp::*;
        use Kind::{Float, Int, Quantity, Str};
        let parts = [&left, &right];
        let (l, r) = (&left.code, &right.code);
        let (dl, dr) = (left.double(), right.double());
        let symbol = op.text();
        let numbers = left.is_number() && right.is_number();

        let val = match (op, &left.kind, &right.kind) {
            // `p + 4`: C counts in items too.
            (Add | Sub, Kind::Pointer(_), Int) => Val::from(format!("({l} {symbol} {r})"), left.kind.clone(), &parts),
            // Whole numbers go through the runtime, which checks them.
            // `--release`: the processor's own arithmetic, which wraps
            // round. It is done on unsigned numbers because in C only
            // those are allowed to wrap; the bits come out the same.
            //
            // `--fast`: C's signed arithmetic as it stands. In C a signed
            // number that overflows has no defined result, so the C
            // compiler takes it that none does, as it does for a program
            // written in C: a count that only goes up is never below
            // zero, and the `%` of it needs no mending (decision 23.6).
            (Add | Sub | Mul, Int, Int) if self.fast => Val::from(format!("({l} {symbol} {r})"), Int, &parts),
            (Add | Sub | Mul, Int, Int) if self.release => {
                let sign = match op { Add => "+", Sub => "-", _ => "*" };
                Val::from(format!("((int64_t)((uint64_t)({l}) {sign} (uint64_t)({r})))"), Int, &parts)
            }
            (Add, Int, Int) => Val::from(format!("zeno_add({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),
            (Sub, Int, Int) => Val::from(format!("zeno_sub({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),
            (Mul, Int, Int) => Val::from(format!("zeno_mul({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),
            // By 2, 4, 8 and so on, written as a number: rounding down is a
            // shift of the bits and what is left over is the low bits, for
            // numbers below zero too. Nothing can fail (decision 23.5).
            (FloorDiv, Int, Int) if power_of_two(r).is_some() => {
                Val::from(format!("zeno_halved({l}, {})", power_of_two(r).unwrap_or(0)), Int, &parts)
            }
            (Mod, Int, Int) if power_of_two(r).is_some() => {
                let low = (1u64 << power_of_two(r).unwrap_or(0)) - 1;
                Val::from(format!("({l} & INT64_C({low}))"), Int, &parts)
            }
            (FloorDiv, Int, Int) => {
                Val::from(format!("zeno_floordiv({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal)
            }
            (Mod, Int, Int) => Val::from(format!("zeno_mod({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),
            (Pow, Int, Int) => Val::from(format!("zeno_pow({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),

            // The bits of whole numbers. C's own operators do these three.
            // The two shifts go through the runtime, which checks how far.
            (BitAnd | BitOr | BitXor, Int, Int) => Val::from(format!("({l} {symbol} {r})"), Int, &parts),
            (Shl, Int, Int) => Val::from(format!("zeno_shl({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),
            (Shr, Int, Int) => Val::from(format!("zeno_shr({l}, {r}, {})", self.at(span)), Int, &parts).failing_if(!self.minimal),

            (Add, Str, Str) => Val::from(format!("zeno_join(2, {l}, {r})"), Str, &parts),
            // `bytes + [300]`: a list of bytes and a list of ints make a
            // list of ints (36.2).
            (Add, Kind::List(a), Kind::List(b))
                if matches!((&**a, &**b), (Kind::Byte, Kind::Int) | (Kind::Int, Kind::Byte)) =>
            {
                let (a_bytes, b_bytes) = (**a == Kind::Byte, **b == Kind::Byte);
                let code = format!("zeno_list_concat_mixed({l}, {a_bytes}, {r}, {b_bytes})");
                let mut val = Val::from(code, Kind::List(Box::new(Int)), &parts);
                val.made = true;
                val
            }
            // `[0] * 64`: a new list with the items so many times over.
            (Mul, Kind::List(a), Int) => {
                let repeat = if **a == Kind::Byte { "zeno_bytes_repeat" } else { "zeno_list_repeat" };
                let repeated = format!("{repeat}({l}, {r}, {})", self.at(span));
                let code = match self.keeps_copies(a) {
                    true => self.keep_items(a, &repeated, span)?,
                    false => repeated,
                };
                let mut val = Val::from(code, left.kind.clone(), &parts).failing_if(!self.minimal);
                val.made = true;
                val
            }
            (Add, Kind::List(a), Kind::List(b)) if a == b => {
                let joined = format!("{}({l}, {r})", list_fn(a, "concat"));
                // With no collector a list of text has its own copy of
                // each item, and so has the list that `+` makes.
                let code = match self.keeps_copies(a) {
                    true => self.keep_items(a, &joined, span)?,
                    false => joined,
                };
                let mut val = Val::from(code, left.kind.clone(), &parts);
                val.made = true;
                val
            }

            // Quantities. The checker has made sure the two measure the
            // same thing; if the units differ (`1 s + 500 ms`), both sides
            // are scaled to the finer one.
            (Add | Sub, Quantity(a), Quantity(b)) => {
                let (unit, l, r) = in_common(&left, a, &right, b, span)?;
                Val::from(format!("({l} {symbol} {r})"), Quantity(unit), &parts)
            }
            (Mul, Quantity(unit), Int | Float) | (Mul, Int | Float, Quantity(unit)) => {
                Val::from(format!("({dl} * {dr})"), Quantity(unit.clone()), &parts)
            }
            // `3.3 V * 20 mA` and `1 / (2 ms)` (decision 43): each side in
            // the unit of its kind with no prefix, as the interpreter does.
            (Mul, Quantity(a), Quantity(b)) => {
                let (kind, l, r) = made("*", Some((&left.code, a)), (&right.code, b), span)?;
                Val::from(format!("({l} * {r})"), kind, &parts)
            }
            (Div, Quantity(a), Quantity(b)) if units::common(a, b).is_none() => {
                let (kind, l, r) = made("/", Some((&left.code, a)), (&right.code, b), span)?;
                Val::from(self.divided(&l, &r, span), kind, &parts).failing_if(!self.minimal)
            }
            (Div, Int | Float, Quantity(b)) => {
                let (kind, _, r) = made("/", None, (&right.code, b), span)?;
                Val::from(self.divided(&dl, &r, span), kind, &parts).failing_if(!self.minimal)
            }
            (Div, Quantity(a), Quantity(b)) => {
                let (_, l, r) = in_common(&left, a, &right, b, span)?;
                Val::from(self.divided(&l, &r, span), Float, &parts).failing_if(!self.minimal)
            }
            (Div, Quantity(unit), Int | Float) => {
                Val::from(self.divided(&dl, &dr, span), Quantity(unit.clone()), &parts).failing_if(!self.minimal)
            }

            // Two u64s (decision 42). The checker has made sure both sides
            // are u64s: a number written beside one has become one. C's
            // own unsigned arithmetic wraps round, which is what `u64.wrap`
            // asks for and what `--release`, `--fast` and `--minimal` do;
            // a checked build goes through the runtime, which says
            // OverflowError instead.
            (Add | Sub | Mul, Kind::U64, Kind::U64) if self.fast || self.release || self.minimal => {
                Val::from(format!("({l} {symbol} {r})"), Kind::U64, &parts)
            }
            (Add | Sub | Mul | FloorDiv | Mod | Pow, Kind::U64, Kind::U64) => {
                let name = match op {
                    Add => "add",
                    Sub => "sub",
                    Mul => "mul",
                    FloorDiv => "floordiv",
                    Mod => "mod",
                    _ => "pow",
                };
                Val::from(format!("zeno_{name}_u64({l}, {r}, {})", self.at(span)), Kind::U64, &parts)
                    .failing_if(!self.minimal)
            }
            (Div, Kind::U64, Kind::U64) => {
                Val::from(self.divided(&format!("(double){l}"), &format!("(double){r}"), span), Float, &parts)
                    .failing_if(!self.minimal)
            }
            (BitAnd | BitOr | BitXor, Kind::U64, Kind::U64) => Val::from(format!("({l} {symbol} {r})"), Kind::U64, &parts),
            (Shl | Shr, Kind::U64, Int | Kind::U64) => {
                let name = if op == Shl { "shl" } else { "shr" };
                let is_int = right.kind == Int;
                let code = format!("zeno_{name}_u64({l}, (uint64_t)({r}), {is_int}, {})", self.at(span));
                Val::from(code, Kind::U64, &parts).failing_if(!self.minimal)
            }

            // An f32 with an f32 or an int (decision 41.2). `+`, `-`, `*`
            // and `/` are C's own on `float`; the cast holds the answer to
            // 32 bits on a processor that would keep more. `//`, `%` and
            // `**` are done in double and rounded once, as every engine does.
            (_, Kind::F32, Kind::F32 | Int) | (_, Int, Kind::F32) => {
                let (sl, sr) = (left.single(), right.single());
                let (dl, dr) = (format!("((double){sl})"), format!("((double){sr})"));
                let code = match op {
                    Add | Sub | Mul => format!("((float)({sl} {symbol} {sr}))"),
                    Div if self.minimal => format!("((float)({sl} / {sr}))"),
                    Div => format!("zeno_div_f32({sl}, {sr}, {})", self.at(span)),
                    FloorDiv => format!("((float)zeno_floordiv_float({dl}, {dr}, {}))", self.at(span)),
                    Mod => format!("((float)zeno_mod_float({dl}, {dr}, {}))", self.at(span)),
                    Pow => format!("((float)pow({dl}, {dr}))"),
                    _ => return Err(cannot(format!("`{symbol}` between these two types"), span)),
                };
                let fails = matches!(op, Div | FloorDiv | Mod) && !self.minimal;
                Val::from(code, Kind::F32, &parts).failing_if(fails)
            }

            // What is left is floats, or an int with a float. C's own float
            // arithmetic already does what Zeno wants, except for dividing
            // by zero.
            (Add | Sub | Mul, _, _) if numbers => Val::from(format!("({dl} {symbol} {dr})"), Float, &parts),
            (Div, _, _) if numbers => {
                Val::from(self.divided(&dl, &dr, span), Float, &parts).failing_if(!self.minimal)
            }
            (FloorDiv, _, _) if numbers => {
                Val::from(format!("zeno_floordiv_float({dl}, {dr}, {})", self.at(span)), Float, &parts).failing_if(!self.minimal)
            }
            (Mod, _, _) if numbers => {
                Val::from(format!("zeno_mod_float({dl}, {dr}, {})", self.at(span)), Float, &parts).failing_if(!self.minimal)
            }
            (Pow, _, _) if numbers => Val::from(format!("pow({dl}, {dr})"), Float, &parts),

            _ => return Err(cannot(format!("`{symbol}` between these two types"), span)),
        };
        Ok(val)
    }

    /// `a and b`, `a or b`. The right side must not run unless it is needed.
    ///
    /// Each side is a condition: once it has its answer, the text it
    /// built and the values it alone owned are given back.
    fn logic(&mut self, op: BinaryOp, left: &Expr, right: &Expr, span: Span) -> Result<Val, Error> {
        let left = self.condition(left)?;
        let mark = self.lines.len();
        let right = self.condition(right)?;
        let before = self.lines.split_off(mark);
        let symbol = if op == BinaryOp::And { "&&" } else { "||" };

        if before.is_empty() {
            // C's `&&` and `||` have the same rule, so this is direct.
            return Ok(Val::from(format!("({} {symbol} {})", left.code, right.code), Kind::Bool, &[&left, &right]));
        }
        // The right side needed statements of its own. They go inside an
        // `if`, so that they only run when the left side did not settle it.
        let result = self.temp(left, span)?;
        let guard = if op == BinaryOp::And { result.code.clone() } else { format!("!{}", result.code) };
        self.line(format!("if ({guard}) {{"));
        self.lines.extend(before.into_iter().map(|(indent, line)| (indent + 1, line)));
        self.indent += 1;
        self.line(format!("{} = {};", result.code, right.code));
        self.indent -= 1;
        self.line("}");
        let mut val = Val::plain(result.code, Kind::Bool);
        val.pre_fails = true;
        val.pre_calls = true;
        Ok(val)
    }

    /// `a < b < c`: each piece is worked out once, and only if the
    /// comparisons before it held.
    fn chain(&mut self, first: &Expr, rest: &[(BinaryOp, Expr)], span: Span) -> Result<Val, Error> {
        // When every piece is a plain name or literal, nothing can go wrong
        // by mentioning the middle one twice: `(a < b && b < c)`.
        if is_plain(first) && rest.iter().all(|(_, expr)| is_plain(expr)) {
            let mut exprs = vec![first];
            exprs.extend(rest.iter().map(|(_, expr)| expr));
            let vals = self.sequence(&exprs)?;
            let mut tests = Vec::new();
            for (index, (op, expr)) in rest.iter().enumerate() {
                tests.push(self.compare(*op, &vals[index], &vals[index + 1], expr.span)?);
            }
            let parts: Vec<&Val> = vals.iter().collect();
            return Ok(Val::from(format!("({})", tests.join(" && ")), Kind::Bool, &parts));
        }

        // Otherwise: one temporary per piece, and an `if` per comparison.
        let result = self.new_temp();
        self.line(format!("bool {result} = false;"));
        let first = self.expr(first)?;
        let mut previous = self.temp(first, span)?;
        let mut opened = 0;
        for (index, (op, expr)) in rest.iter().enumerate() {
            let next = self.expr(expr)?;
            let next = self.temp(next, expr.span)?;
            let test = self.compare(*op, &previous, &next, expr.span)?;
            if index + 1 == rest.len() {
                self.line(format!("{result} = {test};"));
            } else {
                self.line(format!("if ({}) {{", bare(&test)));
                self.indent += 1;
                opened += 1;
            }
            previous = next;
        }
        for _ in 0..opened {
            self.indent -= 1;
            self.line("}");
        }
        let mut val = Val::plain(result, Kind::Bool);
        val.pre_fails = true;
        val.pre_calls = true;
        Ok(val)
    }

    // ---------------------------------------------------------------- Calls

    fn call(&mut self, callee: &Expr, args: &[Arg], span: Span) -> Result<Val, Error> {
        match &callee.kind {
            ExprKind::Name(name) if self.lookup(name).is_none() => {
                if self.functions.contains_key(name) {
                    return self.call_function(name, args, span);
                }
                if self.classes.contains_key(name) {
                    return self.construct(name, args, span);
                }
                if let Some((owner, index)) = self.cases.get(name).cloned() {
                    return self.construct_case(&owner, index, args, span);
                }
                if self.imported.get(name).is_some_and(|module| module == "math") {
                    return self.math(name, args, span);
                }
                if let Some(module) = self.imported.get(name).filter(|module| *module != "math").cloned() {
                    return self.device(&module, name, args, span);
                }
                if name == "allocate" {
                    return self.allocate(args, span);
                }
                if args.iter().any(|arg| arg.name.is_some()) {
                    return Err(cannot("named arguments to a built-in", span));
                }
                // A build for a board does not hand the line to `printf`
                // (decision 52.4).
                if name == "print" && self.minimal && !self.board_prints {
                    return self.print_direct(args, span);
                }
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence(&exprs)?;
                // `ValueError("bad")` makes an error value. Its message is
                // whatever it is given, as text.
                if ERROR_KINDS.contains(&name.as_str()) {
                    let message = match vals.as_slice() {
                        [] => "\"\"".to_string(),
                        [message] => self.text(message, span)?,
                        _ => return Err(cannot(format!("`{name}` with more than one message"), span)),
                    };
                    let parts: Vec<&Val> = vals.iter().collect();
                    let code = format!("zeno_error_new({}, {message}, NULL)", c_string(name));
                    return Ok(Val::from(code, Kind::Error, &parts));
                }
                self.builtin(name, vals, span)
            }
            ExprKind::Field { object, name, .. } => {
                if let ExprKind::Name(owner) = &object.kind {
                    if self.lookup(owner).is_none() {
                        // `math.sqrt(x)`
                        if owner == "math" {
                            return self.math(name, args, span);
                        }
                        // `c.library("m", "double cbrt(double x);")`: known
                        // while building, and nothing while the program
                        // runs (decision 44.10).
                        if (owner.as_str(), name.as_str()) == ("c", "library") {
                            return match self.types.c_libraries.iter().find(|library| library.span == span) {
                                Some(library) => Ok(Val::plain("0", Kind::CLibrary(library.number))),
                                None => Err(Error::new(
                                    "`zeno build` needs the library given to `c.library`, and its declarations, written out \
                                     in the program as texts, so that it knows them while it builds",
                                    span,
                                )),
                            };
                        }
                        // `time.sleep(1 s)`, `board.pin(7)`, `files.read(path)`
                        if matches!(owner.as_str(), "time" | "board" | "files" | "program") {
                            return self.device(owner, name, args, span);
                        }
                        // `u8.wrap(n)`: the low bits of `n`, which cannot fail.
                        if let (Some(width), "wrap", [arg]) = (self.sized_name(owner), name.as_str(), args) {
                            let n = self.expr(&arg.value)?;
                            if n.kind != Kind::Int || arg.name.is_some() {
                                return Err(cannot(format!("`{owner}.wrap` called this way"), span));
                            }
                            let code = format!("zeno_wrap({}, {}, {})", n.code, width.bits, width.signed);
                            return Ok(Val::from(code, Kind::Int, &[&n]));
                        }
                        // `Shape.Circle(1.0)`
                        let case = self.enums.get(owner).and_then(|entry| entry.cases.iter().position(|(case, _)| case == name));
                        if let Some(index) = case {
                            return self.construct_case(owner, index, args, span);
                        }
                    }
                }
                self.method(object, name, args, span)
            }
            // A function with a name written inside a function, held as a
            // closure (decision 56.16): called with names and defaults as
            // any function with a name is.
            ExprKind::Name(name) if self.lookup(name).is_some_and(|variable| variable.inner.is_some()) => {
                let variable = self.lookup(name).expect("looked up above");
                let (decl, usable) = variable.inner.clone().expect("looked up above");
                if !usable {
                    return Err(inner_outlives(name, span));
                }
                let function = Val::plain(variable.c_name.clone(), variable.kind.clone());
                let args = in_order(&decl, args, span, "`zeno build`")?;
                self.call_value(function, &args, span)
            }
            // Anything else that is called must be a function value.
            _ => {
                let function = self.expr(callee)?;
                self.call_value(function, args, span)
            }
        }
    }

    /// The size called `name`, if `name` is still the built-in one and not
    /// something of the program's own.
    fn sized_name(&self, name: &str) -> Option<Width> {
        let taken = self.functions.contains_key(name) || self.classes.contains_key(name) || self.enums.contains_key(name);
        ints::width(name).filter(|_| !taken)
    }

    /// The type written as the first argument of `allocate` or `register`.
    fn written_type<'b>(what: &str, args: &'b [Arg], span: Span) -> Result<(Pointee, &'b Expr), Error> {
        match args {
            [Arg { name: None, value: Expr { kind: ExprKind::Name(item), .. } }, Arg { name: None, value }] => {
                match Pointee::named(item) {
                    Some(pointee) => Ok((pointee, value)),
                    None => Err(cannot(format!("`{what}` of `{item}`"), span)),
                }
            }
            _ => Err(cannot(format!("`{what}` called this way"), span)),
        }
    }

    /// `allocate(u8, 16)`: sixteen bytes, all zero. The runtime takes
    /// them from the heap, or from the pool where there is no collector.
    fn allocate(&mut self, args: &[Arg], span: Span) -> Result<Val, Error> {
        let (pointee, count) = Self::written_type("allocate", args, span)?;
        let count = self.expr(count)?;
        if count.kind != Kind::Int {
            return Err(cannot("`allocate` of a number of items that is not an int", span));
        }
        let item = pointee.c_type();
        let call = format!("(({item} *)zeno_allocate({}, sizeof({item}), {}))", count.code, self.at(span));
        Ok(self.called(call, &[count], Kind::Pointer(pointee)))
    }

    /// A function of the `time` or the `board` module. Each is one call
    /// into the runtime, which passes it on to the board.
    fn device(&mut self, module: &str, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        // `register(u32, 0x4002_0014)`: a register is its address.
        if (module, name) == ("board", "register") {
            let (pointee, address) = Self::written_type("register", args, span)?;
            let address = self.expr(address)?;
            return match (pointee, &address.kind) {
                (Pointee::Sized(width), Kind::Int) => Ok(Val { kind: Kind::Register(width), ..address }),
                _ => Err(cannot("`register` called this way", span)),
            };
        }
        let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        let vals = self.sequence(&exprs)?;
        match (module, name, vals.as_slice()) {
            // The board counts in seconds, whatever unit the program wrote.
            ("time", "sleep", [time]) => {
                let scale = match &time.kind {
                    Kind::Quantity(unit) => units::convert(unit, "s"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`sleep` of something that is not a time", span));
                };
                let call = format!("zeno_sleep({})", scaled(&time.code, scale));
                Ok(self.called(call, &vals, Kind::None))
            }
            // The clock moves by itself, so two readings are never one
            // value: it counts as a call, and is not worked out twice.
            ("time", "now", []) => Ok(self.called("zeno_now()".to_string(), &vals, Kind::Quantity("ms".to_string()))),
            ("board", "pin", [number]) if number.kind == Kind::Int => {
                Ok(Val::from(format!("zeno_pin({}, {})", number.code, self.at(span)), Kind::Pin, &[number]).failing_if(!self.minimal))
            }
            // `ticker(10 ms)`. The runtime counts in milliseconds, as
            // the board's clock does, whatever unit the program wrote.
            ("board", "ticker", [time]) => {
                let scale = match &time.kind {
                    Kind::Quantity(unit) => units::convert(unit, "ms"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`ticker` with something that is not a time", span));
                };
                self.uses_board_part("ZENO_TICKING");
                let call = format!("zeno_ticker({}, {})", scaled(&time.code, scale), self.at(span));
                Ok(self.called(call, &vals, Kind::Ticker).failing_if(!self.minimal))
            }
            // `pwm_pin(9, 1 kHz)`. The board counts in hertz, whatever
            // unit the program wrote.
            ("board", "pwm_pin", [number, rate]) if number.kind == Kind::Int => {
                let scale = match &rate.kind {
                    Kind::Quantity(unit) => units::convert(unit, "Hz"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`pwm_pin` with something that is not a frequency", span));
                };
                self.uses_board_part("ZENO_PWM_PINS");
                let call = format!("zeno_pwm_pin({}, {}, {})", number.code, scaled(&rate.code, scale), self.at(span));
                Ok(self.called(call, &vals, Kind::PwmPin).failing_if(!self.minimal))
            }
            // `analog_pin(0)`. Making a pin one the converter reads is
            // done to the board, so it is a call.
            ("board", "analog_pin", [number]) if number.kind == Kind::Int => {
                self.uses_board_part("ZENO_ANALOG_PINS");
                let call = format!("zeno_analog_pin({}, {})", number.code, self.at(span));
                Ok(self.called(call, &vals, Kind::AnalogPin).failing_if(!self.minimal))
            }
            // `serial(1, 115200 baud)`. The speed is in baud already, and
            // opening a port is done to the board, so it is a call.
            ("board", "serial", [number, speed]) if number.kind == Kind::Int && speed.kind == Kind::Quantity("baud".to_string()) => {
                self.uses_board_part("ZENO_SERIAL_PORTS");
                let call = format!("zeno_serial({}, {}, {})", number.code, speed.code, self.at(span));
                Ok(self.called(call, &vals, Kind::Serial).failing_if(!self.minimal))
            }
            // `input_pin(2, pull_up)`. Making a pin an input is done to
            // the board, so it is a call, and is not worked out twice.
            ("board", "input_pin", [number, rest @ ..]) if number.kind == Kind::Int && rest.len() < 2 => {
                self.uses_board_part("ZENO_INPUT_PINS");
                let pull = rest.first().map_or("NULL".to_string(), |pull| pull.code.clone());
                let call = format!("zeno_input_pin({}, {pull}, {})", number.code, self.at(span));
                Ok(self.called(call, &vals, Kind::InputPin).failing_if(!self.minimal))
            }
            // The `files` module. Each is one call into the runtime.
            ("files", "exists", [path]) if path.kind == Kind::Str => {
                Ok(self.called(format!("zeno_files_exists({})", path.code), &vals, Kind::Bool))
            }
            // The rest can fail, with a FileError. An ordinary build
            // jumps to the `except`. A firmware build is told whether the
            // call worked, and tests that, as for any function that
            // `raises`:
            //
            //     const char *t1;
            //     if (!zeno_files_read(z_path, 3, 12, &t1)) goto zeno_except_1;
            ("files", "read" | "write" | "append" | "remove", _)
                if vals.iter().all(|val| val.kind == Kind::Str)
                    && vals.len() == if matches!(name, "write" | "append") { 2 } else { 1 } =>
            {
                let returns = if name == "read" { Kind::Str } else { Kind::None };
                let texts: Vec<&str> = vals.iter().map(|val| val.code.as_str()).collect();
                let call = format!("zeno_files_{name}({}, {})", texts.join(", "), self.at(span));
                // The runtime builds the message of the error, and the
                // text of the file, in the text room.
                self.built = !self.collector;
                if self.firmware {
                    return self.checked_call(call, returns, &["FileError".to_string()], span);
                }
                Ok(self.called(call, &vals, returns))
            }
            // `files.read_bytes(path)`: a new list[u8], which is nobody's
            // yet. `files.write_bytes(path, bytes)` lends the list to the
            // runtime for the call (decision 36.1).
            ("files", "read_bytes", [path]) if path.kind == Kind::Str => {
                let call = format!("zeno_files_read_bytes({}, {})", path.code, self.at(span));
                let returns = Kind::List(Box::new(Kind::Byte));
                self.built = !self.collector;
                let mut bytes = if self.firmware {
                    self.checked_call(call, returns, &["FileError".to_string()], span)?
                } else {
                    self.called(call, &vals, returns)
                };
                bytes.made = true;
                Ok(bytes)
            }
            // `files.list(folder)`: a new list of text, which is nobody's
            // yet. `files.make_folder(path)`. Each is in a
            // piece of the runtime (decisions 59.20 and 59.21).
            ("files", "list", [folder]) if folder.kind == Kind::Str => {
                self.uses_piece("ZENO_FILES_LIST");
                let call = format!("zeno_files_list({}, {})", folder.code, self.at(span));
                let returns = Kind::List(Box::new(Kind::Str));
                self.built = !self.collector;
                let mut names = if self.firmware {
                    self.checked_call(call, returns, &["FileError".to_string()], span)?
                } else {
                    self.called(call, &vals, returns)
                };
                names.made = true;
                Ok(names)
            }
            ("files", "make_folder", [path]) if path.kind == Kind::Str => {
                self.uses_piece("ZENO_FILES_MAKE_FOLDER");
                let call = format!("zeno_files_make_folder({}, {})", path.code, self.at(span));
                self.built = !self.collector;
                if self.firmware {
                    return self.checked_call(call, Kind::None, &["FileError".to_string()], span);
                }
                Ok(self.called(call, &vals, Kind::None))
            }
            // What a `files.Reader` and a `files.Writer` are made of, in a
            // piece of the runtime (decision 59.6). The number of the
            // open file is an int, the address of its `FILE`.
            ("files", "handle_open" | "handle_line" | "handle_bytes" | "handle_write" | "handle_write_bytes", _)
                if handle_arguments(name, &vals.iter().map(|val| &val.kind).collect::<Vec<_>>()) =>
            {
                self.uses_piece("ZENO_FILES_PIECES");
                let codes: Vec<&str> = vals.iter().map(|val| val.code.as_str()).collect();
                let call = format!("zeno_files_{name}({}, {})", codes.join(", "), self.at(span));
                let returns = match name {
                    "handle_open" => Kind::Int,
                    "handle_line" => Kind::Optional(Box::new(Kind::Str)),
                    "handle_bytes" => Kind::List(Box::new(Kind::Byte)),
                    _ => Kind::None,
                };
                // The message of an error, and a line, are built in the
                // text room.
                self.built = !self.collector;
                let mut val = if self.firmware {
                    self.checked_call(call, returns, &["FileError".to_string()], span)?
                } else {
                    self.called(call, &vals, returns)
                };
                // The bytes are a new list, which is nobody's yet.
                val.made = name == "handle_bytes";
                Ok(val)
            }
            ("files", "handle_close", [file]) if file.kind == Kind::Int => {
                self.uses_piece("ZENO_FILES_PIECES");
                Ok(self.called(format!("zeno_files_handle_close({})", file.code), &vals, Kind::None))
            }
            ("files", "write_bytes", [path, bytes])
                if path.kind == Kind::Str && bytes.kind == Kind::List(Box::new(Kind::Byte)) =>
            {
                let call = format!("zeno_files_write_bytes({}, {}, {})", path.code, bytes.code, self.at(span));
                self.built = !self.collector;
                if self.firmware {
                    return self.checked_call(call, Kind::None, &["FileError".to_string()], span);
                }
                Ok(self.called(call, &vals, Kind::None))
            }
            // `program.arguments()`: a new list, which is nobody's yet.
            ("program", "arguments", []) => {
                self.arguments = true;
                let mut words =
                    self.called("zeno_program_arguments()".to_string(), &vals, Kind::List(Box::new(Kind::Str)));
                words.made = true;
                Ok(words)
            }
            // `program.exit(code)`: C's own `exit`, after the code is
            // checked, which can raise as `char(n)` can.
            ("program", "exit", [code]) if code.kind == Kind::Int => {
                let call = format!("zeno_program_exit({}, {})", code.code, self.at(span));
                Ok(self.called(call, &vals, Kind::None))
            }
            // `program.environment(name)`: a copy of the text, or NULL.
            ("program", "environment", [name]) if name.kind == Kind::Str => {
                let call = format!("zeno_program_environment({})", name.code);
                self.built = !self.collector;
                Ok(self.called(call, &vals, Kind::Optional(Box::new(Kind::Str))))
            }
            ("program", "warn", [text]) if text.kind == Kind::Str => {
                let call = format!("zeno_program_warn({})", text.code);
                Ok(self.called(call, &vals, Kind::None))
            }
            // `program.run(name, words)` starts another program, which
            // only `zeno run` does so far.
            ("program", "run", _) => Err(cannot("`program.run`", span)),
            _ => Err(cannot(format!("`{module}.{name}` called this way"), span)),
        }
    }

    fn math(&mut self, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        let vals = self.sequence(&exprs)?;
        // An angle that a function works out is in radians, and says so.
        let radians = Kind::Quantity("rad".to_string());
        // `atan2(y, x)` is the one function of two numbers.
        if let ("atan2", [y, x]) = (name, vals.as_slice()) {
            if y.is_number() && x.is_number() {
                return Ok(Val::from(format!("atan2({}, {})", y.double(), x.double()), radians, &[y, x]));
            }
        }
        let [x] = vals.as_slice() else {
            return Err(cannot(format!("`{name}` called this way"), span));
        };
        let d = match &x.kind {
            // `sin(30 deg)`: the C library counts in radians.
            Kind::Quantity(unit) if matches!(name, "sin" | "cos" | "tan") => match units::convert(unit, "rad") {
                Some(scale) => scaled(&x.code, scale),
                None => return Err(cannot(format!("`{name}` of a quantity in {unit}"), span)),
            },
            Kind::Int | Kind::Float => x.double(),
            _ => return Err(cannot(format!("`{name}` called this way"), span)),
        };
        Ok(match name {
            "sqrt" if self.minimal => Val::from(format!("sqrt({d})"), Kind::Float, &[x]),
            "sqrt" => Val::from(format!("zeno_sqrt({d}, {})", self.at(span)), Kind::Float, &[x]).failing_if(!self.minimal),
            "floor" => Val::from(format!("zeno_whole(floor({d}))"), Kind::Int, &[x]),
            "ceil" => Val::from(format!("zeno_whole(ceil({d}))"), Kind::Int, &[x]),
            // C's `round` sends a half away from zero, as Zeno's does.
            "round" => Val::from(format!("zeno_whole(round({d}))"), Kind::Int, &[x]),
            "sin" | "cos" | "tan" | "exp" => Val::from(format!("{name}({d})"), Kind::Float, &[x]),
            "atan" => Val::from(format!("atan({d})"), radians, &[x]),
            // These two refuse a number that is the sine of no angle.
            "asin" | "acos" => Val::from(format!("zeno_{name}({d}, {})", self.at(span)), radians, &[x]).failing_if(!self.minimal),
            // And these a number that has no logarithm.
            "log" | "log2" | "log10" => {
                let base = match name {
                    "log2" => 2,
                    "log10" => 10,
                    _ => 0,
                };
                Val::from(format!("zeno_log({d}, {base}, {})", self.at(span)), Kind::Float, &[x]).failing_if(!self.minimal)
            }
            _ => return Err(cannot(format!("`math.{name}`"), span)),
        })
    }

    /// The C for a call: `c_name(...)` with one argument for each of
    /// `slots`, which are the names and default values of what the function
    /// takes. `receiver` is the object a method is called on, already
    /// compiled; it goes first.
    ///
    /// The arguments are worked out in the order they are written, even
    /// when names put them elsewhere. Returns the call, and the values that
    /// went into it.
    fn call_c(
        &mut self,
        c_name: &str,
        slots: &[(String, Kind, Option<Arc<Expr>>)],
        owned: &[bool],
        lends: &[bool],
        receiver: Option<Val>,
        args: &[Arg],
        span: Span,
    ) -> Result<(String, Vec<Val>), Error> {
        // Where the function was written, for its defaults: taken now, so
        // that a call among the arguments does not see it.
        let callee = self.callee.take();
        let mut places = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            places.push(match &arg.name {
                None => index,
                Some(name) => match slots.iter().position(|(slot, _, _)| slot == name) {
                    Some(place) => place,
                    None => return Err(cannot(format!("the argument `{name}`"), span)),
                },
            });
        }
        let first = receiver.is_some() as usize;
        // The arguments, left to right, as `sequence` would work them out.
        let mut vals: Vec<Val> = receiver.into_iter().collect();
        let mut held = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let mark = self.lines.len();
            let mut next = self.expr(&arg.value)?;
            // A parameter without `take` only looks, so it may be lent what
            // the statement owns. Anything else may keep what it is given.
            let lent = next.passing && lends.get(places[index]).copied().unwrap_or(false);
            if !lent {
                self.stays(&next, arg.value.span)?;
            }
            self.settle(&mut vals, &next, mark, arg.value.span)?;
            // An owned value on its way into the call has left its owner
            // and not yet reached the function. In a firmware build a
            // later argument may fail, and the failure leaves this
            // function at once. Hold the value until the call, so that
            // such a failure frees it.
            let taken = owned.get(places[index]).copied().unwrap_or(false);
            let later_work = args[index + 1..].iter().any(|arg| !is_plain(&arg.value));
            if taken && later_work && self.firmware {
                if !self.types.moves(&arg.value) {
                    next = self.temp(next, arg.value.span)?;
                }
                held.push(self.hold(next.kind.clone(), &next.code));
            }
            vals.push(next);
        }
        // From here the function owns what it was given.
        for key in &held {
            self.let_go(key);
        }

        let mut codes: Vec<Option<String>> = vec![None; slots.len()];
        for (place, val) in places.iter().zip(&vals[first..]) {
            let Some((_, kind, _)) = slots.get(*place) else {
                return Err(cannot("this many arguments", span));
            };
            codes[*place] = Some(self.coerce(val.clone(), kind, span)?.code);
        }
        // A default that is more than a literal is worked out at the call,
        // in the order of the parameters, and may read the ones before it
        // (decision 56.16). So each argument is held in a variable first.
        let worked_out = codes.iter().zip(slots).any(|(code, (_, _, default))| code.is_none() && default.as_ref().is_some_and(|d| !is_literal(d)));
        let mut receiver = (first == 1).then(|| vals[0].clone());
        if worked_out {
            for (code, (_, kind, _)) in codes.iter_mut().zip(slots) {
                if let Some(given) = code.take() {
                    let held = self.new_temp();
                    self.line(format!("{}{held} = {given};", c_type(kind, span)?));
                    *code = Some(held);
                }
            }
            if let Some(object) = receiver.take() {
                let held = self.temp(object, span)?;
                receiver = Some(held);
            }
        }
        // An argument that was left out takes the default.
        for place in 0..slots.len() {
            if codes[place].is_some() {
                continue;
            }
            let (_, kind, default) = &slots[place];
            let Some(default) = default.as_ref() else {
                return Err(cannot("this many arguments", span));
            };
            // A place that keeps what it is given (a field of a new
            // object, a `take` parameter) owns the list made for it
            // here, as it would own a `[]` written in the call. Any
            // other parameter only looks, and the statement frees
            // the list after the call (see `belongs`).
            let keeps = !lends.get(place).copied().unwrap_or(false);
            if is_literal(default) {
                let default = if keeps { self.value(default)? } else { self.expr(default)? };
                codes[place] = Some(self.coerce(default, kind, span)?.code);
                continue;
            }
            let before: Vec<(String, Kind, String)> = slots[..place]
                .iter()
                .zip(&codes)
                .filter_map(|((name, kind, _), code)| code.clone().map(|code| (name.clone(), kind.clone(), code)))
                .collect();
            let val = self.default_value(callee.as_ref(), default, keeps, &before, receiver.as_ref())?;
            let val = self.coerce(val, kind, span)?;
            let held = self.new_temp();
            self.line(format!("{}{held} = {};", c_type(kind, span)?, val.code));
            codes[place] = Some(held);
        }
        let mut codes: Vec<String> = codes.into_iter().flatten().collect();
        if let Some(object) = receiver {
            codes.insert(0, object.code);
        }
        Ok((format!("{c_name}({})", codes.join(", ")), vals))
    }

    /// A default that is not a literal, worked out at a call that leaves
    /// its argument out (decision 56.16). It reads what it would read
    /// where the function is written: the file's variables and functions,
    /// and the parameters before it, which `before` holds, with `self`
    /// for a method. Nothing of the function that calls.
    fn default_value(
        &mut self,
        callee: Option<&(usize, Vec<(String, FunctionEntry)>)>,
        default: &Expr,
        keeps: bool,
        before: &[(String, Kind, String)],
        receiver: Option<&Val>,
    ) -> Result<Val, Error> {
        let (within, sight) = callee.cloned().unwrap_or_default();
        let mut params = HashMap::new();
        let given = |c_name: &str, kind: &Kind| Variable {
            c_name: c_name.to_string(),
            kind: kind.clone(),
            mutable: false,
            boxed: None,
            used: Cell::new(false),
            owner: false,
            looks: false,
            inner: None,
        };
        if let Some(object) = receiver {
            params.insert("self".to_string(), given(&object.code, &object.kind));
        }
        for (name, kind, code) in before {
            params.insert(name.clone(), given(code, kind));
        }
        let mut outside = std::mem::take(&mut self.scopes);
        self.scopes = vec![outside.remove(0), params];
        let mut functions = std::mem::replace(&mut self.functions, self.top_functions.clone());
        for (name, entry) in sight {
            self.functions.insert(name, entry);
        }
        let within = std::mem::replace(&mut self.within, within);
        let once = self.once.take();
        let val = if keeps { self.value(default) } else { self.expr(default) };
        self.once = once;
        self.within = within;
        std::mem::swap(&mut self.functions, &mut functions);
        let mut inside = std::mem::take(&mut self.scopes);
        outside.insert(0, inside.remove(0));
        self.scopes = outside;
        val
    }

    /// Write the piece `piece` of the runtime (see `PIECES`).
    fn uses_piece(&mut self, piece: &'static str) {
        debug_assert!(PIECES.iter().any(|(name, _)| *name == piece), "`{piece}` is not in PIECES");
        if !self.pieces.contains(&piece) {
            self.pieces.push(piece);
        }
    }

    /// Turn a call to a function or method into a value.
    /// Keep the part of the runtime marked `#ifdef part`.
    fn uses_board_part(&mut self, part: &'static str) {
        if !self.board_parts.contains(&part) {
            self.board_parts.push(part);
        }
    }

    fn called(&mut self, call: String, vals: &[Val], returns: Kind) -> Val {
        let parts: Vec<&Val> = vals.iter().collect();
        if returns == Kind::None {
            // Nothing comes back, so the call is a statement by itself.
            self.line(format!("{call};"));
            let mut val = Val::from(String::new(), Kind::None, &parts);
            val.pre_fails = true;
            val.pre_calls = true;
            return val;
        }
        // With no collector, text that comes back is in the text room: the
        // answer, or the first of several values (decision 57).
        let mut val = Val::from(call, returns, &parts);
        if self.collector || !is_text(first_value(&val.kind)) {
            val = val.fixed();
        }
        val.fails = true;
        val.calls = true;
        val
    }

    /// A call of a function of a library of C, as C calls it: each
    /// number checked to fit what the function takes, each list lent for
    /// the call, and what comes back made a value of Zeno (decision
    /// 44.10). The C of `crc.crc16(frame, len(frame))` is about
    ///
    /// ```c
    /// void *t2 = zeno_c_lend(z_frame, ...);
    /// int64_t t3 = (int64_t)crc16((const uint8_t *)t2, ZENO_C_FIT(size_t, t1, ...));
    /// zeno_c_give_back(z_frame, t2, ...);
    /// ```
    fn c_call(&mut self, number: usize, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        use crate::bridge_c::Class;
        let library = self.types.c_libraries[number].clone();
        let Some(function) = library.functions.iter().find(|function| function.name == name) else {
            return Err(cannot(format!("`{name}`, which `{}` does not declare", library.label), span));
        };
        let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        let vals = self.sequence(&exprs)?;
        let mut passed = Vec::new();
        let mut after = Vec::new();
        for (index, (val, (of, class))) in vals.iter().zip(&function.takes).enumerate() {
            let what = c_string(&format!("the {} parameter of `{name}` (`{}`)", crate::bridge_c::ordinal(index + 1), of.text));
            let t = &of.text;
            let code = match (class, &val.kind) {
                (Class::Whole, Kind::Int) if self.minimal => format!("(({t})({}))", val.code),
                (Class::Whole, Kind::Int) => {
                    let held = self.temp(val.clone(), span)?;
                    format!("ZENO_C_FIT({t}, {}, {what}, {})", held.code, self.at(span))
                }
                (Class::U64, Kind::U64 | Kind::Int) | (Class::Float, Kind::Float | Kind::F32) | (Class::Bool, Kind::Bool) => {
                    format!("(({t})({}))", val.code)
                }
                (Class::Text { .. } | Class::Pointer { .. }, Kind::None) => format!("(({t})NULL)"),
                (Class::Pointer { .. }, Kind::CPointer(_)) => format!("(({t})({}))", val.code),
                (Class::Pointer { .. }, Kind::Optional(inner)) if matches!(**inner, Kind::CPointer(_)) => {
                    format!("(({t})({}))", val.code)
                }
                // Text: C reads it, and the copy C may change is its own.
                (Class::Text { konst: true } | Class::Pointer { konst: true, .. }, kind)
                    if matches!(kind, Kind::Str) || matches!(kind, Kind::Optional(inner) if **inner == Kind::Str) =>
                {
                    format!("(({t})({}))", val.code)
                }
                (Class::Text { .. } | Class::Pointer { .. }, kind)
                    if matches!(kind, Kind::Str) || matches!(kind, Kind::Optional(inner) if **inner == Kind::Str) =>
                {
                    self.built = !self.collector;
                    format!("(({t})zeno_c_text_copy({}))", val.code)
                }
                // A list is lent: copied into memory as C lays it out,
                // and copied back after the call unless C promised not to
                // change it, as `zeno run` does.
                (Class::Text { konst } | Class::Pointer { konst, .. }, Kind::List(item)) => {
                    let to = match class {
                        Class::Pointer { to, .. } if to.ends_with('*') => {
                            return Err(cannot(format!("a list given for a pointer to a pointer (`{t}`)"), span))
                        }
                        Class::Pointer { to, .. } if to != "void" => to.clone(),
                        _ => "unsigned char".to_string(),
                    };
                    let bytes = **item == Kind::Byte;
                    // A list[u8] is bytes side by side already (36.2): C
                    // is given them where they are, with no copy, and
                    // what it writes there is what the list holds after.
                    if bytes && matches!(to.as_str(), "unsigned char" | "uint8_t" | "char") {
                        let list = self.temp(val.clone(), span)?;
                        passed.push(format!("(({t})ZENO_BYTES({}))", list.code));
                        continue;
                    }
                    if !matches!(**item, Kind::Byte | Kind::Int | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool) {
                        return Err(cannot(format!("a list of these given for `{t}`"), span));
                    }
                    let float = matches!(**item, Kind::Float | Kind::F32);
                    let memory = self.new_temp();
                    self.line(format!(
                        "void *{memory} = zeno_c_lend({}, {bytes}, {float}, sizeof({to}), ZENO_C_CODE({to}), {what}, {});",
                        val.code,
                        self.at(span)
                    ));
                    after.push(format!(
                        "zeno_c_give_back({}, {memory}, {bytes}, {float}, sizeof({to}), ZENO_C_CODE({to}), {});",
                        val.code,
                        !konst
                    ));
                    format!("(({t}){memory})")
                }
                _ => return Err(cannot(format!("this value given for `{t}`"), span)),
            };
            passed.push(code);
        }
        let call = format!("{name}({})", passed.join(", "));
        let (code, kind) = match &function.gives.1 {
            Class::Nothing => (call, Kind::None),
            Class::Whole => (format!("((int64_t)({call}))"), Kind::Int),
            Class::U64 => (format!("((uint64_t)({call}))"), Kind::U64),
            Class::Float => (format!("((double)({call}))"), Kind::Float),
            Class::Bool => (format!("((bool)({call}))"), Kind::Bool),
            Class::Text { .. } => {
                self.built = !self.collector;
                (format!("zeno_c_text({call})"), Kind::Optional(Box::new(Kind::Str)))
            }
            Class::Pointer { to, .. } => {
                (format!("((void *)({call}))"), Kind::Optional(Box::new(Kind::CPointer(to.clone()))))
            }
        };
        if after.is_empty() {
            return Ok(self.called(code, &vals, kind));
        }
        let val = self.called(code, &vals, kind);
        let val = if val.kind == Kind::None { val } else { self.temp(val, span)? };
        for line in after {
            self.line(line);
        }
        Ok(val)
    }

    /// A call to one of the program's own functions.
    fn call_function(&mut self, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let entry = &self.functions[name];
        let (c_name, returns) = (entry.c_name.clone(), entry.kind.returns.clone());
        let slots: Vec<_> = entry
            .decl
            .params
            .iter()
            .zip(&entry.kind.params)
            .map(|(param, kind)| (param.name.clone(), kind.clone(), param.default.clone()))
            .collect();
        let (raises, owned, lends) = (entry.decl.raises.clone(), entry.owned.clone(), entry.lends.clone());
        self.callee = Some((entry.within, entry.sight.clone()));
        let (call, vals) = self.call_c(&c_name, &slots, &owned, &lends, None, args, span)?;
        let call = with_args(&call, &self.outs_for(&returns, span)?);
        if self.firmware && !raises.is_empty() {
            return self.checked_call(call, returns, &raises, span);
        }
        Ok(self.called(call, &vals, returns))
    }

    /// For the call of a function that gives several values, where the
    /// later ones go: what `Receive` or `return` said (decision 57).
    fn outs_for(&mut self, returns: &Kind, span: Span) -> Result<Vec<String>, Error> {
        if later_values(returns).is_empty() {
            return Ok(Vec::new());
        }
        self.outs.take().ok_or_else(|| cannot("several values given anywhere but to as many places", span))
    }

    /// `Point(3.0, 4.0)`: a new object.
    fn construct(&mut self, class: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let entry = &self.classes[class];
        let slots = entry.fields.clone();
        let owned: Vec<bool> = slots.iter().map(|(field, _, _)| entry.owned_fields.contains(field)).collect();
        let (call, vals) = self.call_c(&format!("zeno_new_{}", ident(class)), &slots, &owned, &[], None, args, span)?;
        let parts: Vec<&Val> = vals.iter().collect();
        Ok(Val::from(call, Kind::Class(class.to_string()), &parts))
    }

    /// `Circle(1.0)`: a new value of an enum.
    fn construct_case(&mut self, owner: &str, index: usize, args: &[Arg], span: Span) -> Result<Val, Error> {
        let (case, fields) = self.enums[owner].cases[index].clone();
        if fields.is_empty() {
            return Err(cannot(format!("`{case}` called like a function: it carries no values"), span));
        }
        // Which of the values it carries have one owner: the new value
        // takes those over.
        let takes = self.enums[owner].takes[index].clone();
        let slots: Vec<_> = fields.iter().map(|(name, kind)| (name.clone(), kind.clone(), None)).collect();
        let c_function = format!("zeno_new_{}_{}", ident(owner), ident(&case));
        let (call, vals) = self.call_c(&c_function, &slots, &takes, &[], None, args, span)?;
        let parts: Vec<&Val> = vals.iter().collect();
        Ok(Val::from(call, Kind::Enum(owner.to_string()), &parts))
    }

    /// `object.name(args)`: a method of a class, or of a list.
    fn method(&mut self, object: &Expr, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let makes_bytes = std::mem::take(&mut self.makes_bytes);
        let receiver = self.expr(object)?;
        match receiver.kind.clone() {
            Kind::CLibrary(number) => self.c_call(number, name, args, span),
            Kind::Class(class) => {
                let Some(entry) = self.classes[&class].methods.get(name) else {
                    // Not a method. It may be a field that holds a function.
                    let field = self.classes[&class].fields.iter().find(|(field, _, _)| field == name);
                    let Some((_, kind @ Kind::Function(_), _)) = field else {
                        return Err(cannot(format!("the method `{name}`"), span));
                    };
                    let mut function = Val::from(format!("{}->z_{name}", receiver.code), kind.clone(), &[&receiver]);
                    function.reads = true;
                    return self.call_value(function, args, span);
                };
                let (c_name, returns) = (entry.c_name.clone(), entry.kind.returns.clone());
                // The first parameter is `self`, which the receiver fills.
                let slots: Vec<_> = entry
                    .decl
                    .params
                    .iter()
                    .zip(&entry.kind.params)
                    .skip(1)
                    .map(|(param, kind)| (param.name.clone(), kind.clone(), param.default.clone()))
                    .collect();
                let raises = entry.decl.raises.clone();
                let owned: Vec<bool> = entry.owned.iter().skip(1).copied().collect();
                let lends: Vec<bool> = entry.lends.iter().skip(1).copied().collect();
                let (call, vals) = self.call_c(&c_name, &slots, &owned, &lends, Some(receiver), args, span)?;
                let call = with_args(&call, &self.outs_for(&returns, span)?);
                if self.firmware && !raises.is_empty() {
                    return self.checked_call(call, returns, &raises, span);
                }
                Ok(self.called(call, &vals, returns))
            }
            // A register of the board: one call into the runtime, with
            // the address and how many bits wide the register is.
            Kind::Register(width) => {
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                let bits = width.bits;
                match (name, vals.as_slice()) {
                    ("read", [register]) => {
                        let call = format!("zeno_register_read({}, {bits})", register.code);
                        let mut val = Val::from(call, Kind::Int, &[register]);
                        // A register can change by itself, so two reads
                        // are two reads.
                        val.calls = true;
                        Ok(val)
                    }
                    ("write", [register, value]) if value.kind == Kind::Int => {
                        let call = format!("zeno_register_write({}, {bits}, {})", register.code, value.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    _ => Err(cannot(format!("`{name}` on a register called this way"), span)),
                }
            }
            // A pin of the board: `led.on()`. Each is one call into the
            // runtime, with the pin's number.
            Kind::Pin => {
                if !args.is_empty() {
                    return Err(cannot(format!("`{name}` on a pin with arguments"), span));
                }
                match name {
                    "on" | "off" | "toggle" => {
                        let call = format!("zeno_pin_{name}({})", receiver.code);
                        Ok(self.called(call, &[receiver], Kind::None))
                    }
                    "is_on" => {
                        let mut val = Val::from(format!("zeno_pin_is_on({})", receiver.code), Kind::Bool, &[&receiver]);
                        // What it reads is changed by `on` and `off`.
                        val.reads = true;
                        Ok(val)
                    }
                    _ => Err(cannot(format!("the method `{name}`"), span)),
                }
            }
            // `beat.wait()`, `beat.ticks()` and `beat.restart()`: one call
            // into the runtime each. The clock moves by itself, so two
            // calls are two calls.
            Kind::Ticker => match (name, args) {
                ("wait" | "ticks" | "restart", []) => {
                    self.uses_board_part("ZENO_TICKING");
                    let returns = if name == "restart" { Kind::None } else { Kind::Int };
                    Ok(self.called(format!("zeno_ticker_{name}({})", receiver.code), &[receiver], returns))
                }
                _ => Err(cannot(format!("the method `{name}` of a ticker"), span)),
            },
            // `lamp.duty(0.25)` and `servo.pulse(1500 us)`: one call into
            // the runtime each, the pulse's time in seconds.
            Kind::PwmPin => {
                self.uses_board_part("ZENO_PWM_PINS");
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                let at = self.at(span);
                match (name, vals.as_slice()) {
                    ("duty", [pin, share]) if share.kind == Kind::Float => {
                        let call = format!("zeno_pwm_duty({}, {}, {at})", pin.code, share.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    ("pulse", [pin, time]) => {
                        let scale = match &time.kind {
                            Kind::Quantity(unit) => units::convert(unit, "s"),
                            _ => None,
                        };
                        let Some(scale) = scale else {
                            return Err(cannot("`pulse` of something that is not a time", span));
                        };
                        let call = format!("zeno_pwm_pulse({}, {}, {at})", pin.code, scaled(&time.code, scale));
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    _ => Err(cannot(format!("`{name}` on a PWM pin called this way"), span)),
                }
            }
            // `sensor.voltage()` and `sensor.count()`: what the converter
            // reads now. A voltage changes by itself, so two reads are
            // two reads.
            Kind::AnalogPin => match (name, args) {
                ("voltage" | "count", []) => {
                    self.uses_board_part("ZENO_ANALOG_PINS");
                    let returns = if name == "voltage" { Kind::Quantity("V".to_string()) } else { Kind::Int };
                    Ok(self.called(format!("zeno_analog_{name}({})", receiver.code), &[receiver], returns))
                }
                _ => Err(cannot(format!("the method `{name}` of an analog pin"), span)),
            },
            // A serial port: `port.write(text)`, `port.write_bytes(bytes)`,
            // `port.read_byte()`. Each is one call into the runtime.
            Kind::Serial => {
                // Also for a function that is handed a port and makes none.
                self.uses_board_part("ZENO_SERIAL_PORTS");
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                match (name, vals.as_slice()) {
                    ("write", [port, text]) if text.kind == Kind::Str => {
                        let call = format!("zeno_serial_write({}, {})", port.code, text.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    ("write_bytes", [port, bytes]) if bytes.kind == Kind::List(Box::new(Kind::Byte)) => {
                        let call = format!("zeno_serial_write_bytes({}, {})", port.code, bytes.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    ("read_byte", [port]) => {
                        let call = format!("zeno_found(zeno_serial_read_byte({}))", port.code);
                        Ok(self.called(call, &vals, Kind::Optional(Box::new(Kind::Int))))
                    }
                    ("has_byte", [port]) => {
                        let call = format!("zeno_serial_has_byte({})", port.code);
                        Ok(self.called(call, &vals, Kind::Bool))
                    }
                    _ => Err(cannot(format!("`{name}` on a serial port called this way"), span)),
                }
            }
            // `button.is_low()`: what the pin reads now. The board can
            // change it at any time, so it is read each time it is asked.
            // `button.interrupt(falling)`: the board is told to count,
            // which is a call, and is not worked out twice.
            Kind::InputPin if name == "interrupt" => {
                self.uses_board_part("ZENO_INTERRUPTS");
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                match vals.as_slice() {
                    [pin, edge] if edge.kind == Kind::Str => {
                        let call = format!("zeno_interrupt({}, {}, {})", pin.code, edge.code, self.at(span));
                        Ok(self.called(call, &vals, Kind::Interrupt).failing_if(!self.minimal))
                    }
                    _ => Err(cannot("`interrupt` called this way", span)),
                }
            }
            // `press.wait()` and `press.edges()`: one call into the
            // runtime each. An edge comes by itself, so two calls are
            // two calls.
            Kind::Interrupt => match (name, args) {
                ("edges", []) => {
                    self.uses_board_part("ZENO_INTERRUPTS");
                    Ok(self.called(format!("zeno_interrupt_edges({})", receiver.code), &[receiver], Kind::Int))
                }
                ("wait", []) => {
                    self.uses_board_part("ZENO_INTERRUPTS");
                    let call = format!("zeno_interrupt_wait({}, {})", receiver.code, self.at(span));
                    Ok(self.called(call, &[receiver], Kind::Int).failing_if(!self.minimal))
                }
                _ => Err(cannot(format!("the method `{name}` of an interrupt"), span)),
            },
            Kind::InputPin => match (name, args) {
                ("is_high" | "is_low", []) => {
                    let mut val = Val::from(format!("zeno_input_{name}({})", receiver.code), Kind::Bool, &[&receiver]);
                    val.reads = true;
                    Ok(val)
                }
                _ => Err(cannot(format!("the method `{name}` of an input pin"), span)),
            },
            Kind::Str => {
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                let parts: Vec<&Val> = vals.iter().collect();
                let all_text = vals.iter().all(|val| val.kind == Kind::Str);
                // `slice(from, to)`: everything after the text is a position.
                let positions = vals[1..].iter().all(|val| val.kind == Kind::Int);
                let codes: Vec<&str> = vals.iter().map(|val| val.code.as_str()).collect();
                let mut val = match (name, codes.as_slice()) {
                    ("upper", [text]) => Val::from(format!("zeno_upper({text})"), Kind::Str, &parts),
                    ("lower", [text]) => Val::from(format!("zeno_lower({text})"), Kind::Str, &parts),
                    ("strip", [text]) => Val::from(format!("zeno_strip({text})"), Kind::Str, &parts),
                    ("split", [text]) => {
                        Val::from(format!("zeno_split_blanks({text})"), Kind::List(Box::new(Kind::Str)), &parts)
                    }
                    ("split", [text, separator]) if all_text => Val::from(
                        format!("zeno_split({text}, {separator}, {})", self.at(span)),
                        Kind::List(Box::new(Kind::Str)),
                        &parts,
                    )
                    .failing_if(!self.minimal),
                    ("lines", [text]) => {
                        Val::from(format!("zeno_lines({text})"), Kind::List(Box::new(Kind::Str)), &parts)
                    }
                    ("replace", [text, old, new]) if all_text => {
                        Val::from(format!("zeno_replace({text}, {old}, {new})"), Kind::Str, &parts)
                    }
                    ("starts_with", [text, start]) if all_text => {
                        Val::from(format!("zeno_starts_with({text}, {start})"), Kind::Bool, &parts)
                    }
                    ("ends_with", [text, end]) if all_text => {
                        Val::from(format!("zeno_ends_with({text}, {end})"), Kind::Bool, &parts)
                    }
                    // The runtime answers -1 for "not there", and
                    // `zeno_found` makes that the `none` of an
                    // `int or none`.
                    ("find", [text, part]) if all_text => Val::from(
                        format!("zeno_found(zeno_find({text}, {part}))"),
                        Kind::Optional(Box::new(Kind::Int)),
                        &parts,
                    ),
                    ("slice", [text, from]) if positions => {
                        Val::from(format!("zeno_slice({text}, {from}, 0, true, {})", self.at(span)), Kind::Str, &parts)
                            .failing_if(!self.minimal)
                    }
                    ("slice", [text, from, to]) if positions => {
                        Val::from(format!("zeno_slice({text}, {from}, {to}, false, {})", self.at(span)), Kind::Str, &parts)
                            .failing_if(!self.minimal)
                    }
                    ("code", [text]) => {
                        Val::from(format!("zeno_char_code({text}, {})", self.at(span)), Kind::Int, &parts).failing_if(!self.minimal)
                    }
                    _ => return Err(cannot(format!("the str method `{name}` called this way"), span)),
                };
                // The list of pieces is new, and nobody's yet.
                val.made = name == "split" || name == "lines";
                Ok(val)
            }
            Kind::List(item) => {
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                let parts: Vec<&Val> = vals.iter().collect();
                match (name, vals.as_slice()) {
                    ("map" | "filter" | "reduce", _) => {
                        self.over_list(name, vals, &item, makes_bytes, span)
                    }
                    // With a function, `sort` is a walk that asks for
                    // the key of each item, and then the sorting.
                    ("sort", [_, _]) => self.over_list(name, vals, &item, false, span),
                    ("append", [list, value]) => {
                        self.stays(value, span)?;
                        let value = self.keep(value.clone(), is_written(&args[0].value));
                        let value = self.coerce(value, &item, span)?;
                        let slot = self.kept_slot(&value.kind, &value.code, span)?;
                        self.line(format!("{}({}, {slot});", list_fn(&item, "append"), list.code));
                        let mut val = Val::from(String::new(), Kind::None, &parts);
                        val.pre_calls = true;
                        Ok(val)
                    }
                    ("pop", [list]) | ("pop", [list, _]) => {
                        let slot = match vals.get(1) {
                            // `pop(2)`: the item at that position.
                            Some(position) if position.kind == Kind::Int => {
                                format!("{}({}, {}, {})", list_fn(&item, "pop_at"), list.code, position.code, self.at(span))
                            }
                            Some(_) => return Err(cannot("`pop` with a position that is not an int", span)),
                            None => format!("{}({}, {})", list_fn(&item, "pop"), list.code, self.at(span)),
                        };
                        // The list is done with it: the statement gets it,
                        // and the list's copy of text, or its box, is freed.
                        let code = self.taken(&item, &slot, span)?;
                        let mut val = Val::from(code, read_kind(&item), &parts);
                        // It changes the list, which is as visible as a call.
                        val.fails = true;
                        val.calls = true;
                        Ok(val)
                    }
                    // The list keeps what it is given, as `append` does.
                    ("insert", [list, position, value]) if position.kind == Kind::Int => {
                        self.stays(value, span)?;
                        let value = self.keep(value.clone(), is_written(&args[1].value));
                        let value = self.coerce(value, &item, span)?;
                        let slot = self.kept_slot(&value.kind, &value.code, span)?;
                        let insert = list_fn(&item, "insert");
                        let call = format!("{insert}({}, {}, {slot}, {})", list.code, position.code, self.at(span));
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    ("reverse", [list]) => {
                        let call = format!("{}({})", list_fn(&item, "reverse"), list.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    // The runtime compares two slots, and has to be told
                    // which member of the slot to read.
                    ("sort", [list]) if *item == Kind::Byte => {
                        Ok(self.called(format!("zeno_bytes_sort({})", list.code), &vals, Kind::None))
                    }
                    ("sort", [list]) => {
                        let kind = match &*item {
                            Kind::Int => 0,
                            Kind::Float | Kind::F32 => 1,
                            Kind::Str => 2,
                            Kind::U64 => 3,
                            _ => return Err(cannot("`sort` on a list of anything but numbers or text", span)),
                        };
                        let call = format!("zeno_list_sort({}, {kind})", list.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    // Items that have one owner (and, with no collector,
                    // text, which the list keeps copies of) end with the
                    // list. `clear` is the end of each of them:
                    //
                    //     zeno_list *t1 = z_tasks;
                    //     for (int64_t i = 0; i < t1->len; i++) zeno_drop_Task(((z_Task *)t1->items[i].p));
                    //     zeno_list_clear(t1);
                    ("clear", [list]) => {
                        let list = self.temp(list.clone(), span)?;
                        if !self.collector && is_boxed(&item) {
                            self.line(format!("for (int64_t i = 0; i < {}->len; i++) zeno_free({}->items[i].p);", list.code, list.code));
                        } else if self.drops.owns(&item) || (!self.collector && is_text(&item)) {
                            let place = from_slot(&item, &format!("{}->items[i]", list.code), span)?;
                            let each = self.drops.code(&item, &place);
                            self.line(format!("for (int64_t i = 0; i < {}->len; i++) {each}", list.code));
                        }
                        let call = format!("{}({})", list_fn(&item, "clear"), list.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    ("find", [list, value]) if *item == Kind::Byte => {
                        let mut val = Val::from(
                            format!("zeno_found(zeno_bytes_find({}, {}))", list.code, value.code),
                            Kind::Optional(Box::new(Kind::Int)),
                            &parts,
                        );
                        val.reads = true;
                        Ok(val)
                    }
                    ("find", [list, value]) => {
                        let same = self.eq_adapter(&item, span)?;
                        let value = self.coerce(value.clone(), &item, span)?;
                        let slot = self.looked_for(&item, &value.code, span)?;
                        let mut val = Val::from(
                            format!("zeno_found(zeno_list_find({}, {slot}, {same}))", list.code),
                            Kind::Optional(Box::new(Kind::Int)),
                            &parts,
                        );
                        // What it finds depends on what is in the list now.
                        val.reads = true;
                        Ok(val)
                    }
                    ("slice", [list, from]) | ("slice", [list, from, _]) if from.kind == Kind::Int => {
                        let ends = match vals.get(2) {
                            Some(to) if to.kind == Kind::Int => format!("{}, false", to.code),
                            Some(_) => return Err(cannot("`slice` with a position that is not an int", span)),
                            None => "0, true".to_string(),
                        };
                        let slice =
                            format!("{}({}, {}, {ends}, {})", list_fn(&item, "slice"), list.code, from.code, self.at(span));
                        // With no collector a list of text has its own
                        // copy of each item, and so has the slice.
                        let code = match self.keeps_copies(&item) {
                            true => self.keep_items(&item, &slice, span)?,
                            false => slice,
                        };
                        let mut val = Val::from(code, Kind::List(item.clone()), &parts).failing_if(!self.minimal);
                        // The new list is nobody's yet.
                        val.made = true;
                        val.reads = true;
                        Ok(val)
                    }
                    ("join", [list, separator]) if *item == Kind::Str && separator.kind == Kind::Str => {
                        let mut val = Val::from(
                            format!("zeno_list_join({}, {})", list.code, separator.code),
                            Kind::Str,
                            &parts,
                        );
                        val.reads = true;
                        Ok(val)
                    }
                    _ => Err(cannot(format!("the list method `{name}`"), span)),
                }
            }
            Kind::Dict(key, value) => {
                let exprs: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
                let vals = self.sequence_after(vec![receiver], &exprs)?;
                let parts: Vec<&Val> = vals.iter().collect();
                match (name, vals.as_slice()) {
                    // `get(key)`: the value, or `none`. The runtime has
                    // one of these for each kind of value, because a
                    // number that may be `none` travels with a flag.
                    ("get", [dict, wanted]) => {
                        let which = match &*value {
                            Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "int",
                            Kind::Float | Kind::F32 | Kind::Quantity(_) => "float",
                            Kind::Bool => "bool",
                            Kind::Str => "str",
                            // A list, an object, another dict: NULL is
                            // `none`, and is what comes back for a key
                            // that is not there.
                            value if is_pointer(value) => "",
                            _ => return Err(cannot("`get` on a dict with these values", span)),
                        };
                        let code = match which {
                            "" => {
                                let slot = format!(
                                    "zeno_dict_get({}, {}, (zeno_slot){{.p = NULL}})",
                                    dict.code,
                                    to_slot(&key, &wanted.code, span)?
                                );
                                from_slot(&value, &slot, span)?
                            }
                            which => format!("zeno_dict_get_{which}({}, {})", dict.code, to_slot(&key, &wanted.code, span)?),
                        };
                        let mut val = Val::from(code, Kind::Optional(value.clone()), &parts);
                        val.reads = true;
                        Ok(self.lend(val))
                    }
                    // `get(key, instead)`.
                    ("get", [dict, wanted, instead]) => {
                        let slot = format!(
                            "zeno_dict_get({}, {}, {})",
                            dict.code,
                            to_slot(&key, &wanted.code, span)?,
                            to_slot(&value, &instead.code, span)?
                        );
                        let mut val = Val::from(from_slot(&value, &slot, span)?, (*value).clone(), &parts);
                        val.reads = true;
                        Ok(self.lend(val))
                    }
                    ("pop", [dict, wanted]) => {
                        let slot =
                            format!("zeno_dict_pop({}, {}, {})", dict.code, to_slot(&key, &wanted.code, span)?, self.at(span));
                        let mut code = from_slot(&value, &slot, span)?;
                        if !self.collector && is_text(&value) {
                            // The dict is done with it: the statement gets
                            // it, and the dict's copy is freed.
                            code = format!("zeno_text_take({code})");
                        }
                        let mut val = Val::from(code, (*value).clone(), &parts);
                        // It changes the dict, which is as visible as a call.
                        val.fails = true;
                        val.calls = true;
                        Ok(val)
                    }
                    // A new list each time, which is nobody's yet.
                    ("keys" | "values", [dict]) => {
                        let item = if name == "keys" { key } else { value };
                        let mut val = Val::from(format!("zeno_dict_{name}({})", dict.code), Kind::List(item), &parts);
                        val.made = true;
                        val.reads = true;
                        Ok(val)
                    }
                    ("clear", [dict]) => {
                        // A dict that owns its values frees each of them.
                        let clear = match self.drops.owns(&value) {
                            true => self.drops.dict_helper("clear", &value),
                            false => "zeno_dict_clear".to_string(),
                        };
                        let call = format!("{clear}({})", dict.code);
                        Ok(self.called(call, &vals, Kind::None))
                    }
                    _ => Err(cannot(format!("the dict method `{name}`"), span)),
                }
            }
            _ => Err(cannot(format!("the method `{name}`"), span)),
        }
    }

    /// `object.name`: a field of an object, or a quantity in another unit.
    /// `object.name`. A number in an object a parameter holds may be read
    /// once, at the start of the function, and kept in a local:
    ///
    ///     double t4 = z_c->z_mass;
    ///
    /// See `ast::fields_read_once` for when.
    fn field(&mut self, object: &Expr, name: &str, span: Span) -> Result<Val, Error> {
        let key = match (&object.kind, &self.once) {
            (ExprKind::Name(param), Some(once)) if once.params.contains(param) && !once.written.contains(name) => {
                (param.clone(), name.to_string())
            }
            _ => return self.field_read(object, name, span),
        };
        if let Some((local, kind)) = self.once.as_ref().and_then(|once| once.made.get(&key)) {
            return Ok(Val::plain(local.clone(), kind.clone()));
        }
        let val = self.field_read(object, name, span)?;
        if !matches!(val.kind, Kind::Int | Kind::Float | Kind::Bool) || val.code.contains("zeno_") {
            return Ok(val);
        }
        let local = self.new_temp();
        // At the top, after the ones before it.
        let at = self.once.as_ref().map_or(0, |once| once.made.len());
        self.lines.insert(at, (1, format!("{}{local} = {};", c_type(&val.kind, span)?, val.code)));
        if let Some(once) = self.once.as_mut() {
            once.made.insert(key, (local.clone(), val.kind.clone()));
        }
        Ok(Val::plain(local, val.kind))
    }

    fn field_read(&mut self, object: &Expr, name: &str, span: Span) -> Result<Val, Error> {
        if let ExprKind::Name(owner) = &object.kind {
            if self.lookup(owner).is_none() {
                if let ("math", Some(number)) = (owner.as_str(), math_constant(name)) {
                    return Ok(Val::plain(format!("{number:?}"), Kind::Float));
                }
                if owner == "board" && name == "led" {
                    return Ok(Val::plain("ZENO_LED", Kind::Pin));
                }
                if owner == "board" && matches!(name, "pull_up" | "pull_down" | "rising" | "falling" | "either") {
                    return Ok(Val::plain(c_string(name), Kind::Str).fixed());
                }
                // `Shape.Idle`
                if self.enums.contains_key(owner) && self.cases.get(name).is_some_and(|(of, _)| of == owner) {
                    let index = self.cases[name].1;
                    if self.enums[owner].cases[index].1.is_empty() {
                        return Ok(Val::plain(format!("(&zeno_case_{}_{})", ident(owner), ident(name)), Kind::Enum(owner.clone())));
                    }
                }
            }
        }
        let val = self.expr(object)?;
        match &val.kind {
            // `hal.GPIO_PIN_5`: a value a library of C declares, read by
            // its name, as C reads it (decision 44.11).
            Kind::CLibrary(number) => {
                use crate::bridge_c::Class;
                let library = self.types.c_libraries[*number].clone();
                let Some(value) = library.functions.iter().find(|function| function.name == name && function.value) else {
                    return Err(cannot(format!("`{name}` of a library of C used as a value"), span));
                };
                let (code, kind) = match &value.gives.1 {
                    Class::Whole => (format!("((int64_t)({name}))"), Kind::Int),
                    Class::U64 => (format!("((uint64_t)({name}))"), Kind::U64),
                    Class::Float => (format!("((double)({name}))"), Kind::Float),
                    Class::Bool => (format!("((bool)({name}))"), Kind::Bool),
                    Class::Text { .. } => {
                        self.built = !self.collector;
                        (format!("zeno_c_text({name})"), Kind::Optional(Box::new(Kind::Str)))
                    }
                    Class::Pointer { to, .. } => {
                        (format!("((void *)({name}))"), Kind::Optional(Box::new(Kind::CPointer(to.clone()))))
                    }
                    Class::Nothing => return Err(cannot(format!("`{name}`, which is `void`"), span)),
                };
                let mut read = Val::plain(code, kind);
                // A variable of C may change in any call.
                read.reads = true;
                Ok(read)
            }
            // `(3 s).ms`: the same quantity in another unit.
            Kind::Quantity(unit) if units::is_unit(name) => match units::convert(unit, name) {
                Some(scale) => Ok(Val::from(scaled(&val.code, scale), Kind::Quantity(name.to_string()), &[&val])),
                None => Err(cannot(format!("`.{name}` on this"), span)),
            },
            Kind::Class(class) => match self.classes[class].fields.iter().find(|(field, _, _)| field == name) {
                Some((_, kind, _)) => {
                    let code = match self.classes[class].sized.contains_key(name) {
                        // A small field is an ordinary int again as soon
                        // as it is read. Left as a `uint32_t`, C would do
                        // the sums that follow without a sign.
                        true => format!("((int64_t){}->z_{name})", val.code),
                        false => format!("{}->z_{name}", val.code),
                    };
                    let mut field = Val::from(code, kind.clone(), &[&val]);
                    // A function that is called may change the field.
                    field.reads = true;
                    // A list or an object inside an object that the
                    // statement owns is gone when the statement is.
                    field.passing = val.passing && matches!(kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_));
                    Ok(self.lend(field))
                }
                None => Err(Error::new(
                    format!(
                        "`zeno build` cannot compile a method used as a value of an object that no variable holds, \
                         `.{name}`, yet. Put the object in a `let` first and use `.{name}` of that: then it builds. \
                         `zeno run` can still run this program"
                    ),
                    span,
                )),
            },
            Kind::Error if matches!(name, "message" | "kind") => {
                Ok(Val::from(format!("{}->{name}", val.code), Kind::Str, &[&val]))
            }
            // `led.number`: a pin is its number already.
            Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Interrupt if name == "number" => {
                Ok(Val { kind: Kind::Int, ..val })
            }
            // `beat.period`, in milliseconds.
            Kind::Ticker if name == "period" => {
                self.uses_board_part("ZENO_TICKING");
                Ok(Val::from(format!("zeno_ticker_period({})", val.code), Kind::Quantity("ms".to_string()), &[&val]))
            }
            // `sensor.max_count`: the board's, whichever pin it is.
            Kind::AnalogPin if name == "max_count" => {
                self.uses_board_part("ZENO_ANALOG_PINS");
                Ok(Val::from("zeno_analog_max()".to_string(), Kind::Int, &[&val]))
            }
            Kind::Register(_) if name == "address" => Ok(Val { kind: Kind::Int, ..val }),
            // The checker took it, so it is a method of a list, a text or
            // a dict, which is written in place where it is called.
            Kind::List(_) | Kind::Str | Kind::Dict(..) => {
                Err(cannot(format!("a method that comes with Zeno used as a value, `.{name}`,"), span))
            }
            _ => Err(cannot(format!("`.{name}` on this"), span)),
        }
    }

    /// `x in items`, `x not in items`, and the same for a piece of a str.
    fn contains(&mut self, op: BinaryOp, left: Val, right: Val, span: Span) -> Result<Val, Error> {
        let code = match (&left.kind, &right.kind) {
            (Kind::Int, Kind::List(items)) if **items == Kind::Byte => {
                format!("zeno_bytes_contains({}, {})", right.code, left.code)
            }
            (item, Kind::List(items)) if item == &**items => {
                let same = self.eq_adapter(item, span)?;
                format!("zeno_list_contains({}, {}, {same})", right.code, self.looked_for(item, &left.code, span)?)
            }
            // `none in items` and `2 in items`, where the items may be
            // `none`, first become what the list holds, as in LLVM.
            (item, Kind::List(items)) if joined(item, items).as_ref() == Some(&**items) => {
                let same = self.eq_adapter(items, span)?;
                let left = self.coerce(left.clone(), items, span)?;
                format!("zeno_list_contains({}, {}, {same})", right.code, self.looked_for(items, &left.code, span)?)
            }
            // `"Ada" in ages`: is there such a key?
            (item, Kind::Dict(key, _)) if item == &**key => {
                format!("(zeno_dict_find({}, {}) >= 0)", right.code, to_slot(item, &left.code, span)?)
            }
            (Kind::Str, Kind::Str) => format!("(strstr({}, {}) != NULL)", right.code, left.code),
            _ => return Err(cannot("`in` between these two types", span)),
        };
        let code = if op == BinaryOp::NotIn { format!("(!{code})") } else { code };
        let mut val = Val::from(code, Kind::Bool, &[&left, &right]);
        val.reads = true;
        Ok(val)
    }

    // ------------------------------------------------- Showing and comparing

    /// The C expression that turns a value into the text `print` shows.
    fn text(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        // Anything but text has to be turned into text, which builds some.
        if val.kind != Kind::Str && !self.collector {
            self.built = true;
        }
        Ok(match &val.kind {
            Kind::Str => val.code.clone(),
            Kind::Int => format!("zeno_str_int({})", val.code),
            Kind::Float => format!("zeno_str_float({})", val.code),
            Kind::F32 => format!("zeno_str_f32({})", val.code),
            Kind::U64 => format!("zeno_str_u64({})", val.code),
            Kind::Bool => format!("zeno_str_bool({})", val.code),
            Kind::Pin => format!("zeno_str_pin({})", val.code),
            Kind::InputPin => format!("zeno_str_input_pin({})", val.code),
            Kind::Serial => format!("zeno_str_serial({})", val.code),
            Kind::AnalogPin => format!("zeno_str_analog_pin({})", val.code),
            Kind::PwmPin => format!("zeno_str_pwm_pin({})", val.code),
            Kind::Interrupt => format!("zeno_str_interrupt({})", val.code),
            Kind::Ticker => {
                self.uses_board_part("ZENO_TICKING");
                format!("zeno_ticker_str({})", val.code)
            }
            // Where a pointer points is different on every run, so it
            // is not shown.
            Kind::Pointer(pointee) => c_string(&format!("<pointer[{}]>", pointee.name())),
            // As `zeno run` shows it (decision 44.10).
            Kind::CPointer(_) => format!("zeno_c_pointer_str({})", val.code),
            Kind::CLibrary(number) => c_string(&format!("<C library {}>", self.types.c_libraries[*number].label)),
            Kind::Register(_) => format!("zeno_str_register({})", val.code),
            Kind::Quantity(unit) => format!("zeno_str_quantity({}, {})", val.code, c_string(unit)),
            Kind::Class(name) | Kind::Enum(name) => format!("zeno_show_{}({})", ident(name), val.code),
            Kind::List(item) if **item == Kind::Byte => format!("zeno_bytes_str({})", val.code),
            Kind::List(item) => format!("zeno_list_str({}, {})", val.code, self.repr_adapter(item, span)?),
            Kind::Byte => format!("zeno_str_int({})", val.code),
            Kind::Dict(_, value) if !plain_value(value) && !matches!(**value, Kind::Other(_)) => {
                format!("zeno_dict_str_with({}, {})", val.code, self.repr_adapter(value, span)?)
            }
            Kind::Dict(..) => format!("zeno_dict_str({})", val.code),
            Kind::Error => format!("{}->message", val.code),
            Kind::Function(_) => format!("zeno_show_function({})", val.code),
            Kind::Optional(inner) => format!("{}({})", self.optional_text(inner, false, span)?, val.code),
            // `none` itself, or a call that gave nothing back.
            Kind::None => "\"none\"".to_string(),
            Kind::Other(name) => return Err(cannot(format!("printing a value of type {name}"), span)),
            Kind::Owned(_) | Kind::Several(_) => return Err(cannot("printing this", span)),
        })
    }

    /// The text for a value inside a list or an object. Only a str differs:
    /// there it is shown in quotes.
    fn repr(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        if !self.collector {
            self.built = true;
        }
        match &val.kind {
            Kind::Str => Ok(format!("zeno_repr_str({})", val.code)),
            Kind::Optional(inner) => Ok(format!("{}({})", self.optional_text(inner, true, span)?, val.code)),
            _ => self.text(val, span),
        }
    }

    /// A C expression that is true when two values of this kind are equal.
    /// Objects, enum values and lists are equal when their contents are.
    fn equal(&mut self, kind: &Kind, a: &str, b: &str, span: Span) -> Result<String, Error> {
        Ok(match kind {
            Kind::Int | Kind::Byte | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Quantity(_) => {
                format!("({a} == {b})")
            }
            // Two pointers are equal when they point at the same item.
            Kind::Pointer(_) | Kind::Register(_) | Kind::CPointer(_) => format!("({a} == {b})"),
            Kind::CLibrary(_) => "true".to_string(),
            Kind::Str => format!("(strcmp({a}, {b}) == 0)"),
            Kind::Class(name) | Kind::Enum(name) => format!("zeno_same_{}({a}, {b})", ident(name)),
            Kind::List(item) if **item == Kind::Byte => format!("zeno_bytes_eq({a}, {b})"),
            Kind::List(item) => format!("zeno_list_eq({a}, {b}, {})", self.eq_adapter(item, span)?),
            Kind::Dict(_, value) if !plain_value(value) && !matches!(**value, Kind::Other(_)) => {
                format!("zeno_dict_eq_with({a}, {b}, {})", self.eq_adapter(value, span)?)
            }
            Kind::Dict(..) => format!("zeno_dict_eq({a}, {b})"),
            Kind::Error => format!("zeno_error_same({a}, {b})"),
            Kind::Optional(inner) => format!("{}({a}, {b})", self.optional_same(inner, span)?),
            Kind::None => "true".to_string(),
            // Two function values are equal only if they are the same one.
            Kind::Function(_) => format!("({a} == {b})"),
            Kind::Other(_) | Kind::Owned(_) | Kind::Several(_) => return Err(cannot("comparing these", span)),
        })
    }

    /// `==` and the rest, between two compiled values.
    fn compare(&mut self, op: BinaryOp, left: &Val, right: &Val, span: Span) -> Result<String, Error> {
        let plain = |kind: &Kind| matches!(kind, Kind::Int | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool | Kind::Str | Kind::Quantity(_));
        if plain(&left.kind) && plain(&right.kind) || !matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
            return comparison(op, left, right, span);
        }
        // A list of bytes and a list of ints hold numbers alike, and are
        // equal when the numbers are (36.2).
        let ints = Kind::List(Box::new(Kind::Int));
        let bytes = Kind::List(Box::new(Kind::Byte));
        let mixed = match (&left.kind, &right.kind) {
            (a, b) if *a == bytes && *b == ints => Some((&left.code, &right.code)),
            (a, b) if *a == ints && *b == bytes => Some((&right.code, &left.code)),
            _ => None,
        };
        if let Some((bytes, ints)) = mixed {
            let same = format!("zeno_bytes_eq_ints({bytes}, {ints})");
            return Ok(if op == BinaryOp::NotEq { format!("(!{same})") } else { same });
        }
        // Everything else is equal when its contents are. `x == none` and
        // `x == 5`, where `x` may be `none`, first bring both sides to the
        // same kind.
        let Some(kind) = joined(&left.kind, &right.kind) else {
            return Err(cannot(format!("`{}` between these two types", op.text()), span));
        };
        let left = self.coerce(left.clone(), &kind, span)?;
        let right = self.coerce(right.clone(), &kind, span)?;
        let same = self.equal(&kind, &left.code, &right.code, span)?;
        Ok(if op == BinaryOp::Eq { same } else { format!("(!{same})") })
    }

    /// The runtime's list functions do not know what a list holds. To show
    /// a list they are handed a small function that shows one item. This
    /// writes that function for a kind, the first time it is needed, and
    /// returns its name.
    fn repr_adapter(&mut self, kind: &Kind, span: Span) -> Result<String, Error> {
        let name = format!("zeno_item_repr_{}", key(kind));
        if self.adapters.insert(name.clone()) {
            let item = Val::plain(from_slot(kind, "item", span)?, kind.clone());
            let body = self.repr(&item, span)?;
            self.helper(format!("static const char *{name}(zeno_slot item)"), format!("    return {body};\n"));
        }
        Ok(name)
    }

    /// The same, for comparing two items.
    fn eq_adapter(&mut self, kind: &Kind, span: Span) -> Result<String, Error> {
        let name = format!("zeno_item_eq_{}", key(kind));
        if self.adapters.insert(name.clone()) {
            let (a, b) = (from_slot(kind, "a", span)?, from_slot(kind, "b", span)?);
            let body = self.equal(kind, &a, &b, span)?;
            self.helper(format!("static bool {name}(zeno_slot a, zeno_slot b)"), format!("    return {body};\n"));
        }
        Ok(name)
    }

    /// `print` in a minimal build: one `printf`, or `puts` where there is
    /// only words to show.
    ///
    /// The ordinary `print` turns each value into text and hands the
    /// texts to the runtime, and text takes memory. Here a whole number,
    /// a float, a bool and a piece of text go to `printf` as they are,
    /// and the words of an f-string are its format:
    /// `print(f"{n} is prime")` is `printf("%lld is prime\n", (long long)z_n)`.
    /// Whatever else there is to show, a list or an object, is made into
    /// text as before and shown with `%s`.
    fn print_direct(&mut self, args: &[Arg], span: Span) -> Result<Val, Error> {
        /// One piece of the line: words written in the program, or the
        /// value at this place in `vals` with the format it was given.
        enum Piece<'a> {
            Words(&'a str),
            Value(usize, Option<&'a str>),
        }
        let mut vals: Vec<Val> = Vec::new();
        let mut pieces: Vec<Piece> = Vec::new();
        // Values that are made into text, which takes room while it is
        // made: their places in `vals`.
        let mut made: Vec<usize> = Vec::new();
        let mut looked_into = false;
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                pieces.push(Piece::Words(" "));
            }
            match &arg.value.kind {
                ExprKind::Str(words) => pieces.push(Piece::Words(words)),
                ExprKind::FormatStr(parts) => {
                    for part in parts {
                        let (value, format) = match part {
                            FormatPart::Text(words) => {
                                pieces.push(Piece::Words(words));
                                continue;
                            }
                            FormatPart::Value { value, format } => (value, format.as_deref()),
                        };
                        let mark = self.lines.len();
                        let val = self.expr(value)?;
                        // What `printf` cannot show is turned into text
                        // as soon as it is worked out, before the next
                        // value runs, as `format_string` does it.
                        let (piece, format) = match printf_of(&val, format).is_some() {
                            true => (val, format),
                            false => {
                                let plain = self.text(&val, span)?;
                                let code = match format {
                                    Some(spec) => formatted(&val, spec, plain, span)?,
                                    None => plain,
                                };
                                let mut piece = Val::from(code, Kind::Str, &[&val]);
                                piece.reads |= matches!(val.kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_));
                                made.push(vals.len());
                                (piece, None)
                            }
                        };
                        self.settle(&mut vals, &piece, mark, span)?;
                        pieces.push(Piece::Value(vals.len(), format));
                        vals.push(piece);
                    }
                }
                _ => {
                    let mark = self.lines.len();
                    let val = self.expr(&arg.value)?;
                    looked_into |= matches!(val.kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_));
                    if printf_of(&val, None).is_none() {
                        made.push(vals.len());
                    }
                    self.settle(&mut vals, &val, mark, span)?;
                    pieces.push(Piece::Value(vals.len(), None));
                    vals.push(val);
                }
            }
        }
        // `print` shows its values after all of them are worked out. If
        // one is a list or an object, and another calls something that
        // could change it, finish the calls first.
        if looked_into {
            for val in &mut vals {
                if val.calls {
                    let taken = std::mem::replace(val, Val::plain("", Kind::None));
                    *val = self.temp(taken, span)?;
                }
            }
        }
        // C does not say in which order the arguments of a call are
        // worked out. Where two or more are made into text, each is made
        // on a line of its own, so that the order is the one written.
        let in_turn = made.len() > 1;
        let (mut format, mut words, mut shown) = (String::new(), String::new(), Vec::new());
        for piece in &pieces {
            let (index, spec) = match piece {
                Piece::Words(text) => {
                    let literal = c_string(text);
                    let body = &literal[1..literal.len() - 1];
                    format += &body.replace('%', "%%");
                    words += body;
                    continue;
                }
                Piece::Value(index, spec) => (*index, *spec),
            };
            let val = &vals[index];
            let (conversion, code) = match printf_of(val, spec) {
                Some(direct) => direct,
                None => ("%s".to_string(), Some(self.text(val, span)?)),
            };
            format += &conversion;
            match code {
                Some(code) if in_turn && made.contains(&index) => {
                    let name = self.new_temp();
                    self.line(format!("const char *{name} = {code};"));
                    shown.push(name);
                }
                Some(code) => shown.push(code),
                None => {}
            }
        }
        match (shown.is_empty(), words.is_empty()) {
            (true, true) => self.line("putchar('\\n');"),
            (true, false) => self.line(format!("puts(\"{words}\");")),
            (false, _) => self.line(format!("printf(\"{format}\\n\", {});", shown.join(", "))),
        }
        let parts: Vec<&Val> = vals.iter().collect();
        let mut val = Val::from(String::new(), Kind::None, &parts);
        val.pre_calls = true;
        Ok(val)
    }

    fn builtin(&mut self, name: &str, vals: Vec<Val>, span: Span) -> Result<Val, Error> {
        let parts: Vec<&Val> = vals.iter().collect();
        let one = || match vals.as_slice() {
            [val] => Ok(val),
            _ => Err(cannot(format!("`{name}` called this way"), span)),
        };
        Ok(match name {
            "print" => {
                // `print` shows its values after all of them are worked out.
                // If one is a list or an object, and another calls
                // something that could change it, finish the calls first.
                let mut vals = vals;
                if vals.iter().any(|val| matches!(val.kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_))) {
                    for val in &mut vals {
                        if val.calls {
                            let taken = std::mem::replace(val, Val::plain("", Kind::None));
                            *val = self.temp(taken, span)?;
                        }
                    }
                }
                // C does not say in which order the arguments of a call
                // are worked out. Making the text of a list, a dict or an
                // object takes room for its pieces while it is made, and
                // so can a call. So how much room is in use at most would
                // depend on the C compiler. Where two or more of the
                // values have work to do and one of them is such a one,
                // the work is done on lines of its own, in the order the
                // LLVM backend does it: every value first, then every text.
                let named = |val: &Val| {
                    val.code.starts_with('"') || val.code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                };
                let plain = |val: &Val| matches!(val.kind, Kind::Int | Kind::U64 | Kind::Float | Kind::Bool | Kind::Str);
                // A call may use the room while it runs, and text that
                // is made here is put in it.
                let roomy = |val: &Val| !plain(val) || val.calls || (val.kind == Kind::Str && !named(val));
                let works = |val: &Val| val.kind != Kind::Str || !named(val);
                let values_first = vals.iter().filter(|val| works(val)).count() > 1 && vals.iter().any(roomy);
                if values_first {
                    for val in &mut vals {
                        if !named(val) {
                            let taken = std::mem::replace(val, Val::plain("", Kind::None));
                            *val = self.temp(taken, span)?;
                        }
                    }
                }
                // The text of a number takes no room while it is made, so
                // numbers alone have no order to get wrong.
                let in_turn = vals.iter().filter(|val| val.kind != Kind::Str).count() > 1
                    && vals.iter().any(|val| !plain(val));
                let parts: Vec<&Val> = vals.iter().collect();
                let mut texts = vec![vals.len().to_string()];
                for val in &vals {
                    let text = self.text(val, span)?;
                    if in_turn && val.kind != Kind::Str {
                        let name = self.new_temp();
                        self.line(format!("const char *{name} = {text};"));
                        texts.push(name);
                    } else {
                        texts.push(text);
                    }
                }
                self.line(format!("zeno_print({});", texts.join(", ")));
                let mut val = Val::from(String::new(), Kind::None, &parts);
                val.pre_calls = true;
                val
            }
            "str" => Val::from(self.text(one()?, span)?, Kind::Str, &parts),
            // `input()`, `input("Name: ")`: the runtime reads a line, and
            // gives NULL, which is `none`, when there are no more.
            "input" if vals.len() <= 1 && vals.iter().all(|val| val.kind == Kind::Str) => {
                let question = vals.first().map_or("NULL", |val| val.code.as_str());
                let call = format!("zeno_input({question})");
                self.built = !self.collector;
                return Ok(self.called(call, &vals, Kind::Optional(Box::new(Kind::Str))));
            }
            // `range(5)` used as a value, as in `3 in range(5)` or `let r
            // = range(5)`: the list of numbers that `zeno run` makes. A
            // `for` over a range builds none (see `open_loop`).
            "range" if (1..=3).contains(&vals.len()) && vals.iter().all(|val| val.kind == Kind::Int) => {
                let (start, stop, step) = match vals.as_slice() {
                    [stop] => ("0", stop.code.as_str(), "1"),
                    [start, stop] => (start.code.as_str(), stop.code.as_str(), "1"),
                    [start, stop, step, ..] => (start.code.as_str(), stop.code.as_str(), step.code.as_str()),
                    [] => return Err(cannot("this `range`", span)),
                };
                self.uses_piece("ZENO_RANGE_LIST");
                let code = format!("zeno_range_list({start}, {stop}, {step}, {})", self.at(span));
                let mut val = Val::from(code, Kind::List(Box::new(Kind::Int)), &parts).failing_if(!self.minimal);
                val.made = true;
                val
            }
            // `char(65)`: the runtime writes the bytes of that character.
            "char" if matches!(one(), Ok(val) if val.kind == Kind::Int) => {
                Val::from(format!("zeno_char({}, {})", one()?.code, self.at(span)), Kind::Str, &parts).failing_if(!self.minimal)
            }
            // `free(p)`: back to the heap, or to the pool.
            "free" if matches!(one(), Ok(val) if matches!(val.kind, Kind::Pointer(_))) => {
                let call = format!("zeno_release({})", one()?.code);
                return Ok(self.called(call, &vals, Kind::None));
            }
            // `u8(n)`: `n` itself, once the runtime has seen that it fits.
            // The size is handed over as the words for the message.
            // A minimal build does not look: the number goes on as it is.
            _ if self.minimal && ints::width(name).is_some() && matches!(one(), Ok(val) if val.kind == Kind::Int) => {
                Val::from(one()?.code.clone(), Kind::Int, &parts)
            }
            _ if ints::width(name).is_some() && matches!(one(), Ok(val) if val.kind == Kind::Int) => {
                let width = ints::width(name).unwrap_or(ints::WIDTHS[0]);
                let article = if width.signed { "an" } else { "a" };
                let code = format!(
                    "zeno_fit({}, INT64_C({}), INT64_C({}), \"{article} {name}\", {})",
                    one()?.code,
                    width.min(),
                    width.max(),
                    self.at(span)
                );
                Val::from(code, Kind::Int, &parts).failing_if(!self.minimal)
            }
            // `u64(n)` (decision 42.4): an int below 0 is an OverflowError.
            // `u64..of` is the checker's name for `u64.wrap`, and for a
            // u64 written out: the int's 64 bits, read with no sign.
            "u64" | "u64..of" => {
                let val = one()?;
                match &val.kind {
                    Kind::U64 => val.clone(),
                    Kind::Int if name == "u64..of" || self.minimal || val.code.parse::<u64>().is_ok() => {
                        match val.code.parse::<u64>() {
                            Ok(n) => Val::plain(format!("UINT64_C({n})"), Kind::U64),
                            Err(_) => Val::from(format!("((uint64_t)({}))", val.code), Kind::U64, &parts),
                        }
                    }
                    Kind::Int => Val::from(format!("zeno_u64({}, {})", val.code, self.at(span)), Kind::U64, &parts).failing(),
                    _ => return Err(cannot("`u64` of anything but a whole number", span)),
                }
            }
            // `u64.wrap(a + b)`: C's own unsigned arithmetic, which wraps.
            "u64..add" | "u64..sub" | "u64..mul" | "u64..shl" => {
                let [a, b] = vals.as_slice() else { return Err(cannot(format!("`{name}` called this way"), span)) };
                let a = match a.kind {
                    Kind::Int => format!("((uint64_t)({}))", a.code),
                    _ => a.code.clone(),
                };
                match name {
                    "u64..shl" if self.minimal => Val::from(format!("({a} << {})", b.code), Kind::U64, &parts),
                    "u64..shl" => {
                        let code = format!("zeno_shl_wrap_u64({a}, (uint64_t)({}), {}, {})", b.code, b.kind == Kind::Int, self.at(span));
                        Val::from(code, Kind::U64, &parts).failing()
                    }
                    _ => {
                        let symbol = match name { "u64..add" => "+", "u64..sub" => "-", _ => "*" };
                        let b = match b.kind {
                            Kind::Int => format!("((uint64_t)({}))", b.code),
                            _ => b.code.clone(),
                        };
                        Val::from(format!("({a} {symbol} {b})"), Kind::U64, &parts)
                    }
                }
            }
            "len" => match one()? {
                val if val.kind == Kind::Str => Val::from(format!("zeno_len({})", val.code), Kind::Int, &parts),
                val if matches!(val.kind, Kind::List(_)) => {
                    let mut len = Val::from(format!("{}->len", val.code), Kind::Int, &parts);
                    len.reads = true;
                    len
                }
                // A dict has as many entries as its list of keys has items.
                val if matches!(val.kind, Kind::Dict(..)) => {
                    let mut len = Val::from(format!("{}->keys->len", val.code), Kind::Int, &parts);
                    len.reads = true;
                    len
                }
                _ => return Err(cannot("`len` of this", span)),
            },
            "abs" => {
                let val = one()?;
                match &val.kind {
                    Kind::Int => Val::from(format!("zeno_abs({}, {})", val.code, self.at(span)), Kind::Int, &parts).failing_if(!self.minimal),
                    Kind::Float | Kind::Quantity(_) => {
                        Val::from(format!("fabs({})", val.code), val.kind.clone(), &parts)
                    }
                    Kind::F32 => Val::from(format!("fabsf({})", val.code), Kind::F32, &parts),
                    Kind::U64 => val.clone(),
                    _ => return Err(cannot("`abs` of this", span)),
                }
            }
            "int" => {
                let val = one()?;
                match &val.kind {
                    Kind::Int => Val::from(val.code.clone(), Kind::Int, &parts),
                    Kind::U64 if self.minimal => Val::from(format!("((int64_t)({}))", val.code), Kind::Int, &parts),
                    Kind::U64 => {
                        Val::from(format!("zeno_int_u64({}, {})", val.code, self.at(span)), Kind::Int, &parts).failing()
                    }
                    Kind::Float | Kind::F32 => {
                        Val::from(format!("zeno_int({}, {})", val.code, self.at(span)), Kind::Int, &parts).failing_if(!self.minimal)
                    }
                    Kind::Str => {
                        self.built = !self.collector;
                        let call = format!("zeno_parse_int({}, {})", val.code, self.at(span));
                        if self.firmware {
                            return self.checked_call(call, Kind::Int, &["ValueError".to_string()], span);
                        }
                        Val::from(call, Kind::Int, &parts).failing()
                    }
                    _ => return Err(cannot("`int` of anything but a number or a str", span)),
                }
            }
            "float" => {
                let val = one()?;
                match &val.kind {
                    Kind::Int | Kind::Float => Val::from(val.double(), Kind::Float, &parts),
                    Kind::F32 | Kind::U64 => Val::from(format!("((double){})", val.code), Kind::Float, &parts),
                    Kind::Str => {
                        self.built = !self.collector;
                        let call = format!("zeno_parse_float({}, {})", val.code, self.at(span));
                        if self.firmware {
                            return self.checked_call(call, Kind::Float, &["ValueError".to_string()], span);
                        }
                        Val::from(call, Kind::Float, &parts).failing()
                    }
                    _ => return Err(cannot("`float` of anything but a number or a str", span)),
                }
            }
            // `f32(x)`: the nearest f32 (decision 41.5). A float written
            // out was checked to fit, and a minimal build does not look.
            "f32" => {
                let val = one()?;
                match &val.kind {
                    Kind::F32 => val.clone(),
                    Kind::Int => Val::from(val.single(), Kind::F32, &parts),
                    Kind::Float if self.minimal || val.code.parse::<f64>().is_ok() => {
                        Val::from(format!("((float){})", val.code), Kind::F32, &parts)
                    }
                    Kind::Float => {
                        Val::from(format!("zeno_f32({}, {})", val.code, self.at(span)), Kind::F32, &parts).failing()
                    }
                    _ => return Err(cannot("`f32` of anything but a number", span)),
                }
            }
            // `sum`, `min` and `max` of one list of numbers.
            "sum" | "min" | "max" if matches!(vals.as_slice(), [Val { kind: Kind::List(_), .. }]) => {
                let list = &vals[0];
                let Kind::List(item) = &list.kind else { unreachable!() };
                let code = match (name, &**item) {
                    ("sum", Kind::Byte) => format!("zeno_bytes_sum({})", list.code),
                    (_, Kind::Byte) => {
                        format!("zeno_bytes_best({}, {}, {}, {})", list.code, name == "min", c_string(name), self.at(span))
                    }
                    ("sum", Kind::Int) => format!("zeno_list_sum_int({}, {})", list.code, self.at(span)),
                    ("sum", Kind::Float) => format!("zeno_list_sum_float({})", list.code),
                    ("sum", Kind::F32) => format!("zeno_list_sum_f32({})", list.code),
                    ("sum", Kind::U64) => format!("zeno_list_sum_u64({}, {})", list.code, self.at(span)),
                    ("min" | "max", Kind::F32) => format!(
                        "((float)zeno_list_best_float({}, {}, {}, {}))",
                        list.code,
                        name == "min",
                        c_string(name),
                        self.at(span)
                    ),
                    // A quantity is kept as a float, in its own unit.
                    ("min" | "max", Kind::Quantity(_)) => {
                        format!("zeno_list_best_float({}, {}, {}, {})", list.code, name == "min", c_string(name), self.at(span))
                    }
                    (_, Kind::Int | Kind::Float | Kind::U64) => format!(
                        "zeno_list_best_{}({}, {}, {}, {})",
                        key(item),
                        list.code,
                        name == "min",
                        c_string(name),
                        self.at(span)
                    ),
                    _ => return Err(cannot(format!("`{name}` of a list of anything but ints or floats"), span)),
                };
                let mut val = Val::from(code, read_kind(item), &parts).failing_if(!self.minimal);
                val.reads = true;
                val
            }
            "min" | "max" => {
                let singles = vals.iter().any(|val| val.kind == Kind::F32)
                    && vals.iter().all(|val| matches!(val.kind, Kind::F32 | Kind::Int));
                if singles && vals.len() >= 2 {
                    let mut code = vals[0].single();
                    for val in &vals[1..] {
                        code = format!("((float)zeno_{name}_float({code}, {}))", val.single());
                    }
                    return Ok(Val::from(code, Kind::F32, &parts));
                }
                let kind = match vals.first() {
                    Some(first) if vals.len() >= 2 && vals.iter().all(|val| val.kind == first.kind) => {
                        first.kind.clone()
                    }
                    _ => return Err(cannot(format!("`{name}` of values that are not all one type"), span)),
                };
                let helper = match kind {
                    Kind::Int => format!("zeno_{name}"),
                    Kind::U64 => format!("zeno_{name}_u64"),
                    Kind::Float | Kind::Quantity(_) => format!("zeno_{name}_float"),
                    _ => return Err(cannot(format!("`{name}` of anything but numbers"), span)),
                };
                let mut code = vals[0].code.clone();
                for val in &vals[1..] {
                    code = format!("{helper}({code}, {})", val.code);
                }
                Val::from(code, kind, &parts)
            }
            _ => return Err(cannot(format!("`{name}` used this way"), span)),
        })
    }

    // ------------------------------------------------------------ f-strings

    /// `f"area = {area:.2f}"` becomes one call that joins the pieces.
    fn format_string(&mut self, parts: &[FormatPart], span: Span) -> Result<Val, Error> {
        // Each `{value}` is turned into text as soon as it is worked out,
        // before the next one runs. So the pieces of text are what must be
        // kept in order here, not just the values.
        let mut pieces: Vec<Val> = Vec::new();
        for part in parts {
            match part {
                FormatPart::Text(literal) => pieces.push(Val::plain(c_string(literal), Kind::Str)),
                FormatPart::Value { value, format } => {
                    let mark = self.lines.len();
                    let val = self.expr(value)?;
                    let plain = self.text(&val, span)?;
                    let code = match format {
                        Some(spec) => formatted(&val, spec, plain, span)?,
                        None => plain,
                    };
                    let mut piece = Val::from(code, Kind::Str, &[&val]);
                    // Showing a list or an object looks inside it, and a
                    // later call could change what is there.
                    piece.reads |= matches!(val.kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_));
                    self.settle(&mut pieces, &piece, mark, span)?;
                    pieces.push(piece);
                }
            }
        }
        let used: Vec<&Val> = pieces.iter().collect();
        let code = match pieces.as_slice() {
            [] => "\"\"".to_string(),
            [only] => only.code.clone(),
            _ => {
                let codes: Vec<&str> = pieces.iter().map(|piece| piece.code.as_str()).collect();
                format!("zeno_join({}, {})", pieces.len(), codes.join(", "))
            }
        };
        Ok(Val::from(code, Kind::Str, &used))
    }
}

/// `(a < b)` without its outer brackets, for places such as `if (...)` that
/// supply their own.
fn bare(code: &str) -> &str {
    let Some(inner) = code.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')) else {
        return code;
    };
    // Only if the first bracket is the one that closes at the very end:
    // `(a) && (b)` must be left alone.
    let mut depth = 0;
    for byte in inner.bytes() {
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return code,
            b')' => depth -= 1,
            // Brackets inside a string do not count; leave such code as it is.
            b'"' | b'\'' => return code,
            _ => {}
        }
    }
    inner
}

/// Is this expression a name or a literal: something that cannot fail, does
/// nothing, and may safely be mentioned twice?
pub(crate) fn is_plain(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Name(_)) || is_literal(expr)
}

/// Somewhere a value can be stored that has a name: `point`, `self.total`,
/// `a.b.c`. Mentioning it does nothing, so it is safe to mention again.
fn is_place(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Name(_) => true,
        ExprKind::Field { object, .. } => is_place(object),
        _ => false,
    }
}

pub fn is_literal(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::None => true,
        // A new empty list for each call, as `zeno run` makes one.
        ExprKind::List(items) => items.is_empty(),
        ExprKind::Dict(entries) => entries.is_empty(),
        ExprKind::Unit { value, .. } | ExprKind::Unary { op: UnaryOp::Neg, operand: value } => {
            matches!(value.kind, ExprKind::Int(_) | ExprKind::Float(_))
        }
        _ => false,
    }
}

/// Is this piece of C a plain name for a value, safe to mention twice?
/// `z_f`, `env->z_f`, `(*z_f)` and `(&zeno_value_f)` are; a call is not.
fn is_c_place(code: &str) -> bool {
    let inner = code
        .strip_prefix("(*")
        .or_else(|| code.strip_prefix("(&"))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(code);
    !inner.is_empty() && inner.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'>' | b'.'))
}

/// The built-in kinds of error. An object of any class can be raised too.
pub const ERROR_KINDS: &[&str] =
    &["Error", "ValueError", "IndexError", "KeyError", "ZeroDivisionError", "OverflowError", "FileError"];

/// Find the variables that are given a new value inside a `try` block.
///
/// `try` is built on C's `setjmp` and `longjmp`, and C has a rule about
/// them: after the jump back, a local variable that was changed since the
/// `setjmp` may hold either its old value or its new one, unless it was
/// declared `volatile`. (The C compiler may have been keeping the new value
/// in a processor register, and the jump puts the registers back.) So the
/// variables found here are declared `volatile`, which makes C keep them in
/// memory, where the jump cannot touch them.
///
/// Only `var` variables of the function itself matter. Objects and lists
/// live on the heap, which the jump leaves alone.
fn changed_in_try(statement: &Stmt, inside: bool, names: &mut HashSet<String>) {
    let mut each = |block: &[Stmt], inside: bool| {
        for statement in block {
            changed_in_try(statement, inside, names);
        }
    };
    match &statement.kind {
        StmtKind::Assign { target: Expr { kind: ExprKind::Name(name), .. }, .. } if inside => {
            names.insert(name.clone());
        }
        StmtKind::Receive { places, .. } if inside => names.extend(places.iter().filter_map(Place::assigned).cloned()),
        StmtKind::If { then_block, else_block, .. } => {
            each(then_block, inside);
            each(else_block.as_deref().unwrap_or_default(), inside);
        }
        StmtKind::While { body, .. } | StmtKind::For { body, .. } | StmtKind::Unsafe(body) | StmtKind::Scoped { body, .. } => {
            each(body, inside)
        }
        StmtKind::Match { arms, .. } => arms.iter().for_each(|arm| each(&arm.body, inside)),
        StmtKind::Try { body, handlers } => {
            each(body, true);
            handlers.iter().for_each(|handler| each(&handler.body, inside));
        }
        StmtKind::AssertRaises { body, .. } => each(body, true),
        _ => {}
    }
}

/// Find the names that functions written inside this statement mention.
/// `inside` is whether the walk has already entered such a function.
///
/// A `var` whose name turns up here may be changed by the inner function
/// or by the outer one, and each must see the other's change. See the
/// `boxed` field of `Variable`.
pub fn names_in_functions(statement: &Stmt, inside: bool, names: &mut HashSet<String>) {
    let block = |block: &[Stmt], inside: bool, names: &mut HashSet<String>| {
        for statement in block {
            names_in_functions(statement, inside, names);
        }
    };
    match &statement.kind {
        // More of the same block (decision 59.1).
        StmtKind::Scoped { body, .. } => block(body, inside, names),
        StmtKind::Let { value, .. } | StmtKind::Const { value, .. } => names_in_expr(value, inside, names),
        StmtKind::Assign { target, value, .. } => {
            names_in_expr(target, inside, names);
            names_in_expr(value, inside, names);
        }
        StmtKind::Receive { places, value, .. } => {
            for place in places {
                match place {
                    Place::Target(target) => names_in_expr(target, inside, names),
                    // A name declared inside a function written inside
                    // is that function's own, as for a `let`.
                    Place::Name { .. } | Place::Drop => {}
                }
            }
            names_in_expr(value, inside, names);
        }
        StmtKind::Expr(expr) | StmtKind::Raise(expr) | StmtKind::Return(Some(expr)) => names_in_expr(expr, inside, names),
        StmtKind::Function(decl) => block(&decl.body, true, names),
        StmtKind::Class(class) => class.methods.iter().for_each(|method| block(&method.body, true, names)),
        StmtKind::If { condition, then_block, else_block } => {
            names_in_expr(condition, inside, names);
            block(then_block, inside, names);
            block(else_block.as_deref().unwrap_or_default(), inside, names);
        }
        StmtKind::While { condition, body } => {
            names_in_expr(condition, inside, names);
            block(body, inside, names);
        }
        StmtKind::Unsafe(body) => block(body, inside, names),
        StmtKind::Test { .. } | StmtKind::Assert(..) | StmtKind::AssertRaises { .. } => {}
        StmtKind::For { iterable, body, .. } => {
            names_in_expr(iterable, inside, names);
            block(body, inside, names);
        }
        StmtKind::Match { subject, arms } => {
            names_in_expr(subject, inside, names);
            arms.iter().for_each(|arm| block(&arm.body, inside, names));
        }
        StmtKind::Try { body, handlers } => {
            block(body, inside, names);
            handlers.iter().for_each(|handler| block(&handler.body, inside, names));
        }
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Enum(_) | StmtKind::Import { .. } => {}
    }
}

/// The arguments of a call to a function with a name that is held as a
/// closure, in the order of its parameters: those given by name put in
/// their places, and a default for each left out (decision 56.16).
/// The functions with names that a block of a function declares, sorted
/// (decision 56.16). Both compilers use this, so that they agree.
pub(crate) struct InnerPlan {
    /// Those that use nothing of the function around them: each becomes a
    /// function of the file.
    pub plain: Vec<(Arc<Function>, Span)>,
    /// Those that do, each with the index of the statement it is made
    /// before (the number of statements for "after the last"), in an
    /// order where each comes after the others it calls.
    pub closures: Vec<(usize, Arc<Function>, Span)>,
}

/// Sort the functions with names of a block. `around` holds the names of
/// the variables of the function around the block, further out. One uses
/// something if it names one of those, or a `let`, a `const` or an
/// `import` of the block, or calls one that does. One that does is made
/// once, after the last of those declarations it uses and after each
/// closure beside it that it calls; two that call each other cannot both
/// come first, and are refused.
pub(crate) fn inner_plan(statements: &[&Stmt], around: &HashSet<String>, build: &str) -> Result<InnerPlan, Error> {
    let cannot = |what: String, span: Span| cannot_in(build, &what, span);
    let mut found: Vec<(&Arc<Function>, Span)> = Vec::new();
    let mut declared: HashMap<&str, usize> = HashMap::new();
    for (index, statement) in statements.iter().enumerate() {
        match &statement.kind {
            StmtKind::Function(decl) if !decl.type_params.is_empty() => {
                return Err(cannot("a generic function inside a function or block".to_string(), statement.span));
            }
            StmtKind::Function(decl) => found.push((decl, statement.span)),
            StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => {
                declared.entry(name).or_insert(index);
            }
            StmtKind::Import { names, .. } => {
                for name in names {
                    declared.entry(name).or_insert(index);
                }
            }
            _ => {}
        }
    }
    if found.is_empty() {
        return Ok(InnerPlan { plain: Vec::new(), closures: Vec::new() });
    }
    let free: Vec<HashSet<String>> = found.iter().map(|(decl, _)| free_names(decl)).collect();
    let mut closure: Vec<bool> = free
        .iter()
        .map(|names| names.iter().any(|name| around.contains(name) || declared.contains_key(name.as_str())))
        .collect();
    // One that calls a closure is one too.
    loop {
        let before = closure.clone();
        for (i, names) in free.iter().enumerate() {
            if !closure[i] {
                closure[i] = found.iter().enumerate().any(|(j, (decl, _))| before[j] && j != i && names.contains(&decl.name));
            }
        }
        if closure == before {
            break;
        }
    }
    let mut plain = Vec::new();
    for (i, (decl, span)) in found.iter().enumerate() {
        if closure[i] {
            continue;
        }
        if around.contains(&decl.name) {
            return Err(cannot(format!("a function inside a function with the name of a variable around it, `{}`,", decl.name), *span));
        }
        plain.push(((*decl).clone(), *span));
    }
    struct Places<'a> {
        build: &'a str,
        found: &'a [(&'a Arc<Function>, Span)],
        free: &'a [HashSet<String>],
        closure: &'a [bool],
        declared: &'a HashMap<&'a str, usize>,
        at: HashMap<usize, usize>,
        order: Vec<usize>,
        visiting: HashSet<usize>,
    }
    impl Places<'_> {
        fn place(&mut self, i: usize) -> Result<usize, Error> {
            if let Some(&known) = self.at.get(&i) {
                return Ok(known);
            }
            let (decl, span) = self.found[i];
            if !self.visiting.insert(i) {
                return Err(Error::new(
                    format!(
                        "{} cannot compile two functions inside a function that call each other and use variables \
                         of the function around them, such as `{}`, yet. Give one of them to the other as a \
                         parameter. `zeno run` can still run this program",
                        self.build, decl.name
                    ),
                    span,
                ));
            }
            let mut after =
                self.free[i].iter().filter_map(|name| self.declared.get(name.as_str())).map(|index| index + 1).max().unwrap_or(0);
            for j in 0..self.found.len() {
                if j != i && self.closure[j] && self.free[i].contains(&self.found[j].0.name) {
                    after = after.max(self.place(j)?);
                }
            }
            self.visiting.remove(&i);
            self.at.insert(i, after);
            self.order.push(i);
            Ok(after)
        }
    }
    let mut places = Places {
        build,
        found: &found,
        free: &free,
        closure: &closure,
        declared: &declared,
        at: HashMap::new(),
        order: Vec::new(),
        visiting: HashSet::new(),
    };
    for i in 0..found.len() {
        if closure[i] {
            places.place(i)?;
        }
    }
    let closures = places.order.iter().map(|&i| (places.at[&i], found[i].0.clone(), found[i].1)).collect();
    Ok(InnerPlan { plain, closures })
}

pub(crate) fn in_order(decl: &Function, args: &[Arg], span: Span, build: &str) -> Result<Vec<Arg>, Error> {
    let cannot = |what: &str, span: Span| cannot_in(build, what, span);
    let mut placed: Vec<Option<Expr>> = vec![None; decl.params.len()];
    for (index, arg) in args.iter().enumerate() {
        let place = match &arg.name {
            None => index,
            Some(name) => match decl.params.iter().position(|param| &param.name == name) {
                Some(place) => place,
                None => return Err(cannot(&format!("the argument `{name}`"), span)),
            },
        };
        match placed.get_mut(place) {
            Some(slot) => *slot = Some(arg.value.clone()),
            None => return Err(cannot("this many arguments", span)),
        }
    }
    let mut ordered = Vec::new();
    for (slot, param) in placed.into_iter().zip(&decl.params) {
        let value = match (slot, &param.default) {
            (Some(value), _) => value,
            (None, Some(default)) if is_literal(default) => (**default).clone(),
            (None, Some(_)) => {
                return Err(cannot(
                    "a default that is not a plain literal, for a function inside a function that uses variables around it,",
                    span,
                ));
            }
            (None, None) => return Err(cannot("this many arguments", span)),
        };
        ordered.push(Arg { name: None, value });
    }
    Ok(ordered)
}

/// The names a function uses that it does not declare itself: in its
/// body, in its defaults, and in the functions written inside it. Those
/// are what it needs from around it (decision 56.16). A name it declares
/// anywhere counts as its own everywhere, which may only make it seem to
/// need less; the checker has made sure that it never uses a variable of
/// its own and one from around it under the same name in a way that
/// matters here.
pub fn free_names(decl: &Function) -> HashSet<String> {
    let mut used = HashSet::new();
    for param in &decl.params {
        if let Some(default) = &param.default {
            names_in_expr(default, true, &mut used);
        }
    }
    for statement in &decl.body {
        names_in_functions(statement, true, &mut used);
    }
    let mut own: HashSet<String> = decl.params.iter().map(|param| param.name.clone()).collect();
    declared_in(&decl.body, &mut own);
    used.retain(|name| !own.contains(name) && *name != decl.name);
    used
}

/// Every name the statements declare, in blocks inside them too.
fn declared_in(statements: &[Stmt], names: &mut HashSet<String>) {
    fn pattern(written: &Pattern, names: &mut HashSet<String>) {
        match written {
            Pattern::Name(name) => {
                names.insert(name.clone());
            }
            Pattern::Variant { fields, .. } => fields.iter().for_each(|field| pattern(field, names)),
            Pattern::Wildcard | Pattern::Literal(_) => {}
        }
    }
    for statement in statements {
        let mut exprs: Vec<&Expr> = Vec::new();
        match &statement.kind {
            StmtKind::Scoped { body, .. } => declared_in(body, names),
            StmtKind::Let { name, value, .. } | StmtKind::Const { name, value, .. } => {
                names.insert(name.clone());
                exprs.push(value);
            }
            StmtKind::Receive { places, value, .. } => {
                for place in places {
                    match place {
                        Place::Name { name, .. } => {
                            names.insert(name.clone());
                        }
                        Place::Target(target) => exprs.push(target),
                        Place::Drop => {}
                    }
                }
                exprs.push(value);
            }
            StmtKind::Import { module, names: imported } => {
                names.extend(imported.iter().cloned());
                if imported.is_empty() {
                    names.extend(module.last().cloned());
                }
            }
            StmtKind::Function(decl) => {
                names.insert(decl.name.clone());
                names.extend(decl.params.iter().map(|param| param.name.clone()));
                exprs.extend(decl.params.iter().filter_map(|param| param.default.as_deref()));
                declared_in(&decl.body, names);
            }
            StmtKind::Class(class) => {
                names.insert(class.name.clone());
                for method in &class.methods {
                    names.extend(method.params.iter().map(|param| param.name.clone()));
                    declared_in(&method.body, names);
                }
            }
            StmtKind::Enum(decl) => {
                names.insert(decl.name.clone());
            }
            StmtKind::For { variable, second, iterable, body } => {
                names.insert(variable.clone());
                names.extend(second.iter().cloned());
                exprs.push(iterable);
                declared_in(body, names);
            }
            StmtKind::If { condition, then_block, else_block } => {
                exprs.push(condition);
                declared_in(then_block, names);
                declared_in(else_block.as_deref().unwrap_or_default(), names);
            }
            StmtKind::While { condition, body } => {
                exprs.push(condition);
                declared_in(body, names);
            }
            StmtKind::Unsafe(body) | StmtKind::Test { body, .. } | StmtKind::AssertRaises { body, .. } => declared_in(body, names),
            StmtKind::Match { subject, arms } => {
                exprs.push(subject);
                for arm in arms {
                    pattern(&arm.pattern, names);
                    declared_in(&arm.body, names);
                }
            }
            StmtKind::Try { body, handlers } => {
                declared_in(body, names);
                for handler in handlers {
                    names.extend(handler.binding.iter().cloned());
                    declared_in(&handler.body, names);
                }
            }
            StmtKind::Assign { target, value, .. } => exprs.extend([target, value]),
            StmtKind::Expr(expr) | StmtKind::Raise(expr) | StmtKind::Return(Some(expr)) | StmtKind::Assert(expr, None) => exprs.push(expr),
            StmtKind::Assert(expr, Some(message)) => exprs.extend([expr, message]),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
        for expr in exprs {
            declared_in_expr(expr, names);
        }
    }
}

/// The names declared inside an expression: the variable of a
/// comprehension, and the parameters and names of a function written
/// where a value goes.
fn declared_in_expr(expr: &Expr, names: &mut HashSet<String>) {
    let mut each = |expr: &Expr| declared_in_expr(expr, names);
    match &expr.kind {
        ExprKind::Name(_) | ExprKind::Give(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
        ExprKind::Unit { value: inner, .. }
        | ExprKind::Unary { operand: inner, .. }
        | ExprKind::Field { object: inner, .. }
        | ExprKind::Take(inner) => each(inner),
        ExprKind::Binary { left, right, .. } | ExprKind::Index { object: left, index: right } => {
            each(left);
            each(right);
        }
        ExprKind::Compare { first, rest } => {
            each(first);
            rest.iter().for_each(|(_, expr)| each(expr));
        }
        ExprKind::FormatStr(parts) => {
            for part in parts {
                if let FormatPart::Value { value, .. } = part {
                    each(value);
                }
            }
        }
        ExprKind::Call { callee, args } => {
            each(callee);
            args.iter().for_each(|arg| each(&arg.value));
        }
        ExprKind::List(items) | ExprKind::Several(items) => items.iter().for_each(each),
        ExprKind::Dict(entries) => {
            for (key, value) in entries {
                each(key);
                each(value);
            }
        }
        ExprKind::ListComp { element, variable, iterable, condition } => {
            names.insert(variable.clone());
            declared_in_expr(element, names);
            declared_in_expr(iterable, names);
            if let Some(condition) = condition {
                declared_in_expr(condition, names);
            }
        }
        ExprKind::Function(decl) => {
            names.extend(decl.params.iter().map(|param| param.name.clone()));
            for param in &decl.params {
                if let Some(default) = &param.default {
                    declared_in_expr(default, names);
                }
            }
            declared_in(&decl.body, names);
        }
    }
}

fn names_in_expr(expr: &Expr, inside: bool, names: &mut HashSet<String>) {
    let mut each = |expr: &Expr| names_in_expr(expr, inside, names);
    match &expr.kind {
        ExprKind::Name(name) | ExprKind::Give(name) if inside => {
            names.insert(name.clone());
        }
        ExprKind::Name(_) | ExprKind::Give(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
        ExprKind::Unit { value: inner, .. }
        | ExprKind::Unary { operand: inner, .. }
        | ExprKind::Field { object: inner, .. }
        | ExprKind::Take(inner) => each(inner),
        ExprKind::Binary { left, right, .. } | ExprKind::Index { object: left, index: right } => {
            each(left);
            each(right);
        }
        ExprKind::Compare { first, rest } => {
            each(first);
            rest.iter().for_each(|(_, expr)| each(expr));
        }
        ExprKind::FormatStr(parts) => {
            for part in parts {
                if let FormatPart::Value { value, .. } = part {
                    each(value);
                }
            }
        }
        ExprKind::Call { callee, args } => {
            each(callee);
            args.iter().for_each(|arg| each(&arg.value));
        }
        ExprKind::List(items) | ExprKind::Several(items) => items.iter().for_each(each),
        ExprKind::Dict(entries) => {
            for (key, value) in entries {
                each(key);
                each(value);
            }
        }
        ExprKind::ListComp { element, iterable, condition, .. } => {
            each(element);
            each(iterable);
            if let Some(condition) = condition {
                each(condition);
            }
        }
        ExprKind::Function(decl) => {
            for statement in &decl.body {
                names_in_functions(statement, true, names);
            }
        }
    }
}

/// The numbers of the `math` module: `math.pi` and `math.e`. A compiled
/// program has them written in it, to the last digit a float holds.
pub(crate) fn math_constant(name: &str) -> Option<f64> {
    match name {
        "pi" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        _ => None,
    }
}

/// C for a quantity after it has been scaled to another unit.
fn scaled(code: &str, scale: units::Scale) -> String {
    if scale.times != 1.0 {
        format!("({code} * {:?})", scale.times)
    } else if scale.over != 1.0 {
        format!("({code} / {:?})", scale.over)
    } else {
        code.to_string()
    }
}

/// What `*` or `/` makes of two quantities, or of a number over one
/// (decision 43): its kind, and each side scaled to the unit of its own
/// kind with no prefix. `left` is `None` for a plain number.
fn made(op: &str, left: Option<(&str, &str)>, right: (&str, &str), span: Span) -> Result<(Kind, String, String), Error> {
    let refused = || Error::new(format!("units do not make a unit with `{op}` here"), span);
    let made = units::made(op, left.map(|(_, unit)| unit), right.1).ok_or_else(refused)?;
    let l = match left {
        Some((code, unit)) => scaled(code, units::to_plain(unit).ok_or_else(refused)?),
        None => String::new(),
    };
    let r = scaled(right.0, units::to_plain(right.1).ok_or_else(refused)?);
    let kind = match made {
        units::Made::Unit(unit) => Kind::Quantity(unit.to_string()),
        units::Made::Number => Kind::Float,
    };
    Ok((kind, l, r))
}

/// Two quantities, both scaled to the unit they share.
fn in_common(left: &Val, a: &str, right: &Val, b: &str, span: Span) -> Result<(String, String, String), Error> {
    match units::common(a, b) {
        Some((unit, l, r)) => Ok((unit.to_string(), scaled(&left.code, l), scaled(&right.code, r))),
        None => Err(cannot(format!("a quantity in {a} with a quantity in {b}"), span)),
    }
}

/// `== != < <= > >=` as a C expression.
fn comparison(op: BinaryOp, left: &Val, right: &Val, span: Span) -> Result<String, Error> {
    let symbol = op.text();
    Ok(match (&left.kind, &right.kind) {
        // C compares strings by where they are in memory. `strcmp` compares
        // what they say.
        (Kind::Str, Kind::Str) => format!("(strcmp({}, {}) {symbol} 0)", left.code, right.code),
        (Kind::Int, Kind::Int) | (Kind::Float, Kind::Float) | (Kind::U64, Kind::U64) => {
            format!("({} {symbol} {})", left.code, right.code)
        }
        // An f32 with an int: the int becomes the nearest f32, as in a sum.
        (Kind::F32, Kind::F32 | Kind::Int) | (Kind::Int, Kind::F32) => {
            format!("({} {symbol} {})", left.single(), right.single())
        }
        (Kind::Quantity(a), Kind::Quantity(b)) => {
            let (_, l, r) = in_common(left, a, right, b, span)?;
            format!("({l} {symbol} {r})")
        }
        (Kind::Bool, Kind::Bool) if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) => {
            format!("({} {symbol} {})", left.code, right.code)
        }
        _ if left.is_number() && right.is_number() => format!("({} {symbol} {})", left.double(), right.double()),
        _ => return Err(cannot(format!("`{symbol}` between these two types"), span)),
    })
}

/// The format after the colon in `{x:>8.2f}`, taken apart: line up to the
/// right, eight wide, two decimals, as a float.
pub struct Format {
    /// `<`, `>` or `^`, if one was written.
    pub align: Option<char>,
    /// Fill with zeros instead of spaces.
    pub zero: bool,
    /// How wide to make it. 0 means as wide as it is.
    pub width: usize,
    /// The digits after the `.`, if any.
    pub precision: Option<usize>,
    /// The letter at the end: `f`, `d`, `x`, `X` or `b`.
    pub kind: Option<char>,
}

/// Read a format. `None` means it is not one Zeno understands.
///
/// It is read here, while compiling, so the compiled program does not
/// have to read it again every time the line runs. The rules are the
/// ones `format` in `builtins.rs` follows. The LLVM backend reads formats
/// with this too.
pub fn read_format(spec: &str) -> Option<Format> {
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
        return None;
    }
    Some(Format { align, zero, width, precision, kind })
}

/// A statement of a minimal build as a person would write it:
/// `x = (x + 1);` is `x += 1;`, and `return (a * b);` is `return a * b;`.
/// The emitter puts every operation in brackets, so that what is around
/// it cannot change what it means; around a whole statement there is
/// nothing left that could.
fn tidy(line: &str) -> String {
    let Some(statement) = line.strip_suffix(';') else {
        return line.to_string();
    };
    let indent = &line[..line.len() - line.trim_start().len()];
    let statement = statement.trim_start();
    if let Some(value) = statement.strip_prefix("return ") {
        return format!("{indent}return {};", bare(value));
    }
    // `to = value`, where `to` is a name, a field, or a new variable.
    let Some((to, value)) = statement.split_once(" = ") else {
        return line.to_string();
    };
    let place = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ' ' | '*'));
    if !place(&to.replace("->", ".")) {
        return line.to_string();
    }
    let value = bare(value);
    // `x = x + 1`, where the brackets taken off were around all of it.
    for symbol in ["+", "-", "*", "/", "&", "|", "^"] {
        let rest = value.strip_prefix(to).and_then(|rest| rest.strip_prefix(' '));
        let Some(by) = rest.and_then(|rest| rest.strip_prefix(symbol)).and_then(|rest| rest.strip_prefix(' ')) else {
            continue;
        };
        if !to.contains(' ') && atom(by) == by {
            return format!("{indent}{to} {symbol}= {};", bare(by));
        }
    }
    format!("{indent}{to} = {value};")
}

/// C that a cast or a `?` can be written next to: a name, a call, or
/// something in brackets already. Anything else is put in brackets.
fn atom(code: &str) -> String {
    let named = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'));
    let bracketed = |text: &str| bare(text).len() != text.len();
    let call = code.find('(').is_some_and(|open| named(&code[..open]) && bracketed(&code[open..]));
    match named(&code.replace("->", ".")) || bracketed(code) || call {
        true => code.to_string(),
        false => format!("({code})"),
    }
}

/// How C's `printf` shows a value by itself, in a minimal build: what to
/// write in its format, and the argument that goes with it, if any.
/// `None` where `printf` has no way of its own, and the value has to be
/// made into text first: a list, an object, a format `printf` lacks.
///
/// Each answer prints what the runtime's text of the value would be. So
/// the formats that differ are left to the runtime: text is padded there
/// by its characters and here by its bytes, and `printf` does not fill a
/// float that is not a number with zeros.
fn printf_of(val: &Val, spec: Option<&str>) -> Option<(String, Option<String>)> {
    if val.kind == Kind::F32 {
        if spec.is_none() {
            return Some(("%s".to_string(), Some(format!("zeno_f32_text({}, (char[64]){{0}})", val.code))));
        }
        let wide = Val { code: format!("((double){})", val.code), kind: Kind::Float, ..val.clone() };
        return printf_of(&wide, spec);
    }
    let whole = || Some(format!("(long long){}", atom(&val.code)));
    let float_text = |name: &str| Some(format!("{name}({}, (char[64]){{0}})", val.code));
    let unit_of = |unit: &str| {
        let literal = c_string(unit);
        literal[1..literal.len() - 1].replace('%', "%%")
    };
    let Some(spec) = spec else {
        return Some(match &val.kind {
            Kind::Str => ("%s".to_string(), Some(val.code.clone())),
            Kind::Int => ("%lld".to_string(), whole()),
            Kind::U64 => ("%llu".to_string(), Some(format!("(unsigned long long){}", atom(&val.code)))),
            Kind::Float => ("%s".to_string(), float_text("zeno_float_text")),
            Kind::Bool => ("%s".to_string(), Some(format!("{} ? \"true\" : \"false\"", atom(&val.code)))),
            Kind::Quantity(unit) => (format!("%s {}", unit_of(unit)), float_text("zeno_quantity_text")),
            Kind::None => ("none".to_string(), None),
            _ => return None,
        });
    };
    let Format { align, zero, width, precision, kind } = read_format(spec)?;
    // `{n:5}` is `%5lld`, `{n:<5}` is `%-5lld`, `{n:05}` is `%05lld`.
    let wide = |zeros: bool| match (width, align, zero) {
        (0, _, _) => Some(String::new()),
        (_, _, true) if zeros => Some(format!("0{width}")),
        (_, Some('^'), _) | (_, _, true) => None,
        (_, Some('<'), false) => Some(format!("-{width}")),
        (_, _, false) => Some(width.to_string()),
    };
    let decimals = precision.unwrap_or(6);
    match (kind, precision, &val.kind) {
        (Some('f'), _, Kind::Quantity(unit)) | (None, Some(_), Kind::Quantity(unit)) if width == 0 => {
            Some((format!("%.{decimals}f {}", unit_of(unit)), Some(val.code.clone())))
        }
        (Some('f'), _, Kind::Int | Kind::Float) | (None, Some(_), Kind::Float) => {
            Some((format!("%{}.{decimals}f", wide(false)?), Some(val.double())))
        }
        (None | Some('d'), None, Kind::Int) => Some((format!("%{}lld", wide(true)?), whole())),
        (Some('f'), _, Kind::U64) => Some((format!("%{}.{decimals}f", wide(false)?), Some(val.double()))),
        // A u64 has no sign, so C's own `%llx` writes it as Zeno does.
        (None | Some('d' | 'x' | 'X'), None, Kind::U64) => {
            let letter = match kind { Some('x') => "x", Some('X') => "X", _ => "u" };
            Some((format!("%{}ll{letter}", wide(true)?), Some(format!("(unsigned long long){}", atom(&val.code)))))
        }
        // `{n:x}`, `{reg:08b}`: the runtime writes the digits, and the
        // zeros in front of them, into bytes of the statement's own.
        (Some(kind @ ('x' | 'X' | 'b')), None, Kind::Int) if width <= 64 => {
            let (zeros, wide) = match zero {
                true => (width, String::new()),
                false => (0, wide(false)?),
            };
            Some((format!("%{wide}s"), Some(format!("zeno_radix_text({}, '{kind}', {zeros}, (char[72]){{0}})", val.code))))
        }
        (None, None, Kind::Str) if width == 0 => Some(("%s".to_string(), Some(val.code.clone()))),
        _ => None,
    }
}

/// A value with a format after the colon, as in `{x:.2f}` or `{n:08b}`.
fn formatted(val: &Val, spec: &str, plain: String, span: Span) -> Result<String, Error> {
    // `{gain:.3f}`: the decimals of the float an f32 is exactly.
    if val.kind == Kind::F32 {
        let wide = Val { code: format!("((double){})", val.code), kind: Kind::Float, ..val.clone() };
        return formatted(&wide, spec, plain, span);
    }
    let bad = || Error::new(format!("`{spec}` is not a format Zeno understands"), span);
    let Format { align, zero, width, precision, kind } = read_format(spec).ok_or_else(bad)?;

    // A quantity is a number with its unit, and lines up as numbers do.
    let is_number = val.is_number() || matches!(val.kind, Kind::Quantity(_) | Kind::U64);
    let body = match (kind, precision, &val.kind) {
        // `{angle:.1f}` is `30.5 deg`: the decimals are the number's,
        // and the unit follows it as it always does.
        (Some('f'), _, Kind::Quantity(unit)) | (None, Some(_), Kind::Quantity(unit)) => {
            format!("zeno_fixed_quantity({}, {}, {})", val.code, precision.unwrap_or(6), c_string(unit))
        }
        (Some('f'), _, _) | (None, Some(_), Kind::Float) if is_number => {
            format!("zeno_fixed({}, {})", val.double(), precision.unwrap_or(6))
        }
        (None, None, _) | (Some('d'), None, Kind::Int | Kind::U64) => plain,
        (Some(kind @ ('x' | 'X' | 'b')), None, Kind::Int) => format!("zeno_radix({}, '{kind}')", val.code),
        (Some(kind @ ('x' | 'X' | 'b')), None, Kind::U64) => format!("zeno_radix_u64({}, '{kind}')", val.code),
        _ => return Err(bad()),
    };
    if width == 0 {
        return Ok(body);
    }
    // Numbers line up on the right by default, everything else on the left.
    let align = align.unwrap_or(if is_number { '>' } else { '<' });
    Ok(format!("zeno_pad({body}, {width}, '{align}', {})", zero && is_number))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    fn compile(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_with_types(&program, true);
        if let Some(error) = errors.first() {
            let line = source.lines().nth(error.span.line.saturating_sub(1)).unwrap_or_default();
            panic!("the test program has mistakes: {:?}\nline {}: {line}", error.message, error.span.line);
        }
        emit(&program, &types, "test.zeno", Build::Ordinary).map_err(|error| error.message)
    }

    /// The same, as `zeno build --firmware` compiles it.
    fn compile_firmware(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, Build::Firmware.rules());
        // A firmware build's check refuses a few programs that `zeno run`
        // takes (5.19m), and says so, as `zeno build --firmware` does.
        if let Some(error) = errors.first() {
            return Err(error.message.clone());
        }
        emit(&program, &types, "test.zeno", Build::Firmware).map_err(|error| error.message)
    }

    /// Build the C with the machine's C compiler, run it, and return what it
    /// printed, what it wrote as errors, and whether it finished cleanly.
    /// `None` if there is no C compiler here to try it with.
    fn build_and_run(name: &str, c: &str, flags: &[&str]) -> Option<(String, String, bool)> {
        let ran = built_output(name, c, flags)?;
        Some((
            String::from_utf8_lossy(&ran.stdout).into_owned(),
            String::from_utf8_lossy(&ran.stderr).into_owned(),
            ran.status.success(),
        ))
    }

    /// The same, with everything the run gave: the number it ended with
    /// among the rest.
    fn built_output(name: &str, c: &str, flags: &[&str]) -> Option<std::process::Output> {
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let (c_file, binary) = (dir.join("test.c"), dir.join(program_file("test")));
        std::fs::write(&c_file, c).expect("the C file is written");
        let Ok(built) = Command::new("cc").args(flags).arg("-o").arg(&binary).arg(&c_file).args(LIBRARIES).output() else {
            eprintln!("no C compiler found; skipping the compiled half of this test");
            return None;
        };
        let complaints = String::from_utf8_lossy(&built.stderr);
        let complaints: String = complaints.chars().take(1500).collect();
        assert!(built.status.success(), "the C does not compile:\n{complaints}");
        let ran = crate::value::testing::run(&mut Command::new(&binary)).expect("the program runs");
        let _ = std::fs::remove_dir_all(&dir);
        Some(ran)
    }

    /// The heart of these tests: the compiled program must print exactly
    /// what the interpreter prints.
    fn same_as_interpreter(name: &str, source: &str) {
        let program = crate::modules::testing::program(source);
        let (interpreted, result) = crate::vm::run_both(&program, source);

        // Both kinds of build must agree with the interpreter: the
        // ordinary one, and `--firmware`, where errors travel as values.
        for firmware in [false, true] {
            let c = if firmware {
                match compile_firmware(source) {
                    Ok(c) => c,
                    // A firmware build refuses a few things on purpose,
                    // and says so. Any other refusal is a failure.
                    Err(message) => {
                        assert!(message.contains("--firmware"), "the firmware build failed: {message}\n{source}");
                        continue;
                    }
                }
            } else {
                compile(source).expect("the test program compiles")
            };
            let name = if firmware { format!("{name}-firmware") } else { name.to_string() };
            // ZENO_COLLECT_ALWAYS runs the garbage collector before every
            // single allocation. If the collector ever frees something that
            // is still in use, this is the setting most likely to show it.
            let Some((compiled, errors, finished)) = build_and_run(&name, &c, &["-DZENO_COLLECT_ALWAYS"]) else {
                return;
            };
            assert_eq!(compiled, interpreted, "\nfirmware: {firmware}\n{source}");
            match &result {
                Ok(()) => assert!(finished, "the compiled program failed (firmware: {firmware}): {errors}"),
                // Both must stop with the same error, at the same place.
                Err(error) => {
                    assert!(!finished, "the compiled program should have failed");
                    let place = crate::modules::testing::place(&program, error.span);
                    let expected = format!("error: {}\n --> {place}\n", error.message);
                    assert_eq!(errors, expected, "\nfirmware: {firmware}\n{source}");
                }
            }
        }
        // The LLVM backend is held to every program the C backend is.
        crate::emit_llvm::tests::same_as_interpreter(&format!("{name}-llvm"), source);
    }

    #[test]
    fn the_smallest_and_largest_of_quantities() {
        same_as_interpreter(
            "min-max-quantities",
            "let readings = [1.5 V, 3 V, 2 V]\nprint(max(readings))\nprint(min(readings))\n\
             print(max(2 s, 3 s))\nprint(min(4 ms, 2 ms, 3 ms))\n\
             let low: V = min(readings)\nprint(low + 1 V)\n\
             var total = 0 V\nfor reading in readings {\n    total += reading\n}\nprint(total)\n",
        );
    }

    #[test]
    fn arithmetic_matches() {
        same_as_interpreter(
            "arithmetic",
            "print(1 + 2 * 3, 7 // 2, -7 // 2, 7 % -3, -7 % 3, 2 ** 10, 7 / 2, 2 ** 0.5)\n\
             print(1.5 + 1, 3 * 0.5, 7.5 // 2, -7.5 % 2, 10 - 2.5, -(3 + 4), 0 ** 0, (-1) ** 7)\n\
             print(1 == 1.0, 2 < 3.5, \"a\" < \"b\", \"a\" == \"a\", not true, true and false, true or false)\n\
             print(abs(-3), abs(-2.5), min(3, 1, 2), max(1.5, 2.5), int(3.9), int(-3.9), float(2))",
        );
    }

    #[test]
    fn floats_print_the_same() {
        same_as_interpreter(
            "floats",
            "print(0.1 + 0.2, 100.0, 1e16, 1.5e-7, 0.0001, 123456.789, 1e15, 5e-324, 1.7976931348623157e308)\n\
             print(3.0, -0.5, 1 / 3, 2.0 ** 70, 12345678912345678.0, 0.1 * 3, -0.0)",
        );
    }

    #[test]
    fn strings_and_formats_match() {
        same_as_interpreter(
            "strings",
            "let name = \"Zeno\"\nlet pi = 3.14159\n\
             print(f\"Hello, {name}!\", name + \"!\", len(\"h\u{e9}llo\"), str(42) + str(1.5) + str(true))\n\
             print(f\"{3:5}|{pi:.2f}|{255:x}|{255:X}|{5:08b}|{name:>6}|{name:^8}|{2.5:8.1f}|{7:<4}|{-3:05}|{pi:10}|\")\n\
             print(f\"{2:.3f} {name:2} {-255:x} {1.0} {none_yet()}\")\n\
             function none_yet() -> str { return \"tab\\there \\\"quoted\\\" ??= done\" }",
        );
    }

    #[test]
    fn control_flow_matches() {
        same_as_interpreter(
            "control",
            "var total = 0\nfor i in range(10) {\n if i % 2 == 0 { continue }\n if i > 7 { break }\n total += i\n}\nprint(total)\n\
             for i in range(10, 0, -3) { print(i) }\nfor i in range(2, 5) { print(i) }\nfor i in range(5, 2) { print(i) }\n\
             var n = 27\nvar steps = 0\nwhile n != 1 {\n if n % 2 == 0 { n = n // 2 } else { n = 3 * n + 1 }\n steps += 1\n}\nprint(steps)\n\
             let x = 5\nif x < 3 { print(\"small\") } else if x < 10 { print(\"medium\") } else { print(\"big\") }\n\
             print(0 <= x < 10, 0 < x < 3, 1 < 2 < 3 < 4)\n\
             if x > 0 {\n let x = x + 1\n print(x)\n}\nprint(x)",
        );
    }

    #[test]
    fn functions_match() {
        same_as_interpreter(
            "functions",
            "let base = 10\n\
             function add(a, b) { return a + b }\n\
             function fact(n) {\n if n <= 1 { return 1 }\n return n * fact(n - 1)\n}\n\
             function greet(who: str, excited: bool = false) -> str {\n if excited { return f\"Hello, {who}!\" }\n return f\"Hello, {who}\"\n}\n\
             function area(w, h) { return w * h }\n\
             function show(label: str) { print(label, base) }\n\
             function average(a, b) {\n if a == b { return float(a) }\n return (a + b) / 2\n}\n\
             function wait(d: ms) -> ms { return d * 2 }\n\
             print(add(1, 2), fact(10), greet(\"Zeno\"), greet(excited=true, who=\"you\"), area(2.5, 4.0))\n\
             show(\"base is\")\nprint(average(2, 2), average(1, 2), wait(250 ms), 3 s / 2, 1.5 V + 1 V)",
        );
    }

    #[test]
    fn units_convert_the_same_way() {
        same_as_interpreter(
            "units",
            "let t = 3 s\nlet d = 250 ms\n\
             print(t + d, d + t, t - d, t.ms, d.s, (90 min).h, (1 kWh).J, t.ms.us)\n\
             print(1 s == 1000 ms, 1 s != 1000 ms, t > d, 999 ms < 1 s, 1 h / 1 min, 2 * t + 0.5 * d)\n\
             function wait(d: ms) -> ms { return d }\nprint(wait(t.ms), wait((2 min).ms))",
        );
    }

    #[test]
    fn classes_match() {
        same_as_interpreter(
            "classes",
            "import math\n\
             class Point {\n x: float\n y: float\n\
              function length(self) -> float { return math.sqrt(self.x * self.x + self.y * self.y) }\n\
              function scaled(self, by: float) -> Point { return Point(self.x * by, self.y * by) }\n\
              function move(self, dx: float) { self.x += dx }\n}\n\
             class Counter {\n count = 0\n name: str = \"c\"\n\
              function bump(self) -> int {\n  self.count += 1\n  return self.count\n }\n}\n\
             class Line {\n start: Point\n end: Point\n}\n\
             let p = Point(3.0, 4.0)\nlet q = Point(y=4.0, x=3.0)\n\
             print(p, p.length(), p == q, p != q, p.scaled(2.0), p.x + q.y)\n\
             p.move(1.5)\nprint(p, p == q, Line(p, q), Line(p, q).end.y)\n\
             let c = Counter()\nlet d = c\n\
             print(c.bump(), d.bump(), c.count, c, Counter(5, \"fi\\\"ve\"))\n\
             c.count = 10\nprint(d.count, c.bump() + c.bump(), c.bump() - d.count, c)\n\
             print(f\"{c} {c.bump()} {c}\")\nprint(c, c.bump(), c)",
        );
    }

    #[test]
    fn lists_match() {
        same_as_interpreter(
            "lists",
            "let xs = [3, 1, 2]\nvar ys: list[int] = []\n\
             for x in xs { ys.append(x * 10) }\n\
             print(xs, ys, len(xs), xs[0] + ys[2], xs + ys, xs == [3, 1, 2], xs != ys, 2 in xs, 5 not in xs)\n\
             ys[1] = 99\nprint(ys, ys.pop(), ys)\nprint(f\"{ys} {ys.pop()} {ys}\")\n\
             let names = [\"a\", \"b c\"]\nlet grid = [[1, 2], [3]]\nlet halves = [0.5, 1.0]\n\
             print(names, grid, halves, [true, false], [250 ms, 500 ms], grid[1][0], \"b c\" in names, \"b\" in \"abc\")\n\
             let squares = [n * n for n in range(6) if n % 2 == 0]\n\
             let doubled = [x * 2 for x in xs]\nprint(squares, doubled, [len(name) for name in names])\n\
             var total = 0\nfor x in xs {\n xs.append(x)\n total += x\n}\nprint(total, xs)\n\
             function first(items: list[str]) -> str { return items[0] }\n\
             function build(n: int) -> list[int] {\n var out: list[int] = []\n for i in range(n) { out.append(i) }\n return out\n}\n\
             print(first(names), build(4), build(0), len(build(3)))",
        );
        same_as_interpreter("index", "let xs = [1, 2]\nlet i = 2\nprint(xs[i])");
        same_as_interpreter("negative-index", "let xs = [1, 2]\nlet i = -1\nxs[i] = 5");
        same_as_interpreter("pop", "var xs: list[int] = []\nprint(xs.pop())");
    }

    #[test]
    fn enums_and_match_match() {
        same_as_interpreter(
            "enums",
            "enum Shape {\n Circle(radius: float)\n Rect(width: float, height: float)\n Dot\n}\n\
             enum Light {\n Red\n Green\n}\n\
             function area(shape: Shape) -> float {\n match shape {\n\
               case Circle(r) { return 3.0 * r * r }\n\
               case Rect(w, h) { return w * h }\n\
               case Dot { return 0.0 }\n }\n}\n\
             function next(light: Light) -> Light {\n match light {\n  case Red { return Green }\n  case Green { return Red }\n }\n}\n\
             function name(n: int) -> str {\n match n {\n  case 0 { return \"zero\" }\n  case 1 { return \"one\" }\n  case other { return f\"many ({other})\" }\n }\n}\n\
             let shapes = [Circle(1.0), Rect(2.0, 3.0), Dot, Shape.Rect(1.0, 1.0), Shape.Dot]\n\
             for shape in shapes { print(shape, area(shape)) }\n\
             print(shapes, Circle(1.0) == Circle(1.0), Circle(1.0) == Dot, Dot != Dot, next(Red), next(next(Red)) == Red)\n\
             print(name(0), name(1), name(7))\n\
             match shapes[1] {\n case Rect(w, 3.0) { print(\"three high\", w) }\n case _ { print(\"no\") }\n}\n\
             match \"b\" {\n case \"a\" { print(1) }\n case \"b\" { print(2) }\n case _ { print(3) }\n}",
        );
    }

    #[test]
    fn things_happen_in_the_order_zeno_says() {
        // C does not promise an order for the pieces of `a() + b()` or of
        // `f(a(), b())`. Zeno does: left to right.
        same_as_interpreter(
            "order",
            "var count = 0\n\
             function next(label: int) -> int {\n print(label)\n count += 1\n return count\n}\n\
             function three(a: int, b: int, c: int) -> int { return a * 100 + b * 10 + c }\n\
             print(next(1) - next(2))\n\
             print(three(next(3), next(4), next(5)))\n\
             print(count + next(6), count)\n\
             print(f\"{next(7)} {next(8)}\")\n\
             print(next(9) > 100 and next(-1) > 0, next(10) > 0 or next(-1) > 0)\n\
             print(0 < next(11) < next(12) < 5, 100 < next(13) < next(-1))\n\
             var i = 0\nwhile next(14) < 20 and i < 2 { i += 1 }\n\
             if count > 100 { print(\"no\") } else if next(15) > 0 and next(16) > 0 { print(\"yes\") }",
        );
    }

    #[test]
    fn a_function_does_not_see_what_is_declared_below_it() {
        // `len` inside `size` is the built-in. The variable of the same
        // name further down the file is not in sight from there.
        same_as_interpreter(
            "below",
            "function size(items: list[int]) -> int { return len(items) }\n\
             print(size([1, 2]))\nlet len = 5\nprint(size([1, 2, 3]), len)",
        );
    }

    #[test]
    fn errors_stop_the_program_the_same_way() {
        same_as_interpreter("overflow", "var n = 9223372036854775807\nprint(\"before\")\nn += 1\nprint(n)");
        same_as_interpreter("zero", "let z = 0\nprint(1 // z)");
        same_as_interpreter("zero-float", "let z = 0.0\nprint(1 / z)");
        same_as_interpreter("power", "let p = -1\nprint(2 ** p)");
        same_as_interpreter("big-power", "print(3 ** 50)");
        same_as_interpreter("root", "import math\nprint(math.sqrt(-4.0), math.pi)");
        same_as_interpreter("step", "let s = 0\nfor i in range(1, 5, s) { print(i) }");
        // Which of two errors is reported is also a matter of order.
        same_as_interpreter("first-error", "let z = 0\nlet big = 9223372036854775807\nprint((1 // z) + (big * 2))");
    }

    /// The same program built with `--release`.
    fn compile_release(source: &str) -> String {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_with_types(&program, true);
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        emit_with(&program, &types, "test.zeno", Build::Ordinary, Options { release: true, grows: false, fast: false, ..Default::default() })
            .expect("it builds")
    }

    #[test]
    fn with_release_whole_numbers_wrap_round_and_nothing_else_changes() {
        // An overflow wraps round, as the processor does, instead of
        // stopping the program.
        let source = "var n = 9223372036854775807\nn += 1\nprint(n)\nprint(n - 1)\nprint(-n)\nlet k = 4611686018427387904\nprint(k * 4)";
        let c = compile_release(source);
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        assert!(!program.contains("zeno_add(") && !program.contains("zeno_sub(") && !program.contains("zeno_mul("), "{program}");
        let Some((printed, errors, finished)) = build_and_run("release-wraps", &c, &["-O2"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, "-9223372036854775808\n9223372036854775807\n-9223372036854775808\n0\n");
        // What does not overflow comes out as in every other build, and
        // the checks that are not about overflow stay: `//` by zero, an
        // index past the end.
        let source = include_str!("../examples/primes.zeno");
        let Some((printed, _, _)) = build_and_run("release-primes", &compile_release(source), &["-O2"]) else { return };
        assert_eq!(printed, interpreted(source));
        for (name, source, error) in [
            ("release-zero", "let z = 0\nprint(1 // z)", "ZeroDivisionError"),
            ("release-index", "let xs = [1, 2]\nlet i = 2\nprint(xs[i])", "IndexError"),
        ] {
            let Some((_, errors, finished)) = build_and_run(name, &compile_release(source), &["-O2"]) else { return };
            assert!(!finished && errors.contains(error), "{errors}");
        }
    }

    /// The same program built with `--minimal`, for one kind of build.
    /// `grows` is `--owned`: no collector, and memory from the system.
    fn compile_minimal(source: &str, build: Build, grows: bool) -> String {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, build.rules());
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        emit_with(&program, &types, "test.zeno", build, Options { grows, minimal: true, ..Default::default() }).expect("it builds")
    }

    /// The kinds of build `--minimal` goes with: the collector, `--owned`,
    /// `--no-collector` and `--firmware`.
    const MINIMAL_KINDS: [(&str, Build, bool); 4] = [
        ("minimal", Build::Ordinary, false),
        ("minimal-owned", Build::NoCollector, true),
        ("minimal-pool", Build::NoCollector, false),
        ("minimal-firmware", Build::Firmware, false),
    ];

    /// A folder with a header and a file of C in the shape of a vendor's
    /// HAL, for the tests of `import c` (decision 44.10).
    fn hal_folder() -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("zeno-hal-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("a temporary folder");
        std::fs::write(
            folder.join("hal.h"),
            "#include <stdint.h>\n\
             #include <stddef.h>\n\
             typedef struct { volatile uint32_t ODR; } GPIO_TypeDef;\n\
             typedef enum { HAL_OK = 0, HAL_ERROR = 1 } HAL_StatusTypeDef;\n\
             typedef enum { GPIO_PIN_RESET = 0, GPIO_PIN_SET } GPIO_PinState;\n\
             extern GPIO_TypeDef gpio_a;\n\
             #define GPIOA (&gpio_a)\n\
             #define GPIO_PIN_5 ((uint16_t)0x0020)\n\
             #define HAL_VERSION \"1.2.3\"\n\
             #define TICK_HZ 1000.0f\n\
             void HAL_GPIO_WritePin(GPIO_TypeDef *port, uint16_t pin, GPIO_PinState state);\n\
             GPIO_PinState HAL_GPIO_ReadPin(GPIO_TypeDef *port, uint16_t pin);\n\
             HAL_StatusTypeDef HAL_Checksum(const uint8_t *data, uint16_t size, uint16_t *crc);\n\
             void HAL_Scale(float *values, size_t count, float by);\n\
             int8_t HAL_Same8(int8_t x);\n\
             uint64_t HAL_Biggest(void);\n\
             extern uint32_t uptime_ms;\n",
        )
        .expect("the header is written");
        std::fs::write(
            folder.join("hal.c"),
            "#include \"hal.h\"\n\
             GPIO_TypeDef gpio_a;\n\
             uint32_t uptime_ms = 42;\n\
             void HAL_GPIO_WritePin(GPIO_TypeDef *port, uint16_t pin, GPIO_PinState state) {\n\
                 if (state == GPIO_PIN_SET) port->ODR |= pin; else port->ODR &= ~(uint32_t)pin;\n\
             }\n\
             GPIO_PinState HAL_GPIO_ReadPin(GPIO_TypeDef *port, uint16_t pin) { return (port->ODR & pin) ? GPIO_PIN_SET : GPIO_PIN_RESET; }\n\
             HAL_StatusTypeDef HAL_Checksum(const uint8_t *data, uint16_t size, uint16_t *crc) {\n\
                 uint16_t sum = 0xFFFF;\n\
                 for (uint16_t i = 0; i < size; i++) { sum ^= data[i]; for (int b = 0; b < 8; b++) sum = (sum & 1) ? (sum >> 1) ^ 0xA001 : sum >> 1; }\n\
                 *crc = sum;\n\
                 return size ? HAL_OK : HAL_ERROR;\n\
             }\n\
             void HAL_Scale(float *values, size_t count, float by) { for (size_t i = 0; i < count; i++) values[i] *= by; }\n\
             int8_t HAL_Same8(int8_t x) { return x; }\n\
             uint64_t HAL_Biggest(void) { return UINT64_MAX; }\n",
        )
        .expect("the file of C is written");
        folder
    }

    const HAL_PROGRAM: &str = r##"import c

let hal = c.library("{folder}/hal.c", [
    "#include \"{folder}/hal.h\"",
    "void HAL_GPIO_WritePin(GPIO_TypeDef *port, uint16_t pin, GPIO_PinState state);",
    "GPIO_PinState HAL_GPIO_ReadPin(GPIO_TypeDef *port, uint16_t pin);",
    "HAL_StatusTypeDef HAL_Checksum(const uint8_t *data, uint16_t size, uint16_t *crc);",
    "void HAL_Scale(float *values, size_t count, float by);",
    "int8_t HAL_Same8(int8_t x);",
    "uint64_t HAL_Biggest(void);",
    "GPIO_TypeDef *GPIOA;",
    "uint16_t GPIO_PIN_5;",
    "GPIO_PinState GPIO_PIN_SET;",
    "GPIO_PinState GPIO_PIN_RESET;",
    "HAL_StatusTypeDef HAL_OK;",
    "const char *HAL_VERSION;",
    "float TICK_HZ;",
    "extern uint32_t uptime_ms;",
])

function lit(pin: int) -> bool {
    return hal.HAL_GPIO_ReadPin(hal.GPIOA, pin) == hal.GPIO_PIN_SET
}

print("HAL", hal.HAL_VERSION, hal.TICK_HZ, hal.uptime_ms, hal.HAL_Biggest(), hal.HAL_Same8(-3))
hal.HAL_GPIO_WritePin(hal.GPIOA, hal.GPIO_PIN_5, hal.GPIO_PIN_SET)
print("pin 5:", lit(hal.GPIO_PIN_5))
hal.HAL_GPIO_WritePin(hal.GPIOA, hal.GPIO_PIN_5, hal.GPIO_PIN_RESET)
print("pin 5:", lit(hal.GPIO_PIN_5))
let frame: list[u8] = [0x01, 0x03, 0x00, 0x6B, 0x00, 0x03]
let words = [1, 3, 0, 107, 0, 3]
let crc = [0]
print(hal.HAL_Checksum(frame, len(frame), crc) == hal.HAL_OK, crc, hal.HAL_Checksum(words, len(words), crc), crc)
let readings = [1.5, 2.0, 4.0]
hal.HAL_Scale(readings, len(readings), 2.0)
print(readings)
"##;

    #[test]
    fn a_library_of_c_is_called_directly_in_every_build() {
        if crate::bridge_c::helper().is_err() {
            return;
        }
        let folder = hal_folder();
        let source = HAL_PROGRAM.replace("{folder}", &folder.to_string_lossy().replace('\\', "/"));
        let program = crate::modules::testing::program(&source);
        let (interpreted, result) = crate::vm::run_both(&program, &source);
        assert!(result.is_ok(), "zeno run failed: {result:?}");
        assert_eq!(
            interpreted,
            "HAL 1.2.3 1000.0 42 18446744073709551615 -3\n\
             pin 5: true\n\
             pin 5: false\n\
             true [6004] 0 [6004]\n\
             [3.0, 4.0, 8.0]\n"
        );
        let builds = [("plain", Build::Ordinary, false, false), ("firmware", Build::Firmware, false, false), ("owned", Build::NoCollector, true, false)];
        let minimal = MINIMAL_KINDS.iter().map(|(name, build, grows)| (*name, *build, *grows, true));
        for (name, build, grows, minimal) in builds.into_iter().chain(minimal) {
            let program = crate::modules::testing::built(&source);
            let (errors, types) = crate::checker::check_by(&program, build.rules());
            assert!(errors.is_empty(), "{name}: {:?}", errors[0].message);
            let c = emit_with(&program, &types, "test.zeno", build, Options { grows, minimal, ..Default::default() }).expect("it builds");
            let args = crate::bridge_c::build_args(&types.c_libraries, &folder).expect("the library is there");
            let flags: Vec<&str> = args.iter().map(String::as_str).collect();
            let Some((printed, errors, finished)) = build_and_run(&format!("hal-{name}"), &c, &flags) else { return };
            assert!(finished, "{name}: {errors}");
            assert_eq!(printed, interpreted, "{name}");
        }
    }

    #[test]
    fn a_block_of_c_is_part_of_the_c_of_every_build() {
        if crate::bridge_c::helper().is_err() {
            return;
        }
        let source = "import c\n\
             \n\
             let lib = inline c {\n    \
                 #include <stdint.h>\n\n    \
                 static const uint16_t STEP = 0xA001;\n\n    \
                 uint16_t crc16(const uint8_t *data, size_t length) {\n        \
                     uint16_t crc = 0xFFFF;\n        \
                     for (size_t i = 0; i < length; i++) {\n            \
                         crc ^= data[i];\n            \
                         for (int b = 0; b < 8; b++) { crc = (crc & 1) ? (crc >> 1) ^ STEP : crc >> 1; }\n        \
                     }\n        \
                     return crc;\n    \
                 }\n\n    \
                 void scale(float *values, size_t count, float by) { for (size_t i = 0; i < count; i++) { values[i] *= by; } }\n    \
                 const char *closing(void) { return \"}\"; }\n\
             }\n\
             \n\
             function checksum(frame: list[u8]) -> int {\n    \
                 let more = inline c {\n        int plus(int a, int b) { return a + b; }\n    }\n    \
                 return more.plus(lib.crc16(frame, len(frame)), 1)\n\
             }\n\
             let frame: list[u8] = [0x01, 0x03, 0x00, 0x6B, 0x00, 0x03]\n\
             let readings = [1.5, 2.0, 4.0]\n\
             lib.scale(readings, len(readings), 2.0)\n\
             print(checksum(frame), readings, lib.closing(), lib)\n";
        let program = crate::modules::testing::program(source);
        let (interpreted, result) = crate::vm::run_both(&program, source);
        assert!(result.is_ok(), "zeno run failed: {result:?}");
        assert_eq!(interpreted, "6005 [3.0, 4.0, 8.0] } <C library inline c>\n");
        let builds = [("plain", Build::Ordinary, false, false), ("firmware", Build::Firmware, false, false), ("owned", Build::NoCollector, true, false)];
        let minimal = MINIMAL_KINDS.iter().map(|(name, build, grows)| (*name, *build, *grows, true));
        for (name, build, grows, minimal) in builds.into_iter().chain(minimal) {
            let program = crate::modules::testing::built(source);
            let (errors, types) = crate::checker::check_by(&program, build.rules());
            assert!(errors.is_empty(), "{name}: {:?}", errors[0].message);
            let c = emit_with(&program, &types, "test.zeno", build, Options { grows, minimal, ..Default::default() }).expect("it builds");
            // The block is in the C as it was written, between a line
            // that says where it was written and one that says where
            // the C goes on, which is the line that one is on, plus one.
            assert!(c.contains("#line 4 \"test.zeno\"\n#include <stdint.h>\n\nstatic const uint16_t STEP = 0xA001;\n"), "{name}: {c}");
            assert!(c.contains("\n#line 23 \"test.zeno\"\nint plus(int a, int b) { return a + b; }\n#line "), "{name}");
            for (at, line) in c.lines().enumerate() {
                if let Some(rest) = line.strip_prefix("#line ").and_then(|rest| rest.strip_suffix(" \"test.c\"")) {
                    assert_eq!(rest, (at + 2).to_string(), "{name}: the C goes on at its own line");
                }
            }
            assert_eq!(c.matches("\"test.c\"").count(), 2, "{name}");
            let Some((printed, errors, finished)) = build_and_run(&format!("block-{name}"), &c, &[]) else { return };
            assert!(finished, "{name}: {errors}");
            assert_eq!(printed, interpreted, "{name}");
        }
        // A program with no block has no `#line` in it.
        let plain = crate::modules::testing::built("print(1)\n");
        let (_, types) = crate::checker::check_by(&plain, Build::Ordinary.rules());
        assert!(!emit(&plain, &types, "test.zeno", Build::Ordinary).expect("it builds").contains("#line"));
    }

    #[test]
    fn a_number_too_big_for_c_stops_a_build_as_it_stops_zeno_run() {
        if crate::bridge_c::helper().is_err() {
            return;
        }
        let folder = hal_folder();
        let source = HAL_PROGRAM.replace("{folder}", &folder.to_string_lossy().replace('\\', "/")).lines().take(19).collect::<Vec<_>>().join("\n")
            + "\nprint(\"before\")\nhal.HAL_Same8(300)\n";
        let program = crate::modules::testing::program(&source);
        let (interpreted, result) = crate::vm::run_both(&program, &source);
        let said = "the 1st parameter of `HAL_Same8` (`int8_t`) holds -128 to 127, and this is 300";
        assert_eq!((interpreted.as_str(), result.map_err(|error| error.message)), ("before\n", Err(format!("uncaught OverflowError: {said}"))));
        let program = crate::modules::testing::built(&source);
        let (errors, types) = crate::checker::check_by(&program, Build::Ordinary.rules());
        assert!(errors.is_empty(), "{:?}", errors[0].message);
        let c = emit(&program, &types, "test.zeno", Build::Ordinary).expect("it builds");
        let args = crate::bridge_c::build_args(&types.c_libraries, &folder).expect("the library is there");
        let flags: Vec<&str> = args.iter().map(String::as_str).collect();
        let Some((printed, errors, finished)) = build_and_run("hal-too-big", &c, &flags) else { return };
        assert_eq!(printed, "before\n");
        assert!(!finished && errors.contains(said) && errors.contains("OverflowError"), "{errors}");
    }

    #[test]
    fn a_minimal_build_of_hello_is_one_call_of_puts() {
        let c = compile_minimal("print(\"Hello, Zeno!\")\n", Build::Ordinary, false);
        assert!(c.contains("int main(void) {\n    puts(\"Hello, Zeno!\");\n    return 0;\n}\n"), "{c}");
        // Nothing of the runtime is in it but what tells Windows to
        // leave the ends of lines alone.
        let runtime: Vec<&str> = c.match_indices("zeno_").map(|(at, _)| c[at..].split('(').next().unwrap_or_default()).collect();
        assert_eq!(runtime, ["zeno_plain_line_ends"], "{c}");
        assert!(c.lines().count() < 30, "{c}");
        let Some((printed, _, finished)) = build_and_run("minimal-hello", &c, &["-Os"]) else { return };
        assert!(finished);
        assert_eq!(printed, "Hello, Zeno!\n");
    }

    #[test]
    fn a_minimal_build_has_no_check_in_it_and_no_place_in_the_source() {
        let source = r#"function middle(xs: list[int]) -> int {
    return xs[len(xs) // 2] + 1
}
var xs = [3, 1, 4, 1, 5]
xs[0] = xs[1] * 7 - 2
let ages = ["Ada": 36]
print(middle(xs), 7.0 / 2.0, math.sqrt(2.0), ages["Ada"], u8(200), -xs[2], abs(xs[3]), "abc"[1], 9 % 4, 9 // 4, 1 << 3)
"#;
        for (name, build, grows) in MINIMAL_KINDS {
            let c = compile_minimal(&format!("import math\n{source}"), build, grows);
            // The index is C's own, and so are the arithmetic and the division.
            assert!(c.contains("return z2_xs->items[zeno_halved(z2_xs->len, 1)].i + 1;"), "{name}: {c}");
            assert!(c.contains("z_xs->items[0] = (zeno_slot){.i = "), "{name}: {c}");
            assert!(c.contains("(7.0 / 2.0)") && c.contains("sqrt(2.0)"), "{name}: {c}");
            for gone in [
                "zeno_fail", "zeno_bug", "zeno_overflow", "zeno_by_zero", "zeno_list_at", "zeno_div", "zeno_sqrt", "zeno_fit",
                "zeno_add(", "zeno_sub(", "zeno_mul(", "IndexError", "ZeroDivisionError", "OverflowError", "KeyError", "int line", "line, col",
                "zeno_raised_line", "ZENO_MINIMAL", "ZENO_UNCHECKED", NO_PLACE,
            ] {
                assert!(!c.contains(gone), "{name}: `{gone}` is in the minimal C");
            }
            let Some((printed, errors, finished)) =
                build_and_run(&format!("{name}-bare"), &c, &["-Os", "-Wall", "-Werror=implicit-function-declaration"])
            else {
                return;
            };
            assert!(finished, "{name}: {errors}");
            assert_eq!(printed, interpreted(&format!("import math\n{source}")), "{name}");
        }
    }

    #[test]
    fn a_minimal_build_prints_what_zeno_run_prints_for_a_correct_program() {
        // Lists, dicts, objects, text, errors that are raised and caught,
        // quantities, functions as values: a correct program cannot tell
        // a minimal build from any other.
        let source = r#"class Reading {
    name: str
    values: list[float]

    function mean(self) -> float {
        var total = 0.0
        for value in self.values {
            total += value
        }
        return total / len(self.values)
    }
}

function parsed(text: str) -> int {
    try {
        return int(text)
    } except ValueError as error {
        print("not a number:", error.message)
        return -1
    }
}

function checked(n: int) -> int raises ValueError {
    if n < 0 {
        raise ValueError(f"{n} is below zero")
    }
    return n * 2
}

let reading = Reading("inlet", [20.5, 21.25, 19.0])
print(reading.name, reading.mean(), reading)
var counts: dict[str, int] = [:]
for word in "the cat and the hat and the bat".split(" ") {
    counts[word] = counts.get(word, 0) + 1
}
print(counts, counts["the"], len(counts))
var squares: list[int] = []
for i in range(10) {
    squares.append(i * i)
}
squares[3] = -squares[3]
print(squares, sum(squares), max(squares), squares.slice(2, 5))
print(parsed("42"), parsed("4x2"), parsed("-7"))
try {
    print(checked(4))
    print(checked(-4))
} except ValueError as error {
    print("caught:", error.message)
}
let wait = 250 ms
print(wait, wait * 4, 1.5 s, f"{wait} and {wait * 2.5:.1f}")
print(17 % 5, -17 % 5, 17 // 5, -17 // 5, 2 ** 10, 7 / 2, 1 << 40, -9 >> 1)
print(true, false, none, 1 == 1, "a" < "b", not true)
let text = "Zeno, நன்றி"
print(text.upper(), len(text), text[0], text.slice(6), text.find("no"), text.replace("Zeno", "Elea"))
print()
"#;
        let expected = interpreted(source);
        for (name, build, grows) in MINIMAL_KINDS {
            let c = compile_minimal(source, build, grows);
            let Some((printed, errors, finished)) = build_and_run(&format!("{name}-whole"), &c, &["-Os"]) else { return };
            assert!(finished, "{name}: {errors}");
            assert_eq!(printed, expected, "{name}");
        }
    }

    #[test]
    fn print_in_a_minimal_build_is_printf_wherever_printf_prints_the_same() {
        let source = r#"let n = 42
let low = -42
let x = 3.14159
let name = "Ada"
let on = true
let big = 9223372036854775807
print(n, low, x, name, on, big)
print(f"{n} {low} {x} {name} {on} 100% of {big}")
print(f"[{n:5}] [{n:<5}] [{n:05}] [{low:05}] [{low:>6}] [{n:^7}] [{n:d}]")
print(f"[{x:.2f}] [{x:8.3f}] [{x:<8.1f}] [{x:08.2f}] [{n:.1f}] [{low:.0f}] [{x:^9.2f}]")
print(f"[{n:x}] [{n:X}] [{n:b}] [{n:08b}] [{low:x}] [{low:08b}] [{n:6x}] [{n:<6x}] [{255:04X}]")
print(f"[{name:6}] [{name:>6}] [{name:^7}] [{'நன்றி':>7}] [{on}] [{not on:>6}]")
print(0.1 + 0.2, 1.0, 1e21, 1.5e-7, -0.0, 100.0 / 3.0, 2.0 ** 70)
print(f"{0.1 + 0.2} {1e100} {1.0 / 3.0:.3f}")
print(500 ms, 1.5 s, 2 V, f"{500 ms} {1.25 s:.1f} {3 V * 2}")
print("50%", "a\tb", "q?", "\"quoted\"", 'back\\slash')
print("only", "words")
print()
print([1, 2], ["a": 1], f"{[n, low]} {name}")
"#;
        let c = compile_minimal(source, Build::Ordinary, false);
        for written in [
            "printf(\"%lld %lld %s %s %s %lld\\n\", (long long)z_n, (long long)z_low, zeno_float_text(z_x, (char[64]){0}), z_name, z_on ? \"true\" : \"false\", (long long)z_big);",
            "printf(\"%lld %lld %s %s %s 100%% of %lld\\n\"",
            "[%5lld] [%-5lld] [%05lld] [%05lld] [%6lld] [%s] [%lld]",
            "[%.2f] [%8.3f] [%-8.1f] [%s] [%.1f] [%.0f] [%s]",
            "zeno_radix_text(z_n, 'b', 8, (char[72]){0})",
            "[%6s] [%-6s]",
            "puts(\"only words\");",
            "putchar('\\n');",
            "printf(\"%s ms %s s %s V ",
        ] {
            assert!(c.contains(written), "`{written}` is not in the C:\n{}", &c[c.find("int main").unwrap_or(0)..]);
        }
        // Text is lined up by its characters, which `printf` cannot do:
        // that is left to the runtime.
        assert!(c.contains("zeno_pad(z_name, 6, '<', false)"), "{c}");
        let Some((printed, errors, finished)) = build_and_run("minimal-printf", &c, &["-Os", "-Wall", "-Wextra"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, interpreted(source));
    }

    #[test]
    fn a_minimal_build_keeps_the_errors_a_correct_program_can_meet() {
        // Text that is not a number is not a mistake in the program: it
        // comes from outside. So that error is still raised, and still
        // stops the program when nothing catches it, with what it is
        // and without where.
        let source = "function half(text: str) -> int raises ValueError {\n    return int(text) // 2\n}\nprint(half(\"10\"))\nprint(half(\"ten\"))\n";
        for (name, build, grows) in MINIMAL_KINDS {
            let c = compile_minimal(source, build, grows);
            let Some((printed, errors, finished)) = build_and_run(&format!("{name}-error"), &c, &["-Os"]) else { return };
            assert!(!finished, "{name}");
            assert_eq!(printed, "5\n", "{name}");
            assert_eq!(errors, "error: uncaught ValueError: cannot turn \"ten\" into an int\n", "{name}");
        }
    }

    #[test]
    fn the_whole_runtime_of_a_minimal_build_is_c_for_every_kind_of_build() {
        // A program uses a part of it. Every part must still be C once
        // the checks and the places are taken out, so all of it is
        // given to the C compiler here, for each kind of build.
        for (name, firmware, collector, grows) in
            [("collector", false, true, false), ("firmware", true, true, false), ("pool", true, false, false), ("owned", true, false, true)]
        {
            for report in [false, true] {
                let runtime = crate::minimal::runtime(&runtime_of(firmware, collector, grows, true), report);
                // (The note at the top of the runtime speaks of the marks.)
                let marks = runtime.lines().filter(|line| line.starts_with('#') && line.contains("ZENO_MINIMAL")).count();
                assert_eq!(marks, 0, "{name}: a mark is left in the runtime of a minimal build");
                for gone in [", int line, int col", "(int line, int col", "line, col)"] {
                    assert!(!runtime.contains(gone), "{name}: `{gone}` is in the runtime of a minimal build");
                }
                let roots = "static void zeno_roots(void) {}\n";
                let c = format!("{runtime}\n{}int main(void) {{\n    return 0;\n}}\n", if collector { roots } else { "" });
                let flags = ["-Wall", "-Wno-unused-function", "-Wno-unused-variable", "-Wno-unused-const-variable", "-Werror"];
                let Some((_, _, finished)) = build_and_run(&format!("minimal-runtime-{name}-{report}"), &c, &flags) else { return };
                assert!(finished, "{name}");
            }
        }
    }

    #[test]
    fn a_statement_of_a_minimal_build_is_written_the_short_way() {
        assert_eq!(tidy("    z_d = (z_d + 1);"), "    z_d += 1;");
        assert_eq!(tidy("    z_self->z_total = (z_self->z_total - (z_a * z_b));"), "    z_self->z_total -= z_a * z_b;");
        assert_eq!(tidy("    z_x = (z_x * zeno_mod(z_a, 3));"), "    z_x *= zeno_mod(z_a, 3);");
        assert_eq!(tidy("    int64_t z_n = (z_a + z_b);"), "    int64_t z_n = z_a + z_b;");
        assert_eq!(tidy("    return ((double)z_a / (double)z_b);"), "    return (double)z_a / (double)z_b;");
        assert_eq!(tidy("    z_x = (z_y + z_x);"), "    z_x = z_y + z_x;");
        // What is not all in one pair of brackets is left as it is.
        assert_eq!(tidy("    z_x = (z_a) + (z_b);"), "    z_x = (z_a) + (z_b);");
        assert_eq!(tidy("    z_x = (z_x + z_a) * (z_b);"), "    z_x = (z_x + z_a) * (z_b);");
        for same in ["    if (z_a == z_b) {", "    printf(\"a = (b);\\n\");", "    for (int64_t i = 0; i < n; i++) {", "    z_f(z_a);", "}"] {
            assert_eq!(tidy(same), same);
        }
        // What a cast can be written next to.
        assert_eq!(atom("z_n"), "z_n");
        assert_eq!(atom("z_p->z_x"), "z_p->z_x");
        assert_eq!(atom("(z_a + z_b)"), "(z_a + z_b)");
        assert_eq!(atom("zeno_mod(z_a, z_b)"), "zeno_mod(z_a, z_b)");
        assert_eq!(atom("(z_a) + (z_b)"), "((z_a) + (z_b))");
        assert_eq!(atom("z_f(1) + z_g(2)"), "(z_f(1) + z_g(2))");
    }

    /// The same program built with `--fast`.
    fn compile_fast(source: &str, grows: bool) -> String {
        let program = crate::modules::testing::built(source);
        let build = if grows { Build::NoCollector } else { Build::Ordinary };
        let (errors, types) = crate::checker::check_by(&program, build.rules());
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        emit_with(&program, &types, "test.zeno", build, Options { release: true, grows, fast: true, ..Default::default() }).expect("it builds")
    }

    #[test]
    fn a_fast_build_prints_what_zeno_run_prints_for_a_correct_program() {
        // A correct program cannot tell `--fast` from any other build.
        // With a collector, and owned with a growing pool, where a list
        // big enough to take its items from the system is grown in place.
        let source = r#"var xs: list[int] = []
var seed = 7
for k in range(20000) {
    seed = (seed * 1103515245 + 12345) % 2147483648
    xs.append(seed % 1000)
}
var total = 0
for i in range(len(xs)) {
    total += xs[i] * (i % 3)
}
xs[0] = -5
var parts: list[str] = []
for i in range(-12, 12) {
    parts.append(str(i * 987654321))
}
print(total, xs[0], xs[19999], parts.join(","))
print(str(9223372036854775807), str(-9223372036854775807 - 1), str(0), str(-7))
"#;
        let expected = interpreted(source);
        for (name, grows) in [("fast", false), ("fast-owned", true)] {
            let c = compile_fast(source, grows);
            assert!(c.starts_with("#define ZENO_UNCHECKED 1\n"), "{}", &c[..80]);
            let flags = ["-O3", "-fno-math-errno", "-DZENO_MEMORY_REPORT"];
            let Some((printed, errors, finished)) = build_and_run(name, &c, &flags) else { return };
            assert!(finished, "{errors}");
            assert_eq!(printed, expected);
            // Owned, every block is given back, the grown rows too.
            assert!(!grows || errors.contains(", 0 blocks left at the end"), "{errors}");
        }
    }

    #[test]
    fn a_fast_build_does_whole_number_sums_as_c_does() {
        // `+`, `-` and `*` are C's own on signed numbers, so the C
        // compiler knows what it knows of a C program: they do not
        // overflow (decision 23.6). `--release` keeps the sums that wrap.
        let source = "function sum(n: int) -> int {\n    var total = 0\n    var i = 0\n    while i < n {\n        total += (i * i) % 7 - i\n        i += 1\n    }\n    return -total\n}\nprint(sum(1000), sum(0), 3 - 5 * 4)";
        let body = |c: &str| c.split("static int64_t z_sum(int64_t z_n) {\n").nth(1).expect("the function").split("\n}\n").next().unwrap_or_default().to_string();
        let fast = body(&compile_fast(source, false));
        assert!(fast.contains("z_i = (z_i + 1);") && fast.contains("(z_i * z_i)") && fast.contains("return (-(z_total));"), "{fast}");
        assert!(!fast.contains("uint64_t") && !fast.contains("zeno_add("), "{fast}");
        let release = body(&compile_release(source));
        assert!(release.contains("z_i = ((int64_t)((uint64_t)(z_i) + (uint64_t)(1)));"), "{release}");
        let Some((printed, errors, finished)) = build_and_run("fast-sums", &compile_fast(source, false), &["-O3"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, interpreted(source));
    }

    #[test]
    fn dividing_by_a_power_of_two_is_a_shift_and_a_mask_in_every_build() {
        // `x // 8` rounds down and `x % 8` is never below zero, for an
        // `x` below zero too: exactly what a shift that keeps the sign
        // and the low bits give. Neither can fail, so neither is checked
        // (decision 23.5). Any other number goes the usual way.
        let source = r#"let big = 9223372036854775807
let small = -big - 1
for x in [-17, -16, -9, -8, -7, -1, 0, 1, 7, 8, 9, 16, 17, big, small] {
    print(x // 1, x % 1, x // 2, x % 2, x // 8, x % 8, x // 4611686018427387904, x % 4611686018427387904)
    print(x // 3, x % 3, x // -8, x % -8)
}
"#;
        let c = compile(source).expect("it compiles");
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        assert!(program.contains("zeno_halved(z_x, 3)") && program.contains("(z_x & INT64_C(7))"), "{program}");
        assert!(program.contains("zeno_halved(z_x, 62)") && program.contains("(z_x & INT64_C(4611686018427387903))"), "{program}");
        assert!(program.contains("zeno_floordiv(z_x, 3,") && program.contains("zeno_mod(z_x, 3,"), "{program}");
        assert!(program.contains("zeno_floordiv(z_x, -8,") && program.contains("zeno_mod(z_x, -8,"), "{program}");
        let expected = interpreted(source);
        let Some((printed, errors, finished)) = build_and_run("power-of-two", &c, &["-O2"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, expected);
        let Some((printed, _, _)) = build_and_run("power-of-two-fast", &compile_fast(source, false), &["-O3"]) else { return };
        assert_eq!(printed, expected);
        assert_eq!(power_of_two("INT64_C(1024)"), Some(10));
        assert_eq!(power_of_two("1"), Some(0));
        for code in ["0", "12", "-8", "z_n", "INT64_C(9223372036854775807)", ""] {
            assert_eq!(power_of_two(code), None, "{code}");
        }
    }

    #[test]
    fn a_fast_build_leaves_out_the_checks_a_correct_program_never_fails() {
        // A float divided by zero is infinity and the square root of a
        // number below zero is NaN, as in C and Rust. A whole number
        // divided by zero is still caught.
        let source = "import math\nlet z = 0.0\nprint(1.0 / z, math.sqrt(-4.0))\nlet n = 0\nprint(1 // n)";
        let Some((printed, errors, finished)) = build_and_run("fast-unchecked", &compile_fast(source, false), &["-O3"]) else {
            return;
        };
        assert_eq!(printed, "inf NaN\n");
        assert!(!finished && errors.contains("ZeroDivisionError"), "{errors}");
        // Without `--fast`, every one of them is still an error.
        let release = compile_release("let xs = [1, 2]\nlet i = 2\nprint(xs[i])");
        assert!(!release.contains("ZENO_UNCHECKED 1"));
    }

    #[test]
    fn a_fast_build_reads_an_item_with_no_index_check() {
        let c = compile_fast("let xs = [1, 2, 3]\nlet i = 1\nprint(xs[i])", false);
        // The runtime's check is behind `#ifndef ZENO_UNCHECKED`, and the
        // first line of the file defines it.
        assert!(c.contains("#ifndef ZENO_UNCHECKED\n    if (i < 0 || i >= list->len) {"), "the check is there to leave out");
        let Some((printed, _, _)) = build_and_run("fast-index", &c, &["-O3"]) else { return };
        assert_eq!(printed, "2\n");
    }

    #[test]
    fn a_field_that_a_function_never_writes_is_read_once() {
        let source = r##"class P {
    x: float
    y: float
    weight: int
}
function pull(a: P, b: P) {
    a.x += b.weight * 0.5
    b.x -= a.weight * 0.5
    a.y = a.x + b.y
}
function moved(a: P, b: P) {
    a.weight = 7
    print(b.weight)
}
function shown(a: P) -> int {
    print(a.weight)
    return a.weight + abs(-1)
}
function told(a: P) -> int {
    let w = a.weight
    show(a)
    return a.weight - w
}
function show(a: P) {
    a.weight += 1
}
let p = P(1.0, 2.0, 3)
let q = P(4.0, 5.0, 6)
pull(p, q)
pull(p, p)
print(p.x)
print(p.y)
print(q.x)
moved(p, p)
print(shown(q))
print(told(q))
"##;
        let c = compile(source).expect("it compiles");
        // The body of a function: from the line that opens it to its end.
        let body = |name: &str| {
            let head = c.lines().find(|line| line.starts_with("static ") && line.contains(&format!(" {name}(")) && line.ends_with('{'));
            let after = c.split(head.expect("the function")).nth(1).unwrap();
            after.split("\n}\n").next().unwrap().to_string()
        };
        // `weight` is read once from each; `x` and `y` are written, and
        // read each time.
        let pull = body("z_pull");
        assert_eq!(pull.matches("->z_weight;").count(), 2, "{pull}");
        assert!(pull.contains("z_a->z_x = ") && pull.contains("z_b->z_y"), "{pull}");
        assert!(body("z_shown").contains("int64_t t"), "{}", body("z_shown"));
        // A field written anywhere in the function is read each time,
        // even through another parameter that may be the same object;
        // and a function that calls another reads each time.
        for name in ["z_moved", "z_told"] {
            let read_once = body(name).lines().any(|line| line.trim_start().starts_with("int64_t t") && line.ends_with("->z_weight;"));
            assert!(!read_once, "{name}: {}", body(name));
        }
        same_as_interpreter("read-once", source);
        no_collector_as_interpreter("read-once", source);
    }

    #[test]
    fn an_old_object_given_a_young_one_keeps_it() {
        // Old objects (in a long list, and one kept at the top) are given
        // new objects again and again while garbage is made around them.
        // A small collection must still find the new ones (decision 22.10).
        same_as_interpreter("old-and-young", r##"class Cell {
    value: int
    next: Cell or none
}
let keep = Cell(0, none)
var cells: list[Cell] = []
for i in range(300) {
    cells.append(Cell(i, none))
}
for round in range(4) {
    for i in range(len(cells)) {
        var junk = Cell(-1, none)
        junk = Cell(-2, junk)
        cells[i].next = Cell(i * round, none)
        if i % 7 == 0 {
            cells[i] = Cell(i + round, Cell(round, none))
        }
    }
    keep.next = Cell(round, keep.next)
}
var total = 0
for c in cells {
    total += c.value
    let after = c.next
    if after is not none {
        total += after.value
    }
}
var n = 0
var at = keep.next
while at is not none {
    n += at.value
    at = at.next
}
print(total, n)
"##);
    }

    #[test]
    fn trees_of_garbage_beside_a_kept_tree_stay_small() {
        // Each new tree's nodes sit next to the old trees' in memory. A
        // pointer to a node must not keep the node before it alive, or
        // every old tree is kept (decision 22.10).
        let source = r##"class Node {
    left: Node or none
    right: Node or none
}
function make(depth: int) -> Node {
    if depth == 0 {
        return Node(none, none)
    }
    return Node(make(depth - 1), make(depth - 1))
}
function count(node: Node or none) -> int {
    if node is none {
        return 0
    }
    return 1 + count(node.left) + count(node.right)
}
let kept = make(13)
var total = 0
for i in range(400) {
    total += count(make(11))
}
print(total, count(kept))
"##;
        let Some((printed, [made, most, collections])) = memory_of("garbage-trees", source) else {
            return;
        };
        assert_eq!(printed, interpreted(source));
        assert!(made > 20_000_000, "the program made {made} bytes");
        assert!(collections > 2, "the collector ran {collections} times");
        assert!(most < 12_000_000, "at most {most} bytes were in use at once, of {made} made");
    }

    #[test]
    fn one_file_of_a_program_can_say_memory_owned() {
        // The file of trees frees each object where its owner ends; the
        // first file keeps the collector for its own values.
        let source = r##"import shapes
class Box {
    items: list[int]
}
var total = 0
for i in range(20) {
    let tree = shapes.make(4)
    total += shapes.count(tree)
}
let b = Box([1, 2])
b.items.append(3)
print(total)
print(len(b.items))
==> shapes.zeno
memory owned

class Node {
    left: Node or none
    right: Node or none
}

function make(depth: int) -> Node {
    if depth == 0 {
        return Node(none, none)
    }
    return Node(make(depth - 1), make(depth - 1))
}

function count(node: Node or none) -> int {
    if node is none {
        return 0
    }
    return 1 + count(node.left) + count(node.right)
}
"##;
        let program = crate::modules::testing::built(source);
        assert_eq!(program.memory, None);
        assert_eq!(program.modules[0].memory, Some(crate::ast::Memory::Owned));
        let c = compile(source).expect("it compiles");
        assert!(c.contains("static void zeno_drop_shapes_Node("), "the owned file's objects are freed");
        assert!(!c.contains("zeno_drop_Box("), "the first file's are the collector's");
        same_as_interpreter("memory-per-file", source);
        // In a program built with no collector, a file may not ask for one.
        let clash = source.replacen("import shapes", "memory collector\nimport shapes", 1);
        let program = crate::modules::testing::built(&clash);
        let rules = crate::checker::Rules { strict: false, one_owner: true, firmware: false };
        let errors = crate::checker::check_by(&program, rules).0;
        assert!(errors.iter().any(|error| error.message.starts_with("this file says `memory collector`") && error.span.line == 1), "{:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
    }

    #[test]
    fn a_loop_over_text_walks_the_text_and_makes_no_list() {
        let source = r##"function first_wide(text: str) -> str {
    for ch in text {
        if ch == "ö" {
            return ch
        }
    }
    return "-"
}
let text = "héllo, wörld ✓"
var n = 0
for ch in text {
    if ch == "w" {
        break
    }
    if ch == "é" {
        continue
    }
    n += 1
}
print(n)
print(first_wide(text))
var keep: list[str] = []
for ch in text {
    keep.append(ch)
}
print(keep[1])
print(keep[13])
var last = ""
for ch in "añb" {
    last = ch
}
print(last)
for i, ch in "ab" {
    print(i)
}
for ch in text + "!" {
    keep.append(ch)
}
print(len(keep))
"##;
        // With a collector every loop over text walks it.
        let c = compile(source).expect("it compiles");
        assert_eq!(c.matches("zeno_chars(").count(), 2, "only the one with two names, and the definition");
        same_as_interpreter("text-walk", source);
        // With no collector the text must stay put while it is walked:
        // a variable the body leaves alone, or a literal. `text + "!"` is
        // built in the text room, and walks a list as before.
        let c = compile_no_collector(source).expect("it compiles");
        assert_eq!(c.matches("zeno_chars(").count(), 3, "two names, a built text, and the definition");
        no_collector_as_interpreter("text-walk", source);
    }

    #[test]
    fn values_that_may_be_none_match() {
        same_as_interpreter("none", r##"function find(id: int) -> str or none {
    if id == 1 { return "one" }
}
print(find(1), find(2))
let a = find(1)
print(a == "one", a is none)
var n: int or none = 5
if n is not none {
    print(n + 1)
}
function get() -> int or none { return 3 }
let g = get()
if g is none { print("none") } else { print(g * 2) }
let names = ["a", none, "c"]
print(names, names[1] is none, names[0] == "a")
var best: int or none = none
for x in [3, 9, 4] {
    if best is none or x > best { best = x }
}
print(best, best == 9, best is not none)
function half(n: int) -> float or none {
    if n % 2 == 0 { return n / 2 }
    return none
}
print(half(4), half(3), half(3) is none, half(4) == 2.0)
function flag(n: int) -> bool or none {
    if n > 0 { return true }
    if n < 0 { return false }
}
print(flag(1), flag(0), flag(-1))
let w: ms or none = 250 ms
print(w)
if w is not none { print(w + 1 s) }
"##);
        same_as_interpreter("none-in-objects", r##"function f(x: int or none) -> int {
    match x {
        case none { return 0 }
        case other { return 1 }
    }
}
print(f(none), f(1))
let e = ValueError("a")
let e2 = ValueError("a")
print(e == e2, e == e)
function show(u: str or none = none) { print(u) }
show()
show("x")
class Node { value: int
 next: Node or none = none }
let a = Node(1)
let b = Node(2, a)
print(b, a.next is none, b.next is not none)
let nx = b.next
if nx is not none { print(nx.value) }
print(f"{a.next} {nx}")
"##);
    }

    #[test]
    fn a_shared_variable_is_looked_at_again_after_a_check() {
        // `clear()` sets `n` back to none after the check. The compiled
        // program must stop, not carry on with a number that is not there.
        let source = "var n: int or none = 5\nfunction clear() { n = none }\nif n is not none {\n clear()\n print(n + 1)\n}";
        let c = compile(source).unwrap();
        assert!(c.contains("zeno_still_int(z_n, 5, 8)"));
        let Some((printed, errors, finished)) = build_and_run("gone", &c, &[]) else {
            return;
        };
        assert_eq!(printed, "");
        assert!(!finished);
        assert_eq!(
            errors,
            "error: this was checked with `is not none`, but it has been set to `none` since\n --> test.zeno:5:8\n"
        );
        // A variable nothing else can reach needs no second look.
        let c = compile("function f(n: int or none) -> int {\n if n is not none { return n + 1 }\n return 0\n}\nprint(f(1))").unwrap();
        assert!(c.contains("return zeno_add(z_n.value, 1, "));
    }

    #[test]
    fn errors_are_raised_and_caught_the_same_way() {
        same_as_interpreter("errors", r##"function parse_port(text: str) -> int raises ValueError {
    let port = int(text)
    if not 1 <= port <= 65535 {
        raise ValueError("port out of range")
    }
    return port
}
for text in ["8080", "0", "abc", " 1_000 ", "", "-5", "99999999999999999999", "+7", "_-5", "1__2", "-"] {
    try {
        print("port", parse_port(text))
    } except ValueError as err {
        print(f"error: {err}", err.kind, err.message == str(err))
    }
}
for text in ["2.5", " 1e3 ", "abc", "0x10", "inf", "-.5", "1.", "", "nan", "1e", "+3", "1_0"] {
    try { print(float(text)) } except ValueError as err { print(err) }
}
function depth(n: int) -> int {
    if n == 0 { return 1 // n }
    return depth(n - 1) + 1
}
function safe(n: int) -> int {
    var result = -1
    try {
        result = 10
        result = depth(n)
        result = 20
    } except ZeroDivisionError {
        result += 100
    }
    return result
}
print(safe(5))
function find(xs: list[int], want: int) -> int {
    var i = 0
    while i < 10 {
        try {
            if xs[i] == want { return i }
            if xs[i] < 0 { break }
            i += 1
            continue
        } except IndexError as e {
            print("ran off the end:", e)
            return -1
        }
    }
    return -2
}
print(find([5, 6, 7], 7), find([5, 6], 9), find([1, -1, 3], 3))
try {
    try {
        print([1, 2][5])
    } except ValueError {
        print("wrong one")
    }
} except IndexError as e {
    print("outer caught:", e)
}
try {
    try { raise ValueError("inner") } except ValueError as e { raise IndexError(f"from {e}") }
} except Error as e {
    print(e.kind, e)
}
var n = 0
try {
    n = 1
    print(9223372036854775807 + n)
    n = 2
} except OverflowError as e {
    print("overflow", n, e)
} except {
    print("other")
}
print(n)
let big = [1, 2, 3]
try { print(big.pop(), big.pop(), big.pop(), big.pop()) } except IndexError as e { print(e, big) }
print(parse_port("70000"))
"##);
        same_as_interpreter("error-values", r##"class Oops { code: int }
function f(n: int) -> int raises Oops, ValueError {
    if n == 0 { raise Oops(3) }
    if n == 1 { raise ValueError("one") }
    return n
}
try { print(f(0)) } except Oops as e { print("oops", e.code, e) }
try { print(f(1)) } except Error as e { print(e, e.kind, e.message) }
try { print(f(0)) } except Error as e { print(e) }
try { print(1 // 0) } except ZeroDivisionError as e { print(e.kind, e) }
try { print(f(1)) } except { print("any") }
let e = ValueError("made")
print(e, [e])
try { raise e } except ValueError as x { print(x == e) }
print(f(0))
"##);
        same_as_interpreter("raised-objects", r##"class Timeout {
    after: ms
    tries: int = 1
}
function wait(n: int) raises Timeout, ValueError {
    if n > 2 { raise Timeout(n * 100 ms, n) }
    if n < 0 { raise ValueError("negative") }
    print("waited", n)
}
for n in [1, 3, -1] {
    try {
        wait(n)
    } except Timeout as t {
        print("timeout", t.after, t.tries, t)
        t.tries = 0
    } except Error as e {
        print("error", e.kind, e.message, e)
    }
}
try { wait(5) } except Error as e { print(e.kind, e.message, e == e) }
try {
    try { wait(9) } except Error as e {
        print("passing on", e.kind)
        raise e
    }
} except Timeout as t {
    print("got it back", t.after)
}
let s = "  Hello, Wörld \t"
print(s.upper(), s.lower(), f"[{s.strip()}]", s.split(), "a,b,,c".split(","), "abc".replace("b", "--"), "abc".replace("", "."))
print("hello".starts_with("he"), "hello".ends_with("lo"), "hello".starts_with("lo"), "é".upper(), "x".replace("", "é"))
print(sum([1, 2, 3]), sum([0.5, 0.25]), min([3, 1, 2]), max([1.5, 0.5]), len("a b".split()))
try { print("a b".split("")) } except ValueError as e { print(e) }
let empty: list[int] = []
try { print(min(empty)) } except ValueError as e { print(e) }
print(sum(empty))
wait(7)
"##);
    }

    #[test]
    fn functions_as_values_match() {
        same_as_interpreter("closures", r##"function twice(f: function(int) -> int, x: int) -> int { return f(f(x)) }
print(twice(function(n) { return n * 3 }, 2))
function make(k: int) { return function(n: int) { return n * k } }
let triple = make(3)
print(triple(5), make(2)(4))
function counter() {
    var count = 0
    return function() {
        count += 1
        return count
    }
}
let c = counter()
let d = counter()
print(c(), c(), c(), d())
let fs: list[function(int) -> int] = [triple, make(10)]
print(fs[1](2), fs[0](2))
function apply(f: function(int) -> int, x: int) -> int { return f(x) }
function double(n: int) -> int { return n * 2 }
print(apply(triple, 1), apply(double, 21), twice(double, 1))
print(triple, double, [double])
let xs = [1, 2, 3, 4]
print(xs.map(double), xs.filter(function(n) { return n % 2 == 0 }), xs.reduce(function(a, b) { return a + b }, 0))
print(xs.map(function(n) { return f"<{n}>" }), xs.map(function(n) { return n / 2 }))
function both() {
    var total = 0
    let add = function(n: int) { total += n }
    add(5)
    total += 1
    add(10)
    return total
}
print(both())
class Button {
    label: str
    on_press: function(str) -> str
}
let b = Button("ok", function(s) { return s + "!" })
print(b.on_press(b.label))
function compose(f: function(int) -> int, g: function(int) -> int) {
    return function(x: int) { return g(f(x)) }
}
print(compose(double, triple)(1))
function adders() {
    var out: list[function(int) -> int] = []
    for i in range(3) {
        out.append(function(n: int) { return n + i })
    }
    return out
}
for f in adders() { print(f(10)) }
"##);
        same_as_interpreter("closures-and-try", r##"var n: int or none = 5
if n is not none {
    n += 1
    print(n)
}
function bump(x: int or none) -> int or none {
    var y = x
    if y is not none {
        y += 10
        y = y * 2
    }
    return y
}
print(bump(1), bump(none))
function outer(a: int) -> function(int) -> function(int) -> int {
    return function(b: int) {
        return function(c: int) { return a * 100 + b * 10 + c }
    }
}
print(outer(1)(2)(3))
class Acc {
    total: int = 0
    function adder(self) -> function(int) -> int {
        return function(n: int) {
            self.total += n
            return self.total
        }
    }
}
let acc = Acc()
let add = acc.adder()
print(add(5), add(6), acc)
function careful(text: str or none) -> function() -> str {
    if text is none { return function() { return "nothing" } }
    return function() { return text.upper() }
}
print(careful(none)(), careful("abc")())
function tries(xs: list[int]) -> list[str] {
    var log: list[str] = []
    let attempt = function(i: int) -> int {
        try {
            return xs[i]
        } except IndexError {
            log.append(f"miss {i}")
            return -1
        }
    }
    var hits = 0
    for i in range(4) {
        try {
            if attempt(i) < 0 { continue }
            hits += 1
            if hits == 2 { break }
        } except Error {
            log.append("never")
        }
    }
    log.append(f"hits {hits}")
    return log
}
print(tries([7, 8, 9]), tries([7]))
for round in range(2) {
    var count = 0
    let step = function() { count += round + 1 }
    try {
        step()
        count += 10
        print(1 // round)
        count += 100
    } except ZeroDivisionError {
        count += 1000
    }
    step()
    print(round, count)
}
function loop_out() -> int {
    var i = 0
    while true {
        try {
            try {
                i += 1
                if i == 3 { break }
                if i == 1 { continue }
                print([0][i])
            } except ValueError { print("no") }
        } except IndexError as e {
            print("caught", i, e)
        }
    }
    try { print([1][0]) } except IndexError { print("no") }
    return i
}
print(loop_out())
print(1 // 0)
"##);
    }

    #[test]
    fn a_new_value_changes_what_is_known_about_none() {
        // Walking a linked list, filling in a default, keeping the last
        // match: each gives the variable a new value after a check.
        same_as_interpreter("walk", r##"class Node {
    value: int
    next: Node or none = none
}
function total(head: Node or none) -> int {
    var node = head
    var sum = 0
    while node is not none {
        sum += node.value
        node = node.next
    }
    return sum
}
let list = Node(1, Node(2, Node(3)))
print(total(list), total(none))
function name_of(given: str or none) -> str {
    var name = given
    if name is none {
        name = "anon"
    }
    return name
}
print(name_of(none), name_of("zeno"))
function last_even(xs: list[int]) -> int or none {
    var found: int or none = none
    for x in xs {
        if x % 2 == 0 { found = x }
    }
    return found
}
print(last_even([1, 2, 3, 4, 5]), last_even([1, 3]))
var top: int or none = none
top = 5
print(top + 1)
function longest(words: list[str]) -> str or none {
    var best: str or none = none
    for word in words {
        if best is none or len(word) > len(best) {
            best = word
        }
    }
    return best
}
print(longest(["a", "ccc", "bb"]), longest([]))
"##);
    }

    #[test]
    fn an_empty_list_is_a_list_of_whatever_it_arrives_at() {
        same_as_interpreter("empty", r##"class Bag {
    items: list[int] = []
    name: str or none = none
}
function count(xs: list[str]) -> int { return len(xs) }
function maybe(flag: bool) -> list[int] or none {
    if flag { return [] }
    return none
}
let b = Bag()
b.items.append(4)
print(b, count([]), maybe(true), maybe(false))
var grid: list[list[int]] = [[], [1, 2]]
grid.append([])
grid[1] = []
print(grid, len([]))
var xs: list[int] = [1]
xs = []
print(xs, [] == xs)
let c = Bag()
c.items.append(9)
print(b.items, c.items)
function push(x: int, into: list[int] = []) -> list[int] {
    into.append(x)
    return into
}
print(push(1), push(2), push(3, [0]))
"##);
        let message = compile("print([])").err().unwrap_or_else(|| "it compiled".to_string());
        assert!(message.starts_with("`zeno build` needs to know what a list holds, and nothing says it here"), "{message}");
    }

    #[test]
    fn a_program_for_a_page_is_sent_to_the_build_for_pages() {
        let message = compile("import web\nweb.element(\"#count\").text = \"3\"").err().unwrap_or_else(|| "it compiled".to_string());
        assert_eq!(
            message,
            "the module `web` is for a program in a page, and this build makes a program for a desktop or a board. \
             `zeno build --web` builds the page"
        );
    }

    #[test]
    fn starting_another_program_is_left_to_zeno_run() {
        let message = compile("import program\nprint(program.run(\"ls\", [\"-l\"]))").err().unwrap_or_else(|| "it compiled".to_string());
        assert_eq!(message, "`zeno build` cannot compile `program.run` yet. `zeno run` can still run this program");
    }

    #[test]
    fn the_new_pieces_work_together() {
        same_as_interpreter("optional-fields", r##"class Node {
    value: int
    weight: float or none = none
    next: Node or none = none
}
let a = Node(1, 2.5)
let b = Node(1, 2.5)
let c = Node(1, none, a)
print(a == b, a == c, c == Node(1, none, b), c, [a.weight, c.weight] == [2.5, none] )
"##);
        same_as_interpreter("together", r##"function countdown(start: int or none) -> int {
    var x = start
    var steps = 0
    while x is not none and x > 0 {
        x = x - 1
        steps += 1
    }
    return steps
}
print(countdown(3), countdown(none))
let fs = [function() { return n * n } for n in range(4)]
print([f() for f in fs])
enum Reading {
    Value(v: float)
    Fault(code: int)
}
function read(i: int) -> Reading or none {
    if i == 0 { return Value(1.5) }
    if i == 1 { return Fault(7) }
    return none
}
for i in range(3) {
    let r = read(i)
    var seen = 0
    try {
        if r is none { raise ValueError("nothing to read") }
        seen = 1
        match r {
            case Value(v) { print("value", v) }
            case Fault(code) {
                seen = 2
                raise IndexError(f"fault {code}")
            }
        }
        seen = 3
    } except ValueError as e {
        print("value error:", e, seen)
    } except IndexError as e {
        print("index error:", e, seen)
    }
    print("seen", seen)
}
let gaps = [21.5, none, 22.0]
var total = 0.0
for g in gaps {
    if g is not none { total += g }
}
gaps[1] = 30.0
gaps.append(none)
print(gaps, total, gaps[1] == 30.0, gaps[3] is none, [1, none] == [1, none], [true, none], gaps == [21.5, 30.0, 22.0, none])
let words = "the quick  brown fox".split()
print(words.map(function(w) { return w.upper() }), words.filter(function(w) { return len(w) > 3 }).map(len_of))
function len_of(w: str) -> int { return len(w) }
print(words.reduce(function(acc: str, w: str) { return acc + w[0] }, ""))
let word = "héllo"
print(word[1], word[4], [c.upper() for c in word], len(word))
for ch in "añb" { print(ch) }
print(word[5])
"##);
    }

    #[test]
    fn the_examples_compile_and_match() {
        // `blink.zeno` needs a `board` module, which only exists on a board.
        for (name, source) in [
            ("hello", include_str!("../examples/hello.zeno")),
            ("tour", include_str!("../examples/tour.zeno")),
            ("styles", include_str!("../examples/styles.zeno")),
            ("primes", include_str!("../examples/primes.zeno")),
            ("shapes", include_str!("../examples/shapes.zeno")),
            ("sensor", include_str!("../examples/sensor.zeno")),
            ("memory", include_str!("../examples/memory.zeno")),
            ("buffer", include_str!("../examples/buffer.zeno")),
            ("signal", include_str!("../examples/signal.zeno")),
            ("packet", include_str!("../examples/packet.zeno")),
            ("generics", include_str!("../examples/generics.zeno")),
            ("labels", include_str!("../examples/labels.zeno")),
            ("words", include_str!("../examples/words.zeno")),
            ("scheduler", include_str!("../examples/scheduler.zeno")),
            ("doorbell", include_str!("../examples/doorbell.zeno")),
            ("console", include_str!("../examples/console.zeno")),
            ("battery", include_str!("../examples/battery.zeno")),
            ("dimmer", include_str!("../examples/dimmer.zeno")),
            ("steady", include_str!("../examples/steady.zeno")),
            ("tachometer", include_str!("../examples/tachometer.zeno")),
            ("commands", include_str!("../examples/commands.zeno")),
            ("handlers", include_str!("../examples/handlers.zeno")),
            ("driver", include_str!("../examples/driver.zeno")),
        ] {
            same_as_interpreter(name, source);
        }
    }

    // ---------------------------------------------- Errors as values (firmware)

    /// `same_as_interpreter`, for a program that a firmware build must
    /// accept, so that the firmware half of the comparison really runs.
    fn firmware_as_interpreter(name: &str, source: &str) {
        if let Err(message) = compile_firmware(source) {
            panic!("the firmware build refused this: {message}");
        }
        same_as_interpreter(name, source);
        // The LLVM backend must accept it for firmware too.
        crate::emit_llvm::tests::firmware_as_interpreter(&format!("{name}-llvm"), source);
    }

    #[test]
    fn a_firmware_build_has_no_setjmp() {
        let source = include_str!("../examples/sensor.zeno");
        let c = compile_firmware(source).unwrap();
        for word in ["setjmp", "longjmp", "jmp_buf", "ZENO_FIRMWARE"] {
            assert!(!c.contains(word), "`{word}` is in the C of a firmware build");
        }
        // A function that can raise says whether it worked, and gives its
        // answer through a pointer. Its caller tests what came back.
        assert!(c.contains("static bool z_parse_line(const char *z_line, zeno_opt_float *zeno_result) {"));
        assert!(c.contains("if (!zeno_parse_float(z_text, 18, 17, &t"));
        assert!(c.contains("if (!z_parse_line(z_line, &t"));
        assert!(c.contains(")) goto zeno_except_"));
        assert!(c.contains("if (zeno_is(zeno_caught_"));
        // A function that cannot raise is exactly as before.
        assert!(c.contains("static zeno_closure *z_in_range(double z_low, double z_high) {"));
        // And an ordinary build has none of this.
        let c = compile(source).unwrap();
        assert!(c.contains("setjmp") && !c.contains("zeno_except_") && !c.contains("ZENO_FIRMWARE"));
        assert!(c.contains("static zeno_opt_float z_parse_line(const char *z_line) {"));
    }

    #[test]
    fn errors_as_values_match() {
        // Errors the program raises, through functions, methods, loops,
        // nested `try` blocks, conditions and recursion, and one left
        // uncaught at the end.
        firmware_as_interpreter("values-a", r##"class Timeout {
    after: int
    tries: int
}
class Sensor {
    name: str
    offset: float
    function read(self, raw: str) -> float raises ValueError {
        let value = float(raw) + self.offset
        if value > 100.0 {
            raise ValueError(f"{self.name}: {value} is too hot")
        }
        return value
    }
    function check(self, raw: str) raises ValueError {
        self.read(raw)
    }
}
function parse_port(text: str) -> int raises ValueError {
    let port = int(text)
    if not 1 <= port <= 65535 {
        raise ValueError("port out of range")
    }
    return port
}
function note(what: str) -> int {
    print("  note", what)
    return 1
}
function first_port(texts: list[str]) -> int or none raises ValueError {
    for text in texts {
        if text == "skip" { continue }
        if text == "stop" { break }
        return parse_port(text)
    }
}
function wait(n: int) raises Timeout {
    if n > 3 {
        raise Timeout(n * 100, n)
    }
    print("  waited", n)
}
function depth(n: int) -> int raises ValueError {
    if n == 0 { raise ValueError("bottom") }
    return depth(n - 1) + 1
}
function safe(n: int) -> int {
    var result = -1
    try {
        result = 10
        result = depth(n)
        result = 20
    } except ValueError {
        result += 100
    }
    return result
}
function find(texts: list[str], want: int) -> int {
    var i = 0
    while i < len(texts) {
        try {
            if parse_port(texts[i]) == want { return i }
            if texts[i] == "9" { break }
            i += 1
            continue
        } except ValueError as e {
            print("  bad entry:", e)
            return -1
        }
    }
    return -2
}
function relay(n: int) raises Timeout, ValueError {
    try {
        wait(n)
        parse_port("x")
    } except ValueError as e {
        print("  relay saw", e)
        raise e
    }
}
function total(texts: list[str]) -> int raises ValueError {
    var sum = 0
    for text in texts {
        sum += parse_port(text) + note(text) * parse_port(text)
    }
    return sum
}

for text in ["8080", "0", "abc", " 1_000 ", "", "-5", "99999999999999999999"] {
    try {
        print("port", parse_port(text))
    } except ValueError as err {
        print(f"error: {err}", err.kind, err.message == str(err))
    }
}
for text in ["2.5", " 1e3 ", "abc", "0x10", ""] {
    try { print(float(text)) } except ValueError as err { print(err) }
}
print(safe(5), safe(0))
print(find(["5", "6", "7"], 7), find(["5", "x"], 9), find(["1", "9", "3"], 3), find(["1"], 3))
let s = Sensor("probe", 0.5)
try {
    print(s.read("20"))
    s.check("21")
    print(s.read("99.9"))
    print("not reached")
} except ValueError as e {
    print("caught:", e)
}
try {
    try {
        print(parse_port("7"), parse_port("70000"))
    } except Timeout {
        print("wrong one")
    }
} except ValueError as e {
    print("outer caught:", e)
}
try {
    try { raise ValueError("inner") } except ValueError as e { raise IndexError(f"from {e}") }
} except Error as e {
    print(e.kind, e)
}
for n in [2, 5] {
    try {
        relay(n)
    } except Timeout as t {
        print("timeout", t.after, t.tries, t)
    } except Error as e {
        print("other", e.kind, e)
    }
}
try {
    print(first_port(["skip", "80"]), first_port(["skip", "stop", "81"]), first_port([]))
    print(first_port(["skip", "zero"]))
} except {
    print("any")
}
var count = 0
try {
    if parse_port("1") == 1 and parse_port("2") == 2 or parse_port("bad") == 3 {
        count += 1
    }
    while parse_port(str(count + 1)) < 4 {
        count += 1
    }
    print(f"count {count} port {parse_port('44')} then {parse_port('nope')}")
} except ValueError as e {
    print("stopped at", count, e)
}
try {
    print(total(["1", "2"]))
    print(total(["3", "0", "5"]))
} except ValueError as e {
    print(e)
}
function chain(n: int) -> int raises ValueError {
    try {
        return parse_port(str(n))
    } except ValueError as e {
        if n < -1 {
            raise ValueError(f"gave up at {n}")
        }
        return chain(n - 1)
    }
}
try { print(chain(3), chain(0)) } except ValueError as e { print(e) }
let e = ValueError("kept")
try { raise e } except ValueError as x { print(x == e) }
print(parse_port("70000"))
print("never printed")
"##);
    }

    #[test]
    fn errors_as_values_match_in_methods_loops_and_function_values() {
        firmware_as_interpreter("values-b", r##"enum Reply {
    Ok(value: int)
    Missing
}
class Bad {
    why: str
}
class Parser {
    seen: int = 0
    function one(self, text: str) -> int raises ValueError {
        self.seen += 1
        return int(text)
    }
    function many(self, texts: list[str]) -> list[int] raises ValueError {
        return [self.one(text) for text in texts]
    }
    function reply(self, text: str) -> Reply raises Bad {
        if text == "" { return Missing }
        if text == "!" { raise Bad("bang") }
        try {
            return Ok(self.one(text))
        } except ValueError as e {
            raise Bad(f"not a number: {text}")
        }
    }
}
function describe(p: Parser, text: str) -> str raises Bad {
    match p.reply(text) {
        case Ok(value) { return f"ok {value}" }
        case Missing { return "missing" }
    }
}
function numbers(text: str) -> list[int] raises ValueError {
    var out: list[int] = []
    for piece in text.split(",") {
        out.append(int(piece))
    }
    return out
}
function first_good(texts: list[str]) -> int or none {
    for text in texts {
        try {
            return int(text)
        } except ValueError {
            continue
        }
    }
    return none
}
function count_good(texts: list[str]) -> int {
    var good = 0
    var i = 0
    while true {
        try {
            if i >= len(texts) { break }
            let text = texts[i]
            i += 1
            int(text)
            good += 1
        } except ValueError {
            try {
                float(texts[i - 1])
                good += 10
            } except ValueError {
                if texts[i - 1] == "end" { break }
            }
        }
    }
    return good
}
let p = Parser()
try {
    print(p.many(["1", "2", "3"]), p.seen)
    print(p.many(["4", "x", "6"]), p.seen)
} except ValueError as e {
    print("stopped:", e, p.seen)
}
for text in ["12", "", "zz", "!"] {
    try {
        print(describe(p, text))
    } except Bad as bad {
        print("bad:", bad.why, bad)
    }
}
try {
    for n in numbers("3,4,5") { print(n) }
    for n in numbers("6,seven") { print(n) }
} except ValueError as e { print(e) }
print(first_good(["a", "b", "7", "8"]), first_good(["a"]), first_good([]))
print(count_good(["1", "2.5", "x", "3", "end", "4"]))
let twice = function(text: str) -> int {
    try {
        return int(text) * 2
    } except ValueError {
        return -1
    }
}
print(twice("21"), twice("no"), ["1", "b", "3"].map(twice))
function boom() raises Bad {
    raise Bad("at the top")
}
print("before")
boom()
print("after")
"##);
    }

    #[test]
    fn what_a_firmware_build_refuses_says_so() {
        let refused = |source: &str| compile_firmware(source).err().unwrap_or_else(|| "it compiled".to_string());
        // A division by zero stops a firmware build. An `except` for it
        // would never run, so it is refused, not left to mislead.
        let message = refused("let zero = 0\ntry { print(1 // zero) } except ZeroDivisionError { print(\"no\") }");
        assert!(message.starts_with("nothing in this `try` can be caught. In a firmware build"), "{message}");
        assert!(message.ends_with("Test for it before it happens, or build without `--firmware`"), "{message}");
        let parse = "function parse(text: str) -> int raises ValueError { return int(text) }\n";
        let message = refused(&format!("{parse}try {{ print(parse(\"1\")) }} except IndexError {{ print(\"no\") }}"));
        assert!(message.starts_with("nothing in this `try` can reach `except IndexError`."), "{message}");
        // A function that can raise, used as a value, builds since a
        // function type can say `raises` (decision 56.5).
        assert!(compile_firmware(&format!("{parse}print([\"1\"].map(parse))")).is_ok());
        assert!(compile_firmware("let f = function(text: str) -> int raises ValueError { return int(text) }\nprint(1)").is_ok());
        assert!(compile(&format!("{parse}print([\"1\"].map(parse))")).is_ok());
    }

    #[test]
    fn raising_function_values_match_every_way() {
        // A function that raises, used as a value through a type that says
        // so: handed to a parameter, kept in a field and in a list, given
        // to `map`, `filter`, `reduce` and `sort`, failing inside a loop a
        // thousand times, and a function that raises nothing put where
        // the type says `raises` (decision 56). Every build must print
        // what `zeno run` prints, and with no collector a failure half way
        // through `map` must free the list it was making.
        let source = include_str!("../tools/targets/edge-raises-in-types.zeno");
        firmware_as_interpreter("raising-values", source);
        no_collector_as_interpreter("raising-values", source);
        // A plain function where a raising one is wanted gets a wrapper
        // that says it worked, and a raising one gets one that passes on
        // what it says.
        let c = compile_firmware(source).unwrap();
        assert!(c.contains("static bool zeno_raising_wrap_double(zeno_closure *zeno_self, int64_t a0, int64_t *zeno_result) {\n    (void)zeno_self;\n    *zeno_result = z_double(a0);\n    return true;\n}"), "{c}");
        assert!(c.contains("static bool zeno_wrap_parse(zeno_closure *zeno_self, const char *a0, int64_t *zeno_result) {\n    (void)zeno_self;\n    return z_parse(a0, zeno_result);\n}"), "{c}");
        // An ordinary build has neither: both kinds are called alike.
        let c = compile(source).unwrap();
        assert!(!c.contains("zeno_raising_") && !c.contains("zeno_result"), "{c}");
    }

    #[test]
    fn a_kept_function_takes_what_it_uses_every_way() {
        // Returned, kept in a variable, a field and a list, given to a
        // parameter that says `take`, made a thousand times, called after
        // the function that made it has returned, failing through a type
        // that says `raises`, and called straight from the call that made
        // it (decision 56.8). Every build prints what `zeno run` prints,
        // and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-kept-functions.zeno");
        no_collector_as_interpreter("kept-functions", source);
        firmware_as_interpreter("kept-functions", source);
        crate::emit_llvm::tests::same_as_interpreter("kept-functions", source);
        // With a collector, a value that says `owned` has one owner, and
        // is taken the same way (plan, Changes 4).
        let owned = "function f() -> function() -> int {\n    let xs: owned list[int] = [1, 2]\n\
                     return function() -> int { return len(xs) }\n}\n\
                     function g(ys: take owned list[int]) -> function() -> int {\n\
                     return function() -> int { return len(ys) }\n}\n\
                     let kept = [f(), g([1, 2, 3])]\nprint(f()(), g([4])(), kept[0]() + kept[1]())\n";
        firmware_as_interpreter("kept-owned", owned);
        crate::emit_llvm::tests::same_as_interpreter("kept-owned", owned);
        no_collector_as_interpreter("kept-owned", owned);
        // What it takes is moved into its block, and freed with it.
        let compose = "function compose(f: take function(int) -> int, g: take function(int) -> int) -> function(int) -> int {\n\
                       return function(n: int) -> int { return f(g(n)) }\n}\n\
                       print(compose(function(n: int) -> int { return n + 1 }, function(n: int) -> int { return n * 2 })(3))\n";
        let c = compile_no_collector(compose).unwrap();
        assert!(c.contains("    zeno_closure_drop(env->z_f);\n    zeno_closure_drop(env->z_g);\n    zeno_free(env);\n}"), "{c}");
        assert!(c.contains("->z_f = z_f;\n    z_f = NULL;\n"), "{c}");
    }

    #[test]
    fn a_method_used_as_a_value_every_way() {
        // Passed along, kept, taken with the object, through an interface,
        // raising, and made a thousand times (decision 56.9). Every build
        // prints what `zeno run` prints, and with no collector nothing is
        // left.
        let source = include_str!("../tools/targets/edge-method-values.zeno");
        no_collector_as_interpreter("method-values", source);
        firmware_as_interpreter("method-values", source);
        crate::emit_llvm::tests::same_as_interpreter("method-values", source);
        // The method is bound to the object the variable holds on that
        // line, as in `zeno run`: a `var` given another object after it,
        // at the top of the file or in a function, does not change it.
        let bound = "class Counter {\n    count: int\n    function bump(self, by: int) -> int {\n\
                     self.count += by\n        return self.count\n    }\n}\n\
                     var c = Counter(0)\nlet m = c.bump\nc = Counter(100)\nprint(m(1), c.bump(1))\n\
                     function f() -> int {\n    var d = Counter(0)\n    let n = d.bump\n    d = Counter(50)\n\
                     let shared = function() -> int { return d.count }\n    return n(1) * 1000 + shared()\n}\nprint(f())\n";
        firmware_as_interpreter("method-bound", bound);
        crate::emit_llvm::tests::same_as_interpreter("method-bound", bound);
        // With a collector, an object that says `owned` is taken by one
        // that is kept, and looked at from the top of the file.
        let owned = "owned class Packet {\n    id: int\n    data: owned list[int]\n\
                     function size(self) -> int { return len(self.data) }\n}\n\
                     function make() -> Packet { return Packet(1, [1, 2]) }\n\
                     function apply(f: function() -> int) -> int { return f() }\n\
                     function hold() -> function() -> int {\n    let p = make()\n    return p.size\n}\n\
                     let top = make()\nlet m = top.size\nprint(m(), apply(top.size), hold()(), top.id)\n";
        firmware_as_interpreter("method-owned", owned);
        crate::emit_llvm::tests::same_as_interpreter("method-owned", owned);
        no_collector_as_interpreter("method-owned", owned);
    }

    #[test]
    fn several_values_every_way() {
        // Decision 57: numbers, two texts built inside the function, a
        // list and an object, swapped, from a function that raises, from
        // a generic and a function value, a thousand times. Every build
        // prints what `zeno run` prints, and with no collector nothing is
        // left.
        let source = include_str!("../tools/targets/edge-several-2.zeno");
        no_collector_as_interpreter("several", source);
        firmware_as_interpreter("several", source);
        crate::emit_llvm::tests::same_as_interpreter("several", source);
        // A text that is received and one that is dropped, each statement
        // a thousand times: the text room is given back after each.
        let room = "function names(n: int) -> (str, str) {\n    return \"Ada\" + str(n), \"x\" + str(n)\n}\n\
                    var words = \"\"\nfor i in range(1000) {\n    let label, _ = names(i)\n    let _, other = names(i)\n\
                    if i % 250 == 0 {\n        words += label + other\n    }\n}\nprint(words)\n";
        no_collector_as_interpreter("several-room", room);
    }

    // ------------------------------------------- Resources (decision 59.1)

    /// A program with resources, in the six builds (C and LLVM, each
    /// ordinary, firmware and with no collector), against `zeno run`:
    /// what it prints, how it stops, the number it ends with, and with
    /// no collector every block given back when it ends well. (Against
    /// the interpreter alone: `same_as_interpreter` asks the VM too.)
    fn resources_as_interpreter(name: &str, source: &str) {
        let whole = crate::modules::testing::program(source);
        // The interpreter and the VM, which must agree.
        let ran = crate::vm::run_both_ending(&whole, source);
        let stopped = match &ran.result {
            Ok(()) => String::new(),
            Err(error) => format!("error: {}\n --> {}\n", error.message, crate::modules::testing::place(&whole, error.span)),
        };
        let program = crate::modules::testing::built(source);
        for build in [Build::Ordinary, Build::Firmware, Build::NoCollector] {
            let (errors, types) = crate::checker::check_by(&program, build.rules());
            assert!(errors.is_empty(), "{build:?} refused it: {}\n{source}", errors[0].message);
            let flags: &[&str] = match build {
                Build::Ordinary => &["-DZENO_COLLECT_ALWAYS"],
                Build::Firmware => &[],
                Build::NoCollector => &["-DZENO_MEMORY_REPORT"],
            };
            let c = emit(&program, &types, "test.zeno", build).unwrap_or_else(|error| panic!("{build:?}: {}", error.message));
            let c_way = format!("{name}-{build:?}");
            let ran_c = built_output(&c_way, &c, flags);
            let ir = crate::emit_llvm::emit(&program, &types, "test.zeno", build, build == Build::NoCollector)
                .unwrap_or_else(|error| panic!("{build:?} through LLVM: {}", error.message));
            let ir_way = format!("{name}-{build:?}-llvm");
            let ran_ir = crate::emit_llvm::tests::built_output(&ir_way, &ir, build, flags);
            for (way, output) in [(c_way, ran_c), (ir_way, ran_ir)] {
                let Some(output) = output else { continue };
                let printed = String::from_utf8_lossy(&output.stdout);
                let errors = String::from_utf8_lossy(&output.stderr);
                assert_eq!(printed, ran.output, "{way} prints something else\n{errors}");
                assert_eq!(output.status.code(), Some(i32::from(ran.status)), "{way} ends with another number\n{errors}");
                // With no collector the report of the memory used comes last.
                let report = |line: &&str| ["memory:", "pool:", "text:"].iter().any(|word| line.starts_with(word));
                let said: String = errors.lines().filter(|line| !report(line)).map(|line| format!("{line}\n")).collect();
                assert_eq!(said, format!("{}{stopped}", ran.warned), "{way}");
                if build == Build::NoCollector && ran.status == 0 {
                    assert!(errors.contains("no collector, 0 blocks left at the end\n"), "{way} leaves memory: {errors}");
                }
            }
        }
    }

    const HELD: &str = "owned class Held is Resource {\n    name: str\n\n    function release(self) {\n        \
                        print(\"  released\", self.name)\n    }\n}\n\n";

    #[test]
    fn a_resource_is_released_on_every_way_out_in_every_build() {
        // The end of a block, `return` with a value worked out first,
        // `break` and `continue`, an error passing through two blocks,
        // one caught inside, a parameter that takes one, one given away
        // on one path of an `if`, a `match`, and the top of the file.
        resources_as_interpreter("resource-edges", include_str!("../tools/targets/edge-resource.zeno"));
    }

    #[test]
    fn a_resource_is_released_on_the_way_out_of_a_try_a_loop_and_a_function() {
        let source = format!(
            "{HELD}{}",
            r#"function guarded(fail: bool) -> str {
    let held = Held("guarded")
    try {
        let inside = Held("inside the try")
        if fail {
            raise ValueError("no")
        }
        return "returned from the try"
    } except ValueError as e {
        return f"returned from the except: {e.message}"
    }
}
print(guarded(false))
print(guarded(true))

function loops() {
    for i in range(3) {
        let round = Held(f"round {i}")
        if true {
            let inner = Held(f"inner {i}")
            if i == 0 {
                continue
            }
            if i == 2 {
                break
            }
            print("  end of inner", i)
        }
    }
    var n = 0
    while n < 5 {
        n += 1
        let turn = Held(f"turn {n}")
        match n {
            case 2 {
                let arm = Held("arm")
                continue
            }
            case 3 {
                break
            }
            case _ {
                print("  turn", n)
            }
        }
    }
}
loops()

function taker(held: take Held, early: bool) -> int {
    if early {
        return 1
    }
    print("  taker ends")
    return 2
}
print("taker:", taker(Held("taken early"), true))
print("taker:", taker(Held("taken late"), false))

function choose(give: bool) {
    let held = Held(f"choose {give}")
    if give {
        let other = held
        print("  given to other")
    } else {
        print("  kept")
    }
    print("  end of choose")
}
choose(true)
choose(false)

function later() -> int {
    let x = helper()
    let held = Held("later")
    function helper() -> int {
        return 40
    }
    return x + helper() - 38
}
print("later:", later())

function catcher() {
    let held = Held("catcher")
    try {
        let other = Held("in try")
        thrower()
    } except Error as e {
        print("  caught", e.message)
    }
}
function thrower() raises ValueError {
    let mine = Held("thrower's")
    raise ValueError("thrown")
}
catcher()

function shadow(early: bool) -> int {
    let held = Held("outer held")
    if true {
        let held = 5
        if early {
            return held
        }
    }
    return 0
}
print("shadow", shadow(true), shadow(false))

function as_interface(r: take Resource) {
    print("  given as a Resource")
}
as_interface(Held("through the interface"))

try {
    raise ValueError("first")
} except ValueError as e {
    let handler = Held("in the handler")
    print("  handling", e.message)
}

let run = function(n: int) -> int {
    let inside = Held(f"in the value {n}")
    return n * 2
}
print("gave", run(21))

let top = Held("top")
print("after top")
function below() -> str {
    return "below"
}
class Plain {
    n: int
}
print(below(), Plain(1).n)
"#
        );
        resources_as_interpreter("resource-ways-out", &source);
    }

    #[test]
    fn an_error_through_a_resource_is_reported_unchanged_after_the_release() {
        // Nobody catches it: each block it passes releases what it owns,
        // innermost first, and the error is the one that was raised, at
        // the place it was raised.
        let source = format!(
            "{HELD}{}",
            "function deep(n: int) raises ValueError {\n    let inner = Held(\"inner\")\n    if n > 0 {\n        \
             raise ValueError(\"too deep\")\n    }\n}\n\
             let outer = Held(\"outer\")\nlet second = Held(\"second\")\nprint(\"before\")\ndeep(1)\nprint(\"never\")\n"
        );
        resources_as_interpreter("resource-uncaught", &source);
        // A `release` that catches an error of its own leaves the one on
        // its way as it was.
        let source = "owned class Noisy is Resource {\n    name: str\n\n    function release(self) {\n        \
                      try {\n            raise KeyError(\"inside release\")\n        } except KeyError as e {\n            \
                      print(\"  release caught\", e.message)\n        }\n        print(\"  released\", self.name)\n    }\n}\n\
                      function fails() raises ValueError {\n    let noisy = Noisy(\"noisy\")\n    raise ValueError(\"the first error\")\n}\n\
                      try {\n    fails()\n} except ValueError as e {\n    print(\"caught\", e.message)\n}\n\
                      let top = Noisy(\"top\")\nfails()\n";
        resources_as_interpreter("resource-release-catches", source);
    }

    #[test]
    fn program_exit_releases_nothing() {
        let source = format!(
            "import program\n{HELD}{}",
            "function stop() {\n    let inner = Held(\"inner\")\n    print(\"stopping\")\n    program.exit(3)\n}\n\
             let outer = Held(\"outer\")\nif true {\n    let block = Held(\"block\")\n    stop()\n}\n"
        );
        resources_as_interpreter("resource-exit", &source);
    }

    #[test]
    fn a_resource_is_released_before_its_block_frees_and_given_away_leaves_null() {
        let c = compile(&format!("{HELD}function f() {{\n    let held = Held(\"f\")\n    let other = held\n}}\nf()\n")).unwrap();
        // In an ordinary build the rest of the block is a `try` that
        // releases the resource as an error passes, and lets it go on.
        assert!(c.contains("    if (zeno_setjmp(t"), "{c}");
        assert!(c.contains("        zeno_raise_again();\n    }\n"), "{c}");
        // Given away: the variable holds NULL after, and its release
        // does nothing.
        assert!(c.contains(" = z_held;\n    z_held = NULL;\n"), "{c}");
        assert!(c.contains(
            "if (z_held != NULL) { z_Held *zeno_held = z_held; z_held = NULL; z_Held_release(zeno_held); zeno_drop_Held(zeno_held); }"
        ));
        // A firmware build has no `try` for it.
        let c = compile_firmware(&format!("{HELD}function f() {{\n    let held = Held(\"f\")\n}}\nf()\n")).unwrap();
        assert!(!c.contains("setjmp") && c.contains("z_Held_release(zeno_held)"), "{c}");
    }

    #[test]
    fn take_leaves_none_every_way() {
        // A scheduler that hands its task on, fields and items of every
        // kind that may be `none`, a thousand rounds, and a value nobody
        // receives (decision 56.10). Every build prints what `zeno run`
        // prints, and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-take.zeno");
        no_collector_as_interpreter("take", source);
        firmware_as_interpreter("take", source);
        crate::emit_llvm::tests::same_as_interpreter("take", source);
        // The value is read into a variable of its own, and `none` is
        // written where it was. Nothing is freed there.
        let c = compile_no_collector(
            "class Task {\n    id: int\n}\nclass Slot {\n    task: Task or none\n}\n\
             let s = Slot(Task(1))\nlet t = take s.task\nprint(t, s.task)\n",
        )
        .unwrap();
        assert!(c.contains(" = z_s->z_task;\n    z_s->z_task = NULL;\n"), "{c}");
        // A bad index stops the program at the `take`, in every build.
        let bad = "class Task {\n    id: int\n}\nlet ts: list[Task or none] = [Task(1)]\nprint(\"before\")\n\
                   let t = take ts[5]\nprint(\"never\")\n";
        let wanted = "error: uncaught IndexError: index 5 is out of range for a list of length 1\n --> test.zeno:6:14\n";
        for (name, c) in [("take-bad", compile(bad)), ("take-bad-firmware", compile_firmware(bad)), ("take-bad-owned", compile_no_collector(bad))] {
            let Some((printed, errors, finished)) = build_and_run(name, &c.unwrap(), &[]) else {
                return;
            };
            assert_eq!((printed.as_str(), errors.as_str(), finished), ("before\n", wanted, false), "{name}");
        }
    }

    #[test]
    fn a_generic_enum_every_way() {
        // `Maybe`, `Result` and `Chain` with numbers, text, a list and an
        // object, `Nothing` everywhere its type is said, copies made
        // inside generics, a thousand rounds and every way out of a
        // `match` (decision 58.1). Every build prints what `zeno run`
        // prints, and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-generic-enum.zeno");
        no_collector_as_interpreter("generic-enum", source);
        // One enum for each copy, each with its own cases, and nothing
        // for the generic itself.
        let c = compile(
            "enum Maybe[T] {\n    Nothing\n    Just(value: T)\n}\nlet a = Just(3)\nlet b: Maybe[str] = Nothing\nprint(a, b)\n",
        )
        .unwrap();
        for wanted in ["struct z_Maybe_int {", "struct z_Maybe_str {", "zeno_new_Maybe_int_Just(", "zeno_case_Maybe_str_Nothing"] {
            assert!(c.contains(wanted), "{wanted}\n{c}");
        }
        assert!(!c.contains("z_Maybe ") && !c.contains("zeno_new_Maybe_Just"), "{c}");
    }

    #[test]
    fn a_bound_every_way() {
        // `[T is Ordered]` over every kind of number, a quantity and text,
        // and `[S is Shape]` over circles, squares and shapes, on a
        // function, a class and an enum (decision 58.3). Every build
        // prints what `zeno run` prints, and with no collector nothing is
        // left.
        let source = include_str!("../tools/targets/edge-bounds.zeno");
        no_collector_as_interpreter("bounds", source);
        // A bound is checked at the call and makes no code: the C is what
        // it is without one.
        let plain = source.replace(" is Ordered", "").replace(" is Shape]", "]");
        assert_ne!(plain, source);
        assert_eq!(compile(source).unwrap(), compile(&plain).unwrap());
        assert_eq!(compile_no_collector(source).unwrap(), compile_no_collector(&plain).unwrap());
    }

    #[test]
    fn a_generic_interface_every_way() {
        // `Source[T]` with classes that are `Source[int]`, `Source[str]`
        // and `Source[T]`, through the interface, in a list and as a
        // bound, and an `owned interface Holder[T]` with a default and a
        // function that takes out (decision 58.6). Every build prints
        // what `zeno run` prints, and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-generic-interface.zeno");
        no_collector_as_interpreter("generic-interface", source);
        // One `switch` for each copy of the interface, over the classes
        // that are that copy, and nothing for the generic itself.
        let c = compile(source).unwrap();
        for wanted in ["struct z_Source_int {", "struct z_Source_str {", "z_Source_int_next(z_Source_int *a0)"] {
            assert!(c.contains(wanted), "{wanted}\n{c}");
        }
        assert!(!c.contains("z_Source *") && !c.contains("struct z_Source {"), "{c}");
    }

    #[test]
    fn a_value_with_one_owner_fills_a_type_parameter_every_way() {
        // `same`, `Box`, `Stack`, `Pair` and `Slot` with a list, an object
        // and an enum value that carries values, a thousand rounds, and
        // what a copy prints (decision 56.11). Every build prints what
        // `zeno run` prints, and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-generic-owned.zeno");
        no_collector_as_interpreter("generic-owned", source);
        firmware_as_interpreter("generic-owned", source);
        crate::emit_llvm::tests::same_as_interpreter("generic-owned", source);
        // A copy is named as copies are, without `owned`, which says
        // nothing where everything has one owner.
        let c = compile_no_collector(
            "class Frame {\n    id: int\n}\nclass Stack[T] {\n    items: list[T]\n}\n\
             let s: Stack[Frame] = Stack([Frame(1)])\nlet t = Stack([[1]])\nprint(len(s.items), len(t.items))\n",
        )
        .unwrap();
        for name in ["zeno_drop_Stack_Frame(", "zeno_drop_Stack_list_int("] {
            assert!(c.contains(name), "{name}\n{c}");
        }
        // With a collector, an object of an `owned class` fills `T` of an
        // `owned class` copy, which frees it.
        let owned = "owned class Packet {\n    id: int\n}\nowned class Stack[T] {\n    items: owned list[T]\n\
                     function push(self, x: take T) { self.items.append(x) }\n}\n\
                     function same[T](x: take T) -> T { return x }\n\
                     let s: Stack[Packet] = Stack([])\nfor i in range(100) {\n    s.push(same(Packet(i)))\n}\n\
                     print(len(s.items), s.items[99].id)\n";
        // A kept function in a generic takes a list in one copy and a
        // number in another, and frees each by its kind (56.8, 56.11).
        let kept = "class Frame {\n    id: int\n}\n\
                    function hold[T](x: take T) -> function() -> str {\n    return function() -> str { return f\"{x}\" }\n}\n\
                    let a = hold([1, 2])\nlet b = hold(5)\nlet c = hold(Frame(3))\nprint(a(), b(), c())\n\
                    for i in range(1000) {\n    let d = hold([i])\n    if i == 999 {\n        print(d())\n    }\n}\n";
        no_collector_as_interpreter("generic-kept", kept);
        firmware_as_interpreter("generic-kept", kept);
        firmware_as_interpreter("generic-owned-class", owned);
        crate::emit_llvm::tests::same_as_interpreter("generic-owned-class", owned);
        no_collector_as_interpreter("generic-owned-class", owned);
    }

    #[test]
    fn a_dict_owns_its_values_every_way() {
        // A dict of objects, of lists and of enum values, each way a dict
        // is used, in a field and in a list, a thousand stores over one
        // key, and a dict made where a loop walks it (decision 56.12).
        // Every build prints what `zeno run` prints, and with no collector
        // nothing is left.
        let source = include_str!("../tools/targets/edge-dict-owned.zeno");
        no_collector_as_interpreter("dict-owned", source);
        firmware_as_interpreter("dict-owned", source);
        crate::emit_llvm::tests::same_as_interpreter("dict-owned", source);
        // Storing over a key frees what was there, and the end of the dict
        // frees each value, by functions written for the kind.
        let c = compile_no_collector(
            "class Frame {\n    id: int\n}\nlet d: dict[str, Frame] = [:]\nd[\"a\"] = Frame(1)\nd.clear()\nprint(len(d))\n",
        )
        .unwrap();
        for wanted in [
            "static void zeno_dict_put_values_Frame(zeno_dict *dict, zeno_slot key, zeno_slot value) {\n    \
             zeno_drop_Frame(((z_Frame *)zeno_dict_value(dict, zeno_dict_find(dict, key)).p));\n    \
             zeno_dict_set(dict, key, value);\n}",
            "static void zeno_dict_clear_values_Frame(zeno_dict *dict) {\n    \
             for (int64_t i = 0; i < dict->values->len; i++) zeno_drop_Frame(((z_Frame *)dict->values->items[i].p));\n    \
             zeno_dict_clear(dict);\n}",
            "zeno_dict_clear_values_Frame(z_d);",
            "zeno_dict_drop_values_Frame(z_d);",
        ] {
            assert!(c.contains(wanted), "{wanted}\n{c}");
        }
        // A list of dicts is named by what the dicts hold, so a list of
        // lists of ints and a list of dicts are two functions.
        let lists = "let a = [[1], [2]]\nlet b = [[\"x\": 1]]\nlet c = [[\"y\": [2]]]\nprint(a, b, c)\n";
        no_collector_as_interpreter("dict-lists", lists);
        let c = compile_no_collector(lists).unwrap();
        for wanted in ["zeno_list_drop_lists1(z_a)", "zeno_list_drop_dicts1(z_b)", "zeno_list_drop_dicts1_values_lists(z_c)"] {
            assert!(c.contains(wanted), "{wanted}\n{c}");
        }
        // With a collector a dict is shared, and cannot be the one owner
        // of an object of an `owned class`: refused as before.
        let shared = "owned class P {\n    n: int\n}\nlet d: dict[str, P] = [:]\nd[\"a\"] = P(1)\nprint(d[\"a\"].n)\n";
        assert!(compile(shared).unwrap_err().contains("keeps a dict where anything may share it"));
    }

    #[test]
    fn a_list_of_numbers_that_may_be_none_every_way() {
        // Ints, floats, u8s and bools that may be `none`, each way a list
        // is used (decision 56.14). Every build prints what `zeno run`
        // prints, and with no collector nothing is left.
        let source = include_str!("../tools/targets/edge-none-list.zeno");
        no_collector_as_interpreter("none-list", source);
        firmware_as_interpreter("none-list", source);
        crate::emit_llvm::tests::same_as_interpreter("none-list", source);
        // With no collector the list keeps each number in a box of its
        // own, in the pool, and frees the boxes with itself.
        let c = compile_no_collector(
            "var xs: list[int or none] = [1, none]\nxs.append(3)\nxs[0] = none\nlet x = xs.pop()\nlet ys = xs + xs\nprint(x, ys)\n",
        )
        .unwrap();
        for wanted in [
            "static void *zeno_box_keep_int(zeno_opt_int value) {\n    if (!value.present) return NULL;\n    \
             int64_t *box = (int64_t *)zeno_alloc_raw(sizeof(int64_t));\n    *box = value.value;\n    return box;\n}",
            "static zeno_opt_int zeno_box_take_int(void *box) {\n    zeno_opt_int value = zeno_unbox_int(box);\n    \
             zeno_free(box);\n    return value;\n}",
            "static void zeno_list_drop_boxes(zeno_list *list) {\n    if (list == NULL) return;\n    \
             for (int64_t i = 0; i < list->len; i++) zeno_free(list->items[i].p);\n    zeno_list_drop(list);\n}",
            "zeno_list_append(z_xs, (zeno_slot){.p = zeno_box_keep_int(",
            "zeno_free(t",
            "zeno_box_take_int(zeno_list_pop(z_xs, ",
            "zeno_list_keep_boxes_int(zeno_list_concat(z_xs, z_xs))",
            "zeno_list_drop_boxes(z_ys);",
        ] {
            assert!(c.contains(wanted), "{wanted}\n{c}");
        }
        // With a collector the list is as it was: a box in the text room
        // for each number, which the collector looks after.
        let c = compile("var xs: list[int or none] = [1, none]\nxs.append(3)\nprint(xs.pop())\n").unwrap();
        assert!(c.contains("zeno_list_append(z_xs, (zeno_slot){.p = zeno_box_int("), "{c}");
        assert!(!c.contains("zeno_box_keep_int"), "{c}");
        // A box is a block of the pool, so a list too long for the pool
        // stops the program with the message every full pool gives, in C
        // and in LLVM alike.
        let source = "var xs: list[int or none] = []\nprint(\"before\")\nfor n in range(5000) { xs.append(n) }\nprint(len(xs))\n";
        let full = "error: the memory pool is full (ZENO_POOL is 32768 bytes)\n";
        let Some((printed, errors, finished)) = build_and_run("none-list-full", &compile_no_collector(source).unwrap(), &[]) else {
            return;
        };
        assert_eq!((printed.as_str(), errors.as_str(), finished), ("before\n", full, false));
        let ir = crate::emit_llvm::tests::compile_no_collector(source).unwrap();
        let Some(ran) = crate::emit_llvm::tests::built_output("none-list-full", &ir, Build::NoCollector, &["-DZENO_MEMORY_REPORT"]) else { return };
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "before\n");
        // (Built with `--memory`, the report of what was taken follows.)
        assert!(String::from_utf8_lossy(&ran.stderr).starts_with(full));
        assert!(!ran.status.success());
    }

    #[test]
    fn a_parameter_that_may_be_none_is_read_after_its_test() {
        // A field of a parameter is read once, at the start, when nothing
        // in the function can change it (decision 22.8). Not one that may
        // be `none`: the read waits for the test, or the program reads
        // through NULL. 0.7.0 did, in every build.
        let source = "class Frame {\n    id: int\n}\n\
                      function id_of(f: Frame or none) -> int {\n    if f is none {\n        return -1\n    }\n    return f.id\n}\n\
                      let d: dict[str, Frame] = [\"a\": Frame(1)]\nprint(id_of(d.get(\"a\")), id_of(d.get(\"z\")), id_of(none))\n";
        firmware_as_interpreter("none-first", source);
        crate::emit_llvm::tests::same_as_interpreter("none-first", source);
        let c = compile(source).unwrap();
        assert!(c.contains("    if (z_f == NULL) {\n        return -1;\n    }\n    return z_f->z_id;\n"), "{c}");
    }

    #[test]
    fn each_copy_of_a_generic_makes_its_own_functions_raising() {
        // The same line hands `size` to a list of raising functions in one
        // copy and to a list of plain ones in the other (plan, Changes 2).
        let source = "function size(text: str) -> int {\n    return len(text)\n}\n\
                      function parse(text: str) -> int raises ValueError {\n    return int(text)\n}\n\
                      function fill[T](xs: list[T], n: int) -> int {\n    var ys: list[T] = []\n    ys.append(size)\n\
                      return len(ys) + len(xs) + n\n}\n\
                      let raising: list[function(str) -> int raises ValueError] = [parse]\n\
                      let plain: list[function(str) -> int] = [size]\n\
                      print(fill(raising, 1), fill(plain, 2))\n";
        firmware_as_interpreter("generic-raising", source);
        crate::emit_llvm::tests::firmware_as_interpreter("generic-raising", source);
    }

    #[test]
    fn a_fault_stops_a_firmware_build_where_an_ordinary_one_catches_it() {
        // The one place the two kinds of build differ, on purpose. The
        // `except` here takes anything. An ordinary build catches the
        // division by zero with it. A firmware build catches the errors
        // the program raises, and stops at the division.
        let source = "function parse(text: str) -> int raises ValueError { return int(text) }\nlet zero = 0\ntry {\n print(parse(\"1\"))\n print(1 // zero)\n} except {\n print(\"caught\")\n}\ntry { print(parse(\"x\")) } except { print(\"caught too\") }";
        let Some((printed, _, finished)) = build_and_run("fault", &compile(source).unwrap(), &[]) else {
            return;
        };
        assert_eq!(printed, "1\ncaught\ncaught too\n");
        assert!(finished);
        let (printed, errors, finished) = build_and_run("fault-firmware", &compile_firmware(source).unwrap(), &[]).unwrap();
        assert_eq!(printed, "1\n");
        assert!(!finished);
        assert_eq!(errors, "error: uncaught ZeroDivisionError: division by zero\n --> test.zeno:5:8\n");
    }

    // ------------------------------------------------------------ Memory

    /// Build with the report switched on and a low first limit, so that
    /// the collector runs often, and at -O2, which is what `zeno build`
    /// uses and where the C compiler is most inventive about where it
    /// keeps a pointer. Gives back what the program printed and the three
    /// numbers of the report: bytes made, most in use at once, collections.
    fn memory_of(name: &str, source: &str) -> Option<(String, [usize; 3])> {
        let c = compile(source).expect("the test program compiles");
        let flags = ["-O2", "-DZENO_MEMORY_REPORT", "-DZENO_FIRST_COLLECTION=16384"];
        let (printed, errors, finished) = build_and_run(name, &c, &flags)?;
        assert!(finished, "the compiled program failed: {errors}");
        let report = errors.lines().find(|line| line.starts_with("memory:")).expect("a memory report");
        let numbers: Vec<usize> = report
            .split(|c: char| !c.is_ascii_digit())
            .filter(|word| !word.is_empty())
            .map(|word| word.parse().expect("a number"))
            .collect();
        Some((printed, [numbers[1], numbers[2], numbers[3]]))
    }

    fn interpreted(source: &str) -> String {
        let program = crate::modules::testing::program(source);
        let (printed, result) = crate::vm::run_both(&program, source);
        assert!(result.is_ok(), "the interpreter failed");
        printed
    }

    #[test]
    fn memory_that_is_no_longer_used_is_given_back() {
        // Every trip round the loop makes text, objects, a list, a ring of
        // objects that point at each other, and a function value, and
        // keeps almost none of it.
        let source = r##"class Point {
    x: int
    label: str
}
class Node {
    value: int
    next: Node or none = none
    back: Node or none = none
}
function ring(n: int) -> Node {
    let first = Node(0)
    var last = first
    for i in range(1, n) {
        let node = Node(i)
        last.next = node
        node.back = last
        last = node
    }
    last.next = first
    first.back = last
    return first
}
function adder(n: int) -> function(int) -> int {
    return function(x: int) -> int { return x + n }
}
var kept: list[str] = []
var total = 0
for i in range(20000) {
    let text = f"item {i} of many"
    let pair = [Point(i, text), Point(i + 1, text.upper())]
    let add = adder(i)
    total += len(text) + pair[1].x + ring(5).value + add(1) - i
    if i % 5000 == 0 {
        kept.append(text)
    }
}
print(total, kept)
"##;
        let Some((printed, [made, most, collections])) = memory_of("garbage", source) else {
            return;
        };
        assert_eq!(printed, interpreted(source));
        assert!(made > 4_000_000, "the program made {made} bytes");
        assert!(collections > 20, "the collector ran {collections} times");
        // Without a collector `most` would equal `made`. (A block counts
        // as the size of its page's blocks, which rounds it up a little.)
        assert!(most < 400_000, "at most {most} bytes were in use at once, of {made} made");
    }

    #[test]
    fn memory_that_is_still_used_is_kept() {
        // The other half: things the program holds on to must come through
        // every collection untouched. A long chain (which also
        // shows the collector follows pointers without using the stack to
        // do it), a list in a top-level variable, a list reachable only
        // through an object, text in a list, and a function value that
        // remembers a variable.
        let source = r##"class Node {
    value: int
    next: Node or none = none
}
class Store {
    names: list[str]
    sizes: list[int or none]
}
function counter() {
    var count = 0
    return function() {
        count += 1
        return count
    }
}
function chain(n: int) -> Node or none {
    var head: Node or none = none
    for i in range(n) {
        head = Node(i, head)
    }
    return head
}
function churn(n: int) -> int {
    var seen = 0
    for i in range(n) {
        let scrap = [f"scrap {i}", f"{i * 2}"]
        seen += len(scrap[0]) + len(scrap)
    }
    return seen
}
let store = Store([], [])
let tick = counter()
let head = chain(30000)
var squares: list[int] = []
for i in range(2000) {
    squares.append(i * i)
    if i % 100 == 0 {
        store.names.append(f"name {i}")
        if i % 200 == 0 {
            store.sizes.append(none)
        } else {
            store.sizes.append(i)
        }
        tick()
    }
}
print(churn(30000))
var count = 0
var total = 0
var node = head
while node is not none {
    count += 1
    total += node.value
    node = node.next
}
print(count, total, len(squares), squares[1999], sum(squares))
print(store.names, store.sizes, tick())
"##;
        let Some((printed, [_, _, collections])) = memory_of("kept", source) else {
            return;
        };
        assert_eq!(printed, interpreted(source));
        // Fewer runs than in the test above, because the limit grows with
        // what is kept.
        assert!(collections > 5, "the collector ran {collections} times");
    }

    #[test]
    fn an_error_on_its_way_to_an_except_is_kept() {
        // Between `raise` and `except` the error is in no variable of the
        // program's, only in the runtime's own. The handlers here make
        // garbage before they look at what they caught.
        same_as_interpreter("raised", r##"class Fault {
    code: int
    notes: list[str]
}
function risky(i: int) -> int raises Fault {
    if i % 3 == 0 {
        raise Fault(i, [f"note {i}", f"again {i}"])
    }
    return i
}
var total = 0
var notes: list[str] = []
for i in range(30) {
    try {
        total += risky(i)
    } except Fault as fault {
        let filler = [f"filler {i}", f"more {i}"]
        total += fault.code + len(filler)
        notes.append(fault.notes[1])
    }
}
print(total, notes)
try {
    let n = int("not a number")
    print(n)
} except ValueError as err {
    let filler = [f"{total}", f"{len(notes)}"]
    print(err, filler)
}
"##);
    }

    // ----------------------------------------------------------- Owned values

    #[test]
    fn owned_values_match() {
        // Making, lending, giving away, replacing, and leaving a block
        // early with `continue`.
        same_as_interpreter(
            "owned",
            r##"owned class Packet {
    id: int
    data: owned list[int]

    function size(self) -> int {
        return len(self.data)
    }

    function add(self, value: int) {
        self.data.append(value)
    }
}

function total(xs: owned list[int]) -> int {
    var sum_of = 0
    for x in xs {
        sum_of += x
    }
    return sum_of
}

function make(id: int, count: int) -> Packet {
    let data: owned list[int] = [n * id for n in range(count)]
    return Packet(id, data)
}

function send(p: Packet) -> int {
    print(f"sending {p.id} with {p.size()} items, total {total(p.data)}")
    return p.size()
}

function describe(p: Packet) -> str {
    return f"packet {p.id}: {p.data}"
}

let first = make(1, 4)
first.add(99)
print(describe(first))
print(first)
let sent = send(first)
print(sent)

var buffer: owned list[int] = [1, 2, 3]
print(total(buffer), len(buffer), buffer[0])
buffer = [4, 5]
print(buffer)
let shared = [7, 8, 9]
print(total(shared))

for i in range(3) {
    let p = make(i + 1, 3)
    if i == 1 {
        print(send(p))
        continue
    }
    print(describe(p))
}
"##,
        );
    }

    #[test]
    fn owned_values_match_when_errors_and_early_exits_end_their_blocks() {
        // Owners in scope when a function raises, when a `try` catches,
        // and when `return`, `break` and `continue` leave from deep inside.
        same_as_interpreter(
            "owned-exits",
            r##"# Errors, early exits and owners.
owned class Frame {
    id: int
    bytes: owned list[int]

    function checksum(self) -> int {
        var total = 0
        for b in self.bytes {
            total = (total + b) % 256
        }
        return total
    }
}

owned class Link {
    name: str
    last: Frame
    sent: int
}

class Timeout {
    after: int
}

function build(id: int, size: int) -> Frame {
    return Frame(id, [(id * 7 + n) % 256 for n in range(size)])
}

function transmit(frame: Frame) -> int raises Timeout {
    if frame.id % 4 == 3 {
        raise Timeout(frame.id)
    }
    return frame.checksum()
}

function parse(text: str, into: owned list[int]) raises ValueError {
    for part in text.split(",") {
        into.append(int(part))
    }
}

function longest(a: owned list[int], b: owned list[int]) -> int {
    if len(a) > len(b) {
        return len(a)
    }
    return len(b)
}

function first_big(limit: int) -> Frame {
    var n = 0
    while true {
        let candidate = build(n, n)
        if candidate.checksum() > limit {
            return candidate
        }
        n += 1
    }
}

function drain(frames: int) -> int {
    var good = 0
    for id in range(frames) {
        let frame = build(id, 5)
        if id % 5 == 4 {
            continue
        }
        try {
            let sum = transmit(frame)
            good += 1
            print(f"frame {id} ok, checksum {sum}")
        } except Timeout as t {
            print(f"frame {t.after} timed out")
        }
    }
    return good
}

print(drain(10))

let numbers: owned list[int] = []
try {
    parse("1,2,3", numbers)
    parse("4,five,6", numbers)
} except ValueError as e {
    print("bad:", e.message)
}
print(numbers, longest(numbers, [1, 2]))

let big = first_big(100)
print(big.id, big.checksum(), len(big.bytes))

var link = Link("uart0", build(1, 3), 0)
print(link)
link.last = build(2, 4)
link.sent += 1
print(link.last.checksum(), link)
link = Link("uart1", build(9, 2), 0)
print(link.name, link.last)

function swap_in(link: take Link, frame: take Frame) -> Link {
    link.last = frame
    link.sent += 1
    return link
}
let again = swap_in(link, build(5, 1))
print(again)

var kept: owned list[str] = ["a"]
for round in range(4) {
    let next: owned list[str] = [s + str(round) for s in kept]
    next.append("x")
    kept = next
}
print(kept)
"##,
        );
        // A value moved into a function that frees it and then raises;
        // owners in `match` cases, in a function value, in nested loops.
        same_as_interpreter(
            "owned-raises",
            r##"owned class Buf {
    tag: str
    data: owned list[int]
}

class Refused {
    why: str
}

enum Mode {
    Fast
    Slow(factor: int)
}

function fresh(tag: str, n: int) -> Buf {
    return Buf(tag, [i for i in range(n)])
}

# Takes the buffer, frees it itself, and then raises.
function swallow(b: take Buf, fail: bool) -> int raises Refused {
    let size = len(b.data)
    if fail {
        let inner = b
        print("swallowed", inner.tag)
        raise Refused("too big: " + str(size))
    }
    return size
}

function attempt(n: int, fail: bool) -> str {
    let b = fresh("b" + str(n), n)
    let spare = fresh("spare", 2)
    var result = "none"
    try {
        let got = swallow(b, fail)
        result = "took " + str(got)
    } except Refused as r {
        result = r.why
    }
    return result + " / " + spare.tag
}

function takes_and_tries(b: take Buf, fail: bool) -> int {
    try {
        return swallow(b, fail)
    } except Refused {
        return -1
    }
}

function regrow(b: take Buf) -> Buf {
    b.data.append(len(b.data))
    return b
}

function by_mode(mode: Mode) -> int {
    match mode {
        case Fast {
            let b = fresh("fast", 3)
            return len(b.data)
        }
        case Slow(factor) {
            let b = fresh("slow", factor)
            if factor > 3 {
                return -len(b.data)
            }
            print("slow", b.tag)
        }
    }
    return 0
}

function counter() -> function() -> int {
    var count = 0
    return function() -> int {
        let scratch: owned list[int] = [count, count + 1]
        count += len(scratch)
        return count
    }
}

function search(limit: int) -> int {
    var found = -1
    for i in range(limit) {
        let outer = fresh("o", i)
        var j = 0
        while j < 3 {
            let inner = fresh("i", j)
            j += 1
            if len(inner.data) + len(outer.data) == 4 {
                found = i * 10 + j
                break
            }
            if j == 2 {
                continue
            }
            inner.data.append(1)
        }
        if found >= 0 {
            break
        }
    }
    return found
}

print(attempt(3, false))
print(attempt(4, true))
print(takes_and_tries(fresh("t", 5), false), takes_and_tries(fresh("t", 6), true))

var b = fresh("grow", 1)
for i in range(3) {
    b = regrow(b)
}
print(b)

print(by_mode(Fast), by_mode(Slow(2)), by_mode(Slow(5)))
let next = counter()
print(next(), next(), next())
print(search(6))

fresh("nobody", 3)
Buf("nobody either", [])

if len(b.data) > 2 {
    let local = fresh("local", 2)
    try {
        print(swallow(local, true))
    } except Refused as r {
        print("top level:", r.why)
    }
}

var count = 0
function bump() -> int {
    var count = 10
    let adder = function() -> int {
        count += 1
        return count
    }
    return adder()
}
var scratch: owned list[int] = [1]
scratch = [n for n in range(bump())]
print(len(scratch), count)
"##,
        );
    }

    #[test]
    fn an_owned_value_is_freed_without_the_collector() {
        // The same work twice. With shared lists the collector has to run
        // to get the memory back. With owned lists it never needs to.
        let work = |list_type: &str| {
            format!(
                "function work(rounds: int) -> int {{
    var total = 0
    for round in range(rounds) {{
        let samples: {list_type} = [n * round for n in range(500)]
        total += samples[499] % 7
    }}
    return total
}}
print(work(400))
"
            )
        };
        let Some((shared_printed, [_, shared_most, shared_collections])) = memory_of("shared-work", &work("list[int]"))
        else {
            return;
        };
        let Some((owned_printed, [made, owned_most, owned_collections])) =
            memory_of("owned-work", &work("owned list[int]"))
        else {
            return;
        };
        assert_eq!(owned_printed, shared_printed);
        assert_eq!(owned_printed, interpreted(&work("owned list[int]")));
        assert!(made > 2_000_000, "the program made {made} bytes");
        assert!(shared_collections > 20, "with shared lists the collector ran {shared_collections} times");
        assert_eq!(owned_collections, 0, "with owned lists the collector should have nothing to do");
        assert!(owned_most < 8_192, "at most {owned_most} bytes were in use at once");
        assert!(owned_most < shared_most);
    }

    #[test]
    fn the_c_for_an_owned_value() {
        let source = "owned class Frame {
    id: int
    bytes: owned list[int]
}
class Refused {
    id: int
}
function check(frame: Frame) -> int raises Refused {
    if frame.id > 9 {
        raise Refused(frame.id)
    }
    return len(frame.bytes)
}
function send(frame: take Frame) -> int raises Refused {
    let size = check(frame)
    return size + 1
}
function main() -> int raises Refused {
    let frame = Frame(1, [1, 2, 3])
    return send(frame)
}
";
        let c = compile(source).expect("the program compiles");
        // An object of an owned class frees what its fields own, then itself.
        assert!(c.contains("static void zeno_drop_Frame(z_Frame *self) {\n    if (self == NULL) return;\n    zeno_list_drop(self->z_bytes);\n    zeno_free(self);\n}"), "{c}");
        // Giving a value away takes the pointer and leaves NULL behind.
        assert!(c.contains(" = z_frame;\n    z_frame = NULL;\n"), "{c}");
        // The one that took it over frees it before it returns.
        assert!(c.contains("    zeno_drop_Frame(z_frame);\n    return t"), "{c}");
        // A value that is looked at is an ordinary pointer, and nothing frees it.
        let check = c.split("static int64_t z_check(z_Frame *z_frame) {").nth(1).expect("the C for `check`");
        assert!(!check.split("\n}\n").next().expect("its body").contains("zeno_drop"), "{c}");

        // In a firmware build, a failed call frees on its way out too.
        let c = compile_firmware(source).expect("the program compiles for firmware");
        assert!(c.contains(")) { zeno_drop_Frame(z_frame); return false; }"), "{c}");
    }

    // ------------------------------------------------ A build with no collector

    /// The same, as `zeno build --no-collector` compiles it.
    fn compile_no_collector(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        if let Some(error) = errors.first() {
            // Who owns what is the checker's to say in this build.
            return Err(error.message.clone());
        }
        emit(&program, &types, "test.zeno", Build::NoCollector).map_err(|error| error.message)
    }

    /// A list field with a default: the new object owns the list made
    /// for it, as it owns one written in the call. (The list was freed
    /// when the statement ended, with the object still pointing at it.)
    /// The same for a parameter that takes its list.
    #[test]
    fn a_default_list_belongs_to_whoever_keeps_it() {
        let source = "class Bag {\n    items: list[str] = []\n    count: int = 0\n}\n\
                      function made() -> Bag {\n    return Bag()\n}\n\
                      function length(numbers: take list[int] = []) -> int {\n    return len(numbers)\n}\n\
                      let bag = Bag()\nbag.items.append(\"x\")\nbag.items.append(\"y\")\nprint(bag)\n\
                      let bags: list[Bag] = [Bag(), made(), Bag([\"z\"])]\nbags[0].items.append(\"first\")\n\
                      bags[1].items.append(\"second\")\nprint(bags)\n\
                      print(length(), length([1, 2]))\n";
        assert_eq!(
            interpreted(source),
            "Bag(items=[\"x\", \"y\"], count=0)\n\
             [Bag(items=[\"first\"], count=0), Bag(items=[\"second\"], count=0), Bag(items=[\"z\"], count=0)]\n0 2\n"
        );
        no_collector_as_interpreter("default-list", source);
        // No list is made beside the call and freed after it.
        let c = compile_no_collector(source).unwrap();
        assert!(c.contains("z_bag = zeno_new_Bag(zeno_list_new(0), 0);"), "{c}");
    }

    /// A program that a build with no collector must accept. Built that
    /// way it must print what the interpreter prints, and it must end
    /// with every block it made given back: with no collector, a block
    /// that is not freed by some line of the program is never freed.
    /// Returns the most text that was in the text room at once.
    fn no_collector_as_interpreter(name: &str, source: &str) -> Option<usize> {
        let c = match compile_no_collector(source) {
            Ok(c) => c,
            Err(message) => {
                // Say where, when it is the checker that refused.
                let program = crate::modules::testing::built(source);
                let (errors, _) = crate::checker::check_by(&program, Build::NoCollector.rules());
                let place = errors.first().map(|error| crate::modules::testing::place(&program, error.span));
                panic!("the build with no collector refused this: {message}\nat {}", place.unwrap_or_default())
            }
        };
        // The point of the build: none of this is in the C file.
        for gone in ["zeno_collect", "zeno_roots", "zeno_stack_base", "zeno_pages", "setjmp"] {
            assert!(!c.contains(gone), "`{gone}` is still in a build with no collector");
        }
        // Nor is the heap: lists, objects and errors come from the pool.
        assert_eq!(crate::emit_llvm::tests::asks_the_heap(&c), None, "a build with no collector uses the heap");
        let name = format!("{name}-no-collector");
        let (compiled, errors, finished) = build_and_run(&name, &c, &["-O1", "-DZENO_MEMORY_REPORT"])?;
        assert!(finished, "the compiled program failed: {errors}");
        assert_eq!(compiled, interpreted(source), "\n{source}");
        assert!(errors.contains("no collector, 0 blocks left at the end\n"), "blocks were never freed: {errors}");
        // `memory owned` on a computer is the same build with a pool and
        // a text room that grow. Starting both tiny makes the program
        // grow them many times over, which is where a mistake would be.
        grown_as_interpreter(&name, source, &compiled, &errors);
        let text = errors.lines().find(|line| line.starts_with("text:")).expect("a line about the text room");
        let most = text.split(|c: char| !c.is_ascii_digit()).find(|word| !word.is_empty()).expect("a number");
        // A program with no shared values is still a Zeno program: the
        // two builds that do have a collector must agree as well.
        same_as_interpreter(&name, source);
        // And the build with no collector through LLVM must use memory
        // exactly as this one did: the same report, line for line.
        crate::emit_llvm::tests::no_collector_as_c(&name, source, &compiled, &errors);
        Some(most.parse().expect("a number"))
    }

    /// The program built as `memory owned` builds it on a computer: no
    /// collector, and memory that grows. It must print what the fixed
    /// build printed, and make and free the same blocks.
    fn grown_as_interpreter(name: &str, source: &str, printed: &str, report: &str) {
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        let options = Options { release: false, grows: true, fast: false, ..Default::default() };
        let c = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
        assert!(c.contains("zeno_pool_cut") && !c.contains("zeno_pool.bytes"), "the pool does not grow");
        let flags = ["-O1", "-DZENO_MEMORY_REPORT", "-DZENO_POOL=64", "-DZENO_TEXT_ROOM=64"];
        let Some((compiled, errors, finished)) = build_and_run(&format!("{name}-grown"), &c, &flags) else { return };
        assert!(finished, "the program built with `memory owned` failed: {errors}");
        assert_eq!(compiled, printed, "\n{source}");
        // The first line counts blocks and bytes, which do not depend on
        // where the blocks are.
        assert_eq!(errors.lines().next(), report.lines().next(), "\n{source}");
    }

    #[test]
    fn with_memory_owned_the_pool_and_the_text_room_grow() {
        // Far more than either a fixed pool or a fixed text room of the
        // first sizes could hold, in many sizes of block and of text.
        let source = "var words: list[str] = []\nfor i in range(20000) {\n    words.append(f\"word {i}\")\n}\n\
                      let all = words.join(\" \")\nprint(len(words), len(all))\nvar long = \"\"\n\
                      for i in range(300) {\n    long = long + str(i)\n}\nprint(len(long), long.slice(0, 10))";
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        assert!(errors.is_empty(), "{:?}", errors[0].message);
        let fixed = emit(&program, &types, "test.zeno", Build::NoCollector).expect("it builds");
        let room = ["-O1", "-DZENO_MEMORY_REPORT", "-DZENO_POOL=8000000", "-DZENO_TEXT_ROOM=1000000"];
        let Some((printed, report, finished)) = build_and_run("grows-fixed", &fixed, &room) else { return };
        assert!(finished, "{report}");
        assert_eq!(printed, interpreted(source));
        assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
        grown_as_interpreter("grows", source, &printed, &report);
    }

    #[test]
    fn with_no_collector_errors_and_early_exits_free_everything() {
        no_collector_as_interpreter("errors-and-exits", NO_COLLECTOR_ERRORS);
    }

    #[test]
    fn with_no_collector_a_failure_half_way_frees_everything() {
        no_collector_as_interpreter("half-way", NO_COLLECTOR_HALF_WAY);
    }

    /// Errors, early exits and text.
    const NO_COLLECTOR_ERRORS: &str = r#"# Errors, early exits and text, with no collector to tidy up after them.
owned class Packet {
    id: int
    name: str
    bytes: owned list[int]

    function total(self) -> int {
        var sum_ = 0
        for b in self.bytes {
            sum_ += b
        }
        return sum_
    }

    function first_over(self, limit: int) -> int {
        for b in self.bytes {
            if b > limit {
                return b              # leaves from inside the loop
            }
        }
        return -1
    }

    function check(self) -> int raises BadPacket {
        for b in self.bytes {
            if b < 0 {
                raise BadPacket(self.id, "negative byte")
            }
        }
        return len(self.bytes)
    }
}

class BadPacket {
    id: int
    why: str

    function label(self) -> str {
        if self.id > 100 {
            return "big"
        }
        return "small"
    }
}

class Fatal {
    code: int
}

function make(id: int, size: int) -> Packet {
    let bytes: owned list[int] = [(n * 7 + id) % 50 - (id % 3) for n in range(size)]
    return Packet(id, "pkt", bytes)
}

function risky(n: int) -> int raises BadPacket {
    if n % 4 == 3 {
        raise BadPacket(n, "risky")
    }
    return n * 2
}

function consume(p: Packet, extra: int) -> int {
    return p.total() + extra
}

function name_of(code: int) -> str {
    if code == 0 {
        return "idle"
    } else if code == 1 {
        return "busy"
    }
    return "unknown"
}

function parse(text: str) -> int raises ValueError {
    let n = int(text)
    if n > 1000 {
        raise ValueError(f"{n} is more than {1000}")
    }
    return n
}

function squares(count: int) -> owned list[int] raises BadPacket {
    return [risky(n) for n in range(count)]
}

function process(id: int) -> int raises BadPacket, Fatal {
    let p = make(id, 6)
    if id == 9 {
        raise Fatal(id)                    # p is freed on the way out
    }
    let checked = p.check()
    return consume(p, risky(id)) + checked # p is held while risky runs
}

var good = 0
var bad = 0
for id in range(12) {
    try {
        good += process(id)
    } except BadPacket as e {
        bad += 1
        print(f"bad packet {e.id}: {e.why} ({e.label()})")
        if e.id == 7 {
            continue
        }
        print("  noted")
    } except Fatal as f {
        print(f"fatal {f.code}")
    }
}
print(good, bad)

# An error raised while another is being handled.
function twice(n: int) -> int raises BadPacket, ValueError {
    try {
        return risky(n)
    } except BadPacket as first {
        if first.id > 10 {
            raise ValueError(f"gave up on {first.id}")
        }
        return risky(n + 4)
    }
}

for n in [3, 5, 11, 2] {
    try {
        print(twice(n))
    } except BadPacket as e {
        print(f"still bad: {e.id}")
    } except ValueError as e {
        print(f"value error: {e}")
    }
}

# A list that fails while it is being filled.
for count in [3, 5] {
    try {
        let s = squares(count)
        print(s, len(s))
    } except BadPacket as e {
        print(f"squares({count}) stopped at {e.id}")
    }
}

# Text: built, compared and printed, never kept.
let words: owned list[str] = ["On", "off", " ON ", "7", "x9", "2000"]
var on = 0
for word in words {
    if word.strip().upper() == "ON" {
        on += 1
    } else if word.lower() == "off" {
        print("off seen")
    } else {
        try {
            print(f"number {parse(word):>6} from {word}")
        } except ValueError as e {
            print(f"not usable: {e}")
        }
    }
}
print(f"on {on} times, {name_of(1)} {name_of(5)}")

var i = 0
while str(i * i) != "49" {
    i += 1
}
print(i, 3.5, 2.0 / 3.0, f"{i / 3:.3f}", true, none)

let label = name_of(0)
var count_e = 0
for k in range(len(label)) {
    if label[k] == "e" or label[k] == "i" {
        count_e += 1
    }
}
print(label, count_e, len(label + "!"), "dl" in label, label < "idler")

# Moves into calls that come to nothing, breaks and whiles.
var round = 0
while true {
    let p = make(round, 3 + round)
    if round == 4 {
        break
    }
    round += 1
    if p.first_over(40) > 0 {
        continue
    }
    print(consume(p, round))
}
print(round)
"#;

    /// Failures that happen while a list is being filled, while owned
    /// values are on their way into a call, and while text is being built.
    const NO_COLLECTOR_HALF_WAY: &str = r#"# Failures that happen half way through something.
owned class Batch {
    tag: str
    values: owned list[int]

    function sum_until(self, stop: int) -> int raises TooBig {
        var total = 0
        for v in self.values {
            if v == stop {
                raise TooBig(v, self.tag)      # the loop's list is freed too
            }
            total += v
        }
        return total
    }

    function grow(self, by: int) {
        for n in range(by) {
            self.values.append(n)
        }
    }
}

class TooBig {
    value: int
    tag: str
}

enum Level {
    Low
    High
}

function level(n: int) -> Level {
    if n > 5 {
        return High
    }
    return Low
}

function check(n: int) -> int raises TooBig {
    if n >= 6 {
        raise TooBig(n, "check")
    }
    return n
}

function batch(tag: str, size: int) -> Batch raises TooBig {
    return Batch(tag, [check(n) for n in range(size)])
}

function join(a: Batch, b: Batch, extra: int) -> int {
    return len(a.values) + len(b.values) + extra
}

function total(xs: owned list[int]) -> int {
    var t = 0
    for x in xs {
        t += x
    }
    return t
}

function describe(n: int) -> int raises TooBig {
    # Text is in the making when `check` fails.
    print(f"n is {n}, checked {check(n)}, level {level(n)}")
    return n
}

function deep(n: int) -> int {
    if n == 0 {
        return 0
    }
    print(f"depth {n}: {deep(n - 1)}")
    return n
}

var failures = 0
for round in range(300) {
    try {
        let a = batch("a", round % 9)
        let b = batch("b", 3)
        # `a` and `b` are on their way into `join` when `check` fails.
        failures += join(a, b, check(round % 8)) * 0
    } except TooBig as e {
        failures += 1
    }
    try {
        let c = batch("c", 5)
        c.grow(round % 4)
        failures += c.sum_until(round % 7) * 0
    } except TooBig as e {
        if e.tag != "c" {
            print("wrong tag")
        }
        failures += 1
    }
    try {
        if round % 50 == 0 {
            failures += describe((round // 50) * 2) * 0
        }
    } except TooBig as e {
        print(f"round {round}: {e.value} from {e.tag}")
    }
}
print(failures)
print(deep(4))

let big: owned list[int] = [n * n for n in range(40)]
print(total(big), big.reduce(larger, 0), level(3), level(9) == High)
match level(total(big)) {
    case Low {
        print("low")
    }
    case High {
        print("high")
    }
}

function larger(a: int, b: int) -> int {
    if a > b {
        return a
    }
    return b
}

function first_even(xs: owned list[int]) -> int or none {
    for x in xs {
        if x % 2 == 0 and x > 0 {
            return x
        }
    }
    return none
}

let found = first_even(big)
if found is not none {
    print(f"first even square: {found}")
}
let odd: owned list[int] = [1, 3, 5]
print(first_even(odd) is none)
"#;

    #[test]
    fn with_no_collector_text_is_given_back_after_each_statement() {
        // Ten thousand lines of text are built, a few bytes at a time,
        // in a loop, a comprehension, a condition and a `while`. If any
        // statement left its text behind, the room would overflow.
        let source = r#"owned class Reading {
    tick: int
    celsius: float
}

function label(tick: int) -> str {
    if tick % 2 == 0 {
        return "even"
    }
    return "odd"
}

function width(n: int) -> int {
    return len(str(n))
}

var shown = 0
var longest = 0
for tick in range(2000) {
    let reading = Reading(tick, 20.0 + tick % 7)
    if f"{reading.tick}" == "1999" or tick % 500 == 0 {
        print(f"tick {reading.tick:>5}: {reading.celsius:.1f} C, {label(tick)}", reading)
        shown += 1
    }
    let widths: owned list[int] = [width(n) + len(f"<{n}>") for n in range(tick % 5) if str(n) != "3"]
    longest = max(longest, len(widths), width(tick))
}
var n = 1
while len(str(n)) < 6 {
    n *= 10
}
print(shown, longest, n, label(shown).upper() + "!")
"#;
        let Some(most_text) = no_collector_as_interpreter("text", source) else { return };
        assert!(most_text < 200, "text is piling up in the text room: {most_text} bytes");

        // What the C for one such statement looks like.
        let c = compile_no_collector("let count = 3\nprint(f\"count: {count}\")\nprint(\"done\")").unwrap();
        assert!(
            c.contains(
                "    size_t m1 = zeno_text_top;\n    \
                 zeno_print(1, zeno_join(2, \"count: \", zeno_str_int(z_count)));\n    \
                 zeno_text_top = m1;\n    \
                 zeno_print(1, \"done\");\n"
            ),
            "{c}"
        );
    }

    #[test]
    fn with_no_collector_the_text_room_has_an_end() {
        let source = "let squares: owned list[int] = [n * n for n in range(40)]\nprint(\"before\")\nprint(squares)";
        let c = compile_no_collector(source).unwrap();
        let Some((printed, errors, finished)) = build_and_run("small-room", &c, &["-DZENO_TEXT_ROOM=64"]) else {
            return;
        };
        assert!(!finished);
        assert_eq!(printed, "before\n");
        assert_eq!(errors, "error: a statement built more text than there is room for (ZENO_TEXT_ROOM is 64 bytes)\n");
        // Showing a list gives back each half-built piece as it goes, so
        // the room needs about twice the finished text, not far more.
        let Some((printed, _, finished)) = build_and_run("room", &c, &["-DZENO_TEXT_ROOM=400"]) else { return };
        assert!(finished);
        assert_eq!(printed, interpreted(source));
    }

    /// The number in the line `pool: 2104 bytes taken, in a pool of 32768`
    /// of a memory report: how much of the pool was ever cut into blocks.
    fn pool_needed(report: &str) -> usize {
        let line = report.lines().find(|line| line.starts_with("pool:")).expect("a line about the pool");
        let number = line.split(|c: char| !c.is_ascii_digit()).find(|word| !word.is_empty()).expect("a number");
        number.parse().expect("a number")
    }

    #[test]
    fn with_no_collector_the_pool_has_an_end() {
        let source = "var items: owned list[int] = []\nprint(\"before\")\nfor n in range(100) { items.append(n) }\nprint(len(items))";
        let c = compile_no_collector(source).unwrap();
        // A pool that is too small stops the program, and says what to change.
        let Some((printed, errors, finished)) = build_and_run("small-pool", &c, &["-DZENO_POOL=1024"]) else {
            return;
        };
        assert!(!finished);
        assert_eq!(printed, "before\n");
        assert_eq!(errors, "error: the memory pool is full (ZENO_POOL is 1024 bytes)\n");
        // The report says how much of the pool the program took...
        let Some((_, report, finished)) = build_and_run("pool", &c, &["-DZENO_MEMORY_REPORT"]) else { return };
        assert!(finished);
        let needed = pool_needed(&report);
        assert!(needed > 1024 && needed < 4096, "{report}");
        // ...and that is exact: it runs in a pool of that size, and not
        // in one that is a byte smaller. The sizes are given as
        // `--pool` gives them to the C compiler (decision 56.15).
        let pool = |bytes: usize| crate::Sizes { pool: Some(bytes as u32), text_room: None }.defines();
        let exact = pool(needed);
        let Some((printed, _, finished)) = build_and_run("exact-pool", &c, &[&exact[0]]) else { return };
        assert!(finished);
        assert_eq!(printed, interpreted(source));
        let short = pool(needed - 1);
        let Some((_, errors, finished)) = build_and_run("short-pool", &c, &[&short[0]]) else { return };
        assert!(!finished);
        assert_eq!(errors, format!("error: the memory pool is full (ZENO_POOL is {} bytes)\n", needed - 1));
    }

    #[test]
    fn with_memory_a_write_to_a_block_given_back_stops_the_program() {
        // `free(a)` gives the block back, and the next line writes to it.
        // With `--memory` the block was filled as it was given back, and
        // is looked at when `allocate` hands it out again (decision 56.15).
        let into_the_block = "let a = allocate(u8, 16)\nunsafe {\n    free(a)\n    a[12] = 1\n}\nlet b = allocate(u8, 16)\nprint(\"not here\")\n";
        // The first byte is the link to the next spare block: the link is
        // looked at too, before it is followed.
        let into_the_link = "let a = allocate(u8, 16)\nlet c = allocate(u8, 16)\nunsafe {\n    free(c)\n    free(a)\n    a[0] = 1\n}\n\
                             let b = allocate(u8, 16)\nprint(\"not here\")\n";
        let said = "error: a block of memory was written to after it was given back\n";
        let checked = |source: &str| {
            let program = crate::modules::testing::built(source);
            let (errors, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
            assert!(errors.is_empty());
            let options = Options { spares_checked: true, ..Default::default() };
            emit_with(&program, &types, "test.zeno", Build::NoCollector, options).unwrap()
        };
        for (name, source) in [("into-the-block", into_the_block), ("into-the-link", into_the_link)] {
            let c = checked(source);
            let Some((printed, errors, finished)) = build_and_run(&format!("spare-{name}"), &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert_eq!((printed.as_str(), finished), ("", false), "{errors}");
            assert!(errors.starts_with(said), "{errors}");
            let ir = crate::emit_llvm::tests::compile_no_collector(source).unwrap();
            let runtime = crate::emit_llvm::runtime_with_spares_checked(false);
            let ran = crate::emit_llvm::tests::built_beside(&format!("spare-{name}"), &ir, Build::NoCollector, &runtime, &["-DZENO_MEMORY_REPORT"])
                .expect("clang, as above");
            assert!(String::from_utf8_lossy(&ran.stderr).starts_with(said) && !ran.status.success());
            // The same C, built without `--memory`, has none of it.
            let Some((printed, _, finished)) = build_and_run(&format!("spare-{name}-unchecked"), &c, &[]) else { return };
            assert_eq!((printed.as_str(), finished), ("not here\n", true));
        }
        // It is written only into a build that asks, so the C of every
        // other build is what it was, and it is the only difference.
        let c = checked(into_the_block);
        let plain = compile_no_collector(into_the_block).unwrap();
        assert!(c.contains("zeno_spare_check(block);") && !plain.contains("zeno_spare_check"));
        assert_eq!(with_spares_checked(&plain), c);
        // A program that gives blocks back and never writes to them runs
        // as it did, with nothing left.
        let source = include_str!("../tools/targets/edge-none-list.zeno");
        let Some((printed, report, finished)) = build_and_run("spare-fine", &checked(source), &["-DZENO_MEMORY_REPORT"]) else { return };
        assert!(finished && report.contains(" 0 blocks left at the end"), "{report}");
        assert_eq!(printed, interpreted(source));
    }

    #[test]
    fn with_no_collector_the_pool_does_not_fill_up_over_time() {
        // Lists of forty different lengths, and an object for each, made
        // and given back over and over. A pool that broke up into pieces,
        // or lost a block now and then, would need more for more trips.
        let program = |trips: usize| {
            format!(
                "owned class Frame {{
    id: int
    bytes: owned list[int]
}}

var total = 0
for trip in range({trips}) {{
    let bytes: owned list[int] = [n for n in range(trip % 40)]
    let frame = Frame(trip, bytes)
    total = total + len(frame.bytes)
}}
print(total)
"
            )
        };
        let mut needed = Vec::new();
        for trips in [80, 4000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("pool-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "fifty times the trips needed a bigger pool");
        assert!(needed[0] < 2048, "{needed:?}");
    }

    #[test]
    fn the_pool_gives_a_block_back_to_its_own_size() {
        // The pool on its own, driven from C: what the comments in
        // runtime.c say it does.
        let c = runtime(true, false)
            + r#"
int main(void) {
    char *a = zeno_alloc(24);  /* fits the 32-byte size */
    char *b = zeno_alloc(100); /* fits the 128-byte size */
    /* Lined up for an int or a float, and 8 bytes of note in front of each. */
    printf("%d %d\n", (int)((uintptr_t)a % 8), (int)(b - a));
    memset(a, 'x', 24);
    zeno_free(a);
    /* The next request of that size gets the same place, wiped. */
    char *c = zeno_alloc(17);
    printf("%d %d\n", c == a, c[0] == 0 && c[23] == 0);
    /* Another size does not: it is cut from what has never been used. */
    char *d = zeno_alloc(16);
    printf("%d %d\n", d == b + 128 + 8, (int)zeno_pool_top);
    /* NULL is nothing to give back. A block given back twice is caught. */
    zeno_free(NULL);
    zeno_free(d);
    zeno_free(d);
    printf("not reached\n");
    return 0;
}
"#;
        let Some((printed, errors, finished)) = build_and_run("pool-alone", &c, &[]) else { return };
        assert!(!finished);
        assert_eq!(printed, "0 40\n1 1\n1 200\n");
        assert_eq!(errors, "error: a block of memory was given back twice\n");
    }

    // ------------------------------------------------------------ Kept text

    #[test]
    fn with_no_collector_text_is_kept_by_copy() {
        // Every line that used to be refused because "this text is built
        // while the program runs", and the lines beside them: text kept
        // in a variable, a field and a list, given to a function, given
        // back by one, matched on, and read from a place that a function
        // changes in the middle of the statement.
        no_collector_as_interpreter("text-kept", r#"# The ten lines that used to be refused, and their neighbours.
owned class Packet {
    id: int
    name: str
    bytes: owned list[int]
}
class Bad {
    id: int
    why: str
}
function risky(n: int) -> int raises Bad {
    if n > 2 { raise Bad(n, "big " + str(n)) }
    return n
}
function name_len(text: str) -> int { return len(text) }
function f(n: int) -> str { return str(n) }
function join(a: str, b: str) -> str { return a + b }
function longer(a: str, b: str) -> str {
    if len(a) > len(b) { return a }
    return b
}
var state = "start"
function advance() -> str {
    state = state + "+"
    return state
}
function echo(text: str) -> str {
    state = "changed by echo"
    return text
}
let x = 7
let s = f"{x}"
print(s)
print(f(1))
let p = Packet(1, "a", [1])
p.name = "a" + "b"
print(p, p.name)
let w: owned list[str] = ["a"]
w.append(str(x))
w.append(p.name)
w[0] = w[1] + w[2]
print(w, len(w), "7" in w, "8" in w)
let v: owned list[str] = [str(n) for n in range(3)]
print(v)
print(name_len(str(x)))
let c = "abc"[0]
print(c)
var t = "a"
t += "b"
t = t + t
print(t)
var keep = ""
try { print(int("x")) } except ValueError as e { keep = str(e) }
print(keep)
try { print(risky(5)) } except Bad as e { keep = e.why + "!" }
print(keep)
for i in range(len(str(x * 100))) { print(i) }
print(state, advance(), state)
print(echo(state), state)
print(join(f(1), f(2)), join(join("a", f(3)), longer(t, state)))
print(longer(state, t), longer(p.name, w[0]))
function fold(a: str, b: str) -> str { return a + "," + b }
print(v.reduce(fold, ""))
var maybe: str or none = none
print(maybe)
maybe = f"{x}!"
if maybe is not none { print(maybe + "?") }
maybe = none
print(maybe)
function first(names: owned list[str]) -> str { return names[0] }
function pick(n: int) -> str or none {
    if n > 1 { return f"n{n}" }
    return none
}
print(first(w), first(v), pick(1), pick(2))
let chosen = pick(3)
print(chosen)
match f(42) {
    case "42" { print("forty-two") }
    case _ { print("other") }
}
match join("a", "b") {
    case "x" { print("x") }
    case other { print("got", other, other + other) }
}
var count = 0
while count < 3 {
    let label = f"trip {count}"
    if count == 1 {
        count += 1
        continue
    }
    print(label)
    count += 1
}
print(w.pop(), w)
"#);
    }

    #[test]
    fn with_no_collector_kept_text_is_freed_on_every_way_out() {
        // A raised object that keeps text, a function that fails half way
        // with text in hand, `break`, `continue` and an early `return`
        // inside loops over text, and a function that calls itself.
        no_collector_as_interpreter("text-ways-out", r#"# Errors, failures half way, and text in the hard places.
class Refused {
    channel: int
    reason: str
}

owned class Reading {
    name: str
    values: owned list[int]
    note: str or none

    function tag(self) -> str {
        if self.note is none { return self.name }
        return f"{self.name} [{self.note}]"
    }
}

function name_of(channel: int) -> str raises Refused {
    if channel % 5 == 4 { raise Refused(channel, f"channel {channel} is off") }
    if channel == 0 { return "zero" }
    return f"ch{channel}"
}

function read(channel: int) -> Reading raises Refused {
    let name = name_of(channel)
    let values: owned list[int] = [channel, channel * 2]
    let other = name_of(channel + 1)
    return Reading(name + "/" + other, values, none)
}

function all_names(count: int) -> int raises Refused {
    let names: owned list[str] = [name_of(n) for n in range(count)]
    var longest = ""
    for name in names {
        if len(name) > len(longest) { longest = name }
        if name == "ch2" { return len(longest) }
    }
    print(names)
    return len(names)
}

function repeat(text: str, times: int) -> str {
    if times == 0 { return "" }
    return text + repeat(text, times - 1)
}

var kept: owned list[str] = []
var last = "none yet"
for channel in range(12) {
    try {
        let reading = read(channel)
        reading.note = f"round {channel}"
        print(reading.tag(), reading)
        kept.append(reading.name)
        last = reading.tag()
    } except Refused as refused {
        print("refused:", refused.reason, refused.channel)
        last = refused.reason
        kept.append(refused.reason.upper())
        if channel > 8 { break }
        continue
    }
}
print(last)
print(kept)
for count in [2, 3, 6] {
    try {
        print(all_names(count))
    } except Refused as refused {
        print(refused)
    }
}
print(repeat("ab", 4), len(repeat("xyz", 30)))
var total = ""
for n in range(5) {
    total = total + str(n)
    total += "-"
}
print(total)
var trips = 0
while trips < 3 {
    total = total.replace(str(trips), "") + "!"
    trips += 1
    print(total)
}
"#);
    }

    #[test]
    fn with_no_collector_objects_and_lists_keep_their_own_text() {
        // Text in the fields of an object and in the items of a list,
        // `none` among them, and a loop whose body gives the list it is
        // walking a new item.
        no_collector_as_interpreter("text-inside", r#"owned class Packet {
    id: int
    name: str
    tags: owned list[str]

    function rename(self, name: str) {
        self.name = name + "/" + self.name
        self.tags.append(self.name)
    }
    function label(self) -> str { return f"{self.id}:{self.name}" }
}
function make(id: int) -> Packet {
    let tags: owned list[str] = [f"t{id}", "plain"]
    return Packet(id, f"p{id}", tags)
}
var gone = make(9)
for id in range(4) {
    let packet = make(id)
    packet.rename(str(id * 10))
    print(packet.label())
    print(packet.tags)
    print(len(packet.name))
    print(packet)
    if id == 2 { gone = packet }
}
print(gone.label())
var notes: owned list[str or none] = [none, "a", f"{1 + 1}"]
notes.append(none)
notes.append(gone.name)
notes[0] = gone.label()
notes[1] = none
print(notes)
for note in notes {
    if note is not none { print(note + "!") }
}
let names: owned list[str] = ["zero", gone.name, f"two {gone.id}"]
for name in names {
    names[1] = name + "?"
    print(name)
}
print(names)
print(names.pop())
print(names)
"#);
    }

    #[test]
    fn the_labels_example_builds_with_no_collector() {
        // docs/text.md is written around this file.
        no_collector_as_interpreter("labels", include_str!("../examples/labels.zeno"));
    }

    #[test]
    fn with_no_collector_kept_text_does_not_fill_the_pool_over_time() {
        // Each trip builds text, keeps some of it and lets the rest go.
        // A hundred times the trips makes a hundred times the text and
        // needs no more of the pool: every copy is given back.
        let program = |trips: usize| r#"owned class Log {
    last: str
    lines: owned list[str]

    function add(self, line: str) {
        self.last = line
        if len(self.lines) < 4 {
            self.lines.append(line)
        } else {
            self.lines[len(self.lines) - 1] = line
        }
    }
}
function line_for(n: int) -> str {
    if n % 7 == 0 { return "seven" }
    return f"line {n}"
}
let log = Log("", [])
var longest = ""
var total = 0
for n in range({trips}) {
    let line = line_for(n)
    log.add(line)
    if len(line) > len(longest) { longest = line }
    match line_for(n + 1) {
        case "seven" { total += 7 }
        case other { total += len(other) }
    }
}
print(log.last, longest, total)
print(log.lines)
"#.replace("{trips}", &trips.to_string());
        let mut needed = Vec::new();
        for trips in [50, 5000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("text-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            crate::emit_llvm::tests::no_collector_as_c(&name, &source, &printed, &report);
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "a hundred times the trips needed a bigger pool");
        assert!(needed[0] < 512, "{needed:?}");
    }

    #[test]
    fn the_c_for_kept_text() {
        let c = compile_no_collector(
            "function label(channel: int) -> str {
    return f\"ch{channel}\"
}

var best = \"none yet\"
for channel in range(3) {
    let name = label(channel)
    if channel == 1 {
        best = name
    }
}
print(best)
",
        )
        .unwrap();
        // A function's answer travels in the text room: the runtime moves
        // it down to where the room's top was when the `return` began.
        let label = c.split("static const char *z_label(int64_t z2_channel) {\n").nth(1).expect("the function");
        let label = label.split("\n}\n").next().unwrap();
        assert_eq!(
            label,
            "    size_t m3 = zeno_text_top;
    const char *t2 = zeno_text_return(m3, zeno_join(2, \"ch\", zeno_str_int(z2_channel)));
    return t2;"
        );
        // Whatever keeps text has a copy of its own, and frees it: `name`
        // at the end of each trip, `best` when it is given other text and
        // when the program ends. Reading `best`, which a function could
        // change, lends the statement a copy.
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        assert_eq!(
            program,
            "    z_best = \"none yet\";
    for (int64_t z_channel = 0; z_channel < 3; z_channel++) {
        size_t m1 = zeno_text_top;
        const char *z_name = zeno_text_keep(z_label(z_channel));
        zeno_text_top = m1;
        if (z_channel == 1) {
            const char *t1 = zeno_text_keep(z_name);
            zeno_text_drop(z_best);
            z_best = t1;
        }
        zeno_text_drop(z_name);
    }
    size_t m2 = zeno_text_top;
    zeno_print(1, zeno_text_lend(z_best));
    zeno_text_top = m2;
    zeno_text_drop(z_best);"
        );
        // With a collector none of it is there: text is shared, as ever.
        let shared = compile("var best = \"a\"\nbest = best + \"b\"\nprint(best)").unwrap();
        for gone in ["zeno_text_keep", "zeno_text_drop", "zeno_text_lend", "zeno_text_return"] {
            assert!(!shared.contains(gone), "{gone}");
        }
    }

    #[test]
    fn the_three_places_text_can_be() {
        // The runtime on its own, driven from C: what the comments in
        // runtime.c say about text written in the program, text in the
        // text room and text in the pool.
        let c = runtime(true, false)
            + r#"
int main(void) {
    atexit(zeno_memory_report);
    const char *written = "written";
    /* Text written in the program is kept, lent and freed as it is. */
    printf("%d %d\n", zeno_text_keep(written) == written, zeno_text_lend(written) == written);
    zeno_text_drop(written);
    zeno_text_drop(NULL);
    /* Text in the room is copied into the pool to be kept. */
    size_t mark = zeno_text_top;
    const char *built = zeno_join(2, "bu", "ilt");
    const char *kept = zeno_text_keep(built);
    printf("%d %d %d %s\n", zeno_in_room(built), zeno_in_pool(kept), kept != built, kept);
    zeno_text_top = mark;
    /* Kept text is lent as a copy in the room, which outlives the kept one. */
    const char *lent = zeno_text_lend(kept);
    zeno_text_drop(kept);
    printf("%d %s\n", zeno_in_room(lent), lent);
    zeno_text_top = mark;
    /* A function's answer is moved down to where its `return` began. */
    size_t began = zeno_text_top;
    zeno_join(2, "some", "thing in the way");
    const char *answer = zeno_text_return(began, zeno_join(2, "ans", "wer"));
    printf("%d %d %s\n", answer == zeno_text_room + began, (int)(zeno_text_top - began), answer);
    /* The caller's own text, lower in the room, comes back as it is. */
    size_t inner = zeno_text_top;
    printf("%d %d\n", zeno_text_return(inner, answer) == answer, zeno_text_return(inner, written) == written);
    /* A list of text frees its copies with itself, and nothing is left. */
    zeno_list *list = zeno_list_of(2, (zeno_slot){.s = zeno_text_keep(answer)}, (zeno_slot){.s = written});
    zeno_list *copy = zeno_list_copy_text(list);
    printf("%d %d\n", copy->items[0].s != list->items[0].s, copy->items[1].s == written);
    zeno_list_drop_text(copy);
    zeno_list_drop_text(list);
    return 0;
}
"#;
        let Some((printed, errors, finished)) = build_and_run("text-alone", &c, &["-DZENO_MEMORY_REPORT"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, "1 1\n1 1 1 built\n1 built\n1 7 answer\n1 1\n1 1\n");
        assert!(errors.contains("no collector, 0 blocks left at the end\n"), "{errors}");
    }

    #[test]
    fn with_no_collector_a_statement_owns_the_lists_it_makes() {
        // docs/temporaries.md. Lists made by `split`, `map`, `filter` and
        // `+`, and lists written out, that are looked at and given to
        // nobody: in a call, a condition, a loop. Each of these lines was
        // refused once ("would have no owner").
        no_collector_as_interpreter("statement-lists", r#"function double(n: int) -> int { return n * 2 }
function big(n: int) -> bool { return n > 2 }
function look(xs: owned list[int]) -> int { return len(xs) }
function make() -> owned list[int] { return [1, 2, 3] }

let a: owned list[int] = [1, 2, 3]
print(look(make()))
print(len(a + a))
print(2 in a)
print(5 in [4, 5, 6])
print(a.map(double))
print(a.filter(big))
print(look(a + [9]))
print(look([7, 8]))
let b: owned list[int] = a.map(double)
let c: owned list[int] = a.filter(big) + b
print(b)
print(c)
for w in "x y z".split() { print(w) }
for n in a.map(double) { print(n) }
for ch in "hey" { print(ch) }
for n in make() { print(n) }
if len("a b".split()) == 2 { print("two") }
var i = 0
while len([i, i]) > i { i += 1 }
print(i)
"#);
    }

    #[test]
    fn with_no_collector_made_lists_of_text_keep_their_own_copies() {
        // The same with text in the lists, which each list has copies of,
        // and with the lists inside an object that a statement owns.
        no_collector_as_interpreter("statement-text-lists", r#"owned class Bag {
    name: str
    items: owned list[str]

    function size(self) -> int { return len(self.items) }
}

function shout(word: str) -> str { return word.upper() + "!" }
function long(word: str) -> bool { return len(word) > 2 }
function words(text: str) -> owned list[str] { return text.split() }
function bag(text: str) -> Bag { return Bag(text, text.split(",")) }
function count(xs: owned list[str]) -> int { return len(xs) }
function first(text: str) -> str raises ValueError {
    let parts: owned list[str] = text.split(",")
    if len(parts) < 2 { raise ValueError("no comma in " + text) }
    return parts[0]
}
function pick(text: str) -> str raises ValueError {
    # A statement that owns a list, and then fails half way.
    return str(count(text.split())) + first(text)
}
function total(text: str) -> int {
    # A return that owns a list.
    return count(text.split()) + len(words(text))
}

let line = "alpha be gamma"
print(line.split().map(shout))
print(line.split().filter(long))
print(count(line.split() + ["x"]))
let kept: owned list[str] = line.split().map(shout)
let more: owned list[str] = kept + line.split()
print(kept)
print(more)
print(bag("a,b,c").size())
print(bag("p,q").name)
print(count(bag("x,y").items))
print([shout(w) for w in line.split()])
print([len(w.split("a")) for w in words(line)])
for w in bag("m,n").items { print(w) }
for ch in line.split()[0] { print(ch) }
print(total(line))
try {
    print(pick("a b,c d"))
    print(pick("nothing here"))
} except ValueError as e {
    print("failed:")
    print(e.message)
}
match len(line.split()) {
    case 3 { print("three") }
    case _ { print("other") }
}
let ok = len(line) > 100 and count(line.split()) > 1
let or_so = len(line) > 100 or count(line.split()) > 1
print(ok)
print(or_so)
var acc: owned list[str] = []
acc += line.split()
acc += ["tail"]
acc = acc + ["end"]
print(acc)
"#);
    }

    #[test]
    fn with_no_collector_what_a_statement_owns_is_freed_on_every_way_out() {
        // A call that fails half way through the statement, a `return`
        // from inside a loop over a list the statement owns, conditions
        // of `if`, `else if`, `while` and `match`, and `+=` on a list.
        no_collector_as_interpreter("statement-exits", r#"function double(n: int) -> int { return n * 2 }
owned class Box {
    items: owned list[int]
    tags: owned list[str]
}
function make(n: int) -> Box { return Box([n, n + 1], str(n).split()) }
function risky(n: int) -> int raises ValueError {
    if n > 2 { raise ValueError("too big: " + str(n)) }
    return n
}
function sum_of(xs: owned list[int]) -> int {
    var total = 0
    for x in xs { total += x }
    return total
}
function careful(n: int) -> int raises ValueError {
    # The statement owns a list and a Box when `risky` fails.
    return sum_of([n, n] + [1]) + len(make(n).tags) + risky(n)
}
function early(n: int) -> int {
    for x in make(n).items {
        if x > n { return x + len([x, x]) }
    }
    return 0
}

let a: owned list[int] = [1]
var b: owned list[int] = [2]
b = a.map(double)
b += a
b += [3] + [4]
print(b)
let bx = Box([1, 2] + a, "p q".split())
bx.items += [5]
print(bx.items)
print(bx.tags)
bx.items = a.map(double)
bx.tags += ["r"] + "s t".split(" ")
print(bx.items)
print(bx.tags)
print([1, 2][0])
print(([1] + [2])[1])
print(len([n for n in [1, 2, 3] if n > 1]))
[1, 2]
a.map(double)
print(Box([4], ["x"]).items)
print(len(Box([4, 5], []).items))
print(make(7).tags)
print([sum_of([n, n]) + len(make(n).items) for n in [1, 2, 3]])
print(early(3))
for n in range(4) {
    try {
        print(careful(n))
    } except ValueError as e {
        print("failed:")
        print(e.message)
    }
}
var i = 0
while i < 3 and sum_of([i, 1]) < 4 {
    i += 1
}
print(i)
if sum_of(make(2).items) == 5 {
    print("five")
} else if len("a b c".split()) == 3 {
    print("three")
}
match sum_of(make(1).items) {
    case 3 { print("yes", len(make(9).tags)) }
    case _ { print("no") }
}
"#);
    }

    #[test]
    fn with_no_collector_a_statement_owns_what_is_made_in_awkward_places() {
        // In the middle of `a < b < c`, which is nested `if`s in C and
        // blocks that may be skipped in IR. Inside an item and a test of
        // a comprehension. As what `not` is in front of. Inside another
        // list that is itself only looked at.
        no_collector_as_interpreter("statement-awkward", r#"function n(xs: owned list[int]) -> int { return len(xs) }
function twice(xs: owned list[int]) -> owned list[int] { return xs + xs }
owned class Box {
    items: owned list[int]

    function total(self) -> int { return sum(self.items) }
}
print(1 < n([1, 2]) < n([1, 2, 3]))
print(5 < n([1, 2]) < n([1, 2, 3]))
let sizes: owned list[int] = [n([i, i]) for i in range(3) if n([i]) == 1]
print(sizes)
print(n(twice([1, 2])))
print(n(twice(twice([1]) + [5])))
print(not n([1]) == 2)
let b = Box([1])
b.items += [2, 3]
b.items += twice([4])
print(b.total())
print(sum([1, 2] + [3]))
print(max(twice([3, 9])))
print([x * 2 for x in twice([1, 2])])
print(len([[1, 2][0], [3][0]]))
var k = 0
while n([k]) + k < 4 {
    k += n([k, k]) - 1
    if k > 10 { break }
}
print(k)
print(twice([7])[1])
print(n([n([1, 2]), n(twice([3]))]))
"#);
    }

    #[test]
    fn the_words_example_builds_with_no_collector() {
        // docs/temporaries.md is written around this file.
        no_collector_as_interpreter("words", include_str!("../examples/words.zeno"));
    }

    #[test]
    fn with_no_collector_made_lists_do_not_fill_the_pool_over_time() {
        // Each trip makes a dozen lists and keeps one. A hundred times
        // the trips needs no more of the pool.
        let program = |trips: usize| r#"function loud(word: str) -> str { return word.upper() }
function short(word: str) -> bool { return len(word) < 4 }
function count(items: owned list[str]) -> int { return len(items) }
var total = 0
var last: owned list[str] = []
for n in range({trips}) {
    let line = f"set led {n} on"
    total += count(line.split())
    total += len(line.split().map(loud) + line.split(" ").filter(short))
    for word in line.split() { total += len(word) }
    for ch in line {
        if ch == "e" { total += 1 }
    }
    last = line.split().map(loud)
    last += ["end"]
}
print(total)
print(last)
"#.replace("{trips}", &trips.to_string());
        let mut needed = Vec::new();
        for trips in [50, 5000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("lists-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            crate::emit_llvm::tests::no_collector_as_c(&name, &source, &printed, &report);
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "a hundred times the trips needed a bigger pool");
        assert!(needed[0] < 2048, "{needed:?}");
    }

    #[test]
    fn the_c_for_what_a_statement_owns() {
        let c = compile_no_collector(
            "function longest(words: list[str]) -> int {
    var most = 0
    for word in words {
        if len(word) > most { most = len(word) }
    }
    return most
}

let line = \"set led on\"
print(longest(line.split()))
let ok = len(line) > 4 and len(line.split()) == 3
print(ok)
",
        )
        .unwrap();
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // The list of words is put in a variable of the statement's own,
        // which is declared where the statement begins and freed where it
        // ends. The right side of `and` is over before the statement is,
        // so the list it made is freed there.
        assert_eq!(
            program,
            "    z_line = \"set led on\";
    size_t m1 = zeno_text_top;
    zeno_list *t1 = NULL;
    t1 = zeno_split_blanks(z_line);
    zeno_print(1, zeno_str_int(z_longest(t1)));
    zeno_list_drop_text(t1);
    zeno_text_top = m1;
    bool t4 = (zeno_len(z_line) > 4);
    if (t4) {
        zeno_list *t2 = NULL;
        t2 = zeno_split_blanks(z_line);
        bool t3 = (t2->len == 3);
        zeno_list_drop_text(t2);
        t4 = t3;
    }
    z_ok = t4;
    size_t m2 = zeno_text_top;
    zeno_print(1, zeno_str_bool(z_ok));
    zeno_text_top = m2;"
        );
    }

    #[test]
    fn an_owned_value_that_is_only_looked_at_is_freed_in_every_build() {
        // `make(round)` is given to nobody: the statement owns it. That
        // is so with a collector too, which then has nothing to do.
        let source = "owned class Packet {
    id: int
    bytes: owned list[int]
}
function make(round: int) -> Packet { return Packet(round, [n * round for n in range(500)]) }
function last(p: Packet) -> int { return p.bytes[499] % 7 }
function work(rounds: int) -> int {
    var total = 0
    for round in range(rounds) {
        total += last(make(round)) + make(round).id % 2
    }
    return total
}
print(work(400))
";
        same_as_interpreter("statement-owned", source);
        let c = compile(source).unwrap();
        let work = c.split("static int64_t z_work(int64_t z_rounds) {\n").nth(1).expect("the function");
        let work = work.split("\n}\n").next().unwrap();
        assert_eq!(
            work,
            "    int64_t z_total = 0;
    int64_t t2 = z_rounds;
    for (int64_t z_round = 0; z_round < t2; z_round++) {
        z_Packet *t3 = NULL;
        z_Packet *t4 = NULL;
        t3 = z_make(z_round);
        int64_t t5 = z_last(t3);
        t4 = z_make(z_round);
        z_total = zeno_add(z_total, zeno_add(t5, (t4->z_id & INT64_C(1)), 10, 18), 10, 9);
        zeno_drop_Packet(t4);
        zeno_drop_Packet(t3);
    }
    return z_total;"
        );
        let Some((printed, [made, most, collections])) = memory_of("statement-owned-memory", source) else {
            return;
        };
        assert_eq!(printed, interpreted(source));
        assert!(made > 2_000_000, "the program made {made} bytes");
        assert_eq!(collections, 0, "the collector should have nothing to do");
        assert!(most < 16_384, "at most {most} bytes were in use at once");
    }

    #[test]
    fn what_one_owner_turns_down_says_so() {
        const SETTING: &str = r#"owned class Packet {
    id: int
    name: str
    bytes: owned list[int]
}
class Bad {
    id: int
    why: str
}
enum Shape {
    Dot
    Circle(r: float)
}
function risky(n: int) -> int raises Bad {
    if n > 2 { raise Bad(n, "big") }
    return n
}
function name_len(text: str) -> int { return len(text) }
function look(xs: owned list[int]) -> int { return len(xs) }
function double(n: int) -> int { return n * 2 }
function add(a: int, b: int) -> int { return a + b }
let x = 7
"#;
        // What the checker turns down, because in this build every list
        // and object has one owner. It is the same language with fewer
        // programs in it: each of these is fine where values are shared.
        let one_owner = |body: &str, why: &str| {
            let source = format!("{SETTING}{body}\n");
            let message = compile_no_collector(&source).err().unwrap_or_else(|| "it compiled".to_string());
            assert!(message.contains(why), "{body}\n{message}");
            assert!(compile(&source).is_ok(), "{body}");
            crate::emit_llvm::tests::refuses_as_c(&source, &message);
        };

        // (Text is not in this table: it is kept by copy. See
        // `with_no_collector_text_is_kept_by_copy`. Nor is a plain list
        // or a plain class: whoever receives one owns it. See
        // `with_no_collector_a_list_and_an_object_need_no_word`.)

        // (Nor is an error: the `except` that catches it owns it, and it
        // can be kept, given away and raised again. Nor a function
        // written where a value goes. See
        // `with_no_collector_an_error_and_a_function_value_have_one_owner`.)

        // An error that was given away is gone, like any other value.
        let caught = "try { print(risky(5)) } except Bad as e";
        one_owner(&format!("{caught} {{ let again = e\nprint(e.id) }}"), "was given away on line");
        one_owner(
            &format!("function pass_on(b: take Bad) -> int {{ return b.id }}\n{caught} {{ print(pass_on(e))\nprint(e) }}"),
            "was given away on line",
        );

        // A value that was given away is gone, and a look cannot be kept.
        one_owner("let xs = [1, 2, 3]\nlet ys = xs\nprint(len(xs))", "was given away on line");
        one_owner(
            "function total(xs: take list[int]) -> int { return len(xs) }\nlet xs = [1, 2]\nprint(total(xs))\nprint(total(xs))",
            "was given away on line",
        );
        one_owner("let b = Bad(1, \"x\")\nlet c = b\nprint(b.id)", "was given away on line");
        one_owner("let grid = [[1], [2]]\nlet row = grid[0]\nprint(row)", "`[ ]` only looks at it");

        // So is an enum value that carries something, and what may be
        // `none`. (Both were refused outright before.)
        one_owner("let s = Circle(1.0)\nlet t = s\nprint(s)", "was given away on line");
        one_owner("var last: Bad or none = Bad(1, \"x\")\nlet other = last\nprint(last is none)", "was given away on line");

        // A function value has one owner as well. One that uses a list
        // from outside it looks at the list, so it cannot be kept, and
        // nothing is shared, so it cannot use a `var` of the function
        // around it.
        one_owner("let f = function(n: int) -> int { return n + x }\nlet g = f\nprint(f(1))", "was given away on line");
        one_owner(
            // A function that is kept takes what it uses (decision 56.8),
            // and the owner holds nothing after it.
            "function main() {\n    let xs = [1, 2]\n    let f = function(n: int) -> int { return n + len(xs) }\n\
             print(f(1), len(xs))\n}\nmain()",
            "was given away on line",
        );
        one_owner(
            // A `var` could be given a new value after it was taken.
            "var xs = [1, 2]\nlet f = function(n: int) -> int { return n + len(xs) }\nprint(f(1))",
            "but `xs` is a `var`",
        );
        one_owner(
            "function counter() -> function() -> int {\n    var count = 0\n    return function() -> int { return count }\n}\nprint(counter()())",
            "the two functions would share it",
        );

        // A list of numbers that may be `none` was refused here until
        // 0.8.0; the list owns a box for each (decision 56.14).

        // What sits right beside those refusals, and is accepted.
        let accepted = format!(
            "{SETTING}{}",
            r#"var why = ""
try {
    print(risky(5))
} except Bad as e {
    print(e.id, e.why, e)
    why = e.why                       # a copy, which outlives the error
}
print(why)
for pin in [2, 3, 5] {                # a list the loop frees when it ends
    print(pin)
}
let a: owned list[int] = [1, 2, 3]
print(a.reduce(add, 0), look(a), a, Dot == Dot, 3 s, (1500 ms).s)
print(look([1, 2]), x in [1, 2])      # lists the statement frees when it ends
print(len(a + a), len("a b".split()), len(a.map(double)))
let plain = [1, 2, 3]                 # no word needed: `plain` owns it
let doubled = plain.map(double)
let words = "a b".split()
print(look(plain), doubled)
print(words)
print(plain + doubled)
let b = Bad(1, "x")                   # nor for an object
print(b.id, Bad(2, "y").why, b)
for ch in "abc" {                     # a list of characters, the loop's own
    print(ch)
}
var best: int or none = none
best = 4
if best is not none {
    print(best + 1)
}
let round = Circle(1.5)                # an enum value that carries something
print(round, round == Dot)
var last: Bad or none = none           # and what may be `none`
last = Bad(3, "z")
print(last is none)
try {
    print(int("12"), float("1.5"))
    print(int("zz"))
} except ValueError as e {
    print(e)
}
let ready = ValueError("made early")   # an error is a value like any other
print(ready.message)
let seen: list[Bad] = []
try {
    print(risky(9))
} except Bad as e {
    seen.append(e)                     # kept: the list owns it now
}
print(len(seen), seen[0].why)
let scaled = function(n: int) -> int { return n * x }   # a function value
print(scaled(2), plain.map(function(n: int) -> int { return n + len(plain) }))
"#
        );
        no_collector_as_interpreter("accepted", &accepted);
    }

    /// Plain lists and plain classes, lists in lists and objects in
    /// lists, with not one `owned` in the program.
    const NO_WORD: &str = r##"class Point {
    x: int
    y: int
    function sum(self) -> int { return self.x + self.y }
}

function total(ps: list[Point]) -> int {
    var t = 0
    for p in ps { t += p.sum() }
    return t
}

function sum_of(p: Point) -> int { return p.sum() }
function add_point(so_far: int, p: Point) -> int { return so_far + p.x }
function row(n: int) -> list[int] {
    return [i for i in range(n)]
}

var grid = [[1, 2], [3, 4]]
grid.append([5, 6])
grid.append(row(3))
print(grid[2][0])
print(grid)
grid[0] = [9]
grid[1].append(7)
print(grid)
let last = grid.pop()
print(last)
print(len(grid))

var points = [Point(1, 2), Point(3, 4)]
points.append(Point(5, 6))
let extra = Point(7, 8)
points.append(extra)
for q in points { print(q.sum()) }
print(total(points))
print(points[1].x)
points[1].x = 30
print(points[1])
let gone = points.pop()
print(gone.x)
points[0] = Point(0, 0)
print(points)
print(len([Point(1, 1)]))
let sums = points.map(sum_of)
print(sums)
print(points.reduce(add_point, 0))
grid[1][0] = 33
print(grid, [[1], [2, 3]], len([[1], [2]]))
for r in grid {
    for c in r { print(c) }
    r.append(0)
}
print(grid)
for r in [[1, 2], [3]] { print(r) }
print([Point(i, i) for i in range(3)])
let made = [row(i) for i in range(3)]
print(made)
"##;

    /// The same, where errors and early exits end blocks half way.
    const NO_WORD_EXITS: &str = r##"class Reading {
    name: str
    values: list[int]
    function peak(self) -> int {
        var best = 0
        for v in self.values {
            if v > best { best = v }
        }
        return best
    }
}

class Log {
    title: str
    readings: list[Reading]
    function add(self, r: take Reading) {
        self.readings.append(r)
    }
    function peak(self) -> int {
        var best = 0
        for r in self.readings {
            if r.peak() > best { best = r.peak() }
        }
        return best
    }
    function names(self) -> list[str] {
        return [r.name for r in self.readings]
    }
}

function reading(name: str, n: int) -> Reading raises ValueError {
    if n < 0 { raise ValueError(f"no reading called {name} can have {n} values") }
    return Reading(name, [i * n for i in range(n)])
}

function build(sizes: list[int]) -> Log raises ValueError {
    let log = Log("run", [])
    for n in sizes {
        log.add(reading(f"r{n}", n))
    }
    return log
}

function first_big(log: Log, limit: int) -> str {
    for r in log.readings {
        if r.peak() > limit { return r.name }
    }
    return "none"
}

function pair(a: int, b: int) -> list[Reading] raises ValueError {
    return [reading("a", a), reading("b", b)]
}

let log = build([1, 2, 3, 4])
print(log.peak())
print(log.names())
print(first_big(log, 3))
print(first_big(log, 100))
print(log.readings[2])
log.readings[2].values.append(99)
log.readings[2].name = "changed" + str(2)
print(log.readings[2].peak())
print(log.names())

try {
    let bad = build([2, -1, 3])
    print(bad.peak())
} except ValueError as e {
    print(e)
}

try {
    let two = pair(2, -5)
    print(len(two))
} except ValueError as e {
    print(e)
}
let two = pair(2, 3)
print(len(two), two[1].peak())

var kept = [reading("x", 1)]
for i in range(6) {
    let r = reading(f"loop{i}", i)
    if i == 1 { continue }
    if i == 4 { break }
    kept.append(r)
}
print(len(kept))
for r in kept { print(r.name, r.values) }
while len(kept) > 2 {
    let r = kept.pop()
    print("popped", r.name)
}
kept.pop()
print(len(kept))
kept[0] = reading("fresh", 2)
kept[0] = reading("fresher", 3)
print(kept)
let a = reading("a", 1)
let b = reading("b", 2)
let both = [a, b]
print(len(both), len([reading("t", 2)]), [reading("u", 1)][0].name)
var table = [[1, 2, 3], [4, 5]]
table[1] = [6]
let moved = table.pop()
table.append(moved)
let words = ["a b".split(), "c d e".split()]
print(table, words, len(words[1]))
"##;

    /// A list that owns its items, said in words: an `owned list` of
    /// objects of an `owned class`. This one is for every build.
    const OWNS_ITEMS: &str = r##"owned class Packet {
    id: int
    name: str
    data: owned list[int]

    function size(self) -> int {
        return len(self.data)
    }
}

owned class Queue {
    waiting: owned list[Packet]

    function add(self, p: take Packet) {
        self.waiting.append(p)
    }

    function sizes(self) -> list[int] {
        return [p.size() for p in self.waiting]
    }

    function biggest(self) -> int {
        var best = 0
        for p in self.waiting {
            if p.size() > best {
                best = p.size()
            }
        }
        return best
    }
}

function make(id: int, count: int) -> Packet raises ValueError {
    if count < 0 {
        raise ValueError(f"packet {id} cannot hold {count} items")
    }
    return Packet(id, f"p{id}", [n * id for n in range(count)])
}

function fill(counts: list[int]) -> Queue raises ValueError {
    let q = Queue([])
    for count in counts {
        q.add(make(len(q.waiting) + 1, count))
    }
    return q
}

function describe(p: Packet) -> str {
    return f"{p.name}: {p.data}"
}

function first_over(ps: owned list[Packet], limit: int) -> str {
    for p in ps {
        if p.size() > limit {
            return p.name
        }
    }
    return "none"
}

function drain(ps: owned list[Packet]) -> int {
    var total = 0
    for p in ps {
        total += p.size()
    }
    return total
}

let q = fill([3, 1, 4])
print(q.sizes(), q.biggest(), len(q.waiting))
print(describe(q.waiting[0]), first_over(q.waiting, 3), first_over(q.waiting, 9))
q.waiting[1].data.append(42)
q.waiting[1].name = "changed"
print(q.waiting[1])
q.waiting[0] = make(9, 2)
let last = q.waiting.pop()
print(describe(last), len(q.waiting))
print(q)

try {
    let bad = fill([2, -1, 5])
    print(bad.biggest())
} except ValueError as e {
    print(e)
}

var kept: owned list[Packet] = []
for i in range(6) {
    let p = make(i, i)
    if i == 2 {
        continue
    }
    if i == 5 {
        break
    }
    kept.append(p)
}
print(len(kept), first_over(kept, 2))
let pair = [make(1, 1), make(2, 2)]
print(len(pair), describe(pair[1]), len([make(3, 3)]), [make(4, 1)][0].name)
print(drain(pair), drain([make(5, 5), make(6, 6)]))
while len(kept) > 1 {
    let p = kept.pop()
    print("popped", p.name)
}
kept[0] = make(7, 1)
print(kept)
kept = [make(8, 2)]
print(kept.map(describe))
"##;

    #[test]
    fn with_no_collector_a_list_and_an_object_need_no_word() {
        no_collector_as_interpreter("no-word", NO_WORD);
        no_collector_as_interpreter("no-word-exits", NO_WORD_EXITS);
        no_collector_as_interpreter("scheduler", include_str!("../examples/scheduler.zeno"));
    }

    #[test]
    fn a_list_owns_items_that_have_one_owner() {
        // With a collector, run before every allocation: the list frees
        // its items itself, and the collector must not free them first.
        same_as_interpreter("owns-items", OWNS_ITEMS);
        no_collector_as_interpreter("owns-items", OWNS_ITEMS);
    }

    // ------------------------- What may be `none`, and enums that carry values

    /// A list or an object that may be `none`, with no collector.
    const MAY_BE_NONE: &str = r##"# What may be `none` can have one owner.
class Task {
    name: str
    runs: list[int]
}

class Device {
    name: str
    current: Task or none
    history: list[int] or none
}

function parse(line: str) -> Task or none {
    let words = line.split()
    if len(words) < 2 {
        return none
    }
    return Task(words[1], [len(words)])
}

function describe(task: Task or none) -> str {
    if task is none {
        return "nothing"
    }
    return f"{task.name} {task.runs}"
}

function run(task: Task) -> int {
    print("running", task.name)
    return len(task.runs)
}

function count(xs: list[int] or none) -> int {
    if xs is none { return -1 }
    return len(xs)
}

var current: Task or none = none
print(current is none, describe(current))
current = parse("run blink now")
print(current is none, describe(current))
if current is not none {
    current.runs.append(7)
    print(current.name, current.runs)
}
current = parse("stop")
print(describe(current))
current = parse("run sample")
current = none
print(describe(current))

let made = parse("go fast")
if made is not none {
    print(run(made))
}
print(describe(parse("a b c")), describe(parse("a")))
if parse("x y") is not none {
    print("parsed")
}

let device = Device("pump", parse("run prime"), none)
print(device.name, describe(device.current), count(device.history))
device.current = none
device.history = [1, 2, 3]
print(describe(device.current), count(device.history))
device.current = parse("run again please")
match device.current {
    case none { print("idle") }
    case task {
        if task is not none {
            print("busy with", task.name)
        }
    }
}
print(device)

var maybe: list[int] or none = none
print(count(maybe))
maybe = [4, 5]
if maybe is not none {
    maybe.append(6)
    print(maybe, len(maybe))
}
print(count(maybe))

let slots: list[Task or none] = [parse("run a"), none, parse("run b")]
for slot in slots {
    print(describe(slot))
}
slots[1] = parse("run c")
slots[0] = none
print(describe(slots[0]), describe(slots[1]))
let last = slots.pop()
print(describe(last), len(slots))
"##;

    /// An enum whose cases carry values, with no collector.
    const ENUM_VALUES: &str = r##"# An enum whose cases carry values has one owner, like an object.
enum Command {
    Stop
    Wait(ms: int)
    Set(pin: int, on: bool)
    Say(text: str)
    Batch(pins: list[int])
}

enum Color {
    Red
    Green
}

class Step {
    command: Command
    color: Color
}

function parse(line: str) -> Command or none {
    let words = line.split()
    if len(words) == 0 { return none }
    match words[0] {
        case "stop" { return Stop }
        case "wait" { return Wait(len(line)) }
        case "set" { return Set(len(words), len(words) > 2) }
        case "say" { return Say(line) }
        case "batch" { return Batch([len(w) for w in words]) }
        case _ { return none }
    }
}

function cost(command: Command) -> int {
    match command {
        case Stop { return 0 }
        case Wait(ms) { return ms }
        case Set(pin, on) {
            if on { return pin }
            return -pin
        }
        case Say(text) { return len(text) }
        case Batch(pins) {
            var sum = 0
            for pin in pins { sum += pin }
            return sum
        }
    }
}

function run(command: Command) -> str {
    match command {
        case Stop { return "stopped" }
        case Say(text) { return text.upper() }
        case other { return f"ran {other}" }
    }
}

let first = Wait(250)
print(first, cost(first), first == Wait(250), first == Stop)
let second = first
print(run(second))
print(run(Stop), run(Say("hi there")), run(Batch([1, 2])))

var state = Stop
var trips = 0
while trips < 4 {
    match state {
        case Stop { state = Wait(1) }
        case Wait(ms) { state = Set(ms + 1, true) }
        case Set(pin, on) { state = Say(f"pin {pin} is {on}") }
        case Say(text) { state = Batch([len(text), trips]) }
        case Batch(_) { state = Stop }
    }
    trips += 1
    print(trips, state)
}

let commands = [Wait(5), Stop, Set(3, false), Say("abc"), Batch([4, 5, 6])]
var total = 0
for command in commands {
    total += cost(command)
}
print("total", total, len(commands))
commands.append(Wait(9))
commands[1] = Say("replaced")
print(commands)
let taken = commands.pop()
print(taken)
print(run(taken))

for line in ["wait a bit", "set 4 on", "say hello", "batch aa b cccc", "", "jump", "stop"] {
    let command = parse(line)
    if command is none {
        print("?", line)
    } else {
        print(command, cost(command))
    }
}

let step = Step(Say("in a step"), Green)
print(step, cost(step.command), step.color == Green)
step.command = Batch([7])
match step.command {
    case Batch(pins) { print("batch of", len(pins), pins) }
    case _ { print("other") }
}
var color = Red
let copy = color
color = Green
print(color, copy, color == copy)
"##;

    /// The same where things go wrong and blocks are left early.
    const ENUM_EXITS: &str = r##"# Enums and `or none` where things go wrong, and where blocks are left early.
class Bad {
    why: str
}

class Reading {
    channel: int
    label: str
}

enum Tree {
    Leaf(value: int)
    Node(left: Tree, right: Tree)
}

enum Event {
    Quiet
    Sample(reading: Reading)
    Burst(readings: list[Reading])
    Note(text: str, extra: Reading or none)
}

function risky(n: int) -> int raises Bad {
    if n > 3 { raise Bad(f"{n} is too big") }
    return n
}

function build(depth: int) -> Tree raises Bad {
    if depth == 0 {
        return Leaf(risky(depth))
    }
    return Node(build(depth - 1), Leaf(risky(depth)))
}

function sum(tree: Tree) -> int {
    match tree {
        case Leaf(value) { return value }
        case Node(left, right) { return sum(left) + sum(right) }
    }
}

function depth(tree: Tree) -> int {
    match tree {
        case Leaf(_) { return 0 }
        case Node(Leaf(_), Leaf(_)) { return 1 }
        case Node(left, _) { return depth(left) + 1 }
    }
}

function event(n: int) -> Event raises Bad {
    if n == 0 { return Quiet }
    if n == 1 { return Sample(Reading(risky(n), "one")) }
    if n == 2 { return Burst([Reading(1, "a"), Reading(risky(n), "b")]) }
    if n == 3 { return Note(f"note {n}", Reading(n, "extra")) }
    if n == 4 { return Note("plain", none) }
    return Burst([Reading(1, "kept"), Reading(risky(n), "never")])
}

function describe(e: Event) -> str {
    match e {
        case Quiet { return "quiet" }
        case Sample(reading) { return f"sample {reading.channel} {reading.label}" }
        case Burst(readings) {
            var labels = ""
            for reading in readings {
                if reading.channel > 1 { continue }
                labels = labels + reading.label
            }
            return f"burst of {len(readings)}: {labels}"
        }
        case Note(text, extra) {
            if extra is none {
                return text
            }
            return f"{text} with {extra.label}"
        }
    }
}

let small = build(2)
print(small, sum(small), depth(small))
try {
    let big = build(5)
    print(sum(big))
} except Bad as bad {
    print("refused:", bad.why)
}
print(Leaf(1) == Leaf(1), Node(Leaf(1), Leaf(2)) == Node(Leaf(1), Leaf(3)), depth(Leaf(9)))

var seen = 0
for n in range(7) {
    try {
        let e = event(n)
        seen += 1
        print(n, describe(e), e)
    } except Bad as bad {
        print(n, "failed:", bad.why)
    }
}
print("seen", seen)

var kept: Event or none = none
var n = 0
while true {
    n += 1
    try {
        kept = event(n)
    } except Bad as bad {
        print("stopped at", n)
        break
    }
    match kept {
        case none { print("nothing") }
        case Quiet { continue }
        case found {
            if found is not none {
                print(n, describe(found))
            }
        }
    }
}
print(kept is none)
match event(3) {
    case Note(text, _) { print("made and matched:", text) }
    case _ { print("other") }
}
match event(2) {
    case Burst(readings) { print(len(readings), readings[1].label) }
    case _ { print("other") }
}
Leaf(4)
event(1)
let trees = [Leaf(1), build(1), build(3)]
var total = 0
for tree in trees {
    match tree {
        case Leaf(value) {
            if value == 1 { continue }
            total += value
        }
        case Node(left, right) { total += sum(left) * 10 + sum(right) }
    }
}
print(total, Leaf(1) in trees, len(trees))
"##;

    /// What may be `none`, with the words written and a collector beside.
    const OWNED_OR_NONE: &str = r##"# What may be `none` can have one owner in every build: here the words
# are written, and a collector runs beside them.
owned class Packet {
    id: int
    name: str
    data: owned list[int]
}

owned class Slot {
    held: Packet or none
    spare: owned list[int] or none
}

function find(n: int) -> Packet or none {
    if n % 2 == 0 {
        return none
    }
    return Packet(n, f"packet {n}", [n, n * 2])
}

function describe(p: Packet or none) -> str {
    if p is none { return "nothing" }
    return f"{p.name} {p.data}"
}

function size(xs: owned list[int] or none) -> int {
    if xs is none { return 0 }
    return len(xs)
}

function send(p: Packet) -> int {
    return p.id + len(p.data)
}

var last: Packet or none = none
var sent = 0
for n in range(8) {
    last = find(n)
    print(n, describe(last))
    let found = find(n + 1)
    if found is not none {
        sent += send(found)
    }
}
print("sent", sent, describe(last))
let slot = Slot(find(3), none)
print(describe(slot.held), size(slot.spare), slot)
slot.held = find(4)
slot.spare = [7, 8, 9]
print(describe(slot.held), size(slot.spare))
slot.held = find(5)
match slot.held {
    case none { print("empty") }
    case packet {
        if packet is not none { print("holds", packet.id) }
    }
}
let queue: owned list[Packet or none] = [find(1), find(2), find(3)]
for p in queue {
    print(describe(p))
}
queue[1] = find(7)
queue[0] = none
let taken = queue.pop()
print(describe(taken), describe(queue[0]), describe(queue[1]), len(queue))
let maybe: owned list[int] or none = [1, 2, 3]
print(size(maybe), size(none), describe(find(9)), describe(find(10)))
"##;

    #[test]
    fn with_no_collector_what_may_be_none_has_one_owner() {
        no_collector_as_interpreter("may-be-none", MAY_BE_NONE);
    }

    #[test]
    fn with_no_collector_an_enum_value_that_carries_values_has_one_owner() {
        no_collector_as_interpreter("enum-values", ENUM_VALUES);
        no_collector_as_interpreter("enum-exits", ENUM_EXITS);
        no_collector_as_interpreter("commands", include_str!("../examples/commands.zeno"));
    }

    /// `match` on values that may be `none`: an object, a number, an enum
    /// value and a text, each with a `case none` first.
    pub(crate) const AFTER_CASE_NONE: &str = r##"class Point {
    x: int
}
enum Shape {
    Dot
    Circle(r: float)
}
function find(n: int) -> Point or none {
    if n > 0 { return Point(n) }
    return none
}
function half(n: int) -> int or none {
    if n % 2 == 0 { return n // 2 }
    return none
}
function pick(n: int) -> Shape or none {
    if n == 0 { return none }
    if n == 1 { return Dot }
    return Circle(2.0)
}
function name(n: int) -> str or none {
    if n == 0 { return none }
    return "n" + str(n)
}
function show(n: int) -> str {
    let found = find(n)
    match found {
        case none { return "nothing" }
        case p { return "point " + str(p.x) }
    }
}
function twice(n: int) -> int {
    let h = half(n)
    match h {
        case none { return -1 }
        case 2 { return 200 }
        case value { return value * 4 }
    }
}
function area(n: int) -> float {
    let shape = pick(n)
    match shape {
        case none { return -1.0 }
        case Dot { return 0.0 }
        case Circle(r) { return r * r }
    }
}
function after(n: int) -> int {
    let h = half(n)
    match h {
        case none { return 0 }
        case _ { }
    }
    return h + 1
}
function size(n: int) -> int {
    let text = name(n)
    match text {
        case none { return 0 }
        case "n1" { return 100 }
        case _ { return len(text) }
    }
}
for n in range(5) {
    print(show(n), twice(n), area(n), after(n), size(n))
}
"##;

    #[test]
    fn after_case_none_the_value_is_there_in_every_build() {
        // The cases after `case none` read the value inside, by a new
        // name or by the variable's own, and so does what follows a
        // `match` whose `case none` left. A number that may be `none` is
        // stored differently from a number, so each read has to unwrap.
        same_as_interpreter("after-case-none", AFTER_CASE_NONE);
        no_collector_as_interpreter("after-case-none", AFTER_CASE_NONE);
    }

    /// Tables at the top of the file, which functions, methods and a kept
    /// function read and change.
    pub(crate) const TOP_TABLES: &str = r##"# Tables at the top of the file, read by functions.
class Config {
    gain: int
    name: str
    limits: list[int]
}
enum Mode {
    Idle
    Run(speed: int)
}
let table = [10, 20, 30, 40]
let config = Config(3, "main", [0, 100])
let grid = [[1, 2], [3, 4]]
let mode = Run(7)
let names = ["a", "bb", "ccc"]
let spare: Config or none = none

class Channel {
    id: int
    function scaled(self, i: int) -> int {
        return table[i] * config.gain + self.id
    }
}

function total() -> int {
    var sum = 0
    for value in table { sum += value }
    for row in grid {
        for cell in row { sum += cell }
    }
    return sum
}

function in_range(value: int) -> bool {
    return value >= config.limits[0] and value <= config.limits[1]
}

function speed() -> int {
    match mode {
        case Idle { return 0 }
        case Run(s) { return s }
    }
}

function longest() -> str {
    var best = ""
    for name in names {
        if len(name) > len(best) { best = name }
    }
    return best
}

function tune(by: int) {
    config.gain += by
    table[0] = table[0] + by
    grid[1].append(by)
}

function has_spare() -> bool { return spare is not none }

function count(xs: list[int]) -> int { return len(xs) }

let pick = function(i: int) -> int { return table[i] + config.gain }
let channel = Channel(5)

print(total(), in_range(50), in_range(500), speed(), longest(), has_spare())
print(channel.scaled(2), pick(1))
tune(2)
print(total(), channel.scaled(0), pick(0), count(table), count(grid[1]))
print(table.map(function(v: int) -> int { return v + config.gain }))
print(table)
print(config.gain)
print(grid)
"##;

    #[test]
    fn a_let_at_the_top_of_the_file_can_be_read_by_every_function() {
        // With no collector the top of the file owns each table, and
        // frees it when the program ends: 0 blocks left.
        no_collector_as_interpreter("top-tables", TOP_TABLES);
    }

    /// A class that looks after what it owns through its own methods:
    /// they take items out, and put new values in the place of old ones.
    pub(crate) const METHODS_TAKE_OUT: &str = r##"# A class that looks after what it owns, through its own methods.
class Task {
    name: str
    runs: list[int]
}

class Queue {
    tasks: list[Task]
    last: Task or none
    log: list[int]

    function add(self, task: take Task) {
        self.tasks.append(task)
    }

    # Takes an item out of a list the object owns.
    function next(self) -> Task or none {
        if len(self.tasks) == 0 { return none }
        return self.tasks.pop()
    }

    # Puts a new value in the place of an old one, which is freed.
    function remember(self, task: take Task) {
        self.last = task
    }

    function replace_first(self, task: take Task) {
        self.tasks[0] = task
    }

    function reset(self) {
        self.log = []
        self.last = none
    }

    # Calls another method that takes out.
    function run_one(self) -> bool {
        let task = self.next()
        if task is none { return false }
        task.runs.append(len(self.log))
        self.log.append(len(task.name))
        self.remember(task)
        return true
    }

    function size(self) -> int { return len(self.tasks) }

    function describe_last(self) -> str {
        match self.last {
            case none { return "nothing" }
            case task { return f"{task.name} {task.runs}" }
        }
    }
}

function drain(queue: take Queue) -> int {
    var count = 0
    while queue.run_one() { count += 1 }
    return count
}

let queue = Queue([], none, [])
queue.add(Task("blink", []))
queue.add(Task("read", []))
queue.add(Task("send", []))
queue.replace_first(Task("first", [9]))
print(queue.size())
let ran = queue.run_one()
print(ran, queue.size(), queue.describe_last())
queue.reset()
print(queue.log, queue.last is none)
let taken = queue.next()
if taken is not none { print(taken.name) }
print(drain(queue))
"##;

    #[test]
    fn a_method_can_take_out_what_its_object_owns() {
        // Each item that leaves has one new owner, and each value that
        // is replaced is freed on the spot: 0 blocks left.
        no_collector_as_interpreter("methods-take-out", METHODS_TAKE_OUT);
    }

    /// The methods of text and lists that the standard library adds
    /// (docs/library.md): on text, on a list of objects that the list
    /// owns, on a list of text and on lists of numbers.
    pub(crate) const LIBRARY: &str = r##"# The methods of text and lists, in a program that owns what it holds.
class Task {
    name: str
    priority: int
}

class Queue {
    tasks: list[Task]

    function add_first(self, task: take Task) { self.tasks.insert(0, task) }
    function drop_all(self) { self.tasks.clear() }
    function take_at(self, at: int) -> Task { return self.tasks.pop(at) }
    function names(self) -> str {
        var names: list[str] = []
        for task in self.tasks { names.append(task.name) }
        return names.join(" ")
    }
}

# `find` answers with a position or `none`, and `slice` takes a part.
function first_word(line: str) -> str {
    let space = line.find(" ")
    if space is none { return line }
    return line.slice(0, space)
}

function rest(line: str) -> str {
    match line.find(" ") {
        case none { return "" }
        case at { return line.slice(at + 1) }
    }
}

let text = "hello, wörld"
print(text.find("l"))
print(text.find("xyz"))
print(text.find("rld"))
print(text.find(""))
print(text.slice(7))
print(text.slice(0, 5))
print(text.slice(12) == "")
print("A".code())
print("ö".code())
print("€".code())
print(char(66))
print(char(246))
print(char(8364) + char(128512))
print(first_word("set speed 30"))
print(first_word("stop"))
print(rest("set speed 30"))
print(rest("stop") == "")
let letters = "zeno".slice(1, 3) + char("a".code() + 1)
print(letters)
var total = 0
for ch in "abc" {
    total += ch.code()
}
print(total)

# A list of objects: each has one owner, the list.
let queue = Queue([Task("b", 2), Task("c", 3)])
queue.add_first(Task("a", 1))
queue.tasks.insert(3, Task("d", 4))
print(queue.names())
let second = queue.take_at(1)
print(second.name, second.priority)
queue.tasks.reverse()
print(queue.names())
print(queue.tasks.find(Task("d", 4)))
print(queue.tasks.find(Task("zz", 0)))
let gone = queue.tasks.pop(0)
print(gone.name)
print(len(queue.tasks))
queue.drop_all()
print(len(queue.tasks))
queue.add_first(Task("again", 9))
print(queue.names())

# A list of text keeps a copy of each item.
var lines = ["delta", "alpha"]
let word = "char" + "lie"
lines.insert(1, word)
lines.insert(0, first_word("bravo two"))
print(lines)
let taken = lines.pop(2)
print(taken)
let part = lines.slice(1)
print(part)
print(len(part))
print(lines.slice(0, 0))
lines.sort()
print(lines)
print(lines.join("+"))
print(lines.find("bravo"))
print(lines.find("zulu"))
lines.clear()
print(lines)
lines.append(taken + "!")
print(lines)
let empty: list[str] = []
print(empty.join(", ") == "")

# Numbers.
let numbers = [5, 3, 9, 1, 4, 1, 8, 2, 7, 6, 0]
numbers.sort()
print(numbers)
numbers.reverse()
print(numbers)
print(numbers.slice(2, 5))
print(numbers.pop(0))
numbers.insert(len(numbers), 42)
print(numbers)
let halves = [2.5, -1.0, 0.5, 0.0, -0.0]
halves.sort()
print(halves)
let one = [7]
one.sort()
one.reverse()
print(one)
print([1, 2, 3].map(function(n: int) -> str { return str(n) }).join("-"))

# A loop walks the list as it was when the loop began.
let readings = [40, 10, 30, 20]
for reading in readings {
    readings.sort()
    print(reading)
}
print(readings)
print(readings.find(30))
"##;

    #[test]
    fn text_and_list_methods_are_the_same_in_every_build() {
        // Through C and through LLVM, ordinary, `--firmware` and with no
        // collector, where `clear`, `pop(at)` and `insert` must leave 0
        // blocks behind.
        no_collector_as_interpreter("library", LIBRARY);
        // A program that uses them for a job: lines from a sensor.
        no_collector_as_interpreter("sentence", include_str!("../examples/sentence.zeno"));
    }

    const SORT_BY: &str = r##"class Person {
    name: str
    age: int
}
let people = [Person("Ada", 36), Person("Bo", 20), Person("Cy", 36), Person("Di", 5)]
people.sort(function(p: Person) -> int { return p.age })
for p in people { print(p.name, p.age) }
people.sort(function(p: Person) -> str { return p.name })
for p in people { print(p.name) }
let words = ["pear", "fig", "banana", "kiwi", "apple"]
words.sort(function(w: str) -> int { return len(w) })
words.sort(function(w: str) -> str { return w.slice(1) })
print(words)
words.sort(function(w: str) -> int { return len(w) })
print(words)
let numbers = [3, -7, 1, -2]
numbers.sort(function(n: int) -> int { return abs(n) })
print(numbers)
numbers.sort(function(n: int) -> float { return -1.0 * n })
print(numbers)
function make() -> list[int] { return [5, 3, 4] }
let made = make()
made.sort(function(n: int) -> int { return 0 - n })
print(made)
let empty: list[int] = []
empty.sort(function(n: int) -> int { return n })
print(empty)
let floats = [2.5, -1.0, 2.5, 0.0]
floats.sort(function(x: float) -> float { return x * x })
print(floats)
"##;

    #[test]
    fn sort_with_a_function_is_the_same_in_every_build() {
        // The items go in the order of what the function gives for
        // each, and those with the same stay as they were. Through C
        // and LLVM, and with no collector the keys leave 0 blocks.
        no_collector_as_interpreter("sort-by", SORT_BY);
        // A function that changes the list while it is being sorted.
        let changed = r##"var items = [3, 1, 2]
try {
    items.sort(function(n: int) -> int {
        items.append(n)
        return n
    })
} except ValueError as e {
    print("caught:", e.message)
}
print(len(items))
"##;
        same_as_interpreter("sort-changed", changed);
        crate::emit_llvm::tests::same_as_interpreter("sort-changed", changed);
    }

    #[test]
    fn a_position_outside_the_text_or_the_list_is_an_error_in_every_build() {
        // An ordinary build catches these, as `zeno run` does.
        same_as_interpreter(
            "library-caught",
            "let xs = [1, 2, 3]\n\
             try { xs.insert(9, 1) } except IndexError as e { print(e.message) }\n\
             try { print(xs.pop(3)) } except IndexError as e { print(e.message) }\n\
             try { print(xs.slice(2, 1)) } except IndexError as e { print(e.message) }\n\
             try { print(xs.slice(4)) } except IndexError as e { print(e.message) }\n\
             try { print(\"hello\".slice(0, 9)) } except IndexError as e { print(e.message) }\n\
             try { print(char(0)) } except ValueError as e { print(e.message) }\n\
             try { print(\"\".code()) } except ValueError as e { print(e.message) }\n\
             print(xs)\n",
        );
        // Not caught, each stops the program with the same words at the
        // same place, in a firmware build too.
        for (name, source) in [
            ("library-slice", "let text = \"hello\"\nprint(text.slice(2, 9))\n"),
            ("library-insert", "let xs = [1, 2]\nxs.insert(-1, 5)\n"),
            ("library-pop", "let xs = [\"a\"]\nprint(xs.pop(1))\n"),
            ("library-code", "let text = \"ab\"\nprint(text.code())\n"),
            ("library-char", "let n = 1114112\nprint(char(n))\n"),
        ] {
            same_as_interpreter(name, source);
        }
    }

    /// The `math` module, past `sqrt`, `floor`, `ceil`, `sin` and `cos`.
    const MATH: &str = r#"import math
from math import atan2, e, log

function turn(angle: rad) -> deg {
    return (angle * 2).deg
}

print(math.e, e, math.pi)
print(math.tan(0), math.tan(1), math.tan(45 deg), math.tan(math.pi / 4))
print(math.sin(30 deg), math.cos(60 deg), math.sin(90 deg), math.cos(180 deg), math.sin(0.5 rad), math.sin(0.5))
print(math.asin(1), math.acos(1), math.asin(0.5), math.acos(-1), math.atan(1), math.atan(-1e300))
print(math.atan2(1, 1), math.atan2(-1, -1), math.atan2(0, -1), atan2(1.5, 0), math.atan2(0, 0))
print(math.asin(1).deg, math.atan2(1, 1).deg, math.acos(0.5).deg, (180 deg).rad, (90 deg).rad, (1 rad).deg)
print(math.exp(0), math.exp(1), math.exp(-1), math.exp(710), math.exp(2.5))
print(math.log(1), math.log(math.e), log(10), math.log2(8), math.log2(1e-3), math.log10(1000), math.log10(2))
print(math.round(2.5), math.round(-2.5), math.round(2.4), math.round(-2.6), math.round(0.5), math.round(-0.5))
print(math.round(3), math.round(1e30), math.round(-1e30), math.round(0.49999999999999994))
print(math.sin(math.asin(0.25)), math.cos(math.atan2(3, 4)), math.tan(math.atan(2)))
let heading = math.atan2(1, -1)
print(heading, heading.deg, heading.deg > 90 deg, heading * 2, heading / 1 rad)
print(turn(math.atan(1)))
var total = 0.0
for step in range(360) {
    total += math.sin(step * 1 deg) * math.sin(step * 1 deg)
}
print(total)
let angles = [0 deg, 30 deg, 45 deg, 60 deg, 90 deg]
for angle in angles {
    print(angle, math.sin(angle), math.cos(angle), math.tan(angle.rad))
}
"#;

    #[test]
    fn more_of_math_is_the_same_in_every_build() {
        no_collector_as_interpreter("math-more", MATH);
        no_collector_as_interpreter("instruments", include_str!("../examples/instruments.zeno"));
        // What can be caught, where errors jump. A firmware build says
        // that nothing in the `try` can be caught, and is skipped.
        same_as_interpreter(
            "math-caught",
            "import math\n\
             for x in [8.0, 0.0, -2.5] {\n\
             \x20   try {\n        print(math.log2(x))\n    } except ValueError as error {\n        print(error.message)\n    }\n\
             \x20   try {\n        print(math.asin(x / 8))\n    } except ValueError as error {\n        print(error.message)\n    }\n\
             }\n\
             try {\n    print(math.acos(1.0000001))\n} except ValueError as error {\n    print(error.message)\n}\n",
        );
        // Left alone, each stops the program in every build, with the
        // same words at the same place.
        for (name, source) in [
            ("math-log", "import math\nlet x = 0\nprint(math.log(x))\n"),
            ("math-log10", "import math\nlet x = -0.001\nprint(math.exp(1))\nprint(math.log10(x))\n"),
            ("math-asin", "import math\nlet x = 1.5\nprint(math.asin(x))\n"),
            ("math-acos", "from math import acos\nlet x = -2\nprint(acos(x))\n"),
        ] {
            same_as_interpreter(name, source);
        }
    }

    /// A dict: writing one, reading, storing, `in`, `len`, loops, `get`,
    /// `pop`, `keys`, `values`, `clear`, `==`, and one big enough for its
    /// index to grow several times.
    const DICT: &str = r#"let ages = ["Ada": 36, "Alan": 41]
print(ages)
print(ages["Ada"], len(ages))
ages["Grace"] = 45
ages["Ada"] = 37
print(ages)
print("Alan" in ages, "Bob" in ages, "Bob" not in ages)
for name in ages {
    print(name, ages[name])
}
print(ages.get("Bob"), ages.get("Ada"), ages.get("Bob", 0), ages.get("Grace", 0))
let gone = ages.pop("Alan")
print(gone, ages, ages.keys(), ages.values())
let empty: dict[str, int] = [:]
print(empty, len(empty), empty == ages)
let same = ["Grace": 45, "Ada": 37]
print(same == ages, same != ages)
ages.clear()
print(ages, len(ages))

let names: dict[int, str] = [:]
names[3] = "three"
names[1] = "one"
names[3] = "THREE" + "!"
print(names, names[1], names.get(2), names.get(2, "none there"))
for n in names {
    print(n, names[n])
}
let flags = ["a": true, "b": false]
let sizes = ["x": 1.5, "y": 2.0]
print(flags, sizes, flags["a"], sizes["y"], sizes.get("z"), flags.get("b"))

function count(words: list[str]) -> dict[str, int] {
    let seen: dict[str, int] = [:]
    for word in words {
        seen[word] = seen.get(word, 0) + 1
    }
    return seen
}
let counts = count("the cat and the hat and the bat".split())
print(counts)
var big: dict[int, int] = [:]
for i in range(200) {
    big[i * 7] = i
}
var total = 0
for k in big {
    total += big[k]
}
print(len(big), total, big[693], 694 in big)
for i in range(0, 200, 2) {
    big.pop(i * 7)
}
print(len(big), big.get(7), big.get(14))
"#;

    /// Dicts and their owners: in a field, given back by a function, lent
    /// to one, made in the middle of a statement, and one that may be
    /// `none`.
    const DICT_OWNERS: &str = r#"class Shop {
    name: str
    stock: dict[str, int]

    function add(self, item: str, count: int) {
        self.stock[item] = self.stock.get(item, 0) + count
    }

    function total(self) -> int {
        var sum = 0
        for item in self.stock {
            sum += self.stock[item]
        }
        return sum
    }
}

function make() -> dict[str, str] {
    let colours = ["sky": "blue", "grass": "green"]
    return colours
}

function show(d: dict[str, str]) {
    for key in d {
        print(f"{key} is {d[key]}")
    }
    print(len(d), d)
}

function longest(d: dict[str, str]) -> str {
    var best = ""
    for key in d {
        if len(d[key]) > len(best) {
            best = d[key]
        }
    }
    return best
}

let shop = Shop("corner", [:])
shop.add("apple", 3)
shop.add("pear", 2)
shop.add("apple", 4)
print(shop.stock, shop.total(), shop.name)

var colours = make()
show(colours)
colours["sea"] = "dark " + colours["sky"]
colours["sky"] = colours["sky"]
print(longest(colours), str(colours), f"{colours}")
let taken = colours.pop("grass")
print(taken, colours)
colours = ["one": "1"]
print(colours)
show(make())
for key in make() {
    print(key)
}
for key in ["z": 26, "y": 25] {
    print(key)
}
print(len(["a": 1, "b": 2]), ["a": 1] == ["a": 1], ["a": 1] == ["a": 2], ["a": 1].get("a"))
var maybe: dict[str, int] or none = none
print(maybe)
maybe = ["k": 1]
if maybe is not none {
    print(maybe["k"])
}
let words = colours.keys()
let texts = colours.values()
print(words, texts, words.join("+"))
let codes = [404: "missing", 200: "fine"]
colours.clear()
print(colours)
print(codes[200])
print(codes.get(7), 7 in codes)
"#;

    /// Dicts in a list, a dict that a function takes over, int keys below
    /// zero, keys with every kind of character in them, and comprehensions.
    const DICT_SHAPES: &str = r#"let scores: dict[str, int] = [:]

function record(name: str, points: int) {
    scores[name] = scores.get(name, 0) + points
}

function first_over(d: dict[str, int], limit: int) -> str or none {
    for key in d {
        if d[key] > limit {
            return key
        }
    }
    return none
}

function drain(d: take dict[str, int]) -> int {
    var sum = 0
    for key in d.keys() {
        sum += d.pop(key)
    }
    return sum
}

function build(n: int) -> dict[int, str] {
    let made: dict[int, str] = [:]
    for i in range(n) {
        made[i - 2] = f"no. {i}"
    }
    return made
}

record("ann", 5)
record("bob", 7)
record("ann", 1)
print(scores, first_over(scores, 6), first_over(scores, 60))

let rows = [["a": 1], ["b": 2, "c": 3]]
print(rows, len(rows[1]), rows[1]["c"])
rows[0]["z"] = 26
rows.append(["q": 9])
for row in rows {
    for key in row {
        print(key, row[key])
    }
}
let squares = build(5)
print(squares, squares[-2], squares.get(2), -1 in squares, 5 in squares)
let total = drain(["x": 1, "y": 2, "z": 3])
print(total)
let mine = ["p": 10, "q": 20]
print(drain(mine))
let odd = ["": 0, "héllo wörld": 1, "a b": 2, "tab\there": 3]
print(odd, odd[""], odd["héllo wörld"])
let a = [1: "x", 2: "y", 3: "z"]
let b = [3: "z", 1: "x", 2: "y"]
print(a == b, a != b, a.keys() == b.keys())
let key = "k" + str(4)
let dyn = [key: 4.5]
print(dyn, f"k{2 + 2}" in dyn, dyn[f"k{4}"])
let doubled = [n * 2 for n in a]
let labels = [a[n].upper() for n in a if n != 2]
print(doubled, labels)
var count = 0
for word in "to be or not to be".split() {
    record(word, 1)
    count += 1
}
print(scores, count)
print(scores.get("zed"))
"#;

    /// Leaving early with a dict in hand: `return` and `break` inside a
    /// loop over one, an error raised while one is being filled, a new
    /// dict given to a variable on every trip, and a function written
    /// inside the program that changes one.
    const DICT_EXITS: &str = r#"function fill(d: dict[str, str], n: int) {
    for i in range(n) {
        d[f"key{i}"] = f"value {i * i}"
    }
}

function find(d: dict[str, str], wanted: str) -> str or none {
    for key in d {
        if d[key] == wanted {
            return key
        }
        if key == "key7" {
            break
        }
    }
    return none
}

function lookup(d: dict[str, str], key: str) -> str or none {
    return d.get(key)
}

function maybe(n: int) -> dict[int, bool] or none {
    if n < 0 {
        return none
    }
    let made = [n: true]
    made[n + 1] = false
    return made
}

function parse(text: str) -> dict[str, int] raises ValueError {
    let out: dict[str, int] = [:]
    for part in text.split(",") {
        let pair = part.split("=")
        if len(pair) != 2 {
            raise ValueError(f"bad part: {part}")
        }
        out[pair[0].strip()] = int(pair[1])
    }
    return out
}

let table: dict[str, str] = [:]
fill(table, 10)
print(len(table), find(table, "value 9"), find(table, "value 81"), find(table, "nothing"))
let hit = lookup(table, "key4")
if hit is not none {
    print(hit, len(hit))
}
print(lookup(table, "key44"))
let v = table.get("key2")
if v is not none {
    print(v.upper())
}
var current = ["a": "1"]
for round in range(3) {
    current = [f"r{round}": str(round), "same": "x"]
    current["same"] = current["same"] + str(round)
}
print(current)
let yes = maybe(4)
let no = maybe(-1)
print(yes, no, yes == no)
if yes is not none {
    print(yes[5], len(yes))
}
try {
    print(parse("a=1, b=22 ,c = 3"))
    print(parse("a=1,b"))
} except ValueError as e {
    print("caught", e.message)
}
let owned_one: owned dict[str, int] = ["o": 1]
owned_one["p"] = 2
print(owned_one, owned_one.values())
let counter = ["n": 0]
let bump = function() {
    counter["n"] = counter["n"] + 1
}
bump()
bump()
print(counter)
let lookups = [table.get(f"key{i}", "?") for i in range(8, 12)]
print(lookups)
print(table.get("key99"))
"#;

    /// A program in three files. `test.zeno` imports `shapes` and
    /// `drivers.uart`, and `drivers/uart.zeno` imports `shapes` too.
    const MODULES: &str = r#"import shapes
import drivers.uart
from shapes import Point, Circle, Shape

function describe(shape: Shape) -> str {
    match shape {
        case shapes.Dot { return "a dot" }
        case Circle(r) { return f"a circle of {r}" }
        case Shape.Rect(w, h) { return f"a rect {w} by {h}" }
    }
}

let p = Point(3.0, 4.0)
let q: shapes.Point = shapes.Point(6.0, 8.0)
print(p, p.far(), q.far(), shapes.origin, shapes.SIDES)
let all: list[Shape] = [shapes.Shape.Dot, Circle(1.0), shapes.Rect(2.0, 3.0), shapes.square(2.0)]
for shape in all {
    print(describe(shape), shapes.area(shape), shape)
}
print(shapes.count(), drivers.uart.send("hello"), drivers.uart.speed, drivers.uart.centre())
try {
    print(shapes.check(2.5))
    print(shapes.check(-1.0))
} except shapes.BadShape as bad {
    print("caught", bad.why)
}
let area = 5
print(area, [shapes.area(s) for s in all if s != shapes.Dot], shapes.count())
==> shapes.zeno
import math

const SIDES = 4
let origin = Point(0.0, 0.0)
var made = 0
print("shapes is ready")

class Point {
    x: float
    y: float

    function far(self) -> float {
        return math.sqrt(self.x * self.x + self.y * self.y)
    }
}

enum Shape {
    Dot
    Circle(radius: float)
    Rect(w: float, h: float)
}

class BadShape {
    why: str
}

function area(shape: Shape) -> float {
    made += 1
    match shape {
        case Dot { return 0.0 }
        case Circle(r) { return math.pi * r * r }
        case Rect(w, h) { return w * h }
    }
}

function check(side: float) -> float raises BadShape {
    if side < 0.0 {
        raise BadShape("a side cannot be negative")
    }
    return side
}

function count() -> int {
    return made
}

function square(side: float) -> Shape {
    return Rect(side, side)
}
==> drivers/uart.zeno
import shapes

let speed = 9600
print("uart is ready")

function send(text: str) -> int {
    print(f"uart at {speed}: {text}")
    return len(text)
}

function centre() -> shapes.Point {
    return shapes.Point(shapes.origin.x, shapes.origin.y)
}
"#;

    /// Generic classes and functions that live in an imported file.
    const MODULES_GENERIC: &str = r#"import stack
from stack import Stack, first

let s: Stack[int] = Stack([])
s.push(4)
s.push(9)
print(s, s.top(), first(["a", "b"]), stack.first([1.5]), stack.over(2), stack.over(9))
let names: stack.Stack[str] = Stack([])
names.push("x")
print(names, names.top())
let empty: Stack[float] = Stack([])
try {
    print(empty.top())
} except stack.Empty as e {
    print("empty:", e.why)
}
print(stack.ratio(7, 2))
==> stack.zeno
class Stack[T] {
    items: list[T]

    function push(self, item: T) {
        self.items.append(item)
    }

    function top(self) -> T raises Empty {
        if len(self.items) == 0 {
            raise Empty("nothing on the stack")
        }
        return self.items[len(self.items) - 1]
    }
}

class Empty {
    why: str
}

function first[T](items: list[T]) -> T {
    return items[0]
}

let limit = 3

function over(n: int) -> bool {
    return n > limit
}

function ratio(a: int, b: int) -> int {
    return a // b
}
"#;

    /// Interfaces where objects are shared: two interfaces on one class,
    /// a function type, a generic class, `==`, lists of lists.
    const INTERFACES: &str = r##"interface Named {
    function name(self) -> str
}

interface Sized {
    function size(self) -> int
}

interface Marker {
}

class Box is Named, Sized, Marker {
    label: str
    count: int

    function name(self) -> str {
        return self.label
    }

    function size(self) -> int {
        return self.count
    }
}

class Tag is Named, Marker {
    text: str

    function name(self) -> str {
        return f"#{self.text}"
    }
}

class Plain is Marker {
}

class Pair[T] {
    first: T
    second: T
}

class Holder[T] is Sized {
    items: list[T]

    function size(self) -> int {
        return len(self.items)
    }
}

function longest(items: list[Named]) -> Named or none {
    var best: Named or none = none
    for item in items {
        if best is none {
            best = item
        } else if len(item.name()) > len(best.name()) {
            best = item
        }
    }
    return best
}

function describe(thing: Sized) -> str {
    return f"size {thing.size()}"
}

function apply(thing: Named, how: function(Named) -> str) -> str {
    return how(thing)
}

let box = Box("crate", 4)
let tag = Tag("red")
let named: list[Named] = [box, tag, Tag("a very long one")]
let best = longest(named)
if best is not none {
    print(best.name())
    print(best)
}
print(describe(box), describe(Holder([1, 2, 3])), describe(Holder(["a"])))
let a: Named = box
let b: Named = Box("crate", 4)
let c: Named = tag
print(a == b, a == c, a != c)
let pair: Pair[Named] = Pair(box, tag)
print(pair.first.name(), pair.second.name())
print(pair)
print(apply(tag, function(thing: Named) -> str { return thing.name() + "!" }))
let empty: list[Named] = []
print(longest(empty) is none)
var names: list[str] = []
for item in named {
    names.append(item.name())
}
print(names)
var lists: list[list[Named]] = [[box], [tag, box]]
lists.append([Tag("last"), box])
print(len(lists[1]), lists[2][0].name())
let marked: list[Marker] = [box, tag, Plain()]
print(marked, marked[0] == marked[1], marked[2] == marked[2])
"##;

    /// A list that is written out is built as what the place wants: a
    /// `1` given where a `list[int or none]` goes is kept as an `int or
    /// none`, in an argument as in a `let`.
    const LISTS_AS_WANTED: &str = r#"class Row {
    cells: list[str or none]
}

function show(items: list[int or none]) -> int {
    print(items)
    return len(items)
}

function first[T](items: list[T]) -> T {
    return items[0]
}

function same[T](item: T) -> T {
    return item
}

function make() -> list[float or none] {
    return [1.5, 2.5]
}

print(show([1, none]))
print(show([1, 2]))
let count = 5
let n: int or none = first([1, 2])
let m: int or none = same(count)
print(n, m)
let row = Row(["a", "b"])
row.cells = ["x"]
print(row)
let nested: list[list[int or none]] = [[1, 2], [none]]
print(nested)
print(make())
var later: list[int or none] = []
later = [4, 5]
later.append(none)
print(later)
var grid: list[list[int or none]] = []
grid.append([7, 8])
grid.insert(0, [9])
print(grid)
let maybe: list[int or none] or none = [1]
print(maybe)
"#;

    #[test]
    fn an_interface_is_the_same_in_every_build() {
        // The example gives each object one owner, so it goes through
        // every build: the interpreter, the VM, and C and LLVM of all
        // three kinds. With no collector every block is given back.
        let ports = include_str!("../examples/ports.zeno");
        no_collector_as_interpreter("ports", ports);
        // Its `send` raises through the interface, and a firmware build
        // carries that as a value.
        firmware_as_interpreter("ports-firmware", ports);
        same_as_interpreter("interfaces", INTERFACES);
        // An interface in one file and a class that is it in another.
        no_collector_as_interpreter(
            "interfaces-modules",
            "import ports\n\n\
             class Uart is ports.Port {\n    speed: int\n\n    function send(self, text: str) -> int {\n        return len(text)\n    }\n}\n\n\
             let all: list[ports.Port] = [Uart(9600), ports.Null()]\n\
             print(ports.send_all(all, \"hello\"))\n\
             print(ports.send_all([Uart(1), Uart(2)], \"hi\"))\n\
             print(all)\n\
             ==> ports.zeno\n\
             interface Port {\n    function send(self, text: str) -> int\n}\n\n\
             class Null is Port {\n    function send(self, text: str) -> int {\n        return 0\n    }\n}\n\n\
             function send_all(ports: list[Port], text: str) -> int {\n    var sent = 0\n    \
             for port in ports {\n        sent += port.send(text)\n    }\n    return sent\n}\n",
        );
        same_as_interpreter("lists-as-wanted", LISTS_AS_WANTED);
    }

    #[test]
    fn what_interfaces_left_out_is_the_same_in_every_build() {
        // Defaults on the functions of an interface, of every kind, called
        // through the interface and on the object (decisions 58.20 to
        // 58.22). Every build prints what `zeno run` prints, and with no
        // collector every block is given back.
        let source = include_str!("../tools/targets/edge-interfaces.zeno");
        same_as_interpreter("interfaces-left-out", source);
        firmware_as_interpreter("interfaces-left-out-firmware", source);
        no_collector_as_interpreter("interfaces-left-out-owned", source);
        crate::emit_llvm::tests::same_as_interpreter("interfaces-left-out", source);
    }

    #[test]
    fn an_interface_is_a_number_in_the_object_and_a_switch() {
        let c = compile(
            "interface Shape {\n    function area(self) -> float\n}\n\
             class Circle is Shape {\n    radius: float\n\n    function area(self) -> float {\n        return 3.0 * self.radius * self.radius\n    }\n}\n\
             class Square is Shape {\n    side: float\n\n    function area(self) -> float {\n        return self.side * self.side\n    }\n}\n\
             class Other {\n    x: int\n}\n\
             let shapes: list[Shape] = [Circle(1.0), Square(2.0)]\n\
             for shape in shapes {\n    print(shape.area())\n}\nprint(Other(1))\n",
        )
        .unwrap();
        // A class that is some interface carries its number first, where
        // the interface's own struct has it too.
        assert!(c.contains("struct z_Shape {\n    int32_t zeno_class;\n};"), "{c}");
        assert!(c.contains("struct z_Circle {\n    int32_t zeno_class;\n    double z_radius;\n};"));
        assert!(c.contains("    self->zeno_class = 1;\n"));
        // A class that is no interface is what it always was.
        assert!(c.contains("struct z_Other {\n    int64_t z_x;\n};"));
        // A call through the interface looks at the number and calls the
        // function of that class. No pointer to a function is kept.
        assert!(c.contains(
            "static double z_Shape_area(z_Shape *a0) {\n    switch (a0->zeno_class) {\n    \
             case 1: return z_Circle_area((z_Circle *)a0);\n    \
             default: return z_Square_area((z_Square *)a0);\n    }\n}\n"
        ));
    }

    #[test]
    fn a_program_in_several_files_is_the_same_in_every_build() {
        // The interpreter, the VM, and the C and LLVM builds of all three
        // kinds. The builds with no collector free every block.
        no_collector_as_interpreter("modules", MODULES);
        no_collector_as_interpreter("modules-generic", MODULES_GENERIC);
        // The example, which is four files in two folders.
        let station = [
            include_str!("../examples/station/main.zeno"),
            "==> sensors.zeno\n",
            include_str!("../examples/station/sensors.zeno"),
            "==> report.zeno\n",
            include_str!("../examples/station/report.zeno"),
            "==> drivers/uart.zeno\n",
            include_str!("../examples/station/drivers/uart.zeno"),
        ];
        no_collector_as_interpreter("station", &station.concat());
        // A module that keeps its insides to itself with `private`.
        let ring = [include_str!("../examples/ring/main.zeno"), "==> ring.zeno\n", include_str!("../examples/ring/ring.zeno")];
        no_collector_as_interpreter("ring", &ring.concat());
        // A program that needs a package: its own files, its
        // `zeno.package`, and the package in the folder beside it.
        let filters = [
            ("zeno.package", include_str!("../examples/filters/zeno.package")),
            ("filters.zeno", include_str!("../examples/filters/filters.zeno")),
            ("lowpass.zeno", include_str!("../examples/filters/lowpass.zeno")),
        ];
        let mut thermostat = [
            include_str!("../examples/thermostat/main.zeno"),
            "==> zeno.package\n",
            include_str!("../examples/thermostat/zeno.package"),
            "==> control.zeno\n",
            include_str!("../examples/thermostat/control.zeno"),
        ]
        .concat();
        // The package is also a program of its own.
        let mut package = include_str!("../examples/filters/main.zeno").to_string();
        for (name, text) in filters {
            thermostat += &format!("==> ../filters/{name}\n{text}");
            package += &format!("==> {name}\n{text}");
        }
        no_collector_as_interpreter("thermostat", &thermostat);
        no_collector_as_interpreter("filters", &package);
        // A fault inside an imported file stops the program in every
        // build with the same words, and names that file and its line.
        same_as_interpreter("modules-fault", &format!("{MODULES_GENERIC}==> more.zeno\n").replace(
            "print(stack.ratio(7, 2))\n",
            "print(stack.ratio(7, 2))\nprint(stack.ratio(1, 0))\n",
        ));
        // So does an error that nothing catches. It is named as its own
        // file names it: `Empty`, not `stack.Empty`.
        same_as_interpreter(
            "modules-uncaught",
            "import stack\nlet none_yet: stack.Stack[int] = stack.Stack([])\nprint(none_yet.top())\n\
             ==> stack.zeno\n\
             class Stack[T] {\n    items: list[T]\n\n    function top(self) -> T raises Empty {\n        \
             if len(self.items) == 0 {\n            raise Empty(\"nothing on the stack\")\n        }\n        \
             return self.items[0]\n    }\n}\n\nclass Empty {\n    why: str\n}\n",
        );
    }

    /// An enum whose cases have numbers (decisions 35.1 and 35.2): a
    /// case as its number, a number as a case or `none`, in a module of
    /// its own too, in every engine and build.
    #[test]
    fn the_numbers_of_an_enum_are_the_same_in_every_build() {
        same_as_interpreter(
            "enum-numbers",
            "enum Reply {\n    Ack = 6\n    Nak = 21\n    Old = -1\n    Big = 0x7FFF_FFFF_FFFF_FFFF\n}\n\
             print(int(Ack), int(Nak), int(Old), int(Big))\n\
             print(Reply(6), Reply(21), Reply(-1), Reply(5))\n\
             let byte: u8 = u8(Nak)\nprint(byte + 1)\n\
             function decode(code: int) -> str {\n    let reply = Reply(code)\n    if reply is none {\n        return \"unknown\"\n    }\n\
             \x20   match reply {\n        case Ack { return \"ack\" }\n        case Nak { return \"nak\" }\n        case _ { return \"other\" }\n    }\n}\n\
             for code in [6, 21, 7, -1] {\n    print(code, decode(code))\n}\n\
             let replies = [Ack, Old, Nak]\nprint(replies.map(function(r: Reply) -> int { return int(r) }))\n\
             print(Reply(int(Old)) == Old, Reply.Ack)\n\
             try {\n    print(u8(Old))\n} except OverflowError as e {\n    print(e)\n}\n",
        );
    }

    #[test]
    fn the_numbers_of_an_enum_follow_the_rules() {
        let refused = |source: &str| {
            let program = crate::modules::testing::load(source).program.expect("the program parses");
            let errors = crate::checker::checked(program, crate::checker::Rules::default()).1;
            errors.first().map(|error| error.message.clone()).unwrap_or_default()
        };
        assert_eq!(
            refused("enum A {\n    X = 1\n    Y\n}\n"),
            "`Y` has no number, and the other cases of `A` have one. Give every case a number, or none"
        );
        assert_eq!(
            refused("enum A {\n    X = 1\n    Y = 1\n}\n"),
            "`X` and `Y` are both 1. Each case needs a number of its own, so that a number gives back one case"
        );
        assert_eq!(
            refused("enum A {\n    X(v: int) = 1\n    Y = 2\n}\n"),
            "`X` carries values, so it cannot also stand for a number. Only an enum whose cases carry nothing can have numbers"
        );
        assert_eq!(
            refused("enum A {\n    X\n}\nprint(int(X))\n"),
            "the cases of `A` stand for no number. To turn a case into one, write a number for each case: `Ack = 6`"
        );
        assert_eq!(
            refused("enum A {\n    X = 1\n}\nlet a: A = A(1)\n"),
            "`a` should be A, but this is A or none. Check it with `is not none` first"
        );
        assert_eq!(refused("enum A {\n    X = 1\n}\nprint(X == 1)\n").is_empty(), false);
    }

    /// `program.exit`, `program.warn` and `program.environment`
    /// (decisions 34.1 to 34.3): the same printed text, the same lines on
    /// the error stream and the same number at the end, in both engines
    /// and in C and LLVM builds.
    #[test]
    fn a_program_ends_warns_and_reads_its_environment_alike_in_every_build() {
        let source = "import program\n\
            let word = program.environment(\"ZENO_TEST_WORD\")\n\
            if word is none {\n    print(\"no word\")\n} else {\n    print(\"the word is\", word, len(word))\n}\n\
            print(program.environment(\"ZENO_TEST_NEVER_SET\"))\n\
            program.warn(\"first\")\n\
            let big = 256\n\
            try {\n    program.exit(big)\n} except ValueError as e {\n    print(e)\n}\n\
            function stop(code: int) {\n    try {\n        program.exit(code)\n    } except {\n        print(\"caught\")\n    }\n}\n\
            print(\"before\")\n\
            program.warn(f\"ending with {1 + 2}\")\n\
            stop(3)\n\
            print(\"after\")\n";
        crate::value::testing::with_environment(&[("ZENO_TEST_WORD", "வணக்கம்")], || {
            let program = crate::modules::testing::program(source);
            let ran = crate::vm::run_both_ending(&program, source);
            assert_eq!(ran.result, Ok(()));
            assert_eq!(
                ran.output,
                "the word is வணக்கம் 7\nnone\na program ends with a number from 0 to 255, not 256\nbefore\n"
            );
            assert_eq!(ran.warned, "first\nending with 3\n");
            assert_eq!(ran.status, 3);
            let c = compile(source).expect("the program compiles");
            let llvm = crate::emit_llvm::tests::compile(source).expect("the program compiles through LLVM");
            let built = [
                ("C", built_output("ending", &c, &[])),
                ("LLVM", crate::emit_llvm::tests::built_output("ending", &llvm, Build::Ordinary, &[])),
            ];
            for (how, output) in built {
                let Some(output) = output else { continue };
                assert_eq!(String::from_utf8_lossy(&output.stdout), ran.output, "{how}");
                assert_eq!(String::from_utf8_lossy(&output.stderr), ran.warned, "{how}");
                assert_eq!(output.status.code(), Some(3), "{how}");
            }
        });
        // A firmware build ends and warns the same way. It has no
        // environment to read: a board has none.
        let source = "import program\nprint(\"one\")\nprogram.warn(\"two\")\nprogram.exit(4)\nprint(\"three\")\n";
        let firmware = compile_firmware(source).expect("a firmware build can end");
        if let Some(output) = built_output("ending-firmware", &firmware, &[]) {
            assert_eq!(String::from_utf8_lossy(&output.stdout), "one\n");
            assert_eq!(String::from_utf8_lossy(&output.stderr), "two\n");
            assert_eq!(output.status.code(), Some(4));
        }
        let program = crate::modules::testing::built("import program\nprint(program.environment(\"HOME\"))\n");
        let rules = crate::checker::Rules { strict: false, one_owner: false, firmware: true };
        let errors = crate::checker::checked(program, rules).1;
        assert_eq!(
            errors.first().map(|error| error.message.as_str()),
            Some(
                "a firmware build cannot use `program.environment`: a board has no environment to look in. \
                 Give the program what it needs as a `const`"
            )
        );
    }

    #[test]
    fn the_library_written_in_zeno_is_the_same_in_every_build() {
        // `math.hypot` and its neighbours, and `random`: one file of
        // Zeno each, that every engine and every build runs as it runs
        // the program's own files. The program tries all of both.
        no_collector_as_interpreter("library", include_str!("../tools/targets/library.zeno"));
        // A mistake inside the library stops the program in every
        // build with the same words, and names the library's file.
        same_as_interpreter(
            "library-fault",
            "import random\nprint(random.between(1, 6))\nprint(random.between(-9223372036854775807, 9223372036854775807))\n",
        );
        // A function of an imported file as a value. Its name has a
        // dot in it, which a name in C cannot have.
        let value = "import shapes\nlet f = shapes.area\nprint(f(1.0), [1.0, 2.0].map(shapes.area), shapes.area)\n\
                     ==> shapes.zeno\nfunction area(r: float) -> float { return r * 2.0 }\n";
        no_collector_as_interpreter("module-function-value", value);
        let c = compile(value).expect("it compiles");
        assert!(c.contains("static zeno_closure zeno_value_z_shapes_area = {(zeno_code)zeno_wrap_z_shapes_area, \"shapes.area\"};\n"), "{c}");
        // Only what the program reaches is in the C.
        let c = compile("import math\nprint(math.gcd(4, 6))\n").expect("it compiles");
        assert!(c.contains("static int64_t z_math_gcd(int64_t z_a, int64_t z_b)"), "{c}");
        assert!(!c.contains("z_math_hypot") && !c.contains("z_math_size") && !c.contains("z_math_clamp"), "{c}");
        assert!(c.contains("static const char *const modules[] = {\"library/math.zeno\"};"), "{c}");
        // And a program that reaches none of it has none of it: not
        // the file's name, and not a line that was not there before.
        let plain = compile("import math\nprint(math.sqrt(4.0))\n").expect("it compiles");
        assert!(!plain.contains("library/") && !plain.contains("z_math_"), "{plain}");
    }

    #[test]
    fn the_c_for_an_imported_file() {
        let c = compile(MODULES).expect("it compiles");
        // A name of an imported file has the module's name in it.
        assert!(c.contains("static double z_shapes_area(z_shapes_Shape *"), "{c}");
        assert!(c.contains("static int64_t z_drivers_uart_send(const char *z_text)"), "{c}");
        assert!(c.contains("static int64_t z_shapes_made;"), "{c}");
        // What the program prints is the name the file gave it.
        assert!(c.contains("case 0: return \"Dot\";"), "{c}");
        assert!(c.contains("zeno_join(5, \"Point(x=\", "), "{c}");
        // The files, for an error that stops the program in one of them.
        assert!(c.contains("    zeno_file = \"test.zeno\";\n    static const char *const modules[] = {\"shapes.zeno\", \"drivers/uart.zeno\"};\n    zeno_modules = modules;\n"), "{c}");
        // A position in the second file has its line counted on from
        // 1,000,000: this is line 31 of `stack.zeno`.
        let generic = compile(MODULES_GENERIC).expect("it compiles");
        assert!(generic.contains("zeno_floordiv(z_a, z_b, 1000031, 12)"), "{generic}");
        // A program in one file has none of this.
        assert!(!compile("print(1)").expect("it compiles").contains("zeno_modules = modules;"));
        // Two classes that C would know by one name are refused.
        assert_eq!(
            compile("import shapes\nclass shapes_Point {\n    y: int\n}\nprint(shapes.Point(1), shapes_Point(2))\n==> shapes.zeno\nclass Point {\n    x: int\n}\n").unwrap_err(),
            "`shapes_Point` and another class or enum of this program would both be called `z_shapes_Point` in C. \
             A copy of a generic class gets such a name: `Stack[int]` becomes `z_Stack_int`. So does a class \
             of an imported file: `Point` of the module `shapes` becomes `z_shapes_Point`. \
             Rename one of them, or build with `--llvm`"
        );
    }

    /// A field may have the name of a class or an enum. The function that
    /// makes the object has a parameter for each field, and one called as
    /// a type is would hide the type from the rest of that function.
    const NAMED_AS_A_TYPE: &str = r#"class point {
    point: int
    name: str
}
class Line {
    Dot: int
    point: point
    end: Dot
}
class Dot {
    Line: int
}
enum Shape {
    round(Shape: int, point: point)
    flat
}
class new_ {
    new: u8
}
let p = point(3, "here")
print(p, Line(1, p, Dot(2)), Shape.round(4, p), Shape.flat, new_(5))
"#;

    #[test]
    fn a_field_with_the_name_of_a_type_does_not_hide_the_type() {
        same_as_interpreter("named-as-a-type", NAMED_AS_A_TYPE);
        let c = compile(NAMED_AS_A_TYPE).expect("it compiles");
        // The parameter steps aside, and the field keeps its name.
        assert!(c.contains("static z_point *zeno_new_point(int64_t z2_point, const char *z_name)"), "{c}");
        assert!(c.contains("    self->z_point = z2_point;\n"), "{c}");
        // `Dot` is written below `Line`, and is a type all the same.
        assert!(c.contains("static z_Line *zeno_new_Line(int64_t z2_Dot, z_point *z2_point, z_Dot *z_end)"), "{c}");
        assert!(c.contains("static z_Shape *zeno_new_Shape_round(int64_t z2_Shape, z_point *z2_point)"), "{c}");
        // `new_` is `z_new` to C, which is what its field `new` would be.
        assert!(c.contains("    self->z_new = (uint8_t)z2_new;\n"), "{c}");
        // A class with no such field is written as it always was.
        let plain = compile("class Point {\n    x: int\n}\nprint(Point(1))\n").expect("it compiles");
        assert!(plain.contains("static z_Point *zeno_new_Point(int64_t z_x)"), "{plain}");
        // And with no collector, where an object has its own copy of its text.
        let owned = "class point {\n    point: int\n    name: str\n}\nenum Shape {\n    round(Shape: int, name: str)\n    flat\n}\n\
                     print(point(3, \"here\"), Shape.round(4, \"there\"), Shape.flat)\n";
        for (name, build) in [("no-collector", Build::NoCollector), ("firmware", Build::Firmware)] {
            let c = compile_minimal(owned, build, false);
            assert!(c.contains("zeno_text_keep(z_name)") != matches!(build, Build::Firmware), "{c}");
            let Some((printed, _, finished)) = build_and_run(&format!("named-as-a-type-{name}"), &c, &["-Os"]) else { return };
            assert!(finished, "{printed}");
            assert_eq!(printed, "point(point=3, name=\"here\") round(4, \"there\") flat\n");
        }
    }

    /// A loop with two names: over a dict, a list, text and a range.
    const DICT_PAIRS: &str = r#"let ages = ["Ada": 36, "Alan": 41, "Grace": 45]
for name, age in ages {
    print(name, age)
}
let words = ["zero", "one", "two"]
for i, word in words {
    print(i, word)
}
for i, ch in "héy" {
    print(i, ch)
}
for i, n in range(3) {
    print(i, n)
}
for i, n in range(5, 8) {
    print(i, n)
}
for i, n in range(10, 0, -4) {
    print(i, n)
}
function total(prices: dict[str, float]) -> float {
    var sum = 0.0
    for item, price in prices {
        if item == "skip" {
            continue
        }
        sum += price
    }
    return sum
}
print(total(["a": 1.5, "skip": 100.0, "b": 2.5]))
var counts: dict[int, int] = [:]
for i, n in [5, 5, 7] {
    counts[n] = counts.get(n, 0) + 1
}
for n, count in counts {
    counts[n + 100] = count
    print(n, count)
}
print(counts)
for key, flag in ["x": true] {
    print(key, flag)
}
let functions = [function(x: int) -> int { return x + 1 }]
for i, f in functions {
    print(i, f(i))
}
function find(words: list[str], wanted: str) -> int {
    for at, word in words {
        if word == wanted {
            return at
        }
    }
    return -1
}
print(find(words, "two"), find(words, "nine"))
"#;

    /// What a dict can hold: lists, objects, values of an interface,
    /// enums, dicts, quantities, functions and sized numbers.
    const DICT_ANY: &str = r#"interface Shape {
    function area(self) -> float
}

class Circle is Shape {
    radius: float

    function area(self) -> float {
        return 3.0 * self.radius * self.radius
    }
}

class Square is Shape {
    side: float

    function area(self) -> float {
        return self.side * self.side
    }
}

class Point {
    x: int
    y: int
}

enum Level {
    Low
    High(limit: int)
}

let lists: dict[str, list[int]] = ["odd": [1, 3], "even": [2]]
lists["even"].append(4)
lists["none"] = []
print(lists, lists["odd"][1], len(lists))
for name, numbers in lists {
    print(name, len(numbers))
}

let points = ["origin": Point(0, 0), "far": Point(9, 9)]
points["far"].x = 10
print(points, points["far"].x)
let found = points.get("origin")
if found is not none {
    print(found.y)
}
print(points.get("nowhere") is none)
let gone = points.pop("origin")
print(gone, points)

let shapes: dict[str, Shape] = ["round": Circle(1.0), "square": Square(2.0)]
shapes["big"] = Circle(10.0)
for name, shape in shapes {
    print(name, shape.area())
}
print(shapes)

let levels = [1: Low, 2: High(7)]
match levels[2] {
    case High(limit) {
        print("high", limit)
    }
    case Low {
        print("low")
    }
}
print(levels, levels == [1: Low, 2: High(7)], levels == [1: Low])

let nested: dict[str, dict[str, int]] = ["a": ["x": 1], "b": [:]]
nested["b"]["y"] = 2
print(nested, nested["a"]["x"])
let same = nested == ["a": ["x": 1], "b": ["y": 2]]
print(same)

let times = ["blink": 500 ms, "wait": 2000 ms]
print(times, times["wait"])
let wait = times.get("blink")
print(wait)

let steps: dict[str, function(int) -> int] = ["double": function(n: int) -> int { return n * 2 }]
print(steps["double"](21))
let values = lists.values()
print(values, points.values())
let bytes: dict[str, u8] = ["a": 200, "b": 7]
print(bytes, bytes["a"] + 100)
"#;

    /// A dict of a type parameter, in a generic function and class.
    const DICT_GENERIC: &str = r#"function first[T](d: dict[str, T], key: str) -> T {
    return d[key]
}
function flip[K, V](d: dict[K, V]) -> dict[V, K] {
    var out: dict[V, K] = [:]
    for key, value in d {
        out[value] = key
    }
    return out
}
function count[T](items: list[T]) -> dict[int, T] {
    var out: dict[int, T] = [:]
    for at, item in items {
        out[at] = item
    }
    return out
}
class Table[T] {
    rows: dict[str, T]

    function put(self, key: str, row: T) {
        self.rows[key] = row
    }

    function each(self) -> list[T] {
        return self.rows.values()
    }
}
print(first(["a": 1], "a"), first(["b": [1.5]], "b"))
print(flip(["one": 1, "two": 2]))
print(count(["x", "y"]), count([[1], [2, 3]]))
let table: Table[list[str]] = Table([:])
table.put("k", ["v"])
print(table.rows, table.each())
let n: int or none = first(["a": 1], "a")
print(n)
"#;

    /// The same with plain values, which a build with no collector takes.
    const DICT_GENERIC_PLAIN: &str = r#"function flip[K, V](d: dict[K, V]) -> dict[V, K] {
    var out: dict[V, K] = [:]
    for key, value in d {
        out[value] = key
    }
    return out
}
function tally[T](d: dict[str, T], key: str, instead: T) -> T {
    return d.get(key, instead)
}
print(flip(["one": 1, "two": 2]), tally(["a": 2.5], "b", 0.0), tally(["a": "x"], "a", ""))
"#;

    #[test]
    fn a_dict_is_the_same_in_every_build() {
        // Each of these also checks that a build with no collector frees
        // every dict, with its keys and its values: no block is left.
        no_collector_as_interpreter("dict", DICT);
        no_collector_as_interpreter("dict-owners", DICT_OWNERS);
        no_collector_as_interpreter("dict-shapes", DICT_SHAPES);
        no_collector_as_interpreter("dict-exits", DICT_EXITS);
        no_collector_as_interpreter("faults", include_str!("../examples/faults.zeno"));
        // A loop with two names, which lends each value and leaves the
        // dict whole, also where a `continue` or a `return` leaves early.
        no_collector_as_interpreter("dict-pairs", DICT_PAIRS);
        no_collector_as_interpreter("dict-generic-plain", DICT_GENERIC_PLAIN);
        // Values that are lists, objects and so on are shared, and need
        // the collector (decision 10.14).
        same_as_interpreter("dict-any", DICT_ANY);
        same_as_interpreter("dict-generic", DICT_GENERIC);
        same_as_interpreter("channels", include_str!("../examples/channels.zeno"));
        // A key that is not there can be caught where errors jump. A
        // firmware build says that nothing in the `try` can be caught,
        // and is skipped.
        same_as_interpreter(
            "dict-caught",
            "let colours = [\"sky\": \"blue\"]\nlet codes = [404: \"missing\"]\n\
             try {\n    print(colours[\"sea\"])\n} except KeyError as e {\n    print(e.kind, e.message)\n}\n\
             try {\n    print(colours.pop(\"sea\"))\n} except KeyError as e {\n    print(e.message)\n}\n\
             try {\n    print(codes[500])\n} except Error as e {\n    print(e.message)\n}\n\
             print(colours, codes)\n",
        );
        // Left alone, it stops the program in every build, with the same
        // words at the same place.
        for (name, source) in [
            ("dict-missing", "let ages = [\"Ada\": 36]\nprint(ages[\"Ada\"])\nprint(ages[\"Bob\"])\n"),
            ("dict-missing-int", "let names = [1: \"one\"]\nlet key = -7\nprint(names[key])\n"),
            ("dict-missing-pop", "let ages: dict[str, int] = [:]\nprint(len(ages))\nprint(ages.pop(\"\"))\n"),
        ] {
            same_as_interpreter(name, source);
        }
    }

    #[test]
    fn the_c_for_a_dict() {
        let source = "let ages = [\"Ada\": 36, \"Alan\": 41]\nages[\"Grace\"] = 45\n\
                      print(ages[\"Ada\"], \"Bob\" in ages, len(ages))\nfor name in ages {\n    print(name)\n}\n";
        let c = compile(source).unwrap();
        let program = &c[c.rfind("static void zeno_program(void)").expect("the program")..];
        // The dict is told what its keys and values are: text and ints.
        assert!(
            program.contains(
                "z_ages = zeno_dict_of('s', 'i', 2, (zeno_slot){.s = \"Ada\"}, (zeno_slot){.i = 36}, \
                 (zeno_slot){.s = \"Alan\"}, (zeno_slot){.i = 41});"
            ),
            "{program}"
        );
        assert!(program.contains("zeno_dict_set(z_ages, (zeno_slot){.s = \"Grace\"}, (zeno_slot){.i = t1});"), "{program}");
        // Reading says where in the program it is, for the KeyError.
        assert!(program.contains("zeno_dict_at(z_ages, (zeno_slot){.s = \"Ada\"}, 3, 7).i"), "{program}");
        assert!(program.contains("(zeno_dict_find(z_ages, (zeno_slot){.s = \"Bob\"}) >= 0)"), "{program}");
        assert!(program.contains("z_ages->keys->len"), "{program}");
        // The loop walks a list of the keys that is its own, and frees it.
        assert!(program.contains("zeno_list *t2 = zeno_dict_keys(z_ages);"), "{program}");
        assert!(program.contains("zeno_list_drop(t2);"), "{program}");
        // With a collector nothing frees the dict. With none, its owner does.
        assert!(!program.contains("zeno_dict_drop"), "{program}");
        // With two names the loop has a list of the values beside the
        // list of the keys, and one position for both. Over a list the
        // first name is that position.
        let pairs = "let ages = [\"Ada\": 36]\nfor name, age in ages {\n    print(name, age)\n}\n\
                     for at, n in [5, 6] {\n    print(at, n)\n}\nlet lists = [\"a\": [1.5]]\nprint(lists, lists == lists)\n";
        let c = compile(pairs).unwrap();
        let program = &c[c.rfind("static void zeno_program(void)").expect("the program")..];
        assert!(program.contains("zeno_list *t2 = zeno_dict_keys(t1);\n    zeno_list *t3 = zeno_dict_values(t1);"), "{program}");
        assert!(program.contains("const char *z_name = t4->items[t5].s;\n        int64_t z_age = t3->items[t5].i;"), "{program}");
        assert!(program.contains("int64_t z_n = t6->items[t7].i;\n        int64_t z_at = t7;"), "{program}");
        // A value that is not a plain one is a pointer to the dict, and
        // the compiler writes how such a value is shown and compared.
        assert!(program.contains("zeno_dict_of('s', 'p', 1, (zeno_slot){.s = \"a\"}, (zeno_slot){.p = zeno_list_of("), "{program}");
        assert!(program.contains("zeno_dict_str_with(z_lists, zeno_item_repr_list_float)"), "{program}");
        assert!(program.contains("zeno_dict_eq_with(z_lists, z_lists, zeno_item_eq_list_float)"), "{program}");
        let c = compile_no_collector(source).unwrap();
        let program = &c[c.rfind("static void zeno_program(void)").expect("the program")..];
        // The keys are text, and the list the loop walks has copies of them.
        assert!(program.contains("    zeno_list_drop_text(t"), "{program}");
        assert!(program.contains("    zeno_dict_drop(z_ages);\n}"), "{program}");
    }

    #[test]
    fn a_build_leaves_the_tests_out() {
        // The tests of a program are not in what is built, and are not
        // held to the rules of the build: this one shares a list, which
        // a build with no collector would refuse in the program itself.
        let source = "function double(n: int) -> int {\n    return n * 2\n}\n\n\
                      test \"double\" {\n    let shared = [1, 2]\n    let again = shared\n    \
                      assert double(len(shared) + len(again)) == 8\n    print(\"tested\")\n}\n\n\
                      print(double(21))\n";
        no_collector_as_interpreter("with-tests", source);
        let c = compile(source).unwrap();
        assert!(!c.contains("tested"), "the test is in the C");
        // The example has its tests in the module they test.
        no_collector_as_interpreter(
            "ring-tests",
            &format!(
                "{}==> ring.zeno\n{}",
                include_str!("../examples/ring/main.zeno"),
                include_str!("../examples/ring/ring.zeno")
            ),
        );
    }

    #[test]
    fn print_makes_its_texts_in_the_order_written() {
        // C does not say in which order the arguments of a call are
        // worked out, and making the text of a list takes room while it
        // is made. So beside a list each text gets a line of its own:
        // first the values that are made by a call, then the texts.
        let source = "let w = [\"x\", \"y\"]\nlet x = 2.5\nprint(w, x, w.join(\"+\"))\nprint(x, 3, len(w))\nprint(\"a\", w)\n";
        let c = compile(source).unwrap();
        let program = &c[c.rfind("static void zeno_program(void)").expect("the program")..];
        assert!(
            program.contains(
                "    const char *t1 = zeno_list_join(z_w, \"+\");\n    \
                 const char *t2 = zeno_list_str(z_w, zeno_item_repr_str);\n    \
                 const char *t3 = zeno_str_float(z_x);\n    \
                 zeno_print(3, t2, t3, t1);\n"
            ),
            "{program}"
        );
        // Numbers take no room while they are made, and one text alone
        // has no order to get wrong. Those stay in the call.
        assert!(program.contains("zeno_print(3, zeno_str_float(z_x), zeno_str_int(3), zeno_str_int(z_w->len));"), "{program}");
        assert!(program.contains("zeno_print(2, \"a\", zeno_list_str(z_w, zeno_item_repr_str));"), "{program}");
        // The two backends then use the text room alike (5.11t).
        no_collector_as_interpreter(
            "print-order",
            "function names() -> list[str] {\n    return [\"x\", \"y\"]\n}\n\
             let w = names()\nlet d = [\"one\": 1, \"two\": 2]\nlet x = 2.5\n\
             print(w, d, w.join(\"+\"))\nprint(d, x)\nprint(names(), x, f\"{x} and {d}\")\n",
        );
    }

    /// A place for a test's file that no other test uses.
    fn scratch(name: &str) -> String {
        crate::modules::shown(&std::env::temp_dir().join(format!("zeno-test-{}-{name}", std::process::id())))
    }

    /// The `files` module: every program here leaves no file behind, so
    /// each engine and each build starts from the same.
    const FILES: &str = r#"import files

function size(path: str) -> int raises FileError {
    return len(files.read(path))
}

function lines_in(path: str) -> int {
    try {
        return len(files.read(path).lines())
    } except FileError as error {
        print("lines_in:", error.message)
        return -1
    }
}

let path = "PATH"
print(files.exists(path))
files.write(path, "one\ntwo\n")
print(files.exists(path))
print(size(path))
print(lines_in(path))
files.append(path, "three")
print(files.read(path))
for line in files.read(path).lines() {
    print(f"[{line}]")
}
files.write(path, "")
print(size(path))
files.write(path, "தமிழ்\r\nok")
print(size(path))
let kept = files.read(path).lines()
print(len(kept))
print(kept[0])
print(kept[1] + "!")
files.remove(path)
print(lines_in(path))
try {
    files.remove(path)
} except FileError as error {
    print(error.message)
}
try {
    files.append("PATH/no/such/folder.txt", "x")
} except FileError as error {
    print(error.message)
}
try {
    print(files.read("FOLDER"))
} except FileError as error {
    print(error.message)
}
print(files.exists("FOLDER"))
try {
    print(size(path))
} except FileError as error {
    print("size:", error.message)
}
"#;

    #[test]
    fn files_are_the_same_in_every_build() {
        let folder = crate::modules::shown(&std::env::temp_dir());
        let source = FILES.replace("PATH", &scratch("every-build.txt")).replace("FOLDER", &folder);
        no_collector_as_interpreter("files", &source);
    }

    #[test]
    fn an_uncaught_file_error_is_the_same_in_every_build() {
        let path = scratch("uncaught.txt");
        same_as_interpreter("files-uncaught", &format!("import files\nprint(\"before\")\nprint(files.read(\"{path}\"))\n"));
        same_as_interpreter(
            "files-uncaught-in-function",
            &format!(
                "import files\nfunction first(path: str) -> str raises FileError {{\n    return files.read(path)\n}}\n\
                 print(first(\"{path}\"))\n"
            ),
        );
    }

    /// Folders (decisions 59.20 to 59.22). The program finds the folder as
    /// it left it, so every engine and every build prints the same.
    const FOLDERS: &str = r#"import files

function count(folder: str) -> int raises FileError {
    let names: owned list[str] = files.list(folder)
    return len(names)
}

let top = "TOP"
files.make_folder(files.join(top, "deep/er"))
files.make_folder(files.join(top, "deep/er/"))
files.make_folder(top)
for name in ["b.txt", "C.txt", "a b", "10", "9", "é", "ｚ", "😀"] {
    files.write(files.join(top, name), name)
}
let names: owned list[str] = files.list(top)
print(names)
for name in names {
    if name != "deep" {
        print(name, files.read(files.join(top, name)) == name)
    }
}
# A file whose name is not ASCII, made, listed, and the listed name
# opened in pieces: on Windows too, by the name it was given.
let inside = files.join(top, "deep/in")
files.make_folder(inside)
files.write(files.join(inside, "ü.txt"), "ü\nsecond\n")
let inner: owned list[str] = files.list(inside)
for name in inner {
    let source = files.read_from(files.join(inside, name))
    print(name, source.read_line())
}
print(count(top), count(files.join(top, "deep")), count(files.join(top, "deep/er")))
try {
    print(count(files.join(top, "none")))
} except FileError as error {
    print(error.message.ends_with("none`"))
}
try {
    print(count(files.join(top, "b.txt")))
} except FileError as error {
    print(error.message.starts_with("cannot list the folder"))
}
try {
    files.make_folder(files.join(top, "b.txt/below"))
} except FileError as error {
    print(error.message.starts_with("cannot make the folder"))
}
try {
    files.make_folder("")
} except FileError as error {
    print(error.message)
}
print(files.join("a", "b"), files.join("a/", "/b"), files.join("", "b"), files.join("a", ""))
# Each engine and build runs in the same folder, one after another, and
# on Windows a C build names a file through the code page where `zeno
# run` does not (59.20): so each takes away the files it made.
for name in names {
    if name != "deep" {
        files.remove(files.join(top, name))
    }
}
files.remove(files.join(inside, "ü.txt"))
"#;

    #[test]
    fn folders_are_the_same_in_every_build() {
        let top = scratch("folders");
        let _ = std::fs::remove_dir_all(&top);
        no_collector_as_interpreter("folders", &FOLDERS.replace("TOP", &top));
        let _ = std::fs::remove_dir_all(&top);
        // Uncaught, it stops the program as `files.read` does.
        same_as_interpreter("folders-uncaught", &format!("import files\nprint(files.list(\"{top}\"))\n"));
    }

    /// A program that lists no folder has no folder code in it.
    #[test]
    fn folders_are_only_in_a_program_that_uses_them() {
        let without = compile("import files\nprint(files.exists(\"x\"))\n").expect("it compiles");
        assert!(!without.contains("zeno_file_list") && !without.contains("opendir"), "folder code in a program without folders");
        let with = compile("import files\nprint(files.list(\".\"))\n").expect("it compiles");
        assert!(with.contains("zeno_file_list") && with.contains("opendir"), "no folder code in a program that lists a folder");
    }

    /// A file in pieces (decision 59.6): a thousand lines written by a
    /// `Writer`, every seventh with a Windows line end and the last with
    /// none, read by `for` and by `read_line`; a file opened again at
    /// once; the shapes a file can end in; bytes in pieces across the end;
    /// a writer that adds; and each thing that cannot be opened or read.
    const IN_PIECES: &str = r#"import files

let path = "PATH"

function if_seventh(i: int) -> str {
    if i % 7 == 0 {
        return "\r\n"
    }
    return "\n"
}

function write_lines(at: str) raises FileError {
    let out = files.write_to(at)
    for i in range(1000) {
        out.write(f"line {i}")
        if i == 999 {
            break
        }
        out.write(if_seventh(i))
    }
}

function read_all(at: str) -> list[str] raises FileError {
    let source = files.read_from(at)
    var lines: list[str] = []
    while true {
        let line = source.read_line()
        if line is none {
            break
        }
        lines.append(line)
    }
    return lines
}

function by_for(at: str) raises FileError {
    var count = 0
    var total = 0
    let source = files.read_from(at)
    for line in source {
        count += 1
        total += len(line)
        if line == "line 3" {
            continue
        }
        if count == 1000 {
            print("last:", line)
        }
    }
    print("by for:", count, total)
}

function by_read_line(at: str) raises FileError {
    let source = files.read_from(at)
    var count = 0
    while true {
        let line = source.read_line()
        if line is none {
            break
        }
        count += 1
        if count == 1 or count == 8 or count == 1000 {
            print(count, line)
        }
    }
    print("by read_line:", count)
}

function attempt_read(at: str) {
    try {
        let source = files.read_from(at)
        while true {
            let line = source.read_line()
            if line is none {
                print("the end")
                return
            }
        }
    } except FileError as e {
        print("FileError:", e.message.replace(path, "PATH"))
    }
}

function attempt_write(at: str, adding: bool) {
    try {
        if adding {
            let out = files.append_to(at)
        } else {
            let out = files.write_to(at)
        }
        print("opened")
    } except FileError as e {
        print("FileError:", e.message.replace(path, "PATH"))
    }
}

function bytes_in_pieces(at: str) raises FileError, ValueError {
    var all: list[u8] = []
    for b in range(256) {
        all.append(u8(b))
    }
    if true {
        let out = files.write_to(at)
        out.write_bytes(all)
        out.write_bytes([])
        out.write("tail")
    }
    let source = files.read_from(at)
    print("no bytes:", len(source.read_bytes(0)))
    var sizes: list[int] = []
    var sum = 0
    while true {
        let piece = source.read_bytes(100)
        sizes.append(len(piece))
        for b in piece {
            sum += int(b)
        }
        if len(piece) == 0 {
            break
        }
    }
    print("pieces:", sizes, sum, len(source.read_bytes(5)), source.read_line())
    try {
        print(len(source.read_bytes(-1)))
    } except ValueError as e {
        print("ValueError:", e.message)
    }
}

# The end is where the file ends at that moment: a read after more was
# written looks again (59.6).
function grow(at: str) raises FileError, ValueError {
    let out = files.write_to(at)
    let source = files.read_from(at)
    print("grow", source.read_line(), len(source.read_bytes(3)))
    out.write("one\n")
    print("grow", source.read_line(), source.read_line())
    out.write("tw")
    print("grow", source.read_bytes(5), len(source.read_bytes(1)))
    out.write("o\nthree")
    for piece in source {
        print("grow for", piece)
    }
    out.write("\nfour\n")
    print("grow", source.read_line() == "", source.read_line(), source.read_line())
    out.write_bytes([0x66, 0x69, 0x76, 0x65])
    for piece in source {
        print("grow for", piece)
    }
}

function main() raises FileError, ValueError {
    grow(path)
    write_lines(path)
    by_for(path)
    by_read_line(path)
    for round in range(3) {
        let again = files.read_from(path)
        print("round", round, again.read_line(), again.read_line())
    }
    files.remove(path)
    print("removed:", not files.exists(path))
    let shapes = ["a\n", "", "\n", "\n\n", "x\r\n\r\ny", "\r", "é😀தமிழ்\r"]
    for shape in shapes {
        files.write(path, shape)
        print(shape.lines() == read_all(path), len(read_all(path)))
    }
    bytes_in_pieces(path)
    files.write(path, "first\n")
    if true {
        let more = files.append_to(path)
        more.write("second\n")
        more.write("é😀தமிழ்\n")
    }
    print(read_all(path))
    if true {
        let fresh = files.write_to(path)
    }
    print("emptied:", len(files.read(path)))
    attempt_read(path + ".none")
    attempt_read(".")
    files.write_bytes(path, [0x41, 0x0A, 0xFF, 0x0A])
    attempt_read(path)
    files.write_bytes(path, [0x41, 0x00, 0x42])
    attempt_read(path)
    attempt_write(path + ".none/x.txt", false)
    attempt_write(".", true)
    files.remove(path)
}

main()
"#;

    #[test]
    fn a_file_in_pieces_is_the_same_in_every_build() {
        let path = scratch("pieces.txt");
        no_collector_as_interpreter("pieces", &IN_PIECES.replace("PATH", &path));
        let _ = std::fs::remove_file(&path);
        // Uncaught, it stops the program as `files.read` does.
        same_as_interpreter(
            "pieces-uncaught",
            &format!("import files\nlet source = files.read_from(\"{path}.none\")\nprint(source.read_line())\n"),
        );
    }

    /// Reading a thousand lines takes no more of the pool than reading
    /// ten: each line is built text, and the statement gives it back.
    #[test]
    fn reading_a_file_in_pieces_does_not_grow_the_pool() {
        let taken = |lines: usize| {
            let path = scratch(&format!("pool-{lines}.txt"));
            let source = format!(
                "import files\nfunction count(at: str) -> int raises FileError {{\n    let source = files.read_from(at)\n    \
                 var n = 0\n    for line in source {{\n        n += len(line)\n    }}\n    return n\n}}\n\
                 if true {{\n    let out = files.write_to(\"{path}\")\n    for i in range({lines}) {{\n        \
                 out.write(f\"line number {{i}} of the file\\n\")\n    }}\n}}\nprint(count(\"{path}\"))\nfiles.remove(\"{path}\")\n"
            );
            let c = compile_no_collector(&source).expect("it compiles");
            let (_, errors, finished) = build_and_run(&format!("pieces-pool-{lines}"), &c, &["-DZENO_MEMORY_REPORT"])?;
            assert!(finished, "{errors}");
            assert!(errors.contains("no collector, 0 blocks left at the end"), "{errors}");
            errors.lines().find(|line| line.starts_with("pool:")).map(str::to_string)
        };
        if let (Some(ten), Some(thousand)) = (taken(10), taken(1000)) {
            assert_eq!(ten, thousand, "the pool grew with the lines read");
        }
    }

    /// A line longer than the text room stops a build with no collector
    /// with the room's own message, as any text too long for it does.
    #[test]
    fn a_line_longer_than_the_text_room_stops_with_its_message() {
        let path = scratch("long-line.txt");
        std::fs::write(&path, "x".repeat(3000) + "\n").expect("written");
        let source = format!("import files\nlet source = files.read_from(\"{path}\")\nprint(source.read_line() is none)\n");
        let c = compile_no_collector(&source).expect("it compiles");
        let ran = build_and_run("pieces-long-line", &c, &[]);
        let _ = std::fs::remove_file(&path);
        let Some((_, errors, finished)) = ran else { return };
        assert!(!finished && errors.contains("more text than there is room for"), "{errors}");
    }

    /// A `write` hands its text to the system before it gives back, so a
    /// full disk is a FileError at that `write` (decision 59.6). Linux has
    /// a file that is always full.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_write_to_a_full_disk_is_an_error_at_that_write() {
        let source = "import files\nlet out = files.write_to(\"/dev/full\")\nfor piece in [\"a\", \"b\"] {\n    try {\n        \
                      out.write(piece)\n        print(\"written\", piece)\n    } except FileError as e {\n        print(e.message)\n    }\n}\n\
                      try {\n    out.write_bytes([1, 2])\n} except FileError as e {\n    print(e.message)\n}\n";
        assert_eq!(interpreted(source), "cannot write `/dev/full`\ncannot write `/dev/full`\ncannot write `/dev/full`\n");
        no_collector_as_interpreter("pieces-full", source);
    }

    /// A program that reads no file in pieces has none of that code in it.
    #[test]
    fn a_file_in_pieces_is_only_in_a_program_that_uses_one() {
        let without = compile("import files\nprint(files.read(\"x\"))\n").expect("it compiles");
        assert!(!without.contains("zeno_files_handle_open"), "piece code in a program without a reader");
        let with = compile("import files\nlet source = files.read_from(\"x\")\nprint(source.read_line())\n").expect("it compiles");
        assert!(with.contains("zeno_files_handle_open") && with.contains("zeno_file_handle_line"), "no piece code in a program with one");
    }

    /// `files.read_bytes` and `files.write_bytes` (decision 36.1). Each
    /// list has an owner, so that the build with no collector takes it.
    const BYTES: &str = r#"import files

function checksum(bytes: owned list[u8]) -> int {
    var sum = 0
    for b in bytes {
        sum = (sum + b) % 256
    }
    return sum
}

let path = "PATH"
let frame: owned list[u8] = [0x7E, 0x00, 0xFF, 0x0A, 0x00]
files.write_bytes(path, frame)
let back: owned list[u8] = files.read_bytes(path)
print(len(back), checksum(back), back[2] + 1)
print(back)
files.write(path, "தமிழ்")
let text: owned list[u8] = files.read_bytes(path)
print(len(text), text[0], text[len(text) - 1])
let hi: owned list[u8] = [72, 105, 10]
files.write_bytes(path, hi)
print(files.read(path))
let empty: owned list[u8] = []
files.write_bytes(path, empty)
let nothing: owned list[u8] = files.read_bytes(path)
print(len(nothing), files.read(path) == "")
files.remove(path)
try {
    let gone: owned list[u8] = files.read_bytes(path)
    print(len(gone))
} except FileError as error {
    print(error.message)
}
try {
    files.write_bytes("PATH/no/such/folder.bin", frame)
} except FileError as error {
    print(error.message)
}
"#;

    #[test]
    fn bytes_from_and_to_a_file_are_the_same_in_every_build() {
        let source = BYTES.replace("PATH", &scratch("bytes.bin"));
        no_collector_as_interpreter("bytes", &source);
        same_as_interpreter("bytes-collected", &source);
        // Uncaught, it stops the program as `files.read` does.
        let path = scratch("bytes-uncaught.bin");
        same_as_interpreter("bytes-uncaught", &format!("import files\nprint(len(files.read_bytes(\"{path}\")))\n"));
    }

    const PACKED: &str = r#"# Every way a list[u8] is made and used.
function checksum(bytes: list[u8]) -> int {
    var sum = 0
    for b in bytes {
        sum = (sum + b) % 256
    }
    return sum
}

function make(n: int) -> list[u8] {
    var out: list[u8] = []
    for i in range(n) {
        out.append(u8(i * 7 % 256))
    }
    return out
}

function first[T](xs: list[T]) -> T {
    return xs[0]
}

class Frame {
    kind: u8
    payload: list[u8]
}

let a: list[u8] = [1, 2, 3, 255]
print(a, len(a), a[0], a[3])
a[1] = 200
a.append(9)
print(a)
let b = make(10)
print(b, checksum(b))
let c = a + b
print(len(c), c[4], c[5])
print(c.slice(2, 5), a.slice(1))
print(a == [1, 200, 3, 255, 9], a != b, 3 in a, 4 in a)
print(a.find(255), a.find(77))
var d = [u8(n) for n in range(5)]
d.insert(0, 99)
print(d, d.pop(), d.pop(0), d)
d.reverse()
print(d)
d.sort()
print(d)
d.sort(function(x: u8) -> int { return 0 - x })
print(d)
let e = [u8(n * n % 256) for n in range(20) if n % 3 == 0]
print(e, sum(e), min(e), max(e))
let doubled = e.map(function(x: u8) -> int { return x * 2 })
print(doubled)
let small = e.filter(function(x: u8) -> bool { return x < 50 })
print(small)
let as_bytes = e.map(function(x: u8) -> u8 { return u8.wrap(x * 3) })
print(as_bytes)
print(first(a), first([u8(7)]))
let f = Frame(3, [0x7E, 0x10])
f.payload.append(0x20)
print(f.kind, f.payload, len(f.payload))
let frames = [f, Frame(4, [])]
print(frames[1].payload, len(frames))
let table = ["one": a, "two": b]
print(table["one"][0], len(table["two"]))
let nested: list[list[u8]] = [[1, 2], [3]]
nested[0].append(4)
print(nested)
var maybe: list[u8] or none = none
print(maybe)
maybe = [5, 6]
if maybe is not none {
    print(maybe[1])
}
var wrapped = [u8(250)]
wrapped[0] = u8.wrap(wrapped[0] + 10)
print(wrapped)
d.clear()
print(d, len(d))
var big: list[u8] = []
for i in range(1000) {
    big.append(u8(i % 256))
}
print(len(big), big[999], checksum(big))
print(str(a), f"{a}")
"#;

    const PACKED_OWNED: &str = r#"function make(n: int) -> owned list[u8] {
    var out: owned list[u8] = []
    for i in range(n) {
        out.append(u8(i * 7 % 256))
    }
    return out
}
function total(bytes: owned list[u8]) -> int {
    var sum = 0
    for b in bytes {
        sum = sum + b
    }
    return sum
}
let a: owned list[u8] = [1, 2, 3, 255]
a[1] = 200
a.append(9)
print(a, len(a), total(a))
let b = make(40)
print(b.slice(30), total(b))
let c: owned list[u8] = [u8(n) for n in range(10) if n % 2 == 0]
c.reverse()
print(c, c.pop(), c.find(4), 4 in c, sum(c), min(c), max(c))
let s = str(c)
print(s, f"{c}", c == [6, 4, 2])
let m: owned list[u8] = c.map(function(x: u8) -> u8 { return u8(x + 1) })
print(m)
"#;

    #[test]
    fn a_list_of_bytes_is_packed_and_does_what_any_list_does() {
        // One byte to an item: the list is made by the functions for
        // bytes, in C and in LLVM (decision 36.2).
        let c = compile(PACKED).expect("the program compiles");
        assert!(c.contains("zeno_bytes_of(") && c.contains("ZENO_BYTES("), "the list[u8] is not packed");
        same_as_interpreter("packed", PACKED);
        no_collector_as_interpreter("packed-owned", PACKED_OWNED);
        // A position outside the list stops the program, at the same
        // place, in every build.
        same_as_interpreter("packed-outside", "let a: list[u8] = [1, 2]\nprint(a[2])\n");
        same_as_interpreter("packed-pop", "var a: list[u8] = []\nprint(a.pop())\n");
    }

    #[test]
    fn a_list_of_a_fixed_length_is_the_same_in_every_build() {
        same_as_interpreter("fixed", include_str!("../tools/targets/edge-fixed.zeno"));
        no_collector_as_interpreter(
            "fixed-owned",
            "var buffer: owned list[u8, 8] = [0] * 8\nbuffer[7] = 255\n\
             function fill(data: owned list[u8], value: u8) {\n    for i in range(len(data)) {\n        data[i] = value\n    }\n}\n\
             fill(buffer, 2)\nprint(buffer, len(buffer))\nlet header: owned list[u8, 3] = [1, 2, 3]\nprint(header == [1, 2, 3])\n",
        );
        // A list of bytes and a list of ints make a list of ints.
        same_as_interpreter("bytes-and-ints", "let a: list[u8] = [1, 2]\nprint(a + [300], [300] + a)\n");
    }

    #[test]
    fn a_list_repeated_with_a_star_is_the_same_in_every_build() {
        same_as_interpreter("repeat", include_str!("../tools/targets/edge-repeat.zeno"));
        no_collector_as_interpreter(
            "repeat-owned",
            "let zeros: owned list[int] = [0] * 8\nprint(zeros)\nlet frame: owned list[u8] = [0x7E, 0] * 3\nprint(frame)\n\
             let words: owned list[str] = [\"ab\", \"c\"] * 2\nprint(words)\nlet again: owned list[int] = zeros * 2\nprint(len(again))\n",
        );
        same_as_interpreter("repeat-negative", "let n = -2\nprint([0] * n)\n");
    }

    #[test]
    fn an_f32_is_the_same_in_every_build() {
        same_as_interpreter("f32", include_str!("../tools/targets/edge-f32.zeno"));
        // Without `try`, so that the firmware build runs it too.
        same_as_interpreter(
            "f32-plain",
            "let a: f32 = 0.1\nvar total: f32 = 0.0\nfor i in range(10) {\n    total += a\n}\n\
             print(a, total, total == 1.0, a * 3, a / 3, a // 0.03, a % 0.03, a ** 2, -a, f32(16777217), f32(1e-45))\n\
             let xs: list[f32] = [0.5, -2.0, 1.25]\nprint(sum(xs), min(xs), max(xs), abs(xs[1]), float(a), int(total * 7))\n",
        );
        no_collector_as_interpreter(
            "f32-owned",
            "let xs: owned list[f32] = [0.5, 0.25, 0.1]\nvar ys: owned list[f32] = [x * 2.0 for x in xs]\nys.sort()\nprint(ys, sum(ys), ys == [0.2, 0.5, 1.0])\n",
        );
    }

    #[test]
    fn a_u64_is_the_same_in_every_build() {
        same_as_interpreter("u64", include_str!("../tools/targets/edge-u64.zeno"));
        // Without `try`, so that the firmware build runs it too.
        same_as_interpreter(
            "u64-plain",
            "let big: u64 = 18446744073709551615\n\
             let small: u64 = 5\n\
             var h: u64 = 0xcbf29ce484222325\n\
             for c in \"zeno\" {\n\
                 h = h ^ u64(c.code())\n\
                 h = u64.wrap(h * 0x100000001b3)\n\
             }\n\
             print(big, small, big - small, h, f\"{h:016x}\", big >> 60, small << 3, ~small, big // 7, big % 7, small ** 3)\n\
             print(u8.max, i8.min, u16.max, i32.min, u32.max, int.max, int.min, u64.max, u64.min, u64.wrap(-1))\n\
             let xs: list[u64] = [3, big, 1]\n\
             print(sum([small, small]), min(xs), max(xs), xs == [3, big, 1], small < big, float(big), int(small), abs(small))\n",
        );
        no_collector_as_interpreter(
            "u64-owned",
            "let xs: owned list[u64] = [9223372036854775808, 3, 18446744073709551615]\n\
             var ys: owned list[u64] = [x // 2 for x in xs]\n\
             ys.sort()\n\
             print(ys, sum([ys[0], ys[1]]), ys == [1, 4611686018427387904, 9223372036854775807])\n",
        );
    }

    #[test]
    fn units_made_of_units_are_the_same_in_every_build() {
        same_as_interpreter("units-made", include_str!("../tools/targets/edge-units-made.zeno"));
        // Without `try`, so that the firmware build runs it too.
        same_as_interpreter(
            "units-made-plain",
            "let power = 3.3 V * 20 mA\nprint(power, power.mW, 5 V / 2 mA, 2 mA * 4.7 kohm, (5 W * 2 h).Wh)\n\
             print(1 / 2 ms, 2.5 / 1 kHz, 10 ms * 1 kHz, 1 kWh / 1 h, 1 kWh / 5 W)\n",
        );
    }

    #[test]
    fn what_is_text_is_the_same_in_every_build() {
        // The interpreter asks Rust whether the bytes are UTF-8, and the
        // runtime has a test of its own. They must draw the same line.
        let cases: [(&str, &[u8], bool); 9] = [
            ("plain", b"plain", true),
            ("last", &[0xF4, 0x8F, 0xBF, 0xBF], true),      // U+10FFFF, the last character
            ("bom", &[0xEF, 0xBB, 0xBF, b'a'], true),        // a file that begins with a byte order mark
            ("stray", &[b'a', 0xFF], false),                 // a byte that is in no character
            ("zero", &[b'a', 0, b'b'], false),               // a zero byte
            ("cut", &[0xE2, 0x82], false),                   // a character cut short
            ("long", &[0xC0, 0x80], false),                  // a character written longer than it need be
            ("half", &[0xED, 0xA0, 0x80], false),            // half of a pair, which is no character
            ("beyond", &[0xF4, 0x90, 0x80, 0x80], false),    // past the last character
        ];
        let mut source = "import files\n".to_string();
        let mut expected = String::new();
        let mut paths = Vec::new();
        for (name, bytes, text) in cases {
            let path = scratch(&format!("text-{name}.bin"));
            std::fs::write(&path, bytes).expect("the file is written");
            source += &format!(
                "try {{\n    print(len(files.read(\"{path}\")))\n}} except FileError as error {{\n    print(error.message)\n}}\n"
            );
            expected += &match text {
                true => format!("{}\n", String::from_utf8_lossy(bytes).chars().count()),
                false => format!("`{path}` does not hold text\n"),
            };
            paths.push(path);
        }
        assert_eq!(interpreted(&source), expected);
        no_collector_as_interpreter("files-text", &source);
        for path in paths {
            std::fs::remove_file(path).expect("the file is removed");
        }
    }

    /// The keyboard and the command line.
    const OUTSIDE: &str = r#"import program

function shout(line: str or none) -> str {
    if line is none {
        return "(nothing)"
    }
    return line.upper()
}

let words = program.arguments()
print(len(words))
for word in words {
    print(f"<{word}>")
}
print(len(program.arguments()))

let name = input("Name: ")
if name is none {
    print("nobody")
} else {
    print(f"Hello, {name}!")
}
input()
print(shout(input()))
var count = 0
var longest = ""
while true {
    let line = input()
    if line is none {
        break
    }
    count += 1
    if len(line) > len(longest) {
        longest = line
    }
    for word in line.split() {
        print(word)
    }
    print(len(line))
}
print(count)
print(len(longest))
print(shout(input("more? ")))
"#;

    #[test]
    fn the_keyboard_and_the_arguments_are_the_same_in_every_build() {
        use crate::value::testing::with_outside;
        // A line longer than the runtime reads in one piece, a "\r" where
        // two pieces meet, "\r" inside a line and at the very end, an
        // empty line, and a zero byte.
        let long = "w".repeat(300);
        let edge = "x".repeat(125);
        let typed = format!("Partha\nskipped\nloud\none two\r\n{long}\n{edge}\r\n\na\rb\r\r\nnul\0byte\nதமிழ்\nend\r");
        with_outside(&typed, &["alpha", "two words", "--strict", "தமிழ்"], || {
            let printed = interpreted(OUTSIDE);
            assert!(printed.starts_with("4\n<alpha>\n<two words>\n<--strict>\n<தமிழ்>\n4\nName: Hello, Partha!\nLOUD\n"));
            // The "\r" at the very end stays: `end` and it are 4 characters.
            assert!(printed.ends_with("nulbyte\n7\nதமிழ்\n5\nend\n4\n8\n300\nmore? (nothing)\n"), "{printed}");
            no_collector_as_interpreter("outside", OUTSIDE);
        });
        // Nothing typed, and no arguments.
        assert_eq!(interpreted(OUTSIDE), "0\n0\nName: nobody\n(nothing)\n0\n0\nmore? (nothing)\n");
        no_collector_as_interpreter("outside-nothing", OUTSIDE);
    }

    #[test]
    fn the_logbook_example_is_the_same_in_every_build() {
        use crate::value::testing::with_outside;
        let source = include_str!("../examples/logbook.zeno");
        let path = scratch("logbook.txt");
        with_outside("temp 23.5\nhello\nhumidity 40\n\n", &[&path], || {
            let printed = interpreted(source);
            let expected = format!(
                "reading> reading> reading> reading> reading> \n4 lines typed\n\
                 4 readings in {path}, mean 26.75, lines skipped: 2\nstill there: false\n\
                 FileError: there is no file `{path}`\n"
            );
            assert_eq!(printed, expected);
            no_collector_as_interpreter("logbook", source);
        });
    }

    #[test]
    fn main_takes_the_command_line_only_when_the_program_reads_it() {
        let with = compile("import program\nprint(len(program.arguments()))").unwrap();
        assert!(with.contains("int main(int argc, char **argv) {"));
        assert!(with.contains("    zeno_program_start(argc, argv);\n"));
        let without = compile("print(1)").unwrap();
        assert!(without.contains("int main(void) {"));
        assert!(!without.contains("zeno_program_start(argc"));
    }

    /// A `var` that starts with a function of the program and is given
    /// others: by name, and one that keeps a number.
    pub(crate) const VAR_FUNCTION: &str = r##"function double(n: int) -> int { return n * 2 }
function triple(n: int) -> int { return n * 3 }
function offset_by(k: int) -> function(int) -> int {
    return function(n: int) -> int { return n + k }
}
function total(xs: list[int]) -> int {
    var sum = 0
    for x in xs { sum += x }
    return sum
}
function first(xs: list[int]) -> int { return xs[0] }

var f = double
print(f(2))
f = triple
print(f(2))
f = offset_by(100)
print(f(2))
f = double
print(f(5))

var pick = total
let data = [4, 5, 6]
print(pick(data))
pick = first
print(pick(data))

let fixed = double
let again = fixed
print(fixed(1), again(1))
"##;

    #[test]
    fn a_var_that_holds_a_function_can_be_given_another() {
        same_as_interpreter("var-function", VAR_FUNCTION);
        no_collector_as_interpreter("var-function", VAR_FUNCTION);
    }

    #[test]
    fn what_may_be_none_can_have_one_owner_in_every_build() {
        // With a collector, run before every allocation: the owner frees
        // the value itself, and the collector must not free it first.
        same_as_interpreter("owned-or-none", OWNED_OR_NONE);
        no_collector_as_interpreter("owned-or-none", OWNED_OR_NONE);
    }

    #[test]
    fn a_case_of_an_enum_matches_a_value_that_may_be_none() {
        // `case Stop` on a `Command or none` was taken for a new name
        // that matches anything, in the compiled program only.
        same_as_interpreter("case-or-none", r##"enum Command {
    Stop
    Wait(ms: int)
}

function pick(n: int) -> Command or none {
    if n == 0 { return none }
    if n == 1 { return Stop }
    return Wait(n)
}

for n in range(4) {
    match pick(n) {
        case none { print(n, "nothing") }
        case Stop { print(n, "stop") }
        case Wait(2) { print(n, "wait two") }
        case Wait(ms) { print(n, "wait", ms) }
        case _ { print(n, "something else") }
    }
    match pick(n) {
        case Wait(ms) { print("waits", ms) }
        case _ { print("does not wait") }
    }
}
"##);
    }

    #[test]
    fn with_no_collector_enum_values_do_not_fill_the_pool_over_time() {
        // A state that changes on every trip, and a value that may be
        // `none`. A hundred times the trips needs no more of the pool.
        let program = |trips: usize| r#"enum State {
    Idle
    Counting(n: int)
    Named(name: str, history: list[int])
}
function next(state: State, trip: int) -> State or none {
    match state {
        case Idle { return Counting(trip) }
        case Counting(n) {
            if n % 3 == 0 { return none }
            return Named(f"n{n}", [n, trip])
        }
        case Named(_, history) {
            if len(history) > 1 { return Idle }
            return none
        }
    }
}
var state = Idle
var kept = 0
for trip in range({trips}) {
    let found = next(state, trip)
    if found is not none {
        state = found
        kept += 1
    }
}
print(kept, state)
"#.replace("{trips}", &trips.to_string());
        let mut needed = Vec::new();
        for trips in [50, 5000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("states-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            crate::emit_llvm::tests::no_collector_as_c(&name, &source, &printed, &report);
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "a hundred times the trips needed a bigger pool");
        assert!(needed[0] < 1024, "{needed:?}");
    }

    #[test]
    fn the_c_for_an_enum_value_that_has_one_owner() {
        // docs/enums-and-none.md.
        let c = compile_no_collector(ENUM_C).unwrap();
        // A case that carries something is a block: only as big as that
        // case, with its own copy of the text it carries.
        assert!(
            c.contains(
                "static z_Shape *zeno_new_Shape_Named(const char *z_name, zeno_list *z_points) {
    z_Shape *self = (z_Shape *)zeno_alloc(offsetof(z_Shape, as) + sizeof(((z_Shape *)0)->as.c2));
    self->tag = 2;
    self->as.c2.z_name = zeno_text_keep(z_name);
    self->as.c2.z_points = z_points;
    return self;
}
"
            ),
            "{c}"
        );
        // Freeing one frees what it carries. A bare case is one fixed
        // value, which is never freed.
        assert!(
            c.contains(
                "static void zeno_drop_Shape(z_Shape *self) {
    if (self == NULL) return;
    switch (self->tag) {
    case 1: zeno_free(self); break;
    case 2: zeno_text_drop(self->as.c2.z_name); zeno_list_drop(self->as.c2.z_points); zeno_free(self); break;
    default: break;
    }
}
"
            ),
            "{c}"
        );
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // `match` looks: the text is copied out and freed when the case
        // is over, and the list stays in the value. A new value frees
        // the old one, and `none` is NULL, which frees as nothing.
        assert_eq!(
            program,
            "    z_shape = zeno_new_Shape_Named(\"a\", zeno_list_of(2, (zeno_slot){.i = 1}, (zeno_slot){.i = 2}));
    z_Shape *t1 = z_shape;
    if (t1->tag == 2) {
        const char *z_name = zeno_text_keep(t1->as.c2.z_name);
        zeno_list *z_points = t1->as.c2.z_points;
        size_t m1 = zeno_text_top;
        zeno_print(2, z_name, zeno_str_int(z_points->len));
        zeno_text_top = m1;
        zeno_text_drop(z_name);
    } else {
    }
    z_Shape *t2 = (&zeno_case_Shape_Dot);
    zeno_drop_Shape(z_shape);
    z_shape = t2;
    z_last = NULL;
    z_Shape *t3 = zeno_new_Shape_Circle(1.0);
    zeno_drop_Shape(z_last);
    z_last = t3;
    zeno_drop_Shape(z_last);
    zeno_drop_Shape(z_shape);"
        );
    }

    pub(crate) const ENUM_C: &str = "enum Shape {\n    Dot\n    Circle(r: float)\n    Named(name: str, points: list[int])\n}\n\
        var shape = Named(\"a\", [1, 2])\n\
        match shape {\n    case Named(name, points) { print(name, len(points)) }\n    case _ { }\n}\n\
        shape = Dot\n\
        var last: Shape or none = none\n\
        last = Circle(1.0)\n";

    /// Errors that are kept, handed on, returned and raised again.
    const KEPT_ERRORS: &str = r##"# Errors that are kept, given on and raised again.
class Overload {
    channel: int
    name: str
    amps: list[int]
}

class Log {
    last: Overload or none
    seen: list[Overload]
    faults: list[ValueError]
}

function measure(channel: int) -> int raises Overload {
    if channel % 3 == 0 {
        raise Overload(channel, f"ch{channel}", [channel, channel * 2])
    }
    return channel * 10
}

function parse(text: str) -> int raises ValueError {
    if text == "" {
        raise ValueError("nothing to read")
    }
    return int(text)
}

# Seen, noted, and raised again for the caller.
function checked(channel: int) -> int raises Overload {
    try {
        return measure(channel)
    } except Overload as overload {
        print("  passing on", overload.name)
        raise overload
    }
}

# The error is the answer.
function attempt(channel: int) -> Overload or none {
    try {
        print("  read", measure(channel))
    } except Overload as overload {
        return overload
    }
    return none
}

function describe(overload: Overload) -> str {
    return f"{overload.name} at {overload.amps}"
}

function why(fault: ValueError) -> str {
    return fault.message
}

let log = Log(none, [], [])
for channel in range(7) {
    try {
        print(channel, checked(channel))
    } except Overload as overload {
        print(channel, describe(overload))
        if channel == 0 {
            log.last = overload
        } else {
            log.seen.append(overload)
        }
    }
}
print(log)

for channel in [3, 4] {
    let found = attempt(channel)
    if found is none {
        print("fine")
    } else {
        print("kept", describe(found))
        log.last = found
    }
}
print(log.last)

for text in ["12", "", "x1", "7"] {
    try {
        print(parse(text))
    } except ValueError as fault {
        print("  ", why(fault))
        if text == "" { continue }
        log.faults.append(fault)
    }
}
print(len(log.faults), log.faults)

# An error made ahead of time, and raised later.
function fail_with(fault: take ValueError, really: bool) -> int raises ValueError {
    if really {
        raise fault
    }
    return 1
}
let ready = ValueError("made early")
print(ready.message, ready.kind)
try {
    print(fail_with(ValueError("not raised"), false))
    print(fail_with(ready, true))
} except Error as anything {
    print("caught", anything.kind, anything.message)
    var again: Error or none = none
    again = anything
    print(again)
}
"##;

    /// Function values: made, kept, lent, given away, replaced, and
    /// written where they look at a list or an object.
    const FUNCTION_VALUES: &str = r##"# Function values that have one owner.
class Counter {
    count: int
    name: str
}

class Menu {
    title: str
    actions: list[function(int) -> int]
    on_done: function(str) -> str

    function add(self, action: take function(int) -> int) {
        self.actions.append(action)
    }

    function run(self, start: int) -> int {
        var value = start
        for action in self.actions {
            value = action(value)
        }
        return value
    }

    # A function that looks at `self` is lent straight to `map`.
    function titles(self, names: list[str]) -> list[str] {
        return names.map(function(name: str) -> str { return f"{self.title}/{name}" })
    }
}

function double(n: int) -> int { return n * 2 }

function make_adder(n: int) -> function(int) -> int {
    return function(x: int) -> int { return x + n }
}

function make_tagger(tag: str) -> function(str) -> str {
    let mark = f"[{tag}]"
    return function(text: str) -> str { return f"{mark} {text}" }
}

function apply(f: function(int) -> int, x: int) -> int {
    return f(x)
}

function twice(f: function(int) -> int, x: int) -> int {
    return f(f(x))
}

function choose(big: bool) -> function(int) -> int {
    if big {
        return make_adder(100)
    }
    return double
}

let add2 = make_adder(2)
print(add2(3), apply(add2, 10), apply(double, 10))
print(twice(make_adder(5), 1), twice(double, 3))
print(add2, double)

# A variable can hold one function and then another. The old one goes.
var current: function(int) -> int = double
print(current(4))
current = make_adder(7)
print(current(4))
current = choose(true)
print(current(4))
current = choose(false)
print(current(4))

let tagger = make_tagger("warn")
print(tagger("low battery"))
print(tagger("door open"))

let menu = Menu("main", [], make_tagger("done"))
menu.add(double)
menu.add(make_adder(1))
menu.add(add2)
let step = 10
menu.add(function(n: int) -> int { return n * step })
print(menu.run(1), len(menu.actions))
print(menu.on_done("all"))
menu.on_done = make_tagger("again")
print(menu.on_done("all"))
let last = menu.actions.pop()
print(last(3), menu.run(1))
menu.actions[0] = make_adder(50)
print(menu.run(1))
print(menu.titles(["file", "edit"]))

# A function that looks at a list, an object and a loop's item.
let table = [10, 20, 30]
let counter = Counter(0, "calls")
let picks = [2, 0, 1].map(function(i: int) -> int {
    counter.count += 1
    return table[i]
})
print(picks, counter.count)
print(table.filter(function(n: int) -> bool { return n > table[0] }))
print(table.reduce(function(total: int, n: int) -> int { return total + n + counter.count }, 0))
for row in [[1, 2], [3, 4, 5]] {
    print(row.map(function(n: int) -> int { return n * len(row) + apply(function(m: int) -> int { return m + table[0] }, n) }))
}
print(apply(function(n: int) -> int { return n + len(counter.name) }, 1), counter)

# Early ways out, with functions in hand.
function first_big(limit: int) -> int {
    let bump = make_adder(limit)
    for n in range(10) {
        let scaled = make_adder(n)
        if scaled(n) > limit {
            return bump(n)
        }
        if n % 2 == 0 {
            continue
        }
        print("  tried", n)
    }
    return 0
}
print(first_big(5), first_big(100))
"##;

    /// Function types that say what each parameter does with a list or
    /// an object, and errors that leave a statement holding a function.
    const FUNCTION_TYPES: &str = r##"# Function types that mention lists and objects, and functions with errors about.
class Reading {
    channel: int
    values: list[int]
}

class TooHot {
    channel: int
    note: str
}

function total(xs: list[int]) -> int {
    var sum = 0
    for x in xs { sum += x }
    return sum
}

function drain(xs: list[int]) -> int {
    return len(xs)
}

function spread(n: int) -> list[int] {
    return [n, n + 1, n + 2]
}

function describe(reading: Reading) -> str {
    return f"ch{reading.channel}: {reading.values}"
}

function make(channel: int) -> Reading {
    return Reading(channel, spread(channel))
}

# Each of these takes a function whose type says what it does with a list.
function measure(with: function(list[int]) -> int, data: list[int]) -> int {
    return with(data)
}

function consume(with: function(list[int]) -> int, n: int) -> int {
    return with(spread(n)) + with([n])
}

function build(with: function(int) -> list[int], n: int) -> int {
    let made = with(n)
    return total(made) + len(with(n + 1))
}

function show(with: function(Reading) -> str, reading: Reading) -> str {
    return with(reading)
}

function produce(with: function(int) -> Reading, n: int) -> Reading {
    return with(n)
}

function check(level: int) -> int raises TooHot {
    if level > 30 {
        raise TooHot(level, f"level {level}")
    }
    return level
}

let data = [5, 6, 7]
print(measure(total, data), measure(function(xs: list[int]) -> int { return len(xs) }, data))
print(consume(drain, 4), consume(function(xs: list[int]) -> int { return total(xs) }, 4))
print(build(spread, 1), build(function(n: int) -> list[int] { return [n] }, 1))
let reading = make(3)
print(show(describe, reading), show(function(r: Reading) -> str { return f"{len(r.values)} values" }, reading))
let fresh = produce(make, 9)
let other = produce(function(n: int) -> Reading { return Reading(n, []) }, 8)
print(fresh, other)

# A variable and a field with such a type.
class Pipeline {
    name: str
    source: function(int) -> list[int]
    sink: function(list[int]) -> int

    function run(self, n: int) -> int {
        return self.sink(self.source(n))
    }
}
let offset = 100
let pipeline = Pipeline("p", function(n: int) -> list[int] { return [n + offset, n] }, drain)
print(pipeline.run(1), pipeline.run(2))
pipeline.sink = function(xs: list[int]) -> int { return total(xs) + offset }
print(pipeline.run(1))
var reducer: function(list[int]) -> int = total
print(reducer(data))
reducer = function(xs: list[int]) -> int { return xs[0] * offset }
print(reducer(data), reducer(spread(2)))

# An error leaves a statement that had a function in hand.
function apply(f: function(int) -> int, x: int) -> int {
    return f(x)
}
for level in [10, 40, 20] {
    try {
        let scale = level * 2
        let adder = function(n: int) -> int { return n + scale }
        print(apply(function(n: int) -> int { return n + len(data) }, check(level)), adder(1))
        print(apply(adder, check(level + 15)))
    } except TooHot as hot {
        print("too hot:", hot.note)
    }
}
class Task {
    id: int
}
function each(with: function(list[int]), n: int) {
    with([n, n])
    with([n])
}
function hand(with: function(Task), n: int) {
    with(Task(n))
}
let seen = [0]
each(function(xs: list[int]) { seen.append(len(xs)) }, 3)
hand(function(t: Task) { seen.append(t.id) }, 7)
hand(function(t: Task) {
    if t.id > 3 { return }
    seen.append(t.id)
}, 2)
print(seen)
var sink: function(Task) = function(t: Task) { print("got", t.id) }
sink(Task(1))
let kept: list[Task] = []
sink = function(t: Task) { print("again", t.id) }
sink(Task(2))
print(len(kept))
"##;

    /// Function types that mention an owned class, where a collector
    /// runs beside the owners.
    const OWNED_FUNCTION_TYPES: &str = r##"owned class Packet {
    id: int
    data: owned list[int]
}
function make(n: int) -> Packet { return Packet(n, [n, n + 1]) }
function send(p: Packet) -> int { return p.id + len(p.data) }
function look(p: Packet) -> int { return p.id }
function twice(with: function(Packet) -> int, p: Packet) -> int { return with(p) + with(p) }
function feed(with: function(Packet) -> int, n: int) -> int { return with(make(n)) }
function produce(with: function(int) -> Packet, n: int) -> int { return send(with(n)) }
var total = 0
for round in range(300) {
    let p = make(round)
    total += twice(look, p) + feed(send, round) + produce(make, round)
    total += twice(function(q: Packet) -> int { return len(q.data) }, p)
    total += feed(function(q: Packet) -> int { return q.id * 2 }, round)
}
print(total)
"##;

    #[test]
    fn with_no_collector_an_error_and_a_function_value_have_one_owner() {
        no_collector_as_interpreter("kept-errors", KEPT_ERRORS);
        no_collector_as_interpreter("function-values", FUNCTION_VALUES);
        no_collector_as_interpreter("function-types", FUNCTION_TYPES);
        no_collector_as_interpreter("handlers", include_str!("../examples/handlers.zeno"));
    }

    #[test]
    fn a_function_type_says_who_owns_what_in_every_build() {
        // With a collector the function values are shared, and the
        // packets they are handed still have one owner each.
        same_as_interpreter("owned-function-types", OWNED_FUNCTION_TYPES);
        crate::emit_llvm::tests::same_as_interpreter("owned-function-types", OWNED_FUNCTION_TYPES);
        let Some((printed, [_, _, collections])) = memory_of("owned-function-types-memory", OWNED_FUNCTION_TYPES) else {
            return;
        };
        assert_eq!(printed, interpreted(OWNED_FUNCTION_TYPES));
        assert_eq!(collections, 0, "the collector should have nothing to do");
    }

    #[test]
    fn with_no_collector_errors_and_function_values_do_not_fill_the_pool_over_time() {
        // A function value made and replaced on every trip, one that
        // keeps text, one that looks at a list, and an error that is
        // caught, raised again and kept. A hundred times the trips needs
        // no more of the pool.
        let program = |trips: usize| r#"class Fault {
    code: int
    note: str
}
function risky(n: int) -> int raises Fault {
    if n % 3 == 0 { raise Fault(n % 100, f"fault {n % 10}") }
    return n
}
function make_adder(n: int) -> function(int) -> int {
    return function(x: int) -> int { return x + n }
}
function tagger(tag: str) -> function(int) -> str {
    return function(n: int) -> str { return f"{tag}{n % 10}" }
}
function retry(n: int) -> int raises Fault {
    try {
        return risky(n)
    } except Fault as fault {
        if n % 2 == 0 { raise fault }
        return 0
    }
}
var last: Fault or none = none
var step: function(int) -> int = make_adder(0)
var total = 0
let kept: list[function(int) -> str] = []
for trip in range({trips}) {
    step = make_adder(trip % 5)
    let name = tagger(f"t{trip % 7}")
    try {
        total += step(retry(trip)) + len(name(trip))
    } except Fault as fault {
        last = fault
    }
    total += len([trip, total].map(function(n: int) -> int { return n + step(1) }))
    if len(kept) < 3 { kept.append(name) }
}
print(total, last, len(kept), kept[0](3))
"#.replace("{trips}", &trips.to_string());
        let mut needed = Vec::new();
        for trips in [50, 5000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("handlers-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            crate::emit_llvm::tests::no_collector_as_c(&name, &source, &printed, &report);
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "a hundred times the trips needed a bigger pool");
        assert!(needed[0] < 1024, "{needed:?}");
    }

    #[test]
    fn the_c_for_a_function_value_that_has_one_owner() {
        // docs/functions-and-errors.md.
        let c = compile_no_collector(FUNCTION_C).unwrap();
        // A function of the program is one fixed value. Its `drop` is
        // NULL, so freeing it does nothing.
        assert!(c.contains("static zeno_closure zeno_value_double = {(zeno_code)zeno_wrap_double, \"double\"};\n"), "{c}");
        // One written where a value goes is a block, with a copy of the
        // text it uses and a function that frees both.
        assert!(
            c.contains(
                "static void zeno_fn_drop_2(zeno_closure *zeno_self) {
    struct zeno_env_2 *env = (struct zeno_env_2 *)zeno_self;
    zeno_text_drop(env->z_tag);
    zeno_free(env);
}

static zeno_closure *z_make_tagger(const char *z_tag, int64_t z_width) {
    struct zeno_env_2 *t5 = (struct zeno_env_2 *)zeno_alloc(sizeof(struct zeno_env_2));
    t5->base.code = (zeno_code)zeno_fn_2;
    t5->base.name = \"\";
    t5->base.drop = zeno_fn_drop_2;
    t5->z_tag = zeno_text_keep(z_tag);
    t5->z_width = z_width;
    return (&t5->base);
}
"
            ),
            "{c}"
        );
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // A new value frees the old one, whichever kind each is.
        assert_eq!(
            program,
            "    z_tagger = z_make_tagger(\"warn\", 4);
    zeno_closure *t1 = z_make_tagger(\"note\", 2);
    zeno_closure_drop(z_tagger);
    z_tagger = t1;
    z_current = (&zeno_value_double);
    zeno_closure *t2 = (zeno_closure *)zeno_alloc(sizeof(zeno_closure));
    t2->code = (zeno_code)zeno_fn_1;
    t2->name = \"\";
    t2->drop = zeno_closure_release;
    zeno_closure *t3 = t2;
    zeno_closure_drop(z_current);
    z_current = t3;
    zeno_closure_drop(z_current);
    zeno_closure_drop(z_tagger);"
        );
        // Where there is a collector, a function value has no `drop`.
        let shared = compile(FUNCTION_C).unwrap();
        assert!(!shared.contains("zeno_closure_drop") && !shared.contains("->drop"), "{shared}");
    }

    pub(crate) const FUNCTION_C: &str = "function make_tagger(tag: str, width: int) -> function(str) -> str {\n    \
        return function(text: str) -> str { return f\"[{tag}:{width}] {text}\" }\n}\n\
        function double(n: int) -> int { return n * 2 }\n\
        var tagger = make_tagger(\"warn\", 4)\n\
        tagger = make_tagger(\"note\", 2)\n\
        var current: function(int) -> int = double\n\
        current = function(n: int) -> int { return n + 1 }\n";

    #[test]
    fn with_no_collector_lists_of_objects_do_not_fill_the_pool_over_time() {
        // A list of objects, and a list of lists, made and dropped on
        // every trip. A hundred times the trips needs no more of the pool.
        let program = |trips: usize| r#"class Sample {
    name: str
    values: list[int]
}
function batch(n: int) -> list[Sample] {
    return [Sample(f"s{i}", [i, n]) for i in range(4)]
}
var total = 0
for n in range({trips}) {
    var samples = batch(n)
    samples.append(Sample("extra", [n]))
    samples[0] = Sample("first", [])
    let gone = samples.pop()
    let rows = [[n, n], [n]]
    for s in samples {
        total += len(s.values) + len(rows[0])
    }
    total += len(gone.name)
}
print(total)
"#.replace("{trips}", &trips.to_string());
        let mut needed = Vec::new();
        for trips in [50, 5000] {
            let source = program(trips);
            let c = compile_no_collector(&source).unwrap();
            let name = format!("items-{trips}-trips");
            let Some((printed, report, finished)) = build_and_run(&name, &c, &["-DZENO_MEMORY_REPORT"]) else { return };
            assert!(finished, "{report}");
            assert_eq!(printed, interpreted(&source));
            assert!(report.contains("no collector, 0 blocks left at the end\n"), "{report}");
            crate::emit_llvm::tests::no_collector_as_c(&name, &source, &printed, &report);
            needed.push(pool_needed(&report));
        }
        assert_eq!(needed[0], needed[1], "a hundred times the trips needed a bigger pool");
        assert!(needed[0] < 2048, "{needed:?}");
    }

    #[test]
    fn the_c_for_a_list_that_owns_its_items() {
        let c = compile_no_collector(
            "class Point {\n    x: int\n}\n\
             let points = [Point(1), Point(2)]\n\
             points[0] = Point(3)\n\
             for p in points {\n    print(p.x, len(points))\n}\n\
             let grid = [[points.pop()]]\n",
        )
        .unwrap();
        // One function for each kind of list that frees its items.
        assert!(
            c.contains(
                "static void zeno_list_drop_1_Point(zeno_list *list) {\n    if (list == NULL) return;\n    \
                 for (int64_t i = 0; i < list->len; i++) zeno_drop_Point(((z_Point *)list->items[i].p));\n    \
                 zeno_list_drop(list);\n}\n"
            ),
            "{c}"
        );
        // A list of lists of Points frees each item with the one above.
        assert!(
            c.contains("for (int64_t i = 0; i < list->len; i++) zeno_list_drop_1_Point(((zeno_list *)list->items[i].p));"),
            "{c}"
        );
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        // A new item frees the one whose place it takes.
        assert!(program.contains("zeno_drop_Point(((z_Point *)(*t"), "{program}");
        // The copy a loop walks is freed alone: its items are the list's.
        let after_loop = program.split("print").nth(1).expect("a loop");
        assert!(after_loop.contains("    }\n    zeno_list_drop(t"), "{program}");
        // The end of the program frees each list with its items.
        assert!(program.contains("zeno_list_drop_2_Point(z_grid);\n    zeno_list_drop_1_Point(z_points);"), "{program}");
        // A body that cannot change the list (it does not name it, and
        // calls nothing of the program's) walks it where it is.
        let c = compile_no_collector("class Point {\n    x: int\n}\nlet points = [Point(1), Point(2)]\nfor p in points {\n    p.x += 1\n    print(p.x)\n}\n").unwrap();
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        assert!(!program.contains("zeno_list_copy") && program.contains("zeno_list *t2 = z_points;"), "{program}");
    }

    #[test]
    fn an_assignment_can_reach_through_a_list() {
        // `grid[1][0] = 5` and `points[1].x = 30`: the value first, then
        // the list or the object, then the index, as `zeno run` does.
        same_as_interpreter("reach", r##"class Point {
    x: int
    y: int
}

function say(what: str, n: int) -> int {
    print(what, n)
    return n
}

var grid = [[1, 2], [3, 4]]
grid[1][0] = 5
grid[say("row", 0)][say("column", 1)] = say("value", 9)
print(grid)
let points = [Point(1, 2), Point(3, 4)]
points[1].x = 30
points[say("point", 0)].y = say("y", 7)
print(points)
grid[0][1] += 4
points[1].x += 1
print(grid, points)
try {
    grid[say("row", 5)][say("column", 0)] = say("value", 1)
} except IndexError as e {
    print(e)
}
try {
    points[say("point", 2)].x = say("x", 1)
} except IndexError as e {
    print(e)
}
"##);
    }

    #[test]
    fn the_owned_example_builds_with_no_collector() {
        no_collector_as_interpreter("buffer", include_str!("../examples/buffer.zeno"));
        // Simple code stays simple here too: nothing is noted or freed
        // for a program that makes no memory.
        let c = compile_no_collector("var total = 0\nfor i in range(10) {\n    total += i\n}\nlet done = total > 40").unwrap();
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        assert!(!program.contains("zeno_text_top") && !program.contains("drop"), "{program}");
    }

    #[test]
    fn what_a_build_with_no_collector_refuses_says_so() {
        // The sweep of 9.9. What a build with no collector refuses and
        // the others take is the table of docs/no-collector.md, all by
        // design; what every build refuses is the table of
        // docs/compiler.md. Each row here is a row of one of them, with
        // a program and the words of its refusal through C and through
        // LLVM. A row of either page with nothing here, or a row here
        // with no row on its page, fails, and so does a refusal in other
        // words or a program that builds.
        let tail = ". `zeno build --no-collector` leaves the collector out, so nothing would free it. \
                    docs/no-collector.md lists what such a build accepts";
        let inner_value = format!(
            "`add` is a function inside a function that uses variables of the function around it. With no collector \
             it can be called, and not used as a value: the block owns it. Write it as `let add = function(...) {{ ... }}`, \
             which has one owner{tail}"
        );
        let inner_kept = format!(
            "`add` is a function inside a function that uses variables of the function around it, and a function \
             written where a value goes may outlive the block that owns it. Give it what it needs as parameters, or \
             call `add` outside it{tail}"
        );
        // (the words of the row on its page, the program, the refusal)
        let no_collector: [(&str, &str, &str); 11] = [
            (
                "Keeping an item of a list of lists or of objects",
                "let grid = [[1], [2]]\nlet row = grid[0]\nprint(row)\n",
                "this item is an owned list[int], which has one owner: the list. `[ ]` only looks at it. Use it where it \
                 is, pass it to a parameter that looks, or take it out with `pop()`",
            ),
            (
                "A function that is kept and uses a `var`",
                "var xs = [1, 2]\nlet f = function(n: int) -> int { return n + len(xs) }\nprint(f(1))\n",
                "this function is kept, so it takes `xs`, but `xs` is a `var`: the function around it could give it a \
                 new value after this one took it. Declare it with `let`",
            ),
            (
                "used after it was given away",
                "var a = [1]\nlet b = a\nprint(a, b)\n",
                "`a` was given away on line 2 and holds nothing now. Use it before it goes, or give it only to what \
                 looks: a parameter without `take`",
            ),
            (
                "uses a `var` of the function around it",
                "function f() -> int {\n    var n = 1\n    let g = function() -> int { return n }\n    return g()\n}\nprint(f())\n",
                "`n` is a `var` of the function around this one, so the two functions would share it, and with no \
                 collector nothing is shared. If it does not change, declare it with `let`. If it does, keep it in an \
                 object, which a function written here can look at and change",
            ),
            (
                "Replacing a list or an object that a parameter only looks at",
                "class Box {\n    items: list[int]\n}\nfunction refill(b: Box) {\n    b.items = [7, 8]\n}\nlet b = Box([1])\nrefill(b)\nprint(b.items)\n",
                "only the owner can replace what `items` holds: somebody else may have been lent it. Here `b` only looks at it",
            ),
            (
                "Taking an item out of a list while a `for` walks it",
                "var xs = [[1], [2]]\nfor x in xs {\n    print(xs.pop())\n}\n",
                "the `for` on line 2 is walking a list that `xs` owns, and is still looking at its items. Nothing can \
                 take an item out of this list until the loop is over",
            ),
            (
                "used as a value or called from",
                "function maker(step: int) -> function(int) -> int {\n    function add(x: int) -> int {\n        return x + step\n    }\n    return add\n}\nprint(maker(1)(2))\n",
                &inner_value,
            ),
            (
                "used as a value or called from",
                "function g(step: int) -> int {\n    function add(x: int) -> int {\n        return x + step\n    }\n\
                 let h = function(y: int) -> int { return add(y) }\n    return h(1)\n}\nprint(g(1))\n",
                &inner_kept,
            ),
            (
                "Returning or keeping what a parameter only looks at",
                "function same(row: list[int]) -> list[int] {\n    return row\n}\nprint(same([1, 2]))\n",
                "`row` is only looked at here: its owner still has it. So it cannot be kept, returned or given away. \
                 If it is a parameter, write `take` in front of its type to take the value over",
            ),
            (
                "`filter` on a list of lists or of objects",
                "let rows = [[1], [2, 3]]\nlet long = rows.filter(function(row: list[int]) -> bool { return len(row) > 1 })\nprint(long)\n",
                "`filter` would put the items it keeps in a second list, and each has one owner. Use a `for` loop \
                 that makes what the new list needs",
            ),
            (
                "that a `for` or a list comprehension walks",
                "let rows = [[1], [2, 3]]\nlet copy = [row for row in rows]\nprint(copy)\n",
                "`row` is a look at an item of the list that the loop is walking. The list owns the item, so it \
                 cannot be kept, returned or given away. Use it here, or pass it to a parameter that looks",
            ),
        ];
        let yet = " yet. `zeno run` can still run this program";
        let every_build: Vec<(&str, &str, String)> = vec![
            (
                "A class or an enum inside a function",
                "function f() -> int {\n    class P {\n        x: int\n    }\n    return P(1).x\n}\nprint(f())\n",
                format!("`zeno build` cannot compile a class inside a function or block{yet}"),
            ),
            (
                "A class or an enum inside a function",
                "function f() -> int {\n    enum E {\n        A\n        B\n    }\n    let e = E.A\n    return 1\n}\nprint(f())\n",
                format!("`zeno build` cannot compile an enum inside a function or block{yet}"),
            ),
            (
                "A generic function inside a function",
                "function outer() -> int {\n    function id[T](x: T) -> T {\n        return x\n    }\n    return id(4)\n}\nprint(outer())\n",
                format!("`zeno build` cannot compile a generic function inside a function or block{yet}"),
            ),
            (
                "Two functions inside a function that call each other",
                "function m(n: int, stop: int) -> bool {\n    function even(k: int) -> bool {\n        if k == stop {\n            return true\n        }\n        return odd(k - 1)\n    }\n\
                 function odd(k: int) -> bool {\n        if k == stop {\n            return false\n        }\n        return even(k - 1)\n    }\n    return even(n)\n}\nprint(m(4, 0))\n",
                "`zeno build` cannot compile two functions inside a function that call each other and use variables of \
                 the function around them, such as `even`, yet. Give one of them to the other as a parameter. `zeno run` \
                 can still run this program"
                    .to_string(),
            ),
            (
                "A default that is more than a literal, on a function inside a function",
                "function g(n: int) -> int {\n    function add(x: int, y: int = x + 1) -> int {\n        return x + y + n\n    }\n    return add(1)\n}\nprint(g(1))\n",
                format!("`zeno build` cannot compile a default that is not a plain literal, for a function inside a function that uses variables around it,{yet}"),
            ),
            (
                "Named arguments to a function held in a variable",
                "let f = function(a: int) -> int { return a }\nprint(f(a = 1))\n",
                format!("`zeno build` cannot compile named arguments to a function held in a variable{yet}"),
            ),
            (
                "A built-in function used as a value",
                "print([1, 2].map(str))\n",
                format!("`zeno build` cannot compile `str` used this way{yet}"),
            ),
            (
                "A method that comes with Zeno used as a value",
                "var xs = [1]\nlet f = xs.append\nf(2)\nprint(xs)\n",
                format!("`zeno build` cannot compile a method that comes with Zeno used as a value, `.append`,{yet}"),
            ),
            (
                "A method of an object that no variable holds",
                "class Box {\n    function size(self) -> int { return 1 }\n}\nlet f = Box().size\nprint(f())\n",
                "`zeno build` cannot compile a method used as a value of an object that no variable holds, `.size`, yet. \
                 Put the object in a `let` first and use `.size` of that: then it builds. `zeno run` can still run this program"
                    .to_string(),
            ),
            (
                "A list whose items are not all one type",
                "print([1, \"a\"])\n",
                format!("`zeno build` cannot compile a list whose items are not all one type{yet}"),
            ),
            (
                "A variable that only ever holds `none`",
                "var x = none\nprint(x)\n",
                "`zeno build` needs a type for a variable that only ever holds `none`. Give it one, as in `x: int or none`".to_string(),
            ),
            (
                "A variable that only ever holds `none`",
                "print([])\n",
                "`zeno build` needs to know what a list holds, and nothing says it here: `[]` and `[none]` alone could \
                 hold anything. Give it a type first, as in `let items: list[int] = []`"
                    .to_string(),
            ),
            (
                "`program.run`",
                "import program\nif false {\n    print(program.run(\"true\", []))\n}\n",
                format!("`zeno build` cannot compile `program.run`{yet}"),
            ),
            (
                "A u64 that may be `none`",
                "let big: u64 or none = u64(5)\nprint(big)\n",
                format!("`zeno build` cannot compile a u64 that may be `none`{yet}"),
            ),
            (
                "A dict of objects of an `owned class`, in a build with a collector",
                "owned class Frame {\n    id: int\n}\nvar frames: dict[str, Frame] = [:]\nframes[\"a\"] = Frame(1)\nprint(frames[\"a\"].id)\n",
                "`zeno build` keeps a dict where anything may share it, as there is a collector, and an object of an \
                 `owned class` has one owner. Make it a plain `class`, or build with `--no-collector`, where a dict \
                 owns its values"
                    .to_string(),
            ),
        ];
        let through_llvm = |message: &str| message.replacen("`zeno build`", "`zeno build --llvm`", 1);
        for (_, source, message) in &no_collector {
            // `zeno run` and a build with a collector take it.
            let program = crate::modules::testing::program(source);
            assert!(crate::vm::run_both(&program, source).1.is_ok(), "`zeno run` refused\n{source}");
            assert!(compile(source).is_ok() && crate::emit_llvm::tests::compile(source).is_ok(), "a build with a collector refused\n{source}");
            assert_eq!(compile_no_collector(source).unwrap_err(), *message, "\n{source}");
            assert_eq!(crate::emit_llvm::tests::compile_no_collector(source).unwrap_err(), *message, "\n{source}");
        }
        for (key, source, message) in &every_build {
            let program = crate::modules::testing::program(source);
            assert!(crate::vm::run_both(&program, source).1.is_ok(), "`zeno run` refused\n{source}");
            assert_eq!(compile(source).unwrap_err(), *message, "\n{source}");
            assert_eq!(crate::emit_llvm::tests::compile(source).unwrap_err(), through_llvm(message), "\n{source}");
            // One row is refused only where there is a collector.
            let refused_without = !key.ends_with("in a build with a collector");
            assert_eq!(compile_no_collector(source).is_err(), refused_without, "with no collector\n{source}");
            assert_eq!(crate::emit_llvm::tests::compile_no_collector(source).is_err(), refused_without, "with no collector\n{source}");
        }
        // Each page's table and these rows, one to one.
        let rows = |page: &str, after: &str| -> Vec<String> {
            let table = page.split(after).nth(1).expect("the section").split("\n\n").find(|part| part.starts_with('|')).expect("a table");
            table.lines().skip(2).map(str::to_string).collect()
        };
        for (page, after, keys) in [
            (
                include_str!("../docs/no-collector.md"),
                "## What is refused\n",
                no_collector.iter().map(|(key, _, _)| *key).collect::<Vec<_>>(),
            ),
            (
                include_str!("../docs/compiler.md"),
                "## What it compiles so far\n",
                every_build.iter().map(|(key, _, _)| *key).collect::<Vec<_>>(),
            ),
        ] {
            let rows = rows(page, after);
            for row in &rows {
                assert!(keys.iter().any(|key| row.contains(key)), "a row with no program to hold it to: {row}");
            }
            for key in &keys {
                assert!(rows.iter().any(|row| row.contains(key)), "no row on the page for `{key}`");
            }
        }
        // What tools/refused.py found, by the row each refusal belongs
        // to: the row must be on its page, and held to a program here.
        for line in include_str!("../tools/refused.txt").lines() {
            let [page, row, message] = line.splitn(3, " | ").collect::<Vec<_>>()[..] else {
                panic!("a line of tools/refused.txt that is not `page | row | message`: {line}");
            };
            let (text, keys) = match page {
                "docs/no-collector.md" => (rows(include_str!("../docs/no-collector.md"), "## What is refused\n"), no_collector.iter().map(|(key, _, _)| *key).collect::<Vec<_>>()),
                "docs/compiler.md" => (rows(include_str!("../docs/compiler.md"), "## What it compiles so far\n"), every_build.iter().map(|(key, _, _)| *key).collect()),
                _ => panic!("tools/refused.txt names a page with no table: {line}"),
            };
            let found = text.iter().find(|line| line.starts_with(&format!("| {row}")));
            let found = found.unwrap_or_else(|| panic!("no row of {page} begins `{row}`, for: {message}"));
            assert!(keys.iter().any(|key| found.contains(key)), "no program here holds the row of: {message}");
        }
    }

    #[test]
    fn in_and_a_range_as_a_value_are_the_same_in_every_build() {
        // What the sweep of item 9 missed: `in` on a list whose items may
        // be `none`, which C refused and LLVM took, and `range(...)`
        // anywhere but after `for`, which both refused.
        let source = r#"let xs: list[int or none] = [1, none, 3]
print(3 in xs, none in xs, 2 in xs)
let ys: list[str or none] = ["a", none]
print("a" in ys, none in ys, "b" in ys)
let zs: list[float or none] = [1.5, none]
print(1.5 in zs, none in zs)
let bs: list[bool or none] = [true, none]
print(false in bs, none in bs)
function total(xs: list[int]) -> int {
    var sum = 0
    for x in xs {
        sum += x
    }
    return sum
}
function upto(n: int) -> list[int] {
    return range(n)
}
var r = range(3)
r.append(9)
print(3 in range(5), 7 in range(5), 4 in range(0, 10, 2), 3 in range(0, 10, 2), -1 in range(5, 0, -1), 9 in r)
print(len(range(2, 9, 3)), range(10, 0, -4), range(0), range(-9223372036854775807, 9223372036854775807, 9223372036854775807))
print(r, total(range(5)), upto(4), range(3) + range(2), [x * 2 for x in range(4)])
print(range(4).map(function(x: int) -> int { return x * x }), range(3) == [0, 1, 2])
"#;
        no_collector_as_interpreter("in-and-range", source);
        crate::emit_llvm::tests::same_as_interpreter("in-and-range", source);
        // A step of 0 is the ValueError of `zeno run`, in each compiler.
        let source = "print(range(1, 5, 0))\n";
        let called = [
            (compile(source), "zeno_range_list(1, 5, 0, "),
            (crate::emit_llvm::tests::compile(source), "@zeno_range_list(i64 1, i64 5, i64 0, "),
        ];
        for (built, call) in called {
            assert!(built.as_ref().is_ok_and(|code| code.contains(call)), "no `{call}` in {built:?}");
        }
    }

    #[test]
    fn a_default_made_for_a_parameter_that_looks_is_the_statements() {
        // With no collector, `xs: list[int] = [1, 2]` on a parameter that
        // only looks was refused with advice to name the list with `let`,
        // which a default cannot do. The list or object is made at each
        // call that leaves the argument out, and the statement of the call
        // owns it, as it owns one written in the call. Each call frees
        // it: 0 blocks are left after a hundred.
        let source = r#"owned class Tag {
    name: str
}
class Box {
    n: int
    function size(self, extra: list[int] = [self.n, 1]) -> int {
        return self.n + len(extra)
    }
}
let kept = Box(7)
function show(x: int, xs: list[int] = [x, x + 1], words: list[str] = [f"w{x}", "b"], b: Box = Box(x), d: dict[str, int] = ["k": x]) -> int {
    return len(xs) + xs[1] + len(words[0]) + b.n + d["k"]
}
function named(t: Tag = Tag("x")) -> str {
    return t.name
}
function shared(b: Box = kept) -> int {
    return b.n
}
function boxes(ps: list[Box] = [Box(1), Box(2)]) -> int {
    return len(ps) + ps[1].n
}
var total = 0
for i in range(100) {
    total += show(i) + len(named()) + shared() + boxes()
}
print(total, show(1, [7, 8]), Box(3).size(), Box(4).size([1, 2, 3]), named(Tag("y")), kept.n)
"#;
        no_collector_as_interpreter("looking-default", source);
        crate::emit_llvm::tests::same_as_interpreter("looking-default", source);
    }

    #[test]
    fn what_tools_refused_py_found_builds_in_every_build() {
        // The first run of tools/refused.py found these refused by a
        // build that `zeno run` ran (decision 56.19). A default that holds
        // lists, or bytes, was worked out from a copy of itself, which the
        // checker had never seen; `[3] in rows` with no collector; two
        // functions of one type in a list; and `[none]` or `[]` beside
        // lists that say what they hold, or on the left of `in`.
        let source = r#"class Holder {
    v: list[list[int]] = [[1], [2]]
    bytes: list[u8] = [u8(1), u8(2)]
}
function nested(x: list[list[int]] = [[1], [2]], b: list[u8] = [u8(3)]) {
    print(x, b)
}
nested()
nested([[3]])
let h = Holder()
print(h.v, h.bytes)
let rows = [[1, 2], [3]]
print([3] in rows, [4] in rows, [1, 2] not in rows)
function twice(n: int) -> int {
    return n * 2
}
let a = function(n: int) -> int { return n + 1 }
let fs = [a, twice, function(n: int) -> int { return n - 1 }]
for f in fs {
    print(f(3))
}
print(len([twice, twice]), [twice, twice].map(function(f: function(int) -> int) -> int { return f(1) }))
let words = [["a", none], [none]]
print(words, len(words[1]))
for w in [["a", none], [none]] {
    print(w)
}
let grid = [[1, none], [none], []]
print(grid, [none] in grid, [none, none] not in grid, [] in grid, [none] in words)
"#;
        no_collector_as_interpreter("refused-py", source);
        crate::emit_llvm::tests::same_as_interpreter("refused-py", source);
    }

    #[test]
    fn a_program_that_uses_no_piece_gets_the_runtime_of_0_7_0() {
        // Decision 56.19: what is added to the runtime goes in a piece of
        // `PIECES`, written only for a program that uses it. With none,
        // the runtime is the one 0.7.0 wrote, to the byte. These are the
        // length and the FNV-1a hash of the runtime beside the IR that
        // `zeno build --llvm` of 0.7.0 (17453fd) wrote for `print(1)`,
        // which is the runtime of C with `static` taken off: with a
        // collector, `--firmware`, `--no-collector`, and `--owned`. A line
        // slipped in outside a piece fails here at once.
        let fnv = |text: &str| text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3));
        for (build, grows, length, hash) in [
            (Build::Ordinary, false, 174_624, 0xf115_5511_d95b_2718_u64),
            (Build::Firmware, false, 172_099, 0x6b81_dc12_3104_db69),
            (Build::NoCollector, false, 159_740, 0x86f2_432b_57a5_8313),
            (Build::NoCollector, true, 165_448, 0xb711_e5da_d632_3ad6),
        ] {
            let runtime = with_board_parts(&with_pieces(&crate::emit_llvm::runtime_for(build, grows), &[]), &[]);
            assert_eq!((runtime.len(), fnv(&runtime)), (length, hash), "{build:?}, growing: {grows}");
        }
        // And each piece is there for a program that uses it, through C
        // and through LLVM, and not for one that does not.
        for (name, function) in PIECES {
            let all = RUNTIME.lines().filter(|line| line.trim() == format!("#ifdef {name}")).count();
            assert_eq!(all, 1, "`#ifdef {name}` is in runtime.c once");
            let with = with_pieces(&runtime_for(false, true, false), &[name]);
            assert!(with.contains(&format!("{function}(")) && !with.contains(name), "{name}");
        }
        let source = "print(3 in range(5))\n";
        assert!(compile(source).unwrap().contains("static zeno_list *zeno_range_list("));
        assert!(!compile("print(3 in [0, 1])\n").unwrap().contains("zeno_range_list"));
    }

    #[test]
    fn functions_inside_functions_every_way() {
        // Functions with names written inside functions, and defaults
        // that are more than a literal (decision 56.16): every build
        // prints what `zeno run` prints, and with no collector nothing is
        // left.
        let source = include_str!("../tools/targets/edge-inner-functions.zeno");
        no_collector_as_interpreter("inner-functions", source);
        crate::emit_llvm::tests::same_as_interpreter("inner-functions-llvm", source);
        // One that changes a `var` around it, and one that is given away
        // as a value: with a collector, which shares and keeps them.
        let shared = "function counting() -> int {\n    var total = 0\n    function bump(by: int) {\n        total += by\n    }\n\
                      bump(2)\n    total += 10\n    bump(1)\n    return total\n}\n\
                      function maker(step: int) -> function(int) -> int {\n    function add(x: int) -> int {\n        return x + step\n    }\n    return add\n}\n\
                      let made = maker(5)\nprint(counting(), made(1))\n";
        same_as_interpreter("inner-shared", shared);
        crate::emit_llvm::tests::same_as_interpreter("inner-shared-llvm", shared);
        // One that uses nothing around it is a function of the file, and
        // can be called above its line. One that does is a closure, made
        // once, after the last thing it uses is declared.
        let placed = "function g(n: int) -> int {\n    let a = sq(2)\n    function sq(x: int) -> int {\n        return x * x\n    }\n\
                      let k = n * 2\n    function add(x: int) -> int {\n        return x + k\n    }\n    return a + add(1)\n}\nprint(g(5))\n";
        let c = compile(placed).unwrap();
        assert!(c.contains("static int64_t zeno_inner_1_sq(int64_t z_x) {"), "{c}");
        let body = c.split("static int64_t z_g(int64_t z_n) {\n").nth(1).expect("the function");
        assert!(body.starts_with("    int64_t z_a = zeno_inner_1_sq(2);\n    int64_t z_k = zeno_mul(z_n, 2, "), "{body}");
        assert!(body.contains("    t1->z_k = z_k;\n    zeno_closure *z_add = (&t1->base);\n"), "{body}");
        let ir = crate::emit_llvm::tests::compile(placed).unwrap();
        assert!(ir.contains("define internal i64 @zeno.inner.1.sq(i64 %arg.x) {") && ir.contains("call i64 @zeno.inner.1.sq(i64 2)"), "{ir}");
        // What is refused, and why, the same through C and through LLVM.
        let cycle = "function m(n: int, stop: int) -> bool {\n    function even(k: int) -> bool {\n        if k == stop {\n            return true\n        }\n        return odd(k - 1)\n    }\n\
                     function odd(k: int) -> bool {\n        if k == stop {\n            return false\n        }\n        return even(k - 1)\n    }\n    return even(n)\n}\nprint(m(4, 0))\n";
        let generic = "function outer() -> int {\n    function id[T](x: T) -> T {\n        return x\n    }\n    return id(4)\n}\nprint(outer())\n";
        let named_default = "function g(n: int) -> int {\n    function add(x: int, y: int = x + 1) -> int {\n        return x + y + n\n    }\n    return add(1)\n}\nprint(g(1))\n";
        for (build, refused) in [
            ("`zeno build`", compile as fn(&str) -> Result<String, String>),
            ("`zeno build --llvm`", crate::emit_llvm::tests::compile),
        ] {
            assert_eq!(
                refused(cycle).unwrap_err(),
                format!(
                    "{build} cannot compile two functions inside a function that call each other and use variables of \
                     the function around them, such as `even`, yet. Give one of them to the other as a parameter. \
                     `zeno run` can still run this program"
                )
            );
            assert_eq!(
                refused(generic).unwrap_err(),
                format!("{build} cannot compile a generic function inside a function or block yet. `zeno run` can still run this program")
            );
            assert_eq!(
                refused(named_default).unwrap_err(),
                format!(
                    "{build} cannot compile a default that is not a plain literal, for a function inside a function \
                     that uses variables around it, yet. `zeno run` can still run this program"
                )
            );
        }
        // With no collector the block owns such a closure: it can be
        // called, and not given away or kept by a function written where
        // a value goes, which may outlive the block.
        let given = "function maker(step: int) -> function(int) -> int {\n    function add(x: int) -> int {\n        return x + step\n    }\n    return add\n}\nprint(maker(1)(2))\n";
        let kept = "function g(step: int) -> int {\n    function add(x: int) -> int {\n        return x + step\n    }\n\
                    let h = function(y: int) -> int { return add(y) }\n    return h(1)\n}\nprint(g(1))\n";
        let rest = ". `zeno build --no-collector` leaves the collector out, so nothing would free it. \
                    docs/no-collector.md lists what such a build accepts";
        for refused in [compile_no_collector as fn(&str) -> Result<String, String>, crate::emit_llvm::tests::compile_no_collector] {
            assert_eq!(
                refused(given).unwrap_err(),
                format!(
                    "`add` is a function inside a function that uses variables of the function around it. With no \
                     collector it can be called, and not used as a value: the block owns it. Write it as `let add = \
                     function(...) {{ ... }}`, which has one owner{rest}"
                )
            );
            assert_eq!(
                refused(kept).unwrap_err(),
                format!(
                    "`add` is a function inside a function that uses variables of the function around it, and a \
                     function written where a value goes may outlive the block that owns it. Give it what it needs as \
                     parameters, or call `add` outside it{rest}"
                )
            );
        }
    }

    // ----- The bits of whole numbers -----

    #[test]
    fn bit_operators_match() {
        same_as_interpreter(
            "bits",
            "let flags = 0b1010_0101\nlet mask = 0x0F\n\
             print(flags & mask, flags | mask, flags ^ mask, ~flags, ~0, ~-1)\n\
             print(1 << 5, 1 << 62, 0xFF >> 4, -8 >> 1, -1 >> 63, -7 >> 1, 5 >> 0, 3 << 0, -1 << 63, -2 << 62)\n\
             print(flags & mask == 5, 1 << 2 + 1, 6 | 1 ^ 3 & 2, 1 | 2 == 3, -1 & 0xFF, ~5 & 0xFF)\n\
             print(f\"{flags & ~mask:08b} {1 << 12:x}\")\n\
             var reg = 0\nreg |= 1 << 3\nreg |= 0x81\nreg &= ~0x01\nreg ^= 0xFF\nreg <<= 4\nreg >>= 2\nprint(reg)\n\
             function bit(n: int, at: int) -> bool { return (n >> at) & 1 == 1 }\n\
             print(bit(5, 0), bit(5, 1), bit(5, 2))\n",
        );
    }

    #[test]
    fn a_shift_too_far_is_an_error() {
        let shift = "function shift(n: int, by: int) -> int { return n << by }\n\
                     function back(n: int, by: int) -> int { return n >> by }\n";
        same_as_interpreter("shift-64", &format!("{shift}print(shift(1, 63), back(-1, 63))\nprint(shift(1, 64))\n"));
        same_as_interpreter("shift-minus", &format!("{shift}print(back(8, -1))\n"));
        same_as_interpreter("shift-big", &format!("{shift}print(shift(3, 61))\nprint(shift(3, 62))\n"));
        same_as_interpreter("shift-small", &format!("{shift}print(shift(-3, 61))\nprint(shift(-3, 62))\n"));
    }

    // ----- Sized whole numbers -----

    const SIZED: &str = r#"class Packet {
    kind: u8
    length: u16
    offset: i8 = -1
    total: i32 = 0
    big: u32 = 4294967295
    name: str = "p"
}
function checksum(data: list[u8]) -> u8 {
    var total = 0
    for byte in data { total += byte }
    return u8.wrap(total)
}
function double(n: int) -> int { return n * 2 }
function small(n: int) -> i8 { return i8(n) }
function level(by: u8 = 7) -> u8 { return by }
let a: u8 = 200
let b: u8 = 100
print(a + b, a - b, b - a, a * b, a // b, a % b, -a, ~a, a << 8, a >> 3, a & b, a | b, a ^ b, a ** 2)
print(a == 200, a != b, a < b, a >= b, a == b + 100, a in [100, 200], f"{a} {a:x} {a:08b}", str(a), abs(small(-5)), min(a, b), max(a, 1000), float(a), int(a))
print(u8.wrap(a + b), i8.wrap(a), i16.wrap(0xFFFF), u16.wrap(-1), u32.wrap(-1), i32.wrap(0x80000000), u8.wrap(256))
print(u8(255), u16(65535), u32(4294967295), i8(-128), i16(-32768), i32(-2147483648), small(127), level(), level(9), level(by=b))
print(double(a), double(small(-3)))
let bytes: list[u8] = [1, 2, 0xFF]
bytes.append(7)
bytes.append(a)
bytes[0] = 16
print(bytes, sum(bytes), max(bytes), min(bytes), len(bytes), checksum(bytes), bytes[1] << 4 | bytes[0], a in bytes, 3 in bytes)
let doubled = [double(n) for n in bytes]
let wide: list[int] = [int(n) for n in bytes]
print(doubled, wide, wide == doubled)
var maybe: u8 or none = none
print(maybe)
maybe = 5
if maybe is not none { print(maybe + 1) }
let p = Packet(kind=3, length=1024)
let q = Packet(255, 65535, -128, -2147483648, 0)
print(p, q, p == q, p == Packet(3, 1024), q.big, ~q.big, -p.big, p.big < -1, q.offset < q.length, p.offset + p.big)
p.kind = u8(p.kind + 250)
p.length = u16.wrap(-2)
p.offset = 127
p.total = i32(p.offset * -1000)
q.big = u32.wrap(-1)
print(p, q, p.kind + q.kind, p.length >> 8)
var count: u8 = 254
for i in range(3) {
    count = u8.wrap(count + 1)
    print(count)
}
match a {
    case 200 { print("two hundred") }
    case _ { print("other") }
}
"#;

    #[test]
    fn sized_ints_match() {
        same_as_interpreter("sized", SIZED);
        same_as_interpreter("packet", include_str!("../examples/packet.zeno"));
    }

    /// `count += 1` on a sized number is `count = u8(count + 1)`, with
    /// every operator, in a variable, a field and an item of a list.
    const SIZED_COMPOUND: &str = r#"
class Counter {
    count: u8
    total: u16

    function bump(self, by: u8) {
        self.count += by
        self.total += self.count
    }
}

function drain(level: u8) -> u8 {
    var left: u8 = level
    while left > 3 {
        left -= 3
    }
    return left
}

var count: u8 = 250
count += 1
count += 2
print(count)
count -= 3
count *= 1
count //= 2
count %= 100
count **= 1
print(count)
var flags: u8 = 0b0000_0001
flags |= 0x80
flags <<= 0
flags >>= 0
flags &= 0xF0
flags ^= 0x0F
print(flags)
var wide: i16 = -5
wide -= 10
wide *= 2
print(wide)

let counter = Counter(0, 0)
counter.bump(200)
counter.bump(50)
counter.count -= 1
print(counter.count, counter.total, drain(20))

let frame: list[u8] = [1, 2, 3]
var i = 1
frame[i] += 40
frame[i + 1] *= 2
print(frame)
"#;

    #[test]
    fn compound_assignment_on_sized_numbers_matches() {
        same_as_interpreter("sized-compound", SIZED_COMPOUND);
        // A sum that does not fit is an error that can be caught, and
        // the variable keeps what it had.
        same_as_interpreter(
            "sized-compound-caught",
            "var count: u8 = 250\ntry {\n    count += 10\n    print(\"not reached\")\n} except OverflowError as error {\n    \
             print(error.message, count)\n}\ntry {\n    count = 3\n    count -= 4\n} except OverflowError as error {\n    \
             print(error.message, count)\n}",
        );
        // Not caught, it stops every build at the same statement.
        same_as_interpreter("sized-compound-over", "var count: u8 = 250\ncount += 5\nprint(count)\ncount += 1\nprint(\"not reached\")");
        // A variable that a function written inside another shares.
        same_as_interpreter(
            "sized-compound-shared",
            "function step() -> function() -> u8 {\n    var seen: u8 = 0\n    return function() -> u8 {\n        seen += 1\n        return seen\n    }\n}\n\
             let next = step()\nnext()\nprint(next())",
        );
    }

    #[test]
    fn with_no_collector_a_sized_number_takes_an_operator_and_an_equals() {
        no_collector_as_interpreter("sized-compound", SIZED_COMPOUND);
    }

    #[test]
    fn what_is_there_is_read_before_the_value_is_worked_out() {
        // `count += bump()` is `count = count + bump()`, in every build.
        same_as_interpreter(
            "read-first",
            "var count = 1\nfunction bump() -> int {\n    count = 10\n    return 1\n}\ncount += bump()\nprint(count)\n\
             class Box {\n    n: int\n}\nlet box = Box(1)\nfunction swap() -> int {\n    box.n = 10\n    return 1\n}\nbox.n += swap()\nprint(box.n)\n\
             let xs = [1, 2]\nfunction grow() -> int {\n    xs[0] = 10\n    return 1\n}\nxs[0] += grow()\nprint(xs)\n\
             var n = 17\nn //= 2\nn %= 5\nn **= 4\nprint(n)",
        );
    }

    #[test]
    fn a_quantity_takes_the_formats_of_a_float() {
        no_collector_as_interpreter(
            "quantity-format",
            "import math\n\nlet angle = 30.456 deg\nlet supply = 3.3 V\nlet wait = 500 ms\nprint(f\"{angle:.1f}\")\n\
             print(f\"{supply:.2f} and {supply}\")\n\
             print(f\"[{supply:10.2f}] [{supply:<10.2f}] [{supply:^10.2f}] [{supply:010.2f}]\")\n\
             print(f\"[{wait:10}] [{wait:<10}] [{wait:.0f}] [{wait:f}] [{wait:.3}]\")\n\
             print(f\"{math.atan(1):.4f} {(angle * 2).rad:.3f} {-supply:8.1f}|\")\n\
             for reading in [3.3 V, 12.0 V, 0.75 V] {\n    print(f\"{reading:8.2f}\")\n}",
        );
    }

    #[test]
    fn a_number_too_big_for_its_size_is_an_error() {
        let small = "function small(n: int) -> i8 { return i8(n) }\nfunction byte(n: int) -> u8 { return u8(n) }\n";
        same_as_interpreter("sized-fits", &format!("{small}print(small(-128), small(127), byte(0), byte(255))\nprint(byte(256))\n"));
        same_as_interpreter("sized-below", &format!("{small}print(small(-129))\n"));
        same_as_interpreter("sized-minus", &format!("{small}print(byte(-1))\n"));
        same_as_interpreter(
            "sized-caught",
            &format!("{small}try {{ print(byte(300)) }} except OverflowError as e {{ print(e.kind, e.message) }}\nprint(small(5))\n"),
        );
        same_as_interpreter("sized-field", "class Reg {\n    value: u8\n}\nlet r = Reg(value=200)\nr.value = u8(r.value + 100)\n");
    }

    #[test]
    fn a_sized_field_is_small_in_the_struct() {
        let source = "class Packet {\n    kind: u8\n    length: u16\n    offset: i8\n    total: i32\n    big: u32\n    plain: int\n}\n\
                      let p = Packet(3, 1024, -1, 0, 0, 0)\np.kind = 9\nprint(p.kind + p.offset)\n";
        let c = compile(source).unwrap();
        assert!(
            c.contains(
                "struct z_Packet {\n    uint8_t z_kind;\n    uint16_t z_length;\n    int8_t z_offset;\n    \
                 int32_t z_total;\n    uint32_t z_big;\n    int64_t z_plain;\n};"
            ),
            "{c}"
        );
        // Small going in, an ordinary int coming out.
        assert!(c.contains("    self->z_kind = (uint8_t)z_kind;\n"), "{c}");
        assert!(c.contains("    z_p->z_kind = (uint8_t)9;\n"), "{c}");
        assert!(c.contains("zeno_add(((int64_t)z_p->z_kind), ((int64_t)z_p->z_offset), 11, 7)"), "{c}");
        // A sized number outside an object is an int64_t like any other.
        let c = compile("function next(n: u8) -> u8 { return u8.wrap(n + 1) }\nlet a: u8 = next(255)\n").unwrap();
        assert!(c.contains("static int64_t z_next(int64_t z_n) {\n    return zeno_wrap(zeno_add(z_n, 1, 1, 45), 8, false);\n}"), "{c}");
    }

    // ----- Generics -----

    /// Generic functions and classes, used with ints, floats, strs, a
    /// u8, an enum, lists and objects, with errors, defaults, functions
    /// as arguments, and one generic calling another.
    const GENERIC: &str = r#"function first[T](items: list[T]) -> T {
    return items[0]
}

function largest[T](items: list[T]) -> T {
    var best = items[0]
    for item in items {
        if item > best {
            best = item
        }
    }
    return best
}

function pair[A, B](a: A, b: B) -> str {
    return f"({a}, {b})"
}

function apply[T, U](value: T, f: function(T) -> U) -> U {
    return f(value)
}

class Stack[T] {
    items: list[T]

    function push(self, item: T) {
        self.items.append(item)
    }

    function pop(self) -> T {
        return self.items.pop()
    }

    function size(self) -> int {
        return len(self.items)
    }
}

class Pair[A, B] {
    left: A
    right: B
}

function swap[A, B](p: Pair[A, B]) -> Pair[B, A] {
    return Pair(p.right, p.left)
}

print(first([1, 2, 3]), first(["a", "b"]), first([1.5]))
print(largest([3, 9, 2]), largest(["pear", "apple"]), largest([2.5, 1.5]))
print(pair(1, "one"), pair(true, 2.5))
print(apply(5, function(n: int) -> int { return n * 2 }), apply("hi", function(s) { return s.upper() }))

let numbers: Stack[int] = Stack(items=[])
numbers.push(1)
numbers.push(2)
let words = Stack(items=["a"])
words.push("b")
print(numbers.pop(), numbers.size(), words.pop(), words, numbers)
let p = Pair(1, "x")
let q = swap(p)
print(p, q, q.left, q.right + 1)
enum Colour {
    Red
    Green
}

class Cell[T] {
    value: T
    count: u8 = 0

    function set(self, value: T) {
        self.value = value
        self.count = u8.wrap(self.count + 1)
    }
}

function reversed[T](items: list[T]) -> list[T] {
    var out: list[T] = []
    var at = len(items) - 1
    while at >= 0 {
        out.append(items[at])
        at -= 1
    }
    return out
}

function find[T](items: list[T], wanted: T) -> int or none {
    var at = 0
    for item in items {
        if item == wanted {
            return at
        }
        at += 1
    }
    return none
}

function first_or[T](items: list[T], otherwise: T) -> T {
    if len(items) == 0 {
        return otherwise
    }
    return items[0]
}

function checked[T](items: list[T], at: int) -> T raises ValueError {
    if at < 0 or at >= len(items) {
        raise ValueError(f"no item {at}")
    }
    return items[at]
}

function map[T, U](items: list[T], f: function(T) -> U) -> list[U] {
    var out: list[U] = []
    for item in items {
        out.append(f(item))
    }
    return out
}

function keep[T](items: list[T], want: function(T) -> bool) -> list[T] {
    return [item for item in items if want(item)]
}

function make[T](value: T, times: int = 2) -> list[T] {
    var out: list[T] = []
    for i in range(times) {
        out.append(value)
    }
    return out
}

let small: u8 = 200
let cell = Cell(small)
cell.set(7)
let named = Cell("a", 3)
named.set("b")
print(cell, named, cell.value + 1, cell == Cell(small), cell == cell)
print(reversed([1, 2, 3]), reversed(["x", "y"]), reversed([Colour.Red, Colour.Green]))
print(find([10, 20, 30], 20), find(["a"], "z"), find([2.5], 2.5))
let nothing: list[str] = []
print(first_or(nothing, "none there"), first_or([4], 0))
try {
    print(checked([1, 2], 1))
    print(checked(["a"], 5))
} except ValueError as e {
    print("caught", e)
}
print(map([1, 2, 3], function(n: int) -> str { return f"<{n}>" }), map(["a", "bb"], function(s: str) -> int { return len(s) }))
print(keep([1, 2, 3, 4], function(n: int) -> bool { return n % 2 == 0 }))
print(make("z"), make(1.5, times=3), make(Cell(1), 1))
let cells: list[Cell[int]] = []
cells.append(Cell(5))
print(cells, len(cells))
let maybe: Cell[str] or none = none
print(maybe)

# A function written inside a generic is copied with it: `both` takes
# an int in one copy and a str in the other.
function doubled[T](items: list[T], twice: function(T) -> T) -> list[T] {
    let both = function(item: T) -> T {
        return twice(twice(item))
    }
    return items.map(both)
}
print(doubled([1, 2], function(n: int) -> int { return n * 2 }))
print(doubled(["a"], function(s: str) -> str { return s + s }))
"#;

    /// An owned class that is generic: each copy frees its own objects.
    const GENERIC_OWNED: &str = r#"owned class Box[T] {
    value: T
    function get(self) -> T {
        return self.value
    }
}
function show[T](box: Box[T]) -> T {
    return box.get()
}
function open[T](box: Box[T]) -> T {
    return box.value
}
function boxed[T](value: T) -> Box[T] {
    return Box(value)
}
let a = Box(5)
let b = Box(2.5)
print(a.get() + 1, show(b), a.value)
let c = boxed("in")
print(show(c))
print(open(c))
print(open(b))
let d: Box[int] = Box(9)
print(d.value)
"#;

    #[test]
    fn generics_match() {
        same_as_interpreter("generic", GENERIC);
        no_collector_as_interpreter("generic-owned", GENERIC_OWNED);
    }

    #[test]
    fn a_generic_is_compiled_once_for_each_set_of_types() {
        let source = "function first[T](items: list[T]) -> T {\n    return items[0]\n}\n\
                      class Cell[T] {\n    value: T\n    function get(self) -> T {\n        return self.value\n    }\n}\n\
                      function unused[T](x: T) -> T {\n    return x\n}\n\
                      let small: u8 = 7\n\
                      print(first([1, 2]), first([\"a\"]), first([3]))\n\
                      print(Cell(1.5).get(), Cell(small), Cell([1]))\n";
        let c = compile(source).unwrap();
        // Two copies of `first`, though it is called three times: an
        // ordinary C function each, with the type filled in.
        assert!(c.contains("static int64_t z_first_int(zeno_list *z_items) {\n"), "{c}");
        assert!(c.contains("static const char *z_first_str(zeno_list *z_items) {\n"), "{c}");
        assert_eq!(c.matches("z_first_int(zeno_list *z_items) {").count(), 1, "{c}");
        assert!(c.contains(" = z_first_int(zeno_list_of(2, (zeno_slot){.i = 1}, (zeno_slot){.i = 2}));"), "{c}");
        // Three structs for `Cell`, each as big as what it holds. A
        // `Cell[u8]` is one byte.
        assert!(c.contains("struct z_Cell_float {\n    double z_value;\n};"), "{c}");
        assert!(c.contains("struct z_Cell_u8 {\n    uint8_t z_value;\n};"), "{c}");
        assert!(c.contains("struct z_Cell_list_int {\n    zeno_list *z_value;\n};"), "{c}");
        // (The field is read once, into a local: decision 22.8.)
        assert!(c.contains("static double z_Cell_float_get(z_Cell_float *z_self) {\n    double t10 = z_self->z_value;\n    return t10;\n}"), "{c}");
        // It still prints as a `Cell`.
        assert!(c.contains("zeno_join(3, \"Cell(value=\", "), "{c}");
        // Nothing is written for the generic itself, or for one that
        // nothing calls.
        assert!(!c.contains("z_first(") && !c.contains("z_Cell ") && !c.contains("z_unused"), "{c}");
    }

    #[test]
    fn what_a_generic_cannot_be_compiled_to_says_so() {
        let refused = |source: &str| compile(source).err().unwrap_or_else(|| "it compiled".to_string());
        assert_eq!(
            refused("class Stack[T] {\n    items: list[T]\n}\nclass Stack_int {\n    n: int\n}\nprint(Stack([1]), Stack_int(2))\n"),
            "`Stack_int` and another class or enum of this program would both be called `z_Stack_int` in C. \
             A copy of a generic class gets such a name: `Stack[int]` becomes `z_Stack_int`. So does a class \
             of an imported file: `Point` of the module `shapes` becomes `z_shapes_Point`. \
             Rename one of them, or build with `--llvm`"
        );
        assert_eq!(
            refused("function outer() -> int {\n    function id[T](x: T) -> T {\n        return x\n    }\n    return id(4)\n}\nprint(outer())\n"),
            "`zeno build` cannot compile a generic function inside a function or block yet. `zeno run` can still run this program"
        );
    }

    // ----- The board and the clock -----

    const PINS: &str = "import board\nimport time\nfrom board import led, pin\nfrom time import sleep, now\n\
        function flash(light: Pin, times: int) {\n    for i in range(times) {\n        light.toggle()\n        sleep(1 ms)\n    }\n}\n\
        class Lamp {\n    light: Pin\n    name: str\n}\n\
        let start = now()\nled.on()\nled.on()\n\
        print(led, led.is_on(), led.number, led == pin(13), led != pin(2))\n\
        flash(led, 3)\n\
        let relay = board.pin(7)\nrelay.on()\nrelay.off()\nrelay.off()\n\
        let all = [led, relay, pin(0)]\nfor p in all { p.on() }\n\
        print(all, relay in all, pin(5) in all, f\"{relay} is {relay.is_on()}\")\n\
        let lamp = Lamp(board.led, \"front\")\nprint(lamp, lamp.light.is_on())\n\
        var spare: Pin or none = none\nprint(spare)\nspare = pin(63)\n\
        if spare is not none { spare.toggle() }\nprint(spare, spare == pin(63))\n\
        time.sleep(0.001 s)\nsleep(0 ms)\nsleep(-5 s)\n\
        print(now() >= start, time.now() - start < 10 s)\n";

    #[test]
    fn pins_and_the_clock_match() {
        same_as_interpreter("pins", PINS);
    }

    #[test]
    fn a_pin_the_board_does_not_have_is_an_error() {
        same_as_interpreter(
            "no-pin",
            "from board import pin\nfunction pick(n: int) -> Pin { return pin(n) }\n\
             try {\n    pick(64).on()\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
             pick(2).on()\npick(-1).on()\n",
        );
    }

    /// The same program on a board of its own: the four functions that
    /// stand between a Zeno program and the hardware, written again in a
    /// header. This is how a real board is put underneath.
    #[test]
    fn a_board_of_its_own_can_be_put_underneath() {
        let source = "from board import led, pin\nfrom time import sleep, now\n\
                      let before = now()\nled.toggle()\nsleep(250 ms)\nled.toggle()\npin(1).on()\n\
                      print(pin(1).is_on(), now() - before)\nprint(pin(2))\n";
        // A board with two pins that writes down every call, and a clock
        // that moves only when the program sleeps. Its LED is on pin 0.
        let header = "#define ZENO_PINS 2\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static double my_clock = 0.0;\n\
                      static int my_pin(int pin) { return pin == ZENO_LED ? 0 : pin; }\n\
                      static bool zeno_board_read(int pin) { return my_levels[my_pin(pin)]; }\n\
                      static void zeno_board_write(int pin, bool level) {\n\
                      \x20   my_levels[my_pin(pin)] = level;\n\
                      \x20   printf(\"write %d %d\\n\", my_pin(pin), (int)level);\n\
                      }\n\
                      static void zeno_board_sleep(double seconds) { my_clock += seconds * 1000.0; }\n\
                      static double zeno_board_now(void) { return my_clock; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-board", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("board-header", &c, &[&flag]) else {
                return;
            };
            // The LED is pin 13 to the program, and pin 0 to this board.
            assert_eq!(printed, "write 0 1\nwrite 0 0\nwrite 1 1\ntrue 250 ms\n");
            assert!(!finished);
            assert_eq!(errors, "error: uncaught ValueError: this board has no pin 2. Its pins are 0 to 1\n --> test.zeno:9:7\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the program reads (decision 45).
    const INPUTS: &str = "from board import led, pin, input_pin, pull_up, pull_down\n\
        import board\n\
        let start = input_pin(2, pull_up)\nlet stop = input_pin(3, board.pull_down)\nlet door = board.input_pin(4)\n\
        print(start, stop, door)\n\
        print(start.is_high(), start.is_low(), stop.is_high(), door.is_low())\n\
        print(start.number, pull_up, pull_down == board.pull_down, f\"{door}\")\n\
        function wired(n: int, how: Pull) -> InputPin {\n    return input_pin(n, how)\n}\n\
        let inputs: list[InputPin] = [start, wired(5, pull_up), door]\n\
        for p in inputs {\n    print(p.number, p.is_high())\n}\n\
        print(start in inputs, stop in inputs, start == door, inputs[0] == start)\n\
        var spare: InputPin or none = none\nprint(spare)\nspare = wired(6, pull_down)\n\
        if spare is not none { print(spare.is_low()) }\n\
        led.on()\nlet back = input_pin(13)\nprint(back.is_high())\n\
        pin(4).on()\nprint(door.is_high())\nlet held = input_pin(4, pull_down)\nprint(held.is_high(), door.is_high())\n";

    #[test]
    fn input_pins_match() {
        same_as_interpreter("inputs", INPUTS);
        no_collector_as_interpreter("inputs", INPUTS);
        same_as_interpreter(
            "no-input-pin",
            "from board import input_pin, pull_up\n\
             try {\n    input_pin(64, pull_up)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
             input_pin(-1)\n",
        );
    }

    /// A program that makes no input pin never calls the fifth function
    /// of the board layer, so a board header written before there were
    /// inputs builds it as before (decision 45.4).
    #[test]
    fn only_a_program_with_inputs_asks_the_board_for_them() {
        for c in [compile(PINS).unwrap(), compile_firmware(PINS).unwrap(), compile_no_collector(PINS).unwrap()] {
            // The comment at the top of the board layer names it; no C does.
            assert!(!c.contains("static void zeno_board_input(") && !c.contains("zeno_board_input((int)"), "a program with no input pins asks the board for one");
            // Nor for a serial port (decision 46.5).
            assert!(
                !c.contains("static void zeno_board_serial_open(") && !c.contains("zeno_board_serial_open((int)"),
                "a program with no serial port asks the board for one"
            );
            // Nor for its converter (decision 47.3).
            assert!(
                !c.contains("static bool zeno_board_analog(") && !c.contains("zeno_board_analog_read((int)"),
                "a program that reads no voltage asks the board for its converter"
            );
            // Nor for a PWM signal (decision 48.4).
            assert!(
                !c.contains("static bool zeno_board_pwm(") && !c.contains("zeno_board_pwm((int)"),
                "a program with no PWM pin asks the board for one"
            );
            // Nor does it keep the table of tickers (decision 49.4).
            assert!(!c.contains("static double zeno_ticker_due[") && !c.contains("ZENO_TICKING"), "a program with no ticker keeps their table");
            // Nor does it ask the board to count edges (decision 50.4).
            assert!(
                !c.contains("static bool zeno_board_interrupt(") && !c.contains("zeno_edges[pin]++") && !c.contains("ZENO_INTERRUPTS"),
                "a program with no interrupt asks the board to count edges"
            );
            assert!(!c.contains("ZENO_INPUT_PINS"));
        }
        for c in [compile(INTERRUPTS).unwrap(), compile_firmware(INTERRUPTS).unwrap(), compile_no_collector(INTERRUPTS).unwrap()] {
            assert!(c.contains("zeno_board_interrupt((int)pin, which)") && c.contains("zeno_edges[pin]++"), "the edges are not counted");
            assert!(!c.contains("ZENO_INTERRUPTS") && !c.contains("static double zeno_ticker_due["));
        }
        for c in [compile(TICKERS).unwrap(), compile_firmware(TICKERS).unwrap(), compile_no_collector(TICKERS).unwrap()] {
            assert!(c.contains("static double zeno_ticker_due[ZENO_TICKERS];"), "the tickers are not kept");
            assert!(!c.contains("static void zeno_board_input(") && !c.contains("ZENO_TICKING"));
        }
        for c in [compile(PWM).unwrap(), compile_firmware(PWM).unwrap(), compile_no_collector(PWM).unwrap()] {
            assert!(c.contains("zeno_board_pwm((int)"), "the PWM pins are not made");
            assert!(!c.contains("static void zeno_board_input(") && !c.contains("ZENO_PWM_PINS"));
        }
        for c in [compile(INPUTS).unwrap(), compile_firmware(INPUTS).unwrap(), compile_no_collector(INPUTS).unwrap()] {
            assert!(c.contains("zeno_board_input((int)pin, "), "the input pins are not made");
            assert!(!c.contains("ZENO_INPUT_PINS"));
        }
    }

    /// The doorbell on a board of its own, whose button is held down for
    /// three of the times it is read, and whose clock moves only when the
    /// program sleeps. It is how a button is pressed on a desk.
    #[test]
    fn a_button_is_pressed_on_a_board_of_its_own() {
        let header = "#define ZENO_PINS 10\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static int my_reads = 0;\n\
                      static double my_clock = 0.0;\n\
                      static bool zeno_board_read(int pin) {\n\
                      \x20   if (pin != 2) return my_levels[pin];\n\
                      \x20   my_reads++;\n\
                      \x20   return !(my_reads >= 3 && my_reads <= 5);\n\
                      }\n\
                      static void zeno_board_write(int pin, bool level) {\n\
                      \x20   my_levels[pin] = level;\n\
                      \x20   printf(\"write %d %d at %g\\n\", pin, (int)level, my_clock);\n\
                      }\n\
                      static void zeno_board_input(int pin, int pull) { printf(\"input %d %d\\n\", pin, pull); }\n\
                      static void zeno_board_sleep(double seconds) { my_clock += seconds * 1000.0; }\n\
                      static double zeno_board_now(void) { return my_clock; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-button", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        let source = include_str!("../examples/doorbell.zeno");
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("button-header", &c, &[&flag]) else {
                return;
            };
            assert!(finished, "{errors}");
            assert_eq!(printed, "input 2 1\nwrite 9 1 at 10\nwrite 9 0 at 60\npresses: 1\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins whose voltage is read (decision 47).
    const ANALOG: &str = "from board import analog_pin, pin, input_pin, pull_up\nimport board\n\
        let sensor = analog_pin(0)\nlet other = board.analog_pin(4)\n\
        print(sensor, sensor.number, sensor.max_count, sensor == other, f\"{other}\")\n\
        print(sensor.voltage(), sensor.count())\npin(4).on()\n\
        print(other.voltage(), other.count(), other.voltage().mV)\n\
        let level: V = other.voltage()\n\
        if level > 1.5 V {\n    print(\"high enough\", level / 2, level * 20 mA)\n}\n\
        function share(of: AnalogPin) -> float {\n    return of.count() / of.max_count\n}\n\
        print(share(sensor), share(other))\n\
        let pins: list[AnalogPin] = [sensor, other]\n\
        for p in pins {\n    print(p.number, p.voltage())\n}\n\
        var spare: AnalogPin or none = none\nprint(spare)\nspare = other\n\
        if spare is not none { print(spare.count()) }\n\
        input_pin(7, pull_up)\nprint(analog_pin(7).voltage())\n";

    #[test]
    fn analog_pins_match() {
        assert!(interpreted(ANALOG).contains("3.3 V 4095 3300 mV\nhigh enough 1.65 V 0.066 W\n0.0 1.0\n"), "{}", interpreted(ANALOG));
        same_as_interpreter("analog", ANALOG);
        no_collector_as_interpreter("analog", ANALOG);
        same_as_interpreter(
            "no-analog-pin",
            "from board import analog_pin\n\
             try {\n    analog_pin(64)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
             analog_pin(-1)\n",
        );
    }

    /// A board of its own whose converter counts with 10 bits against
    /// 5 V, reads a ramp on pin 0, and cannot read pin 1 at all.
    #[test]
    fn a_voltage_is_read_on_a_board_of_its_own() {
        let header = "#define ZENO_PINS 2\n\
                      #define ZENO_ANALOG_BITS 10\n\
                      #define ZENO_ANALOG_REFERENCE 5.0\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static int my_ramp = 0;\n\
                      static bool zeno_board_read(int pin) { return my_levels[pin]; }\n\
                      static void zeno_board_write(int pin, bool level) { my_levels[pin] = level; }\n\
                      static void zeno_board_sleep(double seconds) { (void)seconds; }\n\
                      static double zeno_board_now(void) { return 0.0; }\n\
                      static bool zeno_board_analog(int pin) { printf(\"analog %d\\n\", pin); return pin == 0; }\n\
                      static int zeno_board_analog_read(int pin) { (void)pin; my_ramp += 341; return my_ramp; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-analog", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        // In a firmware build an error of the runtime stops the program,
        // so the pin that cannot be read ends it there.
        let source = "from board import analog_pin\nlet sensor = analog_pin(0)\n\
                      print(sensor.max_count, sensor.count(), sensor.count(), sensor.voltage())\n\
                      analog_pin(1)\n";
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("analog-header", &c, &[&flag]) else {
                return;
            };
            // 1023 of 1023 is the whole 5 V.
            assert_eq!(printed, "analog 0\n1023 341 682 5 V\nanalog 1\n");
            assert!(!finished);
            assert_eq!(errors, "error: uncaught ValueError: pin 1 of this board cannot read a voltage\n --> test.zeno:4:1\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PWM signals (decision 48). The simulated board prints a line each
    /// time a signal changes.
    const PWM: &str = "from board import pwm_pin, pin, led\nimport board\n\
        let lamp = pwm_pin(9, 1 kHz)\nlet servo = board.pwm_pin(10, 50 Hz)\n\
        print(lamp, lamp.number, lamp == servo, f\"{servo}\")\n\
        lamp.duty(0.25)\nlamp.duty(0.25)\nlamp.duty(1.0)\nlamp.duty(0.0)\nlamp.duty(0.0)\n\
        servo.pulse(1500 us)\nservo.pulse(2 ms)\nservo.pulse(0.02 s)\nservo.pulse(0 ms)\n\
        let rate: Hz = 1 / 400 us\nlet fast = pwm_pin(3, rate)\nfast.duty(0.5)\n\
        let slow = pwm_pin(4, 1 / 2 s)\nslow.pulse(500 ms)\n\
        function fade(light: PwmPin, steps: int) {\n    for step in range(steps + 1) {\n        light.duty(step / steps)\n    }\n}\n\
        fade(lamp, 4)\n\
        let lights: list[PwmPin] = [lamp, fast]\n\
        for light in lights {\n    light.duty(0.125)\n    print(light.number)\n}\n\
        var spare: PwmPin or none = none\nprint(spare)\nspare = servo\n\
        if spare is not none { spare.pulse(1 ms) }\n\
        led.on()\nlet glow = pwm_pin(13, 2.5 kHz)\nglow.duty(0.5)\nprint(led.is_on())\n\
        pin(5).on()\nlet again = pwm_pin(9, 2 kHz)\nagain.duty(0.125)\nagain.duty(0.125)\nlamp.duty(0.125)\n";

    #[test]
    fn pwm_pins_match() {
        let printed = interpreted(PWM);
        assert!(
            printed.starts_with(
                "PwmPin(9) 9 false PwmPin(10)\npin 9 pwm 0.25 at 1000 Hz\npin 9 pwm 1.0 at 1000 Hz\npin 9 pwm 0.0 at 1000 Hz\n\
                 pin 10 pwm 0.075 at 50 Hz\npin 10 pwm 0.1 at 50 Hz\npin 10 pwm 1.0 at 50 Hz\npin 10 pwm 0.0 at 50 Hz\n\
                 pin 3 pwm 0.5 at 2500 Hz\npin 4 pwm 0.25 at 0.5 Hz\n"
            ),
            "{printed}"
        );
        same_as_interpreter("pwm", PWM);
        no_collector_as_interpreter("pwm", PWM);
        for (name, source) in [
            ("pwm-no-pin", "from board import pwm_pin\npwm_pin(64, 1 kHz)\n"),
            ("pwm-no-rate", "from board import pwm_pin\nlet rate = 0 Hz\npwm_pin(9, rate)\n"),
            ("pwm-duty-over", "from board import pwm_pin\nlet lamp = pwm_pin(9, 1 kHz)\nlet share = 1.5\nlamp.duty(share)\n"),
            ("pwm-duty-under", "from board import pwm_pin\nlet lamp = pwm_pin(9, 1 kHz)\nlamp.duty(-0.1)\n"),
            ("pwm-pulse-long", "from board import pwm_pin\nlet servo = pwm_pin(9, 50 Hz)\nservo.pulse(30 ms)\n"),
            ("pwm-pulse-under", "from board import pwm_pin\nlet servo = pwm_pin(9, 50 Hz)\nservo.pulse(-1 ms)\n"),
            (
                "pwm-caught",
                "from board import pwm_pin\nlet lamp = pwm_pin(9, 1 kHz)\n\
                 try {\n    lamp.duty(2.0)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
                 try {\n    lamp.pulse(2 ms)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
                 try {\n    pwm_pin(-1, 1 kHz)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
                 lamp.duty(1.0)\n",
            ),
        ] {
            same_as_interpreter(name, source);
        }
    }

    /// A board of its own with a timer on pins 0 and 1 only. It is told
    /// the pin, the frequency in hertz and the share of the time on.
    #[test]
    fn a_pwm_signal_on_a_board_of_its_own() {
        let header = "#define ZENO_PINS 3\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static bool zeno_board_read(int pin) { return my_levels[pin]; }\n\
                      static void zeno_board_write(int pin, bool level) { my_levels[pin] = level; }\n\
                      static void zeno_board_sleep(double seconds) { (void)seconds; }\n\
                      static double zeno_board_now(void) { return 0.0; }\n\
                      static bool zeno_board_pwm(int pin, double hertz, double duty) {\n\
                      \x20   printf(\"pwm %d %g %g\\n\", pin, hertz, duty);\n\
                      \x20   return pin < 2;\n\
                      }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-pwm", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        // In a firmware build an error of the runtime stops the program,
        // so the pin with no timer ends it there.
        let source = "from board import pwm_pin\nlet lamp = pwm_pin(0, 20 kHz)\nlamp.duty(0.5)\n\
                      let servo = pwm_pin(1, 50 Hz)\nservo.pulse(1500 us)\n\
                      pwm_pin(2, 1 kHz)\n";
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("pwm-header", &c, &[&flag]) else {
                return;
            };
            // Making a pin asks the board for the signal, off all the time.
            assert_eq!(printed, "pwm 0 20000 0\npwm 0 20000 0.5\npwm 1 50 0\npwm 1 50 0.075\npwm 2 1000 0\n");
            assert!(!finished);
            assert_eq!(errors, "error: uncaught ValueError: pin 2 of this board cannot make a PWM signal\n --> test.zeno:6:1\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tickers (decision 49). The clock is the computer's, so the program
    /// prints only what is true of any clock.
    const TICKERS: &str = "from board import ticker, led\nfrom time import now, sleep\nimport board\n\
        let start = now()\nlet beat = ticker(5 ms)\n\
        print(beat, beat.period, beat.period.s, beat == beat, f\"{beat}\")\n\
        var total = 0\nfor n in range(4) {\n    total += beat.wait()\n}\n\
        print(total >= 4, now() - start >= 20 ms, now() - start >= total * beat.period)\n\
        let slow = board.ticker(2 ms)\nprint(slow == beat)\nsleep(11 ms)\n\
        print(slow.ticks() >= 5, slow.wait() >= 1)\n\
        let far = ticker(1 h)\nprint(far, far.ticks(), far.period.s, ticker(0.25 s))\n\
        let idle = ticker(40 ms)\nsleep(45 ms)\nidle.restart()\nprint(idle.ticks())\n\
        function run(clock: Ticker, times: int) -> int {\n    var late = 0\n    \
        for n in range(times) {\n        if clock.wait() > 1 { late += 1 }\n        led.toggle()\n    }\n    return late\n}\n\
        print(run(ticker(0.5 s / 100), 3) >= 0)\n\
        let all: list[Ticker] = [beat, slow]\nprint(all, far in all, beat in all)\n\
        var spare: Ticker or none = none\nprint(spare)\nspare = far\n\
        if spare is not none { print(spare.ticks(), spare.period) }\n";

    #[test]
    fn tickers_match() {
        let printed = interpreted(TICKERS);
        assert_eq!(
            printed,
            "Ticker(5 ms) 5 ms 0.005 s true Ticker(5 ms)\ntrue true true\nfalse\ntrue true\n\
             Ticker(3600000 ms) 0 3600 s Ticker(250 ms)\n0\nled on\nled off\nled on\ntrue\n\
             [Ticker(5 ms), Ticker(2 ms)] false true\nnone\n0 3600000 ms\n"
        );
        same_as_interpreter("tickers", TICKERS);
        no_collector_as_interpreter("tickers", TICKERS);
        for (name, source) in [
            ("ticker-no-time", "from board import ticker\nlet none_at_all = 0 ms\nticker(none_at_all)\n"),
            ("ticker-backwards", "from board import ticker\nticker(-1 s)\n"),
            ("ticker-too-many", "from board import ticker\nfor n in range(17) {\n    print(n, ticker(1 s))\n}\n"),
            (
                "ticker-caught",
                "from board import ticker\n\
                 try {\n    ticker(0 us)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
                 var made = 0\n\
                 try {\n    for n in range(20) {\n        ticker(1 s)\n        made += 1\n    }\n} except ValueError as error {\n    print(made, error.message)\n}\n",
            ),
        ] {
            same_as_interpreter(name, source);
        }
    }

    /// A board of its own whose clock moves only when the program
    /// sleeps, so every tick is where the sums say. Its first sleep ends
    /// a millisecond early, as a real one may, and it keeps two tickers.
    #[test]
    fn a_ticker_on_a_board_of_its_own() {
        let header = "#define ZENO_PINS 2\n\
                      #define ZENO_TICKERS 2\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static double my_clock = 100.0;\n\
                      static bool my_first = true;\n\
                      static bool zeno_board_read(int pin) { return my_levels[pin]; }\n\
                      static void zeno_board_write(int pin, bool level) { my_levels[pin] = level; }\n\
                      static void zeno_board_sleep(double seconds) {\n\
                      \x20   printf(\"sleep %g\\n\", seconds * 1000.0);\n\
                      \x20   my_clock += seconds * 1000.0 - (my_first ? 1.0 : 0.0);\n\
                      \x20   my_first = false;\n\
                      }\n\
                      static double zeno_board_now(void) { return my_clock; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-ticker", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        // In a firmware build an error of the runtime stops the program,
        // so the ticker the board has no room for ends it there.
        let source = "from board import ticker\nfrom time import sleep, now\n\
                      let beat = ticker(10 ms)\nprint(beat.ticks())\nprint(beat.wait())\n\
                      sleep(35 ms)\nprint(beat.ticks(), beat.ticks())\nprint(beat.wait())\n\
                      sleep(3 ms)\nbeat.restart()\nprint(beat.wait(), now())\n\
                      sleep(10 ms)\nprint(beat.wait(), now())\n\
                      let second = ticker(1 s)\nprint(second, second.ticks())\n\
                      ticker(1 s)\n";
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("ticker-header", &c, &[&flag]) else {
                return;
            };
            // The clock starts at 100. The first tick is due at 110: the
            // sleep ends at 109, so there is a second, of 1 ms. Then 35 ms
            // go by, to 145: the ticks of 120, 130 and 140 have come, and
            // the next is at 150. Started again at 153, it ticks at 163.
            // The tick of 173 has come when it is waited for, with no sleep.
            assert_eq!(
                printed,
                "0\nsleep 10\nsleep 1\n1\nsleep 35\n3 0\nsleep 5\n1\nsleep 3\nsleep 10\n1 163 ms\nsleep 10\n1 173 ms\n\
                 Ticker(1000 ms) 0\n"
            );
            assert!(!finished);
            assert_eq!(
                errors,
                "error: uncaught ValueError: this board keeps 2 tickers, and this would be one more. \
                 Make each ticker once, outside any loop\n --> test.zeno:16:1\n"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Interrupts (decision 50). Nothing is wired to the simulated board,
    /// so the edges are the ones the program drives itself.
    const INTERRUPTS: &str = "from board import input_pin, pin, pull_up, rising, falling, either, led\nimport board\n\
        let line = pin(4)\nlet button = input_pin(4)\nlet press = button.interrupt(rising)\n\
        print(press, press.number, press == press, press.edges(), f\"{press}\")\n\
        line.on()\nline.off()\nline.on()\nprint(press.edges(), press.edges())\n\
        line.off()\nprint(press.edges())\nline.on()\nprint(press.wait(), button.is_high())\n\
        let drop = input_pin(5).interrupt(falling)\n\
        let any: Interrupt = board.input_pin(6).interrupt(board.either)\n\
        pin(5).on()\npin(5).off()\npin(5).off()\npin(6).toggle()\npin(6).toggle()\npin(6).toggle()\n\
        print(drop.edges(), any.wait(), drop == any)\n\
        function count(of: Interrupt) -> int {\n    return of.edges()\n}\n\
        function watch(what: InputPin, which: Edge) -> Interrupt {\n    return what.interrupt(which)\n}\n\
        let other = watch(input_pin(7), rising)\npin(7).on()\n\
        print(count(other), count(other), rising, f\"{falling}\", either == rising, either == board.either)\n\
        let kinds: list[Edge] = [rising, falling]\nprint(kinds, either in kinds)\n\
        let all: list[Interrupt] = [press, drop]\nprint(all, any in all)\n\
        var spare: Interrupt or none = none\nprint(spare)\nspare = any\n\
        if spare is not none { print(spare.edges()) }\n\
        let again = button.interrupt(falling)\nline.off()\nprint(again.edges(), press.edges(), again == press)\n\
        let held = input_pin(8, pull_up).interrupt(either)\nprint(held.edges())\n\
        led.on()\nlet glow = input_pin(13).interrupt(either)\nled.off()\nled.toggle()\nprint(glow.wait())\n";

    #[test]
    fn interrupts_match() {
        let printed = interpreted(INTERRUPTS);
        assert!(printed.starts_with("Interrupt(4) 4 true 0 Interrupt(4)\npin 4 on\npin 4 off\npin 4 on\n2 0\npin 4 off\n0\npin 4 on\n1 true\n"), "{printed}");
        assert!(
            printed.ends_with(
                "1 3 false\npin 7 on\n1 0 rising falling false true\n[\"rising\", \"falling\"] false\n[Interrupt(4), Interrupt(5)] false\n\
                 none\n0\npin 4 off\n1 0 true\n0\nled on\nled off\nled on\n2\n"
            ),
            "{printed}"
        );
        same_as_interpreter("interrupts", INTERRUPTS);
        no_collector_as_interpreter("interrupts", INTERRUPTS);
        for (name, source) in [
            // Nothing can drive a pin while the program waits.
            ("interrupt-for-ever", "from board import input_pin, falling\nlet press = input_pin(2).interrupt(falling)\nprint(press.edges())\npress.wait()\n"),
            (
                "interrupt-caught",
                "from board import input_pin, pin, rising\nlet press = input_pin(2).interrupt(rising)\n\
                 try {\n    print(press.wait())\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
                 pin(2).on()\nprint(press.wait())\n",
            ),
        ] {
            same_as_interpreter(name, source);
        }
    }

    /// A board of its own, which counts the edges itself: three have
    /// come the second time it is asked, a wait gives two, and pin 1 has
    /// no interrupt.
    #[test]
    fn edges_are_counted_by_a_board_of_its_own() {
        let header = "#define ZENO_PINS 3\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static int my_asked = 0;\n\
                      static bool zeno_board_read(int pin) { return my_levels[pin]; }\n\
                      static void zeno_board_write(int pin, bool level) { my_levels[pin] = level; }\n\
                      static void zeno_board_sleep(double seconds) { (void)seconds; }\n\
                      static double zeno_board_now(void) { return 0.0; }\n\
                      static void zeno_board_input(int pin, int pull) { printf(\"input %d %d\\n\", pin, pull); }\n\
                      static bool zeno_board_interrupt(int pin, int edge) { printf(\"interrupt %d %d\\n\", pin, edge); return pin != 1; }\n\
                      static int zeno_board_edges(int pin) { (void)pin; my_asked++; return my_asked == 2 ? 3 : 0; }\n\
                      static int zeno_board_edges_wait(int pin) { printf(\"wait %d\\n\", pin); return 2; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-edges", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        // In a firmware build an error of the runtime stops the program,
        // so the pin with no interrupt ends it there.
        let source = "from board import input_pin, pull_up, falling, rising, either\n\
                      let press = input_pin(0, pull_up).interrupt(falling)\n\
                      print(press.edges(), press.edges(), press.edges())\nprint(press.wait())\n\
                      let both = input_pin(2).interrupt(either)\nprint(both.wait())\n\
                      input_pin(1).interrupt(rising)\n";
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("edges-header", &c, &[&flag]) else {
                return;
            };
            assert_eq!(
                printed,
                "input 0 1\ninterrupt 0 2\n0 3 0\nwait 0\n2\ninput 2 0\ninterrupt 2 3\nwait 2\n2\ninput 1 0\ninterrupt 1 1\n"
            );
            assert!(!finished);
            assert_eq!(errors, "error: uncaught ValueError: pin 1 of this board cannot count its edges\n --> test.zeno:7:1\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A board of `boards/`, as its `zeno.board` says it.
    fn board_named(name: &str) -> (std::path::PathBuf, crate::board_build::Board) {
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("boards").join(name);
        let said = std::fs::read_to_string(folder.join(crate::board_build::FILE)).expect("the board has a `zeno.board`");
        let board = crate::board_build::parse(&said).unwrap_or_else(|errors| panic!("{}", errors[0].message));
        (folder, board)
    }

    /// Build the C for the emulated Cortex-M3 as `zeno build --board`
    /// does, and run it in QEMU: what it printed, what it wrote as
    /// errors, and whether it finished cleanly. `None` where the
    /// compiler for ARM or QEMU is not installed.
    fn on_the_emulated_board(name: &str, c: &str, before: &[&str]) -> Option<(String, String, bool)> {
        on_the_board("qemu-cortex-m3", name, c, before)
    }

    /// The same for any board of `boards/` that QEMU runs. What a test
    /// has typed (`with_outside`) comes in at its first serial port.
    fn on_the_board(which: &str, name: &str, c: &str, before: &[&str]) -> Option<(String, String, bool)> {
        let (folder, board) = board_named(which);
        let there = |tool: &str| Command::new(tool).arg("--version").output().is_ok();
        if !there(&board.compiler) || !there(&board.run[0]) {
            eprintln!("no `{}` or no `{}` here; skipping the run on the emulated board", board.compiler, board.run[0]);
            return None;
        }
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let (c_file, program) = (dir.join("test.c"), dir.join("test.elf"));
        std::fs::write(&c_file, c).expect("the C file is written");
        let before: Vec<String> = before.iter().map(|flag| flag.to_string()).collect();
        let after: Vec<String> = LIBRARIES.iter().map(|library| library.to_string()).collect();
        let asks = board.asks(&folder, &before, &c_file.to_string_lossy(), &program.to_string_lossy(), &after);
        let built = Command::new(&board.compiler).args(&asks).output().expect("the compiler runs");
        let complaints: String = String::from_utf8_lossy(&built.stderr).chars().take(1500).collect();
        assert!(built.status.success(), "the C does not build for the board:\n{complaints}");
        let run = board.runs(&program.to_string_lossy()).expect("the board says how a program is run");
        let ran = crate::value::testing::run(Command::new(&run[0]).args(&run[1..])).expect("QEMU runs");
        let _ = std::fs::remove_dir_all(&dir);
        // QEMU says one thing of its own as this board starts.
        let errors = String::from_utf8_lossy(&ran.stderr).replace("Timer with period zero, disabling\n", "");
        Some((String::from_utf8_lossy(&ran.stdout).into_owned(), errors, ran.status.success()))
    }

    /// The folder of the board that comes with Zeno is whole: every file
    /// its `zeno.board` names is there.
    #[test]
    fn the_emulated_board_has_its_files() {
        let (folder, board) = board_named("qemu-cortex-m3");
        assert_eq!(board.missing(&folder), Vec::<String>::new());
        assert_eq!(board.compiler, "arm-none-eabi-gcc");
        // It has no header: its pins are the simulated ones.
        assert_eq!(board.header, None);
        assert_eq!(board.sources, ["startup.c", "system.c"]);
        assert_eq!(board.script.as_deref(), Some("link.ld"));
    }

    /// One program with every part of a board in it, whole numbers of 64
    /// bits, floats, text, a list, a dict, an error that is caught and
    /// one that is not: on an ARM Cortex-M3 with 64 KB of RAM, which
    /// QEMU emulates, it prints what `zeno run` prints, in the build
    /// that checks and in the minimal one, and it ends as an error ends.
    #[test]
    fn a_program_does_on_a_cortex_m3_what_it_does_under_zeno_run() {
        let source = "from board import led, pin, input_pin, pull_up, analog_pin, pwm_pin, ticker, falling\n\
                      from time import sleep, now\n\
                      \n\
                      class Reading {\n\
                          volts: float\n\
                          count: int\n\
                      }\n\
                      \n\
                      function mean(values: list[float]) -> float raises ValueError {\n\
                          if len(values) == 0 {\n\
                              raise ValueError(\"no values\")\n\
                          }\n\
                          var sum = 0.0\n\
                          for value in values {\n\
                              sum += value\n\
                          }\n\
                          return sum / len(values)\n\
                      }\n\
                      \n\
                      led.on()\n\
                      sleep(2 ms)\n\
                      led.off()\n\
                      let button = input_pin(2, pull_up)\n\
                      print(button.is_high(), button.number)\n\
                      let sensor = analog_pin(2)\n\
                      let reading = Reading(sensor.voltage() / 1 V, sensor.count())\n\
                      print(reading, sensor.max_count)\n\
                      let fan = pwm_pin(9, 1 kHz)\n\
                      fan.duty(0.25)\n\
                      let beat = ticker(5 ms)\n\
                      print(beat.wait() >= 1, beat)\n\
                      let press = button.interrupt(falling)\n\
                      pin(2).off()\n\
                      pin(2).on()\n\
                      pin(2).off()\n\
                      print(press.edges(), press.edges())\n\
                      let started = now()\n\
                      sleep(3 ms)\n\
                      print(now() - started >= 3 ms)\n\
                      print(2 ** 40, 7 // 2, -7 % 3, 1.0 / 3.0, 9007199254740993, 0.1 + 0.2, 1e300 * 10.0)\n\
                      let big: u64 = 18446744073709551615\n\
                      let small: f32 = 0.1\n\
                      print(big, small, 3000000000 * 3, \"héllo\".upper(), len(\"héllo\"))\n\
                      try {\n\
                          print(mean([1.5, 2.5, 5.0]))\n\
                          print(mean([]))\n\
                      } except ValueError as error {\n\
                          print(\"caught:\", error.message)\n\
                      }\n\
                      let names: dict[str, int] = [\"a\": 1]\n\
                      names[\"b\"] = 2\n\
                      print(names, [n * n for n in range(5)])\n";
        let mistake = "let numbers = [1, 2, 3]\nprint(numbers[5])\n";
        let wanted = interpreted(source);
        assert!(wanted.ends_with("[\"a\": 1, \"b\": 2] [0, 1, 4, 9, 16]\n"), "{wanted}");
        let checked = compile_no_collector(&format!("{source}{mistake}")).unwrap();
        let Some((printed, errors, finished)) = on_the_emulated_board("cortex-m3", &checked, &["-O2"]) else {
            return;
        };
        assert_eq!(printed, wanted);
        assert!(!finished);
        assert_eq!(errors, "error: uncaught IndexError: index 5 is out of range for a list of length 3\n --> test.zeno:54:7\n");
        // The least C, which checks nothing, so without the mistake, as
        // `--board --minimal` writes it: `print` through the runtime.
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        let options = Options { minimal: true, board_prints: true, ..Default::default() };
        let minimal = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
        let (printed, errors, finished) = on_the_emulated_board("cortex-m3-minimal", &minimal, &["-Os"]).expect("as above");
        assert_eq!((printed, errors.as_str(), finished), (wanted, "", true));
    }

    /// A header that says `ZENO_BOARD_PRINTS` brings four functions,
    /// and then nothing of the program goes through the C library's
    /// streams or its `exit` (decision 52). This board, on a computer,
    /// shows it by crossing the two streams: what is printed comes out
    /// of the error stream, what is said beside it out of the other, and
    /// the number a program stops with is 40 more than it gave.
    const PRINTING_BOARD: &str = "#include <stdio.h>\n\
        #include <stdlib.h>\n\
        #define ZENO_BOARD_PRINTS\n\
        #define ZENO_PINS 2\n\
        static bool my_levels[ZENO_PINS];\n\
        static bool zeno_board_read(int pin) { return my_levels[pin % 2]; }\n\
        static void zeno_board_write(int pin, bool level) { my_levels[pin % 2] = level; }\n\
        static void zeno_board_sleep(double seconds) { (void)seconds; }\n\
        static double zeno_board_now(void) { return 0.0; }\n\
        static void zeno_board_print(const char *bytes, size_t count) { fwrite(bytes, 1, count, stderr); }\n\
        static void zeno_board_warn(const char *bytes, size_t count) { fwrite(bytes, 1, count, stdout); }\n\
        static int zeno_board_typed(void) {\n\
        \x20   static const char typed[] = \"Ada\\n\";\n\
        \x20   static size_t at = 0;\n\
        \x20   return at < sizeof typed - 1 ? typed[at++] : -1;\n\
        }\n\
        static void zeno_board_stop(int status) { exit(status + 40); }\n";

    /// The C of every kind of build, with `PRINTING_BOARD` under it:
    /// what came out of each stream, and the number it ended with.
    fn on_the_printing_board(name: &str, source: &str, with_minimal: bool) -> Vec<(String, String, Option<i32>)> {
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-{name}-header", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("printing_board.h");
        std::fs::write(&file, PRINTING_BOARD).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        let mut kinds = vec![compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()];
        if with_minimal {
            let program = crate::modules::testing::built(source);
            for (build, grows) in [(Build::Ordinary, false), (Build::NoCollector, false), (Build::Firmware, false)] {
                let (errors, types) = crate::checker::check_by(&program, build.rules());
                assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
                let options = Options { grows, minimal: true, board_prints: true, board_header: true, ..Default::default() };
                kinds.push(emit_with(&program, &types, "test.zeno", build, options).expect("it builds"));
            }
        }
        let mut ran = Vec::new();
        for c in kinds {
            let Some(output) = built_output(name, &c, &[&flag]) else { break };
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            ran.push((text(&output.stdout), text(&output.stderr), output.status.code()));
        }
        let _ = std::fs::remove_dir_all(&dir);
        ran
    }

    #[test]
    fn a_board_can_print_read_and_stop_by_itself() {
        let source = "import program\n\
                      print(\"a\", 1, 2.5, [1, 2])\n\
                      program.warn(\"careful\")\n\
                      let name = input(\"name? \")\n\
                      print(f\"hi {name}\")\n\
                      print(input())\n\
                      program.exit(3)\n";
        for (warned, printed, status) in on_the_printing_board("board-prints", source, true) {
            // Nothing more was typed: the board said so with -1.
            assert_eq!(printed, "a 1 2.5 [1, 2]\nname? hi Ada\nnone\n");
            assert_eq!(warned, "careful\n");
            assert_eq!(status, Some(43));
        }
        // The error that stops a program is said through the board too,
        // and the program stops through it.
        for (said, printed, status) in on_the_printing_board("board-stops", "let numbers = [1, 2, 3]\nprint(\"before\")\nprint(numbers[5])\n", false) {
            assert_eq!(printed, "before\n");
            assert_eq!(said, "error: uncaught IndexError: index 5 is out of range for a list of length 3\n --> test.zeno:3:7\n");
            assert_eq!(status, Some(41));
        }
    }

    /// A minimal build writes `print` as `printf`, and only the half of
    /// the outside that the C library does. For a board it writes
    /// `print` through the runtime, and for a board with a header it
    /// leaves the choice of who prints to the header (decision 52.4).
    #[test]
    fn a_minimal_build_prints_through_a_board_that_has_a_header() {
        let source = "let n = 3\nprint(\"n is\", n)\n";
        let plain = compile_minimal(source, Build::NoCollector, false);
        assert!(plain.contains("printf(") && !plain.contains("ZENO_BOARD"), "{plain}");
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        // A board with no header: the runtime prints, through the C library.
        let options = Options { minimal: true, board_prints: true, ..Default::default() };
        let bare = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
        assert!(!bare.contains("printf(") && !bare.contains("ZENO_BOARD") && bare.contains("fwrite(bytes, 1, count, stdout);"), "{bare}");
        let options = Options { minimal: true, board_prints: true, board_header: true, ..Default::default() };
        let board = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
        assert!(!board.contains("printf("), "{board}");
        for wanted in ["#include ZENO_BOARD\n", "#ifdef ZENO_BOARD_PRINTS\n", "zeno_board_print(bytes, count);", "zeno_print(2, \"n is\", "] {
            assert!(board.contains(wanted), "no `{wanted}` in:\n{board}");
        }
        // The report of `--memory` cannot be written by a board that
        // stops itself, and the C says so when it is compiled.
        let options = Options { minimal: true, board_prints: true, board_header: true, memory_report: true, ..Default::default() };
        let reported = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
        assert!(reported.contains("#error \"`--memory` does not go with a board that prints and stops itself"), "{reported}");
    }

    /// Decision 53: the runtime makes a float into text, writes it with
    /// a fixed number of decimals and reads it out of text by itself,
    /// with whole numbers and exactly, and asks the C library for none
    /// of it. This holds it to what Rust answers, which is what
    /// `zeno run` prints, over many thousands of floats.
    #[test]
    fn a_float_is_written_and_read_with_no_c_library() {
        for gone in ["snprintf(", "sprintf(", "strtod(", "strtof(", "atoi(", "sscanf("] {
            assert!(!RUNTIME.contains(gone), "the runtime calls `{gone}`");
        }
        // Numbers of every kind, from a row that is the same each time.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut floats: Vec<f64> = vec![
            0.0, -0.0, 0.1, 0.1 + 0.2, 2f64.powi(-44), 1e15 + 0.25, 5e-324, f64::MAX, f64::MIN_POSITIVE, 1e16, 1e-4,
            9.999999999999999e-5, 0.125, 0.375, 9.995, 1e21, 1e23, f64::INFINITY, f64::NEG_INFINITY, f64::NAN,
        ];
        for i in 0..12000 {
            let bits = next();
            floats.push(match i % 6 {
                // Any float at all.
                0 => f64::from_bits(bits),
                // What a sensor reads.
                1 => (bits % 2_000_000) as f64 / 1000.0 - 1000.0,
                // Near 1.
                2 => f64::from_bits((bits & 0x800F_FFFF_FFFF_FFFF) | ((963 + next() % 120) << 52)),
                // The smallest ones, which have fewer bits.
                3 => f64::from_bits(bits & 0x800F_FFFF_FFFF_FFFF),
                // Whole.
                4 => (bits >> (next() % 64)) as f64,
                // A power of two, where the float below is nearer than the one above.
                _ => f64::from_bits(bits & 0xFFF0_0000_0000_0000),
            });
        }

        // Every power of two: the float below one is nearer than the
        // float above, and 46 of them are named by digits that are not
        // the rounded ones (decision 53.2).
        floats.extend((-1074..=1023).map(|power| 2f64.powi(power)));

        // A float written: as it is, and with decimals.
        let writes = "while true {\n    let line = input()\n    if line is none {\n        break\n    }\n    let x = float(line)\n    print(x, f\"{x:.0f} {x:.2f} {x:.9f}\")\n}\n";
        let typed: String = floats.iter().map(|x| format!("{x:e}\n")).collect();
        let wanted: String =
            floats.iter().map(|x| format!("{} {x:.0} {x:.2} {x:.9}\n", crate::value::float_text(*x))).collect();
        assert!(wanted.contains("5.684341886080802e-14 0 0.00 0.000000000\n") && wanted.contains("NaN NaN NaN NaN\n"));
        same_floats("floats-written", writes, &typed, &wanted);

        // An f32 written.
        let narrow: Vec<f32> = (0..6000)
            .map(|i| match i % 3 {
                0 => f32::from_bits(next() as u32),
                1 => (next() % 200_000) as f32 / 100.0 - 1000.0,
                _ => f32::from_bits((next() as u32) & 0xFF80_0000),
            })
            .collect();
        let writes = "while true {\n    let line = input()\n    if line is none {\n        break\n    }\n    let x = f32(float(line))\n    print(x, f\"{x:.3f}\")\n}\n";
        let typed: String = narrow.iter().map(|x| format!("{:e}\n", *x as f64)).collect();
        let wanted: String = narrow.iter().map(|x| format!("{} {:.3}\n", crate::value::f32_text(*x), *x as f64)).collect();
        same_floats("f32s-written", writes, &typed, &wanted);

        // A float read. First what `zeno run` takes and what it refuses.
        let mut texts: Vec<String> = [
            " 2.5 ", "+2.5", "1.", ".5", "-.5e1", "1E5", "INF", "-Infinity", "+inf", "nan", "NaN", "0", "-0", "000.000",
            "00012.5000", "1e400", "-1e400", "1e-400", "0e999999999", "1e999999999", "1e-999999999", "4.9e-324", "2.47e-324",
            "2.48e-324", "1.7976931348623158e308", "1.7976931348623159e308", "9007199254740993", "9007199254740992.99999999999999999999",
            "", " ", ".", "e5", "1e", "1e+", "1_0", "0x10", "nan(1)", "infinit", "in", "1.2.3", "--1", "+-1", "1 2", "1e5.0", "1f",
            "NaN0", "-", "+",
        ]
        .iter()
        .map(|text| text.to_string())
        .collect();
        for (i, x) in floats.iter().enumerate().filter(|(_, x)| x.is_finite()).take(4000) {
            // Digits enough, too few, and far too many.
            texts.push(format!("{:.*e}", (next() % 25) as usize, x));
            // Any digits, with a point anywhere and any power of ten.
            let digits: String = (0..1 + next() % 40).map(|_| (b'0' + (next() % 10) as u8) as char).collect();
            let point = next() as usize % (digits.len() + 1);
            texts.push(format!("{}.{}e{}", &digits[..point], &digits[point..], (next() % 700) as i64 - 350));
            // A whole number exactly between two floats, a hair over it,
            // and one under it: floats from 2^53 are 2 apart or more.
            let low = ((1u128 << 52) | (next() as u128 >> 12)) << (1 + next() % 60);
            let between = low + (1u128 << low.trailing_zeros().min(127)) / 2;
            if i % 4 == 0 {
                let exact = (low as f64) as u128 == low;
                assert!(exact);
                let gap = 1u128 << ((127 - low.leading_zeros()) - 52);
                let between = low + gap / 2;
                texts.push(between.to_string());
                texts.push(format!("{between}.{}1", "0".repeat(next() as usize % 30)));
                texts.push((between - 1).to_string());
            }
            let _ = between;
            // Half of one of the smallest floats but one: exactly between
            // two of them, in some 750 digits.
            if i % 40 == 0 {
                let odd = f64::from_bits((next() >> (12 + next() % 52)) | 1);
                let exact = format!("{odd:.1080e}");
                let (digits, power) = exact.split_once('e').expect("an exponent");
                let mut left = 0;
                let halved: String = digits
                    .bytes()
                    .filter(u8::is_ascii_digit)
                    .map(|digit| {
                        let now = left * 10 + (digit - b'0') as u32;
                        left = now % 2;
                        (b'0' + (now / 2) as u8) as char
                    })
                    .collect();
                assert_eq!(left, 0);
                let end = halved.trim_end_matches('0').len();
                texts.push(format!("0.{}e{power}", &halved[..end]));
                texts.push(format!("0.{}1e{power}", &halved[..end]));
                texts.push(format!("0.{}e{power}", &halved[..end - 1]));
                texts.push(format!("0.{}{}e{power}", &halved[..end], "0".repeat(900)));
            }
        }
        let reads = "while true {\n    let line = input()\n    if line is none {\n        break\n    }\n    try {\n        print(float(line))\n    } except ValueError {\n        print(\"no\")\n    }\n}\n";
        let typed: String = texts.iter().map(|text| format!("{text}\n")).collect();
        let wanted: String = texts
            .iter()
            .map(|text| match text.trim_matches([' ', '\t']).parse::<f64>() {
                Ok(x) => format!("{}\n", crate::value::float_text(x)),
                Err(_) => "no\n".to_string(),
            })
            .collect();
        assert_eq!(wanted.matches("no\n").count(), 20);
        same_floats("floats-read", reads, &typed, &wanted);
    }

    /// `source` reads what is typed a line at a time: it prints `wanted`
    /// under `zeno run`, and built, with and without `--firmware`.
    fn same_floats(name: &str, source: &str, typed: &str, wanted: &str) {
        crate::value::testing::with_outside(typed, &[], || {
            let ran = interpreted(source);
            assert!(ran == wanted, "zeno run: {}", first_difference(&ran, wanted));
            for firmware in [false, true] {
                let c = if firmware { compile_firmware(source) } else { compile(source) }.expect("it builds");
                let name = format!("{name}-{firmware}");
                let Some((printed, errors, finished)) = build_and_run(&name, &c, &["-O1"]) else { return };
                assert!(finished, "{errors}");
                assert!(printed == wanted, "built (firmware: {firmware}): {}", first_difference(&printed, wanted));
            }
        });
    }

    /// The same on the chip (decision 53.7): a 32-bit processor with no
    /// floating point of its own, and on `boards/lm3s6965evb` no C
    /// library under the digits. QEMU emulates it. Floats, f32s and
    /// texts are typed at its serial port, and it prints what `zeno run`
    /// prints, in the build that checks and in the minimal one.
    #[test]
    fn floats_are_written_and_read_on_a_cortex_m3() {
        let source = r#"var narrow = false
while true {
    let line = input()
    if line is none {
        break
    }
    if line == "end" {
        break
    }
    if line == "f32" {
        narrow = true
        continue
    }
    try {
        let x = float(line)
        if narrow {
            let y = f32(x)
            print(y, f"{y:.3f}")
        } else if abs(x) < 1e15 {
            print(x, f"{x:.0f} {x:.2f} {x:.9f}")
        } else {
            # A build for a board has 1,024 bytes for the text of one
            # statement, and the largest float has 309 digits.
            print(x)
        }
    } except ValueError {
        print("no")
    }
}
"#;
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut typed: Vec<String> = [
            "0", "-0", "0.1", "0.30000000000000004", "5.684341886080802e-14", "1000000000000000.25", "5e-324", "1.7976931348623157e308",
            "2.2250738585072014e-308", "1e16", "0.0001", "9.999999999999999e-5", "0.125", "0.375", "9.995", "1e23", "inf", "-Infinity",
            "nan", " 2.5 ", "+.5e1", "1.", "1e400", "1e-400", "9007199254740993", "2.4703282292062327e-324", "2.4703282292062328e-324",
            "", ".", "1e", "1_0", "0x10", "nan(1)", "1.2.3",
        ]
        .iter()
        .map(|text| text.to_string())
        .collect();
        // Some 750 digits each: the two smallest floats but one, exactly.
        typed.push(format!("{:.760e}", f64::from_bits(1)));
        typed.push(format!("{:.760e}", f64::from_bits(3)));
        for i in 0..240 {
            let bits = next();
            let x = match i % 6 {
                0 => f64::from_bits(bits & 0x7FEF_FFFF_FFFF_FFFF),
                1 => (bits % 2_000_000) as f64 / 1000.0 - 1000.0,
                2 => f64::from_bits((bits & 0x800F_FFFF_FFFF_FFFF) | ((963 + next() % 120) << 52)),
                3 => f64::from_bits(bits & 0x800F_FFFF_FFFF_FFFF),
                4 => (bits >> (next() % 64)) as f64,
                _ => f64::from_bits(bits & 0x7FE0_0000_0000_0000),
            };
            typed.push(match i % 5 {
                0 => format!("{:.*e}", (next() % 25) as usize, x),
                _ => format!("{x:e}"),
            });
        }
        typed.push("f32".to_string());
        for i in 0..120 {
            let x = match i % 3 {
                0 => f32::from_bits((next() as u32) & 0xFF7F_FFFF),
                1 => (next() % 200_000) as f32 / 100.0 - 1000.0,
                _ => f32::from_bits((next() as u32) & 0x7F00_0000),
            };
            typed.push(format!("{:e}", x as f64));
        }
        typed.push("end\n".to_string());
        crate::value::testing::with_outside(&typed.join("\n"), &[], || {
            let wanted = interpreted(source);
            assert!(wanted.contains("5.684341886080802e-14 0 0.00 0.000000000\n") && wanted.contains("-inf\nNaN\n2.5 2 2.50 2.500000000\n") && wanted.contains(&"no\n".repeat(7)), "{wanted}");
            assert!(wanted.contains("\n5e-324 0 0.00 0.000000000\n1.5e-323 0 0.00 0.000000000\n"), "{wanted}");
            assert_eq!(wanted.lines().count(), typed.len() - 2);
            let checked = compile_no_collector(source).unwrap();
            let program = crate::modules::testing::built(source);
            let (_, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
            for (which, header) in [("lm3s6965evb", true), ("qemu-cortex-m3", false)] {
                let Some((printed, errors, finished)) = on_the_board(which, &format!("floats-{which}"), &checked, &["-O2"]) else {
                    return;
                };
                assert!(finished, "{which}: {errors}");
                assert!(printed == wanted, "{which}:\n{}", first_difference(&printed, &wanted));
                let options = Options { minimal: true, board_prints: true, board_header: header, ..Default::default() };
                let minimal = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
                let (printed, errors, finished) = on_the_board(which, &format!("floats-minimal-{which}"), &minimal, &["-Os"]).expect("as above");
                assert!(finished, "{which}, minimal: {errors}");
                assert!(printed == wanted, "{which}, minimal:\n{}", first_difference(&printed, &wanted));
            }
        });
    }

    /// The first line where what was printed is not what was wanted.
    fn first_difference(printed: &str, wanted: &str) -> String {
        match printed.lines().zip(wanted.lines()).position(|(got, want)| got != want) {
            Some(at) => format!(
                "line {}: printed `{}`, wanted `{}`",
                at + 1,
                printed.lines().nth(at).unwrap_or_default(),
                wanted.lines().nth(at).unwrap_or_default()
            ),
            None => format!("{} lines printed, {} wanted", printed.lines().count(), wanted.lines().count()),
        }
    }

    // ------------------------------------------------ Plain C (decision 55)

    /// The C of the runtime with its comments taken out.
    fn without_comments(c: &str) -> String {
        let mut out = String::new();
        let mut rest = c;
        while let Some(at) = rest.find("/*") {
            out += &rest[..at];
            rest = rest[at..].find("*/").map_or("", |end| &rest[at + end + 2..]);
        }
        out + rest
    }

    /// Told to be plain C (`ZENO_PLAIN_C`), the runtime asks GCC and
    /// Clang for nothing by their own names: no `__builtin_`, and no
    /// `__attribute__` but the one that only Windows reads. And the
    /// program after it never does.
    #[test]
    fn plain_c_asks_gcc_for_nothing() {
        let plain = without_comments(&crate::minimal::resolved(RUNTIME, &[("ZENO_PLAIN_C", true)]));
        assert!(!plain.contains("__builtin_"), "a `__builtin_` outside `#ifndef ZENO_PLAIN_C`");
        let asked: Vec<&str> = plain.lines().filter(|line| line.contains("__attribute__")).collect();
        assert_eq!(asked, ["__attribute__((constructor)) static void zeno_plain_line_ends(void) {"]);
        // With the other half kept, each name is there: this test would
        // pass for a runtime with no marks in it at all.
        let named = without_comments(&crate::minimal::resolved(RUNTIME, &[("ZENO_PLAIN_C", false)]));
        for name in ["__builtin_add_overflow", "__builtin_sub_overflow", "__builtin_mul_overflow", "__builtin_ctzll", "__builtin_unwind_init"] {
            assert!(named.contains(name), "{name}");
        }
        // It is decided in one place.
        assert_eq!(RUNTIME.matches("#define ZENO_PLAIN_C").count(), 1);
        assert!(RUNTIME.contains("#if !defined(ZENO_PLAIN_C) && !defined(__clang__) && !(defined(__GNUC__) && __GNUC__ >= 5)\n#define ZENO_PLAIN_C\n#endif\n"));

        let source = "let xs = [1, 2, 3]\nvar total = 0\nfor x in xs {\n    total += x * x\n}\nprint(total, 2 ** 10, f\"{total:x}\")\n";
        for c in [compile(source).unwrap(), compile_no_collector(source).unwrap(), compile_minimal(source, Build::Ordinary, false)] {
            let program = &c[c.rfind("static void zeno_program(void)").or(c.rfind("int main(")).expect("the program")..];
            assert!(!program.contains("__"), "{program}");
        }

        // A firmware build has no `setjmp`, which is what reads the
        // registers for the collector in plain C. So a firmware build
        // with a collector says, to a compiler that is neither GCC nor
        // Clang, that it cannot be built (55.5). No other build says it.
        let refusal = "#error \"The collector of a firmware build reads the processor's registers in a way that only GCC and Clang have.";
        let firmware = compile_firmware(source).unwrap();
        let said = firmware.find(refusal).expect("a firmware build with a collector says so");
        let before = &firmware[..said];
        assert!(before.rfind("#ifdef ZENO_PLAIN_C\n").expect("only in plain C") > before.rfind("#endif").expect("an #endif before it"));
        assert!(!firmware.contains("setjmp"));
        for c in [compile(source).unwrap(), compile_no_collector(source).unwrap()] {
            assert!(!c.contains("#error \"The collector"), "{c}");
        }
    }

    /// The functions that say whether a sum, a difference or a product
    /// overflowed, as the runtime has them, written out twice in one C
    /// file: once with the built-ins of GCC and Clang (their names end
    /// in `_b`) and once in plain C (`_p`). Then a `main` that holds the
    /// second to the first: on every pair of the numbers at the edges
    /// (each power of two, one less, one more, and each of those below
    /// zero), and on `rounds` pairs of every length, with the products
    /// that just fit and the ones that just do not.
    fn overflow_harness(rounds: u32) -> String {
        let from = RUNTIME.find("/* a + b. Added as numbers with no sign").expect("the four functions begin");
        let to = RUNTIME.find("static int64_t zeno_add(int64_t a, int64_t b, int line, int col)").expect("and end");
        let low = RUNTIME.find("/* Which bit of a word is its lowest one").expect("the lowest bit begins");
        let low_end = RUNTIME.find("/* Look inside every old block of a page").expect("and ends");
        let functions = format!("{}{}", &RUNTIME[from..to], &RUNTIME[low..low_end]);
        let named = |ending: &str| {
            let mut text = functions.clone();
            for name in ["zeno_add_overflows", "zeno_sub_overflows", "zeno_mul_overflows_u64", "zeno_mul_overflows", "zeno_lowest_bit"] {
                text = text.replace(&format!("{name}("), &format!("{name}{ending}("));
            }
            text
        };
        let mut c = "#include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n".to_string();
        c += &named("_b");
        c += "#define ZENO_PLAIN_C\n";
        c += &named("_p");
        c += &r#"
static uint64_t state = UINT64_C(0x9E3779B97F4A7C15);
static uint64_t next(void) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    return state;
}
static unsigned long pairs = 0, wrong = 0, over = 0;
static void differs(const char *what, uint64_t a, uint64_t b) {
    if (wrong++ < 10) printf("%s differs for %08lx%08lx and %08lx%08lx\n", what, (unsigned long)(a >> 32), (unsigned long)(a & 0xFFFFFFFFu), (unsigned long)(b >> 32), (unsigned long)(b & 0xFFFFFFFFu));
}
static void pair(uint64_t ua, uint64_t ub) {
    /* Each is told a different thing to begin with, so that an answer
     * neither wrote is not taken for one both gave. */
    int64_t a, b, x = 1, y = 2;
    uint64_t ux = 1, uy = 2;
    bool p, q;
    memcpy(&a, &ua, sizeof a);
    memcpy(&b, &ub, sizeof b);
    pairs++;
    p = zeno_add_overflows_b(a, b, &x), q = zeno_add_overflows_p(a, b, &y);
    if (p != q || (!p && x != y)) differs("+", ua, ub);
    over += p;
    p = zeno_sub_overflows_b(a, b, &x), q = zeno_sub_overflows_p(a, b, &y);
    if (p != q || (!p && x != y)) differs("-", ua, ub);
    over += p;
    p = zeno_mul_overflows_b(a, b, &x), q = zeno_mul_overflows_p(a, b, &y);
    if (p != q || (!p && x != y)) differs("*", ua, ub);
    over += p;
    p = zeno_mul_overflows_u64_b(ua, ub, &ux), q = zeno_mul_overflows_u64_p(ua, ub, &uy);
    if (p != q || (!p && ux != uy)) differs("* of u64", ua, ub);
    over += p;
    if (ua != 0 && zeno_lowest_bit_b(ua) != zeno_lowest_bit_p(ua)) differs("the lowest bit", ua, ua);
}
int main(void) {
    static uint64_t edges[384];
    int n = 0;
    for (int k = 0; k < 64; k++) {
        for (int d = 0; d < 3; d++) {
            uint64_t edge = ((uint64_t)1 << k) + (uint64_t)d - 1;
            edges[n++] = edge;
            edges[n++] = 0 - edge;
        }
    }
    for (int i = 0; i < n; i++) {
        for (int j = 0; j < n; j++) pair(edges[i], edges[j]);
    }
    for (long round = 0; round < ROUNDS; round++) {
        int wide_a = (int)(next() % 65), wide_b = (int)(next() % 65);
        uint64_t a = wide_a == 0 ? 0 : next() >> (64 - wide_a), b = wide_b == 0 ? 0 : next() >> (64 - wide_b);
        if (next() & 1) a = 0 - a;
        if (next() & 1) b = 0 - b;
        pair(a, b);
        pair(a, edges[next() % (unsigned)n]);
        if (a != 0) {
            /* What the biggest int and the biggest u64 hold of a, and one either side. */
            uint64_t size = a >> 63 ? 0 - a : a;
            uint64_t fits = (uint64_t)INT64_MAX / size, fits_u64 = UINT64_MAX / a;
            pair(a, fits), pair(a, fits + 1), pair(a, fits - 1), pair(a, 0 - fits), pair(a, 0 - fits - 1), pair(a, 1 - fits);
            pair(a, fits_u64), pair(a, fits_u64 + 1);
        }
    }
    printf("%lu pairs, %lu wrong, %s\n", pairs, wrong, over > pairs / 4 ? "many of them too big" : "few of them too big");
    return 0;
}
"#
        .replace("ROUNDS", &rounds.to_string());
        c.replace("#include <stdio.h>\n", "#include <stdio.h>\n#include <string.h>\n")
    }

    /// The plain C says of every pair what the built-ins say: whether
    /// the result fits, and what it is. Read as the C of 1999 with
    /// nothing added, and with what the compiler knows to be undefined
    /// stopped as it happens, where the compiler can do that.
    #[test]
    fn plain_c_finds_an_overflow_where_the_built_ins_do() {
        let c = overflow_harness(2_000_000);
        let Some((printed, errors, finished)) = build_and_run("plain-harness", &c, &["-O2", "-std=c99", "-pedantic-errors", "-Wall", "-Werror"]) else {
            return;
        };
        assert!(finished, "{errors}");
        // The harness prints with `printf`, which ends a line with two
        // bytes on Windows.
        assert_eq!(printed.replace("\r\n", "\n"), "19655520 pairs, 0 wrong, many of them too big\n");
        // Unoptimized, which is how a compiler with none of this built
        // in sees it.
        let (printed, _, _) = build_and_run("plain-harness-0", &overflow_harness(100_000), &["-O0"]).expect("as above");
        assert_eq!(printed.replace("\r\n", "\n"), "1123240 pairs, 0 wrong, many of them too big\n");
    }

    /// The same on the chip: a processor of 32 bits, where a number of
    /// 64 is two words and every step of the plain C is the compiler's
    /// own code for them.
    #[test]
    fn plain_c_finds_an_overflow_on_a_cortex_m3() {
        for flag in ["-O2", "-Os"] {
            let Some((printed, errors, _)) = on_the_board("qemu-cortex-m3", &format!("plain-harness{flag}"), &overflow_harness(20_000), &[flag]) else {
                return;
            };
            assert_eq!(printed, "342472 pairs, 0 wrong, many of them too big\n", "{flag}: {errors}");
        }
    }

    /// A program that adds, takes away and multiplies the numbers it is
    /// typed, ints and u64s, and reads ints from text. It says `over`
    /// where the result does not fit.
    const SUMS: &str = r#"function tried(a: int, b: int) -> str {
    var line = ""
    try {
        line += f"{a + b}"
    } except OverflowError {
        line += "over"
    }
    try {
        line += f" {a - b}"
    } except OverflowError {
        line += " over"
    }
    try {
        line += f" {a * b}"
    } except OverflowError {
        line += " over"
    }
    try {
        line += f" {-a} {abs(b)}"
    } except OverflowError {
        line += " over"
    }
    return line
}

function tried_u64(a: u64, b: u64) -> str {
    var line = ""
    try {
        line += f"{a + b}"
    } except OverflowError {
        line += "over"
    }
    try {
        line += f" {a - b}"
    } except OverflowError {
        line += " over"
    }
    try {
        line += f" {a * b}"
    } except OverflowError {
        line += " over"
    }
    return line
}

function typed() -> int raises ValueError {
    let line = input()
    if line is none {
        return 0
    }
    return int(line)
}

var kind = "int"
while true {
    let first = input()
    if first is none {
        break
    }
    if first == "end" {
        break
    }
    if first == "u64" or first == "read" {
        kind = first
        continue
    }
    try {
        if kind == "u64" {
            let a = (u64(int(first)) << 32) | u64(typed())
            let b = (u64(typed()) << 32) | u64(typed())
            print(tried_u64(a, b))
        } else if kind == "read" {
            print(int(first))
        } else {
            let a = int(first)
            print(tried(a, typed()))
        }
    } except ValueError {
        print("no")
    }
}
"#;

    /// What `SUMS` is typed, and what it then prints, worked out here
    /// with Rust's own checked arithmetic: `pairs` pairs of ints, as
    /// many of u64s, and texts to read.
    fn sums_typed(pairs: usize) -> (String, String) {
        let mut seed: u64 = 0xD1B5_4A32_D192_ED03;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut typed, mut wanted) = (String::new(), String::new());
        let said = |result: Option<String>| result.unwrap_or_else(|| "over".to_string());
        let edges = [0, 1, -1, 2, 3, i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1, 1 << 31, 1 << 32, (1 << 32) - 1, 3_037_000_499, 3_037_000_500, -3_037_000_500, 1 << 62, -(1 << 62)];
        let mut ints: Vec<(i64, i64)> = Vec::new();
        for a in edges {
            for b in edges {
                ints.push((a, b));
            }
        }
        while ints.len() < pairs {
            // Numbers of every length, and the ones whose product only
            // just fits, or only just does not.
            let a = ((next() >> (next() % 64)) as i64).wrapping_mul(if next() % 2 == 0 { 1 } else { -1 });
            let b = match (next() % 4, a) {
                (0, _) | (_, 0) => (next() >> (next() % 64)) as i64,
                (1, _) => i64::MAX / a,
                (2, _) => (i64::MAX / a).wrapping_add(if a < 0 { -1 } else { 1 }),
                _ => i64::MAX.wrapping_sub(a.wrapping_abs().max(0)).wrapping_add((next() % 3) as i64 - 1),
            };
            ints.push((a, b));
        }
        for (a, b) in ints {
            typed += &format!("{a}\n{b}\n");
            // `-a` and `abs(b)` are in one piece of text: nothing of
            // it is written when either is too big.
            let sign = match (a.checked_neg(), b.checked_abs()) {
                (Some(minus), Some(size)) => format!("{minus} {size}"),
                _ => "over".to_string(),
            };
            wanted += &format!(
                "{} {} {} {sign}\n",
                said(a.checked_add(b).map(|n| n.to_string())),
                said(a.checked_sub(b).map(|n| n.to_string())),
                said(a.checked_mul(b).map(|n| n.to_string()))
            );
        }
        typed += "u64\n";
        for round in 0..pairs {
            let a = if round % 7 == 0 { u64::MAX - next() % 3 } else { next() >> (next() % 64) };
            let b = match (round % 4, a) {
                (0, _) | (_, 0) => next() >> (next() % 64),
                (1, _) => u64::MAX / a,
                (2, _) => (u64::MAX / a).wrapping_add(1),
                _ => (u64::MAX - a).wrapping_add(next() % 3),
            };
            typed += &format!("{}\n{}\n{}\n{}\n", a >> 32, a & 0xFFFF_FFFF, b >> 32, b & 0xFFFF_FFFF);
            wanted += &format!(
                "{} {} {}\n",
                said(a.checked_add(b).map(|n| n.to_string())),
                said(a.checked_sub(b).map(|n| n.to_string())),
                said(a.checked_mul(b).map(|n| n.to_string()))
            );
        }
        typed += "read\n";
        for (text, read) in [
            ("9223372036854775807", "9223372036854775807"),
            ("9223372036854775808", "no"),
            ("-9223372036854775808", "-9223372036854775808"),
            ("-9223372036854775809", "no"),
            ("92233720368547758070", "no"),
            ("-92233720368547758080", "no"),
            (" 1_000 ", "1000"),
            ("+7", "7"),
            ("99999999999999999999999999", "no"),
            ("-0", "0"),
        ] {
            typed += &format!("{text}\n");
            wanted += &format!("{read}\n");
        }
        typed += "end\n";
        (typed, wanted)
    }

    /// Built as it is and built as plain C, a program finds every
    /// overflow that `zeno run` finds and prints every result it
    /// prints. With the collector run before every piece of memory is
    /// handed out, which in plain C reads the registers with `setjmp`.
    /// And with tcc, a C compiler that is neither GCC nor Clang, where
    /// there is one.
    #[test]
    fn a_program_overflows_in_plain_c_where_it_does_under_zeno_run() {
        let (typed, wanted) = sums_typed(3000);
        assert!(wanted.matches("over").count() > 3000 && wanted.contains("\nover -9223372036854775807 over over\n"), "{wanted}");
        crate::value::testing::with_outside(&typed, &[], || {
            let ran = interpreted(SUMS);
            assert!(ran == wanted, "zeno run: {}", first_difference(&ran, &wanted));
            let c = compile(SUMS).expect("it builds");
            // Rather than the frame pointer left out: see decision 55.5.
            let ways: [&[&str]; 4] = [
                &["-O2"],
                &["-O2", "-DZENO_PLAIN_C"],
                &["-O0", "-DZENO_PLAIN_C"],
                &["-O2", "-DZENO_PLAIN_C", "-DZENO_COLLECT_ALWAYS", "-fno-omit-frame-pointer"],
            ];
            for (at, flags) in ways.iter().enumerate() {
                let Some((printed, errors, finished)) = build_and_run(&format!("plain-sums-{at}"), &c, flags) else { return };
                assert!(finished, "{flags:?}: {errors}");
                assert!(printed == wanted, "{flags:?}: {}", first_difference(&printed, &wanted));
            }
            if Command::new("tcc").arg("-v").output().is_err() {
                eprintln!("no `tcc` here; skipping the build with a compiler that is neither GCC nor Clang");
                return;
            }
            for flags in [&[][..], &["-DZENO_COLLECT_ALWAYS"][..]] {
                let dir = std::env::temp_dir().join(format!("zeno-test-{}-plain-tcc-{}", std::process::id(), flags.len()));
                std::fs::create_dir_all(&dir).expect("a temporary folder");
                let (c_file, binary) = (dir.join("test.c"), dir.join("test"));
                std::fs::write(&c_file, &c).expect("the C file is written");
                let built = Command::new("tcc").args(flags).arg("-w").arg("-o").arg(&binary).arg(&c_file).args(LIBRARIES).output().expect("tcc runs");
                assert!(built.status.success(), "tcc: {}", String::from_utf8_lossy(&built.stderr));
                let ran = crate::value::testing::run(&mut Command::new(&binary)).expect("the program runs");
                let _ = std::fs::remove_dir_all(&dir);
                let printed = String::from_utf8_lossy(&ran.stdout);
                assert!(ran.status.success() && printed == wanted, "tcc {flags:?}: {}", first_difference(&printed, &wanted));
            }
        });
    }

    /// On the chip, built as plain C: sums, differences and products
    /// that only just fit are printed as `zeno run` prints them, and
    /// the first that does not fit stops the program and says so (a
    /// build for a board cannot catch an overflow).
    #[test]
    fn a_program_overflows_in_plain_c_on_a_cortex_m3() {
        let source = r#"function typed() -> int raises ValueError {
    let line = input()
    if line is none {
        return 0
    }
    return int(line)
}

var kind = "int"
while true {
    let first = input()
    if first is none {
        break
    }
    if first == "end" {
        break
    }
    if first == "u64" {
        kind = first
        continue
    }
    if kind == "u64" {
        let a = (u64(int(first)) << 32) | u64(typed())
        let b = (u64(typed()) << 32) | u64(typed())
        print(a + b, a * b)
    } else {
        let a = int(first)
        let b = typed()
        print(a + b, a - b, a * b, -a, abs(b))
    }
}
print("done")
"#;
        let mut seed: u64 = 0x94D0_49BB_1331_11EB;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Pairs of which every result fits, many of them only just.
        let (mut typed, mut wanted) = (String::new(), String::new());
        let mut pairs = 0;
        while pairs < 300 {
            let a = ((next() >> (next() % 64)) as i64).wrapping_mul(if next() % 2 == 0 { 1 } else { -1 });
            let b = match (next() % 3, a) {
                (0, _) | (_, 0) => (next() >> (32 + next() % 32)) as i64,
                (1, _) => i64::MAX / a,
                _ => i64::MIN / a.wrapping_abs().max(1),
            };
            let (Some(sum), Some(less), Some(times), Some(minus), Some(size)) = (a.checked_add(b), a.checked_sub(b), a.checked_mul(b), a.checked_neg(), b.checked_abs()) else {
                continue;
            };
            typed += &format!("{a}\n{b}\n");
            wanted += &format!("{sum} {less} {times} {minus} {size}\n");
            pairs += 1;
        }
        typed += &format!("{}\n1\n1\n0\n{}\n-2\n", i64::MAX - 1, 1_i64 << 62);
        wanted += &format!("{} {} {} {} 1\n1 1 0 -1 0\n", i64::MAX, i64::MAX - 2, i64::MAX - 1, 1 - i64::MAX);
        wanted += &format!("{} {} {} {} 2\n", (1_i64 << 62) - 2, (1_i64 << 62) + 2, i64::MIN, -(1_i64 << 62));
        let mut u64s = String::from("u64\n");
        let mut wanted_u64s = String::new();
        pairs = 0;
        while pairs < 100 {
            let a = next() >> (next() % 64);
            let b = if pairs % 2 == 0 { u64::MAX / a.max(1) } else { next() >> (32 + next() % 32) };
            let (Some(sum), Some(times)) = (a.checked_add(b), a.checked_mul(b)) else { continue };
            u64s += &format!("{}\n{}\n{}\n{}\n", a >> 32, a & 0xFFFF_FFFF, b >> 32, b & 0xFFFF_FFFF);
            wanted_u64s += &format!("{sum} {times}\n");
            pairs += 1;
        }
        // Where in the program each thing is worked out: an error says
        // the line and the column of the one that was too big.
        let place = |line: &str, piece: &str| {
            let at = source.lines().position(|found| found.trim() == line).expect("the line is in the program");
            format!("{}:{}", at + 1, source.lines().nth(at).unwrap_or_default().find(piece).expect("the piece is in the line") + 1)
        };
        let ints = "print(a + b, a - b, a * b, -a, abs(b))";
        let wide = "print(a + b, a * b)";
        let int_too_big = |piece: &str| format!("error: uncaught OverflowError: the result is too big for an int\n --> test.zeno:{}\n", place(ints, piece));
        let u64_too_big = |piece: &str| format!("error: uncaught OverflowError: the result is too big for a u64\n --> test.zeno:{}\n", place(wide, piece));
        // Each way of ending: every result fits, or the last pair is
        // one too many for one of the signs. (Nothing of a line is
        // printed when one of its numbers is too big.)
        let endings = [
            (format!("{u64s}end\n"), format!("{wanted_u64s}done\n"), String::new()),
            (format!("{}\n1\n", i64::MAX), String::new(), int_too_big("a + b")),
            (format!("{}\n-1\n", i64::MIN), String::new(), int_too_big("a + b")),
            (format!("{}\n1\n", i64::MIN), String::new(), int_too_big("a - b")),
            (format!("-2\n{}\n", i64::MAX), String::new(), int_too_big("a - b")),
            ("3037000500\n3037000500\n".to_string(), String::new(), int_too_big("a * b")),
            (format!("{}\n2\n", 1_i64 << 62), String::new(), int_too_big("a * b")),
            (format!("{}\n-2\n", (1_i64 << 62) + 1), String::new(), int_too_big("a * b")),
            (format!("{}\n0\n", i64::MIN), String::new(), int_too_big("-a")),
            (format!("{u64s}4294967295\n4294967295\n0\n1\n"), wanted_u64s.clone(), u64_too_big("a + b")),
            (format!("{u64s}1\n0\n1\n0\n"), wanted_u64s.clone(), u64_too_big("a * b")),
        ];
        let checked = compile_no_collector(source).unwrap();
        for (at, (last, printed_last, said)) in endings.iter().enumerate() {
            // The first board has no C library under its `print`, and
            // takes every ending. The other takes two.
            for which in ["lm3s6965evb", "qemu-cortex-m3"] {
                if which == "qemu-cortex-m3" && at > 1 {
                    continue;
                }
                crate::value::testing::with_outside(&format!("{typed}{last}"), &[], || {
                    let name = format!("plain-sums-{which}-{at}");
                    let Some((printed, errors, finished)) = on_the_board(which, &name, &checked, &["-O2", "-DZENO_PLAIN_C"]) else {
                        return;
                    };
                    let wanted = format!("{wanted}{printed_last}");
                    assert!(printed == wanted, "{which}, ending {at}: {}\n{errors}", first_difference(&printed, &wanted));
                    assert_eq!(finished, said.is_empty(), "{which}, ending {at}: {errors}");
                    assert_eq!(&errors, said, "{which}, ending {at}");
                });
            }
        }
    }

    /// The board in `boards/lm3s6965evb` is the same chip with a header
    /// of its own: pins that are registers of the chip, its SysTick
    /// clock, its UARTs, and `print` through UART0 with no `printf`
    /// under it. QEMU emulates the chip, so this runs with no hardware.
    #[test]
    fn a_program_prints_through_the_registers_of_a_cortex_m3() {
        let (folder, board) = board_named("lm3s6965evb");
        assert_eq!(board.missing(&folder), Vec::<String>::new());
        assert_eq!(board.header.as_deref(), Some("board.h"));
        assert_eq!(board.sources, ["startup.c"]);
        let source = "from board import led, pin, input_pin, pull_up, serial\n\
                      from time import sleep, now\n\
                      import program\n\
                      \n\
                      function half(n: int) -> int {\n\
                          return 100 // n\n\
                      }\n\
                      \n\
                      let before = now()\n\
                      led.on()\n\
                      print(led, led.is_on(), pin(40).is_on(), pin(3).is_on())\n\
                      led.off()\n\
                      pin(3).on()\n\
                      print(led.is_on(), pin(40).is_on(), pin(3).is_on(), pin(4).is_on(), pin(55).is_on())\n\
                      let button = input_pin(41, pull_up)\n\
                      print(button.number, pin(3).is_on())\n\
                      sleep(20 ms)\n\
                      print(now() - before >= 20 ms, now() - before < 200 ms)\n\
                      let port = serial(0, 115200 baud)\n\
                      port.write(\"over the serial port\\n\")\n\
                      let big: u64 = 18446744073709551615\n\
                      print(2 ** 40, -7 // 2, big, 0.1 + 0.2, 1e300 * 10.0, true, [1, 2, 3], \"héllo\".upper())\n\
                      program.warn(\"careful\")\n\
                      let name = input(\"name? \")\n\
                      print(f\"hi {name}, {half(4)} is a quarter\")\n";
        let wanted = "Pin(13) true true false\n\
                      false false true false false\n\
                      41 true\n\
                      true true\n\
                      over the serial port\n\
                      1099511627776 -4 18446744073709551615 0.30000000000000004 1e301 true [1, 2, 3] HéLLO\n\
                      name? hi Ada, 25 is a quarter\n";
        crate::value::testing::with_outside("Ada\n", &[], || {
            let checked = compile_no_collector(&format!("{source}print(half(0))\n")).unwrap();
            let Some((printed, errors, finished)) = on_the_board("lm3s6965evb", "registers", &checked, &["-O2"]) else {
                return;
            };
            assert_eq!(printed, wanted);
            assert!(!finished);
            assert_eq!(errors, "careful\nerror: uncaught ZeroDivisionError: division by zero\n --> test.zeno:6:8\n");
            // The least C, which checks nothing, so without the mistake.
            let program = crate::modules::testing::built(source);
            let (_, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
            let options = Options { minimal: true, board_prints: true, board_header: true, ..Default::default() };
            let minimal = emit_with(&program, &types, "test.zeno", Build::NoCollector, options).expect("it builds");
            let (printed, errors, finished) = on_the_board("lm3s6965evb", "registers-minimal", &minimal, &["-Os"]).expect("as above");
            assert_eq!((printed.as_str(), errors.as_str(), finished), (wanted, "careful\n", true));
        });
    }

    /// A function that is handed a port, in a program that makes none,
    /// still finds the runtime's part for serial ports.
    #[test]
    fn a_port_that_is_only_handed_over_builds() {
        let source = "function talk(to: Serial) {\n    to.write(\"hi\")\n    print(to.has_byte())\n}\n\
                      function level(of: AnalogPin) -> V {\n    return of.voltage()\n}\n\
                      function dim(light: PwmPin) {\n    light.duty(0.1)\n    light.pulse(1 ms)\n}\n\
                      function keep(beat: Ticker) -> int {\n    print(beat, beat.period)\n    beat.restart()\n    return beat.wait() + beat.ticks()\n}\n\
                      function turns(of: Interrupt) -> int {\n    print(of, of.number)\n    return of.edges() + of.wait()\n}\n\
                      print(1)\n";
        same_as_interpreter("handed-over", source);
    }

    /// Serial ports (decision 46). On a computer each is the terminal.
    const SERIAL: &str = "from board import serial\nimport board\n\
        let port = serial(1, 115200 baud)\nlet other = board.serial(0, 9600 baud)\n\
        print(port, other.number, port == other, f\"{port}\")\n\
        port.write(\"hello, \")\nport.write(\"world\\n\")\n\
        let frame: list[u8] = [0x48, 0x69, 0x0a]\nport.write_bytes(frame)\nport.write_bytes([0x4f, 0x4b, 0x0a])\n\
        function talk(to: Serial, text: str) {\n    to.write(text)\n}\ntalk(other, \"two\\n\")\n\
        let ports: list[Serial] = [port, other]\nprint(ports)\n\
        var line = \"\"\nvar b = port.read_byte()\n\
        while b is not none and b != 10 {\n    line = line + char(b)\n    b = port.read_byte()\n}\n\
        print(f\"got [{line}]\", port.has_byte())\n\
        var count = 0\nwhile port.has_byte() {\n    let byte = port.read_byte()\n    if byte is not none { count += byte }\n}\n\
        print(count, \"then\", port.read_byte(), port.has_byte())\n";

    #[test]
    fn serial_ports_match() {
        use crate::value::testing::with_outside;
        with_outside("ping\nab", &[], || {
            assert!(interpreted(SERIAL).ends_with("got [ping] true\n195 then none false\n"), "{}", interpreted(SERIAL));
            same_as_interpreter("serial", SERIAL);
            no_collector_as_interpreter("serial", SERIAL);
        });
        // Nothing typed: the port has ended before it began.
        assert!(interpreted(SERIAL).ends_with("got [] false\n0 then none false\n"));
        same_as_interpreter("serial-nothing", SERIAL);
        same_as_interpreter(
            "no-serial-port",
            "from board import serial\n\
             try {\n    serial(4, 9600 baud)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
             try {\n    serial(1, 0 baud)\n} except ValueError as error {\n    print(\"no:\", error.message)\n}\n\
             serial(-1, 9600 baud)\n",
        );
    }

    /// A board of its own whose serial port 1 is wired back to itself: what
    /// is sent comes back to be read, and when all of it has, no more can.
    #[test]
    fn a_serial_port_on_a_board_of_its_own() {
        let header = "#define ZENO_PINS 2\n\
                      #define ZENO_SERIALS 2\n\
                      static bool my_levels[ZENO_PINS];\n\
                      static uint8_t my_wire[64];\n\
                      static size_t my_sent = 0, my_read = 0;\n\
                      static bool zeno_board_read(int pin) { return my_levels[pin]; }\n\
                      static void zeno_board_write(int pin, bool level) { my_levels[pin] = level; }\n\
                      static void zeno_board_sleep(double seconds) { (void)seconds; }\n\
                      static double zeno_board_now(void) { return 0.0; }\n\
                      static void zeno_board_serial_open(int port, double baud) { printf(\"open %d %g\\n\", port, baud); }\n\
                      static void zeno_board_serial_write(int port, const uint8_t *bytes, size_t count) {\n\
                      \x20   printf(\"sent %d:\", port);\n\
                      \x20   for (size_t i = 0; i < count; i++) { printf(\" %02x\", bytes[i]); my_wire[my_sent++] = bytes[i]; }\n\
                      \x20   printf(\"\\n\");\n\
                      }\n\
                      static bool zeno_board_serial_ready(int port) { (void)port; return my_read < my_sent; }\n\
                      static int zeno_board_serial_read(int port) { (void)port; return my_read < my_sent ? my_wire[my_read++] : -1; }\n";
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-my-serial", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let file = dir.join("my_board.h");
        std::fs::write(&file, header).expect("the header is written");
        let flag = format!("-DZENO_BOARD=\"{}\"", file.display());
        let source = "from board import serial\nlet port = serial(1, 9600 baud)\nport.write(\"hi\\n\")\n\
                      port.write_bytes([1, 2, 255])\nvar got: list[int] = []\n\
                      while port.has_byte() {\n    let b = port.read_byte()\n    if b is not none { got.append(int(b)) }\n}\n\
                      print(got, port.read_byte())\n";
        for c in [compile(source).unwrap(), compile_firmware(source).unwrap(), compile_no_collector(source).unwrap()] {
            let Some((printed, errors, finished)) = build_and_run("serial-header", &c, &[&flag]) else {
                return;
            };
            assert!(finished, "{errors}");
            assert_eq!(printed, "open 1 9600\nsent 1: 68 69 0a\nsent 1: 01 02 ff\n[104, 105, 10, 1, 2, 255] none\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blink_is_three_lines_of_c_and_needs_no_collector() {
        let c = compile_no_collector(include_str!("../examples/blink.zeno")).unwrap();
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        let body = program.split("\n}\n").next().unwrap();
        assert_eq!(
            body,
            "    while (true) {\n        zeno_pin_toggle(ZENO_LED);\n        zeno_sleep((500.0 / 1000.0));\n    }"
        );
        no_collector_as_interpreter("signal", include_str!("../examples/signal.zeno"));
    }

    #[test]
    fn simple_code_stays_simple() {
        let c = compile("function area(w: float, h: float) -> float { return w * h }\nlet a = area(2.0, 3.0)").unwrap();
        assert!(c.contains("static double z_area(double z_w, double z_h) {\n    return (z_w * z_h);\n}"));
        assert!(c.contains("    z_a = z_area(2.0, 3.0);\n"));
    }

    // ------------------------------------------- Memory by hand (docs/unsafe.md)

    #[test]
    fn memory_by_hand_is_the_same_in_every_build() {
        // The ordinary build and `--firmware`, with the collector run
        // before every allocation of its own, and the build with no
        // collector, through C and through LLVM. What `allocate` gave
        // is all given back, so the pool ends with no blocks left.
        no_collector_as_interpreter("by-hand", BY_HAND);
        no_collector_as_interpreter("driver", include_str!("../examples/driver.zeno"));
        // The one check a compiled program does make: how many items.
        same_as_interpreter("no-items", "let count = 0\nlet p = allocate(u8, count)\nprint(p)");
    }

    #[test]
    fn the_memory_report_counts_what_was_made_by_hand() {
        let source = "let a = allocate(u8, 16)\nlet b = allocate(int, 4)\nlet c = allocate(float, 2)\n\
                      unsafe {\n    free(a)\n    free(c)\n}\nprint(b)";
        // Where there is a collector, memory made by hand is the heap's
        // and gets a line of its own.
        let c = compile(source).unwrap();
        let Some((printed, errors, finished)) = build_and_run("by-hand-report", &c, &["-O1", "-DZENO_MEMORY_REPORT"]) else {
            return;
        };
        assert!(finished && printed == "<pointer[int]>\n", "{printed}{errors}");
        assert!(errors.ends_with("by hand: 3 blocks from `allocate`, 1 never freed\n"), "{errors}");
        // A program that never calls `allocate` has no such line.
        let c = compile("print([1, 2])").unwrap();
        let (_, errors, _) = build_and_run("no-by-hand-report", &c, &["-O1", "-DZENO_MEMORY_REPORT"]).unwrap();
        assert!(!errors.contains("by hand"), "{errors}");
        // Where there is none, it comes from the pool, and a block that
        // was never freed is a block left at the end.
        let c = compile_no_collector(source).unwrap();
        let (printed, errors, finished) =
            build_and_run("by-hand-report-no-collector", &c, &["-O1", "-DZENO_MEMORY_REPORT"]).unwrap();
        assert!(finished && printed == "<pointer[int]>\n", "{printed}{errors}");
        assert!(errors.contains("3 blocks made (64 bytes)") && errors.contains("no collector, 1 block left at the end\n"), "{errors}");
        assert!(!errors.contains("by hand"), "{errors}");
    }

    #[test]
    fn the_c_for_memory_by_hand() {
        // docs/unsafe.md. A pointer is C's own pointer, `[ ]` is C's own
        // `[ ]`, and the block is a pair of braces.
        let c = compile(BY_HAND_C).unwrap();
        assert!(
            c.contains(
                "static int64_t z_sum(uint8_t *z2_bytes, int64_t z_count) {
    int64_t z_total = 0;
    {
        int64_t t5 = z_count;
        for (int64_t z_i = 0; z_i < t5; z_i++) {
            z_total = zeno_add(z_total, ((int64_t)z2_bytes[z_i]), 7, 13);
        }
    }
    return z_total;
}

static void z_set_bit(int64_t z_port, int64_t z_bit) {
    {
        int64_t t6 = zeno_register_read(z_port, 32);
        zeno_register_write(z_port, 32, zeno_fit((t6 | zeno_shl(1, z_bit, 15, 38)), INT64_C(0), INT64_C(4294967295), \"a u32\", 15, 20));
    }
}
"
            ),
            "{c}"
        );
        let program = c.split("static void zeno_program(void) {\n").nth(1).expect("the program");
        assert_eq!(
            program.split("\n}\n").next().unwrap(),
            "    z_bytes = ((uint8_t *)zeno_allocate(4, sizeof(uint8_t), 19, 13));
    z_words = ((int16_t *)zeno_allocate(2, sizeof(int16_t), 20, 13));
    {
        int64_t t1 = 200;
        z_bytes[0] = (uint8_t)t1;
        int64_t t2 = zeno_fit(zeno_add(((int64_t)z_bytes[0]), 1, 23, 19), INT64_C(0), INT64_C(255), \"a u8\", 23, 16);
        z_bytes[1] = (uint8_t)t2;
        int64_t t3 = -300;
        z_words[1] = (int16_t)t3;
        int64_t t4 = z_sum((z_bytes + 1), 3);
        zeno_print(2, zeno_str_int(t4), zeno_str_int(((int64_t)z_words[1])));
        zeno_release(z_words);
        zeno_release(z_bytes);
    }
    z_set_bit(1073872916, 5);"
        );
        // On a real board a register is one access the C compiler may
        // not remove or repeat, and a board may bring its own.
        let runtime = runtime(true, false);
        assert!(runtime.contains("    case 8: *(volatile uint8_t *)(uintptr_t)address = (uint8_t)value; break;\n"));
        assert!(runtime.contains("    default: return *(volatile uint32_t *)(uintptr_t)address;\n"));
        assert!(runtime.contains("#ifndef ZENO_BOARD_REGISTERS\n"));
    }

    #[test]
    fn leaving_from_inside_an_unsafe_block_frees_what_has_an_owner() {
        // `return`, `break` and `continue` inside the block leave it as
        // they leave any block: the lists and the object are freed, and
        // the C has no second `return` after the block.
        no_collector_as_interpreter("unsafe-exits", UNSAFE_EXITS);
        let c = compile_no_collector(UNSAFE_EXITS).unwrap();
        let first = c.split("static int64_t z_first(int64_t z_n) {\n").nth(1).expect("the function");
        let first = first.split("\n}\n").next().unwrap();
        assert!(
            first.ends_with(
                "    {
        zeno_release(z_box->z_bytes);
        int64_t t7 = -1;
        zeno_drop_Box(z_box);
        zeno_list_drop_text(z_names);
        return t7;
    }"
            ),
            "{first}"
        );
    }

    const UNSAFE_EXITS: &str = r##"class Box {
    items: list[int]
    bytes: pointer[u8]
}
function first(n: int) -> int {
    let box = Box([1, 2, 3], allocate(u8, 4))
    let names = ["a", "b"]
    for i in range(n) {
        let more = [i, i]
        unsafe {
            if i == 2 {
                free(box.bytes)
                return more[0] + len(names)
            }
            box.bytes[i] = u8(i)
            if i == 1 { continue }
        }
        unsafe { if i == 3 { break } }
    }
    unsafe {
        free(box.bytes)
        return -1
    }
}
print(first(5), first(0))
"##;

    pub(crate) const BY_HAND_C: &str = r##"from board import register

function sum(bytes: pointer[u8], count: int) -> int {
    var total = 0
    unsafe {
        for i in range(count) {
            total += bytes[i]
        }
    }
    return total
}

function set_bit(port: Register[u32], bit: int) {
    unsafe {
        port.write(u32(port.read() | 1 << bit))
    }
}

let bytes = allocate(u8, 4)
let words = allocate(i16, 2)
unsafe {
    bytes[0] = 200
    bytes[1] = u8(bytes[0] + 1)
    words[1] = -300
    print(sum(bytes + 1, 3), words[1])
    free(words)
    free(bytes)
}
set_bit(register(u32, 0x4002_0014), 5)
"##;

    const BY_HAND: &str = r##"# Memory by hand in every build: each kind of item, a pointer in a
# field, in a list and where `none` may be, and three registers.
import board

class Ring {
    data: pointer[u16]
    size: int
    head: int

    function push(self, value: u16) {
        unsafe {
            self.data[self.head] = value
        }
        self.head = (self.head + 1) % self.size
    }

    function total(self) -> int {
        var sum = 0
        unsafe {
            for i in range(self.size) {
                sum += self.data[i]
            }
        }
        return sum
    }
}

function find(bytes: pointer[u8], count: int, wanted: u8) -> pointer[u8] or none {
    for i in range(count) {
        let here = bytes + i
        unsafe {
            if here[0] == wanted {
                return here
            }
        }
    }
    return none
}

function fill(bytes: pointer[u8], count: int, step: int) {
    unsafe {
        for i in range(count) {
            bytes[i] = u8(i * step % 256)
        }
    }
}

function main() {
    let ring = Ring(allocate(u16, 4), 4, 0)
    for n in range(6) {
        ring.push(u16(n * 1000))
    }
    print(ring.total(), ring)

    let bytes = allocate(u8, 8)
    unsafe { print(bytes[0], bytes[7]) }
    fill(bytes, 8, 40)
    let found = find(bytes, 8, 120)
    if found is not none {
        print(found == bytes + 3, found == bytes, found - 3 == bytes)
        unsafe { print(found[0], found[1], (found - 1)[0]) }
    } else {
        print("not there")
    }
    if find(bytes, 8, 13) is none {
        print("13 is not there")
    }

    let blocks = [allocate(i32, 2), allocate(i32, 2)]
    let wide = allocate(int, 2)
    let small = allocate(i8, 1)
    unsafe {
        blocks[0][1] = -70000
        blocks[1][0] = 5
        wide[1] = -7000000000
        wide[1] += blocks[0][1] + blocks[1][0]
        small[0] = -128
        print(blocks[0][1] + blocks[1][0], wide[0], wide[1], small[0])
        for block in blocks {
            free(block)
        }
        free(wide)
        free(small)
    }

    let floats = allocate(float, 3)
    unsafe {
        floats[0] = 1.5
        floats[2] = floats[0] * 2.0
        print(floats[0], floats[1], floats[2])
        free(floats)
    }

    let mode = board.register(u32, 0x4002_0000)
    let speed = board.register(u16, 0x4002_0008)
    let data = board.register(u8, 0x4002_0014)
    unsafe {
        print(mode.read(), speed.read())
        mode.write(u32(mode.read() | 1 << 10))
        mode.write(u32(mode.read() | 1 << 31))
        speed.write(0xBEEF)
        data.write(0x55)
        data.write(0x55)
        mode.write(u32(mode.read() & ~(1 << 10)))
        print(mode.read(), speed.read(), data.read(), data.address)
        free(bytes)
        free(ring.data)
    }
    print(mode, bytes, mode == board.register(u32, 0x4002_0000))
}

main()
"##;
}
