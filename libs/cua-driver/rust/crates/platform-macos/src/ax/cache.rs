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

    /// Give up the retain to the caller, who becomes responsible for it.
    fn into_raw(self) -> usize {
        let ptr = self.0;
        std::mem::forget(self);
        ptr
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

/// The mapping from a row's screen frame to its `screenshot_frame` in the
/// delivered image: window origin in screen points and delivered pixels per
/// point. Stored at fixed precision so two looks compare exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenshotTransform {
    pub origin: (i64, i64),
    /// Pixels per point times 10,000, rounded.
    pub pixels_per_point_e4: u32,
}

impl ScreenshotTransform {
    pub fn new(origin: (f64, f64), pixels_per_point: f64) -> Self {
        Self {
            origin: (origin.0.round() as i64, origin.1.round() as i64),
            pixels_per_point_e4: (pixels_per_point * 10_000.0).round() as u32,
        }
    }
}

/// Everything about a look that changes what its rows mean without the app
/// changing: the walk bounds, and the screenshot transform actually
/// delivered (None when no screenshot came back), which decides each row's
/// `screenshot_frame`. A diff is only offered between looks with equal
/// bounds, so a consumer never keeps coordinates for a differently scaled or
/// differently placed image, or misses frames that only now exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LookBounds {
    pub max_elements: usize,
    pub max_depth: usize,
    pub screenshot: Option<ScreenshotTransform>,
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
    pub bounds: LookBounds,
    /// Session that took this look. A diff is only offered to the same one,
    /// since another session never saw the outline the diff is relative to.
    pub session: Option<String>,
    /// Whether the session received every row (no query filter), so a later
    /// diff against these rows describes changes it can follow.
    pub full_delivered: bool,
    /// False for a screenshot-only look, which carries the previous rows for
    /// numbering continuity but must not resolve tokens the caller never saw.
    pub actionable: bool,
}

/// What a new look borrows from the previous one, retained independently so
/// the cache can replace the snapshot while the matcher is still alive.
pub struct PriorLook {
    pub elements: Vec<(usize, RetainedElement)>,
    pub rows: Rows,
    pub next_id: usize,
    pub bounds: LookBounds,
    pub session: Option<String>,
    pub full_delivered: bool,
}

impl PriorLook {
    /// `(element_index, raw ptr)` pairs for `diff::identity_matcher`. Valid
    /// only while `self` lives, which holds the retains.
    pub fn identity_pairs(&self) -> Vec<(usize, usize)> {
        self.elements.iter().map(|(i, e)| (*i, e.as_ptr())).collect()
    }

    /// A payload that keeps this look's numbering history alive across a
    /// screenshot-only look without exposing its rows to token resolution.
    pub fn into_history_payload(self) -> CachedSnapshot {
        CachedSnapshot {
            elements: self.elements.into_iter().map(|(i, e)| (i, e.into_raw())).collect(),
            rows: self.rows,
            next_id: self.next_id,
            bounds: self.bounds,
            session: self.session,
            full_delivered: self.full_delivered,
            actionable: false,
        }
    }
}

impl CachedSnapshot {
    /// Takes ownership of the +1 retain the walk left on every actionable
    /// `element_ptr`.
    pub fn from_nodes(nodes: &[AXNode]) -> Self {
        Self {
            elements: nodes
                .iter()
                .filter_map(|node| node.element_index.map(|i| (i, node.element_ptr)))
                .collect(),
            rows: super::diff::rows_of(nodes),
            next_id: 0,
            bounds: LookBounds::default(),
            session: None,
            full_delivered: true,
            actionable: true,
        }
    }

    /// Rebuild the row bookkeeping after `nodes` were renumbered. The set of
    /// actionable elements is unchanged, so the retains carry over one to one.
    pub fn renumber(
        &mut self,
        nodes: &[AXNode],
        next_id: usize,
        bounds: LookBounds,
        session: Option<String>,
        full_delivered: bool,
    ) {
        let elements: Vec<(usize, usize)> = nodes
            .iter()
            .filter_map(|node| node.element_index.map(|i| (i, node.element_ptr)))
            .collect();
        debug_assert_eq!(
            sorted_ptrs(&elements),
            sorted_ptrs(&self.elements),
            "renumber must keep exactly the elements the walk retained"
        );
        self.elements = elements;
        self.rows = super::diff::rows_of(nodes);
        self.next_id = next_id;
        self.bounds = bounds;
        self.session = session;
        self.full_delivered = full_delivered;
        self.actionable = true;
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
            full_delivered: self.full_delivered,
        }
    }
}

fn sorted_ptrs(elements: &[(usize, usize)]) -> Vec<usize> {
    let mut ptrs: Vec<usize> = elements.iter().map(|(_, p)| *p).collect();
    ptrs.sort_unstable();
    ptrs
}

