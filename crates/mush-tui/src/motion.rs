#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Word {
    Small,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    WordForward(Word),
    WordBack(Word),
    WordEnd(Word),
    WordEndBack(Word),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Within,
    Crossed,
    OntoLineEnd,
    Stuck,
}

impl Step {
    fn left_the_line(self) -> bool {
        matches!(self, Self::Crossed | Self::OntoLineEnd)
    }
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
        Some(start + column.min(self.line_end(line) - start))
    }

    fn position(&self, index: usize) -> (usize, usize) {
        let line = self.line_of(index);
        (line, index - self.starts[line])
    }

    fn line_of(&self, index: usize) -> usize {
        self.starts.partition_point(|start| *start <= index) - 1
    }

    fn column(&self, index: usize) -> usize {
        index - self.starts[self.line_of(index)]
    }

    fn line_end(&self, line: usize) -> usize {
        self.starts
            .get(line + 1)
            .map_or(self.len(), |next| next - 1)
    }

    fn is_line_end(&self, index: usize) -> bool {
        self.chars.get(index).is_none_or(|c| *c == '\n')
    }

    fn on_empty_line(&self, index: usize) -> bool {
        self.column(index) == 0 && self.is_line_end(index)
    }

    fn on_last_line(&self, index: usize) -> bool {
        self.line_of(index) + 1 == self.starts.len()
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

    fn inc(&self, index: usize) -> (usize, Step) {
        match self.chars.get(index) {
            None => (index, Step::Stuck),
            Some('\n') => (index + 1, Step::Crossed),
            Some(_) if self.is_line_end(index + 1) => (index + 1, Step::OntoLineEnd),
            Some(_) => (index + 1, Step::Within),
        }
    }

    fn dec(&self, index: usize) -> (usize, Step) {
        match index.checked_sub(1) {
            None => (index, Step::Stuck),
            Some(previous) if self.chars[previous] == '\n' => (previous, Step::Crossed),
            Some(previous) => (previous, Step::Within),
        }
    }

    fn rest(&self, index: usize) -> (usize, usize) {
        if self.is_line_end(index) && self.column(index) > 0 {
            self.position(index - 1)
        } else {
            self.position(index)
        }
    }

    fn forward_word(&self, mut at: usize, word: Word, count: usize, stop_at_eol: bool) -> usize {
        for left in (0..count).rev() {
            let eol = stop_at_eol && left == 0;
            let class = self.class(at, word);
            let last_line = self.on_last_line(at);
            let (next, step) = self.inc(at);
            at = next;
            if step == Step::Stuck || (step.left_the_line() && (last_line || eol)) {
                return at;
            }
            if class != Class::Blank {
                while self.class(at, word) == class {
                    let (next, step) = self.inc(at);
                    at = next;
                    if step == Step::Stuck || (step.left_the_line() && eol) {
                        return at;
                    }
                }
            }
            while self.class(at, word) == Class::Blank && !self.on_empty_line(at) {
                let (next, step) = self.inc(at);
                at = next;
                if step == Step::Stuck || (step.left_the_line() && eol) {
                    return at;
                }
            }
        }
        at
    }

    fn back_word(&self, mut at: usize, word: Word, count: usize) -> usize {
        for _ in 0..count {
            let (previous, step) = self.dec(at);
            if step == Step::Stuck {
                return at;
            }
            at = previous;
            while self.class(at, word) == Class::Blank {
                if self.on_empty_line(at) {
                    break;
                }
                let (previous, step) = self.dec(at);
                if step == Step::Stuck {
                    return at;
                }
                at = previous;
            }
            if self.on_empty_line(at) {
                continue;
            }
            let class = self.class(at, word);
            while self.class(at, word) == class {
                let (previous, step) = self.dec(at);
                if step == Step::Stuck {
                    return at;
                }
                at = previous;
            }
            at = self.inc(at).0;
        }
        at
    }

    fn end_word(&self, mut at: usize, word: Word, count: usize, mut stop: bool) -> usize {
        for _ in 0..count {
            let class = self.class(at, word);
            let (next, step) = self.inc(at);
            if step == Step::Stuck {
                return at;
            }
            at = next;
            if self.class(at, word) == class && class != Class::Blank {
                at = self.skip_forward(at, word, class);
            } else if !stop || class == Class::Blank {
                while self.class(at, word) == Class::Blank {
                    let (next, step) = self.inc(at);
                    if step == Step::Stuck {
                        return at;
                    }
                    at = next;
                }
                at = self.skip_forward(at, word, self.class(at, word));
            }
            at = self.dec(at).0;
            stop = false;
        }
        at
    }

    fn skip_forward(&self, mut at: usize, word: Word, class: Class) -> usize {
        while self.class(at, word) == class {
            let (next, step) = self.inc(at);
            if step == Step::Stuck {
                return at;
            }
            at = next;
        }
        at
    }

    fn back_end_word(&self, mut at: usize, word: Word, count: usize) -> usize {
        for _ in 0..count {
            let class = self.class(at, word);
            let (previous, step) = self.dec(at);
            if step == Step::Stuck {
                return at;
            }
            at = previous;
            if class != Class::Blank {
                while self.class(at, word) == class {
                    let (previous, step) = self.dec(at);
                    if step == Step::Stuck {
                        return at;
                    }
                    at = previous;
                }
            }
            while self.class(at, word) == Class::Blank && !self.on_empty_line(at) {
                let (previous, step) = self.dec(at);
                if step == Step::Stuck {
                    return at;
                }
                at = previous;
            }
        }
        at
    }
}

