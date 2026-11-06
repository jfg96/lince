//! Pseudo file systems mounted below a root.
//!
//! A crawl of "/" would otherwise walk /proc and /sys: hundreds of thousands
//! of entries nobody searches for by name, whose changes inotify mostly does
//! not report, and which spend watches that real directories need. tmpfs is
//! left in: /tmp and the like hold real files. A root placed inside one of
//! these file systems is still indexed, since only mounts below it are skipped.

use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const PSEUDO: &[&str] = &[
    "binfmt_misc", "bpf", "cgroup", "cgroup2", "configfs", "debugfs", "devpts", "devtmpfs", "efivarfs",
    "fuse.gvfsd-fuse", "fuse.portal", "fusectl", "hugetlbfs", "mqueue", "proc", "pstore", "securityfs",
    "sysfs", "tracefs",
];

/// Mount points in /proc/self/mountinfo escape spaces and the like as \ooo.
fn unescape(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).filter(|d| bytes[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)));
        if let Some(d) = octal {
            out.push(d.iter().fold(0u8, |v, b| v.wrapping_mul(8).wrapping_add(b - b'0')));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Mount points of pseudo file systems. A later mount over the same point
/// hides an earlier one, so the last line for a point decides.
fn pseudo_mounts(mountinfo: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for line in mountinfo.lines() {
        // id parent dev root MOUNT options [tags...] - FSTYPE source options
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let (Some(mount), Some(fstype)) = (left.split(' ').nth(4), right.split(' ').next()) else { continue };
        let mount = unescape(mount);
        found.retain(|m| *m != mount);
        if PSEUDO.contains(&fstype) {
            found.push(mount);
        }
    }
    found
}

fn relative_to(mountinfo: &str, root: &Path) -> Vec<PathBuf> {
    pseudo_mounts(mountinfo)
        .iter()
        .filter_map(|m| m.strip_prefix(root).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect()
}

/// Pseudo file systems mounted strictly below `root`, relative to it.
pub fn pseudo_below(root: &Path) -> Vec<PathBuf> {
    relative_to(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), root)
}

/// Exclude pattern for exactly the path `rel` below the root.
pub fn anchored(rel: &Path) -> String {
    let mut out = vec![b'/'];
    for &b in rel.as_os_str().as_bytes() {
        if b"\\*?[]{}".contains(&b) {
            out.push(b'\\');
        }
        out.push(b);
    }
    OsStr::from_bytes(&out).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
23 1 0:22 / /sys rw,nosuid shared:6 - sysfs sysfs rw
24 23 0:7 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw
25 1 0:5 / /dev rw shared:2 - devtmpfs devtmpfs rw
26 1 0:30 / /run rw shared:7 - tmpfs tmpfs rw
27 1 8:2 /@home /home rw,relatime shared:8 - btrfs /dev/nvme0n1p2 rw
28 27 0:40 / /home/me/odd\\040name rw - fuse.gvfsd-fuse gvfsd rw
29 1 0:41 / /over rw - proc proc rw
30 1 8:3 / /over rw - ext4 /dev/sdb1 rw
";

    #[test]
    fn finds_pseudo_mounts_below_a_root() {
        let names = |root: &str| relative_to(INFO, Path::new(root));
        assert_eq!(
            names("/"),
            ["proc", "sys", "sys/fs/cgroup", "dev", "home/me/odd name"].map(PathBuf::from),
            "tmpfs and real disks stay; a mount hidden by a real one is not skipped"
        );
        assert_eq!(names("/home/me"), [PathBuf::from("odd name")]);
        assert_eq!(names("/sys"), [PathBuf::from("fs/cgroup")], "the root itself is never skipped");
        assert!(names("/mnt").is_empty());
    }

    #[test]
    fn anchored_patterns_match_only_that_literal_path() {
        let root = Path::new("/r");
        let pattern = anchored(Path::new("a[b]/c*"));
        let ov = crate::index::build_excludes(root, &[pattern]).unwrap();
        assert!(ov.matched("/r/a[b]/c*", true).is_ignore());
        assert!(!ov.matched("/r/ab/cd", true).is_ignore());
        assert!(!ov.matched("/r/x/a[b]/c*", true).is_ignore());
    }
}
