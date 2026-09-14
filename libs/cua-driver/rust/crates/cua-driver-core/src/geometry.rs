/// Converts screenshot pixels to screen coordinates without rounding or clamping.
///
/// Adapters must validate finite coordinates, finite positive resize and capture
/// scales, and a finite output before dispatch.
pub fn screenshot_to_screen(
    pixel: (f64, f64),
    resize_ratio: f64,
    capture_scale: f64,
    window_origin: (f64, f64),
) -> (f64, f64) {
    (
        window_origin.0 + pixel.0 * resize_ratio / capture_scale,
        window_origin.1 + pixel.1 * resize_ratio / capture_scale,
    )
}

#[cfg(test)]
mod tests {
    use super::screenshot_to_screen;

    #[test]
    fn screenshot_coordinates_map_to_screen_coordinates() {
        let cases = [
            ((30.0, 40.0), 1.0, 1.0, (100.0, 580.0), (130.0, 620.0)),
            ((60.0, 80.0), 1.0, 2.0, (100.0, 580.0), (130.0, 620.0)),
            ((30.0, 40.0), 2.0, 2.0, (100.0, 580.0), (130.0, 620.0)),
            ((30.0, 40.0), 2.0, 2.0, (500.0, 100.0), (530.0, 140.0)),
            ((60.0, 80.0), 1.0, 2.0, (-1440.0, -900.0), (-1410.0, -860.0)),
            ((3.0, 5.0), 1.5, 2.0, (-10.0, 20.0), (-7.75, 23.75)),
        ];

        for (pixel, resize_ratio, capture_scale, window_origin, expected) in cases {
            assert_eq!(
                screenshot_to_screen(pixel, resize_ratio, capture_scale, window_origin),
                expected,
                "pixel={pixel:?}, resize_ratio={resize_ratio}, capture_scale={capture_scale}, window_origin={window_origin:?}"
            );
        }
    }
}
