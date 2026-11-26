use anyhow::{Context, Result};
#[cfg(any(test, target_os = "linux"))]
use std::fs::File;
#[cfg(test)]
use std::os::fd::AsRawFd;
use std::{
    collections::VecDeque,
    io::{self, Write},
    os::fd::RawFd,
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc, Arc, Condvar, Mutex, PoisonError,
    },
    time::{Duration, Instant},
};

const DIAGNOSTIC_LIMIT: usize = 16 * 1024;
const DIAGNOSTIC_OMITTED: &[u8] = b"\n[... Git-on diagnostic truncated ...]\n";

#[derive(Default)]
struct DiagnosticBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: bool,
}

impl std::fmt::Write for DiagnosticBuffer {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        const HALF: usize = DIAGNOSTIC_LIMIT / 2;
        let bytes = value.as_bytes();
        let prefix = (HALF - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..prefix]);
        let rest = &bytes[prefix..];
        let kept = rest.len().min(HALF);
        let overflow = (self.tail.len() + kept).saturating_sub(HALF);
        self.tail.drain(..overflow);
        self.tail.extend(rest[rest.len() - kept..].iter().copied());
        self.omitted |= overflow != 0 || kept != rest.len();
        Ok(())
    }
}

pub(super) fn format_diagnostic(message: std::fmt::Arguments<'_>) -> Arc<Vec<u8>> {
    let mut buffer = DiagnosticBuffer::default();
    std::fmt::write(&mut buffer, message).expect("format Git diagnostic");
    let middle = if buffer.omitted {
        DIAGNOSTIC_OMITTED
    } else {
        &[]
    };
    buffer.head.reserve_exact(middle.len() + buffer.tail.len());
    buffer.head.extend_from_slice(middle);
    buffer.head.extend(buffer.tail);
    fit_diagnostic(Arc::new(buffer.head), DIAGNOSTIC_LIMIT)
}

#[cfg(test)]
#[test]
fn formatted_diagnostic_retains_both_ends_in_a_bounded_buffer() {
    let body = "é".repeat(DIAGNOSTIC_LIMIT);
    let bytes = format_diagnostic(format_args!(
        "first context\n{body}\n{body}\nlast context\n"
    ));
    assert!(bytes.capacity() <= DIAGNOSTIC_LIMIT);
    assert!(std::str::from_utf8(&bytes).is_ok());
    assert!(bytes.starts_with(b"first context\n"));
    assert!(bytes.ends_with(b"\nlast context\n"));
    assert!(bytes
        .windows(DIAGNOSTIC_OMITTED.len())
        .any(|part| part == DIAGNOSTIC_OMITTED));
}

fn fit_diagnostic(bytes: Arc<Vec<u8>>, limit: usize) -> Arc<Vec<u8>> {
    if bytes.len() <= limit {
        return bytes;
    }
    let Some(payload) = limit.checked_sub(DIAGNOSTIC_OMITTED.len()) else {
        return Arc::new(bytes[bytes.len() - limit..].to_vec());
    };
    // Keep the initial error and final context, including when Git has
    // emitted arbitrary bytes rather than valid UTF-8.
    let mut head = payload / 2;
    let mut tail = bytes.len() - (payload - head);
    while head > 0 && bytes[head] & 0xc0 == 0x80 {
        head -= 1;
    }
    while tail < bytes.len() && bytes[tail] & 0xc0 == 0x80 {
        tail += 1;
    }
    Arc::new([&bytes[..head], DIAGNOSTIC_OMITTED, &bytes[tail..]].concat())
}

const RECOVERY_GRACE: Duration = Duration::from_millis(250);
// The caller may read stdout all the way to EOF before consuming stderr.
const DIAGNOSTIC_GRACE: Duration = Duration::from_millis(250);
const CLOSE_GRACE: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: Duration = Duration::from_millis(10);
const WAITING: u8 = 0;
const WRITING: u8 = 1;
const CANCELLED: u8 = 2;
const FINISHED: u8 = 3;

