cmake_minimum_required(VERSION 3.21)

# Select C++ and C as languages, as Corrosion needs ${CMAKE_C_COMPILER}
# for linking
project(Resolvo LANGUAGES C CXX VERSION 0.1.0)

# Add the Corrosion dependency (used to build Rust code)
include(FetchContent)
FetchContent_Declare(
    Corrosion
    GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
    GIT_TAG v0.4.9
)
FetchContent_MakeAvailable(Corrosion)

# Add the Corrosion CMake module path to the list of paths to search for modules
list(PREPEND CMAKE_MODULE_PATH ${Corrosion_SOURCE_DIR}/cmake)
find_package(Rust 1.73 REQUIRED MODULE)

option(BUILD_SHARED_LIBS "Build Slint as shared library" ON)

set(RESOLVO_LIBRARY_CARGO_FLAGS "" CACHE STRING
    "Flags to pass to cargo when building the Slint runtime library")

if(BUILD_SHARED_LIBS)
    set(rustc_lib_type "cdylib")
    set(resolvo_cpp_impl "resolvo_cpp-shared")
    set(cmake_lib_type "SHARED")
else()
    set(rustc_lib_type "staticlib")
    set(resolvo_cpp_impl "resolvo_cpp-static")
    set(cmake_lib_type "STATIC")
endif()

corrosion_import_crate(MANIFEST_PATH "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
    CRATES resolvo_cpp CRATE_TYPES bin ${rustc_lib_type})