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
    fanout_messages::{Message, ReminderText, Template},
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

/// The title template for a date's kind (and label) and how far off it is.
fn title_template(kind: DateKind, labeled: bool, offset: i64) -> Template {
    use crate::fanout_messages as m;
    let [today, tomorrow, on_date] = match (kind, labeled) {
        (DateKind::Birthday, _) => [
            m::BIRTHDAY_TODAY_MESSAGE,
            m::BIRTHDAY_TOMORROW_MESSAGE,
            m::BIRTHDAY_ON_DATE_MESSAGE,
        ],
        (_, true) => [
            m::LABELED_DATE_TODAY_MESSAGE,
            m::LABELED_DATE_TOMORROW_MESSAGE,
            m::LABELED_DATE_ON_DATE_MESSAGE,
        ],
        (DateKind::Anniversary, false) => [
            m::ANNIVERSARY_TODAY_MESSAGE,
            m::ANNIVERSARY_TOMORROW_MESSAGE,
            m::ANNIVERSARY_ON_DATE_MESSAGE,
        ],
        (DateKind::Custom, false) => [
            m::IMPORTANT_DATE_TODAY_MESSAGE,
            m::IMPORTANT_DATE_TOMORROW_MESSAGE,
            m::IMPORTANT_DATE_ON_DATE_MESSAGE,
        ],
    };
    match offset {
        0 => today,
        1 => tomorrow,
        _ => on_date,
    }
}

fn title_message(date: &DateRow, name: &str, occurrence: NaiveDate, offset: i64) -> Message {
    // A birthday's label is never shown: the sentence names the occasion.
    let label = date
        .label
        .as_deref()
        .filter(|label| date.kind != DateKind::Birthday && !label.trim().is_empty());
    let mut title =
        Message::new(title_template(date.kind, label.is_some(), offset)).text("name", name);
    if let Some(label) = label {
        title = title.text("label", label);
    }
    if offset > 1 {
        title = title.date("date", occurrence);
    }
    title
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
    let mut name = person.display_name.as_str();
    let mut title = title_message(date, name, occurrence, offset);
    while title.english().len() > MAX_TITLE_BYTES && !name.is_empty() {
        let excess = title.english().len() - MAX_TITLE_BYTES;
        name = crate::host::truncate_bytes(name, name.len().saturating_sub(excess));
        title = title_message(date, name, occurrence, offset);
    }

    let mut body = Vec::new();
    match (
        date.kind,
        turning(date.kind, date.year, occurrence),
        date.year,
    ) {
        (DateKind::Birthday, Some(age), _) => {
            body.push(Message::new(crate::fanout_messages::TURNING_AGE_MESSAGE).count("age", age));
        }
        (DateKind::Anniversary, Some(_), Some(year)) => {
            // The first year, not "N years": Core's renderer has no plural.
            body.push(
                Message::new(crate::fanout_messages::SINCE_YEAR_MESSAGE)
                    .text("year", year.to_string()),
            );
        }
        _ => {}
    }
    let is_honoree = person.member_user_id.as_deref() == Some(recipient);
    if !is_honoree && visible_gift_ideas > 0 {
        body.push(
            Message::new(crate::fanout_messages::GIFT_IDEAS_SAVED_MESSAGE)
                .count("count", visible_gift_ideas),
        );
    }
    let text = ReminderText { title, body };
    Notification {
        title: crate::host::truncate_bytes(&text.english_title(), MAX_TITLE_BYTES).to_owned(),
        body: text.english_body(),
        target_user_id: (recipient != authz::LOCAL_USER).then(|| recipient.to_owned()),
        i18n: Some(text.to_wire()),
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
#[path = "tick_test.rs"]
mod tests;
