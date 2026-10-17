use super::*;

const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
const FLAG: &str = "\u{1f1ef}\u{1f1f5}";
const ACUTE_E: &str = "e\u{301}";
const COMBINED_GA: &str = "\u{304b}\u{3099}";

fn typed(text: &str) -> InputBuffer {
    let mut input = InputBuffer::new();
    input.insert_str(text);
    input
}

#[test]
fn backspace_takes_off_one_whole_grapheme_at_a_time() {
    let mut input = typed(&format!("a{FAMILY}{FLAG}{ACUTE_E}{COMBINED_GA}中"));
    let mut seen = Vec::new();
    while !input.text.is_empty() {
        input.backspace();
        seen.push(input.text.clone());
    }

    assert_eq!(
        seen,
        [
            format!("a{FAMILY}{FLAG}{ACUTE_E}{COMBINED_GA}"),
            format!("a{FAMILY}{FLAG}{ACUTE_E}"),
            format!("a{FAMILY}{FLAG}"),
            format!("a{FAMILY}"),
            "a".to_string(),
            String::new(),
        ]
    );
}

#[test]
fn arrows_and_delete_step_over_joined_emoji_and_combining_marks() {
    let mut input = typed(&format!("{FAMILY}{ACUTE_E}x"));
    input.cursor_home();

    input.cursor_right();
    assert_eq!(input.cursor, FAMILY.len());
    input.cursor_right();
    assert_eq!(input.cursor, FAMILY.len() + ACUTE_E.len());
    input.cursor_left();
    assert_eq!(input.cursor, FAMILY.len());

    input.delete();
    assert_eq!(input.text, format!("{FAMILY}x"));
}

#[test]
fn up_and_down_keep_the_screen_column_across_wide_text() {
    let mut input = typed("中文abc\nabcdef");
    input.cursor = "中文".len();

    assert!(input.cursor_down());
    assert_eq!(&input.text[input.cursor..], "ef");

    assert!(input.cursor_up());
    assert_eq!(&input.text[input.cursor..], "abc\nabcdef");

    let mut input = typed("abc\n中文");
    input.cursor = 1;
    assert!(input.cursor_down());
    assert_eq!(
        &input.text[input.cursor..],
        "中文",
        "lands on the wide char"
    );
}

#[test]
fn word_motions_split_cjk_text_by_script() {
    let mut input = typed("日本語のテキスト");

    input.delete_word_backward();
    assert_eq!(input.text, "日本語の");
    input.delete_word_backward();
    assert_eq!(input.text, "日本語");

    let mut input = typed("한국어 텍스트");
    input.delete_word_backward();
    assert_eq!(input.text, "한국어 ");

    let mut input = typed("日本語のテキスト");
    input.cursor_home();
    input.cursor_word_right();
    assert_eq!(&input.text[input.cursor..], "のテキスト");
    input.cursor_word_right();
    assert_eq!(&input.text[input.cursor..], "テキスト");
}

#[test]
fn a_selection_ends_after_the_whole_grapheme_under_the_cursor() {
    let mut input = typed(&format!("a{FAMILY}b"));
    input.anchor = Some(0);
    input.cursor = 1;

    assert_eq!(input.selected_bytes(), Some(0..1 + FAMILY.len()));
}
