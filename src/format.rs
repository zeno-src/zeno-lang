//! `zeno format`: lay a program out the one way Zeno lays programs out.
//!
//! There are no options. Every Zeno file that has been through the
//! formatter is indented, spaced and commented the same way, so nobody
//! has to decide how, or argue about it.
//!
//! The formatter works on the **tokens** of the file, not on its syntax
//! tree. It keeps every token exactly as it was written and in the
//! order it was written, and it keeps every line break. All it changes
//! is the white space around them:
//!
//! 1. **Indentation.** Four spaces for each `{`, `(` or `[` that is
//!    open, counted in lines: a line that opens two brackets indents
//!    what follows once.
//! 2. **Spacing.** One space around an operator and after a comma or a
//!    colon. None inside `( )` and `[ ]`, none before a call's `(` or an
//!    index's `[`, one inside `{ }`. None around the `=` that names an
//!    argument: `Point(x=3.0, y=4.0)`.
//! 3. **Blank lines.** Never two in a row, none at the top of a block or
//!    of the file, none at the bottom of either.
//! 4. **Comments.** A space after the `#`. A comment after code keeps
//!    its distance from the code, and has at least two spaces.
//!
//! Because no token is added, removed or moved, a formatted program
//! means what it meant. The formatter still checks: it reads its own
//! output back and compares the tokens with the ones it started from.
//!
//! See docs/format.md for a walk through this file.

use crate::error::Error;
use crate::lexer::{self, Written};
use crate::parser;
use crate::token::{Span, Token, TokenKind};

/// How far one step of indentation is.
const STEP: usize = 4;

/// The least room between code and a comment after it.
const BEFORE_COMMENT: usize = 2;

/// The source, formatted. A file that has a mistake in it is not
/// formatted: the mistakes come back, as `zeno check` would show them.
pub fn format(source: &str) -> Result<String, Vec<Error>> {
    let written = lexer::lex_written(source)?;
    // Brackets that match are what the indentation is counted from, and
    // the parser is what knows that they do.
    parser::parse(written.tokens.clone()).map_err(|error| vec![error])?;

    let formatted = layout(&written);

    // The promise of this file, checked: the same tokens, the same
    // comments, in the same order.
    let again = lexer::lex_written(&formatted)?;
    let kinds = |written: &Written| -> Vec<TokenKind> { written.tokens.iter().map(|token| token.kind.clone()).collect() };
    let comments = |written: &Written| -> Vec<String> { written.comments.iter().map(|(_, text)| comment(text)).collect() };
    if kinds(&again) != kinds(&written) || comments(&again) != comments(&written) {
        let at = again
            .tokens
            .iter()
            .zip(&written.tokens)
            .find(|(new, old)| new.kind != old.kind)
            .map(|(_, old)| old.span)
            .unwrap_or(Span { line: 1, col: 1 });
        return Err(vec![Error::new(
            "`zeno format` would have changed what this file means here, so it left the file as it is. \
             That is a mistake in Zeno itself, not in the file",
            at,
        )]);
    }
    Ok(formatted)
}

/// One line of the source: the tokens that start on it, and its comment.
#[derive(Default)]
struct Line<'a> {
    /// Places in `Written::tokens`.
    tokens: Vec<usize>,
    comment: Option<&'a str>,
    /// How many characters stood between the code and the comment.
    before_comment: usize,
}

/// A bracket that is open: which kind, and how far the line that
/// opened it is indented.
struct Open {
    brace: bool,
    indent: usize,
}

