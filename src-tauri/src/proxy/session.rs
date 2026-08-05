//! Proxy Session - 请求会话管理
//!
//! 为每个代理请求创建会话上下文，在整个请求生命周期中跟踪状态和元数据。
//!
//! ## Session ID 提取
//!
//! 支持从客户端请求中提取 Session ID，用于关联同一对话的多个请求：
//! - Claude: 从 `metadata.user_id` (格式: `user_xxx_session_yyy`) 或 `metadata.session_id` 提取
//! - Codex: 从 headers、`metadata.session_id` 或 Codex Desktop `client_metadata` 提取
//! - 其他: 生成新的 UUID

use axum::http::HeaderMap;
use std::time::Instant;
use uuid::Uuid;

/// 客户端请求格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ClientFormat {
    /// Claude Messages API (/v1/messages)
    Claude,
    /// Codex Response API (/v1/responses)
    Codex,
    /// OpenAI Chat Completions API (/v1/chat/completions)
    OpenAI,
    /// Gemini API (/v1beta/models/*/generateContent)
    Gemini,
    /// Gemini CLI API (/v1internal/models/*/generateContent)
    GeminiCli,
    /// 未知格式
    Unknown,
}

#[allow(dead_code)]
impl ClientFormat {
    /// 从请求路径检测格式
    pub fn from_path(path: &str) -> Self {
        if path.contains("/v1/messages") {
            ClientFormat::Claude
        } else if path.contains("/v1/responses") {
            ClientFormat::Codex
        } else if path.contains("/v1/chat/completions") {
            ClientFormat::OpenAI
        } else if path.contains("/v1internal/") && path.contains("generateContent") {
            // Gemini CLI 使用 /v1internal/ 路径
            ClientFormat::GeminiCli
        } else if (path.contains("/v1beta/") || path.contains("/v1/"))
            && path.contains("generateContent")
        {
            // Gemini API 使用 /v1beta/ 或 /v1/ 路径
            ClientFormat::Gemini
        } else if path.contains("generateContent") {
            // 通用 Gemini 端点
            ClientFormat::Gemini
        } else {
            ClientFormat::Unknown
        }
    }

    /// 从请求体内容检测格式（回退方案）
    pub fn from_body(body: &serde_json::Value) -> Self {
        // Claude 格式特征: messages 数组 + model 字段 + 无 response_format
        if body.get("messages").is_some()
            && body.get("model").is_some()
            && body.get("response_format").is_none()
            && body.get("contents").is_none()
        {
            // 区分 Claude 和 OpenAI
            if body.get("max_tokens").is_some() {
                return ClientFormat::Claude;
            }
            return ClientFormat::OpenAI;
        }

        // Codex 格式特征: input 字段
        if body.get("input").is_some() {
            return ClientFormat::Codex;
        }

        // Gemini 格式特征: contents 数组
        if body.get("contents").is_some() {
            return ClientFormat::Gemini;
        }

        ClientFormat::Unknown
    }

