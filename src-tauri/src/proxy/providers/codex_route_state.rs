//! Successful Codex provider ownership by session.
//!
//! A route attempt is read-only until `complete_successfully` is called. This
//! keeps response headers, partial SSE streams, failures, and cancellations
//! from changing the provider that owns the session's portable history.

use crate::database::Database;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

const DEFAULT_MAX_SESSIONS: usize = 512;

#[derive(Debug, Default)]
struct CodexRouteStateInner {
    providers: HashMap<String, String>,
    session_order: VecDeque<String>,
}

/// Persistent successful-provider ownership with a bounded process-local cache.
pub struct CodexRouteState {
    max_sessions: usize,
    db: Option<Arc<Database>>,
    inner: Mutex<CodexRouteStateInner>,
}

impl std::fmt::Debug for CodexRouteState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexRouteState")
            .field("max_sessions", &self.max_sessions)
            .field("persistent", &self.db.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for CodexRouteState {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAX_SESSIONS)
    }
}

/// A request attempt that does not affect route ownership until committed.
#[derive(Debug)]
#[must_use = "route ownership changes only after complete_successfully is called"]
pub struct CodexRouteAttempt {
    state: Arc<CodexRouteState>,
    session_id: String,
    provider_id: String,
    previous_provider_id: Option<String>,
    expected_pinned_provider_id: Option<String>,
    force_provider_boundary: bool,
    pin_provider_on_success: bool,
    persistable: bool,
}

impl CodexRouteState {
    pub fn with_capacity(max_sessions: usize) -> Self {
        Self {
            max_sessions: max_sessions.max(1),
            db: None,
            inner: Mutex::new(CodexRouteStateInner::default()),
        }
    }

    pub fn with_database(db: Arc<Database>) -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            db: Some(db),
            inner: Mutex::new(CodexRouteStateInner::default()),
        }
    }

    /// Start a request without changing the last successful provider.
    #[cfg(test)]
    pub fn begin_attempt(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> CodexRouteAttempt {
        self.begin_attempt_with_boundary(session_id, provider_id, false)
    }

    #[cfg(test)]
    pub fn begin_attempt_with_boundary(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
        force_provider_boundary: bool,
    ) -> CodexRouteAttempt {
        self.begin_attempt_with_boundary_and_repin(
            session_id,
            provider_id,
            force_provider_boundary,
            false,
        )
    }

    pub fn begin_attempt_with_boundary_and_repin(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
        force_provider_boundary: bool,
        pin_provider_on_success: bool,
    ) -> CodexRouteAttempt {
        let original_session_id = session_id.into();
        let provider_id = provider_id.into();
        let canonical_session_id = canonical_session_id(&original_session_id);
        let persistable = canonical_session_id.is_some() && self.db.is_some();
        let session_id = canonical_session_id.unwrap_or(original_session_id);
        let persistent_route = self.lookup_persistent_route(&session_id);
        let expected_pinned_provider_id = persistent_route
            .as_ref()
            .and_then(|route| route.pinned_provider_id.clone());
        let previous_provider_id = persistent_route
            .and_then(|route| route.last_successful_provider_id)
            .or_else(|| self.lookup_and_touch(&session_id));
        let force_provider_boundary = force_provider_boundary && previous_provider_id.is_none();

        CodexRouteAttempt {
            state: self.clone(),
            session_id,
            provider_id,
            previous_provider_id,
            expected_pinned_provider_id,
            force_provider_boundary,
            pin_provider_on_success,
            persistable,
        }
    }

    fn lookup_persistent_route(
        &self,
        session_id: &str,
    ) -> Option<crate::database::CodexSessionRoute> {
        let db = self.db.as_ref()?;
        match db.get_codex_session_route(session_id) {
            Ok(route) => route,
            Err(error) => {
                log::warn!(
                    "[CodexRouteState] failed to read persistent route: session={session_id}, error={error}"
                );
                None
            }
        }
    }

    fn lookup_and_touch(&self, session_id: &str) -> Option<String> {
        let mut inner = self.lock_inner();
        let provider_id = inner.providers.get(session_id).cloned();
        if provider_id.is_some() {
            touch_session(&mut inner.session_order, session_id);
        }
        provider_id
    }

    fn record_success(&self, session_id: String, provider_id: String) {
        let mut inner = self.lock_inner();
        inner.providers.insert(session_id.clone(), provider_id);
        touch_session(&mut inner.session_order, &session_id);

        while inner.providers.len() > self.max_sessions {
            let Some(oldest_session) = inner.session_order.pop_front() else {
                break;
            };
            inner.providers.remove(&oldest_session);
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, CodexRouteStateInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            log::warn!("[CodexRouteState] recovering poisoned route-state lock");
            poisoned.into_inner()
        })
    }
}

