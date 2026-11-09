//! Shared fixtures for the family integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::{DateTime, TimeZone, Utc};
use ryu_family::{
    api,
    authz::Caller,
    host::NotifyHost,
    state::{AppState, Config},
    store::FamilyStore,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

pub const TOKEN: &str = "test-token";

pub fn memory_state() -> AppState {
    AppState::new(
        FamilyStore::open_memory().expect("in-memory store"),
        Config::for_tests(TOKEN),
        None,
    )
}

pub fn app(state: &AppState) -> Router {
    api::app(state.clone())
}

pub fn managed_app(store: FamilyStore) -> Router {
    let mut config = Config::for_tests(TOKEN);
    config.managed_node = true;
    api::app(AppState::new(store, config, None))
}

/// One HTTP call through the full app (bearer gate + caller resolution).
pub async fn send(
    app: &Router,
    method: &str,
    path: &str,
    user: Option<&str>,
    org: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("{}{path}", api::MOUNT))
        .header("authorization", format!("Bearer {TOKEN}"));
    if let Some(user) = user {
        request = request.header(api::CALLER_USER_HEADER, user);
    }
    if let Some(org) = org {
        request = request.header(api::CALLER_ORG_HEADER, org);
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
    };
    (status, value)
}

/// A member of org `o1`.
pub async fn org_call(
    app: &Router,
    method: &str,
    path: &str,
    user: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send(app, method, path, Some(user), Some("o1"), body).await
}

pub fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("valid instant")
}

pub fn member(tenant: &str, user: &str) -> Caller {
    Caller::member(tenant, user)
}

/// A stand-in for Core's `notify.deliver` that records every call and answers
/// with a configurable status.
#[derive(Clone)]
pub struct NotifyStub {
    pub addr: SocketAddr,
    pub calls: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    pub status: Arc<AtomicU16>,
}

impl NotifyStub {
    pub async fn start() -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(200));
        let router = Router::new()
            .route("/api/host/capability/notify.deliver", post(record))
            .with_state((calls.clone(), status.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("stub server");
        });
        Self {
            addr,
            calls,
            status,
        }
    }

    pub fn host(&self) -> NotifyHost {
        NotifyHost::new(format!("http://{}", self.addr), "ext-token", "@ryu/family")
    }

    pub fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }

    pub async fn bodies(&self) -> Vec<Value> {
        self.calls
            .lock()
            .await
            .iter()
            .map(|(_, body)| body.clone())
            .collect()
    }

    pub async fn count(&self) -> usize {
        self.calls.lock().await.len()
    }

    pub async fn clear(&self) {
        self.calls.lock().await.clear();
    }
}

type StubState = (Arc<Mutex<Vec<(HeaderMap, Value)>>>, Arc<AtomicU16>);

async fn record(
    State((calls, status)): State<StubState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut calls = calls.lock().await;
    calls.push((headers, body));
    let status = StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap_or(StatusCode::OK);
    let id = format!("n-{}", calls.len());
    (
        status,
        Json(json!({ "ok": status.is_success(), "notification_id": id })),
    )
}

/// An app state whose sweep delivers to `stub`.
pub fn state_with_host(store: FamilyStore, stub: &NotifyStub) -> AppState {
    AppState::new(store, Config::for_tests(TOKEN), Some(stub.host()))
}
