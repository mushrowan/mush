use super::{Class, Ends, Region, Step, Text, Word, blank};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Word(Word),
    Quote(char),
    Block(char, char),
    Paragraph,
    Sentence,
    Tag,
}

impl Kind {
    pub(crate) fn for_char(c: char) -> Option<Self> {
        Some(match c {
            'w' => Self::Word(Word::Small),
            'W' => Self::Word(Word::Big),
            '"' | '\'' | '`' => Self::Quote(c),
            '(' | ')' | 'b' => Self::Block('(', ')'),
            '[' | ']' => Self::Block('[', ']'),
            '{' | '}' | 'B' => Self::Block('{', '}'),
            '<' | '>' => Self::Block('<', '>'),
            'p' => Self::Paragraph,
            's' => Self::Sentence,
            't' => Self::Tag,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Object {
    pub(crate) kind: Kind,
    pub(crate) around: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: (usize, usize),
    pub(crate) head: (usize, usize),
    pub(crate) linewise: bool,
}

pub(crate) fn span(
    rows: &[String],
    at: (usize, usize),
    object: Object,
    count: Option<usize>,
) -> Option<Region> {
    let text = Text::new(rows);
    let at = text.index_of(at)?;
    let count = count.unwrap_or(1);
    match object.kind {
        Kind::Word(word) => Some(words(&text, at, word, object.around, count)),
        Kind::Quote(quote) => quoted(&text, at, quote, object.around, count),
        Kind::Block(open, close) => block(&text, at, (open, close), object.around, count),
        Kind::Paragraph => paragraph(rows, text.line_of(at), object.around, count),
        Kind::Sentence => sentence(&text, rows, at, object.around, count),
        Kind::Tag => tag(&text.chars, at, object.around, count),
    }
}

pub(crate) fn selection(
    rows: &[String],
    at: (usize, usize),
    object: Object,
    count: Option<usize>,
) -> Option<Selection> {
    let text = Text::new(rows);
    match span(rows, at, object, count)? {
        Region::Chars(range) if range.is_empty() => None,
        Region::Chars(range) => Some(Selection {
            anchor: text.position(range.start),
            head: text.position(range.end - 1),
            linewise: false,
        }),
        Region::Lines(lines) => Some(Selection {
            anchor: (*lines.start(), 0),
            head: (*lines.end(), 0),
            linewise: true,
        }),
    }
}

fn words(text: &Text, at: usize, word: Word, around: bool, count: usize) -> Region {
    let line_start = text.starts[text.line_of(at)];
    let class = text.class(at, word);
    let mut start = at;
    while start > line_start && text.class(start - 1, word) == class {
        start -= 1;
    }
    let ends = Ends {
        stay: true,
        empty_lines: true,
    };
    let mut white_after = false;
    let mut end = if (class == Class::Blank) == around {
        text.end_word(start, word, 1, ends)
    } else {
        white_after = around;
        let next = text.forward_word(start, word, 1, true);
        if text.column(next) == 0 {
            text.decl(next).0
        } else {
            next - 1
        }
    };
    let mut inclusive = true;
    for _ in 1..count {
        inclusive = true;
        let (next, step) = text.incl(end);
        if step == Step::Stuck {
            break;
        }
        end = next;
        if around == (text.class(end, word) == Class::Blank) {
            end = text.end_word(end, word, 1, ends);
        } else {
            end = text.forward_word(end, word, 1, true);
            if text.column(end) == 0 {
                inclusive = false;
            } else {
                end -= 1;
            }
        }
    }
    if white_after
        && (text.class(end, word) != Class::Blank || (text.column(end) == 0 && !inclusive))
        && start > line_start
    {
        let mut before = start - 1;
        let class = text.class(before, word);
        while before > line_start && text.class(before - 1, word) == class {
            before -= 1;
        }
        if class == Class::Blank && before > line_start {
            start = before;
        }
    }
    text.region(start, end, inclusive)
}

fn paragraph(rows: &[String], line: usize, around: bool, count: usize) -> Option<Region> {
    let white = |line: usize| blank(&rows[line]);
    let last = rows.len() - 1;
    let white_in_front = white(line);
    let mut start = line;
    while start > 0 && white(start - 1) == white_in_front {
        start -= 1;
    }
    let mut next = start;
    while next <= last && white(next) {
        next += 1;
    }
    let mut left = count;
    if !around && white_in_front {
        left -= 1;
    }
    while left > 0 {
        left -= 1;
        if next > last {
            return None;
        }
        let into_white = !around && white(next);
        if around || !into_white {
            next += 1;
            while next <= last && !white(next) {
                next += 1;
            }
        }
        if left == 0 && white_in_front && around {
            break;
        }
        if around || into_white {
            while next <= last && white(next) {
                next += 1;
            }
        }
    }
    let end = next - 1;
    if around && !white_in_front && !white(end) {
        while start > 0 && white(start - 1) {
            start -= 1;
        }
    }
    Some(Region::Lines(start..=end))
}

fn sentence(text: &Text, rows: &[String], at: usize, around: bool, count: usize) -> Option<Region> {
    let line = text.line_of(at);
    if blank(&rows[line]) {
        return paragraph(rows, line, false, 1);
    }
    let mut first = line;
    while first > 0 && !blank(&rows[first - 1]) {
        first -= 1;
    }
    let mut last = line;
    while last + 1 < rows.len() && !blank(&rows[last + 1]) {
        last += 1;
    }
    let pieces = sentences(&text.chars, text.starts[first]..text.line_end(last));
    let here = pieces.iter().position(|(range, _)| range.contains(&at))?;
    let (start, end) = if around {
        let white_first = pieces[here].1;
        let mut end = here + 2 * count - 1;
        if end == pieces.len() && !white_first {
            end -= 1;
        }
        if end >= pieces.len() {
            return None;
        }
        let lead_in = !white_first && !pieces[end].1 && here >= 2;
        (if lead_in { here - 1 } else { here }, end)
    } else {
        let end = here + count - 1;
        if end >= pieces.len() {
            return None;
        }
        (here, end)
    };
    Some(Region::Chars(pieces[start].0.start..pieces[end].0.end))
}

fn sentences(chars: &[char], span: std::ops::Range<usize>) -> Vec<(std::ops::Range<usize>, bool)> {
    let mut pieces = Vec::new();
    let mut i = span.start;
    while i < span.end {
        let from = i;
        if chars[i].is_whitespace() {
            while i < span.end && chars[i].is_whitespace() {
                i += 1;
            }
            pieces.push((from..i, true));
            continue;
        }
        while i < span.end {
            let c = chars[i];
            i += 1;
            if matches!(c, '.' | '!' | '?') {
                while i < span.end && matches!(chars[i], ')' | ']' | '"' | '\'') {
                    i += 1;
                }
                if i == span.end || chars[i].is_whitespace() {
                    break;
                }
            }
        }
        pieces.push((from..i, false));
    }
    pieces
}

struct Tag {
    name: String,
    span: std::ops::Range<usize>,
    closing: bool,
}

fn tag(chars: &[char], at: usize, around: bool, count: usize) -> Option<Region> {
    let mut open: Vec<Tag> = Vec::new();
    let mut elements = Vec::new();
    for tag in tags(chars) {
        if !tag.closing {
            open.push(tag);
            continue;
        }
        if let Some(depth) = open
            .iter()
            .rposition(|o| o.name.eq_ignore_ascii_case(&tag.name))
        {
            let opening = open.swap_remove(depth);
            open.truncate(depth);
            elements.push((opening.span, tag.span));
        }
    }
    elements.retain(|(opening, closing)| opening.start <= at && at < closing.end);
    elements.sort_by_key(|(opening, closing)| closing.end - opening.start);
    let (opening, closing) = elements.get(count.checked_sub(1)?)?;
    Some(Region::Chars(if around {
        opening.start..closing.end
    } else {
        opening.end..closing.start
    }))
}

fn tags(chars: &[char]) -> Vec<Tag> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            i += 1;
            continue;
        }
        let start = i;
        let closing = chars.get(i + 1) == Some(&'/');
        let name_start = i + 1 + usize::from(closing);
        let mut j = name_start;
        while chars
            .get(j)
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        {
            j += 1;
        }
        if j == name_start || !chars[name_start].is_alphabetic() {
            i += 1;
            continue;
        }
        let name: String = chars[name_start..j].iter().collect();
        let mut quote = None;
        while let Some(&c) = chars.get(j) {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '>' || c == '<' => break,
                None => {}
            }
            j += 1;
        }
        if chars.get(j) != Some(&'>') {
            i += 1;
            continue;
        }
        let self_closing = chars[j - 1] == '/';
        if !self_closing {
            found.push(Tag {
                name,
                span: start..j + 1,
                closing,
            });
        }
        i = j + 1;
    }
    found
}

