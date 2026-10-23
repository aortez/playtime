//! Reviewed recipes as data (#74). A catalog recipe is checked against the
//! compiled bounds below, then frozen with the live model into a run's profile.
//! The worker accepts any frozen recipe within the same bounds.
use super::{
    IMAGE_LOCK, LIVE_SCENARIO, LiveHost, LiveModel, MODEL_LOCK, Profile, RETURN_OVERHEAD_MS,
};
use crate::{Error, Result};
use playtime_core::{MemoryCondition, RunMode, RunParameters};
use playtime_docker::{DaemonIdentity, Limits, capture::CaptureLimits, transport::Selection};
use playtime_engine::simulation::Scenario;
use playtime_storage::StorageLimits;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const RECIPE_PROFILE: &str = "docker_live_recipe_v1";

/// A catalog recipe exactly as it is frozen into a run's archive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub protocol: String,
    pub inventory: String,
    pub parameters: RunParameters,
    /// Files and directories `/work` may hold.
    pub workspace_entries: u32,
    pub model: Sampling,
    /// Paths under `/work/seeds` and their UTF-8 contents, written there before
    /// session 1.
    #[serde(default)]
    pub seeds: BTreeMap<String, String>,
}

/// The live request settings a recipe chooses. The model lock is fixed, the
/// context size comes from the limits and the controller advances the seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sampling {
    pub thinking: String,
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: u32,
    pub repeat_penalty: f64,
}

