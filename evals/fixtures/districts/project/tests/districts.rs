use chalk_districts::{resolve_district, School};

#[test]
fn assigned_school_resolves_to_its_district() {
    let school = School {
        name: "Northside Elementary".into(),
        district: Some("District 12".into()),
    };
    assert_eq!(resolve_district(&school), "District 12");
}

#[test]
fn unassigned_school_resolves_to_unassigned_not_a_panic() {
    let school = School {
        name: "Brand New Academy".into(),
        district: None,
    };
    assert_eq!(resolve_district(&school), "unassigned");
}
