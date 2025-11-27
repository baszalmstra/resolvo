use std::hash::Hash;

use indexmap::IndexSet;

/// Tracks variables that are part of an at-most-one constraint.
/// When any variable is set to TRUE, all other variables must be FALSE.
pub(crate) struct AtMostOnceTracker<V> {
    /// The set of variables of which at most one can be assigned true.
    /// Uses IndexSet for fast iteration during propagation.
    pub(crate) variables: IndexSet<V, ahash::RandomState>,
}

impl<V> Default for AtMostOnceTracker<V> {
    fn default() -> Self {
        Self {
            variables: IndexSet::default(),
        }
    }
}

impl<V: Hash + Eq> AtMostOnceTracker<V> {
    /// Add a variable to the set of variables of which at most one can be
    /// assigned true. Returns true if the variable was newly added.
    pub fn add(&mut self, variable: V) -> bool {
        self.variables.insert(variable)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_at_most_once_tracker() {
        let mut tracker: AtMostOnceTracker<usize> = AtMostOnceTracker::default();

        assert!(tracker.add(1));
        assert!(tracker.add(2));
        assert!(tracker.add(3));
        assert!(!tracker.add(2)); // Already exists

        assert_eq!(tracker.variables.len(), 3);
        assert!(tracker.variables.contains(&1));
        assert!(tracker.variables.contains(&2));
        assert!(tracker.variables.contains(&3));
    }
}
