//! The daily reminder sweep: Feb 29, DST, time zones, lead times, the reminder
//! hour, once-only claims, delivery failures, mutes and the birthday honoree.

mod common;

use common::{member, state_with_host, utc, NotifyStub};
use ryu_family::{
    authz::Caller,
    models::{DateKind, NewDate, NewPerson, SettingsPatch},
    state::{AppState, Config},
    store::{members_of, ClaimKey, FamilyStore},
    tick,
};
use serde_json::Value;

const LOCAL: &str = "local";

async fn local_setup(stub: &NotifyStub, tz: &str, hour: i64) -> (AppState, Caller) {
    let state = state_with_host(FamilyStore::open_memory().unwrap(), stub);
    state.store.ensure_settings(LOCAL, LOCAL).await.unwrap();
    let caller = member(LOCAL, LOCAL);
    state
        .store
        .patch_settings(
            &caller,
            SettingsPatch {
                tz: Some(tz.to_owned()),
                reminder_hour: Some(hour),
                ..SettingsPatch::default()
            },
        )
        .await
        .unwrap();
    (state, caller)
}

async fn add_date(
    state: &AppState,
    caller: &Caller,
    name: &str,
    kind: DateKind,
    (month, day): (i64, i64),
    offsets: &[i64],
    tz: Option<&str>,
) -> String {
    let person = state
        .store
        .create_person(
            caller,
            NewPerson {
                display_name: name.to_owned(),
                ..NewPerson::default()
            },
        )
        .await
        .unwrap();
    state
        .store
        .create_date(
            caller,
            NewDate {
                person_id: person.base.id,
                kind,
                label: None,
                month,
                day,
                year: None,
                remind_offsets_days: Some(offsets.to_vec()),
                tz: tz.map(str::to_owned),
                visibility: None,
            },
        )
        .await
        .unwrap()
        .base
        .id
}

fn key(date_id: &str, offset: i64, year: i32, recipient: &str) -> ClaimKey {
    ClaimKey {
        date_id: date_id.to_owned(),
        offset_days: offset,
        occurrence_year: year,
        recipient: recipient.to_owned(),
    }
}

async fn titles(stub: &NotifyStub) -> Vec<String> {
    stub.bodies()
        .await
        .iter()
        .map(|body| body["title"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn feb_29_is_observed_on_mar_1_in_non_leap_years() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    add_date(
        &state,
        &caller,
        "Leo",
        DateKind::Birthday,
        (2, 29),
        &[0],
        None,
    )
    .await;

    assert_eq!(
        tick::once_at(&state, utc(2027, 2, 28, 12, 0))
            .await
            .unwrap()
            .sent,
        0
    );
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 1, 9, 30))
            .await
            .unwrap()
            .sent,
        1
    );
    let body = &stub.bodies().await[0];
    assert_eq!(body["title"], "Leo's birthday is today");
    assert_eq!(body["body"], "Monday, March 1");
    // In a leap year it is Feb 29 itself, and Mar 1 is quiet.
    assert_eq!(
        tick::once_at(&state, utc(2028, 2, 29, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(
        tick::once_at(&state, utc(2028, 3, 1, 9, 0))
            .await
            .unwrap()
            .sent,
        0
    );
}

#[tokio::test]
async fn new_york_spring_forward_does_not_skip_a_reminder_in_the_missing_hour() {
    let stub = NotifyStub::start().await;
    // 02:00 does not exist in New York on 2027-03-14 (clocks jump to 03:00).
    let (state, caller) = local_setup(&stub, "America/New_York", 2).await;
    add_date(
        &state,
        &caller,
        "Mia",
        DateKind::Birthday,
        (3, 14),
        &[0],
        None,
    )
    .await;
    // 06:30Z = 01:30 EST: before the reminder hour.
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 14, 6, 30))
            .await
            .unwrap()
            .sent,
        0
    );
    // 07:00Z = 03:00 EDT: the first local instant at or after 02:00.
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 14, 7, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 14, 20, 0))
            .await
            .unwrap()
            .sent,
        0
    );
    // 03:59Z on Mar 15 is still Mar 14 locally (23:59 EDT): no second send.
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 15, 3, 59))
            .await
            .unwrap()
            .sent,
        0
    );
}

