//! Per-window screenshot downscale observations shared by native adapters.
use std::{collections::HashMap, hash::Hash, sync::Mutex};

pub struct ResizeRegistry<P, W> {
    inner: Mutex<HashMap<(P, W), Option<f64>>>,
}

impl<P: Copy + Eq + Hash, W: Copy + Eq + Hash> Default for ResizeRegistry<P, W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Copy + Eq + Hash, W: Copy + Eq + Hash> ResizeRegistry<P, W> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
    pub fn set_ratio(&self, pid: P, window_id: W, ratio: f64) {
        self.inner
            .lock()
            .unwrap()
            .insert((pid, window_id), Some(ratio));
    }
    pub fn clear_ratio(&self, pid: P, window_id: W) {
        // Keep the observation: an unresized sibling makes PID-only downscaling ambiguous.
        self.inner.lock().unwrap().insert((pid, window_id), None);
    }
    /// Record a successful capture, replacing its previous resize observation.
    pub fn record_capture(&self, pid: P, window_id: W, original_width: Option<u32>, width: u32) {
        if let Some(original) = original_width.filter(|_| width > 0) {
            self.set_ratio(pid, window_id, original as f64 / width as f64);
        } else {
            self.clear_ratio(pid, window_id);
        }
    }
    pub fn ratio(&self, pid: P, window_id: Option<W>) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        if let Some(wid) = window_id {
            return inner.get(&(pid, wid)).copied().flatten();
        }
        let mut agreed: Option<f64> = None;
        for (_, ratio) in inner.iter().filter(|((p, _), _)| *p == pid) {
            let ratio = (*ratio)?;
            match agreed {
                None => agreed = Some(ratio),
                Some(seen) if (seen - ratio).abs() < 1e-9 => {}
                Some(_) => return None,
            }
        }
        agreed
    }
}

#[cfg(test)]
mod resize_registry_tests {
    use super::*;
    #[test]
    fn exact_windows_and_pids_are_isolated() {
        let r = ResizeRegistry::new();
        r.set_ratio(10, 101, 2.0);
        r.set_ratio(10, 102, 1.5);
        r.set_ratio(20, 101, 3.0);
        assert_eq!(r.ratio(10, Some(101)), Some(2.0));
        assert_eq!(r.ratio(10, Some(102)), Some(1.5));
        assert_eq!(r.ratio(20, Some(101)), Some(3.0));
        assert_eq!(r.ratio(30, Some(101)), None);
        assert_eq!(r.ratio(10, Some(103)), None);
        assert_eq!(r.ratio(10, None), None);
        r.clear_ratio(10, 102);
        assert_eq!(r.ratio(10, Some(101)), Some(2.0));
        assert_eq!(r.ratio(10, Some(102)), None);
        assert_eq!(r.ratio(10, None), None);
    }
    #[test]
    fn producer_replaces_and_clears_without_forgetting_siblings() {
        let r = ResizeRegistry::new();
        r.record_capture(10, 101, Some(1200), 600);
        assert_eq!(r.ratio(10, None), Some(2.0));
        r.record_capture(10, 102, None, 600);
        assert_eq!(r.ratio(10, None), None);
        r.record_capture(10, 102, Some(900), 600);
        assert_eq!(r.ratio(10, Some(102)), Some(1.5));
        r.record_capture(10, 101, Some(900), 600);
        assert_eq!(r.ratio(10, Some(101)), Some(1.5));
        assert_eq!(r.ratio(10, None), Some(1.5));
        r.record_capture(10, 102, None, 600);
        assert_eq!(r.ratio(10, Some(102)), None);
        assert_eq!(r.ratio(10, None), None);
        r.record_capture(10, 101, Some(900), 0);
        assert_eq!(r.ratio(10, Some(101)), None);
    }
}
