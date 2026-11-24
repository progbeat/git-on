# Run inside disposable mount, network and UTS namespaces as root. The
# server binds only to loopback and the tests run as the ordinary user.
set -eu
binary=$(realpath "$1")
test_uid=$2
test_gid=$3
test_binary=$(realpath "$4")
root=$(mktemp -d /tmp/git-on-nfs.XXXXXXXX)
mountd=
nfs_started=false
small_shm=false
cleanup() {
    status=$?
    trap - EXIT
    if [ "$status" -ne 0 ] && [ -f "$root/mountd.log" ]; then
        cat "$root/mountd.log" >&2
    fi
    if mountpoint -q "$root/client"; then umount "$root/client" || exit 1; fi
    if "$small_shm"; then umount /dev/shm || exit 1; fi
    if mountpoint -q "$root/memory"; then umount "$root/memory" || exit 1; fi
    if "$nfs_started"; then rpc.nfsd 0; fi
    if [ -n "$mountd" ]; then
        kill "$mountd" 2>/dev/null || :
        wait "$mountd" 2>/dev/null || :
    fi
    rm -rf -- "$root"
    exit "$status"
}
trap cleanup EXIT
test "$test_uid" -ne 0
chmod 0755 "$root"
TMPDIR="$root" "$test_binary" --ignored --exact \
    run_directory::tests::cleanup_walk_leaves_a_late_bind_mount_untouched \
    > "$root/bind-test.log" || { cat "$root/bind-test.log"; exit 1; }
cat "$root/bind-test.log"
grep -Fq 'test result: ok. 1 passed;' "$root/bind-test.log"
install -d -o "$test_uid" -g "$test_gid" "$root/export"
mkdir "$root/client"
ip link set lo up
hostname git-on-nfs-test
mount -t nfsd nfsd /proc/fs/nfsd
mount -t tmpfs -o size=16m tmpfs /var/lib
mkdir -p /var/lib/nfs/v4recovery
touch /var/lib/nfs/etab /var/lib/nfs/rmtab
# fusermount3 must be able to enter the user's private mountpoint.
exportfs -i -o rw,sync,fsid=0,no_subtree_check,no_root_squash "127.0.0.1:$root/export"
rpc.mountd --foreground --no-nfs-version 2 --no-nfs-version 3 > "$root/mountd.log" 2>&1 &
mountd=$!
rpc.nfsd -H 127.0.0.1 -N 3 -V 4 -L 10 -G 10 2
nfs_started=true
timeout 35 mount -i -n -t nfs4 -o vers=4.1,proto=tcp,addr=127.0.0.1,clientaddr=127.0.0.1,local_lock=none,timeo=20,retrans=2,soft \
    127.0.0.1:/ "$root/client"
run_tests() {
    setpriv --reuid "$test_uid" --regid "$test_gid" --clear-groups \
        env -u LD_LIBRARY_PATH TMPDIR="$root/client" timeout 90 "$@"
}
run_tests python3 tests/integration.py --binary "$binary" --require-nfs-tmpdir --require-fuse \
    CliTests.test_copy_status_when_nfs_defers_removing_run_contents \
    CliTests.test_legacy_cleanup_can_retry_after_masked_retirement_directory \
    MountTests.test_cleanup_after_reading_a_spilled_blob
run_tests "$test_binary" --include-ignored --exact --test-threads=1 \
    run_directory::tests::startup_retains_recovery_when_nfs_defers_rollback \
    run_directory::tests::unconfirmed_nfs_removal_reports_the_run_for_recovery \
    run_directory::tests::nfs_legacy_recovery_retries_after_parking_the_ready_marker \
    run_directory::tests::nfs_mount_information_survives_a_failed_filesystem_probe \
    run_directory::tests::nfs_removal_retries_when_the_open_file_closes_before_the_scan \
    run_directory::tests::deferred_removal_without_identity_reports_manual_recovery \
    run_directory::tests::nfs_retirement_closes_the_marker_before_unlink \
    > "$root/rust-tests.log" || { cat "$root/rust-tests.log"; exit 1; }
cat "$root/rust-tests.log"
grep -Fq 'test result: ok. 7 passed;' "$root/rust-tests.log"
mkdir "$root/memory"
mount -t tmpfs -o "size=16m,mode=0700,uid=$test_uid,gid=$test_gid" tmpfs "$root/memory"
mount -t tmpfs -o size=4m,mode=1777 tmpfs /dev/shm
small_shm=true
GIT_ON_TEST_SERVICE_TMPFS="$root/memory" run_tests python3 tests/integration.py --binary "$binary" \
    CliTests.test_native_copy_uses_larger_service_tmpfs
