#!/usr/bin/env bash
# Runs cargo-mutants on the move transaction, recovery and exclusions, with
# the fault-injection build so that the crash tests take part, and leaves its
# report in mutants.out. Mutants run one at a time: tests that measure free
# space on the cache branch must not run concurrently.
#
# With <base>, only the mutants in the diff from <base> to HEAD are tested.
# Missed mutants and timeouts are the report's findings, not failures: the
# script fails only if cargo-mutants could not test at all.
#
# Usage: ci/mutants.sh <shard>/<shards> [<base>]
set -uo pipefail

shard=${1:?usage: $0 <shard>/<shards> [<base>]}
base=${2:-}

args=(
  --file src/transaction.rs
  --file src/recovery.rs
  --file src/exclude.rs
  --features fault-injection
  --shard "$shard"
  --jobs 1
)
if [[ -n $base ]]; then
  diff=$(mktemp)
  git diff "$base"...HEAD > "$diff"
  args+=(--in-diff "$diff")
fi

cargo mutants "${args[@]}"
status=$?
# 2: mutants missed; 3: mutants timed out.
case $status in
  0 | 2 | 3) exit 0 ;;
  *) exit "$status" ;;
esac
