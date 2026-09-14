// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Pure screen geometry shared by platform adapters.

/// Resolve logical screen and window-local coordinates only for a positive,
/// finite element rectangle contained by the current window rectangle.
pub fn contained_element_center(
    element: [f64; 4],
    window: [f64; 4],
) -> Option<(f64, f64, f64, f64)> {
    if !element.iter().chain(window.iter()).all(|v| v.is_finite()) {
        return None;
    }
    let [x, y, width, height] = element;
    let [wx, wy, ww, wh] = window;
    let edges = [x + width, y + height, wx + ww, wy + wh];
    if width <= 0.0
        || height <= 0.0
        || ww <= 0.0
        || wh <= 0.0
        || !edges.iter().all(|v| v.is_finite())
        || x < wx
        || y < wy
        || edges[0] > edges[2]
        || edges[1] > edges[3]
    {
        return None;
    }
    let cx = x + width / 2.0;
    let cy = y + height / 2.0;
    Some((cx, cy, cx - wx, cy - wy))
}

#[cfg(test)]
mod element_center_tests {
    use super::contained_element_center;

    #[test]
    fn contained_center_keeps_logical_points_on_negative_origin_display() {
        assert_eq!(
            contained_element_center(
                [-900.0, -400.0, 300.0, 200.0],
                [-1000.0, -500.0, 800.0, 600.0]
            ),
            Some((-750.0, -300.0, 250.0, 200.0))
        );
        assert_eq!(
            contained_element_center([100.0, 200.0, 600.0, 300.0], [100.0, 200.0, 600.0, 300.0]),
            Some((400.0, 350.0, 300.0, 150.0))
        );
    }

    #[test]
    fn invalid_or_partially_clipped_rectangles_never_choose_a_wheel_point() {
        let window = [100.0, 200.0, 600.0, 300.0];
        for element in [
            [90.0, 220.0, 100.0, 100.0],
            [110.0, 190.0, 100.0, 100.0],
            [650.0, 220.0, 100.0, 100.0],
            [110.0, 450.0, 100.0, 100.0],
            [110.0, 220.0, 0.0, 100.0],
            [110.0, 220.0, 100.0, -1.0],
            [f64::NAN, 220.0, 100.0, 100.0],
            [110.0, 220.0, f64::INFINITY, 100.0],
        ] {
            assert_eq!(
                contained_element_center(element, window),
                None,
                "{element:?}"
            );
        }
        for invalid_window in [
            [100.0, 200.0, 0.0, 300.0],
            [100.0, 200.0, 600.0, f64::NAN],
            [f64::MAX, 200.0, f64::MAX, 300.0],
        ] {
            assert_eq!(
                contained_element_center([110.0, 220.0, 100.0, 100.0], invalid_window),
                None
            );
        }
    }
}
