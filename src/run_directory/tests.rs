use super::legacy::{
    cleanup_legacy_marker_records, park_run_marker_with_reservation, prepare_retry_record,
    retiring_lock_name, retry_marker_name,
};
use super::removal::{
    pending_nfs_entry, BeforeChildOpen, AFTER_NFS_REMOVAL_FAILURE, BEFORE_CHILD_OPEN,
    DESCENDANT_BUSY, DESCENDANT_UNLINK_LOST_REPLY, EXTRA_ROOT_ENTRIES, EXTRA_ROOT_ENTRY_NAME,
    FORCE_NFS_REMOVAL, MOVE_RUN_BEFORE_REMOVAL, REMOVE_OPEN_CHILD,
    RUN_RMDIR_ATTRIBUTES_UNAVAILABLE, RUN_RMDIR_LOST_REPLY, RUN_RMDIR_WAS_BUSY,
};
use super::*;
use crate::tests::PROCESS_TEST_LOCK;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn recovery() -> Recovery {
    Recovery {
        format: "git-on-temp-v1".into(),
        id: "unit-test".into(),
    }
}
#[test]
fn flat_recovery_survives_an_active_lock_and_a_failed_retirement() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("temp");
    private_dir(&dir).unwrap();
    File::create(dir.join(".lock")).unwrap();
    let held = open_legacy_lock(&dir).unwrap();
    let state = dir.join(".state");
    let record = serde_json::to_vec(&recovery()).unwrap();
    fs::write(&state, &record).unwrap();
    assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(held);

    DESCENDANT_BUSY.set(Some(c".state"));
    let interrupted = cleanup(&dir).unwrap_err();
    DESCENDANT_BUSY.set(None);
    assert_eq!(io_errno(&interrupted), Some(libc::EBUSY));
    assert!(!dir.join(".lock").exists());
    assert_eq!(fs::read(&state).unwrap(), record);

    AFTER_MARKER_UNLOCK.set(Some(Box::new(|| {
        Err(std::io::Error::from_raw_os_error(libc::EPERM))
    })));
    assert_eq!(io_errno(&cleanup(&dir).unwrap_err()), Some(libc::EPERM));
    assert!(!dir.exists());
    cleanup(&dir).unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);

    private_dir(&dir).unwrap();
    fs::write(&state, &record).unwrap();
    let lock = dir.join(".lock");
    File::create(&lock).unwrap();
    let payload = dir.join("payload");
    fs::write(&payload, b"stale data").unwrap();
    RUN_NO_PERSISTENT_IDENTITY.set(true);
    DESCENDANT_BUSY.set(Some(c"payload"));
    let interrupted = cleanup(&dir).unwrap_err();
    DESCENDANT_BUSY.set(None);
    assert_eq!(io_errno(&interrupted), Some(libc::EBUSY));
    assert_eq!(fs::read(&state).unwrap(), record);
    assert!(lock.is_file());
    RUN_NO_PERSISTENT_IDENTITY.set(true);
    cleanup(&dir).unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}
fn reserve_test_run(parent: &Path, name: &str) -> (PathBuf, LockedRun) {
    try_reserve_test_run(parent, name).unwrap()
}

fn try_reserve_test_run(parent: &Path, name: &str) -> std::io::Result<(PathBuf, LockedRun)> {
    create_temporary_entry_with(parent, name, |parent, group, identity, basename| {
        reserve_run(parent, group, identity, basename, flock_exclusive)
    })
    .map(|(_, run)| run)
}

fn reserved_run() -> (tempfile::TempDir, PathBuf, LockedRun) {
    let root = tempfile::tempdir().unwrap();
    let (dir, run) = reserve_test_run(root.path(), "r123ab");
    (root, dir, run)
}

fn ownership_record(dir: &Path) -> PathBuf {
    record_path(dir, &run_record_name("r123ab"))
}

fn fixture_has_run_identity(dir: &Path) -> bool {
    run_identity(dir).unwrap().unwrap().1.is_some()
}

fn finish_test_run(dir: &Path, run: LockedRun) {
    if fixture_has_run_identity(dir) {
        drop(run);
        cleanup(dir).unwrap();
    } else {
        assert!(run.remove(dir).unwrap());
        remove_empty_temporary_group(dir.parent().unwrap()).unwrap();
    }
}

fn legacy_test_run(parent: &Path) -> (PathBuf, LockedRun) {
    let dir = git_on::platform::create_temporary_entry(parent, "g123ab").unwrap();
    let (directory, identity) = run_identity(&dir).unwrap().unwrap();
    let id = identity.unwrap().primary().to_string();
    let lock = create_run_marker(directory.as_raw_fd(), RUN_LOCK_NAME).unwrap();
    flock_exclusive(&lock).unwrap();
    (
        dir,
        LockedRun {
            directory,
            lock,
            id,
            location: RunLock::Grouped {
                marker: RUN_LOCK_NAME,
                parked: false,
            },
        },
    )
}

#[test]
fn legacy_recovery_refuses_a_parked_marker_while_the_directory_is_locked() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = git_on::platform::create_temporary_entry(root.path(), "g123ab").unwrap();
    let (directory, identity) = run_identity(&dir).unwrap().unwrap();
    let id = identity.unwrap().primary().to_string();
    let group = open_removal_group(&dir).unwrap();
    let name = retiring_lock_name(&dir, &id, RUN_LOCK_NAME);
    drop(create_run_marker(group.as_raw_fd(), &name).unwrap());
    flock_exclusive(&directory).unwrap();
    assert!(cleanup(&dir)
        .unwrap_err()
        .to_string()
        .contains("active or locked"));
    assert!(dir.is_dir());
    assert!(record_path(&dir, &name).is_file());
    drop(directory);
    cleanup(&dir).unwrap();
    assert!(!dir.exists());
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn nfs_legacy_recovery_retries_after_parking_the_ready_marker() {
    let root = tempfile::tempdir().unwrap();
    let (dir, run) = legacy_test_run(root.path());
    assert!(may_defer_unlink(&run.directory));
    let parked = record_path(&dir, &retiring_lock_name(&dir, &run.id, RUN_LOCK_NAME));
    let reader = File::create(dir.join("open-runtime-file")).unwrap();
    drop(run);
    assert!(cleanup(&dir).unwrap_err().is::<PendingGroupCleanup>());
    assert!(dir.is_dir() && parked.is_file());
    drop(reader);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        LEGACY_DIR_FLOCK_ERROR.set(Some(libc::EBADF));
        match cleanup(&dir) {
            Ok(()) => break,
            Err(error) if error.is::<PendingGroupCleanup>() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error:#}"),
        }
    }
    assert!(!dir.exists() && !parked.exists());
}

