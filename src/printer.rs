//! Prints a syntax tree back out as text, so you can see what the parser understood.
//!
//! Blocks are shown by indentation, and every operator gets its own
//! brackets, which makes precedence visible: `1 + 2 * 3` prints as
//! `(1 + (2 * 3))`.

use crate::ast::*;

/// The whole program as an indented outline.
pub fn print(program: &Program) -> String {
    let mut out = String::new();
    if program.strict {
        out.push_str("strict\n");
    }
    match program.memory {
        Some(Memory::Owned) => out.push_str("memory owned\n"),
        Some(Memory::Collector) => out.push_str("memory collector\n"),
        None => {}
    }
    for statement in &program.statements {
        let name = match &statement.kind {
            StmtKind::Let { name, .. } | StmtKind::Const { name, .. } => Some(name),
            StmtKind::Function(function) => Some(&function.name),
            StmtKind::Class(class) => Some(&class.name),
            StmtKind::Enum(decl) => Some(&decl.name),
            _ => None,
        };
        if name.is_some_and(|name| program.private.contains(name)) {
            out.push_str("private ");
        }
        stmt(statement, 0, &mut out);
    }
    out
}

fn line(text: &str, depth: usize, out: &mut String) {
    out.push_str(&"    ".repeat(depth));
    out.push_str(text);
    out.push('\n');
}

fn block(statements: &[Stmt], depth: usize, out: &mut String) {
    for statement in statements {
        stmt(statement, depth, out);
    }
}

fn stmt(statement: &Stmt, depth: usize, out: &mut String) {
    match &statement.kind {
        StmtKind::Let { mutable, name, ty, value } => {
            let keyword = if *mutable { "var" } else { "let" };
            line(&format!("{keyword} {name}{} = {}", annotation(ty), expr(value)), depth, out);
        }
        StmtKind::Const { name, ty, value } => {
            line(&format!("const {name}{} = {}", annotation(ty), expr(value)), depth, out);
        }
        StmtKind::Assign { target, op, value } => {
            let op = op.map(BinaryOp::text).unwrap_or("");
            line(&format!("{} {op}= {}", expr(target), expr(value)), depth, out);
        }
        StmtKind::Receive { declare, places, value } => {
            let keyword = match declare {
                Some(true) => "var ",
                Some(false) => "let ",
                None => "",
            };
            let places = join(places, |place| match place {
                Place::Name { name, ty } => format!("{name}{}", annotation(ty)),
                Place::Drop => "_".to_string(),
                Place::Target(target) => expr(target),
            });
            line(&format!("{keyword}{places} = {}", expr(value)), depth, out);
        }
        StmtKind::Expr(e) => line(&expr(e), depth, out),

        StmtKind::Function(f) => function(f, depth, out),
        StmtKind::Class(class) => {
            let owned = if class.owned { "owned " } else { "" };
            let word = if class.interface { "interface" } else { "class" };
            let is = if class.is.is_empty() { String::new() } else { format!(" is {}", class.is.iter().map(type_).collect::<Vec<_>>().join(", ")) };
            line(&format!("{owned}{word} {}{}{is}", class.name, type_params(&class.type_params)), depth, out);
            for f in &class.fields {
                let private = if class.private.contains(&f.name) { "private " } else { "" };
                line(&format!("{private}field {}", field(f)), depth + 1, out);
            }
            for method in &class.methods {
                let mut text = String::new();
                function(method, depth + 1, &mut text);
                if class.private.contains(&method.name) {
                    text.insert_str((depth + 1) * 4, "private ");
                }
                out.push_str(&text);
            }
        }
        StmtKind::Enum(e) => {
            line(&format!("enum {}{}", e.name, type_params(&e.type_params)), depth, out);
            for variant in &e.variants {
                let mut text = variant.name.clone();
                if !variant.fields.is_empty() {
                    text += &format!("({})", join(&variant.fields, field));
                }
                if let Some((number, _)) = variant.number {
                    text += &format!(" = {number}");
                }
                line(&text, depth + 1, out);
            }
        }

        StmtKind::If { .. } => if_chain(statement, "if", depth, out),
        StmtKind::While { condition, body } => {
            line(&format!("while {}", expr(condition)), depth, out);
            block(body, depth + 1, out);
        }
        StmtKind::Unsafe(body) => {
            line("unsafe", depth, out);
            block(body, depth + 1, out);
        }
        // Made by the checker, after which nothing is printed back as
        // Zeno: shown, for `zeno parse` of a checked tree, as what it is.
        StmtKind::Scoped { name, body } => {
            line(&format!("released at the end: {name}"), depth, out);
            block(body, depth + 1, out);
        }
        StmtKind::Test { name, body } => {
            line(&format!("test {name:?}"), depth, out);
            block(body, depth + 1, out);
        }
        StmtKind::Assert(condition, None) => line(&format!("assert {}", expr(condition)), depth, out),
        StmtKind::Assert(condition, Some(message)) => {
            line(&format!("assert {}, {}", expr(condition), expr(message)), depth, out)
        }
        StmtKind::AssertRaises { error, body } => {
            line(&format!("assert raises {error}"), depth, out);
            block(body, depth + 1, out);
        }
        StmtKind::For { variable, second, iterable, body } => {
            let names = match second {
                Some(second) => format!("{variable}, {second}"),
                None => variable.clone(),
            };
            line(&format!("for {names} in {}", expr(iterable)), depth, out);
            block(body, depth + 1, out);
        }
        StmtKind::Match { subject, arms } => {
            line(&format!("match {}", expr(subject)), depth, out);
            for arm in arms {
                line(&format!("case {}", pattern(&arm.pattern)), depth + 1, out);
                block(&arm.body, depth + 2, out);
            }
        }
        StmtKind::Try { body, handlers } => {
            line("try", depth, out);
            block(body, depth + 1, out);
            for handler in handlers {
                let mut text = "except".to_string();
                if let Some(error) = &handler.error {
                    text += &format!(" {error}");
                }
                if let Some(binding) = &handler.binding {
                    text += &format!(" as {binding}");
                }
                line(&text, depth, out);
                block(&handler.body, depth + 1, out);
            }
        }

        StmtKind::Return(None) => line("return", depth, out),
        StmtKind::Return(Some(value)) => line(&format!("return {}", expr(value)), depth, out),
        StmtKind::Raise(value) => line(&format!("raise {}", expr(value)), depth, out),
        StmtKind::Break => line("break", depth, out),
        StmtKind::Continue => line("continue", depth, out),

        StmtKind::Import { module, names } => {
            let module = module.join(".");
            if names.is_empty() {
                line(&format!("import {module}"), depth, out);
            } else {
                line(&format!("from {module} import {}", names.join(", ")), depth, out);
            }
        }
    }
}

