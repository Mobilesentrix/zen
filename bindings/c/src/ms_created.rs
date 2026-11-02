//! Compiled content for decisions created from JSON through the C API.
//!
//! The graph is prepared exactly like the loader path (`CachedLoader::prepared`: compile +
//! resolve schemas) and remembered per engine in a small LRU keyed by the JSON text. Only rule
//! code is cached, never evaluation data. Contract documented in MS-FFI.md §1 ("Created decisions").

use hashlink::LruCache;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use zen_engine::loader::DynamicLoader;
use zen_engine::model::GraphContent;

/// Prepared graphs an engine keeps for created decisions. Bounded: a long-lived engine that
/// sees many distinct rule texts evicts the least recently created ones.
pub(crate) const CREATED_CACHE_CAPACITY: usize = 64;

struct Entry {
    /// The exact JSON text the graph was parsed from; checked on every hit so a 64-bit hash
    /// collision can never serve a different rule.
    source: Arc<str>,
    graph: Arc<GraphContent>,
}

pub(crate) struct CreatedDecisionCache {
    entries: Mutex<LruCache<u64, Entry>>,
    #[cfg(test)]
    hits: std::sync::atomic::AtomicUsize,
}

impl CreatedDecisionCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(CREATED_CACHE_CAPACITY)),
            #[cfg(test)]
            hits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Returns the prepared graph for `source`. A hit costs a hash and a string compare; on a
    /// miss `parse` turns the text into a graph, which is then compiled and schema-resolved
    /// (the loader path's preparation) and remembered.
    pub(crate) async fn prepared<E>(
        &self,
        loader: &DynamicLoader,
        source: &str,
        parse: impl FnOnce(&str) -> Result<Arc<GraphContent>, E>,
    ) -> Result<Arc<GraphContent>, E> {
        let key = hash_of(source);
        if let Some(graph) = self.lookup(key, source) {
            #[cfg(test)]
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(graph);
        }

        let graph = prepare(loader, parse(source)?).await;
        self.store(key, source, graph.clone());
        Ok(graph)
    }

    /// Forgets every prepared graph.
    pub(crate) fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    fn lookup(&self, key: u64, source: &str) -> Option<Arc<GraphContent>> {
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.get(&key)?;
        (entry.source.as_ref() == source).then(|| entry.graph.clone())
    }

    fn store(&self, key: u64, source: &str, graph: Arc<GraphContent>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                key,
                Entry {
                    source: Arc::from(source),
                    graph,
                },
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }
}

/// Compiles the graph and resolves its schemas, the same preparation the loader path applies.
async fn prepare(loader: &DynamicLoader, graph: Arc<GraphContent>) -> Arc<GraphContent> {
    if graph.compiled_cache.is_some() && graph.resolved_schemas.is_some() {
        return graph;
    }
    let mut owned = Arc::try_unwrap(graph).unwrap_or_else(|shared| (*shared).clone());
    owned.compile();
    let _ = owned.resolve_schemas(loader).await;
    Arc::new(owned)
}

fn hash_of(source: &str) -> u64 {
    let mut hasher = ahash::AHasher::default();
    source.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mt::tokio_runtime;
    use zen_engine::loader::NoopLoader;

    fn graph_json(value: i64) -> String {
        format!(
            r#"{{"nodes":[
              {{"id":"in","name":"Request","type":"inputNode","position":{{"x":0,"y":0}}}},
              {{"id":"e","name":"e","type":"expressionNode","position":{{"x":0,"y":0}},
               "content":{{"expressions":[{{"id":"x","key":"v","value":"{value}"}}],"passThrough":false}}}},
              {{"id":"out","name":"Response","type":"outputNode","position":{{"x":0,"y":0}}}}],
             "edges":[{{"id":"e1","type":"edge","sourceId":"in","targetId":"e"}},
                      {{"id":"e2","type":"edge","sourceId":"e","targetId":"out"}}]}}"#
        )
    }

    fn parse(source: &str) -> Arc<GraphContent> {
        Arc::new(serde_json::from_str(source).unwrap())
    }

    fn prepared(cache: &CreatedDecisionCache, source: &str) -> Arc<GraphContent> {
        let loader: DynamicLoader = Arc::new(NoopLoader);
        tokio_runtime()
            .block_on(cache.prepared(&loader, source, |text| Ok::<_, ()>(parse(text))))
            .unwrap()
    }

    #[test]
    fn hit_does_not_parse_again() {
        let cache = CreatedDecisionCache::new();
        let source = graph_json(1);
        let loader: DynamicLoader = Arc::new(NoopLoader);
        prepared(&cache, &source);

        let hit = tokio_runtime().block_on(cache.prepared(&loader, &source, |_| {
            Err::<Arc<GraphContent>, &str>("parse must not run on a hit")
        }));
        assert!(hit.is_ok());

        let miss = tokio_runtime().block_on(cache.prepared(&loader, &graph_json(2), |_| {
            Err::<Arc<GraphContent>, &str>("bad json")
        }));
        assert_eq!(miss.unwrap_err(), "bad json");
        assert_eq!(cache.len(), 1, "a failed parse stores nothing");
    }

    #[test]
    fn created_graph_is_compiled_and_schema_resolved() {
        let cache = CreatedDecisionCache::new();
        let source = graph_json(1);
        assert!(
            parse(&source).compiled_cache.is_none(),
            "fresh parse is raw"
        );

        let graph = prepared(&cache, &source);
        assert!(graph.compiled_cache.is_some(), "expressions compiled");
        assert!(graph.resolved_schemas.is_some(), "schemas resolved");
    }

    #[test]
    fn same_source_reuses_the_prepared_graph() {
        let cache = CreatedDecisionCache::new();
        let source = graph_json(1);

        let first = prepared(&cache, &source);
        let second = prepared(&cache, &source);
        assert!(Arc::ptr_eq(&first, &second), "second create is a hit");
        assert_eq!(cache.hits(), 1);

        let other = prepared(&cache, &graph_json(2));
        assert!(
            !Arc::ptr_eq(&first, &other),
            "different rule text is a miss"
        );
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn hash_collision_never_serves_another_rule() {
        let cache = CreatedDecisionCache::new();
        let a = graph_json(1);
        let b = graph_json(2);
        let graph_a = prepared(&cache, &a);
        // force both sources onto the same slot
        cache.store(hash_of(&b), &a, graph_a.clone());

        let graph_b = prepared(&cache, &b);
        assert!(!Arc::ptr_eq(&graph_a, &graph_b));
        assert_eq!(cache.hits(), 0, "source mismatch is a miss");
        assert!(
            cache.lookup(hash_of(&b), &b).is_some(),
            "the slot now holds rule b"
        );
    }

    #[test]
    fn clear_forgets_prepared_graphs_and_capacity_is_bounded() {
        let cache = CreatedDecisionCache::new();
        let source = graph_json(1);
        let first = prepared(&cache, &source);
        cache.clear();
        assert_eq!(cache.len(), 0);
        let again = prepared(&cache, &source);
        assert!(!Arc::ptr_eq(&first, &again), "recompiled after clear");

        for value in 0..(CREATED_CACHE_CAPACITY as i64 + 10) {
            prepared(&cache, &graph_json(value));
        }
        assert_eq!(cache.len(), CREATED_CACHE_CAPACITY);
    }
}
