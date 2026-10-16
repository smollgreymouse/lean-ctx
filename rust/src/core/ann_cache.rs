//! Process-wide cache for the HNSW [`AnnIndex`] used by dense semantic search.
//!
//! Building an HNSW graph is O(n log n) with a wide construction beam, so doing
//! it per query would be slower than brute force. This cache keeps one built
//! index keyed by a content fingerprint of the embedding set: repeated queries
//! over the same corpus reuse the graph and get sub-linear search, while a
//! changed corpus (different fingerprint) transparently triggers a rebuild.
//!
//! It is threshold-gated — corpora below [`ANN_MIN_VECTORS`] skip the cache and
//! use exact SIMD brute-force top-k, which is both faster (no graph overhead)
//! and *exact*.
//! On any lock failure it falls back to brute force, so correctness never
//! depends on the cache being available.

use std::sync::{Mutex, OnceLock};

use super::hnsw::{AnnIndex, FlatEmbeddings, brute_force_topk};

/// Minimum corpus size before an HNSW graph is worth building and caching.
/// Below this, exact SIMD brute force is faster *and* exact (no recall loss).
/// At 2500, a medium codebase (~7k chunks for lean-ctx itself) enters the
/// HNSW path and gets sub-linear dense search; brute force remains the default
/// for smaller projects where it is both simpler and just as fast.
pub(crate) const ANN_MIN_VECTORS: usize = 2_500;

