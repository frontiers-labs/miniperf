use std::sync::Arc;

/// A background-computed dataset tagged with the key (filter generation plus
/// view options) it was built for. A stale value keeps rendering until its
/// replacement lands, so filter tweaks never blank a view, and results that
/// arrive after a newer request are dropped.
pub(super) struct Derived<T> {
    value: Option<Arc<T>>,
    key: Option<u64>,
    inflight: Option<u64>,
}

impl<T> Default for Derived<T> {
    fn default() -> Self {
        Self {
            value: None,
            key: None,
            inflight: None,
        }
    }
}

impl<T> Derived<T> {
    /// True when neither the current value nor an in-flight compute is for `key`.
    pub fn needs(&self, key: u64) -> bool {
        self.key != Some(key) && self.inflight != Some(key)
    }

    pub fn begin(&mut self, key: u64) {
        self.inflight = Some(key);
    }

    pub fn install(&mut self, key: u64, value: Arc<T>) -> bool {
        if self.inflight != Some(key) {
            return false;
        }
        self.value = Some(value);
        self.key = Some(key);
        self.inflight = None;
        true
    }

    /// Marks an in-flight compute as finished with no value, so a view can
    /// tell "still computing" from "nothing to show".
    pub fn discard(&mut self, key: u64) {
        if self.inflight == Some(key) {
            self.inflight = None;
        }
    }

    pub fn is_computing(&self) -> bool {
        self.inflight.is_some()
    }

    /// The last computed value, even when a newer key is pending.
    pub fn latest(&self) -> Option<&Arc<T>> {
        self.value.as_ref()
    }

    pub fn stale(&self, key: u64) -> bool {
        self.key != Some(key)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