fn quoted(text: &Text, at: usize, quote: char, around: bool, count: usize) -> Option<Region> {
    let line = text.line_of(at);
    let start = text.starts[line];
    let row = &text.chars[start..text.line_end(line)];
    let column = at - start;
    let escaped = |i: usize| row[..i].iter().rev().take_while(|c| **c == '\\').count() % 2 == 1;
    let next_quote = |from: usize| (from..row.len()).find(|&i| row[i] == quote && !escaped(i));
    let (open, close) = if row.get(column) == Some(&quote) {
        let mut from = 0;
        loop {
            let open = next_quote(from).filter(|open| *open <= column)?;
            let close = next_quote(open + 1)?;
            if column <= close {
                break (open, close);
            }
            from = close + 1;
        }
    } else {
        let open = (0..column)
            .rev()
            .find(|&i| row[i] == quote && !escaped(i))
            .or_else(|| next_quote(column))?;
        (open, next_quote(open + 1)?)
    };
    let white = |i: usize| row.get(i).is_some_and(|c| *c == ' ' || *c == '\t');
    let (first, last) = if around {
        let (mut first, mut last) = (open, close);
        if white(close + 1) {
            while white(last + 1) {
                last += 1;
            }
        } else {
            while first > 0 && white(first - 1) {
                first -= 1;
            }
        }
        (first, last + 1)
    } else if count > 1 {
        (open, close + 1)
    } else {
        (open + 1, close)
    };
    Some(Region::Chars(start + first..start + last))
}

