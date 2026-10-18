//! Audit Cargo's actual resolution before any dependency/build-script compilation.
use crate::{Result, snapshot::Snapshot};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Metadata {
    version: u32,
    workspace_root: PathBuf,
    packages: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    source: Option<String>,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}
#[derive(Deserialize)]
struct Target {
    kind: Vec<String>,
    src_path: PathBuf,
}

fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        format!(
            "cannot resolve Cargo source path {}: {error}",
            path.display()
        )
        .into()
    })
}

/// The caller supplies the same isolated command environment used by `build`.
/// Full metadata is required: --no-deps would omit external path patches.
pub fn verify(
    command: &mut Command,
    snapshot: &Snapshot,
    staging: &Path,
    cargo_home: &Path,
    target: &str,
) -> Result<()> {
    let output = command
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "staged Cargo resolution failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    verify_metadata(&output.stdout, snapshot, staging, cargo_home)
}

fn verify_metadata(
    bytes: &[u8],
    snapshot: &Snapshot,
    staging: &Path,
    cargo_home: &Path,
) -> Result<()> {
    let metadata: Metadata = serde_json::from_slice(bytes)?;
    let staging = canonical(staging)?;
    let cargo_home = canonical(cargo_home)?;
    if metadata.version != 1
        || canonical(&metadata.workspace_root)? != staging
        || metadata.packages.is_empty()
    {
        return Err("Cargo resolution does not describe the staged workspace".into());
    }
    for package in metadata.packages {
        let manifest = canonical(&package.manifest_path)?;
        if package.source.is_none() || manifest.starts_with(&staging) {
            snapshot.verify_staged_file(&staging, &manifest)?;
            for target in package.targets {
                snapshot.verify_staged_file(&staging, &target.src_path)?;
            }
        } else {
            // Registry/git dependencies may be fetched into this isolated Cargo
            // home. Source replacements must not masquerade as registry IDs
            // while using a local directory elsewhere on the host.
            if !manifest.starts_with(&cargo_home) {
                return Err(format!(
                    "resolved dependency {} is outside staging and the isolated Cargo cache: {}",
                    package.name,
                    manifest.display()
                )
                .into());
            }
            for target in package.targets {
                // Published crates can retain metadata for excluded tests or
                // benchmarks. They are not compiled as dependency targets.
                if target
                    .kind
                    .iter()
                    .any(|kind| ["test", "bench", "example"].contains(&kind.as_str()))
                {
                    continue;
                }
                if !canonical(&target.src_path)?.starts_with(&cargo_home) {
                    return Err(format!(
                        "resolved dependency {} has an external source target",
                        package.name
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;

    struct Fixture {
        root: PathBuf,
        checkout: PathBuf,
        staging: PathBuf,
        cargo_home: PathBuf,
        cargo: PathBuf,
        rustc: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pt-cargo-sources-{}",
                playtime_worker_control::random_id().unwrap().as_str()
            ));
            let checkout = root.join("checkout");
            let staging = root.join("staging");
            let cargo_home = root.join("cargo-home");
            let cargo = crate::output(Command::new("rustup").args(["which", "cargo"]))
                .unwrap()
                .into();
            let rustc = crate::output(Command::new("rustup").args(["which", "rustc"]))
                .unwrap()
                .into();
            let fixture = Self {
                root,
                checkout,
                staging,
                cargo_home,
                cargo,
                rustc,
            };
            for dir in [
                "checkout/crates/app/src",
                "checkout/crates/dep/src",
                "checkout/.cargo",
                "checkout/xtask",
                "outside/src",
                "cargo-home",
            ] {
                fs::create_dir_all(fixture.root.join(dir)).unwrap();
            }
            for (name, text) in [
                (
                    "Cargo.toml",
                    "[workspace]\nmembers=['crates/app']\nexclude=['crates/dep']\nresolver='3'",
                ),
                (
                    "rust-toolchain.toml",
                    include_str!("../../rust-toolchain.toml"),
                ),
                (
                    "crates/app/Cargo.toml",
                    "[package]\nname='probe'\nversion='0.1.0'\nedition='2024'\n[dependencies]\ndep='=1.0.0'",
                ),
                (
                    "crates/app/src/main.rs",
                    "fn main() { println!(\"{}\", dep::value()); }",
                ),
                (
                    "crates/dep/Cargo.toml",
                    "[package]\nname='dep'\nversion='1.0.0'\nedition='2024'\n[workspace]",
                ),
                ("crates/dep/src/lib.rs", "pub fn value() -> u32 { 1 }"),
                (
                    ".cargo/config.toml",
                    "[net]\noffline=true\n[patch.crates-io]\ndep={path='crates/dep'}",
                ),
            ] {
                fs::write(fixture.checkout.join(name), text).unwrap();
            }
            fs::copy(
                fixture.checkout.join("crates/dep/Cargo.toml"),
                fixture.root.join("outside/Cargo.toml"),
            )
            .unwrap();
            fs::write(
                fixture.root.join("outside/src/lib.rs"),
                "pub fn value() -> u32 { 99 }",
            )
            .unwrap();
            fs::write(
                fixture.checkout.join("crates/app/build.rs"),
                format!(
                    "fn main() {{ std::fs::write({:?}, b\"compiled\").unwrap(); }}",
                    fixture.root.join("compiled")
                ),
            )
            .unwrap();
            crate::output(
                fixture
                    .command(&fixture.checkout)
                    .arg("generate-lockfile")
                    .arg("--offline"),
            )
            .unwrap();
            fixture
        }
        fn command(&self, directory: &Path) -> Command {
            let mut command = Command::new(&self.cargo);
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("RUSTC", &self.rustc)
                .env("CARGO_HOME", &self.cargo_home)
                .env("CARGO_TARGET_DIR", self.root.join("target"))
                .current_dir(directory);
            command
        }
        fn snapshot(&self) -> Snapshot {
            let captured = snapshot::capture(&self.checkout).unwrap();
            captured.materialize(&self.staging).unwrap();
            captured
        }
        fn resolve(&self, captured: &Snapshot) -> Result<()> {
            verify(
                &mut self.command(&self.staging),
                captured,
                &self.staging,
                &self.cargo_home,
                "x86_64-unknown-linux-gnu",
            )
        }
    }

    #[test]
    fn real_cargo_resolution_accepts_a_captured_configuration_patch() {
        let fixture = Fixture::new();
        let captured = fixture.snapshot();
        // Later changes in the original checkout cannot influence the staged
        // patch. Metadata sees the staged dependency, and compilation uses it.
        fs::write(
            fixture.checkout.join("crates/dep/src/lib.rs"),
            "pub fn value() -> u32 { 2 }",
        )
        .unwrap();
        fixture.resolve(&captured).unwrap();
        assert!(
            !fixture.root.join("compiled").exists(),
            "metadata compiled a build script"
        );
        crate::output(
            fixture
                .command(&fixture.staging)
                .args(["build", "--locked", "--offline"]),
        )
        .unwrap();
        assert!(fixture.root.join("compiled").exists());
        assert_eq!(
            crate::output(&mut Command::new(fixture.root.join("target/debug/probe"))).unwrap(),
            "1"
        );
    }

    #[test]
    fn real_cargo_resolution_rejects_an_external_patch_before_compilation() {
        let fixture = Fixture::new();
        let captured = fixture.snapshot();
        let frozen_source = serde_json::to_vec(&captured).unwrap();
        let frozen_lock = fs::read(fixture.staging.join("Cargo.lock")).unwrap();
        // Intentionally bypass capture-time configuration validation to test
        // the independent resolved-graph guard with Cargo's real metadata.
        fs::write(
            fixture.staging.join(".cargo/config.toml"),
            format!(
                "[net]\noffline=true\n[patch.crates-io]\ndep={{path={:?}}}",
                fixture.root.join("outside")
            ),
        )
        .unwrap();
        for value in [99, 100] {
            fs::write(
                fixture.root.join("outside/src/lib.rs"),
                format!("pub fn value() -> u32 {{ {value} }}"),
            )
            .unwrap();
            let error = fixture.resolve(&captured).unwrap_err().to_string();
            assert!(
                error.contains("resolved local input is outside staging"),
                "{error}"
            );
            assert!(!fixture.root.join("compiled").exists());
            assert_eq!(
                fs::read(fixture.staging.join("Cargo.lock")).unwrap(),
                frozen_lock
            );
            assert_eq!(serde_json::to_vec(&captured).unwrap(), frozen_source);
        }
    }

    #[test]
    fn resolved_local_paths_are_canonical_and_must_have_been_captured() {
        let fixture = Fixture::new();
        let captured = fixture.snapshot();
        fs::remove_dir_all(fixture.staging.join("crates/dep")).unwrap();
        std::os::unix::fs::symlink(
            fixture.root.join("outside"),
            fixture.staging.join("crates/dep"),
        )
        .unwrap();
        let error = fixture.resolve(&captured).unwrap_err().to_string();
        assert!(error.contains("outside staging"), "{error}");
        assert!(!fixture.root.join("compiled").exists());
    }
}
