use std::hash::Hash;

use indexmap::IndexSet;

/// The encoding used for the at-most-one ("forbid multiple instances")
/// constraints between the candidates of a single package.
///
/// This is an experimental knob used to evaluate the trade-offs between the
/// different encodings. All encodings are *incremental*: candidates are
/// discovered lazily during solving and clauses are added on the fly, so each
/// encoding must remain a valid at-most-one constraint over the variables
/// added so far at every step.
///
/// Every clause produced by any of these encodings is a binary clause of the
/// shape `(¬a ∨ b)` or `(¬a ∨ ¬b)`, which keeps the fast binary-clause
/// propagation path applicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmoEncoding {
    /// One clause `(¬xᵢ ∨ ¬xⱼ)` for every pair of candidates.
    ///
    /// `n(n-1)/2` clauses, no auxiliary variables. Propagation is a single
    /// hop and learnt clauses stay in terms of solvables, but the clause
    /// count grows quadratically.
    Pairwise,

    /// Each candidate is assigned a distinct bit pattern over `⌈log₂ n⌉`
    /// auxiliary variables; a candidate being true forces the pattern, which
    /// falsifies every other candidate.
    ///
    /// `n⌈log₂ n⌉` clauses and `⌈log₂ n⌉` auxiliary variables.
    Binary,

    /// The sequential ("ladder"/Sinz) encoding. A chain of auxiliary
    /// variables `sᵢ` encodes "one of x₀..xᵢ is true".
    ///
    /// `3n - 4` clauses and `n - 1` auxiliary variables.
    Sequential,

    /// Pairwise clauses while the package has at most `threshold` candidates,
    /// switching to the binary encoding beyond that.
    ///
    /// When the switch happens the binary encoding is emitted for all
    /// candidates seen so far; the earlier pairwise clauses remain (they are
    /// redundant but sound).
    Hybrid {
        /// The number of candidates up to which pairwise clauses are used.
        threshold: usize,
    },
}

impl Default for AmoEncoding {
    fn default() -> Self {
        AmoEncoding::Binary
    }
}

impl std::str::FromStr for AmoEncoding {
    type Err = String;

    /// Parses an encoding name: `pairwise`, `binary`, `sequential` or
    /// `hybrid:<threshold>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pairwise" => Ok(AmoEncoding::Pairwise),
            "binary" => Ok(AmoEncoding::Binary),
            "sequential" => Ok(AmoEncoding::Sequential),
            _ => {
                if let Some(threshold) = s.strip_prefix("hybrid:") {
                    let threshold = threshold
                        .parse()
                        .map_err(|e| format!("invalid hybrid threshold: {e}"))?;
                    Ok(AmoEncoding::Hybrid { threshold })
                } else {
                    Err(format!(
                        "unknown at-most-one encoding {s:?}, expected `pairwise`, `binary`, \
                         `sequential` or `hybrid:<threshold>`"
                    ))
                }
            }
        }
    }
}

impl AmoEncoding {
    /// Returns the encoding configured through the `RESOLVO_AMO_ENCODING`
    /// environment variable, or the default encoding if the variable is not
    /// set.
    ///
    /// This is an experimental escape hatch to compare encodings across
    /// existing test suites and benchmarks without code changes.
    pub fn from_env() -> Self {
        match std::env::var("RESOLVO_AMO_ENCODING") {
            Ok(value) => value
                .parse()
                .unwrap_or_else(|err: String| panic!("RESOLVO_AMO_ENCODING: {err}")),
            Err(_) => Self::default(),
        }
    }
}