/// Prints `if`, then each `else if`, then the final `else`, all at one depth.
fn if_chain(statement: &Stmt, keyword: &str, depth: usize, out: &mut String) {
    let StmtKind::If { condition, then_block, else_block } = &statement.kind else {
        return;
    };
    line(&format!("{keyword} {}", expr(condition)), depth, out);
    block(then_block, depth + 1, out);

    match else_block.as_deref() {
        None => {}
        // An else block that is exactly one `if` was written as `else if`.
        Some([only]) if matches!(only.kind, StmtKind::If { .. }) => {
            if_chain(only, "else if", depth, out);
        }
        Some(statements) => {
            line("else", depth, out);
            block(statements, depth + 1, out);
        }
    }
}

fn function(f: &Function, depth: usize, out: &mut String) {
    line(&signature(f), depth, out);
    block(&f.body, depth + 1, out);
}

/// What a function returns. A `raises` after it would belong to the
/// function, so a returned function type that raises is bracketed.
fn result(ty: &Type) -> String {
    match ty {
        Type::Function { raises: Some(_), .. } => format!("({})", type_(ty)),
        _ => type_(ty),
    }
}

/// `function name(params) -> type raises Error`
fn signature(f: &Function) -> String {
    let space = if f.name.is_empty() { "" } else { " " };
    let mut text = format!("function{space}{}{}({})", f.name, type_params(&f.type_params), join(&f.params, param));
    if let Some(ty) = &f.return_type {
        text += &format!(" -> {}", result(ty));
    }
    if !f.raises.is_empty() {
        text += &format!(" raises {}", f.raises.join(", "));
    }
    text
}

/// `[T, U is Shape]`, or nothing when there are none.
fn type_params(params: &[TypeParam]) -> String {
    let shown: Vec<String> = params
        .iter()
        .map(|param| match &param.bound {
            Some(bound) => format!("{} is {}", param.name, type_(bound)),
            None => param.name.clone(),
        })
        .collect();
    if shown.is_empty() { String::new() } else { format!("[{}]", shown.join(", ")) }
}

fn param(p: &Param) -> String {
    let mut text = format!("{}{}", p.name, annotation(&p.ty));
    if let Some(default) = &p.default {
        text += &format!(" = {}", expr(default));
    }
    text
}

fn field(f: &Field) -> String {
    let mut text = format!("{}{}", f.name, annotation(&f.ty));
    if let Some(default) = &f.default {
        text += &format!(" = {}", expr(default));
    }
    text
}

/// `: type` if there is one, otherwise nothing.
fn annotation(ty: &Option<Type>) -> String {
    match ty {
        Some(ty) => format!(": {}", type_(ty)),
        None => String::new(),
    }
}

