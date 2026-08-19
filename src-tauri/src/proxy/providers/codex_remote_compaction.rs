use super::codex_responses_sse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bytes::Bytes;
use serde_json::{json, Value};

const ENCRYPTED_PREFIX: &str = "ccswitch-remote-compaction-v1:";
const COMPACTION_PROMPT: &str = "Create a concise but complete handoff summary of the conversation so far. Preserve the user's goals, constraints, decisions, important technical details, file paths, commands, errors, and remaining work. Return only the summary; do not continue the task.";
const RESTORED_CONTEXT_PREFIX: &str = "The following is a compacted summary of the earlier conversation. Treat it as authoritative context:\n\n";

pub(crate) fn is_remote_compaction_request(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
}

pub(crate) fn compaction_prompt() -> &'static str {
    COMPACTION_PROMPT
}

pub(crate) fn restored_compaction_context(item: &Value) -> Option<String> {
    let encrypted = item.get("encrypted_content")?.as_str()?;
    let encoded = encrypted.strip_prefix(ENCRYPTED_PREFIX)?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let summary = String::from_utf8(decoded).ok()?;
    (!summary.trim().is_empty()).then(|| format!("{RESTORED_CONTEXT_PREFIX}{summary}"))
}

pub(crate) fn convert_response_to_compaction(mut response: Value) -> Value {
    let summary = response_summary_text(&response);
    let encrypted_content = format!(
        "{ENCRYPTED_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(summary.as_bytes())
    );
    response["output"] = json!([{
        "type": "compaction",
        "encrypted_content": encrypted_content
    }]);
    response["status"] = json!("completed");
    response
}

pub(crate) fn compaction_sse_events(response: &Value) -> Vec<Bytes> {
    let item = response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "type": "compaction",
                "encrypted_content": format!("{ENCRYPTED_PREFIX}{}", URL_SAFE_NO_PAD.encode(b""))
            })
        });

    vec![
        codex_responses_sse::response_created(response),
        codex_responses_sse::response_in_progress(response),
        codex_responses_sse::output_item_added(0, &item),
        codex_responses_sse::output_item_done(0, &item),
        codex_responses_sse::response_completed(response),
    ]
}

fn response_summary_text(response: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    for key in ["text", "refusal"] {
                        if let Some(text) = part.get(key).and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                parts.push(text.trim().to_string());
                            }
                        }
                    }
                }
            }
            if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                for part in summary {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            parts.push(text.trim().to_string());
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        "No textual summary was returned by the upstream model.".to_string()
    } else {
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_transforms_forward_trigger_as_summary_prompt() {
        let request = json!({
            "model": "test-model",
            "max_output_tokens": 1024,
            "tools": [{
                "type": "function",
                "name": "should_not_run",
                "parameters": {"type": "object", "properties": {}}
            }],
            "tool_choice": "required",
            "parallel_tool_calls": false,
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
                {"type": "compaction_trigger"}
            ]
        });

        let chat =
            super::super::transform_codex_chat::responses_to_chat_completions(request.clone())
                .unwrap();
        assert!(chat["messages"].as_array().unwrap().iter().any(|message| {
            message.get("content").and_then(Value::as_str) == Some(COMPACTION_PROMPT)
        }));
        assert!(chat.get("tools").is_none());
        assert!(chat.get("tool_choice").is_none());
        assert!(chat.get("parallel_tool_calls").is_none());

        let anthropic =
            super::super::transform_codex_anthropic::responses_request_to_anthropic(request, 1024)
                .unwrap();
        assert!(anthropic["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .any(|part| part.get("text").and_then(Value::as_str) == Some(COMPACTION_PROMPT)));
        assert!(anthropic.get("tools").is_none());
        assert!(anthropic.get("tool_choice").is_none());
    }

    #[test]
    fn recognizes_v2_trigger_and_collapses_three_outputs_to_one_compaction() {
        let request = json!({
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
                {"type": "compaction_trigger"}
            ]
        });
        assert!(is_remote_compaction_request(&request));

        let response = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "reason"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "summary"}]},
                {"type": "function_call", "name": "ignored", "arguments": "{}"}
            ]
        });
        let compacted = convert_response_to_compaction(response);
        let output = compacted["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "compaction");
        assert!(restored_compaction_context(&output[0])
            .unwrap()
            .contains("summary"));

        let events = compaction_sse_events(&compacted);
        let done_count = events
            .iter()
            .filter(|event| {
                String::from_utf8_lossy(event).contains("event: response.output_item.done")
            })
            .count();
        assert_eq!(done_count, 1);
    }
}
