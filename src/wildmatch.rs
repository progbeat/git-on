//! Stack-free byte-pattern matching for Git's pathname wildcards.
//!
//! A row of reachable text offsets is advanced for each atom. Stars add a
//! left-to-right closure, so time is O(pattern atoms * text bytes) and scratch
//! memory is O(text bytes), independent of wildcard nesting depth.
use gix_glob::wildmatch::Mode;

#[derive(Debug)]
enum Atom {
    Byte(u8),
    Any,
    Class(Box<[bool; 256]>),
    Star { slash: bool },
    // Git's boundary **/ can also match zero complete directory components.
    Directories,
    Never,
}

#[derive(Debug)]
pub(crate) struct Pattern(Vec<Atom>);

impl Pattern {
    pub(crate) fn new(pattern: &[u8]) -> Self {
        let mut atoms = Vec::new();
        let mut i = 0;
        while i < pattern.len() {
            match pattern[i] {
                b'\\' => {
                    i += 1;
                    atoms.push(pattern.get(i).copied().map_or(Atom::Never, Atom::Byte));
                    i += 1;
                }
                b'?' => {
                    atoms.push(Atom::Any);
                    i += 1;
                }
                b'*' => {
                    let start = i;
                    while pattern.get(i) == Some(&b'*') {
                        i += 1;
                    }
                    let boundary = start == 0 || pattern[start - 1] == b'/';
                    let following_boundary = i == pattern.len()
                        || pattern.get(i) == Some(&b'/')
                        || pattern.get(i..i + 2) == Some(b"\\/");
                    let slash = i - start >= 2 && boundary && following_boundary;
                    if slash && pattern.get(i) == Some(&b'/') {
                        atoms.push(Atom::Directories);
                        i += 1;
                    } else {
                        atoms.push(Atom::Star { slash });
                    }
                }
                b'[' => {
                    let Some(end) = class_end(pattern, i) else {
                        atoms.push(Atom::Never);
                        break;
                    };
                    let mut allowed = Box::new([false; 256]);
                    for byte in 1..=255u8 {
                        allowed[usize::from(byte)] = gix_glob::wildmatch(
                            pattern[i..end].into(),
                            [byte].as_slice().into(),
                            Mode::NO_MATCH_SLASH_LITERAL,
                        );
                    }
                    atoms.push(Atom::Class(allowed));
                    i = end;
                }
                byte => {
                    atoms.push(Atom::Byte(byte));
                    i += 1;
                }
            }
        }
        Self(atoms)
    }

    pub(crate) fn matches(&self, text: &[u8]) -> bool {
        let mut row = vec![false; text.len() + 1];
        let mut next = row.clone();
        row[0] = true;
        for atom in &self.0 {
            next.fill(false);
            match atom {
                Atom::Star { slash } => {
                    for i in 0..=text.len() {
                        next[i] =
                            row[i] || (i > 0 && next[i - 1] && (*slash || text[i - 1] != b'/'));
                    }
                }
                Atom::Directories => {
                    let mut reachable = false;
                    for i in 0..=text.len() {
                        next[i] = row[i] || (reachable && i > 0 && text[i - 1] == b'/');
                        reachable |= row[i];
                    }
                }
                Atom::Never => return false,
                _ => {
                    for (i, &byte) in text.iter().enumerate() {
                        next[i + 1] = row[i]
                            && match atom {
                                Atom::Byte(expected) => byte == *expected,
                                Atom::Any => byte != b'/',
                                Atom::Class(allowed) => allowed[usize::from(byte)],
                                _ => unreachable!(),
                            };
                    }
                }
            }
            std::mem::swap(&mut row, &mut next);
        }
        row[text.len()]
    }
}

