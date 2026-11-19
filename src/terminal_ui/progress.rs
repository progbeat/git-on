//! Transfer telemetry. Terminal drawing remains on the preparation UI thread.
use std::{fs::File, os::unix::fs::FileExt, sync::Mutex};

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

/// Read captured stderr by offset: seeking a dup would move the child's shared
/// write offset and corrupt its diagnostics. Polling is bounded and best effort.
pub(crate) struct NativeProgress {
    previous: Option<Report>,
    offset: u64,
    lines: Lines,
}
impl NativeProgress {
    pub(crate) fn new() -> Self {
        Self {
            previous: REPORT.lock().unwrap().take(),
            offset: 0,
            lines: Lines::default(),
        }
    }
    pub(crate) fn poll(&mut self, file: &File) {
        let mut bytes = [0; 4096];
        for _ in 0..8 {
            let Ok(count) = file.read_at(&mut bytes, self.offset) else {
                break;
            };
            if count == 0 {
                break;
            }
            self.offset += count as u64;
            self.lines.feed(&bytes[..count]);
        }
    }
}

impl Drop for NativeProgress {
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

pub(crate) fn diagnostics(bytes: &[u8]) -> Vec<u8> {
    bytes
        .split_inclusive(|byte| *byte == b'\r' || *byte == b'\n')
        .filter(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let summary = line
                .strip_prefix(b"remote: Total ")
                .is_some_and(|rest| rest.first().is_some_and(u8::is_ascii_digit));
            parse(line).is_none() && !summary
        })
        .flatten()
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn native_progress_preserves_capture_offsets_and_bounds_partial_lines() {
        let file = tempfile::tempfile().unwrap();
        let mut writer = file.try_clone().unwrap();
        let first = b"remote: Compressing objects:  25% (1/4)\r";
        writer.write_all(first).unwrap();
        let mut progress = NativeProgress::new();
        progress.poll(&file);
        assert_eq!(current().unwrap().counts, Some((1, 4)));
        assert_eq!(writer.stream_position().unwrap(), first.len() as u64);
        let second = b"Receiving objects: 50% (2/4), 1.25 MiB | 640.00 KiB/s\r";
        writer.write_all(second).unwrap();
        progress.poll(&file);
        let report = current().unwrap();
        assert_eq!(report.label, "Receiving objects");
        assert_eq!(report.counts, Some((2, 4)));
        assert_eq!(report.detail, "1.25 MiB | 640.00 KiB/s");
        let expected = [first.as_slice(), second.as_slice()].concat();
        let mut actual = vec![0; expected.len()];
        file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, expected);
        progress.lines.feed(&vec![b'x'; 65536]);
        assert!(progress.lines.pending.len() <= 2048);
        progress.lines.feed(b"Receiving objects: 90% (9/10)\r");
        assert_eq!(current().unwrap().counts, Some((2, 4)));
        progress.lines.feed(b"Receiving objects: 75% (3/4)\r");
        assert_eq!(current().unwrap().counts, Some((3, 4)));
        assert!(parse(b"warning: 50% (1/2)").is_none());
        assert!(parse(b"Receiving objects: 200% (2/1)").is_none());
        assert_eq!(
            diagnostics(b"Receiving objects: 100% (4/4), done.\rfatal: broken pack\n"),
            b"fatal: broken pack\n"
        );
    }
}
