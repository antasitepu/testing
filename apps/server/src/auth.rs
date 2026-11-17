//! Application login: users, one session row per logged-in device, and the rule
//! that a login beyond the user's device limit signs out the oldest device.
//! The cookie holds a random token; the database holds only its SHA-256.
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// A device stays signed in for one day after its login.
pub const SESSION_LIFETIME_SECONDS: i64 = 86_400;
pub const DEFAULT_DEVICE_LIMIT: i16 = 2;
const MAX_DEVICE_LIMIT: i16 = 20;
const MIN_PASSWORD_CHARS: usize = 10;
const MAX_PASSWORD_BYTES: usize = 128;
const MAX_USER_AGENT_CHARS: usize = 200;
const MAX_FAILED_LOGINS: u32 = 5;
const FAILED_LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("username or password is wrong")]
    InvalidCredentials,
    #[error("too many failed logins; try again later")]
    TooManyAttempts,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("username is already taken")]
    UsernameTaken,
    #[error("not found")]
    NotFound,
    #[error("at least one active admin must remain")]
    LastAdmin,
    #[error("auth storage failed: {0}")]
    Storage(#[from] sqlx::Error),
    #[error("password hashing failed")]
    Hashing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "ADMIN",
            Self::User => "USER",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AuthError> {
        match value {
            "ADMIN" => Ok(Self::Admin),
            "USER" => Ok(Self::User),
            _ => Err(AuthError::Invalid("role must be ADMIN or USER")),
        }
    }
}

/// The signed-in user behind one request.
#[derive(Clone, Debug)]
pub struct SessionUser {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    pub user_id: Uuid,
    pub username: String,
    pub role: Role,
    pub device_limit: i16,
    pub disabled: bool,
    pub active_sessions: i64,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    pub session_id: Uuid,
    pub user_agent: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Default)]
pub struct UserPatch {
    pub role: Option<Role>,
    pub device_limit: Option<i16>,
    pub disabled: Option<bool>,
    pub password: Option<String>,
}

pub struct AuthService {
    pool: PgPool,
    secure_cookie: bool,
    failed_logins: Mutex<HashMap<String, (u32, Instant)>>,
}

impl AuthService {
    pub fn new(pool: PgPool, secure_cookie: bool) -> Self {
        Self {
            pool,
            secure_cookie,
            failed_logins: Mutex::new(HashMap::new()),
        }
    }

    pub fn secure_cookie(&self) -> bool {
        self.secure_cookie
    }

