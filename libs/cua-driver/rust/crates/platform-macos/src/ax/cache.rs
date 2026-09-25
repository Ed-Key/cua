use super::bindings::AXUIElementRef;
use super::diff::Rows;
use super::tree::AXNode;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::element_cache::{ElementCacheCore, SnapshotPayload};

pub struct RetainedElement(usize);

impl RetainedElement {
    pub fn as_ptr(&self) -> usize {
        self.0
    }

    pub unsafe fn retain(ptr: usize) -> Self {
        if ptr != 0 {
            unsafe { CFRetain(ptr as AXUIElementRef as CFTypeRef) };
        }
        Self(ptr)
    }
}

impl Clone for RetainedElement {
    fn clone(&self) -> Self {
        unsafe { Self::retain(self.0) }
    }
}

impl Drop for RetainedElement {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CFRelease(self.0 as AXUIElementRef as CFTypeRef) };
        }
    }
}

/// The latest look at one window. Besides owning the element handles that
/// tokens resolve to, it keeps what the next look needs to number rows the
/// same way (`diff::assign_stable_indices`) and to send only what changed
/// (`diff::diff_outline`).
pub struct CachedSnapshot {
    /// `(element_index, AXUIElementRef)` per actionable row, one retain each.
    /// Indices are sparse after the first look at a window: a row keeps its
    /// number across looks and a vanished row's number is never reused.
    pub elements: Vec<(usize, usize)>,
    pub rows: Rows,
    /// Next never-used `element_index` for this window.
    pub next_id: usize,
    /// `(max_elements, max_depth)` the walk was bounded by.
    pub bounds: (usize, usize),
    /// Session that took this look. A diff is only offered to the same one,
    /// since another session never saw the outline the diff is relative to.
    pub session: Option<String>,
}

/// What a new look borrows from the previous one, retained independently so
/// the cache can replace the snapshot while the matcher is still alive.
pub struct PriorLook {
    pub elements: Vec<(usize, RetainedElement)>,
    pub rows: Rows,
    pub next_id: usize,
    pub bounds: (usize, usize),
    pub session: Option<String>,
}

impl PriorLook {
    /// `(element_index, raw ptr)` pairs for `diff::identity_matcher`. Valid
    /// only while `self` lives, which holds the retains.
    pub fn identity_pairs(&self) -> Vec<(usize, usize)> {
        self.elements.iter().map(|(i, e)| (*i, e.as_ptr())).collect()
    }
}

impl CachedSnapshot {
    /// Takes ownership of the +1 retain the walk left on every actionable
    /// `element_ptr`.
    pub fn from_nodes(nodes: &[AXNode]) -> Self {
        Self::new(nodes, 0, (0, 0), None)
    }

    pub fn new(
        nodes: &[AXNode],
        next_id: usize,
        bounds: (usize, usize),
        session: Option<String>,
    ) -> Self {
        Self {
            elements: nodes
                .iter()
                .filter_map(|node| node.element_index.map(|i| (i, node.element_ptr)))
                .collect(),
            rows: super::diff::rows_of(nodes),
            next_id,
            bounds,
            session,
        }
    }

    pub fn prior_look(&self) -> PriorLook {
        PriorLook {
            elements: self
                .elements
                .iter()
                .map(|(i, ptr)| (*i, unsafe { RetainedElement::retain(*ptr) }))
                .collect(),
            rows: self.rows.clone(),
            next_id: self.next_id,
            bounds: self.bounds,
            session: self.session.clone(),
        }
    }
}

impl SnapshotPayload for CachedSnapshot {
    type Element = RetainedElement;
    fn len(&self) -> usize {
        self.elements.len()
    }
    fn retain(&self, index: usize) -> Option<RetainedElement> {
        self.elements
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, ptr)| unsafe { RetainedElement::retain(*ptr) })
    }
}

