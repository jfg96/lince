//! Resident filename index.
//!
//! Entries store a basename and a reference to their parent directory instead
//! of full paths: renaming or moving a directory touches one entry, never its
//! descendants, and full paths are rebuilt only for the handful of results
//! that are shown.
//!
//! Entry 0 is a virtual node whose children are the indexed roots; a root's
//! name is its absolute path. Paths "below the root", hidden flags, depth and
//! excludes are all relative to the entry's own root.
//!
//! Names live in two append-only byte blobs: the original bytes (to rebuild
//! paths) and the folded bytes used for matching, each folded name followed by
//! a NUL so a match can never straddle two names. Entry ids are stable until
//! `compact()`, which rewrites the arrays in parent-before-child order.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use ignore::overrides::{Override, OverrideBuilder};

use crate::fold::fold_into;

/// Virtual parent of every indexed root.
pub const VROOT: u32 = 0;
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Kind {
    Dead = 0,
    Dir = 1,
    File = 2,
}

/// Name or an ancestor below the root starts with '.' (fd's hidden rule).
pub const HIDDEN: u8 = 1;
/// The remaining flag bits hold the index of the entry's root.
pub const MAX_ROOTS: usize = 127;
/// Set in `name_len` when the name is its own folded form and is read from
/// the folded blob instead of being stored twice. Names are at most 255
/// bytes on Linux, a root's path at most 4096.
const SHARED: u16 = 0x8000;

pub struct Root {
    pub path: PathBuf,
    pub id: u32,
    pub excludes: Override,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub parent: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    name_off: u32,
    fold_off: u32,
    name_len: u16,
    fold_len: u16,
    pub kind: Kind,
    pub flags: u8,
    pub depth: u16,
}

pub struct Index {
    pub roots: Vec<Root>,
    pub entries: Vec<Entry>,
    names: Vec<u8>,
    folded: Vec<u8>,
    /// ctime of each directory when it was last read. A directory's ctime
    /// changes whenever an entry is created, removed or renamed in it, which
    /// makes it a cheap way to validate a persisted index.
    pub dir_stamp: IdMap<i64>,
    pub exclude_patterns: Vec<String>,
    pub live: usize,
    pub dead: usize,
    /// Bumped on every mutation so caches can tell they are stale.
    pub generation: u64,
    /// (offset in `folded`, entry id) for every folded name ever appended,
    /// sorted by offset because the blob only grows. Maps a hit found by
    /// scanning the whole blob back to its entry; names left behind by a
    /// rename no longer match their entry's fold_off and are skipped.
    pub owner_off: Vec<u32>,
    pub owner_id: Vec<u32>,
    /// Interned lowercase extension of every entry (0 = none), so category
    /// filters test a table instead of comparing suffixes per file.
    pub ext: Vec<u16>,
    pub ext_names: Vec<Vec<u8>>,
    ext_ids: HashMap<Vec<u8>, u16>,
    /// Child count per entry, and a name-hash → child map for directories
    /// with more than WIDE children (names are verified against the blob;
    /// a colliding name falls back to the sibling scan), so events in huge directories do not turn
    /// every lookup into a scan of the sibling list.
    kid_count: Vec<u32>,
    wide: IdMap<HashMap<u64, u32>>,
}

const WIDE: u32 = 64;

pub fn build_excludes(root: &Path, patterns: &[String]) -> Result<Override, String> {
    // fd turns every --exclude into a negated override glob rooted at the
    // search path; doing the same keeps the gitignore-style semantics
    // (".git" anywhere, ".local/share/Trash" anchored to the root).
    let mut builder = OverrideBuilder::new(root);
    for pattern in patterns {
        builder.add(&format!("!{pattern}")).map_err(|e| e.to_string())?;
    }
    builder.build().map_err(|e| e.to_string())
}

