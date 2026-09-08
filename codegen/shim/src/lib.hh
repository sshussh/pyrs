#pragma once

#include <cstddef>
#include <cstdint>

extern "C" {

// Parse LLVM IR text, verify it, optimize at `opt_level` (0-3), and write a
// native object file to `out_path`. Returns 0 on success; on failure writes
// a NUL-terminated message into `err_buf` and returns nonzero.
//
// `cpu` selects the target CPU: NULL or "generic" for the portable baseline,
// "native" for this host's model and features, or a specific model name.
// `opt_level` reaches both the IR pass pipeline and the backend.
int pyrs_compile_ir(const uint8_t *ir_data, size_t ir_len,
                    const char *out_path, int opt_level, const char *cpu,
                    char *err_buf, size_t err_buf_len);

// Write the resolved "<cpu>|<features>" identity for `cpu` into `out`, so the
// caller can put it in a compile cache key. Two hosts that resolve "native"
// differently must not share a cache entry. Returns 0 on success.
int pyrs_target_identity(const char *cpu, char *out, size_t out_len);
}
