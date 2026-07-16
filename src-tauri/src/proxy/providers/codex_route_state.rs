//! Successful Codex provider ownership by session.
//!
//! A route attempt is read-only until `complete_successfully` is called. This
//! keeps response headers, partial SSE streams, failures, and cancellations
//! from changing the provider that owns the session's portable history.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

const DEFAULT_MAX_SESSIONS: usize = 512;

#[derive(Debug, Default)]
struct CodexRouteStateInner {
    providers: HashMap<String, String>,
    session_order: VecDeque<String>,
}

/// Bounded, process-local record of the last successful provider per session.
#[derive(Debug)]
pub struct CodexRouteState {
    max_sessions: usize,
    inner: Mutex<CodexRouteStateInner>,
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
}

impl CodexRouteState {
    pub fn with_capacity(max_sessions: usize) -> Self {
        Self {
            max_sessions: max_sessions.max(1),
            inner: Mutex::new(CodexRouteStateInner::default()),
        }
    }

    /// Start a request without changing the last successful provider.
    pub fn begin_attempt(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> CodexRouteAttempt {
        let session_id = session_id.into();
        let provider_id = provider_id.into();
        let previous_provider_id = self.lookup_and_touch(&session_id);

        CodexRouteAttempt {
            state: self.clone(),
            session_id,
            provider_id,
            previous_provider_id,
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
    pub fn previous_provider_id(&self) -> Option<&str> {
        self.previous_provider_id.as_deref()
    }

    pub fn is_provider_change(&self) -> bool {
        self.previous_provider_id
            .as_deref()
            .is_some_and(|previous| previous != self.provider_id)
    }

    /// Commit ownership after the full response has completed successfully.
    pub fn complete_successfully(self) {
        self.state.record_success(self.session_id, self.provider_id);
    }
}

fn touch_session(session_order: &mut VecDeque<String>, session_id: &str) {
    session_order.retain(|cached_session| cached_session != session_id);
    session_order.push_back(session_id.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
}
