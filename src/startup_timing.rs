//! Private full-invocation phase instrumentation. Absent from normal builds.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{fs::File, io::Write, os::fd::FromRawFd};

#[derive(Serialize)]
struct Event {
    phase: &'static str,
    monotonic_ns: u64,
}
pub(crate) struct Timing {
    output: Option<File>,
    events: Vec<Event>,
}
impl Timing {
    pub(crate) fn new() -> Result<Self> {
        let output = std::env::var_os("GIT_ON_TEST_TIMING_FD")
            .map(|value| -> Result<File> {
                let fd: i32 = value.to_str().context("invalid timing fd")?.parse()?;
                if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    bail!("invalid test timing descriptor");
                }
                let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                if duplicate < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(unsafe { File::from_raw_fd(duplicate) })
            })
            .transpose()?;
        Ok(Self {
            output,
            events: Vec::new(),
        })
    }
    pub(crate) fn mark(&mut self, phase: &'static str) {
        if self.output.is_none() {
            return;
        }
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, value.as_mut_ptr()) } == 0 {
            let value = unsafe { value.assume_init() };
            self.events.push(Event {
                phase,
                monotonic_ns: value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64,
            });
        }
    }
}
impl Drop for Timing {
    fn drop(&mut self) {
        self.mark("execute_end");
        if let Some(output) = self.output.as_mut() {
            // The diagnostic harness requires a complete record. A profiling
            // channel failure does not change the command's real exit status.
            let _ = serde_json::to_writer(&mut *output, &self.events);
            let _ = output.write_all(b"\n");
        }
    }
}
