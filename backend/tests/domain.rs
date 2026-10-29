//! Store behaviour: people and linking, dates, visibility rules, sensitive
//! preferences, settings and agent access, the agenda, and the audit trail.

mod common;

use common::{member, utc};
use ryu_family::{authz::Caller, models::*, store::FamilyStore};

const T: &str = "org:o1";

async fn store() -> FamilyStore {
    let store = FamilyStore::open_memory().unwrap();
    store.ensure_settings(T, "bo").await.unwrap();
    store
}

async fn person(
    store: &FamilyStore,
    caller: &Caller,
    name: &str,
    visibility: Option<Visibility>,
) -> Person {
    store
        .create_person(
            caller,
            NewPerson {
                display_name: name.into(),
                relationship: None,
                visibility,
            },
        )
        .await
        .unwrap()
}

fn date(person_id: &str, kind: DateKind, month: i64, day: i64) -> NewDate {
    NewDate {
        person_id: person_id.into(),
        kind,
        label: None,
        month,
        day,
        year: None,
        remind_offsets_days: None,
        tz: None,
        visibility: None,
    }
}

#[tokio::test]
async fn people_patch_keeps_omitted_fields_and_clears_explicit_nulls() {
    let store = store().await;
    let bo = member(T, "bo");
    let created = store
        .create_person(
            &bo,
            NewPerson {
                display_name: " Gran ".into(),
                relationship: Some("grandmother".into()),
                visibility: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.display_name, "Gran");
    let kept = store
        .update_person(
            &bo,
            &created.base.id,
            PersonPatch {
                display_name: Some("Granny".into()),
                ..PersonPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(kept.relationship.as_deref(), Some("grandmother"));
    let patch: PersonPatch =
        serde_json::from_value(serde_json::json!({ "relationship": null })).unwrap();
    let cleared = store
        .update_person(&bo, &created.base.id, patch)
        .await
        .unwrap();
    assert_eq!(cleared.relationship, None);
    assert!(matches!(
        store
            .update_person(&bo, &created.base.id, PersonPatch::default())
            .await,
        Err(FamilyError::Validation(_))
    ));
}

#[tokio::test]
async fn linking_is_once_per_person_and_once_per_member() {
    let store = store().await;
    let bo = member(T, "bo");
    let ana = member(T, "ana");
    let a = person(&store, &bo, "Ana", None).await;
    let b = person(&store, &bo, "Also Ana", None).await;
    let hidden = person(&store, &bo, "Hidden", Some(Visibility::Private)).await;
    let linked = store.claim_person(&ana, &a.base.id).await.unwrap();
    assert!(linked.is_me && linked.member_linked);
    assert!(!store
        .list_people(&bo)
        .await
        .unwrap()
        .iter()
        .any(|p| p.is_me));
    assert!(matches!(
        store.claim_person(&ana, &b.base.id).await,
        Err(FamilyError::Conflict(_))
    ));
    assert!(matches!(
        store.claim_person(&member(T, "cy"), &a.base.id).await,
        Err(FamilyError::Conflict(_))
    ));
    assert!(matches!(
        store.claim_person(&member(T, "cy"), &hidden.base.id).await,
        Err(FamilyError::NotFound)
    ));
    assert!(matches!(
        store.claim_person(&Caller::shared(T), &b.base.id).await,
        Err(FamilyError::Forbidden(_))
    ));
    let me = store.get_settings(&ana).await.unwrap().me;
    assert_eq!(me.person_id.as_deref(), Some(a.base.id.as_str()));
    // Only the creator may delete; others get 403, and so does the linked
    // member (the cascade would wipe surprises planned for them).
    assert!(matches!(
        store.delete_person(&member(T, "cy"), &a.base.id).await,
        Err(FamilyError::Forbidden(_))
    ));
    assert!(matches!(
        store.delete_person(&ana, &a.base.id).await,
        Err(FamilyError::Forbidden(_))
    ));
    store.delete_person(&bo, &a.base.id).await.unwrap();
}

#[tokio::test]
async fn deleting_a_person_cascades_their_rows() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    store
        .create_date(&bo, date(&kid.base.id, DateKind::Birthday, 1, 1))
        .await
        .unwrap();
    store
        .create_gift(
            &bo,
            NewGift {
                person_id: kid.base.id.clone(),
                idea: "Lego".into(),
                ..NewGift::default()
            },
        )
        .await
        .unwrap();
    store
        .create_wish(
            &bo,
            NewWish {
                person_id: kid.base.id.clone(),
                title: "Bike".into(),
                ..NewWish::default()
            },
        )
        .await
        .unwrap();
    store.delete_person(&bo, &kid.base.id).await.unwrap();
    assert!(store.list_dates(&bo, None).await.unwrap().is_empty());
    assert!(store.list_gifts(&bo, None, None).await.unwrap().is_empty());
    assert!(store.list_wishlist(&bo, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn dates_validate_and_default_their_lead_times_from_settings() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    store
        .patch_settings(
            &bo,
            SettingsPatch {
                default_remind_offsets_days: Some(vec![0, 3]),
                ..SettingsPatch::default()
            },
        )
        .await
        .unwrap();
    let created = store
        .create_date(&bo, date(&kid.base.id, DateKind::Birthday, 2, 29))
        .await
        .unwrap();
    assert_eq!(created.remind_offsets_days, vec![3, 0]);
    for (month, day) in [(2, 30), (13, 1), (4, 31)] {
        assert!(matches!(
            store
                .create_date(&bo, date(&kid.base.id, DateKind::Custom, month, day))
                .await,
            Err(FamilyError::Validation(_))
        ));
    }
    let mut bad_tz = date(&kid.base.id, DateKind::Custom, 1, 1);
    bad_tz.tz = Some("Nowhere/Land".into());
    assert!(matches!(
        store.create_date(&bo, bad_tz).await,
        Err(FamilyError::Validation(_))
    ));
    assert!(matches!(
        store
            .create_date(&bo, date("missing", DateKind::Custom, 1, 1))
            .await,
        Err(FamilyError::NotFound)
    ));
    let patch: DatePatch =
        serde_json::from_value(serde_json::json!({ "tz": "Europe/Paris", "label": "Leap day" }))
            .unwrap();
    let updated = store
        .update_date(&bo, &created.base.id, patch)
        .await
        .unwrap();
    assert_eq!(updated.tz.as_deref(), Some("Europe/Paris"));
    let cleared: DatePatch = serde_json::from_value(serde_json::json!({ "tz": null })).unwrap();
    assert_eq!(
        store
            .update_date(&bo, &created.base.id, cleared)
            .await
            .unwrap()
            .tz,
        None
    );
}

#[tokio::test]
async fn private_rows_are_their_owners_alone() {
    let store = store().await;
    let bo = member(T, "bo");
    let cy = member(T, "cy");
    let kid = person(&store, &bo, "Kid", None).await;
    let mut private_date = date(&kid.base.id, DateKind::Custom, 6, 1);
    private_date.visibility = Some(Visibility::Private);
    let hidden = store.create_date(&bo, private_date).await.unwrap();
    assert!(store.list_dates(&cy, None).await.unwrap().is_empty());
    assert!(matches!(
        store
            .update_date(
                &cy,
                &hidden.base.id,
                DatePatch {
                    day: Some(2),
                    ..DatePatch::default()
                }
            )
            .await,
        Err(FamilyError::NotFound)
    ));
    let list = store
        .create_list(
            &bo,
            NewList {
                name: "Bo's".into(),
                kind: ListKind::Todo,
                visibility: Some(Visibility::Private),
            },
        )
        .await
        .unwrap();
    store
        .add_items(
            &bo,
            &list.base.id,
            vec![NewListItem {
                text: "Secret".into(),
                ..NewListItem::default()
            }],
        )
        .await
        .unwrap();
    assert!(store.list_lists(&cy, None).await.unwrap().is_empty());
    assert!(matches!(
        store.list_items(&cy, &list.base.id, true).await,
        Err(FamilyError::NotFound)
    ));
    // Sharing the list moves its items with it.
    store
        .update_list(
            &bo,
            &list.base.id,
            ListPatch {
                visibility: Some(Visibility::Family),
                ..ListPatch::default()
            },
        )
        .await
        .unwrap();
    let (_, items) = store.list_items(&cy, &list.base.id, true).await.unwrap();
    assert_eq!(items[0].base.visibility, Visibility::Family);
    // The shared lane may not delete a list or make anything private.
    assert!(matches!(
        store.delete_list(&Caller::shared(T), &list.base.id).await,
        Err(FamilyError::Forbidden(_))
    ));
    assert!(matches!(
        store
            .update_list(
                &Caller::shared(T),
                &list.base.id,
                ListPatch {
                    visibility: Some(Visibility::Private),
                    ..ListPatch::default()
                }
            )
            .await,
        Err(FamilyError::Forbidden(_))
    ));
    // A private person makes their rows effectively private.
    let secret = person(&store, &bo, "Surprise guest", Some(Visibility::Private)).await;
    store
        .create_date(&bo, date(&secret.base.id, DateKind::Birthday, 3, 3))
        .await
        .unwrap();
    assert_eq!(store.list_dates(&cy, None).await.unwrap().len(), 0);
    assert_eq!(store.list_dates(&bo, None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn sensitive_preferences_stay_private_until_the_person_shares_them() {
    let store = store().await;
    let bo = member(T, "bo");
    let ana = member(T, "ana");
    let cy = member(T, "cy");
    let ana_person = person(&store, &bo, "Ana", None).await;
    store.claim_person(&ana, &ana_person.base.id).await.unwrap();
    let pref = |sensitive: bool, visibility: Option<Visibility>| NewPreference {
        person_id: ana_person.base.id.clone(),
        key: "allergy".into(),
        value: "peanuts".into(),
        sensitive: Some(sensitive),
        visibility,
    };
    let by_bo = store
        .create_preference(&bo, pref(true, None))
        .await
        .unwrap();
    assert_eq!(by_bo.base.visibility, Visibility::Private);
    assert!(!by_bo.can_change_visibility);
    assert!(matches!(
        store
            .create_preference(&bo, pref(true, Some(Visibility::Family)))
            .await,
        Err(FamilyError::Forbidden(_))
    ));
    assert!(store
        .list_preferences(&cy, &ana_person.base.id)
        .await
        .unwrap()
        .is_empty());
    let for_ana = store
        .list_preferences(&ana, &ana_person.base.id)
        .await
        .unwrap();
    assert_eq!(for_ana.len(), 1);
    assert!(for_ana[0].can_change_visibility);
    assert!(matches!(
        store
            .update_preference(
                &bo,
                &by_bo.base.id,
                PreferencePatch {
                    visibility: Some(Visibility::Family),
                    ..PreferencePatch::default()
                }
            )
            .await,
        Err(FamilyError::Forbidden(_))
    ));
    assert!(matches!(
        store
            .update_preference(
                &bo,
                &by_bo.base.id,
                PreferencePatch {
                    sensitive: Some(false),
                    ..PreferencePatch::default()
                }
            )
            .await,
        Err(FamilyError::Forbidden(_))
    ));
    let shared = store
        .update_preference(
            &ana,
            &by_bo.base.id,
            PreferencePatch {
                visibility: Some(Visibility::Family),
                ..PreferencePatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(shared.base.visibility, Visibility::Family);
    assert_eq!(
        store
            .list_preferences(&cy, &ana_person.base.id)
            .await
            .unwrap()
            .len(),
        1
    );
    // Marking an ordinary row sensitive makes it private by default.
    let plain = store
        .create_preference(&cy, pref(false, None))
        .await
        .unwrap();
    let marked = store
        .update_preference(
            &cy,
            &plain.base.id,
            PreferencePatch {
                sensitive: Some(true),
                ..PreferencePatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(marked.base.visibility, Visibility::Private);
    // A guardian proxy controls an unlinked child's sensitive rows.
    let child = person(&store, &bo, "Baby", None).await;
    let proxy = store
        .create_preference(
            &bo,
            NewPreference {
                person_id: child.base.id.clone(),
                key: "formula".into(),
                value: "brand X".into(),
                sensitive: Some(true),
                visibility: Some(Visibility::Family),
            },
        )
        .await
        .unwrap();
    assert_eq!(proxy.base.visibility, Visibility::Family);
    assert!(matches!(
        store.list_preferences(&cy, "").await,
        Err(FamilyError::Validation(_))
    ));
}

#[tokio::test]
async fn agent_access_belongs_to_one_tenant_at_a_time() {
    let store = FamilyStore::open_memory().unwrap();
    store.ensure_settings("org:first", "a").await.unwrap();
    store.ensure_settings("org:second", "b").await.unwrap();
    // Nobody holds the lane until a family opts in.
    assert_eq!(store.agent_tenant().await.unwrap(), None);
    let enable = SettingsPatch {
        agent_access: Some(true),
        ..SettingsPatch::default()
    };
    let disable = SettingsPatch {
        agent_access: Some(false),
        ..SettingsPatch::default()
    };
    let first = member("org:first", "a");
    let second = member("org:second", "b");
    let response = store.patch_settings(&first, enable.clone()).await.unwrap();
    assert!(response.settings.agent_access);
    assert_eq!(
        store.agent_tenant().await.unwrap().as_deref(),
        Some("org:first")
    );
    // Another tenant cannot take the lane away.
    assert!(matches!(
        store.patch_settings(&second, enable.clone()).await,
        Err(FamilyError::Conflict(_))
    ));
    assert_eq!(
        store.agent_tenant().await.unwrap().as_deref(),
        Some("org:first")
    );
    assert!(
        !store
            .get_settings(&second)
            .await
            .unwrap()
            .settings
            .agent_access
    );
    // Once the holder lets go, the other tenant may opt in.
    store.patch_settings(&first, disable.clone()).await.unwrap();
    assert_eq!(store.agent_tenant().await.unwrap(), None);
    store.patch_settings(&second, enable).await.unwrap();
    assert_eq!(
        store.agent_tenant().await.unwrap().as_deref(),
        Some("org:second")
    );
    store.patch_settings(&second, disable).await.unwrap();
    assert_eq!(store.agent_tenant().await.unwrap(), None);
    assert!(matches!(
        store
            .patch_settings(&second, SettingsPatch::default())
            .await,
        Err(FamilyError::Validation(_))
    ));
    assert!(matches!(
        store
            .patch_settings(
                &second,
                SettingsPatch {
                    currency: Some("eur".into()),
                    ..SettingsPatch::default()
                }
            )
            .await,
        Err(FamilyError::Validation(_))
    ));
}

#[tokio::test]
async fn upcoming_merges_dates_and_due_items_with_overdue_first() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    let mut birthday = date(&kid.base.id, DateKind::Birthday, 10, 20);
    birthday.year = Some(2019);
    store.create_date(&bo, birthday).await.unwrap();
    store
        .create_date(&bo, date(&kid.base.id, DateKind::Custom, 12, 25))
        .await
        .unwrap();
    let list = store
        .create_list(
            &bo,
            NewList {
                name: "Todo".into(),
                kind: ListKind::Todo,
                visibility: None,
            },
        )
        .await
        .unwrap();
    store
        .add_items(
            &bo,
            &list.base.id,
            vec![
                NewListItem {
                    text: "Overdue".into(),
                    due_on: Some("2026-10-10".into()),
                    ..NewListItem::default()
                },
                NewListItem {
                    text: "Same day".into(),
                    due_on: Some("2026-10-20".into()),
                    ..NewListItem::default()
                },
                NewListItem {
                    text: "Far".into(),
                    due_on: Some("2027-01-01".into()),
                    ..NewListItem::default()
                },
                NewListItem {
                    text: "Undated".into(),
                    ..NewListItem::default()
                },
            ],
        )
        .await
        .unwrap();
    let agenda = store
        .upcoming_at(&bo, Some(30), utc(2026, 10, 13, 12, 0))
        .await
        .unwrap();
    assert_eq!(agenda.today, "2026-10-13");
    let summary: Vec<(String, i64)> = agenda
        .entries
        .iter()
        .map(|entry| match entry {
            UpcomingEntry::Date(date) => (format!("date:{}", date.person_name), date.days_away),
            UpcomingEntry::Item(item) => (format!("item:{}", item.text), item.days_away),
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("item:Overdue".to_owned(), -3),
            ("date:Kid".to_owned(), 7),
            ("item:Same day".to_owned(), 7),
        ]
    );
    let UpcomingEntry::Date(entry) = &agenda.entries[1] else {
        panic!("date entry")
    };
    assert_eq!(entry.turning, Some(7));
    let UpcomingEntry::Item(overdue) = &agenda.entries[0] else {
        panic!("item entry")
    };
    assert!(overdue.overdue);
    let json = serde_json::to_value(&agenda).unwrap();
    assert_eq!(json["entries"][0]["type"], "item");
    assert_eq!(json["entries"][1]["type"], "date");
    assert_eq!(json["entries"][1]["personName"], "Kid");
    let wide = store
        .upcoming_at(&bo, Some(366), utc(2026, 10, 13, 12, 0))
        .await
        .unwrap();
    assert_eq!(wide.entries.len(), 5);
}

#[tokio::test]
async fn every_write_is_audited_without_values_and_the_trail_is_append_only() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    store
        .create_gift(
            &bo,
            NewGift {
                person_id: kid.base.id.clone(),
                idea: "Very secret idea".into(),
                ..NewGift::default()
            },
        )
        .await
        .unwrap();
    let rows = store.audit_rows(T).await.unwrap();
    let actions: Vec<&str> = rows.iter().map(|row| row.action.as_str()).collect();
    assert_eq!(
        actions,
        ["settings.create", "people.create", "gift_ideas.create"]
    );
    assert!(rows.iter().all(|row| row.actor == "bo"));
    assert!(rows.iter().all(|row| !row.row_id.contains("secret")));
    assert!(store
        .execute_raw("UPDATE audit SET actor = 'mallory'")
        .await
        .is_err());
    assert!(store.execute_raw("DELETE FROM audit").await.is_err());
    assert_eq!(store.audit_rows(T).await.unwrap().len(), 3);
}

#[tokio::test]
async fn validation_rejects_bad_urls_currencies_and_lengths() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    let bad = [
        NewWish {
            person_id: kid.base.id.clone(),
            title: "x".into(),
            url: Some("ftp://example.com".into()),
            ..NewWish::default()
        },
        NewWish {
            person_id: kid.base.id.clone(),
            title: "x".into(),
            priority: Some(4),
            ..NewWish::default()
        },
        NewWish {
            person_id: kid.base.id.clone(),
            title: "t".repeat(201),
            ..NewWish::default()
        },
    ];
    for wish in bad {
        assert!(matches!(
            store.create_wish(&bo, wish).await,
            Err(FamilyError::Validation(_))
        ));
    }
    assert!(matches!(
        store
            .create_gift(
                &bo,
                NewGift {
                    person_id: kid.base.id.clone(),
                    idea: "x".into(),
                    budget_minor: Some(0),
                    ..NewGift::default()
                }
            )
            .await,
        Err(FamilyError::Validation(_))
    ));
    assert!(matches!(
        store
            .create_gift(
                &bo,
                NewGift {
                    person_id: kid.base.id.clone(),
                    idea: "x".into(),
                    currency: Some("usd".into()),
                    ..NewGift::default()
                }
            )
            .await,
        Err(FamilyError::Validation(_))
    ));
    assert!(matches!(
        store
            .add_items(
                &bo,
                "missing",
                vec![NewListItem {
                    text: "x".into(),
                    ..NewListItem::default()
                }]
            )
            .await,
        Err(FamilyError::NotFound)
    ));
}

#[tokio::test]
async fn binding_the_node_to_a_family_org_drops_personal_agent_access() {
    let store = FamilyStore::open_memory().unwrap();
    store.ensure_settings("local", "local").await.unwrap();
    store
        .patch_settings(
            &member("local", "local"),
            SettingsPatch {
                agent_access: Some(true),
                ..SettingsPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.agent_tenant().await.unwrap().as_deref(),
        Some("local")
    );
    // The node is later bound to a family org: the org's members must never be
    // served the node owner's personal data through the agent.
    store.ensure_settings("org:o1", "bo").await.unwrap();
    assert_eq!(store.agent_tenant().await.unwrap(), None);
    assert!(
        !store
            .get_settings(&member("local", "local"))
            .await
            .unwrap()
            .settings
            .agent_access
    );
}

#[tokio::test]
async fn a_backlog_of_overdue_items_never_crowds_dates_out_of_the_agenda() {
    let store = store().await;
    let bo = member(T, "bo");
    let kid = person(&store, &bo, "Kid", None).await;
    store
        .create_date(&bo, date(&kid.base.id, DateKind::Birthday, 10, 16))
        .await
        .unwrap();
    let list = store
        .create_list(
            &bo,
            NewList {
                name: "Old".into(),
                kind: ListKind::Todo,
                visibility: None,
            },
        )
        .await
        .unwrap();
    for batch in 0..5 {
        let items = (0..50)
            .map(|index| NewListItem {
                text: format!("Old {batch}-{index}"),
                due_on: Some(format!("2026-0{}-{:02}", batch + 1, index % 28 + 1)),
                ..NewListItem::default()
            })
            .collect();
        store.add_items(&bo, &list.base.id, items).await.unwrap();
    }
    store
        .add_items(
            &bo,
            &list.base.id,
            vec![NewListItem {
                text: "This week".into(),
                due_on: Some("2026-10-15".into()),
                ..NewListItem::default()
            }],
        )
        .await
        .unwrap();
    let agenda = store
        .upcoming_at(&bo, Some(30), utc(2026, 10, 13, 12, 0))
        .await
        .unwrap();
    let overdue = agenda
        .entries
        .iter()
        .filter(|entry| matches!(entry, UpcomingEntry::Item(item) if item.overdue))
        .count();
    assert_eq!(overdue, MAX_OVERDUE_ENTRIES);
    assert!(agenda
        .entries
        .iter()
        .any(|entry| matches!(entry, UpcomingEntry::Date(date) if date.person_name == "Kid")));
    assert!(agenda
        .entries
        .iter()
        .any(|entry| matches!(entry, UpcomingEntry::Item(item) if item.text == "This week")));
    // The overdue items kept are the most recently due.
    let UpcomingEntry::Item(oldest_kept) = &agenda.entries[0] else {
        panic!("overdue items sort first")
    };
    assert!(oldest_kept.on.starts_with("2026-05"), "{}", oldest_kept.on);
}