impl Index {
    /// Roots are canonicalised; a root inside another one is dropped (the
    /// outer root already covers it).
    pub fn new(roots: &[PathBuf], patterns: &[String]) -> Result<Index, String> {
        let mut paths = Vec::new();
        for r in roots {
            let p = std::fs::canonicalize(r).map_err(|e| format!("{}: {e}", r.display()))?;
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        let nested: Vec<PathBuf> =
            paths.iter().filter(|p| paths.iter().any(|q| q != *p && p.starts_with(q))).cloned().collect();
        paths.retain(|p| !nested.contains(p));
        if paths.is_empty() {
            return Err("no roots to index".into());
        }
        if paths.len() > MAX_ROOTS {
            return Err(format!("at most {MAX_ROOTS} roots"));
        }
        let mut index = Index {
            roots: Vec::new(),
            entries: Vec::new(),
            names: Vec::new(),
            folded: Vec::new(),
            dir_stamp: IdMap::default(),
            exclude_patterns: patterns.to_vec(),
            live: 0,
            dead: 0,
            generation: 0,
            owner_off: Vec::new(),
            owner_id: Vec::new(),
            ext: vec![0],
            ext_names: vec![Vec::new()],
            ext_ids: HashMap::new(),
            kid_count: vec![0],
            wide: IdMap::default(),
        };
        index.folded.push(0);
        index.entries.push(Entry {
            parent: NONE,
            first_child: NONE,
            next_sibling: NONE,
            name_off: 0,
            fold_off: 0,
            name_len: 0,
            fold_len: 0,
            kind: Kind::Dir,
            flags: 0,
            depth: 0,
        });
        for path in paths {
            let excludes = build_excludes(&path, patterns)?;
            let id = index.add_child(VROOT, path.as_os_str().as_bytes(), Kind::Dir);
            index.roots.push(Root { path, id, excludes });
        }
        Ok(index)
    }

    pub fn is_root(&self, id: u32) -> bool {
        self.entries[id as usize].parent == VROOT
    }

    /// Index into `roots` of the root an entry belongs to.
    pub fn root_of(&self, id: u32) -> usize {
        (self.entries[id as usize].flags >> 1) as usize
    }

    /// Root whose subtree contains `path` (longest match).
    pub fn root_for_path(&self, path: &Path) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, r)| path.starts_with(&r.path))
            .max_by_key(|(_, r)| r.path.as_os_str().len())
            .map(|(i, _)| i)
    }

    pub fn name(&self, id: u32) -> &[u8] {
        let e = &self.entries[id as usize];
        let blob = if e.name_len & SHARED != 0 { &self.folded } else { &self.names };
        &blob[e.name_off as usize..(e.name_off + (e.name_len & !SHARED) as u32) as usize]
    }

    pub fn folded(&self, id: u32) -> &[u8] {
        let e = &self.entries[id as usize];
        &self.folded[e.fold_off as usize..(e.fold_off + e.fold_len as u32) as usize]
    }

    pub fn folded_blob(&self) -> &[u8] {
        &self.folded
    }

    /// Entry owning a blob hit, if the name is still current. A scan's cursor
    /// remembers its last owner. Exponential search brackets the next hit in
    /// O(log gap) rather than searching the entire owner table for every hit.
    pub fn owner_at(&self, off: usize, cursor: &mut usize) -> Option<u32> {
        let offsets = &self.owner_off;
        let mut start = (*cursor).min(offsets.len());
        if start == offsets.len() || offsets[start] as usize > off {
            start = 0;
        }
        let mut step = 1;
        while start + step < offsets.len() && offsets[start + step] as usize <= off {
            step *= 2;
        }
        let end = (start + step + 1).min(offsets.len());
        let i = start + offsets[start..end].partition_point(|&o| o as usize <= off).checked_sub(1)?;
        *cursor = i;
        let id = self.owner_id[i];
        let e = &self.entries[id as usize];
        (e.fold_off == self.owner_off[i] && e.kind != Kind::Dead).then_some(id)
    }

    fn intern_ext(&mut self, name: &[u8]) -> u16 {
        // Same rule as fd -e: a suffix after a dot that is not the first byte.
        let Some(dot) = memchr::memrchr(b'.', name).filter(|&d| d > 0 && d + 1 < name.len()) else { return 0 };
        let ext = &name[dot + 1..];
        // Lowercase short extensions on the stack: this runs for every file
        // that is indexed or loaded.
        let mut buf = [0u8; 16];
        let lower = if ext.len() <= buf.len() {
            let buf = &mut buf[..ext.len()];
            buf.copy_from_slice(ext);
            buf.make_ascii_lowercase();
            &*buf
        } else {
            &ext.to_ascii_lowercase()[..]
        };
        if let Some(&id) = self.ext_ids.get(lower) {
            return id;
        }
        let ext = lower.to_vec();
        if self.ext_names.len() >= u16::MAX as usize {
            return u16::MAX; // table full: callers fall back to the name
        }
        let id = self.ext_names.len() as u16;
        self.ext_names.push(ext.clone());
        self.ext_ids.insert(ext, id);
        id
    }

    /// Appends a name to the blobs; returns (name_off, name_len, fold_off,
    /// fold_len). Most names in a home are already lowercase ASCII: those
    /// are stored once, in the folded blob, and marked SHARED.
    fn push_name(&mut self, id: u32, name: &[u8]) -> (u32, u16, u32, u16) {
        self.make_room(name.len());
        let fold_off = self.folded.len() as u32;
        fold_into(name, &mut self.folded);
        let fold_len = (self.folded.len() as u32 - fold_off).min(u16::MAX as u32) as u16;
        self.folded.truncate((fold_off + fold_len as u32) as usize);
        let (name_off, name_len) = if name.len() < SHARED as usize && self.folded[fold_off as usize..] == *name {
            (fold_off, name.len() as u16 | SHARED)
        } else {
            let name_off = self.names.len() as u32;
            self.names.extend_from_slice(name);
            (name_off, name.len().min(SHARED as usize - 1) as u16)
        };
        self.folded.push(0);
        self.owner_off.push(fold_off);
        self.owner_id.push(id);
        (name_off, name_len, fold_off, fold_len)
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.entries[id as usize].kind == Kind::Dir
    }

    pub fn children(&self, dir: u32) -> ChildIter<'_> {
        ChildIter { index: self, next: self.entries[dir as usize].first_child }
    }

    pub fn child_by_name(&self, dir: u32, name: &[u8]) -> Option<u32> {
        if let Some(map) = self.wide.get(&dir) {
            if let Some(&id) = map.get(&name_hash(name)) {
                if self.name(id) == name {
                    return Some(id);
                }
            } else {
                return None;
            }
        }
        self.children(dir).find(|&c| self.name(c) == name)
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != VROOT {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        let mut path = Vec::new();
        for (i, &c) in chain.iter().rev().enumerate() {
            if i > 0 {
                path.push(b'/');
            }
            path.extend_from_slice(self.name(c));
        }
        PathBuf::from(OsStr::from_bytes(&path))
    }

    /// Folded path relative to the entry's root ("projects/omarchy-find");
    /// empty for a root itself.
    pub fn folded_rel_path(&self, id: u32, out: &mut Vec<u8>) {
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != VROOT && !self.is_root(cur) {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        for (i, &c) in chain.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(self.folded(c));
        }
    }

    /// Appends the path relative to the entry's root ("Projects/omarchy-find");
    /// nothing for a root itself. Recursive, so callers can reuse `out`
    /// without a per-call chain.
    pub fn rel_path(&self, id: u32, out: &mut Vec<u8>) {
        if id == VROOT || self.is_root(id) {
            return;
        }
        let parent = self.entries[id as usize].parent;
        if parent != VROOT && !self.is_root(parent) {
            self.rel_path(parent, out);
            out.push(b'/');
        }
        out.extend_from_slice(self.name(id));
    }

    pub fn add_child(&mut self, parent: u32, name: &[u8], kind: Kind) -> u32 {
        let id = self.entries.len() as u32;
        let (name_off, name_len, fold_off, fold_len) = self.push_name(id, name);
        let ext = if kind == Kind::File { self.intern_ext(name) } else { 0 };
        self.ext.push(ext);
        let p = self.entries[parent as usize];
        let (flags, depth) = if parent == VROOT {
            // A root: not hidden, depth 0, root index = its position.
            ((self.roots.len() as u8) << 1, 0)
        } else {
            let hidden = name.first() == Some(&b'.') || p.flags & HIDDEN != 0;
            ((p.flags & !HIDDEN) | hidden as u8, p.depth.saturating_add(1))
        };
        self.entries.push(Entry {
            parent,
            first_child: NONE,
            next_sibling: NONE,
            name_off,
            fold_off,
            name_len,
            fold_len,
            kind,
            flags,
            depth,
        });
        self.kid_count.push(0);
        self.link(parent, id);
        self.live += 1;
        self.generation += 1;
        id
    }

    fn link(&mut self, parent: u32, id: u32) {
        self.entries[id as usize].parent = parent;
        self.entries[id as usize].next_sibling = self.entries[parent as usize].first_child;
        self.entries[parent as usize].first_child = id;
        self.kid_count[parent as usize] += 1;
        if self.wide.contains_key(&parent) {
            let h = name_hash(self.name(id));
            self.wide.get_mut(&parent).unwrap().entry(h).or_insert(id);
        } else if self.kid_count[parent as usize] > WIDE {
            let mut map = HashMap::with_capacity(WIDE as usize * 2);
            for c in self.children(parent) {
                map.entry(name_hash(self.name(c))).or_insert(c);
            }
            self.wide.insert(parent, map);
        }
    }

    fn unlink(&mut self, id: u32) {
        let parent = self.entries[id as usize].parent;
        self.kid_count[parent as usize] -= 1;
        if self.wide.contains_key(&parent) {
            let h = name_hash(self.name(id));
            let map = self.wide.get_mut(&parent).unwrap();
            if map.get(&h) == Some(&id) {
                map.remove(&h);
            }
        }
        let next = self.entries[id as usize].next_sibling;
        if self.entries[parent as usize].first_child == id {
            self.entries[parent as usize].first_child = next;
            return;
        }
        let mut cur = self.entries[parent as usize].first_child;
        while cur != NONE {
            if self.entries[cur as usize].next_sibling == id {
                self.entries[cur as usize].next_sibling = next;
                return;
            }
            cur = self.entries[cur as usize].next_sibling;
        }
    }

    /// Removes an entry and its subtree. Returns the removed directory ids so
    /// the watcher can forget them.
    pub fn remove(&mut self, id: u32) -> Vec<u32> {
        if id == VROOT || self.entries[id as usize].kind == Kind::Dead {
            return Vec::new();
        }
        self.unlink(id);
        let mut removed_dirs = Vec::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let e = self.entries[cur as usize];
            if e.kind == Kind::Dir {
                removed_dirs.push(cur);
                self.dir_stamp.remove(&cur);
                self.wide.remove(&cur);
                stack.extend(self.children(cur));
            }
            self.entries[cur as usize].kind = Kind::Dead;
            self.live -= 1;
            self.dead += 1;
        }
        self.generation += 1;
        removed_dirs
    }

    /// Renames and/or moves an entry. Descendants keep their records; only
    /// their derived hidden flag and depth are refreshed.
    pub fn rename(&mut self, id: u32, new_parent: u32, new_name: &[u8]) {
        self.unlink(id);
        let (name_off, name_len, fold_off, fold_len) = self.push_name(id, new_name);
        if self.entries[id as usize].kind == Kind::File {
            self.ext[id as usize] = self.intern_ext(new_name);
        }
        let e = &mut self.entries[id as usize];
        e.name_off = name_off;
        e.name_len = name_len;
        e.fold_off = fold_off;
        e.fold_len = fold_len;
        self.link(new_parent, id);
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let p = self.entries[self.entries[cur as usize].parent as usize];
            let hidden = self.name(cur).first() == Some(&b'.') || p.flags & HIDDEN != 0;
            let e = &mut self.entries[cur as usize];
            // A move may cross roots: the root index follows the new parent.
            e.flags = (p.flags & !HIDDEN) | hidden as u8;
            e.depth = p.depth.saturating_add(1);
            if e.kind == Kind::Dir {
                stack.extend(self.children(cur));
            }
        }
        self.generation += 1;
    }

    pub fn is_excluded(&self, path: &Path, is_dir: bool) -> bool {
        match self.root_for_path(path) {
            Some(r) => self.roots[r].excludes.matched(path, is_dir).is_ignore(),
            None => true,
        }
    }

    /// Rewrites the arrays without dead entries and leaked names. Returns the
    /// old→new id map (NONE for removed ids).
    pub fn compact(&mut self) -> Vec<u32> {
        let mut map = vec![NONE; self.entries.len()];
        let mut fresh = Index {
            roots: Vec::new(),
            entries: Vec::with_capacity(self.live + 1),
            names: Vec::with_capacity(self.names.len()),
            folded: Vec::with_capacity(self.folded.len()),
            dir_stamp: IdMap::with_capacity_and_hasher(self.dir_stamp.len(), Default::default()),
            exclude_patterns: self.exclude_patterns.clone(),
            live: 0,
            dead: 0,
            generation: self.generation + 1,
            owner_off: Vec::with_capacity(self.live + 1),
            owner_id: Vec::with_capacity(self.live + 1),
            ext: vec![0],
            ext_names: vec![Vec::new()],
            ext_ids: HashMap::new(),
            kid_count: vec![0],
            wide: IdMap::default(),
        };
        fresh.folded.push(0);
        fresh.entries.push(self.entries[0]);
        fresh.entries[0].first_child = NONE;
        map[0] = VROOT;
        // Roots first, in their original order, so root indexes are kept.
        let mut queue = std::collections::VecDeque::new();
        for r in &self.roots {
            let id = fresh.add_child(VROOT, self.name(r.id), Kind::Dir);
            map[r.id as usize] = id;
            fresh.roots.push(Root { path: r.path.clone(), id, excludes: r.excludes.clone() });
            queue.push_back(r.id);
        }
        // Breadth-first keeps parents before children and siblings together.
        while let Some(old) = queue.pop_front() {
            let mut kids: Vec<u32> = self.children(old).collect();
            kids.reverse();
            for kid in kids {
                let id = fresh.add_child(map[old as usize], self.name(kid), self.entries[kid as usize].kind);
                map[kid as usize] = id;
                if self.entries[kid as usize].kind == Kind::Dir {
                    queue.push_back(kid);
                }
            }
        }
        for (old, stamp) in &self.dir_stamp {
            if map[*old as usize] != NONE {
                fresh.dir_stamp.insert(map[*old as usize], *stamp);
            }
        }
        fresh.generation = self.generation + 1;
        fresh.shrink();
        *self = fresh;
        map
    }

    /// Drops the spare capacity left by Vec growth (up to half of each array
    /// right after a crawl).
    pub fn shrink(&mut self) {
        self.entries.shrink_to_fit();
        self.names.shrink_to_fit();
        self.folded.shrink_to_fit();
        self.owner_off.shrink_to_fit();
        self.owner_id.shrink_to_fit();
        self.ext.shrink_to_fit();
        self.kid_count.shrink_to_fit();
        self.dir_stamp.shrink_to_fit();
    }

    /// Makes room for `entries` more entries, `dirs` of them directories,
    /// whose names add up to `name_bytes`, so a load of known size does not
    /// grow the arrays an eighth at a time. Best effort: if a reservation
    /// fails, make_room grows the arrays as usual.
    pub fn reserve(&mut self, entries: usize, dirs: usize, name_bytes: usize, longest: usize) {
        let _ = self.entries.try_reserve_exact(entries);
        let _ = self.ext.try_reserve_exact(entries);
        let _ = self.kid_count.try_reserve_exact(entries);
        let _ = self.owner_off.try_reserve_exact(entries);
        let _ = self.owner_id.try_reserve_exact(entries);
        let _ = self.names.try_reserve_exact(name_bytes);
        // Folded names usually keep their length; make_room also wants
        // worst-case space for the name being added, plus each NUL.
        let _ = self.folded.try_reserve_exact(name_bytes.saturating_add(entries).saturating_add(longest * 3 + 1));
        let _ = self.dir_stamp.try_reserve(dirs);
    }

    /// Grows the arrays by an eighth instead of doubling them, so the first
    /// change after a shrink() does not double the resident index.
    fn make_room(&mut self, name_len: usize) {
        fn grow<T>(v: &mut Vec<T>, extra: usize) {
            if v.capacity() - v.len() < extra {
                v.reserve_exact((v.len() / 8).max(extra).max(1024));
            }
        }
        grow(&mut self.entries, 1);
        grow(&mut self.ext, 1);
        grow(&mut self.kid_count, 1);
        grow(&mut self.owner_off, 1);
        grow(&mut self.owner_id, 1);
        grow(&mut self.names, name_len);
        grow(&mut self.folded, name_len * 3 + 1);
    }

    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.owner_off.capacity() * 8
            + self.ext.capacity() * 2
            + self.names.capacity()
            + self.folded.capacity()
            + self.dir_stamp.capacity() * 16
            + self.kid_count.capacity() * 4
            + self.wide.values().map(|m| m.capacity() * 13).sum::<usize>()
    }

    /// Every live directory, roots included, the virtual node excluded.
    pub fn live_dirs(&self) -> impl Iterator<Item = u32> + '_ {
        (1..self.entries.len() as u32).filter(|&i| self.entries[i as usize].kind == Kind::Dir)
    }
}

