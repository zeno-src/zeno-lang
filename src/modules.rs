//! Programs in several files.
//!
//! `import shapes` at the top of a file brings in the file `shapes.zeno`
//! from the folder of the program's first file, and `shapes.area(2.0)`
//! then calls the `area` that file declares. `from shapes import area`
//! brings in the one name, to be written as plain `area`. These are the
//! two forms that `import math` and `from math import sqrt` already have
//! for the modules that come with Zeno.
//!
//! This stage runs between the parser and the type checker, and it is the
//! only one that knows a program can be several files. It does three
//! things:
//!
//! 1. **Load.** Read the first file, find its imports, read those files,
//!    find their imports, and so on. Each file is read once however many
//!    files import it. Files that import each other are refused.
//! 2. **Rename.** Give every name that an imported file declares at its
//!    top the file's module name in front: `area` in `shapes.zeno` becomes
//!    `shapes.area`, in the file itself and in every file that uses it.
//!    Two files can then each have an `area` without meeting.
//! 3. **Join.** Put the statements of all the files into one program, an
//!    imported file always before the file that imports it.
//!
//! A program can also take modules from a **package**: another folder of
//! `.zeno` files, with a `zeno.package` in it that gives the package its
//! name. The program's own `zeno.package` says which packages it needs
//! (`package.rs` reads that file). `import filters.lowpass` then reads
//! `lowpass.zeno` from the folder of the package `filters`. Loading is
//! the only step that knows about packages: it works out which file an
//! import names, and gives the module its name in the whole program.
//!
//! What comes out is one ordinary program. The type checker, the
//! interpreter, the VM and both compilers take it as they take a program
//! written in one file, and need no changes to know about modules. A name
//! with a dot in it, like `shapes.area`, is to them one name like any
//! other. Nobody can write such a name as one word in a source file, so
//! it can never collide with a name somebody did write.
//!
//! See docs/modules.md for a walk through this file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ast::*;
use crate::builtins;
use crate::error::Error;
use crate::lexer;
use crate::library;
use crate::package;
use crate::parser;
use crate::token::{Span, LINES_A_FILE, MOST_FILES};

/// One source file of a program.
pub struct File {
    /// Where the file is, as it is shown in an error message.
    pub path: String,
    pub source: String,
}

/// What loading a program gives.
pub struct Loaded {
    /// Every file that was read. The first is the file the program starts
    /// in. A `Span` says which of these it is in: see `Span::file`.
    pub files: Vec<File>,
    /// The files joined into one program, or the mistakes that kept them
    /// from being joined.
    pub program: Result<Program, Vec<Error>>,
}

/// Read the program that starts in the file at `path`, whose text is
/// `source`. `read` gives the text of another file, or `None` if there is
/// no such file. (The tests hand in files that exist only in memory.)
pub fn load(path: &str, source: String, read: &dyn Fn(&str) -> Option<String>) -> Loaded {
    load_in(Path::new(path).parent().unwrap_or(Path::new("")), path, source, read)
}

/// The same, for a file that is not the one its program starts in:
/// `folder` is the program's folder, which `load` takes to be the
/// folder of the file. The language server checks whichever file is
/// open in the editor, and that may be one in a folder below.
pub fn load_in(folder: &Path, path: &str, source: String, read: &dyn Fn(&str) -> Option<String>) -> Loaded {
    let root = tidy(folder);
    let mut loader = Loader {
        read,
        packages: vec![Package { name: String::new(), folder: root, needs: HashMap::new() }],
        by_name: HashMap::new(),
        files: Vec::new(),
        units: Vec::new(),
        numbers: HashMap::new(),
        paths: HashMap::from([(shown(&tidy(Path::new(path))), String::new())]),
        order: Vec::new(),
        loading: Vec::new(),
        missing: HashSet::new(),
        wanted: Vec::new(),
        parts: HashSet::new(),
        errors: Vec::new(),
    };
    loader.file(String::new(), 0, path.to_string(), source);
    loader.library();
    let program = match loader.errors.is_empty() {
        true => loader.join(),
        false => Err(std::mem::take(&mut loader.errors)),
    };
    Loaded { files: loader.files, program }
}

// ------------------------------------------------------------------- Loading

struct Loader<'a> {
    read: &'a dyn Fn(&str) -> Option<String>,
    /// The packages of the program. The first is the program itself: its
    /// folder is the folder of the program's first file, and a module of
    /// the program is looked for from there, whichever file imports it,
    /// so that one name always means one file.
    packages: Vec<Package>,
    /// The place in `packages` of each package that has a name.
    by_name: HashMap<String, usize>,
    /// Every file that was read: the `.zeno` files, and each
    /// `zeno.package`, so that a mistake in one can be shown too.
    files: Vec<File>,
    /// What was found in each file, in the order of `files`. `None` for a
    /// file that is still being read, that has a mistake in it, or that
    /// is a `zeno.package`.
    units: Vec<Option<Unit>>,
    /// The number of the file each module is.
    numbers: HashMap<String, usize>,
    /// The module each file is, by the file's path. A file is one module
    /// and has one name, however it is reached.
    paths: HashMap<String, String>,
    /// The files in the order their statements go into the program: each
    /// one after the files it imports.
    order: Vec<usize>,
    /// The modules whose imports are being followed right now, the first
    /// file's first. A module that turns up here a second time is
    /// importing itself, directly or through others.
    loading: Vec<String>,
    /// The modules that were looked for and not found, so that each is
    /// reported once however many lines import it.
    missing: HashSet<String>,
    /// The lines that import a module which has a part written in Zeno
    /// (`library.rs`), in a file that spells one of that part's names:
    /// the number of the file, and the line. `Loader::library` reads
    /// the parts these ask for, after every file of the program.
    wanted: Vec<(usize, Import)>,
    /// The numbers of the files that are such parts.
    parts: HashSet<usize>,
    errors: Vec<Error>,
}

/// A folder of modules. The program is one, and so is every package it
/// needs.
struct Package {
    /// What its `zeno.package` calls it. Empty for a program that has no
    /// `zeno.package`.
    name: String,
    folder: PathBuf,
    /// The packages this one needs: where each is in `Loader::packages`,
    /// by the name this package's files import it by.
    needs: HashMap<String, usize>,
}

/// One file, parsed.
struct Unit {
    /// The module's name in the whole program: `shapes`, `drivers.uart`,
    /// or `filters.lowpass` for the module `lowpass` of the package
    /// `filters`. Empty for the program's first file, which nothing
    /// imports and whose names stay as they are written.
    module: String,
    program: Program,
    /// The files this one imports, in the order it lists them.
    imports: Vec<Import>,
}

/// `import shapes` (no names) or `from shapes import area, Point`.
struct Import {
    /// The module as the line writes it.
    module: String,
    /// The module's name in the whole program. The two differ inside a
    /// package: there `import lowpass` is the module `filters.lowpass`.
    global: String,
    names: Vec<String>,
    span: Span,
}

