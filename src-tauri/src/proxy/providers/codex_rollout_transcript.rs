use crate::codex_config::get_codex_config_dir;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

const MAX_ROLLOUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SCAN_DEPTH: usize = 4;
const REDACTED: &str = "[REDACTED]";

static SENSITIVE_ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([\"']?(?:authorization|api[_ -]?key|private[_ -]?key|token|access[_ -]?token|refresh[_ -]?token|auth[_ -]?token|client[_ -]?secret|password|passphrase|secret|credentials?|cookie)[\"']?\s*[:=]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}\]]+)"#,
    )
    .expect("valid sensitive assignment regex")
});
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("valid bearer regex")
});
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})\b")
        .expect("valid token regex")
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VisibleTranscript {
    pub entries: Vec<VisibleTranscriptEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum VisibleTranscriptEntry {
    Message {
        role: VisibleRole,
        content: Vec<VisibleContent>,
    },
    ToolResult {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VisibleRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum VisibleContent {
    Text { text: String },
    Attachment { attachment: AttachmentReference },
}

pub(crate) fn normalize_visible_content(value: &Value) -> Option<Vec<VisibleContent>> {
    let content: Vec<VisibleContent> = match value {
        Value::String(text) => nonempty_text(text).into_iter().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(normalize_visible_content_item)
            .collect(),
        _ => return None,
    };
    (!content.is_empty()).then_some(content)
}

fn normalize_visible_content_item(item: &Value) -> Option<VisibleContent> {
    let object = item.as_object()?;
    if object.contains_key("encrypted_content") {
        return None;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => object
            .get("text")
            .and_then(Value::as_str)
            .and_then(nonempty_text),
        Some("input_image" | "image" | "image_url" | "input_file" | "file" | "attachment") => {
            extract_attachment(object).map(|attachment| VisibleContent::Attachment { attachment })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AttachmentReference {
    pub kind: String,
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum RolloutTranscriptError {
    #[error("invalid Codex session UUID: {value}")]
    InvalidSessionId { value: String },
    #[error("Codex home is unavailable: {path}")]
    CodexHomeUnavailable { path: PathBuf },
    #[error("no rollout found for Codex session {session_id}")]
    Missing { session_id: String },
    #[error("multiple rollouts found for Codex session {session_id}: {count}")]
    Ambiguous { session_id: String, count: usize },
    #[error("rollout path escapes the Codex session root: {path}")]
    PathEscapesRoot { path: PathBuf },
    #[error("Codex session tree exceeds the supported scan depth at {path}")]
    ScanDepthExceeded { path: PathBuf },
    #[error("rollout exceeds the {max_bytes}-byte limit: {path} ({size} bytes)")]
    TooLarge {
        path: PathBuf,
        size: u64,
        max_bytes: u64,
    },
    #[error("malformed rollout JSONL at {path}, line {line}")]
    Malformed { path: PathBuf, line: usize },
    #[error("rollout session UUID mismatch at {path}: expected {expected}, found {found}")]
    SessionMismatch {
        path: PathBuf,
        expected: String,
        found: String,
    },
    #[error("failed to {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
struct RolloutLocation {
    root: PathBuf,
    path: PathBuf,
}

struct RolloutSearch<'a> {
    root: &'a Path,
    session_id: &'a str,
    candidates: Vec<RolloutLocation>,
}

impl<'a> RolloutSearch<'a> {
    fn new(root: &'a Path, session_id: &'a str) -> Self {
        Self {
            root,
            session_id,
            candidates: Vec::new(),
        }
    }

    fn scan_directory(
        &mut self,
        directory: &Path,
        depth: usize,
    ) -> Result<(), RolloutTranscriptError> {
        let entries =
            std::fs::read_dir(directory).map_err(|source| RolloutTranscriptError::Io {
                operation: "scan Codex session directory",
                path: directory.to_path_buf(),
                source,
            })?;
        for entry in entries {
            let entry = entry.map_err(|source| RolloutTranscriptError::Io {
                operation: "read Codex session directory entry",
                path: directory.to_path_buf(),
                source,
            })?;
            self.inspect_entry(entry, depth)?;
        }
        Ok(())
    }

    fn inspect_entry(
        &mut self,
        entry: std::fs::DirEntry,
        depth: usize,
    ) -> Result<(), RolloutTranscriptError> {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| RolloutTranscriptError::Io {
                operation: "inspect Codex session directory entry",
                path: path.clone(),
                source,
            })?;

        if file_type.is_symlink() {
            if rollout_filename_matches(&path, self.session_id) {
                return Err(RolloutTranscriptError::PathEscapesRoot { path });
            }
            return Ok(());
        }
        if file_type.is_dir() {
            return self.scan_subdirectory(&path, depth);
        }
        if file_type.is_file() && rollout_filename_matches(&path, self.session_id) {
            self.add_candidate(path)?;
        }
        Ok(())
    }

    fn scan_subdirectory(
        &mut self,
        path: &Path,
        depth: usize,
    ) -> Result<(), RolloutTranscriptError> {
        if depth >= MAX_SCAN_DEPTH {
            return Err(RolloutTranscriptError::ScanDepthExceeded {
                path: path.to_path_buf(),
            });
        }
        self.scan_directory(path, depth + 1)
    }

    fn add_candidate(&mut self, path: PathBuf) -> Result<(), RolloutTranscriptError> {
        let canonical_path = canonicalize_with_context(&path, "canonicalize rollout")?;
        if !canonical_path.starts_with(self.root) {
            return Err(RolloutTranscriptError::PathEscapesRoot { path });
        }
        self.candidates.push(RolloutLocation {
            root: self.root.to_path_buf(),
            path: canonical_path,
        });
        Ok(())
    }
}

/// Read a portable, user-visible transcript from the active Codex home.
pub(crate) fn read_visible_transcript(
    session_id: &str,
) -> Result<VisibleTranscript, RolloutTranscriptError> {
    read_visible_transcript_from_home(&get_codex_config_dir(), session_id)
}

fn read_visible_transcript_from_home(
    codex_home: &Path,
    session_id: &str,
) -> Result<VisibleTranscript, RolloutTranscriptError> {
    let session_uuid =
        Uuid::parse_str(session_id).map_err(|_| RolloutTranscriptError::InvalidSessionId {
            value: session_id.to_string(),
        })?;
    let location = locate_rollout(codex_home, session_uuid)?;
    read_rollout_file(&location.root, &location.path, session_uuid)
}

fn locate_rollout(
    codex_home: &Path,
    session_uuid: Uuid,
) -> Result<RolloutLocation, RolloutTranscriptError> {
    let canonical_home = std::fs::canonicalize(codex_home).map_err(|_| {
        RolloutTranscriptError::CodexHomeUnavailable {
            path: codex_home.to_path_buf(),
        }
    })?;
    let session_id = session_uuid.to_string();
    let mut candidates = Vec::new();

    for root in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        if !root.exists() {
            continue;
        }

        let canonical_root = canonicalize_with_context(&root, "canonicalize session root")?;
        if !canonical_root.starts_with(&canonical_home) {
            return Err(RolloutTranscriptError::PathEscapesRoot { path: root });
        }

        let mut search = RolloutSearch::new(&canonical_root, &session_id);
        search.scan_directory(&canonical_root, 0)?;
        candidates.append(&mut search.candidates);
    }

    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    match candidates.len() {
        0 => Err(RolloutTranscriptError::Missing { session_id }),
        1 => Ok(candidates.remove(0)),
        count => Err(RolloutTranscriptError::Ambiguous { session_id, count }),
    }
}

fn rollout_filename_matches(path: &Path, session_id: &str) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let lower = file_name.to_ascii_lowercase();
    let suffix = format!("{session_id}.jsonl");
    lower
        .strip_suffix(&suffix)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('-'))
}

fn read_rollout_file(
    root: &Path,
    path: &Path,
    expected_session: Uuid,
) -> Result<VisibleTranscript, RolloutTranscriptError> {
    let canonical_path = validated_rollout_path(root, path)?;
    let contents = read_bounded_rollout(&canonical_path)?;
    parse_rollout(&canonical_path, &contents, expected_session)
}

fn validated_rollout_path(root: &Path, path: &Path) -> Result<PathBuf, RolloutTranscriptError> {
    let canonical_root = canonicalize_with_context(root, "canonicalize session root")?;
    let canonical_path = canonicalize_with_context(path, "canonicalize rollout")?;
    canonical_path
        .starts_with(&canonical_root)
        .then_some(canonical_path)
        .ok_or_else(|| RolloutTranscriptError::PathEscapesRoot {
            path: path.to_path_buf(),
        })
}

fn read_bounded_rollout(path: &Path) -> Result<String, RolloutTranscriptError> {
    let file = File::open(path).map_err(|source| RolloutTranscriptError::Io {
        operation: "open rollout",
        path: path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| RolloutTranscriptError::Io {
            operation: "inspect rollout",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if size > MAX_ROLLOUT_BYTES {
        return Err(RolloutTranscriptError::TooLarge {
            path: path.to_path_buf(),
            size,
            max_bytes: MAX_ROLLOUT_BYTES,
        });
    }

    let mut contents = String::new();
    BufReader::new(file)
        .take(MAX_ROLLOUT_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|source| RolloutTranscriptError::Io {
            operation: "read rollout",
            path: path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 > MAX_ROLLOUT_BYTES {
        return Err(RolloutTranscriptError::TooLarge {
            path: path.to_path_buf(),
            size: contents.len() as u64,
            max_bytes: MAX_ROLLOUT_BYTES,
        });
    }
    Ok(contents)
}

fn parse_rollout(
    path: &Path,
    contents: &str,
    expected_session: Uuid,
) -> Result<VisibleTranscript, RolloutTranscriptError> {
    let mut parser = TranscriptParser::new(expected_session);
    for (index, line) in contents.lines().enumerate() {
        parser.read_line(path, index + 1, line)?;
    }
    parser.finish(path)
}

struct TranscriptParser {
    expected_session: Uuid,
    saw_session_meta: bool,
    entries: Vec<VisibleTranscriptEntry>,
}

impl TranscriptParser {
    fn new(expected_session: Uuid) -> Self {
        Self {
            expected_session,
            saw_session_meta: false,
            entries: Vec::new(),
        }
    }

    fn read_line(
        &mut self,
        path: &Path,
        line_number: usize,
        line: &str,
    ) -> Result<(), RolloutTranscriptError> {
        if line.trim().is_empty() {
            return Err(malformed(path, line_number));
        }
        let line_value: Value =
            serde_json::from_str(line).map_err(|_| malformed(path, line_number))?;
        let line_object = line_value
            .as_object()
            .ok_or_else(|| malformed(path, line_number))?;

        match line_object.get("type").and_then(Value::as_str) {
            Some("session_meta") => self.read_session_meta(path, line_number, line_object),
            Some("response_item") => {
                let payload = rollout_payload(path, line_number, line_object)?;
                if let Some(entry) = visible_response_item(payload, path, line_number)? {
                    self.entries.push(entry);
                }
                Ok(())
            }
            // These records are not user-visible and may contain internal instructions.
            Some(_) => Ok(()),
            None => Err(malformed(path, line_number)),
        }
    }

    fn read_session_meta(
        &mut self,
        path: &Path,
        line: usize,
        line_object: &serde_json::Map<String, Value>,
    ) -> Result<(), RolloutTranscriptError> {
        let payload = rollout_payload(path, line, line_object)?;
        let raw_id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(path, line))?;
        let found = Uuid::parse_str(raw_id).map_err(|_| malformed(path, line))?;
        if found != self.expected_session {
            return Err(RolloutTranscriptError::SessionMismatch {
                path: path.to_path_buf(),
                expected: self.expected_session.to_string(),
                found: found.to_string(),
            });
        }
        self.saw_session_meta = true;
        Ok(())
    }

    fn finish(self, path: &Path) -> Result<VisibleTranscript, RolloutTranscriptError> {
        if !self.saw_session_meta {
            return Err(malformed(path, 1));
        }
        Ok(VisibleTranscript {
            entries: self.entries,
        })
    }
}

fn rollout_payload<'a>(
    path: &Path,
    line: usize,
    line_object: &'a serde_json::Map<String, Value>,
) -> Result<&'a serde_json::Map<String, Value>, RolloutTranscriptError> {
    line_object
        .get("payload")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(path, line))
}

fn visible_response_item(
    payload: &serde_json::Map<String, Value>,
    path: &Path,
    line: usize,
) -> Result<Option<VisibleTranscriptEntry>, RolloutTranscriptError> {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => visible_message(payload, path, line),
        Some("function_call_output" | "custom_tool_call_output") => {
            visible_tool_result(payload, path, line)
        }
        // Calls, reasoning, compaction, and agent-only messages are intentionally excluded.
        Some(_) => Ok(None),
        None => Err(malformed(path, line)),
    }
}

fn visible_message(
    payload: &serde_json::Map<String, Value>,
    path: &Path,
    line: usize,
) -> Result<Option<VisibleTranscriptEntry>, RolloutTranscriptError> {
    let role = match payload.get("role").and_then(Value::as_str) {
        Some("user") => VisibleRole::User,
        Some("assistant") => VisibleRole::Assistant,
        Some(_) => return Ok(None),
        None => return Err(malformed(path, line)),
    };
    let raw_content = payload
        .get("content")
        .ok_or_else(|| malformed(path, line))?;
    let content = extract_visible_content(raw_content, path, line)?;

    if content.is_empty() || (role == VisibleRole::User && is_synthetic_user_message(&content)) {
        return Ok(None);
    }
    Ok(Some(VisibleTranscriptEntry::Message { role, content }))
}

fn visible_tool_result(
    payload: &serde_json::Map<String, Value>,
    path: &Path,
    line: usize,
) -> Result<Option<VisibleTranscriptEntry>, RolloutTranscriptError> {
    if payload
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| !matches!(status, "completed" | "success"))
    {
        return Ok(None);
    }

    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| malformed(path, line))?;
    let output = payload.get("output").ok_or_else(|| malformed(path, line))?;
    let output = extract_tool_output(output);
    if output.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(VisibleTranscriptEntry::ToolResult {
        call_id: call_id.to_string(),
        output,
    }))
}