    /// Returns the cookie token of the new session.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        user_agent: &str,
        client_ip: Option<&str>,
    ) -> Result<(String, SessionUser), AuthError> {
        let username = username.trim().to_ascii_lowercase();
        let attempt_key = format!("{username}|{}", client_ip.unwrap_or(""));
        if self.is_locked(&attempt_key) {
            self.record("LOGIN_BLOCKED", &username, None, client_ip)
                .await?;
            return Err(AuthError::TooManyAttempts);
        }
        let row = sqlx::query(
            "SELECT user_id, password_hash, role, device_limit FROM app_users \
             WHERE username = $1 AND disabled_at IS NULL",
        )
        .bind(&username)
        .fetch_optional(&self.pool)
        .await?;
        // An unknown user still costs one hash, so timing does not reveal who exists.
        let stored_hash = row
            .as_ref()
            .map(|row| row.get::<String, _>("password_hash"));
        let password_ok = verify_password(password.to_owned(), stored_hash).await?;
        let Some(row) = row.filter(|_| password_ok) else {
            self.note_failure(&attempt_key);
            self.record("LOGIN_FAILED", &username, None, client_ip)
                .await?;
            return Err(AuthError::InvalidCredentials);
        };
        self.clear_failures(&attempt_key);

        let user = SessionUser {
            session_id: Uuid::now_v7(),
            user_id: row.get("user_id"),
            username: username.clone(),
            role: Role::parse(row.get::<String, _>("role").as_str())?,
        };
        let device_limit: i16 = row.get("device_limit");
        let token = new_token();
        let user_agent: String = user_agent.chars().take(MAX_USER_AGENT_CHARS).collect();

        let mut transaction = self.pool.begin().await?;
        // Serialises concurrent logins of one user so the limit cannot be overshot.
        sqlx::query("SELECT 1 FROM app_users WHERE user_id = $1 FOR UPDATE")
            .bind(user.user_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM app_sessions WHERE user_id = $1 AND expires_at <= NOW()")
            .bind(user.user_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO app_sessions (session_id, user_id, token_hash, user_agent, expires_at) \
             VALUES ($1, $2, $3, $4, NOW() + make_interval(secs => $5))",
        )
        .bind(user.session_id)
        .bind(user.user_id)
        .bind(token_hash(&token))
        .bind(&user_agent)
        .bind(SESSION_LIFETIME_SECONDS as f64)
        .execute(&mut *transaction)
        .await?;
        let evicted = sqlx::query(
            "DELETE FROM app_sessions WHERE session_id IN ( \
                 SELECT session_id FROM app_sessions WHERE user_id = $1 \
                 ORDER BY created_at DESC, session_id DESC OFFSET $2)",
        )
        .bind(user.user_id)
        .bind(i64::from(device_limit))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        transaction.commit().await?;

        self.record("LOGIN_OK", &username, None, client_ip).await?;
        if evicted > 0 {
            self.record("SESSION_EVICTED", &username, None, client_ip)
                .await?;
        }
        Ok((token, user))
    }

    pub async fn authenticate(&self, token: &str) -> Result<Option<SessionUser>, AuthError> {
        let row = sqlx::query(
            "SELECT s.session_id, u.user_id, u.username, u.role, \
                    s.last_seen_at < NOW() - INTERVAL '1 minute' AS stale \
             FROM app_sessions s JOIN app_users u ON u.user_id = s.user_id \
             WHERE s.token_hash = $1 AND s.expires_at > NOW() AND u.disabled_at IS NULL",
        )
        .bind(token_hash(token))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_id: Uuid = row.get("session_id");
        if row.get::<bool, _>("stale") {
            sqlx::query("UPDATE app_sessions SET last_seen_at = NOW() WHERE session_id = $1")
                .bind(session_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(Some(SessionUser {
            session_id,
            user_id: row.get("user_id"),
            username: row.get("username"),
            role: Role::parse(row.get::<String, _>("role").as_str())?,
        }))
    }

    pub async fn logout(&self, user: &SessionUser) -> Result<(), AuthError> {
        sqlx::query("DELETE FROM app_sessions WHERE session_id = $1")
            .bind(user.session_id)
            .execute(&self.pool)
            .await?;
        self.record("LOGOUT", &user.username, None, None).await
    }

    /// Changes the caller's own password and signs out their other devices.
    pub async fn change_own_password(
        &self,
        user: &SessionUser,
        current: &str,
        new: &str,
    ) -> Result<(), AuthError> {
        validate_password(new)?;
        let stored: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM app_users WHERE user_id = $1")
                .bind(user.user_id)
                .fetch_optional(&self.pool)
                .await?;
        if stored.is_none() || !verify_password(current.to_owned(), stored).await? {
            return Err(AuthError::InvalidCredentials);
        }
        let hash = hash_password(new.to_owned()).await?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE app_users SET password_hash = $2, updated_at = NOW() WHERE user_id = $1",
        )
        .bind(user.user_id)
        .bind(hash)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM app_sessions WHERE user_id = $1 AND session_id <> $2")
            .bind(user.user_id)
            .bind(user.session_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        self.record("PASSWORD_CHANGED", &user.username, None, None)
            .await
    }

    pub async fn list_users(&self) -> Result<Vec<UserDto>, AuthError> {
        let rows = sqlx::query(
            "SELECT u.user_id, u.username, u.role, u.device_limit, \
                    u.disabled_at IS NOT NULL AS disabled, \
                    (EXTRACT(EPOCH FROM u.created_at) * 1000)::BIGINT AS created_at, \
                    (SELECT COUNT(*) FROM app_sessions s \
                     WHERE s.user_id = u.user_id AND s.expires_at > NOW()) AS active_sessions \
             FROM app_users u ORDER BY u.username",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(UserDto {
                    user_id: row.get("user_id"),
                    username: row.get("username"),
                    role: Role::parse(row.get::<String, _>("role").as_str())?,
                    device_limit: row.get("device_limit"),
                    disabled: row.get("disabled"),
                    active_sessions: row.get("active_sessions"),
                    created_at: row.get("created_at"),
                })
            })
            .collect()
    }

    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
        role: Role,
        device_limit: i16,
        actor: &str,
    ) -> Result<Uuid, AuthError> {
        let username = username.trim().to_ascii_lowercase();
        validate_username(&username)?;
        validate_password(password)?;
        validate_device_limit(device_limit)?;
        let hash = hash_password(password.to_owned()).await?;
        let user_id = Uuid::now_v7();
        let inserted = sqlx::query(
            "INSERT INTO app_users (user_id, username, password_hash, role, device_limit) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (username) DO NOTHING",
        )
        .bind(user_id)
        .bind(&username)
        .bind(hash)
        .bind(role.as_str())
        .bind(device_limit)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(AuthError::UsernameTaken);
        }
        self.record("USER_CREATED", &username, Some(actor), None)
            .await?;
        Ok(user_id)
    }

    /// A new password, a disable or a lower device limit takes effect on live sessions at once.
    pub async fn update_user(
        &self,
        user_id: Uuid,
        patch: UserPatch,
        actor: &str,
    ) -> Result<(), AuthError> {
        if let Some(limit) = patch.device_limit {
            validate_device_limit(limit)?;
        }
        let hash = match &patch.password {
            Some(password) => {
                validate_password(password)?;
                Some(hash_password(password.clone()).await?)
            }
            None => None,
        };
        let mut transaction = self.pool.begin().await?;
        // Every admin row is locked, so two admins cannot remove each other at once.
        sqlx::query("SELECT 1 FROM app_users WHERE role = 'ADMIN' FOR UPDATE")
            .execute(&mut *transaction)
            .await?;
        let row = sqlx::query(
            "UPDATE app_users SET \
                 role = COALESCE($2, role), \
                 device_limit = COALESCE($3, device_limit), \
                 disabled_at = CASE WHEN $4::BOOLEAN IS NULL THEN disabled_at \
                                    WHEN $4 THEN COALESCE(disabled_at, NOW()) ELSE NULL END, \
                 password_hash = COALESCE($5, password_hash), \
                 updated_at = NOW() \
             WHERE user_id = $1 RETURNING username, device_limit",
        )
        .bind(user_id)
        .bind(patch.role.map(Role::as_str))
        .bind(patch.device_limit)
        .bind(patch.disabled)
        .bind(hash.as_deref())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AuthError::NotFound)?;
        let username: String = row.get("username");
        let device_limit: i16 = row.get("device_limit");
        ensure_active_admin(&mut transaction).await?;
        if patch.disabled == Some(true) || hash.is_some() {
            sqlx::query("DELETE FROM app_sessions WHERE user_id = $1")
                .bind(user_id)
                .execute(&mut *transaction)
                .await?;
        } else {
            sqlx::query(
                "DELETE FROM app_sessions WHERE session_id IN ( \
                     SELECT session_id FROM app_sessions WHERE user_id = $1 \
                     ORDER BY created_at DESC, session_id DESC OFFSET $2)",
            )
            .bind(user_id)
            .bind(i64::from(device_limit))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.record("USER_UPDATED", &username, Some(actor), None)
            .await
    }

    pub async fn delete_user(&self, user_id: Uuid, actor: &str) -> Result<(), AuthError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM app_users WHERE role = 'ADMIN' FOR UPDATE")
            .execute(&mut *transaction)
            .await?;
        let username: String =
            sqlx::query_scalar("DELETE FROM app_users WHERE user_id = $1 RETURNING username")
                .bind(user_id)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AuthError::NotFound)?;
        ensure_active_admin(&mut transaction).await?;
        transaction.commit().await?;
        self.record("USER_DELETED", &username, Some(actor), None)
            .await
    }

    pub async fn list_sessions(&self, user_id: Uuid) -> Result<Vec<SessionDto>, AuthError> {
        let rows = sqlx::query(
            "SELECT session_id, user_agent, \
                    (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at, \
                    (EXTRACT(EPOCH FROM last_seen_at) * 1000)::BIGINT AS last_seen_at, \
                    (EXTRACT(EPOCH FROM expires_at) * 1000)::BIGINT AS expires_at \
             FROM app_sessions WHERE user_id = $1 AND expires_at > NOW() \
             ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| SessionDto {
                session_id: row.get("session_id"),
                user_agent: row.get("user_agent"),
                created_at: row.get("created_at"),
                last_seen_at: row.get("last_seen_at"),
                expires_at: row.get("expires_at"),
            })
            .collect())
    }

    pub async fn revoke_session(&self, session_id: Uuid, actor: &str) -> Result<(), AuthError> {
        let username: String = sqlx::query_scalar(
            "DELETE FROM app_sessions s USING app_users u \
             WHERE s.session_id = $1 AND u.user_id = s.user_id RETURNING u.username",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::NotFound)?;
        self.record("SESSION_REVOKED", &username, Some(actor), None)
            .await
    }

    async fn record(
        &self,
        event_type: &'static str,
        username: &str,
        actor: Option<&str>,
        client_ip: Option<&str>,
    ) -> Result<(), AuthError> {
        sqlx::query(
            "INSERT INTO auth_events (event_id, event_type, username, actor, client_ip) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(event_type)
        .bind(username.chars().take(64).collect::<String>())
        .bind(actor)
        .bind(client_ip)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn is_locked(&self, key: &str) -> bool {
        let mut attempts = self.failed_logins.lock().expect("login attempts lock");
        attempts.retain(|_, (_, first)| first.elapsed() < FAILED_LOGIN_WINDOW);
        attempts
            .get(key)
            .is_some_and(|(count, _)| *count >= MAX_FAILED_LOGINS)
    }

    fn note_failure(&self, key: &str) {
        let mut attempts = self.failed_logins.lock().expect("login attempts lock");
        attempts
            .entry(key.to_owned())
            .or_insert((0, Instant::now()))
            .0 += 1;
    }

    fn clear_failures(&self, key: &str) {
        self.failed_logins
            .lock()
            .expect("login attempts lock")
            .remove(key);
    }
}

async fn ensure_active_admin(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), AuthError> {
    let admins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_users WHERE role = 'ADMIN' AND disabled_at IS NULL",
    )
    .fetch_one(&mut **transaction)
    .await?;
    if admins == 0 {
        return Err(AuthError::LastAdmin);
    }
    Ok(())
}