#[test]
fn portable_marker_parking_retries_an_owned_reservation() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (dir, run) = legacy_test_run(root.path());
    let group = open_removal_group(&dir).unwrap();
    let parked = retiring_lock_name(&dir, &run.id, RUN_LOCK_NAME);
    let path = dir.parent().unwrap().join(parked.to_str().unwrap());
    let from = run.directory.as_raw_fd();
    let to = group.as_raw_fd();

    fs::write(&path, b"fixture content").unwrap();
    assert!(park_run_marker_with_reservation(from, RUN_LOCK_NAME, to, &parked).is_err());
    assert!(marker_matches(from, RUN_LOCK_NAME, &run.lock).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"fixture content");
    fs::remove_file(&path).unwrap();
    let reservation = open_marker(
        to,
        &parked,
        libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL,
        0,
    )
    .unwrap();
    assert_eq!(reservation.metadata().unwrap().mode() & 0o777, 0);
    drop(reservation);

    park_run_marker(from, RUN_LOCK_NAME, to, &parked).unwrap();
    assert!(marker_matches(to, &parked, &run.lock).unwrap());
    assert!(!marker_matches(from, RUN_LOCK_NAME, &run.lock).unwrap());
    drop((group, run));
    cleanup(&dir).unwrap();
    assert!(!dir.exists());
    assert!(!path.exists());
}

#[test]
fn removal_reclaims_inodes_before_retrying_marker_parking() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (dir, run) = legacy_test_run(root.path());
    let contents = dir.join("files");
    fs::create_dir(&contents).unwrap();
    fs::write(contents.join("checkout"), b"temporary contents").unwrap();
    let held = run.lock.metadata().unwrap();
    let mut attempts = 0;
    run.remove_with_parking(&dir, |from, marker, to, parked| {
        attempts += 1;
        assert!(entry_matches(
            &marker_entry(from, marker).unwrap().unwrap(),
            &held
        ));
        if contents.exists() {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
        park_run_marker_with_reservation(from, marker, to, parked)
    })
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(!dir.exists());
    assert_eq!(fs::read_dir(dir.parent().unwrap()).unwrap().count(), 0);
}

#[test]
fn startup_and_recovery_use_the_marker_lock() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let expected = root.path().join("git-on/r123ab");
    let (_, (dir, held)) = create_temporary_entry_with(
        root.path(),
        "r123ab",
        |parent, group, identity, basename| {
            reserve_run(parent, group, identity, basename, |marker| {
                assert!(!expected.exists());
                flock_exclusive(marker)?;
                assert!(cleanup(&expected)
                    .unwrap_err()
                    .to_string()
                    .contains("active"));
                Ok(())
            })
        },
    )
    .unwrap();
    if fixture_has_run_identity(&dir) {
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
    }
    let flags = unsafe { libc::fcntl(held.lock.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDWR);
    finish_test_run(&dir, held);
    assert!(!dir.exists());
}

#[test]
fn startup_can_retry_after_recovery_reclaims_an_unlocked_marker() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let expected = root.path().join("git-on/r123ab");
    let mut pending = false;
    let result = create_temporary_entry_with(
        root.path(),
        "r123ab",
        |parent, group, identity, basename| {
            reserve_run(parent, group, identity, basename, |marker| {
                assert!(!expected.exists());
                let starter = marker.try_clone()?;
                let ownership_record = record_path(&expected, &run_record_name("r123ab"));
                AFTER_MARKER_UNLOCK.set(Some(Box::new(move || {
                    flock_exclusive(&starter).unwrap();
                    let folder = File::open(ownership_record).unwrap();
                    assert!(
                        !marker_matches(folder.as_raw_fd(), RUN_LOCK_NAME, &starter).unwrap(),
                        "startup can acquire a retiring marker under its startup name"
                    );
                    Ok(())
                })));
                MARKER_RENAME_LOST_REPLY.set(Some(libc::ETIMEDOUT));
                if let Err(error) = cleanup(&expected) {
                    assert!(error.is::<PendingGroupCleanup>(), "{error:#}");
                    pending = true;
                }
                flock_exclusive(marker)
            })
        },
    );
    assert!(result
        .err()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists));
    let (dir, starting) = reserve_test_run(root.path(), "r123ac");
    finish_test_run(&dir, starting);
    assert!(!dir.exists());
    if pending {
        cleanup(&expected).unwrap();
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn orphan_cleanup_retries_after_retiring_the_startup_marker() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("git-on/r123ab");
    let record = ownership_record(&dir);
    private_dir(dir.parent().unwrap()).unwrap();
    private_dir(&record).unwrap();
    let folder = File::open(record).unwrap();
    drop(create_run_marker(folder.as_raw_fd(), RUN_LOCK_NAME).unwrap());

    MARKER_RENAME_VERIFY_ERROR.set(Some(libc::ESTALE));
    let error = cleanup(&dir).unwrap_err();
    assert_eq!(io_errno(&error), Some(libc::ESTALE));
    assert_eq!(
        find_run_marker(&folder).unwrap().as_deref(),
        Some(RETIRED_RUN_LOCK)
    );
    cleanup(&dir).unwrap();
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn startup_retries_if_cleanup_removes_an_empty_record() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().canonicalize().unwrap();
    let dir = parent.join("git-on/r123ab");
    let name = run_record_name("r123ab");
    private_dir(dir.parent().unwrap()).unwrap();
    let group = open_removal_group(&dir).unwrap();
    // Keep the group present after the orphaned reservation is removed.
    let sibling = dir.parent().unwrap().join("sibling");
    fs::create_dir(&sibling).unwrap();
    make_temporary_directory(&group, &name).unwrap();
    RETRY_UNLINK_LOST_REPLY.set(Some(libc::ENOENT));
    cleanup(&dir).unwrap();
    let error =
        open_reserved_run_record(&group, &group.metadata().unwrap(), &name, &dir).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let (dir, starting) = reserve_test_run(&parent, "r123ac");
    fs::remove_dir(sibling).unwrap();
    finish_test_run(&dir, starting);
    assert!(fs::read_dir(&parent).unwrap().next().is_none());
}

#[test]
fn startup_reclaims_a_record_after_a_stale_lookup() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    for stale_rollback in [None, Some(libc::ESTALE)] {
        RUN_RECORD_STAT_ERROR.set(Some(libc::ESTALE));
        RUN_RECORD_ROLLBACK_ERROR.set(stale_rollback);
        let (dir, held) = reserve_test_run(root.path(), "r123ab");
        finish_test_run(&dir, held);
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}

