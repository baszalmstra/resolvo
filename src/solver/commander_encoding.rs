//! Incrementally builds clauses enforcing that *at most one* of a set of
//! variables is true (the "forbid multiple instances" constraint: only one
//! candidate of a package may be selected).
//!
//! # Commander encoding
//!
//! This uses the commander encoding (Klieber & Kwon, 2007). The candidates are
//! partitioned into groups of [`GROUP_SIZE`]. The constraint is then the
//! conjunction of:
//!
//! - *within each group*, a pairwise at-most-one (`¬xᵢ ∨ ¬xⱼ` for every pair),
//! - *per group*, a fresh **commander** variable implied by every member
//!   (`¬xᵢ ∨ c`), so the commander is true whenever any member of its group
//!   is, and
//! - recursively, an at-most-one over the commanders themselves.
//!
//! If two candidates in the same group were both true the pairwise clauses
//! would be violated; if they were in different groups both commanders would
//! be true, violating the at-most-one one level up. The recursion bottoms out
//! when a level has a single group.
//!
//! For `n` candidates and group size `g` this is `Θ(n)` clauses and
//! `Θ(n / (g − 1))` auxiliary variables — linear, versus the `Θ(n log n)`
//! clauses of a binary/bitwise encoding. Fewer clauses means less propagation
//! work and lower memory; the auxiliary variables also align with contiguous
//! preference-ordered candidate ranges, which lets conflict analysis learn
//! clauses that generalize across whole version windows.
//!
//! # Incremental construction
//!
//! Candidates are discovered lazily while solving, so they are added one at a
//! time through [`AtMostOnceTracker::add`] and the constraint must hold over
//! the candidates seen so far at every step. Groups are therefore filled as
//! candidates arrive. The subtlety is the commander of a group: a *lone* group
//! needs none (its pairwise clauses already forbid two of its members), but as
//! soon as a second group exists every group must have a commander to be
//! mutually exclusive with the others. The first group of each level
//! therefore **defers** allocating its commander until a second group opens,
//! at which point it is created retroactively. This keeps small packages
//! (a single group, the common case) at pure pairwise cost with no auxiliary
//! variables at all.

use std::hash::Hash;

use indexmap::IndexSet;

/// The number of candidates per commander group.
///
/// Three is the empirically fastest choice for dependency-resolution
/// workloads: it ties the linear (sequential) encoding on clause count while
/// using roughly half as many auxiliary variables.
const GROUP_SIZE: usize = 3;

/// The state of the group currently being filled at one recursion level.
/// Level 0 groups the candidate variables; level `k + 1` groups the commander
/// variables produced when level-`k` groups fill up.
struct Level<V> {
    /// Members of the group currently being filled.
    open: Vec<V>,
    /// The open group's commander, once allocated. Members added afterwards
    /// are linked to it immediately; members added before allocation are
    /// linked when it is created.
    commander: Option<V>,
    /// The members of the very first group of this level, retained only until
    /// a second group appears so the first group's commander can be allocated
    /// retroactively (see the module docs). `None` once that has happened.
    deferred_first_group: Option<Vec<V>>,
    /// The number of groups that have been filled completely at this level.
    closed_groups: usize,
}

impl<V> Level<V> {
    fn new() -> Self {
        Self {
            open: Vec::new(),
            commander: None,
            deferred_first_group: None,
            closed_groups: 0,
        }
    }
}

/// Incrementally builds the at-most-one clauses for a set of variables using
/// the commander encoding.
pub(crate) struct AtMostOnceTracker<V> {
    /// Every candidate variable added so far (the level-0 members). Exposed so
    /// the encoder can build the separate "at least one" clauses over the same
    /// candidates.
    pub(crate) variables: IndexSet<V, ahash::RandomState>,
    /// The group state per recursion level.
    levels: Vec<Level<V>>,
}

impl<V> Default for AtMostOnceTracker<V> {
    fn default() -> Self {
        Self {
            variables: IndexSet::default(),
            levels: Vec::new(),
        }
    }
}

impl<V: Hash + Eq + Clone> AtMostOnceTracker<V> {
    /// Adds `variable` to the set of which at most one may be true, emitting
    /// the clauses this requires. Returns `true` if the variable was newly
    /// added, `false` if it was already tracked.
    ///
    /// `alloc_clause(a, b, positive)` allocates the binary clause `(¬a ∨ b)`
    /// when `positive`, or `(¬a ∨ ¬b)` otherwise. `alloc_var` mints a fresh
    /// commander variable.
    pub fn add(
        &mut self,
        variable: V,
        mut alloc_clause: impl FnMut(V, V, bool),
        mut alloc_var: impl FnMut() -> V,
    ) -> bool {
        if !self.variables.insert(variable.clone()) {
            return false;
        }
        self.insert(0, variable, &mut alloc_clause, &mut alloc_var);
        true
    }

