# Publish moves through per-filesystem staging directories, with no journal

A move writes its copy to a named temp file (`O_CREAT|O_EXCL`) inside a staging directory at the root of the destination filesystem. It `fsync`s the file and publishes it into place with `renameat2(RENAME_NOREPLACE)`. Retiring the source likewise renames it into the cache branch's staging directory before it is unlinked. The filesystem itself is the only state: every step is idempotent, so crash recovery means listing each staging directory and resolving each leftover against its published counterpart, and then simply running again. A retired source is deleted if it is identical to its published counterpart and not in use, and renamed back otherwise. A temp file is always deleted: a source is retired only after its copy is published, so a temp file is never the only copy, and renaming a partial one into place could expose a truncated file.

## Considered options

- **`O_TMPFILE` + `linkat`.** It leaves nothing behind after a crash, but on OpenZFS the link is not ZIL-logged and waits for a full transaction-group sync, which takes seconds per file.
- **An intent journal.** It finds leftovers exactly, but it is a second source of truth to keep consistent. The filesystem state already determines the correct recovery (a duplicate relpath is just a stale copy), so the journal would only locate leftovers, and a staging directory does that too.
- **Temp files next to the destination, found by name pattern.** This is upstream's approach. It deletes user files that happen to match the pattern, and finding them needs a walk of the whole backing pool.

## Consequences

- Each staging directory is visible at the root of the cache pool while a run is active, or after a crash until the next run. cachy removes it once it is empty.
- A directory that cachy creates on a branch is built with its metadata in `<staging directory>/.cachy-staging`, which mirrors no relpath, and renamed into place with `RENAME_NOREPLACE`, so it never shows without its metadata. Recovery removes such a directory if a crash leaves it behind, empty. Its mtime is set only at the end of the run, which keeps no record of it: after a crash, the directory keeps the mtime of its last change.
- A branch with nested filesystems (for example child ZFS datasets) gets one staging directory per filesystem, because renames cannot cross filesystems.
- A crash between publish and retire leaves no leftover, only the relpath on both branches. Recovery cannot find this without walking the backing pool. The next move of that relpath finds an identical file at the destination and adopts it instead of publishing.
- A symlink cannot be fsynced, so a symlink move makes its copy durable by fsyncing the directories that hold it. On ext4 and XFS that commits the copy's owner and timestamps too; on ZFS it may not. A crash within seconds of a symlink move can then leave the published symlink with its link text intact but with cachy's owner and the time of the move. Its source is gone by then, so recovery has nothing to restore them from. Syncing the whole backing filesystem before retiring symlinks would close this window, but costs too much for how rarely it matters.
- Filesystems without `RENAME_NOREPLACE` (for example OpenZFS < 2.2.0) are unsupported. The startup capability probe refuses to run on them.
