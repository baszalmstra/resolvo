#pragma once

#include "resolvo_internal.h"
#include "resolvo_string.h"
#include "resolvo_vector.h"

namespace resolvo {
    using cbindgen_private::Candidates;
    using cbindgen_private::Dependencies;
    using cbindgen_private::ExcludedSolvable;
    using cbindgen_private::SolvableId;
    using cbindgen_private::VersionSetId;
    using cbindgen_private::NameId;
    using cbindgen_private::StringId;
    using cbindgen_private::Slice;
    
    /**
     * An interface that implements ecosystem specific logic.
     */
    struct DependencyProvider {
        /**
         * Returns a user-friendly string representation of the specified solvable.
         *
         * When formatting the solvable, it should it include both the name of
         * the package and any other identifying properties.
         */
        virtual String display_solvable(SolvableId solvable) = 0;

        /**
         * Returns a user-friendly string representation of the name of the
         * specified solvable.
         */
        virtual String display_solvable_name(SolvableId solvable) = 0;

        /**
         * Returns a string representation of multiple solvables merged together.
         *
         * When formatting the solvables, both the name of the packages and any
         * other identifying properties should be included.
         */
        virtual String display_merged_solvables(Slice<SolvableId> solvable) = 0;

        /**
         * Returns an object that can be used to display the given name in a
         * user-friendly way.
         */
        virtual String display_name(NameId name) = 0;

        /**
         * Returns a user-friendly string representation of the specified version
         * set.
         *
         * The name of the package should *not* be included in the display. Where
         * appropriate, this information is added.
         */
        virtual String display_version_set(VersionSetId version_set) = 0;

        /**
         * Returns the string representation of the specified string.
         */
        virtual String display_string(StringId string) = 0;

        /**
         * Returns the name of the package that the specified version set is
         * associated with.
         */
        virtual NameId version_set_name(VersionSetId version_set_id) = 0;

        /**
         * Returns the name of the package for the given solvable.
         */
        virtual NameId solvable_name(SolvableId solvable_id) = 0;

        /**
         * Obtains a list of solvables that should be considered when a package
         * with the given name is requested.
         */
        virtual Candidates get_candidates(NameId package) = 0;

        /**
         * Sort the specified solvables based on which solvable to try first. The
         * solver will iteratively try to select the highest version. If a
         * conflict is found with the highest version the next version is
         * tried. This continues until a solution is found.
         */
        virtual void sort_candidates(Slice<SolvableId> solvables) = 0;
        
        /**
         * Given a set of solvables, return the candidates that match the given
         * version set or if `inverse` is true, the candidates that do *not* match
         * the version set.
         */
        virtual Vector<SolvableId> filter_candidates(Slice<SolvableId> candidates, VersionSetId version_set_id, bool inverse) = 0;

        /**
         * Returns the dependencies for the specified solvable.
         */
        virtual Dependencies get_dependencies(SolvableId solvable) = 0;
    };
}
