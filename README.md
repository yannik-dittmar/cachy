# cachy

Moves files from a [mergerfs](https://github.com/trapexit/mergerfs) cache branch to the backing pool once the cache runs low on free space, without moving files that are still in use.

cachy is heavily inspired by [mergerfs-cache-mover](https://github.com/monstermuffin/mergerfs-cache-mover) by MonsterMuffin. It is a new implementation in Rust with its own configuration and design, not a drop-in port.

## Status

Early development. Do not use it on real data yet. `cachy run` moves files, skips files in use, recovers from a crash in the middle of a move, and preserves owner, group, mode, xattrs, ACLs and timestamps.

A move fails, keeping the source, if the backing branch cannot store the source's xattrs or ACLs. To accept losing them instead, with a warning, list the kinds in the configuration:

```yaml
allow_metadata_loss: [xattrs, acls]
```

Owner, group, mode and mtime are never optional. Directories that cachy creates on a backing branch take the owner, mode, xattrs and ACLs of the cache branch's directory, and its mtime at the end of the run; existing backing directories are left as they are.

Copies keep the holes of sparse files, so moving a sparse file takes up only the space of its data on the backing branch. While copying, cachy hashes each file's content (XXH3-128) and logs the hash of every file it moves. To also read each copy back once it is fsynced, and fail the move, keeping the source, unless the hash matches, set:

```yaml
verify: readback
```

cachy asks the kernel to drop the copy from the page cache before reading it back, so the read comes from storage; ZFS serves it from its own cache (ARC) instead, if the copy is still there.

To keep files and directories on the cache branch, list gitignore patterns under `exclude`. They follow the usual `.gitignore` rules, as if the file sat at the cache branch root: `*.part` matches at any depth, and `/appdata/` only the top-level `appdata` directory. Excluded directories are not walked at all. [`cachy.example.yml`](cachy.example.yml) documents every setting, with a summary of the pattern rules and SnapRAID examples.

cachy moves regular files of any size, including empty ones, hardlink groups and symlinks, and never moves FIFOs, sockets or device nodes; it skips them as special files. A symlink is moved as itself, never followed: it arrives on the backing branch with its link text verbatim (relative, absolute, dangling or pointing at a directory) and keeps its owner and timestamps (on ZFS, a crash within seconds of the move can lose these; see [ADR 0002](docs/adr/0002-publish-via-staging-directory.md)). Like a file, it is never published over an existing file on the backing branch. The names of a file with several hard links move together, as one inode with all its links, so the move frees its space; if the walk does not find all of its names (one is excluded, say), cachy skips it as a partly excluded hardlink group and lists the names it found. It does not walk into other filesystems mounted inside the cache branch.

A run moves files in the configured order (least recently accessed first, by default) until it has moved as many bytes as it needs to reach the target, measured once at its start. If free space then grew by less than half the bytes moved, for example because a snapshot of the cache branch still holds the files, cachy warns; so may writes to the cache branch during the run. Before it warns, cachy waits up to 15 seconds for the space to show as free, because ZFS shows freed space only once its transaction group syncs. The threshold and target may be byte sizes or percentages of the cache branch's size.

With a `max_age` (such as `7d`), files unchanged for longer than that are moved even while free space is above the threshold, and a run moves them before any other file, beyond its byte budget if need be. A check then walks the cache branch to find them; without a max age, it only reads the cache branch's free space. `cachy run --max-age <duration>` overrides the configured max age.

cachy writes to backing branches directly, never through mergerfs ([ADR 0001](docs/adr/0001-write-to-backing-branches-directly.md)), so it needs to know the layout. Point `cache_pool` at the cache pool's mount, and cachy reads the branches, their modes and `minfreespace` from mergerfs's runtime interface. It refuses to run unless the cache branch is the cache pool's first branch. If that interface is unavailable (`xattr=nosys`), or there is no cache pool to read, list the backing branches under `backing_branches` instead. A move goes to a branch that is neither `RO` nor `NC`, keeps its `minfreespace` free and has room for the whole file, preferring a branch that already has the file's directory, then the one with the most free space. Each backing branch takes one move at a time, and different branches move in parallel.

`cachy run --drain` skips the check and moves every file it may move. `cachy run --dry-run` writes nothing: it prints the byte budget, the files a run would move, in order, and every file it would skip, with the reason. Combine both to preview a drain. To tell which files are in use, a dry run briefly takes the same lease on each file that a move would, so a writer opening a file at that moment waits until it is released.

cachy detects files in use with kernel file leases. One race remains that leases cannot close: an application that looked up a file's path just before cachy retired it, but stalled for more than a second before opening it, opens the retired copy, and its writes are lost.

## Testing

`cargo test` runs the integration tests with the cache branch on tmpfs (`/dev/shm`). Tests that need a mergerfs cache pool skip themselves when mergerfs or passwordless sudo is missing.

CI also runs the same tests on every pair of tmpfs, ext4 and ZFS filesystems, with mergerfs available. To do that yourself on a disposable Linux machine with passwordless sudo, ZFS and mergerfs:

```sh
ci/setup-test-filesystems.sh /tmp/cachy-fs
ci/test-filesystem-pairs.sh /tmp/cachy-fs
```

`tests/common/mod.rs` documents the environment variables that select the filesystems.

The crash tests kill cachy, or fail a syscall, at every filesystem operation of a run, and check that the next run recovers without losing anything. They need a test-only build of cachy with fault injection; in a normal build they are skipped:

```sh
cargo test --features fault-injection
```

## Licence

GPL-3.0-or-later, same family as the upstream project. See [LICENSE](LICENSE).
