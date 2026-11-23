//! `zeno convert program.py`: how a value of Python is written in Zeno
//! (docs/convert.md, decisions chapter 62).
//!
//! Every rule here answers one question: is what Zeno does with these
//! words what Python does with those? Where the answer needs the type
//! of a value and the type is not known, the answer is no, and the
//! piece is kept as Python.

use super::*;

/// A value written as Zeno: its words, what it is, and how tightly its
/// outermost operator binds, so that brackets go where they are needed.
#[derive(Debug, Clone)]
pub(super) struct Val {
    pub text: String,
    pub ty: Ty,
    pub prec: u8,
}

pub(super) const OR: u8 = 1;
pub(super) const AND: u8 = 2;
pub(super) const NOT: u8 = 3;
pub(super) const CMP: u8 = 4;
const BIT_OR: u8 = 5;
const BIT_XOR: u8 = 6;
const BIT_AND: u8 = 7;
const SHIFT: u8 = 8;
const SUM: u8 = 9;
const PRODUCT: u8 = 10;
const NEG: u8 = 11;
const POWER: u8 = 12;
pub(super) const ATOM: u8 = 13;

pub(super) fn atom(text: impl Into<String>, ty: Ty) -> Val {
    Val { text: text.into(), ty, prec: ATOM }
}

/// A value whose type this round does not know yet. What is written of
/// it is thrown away: only the last round's words are kept.
fn later() -> Val {
    atom("", Ty::Unset)
}

impl Val {
    /// The words, in brackets if they bind looser than `min`.
    pub(super) fn at(&self, min: u8) -> String {
        if self.prec < min {
            format!("({})", self.text)
        } else {
            self.text.clone()
        }
    }
}

fn is_number(ty: &Ty) -> bool {
    matches!(ty, Ty::Int | Ty::Float)
}

/// A whole number written as a float: `0` as `0.0`, and anything else
/// through `float( )`.
pub(super) fn as_float(value: &Val) -> String {
    if !value.text.is_empty() && value.text.chars().all(|c| c.is_ascii_digit()) {
        format!("{}.0", value.text)
    } else {
        format!("float({})", value.text)
    }
}

/// A number of Python as Zeno writes it.
fn number(text: &str, line: usize) -> Kept<Val> {
    let plain = text.to_lowercase().replace('_', "");
    if plain.ends_with('j') {
        return keep(line, "a complex number");
    }
    let radix = match plain.get(..2) {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 10,
    };
    if radix != 10 {
        return match i64::from_str_radix(&plain[2..], radix) {
            Ok(_) => Ok(atom(text.to_lowercase(), Ty::Int)),
            Err(_) => keep(line, format!("`{text}` is larger than an int of Zeno holds")),
        };
    }
    if plain.contains('.') || plain.contains('e') {
        if plain.parse::<f64>().is_err() {
            return keep(line, format!("the number `{text}`"));
        }
        // Zeno wants a digit on each side of the point.
        let (mantissa, exponent) = match plain.split_once('e') {
            Some((mantissa, exponent)) => (mantissa.to_string(), format!("e{exponent}")),
            None => (plain.clone(), String::new()),
        };
        let mut mantissa = mantissa;
        if mantissa.starts_with('.') {
            mantissa.insert(0, '0');
        }
        if mantissa.ends_with('.') {
            mantissa.push('0');
        }
        if !mantissa.contains('.') {
            mantissa.push_str(".0");
        }
        return Ok(atom(format!("{mantissa}{exponent}"), Ty::Float));
    }
    match plain.parse::<i64>() {
        Ok(value) => Ok(atom(value.to_string(), Ty::Int)),
        Err(_) => keep(line, format!("`{text}` is larger than an int of Zeno holds")),
    }
}

/// The text that the words between the quotes of Python stand for.
fn unescaped(body: &str, line: usize) -> Kept<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let Some(&next) = chars.get(i + 1) else {
            out.push('\\');
            break;
        };
        i += 2;
        match next {
            '\n' => {}
            '\\' | '\'' | '"' => out.push(next),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'a' => out.push('\u{7}'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0'..='7' => {
                let mut digits = String::from(next);
                while digits.len() < 3 && chars.get(i).is_some_and(|c| ('0'..='7').contains(c)) {
                    digits.push(chars[i]);
                    i += 1;
                }
                let code = u32::from_str_radix(&digits, 8).unwrap_or(0);
                out.push(char::from_u32(code).unwrap_or('\0'));
            }
            'x' | 'u' | 'U' => {
                let count = match next {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let digits: String = chars.iter().skip(i).take(count).collect();
                let code = if digits.len() == count { u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32) } else { None };
                match code {
                    Some(c) => out.push(c),
                    None => return keep(line, "a text with a `\\x` or `\\u` that names no character"),
                }
                i += count;
            }
            'N' => return keep(line, "a text with a character given by its name (`\\N{...}`)"),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

/// One part of an f-string: words as they are, or a value in braces.
enum Part {
    Words(String),
    Value { expr: String, spec: String },
}

/// Cut the words of an f-string at its braces.
fn f_parts(body: &str, line: usize) -> Kept<Vec<Part>> {
    let chars: Vec<char> = body.chars().collect();
    let mut parts = Vec::new();
    let mut words = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' && chars.get(i + 1) == Some(&'{') {
            words.push('{');
            i += 2;
        } else if c == '}' && chars.get(i + 1) == Some(&'}') {
            words.push('}');
            i += 2;
        } else if c == '}' {
            return keep(line, "an f-string with a `}` that closes nothing");
        } else if c == '{' {
            if !words.is_empty() {
                parts.push(Part::Words(std::mem::take(&mut words)));
            }
            // To the `}` that closes it: brackets and texts inside count.
            let mut depth = 0usize;
            let mut quote: Option<char> = None;
            let mut colon: Option<usize> = None;
            let mut j = i + 1;
            loop {
                let Some(&d) = chars.get(j) else {
                    return keep(line, "an f-string with a `{` that is never closed");
                };
                match quote {
                    Some(q) => {
                        if d == q {
                            quote = None;
                        }
                    }
                    None => match d {
                        '\'' | '"' => quote = Some(d),
                        '(' | '[' | '{' if colon.is_none() => depth += 1,
                        ')' | ']' if colon.is_none() => depth = depth.saturating_sub(1),
                        '}' if depth > 0 && colon.is_none() => depth -= 1,
                        '{' => return keep(line, "an f-string whose width or precision is itself a value in braces"),
                        '}' => break,
                        ':' if depth == 0 && colon.is_none() => colon = Some(j),
                        '!' if depth == 0 && colon.is_none() && chars.get(j + 1) != Some(&'=') => {
                            return keep(line, "an f-string with `!r`, `!s` or `!a`");
                        }
                        _ => {}
                    },
                }
                j += 1;
            }
            let end = colon.unwrap_or(j);
            let expr: String = chars[i + 1..end].iter().collect();
            let spec: String = if colon.is_some() { chars[end + 1..j].iter().collect() } else { String::new() };
            if expr.trim_end().ends_with('=') && !expr.trim_end().ends_with("==") {
                return keep(line, "an f-string with `{name=}`");
            }
            if expr.trim().is_empty() {
                return keep(line, "an f-string with empty braces");
            }
            parts.push(Part::Value { expr, spec });
            i = j + 1;
        } else {
            words.push(c);
            i += 1;
        }
    }
    if !words.is_empty() {
        parts.push(Part::Words(words));
    }
    Ok(parts)
}

/// What comes after the `:` in the braces of an f-string, as far as
/// Zeno writes the same: `[<>^][0][width][.precision][f d x X b]`.
struct Spec {
    zero: bool,
    precision: bool,
    kind: Option<char>,
}

fn spec_of(spec: &str) -> Option<Spec> {
    let mut rest = spec;
    if let Some(after) = rest.strip_prefix(['<', '>', '^']) {
        rest = after;
    }
    let zero = rest.starts_with('0');
    rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let mut precision = false;
    if let Some(after) = rest.strip_prefix('.') {
        let digits = after.trim_start_matches(|c: char| c.is_ascii_digit());
        if digits.len() == after.len() {
            return None;
        }
        precision = true;
        rest = digits;
    }
    let kind = match rest {
        "" => None,
        "f" | "d" | "x" | "X" | "b" => rest.chars().next(),
        _ => return None,
    };
    Some(Spec { zero, precision, kind })
}

