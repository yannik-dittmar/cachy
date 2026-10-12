# mergerfs write semantics and OpenZFS syscall support

Research date: 2026-10-12. This is a follow-up to [upstream-and-feature-feasibility.md](upstream-and-feature-feasibility.md) and was gathered during the design interview.

Pinned sources:
- mergerfs `8e87bcbff268`, read file by file (raw fetches, so there are no line anchors)
- OpenZFS `4542a317e2fa` (master) plus release tags
- Linux `24a19b953e2a` and `v6.8`

Nothing here was tested on a live system.

Link prefixes:
- **M** = `https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/`
- **Z** = `https://github.com/openzfs/zfs/blob/4542a317e2fa524e24c1c28b6fbca9cd3634ba56/`
- **K** = `https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/`
- **K68** = `https://github.com/torvalds/linux/blob/v6.8/`

## 1. mergerfs: can a file on a backing branch come back to the cache branch?

The setup in question is a cache pool with the cache branch first and `category.create=ff`.

**Editing in place.**
- `open` is a search function, with default policy `ff`, so writes go to the first branch that has the path (M `src/policy_ff.cpp`, `src/fuse_open.cpp`).
- There is no copy-up.
- `cache.writeback` only rewrites open flags.
- `link-cow` (default off) copies within the **same** branch (M `src/fs_cow.cpp`).

**Atomic save** (a temp file is created on the cache branch, then `rename(tmp, name)` while `name` exists only on a backing branch).
- With `ignorepponrename=false` (the default), `_rename_create_path` renames on the branch that holds the source.
- It then **removes the target from every branch without a source**, and unlink failures there are ignored (M `src/fuse_rename.cpp`, `mkdocs/docs/config/rename_and_link.md`).
- Result: the new version exists only on the cache branch.

**`moveonenospc`.**
- It defaults to on, with policy `pfrd`. On an `ENOSPC`/`EDQUOT` write, it copies the file to a branch chosen by the create policy, which can be the cache branch, and then unlinks the source, ignoring errors (M `src/fs_movefile_and_open.cpp`, `src/fuse_write.cpp`).
- Partial failures leave duplicates.

**Duplicates.**
- `open`, `getattr` and `readdir` (`seq`, with deduplication) see the first branch's copy, so a backing copy is shadowed.
- `unlink`, `chmod`, `truncate`, `utimens` and `setxattr` (`epall`) hit **every** copy.
- Duplicates otherwise come only from:
  - unlink failures that are ignored;
  - `moveonenospc` partial failures;
  - out-of-band writes;
  - `mergerfs.dup`.

**`link-exdev` / `rename-exdev`.**
- Both default to `passthrough`.
- Their non-default values replace real files with symlinks.
- They apply only with path-preserving policies.

**Docs on tiering.**
- "mergerfs does not natively support any sort of tiered caching currently."
- The cache branch should be listed first, and the recommended create policy is `ff`, `lus` or `lfs`.
- The mover moves from the cache *branch* (M `mkdocs/docs/extended_usage_patterns.md`).

## 2. OpenZFS

