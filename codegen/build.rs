use std::process::Command;

fn main() {
    // build the C++ shim
    let dst = cmake::Config::new("shim")
        .define("CMAKE_BUILD_TYPE", "Release")
        .build();

    // link the shim static archive
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=codegen_shim");

    // LLVM libdir
    let libdir = run("llvm-config", &["--libdir"]);
    println!("cargo:rustc-link-search=native={}", libdir.trim());

    // LLVM component libs (dynamic - static archives not guaranteed on all distros)
    let libs = run(
        "llvm-config",
        &[
            "--libs",
            "core",
            "support",
            "native",
            "analysis",
            "executionengine",
            "instcombine",
            "scalaropts",
            "bitreader",
            "bitwriter",
        ],
    );
    emit_flags(&libs);

    // system libs LLVM depends on (zlib, ncurses, etc.)
    let sys = run("llvm-config", &["--system-libs"]);
    emit_flags(&sys);

    // C++ runtime
    println!("cargo:rustc-link-lib=dylib=stdc++");

    println!("cargo:rerun-if-changed=shim/src/lib.cc");
    println!("cargo:rerun-if-changed=shim/src/lib.hh");
    println!("cargo:rerun-if-changed=shim/CMakeLists.txt");
}

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{cmd}`: {e}"));
    if !out.status.success() {
        panic!(
            "`{cmd} {}` failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    String::from_utf8(out.stdout).unwrap_or_else(|_| panic!("`{cmd}` output was not utf-8"))
}

fn emit_flags(flags: &str) {
    for tok in flags.split_whitespace() {
        if let Some(name) = tok.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        } else if let Some(path) = tok.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
        // silently ignore -Wl,... and anything else llvm-config may emit
    }
}
