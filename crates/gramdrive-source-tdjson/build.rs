//! Env-gated tdjson linkage (the `real_tdjson` cfg).
//!
//! With `GRAMDRIVE_TDLIB_ARTIFACT_DIR` unset — every gate run, every machine
//! that never built the artifact — this script emits only the cfg
//! declaration: no link flags, no `real_tdjson` cfg, and the crate compiles
//! mock-only. With the variable pointing at the staged artifact
//! (`.temp/tdlib/out`, the layout `build_tdlib.py` produces), it turns on
//! `cfg(real_tdjson)` — compiling the `real` module and the real-linkage
//! smoke test — and points the linker at the artifact's `lib/`, baking that
//! directory in as an rpath so the test binary finds `libtdjson.dylib`
//! without `DYLD_LIBRARY_PATH` (which macOS SIP strips across the
//! make → sh → cargo chain anyway).
//!
//! An env gate rather than a cargo feature on purpose: the lint and test
//! gates run `--all-features`, so a feature would enable the linkage exactly
//! where it must stay off (Cargo.toml in this directory, crates/README.md).

// cargo's build-script protocol is stdout lines, so the workspace
// print_stdout ban cannot apply to this target.
#![allow(clippy::print_stdout)]

use std::path::PathBuf;

const ARTIFACT_ENV: &str = "GRAMDRIVE_TDLIB_ARTIFACT_DIR";

fn main() {
    // Declared unconditionally so `unexpected_cfgs` accepts the cfg in
    // mock-only builds too.
    println!("cargo::rustc-check-cfg=cfg(real_tdjson)");
    println!("cargo::rerun-if-env-changed={ARTIFACT_ENV}");

    let Some(artifact_dir) = std::env::var_os(ARTIFACT_ENV) else {
        return; // Mock-only build: no linkage, no real module.
    };

    let lib_dir = PathBuf::from(artifact_dir).join("lib");
    let lib_dir = lib_dir.to_string_lossy();

    println!("cargo::rustc-cfg=real_tdjson");
    println!("cargo::rustc-link-search=native={lib_dir}");
    println!("cargo::rustc-link-lib=dylib=tdjson");
    // rpath so this crate's test binaries find libtdjson.dylib at run time
    // (rustc-link-arg reaches tests/bins of this crate only — downstream
    // consumers own their own rpath story via packaging).
    println!("cargo::rustc-link-arg=-Wl,-rpath,{lib_dir}");
    println!("cargo::rerun-if-changed={lib_dir}/libtdjson.dylib");
}