#[tokio::test]
async fn london_fall_back_repeats_an_hour_but_sends_once() {
    let stub = NotifyStub::start().await;
    // 01:00 happens twice in London on 2027-10-31.
    let (state, caller) = local_setup(&stub, "Europe/London", 1).await;
    add_date(
        &state,
        &caller,
        "Oli",
        DateKind::Birthday,
        (10, 31),
        &[0],
        None,
    )
    .await;
    // 23:30Z on Oct 30 = 00:30 BST on Oct 31: before the hour.
    assert_eq!(
        tick::once_at(&state, utc(2027, 10, 30, 23, 30))
            .await
            .unwrap()
            .sent,
        0
    );
    // 00:00Z = 01:00 BST, the first 01:00.
    assert_eq!(
        tick::once_at(&state, utc(2027, 10, 31, 0, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    // 01:00Z = 01:00 GMT, the repeated hour: already claimed.
    assert_eq!(
        tick::once_at(&state, utc(2027, 10, 31, 1, 0))
            .await
            .unwrap()
            .sent,
        0
    );
    assert_eq!(stub.count().await, 1);
}

#[tokio::test]
async fn a_date_time_zone_overrides_the_family_zone() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    add_date(
        &state,
        &caller,
        "Kiri",
        DateKind::Birthday,
        (7, 1),
        &[0],
        Some("Pacific/Auckland"),
    )
    .await;
    add_date(
        &state,
        &caller,
        "Uma",
        DateKind::Birthday,
        (7, 1),
        &[0],
        None,
    )
    .await;
    // 21:30Z on Jun 30 = 09:30 on Jul 1 in Auckland (NZST, UTC+12); still Jun 30 in UTC.
    assert_eq!(
        tick::once_at(&state, utc(2027, 6, 30, 21, 30))
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(titles(&stub).await, ["Kiri's birthday is today"]);
    assert_eq!(
        tick::once_at(&state, utc(2027, 7, 1, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(titles(&stub).await[1], "Uma's birthday is today");
}

#[tokio::test]
async fn lead_times_fire_on_their_own_days_only() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    let date_id = add_date(
        &state,
        &caller,
        "Zoe",
        DateKind::Birthday,
        (1, 3),
        &[7, 1, 0],
        None,
    )
    .await;
    let mut sent = 0;
    for day in 20..=31 {
        sent += tick::once_at(&state, utc(2026, 12, day, 10, 0))
            .await
            .unwrap()
            .sent;
    }
    for day in 1..=5 {
        sent += tick::once_at(&state, utc(2027, 1, day, 10, 0))
            .await
            .unwrap()
            .sent;
    }
    assert_eq!(sent, 3);
    assert_eq!(
        titles(&stub).await,
        [
            "Zoe's birthday is in 7 days",
            "Zoe's birthday is tomorrow",
            "Zoe's birthday is today"
        ]
    );
    // The Dec 27 reminder is claimed against the occasion's year, not the reminder's.
    assert_eq!(
        state
            .store
            .claim_status(&key(&date_id, 7, 2027, LOCAL))
            .await
            .unwrap()
            .as_deref(),
        Some("sent")
    );
    assert_eq!(
        state
            .store
            .claim_status(&key(&date_id, 7, 2026, LOCAL))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn nothing_is_sent_before_the_reminder_hour() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "Asia/Tokyo", 9).await;
    add_date(
        &state,
        &caller,
        "Ren",
        DateKind::Birthday,
        (5, 5),
        &[0],
        None,
    )
    .await;
    // 23:59Z on May 4 = 08:59 JST on May 5.
    assert_eq!(
        tick::once_at(&state, utc(2027, 5, 4, 23, 59))
            .await
            .unwrap()
            .sent,
        0
    );
    assert_eq!(
        tick::once_at(&state, utc(2027, 5, 5, 0, 0))
            .await
            .unwrap()
            .sent,
        1
    );
}

#[tokio::test]
async fn a_second_sweep_sends_nothing_and_racing_sweeps_send_once() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    add_date(
        &state,
        &caller,
        "Ivy",
        DateKind::Birthday,
        (8, 8),
        &[0],
        None,
    )
    .await;
    let at = utc(2027, 8, 8, 9, 5);
    let (first, second) = tokio::join!(tick::once_at(&state, at), tick::once_at(&state, at));
    assert_eq!(first.unwrap().sent + second.unwrap().sent, 1);
    assert_eq!(tick::once_at(&state, at).await.unwrap().sent, 0);
    assert_eq!(stub.count().await, 1);
}

#[tokio::test]
async fn two_processes_on_one_database_send_once() {
    let stub = NotifyStub::start().await;
    let dir = std::env::temp_dir().join(format!("ryu-family-race-{}", uuid_like()));
    let path = dir.join("family.db");
    let first_store = FamilyStore::open(path.clone()).unwrap();
    let second_store = FamilyStore::open(path).unwrap();
    first_store.ensure_settings(LOCAL, LOCAL).await.unwrap();
    let first = state_with_host(first_store, &stub);
    let second = state_with_host(second_store, &stub);
    add_date(
        &first,
        &member(LOCAL, LOCAL),
        "Ada",
        DateKind::Birthday,
        (9, 9),
        &[0],
        None,
    )
    .await;
    let at = utc(2027, 9, 9, 12, 0);
    let (a, b) = tokio::join!(tick::once_at(&first, at), tick::once_at(&second, at));
    assert_eq!(a.unwrap().sent + b.unwrap().sent, 1);
    assert_eq!(stub.count().await, 1);
    let _ = std::fs::remove_dir_all(dir);
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

#[tokio::test]
async fn server_errors_release_the_claim_and_client_errors_keep_it_failed() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    let date_id = add_date(
        &state,
        &caller,
        "Max",
        DateKind::Birthday,
        (4, 4),
        &[0],
        None,
    )
    .await;
    let claim = key(&date_id, 0, 2027, LOCAL);

    stub.set_status(503);
    let report = tick::once_at(&state, utc(2027, 4, 4, 9, 0)).await.unwrap();
    assert_eq!(report.released, 1);
    assert_eq!(state.store.claim_status(&claim).await.unwrap(), None);
    stub.set_status(200);
    let report = tick::once_at(&state, utc(2027, 4, 4, 10, 0)).await.unwrap();
    assert_eq!(report.sent, 1, "a later tick on the same local day retries");
    assert_eq!(
        state.store.claim_status(&claim).await.unwrap().as_deref(),
        Some("sent")
    );

    let other = add_date(
        &state,
        &caller,
        "Nia",
        DateKind::Birthday,
        (4, 5),
        &[0],
        None,
    )
    .await;
    stub.set_status(403);
    let report = tick::once_at(&state, utc(2027, 4, 5, 9, 0)).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(
        state
            .store
            .claim_status(&key(&other, 0, 2027, LOCAL))
            .await
            .unwrap()
            .as_deref(),
        Some("failed")
    );
    stub.set_status(200);
    stub.clear().await;
    assert_eq!(
        tick::once_at(&state, utc(2027, 4, 5, 11, 0))
            .await
            .unwrap()
            .sent,
        0
    );
    assert_eq!(stub.count().await, 0, "a rejected reminder is not retried");
}

#[tokio::test]
async fn deliveries_carry_the_plugin_id_and_bearer_and_omit_the_local_target() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    add_date(&state, &caller, "Eva", DateKind::Custom, (6, 6), &[1], None).await;
    tick::once_at(&state, utc(2027, 6, 5, 9, 0)).await.unwrap();
    let calls = stub.calls.lock().await;
    let (headers, body) = &calls[0];
    assert_eq!(headers["authorization"], "Bearer ext-token");
    assert_eq!(headers["x-ryu-plugin-id"], "@ryu/family");
    assert!(body.get("target_user_id").is_none());
    assert_eq!(body["level"], "info");
    assert_eq!(body["title"], "Important date for Eva is tomorrow");
}

#[tokio::test]
async fn without_a_core_callback_nothing_is_claimed() {
    let state = AppState::new(
        FamilyStore::open_memory().unwrap(),
        Config::for_tests("t"),
        None,
    );
    state.store.ensure_settings(LOCAL, LOCAL).await.unwrap();
    let date_id = add_date(
        &state,
        &member(LOCAL, LOCAL),
        "Ula",
        DateKind::Birthday,
        (2, 2),
        &[0],
        None,
    )
    .await;
    let report = tick::once_at(&state, utc(2027, 2, 2, 12, 0)).await.unwrap();
    assert_eq!(report, tick::TickReport::default());
    assert_eq!(
        state
            .store
            .claim_status(&key(&date_id, 0, 2027, LOCAL))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn muted_members_and_the_birthday_honoree_are_skipped() {
    let stub = NotifyStub::start().await;
    let state = state_with_host(FamilyStore::open_memory().unwrap(), &stub);
    let tenant = "org:o1";
    for user in ["ana", "bo", "cy", "di"] {
        state.store.ensure_settings(tenant, user).await.unwrap();
    }
    let bo = member(tenant, "bo");
    let ana_person = state
        .store
        .create_person(
            &bo,
            NewPerson {
                display_name: "Ana".into(),
                ..NewPerson::default()
            },
        )
        .await
        .unwrap();
    state
        .store
        .claim_person(&member(tenant, "ana"), &ana_person.base.id)
        .await
        .unwrap();
    // cy and di become members by writing to the hub.
    for user in ["cy", "di"] {
        state
            .store
            .create_person(
                &member(tenant, user),
                NewPerson {
                    display_name: format!("{user}'s pet"),
                    ..NewPerson::default()
                },
            )
            .await
            .unwrap();
    }
    state
        .store
        .set_muted(&member(tenant, "di"), true)
        .await
        .unwrap();
    let members = members_of(&state.store, tenant).await.unwrap();
    assert_eq!(
        members.into_iter().collect::<Vec<_>>(),
        ["ana", "bo", "cy", "di"]
    );

    state
        .store
        .create_date(
            &bo,
            NewDate {
                person_id: ana_person.base.id.clone(),
                kind: DateKind::Birthday,
                label: None,
                month: 11,
                day: 11,
                year: Some(2015),
                remind_offsets_days: Some(vec![0]),
                tz: None,
                visibility: None,
            },
        )
        .await
        .unwrap();
    let report = tick::once_at(&state, utc(2027, 11, 11, 9, 0))
        .await
        .unwrap();
    assert_eq!(report.sent, 2);
    let targets: Vec<Value> = stub
        .bodies()
        .await
        .iter()
        .map(|body| body["target_user_id"].clone())
        .collect();
    assert_eq!(targets, ["bo", "cy"]);
    assert_eq!(
        stub.bodies().await[0]["body"],
        "Thursday, November 11 · turning 12"
    );
}

#[tokio::test]
async fn a_private_date_reminds_only_its_creator() {
    let stub = NotifyStub::start().await;
    let state = state_with_host(FamilyStore::open_memory().unwrap(), &stub);
    let tenant = "org:o2";
    state.store.ensure_settings(tenant, "bo").await.unwrap();
    let bo = member(tenant, "bo");
    state
        .store
        .create_person(
            &member(tenant, "cy"),
            NewPerson {
                display_name: "Cy's mum".into(),
                ..NewPerson::default()
            },
        )
        .await
        .unwrap();
    let person = state
        .store
        .create_person(
            &bo,
            NewPerson {
                display_name: "Gran".into(),
                ..NewPerson::default()
            },
        )
        .await
        .unwrap();
    state
        .store
        .create_date(
            &bo,
            NewDate {
                person_id: person.base.id,
                kind: DateKind::Anniversary,
                label: Some("Gran and Pop".into()),
                month: 12,
                day: 1,
                year: Some(1970),
                remind_offsets_days: Some(vec![0]),
                tz: None,
                visibility: Some(ryu_family::models::Visibility::Private),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        tick::once_at(&state, utc(2027, 12, 1, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    let body = &stub.bodies().await[0];
    assert_eq!(body["target_user_id"], "bo");
    assert_eq!(body["title"], "Gran and Pop for Gran is today");
    assert_eq!(body["body"], "Wednesday, December 1 · 57 years");
}

#[tokio::test]
async fn a_claim_left_pending_by_a_dead_sweep_is_taken_over_after_the_lease() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    let date_id = add_date(
        &state,
        &caller,
        "Ola",
        DateKind::Birthday,
        (5, 5),
        &[0],
        None,
    )
    .await;
    let claim = key(&date_id, 0, 2027, LOCAL);
    // A sweep claimed at 09:00 and died before delivery finished.
    let claimed_at = utc(2027, 5, 5, 9, 0).timestamp_millis();
    assert!(state
        .store
        .claim_reminder(LOCAL, &claim, claimed_at)
        .await
        .unwrap());
    assert_eq!(
        state.store.claim_status(&claim).await.unwrap().as_deref(),
        Some("pending")
    );
    // Within the lease the claim is still someone else's delivery.
    let soon = utc(2027, 5, 5, 9, 0) + chrono::TimeDelta::seconds(30);
    assert_eq!(tick::once_at(&state, soon).await.unwrap().sent, 0);
    assert_eq!(stub.count().await, 0);
    // After the lease, a later tick on the same local day delivers it once.
    let report = tick::once_at(&state, utc(2027, 5, 5, 9, 5)).await.unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(
        state.store.claim_status(&claim).await.unwrap().as_deref(),
        Some("sent")
    );
    assert_eq!(
        tick::once_at(&state, utc(2027, 5, 5, 9, 30))
            .await
            .unwrap()
            .sent,
        0
    );
    assert_eq!(stub.count().await, 1);
}

#[tokio::test]
async fn correcting_a_date_after_a_reminder_went_out_still_reminds_for_the_real_day() {
    let stub = NotifyStub::start().await;
    let (state, caller) = local_setup(&stub, "UTC", 9).await;
    let date_id = add_date(
        &state,
        &caller,
        "Pia",
        DateKind::Birthday,
        (3, 14),
        &[7, 0],
        None,
    )
    .await;
    // The 7-day reminder for the wrong day (Mar 14) goes out on Mar 7.
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 7, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    // The day is corrected to Mar 21 the same day.
    state
        .store
        .update_date(
            &caller,
            &date_id,
            ryu_family::models::DatePatch {
                day: Some(21),
                ..ryu_family::models::DatePatch::default()
            },
        )
        .await
        .unwrap();
    // The corrected 7-day (Mar 14) and day-of (Mar 21) reminders both go out.
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 14, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(
        tick::once_at(&state, utc(2027, 3, 21, 9, 0))
            .await
            .unwrap()
            .sent,
        1
    );
    let titles: Vec<String> = stub
        .bodies()
        .await
        .iter()
        .map(|body| body["title"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        titles,
        [
            "Pia's birthday is in 7 days",
            "Pia's birthday is in 7 days",
            "Pia's birthday is today"
        ]
    );
}
