//! Opt-in inclusive diagnostic spans. Normal binaries do not compile this module.
//! Bounded operation names, no per-request records or user data are retained.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::Instant,
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub calls: u64,
    pub elapsed_ns: u64,
    pub bytes: u64,
}
static ENABLED: OnceLock<bool> = OnceLock::new();
static TOTALS: Mutex<BTreeMap<&'static str, Summary>> = Mutex::new(BTreeMap::new());
pub(crate) struct Span {
    start: Instant,
    name: &'static str,
    bytes: u64,
}
pub(crate) fn span(name: &'static str, bytes: u64) -> Option<Span> {
    ENABLED
        .get_or_init(|| std::env::var_os("GIT_ON_TEST_PROFILE_SPANS").is_some())
        .then(|| Span {
            start: Instant::now(),
            name,
            bytes,
        })
}
impl Drop for Span {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let mut totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let value = totals.entry(self.name).or_default();
        value.calls += 1;
        value.elapsed_ns = value.elapsed_ns.saturating_add(elapsed);
        value.bytes = value.bytes.saturating_add(self.bytes);
    }
}
pub(crate) fn summary() -> BTreeMap<&'static str, Summary> {
    TOTALS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
