//! `zeno convert program.py`: how a statement, a function and a class
//! of Python are written in Zeno (docs/convert.md, decisions chapter 62).

use super::*;

/// A literal that is the same value every time it is written: a
/// number, a text, `True`, `False`, `None`.
pub(super) fn is_fixed(expr: &Expr) -> bool {
    match &expr.kind {
        E::Number(_) | E::True | E::False | E::None => true,
        E::Str(lit) => !lit.prefix.contains('f'),
        E::Unary("-" | "+", of) => matches!(of.kind, E::Number(_)),
        _ => false,
    }
}

fn mentions(expr: &Expr, name: &str) -> bool {
    let mut found = false;
    each_expr(expr, &mut |part| found |= matches!(&part.kind, E::Name(other) if other == name));
    found
}

/// `d.items()`, `enumerate(xs)` and their like: the method or function,
/// and what it is of.
fn pairs_of(expr: &Expr) -> Option<(&str, &Expr)> {
    let E::Call { function, args, named } = &expr.kind else { return None };
    if !named.is_empty() {
        return None;
    }
    match (&function.kind, args.as_slice()) {
        (E::Attr(of, method), []) if matches!(method.as_str(), "items" | "keys" | "values") => Some((method, of)),
        (E::Name(name), [of]) if name == "enumerate" => Some(("enumerate", of)),
        _ => None,
    }
}

/// `[("ann", 2019), ("bob", 2021)]`: rows of fixed values written out,
/// each with this many.
fn fixed_rows(items: &Expr, width: usize) -> bool {
    let E::List(rows) = &items.kind else { return false };
    !rows.is_empty() && rows.iter().all(|row| matches!(&row.kind, E::Tuple(parts) if parts.len() == width && parts.iter().all(is_fixed)))
}

fn has_inner_function(stmts: &[Stmt]) -> Option<usize> {
    stmts.iter().find_map(|stmt| match &stmt.kind {
        S::Def(_) => Some(stmt.line),
        S::If { branches, otherwise } => {
            branches.iter().find_map(|(_, body, _)| has_inner_function(body)).or_else(|| otherwise.as_ref().and_then(|body| has_inner_function(body)))
        }
        S::While { body, .. } | S::For { body, .. } => has_inner_function(body),
        S::Try { body, handlers } => has_inner_function(body).or_else(|| handlers.iter().find_map(|handler| has_inner_function(&handler.body))),
        _ => None,
    })
}

fn returns_a_value(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        S::Return(value) => value.is_some(),
        S::If { branches, otherwise } => branches.iter().any(|(_, body, _)| returns_a_value(body)) || otherwise.as_ref().is_some_and(|body| returns_a_value(body)),
        S::While { body, .. } | S::For { body, .. } => returns_a_value(body),
        S::Try { body, handlers } => returns_a_value(body) || handlers.iter().any(|handler| returns_a_value(&handler.body)),
        _ => false,
    })
}

