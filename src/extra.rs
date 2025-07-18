//! Support for package extras (optional dependency groups)

use crate::ConditionalRequirement;
use crate::internal::id::SolvableId;

/// Represents an extra (optional dependency group) for a specific solvable.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Extra {
    /// The name of the extra (e.g., "dev", "test", "docs")
    pub name: String,
    /// The specific solvable this extra belongs to
    pub base_solvable: SolvableId,
    /// Dependencies that are included when this extra is selected
    pub dependencies: Vec<ConditionalRequirement>,
}

impl Extra {
    /// Create a new extra
    pub fn new(
        name: String,
        base_solvable: SolvableId,
        dependencies: Vec<ConditionalRequirement>,
    ) -> Self {
        Self {
            name,
            base_solvable,
            dependencies,
        }
    }
}
