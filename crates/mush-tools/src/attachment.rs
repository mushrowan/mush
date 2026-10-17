use std::io::Cursor;

use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};

pub const MAX_ATTACHMENT_DATA_CHARS: usize = 350_000;
pub const MAX_ATTACHMENT_DIMENSION: u32 = 1200;
const MAX_SOURCE_PIXELS: u64 = 36_000_000;
const TRANSPARENCY_BACKGROUND: u8 = 0x88;
const JPEG_QUALITIES: [u8; 5] = [82, 72, 60, 48, 36];

const USER_IMAGE_BASE64_CHARS: usize = 5 * 1024 * 1024;
const MANY_IMAGE_MAX_SIDE: u32 = 2000;
const CLAUDE_PATCH: u32 = 28;
const CLAUDE_STANDARD_SIDE: u32 = 1568;
const CLAUDE_STANDARD_PATCHES: u32 = 1568;
const CLAUDE_HIGH_RESOLUTION_PATCHES: u32 = 4784;
const CLAUDE_HIGH_RESOLUTION_SINCE: (u32, u32) = (4, 7);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudget {
    pub max_side: u32,
    pub max_base64_chars: usize,
    pub patches: Option<Patches>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patches {
    pub side: u32,
    pub max: u32,
}

const ATTACHMENT_BUDGET: ImageBudget = ImageBudget {
    max_side: MAX_ATTACHMENT_DIMENSION,
    max_base64_chars: MAX_ATTACHMENT_DATA_CHARS,
    patches: None,
};

impl ImageBudget {
    #[must_use]
    pub fn for_user_image(model: &mush_ai::types::Model) -> Self {
        let (max_side, patches) = match claude_version(&model.id) {
            Some(version) if version >= CLAUDE_HIGH_RESOLUTION_SINCE => (
                MANY_IMAGE_MAX_SIDE,
                Some(Patches {
                    side: CLAUDE_PATCH,
                    max: CLAUDE_HIGH_RESOLUTION_PATCHES,
                }),
            ),
            Some(_) => (
                CLAUDE_STANDARD_SIDE,
                Some(Patches {
                    side: CLAUDE_PATCH,
                    max: CLAUDE_STANDARD_PATCHES,
                }),
            ),
            None => (MANY_IMAGE_MAX_SIDE, None),
        };
        Self {
            max_side,
            max_base64_chars: USER_IMAGE_BASE64_CHARS,
            patches,
        }
    }

    fn fits(&self, width: u32, height: u32) -> bool {
        width.max(height) <= self.max_side
            && self.patches.is_none_or(|patches| {
                width.div_ceil(patches.side) * height.div_ceil(patches.side) <= patches.max
            })
    }

    fn size_for(&self, width: u32, height: u32) -> (u32, u32) {
        if self.fits(width, height) {
            return (width, height);
        }
        if self.patches.is_none() {
            let scale = (f64::from(self.max_side) / f64::from(width.max(height))).min(1.0);
            let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
            return (scaled(width), scaled(height));
        }
        if height > width {
            let (long, short) = self.size_for(height, width);
            return (short, long);
        }
        let aspect = f64::from(width) / f64::from(height);
        let short = |long: u32| ((f64::from(long) / aspect).round_ties_even() as u32).max(1);
        let (mut fitting, mut over) = (1, width);
        while fitting + 1 < over {
            let middle = (fitting + over) / 2;
            if self.fits(middle, short(middle)) {
                fitting = middle;
            } else {
                over = middle;
            }
        }
        (fitting, short(fitting))
    }
}

fn claude_version(id: &str) -> Option<(u32, u32)> {
    let rest = &id[id.find("claude")? + "claude".len()..];
    let mut numbers = rest
        .split(['-', '.', '@', ':', '_'])
        .skip_while(|token| token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()));
    let major = numbers
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|major| *major < 100)?;
    let minor = numbers
        .next()
        .and_then(|token| token.parse::<u32>().ok())
        .filter(|minor| *minor < 100)
        .unwrap_or(0);
    Some((major, minor))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Shrunk {
    Unchanged,
    Jpeg { bytes: Vec<u8>, note: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fitted {
    Unchanged { size: (u32, u32) },
    Resized(Resized),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Resized {
    pub bytes: Vec<u8>,
    pub original: (u32, u32),
    pub size: (u32, u32),
    pub quality: u8,
    pub conversions: Vec<String>,
}

impl Resized {
    fn note(&self) -> String {
        let ((width, height), (target_width, target_height)) = (self.original, self.size);
        let mut note = format!(
            "original {width}x{height}; attached {target_width}x{target_height} JPEG at quality {}",
            self.quality
        );
        for conversion in &self.conversions {
            note.push_str("; ");
            note.push_str(conversion);
        }
        note
    }
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
    Ok(match fit(data, &ATTACHMENT_BUDGET)? {
        Fitted::Unchanged { .. } => Shrunk::Unchanged,
        Fitted::Resized(resized) => Shrunk::Jpeg {
            note: resized.note(),
            bytes: resized.bytes,
        },
    })
}

pub fn fit(data: &[u8], budget: &ImageBudget) -> Result<Fitted, String> {
    let (image, format) = decode(data)?;
    let original = (image.width(), image.height());
    let (target_width, target_height) = budget.size_for(original.0, original.1);
    if (target_width, target_height) == original
        && base64_chars(data.len()) <= budget.max_base64_chars
    {
        return Ok(Fitted::Unchanged { size: original });
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

    let mut size = (target_width, target_height);
    let mut smallest = (0, size);
    loop {
        let resized = image
            .resize_exact(size.0, size.1, image::imageops::FilterType::Lanczos3)
            .into_rgb8();
        for quality in JPEG_QUALITIES {
            let bytes = jpeg(&resized, quality)?;
            smallest = (bytes.len(), size);
            if base64_chars(bytes.len()) <= budget.max_base64_chars {
                return Ok(Fitted::Resized(Resized {
                    bytes,
                    original,
                    size,
                    quality,
                    conversions,
                }));
            }
        }
        let next = ((size.0 * 3 / 4).max(1), (size.1 * 3 / 4).max(1));
        if next == size {
            break;
        }
        size = next;
    }
    let (len, (smallest_width, smallest_height)) = smallest;
    Err(format!(
        "could not be compressed below {}KB of base64 (smallest was {}KB at {smallest_width}x{smallest_height})",
        budget.max_base64_chars / 1000,
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

    fn model(id: &str, api: mush_ai::types::Api) -> mush_ai::types::Model {
        mush_ai::types::Model::builder()
            .id(id.into())
            .name(id.into())
            .api(api)
            .provider(mush_ai::types::Provider::Anthropic)
            .base_url("https://example.invalid".into())
            .input(vec![mush_ai::types::InputModality::Text])
            .cost(mush_ai::types::ModelCost::default())
            .context_window(mush_ai::types::TokenCount::new(200_000))
            .max_output_tokens(mush_ai::types::TokenCount::new(8192))
            .build()
    }

    #[test]
    fn claude_versions_are_read_from_every_id_style() {
        let cases = [
            ("claude-opus-5-5", Some((5, 5))),
            ("claude-sonnet-5", Some((5, 0))),
            ("claude-opus-4-7", Some((4, 7))),
            ("claude-sonnet-4-20250514", Some((4, 0))),
            ("claude-3-5-sonnet-20241022", Some((3, 5))),
            ("anthropic/claude-opus-4", Some((4, 0))),
            ("us.anthropic.claude-sonnet-4-5-20250929-v1:0", Some((4, 5))),
            ("claude-haiku-4-5@20251001", Some((4, 5))),
            ("gpt-5.6-sol", None),
            ("claude", None),
        ];

        for (id, version) in cases {
            assert_eq!(claude_version(id), version, "{id}");
        }
    }

    #[test]
    fn a_claude_model_gets_its_resolution_tier_and_everything_else_the_shared_caps() {
        use mush_ai::types::Api;
        let high = ImageBudget::for_user_image(&model("claude-opus-5-5", Api::AnthropicMessages));
        let standard =
            ImageBudget::for_user_image(&model("claude-haiku-4-5", Api::AnthropicMessages));
        let routed = ImageBudget::for_user_image(&model(
            "anthropic/claude-sonnet-4",
            Api::OpenaiCompletions,
        ));
        let other = ImageBudget::for_user_image(&model("gpt-5.6-sol", Api::OpenaiResponses));

        assert_eq!(
            (high.max_side, high.patches.map(|p| p.max)),
            (2000, Some(4784))
        );
        assert_eq!(
            (standard.max_side, standard.patches.map(|p| p.max)),
            (1568, Some(1568))
        );
        assert_eq!(routed, standard);
        assert_eq!((other.max_side, other.patches), (2000, None));
        for budget in [high, standard, other] {
            assert_eq!(budget.max_base64_chars, 5 * 1024 * 1024);
        }
    }

    #[test]
    fn the_standard_tier_matches_the_published_resize_examples() {
        let standard = ImageBudget::for_user_image(&model(
            "claude-sonnet-4-6",
            mush_ai::types::Api::AnthropicMessages,
        ));

        assert_eq!(standard.size_for(1920, 1080), (1456, 819));
        assert_eq!(standard.size_for(3840, 2160), (1456, 819));
        assert_eq!(standard.size_for(1075, 1520), (924, 1307));
        assert_eq!(standard.size_for(1000, 1000), (1000, 1000));
    }

    #[test]
    fn the_high_resolution_tier_stops_at_the_many_image_side_limit() {
        let high = ImageBudget::for_user_image(&model(
            "claude-opus-5-5",
            mush_ai::types::Api::AnthropicMessages,
        ));

        assert_eq!(high.size_for(3840, 2160), (2000, 1125));
        assert_eq!(high.size_for(1920, 1080), (1920, 1080));
        assert_eq!(high.size_for(3000, 3000), (1932, 1932));
        assert_eq!(high.size_for(1000, 6000), (333, 2000));
    }

    #[test]
    fn a_screenshot_too_big_for_the_model_is_downscaled_to_its_budget() {
        let budget = ImageBudget::for_user_image(&model(
            "claude-sonnet-4-6",
            mush_ai::types::Api::AnthropicMessages,
        ));
        let screenshot = DynamicImage::ImageRgb8(image::RgbImage::from_fn(1920, 1080, |x, y| {
            image::Rgb([
                (x % 256) as u8,
                (y % 256) as u8,
                ((x / 8 + y / 8) % 2 * 255) as u8,
            ])
        }));

        let Fitted::Resized(resized) = fit(&png(&screenshot), &budget).unwrap() else {
            panic!("a 1920x1080 screenshot is over the standard tier");
        };

        let sent = image::load_from_memory(&resized.bytes).unwrap();
        assert_eq!((sent.width(), sent.height()), (1456, 819));
        assert_eq!(
            (resized.original, resized.size),
            ((1920, 1080), (1456, 819))
        );
        assert!(base64_chars(resized.bytes.len()) <= budget.max_base64_chars);
    }

    #[test]
    fn an_image_inside_the_budget_is_sent_as_it_is() {
        let budget = ImageBudget::for_user_image(&model(
            "claude-opus-5-5",
            mush_ai::types::Api::AnthropicMessages,
        ));

        assert_eq!(
            fit(&png(&noise(640, 360)), &budget),
            Ok(Fitted::Unchanged { size: (640, 360) })
        );
    }

    #[test]
    fn an_image_over_the_byte_cap_is_reencoded_even_when_its_size_fits() {
        let pixels = noise(400, 400);
        let lowest_quality = jpeg(&pixels.to_rgb8(), 36).unwrap().len();
        let budget = ImageBudget {
            max_side: 2000,
            max_base64_chars: base64_chars(lowest_quality),
            patches: None,
        };
        let data = png(&pixels);
        assert!(base64_chars(data.len()) > budget.max_base64_chars);

        let Fitted::Resized(resized) = fit(&data, &budget).unwrap() else {
            panic!("a noisy png over the cap must be re-encoded");
        };

        assert_eq!(resized.size, (400, 400));
        assert!(base64_chars(resized.bytes.len()) <= budget.max_base64_chars);
    }
}
