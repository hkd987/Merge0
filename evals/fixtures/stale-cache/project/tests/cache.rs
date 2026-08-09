use chalk_roster_cache::RosterCache;

#[test]
fn a_first_read_reports_the_recorded_count() {
    let mut cache = RosterCache::new();
    cache.set_count("Northside Elementary", 412);
    assert_eq!(cache.cached_count("Northside Elementary"), 412);
}

#[test]
fn a_later_update_is_visible_to_the_dashboard() {
    let mut cache = RosterCache::new();
    cache.set_count("Northside Elementary", 412);
    assert_eq!(cache.cached_count("Northside Elementary"), 412);

    // Enrollment day: 27 students join.
    cache.set_count("Northside Elementary", 439);
    assert_eq!(
        cache.cached_count("Northside Elementary"),
        439,
        "the dashboard must not keep serving the pre-update count"
    );
}

#[test]
fn other_schools_are_unaffected_by_an_update() {
    let mut cache = RosterCache::new();
    cache.set_count("Northside Elementary", 412);
    cache.set_count("Southgate High", 980);
    assert_eq!(cache.cached_count("Southgate High"), 980);
    cache.set_count("Northside Elementary", 439);
    assert_eq!(cache.cached_count("Southgate High"), 980);
}