fn extract_visible_content(
    value: &Value,
    path: &Path,
    line: usize,
) -> Result<Vec<VisibleContent>, RolloutTranscriptError> {
    match value {
        Value::String(text) => Ok(nonempty_text(text).into_iter().collect()),
        Value::Array(items) => {
            let mut content = Vec::new();
            for item in items {
                if let Some(visible_item) = extract_content_item(item, path, line)? {
                    content.push(visible_item);
                }
            }
            Ok(content)
        }
        _ => Err(malformed(path, line)),
    }
}

fn extract_content_item(
    item: &Value,
    path: &Path,
    line: usize,
) -> Result<Option<VisibleContent>, RolloutTranscriptError> {
    let object = item.as_object().ok_or_else(|| malformed(path, line))?;
    if object.contains_key("encrypted_content") {
        return Ok(None);
    }
    match object.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => {
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| malformed(path, line))?;
            Ok(nonempty_text(text))
        }
        Some("input_image" | "image" | "image_url" | "input_file" | "file" | "attachment") => {
            Ok(extract_attachment(object)
                .map(|attachment| VisibleContent::Attachment { attachment }))
        }
        Some(_) => Ok(None),
        None => Err(malformed(path, line)),
    }
}

fn nonempty_text(text: &str) -> Option<VisibleContent> {
    let text = sanitize_text(text);
    (!text.trim().is_empty()).then_some(VisibleContent::Text { text })
}

