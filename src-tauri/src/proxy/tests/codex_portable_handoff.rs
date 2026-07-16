use std::{
    collections::HashMap,
    env,
    path::Path,
    sync::{Arc, Mutex, Once},
};

use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use log::{LevelFilter, Log, Metadata, Record};
use reqwest::Client;
use serde_json::{json, Value};
use serial_test::serial;
use tempfile::TempDir;
use tokio::sync::oneshot;

use crate::{
    app_config::AppType,
    database::Database,
    provider::{Provider, ProviderMeta},
    proxy::{
        server::ProxyServer,
        types::{AppProxyConfig, ProxyConfig},
    },
};

const SESSION_ID: &str = "019cc369-bd7c-7891-b371-7b20b4fe0b18";
const ENCRYPTED_SECRET: &str = "opaque-provider-a-secret-never-log";
const TRANSCRIPT_SECRET: &str = "sk-transcript-secretvalue-never-log";
const TRANSCRIPT_BODY: &str = "portable-visible-transcript-body-never-log";

static TEST_LOGGER: CapturingLogger = CapturingLogger {
    lines: Mutex::new(Vec::new()),
};
static INSTALL_LOGGER: Once = Once::new();

struct CapturingLogger {
    lines: Mutex<Vec<String>>,
}

impl CapturingLogger {
    fn install_and_clear() {
        INSTALL_LOGGER.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("install test logger");
            log::set_max_level(LevelFilter::Debug);
        });
        TEST_LOGGER.lines.lock().expect("lock test logs").clear();
    }

    fn snapshot() -> String {
        TEST_LOGGER.lines.lock().expect("lock test logs").join("\n")
    }
}

impl Log for CapturingLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            self.lines.lock().expect("lock test logs").push(format!(
                "{} {}",
                record.level(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

struct TempHome {
    dir: TempDir,
    original_home: Option<String>,
    original_userprofile: Option<String>,
    original_test_home: Option<String>,
}

impl TempHome {
    fn new() -> Self {
        let dir = TempDir::new().expect("create temporary home");
        let original_home = env::var("HOME").ok();
        let original_userprofile = env::var("USERPROFILE").ok();
        let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

        env::set_var("HOME", dir.path());
        env::set_var("USERPROFILE", dir.path());
        env::set_var("CC_SWITCH_TEST_HOME", dir.path());
        crate::settings::reload_settings().expect("reload temporary settings");

        Self {
            dir,
            original_home,
            original_userprofile,
            original_test_home,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        restore_env("HOME", self.original_home.as_deref());
        restore_env("USERPROFILE", self.original_userprofile.as_deref());
        restore_env("CC_SWITCH_TEST_HOME", self.original_test_home.as_deref());
        crate::settings::reload_settings().expect("restore settings after proxy fixture");
    }
}

fn restore_env(name: &str, value: Option<&str>) {
    match value {
        Some(value) => env::set_var(name, value),
        None => env::remove_var(name),
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    provider: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone, Default)]
struct FakeUpstreamState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    calls: Arc<Mutex<HashMap<String, usize>>>,
}

struct FakeUpstream {
    base_url: String,
    state: FakeUpstreamState,
    shutdown: Option<oneshot::Sender<()>>,
}

impl FakeUpstream {
    async fn start() -> Self {
        let state = FakeUpstreamState::default();
        let app = Router::new()
            .route("/:provider/responses", post(handle_fake_response))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local fake upstream");
        let address = listener.local_addr().expect("read fake upstream address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve local fake upstream");
        });

        Self {
            base_url: format!("http://{address}"),
            state,
            shutdown: Some(shutdown_tx),
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.state
            .requests
            .lock()
            .expect("lock captured requests")
            .clone()
    }

    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn handle_fake_response(
    AxumPath(provider): AxumPath<String>,
    State(state): State<FakeUpstreamState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state
        .requests
        .lock()
        .expect("lock captured requests")
        .push(CapturedRequest {
            provider: provider.clone(),
            headers,
            body,
        });

    let call_index = {
        let mut calls = state.calls.lock().expect("lock fake upstream calls");
        let call = calls.entry(provider.clone()).or_default();
        let index = *call;
        *call += 1;
        index
    };

    match (provider.as_str(), call_index) {
        ("a", 0) => success_response("resp_a_1", "assistant-a", false),
        ("a", 1) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {"message": "local fixture rate limit"}})),
        )
            .into_response(),
        ("b", 0) => success_response("resp_b_1", "assistant-b", true),
        ("b", 1) => success_response("resp_b_2", "assistant-b-after-failover", false),
        ("c", 0) => success_response("resp_c_1", "assistant-c", false),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": "unexpected local fixture request"}})),
        )
            .into_response(),
    }
}

