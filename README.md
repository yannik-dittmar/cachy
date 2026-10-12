# cachy

Moves files from a [mergerfs](https://github.com/trapexit/mergerfs) cache branch to the backing pool once the cache runs low on free space, without moving files that are still in use.

cachy is heavily inspired by [mergerfs-cache-mover](https://github.com/monstermuffin/mergerfs-cache-mover) by MonsterMuffin. It is a new implementation in Rust with its own configuration and design, not a drop-in port.

## Status

Early development. Do not use it on real data yet: `cachy run` can already move files, but it has no in-use guard, so it can move a file that an application is still writing.

## Testing

`cargo test` runs the integration tests with the cache branch on tmpfs (`/dev/shm`). Tests that need a mergerfs cache pool skip themselves when mergerfs or passwordless sudo is missing.

CI also runs the same tests on every pair of tmpfs, ext4 and ZFS filesystems, with mergerfs available. To do that yourself on a disposable Linux machine with passwordless sudo, ZFS and mergerfs:

```sh
ci/setup-test-filesystems.sh /tmp/cachy-fs
ci/test-filesystem-pairs.sh /tmp/cachy-fs
```

`tests/common/mod.rs` documents the environment variables that select the filesystems.

## Licence

GPL-3.0-or-later, same family as the upstream project. See [LICENSE](LICENSE).
