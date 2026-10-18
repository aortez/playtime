use playtime_worker::{Error, PrivateDirectory, verify_static_elf};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

fn elf() -> Vec<u8> {
    let mut bytes = vec![0; 120];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&3_u16.to_le_bytes()); // static PIE
    bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}
#[test]
fn elf_checks_architecture_interpreter_dependencies_and_bounds_including_static_pie() {
    let bytes = elf();
    verify_static_elf(&bytes).unwrap();
    let mut wrong = bytes.clone();
    wrong[18] = 183;
    assert!(matches!(
        verify_static_elf(&wrong),
        Err(Error::UnsupportedPlatform)
    ));
    let mut interpreter = bytes.clone();
    interpreter[64] = 3;
    assert!(matches!(
        verify_static_elf(&interpreter),
        Err(Error::DynamicExecutable)
    ));
    let mut needed = bytes.clone();
    needed[64] = 2;
    needed[72..80].copy_from_slice(&120_u64.to_le_bytes());
    needed[96..104].copy_from_slice(&16_u64.to_le_bytes());
    needed.extend_from_slice(&1_u64.to_le_bytes());
    needed.extend_from_slice(&1_u64.to_le_bytes());
    assert!(matches!(
        verify_static_elf(&needed),
        Err(Error::DynamicExecutable)
    ));
    for end in 0..bytes.len() {
        assert!(verify_static_elf(&bytes[..end]).is_err());
    }
    let mut overflow = bytes;
    overflow[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(verify_static_elf(&overflow).is_err());
}

struct Root(std::path::PathBuf);
impl Root {
    fn new(parent: &str) -> Self {
        let path = std::path::Path::new(parent).join(format!(
            "pt-package-{}",
            playtime_worker_control::random_id().unwrap().as_str()
        ));
        playtime_worker_control::create_private_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn copies_are_private_exclusive_bounded_and_do_not_follow_links() {
    let root = Root::new("/tmp");
    let dir = PrivateDirectory::open(&root.0).unwrap();
    dir.write_new("nested/image", &elf(), true).unwrap();
    assert_eq!(
        fs::metadata(root.0.join("nested/image"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
    assert_eq!(
        fs::metadata(root.0.join("nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(dir.write_new("nested/image", b"replacement", true).is_err());
    assert!(dir.read("nested/image", 1).is_err());
    assert!(dir.read("../anything", 1000).is_err());
    symlink(root.0.join("nested/image"), root.0.join("link")).unwrap();
    symlink(root.0.join("nested"), root.0.join("directory-link")).unwrap();
    fs::hard_link(root.0.join("nested/image"), root.0.join("hardlink")).unwrap();
    for name in ["link", "directory-link/image", "hardlink", "nested/image"] {
        assert!(dir.read(name, 1000).is_err());
    }
    dir.create("temporary").unwrap();
    dir.create("exists").unwrap();
    assert!(dir.publish_directory("temporary", "exists").is_err());
    dir.publish_directory("temporary", "published").unwrap();
}
#[test]
fn materialization_refuses_a_noexec_filesystem_when_available() {
    if !std::path::Path::new("/dev/shm").exists() {
        return;
    }
    let root = Root::new("/dev/shm");
    let file = fs::File::open(&root.0).unwrap();
    if rustix::fs::fstatvfs(&file)
        .unwrap()
        .f_flag
        .contains(rustix::fs::StatVfsMountFlags::NOEXEC)
    {
        assert!(matches!(
            PrivateDirectory::open(&root.0)
                .unwrap()
                .write_new("image", &elf(), true),
            Err(Error::ExecutionUnavailable)
        ));
    }
}
