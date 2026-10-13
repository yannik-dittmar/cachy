//! The run orchestrator: performs the check and, if it finds work, moves files
//! until the run's byte budget is consumed.

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, LeaseType, OpenFileGuard, Order};
use crate::exclude::Exclusions;
use crate::fsops::{FsOps, FsStat, Lease};
use crate::layout::{self, BackingBranch};
use crate::lock::InstanceLock;
use crate::log;
use crate::metadata::AllowedLosses;
use crate::placement::{Placement, Unplaced};
use crate::probe::probe;
use crate::recovery::recover;
use crate::scan::{Entry, Kind, scan};
use crate::stop;
use crate::summary::{FailedUnit, MovedUnit, Reason, SkippedUnit, StaleCopy, Summary};
use crate::transaction::{
    Branch, Copying, Fate, Finished, Guard, InUseGuard, MoveOutcome, Moved, Mover, RETIRING_POLL,
    STAGING, Skip, open_dirs, split, would_skip,
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
    /// The in-use guard that the run's moves used.
    pub open_file_guard: OpenFileGuard,
    /// The summary of the run, for the run hook; `None` for a dry run, and
    /// when the check found no work and nothing failed.
    pub summary: Option<Summary>,
}

impl Outcome {
    /// Every attempted move succeeded and the run was not cut short.
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.unresolved == 0 && self.undated_dirs == 0 && !self.stopped
    }

    /// Completes `summary` with this outcome and keeps it, unless it is of a
    /// dry run, or of an invocation that did not run (`ran`) and failed in
    /// nothing. The run started at `started`, and moved for `moving`.
    fn summarise(
        mut self,
        mut summary: Summary,
        options: Options,
        ran: bool,
        started: Instant,
        moving: Duration,
    ) -> Outcome {
        if options.dry_run || (!ran && self.succeeded()) {
            return self;
        }
        summary.stopped = self.stopped;
        summary.unresolved_leftovers = self.unresolved;
        summary.undated_dirs = self.undated_dirs;
        summary.finish(self.succeeded(), started.elapsed(), moving);
        self.summary = Some(summary);
        self
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

/// Takes the instance lock on the cache branch, probes the branches'
/// filesystems and recovers from unfinished moves, then performs a check and,
/// if free space is below the threshold or move units are past the max age, a
/// run. The run moves the move units past the max age first, and stops once
/// they are moved and its byte budget is consumed. A drain skips the check and
/// has no byte budget; a dry run only prints what a run would do. An error
/// means cachy refused to run; once recovery has started, errors are reported
/// in the outcome instead.
///
/// The probe performs its operations through `probe_fs`, apart from the
/// others, so that fault injection numbers them apart.
pub fn run(
    fs: &dyn FsOps,
    probe_fs: &dyn FsOps,
    config: &Config,
    options: Options,
) -> Result<Outcome> {
    let started = Instant::now();
    let cache_branch = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let reason = if options.drain {
        Reason::Drain
    } else {
        Reason::Manual
    };
    let mut summary = Summary::new(reason, &cache_branch.path);
    // Before anything writes: another run's moves would look like leftovers
    // to recovery.
    let _lock = InstanceLock::acquire(&config.state_dir, &cache_branch.path)?;
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
    stop::install();
    let guard = Guard {
        in_use: in_use(config),
        minimum_age: config.minimum_age,
    };
    // Measured before cachy writes anything, the probe's scratch files
    // included: ZFS shows their frees only once their transaction group syncs,
    // so a check after them would find less free space than applications left.
    let free_before = stat_fs(fs, &cache_branch);
    let capabilities = probe(probe_fs, guard.in_use, &cache_branch, &backing_branches)?;
    let copying = Copying {
        verify: config.verify,
        dense: capabilities.dense_copies,
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
            guard.in_use.lease(),
            allowed_losses,
        )
    };
    let mut outcome = Outcome {
        failed: 0,
        unresolved,
        undated_dirs: 0,
        stopped: false,
        open_file_guard: config.open_file_guard,
        summary: None,
    };
    // For an invocation that ends before it runs.
    let without_run = |outcome: Outcome, summary| {
        outcome.summarise(summary, options, false, started, Duration::ZERO)
    };
    if stopping(&mut outcome) {
        return Ok(without_run(outcome, summary));
    }
    let free_before = match free_before {
        Ok(stat) => stat.free,
        Err(e) => {
            log::error!("run stopped: {e:#}");
            outcome.stopped = true;
            return Ok(without_run(outcome, summary));
        }
    };
    summary.free_before = Some(free_before);
    let below_threshold = free_before < threshold;
    if !options.drain && !below_threshold && config.max_age.is_none() {
        no_work(free_before, threshold, options);
        return Ok(without_run(outcome, summary));
    }

    let order = match config.order {
        None | Some(Order::Atime) if cache_fs.noatime => {
            log::warning!(
                "the cache branch is mounted noatime, so move units are ordered by mtime instead of atime"
            );
            Order::Mtime
        }
        None if capabilities.cache_branch_on_zfs => {
            log::warning!(
                "the cache branch is on ZFS, where atime=off does not always show as a mount option, so the atimes that move units are ordered by may never change; set `order` to atime or mtime"
            );
            Order::Atime
        }
        order => order.unwrap_or_default(),
    };
    let entries = match scan(fs, &cache_branch, &config.exclude, order) {
        Ok(entries) => entries,
        Err(e) => {
            log::error!(
                "run stopped: cannot scan cache branch {}: {e}",
                cache_branch.path.display()
            );
            outcome.stopped = true;
            return Ok(without_run(outcome, summary));
        }
    };
    if stopping(&mut outcome) {
        return Ok(without_run(outcome, summary));
    }
    // Move units past the max age first, each part still in order.
    let (mut entries, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| {
        config
            .max_age
            .is_some_and(|max_age| entry.is_past_max_age(max_age))
    });
    let past_max_age = entries.len();
    entries.extend(rest);
    if let Some(max_age) = config.max_age {
        log::info!("move units past the max age of {max_age:?}: {past_max_age}");
    }

    let budget = if options.drain {
        log::info!("draining: moving every eligible move unit");
        None
    } else if below_threshold {
        log::info!(
            "free space {free_before} bytes is below the threshold {threshold} bytes; moving until {target} bytes are free"
        );
        Some(target - free_before)
    } else if past_max_age > 0 {
        log::info!(
            "free space {free_before} bytes is at or above the threshold {threshold} bytes; moving the move units past the max age"
        );
        Some(0)
    } else {
        no_work(free_before, threshold, options);
        return Ok(without_run(outcome, summary));
    };
    summary.byte_budget = budget;

    let stop = Stop {
        past_max_age,
        budget,
    };
    let mut placement = Placement::new(&backing_branches);
    if options.dry_run {
        outcome.failed = dry_run(
            fs,
            &cache_branch,
            &guard,
            outcome.open_file_guard,
            &entries,
            stop,
            &mut placement,
        );
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
                    copying,
                )
            })
        })
        .collect();
    let mut tally = Tally::default();
    let moving = Instant::now();
    let (moved_bytes, undated) = move_units(movers, &entries, stop, &mut placement, &mut tally);
    let moving = moving.elapsed();
    for (path, e) in &undated {
        log::error!(
            "cannot set the times of created directory {}: {e:#}",
            path.display()
        );
    }
    outcome.undated_dirs = undated.len();
    let stopped = stopping(&mut outcome);
    if !config.keep_empty_dirs {
        remove_emptied_dirs(fs, &cache_branch, &config.exclude, &tally.left_dirs);
    }

    let written = backing_branches
        .iter()
        .filter(|backing| backing.takes_new_files())
        .map(|backing| &backing.branch);
    for branch in std::iter::once(&cache_branch).chain(written) {
        // Fails harmlessly if the staging directory is absent or not empty.
        let _ = fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }
    match stat_fs(fs, &cache_branch) {
        // A stop leaves the target missed; the frees are not worth waiting for.
        Ok(stat) if stopped => summary.free_after = Some(stat.free),
        Ok(stat) => {
            let freed = |free: u64| free.saturating_sub(free_before);
            let target_missed = |free: u64| below_threshold && !options.drain && free < target;
            let frees_missing = |free: u64| freed(free) < moved_bytes / FREED_DIVISOR;
            // Waiting helps the target only while some moved bytes have yet
            // to show as free.
            let free = wait_for_frees(&cache_branch, stat.free, |free| {
                (target_missed(free) && freed(free) < moved_bytes) || frees_missing(free)
            });
            summary.free_after = Some(free);
            if target_missed(free) {
                log::warning!("target not reached: {free} bytes free, target {target} bytes");
            }
            if frees_missing(free) {
                log::warning!(
                    "free space grew by only {} bytes, but the moves took up {moved_bytes} bytes on it; snapshots of the cache branch may be holding the space",
                    freed(free)
                );
            }
        }
        Err(e) => log::warning!("{e:#}"),
    }
    log::info!(
        "run finished: {} moved, {} skipped, {} failed; in-use guard: {}",
        tally.moved.len(),
        tally.skipped.len(),
        tally.failed.len(),
        outcome.open_file_guard
    );
    outcome.failed = tally.failed.len();
    summary.moved = tally.moved;
    summary.skipped = tally.skipped;
    summary.failed = tally.failed;
    summary.stale_copies = tally.stale_copies;
    Ok(outcome.summarise(summary, options, true, started, moving))
}

