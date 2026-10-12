#!/usr/bin/env bash
# Runs the integration tests once per pair of cache-branch and backing-branch
# filesystems, on the filesystems that ci/setup-test-filesystems.sh prepared
# below <dir>. Every filesystem takes both roles; ZFS to ZFS goes from one zpool
# to the other. Extra arguments are passed to `cargo test`; test output is shown
# so that skipped tests are visible.
#
# Usage: ci/test-filesystem-pairs.sh <dir> [cargo test args...]
set -uo pipefail

root=${1:?usage: $0 <dir> [cargo test args...]}
shift

declare -A cache_dir=([tmpfs]=/dev/shm [ext4]=$root/ext4 [zfs]=$root/zfs-cache)
declare -A backing_dir=([tmpfs]=/dev/shm [ext4]=$root/ext4 [zfs]=$root/zfs-backing)
pairs=(tmpfs:ext4 tmpfs:zfs ext4:tmpfs ext4:zfs zfs:tmpfs zfs:ext4 zfs:zfs)

export CACHY_TEST_PRIVILEGED=1
failed=()
for pair in "${pairs[@]}"; do
  cache_fs=${pair%:*} backing_fs=${pair#*:}
  echo "::group::cache branch on $cache_fs, backing branch on $backing_fs"
  if ! CACHY_TEST_CACHE_BRANCH_FS="$cache_fs:${cache_dir[$cache_fs]}" \
    CACHY_TEST_BACKING_BRANCH_FS="$backing_fs:${backing_dir[$backing_fs]}" \
    cargo test "$@" -- --show-output; then
    failed+=("$cache_fs -> $backing_fs")
  fi
  echo "::endgroup::"
done

if ((${#failed[@]})); then
  (IFS=,; echo "::error::tests failed with cache branch -> backing branch on: ${failed[*]}")
  exit 1
fi
