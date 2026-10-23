//! Native FSKit operation service. This is an authenticated private per-run
//! bridge to the shared overlay, not a FUSE protocol implementation.
use super::{FileType, SetAttributes, TimeOrNow};
use crate::{
    filesystem::{FsResult, Overlay},
    policy::WritePolicy,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::PermissionsExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};
const MAX_FRAME: usize = 8 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Timestamp {
    secs: u64,
    nanos: u32,
}
impl Timestamp {
    fn time(self) -> FsResult<SystemTime> {
        if self.nanos >= 1_000_000_000 {
            return Err(libc::EINVAL);
        }
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(self.secs, self.nanos))
            .ok_or(libc::EOVERFLOW)
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attributes {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<Timestamp>,
    mtime: Option<Timestamp>,
    crtime: Option<Timestamp>,
    flags: Option<u32>,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Initialize,
    Lookup {
        parent: u64,
        name: Vec<u8>,
    },
    Getattr {
        ino: u64,
    },
    Setattr {
        ino: u64,
        attributes: Attributes,
    },
    Readlink {
        ino: u64,
    },
    Create {
        parent: u64,
        name: Vec<u8>,
        kind: Kind,
        mode: u32,
    },
    Symlink {
        parent: u64,
        name: Vec<u8>,
        target: Vec<u8>,
    },
    Link {
        ino: u64,
        parent: u64,
        name: Vec<u8>,
    },
    Remove {
        parent: u64,
        name: Vec<u8>,
        directory: bool,
    },
    Rename {
        parent: u64,
        name: Vec<u8>,
        newparent: u64,
        newname: Vec<u8>,
    },
    Forget {
        ino: u64,
        count: u64,
    },
    Open {
        ino: u64,
        flags: i32,
        directory: bool,
    },
    Close {
        ino: u64,
        handle: u64,
    },
    Read {
        ino: u64,
        handle: u64,
        offset: i64,
        size: u32,
    },
    Write {
        ino: u64,
        handle: u64,
        offset: i64,
        data: Vec<u8>,
    },
    ReadDir {
        ino: u64,
        handle: u64,
        offset: u64,
    },
    Sync {
        ino: u64,
        handle: u64,
        directory: bool,
    },
    Statfs,
    Access {
        ino: u64,
        mask: i32,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    File,
    Directory,
}
fn value<T: serde::Serialize>(v: T) -> FsResult<Value> {
    serde_json::to_value(v).map_err(|_| libc::EIO)
}
fn dispatch(core: &mut Overlay, operation: Operation, read_only: bool) -> FsResult<Value> {
    let _request = crate::git::RequestGuard::enter(0);
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    match operation {
        Operation::Initialize=>Ok(json!({"read_only":read_only,"protocol":1})),
        Operation::Lookup{parent,name}=>value(core.lookup_entry(parent,OsStr::from_bytes(&name))?),
        Operation::Getattr{ino}=>value(core.attributes(ino,None)?),
        Operation::Setattr{ino,attributes:a}=>value(core.set_attributes(ino,uid,SetAttributes{mode:a.mode,owner:a.uid,group:a.gid,size:a.size,atime:a.atime.map(Timestamp::time).transpose()?.map(TimeOrNow::SpecificTime),mtime:a.mtime.map(Timestamp::time).transpose()?.map(TimeOrNow::SpecificTime),crtime:a.crtime.map(Timestamp::time).transpose()?,flags:a.flags,..Default::default()})?),
        Operation::Readlink{ino}=>value(core.read_link(ino)?),
        Operation::Create{parent,name,kind,mode}=>value(core.create_node(parent,OsStr::from_bytes(&name),match kind{Kind::File=>FileType::RegularFile,Kind::Directory=>FileType::Directory},mode,uid,gid,Vec::new())?),
        Operation::Symlink{parent,name,target}=>value(core.create_node(parent,OsStr::from_bytes(&name),FileType::Symlink,0o777,uid,gid,target)?),
        Operation::Link{ino,parent,name}=>value(core.link_node(ino,parent,OsStr::from_bytes(&name))?),
        Operation::Remove{parent,name,directory}=>{core.remove(parent,OsStr::from_bytes(&name),directory)?;Ok(Value::Null)},
        Operation::Rename{parent,name,newparent,newname}=>{core.rename_path(parent,OsStr::from_bytes(&name),newparent,OsStr::from_bytes(&newname),0)?;Ok(Value::Null)},
        Operation::Forget{ino,count}=>{core.forget_inode(ino,count);Ok(Value::Null)},
        Operation::Open{ino,flags,directory}=>value(if directory{core.open_directory(ino,flags)?}else{core.open_file(ino,flags)?}),
        Operation::Close{ino,handle}=>{core.release_handle(ino,handle)?;Ok(Value::Null)},
        Operation::Read{ino,handle,offset,size}=>{if size>512*1024{return Err(libc::E2BIG);}value(core.read_data(ino,handle,offset,size)?)},
        Operation::Write{ino,handle,offset,data}=>{if data.len()>512*1024{return Err(libc::E2BIG);}value(core.write_data(ino,handle,offset,&data,0,libc::O_RDWR)?)},
        Operation::ReadDir{ino,handle,offset}=>Ok(Value::Array(core.entries(ino,handle,offset)?.iter().take(128).enumerate().map(|(i,(ino,kind,name))|json!({"ino":ino,"kind":kind,"name":name.as_bytes(),"next":offset+i as u64+1})).collect())),
        Operation::Sync{ino,handle,directory}=>{if directory{core.sync_directory(ino,handle)?;}else{core.sync_file(ino,handle,false)?;}Ok(Value::Null)},
        Operation::Statfs=>value(core.statistics()?),
        Operation::Access{ino,mask}=>{core.check_access(ino,uid,gid,mask)?;Ok(Value::Null)},
    }
}
fn serve(mut stream: UnixStream, core: &mut Overlay, read_only: bool) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    loop {
        let mut header = [0; 4];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid native operation frame",
            ));
        }
        let mut input = vec![0; length];
        stream.read_exact(&mut input)?;
        let result = serde_json::from_slice::<Operation>(&input)
            .map_err(|_| libc::EINVAL)
            .and_then(|operation| dispatch(core, operation, read_only));
        let response = match result {
            Ok(result) => json!({"ok":result}),
            Err(error) => json!({"error":error}),
        };
        let output = serde_json::to_vec(&response)?;
        if output.len() > MAX_FRAME {
            return Err(io::Error::other("native operation response too large"));
        }
        stream.write_all(&(output.len() as u32).to_le_bytes())?;
        stream.write_all(&output)?;
    }
}
pub struct Session {
    task: Option<JoinHandle<io::Result<()>>>,
    stop: Arc<AtomicBool>,
    connection: Arc<Mutex<Option<UnixStream>>>,
    mountpoint: PathBuf,
}
impl Session {
    pub fn mount(mut core: Overlay, mountpoint: &Path, policy: WritePolicy) -> io::Result<Self> {
        let resource = mountpoint
            .parent()
            .ok_or_else(|| io::Error::other("missing private directory"))?
            .join("native");
        fs::create_dir(&resource)?;
        fs::set_permissions(&resource, fs::Permissions::from_mode(0o700))?;
        let endpoint = resource.join("bridge.sock");
        let listener = UnixListener::bind(&endpoint)?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let connection = Arc::new(Mutex::new(None));
        let worker_stop = stop.clone();
        let worker_connection = connection.clone();
        let task = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (mut uid, mut gid) = (0, 0);
                        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
                            || uid != unsafe { libc::geteuid() }
                        {
                            continue;
                        }
                        *worker_connection
                            .lock()
                            .map_err(|_| io::Error::other("native connection poisoned"))? =
                            Some(stream.try_clone()?);
                        return serve(stream, &mut core, policy == WritePolicy::None);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        });
        let mut session = Self {
            task: Some(task),
            stop,
            connection,
            mountpoint: mountpoint.into(),
        };
        let mut child = Command::new("/sbin/mount")
            .args([
                "-F",
                "-t",
                "giton",
                "-o",
                if policy == WritePolicy::None {
                    "nosuid,nodev,ro"
                } else {
                    "nosuid,nodev"
                },
            ])
            .arg(&resource)
            .arg(mountpoint)
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                if status.success() {
                    return Ok(session);
                }
                session.close();
                return Err(io::Error::other(format!("native Apple FSKit mount failed: {status}; install and activate the GitOn extension")));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                session.close();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native Apple FSKit mount timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(stream) = self.connection.lock() {
            if let Some(stream) = stream.as_ref() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    pub fn join(mut self) -> io::Result<()> {
        self.close();
        self.task
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("native filesystem thread panicked"))?
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
        let _ = &self.mountpoint;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    fn fixture(policy: WritePolicy) -> (tempfile::TempDir, Overlay, Repository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let root = repo.workdir().unwrap();
        fs::write(root.join(".gitignore"), b"out/\n").unwrap();
        fs::write(root.join("tracked"), b"snapshot\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree = index.write_tree().unwrap();
        let signature = Signature::now("fixture", "fixture@example.invalid").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "snapshot",
            &repo.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
        fs::write(root.join("tracked"), b"caller edits\n").unwrap();
        let snapshot =
            crate::snapshot::Snapshot::load(root, OsStr::new("HEAD"), "native-operation-test")
                .unwrap();
        let core =
            Overlay::with_write_policy(snapshot, temp.path().join("backing"), policy).unwrap();
        (temp, core, repo)
    }
    fn operation(core: &mut Overlay, request: Value, read_only: bool) -> FsResult<Value> {
        dispatch(
            core,
            serde_json::from_value(request).map_err(|_| libc::EINVAL)?,
            read_only,
        )
    }
    #[test]
    fn native_operations_preserve_all_policies_snapshot_bytes_and_backing_handles() {
        for policy in [
            WritePolicy::None,
            WritePolicy::Ignored,
            WritePolicy::Untracked,
            WritePolicy::All,
        ] {
            let (_temp, mut core, repo) = fixture(policy);
            let tracked = operation(
                &mut core,
                json!({"op":"lookup","parent":1,"name":b"tracked"}),
                false,
            )
            .unwrap()["ino"]
                .as_u64()
                .unwrap();
            let handle = operation(
                &mut core,
                json!({"op":"open","ino":tracked,"flags":libc::O_RDONLY,"directory":false}),
                false,
            )
            .unwrap();
            assert_eq!(
                operation(
                    &mut core,
                    json!({"op":"read","ino":tracked,"handle":handle,"offset":0,"size":100}),
                    false
                )
                .unwrap(),
                json!(b"snapshot\n")
            );
            assert_eq!(
                operation(
                    &mut core,
                    json!({"op":"open","ino":tracked,"flags":libc::O_RDWR,"directory":false}),
                    false
                )
                .is_ok(),
                policy == WritePolicy::All
            );
            let generated = operation(
                &mut core,
                json!({"op":"create","parent":1,"name":b"fresh","kind":"file","mode":384}),
                false,
            );
            assert_eq!(
                generated.is_ok(),
                matches!(policy, WritePolicy::All | WritePolicy::Untracked)
            );
            let ignored = operation(
                &mut core,
                json!({"op":"create","parent":1,"name":b"out","kind":"directory","mode":448}),
                false,
            );
            assert_eq!(ignored.is_ok(), policy != WritePolicy::None);
            if let Ok(directory) = ignored {
                let ino = directory["ino"].as_u64().unwrap();
                let file = operation(
                    &mut core,
                    json!({"op":"create","parent":ino,"name":b"data","kind":"file","mode":384}),
                    false,
                )
                .unwrap()["ino"]
                    .as_u64()
                    .unwrap();
                let handle = operation(
                    &mut core,
                    json!({"op":"open","ino":file,"flags":libc::O_RDWR,"directory":false}),
                    false,
                )
                .unwrap();
                assert_eq!(
                    operation(
                        &mut core,
                        json!({"op":"write","ino":file,"handle":handle,"offset":4,"data":b"data"}),
                        false
                    )
                    .unwrap(),
                    json!(4)
                );
                assert_eq!(
                    operation(&mut core, json!({"op":"getattr","ino":file}), false).unwrap()
                        ["size"],
                    8
                );
                assert_eq!(
                    operation(
                        &mut core,
                        json!({"op":"read","ino":file,"handle":handle,"offset":0,"size":100}),
                        false
                    )
                    .unwrap(),
                    json!([0, 0, 0, 0, 100, 97, 116, 97])
                );
                operation(
                    &mut core,
                    json!({"op":"remove","parent":ino,"name":b"data","directory":false}),
                    false,
                )
                .unwrap();
                assert_eq!(
                    operation(
                        &mut core,
                        json!({"op":"read","ino":file,"handle":handle,"offset":4,"size":4}),
                        false
                    )
                    .unwrap(),
                    json!(b"data")
                );
                operation(
                    &mut core,
                    json!({"op":"close","ino":file,"handle":handle}),
                    false,
                )
                .unwrap();
            }
            assert_eq!(
                fs::read(repo.workdir().unwrap().join("tracked")).unwrap(),
                b"caller edits\n"
            );
            drop(core);
            assert_eq!(repo.references_glob("refs/git-on/*").unwrap().count(), 0);
        }
    }
    #[test]
    fn native_requests_reject_impersonation_fields_and_unknown_operations() {
        assert!(
            serde_json::from_value::<Operation>(json!({"op":"getattr","ino":1,"uid":0})).is_err()
        );
        assert!(
            serde_json::from_value::<Operation>(json!({"op":"execute","command":"sh"})).is_err()
        );
        assert!(Timestamp {
            secs: 0,
            nanos: 1_000_000_000
        }
        .time()
        .is_err());
    }
    #[test]
    fn framed_native_service_handles_fragmentation_and_rejects_oversized_frames() {
        let (_temp, mut core, _repo) = fixture(WritePolicy::Ignored);
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = thread::spawn(move || serve(server, &mut core, false));
        let input = serde_json::to_vec(&json!({"op":"initialize"})).unwrap();
        client
            .write_all(&(input.len() as u32).to_le_bytes())
            .unwrap();
        for part in input.chunks(3) {
            client.write_all(part).unwrap();
        }
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        let mut output = vec![0; u32::from_le_bytes(header) as usize];
        client.read_exact(&mut output).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            json!({"ok":{"protocol":1,"read_only":false}})
        );
        client.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(
            task.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
