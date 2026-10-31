//! Org A cannot see or touch org B over HTTP, and the verified headers are the
//! only source of tenant and acting user.

mod common;

use axum::http::StatusCode;
use common::{app, managed_app, memory_state, send};
use ryu_family::store::FamilyStore;
use serde_json::{json, Value};

async fn seed(app: &axum::Router, user: &str, org: &str) -> Value {
    let (status, person) = send(
        app,
        "POST",
        "/people",
        Some(user),
        Some(org),
        Some(json!({ "displayName": format!("{org} kid") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let person_id = person["id"].as_str().unwrap().to_owned();
    let (_, date) = send(
        app,
        "POST",
        "/dates",
        Some(user),
        Some(org),
        Some(json!({ "personId": person_id, "kind": "birthday", "month": 5, "day": 2 })),
    )
    .await;
    let (_, list) = send(
        app,
        "POST",
        "/lists",
        Some(user),
        Some(org),
        Some(json!({ "name": "Groceries", "kind": "grocery" })),
    )
    .await;
    let list_id = list["id"].as_str().unwrap().to_owned();
    let (_, items) = send(
        app,
        "POST",
        &format!("/lists/{list_id}/items"),
        Some(user),
        Some(org),
        Some(json!({ "items": [{ "text": "Milk", "dueOn": "2026-10-20" }] })),
    )
    .await;
    let (_, pref) = send(
        app,
        "POST",
        "/preferences",
        Some(user),
        Some(org),
        Some(json!({ "personId": person_id, "key": "size", "value": "M" })),
    )
    .await;
    let (_, wish) = send(
        app,
        "POST",
        "/wishlist",
        Some(user),
        Some(org),
        Some(json!({ "personId": person_id, "title": "Kite" })),
    )
    .await;
    let (_, gift) = send(
        app,
        "POST",
        "/gift-ideas",
        Some(user),
        Some(org),
        Some(json!({ "personId": person_id, "idea": "Paint set" })),
    )
    .await;
    json!({
        "person": person_id,
        "date": date["id"],
        "list": list_id,
        "item": items["items"][0]["id"],
        "pref": pref["id"],
        "wish": wish["id"],
        "gift": gift["id"],
    })
}

#[tokio::test]
async fn org_a_cannot_see_org_b() {
    let state = memory_state();
    let app = app(&state);
    let a = seed(&app, "alice", "org-a").await;
    let _b = seed(&app, "bob", "org-b").await;

    let reads = [
        ("/people", "people"),
        ("/dates", "dates"),
        ("/lists", "lists"),
        ("/wishlist", "items"),
        ("/gift-ideas", "giftIdeas"),
    ];
    for (path, key) in reads {
        let (status, body) = send(&app, "GET", path, Some("bob"), Some("org-b"), None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let rows = body[key].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{path}: org B sees only its own row");
        let leaked = rows
            .iter()
            .any(|row| a.as_object().unwrap().values().any(|id| row["id"] == *id));
        assert!(!leaked, "{path} leaked an org A row");
    }
    let (_, upcoming) = send(
        &app,
        "GET",
        "/upcoming?days=366",
        Some("bob"),
        Some("org-b"),
        None,
    )
    .await;
    assert!(!upcoming.to_string().contains("org-a kid"));
    let (_, tally) = send(
        &app,
        "GET",
        "/chores/tally",
        Some("bob"),
        Some("org-b"),
        None,
    )
    .await;
    assert_eq!(tally["tally"], json!([]));

    let person = a["person"].as_str().unwrap();
    let list = a["list"].as_str().unwrap();
    let not_found = [
        ("GET", format!("/preferences?personId={person}"), None),
        ("GET", format!("/dates?personId={person}"), None),
        ("GET", format!("/wishlist?personId={person}"), None),
        ("GET", format!("/gift-ideas?personId={person}"), None),
        ("GET", format!("/gift-context?personId={person}"), None),
        ("GET", format!("/lists/{list}/items"), None),
        ("GET", format!("/chores/tally?listId={list}"), None),
        (
            "PATCH",
            format!("/people/{person}"),
            Some(json!({ "displayName": "Mine now" })),
        ),
        ("DELETE", format!("/people/{person}"), None),
        ("POST", format!("/people/{person}/claim"), Some(json!({}))),
        (
            "PATCH",
            format!("/dates/{}", a["date"].as_str().unwrap()),
            Some(json!({ "label": "x" })),
        ),
        (
            "DELETE",
            format!("/dates/{}", a["date"].as_str().unwrap()),
            None,
        ),
        (
            "PATCH",
            format!("/lists/{list}"),
            Some(json!({ "name": "x" })),
        ),
        ("DELETE", format!("/lists/{list}"), None),
        (
            "POST",
            format!("/lists/{list}/items"),
            Some(json!({ "items": [{ "text": "x" }] })),
        ),
        (
            "PATCH",
            format!("/items/{}", a["item"].as_str().unwrap()),
            Some(json!({ "text": "x" })),
        ),
        (
            "POST",
            format!("/items/{}/complete", a["item"].as_str().unwrap()),
            Some(json!({ "done": true })),
        ),
        (
            "DELETE",
            format!("/items/{}", a["item"].as_str().unwrap()),
            None,
        ),
        (
            "PATCH",
            format!("/preferences/{}", a["pref"].as_str().unwrap()),
            Some(json!({ "value": "x" })),
        ),
        (
            "DELETE",
            format!("/preferences/{}", a["pref"].as_str().unwrap()),
            None,
        ),
        (
            "PATCH",
            format!("/wishlist/{}", a["wish"].as_str().unwrap()),
            Some(json!({ "title": "x" })),
        ),
        (
            "DELETE",
            format!("/wishlist/{}", a["wish"].as_str().unwrap()),
            None,
        ),
        (
            "POST",
            format!("/wishlist/{}/claim", a["wish"].as_str().unwrap()),
            Some(json!({})),
        ),
        (
            "DELETE",
            format!("/wishlist/{}/claim", a["wish"].as_str().unwrap()),
            None,
        ),
        (
            "PATCH",
            format!("/gift-ideas/{}", a["gift"].as_str().unwrap()),
            Some(json!({ "idea": "x" })),
        ),
        (
            "DELETE",
            format!("/gift-ideas/{}", a["gift"].as_str().unwrap()),
            None,
        ),
        (
            "POST",
            "/dates".to_owned(),
            Some(json!({ "personId": person, "kind": "custom", "month": 1, "day": 1 })),
        ),
        (
            "POST",
            "/wishlist".to_owned(),
            Some(json!({ "personId": person, "title": "x" })),
        ),
        (
            "POST",
            "/gift-ideas".to_owned(),
            Some(json!({ "personId": person, "idea": "x" })),
        ),
        (
            "POST",
            "/preferences".to_owned(),
            Some(json!({ "personId": person, "key": "k", "value": "v" })),
        ),
    ];
    for (method, path, body) in not_found {
        let (status, response) = send(&app, method, &path, Some("bob"), Some("org-b"), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {response}");
        assert_eq!(response["code"], "not_found");
    }

    // Org A's rows are intact.
    let (_, people) = send(&app, "GET", "/people", Some("alice"), Some("org-a"), None).await;
    assert_eq!(people["people"][0]["displayName"], "org-a kid");
    let (_, items) = send(
        &app,
        "GET",
        &format!("/lists/{list}/items"),
        Some("alice"),
        Some("org-a"),
        None,
    )
    .await;
    assert_eq!(items["items"][0]["text"], "Milk");
    assert_eq!(items["items"][0]["done"], false);
}

#[tokio::test]
async fn body_and_query_tenants_are_ignored() {
    let state = memory_state();
    let app = app(&state);
    let (status, _) = send(
        &app,
        "POST",
        "/people?tenant=org:org-a&userId=alice",
        Some("bob"),
        Some("org-b"),
        Some(json!({ "displayName": "Sneaky", "tenant": "org:org-a", "createdBy": "alice", "memberUserId": "alice" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, a_people) = send(&app, "GET", "/people", Some("alice"), Some("org-a"), None).await;
    assert_eq!(a_people["people"], json!([]));
    let (_, b_people) = send(
        &app,
        "GET",
        "/people?tenant=org:org-a",
        Some("bob"),
        Some("org-b"),
        None,
    )
    .await;
    let sneaky = &b_people["people"][0];
    assert_eq!(sneaky["displayName"], "Sneaky");
    assert_eq!(sneaky["createdBy"], "bob");
    assert_eq!(sneaky["memberLinked"], false);
}

#[tokio::test]
async fn a_user_without_an_org_gets_a_personal_tenant() {
    let state = memory_state();
    let app = app(&state);
    send(
        &app,
        "POST",
        "/people",
        Some("solo"),
        None,
        Some(json!({ "displayName": "Solo's dad" })),
    )
    .await;
    let (_, mine) = send(&app, "GET", "/people", Some("solo"), None, None).await;
    assert_eq!(mine["people"].as_array().unwrap().len(), 1);
    let (_, settings) = send(&app, "GET", "/settings", Some("solo"), None, None).await;
    assert_eq!(settings["me"]["tenantKind"], "user");
    // The same user inside an org is a different tenant.
    let (_, in_org) = send(&app, "GET", "/people", Some("solo"), Some("org-x"), None).await;
    assert_eq!(in_org["people"], json!([]));
    // The unmanaged local tenant is separate too.
    let (_, local) = send(&app, "GET", "/people", None, None, None).await;
    assert_eq!(local["people"], json!([]));
}

#[tokio::test]
async fn unresolved_callers_fail_closed() {
    let state = memory_state();
    let open = app(&state);
    let (status, body) = send(&open, "GET", "/people", None, Some("org-a"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "tenant_required");
    let long = "u".repeat(300);
    let (status, body) = send(&open, "GET", "/people", Some(&long), Some("org-a"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "tenant_required");

    let managed = managed_app(FamilyStore::open_memory().unwrap());
    let (status, body) = send(&managed, "GET", "/people", None, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "tenant_required");
    let (status, _) = send(
        &managed,
        "GET",
        "/people",
        Some("alice"),
        Some("org-a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn settings_and_mutes_are_per_tenant_and_per_member() {
    let state = memory_state();
    let app = app(&state);
    let (status, _) = send(
        &app,
        "PATCH",
        "/settings",
        Some("alice"),
        Some("org-a"),
        Some(json!({ "tz": "Europe/London", "reminderHour": 8 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, b) = send(&app, "GET", "/settings", Some("bob"), Some("org-b"), None).await;
    assert_eq!(b["settings"]["tz"], "UTC");
    let (_, muted) = send(
        &app,
        "PUT",
        "/settings/me",
        Some("alice"),
        Some("org-a"),
        Some(json!({ "muted": true })),
    )
    .await;
    assert_eq!(muted["me"]["muted"], true);
    let (_, other) = send(&app, "GET", "/settings", Some("carol"), Some("org-a"), None).await;
    assert_eq!(other["me"]["muted"], false);
    assert_eq!(other["settings"]["tz"], "Europe/London");
}
