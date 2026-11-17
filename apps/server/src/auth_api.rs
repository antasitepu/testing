//! Login endpoints, the admin user API, and the middleware that keeps every other
//! API route behind a session. Without an `AuthService` (local loopback use) the
//! middleware is not installed and `/api/v1/auth/session` reports that no login is needed.
use crate::{
    auth::{
        AuthError, AuthService, DEFAULT_DEVICE_LIMIT, Role, SESSION_LIFETIME_SECONDS, SessionDto,
        SessionUser, UserDto, UserPatch,
    },
    dto::ApiErrorDto,
    state::AppState,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const SESSION_COOKIE: &str = "gm_session";
const LOGIN_PATH: &str = "/api/v1/auth/login";
const SESSION_PATH: &str = "/api/v1/auth/session";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(LOGIN_PATH, post(login))
        .route(SESSION_PATH, get(session))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/password", post(change_password))
        .route("/api/v1/admin/accounts", get(list_users).post(create_user))
        .route(
            "/api/v1/admin/accounts/{user_id}",
            patch(update_user).delete(delete_user),
        )
        .route(
            "/api/v1/admin/accounts/{user_id}/sessions",
            get(list_sessions),
        )
        .route(
            "/api/v1/admin/sessions/{session_id}",
            delete(revoke_session),
        )
}

/// Static files and the liveness probes stay open; the page itself asks for a login.
fn needs_session(path: &str) -> bool {
    (path.starts_with("/api/") && path != LOGIN_PATH && path != SESSION_PATH)
        || path == "/health/diagnostics"
}

pub async fn require_session(
    State(auth): State<Arc<AuthService>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let user = match session_token(request.headers()) {
        Some(token) => match auth.authenticate(&token).await {
            Ok(user) => user,
            Err(error) => return ApiError::from(error).into_response(),
        },
        None => None,
    };
    if needs_session(&path) {
        let Some(user) = &user else {
            return ApiError::unauthenticated().into_response();
        };
        if path.starts_with("/api/v1/admin/") && user.role != Role::Admin {
            return ApiError::new(StatusCode::FORBIDDEN, "FORBIDDEN", "admin only").into_response();
        }
    }
    if let Some(user) = user {
        request.extensions_mut().insert(user);
    }
    next.run(request).await
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_owned())
}

fn session_cookie(token: &str, max_age: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
    ))
    .expect("session cookie is ASCII")
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &str) -> Self {
        Self {
            status,
            code,
            message: message.to_owned(),
        }
    }

    fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "login required",
        )
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let (status, code) = match &error {
            AuthError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS"),
            AuthError::TooManyAttempts => (StatusCode::TOO_MANY_REQUESTS, "TOO_MANY_ATTEMPTS"),
            AuthError::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
            AuthError::UsernameTaken => (StatusCode::CONFLICT, "USERNAME_TAKEN"),
            AuthError::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            AuthError::LastAdmin => (StatusCode::CONFLICT, "LAST_ADMIN"),
            AuthError::Storage(_) | AuthError::Hashing => {
                tracing::error!(%error, "auth request failed");
                return Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "AUTH_UNAVAILABLE",
                    "login service is unavailable",
                );
            }
        };
        Self::new(status, code, &error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorDto {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

fn service(state: &AppState) -> Result<&Arc<AuthService>, ApiError> {
    state.auth.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "AUTH_DISABLED",
            "login is not enabled",
        )
    })
}

