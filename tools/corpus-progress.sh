#!/bin/sh
# Live view of a corpus run driven by the lane scripts.
#
#   tools/corpus-progress.sh [LOG_DIR]        one-shot summary
#   watch -n 10 tools/corpus-progress.sh      keep it on screen
#
# LOG_DIR defaults to the job's tmp dir; it reads laneA2/laneB2 (build,
# replay, pairs) and cmpA/cmpB (compare) logs, plus corpus-results/*.
D="${1:-/home/user/.claude/jobs/6b58f265/tmp}"
R="$(cd "$(dirname "$0")/.." && pwd)/corpus-results"
strip() { sed 's/\x1b\[[0-9;]*m//g'; }

echo "== $(date '+%H:%M:%S')   lanes: A = bazel bazel-8 bazel-7   B = envoy envoy-7   C = sourcegraph apollo"
for lane in A B C; do
  L="$D/lane${lane}2.log"; C="$D/cmp${lane}.log"; [ "$lane" = C ] && { L="$D/laneC.log"; C="$D/laneC.log"; }
  [ -f "$L" ] || continue
  echo "-- lane $lane"
  { [ "$L" = "$C" ] && grep -hE '^===' "$L" || grep -hE '^===' "$L" "$C" 2>/dev/null; } | tail -3 | cut -c1-90
  step=$(grep -E 'replayed step=' "$L" | tail -1 | strip | sed -E 's/.*step=([0-9]+) of=([0-9]+).*dirty=([0-9]+) ms=([0-9]+).*/replay \1\/\2  (last: dirty=\3 \4ms)/')
  [ -n "$step" ] && echo "   $step"
  pair=$(grep -E 'comparing pair=' "$C" 2>/dev/null | tail -1 | strip | sed -E 's/.*pair=([0-9]+) of=([0-9]+).*kind="([a-z]+)".*/compare \1\/\2 (\3)/')
  [ -n "$pair" ] && echo "   $pair"
  grep -hE 'replay done|min recall|disagreements:' "$L" "$C" 2>/dev/null | strip | tail -4 | sed 's/^/   /' | cut -c1-160
  grep -hE '^cambium: |panicked' "$L" "$C" 2>/dev/null | tail -2 | sed 's/^   !! /   /; s/^/   !! /' | cut -c1-160
done

echo "-- results on disk"
for d in "$R"/*/; do
  id=$(basename "$d")
  rep=$(ls "$d" 2>/dev/null | grep -c '^replay-'); cmp=$(ls "$d" 2>/dev/null | grep -c '^compare-')
  st=$(python3 -c "import json,sys;d=json.load(open('$d/state.json'));r=d.get('replay') or {};print('replay ' + ('done' if r.get('done') else 'partial'), 'compared', len(d.get('compared',[])))" 2>/dev/null)
  printf '   %-9s replay files %s  compare files %s  %s\n' "$id" "$rep" "$cmp" "$st"
done

echo "-- processes"
ps -axo pid,etime,rss,command | grep -E 'cambi-run|bazel-diff-rust|tango-cli|bazel\(' | grep -v grep | awk '{printf "   %s %8s %6dMB %s %s %s %s\n",$1,$2,$3/1024,$4,$5,$6,$7}' | cut -c1-110