/// Upper bounds on catalog recipes, on top of the controller's own checks.
/// Bundles advertise them in their profile digest.
#[derive(Serialize)]
pub struct Bounds {
    pub sessions: u32,
    pub work_turns_per_session: u32,
    pub wall_seconds: u64,
    pub generated_tokens: u64,
    pub tokens_per_call: u64,
    pub handoff_tokens: u64,
    pub context_tokens: u64,
    pub model_seconds: u64,
    pub command_seconds: u64,
    pub cpu_cores: f64,
    pub memory_bytes: u64,
    /// Keeps a full workspace's capture tar within the capture's 64 MiB.
    pub workspace_bytes: u64,
    pub workspace_entries: u32,
    pub tmp_bytes: u64,
    pub pids: u32,
    pub thinking: [&'static str; 3],
    pub text_bytes: usize,
    pub name_bytes: usize,
    pub seed_files: usize,
    /// The bridge writes at most this many bytes per call.
    pub seed_file_bytes: usize,
    /// JSON-escaped, so each write fits one bridge request.
    pub seed_encoded_bytes: usize,
    pub seed_bytes: usize,
    pub seed_path_bytes: usize,
}
pub const BOUNDS: Bounds = Bounds {
    sessions: 20,
    work_turns_per_session: 50,
    wall_seconds: 4 * 3600,
    generated_tokens: 1_000_000,
    tokens_per_call: 16_384,
    handoff_tokens: 16_384,
    context_tokens: 131_072,
    model_seconds: 600,
    command_seconds: 300,
    cpu_cores: 16.0,
    memory_bytes: 32 << 30,
    workspace_bytes: 48 << 20,
    workspace_entries: 4096,
    tmp_bytes: 1 << 30,
    pids: 1024,
    thinking: ["low", "medium", "high"],
    text_bytes: 65_536,
    name_bytes: 128,
    seed_files: 16,
    seed_file_bytes: 131_072,
    seed_encoded_bytes: 196_608,
    seed_bytes: 524_288,
    seed_path_bytes: 200,
};
/// Recipe ids the daemon already uses for its built-in runs.
const RESERVED: [&str; 3] = ["two-session", super::RECIPE, super::LIVE_RECIPE];

impl Recipe {
    /// Checks the bounds, including that every session's capture of a full
    /// workspace fits the archive.
    pub fn check(&self) -> Result<()> {
        let invalid = |what: &str| Err(Error::Invalid(format!("recipe {}: {what}", self.id)));
        let (b, p) = (&BOUNDS, &self.parameters);
        let (l, w) = (&p.limits, &p.workshop);
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || RESERVED.contains(&self.id.as_str())
        {
            return invalid("the id must be 1 to 64 of a-z, 0-9 and -, and not a built-in id");
        }
        if self.name.is_empty()
            || self.name.len() > b.name_bytes
            || self.name.chars().any(char::is_control)
            || [&self.prompt, &self.protocol, &self.inventory]
                .iter()
                .any(|text| text.trim().is_empty() || text.len() > b.text_bytes)
        {
            return invalid(
                "the name, prompt, protocol and inventory must be non-empty and bounded",
            );
        }
        p.validate()
            .map_err(|e| Error::Invalid(format!("recipe {}: {}", self.id, e.0)))?;
        if p.memory_condition != MemoryCondition::SelfWritten {
            return invalid("only the self memory condition is supported");
        }
        if l.sessions.get() > b.sessions
            || l.work_turns_per_session.get() > b.work_turns_per_session
            || l.wall_seconds.get() > b.wall_seconds
            || l.generated_tokens.get() > b.generated_tokens
            || l.tokens_per_call.get() > b.tokens_per_call
            || l.handoff_tokens.get() > b.handoff_tokens
            || l.context_tokens.get() > b.context_tokens
            || l.model_seconds.get() > b.model_seconds
            || l.command_seconds.get() > b.command_seconds
            || l.log_bytes.get() > StorageLimits::default().event_log_bytes
        {
            return invalid("a run limit exceeds its bound");
        }
        // The controller refuses a command cap that leaves no time to run.
        if l.command_seconds.get().saturating_mul(1000) <= RETURN_OVERHEAD_MS {
            return invalid("command_seconds must exceed the command return allowance");
        }
        if w.cpu_cores > b.cpu_cores
            || !(64 << 20..=b.memory_bytes).contains(&w.memory_bytes.get())
            || w.workspace_bytes.get() > b.workspace_bytes
            || !(16..=b.workspace_entries).contains(&self.workspace_entries)
            || w.tmp_bytes.get() > b.tmp_bytes
            || !(16..=b.pids).contains(&w.pids.get())
        {
            return invalid("a workshop limit exceeds its bound");
        }
        if !fits_archive(
            l.sessions.get(),
            w.workspace_bytes.get(),
            self.workspace_entries,
        ) {
            return invalid("the archive can't hold a full workspace for every session");
        }
        let m = &self.model;
        let sampling = b.thinking.contains(&m.thinking.as_str())
            && (0.0..=2.0).contains(&m.temperature)
            && m.top_p > 0.0
            && m.top_p <= 1.0
            && (1..=1000).contains(&m.top_k)
            && m.repeat_penalty > 0.0
            && m.repeat_penalty <= 2.0;
        if !sampling {
            return invalid("a model setting is outside its bounds");
        }
        self.check_seeds(w.workspace_bytes.get())
    }
    fn check_seeds(&self, workspace_bytes: u64) -> Result<()> {
        let invalid = |what: &str| Err(Error::Invalid(format!("recipe {}: {what}", self.id)));
        let b = &BOUNDS;
        let mut directories = BTreeSet::from(["seeds".to_owned()]);
        let mut total = 0;
        for (path, content) in &self.seeds {
            let parts: Vec<_> = path.split('/').collect();
            if path.len() > b.seed_path_bytes
                || parts.iter().any(|part| {
                    part.is_empty()
                        || *part == "."
                        || *part == ".."
                        || part.chars().any(char::is_control)
                })
            {
                return invalid("a seed path must be relative, plain and bounded");
            }
            if self
                .seeds
                .keys()
                .any(|other| other.starts_with(&format!("{path}/")))
            {
                return invalid("a seed path is both a file and a directory");
            }
            if content.len() > b.seed_file_bytes
                || serde_json::to_string(content)?.len() > b.seed_encoded_bytes
            {
                return invalid("a seed file exceeds its bound");
            }
            total += content.len();
            for depth in 1..parts.len() {
                directories.insert(format!("seeds/{}", parts[..depth].join("/")));
            }
        }
        // Seeds leave the agent at least half of the workspace.
        let entries = (self.seeds.len() + directories.len()) as u64;
        if self.seeds.len() > b.seed_files
            || total > b.seed_bytes
            || total as u64 * 2 > workspace_bytes
            || entries * 2 > u64::from(self.workspace_entries)
        {
            return invalid("the seeds exceed their bounds");
        }
        Ok(())
    }
    /// A fixed recipe whose frozen template goes into the profile digest, so a
    /// bundle and a daemon agree on how recipes freeze.
    pub(super) fn reference() -> Result<Self> {
        let scenario: Scenario = serde_json::from_slice(LIVE_SCENARIO)?;
        Ok(Self {
            id: "reference".into(),
            name: "Reference".into(),
            prompt: scenario.prompt,
            protocol: scenario.protocol,
            inventory: scenario.inventory,
            parameters: scenario.parameters,
            workspace_entries: 512,
            model: Sampling {
                thinking: "high".into(),
                temperature: 0.8,
                top_p: 0.95,
                top_k: 40,
                repeat_penalty: 1.0,
            },
            seeds: BTreeMap::from([("reference.txt".into(), "reference".into())]),
        })
    }
}

/// Every session's capture of a full workspace fits the archive's verified
/// content and references. A capture stores the tar and each retained file, so
/// it costs about twice the workspace, plus headers and the index. The rest
/// covers the frozen inputs and publication headroom. Readers open archives
/// with the default storage limits, so a recipe can't raise them.
fn fits_archive(sessions: u32, workspace_bytes: u64, entries: u32) -> bool {
    let storage = StorageLimits::default();
    let (sessions, entries) = (u64::from(sessions), u64::from(entries));
    sessions * (2 * workspace_bytes + entries * 2048 + (64 << 10)) + (32 << 20)
        <= storage.verified_content_bytes
        && sessions * (entries + 8) + 64 <= storage.references
}

/// Frozen request settings: the sampling choices, the context size and the
/// fixed keep-alive.
pub(super) fn settings(context_tokens: u64, sampling: &Sampling) -> serde_json::Value {
    serde_json::json!({
        "context_tokens": context_tokens,
        "temperature": sampling.temperature,
        "top_p": sampling.top_p,
        "top_k": sampling.top_k,
        "repeat_penalty": sampling.repeat_penalty,
        "thinking": sampling.thinking,
        "use_mmap": null,
        "keep_alive": "5m",
    })
}

impl Profile {
    /// A checked catalog recipe, run as an experiment on the live model in the
    /// locked workshop. Its limits come from the recipe, and its capture and
    /// storage limits follow from them.
    pub fn freeze_recipe(
        recipe: Recipe,
        selection: Selection,
        daemon: DaemonIdentity,
        host: LiveHost,
    ) -> Result<Self> {
        recipe.check()?;
        let p = recipe.parameters.clone();
        let (l, w) = (&p.limits, &p.workshop);
        Ok(Self {
            version: 1,
            profile: RECIPE_PROFILE.into(),
            mode: RunMode::Experiment,
            scenario: Scenario {
                schema_version: 1,
                mode: RunMode::Experiment,
                parameters: p.clone(),
                protocol: recipe.protocol.clone(),
                prompt: recipe.prompt.clone(),
                inventory: recipe.inventory.clone(),
                initial_files: BTreeMap::new(),
                models: vec![],
                execs: vec![],
                cancel_at_ms: None,
                lifecycle: Default::default(),
                fail_record_sequence: None,
            },
            selection,
            daemon,
            image_lock: serde_json::from_slice(IMAGE_LOCK)?,
            helpers: playtime_docker::capture::helper_digests(),
            resources: Limits {
                nano_cpus: (w.cpu_cores * 1e9).round() as u64,
                memory_bytes: w.memory_bytes.get(),
                workspace_bytes: w.workspace_bytes.get(),
                workspace_inodes: recipe.workspace_entries,
                tmp_bytes: w.tmp_bytes.get(),
                pids: w.pids.get(),
                seed: p.sampling_seed,
            },
            capture: CaptureLimits {
                entries: u64::from(recipe.workspace_entries),
                file_bytes: w.workspace_bytes.get(),
                retained_bytes: w.workspace_bytes.get(),
                memory_bytes: l.memento_bytes.get(),
                ..CaptureLimits::default()
            },
            storage: StorageLimits {
                event_log_bytes: l.log_bytes.get(),
                ..StorageLimits::default()
            },
            command_return_ms: RETURN_OVERHEAD_MS,
            model: Some(LiveModel {
                lock: serde_json::from_slice(MODEL_LOCK)?,
                settings: settings(l.context_tokens.get(), &recipe.model),
                host,
            }),
            recipe: Some(recipe),
            definition: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};
    fn selection() -> Selection {
        Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        }
    }
    fn daemon() -> DaemonIdentity {
        DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        }
    }
    fn host() -> LiveHost {
        LiveHost {
            endpoint: "http://model-host.local:11434".into(),
            addresses: vec!["192.0.2.10".parse().unwrap()],
            live_lock: "/run/user/1000/playtime/live-model.lock".into(),
        }
    }
    /// The owner-confirmed curiosity limits, with small seeds.
    fn recipe() -> Recipe {
        let mut recipe = Recipe::reference().unwrap();
        recipe.id = "curiosity".into();
        recipe.parameters = serde_json::from_value(serde_json::json!({
            "sampling_seed": 1042, "memory_condition": "self",
            "limits": {"sessions": 10, "work_turns_per_session": 10, "wall_seconds": 1800,
                "generated_tokens": 300000, "tokens_per_call": 8192, "handoff_tokens": 8192,
                "context_tokens": 65536, "model_seconds": 180, "command_seconds": 60,
                "output_bytes": 16384, "memento_bytes": 8192, "log_bytes": 67108864u64,
                "tool_parse_retries": 5, "tool_parse_error_limit": 8},
            "workshop": {"cpu_cores": 8.0, "memory_bytes": 10485760000u64,
                "workspace_bytes": 16777216, "tmp_bytes": 1073741824u64, "pids": 256}
        }))
        .unwrap();
        recipe.workspace_entries = 900;
        recipe.seeds = BTreeMap::from([
            ("ABOUT.txt".into(), "About.\n".into()),
            ("data/pulses.json".into(), "[1, 2]\n".into()),
        ]);
        recipe
    }
    fn refused(change: impl FnOnce(&mut Recipe)) -> bool {
        let mut recipe = recipe();
        change(&mut recipe);
        recipe.check().is_err()
            && Profile::freeze_recipe(recipe, selection(), daemon(), host()).is_err()
    }
    #[test]
    fn a_recipe_freezes_into_a_live_experiment_with_derived_limits() {
        let profile = Profile::freeze_recipe(recipe(), selection(), daemon(), host()).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.mode, RunMode::Experiment);
        assert_eq!(profile.profile, RECIPE_PROFILE);
        assert_eq!(
            profile.resources,
            Limits {
                nano_cpus: 8_000_000_000,
                memory_bytes: 10_485_760_000,
                workspace_bytes: 16 << 20,
                workspace_inodes: 900,
                tmp_bytes: 1 << 30,
                pids: 256,
                seed: 1042,
            }
        );
        let capture = &profile.capture;
        assert_eq!(
            (
                capture.entries,
                capture.retained_bytes,
                capture.memory_bytes
            ),
            (900, 16 << 20, 8192)
        );
        assert_eq!(profile.storage.event_log_bytes, 64 << 20);
        let settings = &profile.model.as_ref().unwrap().settings;
        assert_eq!(settings["thinking"], "high");
        assert_eq!(settings["context_tokens"], 65536);
        // The archive freezes the recipe itself, seeds included.
        let frozen: Profile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert_eq!(frozen.recipe, Some(recipe()));
        frozen.validate().unwrap();
    }
    #[test]
    fn recipes_outside_their_bounds_are_refused() {
        assert!(refused(|r| r.id = "two-session".into()));
        assert!(refused(|r| r.id = "Curiosity".into()));
        assert!(refused(|r| r.inventory = " ".into()));
        assert!(refused(
            |r| r.parameters.limits.sessions = NonZeroU32::new(21).unwrap()
        ));
        assert!(refused(|r| {
            r.parameters.limits.context_tokens = NonZeroU64::new(262_144).unwrap()
        }));
        assert!(refused(|r| {
            r.parameters.limits.log_bytes = NonZeroU64::new(128 << 20).unwrap()
        }));
        assert!(refused(|r| {
            r.parameters.memory_condition = MemoryCondition::SelfPlusEvidence
        }));
        assert!(refused(|r| {
            r.parameters.workshop.tmp_bytes = NonZeroU64::new(2 << 30).unwrap()
        }));
        assert!(refused(|r| r.model.thinking = "max".into()));
        assert!(refused(|r| r.model.temperature = f64::NAN));
        assert!(refused(|r| r.model.top_p = 0.0));
        assert!(refused(|r| {
            r.parameters.limits.command_seconds = NonZeroU64::new(3).unwrap()
        }));
        // The controller's own checks still apply.
        assert!(refused(|r| {
            r.parameters.limits.handoff_tokens = NonZeroU64::new(300_000).unwrap()
        }));
        // A full workspace must fit every session's capture in one archive.
        // Python's 1 GiB can't, and neither can 32 MiB or 1,000 entries across
        // ten sessions.
        for bytes in [1 << 30, 32 << 20] {
            assert!(refused(|r| {
                r.parameters.workshop.workspace_bytes = NonZeroU64::new(bytes).unwrap()
            }));
        }
        assert!(refused(|r| r.workspace_entries = 1000));
        assert!(refused(|r| r.workspace_entries = 4097));
        // Seeds are plain relative files, each one bridge write, and use at
        // most half the workspace.
        for path in ["", "/abs", "../up", "a//b", "a/./b", "tab\there", "data"] {
            assert!(
                refused(|r| {
                    r.seeds.insert(path.into(), "x".into());
                }),
                "{path:?}"
            );
        }
        assert!(refused(|r| {
            r.seeds.insert("big".into(), "x".repeat(131_073));
        }));
        assert!(refused(|r| {
            r.seeds.insert("escaped".into(), "\u{1}".repeat(40_000));
        }));
        assert!(refused(|r| {
            r.seeds = (0..17)
                .map(|i| (format!("{i}.txt"), String::new()))
                .collect()
        }));
        assert!(refused(|r| {
            r.seeds = (0..5)
                .map(|i| (format!("{i}.txt"), "x".repeat(120_000)))
                .collect()
        }));
    }
    #[test]
    fn authored_definition_must_match_the_executable_scenario_and_locked_identities() {
        let mut definition = super::super::FrozenRecipe::current(recipe()).unwrap();
        definition.recipe.id = "saved-reviewed".into();
        definition.recipe.prompt = "This exact saved prompt must execute.".into();
        let mut profile =
            Profile::freeze_recipe(definition.recipe.clone(), selection(), daemon(), host())
                .unwrap();
        profile.definition = Some(definition.clone());
        profile.validate().unwrap();
        assert_eq!(profile.scenario.prompt, definition.recipe.prompt);
        for pointer in [
            "/definition/recipe/prompt",
            "/recipe/prompt",
            "/scenario/prompt",
        ] {
            let mut json = serde_json::to_value(&profile).unwrap();
            *json.pointer_mut(pointer).unwrap() = serde_json::json!("a newer head");
            assert!(
                serde_json::from_value::<Profile>(json)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        definition.model_lock["digest"] = serde_json::json!("0".repeat(64));
        definition.validate().unwrap(); // Still editable, but cannot execute.
        assert!(definition.executable().is_err());
        profile.definition = Some(definition);
        assert!(profile.validate().is_err());
    }
    #[test]
    fn a_frozen_recipe_profile_must_match_its_recipe() {
        let profile = Profile::freeze_recipe(recipe(), selection(), daemon(), host()).unwrap();
        for (pointer, replacement) in [
            ("/resources/nano_cpus", serde_json::json!(16_000_000_000u64)),
            ("/capture/entries", serde_json::json!(4096)),
            ("/storage/references", serde_json::json!(20_000)),
            ("/scenario/prompt", serde_json::json!("replacement")),
            ("/recipe/prompt", serde_json::json!("replacement")),
            ("/model/settings/thinking", serde_json::json!("low")),
            ("/model/lock/digest", serde_json::json!("0".repeat(64))),
            ("/recipe/parameters/limits/sessions", serde_json::json!(21)),
            ("/mode", serde_json::json!("live_smoke")),
        ] {
            let mut value = serde_json::to_value(&profile).unwrap();
            *value.pointer_mut(pointer).unwrap() = replacement;
            let changed: Profile = serde_json::from_value(value).unwrap();
            assert!(changed.validate().is_err(), "{pointer}");
        }
        let mut unmodeled = profile.clone();
        unmodeled.model = None;
        assert!(unmodeled.validate().is_err());
        let mut unrecipe = profile;
        unrecipe.recipe = None;
        assert!(unrecipe.validate().is_err());
    }
}
