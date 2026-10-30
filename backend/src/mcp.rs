//! MCP lane: `ryu-family mcp` serves the `family` tools over stdio.
//!
//! Core forwards no verified caller identity into a stdio `tools/call`, so every
//! call here runs as [`Principal::Shared`](crate::authz::Principal::Shared) in
//! the one tenant that granted the family agent access (`settings.agent_access`).
//! That caller may be anyone, including the person a surprise is for, so these
//! tools never read or write gift ideas or wishlist claims, never touch private
//! rows or sensitive preferences, and return the conservative shared view. The
//! protocol handling is the expenses sidecar's.

use std::io::Write as _;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, BufReader};

use crate::{authz::Caller, models::*, state::AppState};
use ryu_sidecar_runtime::mcp_protocol::{
    discover_result, finalize_response, protocol_error, request_is_modern, PROTOCOL_VERSION,
};

const SHARED_NOTE: &str =
    "Gift ideas and wishlist claims live only in the Family app and are never returned here.";

fn describe(text: &str) -> String {
    format!("{text} {SHARED_NOTE}")
}

/// The tool table, in contract order. Names are bare; agents see `family.<name>`.
pub fn tools() -> Value {
    let list_kinds = json!([
        "grocery",
        "chores",
        "todo",
        "packing",
        "meal_plan",
        "custom"
    ]);
    let new_item = json!({
        "type": "object",
        "required": ["text"],
        "properties": {
            "text": { "type": "string" },
            "qty": { "type": "string" },
            "assigneePersonId": { "type": "string" },
            "dueOn": { "type": "string", "description": "Exact YYYY-MM-DD date." },
            "recurrence": { "type": "string", "enum": ["none", "daily", "weekly", "monthly"] },
            "points": { "type": "integer", "description": "Chore points, 0 to 1000. A tally, not money." }
        }
    });
    json!([
        {
            "name": "upcoming",
            "description": describe("List the family's upcoming important dates and due or overdue list items, soonest first, with the next occurrence and age where known."),
            "inputSchema": { "type": "object", "properties": {
                "days": { "type": "integer", "minimum": 1, "maximum": 366, "description": "Window in days (default 30)." }
            } }
        },
        {
            "name": "people_list",
            "description": describe("List the people in the family hub (members and relatives such as grandparents or young children) with their relationship."),
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "person_upsert",
            "description": describe("Add a family person, or rename one by id. Ask before guessing a name or relationship; rows written here are visible to the whole family."),
            "inputSchema": { "type": "object", "required": ["displayName"], "properties": {
                "id": { "type": "string", "description": "Omit to create a new person." },
                "displayName": { "type": "string" },
                "relationship": { "type": "string" }
            } }
        },
        {
            "name": "dates_list",
            "description": describe("List family-visible important dates (birthdays, anniversaries, custom dates), optionally for one person, with the next occurrence."),
            "inputSchema": { "type": "object", "properties": {
                "personId": { "type": "string" }
            } }
        },
        {
            "name": "date_upsert",
            "description": describe("Add an important date for a person, or update one by id. Feb 29 is observed on Mar 1 in non-leap years. Reminders go out on the lead days given."),
            "inputSchema": { "type": "object", "required": ["kind", "month", "day"], "properties": {
                "id": { "type": "string", "description": "Omit to create; then personId is required." },
                "personId": { "type": "string" },
                "kind": { "type": "string", "enum": ["birthday", "anniversary", "custom"] },
                "label": { "type": "string" },
                "month": { "type": "integer", "minimum": 1, "maximum": 12 },
                "day": { "type": "integer", "minimum": 1, "maximum": 31 },
                "year": { "type": "integer", "description": "Optional birth or wedding year." },
                "remindOffsetsDays": { "type": "array", "items": { "type": "integer" }, "description": "Up to 5 lead times in days (0 to 60). Default follows family settings." },
                "tz": { "type": "string", "description": "IANA time zone; omit to use the family's." }
            } }
        },
        {
            "name": "date_delete",
            "description": describe("Delete one family-visible important date by id. Only call this after the user explicitly asks to remove it."),
            "inputSchema": { "type": "object", "required": ["id"], "properties": { "id": { "type": "string" } } }
        },
        {
            "name": "lists_get",
            "description": describe("Without listId, list the family's shared lists with open and done counts. With listId, return that list and its items."),
            "inputSchema": { "type": "object", "properties": {
                "listId": { "type": "string" },
                "includeDone": { "type": "boolean" }
            } }
        },
        {
            "name": "list_create",
            "description": describe("Create a shared family list such as groceries, chores, a to-do list, packing, or a meal plan."),
            "inputSchema": { "type": "object", "required": ["name", "kind"], "properties": {
                "name": { "type": "string" },
                "kind": { "type": "string", "enum": list_kinds }
            } }
        },
        {
            "name": "list_add_items",
            "description": describe("Add 1 to 50 items to a shared list. Chores may carry an assignee, a due date, a recurrence, and points."),
            "inputSchema": { "type": "object", "required": ["listId", "items"], "properties": {
                "listId": { "type": "string" },
                "items": { "type": "array", "minItems": 1, "maxItems": 50, "items": new_item }
            } }
        },
        {
            "name": "list_check",
            "description": describe("Check or uncheck a list item. Checking a recurring chore schedules its next occurrence once."),
            "inputSchema": { "type": "object", "required": ["itemId", "done"], "properties": {
                "itemId": { "type": "string" },
                "done": { "type": "boolean" }
            } }
        },
        {
            "name": "list_assign",
            "description": describe("Assign a list item to a family person, or pass null to clear the assignee."),
            "inputSchema": { "type": "object", "required": ["itemId", "assigneePersonId"], "properties": {
                "itemId": { "type": "string" },
                "assigneePersonId": { "type": ["string", "null"] }
            } }
        },
        {
            "name": "list_remove_item",
            "description": describe("Remove one item from a shared list. Only call this after the user asks to remove that item."),
            "inputSchema": { "type": "object", "required": ["itemId"], "properties": { "itemId": { "type": "string" } } }
        },
        {
            "name": "chores_tally",
            "description": describe("Total chore points and completed counts per assignee, optionally for one list or since a date. Points are a simple tally, not money."),
            "inputSchema": { "type": "object", "properties": {
                "listId": { "type": "string" },
                "since": { "type": "string", "description": "Exact YYYY-MM-DD date." }
            } }
        },
        {
            "name": "prefs_get",
            "description": describe("Read a person's family-visible preferences such as sizes, favourite things, and dislikes. Private and sensitive details are not shown."),
            "inputSchema": { "type": "object", "required": ["personId"], "properties": { "personId": { "type": "string" } } }
        },
        {
            "name": "prefs_set",
            "description": describe("Save a non-sensitive, family-visible preference for a person, or update one by id. Never use this for health or other sensitive details."),
            "inputSchema": { "type": "object", "required": ["personId", "key", "value"], "properties": {
                "personId": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" },
                "id": { "type": "string" }
            } }
        },
        {
            "name": "wishlist_get",
            "description": describe("Read a person's family-visible wishlist with titles, links, price hints, and priority."),
            "inputSchema": { "type": "object", "required": ["personId"], "properties": { "personId": { "type": "string" } } }
        },
        {
            "name": "wishlist_add",
            "description": describe("Add an item to a person's family-visible wishlist. This saves an idea of what they want; it never buys anything."),
            "inputSchema": { "type": "object", "required": ["personId", "title"], "properties": {
                "personId": { "type": "string" },
                "title": { "type": "string" },
                "url": { "type": "string" },
                "priceHint": { "type": "string" },
                "priority": { "type": "integer", "enum": [1, 2, 3], "description": "1 high, 2 medium, 3 low." }
            } }
        },
        {
            "name": "gift_context",
            "description": describe("Gather what helps choose a gift for a person: upcoming dates, family-visible preferences, wishlist items, and the budget. Use it before searching for gift options."),
            "inputSchema": { "type": "object", "required": ["personId"], "properties": {
                "personId": { "type": "string" },
                "occasion": { "type": "string" },
                "budgetMinor": { "type": "integer", "description": "Positive integer minor units." },
                "currency": { "type": "string", "description": "Three-letter currency code." }
            } }
        }
    ])
}

