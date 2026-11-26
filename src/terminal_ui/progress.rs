//! Transfer telemetry. Terminal drawing remains on the preparation UI thread.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Report {
    pub label: String,
    pub detail: String,
    pub counts: Option<(u64, u64)>,
}
static REPORT: Mutex<Option<Report>> = Mutex::new(None);

pub(super) fn current() -> Option<Report> {
    REPORT.lock().unwrap().clone()
}

pub(super) fn display(value: &str) -> String {
    let mut text = String::new();
    for character in value.chars().take(256) {
        if character.is_control() {
            text.extend(character.escape_default());
        } else {
            text.push(character);
        }
    }
    text
}

pub(crate) struct PackProgress {
    previous: Option<Report>,
    lines: Lines,
    started: Instant,
    last_update: Option<Instant>,
}

impl PackProgress {
    pub(crate) fn new() -> Self {
        Self {
            previous: REPORT.lock().unwrap().take(),
            lines: Lines::default(),
            started: Instant::now(),
            last_update: None,
        }
    }

    pub(crate) fn remote(&mut self, bytes: &[u8]) {
        self.lines.feed(bytes);
    }

    pub(crate) fn received(&mut self, position: usize, total: usize, bytes: u64) {
        let now = Instant::now();
        if total == 0
            || self
                .last_update
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(100))
        {
            return;
        }
        self.last_update = Some(now);
        let kib = bytes as f64 / 1024.0;
        let seconds = now.duration_since(self.started).as_secs_f64().max(0.001);
        *REPORT.lock().unwrap() = Some(Report {
            label: "Receiving objects".into(),
            detail: format!("{:.2} MiB | {:.2} KiB/s", kib / 1024.0, kib / seconds),
            counts: Some((position as u64, total as u64)),
        });
    }
}

impl Drop for PackProgress {
    fn drop(&mut self) {
        *REPORT.lock().unwrap() = self.previous.take();
    }
}

#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    overflow: bool,
}
impl Lines {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                if !self.overflow {
                    if let Some(report) = parse(&self.pending) {
                        *REPORT.lock().unwrap() = Some(report);
                    }
                }
                self.pending.clear();
                self.overflow = false;
            } else if self.pending.len() < 2048 {
                self.pending.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }
}
fn parse(bytes: &[u8]) -> Option<Report> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let text = text.strip_prefix("remote: ").unwrap_or(text);
    let (label, rest) = text.split_once(':')?;
    if matches!(label, "fatal" | "error" | "warning" | "hint") {
        return None;
    }
    if label == "Enumerating objects" {
        let count: u64 = rest.trim().trim_end_matches(", done.").parse().ok()?;
        return Some(Report {
            label: label.into(),
            detail: format!("{count} objects"),
            counts: None,
        });
    }
    let (_, numbers) = rest.split_once('%')?;
    let (numbers, detail) = numbers.trim_start().strip_prefix('(')?.split_once(')')?;
    let (position, total) = numbers.split_once('/')?;
    let (position, total): (u64, u64) = (position.trim().parse().ok()?, total.trim().parse().ok()?);
    if total == 0 || position > total {
        return None;
    }
    Some(Report {
        label: display(label),
        detail: display(
            detail
                .trim_start_matches(',')
                .trim()
                .trim_end_matches("done.")
                .trim_end_matches(',')
                .trim(),
        ),
        counts: Some((position, total)),
    })
}

#[cfg(test)]
#[test]
fn sideband_progress_bounds_partial_lines_and_recovers_at_the_next_record() {
    let mut progress = PackProgress::new();
    progress.remote(b"remote: Compressing objects:  25% (1/");
    progress.remote(b"4)\r");
    assert_eq!(current().unwrap().counts, Some((1, 4)));
    progress.remote(&vec![b'x'; 65536]);
    progress.remote(b"Receiving objects: 90% (9/10)\r");
    assert_eq!(current().unwrap().counts, Some((1, 4)));
    progress.remote(b"Receiving objects: 50% (2/4), 1.25 MiB | 640.00 KiB/s\r");
    let report = current().unwrap();
    assert_eq!(report.counts, Some((2, 4)));
    assert_eq!(report.detail, "1.25 MiB | 640.00 KiB/s");
}
