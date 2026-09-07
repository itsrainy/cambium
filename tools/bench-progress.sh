#!/bin/sh
# Live view of a `cambi bench` run driven by the lane scripts.
#
#   tools/bench-progress.sh [LOG_DIR]        one-shot summary
#   watch -n 10 tools/bench-progress.sh      keep it on screen
#
# LOG_DIR defaults to the job's tmp dir; it reads laneA2/laneB2 (subjects
# build, history, change-set selection) and cmpA/cmpB (bench changes) logs,
# plus bench-results/*.
D="${1:-/Users/rainy/.claude/jobs/6b58f265/tmp}"
R="$(cd "$(dirname "$0")/.." && pwd)/bench-results"
strip() { sed 's/\x1b\[[0-9;]*m//g'; }

echo "== $(date '+%H:%M:%S')   lanes: A = bazel   B = envoy   C = sourcegraph apollo"
for lane in A B C; do
  L="$D/lane${lane}2.log"; C="$D/cmp${lane}.log"; [ "$lane" = C ] && { L="$D/laneC.log"; C="$D/laneC.log"; }
  [ -f "$L" ] || continue
  echo "-- lane $lane"
  { [ "$L" = "$C" ] && grep -hE '^===' "$L" || grep -hE '^===' "$L" "$C" 2>/dev/null; } | tail -3 | cut -c1-90
  step=$(grep -E 'applied step=' "$L" | tail -1 | strip | sed -E 's/.*step=([0-9]+) of=([0-9]+).*dirty=([0-9]+) ms=([0-9]+).*/history \1\/\2  (last: dirty=\3 \4ms)/')
  [ -n "$step" ] && echo "   $step"
  cs=$(grep -E 'comparing change_set=' "$C" 2>/dev/null | tail -1 | strip | sed -E 's/.*change_set=([0-9]+) of=([0-9]+).*kind="([a-z]+)".*/changes \1\/\2 (\3)/')
  [ -n "$cs" ] && echo "   $cs"
  grep -hE 'history done|min recall|disagreements:' "$L" "$C" 2>/dev/null | strip | tail -4 | sed 's/^/   /' | cut -c1-160
  grep -hE '^cambium: |panicked' "$L" "$C" 2>/dev/null | tail -2 | sed 's/^   !! /   /; s/^/   !! /' | cut -c1-160
done

# bench-results/<subject>/state.json is the resume record, and each workload's
# files sit under bench-results/<subject>/<workload>/. There is one tree: a
# short run and a published one differ only in the sizes recorded inside the
# result files, so the counts below are of files, not of modes.
echo "-- results on disk"
for d in "$R"/*/; do
  [ -d "$d" ] || continue
  id=$(basename "$d")
  hist=$(ls "$d/history" 2>/dev/null | grep -c '^history-')
  chg=$(ls "$d/changes" 2>/dev/null | grep -c '^changes-')
  st=$(python3 -c "import json;d=json.load(open('$d/state.json'));r=d.get('history') or {};print('history ' + ('done' if r.get('done') else 'partial'), 'compared', len(d.get('compared',[])))" 2>/dev/null)
  printf '   %-14s history files %s  changes files %s  %s\n' "$id" "$hist" "$chg" "$st"
done

echo "-- processes"
ps -axo pid,etime,rss,command | grep -E 'cambi|bazel-diff-rust|tango-cli|bazel\(' | grep -v grep | awk '{printf "   %s %8s %6dMB %s %s %s %s\n",$1,$2,$3/1024,$4,$5,$6,$7}' | cut -c1-110
