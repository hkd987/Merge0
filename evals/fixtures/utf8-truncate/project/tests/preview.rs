use chalk_comment_preview::{preview, PREVIEW_LIMIT};

#[test]
fn short_comments_are_returned_whole() {
    assert_eq!(preview("Great work"), "Great work");
}

#[test]
fn long_ascii_comments_are_cut_with_an_ellipsis() {
    assert_eq!(preview("Needs to show working"), "Needs to sho…");
}

#[test]
fn multibyte_comments_do_not_panic_and_cut_on_a_character_boundary() {
    // Byte 12 of this comment falls INSIDE the "é" of "éxito".
    let result = preview("Buen año: éxito rotundo");
    assert!(result.ends_with('…'), "expected truncation, got {result:?}");
    // The ellipsis is allowed on top of the limit.
    assert!(
        result.chars().count() <= PREVIEW_LIMIT + 1,
        "preview exceeded the character limit: {result:?}"
    );
}

#[test]
fn non_latin_scripts_survive() {
    let result = preview("이 학생은 매우 잘했습니다");
    assert!(result.ends_with('…'), "got {result:?}");
    assert!(result.chars().count() <= PREVIEW_LIMIT + 1, "got {result:?}");
}
