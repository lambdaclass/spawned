#!/usr/bin/env bash
# usage: node.sh <qual_dir> <label> <command...>
# Runs one node in the foreground (normal signal dispositions) under the
# qualification controller: a new log per run, the PID in run/<label>.pid, the
# exit status in run/<label>.exit and both events in results/runs.jsonl.
set -u
D=$1 L=$2
shift 2
mkdir -p "$D/results/logs" "$D/run"
n=$(find "$D/results/logs" -maxdepth 1 -name "$L.*.log" | wc -l)
log="$D/results/logs/$L.$n.log"
rm -f "$D/run/$L.exit"
printf '{"epoch":%s,"event":"start","label":"%s","run":%s,"log":"%s"}\n' \
  "$(date +%s.%N)" "$L" "$n" "$log" >> "$D/results/runs.jsonl"
# The inner shell records its PID, then execs the node, so the PID file names
# the node itself (stop and the sampler check /proc/<pid>/exe against bin/).
bash -c 'echo $$ > "$0"; exec "$@"' "$D/run/$L.pid" "$@" > "$log" 2>&1
rc=$?
printf '{"epoch":%s,"event":"exit","label":"%s","run":%s,"rc":%s}\n' \
  "$(date +%s.%N)" "$L" "$n" "$rc" >> "$D/results/runs.jsonl"
echo "$(date +%s.%N) $rc" > "$D/run/$L.exit"
