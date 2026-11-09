//! HTTP lane (`/api/family/*`, reached through Core's ext-proxy).
//!
//! The caller is resolved once, in [`attach_caller`], from the verified headers
//! Core injects on the ext-proxy hop. User and tenant are never read from a
//! body, a query string or tool arguments.

use std::sync::Arc;

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        Path, Query, Request, State,
    },
    http::{HeaderMap, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    Extension, Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    authz::{Caller, LOCAL_USER},
    models::*,
    state::AppState,
};

pub const MOUNT: &str = "/api/family";
pub const CALLER_USER_HEADER: &str = "x-ryu-caller-user-id";
pub const CALLER_ORG_HEADER: &str = "x-ryu-caller-org-id";
const MAX_HEADER_BYTES: usize = 256;

/// Every distinct path this sidecar serves, in first-appearance order (the
/// manifest declares one route row per method on these paths).
pub const SERVED_ROUTES: &[&str] = &[
    "/upcoming",
    "/people",
    "/people/:id",
    "/people/:id/claim",
    "/dates",
    "/dates/:id",
    "/lists",
    "/lists/:id",
    "/lists/:id/items",
    "/items/:id",
    "/items/:id/complete",
    "/chores/tally",
    "/preferences",
    "/preferences/:id",
    "/wishlist",
    "/wishlist/:id",
    "/wishlist/:id/claim",
    "/gift-ideas",
    "/gift-ideas/:id",
    "/gift-context",
    "/settings",
    "/settings/me",
];

// ── errors ────────────────────────────────────────────────────────────────────

impl FamilyError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::AgentNotConnected | Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for FamilyError {
    fn into_response(self) -> Response {
        if let Self::Internal(error) = &self {
            tracing::error!(error = %format!("{error:#}"), "family request failed");
        }
        (
            self.status(),
            Json(json!({ "error": self.to_string(), "code": self.code() })),
        )
            .into_response()
    }
}

fn tenant_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(
            json!({ "error": "A verified family member is required.", "code": "tenant_required" }),
        ),
    )
        .into_response()
}

type ApiResult = Result<Json<Value>, FamilyError>;

fn body<T: DeserializeOwned>(input: Result<Json<T>, JsonRejection>) -> Result<T, FamilyError> {
    input
        .map(|Json(value)| value)
        .map_err(|rejection| FamilyError::BadRequest(rejection.body_text()))
}

fn query<T: DeserializeOwned>(input: Result<Query<T>, QueryRejection>) -> Result<T, FamilyError> {
    input
        .map(|Query(value)| value)
        .map_err(|rejection| FamilyError::BadRequest(rejection.body_text()))
}

fn to_json<T: Serialize>(value: &T) -> ApiResult {
    Ok(Json(serde_json::to_value(value)?))
}

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

// ── caller resolution ─────────────────────────────────────────────────────────

/// The caller could not be resolved from Core's verified headers (403
/// `tenant_required`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantRequired;

fn server_header(headers: &HeaderMap, name: &str) -> Result<Option<String>, TenantRequired> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| TenantRequired)?.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_HEADER_BYTES || value.chars().any(char::is_control) {
        return Err(TenantRequired);
    }
    Ok(Some(value.to_owned()))
}

/// Resolve the HTTP caller from Core's verified headers (§3.1 of the contract).
pub fn resolve_caller(headers: &HeaderMap, managed_node: bool) -> Result<Caller, TenantRequired> {
    let user = server_header(headers, CALLER_USER_HEADER)?;
    let org = server_header(headers, CALLER_ORG_HEADER)?;
    match (user, org) {
        (None, Some(_)) => Err(TenantRequired),
        (Some(user), Some(org)) => Ok(Caller::member(format!("org:{org}"), user)),
        (Some(user), None) => Ok(Caller::member(format!("user:{user}"), user)),
        (None, None) if managed_node => Err(TenantRequired),
        (None, None) => Ok(Caller::member(LOCAL_USER, LOCAL_USER)),
    }
}

