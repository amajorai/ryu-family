//! Recurring list items and the chore points tally.

mod common;

use axum::http::StatusCode;
use common::{app, member, memory_state, send, utc};
use ryu_family::{
    authz::Caller,
    models::{ItemPatch, ListKind, NewList, NewListItem, NewPerson, Recurrence, SettingsPatch},
    store::FamilyStore,
};
use serde_json::json;

async fn chores(store: &FamilyStore, caller: &Caller) -> String {
    store
        .ensure_settings(&caller.tenant, caller.actor())
        .await
        .unwrap();
    store
        .create_list(
            caller,
            NewList {
                name: "Chores".into(),
                kind: ListKind::Chores,
                visibility: None,
            },
        )
        .await
        .unwrap()
        .base
        .id
}

async fn add(store: &FamilyStore, caller: &Caller, list: &str, item: NewListItem) -> String {
    store.add_items(caller, list, vec![item]).await.unwrap()[0]
        .base
        .id
        .clone()
}

fn chore(text: &str, due: Option<&str>, recurrence: Recurrence, points: i64) -> NewListItem {
    NewListItem {
        text: text.into(),
        due_on: due.map(Into::into),
        recurrence: Some(recurrence),
        points: Some(points),
        ..NewListItem::default()
    }
}

#[tokio::test]
async fn daily_weekly_and_monthly_successors_advance_from_the_due_date() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    let cases = [
        (Recurrence::Daily, "2027-12-31", "2028-01-01"),
        (Recurrence::Weekly, "2027-12-28", "2028-01-04"),
        (Recurrence::Monthly, "2027-01-31", "2027-02-28"),
        (Recurrence::Monthly, "2028-01-31", "2028-02-29"),
        (Recurrence::Monthly, "2027-03-31", "2027-04-30"),
    ];
    for (recurrence, due, expected) in cases {
        let id = add(
            &store,
            &caller,
            &list,
            chore("Feed the cat", Some(due), recurrence, 5),
        )
        .await;
        let (item, next) = store.complete_item(&caller, &id, true).await.unwrap();
        assert!(item.done);
        assert_eq!(item.done_by.as_deref(), Some("local"));
        let next = next.expect("recurring item spawns a successor");
        assert_eq!(
            next.due_on.as_deref(),
            Some(expected),
            "{recurrence:?} from {due}"
        );
        assert!(!next.done);
        assert_eq!(next.text, "Feed the cat");
        assert_eq!(next.points, 5);
        assert_eq!(next.recurrence, recurrence);
        assert_eq!(item.next_item_id.as_deref(), Some(next.base.id.as_str()));
    }
}

#[tokio::test]
async fn without_a_due_date_the_successor_counts_from_the_local_completion_day() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    store
        .patch_settings(
            &caller,
            SettingsPatch {
                tz: Some("Pacific/Auckland".into()),
                ..SettingsPatch::default()
            },
        )
        .await
        .unwrap();
    let id = add(
        &store,
        &caller,
        &list,
        chore("Water plants", None, Recurrence::Weekly, 1),
    )
    .await;
    // 2027-06-30T20:00Z is Jul 1 in Auckland.
    let (_, next) = store
        .complete_item_at(&caller, &id, true, utc(2027, 6, 30, 20, 0))
        .await
        .unwrap();
    assert_eq!(next.unwrap().due_on.as_deref(), Some("2027-07-08"));
}

#[tokio::test]
async fn completing_twice_never_spawns_a_second_successor() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    let id = add(
        &store,
        &caller,
        &list,
        chore("Bins out", Some("2027-05-03"), Recurrence::Weekly, 2),
    )
    .await;
    let (first, next) = store.complete_item(&caller, &id, true).await.unwrap();
    let next = next.unwrap();
    let (again, repeat) = store.complete_item(&caller, &id, true).await.unwrap();
    assert_eq!(repeat.unwrap().base.id, next.base.id);
    assert_eq!(
        again.done_at, first.done_at,
        "a repeat completion changes nothing"
    );
    let (undone, none) = store.complete_item(&caller, &id, false).await.unwrap();
    assert!(!undone.done);
    assert_eq!(undone.done_at, None);
    assert_eq!(undone.done_by, None);
    assert!(none.is_none());
    // The untouched successor goes with the undone completion, so one
    // occurrence never has two open copies (or double points).
    assert_eq!(undone.next_item_id, None, "the untouched successor goes");
    let (_, items) = store.list_items(&caller, &list, true).await.unwrap();
    assert_eq!(items.len(), 1);
    let (_, redo) = store.complete_item(&caller, &id, true).await.unwrap();
    let redo = redo.unwrap();
    assert_eq!(redo.due_on.as_deref(), Some("2027-05-10"));
    // A successor someone already touched stays, and keeps the chain.
    store
        .update_item(
            &caller,
            &redo.base.id,
            ItemPatch {
                text: Some("Bins out (both)".into()),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    let (undone, _) = store.complete_item(&caller, &id, false).await.unwrap();
    assert_eq!(undone.next_item_id.as_deref(), Some(redo.base.id.as_str()));
    let (_, again) = store.complete_item(&caller, &id, true).await.unwrap();
    assert_eq!(again.unwrap().base.id, redo.base.id);
    let (_, items) = store.list_items(&caller, &list, true).await.unwrap();
    assert_eq!(items.len(), 2);
    // A one-off item has no successor.
    let once = add(
        &store,
        &caller,
        &list,
        chore("Fix shelf", None, Recurrence::None, 3),
    )
    .await;
    let (_, none) = store.complete_item(&caller, &once, true).await.unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn a_monthly_chain_keeps_its_day_after_a_short_month() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    let mut id = add(
        &store,
        &caller,
        &list,
        chore("Pay rent", Some("2027-01-31"), Recurrence::Monthly, 1),
    )
    .await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        let (_, next) = store.complete_item(&caller, &id, true).await.unwrap();
        let next = next.unwrap();
        seen.push(next.due_on.clone().unwrap());
        id = next.base.id;
    }
    assert_eq!(
        seen,
        ["2027-02-28", "2027-03-31", "2027-04-30", "2027-05-31"]
    );
}