impl Loader<'_> {
    /// Read one file and, before going on, every file it imports.
    /// `package` is the package the file is in.
    fn file(&mut self, module: String, package: usize, path: String, source: String) {
        let number = self.files.len();
        let is_part = self.parts.contains(&number);
        // The first line of this file, for a mistake that is about the
        // file as a whole.
        let top = Span { line: number * LINES_A_FILE + 1, col: 1 };
        let parsed = match source.lines().count() < LINES_A_FILE {
            true => lexer::lex_in(&source, number).and_then(|tokens| parser::parse(tokens).map_err(|error| vec![error])),
            false => Err(vec![Error::new("this file has a million lines or more, and Zeno reads at most 999,999. Split it", top)]),
        };
        // A block of another language is told which file it is in.
        crate::inline::file_is(number, &path);
        self.files.push(File { path, source });
        self.units.push(None);
        let program = match parsed {
            Ok(program) => program,
            Err(errors) => return self.errors.extend(errors),
        };
        if number == 0 {
            // Which packages there are decides what an import means, so
            // they are found before the first import is followed.
            self.packages_needed();
            if !self.errors.is_empty() {
                // With a package missing, an import of one of its
                // modules would only be reported as a second mistake.
                return;
            }
        }

        // The imports of files are the leading lines of the file. An
        // imported file runs before the file that imports it, wherever
        // the import is written, so the top is the one honest place for it.
        let mut imports = Vec::new();
        let mut below = false;
        for statement in &program.statements {
            let StmtKind::Import { module, names } = &statement.kind else {
                below = true;
                continue;
            };
            let module = module.join(".");
            if builtins::comes_with_zeno(&module) {
                // A module that comes with Zeno may have a part that is
                // written in Zeno. It is wanted if this file spells one
                // of the names of that part. (The part itself imports
                // the built-in part of its module, and nothing more.)
                let part = library::part(&module)
                    .filter(|part| !is_part && (!part.built_in_too || part.spelt_in(&self.files[number].source)));
                if part.is_some() {
                    let import = Import { module: module.clone(), global: module, names: names.clone(), span: statement.span };
                    self.wanted.push((number, import));
                }
                continue;
            }
            if below {
                self.errors.push(Error::new(
                    format!(
                        "`import {module}` goes at the top of the file, above everything that is not an import. \
                         The file `{module}` runs first wherever this line is, so the top is where the line belongs"
                    ),
                    statement.span,
                ));
                continue;
            }
            imports.push(Import { module, global: String::new(), names: names.clone(), span: statement.span });
        }

        self.loading.push(module.clone());
        for import in &mut imports {
            let span = import.span;
            let Some(found) = self.find(package, &import.module, span) else { continue };
            import.global = found.module.clone();
            if let Some(first) = self.loading.iter().position(|loading| *loading == found.module) {
                let mut circle: Vec<&str> = self.loading[first..].iter().map(String::as_str).collect();
                circle.push(&found.module);
                self.errors.push(Error::new(in_a_circle(&circle), span));
                continue;
            }
            if let Some(&known) = self.numbers.get(&found.module) {
                // Read already, for another line that imports it. Two
                // packages can still lead to one name by different files.
                let other = &self.files[known].path;
                if *other != found.path {
                    self.errors.push(Error::new(
                        format!(
                            "two files would both be the module `{}` of this program: `{other}` and `{}`. \
                             A module is one file. Give one of the two another name",
                            found.module, found.path
                        ),
                        span,
                    ));
                }
                continue;
            }
            if self.missing.contains(&found.module) {
                continue;
            }
            match self.paths.get(&found.path) {
                Some(first) if first.is_empty() => {
                    self.errors.push(Error::new(
                        format!(
                            "`{}` is the file the program starts in, and no file imports that one. \
                             Move what the files share into a file of its own, and import that",
                            found.path
                        ),
                        span,
                    ));
                    continue;
                }
                Some(other) => {
                    self.errors.push(Error::new(
                        format!(
                            "the file `{}` is already in this program as the module `{other}`, and this line \
                             would make it a second module, `{}`. A file is one module: import it by one name",
                            found.path, found.module
                        ),
                        span,
                    ));
                    continue;
                }
                None => {}
            }
            let Some(source) = (self.read)(&found.path) else {
                self.missing.insert(found.module.clone());
                self.errors.push(Error::new(self.not_found(package, &import.module, &found), span));
                continue;
            };
            if self.files.len() >= MOST_FILES {
                self.errors.push(Error::new(format!("a program can have {MOST_FILES} files, and this would be one more"), span));
                continue;
            }
            self.numbers.insert(found.module.clone(), self.files.len());
            self.paths.insert(found.path.clone(), found.module.clone());
            self.file(found.module, found.package, found.path, source);
        }
        self.loading.pop();

        // Everything this file imports is in `order` by now, so this
        // file comes after all of it.
        self.order.push(number);
        self.units[number] = Some(Unit { module, program, imports });
    }

    /// Read the parts of Zeno's library that are written in Zeno and
    /// that a file of the program asked for (`library.rs`).
    ///
    /// This is done when every file of the program has been read, so
    /// that the parts are the last files. The numbers of the program's
    /// own files, which are in every place a message can point at, are
    /// then what they would be with no such part. In the joined program
    /// the parts still come first: a part imports nothing of the
    /// program's, and what the program's files do at their top may use it.
    fn library(&mut self) {
        let wanted = std::mem::take(&mut self.wanted);
        if wanted.is_empty() || !self.errors.is_empty() {
            return;
        }
        let mut order = Vec::new();
        for part in library::PARTS {
            let Some((_, first)) = wanted.iter().find(|(_, import)| import.module == part.module) else { continue };
            if self.files.len() >= MOST_FILES {
                self.errors.push(Error::new(format!("a program can have {MOST_FILES} files, and this would be one more"), first.span));
                continue;
            }
            let number = self.files.len();
            self.parts.insert(number);
            self.numbers.insert(part.module.to_string(), number);
            self.file(part.module.to_string(), 0, part.path.to_string(), part.source.to_string());
            self.order.retain(|other| *other != number);
            order.push(number);
        }
        order.append(&mut self.order);
        self.order = order;
        for (number, import) in wanted {
            if let Some(unit) = self.units[number].as_mut() {
                unit.imports.push(import);
            }
        }
    }

    /// Rename the names of every file, and join the files into one program.
    fn join(&mut self) -> Result<Program, Vec<Error>> {
        // One for each file, and `None` for a `zeno.package`.
        let mut units: Vec<Option<Unit>> = std::mem::take(&mut self.units);

        // What each module declares at its top: these are the names
        // another file can ask it for.
        let mut declared: HashMap<String, Vec<String>> = HashMap::new();
        // And what each keeps to itself with `private`.
        let mut hidden: HashMap<String, Vec<String>> = HashMap::new();
        let mut cases: HashSet<String> = HashSet::new();
        // The cases of every enum, by the enum's full name.
        let mut enums: HashMap<String, Vec<String>> = HashMap::new();
        for unit in units.iter().flatten() {
            let full = |name: &str| match unit.module.is_empty() {
                true => name.to_string(),
                false => format!("{}.{name}", unit.module),
            };
            for statement in &unit.program.statements {
                if let StmtKind::Enum(decl) = &statement.kind {
                    enums.insert(full(&decl.name), decl.variants.iter().map(|case| case.name.clone()).collect());
                }
            }
            if unit.module.is_empty() {
                continue;
            }
            let private = private_names(&unit.program);
            let (mut names, mut kept) = (Vec::new(), Vec::new());
            for (name, _, is_case) in declarations(&unit.program) {
                if is_case {
                    cases.insert(full(&name));
                }
                match private.contains(&name) {
                    true => kept.push(name),
                    false => names.push(name),
                }
            }
            declared.insert(unit.module.clone(), names);
            hidden.insert(unit.module.clone(), kept);
        }

        // What the first file declares, and whether each is an enum
        // case. An imported file must not come to mean one of these by
        // an accident of spelling: see `Renamer::plain`.
        let first = units[0].as_ref().expect("the first file was read");
        let strict = first.program.strict;
        let memory = first.program.memory;
        let private = first.program.private.clone();
        let first_file: HashMap<String, (Span, bool)> =
            declarations(&first.program).into_iter().map(|(name, span, is_case)| (name, (span, is_case))).collect();

        let mut errors = Vec::new();
        // What the program's own files name of the library's parts.
        let mut named: Vec<String> = Vec::new();
        // For a part: what each of its functions names, and what the
        // rest of the file names, with the names it takes to be Zeno's
        // own that the program's first file has given another meaning.
        let mut by_function: HashMap<String, Names> = HashMap::new();
        let mut by_part: HashMap<String, Names> = HashMap::new();
        for (number, unit) in units.iter_mut().enumerate() {
            let Some(unit) = unit else { continue };
            let is_part = self.parts.contains(&number);
            let mut renamer = Renamer::new(&unit.module, &declared, &hidden, &cases, &enums, &first_file);
            renamer.is_part = is_part;
            renamer.imports(&unit.imports);
            renamer.own(&declarations(&unit.program));
            for statement in &mut unit.program.statements {
                renamer.statement(statement, true);
                if is_part {
                    let names = std::mem::take(&mut renamer.names);
                    match &statement.kind {
                        StmtKind::Function(function) => drop(by_function.insert(function.name.clone(), names)),
                        // A class is joined only if it is reached, as a
                        // function is: `files.Reader` (decision 59.6).
                        StmtKind::Class(class) => drop(by_function.insert(class.name.clone(), names)),
                        _ => by_part.entry(unit.module.clone()).or_default().add(names),
                    }
                }
            }
            named.append(&mut renamer.names.used);
            errors.extend(renamer.errors);
            // The imports of files have done their work. So has the
            // import of a module that is all written in Zeno, and of a
            // name that is: the checker is shown the built-in ones.
            unit.program.statements.retain_mut(|statement| {
                let StmtKind::Import { module, names } = &mut statement.kind else { return true };
                let module = module.join(".");
                if !builtins::comes_with_zeno(&module) || library::all_in_zeno(&module) {
                    return false;
                }
                let Some(in_zeno) = declared.get(&module).filter(|_| !is_part) else { return true };
                let whole = names.is_empty();
                names.retain(|name| !in_zeno.contains(name));
                whole || !names.is_empty()
            });
        }

        // Which functions and classes of the parts the program reaches: the ones
        // its files name, the ones those name, and so on. A part that
        // has one of its names reached is in the program, with all that
        // is not a function, and what that names is reached too.
        let part_modules: HashSet<String> = units
            .iter()
            .enumerate()
            .filter(|(number, _)| self.parts.contains(number))
            .filter_map(|(_, unit)| unit.as_ref().map(|unit| unit.module.clone()))
            .collect();
        let mut reached: HashSet<String> = HashSet::new();
        let mut parts_in: HashSet<String> = HashSet::new();
        let mut meanings: Vec<(String, String)> = Vec::new();
        while let Some(name) = named.pop() {
            let Some((module, _)) = name.rsplit_once('.') else { continue };
            if !part_modules.contains(module) || !reached.insert(name.clone()) {
                continue;
            }
            let rest = by_part.get(module).filter(|_| !parts_in.contains(module));
            parts_in.insert(module.to_string());
            for names in by_function.get(&name).into_iter().chain(rest) {
                named.extend(names.used.iter().cloned());
                meanings.extend(names.taken.iter().map(|taken| (name.clone(), taken.clone())));
            }
        }
        // A name the part takes to be Zeno's own, such as `len`, would
        // mean the program's `len` once the two are joined.
        meanings.sort();
        meanings.dedup_by(|a, b| a.1 == b.1);
        for (user, taken) in meanings {
            let (span, _) = first_file[&taken];
            errors.push(Error::new(
                format!(
                    "this program uses `{user}`, which is written in Zeno and uses the `{taken}` that comes with \
                     Zeno. So the program cannot give `{taken}` a meaning of its own here. Give this one another name"
                ),
                span,
            ));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        for (number, unit) in units.iter_mut().enumerate() {
            let Some(unit) = unit.as_mut().filter(|_| self.parts.contains(&number)) else { continue };
            let is_in = parts_in.contains(&unit.module);
            unit.program.statements.retain(|statement| match &statement.kind {
                StmtKind::Function(function) => reached.contains(&function.name),
                StmtKind::Class(class) => reached.contains(&class.name),
                _ => is_in,
            });
        }
        // A part that was read and that the program does not use after
        // all is taken off the end again, so that the program is the
        // one it would have been.
        while self.parts.contains(&(units.len() - 1)) && units.last().is_some_and(|unit| unit.as_ref().is_some_and(|unit| !parts_in.contains(&unit.module))) {
            let number = units.len() - 1;
            units.pop();
            self.files.pop();
            self.parts.remove(&number);
            self.order.retain(|other| *other != number);
        }

        // Two files can each import a module that comes with Zeno. In
        // the one program they are joined into, it is imported once.
        let mut statements: Vec<Stmt> = Vec::new();
        let mut whole: HashSet<String> = HashSet::new();
        let mut by_name: HashSet<(String, String)> = HashSet::new();
        for &number in &self.order {
            let unit = units[number].as_mut().expect("a file in the order was read");
            let (mut whole_here, mut by_name_here) = (Vec::new(), Vec::new());
            unit.program.statements.retain_mut(|statement| {
                let StmtKind::Import { module, names } = &mut statement.kind else { return true };
                let module = module.join(".");
                if names.is_empty() {
                    whole_here.push(module.clone());
                    return !whole.contains(&module);
                }
                names.retain(|name| {
                    by_name_here.push((module.clone(), name.clone()));
                    !by_name.contains(&(module.clone(), name.clone()))
                });
                !names.is_empty()
            });
            whole.extend(whole_here);
            by_name.extend(by_name_here);
            statements.append(&mut unit.program.statements);
        }
        let modules = units
            .iter()
            .zip(&self.files)
            .skip(1)
            .map(|(unit, file)| ModuleFile {
                path: file.path.clone(),
                strict: unit.as_ref().is_some_and(|unit| unit.program.strict),
                memory: unit.as_ref().and_then(|unit| unit.program.memory),
            })
            .collect();
        // The `Resource` that comes with Zeno, for a program that uses it
        // (decision 59.1).
        Ok(Program { strict, statements, private, modules, memory }.with_resource())
    }
}

// ------------------------------------------------------------------ Packages

/// The module an import line names.
struct Found {
    /// Its name in the whole program.
    module: String,
    /// The package it is in.
    package: usize,
    /// Its file.
    path: String,
}

