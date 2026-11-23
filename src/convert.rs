//! `zeno convert program.py`: a file of another language, written again
//! as Zeno (docs/convert.md, decisions chapter 62).
//!
//! The one promise is that what is written is never silently wrong. A
//! file is cut into pieces, the statements that begin at its left
//! margin, and each piece ends in one of three states:
//!
//! - **Zeno**: lines of Zeno that mean what the piece meant.
//! - **Zeno with a note**: the same, with a `# convert:` comment where
//!   Zeno does something a reader of the other language would not
//!   expect.
//! - **Kept**: the piece as it was written, in an `inline` block that
//!   the lines of Zeno call (docs/inline.md). So what is written always
//!   runs, and the work left is to make the block smaller.
//!
//! This file is the command and the report. Reading Python is
//! `convert_python.rs`, and writing Zeno from it `convert_python_write.rs`.

use std::path::Path;
use std::process::ExitCode;

/// A place where Zeno does otherwise than the other language, said in
/// the file once and counted in the report every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Note {
    /// A value that `print` or an f-string shows Zeno's way.
    Shown,
    /// A float that is shown: a very large or a very small one is
    /// written otherwise.
    Exponent,
    /// The message of an error that Zeno raised itself.
    ErrorWords,
    /// An `except` that catches a kind of error that Zeno raises in
    /// places where Python raises none.
    Catches,
    /// `upper` and `lower` know a to z only.
    Letters,
    /// `math.round` and a half.
    Half,
    /// A variable that began as a whole number and holds a float later.
    FloatStart,
}

impl Note {
    /// What the `# convert:` comment says, one line of it an item.
    pub fn lines(self, name: &str) -> Vec<String> {
        match self {
            Note::Shown => vec![
                "convert: Zeno shows a value its own way: `true`, `false` and `none` in small".to_string(),
                "letters, a text inside a list, a dict or an object in double quotes, and a".to_string(),
                "dict between `[` and `]`.".to_string(),
            ],
            Note::Exponent => vec![
                "convert: a float is shown as Python shows it, but for a very large or a very".to_string(),
                "small one: Python's 1e+16 is 1e16 here, and its 1e-07 is 1e-7.".to_string(),
            ],
            Note::ErrorWords => vec![
                "convert: the message of an error is in Zeno's words where Zeno raised the".to_string(),
                "error itself. An error that came out of the Python that was kept has its".to_string(),
                "kind in front of its message, and a `KeyError` shows its key without quotes.".to_string(),
            ],
            Note::Catches => vec![
                "convert: this `except` also catches the errors that Zeno raises where Python".to_string(),
                "raises none: a whole number past 64 bits (`OverflowError`), an item at a".to_string(),
                "place below zero that a variable holds (`IndexError`), a whole number raised".to_string(),
                "to a power below zero (`ValueError`).".to_string(),
            ],
            Note::Letters => vec![
                "convert: `upper` and `lower` change a to z only. Python changes every letter".to_string(),
                "that has a capital.".to_string(),
            ],
            Note::Half => vec![
                "convert: `math.round` takes a half away from zero: 2.5 to 3. Python's `round`".to_string(),
                "takes a half to the even number: 2.5 to 2.".to_string(),
            ],
            Note::FloatStart => vec![
                format!("convert: `{name}` is a float from its first value, since it holds a float"),
                "later. In Python it is a whole number until then, and is printed as one.".to_string(),
            ],
        }
    }

    /// The note in the few words the report lists it by.
    pub fn title(self) -> &'static str {
        match self {
            Note::Shown => "a value is shown as Zeno shows it",
            Note::Exponent => "a float is shown, and a very large or very small one is written otherwise",
            Note::ErrorWords => "the message of an error is Zeno's own",
            Note::Catches => "an `except` also catches errors that only Zeno raises",
            Note::Letters => "`upper` and `lower` change a to z only",
            Note::Half => "`math.round` takes a half away from zero",
            Note::FloatStart => "a variable is a float from its first value",
        }
    }
}

/// A piece that was kept as it was written.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptPiece {
    /// The line the piece begins on.
    pub line: usize,
    /// What it is: `def load`, `class Meter`, or the first words of it.
    pub what: String,
    /// The line the reason is on.
    pub at: usize,
    pub why: String,
}

/// What was done with a file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// How many pieces the file has: the statements at its left margin.
    pub pieces: usize,
    /// Every place a note applies, by the line of the source.
    pub noted: Vec<(Note, usize)>,
    pub kept: Vec<KeptPiece>,
    /// Why the whole file was kept as one block, if it was.
    pub whole: Option<String>,
    /// Things the kept pieces do that a block does otherwise.
    pub warnings: Vec<String>,
}

pub struct Converted {
    pub zeno: String,
    pub report: Report,
}