fn layout(written: &Written) -> String {
    let tokens = &written.tokens;

    // ----- Sort the tokens and the comments into the lines they are on.
    let mut lines: Vec<Line> = Vec::new();
    let mut line_of = |number: usize| {
        if lines.len() < number {
            lines.resize_with(number, Line::default);
        }
        number - 1
    };
    let mut on_line = Vec::new();
    for (place, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, TokenKind::Newline | TokenKind::Eof) {
            on_line.push((line_of(token.span.line), place));
        }
    }
    let mut commented = Vec::new();
    for (span, text) in &written.comments {
        commented.push((line_of(span.line), span.col, text.as_str()));
    }
    for (line, place) in on_line {
        lines[line].tokens.push(place);
    }
    for (line, col, text) in commented {
        let line = &mut lines[line];
        line.comment = Some(text);
        if let Some(&last) = line.tokens.last() {
            let end = tokens[last].span.col + written.texts[last].chars().count();
            line.before_comment = col.saturating_sub(end);
        }
    }

    // ----- Write the lines out.
    let mut out = String::new();
    let mut open: Vec<Open> = Vec::new();
    // A blank line was met, and has not been written yet.
    let mut blank = false;
    // The last token of the last line of code that was written.
    let mut last: Option<&TokenKind> = None;
    // The line before this one was code that ended by opening a bracket.
    let mut just_opened = false;

    // The lines of an `inline` block after its first are the block's
    // own, and were written out with it: up to this line, none is a
    // blank line of Zeno.
    let mut block_until = 0;

    for (number, line) in lines.iter().enumerate() {
        if number < block_until {
            continue;
        }
        if line.tokens.is_empty() && line.comment.is_none() {
            blank = true;
            continue;
        }
        let first = line.tokens.first().map(|&place| &tokens[place].kind);
        let closes = first.is_some_and(closer);

        // A blank line is kept where it parts two things, and only one.
        if blank && !out.is_empty() && !just_opened && !closes {
            out.push('\n');
        }
        blank = false;

        // How far in this line goes.
        let indent = if closes {
            // A line that begins by closing brackets goes where the line
            // that opened the last of them is.
            let closed = line.tokens.iter().take_while(|&&place| closer(&tokens[place].kind)).count();
            open.len().checked_sub(closed).and_then(|at| open.get(at)).map_or(0, |bracket| bracket.indent)
        } else {
            let inside = open.last().map_or(0, |bracket| bracket.indent + 1);
            // A statement that goes on from the line before, as in
            // `let total = price +`, is indented once more. Inside
            // `( )` and `[ ]` every line is already a line of one thing.
            let goes_on = first.is_some()
                && last.is_some_and(TokenKind::continues_line)
                && open.last().is_none_or(|bracket| bracket.brace);
            inside + usize::from(goes_on)
        };
        out.push_str(&" ".repeat(indent * STEP));

        // The code, with its spaces.
        let mut width = 0;
        for (nth, &place) in line.tokens.iter().enumerate() {
            let kind = &tokens[place].kind;
            let in_parens = open.last().is_some_and(|bracket| !bracket.brace);
            if nth > 0 && space_between(tokens, place, in_parens) {
                out.push(' ');
                width += 1;
            }
            if let TokenKind::Inline { language, .. } = kind {
                // Code of another language is not laid out: every line
                // of it stays as it was written, and the whole block
                // moves with the line that opens it (decision 61.8).
                let text = &written.texts[place];
                out.push_str(&block(language, text, indent * STEP));
                block_until = number + text.lines().count();
                width += 1;
                continue;
            }
            out.push_str(&written.texts[place]);
            width += written.texts[place].chars().count();
            match kind {
                TokenKind::LBrace => open.push(Open { brace: true, indent }),
                TokenKind::LParen | TokenKind::LBracket => open.push(Open { brace: false, indent }),
                kind if closer(kind) => drop(open.pop()),
                _ => {}
            }
        }

        if let Some(text) = line.comment {
            if width > 0 {
                out.push_str(&" ".repeat(line.before_comment.max(BEFORE_COMMENT)));
            }
            out.push_str(&comment(text));
        }
        out.push('\n');

        if let Some(&place) = line.tokens.last() {
            last = Some(&tokens[place].kind);
        }
        just_opened = line.tokens.last().is_some_and(|&place| opener(&tokens[place].kind));
    }
    out
}

