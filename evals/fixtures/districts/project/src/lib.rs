//! District resolution for school sync.

pub struct School {
    pub name: String,
    pub district: Option<String>,
}

/// Resolve the district label shown on /districts/sync.
pub fn resolve_district(school: &School) -> String {
    // BUG: schools created during onboarding have no district yet; this
    // unwrap crashes the whole sync page for them.
    school.district.clone().unwrap()
}
