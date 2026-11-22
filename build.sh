#!/bin/bash
# Build recipe of this solver, as used for every build of the paper's runs.
# Run it from this directory:  bash build.sh <output directory>   ->  <output directory>/solver
set -e
set -o pipefail
CARGO=$(command -v cargo) || { echo "cargo not found on PATH"; exit 1; }
TC=$(dirname "$(dirname "$(readlink -f "$CARGO")")")
VEND="$TC/vendor_gdrr"
[ -d "$VEND" ] || { echo "vendored crates not found at $VEND"; exit 1; }
[ -f params.json ] || { echo "params.json is missing"; exit 1; }
python3 -c "import json,sys; json.load(open('params.json'))" || { echo "params.json is not valid JSON"; exit 1; }
if [ -n "$GDRR_TARGET_DIR" ]; then          # a kept build directory (your own builds): the crates are compiled once
  TD="$GDRR_TARGET_DIR"; mkdir -p "$TD"
else                                        # official builds: always from an empty directory
  TD=$(mktemp -d "${TMPDIR:-/tmp}/gdrr_build.XXXXXX")
  trap 'rm -rf "$TD"' EXIT
fi
if [ "$2" = check ]; then                   # compile errors only, no binary
  CARGO_HOME="$TD/home" CARGO_TARGET_DIR="$TD/target" cargo check --release --offline --bin gdrr_main \
    --config 'source.crates-io.replace-with="vendored-sources"' \
    --config "source.vendored-sources.directory=\"$VEND\"" 2>&1 | { grep -v "^ *Compiling \|^ *Checking " || true; } | tail -n 150
  exit 0
fi
CARGO_HOME="$TD/home" CARGO_TARGET_DIR="$TD/target" cargo build --release --offline --bin gdrr_main \
  --config 'source.crates-io.replace-with="vendored-sources"' \
  --config "source.vendored-sources.directory=\"$VEND\"" 2>&1 | { grep -v "^ *Compiling \|^ *Finished " || true; } | tail -n 150
cp "$TD/target/release/gdrr_main" "$1/gdrr_main"
cp params.json "$1/params.json"
cat > "$1/solver" <<'LAUNCHER'
#!/usr/bin/env python3
"""solver <instance> <solution> -t <seconds> -seed <int> [-rot 0|1]: writes the run configuration of the host (time
limit, seed, ONE thread, rotation off unless -rot 1, bins valued by their area, plus the algorithm parameters of
params.json) next to the solution file and runs the binary on it."""
import json
import os
import sys
a = sys.argv[1:]
if len(a) < 2:
    sys.exit("usage: solver <instance> <solution> -t <seconds> -seed <int> [-rot 0|1]")
inst, sol, T, seed, rot = a[0], a[1], 60.0, 0, 0
i = 2
while i + 1 < len(a):
    if a[i] == "-t":
        T = float(a[i + 1])
    elif a[i] == "-seed":
        seed = int(a[i + 1])
    elif a[i] == "-rot":
        rot = int(a[i + 1])
    i += 2
here = os.path.dirname(os.path.abspath(__file__))
cfg = json.load(open(os.path.join(here, "params.json")))
cfg.update(maxRunTime=max(1, int(T)), maxRRIterations=None, nThreads=1, randomSeed=seed, rotationAllowed=bool(rot),
           sheetValuationMode="area")
cfgp = sol + ".config.json"
with open(cfgp, "w") as f:
    json.dump(cfg, f)
os.environ["NO_COLOR"] = "1"
sys.stdout.flush()
os.execv(os.path.join(here, "gdrr_main"), ["gdrr_main", inst, cfgp, sol])
LAUNCHER
chmod +x "$1/solver"
