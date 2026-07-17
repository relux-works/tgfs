//! Link the staged tdjson artifact into the smoke binary.
//!
//! The artifact directory is the one `build_tdlib.py` stages
//! (`.temp/tdlib/out`): its `lib/` holds `libtdjson.dylib` and its `include/`
//! holds the C headers. The build script points the linker at that `lib/` and
//! bakes it in as an rpath so the dylib is found at run time without the caller
//! having to set `DYLD_LIBRARY_PATH`. OpenSSL/zlib the dylib depends on carry
//! absolute install names (Homebrew, macOS system), so they need no rpath here.

use std::path::PathBuf;

const ARTIFACT_ENV: &str = "GRAMDRIVE_TDLIB_ARTIFACT_DIR";

fn main() {
    println!("cargo:rerun-if-env-changed={ARTIFACT_ENV}");

    let artifact_dir = match std::env::var_os(ARTIFACT_ENV) {
        Some(dir) => PathBuf::from(dir),
        None => {
            // Default to the staged output relative to this crate:
            // <repo>/.scripts/tdlib/link-smoke -> <repo>/.temp/tdlib/out
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest_dir
                .join("../../../.temp/tdlib/out")
                .canonicalize()
                .unwrap_or_else(|_| manifest_dir.join("../../../.temp/tdlib/out"))
        }
    };

    let lib_dir = artifact_dir.join("lib");
    let lib_dir = lib_dir.to_string_lossy();

    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=dylib=tdjson");
    // rpath so the freshly built binary finds libtdjson.dylib without env help.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    println!("cargo:rerun-if-changed={lib_dir}/libtdjson.dylib");
}