/// The kinds of error that both languages have, by Python's name.
pub(super) fn error_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "ValueError" => "ValueError",
        "KeyError" => "KeyError",
        "IndexError" => "IndexError",
        "ZeroDivisionError" => "ZeroDivisionError",
        "OverflowError" => "OverflowError",
        "Exception" => "Error",
        _ => return None,
    })
}

/// `-3`, written so: an index or a bound counted from the end.
fn from_the_end(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        E::Unary("-", of) => match &of.kind {
            E::Number(text) if text.chars().all(|c| c.is_ascii_digit()) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

/// `name is None` (or, for `wanted`, `name is not None`): the name.
pub(super) fn none_test(expr: &Expr, is_not: bool) -> Option<&str> {
    let E::Compare(first, rest) = &expr.kind else { return None };
    let [(op, other)] = rest.as_slice() else { return None };
    let wanted = if is_not { ["is not", "!="] } else { ["is", "=="] };
    match (&first.kind, &other.kind) {
        (E::Name(name), E::None) if wanted.contains(op) => Some(name),
        _ => None,
    }
}

impl Writer<'_> {
    fn unknown<T>(&self, line: usize, what: &str) -> Kept<T> {
        keep(line, format!("the type of {what} is not known, so what Python does with it here cannot be told"))
    }

    /// Whether the name is a variable here: of this function, of a list
    /// made from a loop, or of the file.
    pub(super) fn is_var(&self, name: &str) -> bool {
        self.temps.iter().any(|(temp, _)| temp == name) || self.is_local(name) || self.env.module_vars.contains(name)
    }

    pub(super) fn is_local(&self, name: &str) -> bool {
        self.in_function && !self.globals.contains(name) && (self.params.contains(name) || self.plan.locals.contains(name))
    }

    /// The type of a variable, as the rounds so far found it.
    pub(super) fn whole_ty(&self, name: &str) -> Ty {
        let scope = if self.is_local(name) { self.scope.clone() } else { String::new() };
        self.settled(self.env.known.vars.get(&(scope, name.to_string())).cloned().unwrap_or(Ty::Unset))
    }

    /// The type of a variable on this line: without the `none` if a test
    /// above has shown that it is not `none`.
    pub(super) fn var_ty(&self, name: &str) -> Ty {
        if let Some((_, ty)) = self.temps.iter().rev().find(|(temp, _)| temp == name) {
            return self.settled(ty.clone());
        }
        if name == "self" && self.params.contains("self") {
            if let Some(class) = &self.class {
                return Ty::Object(class.clone());
            }
        }
        match self.whole_ty(name) {
            Ty::Opt(inner) if self.narrow.iter().any(|narrowed| narrowed == name) => *inner,
            ty => ty,
        }
    }

    /// In the last round, a type that no round found is one that cannot
    /// be known.
    pub(super) fn settled(&self, ty: Ty) -> Ty {
        fn settle(ty: Ty) -> Ty {
            match ty {
                Ty::Unset => Ty::Dyn,
                Ty::List(item) => Ty::List(Box::new(settle(*item))),
                Ty::Dict(key, value) => Ty::Dict(Box::new(settle(*key)), Box::new(settle(*value))),
                Ty::Opt(inner) => Ty::Opt(Box::new(settle(*inner))),
                Ty::Several(items) => Ty::Several(items.into_iter().map(settle).collect()),
                other => other,
            }
        }
        if self.env.last {
            settle(ty)
        } else {
            ty
        }
    }

    /// A name of Python that Zeno can have as a name too.
    pub(super) fn zeno_name(&self, name: &str, line: usize) -> Kept<()> {
        if ZENO_WORDS.contains(&name) {
            return keep(line, format!("`{name}` is a word of Zeno, and cannot be a name there"));
        }
        if ZENO_OWN.contains(&name) || name == self.env.block {
            return keep(line, format!("`{name}` is a name Zeno has a use for already"));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return keep(line, format!("`{name}` has a letter that a name of Zeno cannot have"));
        }
        Ok(())
    }

    pub(super) fn note(&mut self, note: Note, line: usize) {
        self.notes.push((note, line));
    }

    /// A value is put where it is shown: by `print`, by `str`, in an
    /// f-string. Say so where Zeno shows it otherwise, and keep the
    /// piece where what Python shows has no match.
    pub(super) fn shown(&mut self, ty: &Ty, line: usize) -> Kept<()> {
        // The notes for a value of this type; `inside` a list, a dict or
        // an object a text has its quotes, and they are not the same.
        fn notes(ty: &Ty, inside: bool, data: &dyn Fn(&str) -> bool, into: &mut Vec<Note>) -> Result<(), &'static str> {
            match ty {
                Ty::Int | Ty::Unset => {}
                Ty::Str if !inside => {}
                Ty::Float => into.push(Note::Exponent),
                Ty::Str | Ty::Bool | Ty::NoneT | Ty::Foreign => into.push(Note::Shown),
                Ty::Opt(inner) => {
                    into.push(Note::Shown);
                    notes(inner, inside, data, into)?;
                }
                Ty::List(item) => notes(item, true, data, into)?,
                Ty::Dict(key, value) => {
                    into.push(Note::Shown);
                    notes(key, true, data, into)?;
                    notes(value, true, data, into)?;
                }
                Ty::Object(class) if data(class) => into.push(Note::Shown),
                Ty::Object(_) => return Err("an object is shown, and Python shows where in memory it is"),
                Ty::Error if !inside => into.push(Note::ErrorWords),
                Ty::Error => return Err("an error inside a list or a dict is shown"),
                Ty::Several(_) => return Err("several values are shown as one, which is a tuple in Python"),
                Ty::Dyn => return Err("a value is shown whose type is not known, and Zeno may show it otherwise"),
            }
            Ok(())
        }
        let env = self.env;
        let data = |class: &str| env.shapes.get(class).is_some_and(|shape| shape.data);
        let mut found = Vec::new();
        if let Err(why) = notes(ty, false, &data, &mut found) {
            return keep(line, why);
        }
        for note in found {
            if !self.notes.contains(&(note, line)) {
                self.note(note, line);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ Values

    /// A value of Python as Zeno writes it. In a round that is not the
    /// last, a value that cannot be written yet is one whose type is to
    /// come: a later round, which knows more, may write it.
    pub(super) fn expr(&mut self, expr: &Expr) -> Kept<Val> {
        match self.expr_now(expr) {
            Err(_) if !self.env.last => Ok(later()),
            done => done,
        }
    }

    fn expr_now(&mut self, expr: &Expr) -> Kept<Val> {
        let line = expr.line;
        match &expr.kind {
            E::Name(name) => self.name(name, line),
            E::Number(text) => number(text, line),
            E::Str(lit) => self.text(lit, line),
            E::True => Ok(atom("true", Ty::Bool)),
            E::False => Ok(atom("false", Ty::Bool)),
            E::None => Ok(atom("none", Ty::NoneT)),
            E::List(items) => {
                let mut texts = Vec::new();
                let mut ty = Ty::Unset;
                for item in items {
                    let value = self.expr(item)?;
                    ty = join(&ty, &value.ty);
                    texts.push(value.text);
                }
                Ok(atom(format!("[{}]", texts.join(", ")), Ty::List(Box::new(ty))))
            }
            E::Dict(pairs) => {
                let mut texts = Vec::new();
                let (mut keys, mut values) = (Ty::Unset, Ty::Unset);
                for (key, value) in pairs {
                    let key = self.expr(key)?;
                    let value = self.expr(value)?;
                    self.dict_key(&key.ty, line)?;
                    keys = join(&keys, &key.ty);
                    values = join(&values, &value.ty);
                    texts.push(format!("{}: {}", key.text, value.text));
                }
                let text = if texts.is_empty() { "[:]".to_string() } else { format!("[{}]", texts.join(", ")) };
                Ok(atom(text, Ty::Dict(Box::new(keys), Box::new(values))))
            }
            E::Tuple(_) => keep(line, "a tuple: Zeno has several values at once only where a function returns them or names take them"),
            E::Unary(op, of) => self.unary(op, of, line),
            E::Binary(op, left, right) => {
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                self.binary(op, &left, &right, line)
            }
            E::Logic(op, left, right) => {
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                let level = if *op == "or" { OR } else { AND };
                let text = format!("{} {op} {}", left.at(level), right.at(level + 1));
                match (&left.ty, &right.ty) {
                    (Ty::Bool, Ty::Bool) => Ok(Val { text, ty: Ty::Bool, prec: level }),
                    (Ty::Unset, _) | (_, Ty::Unset) => Ok(later()),
                    _ => keep(line, format!("`{op}` between values that are not `True` or `False` gives one of the two values, and in Zeno it gives `true` or `false`")),
                }
            }
            E::Compare(first, rest) => self.compare(first, rest, line),
            E::Call { function, args, named } => match &function.kind {
                E::Name(name) => self.call_name(name, args, named, line),
                E::Attr(of, method) => self.call_method(of, method, args, named, line),
                _ => keep(line, "a call of something other than a name"),
            },
            E::Attr(of, name) => self.attr(of, name, line),
            E::Index(of, index) => self.index(of, index, line),
            E::Slice { of, from, to } => self.slice(of, from.as_deref(), to.as_deref(), line),
            E::Lambda(..) => keep(line, "a `lambda`, which is not translated yet"),
            E::IfElse { .. } => keep(line, "an `if` in the middle of a line, in a place other than `name = a if c else b` and `return a if c else b`"),
            E::ListOf { item, name, items, condition } => self.list_of(item, name, items, condition.as_deref(), line),
        }
    }

    fn dict_key(&self, ty: &Ty, line: usize) -> Kept<()> {
        match ty {
            Ty::Str | Ty::Int | Ty::Unset => Ok(()),
            Ty::Dyn => self.unknown(line, "the key of a dict"),
            other => keep(line, format!("a dict whose key is {}: a dict of Zeno has texts or whole numbers for keys", described(other))),
        }
    }

    fn name(&mut self, name: &str, line: usize) -> Kept<Val> {
        if self.is_var(name) {
            self.zeno_name(name, line)?;
            return Ok(atom(name, self.var_ty(name)));
        }
        let env = self.env;
        if env.kept.contains_key(name) {
            return Ok(atom(format!("{}.{name}", env.block), Ty::Foreign));
        }
        if let Some((Module::Math, real)) = env.from_names.get(name) {
            if matches!(real.as_str(), "pi" | "e") {
                self.need_math = true;
                return Ok(atom(format!("math.{real}"), Ty::Float));
            }
        }
        if env.defs.contains_key(name) {
            return keep(line, format!("the function `{name}` is used as a value, which is not translated yet"));
        }
        if env.shapes.contains_key(name) {
            return keep(line, format!("the class `{name}` is used as a value"));
        }
        if env.modules.contains_key(name) {
            return keep(line, format!("the module `{name}` is used as a value"));
        }
        keep(line, format!("`{name}` is not a name the converter knows the meaning of here"))
    }

    fn text(&mut self, lit: &StrLit, line: usize) -> Kept<Val> {
        match lit.prefix.as_str() {
            "" | "u" => {
                let value = unescaped(&lit.body, line)?;
                self.quoted(&value, line)
            }
            "r" => self.quoted(&lit.body, line),
            "f" | "rf" | "fr" => self.f_string(lit, line),
            _ => keep(line, "a text of bytes (`b\"...\"`)"),
        }
    }

    fn quoted(&self, value: &str, line: usize) -> Kept<Val> {
        if self.quote == '\'' && value.contains(['\'', '"', '\\', '\n', '{', '}']) {
            return keep(line, "a text with a quote, a brace or a `\\` in it, inside the braces of an f-string");
        }
        match zeno_text(value, self.quote) {
            Some(text) => Ok(atom(text, Ty::Str)),
            None => keep(line, "a text with a character that Zeno has no way to write"),
        }
    }

    fn f_string(&mut self, lit: &StrLit, line: usize) -> Kept<Val> {
        if self.quote == '\'' {
            return keep(line, "an f-string inside the braces of an f-string");
        }
        let raw = lit.prefix != "f";
        let mut out = String::from("f\"");
        for part in f_parts(&lit.body, line)? {
            match part {
                Part::Words(words) => {
                    let words = if raw { words } else { unescaped(&words, line)? };
                    let Some(text) = zeno_text(&words, '"') else {
                        return keep(line, "a text with a character that Zeno has no way to write");
                    };
                    out.push_str(&text[1..text.len() - 1].replace('{', "{{").replace('}', "}}"));
                }
                Part::Value { expr, spec } => {
                    let tree = expression_of(expr.trim(), line)?;
                    self.quote = '\'';
                    let value = self.expr(&tree);
                    self.quote = '"';
                    let value = value?;
                    if value.text.contains([':', '{', '}', '"', '\\']) {
                        return keep(line, "a value in the braces of an f-string that Zeno cannot write between braces");
                    }
                    if spec.is_empty() {
                        self.shown(&value.ty, line)?;
                    } else {
                        let Some(parsed) = spec_of(&spec) else {
                            return keep(line, format!("`:{spec}` in an f-string, which Zeno does not write the same"));
                        };
                        let fits = match (&value.ty, parsed.kind) {
                            (Ty::Unset, _) => true,
                            (Ty::Int, Some('d' | 'x' | 'X' | 'b')) => true,
                            (Ty::Int | Ty::Float, Some('f')) => true,
                            (Ty::Int, None) => !parsed.precision,
                            (Ty::Float, None) => {
                                self.note(Note::Exponent, line);
                                !parsed.precision
                            }
                            (Ty::Str, None) => !parsed.precision && !parsed.zero,
                            _ => false,
                        };
                        if !fits {
                            return keep(line, format!("`:{spec}` in an f-string on {}", described(&value.ty)));
                        }
                    }
                    out.push('{');
                    out.push_str(&value.text);
                    if !spec.is_empty() {
                        out.push(':');
                        out.push_str(&spec);
                    }
                    out.push('}');
                }
            }
        }
        out.push('"');
        Ok(atom(out, Ty::Str))
    }

    /// A value where Python asks only whether it is true: the test of
    /// an `if` or a `while`, what `not` is in front of, either side of
    /// `and` and `or` there. Zeno asks this of `true` and `false` only,
    /// so the question Python asks of the value is written out.
    pub(super) fn truth(&mut self, expr: &Expr) -> Kept<Val> {
        match self.truth_now(expr) {
            Err(_) if !self.env.last => Ok(Val { ty: Ty::Bool, ..later() }),
            done => done,
        }
    }

    fn truth_now(&mut self, expr: &Expr) -> Kept<Val> {
        let line = expr.line;
        match &expr.kind {
            E::Logic(op, left, right) => {
                let level = if *op == "or" { OR } else { AND };
                let first = self.truth(left)?;
                // `x is not None and x > 0`: on the right, `x` is not none.
                let mark = self.narrow.len();
                if let Some(name) = none_test(left, *op == "and") {
                    self.narrow.push(name.to_string());
                }
                let second = self.truth(right);
                self.narrow.truncate(mark);
                let second = second?;
                Ok(Val { text: format!("{} {op} {}", first.at(level), second.at(level + 1)), ty: Ty::Bool, prec: level })
            }
            E::Unary("not", of) => {
                let value = self.truth(of)?;
                Ok(Val { text: format!("not {}", value.at(NOT)), ty: Ty::Bool, prec: NOT })
            }
            _ => {
                let value = self.expr(expr)?;
                let empty = format!("{0}{0}", self.quote);
                let text = match &value.ty {
                    Ty::Bool => return Ok(value),
                    Ty::Unset => return Ok(Val { ty: Ty::Bool, ..value }),
                    Ty::Int => format!("{} != 0", value.at(CMP + 1)),
                    Ty::Float => format!("{} != 0.0", value.at(CMP + 1)),
                    Ty::Str => format!("{} != {empty}", value.at(CMP + 1)),
                    Ty::List(_) | Ty::Dict(..) => format!("len({}) > 0", value.text),
                    Ty::Opt(inner) if matches!(**inner, Ty::Object(_)) => format!("{} is not none", value.at(CMP + 1)),
                    Ty::Dyn => return self.unknown(line, "this value, which is used as a condition"),
                    other => {
                        return keep(line, format!("{} is used as a condition, and Zeno has no one question that is what Python asks of it", described(other)));
                    }
                };
                Ok(Val { text, ty: Ty::Bool, prec: CMP })
            }
        }
    }

    fn unary(&mut self, op: &str, of: &Expr, line: usize) -> Kept<Val> {
        if op == "not" {
            let value = self.truth(of)?;
            return Ok(Val { text: format!("not {}", value.at(NOT)), ty: Ty::Bool, prec: NOT });
        }
        let value = self.expr(of)?;
        match (op, &value.ty) {
            (_, Ty::Unset) => Ok(later()),
            ("+", Ty::Int | Ty::Float) => Ok(value),
            ("-", Ty::Int | Ty::Float) => Ok(Val { text: format!("-{}", value.at(NEG)), ty: value.ty.clone(), prec: NEG }),
            ("~", Ty::Int) => Ok(Val { text: format!("~{}", value.at(NEG)), ty: Ty::Int, prec: NEG }),
            (_, Ty::Dyn) => self.unknown(line, &format!("what `{op}` is in front of")),
            (_, other) => keep(line, format!("`{op}` in front of {}", described(other))),
        }
    }

    pub(super) fn binary(&mut self, op: &str, left: &Val, right: &Val, line: usize) -> Kept<Val> {
        if left.ty == Ty::Unset || right.ty == Ty::Unset {
            return Ok(later());
        }
        let level = match op {
            "|" => BIT_OR,
            "^" => BIT_XOR,
            "&" => BIT_AND,
            "<<" | ">>" => SHIFT,
            "+" | "-" => SUM,
            "*" | "/" | "//" | "%" => PRODUCT,
            _ => POWER,
        };
        let both_numbers = is_number(&left.ty) && is_number(&right.ty);
        let both_ints = left.ty == Ty::Int && right.ty == Ty::Int;
        let ty = match op {
            "+" if left.ty == Ty::Str && right.ty == Ty::Str => Ty::Str,
            "+" if matches!((&left.ty, &right.ty), (Ty::List(_), Ty::List(_))) => join(&left.ty, &right.ty),
            "+" | "-" | "*" | "//" | "%" | "**" if both_ints => Ty::Int,
            "+" | "-" | "*" | "/" | "//" | "%" | "**" if both_numbers => Ty::Float,
            "|" | "^" | "&" | "<<" | ">>" if both_ints => Ty::Int,
            _ if left.ty == Ty::Dyn || right.ty == Ty::Dyn => return self.unknown(line, &format!("a value on one side of `{op}`")),
            "%" if left.ty == Ty::Str => return keep(line, "`%` that puts values into a text: an f-string does this in Zeno"),
            _ => return keep(line, format!("`{op}` between {} and {}", described(&left.ty), described(&right.ty))),
        };
        // `**` joins to the right; the others to the left.
        let text = if level == POWER {
            format!("{} ** {}", left.at(ATOM), right.at(NEG))
        } else {
            format!("{} {op} {}", left.at(level), right.at(level + 1))
        };
        Ok(Val { text, ty, prec: level })
    }

    /// Whether `==` between the two asks in Zeno what it asks in Python.
    fn equal_ok(&self, a: &Ty, b: &Ty, line: usize) -> Kept<()> {
        let env = self.env;
        let ok = match (a, b) {
            (Ty::Unset, _) | (_, Ty::Unset) => true,
            (Ty::Foreign, other) | (other, Ty::Foreign) => crosses(other),
            (Ty::Dyn, _) | (_, Ty::Dyn) => return self.unknown(line, "a value that is compared"),
            (a, b) if is_number(a) && is_number(b) => true,
            (Ty::Str, Ty::Str) | (Ty::Bool, Ty::Bool) | (Ty::NoneT, Ty::NoneT) => true,
            (Ty::List(a), Ty::List(b)) => return self.equal_ok(a, b, line),
            (Ty::Dict(ak, av), Ty::Dict(bk, bv)) => return self.equal_ok(ak, bk, line).and(self.equal_ok(av, bv, line)),
            (Ty::Opt(a), Ty::Opt(b)) => return self.equal_ok(a, b, line),
            (Ty::Opt(a), b) | (b, Ty::Opt(a)) => *b == Ty::NoneT || self.equal_ok(a, b, line).is_ok(),
            (Ty::Object(a), Ty::Object(b)) if a == b => {
                if env.shapes.get(a).is_some_and(|shape| shape.data) {
                    true
                } else {
                    return keep(line, "two objects are compared: Python asks whether they are one object, and Zeno whether their fields are equal");
                }
            }
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            keep(line, format!("{} is compared with {}, which Python answers with `False` and Zeno does not allow", described(a), described(b)))
        }
    }

    fn compare(&mut self, first: &Expr, rest: &[(&'static str, Expr)], line: usize) -> Kept<Val> {
        if let [(op, other)] = rest {
            return self.compare_two(first, op, other, line);
        }
        let mut left = self.expr(first)?;
        let mut text = left.at(CMP + 1);
        for (op, other) in rest {
            if !matches!(*op, "==" | "!=" | "<" | "<=" | ">" | ">=") {
                return keep(line, format!("`{op}` in a chain of comparisons"));
            }
            let right = self.expr(other)?;
            self.ordered(op, &left.ty, &right.ty, line)?;
            text = format!("{text} {op} {}", right.at(CMP + 1));
            left = right;
        }
        Ok(Val { text, ty: Ty::Bool, prec: CMP })
    }

    fn ordered(&self, op: &str, a: &Ty, b: &Ty, line: usize) -> Kept<()> {
        if matches!(op, "==" | "!=") {
            return self.equal_ok(a, b, line);
        }
        match (a, b) {
            (Ty::Unset, _) | (_, Ty::Unset) => Ok(()),
            (a, b) if is_number(a) && is_number(b) => Ok(()),
            (Ty::Str, Ty::Str) => Ok(()),
            (Ty::Dyn, _) | (_, Ty::Dyn) => self.unknown(line, "a value that is compared"),
            _ => keep(line, format!("`{op}` between {} and {}", described(a), described(b))),
        }
    }

    fn compare_two(&mut self, first: &Expr, op: &str, other: &Expr, line: usize) -> Kept<Val> {
        let none_on_right = matches!(other.kind, E::None);
        let none_on_left = matches!(first.kind, E::None);
        if matches!(op, "is" | "is not") && !none_on_right {
            return keep(line, format!("`{op}` with something other than `None`: Zeno's `is` tests for `none` only"));
        }
        if (none_on_right || none_on_left) && matches!(op, "is" | "is not" | "==" | "!=") {
            let value = self.expr(if none_on_right { first } else { other })?;
            match &value.ty {
                Ty::Opt(_) | Ty::NoneT | Ty::Foreign | Ty::Unset => {}
                Ty::Dyn => return self.unknown(line, "the value that is tested for `None`"),
                // A type that is found only in a later round may yet turn
                // out to hold `None`.
                other if self.env.last => {
                    return keep(line, format!("{} is tested for `None`, which it never is: Zeno does not allow the test", described(other)));
                }
                _ => {}
            }
            let word = if matches!(op, "is" | "==") { "is none" } else { "is not none" };
            return Ok(Val { text: format!("{} {word}", value.at(CMP + 1)), ty: Ty::Bool, prec: CMP });
        }
        let left = self.expr(first)?;
        let right = self.expr(other)?;
        let text = format!("{} {op} {}", left.at(CMP + 1), right.at(CMP + 1));
        let done = Val { text, ty: Ty::Bool, prec: CMP };
        if matches!(op, "in" | "not in") {
            match (&left.ty, &right.ty) {
                (Ty::Unset, _) | (_, Ty::Unset) => {}
                (Ty::Str, Ty::Str) => {}
                (item, Ty::List(of)) => self.equal_ok(item, of, line)?,
                (key, Ty::Dict(of, _)) => self.equal_ok(key, of, line)?,
                (item, Ty::Foreign) if crosses(item) => self.foreign_calls += 1,
                (_, Ty::Dyn) => return self.unknown(line, &format!("what `{op}` looks in")),
                (a, b) => return keep(line, format!("`{op}` asks whether {} is in {}", described(a), described(b))),
            }
            return Ok(done);
        }
        self.ordered(op, &left.ty, &right.ty, line)?;
        Ok(done)
    }

    // ------------------------------------------------------------- Calls

    /// Record that a function, or a method, is given a value of this
    /// type for its parameter at `at`.
    fn given(&mut self, key: &str, count: usize, at: usize, ty: &Ty) {
        let before = self.env.known.functions.get(key).and_then(|sig| sig.params.get(at)).map_or(Ty::Unset, |par| par.ty.clone());
        let sig = self.found.functions.entry(key.to_string()).or_default();
        if sig.params.len() < count {
            sig.params.resize(count, Joined::default());
        }
        sig.params[at].add(ty, &before);
    }

    /// A call of a function or a method of the file that is written as
    /// Zeno. `key` is how the tables know it and `text` how the call
    /// begins; `params` are its parameters, without `self`.
    fn call_own(&mut self, key: &str, text: &str, def: &Def, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let params: &[Param] = if key.contains('.') { &def.params[1..] } else { &def.params };
        // A parameter that the function gives a new value has another
        // name in Zeno (see `function`).
        let mut changed = HashSet::new();
        bound_names(&def.body, &mut changed);
        if args.len() > params.len() {
            return keep(line, format!("`{text}` is given more values than it takes"));
        }
        let mut parts = Vec::new();
        let mut filled = vec![false; params.len()];
        for (at, arg) in args.iter().enumerate() {
            let value = self.expr(arg)?;
            self.given(key, params.len(), at, &value.ty);
            filled[at] = true;
            parts.push(value.text);
        }
        for (name, arg) in named {
            let Some(at) = params.iter().position(|param| &param.name == name) else {
                return keep(line, format!("`{text}` has no parameter `{name}`"));
            };
            if filled[at] {
                return keep(line, format!("`{name}` of `{text}` is given a value twice"));
            }
            let value = self.expr(arg)?;
            self.given(key, params.len(), at, &value.ty);
            filled[at] = true;
            let name = if changed.contains(name) { format!("{name}_given") } else { name.clone() };
            parts.push(format!("{name}={}", value.text));
        }
        if let Some(at) = (0..params.len()).find(|&at| !filled[at] && params[at].default.is_none()) {
            return keep(line, format!("`{text}` is given no value for `{}`", params[at].name));
        }
        let ret = self.env.known.functions.get(key).map_or(Ty::Unset, |sig| sig.ret.ty.clone());
        Ok(atom(format!("{text}({})", parts.join(", ")), self.settled(ret)))
    }

    /// `Meter("a", 2)`: an object of a class that is written as Zeno.
    fn construct(&mut self, class: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let env = self.env;
        let shape = &env.shapes[class];
        let takes: Vec<&ShapeField> = shape.fields.iter().filter(|field| field.given).collect();
        let mut parts: Vec<Option<String>> = vec![None; takes.len()];
        if args.len() > takes.len() {
            return keep(line, format!("`{class}` is given more values than it takes"));
        }
        let record = |writer: &mut Self, at: usize, arg: &Expr| -> Kept<String> {
            let value = writer.expr(arg)?;
            let key = (class.to_string(), takes[at].name.clone());
            let before = writer.env.known.fields.get(&key).map_or(Ty::Unset, |field| field.ty.clone());
            writer.found.fields.entry(key).or_default().add(&value.ty, &before);
            Ok(value.text)
        };
        for (at, arg) in args.iter().enumerate() {
            parts[at] = Some(record(self, at, arg)?);
        }
        for (name, arg) in named {
            let Some(at) = takes.iter().position(|field| &field.param == name) else {
                return keep(line, format!("`{class}` takes no `{name}`"));
            };
            if parts[at].is_some() {
                return keep(line, format!("`{name}` of `{class}` is given a value twice"));
            }
            parts[at] = Some(record(self, at, arg)?);
        }
        let mut texts = Vec::new();
        for (at, part) in parts.into_iter().enumerate() {
            match part {
                Some(text) => texts.push(text),
                None => return keep(line, format!("`{class}` is given no value for `{}`", takes[at].param)),
            }
        }
        Ok(atom(format!("{class}({})", texts.join(", ")), Ty::Object(class.to_string())))
    }

    /// A call that Python answers: of something kept, or of a value
    /// that is Python's own. What goes across must be the same on both
    /// sides.
    fn call_foreign(&mut self, text: String, args: &[Expr], named: &[(String, Expr)], ret: Ty, line: usize) -> Kept<Val> {
        let mut parts = Vec::new();
        let named_args = named.iter().map(|(name, arg)| (Some(name), arg));
        for (name, arg) in args.iter().map(|arg| (None, arg)).chain(named_args) {
            let value = self.expr(arg)?;
            match &value.ty {
                ty if crosses(ty) => {}
                // A list or a dict written right here is held by nothing
                // else, so a copy of it is as good as it.
                Ty::List(_) | Ty::Dict(..) if self.written_here(arg) => {}
                Ty::Dyn => return self.unknown(line, &format!("a value given to `{text}`, which is Python")),
                other => {
                    return keep(
                        line,
                        format!("{} is given to `{text}`, which is Python. Python would get a copy, and what it changed there would not be seen here", described(other)),
                    );
                }
            }
            parts.push(match name {
                Some(name) => format!("{name}={}", value.text),
                None => value.text,
            });
        }
        self.foreign_calls += 1;
        Ok(atom(format!("{text}({})", parts.join(", ")), ret))
    }

    /// Whether a list or a dict is written out on this line, of values
    /// that are the same on both sides or are written out here too.
    fn written_here(&mut self, expr: &Expr) -> bool {
        let items: Vec<&Expr> = match &expr.kind {
            E::List(items) => items.iter().collect(),
            E::Dict(pairs) => pairs.iter().map(|(_, value)| value).collect(),
            _ => return false,
        };
        items.into_iter().all(|item| crosses(&self.expr_type(item)) || self.written_here(item))
    }

    /// One value given to a call that takes exactly these.
    fn args_exactly<const N: usize>(&mut self, what: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<[Val; N]> {
        if !named.is_empty() || args.len() != N {
            return keep(line, format!("`{what}` with these arguments has no match in Zeno yet"));
        }
        let mut values = Vec::new();
        for arg in args {
            values.push(self.expr(arg)?);
        }
        values.try_into().map_err(|_| Keep { line, why: String::new() })
    }

    fn call_module(&mut self, module: Module, name: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        match module {
            Module::Math => {
                let gives = match name {
                    "sqrt" | "sin" | "cos" | "tan" | "exp" | "log" | "log2" | "log10" => Ty::Float,
                    "floor" | "ceil" => Ty::Int,
                    _ => return keep(line, format!("`math.{name}` has no match in Zeno's `math` yet")),
                };
                let [value] = self.args_exactly::<1>(&format!("math.{name}"), args, named, line)?;
                match &value.ty {
                    Ty::Int | Ty::Float | Ty::Unset => {}
                    Ty::Dyn => return self.unknown(line, &format!("what `math.{name}` is given")),
                    other => return keep(line, format!("`math.{name}` of {}", described(other))),
                }
                self.need_math = true;
                Ok(atom(format!("math.{name}({})", value.text), gives))
            }
            Module::Sys => {
                if name != "exit" || !named.is_empty() || args.len() > 1 {
                    return keep(line, format!("`sys.{name}` has no match in Zeno yet"));
                }
                let code = match args.first() {
                    None => "0".to_string(),
                    Some(arg) => {
                        let value = self.expr(arg)?;
                        match &value.ty {
                            Ty::Int | Ty::Unset => value.text,
                            _ => return keep(line, "`sys.exit` with something other than a whole number"),
                        }
                    }
                };
                self.need_program = true;
                Ok(atom(format!("program.exit({code})"), Ty::NoneT))
            }
            Module::Quiet => keep(line, format!("`{name}`, which comes from a module that only says types")),
        }
    }

    fn is_argv(&self, expr: &Expr) -> bool {
        match &expr.kind {
            E::Attr(of, name) if name == "argv" => matches!(&of.kind, E::Name(alias) if !self.is_var(alias) && self.env.modules.get(alias) == Some(&Module::Sys)),
            E::Name(name) => !self.is_var(name) && matches!(self.env.from_names.get(name), Some((Module::Sys, real)) if real == "argv"),
            _ => false,
        }
    }

    fn call_name(&mut self, name: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        if self.is_var(name) {
            return keep(line, format!("`{name}` holds a function and is called, which is not translated yet"));
        }
        let env = self.env;
        if let Some(def) = env.defs.get(name) {
            return self.call_own(name, name, def, args, named, line);
        }
        if env.shapes.contains_key(name) {
            return self.construct(name, args, named, line);
        }
        if let Some(ret) = env.kept.get(name) {
            return self.call_foreign(format!("{}.{name}", env.block), args, named, ret.clone(), line);
        }
        if let Some((module, real)) = env.from_names.get(name) {
            return self.call_module(*module, real, args, named, line);
        }
        self.builtin(name, args, named, line)
    }

    /// The functions that Python has without an `import`.
    fn builtin(&mut self, name: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        match name {
            "print" => {
                if !named.is_empty() {
                    return keep(line, "`print` with `sep=`, `end=` or `file=`");
                }
                let mut parts = Vec::new();
                for arg in args {
                    let value = self.expr(arg)?;
                    self.shown(&value.ty, line)?;
                    parts.push(value.text);
                }
                Ok(atom(format!("print({})", parts.join(", ")), Ty::NoneT))
            }
            "len" => {
                if let [arg] = args {
                    if self.is_argv(arg) {
                        self.need_program = true;
                        return Ok(Val { text: "len(program.arguments()) + 1".to_string(), ty: Ty::Int, prec: SUM });
                    }
                }
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                match &value.ty {
                    Ty::Str | Ty::List(_) | Ty::Dict(..) | Ty::Unset => {}
                    Ty::Foreign => self.foreign_calls += 1,
                    Ty::Dyn => return self.unknown(line, "what `len` is asked of"),
                    other => return keep(line, format!("`len` of {}", described(other))),
                }
                Ok(atom(format!("len({})", value.text), Ty::Int))
            }
            "int" | "float" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                let gives = if name == "int" { Ty::Int } else { Ty::Float };
                match &value.ty {
                    Ty::Str | Ty::Int | Ty::Float | Ty::Unset => {}
                    Ty::Dyn => return self.unknown(line, &format!("what `{name}` is given")),
                    other => return keep(line, format!("`{name}` of {}", described(other))),
                }
                Ok(atom(format!("{name}({})", value.text), gives))
            }
            "str" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                self.shown(&value.ty, line)?;
                Ok(atom(format!("str({})", value.text), Ty::Str))
            }
            "bool" => {
                if !named.is_empty() || args.len() != 1 {
                    return keep(line, "`bool` with these arguments");
                }
                self.truth(&args[0])
            }
            "abs" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                match &value.ty {
                    Ty::Int | Ty::Float | Ty::Unset => Ok(atom(format!("abs({})", value.text), value.ty.clone())),
                    Ty::Dyn => self.unknown(line, "what `abs` is given"),
                    other => keep(line, format!("`abs` of {}", described(other))),
                }
            }
            "min" | "max" | "sum" => {
                if !named.is_empty() || args.is_empty() || (name == "sum" && args.len() != 1) {
                    return keep(line, format!("`{name}` with these arguments has no match in Zeno yet"));
                }
                let mut parts = Vec::new();
                let mut ty = Ty::Unset;
                for arg in args {
                    let value = self.expr(arg)?;
                    ty = join(&ty, &value.ty);
                    parts.push(value.text);
                }
                let gives = match (&ty, args.len()) {
                    (Ty::Unset, _) => Ty::Unset,
                    (Ty::List(item), 1) if matches!(**item, Ty::Int | Ty::Float | Ty::Unset) => (**item).clone(),
                    (Ty::Int | Ty::Float, count) if count > 1 => ty.clone(),
                    (Ty::Dyn, _) => return self.unknown(line, &format!("what `{name}` is given")),
                    (other, _) => return keep(line, format!("`{name}` of {}", described(other))),
                };
                Ok(atom(format!("{name}({})", parts.join(", ")), gives))
            }
            "round" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                match &value.ty {
                    Ty::Float | Ty::Unset => {}
                    Ty::Int => return Ok(value),
                    Ty::Dyn => return self.unknown(line, "what `round` is given"),
                    other => return keep(line, format!("`round` of {}", described(other))),
                }
                self.need_math = true;
                self.note(Note::Half, line);
                Ok(atom(format!("math.round({})", value.text), Ty::Int))
            }
            "chr" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                match &value.ty {
                    Ty::Int | Ty::Unset => Ok(atom(format!("char({})", value.text), Ty::Str)),
                    other => keep(line, format!("`chr` of {}", described(other))),
                }
            }
            "ord" => {
                let [value] = self.args_exactly::<1>(name, args, named, line)?;
                match &value.ty {
                    Ty::Str | Ty::Unset => Ok(atom(format!("{}.code()", value.at(ATOM)), Ty::Int)),
                    other => keep(line, format!("`ord` of {}", described(other))),
                }
            }
            "list" => {
                let [arg] = args else {
                    return keep(line, "`list` with these arguments has no match in Zeno yet");
                };
                if !named.is_empty() {
                    return keep(line, "`list` with these arguments has no match in Zeno yet");
                }
                // `list(d.keys())` and `list(d.values())` are a list of
                // Zeno as they stand.
                if let E::Call { function, args: none, named: no_names } = &arg.kind {
                    if let E::Attr(of, method) = &function.kind {
                        if matches!(method.as_str(), "keys" | "values") && none.is_empty() && no_names.is_empty() {
                            let dict = self.expr(of)?;
                            if let Ty::Dict(key, value) = &dict.ty {
                                let item = if method == "keys" { key.clone() } else { value.clone() };
                                return Ok(atom(format!("{}.{method}()", dict.at(ATOM)), Ty::List(item)));
                            }
                        }
                    }
                }
                let value = self.expr(arg)?;
                match &value.ty {
                    Ty::Unset => Ok(later()),
                    Ty::List(_) => Ok(atom(format!("{}.slice(0)", value.at(ATOM)), value.ty.clone())),
                    Ty::Dict(key, _) => Ok(atom(format!("{}.keys()", value.at(ATOM)), Ty::List(key.clone()))),
                    Ty::Dyn => self.unknown(line, "what `list` is given"),
                    other => keep(line, format!("`list` of {}", described(other))),
                }
            }
            "input" => keep(line, "`input`: at the end of what is typed Python raises an error, and Zeno gives `none`"),
            "sorted" => keep(line, "`sorted` in a place other than `name = sorted(a_list)`"),
            "range" | "enumerate" | "zip" | "reversed" => keep(line, format!("`{name}` in a place other than the `for` it counts for")),
            "open" => keep(line, "`open`: files are not translated yet"),
            "isinstance" | "type" => keep(line, format!("`{name}` asks for the type of a value while the program runs")),
            _ => keep(line, format!("`{name}` is not a function the converter knows")),
        }
    }

    fn call_method(&mut self, of: &Expr, method: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let env = self.env;
        if let E::Name(alias) = &of.kind {
            if !self.is_var(alias) {
                if let Some(module) = env.modules.get(alias) {
                    return self.call_module(*module, method, args, named, line);
                }
            }
        }
        let on = self.expr(of)?;
        match on.ty.clone() {
            Ty::Unset => {
                for arg in args.iter().chain(named.iter().map(|(_, arg)| arg)) {
                    self.expr(arg)?;
                }
                Ok(later())
            }
            Ty::Foreign => self.call_foreign(format!("{}.{method}", on.at(ATOM)), args, named, Ty::Foreign, line),
            Ty::Str => self.text_method(&on, method, args, named, line),
            Ty::List(item) => self.list_method(of, &on, &item, method, args, named, line),
            Ty::Dict(key, value) => self.dict_method(&on, &key, &value, method, args, named, line),
            Ty::Object(class) => {
                let Some(def) = env.shapes.get(&class).and_then(|shape| shape.methods.get(method)) else {
                    return keep(line, format!("`{class}` has no method `{method}` that the converter knows"));
                };
                self.call_own(&format!("{class}.{method}"), &format!("{}.{method}", on.at(ATOM)), def, args, named, line)
            }
            Ty::Dyn => self.unknown(line, &format!("the value whose `.{method}()` is called")),
            other => keep(line, format!("`.{method}()` of {}", described(&other))),
        }
    }

    fn text_method(&mut self, on: &Val, method: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let what = format!("{method} of a text");
        let on_text = on.at(ATOM);
        let is_text = |value: &Val| matches!(value.ty, Ty::Str | Ty::Unset);
        match (method, args.len()) {
            ("upper" | "lower", 0) => {
                self.args_exactly::<0>(&what, args, named, line)?;
                self.note(Note::Letters, line);
                Ok(atom(format!("{on_text}.{method}()"), Ty::Str))
            }
            ("strip", 0) => Ok(atom(format!("{on_text}.strip()"), Ty::Str)),
            ("splitlines", 0) => Ok(atom(format!("{on_text}.lines()"), Ty::List(Box::new(Ty::Str)))),
            ("split", 0) if named.is_empty() => Ok(atom(format!("{on_text}.split()"), Ty::List(Box::new(Ty::Str)))),
            ("split", 1) => {
                let [by] = self.args_exactly::<1>(&what, args, named, line)?;
                if !is_text(&by) {
                    return keep(line, "`split` by something other than a text");
                }
                Ok(atom(format!("{on_text}.split({})", by.text), Ty::List(Box::new(Ty::Str))))
            }
            ("replace", 2) => {
                let [old, new] = self.args_exactly::<2>(&what, args, named, line)?;
                if !is_text(&old) || !is_text(&new) {
                    return keep(line, "`replace` with something other than texts");
                }
                Ok(atom(format!("{on_text}.replace({}, {})", old.text, new.text), Ty::Str))
            }
            ("startswith" | "endswith", 1) => {
                let [with] = self.args_exactly::<1>(&what, args, named, line)?;
                if !is_text(&with) {
                    return keep(line, format!("`{method}` with something other than one text"));
                }
                let zeno = if method == "startswith" { "starts_with" } else { "ends_with" };
                Ok(atom(format!("{on_text}.{zeno}({})", with.text), Ty::Bool))
            }
            ("join", 1) => {
                let [items] = self.args_exactly::<1>(&what, args, named, line)?;
                match &items.ty {
                    Ty::Unset => Ok(later()),
                    Ty::List(item) if matches!(**item, Ty::Str | Ty::Unset) => Ok(atom(format!("{}.join({})", items.at(ATOM), on.text), Ty::Str)),
                    Ty::Dyn => self.unknown(line, "what `join` is given"),
                    other => keep(line, format!("`join` of {}: Zeno joins a list of texts", described(other))),
                }
            }
            ("find" | "index" | "rfind" | "rindex", _) => keep(line, format!("`{method}` of a text: where Python gives -1 or raises an error, Zeno gives `none`")),
            _ => keep(line, format!("`.{method}()` of a text with these arguments has no match in Zeno yet")),
        }
    }

    /// A list or a dict at `place` is given an item of this type.
    pub(super) fn widen(&mut self, place: &Expr, ty: Ty) {
        match &place.kind {
            E::Name(name) if self.is_var(name) && !self.temps.iter().any(|(temp, _)| temp == name) => self.bind(name, &ty),
            E::Attr(of, field) => {
                if let Ok(Val { ty: Ty::Object(class), .. }) = self.quietly(of) {
                    let key = (class, field.clone());
                    let before = self.env.known.fields.get(&key).map_or(Ty::Unset, |field| field.ty.clone());
                    self.found.fields.entry(key).or_default().add(&ty, &before);
                }
            }
            _ => {}
        }
    }

    /// The value of an expression that was written already, asked for
    /// again for its type alone: nothing it notes is kept twice.
    fn quietly(&mut self, expr: &Expr) -> Kept<Val> {
        let notes = self.notes.len();
        let calls = self.foreign_calls;
        let value = self.expr(expr);
        self.notes.truncate(notes);
        self.foreign_calls = calls;
        value
    }

    #[allow(clippy::too_many_arguments)]
    fn list_method(&mut self, place: &Expr, on: &Val, item: &Ty, method: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let what = format!("{method} of a list");
        let on_text = on.at(ATOM);
        let whole = |value: &Val| matches!(value.ty, Ty::Int | Ty::Unset);
        match (method, args.len()) {
            ("append", 1) => {
                let [value] = self.args_exactly::<1>(&what, args, named, line)?;
                self.widen(place, Ty::List(Box::new(value.ty.clone())));
                Ok(atom(format!("{on_text}.append({})", value.text), Ty::NoneT))
            }
            ("insert", 2) => {
                let [at, value] = self.args_exactly::<2>(&what, args, named, line)?;
                if !whole(&at) {
                    return keep(line, "`insert` at something other than a whole number");
                }
                self.widen(place, Ty::List(Box::new(value.ty.clone())));
                Ok(atom(format!("{on_text}.insert({}, {})", at.text, value.text), Ty::NoneT))
            }
            ("pop", 0) if named.is_empty() => Ok(atom(format!("{on_text}.pop()"), item.clone())),
            ("pop", 1) => {
                let [at] = self.args_exactly::<1>(&what, args, named, line)?;
                if !whole(&at) || at.text.starts_with('-') {
                    return keep(line, "`pop` at something other than a whole number that counts from the front");
                }
                Ok(atom(format!("{on_text}.pop({})", at.text), item.clone()))
            }
            ("clear" | "reverse", 0) if named.is_empty() => Ok(atom(format!("{on_text}.{method}()"), Ty::NoneT)),
            ("sort", 0) if named.is_empty() => match item {
                Ty::Int | Ty::Float | Ty::Str | Ty::Unset => Ok(atom(format!("{on_text}.sort()"), Ty::NoneT)),
                Ty::Dyn => self.unknown(line, "the items that are sorted"),
                other => keep(line, format!("`sort` of a list of {}", described(other))),
            },
            ("copy", 0) if named.is_empty() => Ok(atom(format!("{on_text}.slice(0)"), on.ty.clone())),
            ("sort", _) => keep(line, "`sort` with `key=` or `reverse=`"),
            _ => keep(line, format!("`.{method}()` of a list with these arguments has no match in Zeno yet")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dict_method(&mut self, on: &Val, key: &Ty, value: &Ty, method: &str, args: &[Expr], named: &[(String, Expr)], line: usize) -> Kept<Val> {
        let what = format!("{method} of a dict");
        let on_text = on.at(ATOM);
        match (method, args.len()) {
            ("get", 1) => {
                let [wanted] = self.args_exactly::<1>(&what, args, named, line)?;
                self.equal_ok(&wanted.ty, key, line)?;
                let gives = match value {
                    Ty::Opt(_) | Ty::Unset => value.clone(),
                    other => Ty::Opt(Box::new(other.clone())),
                };
                Ok(atom(format!("{on_text}.get({})", wanted.text), gives))
            }
            ("get", 2) => {
                let [wanted, otherwise] = self.args_exactly::<2>(&what, args, named, line)?;
                self.equal_ok(&wanted.ty, key, line)?;
                Ok(atom(format!("{on_text}.get({}, {})", wanted.text, otherwise.text), join(value, &otherwise.ty)))
            }
            ("pop", 1) => {
                let [wanted] = self.args_exactly::<1>(&what, args, named, line)?;
                self.equal_ok(&wanted.ty, key, line)?;
                Ok(atom(format!("{on_text}.pop({})", wanted.text), value.clone()))
            }
            ("keys" | "values" | "items", 0) => keep(line, format!("`.{method}()` of a dict in a place other than a `for` or `list( )`: Python gives a view, which follows the dict")),
            _ => keep(line, format!("`.{method}()` of a dict with these arguments has no match in Zeno yet")),
        }
    }

    // ------------------------------------------------- Fields and items

    fn attr(&mut self, of: &Expr, name: &str, line: usize) -> Kept<Val> {
        let env = self.env;
        if let E::Name(alias) = &of.kind {
            if !self.is_var(alias) {
                match env.modules.get(alias) {
                    Some(Module::Math) if matches!(name, "pi" | "e") => {
                        self.need_math = true;
                        return Ok(atom(format!("math.{name}"), Ty::Float));
                    }
                    Some(_) => return keep(line, format!("`{alias}.{name}` has no match in Zeno yet")),
                    None => {}
                }
            }
        }
        let on = self.expr(of)?;
        match &on.ty {
            Ty::Unset => Ok(later()),
            Ty::Foreign => {
                self.foreign_calls += 1;
                Ok(atom(format!("{}.{name}", on.at(ATOM)), Ty::Foreign))
            }
            Ty::Object(class) => {
                let Some(shape) = env.shapes.get(class) else {
                    return keep(line, format!("`.{name}` of an object of `{class}`"));
                };
                if !shape.fields.iter().any(|field| field.name == name) {
                    let why = if shape.methods.contains_key(name) { "is a method, taken as a value" } else { "is not a field that `__init__` gives a value" };
                    return keep(line, format!("`{name}` of `{class}` {why}"));
                }
                self.zeno_name(name, line)?;
                let ty = env.known.fields.get(&(class.clone(), name.to_string())).map_or(Ty::Unset, |field| field.ty.clone());
                let text = format!("{}.{name}", on.at(ATOM));
                // A field that a test above has shown not to be `none`.
                let ty = match self.settled(ty) {
                    Ty::Opt(inner) if self.narrow.iter().any(|narrowed| *narrowed == text) => *inner,
                    ty => ty,
                };
                Ok(atom(text, ty))
            }
            Ty::Dyn => self.unknown(line, &format!("the value whose `.{name}` is taken")),
            other => keep(line, format!("`.{name}` of {}", described(other))),
        }
    }

    /// A place in a list or a text, counted from the front in Zeno:
    /// `-1` is written as the length less one, which needs the list to
    /// be a name, so that asking for it twice asks nothing new.
    fn place_in(&mut self, of: &Expr, on: &Val, at: &Expr, line: usize) -> Kept<String> {
        if let Some(back) = from_the_end(at) {
            if !is_plain_place(of) {
                return keep(line, "an item counted from the end of something that is not a name");
            }
            return Ok(format!("len({}) - {back}", on.text));
        }
        let at = self.expr(at)?;
        match &at.ty {
            Ty::Int | Ty::Unset => Ok(at.text),
            Ty::Dyn => self.unknown(line, "the place of an item"),
            other => keep(line, format!("an item at {}", described(other))),
        }
    }

    pub(super) fn index(&mut self, of: &Expr, at: &Expr, line: usize) -> Kept<Val> {
        if self.is_argv(of) {
            if let E::Number(text) = &at.kind {
                if let Ok(place @ 1..) = text.parse::<usize>() {
                    self.need_program = true;
                    return Ok(atom(format!("program.arguments()[{}]", place - 1), Ty::Str));
                }
            }
            return keep(line, "`sys.argv` in a way other than `sys.argv[1:]` or `sys.argv[1]`");
        }
        let on = self.expr(of)?;
        match on.ty.clone() {
            Ty::Unset => {
                self.expr(at)?;
                Ok(later())
            }
            Ty::List(item) => {
                let place = self.place_in(of, &on, at, line)?;
                Ok(atom(format!("{}[{place}]", on.at(ATOM)), *item))
            }
            Ty::Str => {
                let place = self.place_in(of, &on, at, line)?;
                Ok(atom(format!("{}[{place}]", on.at(ATOM)), Ty::Str))
            }
            Ty::Dict(key, value) => {
                let wanted = self.expr(at)?;
                self.equal_ok(&wanted.ty, &key, line)?;
                Ok(atom(format!("{}[{}]", on.at(ATOM), wanted.text), *value))
            }
            Ty::Foreign => {
                let wanted = self.expr(at)?;
                if !crosses(&wanted.ty) {
                    return keep(line, format!("an item of a value of Python at {}", described(&wanted.ty)));
                }
                self.foreign_calls += 1;
                Ok(atom(format!("{}[{}]", on.at(ATOM), wanted.text), Ty::Foreign))
            }
            Ty::Dyn => self.unknown(line, "the value an item is taken of"),
            other => keep(line, format!("an item of {}", described(&other))),
        }
    }

    fn slice(&mut self, of: &Expr, from: Option<&Expr>, to: Option<&Expr>, line: usize) -> Kept<Val> {
        if self.is_argv(of) {
            if let (Some(Expr { kind: E::Number(text), .. }), None) = (from, to) {
                if text == "1" {
                    self.need_program = true;
                    return Ok(atom("program.arguments()", Ty::List(Box::new(Ty::Str))));
                }
            }
            return keep(line, "`sys.argv` in a way other than `sys.argv[1:]` or `sys.argv[1]`");
        }
        let on = self.expr(of)?;
        match &on.ty {
            Ty::List(_) | Ty::Str => {}
            Ty::Unset => {
                for part in from.iter().chain(to.iter()) {
                    self.expr(part)?;
                }
                return Ok(later());
            }
            Ty::Dyn => return self.unknown(line, "the value a part is taken of"),
            other => return keep(line, format!("a part (`[a:b]`) of {}", described(other))),
        }
        // Python cuts a part off at the ends of the list: `xs[:10]` of
        // three items is the three. Zeno raises an error. So each bound
        // is held to the length, which asks for the length, which needs
        // the list to be a name.
        if (from.is_some() || to.is_some()) && !is_plain_place(of) {
            return keep(line, "a part (`[a:b]`) of something that is not a name: Python cuts it off at the ends, and to do the same Zeno must ask its length");
        }
        let length = format!("len({})", on.text);
        let bound = |writer: &mut Self, at: &Expr| -> Kept<String> {
            if let Some(back) = from_the_end(at) {
                return Ok(format!("max({length} - {back}, 0)"));
            }
            let value = writer.expr(at)?;
            match &value.ty {
                Ty::Int | Ty::Unset if value.text == "0" => Ok("0".to_string()),
                Ty::Int | Ty::Unset => Ok(format!("min({}, {length})", value.text)),
                Ty::Dyn => writer.unknown(line, "the end of a part"),
                other => keep(line, format!("a part that ends at {}", described(other))),
            }
        };
        let first = match from {
            Some(from) => bound(self, from)?,
            None => "0".to_string(),
        };
        let text = match to {
            Some(to) => format!("{}.slice({first}, {})", on.at(ATOM), bound(self, to)?),
            None => format!("{}.slice({first})", on.at(ATOM)),
        };
        Ok(atom(text, on.ty.clone()))
    }

    /// What a `for`, or a list made from a loop, goes over: the words,
    /// and the type of each item. `range` is here, since this is the
    /// one place it can be.
    pub(super) fn over(&mut self, items: &Expr, line: usize) -> Kept<(String, Ty)> {
        if let E::Call { function, args, named } = &items.kind {
            if matches!(&function.kind, E::Name(name) if name == "range" && !self.is_var(name)) {
                if !named.is_empty() || args.is_empty() || args.len() > 3 {
                    return keep(line, "`range` with these arguments");
                }
                let mut parts = Vec::new();
                for arg in args {
                    let value = self.expr(arg)?;
                    match &value.ty {
                        Ty::Int | Ty::Unset => parts.push(value.text),
                        Ty::Dyn => return self.unknown(line, "what `range` is given"),
                        other => return keep(line, format!("`range` of {}", described(other))),
                    }
                }
                return Ok((format!("range({})", parts.join(", ")), Ty::Int));
            }
        }
        // `for n in (1, 2, 3)`: a tuple written out here is gone over as
        // a list is.
        if let E::Tuple(values) = &items.kind {
            let mut texts = Vec::new();
            let mut ty = Ty::Unset;
            for value in values {
                let value = self.expr(value)?;
                ty = join(&ty, &value.ty);
                texts.push(value.text);
            }
            return Ok((format!("[{}]", texts.join(", ")), ty));
        }
        let value = self.expr(items)?;
        let item = match &value.ty {
            Ty::Unset => Ty::Unset,
            Ty::List(item) => (**item).clone(),
            Ty::Str => Ty::Str,
            Ty::Dict(key, _) => (**key).clone(),
            Ty::Foreign => Ty::Foreign,
            Ty::Dyn => return self.unknown(line, "what the loop goes over"),
            other => return keep(line, format!("a loop over {}", described(other))),
        };
        Ok((value.text, item))
    }

    fn list_of(&mut self, item: &Expr, name: &str, items: &Expr, condition: Option<&Expr>, line: usize) -> Kept<Val> {
        self.zeno_name(name, line)?;
        let (over, each) = self.over(items, line)?;
        if each == Ty::Foreign {
            return keep(line, "a list made from a loop over a value of Python");
        }
        let mut parts = vec![item];
        parts.extend(condition);
        self.unchanged_by_calls(items, &parts, line)?;
        self.temps.push((name.to_string(), each));
        let made = self.expr(item).and_then(|item| match condition {
            Some(condition) => self.truth(condition).map(|test| (item, format!(" if {}", test.text))),
            None => Ok((item, String::new())),
        });
        self.temps.pop();
        let (item, test) = made?;
        Ok(atom(format!("[{} for {name} in {over}{test}]", item.text), Ty::List(Box::new(item.ty))))
    }
}
