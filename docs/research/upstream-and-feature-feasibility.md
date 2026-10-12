# Upstream behaviour, data-safety audit, and feature feasibility for cachy

Research date: 2026-10-12. Upstream pinned at `e48c80cc40a91a51da0467e35f187fda120b03e4` (tag v1.4.3 plus dependency bumps). All upstream links are permalinks to that commit. Other pinned sources: Linux `24a19b953e2a` (mainline, 2026-10-11), mergerfs `8e87bcbff268`, smartmontools `cf64246ba9a0`, hd-idle `0c42d8cca82f`, sg3_utils `1929463554c5`, man-pages git `c6cefe27dd02`, CPython `v3.14.0` (the version in upstream's Dockerfile).

Method note: man7.org was unreachable from the research sandbox. Every man-page quote was read from the man-pages git tree at `c6cefe27dd02`, which is the source man7.org renders. The man7.org links below point at the same pages.

## TL;DR

1. **Open-file claim: confirmed.** Upstream never checks whether a source file is open, mmapped, locked or being written. It copies, compares sizes only, and then deletes the source ([mover.py L96-L136](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L96-L136)). Any write that lands during or after the copy is silently lost.
2. **Spin-state claim: confirmed, and it is worse than "can't detect".** Upstream has no spin-state logic at all. On every run, before the threshold check, it also does a full `os.walk` of the **entire backing pool** to clean temp files ([cache-mover.py L62-L67](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L62-L67)). That walk can wake every HDD on every scheduled run.
3. **"Moves the oldest files" is false since v1.3.** The mtime sort was dropped in refactor `daaefa9` (2025-03-27). Files are now moved in `os.walk` directory order.
4. **Other high-severity safety gaps:** no `fsync` anywhere before the source is unlinked. Crash recovery scans `BACKING_PATH`, but the `.deleted.` markers are created in `CACHE_PATH`. The orphan cleaner deletes any backing-pool file named like `.<x>.<6 alphanumerics>`. Existing destination files are silently replaced, and mergerfs then removes that target from every other branch too.
5. **The single-instance lock does not work.** It looks for `filesystem.py` in the last argv element, and it reads `INSTANCE_ID` from `/proc/<pid>/environ`, which never reflects `os.environ` changes.
6. **Open-file detection: feasible. Recommended: kernel file leases** (`F_SETLEASE`) on the cache-branch file, plus a min-age filter and identity re-checks. Leases count every open `struct file` on the inode. That includes mergerfs's fds and FUSE-passthrough backing files, which can be invisible to `/proc` and `lsof`. `/proc/*/fd` scanning is an optional second check.
7. **Spin-aware moves: feasible, with one irreducible trade-off.** Detect activity passively via `/sys/block/<disk>/stat`, optionally confirmed by ATA CHECK POWER MODE over SG_IO (needs CAP_SYS_RAWIO). cachy must pick the destination **branch itself** and write to it directly: mergerfs create policies are space-based or path-based, not spin-aware.
8. **The trade-off:** to prove a destination path does not already exist on a sleeping disk, cachy must look it up there, and that lookup may wake the disk. mergerfs's author documents the same limit. This needs a human decision.
9. **Upstream has no tests and no test CI.** CI only builds and pushes Docker images, and `.gitignore` excludes `test_*.py`. Treat upstream's code as a behaviour oracle only after the bugs listed in §2 are filtered out.
10. **Port strategy:** port config, CLI, threshold/target, exclusions, hardlinks, symlinks, empty-dir cleanup, notifications and Docker scheduling. Each needs the fixes listed in §6. Drop AUTO_UPDATE and the `ps`-based instance check.

---

## 1. Upstream behaviour spec (porting checklist)

`U` = `https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4`. Links are written out in full.

### 1.1 CLI surface and process flow

| Behaviour | Detail | Source |
|---|---|---|
| Flags | `--dry-run`, `--console-log` (adds a stderr `StreamHandler`), `--config PATH`. There is no `--version`; it was removed in v1.4.1. | [cache-mover.py L30-L34](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L30-L34), [CHANGELOG L8-L10](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/CHANGELOG.md#L8-L10) |
| Banner | ASCII art is always printed to stdout first, including under cron. | [cache-mover.py L16-L28](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L16-L28) |
| Order of operations | Steps: (1) load config, (2) set up logging, (3) instance check, (4) NotificationManager (`git rev-parse` unless in Docker or not a git repo), (5) CleanupManager, (6) AUTO_UPDATE, which may `execv`, (7) **orphan cleanup on BACKING_PATH, even in dry-run**, (8) usage check, (9) install SIGINT/SIGTERM handlers, (10) move, (11) notify. | [cache-mover.py L36-L107](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L36-L107) |
| Exit codes | `1` on a config error (message to stderr), when another instance is detected, or on an uncaught exception (re-raised after `notify_error`, so Python exits 1 with a traceback). `0` when below threshold or on normal completion, **even if every individual move failed**. | [cache-mover.py L38-L53](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L38-L53), [L75-L78](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L75-L78), [L104-L107](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L104-L107) |
| Dry-run quirks | The threshold gate is skipped (`if not args.dry_run and not needs_cleanup`), so a dry run proceeds even below threshold. Usage never drops in a dry run, so **every** eligible file is reported "Would move" whenever usage > target. Orphan cleanup only logs "Would clean/restore". No notifications are sent. | [cache-mover.py L75](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L75), [L101-L102](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L101-L102), [mover.py L75-L78](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L75-L78) |
| Signals | SIGINT and SIGTERM set a `stop_event`. Workers check it only at task start, so in-flight copies finish and queued tasks return immediately. Handlers are installed only after the threshold check. | [cache-mover.py L80-L84](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L80-L84), [mover.py L60-L61](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L60-L61) |

### 1.2 Configuration

Defaults come from [config.py L6-L36](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L6-L36). Loading is in [L48-L128](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L48-L128).

| Key | Section | Default | Env var and conversion | Notes |
|---|---|---|---|---|
| CACHE_PATH | Paths | (required) | `CACHE_PATH`, str | Must be truthy, otherwise `ValueError` → exit 1. It must be the cache **disk root**, not the mergerfs cache mount ([config.example.yml L3](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/config.example.yml#L3)). Existence is not checked. |
| BACKING_PATH | Paths | (required) | `BACKING_PATH`, str | Usually a mergerfs pool **without** the cache. Never checked to differ from, or not contain, CACHE_PATH. |
| LOG_PATH | Paths | `/var/log/cache-mover.log` | `LOG_PATH` | A missing directory crashes `RotatingFileHandler` → traceback. |
| THRESHOLD_PERCENTAGE | Settings | 70 | `float()` | YAML values are not type-checked. Env gives a float (log shows `70.0%`), YAML an int. |
| TARGET_PERCENTAGE | Settings | 25 | `float()` | Validation: `0/0` ⇒ `EMPTY_CACHE_MODE=True`; otherwise `threshold <= target` ⇒ `ValueError`. No 0–100 bounds check. ([L121-L127](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L121-L127)) |
| MAX_WORKERS | Settings | 8 | `int()` | Not validated. `0` would raise in `ThreadPoolExecutor`. |
| MAX_LOG_SIZE_MB | Settings | 100 | `int()` | Forced to 100 when `DOCKER_CONTAINER` is set. |
| BACKUP_COUNT | Settings | 1 (example file: 5) | `int()` | Forced to 1 when `DOCKER_CONTAINER` is set ([L108-L111](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L108-L111)). |
| AUTO_UPDATE | Settings | False | **none** | Forced False in Docker. |
| UPDATE_BRANCH | Settings | `main` | str | |
| EXCLUDED_DIRS | Settings | hardcoded `snapraid, .snapraid, .content, .snapshots, .snapshot` | `_parse_excluded_dirs`: comma split + strip, unioned with the hardcoded list | YAML accepts a list, a comma string or a scalar, and is unioned with the hardcoded list via `set` (order lost). **An env value replaces the YAML list** instead of merging with it ([L41-L43](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L41-L43), [L64-L73](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L64-L73), [L90](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L90)). |
| SCHEDULE | Settings | `0 3 * * *` | str | Used only by `docker-entrypoint.sh`, which reads env or `/app/config.yml` directly and ignores `--config` ([entrypoint L6-L16](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/docker-entrypoint.sh#L6-L16)). |
| NOTIFICATIONS_ENABLED | Settings | False | `_parse_bool` = `str(v).lower()=='true'` | In YAML the string `"false"` is truthy, because there is no type validation. |
| NOTIFICATION_URLS | Settings | `[]` | `split(',')`, no strip | A YAML string would be iterated character by character. |
| NOTIFY_THRESHOLD | Settings | False | `_parse_bool` | |
| INSTANCE_ID | Settings | None | str | See §1.7. |
| LOG_LEVEL | Settings | `INFO` | str | Upper-cased (YAML at load time, and again in `setup_logging`). Invalid values fall back to INFO with a warning. |
| KEEP_EMPTY_DIRS | Settings | False | `_parse_bool` | v1.4.2 |
| SKIP_HARDLINKED_FILES | Settings | False | `_parse_bool` | v1.4.3 |

Precedence is defaults < YAML < env < Docker overrides ([L101-L111](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L101-L111)).

- **Config file resolution:** `--config` must exist, else `ValueError`. Without it, `<script_dir>/config.yml` is optional and its absence is silent ([L51-L59](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/config.py#L51-L59)).
- **Malformed YAML:** an empty file or `Settings:` with no body makes `.get` on `None` raise, which surfaces as "Error loading configuration" and exit 1.
- **Unknown keys** are accepted silently.

### 1.3 Usage measurement, trigger and stop condition

- **Usage measurement:** `usage% = used/total*100` via `shutil.disk_usage(CACHE_PATH)` ([filesystem.py L8-L10](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L8-L10)). CPython computes `total = f_blocks*f_frsize` and `used = (f_blocks - f_bfree)*f_frsize` ([shutil.py L1443-L1453](https://github.com/python/cpython/blob/v3.14.0/Lib/shutil.py#L1443-L1453)). Reserved blocks therefore count as *free*, so this differs from `df`'s Use%. The mergerfs reference script uses `df` Use% instead ([mergerfs.percent-full-mover L13](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/tools/mergerfs.percent-full-mover#L13)).
- **Trigger:** `needs_cleanup = usage > threshold` (strict), or 0/0 mode ([cleanup.py L19-L30](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/cleanup.py#L19-L30)). Below threshold, it logs "Cache usage below threshold", sends `notify_threshold_not_met` (only if NOTIFY_THRESHOLD is set) and exits 0.
- **Selection order: none.** `gather_files_to_move` returns files in `os.walk` (scandir) order ([filesystem.py L75-L110](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L75-L110)). They are submitted to the pool in that order: all regular files, then hardlink groups, then symlinks ([mover.py L412-L449](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L412-L449)). No atime, mtime or ctime is consulted (`grep` for `sort|atime|mtime|ctime` in `*.py` finds nothing).
  - Up to v1.2, files were sorted by `st_mtime` ascending (`all_files.sort(key=lambda fn: os.stat(fn).st_mtime)`, [v1.2 cache-mover.py L296](https://github.com/monstermuffin/mergerfs-cache-mover/blob/7ca5842e618468c26019cd3e4f6d1256696d8135/cache-mover.py#L296)). The sort was deleted in the "used LLM to try and separate everything out" refactor [`daaefa9`](https://github.com/monstermuffin/mergerfs-cache-mover/commit/daaefa98ce58713537af9cd3dd26c4bb85f309be).
  - The README still says "move the oldest files" ([README L8](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/README.md#L8)). Issue [#4](https://github.com/monstermuffin/mergerfs-cache-mover/issues/4) (open) asks for age-based moving; the maintainer leaned towards mtime there. The mergerfs reference movers sort by **atime** (`find -printf '%A@'`, [L16](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/tools/mergerfs.percent-full-mover#L16)).
- **Per-file stop check:** before each move, under a mutex, re-read cache usage; if `usage <= target`, skip ([mover.py L75-L78](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L75-L78)). After each *successful* completion, re-check; on `<= target`, set `stop_event` and break ([mover.py L451-L473](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L451-L473)). Overshoot of up to `MAX_WORKERS-1` files is expected. The full list is kept as a "buffer" by design (issue [#7](https://github.com/monstermuffin/mergerfs-cache-mover/issues/7)).
- **Empty-cache mode (0/0):** always runs. Since `usage <= 0` is never true while files remain, it moves everything that is not excluded, zero-sized, skipped-hardlinked or a special file. If nothing was found, `notify_threshold_not_met` is sent (gated by NOTIFY_THRESHOLD; fix for issue [#30](https://github.com/monstermuffin/mergerfs-cache-mover/issues/30)/[#31](https://github.com/monstermuffin/mergerfs-cache-mover/issues/31)) ([cache-mover.py L72-L100](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L72-L100)).
- **Completion notification** fires whenever at least one candidate was found, **even if 0 files moved**. `run_cleanup` returns a non-empty tuple in that case ([cleanup.py L37-L68](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/cleanup.py#L37-L68)).

### 1.4 Eligibility, exclusions, hardlinks, symlinks, special files

**Walk.** `os.walk(CACHE_PATH)` runs top-down with `followlinks=False`. It does not prune excluded directories; it only skips their files. It crosses mount points, and `scandir` errors are ignored by default ([Python os.walk docs](https://docs.python.org/3/library/os.html#os.walk)).

**`is_excluded(root)`** ([filesystem.py L22-L41](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L22-L41)):
- It is applied to the **absolute** directory path, which includes the CACHE_PATH components themselves.
- **A pattern without `/`** matches if it equals any path component. Excluding `mnt` or `cache` with `CACHE_PATH=/mnt/cache` therefore excludes everything.
- **A pattern with `/`** matches if `/<pattern>/` is a substring of `/<path>/`. It is **not anchored to the cache root**, despite [README L375-L377](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/README.md#L375-L377).
- Exclusion applies to **directories only**. File names are never checked, so a `snapraid.content` *file* at the cache root is **not** excluded, despite "hardcoded snapraid file/directory exclusions" ([CHANGELOG L87](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/CHANGELOG.md#L87)). SnapRAID content files are conventionally files such as `/mnt/disk1/snapraid.content` ([SnapRAID manual](https://www.snapraid.it/manual)).

**Per-file filters** ([filesystem.py L85-L104](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L85-L104)):
- **Symlinks** go to a separate symlink map. Symlinks *to directories* land in os.walk's `dirnames`, so they are **never moved**.
- **`size == 0`** is skipped. That covers empty files, FIFOs, sockets and device nodes, which are therefore never moved.
- **`nlink > 1`** is skipped when SKIP_HARDLINKED_FILES is set.

**Hardlink groups** ([filesystem.py L50-L64](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L50-L64)):
- Files are grouped by `st_ino` **only, not `st_dev`**, and only if at least 2 members were found in the walk.
- A file with `nlink > 1` whose sibling sits in an excluded directory is treated as a regular file. It is copied, but the cache space is not freed (issue [#75](https://github.com/monstermuffin/mergerfs-cache-mover/issues/75)).

**Hardlink move** ([mover.py L174-L319](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L174-L319)):
1. Copy the first member (in walk order) to a temp file and commit it like a regular file.
2. Create the other members with `os.link(first_dest, dest)` through BACKING_PATH.
3. On `EXDEV`, fall back to `create_hardlink_on_same_disk`. It reads the `user.mergerfs.relpath` xattr and the `user.mergerfs.srcmounts` xattr on `<pool>/.mergerfs`, then `os.link`s directly on the branch ([hardlink_manager.py L46-L166](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/hardlink_manager.py#L46-L166)). `srcmounts` is a hidden, read-only key whose value is branch paths without modes ([mergerfs branches.cpp L547-L564](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/branches.cpp#L547-L564), [config.cpp L154-L185](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/config.cpp#L154-L185)). Directories created in that fallback use plain `os.makedirs`, so they get root ownership ([L153-L157](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/hardlink_manager.py#L153-L157)).
4. Rename every source to a `.deleted.` marker, verify the size of the first destination, then delete the markers.

**Symlink move** ([mover.py L321-L394](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L321-L394)):
- **Every relative target is resolved to an absolute path.** If that path lies under CACHE_PATH, it is rewritten to the corresponding BACKING_PATH path. Relative links therefore become absolute links into BACKING_PATH, which bypasses the user's cache-pool mount.
- **Commit:** `if lexists(dest): os.remove(dest)`, then `os.symlink`, then `os.remove(src)`. This is not atomic, and symlink ownership is not preserved.

### 1.5 Move procedure (regular file)

From [mover.py L59-L172](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L59-L172):

1. `dest = BACKING_PATH/relpath(src, CACHE_PATH)`. It writes **through BACKING_PATH**, which is normally a mergerfs pool, so the mergerfs create policy picks the branch. It does not write to branches directly, except in the hardlink EXDEV fallback.
2. `file_size = getsize(src)`, then the locked usage check.
3. If the destination directory is missing, `makedirs_preserve_stats` creates each missing level and copies `uid`, `gid` and `st_mode` from the corresponding cache directory. Directory timestamps, xattrs and ACLs are **not** copied ([L21-L57](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L21-L57)). This happens *before* the free-space check, so a skipped file can leave empty directories behind.
4. Free-space check: `disk_usage(BACKING_PATH).free < file_size` ⇒ skip. This is the pool aggregate, because mergerfs `statfs=base` sums the branches ([mergerfs statfs](https://trapexit.github.io/mergerfs/latest/config/statfs/)). It ignores per-branch room and `minfreespace` (default 4G, [minfreespace](https://trapexit.github.io/mergerfs/latest/config/minfreespace/)).
5. The temp file is `dirname(dest)/.<basename>.<6 random [A-Za-z0-9]>` ([L14-L19](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L14-L19)), created via `shutil.copy2`, which opens it with `'wb'` (`O_CREAT|O_TRUNC`, no `O_EXCL`).
   - **Data:** `copy_file_range`, falling back to `sendfile`, then `read`/`write` ([shutil.py L283-L345](https://github.com/python/cpython/blob/v3.14.0/Lib/shutil.py#L283-L345)). Cache→FUSE is cross-filesystem, and `copy_file_range` returns `EXDEV` across different filesystem types ([copy_file_range(2)](https://man7.org/linux/man-pages/man2/copy_file_range.2.html)). In practice this means `sendfile`, which reads holes as zeros, so **sparse files become dense**. This is inferred and **unverified** by experiment.
   - **Metadata (`copystat`):** atime/mtime in ns via `utime`, then **all xattrs** (including `system.posix_acl_*` where listed), then `chmod`. xattr errors `EPERM`, `ENOTSUP`, `ENODATA`, `EINVAL` and `EACCES` are **silently ignored** ([shutil.py L382-L475](https://github.com/python/cpython/blob/v3.14.0/Lib/shutil.py#L382-L475)). On a mergerfs pool with `xattr=noattr` or `nosys`, xattrs and ACLs are dropped without any message ([mergerfs xattr](https://trapexit.github.io/mergerfs/latest/config/xattr/)).
   - **Ownership:** explicit `chown(uid,gid)`, then `chmod(S_IMODE)` (the chmod comes after the chown because chown clears setuid/setgid, [chown(2)](https://man7.org/linux/man-pages/man2/chown.2.html)). If either fails, it **only logs a warning** and the move continues with root-owned data ([L101-L108](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L101-L108)).
   - **Not preserved:** ctime, birth time, `security.capability` (cleared by chown, [chown(2)](https://man7.org/linux/man-pages/man2/chown.2.html)), sparseness.
6. "Verification" is `getsize(temp) == file_size`, where `file_size` was taken *before* the copy. A mismatch removes the temp file and fails.
7. `rename(src → src + ".deleted." + 6 hex)` **in the cache**, then `rename(temp → dest)` on the pool.
   - `rename` replaces an existing `dest` atomically ([rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html)). mergerfs then also **removes the target from all branches that have no source file** ([mergerfs rename & link](https://trapexit.github.io/mergerfs/latest/config/rename_and_link/)).
   - If that rename fails, it renames the marker back and removes the temp file.
8. `os.remove(marker)`. A failure here is logged at debug level and the marker is "cleaned up later", but in practice it never is (see §2 R5).
9. **No `fsync` of anything**, neither file nor directory, at any point (`grep fsync` finds nothing).

### 1.6 Concurrency and cleanup

- **Parallelism:** a `ThreadPoolExecutor(MAX_WORKERS)` with one task per file, group or symlink. The only lock serialises the usage check ([mover.py L396-L489](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L396-L489)). Per-file free-space checks race each other, so concurrent copies can exceed real free space; the result is `ENOSPC` mid-copy, which is handled safely (source intact, temp removed in the `except`).
- **Orphan cleanup** ([temp_file_cleanup.py L7-L103](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/temp_file_cleanup.py#L7-L103)) walks **BACKING_PATH** (the whole pool).
  - It prunes only directory *names* that are in EXCLUDED_DIRS. Path patterns are ignored here, unlike `is_excluded`.
  - **Temp regex:** `^\..+\.[a-zA-Z0-9]{6}$`. **Marker regex:** `^.+\.deleted\.[a-fA-F0-9]{6}$`. The temp regex is tested first.
  - **Marker handling:** if the original exists, the marker is deleted; otherwise the marker is renamed to the original.
  - **Temp handling:** temp files are deleted unless a marker exists *in the same backing directory* and the original is missing.
- **Empty-directory cleanup** ([filesystem.py L112-L132](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L112-L132), [cleanup.py L55-L65](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/cleanup.py#L55-L65)):
  - It runs only when `moved_count > 0`, not in a dry run, and only when KEEP_EMPTY_DIRS is off.
  - It walks the cache bottom-up and `rmdir`s **every** empty directory, not just those emptied by this run, cascading upwards. It skips excluded directories and never removes CACHE_PATH itself.
  - Issue [#60](https://github.com/monstermuffin/mergerfs-cache-mover/issues/60) reports this breaking qBittorrent category directories and Immich.

### 1.7 Single-instance locking and INSTANCE_ID

[filesystem.py L134-L195](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L134-L195) scans processes with psutil:
- **Match rule:** the process name is `python` or `python3`, and `script_name in cmdline[-1]`.
- **`script_name` is `basename(__file__)` of `filesystem.py`**, i.e. `"filesystem.py"`. Before the v1.3 refactor it was `cache-mover.py` ([v1.2 L210-L244](https://github.com/monstermuffin/mergerfs-cache-mover/blob/7ca5842e618468c26019cd3e4f6d1256696d8135/cache-mover.py#L210-L244)).
- With INSTANCE_ID, the code sets `os.environ['CACHE_MOVER_INSTANCE_ID']` and compares it with other processes' `process.environ()`.

**Observable behaviour: it never detects a normal second instance.** See R7.

Issue [#37](https://github.com/monstermuffin/mergerfs-cache-mover/issues/37) reports the check "killing" a second Docker instance, which this code cannot explain. Separate containers also have separate PID namespaces, so that report is **unverified**.

### 1.8 Operations

- **Logging** ([logging_setup.py L4-L44](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/logging_setup.py#L4-L44)):
  - Handlers are attached to the root logger, so third-party debug logs (urllib3, apprise) also appear.
  - Size-based rotation uses `RotatingFileHandler(maxBytes=MB*1MiB, backupCount)`, which is not safe across processes.
  - Format: `"%(asctime)s - LEVEL - msg"`. A move is logged as the multi-line `File Move Operation:\n  From: …\n  To: …\n  Moved X.XXMB`, with units in 1024 steps but labelled KB/MB.
  - Summary line: `Moved N files (X) in Ts (Y MB/s)`.
- **Notifications** ([cache_mover/notifications.py](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/notifications.py#L7-L99), [notifications/__init__.py L37-L49](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/notifications/__init__.py#L37-L49)):
  - **Routing:** URLs starting `discord` are converted to `https://discord.com/api/webhooks/{id}/{token}` with custom embeds. URLs starting `slack` need **exactly 3 path tokens**, otherwise they are **silently dropped** ([L78-L89](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/notifications/__init__.py#L78-L89)). The README's `slack://hooks.slack.com/services/…` example has 4 tokens and is therefore dropped. Everything else goes to Apprise.
  - **Events:** completion, error, and threshold-not-met. `notify_empty_cache` is dead code ([L244](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/notifications/__init__.py#L244)).
  - **Apprise speed bug:** the body prints bytes/s labelled "MB/s" ([L141](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/notifications/__init__.py#L141)).
  - **No timeouts:** webhook POSTs and the updater GET have none ([util.py L17](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/notifications/util.py#L17), [updater.py L73](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/updater.py#L73)).
- **AUTO_UPDATE** ([updater.py L57-L99](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/updater.py#L57-L99)): it compares the GitHub API commit of UPDATE_BRANCH with `HEAD`, then runs `git fetch` and `git reset --hard origin/<branch>`, then `os.execv` itself. That means it runs unreviewed remote code as root. It is skipped in Docker.
- **Docker** ([Dockerfile](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/Dockerfile#L1-L27), [docker-entrypoint.sh L1-L28](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/docker-entrypoint.sh#L1-L28)):
  - The image is `python:3.14-slim` with `DOCKER_CONTAINER=1`.
  - The entrypoint dumps `printenv` to `/etc/environment`, writes a cron line that appends output to `/proc/1/fd/1`, and installs it with `crontab`.
  - It **runs a move once at every container start**, then runs `cron -f &` and `tail -f` on the log in the foreground. The HEALTHCHECK tests the crond pid.
  - **PID 1 is bash without a trap.** A child PID namespace's init only receives signals it has a handler for, except SIGKILL and SIGSTOP ([pid_namespaces(7)](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)). `docker stop` sends SIGTERM, then SIGKILL after the grace period ([docker stop](https://docs.docker.com/reference/cli/docker/container/stop/)). So the README's "graceful shutdown" ([L156](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/README.md#L156)) never reaches the Python process; it is killed.
  - The compose file adds `SYS_ADMIN` and `DAC_READ_SEARCH`; the README says privileged.
- **Upstream tests and CI:** none.
  - There are no test files in the tree, and `.gitignore` excludes `test_*.py` ([.gitignore L7](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/.gitignore#L7)). The maintainer mentions private tests in issue [#13](https://github.com/monstermuffin/mergerfs-cache-mover/issues/13).
  - CI = multi-arch Docker build and push ([docker-build.yml](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/.github/workflows/docker-build.yml)) plus a changelog release ([release.yaml](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/.github/workflows/release.yaml)). No lint and no test step. Renovate bumps dependencies.

---

## 2. Data-safety audit of upstream

Severity reflects likelihood × impact for a typical media/torrent NAS.

| # | Risk | Code | Failure scenario | Sev |
|---|---|---|---|---|
| R1 | **No open-file or in-use check** | [mover.py L96-L136](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L96-L136) (no `/proc`, lease or lock code anywhere) | A torrent client writes pieces into a *preallocated* file through the cache pool, so its size stays constant. Upstream copies a partial snapshot; the size check passes; the source is unlinked. The client keeps writing into the unlinked inode (held open by mergerfs), so all later pieces are lost, and the pool now shows the partial HDD copy. The same applies to SQLite, VM images and mmapped files. | **High** |
| R2 | **No re-validation before commit** (size only, measured before the copy) | [L72](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L72), [L110](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L110), [L119-L120](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L119-L120) | An app does an atomic save (`write tmp; rename(tmp, file)`) during the copy. Upstream then renames the **new** inode to the marker and deletes it, and publishes the old content. No fd needs to be open at commit time for this to happen, so open-file detection alone does not prevent it. Same-size in-place edits are also lost (R1). | **High** |
| R3 | **No fsync** of the temp file, the destination directory or the cache directory before unlinking the source | entire mover; `grep fsync` finds nothing | After power loss, the cache filesystem's journal may have committed the unlink while the HDD filesystem has not written the data. fsync does not even persist the directory entry without an fsync on the directory ([fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html)). ext4 `auto_da_alloc` only covers replace-via-rename of an *existing* file ([ext4 docs](https://docs.kernel.org/admin-guide/ext4.html)), and the destination is usually new. Dirty data is written back after `dirty_expire_centisecs` ([vm.rst](https://docs.kernel.org/admin-guide/sysctl/vm.html)). mergerfs's own spin-down FAQ suggests raising that to 120 s ([limit_drive_spinup](https://trapexit.github.io/mergerfs/latest/faq/limit_drive_spinup/)), which widens the window. | **High** |
| R4 | **Orphan cleaner deletes user files by name pattern across the whole backing pool** | [temp_file_cleanup.py L7-L8, L28-L41, L65-L94](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/temp_file_cleanup.py#L7-L94) | Any file matching `^\..+\.[A-Za-z0-9]{6}$` is deleted on every run, for example `.profile.backup`, `.thumbs.sqlite`, or rsync's in-flight temp files (`.name.XXXXXX`). That includes rsync jobs started by mergerfs's own reference movers, which target the base pool. A file `foo.deleted.abcdef` is deleted if `foo` exists. Path-style exclusions are not honoured here. | **Med-High** |
| R5 | **Crash recovery looks in the wrong place** | markers are created next to the *source* ([mover.py L119](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L119)); recovery only scans BACKING_PATH ([cache-mover.py L63-L67](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache-mover.py#L63-L67)) | Crash between the two renames: the next run deletes the HDD temp file as "orphaned" (no marker in that directory). The only copy is then `name.deleted.xxxxxx` on the cache, and the pool shows the file as missing. A later run moves it as a regular file, and the run after that renames it back. Two cycles of the file being absent from apps, with no loss *only if nothing else touches it*. A crash after the publish leaves a duplicate marker that gets moved too. | **Med** |
| R6 | **Silent clobber of an existing destination** | [mover.py L124](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L124); symlinks: [L359-L360](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L359-L360) | The same relpath exists on the HDDs (an out-of-band copy, a second cache instance, or an old version). It is replaced, and mergerfs additionally deletes the target on all other branches ([rename & link](https://trapexit.github.io/mergerfs/latest/config/rename_and_link/)). For symlinks, `os.remove(dest)` deletes *any* existing regular file before creating the link. Silent replacement also reduces SnapRAID recoverability until the next sync ("changed or deleted files since the last sync", [SnapRAID manual](https://www.snapraid.it/manual), Limitations). | **Med** |
| R7 | **Instance lock is non-functional** | [filesystem.py L134-L190](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L134-L190) | It looks for `filesystem.py` in `cmdline[-1]`, so normal invocations (`… cache-mover.py --console-log`) never match. The INSTANCE_ID comparison reads `/proc/<pid>/environ`, which does not reflect later `putenv` changes ([proc_pid_environ(5)](https://man7.org/linux/man-pages/man5/proc_pid_environ.5.html)). Overlapping runs (cron plus a manual run, or a container-start run plus cron): run B's startup cleanup deletes run A's in-flight temp files. A's copy then fails, which is safe but wasteful. Two runs racing on one source are mostly safe because the second `rename(src→marker)` hits `ENOENT`. | **Med** |
| R8 | **Hardlink groups keyed by inode number only**, and the walk crosses mounts | [filesystem.py L50-L64](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L50-L64), [mover.py L242-L281](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L242-L281) | CACHE_PATH contains another mount, such as a bind mount or a btrfs subvolume, where two unrelated files with `nlink>1` share an `st_ino`. They are grouped together; only the first is copied; the others become links to it; all sources are deleted. **Silent content loss.** Low probability, catastrophic impact. (Per-subvolume inode namespaces on btrfs are **unverified** here.) | Low/Crit |
| R9 | **Metadata loss is silent** | [shutil L382-L405](https://github.com/python/cpython/blob/v3.14.0/Lib/shutil.py#L382-L405), [mover.py L101-L108](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L101-L108) | xattr and ACL errors are swallowed. A chown or chmod failure only warns. Directory mtimes, xattrs and default ACLs are not copied. Symlink owners are lost. Sparse files become dense, which can cause `ENOSPC` on the destination. | Low-Med |
| R10 | **Visibility gap** | [L119-L124](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L119-L124) | Between `rename(src→marker)` and `rename(temp→dest)`, the file exists under its name on neither the cache nor the HDDs, so apps get `ENOENT` (a "missing" episode in Plex/*arr). Publishing the destination first and then unlinking would never show ENOENT; a duplicate after a crash is harmless. | Low |
| R11 | **Docker stop kills mid-move** | [entrypoint L26-L28](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/docker-entrypoint.sh#L26-L28) | SIGTERM never reaches Python (see §1.8). After 10 s, SIGKILL lands mid-copy or mid-rename, which leads into R5. | Med |
| R12 | **Exclusion semantics** | [filesystem.py L22-L41](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/filesystem.py#L22-L41) | SnapRAID *files* such as `snapraid.content` at the cache root are not excluded. Matching against the absolute path makes exclusions apply in unexpected places; that direction is fail-safe. | Low-Med |
| R13 | **ENOSPC on the destination** | [L86-L88](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L86-L88), [L138-L150](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L138-L150) | Handled safely: the copy raises, the temp file is removed, the source is intact. The pre-check uses aggregate pool free space, so with N disks each nearly full a large file passes the check and then fails mid-write. `moveonenospc` is ineffective with passthrough ([mergerfs passthrough](https://trapexit.github.io/mergerfs/latest/config/passthrough/)). Wasted I/O, no loss. | Low |
| R14 | **Symlink target rewriting** | [L341-L352](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/cache_mover/mover.py#L341-L352) | Relative links become absolute links into BACKING_PATH. That breaks for consumers that mount the pool at another path, such as other containers. | Low |

**User-reported issues and changelog entries** (read via the REST API; `gh issue view` currently fails with a Projects-classic GraphQL error). No issue reports actual data loss. The related reports are:

- [#43](https://github.com/monstermuffin/mergerfs-cache-mover/issues/43): concurrent readers saw a partial destination because the copy went straight to the final name. Fixed in v1.4 with temp names. trapexit (the mergerfs author) commented "Temp files should absolutely be default."
- [#26](https://github.com/monstermuffin/mergerfs-cache-mover/issues/26) and [#1](https://github.com/monstermuffin/mergerfs-cache-mover/issues/1): directories created with root ownership. Fixed in v1.3.3 for the normal path; still present in the hardlink fallback (see §1.4).
- [#13](https://github.com/monstermuffin/mergerfs-cache-mover/issues/13): hardlinks plus `EXDEV`. During that work, a bug where an empty-string exclusion excluded every file was found and fixed.
- [#60](https://github.com/monstermuffin/mergerfs-cache-mover/issues/60): empty-directory deletion broke apps.
- [#75](https://github.com/monstermuffin/mergerfs-cache-mover/issues/75): hardlinked copies duplicated data.
- [#40](https://github.com/monstermuffin/mergerfs-cache-mover/issues/40): "No files to move" at 90.8%, resolved as hardware.
- [#30](https://github.com/monstermuffin/mergerfs-cache-mover/issues/30): the Docker manual trigger ran twice.
- [#7](https://github.com/monstermuffin/mergerfs-cache-mover/issues/7): every cache file is gathered on every run (by design).

---

## 3. Feasibility: open-file detection on Linux

### 3.1 The mergerfs nuance (verified in source)

Applications open files through the cache-pool FUSE mount. mergerfs then `open(2)`s the branch file and **keeps that fd for the lifetime of the FUSE handle**:
- A per-nodeid `open_files` map holds a canonical `FileInfo` with a ref-count.
- Additional opens get their own `FileInfo` with a *duplicated* fd.
- The fd is closed only on the final `release` ([fuse_open.cpp L19-L83](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/fuse_open.cpp#L19-L83), [fuse_release.cpp L19-L40, L76-L127](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/fuse_release.cpp#L19-L127)).

**What this means for detection:**
- **Process attribution:** on the cache branch, the holder appears as the **mergerfs pid**, not the application's.
- **Finding mergerfs's pid:** read the `user.mergerfs.pid` key on `<pool>/.mergerfs` ([config.cpp L274](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/config.cpp#L274), [runtime interface](https://trapexit.github.io/mergerfs/latest/runtime_interface/)).
- **mmap:** a mapping holds a reference to the FUSE `struct file`, so mergerfs keeps its branch fd until the mapping goes away. mmaps made through mergerfs therefore show up as mergerfs fds, not as `/proc/<app>/maps` entries for the branch path.

**FUSE passthrough** (`passthrough.io`; needs Linux ≥ 6.9 and mergerfs running as root, [mergerfs passthrough](https://trapexit.github.io/mergerfs/latest/config/passthrough/)) changes the picture:
- **Backing file:** mergerfs registers its fd via `FUSE_DEV_IOC_BACKING_OPEN` ([fuse_open.cpp L441-L455](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/src/fuse_open.cpp#L441-L455)). The kernel then opens a separate **backing `struct file`** with the FUSE file's flags (`backing_file_open`, [passthrough.c L152-L186](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/fuse/passthrough.c#L152-L186)).
- **Data path:** reads and writes bypass mergerfs entirely.
- **Visibility:** the kernel documents that once a daemon closes its own fd, the backing file is "**invisible to lsof and other inspection tools**" ([fuse-passthrough.rst, "Resource Accounting and Visibility"](https://docs.kernel.org/filesystems/fuse/fuse-passthrough.html)). Current mergerfs keeps its fd, so `/proc` still works today. That is an implementation detail cachy should not rely on.
- **Counters:** the backing file goes through `do_dentry_open`, which increments `i_readcount`/`i_writecount` and calls `break_lease` ([open.c L954-L961, L987](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/open.c#L954-L987)). Inode-level mechanisms (leases) therefore still see it.

**What mergerfs documents:**
- Movers must operate on the cache **filesystem**, not the cache pool ("You could have data loss if the source is the cache pool"). The docs recommend mergerfs-cache-mover or the reference scripts ([extended usage patterns → tiered cache](https://trapexit.github.io/mergerfs/latest/extended_usage_patterns/)).
- The reference scripts use `rsync --remove-source-files` with **no open-file handling** ([percent-full-mover L23-L36](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/tools/mergerfs.percent-full-mover#L23-L36)).
- A search of the mergerfs docs tree for "lsof", "in use", "open file" and "out-of-band" found **no guidance on moving open files**.
- Out-of-band branch changes are expected to "just work" with the default 1 s entry/attr caches ([cache docs](https://trapexit.github.io/mergerfs/latest/config/cache/)).

### 3.2 Options compared

| Method | Mechanism | Privileges | Cost | Races | Misses |
|---|---|---|---|---|---|
| `/proc/*/fd` readlink or **stat** | Enumerate fds and compare `(st_dev, st_ino)` via `stat` on the magic link; use `fdinfo` `flags` to tell writers apart ([proc_pid_fd(5)](https://man7.org/linux/man-pages/man5/proc_pid_fd.5.html), [proc_pid_fdinfo(5)](https://man7.org/linux/man-pages/man5/proc_pid_fdinfo.5.html)) | PTRACE_MODE_READ_FSCREDS. In practice root; **CAP_SYS_PTRACE** in Docker, because commoncap denies a caller whose capabilities are not a superset of the target's ([ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html)). Needs the host PID namespace. | O(all fds); one snapshot per run is cheap, per-file rescans are not | Snapshot only: opens after the scan are missed | Other PID namespaces, unless `--pid=host`. Passthrough backing files if the daemon closed its fd. Kernel-internal users (loop devices, swap). Readlink paths depend on the mount namespace (compare dev/ino instead). |
| `/proc/*/maps`, `map_files`, `cwd` | Path or dev:ino of mappings ([proc_pid_maps(5)](https://man7.org/linux/man-pages/man5/proc_pid_maps.5.html), [proc_pid_map_files(5)](https://man7.org/linux/man-pages/man5/proc_pid_map_files.5.html), [proc_pid_cwd(5)](https://man7.org/linux/man-pages/man5/proc_pid_cwd.5.html)) | Same as above | Higher (maps files can be large) | Snapshot | Only catches *direct* mmaps of branch paths. mmaps via mergerfs show up as mergerfs fds. |
| `/proc/locks` | Advisory POSIX/flock/OFD locks and leases, keyed by `maj:min:ino` ([proc_locks(5)](https://man7.org/linux/man-pages/man5/proc_locks.5.html)) | none, but filtered to the caller's PID namespace | trivial | Snapshot | Most writers do not lock. mergerfs has no lock handler, so the kernel records locks locally on the **FUSE inode** (`fc->no_lock` path, [fuse/file.c L2552-L2573](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/fuse/file.c#L2552-L2573)), not on the branch inode. That "no handler" conclusion comes from searching mergerfs `src/` and is **partly unverified**. |
| `lsof` / `fuser` | Wrappers over `/proc` ([lsof(8)](https://man7.org/linux/man-pages/man8/lsof.8.html), [fuser(1)](https://man7.org/linux/man-pages/man1/fuser.1.html)) | same as `/proc` | High (fork, full scan, parsing) | Snapshot | Same as `/proc`. Diagnostics only. |
| fanotify | `FAN_MARK_FILESYSTEM` (since 4.20) or `FAN_MARK_MOUNT` on the cache filesystem; `FAN_OPEN`/`FAN_CLOSE_WRITE`; `FAN_OPEN_PERM` can veto opens ([fanotify(7)](https://man7.org/linux/man-pages/man7/fanotify.7.html), [fanotify_mark(2)](https://man7.org/linux/man-pages/man2/fanotify_mark.2.html)) | **CAP_SYS_ADMIN** for filesystem/mount marks and permission events; unprivileged groups are limited ([fanotify_init(2)](https://man7.org/linux/man-pages/man2/fanotify_init.2.html)) | A long-running daemon; per-event cost | Event stream, so nothing is missed *while running* | **Cannot see files already open** at startup. The event pid is mergerfs's. Permission events block the opener, and a hung or buggy cachy stalls every open on the cache filesystem. Rust: [`nix::sys::fanotify`](https://docs.rs/nix/latest/nix/sys/fanotify/index.html). |
| inotify | Per-directory watches: `IN_OPEN`, `IN_CLOSE_WRITE`, `IN_MODIFY` ([inotify(7)](https://man7.org/linux/man-pages/man7/inotify.7.html)) | none | One watch per directory; `max_user_watches` limit | Event stream | No pid, no current-state query, recursion must be done by hand. Usable only as a "recently modified" hint. |
| **fcntl lease** `F_SETLEASE` | The kernel refuses the lease if conflicting opens exist and **breaks** it on later conflicting `open`/`truncate` ([F_SETLEASE(2const)](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/tree/man/man2const/F_GETLEASE.2const?id=c6cefe27dd02beaf518bb654493e5f09e62b3225); [fcntl(2)](https://man7.org/linux/man-pages/man2/fcntl.2.html)) | The file's owner, or **CAP_LEASE** ([locks.c L2136-L2148](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L2136-L2148)). CAP_LEASE is not in Docker's default set ([moby defaults.go](https://github.com/moby/moby/blob/c74ba95583051d0d49280d242b3b72bc3e54693d/daemon/pkg/oci/caps/defaults.go)). | One `fcntl` per file | Detects at acquisition and signals afterwards; there is an inherent window between break and release (see §3.4) | Does not break on `rename`/`unlink` (delegations do, see below). Requires filesystem support and `fs.leases-enable=1`. |

**Your understanding of leases is correct, with refinements:**
- **Write lease (`F_WRLCK`)** is granted only if no *other* `struct file` has the inode open for read or write. The kernel compares `i_writecount`/`i_readcount` against the caller's own fd ([locks.c L599-L635](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L599-L635)). It re-checks after insertion to close the lockless race ([L1973-L1986](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L1973-L1986)), and it fails with `EAGAIN` on a conflict.
- **Read lease (`F_RDLCK`)** is granted if nobody has the file open for **writing**, and it breaks when someone opens it for writing or truncates it. For data safety, writers are what matter. Readers of the unlinked source keep reading identical data; only the freeing of cache space is delayed.
- **Lease breaks:** a conflicting open **blocks in the kernel** and the holder gets **SIGIO** (or the signal set with `F_SETSIG`). If the holder does not unlock or downgrade within `/proc/sys/fs/lease-break-time` (default **45 s**, [locks.c L96-L97](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L96-L97)), the kernel revokes it. An opener using `O_NONBLOCK` gets `EWOULDBLOCK` instead.
- **Filesystem support:**
  - Since **Linux 7.0**, filesystems must opt in. A `NULL ->setlease` now returns `EINVAL` ([commit 2b10994be716](https://github.com/torvalds/linux/commit/2b10994be716), [locks.c L2107-L2116](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L2107-L2116)).
  - Opted in at `24a19b9`: ext4 ([file.c L1055](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/ext4/file.c#L1055)), XFS ([L2179](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/xfs/xfs_file.c#L2179)), btrfs ([L3838](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/btrfs/file.c#L3838)), tmpfs ([shmem.c L5188](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/mm/shmem.c#L5188)), FUSE ([L3095](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/fuse/file.c#L3095)), and OpenZFS ([zpl_file.c L1386](https://github.com/openzfs/zfs/blob/4542a317e2fa524e24c1c28b6fbca9cd3634ba56/module/os/linux/zfs/zpl_file.c#L1386)).
  - Before 7.0, `generic_setlease` was the default for every filesystem.
  - NFS and SMB clients use their own implementations; do not use them as a cache branch.
- **Delegations:** Linux **6.19** added userspace delegations (`F_SETDELEG`). They also break on `unlink`/`rename` of the opened dentry ([F_SETDELEG(2const)](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/tree/man/man2const/F_GETDELEG.2const?id=c6cefe27dd02beaf518bb654493e5f09e62b3225), [locks.c L2206-L2215](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/locks.c#L2206-L2215)). A write delegation requires a writable fd. Whether cachy's *own* rename or unlink breaks its own delegation is **unverified**.
- **Rust:** `nix` 0.31 `FcntlArg` has no lease variant, so use `libc::fcntl` with [`libc::F_SETLEASE`](https://docs.rs/libc/latest/libc/constant.F_SETLEASE.html). Tests can run unprivileged on tmpfs, because the test user owns the files.

### 3.3 Docker requirements

| Approach | Requirements |
|---|---|
| Lease approach | `cap_add: [LEASE]` (files owned by other uids), the cache **branch** bind-mounted. **No** host PID namespace needed, because the kernel counts every opener globally. |
| `/proc` scanning | `pid: host` ([docker run --pid](https://docs.docker.com/reference/cli/docker/container/run/)), `cap_add: [SYS_PTRACE]`. Possibly an AppArmor adjustment (**unverified**). |
| fanotify | `cap_add: [SYS_ADMIN]`, which upstream's compose file already adds. |

### 3.4 Closing the race: recommended move transaction

Per file, on the cache branch (src) and a chosen destination branch:

1. **Candidate filter:** skip if `mtime` or `ctime` is newer than `min_age` (for example 10 min). This catches writers that hold no fd between writes, such as NFSv3 (stateless).
2. Open src with `O_RDONLY|O_NOFOLLOW|O_NOATIME`. `O_NOATIME` needs the file's owner or CAP_FOWNER ([open(2)](https://man7.org/linux/man-pages/man2/open.2.html)); it keeps atime ordering intact on aborted moves. `fstat` it and record `(dev, ino, size, mtime_ns, ctime_ns, nlink)`.
3. `fcntl(F_SETLEASE, F_RDLCK or F_WRLCK)`:
   - `EAGAIN` ⇒ the file is in use: skip it.
   - `EINVAL` ⇒ no lease support: fall back to the `/proc` scan, or refuse, by policy.
   - Install the SIGIO/`F_SETSIG` handler (or signalfd) **before** taking the lease.
4. Optional belt-and-braces: a `/proc` scan for `(dev, ino)`.
5. Create the destination as `O_TMPFILE` in the destination directory. That needs ext4/XFS/btrfs/tmpfs ([open(2)](https://man7.org/linux/man-pages/man2/open.2.html)); OpenZFS support is **unverified**. Otherwise use a named temp file with `O_CREAT|O_EXCL` that cachy records in its journal. **Do not** delete by name pattern.
6. Copy the data. Preserve holes via `SEEK_DATA`/`SEEK_HOLE` ([lseek(2)](https://man7.org/linux/man-pages/man2/lseek.2.html)). Optionally hash while reading.
7. Copy metadata in this order: `fchown` → `fchmod` → xattrs/ACLs → `futimens`. Every failure is an **error**, unless a config flag says otherwise.
8. `fsync(dest)`. Optionally verify the hash by re-reading after `posix_fadvise(DONTNEED)`.
9. **Re-validate** before publishing:
   - the lease is still held (`F_GETLEASE`, no break pending);
   - `fstat(src_fd)` matches step 2;
   - `fstatat(src_dir, name, AT_SYMLINK_NOFOLLOW)` resolves to the same `(dev, ino)`. This catches rename-over (R2).
10. **Publish without clobbering:** `linkat(AT_EMPTY_PATH)` for `O_TMPFILE`, which fails with `EEXIST` if the name exists (needs CAP_DAC_READ_SEARCH, or `/proc/self/fd` with `AT_SYMLINK_FOLLOW`, [open(2)](https://man7.org/linux/man-pages/man2/open.2.html)). For a named temp file, use `renameat2(RENAME_NOREPLACE)` ([rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html)) on the **branch**.
    - mergerfs cannot do this: its `rename2` returns `ENOSYS` ([vendored fuse.cpp L2073-L2077](https://github.com/trapexit/mergerfs/blob/8e87bcbff2689872e61752737308570717af1a3a/vendored/libfuse/lib/fuse.cpp#L2073-L2077)), so the kernel answers `EINVAL` for any flags ([fuse/dir.c L1309-L1335](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/fuse/dir.c#L1309-L1335)). That alone is a strong reason to write to branches directly.
    - Then `fsync` the destination directory and any newly created parents.
11. **Retire the source**, still holding the lease. The duplicate state (dest durable, src present) is safe from here on.
    1. `renameat2(src → hidden marker, RENAME_NOREPLACE)` on the cache, so the step is reversible.
    2. Wait a short grace period (≈1 s) and re-check that no lease break is pending. An open that resolved the path before the rename blocks in `break_lease`, which would be visible now.
    3. If a break is pending: rename the marker back and remove the destination (only if `(dev, ino)` still matches cachy's file).
    4. Otherwise `unlink(marker)` and `fsync` the cache directory.
12. Release the lease and close.

**Residual risk:** an opener that completed its path lookup before step 11.1 but reaches `break_lease` only after the grace period. That requires the open to stall for longer than the grace period between lookup and `do_dentry_open`; it is not eliminable with leases alone. Delegations (6.19+) may remove it, but that is **unverified**.

**Crash recovery:** cachy keeps an **intent journal on the cache filesystem** that records `src`, the marker path, the destination branch and path, size and hash. On startup it reconciles from the journal only:
- If the destination is present and verified, remove the marker.
- Otherwise restore the marker.
- With `O_TMPFILE`, a crash cannot leave a temp file behind.

### 3.5 Verdict and recommendation

**Feasible, and it should be on by default.**
- **Primary:** leases (`F_RDLCK` by default, see the open question). They are inode-level and therefore cover mergerfs fds, passthrough backing files, other PID namespaces and direct openers; they cost one syscall; and they also give *post-acquisition* notification.
- **Secondary:** the min-age filter plus identity re-validation (this closes R2).
- **Optional:** a `/proc` scan for diagnostics (who holds the file?) and as a fallback when leases return `EINVAL`.
- **Avoid:** fanotify permission events, because a stall would freeze every cache open. inotify is too weak to rely on.

---

## 4. Feasibility: HDD spin-state detection and spin-minimising moves

### 4.1 Detection options

| Option | Spins up the disk? | Privileges | Notes |
|---|---|---|---|
| **ATA CHECK POWER MODE** (`0xE5`) via `hdparm -C` | No, by design. hdparm sends `0xE5` (retrying `0x98`) and decodes `0x00` standby, `0x80` idle, `0xFF` active/idle, `0x40`/`0x41` NV-cache ([hdparm-9.65 hdparm.c L2662-L2678](https://sourceforge.net/projects/hdparm/files/hdparm/hdparm-9.65.tar.gz); man page `-C`, [hdparm.8 L72-L83](https://sourceforge.net/projects/hdparm/files/hdparm/hdparm-9.65.tar.gz)). It opens the device `O_RDONLY\|O_NONBLOCK` (L167). | root (raw device access) | The kernel itself uses CHECK POWER MODE to test "active or idle" without waking the disk on resume (`ata_dev_power_is_active`, [libata-core.c L2007-L2032](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/drivers/ata/libata-core.c#L2007-L2032); [commit 2da4c5e24e86](https://github.com/torvalds/linux/commit/2da4c5e24e86313fc09596ae775123b41055fd54)). The ACS spec text saying "shall not change power mode" could not be read (paywalled), so that wording is **unverified**. |
| `smartctl -n standby` | No, if `-d TYPE` is given. Device-type autodetection may spin the disk up ([smartctl.8.in L898-L945](https://github.com/smartmontools/smartmontools/blob/cf64246ba9a0f8c2c210b92b2b3b64fe3679358c/src/smartctl.8.in#L898-L945)) | root | Uses `ata_check_power_mode` → ATA pass-through `0xE5` ([atacmds.cpp L521-L540](https://github.com/smartmontools/smartmontools/blob/cf64246ba9a0f8c2c210b92b2b3b64fe3679358c/lib/atacmds.cpp#L521-L540)). Disks in **SLEEP** do not answer, so a failure must be read as "asleep" ([smartmontools Powermode](https://www.smartmontools.org/wiki/Powermode)). |
| SG_IO ATA PASS-THROUGH(16) from Rust | No (same command) | **CAP_SYS_RAWIO**: `ATA_16` is not in the unprivileged SG_IO allow-list ([scsi_ioctl.c L270-L299](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/drivers/scsi/scsi_ioctl.c#L270-L299)). Also needs an fd on `/dev/sdX`. | Crates: [`hdd`](https://github.com/vthriller/hdd-rs) 0.10.3 (last release 2020) and [`sg`](https://github.com/drrlvn/sg) 0.3.0 (2018). Both are stale. `libc` does not appear to export `sg_io_hdr`/`SG_IO` (a GitHub code search found none). Recommendation: a ~100-line `#[repr(C)] sg_io_hdr` + `ioctl(0x2285)` module, or shell out to `hdparm -C` as the MVP. |
| SAS/SCSI: REQUEST SENSE | No | `REQUEST_SENSE` **is** allowed unprivileged ([scsi_ioctl.c L280](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/drivers/scsi/scsi_ioctl.c#L280)) | Sense ASC `0x5E` with ASCQ `0x00`–`0x0A` encodes idle or standby conditions; anything else means active ([smartctl scsiprint.cpp L3518-L3592](https://github.com/smartmontools/smartmontools/blob/cf64246ba9a0f8c2c210b92b2b3b64fe3679358c/src/scsiprint.cpp#L3518-L3592); [sg_requests.8 L98-L106](https://github.com/doug-gilbert/sg3_utils/blob/1929463554c5dc749c5a89bcefb2c88a08d45575/doc/sg_requests.8#L98-L106)). libata does **not** translate REQUEST SENSE into a power state for SATA disks; `grep` of `libata-scsi.c` finds no CHECK POWER MODE use. |
| USB/SAT bridges | Varies | as above | Needs bridge-specific pass-through (`-d sat`, `usbjmicron`, …, [smartctl.8.in L438-L520](https://github.com/smartmontools/smartmontools/blob/cf64246ba9a0f8c2c210b92b2b3b64fe3679358c/src/smartctl.8.in#L438-L520)). Some bridges support none. Fall back to passive detection. |
| **`/sys/block/<disk>/stat`** (passive) | **Never** (no device access) | none | Contains read/write/discard/flush counters, `in_flight` and `io_ticks` ([stat.rst](https://docs.kernel.org/block/stat.html)). A change between polls means the disk did I/O in that interval, so it was spinning. It cannot see a drive spinning *down* on its own timer, so the state has to be inferred from the configured spindown timeout. **Passthrough commands are not counted by default** (`blk_account_io_start` skips them unless `queue/iostats_passthrough=1`, [blk-mq.c L1103-L1110](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/block/blk-mq.c#L1103-L1110)), so CHECK POWER MODE polling does not disturb counter-based idle tools. |
| Runtime PM `power/runtime_status` | No | none | Reports the kernel runtime-PM state ("active"/"suspended"/"unsupported", [sysfs-devices-power](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/Documentation/ABI/testing/sysfs-devices-power#L264-L271)). It knows nothing about drive-internal standby timers, `hdparm -S` or `hdparm -y`, so it is only valid if spindown is done via kernel runtime PM. Not a general solution. |
| How hd-idle decides | n/a | n/a | It snapshots `/proc/diskstats` **sectors read/written** (columns 6/10) and treats "unchanged for `IdleTime`" as idle. It then issues the spindown itself and *assumes* a spun-down state until counters move ([hdidle.go L123-L178](https://github.com/adelolmo/hd-idle/blob/0c42d8cca82f1fefd255616e339e2070c16cf90e/hdidle.go#L123-L178), [snapshot.go L67-L71](https://github.com/adelolmo/hd-idle/blob/0c42d8cca82f1fefd255616e339e2070c16cf90e/diskstats/snapshot.go#L67-L71)). It never queries drive state. SnapRAID has a `probe` command that reports standby or active ([SnapRAID manual](https://www.snapraid.it/manual)). |

The mergerfs author warns that "some drives actually will spin up if you ask if they are spun down" ([limit_drive_spinup](https://trapexit.github.io/mergerfs/latest/faq/limit_drive_spinup/)). That is first-party for mergerfs but **secondary** for drive behaviour, and **unverified** for specific models. Any active probe should therefore be opt-in per disk.

### 4.2 Mapping the pool to physical disks

1. **Branches:** read `getxattr("<BACKING_PATH>/.mergerfs", "user.mergerfs.branches")`. The value is `path=MODE[,minfreespace]:…`, for example `/mnt/hdd/disk0=RW:/mnt/hdd/disk1=RW` ([runtime interface](https://trapexit.github.io/mergerfs/latest/runtime_interface/)).
   - Respect `RO`/`NC` modes and `minfreespace` (from `user.mergerfs.minfreespace`) the way create policies do ([policies → filtering](https://trapexit.github.io/mergerfs/latest/config/functions_categories_policies/)).
   - With `xattr=nosys`, the control file stops working ([xattr](https://trapexit.github.io/mergerfs/latest/config/xattr/)). In that case cachy needs an explicit `branches` config.
2. **Branch → block device:** read `st_dev` of the branch root (equal to mountinfo field 3 `major:minor`, [proc_pid_mountinfo(5)](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html)), then follow `/sys/dev/block/MAJ:MIN` → the sysfs node ([sysfs-dev ABI](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/Documentation/ABI/testing/sysfs-dev)).
   - A partition's parent disk is the parent directory of that sysfs node (`/sys/block/<disk>/<part>`, see the `stat` ABI at [sysfs-block L212-L218](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/Documentation/ABI/stable/sysfs-block#L212-L218)).
3. **Stacked devices (dm-crypt, LVM, md):** recurse through `/sys/block/<dev>/slaves/*` (created in [genhd.c L520-L525](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/block/genhd.c#L520-L525)) down to leaf disks. The `slaves` directory is not in the ABI docs, so treat it as stable-by-practice. hd-idle does the inverse lookup through `/sys/class/block/<disk>/holders/` for `dm-*` ([snapshot.go L113-L190](https://github.com/adelolmo/hd-idle/blob/0c42d8cca82f1fefd255616e339e2070c16cf90e/diskstats/snapshot.go#L113-L190)).
   - A branch on md RAID or LVM spanning N disks is "active" only if **all** its leaves are. A write wakes every leaf it touches.
4. **btrfs:** `st_dev` is an anonymous device, so use `/sys/fs/btrfs/<fsid>/devices/` ([btrfs sysfs.c L2266-L2276](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/btrfs/sysfs.c#L2266-L2276)).
   - **ZFS:** out-of-tree, with no sysfs mapping, so it needs `zpool status -P` or `zpool list -vHP`. A ZFS pool is a single "branch" whose writes stripe across vdevs. **Spin-aware placement is meaningless inside a raidz pool.**

### 4.3 How writes wake disks, and what cachy must control

- **Create policies:** mergerfs picks the branch for a new file by policy. The default `create` is **`pfrd`**, which is random, weighted by free space. `mfs`, `lfs`, `lus` and `lup` are space-based, `ff` uses branch order, and `ep*`/`msp*` follow existing parent paths ([policies](https://trapexit.github.io/mergerfs/latest/config/functions_categories_policies/)). **None is spin-aware.** Writing through the pool therefore routinely lands on a sleeping disk while another disk is spinning.
  - Before the create, mergerfs also clones the parent path onto the chosen branch, and search policies probe branches in order.
- **Directory operations:** `readdir` fans out to **all** branches. mergerfs's FAQ states plainly that it "makes [spin-up] worse" and that querying only active drives "isn't plausible" without caching all metadata ([limit_drive_spinup](https://trapexit.github.io/mergerfs/latest/faq/limit_drive_spinup/)).
  - `stat`, `mkdir` and `readdir` hit the disk only on dentry/inode/page-cache misses. `vfs_cache_pressure` affects that, but **nothing guarantees a hit**.
  - `statfs` on ext4 reads in-memory per-cpu counters ([ext4 super.c L6987-L7015](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/fs/ext4/super.c#L6987-L7015)), so policy free-space checks do not wake ext4 disks. Project quotas aside, that is.
- **Upstream specifically:**
  - `os.walk(BACKING_PATH)` runs on every run (§1.6), which means readdir across every directory on every branch.
  - `os.path.exists`/`makedirs` go through the pool.
  - Empty-directory removal on the cache changes where `ep*` policies place future files: if the path no longer exists on the cache, the HDD that still has it is chosen.
- **Therefore cachy should choose the destination branch itself and write to `<branch>/<relpath>` directly.** This is the "out-of-band" pattern mergerfs supports ([cache docs](https://trapexit.github.io/mergerfs/latest/config/cache/)), and upstream already does it in its hardlink fallback. Constraints:
  - Honour branch mode and `minfreespace`.
  - Keep hardlink groups on one branch.
  - Prefer branches where the parent directory already exists, to keep `ep*` semantics.
  - Writing to branches directly also enables `RENAME_NOREPLACE`, `O_TMPFILE` and real directory `fsync` (§3.4).
- **The irreducible conflict:** deciding that `relpath` does not already exist on *any* branch requires a lookup on sleeping disks.
  - If it is skipped and an older copy exists on a disk earlier in the cache pool's search order, then after the move the pool may serve the **old** file (`ff` search). That is a silent rollback.
  - mergerfs's author reaches the same conclusion. See open question Q6.
- **SnapRAID:** the parity disk is not a mergerfs branch, and cachy never touches it. It is written only by `snapraid sync`, which reads modified files ([manual](https://www.snapraid.it/manual)). Whether sync must spin unmodified data disks to recompute parity is **unverified**.
  - Newly moved files are unprotected until the next sync.
  - **Replacing** existing files reduces recoverability of *other* files until the sync completes ([manual, Limitations](https://www.snapraid.it/manual)).
  - Issue [#27](https://github.com/monstermuffin/mergerfs-cache-mover/issues/27) (open) asks for a post-move hook, such as `snapraid sync`.
- **Privileges:**

  | What | Requirement |
  |---|---|
  | Passive sysfs | None. Docker mounts `/sys` read-only; host block devices being visible there is **unverified**. |
  | Active ATA probe | CAP_SYS_RAWIO plus device access: `--device /dev/sdX` and `cap_add: [SYS_RAWIO]` ([docker run](https://docs.docker.com/reference/cli/docker/container/run/)). |
  | SAS REQUEST SENSE | Device access only. |
  | Branch-direct writes | Branches must be bind-mounted at identical paths, as upstream already requires for hardlinks ([README L179-L182](https://github.com/monstermuffin/mergerfs-cache-mover/blob/e48c80cc40a91a51da0467e35f187fda120b03e4/README.md#L179-L182)). |

### 4.4 Verdict and recommended design

**Feasible.** Detection is reliable with passive counters plus an optional CHECK POWER MODE probe. The cost is that cachy owns branch selection, and that a strict no-wake guarantee conflicts with duplicate-path safety (Q6).

**Trigger design:**
- **Poll every 30–60 s.** Read `/sys/block/<leaf>/stat` for each destination leaf disk. A disk is `active` if read or write I/Os changed in the last poll, or if `now - last_io < spindown_timeout - margin`, where the user provides `spindown_timeout` and `margin` is for example 2 min.
  - Optionally confirm with CHECK POWER MODE (`0xFF`/`0x80` = spinning) when CAP_SYS_RAWIO is available. Do not count the probe as I/O; it is not counted (blk-mq).
- **Debounce:** require activity that cachy did not cause. Ignore counter deltas while cachy's own job runs, plus a cooldown of one poll interval afterwards. Self-triggering also stops naturally, because the job ends at `target`.
- **Rules:**
  1. **Hard threshold** (`usage ≥ THRESHOLD`, upstream semantics): move now. Prefer active disks; if none are active, wake **one** disk (the best by policy) and fill it up to target.
  2. **Opportunistic threshold** (`usage ≥ OPPORTUNISTIC`, which is below THRESHOLD and new): move only if at least one eligible destination disk is `active`, write **only** to active disks, and stop at `target`, at the cache-side minimum, or when no active disk has room.
  3. **Schedule mode** (cron or a systemd timer): keep upstream's behaviour as a fallback, for example a nightly empty-cache run.
- **No backing-pool walks.** Run recovery from the journal (§3.4), never with `os.walk(BACKING_PATH)`. Group directory creation per branch.
- **Parallelism:** one writer per destination disk (HDD seek thrash); parallelism across disks is fine. MAX_WORKERS then caps disks × streams.

---

## 5. Testing approach (short)

- **Integration tests on tempdirs** using real syscalls. Lease semantics work unprivileged on tmpfs, since tmpfs opts in ([shmem.c L5188](https://github.com/torvalds/linux/blob/24a19b953e2a3d0ea5a8c4951a0ab8f32720c907/mm/shmem.c#L5188)) and the tests own the files.
  - Each upstream bug in §2 becomes a regression test: rename-over during copy, a held fd, a dotfile matching `.x.abcdef`, an existing destination, and a crash at every step.
- **Real ENOSPC:** tmpfs with `size=`/`nr_inodes=` ([tmpfs](https://docs.kernel.org/filesystems/tmpfs.html)), or loop-mounted small ext4/XFS images, run in a privileged CI job. A mergerfs-over-tmpfs fixture (the mergerfs docs' own benchmark setup) covers branch discovery.
- **Fault injection behind a filesystem trait.** Every syscall the mover makes (`openat`, `copy`, `fsync`, `linkat`, `renameat2`, `unlinkat`, `fcntl`) goes through a narrow trait with a real implementation and a scripted fake that injects `EIO`/`ENOSPC`/`EXDEV`/`EINTR` at the Nth call. Alternatively use failpoints via [`fail`](https://github.com/tikv/fail-rs) (`failpoints` feature; global state, so tests must be serialised).
  - At kernel level: [dm-flakey](https://docs.kernel.org/admin-guide/device-mapper/dm-flakey.html) and [fault-injection](https://docs.kernel.org/fault-injection/fault-injection.html).
- **Property tests** ([proptest](https://proptest-rs.github.io/proptest/intro.html)):
  - Generate random trees (names with odd bytes, hardlinks, symlinks, sparse files, exclusion lists).
  - Invariants: (a) every byte that existed before the run exists after it, in exactly one visible location per relpath, with equal content and metadata; (b) excluded paths are untouched; (c) usage ≤ target or no eligible files remain.
  - Model-based: compare against a pure-Rust reference model of the selection and exclusion logic.
- **Crash consistency:**
  - Kill-at-every-step: fork, then SIGKILL at injected points, then run recovery, then check the invariants.
  - Power-loss: [dm-log-writes](https://docs.kernel.org/admin-guide/device-mapper/log-writes.html) to replay to every flush point.
  - Rationale: the ALICE study shows applications commonly assume ordering that filesystems do not give ([Pillai et al., OSDI'14](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)).
- **Coverage:** [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) with `--fail-under-lines`/`--fail-under-regions` and `--branch` in CI.
- **Mutation testing:** [cargo-mutants](https://mutants.rs/). Gate on zero *missed* mutants in the mover, recovery and exclusion modules.
- **Golden oracle:** run upstream at the pinned commit inside a container against generated trees and diff the resulting trees and logs.
  - Do this **only** for the behaviours in §1 that are kept as-is: config precedence, exclusion matching, threshold/target arithmetic, empty-directory removal and notification triggers.
  - Explicitly mask the §2 bugs. The selection order is not reproducible, because it depends on directory order.

---

## 6. Summary

### 6.1 Feature recommendations

| Upstream feature | Recommendation | Note |
|---|---|---|
| YAML config + env overrides, same key names | Port with fix | Keep names and precedence for drop-in compatibility. Add typed validation (bools, 0–100 ranges, `MAX_WORKERS ≥ 1`, paths exist and are distinct). Decide whether env EXCLUDED_DIRS should merge or replace. |
| `--config`, `--dry-run`, `--console-log` | Port as-is | Make dry-run honour the threshold and simulate usage decreasing (fix); add `--version`. |
| Threshold/target and 0/0 empty mode | Port as-is | Strict `>` threshold and `<=` target semantics. |
| Selection order | **Needs decision** | Upstream has none since v1.3. Choices: atime (mergerfs reference, unreliable under `noatime`/`relatime`, [mount(2)](https://man7.org/linux/man-pages/man2/mount.2.html)), mtime (v1.2 behaviour), or ctime. |
| Usage metric | Port as-is (document it) | `(blocks-bfree)/blocks`, not `df`'s formula. |
| EXCLUDED_DIRS + hardcoded SnapRAID list | Port with fix | Anchor path patterns to the cache root, match file names too (snapraid.content), prune the walk. Decide whether to keep matching against CACHE_PATH components (Q4). |
| Skip zero-byte and special files | Port as-is | Document it. |
| Hardlink groups + branch-direct fallback | Port with fix | Key on `(st_dev, st_ino)`, `-xdev` walk, whole group on one branch, transactional. |
| SKIP_HARDLINKED_FILES | Port as-is | |
| Symlink moves | Port with fix | No `os.remove(dest)` of real files. Target rewriting → **needs decision** (Q5). Preserve the symlink owner (`lchown`). |
| Temp-name + rename atomic publish | Port with fix | `O_TMPFILE`/`linkat` or `RENAME_NOREPLACE` on the branch, fsyncs, identity re-check, publish before unlink. |
| `.deleted.` markers + pattern-based orphan cleanup | **Drop / replace** | Use a journal-driven recovery on the cache filesystem. Never delete by name pattern. |
| Metadata copy | Port with fix | Errors become failures. Preserve sparseness, directory mtimes, xattrs and ACLs. |
| Free-space check | Port with fix | Per-branch check including `minfreespace`. |
| Clobbering an existing destination | **Needs decision** (Q3) | |
| MAX_WORKERS thread pool | Port with fix | One writer per destination disk. |
| Empty-directory cleanup + KEEP_EMPTY_DIRS | Port with fix | Default to removing only directories emptied by *this* run (Q7). |
| Instance lock + INSTANCE_ID | Port with fix | Replace with `flock` on a lockfile named after INSTANCE_ID (e.g. under `/run`). |
| Logging, rotation, LOG_LEVEL | Port with fix | Keep the levels. Rotation: native file rotation or defer to journald/logrotate (Q8). Don't log third-party debug output by default. |
| Notifications (Apprise, Discord, Slack embeds) | **Needs decision** (Q9) | Apprise has no Rust equivalent. Options: native Discord/Slack/generic webhook, or shell out to the `apprise` CLI. Fix: units, Slack URL parsing, timeouts. |
| NOTIFY_THRESHOLD, completion and error notifications | Port as-is (event semantics) | Don't send "completion" for 0 moved files (fix, or keep for parity?). |
| AUTO_UPDATE / UPDATE_BRANCH | **Drop** | Remote code execution as root; use package or image updates instead. |
| Docker image + cron entrypoint + SCHEDULE | Port with fix | Use a small init (or cachy as PID 1 with a daemon/scheduler mode) so SIGTERM is delivered. Don't run a move on every start. Document the capabilities: LEASE; optionally SYS_RAWIO, SYS_PTRACE and `pid: host`. |
| HEALTHCHECK | Port with fix | Health of cachy's own scheduler. |
| Commit hash in notifications | Drop | Use the cachy version instead. |

**New features**

| New feature | Recommendation | Note |
|---|---|---|
| Open-file guard (leases + min-age + re-validation) | New, default on | See §3. |
| Spin-aware trigger and branch-direct placement | New, opt-in | See §4. |
| Post-move hook | New | Upstream issue #27, e.g. `snapraid sync`. |
| Age-based moving | Needs decision | Upstream issue #4. |

### 6.2 Open questions for the design interview

1. **Lease strength:** `F_RDLCK` (only writers block a move, so files being streamed can still move) or `F_WRLCK` (any open blocks)? And the behaviour when leases are unsupported (`EINVAL`): refuse, fall back to `/proc`, or proceed?
2. **min_age** default, and whether it uses mtime, ctime or both.
3. **Existing destination:** fail and keep both, replace (the upstream semantics, which also delete copies on other branches), keep the newer by mtime, or move the old copy aside?
4. **Exclusion compatibility:** match exclusion patterns relative to CACHE_PATH (the fix) or keep the absolute-path matching for exact parity?
5. **Symlinks:** keep upstream's "relative → absolute BACKING_PATH" rewrite, or preserve link text verbatim (which mergerfs resolves identically in the pool)?
6. **Spin-safety vs duplicate-path safety:** before writing to an active disk, should cachy look up `relpath` on sleeping branches (possible wake-up), or trust that duplicates cannot exist? Alternatively, maintain an index refreshed only while disks spin.
7. **Empty-directory policy:** remove only directories emptied by this run, all empty directories (upstream), or none?
8. **Run model:** one-shot CLI + cron/systemd timer (upstream), a long-running daemon (required for polling spin state), or both? This also decides how logging and rotation work.
9. **Notifications:** is Apprise parity required (shell out to the Python `apprise` CLI), or are native Discord/Slack/webhook/ntfy enough?
10. **Ordering key** for selection: atime, mtime or ctime, plus a tie-breaker. Should hardlink groups use the oldest or the newest member?
11. **Verification depth:** size only, a source hash during the copy, or a full read-back hash of the destination after `fsync` (doubles HDD read time)?
12. **Active probing:** allow SG_IO CHECK POWER MODE (needs CAP_SYS_RAWIO and per-disk opt-in, since some drives may wake), or passive counters only?
13. **Branch-direct writes as the default** when BACKING_PATH is a mergerfs pool, or keep "write through the pool" as an option for users whose create policy matters to them?
14. **Config compatibility:** must cachy read an unmodified upstream `config.yml` and the same env vars (a drop-in), or may it rename or restructure keys?
15. **Multiple cache disks:** one instance per cache branch (upstream), or a single cachy managing every cache branch of a pool?

### 6.3 Not verified

- The ACS spec wording for CHECK POWER MODE (paywalled).
- Per-model wake-on-query behaviour (only mergerfs's FAQ claims it).
- Sparse-file behaviour of upstream's copy (inferred from CPython code; not run).
- Per-subvolume inode namespaces on btrfs (R8).
- Whether mergerfs implements no lock ops at all (searched `src/` only).
- AppArmor's effect on reading `/proc` of host processes from Docker.
- Host block devices being visible in Docker's `/sys`.
- `O_TMPFILE` on OpenZFS.
- Self-breaking of `F_SETDELEG` on the holder's own unlink.
- Whether `snapraid sync` reads unmodified disks.
- The cause of issue #37.