struct OutputGate {
    busy: Mutex<bool>,
    available: Condvar,
}

impl OutputGate {
    const fn new() -> Self {
        Self {
            busy: Mutex::new(false),
            available: Condvar::new(),
        }
    }

    fn try_acquire(&'static self) -> Option<OutputLease> {
        let mut busy = self.busy.try_lock().ok()?;
        if *busy {
            return None;
        }
        *busy = true;
        Some(OutputLease(self))
    }
}

static STDERR: OutputGate = OutputGate::new();

struct OutputLease(&'static OutputGate);

impl OutputLease {
    fn acquire(check: &mut impl FnMut() -> Result<()>) -> Result<Self> {
        let gate = &STDERR;
        let mut busy = gate.busy.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            check()?;
            if !*busy {
                *busy = true;
                return Ok(Self(gate));
            }
            (busy, _) = gate
                .available
                .wait_timeout(busy, CHECK_INTERVAL)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for OutputLease {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.0.available.notify_one();
    }
}

pub(super) fn output_kind(fd: RawFd) -> Option<libc::mode_t> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    (unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0)
        .then(|| unsafe { stat.assume_init() }.st_mode & libc::S_IFMT)
}

pub(super) fn write_available_stderr(mut bytes: &[u8]) {
    let Some(_lease) = STDERR.try_acquire() else {
        return;
    };

    let fd = libc::STDERR_FILENO;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE == libc::O_RDONLY {
        return;
    }
    let nonblocking = flags & libc::O_NONBLOCK != 0;
    let Some(kind) = output_kind(fd) else {
        return;
    };
    if nonblocking
        && (kind == libc::S_IFIFO || kind == libc::S_IFCHR && unsafe { libc::isatty(fd) } == 1)
    {
        unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        return;
    }
    match kind {
        libc::S_IFSOCK => {
            // Darwin can ignore MSG_DONTWAIT on a blocking Unix socket.
            #[cfg(target_os = "macos")]
            if !nonblocking {
                return;
            }
            while !bytes.is_empty() {
                let count = unsafe {
                    libc::send(
                        fd,
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                };
                if count <= 0 {
                    break;
                }
                bytes = &bytes[count as usize..];
            }
        }
        libc::S_IFCHR => {
            if let Ok(mut terminal) = crate::terminal_ui::nonblocking_stderr_terminal() {
                let _ = terminal.write_all(bytes);
            }
        }
        #[cfg(target_os = "linux")]
        libc::S_IFIFO => write_available_pipe(bytes),
        _ => {}
    }
}

#[cfg(target_os = "linux")]
fn write_available_pipe(bytes: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    // Opening the pipe through procfs creates independent status flags.
    if let Ok(mut pipe) = File::options()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/proc/self/fd/2")
    {
        let _ = pipe.write_all(bytes);
    }
}

#[derive(Debug)]
pub(super) struct StartFailed;

impl std::fmt::Display for StartFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("start diagnostic writer")
    }
}

#[repr(C)]
struct Writer {
    output: RawFd,
    result: libc::c_int,
    setup_failed: libc::c_int,
    bytes: *const u8,
    length: u64,
    context: *mut libc::c_void,
    begin: unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int,
    complete: unsafe extern "C" fn(*mut libc::c_void, libc::c_int, libc::c_int),
}

extern "C" {
    fn git_on_diagnostic_start(thread: *mut libc::pthread_t, writer: *mut Writer) -> libc::c_int;
}

struct PendingWrite {
    writer: Writer,
    bytes: Arc<Vec<u8>>,
    output_lease: OutputLease,
    state: Arc<AtomicU8>,
    result: mpsc::SyncSender<(libc::c_int, bool)>,
}

