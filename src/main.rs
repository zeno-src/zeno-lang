//! The `zeno` command-line tool.
//!
//! The compiler stages get added here one at a time. `zeno tokens`,
//! `zeno parse` and `zeno check` show what the lexer, the parser and the
//! type checker find. `zeno join` shows a program of several files as
//! the one program the later stages get. `zeno run` checks a program and then runs it, with
//! the interpreter or, given `--vm`, by compiling it to bytecode for the
//! VM. `zeno bytecode` shows that bytecode. `zeno build` compiles to C,
//! or with `--llvm` to LLVM IR.

// Built for a browser, the keyboard and a few more things that only
// `main` reaches are still compiled, and nothing calls them.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

mod ast;
mod board;
mod board_build;
mod bridge;
mod bridge_c;
mod builtins;
mod bytecode;
mod checker;
mod convert;
mod convert_c;
mod convert_c_lines;
mod convert_c_write;
mod convert_python;
mod convert_python_write;
mod deflate;
mod emit_bytecode;
mod emit_c;
mod emit_go;
mod emit_cpp;
mod emit_cs;
mod emit_dart;
mod emit_java;
mod emit_js;
mod emit_kotlin;
mod emit_lua;
mod emit_ruby;
mod emit_php;
mod emit_py;
mod emit_rust;
mod emit_llvm;
mod error;
mod format;
mod html;
mod inline;
mod interpreter;
mod json;
mod ints;
mod lengths;
mod lexer;
mod library;
mod lower;
mod lsp;
mod minimal;
mod modules;
mod ops;
mod package;
mod page;
mod parser;
mod printer;
mod registry;
mod test_build;
mod token;
mod units;
mod value;
mod vm;
#[cfg(target_arch = "wasm32")]
mod web_host;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Print what a command had to say, or what went wrong with it.
fn said(lines: Result<Vec<String>, String>) -> ExitCode {
    match lines {
        Ok(lines) => {
            lines.iter().for_each(|line| println!("{line}"));
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("zeno: {why}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!("zeno {VERSION}");
    println!("Every journey is halfway done. Forever.");
    println!();
    println!("Usage:");
    println!("  zeno run <file.zeno>      check a program, then run it. Words after the");
    println!("                            file are the program's own arguments");
    println!("  zeno check <file.zeno>    check a program for mistakes without running it");
    println!("  zeno tokens <file.zeno>   show the tokens the lexer finds");
    println!("  zeno parse <file.zeno>    show the syntax tree the parser builds");
    println!("  zeno join <file.zeno>     show the one program that the file and the");
    println!("                            files it imports are joined into");
    println!("  zeno bytecode <file.zeno> show the bytecode `zeno run --vm` runs");
    println!("  zeno build <file.zeno>    compile to C, then to a program you can run");
    println!("  zeno test [file|folder]   run the tests written in a file: each");
    println!("                            `test \"name\" {{ ... }}`. A folder stands for");
    println!("                            the files in it, and no name for this folder");
    println!("  zeno format <file.zeno>   lay the file out the way Zeno lays programs");
    println!("                            out. Several files can be named, and a");
    println!("                            folder stands for the files in it");
    println!("  zeno convert <file.py>    write a program of Python again as Zeno, in a");
    println!("                            `.zeno` file beside it. What cannot be said in");
    println!("                            Zeno is kept as Python, in an `inline python`");
    println!("                            block, and every piece is reported");
    println!("                            (docs/convert.md)");
    println!("  zeno lsp                  the language server: an editor starts this to");
    println!("                            show mistakes as you type (docs/lsp.md)");
    println!("  zeno playground           write `playground.html`: a page to write Zeno");
    println!("                            programs in and run them, in a browser, with");
    println!("                            nothing installed (docs/playground.md)");
    println!("  zeno new <name>           make a folder with a new program in it: a");
    println!("                            `main.zeno`, a module with a test, and a");
    println!("                            `zeno.package`");
    println!("  zeno new --package <name> make a folder with a new package in it: a");
    println!("                            folder of modules for other programs to need");
    println!("  zeno fetch [folder]       get the packages that the program in this");
    println!("                            folder (or in that one) needs, as the");
    println!("                            `needs` lines of its `zeno.package` list them");
    println!("  zeno search [words]       list the packages of the registry that have");
    println!("                            these words in their name or in what they");
    println!("                            say they are for. With no word, all of them");
    println!("  zeno publish [version]    check that the package in this folder is");
    println!("                            ready for the registry, and say how to ask");
    println!("                            the registry to take it (docs/registry.md)");
    println!("  zeno registry accept <folder> <address> <version> --owner <who>");
    println!("                            for whoever keeps a registry: take a version");
    println!("                            of the package at that address into the");
    println!("                            clone of the registry in that folder");
    println!("  zeno registry index <folder>");
    println!("                            write that registry's `index.json` afresh,");
    println!("                            after a change made by hand");
    println!("  zeno --version            print the version");
    println!("  zeno --help               print this message");
    println!();
    println!("Options:");
    println!("  --strict                 with check or run: every type must be known");
    println!("                           before the program runs, as firmware needs.");
    println!("                           `build` always works this way");
    println!("  --no-collector           with check: check as `build --no-collector`");
    println!("                           would, without building");
    println!("  --vm                     with run: compile the program to bytecode and");
    println!("                           run that, in place of walking the syntax tree.");
    println!("                           The program does the same, faster");
    println!("  --only <text>            with test: run only the tests that have this");
    println!("                           text in their name");
    println!("  --build                  with test: compile the file with its tests, as");
    println!("                           build compiles it, and run the tests in the");
    println!("                           program. Takes --firmware, --no-collector,");
    println!("                           --llvm, --release and --board (docs/testing.md)");
    println!("  --check                  with format: change nothing, and say which");
    println!("                           files are not laid out that way");
    println!("  --package                with new: make a package, a folder of modules");
    println!("                           for other programs to need, with no `main.zeno`");
    println!("  --update                 with fetch: look every version up again, and");
    println!("                           not at the commits `zeno.lock` has pinned");
    println!("  --firmware               with build: errors travel as values that every");
    println!("                           caller tests, with no setjmp or longjmp, as");
    println!("                           coding standards for firmware require");
    println!("  --no-collector           with build: a firmware build with no garbage");
    println!("                           collector in it. Every list and object has");
    println!("                           one owner, and is freed where its owner's");
    println!("                           block ends. Memory comes from a pool of a");
    println!("                           fixed size, not from malloc");
    println!("  --owned                  with build, check or run: every list and object");
    println!("                           has one owner and there is no collector, as");
    println!("                           `memory owned` in zeno.package says. On a");
    println!("                           computer memory grows as the program needs it");
    println!("                           (docs/memory-modes.md)");
    println!("  --collector              with build, check or run: the garbage collector,");
    println!("                           whatever zeno.package says");
    println!("  --release                with build: `+`, `-` and `*` of whole numbers");
    println!("                           wrap round instead of being checked for");
    println!("                           overflow. Every other check stays");
    println!("  --fast                   with build: no check that a correct program");
    println!("                           never fails: a list index out of range, a float");
    println!("                           divided by zero, the square root of a number");
    println!("                           below zero, and overflow, which has no defined");
    println!("                           result here (--release wraps round).");
    println!("                           As fast as C, and as unforgiving");
    println!("  --minimal                with build: the least C that runs the program.");
    println!("                           No check of any kind (an index, an overflow, a");
    println!("                           division by zero), only the parts of Zeno's");
    println!("                           runtime the program uses, and `print` as C's");
    println!("                           own printf. A mistake does whatever the C");
    println!("                           does, as in C. It goes with --firmware,");
    println!("                           --no-collector, --owned, --fast and --memory");
    println!("                           (docs/minimal.md)");
    println!("  --board <folder>         with build: build the program for a board, in one");
    println!("                           step. The `zeno.board` in the folder names the");
    println!("                           board's C compiler, its flags, the files that");
    println!("                           run before the program and the linker script,");
    println!("                           and the program comes out as `<name>.elf`. It");
    println!("                           is built with no collector, as --no-collector");
    println!("                           builds it, and goes with --minimal, --release,");
    println!("                           --fast and --memory (docs/board-build.md)");
    println!("  --llvm                   with build: compile to LLVM IR in place of C,");
    println!("                           and hand that to clang. It goes with");
    println!("                           --firmware, --no-collector and --memory");
    println!("  --web                    with build: write one page, an `.html` file,");
    println!("                           that runs the program in a browser. The");
    println!("                           program reaches the page with `import web`");
    println!("                           (docs/web.md)");
    println!("  --target <language>      with build: write the program in another");
    println!("                           language, by fixed rules: js, ts, html (a");
    println!("                           page that runs it), py, php, lua, ruby, dart,");
    println!("                           kotlin, go, java, cs, cpp, rust, wasm");
    println!("                           (WebAssembly, built with rustc).");
    println!("                           It prints what `zeno run` prints (docs/targets.md)");
    println!("  --memory                 with build: the program reports, as it ends,");
    println!("                           how much memory it used and how often the");
    println!("                           garbage collector ran. With no collector, a");
    println!("                           write to a block after it was given back");
    println!("                           stops the program (docs/pool-size.md)");
    println!("  --pool <bytes>           with build and --no-collector or --board: the");
    println!("                           size of the pool (32768 when not given)");
    println!("  --text-room <bytes>      with build and --no-collector or --board: the");
    println!("                           size of the text room (1024 when not given)");
}

/// Read a source file, or print why it could not be read.
fn read(path: &str) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(source) => Some(source),
        Err(err) => {
            eprintln!("zeno: cannot read `{path}`: {err}");
            None
        }
    }
}

/// The one file of a program that is written in one file, as `report`
/// wants it.
fn alone(path: &str, source: &str) -> [modules::File; 1] {
    [modules::File { path: path.to_string(), source: source.to_string() }]
}

/// Run the lexer, printing any errors it finds.
fn lex(path: &str, source: &str) -> Option<Vec<token::Token>> {
    match lexer::lex(source) {
        Ok(tokens) => Some(tokens),
        Err(errors) => {
            for error in &errors {
                report(&alone(path, source), error);
            }
            None
        }
    }
}

/// `zeno tokens <file>`: run the lexer and print one token per line.
fn tokens(path: &str) -> Option<()> {
    let source = read(path)?;
    for token in lex(path, &source)? {
        // `print_line` and not `println!`, so that `zeno tokens f | head`
        // ends quietly when `head` has seen enough.
        value::print_line(&format!("{:>4}:{:<3} {:?}", token.span.line, token.span.col, token.kind));
    }
    Some(())
}

/// Read a whole program: the file, the files it imports and the files
/// those import, joined into one program. Any errors found on the way
/// are printed. The files come back too, because an error found later
/// is shown with the line of the file it is in.
fn load(path: &str) -> Option<(Vec<modules::File>, ast::Program)> {
    let source = read(path)?;
    let loaded = modules::load(path, source, &|path| fs::read_to_string(path).ok());
    match loaded.program {
        Ok(program) => Some((loaded.files, program)),
        Err(errors) => {
            for error in &errors {
                report(&loaded.files, error);
            }
            None
        }
    }
}

/// `zeno parse <file>`: print the syntax tree of that one file. Its
/// imports are shown as the lines they are, and not followed.
fn parse(path: &str) -> Option<()> {
    let source = read(path)?;
    let tokens = lex(path, &source)?;
    match parser::parse(tokens) {
        Ok(program) => {
            printer::print(&program).lines().for_each(value::print_line);
            Some(())
        }
        Err(error) => {
            report(&alone(path, &source), &error);
            None
        }
    }
}

/// `zeno join <file>`: print the program as the stages after the parser
/// get it, with the files it imports joined to it and their names made
/// whole (`modules.rs`).
fn join(path: &str) -> Option<()> {
    let (_, program) = load(path)?;
    printer::print(&program).lines().for_each(value::print_line);
    Some(())
}

/// The `.zeno` files in a folder and the folders below it. The folder
/// `packages` is left out: what `zeno fetch` put there is somebody
/// else's, and is theirs to lay out.
fn zeno_files(folder: &Path, found: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return eprintln!("zeno: cannot read the folder `{}`", folder.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !path.ends_with(package::FOLDER) {
                zeno_files(&path, found);
            }
        } else if path.extension().is_some_and(|extension| extension == "zeno") {
            found.push(path.display().to_string());
        }
    }
}

