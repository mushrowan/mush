//! shimmer: render a label as per-char styled spans where the
//! foreground colour is sampled from a small palette with a phase
//! offset. advancing `phase` over time produces a wave that scrolls
//! across the text, giving busy labels (thinking…, working,
//! compacting…) a calm rainbow pulse instead of a flat dim grey.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// build per-char spans coloured from `palette` offset by `phase`.
///
/// every char `i` gets `palette[(i + phase) % len]`, so the colour at
/// position `i` next frame equals the colour at position `i+1` this
/// frame: the gradient appears to scroll right-to-left as `phase`
/// grows. `base` carries any non-fg styling (modifiers, bg) that
/// should apply uniformly to every char.
///
/// when `palette` is empty the whole text falls back to a single span
/// with `base`, so callers don't have to special-case unstyled themes.
pub fn shimmer_spans(
    text: &str,
    palette: &[Color],
    phase: usize,
    base: Style,
) -> Vec<Span<'static>> {
    if palette.is_empty() || text.is_empty() {
        return vec![Span::styled(text.to_owned(), base)];
    }
    let len = palette.len();
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let idx = (i + phase) % len;
            Span::styled(c.to_string(), base.fg(palette[idx]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Vec<Color> {
        vec![Color::Red, Color::Green, Color::Blue]
    }

    fn fg_of(span: &Span<'_>) -> Option<Color> {
        span.style.fg
    }

    #[test]
    fn shimmer_assigns_palette_in_order_at_phase_zero() {
        // baseline: each char picks up the palette entry at its own
        // index, modulo palette length. with text "abcd" and a 3-colour
        // palette we should see colours r, g, b, r in order
        let spans = shimmer_spans("abcd", &palette(), 0, Style::default());
        let colours: Vec<_> = spans.iter().map(fg_of).collect();
        assert_eq!(
            colours,
            vec![
                Some(Color::Red),
                Some(Color::Green),
                Some(Color::Blue),
                Some(Color::Red),
            ]
        );
    }

    #[test]
    fn shimmer_phase_shifts_colours_one_position_right() {
        // advancing phase by 1 means char `i` now wears the colour that
        // char `i-1` wore last frame: visually the gradient appears to
        // glide one step to the right
        let prev = shimmer_spans("abc", &palette(), 0, Style::default());
        let next = shimmer_spans("abc", &palette(), 1, Style::default());
        assert_eq!(fg_of(&next[1]), fg_of(&prev[2])); // 'b' takes 'c's old colour
        assert_eq!(fg_of(&next[2]), fg_of(&prev[0])); // 'c' wraps to 'a's old colour
    }

    #[test]
    fn shimmer_falls_back_to_single_span_for_empty_palette() {
        // unstyled themes shouldn't produce per-char span explosion;
        // one span with the base style keeps the line cheap to render
        let base = Style::default().fg(Color::Yellow);
        let spans = shimmer_spans("hello", &[], 0, base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello");
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn shimmer_preserves_base_modifiers() {
        // italic/bold etc. set on `base` must survive on every char so
        // shimmer composes cleanly with the existing thinking style
        // (italic) without callers having to re-add modifiers
        let base = Style::default().add_modifier(ratatui::style::Modifier::ITALIC);
        let spans = shimmer_spans("hi", &palette(), 0, base);
        for span in &spans {
            assert!(
                span.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::ITALIC)
            );
        }
    }
}