#[test]
#[ignore = "requires a private Linux mount namespace with bind-mount permission"]
fn occupied_mount_does_not_prevent_a_basename_retry() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("git-on/r123ab");
    let name = run_record_name("r123ab");
    let result = create_temporary_entry_with(
        root.path(),
        "r123ab",
        |parent, group, identity, basename| {
            reserve_run(parent, group, identity, basename, |lock| {
                flock_exclusive(lock)?;
                fs::create_dir(&dir)?;
                let mounted = Command::new("mount")
                    .arg("--bind")
                    .arg(&dir)
                    .arg(&dir)
                    .status()?;
                if !mounted.success() {
                    return Err(std::io::Error::other("could not mount the fixture"));
                }
                Ok(())
            })
        },
    );
    let preserved = ensure_unmounted(&dir, None).is_err();
    if preserved {
        assert!(Command::new("umount").arg(&dir).status().unwrap().success());
    }
    assert!(preserved);
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(!record_path(&dir, &name).exists());
    assert!(dir.is_dir());
}

#[test]
fn startup_rollback_reclaims_state_after_setup_failure() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (stage, errno, fault) in [
        ("lock marker", libc::ENOLCK, None),
        (
            "stat ownership record",
            libc::EIO,
            Some(&RUN_RECORD_STAT_ERROR),
        ),
        ("mkdir run", libc::ENOSPC, Some(&RUN_MKDIR_ERROR)),
        ("open run", libc::EIO, Some(&RUN_OPEN_ERROR)),
        ("canonicalize run", libc::EIO, Some(&RUN_CANONICALIZE_ERROR)),
        ("stat run", libc::EIO, Some(&RUN_STAT_ERROR)),
        (
            "verify bound marker",
            libc::EIO,
            Some(&MARKER_RENAME_VERIFY_ERROR),
        ),
    ] {
        if let Some(fault) = fault {
            fault.set(Some(errno));
        }
        let result = create_temporary_entry_with(
            root.path(),
            "r123ab",
            |parent, group, identity, basename| {
                reserve_run(parent, group, identity, basename, |lock| {
                    if fault.is_some() {
                        flock_exclusive(lock)
                    } else {
                        Err(std::io::Error::from_raw_os_error(errno))
                    }
                })
            },
        );
        assert_eq!(
            result.err().and_then(|error| error.raw_os_error()),
            Some(errno),
            "{stage}"
        );
        assert!(
            fs::read_dir(root.path()).unwrap().next().is_none(),
            "{stage}"
        );
    }
}

#[test]
#[ignore = "requires an inode-limited tmpfs"]
fn rollback_reclaims_the_record_without_a_free_inode() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let fill = root.path().join("fill");
    fs::create_dir(&fill).unwrap();
    let result = create_temporary_entry_with(
        root.path(),
        "r123ab",
        |parent, group, identity, basename| {
            reserve_run(parent, group, identity, basename, |lock| {
                flock_exclusive(lock)?;
                for index in 0..32 {
                    match File::create_new(fill.join(index.to_string())) {
                        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => return Ok(()),
                        result => drop(result?),
                    }
                }
                panic!("fixture must be on an inode-limited tmpfs");
            })
        },
    );
    assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    assert!(fill.is_dir());
}

#[test]
fn startup_keeps_a_bound_marker_after_a_lost_rename_reply() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    MARKER_RENAME_LOST_REPLY.set(Some(libc::EIO));
    let (dir, held) = reserve_test_run(root.path(), "r123ab");
    let RunLock::Sibling(record) = &held.location else {
        panic!("expected a sibling ownership record");
    };
    assert!(parse_bound_run_marker(&record.marker).is_some());
    finish_test_run(&dir, held);
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn startup_refuses_to_publish_a_replaced_marker() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("git-on/r123ab");
    let record = ownership_record(&dir);
    RUN_BIND_REPLACE_MARKER.set(true);
    let outcome = try_reserve_test_run(root.path(), "r123ab");
    let error = outcome
        .err()
        .expect("startup accepted a replacement marker");
    assert!(error
        .to_string()
        .contains("temporary run marker changed during rename"));
    let folder = File::open(record).unwrap();
    let marker = find_run_marker(&folder).unwrap().unwrap();
    let (_, identity) = parse_bound_run_marker(&marker).unwrap();
    let replacement = open_run_marker(folder.as_raw_fd(), &marker, true).unwrap();
    assert!(check_bound_marker(&replacement, identity).is_err());
    let error = cleanup(&dir).unwrap_err();
    if fixture_has_run_identity(&dir) {
        assert!(error
            .to_string()
            .contains("does not match its recorded file"));
    }
    assert!(dir.is_dir());
}

#[test]
fn startup_refuses_a_run_moved_during_marker_binding() {
    for replaced in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("git-on/r123ab");
        let moved = dir.with_extension("moved");
        let record = ownership_record(&dir);
        let path = dir.clone();
        let destination = moved.clone();
        AFTER_RUN_BIND.set(Some(Box::new(move || {
            fs::rename(&path, &destination).unwrap();
            if replaced {
                private_dir(&path).unwrap();
                fs::write(path.join("sentinel"), b"preserve").unwrap();
            }
        })));
        assert!(
            try_reserve_test_run(root.path(), "r123ab").is_err(),
            "startup accepted a moved run after binding"
        );
        assert!(moved.is_dir());
        assert!(record.is_dir(), "startup discarded the moved run's record");
        if replaced {
            assert_eq!(fs::read(dir.join("sentinel")).unwrap(), b"preserve");
        }
    }
}

#[test]
fn startup_reclaims_an_empty_reservation_after_a_lost_mkdir_reply() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    for fault in [&RUN_RECORD_MKDIR_LOST_REPLY, &RUN_MARKER_CREATE_LOST_REPLY] {
        for errno in [libc::ETIMEDOUT, libc::EEXIST] {
            fault.set(Some(errno));
            let outcome = try_reserve_test_run(root.path(), "r123ab");
            let error = outcome
                .err()
                .expect("startup accepted an ambiguous reservation");
            assert_eq!(error.raw_os_error(), Some(errno));
            assert!(fs::read_dir(root.path()).unwrap().next().is_none());
        }
    }

    let (dir, active) = reserve_test_run(root.path(), "r123ab");
    let error = try_reserve_test_run(root.path(), "r123ab").err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(dir.is_dir());
    assert!(active.remove(&dir).unwrap());
}

