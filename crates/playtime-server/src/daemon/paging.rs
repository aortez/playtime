use crate::application::{ApiError, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use playtime_protocol::api::*;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    version: u32,
    kind: String,
    scope: String,
    after: String,
    cutoff: String,
    pub frontier: Option<Frontier>,
}
pub(super) fn decode(query: &PageQuery, kind: &str, scope: &str) -> Result<Option<Cursor>> {
    query
        .cursor
        .as_ref()
        .map(|value| {
            if value.len() > 2048 {
                return Err(ApiError::new("invalid_cursor"));
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| ApiError::new("invalid_cursor"))?;
            let cursor: Cursor =
                serde_json::from_slice(&bytes).map_err(|_| ApiError::new("invalid_cursor"))?;
            if cursor.version != 1
                || cursor.kind != kind
                || cursor.scope != scope
                || cursor.after.len() > 256
                || cursor.cutoff.len() > 256
            {
                return Err(ApiError::new("invalid_cursor"));
            }
            Ok(cursor)
        })
        .transpose()
}
pub(super) fn page<T: Serialize>(
    mut items: Vec<(String, T)>,
    query: &PageQuery,
    kind: &str,
    scope: &str,
    frontier: Option<Frontier>,
) -> Result<Page<T>> {
    let limits = ApiLimits::default();
    let limit = query
        .limit(&limits)
        .map_err(|_| ApiError::new("malformed_data"))? as usize;
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut cursor = decode(query, kind, scope)?.unwrap_or_else(|| Cursor {
        version: 1,
        kind: kind.into(),
        scope: scope.into(),
        after: String::new(),
        cutoff: items.last().map(|i| i.0.clone()).unwrap_or_default(),
        frontier: frontier.clone(),
    });
    if cursor.frontier != frontier {
        return Err(ApiError::new("invalid_cursor"));
    }
    let mut selected = Vec::new();
    let mut bytes = 1600;
    let mut more = false;
    for (key, item) in items {
        if key <= cursor.after || key > cursor.cutoff {
            continue;
        }
        let size = serde_json::to_vec(&item)
            .map_err(|_| ApiError::new("internal_error"))?
            .len()
            + 1;
        if selected.len() == limit || bytes + size > limits.json_bytes {
            if selected.is_empty() {
                return Err(ApiError::new("evidence_limit_exceeded"));
            }
            more = true;
            break;
        }
        bytes += size;
        cursor.after = key;
        selected.push(item);
    }
    Ok(Page {
        items: selected,
        next_cursor: more.then(|| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap())),
        frontier,
    })
}
