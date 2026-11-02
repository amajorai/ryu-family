//! Domain validation (422 `validation`). Pure functions; the store normalizes
//! every input through these before it writes.

use chrono::NaiveDate;
use chrono_tz::Tz;
use thiserror::Error;

pub const MAX_DISPLAY_NAME: usize = 120;
pub const MAX_RELATIONSHIP: usize = 60;
pub const MAX_LABEL: usize = 120;
pub const MAX_LIST_NAME: usize = 120;
pub const MAX_ITEM_TEXT: usize = 500;
pub const MAX_QTY: usize = 40;
pub const MAX_PREF_KEY: usize = 80;
pub const MAX_PREF_VALUE: usize = 2_000;
pub const MAX_WISH_TITLE: usize = 200;
pub const MAX_URL: usize = 2_000;
pub const MAX_PRICE_HINT: usize = 40;
pub const MAX_IDEA: usize = 500;
pub const MAX_OCCASION: usize = 120;
pub const MAX_OFFSETS: usize = 5;
pub const MAX_OFFSET_DAYS: i64 = 60;
pub const MAX_POINTS: i64 = 1_000;
pub const MAX_ITEMS_PER_ADD: usize = 50;
pub const MIN_YEAR: i64 = 1900;
pub const MAX_YEAR: i64 = 2100;
pub const MAX_DAYS: i64 = 366;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("{field} is required")]
    Required { field: &'static str },
    #[error("{field} is too long")]
    TooLong { field: &'static str },
    #[error("{field} is out of range")]
    OutOfRange { field: &'static str },
    #[error("{field} must be an exact YYYY-MM-DD date")]
    InvalidDate { field: &'static str },
    #[error("day is not valid for that month")]
    InvalidDay,
    #[error("remindOffsetsDays must be up to 5 unique whole days between 0 and 60")]
    InvalidOffsets,
    #[error("tz must be an IANA time zone name such as Europe/London")]
    InvalidTimeZone,
    #[error("url must start with http:// or https://")]
    InvalidUrl,
    #[error("currency must be three uppercase letters")]
    InvalidCurrency,
    #[error("items must contain between 1 and 50 entries")]
    InvalidItemCount,
    #[error("{field} does not name a person in this family")]
    UnknownPerson { field: &'static str },
    #[error("at least one field is required")]
    EmptyPatch,
}

/// Trim and require 1..=max characters.
pub fn required_text(
    field: &'static str,
    value: &str,
    max_len: usize,
) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::Required { field });
    }
    if trimmed.chars().count() > max_len {
        return Err(ValidationError::TooLong { field });
    }
    Ok(trimmed.to_owned())
}

/// Trim; an empty string becomes `None`; enforce the maximum.
pub fn optional_text(
    field: &'static str,
    value: Option<&str>,
    max_len: usize,
) -> Result<Option<String>, ValidationError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > max_len {
        return Err(ValidationError::TooLong { field });
    }
    Ok(Some(trimmed.to_owned()))
}

pub fn parse_date(field: &'static str, value: &str) -> Result<NaiveDate, ValidationError> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| ValidationError::InvalidDate { field })?;
    if date.format("%Y-%m-%d").to_string() == value {
        Ok(date)
    } else {
        Err(ValidationError::InvalidDate { field })
    }
}

pub fn optional_date(
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<String>, ValidationError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => parse_date(field, value).map(|_| Some(value.to_owned())),
        None => Ok(None),
    }
}

pub fn month_day(month: i64, day: i64) -> Result<(u32, u32), ValidationError> {
    if !(1..=12).contains(&month) {
        return Err(ValidationError::OutOfRange { field: "month" });
    }
    let (Ok(month), Ok(day)) = (u32::try_from(month), u32::try_from(day)) else {
        return Err(ValidationError::InvalidDay);
    };
    // Valid in a leap year, so Feb 29 is allowed (it occurs on Mar 1 otherwise).
    if NaiveDate::from_ymd_opt(2024, month, day).is_none() {
        return Err(ValidationError::InvalidDay);
    }
    Ok((month, day))
}

pub fn year(value: Option<i64>) -> Result<Option<i32>, ValidationError> {
    match value {
        None => Ok(None),
        Some(year) if (MIN_YEAR..=MAX_YEAR).contains(&year) => Ok(i32::try_from(year).ok()),
        Some(_) => Err(ValidationError::OutOfRange { field: "year" }),
    }
}

/// 0..=5 unique integers in 0..=60, stored sorted descending.
pub fn offsets(values: &[i64]) -> Result<Vec<i64>, ValidationError> {
    if values.len() > MAX_OFFSETS {
        return Err(ValidationError::InvalidOffsets);
    }
    let mut sorted = values.to_vec();
    if sorted
        .iter()
        .any(|value| !(0..=MAX_OFFSET_DAYS).contains(value))
    {
        return Err(ValidationError::InvalidOffsets);
    }
    sorted.sort_unstable_by(|left, right| right.cmp(left));
    let before = sorted.len();
    sorted.dedup();
    if sorted.len() != before {
        return Err(ValidationError::InvalidOffsets);
    }
    Ok(sorted)
}