impl Drop for CachedSnapshot {
    fn drop(&mut self) {
        for (_, ptr) in &self.elements {
            if *ptr != 0 {
                unsafe { CFRelease(*ptr as AXUIElementRef as CFTypeRef) };
            }
        }
    }
}

pub type ElementCache = ElementCacheCore<CachedSnapshot>;

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::{CFGetRetainCount, TCFType};
    use core_foundation::string::CFString;
    use cua_driver_core::element_token::{token_for, ResolvedElement};

    fn resolve(cache: &ElementCache, snapshot: u32, index: usize) -> Option<RetainedElement> {
        match cache
            .resolve_element_args(
                1,
                None,
                Some(&token_for(snapshot, index)),
                None,
                Some(2),
                "click",
            )
            .ok()?
        {
            ResolvedElement::Element { element, .. } => Some(element),
            _ => None,
        }
    }

    fn payload(index: usize, ptr: usize) -> CachedSnapshot {
        unsafe { CFRetain(ptr as CFTypeRef) };
        CachedSnapshot {
            elements: vec![(index, ptr)],
            rows: Rows::default(),
            next_id: index + 1,
            bounds: (0, 0),
            session: None,
        }
    }

    #[test]
    fn retained_element_survives_concurrent_snapshot_replace() {
        let value = CFString::new("cua-driver-uaf-test-element-placeholder");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(0, ptr));
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 1);
        let guard = resolve(&cache, snapshot, 0).unwrap();
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 2);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 1);
        assert!(resolve(&cache, snapshot, 0).is_none());
        drop(guard);
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base);
    }

    #[test]
    fn admitted_element_survives_cache_destruction_until_native_work_finishes() {
        let value = CFString::new("cua-driver-invariant-admitted-native-work");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(0, ptr));
        let guard = resolve(&cache, snapshot, 0).unwrap();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finish_rx.recv().unwrap();
            assert_eq!(guard.as_ptr(), ptr);
            drop(guard);
        });
        drop(cache);
        let retained = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(retained, base + 1);
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base);
    }

    #[test]
    fn missing_index_returns_none() {
        let cache = ElementCache::new();
        assert!(resolve(&cache, 0, 0).is_none());
        let snapshot = cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert!(resolve(&cache, snapshot, 0).is_none());
        assert!(resolve(&cache, snapshot, 5).is_none());
    }

    #[test]
    fn sparse_indices_resolve_by_number_not_position() {
        // After a second look, a window's only remaining row can be number 7
        // sitting at position 0. Tokens must resolve the number.
        let value = CFString::new("cua-driver-sparse-index-element");
        let ptr = value.as_concrete_TypeRef() as usize;
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(7, ptr));
        assert_eq!(resolve(&cache, snapshot, 7).unwrap().as_ptr(), ptr);
        assert!(resolve(&cache, snapshot, 0).is_none());
    }

    #[test]
    fn prior_look_keeps_elements_alive_after_the_snapshot_is_replaced() {
        let value = CFString::new("cua-driver-prior-look-element");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        let cache = ElementCache::new();
        cache.publish(1, 2, payload(3, ptr));
        let prior = cache
            .with_latest_payload(1, 2, |p| p.prior_look())
            .unwrap();
        assert_eq!(prior.identity_pairs(), vec![(3, ptr)]);
        assert_eq!(prior.next_id, 4);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 1);
        drop(prior);
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base);
    }

    #[test]
    fn abandoned_preparation_releases_native_payload_without_replacing_snapshot() {
        let original = CFString::new("cua-driver-original-published-native-work");
        let replacement = CFString::new("cua-driver-abandoned-prepared-native-work");
        let original_ptr = original.as_concrete_TypeRef() as usize;
        let replacement_ptr = replacement.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(replacement_ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(0, original_ptr));
        let prepared = payload(0, replacement_ptr);
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
        drop(prepared);
        assert_eq!(
            unsafe { CFGetRetainCount(replacement_ptr as CFTypeRef) },
            base
        );
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
    }
}
