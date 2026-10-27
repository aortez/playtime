//! Harness readback is a projection of immutable configuration, not another
//! settings store or authority to execute an unknown implementation.
use super::*;
pub use playtime_core::HarnessId;
use playtime_core::MemoryCondition;

pub const RECIPE_HARNESS_SELECTION: &str = "recipe_harness_selection_v1";
pub const MEMENTO_TURNS_V1: &str = "memento_turns_v1";
pub const CONTINUOUS_COMPACTION_V1: &str = "continuous_compaction_v1";
pub const RECIPE_COMPACTION_SETTINGS: &str = "recipe_compaction_settings_v1";
/// Advertises the optional `allow_finish_run` compaction setting.
pub const RECIPE_COMPACTION_FINISH: &str = "recipe_compaction_finish_v1";
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = "Summarize the older conversation for continued work. Preserve goals, decisions, useful file paths, verified results, failures, unresolved questions, and a concrete next action. Distinguish observations from assumptions. Incorporate still-relevant facts from the previous summary. Return only the summary, without tools.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettings {
    pub schema_version: u32,
    pub trigger_percent: u8,
    pub recent_exchanges: u8,
    pub summary_tokens: u64,
    pub instructions: String,
    /// Whether the model is offered finish_run. Without it, the run lasts until
    /// its time or token limit. Omitted when true, so definitions saved before
    /// this setting keep their exact bytes.
    #[serde(
        default = "default_allow_finish_run",
        skip_serializing_if = "is_default_allow_finish_run"
    )]
    #[schema(default = true)]
    pub allow_finish_run: bool,
}
fn default_allow_finish_run() -> bool {
    true
}
fn is_default_allow_finish_run(allow: &bool) -> bool {
    *allow
}
impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            trigger_percent: 75,
            recent_exchanges: 2,
            summary_tokens: 2048,
            instructions: DEFAULT_COMPACTION_INSTRUCTIONS.into(),
            allow_finish_run: true,
        }
    }
}
impl CompactionSettings {
    pub fn validation_error(&self) -> Option<&'static str> {
        if self.schema_version != 1 {
            return Some("Unsupported compaction settings version.");
        }
        if !(1..=99).contains(&self.trigger_percent) {
            return Some("Context trigger must be between 1 and 99 percent.");
        }
        if self.recent_exchanges > 32 {
            return Some("Retain between 0 and 32 complete exchanges.");
        }
        if !(1..=16384).contains(&self.summary_tokens) {
            return Some("Summary limit must be between 1 and 16,384 tokens.");
        }
        if self.instructions.trim().is_empty() || self.instructions.len() > 65536 {
            return Some("Compaction instructions must contain 1 to 65,536 UTF-8 bytes.");
        }
        None
    }
}
pub const HARNESS_CATALOG_LIMIT: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HarnessCatalog {
    pub schema_version: u32,
    pub harnesses: Vec<HarnessDescriptor>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HarnessDescriptor {
    #[schema(value_type = String)]
    pub id: HarnessId,
    pub name: String,
    pub version: u32,
    pub settings: Vec<String>,
    pub required_model_capabilities: Vec<String>,
    pub required_workshop_image: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MementoHarnessSettings {
    pub sessions: u32,
    pub work_turns_per_session: u32,
    pub handoff_tokens: u64,
    pub memento_bytes: u64,
    #[schema(value_type = schema::MemoryCondition)]
    pub memory_condition: MemoryCondition,
    pub tool_parse_retries: u32,
    /// Effective value, including the retained default when omitted.
    pub tool_parse_error_limit: u64,
    /// Fixed by memento_turns_v1, not an editable allocation.
    pub handoff_attempts: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HarnessView {
    /// Retained verbatim even when this build cannot interpret the identity.
    pub id: String,
    pub supported: bool,
    pub version: Option<u32>,
    pub settings: Option<MementoHarnessSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
}
impl HarnessView {
    /// Read explicit, frozen identity only. Legacy configurations without one
    /// remain unrecorded. This projection never grants execution authority.
    pub fn from_configuration(value: &Value) -> Result<Option<Self>, &'static str> {
        let root = value.get("harness");
        let retained = value.pointer("/definition/harness");
        let Some(id) = root.or(retained) else {
            return Ok(None);
        };
        let id = id
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("invalid harness identity")?;
        if retained.is_some_and(|v| v.as_str() != Some(id)) {
            return Err("conflicting harness identities");
        }
        if id == CONTINUOUS_COMPACTION_V1 {
            let settings = value
                .pointer("/config/settings")
                .ok_or("missing compaction settings")?;
            let settings: CompactionSettings = serde_json::from_value(settings.clone())
                .map_err(|_| "invalid compaction settings")?;
            if value
                .pointer("/definition/compaction/settings")
                .is_some_and(|v| serde_json::to_value(&settings).ok().as_ref() != Some(v))
            {
                return Err("conflicting compaction settings");
            }
            if let Some(parameters) = value.pointer("/definition/recipe/parameters") {
                let parameters: RunParameters = serde_json::from_value(parameters.clone())
                    .map_err(|_| "invalid harness parameters")?;
                if serde_json::to_value(parameters.common()).ok().as_ref()
                    != value.pointer("/config/parameters")
                {
                    return Err("conflicting common parameters");
                }
            }
            return Self::continuous(settings).map(Some);
        }
        let parameters = value
            .pointer("/scenario/parameters")
            .ok_or("missing harness parameters")?;
        if value
            .pointer("/definition/recipe/parameters")
            .is_some_and(|p| p != parameters)
        {
            return Err("conflicting harness parameters");
        }
        let parameters: RunParameters =
            serde_json::from_value(parameters.clone()).map_err(|_| "invalid harness parameters")?;
        Ok(Some(Self::project(id, &parameters)))
    }
    pub fn project(id: &str, parameters: &RunParameters) -> Self {
        let known = id == MEMENTO_TURNS_V1;
        let l = &parameters.limits;
        Self {
            id: id.into(),
            supported: known,
            version: known.then_some(1),
            settings: known.then(|| MementoHarnessSettings {
                sessions: l.sessions.get(),
                work_turns_per_session: l.work_turns_per_session.get(),
                handoff_tokens: l.handoff_tokens.get(),
                memento_bytes: l.memento_bytes.get(),
                memory_condition: parameters.memory_condition,
                tool_parse_retries: l.tool_parse_retries,
                tool_parse_error_limit: l
                    .tool_parse_error_limit
                    .map_or(u64::from(l.tool_parse_retries) + 1, |n| u64::from(n.get())),
                handoff_attempts: 2,
            }),
            compaction: None,
        }
    }
    pub fn continuous(settings: CompactionSettings) -> Result<Self, &'static str> {
        if let Some(error) = settings.validation_error() {
            return Err(error);
        }
        Ok(Self {
            id: CONTINUOUS_COMPACTION_V1.into(),
            supported: true,
            version: Some(1),
            settings: None,
            compaction: Some(settings),
        })
    }
    pub fn from_definition(value: &Value) -> Result<Self, &'static str> {
        let id = value["harness"]
            .as_str()
            .ok_or("missing harness identity")?;
        if id == CONTINUOUS_COMPACTION_V1 {
            Self::continuous(
                serde_json::from_value(value["compaction"]["settings"].clone())
                    .map_err(|_| "invalid compaction settings")?,
            )
        } else {
            let parameters = serde_json::from_value(value["recipe"]["parameters"].clone())
                .map_err(|_| "invalid harness parameters")?;
            Ok(Self::project(id, &parameters))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RecipeHarnessView {
    pub execution_recipe_id: RecipeId,
    pub definition_digest: String,
    pub harness: HarnessView,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ArchivedHarness {
    /// Manifest-bound source of this projection; never the current recipe head.
    pub configuration: ContentRef,
    pub harness: HarnessView,
}
