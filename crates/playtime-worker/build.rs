use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-env-changed=PLAYTIME_WORKER_BUILD_RECORD");
    let bytes = match env::var_os("PLAYTIME_WORKER_BUILD_RECORD") {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", PathBuf::from(&path).display());
            fs::read(path).expect("read selected staged build record")
        }
        None => b"null".to_vec(), // Native tests are not production bundles.
    };
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("build.json"),
        bytes,
    )
    .unwrap();
}
