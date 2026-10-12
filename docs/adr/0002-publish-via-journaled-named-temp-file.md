# Publish moves via a journaled named temp file, not O_TMPFILE

A move writes its copy to a named temp file (`O_CREAT|O_EXCL`) in the destination directory, `fsync`s it, and publishes it with `renameat2(RENAME_NOREPLACE)`. The temp file's path is recorded in the intent journal before the file is created, so crash recovery deletes exactly that path and never deletes anything by name pattern. We rejected `O_TMPFILE` + `linkat`, which would leave no temp files behind after a crash: on OpenZFS, linking a tmpfile is not ZIL-logged and waits for a full transaction-group sync (seconds per file), which makes moving many small files impractically slow. The named-temp path is fast on every supported filesystem, and one code path is easier to test for crash consistency.

## Consequences

- A temp file is briefly visible in the cache pool under its temp name during each move.
- The journal is mandatory, not optional: it is the only thing that may delete temp files.
- Filesystems without `RENAME_NOREPLACE` (for example OpenZFS < 2.2.0) are unsupported. The startup capability probe refuses to run on them.