fn current(user: Option<Extension<SessionUser>>) -> Result<SessionUser, ApiError> {
    user.map(|Extension(user)| user)
        .ok_or_else(ApiError::unauthenticated)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionStateDto {
    auth_required: bool,
    username: Option<String>,
    role: Option<Role>,
}

impl SessionStateDto {
    fn signed_in(user: &SessionUser) -> Self {
        Self {
            auth_required: true,
            username: Some(user.username.clone()),
            role: Some(user.role),
        }
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let auth = service(&state)?;
    let header_text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let (token, user) = auth
        .login(
            &request.username,
            &request.password,
            header_text("user-agent").unwrap_or(""),
            header_text("x-real-ip"),
        )
        .await?;
    let mut response = Json(SessionStateDto::signed_in(&user)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&token, SESSION_LIFETIME_SECONDS, auth.secure_cookie()),
    );
    Ok(response)
}

async fn session(
    State(state): State<AppState>,
    user: Option<Extension<SessionUser>>,
) -> Result<Json<SessionStateDto>, ApiError> {
    if state.auth.is_none() {
        return Ok(Json(SessionStateDto {
            auth_required: false,
            username: None,
            role: None,
        }));
    }
    user.map(|Extension(user)| Json(SessionStateDto::signed_in(&user)))
        .ok_or_else(ApiError::unauthenticated)
}

async fn logout(
    State(state): State<AppState>,
    user: Option<Extension<SessionUser>>,
) -> Result<Response, ApiError> {
    let user = current(user)?;
    let auth = service(&state)?;
    auth.logout(&user).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie("", 0, auth.secure_cookie()),
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    user: Option<Extension<SessionUser>>,
    Json(request): Json<ChangePasswordRequest>,
) -> Result<StatusCode, ApiError> {
    let user = current(user)?;
    service(&state)?
        .change_own_password(&user, &request.current_password, &request.new_password)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_users(State(state): State<AppState>) -> Result<Json<Vec<UserDto>>, ApiError> {
    Ok(Json(service(&state)?.list_users().await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateUserRequest {
    username: String,
    password: String,
    role: Option<String>,
    device_limit: Option<i16>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreatedUserDto {
    user_id: Uuid,
}

async fn create_user(
    State(state): State<AppState>,
    admin: Option<Extension<SessionUser>>,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreatedUserDto>), ApiError> {
    let admin = current(admin)?;
    let role = request
        .role
        .as_deref()
        .map_or(Ok(Role::User), Role::parse)?;
    let user_id = service(&state)?
        .create_user(
            &request.username,
            &request.password,
            role,
            request.device_limit.unwrap_or(DEFAULT_DEVICE_LIMIT),
            &admin.username,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(CreatedUserDto { user_id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateUserRequest {
    role: Option<String>,
    device_limit: Option<i16>,
    disabled: Option<bool>,
    password: Option<String>,
}

async fn update_user(
    State(state): State<AppState>,
    admin: Option<Extension<SessionUser>>,
    Path(user_id): Path<Uuid>,
    Json(request): Json<UpdateUserRequest>,
) -> Result<StatusCode, ApiError> {
    let admin = current(admin)?;
    let patch = UserPatch {
        role: request.role.as_deref().map(Role::parse).transpose()?,
        device_limit: request.device_limit,
        disabled: request.disabled,
        password: request.password,
    };
    service(&state)?
        .update_user(user_id, patch, &admin.username)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(
    State(state): State<AppState>,
    admin: Option<Extension<SessionUser>>,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let admin = current(admin)?;
    service(&state)?
        .delete_user(user_id, &admin.username)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<Vec<SessionDto>>, ApiError> {
    Ok(Json(service(&state)?.list_sessions(user_id).await?))
}

async fn revoke_session(
    State(state): State<AppState>,
    admin: Option<Extension<SessionUser>>,
    Path(session_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let admin = current(admin)?;
    service(&state)?
        .revoke_session(session_id, &admin.username)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{needs_session, session_cookie, session_token};
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn api_and_diagnostics_need_a_session_but_login_and_static_do_not() {
        for path in [
            "/api/v1/candles",
            "/api/v1/admin/accounts",
            "/health/diagnostics",
        ] {
            assert!(needs_session(path), "{path}");
        }
        for path in [
            "/",
            "/assets/app.js",
            "/health/ready",
            "/api/v1/auth/login",
            "/api/v1/auth/session",
        ] {
            assert!(!needs_session(path), "{path}");
        }
    }

    #[test]
    fn session_token_is_read_from_any_cookie_position() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; gm_session=abc123; other=1"),
        );
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
        assert_eq!(session_token(&HeaderMap::new()), None);
    }

    #[test]
    fn cookie_is_http_only_strict_and_secure_on_a_public_host() {
        let cookie = session_cookie("abc", 86_400, true);
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with("gm_session=abc; Path=/; Max-Age=86400"));
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(cookie.ends_with("; Secure"));
        assert!(
            !session_cookie("abc", 0, false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
    }
}
