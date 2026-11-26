//! Preparation owns terminal rendering and standard Git/SSH prompt requests.
use anyhow::Result;
mod progress;
pub(crate) use progress::PackProgress;
use std::{
    collections::BTreeSet,
    ffi::{CStr, OsStr},
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, OpenOptionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const DIRECTORY: &str = "GIT_ON_TERMINAL_DIRECTORY";
const CORE_ASKPASS: &str = "GIT_ON_TERMINAL_CORE_ASKPASS";
const CLEAR_GRACE: Duration = Duration::from_millis(250);
const ENVIRONMENT: [(&str, &str); 5] = [
    ("GIT_ASKPASS", "GIT_ON_ORIGINAL_GIT_ASKPASS"),
    ("SSH_ASKPASS", "GIT_ON_ORIGINAL_SSH_ASKPASS"),
    ("SSH_ASKPASS_REQUIRE", "GIT_ON_ORIGINAL_SSH_ASKPASS_REQUIRE"),
    ("DISPLAY", "GIT_ON_ORIGINAL_DISPLAY"),
    ("WAYLAND_DISPLAY", "GIT_ON_ORIGINAL_WAYLAND_DISPLAY"),
];
static DIRECTORY_PATH: OnceLock<PathBuf> = OnceLock::new();
static PREPARING: AtomicBool = AtomicBool::new(false);
static VISIBLE: AtomicBool = AtomicBool::new(false);
static CLEAR_FAILED: AtomicBool = AtomicBool::new(false);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static RESUME_EPOCH: AtomicUsize = AtomicUsize::new(0);
const ROW_UNUSED: u8 = 0;
const ROW_PAINTED: u8 = 1;
const ROW_ABANDONED: u8 = 2;
static ROW: AtomicU8 = AtomicU8::new(ROW_UNUSED);
static DRAW: Mutex<Option<Drawing>> = Mutex::new(None);
static NATIVE: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());
#[cfg(test)]
thread_local! {
    static BEFORE_ROW_CLAIM: std::cell::Cell<Option<fn()>> = const { std::cell::Cell::new(None) };
    static AFTER_FIRST_WRITE: std::cell::Cell<Option<fn()>> = const { std::cell::Cell::new(None) };
}

fn observe_continuation() -> io::Result<()> {
    let launcher = unsafe { libc::getppid() };
    let group = unsafe { libc::getpgrp() };
    let session = unsafe { libc::getsid(0) };
    // Git can sit between the shell and 'git on'. Walk past launchers in
    // the job's own group to find the shell that can return its terminal.
    let mut controller = launcher;
    while controller > 1 && controller != session && unsafe { libc::getpgid(controller) } == group {
        let Some(parent) = crate::platform::parent_process(controller) else {
            break;
        };
        if parent <= 1 || parent == controller || unsafe { libc::getsid(parent) } != session {
            break;
        }
        controller = parent;
    }
    let controller_group = unsafe { libc::getpgid(controller) };
    let controller = (controller_group > 0
        && controller_group != group
        && unsafe { libc::getsid(controller) } == session)
        .then_some(controller)
        .or((session > 0 && session != group).then_some(session));
    unsafe {
        signal_hook_registry::register_sigaction(libc::SIGCONT, move |info| {
            // A shell can return foreground ownership before sending SIGCONT.
            // Treat the previous row as uncertain in that case.
            continued(Some(info.si_pid()) == controller);
        })?;
    }
    Ok(())
}

fn continued(from_job_controller: bool) {
    if from_job_controller {
        abandon_painted_row();
    }
    RESUME_EPOCH.fetch_add(1, Ordering::Release);
}

fn abandon_painted_row() {
    let _ = ROW.compare_exchange(
        ROW_PAINTED,
        ROW_ABANDONED,
        Ordering::AcqRel,
        Ordering::Relaxed,
    );
}

fn release_unpainted_row() {
    let _ = ROW.compare_exchange(
        ROW_PAINTED,
        ROW_UNUSED,
        Ordering::Release,
        Ordering::Relaxed,
    );
}

