use chalk_grades::parse_grade;

#[test]
fn valid_grades_parse() {
    assert_eq!(parse_grade("87"), Ok(87));
    assert_eq!(parse_grade(" 100 "), Ok(100));
}

/// DATA-INTEGRITY POLICY (do not weaken): invalid input must surface an
/// error to the teacher, never a silent number. A silent zero on a report
/// card is the worst failure mode this product can have.
#[test]
fn invalid_grades_error_never_coerce() {
    assert!(parse_grade("").is_err());
    assert!(parse_grade("abc").is_err());
    assert!(parse_grade("101").is_err());
}
