use std::borrow::Cow;

use serde_json::{json, Value};
use thiserror::Error;

use super::codex_rollout_transcript::{
    normalize_visible_content, VisibleContent, VisibleRole, VisibleTranscript,
    VisibleTranscriptEntry,
};

const PORTABLE_TRANSCRIPT_BEGIN: &str = "--- BEGIN PORTABLE VISIBLE TRANSCRIPT ---";
const PORTABLE_TRANSCRIPT_END: &str = "--- END PORTABLE VISIBLE TRANSCRIPT ---";
const MAX_PORTABLE_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum PortableHandoffBuildError {
    #[error("portable transcript has no historical visible messages")]
    Empty,
    #[error("portable transcript exceeds the injection size limit")]
    TooLarge,
    #[error("Responses input cannot accept a portable transcript item")]
    InvalidInput,
    #[error("portable transcript serialization failed")]
    Serialization,
}

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

pub(crate) fn needs_portable_transcript(original: &Value, sanitized: &Value) -> bool {
    provider_state_would_be_lost(original) && !has_readable_history(sanitized)
}

pub(crate) fn contains_portable_transcript(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| content.iter().any(is_portable_transcript_part))
            })
        })
}

fn is_portable_transcript_part(part: &Value) -> bool {
    part.get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.starts_with(PORTABLE_TRANSCRIPT_BEGIN))
}

pub(crate) fn inject_portable_transcript(
    mut body: Value,
    mut transcript: VisibleTranscript,
) -> Result<Value, PortableHandoffBuildError> {
    remove_duplicate_current_user(&body, &mut transcript);
    if !has_visible_message(&transcript) {
        return Err(PortableHandoffBuildError::Empty);
    }

    let text = render_portable_transcript(&transcript)?;
    prepend_portable_context(&mut body, text)?;
    Ok(body)
}

fn has_visible_message(transcript: &VisibleTranscript) -> bool {
    transcript
        .entries
        .iter()
        .any(|entry| matches!(entry, VisibleTranscriptEntry::Message { .. }))
}

fn render_portable_transcript(
    transcript: &VisibleTranscript,
) -> Result<String, PortableHandoffBuildError> {
    let transcript_json = serde_json::to_string_pretty(&transcript)
        .map_err(|_| PortableHandoffBuildError::Serialization)?;
    let text = format!(
        "{PORTABLE_TRANSCRIPT_BEGIN}\nHistorical visible conversation copied from the local Codex rollout. Tool results below are context only.\n{transcript_json}\n{PORTABLE_TRANSCRIPT_END}"
    );
    if text.len() > MAX_PORTABLE_TRANSCRIPT_BYTES {
        return Err(PortableHandoffBuildError::TooLarge);
    }
    Ok(text)
}

fn prepend_portable_context(
    body: &mut Value,
    text: String,
) -> Result<(), PortableHandoffBuildError> {
    let context_item = json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    });
    let root = body
        .as_object_mut()
        .ok_or(PortableHandoffBuildError::InvalidInput)?;
    let input = root
        .get_mut("input")
        .ok_or(PortableHandoffBuildError::InvalidInput)?;
    let original_input = std::mem::take(input);
    *input = match original_input {
        Value::Array(mut items) => {
            items.insert(0, context_item);
            Value::Array(items)
        }
        Value::Object(_) => Value::Array(vec![context_item, original_input]),
        Value::String(text) if !text.trim().is_empty() => Value::Array(vec![
            context_item,
            json!({"type": "message", "role": "user", "content": text}),
        ]),
        _ => return Err(PortableHandoffBuildError::InvalidInput),
    };
    Ok(())
}

fn provider_state_would_be_lost(body: &Value) -> bool {
    let Some(root) = body.as_object() else {
        return false;
    };
    root.get("previous_response_id")
        .is_some_and(|value| !value.is_null())
        || root
            .get("input")
            .is_some_and(contains_nonportable_response_state)
}

