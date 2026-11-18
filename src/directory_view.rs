//! Partially persistent directory enumeration, independent of payload storage.
//!
//! Each record has one versioned next-link slot. Filling an empty slot costs
//! O(1); copying a full current record clears one full slot in the current
//! list. There is at most one current predecessor, plus its direct arena index,
//! so propagation is O(1) amortized per edit. Iteration is O(1) per row. Old
//! versions share records and never copy a directory-sized vector on mutation.
//!
//! Only modified records enter the reclamation queue. Versions register through
//! RAII, including cursors; retiring one never scans unmodified source entries.
use crate::backend::FileType;
use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) ino: u64,
    pub(crate) kind: FileType,
    pub(crate) name: Arc<OsStr>,
}
type Link = Option<Arc<Record>>;
#[derive(Default)]
struct Links {
    next: Link,
    changed: Option<(u64, Link)>,
}
struct Record {
    slot: usize,
    key: usize,
    entry: Entry,
    links: Mutex<Links>,
    history: Arc<Mutex<History>>,
}
impl Record {
    fn new(slot: usize, entry: Entry, next: Link, history: &Arc<Mutex<History>>) -> Arc<Self> {
        let key = lock(history).record_key();
        Arc::new(Self {
            slot,
            key,
            entry,
            links: Mutex::new(Links {
                next,
                changed: None,
            }),
            history: history.clone(),
        })
    }
    fn next(&self, version: u64) -> Link {
        let links = lock(&self.links);
        match &links.changed {
            Some((changed, next)) if version >= *changed => next.clone(),
            _ => links.next.clone(),
        }
    }
    fn detach(&mut self, pending: &mut Vec<Arc<Record>>) {
        let links = self
            .links
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        pending.extend(links.next.take());
        if let Some((_, next)) = links.changed.take() {
            pending.extend(next);
        }
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        lock(&self.history).release_record(self.key);
        // Arc::into_inner consumes the reference without recursively dropping
        // a last-owned list suffix, including when versions drop concurrently.
        let mut pending = Vec::new();
        self.detach(&mut pending);
        while let Some(next) = pending.pop() {
            if let Some(mut next) = Arc::into_inner(next) {
                next.detach(&mut pending);
                drop(next);
            }
        }
    }
}

struct Change {
    version: u64,
    record: Weak<Record>,
    previous: Option<usize>,
    next: Option<usize>,
}
struct OpenVersion {
    version: u64,
    previous: Option<usize>,
    next: Option<usize>,
}
#[derive(Default)]
struct History {
    current: u64,
    changes: HashMap<usize, Change>,
    first_change: Option<usize>,
    last_change: Option<usize>,
    views: HashMap<usize, OpenVersion>,
    first_view: Option<usize>,
    last_view: Option<usize>,
    next_record: usize,
    free_records: Vec<usize>,
    next_view: usize,
    free_views: Vec<usize>,
    #[cfg(test)]
    created: usize,
    #[cfg(test)]
    live: usize,
    #[cfg(test)]
    collected: usize,
}
impl History {
    fn record_key(&mut self) -> usize {
        #[cfg(test)]
        {
            self.created += 1;
            self.live += 1;
        }
        self.free_records.pop().unwrap_or_else(|| {
            let key = self.next_record;
            self.next_record += 1;
            key
        })
    }
    fn remove_change(&mut self, key: usize) -> Option<Weak<Record>> {
        let change = self.changes.remove(&key)?;
        if let Some(previous) = change.previous {
            self.changes
                .get_mut(&previous)
                .expect("previous change")
                .next = change.next;
        } else {
            self.first_change = change.next;
        }
        if let Some(next) = change.next {
            self.changes.get_mut(&next).expect("next change").previous = change.previous;
        } else {
            self.last_change = change.previous;
        }
        Some(change.record)
    }
    fn release_record(&mut self, key: usize) {
        self.remove_change(key);
        self.free_records.push(key);
        #[cfg(test)]
        {
            self.live -= 1;
        }
    }
    fn changed(&mut self, record: &Arc<Record>, version: u64) {
        if let Some(last) = self.last_change {
            self.changes.get_mut(&last).expect("last change").next = Some(record.key);
        } else {
            self.first_change = Some(record.key);
        }
        let previous = self.last_change.replace(record.key);
        assert!(self
            .changes
            .insert(
                record.key,
                Change {
                    version,
                    record: Arc::downgrade(record),
                    previous,
                    next: None,
                }
            )
            .is_none());
    }
    fn open(&mut self, version: u64) -> usize {
        let key = self.free_views.pop().unwrap_or_else(|| {
            let key = self.next_view;
            self.next_view += 1;
            key
        });
        if let Some(last) = self.last_view {
            let previous = self.views.get_mut(&last).expect("last directory version");
            debug_assert!(previous.version <= version);
            previous.next = Some(key);
        } else {
            self.first_view = Some(key);
        }
        self.views.insert(
            key,
            OpenVersion {
                version,
                previous: self.last_view.replace(key),
                next: None,
            },
        );
        key
    }
    fn close(&mut self, key: usize) {
        let view = self
            .views
            .remove(&key)
            .expect("registered directory version");
        if let Some(previous) = view.previous {
            self.views
                .get_mut(&previous)
                .expect("previous directory version")
                .next = view.next;
        } else {
            self.first_view = view.next;
        }
        if let Some(next) = view.next {
            self.views
                .get_mut(&next)
                .expect("next directory version")
                .previous = view.previous;
        } else {
            self.last_view = view.previous;
        }
        self.free_views.push(key);
    }
    fn ready(&mut self) -> Option<Weak<Record>> {
        let minimum = self
            .first_view
            .map(|key| self.views[&key].version)
            .unwrap_or(self.current);
        let key = self.first_change?;
        if self.changes[&key].version > minimum {
            return None;
        }
        #[cfg(test)]
        {
            self.collected += 1;
        }
        self.remove_change(key)
    }
}
fn collect(history: &Arc<Mutex<History>>) {
    loop {
        let Some(record) = lock(history).ready() else {
            return;
        };
        if let Some(record) = record.upgrade() {
            let previous = {
                let mut links = lock(&record.links);
                let (_, next) = links.changed.take().expect("queued next-link change");
                std::mem::replace(&mut links.next, next)
            };
            // Dropping links may release other queued records. Never hold the
            // history or record lock while their destructors run.
            drop(previous);
        }
    }
}