/// Map keyed by entry id. Ids are assigned by lince, never chosen by whoever
/// names the files, so SipHash's flooding resistance buys nothing here while
/// its cost shows on every insert when millions of entries are loaded.
pub type IdMap<V> = HashMap<u32, V, BuildHasherDefault<IdHasher>>;

#[derive(Default)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0 ^ b as u64);
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }
    fn write_u64(&mut self, n: u64) {
        // Fibonacci hashing: distinct ids keep distinct low bits (the bucket)
        // and the multiply spreads them into the high bits (the tag).
        self.0 = n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// FNV-1a: fast for short names; collisions are tolerated by callers.
fn name_hash(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

pub struct ChildIter<'a> {
    index: &'a Index,
    next: u32,
}

impl Iterator for ChildIter<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let cur = self.next;
        self.next = self.index.entries[cur as usize].next_sibling;
        Some(cur)
    }
}

pub fn ctime_ns(path: &Path) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(meta.ctime() * 1_000_000_000 + meta.ctime_nsec())
}

/// The stamp to record for a directory listed now with ctime `stamp`. The
/// kernel ticks ctime coarsely, so a change in the same tick as the listing
/// leaves it equal; a ctime this recent is recorded off by one, so the next
/// refresh re-reads the directory instead of trusting it (git's racy index).
pub fn settled(stamp: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |d| d.as_nanos() as i64);
    if now.saturating_sub(stamp) < 1_000_000_000 { stamp - 1 } else { stamp }
}

