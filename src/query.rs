//! Query evaluation over the resident index (see PROTOCOL.md).
//!
//! Matching runs in passes; a candidate keeps the first pass it matched:
//! 1. name: every term occurs in the folded name (skipped when a term
//!    contains "/");
//! 2. path: every term occurs in the folded path below its root;
//! 3. fuzzy, only while fewer than min(60, max) candidates were found and
//!    every term has 3+ characters and no "/": every term is an ordered
//!    subsequence of the name.
//!
//! Filters (kind, hidden, extensions, roots, scopes, depth, excludes) follow
//! fd's flags of the same name.
//!
//! The query is compiled once (folded terms, SIMD substring finders, filter
//! tables) and then streams over the entry array. Every match is seen; when
//! there are more than `max_results`, the best-ranked ones are kept.

use std::collections::HashMap;
use std::path::Path;

use memchr::memmem::Finder;
use serde::{Deserialize, Serialize};

use crate::fold::{fold, is_subsequence};
use crate::index::{build_excludes, Index, Kind, HIDDEN, VROOT};

/// Protocol version, reported in every response.
pub const PROTOCOL: u32 = 1;

#[derive(Deserialize, Clone, Debug)]
pub struct Request {
    /// "search" (default) or "status".
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub query: String,
    #[serde(default = "both_kinds")]
    pub kinds: Vec<String>,
    #[serde(default = "default_max")]
    pub max_results: usize,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
    /// Only entries below these indexed roots (absolute paths); all when empty.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Only entries below these directories, relative to each root (like
    /// passing them to fd as search paths).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Maximum depth below each scope, or below the root without scopes.
    #[serde(default)]
    pub max_depth: Option<u16>,
    /// Stat the first N results (dirs, then files) and return their mtimes.
    #[serde(default)]
    pub mtimes: usize,
    /// "best" (default): keep the highest-ranked candidates when over budget;
    /// "first": keep the first ones in index order, like fd.
    #[serde(default)]
    pub select: Option<String>,
}

fn both_kinds() -> Vec<String> {
    vec!["d".into(), "f".into()]
}
fn default_max() -> usize {
    2000
}

#[derive(Serialize, Default, Debug)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    pub dirs: Vec<String>,
    pub files: Vec<String>,
    pub mtimes: HashMap<String, f64>,
    pub matched_dirs: usize,
    pub matched_files: usize,
    pub engine_us: u64,
    /// Engine time split by phase: prepare (term hits), scan, select, emit.
    pub phases_us: [u64; 4],
    pub generation: u64,
    /// False when changes may be missing (no watcher, or inotify ran out of
    /// watches); clients should fall back to a live walk.
    pub fresh: bool,
}


struct Term {
    folded: Vec<u8>,
    finder: Finder<'static>,
    slash: bool,
}

pub struct Compiled {
    terms: Vec<Term>,
    /// Folded terms for ranking; not deduplicated, a repeated term counts twice.
    score_terms: Vec<Term>,
    browse: bool,
    any_slash: bool,
    fuzzy_ok: bool,
    exts: Vec<Vec<u8>>,
    /// An extension like "tar.gz" cannot use the interned last-suffix table.
    multi_dot_exts: bool,
    /// Per-root overrides for excludes the index was not built with.
    extra_excludes: Option<Vec<ignore::overrides::Override>>,
}

