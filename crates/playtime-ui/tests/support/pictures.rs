//! Synthetic pictures and distinct final files for the thumbnail scenes.
use playtime_protocol::api::{Artifact, ArtifactId};

fn hash(name: &str) -> u64 {
    name.bytes()
        .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
}
/// A distinct opaque ID for a named file.
pub fn artifact_id(name: &str) -> ArtifactId {
    ArtifactId::new(format!("{:0>64}", hash(name))).unwrap()
}
/// A retained final file of a continuous run, with a content digest of its
/// own.
pub fn artifact(name: &str, bytes: u64) -> Artifact {
    serde_json::from_value(serde_json::json!({
        "artifact_id": artifact_id(name),
        "name": name,
        "kind": "file_version",
        "content": {
            "path": format!("artifacts/final/files/{name}"),
            "sha256": format!("{:0>64x}", hash(name)),
            "size_bytes": bytes,
        },
        "session_id": null,
        "published_sequence": 25,
        "frozen_input": false,
    }))
    .unwrap()
}
/// A PNG of a gradient, to decode as a thumbnail.
pub fn picture(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x * 255 / width) as u8, (y * 255 / height) as u8, seed, 255])
    });
    let mut bytes = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
