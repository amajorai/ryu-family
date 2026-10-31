//! Calendar math and the daily reminder sweep (HTTP process only).
//!
//! The sweep is idempotent and race-safe: each `(date, offset, occurrence year,
//! recipient)` is claimed as `pending` before anything is sent, and only the
//! tick whose write changed a row delivers (the tuition `claim_event` pattern).
//! The claim becomes `sent` after Core accepts it, `failed` on a 4xx, and is
//! released on a retryable error; a `pending` claim left by a process that died
//! mid-delivery is taken over after a lease (see `FamilyStore::claim_reminder`). A reminder is due only on the recipient-family's local calendar day
//! `occurrence - offset`, from `reminder_hour` onwards; there is no catch-up
//! after that local day ends.

use std::time::Duration;

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeDelta, Timelike, Utc};
use chrono_tz::Tz;

use crate::{
    authz,
    host::{DeliverOutcome, Notification, MAX_TITLE_BYTES},
    models::{DateKind, DateRow, FamilyResult, PersonRow, Recurrence},
    state::{AppState, MIN_TICK_SECS},
    store::ClaimKey,
};

// ── Calendar math ─────────────────────────────────────────────────────────────

/// An IANA zone, falling back to UTC for a value that no longer parses.
pub fn resolve_tz(name: &str) -> Tz {
    name.parse::<Tz>().unwrap_or(Tz::UTC)
}

/// The local calendar day at `now` in `tz`.
pub fn local_today(tz: Tz, now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&tz).date_naive()
}

/// The occurrence of a month/day in `year`. Feb 29 occurs on Mar 1 in a
/// non-leap year.
pub fn occurrence_in_year(month: u32, day: u32, year: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day)
        .or_else(|| NaiveDate::from_ymd_opt(year, 3, 1))
        .unwrap_or(NaiveDate::MIN)
}

/// The first occurrence on or after `today`.
pub fn next_occurrence(month: u32, day: u32, today: NaiveDate) -> NaiveDate {
    let this_year = occurrence_in_year(month, day, today.year());
    if this_year >= today {
        this_year
    } else {
        occurrence_in_year(month, day, today.year() + 1)
    }
}

/// `occurrenceYear - year` for a birthday or anniversary with a known year.
pub fn turning(kind: DateKind, year: Option<i32>, occurrence: NaiveDate) -> Option<i64> {
    match kind {
        DateKind::Birthday | DateKind::Anniversary => year
            .map(|year| i64::from(occurrence.year()) - i64::from(year))
            .filter(|turning| *turning >= 0),
        DateKind::Custom => None,
    }
}

/// The due date of a recurring item's successor. Monthly adds one calendar
/// month clamped to the month's end (Jan 31 → Feb 28 or 29).
pub fn advance(base: NaiveDate, recurrence: Recurrence) -> Option<NaiveDate> {
    advance_anchored(base, recurrence, base.day())
}

/// [`advance`] for a chain whose monthly day is `anchor_day` (the day the chain
/// was first due). Each month lands on the anchor clamped to that month's
/// length, so one short month never drags the chain to an earlier day
/// (Jan 31 → Feb 28 → Mar 31 → Apr 30).
pub fn advance_anchored(
    base: NaiveDate,
    recurrence: Recurrence,
    anchor_day: u32,
) -> Option<NaiveDate> {
    match recurrence {
        Recurrence::None => None,
        Recurrence::Daily => base.checked_add_signed(TimeDelta::days(1)),
        Recurrence::Weekly => base.checked_add_signed(TimeDelta::days(7)),
        Recurrence::Monthly => {
            let first = base.with_day(1)?.checked_add_months(Months::new(1))?;
            let last_day = first.checked_add_months(Months::new(1))?.pred_opt()?.day();
            first.with_day(anchor_day.clamp(1, last_day))
        }
    }
}

// ── Message text ──────────────────────────────────────────────────────────────

fn when(offset: i64) -> String {
    match offset {
        0 => "is today".to_owned(),
        1 => "is tomorrow".to_owned(),
        days => format!("is in {days} days"),
    }
}

fn title_with(kind: DateKind, label: Option<&str>, name: &str, when: &str) -> String {
    match kind {
        DateKind::Birthday => format!("{name}'s birthday {when}"),
        DateKind::Anniversary => {
            format!("{} for {name} {when}", label.unwrap_or("Anniversary"))
        }
        DateKind::Custom => format!("{} for {name} {when}", label.unwrap_or("Important date")),
    }
}

/// Compose one reminder. The honoree's message never mentions gifts; other
/// recipients get at most a count of the gift ideas visible to them. No message
/// carries idea text, claim state, preference values, URLs or private contents.
pub fn compose(
    date: &DateRow,
    person: &PersonRow,
    occurrence: NaiveDate,
    offset: i64,
    recipient: &str,
    visible_gift_ideas: i64,
) -> Notification {
    let when = when(offset);
    let label = date.label.as_deref();
    let mut name = person.display_name.as_str();
    let mut title = title_with(date.kind, label, name, &when);
    while title.len() > MAX_TITLE_BYTES && !name.is_empty() {
        let excess = title.len() - MAX_TITLE_BYTES;
        name = crate::host::truncate_bytes(name, name.len().saturating_sub(excess));
        title = title_with(date.kind, label, name, &when);
    }
    let title = crate::host::truncate_bytes(&title, MAX_TITLE_BYTES).to_owned();

    let mut body = occurrence.format("%A, %B %-d").to_string();
    match (date.kind, turning(date.kind, date.year, occurrence)) {
        (DateKind::Birthday, Some(age)) => body.push_str(&format!(" · turning {age}")),
        (DateKind::Anniversary, Some(years)) => body.push_str(&format!(" · {years} years")),
        _ => {}
    }
    let is_honoree = person.member_user_id.as_deref() == Some(recipient);
    if !is_honoree && visible_gift_ideas > 0 {
        let noun = if visible_gift_ideas == 1 {
            "gift idea"
        } else {
            "gift ideas"
        };
        body.push_str(&format!(" · {visible_gift_ideas} {noun} saved in Family"));
    }
    Notification {
        title,
        body,
        target_user_id: (recipient != authz::LOCAL_USER).then(|| recipient.to_owned()),
    }
}

