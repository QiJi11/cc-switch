#![allow(non_snake_case)]

use std::collections::HashMap;

use serde::Serialize;
use tauri::State;

use crate::{
    database::{canonical_codex_session_id, CodexSessionRoute, Database},
    error::AppError,
    session_manager,
    store::AppState,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListItem {
    #[serde(flatten)]
    session: session_manager::SessionMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_successful_provider_id: Option<String>,
}

#[tauri::command]
pub async fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionListItem>, String> {
    let sessions = tauri::async_runtime::spawn_blocking(session_manager::scan_sessions)
        .await
        .map_err(|e| format!("Failed to scan sessions: {e}"))?;
    let routes = state
        .db
        .list_codex_session_routes()
        .map_err(|error| error.to_string())?;
    Ok(attach_codex_session_routes(sessions, routes))
}

#[tauri::command]
pub async fn get_session_messages(
    providerId: String,
    sourcePath: String,
) -> Result<Vec<session_manager::SessionMessage>, String> {
    let provider_id = providerId.clone();
    let source_path = sourcePath.clone();
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::load_messages(&provider_id, &source_path)
    })
    .await
    .map_err(|e| format!("Failed to load session messages: {e}"))?
}

#[tauri::command]
pub async fn launch_session_terminal(
    command: String,
    cwd: Option<String>,
    custom_config: Option<String>,
) -> Result<bool, String> {
    let command = command.clone();
    let cwd = cwd.clone();
    let custom_config = custom_config.clone();

    // Read preferred terminal from global settings
    let preferred = crate::settings::get_preferred_terminal();
    // Map global setting terminal names to session terminal names
    // Global uses "iterm2", session terminal uses "iterm"
    let target = match preferred.as_deref() {
        Some("iterm2") => "iterm".to_string(),
        Some(t) => t.to_string(),
        None => "terminal".to_string(), // Default to Terminal.app on macOS
    };

    tauri::async_runtime::spawn_blocking(move || {
        session_manager::terminal::launch_terminal(
            &target,
            &command,
            cwd.as_deref(),
            custom_config.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("Failed to launch terminal: {e}"))??;

    Ok(true)
}

#[tauri::command]
pub async fn delete_session(
    state: State<'_, AppState>,
    providerId: String,
    sessionId: String,
    sourcePath: String,
) -> Result<bool, String> {
    let provider_id = providerId.clone();
    let session_id = sessionId.clone();
    let source_path = sourcePath.clone();

    let deleted = tauri::async_runtime::spawn_blocking(move || {
        session_manager::delete_session(&provider_id, &session_id, &source_path)
    })
    .await
    .map_err(|e| format!("Failed to delete session: {e}"))??;
    if deleted {
        delete_codex_route(&state.db, &providerId, &sessionId)
            .map_err(|error| error.to_string())?;
    }
    Ok(deleted)
}

#[tauri::command]
pub async fn delete_sessions(
    state: State<'_, AppState>,
    items: Vec<session_manager::DeleteSessionRequest>,
) -> Result<Vec<session_manager::DeleteSessionOutcome>, String> {
    let outcomes =
        tauri::async_runtime::spawn_blocking(move || session_manager::delete_sessions(&items))
            .await
            .map_err(|e| format!("Failed to delete sessions: {e}"))?;
    Ok(outcomes
        .into_iter()
        .map(|outcome| attach_route_cleanup_result(&state.db, outcome))
        .collect())
}

#[tauri::command]
pub fn set_codex_session_provider(
    state: State<'_, AppState>,
    sessionId: String,
    providerId: Option<String>,
) -> Result<bool, String> {
    save_codex_session_provider(&state.db, &sessionId, providerId.as_deref())
        .map(|_| true)
        .map_err(|error| error.to_string())
}

fn attach_codex_session_routes(
    sessions: Vec<session_manager::SessionMeta>,
    routes: Vec<CodexSessionRoute>,
) -> Vec<SessionListItem> {
    let routes_by_session = routes
        .into_iter()
        .map(|route| (route.session_id.clone(), route))
        .collect::<HashMap<_, _>>();
    sessions
        .into_iter()
        .map(|session| {
            let route = if session.provider_id == "codex" {
                canonical_codex_session_id(&session.session_id)
                    .ok()
                    .and_then(|session_id| routes_by_session.get(&session_id))
            } else {
                None
            };
            SessionListItem {
                pinned_provider_id: route.and_then(|route| route.pinned_provider_id.clone()),
                last_successful_provider_id: route
                    .and_then(|route| route.last_successful_provider_id.clone()),
                session,
            }
        })
        .collect()
}

fn save_codex_session_provider(
    db: &Database,
    session_id: &str,
    provider_id: Option<&str>,
) -> Result<(), AppError> {
    let session_id = canonical_codex_session_id(session_id)?;
    match provider_id {
        Some(provider_id) => {
            db.get_provider_by_id(provider_id, "codex")?
                .ok_or_else(|| {
                    AppError::InvalidInput(format!("Codex provider not found: {provider_id}"))
                })?;
            db.set_codex_session_pin(&session_id, provider_id)
        }
        None => db.clear_codex_session_pin(&session_id),
    }
}

fn delete_codex_route(db: &Database, provider_id: &str, session_id: &str) -> Result<(), AppError> {
    if provider_id == "codex" {
        db.delete_codex_session_route(session_id)?;
    }
    Ok(())
}

fn attach_route_cleanup_result(
    db: &Database,
    mut outcome: session_manager::DeleteSessionOutcome,
) -> session_manager::DeleteSessionOutcome {
    if outcome.success {
        if let Err(error) = delete_codex_route(db, &outcome.provider_id, &outcome.session_id) {
            outcome.success = false;
            outcome.error = Some(error.to_string());
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn session(provider_id: &str) -> session_manager::SessionMeta {
        session_manager::SessionMeta {
            provider_id: provider_id.to_string(),
            session_id: SESSION_ID.to_string(),
            title: None,
            summary: None,
            project_dir: None,
            created_at: None,
            last_active_at: None,
            source_path: None,
            resume_command: None,
        }
    }

    #[test]
    fn session_list_attaches_routes_only_to_codex_sessions() {
        let route = CodexSessionRoute {
            session_id: SESSION_ID.to_string(),
            pinned_provider_id: Some("provider-b".to_string()),
            last_successful_provider_id: Some("provider-a".to_string()),
            created_at: 1,
            updated_at: 2,
        };

        let mut codex_session = session("codex");
        codex_session.session_id = "550E8400E29B41D4A716446655440000".to_string();
        let sessions =
            attach_codex_session_routes(vec![codex_session, session("claude")], vec![route]);

        assert_eq!(
            sessions[0].pinned_provider_id.as_deref(),
            Some("provider-b")
        );
        assert_eq!(
            sessions[0].last_successful_provider_id.as_deref(),
            Some("provider-a")
        );
        assert_eq!(sessions[1].pinned_provider_id, None);
        assert_eq!(sessions[1].last_successful_provider_id, None);
    }

    #[test]
    fn set_provider_validates_codex_provider_and_preserves_success_owner() {
        let db = Database::memory().expect("create memory database");
        let provider = Provider::with_id(
            "provider-b".to_string(),
            "Provider B".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save Codex provider");
        db.set_codex_session_last_successful_provider(SESSION_ID, "provider-a")
            .expect("seed successful owner");

        save_codex_session_provider(&db, SESSION_ID, Some("provider-b"))
            .expect("set conversation pin");
        let route = db
            .get_codex_session_route(SESSION_ID)
            .expect("read route")
            .expect("route should exist");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("provider-b"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );

        save_codex_session_provider(&db, SESSION_ID, None).expect("clear conversation pin");
        let route = db
            .get_codex_session_route(SESSION_ID)
            .expect("read route")
            .expect("route should remain");
        assert_eq!(route.pinned_provider_id, None);
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );
        assert!(save_codex_session_provider(&db, SESSION_ID, Some("missing")).is_err());
        assert!(save_codex_session_provider(&db, "not-a-uuid", None).is_err());
    }

    #[test]
    fn successful_codex_delete_outcome_removes_route() {
        let db = Database::memory().expect("create memory database");
        db.set_codex_session_pin(SESSION_ID, "provider-b")
            .expect("seed route");
        let outcome = session_manager::DeleteSessionOutcome {
            provider_id: "codex".to_string(),
            session_id: SESSION_ID.to_string(),
            source_path: "session.jsonl".to_string(),
            success: true,
            error: None,
        };

        let outcome = attach_route_cleanup_result(&db, outcome);

        assert!(outcome.success);
        assert_eq!(
            db.get_codex_session_route(SESSION_ID)
                .expect("read cleaned route"),
            None
        );
    }
}