/// Lists a directory: (name, kind) for directories and regular files only,
/// like fd --type d / --type f without --follow. Uses d_type, no stat.
pub fn list_dir(index_root_excludes: &Override, dir: &Path) -> Vec<(Vec<u8>, Kind)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            continue;
        };
        let name = entry.file_name();
        if index_root_excludes.matched(dir.join(&name), kind == Kind::Dir).is_ignore() {
            continue;
        }
        out.push((name.as_bytes().to_vec(), kind));
    }
    out
}

/// Hook run by crawl workers before a directory is read (used to add the
/// inotify watch first, so nothing created during the read is missed).
pub trait DirHook: Sync {
    fn before_read(&self, id: u32, path: &Path);
}

pub struct NoHook;
impl DirHook for NoHook {
    fn before_read(&self, _: u32, _: &Path) {}
}

struct Queue {
    work: Vec<(u32, PathBuf)>,
    active: usize,
}

/// Reads the given directories and everything below them in parallel.
/// Workers list directories outside the index lock and append one directory's
/// children per lock acquisition.
pub fn crawl(index: &mut Index, start: Vec<(u32, PathBuf)>, threads: usize, hook: &dyn DirHook) {
    let roots: Vec<(PathBuf, Override)> = index.roots.iter().map(|r| (r.path.clone(), r.excludes.clone())).collect();
    let excludes_for = |path: &Path| {
        roots.iter().filter(|(p, _)| path.starts_with(p)).max_by_key(|(p, _)| p.as_os_str().len()).map(|(_, o)| o)
    };
    let shared = Mutex::new(&mut *index);
    let queue = Mutex::new(Queue { work: start, active: 0 });
    let wake = Condvar::new();
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| loop {
                let (dir, path) = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if let Some(item) = q.work.pop() {
                            q.active += 1;
                            break item;
                        }
                        if q.active == 0 {
                            wake.notify_all();
                            return;
                        }
                        q = wake.wait(q).unwrap();
                    }
                };
                hook.before_read(dir, &path);
                let stamp = ctime_ns(&path);
                let listing = match excludes_for(&path) {
                    Some(ov) => list_dir(ov, &path),
                    None => Vec::new(),
                };
                let mut subdirs = Vec::new();
                {
                    let mut idx = shared.lock().unwrap();
                    if let Some(stamp) = stamp {
                        idx.dir_stamp.insert(dir, settled(stamp));
                    }
                    for (name, kind) in listing {
                        let id = idx.add_child(dir, &name, kind);
                        if kind == Kind::Dir {
                            subdirs.push((id, path.join(OsStr::from_bytes(&name))));
                        }
                    }
                }
                let mut q = queue.lock().unwrap();
                q.active -= 1;
                let had = !subdirs.is_empty();
                q.work.extend(subdirs);
                if had || q.active == 0 {
                    wake.notify_all();
                }
            });
        }
    });
}

