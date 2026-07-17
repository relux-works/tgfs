#!/bin/zsh
# Q4: isolate target-dir state from checkout path at the MAIN checkout.
#
# Q1 varied the path and got one hash (110b1b9a) at both. The reviewer got a
# different hash (bab48d50) at the main checkout. So the variable is not the
# path. The remaining difference is the main checkout's target/: 547 dep
# artifacts, an incremental/ dir, plus a stale rlib/dylib from earlier plain
# builds.
#
#   Q4a: main checkout, EXISTING target, `cargo clean -p` only  -> reproduce reviewer's bab48d50?
#   Q4b: main checkout, FRESH target dir                        -> 110b1b9a or bab48d50?
#
# If Q4b == 110b1b9a, the path was never the variable; a polluted target dir was.

set -u
REPO=/Users/iv/Developer/ReluxWorks/tgfs
TRIPLE=aarch64-apple-darwin
LIB=target/$TRIPLE/release/libgramdrive_ffi.a
LOG=$REPO/.temp/TASK-260715-3akqs8/repro-q4.log

exec > >(tee -a $LOG) 2>&1
echo "=== Q4 start $(date -u +%FT%TZ) ==="

FLAGS="--remap-path-prefix=$REPO=/gramdrive --remap-path-prefix=$HOME/.cargo=/cargo"

echo "--- incremental dir present in main release target?"
ls -d $REPO/target/$TRIPLE/release/incremental 2>/dev/null && echo "  YES" || echo "  no"

# --- Q4a: existing target, clean -p only (what --check-reproducible does today)
echo "--- Q4a: main checkout, existing target, cargo clean -p"
( cd $REPO && \
  RUSTFLAGS="$FLAGS" MACOSX_DEPLOYMENT_TARGET=14.0 LC_ALL=C \
  cargo clean -p gramdrive-ffi >/dev/null 2>&1
  cd $REPO && \
  RUSTFLAGS="$FLAGS" MACOSX_DEPLOYMENT_TARGET=14.0 LC_ALL=C \
  cargo rustc -p gramdrive-ffi --release --target $TRIPLE --crate-type staticlib >/dev/null 2>&1 )
SHA_4A=$(shasum -a 256 $REPO/$LIB | cut -d' ' -f1)
SIZE_4A=$(stat -f%z $REPO/$LIB)
echo "  Q4a: $SHA_4A  ($SIZE_4A B)"

# --- Q4b: same path, fresh target dir (inside repo, so same remap prefix)
echo "--- Q4b: main checkout, FRESH target dir"
FRESH=$REPO/target-repro-q4
rm -rf $FRESH
( cd $REPO && \
  CARGO_TARGET_DIR=$FRESH \
  RUSTFLAGS="$FLAGS" MACOSX_DEPLOYMENT_TARGET=14.0 LC_ALL=C \
  cargo rustc -p gramdrive-ffi --release --target $TRIPLE --crate-type staticlib >/dev/null 2>&1 )
SHA_4B=$(shasum -a 256 $FRESH/$TRIPLE/release/libgramdrive_ffi.a | cut -d' ' -f1)
SIZE_4B=$(stat -f%z $FRESH/$TRIPLE/release/libgramdrive_ffi.a)
echo "  Q4b: $SHA_4B  ($SIZE_4B B)"

echo "=== Q4 summary ==="
echo "  reviewer main-checkout value : bab48d50...  (7920568 B)"
echo "  clean builds at 3 paths (Q1/Q2): 110b1b9a... (7920584 B)"
echo "  Q4a existing target          : $SHA_4A ($SIZE_4A B)"
echo "  Q4b fresh target, same path  : $SHA_4B ($SIZE_4B B)"
if [[ "$SHA_4B" == "$SHA_4A" ]]; then
  echo "  => target state does NOT explain it"
else
  echo "  => target state DOES explain it: same path, different target state, different bytes"
fi
rm -rf $FRESH
echo "=== Q4 end $(date -u +%FT%TZ) ==="