#[test]
fn startup_retains_the_record_if_the_run_mkdir_loses_its_reply() {
    for errno in [libc::ETIMEDOUT, libc::EEXIST] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("git-on/r123ab");
        RUN_MKDIR_LOST_REPLY.set(Some(errno));
        let outcome = try_reserve_test_run(root.path(), "r123ab");
        let error = outcome.err().expect("startup accepted an ambiguous mkdir");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("ownership record was retained for manual recovery"));
        assert_eq!(diagnostic.lines().last(), dir.to_str());
        assert!(dir.is_dir());
        assert!(ownership_record(&dir)
            .join(OsStr::from_bytes(RUN_LOCK_NAME.to_bytes()))
            .is_file());
        assert!(cleanup(&dir).is_err());
        assert!(dir.is_dir());
    }
}

#[test]
#[ignore = "requires root to model a different user's directory"]
fn shared_group_collision_retries_after_another_uid_wins() {
    let root = tempfile::tempdir().unwrap();
    let group = root.path().join("git-on");
    fs::create_dir(&group).unwrap();
    fs::set_permissions(&group, fs::Permissions::from_mode(0o1777)).unwrap();
    RUN_MKDIR_COLLISION_OWNER.set(Some(65534));
    let error = try_reserve_test_run(root.path(), "r123ab").err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let foreign = group.join("r123ab");
    assert_eq!(foreign.metadata().unwrap().uid(), 65534);
    assert!(!record_path(&foreign, &run_record_name("r123ab")).exists());
    let (next, run) = reserve_test_run(root.path(), "r123ac");
    assert!(run.remove(&next).unwrap());
    assert!(foreign.is_dir());
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn startup_retains_recovery_when_nfs_defers_rollback() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    for mode in [0o700, 0o1770] {
        let root = tempfile::tempdir().unwrap();
        let group = root.path().join("git-on");
        fs::create_dir(&group).unwrap();
        fs::set_permissions(&group, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
        let dir = group.join("r123ab");
        let name = run_record_name("r123ab");
        let mut held = None;
        let result = create_temporary_entry_with(
            root.path(),
            "r123ab",
            |parent, group, identity, basename| {
                assert!(may_defer_unlink(group), "requires an NFS-backed TMPDIR");
                reserve_run(parent, group, identity, basename, |marker| {
                    flock_exclusive(marker)?;
                    let record = open_retry_record(group, &name).unwrap().unwrap();
                    held = Some(open_marker(
                        record.as_raw_fd(),
                        RUN_LOCK_NAME,
                        libc::O_RDONLY,
                        0,
                    )?);
                    RUN_MKDIR_ERROR.set(Some(libc::ENOSPC));
                    RETRY_RECORD_ERROR.set(Some(libc::ENOSPC));
                    Ok(())
                })
            },
        );
        assert_eq!(RETRY_RECORD_ERROR.take(), Some(libc::ENOSPC));
        let failure = result.err().expect("injected mkdir failure").to_string();
        assert!(failure.contains("cleanup is pending"), "{failure}");
        assert!(cleanup(&dir).unwrap_err().is::<PendingGroupCleanup>());
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match cleanup(&dir) {
                Ok(()) => break,
                Err(error) if error.is::<PendingGroupCleanup>() && Instant::now() < deadline => {}
                Err(error) => panic!("{error:#}"),
            }
        }
        if mode == 0o1770 {
            assert!(fs::read_dir(group).unwrap().next().is_none());
        } else {
            assert!(fs::read_dir(root.path()).unwrap().next().is_none());
        }
    }
}

#[test]
fn deferred_cleanup_displays_the_path_on_its_own_line() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let dir = Path::new(OsStr::from_bytes(
        b"/tmp/odd path-\xff-\n-'\\/git-on/r123ab",
    ));
    let diagnostic = PendingGroupCleanup::retry(dir).to_string();
    let argument = diagnostic.lines().last().unwrap();
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("printf '%s' {argument}"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, dir.as_os_str().as_bytes());
}

#[test]
fn remaining_mount_guidance_matches_available_recovery() {
    let dir = Path::new("/tmp/with space/git-on/r123ab");
    let manual = remaining_mount_error(dir, false).to_string();
    assert!(manual.contains("Manual recovery is required"));
    assert!(!manual.contains("--cleanup"));
    let path = CleanupPath(dir).to_string();
    assert_eq!(manual.lines().last(), Some(path.as_str()));
    let retry = remaining_mount_error(dir, true).to_string();
    assert!(retry.contains("git on --cleanup"));
    assert_eq!(retry.lines().last(), Some(path.as_str()));
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn deferred_removal_without_identity_reports_manual_recovery() {
    let root = tempfile::tempdir().unwrap();
    RUN_NO_PERSISTENT_IDENTITY.set(true);
    let (dir, run) = reserve_test_run(root.path(), "r123ab");
    let RunLock::Sibling(record) = &run.location else {
        panic!("expected a sibling ownership record");
    };
    assert!(
        may_defer_unlink(&record.group),
        "requires an NFS-backed TMPDIR"
    );
    assert!(!record.has_persistent_identity);
    let held = File::create(dir.join("open-runtime-file")).unwrap();
    let error = run.remove(&dir).unwrap_err();
    assert!(error.is::<PendingGroupCleanup>());
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("Manual recovery is required"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(dir.to_str().unwrap()));
    assert!(dir.is_dir());
    drop(held);
    let refused = cleanup(&dir).unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("does not match its ownership record")
            || refused.contains("persistent run directory identity"),
        "{refused}"
    );
    assert!(dir.is_dir());
    assert!(ownership_record(&dir).is_dir());
    // Remove this fixture directly once the kernel completes the unlink.
    let group = open_removal_group(&dir).unwrap();
    let directory = File::open(&dir).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match remove_run_path(&dir, &group, &directory, false, false) {
            Ok(()) => break,
            Err(error) if error.is::<PendingGroupCleanup>() && Instant::now() < deadline => {}
            Err(error) => panic!("{error:#}"),
        }
    }
}

#[test]
fn finishing_retirement_waits_for_an_asynchronous_unlink() {
    for (replaced, expected) in [
        (false, RetryRemoval::Removed),
        (true, RetryRemoval::Missing),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("retired");
        fs::create_dir(&path).unwrap();
        let hidden = path.join(".nfs-pending");
        File::create(&hidden).unwrap();
        let group = File::open(root.path()).unwrap();
        let folder = File::open(&path).unwrap();
        let mut retried = false;
        assert_eq!(
            wait_for_retry_record(&group, c"retired", &folder, || {
                assert!(!retried);
                retried = true;
                fs::remove_file(&hidden).unwrap();
                if replaced {
                    fs::remove_dir(&path).unwrap();
                    fs::create_dir(&path).unwrap();
                }
                true
            })
            .unwrap(),
            expected
        );
        assert!(retried);
        assert_eq!(path.is_dir(), replaced);
    }
}