pub(crate) struct NativeOperation(u32);
impl NativeOperation {
    pub(crate) fn new(pid: u32) -> Self {
        if owns_terminal() {
            NATIVE.lock().unwrap().insert(pid as i32);
        }
        Self(pid)
    }
}
impl Drop for NativeOperation {
    fn drop(&mut self) {
        NATIVE.lock().unwrap().remove(&(self.0 as i32));
    }
}
pub(crate) fn reap(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    NATIVE.lock().unwrap().remove(&(child.id() as i32));
    child.wait()
}
pub(crate) fn try_wait(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    if !owns_terminal() {
        return child.try_wait();
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if crate::platform::siginfo_pid(&info) == 0 {
        Ok(None)
    } else {
        reap(child).map(Some)
    }
}

type Environment = [Option<std::ffi::OsString>; ENVIRONMENT.len()];

pub(crate) fn preparing() -> bool {
    PREPARING.load(Ordering::Acquire)
}
pub(crate) fn progress_enabled() -> bool {
    preparing() && VISIBLE.load(Ordering::Relaxed) && ROW.load(Ordering::Acquire) != ROW_ABANDONED
}
pub(crate) fn owns_terminal() -> bool {
    DIRECTORY_PATH.get().is_some()
}
fn foreground(fd: i32) -> bool {
    let group = unsafe { libc::tcgetpgrp(fd) };
    group <= 0 || group == unsafe { libc::getpgrp() }
}

pub(crate) fn clearing_failed() -> bool {
    CLEAR_FAILED.load(Ordering::Acquire)
}

pub(crate) fn owns_stderr_foreground() -> bool {
    foreground(libc::STDERR_FILENO)
}
const CLEAR_LINE: &[u8] = b"\r\x1b[2K";
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn progress_line(
    tick: usize,
    revision: &str,
    report: Option<&progress::Report>,
    width: usize,
) -> String {
    use std::fmt::Write as _;

    let (label, detail) = report.map_or(("git on", revision), |report| {
        (report.label.as_str(), report.detail.as_str())
    });
    let mut remaining = width.saturating_sub(1); // the spinner
    let mut reserve = |columns| {
        if columns < remaining {
            remaining -= columns + 1; // the field and its leading space
            true
        } else {
            false
        }
    };
    let count = report
        .and_then(|report| report.counts)
        .map(|(position, total)| (position, total, format!("{position}/{total}")))
        .filter(|(_, _, count)| reserve(count.len()));
    const BAR_WIDTH: usize = 4;
    let show_bar = count.is_some() && reserve(BAR_WIDTH);
    let transfer = report.and_then(|_| detail.split_once(" | "));
    let rate = transfer
        .map(|(_, rate)| rate)
        .filter(|rate| reserve(console::measure_text_width(rate)));
    let bytes = transfer
        .map(|(bytes, _)| bytes)
        .filter(|bytes| reserve(console::measure_text_width(bytes)));
    let label_width = remaining.saturating_sub(1);
    let label = if console::measure_text_width(label) > label_width {
        label.split_whitespace().next().unwrap_or(label)
    } else {
        label
    };
    let label = console::truncate_str(label, label_width, "");
    let spinner = console::style(SPINNER[tick]).for_stderr().cyan();
    let mut line = spinner.to_string();
    if !label.is_empty() {
        let label = console::style(label).for_stderr().bold().cyan();
        write!(line, " {label}").unwrap();
    }
    if let Some((position, total, count)) = count {
        write!(line, " {count}").unwrap();
        if show_bar {
            let filled = (u128::from(position) * BAR_WIDTH as u128 / u128::from(total)) as usize;
            let complete = console::style("━".repeat(filled)).for_stderr().green();
            let pending = console::style("─".repeat(BAR_WIDTH - filled))
                .for_stderr()
                .dim();
            write!(line, " {complete}{pending}").unwrap();
        }
    }
    for field in [rate, bytes].into_iter().flatten() {
        write!(line, " {}", console::style(field).for_stderr().dim()).unwrap();
    }
    if transfer.is_none() && !detail.is_empty() {
        write!(line, " {}", console::style(detail).for_stderr().dim()).unwrap();
    }
    console::truncate_str(&line, width, "").into_owned()
}

fn terminal_columns(fd: i32) -> Option<u16> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    let columns = unsafe { size.assume_init() }.ws_col;
    (columns != 0).then_some(columns)
}

#[cfg(test)]
#[test]
fn narrow_progress_preserves_the_entire_object_count() {
    let report = progress::Report {
        label: "Receiving objects".into(),
        detail: String::new(),
        counts: Some((1_000_000, 1_111_111)),
    };
    let line = progress_line(0, "HEAD", Some(&report), 39);
    assert!(line.contains("1000000/1111111"), "{line}");
    assert!(console::measure_text_width(&line) <= 39);
    assert!(!progress_line(0, "HEAD", Some(&report), 10).contains('/'));
    let report = progress::Report {
        detail: "1.25 MiB | 640.00 KiB/s".into(),
        counts: Some((100, 100)),
        ..report
    };
    let line = progress_line(0, "HEAD", Some(&report), 47);
    for field in ["100/100", "1.25 MiB", "640.00 KiB/s"] {
        assert!(line.contains(field), "{field}: {line}");
    }
    assert!(console::measure_text_width(&line) <= 47);
    let line = progress_line(0, "HEAD", Some(&report), 31);
    assert!(line.contains("100/100") && line.contains("━━━━"), "{line}");
    assert!(console::measure_text_width(&line) <= 31);
}

pub(crate) struct Drawing {
    output: Vec<u8>,
    terminal: File,
    painted: bool,
    tick: usize,
    frame: Option<Frame>,
}

struct Frame {
    columns: u16,
    resume_epoch: Option<usize>,
    started: bool,
}

impl Frame {
    fn new(columns: u16) -> Self {
        Self {
            columns,
            resume_epoch: Some(RESUME_EPOCH.load(Ordering::Acquire)),
            started: false,
        }
    }

    fn clear() -> Self {
        Self {
            columns: 0,
            resume_epoch: None,
            started: false,
        }
    }

    fn stale(&self, fd: i32) -> bool {
        self.resume_epoch
            .is_some_and(|epoch| RESUME_EPOCH.load(Ordering::Acquire) != epoch)
            || (self.columns != 0
                && terminal_columns(fd).is_some_and(|columns| columns < self.columns))
    }
}

impl Drawing {
    fn new(terminal: File) -> Self {
        Self {
            output: Vec::new(),
            terminal,
            painted: false,
            tick: 0,
            frame: None,
        }
    }

    fn for_stderr() -> Option<Self> {
        nonblocking_stderr_terminal().ok().map(Self::new)
    }

    fn fd(&self) -> i32 {
        self.terminal.as_raw_fd()
    }

    fn frame(&mut self, revision: &str) {
        if ROW.load(Ordering::Acquire) == ROW_ABANDONED {
            return;
        }
        // A job can stop during a write and resume after the shell has moved
        // the cursor. Restrict each frame to the current row, without wrapping.
        let fd = self.fd();
        let columns = terminal_columns(fd).unwrap_or_else(|| console::Term::stderr().size().1);
        let width = usize::from(columns.saturating_sub(1));
        if width != 0 {
            let frame = Frame::new(columns);
            let line = progress_line(self.tick, revision, progress::current().as_ref(), width);
            self.tick = (self.tick + 1) % SPINNER.len();
            self.frame = Some(frame);
            self.output.extend_from_slice(CLEAR_LINE);
            self.output.extend_from_slice(line.as_bytes());
        }
    }

