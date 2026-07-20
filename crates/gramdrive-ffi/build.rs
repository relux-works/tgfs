//! Env-gated `real_tdjson` cfg, mirroring `gramdrive-source-tdjson/build.rs`.
//!
//! The tdjson source crate compiles its real FFI module — and emits the
//! actual link directives — under the same `GRAMDRIVE_TDLIB_ARTIFACT_DIR`
//! gate. This crate only needs the *cfg* so the exported auth surface knows
//! whether a real runtime can exist in this build: with the variable unset
//! (every hermetic gate run) `auth::AuthSession` truthfully reports
//! `SourceUnavailable`; with it set, the session claims the process's real
//! tdjson runtime. No link flags are emitted here — linkage is owned by the
//! source crate's build script, exactly once.

// cargo's build-script protocol is stdout lines, so the workspace
// print_stdout ban cannot apply to this target.
#![allow(clippy::print_stdout)]

const ARTIFACT_ENV: &str = "GRAMDRIVE_TDLIB_ARTIFACT_DIR";

fn main() {
    // Declared unconditionally so `unexpected_cfgs` accepts the cfg in
    // mock-only builds too.
    println!("cargo::rustc-check-cfg=cfg(real_tdjson)");
    println!("cargo::rerun-if-env-changed={ARTIFACT_ENV}");

    if let Some(artifact_dir) = std::env::var_os(ARTIFACT_ENV) {
        println!("cargo::rustc-cfg=real_tdjson");
        // This crate's own binaries (the pinned uniffi-bindgen, run by the
        // packaging pipeline against the tdjson-linked staticlib) must find
        // libtdjson.dylib at run time; link-arg rpaths do not propagate
        // across crates, so the bins' rpath is emitted here.
        let lib_dir = std::path::PathBuf::from(artifact_dir).join("lib");
        println!(
            "cargo::rustc-link-arg-bins=-Wl,-rpath,{}",
            lib_dir.to_string_lossy()
        );
    }
}
