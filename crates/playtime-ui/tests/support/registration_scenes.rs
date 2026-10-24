use super::model_scenes;
use playtime_client::registrations::{ModelRegistrationReceipt, PendingModelRegistration};
use playtime_protocol::api::*;
use playtime_ui::{model::Mutation, registrations, ui::Workbench};
use std::sync::Arc;

pub fn prepare(app: &mut Workbench, scene: &str) {
    model_scenes::prepare(app, false, false);
    let m = &mut app.model;
    let view = m.models.inspection.as_mut().unwrap();
    let request = ModelRegistrationRequest {
        endpoint_binding: "d".repeat(64),
        identity: RegisteredModelIdentity::from_inspection(&view.value).unwrap(),
    };
    *view = playtime_ui::models::review(ModelRegistrationReview {
        inspection: view.value.clone(),
        request: Some(request.clone()),
    });
    m.registrations.enabled = true;
    m.registrations.receipts_ready = true;
    if scene.starts_with("model-registered") {
        let value = RegisteredModel {
            schema_version: 1,
            summary: ModelRegistrationSummary {
                registration_id: ModelRegistrationId::new("model-0123456789abcdef").unwrap(),
                identity: request.identity,
            },
            definition: serde_json::json!({
                "template":"{{ .System }}\n{{ .Prompt }}", "parameters":"temperature 0.7",
                "details":{"family":"gptoss"}, "model_info":{}, "capabilities":["completion","tools"]
            }),
        };
        m.models.enabled = false;
        m.registrations.show_saved = true;
        m.registrations.notice =
            "Ollama is unavailable. Stored registrations remain readable.".into();
        m.registrations.items = vec![value.summary.clone()];
        m.registrations.selected = Some(value.summary.registration_id.clone());
        m.registrations.definition = Some(registrations::prepare(Arc::new(value), 0));
    } else if scene == "model-registration-pending" {
        m.registrations.mutation = Mutation::Unresolved(
            "Connection lost after submitting registration. The original request is retained."
                .into(),
        );
        m.registrations.receipts = vec![ModelRegistrationReceipt {
            pending: PendingModelRegistration {
                schema_version: 1,
                service_id: m.service.clone().unwrap(),
                key: IdempotencyKey::new("saved-registration-request").unwrap(),
                request,
            },
            endpoint: "http://127.0.0.1:8000".into(),
            registered_model: None,
        }];
    }
}
