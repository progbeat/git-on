//! Official MFMount 5.3.3 message transport for macFUSE's FSKit backend.
//! Uses only the vendor's public API. The package and helpers must already be
//! owner-provisioned: MFMount may attempt to refresh its own helper services.
use super::{MountOption, mount_options::option_to_string};
use std::{
    ffi::CString,
    io::{self, IoSlice},
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Ref = *mut libc::c_void;
type Create = unsafe extern "C" fn() -> Ref;
type Release = unsafe extern "C" fn(Ref);
type Close = unsafe extern "C" fn(Ref) -> bool;
type CopyMessage = unsafe extern "C" fn(Ref) -> Ref;
type BodySize = unsafe extern "C" fn(Ref) -> libc::ssize_t;
type BodyBuffers = unsafe extern "C" fn(Ref, *mut *const libc::iovec) -> libc::ssize_t;
type SendMessage = unsafe extern "C" fn(Ref, *const libc::iovec, libc::size_t) -> libc::ssize_t;
type MountVolume = unsafe extern "C" fn(Ref, *const libc::c_char, *const libc::c_char, bool) -> i32;

#[derive(Debug)]
struct Api {
    create: Create,
    release: Release,
    close: Close,
    copy_message: CopyMessage,
    body_size: BodySize,
    body_buffers: BodyBuffers,
    send: SendMessage,
    mount: MountVolume,
}

fn api() -> io::Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let library = libc::dlopen(
            c"/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount"
                .as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return None;
        }
        macro_rules! symbol {
            ($name:literal, $type:ty) => {{
                let address = libc::dlsym(library, concat!($name, "\0").as_ptr().cast());
                if address.is_null() {
                    libc::dlclose(library);
                    return None;
                }
                std::mem::transmute::<*mut libc::c_void, $type>(address)
            }};
        }
        // Keep the official library loaded for the entire process. Channels can
        // outlive the mount call and are used by background receive/reply threads.
        Some(Api {
            create: symbol!("MFChannelCreate", Create),
            release: symbol!("MFRelease", Release),
            close: symbol!("MFChannelClose", Close),
            copy_message: symbol!("MFChannelCopyNextMessage", CopyMessage),
            body_size: symbol!("MFMessageGetBodySize", BodySize),
            body_buffers: symbol!("MFMessageGetBodyBuffers", BodyBuffers),
            send: symbol!("MFChannelSendMessage", SendMessage),
            mount: symbol!("MFMount", MountVolume),
        })
    })
    .as_ref()
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "official macFUSE 5.3.3+ MFMount transport is unavailable",
        )
    })
}

/// MFMount documents concurrent message receive/send/close operations. The
/// channel object is pinned by Arc and released only after its final user.
#[derive(Debug)]
pub(crate) struct MacChannel {
    raw: Ref,
    api: &'static Api,
}
unsafe impl Send for MacChannel {}
unsafe impl Sync for MacChannel {}

impl MacChannel {
    fn close(&self) {
        unsafe {
            (self.api.close)(self.raw);
        }
    }

    pub(crate) fn receive(&self, output: &mut [u8]) -> io::Result<usize> {
        let raw = unsafe { (self.api.copy_message)(self.raw) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        struct Message(Ref, Release);
        impl Drop for Message {
            fn drop(&mut self) {
                unsafe {
                    (self.1)(self.0);
                }
            }
        }
        let message = Message(raw, self.api.release);
        let size = unsafe { (self.api.body_size)(message.0) };
        let mut buffers = std::ptr::null();
        let count = unsafe { (self.api.body_buffers)(message.0, &mut buffers) };
        if size < 0 || count < 0 {
            return Err(io::Error::last_os_error());
        }
        if size as usize > output.len() || count == 0 || buffers.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid MFMount request size/buffers",
            ));
        }
        let buffers = unsafe { std::slice::from_raw_parts(buffers, count as usize) };
        let copied = flatten(buffers, output)?;
        if copied != size as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MFMount request size differs from buffer lengths",
            ));
        }
        // The parser owns output, so no borrowed MFMessage storage survives.
        Ok(copied)
    }

    pub(crate) fn send(&self, buffers: &[IoSlice<'_>]) -> io::Result<()> {
        let sent = unsafe { (self.api.send)(self.raw, buffers.as_ptr().cast(), buffers.len()) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != buffers.iter().map(|buffer| buffer.len()).sum::<usize>() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short MFMount reply",
            ));
        }
        Ok(())
    }
}

