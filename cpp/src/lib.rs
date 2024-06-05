mod vector;

use resolvo::{SolvableId, VersionSetId};
use std::ffi::c_void;

#[repr(C)]
pub struct Dependencies {
    /// A pointer to the first element of a list of requirements. Requirements
    /// defines which packages should be installed alongside the depending
    /// package and the constraints applied to the package.
    pub requirements: *const VersionSetId,

    /// The number elements pointed to by `requirements`.
    pub requirements_len: usize,

    /// Defines additional constraints on packages that may or may not be part
    /// of the solution. Different from `requirements`, packages in this set
    /// are not necessarily included in the solution. Only when one or more
    /// packages list the package in their `requirements` is the
    /// package also added to the solution.
    ///
    /// This is often useful to use for optional dependencies.
    pub constraints: *const VersionSetId,

    /// The number of elements pointed to be `constraints_len`.
    pub constraints_len: usize,
}

/// The dependency provider is a struct that is passed to the solver which
/// implements the ecosystem specific logic to resolve dependencies.
#[repr(C)]
pub struct DependencyProvider {
    /// The data pointer is a pointer that is passed to each of the functions.
    pub data: *mut c_void,

    /// Returns the dependencies for the specified solvable.
    pub get_dependencies:
        unsafe extern "C" fn(data: *mut c_void, solvable: SolvableId) -> Dependencies,
}

#[no_mangle]
pub extern "C" fn solve(provider: *const DependencyProvider) {}
