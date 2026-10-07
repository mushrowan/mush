use std::io::Cursor;

use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};

pub const MAX_ATTACHMENT_DATA_CHARS: usize = 350_000;
pub const MAX_ATTACHMENT_DIMENSION: u32 = 1200;
const MAX_SOURCE_PIXELS: u64 = 36_000_000;
const TRANSPARENCY_BACKGROUND: u8 = 0x88;
const JPEG_QUALITIES: [u8; 5] = [82, 72, 60, 48, 36];

#[derive(Debug, PartialEq, Eq)]
pub enum Shrunk {
    Unchanged,
    Jpeg { bytes: Vec<u8>, note: String },
}

fn base64_chars(len: usize) -> usize {
    len.div_ceil(3) * 4
}

fn animated(data: &[u8], format: ImageFormat) -> bool {
    match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(data))
            .is_ok_and(|decoder| decoder.into_frames().take(2).count() > 1),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(data))
            .and_then(|decoder| decoder.is_apng())
            .unwrap_or(false),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(data))
            .is_ok_and(|decoder| decoder.has_animation()),
        _ => false,
    }
}

fn decode(data: &[u8]) -> Result<(DynamicImage, ImageFormat), String> {
    let unreadable = |error: image::ImageError| format!("not a readable image: {error}");
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| format!("not a readable image: {error}"))?;
    let format = reader
        .format()
        .ok_or_else(|| "not a png, jpeg, gif or webp image".to_string())?;
    let mut decoder = reader.into_decoder().map_err(unreadable)?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_SOURCE_PIXELS {
        return Err(format!(
            "{width}x{height} ({}MP) is over the {}MP limit, resize it first",
            pixels / 1_000_000,
            MAX_SOURCE_PIXELS / 1_000_000
        ));
    }
    let orientation = decoder.orientation().map_err(unreadable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(unreadable)?;
    image.apply_orientation(orientation);
    Ok((image, format))
}

fn flatten(mut rgba: image::RgbaImage) -> image::RgbImage {
    for pixel in rgba.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            let blended =
                u16::from(*channel) * alpha + u16::from(TRANSPARENCY_BACKGROUND) * (255 - alpha);
            *channel = u8::try_from((blended + 127) / 255).unwrap_or(u8::MAX);
        }
    }
    DynamicImage::ImageRgba8(rgba).into_rgb8()
}

