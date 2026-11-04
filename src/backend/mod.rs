//! Shared filesystem values. Policy and namespace logic live in Overlay;
//! platform adapters translate their own API without implementing those rules.
use std::time::SystemTime;
#[cfg(target_os = "linux")]
pub mod linux;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum FileType {
    Directory,
    RegularFile,
    Symlink,
    NamedPipe,
    CharDevice,
    BlockDevice,
    Socket,
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct FileAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub crtime: SystemTime,
    pub kind: FileType,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}
#[derive(Clone, Copy)]
pub enum TimeOrNow {
    SpecificTime(SystemTime),
    Now,
}
#[derive(Default)]
pub struct SetAttributes {
    pub mode: Option<u32>,
    pub owner: Option<u32>,
    pub group: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeOrNow>,
    pub mtime: Option<TimeOrNow>,
    pub ctime: Option<SystemTime>,
    pub fh: Option<u64>,
    pub crtime: Option<SystemTime>,
    pub flags: Option<u32>,
}
#[derive(serde::Serialize)]
pub struct Statistics {
    pub blocks: u64,
    pub free: u64,
    pub available: u64,
    pub files: u64,
    pub free_files: u64,
    pub block_size: u32,
    pub fragment_size: u32,
}

pub enum Session {
    #[cfg(target_os = "linux")]
    Linux(fuser::BackgroundSession),
}
impl Session {
    pub fn join(self) -> std::io::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(session) => session.join(),
        }
    }
}
pub fn mount(
    overlay: crate::filesystem::Overlay,
    path: &std::path::Path,
    id: &str,
    policy: crate::policy::WritePolicy,
) -> std::io::Result<Session> {
    #[cfg(target_os = "linux")]
    {
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::FSName(format!("git-on:{id}")),
            fuser::MountOption::Subtype("git-on".into()),
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoDev,
            fuser::MountOption::DefaultPermissions,
        ];
        if policy == crate::policy::WritePolicy::None {
            config.mount_options.push(fuser::MountOption::RO);
        }
        fuser::spawn_mount(linux::Fuse::new(overlay), path, &config).map(Session::Linux)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (overlay, path, id, policy);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "macOS mounts are not supported in git-on 1.3.0",
        ))
    }
}