/// `zeno format <file>`: write the file again in Zeno's one layout.
/// With `check`, nothing is written: the answer is whether it would be.
/// `None` if the file has a mistake, or (with `check`) is not laid out so.
fn format(path: &str, check: bool) -> Option<()> {
    let source = read(path)?;
    let formatted = match format::format(&source) {
        Ok(formatted) => formatted,
        Err(errors) => {
            for error in &errors {
                report(&alone(path, &source), error);
            }
            return None;
        }
    };
    if formatted == source {
        if !check {
            println!("{path} is laid out already");
        }
        return Some(());
    }
    if check {
        println!("{path} is not laid out as `zeno format` lays it out");
        return None;
    }
    match fs::write(path, formatted) {
        Ok(()) => {
            println!("Formatted {path}");
            Some(())
        }
        Err(err) => {
            eprintln!("zeno: cannot write `{path}`: {err}");
            None
        }
    }
}

/// The folder of the program that the file at `path` belongs to: the
/// nearest one, from the file's own upward, that has a `zeno.package`
/// or a `main.zeno` in it. With neither, the file's own folder. This
/// is where the file's imports are looked for, and the language
/// server finds it the same way.
fn program_folder(path: &str) -> PathBuf {
    let own = Path::new(path).parent().unwrap_or(Path::new("")).to_path_buf();
    let mut folder = own.clone();
    loop {
        if folder.join(package::FILE).is_file() || folder.join("main.zeno").is_file() {
            return folder;
        }
        // The empty path is the folder this is run in, and the last one
        // looked in: a file named `drivers/uart.zeno` is not looked for
        // above it.
        if folder.as_os_str().is_empty() || !folder.pop() {
            return own;
        }
    }
}

/// How the tests of one file went.
struct Tally {
    tests: usize,
    failed: usize,
}

/// `zeno test <file>`: run the file as `zeno run` would, and then each
/// test that is written in it. `asked` says that the file was named on
/// the command line, and not found in a folder: then a file with no
/// test in it is worth a line. `None` if the tests could not be run: a
/// mistake in the program, or an error before its tests.
fn test(path: &str, strict: bool, asked: bool, only: Option<&str>) -> Option<Tally> {
    let source = read(path)?;
    // The file alone says whether it has tests. One with none is not run.
    let tokens = lex(path, &source)?;
    let has_tests = match parser::parse(tokens) {
        Ok(program) => program.statements.iter().any(|statement| matches!(statement.kind, ast::StmtKind::Test { .. })),
        Err(error) => {
            report(&alone(path, &source), &error);
            return None;
        }
    };
    if !has_tests {
        if asked {
            value::print_line(&format!("{path} has no tests. A test is written `test \"what it checks\" {{ ... }}`"));
        }
        return Some(Tally { tests: 0, failed: 0 });
    }
    let loaded = modules::load_in(&program_folder(path), path, source, &|path| fs::read_to_string(path).ok());
    let files = loaded.files;
    let program = match loaded.program {
        Ok(program) => program,
        Err(errors) => {
            errors.iter().for_each(|error| report(&files, error));
            return None;
        }
    };
    let (program, errors, _) = checker::checked(program, checker::Rules { strict, one_owner: false, firmware: false });
    if !errors.is_empty() {
        errors.iter().for_each(|error| report(&files, error));
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. No test of {path} was run.", errors.len());
        return None;
    }
    value::print_line(path);
    bridge::program_at(path);
    let (_, tested) = interpreter::run_tests(&program, false, value::Outside::started_with(Vec::new()).with_page(own_page(path)), only);
    let tested = match tested {
        Ok(tested) => tested,
        Err(error) => {
            report(&files, &error);
            eprintln!("{path} stopped with this error before its tests. None of them was run.");
            return None;
        }
    };
    // What went wrong in each test that failed, under the list.
    for test in &tested {
        if let Some(failure) = &test.failure {
            value::print_line("");
            value::print_line(&format!("FAILED: {}", test.name));
            value::print_text(&shown(&files, failure));
        }
    }
    Some(Tally { tests: tested.len(), failed: tested.iter().filter(|test| test.failure.is_some()).count() })
}

/// How `zeno test --build` builds each file: what the command line said
/// beside `--build` (decision 60.2).
struct TestsBuilt {
    llvm: bool,
    board: Option<String>,
    memory_report: bool,
    defines: Vec<String>,
}

