//! API tokens for the JSON API.

use jiff::{SignedDuration, Timestamp};
use uptime_domain::TokenScope;

use crate::{Result, Store, auth::hex, convert, models::ApiTokenRecord};

/// How often `last_used_at` is written, at most (a busy client should not
/// write on every request).
const TOUCH_EVERY: SignedDuration = SignedDuration::from_mins(5);

/// A token to record after it was issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewApiToken {
    pub name: String,
    /// SHA-256 of the token (never the token itself).
    pub token_hash: [u8; 32],
    pub scope: TokenScope,
    pub created_by: String,
}

/// A stored token (without its secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiToken {
    pub id: i64,
    pub name: String,
    pub scope: TokenScope,
    pub created_by: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

fn api_token(record: ApiTokenRecord) -> Result<ApiToken> {
    Ok(ApiToken {
        id: record.id,
        scope: convert::parse("api_tokens.scope", &record.scope)?,
        name: record.name,
        created_by: record.created_by,
        created_at: record.created_at,
        last_used_at: record.last_used_at,
    })
}

impl Store {
    pub async fn create_api_token(&self, token: &NewApiToken, now: Timestamp) -> Result<ApiToken> {
        let record = toasty::create!(ApiTokenRecord {
            name: token.name.trim(),
            token_hash: hex(&token.token_hash),
            scope: token.scope.as_str(),
            created_by: token.created_by.as_str(),
            created_at: now,
        })
        .exec(&mut self.db())
        .await?;
        api_token(record)
    }

    /// The token with this hash. Records its use (at most every few minutes).
    pub async fn authenticate_api_token(
        &self,
        token_hash: &[u8; 32],
        now: Timestamp,
    ) -> Result<Option<ApiToken>> {
        let Some(record) = ApiTokenRecord::filter_by_token_hash(hex(token_hash))
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        let stale = record
            .last_used_at
            .is_none_or(|at| now.duration_since(at) >= TOUCH_EVERY);
        if stale {
            ApiTokenRecord::update_by_id(record.id)
                .last_used_at(Some(now))
                .exec(&mut self.db())
                .await?;
        }
        api_token(record).map(Some)
    }

    /// Every token, newest first.
    pub async fn list_api_tokens(&self) -> Result<Vec<ApiToken>> {
        let mut tokens = ApiTokenRecord::all()
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(api_token)
            .collect::<Result<Vec<_>>>()?;
        tokens.sort_by_key(|t| std::cmp::Reverse((t.created_at, t.id)));
        Ok(tokens)
    }

    /// Revokes a token. Returns whether it existed.
    pub async fn delete_api_token(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "api_tokens" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }
}
