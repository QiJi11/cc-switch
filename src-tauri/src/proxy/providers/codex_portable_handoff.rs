use std::borrow::Cow;

use serde_json::Value;

/// Removes response state that cannot be reused after a Codex provider change.
pub(crate) fn sanitize_codex_handoff_request(
    body: &Value,
    provider_boundary: bool,
) -> Cow<'_, Value> {
    if !provider_boundary {
        return Cow::Borrowed(body);
    }

    let mut sanitized = body.clone();
    let Some(root) = sanitized.as_object_mut() else {
        return Cow::Owned(sanitized);
    };

    root.remove("previous_response_id");
    if let Some(input) = root.get_mut("input") {
        sanitize_input(input);
    }

    Cow::Owned(sanitized)
}

fn sanitize_input(input: &mut Value) {
    match input {
        Value::Array(items) => {
            items.retain_mut(|item| sanitize_history_value(item, true));
        }
        Value::Object(_) => {
            if !sanitize_history_value(input, true) {
                *input = Value::Array(Vec::new());
            }
        }
        _ => {}
    }
}

fn sanitize_history_value(value: &mut Value, is_response_item: bool) -> bool {
    match value {
        Value::Array(items) => {
            items.retain_mut(|item| sanitize_history_value(item, is_response_item));
            true
        }
        Value::Object(object) => {
            let item_type = object
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned);

            if is_response_item {
                object.remove("id");
            }
            object.remove("encrypted_content");
            object.retain(|_, child| sanitize_history_value(child, false));

            match item_type.as_deref() {
                Some("encrypted_content" | "item_reference") => false,
                Some("reasoning" | "compaction" | "compaction_summary" | "context_compaction") => {
                    has_readable_summary_or_content(object)
                }
                _ => true,
            }
        }
        _ => true,
    }
}

fn has_readable_summary_or_content(object: &serde_json::Map<String, Value>) -> bool {
    ["summary", "content"]
        .iter()
        .filter_map(|key| object.get(*key))
        .any(has_readable_value)
}

