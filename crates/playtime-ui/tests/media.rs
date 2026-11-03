use image::{DynamicImage, ImageFormat, RgbaImage};
use playtime_ui::media::*;
use std::io::Cursor;
fn encoded(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([32, 180, 130, 255]),
    ));
    let mut bytes = Cursor::new(vec![]);
    if format == ImageFormat::Jpeg {
        image.to_rgb8().write_to(&mut bytes, format).unwrap();
    } else {
        image.write_to(&mut bytes, format).unwrap();
    }
    bytes.into_inner()
}
#[test]
fn explicit_image_codecs_validate_dimensions_and_reject_active_content() {
    for format in [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::WebP,
        ImageFormat::Gif,
    ] {
        let pixels = decode(&encoded(format, 3, 2)).unwrap();
        assert_eq!(pixels.image.size, [3, 2]);
        assert_eq!(pixels.bytes, 24);
    }
    assert!(decode(b"<svg onload='fetch(\"file:///secret\")'/>").is_err());
    assert!(decode(b"\x89PNG\r\n\x1a\ncorrupt").is_err());
    assert!(decode(&encoded(ImageFormat::Png, 4097, 1)).is_err());
    assert!(decode(&vec![0; ENCODED_BYTES + 1]).is_err());
    assert!(Gif::new(encoded(ImageFormat::Gif, 1025, 1)).is_err());
}
#[test]
fn a_thumbnail_fits_the_box_and_keeps_its_aspect() {
    let wide = thumbnail(&encoded(ImageFormat::Png, 640, 480)).unwrap();
    assert_eq!(wide.image.size, [224, 168]);
    assert_eq!(wide.bytes, 224 * 168 * 4);
    assert_eq!(wide.image.pixels[0], egui::Color32::from_rgb(32, 180, 130));
    let tall = thumbnail(&encoded(ImageFormat::Jpeg, 300, 900)).unwrap();
    assert_eq!(tall.image.size, [56, 168]);
    // A GIF's first frame; a small image is kept as it is.
    let gif = thumbnail(&encoded(ImageFormat::Gif, 448, 100)).unwrap();
    assert_eq!(gif.image.size, [224, 50]);
    assert!(gif.notice.starts_with("GIF frame 1"));
    let small = thumbnail(&encoded(ImageFormat::WebP, 3, 2)).unwrap();
    assert_eq!(small.image.size, [3, 2]);
    // The preview limits still apply before any scaling.
    assert!(thumbnail(b"\x89PNG\r\n\x1a\ncorrupt").is_err());
    assert!(thumbnail(&encoded(ImageFormat::Png, 4097, 1)).is_err());
    assert!(thumbnail(&vec![0; ENCODED_BYTES + 1]).is_err());
}
#[test]
fn animation_is_incremental_and_stops_at_300_frames() {
    let mut encoded = vec![];
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
        for _ in 0..301 {
            encoder
                .encode_frame(image::Frame::from_parts(
                    RgbaImage::new(2, 2),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(1, 1),
                ))
                .unwrap();
        }
    }
    let mut gif = Gif::new(encoded).unwrap();
    for _ in 0..300 {
        let (pixels, delay) = gif.next_frame().unwrap().unwrap();
        assert!(delay >= 34);
        assert_eq!(pixels.bytes, 16);
    }
    assert!(gif.next_frame().err().unwrap().contains("limit"));
}
