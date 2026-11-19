//! Null values retain their amendment parent while all ordinary operations
//! continue to see null. Object literal defaults stay unevaluated until used.

use super::*;
use crate::value::NullDefault;

impl Evaluator {
    #[inline(never)]
    pub(super) fn null_default(
        &mut self,
        arg: &Expr,
        scope: &Scope,
        depth: usize,
    ) -> Result<Value> {
        // Select the branch now, but preserve laziness of its object members.
        if let Expr::If(cond, then_expr, else_expr) = arg {
            let condition = self.eval_expr(cond, scope, depth + 1)?;
            let branch = if expect_bool(&condition)? {
                then_expr
            } else {
                else_expr
            };
            return self.null_default(branch, scope, depth + 1);
        }
        if matches!(arg, Expr::New(None, ..)) {
            return Err(Error::Eval(
                "Cannot instantiate type `Object | Function<Object>`.".into(),
            ));
        }
        if matches!(arg, Expr::New(Some(_), ..)) {
            let thunk = Expr::Lambda(Arc::default(), Arc::new(arg.clone()));
            let thunk = self.eval_expr(&thunk, scope, depth + 1)?;
            return Ok(Value::NullDefault(Arc::new(NullDefault::Deferred(thunk))));
        }
        let value = self.eval_expr(arg, scope, depth + 1)?;
        let valid = match &value {
            Value::Object(_, Some(source)) => {
                !is_class_value(&value)
                    && source.type_identity.as_deref() != Some("pkl:base#Pair")
                    && source.kind != ObjectKind::Reference
            }
            Value::Object(_, None) => false,
            Value::Lambda(..) => true,
            Value::List(items) => matches!(items.kind(), ListKind::Dynamic | ListKind::Listing),
            _ => false,
        };
        if !valid {
            return Err(Error::Eval(format!(
                "Expected value of type `Object | Function<Object>`, but got type `{}`.\nValue: {}",
                pkl_class_name(&value),
                render::display_value(&value)
            )));
        }
        Ok(Value::NullDefault(Arc::new(NullDefault::Value(value))))
    }

