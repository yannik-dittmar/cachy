#!/usr/bin/env bash
# Runs <binary> as systemd runs it: installs it with the shipped units and an
# /etc/cachy/cachy.yml, then starts cachy.service and checks that
#   - a run moves files, exits 0, and works in the state directory that
#     systemd creates;
#   - journald records each log line's priority;
#   - systemctl stop mid-copy stops the run cleanly, with exit code 2, and the
#     next run moves everything;
#   - a refusal (exit code 1) marks the unit failed;
#   - cachy-daemon.service warns that cachy.timer is enabled too, and stops
#     cleanly, with exit code 0.
# Replaces any installed cachy and its config. Needs systemd as PID 1, root
# through passwordless sudo and cgroup v2; meant for a disposable CI runner.
#
# Usage: ci/test-systemd-service.sh <binary> <dir>
set -euo pipefail

binary=${1:?usage: $0 <binary> <dir>}
root=${2:?usage: $0 <binary> <dir>}
repo=$(cd "$(dirname "$0")/.." && pwd)
cache=$root/cache
backing=$root/backing
dropin=/run/systemd/system/cachy.service.d

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Marks the end of the journal, before a start of cachy.service.
# (_SYSTEMD_INVOCATION_ID cannot select an invocation: journald can miss it on
# the lines of a process that exits quickly.)
mark() {
  sudo journalctl --sync
  since=$(sudo journalctl --lines=1 --output=export | sed -n 's/^__CURSOR=//p')
}

# The messages of unit $2 (default cachy.service) since the last mark, at
# priority $1 or more urgent.
journal() {
  sudo journalctl --sync
  sudo journalctl --output=cat --priority="$1" --unit="${2:-cachy.service}" --after-cursor="$since"
}

# The exit status of unit $1 (default cachy.service).
exit_status() {
  systemctl show --property=ExecMainStatus --value "${1:-cachy.service}"
}

# Path and SHA-256 of every file below $1, sorted.
checksums() {
  (cd "$1" && sudo find . -type f -not -path './.cachy-staging/*' -exec sha256sum {} + | sort -k 2)
}

on_exit() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- journal of cachy.service" >&2
    sudo journalctl --no-pager --unit=cachy.service >&2 || true
    echo "--- journal of cachy-daemon.service" >&2
    sudo journalctl --no-pager --unit=cachy-daemon.service >&2 || true
  fi
  sudo systemctl stop cachy.service cachy-daemon.service 2>/dev/null || true
  sudo systemctl disable cachy.timer 2>/dev/null || true
  sudo rm -rf "$dropin"
  sudo systemctl daemon-reload
  sudo umount "$cache" 2>/dev/null || true
}
trap on_exit EXIT

echo "--- install"
sudo install -m 755 "$binary" /usr/local/bin/cachy
sudo install -m 644 "$repo/packaging/systemd/cachy.service" "$repo/packaging/systemd/cachy.timer" \
  "$repo/packaging/systemd/cachy-daemon.service" /etc/systemd/system/
systemd-analyze verify /etc/systemd/system/cachy.service /etc/systemd/system/cachy.timer \
  /etc/systemd/system/cachy-daemon.service
mkdir -p "$cache" "$backing"
sudo mount -t tmpfs -o size=1G cachy-test-cache "$cache"
sudo mkdir -p /etc/cachy
sudo tee /etc/cachy/cachy.yml >/dev/null <<EOF
cache_branch: $cache
backing_branches:
  - $backing
threshold: 1T
target: 2T
minimum_age: 0s
# Reading files for their checksums changes their atimes.
order: mtime
# Only for cachy-daemon.service; cachy run ignores it.
daemon:
  schedules:
    - '0 3 * * *'
EOF
# No state_dir in the config: the default, /var/lib/cachy, is the directory
# that StateDirectory= creates.
sudo rm -rf /var/lib/cachy
sudo systemctl daemon-reload

echo "--- a run moves files"
sudo mkdir -p "$cache/a/b"
echo one | sudo tee "$cache/one.txt" >/dev/null
echo two | sudo tee "$cache/a/b/two.txt" >/dev/null
sudo head -c 1M /dev/urandom | sudo tee "$cache/a/three.bin" >/dev/null
before=$(checksums "$cache")
mark
sudo systemctl start cachy.service
[ "$(exit_status)" = 0 ] || fail "exit status $(exit_status), not 0"
[ "$(checksums "$backing")" = "$before" ] || fail "backing branch holds $(checksums "$backing")"
[ -z "$(checksums "$cache")" ] || fail "cache branch still holds $(checksums "$cache")"
[ "$(sudo stat -c '%U %a' /var/lib/cachy)" = "root 700" ] || fail "state directory: $(sudo stat -c '%U %a' /var/lib/cachy)"

