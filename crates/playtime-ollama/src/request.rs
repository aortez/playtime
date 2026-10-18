use crate::{Failure, FailureKind, Profile, Result, json};
use playtime_engine::ModelRequest;
use serde::{Deserialize, Serialize};
use serde_json::json as value;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Thinking {
    Level(String),
    Boolean(bool),
}
/// The reasoning levels gpt-oss accepts. Booleans and "off" are refused rather
/// than equated with a level.
pub const THINKING_LEVELS: [&str; 3] = ["low", "medium", "high"];
/// `None` is genuine model default/omission, never an invented zero or false.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSettings {
    pub context_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub repeat_penalty: Option<f64>,
    pub thinking: Option<Thinking>,
    pub use_mmap: Option<bool>,
    pub keep_alive: String,
}
impl Default for EffectiveSettings {
    fn default() -> Self {
        Self {
            context_tokens: Some(32768),
            temperature: None,
            top_p: None,
            top_k: None,
            repeat_penalty: None,
            thinking: Some(Thinking::Level("low".into())),
            use_mmap: None,
            keep_alive: "5m".into(),
        }
    }
}
pub fn encode_request(
    request: &ModelRequest,
    settings: &EffectiveSettings,
    limit: usize,
) -> Result<Vec<u8>> {
    if request.maximum_tokens == 0
        || request.maximum_tokens > i64::MAX as u64
        || request.sampling_seed >= 1 << 31
        || settings
            .context_tokens
            .is_some_and(|n| n == 0 || n > i64::MAX as u64)
        || settings
            .temperature
            .is_some_and(|n| !n.is_finite() || n < 0.0)
        || settings
            .top_p
            .is_some_and(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
        || settings
            .repeat_penalty
            .is_some_and(|n| !n.is_finite() || n < 0.0)
        || settings.keep_alive != "5m"
        || settings.thinking.as_ref().is_some_and(
            |t| !matches!(t, Thinking::Level(level) if THINKING_LEVELS.contains(&level.as_str())),
        )
    {
        return Err(Failure::new(
            FailureKind::Request,
            "unsupported effective request setting or unadmitted request",
        ));
    }
    let mut options = serde_json::Map::from_iter([
        ("num_predict".into(), value!(request.maximum_tokens)),
        ("seed".into(), value!(request.sampling_seed)),
    ]);
    for (key, v) in [
        ("num_ctx", settings.context_tokens.map(|n| value!(n))),
        ("temperature", settings.temperature.map(|n| value!(n))),
        ("top_p", settings.top_p.map(|n| value!(n))),
        ("top_k", settings.top_k.map(|n| value!(n))),
        ("repeat_penalty", settings.repeat_penalty.map(|n| value!(n))),
        ("use_mmap", settings.use_mmap.map(|n| value!(n))),
    ] {
        if let Some(v) = v {
            options.insert(key.into(), v);
        }
    }
    let mut payload = value!({"model":Profile::MODEL,"stream":true,"messages":request.messages,"tools":request.tools,"keep_alive":settings.keep_alive,"options":options});
    if let Some(think) = &settings.thinking {
        payload["think"] = serde_json::to_value(think)
            .map_err(|e| Failure::new(FailureKind::Request, e.to_string()))?;
    }
    // Preserve exact admitted messages/tools and Python-compatible number/ASCII
    // rendering. This is not a second context estimator or prompt builder.
    json::canonical(&payload, limit)
}
