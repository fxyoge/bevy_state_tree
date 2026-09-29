use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashSet},
    rc::Rc,
};

use mlua::{ChunkMode, Compiler, Error, Lua, Table, Value};

/// A portable value stored in one Luau state-tree datamodel.
///
/// Values never retain handles into a Luau VM, so state-tree instances may move
/// freely between Bevy worker threads.
#[derive(Clone, Debug, PartialEq)]
pub enum LuauValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(Box<str>),
    Sequence(Vec<LuauValue>),
    Record(BTreeMap<Box<str>, LuauValue>),
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledLuauExpression {
    pub bytecode: Box<[u8]>,
    pub span_start: usize,
    pub reads_event_data: bool,
}

pub(crate) fn compile_expression(
    expression: &str,
    boolean: bool,
    span_start: usize,
) -> Result<CompiledLuauExpression, Error> {
    let source = if boolean {
        format!("return not not ({expression})")
    } else {
        format!("return ({expression})")
    };
    let bytecode = Compiler::new().compile(source)?;
    Ok(CompiledLuauExpression {
        bytecode: bytecode.into_boxed_slice(),
        span_start,
        reads_event_data: expression.contains("_event.data"),
    })
}

thread_local! {
    static WORKER: RefCell<LuauWorker> = RefCell::new(LuauWorker::new());
}

pub(crate) fn evaluate(
    expression: &CompiledLuauExpression,
    names: &[Box<str>],
    values: &[LuauValue],
    event_name: Option<&str>,
    event_data: Option<&LuauValue>,
) -> Result<LuauValue, Error> {
    WORKER.with_borrow(|worker| worker.evaluate(expression, names, values, event_name, event_data))
}

struct LuauWorker {
    lua: Lua,
    interrupts: Rc<Cell<usize>>,
}

impl LuauWorker {
    fn new() -> Self {
        let lua = Lua::new();
        lua.sandbox(true)
            .expect("Luau sandbox initialization failed");
        lua.set_memory_limit(64 * 1024 * 1024)
            .expect("Luau memory limit initialization failed");
        let interrupts = Rc::new(Cell::new(0));
        let callback_interrupts = Rc::clone(&interrupts);
        lua.set_interrupt(move |_| {
            let count = callback_interrupts.get() + 1;
            callback_interrupts.set(count);
            if count > 10_000 {
                return Err(Error::runtime("Luau execution limit exceeded"));
            }
            Ok(mlua::VmState::Continue)
        });
        Self { lua, interrupts }
    }

    fn evaluate(
        &self,
        expression: &CompiledLuauExpression,
        names: &[Box<str>],
        values: &[LuauValue],
        event_name: Option<&str>,
        event_data: Option<&LuauValue>,
    ) -> Result<LuauValue, Error> {
        self.interrupts.set(0);
        let environment = self.lua.create_table()?;
        let metatable = self.lua.create_table()?;
        let declared: HashSet<Box<str>> = names.iter().cloned().collect();
        let globals = self.lua.globals();
        metatable.set(
            "__index",
            self.lua
                .create_function(move |_, (_, key): (Table, Value)| {
                    if let Value::String(name) = &key
                        && declared.contains(name.to_str()?.as_ref())
                    {
                        return Ok(Value::Nil);
                    }
                    globals.raw_get::<Value>(key)
                })?,
        )?;
        environment.set_metatable(Some(metatable))?;

        for (name, value) in names.iter().zip(values) {
            environment.set(name.as_ref(), to_lua(&self.lua, value)?)?;
        }
        if let Some(name) = event_name {
            let event = self.lua.create_table()?;
            event.set("name", name)?;
            if let Some(data) = event_data {
                event.set("data", to_lua(&self.lua, data)?)?;
            }
            event.set_readonly(true);
            environment.set("_event", event)?;
        }

        let value = self
            .lua
            .load(expression.bytecode.as_ref())
            .set_mode(ChunkMode::Binary)
            .set_environment(environment)
            .eval()?;
        from_lua(value, &mut Conversion::new(), 0)
    }
}

