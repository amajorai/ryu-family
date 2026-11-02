//! The one place visibility and edit decisions are made.
//!
//! Every function here is pure: it looks at row metadata, the row's person and
//! the caller, and nothing else. The HTTP handlers, the MCP tools and the
//! reminder sweep reach rows only through store methods that call these
//! functions, and the serialized `canEdit` / `canClaim` / `canChangeVisibility`
//! flags come from the same functions, so no lane can drift from another.
//!
//! Lanes:
//! - [`Principal::Member`]: the verified `x-ryu-caller-user-id` on the HTTP hop
//!   (or `"local"` on an unmanaged single-user node).
//! - [`Principal::Shared`]: the MCP lane. Core forwards no caller identity into a
//!   stdio `tools/call`, so this caller may be anyone, including the person a
//!   surprise is for. It gets the most conservative view.
//! - [`Principal::System`]: the reminder sweep. It reads to compute recipients
//!   and writes only reminder claims; per-recipient views use a `Member` caller.

use std::collections::BTreeSet;

use crate::models::{
    ClaimView, DateKind, DateRow, FamilyError, GiftRow, ListRow, PersonRow, PrefRow, Visibility,
    WishRow,
};

pub const AGENT_ACTOR: &str = "agent";
pub const SYSTEM_ACTOR: &str = "system";
pub const LOCAL_USER: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A verified user id, or `"local"`.
    Member(String),
    /// The MCP lane: unverified, possibly anyone.
    Shared,
    /// The reminder sweep.
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub tenant: String,
    pub principal: Principal,
}

impl Caller {
    pub fn member(tenant: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            principal: Principal::Member(user.into()),
        }
    }

    pub fn shared(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            principal: Principal::Shared,
        }
    }

    pub fn system(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            principal: Principal::System,
        }
    }

    /// The principal string written to `created_by`, `done_by` and `audit.actor`.
    pub fn actor(&self) -> &str {
        match &self.principal {
            Principal::Member(user) => user,
            Principal::Shared => AGENT_ACTOR,
            Principal::System => SYSTEM_ACTOR,
        }
    }

    /// The verified member id, if this caller is a member.
    pub fn user_id(&self) -> Option<&str> {
        match &self.principal {
            Principal::Member(user) => Some(user),
            _ => None,
        }
    }

    pub fn is_shared(&self) -> bool {
        self.principal == Principal::Shared
    }

    pub fn is_system(&self) -> bool {
        self.principal == Principal::System
    }
}

/// `"org" | "user" | "local"` for `Me.tenantKind`.
pub fn tenant_kind(tenant: &str) -> &'static str {
    if tenant.starts_with("org:") {
        "org"
    } else if tenant.starts_with("user:") {
        "user"
    } else {
        "local"
    }
}

/// A principal that names a person (a user id or `"local"`), not a lane.
pub fn is_user_principal(actor: &str) -> bool {
    !actor.is_empty() && actor != AGENT_ACTOR && actor != SYSTEM_ACTOR
}

/// Is the caller the person this row is about? A `Shared` caller is treated as
/// possibly anyone, so it always counts as the recipient. An unlinked person has
/// no recipient among members.
pub fn is_recipient(caller: &Caller, person: &PersonRow) -> bool {
    match &caller.principal {
        Principal::Shared => true,
        Principal::Member(user) => person.member_user_id.as_deref() == Some(user.as_str()),
        Principal::System => false,
    }
}

/// A row is effectively private if it is private or its person is private.
pub fn effectively_private(row: Visibility, person: &PersonRow) -> bool {
    row == Visibility::Private || person.visibility == Visibility::Private
}

/// The visibility a new row gets. The shared lane always writes family rows.
pub fn new_row_visibility(caller: &Caller, requested: Option<Visibility>) -> Visibility {
    if caller.is_shared() {
        Visibility::Family
    } else {
        requested.unwrap_or(Visibility::Family)
    }
}

/// The visibility after a patch. Only the row's creator may make a family row
/// private (a private row is visible to its creator, so anyone else would hide
/// it from themselves and the family). The shared lane never makes a row
/// private. Re-sending the current visibility is always fine.
pub fn patched_visibility(
    caller: &Caller,
    current: Visibility,
    created_by: &str,
    requested: Option<Visibility>,
) -> Result<Visibility, FamilyError> {
    match requested {
        Some(Visibility::Private) if current != Visibility::Private => {
            if caller.user_id() == Some(created_by) {
                Ok(Visibility::Private)
            } else {
                Err(FamilyError::Forbidden(
                    "Only the person who added this can make it private.".to_owned(),
                ))
            }
        }
        Some(visibility) => Ok(visibility),
        None => Ok(current),
    }
}