fn block(
    text: &Text,
    at: usize,
    (open, close): (char, char),
    around: bool,
    count: usize,
) -> Option<Region> {
    let mut at = at;
    if open == '{' {
        while text.on_indent(at) {
            let (next, step) = text.inc(at);
            at = next;
            if step != Step::Within {
                break;
            }
        }
    }
    if text.chars.get(at) == Some(&open) {
        at += 1;
    }
    let mut opening = at;
    for _ in 0..count {
        opening = text.unmatched_open(opening, open, close)?;
    }
    let closing = text.unmatched_close(opening, open, close)?;
    if around {
        return Some(Region::Chars(opening..closing + 1));
    }
    let start = text.incl(opening).0;
    let mut starts_its_line = text.column(closing) == 0;
    let mut end = text.decl(closing).0;
    while text.on_indent(end) {
        starts_its_line = true;
        let (previous, step) = text.decl(end);
        end = previous;
        if step != Step::Within {
            break;
        }
    }
    if starts_its_line {
        return Some(text.region(start, text.incl(end).0, false));
    }
    if start <= end {
        return Some(text.region(start, end, true));
    }
    Some(Region::Chars(start..start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(rows: &[&str]) -> Vec<String> {
        rows.iter().map(std::string::ToString::to_string).collect()
    }

    fn take(text: &[&str], at: (usize, usize), c: char, around: bool, count: usize) -> String {
        let rows = rows(text);
        let joined = rows.join("\n");
        let object = Object {
            kind: Kind::for_char(c).unwrap_or(Kind::Word(Word::Small)),
            around,
        };
        match span(&rows, at, object, Some(count)) {
            Some(Region::Chars(range)) => {
                joined.chars().skip(range.start).take(range.len()).collect()
            }
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn iw_is_the_word_under_the_caret() {
        assert_eq!(take(&["one two three"], (0, 5), 'w', false, 1), "two");
        assert_eq!(take(&["one two three"], (0, 4), 'w', false, 1), "two");
        assert_eq!(take(&["foo.bar"], (0, 1), 'w', false, 1), "foo");
        assert_eq!(take(&["foo.bar"], (0, 1), 'W', false, 1), "foo.bar");
    }

    #[test]
    fn iw_on_blanks_is_the_blanks() {
        assert_eq!(take(&["one   two"], (0, 4), 'w', false, 1), "   ");
    }

    #[test]
    fn aw_takes_the_space_after_or_else_before() {
        assert_eq!(take(&["one two three"], (0, 5), 'w', true, 1), "two ");
        assert_eq!(take(&["one two"], (0, 5), 'w', true, 1), " two");
        assert_eq!(take(&["  one"], (0, 3), 'w', true, 1), "one");
    }

    #[test]
    fn a_count_takes_more_words() {
        assert_eq!(take(&["one two three"], (0, 0), 'w', false, 3), "one two");
        assert_eq!(
            take(&["one two three four"], (0, 0), 'w', true, 2),
            "one two "
        );
    }

    #[test]
    fn iw_on_an_empty_line_is_nothing() {
        assert_eq!(take(&["a", "", "b"], (1, 0), 'w', false, 1), "");
    }

    #[test]
    fn inner_quotes_skip_the_quotes() {
        assert_eq!(
            take(&[r#"say "hi there" now"#], (0, 7), '"', false, 1),
            "hi there"
        );
        assert_eq!(
            take(&[r#"say "hi there" now"#], (0, 4), '"', false, 1),
            "hi there"
        );
        assert_eq!(
            take(&[r#"say "hi there" now"#], (0, 13), '"', false, 1),
            "hi there"
        );
    }

    #[test]
    fn around_quotes_take_the_quotes_and_a_space() {
        assert_eq!(take(&[r#"say "hi" now"#], (0, 5), '"', true, 1), r#""hi" "#);
        assert_eq!(take(&[r#"say "hi""#], (0, 5), '"', true, 1), r#" "hi""#);
    }

    #[test]
    fn quotes_ahead_on_the_line_count_when_the_caret_is_outside() {
        assert_eq!(take(&["x = 'a', 'b'"], (0, 0), '\'', false, 1), "a");
    }

    #[test]
    fn an_escaped_quote_stays_inside() {
        assert_eq!(take(&[r#""a\"b" c"#], (0, 1), '"', false, 1), r#"a\"b"#);
    }

    #[test]
    fn a_count_on_inner_quotes_keeps_the_quotes() {
        assert_eq!(take(&[r#"say "hi" now"#], (0, 5), '"', false, 2), r#""hi""#);
    }

    #[test]
    fn missing_quotes_are_no_object() {
        assert_eq!(take(&["no quotes"], (0, 2), '"', false, 1), "None");
    }

    #[test]
    fn inner_brackets_hold_what_is_between() {
        assert_eq!(
            take(&["call(a, (b), c)"], (0, 6), '(', false, 1),
            "a, (b), c"
        );
        assert_eq!(take(&["call(a, (b), c)"], (0, 9), 'b', false, 1), "b");
        assert_eq!(
            take(&["call(a, (b), c)"], (0, 9), ')', false, 2),
            "a, (b), c"
        );
        assert_eq!(take(&["x[1][2]"], (0, 5), ']', false, 1), "2");
        assert_eq!(take(&["<a href>"], (0, 3), '<', true, 1), "<a href>");
    }

    #[test]
    fn a_bracket_under_the_caret_is_its_own_block() {
        assert_eq!(take(&["f(a(b))"], (0, 3), '(', false, 1), "b");
        assert_eq!(take(&["f(a(b))"], (0, 5), '(', false, 1), "b");
        assert_eq!(take(&["f(a(b))"], (0, 1), '(', true, 1), "(a(b))");
    }

    #[test]
    fn empty_brackets_hold_nothing() {
        assert_eq!(take(&["f()"], (0, 1), '(', false, 1), "");
    }

    #[test]
    fn inner_braces_over_lines_take_the_lines_between() {
        let rows = rows(&["if x {", "    a();", "    b();", "}"]);
        let object = Object {
            kind: Kind::Block('{', '}'),
            around: false,
        };

        assert_eq!(
            span(&rows, (1, 5), object, None),
            Some(Region::Lines(1..=2))
        );
        assert_eq!(
            span(&rows, (0, 5), object, None),
            Some(Region::Lines(1..=2))
        );
        assert_eq!(
            span(&rows, (3, 0), object, None),
            Some(Region::Lines(1..=2))
        );
        assert_eq!(span(&rows, (0, 0), object, None), None);
    }

    fn lines(text: &[&str], line: usize, around: bool, count: usize) -> Option<Region> {
        let object = Object {
            kind: Kind::Paragraph,
            around,
        };
        span(&rows(text), (line, 0), object, Some(count))
    }

    #[test]
    fn ip_is_the_paragraph_and_ap_takes_the_blank_lines_after() {
        let text = ["one", "two", "", "", "three", "", "four"];

        assert_eq!(lines(&text, 1, false, 1), Some(Region::Lines(0..=1)));
        assert_eq!(lines(&text, 1, true, 1), Some(Region::Lines(0..=3)));
        assert_eq!(lines(&text, 4, true, 1), Some(Region::Lines(4..=5)));
    }

    #[test]
    fn ap_on_the_last_paragraph_takes_the_blank_lines_before() {
        let text = ["one", "", "two"];

        assert_eq!(lines(&text, 2, true, 1), Some(Region::Lines(1..=2)));
    }

    #[test]
    fn ip_on_blank_lines_is_the_blank_lines() {
        let text = ["one", "", "  ", "two"];

        assert_eq!(lines(&text, 1, false, 1), Some(Region::Lines(1..=2)));
        assert_eq!(lines(&text, 2, true, 1), Some(Region::Lines(1..=3)));
    }

    #[test]
    fn a_count_on_ip_counts_blank_runs_too() {
        let text = ["one", "", "two", "", "three"];

        assert_eq!(lines(&text, 0, false, 3), Some(Region::Lines(0..=2)));
        assert_eq!(lines(&text, 0, true, 2), Some(Region::Lines(0..=3)));
        assert_eq!(lines(&text, 0, true, 3), Some(Region::Lines(0..=4)));
        assert_eq!(lines(&text, 0, true, 4), None);
    }

    #[test]
    fn is_is_the_sentence_and_as_takes_the_space_after() {
        let text = ["Hello there. How are you? Fine!"];

        assert_eq!(take(&text, (0, 14), 's', false, 1), "How are you?");
        assert_eq!(take(&text, (0, 14), 's', true, 1), "How are you? ");
        assert_eq!(take(&text, (0, 0), 's', false, 1), "Hello there.");
    }

    #[test]
    fn as_on_the_last_sentence_takes_the_space_before() {
        assert_eq!(take(&["One. Two."], (0, 6), 's', true, 1), " Two.");
    }

    #[test]
    fn a_sentence_can_cross_lines_inside_a_paragraph() {
        let text = ["One. Two", "three. Four."];

        assert_eq!(take(&text, (1, 1), 's', false, 1), "Two\nthree.");
    }

    #[test]
    fn closing_quotes_and_brackets_stay_with_their_sentence() {
        let text = [r#"He said "Hi." Then (he left.) Bye"#];

        assert_eq!(take(&text, (0, 0), 's', false, 1), r#"He said "Hi.""#);
        assert_eq!(take(&text, (0, 16), 's', false, 1), "Then (he left.)");
    }

    #[test]
    fn a_dot_inside_a_word_does_not_end_a_sentence() {
        assert_eq!(
            take(&["See e.g.this one. Next"], (0, 0), 's', false, 1),
            "See e.g.this one."
        );
    }

    #[test]
    fn sentence_objects_on_the_gap_and_with_counts() {
        let text = ["One.  Two. Three."];

        assert_eq!(take(&text, (0, 4), 's', false, 1), "  ");
        assert_eq!(take(&text, (0, 4), 's', true, 1), "  Two.");
        assert_eq!(take(&text, (0, 0), 's', false, 2), "One.  ");
        assert_eq!(take(&text, (0, 0), 's', true, 2), "One.  Two. ");
        assert_eq!(take(&text, (0, 4), 's', true, 2), "  Two. Three.");
        assert_eq!(take(&text, (0, 6), 's', true, 2), "  Two. Three.");
        assert_eq!(take(&text, (0, 6), 's', true, 3), "None");
    }

    #[test]
    fn it_is_inside_the_tag_and_at_takes_the_tags() {
        let text = ["<a href=\"x>y\"><b>bold</b> rest</a>"];

        assert_eq!(take(&text, (0, 19), 't', false, 1), "bold");
        assert_eq!(take(&text, (0, 19), 't', true, 1), "<b>bold</b>");
        assert_eq!(take(&text, (0, 19), 't', false, 2), "<b>bold</b> rest");
        assert_eq!(take(&text, (0, 1), 't', false, 1), "<b>bold</b> rest");
    }

    #[test]
    fn a_closing_tag_under_the_caret_belongs_to_its_element() {
        assert_eq!(take(&["<p>hi</p>"], (0, 6), 't', false, 1), "hi");
    }

    #[test]
    fn self_closing_and_unclosed_tags_are_skipped() {
        let text = ["<div>a<br/>b<img src=x>c</div>"];

        assert_eq!(take(&text, (0, 12), 't', false, 1), "a<br/>b<img src=x>c");
    }

    #[test]
    fn tags_reach_across_lines_and_ignore_case() {
        let text = ["<DIV>", "  text", "</div>"];

        assert_eq!(take(&text, (1, 3), 't', false, 1), "\n  text\n");
    }

    #[test]
    fn empty_and_missing_tags() {
        assert_eq!(take(&["<i></i>"], (0, 1), 't', false, 1), "");
        assert_eq!(take(&["a < b > c"], (0, 0), 't', false, 1), "None");
    }

    #[test]
    fn a_selection_covers_the_object() {
        let rows = rows(&["one two three"]);
        let object = Object {
            kind: Kind::Word(Word::Small),
            around: false,
        };

        assert_eq!(
            selection(&rows, (0, 5), object, None),
            Some(Selection {
                anchor: (0, 4),
                head: (0, 6),
                linewise: false,
            })
        );
    }
}