/// `zeno test --build <file>`: compile the file as `zeno build` would,
/// with its tests in it, run the program, and count its tests by the
/// marks it writes as each one ends (decision 60.2). What the program
/// prints is what `zeno test` prints, and goes straight to the terminal;
/// nothing it prints is read. `None` if the tests could not be run.
fn test_built(path: &str, asked: bool, only: Option<&str>, how: &TestsBuilt, built: (emit_c::Build, emit_c::Options)) -> Option<Tally> {
    let (kind, options) = built;
    let source = read(path)?;
    let tokens = lex(path, &source)?;
    let has_tests = match parser::parse(tokens) {
        Ok(program) => program.statements.iter().any(|statement| matches!(statement.kind, ast::StmtKind::Test { .. })),
        Err(error) => {
            report(&alone(path, &source), &error);
            return None;
        }
    };
    if !has_tests {
        if asked {
            value::print_line(&format!("{path} has no tests. A test is written `test \"what it checks\" {{ ... }}`"));
        }
        return Some(Tally { tests: 0, failed: 0 });
    }
    let loaded = modules::load_in(&program_folder(path), path, source, &|path| fs::read_to_string(path).ok());
    let files = loaded.files;
    let program = match loaded.program {
        Ok(program) => program,
        Err(errors) => {
            errors.iter().for_each(|error| report(&files, error));
            return None;
        }
    };
    let (program, tests) = match test_build::with_tests(program, only) {
        Ok(made) => made,
        Err(error) => {
            report(&files, &error);
            eprintln!("1 error found. No test of {path} was run.");
            return None;
        }
    };
    // A test is held to the rules of the build, as the program is.
    let (program, errors, types) = checker::checked(program, kind.rules());
    if !errors.is_empty() {
        errors.iter().for_each(|error| report(&files, error));
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. No test of {path} was run.", errors.len());
        return None;
    }
    let board = match &how.board {
        Some(folder) => Some((folder.as_str(), read_board(folder)?)),
        None => None,
    };
    let built = test_build::TestBuild {
        tests: tests.clone(),
        sources: files.iter().map(|file| file.source.clone()).collect(),
        board: board.is_some(),
    };
    let options = emit_c::Options {
        board_prints: board.is_some(),
        board_header: board.as_ref().is_some_and(|(_, board)| board.header.is_some()),
        ..options
    };
    let name = format!("{}-tests", Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or("program"));
    let home = Path::new(path).parent().map(Path::to_path_buf).filter(|home| !home.as_os_str().is_empty());
    let declared = match bridge_c::build_args(&types.c_libraries, &home.unwrap_or_else(|| PathBuf::from("."))) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("error: {why}");
            return None;
        }
    };

    // For a board the program is written beside the others a build for
    // a board writes, and run there, by whoever has the board (51.5).
    if let Some((folder, board)) = &board {
        let c = match emit_c::emit_tests(&program, &types, path, kind, options, &built) {
            Ok(c) => c,
            Err(error) => {
                report(&files, &error);
                eprintln!("1 error found. No test of {path} was run.");
                return None;
            }
        };
        let c_file = format!("{name}.c");
        if let Err(err) = fs::write(&c_file, c) {
            eprintln!("zeno: cannot write `{c_file}`: {err}");
            return None;
        }
        let mut before: Vec<String> = optimise(options).iter().map(|flag| flag.to_string()).collect();
        if how.memory_report {
            before.push("-DZENO_MEMORY_REPORT".to_string());
        }
        before.extend(how.defines.iter().cloned());
        let libraries = declared.iter().filter(|word| !word.starts_with("-Wl,-rpath,")).cloned();
        let after: Vec<String> = libraries.chain(emit_c::LIBRARIES.iter().map(|library| library.to_string())).collect();
        build_for_board(folder, board, &name, &c_file, &before, &after)?;
        return Some(Tally { tests: 0, failed: 0 });
    }

    // On this computer the program is built in a folder of its own, run,
    // and thrown away with the folder.
    let folder = env::temp_dir().join(format!("zeno-test-build-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&folder);
    if let Err(err) = fs::create_dir_all(&folder) {
        eprintln!("zeno: cannot make the folder `{}` to build the tests in: {err}", folder.display());
        return None;
    }
    let ran = build_and_run_tests(path, &folder, &name, &files, &program, &types, kind, options, &built, how, &declared);
    let _ = fs::remove_dir_all(&folder);
    let marks = ran?;

    // `s` as the tests begin, `p` or `f` as each ends, `e` when all are over.
    let Some(marks) = marks.strip_prefix('s') else {
        eprintln!("{path} stopped with this error before its tests. None of them was run.");
        return None;
    };
    let ended = marks.ends_with('e');
    let marks = marks.trim_end_matches('e');
    let (run, failed) = (marks.len(), marks.chars().filter(|mark| *mark == 'f').count());
    if ended {
        return Some(Tally { tests: run, failed });
    }
    // Stopped half way: the test it was in failed, and those after it
    // were not run.
    match tests.get(run) {
        Some((_, stopped_in)) => {
            let after = tests.len() - run - 1;
            match after {
                0 => eprintln!("{path} stopped in the test \"{stopped_in}\", its last."),
                _ => eprintln!(
                    "{path} stopped in the test \"{stopped_in}\". {} after it {} not run.",
                    error::counted(after, "test"),
                    if after == 1 { "was" } else { "were" }
                ),
            }
            Some(Tally { tests: run + 1, failed: failed + 1 })
        }
        None => {
            eprintln!("{path} stopped after its last test.");
            Some(Tally { tests: run, failed: failed + 1 })
        }
    }
}

/// The C (or the IR) of a test build, compiled in `folder` and run there
/// with a file of marks to write. Gives what the program wrote in it.
#[allow(clippy::too_many_arguments)]
fn build_and_run_tests(
    path: &str,
    folder: &Path,
    name: &str,
    files: &[modules::File],
    program: &ast::Program,
    types: &checker::Types,
    kind: emit_c::Build,
    options: emit_c::Options,
    built: &test_build::TestBuild,
    how: &TestsBuilt,
    declared: &[String],
) -> Option<String> {
    let program_file = folder.join(emit_c::program_file(name));
    let mut flags: Vec<String> = optimise(options).iter().map(|flag| flag.to_string()).collect();
    if how.memory_report {
        flags.push("-DZENO_MEMORY_REPORT".to_string());
    }
    flags.extend(how.defines.iter().cloned());
    flags.extend(["-o".to_string(), program_file.to_string_lossy().into_owned()]);
    let compilers = match how.llvm {
        true => {
            let ir = match emit_llvm::emit_tests(program, types, path, kind, how.memory_report, options, built) {
                Ok(ir) => ir,
                Err(error) => {
                    report(files, &error);
                    eprintln!("1 error found. No test of {path} was run.");
                    return None;
                }
            };
            let runtime = emit_llvm::runtime_for(kind, options.grows);
            // The pieces of the runtime the tests use, as `zeno build
            // --llvm` takes them (decision 56.19).
            let runtime = emit_c::with_pieces(&runtime, &emit_c::pieces_in_ir(&ir));
            let mut runtime = emit_c::with_board_parts(&runtime, &emit_c::board_parts_in_ir(&ir));
            runtime += &emit_llvm::tests_runtime(built, kind);
            let (ir_file, runtime_file) = (folder.join(format!("{name}.ll")), folder.join(format!("{name}_runtime.c")));
            for (file, text) in [(&ir_file, ir), (&runtime_file, runtime)] {
                if let Err(err) = fs::write(file, text) {
                    eprintln!("zeno: cannot write `{}`: {err}", file.display());
                    return None;
                }
            }
            flags.push("-Wno-override-module".to_string());
            flags.push(ir_file.to_string_lossy().into_owned());
            flags.push(runtime_file.to_string_lossy().into_owned());
            vec![env::var("CLANG").unwrap_or_else(|_| "clang".to_string())]
        }
        false => {
            let c = match emit_c::emit_tests(program, types, path, kind, options, built) {
                Ok(c) => c,
                Err(error) => {
                    report(files, &error);
                    eprintln!("1 error found. No test of {path} was run.");
                    return None;
                }
            };
            let c_file = folder.join(format!("{name}.c"));
            if let Err(err) = fs::write(&c_file, c) {
                eprintln!("zeno: cannot write `{}`: {err}", c_file.display());
                return None;
            }
            flags.push(c_file.to_string_lossy().into_owned());
            flags.extend(declared.iter().cloned());
            match env::var("CC") {
                Ok(named) => vec![named],
                Err(_) => vec!["cc".to_string(), "gcc".to_string(), "clang".to_string()],
            }
        }
    };
    flags.extend(emit_c::LIBRARIES.iter().map(|library| library.to_string()));
    let mut compiled = false;
    for compiler in &compilers {
        match Command::new(compiler).args(&flags).status() {
            Ok(status) if status.success() => {
                compiled = true;
                break;
            }
            Ok(_) => {
                eprintln!("zeno: `{compiler}` could not compile the tests of `{path}`. What it said is above");
                return None;
            }
            Err(_) => {}
        }
    }
    if !compiled {
        eprintln!("zeno: no compiler found (tried {}), so the tests of `{path}` were not built", modules::listed(&compilers));
        return None;
    }
    let marks = folder.join("marks");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    if let Err(err) = Command::new(&program_file).env("ZENO_TEST_MARKS", &marks).status() {
        eprintln!("zeno: cannot run the tests of `{path}`: {err}");
        return None;
    }
    Some(fs::read_to_string(&marks).unwrap_or_default())
}

/// `zeno check <file>`: run the type checker, printing every error it finds.
fn check(path: &str, rules: checker::Rules) -> Option<(Vec<modules::File>, ast::Program)> {
    let (files, program) = load(path)?;
    // `check --no-collector` checks what that build would build.
    let program = if rules.one_owner { program.without_tests() } else { program };
    let (program, errors, _) = checker::checked(program, rules);
    for error in &errors {
        report(&files, error);
    }
    if errors.is_empty() {
        Some((files, program))
    } else {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was run.", errors.len());
        None
    }
}

/// `zeno check --no-collector <file>`: say what `zeno build
/// --no-collector` would say, and build nothing.
///
/// Most of what such a build refuses the checker finds: a type that is
/// not known, a list with two owners. A few things only the compiler
/// meets, as it works out where each value is kept (a list of numbers
/// that may be `none` is one). So this runs the compiler too and throws
/// the C away. "No errors found" then means the build will go through.
fn check_no_collector(path: &str) -> Option<()> {
    let kind = emit_c::Build::NoCollector;
    let (files, program) = load(path)?;
    let (program, mut errors, types) = checker::checked(program.without_tests(), kind.rules());
    if errors.is_empty() {
        errors.extend(emit_c::emit(&program, &types, path, kind).err());
    }
    for error in &errors {
        report(&files, error);
    }
    if errors.is_empty() {
        Some(())
    } else {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was run.", errors.len());
        None
    }
}

/// `zeno bytecode <file>`: print the bytecode the program compiles to.
fn bytecode(path: &str) -> Option<()> {
    let (_, program) = load(path)?;
    // What `zeno run --vm` runs. By then `count += 1` on a sized number
    // is `count = u8(count + 1)`, which only the checker can tell.
    let program = checker::checked(program, checker::Rules::default()).0;
    bytecode::print(&emit_bytecode::compile(&program)).lines().for_each(value::print_line);
    Some(())
}

/// `zeno run <file>`: check the program, and if it is sound, run it.
///
/// There are two engines to run it with. Both give the same result.
fn run(path: &str, strict: bool, on_vm: bool, arguments: Vec<String>, owned: bool) -> Option<()> {
    // A program whose memory is `owned` is checked by the rules its
    // build will be checked by, so that what runs also builds.
    let rules = match owned {
        true => emit_c::Build::NoCollector.rules(),
        false => checker::Rules { strict, one_owner: false, firmware: false },
    };
    let (files, program) = check(path, rules)?;
    // The words after the file are the program's (`program.arguments()`),
    // and what it reads with `input()` is the keyboard.
    bridge::program_at(path);
    let outside = value::Outside::started_with(arguments).with_page(own_page(path));
    let ran = match on_vm {
        true => vm::run_ending(&program, false, outside),
        false => interpreter::run_ending(&program, false, outside),
    };
    match ran.result {
        // `program.exit(3)`: the shell is given 3. What was printed is
        // sent first, as when the program ends at its last line.
        Ok(()) if ran.status != 0 => {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(i32::from(ran.status));
        }
        Ok(()) => Some(()),
        Err(error) => {
            report(&files, &error);
            None
        }
    }
}

/// What the `memory` line of the `zeno.package` beside `path` says, if
/// there is one. A mistake in the file is not reported here: loading the
/// program reads the file again and reports it with the rest.
fn package_memory(path: &str) -> Option<package::Memory> {
    let folder = Path::new(path).parent().unwrap_or(Path::new(""));
    let source = fs::read_to_string(folder.join(package::FILE)).ok()?;
    package::parse(&source, 0).ok()?.memory
}

/// What the files of the program say, when its `zeno.package` says
/// nothing: `memory owned` if every file of it says `memory owned` at its
/// top (decision 22.9). A program some of whose files say it is built
/// with the collector, and only those files' classes are owned.
fn files_memory(path: &str) -> Option<package::Memory> {
    let source = fs::read_to_string(path).ok()?;
    let program = modules::load(path, source, &|path| fs::read_to_string(path).ok()).program.ok()?;
    let mut files = std::iter::once(program.memory).chain(program.modules.iter().map(|module| module.memory));
    files.all(|memory| memory == Some(ast::Memory::Owned)).then_some(package::Memory::Owned)
}

/// Which kind of build the options and the program's `memory` ask for.
///
/// `--no-collector` is a build for a chip: no collector, and a pool and a
/// text room of fixed sizes. `memory owned` is the same rules on a
/// computer, where the pool grows, unless `--firmware` says the program
/// is for a chip too. docs/memory-modes.md has the whole table.
fn how_built(
    no_collector: bool,
    firmware: bool,
    release: bool,
    fast: bool,
    memory: Option<package::Memory>,
) -> (emit_c::Build, emit_c::Options) {
    let (kind, grows) = match (no_collector, memory, firmware) {
        (true, _, _) => (emit_c::Build::NoCollector, false),
        (false, Some(package::Memory::Owned), _) => (emit_c::Build::NoCollector, !firmware),
        (false, _, true) => (emit_c::Build::Firmware, false),
        (false, _, false) => (emit_c::Build::Ordinary, false),
    };
    (kind, emit_c::Options { release: release || fast, grows, fast, ..Default::default() })
}

/// `zeno build <file>`: compile the program to C, then hand the C to the
/// machine's C compiler to make a program that runs by itself.
///
/// `hello.zeno` becomes `hello.c` and `hello`, in the folder you are in. The
/// C file is kept, because reading it is the best way to see what the
/// compiler did.
fn build(path: &str, memory_report: bool, kind: emit_c::Build, options: emit_c::Options, board: Option<&str>, defines: &[String]) -> Option<()> {
    // A mistake in the board's own file is said before anything is built.
    let board = match board {
        Some(folder) => Some((folder, read_board(folder)?)),
        None => None,
    };
    // For a board `print` is never written as `printf`, and a board
    // with a header of its own may be the one that prints (decision 52.4).
    let board_header = board.as_ref().is_some_and(|(_, board)| board.header.is_some());
    let options = emit_c::Options { board_prints: board.is_some(), board_header, ..options };
    // Compiled code has no room for values of unknown type, so `build`
    // always checks in strict mode. A build with no collector also
    // checks that every list and object has one owner.
    let (files, program) = load(path)?;
    let (program, errors, types) = checker::checked(program.without_tests(), kind.rules());
    for error in &errors {
        report(&files, error);
    }
    if !errors.is_empty() {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was built.", errors.len());
        return None;
    }

    let c = match emit_c::emit_with(&program, &types, path, kind, options) {
        Ok(c) => c,
        Err(error) => {
            report(&files, &error);
            return None;
        }
    };
    // A library of C the program declares is built in with it (decision 44.10).
    let home = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let home = if home.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { home };
    let declared = match bridge_c::build_args(&types.c_libraries, &home) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("error: {why}");
            return None;
        }
    };
    let name = Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or("program");
    let c_file = format!("{name}.c");
    // The C a build writes must not take the place of a file of C the
    // program builds with.
    let same = |arg: &String| fs::canonicalize(arg).ok().is_some_and(|known| fs::canonicalize(&c_file).ok() == Some(known));
    if declared.iter().any(same) {
        eprintln!(
            "zeno: `zeno build` writes the program's C to `{c_file}`, which is a file `c.library` names. \
             Give one of the two another name"
        );
        return None;
    }
    if let Err(err) = fs::write(&c_file, c) {
        eprintln!("zeno: cannot write `{c_file}`: {err}");
        return None;
    }
    println!("Wrote {c_file}");
    if let Some((folder, board)) = &board {
        let mut before: Vec<String> = optimise(options).iter().map(|flag| flag.to_string()).collect();
        if memory_report && !options.minimal {
            before.push("-DZENO_MEMORY_REPORT".to_string());
        }
        before.extend(defines.iter().cloned());
        // A chip has no loader to be told where a library is.
        let libraries = declared.iter().filter(|word| !word.starts_with("-Wl,-rpath,")).cloned();
        let after: Vec<String> = libraries.chain(emit_c::LIBRARIES.iter().map(|library| library.to_string())).collect();
        return build_for_board(folder, board, name, &c_file, &before, &after);
    }

    // `CC` is the usual way to say which C compiler to use. With no
    // `CC`, the first of these that is installed is used: `cc` is the
    // usual name for whichever one a system has, `gcc` is what MinGW
    // calls itself on Windows, which has no `cc`, and `clang` is the
    // one that comes with Visual Studio.
    let compilers = match env::var("CC") {
        Ok(named) => vec![named],
        Err(_) => vec!["cc".to_string(), "gcc".to_string(), "clang".to_string()],
    };
    let program_file = emit_c::program_file(name);
    for compiler in &compilers {
        let mut command = Command::new(compiler);
        if memory_report && !options.minimal {
            // Turns on a few lines in the runtime that say, as the program
            // ends, how much memory it used and how often the collector ran.
            // (A minimal build has those lines written in or left out.)
            command.arg("-DZENO_MEMORY_REPORT");
        }
        command.args(defines);
        match command.args(optimise(options)).args(["-o", &program_file, &c_file]).args(&declared).args(emit_c::LIBRARIES).status() {
            Ok(status) if status.success() => {
                println!("Built {name}. Run it with {}", how_to_run(name));
                return Some(());
            }
            Ok(_) if !types.c_libraries.is_empty() => {
                eprintln!(
                    "zeno: `{compiler}` could not compile `{c_file}` with the libraries of C it declares. What it said is above: \
                     a declaration that does not agree with its header, or a library that is not there, is the usual reason"
                );
                return None;
            }
            // A firmware build with a collector is the one thing a
            // compiler that is neither GCC nor Clang is refused, by a
            // line of the C that says so (decision 55.5). What the
            // compiler said went straight to the terminal, so which of
            // the two it was is said here.
            Ok(_) if kind == emit_c::Build::Firmware => {
                eprintln!(
                    "zeno: `{compiler}` could not compile `{c_file}`. If it said above that the collector of a firmware build \
                     wants GCC or Clang, build with one of them, or with `--no-collector` or `--owned`. \
                     Anything else it said is a bug in Zeno, not in your program"
                );
                return None;
            }
            Ok(_) => {
                eprintln!("zeno: `{compiler}` could not compile `{c_file}`. That is a bug in Zeno, not in your program");
                return None;
            }
            // There is no such program. The next name is tried.
            Err(_) => {}
        }
    }
    let tried: Vec<String> = compilers.clone();
    eprintln!("zeno: no C compiler found (tried {}), so only the C was written.", modules::listed(&tried));
    eprintln!(
        "      Install one, or set CC, then run: cc {} -o {program_file} {c_file} {}",
        optimise(options).join(" "),
        emit_c::LIBRARIES.join(" ")
    );
    None
}

