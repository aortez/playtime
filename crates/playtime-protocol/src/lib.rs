//! Versioned client-facing values. Transport and persistence live elsewhere.

pub mod api;
pub mod archive;
pub mod archive_queries;
pub mod backend_guard;
pub mod bridge;
pub mod compaction;
pub mod events;
pub mod execution;
pub mod observations;
mod read;
pub mod recovery;
pub mod storage;
pub mod v1;
pub mod worker;
pub mod worker_bundle;

pub use read::ReadError;

/// Envelope/read-model schemas; known event payloads are typed in `events`.
pub fn schema_document() -> utoipa::openapi::OpenApi {
    use utoipa::OpenApi;

    #[derive(OpenApi)]
    #[openapi(
        info(title = "Playtime foundation contracts", version = "1"),
        components(schemas(
            v1::ErrorResponse,
            v1::UsageSummary,
            events::EventEnvelope,
            archive::InputManifest
        ))
    )]
    struct Contracts;

    let mut document = Contracts::openapi();
    document.info.license = None;
    document.info.description = None;
    document
}