impl CodexRouteAttempt {
    #[cfg(test)]
    pub fn previous_provider_id(&self) -> Option<&str> {
        self.previous_provider_id.as_deref()
    }

    pub fn is_provider_change(&self) -> bool {
        self.force_provider_boundary
            || self
                .previous_provider_id
                .as_deref()
                .is_some_and(|previous| previous != self.provider_id)
    }

    /// Commit ownership after the full response has completed successfully.
    pub fn complete_successfully(self) {
        if self.persistable {
            if let Some(db) = &self.state.db {
                let persisted = if self.pin_provider_on_success {
                    db.set_codex_session_pinned_success(&self.session_id, &self.provider_id)
                        .map(|_| true)
                } else {
                    db.set_codex_session_last_successful_if_pin_matches(
                        &self.session_id,
                        &self.provider_id,
                        self.expected_pinned_provider_id.as_deref(),
                    )
                };
                match persisted {
                    Ok(true) => {}
                    Ok(false) => {
                        log::info!(
                            "[CodexRouteState] ignored stale completion after pin changed: session={}, provider={}",
                            self.session_id,
                            self.provider_id
                        );
                        return;
                    }
                    Err(error) => {
                        log::error!(
                            "[CodexRouteState] failed to persist completed route: session={}, provider={}, error={error}",
                            self.session_id,
                            self.provider_id
                        );
                        return;
                    }
                }
            }
        }
        self.state.record_success(self.session_id, self.provider_id);
    }
}

fn canonical_session_id(session_id: &str) -> Option<String> {
    let raw = session_id.strip_prefix("codex_").unwrap_or(session_id);
    uuid::Uuid::parse_str(raw)
        .ok()
        .map(|id| id.hyphenated().to_string())
}