impl Loader<'_> {
    /// Read the program's `zeno.package`, if it has one, and through it
    /// the `zeno.package` of every package the program needs.
    fn packages_needed(&mut self) {
        let path = shown(&self.packages[0].folder.join(package::FILE));
        let Some(source) = (self.read)(&path) else {
            return; // A program needs no `zeno.package` until it needs a package.
        };
        let Some(manifest) = self.manifest(path, source) else { return };
        self.packages[0].name = manifest.name.clone();
        self.by_name.insert(manifest.name, 0);
        for need in manifest.needs {
            self.need(0, need);
        }
    }

    /// Read one `zeno.package`. It becomes one of the program's files,
    /// so that a mistake in it is shown as a mistake in any file is.
    fn manifest(&mut self, path: String, source: String) -> Option<package::Manifest> {
        let number = self.files.len();
        let parsed = package::parse(&source, number);
        self.files.push(File { path, source });
        self.units.push(None);
        match parsed {
            Ok(manifest) => Some(manifest),
            Err(errors) => {
                self.errors.extend(errors);
                None
            }
        }
    }

    /// One `needs` line of the package `by`: find the package it names.
    fn need(&mut self, by: usize, need: package::Need) {
        let folder = match &need.source {
            package::Source::Folder(path) => tidy(&self.packages[by].folder.join(path)),
            // Fetched packages are all kept in one folder of the program,
            // whichever package needs them.
            package::Source::Git { .. } | package::Source::Registry { .. } => {
                self.packages[0].folder.join(package::FOLDER).join(&need.name)
            }
        };
        let name = &need.name;
        if let Some(&known) = self.by_name.get(name) {
            let other = &self.packages[known].folder;
            if *other == folder {
                self.packages[by].needs.insert(need.name, known);
            } else {
                self.errors.push(Error::new(
                    format!(
                        "two packages called `{name}` are needed: the one in `{}` and the one in `{}`. \
                         A program has one package of each name. Make the `needs` lines agree",
                        shown(other),
                        shown(&folder)
                    ),
                    need.span,
                ));
            }
            return;
        }
        let path = shown(&folder.join(package::FILE));
        let Some(source) = (self.read)(&path) else {
            let message = match &need.source {
                package::Source::Folder(written) => format!(
                    "there is no package in `{written}`. This line reads the file `{path}`, and there is no such file"
                ),
                package::Source::Git { .. } | package::Source::Registry { .. } => format!(
                    "the package `{name}` has not been fetched: there is no file `{path}`. \
                     Run `zeno fetch` in the folder of the program"
                ),
            };
            return self.errors.push(Error::new(message, need.span));
        };
        if self.files.len() >= MOST_FILES {
            return self.errors.push(Error::new(format!("a program can have {MOST_FILES} files, and this would be one more"), need.span));
        }
        let Some(theirs) = self.manifest(path, source) else { return };
        if theirs.name != *name {
            return self.errors.push(Error::new(
                format!(
                    "the package in `{}` is called `{}`, not `{name}`. Write `needs {} ...`",
                    shown(&folder),
                    theirs.name,
                    theirs.name
                ),
                need.span,
            ));
        }
        let number = self.packages.len();
        self.packages.push(Package { name: theirs.name.clone(), folder, needs: HashMap::new() });
        self.by_name.insert(theirs.name, number);
        self.packages[by].needs.insert(need.name, number);
        for need in theirs.needs {
            self.need(number, need);
        }
    }

    /// What `import a.b` names, written in a file of the package `from`.
    ///
    /// If `a` is a package that `from` needs, it is the module `b` of
    /// that package: the file `b.zeno` in the package's folder. `import a`
    /// with nothing after it is the module that has the package's own
    /// name, `a.zeno` in that folder. Otherwise it is `a/b.zeno` in the
    /// folder of `from` itself.
    fn find(&mut self, from: usize, written: &str, span: Span) -> Option<Found> {
        let parts: Vec<&str> = written.split('.').collect();
        let file = |folder: &Path, parts: &[&str]| {
            let mut path = folder.to_path_buf();
            parts.iter().for_each(|part| path.push(part));
            format!("{}.zeno", shown(&path))
        };
        let own = file(&self.packages[from].folder, &parts);
        let Some(&needed) = self.packages[from].needs.get(parts[0]) else {
            return Some(Found { module: self.full(from, written), package: from, path: own });
        };
        let name = parts[0];
        if parts.len() == 2 && parts[1] == name {
            self.errors.push(Error::new(
                format!(
                    "the module of the package `{name}` that has the package's own name is imported as \
                     `import {name}`. Write that"
                ),
                span,
            ));
            return None;
        }
        let inside = if parts.len() == 1 { &parts[..] } else { &parts[1..] };
        let path = file(&self.packages[needed].folder, inside);
        if path != own && (self.read)(&own).is_some() {
            self.errors.push(Error::new(
                format!(
                    "`import {written}` has two readings here: the file `{own}`, and the file `{path}` of the \
                     package `{name}` that `{}` needs. Give the first another name",
                    package::FILE
                ),
                span,
            ));
            return None;
        }
        Some(Found { module: self.full(needed, &inside.join(".")), package: needed, path })
    }

    /// The name, in the whole program, of the module `module` of a
    /// package. A module of the program itself keeps its name. A module
    /// of another package gets the package's name in front, and the
    /// module with the package's own name is just that name.
    fn full(&self, package: usize, module: &str) -> String {
        let name = &self.packages[package].name;
        match package == 0 || module == name {
            true => module.to_string(),
            false => format!("{name}.{module}"),
        }
    }

    /// The error for an import whose file is not there. `written` is the
    /// module as the line in a file of the package `from` writes it.
    fn not_found(&self, from: usize, written: &str, found: &Found) -> String {
        let path = &found.path;
        let package = &self.packages[found.package].name;
        if found.package == from {
            format!(
                "there is no module `{written}`. `import {written}` reads the file `{path}`, and there is no such file. {}",
                builtins::MODULES
            )
        } else if written == package {
            format!(
                "the package `{package}` has no module of its own name. `import {written}` reads the file `{path}`, \
                 and there is no such file. Import one of its modules by name, as in `import {package}.name`"
            )
        } else {
            format!(
                "the package `{package}` has no module `{}`. `import {written}` reads the file `{path}`, \
                 and there is no such file",
                &written[package.len() + 1..]
            )
        }
    }
}

/// A path as Zeno writes it, in a message and wherever it keeps one as
/// text: with `/` between its parts, on every system.
///
/// Windows writes `\` there and takes `/` as well. A path in a
/// `zeno.package` is written with `/`, so a path made from one and
/// shown as Windows would show it comes out as `lib/filters\zeno.package`.
/// And an editor names a file with `/` on every system, so the file it
/// has open and the file an `import` looks for would be two names for
/// one file. With one way of writing a path there is one name, and a
/// message reads the same wherever it is given. (Elsewhere a `\` can
/// be part of a file's name, so it is only changed on Windows.)
pub fn shown(path: &Path) -> String {
    let path = path.display().to_string();
    if cfg!(windows) { path.replace('\\', "/") } else { path }
}

/// A path with its `.` and `..` steps worked out, as far as the path
/// itself can say: `examples/thermostat/../filters` is `examples/filters`.
/// Two ways of writing one folder then compare as equal.
fn tidy(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut tidy = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if matches!(tidy.components().next_back(), Some(Component::Normal(_))) => {
                tidy.pop();
            }
            other => tidy.push(other),
        }
    }
    tidy
}

/// The error for files that import each other. `circle` begins and ends
/// with the same module: `a`, `b`, `a`.
fn in_a_circle(circle: &[&str]) -> String {
    if circle.len() == 2 {
        return format!("`{}` imports itself. A file cannot import the file it is", circle[0]);
    }
    let steps: Vec<String> = circle.windows(2).map(|pair| format!("`{}` imports `{}`", pair[0], pair[1])).collect();
    let (last, before) = steps.split_last().expect("a circle has a step");
    format!(
        "these files import each other: {}, and {last}. An imported file runs before the file that imports it, \
         and here none of them can be the first. Move what they share into a file of its own, and import that",
        before.join(", ")
    )
}

/// The names a file declares at its top: each with where, and whether it
/// is a case of an enum.
fn declarations(program: &Program) -> Vec<(String, Span, bool)> {
    let mut names = Vec::new();
    for statement in &program.statements {
        let span = statement.span;
        match &statement.kind {
            StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => names.push((name.clone(), span, false)),
            StmtKind::Receive { places, .. } => {
                names.extend(places.iter().filter_map(Place::declared).map(|name| (name.clone(), span, false)))
            }
            StmtKind::Function(function) => names.push((function.name.clone(), span, false)),
            StmtKind::Class(class) => names.push((class.name.clone(), span, false)),
            StmtKind::Enum(decl) => {
                names.push((decl.name.clone(), span, false));
                names.extend(decl.variants.iter().map(|case| (case.name.clone(), span, true)));
            }
            _ => {}
        }
    }
    names
}

/// The names a file keeps to itself: what it declares with `private`,
/// and the cases of an enum it declares so.
fn private_names(program: &Program) -> HashSet<String> {
    let mut names: HashSet<String> = program.private.iter().cloned().collect();
    for statement in &program.statements {
        if let StmtKind::Enum(decl) = &statement.kind {
            if program.private.contains(&decl.name) {
                names.extend(decl.variants.iter().map(|case| case.name.clone()));
            }
        }
    }
    names
}

/// "`area`, `Point` and `origin`".
pub(crate) fn listed(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((only, [])) => only.clone(),
        Some((last, before)) => format!("{} and {last}", before.join(", ")),
    }
}

/// The error for a name that a module does not declare, or declares
/// and keeps to itself.
fn no_such_name(module: &str, name: &str, names: &[String], hidden: &[String]) -> String {
    if hidden.iter().any(|kept| kept == name) {
        return format!(
            "`{name}` is private in the module `{module}`: only its own file can use it. \
             To use it from another file, take the word `private` off where it is declared"
        );
    }
    match names.is_empty() {
        true => format!("module `{module}` has no `{name}`. Its file declares nothing at the top"),
        false => format!("module `{module}` has no `{name}`. It has {}", listed(names)),
    }
}

// ------------------------------------------------------------------ Renaming

/// Walks one file and gives every name that belongs to a module its full
/// name.
///
/// There are three ways a file reaches such a name, and after this walk
/// all three are the one word `shapes.area`:
///
/// - `shapes.area`, after `import shapes`. The parser read that as the
///   field `area` of a variable `shapes`.
/// - `area`, after `from shapes import area`.
/// - `area`, inside `shapes.zeno` itself.
///
/// A name that a function or a block declares for itself hides a name
/// from the top of the file, as it does everywhere in Zeno. So the walk
/// keeps track of what is declared where it is: `scopes`.
struct Renamer<'a> {
    /// The module this file is. Empty for the program's first file.
    module: &'a str,
    /// What every module declares at its top for other files to use.
    declared: &'a HashMap<String, Vec<String>>,
    /// What every module declares at its top and keeps to itself.
    hidden: &'a HashMap<String, Vec<String>>,
    /// The full names of the enum cases that modules declare.
    cases: &'a HashSet<String>,
    /// The cases of every enum of the program, by the enum's full name.
    enums: &'a HashMap<String, Vec<String>>,
    /// What the program's first file declares at its top, and whether
    /// each is an enum case, with where it is declared.
    first_file: &'a HashMap<String, (Span, bool)>,
    /// The modules this file imports whole, with `import shapes`: as
    /// the file writes each, and its name in the whole program.
    whole: HashMap<String, String>,
    /// Every way the name of one of those begins: `drivers` and
    /// `drivers.uart` for `import drivers.uart`.
    starts: HashSet<String>,
    /// What a plain name written in this file stands for: the file's own
    /// declarations (when it is a module), and the names it imports with
    /// `from`.
    top: HashMap<String, String>,
    /// The module each name imported with `from` comes from.
    from: HashMap<String, String>,
    /// The names declared by the functions and blocks around the place
    /// the walk is at, the innermost last.
    scopes: Vec<HashSet<String>>,
    /// True for a part of Zeno's library that is written in Zeno.
    is_part: bool,
    /// What has been named since this was last taken away.
    names: Names,
    errors: Vec<Error>,
}

/// What a stretch of a file names, for `Loader::join` to work out which
/// functions of the library's parts a program reaches.
#[derive(Default)]
struct Names {
    /// The full names, of this file's own declarations and of what it
    /// imports, that were used.
    used: Vec<String>,
    /// In a part of the library: the names it takes to be Zeno's own,
    /// such as `len`, that the program's first file also declares.
    taken: Vec<String>,
}

impl Names {
    fn add(&mut self, mut other: Names) {
        self.used.append(&mut other.used);
        self.taken.append(&mut other.taken);
    }
}

/// True for a module that has a built-in part beside the part of it
/// that is written in Zeno, as `math` has. What the written part does
/// not declare is then left as it is, for the checker.
fn built_in_too(module: &str) -> bool {
    library::part(module).is_some_and(|part| part.built_in_too)
}

