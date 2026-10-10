//! Path checks for object keys, manifest entries and volume IDs.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Why a relative path was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path {0:?} must be relative")]
    Absolute(String),
    #[error("path {0:?} must not contain '..'")]
    ParentDir(String),
    #[error("path {0:?} contains a NUL byte")]
    Nul(String),
}

/// Check a relative path (an object key below a prefix or a manifest entry) and
/// return it in normal form: no leading or trailing slash, no empty or "." parts.
///
/// Paths that would leave the volume (absolute paths or ".." parts) are rejected.
pub fn safe_relative(path: &str) -> Result<String, PathError> {
    if path.contains('\0') {
        return Err(PathError::Nul(path.to_string()));
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute(path.to_string()));
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(PathError::ParentDir(path.to_string())),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(parts.join("/"))
}

/// Join a checked relative path to a base directory.
pub fn join_safe(base: &Path, relative: &str) -> Result<PathBuf, PathError> {
    Ok(base.join(safe_relative(relative)?))
}

/// Normalize a prefix inside a DataNode, for example `/data/x` becomes `data/x/`.
///
/// An empty prefix, "/" or "." means the root of the DataNode and becomes "".
pub fn normalize_prefix(prefix: &str) -> Result<String, PathError> {
    let trimmed = prefix.trim_start_matches('/');
    match safe_relative(trimmed) {
        Ok(path) => Ok(format!("{path}/")),
        Err(PathError::Empty) => Ok(String::new()),
        Err(err) => Err(err),
    }
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A stable ID for one publication of a volume at one target path.
///
/// The same volume can be published to several pods on a node, so the ID uses both
/// the volume ID and the target path. The result is safe to use as a file name.
pub fn publish_id(volume_id: &str, target_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(volume_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(target_path.as_bytes());
    to_hex(&hasher.finalize())[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_paths_are_cleaned() {
        assert_eq!(safe_relative("a/b/c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(safe_relative("a//b/./c/").unwrap(), "a/b/c");
        assert_eq!(safe_relative("./x").unwrap(), "x");
    }

    #[test]
    fn escaping_paths_are_rejected() {
        assert_eq!(
            safe_relative("/etc/passwd"),
            Err(PathError::Absolute("/etc/passwd".into()))
        );
        assert!(matches!(
            safe_relative("a/../../b"),
            Err(PathError::ParentDir(_))
        ));
        assert!(matches!(safe_relative(".."), Err(PathError::ParentDir(_))));
        assert_eq!(safe_relative(""), Err(PathError::Empty));
        assert_eq!(safe_relative("./"), Err(PathError::Empty));
        assert!(matches!(safe_relative("a\0b"), Err(PathError::Nul(_))));
    }

    #[test]
    fn dots_inside_names_are_fine() {
        assert_eq!(safe_relative("a/..b/c..").unwrap(), "a/..b/c..");
    }

    #[test]
    fn join_stays_inside_base() {
        let base = Path::new("/data/vol");
        assert_eq!(
            join_safe(base, "x/y").unwrap(),
            PathBuf::from("/data/vol/x/y")
        );
        assert!(join_safe(base, "../x").is_err());
    }

    #[test]
    fn prefixes_are_normalized() {
        assert_eq!(normalize_prefix("").unwrap(), "");
        assert_eq!(normalize_prefix("/").unwrap(), "");
        assert_eq!(normalize_prefix("/data/x").unwrap(), "data/x/");
        assert_eq!(normalize_prefix("data/x/").unwrap(), "data/x/");
        assert!(normalize_prefix("/data/../x").is_err());
    }

    #[test]
    fn publish_id_is_stable_and_distinct() {
        let a = publish_id("vol", "/target/a");
        assert_eq!(a, publish_id("vol", "/target/a"));
        assert_eq!(a.len(), 32);
        assert_ne!(a, publish_id("vol", "/target/b"));
        // The separator keeps "ab" + "c" apart from "a" + "bc".
        assert_ne!(publish_id("ab", "c"), publish_id("a", "bc"));
    }
}
