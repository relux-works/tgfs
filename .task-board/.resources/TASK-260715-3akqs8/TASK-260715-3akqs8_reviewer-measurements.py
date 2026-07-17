"""Independent reviewer measurement of B1, using the pipeline's own build_env.

X1: main checkout path, fresh target dir           -> reviewer said bab48d50 at this path
X2: main checkout path, fresh target dir, DIFFERENT target-dir path
    (isolates target-dir PATH from target-dir STATE, which repro-q4 confounded)
"""
import subprocess, shutil, sys, hashlib
from pathlib import Path

sys.path.insert(0, str(Path(".scripts/packaging").resolve()))
import build_core_artifacts as pkg

repo = Path.cwd().resolve()
triple = pkg.SLICES[0].triple

def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()

def build(target_dir: Path) -> tuple[str, int]:
    if target_dir.exists():
        shutil.rmtree(target_dir)
    env = pkg.build_env(repo, target_dir)          # exactly what ships
    argv = pkg.cargo_staticlib_argv(triple)        # exactly what ships
    r = subprocess.run(list(argv), cwd=repo, env=env, capture_output=True, text=True)
    if r.returncode != 0:
        print("BUILD FAILED\n", r.stderr[-2000:]); sys.exit(1)
    lib = target_dir / triple / "release" / f"{pkg.LIB_STEM}.a"
    return sha(lib), lib.stat().st_size

base = repo / ".temp" / "review-3akqs8"
x1, s1 = build(base / "t1")
print(f"X1 main checkout, fresh target 't1'                        : {x1}  ({s1} B)")
x2, s2 = build(base / "t2-a-considerably-longer-target-directory")
print(f"X2 main checkout, fresh target 't2-...longer...'           : {x2}  ({s2} B)")
print()
print(f"  target-dir PATH axis (X1 vs X2): {'IDENTICAL' if x1==x2 else 'DIFFER'}")
print(f"  reviewer's main-checkout value : bab48d50... (7920568 B)")
print(f"  check/artifact value           : 110b1b9a... (7920584 B)")
print(f"  X1 matches check value         : {x1.startswith('110b1b9a')}")
for d in (base / "t1", base / "t2-a-considerably-longer-target-directory"):
    shutil.rmtree(d, ignore_errors=True)
"""X3, third attempt: genuinely recompile inside the polluted target dir.

Attempts 1-2 no-op'd -- cargo keeps the real artifact in deps/ and hardlinks
("uplifts") it into release/, so deleting the uplifted copy just re-uplifts.
`cargo clean -p gramdrive-ffi --target <triple>` evicts it from deps/ for real
while leaving the other 546 dep artifacts, incremental/ and the stale rlib+dylib
in place. That is precisely the experiment: polluted dir, gramdrive-ffi rebuilt.
"""
import subprocess, sys, hashlib
from pathlib import Path

sys.path.insert(0, str(Path(".scripts/packaging").resolve()))
import build_core_artifacts as pkg

repo = Path.cwd().resolve()
triple = pkg.SLICES[0].triple
target = repo / "target"
rel = target / triple / "release"
lib = rel / f"{pkg.LIB_STEM}.a"

env = pkg.build_env(repo, target)
c = subprocess.run(["cargo", "clean", "-p", "gramdrive-ffi", "--target", triple,
                    "--release"], cwd=repo, env=env, capture_output=True, text=True)
print(f"clean --target: rc={c.returncode} {c.stderr.strip()[:90]}")
print(f"  .a evicted: {not lib.exists()}")
print(f"  pollution kept -> deps: {len(list((rel/'deps').glob('*')))}, "
      f"incremental: {(rel/'incremental').exists()}, "
      f"rlib: {(rel/f'{pkg.LIB_STEM}.rlib').exists()}")

r = subprocess.run(list(pkg.cargo_staticlib_argv(triple)), cwd=repo, env=env,
                   capture_output=True, text=True)
if r.returncode != 0:
    print("BUILD FAILED\n", r.stderr[-1500:]); sys.exit(1)
compiled = "Compiling" in r.stderr
print(f"\ncargo actually compiled: {compiled}")
print("  " + "; ".join(l.strip() for l in r.stderr.splitlines() if l.strip())[:150])

d = hashlib.sha256(lib.read_bytes()).hexdigest()
print(f"\nX3 polluted dir, REAL rebuild : {d}  ({lib.stat().st_size} B)")
print(f"X1 fresh dir, same path       : 110b1b9a8ecf...  (7920584 B)")
print(f"\n  -> reproduces prior review's bab48d50 : {d.startswith('bab48d50')}")
print(f"  -> equals the clean-build value       : {d.startswith('110b1b9a')}")
