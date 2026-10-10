//! Read the kernel mount table to find out if a path is a mount point.

use std::path::{Path, PathBuf};

/// One entry of `/proc/self/mountinfo` with the fields kubedal needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fs_type: String,
}

/// Parse the content of a `mountinfo` file.
///
/// Lines that do not follow the expected format are skipped.
pub fn parse_mountinfo(content: &str) -> Vec<MountEntry> {
    content
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            // Field 5 (index 4) is the mount point. The optional fields end with "-",
            // and the file system type follows right after it.
            let mount_point = fields.get(4)?;
            let separator = fields.iter().position(|f| *f == "-")?;
            let fs_type = fields.get(separator + 1)?;
            Some(MountEntry {
                mount_point: PathBuf::from(unescape(mount_point)),
                fs_type: (*fs_type).to_string(),
            })
        })
        .collect()
}

/// The kernel writes space, tab, newline and backslash as octal escapes like `\040`.
fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or("");
            if let Ok(code) = u8::from_str_radix(digits, 8) {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read the current mount table. Returns an empty list if it cannot be read.
pub fn read_mounts() -> Vec<MountEntry> {
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(content) => parse_mountinfo(&content),
        Err(err) => {
            tracing::warn!("cannot read /proc/self/mountinfo: {err}");
            Vec::new()
        }
    }
}

/// Find the mount entry for exactly this path, if it is a mount point.
///
/// If a path is mounted several times, the last (top) entry is returned.
pub fn find_mount(path: &Path) -> Option<MountEntry> {
    find_mount_in(&read_mounts(), path)
}

/// Same as [`find_mount`], but on a given mount table.
pub fn find_mount_in(mounts: &[MountEntry], path: &Path) -> Option<MountEntry> {
    mounts.iter().rev().find(|m| m.mount_point == path).cloned()
}

/// Returns true if the path is a mount point.
pub fn is_mount_point(path: &Path) -> bool {
    find_mount(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
95 22 0:50 / /var/lib/kubelet/pods/abc/volumes/kubernetes.io~csi/data/mount rw,relatime shared:60 - ext4 /dev/sda1 rw
96 22 0:51 / /mnt/with\\040space rw,nosuid master:3 - fuse /dev/fuse rw,user_id=0
broken line
";

    #[test]
    fn parses_mount_points_and_types() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/"));
        assert_eq!(mounts[0].fs_type, "ext4");
        assert_eq!(mounts[2].mount_point, PathBuf::from("/mnt/with space"));
        assert_eq!(mounts[2].fs_type, "fuse");
    }

    #[test]
    fn finds_exact_mount_point_only() {
        let mounts = parse_mountinfo(SAMPLE);
        let target = Path::new("/var/lib/kubelet/pods/abc/volumes/kubernetes.io~csi/data/mount");
        assert!(find_mount_in(&mounts, target).is_some());
        assert!(find_mount_in(&mounts, Path::new("/var/lib/kubelet")).is_none());
    }

    #[test]
    fn unescape_handles_backslash_at_end() {
        assert_eq!(unescape("a\\"), "a\\");
        assert_eq!(unescape("a\\134b"), "a\\b");
    }
}