echo "--- journald records the priorities"
grep -q '^cachy: info: run finished: 3 moved, 0 skipped, 0 failed' <<<"$(journal info)" \
  || fail "no run summary line at priority info"
if grep -q '^cachy: info: ' <<<"$(journal warning)"; then
  fail "info lines are recorded at priority warning or above"
fi

echo "--- systemctl stop mid-copy"
# Older than later.txt, so it moves first.
sudo head -c 256M /dev/urandom | sudo tee "$cache/big.bin" >/dev/null
echo later | sudo tee "$cache/later.txt" >/dev/null
before=$(checksums "$cache")
# At 1% of a CPU, copying and hashing 256 MiB takes many seconds.
sudo mkdir -p "$dropin"
printf '[Service]\nCPUQuota=1%%\n' | sudo tee "$dropin/slow.conf" >/dev/null
sudo systemctl daemon-reload
mark
sudo systemctl start --no-block cachy.service
copying=
for _ in $(seq 600); do
  # The move's temp file, once it holds more than a probe's scratch file
  # (about 1 MiB).
  if [ -n "$(sudo find "$backing/.cachy-staging" -type f -size +8M 2>/dev/null)" ]; then
    copying=1
    break
  fi
  sleep 0.1
done
[ -n "$copying" ] || fail "the copy of big.bin never started"
sudo systemctl stop cachy.service
[ "$(exit_status)" = 2 ] || fail "exit status $(exit_status), not 2"
grep -q '^cachy: warning: run stopped early: cachy was asked to stop' <<<"$(journal warning)" \
  || fail "no warning that the run stopped early"
[ "$(checksums "$cache")" = "$before" ] || fail "cache branch changed: $(checksums "$cache")"
[ -z "$(sudo find "$backing" -mindepth 1 -name 'big.bin*')" ] || fail "a copy of big.bin is left on the backing branch"
[ ! -e "$backing/.cachy-staging" ] || fail "staging directory left: $(sudo find "$backing/.cachy-staging")"
sudo rm -rf "$dropin"
sudo systemctl daemon-reload
sudo systemctl reset-failed cachy.service
sudo systemctl start cachy.service
[ "$(exit_status)" = 0 ] || fail "exit status $(exit_status) of the run after the stop, not 0"
[ -z "$(checksums "$cache")" ] || fail "cache branch still holds $(checksums "$cache")"
for file in big.bin later.txt; do
  [ "$(cd "$backing" && sudo sha256sum "./$file")" = "$(grep " ./$file\$" <<<"$before")" ] \
    || fail "$file did not arrive intact"
done

echo "--- the daemon warns about the enabled timer and stops cleanly"
sudo systemctl enable cachy.timer
mark
sudo systemctl start cachy-daemon.service
waiting=
for _ in $(seq 100); do
  if grep -q '^cachy: info: next scheduled check at' <<<"$(journal info cachy-daemon.service)"; then
    waiting=1
    break
  fi
  sleep 0.1
done
[ -n "$waiting" ] || fail "the daemon never waited for its scheduled check"
grep -q '^cachy: warning: cachy.timer is enabled' <<<"$(journal warning cachy-daemon.service)" \
  || fail "no warning that cachy.timer is enabled too"
sudo systemctl stop cachy-daemon.service
[ "$(exit_status cachy-daemon.service)" = 0 ] \
  || fail "daemon exit status $(exit_status cachy-daemon.service), not 0"
[ "$(systemctl is-failed cachy-daemon.service)" != failed ] || fail "the stopped daemon's unit failed"
sudo systemctl disable cachy.timer

echo "--- a refusal fails the unit"
echo 'treshold: 5G' | sudo tee -a /etc/cachy/cachy.yml >/dev/null
mark
if sudo systemctl start cachy.service; then
  fail "systemctl start succeeded with an invalid config"
fi
[ "$(exit_status)" = 1 ] || fail "exit status $(exit_status), not 1"
[ "$(systemctl is-failed cachy.service)" = failed ] || fail "unit is $(systemctl is-failed cachy.service), not failed"
grep -q '^cachy: error: .*unknown field `treshold`' <<<"$(journal err)" || fail "no error naming the unknown key"
sudo systemctl reset-failed cachy.service

echo "OK: cachy.service and cachy-daemon.service run cachy as documented"