fn to_lua(lua: &Lua, value: &LuauValue) -> Result<Value, Error> {
    Ok(match value {
        LuauValue::Nil => Value::Nil,
        LuauValue::Boolean(value) => Value::Boolean(*value),
        LuauValue::Integer(value) => Value::Integer(*value),
        LuauValue::Number(value) => Value::Number(*value),
        LuauValue::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        LuauValue::Sequence(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
        LuauValue::Record(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.set(key.as_ref(), to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
    })
}

struct Conversion {
    seen_tables: HashSet<usize>,
    remaining_values: usize,
}

impl Conversion {
    fn new() -> Self {
        Self {
            seen_tables: HashSet::new(),
            remaining_values: 100_000,
        }
    }

    fn consume(&mut self) -> Result<(), Error> {
        self.remaining_values = self
            .remaining_values
            .checked_sub(1)
            .ok_or_else(|| Error::runtime("Luau value exceeds the persistence size limit"))?;
        Ok(())
    }
}

fn from_lua(value: Value, conversion: &mut Conversion, depth: usize) -> Result<LuauValue, Error> {
    conversion.consume()?;
    if depth == 64 {
        return Err(Error::runtime("Luau value nesting exceeds 64 levels"));
    }
    Ok(match value {
        Value::Nil => LuauValue::Nil,
        Value::Boolean(value) => LuauValue::Boolean(value),
        Value::Integer(value) => LuauValue::Integer(value),
        Value::Number(value) => LuauValue::Number(value),
        Value::String(value) => {
            let value = value.to_str()?;
            LuauValue::String(Box::from(value.as_ref()))
        }
        Value::Table(table) => from_table(table, conversion, depth + 1)?,
        other => {
            return Err(Error::runtime(format!(
                "Luau value type `{}` cannot be persisted",
                other.type_name()
            )));
        }
    })
}

fn from_table(table: Table, conversion: &mut Conversion, depth: usize) -> Result<LuauValue, Error> {
    if !conversion.seen_tables.insert(table.to_pointer() as usize) {
        return Err(Error::runtime(
            "shared or cyclic Luau tables cannot be persisted",
        ));
    }
    let mut sequence = BTreeMap::new();
    let mut record = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => {
                sequence.insert(index as usize, from_lua(value, conversion, depth)?);
            }
            Value::String(key) => {
                let key = key.to_str()?;
                record.insert(Box::from(key.as_ref()), from_lua(value, conversion, depth)?);
            }
            _ => {
                return Err(Error::runtime(
                    "Luau tables require string or positive integer keys",
                ));
            }
        }
    }
    if !sequence.is_empty() && !record.is_empty() {
        return Err(Error::runtime(
            "mixed sequence and record Luau tables cannot be persisted",
        ));
    }
    if !record.is_empty() {
        return Ok(LuauValue::Record(record));
    }
    let expected: Vec<_> = (1..=sequence.len()).collect();
    if sequence.keys().copied().collect::<Vec<_>>() != expected {
        return Err(Error::runtime("sparse Luau sequences cannot be persisted"));
    }
    Ok(LuauValue::Sequence(sequence.into_values().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_portable_sequences_and_records() {
        let expression =
            compile_expression("{ values = { 1, 2, 3 }, enabled = true }", false, 0).unwrap();

        let value = evaluate(&expression, &[], &[], None, None).unwrap();

        assert_eq!(
            value,
            LuauValue::Record(BTreeMap::from([
                (Box::from("enabled"), LuauValue::Boolean(true)),
                (
                    Box::from("values"),
                    LuauValue::Sequence(vec![
                        LuauValue::Integer(1),
                        LuauValue::Integer(2),
                        LuauValue::Integer(3),
                    ]),
                ),
            ]))
        );
    }

    #[test]
    fn evaluation_environments_do_not_share_authored_globals() {
        let mutate = compile_expression(
            "(function() accidental_global = 42 return accidental_global end)()",
            false,
            0,
        )
        .unwrap();
        let inspect = compile_expression("accidental_global", false, 0).unwrap();

        assert_eq!(
            evaluate(&mutate, &[], &[], None, None).unwrap(),
            LuauValue::Integer(42)
        );
        assert_eq!(
            evaluate(&inspect, &[], &[], None, None).unwrap(),
            LuauValue::Nil
        );
    }

    #[test]
    fn declared_nil_values_shadow_sandbox_globals() {
        let expression = compile_expression("math", false, 0).unwrap();

        assert_eq!(
            evaluate(
                &expression,
                &[Box::from("math")],
                &[LuauValue::Nil],
                None,
                None,
            )
            .unwrap(),
            LuauValue::Nil
        );
    }

    #[test]
    fn interrupts_unbounded_expressions() {
        let expression =
            compile_expression("(function() while true do end return true end)()", false, 0)
                .unwrap();

        let error = evaluate(&expression, &[], &[], None, None).unwrap_err();

        assert!(error.to_string().contains("Luau execution limit exceeded"));
    }

    #[test]
    fn rejects_shared_table_graphs_without_expanding_them() {
        let expression = compile_expression(
            "(function() local shared = { value = 1 } return { a = shared, b = shared } end)()",
            false,
            0,
        )
        .unwrap();

        let error = evaluate(&expression, &[], &[], None, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("shared or cyclic Luau tables cannot be persisted")
        );
    }
}
