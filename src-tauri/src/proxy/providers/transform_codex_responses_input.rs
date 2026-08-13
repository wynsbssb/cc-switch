//! Reactive sanitizer for Codex `/responses` input items whose `content`
//! array is rejected by strict Responses upstreams.
//!
//! The Codex client persists a conversation as Responses `input` items and
//! replays the full history on every turn. History written by one provider can
//! carry a non-empty `content` array that the next provider's schema rejects:
//!
//! - `reasoning` items from gateways that emit plaintext chain-of-thought
//!   (`content: [{type: "reasoning_text", ...}]`);
//! - legacy / old-format `function_call` / `function_call_output` items that
//!   older Codex builds serialized with a `content` field.
//!
//! The strict Responses schema caps `content` on these item types at an empty
//! array, so replaying such history fails with:
//!
//! ```text
//! Invalid 'input[x].content': array too long. Expected an array with maximum
//! length 0, but got an array with length 1 instead.
//! ```
//!
//! (openai/codex#36551, openai/codex#36704). cc-switch applies two layers:
//!
//! 1. **Proactive** (all native Responses upstreams except vendors that
//!    require reasoning-content round-trips, see
//!    `provider_requires_reasoning_content_roundtrip`): normalize the arrays
//!    before the request leaves the proxy, so the invalid shape never reaches
//!    a strict upstream regardless of how that upstream words its rejection.
//! 2. **Reactive** (any provider, including the reasoning-vendor family): if
//!    an upstream still rejects with the content-array signature, the
//!    forwarder sanitizes and retries once. This also covers gateways whose
//!    rejection text the proactive skip could not predict.
//!
//! The reasoning-vendor skip is deliberate: DeepSeek's native Responses API
//! requires `reasoning_text` to be passed back verbatim for thinking-mode
//! continuation, so those arrays must survive on providers that accept them.

use crate::proxy::error::ProxyError;
use serde_json::Value;

/// Item types whose `content` field the strict Responses schema caps at an
/// empty array. `message` items are intentionally excluded: their `content`
/// arrays are valid and must be preserved.
const EMPTY_CONTENT_ITEM_TYPES: &[&str] = &["reasoning", "function_call", "function_call_output"];

/// Whether an upstream error is the "content array too long" rejection this
/// sanitizer can fix.
pub(crate) fn is_content_array_too_long_error(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError { status, body } = error else {
        return false;
    };

    if !matches!(*status, 400 | 422) {
        return false;
    }

    let Some(body) = body.as_deref() else {
        return false;
    };

    let message = extract_error_text(body).to_ascii_lowercase();
    const HINTS: &[&str] = &[
        "array too long",
        "array_above_max_length",
        "maximum length 0",
        "expected an array with maximum length",
    ];
    HINTS.iter().any(|hint| message.contains(hint))
}

/// Normalize `content` on history item types that strict Responses schemas
/// only accept with an empty array: non-empty arrays become `[]`, everything
/// else (strings, null, missing, already-empty arrays, `message` items) is
/// left untouched. Returns the number of items fixed. Deterministic and
/// idempotent.
pub(crate) fn sanitize_input_content_arrays(body: &mut Value) -> usize {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };

    let mut fixed = 0;
    for item in input.iter_mut() {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !EMPTY_CONTENT_ITEM_TYPES.contains(&item_type) {
            continue;
        }
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let Some(content) = obj.get_mut("content") else {
            continue;
        };
        let is_non_empty_array = content.as_array().is_some_and(|parts| !parts.is_empty());
        if !is_non_empty_array {
            continue;
        }
        *content = Value::Array(Vec::new());
        fixed += 1;
    }
    fixed
}

/// Best-effort extraction of the human-readable message from an upstream error
/// body (mirrors `media_sanitizer::extract_error_text`).
fn extract_error_text(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let candidates = [
            value.pointer("/error/message"),
            value.pointer("/message"),
            value.pointer("/detail"),
            value.pointer("/error"),
        ];
        if let Some(message) = candidates
            .into_iter()
            .flatten()
            .find_map(|value| value.as_str())
        {
            return message.to_string();
        }

        if let Ok(compact) = serde_json::to_string(&value) {
            return compact;
        }
    }

    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upstream_error(status: u16, message: &str) -> ProxyError {
        ProxyError::UpstreamError {
            status,
            body: Some(json!({ "error": { "message": message } }).to_string()),
        }
    }

    #[test]
    fn matches_exact_field_error_text() {
        let error = upstream_error(
            400,
            "Invalid 'input[5].content': array too long. Expected an array with maximum length 0, but got an array with length 1 instead.",
        );
        assert!(is_content_array_too_long_error(&error));
    }

    #[test]
    fn matches_array_above_max_length_shape() {
        let error = upstream_error(
            400,
            "[ArrayParam] [input[5].content] [array_above_max_length] Invalid 'input[5].content': array too long. Expected an array with maximum length 0, but got an array with length 1 instead.",
        );
        assert!(is_content_array_too_long_error(&error));
    }

    #[test]
    fn ignores_unrelated_upstream_errors() {
        for (status, message) in [
            (400, "Invalid 'input[0].content': string too long"),
            (400, "maximum context length exceeded"),
            (500, "Invalid 'input[5].content': array too long"),
            (401, "Invalid 'input[5].content': array too long"),
        ] {
            assert!(!is_content_array_too_long_error(&upstream_error(
                status, message
            )));
        }
    }

    #[test]
    fn normalizes_reasoning_and_function_call_content() {
        let mut body = json!({
            "input": [
                {"type": "reasoning", "id": "r1", "content": [{"type": "reasoning_text", "text": "think"}]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}", "content": [{"type": "x"}]},
                {"type": "function_call_output", "call_id": "c1", "output": "ok", "content": [{"type": "y"}]}
            ]
        });
        assert_eq!(sanitize_input_content_arrays(&mut body), 3);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], json!([]));
        assert_eq!(input[1]["content"], json!([]));
        assert_eq!(input[2]["content"], json!([]));
    }

    #[test]
    fn leaves_messages_and_clean_items_untouched() {
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "id": "r2", "content": []},
                {"type": "reasoning", "id": "r3", "content": null},
                {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"}
            ]
        });
        let before = body.clone();
        assert_eq!(sanitize_input_content_arrays(&mut body), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn noop_without_input_array() {
        let mut body = json!({ "model": "gpt-5.6-sol" });
        assert_eq!(sanitize_input_content_arrays(&mut body), 0);
        assert_eq!(body, json!({ "model": "gpt-5.6-sol" }));
    }

    #[test]
    fn idempotent_second_pass() {
        let mut body = json!({
            "input": [
                {"type": "reasoning", "id": "r1", "content": [{"type": "reasoning_text", "text": "think"}]}
            ]
        });
        assert_eq!(sanitize_input_content_arrays(&mut body), 1);
        assert_eq!(sanitize_input_content_arrays(&mut body), 0);
    }
}
