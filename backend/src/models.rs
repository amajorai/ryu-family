//! Row types (what the store reads), wire types (what HTTP and MCP serialize,
//! camelCase, exactly the contract's TypeScript shapes), request inputs, and the
//! one error type every lane maps to its own transport.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::validation::ValidationError;

pub const MAX_LIST_ROWS: usize = 500;
pub const MAX_UPCOMING_ENTRIES: usize = 200;
/// Overdue items kept in the agenda (the most recently due), so a backlog of
/// old items never pushes dates and current items out of it.
pub const MAX_OVERDUE_ENTRIES: usize = 25;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

str_enum!(
    /// Row visibility. `private` rows are visible only to the row's owners.
    Visibility { Family => "family", Private => "private" }
);
str_enum!(DateKind { Birthday => "birthday", Anniversary => "anniversary", Custom => "custom" });
str_enum!(ListKind {
    Grocery => "grocery",
    Chores => "chores",
    Todo => "todo",
    Packing => "packing",
    MealPlan => "meal_plan",
    Custom => "custom",
});
str_enum!(Recurrence { None => "none", Daily => "daily", Weekly => "weekly", Monthly => "monthly" });
str_enum!(GiftStatus { Idea => "idea", Bought => "bought", Given => "given" });

// ── Errors ────────────────────────────────────────────────────────────────────

/// The single error every lane maps: HTTP to a status plus `{error, code}`, MCP
/// to an `isError` tool result with the same `{error, code}`.
#[derive(Debug, Error)]
pub enum FamilyError {
    /// Missing, or invisible to the caller. The two are indistinguishable.
    #[error("not found")]
    NotFound,
    /// Visible to the caller but not something they may do.
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("{0}")]
    BadRequest(String),
    #[error("Turn on Family Planner agent access in the Family app's Settings to connect the family agent.")]
    AgentNotConnected,
    #[error("store unavailable")]
    Unavailable,
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl FamilyError {
    pub fn forbidden() -> Self {
        Self::Forbidden("You can't change this item.".to_owned())
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Forbidden(_) => "forbidden",
            Self::Conflict(_) => "conflict",
            Self::Validation(_) => "validation",
            Self::BadRequest(_) => "bad_request",
            Self::AgentNotConnected => "agent_not_connected",
            Self::Unavailable => "unavailable",
            Self::Internal(_) => "internal",
        }
    }
}

impl From<rusqlite::Error> for FamilyError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(failure, _) = &error {
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return Self::Unavailable;
            }
        }
        Self::Internal(error.into())
    }
}

impl From<serde_json::Error> for FamilyError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.into())
    }
}

pub type FamilyResult<T> = Result<T, FamilyError>;