/// Serve MCP on stdio. `data_dir` is the plugin data dir: when the sidecar
/// published a handoff there, every tool call runs inside it (it holds the
/// sealing key); otherwise this process uses the store it opened.
pub async fn serve(state: AppState, data_dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut legacy_initialized = false;
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            tracing::warn!("mcp: skipping invalid JSON frame");
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let modern = match request_is_modern(&request, method, legacy_initialized) {
            Ok(modern) => modern,
            Err(error) => {
                emit(&protocol_error(id, error));
                continue;
            }
        };
        legacy_initialized |= method == "initialize" && !modern;
        let response = match method {
            "initialize" if modern => error(id, -32601, "unknown method 'initialize'"),
            "server/discover" => ok(id, discover_result()),
            "initialize" => ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "family", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" if !modern => ok(id, json!({})),
            "ping" => error(id, -32601, "unknown method 'ping'"),
            "tools/list" => ok(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                ok(
                    id,
                    routed_call(&state, data_dir.as_deref(), name, &args).await,
                )
            }
            other => error(id, -32601, &format!("unknown method '{other}'")),
        };
        emit(&finalize_response(
            response,
            modern,
            method,
            "family",
            env!("CARGO_PKG_VERSION"),
        ));
    }
    Ok(())
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn tool_content(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn emit(frame: &Value) {
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{frame}").is_ok() {
        let _ = stdout.flush();
    }
}

