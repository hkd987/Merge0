use chalk_import::import_rows;

#[test]
fn imports_every_row_in_order() {
    let rows: Vec<String> = (1..=230).map(|i| format!("student-{i}")).collect();
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let imported = import_rows(&refs);
    assert_eq!(imported.len(), 230);
    assert_eq!(imported[0], "student-1");
    assert_eq!(imported[229], "student-230");
}

#[test]
fn empty_roster_imports_nothing() {
    assert!(import_rows(&[]).is_empty());
}