#[test]
fn retirement_succeeds_after_a_lost_unlink_reply() {
    for (fault, errno) in [
        (&RUN_RMDIR_LOST_REPLY, libc::ETIMEDOUT),
        (&RUN_RETIRE_UNLINK_LOST_REPLY, libc::ETIMEDOUT),
        (&RETRY_UNLINK_LOST_REPLY, libc::ETIMEDOUT),
        (&RETRY_UNLINK_LOST_REPLY, libc::ENOENT),
    ] {
        let (root, dir, run) = reserved_run();
        let record = ownership_record(&dir);
        fault.set(Some(errno));
        assert!(run.remove(&dir).unwrap());
        assert!(!dir.exists());
        assert!(!record.exists());
    }
    for is_directory in [false, true] {
        let (root, dir, run) = reserved_run();
        if is_directory {
            fs::create_dir(dir.join("child")).unwrap();
        } else {
            File::create(dir.join("child")).unwrap();
        }
        DESCENDANT_UNLINK_LOST_REPLY.set(Some(libc::ETIMEDOUT));
        assert!(run.remove(&dir).unwrap());
        assert!(!dir.exists());
    }
}

#[test]
fn stale_marker_after_unlock_requires_a_peer_to_have_removed_the_record() {
    for peer_removed in [false, true] {
        let (root, dir, run) = reserved_run();
        let record = ownership_record(&dir);
        let path = dir.clone();
        AFTER_MARKER_UNLOCK.set(Some(Box::new(move || {
            if peer_removed {
                cleanup(&path).unwrap();
            }
            Err(io::Error::from_raw_os_error(libc::ESTALE))
        })));
        let outcome = run.remove(&dir);
        assert!(!dir.exists());
        assert_eq!(record.exists(), !peer_removed);
        if peer_removed {
            assert!(!outcome.expect("peer cleanup already finished retirement"));
        } else {
            assert_eq!(io_errno(&outcome.unwrap_err()), Some(libc::ESTALE));
        }
    }
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn unconfirmed_nfs_removal_reports_the_run_for_recovery() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let (root, dir, run) = reserved_run();
    assert!(is_nfs(&run.directory).unwrap());
    let record = ownership_record(&dir);
    RUN_RMDIR_LOST_REPLY.set(Some(libc::ETIMEDOUT));
    RUN_RMDIR_ATTRIBUTES_UNAVAILABLE.set(true);
    let result = run.remove(&dir);
    RUN_RMDIR_ATTRIBUTES_UNAVAILABLE.set(false);
    let diagnostic = format!("{:#}", result.unwrap_err());
    assert!(diagnostic.contains("git on --cleanup"), "{diagnostic}");
    assert!(diagnostic.lines().any(|line| Some(line) == dir.to_str()));
    assert!(!dir.exists() && record.is_dir());
    cleanup(&dir).unwrap();
    assert!(!record.exists());
}

#[test]
fn cleanup_checks_disappearing_and_replaced_children() {
    for before_open in [
        None,
        Some(BeforeChildOpen::ReplaceWithSymlink),
        Some(BeforeChildOpen::Stale { removed: true }),
        Some(BeforeChildOpen::Stale { removed: false }),
    ] {
        let (root, dir, run) = reserved_run();
        let outside = dir.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("retained"), b"preserve").unwrap();
        fs::create_dir(dir.join("child")).unwrap();
        fs::write(dir.join("remaining"), b"temporary").unwrap();
        BEFORE_CHILD_OPEN.set(before_open);
        REMOVE_OPEN_CHILD.set(before_open.is_none());
        let outcome = run.remove(&dir);
        assert!(BEFORE_CHILD_OPEN.take().is_none() && !REMOVE_OPEN_CHILD.get());
        if matches!(before_open, Some(BeforeChildOpen::Stale { removed: false })) {
            assert_eq!(io_errno(&outcome.unwrap_err()), Some(libc::ESTALE));
            assert!(dir.join("child").is_dir());
            assert!(ownership_record(&dir).is_dir());
        } else {
            assert!(outcome.unwrap());
            assert!(!dir.exists());
        }
        assert_eq!(fs::read(outside.join("retained")).unwrap(), b"preserve");
    }
}

#[test]
fn a_run_renamed_before_removal_keeps_its_recovery_marker() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    for replacement in [false, true] {
        let (root, dir, run) = reserved_run();
        let record = ownership_record(&dir);
        fs::write(dir.join("state"), b"retained").unwrap();
        if replacement && is_nfs(&run.directory).unwrap() {
            // On NFS, move the run after the retry has checked its identity.
            RUN_RMDIR_WAS_BUSY.set(true);
        }
        MOVE_RUN_BEFORE_REMOVAL.set(Some(replacement));
        let outcome = run.remove(&dir);
        if replacement {
            assert_eq!(
                fs::read(dir.join("replacement")).ok().as_deref(),
                Some(b"preserve".as_slice()),
                "cleanup removed the replacement directory"
            );
            assert!(outcome.is_err());
            fs::remove_dir_all(&dir).unwrap();
        } else {
            let error = outcome.unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::NotFound
            );
        }
        let moved = dir.with_extension("moved");
        assert_eq!(fs::read(moved.join("state")).unwrap(), b"retained");
        assert!(record.is_dir());
        fs::rename(&moved, &dir).unwrap();
        if fixture_has_run_identity(&dir) {
            cleanup(&dir).unwrap();
            assert!(!record.exists());
        }
    }
}

