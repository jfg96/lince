//! Snapshot persistence and validation.
//!
//! Format LNC1 (little endian, varints are LEB128):
//!   "LNC1" u32 version
//!   varint root count, each: varint len + absolute path, i64 ctime_ns
//!   varint exclude count, each: varint len + pattern
//!   varint entry count, then entries breadth-first (parents first):
//!     varint parent (stream position: roots are 0..roots-1, entries follow),
//!     u8 kind, varint keep, varint suffix_len, suffix bytes   (front coding)
//!     dirs only: i64 ctime_ns
//! Siblings are written sorted by name so front coding shares prefixes.
//! Folded names are not stored; they are recomputed on load.
//!
//! An unprivileged process has no change journal to replay on Linux, so a
//! loaded snapshot is validated instead: every directory is stat'ed and only
//! those whose ctime differs are re-read.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::index::{crawl, ctime_ns, list_dir, DirHook, Index, Kind};

const MAGIC: &[u8; 4] = b"LNC1";
const VERSION: u32 = 1;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or("truncated snapshot")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.bytes(1)?[0];
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad varint".into())
    }
}

pub fn save(index: &Index, path: &Path) -> Result<(), String> {
    let mut out = Vec::with_capacity(index.live * 12);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_varint(&mut out, index.roots.len() as u64);
    for r in &index.roots {
        let p = r.path.as_os_str().as_bytes();
        put_varint(&mut out, p.len() as u64);
        out.extend_from_slice(p);
        out.extend_from_slice(&index.dir_stamp.get(&r.id).copied().unwrap_or(0).to_le_bytes());
    }
    put_varint(&mut out, index.exclude_patterns.len() as u64);
    for p in &index.exclude_patterns {
        put_varint(&mut out, p.len() as u64);
        out.extend_from_slice(p.as_bytes());
    }
    put_varint(&mut out, (index.live - index.roots.len()) as u64);
    // Stream position of each written directory, for parent references.
    let mut pos_of = std::collections::HashMap::with_capacity(index.dir_stamp.len() + 1);
    for (i, r) in index.roots.iter().enumerate() {
        pos_of.insert(r.id, i as u64);
    }
    let mut written = index.roots.len() as u64;
    let mut prev: &[u8] = b"";
    let mut queue: std::collections::VecDeque<u32> = index.roots.iter().map(|r| r.id).collect();
    while let Some(dir) = queue.pop_front() {
        let mut kids: Vec<u32> = index.children(dir).collect();
        kids.sort_unstable_by(|a, b| index.name(*a).cmp(index.name(*b)));
        for kid in kids {
            let name = index.name(kid);
            let keep = prev.iter().zip(name).take_while(|(a, b)| a == b).count();
            put_varint(&mut out, pos_of[&dir]);
            out.push(index.entries[kid as usize].kind as u8);
            put_varint(&mut out, keep as u64);
            put_varint(&mut out, (name.len() - keep) as u64);
            out.extend_from_slice(&name[keep..]);
            if index.is_dir(kid) {
                out.extend_from_slice(&index.dir_stamp.get(&kid).copied().unwrap_or(0).to_le_bytes());
                pos_of.insert(kid, written);
                queue.push_back(kid);
            }
            written += 1;
            prev = name;
        }
    }
    // Write-then-rename so a crash never leaves a torn snapshot.
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(&out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Loads a snapshot built for exactly these roots and excludes.
pub fn load(path: &Path, roots: &[PathBuf], patterns: &[String]) -> Result<Index, String> {
    let buf = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut r = Reader { buf: &buf, pos: 0 };
    if r.bytes(4)? != MAGIC || u32::from_le_bytes(r.bytes(4)?.try_into().unwrap()) != VERSION {
        return Err("not a LNC1 snapshot".into());
    }
    let nroots = r.varint()? as usize;
    let mut stored_roots = Vec::with_capacity(nroots);
    for _ in 0..nroots {
        let n = r.varint()? as usize;
        let p = PathBuf::from(std::ffi::OsStr::from_bytes(r.bytes(n)?));
        let stamp = i64::from_le_bytes(r.bytes(8)?.try_into().unwrap());
        stored_roots.push((p, stamp));
    }
    let count = r.varint()? as usize;
    let mut stored = Vec::with_capacity(count);
    for _ in 0..count {
        let n = r.varint()? as usize;
        stored.push(String::from_utf8_lossy(r.bytes(n)?).into_owned());
    }
    if stored != patterns {
        return Err("snapshot was built with different excludes".into());
    }
    let mut index = Index::new(roots, patterns)?;
    if index.roots.len() != nroots || index.roots.iter().zip(&stored_roots).any(|(a, b)| a.path != b.0) {
        return Err("snapshot was built for different roots".into());
    }
    let mut id_at: Vec<u32> = Vec::new();
    for (root, (_, stamp)) in index.roots.iter().zip(&stored_roots) {
        id_at.push(root.id);
        index.dir_stamp.insert(root.id, *stamp);
    }
    let total = r.varint()? as usize;
    let mut prev: Vec<u8> = Vec::new();
    for _ in 0..total {
        let parent_pos = r.varint()? as usize;
        let kind = match r.bytes(1)?[0] {
            1 => Kind::Dir,
            2 => Kind::File,
            _ => return Err("bad entry kind".into()),
        };
        let keep = r.varint()? as usize;
        let suffix = r.varint()? as usize;
        if keep > prev.len() || parent_pos >= id_at.len() {
            return Err("corrupt snapshot".into());
        }
        prev.truncate(keep);
        prev.extend_from_slice(r.bytes(suffix)?);
        let parent = id_at[parent_pos];
        if parent == u32::MAX || !index.is_dir(parent) {
            return Err("corrupt parent reference".into());
        }
        let id = index.add_child(parent, &prev, kind);
        if kind == Kind::Dir {
            let stamp = i64::from_le_bytes(r.bytes(8)?.try_into().unwrap());
            index.dir_stamp.insert(id, stamp);
            id_at.push(id);
        } else {
            id_at.push(u32::MAX);
        }
    }
    index.shrink();
    Ok(index)
}

/// Brings an index up to date with the disk by re-reading only directories
/// whose ctime changed. Returns how many directories were re-read. Handlers
/// are idempotent, so it is safe to run while watch events are pending.
pub fn refresh(index: &mut Index, threads: usize, hook: &dyn DirHook) -> usize {
    // Breadth-first so a changed parent is reconciled before its children.
    let mut order = Vec::with_capacity(index.dir_stamp.len() + 1);
    let mut queue: std::collections::VecDeque<u32> = index.roots.iter().map(|r| r.id).collect();
    while let Some(dir) = queue.pop_front() {
        order.push(dir);
        queue.extend(index.children(dir).filter(|&c| index.is_dir(c)));
    }
    let paths: Vec<PathBuf> = order.iter().map(|&d| index.path(d)).collect();
    let mut stamps = vec![None; order.len()];
    let chunk = order.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        for (paths, stamps) in paths.chunks(chunk).zip(stamps.chunks_mut(chunk)) {
            s.spawn(move || {
                for (p, out) in paths.iter().zip(stamps.iter_mut()) {
                    *out = ctime_ns(p);
                }
            });
        }
    });

    let mut changed = 0;
    let mut new_dirs = Vec::new();
    for ((&dir, path), stamp) in order.iter().zip(&paths).zip(stamps) {
        if index.entries[dir as usize].kind != Kind::Dir {
            continue; // removed together with an ancestor
        }
        let Some(stamp) = stamp else {
            if index.is_root(dir) {
                // A missing root (an unmounted disk) is emptied, not dropped,
                // so it comes back on a later refresh.
                for c in index.children(dir).collect::<Vec<_>>() {
                    index.remove(c);
                }
                index.dir_stamp.insert(dir, 0);
            } else {
                index.remove(dir);
            }
            continue;
        };
        if index.dir_stamp.get(&dir) == Some(&stamp) {
            continue;
        }
        changed += 1;
        index.dir_stamp.insert(dir, stamp);
        let ov = &index.roots[index.root_of(dir)].excludes;
        let listing = list_dir(ov, path);
        let present: std::collections::HashSet<(&[u8], Kind)> =
            listing.iter().map(|(n, k)| (n.as_slice(), *k)).collect();
        let stale: Vec<u32> = index
            .children(dir)
            .filter(|&c| !present.contains(&(index.name(c), index.entries[c as usize].kind)))
            .collect();
        for c in stale {
            index.remove(c);
        }
        for (name, kind) in &listing {
            if index.child_by_name(dir, name).is_some() {
                continue;
            }
            let id = index.add_child(dir, name, *kind);
            if *kind == Kind::Dir {
                new_dirs.push((id, path.join(std::ffi::OsStr::from_bytes(name))));
            }
        }
    }
    if !new_dirs.is_empty() {
        crawl(index, new_dirs, threads, hook);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{build, NoHook};

    #[test]
    fn roundtrip_and_refresh() {
        let dir = std::env::temp_dir().join(format!("lince-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["home/a/b", "home/c", "home/.x/y"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in ["home/a/b/one.txt", "home/a/b/one-two.txt", "home/c/Canción.md"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let home = vec![dir.join("home")];
        let idx = build(&home, &[], 2, &NoHook).unwrap();
        let snap = dir.join("snap.lnc");
        save(&idx, &snap).unwrap();
        let mut loaded = load(&snap, &home, &[]).unwrap();
        assert_eq!(loaded.live, idx.live);
        assert!(load(&snap, &home, &["x".into()]).is_err(), "different excludes invalidate");
        assert!(load(&snap, &[dir.clone()], &[]).is_err(), "different roots invalidate");

        // Offline changes: create, delete, rename a directory.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("home/c/new.md"), b"").unwrap();
        std::fs::remove_file(dir.join("home/a/b/one.txt")).unwrap();
        std::fs::rename(dir.join("home/a/b"), dir.join("home/a/renamed")).unwrap();
        std::fs::create_dir_all(dir.join("home/a/renamed/deep/er")).unwrap();
        assert!(refresh(&mut loaded, 2, &NoHook) >= 2);
        let fresh = build(&home, &[], 2, &NoHook).unwrap();
        let mut a: Vec<_> = (1..loaded.entries.len() as u32)
            .filter(|&i| loaded.entries[i as usize].kind != Kind::Dead)
            .map(|i| loaded.path(i))
            .collect();
        let mut b: Vec<_> = (1..fresh.entries.len() as u32)
            .filter(|&i| fresh.entries[i as usize].kind != Kind::Dead)
            .map(|i| fresh.path(i))
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
