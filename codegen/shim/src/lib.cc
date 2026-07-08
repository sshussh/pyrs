// PyRs LLVM shim: textual IR in, native object file out.
//
// Kept deliberately thin: all language knowledge lives on the Rust side;
// this file only drives LLVM (parse -> verify -> optimize -> emit).

#include "lib.hh"

#include "llvm/IR/LLVMContext.h"
#include "llvm/IR/LegacyPassManager.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/Verifier.h"
#include "llvm/IRReader/IRReader.h"
#include "llvm/MC/TargetRegistry.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/SourceMgr.h"
#include "llvm/Support/TargetSelect.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Target/TargetMachine.h"
#include "llvm/Target/TargetOptions.h"
#include "llvm/TargetParser/Host.h"
#include "llvm/TargetParser/Triple.h"

#include <cstring>
#include <memory>
#include <string>

namespace {

void set_error(char *buf, size_t len, const std::string &msg) {
    if (buf == nullptr || len == 0) {
        return;
    }
    std::strncpy(buf, msg.c_str(), len - 1);
    buf[len - 1] = '\0';
}

llvm::OptimizationLevel to_opt_level(int level) {
    switch (level) {
    case 0:
        return llvm::OptimizationLevel::O0;
    case 1:
        return llvm::OptimizationLevel::O1;
    case 3:
        return llvm::OptimizationLevel::O3;
    default:
        return llvm::OptimizationLevel::O2;
    }
}

} // namespace

extern "C" {

int pyrs_compile_ir(const uint8_t *ir_data, size_t ir_len,
                    const char *out_path, int opt_level, char *err_buf,
                    size_t err_buf_len) {
    llvm::InitializeNativeTarget();
    llvm::InitializeNativeTargetAsmPrinter();
    llvm::InitializeNativeTargetAsmParser();

    llvm::LLVMContext ctx;

    // parse the textual IR
    llvm::SMDiagnostic diag;
    // getMemBufferCopy null-terminates: the LL lexer requires it
    auto buffer = llvm::MemoryBuffer::getMemBufferCopy(
        llvm::StringRef(reinterpret_cast<const char *>(ir_data), ir_len),
        "pyrs-module");
    std::unique_ptr<llvm::Module> module =
        llvm::parseIR(buffer->getMemBufferRef(), diag, ctx);
    if (!module) {
        set_error(err_buf, err_buf_len,
                  "IR parse error at line " + std::to_string(diag.getLineNo()) +
                      ": " + diag.getMessage().str());
        return 1;
    }

    // configure the native target
    std::string triple_str = llvm::sys::getDefaultTargetTriple();
    llvm::Triple triple(triple_str);
    std::string lookup_error;
    const llvm::Target *target =
        llvm::TargetRegistry::lookupTarget(triple_str, lookup_error);
    if (target == nullptr) {
        set_error(err_buf, err_buf_len, "target lookup failed: " + lookup_error);
        return 2;
    }

    llvm::TargetOptions options;
    std::unique_ptr<llvm::TargetMachine> tm(target->createTargetMachine(
        triple, "generic", "", options, llvm::Reloc::PIC_));
    if (!tm) {
        set_error(err_buf, err_buf_len, "could not create target machine");
        return 2;
    }

    module->setTargetTriple(triple);
    module->setDataLayout(tm->createDataLayout());

    // verify before optimizing so emitter bugs surface with a message
    std::string verify_error;
    llvm::raw_string_ostream verify_stream(verify_error);
    if (llvm::verifyModule(*module, &verify_stream)) {
        set_error(err_buf, err_buf_len,
                  "internal error: generated invalid IR: " + verify_stream.str());
        return 3;
    }

    // standard optimization pipeline (new pass manager)
    llvm::LoopAnalysisManager lam;
    llvm::FunctionAnalysisManager fam;
    llvm::CGSCCAnalysisManager cgam;
    llvm::ModuleAnalysisManager mam;
    llvm::PassBuilder pb(tm.get());
    pb.registerModuleAnalyses(mam);
    pb.registerCGSCCAnalyses(cgam);
    pb.registerFunctionAnalyses(fam);
    pb.registerLoopAnalyses(lam);
    pb.crossRegisterProxies(lam, fam, cgam, mam);

    llvm::OptimizationLevel level = to_opt_level(opt_level);
    llvm::ModulePassManager mpm =
        (opt_level == 0) ? pb.buildO0DefaultPipeline(level)
                         : pb.buildPerModuleDefaultPipeline(level);
    mpm.run(*module, mam);

    // emit the object file
    std::error_code ec;
    llvm::raw_fd_ostream dest(out_path, ec, llvm::sys::fs::OF_None);
    if (ec) {
        set_error(err_buf, err_buf_len,
                  std::string("cannot open output file: ") + ec.message());
        return 4;
    }

    llvm::legacy::PassManager emit_pm;
    if (tm->addPassesToEmitFile(emit_pm, dest, nullptr,
                                llvm::CodeGenFileType::ObjectFile)) {
        set_error(err_buf, err_buf_len,
                  "target cannot emit object files");
        return 5;
    }
    emit_pm.run(*module);
    dest.flush();

    return 0;
}
}
