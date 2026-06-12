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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    #[default]
    Binary,

    /// The sequential ("ladder"/Sinz) encoding. A chain of auxiliary
    /// variables `sᵢ` encodes "one of x₀..xᵢ is true".
    ///
    /// `3n - 4` clauses and `n - 1` auxiliary variables.
    Sequential,

    /// The commander encoding (Klieber & Kwon). Variables are split into
    /// groups of `group_size`; within a group the constraint is pairwise, a
    /// commander variable is implied by every member of its group, and the
    /// at-most-one constraint over the commanders themselves is enforced
    /// recursively.
    ///
    /// Roughly `n(group_size + 1)/2` clauses and `n/(group_size - 1)`
    /// auxiliary variables.
    Commander {
        /// The number of variables per group. Must be at least 2.
        group_size: usize,
    },

    /// The bimander encoding (Hölldobler & Nguyen), a hybrid of the binary
    /// and commander encodings. Variables are split into groups of
    /// `group_size`; within a group the constraint is pairwise, and instead
    /// of a commander variable per group the index of a variable's group is
    /// encoded in binary over `⌈log₂(n / group_size)⌉` shared bit variables.
    ///
    /// Roughly `n((group_size - 1)/2 + log₂(n / group_size))` clauses and
    /// `⌈log₂(n / group_size)⌉` auxiliary variables.
    Bimander {
        /// The number of variables per group.
        group_size: usize,
    },

    /// No clauses at all: sibling negations are *virtual*. Assigning a
    /// candidate true records a package-level selection that is consulted by
    /// every value lookup; all other candidates of the package evaluate to
    /// false without being physically assigned. Pairwise reason clauses are
    /// materialized lazily only when conflict analysis needs them.
    ///
    /// See `docs/virtual-sibling-negations.md` for the design.
    Virtual,

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

impl std::str::FromStr for AmoEncoding {
    type Err = String;

