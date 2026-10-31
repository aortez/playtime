use playtime_client::{
    PendingLaunch,
    config::{self, Environment, Options, Source},
    state::{ReceiptStore, StateError},
};
use playtime_core::{RunId, RunMode};
use playtime_local::{Credentials, Token};
use playtime_protocol::api::*;
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::{Arc, Barrier},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "playtime-client-state-{}-{}",
            std::process::id(),
            &Token::generate().unwrap().expose()[..12]
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn env(&self) -> Environment {
        Environment([("HOME".into(), self.0.as_os_str().to_owned())].into())
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn pending() -> PendingLaunch {
    PendingLaunch {
        schema_version: 1,
        service_id: ServiceId::new("service-a").unwrap(),
        key: IdempotencyKey::new("shared").unwrap(),
        request: LaunchRequest {
            target: None,
            recipe_id: RecipeId::new("recipe").unwrap(),
            mode: RunMode::Simulation,
            preview_revision: PreviewRevision::new("frozen-revision").unwrap(),
        },
    }
}
#[test]
fn connection_sources_are_complete_and_never_borrow_credentials() {
    let root = Temp::new();
    let mut env = root.env();
    env.0.insert("PLAYTIME_TOKEN_FILE".into(), "token".into());
    assert!(
        config::resolve(
            &Options {
                endpoint: Some("http://localhost:1".into()),
                ..Default::default()
            },
            &env,
            &root.0
        )
        .is_err()
    );
    assert!(config::resolve(&Options::default(), &env, &root.0).is_err());
    let options = Options {
        connection_file: Some("connection.json".into()),
        ..Default::default()
    };
    let r = config::resolve(&options, &env, &root.0).unwrap();
    assert!(
        matches!(r.source, Some(Source::Discovery { ref path }) if *path == root.0.join("connection.json"))
    );
    assert!(r.load().is_err());
    assert!(
        config::resolve(
            &Options {
                endpoint: Some("http://localhost:1".into()),
                token_file: Some("token".into()),
                ..options
            },
            &env,
            &root.0
        )
        .is_err()
    );
}
#[test]
fn config_paths_and_state_precedence_are_shared_and_strict() {
    let root = Temp::new();
    let mut env = root.env();
    let file = root.0.join("client.toml");
    std::fs::write(
        &file,
        "connection_file = 'service/.playtime/connection.json'\nstate_dir = 'shared-state'\n",
    )
    .unwrap();
    let opts = Options {
        config: Some("client.toml".into()),
        ..Default::default()
    };
    let resolved = config::resolve(&opts, &env, &root.0).unwrap();
    assert_eq!(resolved.state_dir, root.0.join("shared-state"));
    assert!(
        matches!(resolved.source, Some(Source::Discovery { path }) if path == root.0.join("service/.playtime/connection.json"))
    );
    env.0
        .insert("PLAYTIME_STATE_DIR".into(), "env-state".into());
    assert_eq!(
        config::resolve(&opts, &env, &root.0).unwrap().state_dir,
        root.0.join("env-state")
    );
    let opts = Options {
        state_dir: Some("flag-state".into()),
        ..opts
    };
    assert_eq!(
        config::resolve(&opts, &env, &root.0).unwrap().state_dir,
        root.0.join("flag-state")
    );
    for invalid in [
        "unknown = 1",
        "endpoint = 'http://localhost:1'",
        "connection_file = 'a'\nconnection_file = 'b'",
        "endpoint = 'http://secret:secret@localhost:1'\ntoken_file = 't'",
        "connection_file = 'x'\ntoken_file = 'y'",
    ] {
        std::fs::write(&file, invalid).unwrap();
        assert!(config::resolve(&opts, &env, &root.0).is_err());
    }
    std::fs::remove_file(&file).unwrap();
    assert!(config::resolve(&opts, &env, &root.0).is_err());
}
#[test]
fn relative_xdg_is_ignored_and_discovery_never_searches_parents() {
    let root = Temp::new();
    let mut env = root.env();
    env.0.insert("XDG_STATE_HOME".into(), "relative".into());
    env.0.insert("XDG_CONFIG_HOME".into(), "relative".into());
    let creds = Credentials::initialize(&root.0).unwrap();
    creds
        .publish(&Connection {
            schema_version: 1,
            endpoint: "http://127.0.0.1:1".into(),
            service_id: ServiceId::new("service").unwrap(),
            token_file: creds.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let child = root.0.join("child");
    std::fs::create_dir(&child).unwrap();
    let r = config::resolve(&Options::default(), &env, &child).unwrap();
    assert!(r.source.is_none());
    assert_eq!(r.state_dir, root.0.join(".local/state/playtime"));
    assert!(
        config::resolve(&Options::default(), &env, &root.0)
            .unwrap()
            .load()
            .is_ok()
    );
}
#[test]
fn discovery_resolves_relative_token_but_keeps_file_policy() {
    let root = Temp::new();
    let credentials = Credentials::initialize(&root.0).unwrap();
    let connection = serde_json::json!({"schema_version":1,"endpoint":"http://127.0.0.1:1","service_id":"service","token_file":"api-token"});
    playtime_local::replace_private(
        &credentials.connection_path,
        &serde_json::to_vec(&connection).unwrap(),
    )
    .unwrap();
    assert!(playtime_local::read_connection(&credentials.connection_path).is_ok());
    std::fs::set_permissions(
        &credentials.token_path,
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(playtime_local::read_connection(&credentials.connection_path).is_err());
}
#[test]
fn cli_and_workbench_reuse_one_durable_receipt_across_concurrent_access() {
    let root = Temp::new();
    let store = ReceiptStore::new(&root.0.join("state"));
    let pending = pending();
    let gate = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let gate = gate.clone();
            let store = store.clone();
            let pending = pending.clone();
            std::thread::spawn(move || {
                gate.wait();
                store.save(&pending, "http://127.0.0.1:99").unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(receipts.iter().all(|r| r == &receipts[0]));
    assert_eq!(
        std::fs::metadata(store.path(&pending.key))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let ui_store = ReceiptStore::new(&root.0.join("state"));
    let restored = ui_store.read(&pending.key).unwrap().unwrap();
    assert_eq!(restored.pending(), pending);
    let acknowledged = ui_store
        .acknowledge(&restored, &RunId::new("run-a").unwrap())
        .unwrap();
    assert_eq!(store.read(&pending.key).unwrap().unwrap(), acknowledged);
    assert!(matches!(
        store.acknowledge(&restored, &RunId::new("run-b").unwrap()),
        Err(StateError::Conflict)
    ));
    assert_eq!(store.list(None, 1).unwrap().items.len(), 1);
}
#[test]
fn conflicting_content_service_or_implicit_endpoint_change_never_rebinds_receipt() {
    let root = Temp::new();
    let store = ReceiptStore::new(&root.0.join("state"));
    let pending = pending();
    let saved = store.save(&pending, "http://127.0.0.1:1").unwrap();
    let mut changed = pending.clone();
    changed.request.preview_revision = PreviewRevision::new("new-revision").unwrap();
    assert!(matches!(
        store.save(&changed, "http://127.0.0.1:1"),
        Err(StateError::Conflict)
    ));
    changed = pending.clone();
    changed.service_id = ServiceId::new("other").unwrap();
    assert!(matches!(
        store.save(&changed, "http://127.0.0.1:1"),
        Err(StateError::Conflict)
    ));
    assert!(
        saved
            .check_target(&pending.service_id, "http://127.0.0.1:2", false)
            .is_err()
    );
    assert!(
        saved
            .check_target(&pending.service_id, "http://127.0.0.1:2", true)
            .is_ok()
    );
    assert!(
        saved
            .check_target(&changed.service_id, "http://127.0.0.1:1", true)
            .is_err()
    );
    assert_eq!(store.read(&pending.key).unwrap().unwrap(), saved);
}
#[test]
fn unsafe_and_corrupt_receipts_are_not_replaced() {
    let root = Temp::new();
    let state = root.0.join("state");
    let store = ReceiptStore::new(&state);
    let pending = pending();
    store.save(&pending, "http://localhost:1").unwrap();
    let path = store.path(&pending.key);
    std::fs::write(&path, b"broken").unwrap();
    assert!(store.read(&pending.key).is_err());
    assert!(store.save(&pending, "http://localhost:1").is_err());
    std::fs::remove_file(&path).unwrap();
    let target = root.0.join("important");
    std::fs::write(&target, b"preserve").unwrap();
    symlink(&target, &path).unwrap();
    assert!(store.save(&pending, "http://localhost:1").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    assert!(store.discard(&saved_for(&pending)).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
}
fn saved_for(pending: &PendingLaunch) -> playtime_client::state::LaunchReceipt {
    playtime_client::state::LaunchReceipt {
        schema_version: 1,
        service_id: pending.service_id.clone(),
        endpoint: "http://localhost:1".into(),
        key: pending.key.clone(),
        request: pending.request.clone(),
        created_at_unix_ms: 0,
        run_id: None,
    }
}
#[test]
fn only_an_unacknowledged_matching_receipt_is_discarded() {
    let root = Temp::new();
    let store = ReceiptStore::new(&root.0.join("state"));
    let pending = pending();
    let saved = store.save(&pending, "http://127.0.0.1:1").unwrap();
    let mut changed = saved.clone();
    changed.request.preview_revision = PreviewRevision::new("new-revision").unwrap();
    assert!(matches!(store.discard(&changed), Err(StateError::Conflict)));
    let acknowledged = store
        .acknowledge(&saved, &RunId::new("run-a").unwrap())
        .unwrap();
    assert!(matches!(store.discard(&saved), Err(StateError::Conflict)));
    assert!(matches!(
        store.discard(&acknowledged),
        Err(StateError::Conflict)
    ));
    assert_eq!(store.read(&pending.key).unwrap(), Some(acknowledged));

    let mut fresh = pending.clone();
    fresh.key = IdempotencyKey::new("refused").unwrap();
    let refused = store.save(&fresh, "http://127.0.0.1:1").unwrap();
    store.discard(&refused).unwrap();
    assert_eq!(store.read(&fresh.key).unwrap(), None);
    // Discarding is idempotent, and only the refused receipt went away.
    store.discard(&refused).unwrap();
    assert_eq!(store.list(None, 10).unwrap().items.len(), 1);
}

#[test]
fn cursor_receipts_bind_identity_and_refuse_regression() {
    use playtime_client::state::CursorReceipt;
    use playtime_core::Sha256Digest;
    let root = Temp::new();
    let path = root.0.join("cursor.json");
    let frontier = Frontier {
        run_id: RunId::new("run-a").unwrap(),
        manifest_sha256: Sha256Digest::new("11".repeat(32)).unwrap(),
        prefix: Prefix {
            sequence: 3,
            byte_offset: 1000,
            sha256: Sha256Digest::new("22".repeat(32)).unwrap(),
        },
    };
    let receipt = |n| CursorReceipt {
        schema_version: 1,
        service_id: ServiceId::new("service-a").unwrap(),
        run_id: frontier.run_id.clone(),
        cursor: EventCursor::new(frontier.clone(), n)
            .unwrap()
            .encode()
            .unwrap(),
    };
    receipt(2).write(&path).unwrap();
    assert!(receipt(1).write(&path).is_err());
    assert!(
        CursorReceipt::read(
            &path,
            &ServiceId::new("service-b").unwrap(),
            &frontier.run_id
        )
        .is_err()
    );
    assert!(
        CursorReceipt::read(
            &path,
            &ServiceId::new("service-a").unwrap(),
            &RunId::new("run-b").unwrap()
        )
        .is_err()
    );
    assert_eq!(
        CursorReceipt::read(
            &path,
            &ServiceId::new("service-a").unwrap(),
            &frontier.run_id
        )
        .unwrap()
        .unwrap(),
        receipt(2)
    );
    receipt(3).write(&path).unwrap();
}