/// An `inline` block as it is written out: its first line, the lines
/// of the other language one step in from it, and the `}` that ends it
/// under the line that opened it. `text` is the block as it was
/// written, from `inline` to that `}`.
///
/// What the lines say is not touched: only the indentation they all
/// share is changed, which is not part of what a block says (see
/// `Lexer::inline_block`), and each keeps how far in it is from the
/// others, which in Python is part of the program.
fn block(language: &str, text: &str, indent: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let inside: Vec<&str> = lines[1..lines.len().saturating_sub(1)].iter().map(|line| line.trim_end()).collect();
    let blank = |c: &char| *c == ' ' || *c == '\t';
    let shared = inside
        .iter()
        .filter(|line| !line.is_empty())
        .map(|line| line.chars().take_while(blank).collect::<String>())
        .reduce(|shared, own| shared.chars().zip(own.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect())
        .unwrap_or_default();
    let mut out = format!("inline {language} {{\n");
    for line in inside {
        if !line.is_empty() {
            out.push_str(&" ".repeat(indent + STEP));
            out.push_str(line.strip_prefix(shared.as_str()).unwrap_or(line));
        }
        out.push('\n');
    }
    out.push_str(&" ".repeat(indent));
    out.push('}');
    out
}

fn opener(kind: &TokenKind) -> bool {
    matches!(kind, TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket)
}

fn closer(kind: &TokenKind) -> bool {
    matches!(kind, TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket)
}

/// True if a value can end with this token: a name, a number, a text,
/// `)` or `]`. What comes after such a token works on that value: a `-`
/// subtracts, a `(` calls, a `[` picks an item.
fn ends_a_value(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(kind, Ident(_) | Int(_) | Float(_) | Str { .. } | True | False | None | RParen | RBracket)
}

/// True if the token at `place` is the `=` of a named argument, as in
/// `Point(x=3.0)`, or of a parameter's value when it is left out and has
/// no type, as in `function(step=1)`. It stands inside `( )`, after a
/// name that begins an argument.
fn names_an_argument(tokens: &[Token], place: usize, in_parens: bool) -> bool {
    let at = |back: usize| place.checked_sub(back).map(|place| &tokens[place].kind);
    in_parens
        && tokens[place].kind == TokenKind::Assign
        && matches!(at(1), Some(TokenKind::Ident(_)))
        && matches!(at(2), Some(TokenKind::LParen | TokenKind::Comma))
}

/// Whether one space stands between the token at `place` and the one
/// ahead of it on the same line. `in_parens` says that the innermost
/// bracket open around them is a `(` or a `[`.
fn space_between(tokens: &[Token], place: usize, in_parens: bool) -> bool {
    use TokenKind::*;
    let (before, after) = (&tokens[place - 1].kind, &tokens[place].kind);
    // The token ahead of `before`, which says whether a `-` there is
    // the sign of a number.
    let earlier = place.checked_sub(2).map(|place| &tokens[place].kind);
    // `Point(x=3.0, y=4.0)`.
    if names_an_argument(tokens, place, in_parens) || names_an_argument(tokens, place - 1, in_parens) {
        return false;
    }
    // These hold on to what is ahead of them: `f(a, b)`, `port: int`, `a.b`.
    if matches!(after, RParen | RBracket | Comma | Colon | Dot) {
        return false;
    }
    // And these to what follows: `(a`, `[1`, `a.b`, `~bits`.
    if matches!(before, LParen | LBracket | Dot | Tilde) {
        return false;
    }
    // `-x`: a minus with no value ahead of it is the sign of what follows.
    if matches!(before, Minus | Plus) && !earlier.is_some_and(ends_a_value) {
        return false;
    }
    match after {
        // `print(x)`, `f(a)(b)`, `function(x) { }`. But `if (a or b) and c`.
        LParen => !(ends_a_value(before) || *before == Function),
        // `items[0]`, `list[int]`. But `return [1, 2]`.
        LBracket => !ends_a_value(before),
        _ => true,
    }
}

/// A comment as it is written out: a space after the `#`.
fn comment(text: &str) -> String {
    let text = text.trim_end();
    match text.chars().nth(1) {
        Some(c) if c.is_alphanumeric() || matches!(c, '`' | '"' | '\'' | '(' | '[') => format!("# {}", &text[1..]),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    /// What a piece of source is formatted to.
    fn formatted(source: &str) -> String {
        match format(source) {
            Ok(text) => text,
            Err(errors) => panic!("{}\n{source}", errors[0].message),
        }
    }

    #[test]
    fn spaces_go_where_they_always_go() {
        assert_eq!(formatted("let  x=1+2*3\n"), "let x = 1 + 2 * 3\n");
        assert_eq!(formatted("print ( x , y )\n"), "print(x, y)\n");
        assert_eq!(formatted("let a=[ 1,2 ,3 ]\nprint(a [ 0 ])\n"), "let a = [1, 2, 3]\nprint(a[0])\n");
        assert_eq!(formatted("let d=[\"a\":1,\"b\" : 2]\n"), "let d = [\"a\": 1, \"b\": 2]\n");
        assert_eq!(
            formatted("function add(a:int,b:int)->int{return a+b}\n"),
            "function add(a: int, b: int) -> int { return a + b }\n"
        );
        assert_eq!(formatted("let p = shapes . Point( 1 , 2 )\nprint(p . x)\n"), "let p = shapes.Point(1, 2)\nprint(p.x)\n");
        assert_eq!(formatted("let n: list [ int ] = [ ]\n"), "let n: list[int] = []\n");
        assert_eq!(formatted("let wait = 500ms\n"), "let wait = 500 ms\n");
        assert_eq!(formatted("if(a or b)and c{\n}\n"), "if (a or b) and c {\n}\n");
        assert_eq!(formatted("let f = function (x: int) -> int { return x }\n"), "let f = function(x: int) -> int { return x }\n");
        assert_eq!(formatted("if x {}\n"), "if x { }\n");
        assert_eq!(formatted("test \"t\" {\n    assert x==1 , f\"x is {x}\"\n}\n"), "test \"t\" {\n    assert x == 1, f\"x is {x}\"\n}\n");
        // The `=` that names an argument has no spaces. Any other has.
        assert_eq!(formatted("let p = Point(x = 3.0,y=-4.0)\n"), "let p = Point(x=3.0, y=-4.0)\n");
        assert_eq!(
            formatted("function greet(who: str, excited: bool=false, times = 1) {\n}\n"),
            "function greet(who: str, excited: bool = false, times=1) {\n}\n"
        );
        assert_eq!(formatted("let r = Reading(\n    sensor = 1,\n    flags = 0\n)\n"), "let r = Reading(\n    sensor=1,\n    flags=0\n)\n");
        assert_eq!(formatted("each(items, function(item) { total=item })\n"), "each(items, function(item) { total = item })\n");
        assert_eq!(formatted("x<<=2\ny = x>>1|3&~x^1\n".replace("x<<=2", "var x = 1\nx <<= 2").as_str()), "var x = 1\nx <<= 2\ny = x >> 1 | 3 & ~x ^ 1\n");
    }

    #[test]
    fn several_values_are_laid_out_with_a_comma_and_a_space() {
        // Decision 57.
        assert_eq!(
            formatted("function  d(a:int,b:int)->(int,int){\n  return a//b,a%b\n}\nlet q,r=d(7,2)\nq , r = r,q\nlet _ ,s=d(1,2)\n"),
            "function d(a: int, b: int) -> (int, int) {\n    return a // b, a % b\n}\nlet q, r = d(7, 2)\nq, r = r, q\nlet _, s = d(1, 2)\n"
        );
    }

    #[test]
    fn a_minus_is_a_sign_or_a_subtraction() {
        assert_eq!(formatted("let a = - 1\n"), "let a = -1\n");
        assert_eq!(formatted("let b = a-1\n"), "let b = a - 1\n");
        assert_eq!(formatted("let c = a - - 1\n"), "let c = a - -1\n");
        assert_eq!(formatted("let d = [ - 1, - 2]\n"), "let d = [-1, -2]\n");
        assert_eq!(formatted("let e = f( - x) * - y\n"), "let e = f(-x) * -y\n");
        assert_eq!(formatted("let g = (a) - 1\n"), "let g = (a) - 1\n");
        assert_eq!(formatted("let h = a[0] - 1\n"), "let h = a[0] - 1\n");
        assert_eq!(formatted("function f() -> int {\n    return - 1\n}\n"), "function f() -> int {\n    return -1\n}\n");
        assert_eq!(formatted("let i = 2 ** - 1\n"), "let i = 2 ** -1\n");
    }

    #[test]
    fn a_line_is_indented_by_the_brackets_open_around_it() {
        assert_eq!(
            formatted("if a {\nif b {\nprint(1)\n}\n} else {\n        print(2)\n}\n"),
            "if a {\n    if b {\n        print(1)\n    }\n} else {\n    print(2)\n}\n"
        );
        // A list or a call over several lines.
        assert_eq!(
            formatted("let rows = [\n[1, 2],\n  [3, 4],\n    ]\nprint(\nrows,\n)\n"),
            "let rows = [\n    [1, 2],\n    [3, 4],\n]\nprint(\n    rows,\n)\n"
        );
        // Two brackets opened on one line indent what follows once.
        assert_eq!(
            formatted("each(items, function(item) {\nprint(item)\n        })\n"),
            "each(items, function(item) {\n    print(item)\n})\n"
        );
        // A statement that goes on after an operator.
        assert_eq!(
            formatted("let total = price +\ntax +\n        tip\nprint(total)\n"),
            "let total = price +\n    tax +\n    tip\nprint(total)\n"
        );
        // Inside brackets a line is already one of the bracket's lines.
        assert_eq!(formatted("print(price +\ntax)\n"), "print(price +\n    tax)\n");
        // A tab is not indentation in Zeno's layout.
        assert_eq!(formatted("if a {\n\tprint(1)\n}\n"), "if a {\n    print(1)\n}\n");
    }

    #[test]
    fn blank_lines_part_things_and_there_is_one() {
        assert_eq!(formatted("\n\nlet a = 1\n\n\n\nlet b = 2\n\n"), "let a = 1\n\nlet b = 2\n");
        // None at the top of a block, and none at its bottom.
        assert_eq!(
            formatted("function f() {\n\n    print(1)\n\n    print(2)\n\n}\n"),
            "function f() {\n    print(1)\n\n    print(2)\n}\n"
        );
        // A file with no line break at its end gets one.
        assert_eq!(formatted("let a = 1"), "let a = 1\n");
        assert_eq!(formatted(""), "");
        // Spaces at the end of a line go.
        assert_eq!(formatted("let a = 1   \n"), "let a = 1\n");
    }

    #[test]
    fn comments_stay_where_they_are() {
        assert_eq!(formatted("#a note\nlet a = 1 #why\n"), "# a note\nlet a = 1  # why\n");
        // A blank line after a comment stays, also at the top of a block.
        assert_eq!(
            formatted("function f() {  # why\n\n    # About the body.\n\n    print(1)\n}\n"),
            "function f() {  # why\n    # About the body.\n\n    print(1)\n}\n"
        );
        // A comment is indented as the code around it is.
        assert_eq!(
            formatted("if a {\n# first\nprint(1)\n        # last\n}\n"),
            "if a {\n    # first\n    print(1)\n    # last\n}\n"
        );
        // Comments that the author lined up stay lined up.
        assert_eq!(
            formatted("let size = 4      # bytes\nlet first=0      # where the oldest is\n"),
            "let size = 4      # bytes\nlet first = 0      # where the oldest is\n"
        );
        // What stands after the `#` is the author's.
        assert_eq!(formatted("#     zeno run x.zeno\n#!/usr/bin/zeno\n#\n#---\n"), "#     zeno run x.zeno\n#!/usr/bin/zeno\n#\n#---\n");
        // A `#` inside a text is not a comment.
        assert_eq!(formatted("print(\"#1\")  # one\n"), "print(\"#1\")  # one\n");
    }

    #[test]
    fn what_was_written_is_kept_as_written() {
        // Numbers and texts are not spelled anew.
        assert_eq!(formatted("let a = 0x4000_4400\nlet b = 'it'\nlet c = f\"{a:>8}\"\n"), "let a = 0x4000_4400\nlet b = 'it'\nlet c = f\"{a:>8}\"\n");
        // Line breaks stay where the author put them.
        assert_eq!(
            formatted("match x {\n    case 1 { print(\"one\") }\n    case _ { }\n}\n"),
            "match x {\n    case 1 { print(\"one\") }\n    case _ { }\n}\n"
        );
    }

    #[test]
    fn every_example_is_laid_out_this_way() {
        fn files(folder: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(folder).expect("a folder of examples") {
                let path = entry.expect("an example").path();
                if path.is_dir() {
                    files(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "zeno") {
                    found.push(path);
                }
            }
        }
        let mut examples = Vec::new();
        files(std::path::Path::new("examples"), &mut examples);
        assert!(examples.len() > 20);
        for path in examples {
            let source = std::fs::read_to_string(&path).expect("the example can be read");
            assert!(formatted(&source) == source, "{} is not laid out as `zeno format` lays it out", path.display());
            // The indentation is worked out from the brackets alone: take
            // it all away, and the formatter puts the same back.
            let flat = flattened(&source);
            assert!(formatted(&flat) == source, "{} is indented otherwise from nothing", path.display());
            // So are the spaces: with three between every two tokens,
            // the formatter gives the same file.
            let wide = spread(&source);
            assert!(wide != source);
            let narrow = formatted(&wide);
            let differs = narrow.lines().zip(source.lines()).find(|(new, old)| new != old);
            assert!(narrow == source, "{} is spaced otherwise from nothing: {differs:?}", path.display());
        }
    }

    /// The source with no indentation. The lines of an `inline` block are
    /// not Zeno's to indent: they move as far as the line that opens the
    /// block moves, and keep the shape they were written in.
    fn flattened(source: &str) -> String {
        let mut flat = String::new();
        // How far in the line that opened a block was, inside one.
        let mut block: Option<usize> = None;
        for line in source.lines() {
            match block {
                Some(indent) => {
                    flat.push_str(line.get(indent..).unwrap_or("").trim_end());
                    if line.len() == indent + 1 && line.ends_with('}') {
                        block = None;
                    }
                }
                None => {
                    let written = line.trim_start();
                    let words: Vec<&str> = written.split_whitespace().collect();
                    if let [.., "inline", _, "{"] = words[..] {
                        block = Some(line.len() - written.len());
                    }
                    flat.push_str(written);
                }
            }
            flat.push('\n');
        }
        flat
    }

    /// The source with three spaces between every two tokens of a line,
    /// and each comment as far from its code as it was.
    fn spread(source: &str) -> String {
        let written = crate::lexer::lex_written(source).expect("the example has no mistake");
        let mut lines = vec![String::new(); source.lines().count()];
        // Where the last token written on each line ends, in the source.
        let mut ends = vec![0; lines.len()];
        for (token, text) in written.tokens.iter().zip(&written.texts) {
            if text.is_empty() {
                continue;
            }
            let line = token.span.line - 1;
            if !lines[line].is_empty() {
                lines[line] += "   ";
            }
            lines[line] += text;
            ends[line] = token.span.col + text.chars().count();
        }
        for (span, text) in &written.comments {
            let line = span.line - 1;
            if !lines[line].is_empty() {
                lines[line] += &" ".repeat(span.col - ends[line]);
            }
            lines[line] += text;
        }
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn a_file_with_a_mistake_is_not_formatted() {
        let errors = format("let a = = 1\n").expect_err("it has a mistake");
        assert_eq!(errors[0].message, "expected an expression, found `=`");
        assert!(format("let a = \"open\n").is_err());
        assert!(format("if a {\n").is_err());
    }

    #[test]
    fn an_inline_block_moves_with_its_line_and_is_not_laid_out() {
        // The lines of another language are not Zeno's to lay out: no
        // space is added around `*`, the blank line stays, and Python
        // keeps how far in each line is from the others. The block is
        // set one step in from the line that opens it.
        let messy = "function f( )->int{\n  let lib   =   inline   c   {\n        int twice(int x) {\n            return 2*x;   \n        }\n\n  }\n  return lib.twice( 4 )\n}\nlet m = inline python {\n  def mean(xs):\n      return sum(xs)/len(xs)\n}\n";
        let once = formatted(messy);
        assert_eq!(
            once,
            "function f() -> int {\n    let lib = inline c {\n        int twice(int x) {\n            return 2*x;\n        }\n\n    }\n    return lib.twice(4)\n}\nlet m = inline python {\n    def mean(xs):\n        return sum(xs)/len(xs)\n}\n"
        );
        assert_eq!(formatted(&once), once);
    }

    #[test]
    fn formatting_twice_is_formatting_once() {
        let messy = "class  Queue{\n\n  size:int      # how many fit\n\n\n  function put(self,byte:u8)->bool{\n\tif self.size==0{return false}\n  return true\n }\n}\n";
        let once = formatted(messy);
        assert_eq!(
            once,
            "class Queue {\n    size: int      # how many fit\n\n    function put(self, byte: u8) -> bool {\n        if self.size == 0 { return false }\n        return true\n    }\n}\n"
        );
        assert_eq!(formatted(&once), once);
    }
}
