//! Versioned client-facing values. Transport and persistence live elsewhere.

pub mod archive;
pub mod events;
mod read;
pub mod storage;
pub mod v1;

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