impl<'a> Renamer<'a> {
    fn new(
        module: &'a str,
        declared: &'a HashMap<String, Vec<String>>,
        hidden: &'a HashMap<String, Vec<String>>,
        cases: &'a HashSet<String>,
        enums: &'a HashMap<String, Vec<String>>,
        first_file: &'a HashMap<String, (Span, bool)>,
    ) -> Renamer<'a> {
        Renamer {
            module,
            declared,
            hidden,
            cases,
            enums,
            first_file,
            whole: HashMap::new(),
            starts: HashSet::new(),
            top: HashMap::new(),
            from: HashMap::new(),
            scopes: Vec::new(),
            is_part: false,
            names: Names::default(),
            errors: Vec::new(),
        }
    }

    fn error(&mut self, message: impl Into<String>, span: Span) {
        self.errors.push(Error::new(message, span));
    }

    /// Note what the file's imports bring in.
    fn imports(&mut self, imports: &[Import]) {
        for import in imports {
            let (module, global) = (&import.module, &import.global);
            let Some(names) = self.declared.get(global) else { continue };
            if import.names.is_empty() {
                // A file can import `a` and `a.b`: a package's module of
                // its own name beside another of its modules, say. But if
                // the module `a` declares a `b`, then `a.b.x` would have
                // two readings.
                let mut clash = None;
                for (other, other_global) in &self.whole {
                    let (short, short_global, long) = if other.starts_with(&format!("{module}.")) {
                        (module, global, other)
                    } else if module.starts_with(&format!("{other}.")) {
                        (other, other_global, module)
                    } else {
                        continue;
                    };
                    let rest = &long[short.len() + 1..];
                    let next = rest.split('.').next().unwrap_or(rest);
                    if self.declared[short_global].iter().any(|declared| declared == next) {
                        clash = Some(format!(
                            "this file imports `{short}` and `{long}`, and the module `{short}` declares \
                             a `{next}` of its own. `{long}.x` would then have two readings: \
                             `x` of the module `{long}`, and `{rest}.x` of the module `{short}`. \
                             Import one of them with `from`, as in `from {long} import x`"
                        ));
                    }
                }
                if let Some(message) = clash {
                    self.error(message, import.span);
                    continue;
                }
                self.whole.insert(module.clone(), global.clone());
                let mut start = String::new();
                for part in module.split('.') {
                    start = if start.is_empty() { part.to_string() } else { format!("{start}.{part}") };
                    self.starts.insert(start.clone());
                }
            }
            for name in &import.names {
                if !names.contains(name) {
                    if !built_in_too(global) {
                        self.error(no_such_name(module, name, names, &self.hidden[global]), import.span);
                    }
                    continue;
                }
                match self.from.get(name) {
                    Some(other) if other != module => {
                        let other = other.clone();
                        self.error(
                            format!(
                                "`{name}` is imported twice, from `{other}` and from `{module}`. Import one of the \
                                 two modules whole, as in `import {module}`, and write `{module}.{name}`"
                            ),
                            import.span,
                        );
                    }
                    _ => {
                        self.from.insert(name.clone(), module.clone());
                        self.top.insert(name.clone(), format!("{global}.{name}"));
                    }
                }
            }
        }
    }

    /// Note what the file itself declares at its top.
    fn own(&mut self, declarations: &[(String, Span, bool)]) {
        for (name, span, _) in declarations {
            if let Some(module) = self.from.get(name).cloned() {
                self.error(
                    format!(
                        "`{name}` is imported from `{module}` at the top of this file, so the file cannot declare \
                         a second `{name}`. Give this one another name, or import the module whole with \
                         `import {module}` and write `{module}.{name}` for the other"
                    ),
                    *span,
                );
            } else if self.starts.contains(name) {
                self.error(
                    format!(
                        "this file imports the module `{name}`, so it cannot also declare `{name}`: \
                         `{name}.x` would have two readings. Give this one another name"
                    ),
                    *span,
                );
            } else if !self.module.is_empty() {
                self.top.insert(name.clone(), format!("{}.{name}", self.module));
            }
        }
    }

    /// True if a function or a block around here declares `name`.
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains(name))
    }

    fn declare(&mut self, name: &str) {
        self.scopes.last_mut().expect("a scope is open").insert(name.to_string());
    }

    /// A plain name, where one is used: give it its full name if it is a
    /// module's.
    ///
    /// The names of the program's first file stay as they are written,
    /// so in the joined program an imported file could reach one of them
    /// by writing the same word. What `shapes.zeno` means would then
    /// depend on which program imports it. A file sees what it declares
    /// and what it imports, and nothing else, so that is refused here.
    fn plain(&mut self, name: &mut String, span: Span) {
        if self.is_local(name) {
            return;
        }
        if let Some(full) = self.top.get(name.as_str()) {
            *name = full.clone();
            self.names.used.push(full.clone());
        } else if self.is_part && self.first_file.contains_key(name.as_str()) {
            // A part of the library means the `len` that comes with
            // Zeno. `Loader::join` says so at the program's own `len`,
            // if the program reaches the function this is in.
            self.names.taken.push(name.clone());
        } else if !self.module.is_empty() && self.first_file.contains_key(name.as_str()) {
            self.unseen(name, span);
        }
    }

    fn unseen(&mut self, name: &str, span: Span) {
        self.error(
            format!(
                "this file declares no `{name}`. The program's first file has one, and an imported file \
                 cannot see what the file that imports it declares. Hand the value to the function that \
                 needs it, as an argument"
            ),
            span,
        );
    }

    // ----- Statements -----

    /// The statements between `{` and `}`. Functions, classes and enums
    /// can be used above the line that declares them, so their names are
    /// noted before anything in the block is walked.
    fn block(&mut self, block: &mut Block) {
        let mut scope = HashSet::new();
        for statement in block.iter() {
            match &statement.kind {
                StmtKind::Function(function) => drop(scope.insert(function.name.clone())),
                StmtKind::Class(class) => drop(scope.insert(class.name.clone())),
                StmtKind::Enum(decl) => {
                    scope.insert(decl.name.clone());
                    scope.extend(decl.variants.iter().map(|case| case.name.clone()));
                }
                _ => {}
            }
        }
        self.scopes.push(scope);
        for statement in block {
            self.statement(statement, false);
        }
        self.scopes.pop();
    }

    /// `at_top` is true for a statement at the top of the file: what it
    /// declares is the module's, and gets the module's name in front.
    fn statement(&mut self, statement: &mut Stmt, at_top: bool) {
        let span = statement.span;
        let module = self.module;
        let full = |name: &str| format!("{module}.{name}");
        let renamed = at_top && !module.is_empty();
        match &mut statement.kind {
            StmtKind::Let { name, ty, value, .. } | StmtKind::Const { name, ty, value } => {
                if let Some(ty) = ty {
                    self.ty(ty, span);
                }
                // The value is worked out before the name exists, so a
                // `let x = x + 1` in a block reads the `x` outside it.
                self.expr(value);
                if renamed {
                    *name = full(name);
                } else if !at_top {
                    self.declare(name);
                }
            }
            StmtKind::Receive { places, value, .. } => {
                // All of the right is worked out before any place is given
                // its value, so it reads the names outside.
                self.expr(value);
                for place in places.iter_mut() {
                    match place {
                        Place::Name { name, ty } => {
                            if let Some(ty) = ty {
                                self.ty(ty, span);
                            }
                            if renamed {
                                *name = full(name);
                            } else if !at_top {
                                self.declare(name);
                            }
                        }
                        Place::Drop => {}
                        Place::Target(target) => {
                            self.expr(target);
                            if let ExprKind::Name(name) = &target.kind {
                                if let Some((owner, _)) = name.rsplit_once('.').filter(|(owner, _)| *owner != module) {
                                    self.error(
                                        format!(
                                            "`{name}` is a variable of the module `{owner}`, and only the file of `{owner}` \
                                             gives it a new value. Give `{owner}` a function that changes it, and call that"
                                        ),
                                        span,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
                if let ExprKind::Name(name) = &target.kind {
                    if let Some((owner, _)) = name.rsplit_once('.').filter(|(owner, _)| *owner != module) {
                        self.error(
                            format!(
                                "`{name}` is a variable of the module `{owner}`, and only the file of `{owner}` \
                                 gives it a new value. Give `{owner}` a function that changes it, and call that"
                            ),
                            span,
                        );
                    }
                }
            }
            StmtKind::Expr(expr) | StmtKind::Raise(expr) => self.expr(expr),
            StmtKind::Return(value) => value.iter_mut().for_each(|value| self.expr(value)),

            StmtKind::Function(function) => {
                let function = Arc::make_mut(function);
                if renamed {
                    function.name = full(&function.name);
                }
                self.function(function, span);
            }
            StmtKind::Class(class) => {
                if renamed {
                    class.name = full(&class.name);
                }
                // `class Lines[T] is Source[T]`: the `T` after `is` is the class's.
                self.scopes.push(class.type_params.iter().map(|param| param.name.clone()).collect());
                for interface in &mut class.is {
                    self.ty(interface, span);
                }
                for field in &mut class.fields {
                    self.field(field, span);
                }
                for method in &mut class.methods {
                    self.function(Arc::make_mut(method), span);
                }
                self.scopes.pop();
            }
            StmtKind::Enum(decl) => {
                if renamed {
                    decl.name = full(&decl.name);
                }
                // `T` of `enum Maybe[T]` is no name of the file (58.1).
                self.scopes.push(decl.type_params.iter().map(|param| param.name.clone()).collect());
                for case in &mut decl.variants {
                    if renamed {
                        case.name = full(&case.name);
                    }
                    for field in &mut case.fields {
                        self.field(field, span);
                    }
                }
                self.scopes.pop();
            }

            StmtKind::If { condition, then_block, else_block } => {
                self.expr(condition);
                self.block(then_block);
                else_block.iter_mut().for_each(|block| self.block(block));
            }
            StmtKind::While { condition, body } => {
                self.expr(condition);
                self.block(body);
            }
            StmtKind::For { variable, second, iterable, body } => {
                self.expr(iterable);
                self.scopes.push(second.iter().chain([&*variable]).cloned().collect());
                self.block(body);
                self.scopes.pop();
            }
            StmtKind::Match { subject, arms } => {
                self.expr(subject);
                for arm in arms {
                    self.scopes.push(HashSet::new());
                    self.pattern(&mut arm.pattern, span);
                    self.block(&mut arm.body);
                    self.scopes.pop();
                }
            }
            StmtKind::Try { body, handlers } => {
                self.block(body);
                for handler in handlers {
                    if let Some(error) = &mut handler.error {
                        self.named(error, span);
                    }
                    self.scopes.push(handler.binding.iter().cloned().collect());
                    self.block(&mut handler.body);
                    self.scopes.pop();
                }
            }
            StmtKind::Unsafe(body) => self.block(body),
            StmtKind::Test { body, .. } => self.block(body),
            // Made by the checker, which comes after this.
            StmtKind::Scoped { body, .. } => self.block(body),
            StmtKind::Assert(condition, message) => {
                self.expr(condition);
                if let Some(message) = message {
                    self.expr(message);
                }
            }
            StmtKind::AssertRaises { error, body } => {
                self.named(error, span);
                self.block(body);
            }
            StmtKind::Break | StmtKind::Continue => {}

            // At the top of the file `Loader::file` has seen to it.
            StmtKind::Import { module, .. } => {
                let module = module.join(".");
                if !at_top && library::all_in_zeno(&module) {
                    self.error(
                        format!(
                            "`import {module}` goes at the top of the file, not inside a function or a block. \
                             `{module}` is written in Zeno, and is joined to the program as a file of it is"
                        ),
                        span,
                    );
                } else if !at_top && !builtins::comes_with_zeno(&module) {
                    self.error(
                        format!(
                            "`import {module}` goes at the top of the file, not inside a function or a block. \
                             Only a module that comes with Zeno, such as `math`, can be imported in here"
                        ),
                        span,
                    );
                }
            }
        }
    }

    /// A function's types, then its body with the parameters declared.
    fn function(&mut self, function: &mut Function, span: Span) {
        self.scopes.push(function.type_params.iter().map(|param| param.name.clone()).collect());
        for param in &mut function.params {
            if let Some(ty) = &mut param.ty {
                self.ty(ty, span);
            }
            if let Some(default) = &mut param.default {
                self.expr(Arc::make_mut(default));
            }
        }
        if let Some(ty) = &mut function.return_type {
            self.ty(ty, span);
        }
        for error in &mut function.raises {
            self.named(error, span);
        }
        for param in &function.params {
            self.declare(&param.name);
        }
        self.block(&mut function.body);
        self.scopes.pop();
    }

    /// A field of a class, or a value that an enum case carries.
    fn field(&mut self, field: &mut Field, span: Span) {
        if let Some(ty) = &mut field.ty {
            self.ty(ty, span);
        }
        if let Some(default) = &mut field.default {
            self.expr(Arc::make_mut(default));
        }
    }

    fn pattern(&mut self, pattern: &mut Pattern, span: Span) {
        match pattern {
            Pattern::Wildcard | Pattern::Literal(_) => {}
            // `case shapes.Idle`
            Pattern::Name(name) if name.contains('.') => self.case(name, span),
            Pattern::Name(name) => {
                // A plain name is an enum case if it stands for one, and
                // a new variable for the value otherwise.
                let case = self.top.get(name.as_str()).filter(|full| self.cases.contains(*full));
                match case {
                    Some(full) if !self.is_local(name) => *name = full.clone(),
                    _ => {
                        // A case of the first file's would be compared
                        // with, where this file means a new variable.
                        let is_case = self.first_file.get(name.as_str()).is_some_and(|(_, is_case)| *is_case);
                        if is_case && !self.module.is_empty() && !self.is_local(name) {
                            let name = name.clone();
                            self.unseen(&name, span);
                        }
                        self.declare(name);
                    }
                }
            }
            Pattern::Variant { name, fields } => {
                self.case(name, span);
                for field in fields {
                    self.pattern(field, span);
                }
            }
        }
    }

    /// The name of an enum case in a pattern. A case can be written with
    /// its enum in front, `Shape.Circle(r)`, as it can in an expression.
    /// Here that becomes the case's own name, which is what the stages
    /// after this one match by.
    fn case(&mut self, name: &mut String, span: Span) {
        if let Some((owner, case)) = name.rsplit_once('.') {
            let owner = match owner.rsplit_once('.') {
                // `shapes.Shape.Circle`: an enum of a module this file imports.
                Some((module, decl)) => match self.whole.get(module) {
                    Some(global) => format!("{global}.{decl}"),
                    None => owner.to_string(),
                },
                None if self.is_local(owner) => owner.to_string(),
                None => self.top.get(owner).cloned().unwrap_or_else(|| owner.to_string()),
            };
            if self.enums.get(&owner).is_some_and(|cases| cases.iter().any(|known| known == case)) {
                *name = match owner.rsplit_once('.') {
                    Some((module, _)) => format!("{module}.{case}"),
                    None => case.to_string(),
                };
                return;
            }
        }
        self.named(name, span);
    }

    // ----- Names in types -----

    fn ty(&mut self, ty: &mut Type, span: Span) {
        match ty {
            Type::Named { name, args } => {
                self.named(name, span);
                for arg in args {
                    self.ty(arg, span);
                }
            }
            Type::Optional(inner) | Type::Owned(inner) | Type::Take(inner) => self.ty(inner, span),
            Type::Length(_) => {}
            Type::Several(types) => types.iter_mut().for_each(|ty| self.ty(ty, span)),
            Type::Function { params, returns, raises } => {
                for param in params {
                    self.ty(param, span);
                }
                if let Some(returns) = returns {
                    self.ty(returns, span);
                }
                // A kind of error of another file, `shapes.Bad`, is
                // renamed as one after a function's `raises` is.
                if let Some(raises) = raises {
                    self.named(&mut **raises, span);
                }
            }
        }
    }

    /// A name in one of the places where the parser reads `a.b` as one
    /// name: a type, an enum case in a pattern, an error after `except`
    /// or `raises`.
    fn named(&mut self, name: &mut String, span: Span) {
        let Some((module, member)) = name.rsplit_once('.') else {
            return self.plain(name, span);
        };
        // `shapes.Point` has to exist, and gets the name the module has
        // in the whole program.
        match self.whole.get(module).map(|global| (global, &self.declared[global])) {
            Some((global, names)) if names.iter().any(|declared| declared == member) => {
                *name = format!("{global}.{member}");
                // A class of a part named only as a type is reached.
                self.names.used.push(name.clone());
            }
            Some((global, names)) => self.error(no_such_name(module, member, names, &self.hidden[global]), span),
            None => self.error(
                format!(
                    "this file imports no module `{module}`, so `{name}` means nothing here. \
                     Write `import {module}` at the top of the file"
                ),
                span,
            ),
        }
    }

    // ----- Expressions -----

    /// If `expr` is the name of a module this file imports, or the start
    /// of one (`drivers` of `drivers.uart`), that name.
    fn module_named(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Name(name) if self.starts.contains(name) && !self.is_local(name) => Some(name.clone()),
            ExprKind::Field { object, name, .. } => {
                let before = self.module_named(object)?;
                // `drivers.uart` goes on being a module's name as long
                // as this file imports a module that begins so. (Where it
                // imports `drivers` as well, `Renamer::imports` has seen
                // to it that `drivers` declares no `uart` of its own.)
                let longer = format!("{before}.{name}");
                self.starts.contains(&longer).then_some(longer)
            }
            _ => None,
        }
    }

    fn expr(&mut self, expr: &mut Expr) {
        let span = expr.span;
        // `shapes.area`: the parser read a field of a variable `shapes`.
        // It is the name `area` of the module `shapes`.
        if let ExprKind::Field { object, name, .. } = &expr.kind {
            if let Some(module) = self.module_named(object) {
                if self.starts.contains(&format!("{module}.{name}")) {
                    // Still the name of a module: seen to just below.
                } else if let Some(global) = self.whole.get(&module) {
                    let names = &self.declared[global];
                    match names.contains(name) {
                        true => {
                            let full = format!("{global}.{name}");
                            self.names.used.push(full.clone());
                            expr.kind = ExprKind::Name(full);
                        }
                        // `math.sqrt`: built in, and the checker's.
                        false if built_in_too(global) => {}
                        false => self.error(no_such_name(&module, name, names, &self.hidden[global]), span),
                    }
                    return;
                } else {
                    return self.error(
                        format!(
                            "this file imports no module `{module}.{name}`. \
                             Write `import {module}.{name}` at the top of the file"
                        ),
                        span,
                    );
                }
            }
        }
        // A module's name with nothing after it.
        if let Some(module) = self.module_named(expr).filter(|module| !self.whole.get(module).is_some_and(|global| built_in_too(global))) {
            let example = match self.whole.get(&module).and_then(|global| self.declared[global].first()) {
                Some(name) => format!(", as in `{module}.{name}`"),
                None => String::new(),
            };
            return self.error(
                format!("`{module}` is a module, not a value. Write `{module}.` and then a name from it{example}"),
                span,
            );
        }

        match &mut expr.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Inline { .. } | ExprKind::Bool(_) | ExprKind::None => {}
            ExprKind::Name(name) | ExprKind::Give(name) => self.plain(name, span),
            ExprKind::FormatStr(parts) => {
                for part in parts {
                    if let FormatPart::Value { value, .. } = part {
                        self.expr(value);
                    }
                }
            }
            ExprKind::Unit { value, .. } => self.expr(value),
            ExprKind::List(items) | ExprKind::Several(items) => items.iter_mut().for_each(|item| self.expr(item)),
            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key);
                    self.expr(value);
                }
            }
            ExprKind::ListComp { element, variable, iterable, condition } => {
                self.expr(iterable);
                self.scopes.push(HashSet::from([variable.clone()]));
                self.expr(element);
                if let Some(condition) = condition {
                    self.expr(condition);
                }
                self.scopes.pop();
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Binary { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ExprKind::Compare { first, rest } => {
                self.expr(first);
                rest.iter_mut().for_each(|(_, operand)| self.expr(operand));
            }
            ExprKind::Function(function) => self.function(Arc::make_mut(function), span),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                args.iter_mut().for_each(|arg| self.expr(&mut arg.value));
            }
            ExprKind::Field { object, name, .. } => {
                self.expr(object);
                // `Shape.Dot`, where `Shape` is an enum of a module: the
                // case has the module's name in front, like every name
                // the module declares.
                if let ExprKind::Name(owner) = &object.kind {
                    if let Some((module, _)) = owner.rsplit_once('.') {
                        if self.enums.get(owner).is_some_and(|cases| cases.contains(name)) {
                            expr.kind = ExprKind::Name(format!("{module}.{name}"));
                        }
                    }
                }
            }
            ExprKind::Index { object, index } => {
                self.expr(object);
                self.expr(index);
            }
            ExprKind::Take(place) => self.expr(place),
        }
    }
}

// ---------------------------------------------------------------- For tests

/// Programs in several files for the tests of every stage, with no
/// files on disk.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// A test program is one text. A line that begins with `==>` starts
    /// another file, and names it:
    ///
    /// ```text
    /// import shapes
    /// print(shapes.area(2))
    /// ==> shapes.zeno
    /// function area(side: int) -> int { return side * side }
    /// ```
    ///
    /// The text above the first such line is the file the program starts
    /// in, `test.zeno`. A text with no such line is a program in one file.
    pub fn load(source: &str) -> Loaded {
        let mut files: Vec<(String, String)> = vec![("test.zeno".to_string(), String::new())];
        for line in source.split_inclusive('\n') {
            match line.strip_prefix("==> ") {
                Some(name) => files.push((name.trim().to_string(), String::new())),
                None => files.last_mut().expect("there is a file").1 += line,
            }
        }
        let first = files.remove(0).1;
        super::load("test.zeno", first, &|path| {
            files.iter().find(|(name, _)| name == path).map(|(_, source)| source.clone())
        })
    }

    /// The program, joined, as `zeno run` and `zeno build` get it from
    /// `checker::checked`: `count += 1` on a sized number has become
    /// `count = u8(count + 1)`. A program the checker refuses comes
    /// back as it was written. A mistake in a test program that keeps
    /// it from being read stops the test.
    pub fn program(source: &str) -> Program {
        let loaded = load(source);
        match loaded.program {
            Ok(program) => crate::checker::checked(program, crate::checker::Rules::default()).0,
            Err(errors) => panic!("the test program has mistakes: {:?}\n{source}", errors[0].message),
        }
    }

    /// The program as a build gets it: joined, and without its tests.
    pub fn built(source: &str) -> Program {
        program(source).without_tests()
    }

    /// Where a compiled program says an error is: `test.zeno:3:5`, or
    /// `shapes.zeno:2:12` for one in an imported file.
    pub fn place(program: &Program, span: Span) -> String {
        let path = match span.file() {
            0 => "test.zeno",
            file => &program.modules[file - 1].path,
        };
        format!("{path}:{}:{}", span.line_in_file(), span.col)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{load, program};
    use crate::printer;
    use std::cell::RefCell;

    /// The first mistake found in a program, and the file and line it is in.
    fn mistake(source: &str) -> (String, usize, usize) {
        let errors = load(source).program.expect_err("the program should have a mistake");
        (errors[0].message.clone(), errors[0].span.file(), errors[0].span.line_in_file())
    }

    fn error(source: &str) -> String {
        mistake(source).0
    }

    /// A small module that most of these tests import.
    const SHAPES: &str = "==> shapes.zeno\n\
        let sides = 4\n\
        var made = 0\n\
        enum Shape {\n    Dot\n    Square(side: int)\n}\n\
        class Point {\n    x: int\n}\n\
        function area(shape: Shape) -> int {\n    made += 1\n    match shape {\n        case Dot { return 0 }\n        case Square(side) { return side * side }\n    }\n}\n";

    #[test]
    fn a_program_in_one_file_is_left_as_it_is() {
        let source = "import math\nlet x = math.sqrt(4.0)\nfunction twice(n: int) -> int { return n * 2 }\nprint(x, twice(2))\n";
        let alone = crate::parser::parse(crate::lexer::lex(source).unwrap()).unwrap();
        assert_eq!(program(source), alone);
    }

    // ----- The part of the library that is written in Zeno -----

    #[test]
    fn a_part_of_the_library_is_joined_like_a_file_of_the_program() {
        let loaded = load("import math\nprint(math.gcd(12, 18), math.sqrt(4.0))\n");
        let joined = loaded.program.expect("the program is joined");
        // `gcd` is the part's and has the module's name in front, like
        // a name of any file. `sqrt` is built in, and is left for the
        // checker. `math` is imported once, though two files import it.
        assert_eq!(
            printer::print(&joined),
            "import math\n\
             function math.gcd(a: int, b: int) -> int\n    \
                 var big = a\n    var small = b\n    \
                 if (big < 0)\n        big = (-big)\n    \
                 if (small < 0)\n        small = (-small)\n    \
                 while (small != 0)\n        let rest = (big % small)\n        big = small\n        small = rest\n    \
                 return big\n\
             print(math.gcd(12, 18), math.sqrt(4.0))\n"
        );
        // The part is the last file, so the program's own files have
        // the numbers they would have without it.
        assert_eq!(loaded.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(), ["test.zeno", "library/math.zeno"]);
        assert!(joined.modules[0].strict);
    }

    #[test]
    fn only_the_functions_a_program_reaches_are_joined() {
        let functions = |source: &str| -> Vec<String> {
            let joined = load(source).program.expect("the program is joined");
            let mut names = Vec::new();
            for statement in &joined.statements {
                if let crate::ast::StmtKind::Function(function) = &statement.kind {
                    names.push(function.name.clone());
                }
            }
            names
        };
        // `hypot` uses `size`, which is private to the part.
        assert_eq!(functions("import math\nprint(math.hypot(3.0, 4.0))\n"), ["math.size", "math.hypot"]);
        // `tanh` is `sinh` over `cosh`, and both of those use `size`.
        assert_eq!(functions("from math import tanh\nprint(tanh(1.0))\n"), ["math.size", "math.sinh", "math.cosh", "math.tanh"]);
        assert_eq!(functions("import math\nlet f = math.clamp\n"), ["math.clamp"]);
        // What is not a function comes with the part: the four numbers
        // of `random`. `bits` needs `seed`, and nothing needs `number`.
        let joined = load("import random\nprint(random.between(1, 6))\n").program.expect("the program is joined");
        assert!(printer::print(&joined).starts_with("var random.a = 0\nvar random.b = 0\n"), "{}", printer::print(&joined));
        assert_eq!(functions("import random\nprint(random.between(1, 6))\n"), ["random.bits", "random.seed", "random.between"]);
        // A class of a part is joined only when it is reached, by a
        // function that makes one or by its name as a type (59.6).
        let classes = |source: &str| -> Vec<String> {
            let joined = load(source).program.expect("the program is joined");
            let mut names = Vec::new();
            for statement in &joined.statements {
                if let crate::ast::StmtKind::Class(class) = &statement.kind {
                    names.push(class.name.clone());
                }
            }
            names
        };
        assert_eq!(classes("import files\nprint(files.join(\"a\", \"b\"))\n"), Vec::<String>::new());
        assert_eq!(functions("import files\nprint(files.join(\"a\", \"b\"))\n"), ["files.join"]);
        // With the `Resource` that comes with Zeno, which it is (59.1).
        assert_eq!(classes("import files\nlet source = files.read_from(\"a\")\n"), ["Resource", "files.Reader"]);
        assert_eq!(classes("import files\nfunction lines(source: files.Reader) -> int {\n    return 0\n}\n"), ["Resource", "files.Reader"]);
        // A function of a file of the program can be what reaches it.
        assert_eq!(
            functions("import shapes\nprint(shapes.far(1.0))\n==> shapes.zeno\nimport math\nfunction far(x: float) -> float { return math.cosh(x) }\n"),
            ["math.size", "math.cosh", "shapes.far"]
        );
    }

    #[test]
    fn a_program_that_names_nothing_of_a_part_is_the_program_it_was() {
        // No word of the part: the part is not even read.
        let source = "import math\nprint(math.sqrt(4.0))\n";
        let alone = crate::parser::parse(crate::lexer::lex(source).unwrap()).unwrap();
        let loaded = load(source);
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(loaded.program.unwrap(), alone);
        // A word of the part that means something else here: the part
        // is read, found to be of no use, and taken away again.
        let source = "import math\nlet hypot = 3\nfunction clamp(n: int) -> int { return n }\nprint(math.sqrt(4.0), hypot, clamp(1))\n";
        let alone = crate::parser::parse(crate::lexer::lex(source).unwrap()).unwrap();
        let loaded = load(source);
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(loaded.program.unwrap(), alone);
    }

    #[test]
    fn a_name_can_be_imported_from_either_part_of_a_module() {
        let joined = load("from math import hypot, sqrt, pi\nprint(hypot(3.0, 4.0), sqrt(pi))\n").program.expect("the program is joined");
        let text = printer::print(&joined);
        // The import that is left names what is built in, for the checker.
        assert!(text.contains("\nfrom math import sqrt, pi\nprint(math.hypot(3.0, 4.0), sqrt(pi))\n"), "{text}");
        // With nothing built in to name, the line has done all its work.
        let joined = load("from math import gcd\nprint(gcd(4, 6))\n").program.expect("the program is joined");
        assert!(printer::print(&joined).ends_with("    return big\nprint(math.gcd(4, 6))\n"), "{}", printer::print(&joined));
        // A module that is all in Zeno leaves no import behind.
        let joined = load("import random\nfrom random import seed\nseed(7)\n").program.expect("the program is joined");
        assert!(!printer::print(&joined).contains("import"), "{}", printer::print(&joined));
    }

    #[test]
    fn a_mistake_about_a_part_says_what_to_do() {
        assert_eq!(error("import random\nprint(random.dice())\n"), "module `random` has no `dice`. It has `seed`, `number`, `between` and `pick`");
        assert_eq!(
            error("import random\nprint(random.bits())\n"),
            "`bits` is private in the module `random`: only its own file can use it. To use it from another file, take the word `private` off where it is declared"
        );
        assert_eq!(
            error("import random\nprint(random)\n"),
            "`random` is a module, not a value. Write `random.` and then a name from it, as in `random.seed`"
        );
        assert_eq!(
            error("function throw() -> int {\n    import random\n    return random.between(1, 6)\n}\n"),
            "`import random` goes at the top of the file, not inside a function or a block. \
             `random` is written in Zeno, and is joined to the program as a file of it is"
        );
        // A name of the built-in part is the checker's to refuse.
        assert!(load("import math\nprint(math.hypot(1.0, 2.0), math.nothing)\n").program.is_ok());
    }

    #[test]
    fn a_part_means_the_names_that_come_with_zeno() {
        // `random.pick` is written with `len`. If the program has a
        // `len` of its own, the two would be one name once joined.
        let (message, file, line) = mistake("import random\nfunction len(n: int) -> int { return n }\nprint(random.pick([1, 2]))\n");
        assert_eq!(
            message,
            "this program uses `random.pick`, which is written in Zeno and uses the `len` that comes with Zeno. \
             So the program cannot give `len` a meaning of its own here. Give this one another name"
        );
        assert_eq!((file, line), (0, 2));
        // It is no mistake while the program does not reach `pick`.
        assert!(load("import random\nfunction len(n: int) -> int { return n }\nprint(random.between(1, 2), len(3))\n").program.is_ok());
        // And the part of `math` uses no such name at all, so a program
        // is free to have an `abs`, a `max` or a `size` of its own.
        let source = "import math\nfunction abs(n: int) -> int { return 7 }\nlet size = 2\nlet max = 3\nprint(math.hypot(3.0, 4.0), math.tanh(1.0), math.gcd(4, 6), math.clamp(1, 2, 3))\n";
        assert!(load(source).program.is_ok());
    }

    #[test]
    fn the_parts_are_what_library_rs_says_they_are() {
        for part in crate::library::PARTS {
            let tokens = crate::lexer::lex(part.source).unwrap_or_else(|errors| panic!("{}: {}", part.path, errors[0].message));
            let parsed = crate::parser::parse(tokens).unwrap_or_else(|error| panic!("{}: {}", part.path, error.message));
            // Every type is written, as a build asks.
            assert!(parsed.strict, "{} does not begin with `strict`", part.path);
            // The names it offers are the ones a file is searched for.
            let private = super::private_names(&parsed);
            let offered: Vec<String> = super::declarations(&parsed).into_iter().map(|(name, _, _)| name).filter(|name| !private.contains(name)).collect();
            assert_eq!(offered, part.names, "{}", part.path);
            // It imports the built-in part of its own module, and no more.
            for statement in &parsed.statements {
                if let crate::ast::StmtKind::Import { module, names } = &statement.kind {
                    assert!(part.built_in_too && module.join(".") == part.module && names.is_empty(), "{} imports {module:?}", part.path);
                }
            }
            // And it is a module that comes with Zeno.
            assert!(crate::builtins::comes_with_zeno(part.module));
            assert!(crate::builtins::MODULES.contains(&format!("`{}`", part.module)));
        }
    }

    #[test]
    fn two_files_can_import_one_module_that_comes_with_zeno() {
        // Joined, the program imports it once. This used to be "`math`
        // is already declared in this block", at the second of them.
        let joined = program(
            "import math\nimport shapes\nfrom math import pi\nprint(math.sqrt(4.0), shapes.round_area(1.0), pi)\n\
             ==> shapes.zeno\nimport math\nfrom math import pi, floor\nfunction round_area(r: float) -> float { return floor(pi * r * r) + math.e }\n",
        );
        assert_eq!(
            printer::print(&joined),
            "import math\nfrom math import pi, floor\n\
             function shapes.round_area(r: float) -> float\n    return (floor(((pi * r) * r)) + math.e)\n\
             print(math.sqrt(4.0), shapes.round_area(1.0), pi)\n"
        );
        // Twice in one file is still one too many.
        let twice = load("import math\nimport math\nprint(math.pi)\n").program.expect("the files are joined");
        assert_eq!(printer::print(&twice), "import math\nimport math\nprint(math.pi)\n");
    }

    #[test]
    fn the_names_of_an_imported_file_get_its_name_in_front() {
        let joined = program(&format!(
            "import shapes\nfrom shapes import Point, Square\n\
             let p: Point = Point(1)\nlet q: shapes.Point = shapes.Point(2)\n\
             print(shapes.area(Square(3)), shapes.sides, shapes.Shape.Dot)\n{SHAPES}"
        ));
        // The imported file's statements come first, every name it
        // declares at its top with `shapes.` in front. The two imports
        // are gone: they have done their work.
        assert_eq!(
            printer::print(&joined),
            "let shapes.sides = 4\n\
             var shapes.made = 0\n\
             enum shapes.Shape\n    shapes.Dot\n    shapes.Square(side: int)\n\
             class shapes.Point\n    field x: int\n\
             function shapes.area(shape: shapes.Shape) -> int\n    \
                 shapes.made += 1\n    \
                 match shape\n        \
                     case shapes.Dot\n            return 0\n        \
                     case shapes.Square(side)\n            return (side * side)\n\
             let p: shapes.Point = shapes.Point(1)\n\
             let q: shapes.Point = shapes.Point(2)\n\
             print(shapes.area(shapes.Square(3)), shapes.sides, shapes.Dot)\n"
        );
        assert_eq!(joined.modules.len(), 1);
        assert_eq!(joined.modules[0].path, "shapes.zeno");
    }

    #[test]
    fn a_name_declared_nearer_hides_an_imported_one() {
        let joined = program(&format!(
            "import shapes\nfrom shapes import area, sides\n\
             function own(area: int) -> int {{\n    let shapes = [area]\n    return shapes[0] + sides\n}}\n\
             for sides in [1] {{\n    print(sides)\n}}\n\
             print([area for area in [2]], sides)\n{SHAPES}"
        ));
        let text = printer::print(&joined);
        let main = &text[text.find("function own").expect("the first file is there")..];
        assert_eq!(
            main,
            "function own(area: int) -> int\n    let shapes = [area]\n    return (shapes[0] + shapes.sides)\n\
             for sides in [1]\n    print(sides)\n\
             print([area for area in [2]], shapes.sides)\n"
        );
    }

    #[test]
    fn a_case_can_be_written_with_its_enum_in_front() {
        // In a pattern too, for an enum of this file or of another.
        let joined = program(&format!(
            "import shapes\nfrom shapes import Shape\n\
             enum Light {{\n    Off\n    On\n}}\n\
             match Light.On {{\n    case Light.Off {{ print(0) }}\n    case On {{ print(1) }}\n}}\n\
             match shapes.Dot {{\n    case Shape.Dot {{ print(2) }}\n    case shapes.Shape.Square(side) {{ print(side) }}\n}}\n{SHAPES}"
        ));
        let text = printer::print(&joined);
        assert!(text.contains("match Light.On\n    case Off\n        print(0)\n    case On\n        print(1)\n"), "{text}");
        assert!(text.contains("match shapes.Dot\n    case shapes.Dot\n        print(2)\n    case shapes.Square(side)\n"), "{text}");
    }

    #[test]
    fn a_file_is_read_once_and_runs_before_what_imports_it() {
        // `test` imports `a` and `b`, and both import `base`.
        let source = "import a\nimport b\nprint(\"test\")\n\
                      ==> a.zeno\nimport base\nprint(\"a\")\n\
                      ==> b.zeno\nimport base\nimport a\nprint(\"b\")\n\
                      ==> base.zeno\nprint(\"base\")\n";
        assert_eq!(printer::print(&program(source)), "print(\"base\")\nprint(\"a\")\nprint(\"b\")\nprint(\"test\")\n");

        let read = RefCell::new(Vec::new());
        let loaded = super::load("demo/main.zeno", "import a\nimport b\n".to_string(), &|path| {
            read.borrow_mut().push(path.to_string());
            match path {
                path if path.ends_with("zeno.package") => None,
                path if path.ends_with("base.zeno") => Some(String::new()),
                _ => Some("import nested.base\n".to_string()),
            }
        });
        assert!(loaded.program.is_ok());
        // A module is looked for from the folder of the first file, and
        // a dot in its name is a folder. That folder's `zeno.package` is
        // looked for first, and the program can do without one. The
        // paths are written with `/` on every system (see `shown`).
        assert_eq!(*read.borrow(), ["demo/zeno.package", "demo/a.zeno", "demo/nested/base.zeno", "demo/b.zeno"]);
        let paths: Vec<&str> = loaded.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths.len(), 4);
        assert_eq!(paths[0], "demo/main.zeno");
    }

    #[test]
    fn a_mistake_is_in_the_file_it_is_in() {
        // The second file's lines are numbered from 1,000,001 inside
        // the compiler. What is shown is the file, and its own line 2.
        let (message, file, line) = mistake("import shapes\n==> shapes.zeno\nlet a = 1\nlet b = = 2\n");
        assert_eq!((message.as_str(), file, line), ("expected an expression, found `=`", 1, 2));

        // The same for a mistake the type checker finds.
        let joined = program("import shapes\nprint(shapes.half(3))\n==> shapes.zeno\n\nfunction half(n: int) -> int {\n    return n / 2\n}\n");
        let errors = crate::checker::check(&joined, false);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "the returned value should be int, but this is float");
        assert_eq!((errors[0].span.file(), errors[0].span.line_in_file(), errors[0].span.col), (1, 3, 12));
        assert_eq!(super::testing::place(&joined, errors[0].span), "shapes.zeno:3:12");
    }

    #[test]
    fn a_line_in_a_message_is_the_file_s_own() {
        let joined = program(
            "import lists\nlists.twice()\n==> lists.zeno\n\
             function lose(ys: take list[int]) {\n}\n\
             function twice() {\n    let mine = [1]\n    lose(mine)\n    print(mine)\n    print(later)\n}\n\
             let later = 4\n",
        );
        let rules = crate::checker::Rules { strict: true, one_owner: true, firmware: true };
        let messages: Vec<String> = crate::checker::check_by(&joined, rules).0.into_iter().map(|error| error.message).collect();
        assert_eq!(
            messages,
            [
                "`mine` was given away on line 5 and holds nothing now. Use it before it goes, or give it only \
                 to what looks: a parameter without `take`",
                "`lists.later` is declared on line 9, which is further down. A line can use only what is declared \
                 above it, also inside a function. Move the declaration of `lists.later` up",
            ]
        );
    }

    #[test]
    fn strict_is_asked_of_one_file() {
        let check = |source: &str, strict: bool| -> Vec<String> {
            crate::checker::check(&program(source), strict).into_iter().map(|error| error.message).collect()
        };
        let loose = "==> loose.zeno\nfunction anything(n) { return n }\n";
        let needs_a_type = |function: &str| {
            format!("strict mode: the parameter `n` of `{function}` needs a type. It could not be worked out from how it is used")
        };
        // The first file is strict, the file it imports is not.
        assert_eq!(check(&format!("strict\nimport loose\nfunction mine(n) {{ return n }}\n{loose}"), false), [needs_a_type("mine")]);
        // The other way round.
        assert_eq!(
            check("import tight\nfunction mine(n) { return n }\n==> tight.zeno\nstrict\nfunction theirs(n) { return n }\n", false),
            [needs_a_type("tight.theirs")]
        );
        // `--strict` and `zeno build` ask it of every file.
        assert_eq!(
            check(&format!("import loose\nfunction mine(n) {{ return n }}\n{loose}"), true),
            [needs_a_type("mine"), needs_a_type("loose.anything")]
        );
    }

    #[test]
    fn what_cannot_be_imported_says_why() {
        assert_eq!(
            error("import shapes\nimport drivers.uart\nimport shapes\n"),
            "there is no module `shapes`. `import shapes` reads the file `shapes.zeno`, and there is no such file. \
             So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`"
        );
        assert_eq!(load("import shapes\nimport shapes\nfrom shapes import x\n").program.unwrap_err().len(), 1);
        assert_eq!(
            error(&format!("print(1)\nimport shapes\n{SHAPES}")),
            "`import shapes` goes at the top of the file, above everything that is not an import. \
             The file `shapes` runs first wherever this line is, so the top is where the line belongs"
        );
        assert_eq!(
            error(&format!("function f() {{\n    import math\n    import shapes\n}}\n{SHAPES}")),
            "`import shapes` goes at the top of the file, not inside a function or a block. \
             Only a module that comes with Zeno, such as `math`, can be imported in here"
        );
        assert_eq!(error("import maths\n"), "there is no module `maths`. `import maths` reads the file `maths.zeno`, and there is no such file. So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`");
        assert_eq!(
            error("import test\n"),
            "`test.zeno` is the file the program starts in, and no file imports that one. \
             Move what the files share into a file of its own, and import that"
        );
        assert_eq!(error("import a\n==> a.zeno\nimport a\n"), "`a` imports itself. A file cannot import the file it is");
        assert_eq!(
            mistake("import a\n==> a.zeno\nimport b\n==> b.zeno\nimport d\n==> d.zeno\n\nimport a\n"),
            (
                "these files import each other: `a` imports `b`, `b` imports `d`, and `d` imports `a`. \
                 An imported file runs before the file that imports it, and here none of them can be the first. \
                 Move what they share into a file of its own, and import that"
                    .to_string(),
                3,
                2
            )
        );
    }

    #[test]
    fn a_name_that_is_not_there_says_what_is() {
        let has = "It has `sides`, `made`, `Shape`, `Dot`, `Square`, `Point` and `area`";
        assert_eq!(error(&format!("from shapes import aera\n{SHAPES}")), format!("module `shapes` has no `aera`. {has}"));
        assert_eq!(error(&format!("import shapes\nprint(shapes.aera(1))\n{SHAPES}")), format!("module `shapes` has no `aera`. {has}"));
        assert_eq!(error(&format!("import shapes\nlet p: shapes.Pint = 1\n{SHAPES}")), format!("module `shapes` has no `Pint`. {has}"));
        assert_eq!(
            error("import empty\nprint(empty.x)\n==> empty.zeno\nprint(1)\n"),
            "module `empty` has no `x`. Its file declares nothing at the top"
        );
        assert_eq!(
            error(&format!("import shapes\nlet s = shapes\n{SHAPES}")),
            "`shapes` is a module, not a value. Write `shapes.` and then a name from it, as in `shapes.sides`"
        );
        assert_eq!(
            error(&format!("let p: shapes.Point = 1\n{SHAPES}")),
            "this file imports no module `shapes`, so `shapes.Point` means nothing here. Write `import shapes` at the top of the file"
        );
        assert_eq!(
            error("import drivers.uart\nprint(drivers.spi.speed)\n==> drivers/uart.zeno\nlet speed = 1\n"),
            "this file imports no module `drivers.spi`. Write `import drivers.spi` at the top of the file"
        );
    }

    #[test]
    fn two_things_cannot_have_one_name() {
        assert_eq!(
            error(&format!("from shapes import area\nlet area = 4\n{SHAPES}")),
            "`area` is imported from `shapes` at the top of this file, so the file cannot declare a second `area`. \
             Give this one another name, or import the module whole with `import shapes` and write `shapes.area` for the other"
        );
        assert_eq!(
            error(&format!("import shapes\nfunction shapes() {{\n}}\n{SHAPES}")),
            "this file imports the module `shapes`, so it cannot also declare `shapes`: \
             `shapes.x` would have two readings. Give this one another name"
        );
        assert_eq!(
            error(&format!("from shapes import area\nfrom other import area\n{SHAPES}==> other.zeno\nlet area = 1\n")),
            "`area` is imported twice, from `shapes` and from `other`. Import one of the two modules whole, \
             as in `import other`, and write `other.area`"
        );
        assert_eq!(
            error("import a\nimport a.b\n==> a.zeno\nlet b = 1\n==> a/b.zeno\nlet x = 2\n"),
            "this file imports `a` and `a.b`, and the module `a` declares a `b` of its own. `a.b.x` would then \
             have two readings: `x` of the module `a.b`, and `b.x` of the module `a`. Import one of them with \
             `from`, as in `from a.b import x`"
        );
        // With no `b` in `a` there is one reading, and both are imported.
        let joined = program("import a\nimport a.b\nprint(a.c + a.b.x)\n==> a.zeno\nlet c = 1\n==> a/b.zeno\nlet x = 2\n");
        assert!(printer::print(&joined).ends_with("print((a.c + a.b.x))\n"), "{}", printer::print(&joined));
        assert_eq!(
            error("import a\nimport a.b\nprint(a.b)\n==> a.zeno\nlet c = 1\n==> a/b.zeno\nlet x = 2\n"),
            "`a.b` is a module, not a value. Write `a.b.` and then a name from it, as in `a.b.x`"
        );
    }

    // ----- Packages -----

    /// A thermostat that needs two packages: `filters`, in a folder of
    /// its own, and `units`, which `zeno fetch` would have fetched.
    const THERMOSTAT: &str = "import filters\n\
        import filters.lowpass\n\
        import control\n\
        from filters.lowpass import Lowpass\n\
        let filter: Lowpass = filters.lowpass.start(filters.clamp(2))\n\
        print(control.limit)\n\
        ==> zeno.package\n\
        # What the thermostat needs.\n\
        package thermostat\n\
        needs filters in \"lib/filters\"\n\
        needs units from \"https://example.com/units\" at \"v1.0.0\"\n\
        ==> control.zeno\n\
        import units\n\
        let limit = units.scale * 2\n\
        ==> lib/filters/zeno.package\n\
        package filters\n\
        needs units from \"https://example.com/units\" at \"v1.0.0\"\n\
        ==> lib/filters/filters.zeno\n\
        function clamp(value: int) -> int {\n    return value\n}\n\
        ==> lib/filters/lowpass.zeno\n\
        from filters import clamp\n\
        import units\n\
        class Lowpass {\n    weight: int\n}\n\
        function start(weight: int) -> Lowpass {\n    return Lowpass(clamp(weight) * units.scale)\n}\n\
        ==> packages/units/zeno.package\n\
        package units\n\
        ==> packages/units/units.zeno\n\
        let scale = 10\n";

    #[test]
    fn an_import_finds_the_module_of_a_package() {
        // `filters` is the package's module of its own name, and
        // `filters.lowpass` its module `lowpass`. Inside the package the
        // same two are `filters` and `lowpass`. Whoever writes them, each
        // has one name in the joined program, and is read once.
        let loaded = load(THERMOSTAT);
        let paths: Vec<&str> = loaded.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "test.zeno",
                "zeno.package",
                "lib/filters/zeno.package",
                "packages/units/zeno.package",
                "lib/filters/filters.zeno",
                "lib/filters/lowpass.zeno",
                "packages/units/units.zeno",
                "control.zeno",
            ]
        );
        let joined = loaded.program.unwrap_or_else(|errors| panic!("{}", errors[0].message));
        assert_eq!(
            printer::print(&joined),
            "function filters.clamp(value: int) -> int\n    return value\n\
             let units.scale = 10\n\
             class filters.lowpass.Lowpass\n    field weight: int\n\
             function filters.lowpass.start(weight: int) -> filters.lowpass.Lowpass\n    \
             return filters.lowpass.Lowpass((filters.clamp(weight) * units.scale))\n\
             let control.limit = (units.scale * 2)\n\
             let filter: filters.lowpass.Lowpass = filters.lowpass.start(filters.clamp(2))\n\
             print(control.limit)\n"
        );
        // The files of the program are still numbered as they were read,
        // so that a mistake in any of them is shown in the right one.
        let modules: Vec<&str> = joined.modules.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(modules, paths[1..]);

        // Two packages may need each other, as long as no two files
        // import each other.
        let joined = program(
            "import a.one\nprint(a.one.x)\n\
             ==> zeno.package\npackage top\nneeds a in \"a\"\n\
             ==> a/zeno.package\npackage a\nneeds b in \"../b\"\n\
             ==> a/one.zeno\nimport b.two\nlet x = b.two.y\n\
             ==> a/base.zeno\nlet z = 1\n\
             ==> b/zeno.package\npackage b\nneeds a in \"../a\"\n\
             ==> b/two.zeno\nimport a.base\nlet y = a.base.z\n",
        );
        assert_eq!(printer::print(&joined), "let a.base.z = 1\nlet b.two.y = a.base.z\nlet a.one.x = b.two.y\nprint(a.one.x)\n");
    }

    #[test]
    fn a_mistake_about_a_package_says_what_to_do() {
        // A mistake in a `zeno.package` is shown in that file.
        let with = |package: &str, rest: &str| format!("print(1)\n==> zeno.package\n{package}{rest}");
        assert_eq!(
            mistake(&with("package top\nneeds gone in \"../gone\"\n", "")),
            (
                "there is no package in `../gone`. This line reads the file `../gone/zeno.package`, and there is no such file"
                    .to_string(),
                1,
                2
            )
        );
        assert_eq!(
            mistake(&with("package top\n\nneeds units from \"https://example.com/units\" at \"v1\"\n", "")),
            (
                "the package `units` has not been fetched: there is no file `packages/units/zeno.package`. \
                 Run `zeno fetch` in the folder of the program"
                    .to_string(),
                1,
                3
            )
        );
        assert_eq!(
            mistake(&with("package top\nneeds a in \"a\"\n", "==> a/zeno.package\npackage a\nneeds b \"b\"\n")).1,
            2
        );
        assert_eq!(
            error(&with("package top\nneeds filter in \"lib\"\n", "==> lib/zeno.package\npackage filters\n")),
            "the package in `lib` is called `filters`, not `filter`. Write `needs filters ...`"
        );
        assert_eq!(
            error(&with(
                "package top\nneeds a in \"a\"\nneeds b in \"b\"\n",
                "==> a/zeno.package\npackage a\nneeds b in \"b\"\n==> b/zeno.package\npackage b\n==> a/b/zeno.package\npackage b\n"
            )),
            "two packages called `b` are needed: the one in `a/b` and the one in `b`. \
             A program has one package of each name. Make the `needs` lines agree"
        );
        // One folder written two ways is one package.
        assert!(load(&with(
            "package top\nneeds a in \"a\"\nneeds b in \"./a/../b\"\n",
            "==> a/zeno.package\npackage a\nneeds b in \"../b\"\n==> b/zeno.package\npackage b\n"
        ))
        .program
        .is_ok());

        // An import that names a package reads a file of that package.
        let needs = |program: &str, rest: &str| {
            error(&format!(
                "{program}==> zeno.package\npackage top\nneeds filters in \"lib\"\n==> lib/zeno.package\npackage filters\n{rest}"
            ))
        };
        assert_eq!(
            needs("import filters.lowpass\n", ""),
            "the package `filters` has no module `lowpass`. `import filters.lowpass` reads the file `lib/lowpass.zeno`, \
             and there is no such file"
        );
        assert_eq!(
            needs("import filters\n", ""),
            "the package `filters` has no module of its own name. `import filters` reads the file `lib/filters.zeno`, \
             and there is no such file. Import one of its modules by name, as in `import filters.name`"
        );
        assert_eq!(
            needs("import filters.filters\n", "==> lib/filters.zeno\nlet x = 1\n"),
            "the module of the package `filters` that has the package's own name is imported as `import filters`. Write that"
        );
        assert_eq!(
            needs("import filters\n", "==> lib/filters.zeno\nlet x = 1\n==> filters.zeno\nlet x = 2\n"),
            "`import filters` has two readings here: the file `filters.zeno`, and the file `lib/filters.zeno` of the \
             package `filters` that `zeno.package` needs. Give the first another name"
        );
        // Inside the package, a module of the program is not in reach.
        assert_eq!(
            needs("import filters\nimport control\n", "==> control.zeno\nlet x = 1\n==> lib/filters.zeno\nimport control\n"),
            "there is no module `control`. `import control` reads the file `lib/control.zeno`, and there is no such file. \
             So far Zeno has `math`, `time`, `board`, `files`, `program`, `json`, `web`, `python`, `node`, `java`, `random`, `ruby`, `php`, `lua`, `csharp` and `c`"
        );
        // A file is one module, and a module is one file.
        assert_eq!(
            needs("import filters\nimport lib.filters\n", "==> lib/filters.zeno\nlet x = 1\n"),
            "the file `lib/filters.zeno` is already in this program as the module `filters`, and this line would make it \
             a second module, `lib.filters`. A file is one module: import it by one name"
        );
        assert_eq!(
            error(
                "import units\nimport a.m\n==> units.zeno\nlet x = 1\n\
                 ==> zeno.package\npackage top\nneeds a in \"a\"\n\
                 ==> a/zeno.package\npackage a\nneeds units in \"../u\"\n\
                 ==> a/m.zeno\nimport units\n\
                 ==> u/zeno.package\npackage units\n==> u/units.zeno\nlet x = 2\n"
            ),
            "two files would both be the module `units` of this program: `units.zeno` and `u/units.zeno`. \
             A module is one file. Give one of the two another name"
        );
    }

    #[test]
    fn a_path_is_tidied() {
        let tidy = |path: &str| super::shown(&super::tidy(std::path::Path::new(path)));
        assert_eq!(tidy("examples/thermostat/../filters"), "examples/filters");
        assert_eq!(tidy("./a/./b"), "a/b");
        assert_eq!(tidy("../filters"), "../filters");
        assert_eq!(tidy("a/../../b"), "../b");
        assert_eq!(tidy("."), "");
        assert_eq!(tidy("/srv/a/.."), "/srv");
    }

    #[test]
    fn a_test_is_its_own_files() {
        // A test sees what is private to the file it is written in, and
        // `zeno test` runs the tests of the first file only: those of an
        // imported file are run when that file is tested.
        let program = super::testing::program(
            "import kept\n\
             test \"from outside\" {\n    assert kept.outer() == 3\n}\n\
             ==> kept.zeno\n\
             private let limit = 3\n\
             function outer() -> int {\n    return limit\n}\n\
             test \"from inside\" {\n    assert limit == 4\n    assert raises ValueError {\n        outer()\n    }\n}\n",
        );
        let complaints = crate::checker::check(&program, true);
        assert!(complaints.is_empty(), "{complaints:?}");
        let (output, tested) = crate::interpreter::run_tests(&program, true, crate::value::testing::outside(), None);
        assert_eq!(output, "  ok      from outside\n");
        assert_eq!(tested.map(|tested| tested.len()).ok(), Some(1));
        // A build has no tests in it.
        let built = program.without_tests();
        assert!(built.statements.iter().all(|statement| !matches!(statement.kind, crate::ast::StmtKind::Test { .. })));
    }

    #[test]
    fn what_is_private_stays_in_its_file() {
        let kept = "==> kept.zeno\n\
            private let limit = 3\n\
            private function inner() -> int {\n    return limit\n}\n\
            function outer() -> int {\n    let helper = Helper(inner())\n    let mode = Fast\n    return helper.n\n}\n\
            private enum Mode {\n    Fast\n    Slow\n}\n\
            private class Helper {\n    n: int\n}\n";
        // The file itself uses all of it, and another file uses what is
        // not private.
        super::testing::program(&format!("import kept\nprint(kept.outer())\n{kept}"));
        let private = |name: &str| {
            format!(
                "`{name}` is private in the module `kept`: only its own file can use it. \
                 To use it from another file, take the word `private` off where it is declared"
            )
        };
        assert_eq!(error(&format!("import kept\nprint(kept.inner())\n{kept}")), private("inner"));
        assert_eq!(error(&format!("import kept\nprint(kept.limit)\n{kept}")), private("limit"));
        assert_eq!(error(&format!("from kept import outer, inner\n{kept}")), private("inner"));
        assert_eq!(error(&format!("import kept\nlet h: kept.Helper or none = none\n{kept}")), private("Helper"));
        // The cases of a private enum are private with it.
        assert_eq!(error(&format!("import kept\nlet mode = kept.Fast\n{kept}")), private("Fast"));
        assert_eq!(error(&format!("from kept import Slow\n{kept}")), private("Slow"));
        // What a module is said to have is what another file can use.
        assert_eq!(error(&format!("import kept\nprint(kept.other)\n{kept}")), "module `kept` has no `other`. It has `outer`");
        // Two files can each keep a name to themselves: they never meet.
        super::testing::program(&format!("import kept\nprivate let limit = 9\nprint(limit + kept.outer())\n{kept}"));
    }

    #[test]
    fn a_class_can_be_an_interface_of_another_file() {
        let ports = "==> ports.zeno\n\
            interface Port {\n    function send(self, text: str) -> int\n}\n\
            private interface Inner {\n    function reset(self)\n}\n\
            class Null is Port, Inner {\n    function send(self, text: str) -> int {\n        return 0\n    }\n\n    function reset(self) {\n    }\n}\n";
        let uart = |is: &str| {
            format!("class Uart is {is} {{\n    function send(self, text: str) -> int {{\n        return len(text)\n    }}\n}}\n")
        };
        // The name after `is` is looked up as any other name of a module:
        // with the module in front, or imported by name.
        let program = super::testing::program(&format!("import ports\n{}{ports}", uart("ports.Port")));
        assert!(crate::printer::print(&program).contains("class Uart is ports.Port\n"));
        let program = super::testing::program(&format!("from ports import Port\n{}{ports}", uart("Port")));
        assert!(crate::printer::print(&program).contains("class Uart is ports.Port\n"));
        // In its own file the interface is named as it is written.
        assert!(crate::printer::print(&program).contains("class ports.Null is ports.Port, ports.Inner\n"));
        assert_eq!(
            error(&format!("import ports\n{}{ports}", uart("ports.Inner"))),
            "`Inner` is private in the module `ports`: only its own file can use it. \
             To use it from another file, take the word `private` off where it is declared"
        );
        assert_eq!(
            error(&format!("import ports\n{}{ports}", uart("ports.Gone"))),
            "module `ports` has no `Gone`. It has `Port` and `Null`"
        );
    }

    #[test]
    fn a_file_sees_what_it_declares_and_what_it_imports() {
        // Only the file of a module gives its variables a new value.
        let belongs = "`shapes.made` is a variable of the module `shapes`, and only the file of `shapes` gives it a new value. \
                       Give `shapes` a function that changes it, and call that";
        assert_eq!(error(&format!("import shapes\nshapes.made = 3\n{SHAPES}")), belongs);
        assert_eq!(error(&format!("from shapes import made\nmade += 1\n{SHAPES}")), belongs);
        // An imported file does not see the file that imports it.
        let unseen = |name: &str| {
            format!(
                "this file declares no `{name}`. The program's first file has one, and an imported file cannot see \
                 what the file that imports it declares. Hand the value to the function that needs it, as an argument"
            )
        };
        let first = "import peek\nlet total = 5\nenum State {\n    Idle\n    Busy\n}\nclass Reading {\n    value: int\n}\n";
        let peek = |body: &str| error(&format!("{first}==> peek.zeno\n{body}"));
        assert_eq!(peek("function show() {\n    print(total)\n}\n"), unseen("total"));
        assert_eq!(peek("function show(r: Reading) {\n}\n"), unseen("Reading"));
        assert_eq!(peek("function show(n: int) {\n    match n {\n        case Idle { print(1) }\n    }\n}\n"), unseen("Idle"));
        // A name of its own with the same spelling is its own.
        assert!(load(&format!("{first}==> peek.zeno\nlet total = 1\nfunction show(State: int) -> int {{\n    return State + total\n}}\n")).program.is_ok());
    }
}