impl Drop for MacChannel {
    fn drop(&mut self) {
        self.close();
        unsafe {
            (self.api.release)(self.raw);
        }
    }
}

fn flatten(buffers: &[libc::iovec], output: &mut [u8]) -> io::Result<usize> {
    let mut offset = 0usize;
    for buffer in buffers {
        let end = offset
            .checked_add(buffer.iov_len)
            .filter(|end| *end <= output.len())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MFMount request exceeds receive buffer",
                )
            })?;
        if buffer.iov_len != 0 {
            if buffer.iov_base.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "null MFMount body buffer",
                ));
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.iov_base.cast::<u8>(), buffer.iov_len) };
            output[offset..end].copy_from_slice(bytes);
        }
        offset = end;
    }
    Ok(offset)
}

#[derive(Debug)]
pub struct Mount {
    channel: Arc<MacChannel>,
    mountpoint: CString,
    mounting: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl Mount {
    pub(crate) fn new(path: &Path, options: &[MountOption]) -> io::Result<(Arc<MacChannel>, Self)> {
        let api = api()?;
        let raw = unsafe { (api.create)() };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let channel = Arc::new(MacChannel { raw, api });
        let mountpoint = CString::new(path.as_os_str().as_bytes())?;
        let arguments = options
            .iter()
            .map(option_to_string)
            .chain(std::iter::once("backend=fskit".into()))
            .collect::<Vec<_>>()
            .join(",");
        let arguments = CString::new(arguments)?;
        let worker_channel = channel.clone();
        let worker_mountpoint = mountpoint.clone();
        // Mount waits for FUSE INIT. Start it concurrently with Session's
        // receive/dispatch loop, then wait for its result before returning the
        // BackgroundSession to the caller and before any command is launched.
        let mounting = thread::Builder::new().name("macfuse-mount".into()).spawn(move || {
            let status = unsafe { (api.mount)(worker_channel.raw, worker_mountpoint.as_ptr(), arguments.as_ptr(), true) };
            if status != 0 {
                worker_channel.close();
                return Err(io::Error::other(format!("macFUSE FSKit mount failed (MFMount result {status}); owner-provided package, helpers and enabled FSKit extension are required")));
            }
            Ok(())
        })?;
        Ok((
            channel.clone(),
            Self {
                channel,
                mountpoint,
                mounting: Mutex::new(Some(mounting)),
            },
        ))
    }

    pub(crate) fn wait_mounted(&self) -> io::Result<()> {
        let worker = self
            .mounting
            .lock()
            .map_err(|_| io::Error::other("macFUSE mount lock poisoned"))?
            .take();
        if let Some(worker) = worker {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !worker.is_finished() {
                if Instant::now() >= deadline {
                    self.channel.close();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "macFUSE FSKit mount/INIT timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            worker
                .join()
                .map_err(|_| io::Error::other("macFUSE mount worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // Normal unmount only. Git-on separately checks success and retains
        // recovery state when busy; dropping a channel never proves unmount.
        let _ = super::libc_umount(&self.mountpoint);
        self.channel.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn message_fragments_are_copied_without_borrowing_transport_storage() {
        let mut first = [1u8, 2];
        let mut second = [3u8, 4, 5];
        let input = [
            libc::iovec {
                iov_base: first.as_mut_ptr().cast(),
                iov_len: 2,
            },
            libc::iovec {
                iov_base: second.as_mut_ptr().cast(),
                iov_len: 3,
            },
        ];
        let mut output = [0u8; 5];
        assert_eq!(flatten(&input, &mut output).unwrap(), 5);
        first.fill(9);
        second.fill(9);
        assert_eq!(output, [1, 2, 3, 4, 5]);
        assert!(flatten(&input, &mut [0u8; 4]).is_err());
        assert!(
            flatten(
                &[libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 1
                }],
                &mut output
            )
            .is_err()
        );
    }
}
