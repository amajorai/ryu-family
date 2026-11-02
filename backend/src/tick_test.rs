//! Unit tests for the reminder calendar math and message composer.

use super::*;
use crate::models::Visibility;

fn ymd(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

#[test]
fn feb_29_occurs_on_mar_1_in_non_leap_years() {
    assert_eq!(occurrence_in_year(2, 29, 2027), ymd(2027, 3, 1));
    assert_eq!(occurrence_in_year(2, 29, 2028), ymd(2028, 2, 29));
    assert_eq!(next_occurrence(2, 29, ymd(2027, 3, 1)), ymd(2027, 3, 1));
    assert_eq!(next_occurrence(2, 29, ymd(2027, 3, 2)), ymd(2028, 2, 29));
}

#[test]
fn next_occurrence_is_today_or_later() {
    assert_eq!(
        next_occurrence(10, 13, ymd(2026, 10, 13)),
        ymd(2026, 10, 13)
    );
    assert_eq!(next_occurrence(1, 3, ymd(2026, 12, 27)), ymd(2027, 1, 3));
    assert_eq!(
        turning(DateKind::Birthday, Some(2016), ymd(2027, 1, 3)),
        Some(11)
    );
    assert_eq!(turning(DateKind::Custom, Some(2016), ymd(2027, 1, 3)), None);
    assert_eq!(
        turning(DateKind::Birthday, Some(2030), ymd(2027, 1, 3)),
        None
    );
}

#[test]
fn monthly_recurrence_clamps_to_the_end_of_the_month() {
    assert_eq!(
        advance(ymd(2027, 1, 31), Recurrence::Monthly),
        Some(ymd(2027, 2, 28))
    );
    assert_eq!(
        advance(ymd(2028, 1, 31), Recurrence::Monthly),
        Some(ymd(2028, 2, 29))
    );
    assert_eq!(
        advance(ymd(2026, 12, 31), Recurrence::Daily),
        Some(ymd(2027, 1, 1))
    );
    assert_eq!(
        advance(ymd(2026, 12, 28), Recurrence::Weekly),
        Some(ymd(2027, 1, 4))
    );
    assert_eq!(advance(ymd(2026, 12, 28), Recurrence::None), None);
}

#[test]
fn monthly_recurrence_keeps_its_anchor_day_across_short_months() {
    let mut due = ymd(2027, 1, 31);
    let mut seen = Vec::new();
    for _ in 0..4 {
        due = advance_anchored(due, Recurrence::Monthly, 31).unwrap();
        seen.push(due);
    }
    assert_eq!(
        seen,
        [
            ymd(2027, 2, 28),
            ymd(2027, 3, 31),
            ymd(2027, 4, 30),
            ymd(2027, 5, 31)
        ]
    );
    assert_eq!(
        advance_anchored(ymd(2027, 2, 28), Recurrence::Monthly, 30),
        Some(ymd(2027, 3, 30))
    );
    assert_eq!(
        advance_anchored(ymd(2027, 12, 15), Recurrence::Monthly, 15),
        Some(ymd(2028, 1, 15))
    );
}

fn person(member: Option<&str>, name: &str) -> PersonRow {
    PersonRow {
        id: "p".into(),
        tenant: "local".into(),
        display_name: name.into(),
        relationship: None,
        member_user_id: member.map(Into::into),
        visibility: Visibility::Family,
        created_by: "local".into(),
        created_at: 0,
        updated_at: 0,
    }
}

fn date(kind: DateKind, label: Option<&str>, year: Option<i32>) -> DateRow {
    DateRow {
        id: "d".into(),
        tenant: "local".into(),
        person_id: "p".into(),
        kind,
        label: label.map(Into::into),
        month: 3,
        day: 14,
        year,
        remind_offsets_days: vec![7, 1, 0],
        tz: None,
        visibility: Visibility::Family,
        created_by: "local".into(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn message_text_follows_the_contract() {
    let ana = person(Some("ana"), "Ana");
    let occurrence = ymd(2026, 3, 14);
    let birthday = compose(
        &date(DateKind::Birthday, None, Some(2016)),
        &ana,
        occurrence,
        7,
        "bo",
        2,
    );
    assert_eq!(birthday.title, "Ana's birthday is on Saturday, March 14");
    assert_eq!(
        birthday.body,
        "They turn 10. Gift ideas saved in Family: 2."
    );
    assert_eq!(birthday.target_user_id.as_deref(), Some("bo"));
    // Core renders the same text per locale from the messages.
    assert_eq!(
        birthday.i18n,
        Some(serde_json::json!({
            "title": {
                "message": "{name}'s birthday is on {date}",
                "args": { "name": "Ana", "date": { "date": "2026-03-14" } },
            },
            "body": [
                { "message": "They turn {age}.", "args": { "age": 10 } },
                { "message": "Gift ideas saved in Family: {count}.", "args": { "count": 2 } },
            ],
            "join": "sentences",
        }))
    );

    let anniversary = compose(
        &date(DateKind::Anniversary, None, Some(2006)),
        &ana,
        occurrence,
        1,
        "ana",
        3,
    );
    assert_eq!(anniversary.title, "Anniversary for Ana is tomorrow");
    // The honoree never hears about gifts, whatever the count.
    assert_eq!(anniversary.body, "First celebrated in 2006.");
    let wire = anniversary.i18n.unwrap();
    assert_eq!(
        wire["title"]["message"],
        "Anniversary for {name} is tomorrow"
    );
    assert!(wire["title"]["args"].get("date").is_none());
    assert_eq!(wire["body"][0]["args"]["year"], "2006");

    let custom = compose(
        &date(DateKind::Custom, Some("Recital"), None),
        &ana,
        occurrence,
        0,
        "local",
        1,
    );
    assert_eq!(custom.title, "It's Recital for Ana today");
    assert_eq!(custom.body, "Gift ideas saved in Family: 1.");
    let wire = custom.i18n.unwrap();
    assert_eq!(wire["title"]["message"], "It's {label} for {name} today");
    assert_eq!(wire["title"]["args"]["label"], "Recital");

    let unlabeled = compose(
        &date(DateKind::Custom, None, None),
        &ana,
        occurrence,
        0,
        "ana",
        0,
    );
    assert_eq!(unlabeled.title, "Important date for Ana is today");
    assert_eq!(unlabeled.body, "");
    assert_eq!(unlabeled.i18n.unwrap()["body"], serde_json::json!([]));
    assert_eq!(custom.target_user_id, None);
}

#[test]
fn long_names_are_truncated_to_fit_the_title() {
    let long = person(None, &"é".repeat(400));
    let message = compose(
        &date(DateKind::Birthday, None, None),
        &long,
        ymd(2026, 3, 14),
        0,
        "bo",
        0,
    );
    assert!(message.title.len() <= MAX_TITLE_BYTES);
    assert!(message.title.ends_with("'s birthday is today"));
}