#[test]
#[ignore = "requires an isolated Linux mount namespace with bind-mount permission"]
fn cleanup_walk_leaves_a_late_bind_mount_untouched() {
    let (root, dir, run) = reserved_run();
    let outside = root.path().join("outside");
    let mount = dir.join("mount");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&mount).unwrap();
    let original = outside.join("retained");
    fs::write(&original, b"outside contents").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
    let outside_name = CString::new(outside.as_os_str().as_bytes()).unwrap();
    let mount_name = CString::new(mount.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::mount(
                outside_name.as_ptr(),
                mount_name.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    // Exercise the walkers directly, as if the mount arrived after the
    // outer mount-table check. The bind deliberately has the same device ID.
    let restore = git_on::materialize::prepare_cleanup(&run.directory);
    let group = open_removal_group(&dir).unwrap();
    let reclaim = removal::reclaim_run_contents(&dir, &group, &run.directory, &[c".lock"]);
    let remove = remove_run_path(&dir, &group, &run.directory, false, false);
    assert_eq!(unsafe { libc::umount(mount_name.as_ptr()) }, 0);
    assert_eq!(io_errno(&restore.unwrap_err()), Some(libc::EXDEV));
    assert_eq!(io_errno(&reclaim.unwrap_err()), Some(libc::EXDEV));
    assert_eq!(io_errno(&remove.unwrap_err()), Some(libc::EXDEV));
    assert_eq!(fs::read(original).unwrap(), b"outside contents");
    assert_eq!(outside.metadata().unwrap().mode() & 0o777, 0o500);
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn retirement_retries_when_the_last_placeholder_has_gone() {
    let (root, dir, run) = reserved_run();
    LAST_RETIREMENT_RMDIR_WAS_BUSY.set(true);
    assert!(run.remove(&dir).unwrap());
    assert!(!ownership_record(&dir).exists());
}

#[test]
#[ignore = "requires an NFS client that defers unlink of opened files"]
fn nfs_retirement_closes_the_marker_before_unlink() {
    let (root, dir, run) = reserved_run();
    assert!(is_nfs(&run.directory).unwrap());
    ASSERT_MARKER_UNLINK_IS_IMMEDIATE.set(true);
    assert!(run.remove(&dir).unwrap());
    assert!(!ASSERT_MARKER_UNLINK_IS_IMMEDIATE.get());
}

#[test]
fn a_failed_filesystem_probe_does_not_turn_removal_failure_into_pending_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let group = File::open(root.path()).unwrap();
    let dir = root.path().join("run");
    private_dir(&dir).unwrap();
    File::create(dir.join("remaining")).unwrap();
    let directory = File::open(&dir).unwrap();
    RUN_FILESYSTEM_PROBE_ERROR.set(Some(libc::EIO));
    let error = remove_run_path(&dir, &group, &directory, true, true).unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::DirectoryNotEmpty,
    );
    assert_eq!(RUN_FILESYSTEM_PROBE_ERROR.take(), None);
}