async fn attach_caller(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Ok(caller) = resolve_caller(request.headers(), state.config.managed_node) else {
        return tenant_required();
    };
    if let Err(error) = state
        .store
        .ensure_settings(&caller.tenant, caller.actor())
        .await
    {
        tracing::error!(error = %error, "family: could not prepare tenant settings");
        return FamilyError::Unavailable.into_response();
    }
    request.extensions_mut().insert(caller);
    next.run(request).await
}

async fn bearer_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let provided = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    if ryu_sidecar_runtime::token_ok(provided.as_deref(), state.config.token.as_deref()) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn health(State(state): State<AppState>) -> Response {
    match state.store.ping().await {
        Ok(()) => Json(json!({ "ok": true, "sealing": state.store.seal_health() })).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "family health check failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// The whole HTTP app: `/health` outside the bearer gate, everything under
/// [`MOUNT`] behind it, and the caller resolved after the gate.
pub fn app(state: AppState) -> Router {
    let gated = Router::new()
        .nest(MOUNT, routes(state.clone()))
        .layer(from_fn_with_state(state.clone(), bearer_gate));
    Router::new()
        .route("/health", get(health))
        .with_state(state)
        .merge(gated)
}

/// The loopback route the keyless `mcp` process calls through the sidecar
/// handoff (see [`crate::sidecar_link`]). Outside [`MOUNT`]: Core never proxies
/// it, and only the per-run handoff token opens it.
pub const HANDOFF_MCP_ROUTE: &str = "/_handoff/mcp/call";

/// [`app`] plus the handoff route for the `mcp` process, opened only by
/// `handoff_token`.
pub fn app_with_handoff(state: AppState, handoff_token: String) -> Router {
    let token = Arc::new(handoff_token);
    let handoff = Router::new()
        .route(HANDOFF_MCP_ROUTE, axum::routing::post(handoff_mcp_call))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(
            move |request: Request, next: Next| {
                let token = token.clone();
                async move {
                    let provided = request
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .map(str::to_owned);
                    if ryu_sidecar_runtime::token_ok(provided.as_deref(), Some(token.as_str())) {
                        next.run(request).await
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }
            },
        ));
    app(state).merge(handoff)
}

/// One MCP `tools/call` run inside the sidecar (which holds the sealing key);
/// the body is `{name, arguments}`, the response the tool result.
async fn handoff_mcp_call(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = body
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = body.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Json(crate::mcp::call_result(&state, &name, &args).await).into_response()
}

/// The routes under [`MOUNT`], with caller resolution applied.
pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/upcoming", get(upcoming))
        .route("/people", get(list_people).post(create_person))
        .route("/people/:id", patch(update_person).delete(delete_person))
        .route("/people/:id/claim", post(claim_person))
        .route("/dates", get(list_dates).post(create_date))
        .route("/dates/:id", patch(update_date).delete(delete_date))
        .route("/lists", get(list_lists).post(create_list))
        .route("/lists/:id", patch(update_list).delete(delete_list))
        .route("/lists/:id/items", get(list_items).post(add_items))
        .route("/items/:id", patch(update_item).delete(delete_item))
        .route("/items/:id/complete", post(complete_item))
        .route("/chores/tally", get(tally))
        .route(
            "/preferences",
            get(list_preferences).post(create_preference),
        )
        .route(
            "/preferences/:id",
            patch(update_preference).delete(delete_preference),
        )
        .route("/wishlist", get(list_wishlist).post(create_wish))
        .route("/wishlist/:id", patch(update_wish).delete(delete_wish))
        .route("/wishlist/:id/claim", post(claim_wish).delete(unclaim_wish))
        .route("/gift-ideas", get(list_gifts).post(create_gift))
        .route("/gift-ideas/:id", patch(update_gift).delete(delete_gift))
        .route("/gift-context", get(gift_context))
        .route("/settings", get(get_settings).patch(patch_settings))
        .route("/settings/me", put(put_me))
        .layer(from_fn_with_state(state.clone(), attach_caller))
        .with_state(state)
}

// ── query shapes ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaysQuery {
    days: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonQuery {
    person_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KindQuery {
    kind: Option<ListKind>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsQuery {
    include_done: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TallyQuery {
    list_id: Option<String>,
    since: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GiftsQuery {
    person_id: Option<String>,
    status: Option<GiftStatus>,
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn upcoming(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<DaysQuery>, QueryRejection>,
) -> ApiResult {
    let params = query(params)?;
    to_json(&state.store.upcoming(&caller, params.days).await?)
}

async fn list_people(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult {
    let people = state.store.list_people(&caller).await?;
    Ok(Json(json!({ "people": people })))
}

async fn create_person(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewPerson>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_person(&caller, body(input)?).await?)
}

async fn update_person(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<PersonPatch>, JsonRejection>,
) -> ApiResult {
    to_json(
        &state
            .store
            .update_person(&caller, &id, body(input)?)
            .await?,
    )
}

async fn delete_person(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_person(&caller, &id).await?;
    ok()
}

async fn claim_person(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    to_json(&state.store.claim_person(&caller, &id).await?)
}

async fn list_dates(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<PersonQuery>, QueryRejection>,
) -> ApiResult {
    let person_id = non_empty(query(params)?.person_id);
    let dates = state
        .store
        .list_dates(&caller, person_id.as_deref())
        .await?;
    Ok(Json(json!({ "dates": dates })))
}

async fn create_date(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewDate>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_date(&caller, body(input)?).await?)
}

async fn update_date(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<DatePatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.update_date(&caller, &id, body(input)?).await?)
}

async fn delete_date(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_date(&caller, &id).await?;
    ok()
}

async fn list_lists(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<KindQuery>, QueryRejection>,
) -> ApiResult {
    let lists = state.store.list_lists(&caller, query(params)?.kind).await?;
    Ok(Json(json!({ "lists": lists })))
}

async fn create_list(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewList>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_list(&caller, body(input)?).await?)
}

async fn update_list(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<ListPatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.update_list(&caller, &id, body(input)?).await?)
}

async fn delete_list(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_list(&caller, &id).await?;
    ok()
}

async fn list_items(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    params: Result<Query<ItemsQuery>, QueryRejection>,
) -> ApiResult {
    let include_done = query(params)?.include_done.unwrap_or(false);
    let (list, items) = state.store.list_items(&caller, &id, include_done).await?;
    Ok(Json(json!({ "list": list, "items": items })))
}

async fn add_items(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<AddItems>, JsonRejection>,
) -> ApiResult {
    let items = state
        .store
        .add_items(&caller, &id, body(input)?.items)
        .await?;
    Ok(Json(json!({ "items": items })))
}

async fn update_item(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<ItemPatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.update_item(&caller, &id, body(input)?).await?)
}

async fn complete_item(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<CompleteInput>, JsonRejection>,
) -> ApiResult {
    let done = body(input)?.done;
    let (item, next) = state.store.complete_item(&caller, &id, done).await?;
    Ok(Json(json!({ "item": item, "next": next })))
}

async fn delete_item(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_item(&caller, &id).await?;
    ok()
}

async fn tally(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<TallyQuery>, QueryRejection>,
) -> ApiResult {
    let params = query(params)?;
    let list_id = non_empty(params.list_id);
    let since = non_empty(params.since);
    let tally = state
        .store
        .tally(&caller, list_id.as_deref(), since.as_deref())
        .await?;
    Ok(Json(json!({ "tally": tally })))
}

async fn list_preferences(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<PersonQuery>, QueryRejection>,
) -> ApiResult {
    let person_id = non_empty(query(params)?.person_id).unwrap_or_default();
    let preferences = state.store.list_preferences(&caller, &person_id).await?;
    Ok(Json(json!({ "preferences": preferences })))
}

async fn create_preference(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewPreference>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_preference(&caller, body(input)?).await?)
}

