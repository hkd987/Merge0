use chalk_sync_report::{sync_roster, SyncOutcome};

#[test]
fn a_clean_roster_reports_every_row_imported() {
    let outcome = sync_roster(&["1001,4", "1002,5", "1003,3"]);
    assert_eq!(outcome, SyncOutcome::Ok { imported: 3 });
}

#[test]
fn a_malformed_row_fails_the_sync_instead_of_being_swallowed() {
    let outcome = sync_roster(&["1001,4", "not-a-row", "1003,3"]);
    match outcome {
        SyncOutcome::Failed { row, reason } => {
            assert_eq!(row, 1, "the failing row index is reported");
            assert!(!reason.is_empty(), "the reason explains what was wrong");
        }
        other => panic!("expected a reported failure, got {other:?}"),
    }
}