pub(crate) fn first_non_blank(row: &str) -> usize {
    row.chars().take_while(|c| c.is_whitespace()).count()
}

pub(crate) fn last_cell(row: &str) -> usize {
    row.chars().count().saturating_sub(1)
}

pub(crate) fn destination(
    rows: &[String],
    at: (usize, usize),
    motion: Motion,
    count: usize,
) -> Option<(usize, usize)> {
    let (line, column) = at;
    let row = |line: usize| rows.get(line).map_or("", String::as_str);
    let word_motion = |word: Word, travel: fn(&Text, usize, Word, usize) -> usize| {
        let text = Text::new(rows);
        let from = text.index_of(at)?;
        Some(text.rest(travel(&text, from, word, count)))
    };
    match motion {
        Motion::LineStart => Some((line, 0)),
        Motion::FirstNonBlank => Some((line, first_non_blank(row(line)))),
        Motion::LineEnd => {
            let line = (line + count - 1).min(rows.len().saturating_sub(1));
            Some((line, last_cell(row(line))))
        }
        Motion::Left => Some((line, column.saturating_sub(count))),
        Motion::Right => Some((line, column + count)),
        Motion::WordForward(word) => word_motion(word, |text, from, word, count| {
            text.forward_word(from, word, count, false)
        }),
        Motion::WordBack(word) => word_motion(word, Text::back_word),
        Motion::WordEnd(word) => word_motion(word, |text, from, word, count| {
            text.end_word(from, word, count, false)
        }),
        Motion::WordEndBack(word) => word_motion(word, Text::back_end_word),
        Motion::Up | Motion::Down | Motion::Top | Motion::Bottom => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(rows: &[&str]) -> Vec<String> {
        rows.iter().map(std::string::ToString::to_string).collect()
    }

    fn go(rows: &[String], at: (usize, usize), motion: Motion) -> (usize, usize) {
        destination(rows, at, motion, 1).unwrap_or(at)
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

        assert_eq!(go(&rows, (0, 1), Motion::WordForward(Word::Small)), (0, 7));
    }

    #[test]
    fn a_word_back_lands_on_the_start_of_the_one_before() {
        let rows = rows(&[" hello world"]);

        assert_eq!(go(&rows, (0, 7), Motion::WordBack(Word::Small)), (0, 1));
        assert_eq!(go(&rows, (0, 9), Motion::WordBack(Word::Small)), (0, 7));
    }

    #[test]
    fn a_word_end_lands_on_the_last_character() {
        let rows = rows(&[" hello world"]);

        assert_eq!(go(&rows, (0, 1), Motion::WordEnd(Word::Small)), (0, 5));
        assert_eq!(go(&rows, (0, 5), Motion::WordEnd(Word::Small)), (0, 11));
    }

    #[test]
    fn a_chunk_back_ignores_punctuation() {
        let rows = rows(&[" foo.bar baz"]);

        assert_eq!(go(&rows, (0, 9), Motion::WordBack(Word::Big)), (0, 1));
        assert_eq!(go(&rows, (0, 9), Motion::WordBack(Word::Small)), (0, 5));
    }

    #[test]
    fn a_small_word_stops_at_punctuation_and_a_big_one_does_not() {
        let rows = rows(&[" foo.bar baz"]);

        assert_eq!(go(&rows, (0, 1), Motion::WordForward(Word::Small)), (0, 4));
        assert_eq!(go(&rows, (0, 1), Motion::WordForward(Word::Big)), (0, 9));
    }

    #[test]
    fn word_motions_stop_on_an_empty_line() {
        let rows = rows(&["one", "", "two"]);

        assert_eq!(go(&rows, (0, 0), Motion::WordForward(Word::Small)), (1, 0));
        assert_eq!(go(&rows, (1, 0), Motion::WordForward(Word::Small)), (2, 0));
        assert_eq!(go(&rows, (2, 0), Motion::WordBack(Word::Small)), (1, 0));
        assert_eq!(go(&rows, (1, 0), Motion::WordBack(Word::Small)), (0, 0));
    }

    #[test]
    fn a_word_end_skips_empty_lines() {
        let rows = rows(&["one", "", "two"]);

        assert_eq!(go(&rows, (0, 2), Motion::WordEnd(Word::Small)), (2, 2));
    }

    #[test]
    fn w_on_the_last_word_rests_on_its_last_character() {
        let rows = rows(&["hello world"]);

        assert_eq!(go(&rows, (0, 6), Motion::WordForward(Word::Small)), (0, 10));
    }

    #[test]
    fn b_at_the_start_stays_put() {
        let rows = rows(&["hello"]);

        assert_eq!(go(&rows, (0, 0), Motion::WordBack(Word::Small)), (0, 0));
    }

    #[test]
    fn ge_goes_back_to_the_end_of_the_previous_word() {
        let rows = rows(&["foo.bar baz"]);

        assert_eq!(go(&rows, (0, 8), Motion::WordEndBack(Word::Small)), (0, 6));
        assert_eq!(go(&rows, (0, 6), Motion::WordEndBack(Word::Small)), (0, 3));
        assert_eq!(go(&rows, (0, 8), Motion::WordEndBack(Word::Big)), (0, 6));
        assert_eq!(go(&rows, (0, 6), Motion::WordEndBack(Word::Big)), (0, 0));
    }

    #[test]
    fn ge_crosses_back_over_a_line_break() {
        let rows = rows(&["foo  ", "bar"]);

        assert_eq!(go(&rows, (1, 0), Motion::WordEndBack(Word::Small)), (0, 2));
    }

    #[test]
    fn ge_stops_on_an_empty_line() {
        let rows = rows(&["foo", "", "bar"]);

        assert_eq!(go(&rows, (2, 0), Motion::WordEndBack(Word::Small)), (1, 0));
    }

    #[test]
    fn a_count_repeats_a_word_motion() {
        let rows = rows(&["one two three four"]);

        assert_eq!(
            destination(&rows, (0, 0), Motion::WordForward(Word::Small), 3),
            Some((0, 14))
        );
        assert_eq!(
            destination(&rows, (0, 14), Motion::WordEndBack(Word::Small), 2),
            Some((0, 6))
        );
    }

    #[test]
    fn a_counted_dollar_ends_further_down() {
        let rows = rows(&["one", "three"]);

        assert_eq!(destination(&rows, (0, 0), Motion::LineEnd, 2), Some((1, 4)));
    }
}
