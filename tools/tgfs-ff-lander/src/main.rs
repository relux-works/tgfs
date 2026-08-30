//! Exec-only entry point for the fixed repository landing lane.

fn main() {
    std::process::exit(tgfs_ff_lander::production_main(std::env::args_os()));
}