// ── Rows ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonRow {
    pub id: String,
    pub tenant: String,
    pub display_name: String,
    pub relationship: Option<String>,
    pub member_user_id: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateRow {
    pub id: String,
    pub tenant: String,
    pub person_id: String,
    pub kind: DateKind,
    pub label: Option<String>,
    pub month: u32,
    pub day: u32,
    pub year: Option<i32>,
    pub remind_offsets_days: Vec<i64>,
    pub tz: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    pub id: String,
    pub tenant: String,
    pub kind: ListKind,
    pub name: String,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRow {
    pub id: String,
    pub tenant: String,
    pub list_id: String,
    pub text: String,
    pub qty: Option<String>,
    pub done: bool,
    pub done_at: Option<i64>,
    pub done_by: Option<String>,
    pub assignee_person_id: Option<String>,
    pub due_on: Option<String>,
    pub recurrence: Recurrence,
    pub points: i64,
    pub next_item_id: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefRow {
    pub id: String,
    pub tenant: String,
    pub person_id: String,
    pub key: String,
    pub value: String,
    pub sensitive: bool,
    /// The member the person was linked to when the row was written.
    pub subject_user_id: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WishRow {
    pub id: String,
    pub tenant: String,
    pub person_id: String,
    pub title: String,
    pub url: Option<String>,
    pub price_hint: Option<String>,
    pub priority: i64,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<i64>,
    /// The member the person was linked to when the item was written.
    pub subject_user_id: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiftRow {
    pub id: String,
    pub tenant: String,
    pub person_id: String,
    pub idea: String,
    pub url: Option<String>,
    pub budget_minor: Option<i64>,
    pub budget_currency: Option<String>,
    pub status: GiftStatus,
    pub occasion: Option<String>,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRow {
    pub tenant: String,
    pub tz: String,
    pub reminder_hour: u32,
    pub default_offsets_days: Vec<i64>,
    pub currency: String,
    pub agent_access: bool,
    pub muted_user_ids: Vec<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RowBase {
    pub id: String,
    pub visibility: Visibility,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Also governs delete in the UI.
    pub can_edit: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Person {
    #[serde(flatten)]
    pub base: RowBase,
    pub display_name: String,
    pub relationship: Option<String>,
    pub member_linked: bool,
    pub is_me: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FamilyDate {
    #[serde(flatten)]
    pub base: RowBase,
    pub person_id: String,
    pub kind: DateKind,
    pub label: Option<String>,
    pub month: u32,
    pub day: u32,
    pub year: Option<i32>,
    pub remind_offsets_days: Vec<i64>,
    pub tz: Option<String>,
    pub next_occurrence: String,
    pub days_away: i64,
    pub turning: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FamilyList {
    #[serde(flatten)]
    pub base: RowBase,
    pub kind: ListKind,
    pub name: String,
    pub open_count: i64,
    pub done_count: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListItem {
    #[serde(flatten)]
    pub base: RowBase,
    pub list_id: String,
    pub text: String,
    pub qty: Option<String>,
    pub done: bool,
    pub done_at: Option<i64>,
    pub done_by: Option<String>,
    pub assignee_person_id: Option<String>,
    pub due_on: Option<String>,
    pub recurrence: Recurrence,
    pub points: i64,
    pub next_item_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TallyRow {
    pub assignee_person_id: Option<String>,
    pub person_name: Option<String>,
    pub points: i64,
    pub completed: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Preference {
    #[serde(flatten)]
    pub base: RowBase,
    pub person_id: String,
    pub key: String,
    pub value: String,
    pub sensitive: bool,
    pub can_change_visibility: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaimView {
    pub claimed: bool,
    pub by_me: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WishlistItem {
    #[serde(flatten)]
    pub base: RowBase,
    pub person_id: String,
    pub title: String,
    pub url: Option<String>,
    pub price_hint: Option<String>,
    pub priority: i64,
    pub can_claim: bool,
    /// ABSENT (not null) for the wishlist's own person and for the MCP lane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftIdea {
    #[serde(flatten)]
    pub base: RowBase,
    pub person_id: String,
    pub idea: String,
    pub url: Option<String>,
    pub budget_minor: Option<i64>,
    pub currency: Option<String>,
    pub status: GiftStatus,
    pub occasion: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DateEntry {
    pub on: String,
    pub days_away: i64,
    pub date_id: String,
    pub person_id: String,
    pub person_name: String,
    pub kind: DateKind,
    pub label: Option<String>,
    pub turning: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemEntry {
    pub on: String,
    pub days_away: i64,
    pub item_id: String,
    pub list_id: String,
    pub list_name: String,
    pub text: String,
    pub assignee_person_id: Option<String>,
    pub overdue: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UpcomingEntry {
    Date(DateEntry),
    Item(ItemEntry),
}

impl UpcomingEntry {
    pub fn on(&self) -> &str {
        match self {
            Self::Date(entry) => &entry.on,
            Self::Item(entry) => &entry.on,
        }
    }

    /// Dates sort before items on the same day.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Date(_) => 0,
            Self::Item(_) => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpcomingResponse {
    pub today: String,
    pub tz: String,
    pub days: i64,
    pub entries: Vec<UpcomingEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftContextPerson {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftBudget {
    pub minor: Option<i64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftContextDate {
    pub kind: DateKind,
    pub label: Option<String>,
    pub on: String,
    pub turning: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftContextWish {
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    pub price_hint: Option<String>,
    pub priority: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PastGift {
    pub idea: String,
    pub occasion: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GiftContext {
    pub person: GiftContextPerson,
    pub occasion: Option<String>,
    pub budget: GiftBudget,
    pub next_dates: Vec<GiftContextDate>,
    pub preferences: Vec<KeyValue>,
    pub wishlist: Vec<GiftContextWish>,
    pub past_gifts: Vec<PastGift>,
    pub restricted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub tz: String,
    pub reminder_hour: u32,
    pub default_remind_offsets_days: Vec<i64>,
    pub currency: String,
    pub agent_access: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub user_id: String,
    pub person_id: Option<String>,
    pub muted: bool,
    pub tenant_kind: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SettingsResponse {
    pub settings: Settings,
    pub me: Me,
}

// ── Inputs ────────────────────────────────────────────────────────────────────
//
// Required text fields default to "" so a missing field is a 422 validation
// error naming the field, not an opaque 400. Unknown fields (including any
// `tenant` or `userId`) are ignored: identity only ever comes from the lane.

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewPerson {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub relationship: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonPatch {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub relationship: Option<Option<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDate {
    #[serde(default)]
    pub person_id: String,
    pub kind: DateKind,
    #[serde(default)]
    pub label: Option<String>,
    pub month: i64,
    pub day: i64,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub remind_offsets_days: Option<Vec<i64>>,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatePatch {
    #[serde(default)]
    pub kind: Option<DateKind>,
    #[serde(default, deserialize_with = "nullable")]
    pub label: Option<Option<String>>,
    #[serde(default)]
    pub month: Option<i64>,
    #[serde(default)]
    pub day: Option<i64>,
    #[serde(default, deserialize_with = "nullable")]
    pub year: Option<Option<i64>>,
    #[serde(default)]
    pub remind_offsets_days: Option<Vec<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    pub tz: Option<Option<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewList {
    #[serde(default)]
    pub name: String,
    pub kind: ListKind,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<ListKind>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewListItem {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub qty: Option<String>,
    #[serde(default)]
    pub assignee_person_id: Option<String>,
    #[serde(default)]
    pub due_on: Option<String>,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub points: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddItems {
    #[serde(default)]
    pub items: Vec<NewListItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemPatch {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub qty: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub assignee_person_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub due_on: Option<Option<String>>,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub points: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteInput {
    pub done: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewPreference {
    #[serde(default)]
    pub person_id: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub sensitive: Option<bool>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreferencePatch {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub sensitive: Option<bool>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewWish {
    #[serde(default)]
    pub person_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub price_hint: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WishPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub price_hint: Option<Option<String>>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewGift {
    #[serde(default)]
    pub person_id: String,
    #[serde(default)]
    pub idea: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub budget_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub status: Option<GiftStatus>,
    #[serde(default)]
    pub occasion: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftPatch {
    #[serde(default)]
    pub idea: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub budget_minor: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    pub currency: Option<Option<String>>,
    #[serde(default)]
    pub status: Option<GiftStatus>,
    #[serde(default, deserialize_with = "nullable")]
    pub occasion: Option<Option<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftContextQuery {
    #[serde(default)]
    pub person_id: Option<String>,
    #[serde(default)]
    pub occasion: Option<String>,
    #[serde(default)]
    pub budget_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub reminder_hour: Option<i64>,
    #[serde(default)]
    pub default_remind_offsets_days: Option<Vec<i64>>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub agent_access: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeInput {
    pub muted: bool,
}

/// PATCH semantics: an omitted field deserializes to `None` (keep), an explicit
/// JSON `null` to `Some(None)` (clear), a value to `Some(Some(value))`.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
