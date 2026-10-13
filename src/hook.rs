//! The run hook: a user-configured command that cachy executes after a run,
//! with the run summary on stdin. A hook that fails, or that cachy kills for
//! taking too long, is logged; it never changes cachy's exit code.

use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::config::RunHook;
use crate::log;
use crate::stop;
use crate::summary::Summary;

const POLL: Duration = Duration::from_millis(20);

/// Executes `hook` with `summary` on its stdin, unless its `on` filter
/// excludes the run's outcome, and waits for it to exit, or kills it once its
/// timeout is over or cachy is asked to stop.
pub fn call(hook: &RunHook, summary: &Summary) {
    if !hook.on.contains(&summary.outcome) {
        return;
    }
    if let Err(e) = execute(hook, summary) {
        log::error!("run hook failed: {e:#}");
    }
}

fn execute(hook: &RunHook, summary: &Summary) -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&hook.command)
        .stdin(Stdio::piped())
        // Its own process group, so that killing it reaches whatever it
        // started too.
        .process_group(0);
    // SAFETY: signal(2) is async-signal-safe. A run ignores SIGIO, which an
    // exec would pass on to the hook.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGIO, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot execute `{}`", hook.command))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let json = summary.to_json();
    // On a thread of its own: a hook that does not read its stdin must not
    // keep cachy from timing it out. The thread ends once the hook closes its
    // stdin, by exiting or being killed, unless something it left behind
    // keeps that open; cachy then exits without waiting for it.
    thread::spawn(move || {
        // A hook may exit without reading its stdin.
        let _ = stdin.write_all(json.as_bytes());
    });
    match wait(&mut child, hook.timeout)? {
        Ended::Exited(status) if status.success() => Ok(()),
        Ended::Exited(status) => Err(anyhow!("`{}` {}", hook.command, describe(status))),
        Ended::TimedOut => Err(anyhow!(
            "`{}` was killed after its timeout of {:?}",
            hook.command,
            hook.timeout
        )),
        Ended::Stopped => Err(anyhow!(
            "`{}` was killed: cachy was asked to stop",
            hook.command
        )),
    }
}

/// How a run hook ended.
enum Ended {
    Exited(ExitStatus),
    /// It was killed once its timeout was over.
    TimedOut,
    /// It was killed because cachy was asked to stop.
    Stopped,
}

/// Waits for `child` to exit, killing it, with its process group, once
/// `timeout` is over or a stop is requested.
fn wait(child: &mut Child, timeout: Duration) -> Result<Ended> {
    let deadline = Instant::now() + timeout;
    let ended = loop {
        if let Some(status) = child.try_wait().context("wait for the run hook")? {
            return Ok(Ended::Exited(status));
        }
        if stop::requested() {
            break Ended::Stopped;
        }
        if Instant::now() >= deadline {
            break Ended::TimedOut;
        }
        thread::sleep(POLL);
    };
    // SAFETY: kill(2) touches no memory. The group exists: its leader is our
    // child, which has not been reaped.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    child.wait().context("wait for the killed run hook")?;
    Ok(ended)
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        _ => format!("ended with {status}"),
    }
}
