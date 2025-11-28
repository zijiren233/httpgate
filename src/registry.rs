use dashmap::DashMap;
use tracing::debug;

/// Information about a registered devbox
#[derive(Debug, Clone)]
pub struct DevboxInfo {
    pub namespace: String,
}

/// Thread-safe registry mapping uniqueID to devbox info.
pub struct DevboxRegistry {
    inner: DashMap<String, DevboxInfo>,
}

impl DevboxRegistry {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }

    /// Register a devbox with its `unique_id` and namespace.
    /// Returns `true` if this is a new entry, `false` if updating existing.
    pub fn register(&self, unique_id: String, namespace: String) -> bool {
        self.inner
            .insert(unique_id, DevboxInfo { namespace })
            .is_none()
    }

    /// Unregister a devbox by its unique_id.
    pub fn unregister(&self, unique_id: &str) -> bool {
        self.inner.remove(unique_id).is_some()
    }

    /// Clear all entries (used during watcher re-initialization).
    pub fn clear(&self) {
        self.inner.clear();
        debug!("Registry cleared");
    }

    /// Look up a devbox by unique_id.
    ///
    /// Returns a clone of the DevboxInfo to avoid holding any locks.
    pub fn get(&self, unique_id: &str) -> Option<DevboxInfo> {
        self.inner.get(unique_id).map(|r| r.value().clone())
    }

    /// Get the current number of registered devboxes.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for DevboxRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_register_and_get() {
        let registry = DevboxRegistry::new();
        registry.register("test-id".to_string(), "ns-test".to_string());

        let info = registry.get("test-id").unwrap();
        assert_eq!(info.namespace, "ns-test");
    }

    #[test]
    fn test_unregister() {
        let registry = DevboxRegistry::new();
        registry.register("test-id".to_string(), "ns-test".to_string());

        assert!(registry.unregister("test-id"));
        assert!(registry.get("test-id").is_none());
        assert!(!registry.unregister("test-id")); // Already removed
    }

    #[test]
    fn test_clear() {
        let registry = DevboxRegistry::new();
        registry.register("test-1".to_string(), "ns-1".to_string());
        registry.register("test-2".to_string(), "ns-2".to_string());

        assert_eq!(registry.len(), 2);
        registry.clear();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_concurrent_writes() {
        let registry = Arc::new(DevboxRegistry::new());
        let mut handles = vec![];

        // Spawn 100 threads, each registering a unique entry
        for i in 0..100 {
            let reg = Arc::clone(&registry);
            handles.push(thread::spawn(move || {
                reg.register(format!("id-{}", i), format!("ns-{}", i));
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All 100 entries should be present
        assert_eq!(registry.len(), 100);

        for i in 0..100 {
            let info = registry.get(&format!("id-{}", i)).unwrap();
            assert_eq!(info.namespace, format!("ns-{}", i));
        }
    }

    #[test]
    fn test_concurrent_read_write() {
        let registry = Arc::new(DevboxRegistry::new());

        // Pre-populate
        for i in 0..50 {
            registry.register(format!("id-{}", i), format!("ns-{}", i));
        }

        let mut handles = vec![];

        // Writers
        for i in 50..100 {
            let reg = Arc::clone(&registry);
            handles.push(thread::spawn(move || {
                reg.register(format!("id-{}", i), format!("ns-{}", i));
            }));
        }

        // Readers
        for i in 0..50 {
            let reg = Arc::clone(&registry);
            handles.push(thread::spawn(move || {
                // Should always find pre-populated entries
                assert!(reg.get(&format!("id-{}", i)).is_some());
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(registry.len(), 100);
    }
}
