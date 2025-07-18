use std::fmt::Display;

use itertools::Itertools;

use crate::internal::id::ConditionId;
use crate::{ConditionalRequirement, Interner, NameId, StringId, VersionSetId, VersionSetUnionId};

/// Specifies the dependency of a solvable on a set of version sets.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Requirement {
    /// Specifies a dependency on a single version set.
    Single(VersionSetId),
    /// Specifies a dependency on the union (logical OR) of multiple version
    /// sets. A solvable belonging to _any_ of the version sets contained in
    /// the union satisfies the requirement. This variant is typically used
    /// for requirements that can be satisfied by two or more version sets
    /// belonging to _different_ packages.
    Union(VersionSetUnionId),
    /// Specifies a dependency on an extra (optional dependency group).
    /// This represents a request for a specific package with a specific extra,
    /// e.g., "numpy[mkl]" where we want numpy with the mkl extra.
    Extra {
        /// The base package name we want
        base_package: NameId,
        /// The name of the extra we want (interned string)
        extra_name: StringId,
        /// Version constraint for the base package
        version_constraint: VersionSetId,
    },
}

impl Requirement {
    /// Constructs a `ConditionalRequirement` from this `Requirement` and a
    /// condition.
    pub fn with_condition(self, condition: ConditionId) -> ConditionalRequirement {
        ConditionalRequirement {
            condition: Some(condition),
            requirement: self,
        }
    }
}

impl Default for Requirement {
    fn default() -> Self {
        Self::Single(Default::default())
    }
}

impl From<VersionSetId> for Requirement {
    fn from(value: VersionSetId) -> Self {
        Requirement::Single(value)
    }
}

impl From<VersionSetUnionId> for Requirement {
    fn from(value: VersionSetUnionId) -> Self {
        Requirement::Union(value)
    }
}

// Note: No From impl for Extra since it requires multiple parameters

impl Requirement {
    /// Returns an object that implements `Display` for the requirement.
    pub fn display<'i>(&'i self, interner: &'i impl Interner) -> impl Display + 'i {
        DisplayRequirement {
            interner,
            requirement: self,
        }
    }

    pub(crate) fn version_sets<'i>(
        &'i self,
        interner: &'i impl Interner,
    ) -> Box<dyn Iterator<Item = VersionSetId> + 'i> {
        match *self {
            Requirement::Single(version_set) => Box::new(std::iter::once(version_set)),
            Requirement::Union(version_set_union) => {
                Box::new(interner.version_sets_in_union(version_set_union))
            }
            Requirement::Extra {
                version_constraint, ..
            } => {
                // For extras, return the version constraint for the base package
                Box::new(std::iter::once(version_constraint))
            }
        }
    }
}

pub(crate) struct DisplayRequirement<'i, I: Interner> {
    interner: &'i I,
    requirement: &'i Requirement,
}

impl<I: Interner> Display for DisplayRequirement<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self.requirement {
            Requirement::Single(version_set) => write!(
                f,
                "{} {}",
                self.interner
                    .display_name(self.interner.version_set_name(version_set)),
                self.interner.display_version_set(version_set)
            ),
            Requirement::Union(version_set_union) => {
                let formatted_version_sets = self
                    .interner
                    .version_sets_in_union(version_set_union)
                    .format_with(" | ", |version_set, f| {
                        f(&format_args!(
                            "{} {}",
                            self.interner
                                .display_name(self.interner.version_set_name(version_set)),
                            self.interner.display_version_set(version_set)
                        ))
                    });

                write!(f, "{}", formatted_version_sets)
            }
            Requirement::Extra {
                base_package,
                extra_name,
                version_constraint,
            } => {
                write!(
                    f,
                    "{}[{}] {}",
                    self.interner.display_name(base_package),
                    self.interner.display_string(extra_name),
                    self.interner.display_version_set(version_constraint)
                )
            }
        }
    }
}