impl Report {
    /// The report as `zeno convert` prints it.
    pub fn text(&self, from: &str, to: &str) -> String {
        let mut out = format!("Wrote {to} from {from}.\n");
        if let Some(why) = &self.whole {
            out.push_str(&format!("  The whole file is kept as Python, in one `inline python` block: {why}.\n"));
            return out;
        }
        let kept = self.kept.len();
        let zeno = self.pieces.saturating_sub(kept);
        let noted_lines: std::collections::BTreeSet<usize> = self.noted.iter().map(|(_, line)| *line).collect();
        let pieces = if self.pieces == 1 { "1 piece".to_string() } else { format!("{} pieces", self.pieces) };
        out.push_str(&format!("  {pieces}: {zeno} written as Zeno, {kept} kept as Python.\n"));
        if !self.noted.is_empty() {
            let places = if noted_lines.len() == 1 { "1 line".to_string() } else { format!("{} lines", noted_lines.len()) };
            out.push_str(&format!("\n  Where Zeno does otherwise than Python ({places}; each kind has a `# convert:` comment at its first):\n"));
            let mut kinds: Vec<Note> = self.noted.iter().map(|(note, _)| *note).collect();
            kinds.sort();
            kinds.dedup();
            for kind in kinds {
                let lines: Vec<String> = self.noted.iter().filter(|(note, _)| *note == kind).map(|(_, line)| line.to_string()).collect();
                let word = if lines.len() == 1 { "line" } else { "lines" };
                out.push_str(&format!("    {}: {word} {}\n", kind.title(), lines.join(", ")));
            }
        }
        if !self.kept.is_empty() {
            out.push_str("\n  Kept as Python, in the `inline python` block:\n");
            for piece in &self.kept {
                let at = if piece.at == piece.line { String::new() } else { format!(" (line {})", piece.at) };
                out.push_str(&format!("    line {}, {}: {}{at}\n", piece.line, piece.what, piece.why));
            }
        }
        for warning in &self.warnings {
            out.push_str(&format!("\n  Take care: {warning}\n"));
        }
        out
    }
}

/// A text as Zeno writes one, between the quotes asked for. `None` if
/// it holds a character Zeno has no way to write.
pub fn zeno_text(value: &str, quote: char) -> Option<String> {
    let mut out = String::new();
    out.push(quote);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => return None,
            c => out.push(c),
        }
    }
    out.push(quote);
    Some(out)
}

fn usage() {
    eprintln!("zeno: `convert` needs a file of Python, e.g. `zeno convert report.py`");
    eprintln!("      It writes `report.zeno` beside it, and says what it did with each piece.");
    eprintln!("        --to <file.zeno>   write this file in place of that one");
    eprintln!("        --replace          write the file though it is there already");
}

/// `zeno convert file.py [--to out.zeno] [--replace]`.
pub fn command(args: &[String]) -> ExitCode {
    let mut file = None;
    let mut to = None;
    let mut replace = false;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--replace" => replace = true,
            "--to" => match words.next() {
                Some(path) => to = Some(path.clone()),
                None => {
                    eprintln!("zeno: `--to` needs the file to write, e.g. `--to report.zeno`");
                    return ExitCode::FAILURE;
                }
            },
            option if option.starts_with("--") => {
                eprintln!("zeno: `convert` has no option `{option}`");
                usage();
                return ExitCode::FAILURE;
            }
            path if file.is_none() => file = Some(path.to_string()),
            path => {
                eprintln!("zeno: `convert` takes one file at a time, and `{path}` is a second");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(file) = file else {
        usage();
        return ExitCode::FAILURE;
    };
    let path = Path::new(&file);
    let kind = path.extension().and_then(|kind| kind.to_str()).unwrap_or("");
    if kind != "py" {
        let hint = match kind {
            "c" | "h" => " C is the next language it is to read",
            "js" | "ts" | "java" => " Python is the first language it reads; this one is to come",
            "zeno" => " This file is Zeno already",
            _ => "",
        };
        eprintln!("zeno: `convert` reads a file of Python, whose name ends in `.py`, and `{file}` is not one.{hint}");
        return ExitCode::FAILURE;
    }
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("zeno: cannot read `{file}`: {error}");
            return ExitCode::FAILURE;
        }
    };
    let to = to.unwrap_or_else(|| path.with_extension("zeno").to_string_lossy().into_owned());
    if Path::new(&to).exists() && !replace {
        eprintln!("zeno: `{to}` is there already, and `convert` does not write over a file unasked.");
        eprintln!("      Give another name with `--to`, or say `--replace`.");
        return ExitCode::FAILURE;
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| file.clone());
    let converted = crate::convert_python_write::convert(&source, &name);
    if let Err(error) = std::fs::write(&to, &converted.zeno) {
        eprintln!("zeno: cannot write `{to}`: {error}");
        return ExitCode::FAILURE;
    }
    print!("{}", converted.report.text(&file, &to));
    println!();
    println!("  Run it with `zeno run {to}`. It prints what `python3 {file}` prints,");
    println!("  but for the notes above.");
    ExitCode::SUCCESS
}
