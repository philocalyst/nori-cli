//! Typed-data embedding, independent of a module's selected output renderer.
use super::*;

impl Evaluator {
    /// Evaluate captured source at its original URI and return all public data.
    /// Unlike the JSON renderer, this preserves explicit null properties so a
    /// policy can distinguish an omitted override from an explicit null value.
    /// Deferred member failures are raised before data reaches the host.
    pub fn eval_source_data(&mut self, source: &str, path: &Path) -> Result<serde_json::Value> {
        self.on_grown_stack(|this| {
            this.begin_evaluation();
            let value = this.eval_source_inner(source, path)?;
            if let Value::Object(_, Some(metadata)) = &value
                && let Some(failure) = metadata
                    .poisoned_members
                    .as_ref()
                    .and_then(|members| members.values().find(|failure| failure.rendered))
            {
                return Err(Error::Eval(failure.message.clone()));
            }
            this.raise_deep_failed(&value)?;
            let result = value.try_to_data_json();
            drop(value);
            this.refs.release();
            result
        })
    }
}