fn extract_attachment(object: &serde_json::Map<String, Value>) -> Option<AttachmentReference> {
    let kind = object.get("type")?.as_str()?.to_string();
    let (field, raw_reference) = ["file_id", "image_url", "url", "path"]
        .into_iter()
        .find_map(|field| object.get(field)?.as_str().map(|value| (field, value)))?;
    let reference = sanitize_attachment_reference(field, raw_reference);
    if reference.trim().is_empty() {
        return None;
    }
    let name = object
        .get("filename")
        .or_else(|| object.get("name"))
        .and_then(Value::as_str)
        .map(sanitize_text)
        .filter(|value| !value.trim().is_empty());
    Some(AttachmentReference {
        kind,
        reference,
        name,
    })
}

fn sanitize_attachment_reference(field: &str, value: &str) -> String {
    if value.starts_with("data:") {
        return "[embedded attachment]".to_string();
    }
    if matches!(field, "image_url" | "url") {
        if let Ok(mut url) = Url::parse(value) {
            url.set_query(None);
            url.set_fragment(None);
            return sanitize_text(url.as_str());
        }
    }
    sanitize_text(value)
}

fn extract_tool_output(value: &Value) -> String {
    match value {
        Value::String(text) => sanitize_text(text),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                if object.contains_key("encrypted_content") {
                    return None;
                }
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(sanitize_text),
                    _ => None,
                }
            })
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_synthetic_user_message(content: &[VisibleContent]) -> bool {
    let Some(VisibleContent::Text { text }) = content.first() else {
        return false;
    };
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<skills_instructions>")
}