async fn update_preference(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<PreferencePatch>, JsonRejection>,
) -> ApiResult {
    to_json(
        &state
            .store
            .update_preference(&caller, &id, body(input)?)
            .await?,
    )
}

async fn delete_preference(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_preference(&caller, &id).await?;
    ok()
}

async fn list_wishlist(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<PersonQuery>, QueryRejection>,
) -> ApiResult {
    let person_id = non_empty(query(params)?.person_id);
    let items = state
        .store
        .list_wishlist(&caller, person_id.as_deref())
        .await?;
    Ok(Json(json!({ "items": items })))
}

async fn create_wish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewWish>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_wish(&caller, body(input)?).await?)
}

async fn update_wish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<WishPatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.update_wish(&caller, &id, body(input)?).await?)
}

async fn delete_wish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_wish(&caller, &id).await?;
    ok()
}

async fn claim_wish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    to_json(&state.store.claim_wish(&caller, &id).await?)
}

async fn unclaim_wish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    to_json(&state.store.unclaim_wish(&caller, &id).await?)
}

async fn list_gifts(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<GiftsQuery>, QueryRejection>,
) -> ApiResult {
    let params = query(params)?;
    let person_id = non_empty(params.person_id);
    let gift_ideas = state
        .store
        .list_gifts(&caller, person_id.as_deref(), params.status)
        .await?;
    Ok(Json(json!({ "giftIdeas": gift_ideas })))
}