fn has_readable_value(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(values) => values.iter().any(has_readable_value),
        Value::Object(object) => object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "type" | "id" | "status"))
            .any(|(_, value)| has_readable_value(value)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use serde_json::{json, Value};

    use super::sanitize_codex_handoff_request;

    #[test]
    fn sanitizes_provider_state_but_preserves_portable_history() {
        let body = json!({
            "model": "gpt-5",
            "previous_response_id": "resp_provider_a",
            "input": [
                {
                    "type": "message",
                    "id": "msg_provider_a",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Inspect this file"},
                        {"type": "input_file", "file_id": "file_portable", "filename": "notes.txt"}
                    ]
                },
                {
                    "type": "message",
                    "id": "msg_provider_a_assistant",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "I will inspect it."}]
                },
                {
                    "type": "function_call",
                    "id": "fc_provider_a",
                    "call_id": "call_portable",
                    "name": "read_file",
                    "arguments": "{\"path\":\"notes.txt\"}",
                    "status": "completed"
                },
                {
                    "type": "function_call_output",
                    "id": "fco_provider_a",
                    "call_id": "call_portable",
                    "output": "contents",
                    "status": "completed"
                }
            ]
        });

        let sanitized = sanitize_codex_handoff_request(&body, true).into_owned();

        assert!(sanitized.get("previous_response_id").is_none());
        let input = sanitized["input"].as_array().unwrap();
        assert!(input.iter().all(|item| item.get("id").is_none()));
        assert_eq!(input[0]["content"][1]["file_id"], "file_portable");
        assert_eq!(input[2]["call_id"], "call_portable");
        assert_eq!(input[2]["status"], "completed");
        assert_eq!(input[3]["call_id"], "call_portable");
        assert_eq!(input[3]["output"], "contents");
    }

    #[test]
    fn returns_the_original_request_when_there_is_no_provider_boundary() {
        let body = json!({
            "previous_response_id": "resp_same_provider",
            "input": [{
                "type": "reasoning",
                "id": "rs_same_provider",
                "encrypted_content": "opaque"
            }]
        });

        let sanitized = sanitize_codex_handoff_request(&body, false);

        assert!(matches!(sanitized, Cow::Borrowed(_)));
        assert_eq!(sanitized.as_ref(), &body);
        assert_eq!(body["input"][0]["encrypted_content"], "opaque");
    }

    #[test]
    fn removes_nested_encrypted_content_without_losing_visible_content() {
        let body = json!({
            "input": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "Visible answer"},
                        {"type": "encrypted_content", "encrypted_content": "message-secret"}
                    ]
                },
                {
                    "type": "function_call_output",
                    "id": "fco_1",
                    "call_id": "call_1",
                    "output": [
                        {"type": "input_text", "text": "Visible tool result"},
                        {"type": "encrypted_content", "encrypted_content": "tool-secret"}
                    ]
                }
            ]
        });

        let sanitized = sanitize_codex_handoff_request(&body, true).into_owned();

        assert_eq!(
            sanitized["input"][0]["content"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            sanitized["input"][0]["content"][0]["text"],
            "Visible answer"
        );
        assert_eq!(sanitized["input"][1]["output"].as_array().unwrap().len(), 1);
        assert_eq!(
            sanitized["input"][1]["output"][0]["text"],
            "Visible tool result"
        );
        assert!(!contains_key(&sanitized, "encrypted_content"));
    }

    #[test]
    fn drops_encrypted_only_reasoning_compaction_and_item_references() {
        let body = json!({
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_with_summary",
                    "summary": [{"type": "summary_text", "text": "Readable summary"}],
                    "encrypted_content": "reasoning-secret"
                },
                {
                    "type": "reasoning",
                    "id": "rs_encrypted_only",
                    "summary": [],
                    "encrypted_content": "reasoning-secret"
                },
                {
                    "type": "compaction",
                    "id": "cmp_encrypted_only",
                    "encrypted_content": "compaction-secret"
                },
                {
                    "type": "context_compaction",
                    "id": "cmp_with_summary",
                    "summary": "Readable compacted context",
                    "encrypted_content": "compaction-secret"
                },
                {"type": "item_reference", "id": "msg_provider_a"}
            ]
        });

        let sanitized = sanitize_codex_handoff_request(&body, true).into_owned();
        let input = sanitized["input"].as_array().unwrap();

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["summary"][0]["text"], "Readable summary");
        assert_eq!(input[1]["type"], "context_compaction");
        assert_eq!(input[1]["summary"], "Readable compacted context");
        assert!(input.iter().all(|item| item.get("id").is_none()));
        assert!(!contains_key(&sanitized, "encrypted_content"));
    }

    #[test]
    fn tolerates_malformed_payloads_and_nested_input_arrays() {
        let body = json!({
            "previous_response_id": 42,
            "input": [
                null,
                "loose text",
                {"id": 7, "type": 99, "content": {"encrypted_content": "secret"}},
                {"type": "reasoning", "summary": 42, "encrypted_content": ["bad"]},
                [{"type": "message", "id": "nested_id", "content": []}]
            ],
            "metadata": {"id": "client_id", "encrypted_content": "not_response_history"}
        });

        let sanitized = sanitize_codex_handoff_request(&body, true).into_owned();
        let input = sanitized["input"].as_array().unwrap();

        assert_eq!(input.len(), 4);
        assert_eq!(input[0], Value::Null);
        assert_eq!(input[1], "loose text");
        assert!(input[2].get("id").is_none());
        assert!(input[2]["content"].get("encrypted_content").is_none());
        assert!(input[3][0].get("id").is_none());
        assert_eq!(sanitized["metadata"]["id"], "client_id");
        assert_eq!(
            sanitized["metadata"]["encrypted_content"],
            "not_response_history"
        );
    }

    fn contains_key(value: &Value, target: &str) -> bool {
        match value {
            Value::Array(values) => values.iter().any(|value| contains_key(value, target)),
            Value::Object(object) => {
                object.contains_key(target)
                    || object.values().any(|value| contains_key(value, target))
            }
            _ => false,
        }
    }
}