    fn painted_here(&self) -> bool {
        self.painted && ROW.load(Ordering::Acquire) != ROW_ABANDONED
    }

    fn discard_frame(&mut self) {
        self.output.clear();
        self.frame = None;
        if self.painted_here() && owns_stderr_foreground() && console::colors_enabled_stderr() {
            self.output.extend_from_slice(b"\x1b[0m");
        }
    }

    fn flush(&mut self, deadline: Option<Instant>) -> bool {
        let fd = self.fd();
        let mut written = 0;
        while written < self.output.len() {
            if !foreground(fd) {
                abandon_painted_row();
                self.discard_frame();
                return true;
            }
            if ROW.load(Ordering::Acquire) == ROW_ABANDONED {
                self.discard_frame();
                return true;
            }
            // Claim the row before the kernel can paint it. A continuation
            // between write and its acknowledgement can then relinquish it.
            #[cfg(test)]
            if let Some(notify) = BEFORE_ROW_CLAIM.take() {
                notify();
            }
            let first_attempt = !self.painted
                && ROW
                    .compare_exchange(ROW_UNUSED, ROW_PAINTED, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok();
            if self.frame.as_ref().is_some_and(|frame| frame.stale(fd))
                || ROW.load(Ordering::Acquire) == ROW_ABANDONED
            {
                if first_attempt {
                    release_unpainted_row();
                }
                self.discard_frame();
                // A style reset left by the discarded frame must complete
                // on its own before a new frame can use the same buffer.
                written = 0;
                continue;
            }
            let pending = &self.output[written..];
            let count = unsafe { libc::write(fd, pending.as_ptr().cast(), pending.len()) };
            if count <= 0 && first_attempt {
                release_unpainted_row();
            }
            #[cfg(test)]
            if let Some(notify) = (count > 0 && !self.painted)
                .then(|| AFTER_FIRST_WRITE.take())
                .flatten()
            {
                notify();
            }
            if count > 0 {
                written += count as usize;
                if let Some(frame) = &mut self.frame {
                    frame.started = true;
                }
                self.painted = true;
                continue;
            }
            if count == 0
                || !matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted,
                )
            {
                break;
            }
            let Some(time) =
                deadline.and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            else {
                break;
            };
            let mut output = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe { libc::poll(&mut output, 1, time.as_millis().clamp(1, 10) as i32) };
        }
        self.output.drain(..written);
        if self.output.is_empty() {
            self.frame = None;
            true
        } else {
            false
        }
    }

    fn redraw(&mut self, deadline: Option<Instant>, render: impl FnOnce(&mut Self)) -> bool {
        if !self.flush(deadline) {
            return false;
        }
        render(self);
        self.flush(deadline)
    }
}

#[cfg(test)]
fn full_test_drawing() -> (Drawing, File, usize) {
    let (reader, terminal, filled) = crate::git::full_test_output(true);
    let fd = terminal.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let drawing = Drawing::new(terminal);
    (drawing, reader, filled)
}

#[cfg(test)]
#[test]
fn a_partial_frame_is_delivered_before_another_cursor_movement() {
    let (mut drawing, mut reader, filled) = full_test_drawing();
    let first = vec![b'x'; 2 * filled];
    drawing.output.write_all(&first).unwrap();
    let mut queued_cursor = false;
    let mut received = Vec::new();
    let length = filled + first.len() + CLEAR_LINE.len();
    let mut poll = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    while received.len() < length {
        drawing.redraw(None, |drawing| {
            assert!(drawing.output.is_empty());
            if !queued_cursor {
                drawing.output.extend_from_slice(CLEAR_LINE);
                queued_cursor = true;
            }
        });
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
        let mut buffer = [0; 4096];
        let count = reader.read(&mut buffer).unwrap();
        assert!(count > 0);
        received.extend_from_slice(&buffer[..count]);
    }
    assert!(received[..filled].iter().all(|&byte| byte == 0));
    assert_eq!(&received[filled..filled + first.len()], first);
    assert_eq!(&received[filled + first.len()..], CLEAR_LINE);
}

#[cfg(test)]
#[test]
fn a_queued_wide_frame_is_discarded_after_the_terminal_shrinks() {
    let (mut reader, terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal);
    drawing.frame = Some(Frame::new(120));
    drawing.output.extend_from_slice(&[b'x'; 100]);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let fd = drawing.terminal.as_raw_fd();
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) }, 0);
    assert!(drawing.redraw(None, |drawing| drawing
        .output
        .extend_from_slice(b"new frame")));
    let mut poll = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
    let mut transcript = [0; 128];
    let count = reader.read(&mut transcript).unwrap();
    assert_eq!(&transcript[..count], b"new frame");
}

#[cfg(test)]
#[test]
fn a_pending_frame_is_discarded_on_sigcont() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::a_pending_frame_is_discarded_on_sigcont",
    ) {
        return;
    }
    observe_continuation().unwrap();
    let (reader, terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal);
    drawing.frame("interrupted");
    assert!(!drawing.output.is_empty());
    assert_eq!(unsafe { libc::raise(libc::SIGCONT) }, 0);
    assert!(drawing.flush(None));
    drawing.frame("another interrupted frame");
    BEFORE_ROW_CLAIM.set(Some(|| continued(true)));
    assert!(drawing.flush(None));
    let mut ready = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut ready, 1, 0) },
        0,
        "wrote a resumed frame"
    );
}

