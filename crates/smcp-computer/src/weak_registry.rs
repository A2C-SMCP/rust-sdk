use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};

/// A keyed registry that shares live values without extending their lifetime.
///
/// Dead slots are removed whenever a missing key creates a new value. The common
/// lookup path for a live key therefore remains O(1), while churn cannot retain
/// unbounded owned keys after every caller has dropped its value.
pub(crate) struct WeakRegistry<K, V> {
    entries: Mutex<HashMap<K, Weak<V>>>,
}

impl<K, V> Default for WeakRegistry<K, V> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> WeakRegistry<K, V>
where
    K: Eq + Hash,
{
    pub(crate) fn get_or_insert_with(&self, key: K, create: impl FnOnce() -> V) -> Arc<V> {
        let mut entries = self.entries.lock().expect("weak registry poisoned");
        if let Some(value) = entries.get(&key).and_then(Weak::upgrade) {
            return value;
        }

        entries.retain(|_, value| value.strong_count() > 0);
        let value = Arc::new(create());
        entries.insert(key, Arc::downgrade(&value));
        value
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().expect("weak registry poisoned").len()
    }

    #[cfg(test)]
    pub(crate) fn matching_keys(&self, predicate: impl Fn(&K) -> bool) -> usize {
        self.entries
            .lock()
            .expect("weak registry poisoned")
            .keys()
            .filter(|key| predicate(key))
            .count()
    }
}