struct VersionInner {
    version: u64,
    len: usize,
    head: Link,
    key: usize,
    history: Arc<Mutex<History>>,
}
impl Drop for VersionInner {
    fn drop(&mut self) {
        lock(&self.history).close(self.key);
        collect(&self.history);
    }
}
#[derive(Clone)]
pub(crate) struct Version(Arc<VersionInner>);
impl Version {
    pub(crate) fn len(&self) -> usize {
        self.0.len
    }
    pub(crate) fn cursor(&self) -> Cursor {
        Cursor {
            version: self.clone(),
            next: self.0.head.clone(),
        }
    }
}
#[derive(Clone)]
pub(crate) struct Cursor {
    version: Version,
    next: Link,
}
impl Cursor {
    pub(crate) fn entry(&self) -> Option<&Entry> {
        self.next.as_ref().map(|node| &node.entry)
    }
    pub(crate) fn advance(&mut self) {
        self.next = self
            .next
            .as_ref()
            .and_then(|node| node.next(self.version.0.version));
    }
}

struct Current {
    record: Arc<Record>,
    previous: Option<usize>,
}
#[derive(Default)]
pub(crate) struct Rows {
    names: HashMap<Arc<OsStr>, usize>,
    slots: Vec<Option<Current>>,
    free: Vec<usize>,
    head: Link,
    tail: Option<usize>,
    version: u64,
    history: Arc<Mutex<History>>,
}
impl Rows {
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
    fn slot(&mut self) -> usize {
        self.free.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        })
    }
    pub(crate) fn insert(&mut self, ino: u64, kind: FileType, name: Arc<OsStr>) {
        debug_assert_eq!(self.version, 0);
        let slot = self.slot();
        let record = Record::new(
            slot,
            Entry {
                ino,
                kind,
                name: name.clone(),
            },
            None,
            &self.history,
        );
        if let Some(last) = self.tail {
            lock(
                &self.slots[last]
                    .as_ref()
                    .expect("last directory row")
                    .record
                    .links,
            )
            .next = Some(record.clone());
        } else {
            self.head = Some(record.clone());
        }
        self.slots[slot] = Some(Current {
            record,
            previous: self.tail.replace(slot),
        });
        assert!(
            self.names.insert(name, slot).is_none(),
            "duplicate directory name"
        );
    }
    pub(crate) fn snapshot(&self) -> Version {
        let key = lock(&self.history).open(self.version);
        Version(Arc::new(VersionInner {
            version: self.version,
            len: self.len(),
            head: self.head.clone(),
            key,
            history: self.history.clone(),
        }))
    }
    fn splice(&mut self, mut previous: Option<usize>, mut next: Link) {
        while let Some(slot) = previous {
            let current = self.slots[slot].as_ref().expect("current directory row");
            let record = current.record.clone();
            previous = current.previous;
            let mut links = lock(&record.links);
            if links.changed.is_none() {
                links.changed = Some((self.version, next));
                drop(links);
                lock(&self.history).changed(&record, self.version);
                return;
            }
            drop(links);
            let copied = Record::new(slot, record.entry.clone(), next, &self.history);
            self.slots[slot]
                .as_mut()
                .expect("copied directory row")
                .record = copied.clone();
            next = Some(copied);
        }
        self.head = next;
    }
    /// False only on generation wrap: the caller can freeze this family and
    /// rebuild its current state without invalidating any old version/cursor.
    pub(crate) fn update(&mut self, name: &OsStr, binding: Option<(u64, FileType)>) -> bool {
        let slot = self.names.get(name).copied();
        if slot.is_none() && binding.is_none() {
            return true;
        }
        if let (Some(slot), Some((ino, kind))) = (slot, binding) {
            let entry = &self.slots[slot]
                .as_ref()
                .expect("named directory row")
                .record
                .entry;
            if entry.ino == ino && entry.kind == kind {
                return true;
            }
        }
        let Some(version) = self.version.checked_add(1) else {
            return false;
        };
        self.version = version;
        lock(&self.history).current = version;
        match (slot, binding) {
            (Some(slot), None) => {
                let old = self.slots[slot].take().expect("removed directory row");
                let next = old.record.next(version);
                if let Some(next) = &next {
                    self.slots[next.slot]
                        .as_mut()
                        .expect("next directory row")
                        .previous = old.previous;
                } else {
                    self.tail = old.previous;
                }
                self.splice(old.previous, next);
                self.names.remove(name);
                self.free.push(slot);
            }
            (Some(slot), Some((ino, kind))) => {
                let current = self.slots[slot].as_ref().expect("replaced directory row");
                let previous = current.previous;
                let entry = Entry {
                    ino,
                    kind,
                    name: current.record.entry.name.clone(),
                };
                let record = Record::new(slot, entry, current.record.next(version), &self.history);
                self.slots[slot].as_mut().expect("replacement row").record = record.clone();
                self.splice(previous, Some(record));
            }
            (None, Some((ino, kind))) => {
                let slot = self.slot();
                let name: Arc<OsStr> = Arc::from(name);
                let record = Record::new(
                    slot,
                    Entry {
                        ino,
                        kind,
                        name: name.clone(),
                    },
                    None,
                    &self.history,
                );
                self.slots[slot] = Some(Current {
                    record: record.clone(),
                    previous: self.tail,
                });
                self.splice(self.tail, Some(record));
                self.tail = Some(slot);
                self.names.insert(name, slot);
            }
            (None, None) => unreachable!(),
        }
        collect(&self.history);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn values(version: &Version) -> BTreeMap<Vec<u8>, u64> {
        use std::os::unix::ffi::OsStrExt;
        let mut cursor = version.cursor();
        let mut found = BTreeMap::new();
        while let Some(entry) = cursor.entry() {
            assert!(found
                .insert(entry.name.as_bytes().to_vec(), entry.ino)
                .is_none());
            assert!(found.len() <= version.len(), "cyclic enumeration");
            cursor.advance();
        }
        assert_eq!(found.len(), version.len());
        found
    }
    fn fixture(count: usize) -> Rows {
        let mut rows = Rows::default();
        for index in 0..count {
            rows.insert(
                index as u64,
                FileType::RegularFile,
                Arc::from(OsStr::new(&format!("file-{index}"))),
            );
        }
        rows
    }
    #[test]
    fn snapshots_share_rows_and_copy_only_amortized_changes() {
        let mut rows = fixture(10_000);
        let pool = rows.history.clone();
        let mut versions = vec![rows.snapshot()];
        let before = lock(&pool).created;
        for index in 0..256 {
            assert!(rows.update(
                OsStr::new("file-9999"),
                Some((20_000 + index, FileType::RegularFile))
            ));
            versions.push(rows.snapshot());
        }
        let created = lock(&pool).created - before;
        assert!(
            created <= 3 * 256,
            "copied unrelated directory records: {created}"
        );
        eprintln!(
            "directory version evidence: initial=10000, edits=256, extra_records={created}, live={}",
            lock(&pool).live
        );
        // Exhaustive state comparisons use small randomized models below.
        // Sample wide versions around propagation boundaries without making
        // this regression itself perform D*q whole-directory traversals.
        for (index, version) in versions
            .iter()
            .enumerate()
            .filter(|(index, _)| [0, 128, 256].contains(index))
        {
            let expected = if index == 0 {
                9999
            } else {
                20_000 + index as u64 - 1
            };
            assert_eq!(values(version)[b"file-9999".as_slice()], expected);
        }
        drop(versions);
        assert_eq!(
            lock(&pool).live,
            rows.len(),
            "retired versions retained old records"
        );
        assert!(lock(&pool).changes.is_empty());
        drop(rows);
        assert_eq!(lock(&pool).live, 0);
    }
    #[test]
    fn randomized_versions_cursors_and_reused_slots_match_plain_maps() {
        let mut rows = fixture(20);
        let pool = rows.history.clone();
        let mut expected = values(&rows.snapshot());
        let mut versions = Vec::new();
        let mut state = 0x0003_1415_9265_u64;
        for step in 0..3000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let name = format!("file-{}", (state >> 16) % 40);
            if state & 3 == 0 {
                assert!(rows.update(OsStr::new(&name), None));
                expected.remove(name.as_bytes());
            } else {
                assert!(rows.update(OsStr::new(&name), Some((step, FileType::RegularFile))));
                expected.insert(name.as_bytes().to_vec(), step);
            }
            if state & 7 == 1 {
                let version = rows.snapshot();
                let mut cursor = version.cursor();
                if cursor.entry().is_some() {
                    cursor.advance();
                }
                versions.push((version, expected.clone(), cursor));
            }
            if versions.len() > 20 {
                versions.swap_remove((state as usize >> 8) % versions.len());
            }
            assert_eq!(values(&rows.snapshot()), expected);
            for (version, reference, saved) in &versions {
                assert_eq!(values(version), *reference);
                let mut replay = version.cursor();
                if replay.entry().is_some() {
                    replay.advance();
                }
                assert_eq!(saved.entry().map(|e| e.ino), replay.entry().map(|e| e.ino));
            }
        }
        assert!(lock(&pool).created <= 20 + 3 * 3000);
        drop(versions);
        assert_eq!(lock(&pool).live, rows.len());
        drop(rows);
        assert_eq!(lock(&pool).live, 0);
    }
    #[test]
    fn cursor_keeps_its_version_alive_after_owner_drop() {
        let mut rows = fixture(10);
        let pool = rows.history.clone();
        let version = rows.snapshot();
        let mut cursor = version.cursor();
        cursor.advance();
        drop(version);
        for index in 0..100 {
            rows.update(
                OsStr::new("file-1"),
                Some((index + 100, FileType::Directory)),
            );
        }
        drop(rows);
        assert_eq!(cursor.entry().unwrap().ino, 1);
        assert_eq!(cursor.entry().unwrap().kind, FileType::RegularFile);
        cursor.advance();
        assert_eq!(cursor.entry().unwrap().ino, 2);
        drop(cursor);
        assert_eq!(lock(&pool).live, 0);
    }
    #[test]
    fn immutable_cursors_and_retirement_are_safe_during_current_updates() {
        let mut rows = fixture(200);
        let pool = rows.history.clone();
        let old = rows.snapshot();
        let expected = values(&old);
        let start = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            let mut readers = Vec::new();
            for _ in 0..4 {
                let version = old.clone();
                let expected = &expected;
                let start = &start;
                readers.push(scope.spawn(move || {
                    start.wait();
                    for _ in 0..100 {
                        assert_eq!(values(&version), *expected);
                    }
                }));
            }
            drop(old);
            start.wait();
            for index in 0..512 {
                assert!(rows.update(
                    OsStr::new("file-199"),
                    Some((index + 1000, FileType::Directory))
                ));
            }
            for reader in readers {
                reader.join().unwrap();
            }
        });
        assert_eq!(lock(&pool).live, rows.len());
        drop(rows);
        assert_eq!(lock(&pool).live, 0);
    }
    #[test]
    fn generation_wrap_does_not_change_frozen_versions() {
        let mut rows = fixture(3);
        let old = rows.snapshot();
        let expected = values(&old);
        rows.version = u64::MAX;
        lock(&rows.history).current = u64::MAX;
        assert!(!rows.update(OsStr::new("file-1"), Some((500, FileType::Directory))));
        assert_eq!(values(&old), expected);
        assert_eq!(values(&rows.snapshot()), expected);
        drop(rows);
        assert_eq!(values(&old), expected);
    }
    #[test]
    fn long_lists_drop_iteratively_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let mut rows = fixture(10_000);
                let version = rows.snapshot();
                let pool = rows.history.clone();
                for index in 0..100 {
                    rows.update(
                        OsStr::new("file-9999"),
                        Some((index + 20_000, FileType::Directory)),
                    );
                }
                drop(rows);
                drop(version);
                assert_eq!(lock(&pool).live, 0);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
