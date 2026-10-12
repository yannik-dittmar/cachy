# cachy

cachy drains a fast mergerfs cache branch onto slower backing branches once the cache fills up, without losing data that is still in use.

## Language

### Storage layout

**Cache pool**:
The mergerfs mount that applications use, made of the cache branch (listed first) plus the backing branches.
_Avoid_: pool, mergerfs mount

**Cache branch**:
The fast filesystem that cachy drains; one cachy instance owns exactly one.
_Avoid_: cache disk, cache path, CACHE_PATH, SSD, cache

**Backing pool**:
The set of slow branches that receive moved files, optionally also mounted as its own mergerfs pool; a plain single filesystem is a backing pool with one branch.
_Avoid_: storage pool, array, BACKING_PATH

**Backing branch**:
One slow filesystem in the backing pool, and the only place cachy ever writes moved data.
_Avoid_: HDD, data disk, drive

**Disk**:
A physical block device underneath a backing branch; one backing branch may span several disks.
_Avoid_: using "disk" for a branch

**Backing disks**:
The user-configured set of disks under the backing pool that must all be awake before cachy writes to it.
_Avoid_: array, spin group

**Awake**:
The state of the backing disks when every one of them is spun up.
_Avoid_: active, spinning, online

### Moving

**Move unit**:
The smallest thing cachy relocates as a whole: a regular file, a hardlink group, or a symlink.
_Avoid_: task, item, file (when a hardlink group or symlink is meant)

**Move**:
One transaction that relocates a single move unit from the cache branch to one backing branch, after which it exists on exactly one of them.
_Avoid_: copy, transfer, migration

**In use**:
A move unit is in use while any process has it open for writing; in-use move units are never moved, and a move unit that becomes in use mid-move aborts that move.
_Avoid_: busy, locked, open (a file open only for reading is not in use)

**Stale copy**:
A file on a backing branch at the same relative path as a file on the cache branch, hidden from applications by the cache branch's copy; a move replaces it.
_Avoid_: conflict, duplicate, shadow

### Runs

**Run**:
One evaluation of the trigger followed by moves until the stop condition is reached.
_Avoid_: job, pass, cycle

**Free space**:
The space on the cache branch that applications can still write into; the only measure of how full the cache is.
_Avoid_: usage, Use%, used space (these drop on snapshot-holding filesystems even when no space is freed)

**Threshold**:
The free space below which a run starts moving.

**Target**:
The free space at which a run stops moving.

**Byte budget**:
The amount a run must free to reach the target, fixed when the run starts and consumed by the allocated size of each completed move.

**Max age**:
An optional age past which a move unit is moved in any run, regardless of the threshold.

**Drain**:
A run that ignores the threshold and moves every eligible move unit.
_Avoid_: empty-cache mode, flush

**Wake trigger**:
A condition that starts a run because the backing disks have become awake.

**Wake gate**:
A condition that holds back an already-scheduled run until the backing disks are awake.
_Avoid_: using "trigger" for something that only delays a run

**Run hook**:
A user-configured command that cachy executes after each run, handing it the run summary.
_Avoid_: notification, callback
