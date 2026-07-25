//! selection, copying, and block segmentation
//!
//! scroll mode selects either whole messages or individual blocks (code
//! fences and the prose between them). block segmentation is cached
//! because both navigation and rendering need the same split.

use super::*;

impl App {
    /// whether visual selection mode is active (v in scroll mode)
    pub fn has_selection(&self) -> bool {
        self.navigation.selection_anchor.is_some() && self.navigation.selected_message.is_some()
    }

    /// get the inclusive selection range (min..=max), if active
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.navigation.selection_anchor?;
        let cursor = self.navigation.selected_message?;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// Build the exact text copied by scroll-mode yank and the status
    /// shown afterwards. Message mode copies only message body text. Tool
    /// previews and hidden/visible thinking are deliberately excluded for
    /// now, so the copied payload matches the highlighted message bodies.
    /// Block mode copies the selected paragraph or fenced code body.
    pub fn copy_selection(&mut self) -> Option<CopySelection> {
        match self.navigation.scroll_unit {
            ScrollUnit::Block => self.copy_block_selection(),
            ScrollUnit::Message => self.copy_message_selection(),
        }
    }

    fn copy_block_selection(&self) -> Option<CopySelection> {
        let blocks = self.blocks();
        let block = blocks.get(self.navigation.selected_block?)?;
        let label = match &block.kind {
            BlockKind::Paragraph => "paragraph".to_string(),
            BlockKind::Fenced { lang } => lang
                .as_deref()
                .map(|l| format!("{l} block"))
                .unwrap_or_else(|| "code block".into()),
        };
        Some(CopySelection {
            text: block.content.clone(),
            status: format!("copied {label} ({} chars)", block.content.chars().count()),
        })
    }

    fn copy_message_selection(&mut self) -> Option<CopySelection> {
        if let Some((start, end)) = self.selection_range() {
            let text: String = self.messages[start..=end]
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let count = end - start + 1;
            self.navigation.selection_anchor = None;
            return Some(CopySelection {
                status: format!(
                    "copied {count} message bodies ({} chars)",
                    text.chars().count()
                ),
                text,
            });
        }

        let msg = self.messages.get(self.navigation.selected_message?)?;
        Some(CopySelection {
            text: msg.content.clone(),
            status: format!(
                "copied message body ({} chars)",
                msg.content.chars().count()
            ),
        })
    }

    /// extract all fenced code blocks from conversation messages
    pub fn code_blocks(&self) -> Vec<CodeBlock> {
        let mut blocks = Vec::new();
        for (msg_idx, msg) in self.messages.iter().enumerate() {
            let mut in_block = false;
            let mut lang = None;
            let mut lines: Vec<&str> = Vec::new();

            for raw_line in msg.content.lines() {
                if raw_line.starts_with("```") {
                    if in_block {
                        blocks.push(CodeBlock {
                            msg_idx,
                            content: lines.join("\n"),
                            lang: lang.take(),
                        });
                        lines.clear();
                        in_block = false;
                    } else {
                        let tag = raw_line.trim_start_matches('`').trim();
                        lang = if tag.is_empty() {
                            None
                        } else {
                            Some(tag.to_string())
                        };
                        in_block = true;
                    }
                    continue;
                }
                if in_block {
                    lines.push(raw_line);
                }
            }

            // unclosed fence
            if in_block && !lines.is_empty() {
                blocks.push(CodeBlock {
                    msg_idx,
                    content: lines.join("\n"),
                    lang: lang.take(),
                });
            }
        }
        blocks
    }

    /// extract every selectable block across all conversation messages.
    ///
    /// a "block" is one of:
    /// - a paragraph: a contiguous run of non-blank lines outside of
    ///   any fenced code section, joined with newlines
    /// - the body of a fenced ``` block (without the fence markers)
    ///
    /// blocks are returned in document order across messages. used by
    /// scroll-mode block navigation (j/k cycles through these) and the
    /// y-yank action (copies the selected block's content).
    pub fn blocks(&self) -> Rc<Vec<Block>> {
        #[cfg(test)]
        BLOCKS_CALLS.with(|calls| calls.set(calls.get() + 1));

        let fingerprint = self.blocks_fingerprint();
        if let Some((key, blocks)) = self.render_state.blocks_cache.borrow().as_ref()
            && *key == fingerprint
        {
            return Rc::clone(blocks);
        }

        let blocks = Rc::new(self.compute_blocks());
        *self.render_state.blocks_cache.borrow_mut() = Some((fingerprint, Rc::clone(&blocks)));
        blocks
    }

    /// cheap O(n) fingerprint of the message contents that feed `blocks()`.
    /// block structure depends only on each message's `content`, which is
    /// write-once except for the streaming tail, so hashing the per-message
    /// content lengths is a sufficient invalidation key (mirrors the
    /// byte-length keys the height and content-hash caches already use)
    fn blocks_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        self.messages.len().hash(&mut hasher);
        for msg in &self.messages {
            msg.content.len().hash(&mut hasher);
        }
        hasher.finish()
    }

    fn compute_blocks(&self) -> Vec<Block> {
        #[cfg(test)]
        BLOCKS_COMPUTES.with(|calls| calls.set(calls.get() + 1));

        let mut blocks = Vec::new();
        for (msg_idx, msg) in self.messages.iter().enumerate() {
            let mut in_fence = false;
            let mut fence_lang: Option<String> = None;
            let mut fence_lines: Vec<&str> = Vec::new();
            let mut para_lines: Vec<&str> = Vec::new();

            let flush_paragraph = |para_lines: &mut Vec<&str>, blocks: &mut Vec<Block>| {
                if !para_lines.is_empty() {
                    blocks.push(Block {
                        msg_idx,
                        content: para_lines.join("\n"),
                        kind: BlockKind::Paragraph,
                    });
                    para_lines.clear();
                }
            };

            for raw_line in msg.content.lines() {
                if raw_line.starts_with("```") {
                    if in_fence {
                        // closing fence: emit fence body
                        blocks.push(Block {
                            msg_idx,
                            content: fence_lines.join("\n"),
                            kind: BlockKind::Fenced {
                                lang: fence_lang.take(),
                            },
                        });
                        fence_lines.clear();
                        in_fence = false;
                    } else {
                        // opening fence: flush any pending paragraph first
                        flush_paragraph(&mut para_lines, &mut blocks);
                        let tag = raw_line.trim_start_matches('`').trim();
                        fence_lang = if tag.is_empty() {
                            None
                        } else {
                            Some(tag.to_string())
                        };
                        in_fence = true;
                    }
                    continue;
                }

                if in_fence {
                    fence_lines.push(raw_line);
                } else if raw_line.trim().is_empty() {
                    // blank line: paragraph boundary
                    flush_paragraph(&mut para_lines, &mut blocks);
                } else {
                    para_lines.push(raw_line);
                }
            }

            // tail flushes: any pending paragraph + any unclosed fence
            flush_paragraph(&mut para_lines, &mut blocks);
            if in_fence && !fence_lines.is_empty() {
                blocks.push(Block {
                    msg_idx,
                    content: fence_lines.join("\n"),
                    kind: BlockKind::Fenced { lang: fence_lang },
                });
            }
        }
        blocks
    }
}