impl PendingWrite {
    fn new(
        bytes: Arc<Vec<u8>>,
        output_lease: OutputLease,
    ) -> (Box<Self>, mpsc::Receiver<(libc::c_int, bool)>) {
        let (result, receiver) = mpsc::sync_channel(1);
        let writer = Box::new(Self {
            writer: Writer {
                output: libc::STDERR_FILENO,
                result: libc::ECANCELED,
                setup_failed: 0,
                bytes: bytes.as_ptr(),
                length: bytes.len() as u64,
                context: std::ptr::null_mut(),
                begin: start_writing,
                complete: release_writer,
            },
            bytes,
            output_lease,
            state: Arc::new(AtomicU8::new(WAITING)),
            result,
        });
        (writer, receiver)
    }

    fn start(self: Box<Self>) -> Result<DiagnosticThread> {
        let state = Arc::clone(&self.state);
        let owner = Box::into_raw(self);
        unsafe {
            (*owner).writer.context = owner.cast();
        }
        let mut handle = std::mem::MaybeUninit::uninit();
        let mut spawn =
            || unsafe { git_on_diagnostic_start(handle.as_mut_ptr(), &raw mut (*owner).writer) };
        let error = spawn();
        // Only a failed pthread_create leaves ownership in the caller.
        let error = if error != 0 { spawn() } else { error };
        if error != 0 {
            // A successfully started worker releases this allocation itself.
            drop(unsafe { Box::from_raw(owner) });
            return Err(io::Error::from_raw_os_error(error)).context(StartFailed);
        }
        Ok(DiagnosticThread {
            handle: unsafe { handle.assume_init() },
            state,
        })
    }
}

unsafe extern "C" fn start_writing(context: *mut libc::c_void) -> libc::c_int {
    let owner = context.cast::<PendingWrite>();
    let state = unsafe { &(*owner).state };
    if state
        .compare_exchange(WAITING, WRITING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 0;
    }
    1
}

unsafe extern "C" fn release_writer(
    context: *mut libc::c_void,
    errno: libc::c_int,
    setup_failed: libc::c_int,
) {
    let PendingWrite {
        bytes,
        output_lease,
        state,
        result,
        ..
    } = *unsafe { Box::from_raw(context.cast::<PendingWrite>()) };
    // Release stderr only after the worker has stopped using it.
    drop(output_lease);
    // Completion means the worker no longer holds the bytes.
    drop(bytes);
    state.store(FINISHED, Ordering::Release);
    let _ = result.try_send((errno, setup_failed != 0));
}

struct DiagnosticThread {
    handle: libc::pthread_t,
    state: Arc<AtomicU8>,
}

// POSIX permits cancelling and detaching a joinable thread from any thread.
// This value owns the handle until it detaches it.
unsafe impl Send for DiagnosticThread {}

