//! Grade parsing. Invalid grades are a data-integrity error by policy:
//! a K-12 gradebook must never silently coerce a bad grade to a number.

pub fn parse_grade(raw: &str) -> Result<u8, String> {
    match raw.trim().parse::<u8>() {
        Ok(grade) if grade <= 100 => Ok(grade),
        _ => Err(format!("invalid grade {raw:?}")),
    }
}
