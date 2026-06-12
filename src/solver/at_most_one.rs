use std::hash::Hash;

use indexmap::IndexSet;

/// An object that is responsible for incrementally building clauses to enforce
/// that at most one of a set of variables can be true.
///
/// The constraint is encoded with a sequential ("ladder") encoding (the
/// at-most-one special case of the sequential counter from [Sinz 2005]).
/// Helper variable `s_j` means "one of the candidates with insertion index
/// `<= j` is true". For the `i`-th candidate (`i >= 1`) the tracker allocates
/// `s_{i-1}` and emits three binary clauses:
///
/// * `(¬x_{i-1} ∨ s_{i-1})` — candidate `i-1` raises its flag,
/// * `(¬s_{i-2} ∨ s_{i-1})` — flags are monotone along the chain (`i >= 2`),
/// * `(¬x_i ∨ ¬s_{i-1})` — a candidate conflicts with every flag before it.
///
/// Any two distinct candidates `x_j` and `x_i` (`j < i`) conflict through the
/// chain: `x_j` forces `s_j, …, s_{i-1}` which forces `¬x_i`.
///
/// The helper variables are deliberately aligned with the candidate order
/// (which is the order in which the solver tries candidates): `¬s_j` prunes
/// the whole prefix of candidates up to `j` at once. This matters for
/// conflict-driven learning: when a helper variable ends up in a learnt
/// clause, the lesson it encodes ("no candidate up to index j works here")
/// matches how the solver enumerates siblings, so lessons compound
/// monotonically instead of fragmenting per candidate. A binary (bitwise)
/// encoding needs fewer helper variables, but its helpers prune arbitrary
/// index halves; learnt clauses over such helpers flip-flop between
/// conflicting contexts and the solver rediscovers essentially the same
/// conflict once per sibling candidate (e.g. once per `python_abi` build on
/// conda-forge style workloads).
///
/// [Sinz 2005]: https://doi.org/10.1007/11564751_73
pub(crate) struct AtMostOnceTracker<V> {
    /// The set of variables of which at most one can be assigned true.
    pub(crate) variables: IndexSet<V, ahash::RandomState>,
    /// The ladder variables; `helpers[j]` is `s_j` ("one of the candidates
    /// with insertion index `<= j` is true").
    pub(crate) helpers: Vec<V>,
}

impl<V> Default for AtMostOnceTracker<V> {
    fn default() -> Self {
        Self {
            variables: IndexSet::default(),
            helpers: Vec::new(),
        }
    }
}

impl<V: Hash + Eq + Clone> AtMostOnceTracker<V> {
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
        if self.variables.contains(&variable) {
            return false;
        }

        let i = self.variables.len();
        self.variables.insert(variable.clone());

        // A single variable can never be in conflict with itself so the first
        // candidate needs no clauses.
        if i == 0 {
            return true;
        }

        // Allocate the ladder variable `s_{i-1}` and connect it to the
        // previous candidate, the previous ladder variable, and the new
        // candidate.
        debug_assert_eq!(self.helpers.len(), i - 1);
        let s_prev = alloc_var();
        self.helpers.push(s_prev.clone());
        let x_prev = self
            .variables
            .get_index(i - 1)
            .expect("previous candidate must exist")
            .clone();
        alloc_clause(x_prev, s_prev.clone(), true);
        if i >= 2 {
            let s_prev_prev = self.helpers[i - 2].clone();
            alloc_clause(s_prev_prev, s_prev.clone(), true);
        }
        alloc_clause(variable, s_prev, false);

        true
    }
}

#[cfg(test)]
mod test {
    use std::fmt::{Display, Formatter};

    use itertools::Itertools;

    use super::*;

    #[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
    enum Variable {
        Concrete(usize),
        Variable(usize),
    }

    impl Display for Variable {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Variable::Concrete(id) => write!(f, "x{}", id),
                Variable::Variable(id) => write!(f, "v{}", id),
            }
        }
    }

    struct Variables {
        next_concrete: usize,
        next_variable: usize,
    }

    impl Variables {
        fn new() -> Self {
            Self {
                next_concrete: 1,
                next_variable: 1,
            }
        }

        fn concrete(&mut self) -> Variable {
            let id = self.next_concrete;
            self.next_concrete += 1;
            Variable::Concrete(id)
        }

        fn variable(&mut self) -> Variable {
            let id = self.next_variable;
            self.next_variable += 1;
            Variable::Variable(id)
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

    fn build_clauses(num_candidates: usize) -> (Vec<NotBothClause>, Vec<Variable>) {
        let mut clauses = Vec::new();
        let mut variables = Variables::new();
        let mut tracker = AtMostOnceTracker::default();
        let mut candidates = Vec::new();

        for _ in 0..num_candidates {
            let var = variables.concrete();
            candidates.push(var);
            tracker.add(
                var,
                |a, b, positive| {
                    clauses.push(NotBothClause { a, b, positive });
                },
                || variables.variable(),
            );
        }

        (clauses, candidates)
    }

    #[test]
    fn test_at_most_once_tracker() {
        let (clauses, _) = build_clauses(4);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    /// Brute-force check that the emitted clauses enforce exactly an
    /// at-most-one constraint over the candidates: any assignment with two
    /// candidates set true violates a clause, and any assignment with at most
    /// one candidate set true can be extended to satisfy all clauses.
    #[test]
    fn test_at_most_once_semantics() {
        for num_candidates in 1..=5usize {
            let (clauses, candidates) = build_clauses(num_candidates);
            let num_helpers = num_candidates.saturating_sub(1);
            let eval = |var: &Variable, assignment: u32| -> bool {
                match var {
                    Variable::Concrete(id) => (assignment >> (id - 1)) & 1 == 1,
                    Variable::Variable(id) => (assignment >> (num_candidates + id - 1)) & 1 == 1,
                }
            };

            let mut satisfiable_candidate_sets = ahash::HashSet::default();
            for assignment in 0..(1u32 << (num_candidates + num_helpers)) {
                let satisfied = clauses.iter().all(|clause| {
                    !eval(&clause.a, assignment)
                        || (eval(&clause.b, assignment) == clause.positive)
                });
                if satisfied {
                    let true_candidates = candidates
                        .iter()
                        .filter(|c| eval(c, assignment))
                        .collect::<Vec<_>>();
                    assert!(
                        true_candidates.len() <= 1,
                        "at-most-one violated for n={num_candidates}: {true_candidates:?}"
                    );
                    let mask = candidates
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| eval(c, assignment))
                        .map(|(i, _)| 1u32 << i)
                        .sum::<u32>();
                    satisfiable_candidate_sets.insert(mask);
                }
            }

            // The empty set and every singleton must be satisfiable.
            assert!(satisfiable_candidate_sets.contains(&0));
            for i in 0..num_candidates {
                assert!(
                    satisfiable_candidate_sets.contains(&(1u32 << i)),
                    "candidate {i} cannot be selected for n={num_candidates}"
                );
            }
        }
    }
}