pub fn type_(ty: &Type) -> String {
    match ty {
        Type::Named { name, args } if args.is_empty() => name.clone(),
        Type::Named { name, args } => format!("{name}[{}]", join(args, type_)),
        // `function(int) -> int or none` is a function that may return
        // `none`. A function type that may itself be `none` is bracketed.
        Type::Optional(inner) if matches!(**inner, Type::Function { .. }) => format!("({}) or none", type_(inner)),
        Type::Optional(inner) => format!("{} or none", type_(inner)),
        Type::Owned(inner) => format!("owned {}", type_(inner)),
        Type::Take(inner) => format!("take {}", type_(inner)),
        Type::Length(n) => n.to_string(),
        Type::Several(types) => format!("({})", join(types, type_)),
        Type::Function { params, returns, raises } => {
            let mut text = format!("function({})", join(params, type_));
            if let Some(returns) = returns {
                text += &format!(" -> {}", result(returns));
            }
            if let Some(raises) = raises {
                text += &format!(" raises {raises}");
            }
            text
        }
    }
}

fn pattern(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard => "_".to_string(),
        Pattern::Name(name) => name.clone(),
        Pattern::Literal(value) => expr(value),
        Pattern::Variant { name, fields } => format!("{name}({})", join(fields, pattern)),
    }
}

/// One expression on one line, with brackets around every operator.
pub fn expr(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Int(n) => n.to_string(),
        ExprKind::Float(x) => format!("{x:?}"),
        ExprKind::Str(s) => format!("{s:?}"),
        ExprKind::Inline { language, text } => format!("(inline {language} {text:?})"),
        ExprKind::FormatStr(parts) => {
            let mut text = String::from("f\"");
            for part in parts {
                match part {
                    FormatPart::Text(t) => text += t,
                    FormatPart::Value { value, format: None } => {
                        text += &format!("{{{}}}", expr(value));
                    }
                    FormatPart::Value { value, format: Some(spec) } => {
                        text += &format!("{{{}:{spec}}}", expr(value));
                    }
                }
            }
            text + "\""
        }
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::None => "none".to_string(),
        ExprKind::Name(name) | ExprKind::Give(name) => name.clone(),
        ExprKind::Unit { value, unit } => format!("{} {unit}", expr(value)),
        ExprKind::List(items) => format!("[{}]", join(items, expr)),
        ExprKind::Dict(entries) if entries.is_empty() => "[:]".to_string(),
        ExprKind::Dict(entries) => {
            let entries: Vec<String> = entries.iter().map(|(key, value)| format!("{}: {}", expr(key), expr(value))).collect();
            format!("[{}]", entries.join(", "))
        }
        ExprKind::ListComp { element, variable, iterable, condition } => {
            let mut text = format!("[{} for {variable} in {}", expr(element), expr(iterable));
            if let Some(condition) = condition {
                text += &format!(" if {}", expr(condition));
            }
            text + "]"
        }
        ExprKind::Unary { op: UnaryOp::Neg, operand } => format!("(-{})", expr(operand)),
        ExprKind::Unary { op: UnaryOp::Not, operand } => format!("(not {})", expr(operand)),
        ExprKind::Unary { op: UnaryOp::BitNot, operand } => format!("(~{})", expr(operand)),
        ExprKind::Binary { op, left, right } => {
            format!("({} {} {})", expr(left), op.text(), expr(right))
        }
        ExprKind::Compare { first, rest } => {
            let mut text = format!("({}", expr(first));
            for (op, operand) in rest {
                text += &format!(" {} {}", op.text(), expr(operand));
            }
            text + ")"
        }
        ExprKind::Function(f) => {
            // An expression has to fit on one line, so the body goes inside braces.
            let mut body = String::new();
            block(&f.body, 0, &mut body);
            let statements: Vec<&str> = body.lines().map(str::trim).collect();
            if statements.is_empty() {
                format!("{} {{ }}", signature(f))
            } else {
                format!("{} {{ {} }}", signature(f), statements.join("; "))
            }
        }
        ExprKind::Call { callee, args } => format!("{}({})", expr(callee), join(args, arg)),
        ExprKind::Field { object, name, .. } => format!("{}.{name}", expr(object)),
        ExprKind::Index { object, index } => format!("{}[{}]", expr(object), expr(index)),
        ExprKind::Take(place) => format!("(take {})", expr(place)),
        ExprKind::Several(values) => join(values, expr),
    }
}

fn arg(a: &Arg) -> String {
    match &a.name {
        Some(name) => format!("{name}={}", expr(&a.value)),
        None => expr(&a.value),
    }
}

/// Print each item and put ", " between them.
fn join<T>(items: &[T], print: impl Fn(&T) -> String) -> String {
    items.iter().map(print).collect::<Vec<_>>().join(", ")
}