/// Run one tool through the running sidecar when it published a handoff,
/// else on this process's store (refused once the store is sealed: this
/// process holds no key).
async fn routed_call(
    state: &AppState,
    data_dir: Option<&std::path::Path>,
    name: &str,
    args: &Value,
) -> Value {
    if let Some(dir) = data_dir {
        if let Some(client) = crate::sidecar_link::SidecarClient::connect(dir).await {
            return match client
                .call(
                    "POST",
                    crate::api::HANDOFF_MCP_ROUTE,
                    Some(&json!({ "name": name, "arguments": args })),
                )
                .await
            {
                Ok(result) => result,
                Err(error) => tool_content(
                    &json!({ "error": error.to_string(), "code": "unavailable" }),
                    true,
                ),
            };
        }
    }
    if let Err(error) = state.store.refresh_seal().await {
        tracing::warn!("family: could not re-check the store's sealing: {error:#}");
    }
    if state.store.seal_health()["state"] == "unhealthy" {
        return tool_content(
            &json!({
                "error": "The family data is encrypted and only the Family app holds its key; open Ryu and try again.",
                "code": "unavailable",
            }),
            true,
        );
    }
    call_result(state, name, args).await
}

/// A `tools/call` result: the tool's JSON, or `{error, code}` with `isError`.
pub async fn call_result(state: &AppState, name: &str, args: &Value) -> Value {
    match call(state, name, args).await {
        Ok(value) => tool_content(&value, false),
        Err(error) => {
            if let FamilyError::Internal(source) = &error {
                tracing::error!(error = %format!("{source:#}"), tool = name, "family tool failed");
            }
            tool_content(
                &json!({ "error": error.to_string(), "code": error.code() }),
                true,
            )
        }
    }
}

/// The shared-lane caller: the tenant that granted the family agent access.
pub async fn shared_caller(state: &AppState) -> FamilyResult<Caller> {
    state
        .store
        .agent_tenant()
        .await?
        .map(Caller::shared)
        .ok_or(FamilyError::AgentNotConnected)
}

fn args_as<T: serde::de::DeserializeOwned>(args: &Value) -> FamilyResult<T> {
    serde_json::from_value(args.clone())
        .map_err(|error| FamilyError::BadRequest(format!("invalid arguments: {error}")))
}

