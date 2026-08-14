use ratatui::style::{Color, Style};
use ratatui::text::Span;

#[must_use]
pub fn interpolate_palette(keys: &[Color], between_each: usize) -> Vec<Color> {
    if keys.is_empty() {
        return Vec::new();
    }
    let stride = between_each + 1;
    let mut out = Vec::with_capacity(keys.len() * stride);
    for i in 0..keys.len() {
        let a = color_to_rgb(keys[i]);
        let b = color_to_rgb(keys[(i + 1) % keys.len()]);
        out.push(Color::Rgb(a.0, a.1, a.2));
        for step in 1..=between_each {
            let t = step as f32 / stride as f32;
            out.push(Color::Rgb(
                lerp(a.0, b.0, t),
                lerp(a.1, b.1, t),
                lerp(a.2, b.2, t),
            ));
        }
    }
    out
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    let af = f32::from(a);
    let bf = f32::from(b);
    (af + (bf - af) * t).round().clamp(0.0, 255.0) as u8
}

fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(idx) => indexed_to_rgb(idx),
        _ => (128, 128, 128),
    }
}

fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    const CUBE_AXIS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if idx < 16 {
        ANSI_16[idx as usize]
    } else if idx < 232 {
        let i = idx - 16;
        let r = (i / 36) as usize;
        let g = ((i % 36) / 6) as usize;
        let b = (i % 6) as usize;
        (CUBE_AXIS[r], CUBE_AXIS[g], CUBE_AXIS[b])
    } else {
        let v = 8u8.saturating_add((idx - 232).saturating_mul(10));
        (v, v, v)
    }
}

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
        let prev = shimmer_spans("abc", &palette(), 0, Style::default());
        let next = shimmer_spans("abc", &palette(), 1, Style::default());
        assert_eq!(fg_of(&next[1]), fg_of(&prev[2]));
        assert_eq!(fg_of(&next[2]), fg_of(&prev[0]));
    }

    #[test]
    fn shimmer_falls_back_to_single_span_for_empty_palette() {
        let base = Style::default().fg(Color::Yellow);
        let spans = shimmer_spans("hello", &[], 0, base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello");
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn shimmer_preserves_base_modifiers() {
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

    fn rgb_distance(a: &Color, b: &Color) -> f32 {
        let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (*a, *b) else {
            panic!("expected rgb stops, got {:?} and {:?}", a, b);
        };
        let dr = f32::from(ar) - f32::from(br);
        let dg = f32::from(ag) - f32::from(bg);
        let db = f32::from(ab) - f32::from(bb);
        (dr * dr + dg * dg + db * db).sqrt()
    }

    #[test]
    fn interpolate_palette_with_zero_intermediates_returns_keys_as_rgb() {
        let keys = vec![Color::Rgb(10, 20, 30), Color::Rgb(200, 100, 50)];
        let out = interpolate_palette(&keys, 0);
        assert_eq!(out, keys);
    }

    #[test]
    fn interpolate_palette_inserts_midpoints_and_wraps_to_first() {
        let keys = vec![Color::Rgb(0, 0, 0), Color::Rgb(100, 100, 100)];
        let out = interpolate_palette(&keys, 1);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], Color::Rgb(0, 0, 0));
        assert_eq!(out[1], Color::Rgb(50, 50, 50));
        assert_eq!(out[2], Color::Rgb(100, 100, 100));
        assert_eq!(out[3], Color::Rgb(50, 50, 50));
    }

    #[test]
    fn interpolate_palette_keeps_adjacent_distance_well_below_unsmoothed() {
        let keys = vec![
            Color::Rgb(95, 255, 255),
            Color::Rgb(0, 175, 255),
            Color::Rgb(135, 95, 255),
            Color::Rgb(215, 95, 215),
            Color::Rgb(255, 95, 175),
        ];
        let out = interpolate_palette(&keys, 4);
        assert_eq!(out.len(), 25);

        let mut max_d = 0f32;
        for i in 0..out.len() {
            let d = rgb_distance(&out[i], &out[(i + 1) % out.len()]);
            max_d = max_d.max(d);
        }
        assert!(
            max_d < 55.0,
            "smoothed adjacent distance was {max_d}, expected < 55"
        );
    }

    #[test]
    fn default_theme_palette_has_many_smooth_stops() {
        let theme = crate::theme::Theme::default();

        assert_eq!(theme.shimmer.len(), 125);

        let mut max_d = 0f32;
        for i in 0..theme.shimmer.len() {
            let d = rgb_distance(
                &theme.shimmer[i],
                &theme.shimmer[(i + 1) % theme.shimmer.len()],
            );
            max_d = max_d.max(d);
        }
        assert!(
            max_d < 12.0,
            "default theme shimmer step distance was {max_d}, expected < 12"
        );
    }

    #[test]
    fn interpolate_palette_handles_indexed_input() {
        let keys = vec![Color::Indexed(51), Color::Indexed(207)];
        let out = interpolate_palette(&keys, 0);
        assert!(matches!(out[0], Color::Rgb(_, _, _)));
        assert!(matches!(out[1], Color::Rgb(_, _, _)));
    }
}
