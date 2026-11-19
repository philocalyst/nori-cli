//! Message parser for converting JSON to typed messages

use crate::errors::MessageParseError;
use crate::errors::Result;
use crate::types::messages::Message;
use serde::Deserialize;

/// Message parser for CLI output
pub struct MessageParser;

impl MessageParser {
    /// Parse a JSON value into a Message. A message of a type this SDK does not know is kept as
    /// [`Message::Unrecognized`]; only a known type that does not read is an error.
    pub fn parse(data: serde_json::Value) -> Result<Message> {
        let known = data
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| Message::KNOWN_TYPES.contains(&kind));
        if !known {
            return Ok(Message::Unrecognized(data));
        }
        Message::deserialize(&data).map_err(|e| {
            MessageParseError::new(
                format!("Failed to parse message: {e}"),
                Some(data), // Preserve unrecognized event payloads for callers.
            )
            .into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MessageParser;
    use crate::errors::ClaudeError;
    use crate::types::messages::Message;

    #[test]
    fn parses_rate_limit_event_without_losing_subscription_window_metadata() {
        let payload = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "rateLimitType": "five_hour",
                "resetsAt": 1_800_000_000,
                "overageStatus": "rejected"
            },
            "uuid": "evt-1"
        });

        let Message::RateLimitEvent(event) = MessageParser::parse(payload.clone()).unwrap() else {
            panic!("expected rate-limit event");
        };

        assert_eq!(
            event,
            serde_json::json!({
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "five_hour",
                    "resetsAt": 1_800_000_000,
                    "overageStatus": "rejected"
                },
                "uuid": "evt-1"
            })
        );
    }

    #[test]
    fn parses_result_costs_with_arbitrary_precision_enabled() {
        for (cost, expected) in [
            ("0.0042", Some(0.0042)),
            ("1e-3", Some(0.001)),
            ("3", Some(3.0)),
            ("null", None),
        ] {
            let json = format!(
                r#"{{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s","total_cost_usd":{cost}}}"#
            );
            let value = serde_json::from_str(&json).unwrap();
            let Message::Result(result) = MessageParser::parse(value).unwrap() else {
                panic!("expected result message");
            };
            assert_eq!(result.total_cost_usd, expected);
        }

        let invalid = serde_json::json!({
            "type": "result", "subtype": "success", "duration_ms": 1,
            "duration_api_ms": 1, "is_error": false, "num_turns": 1,
            "session_id": "s", "total_cost_usd": "0.0042"
        });
        assert!(MessageParser::parse(invalid).is_err());
    }

    #[test]
    fn keeps_an_unknown_message_type_whole() {
        let payload = serde_json::json!({"type": "tool_progress", "elapsed_time_seconds": 3});

        let Message::Unrecognized(kept) = MessageParser::parse(payload.clone()).unwrap() else {
            panic!("expected an unrecognized message");
        };

        assert_eq!(kept, payload);
    }
}
