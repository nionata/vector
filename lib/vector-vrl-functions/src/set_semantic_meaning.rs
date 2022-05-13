use std::ops::{Deref, DerefMut};

use ::value::Value;
use lookup::LookupBuf;
use vrl::prelude::*;

pub struct MeaningList(pub BTreeMap<String, LookupBuf>);

impl Deref for MeaningList {
    type Target = BTreeMap<String, LookupBuf>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MeaningList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SetSemanticMeaning;

impl Function for SetSemanticMeaning {
    fn identifier(&self) -> &'static str {
        "set_semantic_meaning"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[
            Parameter {
                keyword: "target",
                kind: kind::ANY,
                required: true,
            },
            Parameter {
                keyword: "meaning",
                kind: kind::BYTES,
                required: true,
            },
        ]
    }

    fn examples(&self) -> &'static [Example] {
        &[Example {
            title: "Sets custom field semantic meaning",
            source: r#"set_semantic_meaning(.foo, "bar")"#,
            result: Ok("null"),
        }]
    }

    fn compile(
        &self,
        _state: (&mut state::LocalEnv, &mut state::ExternalEnv),
        ctx: &mut FunctionCompileContext,
        mut arguments: ArgumentList,
    ) -> Compiled {
        let query = arguments.required_query("target")?;

        let meaning = arguments
            .required_literal("meaning")?
            .to_value()
            .try_bytes_utf8_lossy()
            .expect("meaning not bytes")
            .into_owned();

        if !query.is_external() {
            return Err(Box::new(ExpressionError::from(format!(
                "meaning must be set on an external field: {}",
                query
            ))) as Box<dyn DiagnosticMessage>);
        }

        if let Some(list) = ctx.get_external_context_mut::<MeaningList>() {
            list.insert(meaning, query.path().clone());
        };

        Ok(Box::new(SetSemanticMeaningFn))
    }

    fn call_by_vm(&self, _: &mut Context, _: &mut VmArgumentList) -> Result<Value> {
        Ok(Value::Null)
    }
}

#[derive(Debug, Clone)]
struct SetSemanticMeaningFn;

impl Expression for SetSemanticMeaningFn {
    fn resolve<'value, 'ctx: 'value, 'rt: 'ctx>(
        &'rt self,
        _: &'ctx mut Context,
    ) -> Resolved<'value> {
        Ok(Value::Null.into())
    }

    fn type_def(&self, _: (&state::LocalEnv, &state::ExternalEnv)) -> TypeDef {
        TypeDef::null().infallible()
    }
}
