//! Orcave Library - Duplicate detection by content hash (blake3).
//!
//! Files are hashed in streaming fashion; the source is only read, never written.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::Context;
use blake3::Hasher;
use tracing::debug;

/// Size of each read chunk when hashing a file.
const HASH_CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

/// Compute the blake3 content hash of a file, returning [u8; 32].
///
/// Read-only: opens the file with read-only access.
pub fn hash_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let file = File::open(path)
        .with_context(|| format!("failed to open file for hashing: {}", path.display()))?;
    let mut reader = BufReader::with_capacity(HASH_CHUNK_SIZE, file);
    let mut hasher = Hasher::new();
    let mut buf = [0u8; HASH_CHUNK_SIZE];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("failed to read file for hashing: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut out = [0u8; 32];
    hasher.finalize_xof().fill(&mut out);
    debug!(path = %path.display(), hash = %hex_encode(&out), "computed content hash");
    Ok(out)
}

/// Compute blake3 hash of an in-memory byte slice.
pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize_xof().fill(&mut out);
    out
}

/// Hex-encode a 32-byte hash for display/logging.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Duplicate detection index: content hash -> track id.
#[derive(Debug, Clone, Default)]
pub struct DedupIndex {
    /// hash hex -> track id (first-seen track wins).
    by_hash: ahash::HashMap<String, String>,
    /// track id -> hash hex.
    by_track: ahash::HashMap<String, String>,
}

impl DedupIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a track's content hash. Returns `None` if this is a new hash,
    /// or `Some(existing_track_id)` if the same content was already seen.
    pub fn register(&mut self, track_id: &str, hash: &[u8; 32]) -> Option<String> {
        let hex = hex_encode(hash);
        if let Some(existing) = self.by_hash.get(&hex) {
            if existing != track_id {
                return Some(existing.clone());
            }
        }
        self.by_hash.insert(hex.clone(), track_id.to_string());
        self.by_track.insert(track_id.to_string(), hex);
        None
    }

    /// Look up the track id that owns a content hash, if any.
    pub fn owner_of(&self, hash: &[u8; 32]) -> Option<&String> {
        self.by_hash.get(&hex_encode(hash))
    }

    /// Remove a track from the index (e.g. when its file is deleted).
    pub fn remove(&mut self, track_id: &str) {
        if let Some(hex) = self.by_track.remove(track_id) {
            self.by_hash.remove(&hex);
        }
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_is_deterministic() {
        let a = hash_bytes(b"hello world");
        let b = hash_bytes(b"hello world");
        assert_eq!(a, b);
        let c = hash_bytes(b"hello world!");
        assert_ne!(a, c);
    }

    #[test]
    fn dedup_index_flags_repeat_content() {
        let mut idx = DedupIndex::new();
        let h1 = hash_bytes(b"content-a");
        let h2 = hash_bytes(b"content-b");

        assert!(idx.register("track-1", &h1).is_none());
        assert!(idx.register("track-2", &h2).is_none());
        // Same content as track-1: flagged as duplicate of track-1.
        assert_eq!(idx.register("track-3", &h1), Some("track-1".to_string()));
        assert_eq!(idx.owner_of(&h1), Some(&"track-1".to_string()));
    }

    #[test]
    fn dedup_index_remove_unregisters() {
        let mut idx = DedupIndex::new();
        let h = hash_bytes(b"content-x");
        idx.register("track-1", &h);
        assert!(idx.owner_of(&h).is_some());
        idx.remove("track-1");
        assert!(idx.owner_of(&h).is_none());
        assert!(idx.is_empty());
    }

    #[test]
    fn hex_encode_produces_lowercase_hex() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