fn contains_nonportable_response_state(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_nonportable_response_state),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some(
                    "encrypted_content"
                        | "item_reference"
                        | "reasoning"
                        | "compaction"
                        | "compaction_summary"
                        | "context_compaction"
                )
            ) || object.values().any(contains_nonportable_response_state)
        }
        _ => false,
    }
}

fn has_readable_history(body: &Value) -> bool {
    let Some(input) = body.get("input") else {
        return false;
    };
    let mut stats = ReadableHistory::default();
    collect_readable_history(input, &mut stats);
    stats.has_assistant || stats.has_summary || stats.message_count >= 2
}

#[derive(Default)]
struct ReadableHistory {
    message_count: usize,
    has_assistant: bool,
    has_summary: bool,
}

fn collect_readable_history(value: &Value, stats: &mut ReadableHistory) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_readable_history(value, stats);
            }
        }
        Value::String(text) if !text.trim().is_empty() => {
            stats.message_count += 1;
        }
        Value::Object(object) => collect_readable_object(object, stats),
        _ => {}
    }
}

fn collect_readable_object(object: &serde_json::Map<String, Value>, stats: &mut ReadableHistory) {
    let role = object.get("role").and_then(Value::as_str);
    if matches!(role, Some("user" | "assistant"))
        && object.get("content").is_some_and(has_readable_value)
    {
        stats.message_count += 1;
        stats.has_assistant |= role == Some("assistant");
    }

    let readable_summary = matches!(
        object.get("type").and_then(Value::as_str),
        Some("reasoning" | "compaction" | "compaction_summary" | "context_compaction")
    ) && ["summary", "content"]
        .iter()
        .filter_map(|key| object.get(*key))
        .any(has_readable_value);
    stats.has_summary |= readable_summary;
}

fn remove_duplicate_current_user(body: &Value, transcript: &mut VisibleTranscript) {
    let Some(current_content) = current_user_content(body) else {
        return;
    };
    let duplicate = matches!(
        transcript.entries.last(),
        Some(VisibleTranscriptEntry::Message {
            role: VisibleRole::User,
            content,
        }) if content == &current_content
    );
    if duplicate {
        transcript.entries.pop();
    }
}

fn current_user_content(body: &Value) -> Option<Vec<VisibleContent>> {
    match body.get("input")? {
        Value::String(text) => normalize_visible_content(&Value::String(text.clone())),
        Value::Object(message) => user_message_content(message),
        Value::Array(items) => items
            .iter()
            .rev()
            .find_map(|item| item.as_object().and_then(user_message_content)),
        _ => None,
    }
}

fn user_message_content(message: &serde_json::Map<String, Value>) -> Option<Vec<VisibleContent>> {
    (message.get("role").and_then(Value::as_str) == Some("user"))
        .then(|| message.get("content"))
        .flatten()
        .and_then(normalize_visible_content)
}