pub fn time_zone(value: &str) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    trimmed
        .parse::<Tz>()
        .map(|tz| tz.name().to_owned())
        .map_err(|_| ValidationError::InvalidTimeZone)
}

pub fn optional_time_zone(value: Option<&str>) -> Result<Option<String>, ValidationError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => time_zone(value).map(Some),
        None => Ok(None),
    }
}

pub fn url(value: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(value) = optional_text("url", value, MAX_URL)? else {
        return Ok(None);
    };
    let lower = value.to_ascii_lowercase();
    let has_host = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty());
    if has_host && !value.chars().any(char::is_whitespace) {
        Ok(Some(value))
    } else {
        Err(ValidationError::InvalidUrl)
    }
}

pub fn currency(value: &str) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    if trimmed.len() == 3 && trimmed.bytes().all(|byte| byte.is_ascii_uppercase()) {
        Ok(trimmed.to_owned())
    } else {
        Err(ValidationError::InvalidCurrency)
    }
}

pub fn optional_currency(value: Option<&str>) -> Result<Option<String>, ValidationError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => currency(value).map(Some),
        None => Ok(None),
    }
}

pub fn points(value: Option<i64>) -> Result<i64, ValidationError> {
    let points = value.unwrap_or(0);
    if (0..=MAX_POINTS).contains(&points) {
        Ok(points)
    } else {
        Err(ValidationError::OutOfRange { field: "points" })
    }
}

pub fn priority(value: Option<i64>) -> Result<i64, ValidationError> {
    let priority = value.unwrap_or(2);
    if (1..=3).contains(&priority) {
        Ok(priority)
    } else {
        Err(ValidationError::OutOfRange { field: "priority" })
    }
}

pub fn budget_minor(value: Option<i64>) -> Result<Option<i64>, ValidationError> {
    match value {
        Some(minor) if minor <= 0 => Err(ValidationError::OutOfRange {
            field: "budgetMinor",
        }),
        other => Ok(other),
    }
}

pub fn reminder_hour(value: i64) -> Result<u32, ValidationError> {
    u32::try_from(value)
        .ok()
        .filter(|hour| *hour <= 23)
        .ok_or(ValidationError::OutOfRange {
            field: "reminderHour",
        })
}

pub fn days(value: Option<i64>) -> Result<i64, ValidationError> {
    let days = value.unwrap_or(30);
    if (1..=MAX_DAYS).contains(&days) {
        Ok(days)
    } else {
        Err(ValidationError::OutOfRange { field: "days" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feb_29_is_a_valid_month_day_but_feb_30_is_not() {
        assert_eq!(month_day(2, 29), Ok((2, 29)));
        assert_eq!(month_day(2, 30), Err(ValidationError::InvalidDay));
        assert_eq!(month_day(4, 31), Err(ValidationError::InvalidDay));
        assert!(month_day(13, 1).is_err());
        assert!(month_day(0, 1).is_err());
    }

    #[test]
    fn offsets_are_unique_bounded_and_sorted_descending() {
        assert_eq!(offsets(&[0, 7, 1]), Ok(vec![7, 1, 0]));
        assert_eq!(offsets(&[]), Ok(vec![]));
        assert!(offsets(&[1, 1]).is_err());
        assert!(offsets(&[61]).is_err());
        assert!(offsets(&[-1]).is_err());
        assert!(offsets(&[0, 1, 2, 3, 4, 5]).is_err());
    }

    #[test]
    fn text_urls_currency_and_zones_are_checked() {
        assert!(required_text("displayName", "  ", 10).is_err());
        assert_eq!(required_text("displayName", " Ana ", 10).unwrap(), "Ana");
        assert!(required_text("displayName", "abcdefghijk", 10).is_err());
        assert_eq!(optional_text("label", Some("  "), 5), Ok(None));
        assert!(url(Some("javascript:alert(1)")).is_err());
        assert!(url(Some("https://")).is_err());
        assert_eq!(
            url(Some("https://example.com/x")).unwrap().as_deref(),
            Some("https://example.com/x")
        );
        assert!(currency("usd").is_err());
        assert_eq!(currency("EUR").unwrap(), "EUR");
        assert!(time_zone("Mars/Base").is_err());
        assert_eq!(time_zone("America/New_York").unwrap(), "America/New_York");
        assert!(parse_date("dueOn", "2026-2-4").is_err());
        assert!(days(Some(0)).is_err());
        assert!(days(Some(367)).is_err());
        assert_eq!(days(None), Ok(30));
        assert!(reminder_hour(24).is_err());
        assert!(points(Some(1_001)).is_err());
        assert!(priority(Some(4)).is_err());
        assert!(budget_minor(Some(0)).is_err());
        assert_eq!(
            year(Some(1899)),
            Err(ValidationError::OutOfRange { field: "year" })
        );
    }
}
