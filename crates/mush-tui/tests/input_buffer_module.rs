use mush_ai::types::ImageMimeType;
use mush_tui::clipboard::ClipboardImage;
use mush_tui::input_buffer::{IMAGE_PLACEHOLDER, InputBuffer, PendingImage};

#[test]
fn input_buffer_module_exposes_extracted_types() {
    let mut input = InputBuffer::new();
    input.insert_str("hello world");
    input.cursor_word_left();

    assert_eq!(input.cursor, 6);
    assert_eq!(IMAGE_PLACEHOLDER.len_utf8(), 3);

    let image = PendingImage {
        data: vec![],
        mime_type: ImageMimeType::Png,
        dimensions: Some((1, 2)),
    };
    assert_eq!(image.dimensions, Some((1, 2)));
}

#[test]
fn take_text_emits_numbered_image_markers() {
    // a minimal 1x1 PNG so add_image's image crate decode succeeds
    // (we only care about the placeholder insertion + take_text output)
    let png_1x1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let mut input = InputBuffer::new();
    input.insert_str("first ");
    input.add_image(ClipboardImage {
        bytes: png_1x1.to_vec(),
        mime_type: ImageMimeType::Png,
    });
    input.insert_str(" then ");
    input.add_image(ClipboardImage {
        bytes: png_1x1.to_vec(),
        mime_type: ImageMimeType::Png,
    });
    input.insert_str(" done");

    let text = input.take_text();
    assert_eq!(
        text, "first [image 1] then [image 2] done",
        "positional markers should bind the second image reference, got {text:?}"
    );
}
