#pragma once

#include "resolvo_internal.h"
#include "resolvo_dependency_provider.h"

namespace resolvo {
    namespace private_api {
        extern "C" void bridge_display_solvable(void *data, SolvableId solvable, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_solvable(solvable);
        }
        extern "C" void bridge_display_solvable_name(void *data, SolvableId solvable, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_solvable_name(solvable);
        }
        extern "C" void bridge_display_merged_solvables(void *data, Slice<SolvableId> solvable, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_merged_solvables(solvable);
        }
        extern "C" void bridge_display_name(void *data, NameId name, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_name(name);
        }
        extern "C" void bridge_display_version_set(void *data, VersionSetId version_set, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_version_set(version_set);
        }
        extern "C" void bridge_display_string(void *data, StringId string, String* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->display_string(string);
        }

        extern "C" NameId bridge_version_set_name(void *data, VersionSetId version_set_id) {
            return reinterpret_cast<DependencyProvider*>(data)->version_set_name(version_set_id);
        }
        extern "C" NameId bridge_solvable_name(void *data, SolvableId solvable_id) {
            return reinterpret_cast<DependencyProvider*>(data)->solvable_name(solvable_id);
        }

        extern "C" void bridge_get_candidates(void *data, NameId package, Candidates *result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->get_candidates(package);
        }
        extern "C" void bridge_sort_candidates(void *data, Slice<SolvableId> solvables) {
            return reinterpret_cast<DependencyProvider*>(data)->sort_candidates(solvables);
        }
        extern "C" void bridge_filter_candidates(void *data,
                                                Slice<SolvableId> candidates,
                                                VersionSetId version_set_id,
                                                bool inverse,
                                                Vector<SolvableId> *result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->filter_candidates(candidates, version_set_id, inverse);
        }
        extern "C" void bridge_get_dependencies(void *data, SolvableId solvable, Dependencies* result) {
            *result = reinterpret_cast<DependencyProvider*>(data)->get_dependencies(solvable);
        }
    }

    /**
     * Called to solve a package problem.
     * 
     * If the solve was successful, an empty string is returned and selected solvable ids will be 
     * stored in `result`. If the solve was unsuccesfull an error describing the reason is returned.
     */
    String solve(DependencyProvider& provider, Slice<VersionSetId> requirements, Slice<VersionSetId> constraints, Vector<SolvableId> &result) {
        cbindgen_private::DependencyProvider bridge {
            static_cast<void*>(&provider),
            private_api::bridge_display_solvable,
            private_api::bridge_display_solvable_name,
            private_api::bridge_display_merged_solvables,
            private_api::bridge_display_name,
            private_api::bridge_display_version_set,
            private_api::bridge_display_string,
            private_api::bridge_version_set_name,
            private_api::bridge_solvable_name,
            private_api::bridge_get_candidates,
            private_api::bridge_sort_candidates,
            private_api::bridge_filter_candidates,
            private_api::bridge_get_dependencies,
        };
        
        String error;
        cbindgen_private::resolvo_solve(&bridge, requirements, constraints, &error, &result);
        return error;
    }
}
