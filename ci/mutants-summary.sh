#!/usr/bin/env bash
# Prints a Markdown summary of a cargo-mutants report: how many mutants the
# tests caught and missed, and the missed ones.
#
# Usage: ci/mutants-summary.sh <mutants.out> <title>
set -euo pipefail

out=${1:?usage: $0 <mutants.out> <title>}
title=${2:?usage: $0 <mutants.out> <title>}

echo "## Mutation testing ($title)"
if [[ ! -d $out ]]; then
  echo
  echo 'No report: no mutants were tested, for example because the diff changes none of the mutated files.'
  exit 0
fi

count() {
  if [[ -f $out/$1.txt ]]; then wc -l < "$out/$1.txt"; else echo 0; fi
}

echo
echo '| caught | missed | timed out | unviable |'
echo '| --- | --- | --- | --- |'
echo "| $(count caught) | $(count missed) | $(count timeout) | $(count unviable) |"
for kind in missed timeout; do
  if [[ -s $out/$kind.txt ]]; then
    echo
    echo "<details><summary>Mutants $kind</summary>"
    echo
    echo '```'
    cat "$out/$kind.txt"
    echo '```'
    echo '</details>'
  fi
done
