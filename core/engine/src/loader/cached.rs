use ahash::{HashMap, HashMapExt};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::loader::{DecisionLoader, DynamicLoader, LoaderResponse};
use crate::model::DecisionContent;

/// Decisions a [`CachedLoader`] keeps unless built with
/// [`CachedLoader::with_capacity`]. Past it the least recently used decision is dropped (and
/// loaded again through the inner loader when it is next evaluated).
pub const DEFAULT_CACHED_DECISIONS: usize = 4096;

/// One cached decision and when it was last used (a tick of [`CachedLoader::clock`]).
#[derive(Debug)]
struct Entry {
    content: Arc<DecisionContent>,
    used: AtomicU64,
}

#[derive(Debug)]
pub struct CachedLoader {
    loader: DynamicLoader,
    /// Bounded to `capacity` entries, least recently used evicted first. A hit
    /// only takes the read lock (it bumps the entry's tick); eviction happens on insert.
    cache: RwLock<HashMap<String, Entry>>,
    capacity: usize,
    clock: AtomicU64,
    /// Bumped (under the cache write lock) by every invalidation. A load remembers the
    /// generation it started under and only inserts if nothing was invalidated meanwhile, so a
    /// slow inner load can never resurrect content that was invalidated while it was in flight.
    generation: AtomicU64,
}

impl From<DynamicLoader> for CachedLoader {
    fn from(value: DynamicLoader) -> Self {
        Self::with_capacity(value, DEFAULT_CACHED_DECISIONS)
    }
}

fn compiled(content: Arc<DecisionContent>) -> Arc<DecisionContent> {
    let DecisionContent::Graph(graph) = content.as_ref() else {
        return content;
    };
    if graph.compiled_cache.is_some() {
        return content;
    }

    let mut owned = (**graph).clone();
    owned.compile();
    Arc::new(DecisionContent::Graph(Arc::new(owned)))
}

async fn prepared(loader: &DynamicLoader, content: Arc<DecisionContent>) -> Arc<DecisionContent> {
    let DecisionContent::Graph(graph) = content.as_ref() else {
        return content;
    };
    if graph.compiled_cache.is_some() && graph.resolved_schemas.is_some() {
        return content;
    }

    let mut owned = (**graph).clone();
    owned.compile();
    let _ = owned.resolve_schemas(loader).await;
    Arc::new(DecisionContent::Graph(Arc::new(owned)))
}

impl CachedLoader {
    /// A cache that keeps at most `capacity` decisions (at least one).
    pub fn with_capacity(loader: DynamicLoader, capacity: usize) -> Self {
        Self {
            loader,
            cache: RwLock::new(HashMap::new()),
            capacity: capacity.max(1),
            clock: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// Decisions currently cached.
    pub fn len(&self) -> usize {
        self.cache.read().map(|cache| cache.len()).unwrap_or(0)
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Drops the cached content for `key`; the next load goes to the inner loader.
    /// Returns whether an entry existed.
    pub fn invalidate(&self, key: &str) -> bool {
        self.cache
            .write()
            .map(|mut cache| {
                self.generation.fetch_add(1, Ordering::AcqRel);
                cache.remove(key).is_some()
            })
            .unwrap_or(false)
    }

    /// Drops every cached entry.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.write() {
            self.generation.fetch_add(1, Ordering::AcqRel);
            cache.clear();
        }
    }

    fn cached(&self, key: &str) -> Option<Arc<DecisionContent>> {
        self.cache.read().ok().and_then(|cache| {
            cache.get(key).map(|entry| {
                entry.used.store(self.tick(), Ordering::Relaxed);
                entry.content.clone()
            })
        })
    }

    /// Stores `content` for `key` unless an invalidation happened since `generation` was read.
    /// A new key in a full cache evicts the least recently used one.
    fn insert_if_current(&self, generation: u64, key: &str, content: &Arc<DecisionContent>) {
        if let Ok(mut cache) = self.cache.write() {
            if self.generation.load(Ordering::Acquire) != generation {
                return;
            }
            if !cache.contains_key(key) && cache.len() >= self.capacity {
                let oldest = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.used.load(Ordering::Relaxed))
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    cache.remove(&oldest);
                }
            }
            let entry = Entry {
                content: content.clone(),
                used: AtomicU64::new(self.tick()),
            };
            cache.insert(key.to_string(), entry);
        }
    }
}

