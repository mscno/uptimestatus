//! Admin accounts and browser sessions.

use jiff::{SignedDuration, Timestamp};

use crate::{
    Result, Store,
    models::{AdminUserRecord, SessionRecord},
};

/// A GitHub account as reported by GitHub at login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubIdentity {
    pub github_id: i64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// An admin who has signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminUser {
    pub id: i64,
    pub github_id: i64,
    /// Lowercase GitHub username.
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// A session to record after login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    /// SHA-256 of the session token (never the token itself).
    pub token_hash: [u8; 32],
    pub user_id: i64,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub user_agent: Option<String>,
}

/// Bookkeeping for an active session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub last_seen_at: Timestamp,
}

impl Store {
    /// The GitHub id pinned to `login` by an earlier login, if any.
    pub async fn pinned_github_id(&self, login: &str) -> Result<Option<i64>> {
        let record = AdminUserRecord::filter_by_login(login.to_ascii_lowercase())
            .first()
            .exec(&mut self.db())
            .await?;
        Ok(record.map(|admin| admin.github_id))
    }

    /// Records a successful login: creates the admin on first login, then
    /// keeps login/name/avatar in sync with GitHub (following renames).
    #[tracing::instrument(skip_all, fields(github_id = identity.github_id))]
    pub async fn record_admin_login(
        &self,
        identity: &GithubIdentity,
        now: Timestamp,
    ) -> Result<AdminUser> {
        let login = identity.login.to_ascii_lowercase();
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let existing = AdminUserRecord::filter_by_github_id(identity.github_id)
            .first()
            .exec(&mut tx)
            .await?;
        let id = match existing {
            Some(admin) => {
                AdminUserRecord::update_by_id(admin.id)
                    .login(login.as_str())
                    .name(identity.name.clone())
                    .avatar_url(identity.avatar_url.clone())
                    .last_login_at(now)
                    .exec(&mut tx)
                    .await?;
                admin.id
            }
            None => {
                toasty::create!(AdminUserRecord {
                    github_id: identity.github_id,
                    login: login.as_str(),
                    name: identity.name.clone(),
                    avatar_url: identity.avatar_url.clone(),
                    last_login_at: now,
                })
                .exec(&mut tx)
                .await?
                .id
            }
        };
        tx.commit().await?;
        Ok(AdminUser {
            id,
            github_id: identity.github_id,
            login,
            name: identity.name.clone(),
            avatar_url: identity.avatar_url.clone(),
        })
    }

    pub async fn create_session(&self, session: &NewSession) -> Result<()> {
        toasty::create!(SessionRecord {
            token_hash: hex(&session.token_hash),
            user_id: session.user_id,
            created_at: session.created_at,
            expires_at: session.expires_at,
            last_seen_at: session.created_at,
            user_agent: session.user_agent.clone(),
        })
        .exec(&mut self.db())
        .await?;
        Ok(())
    }

    /// The admin behind an unexpired session no older than `max_age`.
    pub async fn session_admin(
        &self,
        token_hash: &[u8; 32],
        now: Timestamp,
        max_age: SignedDuration,
    ) -> Result<Option<(AdminUser, SessionInfo)>> {
        let mut db = self.db();
        let Some(session) = SessionRecord::filter_by_token_hash(hex(token_hash))
            .first()
            .exec(&mut db)
            .await?
        else {
            return Ok(None);
        };
        let too_old = session
            .created_at
            .checked_add(max_age)
            .is_ok_and(|limit| limit <= now);
        if session.expires_at <= now || too_old {
            return Ok(None);
        }
        let Some(admin) = AdminUserRecord::filter_by_id(session.user_id)
            .first()
            .exec(&mut db)
            .await?
        else {
            return Ok(None);
        };
        let info = SessionInfo {
            created_at: session.created_at,
            expires_at: session.expires_at,
            last_seen_at: session.last_seen_at,
        };
        Ok(Some((admin_user(admin), info)))
    }

    /// Slides a session's expiry (after the cookie was re-issued).
    pub async fn extend_session(
        &self,
        token_hash: &[u8; 32],
        expires_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        toasty::sql::statement(
            r#"UPDATE "sessions" SET "expires_at" = $2, "last_seen_at" = $3 WHERE "token_hash" = $1"#,
        )
        .bind(hex(token_hash))
        .bind(self.raw_timestamp(expires_at))
        .bind(self.raw_timestamp(now))
        .exec(&mut self.db())
        .await?;
        Ok(())
    }

    pub async fn delete_session(&self, token_hash: &[u8; 32]) -> Result<()> {
        toasty::sql::statement(r#"DELETE FROM "sessions" WHERE "token_hash" = $1"#)
            .bind(hex(token_hash))
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// Deletes expired sessions. Returns how many were removed.
    pub async fn prune_sessions(&self, now: Timestamp) -> Result<u64> {
        Ok(
            toasty::sql::statement(r#"DELETE FROM "sessions" WHERE "expires_at" <= $1"#)
                .bind(self.raw_timestamp(now))
                .exec(&mut self.db())
                .await?,
        )
    }
}

fn admin_user(record: AdminUserRecord) -> AdminUser {
    AdminUser {
        id: record.id,
        github_id: record.github_id,
        login: record.login,
        name: record.name,
        avatar_url: record.avatar_url,
    }
}

/// Lowercase hex, the storage form of token hashes.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
