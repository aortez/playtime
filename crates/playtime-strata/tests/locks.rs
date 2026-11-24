//! The committed model locks. A saved conversation is bound to its lock's
//! fingerprint, so these digests are the ones real sessions carry.
use playtime_core::Sha256Digest;
use playtime_strata::ModelLock;

fn lock(text: &str) -> ModelLock {
    serde_json::from_str(text).unwrap()
}
fn sha(hex: &str) -> Sha256Digest {
    Sha256Digest::new(hex).unwrap()
}

#[test]
fn strata_locks_pin_their_server_release_and_keep_earlier_sessions_resumable() {
    for (current, earlier, fingerprint) in [
        (
            include_str!("../../../models/framework-strata-gyro-m-fast-v2.lock.json"),
            include_str!("../../../models/framework-strata-gyro-m-fast-v1.lock.json"),
            "85e557d94482e2c27d52ec60671a9147e9569dd213bcb976f01ffca86b898acf",
        ),
        (
            include_str!("../../../models/bird-strata-gyro-m-fast-v2.lock.json"),
            include_str!("../../../models/bird-strata-gyro-m-fast-v1.lock.json"),
            "d512fa2f83f1b18380bbfca8e3ce9c3facf248f686d26d44f5444a8a1fc3bd99",
        ),
    ] {
        let current = lock(current);
        let earlier = lock(earlier);
        // The earlier lock no longer passes on its own: it doesn't pin the release.
        assert!(earlier.validate().is_err());
        // It differs only by the release manifest, and its fingerprint is the
        // digest its saved sessions carry.
        let mut stripped = current.clone();
        stripped.host.server_release_manifest_sha256 = None;
        assert_eq!(stripped, earlier);
        assert_eq!(
            current.earlier_fingerprint().unwrap(),
            Some(sha(fingerprint))
        );
        assert_ne!(current.fingerprint().unwrap(), sha(fingerprint));
        // The other reviewed release's manifest is refused for this engine version.
        let mut swapped = current.clone();
        swapped.identity.engine = if current.identity.engine == "0.1.58" {
            "0.1.57".into()
        } else {
            "0.1.58".into()
        };
        assert!(swapped.validate().is_err());
        let mut changed = current.clone();
        changed.host.server_release_manifest_sha256 = Some(sha(&"1".repeat(64)));
        assert!(changed.validate().is_err());
    }
}

#[test]
fn basalt_locks_keep_their_fingerprints_and_take_no_release_manifest() {
    for (text, fingerprint) in [
        (
            include_str!("../../../models/bird-basalt-iq4-v1.lock.json"),
            "c13b5a3f2f460acc2cf45f830ce2e8f7e488a06795687131e8b2f1841abdd4c2",
        ),
        (
            include_str!("../../../models/bird-basalt-nvfp4-v1.lock.json"),
            "374a9c0439c5d925db789bf906e99f59bff68c4b6c61d96cc44bd6a49ba59f69",
        ),
    ] {
        let basalt = lock(text);
        assert_eq!(basalt.fingerprint().unwrap(), sha(fingerprint));
        assert_eq!(basalt.earlier_fingerprint().unwrap(), None);
        let mut changed = basalt.clone();
        changed.host.server_release_manifest_sha256 = Some(sha(&"1".repeat(64)));
        assert!(changed.validate().is_err());
    }
}