impl SnapshotPayload for CachedSnapshot {
    type Element = RetainedElement;
    fn len(&self) -> usize {
        if self.actionable {
            self.elements.len()
        } else {
            0
        }
    }
    fn retain(&self, index: usize) -> Option<RetainedElement> {
        if !self.actionable {
            return None;
        }
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
            bounds: LookBounds::default(),
            session: None,
            full_delivered: true,
            actionable: true,
        }
    }

    fn retain_count(ptr: usize) -> isize {
        unsafe { CFGetRetainCount(ptr as CFTypeRef) }
    }

    fn button(index: Option<usize>, ptr: usize) -> AXNode {
        AXNode {
            element_index: index,
            role: "AXButton".into(),
            title: Some("Increment".into()),
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec!["AXPress".into()],
            element_ptr: ptr,
            depth: 0,
            parent_element_index: None,
            frame: None,
            value_state: None,
            value_description: None,
            placeholder: None,
            value_settable: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }

    #[test]
    fn retained_element_survives_concurrent_snapshot_replace() {
        let value = CFString::new("cua-driver-uaf-test-element-placeholder");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = retain_count(ptr);
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(0, ptr));
        assert_eq!(retain_count(ptr), base + 1);
        let guard = resolve(&cache, snapshot, 0).unwrap();
        assert_eq!(retain_count(ptr), base + 2);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(retain_count(ptr), base + 1);
        assert!(resolve(&cache, snapshot, 0).is_none());
        drop(guard);
        assert_eq!(retain_count(ptr), base);
    }

    #[test]
    fn admitted_element_survives_cache_destruction_until_native_work_finishes() {
        let value = CFString::new("cua-driver-invariant-admitted-native-work");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = retain_count(ptr);
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
        let retained = retain_count(ptr);
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(retained, base + 1);
        assert_eq!(retain_count(ptr), base);
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
        let base = retain_count(ptr);
        let cache = ElementCache::new();
        cache.publish(1, 2, payload(3, ptr));
        let prior = cache
            .with_latest_payload(1, 2, |p| p.prior_look())
            .unwrap();
        assert_eq!(prior.identity_pairs(), vec![(3, ptr)]);
        assert_eq!(prior.next_id, 4);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(retain_count(ptr), base + 1);
        drop(prior);
        assert_eq!(retain_count(ptr), base);
    }

    #[test]
    fn history_payload_keeps_numbering_but_resolves_nothing() {
        // A screenshot-only look republishes the previous rows as history:
        // the next tree look continues numbering from 4, but a token for row
        // 3 minted against this snapshot must not resolve.
        let value = CFString::new("cua-driver-history-payload-element");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = retain_count(ptr);
        let cache = ElementCache::new();
        cache.publish(1, 2, payload(3, ptr));
        let prior = cache
            .with_latest_payload(1, 2, |p| p.prior_look())
            .unwrap();
        let snapshot = cache.publish(1, 2, prior.into_history_payload());
        assert_eq!(retain_count(ptr), base + 1, "history holds exactly one retain");
        assert!(resolve(&cache, snapshot, 3).is_none());
        let again = cache
            .with_latest_payload(1, 2, |p| p.prior_look())
            .unwrap();
        assert_eq!(again.next_id, 4);
        assert_eq!(again.identity_pairs(), vec![(3, ptr)]);
        drop(again);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(retain_count(ptr), base);
    }

    #[test]
    fn renumber_keeps_one_retain_per_element() {
        let value = CFString::new("cua-driver-renumber-element");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = retain_count(ptr);
        let mut node = button(Some(0), ptr);
        unsafe { CFRetain(ptr as CFTypeRef) }; // what the walk would have left
        let mut owner = CachedSnapshot::from_nodes(std::slice::from_ref(&node));
        node.element_index = Some(9);
        let bounds = LookBounds { max_elements: 1, ..LookBounds::default() };
        owner.renumber(std::slice::from_ref(&node), 10, bounds, Some("s".into()), false);
        assert_eq!(owner.bounds, bounds);
        assert_eq!(owner.elements, vec![(9, ptr)]);
        assert_eq!(retain_count(ptr), base + 1);
        assert!(owner.retain(9).is_some());
        assert!(!owner.full_delivered);
        drop(owner);
        assert_eq!(retain_count(ptr), base);
    }

    #[test]
    fn abandoned_preparation_releases_native_payload_without_replacing_snapshot() {
        let original = CFString::new("cua-driver-original-published-native-work");
        let replacement = CFString::new("cua-driver-abandoned-prepared-native-work");
        let original_ptr = original.as_concrete_TypeRef() as usize;
        let replacement_ptr = replacement.as_concrete_TypeRef() as usize;
        let base = retain_count(replacement_ptr);
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(0, original_ptr));
        let prepared = payload(0, replacement_ptr);
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
        drop(prepared);
        assert_eq!(retain_count(replacement_ptr), base);
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
    }
}