fn success_response(response_id: &str, assistant_text: &str, include_tool_call: bool) -> Response {
    let mut output = vec![json!({
        "id": format!("msg_{response_id}"),
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": assistant_text}]
    })];
    if include_tool_call {
        output.push(json!({
            "id": "fc_provider_b",
            "type": "function_call",
            "status": "completed",
            "call_id": "call_once",
            "name": "local_fixture_tool",
            "arguments": "{}"
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "id": response_id,
            "object": "response",
            "created_at": 1_752_710_400,
            "status": "completed",
            "model": "gpt-local-fixture",
            "output": output,
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
        })),
    )
        .into_response()
}

fn provider(id: &str, upstream_base_url: &str) -> Provider {
    let mut provider = Provider::with_id(
        id.to_string(),
        format!("Local Provider {}", id.to_uppercase()),
        json!({
            "base_url": format!("{upstream_base_url}/{id}"),
            "api_key": format!("local-fixture-key-{id}")
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        api_format: Some("openai_responses".to_string()),
        ..Default::default()
    });
    provider
}

fn write_rollout_fixture(home: &Path) {
    let directory = home
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("07")
        .join("17");
    std::fs::create_dir_all(&directory).expect("create rollout fixture directory");
    let path = directory.join(format!("rollout-2026-07-17T05-31-56-{SESSION_ID}.jsonl"));
    let records = [
        json!({
            "timestamp": "2026-07-17T05:31:56Z",
            "type": "session_meta",
            "payload": {"id": SESSION_ID, "cwd": "C:/local-fixture"}
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": format!("turn-a token={TRANSCRIPT_SECRET}")
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": TRANSCRIPT_BODY
            }
        }),
        json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": "turn-b"}
        }),
    ];
    let contents = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize rollout record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, contents).expect("write rollout fixture");
}

async fn set_failover(db: &Database, enabled: bool) {
    let mut config: AppProxyConfig = db
        .get_proxy_config_for_app("codex")
        .await
        .expect("read Codex proxy config");
    config.enabled = true;
    config.auto_failover_enabled = enabled;
    config.max_retries = 1;
    config.non_streaming_timeout = 10;
    db.update_proxy_config_for_app(config)
        .await
        .expect("update Codex proxy config");
}

