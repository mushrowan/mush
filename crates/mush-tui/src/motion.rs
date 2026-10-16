pub(crate) mod object;

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
    Find(Find),
    MatchPair,
    ParagraphForward,
    ParagraphBack,
    Mark { name: char, exact: bool },
    Jump { to: (usize, usize), linewise: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Find {
    pub(crate) target: char,
    pub(crate) forward: bool,
    pub(crate) till: bool,
    pub(crate) repeat: bool,
}

impl Find {
    pub(crate) fn again(self) -> Self {
        Self {
            repeat: true,
            ..self
        }
    }

    pub(crate) fn reversed(self) -> Self {
        Self {
            forward: !self.forward,
            ..self
        }
    }
}

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Region {
    Chars(std::ops::Range<usize>),
    Lines(std::ops::RangeInclusive<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

#[derive(Debug, Clone, Copy, Default)]
struct Ends {
    stay: bool,
    empty_lines: bool,
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

    fn matching_pair(&self, at: usize) -> Option<usize> {
        let end = self.line_end(self.line_of(at));
        let (from, open, close, forward) = (at..end).find_map(|i| {
            let c = self.chars[i];
            PAIRS.iter().find_map(|&(open, close)| {
                (c == open || c == close).then_some((i, open, close, c == open))
            })
        })?;
        if forward {
            self.unmatched_close(from, open, close)
        } else {
            self.unmatched_open(from, open, close)
        }
    }

    fn unmatched_close(&self, from: usize, open: char, close: char) -> Option<usize> {
        let mut depth = 0usize;
        for (i, c) in self.chars.iter().copied().enumerate().skip(from + 1) {
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
        None
    }

    fn unmatched_open(&self, from: usize, open: char, close: char) -> Option<usize> {
        let mut depth = 0usize;
        for (i, c) in self.chars[..from].iter().copied().enumerate().rev() {
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
        None
    }

    fn incl(&self, at: usize) -> (usize, Step) {
        let (next, step) = self.inc(at);
        if step.left_the_line() && self.column(next) > 0 {
            self.inc(next)
        } else {
            (next, step)
        }
    }

    fn decl(&self, at: usize) -> (usize, Step) {
        let (previous, step) = self.dec(at);
        if step == Step::Crossed && self.column(previous) > 0 {
            self.dec(previous)
        } else {
            (previous, step)
        }
    }

    fn on_indent(&self, index: usize) -> bool {
        self.in_indent(index)
            && self
                .chars
                .get(index)
                .is_some_and(|c| c.is_whitespace() && *c != '\n')
    }

    fn in_indent(&self, index: usize) -> bool {
        let start = self.starts[self.line_of(index)];
        self.chars[start..index]
            .iter()
            .all(|c| c.is_whitespace() && *c != '\n')
    }

    fn region(&self, from: usize, to: usize, inclusive: bool) -> Region {
        let (start, mut end) = (from.min(to), from.max(to));
        if inclusive {
            if !self.is_line_end(end) {
                end += 1;
            }
            return Region::Chars(start..end);
        }
        let (first, last) = (self.line_of(start), self.line_of(end));
        if self.column(end) == 0 && last > first {
            if self.in_indent(start) {
                return Region::Lines(first..=last - 1);
            }
            end = self.line_end(last - 1);
        }
        Region::Chars(start..end)
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

    fn end_word(&self, mut at: usize, word: Word, count: usize, ends: Ends) -> usize {
        let mut stay = ends.stay;
        for _ in 0..count {
            let class = self.class(at, word);
            let (next, step) = self.inc(at);
            if step == Step::Stuck {
                return at;
            }
            at = next;
            if self.class(at, word) == class && class != Class::Blank {
                at = self.skip_forward(at, word, class);
            } else if !stay || class == Class::Blank {
                while self.class(at, word) == Class::Blank {
                    if ends.empty_lines && self.on_empty_line(at) {
                        break;
                    }
                    let (next, step) = self.inc(at);
                    if step == Step::Stuck {
                        return at;
                    }
                    at = next;
                }
                if self.class(at, word) == Class::Blank {
                    stay = false;
                    continue;
                }
                at = self.skip_forward(at, word, self.class(at, word));
            }
            at = self.dec(at).0;
            stay = false;
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

fn blank(row: &str) -> bool {
    row.chars().all(char::is_whitespace)
}

fn paragraph_edge(rows: &[String], line: usize, forward: bool, count: usize) -> Option<usize> {
    let mut at = line;
    for left in (0..count).rev() {
        let mut seen_text = false;
        let mut first = true;
        loop {
            let row = rows.get(at).map_or("", String::as_str);
            seen_text |= !blank(row);
            if !first && seen_text && blank(row) {
                break;
            }
            first = false;
            let next = if forward {
                Some(at + 1).filter(|next| *next < rows.len())
            } else {
                at.checked_sub(1)
            };
            match next {
                Some(next) => at = next,
                None if left > 0 => return None,
                None => break,
            }
        }
    }
    Some(at)
}

pub(crate) fn first_non_blank(row: &str) -> usize {
    row.chars().take_while(|c| c.is_whitespace()).count()
}

pub(crate) fn last_cell(row: &str) -> usize {
    row.chars().count().saturating_sub(1)
}

fn find_in_row(row: &str, column: usize, find: Find, count: usize) -> Option<usize> {
    let chars: Vec<char> = row.chars().collect();
    let skip_adjacent = find.till && find.repeat && count == 1;
    let nth = count.saturating_sub(1);
    let hit = if find.forward {
        (column + 1..chars.len())
            .filter(|i| chars[*i] == find.target && !(skip_adjacent && *i == column + 1))
            .nth(nth)?
    } else {
        (0..column.min(chars.len()))
            .rev()
            .filter(|i| chars[*i] == find.target && !(skip_adjacent && *i + 1 == column))
            .nth(nth)?
    };
    Some(match (find.till, find.forward) {
        (false, _) => hit,
        (true, true) => hit - 1,
        (true, false) => hit + 1,
    })
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
            text.end_word(from, word, count, Ends::default())
        }),
        Motion::WordEndBack(word) => word_motion(word, Text::back_end_word),
        Motion::Find(find) => Some((line, find_in_row(row(line), column, find, count)?)),
        Motion::MatchPair => {
            let text = Text::new(rows);
            let from = text.index_of(at)?;
            Some(text.position(text.matching_pair(from)?))
        }
        Motion::ParagraphForward | Motion::ParagraphBack => {
            let forward = motion == Motion::ParagraphForward;
            let line = paragraph_edge(rows, line, forward, count)?;
            let at_end = forward && line + 1 == rows.len();
            Some((line, if at_end { last_cell(row(line)) } else { 0 }))
        }
        Motion::Jump { to, linewise } => {
            let line = to.0.min(rows.len().saturating_sub(1));
            let column = if linewise {
                first_non_blank(row(line))
            } else {
                to.1
            };
            Some((line, column))
        }
        Motion::Mark { .. } => None,
        Motion::Up | Motion::Down | Motion::Top | Motion::Bottom => None,
    }
}

pub(crate) fn span(
    rows: &[String],
    at: (usize, usize),
    motion: Motion,
    count: Option<usize>,
    change: bool,
) -> Option<Region> {
    let text = Text::new(rows);
    let from = text.index_of(at)?;
    let (line, column) = text.position(from);
    let times = count.unwrap_or(1);
    let last = rows.len().saturating_sub(1);
    let lines = |to: usize| Some(Region::Lines(line.min(to)..=line.max(to)));
    let (to, inclusive) = match motion {
        Motion::Up if line == 0 => return None,
        Motion::Up => return lines(line.saturating_sub(times)),
        Motion::Down if line == last => return None,
        Motion::Down => return lines((line + times).min(last)),
        Motion::Top => return lines(count.map_or(0, |n| n.saturating_sub(1)).min(last)),
        Motion::Bottom => return lines(count.map_or(last, |n| n.saturating_sub(1)).min(last)),
        Motion::MatchPair if let Some(percent) = count => {
            if percent > 100 {
                return None;
            }
            return lines((percent * rows.len()).div_ceil(100).saturating_sub(1));
        }
        Motion::Left if column == 0 => return None,
        Motion::Left => (from - times.min(column), false),
        Motion::Right => (text.index_of((line, column + times))?, false),
        Motion::LineStart => (text.starts[line], false),
        Motion::FirstNonBlank => (text.index_of((line, first_non_blank(&rows[line])))?, false),
        Motion::LineEnd => {
            let end_line = (line + times - 1).min(last);
            let end = text.line_end(end_line);
            if end == text.starts[end_line] {
                (end, false)
            } else {
                (end - 1, true)
            }
        }
        Motion::WordForward(word) if change && text.class(from, word) != Class::Blank => (
            text.end_word(
                from,
                word,
                times,
                Ends {
                    stay: true,
                    empty_lines: false,
                },
            ),
            true,
        ),
        Motion::WordForward(word) => (text.forward_word(from, word, times, true), false),
        Motion::WordBack(word) => (text.back_word(from, word, times), false),
        Motion::WordEnd(word) => (text.end_word(from, word, times, Ends::default()), true),
        Motion::WordEndBack(word) => (text.back_end_word(from, word, times), true),
        Motion::Find(find) => {
            let column = find_in_row(&rows[line], column, find, times)?;
            (text.index_of((line, column))?, find.forward)
        }
        Motion::MatchPair => (text.matching_pair(from)?, true),
        Motion::Jump { to, linewise: true } => return lines(to.0.min(last)),
        Motion::Jump { to, .. } => (text.index_of((to.0.min(last), to.1))?, false),
        Motion::Mark { .. } => return None,
        Motion::ParagraphForward | Motion::ParagraphBack => {
            let forward = motion == Motion::ParagraphForward;
            let edge = paragraph_edge(rows, line, forward, times)?;
            let width = rows[edge].chars().count();
            if forward && edge == last && width > 0 {
                (text.index_of((edge, width - 1))?, true)
            } else {
                (text.index_of((edge, 0))?, false)
            }
        }
    };
    Some(text.region(from, to, inclusive))
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

    fn find(target: char, forward: bool, till: bool) -> Find {
        Find {
            target,
            forward,
            till,
            repeat: false,
        }
    }

    #[test]
    fn f_lands_on_the_next_match_in_the_row() {
        assert_eq!(find_in_row("a,b,c", 0, find(',', true, false), 1), Some(1));
        assert_eq!(find_in_row("a,b,c", 0, find(',', true, false), 2), Some(3));
        assert_eq!(find_in_row("a,b,c", 0, find(',', true, false), 3), None);
    }

    #[test]
    fn t_stops_one_short_of_the_match() {
        assert_eq!(find_in_row("a,b,c", 0, find('c', true, true), 1), Some(3));
        assert_eq!(find_in_row("a,b,c", 4, find('a', false, true), 1), Some(1));
    }

    #[test]
    fn capital_f_searches_back() {
        assert_eq!(find_in_row("a,b,c", 4, find(',', false, false), 1), Some(3));
        assert_eq!(find_in_row("a,b,c", 4, find(',', false, false), 2), Some(1));
    }

    #[test]
    fn repeating_t_skips_the_match_it_is_already_against() {
        let till = find(',', true, true);

        assert_eq!(find_in_row("a,b,c", 0, till, 1), Some(0));
        assert_eq!(find_in_row("a,b,c", 0, till.again(), 1), Some(2));
        assert_eq!(find_in_row("a,b,c", 4, till.reversed().again(), 1), Some(2));
        assert_eq!(find_in_row("a,b,c", 2, till.reversed().again(), 1), None);
    }

    #[test]
    fn a_find_never_leaves_the_row() {
        let rows = rows(&["ab", "xa"]);

        assert_eq!(
            destination(&rows, (0, 0), Motion::Find(find('x', true, false)), 1),
            None
        );
    }

    #[test]
    fn percent_jumps_between_a_bracket_and_its_partner() {
        let rows = rows(&["call(a, (b), c)"]);

        assert_eq!(go(&rows, (0, 4), Motion::MatchPair), (0, 14));
        assert_eq!(go(&rows, (0, 14), Motion::MatchPair), (0, 4));
        assert_eq!(go(&rows, (0, 8), Motion::MatchPair), (0, 10));
    }

    #[test]
    fn percent_looks_ahead_on_the_line_for_a_bracket() {
        let rows = rows(&["x = [1, 2]"]);

        assert_eq!(go(&rows, (0, 0), Motion::MatchPair), (0, 9));
    }

    #[test]
    fn percent_crosses_lines() {
        let rows = rows(&["fn main() {", "    body();", "}"]);

        assert_eq!(go(&rows, (0, 10), Motion::MatchPair), (2, 0));
        assert_eq!(go(&rows, (2, 0), Motion::MatchPair), (0, 10));
    }

    #[test]
    fn percent_without_a_bracket_ahead_stays_put() {
        let rows = rows(&["(a) b", "c)"]);

        assert_eq!(destination(&rows, (0, 4), Motion::MatchPair, 1), None);
    }

    fn chars(text: &[&str], at: (usize, usize), motion: Motion, count: usize) -> String {
        let rows = rows(text);
        let joined = rows.join("\n");
        match span(&rows, at, motion, (count > 1).then_some(count), false) {
            Some(Region::Chars(range)) => {
                joined.chars().skip(range.start).take(range.len()).collect()
            }
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn dw_stops_at_the_end_of_the_line() {
        assert_eq!(
            chars(&["one two"], (0, 4), Motion::WordForward(Word::Small), 1),
            "two"
        );
        assert_eq!(
            chars(
                &["one two", "three"],
                (0, 4),
                Motion::WordForward(Word::Small),
                1
            ),
            "two"
        );
        assert_eq!(
            chars(
                &["one two  ", "three"],
                (0, 4),
                Motion::WordForward(Word::Small),
                1
            ),
            "two  "
        );
    }

    #[test]
    fn a_counted_dw_crosses_lines() {
        assert_eq!(
            chars(
                &["one two", "three four"],
                (0, 4),
                Motion::WordForward(Word::Small),
                2
            ),
            "two\nthree "
        );
    }

    #[test]
    fn cw_on_a_word_changes_only_to_its_end() {
        let rows = rows(&["one two"]);

        assert_eq!(
            span(&rows, (0, 0), Motion::WordForward(Word::Small), None, true),
            Some(Region::Chars(0..3))
        );
        assert_eq!(
            span(&rows, (0, 2), Motion::WordForward(Word::Small), None, true),
            Some(Region::Chars(2..3))
        );
        assert_eq!(
            span(&rows, (0, 3), Motion::WordForward(Word::Small), None, true),
            Some(Region::Chars(3..4))
        );
    }

    #[test]
    fn inclusive_motions_take_the_character_they_land_on() {
        assert_eq!(
            chars(&["one two"], (0, 0), Motion::WordEnd(Word::Small), 1),
            "one"
        );
        assert_eq!(
            chars(&["a,b,c"], (0, 0), Motion::Find(find(',', true, false)), 2),
            "a,b,"
        );
        assert_eq!(
            chars(&["a,b,c"], (0, 0), Motion::Find(find('c', true, true)), 1),
            "a,b,"
        );
        assert_eq!(chars(&["f(a)"], (0, 1), Motion::MatchPair, 1), "(a)");
    }

    #[test]
    fn backward_motions_leave_the_cursor_character() {
        assert_eq!(
            chars(&["one two"], (0, 4), Motion::WordBack(Word::Small), 1),
            "one "
        );
        assert_eq!(
            chars(&["a,b,c"], (0, 4), Motion::Find(find(',', false, false)), 1),
            ","
        );
        assert_eq!(chars(&["one two"], (0, 3), Motion::LineStart, 1), "one");
    }

    #[test]
    fn dge_takes_the_cursor_character_too() {
        assert_eq!(
            chars(&["one two"], (0, 5), Motion::WordEndBack(Word::Small), 1),
            "e tw"
        );
    }

    #[test]
    fn dollar_takes_the_rest_of_the_line_but_not_the_break() {
        assert_eq!(chars(&["one two", "x"], (0, 4), Motion::LineEnd, 1), "two");
        assert_eq!(chars(&["one", "", "x"], (1, 0), Motion::LineEnd, 1), "");
    }

    #[test]
    fn l_reaches_past_the_last_character_for_an_operator() {
        assert_eq!(chars(&["ab"], (0, 1), Motion::Right, 1), "b");
        assert_eq!(chars(&["ab"], (0, 0), Motion::Right, 5), "ab");
        assert_eq!(chars(&[""], (0, 0), Motion::Right, 1), "");
    }

    #[test]
    fn h_at_the_start_of_a_line_is_no_region() {
        assert_eq!(
            span(&rows(&["ab"]), (0, 0), Motion::Left, None, false),
            None
        );
    }

    #[test]
    fn vertical_motions_take_whole_lines() {
        let rows = rows(&["a", "b", "c", "d"]);

        assert_eq!(
            span(&rows, (1, 0), Motion::Down, None, false),
            Some(Region::Lines(1..=2))
        );
        assert_eq!(
            span(&rows, (1, 0), Motion::Up, Some(5), false),
            Some(Region::Lines(0..=1))
        );
        assert_eq!(
            span(&rows, (1, 0), Motion::Bottom, None, false),
            Some(Region::Lines(1..=3))
        );
        assert_eq!(
            span(&rows, (2, 0), Motion::Top, None, false),
            Some(Region::Lines(0..=2))
        );
        assert_eq!(span(&rows, (3, 0), Motion::Down, None, false), None);
    }

    #[test]
    fn an_exclusive_end_at_a_line_start_backs_up_or_goes_linewise() {
        let rows = rows(&["one", "two"]);

        assert_eq!(
            span(&rows, (1, 0), Motion::WordBack(Word::Small), None, false),
            Some(Region::Lines(0..=0))
        );

        let rows2 = super::tests::rows(&["x one", "two"]);
        assert_eq!(
            span(&rows2, (1, 0), Motion::WordBack(Word::Small), None, false),
            Some(Region::Chars(2..5))
        );
    }

    #[test]
    fn dw_on_an_empty_line_takes_the_line() {
        let rows = rows(&["one", "", "two"]);

        assert_eq!(
            span(&rows, (1, 0), Motion::WordForward(Word::Small), None, false),
            Some(Region::Lines(1..=1))
        );
    }

    #[test]
    fn braces_jump_to_the_blank_line_around_a_paragraph() {
        let rows = rows(&["one", "two", "", "three", "four", "", "five"]);

        assert_eq!(go(&rows, (0, 1), Motion::ParagraphForward), (2, 0));
        assert_eq!(go(&rows, (2, 0), Motion::ParagraphForward), (5, 0));
        assert_eq!(go(&rows, (4, 2), Motion::ParagraphBack), (2, 0));
        assert_eq!(go(&rows, (2, 0), Motion::ParagraphBack), (0, 0));
    }

    #[test]
    fn a_closing_brace_past_the_last_paragraph_lands_on_the_last_character() {
        let rows = rows(&["one", "", "two words"]);

        assert_eq!(go(&rows, (2, 0), Motion::ParagraphForward), (2, 8));
        assert_eq!(
            destination(&rows, (0, 0), Motion::ParagraphForward, 3),
            None
        );
        assert_eq!(
            destination(&rows, (0, 0), Motion::ParagraphForward, 2),
            Some((2, 8))
        );
    }

    #[test]
    fn a_line_of_spaces_also_ends_a_paragraph() {
        let rows = rows(&[" one", "   ", " two"]);

        assert_eq!(go(&rows, (0, 1), Motion::ParagraphForward), (1, 0));
    }

    #[test]
    fn d_closing_brace_from_a_line_start_takes_the_paragraph_lines() {
        let rows = rows(&["one", "two", "", "three"]);

        assert_eq!(
            span(&rows, (0, 0), Motion::ParagraphForward, None, false),
            Some(Region::Lines(0..=1))
        );
        assert_eq!(
            chars(
                &["one", "two", "", "three"],
                (0, 1),
                Motion::ParagraphForward,
                1
            ),
            "ne\ntwo"
        );
        assert_eq!(
            chars(
                &["one", "", "two words"],
                (2, 4),
                Motion::ParagraphForward,
                1
            ),
            "words"
        );
    }

    #[test]
    fn a_jump_to_a_mark_lands_exactly_or_on_the_first_non_blank() {
        let rows = rows(&["one", "  two three"]);
        let exact = Motion::Jump {
            to: (1, 6),
            linewise: false,
        };
        let line = Motion::Jump {
            to: (1, 6),
            linewise: true,
        };

        assert_eq!(go(&rows, (0, 0), exact), (1, 6));
        assert_eq!(go(&rows, (0, 0), line), (1, 2));
        assert_eq!(
            span(&rows, (0, 1), line, None, false),
            Some(Region::Lines(0..=1))
        );
        assert_eq!(
            chars(&["one", "  two three"], (0, 1), exact, 1),
            "ne\n  two "
        );
    }

    #[test]
    fn a_counted_dollar_ends_further_down() {
        let rows = rows(&["one", "three"]);

        assert_eq!(destination(&rows, (0, 0), Motion::LineEnd, 2), Some((1, 4)));
    }
}