fn validate_username(username: &str) -> Result<(), AuthError> {
    let valid = (3..=32).contains(&username.len())
        && username.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        });
    if valid {
        Ok(())
    } else {
        Err(AuthError::Invalid(
            "username must be 3-32 characters: a-z, 0-9, dot, underscore or dash",
        ))
    }
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.chars().count() < MIN_PASSWORD_CHARS || password.len() > MAX_PASSWORD_BYTES {
        return Err(AuthError::Invalid("password must be 10 to 128 characters"));
    }
    Ok(())
}

fn validate_device_limit(limit: i16) -> Result<(), AuthError> {
    if (1..=MAX_DEVICE_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(AuthError::Invalid("device limit must be 1 to 20"))
    }
}

fn new_token() -> String {
    use argon2::password_hash::rand_core::RngCore;
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

async fn hash_password(password: String) -> Result<String, AuthError> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| AuthError::Hashing)
    })
    .await
    .map_err(|_| AuthError::Hashing)?
}

/// Without a stored hash a throwaway one is checked, so the attempt costs the same.
async fn verify_password(password: String, stored: Option<String>) -> Result<bool, AuthError> {
    static ABSENT_USER_HASH: OnceLock<String> = OnceLock::new();
    tokio::task::spawn_blocking(move || {
        let known_user = stored.is_some();
        let stored = stored.unwrap_or_else(|| {
            ABSENT_USER_HASH
                .get_or_init(|| {
                    let salt = SaltString::generate(&mut OsRng);
                    Argon2::default()
                        .hash_password(new_token().as_bytes(), &salt)
                        .map(|hash| hash.to_string())
                        .unwrap_or_default()
                })
                .clone()
        });
        let Ok(parsed) = PasswordHash::new(&stored) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
            && known_user
    })
    .await
    .map_err(|_| AuthError::Hashing)
}

#[cfg(test)]
mod tests {
    use super::{validate_device_limit, validate_password, validate_username};

    #[test]
    fn usernames_are_lowercase_and_bounded() {
        assert!(validate_username("iren.s_1-a").is_ok());
        for bad in ["ab", "Iren", "a b c", "a@b.c", &"a".repeat(33)] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords_need_ten_characters_and_fit_the_hasher_bound() {
        assert!(validate_password("0123456789").is_ok());
        assert!(validate_password("012345678").is_err());
        assert!(validate_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn device_limit_is_between_one_and_twenty() {
        assert!(validate_device_limit(1).is_ok() && validate_device_limit(20).is_ok());
        assert!(validate_device_limit(0).is_err() && validate_device_limit(21).is_err());
    }
}