fn long_path_descendant(directory: &File) -> File {
    let mut ancestor = directory.try_clone().unwrap();
    let component = CString::new("d".repeat(200)).unwrap();
    for _ in 0..24 {
        make_temporary_directory(&ancestor, &component).unwrap();
        ancestor = open_marker(
            ancestor.as_raw_fd(),
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .unwrap();
    }
    ancestor
}

#[test]
fn cleanup_handles_a_deferred_name_beyond_the_absolute_path_limit() {
    let (root, dir, run) = reserved_run();
    let ancestor = long_path_descendant(&run.directory);
    let placeholder = c".nfs0123456789abcdef00000001";
    assert_eq!(
        unsafe {
            libc::symlinkat(
                c"missing".as_ptr(),
                ancestor.as_raw_fd(),
                placeholder.as_ptr(),
            )
        },
        0
    );
    assert!(has_nfs_placeholder(&ancestor));
    drop(ancestor);
    assert!(run.remove(&dir).unwrap());
    assert!(!dir.exists());
    assert!(!ownership_record(&dir).exists());
}

fn has_nfs_placeholder(directory: &File) -> bool {
    remaining_entries(directory.as_raw_fd(), pending_nfs_entry)
        .unwrap()
        .deferred
}

#[test]
fn deferred_nfs_cleanup_preserves_command_status() {
    use std::{os::unix::process::ExitStatusExt, process::ExitStatus};

    for late_close in [false, true] {
        let (root, dir, run) = reserved_run();
        let RunLock::Sibling(record) = &run.location else {
            panic!("expected a sibling ownership record");
        };
        let can_recover = record.has_persistent_identity;
        let record = ownership_record(&dir);
        let pending = dir.join(".nfs0123456789abcdef00000001");
        File::create(&pending).unwrap();
        FORCE_NFS_REMOVAL.set(true);
        DESCENDANT_BUSY.set(Some(c".nfs0123456789abcdef00000001"));
        if late_close {
            let pending = pending.clone();
            AFTER_NFS_REMOVAL_FAILURE.replace(Some(Box::new(move |_| {
                if pending.exists() {
                    fs::remove_file(&pending).unwrap();
                }
            })));
        }
        let cleanup_result = run.remove(&dir).map(|_| ());
        AFTER_NFS_REMOVAL_FAILURE.replace(None);
        FORCE_NFS_REMOVAL.set(false);
        DESCENDANT_BUSY.set(None);

        let command_status = Ok(ExitStatus::from_raw(23 << 8));
        let status = crate::finish_execution(&dir, command_status, cleanup_result, None)
            .expect("deferred unlink replaced the command's exit status");
        assert_eq!(status.code(), Some(23));
        assert_eq!(pending.exists(), !late_close);
        assert_eq!(record.exists(), !late_close);
        if !late_close {
            fs::remove_file(pending).unwrap();
            let recovery = cleanup(&dir);
            assert_eq!(recovery.is_ok(), can_recover, "{recovery:?}");
        }
        let removed = late_close || can_recover;
        assert_eq!(dir.exists(), !removed);
        assert_eq!(record.exists(), !removed);
    }
}

#[test]
fn nfs_cleanup_retries_late_entries_once() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    for (name, insertions) in [(".nfs0123456789abcdef00000001", 1), ("late-entry", 3)] {
        let (root, dir, run) = reserved_run();
        let record = ownership_record(&dir);
        let can_recover = fixture_has_run_identity(&dir);
        FORCE_NFS_REMOVAL.set(true);
        EXTRA_ROOT_ENTRY_NAME.set(name);
        EXTRA_ROOT_ENTRIES.set(insertions);
        let outcome = run.remove(&dir);
        FORCE_NFS_REMOVAL.set(false);
        EXTRA_ROOT_ENTRY_NAME.set("late-entry");
        let remaining = EXTRA_ROOT_ENTRIES.replace(0);
        if insertions == 1 {
            assert!(outcome.unwrap());
            assert!(!dir.exists() && !record.exists());
            assert_eq!(remaining, 0);
        } else {
            let error = outcome.expect_err("cleanup retried past its rescan allowance");
            assert!(!error.is::<PendingGroupCleanup>(), "{error:#}");
            assert_eq!(remaining, 1);
            let diagnostic = format!("{error:#}");
            assert!(diagnostic.lines().any(|line| Some(line) == dir.to_str()));
            assert!(dir.is_dir() && record.is_dir());
            if can_recover {
                assert!(diagnostic.contains("git on --cleanup"), "{diagnostic}");
                cleanup(&dir).unwrap();
                assert!(!dir.exists() && !record.exists());
            } else {
                assert!(
                    diagnostic.contains("Manual recovery is required"),
                    "{diagnostic}"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn nfs_mount_information_survives_a_failed_filesystem_probe() {
    let (root, dir, run) = reserved_run();
    assert!(is_nfs(&run.directory).unwrap());
    let descendant = long_path_descendant(&run.directory);
    let fd = descendant.as_raw_fd();
    let held = create_run_marker(fd, c"open-file").unwrap();
    unlink_entry(fd, c"open-file", 0).unwrap();
    assert!(has_nfs_placeholder(&descendant));
    RUN_FILESYSTEM_PROBE_ERROR.set(Some(libc::EIO));
    let error = run.remove(&dir).unwrap_err();
    assert!(error.is::<PendingGroupCleanup>(), "{error:#}");
    assert!(error.to_string().contains(dir.to_str().unwrap()));
    assert_eq!(RUN_FILESYSTEM_PROBE_ERROR.take(), None);
    drop(held);
}

#[test]
#[ignore = "requires an NFS-backed TMPDIR"]
fn nfs_removal_retries_when_the_open_file_closes_before_the_scan() {
    let (root, dir, run) = reserved_run();
    assert!(is_nfs(&run.directory).unwrap());
    let mut held = ["first", "second"].map(|name| {
        let child = dir.join(name);
        fs::create_dir(&child).unwrap();
        let file = File::create(child.join("held")).unwrap();
        (File::open(child).unwrap(), Some(file))
    });
    let closed = std::rc::Rc::new(std::cell::Cell::new(0));
    let closed_in_hook = closed.clone();
    AFTER_NFS_REMOVAL_FAILURE.replace(Some(Box::new(move |scope| {
        let (directory, file) = held
            .iter_mut()
            .find(|(directory, file)| file.is_some() && has_nfs_placeholder(directory))
            .expect("expected a newly deferred unlink");
        let blocked = fs::metadata(format!("/proc/self/fd/{scope}")).unwrap();
        let held = directory.metadata().unwrap();
        assert_eq!(
            (blocked.dev(), blocked.ino()),
            (held.dev(), held.ino()),
            "cleanup rescanned outside the blocked directory"
        );
        drop(file.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        while has_nfs_placeholder(directory) {
            assert!(
                Instant::now() < deadline,
                "NFS did not release the placeholder"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        closed_in_hook.set(closed_in_hook.get() + 1);
    })));
    let outcome = run.remove(&dir);
    AFTER_NFS_REMOVAL_FAILURE.replace(None);
    assert_eq!(closed.get(), 2);
    assert!(outcome.unwrap());
    assert!(!dir.exists());
    assert!(!ownership_record(&dir).exists());
}

#[test]
fn legacy_recovery_retires_marker_before_its_retry_directory() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("git-on");
    private_dir(&path).unwrap();
    let group = File::open(&path).unwrap();
    let dir = path.join("g123ab");
    let marker = c".git-on-remove-g123ab-ready-11111111-1111-1111-1111-111111111111";
    let retry = retry_marker_name(marker);
    prepare_retry_record(&group, &retry, &dir).unwrap();
    create_run_marker(group.as_raw_fd(), marker).unwrap();

    let records = vec![retry.clone(), marker.to_owned()];
    assert!(
        cleanup_legacy_marker_records(&dir, &group, &group.metadata().unwrap(), records).unwrap()
    );
    assert!(!record_path(&dir, marker).exists());
    assert!(!record_path(&dir, &retry).exists());
}

#[test]
fn stale_retirement_group_must_have_disappeared_or_changed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("git-on");
    private_dir(&path).unwrap();
    let group = File::open(&path).unwrap();
    let identity = group.metadata().unwrap();
    let dir = path.join("r123ab");
    let name = retry_marker_name(&run_record_name("r123ab"));
    let folder = prepare_retry_record(&group, &name, &dir).unwrap();
    let finish = || finish_retry_record(&group, &identity, &dir, &name, &folder, || Ok(()));
    for fault in [&RETRY_METADATA_ERROR, &RETRY_UNLINK_ERROR] {
        fault.set(Some(libc::ESTALE));
        let error = finish().unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ESTALE)
        );
        assert!(record_path(&dir, &name).is_dir());
    }
    fs::remove_dir(path.join(name.to_str().unwrap())).unwrap();
    RETRY_METADATA_ERROR.set(Some(libc::ESTALE));
    assert!(!finish().unwrap());
    assert!(path.is_dir());
    fs::remove_dir(&path).unwrap();
    assert!(!finish().unwrap());
    private_dir(&path).unwrap();
    assert!(!finish().unwrap());
}

#[test]
#[ignore = "requires root to set the owner of a fixture directory"]
fn legacy_recovery_skips_another_owners_records() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("git-on");
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o1777)).unwrap();
    let group = File::open(&path).unwrap();
    let identity = group.metadata().unwrap();
    let dir = path.join("g123ab");
    let foreign = c".git-on-remove-g123ab-ready-11111111-1111-1111-1111-111111111111-retry";
    let own = c".git-on-remove-g123ab-ready-22222222-2222-2222-2222-222222222222-retry";
    let other = prepare_retry_record(&group, foreign, &dir).unwrap();
    assert_eq!(unsafe { libc::fchown(other.as_raw_fd(), 65534, !0) }, 0);
    let blocker = ordinary_marker_name(own);
    let marker = create_run_marker(group.as_raw_fd(), &blocker).unwrap();
    assert_eq!(unsafe { libc::fchown(marker.as_raw_fd(), 65534, !0) }, 0);
    prepare_retry_record(&group, own, &dir).unwrap();
    assert!(!cleanup_orphaned_marker(&dir, &group, &identity, foreign, false, None).unwrap());
    cleanup(&dir).unwrap();
    assert!(marker_entry(group.as_raw_fd(), foreign).unwrap().is_some());
    assert!(marker_entry(group.as_raw_fd(), &blocker).unwrap().is_some());
    assert!(marker_entry(group.as_raw_fd(), own).unwrap().is_none());
}

#[test]
fn cleanup_preserves_the_run_if_its_group_loses_private_permissions() {
    let (root, dir, locked) = reserved_run();
    let group = dir.parent().unwrap();
    let sentinel = dir.join("runtime");
    fs::write(&sentinel, b"owned contents").unwrap();
    fs::set_permissions(group, fs::Permissions::from_mode(0o777)).unwrap();
    let result = locked.remove(&dir);
    fs::set_permissions(group, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("temporary run group changed"));
    assert_eq!(fs::read(sentinel).unwrap(), b"owned contents");
    assert!(ownership_record(&dir).is_dir());
}

#[test]
fn cleanup_checks_the_original_directory_before_releasing_storage() {
    let (root, dir, locked) = reserved_run();
    locked.check_directory(&dir).unwrap();
    let original = root.path().join("reserved");
    fs::rename(&dir, &original).unwrap();
    private_dir(&dir).unwrap();
    assert!(locked.check_directory(&dir).is_err());
    drop(locked);
    let sentinel = dir.join("unrelated");
    fs::write(&sentinel, b"preserve").unwrap();
    let error = cleanup(&dir).unwrap_err();
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
    assert!(original.is_dir());
    if !fixture_has_run_identity(&dir) {
        assert!(format!("{error:#}").contains("persistent run directory identity"));
        return;
    }
    assert!(format!("{error:#}").contains("temporary run does not match its ownership record"));

    fs::remove_file(sentinel).unwrap();
    let record = ownership_record(&dir);
    let bound = fs::read_dir(&record)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // Simulate inode reuse by giving the replacement the exact binding the
    // inode-only protocol would have produced for it.
    let metadata = dir.metadata().unwrap();
    let mut old = blake3::Hasher::new_derive_key("git-on temporary run identity v2");
    old.update(&metadata.ino().to_le_bytes());
    old.update(&metadata.uid().to_le_bytes());
    old.update(dir.file_name().unwrap().as_encoded_bytes());
    let old_id = uuid::Uuid::from_bytes(old.finalize().as_bytes()[..16].try_into().unwrap());
    let weak_name = bound_run_marker(
        &RunIdentity::FileHandle {
            handle: old_id,
            creation_time: None,
        },
        {
            let marker = File::open(&bound).unwrap();
            marker_identity(&marker, &marker.metadata().unwrap()).unwrap()
        },
    );
    let weak = record.join(OsStr::from_bytes(weak_name.to_bytes()));
    fs::rename(bound, &weak).unwrap();
    let error = cleanup(&dir).unwrap_err();
    assert!(format!("{error:#}").contains("temporary run does not match its ownership record"));
    assert!(dir.is_dir());

    let unbound = record.join(OsStr::from_bytes(RUN_LOCK_NAME.to_bytes()));
    fs::rename(weak, &unbound).unwrap();
    let error = cleanup(&dir).unwrap_err();
    assert!(format!("{error:#}").contains("temporary run does not match its ownership record"));
    assert!(unbound.is_file());
    assert!(dir.is_dir());
}

#[test]
fn new_run_recovery_requires_a_sibling_record() {
    let root = tempfile::tempdir().unwrap();
    let dir = git_on::platform::create_temporary_entry(root.path(), "r123ab").unwrap();
    let directory = File::open(&dir).unwrap();
    drop(create_run_marker(directory.as_raw_fd(), RUN_LOCK_NAME).unwrap());
    let error = cleanup(&dir).unwrap_err();
    let expected = if fixture_has_run_identity(&dir) {
        "missing temporary run ownership record"
    } else {
        "persistent run directory identity"
    };
    assert!(format!("{error:#}").contains(expected));
    assert!(dir
        .join(OsStr::from_bytes(RUN_LOCK_NAME.to_bytes()))
        .is_file());

    File::create(dir.join(".lock")).unwrap();
    fs::write(dir.join(".state"), serde_json::to_vec(&recovery()).unwrap()).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let result = cleanup(&dir);
    assert!(dir.is_dir(), "cleanup treated a grouped run as legacy");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("unexpected permissions"));
}

