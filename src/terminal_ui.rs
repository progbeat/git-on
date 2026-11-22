//! Preparation owns terminal rendering and standard Git/SSH prompt requests.
use anyhow::Result;
mod progress;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
pub(crate) use progress::{diagnostics as progress_diagnostics, NativeProgress, PackProgress};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{IsTerminal, Read, Write},
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
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const DIRECTORY: &str = "GIT_ON_TERMINAL_DIRECTORY";
const CORE_ASKPASS: &str = "GIT_ON_TERMINAL_CORE_ASKPASS";
const ENVIRONMENT: [&str; 5] = [
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];
static DIRECTORY_PATH: OnceLock<PathBuf> = OnceLock::new();
static PREPARING: AtomicBool = AtomicBool::new(false);
static VISIBLE: AtomicBool = AtomicBool::new(false);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static DRAW: Mutex<Option<ProgressBar>> = Mutex::new(None);
static NATIVE: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

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

type Environment = Vec<Option<Vec<u8>>>;

pub(crate) fn preparing() -> bool {
    PREPARING.load(Ordering::Acquire)
}
pub(crate) fn progress_enabled() -> bool {
    preparing() && VISIBLE.load(Ordering::Relaxed)
}
pub(crate) fn configure_fetch(command: &mut Command) {
    if progress_enabled() {
        command.arg("--progress");
    }
}
pub(crate) fn owns_terminal() -> bool {
    DIRECTORY_PATH.get().is_some()
}
fn foreground(fd: i32) -> bool {
    let group = unsafe { libc::tcgetpgrp(fd) };
    group <= 0 || group == unsafe { libc::getpgrp() }
}
fn clear(drawing: &mut Option<ProgressBar>) {
    if let Some(bar) = drawing.take() {
        if !foreground(std::io::stderr().as_raw_fd()) {
            bar.set_draw_target(ProgressDrawTarget::hidden());
        }
        bar.finish_and_clear();
    }
}
pub(crate) fn native_terminal_lease(
    transfer: bool,
) -> Option<MutexGuard<'static, Option<ProgressBar>>> {
    transfer.then(|| {
        let mut drawing = DRAW.lock().unwrap_or_else(|error| error.into_inner());
        clear(&mut drawing);
        drawing
    })
}

pub struct StartupProgress {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    original_stop: Option<libc::sigaction>,
    _directory: Option<tempfile::TempDir>,
}
impl StartupProgress {
    pub fn start(invoked: Instant, revision: &OsStr, run: &Path) -> Result<Self> {
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
            let environment: Environment = ENVIRONMENT
                .iter()
                .map(|key| std::env::var_os(key).map(|value| value.as_bytes().to_vec()))
                .collect();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(private.path().join("environment"))?;
            serde_json::to_writer(file, &environment)?;
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
        let visible = std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
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
                let mut echo = visible
                    .then(|| Echo::new(std::io::stderr().as_raw_fd()).ok())
                    .flatten();
                while !stopping.load(Ordering::Acquire) {
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
                    if visible
                        && invoked.elapsed() >= Duration::from_millis(500)
                        && last_tick.elapsed() >= Duration::from_millis(100)
                    {
                        let mut drawing = DRAW.lock().unwrap_or_else(|error| error.into_inner());
                        if foreground(std::io::stderr().as_raw_fd()) {
                            // Keep kernel echo from moving the indicator onto
                            // another line. Queued input stays in the terminal
                            // for the requested command, without being read here.
                            if let Some(echo) = &mut echo {
                                let _ = echo.disable();
                            }
                            let bar = drawing.get_or_insert_with(|| {
                                let _ = std::io::stderr().write_all(b"\r");
                                let bar = ProgressBar::new_spinner();
                                bar.set_style(
                                    ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold.cyan} {wide_msg:.dim}")
                                        .expect("static progress template")
                                        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
                                );
                                bar
                            });
                            let report = progress::current();
                            let counts = report.as_ref().and_then(|report| report.counts);
                            if bar.length().is_some() != counts.is_some() {
                                let template = if counts.is_some() {
                                    "{spinner:.cyan} {prefix:.bold.cyan} {wide_msg:.dim}\n  {wide_bar:.green/dim} {percent}% {pos}/{len}"
                                } else {
                                    "{spinner:.cyan} {prefix:.bold.cyan} {wide_msg:.dim}"
                                };
                                bar.set_style(ProgressStyle::with_template(template).expect("static progress template").tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ").progress_chars("━╸─"));
                            }
                            if let Some((position, total)) = counts {
                                bar.set_length(total);
                                bar.set_position(position);
                            } else {
                                bar.unset_length();
                            }
                            let (label, detail) = report.map_or_else(
                                || ("git on".into(), revision.clone()),
                                |report| (report.label, report.detail),
                            );
                            if bar.prefix() != label {
                                bar.set_prefix(label);
                            }
                            if bar.message() != detail {
                                bar.set_message(detail);
                            }
                            bar.tick();
                        } else {
                            clear(&mut drawing);
                            if let Some(echo) = &mut echo {
                                echo.restore();
                            }
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
    pub fn finish(&mut self) {
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
    }
}
fn pause_indication(echo: &mut Option<Echo>) {
    let _drawing = native_terminal_lease(true);
    if let Some(echo) = echo {
        echo.restore();
    }
}
impl Drop for StartupProgress {
    fn drop(&mut self) {
        self.finish();
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
    let mut lease = native_terminal_lease(true);
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
            lease = native_terminal_lease(true);
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
    for (key, value) in ENVIRONMENT.iter().zip(environment) {
        match value {
            Some(value) => {
                command.env(key, OsStr::from_bytes(value));
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    command.env_remove(DIRECTORY).env_remove(CORE_ASKPASS);
}
fn askpass(directory: &Path, ssh: bool) -> Result<i32> {
    let environment: Environment =
        serde_json::from_reader(File::open(directory.join("environment"))?)?;
    let value = |index: usize| environment[index].as_deref().map(OsStr::from_bytes);
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