    /// 转换为字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientFormat::Claude => "claude",
            ClientFormat::Codex => "codex",
            ClientFormat::OpenAI => "openai",
            ClientFormat::Gemini => "gemini",
            ClientFormat::GeminiCli => "gemini_cli",
            ClientFormat::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ClientFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 代理会话
///
/// 包含请求全生命周期的上下文数据
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProxySession {
    /// 唯一会话 ID
    pub session_id: String,
    /// 请求开始时间
    pub start_time: Instant,
    /// HTTP 方法
    pub method: String,
    /// 请求 URL
    pub request_url: String,
    /// User-Agent
    pub user_agent: Option<String>,
    /// 客户端请求格式
    pub client_format: ClientFormat,
    /// 选定的供应商 ID
    pub provider_id: Option<String>,
    /// 模型名称
    pub model: Option<String>,
    /// 是否为流式请求
    pub is_streaming: bool,
}

#[allow(dead_code)]
impl ProxySession {
    /// 从请求创建会话
    pub fn from_request(
        method: &str,
        request_url: &str,
        user_agent: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Self {
        // 检测客户端格式
        let mut client_format = ClientFormat::from_path(request_url);
        if client_format == ClientFormat::Unknown {
            if let Some(body) = body {
                client_format = ClientFormat::from_body(body);
            }
        }

        // 检测是否为流式请求
        let is_streaming = body
            .and_then(|b| b.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 提取模型名称
        let model = body
            .and_then(|b| b.get("model"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Self {
            session_id: Uuid::new_v4().to_string(),
            start_time: Instant::now(),
            method: method.to_string(),
            request_url: request_url.to_string(),
            user_agent: user_agent.map(|s| s.to_string()),
            client_format,
            provider_id: None,
            model,
            is_streaming,
        }
    }

    /// 设置供应商 ID
    pub fn with_provider(mut self, provider_id: &str) -> Self {
        self.provider_id = Some(provider_id.to_string());
        self
    }

    /// 获取请求延迟（毫秒）
    pub fn latency_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }
}

// ============================================================================
// Session ID 提取器
// ============================================================================

/// Session ID 来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdSource {
    /// 从 metadata.user_id 提取 (Claude)
    MetadataUserId,
    /// 从 metadata.session_id 提取
    MetadataSessionId,
    /// 从 Codex Desktop client_metadata 提取
    ClientMetadataSessionId,
    /// 从 headers 提取 (Codex)
    Header,
    /// 新生成
    Generated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdentityError {
    InvalidCodexIdentity,
}

/// Session ID 提取结果
#[derive(Debug, Clone)]
pub struct SessionIdResult {
    /// 提取或生成的 Session ID
    pub session_id: String,
    /// Session ID 来源
    pub source: SessionIdSource,
    /// 是否为客户端提供的 ID（非新生成）
    pub client_provided: bool,
}

/// 从请求中提取或生成 Session ID
///
/// 轻量化实现，仅提取 session_id 用于日志记录，不做复杂的 Session 管理。
///
/// ## 提取优先级
///
/// ### Claude 请求
/// 1. `metadata.user_id` (格式: `user_xxx_session_yyy`) → 提取 `yyy` 部分
/// 2. `metadata.session_id` → 直接使用
/// 3. 生成新 UUID
///
/// ### Codex 请求
/// 1. `client_metadata.session_id` / `client_metadata.thread_id`
/// 2. Headers: `session_id` 或 `x-session-id`
/// 3. `metadata.session_id`
/// 4. 生成新 UUID
///
/// ## 示例
///
/// ```ignore
/// let result = extract_session_id(&headers, &body, "claude").expect("valid identity");
/// println!("Session ID: {} (from {:?})", result.session_id, result.source);
/// ```
pub fn extract_session_id(
    headers: &HeaderMap,
    body: &serde_json::Value,
    client_format: &str,
) -> Result<SessionIdResult, SessionIdentityError> {
    if client_format == "claude" {
        if let Some(result) = extract_claude_session(headers, body) {
            return Ok(result);
        }
    }

    // Codex 请求特殊处理
    if client_format == "codex" || client_format == "openai" {
        if let Some(result) = extract_codex_session(headers, body)? {
            return Ok(result);
        }
    }

    // Claude 请求：从 metadata 提取
    if let Some(result) = extract_from_metadata(body) {
        return Ok(result);
    }

    // 兜底：生成新 Session ID
    Ok(generate_new_session_id())
}

/// 提取 Claude Session ID
fn extract_claude_session(
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<SessionIdResult> {
    for header_name in &["x-claude-code-session-id", "claude-code-session-id"] {
        if let Some(value) = headers.get(*header_name) {
            if let Ok(session_id) = value.to_str() {
                if !session_id.is_empty() {
                    return Some(SessionIdResult {
                        session_id: session_id.to_string(),
                        source: SessionIdSource::Header,
                        client_provided: true,
                    });
                }
            }
        }
    }

    extract_from_metadata(body)
}

/// 提取 Codex Session ID
fn extract_codex_session(
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Result<Option<SessionIdResult>, SessionIdentityError> {
    let client_identity = extract_codex_client_metadata(body)?;
    let legacy_identity = extract_legacy_codex_identity(headers, body)?;

    if let Some(session_id) = client_identity.session_id {
        validate_legacy_codex_identity(legacy_identity.as_ref(), &session_id)?;
        return Ok(Some(SessionIdResult {
            session_id: format!("codex_{session_id}"),
            source: SessionIdSource::ClientMetadataSessionId,
            client_provided: true,
        }));
    }

    if let Some((session_id, source)) = legacy_identity {
        return Ok(Some(SessionIdResult {
            session_id: format!("codex_{session_id}"),
            source,
            client_provided: true,
        }));
    }

    if let Some(thread_id) = client_identity.thread_id {
        return Ok(Some(SessionIdResult {
            session_id: format!("codex_{thread_id}"),
            source: SessionIdSource::ClientMetadataSessionId,
            client_provided: true,
        }));
    }

    // previous_response_id 是 Responses 协议里的响应游标，不是稳定会话身份。
    // Chat/Responses 桥接时该值通常来自上游每轮返回的随机 response id；
    // 若把它当 prompt_cache_key 或 Codex session header，会导致每轮请求换缓存 key。

    Ok(None)
}

fn extract_legacy_codex_identity(
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Result<Option<(String, SessionIdSource)>, SessionIdentityError> {
    let mut identity = None;

    for header_name in &["session_id", "x-session-id"] {
        if let Some(value) = headers.get(*header_name) {
            let value = value
                .to_str()
                .map_err(|_| SessionIdentityError::InvalidCodexIdentity)?;
            let candidate = (
                canonical_codex_uuid(value).ok_or(SessionIdentityError::InvalidCodexIdentity)?,
                SessionIdSource::Header,
            );
            identity = merge_codex_identity(identity, candidate)?;
        }
    }

    if let Some(value) = body.get("metadata").and_then(|m| m.get("session_id")) {
        let value = value
            .as_str()
            .ok_or(SessionIdentityError::InvalidCodexIdentity)?;
        let candidate = (
            canonical_codex_uuid(value).ok_or(SessionIdentityError::InvalidCodexIdentity)?,
            SessionIdSource::MetadataSessionId,
        );
        identity = merge_codex_identity(identity, candidate)?;
    }

    Ok(identity)
}

fn merge_codex_identity(
    current: Option<(String, SessionIdSource)>,
    candidate: (String, SessionIdSource),
) -> Result<Option<(String, SessionIdSource)>, SessionIdentityError> {
    match current {
        Some(current) if current.0 != candidate.0 => {
            Err(SessionIdentityError::InvalidCodexIdentity)
        }
        Some(current) => Ok(Some(current)),
        None => Ok(Some(candidate)),
    }
}

#[derive(Default)]
struct CodexClientIdentity {
    session_id: Option<String>,
    thread_id: Option<String>,
}

fn extract_codex_client_metadata(
    body: &serde_json::Value,
) -> Result<CodexClientIdentity, SessionIdentityError> {
    let Some(metadata_value) = body.get("client_metadata") else {
        return Ok(CodexClientIdentity::default());
    };
    let metadata = metadata_value
        .as_object()
        .ok_or(SessionIdentityError::InvalidCodexIdentity)?;
    let session_id = optional_canonical_uuid_value(metadata.get("session_id"))?;
    let thread_id = optional_canonical_uuid_value(metadata.get("thread_id"))?;
    let (turn_session_id, turn_thread_id) = metadata
        .get("x-codex-turn-metadata")
        .map(codex_turn_metadata_identity)
        .transpose()?
        .unwrap_or_default();

    let session_id = merge_optional_codex_identity(session_id, turn_session_id)?;
    let thread_id = merge_optional_codex_identity(thread_id, turn_thread_id)?;

    // Descendant agents intentionally share the root session_id while using
    // their own thread_id. Route by session_id so they inherit the parent's
    // provider pin; fall back to thread_id for older clients that omit it.
    Ok(CodexClientIdentity {
        session_id,
        thread_id,
    })
}

fn codex_turn_metadata_identity(
    value: &serde_json::Value,
) -> Result<(Option<String>, Option<String>), SessionIdentityError> {
    let serialized = value
        .as_str()
        .ok_or(SessionIdentityError::InvalidCodexIdentity)?;
    let metadata: serde_json::Value =
        serde_json::from_str(serialized).map_err(|_| SessionIdentityError::InvalidCodexIdentity)?;
    let metadata = metadata
        .as_object()
        .ok_or(SessionIdentityError::InvalidCodexIdentity)?;
    Ok((
        optional_canonical_uuid_value(metadata.get("session_id"))?,
        optional_canonical_uuid_value(metadata.get("thread_id"))?,
    ))
}

fn optional_canonical_uuid_value(
    value: Option<&serde_json::Value>,
) -> Result<Option<String>, SessionIdentityError> {
    value.map(canonical_uuid_value).transpose()
}

fn merge_optional_codex_identity(
    current: Option<String>,
    candidate: Option<String>,
) -> Result<Option<String>, SessionIdentityError> {
    match (current, candidate) {
        (Some(current), Some(candidate)) if current != candidate => {
            Err(SessionIdentityError::InvalidCodexIdentity)
        }
        (Some(current), _) => Ok(Some(current)),
        (None, candidate) => Ok(candidate),
    }
}

fn validate_legacy_codex_identity(
    legacy_identity: Option<&(String, SessionIdSource)>,
    expected: &str,
) -> Result<(), SessionIdentityError> {
    if let Some((legacy_session_id, _)) = legacy_identity {
        if legacy_session_id != expected {
            return Err(SessionIdentityError::InvalidCodexIdentity);
        }
    }
    Ok(())
}

fn canonical_uuid_value(value: &serde_json::Value) -> Result<String, SessionIdentityError> {
    value
        .as_str()
        .and_then(canonical_uuid)
        .ok_or(SessionIdentityError::InvalidCodexIdentity)
}

fn canonical_uuid(value: &str) -> Option<String> {
    Uuid::parse_str(value)
        .ok()
        .map(|id| id.hyphenated().to_string())
}

fn canonical_codex_uuid(value: &str) -> Option<String> {
    canonical_uuid(value.strip_prefix("codex_").unwrap_or(value))
}

/// 从 metadata 提取 Session ID (Claude)
fn extract_from_metadata(body: &serde_json::Value) -> Option<SessionIdResult> {
    let metadata = body.get("metadata")?;

    // 1. 从 metadata.user_id 提取（格式: user_xxx_session_yyy）
    if let Some(user_id) = metadata.get("user_id").and_then(|v| v.as_str()) {
        if let Some(session_id) = parse_session_from_user_id(user_id) {
            return Some(SessionIdResult {
                session_id,
                source: SessionIdSource::MetadataUserId,
                client_provided: true,
            });
        }
    }

    // 2. 直接从 metadata.session_id 提取
    if let Some(session_id) = metadata.get("session_id").and_then(|v| v.as_str()) {
        if !session_id.is_empty() {
            return Some(SessionIdResult {
                session_id: session_id.to_string(),
                source: SessionIdSource::MetadataSessionId,
                client_provided: true,
            });
        }
    }

    None
}

/// 从 user_id 解析 session_id
///
/// 格式: `user_identifier_session_actual_session_id`
pub(super) fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    // 查找 "_session_" 分隔符
    if let Some(pos) = user_id.find("_session_") {
        let session_id = &user_id[pos + 9..]; // "_session_" 长度为 9
        if !session_id.is_empty() {
            return Some(session_id.to_string());
        }
    }
    None
}

/// 生成新的 Session ID
fn generate_new_session_id() -> SessionIdResult {
    SessionIdResult {
        session_id: Uuid::new_v4().to_string(),
        source: SessionIdSource::Generated,
        client_provided: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_client_format_from_path_claude() {
        assert_eq!(
            ClientFormat::from_path("/v1/messages"),
            ClientFormat::Claude
        );
        assert_eq!(
            ClientFormat::from_path("/api/v1/messages"),
            ClientFormat::Claude
        );
    }

    #[test]
    fn test_client_format_from_path_codex() {
        assert_eq!(
            ClientFormat::from_path("/v1/responses"),
            ClientFormat::Codex
        );
    }

    #[test]
    fn test_client_format_from_path_openai() {
        assert_eq!(
            ClientFormat::from_path("/v1/chat/completions"),
            ClientFormat::OpenAI
        );
    }

    #[test]
    fn test_client_format_from_path_gemini() {
        assert_eq!(
            ClientFormat::from_path("/v1beta/models/gemini-pro:generateContent"),
            ClientFormat::Gemini
        );
    }

    #[test]
    fn test_client_format_from_path_gemini_cli() {
        assert_eq!(
            ClientFormat::from_path("/v1internal/models/gemini-pro:generateContent"),
            ClientFormat::GeminiCli
        );
    }

    #[test]
    fn test_client_format_from_body_claude() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Claude);
    }

    #[test]
    fn test_client_format_from_body_codex() {
        let body = json!({
            "input": "Write a function"
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Codex);
    }

    #[test]
    fn test_client_format_from_body_gemini() {
        let body = json!({
            "contents": [{"parts": [{"text": "Hello"}]}]
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Gemini);
    }

    #[test]
    fn test_session_id_uniqueness() {
        let session1 = ProxySession::from_request("POST", "/v1/messages", None, None);
        let session2 = ProxySession::from_request("POST", "/v1/messages", None, None);
        assert_ne!(session1.session_id, session2.session_id);
    }

    #[test]
    fn test_session_from_request() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024,
            "stream": true
        });

        let session =
            ProxySession::from_request("POST", "/v1/messages", Some("Mozilla/5.0"), Some(&body));

        assert_eq!(session.method, "POST");
        assert_eq!(session.request_url, "/v1/messages");
        assert_eq!(session.user_agent, Some("Mozilla/5.0".to_string()));
        assert_eq!(session.client_format, ClientFormat::Claude);
        assert_eq!(session.model, Some("claude-3-5-sonnet".to_string()));
        assert!(session.is_streaming);
    }

    #[test]
    fn test_session_with_provider() {
        let session = ProxySession::from_request("POST", "/v1/messages", None, None)
            .with_provider("provider-123");

        assert_eq!(session.provider_id, Some("provider-123".to_string()));
    }

    #[test]
    fn test_client_format_as_str() {
        assert_eq!(ClientFormat::Claude.as_str(), "claude");
        assert_eq!(ClientFormat::Codex.as_str(), "codex");
        assert_eq!(ClientFormat::OpenAI.as_str(), "openai");
        assert_eq!(ClientFormat::Gemini.as_str(), "gemini");
        assert_eq!(ClientFormat::GeminiCli.as_str(), "gemini_cli");
        assert_eq!(ClientFormat::Unknown.as_str(), "unknown");
    }

    // ========== Session ID 提取测试 ==========

    #[test]
    fn test_extract_session_from_claude_metadata_user_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "user_id": "user_john_doe_session_abc123def456"
            }
        });