#[cfg(test)]
fn continuation_fixture(test: &str, through_launcher: bool) -> Option<PathBuf> {
    const READY: &str = "GIT_ON_TEST_CONTINUE_READY";
    if let Some(ready) = std::env::var_os(READY) {
        return Some(ready.into());
    }
    if crate::git::diagnostics::run_isolated_test(test) {
        return None;
    }
    let control = tempfile::tempdir().unwrap();
    let ready = control.path().join("ready");
    let executable = std::env::current_exe().unwrap();
    let mut command = if through_launcher {
        // The sender is a job controller outside the runner's group.
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "\"$@\"; status=$?; exit \"$status\"", "launcher"])
            .arg(executable)
            .process_group(0);
        command
    } else {
        Command::new(executable)
    };
    let mut launcher = command
        .args(["--exact", test])
        .env(READY, &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let continued = loop {
        if let Some(pid) = std::fs::read_to_string(&ready)
            .ok()
            .and_then(|text| text.parse::<libc::pid_t>().ok())
        {
            break unsafe { libc::kill(pid, libc::SIGCONT) } == 0;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !continued {
        let child = launcher.id() as libc::pid_t;
        unsafe { libc::kill(if through_launcher { -child } else { child }, libc::SIGKILL) };
    }
    let status = launcher.wait().unwrap();
    assert!(
        continued && status.success(),
        "continued child exited with {status}"
    );
    None
}

#[cfg(test)]
fn wait_for_continuation(ready: &Path) {
    let epoch = RESUME_EPOCH.load(Ordering::Acquire);
    std::fs::write(ready, std::process::id().to_string()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while RESUME_EPOCH.load(Ordering::Acquire) == epoch {
        assert!(Instant::now() < deadline, "did not receive SIGCONT");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
#[test]
fn clearing_does_not_erase_the_shells_line_after_sigcont() {
    let Some(ready) = continuation_fixture(
        "terminal_ui::clearing_does_not_erase_the_shells_line_after_sigcont",
        true,
    ) else {
        return;
    };
    observe_continuation().unwrap();
    let (mut reader, mut terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal.try_clone().unwrap());
    drawing.frame("before stop");
    let mut painted = vec![0; drawing.output.len()];
    assert!(drawing.flush(None));
    read_pty(&mut reader, &mut painted);
    let marker = b"shell output without a newline";
    terminal.write_all(marker).unwrap();
    drawing.frame("pending before stop");
    wait_for_continuation(&ready);
    assert!(drawing.redraw(None, |drawing| drawing.frame("after resume")));
    clear(&mut Some(drawing));
    let mut received = vec![0; marker.len()];
    read_pty(&mut reader, &mut received);
    assert_eq!(received, marker);
    let mut input = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut input, 1, 100) },
        0,
        "clear wrote over the shell marker"
    );
}

#[cfg(test)]
#[test]
fn continuation_from_the_same_group_keeps_the_painted_line() {
    let Some(ready) = continuation_fixture(
        "terminal_ui::continuation_from_the_same_group_keeps_the_painted_line",
        false,
    ) else {
        return;
    };
    observe_continuation().unwrap();
    let (mut reader, terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal.try_clone().unwrap());
    drawing.frame("before signal");
    let mut painted = vec![0; drawing.output.len()];
    assert!(drawing.flush(None));
    read_pty(&mut reader, &mut painted);
    wait_for_continuation(&ready);
    clear(&mut Some(drawing));
    let mut cleared = [0; CLEAR_LINE.len()];
    read_pty(&mut reader, &mut cleared);
    assert_eq!(cleared, CLEAR_LINE);
}

#[cfg(test)]
fn read_pty(reader: &mut File, mut bytes: &mut [u8]) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut input = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    while !bytes.is_empty() {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        assert_eq!(
            unsafe { libc::poll(&mut input, 1, remaining) },
            1,
            "PTY output timed out"
        );
        let count = reader.read(bytes).unwrap();
        assert_ne!(count, 0);
        bytes = &mut bytes[count..];
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn pending_progress_is_discarded_when_the_shell_takes_foreground() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::pending_progress_is_discarded_when_the_shell_takes_foreground",
    ) {
        return;
    }
    let (mut reader, terminal) = crate::git::diagnostics::output_pair(true);
    let fd = terminal.as_raw_fd();
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    assert!(unsafe { libc::setsid() } > 0);
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCSCTTY, 0) }, 0);
    let mut drawing = Drawing::new(terminal);
    drawing.frame("stale");
    let mut shell = Command::new("/bin/sleep")
        .arg("2")
        .process_group(0)
        .spawn()
        .unwrap();
    assert_eq!(unsafe { libc::tcsetpgrp(fd, shell.id() as libc::pid_t) }, 0);
    let discarded = drawing.flush(None) && drawing.output.is_empty();
    let mut ready = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let quiet = unsafe { libc::poll(&mut ready, 1, 0) } == 0;
    let stderr = unsafe { libc::dup2(fd, libc::STDERR_FILENO) };
    CLEAR_FAILED.store(true, Ordering::Release);
    crate::git::write_run_error(format_args!("failure in a background job\n"));
    shell.kill().unwrap();
    shell.wait().unwrap();
    assert_eq!(stderr, libc::STDERR_FILENO);
    assert!(
        discarded && quiet,
        "drew a pending frame after losing foreground"
    );
    const EXPECTED: &[u8] = b"failure in a background job\r\n";
    let mut diagnostic = [0; EXPECTED.len()];
    read_pty(&mut reader, &mut diagnostic);
    assert_eq!(&diagnostic, EXPECTED);
}

