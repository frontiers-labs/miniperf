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
    /// Register a request unless its value is current or already computing.
    pub fn request(&mut self, key: u64) -> bool {
        if self.key == Some(key) || self.inflight == Some(key) {
            return false;
        }
        self.inflight = Some(key);
        true
    }

    pub fn install(&mut self, key: u64, value: Option<Arc<T>>) -> bool {
        if self.inflight != Some(key) {
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

    pub fn stale(&self, key: u64) -> bool {
        self.key != Some(key)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
