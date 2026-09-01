//! Who is asking, and are they allowed to.
//!
//! The tool this replaces had no answer to either question. It ran with
//! ambient authority: whoever could reach the box could do anything, and
//! nothing was written down afterwards. Every surface here goes through the
//! same two steps instead, so the web UI, the API, MCP and eventually the
//! in-game panel all get the same answer to the same question.
//!
//! Tokens are `rcmt_` followed by a prefix and a secret. Only the sha256 is
//! stored, so the database is not a list of working credentials, and the
//! prefix is kept in the clear purely so you can tell two of your own
//! tokens apart in the UI.

use std::fmt;

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{AnyPool, Row};

use crate::store::Database;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("database error: {0}")]
    Database(String),
    #[error("no token was presented")]
    Missing,
    #[error("that token is not valid")]
    Invalid,
    #[error("that token has been revoked")]
    Revoked,
}

/// What a caller is allowed to do. Ordered, so a check is a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read anything, change nothing.
    Viewer,
    /// Everything a viewer can do, plus running servers and builds.
    Operator,
    /// Everything, plus handing out tokens.
    Admin,
}

impl Role {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a request came in from. Not a permission by itself, but it is what
/// the audit log needs to be worth reading, and some rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Web,
    Api,
    Mcp,
    InGame,
    /// The scheduler acting on its own. Still audited.
    System,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Web => "web",
            Surface::Api => "api",
            Surface::Mcp => "mcp",
            Surface::InGame => "ingame",
            Surface::System => "system",
        }
    }
}

/// Everything the policy layer needs to know about a caller.
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub role: Role,
    pub surface: Surface,
    /// Set when the request came from a linked Minecraft account.
    pub player_uuid: Option<String>,
}

impl Actor {
    /// The scheduler. It is an admin because nobody is there to approve
    /// anything, and every action it takes is written to the audit log
    /// under this name so a surprise at 3am has an author.
    pub fn system() -> Self {
        Self {
            name: "system".into(),
            role: Role::Admin,
            surface: Surface::System,
            player_uuid: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// The readable half, so you can tell your tokens apart.
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A random lowercase hex string. Used for token secrets and row ids.
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill(&mut buffer[..]);
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The readable half of a token, for display. Never enough to authenticate.
fn prefix_of(token: &str) -> String {
    token.chars().take(13).collect()
}

pub struct Tokens<'a> {
    pool: &'a AnyPool,
}

impl<'a> Tokens<'a> {
    pub fn new(database: &'a Database) -> Self {
        Self {
            pool: database.pool(),
        }
    }

    /// Mints a token. The plaintext is returned exactly once and is not
    /// recoverable afterwards, which is the entire point.
    pub async fn create(&self, name: &str, role: Role) -> Result<(TokenRecord, String), AuthError> {
        let secret = format!("rcmt_{}", random_hex(24));
        let record = TokenRecord {
            id: random_hex(16),
            name: name.to_string(),
            role,
            prefix: prefix_of(&secret),
            created_at: Utc::now(),
            last_used_at: None,
            revoked_at: None,
        };
        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_sha256, token_prefix, role, created_at, last_used_at, revoked_at)
             VALUES (?, ?, ?, ?, ?, ?, NULL, NULL)",
        )
        .bind(&record.id)
        .bind(&record.name)
        .bind(sha256_hex(&secret))
        .bind(&record.prefix)
        .bind(role.as_str())
        .bind(record.created_at.to_rfc3339())
        .execute(self.pool)
        .await
        .map_err(|e| AuthError::Database(e.to_string()))?;
        Ok((record, secret))
    }

    pub async fn list(&self) -> Result<Vec<TokenRecord>, AuthError> {
        let rows = sqlx::query("SELECT * FROM api_tokens ORDER BY created_at ASC")
            .fetch_all(self.pool)
            .await
            .map_err(|e| AuthError::Database(e.to_string()))?;
        Ok(rows.iter().map(row_to_token).collect())
    }

