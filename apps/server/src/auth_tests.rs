use crate::{
    auth::{AuthError, AuthService, Role, UserPatch},
    runtime::{TEST_DB_LOCK, build_router},
    state::AppState,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use market_service::MarketHub;
use market_storage::{CandleRepository, MIGRATOR, MarketRepository, SyncRepository};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{path::Path, sync::Arc};
use tokio::sync::OwnedMutexGuard;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

async fn test_pool() -> (PgPool, OwnedMutexGuard<()>) {
    let guard = TEST_DB_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    dotenvy::dotenv().ok();
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    MIGRATOR.run(&pool).await.expect("run migrations");
    sqlx::query("TRUNCATE auth_events, app_sessions, app_users")
        .execute(&pool)
        .await
        .expect("truncate auth tables");
    (pool, guard)
}

async fn user(auth: &AuthService, username: &str, role: Role, device_limit: i16) -> uuid::Uuid {
    auth.create_user(username, PASSWORD, role, device_limit, "test")
        .await
        .expect("create user")
}

async fn login(auth: &AuthService, username: &str) -> String {
    auth.login(username, PASSWORD, "test-agent", Some("10.0.0.5"))
        .await
        .expect("login")
        .0
}

#[tokio::test]
async fn login_beyond_the_device_limit_signs_out_the_oldest_device() {
    let (pool, _guard) = test_pool().await;
    let auth = AuthService::new(pool, false);
    let user_id = user(&auth, "trader", Role::User, 2).await;

    let first = login(&auth, "Trader").await;
    let second = login(&auth, "trader").await;
    let third = login(&auth, "trader").await;

    assert!(auth.authenticate(&first).await.unwrap().is_none());
    assert!(auth.authenticate(&second).await.unwrap().is_some());
    assert!(auth.authenticate(&third).await.unwrap().is_some());
    assert_eq!(auth.list_sessions(user_id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn wrong_passwords_lock_the_login_and_unknown_users_look_the_same() {
    let (pool, _guard) = test_pool().await;
    let auth = AuthService::new(pool, false);
    user(&auth, "trader", Role::User, 2).await;

    assert!(matches!(
        auth.login("nobody", PASSWORD, "", None).await,
        Err(AuthError::InvalidCredentials)
    ));
    for _ in 0..5 {
        assert!(matches!(
            auth.login("trader", "wrong password", "", Some("10.0.0.5"))
                .await,
            Err(AuthError::InvalidCredentials)
        ));
    }
    assert!(matches!(
        auth.login("trader", PASSWORD, "", Some("10.0.0.5")).await,
        Err(AuthError::TooManyAttempts)
    ));
    // Another address is not locked out by that one.
    assert!(
        auth.login("trader", PASSWORD, "", Some("127.0.0.1"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn expired_sessions_and_disabled_users_are_signed_out() {
    let (pool, _guard) = test_pool().await;
    let auth = AuthService::new(pool.clone(), false);
    let user_id = user(&auth, "trader", Role::User, 2).await;
    user(&auth, "owner", Role::Admin, 2).await;

    let expired = login(&auth, "trader").await;
    sqlx::query("UPDATE app_sessions SET expires_at = NOW() - INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(auth.authenticate(&expired).await.unwrap().is_none());

    let live = login(&auth, "trader").await;
    let disable = UserPatch {
        disabled: Some(true),
        ..UserPatch::default()
    };
    auth.update_user(user_id, disable, "owner").await.unwrap();
    assert!(auth.authenticate(&live).await.unwrap().is_none());
    assert!(matches!(
        auth.login("trader", PASSWORD, "", None).await,
        Err(AuthError::InvalidCredentials)
    ));
}

#[tokio::test]
async fn lowering_the_limit_or_resetting_the_password_acts_on_live_sessions() {
    let (pool, _guard) = test_pool().await;
    let auth = AuthService::new(pool, false);
    let user_id = user(&auth, "trader", Role::User, 3).await;
    user(&auth, "owner", Role::Admin, 2).await;
    let oldest = login(&auth, "trader").await;
    let newest = login(&auth, "trader").await;

    let lower = UserPatch {
        device_limit: Some(1),
        ..UserPatch::default()
    };
    auth.update_user(user_id, lower, "owner").await.unwrap();
    assert!(auth.authenticate(&oldest).await.unwrap().is_none());
    assert!(auth.authenticate(&newest).await.unwrap().is_some());

    let reset = UserPatch {
        password: Some("a brand new password".to_owned()),
        ..UserPatch::default()
    };
    auth.update_user(user_id, reset, "owner").await.unwrap();
    assert!(auth.authenticate(&newest).await.unwrap().is_none());
    assert!(
        auth.login("trader", "a brand new password", "", None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_last_active_admin_cannot_be_removed_disabled_or_demoted() {
    let (pool, _guard) = test_pool().await;
    let auth = AuthService::new(pool, false);
    let owner = user(&auth, "owner", Role::Admin, 2).await;

    assert!(matches!(
        auth.delete_user(owner, "owner").await,
        Err(AuthError::LastAdmin)
    ));
    for patch in [
        UserPatch {
            disabled: Some(true),
            ..UserPatch::default()
        },
        UserPatch {
            role: Some(Role::User),
            ..UserPatch::default()
        },
    ] {
        assert!(matches!(
            auth.update_user(owner, patch, "owner").await,
            Err(AuthError::LastAdmin)
        ));
    }
    assert!(matches!(
        auth.create_user("owner", PASSWORD, Role::User, 2, "owner")
            .await,
        Err(AuthError::UsernameTaken)
    ));

    let second = user(&auth, "second", Role::Admin, 2).await;
    auth.delete_user(owner, "second").await.unwrap();
    assert_eq!(auth.list_users().await.unwrap().len(), 1);
    assert!(matches!(
        auth.delete_user(second, "second").await,
        Err(AuthError::LastAdmin)
    ));
}

fn app(pool: &PgPool, auth: Arc<AuthService>) -> Router {
    let state = AppState::new(
        Arc::new(MarketRepository::new(pool.clone())),
        Arc::new(CandleRepository::new(pool.clone())),
        Arc::new(SyncRepository::new(pool.clone())),
        Arc::new(MarketHub::new(8)),
        100,
    )
    .with_auth(auth);
    build_router(state, Path::new("missing-static-dir"), None)
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, cookie, body)
}

fn json_request(method: &str, uri: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn sign_in(app: &Router, username: &str) -> String {
    let (status, cookie, _) = send(
        app,
        json_request(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({"username": username, "password": PASSWORD}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cookie = cookie.expect("session cookie");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    cookie.split(';').next().unwrap().to_owned()
}

#[tokio::test]
async fn api_needs_a_session_and_the_admin_api_needs_an_admin() {
    let (pool, _guard) = test_pool().await;
    let auth = Arc::new(AuthService::new(pool.clone(), false));
    user(&auth, "owner", Role::Admin, 2).await;
    user(&auth, "trader", Role::User, 2).await;
    let app = app(&pool, auth);
    let get = |uri: &str, cookie: Option<&str>| json_request("GET", uri, cookie, Value::Null);

    let (status, _, body) = send(&app, get("/api/v1/markets", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "UNAUTHENTICATED");
    let (status, _, _) = send(&app, get("/api/v1/auth/session", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&app, get("/health/diagnostics", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&app, get("/health/live", None)).await;
    assert_eq!(status, StatusCode::OK);

    let trader = sign_in(&app, "trader").await;
    let (status, _, _) = send(&app, get("/api/v1/markets", Some(&trader))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = send(&app, get("/api/v1/auth/session", Some(&trader))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"authRequired": true, "username": "trader", "role": "USER"})
    );
    let (status, _, _) = send(&app, get("/api/v1/admin/accounts", Some(&trader))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let owner = sign_in(&app, "owner").await;
    let (status, _, body) = send(&app, get("/api/v1/admin/accounts", Some(&owner))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert!(body[0].get("passwordHash").is_none());
    let (status, _, _) = send(
        &app,
        json_request(
            "POST",
            "/api/v1/admin/accounts",
            Some(&owner),
            json!({"username": "guest", "password": PASSWORD, "deviceLimit": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, cookie, _) = send(
        &app,
        json_request("POST", "/api/v1/auth/logout", Some(&trader), Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(cookie.unwrap().contains("Max-Age=0"));
    let (status, _, _) = send(&app, get("/api/v1/markets", Some(&trader))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_auth_the_api_stays_open_and_says_no_login_is_needed() {
    let (pool, _guard) = test_pool().await;
    let state = AppState::new(
        Arc::new(MarketRepository::new(pool.clone())),
        Arc::new(CandleRepository::new(pool.clone())),
        Arc::new(SyncRepository::new(pool.clone())),
        Arc::new(MarketHub::new(8)),
        100,
    );
    let app = build_router(state, Path::new("missing-static-dir"), None);

    let (status, _, body) = send(
        &app,
        json_request("GET", "/api/v1/auth/session", None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["authRequired"], false);
    let (status, _, _) = send(
        &app,
        json_request("GET", "/api/v1/markets", None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