fn touch_session(session_order: &mut VecDeque<String>, session_id: &str) {
    session_order.retain(|cached_session| cached_session != session_id);
    session_order.push_back(session_id.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    const SESSION_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn first_request_commits_only_after_successful_completion() {
        let state = Arc::new(CodexRouteState::default());
        let first = state.begin_attempt("session-1", "provider-a");

        assert_eq!(first.previous_provider_id(), None);
        assert!(!first.is_provider_change());

        let before_completion = state.begin_attempt("session-1", "provider-a");
        assert_eq!(before_completion.previous_provider_id(), None);
        drop(before_completion);

        first.complete_successfully();
        assert_eq!(
            state
                .begin_attempt("session-1", "provider-a")
                .previous_provider_id(),
            Some("provider-a")
        );
    }

    #[test]
    fn same_provider_is_not_a_handoff() {
        let state = Arc::new(CodexRouteState::default());
        state
            .begin_attempt("session-1", "provider-a")
            .complete_successfully();

        let next = state.begin_attempt("session-1", "provider-a");

        assert_eq!(next.previous_provider_id(), Some("provider-a"));
        assert!(!next.is_provider_change());
    }

    #[test]
    fn provider_change_uses_last_successful_provider() {
        let state = Arc::new(CodexRouteState::default());
        state
            .begin_attempt("session-1", "provider-a")
            .complete_successfully();

        let handoff = state.begin_attempt("session-1", "provider-b");

        assert_eq!(handoff.previous_provider_id(), Some("provider-a"));
        assert!(handoff.is_provider_change());
        handoff.complete_successfully();
        assert_eq!(
            state
                .begin_attempt("session-1", "provider-c")
                .previous_provider_id(),
            Some("provider-b")
        );
    }

    #[test]
    fn dropped_partial_attempt_does_not_change_ownership() {
        let state = Arc::new(CodexRouteState::default());
        state
            .begin_attempt("session-1", "provider-a")
            .complete_successfully();

        let partial = state.begin_attempt("session-1", "provider-b");
        assert!(partial.is_provider_change());
        drop(partial);

        assert_eq!(
            state
                .begin_attempt("session-1", "provider-c")
                .previous_provider_id(),
            Some("provider-a")
        );
    }

    #[test]
    fn concurrent_sessions_remain_isolated() {
        let state = Arc::new(CodexRouteState::with_capacity(64));
        let handles = (0..32)
            .map(|index| {
                let state = state.clone();
                thread::spawn(move || {
                    state
                        .begin_attempt(format!("session-{index}"), format!("provider-{index}"))
                        .complete_successfully();
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().expect("route-state worker should not panic");
        }

        for index in 0..32 {
            let session_id = format!("session-{index}");
            let provider_id = format!("provider-{index}");
            let attempt = state.begin_attempt(session_id, "next-provider");
            assert_eq!(attempt.previous_provider_id(), Some(provider_id.as_str()));
        }
    }

    #[test]
    fn least_recently_used_session_is_pruned() {
        let state = Arc::new(CodexRouteState::with_capacity(2));
        state
            .begin_attempt("session-1", "provider-a")
            .complete_successfully();
        state
            .begin_attempt("session-2", "provider-b")
            .complete_successfully();

        drop(state.begin_attempt("session-1", "provider-a"));
        state
            .begin_attempt("session-3", "provider-c")
            .complete_successfully();

        assert_eq!(
            state
                .begin_attempt("session-1", "next-provider")
                .previous_provider_id(),
            Some("provider-a")
        );
        assert_eq!(
            state
                .begin_attempt("session-2", "next-provider")
                .previous_provider_id(),
            None
        );
        assert_eq!(
            state
                .begin_attempt("session-3", "next-provider")
                .previous_provider_id(),
            Some("provider-c")
        );
    }

    #[test]
    fn a_new_state_forgets_previous_routes() {
        let first_process = Arc::new(CodexRouteState::default());
        first_process
            .begin_attempt("session-1", "provider-a")
            .complete_successfully();

        let restarted_process = Arc::new(CodexRouteState::default());

        assert_eq!(
            restarted_process
                .begin_attempt("session-1", "provider-b")
                .previous_provider_id(),
            None
        );
    }

    #[test]
    fn persistent_route_survives_state_recreation_and_first_pin_is_conservative() {
        let db = Arc::new(Database::memory().expect("memory db"));
        let first_process = Arc::new(CodexRouteState::with_database(db.clone()));

        let first_pin = first_process.begin_attempt_with_boundary(
            format!("codex_{SESSION_UUID}"),
            "provider-a",
            true,
        );
        assert!(first_pin.is_provider_change());
        assert_eq!(first_pin.previous_provider_id(), None);
        first_pin.complete_successfully();

        assert_eq!(
            db.get_codex_session_route(SESSION_UUID)
                .expect("read route")
                .expect("route should exist")
                .last_successful_provider_id
                .as_deref(),
            Some("provider-a")
        );

        let restarted_process = Arc::new(CodexRouteState::with_database(db));
        let same_pin =
            restarted_process.begin_attempt_with_boundary(SESSION_UUID, "provider-a", true);
        assert!(!same_pin.is_provider_change());
        drop(same_pin);

        let repin = restarted_process.begin_attempt_with_boundary(SESSION_UUID, "provider-b", true);
        assert_eq!(repin.previous_provider_id(), Some("provider-a"));
        assert!(repin.is_provider_change());
    }

    #[test]
    fn successful_gateway_migration_updates_pin_and_last_successful_provider() {
        let db = Arc::new(Database::memory().expect("memory db"));
        db.set_codex_session_pinned_success(SESSION_UUID, "provider-a")
            .expect("seed original pin");
        let state = Arc::new(CodexRouteState::with_database(db.clone()));

        state
            .begin_attempt_with_boundary_and_repin(SESSION_UUID, "gateway", true, true)
            .complete_successfully();

        let route = db
            .get_codex_session_route(SESSION_UUID)
            .expect("read migrated route")
            .expect("migrated route");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("gateway"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("gateway")
        );
    }

    #[test]
    fn dropped_gateway_migration_keeps_original_pin() {
        let db = Arc::new(Database::memory().expect("memory db"));
        db.set_codex_session_pinned_success(SESSION_UUID, "provider-a")
            .expect("seed original pin");
        let state = Arc::new(CodexRouteState::with_database(db.clone()));

        drop(state.begin_attempt_with_boundary_and_repin(SESSION_UUID, "gateway", true, true));

        let route = db
            .get_codex_session_route(SESSION_UUID)
            .expect("read unchanged route")
            .expect("existing route");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("provider-a"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );
    }

    #[test]
    fn older_pinned_request_cannot_overwrite_completed_gateway_migration() {
        let db = Arc::new(Database::memory().expect("memory db"));
        db.set_codex_session_pinned_success(SESSION_UUID, "provider-a")
            .expect("seed original pin");
        let state = Arc::new(CodexRouteState::with_database(db.clone()));

        let older = state.begin_attempt_with_boundary(SESSION_UUID, "provider-a", true);
        state
            .begin_attempt_with_boundary_and_repin(SESSION_UUID, "gateway", true, true)
            .complete_successfully();
        older.complete_successfully();

        let route = db
            .get_codex_session_route(SESSION_UUID)
            .expect("read migrated route")
            .expect("migrated route");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("gateway"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("gateway")
        );
    }
}