fn set_current_provider(db: &Database, provider_id: &str) {
    db.set_current_provider("codex", provider_id)
        .expect("set database current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some(provider_id))
        .expect("set local current provider");
}

async fn post_response(client: &Client, proxy_url: &str, body: Value) -> Value {
    let response = client
        .post(proxy_url)
        .header("session_id", SESSION_ID)
        .json(&body)
        .send()
        .await
        .expect("send request through local proxy");
    let status = response.status();
    let text = response.text().await.expect("read local proxy response");
    assert_eq!(status, StatusCode::OK, "unexpected proxy response: {text}");
    serde_json::from_str(&text).expect("parse local proxy response")
}

fn output_items(responses: &[Value]) -> impl Iterator<Item = &Value> {
    responses
        .iter()
        .flat_map(|response| response["output"].as_array().into_iter().flatten())
}

fn session_header(request: &CapturedRequest) -> Option<&str> {
    request
        .headers
        .get("session_id")
        .or_else(|| request.headers.get("x-session-id"))
        .and_then(|value| value.to_str().ok())
}

#[tokio::test]
#[serial]
async fn three_provider_handoff_stays_local_and_does_not_duplicate_output_or_log_context() {
    CapturingLogger::install_and_clear();
    let home = TempHome::new();
    write_rollout_fixture(home.path());

    let mut upstream = FakeUpstream::start().await;
    let db = Arc::new(Database::memory().expect("create in-memory database"));
    for provider_id in ["a", "b", "c"] {
        db.save_provider("codex", &provider(provider_id, &upstream.base_url))
            .expect("save local fixture provider");
    }
    db.add_to_failover_queue("codex", "a")
        .expect("queue provider A");
    db.add_to_failover_queue("codex", "b")
        .expect("queue provider B");
    set_current_provider(&db, "a");
    set_failover(&db, false).await;

    let proxy = ProxyServer::new(
        ProxyConfig {
            listen_address: "127.0.0.1".to_string(),
            listen_port: 0,
            ..Default::default()
        },
        db.clone(),
        None,
    );
    let proxy_info = proxy.start().await.expect("start local proxy");
    let proxy_url = format!("http://127.0.0.1:{}/v1/responses", proxy_info.port);
    let client = Client::builder()
        .no_proxy()
        .build()
        .expect("build local-only client");

    let mut responses = Vec::new();
    responses.push(
        post_response(
            &client,
            &proxy_url,
            json!({
                "model": "gpt-local-fixture",
                "stream": false,
                "input": [{"type": "message", "role": "user", "content": "turn-a"}]
            }),
        )
        .await,
    );

    set_current_provider(&db, "b");
    let manual_b_body = json!({
        "model": "gpt-local-fixture",
        "stream": false,
        "previous_response_id": "resp_a_1",
        "input": [
            {
                "type": "compaction",
                "id": "cmp_provider_a",
                "encrypted_content": ENCRYPTED_SECRET
            },
            {"type": "message", "role": "user", "content": "turn-b"}
        ]
    });
    responses.push(post_response(&client, &proxy_url, manual_b_body).await);

    set_failover(&db, true).await;
    responses.push(
        post_response(
            &client,
            &proxy_url,
            json!({
                "model": "gpt-local-fixture",
                "stream": false,
                "previous_response_id": "resp_b_1",
                "input": [
                    {
                        "type": "message",
                        "id": "msg_provider_b",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "assistant-b"}]
                    },
                    {
                        "type": "function_call",
                        "id": "fc_provider_b",
                        "call_id": "call_once",
                        "name": "local_fixture_tool",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "id": "fco_provider_b",
                        "call_id": "call_once",
                        "output": "local result"
                    },
                    {"type": "message", "role": "user", "content": "turn-after-tool"}
                ]
            }),
        )
        .await,
    );

    set_failover(&db, false).await;
    set_current_provider(&db, "c");
    responses.push(
        post_response(
            &client,
            &proxy_url,
            json!({
                "model": "gpt-local-fixture",
                "stream": false,
                "previous_response_id": "resp_b_2",
                "input": [
                    {
                        "type": "message",
                        "id": "msg_provider_b_2",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "assistant-b-after-failover"}]
                    },
                    {
                        "type": "function_call_output",
                        "id": "fco_provider_b",
                        "call_id": "call_once",
                        "output": "local result"
                    },
                    {"type": "message", "role": "user", "content": "turn-c"}
                ]
            }),
        )
        .await,
    );

    proxy.stop().await.expect("stop local proxy");
    upstream.stop();

    let requests = upstream.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.provider.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "a", "b", "c"]
    );
    assert!(requests
        .iter()
        .all(|request| session_header(request) == Some(SESSION_ID)));

    let manual_b = &requests[1].body;
    assert!(manual_b.get("previous_response_id").is_none());
    assert!(!manual_b.to_string().contains(ENCRYPTED_SECRET));
    assert!(manual_b.to_string().contains(TRANSCRIPT_BODY));
    assert!(!manual_b.to_string().contains(TRANSCRIPT_SECRET));
    assert!(manual_b.to_string().contains("[REDACTED]"));

    let failed_a = &requests[2].body;
    let successful_b = &requests[3].body;
    assert!(failed_a.get("previous_response_id").is_none());
    assert_eq!(successful_b["previous_response_id"], "resp_b_1");
    assert!(requests[4].body.get("previous_response_id").is_none());

    let assistant_texts = output_items(&responses)
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["text"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_texts,
        [
            "assistant-a",
            "assistant-b",
            "assistant-b-after-failover",
            "assistant-c"
        ]
    );
    let tool_calls = output_items(&responses)
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_calls, ["call_once"]);

    let logs = CapturingLogger::snapshot();
    assert!(!logs.is_empty(), "proxy debug logs were not captured");
    for excluded in [ENCRYPTED_SECRET, TRANSCRIPT_SECRET, TRANSCRIPT_BODY] {
        assert!(
            !logs.contains(excluded),
            "sensitive fixture data leaked: {excluded}"
        );
    }
}