pub fn compile(index: &Index, req: &Request) -> Result<Compiled, String> {
    let mut seen = std::collections::HashSet::new();
    let raw: Vec<&str> = req.query.split_whitespace().filter(|t| seen.insert(*t)).collect();
    let terms: Vec<Term> = raw
        .iter()
        .map(|t| {
            let folded = fold(t);
            Term { finder: Finder::new(&folded).into_owned(), slash: t.contains('/'), folded }
        })
        .collect();
    let exts: Vec<Vec<u8>> = req
        .extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase().into_bytes())
        .filter(|e| !e.is_empty())
        .collect();
    let extra: Vec<String> =
        req.exclude.iter().filter(|p| !index.exclude_patterns.contains(p)).cloned().collect();
    Ok(Compiled {
        browse: terms.is_empty(),
        any_slash: terms.iter().any(|t| t.slash),
        fuzzy_ok: !raw.is_empty() && raw.iter().all(|t| t.chars().count() >= 3 && !t.contains('/')),
        score_terms: req
            .query
            .split_whitespace()
            .map(|t| {
                let folded = fold(t);
                Term { finder: Finder::new(&folded).into_owned(), slash: false, folded }
            })
            .collect(),
        terms,
        multi_dot_exts: exts.iter().any(|e| e.contains(&b'.')),
        exts,
        extra_excludes: if extra.is_empty() {
            None
        } else {
            Some(index.roots.iter().map(|r| build_excludes(&r.path, &extra)).collect::<Result<_, _>>()?)
        },
    })
}

fn has_extension(name: &[u8], exts: &[Vec<u8>]) -> bool {
    exts.iter().any(|ext| {
        name.len() > ext.len() + 1
            && name[name.len() - ext.len() - 1] == b'.'
            && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
    })
}

/// Fixed-size bitset over entry ids.
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn set(&mut self, i: u32) {
        self.0[i as usize / 64] |= 1 << (i % 64);
    }
    fn get(&self, i: u32) -> bool {
        self.0[i as usize / 64] & (1 << (i % 64)) != 0
    }
}

/// Splits `0..n` (or a byte blob) into roughly equal ranges, one per worker.
fn ranges(n: usize, parts: usize) -> Vec<(usize, usize)> {
    let step = n.div_ceil(parts.max(1)).max(1);
    (0..n).step_by(step).map(|a| (a, (a + step).min(n))).collect()
}

fn workers(n: usize) -> usize {
    // Thread start-up costs ~20 µs; below this size one thread wins.
    if n < 50_000 { 1 } else { crate::threads() }
}