fn sanitize_text(text: &str) -> String {
    let structured = redact_structured_json(text).unwrap_or_else(|| text.to_string());
    let bearer = BEARER_RE.replace_all(&structured, REDACTED);
    let known_tokens = TOKEN_RE.replace_all(&bearer, REDACTED);
    SENSITIVE_ASSIGNMENT_RE
        .replace_all(&known_tokens, "${1}[REDACTED]")
        .into_owned()
}

fn redact_structured_json(text: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    if !matches!(value, Value::Object(_) | Value::Array(_)) {
        return None;
    }
    redact_sensitive_json_fields(&mut value);
    serde_json::to_string(&value).ok()
}

fn redact_sensitive_json_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_sensitive_key(key) {
                    *value = Value::String(REDACTED.to_string());
                } else {
                    redact_sensitive_json_fields(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_sensitive_json_fields(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "authorization"
            | "apikey"
            | "privatekey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "authtoken"
            | "clientsecret"
            | "password"
            | "passphrase"
            | "secret"
            | "credential"
            | "credentials"
            | "cookie"
            | "setcookie"
            | "encryptedcontent"
    )
}

fn canonicalize_with_context(
    path: &Path,
    operation: &'static str,
) -> Result<PathBuf, RolloutTranscriptError> {
    std::fs::canonicalize(path).map_err(|source| RolloutTranscriptError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

fn malformed(path: &Path, line: usize) -> RolloutTranscriptError {
    RolloutTranscriptError::Malformed {
        path: path.to_path_buf(),
        line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::tempdir;

    const SESSION_ID: &str = "019cc369-bd7c-7891-b371-7b20b4fe0b18";
    const OTHER_SESSION_ID: &str = "019cc369-bd7c-7891-b371-7b20b4fe0b19";

    fn rollout_path(home: &Path, root: &str, session_id: &str) -> PathBuf {
        let directory = if root == "sessions" {
            home.join(root).join("2026").join("07").join("17")
        } else {
            home.join(root)
        };
        std::fs::create_dir_all(&directory).expect("create rollout directory");
        directory.join(format!("rollout-2026-07-17T04-08-43-{session_id}.jsonl"))
    }

    fn write_rollout(path: &Path, values: &[Value]) -> String {
        let contents = values
            .iter()
            .map(|value| serde_json::to_string(value).expect("serialize fixture"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, &contents).expect("write rollout fixture");
        contents
    }

    fn session_meta(session_id: &str) -> Value {
        json!({
            "timestamp": "2026-07-17T04:08:43Z",
            "type": "session_meta",
            "payload": { "id": session_id, "cwd": "C:/workspace" }
        })
    }

    #[test]
    fn reads_visible_messages_attachments_and_completed_tool_results() {
        let temp = tempdir().expect("tempdir");
        let path = rollout_path(temp.path(), "sessions", SESSION_ID);
        let original = write_rollout(
            &path,
            &[
                session_meta(SESSION_ID),
                json!({"type":"response_item","payload":{"type":"message","role":"developer","content":"system secret"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":"# AGENTS.md instructions\n<INSTRUCTIONS>hidden</INSTRUCTIONS>"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                    {"type":"input_text","text":"Inspect this. api_key=sk-user-secretvalue Authorization: Bearer bearer-secretvalue"},
                    {"type":"input_image","image_url":"https://files.example/image.png?token=raw-secret"}
                ]}}),
                json!({"type":"response_item","payload":{"type":"reasoning","encrypted_content":"opaque-reasoning-secret","summary":[{"text":"internal thought"}]}}),
                json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"{\"result\":\"ok\",\"api_key\":\"sk-tool-secretvalue\",\"token\":\"plain-tool-token\"}"}}),
                json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_2","status":"in_progress","output":"not complete"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}),
            ],
        );

        let transcript =
            read_visible_transcript_from_home(temp.path(), SESSION_ID).expect("read transcript");

        assert_eq!(transcript.entries.len(), 3);
        let serialized = serde_json::to_string(&transcript).expect("serialize transcript");
        assert!(serialized.contains("Inspect this."));
        assert!(serialized.contains("https://files.example/image.png"));
        assert!(serialized.contains("call_1"));
        assert!(serialized.contains("Done."));
        assert!(serialized.contains(REDACTED));
        for excluded in [
            "system secret",
            "hidden",
            "sk-user-secretvalue",
            "bearer-secretvalue",
            "raw-secret",
            "opaque-reasoning-secret",
            "internal thought",
            "sk-tool-secretvalue",
            "plain-tool-token",
            "not complete",
        ] {
            assert!(!serialized.contains(excluded), "leaked {excluded}");
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("reread rollout"),
            original
        );
    }

    #[test]
    fn compacted_fixture_keeps_visible_chronology_without_internal_summary() {
        let temp = tempdir().expect("tempdir");
        let path = rollout_path(temp.path(), "sessions", SESSION_ID);
        write_rollout(
            &path,
            &[
                session_meta(SESSION_ID),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":"before"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"answer"}}),
                json!({"type":"compacted","payload":{"message":"internal compacted summary with password=secret-value"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":"after"}}),
            ],
        );

        let transcript =
            read_visible_transcript_from_home(temp.path(), SESSION_ID).expect("read transcript");
        let serialized = serde_json::to_string(&transcript).expect("serialize transcript");

        assert_eq!(transcript.entries.len(), 3);
        let before = serialized.find("before").expect("before entry");
        let answer = serialized.find("answer").expect("answer entry");
        let after = serialized.find("after").expect("after entry");
        assert!(before < answer && answer < after);
        assert!(!serialized.contains("compacted summary"));
        assert!(!serialized.contains("secret-value"));
    }

    #[test]
    fn malformed_fixture_returns_typed_error() {
        let temp = tempdir().expect("tempdir");
        let path = rollout_path(temp.path(), "sessions", SESSION_ID);
        let mut file = File::create(&path).expect("create fixture");
        writeln!(file, "{}", session_meta(SESSION_ID)).expect("write metadata");
        writeln!(file, "not-json").expect("write malformed line");

        let error = read_visible_transcript_from_home(temp.path(), SESSION_ID)
            .expect_err("malformed rollout should fail");
        assert!(matches!(
            error,
            RolloutTranscriptError::Malformed { line: 2, .. }
        ));
    }

    #[test]
    fn missing_fixture_returns_typed_error() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join("sessions")).expect("create sessions root");

        let error = read_visible_transcript_from_home(temp.path(), SESSION_ID)
            .expect_err("missing rollout should fail");
        assert!(matches!(error, RolloutTranscriptError::Missing { .. }));
    }

    #[test]
    fn path_like_session_id_is_rejected_before_file_lookup() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join("sessions")).expect("create sessions root");

        let invalid = read_visible_transcript_from_home(temp.path(), "../../auth.json")
            .expect_err("path-like session id should fail");
        assert!(matches!(
            invalid,
            RolloutTranscriptError::InvalidSessionId { .. }
        ));
    }

    #[test]
    fn rollout_outside_session_root_is_rejected() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join("sessions")).expect("create sessions root");

        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, "{}\n").expect("write outside fixture");
        let error = read_rollout_file(
            &temp.path().join("sessions"),
            &outside,
            Uuid::parse_str(SESSION_ID).expect("session UUID"),
        )
        .expect_err("outside rollout should fail");
        assert!(matches!(
            error,
            RolloutTranscriptError::PathEscapesRoot { .. }
        ));
    }

    #[test]
    fn duplicate_exact_session_rollouts_are_ambiguous() {
        let temp = tempdir().expect("tempdir");
        let active = rollout_path(temp.path(), "sessions", SESSION_ID);
        let archived = rollout_path(temp.path(), "archived_sessions", SESSION_ID);
        write_rollout(&active, &[session_meta(SESSION_ID)]);
        write_rollout(&archived, &[session_meta(SESSION_ID)]);

        let error = read_visible_transcript_from_home(temp.path(), SESSION_ID)
            .expect_err("duplicate rollout should fail");
        assert!(matches!(
            error,
            RolloutTranscriptError::Ambiguous { count: 2, .. }
        ));
    }

    #[test]
    fn only_the_exact_session_uuid_matches_a_rollout() {
        let temp = tempdir().expect("tempdir");
        let other = rollout_path(temp.path(), "sessions", OTHER_SESSION_ID);
        write_rollout(&other, &[session_meta(OTHER_SESSION_ID)]);
        let missing = read_visible_transcript_from_home(temp.path(), SESSION_ID)
            .expect_err("different UUID should not match");
        assert!(matches!(missing, RolloutTranscriptError::Missing { .. }));
    }

    #[test]
    fn oversized_rollout_is_rejected_before_parsing() {
        let temp = tempdir().expect("tempdir");
        let oversized = rollout_path(temp.path(), "sessions", SESSION_ID);
        let file = File::create(&oversized).expect("create oversized fixture");
        file.set_len(MAX_ROLLOUT_BYTES + 1)
            .expect("extend oversized fixture");
        let error = read_visible_transcript_from_home(temp.path(), SESSION_ID)
            .expect_err("oversized rollout should fail");
        assert!(matches!(error, RolloutTranscriptError::TooLarge { .. }));
    }
}