| Topic | Finding | Source |
|---|---|---|
| `F_SETLEASE` | `.setlease = generic_setlease` was added in **2.3.7 / 2.4.2**. Before Linux 7.0, the kernel falls back to `generic_setlease` for a NULL handler. On 7.0+, a NULL handler returns `EINVAL`, so 7.0+ needs ZFS ≥ 2.3.7/2.4.2. Leases are broken only by `open`/`truncate`, not by writes through already-open fds. `i_readcount` is only maintained with CONFIG_IMA. | Z `module/os/linux/zfs/zpl_file.c#L1386`; K68 `fs/locks.c#L1956-L1959`; K `fs/locks.c#L2108-L2115`, `fs/open.c#L987` |
| `O_TMPFILE` | Supported since 0.7.0. The `linkat` that publishes it is **not ZIL-logged**: it waits for a full txg sync (`txg_wait_synced`), which takes seconds per link. | Z `zpl_inode.c#L256-L310`, `zfs_vnops_os.c#L3720-L3760` |
| `linkat(AT_EMPTY_PATH)` | On 6.8 it needs CAP_DAC_READ_SEARCH; current kernels relax that (the exact release is unverified). `/proc/self/fd/N` with `AT_SYMLINK_FOLLOW` needs no capability. | K68 `fs/namei.c#L4651`; K `fs/namei.c#L5988-L5997` |
| `renameat2` | `RENAME_NOREPLACE`, `RENAME_EXCHANGE` and `RENAME_WHITEOUT` are supported since **2.2.0**; earlier releases return `EINVAL`. Renames across datasets give `EXDEV`. | Z `zfs_vnops_os.c#L2840-L2846`, `#L3057-L3059` |
| `SEEK_HOLE`/`SEEK_DATA` | Supported. On a file with dirty data, `zfs_dmu_offset_next_sync=1` forces a txg sync. **Bug #15526** (2.2.0, 2.2.1, and 2.1.x < 2.1.14) could report data as holes; it is fixed in 2.2.2. With compression on, all-zero blocks are stored as holes. | Z `module/zfs/dmu.c#L2722-L2748`, `zio.c#L2042` |
| `copy_file_range` across pools | Block cloning returns `EXDEV` for different pools (or different encryption keys). ZFS then falls back to the kernel's `splice_copy_file_range` byte copy, so userspace never sees `EXDEV`. `zfs_bclone_enabled` is 0 in 2.2.1–2.2.11 and 1 from 2.3.0. | Z `module/zfs/zfs_vnops.c#L1620-L1646`, `zpl_file_range.c#L167-L208` |
| `statfs` | `f_bfree == f_bavail == available` (there is no root reserve), and `f_blocks = referenced + available`. When a snapshot holds a file's blocks, **unlinking it lowers `referenced` (so df Use% drops) but not `available`**. Freed space appears only after the txg sync, and large files are freed over several txgs. Freed bytes equal `st_blocks`, which reflects compression. | Z `zfs_vfsops.c#L974-L1029`, `dsl_dataset.c#L3073-L3096`, `man/man7/zfsprops.7#L363-L370` |
| atime | `atime=off` sets SB_NOATIME. `mount.zfs` also passes `noatime`, so it is visible in mountinfo and in `ST_NOATIME`. **Legacy/fstab mounts without `noatime` show `relatime`** even though atime is off. | Z `zfs_vfsops.c#L114-L131`, `lib/libzfs/os/linux/libzfs_mount_os.c#L83-L84`; K `fs/statfs.c#L26-L27` |
| xattrs/ACLs | `acltype=off` makes `system.posix_acl_*` return `EOPNOTSUPP`. `user.*` xattrs work. SA xattrs are limited to 32 KiB per value and about 64 KiB in total; above that, ZFS silently falls back to directory xattrs. | Z `zfs_vfsops.c#L157-L176`, `include/sys/zfs_sa.h#L125-L126` |
| `fsync` | File or directory fsync goes to `zil_commit`. Namespace operations are sync ZIL records, so fsync makes data and directory entries durable. | Z `module/zfs/zfs_vnops.c#L104-L116`, `zil.c#L2572` |
| `st_dev`/`st_ino` | `st_dev` is a per-dataset anonymous device, **not stable across remounts**. `st_ino` is the object number, unique within a dataset and reusable after a delete. | Z `zpl_super.c#L865`, `zfs_znode_os.c#L607` |

## 3. Checked on a GitHub-hosted runner

Run on 2026-10-12 on `ubuntu-latest` (Ubuntu 24.04, kernel `6.17.0-1022-azure`, zfs-kmod 2.3.4, userland 2.2.2, mergerfs 2.33.5 from apt), using runs [38247293342](https://github.com/yannik-dittmar/cachy/actions/runs/38247293342) and [38247414120](https://github.com/yannik-dittmar/cachy/actions/runs/38247414120). The runner allows passwordless `sudo`, `modprobe zfs`, loop devices and `/dev/fuse`, so privileged integration tests need no special runner.

- **ZFS pools on loop files** (2 GiB each) are created in seconds.
- **ZFS, ext4 (loop) and tmpfs:** on all three, the `F_RDLCK` lease is granted, and it is refused with `EAGAIN` while a writer holds the file open. `renameat2(RENAME_NOREPLACE)` fails with `EEXIST` on an existing target, and `SEEK_DATA` skips a 4 MiB hole.
- **Cross-pool `copy_file_range`** (ZFS → ZFS, 64 MiB) succeeds with identical content.
- **Snapshot pinning:** after unlinking a 64 MiB file held by a snapshot, `df` Use% fell from 4% to 1% while `available` stayed at exactly 1811677184 bytes. This confirms that a cache's fill level must be measured in free space, not Use%.
- **`atime=off`** shows as `noatime` in mountinfo for a normal ZFS mount.
- **mergerfs pool** (ZFS cache branch first, then ext4, `category.create=ff`):
  - `user.mergerfs.branches` is readable from `<pool>/.mergerfs`.
  - A file written through the pool lands on the cache branch.
  - While the file is held open for writing through the pool, a lease on the cache-branch file is refused (`EAGAIN`). The open-file guard therefore sees writers that go through mergerfs.
- **Gotcha:** mergerfs's default `minfreespace=4G` makes every create in tiny test pools fail with `ENOSPC`, so test pools need e.g. `minfreespace=1M`. The apt mergerfs is old (2.33.5), so CI should install a current release `.deb`.

## Not verified

- The kernel release that relaxed `linkat(AT_EMPTY_PATH)`.
- Distribution-specific ZFS and kernel patches (backports can change the version boundaries above).
- The cow-shell tool in mergerfs-tools.
- Whether a stale negative dentry can cause a `FUSE_CREATE` for an existing name.
