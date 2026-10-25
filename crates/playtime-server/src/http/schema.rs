use playtime_protocol::{
    api::*,
    events::EventEnvelope,
    v1::{ErrorResponse, UsageSummary},
};
use serde_json::{Value, json};
use utoipa::{OpenApi, PartialSchema, ToSchema};

/// Generated DTO schemas plus the v1 route contract. Kept separate from the
/// foundation schema, whose committed compatibility fixture is unchanged.
pub fn openapi() -> Value {
    #[derive(OpenApi)]
    #[openapi(info(title = "Playtime native HTTP API", version = "1"), components(schemas(
        ModelRegistrationReview, ModelRegistrations, RegisteredModel, ModelRegistrationRequest, ModelRegistrationResponse, ModelInventory, ModelInspection, ModelInspectRequest, Capabilities, Recipe, RecipeSettingsView, RecipeDefinition, SavedRecipe, RecipeSaveResponse, CloneRecipeRequest, ReviseRecipeRequest, PreviewRequest, Preview, LaunchRequest, LaunchResponse, CancelRequest, CancelResponse, ReleaseRequest, ReleaseResponse,
        RunView, Page<Recipe>, Page<RunView>, Page<Checkpoint>, Page<Artifact>, EventPage, EventEnvelope,
        Checkpoint, Artifact, EventCursor, StreamEnd, ErrorResponse, UsageSummary, Connection
    )))]
    struct Contract;
    let mut document = serde_json::to_value(Contract::openapi()).unwrap();
    document["info"]["description"] = json!(
        "Authenticated native clients on loopback. Fixture provenance is explicit. Cursor metadata is not publication authority. Mutation acceptance requires the application adapter's durable confirmation."
    );
    document["security"] = json!([{"bearerAuth":[]}]);
    document["components"]["securitySchemes"] =
        json!({"bearerAuth":{"type":"http","scheme":"bearer"}});
    let errors = [
        (400, "malformed_data, invalid_cursor"),
        (401, "unauthenticated"),
        (403, "forbidden_origin, forbidden_host"),
        (404, "not_found, unsupported_version"),
        (
            409,
            "stale_preview, model_metadata_changed, source_conflict, head_conflict, idempotency_conflict, service_identity_mismatch, run_busy, cursor_ahead, evidence_not_ready, release_refused",
        ),
        (413, "limit_exceeded, catalog_full, model_registry_full"),
        (415, "unsupported_media_type"),
        (422, "unsupported_capability, invalid_parameters"),
        (
            429,
            "too_many_observers, model_discovery_busy, model_registration_busy",
        ),
        (500, "archive_corrupt, content_mismatch, internal_error"),
        (
            503,
            "execution_unavailable, catalog_corrupt, model_registry_corrupt, ownership_unresolved, backend_unavailable, evidence_limit_exceeded",
        ),
        (504, "request_timeout"),
    ];
    for (status, codes) in errors {
        document["components"]["responses"][format!("Error{status}")] = json!({"description": codes,
            "content":{"application/json":{"schema":{"$ref":"#/components/schemas/ErrorResponse"}}}});
    }
    let routes = [
        (
            "post",
            "/models/review",
            "model_review",
            Some(ModelInspectRequest::name().to_string()),
            ModelRegistrationReview::name().to_string(),
            false,
        ),
        (
            "get",
            "/models/registrations",
            "model_registrations",
            None,
            ModelRegistrations::name().to_string(),
            false,
        ),
        (
            "get",
            "/models/registrations/{registration_id}",
            "model_registration",
            None,
            RegisteredModel::name().to_string(),
            false,
        ),
        (
            "post",
            "/models/registrations",
            "register_model",
            Some(ModelRegistrationRequest::name().to_string()),
            ModelRegistrationResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/model-registration-requests/{idempotency_key}",
            "model_registration_lookup",
            None,
            ModelRegistrationResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/models",
            "model_inventory",
            None,
            ModelInventory::name().to_string(),
            false,
        ),
        (
            "post",
            "/models/inspect",
            "inspect_model",
            Some(ModelInspectRequest::name().to_string()),
            ModelInspection::name().to_string(),
            false,
        ),
        (
            "get",
            "/recipes/{recipe_id}/model-settings",
            "recipe_model_settings",
            None,
            RecipeSettingsView::name().to_string(),
            false,
        ),
        (
            "get",
            "/recipes/{recipe_id}/definition",
            "recipe_definition",
            None,
            RecipeDefinition::name().to_string(),
            false,
        ),
        (
            "get",
            "/saved-recipes",
            "saved_recipes",
            None,
            format!("Page_{}", Recipe::name()),
            true,
        ),
        (
            "get",
            "/saved-recipes/{recipe_id}",
            "saved_recipe",
            None,
            SavedRecipe::name().to_string(),
            false,
        ),
        (
            "get",
            "/saved-recipes/{recipe_id}/revisions",
            "recipe_revisions",
            None,
            format!("Page_{}", Recipe::name()),
            true,
        ),
        (
            "post",
            "/saved-recipes",
            "clone_recipe",
            Some(CloneRecipeRequest::name().to_string()),
            RecipeSaveResponse::name().to_string(),
            false,
        ),
        (
            "post",
            "/saved-recipes/{recipe_id}/revisions",
            "revise_recipe",
            Some(ReviseRecipeRequest::name().to_string()),
            RecipeSaveResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/recipe-saves/{idempotency_key}",
            "recipe_save_lookup",
            None,
            RecipeSaveResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/capabilities",
            "capabilities",
            None,
            Capabilities::name().to_string(),
            false,
        ),
        (
            "get",
            "/openapi.json",
            "openapi",
            None,
            String::new(),
            false,
        ),
        (
            "get",
            "/recipes",
            "recipes",
            None,
            format!("Page_{}", Recipe::name()),
            true,
        ),
        (
            "post",
            "/recipes/{recipe_id}/preview",
            "preview",
            Some(PreviewRequest::name().to_string()),
            Preview::name().to_string(),
            false,
        ),
        (
            "post",
            "/runs",
            "launch",
            Some(LaunchRequest::name().to_string()),
            LaunchResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/launches/{idempotency_key}",
            "launch_lookup",
            None,
            LaunchResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/runs",
            "runs",
            None,
            format!("Page_{}", RunView::name()),
            true,
        ),
        (
            "get",
            "/runs/{run_id}",
            "run",
            None,
            RunView::name().to_string(),
            false,
        ),
        (
            "post",
            "/runs/{run_id}/cancel",
            "cancel",
            Some(CancelRequest::name().to_string()),
            CancelResponse::name().to_string(),
            false,
        ),
        (
            "post",
            "/runs/{run_id}/release",
            "release",
            Some(ReleaseRequest::name().to_string()),
            ReleaseResponse::name().to_string(),
            false,
        ),
        (
            "get",
            "/runs/{run_id}/events",
            "events",
            None,
            EventPage::name().to_string(),
            true,
        ),
        (
            "get",
            "/runs/{run_id}/events/stream",
            "event_stream",
            None,
            String::new(),
            false,
        ),
        (
            "get",
            "/runs/{run_id}/checkpoints",
            "checkpoints",
            None,
            format!("Page_{}", Checkpoint::name()),
            true,
        ),
        (
            "get",
            "/runs/{run_id}/checkpoints/{session_id}",
            "checkpoint",
            None,
            Checkpoint::name().to_string(),
            false,
        ),
        (
            "get",
            "/runs/{run_id}/artifacts",
            "artifacts",
            None,
            format!("Page_{}", Artifact::name()),
            true,
        ),
        (
            "get",
            "/runs/{run_id}/artifacts/{artifact_id}",
            "artifact",
            None,
            Artifact::name().to_string(),
            false,
        ),
        (
            "get",
            "/runs/{run_id}/artifacts/{artifact_id}/content",
            "content",
            None,
            String::new(),
            false,
        ),
    ];
    let response_headers = json!({"X-Playtime-Service-ID":{"schema":{"type":"string"}},"X-Request-ID":{"schema":{"type":"string"}},"Cache-Control":{"schema":{"type":"string","const":"no-store"}}});
    for (method, path, name, request, response, paged) in routes {
        let mut parameters = vec![];
        for segment in path.split('/') {
            if let Some(id) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                let pattern = if matches!(id, "run_id" | "session_id") {
                    "^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$"
                } else {
                    "^[A-Za-z0-9_-]+$"
                };
                parameters.push(json!({"name":id,"in":"path","required":true,"schema":{"type":"string","minLength":1,"maxLength":128,"pattern":pattern}}));
            }
        }
        if paged || name == "event_stream" {
            parameters.push(json!({"name":"cursor","in":"query","required":false,"schema":{"type":"string","maxLength":2048}}));
        }
        if paged {
            parameters.push(json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":1000,"default":200}}));
        }
        if name == "event_stream" {
            parameters.push(json!({"name":"Last-Event-ID","in":"header","required":false,"schema":{"type":"string","maxLength":2048},"description":"Must equal cursor when both are supplied. Only acknowledge after applying a record."}));
        }
        if method == "post" {
            parameters.push(json!({"name":"X-Playtime-Service-ID","in":"header","required":true,"schema":{"$ref":"#/components/schemas/ServiceId"}}));
        }
        if matches!(
            name,
            "launch" | "clone_recipe" | "revise_recipe" | "register_model"
        ) {
            parameters.push(json!({"name":"Idempotency-Key","in":"header","required":true,"schema":{"$ref":"#/components/schemas/IdempotencyKey"}}));
        }
        let content = match name {
            "event_stream" => {
                json!({"text/event-stream":{"schema":{"type":"string"}}})
            }
            "content" => {
                json!({"application/octet-stream":{"schema":{"type":"string","format":"binary"}}})
            }
            "openapi" => json!({"application/json":{"schema":{"type":"object"}}}),
            _ => {
                json!({"application/json":{"schema":{"$ref":format!("#/components/schemas/{response}")}}})
            }
        };
        let mut operation = json!({"operationId":name,"parameters":parameters,"responses":{"200":{"description":"Successful response","headers":response_headers,"content":content}}});
        if name == "event_stream" {
            operation["responses"]["200"]["description"] = json!(
                "record frames carry an EventEnvelope and cursor ID. stream_end/stream_error and 15-second heartbeat comments have no ID."
            );
        }
        if name == "content" {
            let headers = operation["responses"]["200"]["headers"]
                .as_object_mut()
                .unwrap();
            headers.insert(
                "Content-Length".into(),
                json!({"schema":{"type":"integer","minimum":0,"maximum":67108864}}),
            );
            headers.insert("Content-Digest".into(), json!({"schema":{"type":"string"},"description":"sha-256 structured digest of the exact bytes"}));
            headers.insert(
                "X-Playtime-SHA256".into(),
                json!({"schema":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),
            );
            headers.insert("Content-Disposition".into(), json!({"schema":{"type":"string"},"description":"attachment with the opaque artifact ID as filename"}));
            headers.insert(
                "Accept-Ranges".into(),
                json!({"schema":{"type":"string","const":"none"}}),
            );
            headers.insert(
                "X-Content-Type-Options".into(),
                json!({"schema":{"type":"string","const":"nosniff"}}),
            );
        }
        if matches!(name, "clone_recipe" | "revise_recipe" | "register_model") {
            operation["responses"]["201"] = operation["responses"]["200"].clone();
            operation["responses"]["201"]["description"] = json!(if name == "register_model" {
                "Immutable registration and keyed result committed atomically"
            } else {
                "Immutable revision and keyed result committed atomically"
            });
            operation["responses"]["200"]["description"] =
                json!("Replay of the original committed save");
        }
        if name == "launch" || name == "cancel" {
            operation["responses"]["202"] = operation["responses"]["200"].clone();
            operation["responses"]["202"]["description"] = json!(
                "Durable acceptance or cancellation intent; does not acknowledge execution or termination"
            );
        }
        for (status, _) in errors {
            operation["responses"][status.to_string()] =
                json!({"$ref":format!("#/components/responses/Error{status}")});
        }
        if let Some(request) = request {
            operation["requestBody"] = json!({"required":true,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{request}")}}}});
        }
        document["paths"][format!("/api/v1{path}")][method] = operation;
    }
    // This identifier appears in headers rather than DTO fields.
    document["components"]["schemas"]["IdempotencyKey"] =
        serde_json::to_value(IdempotencyKey::schema()).unwrap();
    document
}
