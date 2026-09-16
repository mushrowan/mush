pub const WORKING_ICON_FRAMES: [&str; 4] = ["◇", "◈", "◆", "◈"];
pub const WORKING_ICON_TICKS: usize = 8;

#[must_use]
pub fn working_icon(tick: usize) -> &'static str {
    WORKING_ICON_FRAMES[(tick / WORKING_ICON_TICKS) % WORKING_ICON_FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_pulses_through_its_frames_in_order() {
        let frames: Vec<&str> = (0..4)
            .map(|i| working_icon(i * WORKING_ICON_TICKS))
            .collect();

        assert_eq!(frames, WORKING_ICON_FRAMES);
    }

    #[test]
    fn a_frame_holds_for_its_tick_budget() {
        assert_eq!(working_icon(0), working_icon(WORKING_ICON_TICKS - 1));
        assert_ne!(working_icon(0), working_icon(WORKING_ICON_TICKS));
    }

    #[test]
    fn the_cycle_wraps() {
        assert_eq!(
            working_icon(WORKING_ICON_FRAMES.len() * WORKING_ICON_TICKS),
            working_icon(0)
        );
    }
}