    /// Parses an encoding name: `pairwise`, `binary`, `sequential` or
    /// `hybrid:<threshold>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pairwise" => Ok(AmoEncoding::Pairwise),
            "binary" => Ok(AmoEncoding::Binary),
            "sequential" => Ok(AmoEncoding::Sequential),
            "commander" => Ok(AmoEncoding::Commander { group_size: 3 }),
            "bimander" => Ok(AmoEncoding::Bimander { group_size: 2 }),
            "virtual" => Ok(AmoEncoding::Virtual),
            _ => {
                if let Some(threshold) = s.strip_prefix("hybrid:") {
                    let threshold = threshold
                        .parse()
                        .map_err(|e| format!("invalid hybrid threshold: {e}"))?;
                    Ok(AmoEncoding::Hybrid { threshold })
                } else if let Some(group_size) = s.strip_prefix("commander:") {
                    let group_size: usize = group_size
                        .parse()
                        .map_err(|e| format!("invalid commander group size: {e}"))?;
                    if group_size < 2 {
                        return Err("commander group size must be at least 2".to_string());
                    }
                    Ok(AmoEncoding::Commander { group_size })
                } else if let Some(group_size) = s.strip_prefix("bimander:") {
                    let group_size: usize = group_size
                        .parse()
                        .map_err(|e| format!("invalid bimander group size: {e}"))?;
                    if group_size < 1 {
                        return Err("bimander group size must be at least 1".to_string());
                    }
                    Ok(AmoEncoding::Bimander { group_size })
                } else {
                    Err(format!(
                        "unknown at-most-one encoding {s:?}, expected `pairwise`, `binary`, \
                         `sequential`, `commander[:<group size>]`, `bimander[:<group size>]` \
                         or `hybrid:<threshold>`"
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
    /// Per-level group state for the commander encoding. Level 0 groups the
    /// tracked variables themselves, level `k + 1` groups the commander
    /// variables of level `k`.
    commander_levels: Vec<CommanderLevel<V>>,
    /// The package index in the decision map's registry, for the virtual
    /// encoding.
    pub(crate) virtual_package: Option<u32>,
}

/// The state of one level of the commander encoding.
struct CommanderLevel<V> {
    /// The members of the group that is currently being filled.
    open_group: Vec<V>,
    /// The commander variable of the open group. The very first group of a
    /// level only gets a commander once a second group is needed, because a
    /// single group is already mutually exclusive through its pairwise
    /// clauses.
    open_commander: Option<V>,
    /// The number of groups that have been filled completely.
    closed_groups: usize,
    /// The members of the first group if it was closed before any other group
    /// existed. Its commander is allocated retroactively when a second group
    /// is opened.
    deferred_first_group: Option<Vec<V>>,
}

impl<V> Default for CommanderLevel<V> {
    fn default() -> Self {
        Self {
            open_group: Vec::new(),
            open_commander: None,
            closed_groups: 0,
            deferred_first_group: None,
        }
    }
}

impl<V> Default for AtMostOnceTracker<V> {
    fn default() -> Self {
        Self {
            variables: IndexSet::default(),
            helpers: Vec::new(),
            commander_levels: Vec::new(),
            virtual_package: None,
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
        // need to add any clauses. The commander encoding still has to record
        // the variable as a member of its first group.
        if self.variables.is_empty() {
            match encoding {
                AmoEncoding::Commander { group_size } => {
                    return self.add_commander(
                        variable,
                        group_size.max(2),
                        alloc_clause,
                        alloc_var,
                    );
                }
                AmoEncoding::Bimander { group_size } => {
                    return self.add_bimander(variable, group_size.max(1), alloc_clause, alloc_var);
                }
                _ => {
                    self.variables.insert(variable.clone());
                    return true;
                }
            }
        }

        match encoding {
            AmoEncoding::Pairwise => self.add_pairwise(variable, alloc_clause),
            AmoEncoding::Binary => self.add_binary(variable, alloc_clause, alloc_var),
            AmoEncoding::Sequential => self.add_sequential(variable, alloc_clause, alloc_var),
            AmoEncoding::Commander { group_size } => {
                self.add_commander(variable, group_size.max(2), alloc_clause, alloc_var)
            }
            AmoEncoding::Bimander { group_size } => {
                self.add_bimander(variable, group_size.max(1), alloc_clause, alloc_var)
            }
            AmoEncoding::Virtual => {
                unreachable!("the virtual encoding is handled directly by the encoder")
            }
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

    /// Within a group the constraint is pairwise; every member implies the
    /// group's commander variable and the commanders are themselves subject to
    /// an at-most-one constraint one level up.
    ///
    /// Groups are filled one at a time. The first group of a level defers
    /// allocating its commander until the group is full (a single group is
    /// already mutually exclusive through its pairwise clauses); every later
    /// group allocates its commander as soon as it is opened so that the
    /// level-up constraint between the open group and the closed groups holds
    /// at all times.
    fn add_commander(
        &mut self,
        variable: V,
        group_size: usize,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        self.variables.insert(variable.clone());

        // Insert the variable at level 0. Closing or opening a group produces
        // commander variables that have to be inserted one level up.
        let mut work = vec![(0usize, variable)];
        while let Some((level_idx, var)) = work.pop() {
            if self.commander_levels.len() <= level_idx {
                self.commander_levels.push(CommanderLevel::default());
            }
            let level = &mut self.commander_levels[level_idx];

            // Pairwise within the open group, and a link to the group's
            // commander if it already has one.
            for member in &level.open_group {
                alloc_clause(var.clone(), member.clone(), false);
            }
            if let Some(commander) = &level.open_commander {
                alloc_clause(var.clone(), commander.clone(), true);
            }
            level.open_group.push(var);

            if level.open_group.len() == 1 && level.closed_groups > 0 {
                // A new group was just opened next to existing closed groups:
                // it needs a commander right away to be mutually exclusive
                // with the closed groups one level up.
                let commander = alloc_var();
                self.helpers.push(commander.clone());
                alloc_clause(level.open_group[0].clone(), commander.clone(), true);
                level.open_commander = Some(commander.clone());
                work.push((level_idx + 1, commander));

                // If the first group of this level was closed without a
                // commander, allocate one for it now.
                if let Some(members) = level.deferred_first_group.take() {
                    let commander = alloc_var();
                    self.helpers.push(commander.clone());
                    for member in members {
                        alloc_clause(member, commander.clone(), true);
                    }
                    work.push((level_idx + 1, commander));
                }
            } else if level.open_group.len() == group_size {
                // The group is full; close it.
                level.closed_groups += 1;
                if level.open_commander.take().is_some() {
                    // The commander was already inserted one level up when the
                    // group was opened.
                    level.open_group.clear();
                } else {
                    // The first group of a level closes without a commander; a
                    // single group is mutually exclusive through its pairwise
                    // clauses alone. Keep the members for the retrofit above.
                    debug_assert_eq!(level.closed_groups, 1);
                    level.deferred_first_group = Some(std::mem::take(&mut level.open_group));
                }
            }
        }

        true
    }

    /// Like the commander encoding, variables are split into groups that are
    /// internally pairwise, but instead of a commander variable per group the
    /// index of a variable's group is encoded in binary over shared bit
    /// variables (like the binary encoding, but per group instead of per
    /// variable). Two variables from different groups force contradictory
    /// values on at least one bit.
    ///
    /// Reuses level 0 of [`Self::commander_levels`] for the group bookkeeping
    /// (the commander-specific fields stay unused) and [`Self::helpers`] for
    /// the bit variables.
    fn add_bimander(
        &mut self,
        variable: V,
        group_size: usize,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        if self.commander_levels.is_empty() {
            self.commander_levels.push(CommanderLevel::default());
        }

        // If the open group is full, close it and open the next one. This is
        // done before inserting `variable` so that the bit backfill below only
        // covers the variables of earlier groups.
        if self.commander_levels[0].open_group.len() == group_size {
            let level = &mut self.commander_levels[0];
            level.open_group.clear();
            level.closed_groups += 1;
            let group_idx = level.closed_groups;

            // Allocate bit variables until the new group's index is
            // representable. All existing variables belong to earlier groups
            // whose indices have a 0 in any newly allocated bit.
            while group_idx >= (1 << self.helpers.len()) {
                let bit = alloc_var();
                self.helpers.push(bit.clone());
                for var in self.variables.iter().cloned() {
                    alloc_clause(var, bit.clone(), false);
                }
            }
        }

        self.variables.insert(variable.clone());
        let level = &mut self.commander_levels[0];

        // Pairwise within the group.
        for member in &level.open_group {
            alloc_clause(variable.clone(), member.clone(), false);
        }
        level.open_group.push(variable.clone());

        // The bit pattern of the group's index.
        let group_idx = level.closed_groups;
        for (bit_idx, bit) in self.helpers.iter().enumerate() {
            alloc_clause(
                variable.clone(),
                bit.clone(),
                ((group_idx >> bit_idx) & 1) == 1,
            );
        }

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

    #[test]
    fn test_at_most_once_tracker_commander() {
        let clauses = clauses_for(AmoEncoding::Commander { group_size: 3 }, 7);
        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    #[test]
    fn test_at_most_once_tracker_bimander() {
        let clauses = clauses_for(AmoEncoding::Bimander { group_size: 2 }, 7);
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
            AmoEncoding::Commander { group_size: 2 },
            AmoEncoding::Commander { group_size: 3 },
            AmoEncoding::Bimander { group_size: 1 },
            AmoEncoding::Bimander { group_size: 2 },
            AmoEncoding::Bimander { group_size: 3 },
            AmoEncoding::Hybrid { threshold: 3 },
        ] {
            for n in 1..=8usize {
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
