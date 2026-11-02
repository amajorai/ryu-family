mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use common::{app, memory_state, send};
use ryu_family::{api::SERVED_ROUTES, mcp};
use serde_json::{json, Value};
use tower::ServiceExt;

const EXPECTED_ROUTES: [&str; 22] = [
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

const EXPECTED_TOOLS: [&str; 18] = [
    "upcoming",
    "people_list",
    "person_upsert",
    "dates_list",
    "date_upsert",
    "date_delete",
    "lists_get",
    "list_create",
    "list_add_items",
    "list_check",
    "list_assign",
    "list_remove_item",
    "chores_tally",
    "prefs_get",
    "prefs_set",
    "wishlist_get",
    "wishlist_add",
    "gift_context",
];

#[test]
fn served_routes_are_the_contract_surface() {
    assert_eq!(SERVED_ROUTES, EXPECTED_ROUTES);
}

#[test]
fn manifest_route_rows_cover_exactly_the_served_paths() {
    let manifest: Value = serde_json::from_str(include_str!("../../manifest.json")).unwrap();
    let routes = manifest["sidecars"][0]["http"]["routes"]
        .as_array()
        .unwrap();
    assert_eq!(routes.len(), 38);
    let mut paths: Vec<&str> = Vec::new();
    for route in routes {
        let path = route["path"].as_str().unwrap();
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    assert_eq!(paths, SERVED_ROUTES);
}

#[test]
fn mcp_tools_are_bare_descriptive_and_in_contract_order() {
    let table = mcp::tools();
    let tools = table.as_array().expect("MCP tool list");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(names, EXPECTED_TOOLS);
    for tool in tools {
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(description.len() > 40);
        assert!(description.contains("Gift ideas and wishlist claims live only in the Family app"));
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(!tool["name"].as_str().unwrap().contains('.'));
    }
    // No tool can read, write, claim or unclaim gift ideas or claims.
    for forbidden in [
        "gift_idea",
        "claim",
        "unclaim",
        "settings",
        "audit",
        "delete_list",
    ] {
        assert!(
            names.iter().all(|name| !name.contains(forbidden)),
            "{forbidden}"
        );
    }
}

#[tokio::test]
async fn mcp_without_a_connected_tenant_says_how_to_connect() {
    let state = memory_state();
    let result = mcp::call_result(&state, "people_list", &json!({})).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["code"], "agent_not_connected");
    assert_eq!(
        payload["error"],
        "Turn on Family Planner agent access in the Family app's Settings to connect the family agent."
    );
    // The MCP lane never creates a tenant.
    assert!(state.store.reminder_tenants().await.unwrap().is_empty());
}

#[tokio::test]
async fn mcp_errors_are_result_content_with_codes() {
    let state = memory_state();
    state.store.ensure_settings("local", "local").await.unwrap();
    state
        .store
        .patch_settings(
            &ryu_family::authz::Caller::member("local", "local"),
            ryu_family::models::SettingsPatch {
                agent_access: Some(true),
                ..ryu_family::models::SettingsPatch::default()
            },
        )
        .await
        .unwrap();
    let result = mcp::call_result(&state, "date_delete", &json!({ "id": "missing" })).await;
    assert_eq!(result["isError"], true);
    let payload: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["code"], "not_found");
    let unknown = mcp::call_result(&state, "gift_idea_save", &json!({})).await;
    assert_eq!(unknown["isError"], true);
}

#[tokio::test]
async fn health_is_open_and_says_nothing_about_tenants() {
    let state = memory_state();
    let response = app(&state)
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
    // Only the liveness flag and the at-rest sealing state; nothing per tenant.
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({ "ok": true, "sealing": { "state": "ok", "unreadable_rows": 0 } })
    );
}

#[tokio::test]
async fn the_mount_is_bearer_gated() {
    let state = memory_state();
    let response = app(&state)
        .oneshot(
            Request::builder()
                .uri("/api/family/people")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let wrong = app(&state)
        .oneshot(
            Request::builder()
                .uri("/api/family/people")
                .header("authorization", "Bearer nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_bodies_and_queries_are_bad_requests_with_codes() {
    let state = memory_state();
    let app = app(&state);
    let request = Request::builder()
        .method("POST")
        .uri("/api/family/people")
        .header("authorization", format!("Bearer {}", common::TOKEN))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["code"],
        "bad_request"
    );

    let (status, body) = send(&app, "GET", "/upcoming?days=abc", None, None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "bad_request");

    let (status, body) = send(&app, "GET", "/upcoming?days=0", None, None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "validation");

    let (status, body) = send(&app, "POST", "/people", None, None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "validation");
    assert_eq!(body["error"], "displayName is required");

    let (status, body) = send(&app, "PATCH", "/people/x", None, None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "validation");
}

#[tokio::test]
async fn wire_types_are_camel_case() {
    let state = memory_state();
    let app = app(&state);
    let (status, person) = send(
        &app,
        "POST",
        "/people",
        None,
        None,
        Some(json!({ "displayName": "Ana", "relationship": "daughter" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "id",
        "visibility",
        "createdBy",
        "createdAt",
        "updatedAt",
        "canEdit",
        "displayName",
        "relationship",
        "memberLinked",
        "isMe",
    ] {
        assert!(person.get(key).is_some(), "{key}");
    }
    assert!(person.get("memberUserId").is_none());
    assert!(person.get("member_user_id").is_none());

    let (_, settings) = send(&app, "GET", "/settings", None, None, None).await;
    assert_eq!(
        settings["settings"],
        json!({
            "tz": "UTC",
            "reminderHour": 9,
            "defaultRemindOffsetsDays": [7, 1, 0],
            "currency": "USD",
            "agentAccess": false,
            "updatedAt": settings["settings"]["updatedAt"],
        })
    );
    assert_eq!(settings["me"]["tenantKind"], "local");
    assert_eq!(settings["me"]["userId"], "local");
}
