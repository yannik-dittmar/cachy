//! The run orchestrator: performs the check and, if it finds work, moves files
//! until the run's byte budget is consumed.

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, LeaseType, Order};
use crate::fsops::{FsOps, FsStat, Lease};
use crate::layout::{self, BackingBranch};
use crate::metadata::AllowedLosses;
use crate::placement::{Placement, Unplaced};
use crate::recovery::recover;
use crate::scan::{Entry, Kind, scan};
use crate::transaction::{
    Branch, Fate, Guard, MoveOutcome, Moved, Mover, RETIRING_POLL, STAGING, Skip, would_skip,
};

pub struct Outcome {
    /// Number of moves that were attempted and failed; in a dry run, the
    /// number of move units that could not be checked.
    pub failed: usize,
    /// Number of staging-directory leftovers that recovery could not resolve.
    pub unresolved: usize,
    /// Number of directories created on the backing branch whose times could
    /// not be set.
    pub undated_dirs: usize,
    /// The run stopped early on an error outside any move.
    pub stopped: bool,
}

impl Outcome {
    /// Every attempted move succeeded and the run was not cut short.
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.unresolved == 0 && self.undated_dirs == 0 && !self.stopped
    }
}

/// How `cachy run` was invoked.
#[derive(Clone, Copy)]
pub struct Options {
    /// Write nothing; print what a run would do instead.
    pub dry_run: bool,
    /// Skip the check and move every eligible move unit.
    pub drain: bool,
}

/// Free space should grow by at least the bytes moved divided by this; less
/// means that something, such as a snapshot, still holds the moved data.
const FREED_DIVISOR: u64 = 2;
/// How long a run waits at its end for the space its moves freed to show in
/// free space. ZFS shows frees only once their transaction group syncs, by
/// default within 5 s, or the one after; `syncfs` does not sync it.
const FREED_WAIT: Duration = Duration::from_secs(15);
const FREED_POLL: Duration = Duration::from_millis(100);

/// [`FREED_WAIT`], unless a fault-injection build is told otherwise.
fn freed_wait() -> Duration {
    #[cfg(feature = "fault-injection")]
    if let Some(wait) = crate::fault::freed_wait() {
        return wait;
    }
    FREED_WAIT
}