async fn create_gift(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<NewGift>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.create_gift(&caller, body(input)?).await?)
}

async fn update_gift(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    input: Result<Json<GiftPatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.update_gift(&caller, &id, body(input)?).await?)
}

async fn delete_gift(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult {
    state.store.delete_gift(&caller, &id).await?;
    ok()
}

async fn gift_context(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    params: Result<Query<GiftContextQuery>, QueryRejection>,
) -> ApiResult {
    to_json(&state.store.gift_context(&caller, query(params)?).await?)
}

async fn get_settings(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult {
    to_json(&state.store.get_settings(&caller).await?)
}

async fn patch_settings(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<SettingsPatch>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.patch_settings(&caller, body(input)?).await?)
}

async fn put_me(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    input: Result<Json<MeInput>, JsonRejection>,
) -> ApiResult {
    to_json(&state.store.set_muted(&caller, body(input)?.muted).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn callers_come_only_from_verified_headers() {
        let org = resolve_caller(
            &headers(&[(CALLER_USER_HEADER, "u1"), (CALLER_ORG_HEADER, "o1")]),
            true,
        )
        .unwrap();
        assert_eq!(org, Caller::member("org:o1", "u1"));
        let user = resolve_caller(&headers(&[(CALLER_USER_HEADER, " u1 ")]), true).unwrap();
        assert_eq!(user, Caller::member("user:u1", "u1"));
        assert!(resolve_caller(&headers(&[(CALLER_ORG_HEADER, "o1")]), false).is_err());
        assert!(resolve_caller(&HeaderMap::new(), true).is_err());
        assert_eq!(
            resolve_caller(&HeaderMap::new(), false).unwrap(),
            Caller::member("local", "local")
        );
        // An empty value is absent.
        assert_eq!(
            resolve_caller(&headers(&[(CALLER_USER_HEADER, "  ")]), false).unwrap(),
            Caller::member("local", "local")
        );
        let long = "x".repeat(257);
        assert!(resolve_caller(&headers(&[(CALLER_USER_HEADER, &long)]), false).is_err());
    }

    #[test]
    fn errors_map_to_the_contract_codes() {
        assert_eq!(FamilyError::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(FamilyError::forbidden().status(), StatusCode::FORBIDDEN);
        assert_eq!(
            FamilyError::Conflict(String::new()).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            FamilyError::from(crate::validation::ValidationError::EmptyPatch).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(FamilyError::BadRequest(String::new()).code(), "bad_request");
        assert_eq!(
            FamilyError::Unavailable.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
