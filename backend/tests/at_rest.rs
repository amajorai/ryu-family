//! Family data at rest: sealed per value in `family.db`, legacy plaintext
//! sealed once on open, unreadable rows skipped and counted, other keys
//! refused, and the keyless `mcp` process served through the sidecar handoff.

mod common;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use common::member;
use ryu_crypto::{ContentCipher, PluginCipher};
use ryu_family::{
    api,
    authz::Caller,
    seal_keys::{KeySource, PluginKeys},
    sidecar_link::{self, SidecarClient},
    state::{AppState, Config, PLUGIN_ID},
    store::{column_plans, FamilyStore, STORE},
};
use serde_json::json;

const T: &str = "org:o1";
const NAME: &str = "Zebra Grandmother";
const VALUE: &str = "quokka-allergy-marker";
const IDEA: &str = "wombat-telescope-marker";
const ITEM: &str = "numbat-groceries-marker";

fn keys(n: u8) -> PluginKeys {
    PluginKeys::sealing(
        PluginCipher::from_subkey_b64(
            PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode([n; 32]),
        )
        .unwrap(),
        KeySource::Delivered,
    )
}

fn db_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "ryu-family-at-rest-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .join(STORE)
}

fn disk_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap_or_default();
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    bytes.extend(std::fs::read(PathBuf::from(wal)).unwrap_or_default());
    bytes
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn caller() -> Caller {
    member(T, "bo")
}

/// One person with a preference, a gift idea and a list item.
async fn seed(store: &FamilyStore) -> String {
    store.ensure_settings(T, "bo").await.unwrap();
    let person = store
        .create_person(
            &caller(),
            serde_json::from_value(json!({ "displayName": NAME, "relationship": "grandmother" }))
                .unwrap(),
        )
        .await
        .unwrap();
    let person_id = person.base.id.clone();
    store
        .create_preference(
            &caller(),
            serde_json::from_value(json!({ "personId": person_id, "key": "allergy", "value": VALUE, "sensitive": true }))
                .unwrap(),
        )
        .await
        .unwrap();
    store
        .create_gift(
            &caller(),
            serde_json::from_value(
                json!({ "personId": person_id, "idea": IDEA, "url": "https://example.com/t" }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let list = store
        .create_list(
            &caller(),
            serde_json::from_value(json!({ "name": "Weekly shop", "kind": "grocery" })).unwrap(),
        )
        .await
        .unwrap();
    store
        .add_items(
            &caller(),
            &list.base.id,
            serde_json::from_value(json!([{ "text": ITEM, "qty": "2" }])).unwrap(),
        )
        .await
        .unwrap();
    person_id
}

fn raw(path: &Path, sql: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows
}

fn assert_all_sealed(path: &Path, n: u8) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let cipher = keys(n).content_cipher(STORE).unwrap();
    ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap()).unwrap();
}

#[tokio::test]
async fn names_preferences_gifts_and_items_are_sealed_on_disk_and_read_back() {
    let path = db_path();
    let store = FamilyStore::open_with_keys(path.clone(), &keys(1)).unwrap();
    let person_id = seed(&store).await;
    let people = store.list_people(&caller()).await.unwrap();
    assert_eq!(people[0].display_name, NAME);
    let prefs = store.list_preferences(&caller(), &person_id).await.unwrap();
    assert_eq!(prefs[0].value, VALUE);
    let gifts = store
        .list_gifts(&caller(), Some(&person_id), None)
        .await
        .unwrap();
    assert_eq!(gifts[0].idea, IDEA);
    drop(store);
    let bytes = disk_bytes(&path);
    for needle in [NAME, VALUE, IDEA, ITEM, "grandmother", "allergy"] {
        assert!(!contains(&bytes, needle), "{needle} on disk");
    }
    assert_all_sealed(&path, 1);
}

#[tokio::test]
async fn legacy_plaintext_is_sealed_once_and_still_reads() {
    let path = db_path();
    let person_id = {
        let store = FamilyStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap();
        assert_eq!(store.seal_health()["state"], "compat");
        seed(&store).await
    };
    assert!(contains(&disk_bytes(&path), VALUE));
    let store = FamilyStore::open_with_keys(path.clone(), &keys(2)).unwrap();
    let prefs = store.list_preferences(&caller(), &person_id).await.unwrap();
    assert_eq!(prefs[0].value, VALUE);
    drop(store);
    assert_all_sealed(&path, 2);
    let first = raw(&path, "SELECT display_name FROM people");
    assert!(first.iter().all(|v| ContentCipher::is_sealed_text(v)));
    let bytes = disk_bytes(&path);
    for needle in [NAME, VALUE, IDEA, ITEM] {
        assert!(
            !contains(&bytes, needle),
            "{needle} on disk after migration"
        );
    }
    // A second open changes nothing.
    drop(FamilyStore::open_with_keys(path.clone(), &keys(2)).unwrap());
    assert_eq!(raw(&path, "SELECT display_name FROM people"), first);
}

#[tokio::test]
async fn a_value_copied_to_another_row_is_skipped_and_counted() {
    let path = db_path();
    let store = FamilyStore::open_with_keys(path.clone(), &keys(3)).unwrap();
    seed(&store).await;
    store
        .create_person(
            &caller(),
            serde_json::from_value(json!({ "displayName": "Second" })).unwrap(),
        )
        .await
        .unwrap();
    drop(store);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE people SET display_name = (SELECT display_name FROM people WHERE display_name != '' ORDER BY created_at LIMIT 1)
             WHERE id = (SELECT id FROM people ORDER BY created_at DESC LIMIT 1)",
            [],
        )
        .unwrap();
    let store = FamilyStore::open_with_keys(path, &keys(3)).unwrap();
    let people = store.list_people(&caller()).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].display_name, NAME);
    assert_eq!(store.seal_health()["unreadable_rows"], 1);
}

