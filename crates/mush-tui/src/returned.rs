use mush_runtime::ReturnedPrompt;

use crate::app::App;

pub fn apply(app: &mut App, returned: ReturnedPrompt) {
    let ReturnedPrompt {
        text,
        mut images,
        error,
    } = returned;
    let (text, notes) = crate::pasted_image::split_notes(&text);
    for (index, note) in notes {
        if let Some(image) = images.get_mut(index) {
            image.note = Some(note);
        }
    }
    for image in &mut images {
        image.dimensions = dimensions(&image.data);
    }
    app.input.restore_returned(&text, images);
    app.push_system_message(format!(
        "not sent: {}\nthe message is back in the composer, so change or remove what was rejected and send it again",
        rejection(&error)
    ));
    app.status = Some("message returned to the composer".into());
}

fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn rejection(error: &str) -> String {
    error
        .find('{')
        .and_then(|start| serde_json::from_str::<serde_json::Value>(&error[start..]).ok())
        .and_then(|body| {
            body.pointer("/error/message")
                .or_else(|| body.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_buffer::{IMAGE_PLACEHOLDER, PendingImage};
    use mush_ai::types::{ImageMimeType, TokenCount};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn returned(text: &str, data: Vec<u8>) -> ReturnedPrompt {
        ReturnedPrompt {
            text: text.into(),
            images: vec![PendingImage {
                data,
                mime_type: ImageMimeType::Png,
                dimensions: None,
                note: None,
            }],
            error: r#"request failed: anthropic returned 400 Bad Request (application/json): {"type":"error","error":{"type":"invalid_request_error","message":"messages.711.content.1.image.source.base64: image exceeds 10 MB maximum"}}"#.into(),
        }
    }

    #[test]
    fn a_returned_message_lands_in_the_composer_with_its_image_and_the_reason_is_shown() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let data = png(30, 20);

        apply(
            &mut app,
            returned("[image 1] oh i meant to send this", data.clone()),
        );

        assert_eq!(
            app.input.text,
            format!("{IMAGE_PLACEHOLDER} oh i meant to send this")
        );
        assert_eq!(app.input.cursor, app.input.text.len());
        assert_eq!(app.input.images.len(), 1);
        assert_eq!(app.input.images[0].data, data);
        assert_eq!(app.input.images[0].dimensions, Some((30, 20)));
        let shown = &app.messages.last().unwrap().content;
        assert!(
            shown.starts_with(
                "not sent: messages.711.content.1.image.source.base64: image exceeds 10 MB maximum\n"
            ),
            "{shown}"
        );
    }

    #[test]
    fn a_draft_typed_meanwhile_stays_after_the_returned_message() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.insert_str("oop");

        apply(&mut app, returned("[image 1] look", png(4, 4)));

        assert_eq!(app.input.text, format!("{IMAGE_PLACEHOLDER} look\n\noop"));
        assert_eq!(app.input.cursor, format!("{IMAGE_PLACEHOLDER} look").len());
    }

    #[test]
    fn a_resize_note_goes_back_onto_its_image_instead_of_staying_in_the_text() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        apply(
            &mut app,
            returned(
                "[image 1] look\n\n[image 1: 3840x2160 PNG 10.2 MB → 2000x1125 JPEG 412 KB, original at /o.png]",
                png(4, 4),
            ),
        );

        assert_eq!(app.input.text, format!("{IMAGE_PLACEHOLDER} look"));
        assert_eq!(
            app.input.images[0].note.as_deref(),
            Some("3840x2160 PNG 10.2 MB → 2000x1125 JPEG 412 KB, original at /o.png")
        );
    }

    #[test]
    fn an_error_without_a_json_body_is_shown_as_it_is() {
        assert_eq!(
            rejection("stream error: invalid_image"),
            "stream error: invalid_image"
        );
    }
}