/// How moves tell that a move unit is in use, as the configuration sets it.
/// Warns on every run without a guard.
fn in_use(config: &Config) -> InUseGuard {
    let writers_only = match config.lease_type {
        LeaseType::Read => true,
        LeaseType::Write => false,
    };
    match config.open_file_guard {
        OpenFileGuard::Lease if writers_only => InUseGuard::Lease(Lease::Read),
        OpenFileGuard::Lease => InUseGuard::Lease(Lease::Write),
        OpenFileGuard::Proc => InUseGuard::Proc { writers_only },
        OpenFileGuard::Off => {
            log::warning!(
                "open_file_guard is off: NOTHING GUARDS FILES IN USE; files are moved while applications have them open, and a write to one during its move can be lost"
            );
            InUseGuard::Off
        }
    }
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
        /// Each finished move, with the size (`st_size`) of its move unit.
        finished: Vec<(Finished<'a>, u64)>,
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
/// parallel. Once a stop is requested, no further move unit is queued, and
/// the workers skip those still queued. Returns the space the moves freed on
/// the cache branch, and the directories created on backing branches whose
/// times could not be set.
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
            if stop::requested() {
                break;
            }
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
            } => {
                let worker = self.workers[index].as_mut().expect("a worker reports");
                if started {
                    worker.queued.pop_front();
                }
                worker.allocated = allocated;
                worker.retiring = retiring;
                // Move units that did not move give their space back.
                let unmoved = finished
                    .iter()
                    .filter(|(finished, _)| !matches!(finished.outcome, Ok(MoveOutcome::Moved(_))))
                    .map(|(_, size)| size)
                    .sum();
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
        let finished = finished
            .into_iter()
            .map(|finished| {
                let size = self
                    .sizes
                    .remove(&finished.rels.as_ptr())
                    .expect("a finished move unit has started");
                (finished, size)
            })
            .collect();
        let _ = self.reports.send(Report::State {
            index: self.index,
            started,
            allocated: state.0,
            retiring: state.1,
            finished,
        });
    }
}

