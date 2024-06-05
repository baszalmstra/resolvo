mod slice;
mod string;
mod vector;

use std::ffi::c_void;
use std::ptr::NonNull;

use resolvo::{NameId, SolvableId, StringId, VersionSetId};

use crate::{slice::Slice, string::String, vector::Vector};

#[repr(C)]
pub struct Dependencies {
    /// A pointer to the first element of a list of requirements. Requirements
    /// defines which packages should be installed alongside the depending
    /// package and the constraints applied to the package.
    pub requirements: Vector<VersionSetId>,

    /// Defines additional constraints on packages that may or may not be part
    /// of the solution. Different from `requirements`, packages in this set
    /// are not necessarily included in the solution. Only when one or more
    /// packages list the package in their `requirements` is the
    /// package also added to the solution.
    ///
    /// This is often useful to use for optional dependencies.
    pub constraints: Vector<VersionSetId>,
}

#[repr(C)]
pub struct ExcludedSolvable {
    /// The id of the solvable that is excluded from the solver.
    pub solvable: SolvableId,

    /// A string that provides more information about why the solvable is
    /// excluded (e.g. an error message).
    pub reason: StringId,
}

#[repr(C)]
pub struct Candidates {
    /// A list of all solvables for the package.
    pub candidates: Vector<SolvableId>,

    /// Optionally a pointer to the id of the solvable that is favored over
    /// other solvables. The solver will first attempt to solve for the
    /// specified solvable but will fall back to other candidates if no solution
    /// could be found otherwise.
    ///
    /// The same behavior can be achieved by sorting this candidate to the top
    /// using the [`resolvo::DependencyProvider::sort_candidates`] function but
    /// using this method provides better error messages to the user.
    pub favored: *const SolvableId,

    /// If specified this is the Id of the only solvable that can be selected.
    /// Although it would also be possible to simply return a single
    /// candidate using this field provides better error messages to the
    /// user.
    pub locked: *const SolvableId,

    /// A hint to the solver that the dependencies of some of the solvables are
    /// also directly available. This allows the solver to request the
    /// dependencies of these solvables immediately. Having the dependency
    /// information available might make the solver much faster because it
    /// has more information available up-front which provides the solver with a
    /// more complete picture of the entire problem space. However, it might
    /// also be the case that the solver doesnt actually need this
    /// information to form a solution. In general though, if the
    /// dependencies can easily be provided one should provide them up-front.
    pub hint_dependencies_available: Vector<SolvableId>,

    /// A list of solvables that are available but have been excluded from the
    /// solver. For example, a package might be excluded from the solver
    /// because it is not compatible with the runtime. The solver will not
    /// consider these solvables when forming a solution but will use
    /// them in the error message if no solution could be found.
    pub excluded: Vector<ExcludedSolvable>,
}

/// The dependency provider is a struct that is passed to the solver which
/// implements the ecosystem specific logic to resolve dependencies.
#[repr(C)]
pub struct DependencyProvider {
    /// The data pointer is a pointer that is passed to each of the functions.
    pub data: *mut c_void,

    /// Returns a user-friendly string representation of the specified solvable.
    ///
    /// When formatting the solvable, it should it include both the name of
    /// the package and any other identifying properties.
    pub display_solvable: unsafe extern "C" fn(data: *mut c_void, solvable: SolvableId, result: NonNull<String>),

    /// Returns a user-friendly string representation of the name of the
    /// specified solvable.
    pub display_solvable_name:
        unsafe extern "C" fn(data: *mut c_void, solvable: SolvableId, result: NonNull<String>),

    /// Returns a string representation of multiple solvables merged together.
    ///
    /// When formatting the solvables, both the name of the packages and any
    /// other identifying properties should be included.
    pub display_merged_solvables:
        unsafe extern "C" fn(data: *mut c_void, solvable: Slice<SolvableId>, result: NonNull<String>),

    /// Returns an object that can be used to display the given name in a
    /// user-friendly way.
    pub display_name: unsafe extern "C" fn(data: *mut c_void, name: NameId, result: NonNull<String>),

    /// Returns a user-friendly string representation of the specified version
    /// set.
    ///
    /// The name of the package should *not* be included in the display. Where
    /// appropriate, this information is added.
    pub display_version_set:
        unsafe extern "C" fn(data: *mut c_void, version_set: VersionSetId, result: NonNull<String>),

    /// Returns the string representation of the specified string.
    pub display_string: unsafe extern "C" fn(data: *mut c_void, string: StringId, result: NonNull<String>),

    /// Returns the name of the package that the specified version set is
    /// associated with.
    pub version_set_name:
        unsafe extern "C" fn(data: *mut c_void, version_set_id: VersionSetId) -> NameId,

    /// Returns the name of the package for the given solvable.
    pub solvable_name: unsafe extern "C" fn(data: *mut c_void, solvable_id: SolvableId) -> NameId,

    /// Obtains a list of solvables that should be considered when a package
    /// with the given name is requested.
    pub get_candidates: unsafe extern "C" fn(data: *mut c_void, package: NameId, candidates: NonNull<Candidates>),

    /// Sort the specified solvables based on which solvable to try first. The
    /// solver will iteratively try to select the highest version. If a
    /// conflict is found with the highest version the next version is
    /// tried. This continues until a solution is found.
    pub sort_candidates: unsafe extern "C" fn(data: *mut c_void, solvables: Slice<SolvableId>),

    /// Given a set of solvables, return the candidates that match the given
    /// version set or if `inverse` is true, the candidates that do *not* match
    /// the version set.
    pub filter_candidates: unsafe extern "C" fn(
        data: *mut c_void,
        candidates: Slice<SolvableId>,
        version_set_id: VersionSetId,
        inverse: bool,
        filtered: NonNull<Vector<SolvableId>>
    ),

    /// Returns the dependencies for the specified solvable.
    pub get_dependencies:
        unsafe extern "C" fn(data: *mut c_void, solvable: SolvableId, dependencies: NonNull<Dependencies>),
}

#[no_mangle]
#[allow(unused)]
pub extern "C" fn resolvo_solve(
    provider: &DependencyProvider,
    requirements: Slice<VersionSetId>,
    constraints: Slice<VersionSetId>,
    error: &mut String,
    result: &mut Vector<SolvableId>
) -> bool {
    true
}