impl DecisionLoader for CachedLoader {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = LoaderResponse> + 'a + Send>> {
        Box::pin(async move {
            let generation = self.generation.load(Ordering::Acquire);
            let cached = self.cached(key);

            let loaded = match &cached {
                Some(content) => content.clone(),
                None => self.loader.load(key).await?,
            };

            let decision_content = prepared(&self.loader, loaded).await;
            let unchanged = cached
                .as_ref()
                .is_some_and(|content| Arc::ptr_eq(content, &decision_content));
            if !unchanged {
                self.insert_if_current(generation, key, &decision_content);
            }
            Ok(decision_content)
        })
    }

    fn keys(&self) -> Option<Vec<Arc<str>>> {
        self.loader.keys()
    }

    fn load_sync(&self, key: &str) -> Option<LoaderResponse> {
        let generation = self.generation.load(Ordering::Acquire);
        if let Some(content) = self.cached(key) {
            return Some(Ok(content));
        }

        let response = self.loader.load_sync(key)?.map(compiled);
        if let Ok(content) = &response {
            self.insert_if_current(generation, key, content);
        }
        Some(response)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::loader::MemoryLoader;
    use crate::model::DecisionContent;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Debug, Default)]
    struct CountingLoader {
        inner: MemoryLoader,
        sync_loads: AtomicUsize,
    }

    impl DecisionLoader for CountingLoader {
        fn load<'a>(
            &'a self,
            key: &'a str,
        ) -> Pin<Box<dyn Future<Output = LoaderResponse> + 'a + Send>> {
            self.inner.load(key)
        }

        fn load_sync(&self, key: &str) -> Option<LoaderResponse> {
            self.sync_loads.fetch_add(1, Ordering::SeqCst);
            self.inner.load_sync(key)
        }
    }

    /// An async loader that parks every load until the test releases it.
    #[derive(Debug, Default)]
    struct GatedLoader {
        inner: MemoryLoader,
        gate: Notify,
        started: Notify,
        loads: AtomicUsize,
    }

    impl DecisionLoader for GatedLoader {
        fn load<'a>(
            &'a self,
            key: &'a str,
        ) -> Pin<Box<dyn Future<Output = LoaderResponse> + 'a + Send>> {
            Box::pin(async move {
                self.loads.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                self.gate.notified().await;
                self.inner.load(key).await
            })
        }
    }

    #[test]
    fn the_least_recently_used_decision_is_evicted_past_capacity() {
        let counting = Arc::new(CountingLoader::default());
        for key in ["a", "b", "c"] {
            counting.inner.add(key, DecisionContent::default());
        }
        let cached = CachedLoader::with_capacity(counting.clone() as DynamicLoader, 2);
        cached.load_sync("a").unwrap().unwrap();
        cached.load_sync("b").unwrap().unwrap();
        cached.load_sync("a").unwrap().unwrap(); // a is now more recent than b
        cached.load_sync("c").unwrap().unwrap(); // evicts b
        assert_eq!(cached.len(), 2);
        assert_eq!(counting.sync_loads.load(Ordering::SeqCst), 3);
        cached.load_sync("a").unwrap().unwrap();
        cached.load_sync("c").unwrap().unwrap();
        assert_eq!(
            counting.sync_loads.load(Ordering::SeqCst),
            3,
            "a and c stayed"
        );
        cached.load_sync("b").unwrap().unwrap();
        assert_eq!(
            counting.sync_loads.load(Ordering::SeqCst),
            4,
            "b was reloaded"
        );
        assert_eq!(cached.len(), 2);

        // invalidation and clear keep working on a bounded cache
        assert!(cached.invalidate("b"));
        assert_eq!(cached.len(), 1);
        cached.clear();
        assert!(cached.is_empty());
        assert_eq!(
            CachedLoader::from(counting as DynamicLoader).capacity,
            DEFAULT_CACHED_DECISIONS
        );
    }

    #[test]
    fn load_sync_uses_cache_and_hits_inner_once() {
        let counting = Arc::new(CountingLoader::default());
        counting.inner.add("graph.json", DecisionContent::default());
        let cached = CachedLoader::from(counting.clone() as DynamicLoader);

        let first = cached.load_sync("graph.json").unwrap().unwrap();
        let second = cached.load_sync("graph.json").unwrap().unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(counting.sync_loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delegates_keys_and_load_sync_to_inner_loader() {
        let memory_loader = MemoryLoader::default();
        memory_loader.add("graph.json", DecisionContent::default());

        let cached = CachedLoader::from(Arc::new(memory_loader) as DynamicLoader);

        let keys = cached.keys().unwrap();
        assert_eq!(keys, vec![Arc::from("graph.json")]);

        let content = cached.load_sync("graph.json").unwrap().unwrap();
        assert!(content.as_graph().is_some());

        assert!(cached.load_sync("missing.json").unwrap().is_err());
    }

    #[test]
    fn invalidate_drops_one_key_and_clear_drops_all() {
        let counting = Arc::new(CountingLoader::default());
        counting.inner.add("a.json", DecisionContent::default());
        counting.inner.add("b.json", DecisionContent::default());
        let cached = CachedLoader::from(counting.clone() as DynamicLoader);

        cached.load_sync("a.json").unwrap().unwrap();
        cached.load_sync("b.json").unwrap().unwrap();
        assert_eq!(counting.sync_loads.load(Ordering::SeqCst), 2);

        assert!(cached.invalidate("a.json"));
        assert!(!cached.invalidate("a.json"), "already gone");
        assert!(!cached.invalidate("missing.json"));
        cached.load_sync("a.json").unwrap().unwrap();
        cached.load_sync("b.json").unwrap().unwrap();
        assert_eq!(
            counting.sync_loads.load(Ordering::SeqCst),
            3,
            "only a.json reloaded"
        );

        cached.clear();
        cached.load_sync("a.json").unwrap().unwrap();
        cached.load_sync("b.json").unwrap().unwrap();
        assert_eq!(
            counting.sync_loads.load(Ordering::SeqCst),
            5,
            "both reloaded"
        );
    }

    /// A load that started before an invalidation must not insert its (possibly stale)
    /// result after the invalidation, however slow the inner loader is.
    #[tokio::test]
    async fn in_flight_load_does_not_resurrect_an_invalidated_entry() {
        let gated = Arc::new(GatedLoader::default());
        gated.inner.add("g.json", DecisionContent::default());
        let cached = Arc::new(CachedLoader::from(gated.clone() as DynamicLoader));

        let in_flight = {
            let cached = cached.clone();
            tokio::spawn(async move { cached.load("g.json").await })
        };
        gated.started.notified().await;

        // rules changed while the load is parked inside the inner loader
        cached.invalidate("g.json");
        cached.clear();

        gated.gate.notify_one();
        in_flight.await.unwrap().unwrap();

        assert!(
            cached.cached("g.json").is_none(),
            "stale in-flight load was inserted after invalidation"
        );

        // and a load that starts after the invalidation is cached as usual
        let fresh = {
            let cached = cached.clone();
            tokio::spawn(async move { cached.load("g.json").await })
        };
        gated.started.notified().await;
        gated.gate.notify_one();
        let fresh = fresh.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&cached.cached("g.json").unwrap(), &fresh));
        assert_eq!(gated.loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalidation_bumps_the_generation() {
        let cached = CachedLoader::from(Arc::new(MemoryLoader::default()) as DynamicLoader);
        assert_eq!(cached.generation.load(Ordering::SeqCst), 0);
        cached.invalidate("missing.json");
        cached.clear();
        assert_eq!(cached.generation.load(Ordering::SeqCst), 2);
    }
}
