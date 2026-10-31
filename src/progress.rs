//! A delayed, one-line status, owned only while preparing an eager view.
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    io::IsTerminal,
    os::fd::AsRawFd,
    sync::{mpsc, Mutex, MutexGuard, TryLockError},
    thread::JoinHandle,
    time::Duration,
};

// Serialize progress writes with the runner's native-Git terminal handoffs.
// The worker uses try_lock: it stays responsive to cleanup while Git owns the
// foreground. No mutex operation is performed in the fork child's pre_exec.
static DRAW: Mutex<Option<ProgressBar>> = Mutex::new(None);

fn foreground_stderr() -> bool {
    let foreground = unsafe { libc::tcgetpgrp(std::io::stderr().as_raw_fd()) };
    // A non-controlling PTY can be a terminal without an owning process group.
    // TOSTOP job-control restrictions do not apply to that case.
    foreground <= 0 || foreground == unsafe { libc::getpgrp() }
}
fn clear(bar: ProgressBar) {
    if !foreground_stderr() {
        // Never erase another foreground job's output or generate SIGTTOU.
        // Our bars use a plain terminal target, whose disconnect does no I/O.
        bar.set_draw_target(ProgressDrawTarget::hidden());
    }
    bar.finish_and_clear();
}
fn try_draw() -> Option<MutexGuard<'static, Option<ProgressBar>>> {
    match DRAW.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}
/// Clear before a native child receives the terminal, and keep drawing paused
/// until Git's supervision/foreground restoration has completed in the parent.
pub(crate) fn native_terminal_lease(
    transfer: bool,
) -> Option<MutexGuard<'static, Option<ProgressBar>>> {
    if !transfer {
        return None;
    }
    let mut drawing = DRAW.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(bar) = drawing.take() {
        clear(bar);
    }
    Some(drawing)
}

pub(crate) struct MaterializationProgress {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl MaterializationProgress {
    pub(crate) fn start() -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = (std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb"))
        .then(|| {
            std::thread::Builder::new()
                .name("materialization-progress".into())
                .spawn(move || {
                    if receiver.recv_timeout(Duration::from_millis(500))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        return;
                    }
                    loop {
                        if let Some(mut drawing) = try_draw() {
                            if foreground_stderr() {
                                let bar = drawing.get_or_insert_with(|| {
                                    let bar = ProgressBar::new_spinner();
                                    bar.set_style(
                                        ProgressStyle::with_template(
                                            "{spinner} Preparing snapshot",
                                        )
                                        .expect("static progress template")
                                        .tick_strings(&["|", "/", "-", "\\"]),
                                    );
                                    bar
                                });
                                bar.tick();
                            } else if let Some(bar) = drawing.take() {
                                clear(bar);
                            }
                        }
                        if receiver.recv_timeout(Duration::from_millis(100))
                            != Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            break;
                        }
                    }
                    if let Some(mut drawing) = try_draw() {
                        if let Some(bar) = drawing.take() {
                            clear(bar);
                        }
                    }
                })
        })
        .and_then(Result::ok);
        Self { stop, worker }
    }
}
impl Drop for MaterializationProgress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            // Clearing completes before the child inherits the terminal, also
            // on the error/cancellation return path.
            let _ = worker.join();
        }
    }
}
