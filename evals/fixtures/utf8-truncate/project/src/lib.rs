//! Teacher comment previews on the gradebook list.

pub const PREVIEW_LIMIT: usize = 12;

/// Shorten a teacher comment for the list view, appending an ellipsis when
/// anything was cut.
pub fn preview(comment: &str) -> String {
    if comment.len() <= PREVIEW_LIMIT {
        return comment.to_string();
    }
    // BUG: PREVIEW_LIMIT is a BYTE index. Any comment whose 12th byte lands
    // inside a multi-byte character panics — accented names and any
    // non-Latin script crash the whole gradebook list.
    format!("{}…", &comment[..PREVIEW_LIMIT])
}