#[tokio::test]
async fn deleting_a_successor_never_ends_the_chain() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    let id = add(
        &store,
        &caller,
        &list,
        chore("Mow", Some("2027-05-03"), Recurrence::Weekly, 1),
    )
    .await;
    let (_, next) = store.complete_item(&caller, &id, true).await.unwrap();
    let next = next.unwrap();
    store.delete_item(&caller, &next.base.id).await.unwrap();
    let (_, items) = store.list_items(&caller, &list, true).await.unwrap();
    assert_eq!(items[0].next_item_id, None, "no dangling pointer");
    store.complete_item(&caller, &id, false).await.unwrap();
    let (_, again) = store.complete_item(&caller, &id, true).await.unwrap();
    assert_eq!(again.unwrap().due_on.as_deref(), Some("2027-05-10"));
}

#[tokio::test]
async fn the_tally_sums_done_points_per_assignee() {
    let store = FamilyStore::open_memory().unwrap();
    let caller = member("local", "local");
    let list = chores(&store, &caller).await;
    let kid = store
        .create_person(
            &caller,
            NewPerson {
                display_name: "Kid".into(),
                ..NewPerson::default()
            },
        )
        .await
        .unwrap()
        .base
        .id;
    let mut dishes = chore("Dishes", None, Recurrence::None, 10);
    dishes.assignee_person_id = Some(kid.clone());
    let mut room = chore("Tidy room", None, Recurrence::None, 5);
    room.assignee_person_id = Some(kid.clone());
    let a = add(&store, &caller, &list, dishes).await;
    let b = add(&store, &caller, &list, room).await;
    let c = add(
        &store,
        &caller,
        &list,
        chore("Mow lawn", None, Recurrence::None, 20),
    )
    .await;
    add(
        &store,
        &caller,
        &list,
        chore("Not done", None, Recurrence::None, 99),
    )
    .await;
    store
        .complete_item_at(&caller, &a, true, utc(2027, 1, 1, 12, 0))
        .await
        .unwrap();
    store
        .complete_item_at(&caller, &b, true, utc(2027, 2, 1, 12, 0))
        .await
        .unwrap();
    store
        .complete_item_at(&caller, &c, true, utc(2027, 2, 2, 12, 0))
        .await
        .unwrap();

    let tally = store.tally(&caller, None, None).await.unwrap();
    assert_eq!(tally.len(), 2);
    assert_eq!(tally[0].assignee_person_id, None);
    assert_eq!(tally[0].points, 20);
    assert_eq!(tally[1].assignee_person_id.as_deref(), Some(kid.as_str()));
    assert_eq!(tally[1].person_name.as_deref(), Some("Kid"));
    assert_eq!((tally[1].points, tally[1].completed), (15, 2));

    let since = store
        .tally(&caller, Some(&list), Some("2027-01-15"))
        .await
        .unwrap();
    let kid_row = since
        .iter()
        .find(|row| row.assignee_person_id.as_deref() == Some(kid.as_str()))
        .unwrap();
    assert_eq!((kid_row.points, kid_row.completed), (5, 1));
    assert!(store.tally(&caller, None, Some("2027-1-15")).await.is_err());
}

#[tokio::test]
async fn the_complete_route_returns_the_item_and_its_successor() {
    let state = memory_state();
    let app = app(&state);
    let (_, list) = send(
        &app,
        "POST",
        "/lists",
        None,
        None,
        Some(json!({ "name": "Chores", "kind": "chores" })),
    )
    .await;
    let list_id = list["id"].as_str().unwrap();
    let (_, added) = send(
        &app,
        "POST",
        &format!("/lists/{list_id}/items"),
        None,
        None,
        Some(json!({ "items": [{ "text": "Walk dog", "dueOn": "2027-01-31", "recurrence": "monthly", "points": 4 }] })),
    )
    .await;
    let item_id = added["items"][0]["id"].as_str().unwrap();
    let (status, done) = send(
        &app,
        "POST",
        &format!("/items/{item_id}/complete"),
        None,
        None,
        Some(json!({ "done": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["item"]["done"], true);
    assert_eq!(done["next"]["dueOn"], "2027-02-28");
    assert_eq!(done["item"]["nextItemId"], done["next"]["id"]);
    let (_, undone) = send(
        &app,
        "POST",
        &format!("/items/{item_id}/complete"),
        None,
        None,
        Some(json!({ "done": false })),
    )
    .await;
    assert_eq!(undone["next"], serde_json::Value::Null);
    let (_, tally) = send(
        &app,
        "GET",
        &format!("/chores/tally?listId={list_id}"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(tally["tally"], json!([]));
    let (status, too_many) = send(
        &app,
        "POST",
        &format!("/lists/{list_id}/items"),
        None,
        None,
        Some(json!({ "items": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(too_many["code"], "validation");
}
