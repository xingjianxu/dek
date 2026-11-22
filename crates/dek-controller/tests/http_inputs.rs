use dek_controller::{
    cache::{CacheStore, CachedHttpInputs, RefreshHttpInputs, RefreshStats},
    eval::HttpInputProvider,
};

mod support;
use support::http_server::Server;

#[test]
fn only_explicit_refresh_fetches_and_offline_inputs_survive_the_server() {
    let root = tempfile::tempdir().unwrap();
    let store = CacheStore::new(&root.path().join("cache"), root.path()).unwrap();
    let server = Server::start();
    let requested = server.url("/selected");
    let unrelated = server.url("/unrelated");
    server.respond("/selected", 200, "tracked");
    server.respond("/unrelated", 200, "ignored");

    let mut offline = CachedHttpInputs::new(&store);
    assert_eq!(offline.read(&requested).unwrap(), None);
    assert_eq!(server.count("/selected"), 0);

    let mut refresh = RefreshHttpInputs::new(&store);
    assert_eq!(
        refresh.read(&requested).unwrap().as_deref(),
        Some("tracked")
    );
    assert_eq!(
        refresh.read(&requested).unwrap().as_deref(),
        Some("tracked")
    );
    assert_eq!(
        refresh.stats(),
        RefreshStats {
            updated: 1,
            unchanged: 0
        }
    );
    assert_eq!(server.count("/selected"), 1);
    assert_eq!(server.count("/unrelated"), 0);

    drop(server);
    assert_eq!(
        offline.read(&requested).unwrap().as_deref(),
        Some("tracked")
    );
    assert_eq!(offline.read(&unrelated).unwrap(), None);
}

#[test]
fn refresh_compares_response_content_and_preserves_the_last_success_on_error() {
    let root = tempfile::tempdir().unwrap();
    let store = CacheStore::new(&root.path().join("cache"), root.path()).unwrap();
    let server = Server::start();
    let url = server.url("/data");
    server.respond("/data", 200, "");
    let mut refresh = RefreshHttpInputs::new(&store);
    assert_eq!(refresh.read(&url).unwrap().as_deref(), Some(""));
    assert_eq!(refresh.stats().updated, 1);

    let mut refresh = RefreshHttpInputs::new(&store);
    assert_eq!(refresh.read(&url).unwrap().as_deref(), Some(""));
    assert_eq!(
        refresh.stats(),
        RefreshStats {
            updated: 0,
            unchanged: 1
        }
    );

    server.respond("/data", 200, "revised");
    let mut refresh = RefreshHttpInputs::new(&store);
    assert_eq!(refresh.read(&url).unwrap().as_deref(), Some("revised"));
    assert_eq!(refresh.stats().updated, 1);

    server.respond("/data", 503, "failure");
    let error = RefreshHttpInputs::new(&store).read(&url).unwrap_err();
    assert!(error.contains(&url) && error.contains("503"), "{error}");
    assert_eq!(server.count("/data"), 4);
    assert_eq!(
        CachedHttpInputs::new(&store).read(&url).unwrap().as_deref(),
        Some("revised")
    );
}
