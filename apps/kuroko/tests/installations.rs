#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kuroko::router::Installations;
use kuroko_core::Millis;
use kuroko_linear::oauth::{Installation, Scopes};
use kuroko_store::Store;

const NOW: Millis = Millis(1_700_000_000_000);

fn installation() -> Installation {
    Installation {
        app_user_id: String::from("app-user-1"),
        access_token: String::from("access-1"),
        refresh_token: Some(String::from("refresh-1")),
        expires_at: Millis(NOW.0 + 86_399_000),
        scopes: Scopes::parse("read write app:assignable"),
        installed_at: NOW,
    }
}

#[test]
fn an_installation_round_trips_through_the_store() {
    let store = Store::open_in_memory().unwrap();

    Installations::save(&store, &installation()).unwrap();

    let row = store.load_installation("app-user-1").unwrap().unwrap();
    assert_eq!(row.app_user_id, "app-user-1");
    assert_eq!(row.access_token, "access-1");
    assert_eq!(row.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(row.expires_at, NOW.0 + 86_399_000);
    assert_eq!(row.scope, "read,write,app:assignable");
    assert_eq!(row.installed_at, NOW.0);
}

#[test]
fn an_installation_without_a_refresh_token_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let mut bare = installation();
    bare.refresh_token = None;

    Installations::save(&store, &bare).unwrap();

    assert_eq!(
        store
            .load_installation("app-user-1")
            .unwrap()
            .unwrap()
            .refresh_token,
        None
    );
}

#[test]
fn a_second_install_for_the_same_app_user_upserts_one_row() {
    let store = Store::open_in_memory().unwrap();
    Installations::save(&store, &installation()).unwrap();

    let mut replaced = installation();
    replaced.access_token = String::from("access-2");
    replaced.installed_at = Millis(NOW.0 + 1_000);
    Installations::save(&store, &replaced).unwrap();

    let row = store.load_installation("app-user-1").unwrap().unwrap();
    assert_eq!(row.access_token, "access-2");
    assert_eq!(row.installed_at, NOW.0 + 1_000);
}

#[test]
fn the_last_install_is_the_default_installation() {
    let store = Store::open_in_memory().unwrap();
    Installations::save(&store, &installation()).unwrap();

    let mut later = installation();
    later.app_user_id = String::from("app-user-2");
    later.installed_at = Millis(NOW.0 + 60_000);
    Installations::save(&store, &later).unwrap();

    assert_eq!(
        store.default_installation().unwrap().unwrap().app_user_id,
        "app-user-2"
    );
}

#[test]
fn an_unknown_app_user_has_no_row() {
    let store = Store::open_in_memory().unwrap();

    Installations::save(&store, &installation()).unwrap();

    assert_eq!(store.load_installation("app-user-2").unwrap(), None);
}
