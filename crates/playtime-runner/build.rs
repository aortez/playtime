use std::{collections::BTreeMap, env, fs, path::Path, process::Command};

fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<String, serde_json::Value>) {
    println!("cargo:rerun-if-changed={}", directory.display());
    for entry in fs::read_dir(directory).expect("read source directory") {
        let entry = entry.expect("source entry");
        let path = entry.path();
        let kind = entry.file_type().expect("source entry type");
        if kind.is_dir() {
            collect(root, &path, files);
        } else if kind.is_file() {
            include(root, &path, files);
        } else {
            panic!("source bundle does not follow links: {}", path.display());
        }
    }
}

fn include(root: &Path, path: &Path, files: &mut BTreeMap<String, serde_json::Value>) {
    let bytes = fs::read(path).expect("read build source");
    let content = match String::from_utf8(bytes) {
        Ok(text) => serde_json::json!({"encoding":"utf8", "text":text}),
        Err(error) => serde_json::json!({"encoding":"bytes", "bytes":error.into_bytes()}),
    };
    files.insert(
        path.strip_prefix(root).unwrap().to_str().unwrap().into(),
        content,
    );
    println!("cargo:rerun-if-changed={}", path.display());
}

fn output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let manifest = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).parent().unwrap().parent().unwrap();
    let destination = env::var_os("OUT_DIR").unwrap();
    let destination = Path::new(&destination);
    let mut files = BTreeMap::new();
    // Freeze the compiling workspace's source, including include_bytes! inputs.
    // No run records, image archives, or unrelated checkout files are bundled.
    for name in [
        "playtime-core",
        "playtime-protocol",
        "playtime-engine",
        "playtime-storage",
        "playtime-runner",
        "playtime-docker",
        "playtime-ollama",
    ] {
        collect(root, &root.join("crates").join(name), &mut files);
    }
    include(root, &root.join("Cargo.toml"), &mut files);
    include(root, &root.join("Cargo.lock"), &mut files);
    fs::write(
        destination.join("source.json"),
        serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"format":"source_files","capture_method":"demo_cargo_build_script_live_workspace_not_isolated","files":files}),
        )
        .unwrap(),
    )
    .unwrap();
    fs::copy(root.join("Cargo.lock"), destination.join("Cargo.lock")).unwrap();
    let revision = output(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"]),
    );
    let dirty = output(Command::new("git").current_dir(root).args([
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--",
        "Cargo.toml",
        "Cargo.lock",
        "crates",
    ]))
    .is_none_or(|status| !status.is_empty());
    for name in ["HEAD", "index"] {
        if let Some(path) =
            output(
                Command::new("git")
                    .current_dir(root)
                    .args(["rev-parse", "--git-path", name]),
            )
        {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    if let Some(reference) =
        output(
            Command::new("git")
                .current_dir(root)
                .args(["symbolic-ref", "-q", "HEAD"]),
        )
        && let Some(path) = output(Command::new("git").current_dir(root).args([
            "rev-parse",
            "--git-path",
            &reference,
        ]))
    {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let rustc =
        output(Command::new(env::var_os("RUSTC").unwrap()).arg("-vV")).expect("rustc version");
    let build = serde_json::json!({"rustc_version":rustc,"target_triple":env::var("TARGET").unwrap(),"cargo_profile":env::var("PROFILE").unwrap(),"source_revision":revision,"dirty":dirty});
    fs::write(
        destination.join("build.json"),
        serde_json::to_vec(&build).unwrap(),
    )
    .unwrap();
    println!(
        "cargo:rustc-env=PLAYTIME_WORKER_TARGET={}",
        env::var("TARGET").unwrap()
    );
}
