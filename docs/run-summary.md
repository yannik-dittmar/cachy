# Run summary

After each run, cachy hands the run hook a JSON document on stdin: the run summary. This file documents its schema. `schema_version` changes whenever a change could break a reader, such as a removed or renamed key or a changed meaning. A new key does not change it, so readers should ignore keys they do not know.

## When there is a summary

A run follows a check that found work (`cachy run`), or skips the check (`cachy run --drain`). Each run produces a summary.

A check that finds no work is not a run, so it produces none, unless the invocation still failed (exit code 2). That happens when recovery leaves a leftover unresolved, or when cachy stops before the check. A dry run never produces a summary. Neither does an invocation that cachy refused to run (exit code 1): an invalid configuration, a failed capability probe, an invalid layout or another run holding the instance lock. systemd's `OnFailure=` catches those.

## Schema version 1

```json
{
  "schema_version": 1,
  "cachy_version": "0.1.0",
  "reason": "manual",
  "outcome": "moved",
  "cache_branch": "/mnt/cache",
  "free_before": 53687091200,
  "free_after": 214748364800,
  "byte_budget": 161061273600,
  "moved": [
    {"paths": ["media/movie.mkv"], "branch": "/mnt/backing", "bytes": 4294967296},
    {"paths": ["photos/a.jpg", "albums/a.jpg"], "branch": "/mnt/backing", "bytes": 3145728}
  ],
  "skipped": [
    {"paths": ["downloads/file.iso"], "reason": "in_use"}
  ],
  "failed": [
    {"paths": ["docs/report.odt"], "error": "copy: Input/output error (os error 5)"}
  ],
  "stale_copies": [
    {"path": "/mnt/backing/media/movie.mkv", "fate": "replaced"}
  ],
  "moved_bytes": 4298113024,
  "duration_seconds": 61.53,
  "throughput_bytes_per_second": 71968112.4,
  "stopped": false,
  "unresolved_leftovers": 0,
  "undated_dirs": 0
}
```

| Key | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | The version of this schema: `1`. |
| `cachy_version` | string | The version of cachy that ran, as `cachy --version` prints it. |
| `reason` | string | How cachy was invoked: `manual` (`cachy run`), `drain` (`cachy run --drain`), `schedule` (a scheduled check of `cachy daemon`) or `wake_trigger` (a wake-triggered check of `cachy daemon`). |
| `outcome` | string | How the run ended: `moved` (nothing failed, and at least one move unit moved), `nothing_moved` (nothing failed, and nothing moved) or `failed` (cachy exits with code 2). The run hook's `on` filter selects runs by this value. |
| `cache_branch` | string | The cache branch's path. |
| `free_before` | integer or null | The cache branch's free space, in bytes, when the run started; `null` if it could not be measured. |
| `free_after` | integer or null | The cache branch's free space, in bytes, when the run ended, after waiting for frees to show; `null` if the run did not get as far as moving, or the space could not be measured. |
| `byte_budget` | integer or null | How many bytes the run set out to free; `null` for a drain, which has no budget, and when the invocation ended before a run set one, such as a failed invocation whose check found no work. `0` when only move units past the max age are due. |
| `moved` | array | The move units that moved, in the order their moves finished. |
| `moved[].paths` | array of strings | The move unit's paths, relative to the cache branch: one, or each name of a hardlink group. |
| `moved[].branch` | string | The backing branch that it moved to. |
| `moved[].bytes` | integer | Its size (`st_size`). |
| `skipped` | array | The move units that the run left on the cache branch, untouched. |
| `skipped[].paths` | array of strings | As for `moved`. |
| `skipped[].reason` | string | The reason code: `in_use` (the in-use guard found it in use), `changed` (it changed during its move), `too_young` (it changed within the minimum age), `special_file` (a FIFO, socket or device node), `partly_excluded` (a hardlink group with names that the walk did not find), `no_room` (no backing branch has room for it) or `stopping` (cachy was asked to stop). |
| `failed` | array | The move units whose moves failed. |
| `failed[].paths` | array of strings | As for `moved`. |
| `failed[].error` | string | Why the move failed, as cachy logs it. |
| `stale_copies` | array | The stale copies that moves dealt with. That includes those replaced or removed by a move that was then skipped or failed, which cannot bring them back. |
| `stale_copies[].path` | string | The stale copy's path on its backing branch. |
| `stale_copies[].fate` | string | `replaced` (the moved copy took its place; if that move was then skipped or failed, the moved copy is gone too, and so is the stale copy's content), `removed` (it was on another backing branch, and removed from there) or `kept` (it was identical, so it became the moved copy). |
| `moved_bytes` | integer | The sizes of the moved move units, added up. |
| `duration_seconds` | number | How long the run took, from when cachy opened the cache branch (the capability probe and recovery included) to the end of the run, before the hook. |
| `throughput_bytes_per_second` | number | `moved_bytes` divided by the time spent moving, which leaves out the steps before the first move and after the last; `0` if nothing moved. |
| `stopped` | boolean | The run stopped early: cachy was asked to stop (`SIGTERM`, `SIGINT`), or an error outside any move stopped it. |
| `unresolved_leftovers` | integer | How many leftovers of unfinished moves recovery could not resolve. |
| `undated_dirs` | integer | How many directories that moves created on backing branches could not be given the times of their cache-branch directories. |

JSON strings are Unicode, so a path with bytes that are not valid UTF-8 shows each such byte as U+FFFD (�).
