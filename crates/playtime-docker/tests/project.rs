//! Synthetic lifecycle faults and real local admission policy; no Docker runs.
mod support;
use playtime_docker::{project::*, *};
use playtime_local::PrivateDirectory;
use serde_json::json;
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use support::*;

struct Tree {
    root: PathBuf,
    project: PathBuf,
    private: PathBuf,
}
impl Tree {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "playtime-project-fixture-{}",
            Ownership::random_nonce().unwrap()
        ));
        PrivateDirectory::open(&root, true).unwrap();
        let project = root.join("project");
        let private = root.join("private");
        fs::create_dir(&project).unwrap();
        PrivateDirectory::open(&private, true).unwrap();
        Self {
            root,
            project,
            private,
        }
    }
    fn open(&self) -> std::io::Result<ProjectDirectory> {
        ProjectDirectory::open(&self.project, &self.private, &[])
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn limits() -> ProjectLimits {
    ProjectLimits {
        nano_cpus: 1_000_000_000,
        memory_bytes: 1024 * 1024 * 1024,
        tmp_bytes: 16 * 1024 * 1024,
        tmp_inodes: 16384,
        pids: 64,
        seed: 1042,
    }
}
async fn workspace(f: &Fixture, directory: Arc<ProjectDirectory>) -> ProjectWorkspace {
    let ready = f
        .engine
        .readiness(&lock(), deadline(), CancellationToken::new())
        .await
        .unwrap();
    ProjectWorkspace::new(
        ready,
        Ownership {
            service: "synthetic-project".into(),
            run: "run".into(),
            grant: "grant".into(),
            invocation: "invocation".into(),
            nonce: Ownership::random_nonce().unwrap(),
            daemon: f.engine.identity().clone(),
        },
        directory,
        limits(),
        WorkDeadline::after(Duration::from_secs(120)).unwrap(),
        f.journal.clone(),
    )
    .unwrap()
}
async fn tool(w: &mut ProjectWorkspace) -> Result<ToolReply> {
    w.tool(
        Tool::Exec {
            command: "synthetic".into(),
            cwd: None,
            timeout_seconds: None,
        },
        ToolLimits {
            command_seconds: 1.0,
            output_bytes: 4096,
        },
        deadline(),
        CancellationToken::new(),
    )
    .await
}
#[test]
fn admission_rejects_external_hardlinks_and_host_sockets_but_accepts_internal_links() {
    let t = Tree::new();
    fs::write(t.root.join("outside"), "private").unwrap();
    fs::hard_link(t.root.join("outside"), t.project.join("linked")).unwrap();
    assert!(t.open().is_err());
    fs::remove_file(t.project.join("linked")).unwrap();
    fs::write(t.project.join("a"), "internal").unwrap();
    fs::hard_link(t.project.join("a"), t.project.join("b")).unwrap();
    std::os::unix::fs::symlink(t.root.join("outside"), t.project.join("symlink")).unwrap();
    drop(t.open().unwrap());
    fs::hard_link(t.project.join("symlink"), t.root.join("outside-symlink")).unwrap();
    assert!(t.open().is_err());
    fs::remove_file(t.root.join("outside-symlink")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(t.project.join("socket")).unwrap();
    assert!(t.open().is_err());
}
#[test]
fn private_state_and_protected_paths_cannot_be_inside_project() {
    let t = Tree::new();
    let private = t.project.join("private");
    fs::create_dir(&private).unwrap();
    assert!(ProjectDirectory::open(&t.project, &private, &[]).is_err());
    assert!(ProjectDirectory::open(&t.project, &t.private, &[private]).is_err());
}
#[tokio::test]
async fn journal_failure_never_creates_and_cleanup_releases_reservation() {
    let t = Tree::new();
    let f = Fixture::new().await;
    let mut w = workspace(&f, Arc::new(t.open().unwrap())).await;
    *f.journal.fail.lock().unwrap() = true;
    assert_eq!(
        w.start_workspace(deadline(), CancellationToken::new())
            .await
            .unwrap_err()
            .failure,
        Failure::Journal
    );
    assert!(
        !f.fake
            .seen()
            .iter()
            .any(|op| matches!(op, transport::Operation::CreateProject(_)))
    );
    assert!(
        w.cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
    drop(w);
    drop(t.open().unwrap());
}
#[tokio::test]
async fn dropped_owner_retains_durable_project_fence() {
    let t = Tree::new();
    let f = Fixture::new().await;
    let w = workspace(&f, Arc::new(t.open().unwrap())).await;
    drop(w);
    assert!(t.open().is_err());
}
#[tokio::test]
async fn lost_create_absence_is_not_cleanup_until_exact_publication_is_seen() {
    let t = Tree::new();
    let f = Fixture::new().await;
    let mut w = workspace(&f, Arc::new(t.open().unwrap())).await;
    f.fake.inject(
        "project_create",
        Injection::Lost(transport::Fault::Deadline),
    );
    assert!(
        w.start_workspace(deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    let published = std::mem::take(&mut f.fake.state.lock().unwrap().containers);
    assert!(
        !w.cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
    f.fake.state.lock().unwrap().containers = published;
    assert!(
        w.cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
    assert!(tool(&mut w).await.is_err());
    drop(w);
    drop(t.open().unwrap());
}
#[tokio::test]
async fn uncertain_tool_fences_without_replay_then_exact_cleanup_reopens_directory() {
    let t = Tree::new();
    let f = Fixture::new().await;
    let mut w = workspace(&f, Arc::new(t.open().unwrap())).await;
    w.start_workspace(deadline(), CancellationToken::new())
        .await
        .unwrap();
    f.fake
        .inject("tool", Injection::Lost(transport::Fault::Cancelled));
    assert!(tool(&mut w).await.is_err());
    let count = f.fake.seen().len();
    assert!(w.is_fenced() && tool(&mut w).await.is_err());
    assert_eq!(f.fake.seen().len(), count);
    assert!(
        w.cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
    assert!(f.fake.state.lock().unwrap().containers.is_empty());
    assert!(f.fake.state.lock().unwrap().volumes.is_empty());
    drop(w);
    drop(t.open().unwrap());
}
#[tokio::test]
async fn changed_mount_or_network_never_executes_or_removes_unverified_container() {
    for (pointer, value) in [
        ("/HostConfig/Mounts/0/Source", json!("/other")),
        (
            "/HostConfig/Mounts/0/BindOptions/NonRecursive",
            json!(false),
        ),
        ("/HostConfig/NetworkMode", json!("host")),
        ("/Mounts/0/RW", json!(false)),
        (
            "/Config/Labels/io.playtime.project",
            json!("another-project"),
        ),
    ] {
        let t = Tree::new();
        let f = Fixture::new().await;
        let mut w = workspace(&f, Arc::new(t.open().unwrap())).await;
        w.start_workspace(deadline(), CancellationToken::new())
            .await
            .unwrap();
        f.fake.state.lock().unwrap().mutate = Some((pointer, value));
        assert!(tool(&mut w).await.is_err(), "{pointer}");
        assert!(
            !w.cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
        assert!(!f.fake.seen().iter().any(|op| matches!(
            op,
            transport::Operation::Bridge { .. } | transport::Operation::RemoveContainer(_)
        )));
        f.fake.state.lock().unwrap().mutate = None;
        assert!(
            w.cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
    }
}
#[tokio::test]
async fn replacing_host_path_fences_tools_but_still_removes_exact_owned_container() {
    let t = Tree::new();
    let f = Fixture::new().await;
    let mut w = workspace(&f, Arc::new(t.open().unwrap())).await;
    w.start_workspace(deadline(), CancellationToken::new())
        .await
        .unwrap();
    fs::rename(&t.project, t.root.join("moved")).unwrap();
    fs::create_dir(&t.project).unwrap();
    assert_eq!(
        tool(&mut w).await.unwrap_err().failure,
        Failure::ChangedIdentity
    );
    assert!(
        w.cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
}