// ── people ────────────────────────────────────────────────────────────────────

pub fn person_visible(caller: &Caller, person: &PersonRow) -> bool {
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => person.visibility == Visibility::Family,
        Principal::Member(user) => {
            person.visibility == Visibility::Family || person.created_by == *user
        }
    }
}

pub fn person_editable(caller: &Caller, person: &PersonRow) -> bool {
    !caller.is_system() && person_visible(caller, person)
}

/// Delete cascades to every member's dates, preferences, wishlist, claims and
/// gift ideas for the person, including surprises the linked member may not
/// see. So the linked member never deletes their own person (one fixed 403,
/// whatever exists), and the shared lane never deletes. Otherwise the creator
/// deletes; a person whose creator is the agent or the linked member themselves
/// may be deleted by any other member who sees it, so it never becomes stuck.
pub fn person_deletable(caller: &Caller, person: &PersonRow) -> bool {
    let Some(user) = caller.user_id() else {
        return false;
    };
    if !person_visible(caller, person) || is_recipient(caller, person) {
        return false;
    }
    person.created_by == user
        || !is_user_principal(&person.created_by)
        || person.member_user_id.as_deref() == Some(person.created_by.as_str())
}

/// "This is me": a member may link an unlinked, visible, family person.
/// The one-person-per-member rule is enforced by the store's unique index.
pub fn person_claimable(caller: &Caller, person: &PersonRow) -> Result<(), FamilyError> {
    if caller.user_id().is_none() {
        return Err(FamilyError::forbidden());
    }
    if !person_visible(caller, person) {
        return Err(FamilyError::NotFound);
    }
    if person.visibility != Visibility::Family || person.member_user_id.is_some() {
        return Err(FamilyError::Conflict(
            "This person can't be linked to your account.".to_owned(),
        ));
    }
    Ok(())
}

// ── dates ─────────────────────────────────────────────────────────────────────

pub fn date_visible(caller: &Caller, date: &DateRow, person: &PersonRow) -> bool {
    let private = effectively_private(date.visibility, person);
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => !private,
        Principal::Member(user) => !private || date.created_by == *user,
    }
}

pub fn date_editable(caller: &Caller, date: &DateRow, person: &PersonRow) -> bool {
    !caller.is_system() && date_visible(caller, date, person)
}

// ── lists and items (an item's visibility is its list's) ─────────────────────

pub fn list_visible(caller: &Caller, list: &ListRow) -> bool {
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => list.visibility == Visibility::Family,
        Principal::Member(user) => {
            list.visibility == Visibility::Family || list.created_by == *user
        }
    }
}

pub fn list_editable(caller: &Caller, list: &ListRow) -> bool {
    !caller.is_system() && list_visible(caller, list)
}

/// The shared lane may change items but never delete a whole list.
pub fn list_deletable(caller: &Caller, list: &ListRow) -> bool {
    caller.user_id().is_some() && list_visible(caller, list)
}

// ── preferences and wishlist subjects ─────────────────────────────────────────

/// Is `user` the row's subject: the member the person was linked to when the
/// row was written, and still is? A row written before the person was linked
/// ("This is me" is unverified) has no subject, so linking never hands the
/// claimant rows other members wrote earlier.
pub fn is_row_subject(user: &str, subject: Option<&str>, person: &PersonRow) -> bool {
    subject == Some(user) && person.member_user_id.as_deref() == Some(user)
}

// ── preferences ───────────────────────────────────────────────────────────────

fn pref_owner(caller: &Caller, pref: &PrefRow, person: &PersonRow) -> bool {
    caller.user_id().is_some_and(|user| {
        pref.created_by == user || is_row_subject(user, pref.subject_user_id.as_deref(), person)
    })
}

/// The shared lane may be anyone (a group chat included), so it never sees a
/// sensitive row, even one its person shared with the family in the app.
pub fn pref_visible(caller: &Caller, pref: &PrefRow, person: &PersonRow) -> bool {
    let private = effectively_private(pref.visibility, person);
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => !private && !pref.sensitive,
        Principal::Member(_) => !private || pref_owner(caller, pref, person),
    }
}

