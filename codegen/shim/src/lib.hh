#pragma once

#include <cstddef>
#include <cstdint>

extern "C" {

// Parse LLVM IR text, verify it, optimize at `opt_level` (0-3), and write a
// native object file to `out_path`. Returns 0 on success; on failure writes
// a NUL-terminated message into `err_buf` and returns nonzero.
int pyrs_compile_ir(const uint8_t *ir_data, size_t ir_len,
                    const char *out_path, int opt_level, char *err_buf,
                    size_t err_buf_len);
}
