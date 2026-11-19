//! Keep the class of a module snapshot when it leaves its internal scope.

use super::*;

/// The evaluator updates source-less `this`/`module` maps while building a
/// module. A value exposed to a property, collection or closure must retain
/// the module class, rather than becoming indistinguishable from a Map.
///
/// Match the map and class marker at the same scope level. A `let` binding or
/// parameter named `module` has no marker of its own, so it cannot turn an
/// arbitrary map into a module value.
pub(super) fn exposed_module_value(value: &Value, scope: &Scope) -> Option<Value> {
    let Value::Object(members, None) = value else {
        return None;
    };
    let mut level = Some(scope);
    while let Some(current) = level {
        if let Some(Value::Object(module, None)) = current.vars.get("module")
            && Arc::ptr_eq(members, module)
            && let Some(Value::Object(_, Some(class))) =
                current.vars.get(super::module::MODULE_CLASS_IDENTITY)
        {
            return Some(Value::Object(Arc::clone(members), Some(Arc::clone(class))));
        }
        level = current.parent.as_deref();
    }
    None
}
