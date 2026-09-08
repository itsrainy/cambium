#!/bin/bash
# Check an engine change against the real corpus, offline, in minutes.
#
#   tools/bench/precheck.sh                      # every cached subject
#   SUBJECTS="envoy apollo" tools/bench/precheck.sh
#   SAVE_BASELINE=1 tools/bench/precheck.sh      # record today's answers
#   BASELINE=1 tools/bench/precheck.sh           # gate against that record
#
# It pulls, per subject, the stored index at the pin and the recorded results
# from the benchmark box into `bench-precheck/` (git-ignored), builds cambi in
# release, and runs `cambi bench replay --all`, which exits 1 on divergence.
#
# What it does on the box: `scp` of result files, and — only with
# PATHS_FROM_BOX=1, which is the default because the change sets do not record
# their own diffs — a read-only `git diff --name-status` per change set against
# the mirrors. Never Bazel, never a write. A subject already under
# bench-precheck/ is not pulled again; delete its directory to refresh it.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CACHE=${CACHE:-$ROOT/bench-precheck}
PEM=${PEM:-$HOME/.ssh/cambium-corpus.pem}
R=${AWS_REGION:-us-east-2}
BOX_REPO=${BOX_REPO:-cambium}
BOX_SUBJECTS=${BOX_SUBJECTS:-.cache/cambium/corpus}
PATHS_FROM_BOX=${PATHS_FROM_BOX:-1}
SUBJECTS=${SUBJECTS:-"bazel sourcegraph apollo envoy xla"}
# `--repo <dir>` per subject, when a local clone exists: it settles the change
# sets a replay otherwise marks indeterminate (a MODULE.bazel it cannot read).
LOCAL_MIRRORS=${LOCAL_MIRRORS:-${CAMBIUM_SUBJECTS_DIR:-$HOME/.cache/cambium/subjects}}

host() {
  aws ec2 describe-instances --region "$R" \
    --filters Name=tag:cambium,Values=bench,corpus \
      "Name=instance-state-name,Values=running" \
    --query 'Reservations[0].Instances[0].PublicDnsName' --output text 2>/dev/null \
    | grep -v None
}

pin_of() {  # subject -> the manifest pin
  awk -v id="$1" '
    $1=="[[repos]]" { inrepo=1; found=0 }
    inrepo && $1=="id" { gsub(/[",]/,"",$3); found=($3==id) }
    inrepo && found && $1=="commit" { gsub(/[",]/,"",$3); print $3; exit }
  ' "$ROOT/subject_repos.toml"
}

pull() {  # subject
  local s=$1 dir=$CACHE/$s
  if [ -d "$dir" ]; then echo "  cached: $dir"; return 0; fi
  local h; h=$(host)
  if [ -z "$h" ]; then
    echo "  no running benchmark box and nothing cached for $s; skipping" >&2
    return 1
  fi
  mkdir -p "$dir/changes" "$dir/history"
  echo "  pulling $s from $h"
  # Index + results only. Read-only, and small: the largest index in the last
  # published run was envoy's at 7.3 MB.
  scp -q -i "$PEM" -o StrictHostKeyChecking=accept-new \
    "ubuntu@$h:$BOX_REPO/bench-results/$s/index-*.bin" "$dir/" || { rm -rf "$dir"; return 1; }
  scp -q -i "$PEM" "ubuntu@$h:$BOX_REPO/bench-results/$s/changes/*.json" "$dir/changes/" \
    || { rm -rf "$dir"; return 1; }
  scp -q -i "$PEM" "ubuntu@$h:$BOX_REPO/bench-results/$s/history/*.json" "$dir/history/" 2>/dev/null
  [ "$PATHS_FROM_BOX" = 1 ] && paths_from_box "$s" "$h" "$dir"
  return 0
}

# The recorded change sets carry (base, head) and no diff — it was taken from
# the mirror at run time and never written down. This asks the mirror for it,
# once, with git and nothing else.
paths_from_box() {  # subject host dir
  local s=$1 h=$2 dir=$3 pin
  pin=$(pin_of "$s")
  local remote='
set -eu
S='"$s"'; PIN='"$pin"'
M=$HOME/'"$BOX_SUBJECTS"'/$S.git
F=$(ls $HOME/'"$BOX_REPO"'/bench-results/$S/changes/change-sets-*.json | head -1)
python3 - "$M" "$F" "$S" "$PIN" <<'"'"'PY'"'"'
import json, subprocess, sys
mirror, f, subject, pin = sys.argv[1:5]
sets = json.load(open(f))["change_sets"]
out = []
for cs in sets:
    try:
        d = subprocess.run(["git","-C",mirror,"diff","--name-status","-M",
                            cs["base"], cs["head"]],
                           capture_output=True, text=True, check=True).stdout
    except subprocess.CalledProcessError:
        continue
    out.append({"base": cs["base"], "head": cs["head"],
                "changes": [l for l in d.splitlines() if l.strip()]})
print(json.dumps({"schema":1,"repo":subject,"pin":pin,"entries":out}))
PY'
  if ssh -i "$PEM" "ubuntu@$h" "$remote" > "$dir/paths-$pin.json.tmp" 2>/dev/null \
     && [ -s "$dir/paths-$pin.json.tmp" ]; then
    # `cambi bench replay` looks for paths-<pin>[-<version>].json beside the
    # index; apollo's files carry a Bazel-version suffix, so cover both names.
    for f in "$dir"/index-*.bin; do
      base=$(basename "$f" .bin); mv -f "$dir/paths-$pin.json.tmp" "$dir/${base/index-/paths-}.json"
      cp -f "$dir/${base/index-/paths-}.json" "$dir/paths-$pin.json"
    done
    rm -f "$dir/paths-$pin.json.tmp"
  else
    rm -f "$dir/paths-$pin.json.tmp"
    echo "  could not derive the change-set diffs on the box; pass --repo by hand" >&2
  fi
}

echo "=== building cambi (release) ==="
cargo build --release --manifest-path "$ROOT/Cargo.toml" || exit 1
CAMBI=$ROOT/target/release/cambi

fail=0
for s in $SUBJECTS; do
  echo "=== $s ==="
  pull "$s" || { fail=1; continue; }
  args=("$s" --results-dir "$CACHE" --all)
  mirror=$LOCAL_MIRRORS/$s.git
  [ -d "$mirror" ] && args+=(--repo "$mirror")
  [ "${BASELINE:-0}" = 1 ] && args+=(--baseline "$CACHE/$s/baseline.json")
  [ "${SAVE_BASELINE:-0}" = 1 ] && args+=(--save-baseline "$CACHE/$s/baseline.json")
  "$CAMBI" bench replay "${args[@]}" || fail=1
done

if [ "$fail" = 0 ]; then echo "=== PRECHECK CLEAN ==="; else echo "=== PRECHECK FOUND DIVERGENCE ==="; fi
exit "$fail"
