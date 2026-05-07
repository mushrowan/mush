//! shared layout helpers for popup widgets.
//!
//! all centred-overlay popups (login, model, session, settings, ...)
//! historically duplicated the same percentage-of-area-with-clamp
//! math. that math had a sharp edge: a hard `clamp` lower bound
//! could exceed the available area on narrow terminals, producing a
//! popup whose right or bottom border landed one cell past the
//! buffer and panicking inside ratatui's `Buffer::index_of`.
//!
//! [`centred_popup`] does the math once, in one place, with the
//! buffer cap baked in. callers describe their preferred size as
//! a percentage plus min/max bounds; the returned rect is always
//! fully contained in `area`.

use ratatui::layout::Rect;

/// percentage-of-area dimensions with min/max clamps. min and max
/// are absolute cell counts; both are bounded by the available area
/// so a too-large min never overflows the buffer
#[derive(Debug, Clone, Copy)]
pub struct PopupSize {
    pub width_pct: u16,
    pub height_pct: u16,
    pub width_min: u16,
    pub width_max: u16,
    pub height_min: u16,
    pub height_max: u16,
}

/// compute a centred popup rect inside `area`. the returned rect is
/// guaranteed to satisfy `popup.right() <= area.right()` and
/// `popup.bottom() <= area.bottom()` even when `size`'s minimum
/// bounds exceed the available space (in that case the popup is
/// clamped down to fit).
#[must_use]
pub fn centred_popup(area: Rect, size: PopupSize) -> Rect {
    let width = (area.width * size.width_pct / 100)
        .clamp(size.width_min, size.width_max)
        .min(area.width);
    let height = (area.height * size.height_pct / 100)
        .clamp(size.height_min, size.height_max)
        .min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(min_w: u16, max_w: u16, min_h: u16, max_h: u16) -> PopupSize {
        PopupSize {
            width_pct: 80,
            height_pct: 60,
            width_min: min_w,
            width_max: max_w,
            height_min: min_h,
            height_max: max_h,
        }
    }

    #[test]
    fn popup_fits_inside_area_when_min_exceeds_area() {
        // the original off-by-one: a 40-col min on a 39-col area
        // produced a 40-col popup that overflowed the buffer. the
        // helper must clamp the popup down to the available width
        let area = Rect::new(0, 0, 39, 26);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert!(popup.right() <= area.right(), "popup overflowed right edge");
        assert!(
            popup.bottom() <= area.bottom(),
            "popup overflowed bottom edge"
        );
        assert_eq!(popup.width, 39, "popup width should be capped to area");
    }

    #[test]
    fn popup_height_capped_to_area_when_min_exceeds_area() {
        let area = Rect::new(0, 0, 80, 8);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert!(popup.bottom() <= area.bottom());
        assert_eq!(popup.height, 8);
    }

    #[test]
    fn popup_centred_when_smaller_than_area() {
        let area = Rect::new(0, 0, 100, 50);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        // 80% of 100 = 80, clamped to (40, 100) → 80; centred → x=10
        assert_eq!(popup.width, 80);
        assert_eq!(popup.x, 10);
        // 60% of 50 = 30, clamped to (10, 30) → 30; centred → y=10
        assert_eq!(popup.height, 30);
        assert_eq!(popup.y, 10);
    }

    #[test]
    fn popup_respects_non_zero_area_origin() {
        let area = Rect::new(5, 3, 100, 50);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert_eq!(popup.x, 5 + 10);
        assert_eq!(popup.y, 3 + 10);
    }
}