pub(crate) fn jpeg(image: &image::RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

pub fn shrink(data: &[u8]) -> Result<Shrunk, String> {
    let (image, format) = decode(data)?;
    let (width, height) = (image.width(), image.height());
    if data.len() <= MAX_ATTACHMENT_DATA_CHARS * 3 / 4
        && width.max(height) <= MAX_ATTACHMENT_DIMENSION
        && base64_chars(data.len()) <= MAX_ATTACHMENT_DATA_CHARS
    {
        return Ok(Shrunk::Unchanged);
    }

    let mut conversions = Vec::new();
    if animated(data, format) {
        conversions.push("animated image flattened to first frame".to_string());
    }
    let image = if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        if rgba.pixels().any(|pixel| pixel[3] < u8::MAX) {
            conversions.push(format!(
                "transparent pixels composited on #{TRANSPARENCY_BACKGROUND:02x}{TRANSPARENCY_BACKGROUND:02x}{TRANSPARENCY_BACKGROUND:02x} background"
            ));
            flatten(rgba)
        } else {
            DynamicImage::ImageRgba8(rgba).into_rgb8()
        }
    } else {
        image.into_rgb8()
    };
    let image = DynamicImage::ImageRgb8(image);

    let scale = (f64::from(MAX_ATTACHMENT_DIMENSION) / f64::from(width.max(height))).min(1.0);
    let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    let (mut target_width, mut target_height) = (scaled(width), scaled(height));
    let mut smallest = (0, target_width, target_height);
    loop {
        let resized = image
            .resize_exact(
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            )
            .into_rgb8();
        for quality in JPEG_QUALITIES {
            let bytes = jpeg(&resized, quality)?;
            smallest = (bytes.len(), target_width, target_height);
            if base64_chars(bytes.len()) <= MAX_ATTACHMENT_DATA_CHARS {
                let mut note = format!(
                    "original {width}x{height}; attached {target_width}x{target_height} JPEG at quality {quality}"
                );
                for conversion in &conversions {
                    note.push_str("; ");
                    note.push_str(conversion);
                }
                return Ok(Shrunk::Jpeg { bytes, note });
            }
        }
        let next = (
            (target_width * 3 / 4).max(1),
            (target_height * 3 / 4).max(1),
        );
        if next == (target_width, target_height) {
            break;
        }
        (target_width, target_height) = next;
    }
    let (len, smallest_width, smallest_height) = smallest;
    Err(format!(
        "could not be compressed below {}KB of base64 (smallest was {}KB at {smallest_width}x{smallest_height})",
        MAX_ATTACHMENT_DATA_CHARS / 1000,
        base64_chars(len) / 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(image: &DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn noise(width: u32, height: u32) -> DynamicImage {
        let mut seed = 0x2545_f491_u32;
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |_, _| {
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            };
            image::Rgb([next(), next(), next()])
        }))
    }

    fn attached(data: &[u8]) -> (DynamicImage, String) {
        match shrink(data).unwrap() {
            Shrunk::Jpeg { bytes, note } => {
                assert!(base64_chars(bytes.len()) <= MAX_ATTACHMENT_DATA_CHARS);
                (image::load_from_memory(&bytes).unwrap(), note)
            }
            Shrunk::Unchanged => panic!("expected a shrunk image"),
        }
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn a_small_image_is_attached_unchanged() {
        let data = png(&noise(64, 32));

        assert_eq!(shrink(&data), Ok(Shrunk::Unchanged));
    }

    #[test]
    fn a_large_image_is_scaled_to_the_long_side_limit_as_jpeg() {
        let data = png(&noise(2400, 1000));

        let (image, note) = attached(&data);

        assert_eq!((image.width(), image.height()), (1200, 500));
        assert!(
            note.starts_with("original 2400x1000; attached 1200x500 JPEG at quality "),
            "{note}"
        );
    }

    #[test]
    fn a_detailed_image_steps_down_until_it_fits_the_payload_cap() {
        let data = png(&noise(1200, 1200));

        let (image, note) = attached(&data);

        assert!(image.width() < 1200, "{note}");
        assert_eq!(image.width(), image.height());
    }

    #[test]
    fn transparency_is_composited_on_grey() {
        let clear = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1600,
            400,
            image::Rgba([255, 0, 0, 0]),
        ));

        let (image, note) = attached(&png(&clear));

        let pixel = image.to_rgb8().get_pixel(10, 10).0;
        assert!(
            pixel.iter().all(|channel| channel.abs_diff(0x88) <= 3),
            "{pixel:?}"
        );
        assert!(
            note.ends_with("; transparent pixels composited on #888888 background"),
            "{note}"
        );
    }

    #[test]
    fn an_animation_is_flattened_to_its_first_frame() {
        let frame = |colour: [u8; 4]| {
            image::Frame::new(image::RgbaImage::from_pixel(1300, 20, image::Rgba(colour)))
        };
        let mut data = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut data);
            encoder
                .encode_frames([frame([0, 0, 255, 255]), frame([255, 0, 0, 255])])
                .unwrap();
        }

        let (image, note) = attached(&data);

        let [red, _, blue] = image.to_rgb8().get_pixel(5, 5).0;
        assert!(blue > 200 && red < 50, "{red} {blue}");
        assert!(
            note.ends_with("; animated image flattened to first frame"),
            "{note}"
        );
    }

    #[test]
    fn an_image_over_the_pixel_limit_is_refused_before_decoding() {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&7000_u32.to_be_bytes());
        ihdr.extend_from_slice(&7000_u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.extend_from_slice(&13_u32.to_be_bytes());
        data.extend_from_slice(&ihdr);
        data.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        data.extend_from_slice(&0_u32.to_be_bytes());
        data.extend_from_slice(b"IDAT");
        data.extend_from_slice(&crc32(b"IDAT").to_be_bytes());

        let error = shrink(&data).unwrap_err();

        assert_eq!(
            error,
            "7000x7000 (49MP) is over the 36MP limit, resize it first"
        );
    }

    #[test]
    fn something_that_is_not_an_image_is_refused() {
        assert!(shrink(b"plain text, not pixels").is_err());
    }
}
