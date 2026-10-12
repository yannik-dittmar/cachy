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

To keep files and directories on the cache branch, list gitignore patterns under `exclude`. Unlike in a `.gitignore`, every pattern is anchored at the cache branch root: `appdata/` excludes only the top-level `appdata` directory, and `**/` matches at any depth. Excluded directories are not walked at all. For SnapRAID, for example:

```yaml
exclude:
  - snapraid.content
  - snapraid.parity
  - .snapraid/
  - '**/.snapshots/'
```

cachy moves regular files of any size, including empty ones, and never moves FIFOs, sockets or device nodes; it skips them as special files. It does not walk into other filesystems mounted inside the cache branch.

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
