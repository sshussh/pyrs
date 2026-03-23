#pragma once

#include <cstddef>
#include <cstdint>

extern "C" {
int run_llvm_test(const uint8_t *data, size_t len);
}
