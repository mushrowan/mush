//! shared MCP result conversion

use mush_agent::tool::ToolResult;
use mush_ai::types::{
    ImageContent, ImageMimeType, TextContent, ToolOutcome, ToolResultContentPart,
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// convert an MCP `CallToolResult` into an agent `ToolResult`
///
/// content blocks keep their original order. text-like blocks (plain text,
/// embedded text resources, resource links) are merged into runs so a
/// narration split across several blocks reads as one message. images are
/// forwarded natively when the api can carry the mime type.
pub fn convert_call_result(result: CallToolResult) -> ToolResult {
    let mut content = Vec::new();

    for block in &result.content {
        match block {
            ContentBlock::Text(text) => push_text(&mut content, &text.text),
            ContentBlock::Image(image) => match ImageMimeType::from_mime_type(&image.mime_type) {
                Some(mime_type) => content.push(ToolResultContentPart::Image(ImageContent {
                    data: image.data.clone(),
                    mime_type,
                })),
                None => push_text(
                    &mut content,
                    &format!("(image omitted, unsupported type {})", image.mime_type),
                ),
            },
            ContentBlock::Audio(audio) => push_text(
                &mut content,
                &format!("(audio omitted, {} not supported)", audio.mime_type),
            ),
            ContentBlock::Resource(embedded) => push_resource(&mut content, &embedded.resource),
            ContentBlock::ResourceLink(resource) => push_text(
                &mut content,
                &format!("(resource link {} at {})", resource.name, resource.uri),
            ),
            _ => push_text(&mut content, "(unrecognised content block)"),
        }
    }

    if let Some(structured) = &result.structured_content
        && !content
            .iter()
            .any(|part| matches!(part, ToolResultContentPart::Text(_)))
    {
        push_text(
            &mut content,
            &serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string()),
        );
    }

    if content.is_empty() {
        push_text(&mut content, "(no output)");
    }

    let outcome = if result.is_error.unwrap_or(false) {
        ToolOutcome::Error
    } else {
        ToolOutcome::Success
    };

    ToolResult { content, outcome }
}

/// append text, extending the trailing run rather than adding a new part
fn push_text(content: &mut Vec<ToolResultContentPart>, text: &str) {
    match content.last_mut() {
        Some(ToolResultContentPart::Text(last)) => {
            last.text.push('\n');
            last.text.push_str(text);
        }
        _ => content.push(ToolResultContentPart::Text(TextContent {
            text: text.to_string(),
        })),
    }
}

fn push_resource(content: &mut Vec<ToolResultContentPart>, resource: &ResourceContents) {
    match resource {
        ResourceContents::TextResourceContents { uri, text, .. } => {
            push_text(content, &format!("{uri}:\n{text}"));
        }
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => {
            let image = mime_type
                .as_deref()
                .and_then(ImageMimeType::from_mime_type)
                .map(|mime_type| ImageContent {
                    data: blob.clone(),
                    mime_type,
                });
            match image {
                Some(image) => content.push(ToolResultContentPart::Image(image)),
                None => push_text(
                    content,
                    &format!(
                        "(binary resource {uri}, {} bytes of base64{})",
                        blob.len(),
                        mime_type
                            .as_deref()
                            .map(|m| format!(", type {m}"))
                            .unwrap_or_default()
                    ),
                ),
            }
        }
        _ => push_text(content, "(unrecognised resource)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{ImageMimeType, ToolResultContentPart};
    use rmcp::model::ContentBlock;

    #[test]
    fn image_content_survives_conversion() {
        let result = CallToolResult::success(vec![ContentBlock::image("YWJj", "image/jpeg")]);
        let converted = convert_call_result(result);

        assert_eq!(
            converted.content,
            [ToolResultContentPart::Image(mush_ai::types::ImageContent {
                data: "YWJj".into(),
                mime_type: ImageMimeType::Jpeg,
            })]
        );
    }

    #[test]
    fn text_and_image_keep_both_and_stay_ordered() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("rendered the lounge"),
            ContentBlock::image("cG5n", "image/png"),
        ]);
        let converted = convert_call_result(result);

        assert_eq!(converted.content.len(), 2);
        assert!(matches!(
            converted.content[0],
            ToolResultContentPart::Text(_)
        ));
        assert!(matches!(
            converted.content[1],
            ToolResultContentPart::Image(_)
        ));
    }

    #[test]
    fn unsupported_image_mime_degrades_to_a_note() {
        let result = CallToolResult::success(vec![ContentBlock::image("Ym1w", "image/bmp")]);
        let converted = convert_call_result(result);

        let ToolResultContentPart::Text(text) = &converted.content[0] else {
            panic!("expected a text note, got {:?}", converted.content[0]);
        };
        assert!(
            text.text.contains("image/bmp"),
            "note should name the mime: {}",
            text.text
        );
    }

    #[test]
    fn embedded_text_resource_surfaces_its_text() {
        let result = CallToolResult::success(vec![ContentBlock::embedded_text(
            "file:///home.sh3d",
            "wall count: 4",
        )]);
        let converted = convert_call_result(result);

        let ToolResultContentPart::Text(text) = &converted.content[0] else {
            panic!("expected text");
        };
        assert!(text.text.contains("wall count: 4"), "got {}", text.text);
    }

    #[test]
    fn structured_content_is_surfaced_when_there_is_no_text() {
        // servers are allowed to answer with structuredContent alone
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [],
            "structuredContent": { "walls": 4 },
        }))
        .unwrap();
        let converted = convert_call_result(result);

        let ToolResultContentPart::Text(text) = &converted.content[0] else {
            panic!("expected text");
        };
        assert!(text.text.contains("walls"), "got {}", text.text);
    }

    #[test]
    fn error_results_keep_the_error_outcome() {
        let result = CallToolResult::error(vec![ContentBlock::text("no home open")]);
        let converted = convert_call_result(result);

        assert!(converted.outcome.is_error());
    }

    #[test]
    fn empty_content_reports_no_output() {
        let converted = convert_call_result(CallToolResult::success(vec![]));

        let ToolResultContentPart::Text(text) = &converted.content[0] else {
            panic!("expected text");
        };
        assert_eq!(text.text, "(no output)");
    }
}