    pub(super) fn null_amendment_parent(
        &mut self,
        value: Value,
        body: &Body,
        scope: &Scope,
        depth: usize,
    ) -> Result<Value> {
        let parent = match value {
            Value::Null => empty_mapping_or_listing("Dynamic").expect("Dynamic has a default"),
            Value::NullDefault(default) => match &*default {
                NullDefault::TypeDefault(value) | NullDefault::Value(value) => value.clone(),
                NullDefault::DeferredClass {
                    class_name,
                    type_namespace,
                    class_value,
                } => self.instantiate_deferred_class(
                    class_name,
                    type_namespace.as_deref(),
                    class_value.as_deref(),
                    scope,
                    depth,
                )?,
                NullDefault::Deferred(thunk) => {
                    self.apply_function(thunk, &[], scope, depth + 1)?
                }
            },
            value => return Ok(value),
        };
        if matches!(&parent, Value::Object(_, Some(source)) if source.kind == ObjectKind::Mapping)
            && body.iter().any(|entry| matches!(entry, Entry::Property(prop)
                if prop.name.as_ref() != "default" && !has_modifier(&prop.modifiers, Modifier::Local)))
        {
            return Err(Error::Eval(
                "Object of type `Mapping` cannot have a property (other than `default`).".into()
            ));
        }
        // A default does not give an instance permission to add undeclared
        // properties to a closed class. Keep that restriction when the parent
        // was hidden behind a null value.
        if let Value::Object(map, Some(source)) = &parent
            && source.type_name.is_some()
            && !source.is_open
            && matches!(source.kind, ObjectKind::Object | ObjectKind::Class)
        {
            for entry in body.iter() {
                let name = match entry {
                    Entry::Property(prop) if !has_modifier(&prop.modifiers, Modifier::Local) => {
                        Some(prop.name.as_ref())
                    }
                    Entry::DynProperty(Expr::String(name), _) => Some(name.as_ref()),
                    _ => None,
                };
                if let Some(name) = name
                    && !map.contains_key(name)
                    && !source.entries.iter().any(|entry| {
                        matches!(entry, Entry::Property(prop) if prop.name.as_ref() == name)
                    })
                {
                    return Err(Error::Eval(format!(
                        "Cannot find property `{name}` in object of type `{}`.",
                        pkl_class_name(&parent)
                    )));
                }
            }
        }
        Ok(parent)
    }
    /// Dispatch a parameterized amendment without losing its literal body kind.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_parameterized_amendment(
        &mut self,
        expr: &Expr,
        arity: i64,
        lambda: &Expr,
        body: &Body,
        scope: &Scope,
        depth: usize,
    ) -> Result<Value> {
        let parent = self.eval_expr(expr, scope, depth + 1)?;
        let parent = self.null_amendment_parent(parent, body, scope, depth)?;
        if let Value::Lambda(params, ..) = &parent {
            if params.len() as i64 != arity {
                return Err(Error::Eval(format!(
                    "Expected a function with {arity} parameters, but got {}.",
                    params.len()
                )));
            }
            let mut closure_scope = scope.child();
            closure_scope.set(parser::FUNCTION_AMEND_BINDING, parent);
            return self.eval_expr(lambda, &closure_scope, depth + 1);
        }
        let kind = pkl_class_name(&parent);
        if kind == "Dynamic"
            && !body.iter().any(|entry| {
                matches!(
                    entry,
                    Entry::ForGenerator(_)
                        | Entry::WhenGenerator(_)
                        | Entry::Spread(_)
                        | Entry::Predicate(..)
                )
            })
        {
            return self.eval_value_amendment(parent, body, scope, depth);
        }
        let kind = match kind.as_str() {
            "Listing" | "Mapping" => format!("a `{kind}`"),
            _ => "an object".to_string(),
        };
        Err(Error::Eval(format!(
            "Expected a function that returns {kind}, but got {kind}."
        )))
    }

    pub(super) fn resolve_deferred_class(
        &self,
        class_name: &str,
        type_namespace: Option<&str>,
        class_value: Option<&Value>,
        scope: &Scope,
    ) -> Option<Value> {
        if let Some(value) = class_value {
            return Some(value.clone());
        }
        if let Some(namespace) = type_namespace {
            if let Some(snapshot) = self.module_scopes.get(Path::new(namespace))
                && let Some(value) = snapshot.values.get(class_name)
                && is_class_value(value)
            {
                return Some(value.clone());
            }
            if let Some(live) = self.live_module_classes.get(namespace)
                && let Some(value) = live.get(class_name)
                && is_class_value(value)
            {
                return Some(value.clone());
            }
        }
        resolve_type_value(scope, class_name)
    }

    pub(super) fn instantiate_deferred_class(
        &mut self,
        class_name: &str,
        type_namespace: Option<&str>,
        class_value: Option<&Value>,
        scope: &Scope,
        depth: usize,
    ) -> Result<Value> {
        let base = self.resolve_deferred_class(class_name, type_namespace, class_value, scope);
        let check_name = match &base {
            Some(Value::Object(_, Some(source))) => {
                source.type_name.as_deref().unwrap_or(class_name)
            }
            _ => class_name,
        };
        check_instantiable(scope, Some(check_name), base.as_ref())?;
        let Some(Value::Object(ref base_map, Some(ref base_src))) = base else {
            return Ok(empty_mapping_or_listing("Dynamic").expect("Dynamic has a default"));
        };
        let is_open = base_src.is_open;
        let mut result = self.eval_amended_object(base_map, base_src, &[], scope, depth + 1)?;
        if let Value::Object(_, ref mut src_slot) = result {
            let tn = base_src
                .type_name
                .clone()
                .or_else(|| Some(class_name.to_string()));
            if let Some(src) = src_slot.take() {
                let mut s = Arc::unwrap_or_clone(src);
                s.is_open = is_open;
                s.type_name = tn;
                s.display_module = base_src.display_module.clone();
                s.parent_type_names = base_src.parent_type_names.clone();
                *src_slot = Some(Arc::new(s));
            }
        }
        Ok(result)
    }
}
