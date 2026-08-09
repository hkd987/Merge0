//! Cached roster counts for the admin dashboard.

use std::collections::HashMap;

#[derive(Default)]
pub struct RosterCache {
    counts: HashMap<String, usize>,
    cache: HashMap<String, usize>,
}

impl RosterCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the authoritative roster count for a school.
    pub fn set_count(&mut self, school: &str, count: usize) {
        self.counts.insert(school.to_string(), count);
        // BUG: the cached copy is never invalidated here, so the dashboard
        // keeps serving the count from before the roster changed.
    }

    /// The count the admin dashboard renders.
    pub fn cached_count(&mut self, school: &str) -> usize {
        if let Some(cached) = self.cache.get(school) {
            return *cached;
        }
        let count = self.counts.get(school).copied().unwrap_or(0);
        self.cache.insert(school.to_string(), count);
        count
    }
}
