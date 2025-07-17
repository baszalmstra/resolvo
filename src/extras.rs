//! Support for package extras (optional dependency groups)
//!
//! This module provides first-class support for package extras, similar to Python's
//! optional dependencies. Extras allow packages to define optional dependency groups
//! that can be selectively installed.

use crate::internal::id::{ExtraId, NameId, SolvableId, VersionSetId};
use crate::{ConditionalRequirement, Requirement};

/// Represents an extra (optional dependency group) for a package.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Extra {
    /// The name of the extra (e.g., "dev", "test", "docs")
    pub name: String,
    /// The base package this extra belongs to
    pub base_package: NameId,
    /// Dependencies that are included when this extra is selected
    pub dependencies: Vec<ConditionalRequirement>,
}

/// A requirement for a specific extra of a package.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExtraRequirement {
    /// The base package name
    pub base_package: NameId,
    /// The name of the extra
    pub extra_name: String,
    /// Version constraints for the base package
    pub version_set: VersionSetId,
}

/// Configuration for handling missing extras
#[derive(Debug, Clone, Default)]
pub enum MissingExtraStrategy {
    /// Ignore missing extras (create empty extras)
    #[default]
    Ignore,
    /// Warn when an extra is missing
    Warn,
    /// Fail when an extra is missing
    Fail,
}

/// Extension trait for KnownDependencies to support extras
pub trait KnownDependenciesExt {
    /// Add an extra requirement to the dependencies
    fn add_extra(&mut self, extra_req: ExtraRequirement);
    
    /// Add multiple extra requirements
    fn add_extras(&mut self, extra_reqs: impl IntoIterator<Item = ExtraRequirement>);
    
    /// Check if this dependency set includes any extras
    fn has_extras(&self) -> bool;
    
    /// Get all extra requirements
    fn get_extra_requirements(&self) -> Vec<&ExtraRequirement>;
}

impl KnownDependenciesExt for crate::KnownDependencies {
    fn add_extra(&mut self, extra_req: ExtraRequirement) {
        // Convert ExtraRequirement to ConditionalRequirement
        self.requirements.push(ConditionalRequirement {
            condition: None,
            requirement: Requirement::Extra(extra_req),
        });
    }
    
    fn add_extras(&mut self, extra_reqs: impl IntoIterator<Item = ExtraRequirement>) {
        for extra_req in extra_reqs {
            self.add_extra(extra_req);
        }
    }
    
    fn has_extras(&self) -> bool {
        self.requirements.iter().any(|req| {
            matches!(req.requirement, Requirement::Extra(_))
        })
    }
    
    fn get_extra_requirements(&self) -> Vec<&ExtraRequirement> {
        self.requirements.iter().filter_map(|req| {
            if let Requirement::Extra(ref extra_req) = req.requirement {
                Some(extra_req)
            } else {
                None
            }
        }).collect()
    }
}

/// Builder for creating packages with extras
pub struct PackageWithExtras {
    pub name: NameId,
    pub base_dependencies: Vec<ConditionalRequirement>,
    pub extras: Vec<Extra>,
}

impl PackageWithExtras {
    /// Create a new package with extras
    pub fn new(name: NameId) -> Self {
        Self {
            name,
            base_dependencies: Vec::new(),
            extras: Vec::new(),
        }
    }
    
    /// Add a base dependency to this package
    pub fn add_dependency(mut self, requirement: ConditionalRequirement) -> Self {
        self.base_dependencies.push(requirement);
        self
    }
    
    /// Add an extra to this package
    pub fn add_extra(mut self, extra_name: String, dependencies: Vec<ConditionalRequirement>) -> Self {
        self.extras.push(Extra {
            name: extra_name,
            base_package: self.name,
            dependencies,
        });
        self
    }
    
    /// Build the package definition
    pub fn build(self) -> (NameId, Vec<ConditionalRequirement>, Vec<Extra>) {
        (self.name, self.base_dependencies, self.extras)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::id::NameId;
    
    #[test]
    fn test_package_with_extras_builder() {
        let name = NameId::new(0);
        let package = PackageWithExtras::new(name)
            .add_extra("dev".to_string(), vec![])
            .add_extra("test".to_string(), vec![]);
        
        let (pkg_name, _deps, extras) = package.build();
        assert_eq!(pkg_name, name);
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].name, "dev");
        assert_eq!(extras[1].name, "test");
    }
}