/// The shared lane never edits a sensitive row, even a family-visible one.
pub fn pref_editable(caller: &Caller, pref: &PrefRow, person: &PersonRow) -> bool {
    if caller.is_system() || !pref_visible(caller, pref, person) {
        return false;
    }
    !(caller.is_shared() && pref.sensitive)
}

/// Who controls the sharing of a sensitive row: its subject (the person, when
/// they were linked as the row was written), otherwise the row's creator (a
/// guardian proxy). A later "This is me" never takes control of an older row.
pub fn sensitive_controller(
    caller: &Caller,
    person: &PersonRow,
    creator: &str,
    subject: Option<&str>,
) -> bool {
    let Some(user) = caller.user_id() else {
        return false;
    };
    match subject {
        Some(_) => is_row_subject(user, subject, person),
        None => creator == user,
    }
}

pub fn pref_can_change_visibility(caller: &Caller, pref: &PrefRow, person: &PersonRow) -> bool {
    if caller.is_shared() || !pref_editable(caller, pref, person) {
        return false;
    }
    !pref.sensitive
        || sensitive_controller(
            caller,
            person,
            &pref.created_by,
            pref.subject_user_id.as_deref(),
        )
}

/// Visibility for a new preference. Sensitive rows default to private; only the
/// controller may create one family-visible. The shared lane writes only
/// non-sensitive family rows.
pub fn new_pref_visibility(
    caller: &Caller,
    person: &PersonRow,
    sensitive: bool,
    requested: Option<Visibility>,
) -> Result<Visibility, FamilyError> {
    if caller.is_shared() {
        return if sensitive {
            Err(FamilyError::forbidden())
        } else {
            Ok(Visibility::Family)
        };
    }
    if !sensitive {
        return Ok(requested.unwrap_or(Visibility::Family));
    }
    match requested {
        None | Some(Visibility::Private) => Ok(Visibility::Private),
        Some(Visibility::Family)
            if sensitive_controller(
                caller,
                person,
                caller.actor(),
                person.member_user_id.as_deref(),
            ) =>
        {
            Ok(Visibility::Family)
        }
        Some(Visibility::Family) => Err(FamilyError::Forbidden(
            "Only the person can share a sensitive preference with the family.".to_owned(),
        )),
    }
}

/// The `(sensitive, visibility)` pair after a patch, or 403.
pub fn patched_pref_flags(
    caller: &Caller,
    pref: &PrefRow,
    person: &PersonRow,
    sensitive: Option<bool>,
    requested: Option<Visibility>,
) -> Result<(bool, Visibility), FamilyError> {
    let next_sensitive = sensitive.unwrap_or(pref.sensitive);
    if caller.is_shared() {
        if pref.sensitive || next_sensitive || requested == Some(Visibility::Private) {
            return Err(FamilyError::forbidden());
        }
        return Ok((false, Visibility::Family));
    }
    let controller = sensitive_controller(
        caller,
        person,
        &pref.created_by,
        pref.subject_user_id.as_deref(),
    );
    let changes_sensitive = next_sensitive != pref.sensitive;
    let changes_visibility = requested.is_some_and(|visibility| visibility != pref.visibility);
    if pref.sensitive && (changes_sensitive || changes_visibility) && !controller {
        return Err(FamilyError::Forbidden(
            "Only the person can change who sees a sensitive preference.".to_owned(),
        ));
    }
    if !pref.sensitive && next_sensitive {
        // Marking a row sensitive makes it private unless the controller says otherwise.
        return match requested {
            None | Some(Visibility::Private) => Ok((true, Visibility::Private)),
            Some(Visibility::Family) if controller => Ok((true, Visibility::Family)),
            Some(Visibility::Family) => Err(FamilyError::Forbidden(
                "Only the person can share a sensitive preference with the family.".to_owned(),
            )),
        };
    }
    Ok((next_sensitive, requested.unwrap_or(pref.visibility)))
}

// ── wishlist items ────────────────────────────────────────────────────────────

/// Owners of a wishlist item: its creator and the wishlist's own person (when
/// they were linked as the item was written; see [`is_row_subject`]).
pub fn wish_owner(caller: &Caller, wish: &WishRow, person: &PersonRow) -> bool {
    caller.user_id().is_some_and(|user| {
        wish.created_by == user || is_row_subject(user, wish.subject_user_id.as_deref(), person)
    })
}