/// Recovers from unfinished moves, then performs a check and, if free space
/// is below the threshold or move units are past the max age, a run. The run
/// moves the move units past the max age first, and stops once they are moved
/// and its byte budget is consumed. A drain skips the check and has no byte
/// budget; a dry run only prints what a run would do. An error means cachy refused to run; once
/// recovery has started, errors are reported in the outcome instead.
pub fn run(fs: &dyn FsOps, config: &Config, options: Options) -> Result<Outcome> {
    let cache_branch = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let cache_fs = FsStat::of(cache_branch.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure the size of cache branch {}",
            cache_branch.path.display()
        )
    })?;
    let threshold = config.threshold.resolve(cache_fs.size);
    let target = config.target.resolve(cache_fs.size);
    if target <= threshold {
        bail!(
            "target ({target} bytes, from {}) must be above the threshold ({threshold} bytes, from {}) on the cache branch's {} bytes",
            config.target,
            config.threshold,
            cache_fs.size
        );
    }
    let backing_branches = layout::discover(config, &cache_branch, threshold)?;

    ignore_lease_break_signal();
    let guard = Guard {
        lease: match config.lease_type {
            LeaseType::Read => Lease::Read,
            LeaseType::Write => Lease::Write,
        },
        minimum_age: config.minimum_age,
    };
    let allowed_losses = AllowedLosses::of(&config.allow_metadata_loss);
    let unresolved = if options.dry_run {
        warn_of_leftovers(&cache_branch, &backing_branches);
        0
    } else {
        recover(
            fs,
            &cache_branch,
            &backing_branches,
            guard.lease,
            allowed_losses,
        )
    };
    let mut outcome = Outcome {
        failed: 0,
        unresolved,
        undated_dirs: 0,
        stopped: false,
    };
    let free_before = match stat_fs(fs, &cache_branch) {
        Ok(stat) => stat.free,
        Err(e) => {
            eprintln!("cachy: error: run stopped: {e:#}");
            outcome.stopped = true;
            return Ok(outcome);
        }
    };
    let below_threshold = free_before < threshold;
    if !options.drain && !below_threshold && config.max_age.is_none() {
        no_work(free_before, threshold, options);
        return Ok(outcome);
    }

    let order = match config.order {
        Order::Atime if cache_fs.noatime => {
            eprintln!(
                "cachy: warning: the cache branch is mounted noatime, so move units are ordered by mtime instead of atime"
            );
            Order::Mtime
        }
        order => order,
    };
    let entries = match scan(fs, &cache_branch, &config.exclude, order) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "cachy: error: run stopped: cannot scan cache branch {}: {e}",
                cache_branch.path.display()
            );
            outcome.stopped = true;
            return Ok(outcome);
        }
    };
    // Move units past the max age first, each part still in order.
    let (mut entries, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| {
        config
            .max_age
            .is_some_and(|max_age| entry.is_past_max_age(max_age))
    });
    let past_max_age = entries.len();
    entries.extend(rest);
    if let Some(max_age) = config.max_age {
        eprintln!("cachy: move units past the max age of {max_age:?}: {past_max_age}");
    }

    let budget = if options.drain {
        eprintln!("cachy: draining: moving every eligible move unit");
        None
    } else if below_threshold {
        eprintln!(
            "cachy: free space {free_before} bytes is below the threshold {threshold} bytes; moving until {target} bytes are free"
        );
        Some(target - free_before)
    } else if past_max_age > 0 {
        eprintln!(
            "cachy: free space {free_before} bytes is at or above the threshold {threshold} bytes; moving the move units past the max age"
        );
        Some(0)
    } else {
        no_work(free_before, threshold, options);
        return Ok(outcome);
    };

    let stop = Stop {
        past_max_age,
        budget,
    };
    let mut placement = Placement::new(&backing_branches);
    if options.dry_run {
        outcome.failed = dry_run(fs, &cache_branch, &guard, &entries, stop, &mut placement);
        return Ok(outcome);
    }

    let movers = backing_branches
        .iter()
        .enumerate()
        .map(|(index, backing)| {
            backing.takes_new_files().then(|| {
                Mover::new(
                    fs,
                    &cache_branch,
                    &backing_branches,
                    index,
                    &guard,
                    allowed_losses,
                    config.verify,
                )
            })
        })
        .collect();
    let mut tally = Tally::default();
    let (moved_bytes, undated) = move_units(movers, &entries, stop, &mut placement, &mut tally);
    for (path, e) in &undated {
        eprintln!(
            "cachy: error: cannot set the times of created directory {}: {e:#}",
            path.display()
        );
    }
    outcome.undated_dirs = undated.len();

    let written = backing_branches
        .iter()
        .filter(|backing| backing.takes_new_files())
        .map(|backing| &backing.branch);
    for branch in std::iter::once(&cache_branch).chain(written) {
        // Fails harmlessly if the staging directory is absent or not empty.
        let _ = fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }
    match stat_fs(fs, &cache_branch) {
        Ok(stat) => {
            let freed = |free: u64| free.saturating_sub(free_before);
            let target_missed = |free: u64| below_threshold && !options.drain && free < target;
            let frees_missing = |free: u64| freed(free) < moved_bytes / FREED_DIVISOR;
            // Waiting helps the target only while some moved bytes have yet
            // to show as free.
            let free = wait_for_frees(&cache_branch, stat.free, |free| {
                (target_missed(free) && freed(free) < moved_bytes) || frees_missing(free)
            });
            if target_missed(free) {
                eprintln!(
                    "cachy: warning: target not reached: {free} bytes free, target {target} bytes"
                );
            }
            if frees_missing(free) {
                eprintln!(
                    "cachy: warning: free space grew by only {} bytes, but the moves took up {moved_bytes} bytes on it; snapshots of the cache branch may be holding the space",
                    freed(free)
                );
            }
        }
        Err(e) => eprintln!("cachy: warning: {e:#}"),
    }
    let Tally {
        moved,
        skipped,
        failed,
    } = tally;
    eprintln!("cachy: run finished: {moved} moved, {skipped} skipped, {failed} failed");
    outcome.failed = failed;
    Ok(outcome)
}