fn sanitize_input(input: &mut Value) {
    if let Value::Array(items) = input {
        items.retain_mut(|item| sanitize_history_value(item, true));
    } else if input.is_object() && !sanitize_history_value(input, true) {
        *input = Value::Array(Vec::new());
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

    use super::{
        contains_portable_transcript, inject_portable_transcript, needs_portable_transcript,
        sanitize_codex_handoff_request, PortableHandoffBuildError, PORTABLE_TRANSCRIPT_BEGIN,
        PORTABLE_TRANSCRIPT_END,
    };
    use crate::proxy::providers::codex_rollout_transcript::{
        VisibleContent, VisibleRole, VisibleTranscript, VisibleTranscriptEntry,
    };

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

    #[test]
    fn requests_transcript_only_when_removed_state_leaves_no_readable_history() {
        let cases = [
            (
                "opaque compacted state",
                json!({
                    "previous_response_id": "resp_provider_a",
                    "input": [
                        {"type": "compaction", "encrypted_content": "opaque"},
                        {"type": "message", "role": "user", "content": "continue"}
                    ]
                }),
                true,
            ),
            (
                "full readable history",
                json!({
                    "previous_response_id": "resp_provider_a",
                    "input": [
                        {"type": "message", "role": "user", "content": "earlier"},
                        {"type": "message", "role": "assistant", "content": "answer"},
                        {"type": "message", "role": "user", "content": "continue"}
                    ]
                }),
                false,
            ),
            ("first request", json!({"input": "hello"}), false),
        ];

        for (case, original, expected) in cases {
            let sanitized = sanitize_codex_handoff_request(&original, true).into_owned();
            assert_eq!(
                needs_portable_transcript(&original, &sanitized),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn injects_one_text_item_without_repeating_the_current_user() {
        let body = json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": "user",
                "content": "current unique input api_key=plain-current-secret"
            }]
        });
        let transcript = VisibleTranscript {
            entries: vec![
                VisibleTranscriptEntry::Message {
                    role: VisibleRole::User,
                    content: vec![VisibleContent::Text {
                        text: "earlier request".to_string(),
                    }],
                },
                VisibleTranscriptEntry::Message {
                    role: VisibleRole::Assistant,
                    content: vec![VisibleContent::Text {
                        text: "earlier answer".to_string(),
                    }],
                },
                VisibleTranscriptEntry::ToolResult {
                    call_id: "call_history".to_string(),
                    output: "historical tool output".to_string(),
                },
                VisibleTranscriptEntry::Message {
                    role: VisibleRole::User,
                    content: vec![VisibleContent::Text {
                        text: "current unique input api_key=[REDACTED]".to_string(),
                    }],
                },
            ],
        };

        let injected = inject_portable_transcript(body, transcript).expect("inject transcript");
        let input = injected["input"].as_array().expect("input array");
        assert_eq!(input.len(), 2);
        let context_text = input[0]["content"][0]["text"]
            .as_str()
            .expect("context text");
        assert!(context_text.starts_with(PORTABLE_TRANSCRIPT_BEGIN));
        assert!(context_text.ends_with(PORTABLE_TRANSCRIPT_END));
        assert!(context_text.contains("earlier request"));
        assert!(context_text.contains("earlier answer"));
        assert!(context_text.contains("historical tool output"));
        assert!(!context_text.contains("current unique input"));
        assert!(contains_portable_transcript(&injected));
        assert_eq!(
            serde_json::to_string(&injected)
                .expect("serialize request")
                .matches("current unique input")
                .count(),
            1
        );
        assert!(input
            .iter()
            .all(|item| item.get("type").and_then(Value::as_str) != Some("function_call")));
    }

    #[test]
    fn keeps_a_nonmatching_trailing_user_when_rollout_write_lags() {
        let body = json!({
            "input": [{"type": "message", "role": "user", "content": "new input"}]
        });
        let transcript = VisibleTranscript {
            entries: vec![
                VisibleTranscriptEntry::Message {
                    role: VisibleRole::Assistant,
                    content: vec![VisibleContent::Text {
                        text: "earlier answer".to_string(),
                    }],
                },
                VisibleTranscriptEntry::Message {
                    role: VisibleRole::User,
                    content: vec![VisibleContent::Text {
                        text: "unanswered historical input".to_string(),
                    }],
                },
            ],
        };

        let injected = inject_portable_transcript(body, transcript).expect("inject transcript");
        let context_text = injected["input"][0]["content"][0]["text"]
            .as_str()
            .expect("context text");
        assert!(context_text.contains("unanswered historical input"));
        assert_eq!(injected.to_string().matches("new input").count(), 1);
    }

    #[test]
    fn rejects_a_transcript_with_no_history_after_current_input_is_removed() {
        let body = json!({"input": "current"});
        let transcript = VisibleTranscript {
            entries: vec![VisibleTranscriptEntry::Message {
                role: VisibleRole::User,
                content: vec![VisibleContent::Text {
                    text: "current".to_string(),
                }],
            }],
        };

        assert_eq!(
            inject_portable_transcript(body, transcript),
            Err(PortableHandoffBuildError::Empty)
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