    /// Whether anybody can log in at all. A fresh database has nobody, and
    /// the daemon mints a first admin rather than leaving you locked out.
    pub async fn any_usable(&self) -> Result<bool, AuthError> {
        Ok(self.list().await?.iter().any(|t| t.revoked_at.is_none()))
    }

    pub async fn revoke(&self, id: &str) -> Result<bool, AuthError> {
        let result =
            sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(Utc::now().to_rfc3339())
                .bind(id)
                .execute(self.pool)
                .await
                .map_err(|e| AuthError::Database(e.to_string()))?;
        Ok(result.rows_affected() > 0)
    }

    /// Turns a presented token into an actor, or says why not.
    pub async fn verify(&self, presented: &str, surface: Surface) -> Result<Actor, AuthError> {
        let presented = presented.trim();
        if presented.is_empty() {
            return Err(AuthError::Missing);
        }
        let row = sqlx::query("SELECT * FROM api_tokens WHERE token_sha256 = ?")
            .bind(sha256_hex(presented))
            .fetch_optional(self.pool)
            .await
            .map_err(|e| AuthError::Database(e.to_string()))?;
        let Some(row) = row else {
            return Err(AuthError::Invalid);
        };
        let record = row_to_token(&row);
        if record.revoked_at.is_some() {
            return Err(AuthError::Revoked);
        }

        // Best effort: a failed timestamp update must not fail the request.
        let _ = sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(&record.id)
            .execute(self.pool)
            .await;

        Ok(Actor {
            name: record.name,
            role: record.role,
            surface,
            player_uuid: None,
        })
    }
}

fn row_to_token(row: &sqlx::any::AnyRow) -> TokenRecord {
    let role: String = row.get("role");
    let created: String = row.get("created_at");
    let last_used: Option<String> = row.get("last_used_at");
    let revoked: Option<String> = row.get("revoked_at");
    TokenRecord {
        id: row.get("id"),
        name: row.get("name"),
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        // Older rows predate the column, so an absent prefix is not a bug.
        prefix: row
            .try_get::<Option<String>, _>("token_prefix")
            .ok()
            .flatten()
            .unwrap_or_else(|| "rcmt_...".to_string()),
        created_at: parse_time(&created).unwrap_or_else(Utc::now),
        last_used_at: last_used.as_deref().and_then(parse_time),
        revoked_at: revoked.as_deref().and_then(parse_time),
    }
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    text.parse::<DateTime<Utc>>().ok()
}

/// One line of the audit log.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: String,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub surface: String,
    pub action: String,
    pub target: Option<String>,
    pub decision: String,
    pub detail: Option<String>,
}