impl Writer<'_> {
    pub(super) fn put(&mut self, text: impl Into<String>) {
        self.lines.push((self.indent, text.into()));
    }

    /// A variable is given a value of this type.
    pub(super) fn bind(&mut self, name: &str, ty: &Ty) {
        let scope = if self.is_local(name) { self.scope.clone() } else { String::new() };
        let into = self.found.vars.entry((scope, name.to_string())).or_insert(Ty::Unset);
        *into = join(into, ty);
    }

    fn plan_now(&self) -> &Plan {
        if self.in_function {
            &self.plan
        } else {
            self.env.module_plan
        }
    }

    /// The comments on the lines before this one, each on its own line.
    pub(super) fn comments_before(&mut self, line: usize) {
        let env = self.env;
        while let Some(comment) = env.comments.get(self.comment_at) {
            if comment.line >= line {
                break;
            }
            self.put(format!("#{}", comment.text));
            self.comment_at += 1;
        }
    }

    /// The comment at the end of this line of Python, put at the end of
    /// the line of Zeno that was written for it.
    fn comment_beside(&mut self, line: usize, at: usize) {
        let env = self.env;
        let Some(comment) = env.comments.get(self.comment_at) else { return };
        if comment.line != line || comment.alone {
            return;
        }
        match self.lines.get_mut(at) {
            Some((_, text)) => text.push_str(&format!("  #{}", comment.text)),
            None => self.put(format!("#{}", comment.text)),
        }
        self.comment_at += 1;
    }

    /// The statements of a block, one after another. In a round that is
    /// not the last, a statement that cannot be written is passed over,
    /// so that what the rest tells of the types is still learnt.
    pub(super) fn block(&mut self, stmts: &[Stmt]) -> Kept<()> {
        let mark = self.narrow.len();
        for stmt in stmts {
            let state = (self.lines.len(), self.indent, self.temps.len(), self.caught.len());
            if let Err(keep) = self.statement(stmt) {
                if self.env.last {
                    return Err(keep);
                }
                self.lines.truncate(state.0);
                self.indent = state.1;
                self.temps.truncate(state.2);
                self.caught.truncate(state.3);
                self.quote = '"';
            }
        }
        self.narrow.truncate(mark);
        Ok(())
    }

    fn inner(&mut self, stmts: &[Stmt]) -> Kept<()> {
        self.indent += 1;
        let done = self.block(stmts);
        self.indent -= 1;
        done
    }

    fn statement(&mut self, stmt: &Stmt) -> Kept<()> {
        self.comments_before(stmt.line);
        let at = self.lines.len();
        let notes = self.notes.len();
        let hoisted = self.plan_now().hoist.get(&address(stmt)).cloned().unwrap_or_default();
        for name in hoisted {
            self.zeno_name(&name, stmt.line)?;
            let ty = self.whole_ty(&name);
            match (first_value(&ty), zeno_type(&ty)) {
                (Some(first), Some(written)) => self.put(format!("var {name}: {written} = {first}")),
                _ if !self.env.last => {}
                _ => {
                    return keep(
                        stmt.line,
                        format!("`{name}` gets its first value inside a block and is used outside it, and Zeno has no first value for {}", described(&ty)),
                    )
                }
            }
        }
        let first = self.lines.len();
        self.statement_itself(stmt)?;
        self.comment_beside(stmt.line, first);
        // The first place a difference shows is where the file says so.
        let mut said = Vec::new();
        for (note, _) in &self.notes[notes..] {
            if *note != Note::FloatStart && self.noted.insert(*note) {
                said.extend(note.lines(""));
            }
        }
        for name in std::mem::take(&mut self.floats) {
            said.extend(Note::FloatStart.lines(&name));
        }
        let indent = self.lines.get(at).map_or(self.indent, |(indent, _)| *indent);
        for (offset, line) in said.into_iter().enumerate() {
            self.lines.insert(at + offset, (indent, format!("# {line}")));
        }
        Ok(())
    }

    fn statement_itself(&mut self, stmt: &Stmt) -> Kept<()> {
        let line = stmt.line;
        match &stmt.kind {
            S::Expr(expr) => match &expr.kind {
                // A text on a line of its own does nothing: it is there
                // to be read.
                E::Str(lit) if !lit.prefix.contains('f') => {
                    for words in lit.body.lines() {
                        let words = words.trim();
                        self.put(if words.is_empty() { "#".to_string() } else { format!("# {words}") });
                    }
                }
                E::Call { .. } => {
                    let value = self.expr(expr)?;
                    self.put(value.text);
                }
                _ => return keep(line, "a line that names a value and does nothing with it"),
            },
            S::Assign { target, value } => self.assign(stmt, target, value)?,
            S::Annotated { target, annotation, value } => {
                let Some(value) = value else {
                    return keep(line, "a name is given a type and no value");
                };
                if let E::Name(name) = &target.kind {
                    let written = self.annotated(annotation);
                    self.bind(name, &written);
                }
                self.assign(stmt, target, value)?;
            }
            S::Augmented { target, op, value } => self.augmented(target, op, value, line)?,
            S::If { branches, otherwise } => {
                for (at, (condition, body, branch_line)) in branches.iter().enumerate() {
                    if at > 0 {
                        self.comments_before(*branch_line);
                    }
                    let test = self.truth(condition)?;
                    self.put(if at == 0 { format!("if {} {{", test.text) } else { format!("}} else if {} {{", test.text) });
                    let mark = self.narrow.len();
                    if let Some(name) = none_test(condition, true) {
                        self.narrow.push(name.to_string());
                    }
                    let done = self.inner(body);
                    self.narrow.truncate(mark);
                    done?;
                }
                if let Some(body) = otherwise {
                    self.put("} else {");
                    self.inner(body)?;
                }
                self.put("}");
                // `if x is None: return`: below it, `x` is not none.
                if let ([(condition, body, _)], None) = (branches.as_slice(), otherwise) {
                    if let Some(name) = none_test(condition, false) {
                        if always_leaves(body) {
                            self.narrow.push(name.to_string());
                        }
                    }
                }
            }
            S::While { condition, body } => {
                let test = self.truth(condition)?;
                self.put(format!("while {} {{", test.text));
                let mark = self.narrow.len();
                if let Some(name) = none_test(condition, true) {
                    self.narrow.push(name.to_string());
                }
                let done = self.inner(body);
                self.narrow.truncate(mark);
                done?;
                self.put("}");
            }
            S::For { names, items, body } => self.for_loop(stmt, names, items, body, line)?,
            S::Def(def) => return keep(line, format!("the function `{}` is written inside a function or a block, which is not translated yet", def.name)),
            S::Class { name, .. } => return keep(line, format!("the class `{name}` is written inside a function or a block")),
            S::Return(value) => self.returns(value.as_ref(), line)?,
            S::Raise(error) => self.raises(error.as_ref(), line)?,
            S::Try { body, handlers } => {
                self.put("try {");
                self.inner(body)?;
                for handler in handlers {
                    self.comments_before(handler.line);
                    let Some(kind) = &handler.kind else {
                        return keep(handler.line, "an `except:` that names no kind of error: it catches the end of the program too");
                    };
                    let Some(zeno) = error_kind(kind) else {
                        return keep(handler.line, format!("`except {kind}`: Zeno has no error of that kind"));
                    };
                    if matches!(zeno, "Error" | "IndexError" | "OverflowError" | "ValueError") {
                        self.note(Note::Catches, handler.line);
                    }
                    match &handler.name {
                        Some(name) => {
                            self.zeno_name(name, handler.line)?;
                            self.bind(name, &Ty::Error);
                            self.put(format!("}} except {zeno} as {name} {{"));
                        }
                        None => self.put(format!("}} except {zeno} {{")),
                    }
                    self.caught.push(handler.name.clone());
                    let done = self.inner(&handler.body);
                    self.caught.pop();
                    done?;
                }
                self.put("}");
            }
            S::Delete(target) => {
                let E::Index(of, key) = &target.kind else {
                    return keep(line, "a `del` of something other than an item of a dict");
                };
                let on = self.expr(of)?;
                let key = self.expr(key)?;
                match &on.ty {
                    Ty::Dict(..) => self.put(format!("{}.pop({})", on.at(ATOM), key.text)),
                    Ty::Unset => {}
                    other => return keep(line, format!("a `del` of an item of {}", described(other))),
                }
            }
            S::Import { .. } | S::FromImport { .. } => return keep(line, "an `import` inside a function or a block"),
            S::Global(_) | S::Pass => {}
            S::Break => self.put("break"),
            S::Continue => self.put("continue"),
        }
        Ok(())
    }

    /// The type that an annotation of Python says.
    pub(super) fn annotated(&self, expr: &Expr) -> Ty {
        let env = self.env;
        let item = |expr: &Expr| Box::new(self.annotated(expr));
        match &expr.kind {
            E::None => Ty::NoneT,
            E::Name(name) => match name.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "list" | "List" => Ty::List(Box::new(Ty::Unset)),
                "dict" | "Dict" => Ty::Dict(Box::new(Ty::Unset), Box::new(Ty::Unset)),
                class if env.shapes.contains_key(class) => Ty::Object(class.to_string()),
                kept if env.kept.contains_key(kept) => Ty::Foreign,
                _ => Ty::Dyn,
            },
            E::Str(lit) => match expression_of(&lit.body, expr.line) {
                Ok(inner) => self.annotated(&inner),
                Err(_) => Ty::Dyn,
            },
            E::Index(of, inner) => match (&of.kind, &inner.kind) {
                (E::Name(name), _) if matches!(name.as_str(), "list" | "List") => Ty::List(item(inner)),
                (E::Name(name), E::Tuple(parts)) if matches!(name.as_str(), "dict" | "Dict") && parts.len() == 2 => Ty::Dict(item(&parts[0]), item(&parts[1])),
                (E::Name(name), _) if name == "Optional" => join(&self.annotated(inner), &Ty::NoneT),
                _ => Ty::Dyn,
            },
            E::Binary("|", left, right) => join(&self.annotated(left), &self.annotated(right)),
            _ => Ty::Dyn,
        }
    }

    /// The words for a value that goes into a variable whose type, over
    /// the whole program, is `whole`.
    fn fitted(&mut self, name: &str, value: &Val, whole: &Ty, line: usize) -> Kept<(String, Ty)> {
        if !self.env.last || fits(&value.ty, whole) {
            return Ok((value.text.clone(), value.ty.clone()));
        }
        match (&value.ty, whole) {
            (Ty::Int, Ty::Float) => {
                self.note(Note::FloatStart, line);
                if self.float_names.insert(name.to_string()) {
                    self.floats.push(name.to_string());
                }
                Ok((as_float(value), Ty::Float))
            }
            (ty, Ty::Foreign) if crosses(ty) => Ok((value.text.clone(), Ty::Foreign)),
            (ty, Ty::Dyn) => keep(line, format!("`{name}` holds {} here and a value of another type, or of a type that is not known, elsewhere", described(ty))),
            (ty, other) => keep(line, format!("`{name}` holds {} here and {} elsewhere, and a variable of Zeno holds one type", described(ty), described(other))),
        }
    }

    /// Whether working this out changes nothing and cannot fail: a
    /// literal, a name, a field of an object.
    fn calm(&mut self, expr: &Expr) -> bool {
        match &expr.kind {
            E::Name(_) => true,
            E::Attr(of, _) => self.calm(of) && matches!(self.expr_type(of), Ty::Object(_)),
            _ => is_fixed(expr),
        }
    }

    /// `name = value`, where the plan says whether the name is declared
    /// on this line.
    fn store(&mut self, stmt: &Stmt, name: &str, value: &Val, line: usize) -> Kept<()> {
        self.zeno_name(name, line)?;
        self.narrow.retain(|narrowed| narrowed != name);
        self.bind(name, &value.ty);
        let whole = self.whole_ty(name);
        let (text, ty) = self.fitted(name, value, &whole, line)?;
        match self.plan_now().declare.get(&(address(stmt), name.to_string())).copied() {
            Some(changes) => {
                let word = if changes { "var" } else { "let" };
                // The type is written where the first value does not say
                // all of it: `none`, an empty list.
                let written = if self.env.last && ty != whole { zeno_type(&whole).map(|written| format!(": {written}")) } else { None };
                self.put(format!("{word} {name}{} = {text}", written.unwrap_or_default()));
            }
            None => self.put(format!("{name} = {text}")),
        }
        Ok(())
    }

    fn assign(&mut self, stmt: &Stmt, target: &Expr, value: &Expr) -> Kept<()> {
        let line = stmt.line;
        match &target.kind {
            E::Name(name) => {
                if let E::IfElse { then, condition, otherwise } = &value.kind {
                    if self.calm(otherwise) && !mentions(condition, name) && !mentions(then, name) {
                        // `x = a if c else b`: `b` first, then `a` if `c`.
                        // `b` is worked out whether or not `c` holds, so
                        // it must be something that working out changes
                        // nothing and cannot fail.
                        let other = self.expr(otherwise)?;
                        let test = self.truth(condition)?;
                        let first = self.expr(then)?;
                        self.store(stmt, name, &other, line)?;
                        self.bind(name, &first.ty);
                        let whole = self.whole_ty(name);
                        let (text, _) = self.fitted(name, &first, &whole, line)?;
                        self.put(format!("if {} {{", test.text));
                        self.indent += 1;
                        self.put(format!("{name} = {text}"));
                        self.indent -= 1;
                        self.put("}");
                        return Ok(());
                    }
                    // Otherwise an `if` with an `else`, and before it the
                    // variable with the first value of its type, if this
                    // is the line that declares it.
                    let test = self.truth(condition)?;
                    let first = self.expr(then)?;
                    let second = self.expr(otherwise)?;
                    self.zeno_name(name, line)?;
                    self.narrow.retain(|narrowed| narrowed != name);
                    self.bind(name, &first.ty);
                    self.bind(name, &second.ty);
                    let whole = self.whole_ty(name);
                    let (first, _) = self.fitted(name, &first, &whole, line)?;
                    let (second, _) = self.fitted(name, &second, &whole, line)?;
                    if self.plan_now().declare.contains_key(&(address(stmt), name.to_string())) && self.env.last {
                        match (first_value(&whole), zeno_type(&whole)) {
                            (Some(value), Some(ty)) => self.put(format!("var {name}: {ty} = {value}")),
                            _ => return keep(line, format!("`{name}` gets its first value from `a if c else b`, and Zeno has no first value for {}", described(&whole))),
                        }
                    }
                    self.put(format!("if {} {{", test.text));
                    self.indent += 1;
                    self.put(format!("{name} = {first}"));
                    self.indent -= 1;
                    self.put("} else {");
                    self.indent += 1;
                    self.put(format!("{name} = {second}"));
                    self.indent -= 1;
                    self.put("}");
                    return Ok(());
                }
                let value = self.expr(value)?;
                self.store(stmt, name, &value, line)
            }
            E::Tuple(names) => self.assign_several(stmt, names, value),
            E::Attr(of, field) => {
                let on = self.expr(of)?;
                let value = self.expr(value)?;
                match &on.ty {
                    Ty::Unset => {}
                    Ty::Object(class) => {
                        let env = self.env;
                        if !env.shapes.get(class).is_some_and(|shape| shape.fields.iter().any(|known| &known.name == field)) {
                            return keep(line, format!("`{field}` is not a field that `__init__` of `{class}` gives a value"));
                        }
                        let key = (class.clone(), field.clone());
                        let before = env.known.fields.get(&key).map_or(Ty::Unset, |field| field.ty.clone());
                        self.found.fields.entry(key).or_default().add(&value.ty, &before);
                        let place = format!("{}.{field}", on.at(ATOM));
                        self.narrow.retain(|narrowed| *narrowed != place);
                        self.put(format!("{place} = {}", value.text));
                    }
                    Ty::Foreign if crosses(&value.ty) => {
                        self.foreign_calls += 1;
                        self.put(format!("{}.{field} = {}", on.at(ATOM), value.text));
                    }
                    Ty::Foreign => return keep(line, format!("{} is put in a field of a value of Python", described(&value.ty))),
                    other => return keep(line, format!("a field of {} is given a value", described(other))),
                }
                Ok(())
            }
            E::Index(of, key) => {
                let place = self.expr(target)?;
                let value = self.expr(value)?;
                let on = self.expr_type(of);
                match on {
                    Ty::List(_) => self.widen(of, Ty::List(Box::new(value.ty.clone()))),
                    Ty::Dict(..) => {
                        let key = self.expr_type(key);
                        self.widen(of, Ty::Dict(Box::new(key), Box::new(value.ty.clone())));
                    }
                    Ty::Foreign if !crosses(&value.ty) => return keep(line, format!("{} is put in a value of Python", described(&value.ty))),
                    Ty::Str => return keep(line, "one letter of a text is given a value"),
                    _ => {}
                }
                self.put(format!("{} = {}", place.text, value.text));
                Ok(())
            }
            _ => keep(line, "a value is given to something that is not a name, a field or an item"),
        }
    }

    /// The type of an expression that is being written elsewhere.
    pub(super) fn expr_type(&mut self, expr: &Expr) -> Ty {
        let state = (self.notes.len(), self.foreign_calls);
        let ty = self.expr(expr).map_or(Ty::Unset, |value| value.ty);
        self.notes.truncate(state.0);
        self.foreign_calls = state.1;
        ty
    }

    /// `a, b = b, a` and `x, y = point()`.
    fn assign_several(&mut self, stmt: &Stmt, names: &[Expr], value: &Expr) -> Kept<()> {
        let line = stmt.line;
        let mut targets = Vec::new();
        for name in names {
            match &name.kind {
                E::Name(name) => {
                    self.zeno_name(name, line)?;
                    targets.push(name.clone());
                }
                _ => return keep(line, "several values given at once to something other than names"),
            }
        }
        let (texts, types): (Vec<String>, Vec<Ty>) = match &value.kind {
            E::Tuple(values) => {
                if values.len() != names.len() {
                    return keep(line, "as many names as values are needed");
                }
                let mut texts = Vec::new();
                let mut types = Vec::new();
                for value in values {
                    let value = self.expr(value)?;
                    texts.push(value.text);
                    types.push(value.ty);
                }
                (texts, types)
            }
            _ => {
                let value = self.expr(value)?;
                match &value.ty {
                    Ty::Several(types) if types.len() == names.len() => (vec![value.text], types.clone()),
                    Ty::Unset => (vec![value.text], vec![Ty::Unset; names.len()]),
                    other => return keep(line, format!("several names take their values from {}", described(other))),
                }
            }
        };
        let declared: Vec<Option<bool>> = targets.iter().map(|name| self.plan_now().declare.get(&(address(stmt), name.clone())).copied()).collect();
        for (name, ty) in targets.iter().zip(&types) {
            self.narrow.retain(|narrowed| narrowed != name);
            self.bind(name, ty);
            let whole = self.whole_ty(name);
            if self.env.last && !fits(ty, &whole) {
                return keep(line, format!("`{name}` holds {} here and {} elsewhere", described(ty), described(&whole)));
            }
        }
        let word = if declared.iter().all(|declared| declared.is_some()) {
            if declared.contains(&Some(true)) {
                "var "
            } else {
                "let "
            }
        } else if declared.iter().all(|declared| declared.is_none()) {
            ""
        } else {
            return keep(line, "some of these names get their first value here and some had one already");
        };
        self.put(format!("{word}{} = {}", targets.join(", "), texts.join(", ")));
        Ok(())
    }

    fn augmented(&mut self, target: &Expr, op: &str, value: &Expr, line: usize) -> Kept<()> {
        if !matches!(target.kind, E::Name(_) | E::Attr(..) | E::Index(..)) {
            return keep(line, format!("`{op}=` on something that is not a name, a field or an item"));
        }
        let place = self.expr(target)?;
        let value = self.expr(value)?;
        if matches!(place.ty, Ty::List(_)) {
            return keep(line, format!("`{op}=` on a list: Python changes the list where it is, for everything that holds it"));
        }
        let result = self.binary(op, &place, &value, line)?;
        match &target.kind {
            E::Name(name) => {
                self.zeno_name(name, line)?;
                self.bind(name, &result.ty);
            }
            E::Attr(..) => self.widen(target, result.ty.clone()),
            E::Index(of, key) => match self.expr_type(of) {
                Ty::List(_) => self.widen(of, Ty::List(Box::new(result.ty.clone()))),
                Ty::Dict(..) => {
                    let key = self.expr_type(key);
                    self.widen(of, Ty::Dict(Box::new(key), Box::new(result.ty.clone())));
                }
                _ => {}
            },
            _ => {}
        }
        // A float that a whole number is added to stays a float, and
        // Zeno wants the number written as one.
        let text = if place.ty == Ty::Float && value.ty == Ty::Int { as_float(&value) } else { value.text };
        self.put(format!("{} {op}= {text}", place.text));
        Ok(())
    }

    fn for_loop(&mut self, stmt: &Stmt, python_names: &[String], items: &Expr, body: &[Stmt], line: usize) -> Kept<()> {
        for name in python_names {
            self.zeno_name(name, line)?;
        }
        // A name that outlasts its loop is a variable, and the loop
        // counts with a name of its own (`outlasting`).
        let names: Vec<String> = python_names.iter().map(|name| self.plan_now().renamed.get(&(address(stmt), name.clone())).unwrap_or(name).clone()).collect();
        let names = names.as_slice();
        let types: Vec<Ty>;
        let calls = self.foreign_calls;
        let mut foreign = false;
        let mut columns: Vec<String> = Vec::new();
        let header = match (pairs_of(items), names) {
            // `for k, v in d.items()` and `for i, x in enumerate(xs)`:
            // a `for` of Zeno with two names gives exactly these.
            (Some((how @ ("items" | "enumerate"), of)), [first, second]) => {
                let on = self.expr(of)?;
                let (one, two) = match (how, &on.ty) {
                    (_, Ty::Unset) => (Ty::Unset, Ty::Unset),
                    ("items", Ty::Dict(key, value)) => ((**key).clone(), (**value).clone()),
                    ("enumerate", Ty::List(item)) => (Ty::Int, (**item).clone()),
                    ("enumerate", Ty::Str) => (Ty::Int, Ty::Str),
                    (_, other) => return keep(line, format!("a loop over `{how}` of {}", described(other))),
                };
                self.unchanged(of, body, false, line)?;
                types = vec![one, two];
                format!("for {first}, {second} in {} {{", on.text)
            }
            (Some((how @ ("keys" | "values"), of)), [name]) => {
                let on = self.expr(of)?;
                let each = match &on.ty {
                    Ty::Unset => Ty::Unset,
                    Ty::Dict(key, value) => (if how == "keys" { &**key } else { &**value }).clone(),
                    other => return keep(line, format!("a loop over `{how}` of {}", described(other))),
                };
                self.unchanged(of, body, how == "keys", line)?;
                types = vec![each];
                match how {
                    "keys" => format!("for {name} in {} {{", on.text),
                    _ => format!("for {name} in {}.values() {{", on.at(ATOM)),
                }
            }
            (_, [name]) => {
                let (over, each) = self.over(items, line)?;
                foreign = each == Ty::Foreign;
                let values_only = matches!(self.expr_type(items), Ty::Dict(..));
                self.unchanged(items, body, values_only, line)?;
                types = vec![each];
                format!("for {name} in {over} {{")
            }
            // `for name, year in [("ann", 2019), ("bob", 2021)]`: a list of
            // each, gone over by its place.
            (None, [first, ..]) if fixed_rows(items, names.len()) => {
                let E::List(rows) = &items.kind else { return keep(line, "a `for` over rows") };
                let mut values: Vec<Vec<Val>> = vec![Vec::new(); names.len()];
                let mut each: Vec<Ty> = vec![Ty::Unset; names.len()];
                for row in rows {
                    let E::Tuple(parts) = &row.kind else { continue };
                    for (at, part) in parts.iter().enumerate() {
                        let value = self.expr(part)?;
                        each[at] = join(&each[at], &value.ty);
                        values[at].push(value);
                    }
                }
                let mut texts = Vec::new();
                for (column, ty) in values.iter().zip(&each) {
                    if !matches!(ty, Ty::Int | Ty::Float | Ty::Str | Ty::Bool | Ty::Unset) {
                        return keep(line, format!("a `for` over rows that hold {}", described(ty)));
                    }
                    // Each value of Python keeps its own type: `2` among
                    // floats is a whole number, and is printed as one.
                    if column.iter().any(|value| value.ty != *ty && value.ty != Ty::Unset) {
                        return keep(line, "a `for` over rows where one place holds whole numbers and floats both");
                    }
                    let items: Vec<String> = column.iter().map(|value| value.text.clone()).collect();
                    texts.push(format!("[{}]", items.join(", ")));
                }
                let row = format!("row_{line}");
                if self.env.file_names.contains(&row) {
                    return keep(line, format!("the converter goes over these rows with a name `{row}`, and the file has that name already"));
                }
                for (name, text) in names.iter().zip(&texts).skip(1) {
                    columns.push(format!("let {name} = {text}[{row}]"));
                }
                types = each;
                format!("for {row}, {first} in {} {{", texts[0])
            }
            _ => return keep(line, "a `for` with several names, over something other than `d.items()`, `enumerate(a_list)` or rows written out"),
        };
        self.put(header);
        self.indent += 1;
        for column in columns {
            self.put(column);
        }
        for ((name, counts_with), ty) in python_names.iter().zip(names).zip(types) {
            if name == counts_with {
                self.bind(name, &ty);
            } else {
                self.store(stmt, name, &atom(counts_with.clone(), ty), line)?;
            }
        }
        self.indent -= 1;
        self.inner(body)?;
        self.put("}");
        if foreign && self.foreign_calls > calls + 1 {
            return keep(line, "a loop over a value of Python whose block calls Python: Zeno takes the items when the loop begins, and Python as it goes");
        }
        Ok(())
    }

    /// A `for` of Zeno takes its items when the loop begins. Python
    /// takes each as it comes to it. The two are the same as long as
    /// the block does not change what the loop goes over.
    fn unchanged(&self, items: &Expr, body: &[Stmt], values_only: bool, line: usize) -> Kept<()> {
        if is_plain_place(items) && changes(body, items, values_only) {
            return keep(line, "the block of this loop may change what the loop goes over: Zeno takes the items when the loop begins, and Python as it goes");
        }
        let mut calls = Vec::new();
        each_expr_of(body, &mut |expr| {
            if matches!(expr.kind, E::Call { .. }) {
                calls.push(expr.clone());
            }
        });
        let calls: Vec<&Expr> = calls.iter().collect();
        self.unchanged_by_calls(items, &calls, line)
    }

    /// The same, for what the functions called in these expressions do:
    /// a function of the file may change a variable of the file, or a
    /// field, that the loop goes over.
    pub(super) fn unchanged_by_calls(&self, items: &Expr, parts: &[&Expr], line: usize) -> Kept<()> {
        let place = match &items.kind {
            E::Name(name) if !self.is_local(name) && !self.temps.iter().any(|(temp, _)| temp == name) => name.clone(),
            E::Attr(_, field) => format!(".{field}"),
            _ => return Ok(()),
        };
        let env = self.env;
        let mut by = None;
        for part in parts {
            each_expr(part, &mut |expr| {
                if let E::Call { function, .. } = &expr.kind {
                    let called = match &function.kind {
                        E::Name(name) | E::Attr(_, name) => name,
                        _ => return,
                    };
                    if env.effects.get(called).is_some_and(|changed| changed.contains(&place)) {
                        by = Some(called.clone());
                    }
                }
            });
        }
        match by {
            Some(called) => keep(line, format!("`{called}`, which the loop calls, may change what the loop goes over: Zeno takes the items when the loop begins, and Python as it goes")),
            None => Ok(()),
        }
    }

    fn returned(&mut self, ty: &Ty) {
        let before = self.env.known.functions.get(&self.scope).map_or(Ty::Unset, |sig| sig.ret.ty.clone());
        self.found.functions.entry(self.scope.clone()).or_default().ret.add(ty, &before);
    }

    fn returns(&mut self, value: Option<&Expr>, line: usize) -> Kept<()> {
        if !self.in_function {
            return keep(line, "a `return` outside a function");
        }
        let Some(value) = value else {
            self.returned(&Ty::NoneT);
            self.put("return");
            return Ok(());
        };
        match &value.kind {
            E::IfElse { then, condition, otherwise } => {
                let test = self.truth(condition)?;
                let first = self.expr(then)?;
                let second = self.expr(otherwise)?;
                self.returned(&first.ty);
                self.returned(&second.ty);
                self.put(format!("if {} {{", test.text));
                self.indent += 1;
                self.put(format!("return {}", first.text));
                self.indent -= 1;
                self.put("}");
                self.put(format!("return {}", second.text));
            }
            E::Tuple(values) => {
                let mut texts = Vec::new();
                let mut types = Vec::new();
                for value in values {
                    let value = self.expr(value)?;
                    texts.push(value.text);
                    types.push(value.ty);
                }
                self.returned(&Ty::Several(types));
                self.put(format!("return {}", texts.join(", ")));
            }
            _ => {
                let value = self.expr(value)?;
                self.returned(&value.ty);
                self.put(format!("return {}", value.text));
            }
        }
        Ok(())
    }

    fn raises(&mut self, error: Option<&Expr>, line: usize) -> Kept<()> {
        let Some(error) = error else {
            return match self.caught.last() {
                Some(Some(name)) => {
                    self.put(format!("raise {name}"));
                    Ok(())
                }
                Some(None) => keep(line, "a `raise` with nothing after it, in an `except` that gives the error no name"),
                None => keep(line, "a `raise` with nothing after it, outside an `except`"),
            };
        };
        let (kind, message) = match &error.kind {
            E::Name(name) if self.is_var(name) => {
                return match self.var_ty(name) {
                    Ty::Error | Ty::Unset => {
                        self.put(format!("raise {name}"));
                        Ok(())
                    }
                    other => keep(line, format!("{} is raised", described(&other))),
                };
            }
            E::Name(name) => (name, None),
            E::Call { function, args, named } if named.is_empty() && args.len() <= 1 => match &function.kind {
                E::Name(name) if !self.is_var(name) => (name, args.first()),
                _ => return keep(line, "what is raised is not an error the converter knows"),
            },
            _ => return keep(line, "what is raised is not an error the converter knows"),
        };
        let Some(zeno) = error_kind(kind) else {
            return keep(line, format!("`raise {kind}`: Zeno has no error of that kind"));
        };
        let text = match message {
            None => "\"\"".to_string(),
            Some(message) => {
                let message = self.expr(message)?;
                match &message.ty {
                    Ty::Str | Ty::Unset => message.text,
                    other => return keep(line, format!("an error whose message is {}, where Zeno wants a text", described(other))),
                }
            }
        };
        self.put(format!("raise {zeno}({text})"));
        Ok(())
    }

    // ---------------------------------------------- Functions and classes

    /// A function at the left margin of the file, or a method of a
    /// class there.
    pub(super) fn function(&mut self, def: &Def, class: Option<&str>, line: usize) -> Kept<()> {
        self.zeno_name(&def.name, line)?;
        if let Some(inner) = has_inner_function(&def.body) {
            return keep(inner, "a function written inside a function, which is not translated yet");
        }
        let key = match class {
            Some(class) => format!("{class}.{}", def.name),
            None => def.name.clone(),
        };
        let params: &[Param] = if class.is_some() { &def.params[1..] } else { &def.params };
        let env = self.env;
        self.scope = key.clone();
        self.in_function = true;
        self.class = class.map(str::to_string);
        self.params = def.params.iter().map(|param| param.name.clone()).collect();
        self.globals = HashSet::new();
        global_names(&def.body, &mut self.globals);
        self.plan = Plan::default();
        let done = self.function_inside(def, &key, params, class.is_some(), line);
        self.in_function = false;
        self.class = None;
        self.params.clear();
        self.globals.clear();
        self.plan = Plan::default();
        self.scope = String::new();
        self.narrow.clear();
        let _ = env;
        done
    }

    fn function_inside(&mut self, def: &Def, key: &str, params: &[Param], method: bool, line: usize) -> Kept<()> {
        let env = self.env;
        let before = env.known.functions.get(key).cloned().unwrap_or_default();
        let mut written = Vec::new();
        let mut changed = HashSet::new();
        bound_names(&def.body, &mut changed);
        let mut renamed = Vec::new();
        if method {
            written.push("self".to_string());
        }
        for (at, param) in params.iter().enumerate() {
            self.zeno_name(&param.name, line)?;
            let was = before.params.get(at).cloned().unwrap_or_default();
            let mut default = None;
            let mut said = Vec::new();
            if let Some(annotation) = &param.annotation {
                said.push(self.annotated(annotation));
            }
            if let Some(value) = &param.default {
                if !is_fixed(value) {
                    return keep(line, format!("the default of `{}` is more than a literal: Python works it out once, when the function is made, and Zeno at each call", param.name));
                }
                let value = self.expr(value)?;
                said.push(value.ty.clone());
                default = Some(value);
            }
            {
                let sig = self.found.functions.entry(key.to_string()).or_default();
                if sig.params.len() < params.len() {
                    sig.params.resize(params.len(), Joined::default());
                }
                for ty in &said {
                    sig.params[at].add(ty, &was.ty);
                }
            }
            let ty = self.settled(was.ty.clone());
            let into = self.found.vars.entry((key.to_string(), param.name.clone())).or_insert(Ty::Unset);
            *into = join(into, &ty);
            // A parameter cannot be given a new value in Zeno. One that
            // is, in Python, comes in under another name and is put in a
            // variable of the old name on the first line.
            let mut text = if changed.contains(&param.name) {
                let given = format!("{}_given", param.name);
                if self.is_var(&given) || changed.contains(&given) || params.iter().any(|other| other.name == given) {
                    return keep(line, format!("`{}` is given a new value, and `{given}` is a name already", param.name));
                }
                renamed.push(param.name.clone());
                given
            } else {
                param.name.clone()
            };
            if !was.loose {
                if let Some(ty) = zeno_type(&ty) {
                    text = format!("{text}: {ty}");
                }
            }
            if let Some(value) = default {
                let value = if ty == Ty::Float && value.ty == Ty::Int && !was.loose { as_float(&value) } else { value.text };
                text = format!("{text} = {value}");
            }
            written.push(text);
        }
        let given: Vec<String> = def.params.iter().map(|param| param.name.clone()).collect();
        let nothing = HashSet::new();
        self.plan = match plan(&[&def.body], &[], &given, &nothing, &nothing, env.known, key) {
            Ok(plan) => plan,
            Err(keep) => return Err(keep),
        };
        // What it gives: nothing, if no `return` has a value, or if a
        // way through it ends with none.
        let value = returns_a_value(&def.body);
        let always = always_leaves(&def.body);
        if !value || !always {
            self.returned(&Ty::NoneT);
        }
        let gives = match &before.ret {
            ret if env.last && value && always && !ret.loose && !matches!(ret.ty, Ty::NoneT) => zeno_type(&ret.ty).map(|ty| format!(" -> {ty}")),
            _ => None,
        };
        self.put(format!("function {}({}){} {{", def.name, written.join(", "), gives.unwrap_or_default()));
        self.indent += 1;
        for name in renamed {
            self.put(format!("var {name} = {name}_given"));
        }
        self.indent -= 1;
        self.inner(&def.body)?;
        self.put("}");
        Ok(())
    }

    pub(super) fn class(&mut self, name: &str, shape: &Shape, line: usize) -> Kept<()> {
        self.zeno_name(name, line)?;
        let env = self.env;
        self.put(format!("class {name} {{"));
        self.indent += 1;
        for field in &shape.fields {
            self.zeno_name(&field.name, line)?;
            let key = (name.to_string(), field.name.clone());
            let was = env.known.fields.get(&key).cloned().unwrap_or_default();
            let mut said = Vec::new();
            if let Some(annotation) = field.annotation {
                said.push(self.annotated(annotation));
            }
            let mut default = None;
            if let Some(value) = field.default {
                let value = self.expr(value)?;
                said.push(value.ty.clone());
                default = Some(value);
            }
            let found = self.found.fields.entry(key).or_default();
            for ty in &said {
                found.add(ty, &was.ty);
            }
            let ty = self.settled(was.ty.clone());
            let mut text = field.name.clone();
            if !was.loose {
                if let Some(ty) = zeno_type(&ty) {
                    text = format!("{text}: {ty}");
                }
            }
            if let Some(value) = default {
                text = format!("{text} = {}", value.text);
            }
            self.put(text);
        }
        let mut methods: Vec<&&Def> = shape.methods.values().collect();
        methods.sort_by_key(|def| shape.order.iter().position(|name| *name == def.name));
        for def in methods {
            self.put("");
            self.indent -= 1;
            self.comments_before(shape.lines.get(&def.name).copied().unwrap_or(line));
            self.indent += 1;
            self.function(def, Some(name), shape.lines.get(&def.name).copied().unwrap_or(line))?;
        }
        self.indent -= 1;
        self.put("}");
        Ok(())
    }
}