#[cfg(test)]
#[test]
fn continuation_during_the_first_write_relinquishes_its_row() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::continuation_during_the_first_write_relinquishes_its_row",
    ) {
        return;
    }
    let (mut reader, mut terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal.try_clone().unwrap());
    drawing.frame("first frame");
    let mut painted = vec![0; drawing.output.len()];
    AFTER_FIRST_WRITE.set(Some(|| continued(true)));
    assert!(drawing.flush(None));
    read_pty(&mut reader, &mut painted);
    let marker = b"shell row";
    terminal.write_all(marker).unwrap();
    clear(&mut Some(drawing));
    let mut received = [0; 9];
    read_pty(&mut reader, &mut received);
    assert_eq!(&received, marker);
    let mut input = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut input, 1, 100) }, 0);
}

#[cfg(test)]
#[test]
#[cfg(target_os = "linux")]
fn terminal_handoff_stops_when_clearing_failed() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::terminal_handoff_stops_when_clearing_failed",
    ) {
        return;
    }
    let (_reader, terminal) = crate::git::diagnostics::output_pair(true);
    let fd = terminal.as_raw_fd();
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    assert!(unsafe { libc::setsid() } > 0);
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCSCTTY, 0) }, 0);
    CLEAR_FAILED.store(true, Ordering::Release);
    let error = crate::git::run(Command::new("/bin/true"), false).unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot clear the startup indicator"));
}

#[cfg(test)]
#[test]
fn clearing_preserves_style_reset_from_a_discarded_frame() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::clearing_preserves_style_reset_from_a_discarded_frame",
    ) {
        return;
    }
    console::set_colors_enabled_stderr(true);
    let (mut drawing, mut reader, filled) = full_test_drawing();
    let _terminal = drawing.terminal.try_clone().unwrap();
    drawing.painted = true;
    ROW.store(ROW_PAINTED, Ordering::Release);
    drawing.frame("stale frame");
    drawing.frame.as_mut().unwrap().started = true;
    RESUME_EPOCH.fetch_add(1, Ordering::AcqRel);
    drawing.redraw(None, |drawing| drawing.frame("never drawn"));
    read_pty(&mut reader, &mut vec![0; filled]);
    clear(&mut Some(drawing));
    let mut reset = [0; 4];
    read_pty(&mut reader, &mut reset);
    assert_eq!(&reset, b"\x1b[0m");
    let mut cleared = [0; CLEAR_LINE.len()];
    read_pty(&mut reader, &mut cleared);
    assert_eq!(cleared, CLEAR_LINE);
}

#[cfg(test)]
#[test]
fn clearing_discards_an_unwritten_progress_frame() {
    if crate::git::diagnostics::run_isolated_test(
        "terminal_ui::clearing_discards_an_unwritten_progress_frame",
    ) {
        return;
    }
    let (mut reader, terminal) = crate::git::diagnostics::output_pair(true);
    let mut drawing = Drawing::new(terminal.try_clone().unwrap());
    drawing.frame("visible");
    let mut painted = vec![0; drawing.output.len()];
    assert!(drawing.flush(None));
    read_pty(&mut reader, &mut painted);
    drawing.frame("never drawn");
    clear(&mut Some(drawing));
    let mut received = [0; CLEAR_LINE.len()];
    read_pty(&mut reader, &mut received);
    assert_eq!(received, CLEAR_LINE);
    let mut input = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut input, 1, 100) },
        0,
        "clear rendered a queued frame"
    );
}

#[cfg(test)]
#[test]
fn clearing_delivers_the_frame_and_its_clear_when_the_terminal_recovers() {
    let (mut drawing, mut reader, filled) = full_test_drawing();
    let _caller_terminal = drawing.terminal.try_clone().unwrap();
    drawing.output.write_all(b"frame").unwrap();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        let mut prefill = vec![0; filled];
        reader.read_exact(&mut prefill).unwrap();
        let mut received = [0; 10];
        let mut offset = 0;
        let mut input = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while offset != received.len() {
            assert_eq!(unsafe { libc::poll(&mut input, 1, 1000) }, 1);
            let count = reader.read(&mut received[offset..]).unwrap();
            assert_ne!(count, 0);
            offset += count;
        }
        received
    });
    drawing.redraw(Some(Instant::now() + CLEAR_GRACE), |drawing| {
        drawing.output.write_all(b"clear").unwrap();
    });
    drop(drawing);
    assert_eq!(&thread.join().unwrap(), b"frameclear");
}