/// The `zeno.board` of the folder `--board` names, or `None` after
/// saying what is wrong with it.
fn read_board(folder: &str) -> Option<board_build::Board> {
    let file = Path::new(folder).join(board_build::FILE);
    let shown = modules::shown(&file);
    if !Path::new(folder).is_dir() {
        eprintln!("zeno: `--board` is followed by the folder of a board, and `{folder}` is not a folder");
        return None;
    }
    let Ok(source) = fs::read_to_string(&file) else {
        eprintln!("zeno: `{folder}` has no `{}`, the file that says how a program is built for a board", board_build::FILE);
        eprintln!("      (docs/board-build.md)");
        return None;
    };
    let board = match board_build::parse(&source) {
        Ok(board) => board,
        Err(errors) => {
            for error in &errors {
                report(&alone(&shown, &source), error);
            }
            return None;
        }
    };
    let missing = board.missing(Path::new(folder));
    if !missing.is_empty() {
        eprintln!("zeno: `{shown}` names {}, and the folder has no such file", modules::listed(&missing));
        return None;
    }
    Some(board)
}

/// The last step of `zeno build --board`: the board's own compiler makes
/// `<name>.elf` of the program's C, the board's files and its linker
/// script, in one command (decision 51).
fn build_for_board(folder: &str, board: &board_build::Board, name: &str, c_file: &str, before: &[String], after: &[String]) -> Option<()> {
    let program_file = format!("{name}.elf");
    let asks = board.asks(Path::new(folder), before, c_file, &program_file, after);
    let compiler = &board.compiler;
    match Command::new(compiler).args(&asks).status() {
        Ok(status) if status.success() => {
            println!("Built {program_file} for the board in `{folder}`.");
            if let Some(run) = board.runs(&program_file) {
                println!("Run it with: {}", board_build::typed(&run));
            }
            Some(())
        }
        Ok(_) => {
            eprintln!("zeno: `{compiler}` could not build `{c_file}` for the board in `{folder}`. What it said is above.");
            eprintln!("      If it names a file of the board, the mistake is in the board's folder. If it names");
            eprintln!("      `{c_file}`, that is a bug in Zeno, not in your program");
            None
        }
        Err(_) => {
            eprintln!("zeno: the board in `{folder}` is built with `{compiler}`, and there is no such command here,");
            eprintln!("      so only the C was written. Install it, then build again, or run:");
            eprintln!("        {compiler} {}", board_build::typed(&asks));
            None
        }
    }
}

/// How hard the C compiler (or clang, for IR) is asked to work. `--fast`
/// asks for `-O3`, and says that no line reads `errno` after a function
/// of maths, so `sqrt` can be the one instruction the processor has
/// (decision 23.1). `--minimal` asks for the smallest program, `-Os`,
/// unless `--fast` is there too (decision 30.8).
fn optimise(options: emit_c::Options) -> &'static [&'static str] {
    match (options.fast, options.minimal) {
        (true, _) => &["-O3", "-fno-math-errno"],
        (false, true) => &["-Os"],
        (false, false) => &["-O2"],
    }
}

/// What to type to run the program a build made, in the folder it is in.
fn how_to_run(name: &str) -> String {
    if cfg!(windows) { format!(".\\{name}") } else { format!("./{name}") }
}

/// `zeno build --llvm <file>`: compile the program to LLVM IR, then hand
/// the IR to clang to make a program that runs by itself.
///
/// `hello.zeno` becomes `hello.ll`, `hello_runtime.c` and `hello`. The IR
/// holds the program. The C file is the runtime the program calls for
/// text, printing and memory, which a C build has pasted in front of it.
fn build_llvm(path: &str, memory_report: bool, kind: emit_c::Build, options: emit_c::Options, defines: &[String]) -> Option<()> {
    let (files, program) = load(path)?;
    let (program, errors, types) = checker::checked(program.without_tests(), kind.rules());
    for error in &errors {
        report(&files, error);
    }
    if !errors.is_empty() {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was built.", errors.len());
        return None;
    }

    let ir = match emit_llvm::emit_with(&program, &types, path, kind, memory_report, options) {
        Ok(ir) => ir,
        Err(error) => {
            report(&files, &error);
            return None;
        }
    };
    let name = Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or("program");
    let (ir_file, runtime_file) = (format!("{name}.ll"), format!("{name}_runtime.c"));
    // The parts of the runtime for input pins and serial ports, only if
    // the program makes one (decisions 45.4 and 46.5).
    let runtime = match options.spares_checked {
        true => emit_llvm::runtime_with_spares_checked(options.grows),
        false => emit_llvm::runtime_for(kind, options.grows),
    };
    let runtime = emit_c::with_pieces(&runtime, &emit_c::pieces_in_ir(&ir));
    let mut runtime = emit_c::with_board_parts(&runtime, &emit_c::board_parts_in_ir(&ir));
    if options.fast {
        runtime.insert_str(0, "#define ZENO_UNCHECKED 1\n");
    }
    for (file, text) in [(&ir_file, ir), (&runtime_file, runtime)] {
        if let Err(err) = fs::write(file, text) {
            eprintln!("zeno: cannot write `{file}`: {err}");
            return None;
        }
    }
    println!("Wrote {ir_file} and {runtime_file}");

    // Only clang reads IR, so `CC` is not looked at here. `CLANG` names
    // another one, such as `clang-18`.
    let compiler = env::var("CLANG").unwrap_or_else(|_| "clang".to_string());
    // The IR does not say which processor it is for, so that the same
    // file builds anywhere. clang fills in its own and would mention
    // doing so; `-Wno-override-module` says there is no need.
    let mut flags = optimise(options).to_vec();
    flags.push("-Wno-override-module");
    if memory_report {
        // Turns on the lines of the runtime that count memory and print
        // the report. The IR asks for the report to be printed at the end.
        flags.push("-DZENO_MEMORY_REPORT");
    }
    flags.extend(defines.iter().map(String::as_str));
    let program_file = emit_c::program_file(name);
    flags.extend(["-o", &program_file, &ir_file, &runtime_file]);
    flags.extend(emit_c::LIBRARIES);
    match Command::new(&compiler).args(&flags).status() {
        Ok(status) if status.success() => {
            println!("Built {name}. Run it with {}", how_to_run(name));
            Some(())
        }
        Ok(_) => {
            eprintln!("zeno: `{compiler}` could not compile `{ir_file}`. That is a bug in Zeno, not in your program");
            None
        }
        Err(_) => {
            eprintln!("zeno: no clang found (tried `{compiler}`), so only the IR and the runtime were written.");
            eprintln!("      Install clang 15 or later, or set CLANG, then run:");
            eprintln!("      clang {}", flags.join(" "));
            None
        }
    }
}

/// The page a program gets if nobody wrote one for it: a title, and a
/// place for what the program prints, where a long line goes on to the
/// next and not off the side.
const DEFAULT_PAGE: &str = "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>NAME</title>
</head>
<body>
<pre id=\"zeno-output\" style=\"white-space: pre-wrap\"></pre>
</body>
</html>
";

