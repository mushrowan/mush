use std::path::{Path, PathBuf};

use mush_ai::types::{ImageMimeType, InputModality, Model};
use mush_tools::attachment::{Fitted, ImageBudget};

use crate::clipboard::ClipboardImage;
use crate::input_buffer::PendingImage;

#[derive(Debug)]
pub struct Pasted {
    pub image: PendingImage,
    pub status: String,
}

pub fn prepare(image: ClipboardImage, model: &Model, originals: &Path) -> Result<Pasted, String> {
    let budget = ImageBudget::for_user_image(model);
    let fitted = mush_tools::attachment::fit(&image.bytes, &budget)
        .map_err(|error| format!("image not attached: {error}"))?;
    let unreadable = if model.input.contains(&InputModality::Image) {
        String::new()
    } else {
        format!(", but {} can't read images", model.id)
    };
    let resized = match fitted {
        Fitted::Unchanged {
            size: (width, height),
        } => {
            let status = format!(
                "image attached ({} {}, {width}x{height}){unreadable}",
                format_name(image.mime_type),
                format_size(image.bytes.len()),
            );
            return Ok(Pasted {
                image: PendingImage {
                    data: image.bytes,
                    mime_type: image.mime_type,
                    dimensions: Some((width, height)),
                    note: None,
                },
                status,
            });
        }
        Fitted::Resized(resized) => resized,
    };
    let ((width, height), (sent_width, sent_height)) = (resized.original, resized.size);
    let kept = match keep_original(&image, originals) {
        Ok(path) => format!("original at {}", path.display()),
        Err(error) => format!("the original could not be kept: {error}"),
    };
    let note = format!(
        "{width}x{height} {} {} → {sent_width}x{sent_height} JPEG {}, {kept}",
        format_name(image.mime_type),
        format_size(image.bytes.len()),
        format_size(resized.bytes.len()),
    );
    Ok(Pasted {
        status: format!("image resized to fit {}: {note}{unreadable}", model.id),
        image: PendingImage {
            data: resized.bytes,
            mime_type: ImageMimeType::Jpeg,
            dimensions: Some(resized.size),
            note: Some(note),
        },
    })
}

#[must_use]
pub fn annotate(text: String, images: &[PendingImage]) -> String {
    let notes: Vec<String> = images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let note = image.note.as_ref()?;
            Some(format!("[image {}: {note}]", index + 1))
        })
        .collect();
    if notes.is_empty() {
        return text;
    }
    format!("{text}\n\n{}", notes.join("\n"))
}

#[must_use]
pub fn split_notes(text: &str) -> (String, Vec<(usize, String)>) {
    let unannotated = || (text.to_string(), Vec::new());
    let Some((body, block)) = text.rsplit_once("\n\n") else {
        return unannotated();
    };
    let mut notes = Vec::new();
    for line in block.lines() {
        let Some((number, note)) = line
            .strip_prefix("[image ")
            .and_then(|inner| inner.strip_suffix(']'))
            .and_then(|inner| inner.split_once(": "))
        else {
            return unannotated();
        };
        match number.parse::<usize>() {
            Ok(number) if number > 0 => notes.push((number - 1, note.to_string())),
            _ => return unannotated(),
        }
    }
    (body.to_string(), notes)
}

fn keep_original(image: &ClipboardImage, originals: &Path) -> std::io::Result<PathBuf> {
    let stamp = mush_ai::types::Timestamp::now().as_ms();
    let extension = extension(image.mime_type);
    let path = (0..)
        .map(|attempt| match attempt {
            0 => originals.join(format!("pasted-{stamp}.{extension}")),
            n => originals.join(format!("pasted-{stamp}-{n}.{extension}")),
        })
        .find(|path| !path.exists())
        .unwrap_or_else(|| originals.join(format!("pasted-{stamp}.{extension}")));
    mush_ai::private_io::write_private(&path, &image.bytes)?;
    Ok(path)
}

fn format_name(mime_type: ImageMimeType) -> &'static str {
    match mime_type {
        ImageMimeType::Jpeg => "JPEG",
        ImageMimeType::Png => "PNG",
        ImageMimeType::Gif => "GIF",
        ImageMimeType::Webp => "WebP",
    }
}

fn extension(mime_type: ImageMimeType) -> &'static str {
    match mime_type {
        ImageMimeType::Jpeg => "jpg",
        ImageMimeType::Png => "png",
        ImageMimeType::Gif => "gif",
        ImageMimeType::Webp => "webp",
    }
}