pub(crate) fn nonblocking_stderr_terminal() -> io::Result<File> {
    // Reopen the actual stderr terminal with an independent file description
    // so animation cannot block or change flags on the caller's descriptor.
    let mut path = [0; libc::PATH_MAX as usize];
    let fd = libc::STDERR_FILENO;
    if !stderr_is_writable() {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    let result = unsafe { libc::ttyname_r(fd, path.as_mut_ptr(), path.len()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    let path = OsStr::from_bytes(unsafe { CStr::from_ptr(path.as_ptr()) }.to_bytes());
    let terminal = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW)
        .open(path)?;
    let identity = |descriptor| {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok((stat.st_dev, stat.st_ino, stat.st_rdev))
    };
    if identity(fd)? != identity(terminal.as_raw_fd())? {
        return Err(io::Error::other(
            "stderr terminal changed during preparation",
        ));
    }
    Ok(terminal)
}

fn stderr_is_writable() -> bool {
    let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
    flags >= 0 && flags & libc::O_ACCMODE != libc::O_RDONLY
}

fn stderr_disconnected() -> bool {
    let mut output = libc::pollfd {
        fd: libc::STDERR_FILENO,
        // Darwin reports a PTY hangup when the caller requests readiness.
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut output, 1, 0) };
    ready > 0 && output.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

fn clear(drawing: &mut Option<Drawing>) {
    if let Some(mut drawing) = drawing.take() {
        // An unwritten frame can be discarded without moving the cursor.
        // Finish a partial frame before sending another escape sequence.
        if drawing.frame.as_ref().is_some_and(|frame| !frame.started) {
            drawing.output.clear();
            drawing.frame = None;
        }
        // Only clear a frame after its cursor movements have been delivered.
        // A different foreground job can move the cursor, invalidating the frame.
        let cleared = !owns_stderr_foreground()
            || !drawing.painted_here()
            || drawing.redraw(Some(Instant::now() + CLEAR_GRACE), |drawing| {
                drawing.frame = Some(Frame::clear());
                drawing.output.extend_from_slice(CLEAR_LINE);
            });
        if !cleared && !stderr_disconnected() && drawing.painted_here() {
            CLEAR_FAILED.store(true, Ordering::Release);
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }
}
pub(crate) fn native_terminal_lease(
    transfer: bool,
) -> Option<MutexGuard<'static, Option<Drawing>>> {
    transfer.then(|| {
        let mut drawing = DRAW.lock().unwrap_or_else(|error| error.into_inner());
        clear(&mut drawing);
        drawing
    })
}

pub(crate) fn terminal_handoff(
    transfer: bool,
) -> Result<Option<MutexGuard<'static, Option<Drawing>>>> {
    let lease = native_terminal_lease(transfer);
    if transfer {
        ensure_cleared()?;
    }
    Ok(lease)
}

#[doc(hidden)]
#[derive(Debug)]
pub struct TerminalClearFailed;

impl std::fmt::Display for TerminalClearFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cannot clear the startup indicator on stderr")
    }
}

impl std::error::Error for TerminalClearFailed {}

fn ensure_cleared() -> Result<()> {
    if clearing_failed() {
        return Err(TerminalClearFailed.into());
    }
    Ok(())
}

pub(crate) fn stop_drawing() {
    if preparing() {
        VISIBLE.store(false, Ordering::Relaxed);
        drop(native_terminal_lease(true));
    }
}

