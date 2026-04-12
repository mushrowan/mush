use mush_ai::types::ImageMimeType;
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
