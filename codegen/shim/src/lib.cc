#include "lib.hh"

#include "llvm/IR/LLVMContext.h"

#include <iostream>

extern "C" {
int run_llvm_test(const uint8_t* data, size_t len) {
    std::cout << "C++: Received " << len << " bytes.\n";

    auto ctx = std::make_unique<llvm::LLVMContext>();
    if (ctx) {
        std::cout << "C++: LLVM Context created successfully.\n";
    }

    return 0;
}
}