fn format_size(bytes: usize) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} KB", bytes.div_ceil(1_000)),
        _ => format!("{:.1} MB", bytes as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Api, Provider, TokenCount};

    fn model(id: &str, input: Vec<InputModality>) -> Model {
        Model::builder()
            .id(id.into())
            .name(id.into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("https://example.invalid".into())
            .input(input)
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(8192))
            .build()
    }

    fn vision(id: &str) -> Model {
        model(id, vec![InputModality::Text, InputModality::Image])
    }

    fn screenshot(width: u32, height: u32) -> ClipboardImage {
        let pixels = image::RgbImage::from_fn(width, height, |x, y| {
            let stripe = if (x / 7 + y / 11) % 3 == 0 { 30 } else { 220 };
            image::Rgb([stripe, (x % 251) as u8, (y % 241) as u8])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        ClipboardImage {
            bytes,
            mime_type: ImageMimeType::Png,
        }
    }

    #[test]
    fn a_screenshot_too_big_for_the_model_is_sent_smaller_and_its_original_is_kept() {
        let originals = tempfile::tempdir().unwrap();
        let pasted_png = screenshot(2560, 1440);
        let original_bytes = pasted_png.bytes.clone();

        let pasted = prepare(pasted_png, &vision("claude-opus-5-5"), originals.path()).unwrap();

        let sent = image::load_from_memory(&pasted.image.data).unwrap();
        assert_eq!((sent.width(), sent.height()), (2000, 1125));
        assert_eq!(pasted.image.mime_type, ImageMimeType::Jpeg);
        assert_eq!(pasted.image.dimensions, Some((2000, 1125)));
        let kept: Vec<_> = std::fs::read_dir(originals.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read(&kept[0]).unwrap(), original_bytes);
        let note = pasted.image.note.as_deref().unwrap();
        assert!(
            note.starts_with("2560x1440 PNG ") && note.contains(" → 2000x1125 JPEG "),
            "{note}"
        );
        assert!(
            note.ends_with(&format!("original at {}", kept[0].display())),
            "{note}"
        );
        assert_eq!(
            pasted.status,
            format!("image resized to fit claude-opus-5-5: {note}")
        );
    }

    #[test]
    fn a_standard_resolution_model_gets_its_own_smaller_size() {
        let originals = tempfile::tempdir().unwrap();

        let pasted = prepare(
            screenshot(2560, 1440),
            &vision("claude-haiku-4-5"),
            originals.path(),
        )
        .unwrap();

        assert_eq!(pasted.image.dimensions, Some((1456, 819)));
    }

    #[test]
    fn an_image_that_already_fits_is_attached_untouched() {
        let originals = tempfile::tempdir().unwrap();
        let small = screenshot(800, 600);
        let bytes = small.bytes.clone();

        let pasted = prepare(small, &vision("claude-opus-5-5"), originals.path()).unwrap();

        assert_eq!(pasted.image.data, bytes);
        assert_eq!(pasted.image.mime_type, ImageMimeType::Png);
        assert_eq!(pasted.image.dimensions, Some((800, 600)));
        assert_eq!(pasted.image.note, None);
        assert!(
            pasted.status.starts_with("image attached (PNG "),
            "{}",
            pasted.status
        );
        assert_eq!(std::fs::read_dir(originals.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_model_without_vision_is_named_when_an_image_is_pasted() {
        let originals = tempfile::tempdir().unwrap();

        let pasted = prepare(
            screenshot(64, 64),
            &model("text-only", vec![InputModality::Text]),
            originals.path(),
        )
        .unwrap();

        assert!(
            pasted.status.ends_with(", but text-only can't read images"),
            "{}",
            pasted.status
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_not_attached() {
        let originals = tempfile::tempdir().unwrap();
        let junk = ClipboardImage {
            bytes: b"not pixels".to_vec(),
            mime_type: ImageMimeType::Png,
        };

        let error = prepare(junk, &vision("claude-opus-5-5"), originals.path()).unwrap_err();

        assert!(error.starts_with("image not attached: "), "{error}");
    }

    #[test]
    fn the_sent_text_carries_a_note_for_each_resized_image() {
        let plain = PendingImage {
            data: vec![1],
            mime_type: ImageMimeType::Png,
            dimensions: Some((10, 10)),
            note: None,
        };
        let resized = PendingImage {
            note: Some("3840x2160 PNG 10.2 MB → 2000x1125 JPEG 412 KB, original at /o.png".into()),
            ..plain.clone()
        };

        let text = annotate(
            "[image 1] and [image 2]".into(),
            &[plain.clone(), resized.clone()],
        );

        assert_eq!(
            text,
            "[image 1] and [image 2]\n\n[image 2: 3840x2160 PNG 10.2 MB → 2000x1125 JPEG 412 KB, original at /o.png]"
        );
        assert_eq!(annotate("hi".into(), &[plain]), "hi");
    }

    #[test]
    fn notes_split_back_off_the_text_they_were_added_to() {
        let images = [
            PendingImage {
                data: vec![1],
                mime_type: ImageMimeType::Jpeg,
                dimensions: None,
                note: Some("a → b, original at /x.png".into()),
            },
            PendingImage {
                data: vec![2],
                mime_type: ImageMimeType::Jpeg,
                dimensions: None,
                note: Some("c → d, original at /y.png".into()),
            },
        ];
        let sent = annotate("[image 1] [image 2] compare\n\nthese".into(), &images);

        assert_eq!(
            split_notes(&sent),
            (
                "[image 1] [image 2] compare\n\nthese".to_string(),
                vec![
                    (0, "a → b, original at /x.png".to_string()),
                    (1, "c → d, original at /y.png".to_string())
                ]
            )
        );
        assert_eq!(
            split_notes("just text\n\nmore text"),
            ("just text\n\nmore text".to_string(), Vec::new())
        );
    }

    #[test]
    fn sizes_read_the_way_people_say_them() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(411_204), "412 KB");
        assert_eq!(format_size(10_243_539), "10.2 MB");
    }
}
