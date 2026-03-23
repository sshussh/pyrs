cmake_minimum_required(VERSION 3.15)
project(codegen_shim LANGUAGES C CXX)

# C++23 - requires clang 16+ or gcc 13+
set(CMAKE_CXX_STANDARD 26)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# locate LLVM for headers and definitions only
find_package(LLVM REQUIRED CONFIG)

include_directories(${LLVM_INCLUDE_DIRS})
add_definitions(${LLVM_DEFINITIONS})

# build the shim as a plain static archive of C++ objects.
# do NOT link LLVM libs here - that is Cargo's responsibility via build.rs.
add_library(codegen_shim STATIC src/lib.cc)

# hide internal symbols to avoid conflicts with Rust
set_target_properties(codegen_shim PROPERTIES CXX_VISIBILITY_PRESET hidden)

install(TARGETS codegen_shim DESTINATION lib)