    /// Inserts `variable` as a member of the open group at `level`, emitting
    /// the within-group and commander-link clauses, and — when a group fills
    /// or a new group opens beside existing ones — allocating commanders and
    /// recursing one level up.
    fn insert<C, A>(&mut self, level: usize, variable: V, alloc_clause: &mut C, alloc_var: &mut A)
    where
        C: FnMut(V, V, bool),
        A: FnMut() -> V,
    {
        if self.levels.len() == level {
            self.levels.push(Level::new());
        }

        // Pairwise within the open group, and a link to the group's commander
        // if it already has one.
        let lvl = &mut self.levels[level];
        for member in &lvl.open {
            alloc_clause(variable.clone(), member.clone(), false);
        }
        if let Some(commander) = &lvl.commander {
            alloc_clause(variable.clone(), commander.clone(), true);
        }
        lvl.open.push(variable);

        if lvl.open.len() == GROUP_SIZE {
            self.close_group(level);
        } else if lvl.open.len() == 1 && lvl.closed_groups >= 1 {
            self.open_second_group(level, alloc_clause, alloc_var);
        }
    }

    /// Closes the now-full open group at `level`. A non-first group already
    /// has a commander, which became a member of the level above when the
    /// group opened (see [`Self::open_second_group`]), so there is nothing to
    /// bubble up here — the group is simply finished. The first group has no
    /// commander yet; its members are stashed so one can be allocated
    /// retroactively if a second group ever opens.
    fn close_group(&mut self, level: usize) {
        let lvl = &mut self.levels[level];
        lvl.closed_groups += 1;
        if lvl.commander.is_some() {
            // Finished; the next group starts fresh with its own commander.
            lvl.open.clear();
            lvl.commander = None;
        } else {
            lvl.deferred_first_group = Some(std::mem::take(&mut lvl.open));
        }
    }

    /// Handles the first member of a group that opens while other groups
    /// already exist at `level`. Such a group needs a commander immediately to
    /// be mutually exclusive with the rest; if the first group's commander was
    /// deferred it is allocated now as well. Each new commander bubbles up a
    /// level.
    fn open_second_group<C, A>(&mut self, level: usize, alloc_clause: &mut C, alloc_var: &mut A)
    where
        C: FnMut(V, V, bool),
        A: FnMut() -> V,
    {
        // Retroactively give the deferred first group its commander.
        if let Some(members) = self.levels[level].deferred_first_group.take() {
            let commander = alloc_var();
            for member in &members {
                alloc_clause(member.clone(), commander.clone(), true);
            }
            self.insert(level + 1, commander, alloc_clause, alloc_var);
        }

        // Give the freshly opened group its commander.
        let commander = alloc_var();
        let member = self.levels[level].open[0].clone();
        alloc_clause(member, commander.clone(), true);
        self.levels[level].commander = Some(commander.clone());
        self.insert(level + 1, commander, alloc_clause, alloc_var);
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

    #[test]
    fn test_at_most_once_tracker() {
        let mut clauses = Vec::new();
        let mut variables = Variables::new();
        let mut tracker = AtMostOnceTracker::default();

        let mut add_new_var = || {
            let var = variables.concrete();
            tracker.add(
                var.clone(),
                |a, b, positive| {
                    clauses.push(NotBothClause { a, b, positive });
                },
                || variables.variable(),
            );
            var
        };

        for _ in 0..7 {
            add_new_var();
        }

        insta::assert_snapshot!(clauses.iter().format("\n"));
    }

    /// Brute-force check that the emitted clauses encode *exactly* at-most-one:
    /// for every assignment of the `n` concrete variables there must exist an
    /// assignment of the commander variables satisfying all clauses **iff** at
    /// most one concrete variable is true.
    #[test]
    fn enforces_at_most_one() {
        for n in 1..=8usize {
            let mut clauses: Vec<NotBothClause> = Vec::new();
            let mut variables = Variables::new();
            let mut tracker = AtMostOnceTracker::default();
            let mut concrete: Vec<Variable> = Vec::new();
            let mut helpers: Vec<Variable> = Vec::new();
            for _ in 0..n {
                let var = variables.concrete();
                concrete.push(var.clone());
                tracker.add(
                    var,
                    |a, b, positive| clauses.push(NotBothClause { a, b, positive }),
                    || {
                        let v = variables.variable();
                        helpers.push(v.clone());
                        v
                    },
                );
            }

            // Resolve a variable to (is_helper, index) within its array.
            let locate = |v: &Variable| -> (bool, usize) {
                match v {
                    Variable::Concrete(_) => (false, concrete.iter().position(|c| c == v).unwrap()),
                    Variable::Variable(_) => (true, helpers.iter().position(|h| h == v).unwrap()),
                }
            };

            for assignment in 0..(1u32 << n) {
                let satisfiable = (0..(1u64 << helpers.len())).any(|helper_bits| {
                    clauses.iter().all(|c| {
                        let val = |v: &Variable| match locate(v) {
                            (false, p) => (assignment >> p) & 1 == 1,
                            (true, p) => (helper_bits >> p) & 1 == 1,
                        };
                        // (¬a ∨ b) when positive, else (¬a ∨ ¬b).
                        !val(&c.a) || (val(&c.b) == c.positive)
                    })
                });
                let expected = assignment.count_ones() <= 1;
                assert_eq!(
                    satisfiable, expected,
                    "n={n} assignment={assignment:b}: at-most-one should be {expected}"
                );
            }
        }
    }
}
