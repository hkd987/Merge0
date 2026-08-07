//! Gradebook CSV import.

/// Import roster rows in batches. Rows beyond the batch cap are imported
/// in later batches by the caller; this function must return every row it
/// was given, in order.
pub fn import_rows(rows: &[&str]) -> Vec<String> {
    let mut imported = Vec::new();
    // BUG: the inclusive bound indexes one past the end for any non-empty
    // roster, so large-class imports die on the last row.
    for index in 0..=rows.len() {
        imported.push(rows[index].to_string());
    }
    imported
}