fn json_of<T: serde::Serialize>(value: &T) -> FamilyResult<Value> {
    Ok(serde_json::to_value(value)?)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaysArgs {
    days: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonUpsertArgs {
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    relationship: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonArgs {
    person_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DateUpsertArgs {
    id: Option<String>,
    #[serde(default)]
    person_id: Option<String>,
    kind: Option<DateKind>,
    label: Option<String>,
    month: Option<i64>,
    day: Option<i64>,
    year: Option<i64>,
    remind_offsets_days: Option<Vec<i64>>,
    tz: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdArgs {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListsGetArgs {
    list_id: Option<String>,
    include_done: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddItemsArgs {
    #[serde(default)]
    list_id: String,
    #[serde(default)]
    items: Vec<NewListItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckArgs {
    #[serde(default)]
    item_id: String,
    done: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignArgs {
    #[serde(default)]
    item_id: String,
    assignee_person_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemIdArgs {
    #[serde(default)]
    item_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TallyArgs {
    list_id: Option<String>,
    since: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrefsSetArgs {
    #[serde(default)]
    person_id: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WishAddArgs {
    #[serde(default)]
    person_id: String,
    #[serde(default)]
    title: String,
    url: Option<String>,
    price_hint: Option<String>,
    priority: Option<i64>,
}

fn required(field: &'static str, value: &str) -> FamilyResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(crate::validation::ValidationError::Required { field }.into())
    } else {
        Ok(trimmed.to_owned())
    }
}

fn present(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Run one tool as the shared caller. Inputs never carry identity: any
/// `tenant`, `userId` or similar argument is ignored.
pub async fn call(state: &AppState, name: &str, args: &Value) -> FamilyResult<Value> {
    let caller = shared_caller(state).await?;
    let store = &state.store;
    match name {
        "upcoming" => json_of(
            &store
                .upcoming(&caller, args_as::<DaysArgs>(args)?.days)
                .await?,
        ),
        "people_list" => Ok(json!({ "people": store.list_people(&caller).await? })),
        "person_upsert" => {
            let input: PersonUpsertArgs = args_as(args)?;
            match present(input.id) {
                Some(id) => {
                    let patch = PersonPatch {
                        display_name: input.display_name,
                        relationship: input.relationship.map(Some),
                        visibility: None,
                    };
                    json_of(&store.update_person(&caller, &id, patch).await?)
                }
                None => {
                    let new = NewPerson {
                        display_name: input.display_name.unwrap_or_default(),
                        relationship: input.relationship,
                        visibility: None,
                    };
                    json_of(&store.create_person(&caller, new).await?)
                }
            }
        }
        "dates_list" => {
            let person = present(args_as::<PersonArgs>(args)?.person_id);
            Ok(json!({ "dates": store.list_dates(&caller, person.as_deref()).await? }))
        }
        "date_upsert" => {
            let input: DateUpsertArgs = args_as(args)?;
            match present(input.id) {
                Some(id) => {
                    let patch = DatePatch {
                        kind: input.kind,
                        label: input.label.map(Some),
                        month: input.month,
                        day: input.day,
                        year: input.year.map(Some),
                        remind_offsets_days: input.remind_offsets_days,
                        tz: input.tz.map(Some),
                        visibility: None,
                    };
                    json_of(&store.update_date(&caller, &id, patch).await?)
                }
                None => {
                    let kind = input
                        .kind
                        .ok_or(crate::validation::ValidationError::Required { field: "kind" })?;
                    let new = NewDate {
                        person_id: input.person_id.unwrap_or_default(),
                        kind,
                        label: input.label,
                        month: input.month.unwrap_or_default(),
                        day: input.day.unwrap_or_default(),
                        year: input.year,
                        remind_offsets_days: input.remind_offsets_days,
                        tz: input.tz,
                        visibility: None,
                    };
                    json_of(&store.create_date(&caller, new).await?)
                }
            }
        }
        "date_delete" => {
            let id = required("id", &args_as::<IdArgs>(args)?.id)?;
            store.delete_date(&caller, &id).await?;
            Ok(json!({ "ok": true }))
        }
        "lists_get" => {
            let input: ListsGetArgs = args_as(args)?;
            match present(input.list_id) {
                Some(list_id) => {
                    let (list, items) = store
                        .list_items(&caller, &list_id, input.include_done.unwrap_or(false))
                        .await?;
                    Ok(json!({ "list": list, "items": items }))
                }
                None => Ok(json!({ "lists": store.list_lists(&caller, None).await? })),
            }
        }
        "list_create" => {
            let input: NewList = args_as(args)?;
            json_of(
                &store
                    .create_list(
                        &caller,
                        NewList {
                            visibility: None,
                            ..input
                        },
                    )
                    .await?,
            )
        }
        "list_add_items" => {
            let input: AddItemsArgs = args_as(args)?;
            let list_id = required("listId", &input.list_id)?;
            Ok(json!({ "items": store.add_items(&caller, &list_id, input.items).await? }))
        }
        "list_check" => {
            let input: CheckArgs = args_as(args)?;
            let item_id = required("itemId", &input.item_id)?;
            let (item, next) = store.complete_item(&caller, &item_id, input.done).await?;
            Ok(json!({ "item": item, "next": next }))
        }
        "list_assign" => {
            let input: AssignArgs = args_as(args)?;
            let item_id = required("itemId", &input.item_id)?;
            let patch = ItemPatch {
                assignee_person_id: Some(present(input.assignee_person_id)),
                ..ItemPatch::default()
            };
            json_of(&store.update_item(&caller, &item_id, patch).await?)
        }
        "list_remove_item" => {
            let item_id = required("itemId", &args_as::<ItemIdArgs>(args)?.item_id)?;
            store.delete_item(&caller, &item_id).await?;
            Ok(json!({ "ok": true }))
        }
        "chores_tally" => {
            let input: TallyArgs = args_as(args)?;
            let list_id = present(input.list_id);
            let since = present(input.since);
            Ok(
                json!({ "tally": store.tally(&caller, list_id.as_deref(), since.as_deref()).await? }),
            )
        }
        "prefs_get" => {
            let person = present(args_as::<PersonArgs>(args)?.person_id).unwrap_or_default();
            Ok(json!({ "preferences": store.list_preferences(&caller, &person).await? }))
        }
        "prefs_set" => {
            let input: PrefsSetArgs = args_as(args)?;
            let person_id = required("personId", &input.person_id)?;
            match present(input.id) {
                Some(id) => {
                    // The row must belong to the named person and be visible here.
                    let visible = store.list_preferences(&caller, &person_id).await?;
                    if !visible.iter().any(|pref| pref.base.id == id) {
                        return Err(FamilyError::NotFound);
                    }
                    let patch = PreferencePatch {
                        key: Some(input.key),
                        value: Some(input.value),
                        sensitive: None,
                        visibility: None,
                    };
                    json_of(&store.update_preference(&caller, &id, patch).await?)
                }
                None => {
                    let new = NewPreference {
                        person_id,
                        key: input.key,
                        value: input.value,
                        sensitive: Some(false),
                        visibility: None,
                    };
                    json_of(&store.create_preference(&caller, new).await?)
                }
            }
        }
        "wishlist_get" => {
            let person = present(args_as::<PersonArgs>(args)?.person_id)
                .ok_or(crate::validation::ValidationError::Required { field: "personId" })?;
            Ok(json!({ "items": store.list_wishlist(&caller, Some(&person)).await? }))
        }
        "wishlist_add" => {
            let input: WishAddArgs = args_as(args)?;
            let new = NewWish {
                person_id: input.person_id,
                title: input.title,
                url: input.url,
                price_hint: input.price_hint,
                priority: input.priority,
                visibility: None,
            };
            json_of(&store.create_wish(&caller, new).await?)
        }
        "gift_context" => {
            let query: GiftContextQuery = args_as(args)?;
            json_of(&store.gift_context(&caller, query).await?)
        }
        other => Err(FamilyError::BadRequest(format!("unknown tool '{other}'"))),
    }
}
