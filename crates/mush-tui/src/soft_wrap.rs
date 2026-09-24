use ratatui::style::Modifier;
use ratatui::text::Line;

const CONTINUES: Modifier = Modifier::HIDDEN;
const AFTER_SPACE: Modifier = Modifier::RAPID_BLINK;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Break {
    AtSpace,
    MidWord,
}

impl Break {
    #[must_use]
    pub(crate) fn joiner(self) -> &'static str {
        match self {
            Self::AtSpace => " ",
            Self::MidWord => "",
        }
    }
}

#[must_use]
pub(crate) fn continuation(line: Line<'_>, after: Break) -> Line<'_> {
    let marker = match after {
        Break::AtSpace => CONTINUES | AFTER_SPACE,
        Break::MidWord => CONTINUES,
    };
    let style = line.style.remove_modifier(marker);
    line.style(style)
}

#[must_use]
pub(crate) fn break_before(line: &Line<'_>) -> Option<Break> {
    let sub = line.style.sub_modifier;
    if !sub.contains(CONTINUES) {
        return None;
    }
    Some(if sub.contains(AFTER_SPACE) {
        Break::AtSpace
    } else {
        Break::MidWord
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    #[test]
    fn a_marked_row_says_how_it_joins_and_keeps_its_look() {
        let plain = Line::styled("text", Style::default().fg(Color::Red));
        assert_eq!(break_before(&plain), None);

        let spaced = continuation(plain.clone(), Break::AtSpace);
        assert_eq!(break_before(&spaced), Some(Break::AtSpace));
        assert_eq!(spaced.style.fg, Some(Color::Red));
        assert!(!spaced.style.add_modifier.contains(Modifier::HIDDEN));

        let split = continuation(plain, Break::MidWord);
        assert_eq!(break_before(&split), Some(Break::MidWord));
    }
}