pub fn wish_visible(caller: &Caller, wish: &WishRow, person: &PersonRow) -> bool {
    let private = effectively_private(wish.visibility, person);
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => !private,
        Principal::Member(_) => !private || wish_owner(caller, wish, person),
    }
}

/// Only owners edit or delete; everyone else may only claim.
pub fn wish_editable(caller: &Caller, wish: &WishRow, person: &PersonRow) -> bool {
    wish_visible(caller, wish, person) && wish_owner(caller, wish, person)
}

pub fn wish_can_claim(caller: &Caller, wish: &WishRow, person: &PersonRow) -> bool {
    let Some(user) = caller.user_id() else {
        return false;
    };
    wish_visible(caller, wish, person)
        && !effectively_private(wish.visibility, person)
        && !is_recipient(caller, person)
        && wish
            .claimed_by
            .as_deref()
            .is_none_or(|claimer| claimer == user)
}

/// The claim state a caller may see. `None` (the field is omitted) for the
/// wishlist's own person and for the shared lane; never the claimer's id.
pub fn wish_claim_view(caller: &Caller, wish: &WishRow, person: &PersonRow) -> Option<ClaimView> {
    let user = caller.user_id()?;
    if !wish_visible(caller, wish, person) || is_recipient(caller, person) {
        return None;
    }
    Some(ClaimView {
        claimed: wish.claimed_by.is_some(),
        by_me: wish.claimed_by.as_deref() == Some(user),
    })
}

/// The fixed answer to a recipient who tries to claim or unclaim: it is the same
/// whatever the claim state, so it reveals nothing.
pub fn recipient_claim_error() -> FamilyError {
    FamilyError::Forbidden("You can't claim items on your own wishlist.".to_owned())
}

// ── gift ideas ────────────────────────────────────────────────────────────────

/// Gift ideas are invisible to the person they are for on every path, and the
/// shared lane never sees any.
pub fn gift_visible(caller: &Caller, gift: &GiftRow, person: &PersonRow) -> bool {
    match &caller.principal {
        Principal::System => true,
        Principal::Shared => false,
        Principal::Member(user) => {
            !is_recipient(caller, person)
                && (!effectively_private(gift.visibility, person) || gift.created_by == *user)
        }
    }
}

pub fn gift_editable(caller: &Caller, gift: &GiftRow, person: &PersonRow) -> bool {
    !caller.is_system() && gift_visible(caller, gift, person)
}

/// Creating an idea: members only, never for yourself. The person must already
/// be visible to the caller (checked by the store first, giving 404).
pub fn gift_can_create(caller: &Caller, person: &PersonRow) -> Result<(), FamilyError> {
    if caller.user_id().is_none() || is_recipient(caller, person) {
        return Err(FamilyError::forbidden());
    }
    Ok(())
}

/// Whether `caller` may read gift ideas through gift context. Only a member who
/// is not the recipient; everyone else gets the restricted view.
pub fn gift_context_restricted(caller: &Caller, person: &PersonRow) -> bool {
    caller.user_id().is_none() || is_recipient(caller, person)
}

// ── reminders ─────────────────────────────────────────────────────────────────