// ── Sweep ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Delivered (200).
    pub sent: usize,
    /// Rejected by Core (4xx); the claim becomes `failed`.
    pub failed: usize,
    /// Released for a retry (5xx, timeout or network error).
    pub released: usize,
}

/// Run the sweep until the task is aborted. The first tick fires immediately.
pub async fn run(state: AppState) {
    let period = Duration::from_secs(state.config.tick_secs.max(MIN_TICK_SECS));
    let mut ticker = tokio::time::interval(period);
    loop {
        ticker.tick().await;
        match once(&state).await {
            Ok(report) if report != TickReport::default() => {
                tracing::info!(?report, "family: reminder sweep");
            }
            Ok(_) => {}
            // Never propagate: one failing tick must not stop every later reminder.
            Err(error) => tracing::warn!(error = %error, "family: reminder sweep failed"),
        }
    }
}

pub async fn once(state: &AppState) -> FamilyResult<TickReport> {
    once_at(state, Utc::now()).await
}

/// One sweep at an explicit instant (tests drive the clock through this).
pub async fn once_at(state: &AppState, now: DateTime<Utc>) -> FamilyResult<TickReport> {
    let mut report = TickReport::default();
    let Some(host) = state.host.as_ref() else {
        tracing::debug!(
            "family: Core callback unavailable; reminders wait for RYU_CORE_PORT and RYU_EXT_TOKEN"
        );
        return Ok(report);
    };
    let now_ms = now.timestamp_millis();
    for settings in state.store.reminder_tenants().await? {
        let plan = state.store.reminder_plan(&settings.tenant).await?;
        for (date, person) in &plan.dates {
            let tz = resolve_tz(date.tz.as_deref().unwrap_or(&settings.tz));
            let local = now.with_timezone(&tz);
            // `>=` (not `==`) so a DST gap at the reminder hour never skips a day.
            if local.hour() < settings.reminder_hour {
                continue;
            }
            let today = local.date_naive();
            let occurrence = next_occurrence(date.month, date.day, today);
            let recipients =
                authz::reminder_recipients(date, person, &plan.members, &settings.muted_user_ids);
            for &offset in &date.remind_offsets_days {
                if occurrence - TimeDelta::days(offset) != today {
                    continue;
                }
                for recipient in &recipients {
                    let key = ClaimKey {
                        date_id: date.id.clone(),
                        offset_days: offset,
                        occurrence_year: occurrence.year(),
                        recipient: recipient.clone(),
                    };
                    if state.store.claim_status(&key).await?.as_deref() == Some("sent") {
                        continue;
                    }
                    // Everything that can fail runs before the claim, so a
                    // claim is only ever left `pending` by a dying process.
                    let is_honoree = person.member_user_id.as_deref() == Some(recipient.as_str());
                    let gift_ideas = if is_honoree {
                        0
                    } else {
                        state
                            .store
                            .gift_ideas_visible_to(&settings.tenant, person, recipient)
                            .await?
                    };
                    let notification =
                        compose(date, person, occurrence, offset, recipient, gift_ideas);
                    if !state
                        .store
                        .claim_reminder(&settings.tenant, &key, now_ms)
                        .await?
                    {
                        continue;
                    }
                    match host.deliver(&notification).await {
                        DeliverOutcome::Delivered(notification_id) => {
                            state
                                .store
                                .mark_claim_sent(&key, notification_id.as_deref())
                                .await?;
                            report.sent += 1;
                        }
                        DeliverOutcome::Rejected(status) => {
                            tracing::warn!(
                                status,
                                date_id = %key.date_id,
                                "family: Core rejected a reminder; it will not be retried"
                            );
                            state.store.mark_claim_failed(&key).await?;
                            report.failed += 1;
                        }
                        DeliverOutcome::Retry(error) => {
                            tracing::warn!(
                                error = %error,
                                date_id = %key.date_id,
                                "family: reminder delivery failed; retrying on a later tick"
                            );
                            state.store.release_claim(&key).await?;
                            report.released += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
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
        assert_eq!(birthday.title, "Ana's birthday is in 7 days");
        assert_eq!(
            birthday.body,
            "Saturday, March 14 · turning 10 · 2 gift ideas saved in Family"
        );
        assert_eq!(birthday.target_user_id.as_deref(), Some("bo"));

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
        assert_eq!(anniversary.body, "Saturday, March 14 · 20 years");

        let custom = compose(
            &date(DateKind::Custom, Some("Recital"), None),
            &ana,
            occurrence,
            0,
            "local",
            1,
        );
        assert_eq!(custom.title, "Recital for Ana is today");
        assert_eq!(
            custom.body,
            "Saturday, March 14 · 1 gift idea saved in Family"
        );
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
}