        let result = extract_session_id(&headers, &body, "claude").expect("valid Claude identity");

        assert_eq!(result.session_id, "abc123def456");
        assert_eq!(result.source, SessionIdSource::MetadataUserId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_metadata_session_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude").expect("valid Claude identity");

        assert_eq!(result.session_id, "my-session-123");
        assert_eq!(result.source, SessionIdSource::MetadataSessionId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "d937243f-2702-4f20-97b6-c9682235ab81".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude").expect("valid Claude identity");

        assert_eq!(result.session_id, "d937243f-2702-4f20-97b6-c9682235ab81");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header_precedes_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "header-session-123".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude").expect("valid Claude identity");

        assert_eq!(result.session_id, "header-session-123");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_codex_previous_response_id_is_not_stable_session_identity() {
        let headers = HeaderMap::new();
        let body = json!({
            "input": "Write a function",
            "previous_response_id": "resp_abc123def456789"
        });

        let result = extract_session_id(&headers, &body, "codex").expect("no Codex identity");

        assert!(!result.session_id.is_empty());
        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_20260718_codex_desktop_client_metadata_uses_stable_thread_uuid() {
        let headers = HeaderMap::new();
        let expected = "019f7459-02c5-7352-97cf-10dd88bea80d";

        for body in [
            json!({
                "input": "Continue",
                "client_metadata": {
                    "session_id": expected,
                    "thread_id": expected,
                    "x-codex-turn-metadata": format!(
                        r#"{{"session_id":"{expected}","thread_id":"{expected}"}}"#
                    )
                }
            }),
            json!({
                "input": "Continue",
                "client_metadata": { "session_id": expected }
            }),
            json!({
                "input": "Continue",
                "client_metadata": { "thread_id": expected }
            }),
        ] {
            let result =
                extract_session_id(&headers, &body, "codex").expect("valid Codex Desktop identity");

            assert_eq!(result.session_id, format!("codex_{expected}"));
            assert_eq!(result.source, SessionIdSource::ClientMetadataSessionId);
            assert!(result.client_provided);
        }
    }

    #[test]
    fn test_codex_subagent_routes_by_shared_session_id() {
        let headers = HeaderMap::new();
        let session_id = "019f741e-28b7-7cc0-933b-2d96db451865";
        let thread_id = "019f7be5-75d5-7bc2-840c-dacb95336c7a";
        let body = json!({
            "input": "Inspect the route",
            "client_metadata": {
                "session_id": session_id,
                "thread_id": thread_id,
                "x-codex-turn-metadata": format!(
                    r#"{{"session_id":"{session_id}","thread_id":"{thread_id}","parent_thread_id":"{session_id}"}}"#
                )
            }
        });

        let result = extract_session_id(&headers, &body, "codex").expect("valid subagent identity");

        assert_eq!(result.session_id, format!("codex_{session_id}"));
        assert_eq!(result.source, SessionIdSource::ClientMetadataSessionId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_codex_subagent_header_session_precedes_metadata_thread_fallback() {
        let session_id = "019f741e-28b7-7cc0-933b-2d96db451865";
        let thread_id = "019f7be5-75d5-7bc2-840c-dacb95336c7a";
        let mut headers = HeaderMap::new();
        headers.insert("session_id", session_id.parse().unwrap());
        let body = json!({
            "input": "Inspect the route",
            "client_metadata": { "thread_id": thread_id }
        });

        let result = extract_session_id(&headers, &body, "codex").expect("valid identity");

        assert_eq!(result.session_id, format!("codex_{session_id}"));
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_codex_desktop_invalid_or_conflicting_client_metadata_is_rejected() {
        let headers = HeaderMap::new();

        for body in [
            json!({
                "input": "Continue",
                "client_metadata": {
                    "session_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                    "thread_id": "019f7458-f24e-7561-9248-9f4e94812c43",
                    "x-codex-turn-metadata": "{\"session_id\":\"019f7458-f24e-7561-9248-9f4e94812c43\",\"thread_id\":\"019f7458-f24e-7561-9248-9f4e94812c43\"}"
                }
            }),
            json!({
                "input": "Continue",
                "client_metadata": {
                    "session_id": "not-a-uuid",
                    "thread_id": "not-a-uuid"
                }
            }),
            json!({
                "input": "Continue",
                "client_metadata": {
                    "session_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                    "thread_id": "not-a-uuid"
                }
            }),
            json!({
                "input": "Continue",
                "client_metadata": {
                    "session_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                    "thread_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                    "x-codex-turn-metadata": "{\"session_id\":\"019f7459-02c5-7352-97cf-10dd88bea80d\",\"thread_id\":\"019f7458-f24e-7561-9248-9f4e94812c43\"}"
                }
            }),
            json!({
                "input": "Continue",
                "metadata": {
                    "session_id": "019f7458-f24e-7561-9248-9f4e94812c43"
                },
                "client_metadata": {
                    "session_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                    "thread_id": "019f7459-02c5-7352-97cf-10dd88bea80d"
                }
            }),
            json!({
                "input": "Continue",
                "metadata": { "session_id": "not-a-valid-session-uuid" }
            }),
            json!({
                "input": "Continue",
                "client_metadata": "not-an-object"
            }),
        ] {
            let error = extract_session_id(&headers, &body, "codex")
                .expect_err("invalid Codex identity must fail closed");
            assert_eq!(error, SessionIdentityError::InvalidCodexIdentity);
        }
    }

    #[test]
    fn test_codex_desktop_conflicting_legacy_header_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "session_id",
            "019f7458-f24e-7561-9248-9f4e94812c43".parse().unwrap(),
        );
        let body = json!({
            "input": "Continue",
            "client_metadata": {
                "session_id": "019f7459-02c5-7352-97cf-10dd88bea80d",
                "thread_id": "019f7459-02c5-7352-97cf-10dd88bea80d"
            }
        });

        let error = extract_session_id(&headers, &body, "codex")
            .expect_err("conflicting Codex identity must fail closed");
        assert_eq!(error, SessionIdentityError::InvalidCodexIdentity);
    }

    #[test]
    fn test_codex_invalid_legacy_header_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("session_id", "not-a-valid-session-uuid".parse().unwrap());
        let body = json!({ "input": "Continue" });

        let error = extract_session_id(&headers, &body, "codex")
            .expect_err("invalid legacy identity must fail closed");
        assert_eq!(error, SessionIdentityError::InvalidCodexIdentity);
    }

    #[test]
    fn test_extract_session_generates_new_when_not_found() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude").expect("generated identity");

        assert!(!result.session_id.is_empty());
        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_parse_session_from_user_id() {
        assert_eq!(
            parse_session_from_user_id("user_john_session_abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_from_user_id("my_app_session_xyz789"),
            Some("xyz789".to_string())
        );
        // 注意: "_session_" 是分隔符，所以下面的字符串会匹配
        assert_eq!(
            parse_session_from_user_id("no_session_marker"),
            Some("marker".to_string())
        );
        // 没有 "_session_" 分隔符的情况
        assert_eq!(parse_session_from_user_id("user_john_abc123"), None);
        assert_eq!(parse_session_from_user_id("_session_"), None);
    }
}
