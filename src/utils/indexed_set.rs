use std::marker::PhantomData;

use crate::id::DenseIndex;

/// A dense set keyed by a [`DenseIndex`]. Equivalent to a `HashSet<Id>` but
/// backed by plain `u64` words, so test-and-set is O(1) with no hashing
/// overhead. Grows on demand to fit the largest inserted index.
pub struct IndexedSet<Id> {
    words: Vec<u64>,
    _marker: PhantomData<fn(Id) -> Id>,
}

impl<Id> Default for IndexedSet<Id> {
    fn default() -> Self {
        Self {
            words: Vec::new(),
            _marker: PhantomData,
        }
    }
}

impl<Id: DenseIndex> IndexedSet<Id> {
    /// Inserts `id`. Returns `true` if `id` was not already present.
    #[inline]
    pub fn insert(&mut self, id: Id) -> bool {
        let idx = id.to_index();
        let (word, bit) = (idx / 64, 1u64 << (idx % 64));
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let entry = &mut self.words[word];
        let was_set = *entry & bit != 0;
        *entry |= bit;
        !was_set
    }

    /// Returns `true` if `id` is present.
    #[inline]
    pub fn contains(&self, id: Id) -> bool {
        let idx = id.to_index();
        self.words
            .get(idx / 64)
            .is_some_and(|word| word & (1u64 << (idx % 64)) != 0)
    }
}
