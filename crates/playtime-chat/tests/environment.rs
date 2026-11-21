use playtime_chat::environment::{LEGACY_INVENTORY, inventory, save};
use playtime_local::{PrivateDirectory, Token};
use serde_json::json;
use std::{fs, path::Path};

#[test]
fn legacy_inventory_is_unchanged_and_project_inventory_is_bound_to_its_workspace() {
    assert_eq!(
        inventory(&json!({}), Path::new("/project")).unwrap(),
        LEGACY_INVENTORY
    );
    let image = json!({"project_environment":{"schema_version":1,"workspace":"/project","inventory":"Rust 1.99, Zephyr and Git"}});
    let text = inventory(&image, Path::new("/project")).unwrap();
    assert!(text.contains("Rust 1.99"));
    assert!(text.contains("no external network"));
    assert!(!text.contains("Rust/Cargo and Go are not installed"));
    assert!(inventory(&image, Path::new("/another")).is_err());
    let missing = json!({"project_environment":{"schema_version":1,"workspace":"/project"}});
    assert!(inventory(&missing, Path::new("/project")).is_err());
}

#[test]
fn frozen_environment_is_private_and_cannot_be_replaced() {
    let path = std::env::temp_dir().join(format!(
        "playtime-environment-{}",
        Token::generate().unwrap().expose()
    ));
    PrivateDirectory::open(&path, true).unwrap();
    save(&path, b"{\"image_id\":\"original\"}").unwrap();
    save(&path, b"{\"image_id\":\"original\"}").unwrap();
    assert!(save(&path, b"{\"image_id\":\"changed\"}").is_err());
    assert_eq!(
        fs::read(path.join("environment.lock.json")).unwrap(),
        b"{\"image_id\":\"original\"}"
    );
    fs::remove_dir_all(path).unwrap();
}
