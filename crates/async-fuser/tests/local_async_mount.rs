#![cfg(all(feature = "async", target_os = "linux"))]
use fuser::{AsyncFilesystem, AsyncSessionBuilder, Config, KernelConfig, MountOption, Request};
use std::{io, path::Path, process::Command};

#[derive(Debug)]
struct RejectInit;
impl AsyncFilesystem for RejectInit {
    async fn init(&mut self, _: &Request, _: &mut KernelConfig) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }
}
fn mounted(path: &Path) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .any(|line| line.split_whitespace().nth(4) == path.to_str())
}
#[test]
fn failed_init_releases_the_mount_before_runtime_shutdown_returns() {
    assert!(
        Path::new("/dev/fuse").exists(),
        "this lifecycle test requires real FUSE"
    );
    let directory = tempfile::Builder::new()
        .prefix("git-on-async-mount-test-")
        .tempdir()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut options = Config::default();
    options.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("git-on-async-lifecycle-test".into()),
    ];
    let result = runtime.block_on(
        AsyncSessionBuilder::new()
            .filesystem(RejectInit)
            .mountpoint(directory.path())
            .options(options)
            .unwrap()
            .build(),
    );
    assert!(matches!(result, Err(ref e) if e.raw_os_error() == Some(libc::EPERM)));
    drop(runtime);
    let leaked = mounted(directory.path());
    if leaked {
        // Preserve the failed observation while cleaning only this test's mount.
        assert!(
            Command::new("fusermount3")
                .args(["-u"])
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        !leaked,
        "failed INIT left a mount after its runtime and FUSE descriptor closed"
    );
}