pub fn build(roots: &[PathBuf], patterns: &[String], threads: usize, hook: &dyn DirHook) -> Result<Index, String> {
    let mut index = Index::new(roots, patterns)?;
    let start = index.roots.iter().map(|r| (r.id, r.path.clone())).collect();
    crawl(&mut index, start, threads, hook);
    index.shrink();
    Ok(index)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn fixture() -> (tempdir::Dir, Index) {
        let dir = tempdir::Dir::new();
        for d in ["a/b", "a/.hid/c", "node_modules/x", "Música"] {
            std::fs::create_dir_all(dir.0.join(d)).unwrap();
        }
        for f in ["a/b/file.txt", "a/.hid/c/secret.md", "node_modules/x/i.js", "Música/tema.mp3"] {
            std::fs::write(dir.0.join(f), b"").unwrap();
        }
        let idx = build(&[dir.0.clone()], &["node_modules".into()], 4, &NoHook).unwrap();
        (dir, idx)
    }

    pub mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let p = std::env::temp_dir()
                    .join(format!("lince-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn find(idx: &Index, rel: &str) -> Option<u32> {
        let mut cur = idx.roots[0].id;
        for part in rel.split('/') {
            cur = idx.child_by_name(cur, part.as_bytes())?;
        }
        Some(cur)
    }

    #[test]
    fn crawl_excludes_and_flags() {
        let (dir, idx) = fixture();
        assert!(find(&idx, "node_modules").is_none());
        let secret = find(&idx, "a/.hid/c/secret.md").unwrap();
        assert!(idx.entries[secret as usize].flags & HIDDEN != 0);
        let file = find(&idx, "a/b/file.txt").unwrap();
        assert_eq!(idx.entries[file as usize].flags & HIDDEN, 0);
        assert_eq!(idx.path(file), std::fs::canonicalize(&dir.0).unwrap().join("a/b/file.txt"));
        assert_eq!(idx.folded(find(&idx, "Música").unwrap()), b"musica");
        assert_eq!(idx.live, 9, "8 entries plus the root");
    }

    #[test]
    fn rename_moves_subtree_and_compact_preserves_paths() {
        let (_dir, mut idx) = fixture();
        let b = find(&idx, "a/b").unwrap();
        let hid = find(&idx, "a/.hid").unwrap();
        idx.rename(b, hid, b"moved");
        let file = find(&idx, "a/.hid/moved/file.txt").unwrap();
        assert!(idx.entries[file as usize].flags & HIDDEN != 0, "moving under a hidden dir hides descendants");
        assert_eq!(idx.entries[file as usize].depth, 4);
        idx.remove(find(&idx, "Música").unwrap());
        assert_eq!(idx.live, 7);
        let before = idx.path(file);
        let map = idx.compact();
        assert_eq!(idx.path(map[file as usize]), before);
        assert_eq!(idx.dead, 0);
        assert!(find(&idx, "Música").is_none());
    }

    #[test]
    fn names_already_folded_are_stored_once() {
        let (_dir, mut idx) = fixture();
        let file = find(&idx, "a/b/file.txt").unwrap();
        let musica = find(&idx, "Música").unwrap();
        let names_before = idx.names.len();
        assert_eq!(idx.name(file), b"file.txt");
        assert_eq!(idx.name(musica), "Música".as_bytes());
        // Only "Música", and the root's path if it has capitals, differ from
        // their folded form.
        let root = idx.roots[0].id;
        let root_bytes = if idx.name(root) == idx.folded(root) { 0 } else { idx.name(root).len() };
        assert_eq!(names_before, root_bytes + "Música".len());
        idx.rename(file, idx.entries[file as usize].parent, b"Report.TXT");
        assert_eq!(idx.name(file), b"Report.TXT");
        assert_eq!(idx.folded(file), b"report.txt");
        idx.rename(musica, idx.entries[musica as usize].parent, b"musica");
        assert_eq!(idx.name(musica), b"musica");
        assert_eq!(idx.names.len(), names_before + "Report.TXT".len());
        let map = idx.compact();
        assert_eq!(idx.name(map[file as usize]), b"Report.TXT");
        assert_eq!(idx.name(map[musica as usize]), b"musica");
        assert!(find(&idx, "a/b/Report.TXT").is_some());
    }

    #[test]
    fn blob_owner_cursor_handles_gaps_and_stale_names() {
        let (_dir, mut idx) = fixture();
        let file = find(&idx, "a/b/file.txt").unwrap();
        idx.rename(file, idx.entries[file as usize].parent, b"renamed.txt");
        idx.remove(find(&idx, "Música").unwrap());
        fn check(idx: &Index) {
            let mut cursor = 0;
            // Compare every offset (including terminators) with an independent
            // linear lookup; then exercise jumps and backwards/restarted scans.
            let offsets: Vec<usize> = (0..idx.folded_blob().len()).collect();
            for off in offsets.iter().copied().chain(offsets.iter().rev().step_by(7).copied()) {
                let want = idx.owner_off.iter().rposition(|&o| o as usize <= off).and_then(|i| {
                    let id = idx.owner_id[i];
                    let e = &idx.entries[id as usize];
                    (e.fold_off == idx.owner_off[i] && e.kind != Kind::Dead).then_some(id)
                });
                assert_eq!(idx.owner_at(off, &mut cursor), want, "offset {off}");
            }
        }
        check(&idx);
        idx.compact();
        check(&idx);
    }
}

#[cfg(test)]
mod multi_root_tests {
    use super::tests::tempdir;
    use super::*;

    #[test]
    fn roots_keep_their_own_paths_flags_and_excludes() {
        let a = tempdir::Dir::new();
        let b = tempdir::Dir::new();
        std::fs::create_dir_all(a.0.join(".cfg/x")).unwrap();
        std::fs::create_dir_all(b.0.join("data/.git")).unwrap();
        std::fs::write(b.0.join("data/report.txt"), b"").unwrap();
        let nested = a.0.join(".cfg");
        let idx = build(&[a.0.clone(), b.0.clone(), nested], &[".git".into()], 2, &NoHook).unwrap();
        assert_eq!(idx.roots.len(), 2, "a root inside another one is dropped");
        let rb = idx.roots[1].id;
        let data = idx.child_by_name(rb, b"data").unwrap();
        let report = idx.child_by_name(data, b"report.txt").unwrap();
        assert_eq!(idx.root_of(report), 1);
        assert_eq!(idx.entries[report as usize].depth, 2);
        assert_eq!(idx.path(report), std::fs::canonicalize(&b.0).unwrap().join("data/report.txt"));
        assert!(idx.child_by_name(data, b".git").is_none(), "excludes apply in every root");
        let cfg = idx.child_by_name(idx.roots[0].id, b".cfg").unwrap();
        assert!(idx.entries[cfg as usize].flags & HIDDEN != 0);
        assert_eq!(idx.entries[idx.roots[0].id as usize].flags & HIDDEN, 0, "a root is never hidden");
        let mut rel = Vec::new();
        idx.folded_rel_path(report, &mut rel);
        assert_eq!(rel, b"data/report.txt");
    }
}