/// An object that is responsible for incrementally building clauses to enforce
/// that at most one of a set of variables can be true.
pub(crate) struct AtMostOnceTracker<V> {
    /// The set of variables of which at most one can be assigned true.
    pub(crate) variables: IndexSet<V, ahash::RandomState>,
    /// Auxiliary variables: the bit variables for the binary encoding or the
    /// ladder variables for the sequential encoding.
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
        encoding: AmoEncoding,
        alloc_clause: impl FnMut(V, V, bool),
        alloc_var: impl FnMut() -> V,
    ) -> bool {
        // If the variable is already tracked, we don't need to do anything.
        if self.variables.contains(&variable) {
            return false;
        }

        // If there are no variables yet, it means that this is the first variable that
        // is added. A single variable can never be in conflict with itself so we don't
        // need to add any clauses.
        if self.variables.is_empty() {
            self.variables.insert(variable.clone());
            return true;
        }

        match encoding {
            AmoEncoding::Pairwise => self.add_pairwise(variable, alloc_clause),
            AmoEncoding::Binary => self.add_binary(variable, alloc_clause, alloc_var),
            AmoEncoding::Sequential => self.add_sequential(variable, alloc_clause, alloc_var),
            AmoEncoding::Hybrid { threshold } => {
                // Stay pairwise while the set is small and no helper variables
                // have been allocated yet. Once the threshold is crossed the
                // binary path backfills bit clauses for all existing
                // variables, so the constraint also holds between old
                // (pairwise) and new (binary) variables.
                if self.helpers.is_empty() && self.variables.len() < threshold {
                    self.add_pairwise(variable, alloc_clause)
                } else {
                    self.add_binary(variable, alloc_clause, alloc_var)
                }
            }
        }
    }

    /// (¬new ∨ ¬existing) for every existing variable.
    fn add_pairwise(&mut self, variable: V, mut alloc_clause: impl FnMut(V, V, bool)) -> bool {
        for existing in self.variables.iter().cloned() {
            alloc_clause(variable.clone(), existing, false);
        }
        self.variables.insert(variable);
        true
    }

    /// Each variable gets the bit pattern of its index over the helper
    /// variables: (¬x ∨ bitᵢ) or (¬x ∨ ¬bitᵢ) for every bit.
    fn add_binary(
        &mut self,
        variable: V,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        // Allocate additional helper variables as needed and create clauses for the
        // preexisting variables.
        while self.variables.len() > (1 << self.helpers.len()) - 1 {
            // Allocate the helper variable
            let helper_var = alloc_var();
            let bit_idx = self.helpers.len();
            self.helpers.push(helper_var.clone());

            // Add a negative clause for all existing variables.
            let mask = 1 << bit_idx;
            for (idx, var) in self.variables.iter().cloned().enumerate() {
                alloc_clause(var, helper_var.clone(), (idx & mask) == mask);
            }
        }

        let var_idx = self.variables.len();
        self.variables.insert(variable.clone());
        for (bit_idx, helper_var) in self.helpers.iter().enumerate() {
            alloc_clause(
                variable.clone(),
                helper_var.clone(),
                ((var_idx >> bit_idx) & 1) == 1,
            );
        }

        true
    }

    /// The ladder variable sᵢ means "one of x₀..xᵢ is true". Adding the k-th
    /// variable (k ≥ 1) allocates s₍ₖ₋₁₎ and emits:
    ///
    /// - (¬x₍ₖ₋₁₎ ∨ s₍ₖ₋₁₎): the previous variable raises the ladder,
    /// - (¬s₍ₖ₋₂₎ ∨ s₍ₖ₋₁₎): the ladder is monotone,
    /// - (¬xₖ ∨ ¬s₍ₖ₋₁₎): the new variable conflicts with the ladder.
    fn add_sequential(
        &mut self,
        variable: V,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        let prev_var = self
            .variables
            .last()
            .cloned()
            .expect("sequential encoding requires at least one existing variable");
        let ladder_var = alloc_var();

        alloc_clause(prev_var, ladder_var.clone(), true);
        if let Some(prev_ladder) = self.helpers.last().cloned() {
            alloc_clause(prev_ladder, ladder_var.clone(), true);
        }
        alloc_clause(variable.clone(), ladder_var.clone(), false);

        self.helpers.push(ladder_var);
        self.variables.insert(variable);
        true
    }
}

#[cfg(test)]
mod test {
    use std::fmt::{Display, Formatter};

    use itertools::Itertools;

    use super::*;

    #[derive(Hash, Eq, PartialEq, Clone)]
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

    fn clauses_for(encoding: AmoEncoding, n: usize) -> Vec<NotBothClause> {
        let mut clauses = Vec::new();
        let mut variables = Variables::new();
        let mut tracker = AtMostOnceTracker::default();

        for _ in 0..n {
            let var = variables.concrete();
            tracker.add(
                var.clone(),
                encoding,
                |a, b, positive| {
                    clauses.push(NotBothClause { a, b, positive });
                },
                || variables.variable(),
            );
        }

        clauses
    }

    #[test]
    fn test_at_most_once_tracker() {
        let clauses = clauses_for(AmoEncoding::Binary, 4);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    #[test]
    fn test_at_most_once_tracker_pairwise() {
        let clauses = clauses_for(AmoEncoding::Pairwise, 4);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    #[test]
    fn test_at_most_once_tracker_sequential() {
        let clauses = clauses_for(AmoEncoding::Sequential, 4);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    #[test]
    fn test_at_most_once_tracker_hybrid() {
        let clauses = clauses_for(AmoEncoding::Hybrid { threshold: 3 }, 5);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    /// Brute-force check: for every assignment with two or more true
    /// variables, at least one clause must be violated (given the best-case
    /// assignment of helper variables), and assignments with at most one true
    /// variable must be extendable to a model.
    #[test]
    fn test_encodings_enforce_at_most_one() {
        for encoding in [
            AmoEncoding::Pairwise,
            AmoEncoding::Binary,
            AmoEncoding::Sequential,
            AmoEncoding::Hybrid { threshold: 3 },
        ] {
            for n in 1..=6usize {
                let mut clauses = Vec::new();
                let mut concrete = Vec::new();
                let mut helpers = Vec::new();
                let mut next_helper = 0usize;
                let mut tracker = AtMostOnceTracker::default();

                for i in 0..n {
                    concrete.push(Variable::Concrete(i));
                    tracker.add(
                        Variable::Concrete(i),
                        encoding,
                        |a, b, positive| {
                            clauses.push(NotBothClause { a, b, positive });
                        },
                        || {
                            let v = Variable::Variable(next_helper);
                            next_helper += 1;
                            helpers.push(v.clone());
                            v
                        },
                    );
                }

                // Try every assignment of the concrete variables and check
                // satisfiability of the clause set by brute-forcing helpers.
                for assignment in 0..(1u32 << n) {
                    let value = |var: &Variable, helper_bits: u32| match var {
                        Variable::Concrete(i) => (assignment >> i) & 1 == 1,
                        Variable::Variable(i) => (helper_bits >> i) & 1 == 1,
                    };
                    let satisfiable = (0..(1u32 << helpers.len())).any(|helper_bits| {
                        clauses.iter().all(|c| {
                            let a = value(&c.a, helper_bits);
                            let b = value(&c.b, helper_bits);
                            !a || (b == c.positive)
                        })
                    });
                    let true_count = assignment.count_ones();
                    assert_eq!(
                        satisfiable,
                        true_count <= 1,
                        "{encoding:?} with {n} variables: assignment {assignment:b} \
                         (count={true_count}) should be {}",
                        if true_count <= 1 { "sat" } else { "unsat" }
                    );
                }
            }
        }
    }
}
