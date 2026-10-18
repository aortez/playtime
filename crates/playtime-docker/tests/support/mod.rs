//! Explicitly synthetic daemon state. The container fixture was captured from
//! this gate's own Docker 28.4.0 holder, then stripped of host/run identities.
//! No private experiment archive, controller or experimental command executes.
#![allow(dead_code)]
use playtime_docker::{transport::*, *};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

pub fn info() -> Value {
    json!({"ID":"synthetic-daemon","Name":"synthetic-host","ServerVersion":"28.4.0","KernelVersion":"synthetic-linux","OSType":"linux","Architecture":"x86_64","OperatingSystem":"synthetic Linux","CgroupVersion":"2","MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CpuCfsPeriod":true,"PidsLimit":true,"SecurityOptions":["name=seccomp,profile=builtin"],"Plugins":{"Volume":["local"]}})
}
pub fn version() -> Value {
    json!({"Client":{"Version":"28.4.0","Os":"linux","Arch":"amd64"},"Server":{"Version":"28.4.0","ApiVersion":"1.51","Os":"linux","Arch":"amd64"}})
}
pub fn lock() -> Vec<u8> {
    serde_json::to_vec(&json!({"schema":1,"image_id":format!("sha256:{}","a".repeat(64)),"os":"linux","architecture":"amd64","build_sources":{"tool.py":BRIDGE_SHA256}})).unwrap()
}
pub fn response(value: Value) -> Outcome {
    Outcome::completed(serde_json::to_vec(&value).unwrap())
}
pub fn absent(kind: &str) -> Outcome {
    Outcome {
        dispatch: Dispatch::MayHaveReachedDaemon,
        local: LocalExit::Reaped {
            code: Some(1),
            signal: None,
        },
        fault: None,
        stdout: b"[]\n".to_vec(),
        stderr: format!("Error response from daemon: No such {kind}: synthetic").into_bytes(),
    }
}
pub fn lost(fault: Fault) -> Outcome {
    Outcome {
        dispatch: Dispatch::MayHaveReachedDaemon,
        local: LocalExit::Reaped {
            code: None,
            signal: Some(9),
        },
        fault: Some(fault),
        stdout: vec![],
        stderr: vec![],
    }
}
pub fn container(p: &Proposal, id: &str, running: bool) -> Value {
    let mut v: Value = serde_json::from_str(include_str!("../fixtures/container.json")).unwrap();
    v["Id"] = json!(id);
    v["Name"] = json!(format!("/{}", p.name));
    v["Image"] = json!(p.image);
    v["Config"]["Image"] = json!(p.image);
    v["Config"]["Labels"] = json!(p.labels());
    v["Config"]["Cmd"] = json!([
        "-I",
        "-S",
        "-B",
        "-c",
        TIMER,
        p.deadline_monotonic_ns.to_string()
    ]);
    let l = &p.limits;
    v["HostConfig"]["Memory"] = json!(if p.role == Role::Holder {
        67108864
    } else {
        l.memory_bytes
    });
    v["HostConfig"]["MemorySwap"] = v["HostConfig"]["Memory"].clone();
    v["HostConfig"]["NanoCpus"] = json!(l.nano_cpus);
    v["HostConfig"]["PidsLimit"] = json!(if p.role == Role::Holder { 64 } else { l.pids });
    v["HostConfig"]["ShmSize"] = json!(l.tmp_bytes);
    v["HostConfig"]["Tmpfs"] = json!({"/tmp":format!("rw,nosuid,nodev,size={},mode=1777,nr_inodes={}",l.tmp_bytes,l.workspace_inodes)});
    v["Config"]["Env"] = json!([
        format!("PLAYTIME_SEED={}", l.seed),
        format!("PYTHONHASHSEED={}", l.seed)
    ]);
    v["HostConfig"]["Mounts"][0]["Source"] = json!(p.volume);
    v["Mounts"][0]["Name"] = json!(p.volume);
    v["State"]["Running"] = json!(running);
    v["State"]["Status"] = json!(if running { "running" } else { "created" });
    v
}
pub fn volume(p: &Proposal) -> Value {
    let l = &p.limits;
    json!({"Name":p.name,"Driver":"local","Scope":"local","Labels":p.labels(),"Options":{"type":"tmpfs","device":"tmpfs","o":format!("size={},uid=1000,gid=1000,mode=0700,nr_inodes={}",l.workspace_bytes,l.workspace_inodes)}})
}
#[derive(Default)]
pub struct Journal {
    pub proposals: Mutex<Vec<Proposal>>,
    pub fail: Mutex<bool>,
}
#[async_trait::async_trait]
impl OwnershipJournal for Journal {
    async fn commit(&self, p: &Proposal) -> Result<()> {
        if *self.fail.lock().unwrap() {
            return Err(Error {
                failure: Failure::Journal,
                operation: "synthetic_journal",
                detail: "injected",
            });
        }
        self.proposals.lock().unwrap().push(p.clone());
        Ok(())
    }
}
#[derive(Clone)]
pub enum Injection {
    Lost(Fault),
    Malformed,
    ReplaceContainer,
    RefuseRemove,
}
pub struct State {
    pub seen: Vec<Operation>,
    pub deadlines: Vec<Instant>,
    pub volumes: BTreeMap<String, Value>,
    pub containers: BTreeMap<String, Value>,
    pub available: bool,
    pub changed: bool,
    pub fault: Option<(&'static str, Injection)>,
    pub mutate: Option<(&'static str, Value)>,
    pub next_id: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            seen: vec![],
            deadlines: vec![],
            volumes: BTreeMap::new(),
            containers: BTreeMap::new(),
            available: true,
            changed: false,
            fault: None,
            mutate: None,
            next_id: 1,
        }
    }
}
pub struct Fake {
    pub state: Mutex<State>,
    pub journal: Arc<Journal>,
}
impl Fake {
    pub fn seen(&self) -> Vec<Operation> {
        self.state.lock().unwrap().seen.clone()
    }
    pub fn inject(&self, op: &'static str, injection: Injection) {
        self.state.lock().unwrap().fault = Some((op, injection));
    }
    pub fn resources(&self) -> usize {
        let s = self.state.lock().unwrap();
        s.volumes.len() + s.containers.len()
    }
}
#[async_trait::async_trait]
impl Transport for Fake {
    async fn execute(
        &self,
        op: Operation,
        deadline: Instant,
        _cancel: CancellationToken,
    ) -> Outcome {
        let mut s = self.state.lock().unwrap();
        s.seen.push(op.clone());
        s.deadlines.push(deadline);
        if !s.available {
            return lost(Fault::Io);
        }
        let injection = if s.fault.as_ref().is_some_and(|(name, _)| *name == op.name()) {
            s.fault.take().map(|(_, i)| i)
        } else {
            None
        };
        if let Some(Injection::RefuseRemove) = injection {
            return lost(Fault::Io);
        }
        let result = match &op {
            Operation::Version => response(version()),
            Operation::Info => {
                let mut v = info();
                if s.changed {
                    v["ID"] = json!("other-daemon");
                }
                response(v)
            }
            Operation::ImageInspect(id) => response(
                json!([{"Id":id,"Os":"linux","Architecture":"amd64","Config":{"Volumes":null,"OnBuild":null}}]),
            ),
            Operation::CreateVolume(p) => {
                assert!(
                    self.journal.proposals.lock().unwrap().contains(p),
                    "mutation before durable intent"
                );
                s.volumes.entry(p.name.clone()).or_insert_with(|| volume(p));
                Outcome::completed(p.name.as_bytes().to_vec())
            }
            Operation::CreateContainer(p) => {
                assert!(
                    self.journal.proposals.lock().unwrap().contains(p),
                    "mutation before durable intent"
                );
                let id = format!("{:064x}", s.next_id);
                s.next_id += 1;
                s.containers.insert(id.clone(), container(p, &id, false));
                Outcome::completed(id.into_bytes())
            }
            Operation::Start(id) => {
                let v = s.containers.get_mut(id).unwrap();
                v["State"]["Running"] = json!(true);
                v["State"]["Status"] = json!("running");
                v["HostConfig"]["OomKillDisable"] = Value::Null;
                Outcome::completed(id.as_bytes().to_vec())
            }
            Operation::ContainerInspect(id) => {
                let v = s.containers.get(id).cloned().or_else(|| {
                    s.containers
                        .values()
                        .find(|v| v["Name"] == format!("/{id}"))
                        .cloned()
                });
                match v {
                    Some(mut v) => {
                        if let Some((pointer, value)) = &s.mutate {
                            *v.pointer_mut(pointer).expect("fixture pointer") = value.clone();
                        }
                        response(json!([v]))
                    }
                    None => absent("container"),
                }
            }
            Operation::VolumeInspect(name) => s
                .volumes
                .get(name)
                .map(|v| response(json!([v])))
                .unwrap_or_else(|| absent("volume")),
            Operation::Probe(_) => response(
                json!({"python":PYTHON,"bridge_sha256":BRIDGE_SHA256,"uid":1000,"gid":1000}),
            ),
            Operation::Bridge { payload, .. } => {
                let v: Value = serde_json::from_slice(payload).unwrap();
                match v["name"].as_str().unwrap() {
                    "exec" => response(
                        json!({"stdout":"","stderr":"","exit_code":0,"timed_out":false,"output_truncated":false,"duration_seconds":0.0}),
                    ),
                    "write_file" => response(
                        json!({"path":"/work/file","bytes_written":v["arguments"]["content"].as_str().unwrap().len()}),
                    ),
                    _ => response(json!({"content":"","offset_bytes":0,"truncated":false})),
                }
            }
            Operation::Mounts(volume) => Outcome::completed(
                s.containers
                    .iter()
                    .filter(|(_, v)| v["Mounts"][0]["Name"] == *volume)
                    .map(|(id, _)| format!("{id}\n"))
                    .collect::<String>()
                    .into_bytes(),
            ),
            Operation::RemoveContainer(id) => {
                s.containers.remove(id);
                Outcome::completed(id.as_bytes().to_vec())
            }
            Operation::RemoveVolume(name) => {
                if !s
                    .containers
                    .values()
                    .any(|v| v["Mounts"][0]["Name"] == *name)
                {
                    s.volumes.remove(name);
                }
                Outcome::completed(name.as_bytes().to_vec())
            }
        };
        match injection {
            Some(Injection::Lost(fault)) => lost(fault),
            Some(Injection::Malformed) => Outcome::completed(b"not JSON or ID".to_vec()),
            Some(Injection::ReplaceContainer) => {
                if let Operation::ContainerInspect(id) = op {
                    s.containers.remove(&id);
                }
                result
            }
            _ => result,
        }
    }
}
pub struct Fixture {
    pub fake: Arc<Fake>,
    pub journal: Arc<Journal>,
    pub engine: Engine,
    pub sandbox: Sandbox,
}
impl Fixture {
    pub async fn new() -> Self {
        let journal = Arc::new(Journal::default());
        let fake = Arc::new(Fake {
            state: Mutex::new(State::default()),
            journal: journal.clone(),
        });
        let engine = Engine::connect(fake.clone(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let ready = engine
            .readiness(&lock(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let owner = Ownership {
            service: "synthetic-service".into(),
            run: "synthetic-run".into(),
            grant: "synthetic-grant".into(),
            invocation: "synthetic-invocation".into(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            daemon: engine.identity().clone(),
        };
        let sandbox = Sandbox::new(
            ready,
            owner,
            Limits::smoke(),
            WorkDeadline::after(Duration::from_secs(120)).unwrap(),
            journal.clone(),
        )
        .unwrap();
        Self {
            fake,
            journal,
            engine,
            sandbox,
        }
    }
    pub async fn initialized() -> Self {
        let mut f = Self::new().await;
        f.sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap();
        f
    }
    pub async fn running() -> Self {
        let mut f = Self::initialized().await;
        f.sandbox
            .start_session(1, deadline(), CancellationToken::new())
            .await
            .unwrap();
        f
    }
    pub async fn tool(&mut self) -> Result<ToolReply> {
        self.sandbox
            .tool(
                Tool::Exec {
                    command: "model text is data only".into(),
                    cwd: None,
                    timeout_seconds: None,
                },
                ToolLimits {
                    command_seconds: 5.0,
                    output_bytes: 4096,
                },
                deadline(),
                CancellationToken::new(),
            )
            .await
    }
    pub async fn cleanup(&mut self) -> Cleanup {
        self.sandbox
            .cleanup(deadline(), CancellationToken::new())
            .await
    }
}
pub fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(20)
}
