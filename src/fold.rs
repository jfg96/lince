//! Name folding used for matching.
//!
//! Every name is folded once, when it enters the index, so queries compare
//! plain bytes. Folding lowercases and strips combining accents after NFD
//! normalisation (PROTOCOL.md, *Matching*), so "cancion" matches "Canción" and the decomposed (NFD) spelling of it alike.
//!
//! Most names are ASCII. Those take a byte-table fast path and never touch
//! the Unicode tables.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// Appends the folded form of `name` to `out`.
pub fn fold_into(name: &[u8], out: &mut Vec<u8>) {
    if name.is_ascii() {
        out.extend(name.iter().map(u8::to_ascii_lowercase));
        return;
    }
    let text = String::from_utf8_lossy(name);
    let mut buf = [0u8; 4];
    for ch in text.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        for lower in ch.to_lowercase() {
            out.extend_from_slice(lower.encode_utf8(&mut buf).as_bytes());
        }
    }
}

pub fn fold(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    fold_into(text.as_bytes(), &mut out);
    out
}

/// Ordered subsequence test, the fuzzy fallback ("omfind" ⊂ "omarchy-find").
/// ASCII needles compare bytes (an ASCII byte never equals part of a
/// multi-byte character); others compare whole characters.
pub fn is_subsequence(needle: &[u8], hay: &[u8]) -> bool {
    if needle.is_ascii() {
        // Each ASCII character needs at least one byte in the haystack.
        // Keep this byte bound on the ASCII path: the Unicode fallback
        // compares lossy-decoded characters, not the original byte lengths.
        if needle.len() > hay.len() {
            return false;
        }
        let mut it = hay.iter();
        return needle.iter().all(|n| it.any(|h| h == n));
    }
    let hay = String::from_utf8_lossy(hay);
    let mut it = hay.chars();
    String::from_utf8_lossy(needle).chars().all(|n| it.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_accents() {
        assert_eq!(fold("Canción"), b"cancion");
        assert_eq!(fold("ESPAÑA"), b"espana");
        assert_eq!(fold("Cancio\u{301}n"), b"cancion");
        assert_eq!(fold("Música/Tema.MP3"), b"musica/tema.mp3");
        assert_eq!(fold("日本語"), "日本語".as_bytes());
    }

    #[test]
    fn subsequence() {
        assert!(is_subsequence(b"omfind", b"omarchy-find"));
        assert!(!is_subsequence(b"omfindx", b"omarchy-find"));
        assert!(is_subsequence(b"", b"x"));
    }

    #[test]
    fn subsequence_matches_character_oracle() {
        fn oracle(needle: &[u8], hay: &[u8]) -> bool {
            let hay = String::from_utf8_lossy(hay);
            let mut it = hay.chars();
            String::from_utf8_lossy(needle).chars().all(|n| it.any(|h| h == n))
        }
        // Exhaust the small ASCII space, including repeated characters,
        // NULs and empty strings; then exercise Unicode and invalid bytes.
        let mut strings = vec![Vec::new()];
        for len in 1..=5 {
            for mut bits in 0..3usize.pow(len) {
                let mut s = Vec::new();
                for _ in 0..len {
                    s.push([b'a', b'b', 0][bits % 3]);
                    bits /= 3;
                }
                strings.push(s);
            }
        }
        strings.extend(["é日本😀".as_bytes().to_vec(), "日本".as_bytes().to_vec(), vec![0xff], vec![0xff, b'a']]);
        for needle in &strings {
            for hay in &strings {
                assert_eq!(is_subsequence(needle, hay), oracle(needle, hay), "{needle:?} in {hay:?}");
            }
        }
    }
}
