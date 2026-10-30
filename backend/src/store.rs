//! The SQLite store. Every public method takes the [`Caller`] and applies the
//! [`authz`] functions itself, so HTTP, MCP and the reminder sweep cannot see or
//! change a row differently. Every query filters by tenant: a row from another
//! tenant is indistinguishable from a missing one. Every write and its audit row
//! share one transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

use ryu_crypto::content_sqlite::{ColumnPlan, KeyKind};

use crate::seal_keys::{KeySource, PluginKeys};
use crate::seal_sql::{is_unreadable, open_store, opened, opened_opt, SqlSeal};
use crate::{
    authz::{self, Caller},
    models::*,
    tick::{advance_anchored, local_today, next_occurrence, resolve_tz, turning},
    validation::{self as v, ValidationError},
};

pub const SCHEMA_VERSION: i64 = 1;

/// How long a `pending` reminder claim is held before another sweep may take it
/// over: four times the `notify.deliver` timeout, so a live delivery is never
/// raced.
pub const CLAIM_LEASE_MS: i64 = 60_000;

const PRAGMAS: &str = "PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;";

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS people (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  display_name TEXT NOT NULL, relationship TEXT,
  member_user_id TEXT,
  visibility TEXT NOT NULL DEFAULT 'family' CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS ux_people_member ON people(tenant, member_user_id) WHERE member_user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS ix_people_tenant_only ON people(tenant);

CREATE TABLE IF NOT EXISTS dates (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  person_id TEXT NOT NULL REFERENCES people(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('birthday','anniversary','custom')),
  label TEXT, month INTEGER NOT NULL CHECK (month BETWEEN 1 AND 12), day INTEGER NOT NULL CHECK (day BETWEEN 1 AND 31),
  year INTEGER,
  remind_offsets_days TEXT NOT NULL DEFAULT '[7,1,0]',
  tz TEXT,
  visibility TEXT NOT NULL DEFAULT 'family' CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_dates_tenant ON dates(tenant, month, day);

CREATE TABLE IF NOT EXISTS lists (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('grocery','chores','todo','packing','meal_plan','custom')),
  name TEXT NOT NULL,
  visibility TEXT NOT NULL DEFAULT 'family' CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_lists_tenant ON lists(tenant, updated_at DESC);

CREATE TABLE IF NOT EXISTS list_items (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  list_id TEXT NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  text TEXT NOT NULL, qty TEXT,
  done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0,1)), done_at INTEGER, done_by TEXT,
  assignee_person_id TEXT REFERENCES people(id) ON DELETE SET NULL,
  due_on TEXT,
  recurrence TEXT NOT NULL DEFAULT 'none' CHECK (recurrence IN ('none','daily','weekly','monthly')),
  points INTEGER NOT NULL DEFAULT 0 CHECK (points BETWEEN 0 AND 1000),
  next_item_id TEXT,
  anchor_day INTEGER CHECK (anchor_day IS NULL OR anchor_day BETWEEN 1 AND 31),
  visibility TEXT NOT NULL,
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_items_list ON list_items(list_id, done, due_on);
CREATE INDEX IF NOT EXISTS ix_items_due ON list_items(tenant, done, due_on);

CREATE TABLE IF NOT EXISTS preferences (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  person_id TEXT NOT NULL REFERENCES people(id) ON DELETE CASCADE,
  key TEXT NOT NULL, value TEXT NOT NULL,
  sensitive INTEGER NOT NULL DEFAULT 0 CHECK (sensitive IN (0,1)),
  subject_user_id TEXT,
  visibility TEXT NOT NULL CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_prefs_person ON preferences(person_id, key);

CREATE TABLE IF NOT EXISTS wishlist_items (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  person_id TEXT NOT NULL REFERENCES people(id) ON DELETE CASCADE,
  title TEXT NOT NULL, url TEXT, price_hint TEXT,
  priority INTEGER NOT NULL DEFAULT 2 CHECK (priority IN (1,2,3)),
  claimed_by TEXT, claimed_at INTEGER,
  subject_user_id TEXT,
  visibility TEXT NOT NULL DEFAULT 'family' CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_wish_person ON wishlist_items(person_id, priority);

CREATE TABLE IF NOT EXISTS gift_ideas (
  id TEXT PRIMARY KEY, tenant TEXT NOT NULL,
  person_id TEXT NOT NULL REFERENCES people(id) ON DELETE CASCADE,
  idea TEXT NOT NULL, url TEXT,
  budget_minor INTEGER CHECK (budget_minor IS NULL OR budget_minor > 0), budget_currency TEXT,
  status TEXT NOT NULL DEFAULT 'idea' CHECK (status IN ('idea','bought','given')),
  occasion TEXT,
  visibility TEXT NOT NULL DEFAULT 'family' CHECK (visibility IN ('family','private')),
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS ix_gifts_person ON gift_ideas(person_id, status);

CREATE TABLE IF NOT EXISTS reminder_claims (
  tenant TEXT NOT NULL,
  date_id TEXT NOT NULL REFERENCES dates(id) ON DELETE CASCADE,
  offset_days INTEGER NOT NULL, occurrence_year INTEGER NOT NULL, recipient TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','sent','failed')),
  notification_id TEXT, claimed_at INTEGER NOT NULL,
  visibility TEXT NOT NULL DEFAULT 'private', created_by TEXT NOT NULL DEFAULT 'system',
  PRIMARY KEY (date_id, offset_days, occurrence_year, recipient));

CREATE TABLE IF NOT EXISTS settings (
  tenant TEXT PRIMARY KEY,
  tz TEXT NOT NULL DEFAULT 'UTC',
  reminder_hour INTEGER NOT NULL DEFAULT 9 CHECK (reminder_hour BETWEEN 0 AND 23),
  default_offsets_days TEXT NOT NULL DEFAULT '[7,1,0]',
  currency TEXT NOT NULL DEFAULT 'USD',
  agent_access INTEGER NOT NULL DEFAULT 0 CHECK (agent_access IN (0,1)),
  muted_user_ids TEXT NOT NULL DEFAULT '[]',
  visibility TEXT NOT NULL DEFAULT 'family',
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, updated_by TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tenant TEXT NOT NULL,
  actor TEXT NOT NULL, action TEXT NOT NULL, table_name TEXT NOT NULL, row_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  visibility TEXT NOT NULL DEFAULT 'private', created_by TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS ix_audit_tenant ON audit(tenant, actor);
CREATE TRIGGER IF NOT EXISTS audit_no_update BEFORE UPDATE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_delete BEFORE DELETE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
";

const PERSON_COLS: &str =
    "id, tenant, ryu_open('people.display_name', id, display_name) AS display_name, ryu_open('people.relationship', id, relationship) AS relationship, member_user_id, visibility, created_by, created_at, updated_at";
const DATE_COLS: &str = "id, tenant, person_id, kind, ryu_open('dates.label', id, label) AS label, month, day, year, remind_offsets_days, tz, visibility, created_by, created_at, updated_at";
const LIST_COLS: &str = "id, tenant, kind, ryu_open('lists.name', id, name) AS name, visibility, created_by, created_at, updated_at";
const ITEM_COLS: &str = "id, tenant, list_id, ryu_open('list_items.text', id, text) AS text, ryu_open('list_items.qty', id, qty) AS qty, done, done_at, done_by, assignee_person_id, due_on, recurrence, points, next_item_id, visibility, created_by, created_at, updated_at";
const PREF_COLS: &str = "id, tenant, person_id, ryu_open('preferences.key', id, key) AS key, ryu_open('preferences.value', id, value) AS value, sensitive, visibility, created_by, created_at, updated_at, subject_user_id";
const WISH_COLS: &str = "id, tenant, person_id, ryu_open('wishlist_items.title', id, title) AS title, ryu_open('wishlist_items.url', id, url) AS url, ryu_open('wishlist_items.price_hint', id, price_hint) AS price_hint, priority, claimed_by, claimed_at, visibility, created_by, created_at, updated_at, subject_user_id";
const GIFT_COLS: &str = "id, tenant, person_id, ryu_open('gift_ideas.idea', id, idea) AS idea, ryu_open('gift_ideas.url', id, url) AS url, budget_minor, budget_currency, status, ryu_open('gift_ideas.occasion', id, occasion) AS occasion, visibility, created_by, created_at, updated_at";
const SETTINGS_COLS: &str = "tenant, tz, reminder_hour, default_offsets_days, currency, agent_access, muted_user_ids, created_by, created_at, updated_at, updated_by";

/// The key of one reminder claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimKey {
    pub date_id: String,
    pub offset_days: i64,
    pub occurrence_year: i32,
    pub recipient: String,
}

/// What the sweep needs for one tenant.
#[derive(Debug, Clone, Default)]
pub struct ReminderPlan {
    pub dates: Vec<(DateRow, PersonRow)>,
    pub members: BTreeSet<String>,
}

/// One audit row (tests and internal inspection only; there is no read route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub actor: String,
    pub action: String,
    pub table_name: String,
    pub row_id: String,
}

/// The registered content-store name (part of every key derivation; never rename).
pub const STORE: &str = "family.db";

/// The sealed columns (`encb:v1:`, bound to `<table>.<column>` and the row id).
pub const SEALED_COLUMNS: &[(&str, &str)] = &[
    ("people", "display_name"),
    ("people", "relationship"),
    ("dates", "label"),
    ("lists", "name"),
    ("list_items", "text"),
    ("list_items", "qty"),
    ("preferences", "key"),
    ("preferences", "value"),
    ("wishlist_items", "title"),
    ("wishlist_items", "url"),
    ("wishlist_items", "price_hint"),
    ("gift_ideas", "idea"),
    ("gift_ideas", "url"),
    ("gift_ideas", "occasion"),
];

/// The sealed columns, for the open-time migration.
pub fn column_plans() -> anyhow::Result<Vec<ColumnPlan>> {
    SEALED_COLUMNS
        .iter()
        .map(|(table, column)| ColumnPlan::text(table, column, "id", KeyKind::Text))
        .collect()
}

#[derive(Clone)]
pub struct FamilyStore {
    conn: Arc<Mutex<Connection>>,
    seal: SqlSeal,
}

impl FamilyStore {
    /// Open the store, resolving this process's sealing key with the store's
    /// directory as the plugin data dir (see [`crate::seal_keys`]).
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let keys = PluginKeys::resolve(crate::state::PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, &keys)
    }

    /// [`Self::open`] with explicit keys.
    pub fn open_with_keys(path: PathBuf, keys: &PluginKeys) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", path.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening family db at {}", path.display()))?;
        Self::from_connection(conn, keys, Some(&path))
    }

    /// An in-memory store (tests) sealing under a random key, so every test
    /// runs through the sealed read and write paths.
    pub fn open_memory() -> anyhow::Result<Self> {
        Self::open_memory_with_keys(&ephemeral_keys()?)
    }

    /// An in-memory store under `keys` (tests).
    pub fn open_memory_with_keys(keys: &PluginKeys) -> anyhow::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, keys, None)
    }

    fn from_connection(
        mut conn: Connection,
        keys: &PluginKeys,
        path: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        // Before any statement that could rewrite a cell.
        ryu_crypto::content_sqlite::enable_secure_delete(&conn)?;
        conn.execute_batch(PRAGMAS)
            .context("configuring family db")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            conn.execute_batch(SCHEMA_V1)
                .context("creating family schema")?;
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }
        // The old (tenant, display_name) index would hold the sealed name; the
        // name is sorted after opening instead.
        conn.execute_batch(
            "DROP INDEX IF EXISTS ix_people_tenant;
             CREATE INDEX IF NOT EXISTS ix_people_tenant_only ON people(tenant);",
        )?;
        let seal = open_store(&mut conn, keys, STORE, &column_plans()?, path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            seal,
        })
    }

    /// The sealing health item: `{state, detail?, unreadable_rows}`.
    pub fn seal_health(&self) -> serde_json::Value {
        self.seal.health()
    }

    /// A keyless process sharing the store with the keyed sidecar: stop
    /// writing plaintext once the sidecar has sealed the store.
    pub async fn refresh_seal(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        self.seal.refresh_compat(&conn)
    }

    pub async fn ping(&self) -> FamilyResult<()> {
        let conn = self.conn.lock().await;
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
        Ok(())
    }

    // ── settings ──────────────────────────────────────────────────────────────

    /// Create the tenant's settings with defaults on first contact. Agent access
    /// always starts off: the MCP lane serves one tenant and Core forwards no
    /// caller into it, so a family must opt in explicitly in Settings.
    ///
    /// The first contact of an `org:` tenant means the node is now bound to a
    /// family organization, so agent access still held by the node's earlier
    /// personal (`local` or `user:`) tenant is dropped: the organization's
    /// members must never be served that personal data by the agent.
    pub async fn ensure_settings(&self, tenant: &str, actor: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let now = now_ms();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO settings (tenant, agent_access, created_by, created_at, updated_at, updated_by)
             VALUES (?1, 0, ?2, ?3, ?3, ?2)",
            params![tenant, actor, now],
        )?;
        if inserted > 0 {
            write_audit(&tx, tenant, actor, "settings.create", "settings", tenant)?;
            if authz::tenant_kind(tenant) == "org" {
                let revoked = query_rows(
                    &tx,
                    "SELECT tenant FROM settings WHERE agent_access = 1 AND tenant NOT LIKE 'org:%'",
                    [],
                    |row| row.get::<_, String>(0),
                )?;
                for personal in revoked {
                    tx.execute(
                        "UPDATE settings SET agent_access = 0, updated_at = ?2, updated_by = ?3 WHERE tenant = ?1",
                        params![personal, now, authz::SYSTEM_ACTOR],
                    )?;
                    write_audit(
                        &tx,
                        &personal,
                        authz::SYSTEM_ACTOR,
                        "settings.agent_access_revoked",
                        "settings",
                        &personal,
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn get_settings(&self, caller: &Caller) -> FamilyResult<SettingsResponse> {
        let conn = self.conn.lock().await;
        settings_response(&conn, caller)
    }

    pub async fn patch_settings(
        &self,
        caller: &Caller,
        patch: SettingsPatch,
    ) -> FamilyResult<SettingsResponse> {
        if caller.user_id().is_none() {
            return Err(FamilyError::forbidden());
        }
        if patch.tz.is_none()
            && patch.reminder_hour.is_none()
            && patch.default_remind_offsets_days.is_none()
            && patch.currency.is_none()
            && patch.agent_access.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let tz = patch.tz.as_deref().map(v::time_zone).transpose()?;
        let hour = patch.reminder_hour.map(v::reminder_hour).transpose()?;
        let offsets = patch
            .default_remind_offsets_days
            .as_deref()
            .map(v::offsets)
            .transpose()?;
        let currency = patch.currency.as_deref().map(v::currency).transpose()?;
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let current = settings_or_default(&tx, &caller.tenant)?;
        let now = now_ms();
        tx.execute(
            "UPDATE settings SET tz = ?2, reminder_hour = ?3, default_offsets_days = ?4, currency = ?5,
             updated_at = ?6, updated_by = ?7 WHERE tenant = ?1",
            params![
                caller.tenant,
                tz.unwrap_or(current.tz),
                hour.unwrap_or(current.reminder_hour),
                serde_json::to_string(&offsets.unwrap_or(current.default_offsets_days))?,
                currency.unwrap_or(current.currency),
                now,
                caller.actor(),
            ],
        )?;
        match patch.agent_access {
            // At most one tenant holds agent access at any time, and no tenant
            // can take it from another: the holder must turn it off first.
            Some(true) => {
                let held_elsewhere: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM settings WHERE agent_access = 1 AND tenant <> ?1)",
                    params![caller.tenant],
                    |row| row.get(0),
                )?;
                if held_elsewhere {
                    return Err(FamilyError::Conflict(
                        "Another family on this device already uses the Family Planner agent."
                            .to_owned(),
                    ));
                }
                tx.execute(
                    "UPDATE settings SET agent_access = 1 WHERE tenant = ?1",
                    params![caller.tenant],
                )?;
            }
            Some(false) => {
                tx.execute(
                    "UPDATE settings SET agent_access = 0 WHERE tenant = ?1",
                    params![caller.tenant],
                )?;
            }
            None => {}
        }
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "settings.update",
            "settings",
            &caller.tenant,
        )?;
        let response = settings_response(&tx, caller)?;
        tx.commit()?;
        Ok(response)
    }

    /// Mute or unmute the caller's own reminders. Each member edits only themselves.
    pub async fn set_muted(&self, caller: &Caller, muted: bool) -> FamilyResult<SettingsResponse> {
        let Some(user) = caller.user_id() else {
            return Err(FamilyError::forbidden());
        };
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let current = settings_or_default(&tx, &caller.tenant)?;
        let mut ids = current.muted_user_ids;
        ids.retain(|id| id != user);
        if muted {
            ids.push(user.to_owned());
        }
        tx.execute(
            "UPDATE settings SET muted_user_ids = ?2, updated_at = ?3, updated_by = ?4 WHERE tenant = ?1",
            params![caller.tenant, serde_json::to_string(&ids)?, now_ms(), user],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            user,
            "settings.update",
            "settings",
            &caller.tenant,
        )?;
        let response = settings_response(&tx, caller)?;
        tx.commit()?;
        Ok(response)
    }

    /// The single tenant the MCP lane serves, if any.
    pub async fn agent_tenant(&self) -> FamilyResult<Option<String>> {
        let conn = self.conn.lock().await;
        Ok(conn
            .query_row(
                "SELECT tenant FROM settings WHERE agent_access = 1 ORDER BY updated_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    // ── people ────────────────────────────────────────────────────────────────

    pub async fn list_people(&self, caller: &Caller) -> FamilyResult<Vec<Person>> {
        let conn = self.conn.lock().await;
        let mut people = query_rows(
            &conn,
            &format!("SELECT {PERSON_COLS} FROM people WHERE tenant = ?1 ORDER BY display_name COLLATE NOCASE, created_at"),
            params![caller.tenant],
            person_from_row,
        )?;
        people.retain(|person| authz::person_visible(caller, person));
        people.truncate(MAX_LIST_ROWS);
        Ok(people
            .iter()
            .map(|person| person_view(caller, person))
            .collect())
    }

    pub async fn create_person(&self, caller: &Caller, input: NewPerson) -> FamilyResult<Person> {
        let display_name =
            v::required_text("displayName", &input.display_name, v::MAX_DISPLAY_NAME)?;
        let relationship = v::optional_text(
            "relationship",
            input.relationship.as_deref(),
            v::MAX_RELATIONSHIP,
        )?;
        let visibility = authz::new_row_visibility(caller, input.visibility);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO people (id, tenant, display_name, relationship, member_user_id, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ryu_seal('people.display_name', ?1, ?3), ryu_seal('people.relationship', ?1, ?4), NULL, ?5, ?6, ?7, ?7)",
            params![id, caller.tenant, display_name, relationship, visibility.as_str(), caller.actor(), now],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "people.create",
            "people",
            &id,
        )?;
        let person = require(get_person(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(person_view(caller, &person))
    }

    pub async fn update_person(
        &self,
        caller: &Caller,
        id: &str,
        patch: PersonPatch,
    ) -> FamilyResult<Person> {
        if patch.display_name.is_none()
            && patch.relationship.is_none()
            && patch.visibility.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, id)?;
        if !authz::person_editable(caller, &person) {
            return Err(FamilyError::forbidden());
        }
        let display_name = match patch.display_name.as_deref() {
            Some(value) => v::required_text("displayName", value, v::MAX_DISPLAY_NAME)?,
            None => person.display_name.clone(),
        };
        let relationship = match patch.relationship {
            Some(value) => v::optional_text("relationship", value.as_deref(), v::MAX_RELATIONSHIP)?,
            None => person.relationship.clone(),
        };
        let visibility = authz::patched_visibility(
            caller,
            person.visibility,
            &person.created_by,
            patch.visibility,
        )?;
        tx.execute(
            "UPDATE people SET display_name = ryu_seal('people.display_name', ?2, ?3), relationship = ryu_seal('people.relationship', ?2, ?4), visibility = ?5, updated_at = ?6 WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id, display_name, relationship, visibility.as_str(), now_ms()],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "people.update",
            "people",
            id,
        )?;
        let person = require(get_person(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(person_view(caller, &person))
    }

    /// Deleting a person cascades to their dates, preferences, wishlist and gift
    /// ideas, so nothing about them is ever revealed by the deletion.
    pub async fn delete_person(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, id)?;
        if !authz::person_deletable(caller, &person) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM people WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "people.delete",
            "people",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// "This is me": link the caller's account to an unlinked family person.
    /// There is no unlink in Phase 1 (it would let a recipient reveal surprises).
    pub async fn claim_person(&self, caller: &Caller, id: &str) -> FamilyResult<Person> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, id)?;
        authz::person_claimable(caller, &person)?;
        let user = caller.actor().to_owned();
        if linked_person_id(&tx, &caller.tenant, &user)?.is_some() {
            return Err(FamilyError::Conflict(
                "Your account is already linked to a person in this family.".to_owned(),
            ));
        }
        let changed = tx.execute(
            "UPDATE people SET member_user_id = ?3, updated_at = ?4 WHERE tenant = ?1 AND id = ?2 AND member_user_id IS NULL",
            params![caller.tenant, id, user, now_ms()],
        );
        match changed {
            Ok(1) => {}
            Ok(_) => {
                return Err(FamilyError::Conflict(
                    "This person can't be linked to your account.".to_owned(),
                ))
            }
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(FamilyError::Conflict(
                    "Your account is already linked to a person in this family.".to_owned(),
                ))
            }
            Err(error) => return Err(error.into()),
        }
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "people.link",
            "people",
            id,
        )?;
        let person = require(get_person(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(person_view(caller, &person))
    }

    // ── dates ─────────────────────────────────────────────────────────────────

    pub async fn list_dates(
        &self,
        caller: &Caller,
        person_id: Option<&str>,
    ) -> FamilyResult<Vec<FamilyDate>> {
        self.list_dates_at(caller, person_id, Utc::now()).await
    }

    pub async fn list_dates_at(
        &self,
        caller: &Caller,
        person_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> FamilyResult<Vec<FamilyDate>> {
        let conn = self.conn.lock().await;
        if let Some(person_id) = person_id {
            visible_person(&conn, caller, person_id)?;
        }
        let settings = settings_or_default(&conn, &caller.tenant)?;
        let people = people_map(&conn, &caller.tenant)?;
        let mut dates: Vec<FamilyDate> = query_rows(
            &conn,
            &format!("SELECT {DATE_COLS} FROM dates WHERE tenant = ?1 AND (?2 IS NULL OR person_id = ?2) ORDER BY month, day"),
            params![caller.tenant, person_id],
            date_from_row,
        )?
        .iter()
        .filter_map(|date| {
            let person = people.get(&date.person_id)?;
            authz::date_visible(caller, date, person)
                .then(|| date_view(caller, date, person, &settings.tz, now))
        })
        .collect();
        dates.sort_by(|left, right| {
            left.days_away
                .cmp(&right.days_away)
                .then_with(|| left.base.id.cmp(&right.base.id))
        });
        dates.truncate(MAX_LIST_ROWS);
        Ok(dates)
    }

    pub async fn create_date(&self, caller: &Caller, input: NewDate) -> FamilyResult<FamilyDate> {
        if input.person_id.trim().is_empty() {
            return Err(ValidationError::Required { field: "personId" }.into());
        }
        let (month, day) = v::month_day(input.month, input.day)?;
        let label = v::optional_text("label", input.label.as_deref(), v::MAX_LABEL)?;
        let year = v::year(input.year)?;
        let tz = v::optional_time_zone(input.tz.as_deref())?;
        let explicit_offsets = input
            .remind_offsets_days
            .as_deref()
            .map(v::offsets)
            .transpose()?;
        let visibility = authz::new_row_visibility(caller, input.visibility);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, input.person_id.trim())?;
        let settings = settings_or_default(&tx, &caller.tenant)?;
        let offsets = explicit_offsets.unwrap_or_else(|| settings.default_offsets_days.clone());
        tx.execute(
            "INSERT INTO dates (id, tenant, person_id, kind, label, month, day, year, remind_offsets_days, tz, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ryu_seal('dates.label', ?1, ?5), ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                id,
                caller.tenant,
                person.id,
                input.kind.as_str(),
                label,
                month,
                day,
                year,
                serde_json::to_string(&offsets)?,
                tz,
                visibility.as_str(),
                caller.actor(),
                now
            ],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "dates.create",
            "dates",
            &id,
        )?;
        let date = require(get_date(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(date_view(caller, &date, &person, &settings.tz, Utc::now()))
    }

    pub async fn update_date(
        &self,
        caller: &Caller,
        id: &str,
        patch: DatePatch,
    ) -> FamilyResult<FamilyDate> {
        if patch.kind.is_none()
            && patch.label.is_none()
            && patch.month.is_none()
            && patch.day.is_none()
            && patch.year.is_none()
            && patch.remind_offsets_days.is_none()
            && patch.tz.is_none()
            && patch.visibility.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (date, person) = visible_date(&tx, caller, id)?;
        if !authz::date_editable(caller, &date, &person) {
            return Err(FamilyError::forbidden());
        }
        let (month, day) = v::month_day(
            patch.month.unwrap_or(i64::from(date.month)),
            patch.day.unwrap_or(i64::from(date.day)),
        )?;
        let label = match patch.label {
            Some(value) => v::optional_text("label", value.as_deref(), v::MAX_LABEL)?,
            None => date.label.clone(),
        };
        let year = match patch.year {
            Some(value) => v::year(value)?,
            None => date.year,
        };
        let tz = match patch.tz {
            Some(value) => v::optional_time_zone(value.as_deref())?,
            None => date.tz.clone(),
        };
        let offsets = match patch.remind_offsets_days.as_deref() {
            Some(values) => v::offsets(values)?,
            None => date.remind_offsets_days.clone(),
        };
        let visibility =
            authz::patched_visibility(caller, date.visibility, &date.created_by, patch.visibility)?;
        if month != date.month || day != date.day {
            // Claims are keyed by occurrence year, not day. After the day moves,
            // an old claim would swallow the corrected date's reminders, so the
            // date's claims go. The sweep has no catch-up, so only lead times
            // that fall today or later can fire for the moved date.
            tx.execute(
                "DELETE FROM reminder_claims WHERE tenant = ?1 AND date_id = ?2",
                params![caller.tenant, id],
            )?;
        }
        tx.execute(
            "UPDATE dates SET kind = ?3, label = ryu_seal('dates.label', ?2, ?4), month = ?5, day = ?6, year = ?7, remind_offsets_days = ?8, tz = ?9,
             visibility = ?10, updated_at = ?11 WHERE tenant = ?1 AND id = ?2",
            params![
                caller.tenant,
                id,
                patch.kind.unwrap_or(date.kind).as_str(),
                label,
                month,
                day,
                year,
                serde_json::to_string(&offsets)?,
                tz,
                visibility.as_str(),
                now_ms()
            ],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "dates.update",
            "dates",
            id,
        )?;
        let settings = settings_or_default(&tx, &caller.tenant)?;
        let date = require(get_date(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(date_view(caller, &date, &person, &settings.tz, Utc::now()))
    }

    pub async fn delete_date(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (date, person) = visible_date(&tx, caller, id)?;
        if !authz::date_editable(caller, &date, &person) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM dates WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "dates.delete",
            "dates",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── lists and items ───────────────────────────────────────────────────────

    pub async fn list_lists(
        &self,
        caller: &Caller,
        kind: Option<ListKind>,
    ) -> FamilyResult<Vec<FamilyList>> {
        let conn = self.conn.lock().await;
        let mut lists = query_rows(
            &conn,
            &format!("SELECT {LIST_COLS} FROM lists WHERE tenant = ?1 AND (?2 IS NULL OR kind = ?2) ORDER BY updated_at DESC, id"),
            params![caller.tenant, kind.map(ListKind::as_str)],
            list_from_row,
        )?;
        lists.retain(|list| authz::list_visible(caller, list));
        lists.truncate(MAX_LIST_ROWS);
        let counts = list_counts(&conn, &caller.tenant)?;
        Ok(lists
            .iter()
            .map(|list| {
                list_view(
                    caller,
                    list,
                    counts.get(&list.id).copied().unwrap_or_default(),
                )
            })
            .collect())
    }

    pub async fn create_list(&self, caller: &Caller, input: NewList) -> FamilyResult<FamilyList> {
        let name = v::required_text("name", &input.name, v::MAX_LIST_NAME)?;
        let visibility = authz::new_row_visibility(caller, input.visibility);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO lists (id, tenant, kind, name, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ryu_seal('lists.name', ?1, ?4), ?5, ?6, ?7, ?7)",
            params![id, caller.tenant, input.kind.as_str(), name, visibility.as_str(), caller.actor(), now],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "lists.create",
            "lists",
            &id,
        )?;
        let list = require(get_list(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(list_view(caller, &list, (0, 0)))
    }

    /// Changing a list's visibility moves all of its items in the same transaction.
    pub async fn update_list(
        &self,
        caller: &Caller,
        id: &str,
        patch: ListPatch,
    ) -> FamilyResult<FamilyList> {
        if patch.name.is_none() && patch.kind.is_none() && patch.visibility.is_none() {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let list = visible_list(&tx, caller, id)?;
        if !authz::list_editable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        let name = match patch.name.as_deref() {
            Some(value) => v::required_text("name", value, v::MAX_LIST_NAME)?,
            None => list.name.clone(),
        };
        let visibility =
            authz::patched_visibility(caller, list.visibility, &list.created_by, patch.visibility)?;
        let now = now_ms();
        tx.execute(
            "UPDATE lists SET name = ryu_seal('lists.name', ?2, ?3), kind = ?4, visibility = ?5, updated_at = ?6 WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id, name, patch.kind.unwrap_or(list.kind).as_str(), visibility.as_str(), now],
        )?;
        tx.execute(
            "UPDATE list_items SET visibility = ?3 WHERE tenant = ?1 AND list_id = ?2",
            params![caller.tenant, id, visibility.as_str()],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "lists.update",
            "lists",
            id,
        )?;
        let list = require(get_list(&tx, &caller.tenant, id)?)?;
        let counts = list_counts(&tx, &caller.tenant)?;
        tx.commit()?;
        Ok(list_view(
            caller,
            &list,
            counts.get(id).copied().unwrap_or_default(),
        ))
    }

    pub async fn delete_list(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let list = visible_list(&tx, caller, id)?;
        if !authz::list_deletable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM lists WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "lists.delete",
            "lists",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub async fn list_items(
        &self,
        caller: &Caller,
        list_id: &str,
        include_done: bool,
    ) -> FamilyResult<(FamilyList, Vec<ListItem>)> {
        let conn = self.conn.lock().await;
        let list = visible_list(&conn, caller, list_id)?;
        let mut items = query_rows(
            &conn,
            &format!(
                "SELECT {ITEM_COLS} FROM list_items WHERE tenant = ?1 AND list_id = ?2 AND (?3 OR done = 0)
                 ORDER BY done, due_on IS NULL, due_on, created_at, id"
            ),
            params![caller.tenant, list_id, include_done],
            item_from_row,
        )?;
        items.truncate(MAX_LIST_ROWS);
        let counts = list_counts(&conn, &caller.tenant)?;
        Ok((
            list_view(
                caller,
                &list,
                counts.get(list_id).copied().unwrap_or_default(),
            ),
            items
                .iter()
                .map(|item| item_view(caller, item, &list))
                .collect(),
        ))
    }

    pub async fn add_items(
        &self,
        caller: &Caller,
        list_id: &str,
        items: Vec<NewListItem>,
    ) -> FamilyResult<Vec<ListItem>> {
        if items.is_empty() || items.len() > v::MAX_ITEMS_PER_ADD {
            return Err(ValidationError::InvalidItemCount.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let list = visible_list(&tx, caller, list_id)?;
        if !authz::list_editable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        let now = now_ms();
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let text = v::required_text("text", &item.text, v::MAX_ITEM_TEXT)?;
            let qty = v::optional_text("qty", item.qty.as_deref(), v::MAX_QTY)?;
            let due_on = v::optional_date("dueOn", item.due_on.as_deref())?;
            let points = v::points(item.points)?;
            let assignee = assignee_id(&tx, caller, item.assignee_person_id.as_deref())?;
            let id = new_id();
            tx.execute(
                "INSERT INTO list_items (id, tenant, list_id, text, qty, done, assignee_person_id, due_on, recurrence, points, anchor_day, visibility, created_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ryu_seal('list_items.text', ?1, ?4), ryu_seal('list_items.qty', ?1, ?5), 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                params![
                    id,
                    caller.tenant,
                    list.id,
                    text,
                    qty,
                    assignee,
                    due_on,
                    item.recurrence.unwrap_or(Recurrence::None).as_str(),
                    points,
                    anchor_day(due_on.as_deref()),
                    list.visibility.as_str(),
                    caller.actor(),
                    now
                ],
            )?;
            write_audit(
                &tx,
                &caller.tenant,
                caller.actor(),
                "list_items.create",
                "list_items",
                &id,
            )?;
            ids.push(id);
        }
        touch_list(&tx, &caller.tenant, &list.id, now)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let item = require(get_item(&tx, &caller.tenant, id)?)?;
            out.push(item_view(caller, &item, &list));
        }
        tx.commit()?;
        Ok(out)
    }

    pub async fn update_item(
        &self,
        caller: &Caller,
        id: &str,
        patch: ItemPatch,
    ) -> FamilyResult<ListItem> {
        if patch.text.is_none()
            && patch.qty.is_none()
            && patch.assignee_person_id.is_none()
            && patch.due_on.is_none()
            && patch.recurrence.is_none()
            && patch.points.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (item, list) = visible_item(&tx, caller, id)?;
        if !authz::list_editable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        let text = match patch.text.as_deref() {
            Some(value) => v::required_text("text", value, v::MAX_ITEM_TEXT)?,
            None => item.text.clone(),
        };
        let qty = match patch.qty {
            Some(value) => v::optional_text("qty", value.as_deref(), v::MAX_QTY)?,
            None => item.qty.clone(),
        };
        let due_changed = patch.due_on.is_some();
        let due_on = match patch.due_on {
            Some(value) => v::optional_date("dueOn", value.as_deref())?,
            None => item.due_on.clone(),
        };
        let points = match patch.points {
            Some(value) => v::points(Some(value))?,
            None => item.points,
        };
        let assignee = match patch.assignee_person_id {
            Some(value) => assignee_id(&tx, caller, value.as_deref())?,
            None => item.assignee_person_id.clone(),
        };
        let now = now_ms();
        // A new due date restarts the monthly anchor at its day.
        tx.execute(
            "UPDATE list_items SET text = ryu_seal('list_items.text', ?2, ?3), qty = ryu_seal('list_items.qty', ?2, ?4), assignee_person_id = ?5, due_on = ?6, recurrence = ?7, points = ?8,
             updated_at = MAX(?9, created_at + 1), anchor_day = CASE WHEN ?11 THEN ?10 ELSE anchor_day END WHERE tenant = ?1 AND id = ?2",
            params![
                caller.tenant,
                id,
                text,
                qty,
                assignee,
                due_on,
                patch.recurrence.unwrap_or(item.recurrence).as_str(),
                points,
                now,
                anchor_day(due_on.as_deref()),
                due_changed
            ],
        )?;
        touch_list(&tx, &caller.tenant, &list.id, now)?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "list_items.update",
            "list_items",
            id,
        )?;
        let item = require(get_item(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(item_view(caller, &item, &list))
    }

    pub async fn complete_item(
        &self,
        caller: &Caller,
        id: &str,
        done: bool,
    ) -> FamilyResult<(ListItem, Option<ListItem>)> {
        self.complete_item_at(caller, id, done, Utc::now()).await
    }

    /// Check or uncheck an item. Completing a recurring item creates its
    /// successor once (never twice), due one period after its due date, or after
    /// the completion's local date when it had none.
    pub async fn complete_item_at(
        &self,
        caller: &Caller,
        id: &str,
        done: bool,
        now: DateTime<Utc>,
    ) -> FamilyResult<(ListItem, Option<ListItem>)> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (item, list) = visible_item(&tx, caller, id)?;
        if !authz::list_editable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        let now_ms = now.timestamp_millis();
        if !done {
            if item.done {
                tx.execute(
                    "UPDATE list_items SET done = 0, done_at = NULL, done_by = NULL, updated_at = MAX(?3, created_at + 1) WHERE tenant = ?1 AND id = ?2",
                    params![caller.tenant, id, now_ms],
                )?;
                // An untouched successor (still open, never edited or checked:
                // every item write moves `updated_at` past `created_at`) goes
                // with the completion it came from, so one occurrence never
                // has two open copies. One that was touched stays, and so does
                // the pointer, so re-checking never spawns a second successor.
                if let Some(next_id) = item.next_item_id.as_deref() {
                    let removed = tx.execute(
                        "DELETE FROM list_items WHERE tenant = ?1 AND id = ?2 AND done = 0 AND updated_at = created_at AND next_item_id IS NULL",
                        params![caller.tenant, next_id],
                    )?;
                    if removed > 0 {
                        tx.execute(
                            "UPDATE list_items SET next_item_id = NULL WHERE tenant = ?1 AND id = ?2",
                            params![caller.tenant, id],
                        )?;
                        write_audit(
                            &tx,
                            &caller.tenant,
                            caller.actor(),
                            "list_items.delete",
                            "list_items",
                            next_id,
                        )?;
                    }
                }
                touch_list(&tx, &caller.tenant, &list.id, now_ms)?;
                write_audit(
                    &tx,
                    &caller.tenant,
                    caller.actor(),
                    "list_items.update",
                    "list_items",
                    id,
                )?;
            }
            let item = require(get_item(&tx, &caller.tenant, id)?)?;
            tx.commit()?;
            return Ok((item_view(caller, &item, &list), None));
        }
        if !item.done {
            tx.execute(
                "UPDATE list_items SET done = 1, done_at = ?3, done_by = ?4, updated_at = MAX(?3, created_at + 1) WHERE tenant = ?1 AND id = ?2",
                params![caller.tenant, id, now_ms, caller.actor()],
            )?;
            touch_list(&tx, &caller.tenant, &list.id, now_ms)?;
            write_audit(
                &tx,
                &caller.tenant,
                caller.actor(),
                "list_items.complete",
                "list_items",
                id,
            )?;
        }
        let mut next = None;
        if item.recurrence != Recurrence::None {
            // A pointer to a deleted successor counts as none.
            let existing = match item.next_item_id.as_deref() {
                Some(next_id) => get_item(&tx, &caller.tenant, next_id)?,
                None => None,
            };
            match existing {
                Some(existing) => next = Some(existing),
                None => {
                    let settings = settings_or_default(&tx, &caller.tenant)?;
                    let base = match item.due_on.as_deref() {
                        Some(due) => v::parse_date("dueOn", due)?,
                        None => local_today(resolve_tz(&settings.tz), now),
                    };
                    let stored_anchor: Option<u32> = tx.query_row(
                        "SELECT anchor_day FROM list_items WHERE tenant = ?1 AND id = ?2",
                        params![caller.tenant, id],
                        |row| row.get(0),
                    )?;
                    let anchor = stored_anchor.unwrap_or_else(|| base.day());
                    let due = advance_anchored(base, item.recurrence, anchor)
                        .map(|date| date.format("%Y-%m-%d").to_string());
                    let next_id = new_id();
                    tx.execute(
                        "INSERT INTO list_items (id, tenant, list_id, text, qty, done, assignee_person_id, due_on, recurrence, points, anchor_day, visibility, created_by, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ryu_seal('list_items.text', ?1, ?4), ryu_seal('list_items.qty', ?1, ?5), 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                        params![
                            next_id,
                            caller.tenant,
                            list.id,
                            item.text,
                            item.qty,
                            item.assignee_person_id,
                            due,
                            item.recurrence.as_str(),
                            item.points,
                            anchor,
                            list.visibility.as_str(),
                            caller.actor(),
                            now_ms
                        ],
                    )?;
                    tx.execute(
                        "UPDATE list_items SET next_item_id = ?3 WHERE tenant = ?1 AND id = ?2",
                        params![caller.tenant, id, next_id],
                    )?;
                    write_audit(
                        &tx,
                        &caller.tenant,
                        caller.actor(),
                        "list_items.create",
                        "list_items",
                        &next_id,
                    )?;
                    next = get_item(&tx, &caller.tenant, &next_id)?;
                }
            }
        }
        let item = require(get_item(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok((
            item_view(caller, &item, &list),
            next.map(|next| item_view(caller, &next, &list)),
        ))
    }

    pub async fn delete_item(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (_, list) = visible_item(&tx, caller, id)?;
        if !authz::list_editable(caller, &list) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM list_items WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        // A predecessor must not point at a deleted successor, or completing it
        // again would silently end the recurrence.
        tx.execute(
            "UPDATE list_items SET next_item_id = NULL WHERE tenant = ?1 AND next_item_id = ?2",
            params![caller.tenant, id],
        )?;
        touch_list(&tx, &caller.tenant, &list.id, now_ms())?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "list_items.delete",
            "list_items",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Points and completed counts over done items visible to the caller,
    /// grouped by assignee. A simple tally: no money is involved.
    pub async fn tally(
        &self,
        caller: &Caller,
        list_id: Option<&str>,
        since: Option<&str>,
    ) -> FamilyResult<Vec<TallyRow>> {
        let since = since
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| v::parse_date("since", value))
            .transpose()?;
        let conn = self.conn.lock().await;
        if let Some(list_id) = list_id {
            visible_list(&conn, caller, list_id)?;
        }
        let lists: HashMap<String, ListRow> = query_rows(
            &conn,
            &format!("SELECT {LIST_COLS} FROM lists WHERE tenant = ?1"),
            params![caller.tenant],
            list_from_row,
        )?
        .into_iter()
        .filter(|list| authz::list_visible(caller, list))
        .map(|list| (list.id.clone(), list))
        .collect();
        let tz = resolve_tz(&settings_or_default(&conn, &caller.tenant)?.tz);
        let people = people_map(&conn, &caller.tenant)?;
        let items = query_rows(
            &conn,
            &format!("SELECT {ITEM_COLS} FROM list_items WHERE tenant = ?1 AND done = 1 AND (?2 IS NULL OR list_id = ?2)"),
            params![caller.tenant, list_id],
            item_from_row,
        )?;
        let mut groups: BTreeMap<Option<String>, (i64, i64)> = BTreeMap::new();
        for item in items {
            if !lists.contains_key(&item.list_id) {
                continue;
            }
            if let (Some(since), Some(done_at)) = (since, item.done_at) {
                let Some(done_on) = tz.timestamp_millis_opt(done_at).single() else {
                    continue;
                };
                if done_on.date_naive() < since {
                    continue;
                }
            }
            let entry = groups.entry(item.assignee_person_id.clone()).or_default();
            entry.0 += item.points;
            entry.1 += 1;
        }
        let mut rows: Vec<TallyRow> = groups
            .into_iter()
            .map(|(assignee, (points, completed))| TallyRow {
                person_name: assignee
                    .as_deref()
                    .and_then(|id| people.get(id))
                    .filter(|person| authz::person_visible(caller, person))
                    .map(|person| person.display_name.clone()),
                assignee_person_id: assignee,
                points,
                completed,
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .points
                .cmp(&left.points)
                .then_with(|| right.completed.cmp(&left.completed))
                .then_with(|| left.assignee_person_id.cmp(&right.assignee_person_id))
        });
        Ok(rows)
    }

    // ── preferences ───────────────────────────────────────────────────────────

    pub async fn list_preferences(
        &self,
        caller: &Caller,
        person_id: &str,
    ) -> FamilyResult<Vec<Preference>> {
        if person_id.trim().is_empty() {
            return Err(ValidationError::Required { field: "personId" }.into());
        }
        let conn = self.conn.lock().await;
        let person = visible_person(&conn, caller, person_id.trim())?;
        let mut prefs = query_rows(
            &conn,
            &format!("SELECT {PREF_COLS} FROM preferences WHERE tenant = ?1 AND person_id = ?2 ORDER BY key COLLATE NOCASE, created_at"),
            params![caller.tenant, person.id],
            pref_from_row,
        )?;
        prefs.retain(|pref| authz::pref_visible(caller, pref, &person));
        prefs.truncate(MAX_LIST_ROWS);
        Ok(prefs
            .iter()
            .map(|pref| pref_view(caller, pref, &person))
            .collect())
    }

    pub async fn create_preference(
        &self,
        caller: &Caller,
        input: NewPreference,
    ) -> FamilyResult<Preference> {
        if input.person_id.trim().is_empty() {
            return Err(ValidationError::Required { field: "personId" }.into());
        }
        let key = v::required_text("key", &input.key, v::MAX_PREF_KEY)?;
        let value = v::required_text("value", &input.value, v::MAX_PREF_VALUE)?;
        let sensitive = input.sensitive.unwrap_or(false);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, input.person_id.trim())?;
        let visibility = authz::new_pref_visibility(caller, &person, sensitive, input.visibility)?;
        tx.execute(
            "INSERT INTO preferences (id, tenant, person_id, key, value, sensitive, subject_user_id, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ryu_seal('preferences.key', ?1, ?4), ryu_seal('preferences.value', ?1, ?5), ?6, ?7, ?8, ?9, ?10, ?10)",
            params![id, caller.tenant, person.id, key, value, sensitive, person.member_user_id, visibility.as_str(), caller.actor(), now],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "preferences.create",
            "preferences",
            &id,
        )?;
        let pref = require(get_pref(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(pref_view(caller, &pref, &person))
    }

    pub async fn update_preference(
        &self,
        caller: &Caller,
        id: &str,
        patch: PreferencePatch,
    ) -> FamilyResult<Preference> {
        if patch.key.is_none()
            && patch.value.is_none()
            && patch.sensitive.is_none()
            && patch.visibility.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (pref, person) = visible_pref(&tx, caller, id)?;
        if !authz::pref_editable(caller, &pref, &person) {
            return Err(FamilyError::forbidden());
        }
        let (sensitive, visibility) =
            authz::patched_pref_flags(caller, &pref, &person, patch.sensitive, patch.visibility)?;
        let key = match patch.key.as_deref() {
            Some(value) => v::required_text("key", value, v::MAX_PREF_KEY)?,
            None => pref.key.clone(),
        };
        let value = match patch.value.as_deref() {
            Some(value) => v::required_text("value", value, v::MAX_PREF_VALUE)?,
            None => pref.value.clone(),
        };
        tx.execute(
            "UPDATE preferences SET key = ryu_seal('preferences.key', ?2, ?3), value = ryu_seal('preferences.value', ?2, ?4), sensitive = ?5, visibility = ?6, updated_at = ?7 WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id, key, value, sensitive, visibility.as_str(), now_ms()],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "preferences.update",
            "preferences",
            id,
        )?;
        let pref = require(get_pref(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(pref_view(caller, &pref, &person))
    }

    pub async fn delete_preference(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (pref, person) = visible_pref(&tx, caller, id)?;
        if !authz::pref_editable(caller, &pref, &person) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM preferences WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "preferences.delete",
            "preferences",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── wishlist ──────────────────────────────────────────────────────────────

    pub async fn list_wishlist(
        &self,
        caller: &Caller,
        person_id: Option<&str>,
    ) -> FamilyResult<Vec<WishlistItem>> {
        let conn = self.conn.lock().await;
        if let Some(person_id) = person_id {
            visible_person(&conn, caller, person_id)?;
        }
        let people = people_map(&conn, &caller.tenant)?;
        let wishes = query_rows(
            &conn,
            &format!("SELECT {WISH_COLS} FROM wishlist_items WHERE tenant = ?1 AND (?2 IS NULL OR person_id = ?2) ORDER BY priority, created_at, id"),
            params![caller.tenant, person_id],
            wish_from_row,
        )?;
        let mut out: Vec<WishlistItem> = wishes
            .iter()
            .filter_map(|wish| {
                let person = people.get(&wish.person_id)?;
                authz::wish_visible(caller, wish, person).then(|| wish_view(caller, wish, person))
            })
            .collect();
        out.truncate(MAX_LIST_ROWS);
        Ok(out)
    }

    pub async fn create_wish(&self, caller: &Caller, input: NewWish) -> FamilyResult<WishlistItem> {
        if input.person_id.trim().is_empty() {
            return Err(ValidationError::Required { field: "personId" }.into());
        }
        let title = v::required_text("title", &input.title, v::MAX_WISH_TITLE)?;
        let url = v::url(input.url.as_deref())?;
        let price_hint =
            v::optional_text("priceHint", input.price_hint.as_deref(), v::MAX_PRICE_HINT)?;
        let priority = v::priority(input.priority)?;
        let visibility = authz::new_row_visibility(caller, input.visibility);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, input.person_id.trim())?;
        tx.execute(
            "INSERT INTO wishlist_items (id, tenant, person_id, title, url, price_hint, priority, subject_user_id, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ryu_seal('wishlist_items.title', ?1, ?4), ryu_seal('wishlist_items.url', ?1, ?5), ryu_seal('wishlist_items.price_hint', ?1, ?6), ?7, ?8, ?9, ?10, ?11, ?11)",
            params![id, caller.tenant, person.id, title, url, price_hint, priority, person.member_user_id, visibility.as_str(), caller.actor(), now],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "wishlist_items.create",
            "wishlist_items",
            &id,
        )?;
        let wish = require(get_wish(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(wish_view(caller, &wish, &person))
    }

    pub async fn update_wish(
        &self,
        caller: &Caller,
        id: &str,
        patch: WishPatch,
    ) -> FamilyResult<WishlistItem> {
        if patch.title.is_none()
            && patch.url.is_none()
            && patch.price_hint.is_none()
            && patch.priority.is_none()
            && patch.visibility.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (wish, person) = visible_wish(&tx, caller, id)?;
        if !authz::wish_editable(caller, &wish, &person) {
            return Err(FamilyError::forbidden());
        }
        let title = match patch.title.as_deref() {
            Some(value) => v::required_text("title", value, v::MAX_WISH_TITLE)?,
            None => wish.title.clone(),
        };
        let url = match patch.url {
            Some(value) => v::url(value.as_deref())?,
            None => wish.url.clone(),
        };
        let price_hint = match patch.price_hint {
            Some(value) => v::optional_text("priceHint", value.as_deref(), v::MAX_PRICE_HINT)?,
            None => wish.price_hint.clone(),
        };
        let priority = match patch.priority {
            Some(value) => v::priority(Some(value))?,
            None => wish.priority,
        };
        let visibility =
            authz::patched_visibility(caller, wish.visibility, &wish.created_by, patch.visibility)?;
        tx.execute(
            "UPDATE wishlist_items SET title = ryu_seal('wishlist_items.title', ?2, ?3), url = ryu_seal('wishlist_items.url', ?2, ?4), price_hint = ryu_seal('wishlist_items.price_hint', ?2, ?5), priority = ?6, visibility = ?7, updated_at = ?8
             WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id, title, url, price_hint, priority, visibility.as_str(), now_ms()],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "wishlist_items.update",
            "wishlist_items",
            id,
        )?;
        let wish = require(get_wish(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(wish_view(caller, &wish, &person))
    }

    pub async fn delete_wish(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (wish, person) = visible_wish(&tx, caller, id)?;
        if !authz::wish_editable(caller, &wish, &person) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM wishlist_items WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "wishlist_items.delete",
            "wishlist_items",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Claim a wishlist item. Idempotent for the caller's own claim; 409 when
    /// someone else holds it; the recipient always gets the same fixed 403.
    pub async fn claim_wish(&self, caller: &Caller, id: &str) -> FamilyResult<WishlistItem> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (wish, person) = visible_wish(&tx, caller, id)?;
        let Some(user) = caller.user_id() else {
            return Err(FamilyError::forbidden());
        };
        if authz::is_recipient(caller, &person) {
            return Err(authz::recipient_claim_error());
        }
        if authz::effectively_private(wish.visibility, &person) {
            return Err(FamilyError::forbidden());
        }
        match wish.claimed_by.as_deref() {
            Some(claimer) if claimer == user => {}
            Some(_) => {
                return Err(FamilyError::Conflict(
                    "Someone else is already getting this.".to_owned(),
                ))
            }
            None => {
                let now = now_ms();
                tx.execute(
                    // `updated_at` stays put: it reaches the owner and the shared lane, so a
                    // bump would reveal the claim.
                    "UPDATE wishlist_items SET claimed_by = ?3, claimed_at = ?4 WHERE tenant = ?1 AND id = ?2 AND claimed_by IS NULL",
                    params![caller.tenant, id, user, now],
                )?;
                write_audit(
                    &tx,
                    &caller.tenant,
                    user,
                    "wishlist_items.claim",
                    "wishlist_items",
                    id,
                )?;
            }
        }
        let wish = require(get_wish(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(wish_view(caller, &wish, &person))
    }

    /// Release the caller's own claim. A no-op when unclaimed; 403 when the claim
    /// is someone else's; the recipient always gets the same fixed 403.
    pub async fn unclaim_wish(&self, caller: &Caller, id: &str) -> FamilyResult<WishlistItem> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (wish, person) = visible_wish(&tx, caller, id)?;
        let Some(user) = caller.user_id() else {
            return Err(FamilyError::forbidden());
        };
        if authz::is_recipient(caller, &person) {
            return Err(authz::recipient_claim_error());
        }
        match wish.claimed_by.as_deref() {
            None => {}
            Some(claimer) if claimer == user => {
                tx.execute(
                    "UPDATE wishlist_items SET claimed_by = NULL, claimed_at = NULL WHERE tenant = ?1 AND id = ?2",
                    params![caller.tenant, id],
                )?;
                write_audit(
                    &tx,
                    &caller.tenant,
                    user,
                    "wishlist_items.unclaim",
                    "wishlist_items",
                    id,
                )?;
            }
            Some(_) => return Err(FamilyError::forbidden()),
        }
        let wish = require(get_wish(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(wish_view(caller, &wish, &person))
    }

    // ── gift ideas ────────────────────────────────────────────────────────────

    pub async fn list_gifts(
        &self,
        caller: &Caller,
        person_id: Option<&str>,
        status: Option<GiftStatus>,
    ) -> FamilyResult<Vec<GiftIdea>> {
        let conn = self.conn.lock().await;
        if let Some(person_id) = person_id {
            visible_person(&conn, caller, person_id)?;
        }
        let people = people_map(&conn, &caller.tenant)?;
        let gifts = query_rows(
            &conn,
            &format!(
                "SELECT {GIFT_COLS} FROM gift_ideas WHERE tenant = ?1 AND (?2 IS NULL OR person_id = ?2) AND (?3 IS NULL OR status = ?3)
                 ORDER BY updated_at DESC, id"
            ),
            params![caller.tenant, person_id, status.map(GiftStatus::as_str)],
            gift_from_row,
        )?;
        let mut out: Vec<GiftIdea> = gifts
            .iter()
            .filter_map(|gift| {
                let person = people.get(&gift.person_id)?;
                authz::gift_visible(caller, gift, person).then(|| gift_view(caller, gift, person))
            })
            .collect();
        out.truncate(MAX_LIST_ROWS);
        Ok(out)
    }

    pub async fn create_gift(&self, caller: &Caller, input: NewGift) -> FamilyResult<GiftIdea> {
        if input.person_id.trim().is_empty() {
            return Err(ValidationError::Required { field: "personId" }.into());
        }
        let idea = v::required_text("idea", &input.idea, v::MAX_IDEA)?;
        let url = v::url(input.url.as_deref())?;
        let budget_minor = v::budget_minor(input.budget_minor)?;
        let currency = v::optional_currency(input.currency.as_deref())?;
        let occasion = v::optional_text("occasion", input.occasion.as_deref(), v::MAX_OCCASION)?;
        let visibility = authz::new_row_visibility(caller, input.visibility);
        let id = new_id();
        let now = now_ms();
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let person = visible_person(&tx, caller, input.person_id.trim())?;
        authz::gift_can_create(caller, &person)?;
        let currency = match (budget_minor, currency) {
            (Some(_), None) => Some(settings_or_default(&tx, &caller.tenant)?.currency),
            (_, currency) => currency,
        };
        tx.execute(
            "INSERT INTO gift_ideas (id, tenant, person_id, idea, url, budget_minor, budget_currency, status, occasion, visibility, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ryu_seal('gift_ideas.idea', ?1, ?4), ryu_seal('gift_ideas.url', ?1, ?5), ?6, ?7, ?8, ryu_seal('gift_ideas.occasion', ?1, ?9), ?10, ?11, ?12, ?12)",
            params![
                id,
                caller.tenant,
                person.id,
                idea,
                url,
                budget_minor,
                currency,
                input.status.unwrap_or(GiftStatus::Idea).as_str(),
                occasion,
                visibility.as_str(),
                caller.actor(),
                now
            ],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "gift_ideas.create",
            "gift_ideas",
            &id,
        )?;
        let gift = require(get_gift(&tx, &caller.tenant, &id)?)?;
        tx.commit()?;
        Ok(gift_view(caller, &gift, &person))
    }

    pub async fn update_gift(
        &self,
        caller: &Caller,
        id: &str,
        patch: GiftPatch,
    ) -> FamilyResult<GiftIdea> {
        if patch.idea.is_none()
            && patch.url.is_none()
            && patch.budget_minor.is_none()
            && patch.currency.is_none()
            && patch.status.is_none()
            && patch.occasion.is_none()
            && patch.visibility.is_none()
        {
            return Err(ValidationError::EmptyPatch.into());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (gift, person) = visible_gift(&tx, caller, id)?;
        if !authz::gift_editable(caller, &gift, &person) {
            return Err(FamilyError::forbidden());
        }
        let idea = match patch.idea.as_deref() {
            Some(value) => v::required_text("idea", value, v::MAX_IDEA)?,
            None => gift.idea.clone(),
        };
        let url = match patch.url {
            Some(value) => v::url(value.as_deref())?,
            None => gift.url.clone(),
        };
        let budget_minor = match patch.budget_minor {
            Some(value) => v::budget_minor(value)?,
            None => gift.budget_minor,
        };
        let currency = match patch.currency {
            Some(value) => v::optional_currency(value.as_deref())?,
            None => gift.budget_currency.clone(),
        };
        let occasion = match patch.occasion {
            Some(value) => v::optional_text("occasion", value.as_deref(), v::MAX_OCCASION)?,
            None => gift.occasion.clone(),
        };
        let visibility =
            authz::patched_visibility(caller, gift.visibility, &gift.created_by, patch.visibility)?;
        tx.execute(
            "UPDATE gift_ideas SET idea = ryu_seal('gift_ideas.idea', ?2, ?3), url = ryu_seal('gift_ideas.url', ?2, ?4), budget_minor = ?5, budget_currency = ?6, status = ?7, occasion = ryu_seal('gift_ideas.occasion', ?2, ?8),
             visibility = ?9, updated_at = ?10 WHERE tenant = ?1 AND id = ?2",
            params![
                caller.tenant,
                id,
                idea,
                url,
                budget_minor,
                currency,
                patch.status.unwrap_or(gift.status).as_str(),
                occasion,
                visibility.as_str(),
                now_ms()
            ],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "gift_ideas.update",
            "gift_ideas",
            id,
        )?;
        let gift = require(get_gift(&tx, &caller.tenant, id)?)?;
        tx.commit()?;
        Ok(gift_view(caller, &gift, &person))
    }

    pub async fn delete_gift(&self, caller: &Caller, id: &str) -> FamilyResult<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let (gift, person) = visible_gift(&tx, caller, id)?;
        if !authz::gift_editable(caller, &gift, &person) {
            return Err(FamilyError::forbidden());
        }
        tx.execute(
            "DELETE FROM gift_ideas WHERE tenant = ?1 AND id = ?2",
            params![caller.tenant, id],
        )?;
        write_audit(
            &tx,
            &caller.tenant,
            caller.actor(),
            "gift_ideas.delete",
            "gift_ideas",
            id,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// What a gift-giver may use: family-visible preferences, the wishlist, past
    /// gifts and the budget. Never data hidden from the caller: the recipient and
    /// the shared lane get the restricted view (every family wishlist item with
    /// nothing that reveals claims, and no past gifts).
    pub async fn gift_context(
        &self,
        caller: &Caller,
        query: GiftContextQuery,
    ) -> FamilyResult<GiftContext> {
        self.gift_context_at(caller, query, Utc::now()).await
    }

    pub async fn gift_context_at(
        &self,
        caller: &Caller,
        query: GiftContextQuery,
        now: DateTime<Utc>,
    ) -> FamilyResult<GiftContext> {
        let person_id = query
            .person_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ValidationError::Required { field: "personId" })?;
        let occasion = v::optional_text("occasion", query.occasion.as_deref(), v::MAX_OCCASION)?;
        let budget_minor = v::budget_minor(query.budget_minor)?;
        let currency = v::optional_currency(query.currency.as_deref())?;
        let conn = self.conn.lock().await;
        let person = visible_person(&conn, caller, person_id)?;
        let settings = settings_or_default(&conn, &caller.tenant)?;
        let restricted = authz::gift_context_restricted(caller, &person);

        let mut next_dates: Vec<(i64, GiftContextDate)> = query_rows(
            &conn,
            &format!("SELECT {DATE_COLS} FROM dates WHERE tenant = ?1 AND person_id = ?2"),
            params![caller.tenant, person.id],
            date_from_row,
        )?
        .iter()
        .filter(|date| authz::date_visible(caller, date, &person))
        .map(|date| date_view(caller, date, &person, &settings.tz, now))
        .filter(|date| date.days_away < v::MAX_DAYS)
        .map(|date| {
            (
                date.days_away,
                GiftContextDate {
                    kind: date.kind,
                    label: date.label,
                    on: date.next_occurrence,
                    turning: date.turning,
                },
            )
        })
        .collect();
        next_dates.sort_by_key(|(days_away, _)| *days_away);

        // Only family-visible rows, the same for every member. The shared lane
        // (possibly a group chat) never gets a sensitive row, even a shared one.
        let preferences = query_rows(
            &conn,
            &format!("SELECT {PREF_COLS} FROM preferences WHERE tenant = ?1 AND person_id = ?2 ORDER BY key COLLATE NOCASE"),
            params![caller.tenant, person.id],
            pref_from_row,
        )?
        .into_iter()
        .filter(|pref| !authz::effectively_private(pref.visibility, &person))
        .filter(|pref| !(caller.is_shared() && pref.sensitive))
        .map(|pref| KeyValue {
            key: pref.key,
            value: pref.value,
        })
        .collect();

        let wishlist = query_rows(
            &conn,
            &format!("SELECT {WISH_COLS} FROM wishlist_items WHERE tenant = ?1 AND person_id = ?2 ORDER BY priority, created_at"),
            params![caller.tenant, person.id],
            wish_from_row,
        )?
        .into_iter()
        .filter(|wish| !authz::effectively_private(wish.visibility, &person))
        .filter(|wish| restricted || wish.claimed_by.is_none())
        .map(|wish| GiftContextWish {
            id: wish.id,
            title: wish.title,
            url: wish.url,
            price_hint: wish.price_hint,
            priority: wish.priority,
        })
        .collect();

        let past_gifts = if restricted {
            Vec::new()
        } else {
            let mut given = query_rows(
                &conn,
                &format!("SELECT {GIFT_COLS} FROM gift_ideas WHERE tenant = ?1 AND person_id = ?2 AND status = 'given' ORDER BY updated_at DESC"),
                params![caller.tenant, person.id],
                gift_from_row,
            )?;
            given.retain(|gift| authz::gift_visible(caller, gift, &person));
            given
                .into_iter()
                .take(20)
                .map(|gift| PastGift {
                    idea: gift.idea,
                    occasion: gift.occasion,
                })
                .collect()
        };

        let currency = match (budget_minor, currency) {
            (_, Some(currency)) => Some(currency),
            (Some(_), None) => Some(settings.currency.clone()),
            (None, None) => None,
        };
        Ok(GiftContext {
            person: GiftContextPerson {
                id: person.id.clone(),
                display_name: person.display_name.clone(),
            },
            occasion,
            budget: GiftBudget {
                minor: budget_minor,
                currency,
            },
            next_dates: next_dates.into_iter().map(|(_, date)| date).collect(),
            preferences,
            wishlist,
            past_gifts,
            restricted,
        })
    }

    // ── upcoming ──────────────────────────────────────────────────────────────

    pub async fn upcoming(
        &self,
        caller: &Caller,
        days: Option<i64>,
    ) -> FamilyResult<UpcomingResponse> {
        self.upcoming_at(caller, days, Utc::now()).await
    }

    /// The family agenda: visible dates whose next occurrence falls in
    /// `[today, today + days)`, plus visible open items due in that window or
    /// overdue. Sorted by day, dates before items; at most 200 entries. Dates
    /// are kept first and only the latest [`MAX_OVERDUE_ENTRIES`] overdue items
    /// are included, so old items never crowd out what is coming up.
    pub async fn upcoming_at(
        &self,
        caller: &Caller,
        days: Option<i64>,
        now: DateTime<Utc>,
    ) -> FamilyResult<UpcomingResponse> {
        let days = v::days(days)?;
        let conn = self.conn.lock().await;
        let settings = settings_or_default(&conn, &caller.tenant)?;
        let tz = resolve_tz(&settings.tz);
        let today = local_today(tz, now);
        let end = today + TimeDelta::days(days);
        let today_key = today.format("%Y-%m-%d").to_string();
        let people = people_map(&conn, &caller.tenant)?;
        let mut entries = Vec::new();
        for date in query_rows(
            &conn,
            &format!("SELECT {DATE_COLS} FROM dates WHERE tenant = ?1"),
            params![caller.tenant],
            date_from_row,
        )? {
            let Some(person) = people.get(&date.person_id) else {
                continue;
            };
            if !authz::date_visible(caller, &date, person) {
                continue;
            }
            let view = date_view(caller, &date, person, &settings.tz, now);
            if !(0..days).contains(&view.days_away) {
                continue;
            }
            entries.push(UpcomingEntry::Date(DateEntry {
                on: view.next_occurrence,
                days_away: view.days_away,
                date_id: date.id.clone(),
                person_id: person.id.clone(),
                person_name: person.display_name.clone(),
                kind: date.kind,
                label: date.label.clone(),
                turning: view.turning,
            }));
        }
        let lists: HashMap<String, ListRow> = query_rows(
            &conn,
            &format!("SELECT {LIST_COLS} FROM lists WHERE tenant = ?1"),
            params![caller.tenant],
            list_from_row,
        )?
        .into_iter()
        .filter(|list| authz::list_visible(caller, list))
        .map(|list| (list.id.clone(), list))
        .collect();
        let mut items = Vec::new();
        for item in query_rows(
            &conn,
            &format!("SELECT {ITEM_COLS} FROM list_items WHERE tenant = ?1 AND done = 0 AND due_on IS NOT NULL AND due_on < ?2"),
            params![caller.tenant, end.format("%Y-%m-%d").to_string()],
            item_from_row,
        )? {
            let Some(list) = lists.get(&item.list_id) else {
                continue;
            };
            let Some(due) = item
                .due_on
                .as_deref()
                .and_then(|due| v::parse_date("dueOn", due).ok())
            else {
                continue;
            };
            let days_away = (due - today).num_days();
            items.push(UpcomingEntry::Item(ItemEntry {
                on: due.format("%Y-%m-%d").to_string(),
                days_away,
                item_id: item.id.clone(),
                list_id: list.id.clone(),
                list_name: list.name.clone(),
                text: item.text.clone(),
                assignee_person_id: item.assignee_person_id.clone(),
                overdue: days_away < 0,
            }));
        }
        let by_day = |left: &UpcomingEntry, right: &UpcomingEntry| {
            left.on()
                .cmp(right.on())
                .then_with(|| left.rank().cmp(&right.rank()))
        };
        entries.sort_by(by_day);
        entries.truncate(MAX_UPCOMING_ENTRIES);
        let (mut overdue, mut current): (Vec<_>, Vec<_>) = items
            .into_iter()
            .partition(|entry| entry.on() < today_key.as_str());
        let room = MAX_UPCOMING_ENTRIES - entries.len();
        overdue.sort_by(|left, right| by_day(right, left));
        overdue.truncate(MAX_OVERDUE_ENTRIES.min(room));
        current.sort_by(by_day);
        current.truncate(room - overdue.len());
        entries.extend(overdue);
        entries.extend(current);
        entries.sort_by(by_day);
        Ok(UpcomingResponse {
            today: today.format("%Y-%m-%d").to_string(),
            tz: settings.tz,
            days,
            entries,
        })
    }

    // ── reminder sweep support ────────────────────────────────────────────────

    pub async fn reminder_tenants(&self) -> FamilyResult<Vec<SettingsRow>> {
        let conn = self.conn.lock().await;
        query_rows(
            &conn,
            &format!("SELECT {SETTINGS_COLS} FROM settings ORDER BY tenant"),
            [],
            settings_from_row,
        )
    }

    pub async fn reminder_plan(&self, tenant: &str) -> FamilyResult<ReminderPlan> {
        let conn = self.conn.lock().await;
        let people = people_map(&conn, tenant)?;
        let dates = query_rows(
            &conn,
            &format!("SELECT {DATE_COLS} FROM dates WHERE tenant = ?1 ORDER BY id"),
            params![tenant],
            date_from_row,
        )?
        .into_iter()
        .filter_map(|date| {
            let person = people.get(&date.person_id)?.clone();
            Some((date, person))
        })
        .collect();
        Ok(ReminderPlan {
            dates,
            members: members(&conn, tenant)?,
        })
    }

    /// Claim one reminder as `pending`. `true` only for the caller whose write
    /// changed a row, so two racing sweeps can never both deliver. A `pending`
    /// claim older than [`CLAIM_LEASE_MS`] belongs to a sweep that died before
    /// delivery finished (restart, upgrade, a failed write), so it is taken over;
    /// the sweep computes a key only on its due local day, so a takeover happens
    /// on that day only. `sent` and `failed` claims are final.
    pub async fn claim_reminder(
        &self,
        tenant: &str,
        key: &ClaimKey,
        now_ms: i64,
    ) -> FamilyResult<bool> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "INSERT INTO reminder_claims (tenant, date_id, offset_days, occurrence_year, recipient, status, claimed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)
             ON CONFLICT (date_id, offset_days, occurrence_year, recipient) DO UPDATE SET claimed_at = excluded.claimed_at
             WHERE reminder_claims.status = 'pending' AND reminder_claims.claimed_at <= excluded.claimed_at - ?7",
            params![tenant, key.date_id, key.offset_days, key.occurrence_year, key.recipient, now_ms, CLAIM_LEASE_MS],
        )?;
        Ok(changed == 1)
    }

    pub async fn mark_claim_sent(
        &self,
        key: &ClaimKey,
        notification_id: Option<&str>,
    ) -> FamilyResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE reminder_claims SET status = 'sent', notification_id = ?5
             WHERE date_id = ?1 AND offset_days = ?2 AND occurrence_year = ?3 AND recipient = ?4",
            params![
                key.date_id,
                key.offset_days,
                key.occurrence_year,
                key.recipient,
                notification_id
            ],
        )?;
        Ok(())
    }

    pub async fn mark_claim_failed(&self, key: &ClaimKey) -> FamilyResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE reminder_claims SET status = 'failed'
             WHERE date_id = ?1 AND offset_days = ?2 AND occurrence_year = ?3 AND recipient = ?4",
            params![
                key.date_id,
                key.offset_days,
                key.occurrence_year,
                key.recipient
            ],
        )?;
        Ok(())
    }

    pub async fn release_claim(&self, key: &ClaimKey) -> FamilyResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM reminder_claims WHERE date_id = ?1 AND offset_days = ?2 AND occurrence_year = ?3 AND recipient = ?4",
            params![key.date_id, key.offset_days, key.occurrence_year, key.recipient],
        )?;
        Ok(())
    }

    /// The status of one claim (`pending` / `sent` / `failed`), for tests and
    /// diagnostics.
    pub async fn claim_status(&self, key: &ClaimKey) -> FamilyResult<Option<String>> {
        let conn = self.conn.lock().await;
        Ok(conn
            .query_row(
                "SELECT status FROM reminder_claims WHERE date_id = ?1 AND offset_days = ?2 AND occurrence_year = ?3 AND recipient = ?4",
                params![key.date_id, key.offset_days, key.occurrence_year, key.recipient],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// How many open gift ideas (`idea` or `bought`) for `person` the recipient
    /// may see. Only this count ever reaches a reminder.
    pub async fn gift_ideas_visible_to(
        &self,
        tenant: &str,
        person: &PersonRow,
        recipient: &str,
    ) -> FamilyResult<i64> {
        let conn = self.conn.lock().await;
        let viewer = Caller::member(tenant, recipient);
        let count = query_rows(
            &conn,
            &format!("SELECT {GIFT_COLS} FROM gift_ideas WHERE tenant = ?1 AND person_id = ?2 AND status IN ('idea','bought')"),
            params![tenant, person.id],
            gift_from_row,
        )?
        .iter()
        .filter(|gift| authz::gift_visible(&viewer, gift, person))
        .count();
        Ok(i64::try_from(count).unwrap_or(i64::MAX))
    }

    /// The append-only audit trail of one tenant. There is no HTTP or MCP read
    /// route (no role header reaches the sidecar); this exists for tests.
    pub async fn audit_rows(&self, tenant: &str) -> FamilyResult<Vec<AuditRow>> {
        let conn = self.conn.lock().await;
        query_rows(
            &conn,
            "SELECT actor, action, table_name, row_id FROM audit WHERE tenant = ?1 ORDER BY seq",
            params![tenant],
            |row| {
                Ok(AuditRow {
                    actor: row.get(0)?,
                    action: row.get(1)?,
                    table_name: row.get(2)?,
                    row_id: row.get(3)?,
                })
            },
        )
    }

    /// Run raw SQL against the store (tests prove the audit triggers hold).
    pub async fn execute_raw(&self, sql: &str) -> FamilyResult<usize> {
        let conn = self.conn.lock().await;
        Ok(conn.execute(sql, [])?)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// The monthly anchor of a due date: its day of the month.
fn anchor_day(due_on: Option<&str>) -> Option<u32> {
    due_on
        .and_then(|due| v::parse_date("dueOn", due).ok())
        .map(|due| due.day())
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Keys over a random root (in-memory test stores).
fn ephemeral_keys() -> anyhow::Result<PluginKeys> {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut root = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut root);
    Ok(PluginKeys::sealing(
        ryu_crypto::PluginCipher::from_subkey_b64(
            crate::state::PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode(root),
        )?,
        KeySource::Delivered,
    ))
}

fn require<T>(row: Option<T>) -> FamilyResult<T> {
    row.ok_or_else(|| FamilyError::Internal(anyhow::anyhow!("written row disappeared")))
}

fn query_rows<T, P, F>(conn: &Connection, sql: &str, params: P, map: F) -> FamilyResult<Vec<T>>
where
    P: rusqlite::Params,
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params, map)?;
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(row) => out.push(row),
            // A sealed value that does not open (already counted): one bad row
            // never hides the rest of a list.
            Err(error) if is_unreadable(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(out)
}

fn write_audit(
    conn: &Connection,
    tenant: &str,
    actor: &str,
    action: &str,
    table: &str,
    row_id: &str,
) -> FamilyResult<()> {
    conn.execute(
        "INSERT INTO audit (tenant, actor, action, table_name, row_id, at, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?2)",
        params![tenant, actor, action, table, row_id, now_ms()],
    )?;
    Ok(())
}

fn touch_list(conn: &Connection, tenant: &str, list_id: &str, now: i64) -> FamilyResult<()> {
    conn.execute(
        "UPDATE lists SET updated_at = ?3 WHERE tenant = ?1 AND id = ?2",
        params![tenant, list_id, now],
    )?;
    Ok(())
}

fn conversion_error(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, message.into())
}

fn enum_col<T>(row: &Row<'_>, index: usize, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    parse(&text).ok_or_else(|| conversion_error(index, format!("unexpected value {text:?}")))
}

fn json_col<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| conversion_error(index, error.to_string()))
}

fn person_from_row(row: &Row<'_>) -> rusqlite::Result<PersonRow> {
    Ok(PersonRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        display_name: opened(row, 2)?,
        relationship: opened_opt(row, 3)?,
        member_user_id: row.get(4)?,
        visibility: enum_col(row, 5, Visibility::parse)?,
        created_by: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn date_from_row(row: &Row<'_>) -> rusqlite::Result<DateRow> {
    Ok(DateRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        person_id: row.get(2)?,
        kind: enum_col(row, 3, DateKind::parse)?,
        label: opened_opt(row, 4)?,
        month: row.get(5)?,
        day: row.get(6)?,
        year: row.get(7)?,
        remind_offsets_days: json_col(row, 8)?,
        tz: row.get(9)?,
        visibility: enum_col(row, 10, Visibility::parse)?,
        created_by: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn list_from_row(row: &Row<'_>) -> rusqlite::Result<ListRow> {
    Ok(ListRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        kind: enum_col(row, 2, ListKind::parse)?,
        name: opened(row, 3)?,
        visibility: enum_col(row, 4, Visibility::parse)?,
        created_by: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<ItemRow> {
    Ok(ItemRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        list_id: row.get(2)?,
        text: opened(row, 3)?,
        qty: opened_opt(row, 4)?,
        done: row.get(5)?,
        done_at: row.get(6)?,
        done_by: row.get(7)?,
        assignee_person_id: row.get(8)?,
        due_on: row.get(9)?,
        recurrence: enum_col(row, 10, Recurrence::parse)?,
        points: row.get(11)?,
        next_item_id: row.get(12)?,
        visibility: enum_col(row, 13, Visibility::parse)?,
        created_by: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn pref_from_row(row: &Row<'_>) -> rusqlite::Result<PrefRow> {
    Ok(PrefRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        person_id: row.get(2)?,
        key: opened(row, 3)?,
        value: opened(row, 4)?,
        sensitive: row.get(5)?,
        visibility: enum_col(row, 6, Visibility::parse)?,
        created_by: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        subject_user_id: row.get(10)?,
    })
}

fn wish_from_row(row: &Row<'_>) -> rusqlite::Result<WishRow> {
    Ok(WishRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        person_id: row.get(2)?,
        title: opened(row, 3)?,
        url: opened_opt(row, 4)?,
        price_hint: opened_opt(row, 5)?,
        priority: row.get(6)?,
        claimed_by: row.get(7)?,
        claimed_at: row.get(8)?,
        visibility: enum_col(row, 9, Visibility::parse)?,
        created_by: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        subject_user_id: row.get(13)?,
    })
}

fn gift_from_row(row: &Row<'_>) -> rusqlite::Result<GiftRow> {
    Ok(GiftRow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        person_id: row.get(2)?,
        idea: opened(row, 3)?,
        url: opened_opt(row, 4)?,
        budget_minor: row.get(5)?,
        budget_currency: row.get(6)?,
        status: enum_col(row, 7, GiftStatus::parse)?,
        occasion: opened_opt(row, 8)?,
        visibility: enum_col(row, 9, Visibility::parse)?,
        created_by: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn settings_from_row(row: &Row<'_>) -> rusqlite::Result<SettingsRow> {
    Ok(SettingsRow {
        tenant: row.get(0)?,
        tz: row.get(1)?,
        reminder_hour: row.get(2)?,
        default_offsets_days: json_col(row, 3)?,
        currency: row.get(4)?,
        agent_access: row.get(5)?,
        muted_user_ids: json_col(row, 6)?,
        created_by: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        updated_by: row.get(10)?,
    })
}

fn get_one<T>(
    conn: &Connection,
    cols: &str,
    table: &str,
    tenant: &str,
    id: &str,
    map: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> FamilyResult<Option<T>> {
    Ok(conn
        .query_row(
            &format!("SELECT {cols} FROM {table} WHERE tenant = ?1 AND id = ?2"),
            params![tenant, id],
            map,
        )
        .optional()?)
}

fn get_person(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<PersonRow>> {
    get_one(conn, PERSON_COLS, "people", tenant, id, person_from_row)
}

fn get_date(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<DateRow>> {
    get_one(conn, DATE_COLS, "dates", tenant, id, date_from_row)
}

fn get_list(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<ListRow>> {
    get_one(conn, LIST_COLS, "lists", tenant, id, list_from_row)
}

fn get_item(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<ItemRow>> {
    get_one(conn, ITEM_COLS, "list_items", tenant, id, item_from_row)
}

fn get_pref(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<PrefRow>> {
    get_one(conn, PREF_COLS, "preferences", tenant, id, pref_from_row)
}

fn get_wish(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<WishRow>> {
    get_one(conn, WISH_COLS, "wishlist_items", tenant, id, wish_from_row)
}

fn get_gift(conn: &Connection, tenant: &str, id: &str) -> FamilyResult<Option<GiftRow>> {
    get_one(conn, GIFT_COLS, "gift_ideas", tenant, id, gift_from_row)
}

fn people_map(conn: &Connection, tenant: &str) -> FamilyResult<HashMap<String, PersonRow>> {
    Ok(query_rows(
        conn,
        &format!("SELECT {PERSON_COLS} FROM people WHERE tenant = ?1"),
        params![tenant],
        person_from_row,
    )?
    .into_iter()
    .map(|person| (person.id.clone(), person))
    .collect())
}

/// A person the caller can see, or 404.
fn visible_person(conn: &Connection, caller: &Caller, id: &str) -> FamilyResult<PersonRow> {
    get_person(conn, &caller.tenant, id)?
        .filter(|person| authz::person_visible(caller, person))
        .ok_or(FamilyError::NotFound)
}

fn visible_date(
    conn: &Connection,
    caller: &Caller,
    id: &str,
) -> FamilyResult<(DateRow, PersonRow)> {
    let date = get_date(conn, &caller.tenant, id)?.ok_or(FamilyError::NotFound)?;
    let person = get_person(conn, &caller.tenant, &date.person_id)?.ok_or(FamilyError::NotFound)?;
    if authz::date_visible(caller, &date, &person) {
        Ok((date, person))
    } else {
        Err(FamilyError::NotFound)
    }
}

fn visible_list(conn: &Connection, caller: &Caller, id: &str) -> FamilyResult<ListRow> {
    get_list(conn, &caller.tenant, id)?
        .filter(|list| authz::list_visible(caller, list))
        .ok_or(FamilyError::NotFound)
}

fn visible_item(conn: &Connection, caller: &Caller, id: &str) -> FamilyResult<(ItemRow, ListRow)> {
    let item = get_item(conn, &caller.tenant, id)?.ok_or(FamilyError::NotFound)?;
    let list = visible_list(conn, caller, &item.list_id)?;
    Ok((item, list))
}

fn visible_pref(
    conn: &Connection,
    caller: &Caller,
    id: &str,
) -> FamilyResult<(PrefRow, PersonRow)> {
    let pref = get_pref(conn, &caller.tenant, id)?.ok_or(FamilyError::NotFound)?;
    let person = get_person(conn, &caller.tenant, &pref.person_id)?.ok_or(FamilyError::NotFound)?;
    if authz::pref_visible(caller, &pref, &person) {
        Ok((pref, person))
    } else {
        Err(FamilyError::NotFound)
    }
}

fn visible_wish(
    conn: &Connection,
    caller: &Caller,
    id: &str,
) -> FamilyResult<(WishRow, PersonRow)> {
    let wish = get_wish(conn, &caller.tenant, id)?.ok_or(FamilyError::NotFound)?;
    let person = get_person(conn, &caller.tenant, &wish.person_id)?.ok_or(FamilyError::NotFound)?;
    if authz::wish_visible(caller, &wish, &person) {
        Ok((wish, person))
    } else {
        Err(FamilyError::NotFound)
    }
}

fn visible_gift(
    conn: &Connection,
    caller: &Caller,
    id: &str,
) -> FamilyResult<(GiftRow, PersonRow)> {
    let gift = get_gift(conn, &caller.tenant, id)?.ok_or(FamilyError::NotFound)?;
    let person = get_person(conn, &caller.tenant, &gift.person_id)?.ok_or(FamilyError::NotFound)?;
    if authz::gift_visible(caller, &gift, &person) {
        Ok((gift, person))
    } else {
        Err(FamilyError::NotFound)
    }
}

/// An optional assignee must name a person the caller can see.
fn assignee_id(
    conn: &Connection,
    caller: &Caller,
    id: Option<&str>,
) -> FamilyResult<Option<String>> {
    let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    match visible_person(conn, caller, id) {
        Ok(person) => Ok(Some(person.id)),
        Err(FamilyError::NotFound) => Err(ValidationError::UnknownPerson {
            field: "assigneePersonId",
        }
        .into()),
        Err(error) => Err(error),
    }
}

fn list_counts(conn: &Connection, tenant: &str) -> FamilyResult<HashMap<String, (i64, i64)>> {
    Ok(query_rows(
        conn,
        "SELECT list_id, SUM(CASE WHEN done = 0 THEN 1 ELSE 0 END), SUM(CASE WHEN done = 1 THEN 1 ELSE 0 END)
         FROM list_items WHERE tenant = ?1 GROUP BY list_id",
        params![tenant],
        |row| Ok((row.get::<_, String>(0)?, (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))),
    )?
    .into_iter()
    .collect())
}

fn linked_person_id(conn: &Connection, tenant: &str, user: &str) -> FamilyResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM people WHERE tenant = ?1 AND member_user_id = ?2",
            params![tenant, user],
            |row| row.get(0),
        )
        .optional()?)
}

fn default_settings(tenant: &str) -> SettingsRow {
    SettingsRow {
        tenant: tenant.to_owned(),
        tz: "UTC".to_owned(),
        reminder_hour: 9,
        default_offsets_days: vec![7, 1, 0],
        currency: "USD".to_owned(),
        agent_access: false,
        muted_user_ids: Vec::new(),
        created_by: authz::SYSTEM_ACTOR.to_owned(),
        created_at: 0,
        updated_at: 0,
        updated_by: authz::SYSTEM_ACTOR.to_owned(),
    }
}

fn settings_or_default(conn: &Connection, tenant: &str) -> FamilyResult<SettingsRow> {
    Ok(conn
        .query_row(
            &format!("SELECT {SETTINGS_COLS} FROM settings WHERE tenant = ?1"),
            params![tenant],
            settings_from_row,
        )
        .optional()?
        .unwrap_or_else(|| default_settings(tenant)))
}

fn settings_response(conn: &Connection, caller: &Caller) -> FamilyResult<SettingsResponse> {
    let settings = settings_or_default(conn, &caller.tenant)?;
    let user = caller.actor().to_owned();
    let person_id = match caller.user_id() {
        Some(user) => linked_person_id(conn, &caller.tenant, user)?,
        None => None,
    };
    Ok(SettingsResponse {
        me: Me {
            muted: settings.muted_user_ids.contains(&user),
            person_id,
            user_id: user,
            tenant_kind: authz::tenant_kind(&caller.tenant).to_owned(),
        },
        settings: Settings {
            tz: settings.tz,
            reminder_hour: settings.reminder_hour,
            default_remind_offsets_days: settings.default_offsets_days,
            currency: settings.currency,
            agent_access: settings.agent_access,
            updated_at: settings.updated_at,
        },
    })
}

/// "Members who have the app": the kernel exposes no roster, so the set is
/// derived here. `local` and `user:<id>` tenants have exactly one member. An
/// org tenant's members are the linked family-visible people plus every user
/// principal who has written to the hub over HTTP. Core still rejects
/// non-members on delivery.
fn members(conn: &Connection, tenant: &str) -> FamilyResult<BTreeSet<String>> {
    if tenant == authz::LOCAL_USER {
        return Ok(BTreeSet::from([authz::LOCAL_USER.to_owned()]));
    }
    if let Some(user) = tenant.strip_prefix("user:") {
        return Ok(BTreeSet::from([user.to_owned()]));
    }
    let mut set: BTreeSet<String> = query_rows(
        conn,
        "SELECT DISTINCT member_user_id FROM people WHERE tenant = ?1 AND member_user_id IS NOT NULL AND visibility = 'family'",
        params![tenant],
        |row| row.get::<_, String>(0),
    )?
    .into_iter()
    .collect();
    set.extend(query_rows(
        conn,
        "SELECT DISTINCT actor FROM audit WHERE tenant = ?1",
        params![tenant],
        |row| row.get::<_, String>(0),
    )?);
    set.retain(|member| authz::is_user_principal(member) && member != authz::LOCAL_USER);
    Ok(set)
}

// ── views (flags come only from authz) ────────────────────────────────────────

fn base(
    id: &str,
    visibility: Visibility,
    created_by: &str,
    created_at: i64,
    updated_at: i64,
    can_edit: bool,
) -> RowBase {
    RowBase {
        id: id.to_owned(),
        visibility,
        created_by: created_by.to_owned(),
        created_at,
        updated_at,
        can_edit,
    }
}

fn person_view(caller: &Caller, person: &PersonRow) -> Person {
    Person {
        base: base(
            &person.id,
            person.visibility,
            &person.created_by,
            person.created_at,
            person.updated_at,
            authz::person_editable(caller, person),
        ),
        display_name: person.display_name.clone(),
        relationship: person.relationship.clone(),
        member_linked: person.member_user_id.is_some(),
        is_me: caller.user_id().is_some() && person.member_user_id.as_deref() == caller.user_id(),
    }
}

fn date_view(
    caller: &Caller,
    date: &DateRow,
    person: &PersonRow,
    tenant_tz: &str,
    now: DateTime<Utc>,
) -> FamilyDate {
    let tz = resolve_tz(date.tz.as_deref().unwrap_or(tenant_tz));
    let today: NaiveDate = local_today(tz, now);
    let occurrence = next_occurrence(date.month, date.day, today);
    FamilyDate {
        base: base(
            &date.id,
            date.visibility,
            &date.created_by,
            date.created_at,
            date.updated_at,
            authz::date_editable(caller, date, person),
        ),
        person_id: date.person_id.clone(),
        kind: date.kind,
        label: date.label.clone(),
        month: date.month,
        day: date.day,
        year: date.year,
        remind_offsets_days: date.remind_offsets_days.clone(),
        tz: date.tz.clone(),
        next_occurrence: occurrence.format("%Y-%m-%d").to_string(),
        days_away: (occurrence - today).num_days(),
        turning: turning(date.kind, date.year, occurrence),
    }
}

fn list_view(caller: &Caller, list: &ListRow, (open, done): (i64, i64)) -> FamilyList {
    FamilyList {
        base: base(
            &list.id,
            list.visibility,
            &list.created_by,
            list.created_at,
            list.updated_at,
            authz::list_editable(caller, list),
        ),
        kind: list.kind,
        name: list.name.clone(),
        open_count: open,
        done_count: done,
    }
}

fn item_view(caller: &Caller, item: &ItemRow, list: &ListRow) -> ListItem {
    ListItem {
        base: base(
            &item.id,
            list.visibility,
            &item.created_by,
            item.created_at,
            item.updated_at,
            authz::list_editable(caller, list),
        ),
        list_id: item.list_id.clone(),
        text: item.text.clone(),
        qty: item.qty.clone(),
        done: item.done,
        done_at: item.done_at,
        done_by: item.done_by.clone(),
        assignee_person_id: item.assignee_person_id.clone(),
        due_on: item.due_on.clone(),
        recurrence: item.recurrence,
        points: item.points,
        next_item_id: item.next_item_id.clone(),
    }
}

fn pref_view(caller: &Caller, pref: &PrefRow, person: &PersonRow) -> Preference {
    Preference {
        base: base(
            &pref.id,
            pref.visibility,
            &pref.created_by,
            pref.created_at,
            pref.updated_at,
            authz::pref_editable(caller, pref, person),
        ),
        person_id: pref.person_id.clone(),
        key: pref.key.clone(),
        value: pref.value.clone(),
        sensitive: pref.sensitive,
        can_change_visibility: authz::pref_can_change_visibility(caller, pref, person),
    }
}

fn wish_view(caller: &Caller, wish: &WishRow, person: &PersonRow) -> WishlistItem {
    WishlistItem {
        base: base(
            &wish.id,
            wish.visibility,
            &wish.created_by,
            wish.created_at,
            wish.updated_at,
            authz::wish_editable(caller, wish, person),
        ),
        person_id: wish.person_id.clone(),
        title: wish.title.clone(),
        url: wish.url.clone(),
        price_hint: wish.price_hint.clone(),
        priority: wish.priority,
        can_claim: authz::wish_can_claim(caller, wish, person),
        claim: authz::wish_claim_view(caller, wish, person),
    }
}

fn gift_view(caller: &Caller, gift: &GiftRow, person: &PersonRow) -> GiftIdea {
    GiftIdea {
        base: base(
            &gift.id,
            gift.visibility,
            &gift.created_by,
            gift.created_at,
            gift.updated_at,
            authz::gift_editable(caller, gift, person),
        ),
        person_id: gift.person_id.clone(),
        idea: gift.idea.clone(),
        url: gift.url.clone(),
        budget_minor: gift.budget_minor,
        currency: gift.budget_currency.clone(),
        status: gift.status,
        occasion: gift.occasion.clone(),
    }
}

/// Members for tests of the recipient derivation.
#[doc(hidden)]
pub async fn members_of(store: &FamilyStore, tenant: &str) -> FamilyResult<BTreeSet<String>> {
    let conn = store.conn.lock().await;
    members(&conn, tenant)
}
