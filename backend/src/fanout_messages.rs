//! Reminder copy as localizable messages.
//!
//! Core renders a `notify.deliver` call's `i18n` text per recipient (the inbox
//! row and toast in the node's `ui-language`, each push in the locale its device
//! registered with), so a reminder carries its text as messages: an English ICU
//! source plus named arguments ([`ReminderText::to_wire`]). The English
//! rendering still goes out as the plain `title`/`body`, which an older Core
//! delivers as-is.
//!
//! Each `*_MESSAGE` is `format!` with one named argument per placeholder
//! (`name = arg("name")`) so the i18n extractor catalogs the ICU source;
//! `tools/generate-core-i18n.mjs` copies the translations into Core's catalog.
//! Every message is a whole sentence, one per kind and timing; never assemble
//! one from fragments. Counts are written as `label: {count}` because Core's
//! renderer has no plural selector. The occurrence travels as a date argument
//! that Core writes in the reader's language.
#![allow(clippy::useless_format)]

use chrono::NaiveDate;
use serde_json::{json, Map, Value};

/// An English template. With a placeholder echo it returns its ICU source.
pub(crate) type Template = fn(&dyn Fn(&'static str) -> String) -> String;

// ---- titles: birthdays ------------------------------------------------------

pub(crate) const BIRTHDAY_TODAY_MESSAGE: Template =
    |arg| format!("{name}'s birthday is today", name = arg("name"));
pub(crate) const BIRTHDAY_TOMORROW_MESSAGE: Template =
    |arg| format!("{name}'s birthday is tomorrow", name = arg("name"));
pub(crate) const BIRTHDAY_ON_DATE_MESSAGE: Template = |arg| {
    format!(
        "{name}'s birthday is on {date}",
        name = arg("name"),
        date = arg("date")
    )
};

// ---- titles: anniversaries without a label ----------------------------------

pub(crate) const ANNIVERSARY_TODAY_MESSAGE: Template =
    |arg| format!("Anniversary for {name} is today", name = arg("name"));
pub(crate) const ANNIVERSARY_TOMORROW_MESSAGE: Template =
    |arg| format!("Anniversary for {name} is tomorrow", name = arg("name"));
pub(crate) const ANNIVERSARY_ON_DATE_MESSAGE: Template = |arg| {
    format!(
        "Anniversary for {name} is on {date}",
        name = arg("name"),
        date = arg("date")
    )
};

// ---- titles: other dates without a label ------------------------------------

pub(crate) const IMPORTANT_DATE_TODAY_MESSAGE: Template =
    |arg| format!("Important date for {name} is today", name = arg("name"));
pub(crate) const IMPORTANT_DATE_TOMORROW_MESSAGE: Template =
    |arg| format!("Important date for {name} is tomorrow", name = arg("name"));
pub(crate) const IMPORTANT_DATE_ON_DATE_MESSAGE: Template = |arg| {
    format!(
        "Important date for {name} is on {date}",
        name = arg("name"),
        date = arg("date")
    )
};

// ---- titles: a date the family labeled ("Recital") --------------------------

pub(crate) const LABELED_DATE_TODAY_MESSAGE: Template = |arg| {
    format!(
        "It's {label} for {name} today",
        label = arg("label"),
        name = arg("name")
    )
};
pub(crate) const LABELED_DATE_TOMORROW_MESSAGE: Template = |arg| {
    format!(
        "It's {label} for {name} tomorrow",
        label = arg("label"),
        name = arg("name")
    )
};
pub(crate) const LABELED_DATE_ON_DATE_MESSAGE: Template = |arg| {
    format!(
        "It's {label} for {name} on {date}",
        label = arg("label"),
        name = arg("name"),
        date = arg("date")
    )
};

// ---- body sentences ---------------------------------------------------------

pub(crate) const TURNING_AGE_MESSAGE: Template =
    |arg| format!("They turn {age}.", age = arg("age"));
pub(crate) const SINCE_YEAR_MESSAGE: Template =
    |arg| format!("First celebrated in {year}.", year = arg("year"));
pub(crate) const GIFT_IDEAS_SAVED_MESSAGE: Template =
    |arg| format!("Gift ideas saved in Family: {count}.", count = arg("count"));

/// One argument: verbatim text, a whole number Core groups per locale, or a
/// calendar day Core writes with the locale's weekday and month names.
#[derive(Debug, Clone, PartialEq)]
enum Arg {
    Text(String),
    Count(i64),
    Date(NaiveDate),
}

/// An English ICU source plus its arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    source: String,
    args: Vec<(&'static str, Arg)>,
}

impl Message {
    pub(crate) fn new(template: Template) -> Self {
        Self {
            source: template(&|name| format!("{{{name}}}")),
            args: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn text(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.args.push((name, Arg::Text(value.into())));
        self
    }

    pub(crate) fn count(mut self, name: &'static str, value: i64) -> Self {
        self.args.push((name, Arg::Count(value)));
        self
    }

    pub(crate) fn date(mut self, name: &'static str, value: NaiveDate) -> Self {
        self.args.push((name, Arg::Date(value)));
        self
    }

    /// The English sentence (the plain `title`/`body` sent alongside).
    pub(crate) fn english(&self) -> String {
        // One pass, so an argument's own text is never substituted again.
        let mut out = String::with_capacity(self.source.len() + 16);
        let mut rest = self.source.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                rest = &rest[open..];
                break;
            };
            let name = &after[..close];
            match self.args.iter().find(|(key, _)| *key == name) {
                Some((_, Arg::Text(text))) => out.push_str(text),
                Some((_, Arg::Count(value))) => out.push_str(&english_count(*value)),
                Some((_, Arg::Date(day))) => out.push_str(&day.format("%A, %B %-d").to_string()),
                None => out.push_str(&rest[open..open + close + 2]),
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        out
    }

    fn to_wire(&self) -> Value {
        let args: Map<String, Value> = self
            .args
            .iter()
            .map(|(name, arg)| {
                let value = match arg {
                    Arg::Text(text) => Value::String(text.clone()),
                    Arg::Count(value) => json!(value),
                    Arg::Date(day) => json!({ "date": day.format("%Y-%m-%d").to_string() }),
                };
                ((*name).to_owned(), value)
            })
            .collect();
        json!({ "message": self.source, "args": args })
    }
}

/// A reminder's title plus body sentences.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReminderText {
    pub(crate) title: Message,
    pub(crate) body: Vec<Message>,
}

impl ReminderText {
    pub(crate) fn english_title(&self) -> String {
        self.title.english()
    }

    pub(crate) fn english_body(&self) -> String {
        let sentences: Vec<String> = self.body.iter().map(Message::english).collect();
        sentences.join(" ")
    }

    /// The `i18n` object `notify.deliver` renders per locale.
    pub(crate) fn to_wire(&self) -> Value {
        json!({
            "title": self.title.to_wire(),
            "body": self.body.iter().map(Message::to_wire).collect::<Vec<_>>(),
            "join": "sentences",
        })
    }
}

/// English digits with `,` grouping, as `Intl.NumberFormat("en")` prints them.
fn english_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_icu_messages_and_english_fills_them() {
        let day = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
        let title = Message::new(BIRTHDAY_ON_DATE_MESSAGE)
            .text("name", "Ana {date}")
            .date("date", day);
        assert_eq!(title.source(), "{name}'s birthday is on {date}");
        // An argument's own braces are never substituted again.
        assert_eq!(
            title.english(),
            "Ana {date}'s birthday is on Saturday, March 14"
        );
        assert_eq!(
            title.to_wire(),
            json!({
                "message": "{name}'s birthday is on {date}",
                "args": { "name": "Ana {date}", "date": { "date": "2026-03-14" } },
            })
        );
        let gifts = Message::new(GIFT_IDEAS_SAVED_MESSAGE).count("count", 1200);
        assert_eq!(gifts.english(), "Gift ideas saved in Family: 1,200.");
        assert_eq!(gifts.to_wire()["args"]["count"], json!(1200));
    }
}
