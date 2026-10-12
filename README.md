# cachy

Moves files from a [mergerfs](https://github.com/trapexit/mergerfs) cache branch to the backing pool once the cache runs low on free space, without moving files that are still in use.

cachy is heavily inspired by [mergerfs-cache-mover](https://github.com/monstermuffin/mergerfs-cache-mover) by MonsterMuffin. It is a new implementation in Rust with its own configuration and design, not a drop-in port.

## Status

Early development. Do not use it on real data yet: `cachy run` can already move files, but it has no in-use guard, so it can move a file that an application is still writing.

## Licence

GPL-3.0-or-later, same family as the upstream project. See [LICENSE](LICENSE).