/// Bytes as text, in base64: three bytes become four letters. It is how
/// `zeno.wasm` is written into a page, which is text.
fn base64(bytes: &[u8]) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for three in bytes.chunks(3) {
        let bits = (three[0] as u32) << 16 | (*three.get(1).unwrap_or(&0) as u32) << 8 | *three.get(2).unwrap_or(&0) as u32;
        for place in 0..4 {
            // Three bytes fill four letters, two fill three, one fills two.
            // The rest is written `=`.
            match place <= three.len() {
                true => out.push(LETTERS[(bits >> (18 - 6 * place) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

/// `zeno.wasm`, which is Zeno itself built for a browser: where it is,
/// and its bytes. It is looked for where `ZENO_WASM` says, then beside
/// `zeno`, where a release has it, then where `cargo build --release
/// --target wasm32-unknown-unknown` leaves it when `zeno` was built by
/// cargo too.
fn zeno_wasm() -> Option<Vec<u8>> {
    let mut places = Vec::new();
    if let Ok(named) = env::var("ZENO_WASM") {
        places.push(PathBuf::from(named));
    } else if let Some(folder) = env::current_exe().ok().as_deref().and_then(Path::parent) {
        places.push(folder.join("zeno.wasm"));
        places.push(folder.join("../wasm32-unknown-unknown/release/zeno.wasm"));
    }
    if let Some(bytes) = places.iter().find_map(|place| fs::read(place).ok()) {
        return Some(bytes);
    }
    eprintln!("zeno: `build --web` puts `zeno.wasm` in the page, which is Zeno itself built for a browser,");
    eprintln!("      and there is none at:");
    for place in &places {
        eprintln!("        {}", modules::shown(place));
    }
    eprintln!("      A release of Zeno has it beside `zeno`. From the source, build it with");
    eprintln!("        cargo build --release --target wasm32-unknown-unknown");
    eprintln!("      and set ZENO_WASM to `target/wasm32-unknown-unknown/release/zeno.wasm`.");
    None
}

/// `zeno playground`: write `playground.html`, one page in which
/// anybody can write a Zeno program and run it, with nothing installed.
///
/// The page is `src/playground.html` with four things written into it:
/// `zeno.wasm` (packed, as in a built page), `src/web.js`, the page a
/// program gets when it has none of its own, and a few of the examples
/// to start from. docs/playground.md walks through it.
fn playground() -> Option<()> {
    let wasm = zeno_wasm()?;
    let html_file = "playground.html";
    if let Err(err) = fs::write(html_file, playground_page(&wasm)) {
        eprintln!("zeno: cannot write `{html_file}`: {err}");
        return None;
    }
    println!("Wrote {html_file}. Open it in a browser, or put it on a web server");
    Some(())
}

/// The playground, with Zeno and the examples in it.
fn playground_page(wasm: &[u8]) -> String {
    // Text as JavaScript reads it. `<` is written another way for the
    // reason `with_program` gives.
    let quoted = |json: json::Json| json.write().replace('<', "\\u003c");
    // Each example has a group, a name, one line on what it shows, the
    // program, and the page it is in ("" for one that only prints).
    let example = |group: &str, name: &str, about: &str, program: &str, page: &str| {
        json::Json::object([
            ("group", json::Json::text(group)),
            ("name", json::Json::text(name)),
            ("about", json::Json::text(about)),
            ("program", json::Json::text(program)),
            ("page", json::Json::text(page)),
        ])
    };
    let examples = json::Json::List(vec![
        example("Start here", "Hello", "The whole program is one line", include_str!("../examples/hello.zeno"), ""),
        example("Start here", "A tour of the language", "Most of Zeno on one page", include_str!("../examples/tour.zeno"), ""),
        example("Start here", "Primes", "Loops, lists and numbers", include_str!("../examples/primes.zeno"), ""),
        example("Start here", "Four ways to write it", "One job, typed, untyped, with a class, functional", include_str!("../examples/styles.zeno"), ""),
        example("The language", "Shapes", "Classes, enums and match", include_str!("../examples/shapes.zeno"), ""),
        example("The language", "Generics", "One ring buffer for many types", include_str!("../examples/generics.zeno"), ""),
        example("The language", "Counting faults", "A dict of counts, sorted", include_str!("../examples/faults.zeno"), ""),
        example("The language", "Handlers", "Functions as values, and errors", include_str!("../examples/handlers.zeno"), ""),
        example("The language", "Commands", "Enum cases that carry values", include_str!("../examples/commands.zeno"), ""),
        example("Firmware", "Packets", "Sized numbers and bit operators", include_str!("../examples/packet.zeno"), ""),
        example("Firmware", "Sensor sentences", "Reading GPS-style lines and checksums", include_str!("../examples/sentence.zeno"), ""),
        example("Firmware", "A scheduler", "Tasks that run every so many ticks", include_str!("../examples/scheduler.zeno"), ""),
        example("Firmware", "Owned buffers", "Memory given back at a line you can point to", include_str!("../examples/buffer.zeno"), ""),
        example("Pages", "Things to do", "A list you add to, in a page", include_str!("../examples/todo.zeno"), include_str!("../examples/todo.html")),
        example("Pages", "Sketch", "Draw with the mouse or a finger", include_str!("../examples/sketch.zeno"), include_str!("../examples/sketch.html")),
    ]);
    // Each marker is in the page once. The page is cut at them, so that
    // nothing written in is itself searched for a marker.
    let mut page = String::new();
    let mut rest = include_str!("playground.html");
    let parts = [
        ("__WEB_JS__", quoted(json::Json::text(include_str!("web.js")))),
        ("__WASM__", base64(&deflate::pack(wasm))),
        ("__EXAMPLES__", quoted(examples)),
    ];
    for (marker, value) in parts {
        let (before, after) = rest.split_once(marker).expect("the playground has each marker");
        page += before;
        page += &value;
        rest = after;
    }
    page + rest
}

/// The page that was written for a program: `todo.html` beside
/// `todo.zeno`, if there is one. `zeno build --web` builds on it, and
/// `zeno run` and `zeno test` give it to the simulated page, so that
/// asking for an element the page does not have is an error on a desk
/// as it is in a browser.
fn own_page(path: &str) -> Option<String> {
    fs::read_to_string(Path::new(path).with_extension("html")).ok()
}

/// The languages `--target` can write a program in: the name after
/// `--target`, the file it writes, and how to run that file.
const TARGETS: &[(&str, &str, &str)] =
    &[("js", "js", "node {file}"), ("ts", "ts", "node {file}"), ("html", "html", "open {file} in a browser"), ("py", "py", "python3 {file}"), ("php", "php", "php {file}"), ("lua", "lua", "lua {file}"), ("ruby", "rb", "ruby {file}"), ("dart", "dart", "dart run {file}"), ("kotlin", "kt", "kotlinc {file} -include-runtime -d {name}.jar && java -jar {name}.jar"), ("go", "go", "go run {file}"), ("java", "java", "java {file}"), ("cs", "cs", "dotnet run {file}"), ("cpp", "cpp", "g++ -std=c++17 -O2 {file} -o {name} && ./{name}"), ("rust", "rs", "rustc -O --edition 2021 {file} -o {name} && ./{name}"), ("wasm", "wasm", "wasmtime {file}")];

/// `zeno build --target js <file>`: write the program in another
/// language (lower.rs, and emit_js.rs and the others).
///
/// The program is checked as `zeno run` checks it, and what is written
/// does what `zeno run` does: each value is a value of the target's
/// runtime, which is a port of ops.rs and builtins.rs.
fn build_target(path: &str, target: &str) -> Option<()> {
    let Some((_, extension, how)) = TARGETS.iter().find(|(name, _, _)| *name == target) else {
        let names: Vec<String> = TARGETS.iter().map(|(name, _, _)| format!("`{name}`")).collect();
        eprintln!("zeno: there is no target `{target}`. The targets are {}", modules::listed(&names));
        return None;
    };
    let (files, program) = load(path)?;
    let (program, errors, _) = checker::checked(program.without_tests(), checker::Rules::default());
    for error in &errors {
        report(&files, error);
    }
    if !errors.is_empty() {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was built.", errors.len());
        return None;
    }
    let names: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
    let options = lower::Options { unordered_arguments: matches!(target, "go" | "cpp") };
    let tree = match lower::lower(&program, names, options) {
        Ok(tree) => tree,
        Err(error) => {
            report(&files, &error);
            return None;
        }
    };
    let source = Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path);
    let name = Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or("program");
    // Go reads a meaning into two kinds of file name: `go run` refuses a
    // file that ends in `_test.go`, and does not see one that begins
    // with `_` or `.`. Such a program gets `program` in its name.
    let go_name = match name {
        _ if target != "go" => name.to_string(),
        _ if name.starts_with(['_', '.']) => format!("program{name}"),
        _ if name.ends_with("_test") => format!("{name}_program"),
        _ => name.to_string(),
    };
    let name = go_name.as_str();
    let file = format!("{name}.{extension}");
    let text = match target {
        // The page written for the program, if there is one, with the
        // program put in it. Building must not write over that page.
        "html" => {
            let own_page = Path::new(path).with_extension("html");
            if fs::canonicalize(&own_page).ok().is_some_and(|own| fs::canonicalize(&file).ok() == Some(own)) {
                eprintln!("zeno: building here would write over `{file}`, the page written for {path}.");
                eprintln!("      Build from another folder, as in: mkdir built && cd built && zeno build --target html ../{path}");
                return None;
            }
            let page = fs::read_to_string(&own_page).unwrap_or_else(|_| DEFAULT_PAGE.replace("NAME", name));
            emit_js::page(&tree, &page)
        }
        "ts" => emit_js::emit_typed(&tree, source),
        "py" => emit_py::emit(&tree, source),
        "php" => emit_php::emit(&tree, source),
        "lua" => emit_lua::emit(&tree, source),
        "ruby" => emit_ruby::emit(&tree, source),
        "dart" => emit_dart::emit(&tree, source),
        "kotlin" => emit_kotlin::emit(&tree, source),
        "go" => emit_go::emit(&tree, source),
        "java" => emit_java::emit(&tree, source),
        "cs" => emit_cs::emit(&tree, source),
        "cpp" => emit_cpp::emit(&tree, source),
        "rust" => emit_rust::emit(&tree, source),
        "wasm" => return build_wasm(&emit_rust::emit(&tree, source), name, &file, how),
        _ => emit_js::emit(&tree, source),
    };
    if let Err(err) = fs::write(&file, text) {
        eprintln!("zeno: cannot write `{file}`: {err}");
        return None;
    }
    if target == "html" {
        println!("Wrote {file}. Open it in a browser");
    } else {
        println!("Wrote {file}. Run it with `{}`", command_for(how, &file, name));
    }
    Some(())
}

/// The command that runs what was built, with the names in quotes where a
/// shell would otherwise read them as something else (`my prog.py`).
fn command_for(how: &str, file: &str, name: &str) -> String {
    fn quoted(name: &str) -> String {
        if name.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+')) {
            return name.to_string();
        }
        let mut out = String::from("\"");
        for c in name.chars() {
            if matches!(c, '"' | '\\' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    }
    how.replace("./{name}", &quoted(&format!("./{name}"))).replace("{file}", &quoted(file)).replace("{name}", &quoted(name))
}

/// `zeno build --target wasm`: the program written in Rust, then built by
/// `rustc` for WebAssembly with WASI, the system interface every
/// WebAssembly host outside a browser has (wasmtime, wasmer, Node). The
/// Rust is written in a folder of its own and left out; what is kept is
/// the `.wasm` file (decision 24.19).
fn build_wasm(rust: &str, name: &str, file: &str, how: &str) -> Option<()> {
    let folder = env::temp_dir().join(format!("zeno-wasm-{}-{name}", std::process::id()));
    let source = folder.join("program.rs");
    if fs::create_dir_all(&folder).and_then(|_| fs::write(&source, rust)).is_err() {
        eprintln!("zeno: cannot write the Rust for `{file}` in {}", folder.display());
        return None;
    }
    let built = Command::new("rustc")
        .args(["-O", "--edition", "2021", "--target", "wasm32-wasip1", "-C", "link-arg=-zstack-size=33554432", "-o", file])
        .arg(&source)
        .status();
    let _ = fs::remove_dir_all(&folder);
    match built {
        Ok(status) if status.success() => {
            println!("Built {file}. Run it with `{}`, or any WebAssembly host with WASI", command_for(how, file, ""));
            Some(())
        }
        Ok(_) => {
            eprintln!("zeno: rustc could not build `{file}`. It needs Rust's WebAssembly target: rustup target add wasm32-wasip1");
            None
        }
        Err(_) => {
            eprintln!("zeno: `--target wasm` needs rustc, from https://rustup.rs, with: rustup target add wasm32-wasip1");
            None
        }
    }
}

/// `zeno build --web <file>`: write one page that runs the program in a
/// browser.
///
/// `counter.zeno` becomes `counter.web.html`, in the folder you are in.
/// The page holds three things: the program, as the text you wrote;
/// `zeno.wasm`, which is Zeno itself built for a browser; and
/// `src/web.js`, which starts the one with the other. Nothing is
/// compiled: the browser runs the program the way `zeno run` does.
///
/// If there is a `counter.html` beside `counter.zeno`, that is the page,
/// and the three are put at its end. If there is none, the page is an
/// empty one that shows what the program prints.
fn build_web(path: &str) -> Option<()> {
    // Checked as `zeno run` checks, since the page runs it as `zeno run`
    // does.
    let (files, program) = load(path)?;
    let errors = checker::checked(program, checker::Rules::default()).1;
    for error in &errors {
        report(&files, error);
    }
    if !errors.is_empty() {
        let s = if errors.len() == 1 { "" } else { "s" };
        eprintln!("{} error{s} found. Nothing was built.", errors.len());
        return None;
    }
    let wasm = zeno_wasm()?;

    let name = Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or("program");
    let own_page = Path::new(path).with_extension("html");
    let page = match fs::read_to_string(&own_page) {
        Ok(page) => page,
        Err(_) => DEFAULT_PAGE.replace("NAME", name),
    };
    let html_file = format!("{name}.web.html");
    if let Err(err) = fs::write(&html_file, with_program(&page, &files_of(path)?, &wasm)) {
        eprintln!("zeno: cannot write `{html_file}`: {err}");
        return None;
    }
    match own_page.exists() {
        true => println!("Built {html_file} from {} and {path}. Open it in a browser", modules::shown(&own_page)),
        false => println!("Built {html_file}. Open it in a browser"),
    }
    Some(())
}

/// The files of a program as the page carries them, in JSON:
///
/// ```text
/// {"main": "todo.zeno", "files": {"todo.zeno": "import web\n...", "words.zeno": "..."}}
/// ```
///
/// The program is read once more for this, as if from inside its own
/// folder. A file is then named from that folder (`todo.zeno`, and not
/// `/home/partha/site/todo.zeno`), which is what the browser looks for,
/// what an error in the page shows, and all that anyone who opens the
/// page learns about the machine it was built on.
fn files_of(path: &str) -> Option<String> {
    let folder = Path::new(path).parent().unwrap_or(Path::new(""));
    let main = Path::new(path).file_name().and_then(|name| name.to_str())?;
    let found = std::cell::RefCell::new(vec![(main.to_string(), json::Json::Text(read(path)?))]);
    modules::load(main, read(path)?, &|path| {
        let text = fs::read_to_string(folder.join(path)).ok()?;
        found.borrow_mut().push((path.to_string(), json::Json::Text(text.clone())));
        Some(text)
    });
    let program =
        json::Json::object([("main", json::Json::Text(main.to_string())), ("files", json::Json::Object(found.into_inner()))]);
    Some(program.write())
}

/// The page, with the program, Zeno and what starts them written into it.
fn with_program(page: &str, program: &str, wasm: &[u8]) -> String {
    // A browser reads `</script>` as the end of the script, even inside
    // a str of the program. `<` written as `<` is the same str to
    // JSON, and nothing to the browser.
    let program = program.replace('<', "\\u003c");
    // `zeno.wasm` is packed, and then written as text (decision 16.11).
    let scripts = format!(
        "<script>\n{}</script>\n<script>Zeno.start(Zeno.unpacked(\"{}\"), {program});</script>\n",
        include_str!("web.js"),
        base64(&deflate::pack(wasm)),
    );
    // The scripts go last in the body, so that the page is all there
    // when the program starts. A page with no `</body>` gets them at
    // its end, which a browser reads the same way.
    let at = page.to_ascii_lowercase().rfind("</body>").unwrap_or(page.len());
    format!("{}{scripts}{}", &page[..at], &page[at..])
}

/// Print an error with the offending line and a caret under the exact spot:
///
/// ```text
/// error: Zeno has no semicolons. A line break ends a statement
///   --> main.zeno:3:10
///    |
///  3 | let x = 1;
///    |          ^
/// ```
///
/// The position says which file of the program it is in (`Span::file`),
/// and the line shown is that file's.
fn report(files: &[modules::File], error: &error::Error) {
    eprint!("{}", shown(files, error));
}

/// An error as it is shown: what is wrong, where, and the line itself
/// with a mark under the place.
fn shown(files: &[modules::File], error: &error::Error) -> String {
    let file = &files[error.span.file().min(files.len() - 1)];
    let (path, line, col) = (&file.path, error.span.line_in_file(), error.span.col);
    let text = file.source.lines().nth(line.saturating_sub(1)).unwrap_or("");
    let gutter = line.to_string().len();
    // Copy tabs from the source line into the padding so the caret lines up.
    let pad: String = text
        .chars()
        .take(col - 1)
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    let mut out = format!("error: {}\n", error.message);
    out += &format!("{:gutter$}--> {path}:{line}:{col}\n", "");
    out += &format!("{:gutter$} |\n", "");
    out += &format!("{line} | {text}\n");
    out += &format!("{:gutter$} | {pad}^\n", "");
    if let Some(note) = &error.note {
        for line in note.lines() {
            out += &format!("{:gutter$} = note: {line}\n", "");
        }
    }
    out + "\n"
}

/// `--pool` and `--text-room`, as the command line gave them.
struct Sizes {
    pool: Option<u32>,
    text_room: Option<u32>,
}

impl Sizes {
    /// The first of the two that was given, to name in a message.
    fn named(&self) -> Option<&'static str> {
        match (self.pool, self.text_room) {
            (Some(_), _) => Some("--pool"),
            (None, Some(_)) => Some("--text-room"),
            (None, None) => None,
        }
    }

    /// What they tell the C compiler: the sizes the runtime takes when
    /// it is not told (5.9c, 5.9j), in their place.
    fn defines(&self) -> Vec<String> {
        let pool = self.pool.map(|bytes| format!("-DZENO_POOL={bytes}"));
        let text_room = self.text_room.map(|bytes| format!("-DZENO_TEXT_ROOM={bytes}"));
        pool.into_iter().chain(text_room).collect()
    }
}

/// `--pool 4096` or `--pool=4096`, taken out of `args`: a whole number of
/// bytes above zero, and at most what the runtime's messages can print.
fn size_option(args: &mut Vec<String>, name: &str) -> Result<Option<u32>, String> {
    let Some(at) = args.iter().position(|arg| arg == name || arg.strip_prefix(name).is_some_and(|rest| rest.starts_with('='))) else {
        return Ok(None);
    };
    let option = args.remove(at);
    let given = match option.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')) {
        Some(given) => given.to_string(),
        None if at < args.len() => args.remove(at),
        None => String::new(),
    };
    let what = if name == "--pool" { "the pool" } else { "the text room" };
    // The runtime prints the size in its messages as a C `int`.
    let most = i32::MAX as u64;
    match given.parse::<u64>() {
        Ok(bytes) if bytes > most => {
            return Err(format!("zeno: `{name}` can be at most {most} bytes, which is 2 GB less a byte (`{given}` is more)"));
        }
        Ok(bytes) if bytes > 0 => {}
        _ => {
            return Err(format!(
                "zeno: `{name}` is followed by the size of {what} in bytes, a whole number above zero, e.g.\n        \
                 zeno build --no-collector {name} 4096 examples/blink.zeno\n      (`{given}` is not one)"
            ));
        }
    }
    if args.iter().any(|arg| arg == name || arg.starts_with(&format!("{name}="))) {
        return Err(format!("zeno: `{name}` is given twice. Give it once, with the size of {what}"));
    }
    Ok(Some(given.parse().expect("checked above")))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    // `zeno run copy.zeno a.txt --strict`: the words after the file of a
    // `run` are the program's own, whatever they look like. Zeno's
    // options go before the file.
    let mut arguments = Vec::new();
    if args.first().is_some_and(|command| command == "run") {
        if let Some(file) = args.iter().skip(1).position(|arg| !arg.starts_with("--")) {
            arguments = args.split_off(file + 2);
        }
    }
    // `--strict` may appear anywhere after the command.
    let strict = args.iter().any(|arg| arg == "--strict");
    args.retain(|arg| arg != "--strict");
    // `--memory` makes a built program report how much memory it used.
    let memory_report = args.iter().any(|arg| arg == "--memory");
    args.retain(|arg| arg != "--memory");
    // `--firmware` builds C that firmware coding standards accept.
    let firmware = args.iter().any(|arg| arg == "--firmware");
    args.retain(|arg| arg != "--firmware");
    // `--no-collector` is a firmware build that also leaves the garbage
    // collector out. (It has to be a firmware build: when an error jumps,
    // it skips the lines that free memory, and with no collector nothing
    // would ever free it.)
    let no_collector = args.iter().any(|arg| arg == "--no-collector");
    args.retain(|arg| arg != "--no-collector");
    // `--vm` runs a program as bytecode.
    let on_vm = args.iter().any(|arg| arg == "--vm");
    args.retain(|arg| arg != "--vm");
    // `--llvm` builds through LLVM IR in place of C.
    let llvm = args.iter().any(|arg| arg == "--llvm");
    args.retain(|arg| arg != "--llvm");
    // `--web` builds a page for a browser.
    let web = args.iter().any(|arg| arg == "--web");
    args.retain(|arg| arg != "--web");
    // `--target js` writes the program in another language (docs/targets.md).
    let mut target: Option<String> = None;
    if let Some(at) = args.iter().position(|arg| arg == "--target" || arg.starts_with("--target=")) {
        let option = args.remove(at);
        target = match option.strip_prefix("--target=") {
            Some(named) => Some(named.to_string()),
            None if at < args.len() => Some(args.remove(at)),
            None => Some(String::new()),
        };
    }
    // `--board boards/mine` builds for a board, as the `zeno.board` in
    // that folder says (docs/board-build.md).
    let mut board: Option<String> = None;
    if let Some(at) = args.iter().position(|arg| arg == "--board" || arg.starts_with("--board=")) {
        let option = args.remove(at);
        board = match option.strip_prefix("--board=") {
            Some(named) => Some(named.to_string()),
            None if at < args.len() => Some(args.remove(at)),
            None => Some(String::new()),
        };
    }
    // `--owned` and `--collector` say how a program frees its memory,
    // over what its `zeno.package` says (docs/memory-modes.md).
    let owned = args.iter().any(|arg| arg == "--owned");
    let collector = args.iter().any(|arg| arg == "--collector");
    args.retain(|arg| arg != "--owned" && arg != "--collector");
    // `--release` builds without overflow checks.
    let release = args.iter().any(|arg| arg == "--release");
    args.retain(|arg| arg != "--release");
    // `--fast` is `--release` with the other checks a correct program
    // never fails left out too, and the C compiler asked to try harder.
    let fast = args.iter().any(|arg| arg == "--fast");
    args.retain(|arg| arg != "--fast");
    // `--minimal` writes the least C that runs the program: no checks,
    // and only the parts of the runtime the program uses.
    let minimal = args.iter().any(|arg| arg == "--minimal");
    args.retain(|arg| arg != "--minimal");
    // `--pool 4096` and `--text-room 2048`: the size of the pool and of
    // the text room of a build with no collector (decision 56.15).
    let sizes = match (size_option(&mut args, "--pool"), size_option(&mut args, "--text-room")) {
        (Ok(pool), Ok(text_room)) => Sizes { pool, text_room },
        (Err(why), _) | (_, Err(why)) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    // `zeno test --build` builds, and takes what a build takes.
    let builds = |args: &[String]| match args.first().map(String::as_str) {
        Some("build") => true,
        Some("test") => args.iter().any(|arg| arg == "--build"),
        _ => false,
    };
    if let Some(option) = sizes.named() {
        if !builds(&args) {
            eprintln!("zeno: `{option}` is an option of `zeno build`: it sets a size of the memory of a program built");
            eprintln!("      with no collector, and no other command builds one");
            return ExitCode::FAILURE;
        }
        if web || target.is_some() {
            eprintln!("zeno: `{option}` sets a size of the memory of a program compiled with no collector. `--web` and");
            eprintln!("      `--target` write the program in another language, which looks after its own memory");
            return ExitCode::FAILURE;
        }
    }
    if owned && collector {
        eprintln!("zeno: `--owned` and `--collector` are two ways to free memory. Choose one");
        return ExitCode::FAILURE;
    }
    if let Some(folder) = &board {
        if !builds(&args) {
            eprintln!("zeno: `--board` is an option of `zeno build`: it says which board a program is built for");
            return ExitCode::FAILURE;
        }
        if folder.is_empty() || folder.starts_with("--") {
            eprintln!("zeno: `--board` is followed by the folder of a board, e.g.");
            eprintln!("        zeno build --board boards/qemu-cortex-m3 examples/blink.zeno");
            return ExitCode::FAILURE;
        }
        if collector {
            eprintln!("zeno: a program for a board is built with no collector, as `--no-collector` builds it.");
            eprintln!("      (The collector is made for a computer: it takes its memory from the system 64 KB");
            eprintln!("      at a time.) `--collector` asks for it. Leave one of the two out");
            return ExitCode::FAILURE;
        }
        if llvm || web || target.is_some() {
            eprintln!("zeno: `--board` hands the program's C to the C compiler of a board. `--llvm`, `--web` and");
            eprintln!("      `--target` write something other than C. Choose one");
            return ExitCode::FAILURE;
        }
    }
    // A board is a chip, and a build for a chip has no collector (decision 51.3).
    let no_collector = no_collector || board.is_some();
    if no_collector && collector {
        eprintln!("zeno: `--no-collector` leaves the collector out, and `--collector` asks for it. Choose one");
        return ExitCode::FAILURE;
    }
    // What the program's own file says, unless the command line says.
    let memory = |path: &str| match (owned, collector) {
        (true, _) => Some(package::Memory::Owned),
        (_, true) => Some(package::Memory::Collector),
        _ => package_memory(path).or_else(|| files_memory(path)),
    };
    let built = |path: &str| {
        let (kind, options) = how_built(no_collector, firmware, release, fast, memory(path));
        let spares_checked = memory_report && !minimal && kind == emit_c::Build::NoCollector;
        (kind, emit_c::Options { minimal, memory_report: minimal && memory_report, spares_checked, ..options })
    };

    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") | Some("help") => {
            usage();
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("zeno {VERSION}");
            ExitCode::SUCCESS
        }
        Some(cmd @ ("tokens" | "parse" | "join" | "bytecode" | "check" | "run")) => {
            let Some(path) = args.get(1) else {
                eprintln!("zeno: `{cmd}` needs a file, e.g. `zeno {cmd} examples/hello.zeno`");
                return ExitCode::FAILURE;
            };
            let done = match cmd {
                "tokens" => tokens(path),
                "parse" => parse(path),
                "join" => join(path),
                "bytecode" => bytecode(path),
                "check" if no_collector || memory(path) == Some(package::Memory::Owned) => {
                    check_no_collector(path).map(|_| println!("No errors found."))
                }
                "check" => check(path, checker::Rules { strict, one_owner: false, firmware: false })
                    .map(|_| println!("No errors found.")),
                _ => run(path, strict, on_vm, arguments, memory(path) == Some(package::Memory::Owned)),
            };
            match done {
                Some(()) => ExitCode::SUCCESS,
                None => ExitCode::FAILURE,
            }
        }
        Some("lsp") => lsp::run(),
        Some("convert") => convert::command(&args[1..]),
        Some("playground") => match playground() {
            Some(()) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        },
        Some("format") => {
            let check = args.iter().any(|arg| arg == "--check");
            args.retain(|arg| arg != "--check");
            if args.len() < 2 {
                eprintln!("zeno: `format` needs a file or a folder, e.g. `zeno format examples/hello.zeno`");
                return ExitCode::FAILURE;
            }
            // A folder stands for the `.zeno` files in it and below it.
            let mut files = Vec::new();
            for path in &args[1..] {
                match Path::new(path).is_dir() {
                    true => zeno_files(Path::new(path), &mut files),
                    false => files.push(path.clone()),
                }
            }
            files.sort();
            // Every file is tried, so that one run says everything.
            let done = files.iter().filter(|path| format(path, check).is_none()).count();
            match done {
                0 => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        Some("test") => {
            // `zeno test`, `zeno test ring.zeno`, `zeno test examples`. A
            // folder stands for the `.zeno` files in it and below it,
            // and no name at all for the folder this is run in.
            let mut named = args[1..].to_vec();
            // `zeno test --only "empty box"`: only the tests that have
            // that in their name.
            let mut only = None;
            if let Some(at) = named.iter().position(|arg| arg == "--only") {
                if at + 1 >= named.len() {
                    eprintln!("zeno: `--only` needs the text to look for in the names of tests, e.g. `zeno test --only \"empty\"`");
                    return ExitCode::FAILURE;
                }
                only = Some(named.remove(at + 1));
                named.remove(at);
            }
            // `zeno test --build`: each file is compiled with its tests,
            // as `zeno build` would compile it, and the program run
            // (decision 60.2).
            let build_tests = match named.iter().position(|arg| arg == "--build") {
                Some(at) => {
                    named.remove(at);
                    if fast || minimal || web || target.is_some() {
                        eprintln!("zeno: `zeno test --build` tests the program as `zeno build` builds it, and takes `--firmware`,");
                        eprintln!("      `--no-collector`, `--llvm`, `--release` and `--board`. `--fast` and `--minimal` leave out");
                        eprintln!("      the checks a test may be written to meet, and `--web` and `--target` build no program");
                        return ExitCode::FAILURE;
                    }
                    Some(TestsBuilt { llvm, board: board.clone(), memory_report, defines: sizes.defines() })
                }
                None => None,
            };
            if named.is_empty() {
                named.push(".".to_string());
            }
            let mut files = Vec::new();
            for path in &named {
                match Path::new(path).is_dir() {
                    true => zeno_files(Path::new(path), &mut files),
                    false => files.push(path.clone()),
                }
            }
            // `./ring.zeno`, found in the folder this is run in, is `ring.zeno`.
            let mut files: Vec<String> =
                files.iter().map(|path| path.strip_prefix("./").unwrap_or(path).to_string()).collect();
            files.sort();
            let asked = |path: &String| named.contains(path);
            // Every file is tried, so that one run says everything.
            let (mut tests, mut failed, mut stopped) = (0, 0, 0);
            for path in &files {
                let tally = match &build_tests {
                    Some(how) => test_built(path, asked(path), only.as_deref(), how, built(path)),
                    None => test(path, strict, asked(path), only.as_deref()),
                };
                match tally {
                    Some(tally) => {
                        tests += tally.tests;
                        failed += tally.failed;
                    }
                    None => stopped += 1,
                }
            }
            // For a board the tests run on the board, which prints the
            // totals of each file itself: here none has been run yet.
            let on_board = build_tests.as_ref().is_some_and(|how| how.board.is_some());
            if !on_board {
                value::print_line("");
                value::print_line(&match (tests, &only) {
                    (0, Some(text)) => format!("No tests were run: none has `{text}` in its name."),
                    (0, None) => "No tests were run.".to_string(),
                    _ => format!("{}: {} passed, {} failed.", error::counted(tests, "test"), tests - failed, failed),
                });
            }
            if stopped > 0 {
                value::print_line(&format!("{} could not be run.", error::counted(stopped, "file")));
            }
            match failed + stopped {
                0 => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        Some("new") => {
            // `zeno new blinker`, `zeno new --package filters`
            let package = args.iter().any(|arg| arg == "--package");
            args.retain(|arg| arg != "--package");
            // An option it does not have is said, wherever it stands.
            let path = match (args.iter().skip(1).find(|arg| arg.starts_with('-')), args.get(1), args.get(2)) {
                (Some(option), _, _) => option,
                (None, Some(path), None) => path,
                (None, Some(_), Some(_)) => {
                    eprintln!("zeno: `new` makes one folder, so it takes one name, e.g. `zeno new blinker`");
                    return ExitCode::FAILURE;
                }
                (None, None, _) => {
                    eprintln!("zeno: `new` needs a name for the program, e.g. `zeno new blinker`");
                    return ExitCode::FAILURE;
                }
            };
            match package::new(Path::new(path), package) {
                Ok(said) => {
                    said.iter().for_each(|line| println!("{line}"));
                    ExitCode::SUCCESS
                }
                Err(why) => {
                    eprintln!("zeno: {why}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("fetch") => {
            // `zeno fetch`, `zeno fetch --update`, `zeno fetch examples/thermostat`
            let update = args.iter().any(|arg| arg == "--update");
            args.retain(|arg| arg != "--update");
            let folder = args.get(1).map(String::as_str).unwrap_or("");
            match package::fetch(Path::new(folder), update) {
                Ok(said) => {
                    said.iter().for_each(|line| println!("{line}"));
                    ExitCode::SUCCESS
                }
                Err(why) => {
                    eprintln!("zeno: {why}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("search") => {
            // `zeno search`, `zeno search filter noise`
            if let Some(option) = args.iter().skip(1).find(|arg| arg.starts_with('-')) {
                eprintln!("zeno: `search` has no option `{option}`. It takes words to look for, as in `zeno search filter`");
                return ExitCode::FAILURE;
            }
            said(registry::search(&args[1..], Path::new(""), None))
        }
        Some("publish") => {
            // `zeno publish`, `zeno publish v1.2.0`
            match (args.iter().skip(1).find(|arg| arg.starts_with('-')), args.get(2)) {
                (Some(option), _) => {
                    eprintln!("zeno: `publish` has no option `{option}`. It is run in the folder of a package, as `zeno publish`");
                    ExitCode::FAILURE
                }
                (None, Some(_)) => {
                    eprintln!("zeno: `publish` takes one version at most, as in `zeno publish v1.2.0`");
                    ExitCode::FAILURE
                }
                (None, None) => said(registry::publish(Path::new(""), args.get(1).map(String::as_str), None)),
            }
        }
        Some("registry") => {
            // `zeno registry accept . https://github.com/someone/zeno-units v1.2.0 --owner someone`
            let owner = args.iter().position(|arg| arg == "--owner").and_then(|at| {
                let owner = args.get(at + 1).cloned();
                args.drain(at..(at + 2).min(args.len()));
                owner
            });
            match (args.get(1).map(String::as_str), &args[2.min(args.len())..], owner) {
                (Some("accept"), [folder, address, version], Some(owner)) => said(registry::accept(Path::new(folder), address, version, &owner)),
                (Some("index"), [folder], None) => said(registry::relist(Path::new(folder))),
                _ => {
                    eprintln!("zeno: `registry` is for whoever keeps a registry, and does two things (docs/registry.md):");
                    eprintln!("        zeno registry accept <folder> <address> <version> --owner <who>");
                    eprintln!("      takes that version of the package at that address into the clone of the");
                    eprintln!("      registry in that folder, for the owner named");
                    eprintln!("        zeno registry index <folder>");
                    eprintln!("      writes the registry's `index.json` afresh, after a change made by hand");
                    ExitCode::FAILURE
                }
            }
        }
        Some("build") => {
            let Some(path) = args.get(1) else {
                eprintln!("zeno: `build` needs a file, e.g. `zeno build examples/hello.zeno`");
                return ExitCode::FAILURE;
            };
            // A page is not compiled, so the options that say how to
            // compile have nothing to say about it.
            if web && (llvm || memory_report || firmware || no_collector || owned || release || fast || minimal) {
                eprintln!("zeno: `build --web` writes a page, which the browser runs as `zeno run` would.");
                eprintln!("      `--llvm`, `--firmware`, `--no-collector`, `--owned`, `--release`, `--fast`, `--minimal` and");
                eprintln!("      `--memory` are for a compiled program");
                return ExitCode::FAILURE;
            }
            // The least C is C: LLVM IR has no part in it.
            if minimal && llvm {
                eprintln!("zeno: `--minimal` writes the least C that runs the program, and `--llvm` writes LLVM IR");
                eprintln!("      in place of C. Choose one");
                return ExitCode::FAILURE;
            }
            if let Some(target) = &target {
                if web || llvm || memory_report || firmware || no_collector || owned || release || fast || minimal {
                    eprintln!("zeno: `--target` writes the program in another language, which builds and runs it.");
                    eprintln!("      `--web`, `--llvm`, `--firmware`, `--no-collector`, `--owned`, `--release`, `--fast`,");
                    eprintln!("      `--minimal` and `--memory` are for a program compiled to machine code");
                    return ExitCode::FAILURE;
                }
                return match build_target(path, target) {
                    Some(()) => ExitCode::SUCCESS,
                    None => ExitCode::FAILURE,
                };
            }
            let (kind, options) = built(path);
            if let Some(option) = sizes.named() {
                if kind != emit_c::Build::NoCollector {
                    eprintln!("zeno: `{option}` sets a size of the memory of a build with no collector, which is fixed when the");
                    eprintln!("      program is built. Add `--no-collector`, or build for a board with `--board`. A build with");
                    eprintln!("      a collector takes its memory from the system as it needs it");
                    return ExitCode::FAILURE;
                }
                if options.grows {
                    eprintln!("zeno: `{option}` sets a size of the memory of a build with no collector for a chip, which is");
                    eprintln!("      fixed when the program is built. This one is `memory owned` on a computer, whose pool");
                    eprintln!("      and text room grow as the program needs, so there is no size to set. `--no-collector`");
                    eprintln!("      asks for the fixed ones");
                    return ExitCode::FAILURE;
                }
            }
            let defines = sizes.defines();
            let built = match (web, llvm) {
                (true, _) => build_web(path),
                (false, true) => build_llvm(path, memory_report, kind, options, &defines),
                (false, false) => build(path, memory_report, kind, options, board.as_deref(), &defines),
            };
            match built {
                Some(()) => ExitCode::SUCCESS,
                None => ExitCode::FAILURE,
            }
        }
        Some(other) => {
            eprintln!("zeno: unknown command `{other}`\n");
            usage();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of files for one test, which no other test uses.
    fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = env::temp_dir().join(format!("zeno-main-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("a folder")).expect("the folder can be made");
            fs::write(path, text).expect("the file can be written");
        }
        root
    }

    #[test]
    fn the_memory_a_program_asks_for_chooses_the_build() {
        use emit_c::Build::*;
        use package::Memory::*;
        let built = |no_collector, firmware, memory| {
            let (kind, options) = how_built(no_collector, firmware, false, false, memory);
            (kind, options.grows)
        };
        // Nothing said: the collector.
        assert_eq!(built(false, false, None), (Ordinary, false));
        assert_eq!(built(false, false, Some(Collector)), (Ordinary, false));
        assert_eq!(built(false, true, None), (Firmware, false));
        // `memory owned`: no collector, and on a computer the memory grows.
        assert_eq!(built(false, false, Some(Owned)), (NoCollector, true));
        // On a chip (`--firmware`) the pool is fixed, as `--no-collector`.
        assert_eq!(built(false, true, Some(Owned)), (NoCollector, false));
        assert_eq!(built(true, false, None), (NoCollector, false));
        assert_eq!(built(true, false, Some(Owned)), (NoCollector, false));
        assert!(how_built(false, false, true, false, None).1.release);
        let fast = how_built(false, false, false, true, None).1;
        assert!(fast.fast && fast.release);

        let root = folder("memory", &[("zeno.package", "package app\nmemory owned\n"), ("main.zeno", "print(1)\n")]);
        assert_eq!(package_memory(&root.join("main.zeno").to_string_lossy()), Some(Owned));
        let root = folder("no-memory", &[("zeno.package", "package app\n"), ("main.zeno", "print(1)\n")]);
        assert_eq!(package_memory(&root.join("main.zeno").to_string_lossy()), None);
        let root = folder("no-package", &[("main.zeno", "print(1)\n")]);
        assert_eq!(package_memory(&root.join("main.zeno").to_string_lossy()), None);
        assert_eq!(files_memory(&root.join("main.zeno").to_string_lossy()), None);
        // With no `memory` line in the package, the files say: owned when
        // every one of them says `memory owned`.
        let main = "memory owned\nimport parts\nprint(parts.n)\n";
        let root = folder("all-owned", &[("main.zeno", main), ("parts.zeno", "memory owned\nlet n = 1\n")]);
        assert_eq!(files_memory(&root.join("main.zeno").to_string_lossy()), Some(Owned));
        let root = folder("one-owned", &[("main.zeno", main), ("parts.zeno", "let n = 1\n")]);
        assert_eq!(files_memory(&root.join("main.zeno").to_string_lossy()), None);
    }

    #[test]
    fn the_pool_and_the_text_room_are_sized_on_the_command_line() {
        let words = |line: &str| line.split_whitespace().map(String::from).collect::<Vec<_>>();
        // Both ways of writing it, taken out of the words, and handed to
        // the C compiler as the sizes the runtime takes when not told.
        let mut args = words("build --no-collector --pool 2104 --text-room=64 blink.zeno");
        let sizes = Sizes { pool: size_option(&mut args, "--pool").unwrap(), text_room: size_option(&mut args, "--text-room").unwrap() };
        assert_eq!(args, words("build --no-collector blink.zeno"));
        assert_eq!(sizes.defines(), ["-DZENO_POOL=2104", "-DZENO_TEXT_ROOM=64"]);
        assert_eq!(sizes.named(), Some("--pool"));
        let mut args = words("build --no-collector --text-room 64 blink.zeno");
        let sizes = Sizes { pool: size_option(&mut args, "--pool").unwrap(), text_room: size_option(&mut args, "--text-room").unwrap() };
        assert_eq!((sizes.named(), sizes.defines()), (Some("--text-room"), vec!["-DZENO_TEXT_ROOM=64".to_string()]));
        // Not given: nothing, and nothing taken.
        let mut args = words("build --no-collector blink.zeno");
        assert_eq!(size_option(&mut args, "--pool"), Ok(None));
        assert_eq!(args.len(), 3);
        assert_eq!(Sizes { pool: None, text_room: None }.defines(), Vec::<String>::new());
        // A size is a whole number of bytes above zero, given once.
        for (line, said) in [
            ("--pool 0", "a whole number above zero"),
            ("--pool -3", "(`-3` is not one)"),
            ("--pool 4k", "(`4k` is not one)"),
            ("--pool", "(`` is not one)"),
            ("--pool= blink.zeno", "(`` is not one)"),
            ("--pool 2147483648", "can be at most 2147483647 bytes"),
            ("--pool 16 --pool 32", "is given twice"),
            ("--text-room 0", "the size of the text room in bytes"),
        ] {
            let name = if line.starts_with("--pool") { "--pool" } else { "--text-room" };
            let why = size_option(&mut words(line), name).unwrap_err();
            assert!(why.contains(said), "{line}: {why}");
        }
        assert_eq!(size_option(&mut words("--pool 2147483647"), "--pool"), Ok(Some(2147483647)));
    }

    #[test]
    fn base64_is_three_bytes_in_four_letters() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"Z"), "Wg==");
        assert_eq!(base64(b"Ze"), "WmU=");
        assert_eq!(base64(b"Zeno"), "WmVubw==");
        assert_eq!(base64(&[0, 255, 254, 253]), "AP/+/Q==");
    }

    #[test]
    fn the_playground_is_one_page_with_everything_written_in() {
        let page = super::playground_page(b"not really zeno.wasm");
        for marker in ["__WEB_JS__", "__WASM__", "__EXAMPLES__"] {
            assert!(!page.contains(marker), "{marker} was left in the playground");
        }
        // Two scripts, each closed once (the theme, read before anything
        // is drawn, and the playground): nothing written into them ends
        // one early, not even an example that has a page with a
        // `<style>` of its own.
        assert_eq!(page.matches("</script>").count(), 2);
        assert!(page.contains("import web"));
    }

    #[test]
    fn a_page_carries_the_files_of_its_program_named_from_its_folder() {
        let root = folder(
            "web-files",
            &[
                ("site/main.zeno", "import words\nimport shout.loud\nprint(words.greeting, shout.loud.twice(\"</script>\"))\n"),
                ("site/words.zeno", "let greeting = \"வணக்கம்\"\n"),
                ("site/zeno.package", "package site\n\nneeds shout in \"../shout\"\n"),
                ("shout/zeno.package", "package shout\n"),
                ("shout/loud.zeno", "function twice(word: str) -> str { return word + word }\n"),
                ("site/unused.zeno", "print(\"never read\")\n"),
            ],
        );
        let files = files_of(&root.join("site/main.zeno").to_string_lossy()).expect("the files");
        let json::Json::Object(members) = json::Json::parse(&files).expect("JSON") else { panic!("an object") };
        assert_eq!(members[0], ("main".to_string(), json::Json::Text("main.zeno".to_string())));
        let json::Json::Object(carried) = &members[1].1 else { panic!("the files") };
        let mut names: Vec<&str> = carried.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["../shout/loud.zeno", "../shout/zeno.package", "main.zeno", "words.zeno", "zeno.package"]);
        // Nothing in it says where on this machine the program is.
        assert!(!files.contains(&modules::shown(&root)), "{files}");

        // The browser is given the same files and joins them the same way.
        let read = |path: &str| carried.iter().find(|(name, _)| name == path).and_then(|(_, text)| text.as_str().map(String::from));
        let loaded = modules::load("main.zeno", read("main.zeno").expect("the first file"), &read);
        let program = loaded.program.unwrap_or_else(|errors| panic!("{}", errors[0].message));
        let (program, errors, _) = checker::checked(program, checker::Rules::default());
        assert!(errors.is_empty(), "{}", errors[0].message);
        let (output, result) = interpreter::run_with(&program, true, value::Outside::default());
        assert!(result.is_ok(), "{:?}", result.err().map(|error| error.message));
        assert_eq!(output, "வணக்கம் </script></script>\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_program_goes_last_in_the_body_of_its_page() {
        let program = r#"{"main":"a.zeno","files":{"a.zeno":"print(\"</script><b>\")"}}"#;
        let page = "<html><BODY>\n<h1 id=\"count\"></h1>\n</BODY></html>\n";
        let built = with_program(page, program, b"Zeno");
        let (before, after) = built.split_once("<script>").expect("a script");
        assert_eq!(before, "<html><BODY>\n<h1 id=\"count\"></h1>\n");
        assert!(after.ends_with("</script>\n</BODY></html>\n"), "{after}");
        // The program cannot end the script it is written in.
        let packed = base64(&deflate::pack(b"Zeno"));
        let start = r#"<script>Zeno.start(Zeno.unpacked("PACKED"), {"main":"a.zeno","files":{"a.zeno":"print(\"LESS/script>LESSb>\")"}});</script>"#
            .replace("LESS", "\\u003c")
            .replace("PACKED", &packed);
        assert!(built.contains(&start), "{}", &built[built.len() - 300..]);
        assert_eq!(built.matches("</script>").count(), 2);

        // A page with no `</body>` gets it at its end.
        let built = with_program("<p>Hello</p>\n", program, b"Zeno");
        assert!(built.starts_with("<p>Hello</p>\n<script>"), "{built}");
        assert!(DEFAULT_PAGE.contains("id=\"zeno-output\"") && DEFAULT_PAGE.contains("</body>"));
    }

    #[test]
    fn check_with_no_collector_says_what_that_build_would_say() {
        let root = folder(
            "check-no-collector",
            &[
                // The checker passes this, and the compiler refuses it: a
                // list of nothing but `none` says nothing of what else it
                // may hold. (Until 0.8.0 this was a `list[int or none]`,
                // which a build with no collector now takes, 56.14.)
                ("gaps.zeno", "let readings = [none]\nprint(len(readings))\n"),
                ("fine.zeno", "let readings = [1, 2, 3]\nprint(len(readings))\n"),
            ],
        );
        let at = |name: &str| root.join(name).display().to_string();
        assert!(check(&at("gaps.zeno"), checker::Rules { strict: true, one_owner: true, firmware: true }).is_some());
        assert!(check_no_collector(&at("gaps.zeno")).is_none());
        assert!(build(&at("gaps.zeno"), false, emit_c::Build::NoCollector, emit_c::Options::default(), None, &[]).is_none());
        assert!(check_no_collector(&at("fine.zeno")).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zeno_test_runs_the_tests_of_each_file_in_its_own_program() {
        let root = folder(
            "tests",
            &[
                ("app/main.zeno", "import sensors\nprint(sensors.scale(1))\n"),
                ("app/sensors.zeno", "function scale(n: int) -> int {\n    return n * 2\n}\n\ntest \"scale\" {\n    assert scale(3) == 6\n}\n"),
                // A file in a folder below imports as it does in the
                // program: from the folder that has `main.zeno`.
                (
                    "app/drivers/uart.zeno",
                    "import sensors\n\ntest \"one\" {\n    assert sensors.scale(2) == 4\n}\n\ntest \"two\" {\n    assert sensors.scale(2) == 5\n}\n",
                ),
                ("app/broken.zeno", "test \"x\" {\n    assert nothing_here == 1\n}\n"),
                ("app/stops.zeno", "print(1 // 0)\n\ntest \"x\" {\n    assert true\n}\n"),
                ("alone/one.zeno", "test \"alone\" {\n    assert 1 + 1 == 2\n}\n"),
            ],
        );
        let at = |path: &str| root.join(path).display().to_string();
        assert_eq!(program_folder(&at("app/drivers/uart.zeno")), root.join("app"));
        assert_eq!(program_folder(&at("alone/one.zeno")), root.join("alone"));

        let counts = |path: &str| test(&at(path), false, true, None).map(|tally| (tally.tests, tally.failed));
        assert_eq!(counts("app/sensors.zeno"), Some((1, 0)));
        assert_eq!(counts("app/drivers/uart.zeno"), Some((2, 1)));
        assert_eq!(counts("alone/one.zeno"), Some((1, 0)));
        // A file with no test is not run, and is no failure.
        assert_eq!(counts("app/main.zeno"), Some((0, 0)));
        // A mistake in the file, or an error before its tests: none is run.
        assert_eq!(counts("app/broken.zeno"), None);
        assert_eq!(counts("app/stops.zeno"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zeno_new_makes_a_program_that_runs_and_a_package_it_can_need() {
        let root = folder("new", &[("taken/mine.txt", "keep me\n")]);
        let at = |path: &str| root.join(path).display().to_string();
        let text = |path: &str| fs::read_to_string(root.join(path)).expect("the file is there");
        let files = |folder: &str| {
            let mut names: Vec<String> =
                fs::read_dir(root.join(folder)).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect();
            names.sort();
            names
        };
        // What a program prints, and how many of its tests pass and fail.
        let run = |path: &str| {
            let (_, program) = check(&at(path), checker::Rules { strict: true, one_owner: false, firmware: false }).expect("no mistake");
            interpreter::run_tests(&program, true, value::Outside::started_with(Vec::new()), None).0
        };
        let counts = |path: &str| test(&at(path), true, true, None).map(|tally| (tally.tests, tally.failed));

        // A program: its name, where it starts, and a module with a test.
        let said = package::new(&root.join("blinker"), false).expect("it is made");
        assert_eq!(said.len(), 5, "{said:?}");
        assert!(said[0].starts_with("Made the program `blinker` in ") && said[4].starts_with("Run it with `zeno run "), "{said:?}");
        assert_eq!(files("blinker"), ["greeting.zeno", "main.zeno", "zeno.package"]);
        let manifest = package::parse(&text("blinker/zeno.package"), 0).expect("it is read");
        assert_eq!((manifest.name.as_str(), manifest.needs.len()), ("blinker", 0));
        assert_eq!(run("blinker/main.zeno"), "Hello, world, from blinker.\n");
        assert_eq!(counts("blinker/greeting.zeno"), Some((1, 0)));
        // It is checked as a build with no collector checks, too.
        assert!(check(&at("blinker/main.zeno"), checker::Rules { strict: true, one_owner: true, firmware: true }).is_some());

        // A package: its name, and the module that has the name.
        let said = package::new(&root.join("filters"), true).expect("it is made");
        assert_eq!(said.len(), 4, "{said:?}");
        assert!(said[0].starts_with("Made the package `filters` in ") && said[3].starts_with("Run its tests with `zeno test "), "{said:?}");
        assert_eq!(files("filters"), ["filters.zeno", "zeno.package"]);
        assert_eq!(package::parse(&text("filters/zeno.package"), 0).expect("it is read").name, "filters");
        assert_eq!(counts("filters/filters.zeno"), Some((1, 0)));

        // Each file is laid out as `zeno format` lays it out.
        for file in ["blinker/main.zeno", "blinker/greeting.zeno", "filters/filters.zeno"] {
            assert_eq!(format::format(&text(file)).expect("it is read"), text(file), "{file}");
        }

        // The program can need the package, as the package's file says.
        fs::write(root.join("blinker/zeno.package"), "package blinker\nneeds filters in \"../filters\"\n").unwrap();
        fs::write(root.join("blinker/main.zeno"), "import filters\n\nprint(filters.hello(\"blinker\"))\n").unwrap();
        assert_eq!(run("blinker/main.zeno"), "Hello, blinker, from filters.\n");

        // A folder is made as deep as the path says.
        package::new(&root.join("deep/down/thing"), false).expect("it is made");
        assert_eq!(run("deep/down/thing/main.zeno"), "Hello, world, from thing.\n");

        // What cannot be made says why, and nothing is written.
        let refused = |path: &str, package: bool| package::new(&root.join(path), package).expect_err("it is refused");
        assert_eq!(
            refused("my-app", false),
            "`my-app` cannot be the name of a program. A name is letters, digits and `_`, and does not start with a digit, \
             as every name in a Zeno program is. Try `my_app`"
        );
        assert!(refused("2fast", true).ends_with("Try `my_2fast`"));
        assert_eq!(refused("while", false), "`while` is a word of the language, so a program cannot have that name");
        assert_eq!(refused("math", true), "`math` is a module that comes with Zeno, so a package cannot have that name");
        assert!(refused("--lib", false).starts_with("`zeno new` has no option `--lib`."));
        assert!(refused("taken", false).ends_with("`zeno new` makes a new folder, and writes nothing into one that is there"));
        assert_eq!(files("taken"), ["mine.txt"]);
        assert!(refused("blinker", true).contains("here already"));
        for gone in ["my-app", "2fast", "while", "math", "--lib"] {
            assert!(!root.join(gone).exists(), "{gone}");
        }
        assert!(package::new(Path::new(""), false).unwrap_err().starts_with("`zeno new` makes a new folder, so it needs a name"));
        let _ = fs::remove_dir_all(&root);
    }
}
