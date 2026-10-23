//! Direct Apple FSKit adapter transport. Our extension translates native FSKit
//! operations into framed requests for the existing Rust filesystem dispatcher.
//! No macFUSE library, helper, driver or private Apple API is used.
use super::{MountOption, mount_options::option_to_string};
use std::{
    ffi::CString,
    fs,
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::PermissionsExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct NativeChannel {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
}
impl NativeChannel {
    fn new(stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            writer: Mutex::new(stream.try_clone()?),
            reader: Mutex::new(stream),
        })
    }
    pub(crate) fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut stream = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("FSKit channel poisoned"))?;
        let mut frame = [0; 4];
        match stream.read_exact(&mut frame) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(e),
        }
        let length = u32::from_le_bytes(frame) as usize;
        if length < 40 || length > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FSKit operation frame",
            ));
        }
        stream.read_exact(&mut buffer[..length])?;
        Ok(length)
    }
    pub(crate) fn send(&self, buffers: &[io::IoSlice<'_>]) -> io::Result<()> {
        let length = buffers
            .iter()
            .try_fold(0usize, |sum, b| sum.checked_add(b.len()))
            .filter(|n| *n <= 16 * 1024 * 1024)
            .ok_or_else(|| io::Error::other("FSKit response too large"))?;
        let mut stream = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("FSKit channel poisoned"))?;
        stream.write_all(&(length as u32).to_le_bytes())?;
        for buffer in buffers {
            stream.write_all(buffer)?;
        }
        Ok(())
    }
    fn close(&self) {
        if let Ok(stream) = self.writer.lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}
#[derive(Debug)]
pub struct Mount {
    channel: Arc<NativeChannel>,
    mountpoint: CString,
    mounting: Mutex<Option<Child>>,
}
impl Mount {
    pub(crate) fn new(
        path: &Path,
        options: &[MountOption],
    ) -> io::Result<(Arc<NativeChannel>, Self)> {
        let resource = path
            .parent()
            .ok_or_else(|| io::Error::other("missing private run directory"))?
            .join("native");
        fs::create_dir(&resource)?;
        fs::set_permissions(&resource, fs::Permissions::from_mode(0o700))?;
        if options.contains(&MountOption::RO) {
            fs::write(resource.join("readonly"), b"git-on policy none\n")?;
        }
        let endpoint = resource.join("bridge.sock");
        let listener = UnixListener::bind(&endpoint)?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let arguments = options
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    MountOption::NoDev | MountOption::NoSuid | MountOption::RO
                )
            })
            .map(option_to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut mounting = Command::new("/sbin/mount")
            .args(["-F", "-t", "giton", "-o"])
            .arg(arguments)
            .arg(&resource)
            .arg(path)
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let mut uid = 0;
                    let mut gid = 0;
                    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
                        || uid != unsafe { libc::geteuid() }
                    {
                        continue;
                    }
                    break stream;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    let _ = mounting.kill();
                    let _ = mounting.wait();
                    return Err(e);
                }
            }
            if let Some(status) = mounting.try_wait()? {
                return Err(io::Error::other(format!(
                    "native FSKit mount failed: {status}; install and activate the GitOn file system extension"
                )));
            }
            if Instant::now() >= deadline {
                let _ = mounting.kill();
                let _ = mounting.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native FSKit connection timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let channel = Arc::new(NativeChannel::new(stream)?);
        Ok((
            channel.clone(),
            Self {
                channel,
                mountpoint: CString::new(path.as_os_str().as_bytes())?,
                mounting: Mutex::new(Some(mounting)),
            },
        ))
    }
    pub(crate) fn wait_mounted(&self) -> io::Result<()> {
        let child = self
            .mounting
            .lock()
            .map_err(|_| io::Error::other("FSKit mount lock poisoned"))?
            .take();
        if let Some(mut child) = child {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait()? {
                    return if status.success() {
                        Ok(())
                    } else {
                        self.channel.close();
                        Err(io::Error::other(format!(
                            "native FSKit mount failed: {status}"
                        )))
                    };
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.channel.close();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native FSKit mount timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
}
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = super::libc_umount(&self.mountpoint);
        self.channel.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framed_stream_preserves_fragmented_requests_and_complete_replies() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let channel = NativeChannel::new(server).unwrap();
        client.write_all(&40u32.to_le_bytes()).unwrap();
        client.write_all(&[7; 13]).unwrap();
        client.write_all(&[7; 27]).unwrap();
        let mut buffer = [0; 64];
        assert_eq!(channel.receive(&mut buffer).unwrap(), 40);
        assert_eq!(&buffer[..40], &[7; 40]);
        channel
            .send(&[io::IoSlice::new(b"hello"), io::IoSlice::new(b" world")])
            .unwrap();
        let mut reply = [0; 15];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply[..4], &11u32.to_le_bytes());
        assert_eq!(&reply[4..], b"hello world");
        client.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(
            channel.receive(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