/// How many move units the dispatcher places ahead of the moves that have
/// started, across all backing branches. Placing ahead lets a branch start
/// its next move while another branch works through a run of move units;
/// the limit keeps the free space that placement sees recent.
const LOOKAHEAD: usize = 64;

/// What a worker tells the dispatcher.
enum Report<'a> {
    /// The state of the worker for the backing branch `index`.
    State {
        index: usize,
        /// The worker has started the next move unit in its queue.
        started: bool,
        /// [`Mover::allocated_bytes`].
        allocated: u64,
        /// [`Mover::retiring`].
        retiring: bool,
        finished: Vec<(&'a [PathBuf], Result<MoveOutcome>)>,
        /// The sizes (`st_size`) of the finished move units that did not
        /// move.
        unmoved: u64,
    },
    /// The worker for the backing branch `index` has stopped: once its queue
    /// has ended, or by panicking.
    Exited { index: usize },
}

/// What the dispatcher knows of a worker.
struct Worker<'a> {
    /// The path of its backing branch.
    path: PathBuf,
    /// Where the dispatcher queues move units for it; `None` once the queue
    /// has ended.
    queue: Option<mpsc::Sender<&'a Entry>>,
    /// The space (`st_blocks`) of each queued move unit that has not
    /// started, in order.
    queued: VecDeque<u64>,
    allocated: u64,
    retiring: bool,
}

/// Moves `entries`, in order, each to the backing branch that placement
/// chooses, until the run stops by `stop`. Each backing branch that
/// takes new files has a mover (`None` for the others), which runs on a
/// worker thread of its own and works through a queue of move units: moves
/// on one branch run one at a time, and moves on different branches in
/// parallel. Returns the space the moves freed on the cache branch, and the
/// directories created on backing branches whose times could not be set.
fn move_units<'a>(
    movers: Vec<Option<Mover<'a>>>,
    entries: &'a [Entry],
    stop: Stop,
    placement: &mut Placement,
    tally: &mut Tally,
) -> (u64, Vec<(PathBuf, anyhow::Error)>) {
    thread::scope(|scope| {
        let (report_tx, reports) = mpsc::channel();
        let mut handles = Vec::new();
        let workers = movers
            .into_iter()
            .enumerate()
            .map(|(index, mover)| {
                let mover = mover?;
                let (queue, units) = mpsc::channel();
                let path = mover.backing_branch().to_owned();
                let report_tx = report_tx.clone();
                handles.push(scope.spawn(move || work(index, mover, units, report_tx)));
                Some(Worker {
                    path,
                    queue: Some(queue),
                    queued: VecDeque::new(),
                    allocated: 0,
                    retiring: false,
                })
            })
            .collect();
        drop(report_tx);

        let mut dispatcher = Dispatcher {
            workers,
            placement,
            tally,
        };
        let next = || reports.recv().expect("the workers are running");
        'entries: for (i, entry) in entries.iter().enumerate() {
            while let Ok(report) = reports.try_recv() {
                dispatcher.apply(report);
            }
            // A retiring move that is rolled back hands its bytes back, so the
            // budget is consumed only once no move is queued or retiring.
            while stop.before(i, dispatcher.committed()) {
                if dispatcher.settled() {
                    break 'entries;
                }
                dispatcher.apply(next());
            }
            while dispatcher.queued() >= LOOKAHEAD {
                dispatcher.apply(next());
            }
            let skip = match entry.kind {
                Kind::Special => Some(Skip::SpecialFile),
                Kind::PartlyExcluded => Some(Skip::PartlyExcluded),
                Kind::Regular | Kind::Symlink => None,
            };
            if let Some(skip) = skip {
                dispatcher.tally.skip(&entry.rels, skip);
                continue;
            }
            let mut placed = dispatcher.placement.place(&entry.rels, entry.size);
            if matches!(placed, Err(Unplaced::NoRoom)) && !dispatcher.settled() {
                // Queued move units that do not move give their space back.
                while !dispatcher.settled() {
                    dispatcher.apply(next());
                }
                placed = dispatcher.placement.place(&entry.rels, entry.size);
            }
            match placed {
                Ok(index) => dispatcher.queue(index, entry),
                Err(Unplaced::NoRoom) => dispatcher.tally.skip(&entry.rels, Skip::NoRoom),
                Err(unplaced) => dispatcher.tally.fail(&entry.rels, &unplaced),
            }
        }

        // Ending the queues lets each worker finish its moves and exit.
        for worker in dispatcher.workers.iter_mut().flatten() {
            worker.queue = None;
        }
        for report in reports.iter() {
            dispatcher.apply(report);
        }
        let moved = dispatcher.committed();
        let undated = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a panicked worker ends the run"))
            .collect();
        (moved, undated)
    })
}