/// Writes what happened, allowed or not.
///
/// Denials are recorded too. A log that only contains successes tells you
/// what worked, not who has been trying things.
pub async fn audit(
    database: &Database,
    actor: &Actor,
    action: &str,
    target: Option<&str>,
    decision: &str,
    detail: Option<&str>,
) {
    let result = sqlx::query(
        "INSERT INTO audit_log (id, at, actor, surface, action, target, decision, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(random_hex(16))
    .bind(Utc::now().to_rfc3339())
    .bind(&actor.name)
    .bind(actor.surface.as_str())
    .bind(action)
    .bind(target)
    .bind(decision)
    .bind(detail)
    .execute(database.pool())
    .await;
    if let Err(e) = result {
        // An unwritable audit log is worth a loud line, but refusing the
        // action the operator asked for because the bookkeeping failed
        // would be a strange trade.
        tracing::error!("could not write the audit log: {e}");
    }
}

pub async fn recent_audit(database: &Database, limit: i64) -> Result<Vec<AuditEntry>, AuthError> {
    let rows = sqlx::query("SELECT * FROM audit_log ORDER BY at DESC LIMIT ?")
        .bind(limit.clamp(1, 1000))
        .fetch_all(database.pool())
        .await
        .map_err(|e| AuthError::Database(e.to_string()))?;
    Ok(rows
        .iter()
        .map(|row| {
            let at: String = row.get("at");
            AuditEntry {
                id: row.get("id"),
                at: parse_time(&at).unwrap_or_else(Utc::now),
                actor: row.get("actor"),
                surface: row.get("surface"),
                action: row.get("action"),
                target: row.get("target"),
                decision: row.get("decision"),
                detail: row.get("detail"),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn database() -> Database {
        Database::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn a_minted_token_authenticates_once_and_only_as_itself() {
        let db = database().await;
        let tokens = Tokens::new(&db);
        let (record, secret) = tokens.create("laptop", Role::Admin).await.unwrap();
        assert!(secret.starts_with("rcmt_"));

        let actor = tokens.verify(&secret, Surface::Api).await.unwrap();
        assert_eq!(actor.name, "laptop");
        assert_eq!(actor.role, Role::Admin);

        assert!(matches!(
            tokens.verify("rcmt_nonsense", Surface::Api).await,
            Err(AuthError::Invalid)
        ));
        assert!(matches!(
            tokens.verify("", Surface::Api).await,
            Err(AuthError::Missing)
        ));
        // The id is what the UI revokes by, so it has to come back.
        assert_eq!(tokens.list().await.unwrap()[0].id, record.id);
    }

    #[tokio::test]
    async fn the_plaintext_is_not_in_the_database() {
        // If this ever fails, the database has become a list of working
        // credentials and the whole scheme is pointless.
        let db = database().await;
        let tokens = Tokens::new(&db);
        let (_, secret) = tokens.create("laptop", Role::Operator).await.unwrap();
        let rows = sqlx::query("SELECT token_sha256 FROM api_tokens")
            .fetch_all(db.pool())
            .await
            .unwrap();
        let stored: String = rows[0].get("token_sha256");
        assert_ne!(stored, secret);
        assert_eq!(stored, sha256_hex(&secret));
    }

    #[tokio::test]
    async fn a_revoked_token_stops_working_and_stays_revoked() {
        let db = database().await;
        let tokens = Tokens::new(&db);
        let (record, secret) = tokens.create("old laptop", Role::Admin).await.unwrap();
        assert!(tokens.verify(&secret, Surface::Api).await.is_ok());

        assert!(tokens.revoke(&record.id).await.unwrap());
        assert!(matches!(
            tokens.verify(&secret, Surface::Api).await,
            Err(AuthError::Revoked)
        ));
        // Revoking twice is not an error, it just changes nothing.
        assert!(!tokens.revoke(&record.id).await.unwrap());
        assert!(!tokens.any_usable().await.unwrap());
    }

    #[tokio::test]
    async fn using_a_token_records_when() {
        let db = database().await;
        let tokens = Tokens::new(&db);
        let (_, secret) = tokens.create("laptop", Role::Viewer).await.unwrap();
        assert!(tokens.list().await.unwrap()[0].last_used_at.is_none());
        tokens.verify(&secret, Surface::Api).await.unwrap();
        assert!(tokens.list().await.unwrap()[0].last_used_at.is_some());
    }

    #[tokio::test]
    async fn denials_are_written_down_too() {
        let db = database().await;
        let actor = Actor {
            name: "someone".into(),
            role: Role::Viewer,
            surface: Surface::Web,
            player_uuid: None,
        };
        audit(
            &db,
            &actor,
            "server.restart",
            Some("hub"),
            "denied",
            Some("viewer"),
        )
        .await;
        let entries = recent_audit(&db, 10).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].decision, "denied");
        assert_eq!(entries[0].target.as_deref(), Some("hub"));
        assert_eq!(entries[0].surface, "web");
    }

    #[test]
    fn roles_compare_the_way_permissions_should() {
        assert!(Role::Admin > Role::Operator);
        assert!(Role::Operator > Role::Viewer);
        assert_eq!(Role::parse("ADMIN"), Some(Role::Admin));
        assert_eq!(Role::parse("root"), None);
    }
}