pub struct StartupProgress {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    original_stop: Option<libc::sigaction>,
    _directory: Option<tempfile::TempDir>,
}
impl StartupProgress {
    pub fn start(invoked: Instant, revision: &OsStr, run: &Path) -> Result<Self> {
        let visible = std::io::stderr().is_terminal()
            && stderr_is_writable()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
        if visible {
            observe_continuation()?;
        }
        CLEAR_FAILED.store(false, Ordering::Release);
        PREPARING.store(true, Ordering::Release);
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")
            .ok();
        let mut directory = None;
        let mut listener = None;
        let mut original_stop = None;
        if tty.is_some() {
            let private = tempfile::Builder::new()
                // Keep askpass paths within the old flat layout's length.
                .prefix("t")
                .rand_bytes(3)
                .tempdir_in(run)?;
            let executable = std::env::current_exe()?;
            symlink(&executable, private.path().join("git-askpass"))?;
            symlink(&executable, private.path().join("ssh-askpass"))?;
            listener = Some(in_directory(private.path(), || {
                UnixListener::bind("socket")
            })?);
            listener.as_ref().unwrap().set_nonblocking(true)?;
            DIRECTORY_PATH
                .set(private.path().to_path_buf())
                .map_err(|_| anyhow::anyhow!("terminal UI already started"))?;
            original_stop = install_stop_handler()?;
            directory = Some(private);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        VISIBLE.store(visible, Ordering::Relaxed);
        let revision = progress::display(&revision.to_string_lossy());
        if !visible && listener.is_none() {
            return Ok(Self {
                stop,
                worker: None,
                original_stop,
                _directory: directory,
            });
        }
        let worker = std::thread::Builder::new()
            .name("preparation-terminal".into())
            .spawn(move || {
                let mut last_tick = invoked;
                let mut echo = None;
                while !stopping.load(Ordering::Acquire) {
                    if ROW.load(Ordering::Acquire) == ROW_ABANDONED
                        && VISIBLE.swap(false, Ordering::Relaxed)
                    {
                        pause_indication(&mut echo);
                    }
                    if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
                        pause_indication(&mut echo);
                        suspend(original_stop.as_ref());
                    }
                    if let (Some(listener), Some(tty)) = (&listener, &tty) {
                        if let Ok((mut stream, _)) = listener.accept() {
                            pause_indication(&mut echo);
                            let _ = respond(&mut stream, tty, &stopping, original_stop.as_ref());
                        }
                    }
                    if VISIBLE.load(Ordering::Relaxed)
                        && invoked.elapsed() >= Duration::from_millis(500)
                        && last_tick.elapsed() >= Duration::from_millis(100)
                    {
                        let mut drawing = DRAW.lock().unwrap_or_else(|error| error.into_inner());
                        if cancelled(&stopping) {
                            break;
                        }
                        let foreground = foreground(libc::STDERR_FILENO);
                        if !foreground {
                            abandon_painted_row();
                        }
                        if foreground && progress_enabled() {
                            if drawing.is_none() {
                                *drawing = Drawing::for_stderr();
                            }
                            if let Some(frame) = &mut *drawing {
                                // Preserve queued input for the command while
                                // preventing echo from moving the cursor.
                                if echo.is_none() {
                                    echo = Echo::new(libc::STDERR_FILENO).ok();
                                }
                                if let Some(echo) = &mut echo {
                                    let _ = echo.disable();
                                }
                                frame.redraw(None, |frame| frame.frame(&revision));
                            } else {
                                VISIBLE.store(false, Ordering::Relaxed);
                            }
                        } else {
                            clear(&mut drawing);
                            drop(echo.take());
                        }
                        last_tick = Instant::now();
                    }
                    std::thread::park_timeout(Duration::from_millis(10));
                }
                pause_indication(&mut echo);
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
            original_stop,
            _directory: directory,
        })
    }
    pub fn finish(&mut self) -> Result<()> {
        PREPARING.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
            if let Some(original) = self.original_stop.take() {
                unsafe {
                    libc::sigaction(libc::SIGTSTP, &original, std::ptr::null_mut());
                }
                if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
                    unsafe {
                        libc::raise(libc::SIGTSTP);
                    }
                }
            }
        }
        ensure_cleared()
    }
}
fn pause_indication(echo: &mut Option<Echo>) {
    let _drawing = native_terminal_lease(true);
    drop(echo.take());
}
impl Drop for StartupProgress {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

// Called before application worker threads start, or by a standalone askpass
// process. Relative Unix socket names also work with long TMPDIR paths.
fn in_directory<T>(
    directory: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let original = File::open(".")?;
    std::env::set_current_dir(directory)?;
    let result = operation();
    if unsafe { libc::fchdir(original.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    result
}
extern "C" fn request_stop(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Release);
}
fn stop_action() -> libc::sigaction {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = request_stop as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    action
}
fn install_stop_handler() -> std::io::Result<Option<libc::sigaction>> {
    let mut original = std::mem::MaybeUninit::uninit();
    if unsafe { libc::sigaction(libc::SIGTSTP, std::ptr::null(), original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let original = unsafe { original.assume_init() };
    if original.sa_sigaction != libc::SIG_DFL {
        return Ok(None);
    }
    if unsafe { libc::sigaction(libc::SIGTSTP, &stop_action(), std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Some(original))
}
fn suspend(original: Option<&libc::sigaction>) {
    let _drawing = native_terminal_lease(true);
    if let Some(original) = original {
        let operations = NATIVE.lock().unwrap();
        unsafe {
            for group in operations.iter() {
                libc::kill(-group, libc::SIGSTOP);
            }
            libc::sigaction(libc::SIGTSTP, original, std::ptr::null_mut());
            libc::raise(libc::SIGTSTP);
            libc::sigaction(libc::SIGTSTP, &stop_action(), std::ptr::null_mut());
            for group in operations.iter() {
                libc::kill(-group, libc::SIGCONT);
            }
        }
    }
}
fn cancelled(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire) || crate::git::cancelled_signal().is_some()
}
fn read_request(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    stop: &AtomicBool,
    original_stop: Option<&libc::sigaction>,
) -> std::io::Result<()> {
    let mut offset = 0;
    while offset < buffer.len() && !cancelled(stop) {
        if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
            suspend(original_stop);
        }
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    if offset == buffer.len() {
        Ok(())
    } else {
        Err(std::io::ErrorKind::Interrupted.into())
    }
}
fn connected(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        ) != 0
    }
}
struct Echo {
    fd: i32,
    original: libc::termios,
    active: bool,
}
impl Echo {
    fn new(fd: i32) -> std::io::Result<Self> {
        let mut original = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            original: unsafe { original.assume_init() },
            active: false,
        })
    }
    fn disable(&mut self) -> std::io::Result<()> {
        if self.active {
            return Ok(());
        }
        let mut quiet = self.original;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &quiet) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.active = true;
        Ok(())
    }
    fn restore(&mut self) {
        if self.active {
            unsafe {
                let mut blocked = std::mem::zeroed();
                let mut original = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGTTOU);
                let masked = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut original) == 0;
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
                if masked {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut());
                }
            }
            self.active = false;
        }
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.restore();
    }
}
fn acquire_terminal(
    fd: i32,
    stop: &AtomicBool,
    original_stop: Option<&libc::sigaction>,
) -> std::io::Result<()> {
    while !foreground(fd) && !cancelled(stop) {
        if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
            suspend(original_stop);
        }
        let mut action = std::mem::MaybeUninit::uninit();
        let mut blocked = std::mem::MaybeUninit::uninit();
        unsafe {
            if libc::sigaction(libc::SIGTTOU, std::ptr::null(), action.as_mut_ptr()) == 0
                && libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), blocked.as_mut_ptr())
                    == 0
                && action.assume_init().sa_sigaction == libc::SIG_DFL
                && libc::sigismember(&blocked.assume_init(), libc::SIGTTOU) == 0
            {
                // The kernel checks background ownership and stops the whole
                // job atomically; a concurrent fg cannot lose its SIGCONT.
                let operations = NATIVE.lock().unwrap();
                for group in operations.iter() {
                    libc::kill(-group, libc::SIGSTOP);
                }
                libc::tcsetpgrp(fd, libc::getpgrp());
                for group in operations.iter() {
                    libc::kill(-group, libc::SIGCONT);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if cancelled(stop) {
        Err(std::io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}
fn respond(
    stream: &mut UnixStream,
    mut tty: &File,
    stop: &AtomicBool,
    original_stop: Option<&libc::sigaction>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut header = [0; 5];
    read_request(stream, &mut header, stop, original_stop)?;
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    anyhow::ensure!(length <= 1024 * 1024, "askpass prompt too long");
    let mut prompt = vec![0; length];
    read_request(stream, &mut prompt, stop, original_stop)?;
    acquire_terminal(tty.as_raw_fd(), stop, original_stop)?;
    let mut lease = terminal_handoff(true)?;
    let mut echo = Echo::new(tty.as_raw_fd())?;
    if header[0] != b'N' {
        echo.disable()?;
    }
    tty.write_all(&prompt)?;
    let mut answer = Vec::new();
    let mut accepted = false;
    while !cancelled(stop) && connected(stream) {
        if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
            echo.restore();
            tty.write_all(b"\r\x1b[2K")?;
            drop(lease.take());
            suspend(original_stop);
            acquire_terminal(tty.as_raw_fd(), stop, original_stop)?;
            lease = terminal_handoff(true)?;
            if header[0] != b'N' {
                echo.disable()?;
            }
            tty.write_all(&prompt)?;
        }
        if header[0] != b'N' {
            let mut byte = [0];
            match tty.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    answer.push(byte[0]);
                    if byte[0] == b'\n' {
                        accepted = true;
                        break;
                    }
                    continue;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    echo.restore();
    if foreground(tty.as_raw_fd()) {
        tty.write_all(if header[0] == b'N' {
            b"\r\x1b[2K"
        } else {
            b"\n"
        })?;
    }
    if accepted {
        stream.write_all(&[1])?;
        stream.write_all(&answer)?;
    }
    Ok(())
}

pub(crate) fn configure(command: &mut Command) {
    if preparing() {
        if let Some(directory) = DIRECTORY_PATH.get() {
            for (key, saved) in ENVIRONMENT {
                if let Some(value) = std::env::var_os(key) {
                    command.env(saved, value);
                } else {
                    command.env_remove(saved);
                }
            }
            command
                .env(DIRECTORY, directory)
                .env("GIT_ASKPASS", directory.join("git-askpass"));
            let requirement = std::env::var("SSH_ASKPASS_REQUIRE").unwrap_or_default();
            let display = ["DISPLAY", "WAYLAND_DISPLAY"]
                .iter()
                .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
            // An explicitly selected compiled GUI helper must still be chosen
            // by OpenSSH, whose compiled-in executable path is platform-specific.
            let native_default = std::env::var_os("SSH_ASKPASS").is_none()
                && (requirement.eq_ignore_ascii_case("force")
                    || requirement.eq_ignore_ascii_case("prefer") && display);
            if !native_default {
                command
                    .env("SSH_ASKPASS", directory.join("ssh-askpass"))
                    .env("SSH_ASKPASS_REQUIRE", "force");
            }
        }
    }
}
pub(crate) fn core_askpass(command: &mut Command, program: &OsStr) {
    if preparing() && owns_terminal() {
        command.env(CORE_ASKPASS, program);
    }
}
fn restore(command: &mut Command, environment: &Environment) {
    for ((key, saved), value) in ENVIRONMENT.iter().zip(environment) {
        command.env_remove(saved);
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    command.env_remove(DIRECTORY).env_remove(CORE_ASKPASS);
}
fn askpass(directory: &Path, ssh: bool) -> Result<i32> {
    let environment: Environment = ENVIRONMENT.map(|(_, saved)| std::env::var_os(saved));
    let value = |index: usize| environment[index].as_deref();
    let hint = std::env::var_os("SSH_ASKPASS_PROMPT");
    let program = if ssh {
        let requirement = value(2).and_then(OsStr::to_str);
        let required =
            |expected| requirement.is_some_and(|value| value.eq_ignore_ascii_case(expected));
        let notification = hint.as_deref() == Some(OsStr::new("none"));
        let display = [value(3), value(4)];
        let display_available = display.iter().flatten().any(|value| !value.is_empty());
        if hint.as_deref() == Some(OsStr::new("confirm"))
            && !(required("force") || display_available && !required("never"))
        {
            return Ok(1);
        }
        let configured = if notification {
            requirement == Some("force") || display.iter().any(Option::is_some)
        } else {
            required("force")
                || (display_available
                    && (required("prefer") || hint.as_deref() == Some(OsStr::new("confirm"))))
        };
        configured
            .then(|| value(1).map(OsStr::to_os_string))
            .flatten()
    } else {
        value(0)
            .map(OsStr::to_os_string)
            .or_else(|| std::env::var_os(CORE_ASKPASS))
            .or_else(|| value(1).map(OsStr::to_os_string))
    };
    if ssh && program.as_ref().is_some_and(|program| program.is_empty()) {
        return Ok(1);
    }
    if let Some(program) = program.filter(|program| !program.is_empty()) {
        let mut command = Command::new(program);
        command.args(std::env::args_os().skip(1));
        restore(&mut command, &environment);
        if ssh && hint.as_deref() == Some(OsStr::new("none")) {
            return Err(command.exec().into());
        }
        let output = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .and_then(|child| child.wait_with_output());
        if let Ok(output) = output {
            if output.status.success() {
                std::io::stdout().write_all(&output.stdout)?;
                return Ok(0);
            }
            if ssh {
                return Ok(output.status.code().unwrap_or(1));
            }
        } else if ssh {
            return Ok(1);
        }
    }
    if !ssh
        && std::env::var("GIT_TERMINAL_PROMPT")
            .is_ok_and(|value| !git2::Config::parse_bool(&value).unwrap_or(false))
    {
        return Ok(1);
    }
    let mut stream = in_directory(directory, || UnixStream::connect("socket"))?;
    let prompt = std::env::args_os().nth(1).unwrap_or_default();
    let kind = if ssh && hint.as_deref() == Some(OsStr::new("none")) {
        b'N'
    } else {
        b'P'
    };
    stream.write_all(&[kind])?;
    stream.write_all(&u32::try_from(prompt.len())?.to_be_bytes())?;
    stream.write_all(prompt.as_bytes())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    if response.first() == Some(&1) {
        std::io::stdout().write_all(&response[1..])?;
        Ok(0)
    } else {
        Ok(1)
    }
}
pub fn invoke_askpass() -> Option<i32> {
    let directory = std::env::var_os(DIRECTORY)?;
    let executable = std::env::args_os().next()?;
    let name = Path::new(&executable).file_name()?;
    if name != "git-askpass" && name != "ssh-askpass" {
        return None;
    }
    Some(askpass(Path::new(&directory), name == "ssh-askpass").unwrap_or(1))
}