/// Who receives a reminder for `date`.
///
/// - An effectively private date reminds only its creator, and only when the
///   creator is a user principal (not the agent or the sweep).
/// - A family date reminds every member, except that a birthday never reminds
///   its own honoree. An anniversary or custom date does remind its honoree
///   (the message then carries no gift hints).
/// - Muted members are excluded either way: muting means "no reminders for me".
pub fn reminder_recipients(
    date: &DateRow,
    person: &PersonRow,
    members: &BTreeSet<String>,
    muted: &[String],
) -> Vec<String> {
    let honoree = person.member_user_id.as_deref();
    let mut recipients: Vec<String> = if effectively_private(date.visibility, person) {
        if is_user_principal(&date.created_by) {
            vec![date.created_by.clone()]
        } else {
            Vec::new()
        }
    } else {
        members
            .iter()
            .filter(|member| !(date.kind == DateKind::Birthday && Some(member.as_str()) == honoree))
            .cloned()
            .collect()
    };
    recipients.retain(|recipient| !muted.iter().any(|muted| muted == recipient));
    recipients
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{GiftStatus, Visibility::*};

    fn person(member: Option<&str>, visibility: Visibility) -> PersonRow {
        PersonRow {
            id: "p".into(),
            tenant: "org:o".into(),
            display_name: "Ana".into(),
            relationship: None,
            member_user_id: member.map(Into::into),
            visibility,
            created_by: "creator".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn date(kind: DateKind, visibility: Visibility, created_by: &str) -> DateRow {
        DateRow {
            id: "d".into(),
            tenant: "org:o".into(),
            person_id: "p".into(),
            kind,
            label: None,
            month: 3,
            day: 14,
            year: None,
            remind_offsets_days: vec![7, 1, 0],
            tz: None,
            visibility,
            created_by: created_by.into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn gift(created_by: &str, visibility: Visibility) -> GiftRow {
        GiftRow {
            id: "g".into(),
            tenant: "org:o".into(),
            person_id: "p".into(),
            idea: "Kite".into(),
            url: None,
            budget_minor: None,
            budget_currency: None,
            status: GiftStatus::Idea,
            occasion: None,
            visibility,
            created_by: created_by.into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn wish(claimed_by: Option<&str>) -> WishRow {
        WishRow {
            id: "w".into(),
            tenant: "org:o".into(),
            person_id: "p".into(),
            title: "Book".into(),
            url: None,
            price_hint: None,
            priority: 2,
            claimed_by: claimed_by.map(Into::into),
            claimed_at: None,
            subject_user_id: Some("ana".into()),
            visibility: Family,
            created_by: "ana".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn members(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn gift_ideas_are_hidden_from_the_recipient_and_the_shared_lane() {
        let ana = person(Some("ana"), Family);
        let idea = gift("bo", Family);
        assert!(!gift_visible(&Caller::member("org:o", "ana"), &idea, &ana));
        assert!(!gift_visible(&Caller::shared("org:o"), &idea, &ana));
        assert!(gift_visible(&Caller::member("org:o", "cy"), &idea, &ana));
        // Even the recipient's own idea for themselves cannot exist or be seen.
        assert!(gift_can_create(&Caller::member("org:o", "ana"), &ana).is_err());
        assert!(gift_can_create(&Caller::shared("org:o"), &ana).is_err());
        // A private idea is the creator's alone.
        let private = gift("bo", Private);
        assert!(gift_visible(&Caller::member("org:o", "bo"), &private, &ana));
        assert!(!gift_visible(
            &Caller::member("org:o", "cy"),
            &private,
            &ana
        ));
        // An unlinked person has no recipient among members.
        let grandma = person(None, Family);
        assert!(gift_visible(
            &Caller::member("org:o", "ana"),
            &idea,
            &grandma
        ));
        assert!(!gift_visible(&Caller::shared("org:o"), &idea, &grandma));
    }

    #[test]
    fn wishlist_claims_are_never_shown_to_the_owner_or_the_agent() {
        let ana = person(Some("ana"), Family);
        let claimed = wish(Some("bo"));
        assert_eq!(
            wish_claim_view(&Caller::member("org:o", "ana"), &claimed, &ana),
            None
        );
        assert_eq!(
            wish_claim_view(&Caller::shared("org:o"), &claimed, &ana),
            None
        );
        assert_eq!(
            wish_claim_view(&Caller::member("org:o", "bo"), &claimed, &ana),
            Some(ClaimView {
                claimed: true,
                by_me: true
            })
        );
        assert_eq!(
            wish_claim_view(&Caller::member("org:o", "cy"), &claimed, &ana),
            Some(ClaimView {
                claimed: true,
                by_me: false
            })
        );
        assert!(!wish_can_claim(
            &Caller::member("org:o", "ana"),
            &claimed,
            &ana
        ));
        assert!(!wish_can_claim(
            &Caller::member("org:o", "cy"),
            &claimed,
            &ana
        ));
        assert!(wish_can_claim(
            &Caller::member("org:o", "bo"),
            &claimed,
            &ana
        ));
        assert!(!wish_can_claim(&Caller::shared("org:o"), &wish(None), &ana));
        // Only owners edit.
        assert!(wish_editable(
            &Caller::member("org:o", "ana"),
            &claimed,
            &ana
        ));
        assert!(!wish_editable(
            &Caller::member("org:o", "bo"),
            &claimed,
            &ana
        ));
    }

    #[test]
    fn sensitive_preferences_are_controlled_by_the_person_or_a_guardian_proxy() {
        let linked = person(Some("ana"), Family);
        let unlinked = person(None, Family);
        let bo = Caller::member("org:o", "bo");
        let ana = Caller::member("org:o", "ana");
        assert_eq!(
            new_pref_visibility(&bo, &linked, true, None).unwrap(),
            Private
        );
        assert!(new_pref_visibility(&bo, &linked, true, Some(Family)).is_err());
        assert_eq!(
            new_pref_visibility(&ana, &linked, true, Some(Family)).unwrap(),
            Family
        );
        // The creator of an unlinked person's sensitive row is its guardian proxy.
        assert_eq!(
            new_pref_visibility(&bo, &unlinked, true, Some(Family)).unwrap(),
            Family
        );
        assert!(new_pref_visibility(&Caller::shared("org:o"), &linked, true, None).is_err());

        let row = PrefRow {
            id: "x".into(),
            tenant: "org:o".into(),
            person_id: "p".into(),
            key: "allergy".into(),
            value: "peanuts".into(),
            sensitive: true,
            subject_user_id: Some("ana".into()),
            visibility: Private,
            created_by: "bo".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(pref_visible(&bo, &row, &linked));
        assert!(pref_visible(&ana, &row, &linked));
        assert!(!pref_visible(&Caller::member("org:o", "cy"), &row, &linked));
        assert!(!pref_visible(&Caller::shared("org:o"), &row, &linked));
        assert!(!pref_can_change_visibility(&bo, &row, &linked));
        assert!(pref_can_change_visibility(&ana, &row, &linked));
        assert!(patched_pref_flags(&bo, &row, &linked, None, Some(Family)).is_err());
        assert_eq!(
            patched_pref_flags(&ana, &row, &linked, None, Some(Family)).unwrap(),
            (true, Family)
        );
        let shared_family = PrefRow {
            visibility: Family,
            ..row
        };
        // The shared lane may be a group chat: a sensitive row stays out of it
        // even once its person shared it with the family.
        assert!(!pref_visible(
            &Caller::shared("org:o"),
            &shared_family,
            &linked
        ));
        assert!(pref_visible(
            &Caller::member("org:o", "cy"),
            &shared_family,
            &linked
        ));
        assert!(!pref_editable(
            &Caller::shared("org:o"),
            &shared_family,
            &linked
        ));
    }

    #[test]
    fn linking_a_person_never_hands_over_rows_written_before_the_link() {
        // Bo wrote a private sensitive row for unlinked Grandma (no subject),
        // then Cy said "This is me" for Grandma.
        let claimed = PersonRow {
            created_by: "bo".into(),
            ..person(Some("cy"), Family)
        };
        let row = PrefRow {
            id: "x".into(),
            tenant: "org:o".into(),
            person_id: "p".into(),
            key: "medication".into(),
            value: "warfarin".into(),
            sensitive: true,
            subject_user_id: None,
            visibility: Private,
            created_by: "bo".into(),
            created_at: 0,
            updated_at: 0,
        };
        let cy = Caller::member("org:o", "cy");
        let bo = Caller::member("org:o", "bo");
        assert!(!pref_visible(&cy, &row, &claimed));
        assert!(pref_visible(&bo, &row, &claimed));
        assert!(pref_can_change_visibility(&bo, &row, &claimed));
        assert!(patched_pref_flags(&cy, &row, &claimed, None, Some(Family)).is_err());
        let old_wish = WishRow {
            subject_user_id: None,
            created_by: "bo".into(),
            visibility: Private,
            ..wish(None)
        };
        assert!(!wish_visible(&cy, &old_wish, &claimed));
        assert!(!wish_editable(&cy, &old_wish, &claimed));
        // Rows written after the link do belong to the linked member.
        let new_row = PrefRow {
            subject_user_id: Some("cy".into()),
            ..row
        };
        assert!(pref_visible(&cy, &new_row, &claimed));
        assert!(!pref_can_change_visibility(&bo, &new_row, &claimed));
        // And the linked member can never delete the person.
        assert!(!person_deletable(&cy, &claimed));
        assert!(person_deletable(&bo, &claimed));
    }

    #[test]
    fn only_the_creator_makes_a_row_private() {
        let bo = Caller::member("org:o", "bo");
        let cy = Caller::member("org:o", "cy");
        assert_eq!(
            patched_visibility(&bo, Family, "bo", Some(Private)).unwrap(),
            Private
        );
        assert!(patched_visibility(&cy, Family, "bo", Some(Private)).is_err());
        // Re-sending the current value or making a row family-visible is fine.
        assert_eq!(
            patched_visibility(&cy, Private, "bo", Some(Private)).unwrap(),
            Private
        );
        assert_eq!(
            patched_visibility(&cy, Private, "bo", Some(Family)).unwrap(),
            Family
        );
        assert_eq!(patched_visibility(&cy, Family, "bo", None).unwrap(), Family);
    }

    #[test]
    fn reminder_recipients_exclude_the_birthday_honoree_and_muted_members() {
        let ana = person(Some("ana"), Family);
        let all = members(&["ana", "bo", "cy"]);
        let birthday = date(DateKind::Birthday, Family, "bo");
        assert_eq!(
            reminder_recipients(&birthday, &ana, &all, &[]),
            ["bo", "cy"]
        );
        assert_eq!(
            reminder_recipients(&birthday, &ana, &all, &["cy".to_owned()]),
            ["bo"]
        );
        let anniversary = date(DateKind::Anniversary, Family, "bo");
        assert_eq!(
            reminder_recipients(&anniversary, &ana, &all, &[]),
            ["ana", "bo", "cy"]
        );
        let private = date(DateKind::Birthday, Private, "bo");
        assert_eq!(reminder_recipients(&private, &ana, &all, &[]), ["bo"]);
        let agent_private = date(DateKind::Custom, Private, AGENT_ACTOR);
        assert!(reminder_recipients(&agent_private, &ana, &all, &[]).is_empty());
        // A private person makes the date effectively private.
        let hidden = person(None, Private);
        assert_eq!(
            reminder_recipients(&date(DateKind::Custom, Family, "cy"), &hidden, &all, &[]),
            ["cy"]
        );
    }

    #[test]
    fn people_lists_and_dates_follow_row_visibility() {
        let private_person = PersonRow {
            created_by: "bo".into(),
            ..person(None, Private)
        };
        assert!(person_visible(
            &Caller::member("org:o", "bo"),
            &private_person
        ));
        assert!(!person_visible(
            &Caller::member("org:o", "cy"),
            &private_person
        ));
        assert!(!person_visible(&Caller::shared("org:o"), &private_person));
        let linked = person(Some("ana"), Family);
        // The linked member never deletes their own person: the cascade would
        // wipe surprises they cannot see.
        assert!(!person_deletable(&Caller::member("org:o", "ana"), &linked));
        assert!(person_deletable(
            &Caller::member("org:o", "creator"),
            &linked
        ));
        // A person the linked member created themselves is not stuck.
        let self_made = PersonRow {
            created_by: "ana".into(),
            ..person(Some("ana"), Family)
        };
        assert!(!person_deletable(
            &Caller::member("org:o", "ana"),
            &self_made
        ));
        assert!(person_deletable(&Caller::member("org:o", "cy"), &self_made));
        assert!(!person_deletable(&Caller::member("org:o", "cy"), &linked));
        assert!(!person_deletable(&Caller::shared("org:o"), &linked));
        assert!(person_claimable(&Caller::member("org:o", "cy"), &linked).is_err());
        assert!(person_claimable(&Caller::shared("org:o"), &person(None, Family)).is_err());

        let shared = Caller::shared("org:o");
        assert!(patched_visibility(&shared, Family, "bo", Some(Private)).is_err());
        assert_eq!(new_row_visibility(&shared, Some(Private)), Family);
        let private_date = date(DateKind::Custom, Private, "bo");
        assert!(date_visible(
            &Caller::member("org:o", "bo"),
            &private_date,
            &linked
        ));
        assert!(!date_visible(
            &Caller::member("org:o", "ana"),
            &private_date,
            &linked
        ));
        assert!(!date_visible(&shared, &private_date, &linked));

        let list = ListRow {
            id: "l".into(),
            tenant: "org:o".into(),
            kind: crate::models::ListKind::Todo,
            name: "Mine".into(),
            visibility: Private,
            created_by: "bo".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(list_visible(&Caller::member("org:o", "bo"), &list));
        assert!(!list_visible(&Caller::member("org:o", "cy"), &list));
        assert!(!list_deletable(
            &shared,
            &ListRow {
                visibility: Family,
                ..list
            }
        ));
    }
}
