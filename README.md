# cachy

Moves files from a [mergerfs](https://github.com/trapexit/mergerfs) cache branch to the backing pool once the cache runs low on free space, without moving files that are still in use.

cachy is heavily inspired by [mergerfs-cache-mover](https://github.com/monstermuffin/mergerfs-cache-mover) by MonsterMuffin. It is a new implementation in Rust with its own configuration and design, not a drop-in port.

## Status

Early development. Do not use it on real data yet. `cachy run` moves files, skips files in use, recovers from a crash in the middle of a move, and preserves owner, group, mode, xattrs, ACLs and timestamps. `cachy daemon` performs checks on schedules, and while the backing disks are awake.

## How it works

A move fails, keeping the source, if the backing branch cannot store the source's xattrs or ACLs. To accept losing them instead, with a warning, list the kinds in the configuration:

```yaml
allow_metadata_loss: [xattrs, acls]
```

Owner, group, mode and mtime are never optional. Directories that cachy creates on a backing branch take the owner, mode, xattrs and ACLs of the cache branch's directory, and its mtime at the end of the run; existing backing directories are left as they are.

Copies keep the holes of sparse files, so moving a sparse file takes up only the space of its data on the backing branch. If the cache branch cannot be trusted to report holes, such as under OpenZFS 2.2.0 and 2.2.1, which can report data as holes, cachy copies every byte instead and warns. While copying, cachy hashes each file's content (XXH3-128) and logs the hash of every file it moves. To also read each copy back once it is fsynced, and fail the move, keeping the source, unless the hash matches, set:

```yaml
verify: readback
```

cachy asks the kernel to drop the copy from the page cache before reading it back, so the read comes from storage; ZFS serves it from its own cache (ARC) instead, if the copy is still there.

To keep files and directories on the cache branch, list gitignore patterns under `exclude`. They follow the usual `.gitignore` rules, as if the file sat at the cache branch root: `*.part` matches at any depth, and `/appdata/` only the top-level `appdata` directory. Excluded directories are not walked at all. [`cachy.example.yml`](cachy.example.yml) documents every setting, with a summary of the pattern rules and SnapRAID examples.

cachy moves regular files of any size, including empty ones, hardlink groups and symlinks, and never moves FIFOs, sockets or device nodes; it skips them as special files. A symlink is moved as itself, never followed: it arrives on the backing branch with its link text verbatim (relative, absolute, dangling or pointing at a directory) and keeps its owner and timestamps (on ZFS, a crash within seconds of the move can lose these; see [ADR 0002](docs/adr/0002-publish-via-staging-directory.md)). Like a file, it is never published over an existing file on the backing branch. The names of a file with several hard links move together, as one inode with all its links, so the move frees its space; if the walk does not find all of its names (one is excluded, say), cachy skips it as a partly excluded hardlink group and lists the names it found. It does not walk into other filesystems mounted inside the cache branch.

A run moves files in the configured order (least recently accessed first, by default) until it has moved as many bytes as it needs to reach the target, measured once at its start. If free space then grew by less than half the bytes moved, for example because a snapshot of the cache branch still holds the files, cachy warns; so may writes to the cache branch during the run. Before it warns, cachy waits up to 15 seconds for the space to show as free, because ZFS shows freed space only once its transaction group syncs. The threshold and target may be byte sizes or percentages of the cache branch's size.

With a `max_age` (such as `7d`), files unchanged for longer than that are moved even while free space is above the threshold, and a run moves them before any other file, beyond its byte budget if need be. A check then walks the cache branch to find them; without a max age, it only reads the cache branch's free space. `cachy run --max-age <duration>` overrides the configured max age.

cachy writes to backing branches directly, never through mergerfs ([ADR 0001](docs/adr/0001-write-to-backing-branches-directly.md)), so it needs to know the layout. Point `cache_pool` at the cache pool's mount, and cachy reads the branches, their modes and `minfreespace` from mergerfs's runtime interface. It refuses to run unless the cache branch is the cache pool's first branch. If that interface is unavailable (`xattr=nosys`), or there is no cache pool to read, list the backing branches under `backing_branches` instead. A move goes to a branch that is neither `RO` nor `NC`, keeps its `minfreespace` free and has room for the whole file, preferring a branch that already has the file's directory, then the one with the most free space. Each backing branch takes one move at a time, and different branches move in parallel.

`cachy run --drain` skips the check and moves every file it may move. `cachy run --dry-run` writes nothing but the capability probe's scratch files: it prints the byte budget, the files a run would move, in order, and every file it would skip, with the reason. Combine both to preview a drain. To tell which files are in use, a dry run briefly takes the same lease on each file that a move would, so a writer opening a file at that moment waits until it is released (with `open_file_guard: proc`, it scans `/proc` instead).

Only one invocation at a time works on a cache branch: before anything else, each one takes a lock on it in the state directory (`state_dir`, `/var/lib/cachy` by default), and refuses to run (exit 1) while another holds it. Two cache branches can be drained at once, each by its own invocation.

After a run, cachy removes the cache-branch directories that its moves emptied, and then their parents while those are empty in turn. It keeps directories that were already empty before the run, and excluded ones. For a path-preserving create policy, which creates new files only where their directory already exists, set `keep_empty_dirs: true` to keep every directory.

On `SIGTERM` (as `systemctl stop` sends it) or `SIGINT`, cachy starts no further move, aborts the moves that are still copying, removing their copies, and lets those already published finish, which takes about a second. It then exits with code 2, as for a run that stopped early. Whatever it moved stays moved, and the files it did not get to stay on the cache branch, untouched.

After taking the lock, every invocation probes the cache branch and each backing branch that takes new files, on scratch files in their staging directories, for what cachy's guarantees depend on: on the cache branch, the in-use guard (leases of the configured `lease_type`, or, with `open_file_guard: proc`, a scan of `/proc` that finds a scratch file's opens), no-replace renames and hole reporting; on backing branches, no-replace and exchange renames (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`). cachy refuses to run (exit 1) if a filesystem lacks a lease, `/proc` scan or rename, naming the filesystem and what it lacks, so that a kernel or module upgrade cannot silently weaken it. A filesystem without room for the scratch files is not probed, with a warning, so that a full cache branch still drains. On a ZFS cache branch, cachy warns unless `order` is set: ZFS's `atime=off` does not always show as a mount option, so cachy cannot tell whether atimes change.

cachy detects files in use with kernel file leases. One race remains that leases cannot close: an application that looked up a file's path just before cachy retired it, but stalled for more than a second before opening it, opens the retired copy, and its writes are lost.

Where the cache branch has no leases, cachy refuses to run unless `open_file_guard` explicitly chooses another in-use guard; it never falls back on its own:

- `proc` looks through every process's open descriptors in `/proc` for the file, by device and inode, when a move starts, right before it publishes and before it deletes the retired source. Unlike a lease, it does not make a writer wait, so a writer that opens a file mid-move is noticed only at the next scan, which rolls the move back; one that opens and writes it between scans is noticed by the change. The race above remains as it is with leases. Recovery cannot rule out writers of a retired source without a lease, so it puts every one back on the cache branch, and the next move of it adopts the identical copy. cachy sees only the processes in its PID namespace whose descriptors it may read: run it as root (or with `CAP_SYS_PTRACE`) in the host's PID namespace. It warns when it cannot read some.
- `off` moves files whether they are in use or not, and warns on every run. Writes to a file during its move can be lost.

Under either, the minimum age still applies. Each run's last line names the in-use guard it used.

After each run, cachy executes the configured run hook, if there is one, and hands it the run summary on stdin: a JSON document that lists the moved, skipped and failed move units (files, hardlink groups and symlinks), with reason codes, the stale copies that moves replaced or removed, the free space before and after, the byte budget, the throughput and the duration. [`docs/run-summary.md`](docs/run-summary.md) documents its schema. An `on` filter limits the hook to runs with some outcomes, such as failed ones; a hook that fails, or that runs past its timeout (60 seconds by default) and is killed, is logged without changing the exit code.

cachy logs to stderr, one line per event, each starting with `cachy: ` and its level: `error`, `warning` or `info`. Lines are never coloured. When systemd connects its stderr to the journal, cachy also gives each line its syslog priority, which journald records, so `journalctl -p warning` shows just the warnings and errors. `cachy --version` prints the version.

## Installation

CI builds a statically linked x86_64 binary on every push to `main` and every pull request; download the `cachy-x86_64-unknown-linux-musl` artifact of a run from the repository's Actions page, which holds the binary `cachy`. It has no runtime dependencies. To build it yourself:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
# The binary is target/x86_64-unknown-linux-musl/release/cachy.
```

Then install the binary, a configuration and the systemd units from [`packaging/systemd`](packaging/systemd):

```sh
sudo install -m 755 cachy /usr/local/bin/cachy
sudo install -D -m 644 cachy.example.yml /etc/cachy/cachy.yml
sudo install -m 644 packaging/systemd/cachy.service packaging/systemd/cachy.timer packaging/systemd/cachy-daemon.service /etc/systemd/system/
sudoedit /etc/cachy/cachy.yml
sudo /usr/local/bin/cachy run --config /etc/cachy/cachy.yml --dry-run
sudo systemctl daemon-reload
sudo systemctl enable --now cachy.timer
```

`cachy.service` runs `cachy run --config /etc/cachy/cachy.yml` once, as root, and has systemd create the state directory `/var/lib/cachy` (`StateDirectory=cachy`), the default `state_dir`. `cachy.timer` starts it every hour, and catches up on a run missed while the machine was off; a check that finds nothing to do only reads the cache branch's free space, unless a `max_age` makes it walk the cache branch. To run it more often, or only after the cache branch and pools are mounted, override the units with `systemctl edit cachy.timer` or `systemctl edit cachy.service` (such as `RequiresMountsFor=/mnt/cache /mnt/pool /mnt/storage` under `[Unit]`). cachy's log goes to the journal: `journalctl -u cachy`. Exit codes 1 and 2 mark the service failed, so `OnFailure=` catches refusals and failed moves alike.

## Daemon

The timer is the default, and all that most setups need. `cachy daemon` is the alternative: a long-running service that performs a check at each time of the schedules in the config's `daemon` section, and a run whenever a check finds work, just as `cachy run` does. Use it instead of the timer when one config file should hold every schedule, such as a frequent check by day and a nightly one. `cachy-daemon.service` runs it, and is not enabled on install. To switch from the timer to the daemon, set `daemon.schedules` in the config, then:

```sh
sudo systemctl disable --now cachy.timer
sudo systemctl enable --now cachy-daemon.service
```

Schedules are cron expressions of five fields (minute, hour, day of month, month, day of week, with `*`, lists, ranges and steps), or nicknames such as `@hourly` and `@daily`, in the system's time zone. An invalid one is a configuration error. The daemon's checks take the instance lock like any other invocation: a scheduled check that finds a run in progress, such as a manual `cachy run`, is skipped with a warning, and a manual run during a scheduled run refuses to run. If `cachy.timer` is enabled too, the daemon warns at its start. On `SIGTERM` (`systemctl stop`), the daemon stops a run in progress as `cachy run` does and exits. Run summaries of its scheduled runs give the reason `schedule`. Its log goes to the journal: `journalctl -u cachy-daemon`.

### Backing disks and the wake gate

To keep a nightly check from waking the backing disks, list them in `daemon.backing_disks`, by their stable IDs in `/dev/disk/by-id/`, and set a `daemon.wake_gate`. The backing disks are awake only while every one of them is spun up. Each scheduled check then waits at the wake gate until they are, looking every 30 seconds, and goes on as soon as they are. When the gate's `time_limit` passes first, the check goes on only if the cache branch's free space is below the `threshold`, so that cache pressure still wins over sleeping disks; otherwise it is skipped, and the log says why.

cachy reads a disk's state with ATA CHECK POWER MODE, which does not wake the disk and which needs `CAP_SYS_RAWIO` (the shipped service runs as root). A disk that does not answer counts as asleep. Where the command cannot get through, such as behind a USB bridge without ATA pass-through, cachy warns once and judges that disk by its I/O counters in `/sys/block/<disk>/stat` instead: it is spun up if it did I/O since cachy last looked at it, or more recently than its `spindown_timeout`, the time the disk stays spun up after its last I/O (set it to the disk's own standby timer, such as the one `hdparm -S` sets). With a `spindown_timeout`, I/O since a look longer ago than the timeout does not count, as the disk may have spun down since. The first look has nothing to compare the counters with, so it counts the disk as spun up only if it has I/O in flight. An ID that is not in `/dev/disk/by-id/`, or listed twice, is a configuration error; one that names no disk keeps the daemon from starting.

### Wake trigger

With a `daemon.wake_trigger`, the daemon also looks at the backing disks every `check_interval` (5 minutes unless set), and performs a check whenever they are awake, so that the cache branch drains while the backing disks spin anyway. A wake-triggered check is an ordinary check, with the normal `threshold`, `target` and `max_age`: it starts a run only if it finds work, and its run summary gives the reason `wake_trigger`. The trigger needs `daemon.backing_disks`, and works with or without schedules; a scheduled check stands in for the look that comes due with it. An invalid `check_interval`, or one of `0s`, is a configuration error.

cachy's own I/O on the backing disks, such as a check's probe or a run's moves, is no sign that they are in use, so the trigger cannot keep itself firing, nor the disks spun up. After each of the daemon's checks that reaches the backing disks, a disk counts as spun up to the trigger only once its I/O counters in `/sys/block/<disk>/stat` show I/O of others. As a filesystem can write cachy's changes back after the check is over, the changes in the minute after the check still count as cachy's (up to the next look, if the `check_interval` is shorter); I/O of others in that minute goes unseen. While others keep the disks in use, wake-triggered checks therefore come every `check_interval`. The wake gate is not affected: to it, a disk that cachy keeps spun up is awake.

## Configuration

cachy reads one YAML file, given with `--config`; [`cachy.example.yml`](cachy.example.yml) explains every key, with examples. Byte sizes take a `K`, `M`, `G` or `T` suffix (binary: `1G` is 1024³ bytes), and durations an `s`, `m`, `h` or `d` suffix. Unknown keys are refused.

| Key | Default | Meaning |
| --- | --- | --- |
| `cache_branch` | required | The cache branch: the fast branch that cachy drains, listed first in the cache pool. |
| `state_dir` | `/var/lib/cachy` | Where the instance lock of each cache branch lives. |
| `cache_pool` | none | The cache pool's mount, whose mergerfs runtime interface lists the backing branches. |
| `backing_branches` | none | The backing branches, as `<path>[=<mode>[,<minfreespace>]]` with mode `RW`, `RO` or `NC`, for when the cache pool's runtime interface is unavailable or there is no `cache_pool`. One of the two is required. |
| `threshold` | required | A check runs when the cache branch's free space is below this byte size or percentage of its size. |
| `target` | required | A run moves files until free space reaches this; must be above `threshold`. |
| `order` | `atime` | Which files move first: `atime` (least recently accessed) or `mtime` (least recently modified). |
| `minimum_age` | `15m` | Files changed more recently than this, by the later of their mtime and ctime, are never moved. |
| `max_age` | none | Files unchanged for longer than this move on every run, whatever the free space. Must be above `minimum_age`. |
| `open_file_guard` | `lease` | How cachy tells that a file is in use: `lease`, `proc` or `off`. |
| `lease_type` | `read` | What counts as in use: `read` for a file open for writing; `write` for a file open at all, even for reading only. |
| `allow_metadata_loss` | `[]` | Metadata a move may lose, with a warning, when the backing branch cannot store it: `xattrs`, `acls`. |
| `verify` | `hash` | `hash` hashes the source while copying; `readback` also reads the copy back from storage and compares. |
| `exclude` | `[]` | Gitignore patterns for files and directories that stay on the cache branch. |
| `keep_empty_dirs` | `false` | Keep the cache-branch directories that a run empties, rather than remove them. |
| `run_hook` | none | A command to execute after each run, with the run summary on stdin. Its keys: `command` (required, executed by `/bin/sh -c`), `on` (the outcomes `moved`, `nothing_moved` and `failed` that call it; default all three) and `timeout` (default `60s`). |
| `daemon` | none | What `cachy daemon` does; `cachy run` ignores it. Its keys: `schedules` (the cron expressions of the times of its checks; see [Daemon](#daemon)), `backing_disks` (the disks under the backing pool, each with an `id` in `/dev/disk/by-id/` and an optional `spindown_timeout`) `wake_gate` (with a `time_limit`, it holds each scheduled check until the backing disks are awake; needs `backing_disks`; see [Backing disks and the wake gate](#backing-disks-and-the-wake-gate)) and `wake_trigger` (with a `check_interval`, `5m` if unset, it performs a check every check interval while the backing disks are awake; needs `backing_disks`; see [Wake trigger](#wake-trigger)). |

### Cache pool requirements

- The cache branch must be the cache pool's **first** branch, so that applications see its copy of a file over a stale copy on a backing branch. With a readable `cache_pool`, cachy refuses to run otherwise.
- cachy writes moved files to the backing branches directly, never through the cache pool. The cache pool's create policy decides where applications create files: on the cache branch while it has room, as with `category.create=ff`.
- Set `moveonenospc=false` on the cache pool: otherwise, a write to a backing branch that runs out of space can move the file back onto the cache branch. cachy warns when it is enabled.
- The `threshold` should not be below the cache branch's `minfreespace`, or mergerfs stops creating files on the cache branch before cachy starts a run. cachy warns if it is.
- Snapshots of the cache branch keep the space of the files that cachy moves; cachy warns when a run frees much less than it moved.

### Privileges

Run cachy as root, as the shipped service does. The in-use guard takes kernel file leases (`fcntl(F_SETLEASE)`), which need leases enabled (`fs.leases-enable = 1`, the default) and the file's owner or `CAP_LEASE`. When cachy runs as another user, or in a container, it needs `CAP_LEASE` for files owned by other users (in Docker, `cap_add: [LEASE]`), and the capabilities to read those files and to recreate them with their owner, mode and timestamps on the backing branch, such as `CAP_DAC_READ_SEARCH`, `CAP_CHOWN` and `CAP_FOWNER`. The capability probe uses scratch files that cachy owns, so it cannot tell that `CAP_LEASE` is missing: each move of another user's file then fails. With `open_file_guard: proc`, cachy needs root or `CAP_SYS_PTRACE` in the host's PID namespace to see every process's open files.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success: a run whose moves all succeeded, a check that found nothing to do, or a dry run that could check every move unit. Skips (in use, changed during the move, too young and the like) are not failures. |
| `1` | cachy refused to run, and moved nothing: for example, an invalid configuration or command line, an unreadable cache branch, another invocation holding the instance lock, a failed capability probe, or a cache pool whose first branch is not the cache branch. |
| `2` | At least one move failed, or the run stopped early: on `SIGTERM` or `SIGINT`, or on an error outside any move, such as a recovery leftover that could not be resolved. Whatever moved stays moved; the run summary lists the failures. |

A failing run hook never changes the exit code.

`cachy daemon` exits with `0` once it is stopped, even during a run, and with `1` if it refuses to start, such as on an invalid configuration, one with neither `daemon.schedules` nor `daemon.wake_trigger`, or a backing disk that is not there. Its checks do not end it: a refused or failed one is logged, and the daemon goes on to the next.

## Testing

`cargo test` runs the integration tests with the cache branch on tmpfs (`/dev/shm`). Tests that need a mergerfs cache pool skip themselves when mergerfs or passwordless sudo is missing.

CI also runs the same tests on every pair of tmpfs, ext4 and ZFS filesystems, with mergerfs available. To do that yourself on a disposable Linux machine with passwordless sudo, ZFS and mergerfs:

```sh
ci/setup-test-filesystems.sh /tmp/cachy-fs
ci/test-filesystem-pairs.sh /tmp/cachy-fs
```

`tests/common/mod.rs` documents the environment variables that select the filesystems.

CI also installs the static binary with the shipped systemd units and runs it as `cachy.service`: a run, `systemctl stop` in the middle of a copy, a refusal, and the priorities that the journal records; then it starts `cachy-daemon.service` with the timer enabled, for the warning, and stops it. The script needs systemd, cgroup v2 and passwordless sudo, and replaces any installed cachy and `/etc/cachy/cachy.yml`, so run it only on a disposable machine:

```sh
ci/test-systemd-service.sh target/x86_64-unknown-linux-musl/release/cachy /tmp/cachy-service
```

The crash tests kill cachy, or fail a syscall, at every filesystem operation of a run, and check that the next run recovers without losing anything. They need a test-only build of cachy with fault injection; in a normal build they are skipped. So do most daemon tests (`tests/daemon.rs`), which set the daemon's clock rather than wait for its schedules:

```sh
cargo test --features fault-injection
```

The property tests (`tests/properties.rs`) generate random cache-branch trees, with odd byte names, hardlink groups, symlinks, sparse and zero-byte files and exclusions, run cachy on each, and check that every move unit ends up exactly once, on one branch, with its content and metadata, that excluded ones stay untouched, and that the run reached its target or left nothing eligible behind. They run 32 cases by default; `PROPTEST_CASES` sets another number. A failing case shrinks to a minimal tree, saved in `tests/proptest-regressions/` and replayed first on later runs; commit it with the fix. A fault-injection build runs them much faster, as it skips the one-second wait before a retired source is deleted:

```sh
PROPTEST_CASES=500 cargo test --features fault-injection --test properties
```

The Quality reports workflow publishes test coverage (`cargo-llvm-cov`) and mutation testing (`cargo-mutants`) of the move transaction, recovery and exclusions as job summaries and artifacts; neither fails the build. A pull request tests the mutants in its own diff; running the workflow manually tests them all. To run the same locally:

```sh
cargo llvm-cov --features fault-injection --html
ci/mutants.sh 0/1          # all mutants; add a base commit to test only its diff
ci/mutants-summary.sh mutants.out local
```

## Licence

GPL-3.0-or-later, same family as the upstream project. See [LICENSE](LICENSE).
