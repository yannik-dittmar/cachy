# cachy

cachy drains a fast mergerfs cache branch onto slower backing branches once the cache fills up, without losing data that is still in use.

## Language

### Storage layout

**Cache pool**:
The mergerfs mount that applications use, made of the cache branch plus the backing branches.
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

### Moving

**Move unit**:
The smallest thing cachy relocates as a whole: a regular file, a hardlink group, or a symlink.
_Avoid_: task, item, file (when a hardlink group or symlink is meant)

**Move**:
One transaction that relocates a single move unit from the cache branch to one backing branch, after which it exists on exactly one of them.
_Avoid_: copy, transfer, migration

**Run**:
One evaluation of the trigger followed by moves until the stop condition is reached.
_Avoid_: job, pass, cycle

**Threshold**:
The cache-branch usage percentage above which a run starts moving.

**Target**:
The cache-branch usage percentage at which a run stops moving.

**In use**:
A move unit is in use while any process has it open for writing; in-use move units are never moved.
_Avoid_: busy, locked, open (a file open only for reading is not in use)