/// Entries whose folded name contains the term, found by scanning the whole
/// folded blob at once: one SIMD substring search over contiguous memory
/// instead of one search per name. After a hit the scan skips to the end of
/// that name, so a name is reported once however often it contains the term.
fn name_hits(index: &Index, term: &Term) -> Bits {
    let blob = index.folded_blob();
    let mut cuts: Vec<usize> = ranges(blob.len(), workers(index.entries.len()))
        .into_iter()
        .map(|(a, _)| a + memchr::memchr(0, &blob[a..]).unwrap_or(blob.len() - a))
        .collect();
    cuts[0] = 0;
    cuts.push(blob.len());
    cuts.dedup();
    let found: Vec<Vec<u32>> = std::thread::scope(|s| {
        let handles: Vec<_> = cuts
            .windows(2)
            .map(|w| {
                let (start, end) = (w[0], w[1]);
                s.spawn(move || {
                    let mut ids = Vec::new();
                    let mut pos = start;
                    while let Some(i) = term.finder.find(&blob[pos..end]) {
                        let hit = pos + i;
                        if let Some(id) = index.owner_at(hit) {
                            ids.push(id);
                        }
                        pos = hit + memchr::memchr(0, &blob[hit..end]).unwrap_or(end - hit) + 1;
                        if pos >= end {
                            break;
                        }
                    }
                    ids
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut bits = Bits::new(index.entries.len());
    for id in found.into_iter().flatten() {
        bits.set(id);
    }
    bits
}

/// Per term, per directory: does the directory's path below the root contain
/// the term? Children inherit a parent's answer, so the whole table costs one
/// visit per directory.
fn dir_path_hits(index: &Index, term: &Term, names: &Bits, rel: &RelPaths) -> Bits {
    let n = index.entries.len();
    let mut known = Bits::new(n);
    let mut hit = Bits::new(n);
    let mut chain = Vec::new();
    for dir in index.live_dirs() {
        let mut cur = dir;
        // A root's own path is not "below the root": the chain stops there.
        while cur != VROOT && !index.is_root(cur) && !known.get(cur) {
            chain.push(cur);
            cur = index.entries[cur as usize].parent;
        }
        let mut inherited = cur != VROOT && !index.is_root(cur) && hit.get(cur);
        while let Some(d) = chain.pop() {
            let own = if term.slash { term.finder.find(rel.get(d)).is_some() } else { names.get(d) };
            inherited |= own;
            known.set(d);
            if inherited {
                hit.set(d);
            }
        }
    }
    hit
}

/// Folded relative paths of every directory, built only for terms that
/// contain "/" (they can span components, so names alone are not enough).
struct RelPaths {
    span: Vec<(u32, u32)>,
    buf: Vec<u8>,
}

impl RelPaths {
    fn empty() -> RelPaths {
        RelPaths { span: Vec::new(), buf: Vec::new() }
    }

    fn build(index: &Index) -> RelPaths {
        let mut rel = RelPaths { span: vec![(0, 0); index.entries.len()], buf: Vec::new() };
        let mut queue: std::collections::VecDeque<u32> = index.roots.iter().map(|r| r.id).collect();
        while let Some(dir) = queue.pop_front() {
            for kid in index.children(dir).filter(|&k| index.is_dir(k)) {
                let start = rel.buf.len();
                if !index.is_root(dir) {
                    let (a, b) = rel.span[dir as usize];
                    rel.buf.extend_from_within(a as usize..b as usize);
                    rel.buf.push(b'/');
                }
                rel.buf.extend_from_slice(index.folded(kid));
                rel.span[kid as usize] = (start as u32, rel.buf.len() as u32);
                queue.push_back(kid);
            }
        }
        rel
    }

    fn get(&self, dir: u32) -> &[u8] {
        let (a, b) = self.span[dir as usize];
        &self.buf[a as usize..b as usize]
    }
}

struct Scan<'a> {
    index: &'a Index,
    q: &'a Compiled,
    req: &'a Request,
    names: Vec<Bits>,
    dirs: Vec<Bits>,
    rel: RelPaths,
    ext_ok: Vec<bool>,
    /// Indexed roots this request covers.
    allowed_roots: Vec<bool>,
    scope_dirs: Vec<u32>,
    excluded_memo: HashMap<u32, bool>,
}

impl<'a> Scan<'a> {
    fn new(index: &'a Index, q: &'a Compiled, req: &'a Request) -> Result<Scan<'a>, String> {
        let mut allowed_roots = vec![req.roots.is_empty(); index.roots.len()];
        for want in &req.roots {
            let want = Path::new(want.trim_end_matches('/'));
            let i = index
                .roots
                .iter()
                .position(|r| r.path == want || std::fs::canonicalize(want).is_ok_and(|c| c == r.path))
                .ok_or_else(|| format!("root not indexed: {}", want.display()))?;
            allowed_roots[i] = true;
        }
        let scope_dirs = index
            .roots
            .iter()
            .enumerate()
            .filter(|(i, _)| allowed_roots[*i])
            .flat_map(|(_, r)| {
                req.scopes.iter().filter_map(move |scope| {
                    let mut cur = r.id;
                    for part in scope.split('/').filter(|p| !p.is_empty()) {
                        cur = index.child_by_name(cur, part.as_bytes())?;
                    }
                    (cur != r.id).then_some(cur)
                })
            })
            .collect();
        let rel = if q.any_slash { RelPaths::build(index) } else { RelPaths::empty() };
        let names: Vec<Bits> = q.terms.iter().map(|t| name_hits(index, t)).collect();
        let dirs = q.terms.iter().zip(&names).map(|(t, n)| dir_path_hits(index, t, n, &rel)).collect();
        let ext_ok = index
            .ext_names
            .iter()
            .enumerate()
            .map(|(i, e)| i != 0 && q.exts.iter().any(|x| x == e))
            .collect();
        Ok(Scan { index, q, req, names, dirs, rel, ext_ok, allowed_roots, scope_dirs, excluded_memo: HashMap::new() })
    }

    /// Filters that do not depend on the query text.
    fn passes_filters(&self, id: u32, kind: Kind) -> bool {
        let e = &self.index.entries[id as usize];
        if e.kind != kind || e.parent == VROOT || !self.allowed_roots[self.index.root_of(id)] {
            return false;
        }
        if !self.req.hidden && e.flags & HIDDEN != 0 {
            return false;
        }
        if kind == Kind::File && !self.q.exts.is_empty() {
            let ext = self.index.ext[id as usize];
            let ok = match self.ext_ok.get(ext as usize) {
                Some(&ok) if ext != u16::MAX && !self.q.multi_dot_exts => ok,
                _ => has_extension(self.index.name(id), &self.q.exts),
            };
            if !ok {
                return false;
            }
        }
        if !self.req.scopes.is_empty() {
            // Below one of the scope directories, within max_depth of it.
            let max = self.req.max_depth.unwrap_or(u16::MAX);
            let mut cur = id;
            let mut depth = 0u16;
            loop {
                let parent = self.index.entries[cur as usize].parent;
                depth += 1;
                if depth > max || parent == VROOT || self.index.is_root(parent) {
                    return false;
                }
                if self.scope_dirs.contains(&parent) {
                    break;
                }
                cur = parent;
            }
        } else if let Some(max) = self.req.max_depth {
            if e.depth > max {
                return false;
            }
        }
        true
    }

    /// Per-query excludes beyond the ones the index was built with: an entry
    /// is excluded when it or any ancestor matches, as fd prunes the walk.
    fn excluded(&mut self, id: u32) -> bool {
        let Some(overrides) = &self.q.extra_excludes else { return false };
        if id == VROOT || self.index.is_root(id) {
            return false;
        }
        let ov = &overrides[self.index.root_of(id)];
        if let Some(&v) = self.excluded_memo.get(&id) {
            return v;
        }
        let is_dir = self.index.is_dir(id);
        let hit = ov.matched(self.index.path(id), is_dir).is_ignore() || {
            let parent = self.index.entries[id as usize].parent;
            self.excluded(parent)
        };
        self.excluded_memo.insert(id, hit);
        hit
    }

    fn path_match(&self, id: u32, t: usize, tail: &mut Vec<u8>) -> bool {
        let index = self.index;
        if index.is_dir(id) {
            return self.dirs[t].get(id);
        }
        let parent = index.entries[id as usize].parent;
        if self.names[t].get(id) || self.dirs[t].get(parent) {
            return true;
        }
        let term = &self.q.terms[t];
        if term.slash && !index.is_root(parent) {
            // A "/" term can straddle the parent path and the name.
            let keep = term.folded.len().saturating_sub(1);
            let ppath = self.rel.get(parent);
            tail.clear();
            tail.extend_from_slice(&ppath[ppath.len().saturating_sub(keep)..]);
            tail.push(b'/');
            tail.extend_from_slice(index.folded(id));
            return term.finder.find(tail).is_some();
        }
        false
    }

    /// One worker's share of the entry array: (basename hits, path hits).
    fn scan_range(&self, kind: Kind, from: usize, to: usize) -> (Vec<u32>, Vec<u32>) {
        let mut basename = Vec::new();
        let mut path = Vec::new();
        let mut tail = Vec::new();
        let terms = self.q.terms.len();
        for id in from.max(1) as u32..to as u32 {
            if !self.passes_filters(id, kind) {
                continue;
            }
            if self.q.browse || (!self.q.any_slash && self.names.iter().all(|b| b.get(id))) {
                basename.push(id);
            } else if (0..terms).all(|t| self.path_match(id, t, &mut tail)) {
                path.push(id);
            }
        }
        (basename, path)
    }

    fn fuzzy_range(&self, kind: Kind, from: usize, to: usize) -> Vec<u32> {
        (from.max(1) as u32..to as u32)
            .filter(|&id| {
                self.passes_filters(id, kind) && {
                    let name = self.index.folded(id);
                    self.q.terms.iter().all(|t| is_subsequence(&t.folded, name))
                }
            })
            .collect()
    }

    fn run(&mut self, kind: Kind, phases: &mut [u64; 4]) -> (Vec<u32>, usize) {
        let t = std::time::Instant::now();
        let n = self.index.entries.len();
        let parts = ranges(n, workers(n));
        let this = &*self;
        let chunks: Vec<(Vec<u32>, Vec<u32>)> = std::thread::scope(|s| {
            let handles: Vec<_> =
                parts.iter().map(|&(a, b)| s.spawn(move || this.scan_range(kind, a, b))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let (mut basename, mut path) = (Vec::new(), Vec::new());
        for (b, p) in chunks {
            basename.extend(b);
            path.extend(p);
        }
        let mut found = basename;
        found.extend(path);
        if self.q.extra_excludes.is_some() {
            found.retain(|&id| !self.excluded(id));
        }
        let max = self.req.max_results;
        if self.q.fuzzy_ok && found.len() < max.min(60) {
            let already: std::collections::HashSet<u32> = found.iter().copied().collect();
            let this = &*self;
            let fuzzy: Vec<Vec<u32>> = std::thread::scope(|s| {
                let handles: Vec<_> =
                    parts.iter().map(|&(a, b)| s.spawn(move || this.fuzzy_range(kind, a, b))).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for id in fuzzy.into_iter().flatten() {
                if !already.contains(&id) && !self.excluded(id) {
                    found.push(id);
                }
            }
        }
        let matched = found.len();
        phases[1] += t.elapsed().as_micros() as u64;
        let t = std::time::Instant::now();
        if found.len() > max {
            if self.req.select.as_deref() == Some("first") {
                found.truncate(max);
            } else {
                self.keep_best(&mut found, max);
            }
        }
        phases[2] += t.elapsed().as_micros() as u64;
        (found, matched)
    }

    /// Keeps the `max` best-ranked candidates (PROTOCOL.md, *Ranking*):
    /// score, user files before hidden ones, shorter name, then path.
    fn keep_best(&mut self, found: &mut Vec<u32>, max: usize) {
        let index = self.index;
        let browse = self.q.browse;
        // Ranking order, packed into one integer: score (or depth when
        // browsing), user files before hidden ones, shorter names first.
        let pack = move |score: u32, hidden: u8, len: u32| -> u64 {
            let (a, b) = if browse { (hidden as u64, score as u64) } else { (score as u64, hidden as u64) };
            (a.min(0xffff_ffff) << 32) | (b.min(0xff) << 24) | (len as u64).min(0xff_ffff)
        };
        // First pass without building paths: exact when every term occurs in
        // the name, otherwise a lower bound (a term found only in the path
        // scores at least 4, the best path score).
        let this = &*self;
        let parts: Vec<Vec<(u64, u32, bool)>> = std::thread::scope(|s| {
            let handles: Vec<_> = found
                .chunks(found.len().div_ceil(workers(found.len() * 8)).max(1))
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .iter()
                            .map(|&id| {
                                let e = &index.entries[id as usize];
                                let len = utf16_len(index.name(id));
                                if browse {
                                    // Browsing orders by name, not length: leave
                                    // names to the tie-break below.
                                    return (pack(e.depth as u32, e.flags & HIDDEN, 0), id, true);
                                }
                                let name = index.folded(id);
                                let mut exact = true;
                                let score = this.q.score_terms.iter().fold(0u32, |total, t| {
                                    let s = score_term(name, t);
                                    if s < 0 {
                                        exact = false;
                                        total + 4
                                    } else {
                                        total + s as u32
                                    }
                                });
                                (pack(score, e.flags & HIDDEN, len), id, exact)
                            })
                            .collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut keyed: Vec<(u64, u32, bool)> = parts.into_iter().flatten().collect();
        // Refine bounds that could still make the cut until the cut is made
        // of exact keys only. Keys only grow, so the boundary only moves up.
        // Folded absolute path prefix of each root: path scores see full paths.
        let root_folded: Vec<Vec<u8>> = index
            .roots
            .iter()
            .map(|r| {
                let mut p = index.folded(r.id).to_vec();
                p.push(b'/');
                p
            })
            .collect();
        let mut path = Vec::new();
        loop {
            keyed.select_nth_unstable_by_key(max - 1, |k| k.0);
            let boundary = keyed[max - 1].0;
            let mut refined = false;
            for k in keyed.iter_mut().filter(|k| !k.2 && k.0 <= boundary) {
                let e = &index.entries[k.1 as usize];
                path.clear();
                path.extend_from_slice(&root_folded[index.root_of(k.1)]);
                index.folded_rel_path(k.1, &mut path);
                let score = score_item(index.folded(k.1), &path, &self.q.score_terms);
                k.0 = pack(score, e.flags & HIDDEN, (k.0 & 0xff_ffff) as u32);
                k.2 = true;
                refined = true;
            }
            if !refined {
                break;
            }
        }
        let boundary = keyed[max - 1].0;
        let mut keep: Vec<u32> = keyed.iter().filter(|k| k.0 < boundary).map(|k| k.1).collect();
        // Entries tied with the boundary are decided by the protocol's last
        // keys: by path for queries, by folded name and then path when
        // browsing. The kept set never depends on crawl order.
        let mut tied: Vec<u32> = keyed.iter().filter(|k| k.0 == boundary).map(|k| k.1).collect();
        let utf16_path = |id: u32| -> Vec<u16> { index.path(id).to_string_lossy().encode_utf16().collect() };
        if browse {
            tied.sort_unstable_by(|&a, &b| {
                index.folded(a).cmp(index.folded(b)).then_with(|| utf16_path(a).cmp(&utf16_path(b)))
            });
        } else {
            let mut paths: Vec<(Vec<u16>, u32)> = tied.iter().map(|&id| (utf16_path(id), id)).collect();
            paths.sort_unstable();
            tied = paths.into_iter().map(|p| p.1).collect();
        }
        keep.extend(tied.into_iter().take(max - keep.len()));
        keep.sort_unstable();
        *found = keep;
    }
}

/// Length of a UTF-8 name in UTF-16 code units, the unit the protocol ranks by.
fn utf16_len(name: &[u8]) -> u32 {
    name.iter().map(|&b| match b {
        0x80..=0xbf => 0, // continuation byte
        0xf0..=0xff => 2, // 4-byte sequence: a surrogate pair
        _ => 1,
    }).sum()
}

fn score_term(name: &[u8], term: &Term) -> i32 {
    if name == term.folded.as_slice() {
        return 0;
    }
    match term.finder.find(name) {
        Some(0) => 1,
        Some(idx) if matches!(name[idx - 1], b' ' | b'-' | b'_' | b'.' | b'/') => 2,
        Some(_) => 3,
        None => -1,
    }
}

/// Score of an entry (lower is better), as PROTOCOL.md defines it. An entry
/// that misses a term cannot reach this point; if one did, it would rank last.
fn score_item(name: &[u8], path: &[u8], terms: &[Term]) -> u32 {
    let mut total = 0u32;
    let mut probe = Vec::new();
    for t in terms {
        let term = t.folded.as_slice();
        let mut s = score_term(name, t);
        if s < 0 {
            let boundary = [b"/".as_slice(), b"/.", b" ", b"_", b"-"].iter().any(|prefix| {
                probe.clear();
                probe.extend_from_slice(prefix);
                probe.extend_from_slice(term);
                memchr::memmem::find(path, &probe).is_some()
            });
            s = if boundary {
                4
            } else if t.finder.find(path).is_some() {
                5
            } else if is_subsequence(term, name) {
                6
            } else if is_subsequence(term, path) {
                7
            } else {
                return u32::MAX;
            };
        }
        total += s as u32;
    }
    total
}

fn mtime_ms(path: &Path) -> Option<f64> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(meta.mtime() as f64 * 1000.0)
}

pub fn search(index: &Index, req: &Request) -> Result<Response, String> {
    let start = std::time::Instant::now();
    let compiled = compile(index, req)?;
    let mut resp = Response { v: PROTOCOL, id: req.id, generation: index.generation, ..Default::default() };
    if req.max_results == 0 {
        return Ok(resp);
    }
    let t = std::time::Instant::now();
    let mut scan = Scan::new(index, &compiled, req)?;
    resp.phases_us[0] = t.elapsed().as_micros() as u64;
    let mut stat_budget = req.mtimes;
    for kind in [Kind::Dir, Kind::File] {
        let flag = if kind == Kind::Dir { "d" } else { "f" };
        if !req.kinds.iter().any(|k| k == flag) {
            continue;
        }
        let (ids, matched) = scan.run(kind, &mut resp.phases_us);
        let t = std::time::Instant::now();
        let out = if kind == Kind::Dir { &mut resp.dirs } else { &mut resp.files };
        for id in ids {
            let path = index.path(id);
            // JSON strings cannot carry these bytes, and a lossy copy would
            // name a file that does not exist.
            let Some(text) = path.to_str().map(str::to_owned) else { continue };
            if stat_budget > 0 {
                stat_budget -= 1;
                if let Some(ms) = mtime_ms(&path) {
                    resp.mtimes.insert(text.clone(), ms);
                }
            }
            out.push(text);
        }
        if kind == Kind::Dir {
            resp.matched_dirs = matched;
        } else {
            resp.matched_files = matched;
        }
        resp.phases_us[3] += t.elapsed().as_micros() as u64;
    }
    resp.engine_us = start.elapsed().as_micros() as u64;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::tempdir;
    use crate::index::{build, NoHook};

    fn files(dir: &tempdir::Dir, names: &[&[u8]]) -> Index {
        use std::os::unix::ffi::OsStrExt;
        for name in names {
            let path = dir.0.join(std::ffi::OsStr::from_bytes(name));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        build(&[dir.0.clone()], &[], 2, &NoHook).unwrap()
    }

    fn kept(index: &Index, query: &str, max: usize) -> Vec<String> {
        let req: Request =
            serde_json::from_value(serde_json::json!({ "query": query, "kinds": ["f"], "max_results": max })).unwrap();
        let root = index.roots[0].path.to_str().unwrap().to_owned() + "/";
        let mut out: Vec<String> =
            search(index, &req).unwrap().files.iter().map(|p| p.strip_prefix(&root).unwrap().to_owned()).collect();
        out.sort();
        out
    }

    #[test]
    fn utf16_lengths() {
        assert_eq!(utf16_len(b"report"), 6);
        assert_eq!(utf16_len("canción".as_bytes()), 7);
        assert_eq!(utf16_len("a😀".as_bytes()), 3);
    }

    #[test]
    fn keeps_the_best_scores() {
        let dir = tempdir::Dir::new();
        // Scores 0 to 4: equal, prefix, word start, substring, path component.
        let ranked = ["x/report", "x/report-old.txt", "x/old-report.txt", "x/myreport.txt", "reports/notes.txt"];
        let index = files(&dir, &ranked.map(str::as_bytes));
        for max in 1..=ranked.len() {
            let mut want: Vec<String> = ranked[..max].iter().map(|s| s.to_string()).collect();
            want.sort();
            assert_eq!(kept(&index, "report", max), want, "max_results {max}");
        }
    }

    #[test]
    fn equal_scores_keep_the_shorter_name_in_utf16() {
        let dir = tempdir::Dir::new();
        // Both are prefix matches. "report-ññ" is longer in bytes (11 vs 10)
        // but shorter in UTF-16 code units (9 vs 10).
        let index = files(&dir, &["report-ññ".as_bytes(), b"report-abc"]);
        assert_eq!(kept(&index, "report", 1), ["report-ññ"]);
    }

    #[test]
    fn non_utf8_paths_are_left_out() {
        let dir = tempdir::Dir::new();
        let index = files(&dir, &[b"report-\xff", b"report-ok"]);
        let req: Request = serde_json::from_value(serde_json::json!({ "query": "report", "kinds": ["f"] })).unwrap();
        let resp = search(&index, &req).unwrap();
        assert_eq!(resp.files.len(), 1);
        assert!(resp.files[0].ends_with("/report-ok"));
        assert_eq!(resp.matched_files, 2);
    }
}
