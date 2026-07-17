//! Persistent provider routing state for Codex conversations.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::params;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionRoute {
    pub session_id: String,
    pub pinned_provider_id: Option<String>,
    pub last_successful_provider_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn canonical_session_id(session_id: &str) -> Result<String, AppError> {
    Uuid::parse_str(session_id)
        .map(|id| id.hyphenated().to_string())
        .map_err(|_| AppError::InvalidInput(format!("Invalid Codex session UUID: {session_id}")))
}

impl Database {
    pub fn get_codex_session_route(
        &self,
        session_id: &str,
    ) -> Result<Option<CodexSessionRoute>, AppError> {
        let session_id = canonical_session_id(session_id)?;
        let conn = lock_conn!(self.conn);
        match conn.query_row(
            "SELECT session_id, pinned_provider_id, last_successful_provider_id,
                    created_at, updated_at
             FROM codex_session_routes WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(CodexSessionRoute {
                    session_id: row.get(0)?,
                    pinned_provider_id: row.get(1)?,
                    last_successful_provider_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        ) {
            Ok(route) => Ok(Some(route)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(AppError::Database(error.to_string())),
        }
    }

    /// Set a conversation pin without changing its last-success marker.
    pub fn set_codex_session_pin(
        &self,
        session_id: &str,
        provider_id: &str,
    ) -> Result<(), AppError> {
        let session_id = canonical_session_id(session_id)?;
        let now = chrono::Utc::now().timestamp_millis();
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO codex_session_routes
                (session_id, pinned_provider_id, last_successful_provider_id,
                 created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                pinned_provider_id = excluded.pinned_provider_id,
                updated_at = excluded.updated_at",
            params![session_id, provider_id, now],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Clear a conversation pin without changing its last-success marker.
    pub fn clear_codex_session_pin(&self, session_id: &str) -> Result<(), AppError> {
        let session_id = canonical_session_id(session_id)?;
        let now = chrono::Utc::now().timestamp_millis();
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE codex_session_routes
             SET pinned_provider_id = NULL, updated_at = ?2
             WHERE session_id = ?1",
            params![session_id, now],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Record completed ownership without changing the user's current pin.
    pub fn set_codex_session_last_successful_provider(
        &self,
        session_id: &str,
        provider_id: &str,
    ) -> Result<(), AppError> {
        let session_id = canonical_session_id(session_id)?;
        let now = chrono::Utc::now().timestamp_millis();
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO codex_session_routes
                (session_id, pinned_provider_id, last_successful_provider_id,
                 created_at, updated_at)
             VALUES (?1, NULL, ?2, ?3, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                last_successful_provider_id = excluded.last_successful_provider_id,
                updated_at = excluded.updated_at",
            params![session_id, provider_id, now],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Clear every pin targeting a deleted provider while retaining route ownership.
    pub fn clear_codex_session_pins_for_provider(
        &self,
        provider_id: &str,
    ) -> Result<usize, AppError> {
        let now = chrono::Utc::now().timestamp_millis();
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE codex_session_routes
             SET pinned_provider_id = NULL, updated_at = ?2
             WHERE pinned_provider_id = ?1",
            params![provider_id, now],
        )
        .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn delete_codex_session_route(&self, session_id: &str) -> Result<bool, AppError> {
        let session_id = canonical_session_id(session_id)?;
        let conn = lock_conn!(self.conn);
        let affected = conn
            .execute(
                "DELETE FROM codex_session_routes WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn route_upserts_preserve_independent_pin_and_success_fields() -> Result<(), AppError> {
        let db = Database::memory()?;

        db.set_codex_session_pin(SESSION_ID, "provider-a")?;
        db.set_codex_session_last_successful_provider(SESSION_ID, "provider-a")?;
        let route = db
            .get_codex_session_route(SESSION_ID)?
            .expect("route should exist");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("provider-a"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );
        assert!(route.created_at <= route.updated_at);

        db.set_codex_session_pin(SESSION_ID, "provider-b")?;
        let route = db
            .get_codex_session_route(SESSION_ID)?
            .expect("route should exist");
        assert_eq!(route.pinned_provider_id.as_deref(), Some("provider-b"));
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );

        db.clear_codex_session_pin(SESSION_ID)?;
        let route = db
            .get_codex_session_route(SESSION_ID)?
            .expect("route should remain after clearing its pin");
        assert_eq!(route.pinned_provider_id, None);
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );
        Ok(())
    }

    #[test]
    fn provider_and_session_cleanup_have_distinct_semantics() -> Result<(), AppError> {
        let db = Database::memory()?;
        db.set_codex_session_pin(SESSION_ID, "provider-b")?;
        db.set_codex_session_last_successful_provider(SESSION_ID, "provider-a")?;

        assert_eq!(db.clear_codex_session_pins_for_provider("provider-b")?, 1);
        let route = db
            .get_codex_session_route(SESSION_ID)?
            .expect("provider cleanup should preserve the route");
        assert_eq!(route.pinned_provider_id, None);
        assert_eq!(
            route.last_successful_provider_id.as_deref(),
            Some("provider-a")
        );

        assert!(db.delete_codex_session_route(SESSION_ID)?);
        assert!(!db.delete_codex_session_route(SESSION_ID)?);
        assert_eq!(db.get_codex_session_route(SESSION_ID)?, None);
        Ok(())
    }

    #[test]
    fn session_ids_are_canonicalized_and_invalid_ids_never_write() -> Result<(), AppError> {
        let db = Database::memory()?;
        let alternate = "550E8400E29B41D4A716446655440000";

        db.set_codex_session_pin(alternate, "provider-a")?;
        let route = db
            .get_codex_session_route(SESSION_ID)?
            .expect("canonical route should exist");
        assert_eq!(route.session_id, SESSION_ID);

        assert!(db
            .set_codex_session_pin("codex_550e8400-e29b-41d4-a716-446655440001", "provider-b")
            .is_err());
        assert!(db
            .set_codex_session_last_successful_provider("not-a-uuid", "provider-b")
            .is_err());
        assert!(db.get_codex_session_route("").is_err());

        let count: i64 = {
            let conn = lock_conn!(db.conn);
            conn.query_row("SELECT COUNT(*) FROM codex_session_routes", [], |row| {
                row.get(0)
            })?
        };
        assert_eq!(count, 1, "invalid ids must not add route rows");
        Ok(())
    }
}
