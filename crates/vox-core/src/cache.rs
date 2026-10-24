//! Audio-cache statistics (contracts.md §3.7). The cache itself is `vox_store::audio_cache`.

/// Totals from the in-memory cache index (no disk scan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Cached entries.
    pub reads: u64,
    /// Chunk files.
    pub files: u64,
    /// Audio + manifest bytes.
    pub bytes: u64,
}
