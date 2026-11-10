//! The compiled-code cache: `<dir>/<key>.cwasm` files holding serialized components, keyed
//! by the module hash and the engine's compatibility hash.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wasmtime::Engine;
use wasmtime::component::Component;

use crate::CodeCache;

/// How a cache hit is turned back into a component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    /// `Component::deserialize_file`: the file is memory-mapped.
    Mmap,
    /// Read the bytes, then `Component::deserialize`: never memory-mapped (Android).
    Read,
}

impl Strategy {
    pub(crate) fn of(cache: &CodeCache) -> Option<Strategy> {
        match cache {
            CodeCache::None => None,
            CodeCache::Mmap(_) => Some(Strategy::Mmap),
            CodeCache::Read(_) => Some(Strategy::Read),
        }
    }

    #[cfg(test)]
    pub(crate) fn maps_files(self) -> bool {
        matches!(self, Strategy::Mmap)
    }

    fn deserialize(self, engine: &Engine, path: &Path) -> wasmtime::Result<Component> {
        // SAFETY: wasmtime requires the bytes to come from `Component::serialize` of a
        // compatible engine. The cache directory is node-private and only this module
        // writes it (atomically, under a key that includes the engine's compatibility
        // hash), so entries are trusted. For `Mmap` the file must also not change while
        // mapped: entries are only ever replaced by rename, never written in place.
        unsafe {
            match self {
                Strategy::Mmap => Component::deserialize_file(engine, path),
                Strategy::Read => Component::deserialize(engine, std::fs::read(path)?),
            }
        }
    }
}

pub(crate) struct Cache {
    /// `None` when caching is off.
    store: Option<Store>,
    hits: AtomicU64,
    misses: AtomicU64,
}

struct Store {
    dir: PathBuf,
    strategy: Strategy,
    /// Hex of the engine's compatibility hash: the second half of every key.
    engine_key: String,
}

/// Adapts `Hash` output to blake3: a fixed algorithm instead of `DefaultHasher`, whose
/// output is not guaranteed stable across Rust releases.
struct Blake3Hasher(blake3::Hasher);

impl Hasher for Blake3Hasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    fn finish(&self) -> u64 {
        unreachable!("only the full digest is used")
    }
}

impl Cache {
    pub(crate) fn new(config: CodeCache, engine: &Engine) -> Cache {
        let store = Strategy::of(&config).map(|strategy| {
            let (CodeCache::Mmap(dir) | CodeCache::Read(dir)) = config else {
                unreachable!("a strategy implies a directory")
            };
            // `precompile_compatibility_hash` only offers `impl Hash`, so feed it to blake3.
            let mut h = Blake3Hasher(blake3::Hasher::new());
            engine.precompile_compatibility_hash().hash(&mut h);
            Store {
                dir,
                strategy,
                engine_key: h.0.finalize().to_hex().to_string(),
            }
        });
        Cache {
            store,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// The component for `wasm`, from the cache when possible. Blocking: call from
    /// `spawn_blocking`. `hash` must identify `wasm`; it is trusted, not checked.
    /// Cache problems never fail the call: a bad entry is deleted and recompiled, and a
    /// failed write is logged.
    pub(crate) fn load(
        &self,
        engine: &Engine,
        hash: &[u8; 32],
        wasm: &[u8],
    ) -> wasmtime::Result<Component> {
        let Some(store) = &self.store else {
            return Component::from_binary(engine, wasm);
        };
        let path = store
            .dir
            .join(format!("{}{}.cwasm", hex::encode(hash), store.engine_key));
        if path.exists() {
            match store.strategy.deserialize(engine, &path) {
                Ok(c) => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(c);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), "discarding unusable code cache entry: {e:#}");
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!(path = %path.display(), "removing code cache entry: {e}");
                    }
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let component = Component::from_binary(engine, wasm)?;
        match component.serialize() {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&path, &bytes) {
                    tracing::warn!(path = %path.display(), "writing code cache entry: {e}");
                }
            }
            Err(e) => tracing::warn!("serializing component for the code cache: {e:#}"),
        }
        Ok(component)
    }
}

/// Writes `bytes` to `path` via a temp file in the same directory: fsync, then rename
/// (which replaces an existing file on Windows too).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().expect("cache path has a directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_mode_never_mmaps() {
        assert_eq!(
            Strategy::of(&CodeCache::Read("x".into())),
            Some(Strategy::Read)
        );
        assert!(!Strategy::Read.maps_files());
        assert_eq!(
            Strategy::of(&CodeCache::Mmap("x".into())),
            Some(Strategy::Mmap)
        );
        assert!(Strategy::Mmap.maps_files());
        assert_eq!(Strategy::of(&CodeCache::None), None);
    }
}
