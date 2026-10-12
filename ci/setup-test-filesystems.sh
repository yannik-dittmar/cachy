#!/usr/bin/env bash
# Prepares the filesystems of the privileged test tier below <dir>:
#   zfs-cache/, zfs-backing/  two ZFS pools on loop files, so a cache branch and
#                             a backing branch can both be ZFS (as two pools)
#   ext4/                     an ext4 loop mount
# tmpfs comes from /dev/shm. Each mount is owned by the invoking user, who runs
# the tests. Needs passwordless sudo, ZFS userland and the zfs kernel module.
#
# Usage: ci/setup-test-filesystems.sh <dir>
set -euo pipefail

root=${1:?usage: $0 <dir>}
images=$root/images
mkdir -p "$images" "$root/ext4"

# Test files have repetitive content, so compression is off: writing N bytes
# must take N bytes of free space.
for pool in cache backing; do
  truncate -s 1G "$images/zfs-$pool.img"
  sudo zpool create -f -O compression=off -m "$root/zfs-$pool" \
    "cachy-test-$pool" "$images/zfs-$pool.img"
done

truncate -s 512M "$images/ext4.img"
mkfs.ext4 -q "$images/ext4.img"
sudo mount -o loop "$images/ext4.img" "$root/ext4"

sudo chown "$(id -u):$(id -g)" "$root/zfs-cache" "$root/zfs-backing" "$root/ext4"
findmnt --types zfs,ext4,tmpfs --output TARGET,FSTYPE,SIZE,AVAIL \
  | grep -E "$root|/dev/shm"
