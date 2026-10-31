use std::sync::Arc;

/// A background-computed dataset tagged with its request identity. A stale
/// value keeps rendering until its replacement lands, and results that arrive
/// after a newer request are dropped.
pub(super) struct Derived<T, K = u64> {
    value: Option<Arc<T>>,
    key: Option<K>,
    inflight: Option<K>,
}

impl<T, K> Default for Derived<T, K> {
    fn default() -> Self {
        Self {
            value: None,
            key: None,
            inflight: None,
        }
    }
}

impl<T, K: PartialEq> Derived<T, K> {
    /// True when neither the current value nor an in-flight compute is for `key`.
    pub fn needs(&self, key: &K) -> bool {
        self.key.as_ref() != Some(key) && self.inflight.as_ref() != Some(key)
    }

    pub fn begin(&mut self, key: K) {
        self.inflight = Some(key);
    }

    /// Completes the current request, retaining the old value on absent results.
    /// An absent result permits a later retry without notifying the view.
    pub fn complete(&mut self, key: K, value: Option<Arc<T>>) -> bool {
        if self.inflight.as_ref() != Some(&key) {
            return false;
        }
        self.inflight = None;
        let Some(value) = value else {
            return false;
        };
        self.value = Some(value);
        self.key = Some(key);
        true
    }

    pub fn is_computing(&self) -> bool {
        self.inflight.is_some()
    }

    /// The last computed value, even when a newer key is pending.
    pub fn latest(&self) -> Option<&Arc<T>> {
        self.value.as_ref()
    }

    pub fn stale(&self, key: &K) -> bool {
        self.key.as_ref() != Some(key)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
