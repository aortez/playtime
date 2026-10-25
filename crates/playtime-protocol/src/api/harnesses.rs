//! Harness readback is a projection of immutable configuration, not another
//! settings store or authority to execute an unknown implementation.
use super::*;
pub use playtime_core::HarnessId;
use playtime_core::MemoryCondition;

pub const RECIPE_HARNESS_SELECTION: &str = "recipe_harness_selection_v1";
pub const MEMENTO_TURNS_V1: &str = "memento_turns_v1";
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
