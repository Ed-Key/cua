//! Pure macOS display geometry for agent-cursor presentation.
//!
//! Cua cursor coordinates use CoreGraphics' global top-left coordinate space.
//! AppKit window frames use a bottom-left origin. Keeping both frames here
//! prevents either convention from leaking into cursor state or rendering.

use objc2_foundation::{NSPoint, NSRect, NSSize};

pub(crate) type DisplayId = u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayGeometry {
    pub id: DisplayId,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub backing_scale: f64,
    pub is_primary: bool,
}

impl DisplayGeometry {
    pub(crate) fn contains(self, x: f64, y: f64) -> bool {
        cursor_overlay::DisplayBounds {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
        .contains((x, y))
    }

    pub(crate) fn pixel_size(self) -> (u32, u32) {
        let scale = self.backing_scale.max(1.0);
        (
            (self.width * scale).round().max(1.0) as u32,
            (self.height * scale).round().max(1.0) as u32,
        )
    }

    /// Convert the CoreGraphics top-left frame into AppKit's bottom-left frame.
    pub(crate) fn appkit_frame(self, primary_height: f64) -> NSRect {
        NSRect::new(
            NSPoint::new(self.x, primary_height - self.y - self.height),
            NSSize::new(self.width, self.height),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DisplayLayout {
    pub generation: u64,
    pub displays: Vec<DisplayGeometry>,
}

impl DisplayLayout {
    pub(crate) fn display_at(&self, x: f64, y: f64) -> Option<DisplayGeometry> {
        self.displays
            .iter()
            .copied()
            .find(|display| display.contains(x, y))
    }

    pub(crate) fn primary_height(&self) -> Option<f64> {
        self.displays
            .iter()
            .find(|display| display.is_primary)
            .or_else(|| self.displays.first())
            .map(|display| display.height)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn active_layout(generation: u64) -> Result<DisplayLayout, i32> {
    use core_graphics::display::CGDisplay;

    let primary_id = CGDisplay::main().id;
    let mut displays = CGDisplay::active_displays()?
        .into_iter()
        .filter_map(|id| {
            let display = CGDisplay::new(id);
            // A mirrored destination shares its source's coordinate space and
            // already receives that source window through system mirroring.
            if display.mirrors_display() != 0 {
                return None;
            }
            let bounds = display.bounds();
            let width = bounds.size.width;
            let height = bounds.size.height;
            if !(cursor_overlay::DisplayBounds {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width,
                height,
            })
            .contains((bounds.origin.x, bounds.origin.y))
            {
                return None;
            }
            let scale = crate::tools::get_screen_size::get_backing_scale(id);
            Some(DisplayGeometry {
                id,
                x: bounds.origin.x,
                y: bounds.origin.y,
                width,
                height,
                backing_scale: if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    1.0
                },
                is_primary: id == primary_id,
            })
        })
        .collect::<Vec<_>>();
    displays.sort_by_key(|display| (!display.is_primary, display.id));
    Ok(DisplayLayout {
        generation,
        displays,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> DisplayLayout {
        DisplayLayout {
            generation: 7,
            displays: vec![
                DisplayGeometry {
                    id: 1,
                    x: 0.0,
                    y: 0.0,
                    width: 1440.0,
                    height: 900.0,
                    backing_scale: 2.0,
                    is_primary: true,
                },
                DisplayGeometry {
                    id: 2,
                    x: -1920.0,
                    y: -180.0,
                    width: 1920.0,
                    height: 1080.0,
                    backing_scale: 1.0,
                    is_primary: false,
                },
                DisplayGeometry {
                    id: 3,
                    x: 0.0,
                    y: -1200.0,
                    width: 1200.0,
                    height: 1200.0,
                    backing_scale: 1.5,
                    is_primary: false,
                },
            ],
        }
    }

    #[test]
    fn slice_a_invalid_geometry_and_targets_have_no_containing_display() {
        let valid = layout().displays[0];
        for display in [
            DisplayGeometry {
                width: f64::INFINITY,
                ..valid
            },
            DisplayGeometry {
                height: 0.0,
                ..valid
            },
            DisplayGeometry {
                x: f64::NAN,
                ..valid
            },
            DisplayGeometry {
                y: f64::NEG_INFINITY,
                height: f64::INFINITY,
                ..valid
            },
        ] {
            let layout = DisplayLayout {
                generation: 1,
                displays: vec![display],
            };
            assert!(layout.display_at(50.0, 50.0).is_none());
        }
        assert!(layout().display_at(3000.0, 3000.0).is_none());
        assert!(layout().display_at(f64::INFINITY, 50.0).is_none());
    }

    #[test]
    fn slice_a_requested_origins_project_appkit_frames_and_native_sizes() {
        for (x, y, scale, want_y, want_size) in [
            (0.0, 0.0, 2.0, 82.0, (2880, 1800)),
            (-1440.0, 0.0, 1.0, 82.0, (1440, 900)),
            (0.0, -900.0, 2.0, 982.0, (2880, 1800)),
        ] {
            let d = DisplayGeometry {
                id: 1,
                x,
                y,
                width: 1440.0,
                height: 900.0,
                backing_scale: scale,
                is_primary: false,
            };
            assert_eq!(d.appkit_frame(982.0).origin, NSPoint::new(x, want_y));
            assert_eq!(d.pixel_size(), want_size);
        }
    }

    #[test]
    fn routes_negative_axes_to_their_displays() {
        let layout = layout();
        assert_eq!(layout.display_at(-867.0, 400.0).unwrap().id, 2);
        assert_eq!(layout.display_at(300.0, -867.0).unwrap().id, 3);
    }

    #[test]
    fn seams_are_half_open_and_owned_once() {
        let layout = layout();
        assert_eq!(layout.display_at(-0.001, 100.0).unwrap().id, 2);
        assert_eq!(layout.display_at(0.0, 100.0).unwrap().id, 1);
    }

    #[test]
    fn each_display_keeps_its_own_pixel_scale() {
        let layout = layout();
        let pixel_sizes = layout
            .displays
            .iter()
            .map(|display| (display.id, display.pixel_size()))
            .collect::<Vec<_>>();
        assert_eq!(
            pixel_sizes,
            vec![(1, (2880, 1800)), (2, (1920, 1080)), (3, (1800, 1800))]
        );
    }

    #[test]
    fn appkit_frame_flips_global_y_around_the_primary_display() {
        let display = layout().displays[2];
        let frame = display.appkit_frame(900.0);
        assert_eq!(frame.origin.x, 0.0);
        assert_eq!(frame.origin.y, 900.0);
        assert_eq!(frame.size.width, 1200.0);
        assert_eq!(frame.size.height, 1200.0);
    }
}
