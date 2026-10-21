//! Image blocks for requests (host mode `CLAW_IMAGES=1`, see `runtime::images`): session image
//! references become base64 `image` blocks, rehydrated from the workdir; an image that follows a
//! tool result goes inside that result.
use std::path::PathBuf;

use runtime::images::{images_to_omit, omitted_note, rehydrate};
use runtime::{ConversationMessage, ImageRef};

use crate::types::{ImageSource, InputContentBlock, ToolResultContentBlock};

/// Turns a session's image references into request blocks, oldest first: the oldest beyond
/// `runtime::images::MAX_IMAGES_PER_REQUEST` become a text note.
pub struct ImageAttacher {
    omit: usize,
    root: PathBuf,
}

impl ImageAttacher {
    #[must_use]
    pub fn new(messages: &[ConversationMessage]) -> Self {
        Self {
            omit: images_to_omit(messages),
            root: std::env::current_dir().unwrap_or_default(),
        }
    }

    /// Appends `image` to `content`: inside the last block when that is a tool result, else as
    /// a block of its own; a text note when it is omitted or no longer available.
    pub fn attach(&mut self, content: &mut Vec<InputContentBlock>, image: &ImageRef) {
        let source = if self.omit > 0 {
            self.omit -= 1;
            Err(omitted_note(&image.path))
        } else {
            rehydrate(&self.root, image).map(|processed| {
                ImageSource::base64(processed.media_type.clone(), processed.base64())
            })
        };
        if let Some(InputContentBlock::ToolResult { content: inner, .. }) = content.last_mut() {
            inner.push(match source {
                Ok(source) => ToolResultContentBlock::Image { source },
                Err(text) => ToolResultContentBlock::Text { text },
            });
            return;
        }
        content.push(match source {
            Ok(source) => InputContentBlock::Image { source },
            Err(text) => InputContentBlock::Text { text },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ContentBlock, MessageRole};

    fn image(path: &str) -> ImageRef {
        ImageRef {
            path: path.to_string(),
            sha256: "0".repeat(64),
            media_type: "image/png".to_string(),
            page: None,
            width: 1,
            height: 1,
        }
    }

    #[test]
    fn missing_image_inside_a_tool_result_becomes_a_note_there() {
        let messages = vec![ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::Image {
                image: image("gone.png"),
            }],
            usage: None,
        }];
        let mut attacher = ImageAttacher::new(&messages);
        let mut content = vec![InputContentBlock::ToolResult {
            tool_use_id: "t".into(),
            content: vec![ToolResultContentBlock::Text { text: "x".into() }],
            is_error: false,
        }];
        attacher.attach(&mut content, &image("gone.png"));
        let InputContentBlock::ToolResult { content: inner, .. } = &content[0] else {
            panic!("tool result expected");
        };
        assert_eq!(inner.len(), 2);
        assert!(
            matches!(&inner[1], ToolResultContentBlock::Text { text } if text.contains("gone.png is no longer available"))
        );
    }

    #[test]
    fn image_blocks_serialize_in_anthropic_format() {
        let block = InputContentBlock::Image {
            source: ImageSource::base64("image/png", "AAAA"),
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})
        );
        let block = InputContentBlock::ToolResult {
            tool_use_id: "t".into(),
            content: vec![
                ToolResultContentBlock::Text {
                    text: "Image a.png".into(),
                },
                ToolResultContentBlock::Image {
                    source: ImageSource::base64("image/jpeg", "BBBB"),
                },
            ],
            is_error: false,
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap()["content"][1],
            serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}})
        );
        let back: InputContentBlock =
            serde_json::from_value(serde_json::to_value(&block).unwrap()).unwrap();
        assert_eq!(back, block);
    }
}