#[test]
fn recovery_preserves_the_mount_identity_with_either_recorded_identifier() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = git_on::platform::create_temporary_entry(root.path(), "r123ab").unwrap();
    let base = uuid::Uuid::from_u128(1);
    let alternate = uuid::Uuid::from_u128(2);
    let name = run_record_name("r123ab");
    let path = record_path(&dir, &name);
    private_dir(&path).unwrap();
    let folder = File::open(path).unwrap();
    let lock = create_run_marker(folder.as_raw_fd(), RUN_LOCK_NAME).unwrap();
    let marker = bound_run_marker(
        &RunIdentity::FileHandle {
            handle: base,
            creation_time: Some(alternate),
        },
        marker_identity(&lock, &lock.metadata().unwrap()).unwrap(),
    );
    park_run_marker(
        folder.as_raw_fd(),
        RUN_LOCK_NAME,
        folder.as_raw_fd(),
        &marker,
    )
    .unwrap();
    drop(lock);
    for available in [
        RunIdentity::FileHandle {
            handle: base,
            creation_time: None,
        },
        RunIdentity::CreationTime(alternate),
    ] {
        let recovered = lock_sibling_run(
            &dir,
            File::open(&dir).unwrap(),
            open_removal_group(&dir).unwrap(),
            &available,
        )
        .unwrap();
        assert_eq!(recovered.id, base.to_string());
    }
}

#[test]
fn flat_cleanup_preserves_the_legacy_pin_in_the_source_repository() {
    let root = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(root.path().join("source")).unwrap();
    let oid = repo.treebuilder(None).unwrap().write().unwrap();
    let pin = "refs/git-on/source-state";
    repo.reference(pin, oid, false, "fixture").unwrap();

    let run = root.path().join("runtime");
    private_dir(&run).unwrap();
    File::create(run.join(".lock")).unwrap();
    let record = serde_json::json!({
        "format": "git-on-temp-v1",
        "id": "source-state",
        "git_dir": repo.path(),
        "tree_oid": oid.to_string(),
        "pin_ref": pin,
    });
    fs::write(run.join(".state"), record.to_string()).unwrap();
    cleanup(&run).unwrap();
    assert!(!run.exists());
    assert_eq!(repo.find_reference(pin).unwrap().target(), Some(oid));

    let source = repo.workdir().unwrap();
    fs::set_permissions(source, fs::Permissions::from_mode(0o700)).unwrap();
    File::create(source.join(".lock")).unwrap();
    fs::write(source.join(".state"), record.to_string()).unwrap();
    let error = cleanup(source).unwrap_err();
    assert!(
        error.to_string().contains("refuses a Git repository"),
        "{error:#}"
    );
    assert_eq!(repo.find_reference(pin).unwrap().target(), Some(oid));
}