/// Free space once `warns` no longer holds for it, after [`FREED_WAIT`] or
/// once a stop is requested, starting from `free`. Frees can show late, and a
/// part at a time: on ZFS, once their transaction group syncs, and a large
/// file over several. Measured outside the port: how often it measures
/// depends on timing.
fn wait_for_frees(cache_branch: &Branch, mut free: u64, warns: impl Fn(u64) -> bool) -> u64 {
    let deadline = Instant::now() + freed_wait();
    while warns(free) && Instant::now() < deadline && !stop::requested() {
        thread::sleep(FREED_POLL);
        match FsStat::of(cache_branch.fd.as_fd()) {
            Ok(stat) => free = stat.free,
            Err(_) => break,
        }
    }
    free
}

/// Removes each directory in `dirs`, relpaths on the cache branch, if it is
/// empty, then its parent if that is empty in turn, and so on up to the cache
/// branch root. Each of them held a move unit or a directory when the run
/// started, so a directory that was empty before the run is never removed.
/// Neither is an excluded one: the walk never enters those.
fn remove_emptied_dirs(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    exclude: &Exclusions,
    dirs: &[PathBuf],
) {
    let mut dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    // Deepest first, so that a parent comes after its children.
    dirs.sort_by_key(|dir| (std::cmp::Reverse(dir.components().count()), *dir));
    dirs.dedup();
    for dir in dirs {
        for dir in dir.ancestors().filter(|dir| !dir.as_os_str().is_empty()) {
            if exclude.excludes(dir, true) {
                break;
            }
            let (parent, name) = split(dir);
            // Opened component by component, so that no symlink leads out of
            // the cache branch.
            let removed = open_dirs(fs, &cache_branch.fd, parent)
                .and_then(|parent| fs.rmdir(parent.as_fd(), name));
            match removed {
                Ok(()) => {}
                // Not empty, or removed already as another's parent.
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::ENOTEMPTY | libc::EEXIST | libc::ENOENT)
                    ) =>
                {
                    break;
                }
                Err(e) => {
                    log::warning!(
                        "cannot remove emptied directory {}: {e}",
                        cache_branch.path.join(dir).display()
                    );
                    break;
                }
            }
        }
    }
}

