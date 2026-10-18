use playtime_local::{Credentials, Token, read_connection};
use playtime_protocol::api::{Connection, ServiceId};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "playtime-local-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn connection(c: &Credentials) -> Connection {
    Connection {
        schema_version: 1,
        endpoint: "http://127.0.0.1:8766".into(),
        service_id: ServiceId::new("service-files").unwrap(),
        token_file: c.token_path.to_string_lossy().into_owned(),
    }
}

#[test]
fn credentials_are_random_private_stable_and_excluded_from_discovery() {
    let temp = Temp::new();
    let c = Credentials::initialize(&temp.0).unwrap();
    assert_eq!(
        fs::metadata(temp.0.join(".playtime")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&c.token_path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::read(&c.token_path).unwrap().len(), 65);
    c.publish(&connection(&c)).unwrap();
    let (read, token) = read_connection(&c.connection_path).unwrap();
    assert_eq!(read, connection(&c));
    assert!(token.matches(&c.token.expose()));
    assert!(
        !fs::read_to_string(&c.connection_path)
            .unwrap()
            .contains(&c.token.expose())
    );
    assert!(!format!("{:?}", c.token).contains(&c.token.expose()));
    let again = Credentials::initialize(&temp.0).unwrap();
    assert!(again.token.matches(&c.token.expose()));
    assert!(!Token::generate().unwrap().matches(&c.token.expose()));
}
#[test]
fn invalid_credentials_are_never_replaced() {
    for contents in ["", "partial", &"z".repeat(64), &"a".repeat(66)] {
        let temp = Temp::new();
        let c = Credentials::initialize(&temp.0).unwrap();
        fs::write(&c.token_path, contents).unwrap();
        assert!(Credentials::initialize(&temp.0).is_err());
        assert_eq!(fs::read_to_string(&c.token_path).unwrap(), contents);
    }
    for mode in [0o644, 0o660, 0o000] {
        let temp = Temp::new();
        let c = Credentials::initialize(&temp.0).unwrap();
        fs::set_permissions(&c.token_path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(Credentials::initialize(&temp.0).is_err());
    }
}
#[test]
fn links_nonregular_files_and_writable_parents_are_refused() {
    let temp = Temp::new();
    let c = Credentials::initialize(&temp.0).unwrap();
    let original = temp.0.join("saved-token");
    fs::rename(&c.token_path, &original).unwrap();
    symlink(&original, &c.token_path).unwrap();
    assert!(Credentials::initialize(&temp.0).is_err());
    fs::remove_file(&c.token_path).unwrap();
    fs::hard_link(&original, &c.token_path).unwrap();
    assert!(Credentials::initialize(&temp.0).is_err());
    fs::remove_file(&c.token_path).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &c.token_path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    assert!(Credentials::initialize(&temp.0).is_err());
    fs::remove_file(&c.token_path).unwrap();
    fs::rename(original, &c.token_path).unwrap();
    for mode in [0o777, 0o770] {
        fs::set_permissions(temp.0.join(".playtime"), fs::Permissions::from_mode(mode)).unwrap();
        assert!(Credentials::initialize(&temp.0).is_err());
    }
    fs::set_permissions(temp.0.join(".playtime"), fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.0.join("linked");
    symlink(temp.0.join(".playtime"), &link).unwrap();
    assert!(Credentials::initialize(&link).is_err());
}
#[test]
fn discovery_writes_are_atomic_and_refuse_existing_links() {
    let temp = Temp::new();
    let c = Credentials::initialize(&temp.0).unwrap();
    c.publish(&connection(&c)).unwrap();
    let mut next = connection(&c);
    next.endpoint = "http://127.0.0.1:12345".into();
    c.publish(&next).unwrap();
    assert_eq!(read_connection(&c.connection_path).unwrap().0, next);
    fs::remove_file(&c.connection_path).unwrap();
    symlink(&c.token_path, &c.connection_path).unwrap();
    let before = fs::read(&c.token_path).unwrap();
    assert!(c.publish(&next).is_err());
    assert_eq!(fs::read(&c.token_path).unwrap(), before);
    assert!(read_connection(&c.connection_path).is_err());
}
