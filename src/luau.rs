//! State-tree policy around the shared expression engine: assignment locations,
//! lazy event projection metadata, boolean guards, and authored source spans.
pub use bevy_luau::LuauValue;
use bevy_luau::{CompiledLuauExpression as EngineExpression, Error};
use full_moon::{
    ast::{Expression, FunctionCall, Index, Prefix, Stmt, Suffix, Var, VarExpression},
    tokenizer::{TokenReference, TokenType},
    visitors::Visitor,
};
use std::{collections::BTreeMap, ops::Deref};

pub(crate) fn environment() -> &'static bevy_luau::EnvironmentSpec {
    static SPEC: std::sync::OnceLock<bevy_luau::EnvironmentSpec> = std::sync::OnceLock::new();
    SPEC.get_or_init(|| {
        let mut builder = bevy_luau::EnvironmentSpec::builder().globals([
            "assert",
            "error",
            "ipairs",
            "pairs",
            "next",
            "select",
            "tonumber",
            "tostring",
            "type",
            "typeof",
            "print",
            "pcall",
            "xpcall",
            "unpack",
            "gcinfo",
            "newproxy",
            "getfenv",
            "setfenv",
            "getmetatable",
            "setmetatable",
            "rawequal",
            "rawget",
            "rawset",
            "rawlen",
            "_G",
            "_VERSION",
        ]);
        for name in [
            "math",
            "string",
            "table",
            "utf8",
            "bit32",
            "buffer",
            "vector",
            "coroutine",
            "os",
            "debug",
        ] {
            builder = builder.library(name, &[]);
        }
        builder
            .build()
            .expect("valid state-tree expression environment")
    })
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledLuauExpression {
    engine: EngineExpression,
    #[cfg(test)]
    pub source: Box<str>,
    #[cfg(test)]
    pub boolean: bool,
    pub span_start: usize,
    pub reads_event_data: bool,
}

impl Deref for CompiledLuauExpression {
    type Target = EngineExpression;
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

pub(crate) fn compile_expression(
    source: &str,
    boolean: bool,
    span_start: usize,
) -> Result<CompiledLuauExpression, Error> {
    let engine = environment().compile(source, boolean)?;
    let ast =
        full_moon::parse(&format!("return ({source})")).map_err(|errors| Error::SyntaxError {
            message: errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
            incomplete_input: false,
        })?;
    let mut visitor = EventDataVisitor::default();
    visitor.visit_ast(&ast);
    Ok(CompiledLuauExpression {
        engine,
        #[cfg(test)]
        source: source.into(),
        #[cfg(test)]
        boolean,
        span_start,
        reads_event_data: visitor.found,
    })
}

pub(crate) fn evaluate(
    expression: &CompiledLuauExpression,
    names: &[Box<str>],
    values: &[LuauValue],
    event_name: Option<&str>,
    event_data: Option<&LuauValue>,
) -> Result<LuauValue, Error> {
    let event = event_name
        .filter(|_| {
            !expression.is_native() || expression.dependencies.iter().any(|root| root == "_event")
        })
        .map(|name| {
            let mut record = BTreeMap::from([("name".into(), LuauValue::String(name.into()))]);
            if let Some(data) = event_data {
                record.insert("data".into(), data.clone());
            }
            LuauValue::Record(record)
        });
    bevy_luau::evaluate(
        expression,
        names,
        values,
        event.as_ref().map(|event| ("_event", event)),
    )
}

/// Accept one assignable path rooted in a declared datamodel variable.
pub(crate) fn assignment_root(location: &str) -> Result<String, String> {
    let ast = full_moon::parse(&format!("{location} = nil")).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let mut statements = ast.nodes().stmts();
    let Some(Stmt::Assignment(assignment)) = statements.next() else {
        return Err("assignment location must be a variable or indexed path".into());
    };
    if statements.next().is_some()
        || ast.nodes().last_stmt().is_some()
        || assignment.variables().iter().count() != 1
    {
        return Err("assignment location must be a single path".into());
    }
    let token = match assignment.variables().iter().next().unwrap() {
        Var::Name(name) => name,
        Var::Expression(path)
            if path
                .suffixes()
                .all(|suffix| matches!(suffix, Suffix::Index(_))) =>
        {
            let Prefix::Name(name) = path.prefix() else {
                return Err("assignment path must start with a datamodel name".into());
            };
            name
        }
        _ => return Err("assignment path cannot contain calls".into()),
    };
    match token.token().token_type() {
        TokenType::Identifier { identifier } => Ok(identifier.to_string()),
        _ => Err("assignment path must start with a datamodel name".into()),
    }
}

#[derive(Default)]
struct EventDataVisitor {
    found: bool,
}
impl EventDataVisitor {
    fn inspect<'a>(&mut self, prefix: &Prefix, mut suffixes: impl Iterator<Item = &'a Suffix>) {
        if !self.found && is_event_prefix(prefix) {
            self.found = suffixes.next().is_some_and(is_data_index);
        }
    }
}
impl Visitor for EventDataVisitor {
    fn visit_var_expression(&mut self, expression: &VarExpression) {
        self.inspect(expression.prefix(), expression.suffixes());
    }
    fn visit_function_call(&mut self, expression: &FunctionCall) {
        self.inspect(expression.prefix(), expression.suffixes());
    }
}
fn is_event_prefix(prefix: &Prefix) -> bool {
    match prefix {
        Prefix::Name(name) => is_identifier(name, "_event"),
        Prefix::Expression(expression) => is_event_expression(expression),
        _ => false,
    }
}
fn is_event_expression(expression: &Expression) -> bool {
    match expression {
        Expression::Parentheses { expression, .. }
        | Expression::TypeAssertion { expression, .. } => is_event_expression(expression),
        Expression::Var(Var::Name(name)) => is_identifier(name, "_event"),
        _ => false,
    }
}
fn is_data_index(suffix: &Suffix) -> bool {
    let Suffix::Index(index) = suffix else {
        return false;
    };
    match index {
        Index::Dot { name, .. } => is_identifier(name, "data"),
        Index::Brackets { expression, .. } => match expression {
            Expression::String(value) => match value.token().token_type() {
                TokenType::StringLiteral { literal, .. } => {
                    literal.as_str() == "data" || literal.contains('\\')
                }
                _ => true,
            },
            _ => true,
        },
        _ => false,
    }
}
fn is_identifier(token: &TokenReference, expected: &str) -> bool {
    matches!(token.token().token_type(), TokenType::Identifier { identifier } if identifier.as_str() == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_projection_metadata_tracks_accesses_not_text() {
        for source in [
            "_event.data",
            "_event . data.amount",
            "_event['data']",
            "_event[key]",
            "(_event).data",
            "(_event :: any).data",
            "_event-- comment\n.data",
        ] {
            assert!(
                compile_expression(source, false, 0)
                    .unwrap()
                    .reads_event_data,
                "{source}"
            );
        }
        for source in [
            "'_event.data'",
            "1 -- _event.data\n",
            "other._event.data",
            "_event.name",
            "_event.foo.data",
            "_event['name']",
        ] {
            assert!(
                !compile_expression(source, false, 0)
                    .unwrap()
                    .reads_event_data,
                "{source}"
            );
        }
    }
}
