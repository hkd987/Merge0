//! Nightly roster sync reporting.

#[derive(Debug, PartialEq)]
pub enum SyncOutcome {
    Ok { imported: usize },
    Failed { row: usize, reason: String },
}

/// Parse one roster row: `"<student id>,<grade>"`.
pub fn parse_row(row: &str) -> Result<(u64, u8), String> {
    let (id, grade) = row.split_once(',').ok_or("row is not id,grade")?;
    let id: u64 = id.trim().parse().map_err(|_| format!("bad id {id:?}"))?;
    let grade: u8 = grade.trim().parse().map_err(|_| format!("bad grade {grade:?}"))?;
    Ok((id, grade))
}

/// Import a roster and report what happened.
pub fn sync_roster(rows: &[&str]) -> SyncOutcome {
    let mut imported = 0;
    for row in rows.iter() {
        match parse_row(row) {
            Ok(_) => imported += 1,
            // BUG: a malformed row is swallowed here, so a partial import
            // reports success and nobody learns the roster is incomplete.
            Err(_) => {}
        }
    }
    SyncOutcome::Ok { imported }
}