struct Cached {
    fingerprint: [u8; 32],
    index: AnnIndex,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Drop the cached HNSW index (#685 eviction hook). The next large-corpus
/// query transparently rebuilds it; queries in between fall back to exact
/// brute force, so correctness is unaffected. Called by the eviction
/// orchestrator under memory pressure — before this hook the built graph +
/// its `FlatEmbeddings` corpus stayed resident forever.
pub(crate) fn clear() {
    if let Ok(mut guard) = cache().lock() {
        *guard = None;
    }
}

/// Approximate resident bytes held by the cached HNSW index (flat embedding
/// matrix + graph adjacency), 0 when empty. Used by the eviction orchestrator
/// to weigh the ANN cache against the RSS budget.
#[must_use]
pub(crate) fn memory_usage_bytes() -> usize {
    let Ok(guard) = cache().lock() else {
        return 0;
    };
    guard.as_ref().map_or(0, |c| c.index.memory_usage_bytes())
}

/// Returns the top-k `(index, similarity)` pairs for `query` over `embeddings`,
/// sorted by descending similarity.
///
/// Small corpora use exact brute force. Large corpora build (once) and reuse a
/// cached HNSW index. Falls back to brute force on lock failure.
///
/// The [`FlatEmbeddings`] data is shared via `Arc::clone` (a refcount bump, zero
/// bytes copied) when building the cached HNSW index.
#[must_use]
pub(crate) fn topk(embeddings: &FlatEmbeddings, query: &[f32], top_k: usize) -> Vec<(usize, f32)> {
    topk_gated(embeddings, query, top_k, ANN_MIN_VECTORS)
}

/// Core implementation with an injectable gate so tests can exercise the HNSW
/// path without materializing a 50k-vector corpus.
fn topk_gated(
    embeddings: &FlatEmbeddings,
    query: &[f32],
    top_k: usize,
    min_vectors: usize,
) -> Vec<(usize, f32)> {
    if embeddings.n_vectors() < min_vectors {
        return brute_force_topk(embeddings, query, top_k);
    }

    let fp = fingerprint(embeddings);
    let Ok(mut guard) = cache().lock() else {
        return brute_force_topk(embeddings, query, top_k);
    };

    let needs_build = match guard.as_ref() {
        Some(c) => c.fingerprint != fp,
        None => true,
    };
    if needs_build {
        *guard = Some(Cached {
            fingerprint: fp,
            index: AnnIndex::build(embeddings.clone()),
        });
    }

    match guard.as_ref() {
        Some(c) => c.index.search(query, top_k),
        None => brute_force_topk(embeddings, query, top_k),
    }
}

/// Full content hash for the embedding matrix.
///
/// This key is correctness- and isolation-sensitive: a sampled fingerprint can
/// alias distinct corpora and make the process-wide HNSW cache serve a graph
/// built from another request's embeddings.
fn fingerprint(embeddings: &FlatEmbeddings) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(embeddings.n_vectors() as u64).to_le_bytes());
    hasher.update(&(embeddings.dim as u64).to_le_bytes());
    let mut bytes = Vec::with_capacity(4096);
    for chunk in embeddings.data.chunks(1024) {
        bytes.clear();
        for value in chunk {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        hasher.update(&bytes);
    }
    *hasher.finalize().as_bytes()
}

/// Cross-module test lock for the process-wide ANN cache. All unit tests that
/// mutate/read/clear the shared slot must serialize through this guard.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(debug_assertions)]
pub(crate) fn debug_adversarial_fingerprint_self_test() -> Result<(), String> {
    let _serial = {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| "ANN test lock poisoned")?
    };
    clear();
    let n = 1100usize;
    let mut a = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);
    for i in 0..n {
        let ta = std::f32::consts::TAU * (i as f32) / (n as f32);
        let tb = std::f32::consts::TAU * ((n - 1 - i) as f32) / (n as f32);
        a.push(vec![0.2, ta.cos(), 0.3, ta.sin(), 0.4]);
        b.push(vec![0.2, tb.cos(), 0.3, tb.sin(), 0.4]);
    }
    let a = FlatEmbeddings::from_vecs(a);
    let b = FlatEmbeddings::from_vecs(b);
    if fingerprint(&a) == fingerprint(&b) {
        return Err("full ANN corpus hash collided for adversarial corpora".into());
    }
    let query = b.get_vec(100);
    let _ = topk_gated(&a, &query, 8, 1000);
    let cached = topk_gated(&b, &query, 8, 1000);
    let exact = brute_force_topk(&b, &query, 8);
    if cached.first().map(|x| x.0) != exact.first().map(|x| x.0) {
        return Err(format!(
            "ANN cache failed to rebuild for distinct corpus: cached_top={:?}, exact_top={:?}",
            cached.first().map(|x| x.0),
            exact.first().map(|x| x.0)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Test gate that forces the HNSW path on modest corpora (AnnIndex itself
    // switches to HNSW at 1000 vectors, so 1000 here exercises the real graph).
    const TEST_GATE: usize = 1000;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        super::test_lock()
    }

    /// Reads the fingerprint of the currently cached index (test-only
    /// introspection; `tests` is a child module so it may touch private state).
    fn cached_fingerprint() -> Option<[u8; 32]> {
        cache()
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|c| c.fingerprint))
    }

    fn random_vec(dim: usize, seed: u64) -> Vec<f32> {
        let mut v = Vec::with_capacity(dim);
        let mut s = seed;
        for _ in 0..dim {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            v.push((s as f32 / u64::MAX as f32) * 2.0 - 1.0);
        }
        v
    }

    fn flat_from(vecs: Vec<Vec<f32>>) -> FlatEmbeddings {
        FlatEmbeddings::from_vecs(vecs)
    }

    /// A vector near `base` with small per-dimension noise.
    fn jitter(base: &[f32], seed: u64, scale: f32) -> Vec<f32> {
        base.iter()
            .enumerate()
            .map(|(i, &b)| {
                let s = seed
                    .wrapping_add(i as u64)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                b + ((s as f32 / u64::MAX as f32) * 2.0 - 1.0) * scale
            })
            .collect()
    }

    fn clustered(
        n_clusters: usize,
        per_cluster: usize,
        dim: usize,
    ) -> (FlatEmbeddings, Vec<Vec<f32>>) {
        let centers: Vec<Vec<f32>> = (0..n_clusters)
            .map(|c| random_vec(dim, (c as u64 + 1) * 1_000))
            .collect();
        let mut vectors = Vec::with_capacity(n_clusters * per_cluster);
        for (c, center) in centers.iter().enumerate() {
            for j in 0..per_cluster {
                vectors.push(jitter(center, (c * per_cluster + j) as u64 + 7, 0.02));
            }
        }
        (flat_from(vectors), centers)
    }

    #[test]
    fn small_corpus_matches_brute_force_exactly() {
        let flat = flat_from((0..200).map(|i| random_vec(32, i)).collect());
        let query = random_vec(32, 9_999);

        // Production gate (2500) → 200 vectors is below threshold → exact brute force.
        let via_cache = topk(&flat, &query, 8);
        let exact = brute_force_topk(&flat, &query, 8);

        assert_eq!(via_cache.len(), exact.len());
        for (a, b) in via_cache.iter().zip(exact.iter()) {
            assert_eq!(a.0, b.0, "below threshold must be exact brute force");
        }
    }

    #[test]
    fn hnsw_path_recall_matches_brute_force_on_clusters() {
        let _serial = serial();
        let (flat, centers) = clustered(24, 60, 32); // 1440 vectors
        let query = &centers[5];
        let k = 20;

        let ann = topk_gated(&flat, query, k, TEST_GATE); // forces HNSW
        let exact = brute_force_topk(&flat, query, k);
        assert_eq!(ann.len(), k);

        let exact_set: HashSet<usize> = exact.iter().map(|(i, _)| *i).collect();
        let overlap = ann.iter().filter(|(i, _)| exact_set.contains(i)).count();
        assert!(
            overlap * 100 >= k * 50,
            "HNSW recall@{k} too low: {overlap}/{k}"
        );
    }

    #[test]
    fn hnsw_path_results_are_descending() {
        let _serial = serial();
        let (flat, centers) = clustered(20, 60, 24); // 1200 vectors
        let results = topk_gated(&flat, &centers[3], 10, TEST_GATE);
        for w in results.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "results must be sorted by descending similarity"
            );
        }
    }

    #[test]
    fn rebuilds_when_corpus_changes() {
        let _serial = serial();
        let (a, ca) = clustered(20, 55, 32); // 1100 vectors
        let (b, cb) = clustered(18, 60, 32); // 1080 vectors

        let _ = topk_gated(&a, &ca[7], 5, TEST_GATE);
        assert_eq!(
            cached_fingerprint(),
            Some(fingerprint(&a)),
            "first query caches corpus A's index"
        );

        let _ = topk_gated(&b, &cb[4], 5, TEST_GATE);
        assert_eq!(
            cached_fingerprint(),
            Some(fingerprint(&b)),
            "a different corpus must force a rebuild to B"
        );

        let _ = topk_gated(&a, &ca[7], 5, TEST_GATE);
        assert_eq!(
            cached_fingerprint(),
            Some(fingerprint(&a)),
            "re-querying A must rebuild A — never serve stale B"
        );
    }

    #[test]
    fn fingerprint_differs_on_content_change() {
        let a = flat_from((0..10).map(|i| random_vec(8, i)).collect());
        let mut b_vecs: Vec<Vec<f32>> = (0..10).map(|i| random_vec(8, i)).collect();
        b_vecs[3][0] += 0.5;
        let b = flat_from(b_vecs);
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
