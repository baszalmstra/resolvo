use crate::internal::id::{ClauseId, VariableId};

/// The reason why a variable was assigned a particular value during propagation.
///
/// This is used for conflict analysis - when a conflict is detected, we trace back
/// through the decisions to understand what caused the conflict.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum PropagationReason {
    /// The decision was derived from a clause in the clause database.
    Clause(ClauseId),
    /// The decision was derived from the at-most-one constraint.
    /// When a solvable is set to TRUE, all other solvables of the same package
    /// must be FALSE. The stored VariableId is the solvable that was set to TRUE
    /// (the "cause"), and the Decision's variable is the one being set to FALSE.
    DirectForbid(VariableId),
}

impl From<ClauseId> for PropagationReason {
    fn from(clause_id: ClauseId) -> Self {
        PropagationReason::Clause(clause_id)
    }
}

/// Represents an assignment to a variable
#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) struct Decision {
    pub(crate) variable: VariableId,
    pub(crate) value: bool,
    pub(crate) derived_from: PropagationReason,
}

impl Decision {
    pub(crate) fn new(variable: VariableId, value: bool, derived_from: impl Into<PropagationReason>) -> Self {
        Self {
            variable,
            value,
            derived_from: derived_from.into(),
        }
    }
}