/// Whether a stop was requested, as the run notes in `outcome`.
fn stopping(outcome: &mut Outcome) -> bool {
    if stop::requested() && !outcome.stopped {
        log::warning!("run stopped early: cachy was asked to stop");
        outcome.stopped = true;
    }
    outcome.stopped
}

/// Notes that a check found no work.
fn no_work(free: u64, threshold: u64, options: Options) {
    log::info!(
        "free space {free} bytes is at or above the threshold {threshold} bytes; nothing to do"
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
/// the number of move units that could not be checked. The last line names
/// `open_file_guard`, the configured in-use guard.
fn dry_run(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    guard: &Guard,
    open_file_guard: OpenFileGuard,
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
                log::error!("{rel}: {e:#}");
                failed += 1;
            }
        }
    }
    log::info!(
        "dry run finished: {moves} would move ({planned} bytes), {skips} would skip, {failed} failed; in-use guard: {open_file_guard}"
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
            log::warning!(
                "{} holds leftovers of an unfinished move, which the next run will recover",
                staging.display()
            );
        }
    }
}

/// Records finished moves by outcome for the run summary, logging each.
#[derive(Default)]
struct Tally {
    moved: Vec<MovedUnit>,
    skipped: Vec<SkippedUnit>,
    failed: Vec<FailedUnit>,
    /// The stale copies that moves dealt with.
    stale_copies: Vec<StaleCopy>,
    /// The cache-branch directories, by relpath, that moved move units left,
    /// which may now be empty. The cache branch root is not among them.
    left_dirs: Vec<PathBuf>,
}

impl Tally {
    /// Records the outcomes of moves to the backing branch at `branch`.
    fn record(&mut self, branch: &Path, finished: Vec<(Finished, u64)>) {
        for (finished, size) in finished {
            let rels = finished.rels;
            match finished.outcome {
                Ok(MoveOutcome::Moved(Moved { hash })) => {
                    self.moved.push(MovedUnit::new(rels, branch, size));
                    self.left_dirs.extend(
                        rels.iter()
                            .filter_map(|rel| rel.parent())
                            .filter(|dir| !dir.as_os_str().is_empty())
                            .map(Path::to_owned),
                    );
                    log::info!(
                        "moved {} to {} (xxh3 {hash})",
                        names(rels),
                        branch.display()
                    );
                }
                Ok(MoveOutcome::Skipped(reason)) => self.skip(rels, reason),
                Err(e) => self.fail(rels, &format!("{e:#}")),
            }
            for (path, fate) in finished.stale_copies {
                let done = match fate {
                    Fate::Kept => "kept identical",
                    Fate::Replaced => "replaced",
                    Fate::Removed => "removed",
                };
                log::info!("{done} stale copy {}", path.display());
                self.stale_copies.push(StaleCopy::new(&path, fate));
            }
        }
    }

    /// Records a move unit whose move failed.
    fn fail(&mut self, rels: &[PathBuf], e: &dyn std::fmt::Display) {
        log::error!("move of {} failed: {e}", names(rels));
        self.failed.push(FailedUnit::new(rels, e.to_string()));
    }

    /// Records a move unit that was skipped.
    fn skip(&mut self, rels: &[PathBuf], reason: Skip) {
        log::info!("skipped {}: {reason}", names(rels));
        self.skipped.push(SkippedUnit::new(rels, reason));
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
