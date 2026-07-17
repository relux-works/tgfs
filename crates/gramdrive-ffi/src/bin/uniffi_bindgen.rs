//! Workspace-local `uniffi-bindgen`, version-locked to the `uniffi` crate
//! the library links (a skewed generator produces bindings that fail the
//! runtime contract-version check). Built only with `--features bindgen`;
//! see `.scripts/smoke/run_bindings_smoke.py` and README.md for usage.

fn main() {
    uniffi::uniffi_bindgen_main();
}
