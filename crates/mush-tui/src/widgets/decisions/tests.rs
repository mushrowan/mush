use super::*;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(0xdec1511),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_unicode_clipping_and_wrapping_respect_cell_width(
        text in proptest::collection::vec(any::<char>(), 0..512).prop_map(|chars| chars.into_iter().collect::<String>()),
        width in 0usize..160,
    ) {
        let clipped = truncate(&text, width);
        prop_assert!(width_of(&clipped) <= width, "{width} {clipped:?}");
        for line in wrap(&text, width) {
            prop_assert!(width_of(&line) <= width.max(8), "{width} {line:?}");
        }
        let spans = vec![Span::raw(text.clone()), Span::raw(text)];
        prop_assert!(spans_width(&truncate_spans(spans, width)) <= width);
    }

    #[test]
    fn random_windows_account_for_every_item_and_respect_height(
        kinds in proptest::collection::vec(0u8..4, 0..256),
        height in 0usize..64,
        offset in any::<usize>(),
    ) {
        let rows: Listing = kinds.iter().map(|kind| {
            let row = match kind { 0 => Row::Gap, 1 => Row::Header, 2 => Row::Item, _ => Row::Detail };
            (row, Line::default())
        }).collect();
        let window = Window::at(&rows, height, offset);
        prop_assert!(window.offset + window.body <= rows.len());
        prop_assert!(window.body + usize::from(window.above) + usize::from(window.below) <= height);
        prop_assert_eq!(window.above, window.offset > 0);
        prop_assert_eq!(window.below, window.offset + window.body < rows.len() && height >= 3);
        let count = |slice: &[(Row, Line<'static>)]| slice.iter().filter(|(kind, _)| *kind == Row::Item).count();
        prop_assert_eq!(
            count(&rows[..window.offset]) + count(&rows[window.offset..window.offset + window.body]) + count(&rows[window.offset + window.body..]),
            count(&rows),
        );
        for row in 0..rows.len() {
            prop_assert_eq!(window.screen_row(row).is_some(), (window.offset..window.offset + window.body).contains(&row));
            if let Some(screen) = window.screen_row(row) { prop_assert!(screen < height); }
        }
    }

    #[test]
    fn random_focused_ranges_are_reachable_without_skipping_rows(
        before in 0usize..64,
        length in 1usize..128,
        after in 0usize..64,
        height in 4usize..40,
    ) {
        let rows: Listing = (0..before + length + after).map(|_| (Row::Detail, Line::default())).collect();
        let focus = (before, before + length - 1);
        let mut seen = vec![false; length];
        for scroll in 0..=length {
            let window = Window::within(&rows, height, focus, scroll)
                .map_or_else(|| Window::around(&rows, height, focus), |(window, _)| window);
            for (index, seen) in seen.iter_mut().enumerate() {
                *seen |= window.screen_row(before + index).is_some();
            }
        }
        prop_assert!(seen.iter().all(|seen| *seen), "{before} {length} {after} {height}: {seen:?}");
    }
}
