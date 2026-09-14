pub type ResizeRegistry = cua_driver_core::resize_registry::ResizeRegistry<u32, u64>;

#[cfg(test)]
mod resize_registry_tests {
    use super::*;
    #[test]
    fn sibling_capture_must_not_replace_first_windows_ratio() {
        let registry = ResizeRegistry::new();
        registry.record_capture(10, 101, Some(1200), 600);
        registry.record_capture(10, 102, Some(900), 600);
        assert_eq!(registry.ratio(10, Some(101)), Some(2.0));
        assert_eq!(registry.ratio(10, Some(102)), Some(1.5));
        assert_eq!(registry.ratio(10, None), None);
        assert_eq!(registry.ratio(20, Some(101)), None);
        assert_eq!(registry.ratio(10, Some(103)), None);
        registry.record_capture(10, 102, None, 600);
        assert_eq!(registry.ratio(10, Some(102)), None);
        assert_eq!(registry.ratio(10, Some(101)), Some(2.0));
        assert_eq!(registry.ratio(10, None), None);
        registry.record_capture(10, 101, Some(1800), 600);
        assert_eq!(registry.ratio(10, Some(101)), Some(3.0));
    }
}