struct Dispatcher<'d, 'a, 'b> {
    workers: Vec<Option<Worker<'a>>>,
    placement: &'d mut Placement<'b>,
    tally: &'d mut Tally,
}

impl<'a> Dispatcher<'_, 'a, '_> {
    /// Queues `entry` for the worker of the backing branch `index`.
    fn queue(&mut self, index: usize, entry: &'a Entry) {
        let worker = self.workers[index]
            .as_mut()
            .expect("placement chooses only branches that take new files");
        worker.queued.push_back(entry.allocated);
        let queue = worker
            .queue
            .as_ref()
            .expect("queues end after the last entry");
        queue
            .send(entry)
            .expect("a worker runs until its queue ends");
    }

    fn apply(&mut self, report: Report<'_>) {
        match report {
            Report::State {
                index,
                started,
                allocated,
                retiring,
                finished,
                unmoved,
            } => {
                let worker = self.workers[index].as_mut().expect("a worker reports");
                if started {
                    worker.queued.pop_front();
                }
                worker.allocated = allocated;
                worker.retiring = retiring;
                self.tally.record(&worker.path, finished);
                self.placement.release(index, unmoved);
            }
            Report::Exited { index } => {
                let worker = self.workers[index].as_ref().expect("a worker reports");
                // A worker stops early only by panicking; joining it passes
                // the panic on.
                assert!(
                    worker.queue.is_none(),
                    "the worker for backing branch {} stopped",
                    worker.path.display()
                );
            }
        }
    }

    /// Space on the cache branch that the moves so far free, counting the
    /// queued move units in full.
    fn committed(&self) -> u64 {
        self.workers
            .iter()
            .flatten()
            .map(|w| w.allocated + w.queued.iter().sum::<u64>())
            .sum()
    }

    /// The number of queued move units that have not started.
    fn queued(&self) -> usize {
        self.workers.iter().flatten().map(|w| w.queued.len()).sum()
    }

    /// No move unit is queued and no move is retiring.
    fn settled(&self) -> bool {
        self.workers
            .iter()
            .flatten()
            .all(|w| w.queued.is_empty() && !w.retiring)
    }
}

