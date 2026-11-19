//! Select an explicit Core authentication path without changing providers or billing.
use nori_acp_kit::AgentError;
use std::collections::HashMap;
use std::ffi::OsString;

pub(crate) fn select(
    environment: &HashMap<OsString, OsString>,
) -> Result<&'static str, AgentError> {
    let get = |key: &str| {
        environment
            .get(&OsString::from(key))
            .cloned()
            .or_else(|| std::env::var_os(key))
    };
    if let Some(mode) = get("NORI_GEMINI_AUTH") {
        return match mode.to_str() {
            Some("gemini") => Ok("gemini"),
            Some("vertex") => Ok("vertex"),
            _ => Err(AgentError::invalid(
                "Native Gemini auth must be gemini or vertex; use the official Gemini CLI for Google account login",
            )),
        };
    }
    if get("GOOGLE_GENAI_USE_GCA").is_some_and(|value| value == "true") {
        return Err(AgentError::AuthRequired(
            "Use the official Gemini CLI for Google account authentication".to_owned(),
        ));
    }
    if get("GOOGLE_GENAI_USE_VERTEXAI").is_some_and(|value| value == "true") {
        return Ok("vertex");
    }
    if get("GEMINI_API_KEY").is_some_and(|value| !value.is_empty()) {
        return Ok("gemini");
    }
    Err(AgentError::AuthRequired(
        "Select Gemini API or Vertex authentication, or use the official Gemini CLI".to_owned(),
    ))
}
