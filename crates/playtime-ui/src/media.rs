//! Verified-content decoding and memory comparison. Call from background work.
use image::{
    AnimationDecoder, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::gif::GifDecoder,
};
use std::{io::Cursor, sync::Arc};

pub const ENCODED_BYTES: usize = 4 << 20;
pub const PIXEL_BYTES: usize = 64 << 20;
#[derive(Clone, Debug)]
pub struct Pixels {
    pub image: Arc<egui::ColorImage>,
    pub bytes: usize,
    pub notice: String,
    budget: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}
impl Pixels {
    pub fn account(mut self, permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        self.budget = Some(Arc::new(permit));
        self
    }
}
fn limits(side: u32) -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(side);
    limits.max_image_height = Some(side);
    limits.max_alloc = Some(64 << 20);
    limits
}
fn pixels(image: image::RgbaImage, notice: String) -> Result<Pixels, String> {
    let size = [image.width() as usize, image.height() as usize];
    let count = size[0]
        .checked_mul(size[1])
        .ok_or("Image dimensions overflow")?;
    if count > 16_000_000 || count * 4 > PIXEL_BYTES {
        return Err("Image exceeds preview pixel budget".into());
    }
    let converted = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Ok(Pixels {
        image: Arc::new(converted),
        bytes: count * 4,
        notice,
        budget: None,
    })
}
/// Whether the workbench previews a file of this name as an image.
pub use playtime_client::report::image_name;
pub fn decode(bytes: &[u8]) -> Result<Pixels, String> {
    let (image, notice) = decode_image(bytes)?;
    pixels(image, notice)
}
/// The box a thumbnail fits in: twice the Results tab's cell, so it stays
/// sharp on a dense display.
pub const THUMBNAIL: [u32; 2] = [224, 168];
/// A thumbnail of the image: decoded under the preview limits, then scaled
/// to fit the box, keeping its aspect; a smaller image is kept as it is.
pub fn thumbnail(bytes: &[u8]) -> Result<Pixels, String> {
    let (image, notice) = decode_image(bytes)?;
    let (width, height) = (image.width(), image.height());
    let [box_width, box_height] = THUMBNAIL;
    if width <= box_width && height <= box_height {
        return pixels(image, notice);
    }
    let scale =
        (f64::from(box_width) / f64::from(width)).min(f64::from(box_height) / f64::from(height));
    let fitted = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    pixels(
        image::imageops::thumbnail(&image, fitted(width), fitted(height)),
        notice,
    )
}
/// The decoded image, a GIF's first frame, and the notice to show with it.
fn decode_image(bytes: &[u8]) -> Result<(image::RgbaImage, String), String> {
    if bytes.len() > ENCODED_BYTES {
        return Err("Image exceeds the 4 MiB encoded preview limit".into());
    }
    let format = image::guess_format(bytes).map_err(|_| "Unsupported or corrupt image")?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
    ) {
        return Err("Preview supports PNG, JPEG, WebP and GIF only".into());
    }
    if format == ImageFormat::Gif {
        return Gif::new(bytes.to_vec())?
            .next_image()?
            .ok_or_else(|| "GIF has no frames".into())
            .map(|(image, notice, _)| (image, notice));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits(4096));
    let decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    if width > 4096 || height > 4096 || u64::from(width) * u64::from(height) > 16_000_000 {
        return Err("Image exceeds preview dimension limit".into());
    }
    let image = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    Ok((
        image,
        if matches!(format, ImageFormat::WebP | ImageFormat::Png) {
            "Still preview; animation is not played for PNG/WebP"
        } else {
            "Verified saved image"
        }
        .into(),
    ))
}
pub struct Gif {
    frames: image::Frames<'static>,
    count: usize,
    duration_ms: u64,
}
impl Gif {
    pub fn new(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() > ENCODED_BYTES {
            return Err("GIF exceeds encoded preview limit".into());
        }
        let mut decoder = GifDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        decoder
            .set_limits(limits(1024))
            .map_err(|e| e.to_string())?;
        let (width, height) = decoder.dimensions();
        if width > 1024 || height > 1024 {
            return Err("GIF exceeds 1024 × 1024 preview canvas".into());
        }
        Ok(Self {
            frames: decoder.into_frames(),
            count: 0,
            duration_ms: 0,
        })
    }
    /// One owned display frame at a time, in addition to the decoder's bounded
    /// working canvas. Never collect the animation. Replaying creates a new Gif.
    pub fn next_frame(&mut self) -> Result<Option<(Pixels, u64)>, String> {
        let Some((image, notice, delay)) = self.next_image()? else {
            return Ok(None);
        };
        Ok(Some((pixels(image, notice)?, delay)))
    }
    /// The next frame's buffer, the notice to show with it and its delay.
    fn next_image(&mut self) -> Result<Option<(image::RgbaImage, String, u64)>, String> {
        if self.count >= 300 || self.duration_ms >= 60_000 {
            return Err("GIF preview limit reached; Replay is explicit".into());
        }
        let Some(frame) = self.frames.next() else {
            return Ok(None);
        };
        let frame = frame.map_err(|e| e.to_string())?;
        let (num, den) = frame.delay().numer_denom_ms();
        let source_delay = u64::from(num).checked_div(u64::from(den)).unwrap_or(0);
        let delay = source_delay.max(34); // No more than 30 display updates/sec.
        self.duration_ms = self.duration_ms.saturating_add(source_delay.max(1));
        self.count += 1;
        Ok(Some((
            frame.into_buffer(),
            format!("GIF frame {} · paused unless Play is selected", self.count),
            delay,
        )))
    }
}

pub fn memory_diff(incoming: &str, outgoing: &str) -> (String, String) {
    if incoming.len() > 256 << 10
        || outgoing.len() > 256 << 10
        || incoming.lines().count() > 4000
        || outgoing.lines().count() > 4000
    {
        return (
            String::new(),
            "Diff limit: 256 KiB / 4,000 lines per input. Compare the paged sources instead."
                .into(),
        );
    }
    if incoming == outgoing {
        return (
            "Unchanged".into(),
            "Saved content is unchanged; publication does not validate its claims.".into(),
        );
    }
    let left: Vec<_> = incoming.lines().collect();
    let right: Vec<_> = outgoing.lines().collect();
    let prefix = left.iter().zip(&right).take_while(|(a, b)| a == b).count();
    let suffix = left[prefix..]
        .iter()
        .rev()
        .zip(right[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut diff = String::new();
    for line in &left[prefix..left.len() - suffix] {
        diff.push_str("- ");
        diff.push_str(line);
        diff.push('\n');
    }
    for line in &right[prefix..right.len() - suffix] {
        diff.push_str("+ ");
        diff.push_str(line);
        diff.push('\n');
    }
    (
        diff,
        "Saved memory · exact source comparison; publication does not validate its claims.".into(),
    )
}
