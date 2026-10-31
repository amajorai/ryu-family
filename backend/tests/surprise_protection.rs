//! Surprises stay surprises on every path: HTTP, MCP and reminders.

mod common;

use axum::{http::StatusCode, Router};
use common::{app, memory_state, org_call, state_with_host, utc, NotifyStub};
use ryu_family::{mcp, state::AppState, store::FamilyStore, tick};
use serde_json::{json, Value};

const SECRET: &str = "Secret telescope";
const GIVEN: &str = "Last year's scarf";

struct Family {
    app: Router,
    state: AppState,
    ana_person: String,
    wish: String,
    gift: String,
}

/// Org `o1`: Ana is a member linked to her person row; Bo and Cy are givers.
async fn family(state: AppState) -> Family {
    let app = app(&state);
    let (_, ana) = org_call(
        &app,
        "POST",
        "/people",
        "bo",
        Some(json!({ "displayName": "Ana", "relationship": "daughter" })),
    )
    .await;
    let ana_person = ana["id"].as_str().unwrap().to_owned();
    let (status, linked) = org_call(
        &app,
        "POST",
        &format!("/people/{ana_person}/claim"),
        "ana",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    assert_eq!(linked["isMe"], true);

    let (status, gift) = org_call(&app, "POST", "/gift-ideas", "bo", Some(json!({ "personId": ana_person, "idea": SECRET, "url": "https://example.com/telescope", "budgetMinor": 15000 }))).await;
    assert_eq!(status, StatusCode::OK, "{gift}");
    assert_eq!(gift["currency"], "USD");
    org_call(&app, "POST", "/gift-ideas", "cy", Some(json!({ "personId": ana_person, "idea": GIVEN, "status": "given", "occasion": "Birthday" }))).await;

    let (_, wish) = org_call(
        &app,
        "POST",
        "/wishlist",
        "ana",
        Some(json!({ "personId": ana_person, "title": "Book", "priority": 1 })),
    )
    .await;
    let wish_id = wish["id"].as_str().unwrap().to_owned();
    assert!(
        wish.get("claim").is_none(),
        "owner's own create has no claim key"
    );
    let (status, claimed) = org_call(
        &app,
        "POST",
        &format!("/wishlist/{wish_id}/claim"),
        "bo",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claimed["claim"], json!({ "claimed": true, "byMe": true }));
    org_call(
        &app,
        "POST",
        "/wishlist",
        "ana",
        Some(json!({ "personId": ana_person, "title": "Puzzle" })),
    )
    .await;

    Family {
        app,
        state,
        ana_person,
        wish: wish_id,
        gift: gift["id"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test]
async fn the_recipient_never_sees_gift_ideas_over_http() {
    let f = family(memory_state()).await;
    let (status, list) = org_call(&f.app, "GET", "/gift-ideas", "ana", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["giftIdeas"], json!([]));
    let (_, filtered) = org_call(
        &f.app,
        "GET",
        &format!("/gift-ideas?personId={}", f.ana_person),
        "ana",
        None,
    )
    .await;
    assert_eq!(filtered["giftIdeas"], json!([]));
    let (_, by_status) = org_call(&f.app, "GET", "/gift-ideas?status=idea", "ana", None).await;
    assert_eq!(by_status["giftIdeas"], json!([]));

    for (method, body) in [
        ("PATCH", Some(json!({ "status": "bought" }))),
        ("DELETE", None),
    ] {
        let (status, response) = org_call(
            &f.app,
            method,
            &format!("/gift-ideas/{}", f.gift),
            "ana",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        assert_eq!(response["code"], "not_found");
    }
    // The same 404 a missing id gets.
    let (_, missing) = org_call(&f.app, "DELETE", "/gift-ideas/does-not-exist", "ana", None).await;
    let (_, hidden) = org_call(
        &f.app,
        "DELETE",
        &format!("/gift-ideas/{}", f.gift),
        "ana",
        None,
    )
    .await;
    assert_eq!(missing, hidden);

    let (status, own) = org_call(
        &f.app,
        "POST",
        "/gift-ideas",
        "ana",
        Some(json!({ "personId": f.ana_person, "idea": "Something for me" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(own["code"], "forbidden");

    // Givers still see and edit them.
    let (_, bo) = org_call(&f.app, "GET", "/gift-ideas", "bo", None).await;
    assert_eq!(bo["giftIdeas"].as_array().unwrap().len(), 2);
    let (status, _) = org_call(
        &f.app,
        "PATCH",
        &format!("/gift-ideas/{}", f.gift),
        "cy",
        Some(json!({ "status": "bought" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Nothing the recipient can read anywhere mentions the idea.
    for path in [
        "/people",
        "/upcoming?days=366",
        "/settings",
        &format!("/wishlist?personId={}", f.ana_person),
    ] {
        let (_, body) = org_call(&f.app, "GET", path, "ana", None).await;
        assert!(!body.to_string().contains(SECRET), "{path}");
    }
}

#[tokio::test]
async fn the_wishlist_owner_never_sees_claims() {
    let f = family(memory_state()).await;
    let (_, list) = org_call(
        &f.app,
        "GET",
        &format!("/wishlist?personId={}", f.ana_person),
        "ana",
        None,
    )
    .await;
    for item in list["items"].as_array().unwrap() {
        assert!(
            item.get("claim").is_none(),
            "claim key must be absent: {item}"
        );
        assert_eq!(item["canClaim"], false);
        assert_eq!(item["canEdit"], true);
    }
    let (_, all) = org_call(&f.app, "GET", "/wishlist", "ana", None).await;
    assert!(all["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("claim").is_none()));

    let (status, patched) = org_call(
        &f.app,
        "PATCH",
        &format!("/wishlist/{}", f.wish),
        "ana",
        Some(json!({ "priceHint": "about $20" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(patched.get("claim").is_none());
    assert!(!patched.to_string().contains("bo\""));

    let (claim_status, claim) = org_call(
        &f.app,
        "POST",
        &format!("/wishlist/{}/claim", f.wish),
        "ana",
        Some(json!({})),
    )
    .await;
    let (unclaim_status, unclaim) = org_call(
        &f.app,
        "DELETE",
        &format!("/wishlist/{}/claim", f.wish),
        "ana",
        None,
    )
    .await;
    assert_eq!(claim_status, StatusCode::FORBIDDEN);
    assert_eq!(unclaim_status, StatusCode::FORBIDDEN);
    assert_eq!(
        claim["error"],
        "You can't claim items on your own wishlist."
    );
    assert_eq!(claim, unclaim);
    // The same answer for an unclaimed item: nothing about the state leaks.
    let (_, puzzle) = org_call(
        &f.app,
        "GET",
        &format!("/wishlist?personId={}", f.ana_person),
        "ana",
        None,
    )
    .await;
    let puzzle_id = puzzle["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["title"] == "Puzzle")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, unclaimed) = org_call(
        &f.app,
        "POST",
        &format!("/wishlist/{puzzle_id}/claim"),
        "ana",
        Some(json!({})),
    )
    .await;
    assert_eq!(unclaimed, claim);

    // Others see the claim state but never who holds it.
    let (_, cy) = org_call(
        &f.app,
        "GET",
        &format!("/wishlist?personId={}", f.ana_person),
        "cy",
        None,
    )
    .await;
    let book = cy["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == f.wish.as_str())
        .unwrap()
        .clone();
    assert_eq!(book["claim"], json!({ "claimed": true, "byMe": false }));
    assert_eq!(book["canClaim"], false);
    assert_eq!(book["canEdit"], false);
    let (status, conflict) = org_call(
        &f.app,
        "POST",
        &format!("/wishlist/{}/claim", f.wish),
        "cy",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!conflict.to_string().contains("\"bo\""));
    let (status, _) = org_call(
        &f.app,
        "DELETE",
        &format!("/wishlist/{}/claim", f.wish),
        "cy",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = org_call(
        &f.app,
        "PATCH",
        &format!("/wishlist/{}", f.wish),
        "cy",
        Some(json!({ "title": "Other" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Claiming is idempotent for the holder; unclaiming twice is a no-op.
    let (status, again) = org_call(
        &f.app,
        "POST",
        &format!("/wishlist/{}/claim", f.wish),
        "bo",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["claim"]["byMe"], true);
    for _ in 0..2 {
        let (status, released) = org_call(
            &f.app,
            "DELETE",
            &format!("/wishlist/{}/claim", f.wish),
            "bo",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            released["claim"],
            json!({ "claimed": false, "byMe": false })
        );
    }
}

#[tokio::test]
async fn gift_context_never_returns_hidden_data() {
    let f = family(memory_state()).await;
    org_call(
        &f.app,
        "POST",
        "/preferences",
        "ana",
        Some(json!({ "personId": f.ana_person, "key": "colour", "value": "green" })),
    )
    .await;
    org_call(&f.app, "POST", "/preferences", "ana", Some(json!({ "personId": f.ana_person, "key": "allergy", "value": "peanuts", "sensitive": true }))).await;
    org_call(&f.app, "POST", "/preferences", "bo", Some(json!({ "personId": f.ana_person, "key": "note", "value": "bo-only", "visibility": "private" }))).await;
    org_call(&f.app, "POST", "/dates", "bo", Some(json!({ "personId": f.ana_person, "kind": "birthday", "month": 3, "day": 14, "year": 2016 }))).await;

    let path = format!(
        "/gift-context?personId={}&occasion=Birthday&budgetMinor=5000",
        f.ana_person
    );
    let (_, giver) = org_call(&f.app, "GET", &path, "cy", None).await;
    assert_eq!(giver["restricted"], false);
    assert_eq!(giver["budget"], json!({ "minor": 5000, "currency": "USD" }));
    assert_eq!(
        giver["preferences"],
        json!([{ "key": "colour", "value": "green" }])
    );
    let wish_titles: Vec<&str> = giver["wishlist"]
        .as_array()
        .unwrap()
        .iter()
        .map(|wish| wish["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        wish_titles,
        ["Puzzle"],
        "claimed items are left out for givers"
    );
    assert_eq!(
        giver["pastGifts"],
        json!([{ "idea": GIVEN, "occasion": "Birthday" }])
    );
    assert_eq!(giver["nextDates"][0]["kind"], "birthday");
    assert!(
        !giver.to_string().contains(SECRET),
        "only given gifts appear"
    );

    let (_, recipient) = org_call(&f.app, "GET", &path, "ana", None).await;
    assert_eq!(recipient["restricted"], true);
    assert_eq!(recipient["pastGifts"], json!([]));
    assert_eq!(
        recipient["wishlist"].as_array().unwrap().len(),
        2,
        "all items, nothing revealing claims"
    );
    assert!(recipient["wishlist"]
        .as_array()
        .unwrap()
        .iter()
        .all(|wish| wish.get("claim").is_none()));
    assert!(!recipient.to_string().contains(SECRET));
    assert!(!recipient.to_string().contains(GIVEN));
    assert!(!recipient.to_string().contains("bo-only"));
    assert!(!recipient.to_string().contains("peanuts"));
}

fn tool_json(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn the_agent_lane_never_sees_gift_ideas_claims_or_private_rows() {
    let f = family(memory_state()).await;
    org_call(&f.app, "POST", "/preferences", "ana", Some(json!({ "personId": f.ana_person, "key": "allergy", "value": "peanuts", "sensitive": true }))).await;
    let (_, shared_sensitive) = org_call(&f.app, "POST", "/preferences", "ana", Some(json!({ "personId": f.ana_person, "key": "shoe", "value": "38", "sensitive": true, "visibility": "family" }))).await;
    org_call(&f.app, "POST", "/dates", "bo", Some(json!({ "personId": f.ana_person, "kind": "custom", "label": "Bo's private plan", "month": 6, "day": 1, "visibility": "private" }))).await;
    org_call(
        &f.app,
        "POST",
        "/lists",
        "bo",
        Some(json!({ "name": "Bo's secret list", "kind": "todo", "visibility": "private" })),
    )
    .await;
    let (status, settings) = org_call(&f.app, "GET", "/settings", "bo", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        settings["settings"]["agentAccess"], false,
        "agent access starts off"
    );
    let (status, settings) = org_call(
        &f.app,
        "PATCH",
        "/settings",
        "bo",
        Some(json!({ "agentAccess": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["settings"]["agentAccess"], true);

    let calls = [
        ("upcoming", json!({ "days": 366 })),
        ("people_list", json!({})),
        ("dates_list", json!({})),
        ("lists_get", json!({})),
        ("prefs_get", json!({ "personId": f.ana_person })),
        ("wishlist_get", json!({ "personId": f.ana_person })),
        (
            "gift_context",
            json!({ "personId": f.ana_person, "budgetMinor": 2000 }),
        ),
    ];
    for (tool, args) in calls {
        let result = mcp::call_result(&f.state, tool, &args).await;
        assert_eq!(result["isError"], false, "{tool}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        for hidden in [
            SECRET,
            GIVEN,
            "\"claim\"",
            "peanuts",
            "\"shoe\"",
            "Bo's private plan",
            "Bo's secret list",
        ] {
            assert!(!text.contains(hidden), "{tool} leaked {hidden}");
        }
    }
    let wishlist = tool_json(
        &mcp::call_result(
            &f.state,
            "wishlist_get",
            &json!({ "personId": f.ana_person }),
        )
        .await,
    );
    assert_eq!(wishlist["items"].as_array().unwrap().len(), 2);
    assert!(wishlist["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["canClaim"] == false && item["canEdit"] == false));
    let context = tool_json(
        &mcp::call_result(
            &f.state,
            "gift_context",
            &json!({ "personId": f.ana_person }),
        )
        .await,
    );
    assert_eq!(context["restricted"], true);
    assert_eq!(context["pastGifts"], json!([]));
    let prefs = tool_json(
        &mcp::call_result(&f.state, "prefs_get", &json!({ "personId": f.ana_person })).await,
    );
    // The lane may be a group chat: even the sensitive row Ana shared with the
    // family in the app stays out of it.
    assert_eq!(prefs["preferences"], json!([]));
    let (_, in_app) = org_call(
        &f.app,
        "GET",
        &format!("/preferences?personId={}", f.ana_person),
        "cy",
        None,
    )
    .await;
    assert!(in_app.to_string().contains("\"shoe\""), "{in_app}");

    // The agent cannot touch a sensitive row, even a shared one.
    let edit = mcp::call_result(&f.state, "prefs_set", &json!({ "personId": f.ana_person, "id": shared_sensitive["id"], "key": "shoe", "value": "40" })).await;
    assert_eq!(edit["isError"], true);
    assert_eq!(tool_json(&edit)["code"], "not_found");
    // Its writes are always family-visible and carry its own actor.
    let added = tool_json(
        &mcp::call_result(
            &f.state,
            "wishlist_add",
            &json!({ "personId": f.ana_person, "title": "Torch", "visibility": "private" }),
        )
        .await,
    );
    assert_eq!(added["visibility"], "family");
    assert_eq!(added["createdBy"], "agent");
    assert!(added.get("claim").is_none());
    let audit = f.state.store.audit_rows("org:o1").await.unwrap();
    assert!(audit
        .iter()
        .any(|row| row.actor == "agent" && row.action == "wishlist_items.create"));
}

#[tokio::test]
async fn reminders_never_carry_gift_details() {
    let stub = NotifyStub::start().await;
    let state = state_with_host(FamilyStore::open_memory().unwrap(), &stub);
    let f = family(state).await;
    // Ana's anniversary reminds Ana too; her birthday never reminds her.
    org_call(&f.app, "POST", "/dates", "bo", Some(json!({ "personId": f.ana_person, "kind": "anniversary", "label": "Adoption day", "month": 3, "day": 14, "remindOffsetsDays": [0] }))).await;
    org_call(&f.app, "POST", "/dates", "bo", Some(json!({ "personId": f.ana_person, "kind": "birthday", "month": 3, "day": 15, "remindOffsetsDays": [1] }))).await;

    let report = tick::once_at(&f.state, utc(2027, 3, 14, 10, 0))
        .await
        .unwrap();
    assert_eq!(report.sent, 5, "anniversary: ana, bo, cy; birthday: bo, cy");
    let bodies = stub.bodies().await;
    for body in &bodies {
        let text = body.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains("telescope"));
        assert!(!text.contains("Book"));
        assert!(!text.contains("claim"));
    }
    let to_ana: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["target_user_id"] == "ana")
        .collect();
    assert_eq!(to_ana.len(), 1);
    assert_eq!(to_ana[0]["title"], "Adoption day for Ana is today");
    assert!(!to_ana[0]["body"].as_str().unwrap().contains("gift"));
    let to_bo: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["target_user_id"] == "bo")
        .collect();
    assert_eq!(to_bo.len(), 2);
    assert!(to_bo.iter().all(|body| body["body"]
        .as_str()
        .unwrap()
        .ends_with("· 1 gift idea saved in Family")));
    assert!(bodies.iter().all(|body| !(body["target_user_id"] == "ana"
        && body["title"].as_str().unwrap().contains("birthday"))));
}

#[tokio::test]
async fn claiming_a_wish_leaves_the_owner_and_agent_views_unchanged() {
    let f = family(memory_state()).await;
    org_call(
        &f.app,
        "PATCH",
        "/settings",
        "ana",
        Some(json!({ "agentAccess": true })),
    )
    .await;
    let path = format!("/wishlist?personId={}", f.ana_person);
    let owner_view = |app: Router, path: String| async move {
        let (status, body) = org_call(&app, "GET", &path, "ana", None).await;
        assert_eq!(status, StatusCode::OK);
        body.to_string()
    };
    let agent_view = |state: AppState, person: String| async move {
        mcp::call_result(&state, "wishlist_get", &json!({ "personId": person }))
            .await
            .to_string()
    };
    let (_, puzzle) = org_call(&f.app, "GET", &path, "cy", None).await;
    let puzzle_id = puzzle
        .as_array()
        .or_else(|| puzzle["items"].as_array())
        .unwrap()
        .iter()
        .find(|item| item["title"] == "Puzzle")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_before = owner_view(f.app.clone(), path.clone()).await;
    let agent_before = agent_view(f.state.clone(), f.ana_person.clone()).await;
    let claim = format!("/wishlist/{puzzle_id}/claim");
    let (status, _) = org_call(&f.app, "POST", &claim, "cy", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(owner_view(f.app.clone(), path.clone()).await, owner_before);
    assert_eq!(
        agent_view(f.state.clone(), f.ana_person.clone()).await,
        agent_before
    );
    let (status, _) = org_call(&f.app, "DELETE", &claim, "cy", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(owner_view(f.app.clone(), path).await, owner_before);
    assert_eq!(
        agent_view(f.state.clone(), f.ana_person.clone()).await,
        agent_before
    );
}

#[tokio::test]
async fn saying_this_is_me_never_unlocks_what_others_wrote_before() {
    let app = app(&memory_state());
    let (_, grandma) = org_call(
        &app,
        "POST",
        "/people",
        "bo",
        Some(json!({ "displayName": "Grandma" })),
    )
    .await;
    let grandma = grandma["id"].as_str().unwrap().to_owned();
    let (status, medication) = org_call(&app, "POST", "/preferences", "bo", Some(json!({ "personId": grandma, "key": "medication", "value": "warfarin", "sensitive": true }))).await;
    assert_eq!(status, StatusCode::OK, "{medication}");
    assert_eq!(medication["visibility"], "private");
    let (_, wish) = org_call(
        &app,
        "POST",
        "/wishlist",
        "bo",
        Some(json!({ "personId": grandma, "title": "Bo's private note", "visibility": "private" })),
    )
    .await;
    // Cy, who is not Grandma, claims her.
    let (status, _) = org_call(
        &app,
        "POST",
        &format!("/people/{grandma}/claim"),
        "cy",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, prefs) = org_call(
        &app,
        "GET",
        &format!("/preferences?personId={grandma}"),
        "cy",
        None,
    )
    .await;
    assert!(!prefs.to_string().contains("warfarin"), "{prefs}");
    let (status, _) = org_call(
        &app,
        "PATCH",
        &format!("/preferences/{}", medication["id"].as_str().unwrap()),
        "cy",
        Some(json!({ "visibility": "family" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, wishes) = org_call(
        &app,
        "GET",
        &format!("/wishlist?personId={grandma}"),
        "cy",
        None,
    )
    .await;
    assert!(
        !wishes.to_string().contains("Bo's private note"),
        "{wishes}"
    );
    let (status, _) = org_call(
        &app,
        "DELETE",
        &format!("/wishlist/{}", wish["id"].as_str().unwrap()),
        "cy",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The claimant cannot delete the person (and everyone's rows with it).
    let (status, _) = org_call(&app, "DELETE", &format!("/people/{grandma}"), "cy", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Bo still controls the row.
    let (_, prefs) = org_call(
        &app,
        "GET",
        &format!("/preferences?personId={grandma}"),
        "bo",
        None,
    )
    .await;
    assert!(prefs.to_string().contains("warfarin"));
}