// Find the outer class terminator, respecting initial ], escapes, ranges and
// POSIX class syntax. Matching the isolated atom is delegated to gix-glob;
// it has no wildcard recursion and preserves Git's byte-class semantics.
fn class_end(pattern: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if matches!(pattern.get(i), Some(b'!' | b'^')) {
        i += 1;
    }
    let mut previous = false;
    loop {
        let byte = *pattern.get(i)?;
        match byte {
            b'\\' => {
                i += 1;
                previous = *pattern.get(i)? != 0;
                i += 1;
            }
            b'-' if previous && pattern.get(i + 1).is_some_and(|&b| b != b']') => {
                i += 1;
                if pattern[i] == b'\\' {
                    i += 1;
                }
                pattern.get(i)?;
                i += 1;
                previous = false;
            }
            b'[' if pattern.get(i + 1) == Some(&b':') => {
                let close = i + 2 + pattern.get(i + 2..)?.iter().position(|&b| b == b']')?;
                if close >= i + 3 && pattern[close - 1] == b':' {
                    i = close + 1;
                    previous = false;
                } else {
                    // Without :], the opening [ is just a class member.
                    previous = true;
                    i += 1;
                }
            }
            _ => {
                previous = byte != 0;
                i += 1;
            }
        }
        if pattern.get(i) == Some(&b']') {
            return Some(i + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deep_stars_have_no_semantic_limit() {
        for n in [63, 64, 65, 100, 1000] {
            let pattern = Pattern::new("*a".repeat(n).as_bytes());
            assert!(pattern.matches("a".repeat(n).as_bytes()));
            assert!(!pattern.matches("a".repeat(n - 1).as_bytes()));
        }
    }
    #[test]
    fn compare_shallow_patterns_to_gix() {
        let atoms: &[&[u8]] = &[
            b"a",
            b"b",
            b"/",
            b"*",
            b"**",
            b"***",
            b"?",
            b"[ab]",
            b"[!a]",
            b"[a-z]",
            b"[]a]",
            b"[[:digit:]]",
            b"[[:bogus:]]",
            b"\\*",
            b"\\/",
            b"[abc",
            b"\\",
            b"[a\\-z]",
            b"[a[]",
            b"[[]",
        ];
        let mut seed = 7u64;
        let mut rand = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize % n
        };
        for _ in 0..20000 {
            let mut pattern = Vec::new();
            for _ in 0..1 + rand(8) {
                pattern.extend_from_slice(atoms[rand(atoms.len())]);
            }
            let compiled = Pattern::new(&pattern);
            for _ in 0..12 {
                let text: Vec<_> = (0..rand(15)).map(|_| b"ab/1*[]\\"[rand(8)]).collect();
                assert_eq!(
                    compiled.matches(&text),
                    gix_glob::wildmatch(
                        pattern.as_slice().into(),
                        text.as_slice().into(),
                        Mode::NO_MATCH_SLASH_LITERAL
                    ),
                    "pattern={pattern:?} text={text:?}"
                );
            }
        }
    }
    #[test]
    fn exhaustive_star_slash_boundaries_match_gix() {
        let fragments: &[&[u8]] = &[b"a", b"/", b"*", b"**", b"**/", b"\\/", b"?"];
        let mut texts = vec![Vec::new()];
        for _ in 0..5 {
            let prior = texts.clone();
            for text in prior {
                for byte in b"a/" {
                    let mut text = text.clone();
                    text.push(*byte);
                    texts.push(text);
                }
            }
        }
        texts.sort();
        texts.dedup();
        for mut encoded in 0..fragments.len().pow(4) {
            let mut pattern = Vec::new();
            for _ in 0..4 {
                pattern.extend_from_slice(fragments[encoded % fragments.len()]);
                encoded /= fragments.len();
            }
            let compiled = Pattern::new(&pattern);
            for text in &texts {
                assert_eq!(
                    compiled.matches(text),
                    gix_glob::wildmatch(
                        pattern.as_slice().into(),
                        text.as_slice().into(),
                        Mode::NO_MATCH_SLASH_LITERAL
                    ),
                    "pattern={pattern:?}, text={text:?}"
                );
            }
        }
    }
}
