//! Standing coverage dashboard for the universal property gate.
//!
//! A handful of solver code paths are exercised only (or primarily) by the
//! generated corpus of `universal_prop.rs` and by the targeted universal
//! scenario tests. Each such path increments a named hit counter through
//! [`prop_hit!`]; the property gate dumps the counters at the end of its run
//! and asserts floors for the paths its generator is responsible for, so
//! generator drift cannot silently zero out coverage.
//!
//! The counters exist only under `cfg(test)`; in non-test builds the
//! [`prop_hit!`] macro expands to nothing, so instrumented production code
//! carries no runtime or dependency cost.
//!
//! Note that the test binary runs tests concurrently, so counters observed
//! from one test may include increments from other tests running in
//! parallel. Assertions on them must therefore be monotone ("at least this
//! many new hits"), which is what every current user asserts.

/// Increments the named hit counter in test builds; expands to nothing
/// otherwise. Usable from any solver module.
macro_rules! prop_hit {
    ($name:ident) => {{
        #[cfg(test)]
        {
            crate::solver::prop_counters::hits::$name
                .fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
        }
    }};
}

pub(crate) use prop_hit;

#[cfg(test)]
pub(crate) mod hits {
    use std::sync::atomic::AtomicU64;

    macro_rules! declare_counters {
        ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
            $($(#[$doc])* pub static $name: AtomicU64 = AtomicU64::new(0);)+

            /// All counters with their names, for the dashboard dump.
            pub(crate) static ALL: &[(&str, &AtomicU64)] = &[
                $((stringify!($name), &$name),)+
            ];
        };
    }

    declare_counters! {
        /// `extract_cell`: a requires clause of an installed parent was
        /// satisfied by a concrete condition complement literal
        /// (`satisfied_by_concrete`).
        EXTRACT_SATISFIED_BY_CONCRETE,
        /// Encoder: a condition disjunction emitted a non-empty concrete
        /// complement (`DisjunctionComplement::Solvables`).
        ENCODE_CONDITION_COMPLEMENT_SOLVABLES,
        /// Encoder: a condition disjunction emitted an empty concrete
        /// complement (`DisjunctionComplement::Empty`, the at-least-one
        /// tracker encoding).
        ENCODE_CONDITION_COMPLEMENT_EMPTY,
        /// Encoder: a condition disjunction emitted an environment literal
        /// complement (`DisjunctionComplement::EnvLiteral`).
        ENCODE_CONDITION_COMPLEMENT_ENV,
        /// Encoder: a `Requirement::Union` over concrete version sets was
        /// encoded.
        ENCODE_UNION_CONCRETE,
        /// Encoder: a `Requirement::Union` over environment version sets was
        /// encoded.
        ENCODE_UNION_ENV,
        /// Encoder: a constraint of the root problem (a `Problem`/
        /// `UniversalProblem` `constraints()` entry) was queued.
        ENCODE_ROOT_CONSTRAINT,
        /// Encoder: lock clauses were added for a package with a locked
        /// candidate.
        ENCODE_LOCKED,
        /// Encoder: an exclusion clause was added for an externally excluded
        /// candidate.
        ENCODE_EXCLUDED,
        /// Encoder: a solvable reported `Dependencies::Unknown` and was
        /// excluded.
        ENCODE_UNKNOWN_DEPS,
        /// Cache: a favored candidate was moved to the front of the sorted
        /// candidates.
        CACHE_FAVORED_SORTED,
        /// `solve_universal`: a trail-reuse enumeration exceeded its work
        /// budget and the fallback re-enumeration (reuse disabled) ran.
        UNIVERSAL_REUSE_ABANDONED,
        /// `propagate`: the kept-prefix work budget was exhausted and the
        /// run aborted with `PrefixBudgetExhausted`.
        PREFIX_BUDGET_ABORT,
        /// `propagate`: a free universal enumeration episode exceeded the
        /// witness-probe budget and aborted with `WitnessProbeTripped`.
        WITNESS_PROBE_TRIP,
        /// Universal enumeration: a tripped free episode found an uncovered
        /// environment witness and escalated to a witness-directed
        /// (assumption) solve of that region.
        WITNESS_PROBE_ESCALATED,
        /// Universal enumeration: a tripped free episode found NO witness
        /// (coverage complete) and terminated the enumeration, replacing
        /// the remainder of the final refutation.
        WITNESS_PROBE_COVERAGE_BREAK,
        /// Universal enumeration: a witness-directed solve proved its
        /// region unsolvable and ended the whole solve with the verdict
        /// cell and scoped conflict.
        WITNESS_PROBE_VERDICT,
        /// Oracle consistency encoding: the relation oracle answered
        /// `VersionSetRelation::Equal` for two distinct version set ids and
        /// both implication clauses were emitted.
        ORACLE_EQUAL_CLAUSES,
        /// Universal enumeration: a cell-to-cell retraction would pop more
        /// than `TRAIL_RESHAPE_ORDINARY_LEVELS` ordinary levels and was
        /// widened to a full retraction (trail reshape).
        TRAIL_RESHAPE_FULL_RETRACT,
        /// Seeded `solve_universal`: the reseed iteration closed an orbit (an
        /// output equal to an earlier input) without finding a fixed point.
        RESEED_ORBIT_CLOSED,
        /// `propagate`: the fallback-replay deadline was exhausted and the
        /// bounded internal seed replay aborted with
        /// `FallbackReplayBudgetExhausted`.
        FALLBACK_REPLAY_ABORT,
        /// `solve_universal`: a bounded internal replay was abandoned by its
        /// actual-work deadline and the enumeration restarted from the
        /// historical baseline (attempt 3).
        UNIVERSAL_REPLAY_ABANDONED,
        /// `solve_universal`: the replay-prefix selection truncated the
        /// abandoned attempt's recorded cells (either cap).
        UNIVERSAL_REPLAY_TRUNCATED,
        /// `solve_universal`: the replay-prefix selection selected no cell at
        /// all and the fallback retried directly with the original caller
        /// seed partition.
        UNIVERSAL_REPLAY_EMPTY_SELECTION,
    }

    /// Formats the dashboard: one `name = value` line per counter.
    pub(crate) fn dump() -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (name, counter) in ALL {
            writeln!(
                out,
                "  {name} = {}",
                counter.load(std::sync::atomic::Ordering::Relaxed)
            )
            .unwrap();
        }
        out
    }

    /// Reads one counter (helper for before/after deltas in tests).
    pub(crate) fn get(counter: &AtomicU64) -> u64 {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }
}
