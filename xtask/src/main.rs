mod cargo_sources;
mod snapshot;
use playtime_protocol::{
    archive::{BuildProvenance, PlatformRequirements, WorkerIdentity},
    worker_bundle::{self as wire, BuildRecord, BundleDescriptor, Capabilities},
};
use playtime_worker::{
    PrivateDirectory,
    bundle::{EXECUTABLE, LOCK, SOURCE, VerifiedBundle, reference},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn output(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn toolchain_tool(channel: &str, tool: &str) -> Result<PathBuf> {
    Ok(output(Command::new("rustup").args(["which", "--toolchain", channel, tool]))?.into())
}

fn build(root: &Path, destination: &Path) -> Result<()> {
    let destination = std::path::absolute(destination)?;
    if destination.try_exists()? {
        return Err("bundle destination already exists".into());
    }
    let parent = destination.parent().ok_or("missing bundle parent")?;
    let name = destination
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid bundle name")?;
    let parent_dir = PrivateDirectory::open(parent)?;
    let id = playtime_worker_control::random_id()?;
    // Outside the checkout: Cargo walks ancestor .cargo configurations. A
    // target/source directory inside the live checkout would not be isolated.
    let build_root = std::env::temp_dir().join(format!("playtime-worker-build-{}", id.as_str()));
    if build_root.starts_with(root) {
        return Err("worker staging must be outside the live checkout".into());
    }
    for ancestor in build_root.ancestors().skip(1) {
        for name in [".cargo/config", ".cargo/config.toml"] {
            if ancestor.join(name).try_exists()? {
                return Err("worker staging inherits external Cargo configuration".into());
            }
        }
    }
    playtime_worker_control::create_private_dir(&build_root)?;
    let staging = build_root.join("source");
    fs::create_dir(&staging)?;
    eprintln!("Capturing first-party source before compilation...");
    let captured = snapshot::capture(root)?;
    captured.materialize(&staging)?;
    let source = serde_json::to_vec(&captured)?;
    if source.len() as u64 > playtime_storage::StorageLimits::default().content_bytes {
        return Err("source snapshot exceeds archive content bound".into());
    }
    let lock = captured.bytes("Cargo.lock")?;
    let toolchain: toml::Value = toml::from_str(std::str::from_utf8(
        &captured.bytes("rust-toolchain.toml")?,
    )?)?;
    let channel = toolchain["toolchain"]["channel"]
        .as_str()
        .ok_or("missing pinned toolchain")?;
    let cargo = toolchain_tool(channel, "cargo")?;
    let rustc = toolchain_tool(channel, "rustc")?;
    let linker = Path::new("/usr/bin/cc");
    let ld = PathBuf::from(output(Command::new(linker).arg("-print-prog-name=ld"))?);
    let ld = if ld.is_absolute() {
        ld
    } else {
        Path::new("/usr/bin").join(ld)
    };
    let rustflags = vec![
        "-C".into(),
        "target-feature=+crt-static".into(),
        "-C".into(),
        format!("linker={}", linker.display()),
    ];
    let provenance = BuildProvenance {
        rustc_version: output(Command::new(&rustc).arg("-vV"))?,
        target_triple: wire::TARGET.into(),
        cargo_profile: "release".into(),
        source_revision: captured.revision.clone(),
        dirty: captured.dirty,
    };
    let record = BuildRecord {
        schema_version: wire::VERSION,
        source_sha256: playtime_storage::sha256(&source),
        lock_sha256: playtime_storage::sha256(&lock),
        provenance: provenance.clone(),
        cargo_version: output(Command::new(&cargo).arg("-Vv"))?,
        linker_version: format!(
            "{}\n{}",
            output(Command::new(linker).arg("--version"))?,
            output(Command::new(ld).arg("--version"))?
        ),
        features: vec![],
        rustflags: rustflags.clone(),
        capabilities: Capabilities::current(),
    };
    let record_bytes = serde_json::to_vec(&record)?;
    let record_path = build_root.join("build.json");
    fs::write(&record_path, &record_bytes)?;
    let target = build_root.join("target");
    let cargo_home = build_root.join("cargo-home");
    playtime_worker_control::create_private_dir(&cargo_home)?;
    // Isolate Cargo configuration as well as target files. Registry packages may
    // be fetched; the locked first-party snapshot is the only compiled source.
    let cargo_command = || {
        let mut command = Command::new(&cargo);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("RUSTC", &rustc)
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
            .env("PLAYTIME_WORKER_BUILD_RECORD", &record_path)
            .current_dir(&staging);
        command
    };
    eprintln!("Verifying resolved dependencies stay inside the frozen build...");
    cargo_sources::verify(
        &mut cargo_command(),
        &captured,
        &staging,
        &cargo_home,
        wire::TARGET,
    )?;
    let mut command = cargo_command();
    command.args([
        "build",
        "--locked",
        "--release",
        "--target",
        wire::TARGET,
        "-p",
        "playtime-worker",
        "--bin",
        "playtime-worker",
    ]);
    eprintln!("Building staged worker for {}...", wire::TARGET);
    if !command.status()?.success() {
        return Err(format!("staged build failed; install Rust target {} and a C compiler/linker; staging retained at {}", wire::TARGET, build_root.display()).into());
    }
    // Cargo hardlinks its output to its own deps directory. Copy out of this
    // private build tree before applying the bundle's single-link file policy.
    let compiled = target.join(wire::TARGET).join("release/playtime-worker");
    if fs::metadata(&compiled)?.len() > playtime_storage::StorageLimits::default().content_bytes {
        return Err("compiled worker exceeds archive content bound".into());
    }
    fs::copy(compiled, build_root.join("compiled-worker"))?;
    let executable = PrivateDirectory::open(&build_root)?.read(
        "compiled-worker",
        playtime_storage::StorageLimits::default().content_bytes,
    )?;
    playtime_worker::verify_static_elf(&executable)?;
    let platform = PlatformRequirements {
        os: "linux".into(),
        architecture: "x86_64".into(),
        abi: wire::TARGET.into(),
        requirements: vec![
            "Linux /proc and durable local filesystem; executable service-private filesystem"
                .into(),
            "CPU-only controller simulation; no real model/workshop adapters".into(),
        ],
    };
    let descriptor = BundleDescriptor {
        schema_version: wire::VERSION,
        worker: WorkerIdentity {
            executable: reference(EXECUTABLE, &executable),
            source: reference(SOURCE, &source),
            cargo_lock: reference(LOCK, &lock),
            build: provenance,
            platform,
        },
        build_record: reference(wire::BUILD, &record_bytes),
        capabilities: record.capabilities,
    };
    let temporary = format!(".worker-bundle-{}", id.as_str());
    let bundle = parent_dir.create(&temporary)?;
    for (name, bytes) in [
        (EXECUTABLE, executable),
        (SOURCE, source),
        (LOCK, lock),
        (wire::BUILD, record_bytes),
        ("bundle.json", serde_json::to_vec(&descriptor)?),
    ] {
        bundle.write_new(name, &bytes, false)?;
    }
    let validated = VerifiedBundle::open(&parent.join(&temporary))?;
    // Publication is no-replace and happens only after complete validation.
    parent_dir.publish_directory(&temporary, name)?;
    println!(
        "Bundle {} published to {}",
        validated.id(),
        destination.display()
    );
    // Only our uniquely named scratch build is disposable. Never touch archives.
    fs::remove_dir_all(&build_root)?;
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 || args[0] != "bundle-worker" || args[1] != "--output" {
        return Err(
            "usage: cargo run --locked -p xtask -- bundle-worker --output <new-directory>".into(),
        );
    }
    build(
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
        Path::new(&args[2]),
    )
}
