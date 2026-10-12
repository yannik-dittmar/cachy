# Write to backing branches directly, never through mergerfs

cachy writes moved files straight to `<backing branch>/<relpath>` and picks the branch itself, instead of writing through a mergerfs mount and letting mergerfs's create policy choose. mergerfs cannot publish without clobbering (its `rename2` returns `ENOSYS`, so the kernel answers `EINVAL` for `RENAME_NOREPLACE`), and the FUSE layer hides real `O_TMPFILE` and directory-`fsync` semantics. All of these are needed for a crash-safe, no-clobber move, and owning branch selection is also what later lets cachy respect the backing disks' wake state.

## Consequences

- cachy must know the branch layout. It reads `user.mergerfs.branches` from the cache pool, or takes an explicit branch list, and refuses to start unless the cache branch is the first branch of the cache pool.
- cachy honours `RO`/`NC` branch modes and `minfreespace` itself, keeps hardlink groups on one branch, and removes stale copies on other branches when it publishes a move.
- Users cannot get their cache pool's create policy for moved files. cachy's placement (existing parent directory first, then most free space) applies instead.
