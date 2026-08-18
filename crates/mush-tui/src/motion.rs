#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Word {
    Small,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

struct Text {
    chars: Vec<char>,
    starts: Vec<usize>,
}

impl Text {
    fn new(rows: &[String]) -> Self {
        let mut chars = Vec::new();
        let mut starts = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                chars.push('\n');
            }
            starts.push(chars.len());
            chars.extend(row.chars());
        }
        Self { chars, starts }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn index_of(&self, (line, column): (usize, usize)) -> Option<usize> {
        let start = *self.starts.get(line)?;
        Some(start + column)
    }

    fn position(&self, index: usize) -> (usize, usize) {
        let line = self.starts.partition_point(|start| *start <= index) - 1;
        (line, index - self.starts[line])
    }

    fn class(&self, index: usize, word: Word) -> Class {
        let Some(c) = self.chars.get(index) else {
            return Class::Blank;
        };
        if c.is_whitespace() {
            Class::Blank
        } else if word == Word::Big || c.is_alphanumeric() || *c == '_' {
            Class::Word
        } else {
            Class::Punct
        }
    }
}

pub(crate) fn first_non_blank(row: &str) -> usize {
    row.chars().take_while(|c| c.is_whitespace()).count()
}

pub(crate) fn last_cell(row: &str) -> usize {
    row.chars().count().saturating_sub(1)
}

pub(crate) fn word_forward(rows: &[String], at: (usize, usize), word: Word) -> (usize, usize) {
    let text = Text::new(rows);
    let Some(start) = text.index_of(at) else {
        return at;
    };
    let mut i = start;
    let from = text.class(i, word);
    if from != Class::Blank {
        while i + 1 < text.len() && text.class(i + 1, word) == from {
            i += 1;
        }
        i += 1;
    }
    while i < text.len() && text.class(i, word) == Class::Blank {
        i += 1;
    }
    text.position(i.min(text.len().saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(rows: &[&str]) -> Vec<String> {
        rows.iter().map(std::string::ToString::to_string).collect()
    }

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

    #[test]
    fn a_word_forward_lands_on_the_next_word() {
        let rows = rows(&[" hello world"]);

        assert_eq!(word_forward(&rows, (0, 1), Word::Small), (0, 7));
    }

    #[test]
    fn a_small_word_stops_at_punctuation_and_a_big_one_does_not() {
        let rows = rows(&[" foo.bar baz"]);

        assert_eq!(word_forward(&rows, (0, 1), Word::Small), (0, 4));
        assert_eq!(word_forward(&rows, (0, 1), Word::Big), (0, 9));
    }
}
