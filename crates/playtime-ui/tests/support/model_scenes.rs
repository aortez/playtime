use playtime_core::UtcTime;
use playtime_protocol::api::*;
use playtime_ui::{models, ui::Workbench};
pub fn inventory() -> ModelInventory {
    ModelInventory {
        schema_version: 1,
        observed_at: UtcTime::new("2026-10-24T12:00:00Z").unwrap(),
        backend_version: "0.32.3".into(),
        loaded_warning: Some(
            "Loaded-state metadata unavailable; installed inventory retained".into(),
        ),
        models: vec![
            DiscoveredModel {
                name: "gpt-oss:20b".into(),
                digest: Some("a".repeat(64)),
                status: ModelMetadataStatus::Unverified,
                reason: None,
                loaded: ModelLoaded::Unknown,
            },
            DiscoveredModel {
                name: "experiment/alias:latest".into(),
                digest: Some("a".repeat(64)),
                status: ModelMetadataStatus::Unverified,
                reason: None,
                loaded: ModelLoaded::Unknown,
            },
            DiscoveredModel {
                name: "remote:cloud".into(),
                digest: Some("b".repeat(64)),
                status: ModelMetadataStatus::Rejected,
                reason: Some("Remote/cloud delegation is unsupported".into()),
                loaded: ModelLoaded::Unknown,
            },
        ],
    }
}
pub fn inspection(name: &str) -> ModelInspection {
    ModelInspection {
        schema_version: 1,
        observed_at: inventory().observed_at,
        name: name.into(),
        backend_version: "0.32.3".into(),
        digest: Some("a".repeat(64)),
        status: ModelMetadataStatus::Candidate,
        reason: Some(
            "Identity candidate only; registration and recipe selection are separate operations"
                .into(),
        ),
        definition_sha256: Some("c".repeat(64)),
        advertised_capabilities: vec!["completion".into(), "tools".into()],
        capability_provenance: "Ollama metadata observation; capability support unreviewed".into(),
        reviewed_capabilities: None,
    }
}
pub fn missing_inspection(name: &str) -> ModelInspection {
    let mut value = inspection(name);
    value.status = ModelMetadataStatus::Missing;
    value.digest = None;
    value.definition_sha256 = None;
    value.advertised_capabilities.clear();
    value.reason = Some("Selected model is not installed; no replacement was selected".into());
    value
}
pub fn prepare(app: &mut Workbench, missing: bool, offline: bool) {
    app.model.models.enabled = true;
    app.model.models.open = true;
    app.model.models.inventory = Some(models::inventory(inventory()));
    let name = if missing {
        "saved-model:no-longer-installed"
    } else {
        "gpt-oss:20b"
    };
    app.model.models.selection = Some(ModelInspectRequest {
        name: name.into(),
        expected_digest: None,
    });
    if !missing {
        app.model.models.inspection = Some(models::inspection(inspection(name)));
    }
    app.model.models.stale = offline;
    app.model.models.notice = if offline {
        "Metadata unavailable: backend_unavailable · refresh to retry"
    } else {
        "Read-only metadata · recipe selection is unchanged"
    }
    .into();
}
