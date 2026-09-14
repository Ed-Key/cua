//! Capture bitmap geometry in physical desktop pixels, independent of Win32.
use crate::resize_registry::ResizeRegistry;
use cua_driver_core::{protocol::ToolResult, tool_args::ArgsExt};
use cursor_overlay::CursorRegistry;
use serde_json::Value;

pub type Rect = (i32, i32, i32, i32);

pub fn bitmap_origin(
    dwm: Option<Rect>,
    window_rect: Option<Rect>,
) -> Result<(f64, f64), ToolResult> {
    let (rect, inset) = match (dwm, window_rect) {
        (Some(rect), _) => (rect, 1.0),
        (None, Some(rect)) => (rect, 0.0),
        (None, None) => {
            return Err(ToolResult::error(
                "window capture origin unavailable: DWM and GetWindowRect failed",
            ))
        }
    };
    let (left, top, right, bottom) = rect;
    if right as f64 - left as f64 <= 2.0 * inset || bottom as f64 - top as f64 <= 2.0 * inset {
        return Err(ToolResult::error("window capture geometry is empty"));
    }
    Ok((left as f64 + inset, top as f64 + inset))
}

pub async fn move_overlay<F, P, V>(
    args: &Value,
    key: String,
    resize: &ResizeRegistry,
    cursors: &CursorRegistry,
    resolve: F,
    publish: P,
) -> ToolResult
where
    F: FnOnce(u32, u64) -> Result<(f64, f64), ToolResult>,
    P: FnOnce(String, f64, f64) -> V,
    V: std::future::Future<Output = ()>,
{
    let resolved = (|| {
        let (x, y) = (args.require_f64("x")?, args.require_f64("y")?);
        if !x.is_finite() || !y.is_finite() {
            return Err(ToolResult::error("cursor coordinates must be finite"));
        }
        if args.get("pid").is_none() && args.get("window_id").is_none() {
            return Ok((x, y));
        }
        let pid = args
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0);
        let wid = args
            .get("window_id")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0);
        let (Some(pid), Some(wid)) = (pid, wid) else {
            return Err(ToolResult::error(
                "window target requires a positive 32-bit PID and window_id",
            ));
        };
        let origin = resolve(pid, wid)?;
        let ratio = resize.ratio(pid, Some(wid)).unwrap_or(1.0);
        if !ratio.is_finite() || ratio <= 0.0 {
            return Err(ToolResult::error(
                "screenshot resize ratio must be finite and positive",
            ));
        }
        let point = cua_driver_core::geometry::screenshot_to_screen((x, y), ratio, 1.0, origin);
        if !point.0.is_finite() || !point.1.is_finite() {
            return Err(ToolResult::error(
                "window coordinates do not resolve to a finite screen point",
            ));
        }
        Ok(point)
    })();
    let (x, y) = match resolved {
        Ok(point) => point,
        Err(error) => return error,
    };
    cursors.update_position(&key, x, y);
    publish(key.clone(), x, y).await;
    ToolResult::text(format!("Agent cursor '{key}' moved to ({x:.1}, {y:.1})."))
}

#[cfg(test)]
mod move_cursor_geometry_tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    #[test]
    fn capture_origin_inset_full_frame_and_failed_queries() {
        assert_eq!(
            bitmap_origin(Some((100, 200, 600, 700)), None).unwrap(),
            (101.0, 201.0)
        );
        assert_eq!(
            bitmap_origin(None, Some((-500, -300, 0, 200))).unwrap(),
            (-500.0, -300.0)
        );
        assert!(bitmap_origin(None, None).is_err());
        assert!(bitmap_origin(Some((0, 0, 0, 0)), None).is_err());
        assert!(bitmap_origin(None, Some((10, 20, 10, 40))).is_err());
    }

    #[tokio::test]
    async fn exact_window_move_keeps_fractions_and_sibling_ratios() {
        let resize = ResizeRegistry::new();
        let cursors = CursorRegistry::new();
        resize.record_capture(10, 101, Some(1200), 600);
        resize.record_capture(10, 102, Some(900), 600);
        for (wid, origin, want) in [
            (101, (101.0, 201.0), (161.5, 281.5)),
            (102, (101.0, 201.0), (146.375, 261.375)),
            (101, (-999.0, -499.0), (-938.5, -418.5)),
        ] {
            let published = Cell::new(None);
            let result = move_overlay(
                &json!({"pid":10,"window_id":wid,"x":30.25,"y":40.25}),
                "test".into(),
                &resize,
                &cursors,
                |pid, id| {
                    assert_eq!((pid, id), (10, wid));
                    Ok(origin)
                },
                |_, x, y| {
                    published.set(Some((x, y)));
                    async {}
                },
            )
            .await;
            assert_ne!(result.is_error, Some(true));
            let state = cursors.get("test").unwrap();
            assert_eq!((state.x, state.y), (Some(want.0), Some(want.1)));
            assert_eq!(published.get(), Some(want));
        }
    }

    #[tokio::test]
    async fn refused_target_never_publishes_or_changes_registry() {
        let resize = ResizeRegistry::new();
        let cursors = CursorRegistry::new();
        cursors.update_position("test", 7.0, 9.0);
        for args in [
            json!({"pid":10,"window_id":101,"x":3,"y":4}),
            json!({"pid":10,"x":3,"y":4}),
            json!({"pid":4294967296u64,"window_id":101,"x":3,"y":4}),
        ] {
            let published = Cell::new(0);
            let result = move_overlay(
                &args,
                "test".into(),
                &resize,
                &cursors,
                |_, _| Err(ToolResult::error("missing geometry or foreign owner")),
                |_, _, _| {
                    published.set(published.get() + 1);
                    async {}
                },
            )
            .await;
            assert_eq!(result.is_error, Some(true));
            assert_eq!(published.get(), 0);
            let state = cursors.get("test").unwrap();
            assert_eq!((state.x, state.y), (Some(7.0), Some(9.0)));
        }
    }

    #[tokio::test]
    async fn invalid_ratio_or_overflow_preserves_existing_and_absent_cursors() {
        for ratio in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            let resize = ResizeRegistry::new();
            resize.set_ratio(10, 101, ratio);
            let cursors = CursorRegistry::new();
            cursors.update_position("existing", 7.0, 9.0);
            for key in ["existing", "absent"] {
                let result = move_overlay(
                    &json!({"pid":10,"window_id":101,"x":30.25,"y":40.25}),
                    key.into(),
                    &resize,
                    &cursors,
                    |_, _| Ok((101.0, 201.0)),
                    |_, _, _| async { panic!("refused move published") },
                )
                .await;
                assert_eq!(result.is_error, Some(true));
                assert!(cursors.get("absent").is_none());
                let old = cursors.get("existing").unwrap();
                assert_eq!((old.x, old.y), (Some(7.0), Some(9.0)));
            }
        }
    }

    #[tokio::test]
    async fn legacy_move_uses_screen_points_without_lookup() {
        let result = move_overlay(
            &json!({"x":-3.25,"y":4.5}),
            "legacy".into(),
            &ResizeRegistry::new(),
            &CursorRegistry::new(),
            |_, _| panic!("legacy lookup"),
            |_, x, y| {
                assert_eq!((x, y), (-3.25, 4.5));
                async {}
            },
        )
        .await;
        assert_ne!(result.is_error, Some(true));
    }
}
