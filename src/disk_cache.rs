//! Multiprocess, byte/entry-bounded disk LRU. Ordinary operations follow a
//! constant number of on-disk links; only dirty recovery enumerates the cache.
use crate::object::{check_transfer, copy_from_odb, copy_verified};
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid, Repository};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::PathBuf,
    sync::Mutex,
    time::Instant,
};

type Key = [u8; 20];
fn key(oid: Oid) -> Key {
    oid.as_bytes().try_into().expect("SHA-1 OID")
}
fn oid(key: Key) -> Oid {
    Oid::from_bytes(&key).expect("SHA-1 OID")
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    first: Option<Key>,
    last: Option<Key>,
    bytes: u64,
    count: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    previous: Option<Key>,
    next: Option<Key>,
    weight: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiskCacheMetrics {
    pub metadata_reads: u64,
    pub metadata_writes: u64,
    pub recovery_scans: u64,
    pub recovery_entries: u64,
    pub evictions: u64,
    pub restores: u64,
    pub misses: u64,
    pub stores: u64,
    pub lock_wait_ns: u128,
    pub lock_held_ns: u128,
}
/// Payloads keep the v1 format (type byte + exact object bytes). The index is
/// private metadata, rebuilt once on migration or after an interrupted mutation.
/// Run-local ODB copies remain independent of global eviction.
pub struct DiskCache {
    root: PathBuf,
    budget: u64,
    entries: usize,
    metrics: Mutex<DiskCacheMetrics>,
}
struct Locked<'a> {
    cache: &'a DiskCache,
    _lock: File,
    index: Index,
    dirty: bool,
    started: Instant,
}
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        self.cache
            .measure(|m| m.lock_held_ns += self.started.elapsed().as_nanos());
        // A failed mutation intentionally leaves .dirty. The next holder
        // rebuilds from atomically published payloads, never partial links.
    }
}
impl DiskCache {
    pub fn standard() -> Result<Self> {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|x| PathBuf::from(x).join(".cache")))
            .context("remote cache requires XDG_CACHE_HOME or HOME")?
            .join("git-on/objects-v2");
        Self::new(
            root,
            crate::remote::DISK_CACHE_BYTES,
            crate::remote::DISK_CACHE_ENTRIES,
        )
    }
    pub fn new(root: PathBuf, budget: u64, entries: usize) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        Ok(Self {
            root,
            budget,
            entries,
            metrics: Mutex::new(DiskCacheMetrics::default()),
        })
    }
    fn measure(&self, update: impl FnOnce(&mut DiskCacheMetrics)) {
        update(&mut self.metrics.lock().unwrap_or_else(|e| e.into_inner()));
    }
    pub fn metrics(&self) -> DiskCacheMetrics {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn payload(&self, id: Key) -> PathBuf {
        self.root.join(oid(id).to_string())
    }
    fn node_path(&self, id: Key) -> PathBuf {
        self.root.join(".index").join(oid(id).to_string())
    }
    fn read_json<T: DeserializeOwned>(&self, path: PathBuf) -> Result<T> {
        self.measure(|m| m.metadata_reads += 1);
        // Index and link records have a fixed maximum size, independent of K.
        let mut data = Vec::with_capacity(256);
        File::open(path)?.take(2048).read_to_end(&mut data)?;
        Ok(serde_json::from_slice(&data)?)
    }
    fn atomic_json(&self, path: PathBuf, value: &impl Serialize) -> Result<()> {
        self.measure(|m| m.metadata_writes += 1);
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        let data = serde_json::to_vec(value)?;
        ensure!(
            data.len() <= 256,
            "cache index record exceeds its metadata budget"
        );
        file.write_all(&data)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
    fn node(&self, id: Key) -> Result<Node> {
        self.read_json(self.node_path(id))
    }
    fn put_node(&self, id: Key, value: &Node) -> Result<()> {
        self.atomic_json(self.node_path(id), value)
    }
    fn lock(&self) -> Result<Locked<'_>> {
        let waiting = Instant::now();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join("lock"))?;
        loop {
            check_transfer()?;
            ensure!(
                !crate::git::operations_stopped(),
                "remote cache lock stopped after command completion"
            );
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Err(error.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.measure(|m| m.lock_wait_ns += waiting.elapsed().as_nanos());
        let mut locked = Locked {
            cache: self,
            _lock: file,
            index: Index::default(),
            dirty: false,
            started: Instant::now(),
        };
        if self.root.join(".dirty").try_exists()? || !self.root.join(".state").try_exists()? {
            locked.recover()?;
        } else {
            locked.index = self.read_json(self.root.join(".state"))?;
        }
        locked.evict(0, 0)?;
        locked.commit()?;
        Ok(locked)
    }
    pub(crate) fn restore(&self, repo: &Repository, object: Oid) -> Result<bool> {
        let mut locked = self.lock()?;
        let id = key(object);
        let mut file = match File::open(self.payload(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.node_path(id).try_exists()? {
                    locked.remove(id)?;
                    locked.commit()?;
                }
                self.measure(|m| m.misses += 1);
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let length = file.metadata()?.len();
        let mut byte = [0];
        let kind = if file.read(&mut byte)? == 1 {
            ObjectType::from_raw(i32::from(byte[0]))
        } else {
            None
        };
        let kind = kind.filter(|kind| {
            matches!(
                kind,
                ObjectType::Commit | ObjectType::Tree | ObjectType::Blob | ObjectType::Tag
            )
        });
        let Some(kind) = kind else {
            locked.remove(id)?;
            locked.commit()?;
            return Ok(false);
        };
        let size =
            usize::try_from(length - 1).context("cache object too large for this platform")?;
        // First verify the untrusted payload without publishing anything to the
        // ODB. A second streamed pass writes its verified bytes to a staging ODB
        // writer, and checks again before finalizing its atomic publication.
        if let Err(error) = copy_verified(&mut file, std::io::sink(), kind, size, object) {
            if !error.is::<crate::object::InvalidObject>() {
                return Err(error).context("verify remote cache object");
            }
            check_transfer()?;
            locked.remove(id)?;
            locked.commit()?;
            return Ok(false);
        }
        file.seek(SeekFrom::Start(1))?;
        let odb = repo.odb()?;
        let mut writer = odb.writer(size, kind)?;
        copy_verified(&mut file, &mut writer, kind, size, object)?;
        ensure!(
            writer.finalize()? == object,
            "cache object changed during restore"
        );
        locked.touch(id)?;
        locked.commit()?;
        self.measure(|m| m.restores += 1);
        Ok(true)
    }
    pub(crate) fn store(&self, repo: &Repository, object: Oid) -> Result<()> {
        let odb = repo.odb()?;
        let (size, kind) = odb.read_header(object)?;
        let weight = (size as u64)
            .checked_add(1)
            .context("cache object size overflow")?;
        // In particular, never decode an oversized packed blob merely to reject
        // it. Opening a header is enough to apply the retention policy.
        let mut locked = self.lock()?;
        let id = key(object);
        let exists = self.payload(id).try_exists()?;
        if !exists && self.node_path(id).try_exists()? {
            locked.remove(id)?;
        }
        if weight > self.budget || self.entries == 0 {
            locked.commit()?;
            return Ok(());
        }
        if exists {
            locked.touch(id)?;
            locked.commit()?;
            return Ok(());
        }
        locked.begin()?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&[kind.raw() as u8])?;
        let (copied, copied_kind, _) = copy_from_odb(&odb, object, &mut file)?;
        ensure!(
            copied == size && copied_kind == kind,
            "Git object changed during cache store"
        );
        file.as_file().sync_all()?;
        locked.evict(weight, 1)?;
        file.persist(self.payload(id))?;
        locked.append(id, weight)?;
        locked.commit()?;
        self.measure(|m| m.stores += 1);
        Ok(())
    }
}
impl Locked<'_> {
    fn begin(&mut self) -> Result<()> {
        if !self.dirty {
            let marker = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(self.cache.root.join(".dirty"))?;
            marker.sync_all()?;
            File::open(&self.cache.root)?.sync_all()?;
            self.dirty = true;
        }
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        if self.dirty {
            self.cache
                .atomic_json(self.cache.root.join(".state"), &self.index)?;
            File::open(self.cache.root.join(".index"))?.sync_all()?;
            File::open(&self.cache.root)?.sync_all()?;
            fs::remove_file(self.cache.root.join(".dirty"))?;
            File::open(&self.cache.root)?.sync_all()?;
            self.dirty = false;
        }
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        self.begin()?;
        self.cache.measure(|m| m.recovery_scans += 1);
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.cache.root)? {
            check_transfer()?;
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("unrecognized remote cache entry")?;
            match name {
                "lock" | ".state" | ".dirty" => continue,
                ".index" => {
                    ensure!(entry.file_type()?.is_dir(), "invalid remote cache index");
                    fs::remove_dir_all(entry.path())?;
                    continue;
                }
                _ if name.starts_with(".tmp") => {
                    fs::remove_file(entry.path())?;
                    continue;
                }
                _ => {}
            }
            let object = Oid::from_str(name).ok().filter(|_| name.len() == 40);
            ensure!(
                object.is_some() && entry.file_type()?.is_file(),
                "unrecognized remote cache entry {name:?}"
            );
            let metadata = entry.metadata()?;
            self.cache.measure(|m| {
                m.recovery_entries += 1;
                m.metadata_reads += 1;
            });
            files.push((metadata.modified()?, key(object.unwrap()), metadata.len()));
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(self.cache.root.join(".index"))?;
        files.sort_unstable_by_key(|(modified, id, _)| (*modified, *id));
        self.index = Index::default();
        for (_, id, weight) in files {
            check_transfer()?;
            self.append(id, weight)?;
        }
        self.evict(0, 0)?;
        self.commit()
    }
    fn append(&mut self, id: Key, weight: u64) -> Result<()> {
        self.begin()?;
        self.cache.put_node(
            id,
            &Node {
                previous: self.index.last,
                next: None,
                weight,
            },
        )?;
        if let Some(last) = self.index.last {
            let mut node = self.cache.node(last)?;
            node.next = Some(id);
            self.cache.put_node(last, &node)?;
        } else {
            self.index.first = Some(id);
        }
        self.index.last = Some(id);
        self.index.count = self
            .index
            .count
            .checked_add(1)
            .context("cache count overflow")?;
        self.index.bytes = self
            .index
            .bytes
            .checked_add(weight)
            .context("cache size overflow")?;
        Ok(())
    }
    fn unlink(&mut self, id: Key) -> Result<Node> {
        self.begin()?;
        let node = self.cache.node(id)?;
        if let Some(previous) = node.previous {
            let mut neighbour = self.cache.node(previous)?;
            neighbour.next = node.next;
            self.cache.put_node(previous, &neighbour)?;
        } else {
            self.index.first = node.next;
        }
        if let Some(next) = node.next {
            let mut neighbour = self.cache.node(next)?;
            neighbour.previous = node.previous;
            self.cache.put_node(next, &neighbour)?;
        } else {
            self.index.last = node.previous;
        }
        fs::remove_file(self.cache.node_path(id))?;
        self.index.count = self
            .index
            .count
            .checked_sub(1)
            .context("cache count underflow")?;
        self.index.bytes = self
            .index
            .bytes
            .checked_sub(node.weight)
            .context("cache size underflow")?;
        Ok(node)
    }
    fn touch(&mut self, id: Key) -> Result<()> {
        // Recovery can reconstruct approximate recency from payload mtimes.
        // Normal eviction order uses links, independent of clock resolution.
        File::open(self.cache.payload(id))?
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        if self.index.last != Some(id) {
            let node = self.unlink(id)?;
            self.append(id, node.weight)?;
        }
        Ok(())
    }
    fn remove(&mut self, id: Key) -> Result<()> {
        self.unlink(id)?;
        match fs::remove_file(self.cache.payload(id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn evict(&mut self, bytes: u64, count: usize) -> Result<()> {
        while self.index.bytes > self.cache.budget.saturating_sub(bytes)
            || self.index.count > self.cache.entries.saturating_sub(count)
        {
            check_transfer()?;
            let first = self
                .index
                .first
                .context("cache index has no eviction victim")?;
            self.remove(first)?;
            self.cache.measure(|m| m.evictions += 1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn deleted_payload_restore_repairs_accounting_before_replacement() {
        for entries in [2, 4] {
            let temporary = tempfile::tempdir().unwrap();
            let cache = DiskCache::new(temporary.path().join("cache"), 1024, entries).unwrap();
            let original = Repository::init_bare(temporary.path().join("original")).unwrap();
            let active = Repository::init_bare(temporary.path().join("active")).unwrap();
            let first = original.blob(b"first").unwrap();
            let second = original.blob(b"second").unwrap();
            cache.store(&original, first).unwrap();
            cache.store(&original, second).unwrap();
            fs::remove_file(cache.payload(key(first))).unwrap();
            assert!(!cache.restore(&active, first).unwrap());
            assert_eq!(cache.lock().unwrap().index.count, 1);
            cache.store(&original, first).unwrap();
            assert_eq!(cache.lock().unwrap().index.count, 2);
            assert!(cache.restore(&active, first).unwrap());
            assert_eq!(active.find_blob(first).unwrap().content(), b"first");
        }
    }
    #[test]
    fn deleted_payload_direct_store_and_eviction_preserve_valid_links() {
        for entries in [2, 4] {
            let temporary = tempfile::tempdir().unwrap();
            let cache = DiskCache::new(temporary.path().join("cache"), 1024, entries).unwrap();
            let original = Repository::init_bare(temporary.path().join("original")).unwrap();
            let first = original.blob(b"first").unwrap();
            let second = original.blob(b"second").unwrap();
            let third = original.blob(b"third").unwrap();
            cache.store(&original, first).unwrap();
            cache.store(&original, second).unwrap();
            fs::remove_file(cache.payload(key(first))).unwrap();
            cache.store(&original, first).unwrap();
            {
                let locked = cache.lock().unwrap();
                assert_eq!(locked.index.count, 2);
                assert_eq!(locked.index.bytes, 13);
                let a = cache.node(locked.index.first.unwrap()).unwrap();
                assert_eq!(a.previous, None);
                let b = cache.node(a.next.unwrap()).unwrap();
                assert_eq!(b.next, None);
                assert_eq!(b.previous, locked.index.first);
            }
            // A missing different victim must not make insertion fail either.
            fs::remove_file(cache.payload(key(second))).unwrap();
            cache.store(&original, third).unwrap();
            assert!(cache.payload(key(first)).exists());
            assert!(cache.payload(key(third)).exists());
            assert!(cache.lock().unwrap().index.count <= entries);
        }
    }
    #[test]
    fn cache_process_worker() {
        let Some(root) = std::env::var_os("GIT_ON_CACHE_PROCESS_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let cache = DiskCache::new(root.join("cache"), 1024, 2).unwrap();
        let worker = std::env::var("GIT_ON_CACHE_PROCESS_WORKER").unwrap();
        if worker == "crash" {
            let mut locked = cache.lock().unwrap();
            let id = locked.index.first.unwrap();
            locked.unlink(id).unwrap();
            fs::write(root.join("cache/.tmpKilledWriter"), b"partial").unwrap();
            fs::write(root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::park();
            }
        }
        let repo = Repository::init_bare(root.join(format!("worker-{worker}"))).unwrap();
        for number in 0..30 {
            let id = repo.blob(format!("{worker}-{number}").as_bytes()).unwrap();
            cache.store(&repo, id).unwrap();
        }
    }
    #[test]
    fn independent_processes_and_killed_writer_leave_a_recoverable_bounded_cache() {
        use std::process::{Command, Stdio};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let cache = DiskCache::new(root.join("cache"), 1024, 2).unwrap();
        let original = Repository::init_bare(root.join("original")).unwrap();
        let first = original.blob(b"first").unwrap();
        let second = original.blob(b"second").unwrap();
        cache.store(&original, first).unwrap();
        cache.store(&original, second).unwrap();
        let spawn = |worker: &str| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "disk_cache::tests::cache_process_worker"])
                .env("GIT_ON_CACHE_PROCESS_ROOT", root)
                .env("GIT_ON_CACHE_PROCESS_WORKER", worker)
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut killed = spawn("crash");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("ready").exists() {
            assert!(
                Instant::now() < deadline && killed.try_wait().unwrap().is_none(),
                "cache writer did not reach controlled crash point"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        killed.kill().unwrap();
        assert!(!killed.wait().unwrap().success());
        let active = Repository::init_bare(root.join("active")).unwrap();
        assert!(cache.restore(&active, first).unwrap());
        assert!(cache.restore(&active, second).unwrap());
        assert!(!root.join("cache/.dirty").exists());
        assert!(!root.join("cache/.tmpKilledWriter").exists());
        let mut children: Vec<_> = (0..3).map(|worker| spawn(&worker.to_string())).collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let locked = cache.lock().unwrap();
        assert!(locked.index.count <= 2 && locked.index.bytes <= 1024);
        assert!(fs::read_dir(root.join("cache/.index")).unwrap().count() <= 2);
        assert_eq!(active.find_blob(first).unwrap().content(), b"first");
        assert_eq!(active.find_blob(second).unwrap().content(), b"second");
    }
    #[test]
    fn ordinary_metadata_work_is_independent_of_occupancy_and_run_length() {
        for capacity in [8, 64, 256] {
            let directory = tempfile::tempdir().unwrap();
            let cache =
                DiskCache::new(directory.path().join("cache"), 1_000_000, capacity).unwrap();
            let original = Repository::init_bare(directory.path().join("original")).unwrap();
            let active = Repository::init_bare(directory.path().join("active")).unwrap();
            let ids: Vec<_> = (0..capacity)
                .map(|i| original.blob(&i.to_le_bytes()).unwrap())
                .collect();
            for id in &ids {
                cache.store(&original, *id).unwrap();
            }
            for operations in [16, 128] {
                let before = cache.metrics();
                for i in 0..operations {
                    assert!(cache.restore(&active, ids[i % capacity]).unwrap());
                    cache.store(&original, ids[i % capacity]).unwrap();
                    let absent = Oid::from_bytes(&[255; 20]).unwrap();
                    assert!(!cache.restore(&active, absent).unwrap());
                }
                let after = cache.metrics();
                assert_eq!(after.recovery_scans, before.recovery_scans);
                assert_eq!(after.recovery_entries, before.recovery_entries);
                assert!(after.metadata_reads - before.metadata_reads <= 10 * operations as u64);
                assert!(after.metadata_writes - before.metadata_writes <= 8 * operations as u64);
                println!(
                    "cache scaling K={capacity} N={operations}: reads={} writes={} lock_ns={}",
                    after.metadata_reads - before.metadata_reads,
                    after.metadata_writes - before.metadata_writes,
                    after.lock_held_ns - before.lock_held_ns
                );
            }
        }
    }
    #[test]
    fn dirty_links_recover_once_and_eviction_preserves_recent_and_run_local_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let cache = DiskCache::new(root.clone(), 128, 2).unwrap();
        let original = Repository::init_bare(directory.path().join("original")).unwrap();
        let active = Repository::init_bare(directory.path().join("active")).unwrap();
        let first = original.blob(b"first").unwrap();
        let second = original.blob(b"second").unwrap();
        let third = original.blob(b"third").unwrap();
        cache.store(&original, first).unwrap();
        cache.store(&original, second).unwrap();
        {
            let mut locked = cache.lock().unwrap();
            // This is the durable state left by interruption between link
            // updates and header commit; a staged payload is never a valid hit.
            locked.unlink(key(first)).unwrap();
            fs::write(root.join(".tmpInterrupted"), b"partial").unwrap();
        }
        let before = cache.metrics();
        assert!(cache.restore(&active, first).unwrap());
        assert_eq!(cache.metrics().recovery_scans, before.recovery_scans + 1);
        assert!(!root.join(".tmpInterrupted").exists());
        cache.store(&original, third).unwrap();
        assert!(root.join(first.to_string()).exists());
        assert!(!root.join(second.to_string()).exists());
        assert!(cache.restore(&active, first).unwrap());
        cache.store(&original, second).unwrap();
        assert!(!root.join(third.to_string()).exists());
        assert_eq!(active.find_blob(first).unwrap().content(), b"first");
        assert_eq!(cache.metrics().recovery_scans, before.recovery_scans + 1);
    }
    #[test]
    fn corrupted_payload_is_removed_before_any_odb_publication() {
        let directory = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(directory.path().join("cache"), 1024, 8).unwrap();
        let original = Repository::init_bare(directory.path().join("original")).unwrap();
        let active = Repository::init_bare(directory.path().join("active")).unwrap();
        let id = original.blob(b"expected").unwrap();
        for bytes in [vec![], vec![255], [vec![3], b"modified".to_vec()].concat()] {
            cache.store(&original, id).unwrap();
            fs::write(cache.payload(key(id)), bytes).unwrap();
            assert!(!cache.restore(&active, id).unwrap());
            assert!(!active.odb().unwrap().exists(id));
            assert!(!cache.payload(key(id)).exists());
        }
        cache.store(&original, id).unwrap();
        assert!(cache.restore(&active, id).unwrap());
        assert_eq!(active.find_blob(id).unwrap().content(), b"expected");
    }
    fn total(root: &Path) -> u64 {
        fs::read_dir(root)
            .unwrap()
            .filter_map(|x| {
                let x = x.unwrap();
                (x.file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 40 && Oid::from_str(s).is_ok()))
                .then(|| x.metadata().unwrap().len())
            })
            .sum()
    }
    #[test]
    fn review_abandoned_staging_files_are_reclaimed_with_cache_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
        fs::write(root.join(".tmpAbandonedWrite"), vec![0; 128]).unwrap();
        let repo = Repository::init_bare(temporary.path().join("repository")).unwrap();
        let oid = repo.blob(b"demanded").unwrap();
        cache.store(&repo, oid).unwrap();
        assert!(!root.join(".tmpAbandonedWrite").exists());
        assert!(total(&root) <= 64);
    }
    #[test]
    fn concurrent_eviction_keeps_active_run_copies_and_bounds_bytes_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
        let original = Repository::init_bare(temporary.path().join("original")).unwrap();
        let active = Repository::init_bare(temporary.path().join("active")).unwrap();
        let oid = original.blob(b"active copy").unwrap();
        cache.store(&original, oid).unwrap();
        assert!(cache.restore(&active, oid).unwrap());
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let root = root.clone();
                let path = temporary.path().join(format!("worker-{worker}"));
                scope.spawn(move || {
                    let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
                    let repo = Repository::init_bare(path).unwrap();
                    for n in 0..30 {
                        let oid = repo.blob(&[worker, n]).unwrap();
                        cache.store(&repo, oid).unwrap();
                        let _lock = cache.lock().unwrap();
                        assert!(total(&root) <= 64);
                        assert!(fs::read_dir(root.join(".index")).unwrap().count() <= 2);
                    }
                });
            }
        });
        assert_eq!(active.find_blob(oid).unwrap().content(), b"active copy");
        let oversized = original.blob(&[0; 65]).unwrap();
        cache.store(&original, oversized).unwrap();
        assert!(!root.join(oversized.to_string()).exists());
    }
}