impl DiagnosticThread {
    fn cancel(&self) {
        let state = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                matches!(state, WAITING | WRITING).then_some(CANCELLED)
            });
        if state == Ok(WRITING) {
            unsafe { libc::pthread_cancel(self.handle) };
        }
    }

    fn wait(
        &self,
        receiver: &mpsc::Receiver<(libc::c_int, bool)>,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<(libc::c_int, bool)> {
        loop {
            match receiver.recv_timeout(CHECK_INTERVAL) {
                Ok(status) => return Ok(status),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = check() {
                        self.cancel();
                        // Let the worker release the output lease.
                        let _ = receiver.recv_timeout(CLOSE_GRACE);
                        return Err(error);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for DiagnosticThread {
    fn drop(&mut self) {
        // The kernel can hold a syscall even after pthread_cancel.
        // The worker owns its buffer and can release it independently.
        self.cancel();
        unsafe { libc::pthread_detach(self.handle) };
    }
}

pub(super) fn write(message: std::fmt::Arguments<'_>, live: bool) {
    let bytes = format_diagnostic(message);
    crate::terminal_ui::stop_drawing();
    if crate::terminal_ui::clearing_failed()
        || write_stderr(Arc::clone(&bytes), live).is_err_and(|error| error.is::<StartFailed>())
    {
        // Preserve ordinary terminal job control if drawing or writer setup fails.
        write_available_stderr(&bytes);
    }
}

fn write_stderr(bytes: Arc<Vec<u8>>, live: bool) -> Result<()> {
    let signals = super::initialize_cancellation();
    if live {
        signals?;
    }
    let live_deadline = Instant::now() + DIAGNOSTIC_GRACE;
    let mut cancelled_deadline = None;
    let mut check = || {
        if live {
            crate::object::check_transfer()?;
            anyhow::ensure!(
                Instant::now() < live_deadline,
                "Git diagnostic output is blocked"
            );
            return Ok(());
        }
        check_recovery(
            &mut cancelled_deadline,
            super::cancelled_signal().is_some(),
            Instant::now(),
        )
    };
    check()?;
    // Standard descriptors stay open to preserve any inherited record locks.
    let lease = OutputLease::acquire(&mut check)?;
    let (writer, receiver) = PendingWrite::new(bytes, lease);
    let thread = writer.start()?;
    let result = thread.wait(&receiver, &mut check)?;
    check()?;
    writer_outcome(result)
}

fn check_recovery(deadline: &mut Option<Instant>, cancelled: bool, now: Instant) -> Result<()> {
    if cancelled {
        // Start the window when a cancellation is first observed, including
        // a signal delivered just before the writer starts.
        deadline.get_or_insert(now + RECOVERY_GRACE);
    }
    if deadline.is_some_and(|deadline| now >= deadline) {
        return Err(crate::object::OperationCancelled.into());
    }
    Ok(())
}

fn writer_outcome((errno, setup_failed): (libc::c_int, bool)) -> Result<()> {
    if errno == 0 {
        return Ok(());
    }
    let error = io::Error::from_raw_os_error(errno);
    if setup_failed {
        Err(error).context(StartFailed)
    } else {
        Err(error.into())
    }
}

#[cfg(test)]
pub(crate) fn output_pair(terminal: bool) -> (File, File) {
    use std::os::{
        fd::{FromRawFd, OwnedFd},
        unix::net::UnixStream,
    };

    if terminal {
        // Set close-on-exec at open time while other tests can spawn children.
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        let master = unsafe { libc::posix_openpt(flags) };
        assert!(master >= 0, "{}", io::Error::last_os_error());
        let reader = unsafe { File::from_raw_fd(master) };
        assert_eq!(unsafe { libc::grantpt(master) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master) }, 0);
        let mut name = [0 as libc::c_char; 128];
        #[cfg(target_os = "macos")]
        let status = unsafe { libc::ioctl(master, libc::TIOCPTYGNAME as _, name.as_mut_ptr()) };
        #[cfg(not(target_os = "macos"))]
        let status = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
        assert_eq!(status, 0);
        let slave = unsafe { libc::open(name.as_ptr(), flags) };
        assert!(slave >= 0, "{}", io::Error::last_os_error());
        (reader, unsafe { File::from_raw_fd(slave) })
    } else {
        let (reader, output) = UnixStream::pair().unwrap();
        (
            File::from(OwnedFd::from(reader)),
            File::from(OwnedFd::from(output)),
        )
    }
}

#[cfg(test)]
pub(crate) fn full_output(terminal: bool) -> (File, File, usize) {
    let (reader, mut output) = output_pair(terminal);
    let fd = output.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let filled = fill_test_output(&mut output);
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
    (reader, output, filled)
}

#[cfg(test)]
fn fill_test_output(mut output: impl Write) -> usize {
    let mut filled = 0;
    loop {
        match output.write(&[0]) {
            Ok(1) => filled += 1,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            result => panic!("cannot fill fallback output: {result:?}"),
        }
    }
    filled
}

#[cfg(test)]
pub(crate) fn run_isolated_test(test: &str) -> bool {
    use std::{
        env,
        process::{Command, Stdio},
        thread,
    };

    const CHILD: &str = "GIT_ON_TEST_ISOLATED_TERMINAL";
    if env::var_os(CHILD).is_some() {
        return false;
    }
    let mut child = Command::new(env::current_exe().unwrap())
        .arg("--exact")
        .arg(test)
        .env(CHILD, "1")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "terminal fixture {test} exited with {status}"
            );
            return true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("terminal fixture {test} timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}
