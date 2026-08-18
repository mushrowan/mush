pub(crate) fn first_non_blank(row: &str) -> usize {
    row.chars().take_while(|c| c.is_whitespace()).count()
}

pub(crate) fn last_cell(row: &str) -> usize {
    row.chars().count().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_non_blank_skips_the_indent() {
        assert_eq!(first_non_blank("   hello"), 3);
        assert_eq!(first_non_blank("hello"), 0);
        assert_eq!(first_non_blank("   "), 3);
    }

    #[test]
    fn the_last_cell_is_the_final_character() {
        assert_eq!(last_cell("   hello"), 7);
        assert_eq!(last_cell(""), 0);
    }
}