/// Runs the moves of the move units that the dispatcher queues for `mover`,
/// the mover for the backing branch `index`, until the queue ends and every
/// retiring move has finished, reporting each change of its state. Returns
/// the directories created on the branch whose times could not be set.
fn work<'a>(
    index: usize,
    mut mover: Mover<'a>,
    units: mpsc::Receiver<&'a Entry>,
    reports: mpsc::Sender<Report<'a>>,
) -> Vec<(PathBuf, anyhow::Error)> {
    struct Exit<'a> {
        index: usize,
        reports: mpsc::Sender<Report<'a>>,
    }
    impl Drop for Exit<'_> {
        fn drop(&mut self) {
            let _ = self.reports.send(Report::Exited { index: self.index });
        }
    }
    let _exit = Exit {
        index,
        reports: reports.clone(),
    };
    let mut reporter = Reporter {
        index,
        reports,
        sizes: HashMap::new(),
        last: (0, false),
    };
    loop {
        let unit = if mover.retiring() {
            units.recv_timeout(RETIRING_POLL)
        } else {
            units
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match unit {
            Ok(entry) => {
                reporter.sizes.insert(entry.rels.as_ptr(), entry.size);
                mover.start(&entry.rels, entry.kind);
                reporter.report(&mut mover, true);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                mover.service();
                reporter.report(&mut mover, false);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    mover.finish();
    reporter.report(&mut mover, false);
    let path = mover.backing_branch().to_owned();
    mover
        .date_created_dirs()
        .into_iter()
        .map(|(rel, e)| (path.join(rel), e))
        .collect()
}

/// Reports a worker's state to the dispatcher.
struct Reporter<'a> {
    index: usize,
    reports: mpsc::Sender<Report<'a>>,
    /// The size of each started move unit that has not finished, by the
    /// address of its names.
    sizes: HashMap<*const PathBuf, u64>,
    /// [`Mover::allocated_bytes`] and [`Mover::retiring`] as last reported.
    last: (u64, bool),
}

impl<'a> Reporter<'a> {
    /// Reports the state of `mover`, if it `started` a move unit or changed
    /// since the last report.
    fn report(&mut self, mover: &mut Mover<'a>, started: bool) {
        let finished = mover.finished();
        let state = (mover.allocated_bytes(), mover.retiring());
        if !started && finished.is_empty() && state == self.last {
            return;
        }
        self.last = state;
        let unmoved = finished
            .iter()
            .map(|(rels, outcome)| {
                let size = self
                    .sizes
                    .remove(&rels.as_ptr())
                    .expect("a finished move unit has started");
                match outcome {
                    Ok(MoveOutcome::Moved(_)) => 0,
                    _ => size,
                }
            })
            .sum();
        let _ = self.reports.send(Report::State {
            index: self.index,
            started,
            allocated: state.0,
            retiring: state.1,
            finished,
            unmoved,
        });
    }
}

/// Free space once `warns` no longer holds for it, or after [`FREED_WAIT`],
/// starting from `free`. Frees can show late, and a part at a time: on ZFS,
/// once their transaction group syncs, and a large file over several.
/// Measured outside the port: how often it measures depends on timing.
fn wait_for_frees(cache_branch: &Branch, mut free: u64, warns: impl Fn(u64) -> bool) -> u64 {
    let deadline = Instant::now() + freed_wait();
    while warns(free) && Instant::now() < deadline {
        thread::sleep(FREED_POLL);
        match FsStat::of(cache_branch.fd.as_fd()) {
            Ok(stat) => free = stat.free,
            Err(_) => break,
        }
    }
    free
}

/// Notes that a check found no work.
fn no_work(free: u64, threshold: u64, options: Options) {
    eprintln!(
        "cachy: free space {free} bytes is at or above the threshold {threshold} bytes; nothing to do"
    );
    if options.dry_run {
        println!("byte budget: none (free space is at or above the threshold)");
    }
}

/// When a run stops: once it has moved the move units past the max age, which
/// come first, and its byte budget, if it has one, is consumed.
#[derive(Clone, Copy)]
struct Stop {
    /// How many entries, from the first, are past the max age.
    past_max_age: usize,
    budget: Option<u64>,
}

impl Stop {
    /// Whether the run stops before entry `i`, having moved `bytes`.
    fn before(self, i: usize, bytes: u64) -> bool {
        i >= self.past_max_age && self.budget.is_some_and(|budget| bytes >= budget)
    }
}

/// Prints the byte budget, then the move units that a run would move, in
/// order, and every skip with its reason, until the run would stop. Returns
/// the number of move units that could not be checked.
fn dry_run(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    guard: &Guard,
    entries: &[Entry],
    stop: Stop,
    placement: &mut Placement,
) -> usize {
    match stop.budget {
        Some(budget) => println!("byte budget: {budget} bytes"),
        None => println!("byte budget: unlimited (drain)"),
    }
    let (mut planned, mut moves, mut skips, mut failed) = (0, 0, 0, 0);
    for (i, entry) in entries.iter().enumerate() {
        if stop.before(i, planned) {
            break;
        }
        let rel = names(&entry.rels);
        let checked = would_skip(fs, cache_branch, guard, &entry.rels, entry.kind)
            .map_err(|e| e.context("cannot check"))
            .and_then(|skip| match skip {
                Some(skip) => Ok(Some(skip)),
                None => match placement.place(&entry.rels, entry.size) {
                    Ok(_) => Ok(None),
                    Err(Unplaced::NoRoom) => Ok(Some(Skip::NoRoom)),
                    Err(unplaced) => Err(anyhow::anyhow!("{unplaced}").context("would fail")),
                },
            });
        match checked {
            Ok(None) => {
                println!("would move {rel}");
                planned += entry.allocated;
                moves += 1;
            }
            Ok(Some(skip)) => {
                println!("would skip {rel}: {skip}");
                skips += 1;
            }
            Err(e) => {
                eprintln!("cachy: error: {rel}: {e:#}");
                failed += 1;
            }
        }
    }
    eprintln!(
        "cachy: dry run finished: {moves} would move ({planned} bytes), {skips} would skip, {failed} failed"
    );
    failed
}

/// A dry run does not recover, so it notes the staging directories that the
/// next run's recovery will resolve.
fn warn_of_leftovers(cache_branch: &Branch, backing_branches: &[BackingBranch]) {
    let backing = backing_branches.iter().map(|backing| &backing.branch);
    for branch in std::iter::once(cache_branch).chain(backing) {
        let staging = branch.path.join(STAGING);
        if staging.symlink_metadata().is_ok() {
            eprintln!(
                "cachy: warning: {} holds leftovers of an unfinished move, which the next run will recover",
                staging.display()
            );
        }
    }
}

/// Counts finished moves by outcome, logging skips and failures.
#[derive(Default)]
struct Tally {
    moved: usize,
    skipped: usize,
    failed: usize,
}

impl Tally {
    /// Records the outcomes of moves to the backing branch at `branch`.
    fn record(&mut self, branch: &Path, finished: Vec<(&[PathBuf], Result<MoveOutcome>)>) {
        for (rels, outcome) in finished {
            let rel = names(rels);
            match outcome {
                Ok(MoveOutcome::Moved(Moved { hash, stale_copies })) => {
                    self.moved += 1;
                    eprintln!("cachy: moved {rel} to {} (xxh3 {hash})", branch.display());
                    for (path, fate) in stale_copies {
                        let done = match fate {
                            Fate::Kept => "kept identical",
                            Fate::Replaced => "replaced",
                            Fate::Removed => "removed",
                        };
                        eprintln!("cachy: {done} stale copy {}", path.display());
                    }
                }
                Ok(MoveOutcome::Skipped(reason)) => {
                    self.skipped += 1;
                    eprintln!("cachy: skipped {rel}: {reason}");
                }
                Err(e) => {
                    self.failed += 1;
                    eprintln!("cachy: error: move of {rel} failed: {e:#}");
                }
            }
        }
    }

    /// Records a move unit whose move failed before it started.
    fn fail(&mut self, rels: &[PathBuf], e: &dyn std::fmt::Display) {
        self.failed += 1;
        eprintln!("cachy: error: move of {} failed: {e}", names(rels));
    }

    /// Records a move unit that was skipped without a move.
    fn skip(&mut self, rels: &[PathBuf], reason: Skip) {
        self.skipped += 1;
        eprintln!("cachy: skipped {}: {reason}", names(rels));
    }
}

/// The names of a move unit, as one line: a hardlink group's joined by
/// commas.
fn names(rels: &[PathBuf]) -> String {
    rels.iter()
        .map(|rel| rel.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A lease break signals its holder with `SIGIO`, which would terminate cachy.
/// Moves notice a break by querying their lease instead.
fn ignore_lease_break_signal() {
    // SAFETY: setting a signal to SIG_IGN installs no handler code.
    unsafe { libc::signal(libc::SIGIO, libc::SIG_IGN) };
}

fn stat_fs(fs: &dyn FsOps, cache_branch: &Branch) -> Result<FsStat> {
    fs.stat_fs(cache_branch.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure free space of cache branch {}",
            cache_branch.path.display()
        )
    })
}