#[tokio::test]
async fn another_key_or_no_key_refuses_writes_and_keeps_data() {
    let path = db_path();
    drop({
        let store = FamilyStore::open_with_keys(path.clone(), &keys(4)).unwrap();
        seed(&store).await;
        store
    });
    for other in [keys(5), PluginKeys::compat()] {
        let wrong = FamilyStore::open_with_keys(path.clone(), &other).unwrap();
        assert_eq!(wrong.seal_health()["state"], "unhealthy");
        assert!(wrong
            .create_person(
                &caller(),
                serde_json::from_value(json!({ "displayName": "Refused" })).unwrap(),
            )
            .await
            .is_err());
    }
    let back = FamilyStore::open_with_keys(path.clone(), &keys(4)).unwrap();
    let people = back.list_people(&caller()).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].display_name, NAME);
}

#[tokio::test]
async fn the_mcp_process_reaches_sealed_data_through_the_sidecar_handoff() {
    let path = db_path();
    let dir = path.parent().unwrap().to_path_buf();
    let store = FamilyStore::open_with_keys(path.clone(), &keys(6)).unwrap();
    seed(&store).await;
    store
        .patch_settings(
            &caller(),
            serde_json::from_value(json!({ "agentAccess": true })).unwrap(),
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handoff = sidecar_link::publish(&dir, listener.local_addr().unwrap().port()).unwrap();
    let state = AppState::new(store, Config::for_tests("core-token"), None);
    let app = api::app_with_handoff(state, handoff.token.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = SidecarClient::connect(&dir).await.expect("sidecar answers");
    let result = client
        .call(
            "POST",
            api::HANDOFF_MCP_ROUTE,
            Some(&json!({ "name": "people_list", "arguments": {} })),
        )
        .await
        .unwrap();
    assert_eq!(result["isError"], false);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(NAME));
    // The handoff token opens nothing under the Core-proxied mount.
    let denied = client
        .request("GET", &format!("{}/people", api::MOUNT), None)
        .await
        .unwrap();
    assert_eq!(denied.status, 401);
    sidecar_link::withdraw(&dir, &handoff);

    // Without a sidecar, a keyless process refuses the sealed store.
    let keyless = FamilyStore::open_with_keys(path, &PluginKeys::compat()).unwrap();
    assert_eq!(keyless.seal_health()["state"], "unhealthy");
}
