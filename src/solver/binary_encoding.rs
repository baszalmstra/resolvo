use bitvec::vec::BitVec;

use crate::internal::arena::ArenaId;

/// An object that is responsible for incrementally building clauses to enforce
/// that at most one of a set of variables can be true.
pub(crate) struct AtMostOnceTracker<V> {
    /// The set of variables of which at most one can be assigned true.
    /// Stored as a Vec for ordered iteration with indices.
    pub(crate) variables: Vec<V>,
    /// BitVec for O(1) membership checking, indexed by variable ID.
    seen: BitVec,
    pub(crate) helpers: Vec<V>,
}

impl<V> Default for AtMostOnceTracker<V> {
    fn default() -> Self {
        Self {
            variables: Vec::new(),
            seen: BitVec::new(),
            helpers: Vec::new(),
        }
    }
}

impl<V: ArenaId + Copy> AtMostOnceTracker<V> {
    /// Add a `variable` to the set of variables of which at most one can be
    /// assigned true.
    ///
    /// `alloc_clause` is a closure that is called to allocate a new clause to
    /// enforce (¬A ∨ B?). Where B is either positive or negative based on the
    /// value passed to the closure.
    pub fn add(
        &mut self,
        variable: V,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        // If the variable is already tracked, we don't need to do anything.
        let var_id = variable.to_usize();
        if self.seen.len() > var_id && self.seen[var_id] {
            return false;
        }

        // If there are no variables yet, it means that this is the first variable that
        // is added. A single variable can never be in conflict with itself so we don't
        // need to add any clauses.
        if self.variables.is_empty() {
            if self.seen.len() <= var_id {
                self.seen.resize(var_id + 1, false);
            }
            self.seen.set(var_id, true);
            self.variables.push(variable);
            return true;
        }

        // Allocate additional helper variables as needed and create clauses for the
        // preexisting variables.
        while self.variables.len() > (1 << self.helpers.len()) - 1 {
            // Allocate the helper variable
            let helper_var = alloc_var();
            let bit_idx = self.helpers.len();
            self.helpers.push(helper_var);

            // Add a negative clause for all existing variables.
            let mask = 1 << bit_idx;
            for (idx, &var) in self.variables.iter().enumerate() {
                alloc_clause(var, helper_var, (idx & mask) == mask);
            }
        }

        let var_idx = self.variables.len();
        if self.seen.len() <= var_id {
            self.seen.resize(var_id + 1, false);
        }
        self.seen.set(var_id, true);
        self.variables.push(variable);
        for (bit_idx, &helper_var) in self.helpers.iter().enumerate() {
            alloc_clause(variable, helper_var, ((var_idx >> bit_idx) & 1) == 1);
        }

        true
    }
}

#[cfg(test)]
mod test {
    use std::fmt::{Display, Formatter};

    use itertools::Itertools;

    use super::*;

    #[derive(Hash, Eq, PartialEq, Clone, Copy)]
    enum Variable {
        Concrete(usize),
        Helper(usize),
    }

    impl ArenaId for Variable {
        fn from_usize(x: usize) -> Self {
            // Use high bit to distinguish concrete vs helper
            if x & (1 << 31) != 0 {
                Variable::Helper(x & !(1 << 31))
            } else {
                Variable::Concrete(x)
            }
        }

        fn to_usize(self) -> usize {
            match self {
                Variable::Concrete(id) => id,
                Variable::Helper(id) => id | (1 << 31),
            }
        }
    }

    impl Display for Variable {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Variable::Concrete(id) => write!(f, "x{}", id),
                Variable::Helper(id) => write!(f, "v{}", id),
            }
        }
    }

    struct Variables {
        next_concrete: usize,
        next_helper: usize,
    }

    impl Variables {
        fn new() -> Self {
            Self {
                next_concrete: 1,
                next_helper: 1,
            }
        }

        fn concrete(&mut self) -> Variable {
            let id = self.next_concrete;
            self.next_concrete += 1;
            Variable::Concrete(id)
        }

        fn helper(&mut self) -> Variable {
            let id = self.next_helper;
            self.next_helper += 1;
            Variable::Helper(id)
        }
    }

    struct NotBothClause {
        a: Variable,
        b: Variable,
        positive: bool,
    }

    impl Display for NotBothClause {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "¬{} ∨ {}{}",
                self.a,
                if self.positive { "" } else { "¬" },
                self.b
            )
        }
    }

    #[test]
    fn test_at_most_once_tracker() {
        let mut clauses = Vec::new();
        let mut variables = Variables::new();
        let mut tracker = AtMostOnceTracker::default();

        let mut add_new_var = || {
            let var = variables.concrete();
            tracker.add(
                var,
                |a, b, positive| {
                    clauses.push(NotBothClause { a, b, positive });
                },
                || variables.helper(),
            );
            var
        };

        add_new_var();
        add_new_var();
        add_new_var();
        add_new_var();

        insta::assert_snapshot!(clauses.iter().format("\n"));
    }
}
