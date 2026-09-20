//! AppKit ownership for the companion. Every entry point runs on its main thread.
use super::observer::{self, Draw, NativeCGImage};
use objc2::{class, msg_send, runtime::AnyObject, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use pip_preview::{session_observer::ObserverSnapshot, PipConfig};
use std::{cell::RefCell, collections::BTreeMap};

thread_local! {
    static UI: RefCell<Panels> = RefCell::new(Panels { cfg: PipConfig::default(), entries: BTreeMap::new(), status: 0 });
}
struct Panel {
    window: usize,
    image: usize,
    label: usize,
    cursor_image: usize,
    cursor: cursor_overlay::RenderStateCore,
    generation: u64,
    order: u64,
    tick: std::time::Instant,
    bounds: Option<cursor_overlay::DisplayBounds>,
}
struct Panels {
    cfg: PipConfig,
    entries: BTreeMap<u64, Panel>,
    status: usize,
}

pub(super) fn configure(cfg: PipConfig) {
    assert!(objc2_foundation::MainThreadMarker::new().is_some());
    UI.with(|ui| ui.borrow_mut().cfg = cfg);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_raster_is_bounded_by_preview_pixels() {
        let source = cursor_overlay::DisplayBounds {
            x: 0.0,
            y: 0.0,
            width: 3840.0,
            height: 2160.0,
        };
        let (width, height, scale) = cursor_dimensions(source, NSSize::new(320.0, 240.0), 2.0);
        assert_eq!((width, height), (640, 360));
        let mut core =
            cursor_overlay::RenderStateCore::new(cursor_overlay::CursorConfig::default());
        core.apply_visual_event(
            cursor_overlay::VisualEvent {
                id: cursor_overlay::VisualActionId {
                    generation: 1,
                    action: 1,
                },
                timestamp: std::time::Instant::now(),
                target: Some((1920.0, 1080.0)),
                window: Some(7),
                bounds: None,
                action: cursor_overlay::CursorAction::Click,
                scroll_direction: None,
                modifiers: None,
                phase: cursor_overlay::VisualPhase::Intent,
            },
            Some(source),
            std::time::Instant::now(),
        );
        let start = std::time::Instant::now();
        for _ in 0..90 {
            std::hint::black_box(cursor_overlay::render_frame(
                &core, width, height, 0.0, 0.0, None, scale,
            ));
        }
        eprintln!(
            "90 preview cursor rasters at {width}x{height}: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn cursor_projects_into_the_same_aspect_fit_image_at_retina_and_resized_sizes() {
        let source = cursor_overlay::DisplayBounds {
            x: -300.0,
            y: 80.0,
            width: 800.0,
            height: 600.0,
        };
        for (w, h, backing) in [
            (400.0, 400.0, 1.0),
            (400.0, 400.0, 2.0),
            (800.0, 240.0, 2.0),
            (240.0, 800.0, 1.0),
        ] {
            let (pw, ph, scale) = cursor_dimensions(source, NSSize::new(w, h), backing);
            let fit = (w / source.width).min(h / source.height);
            assert!((pw as f64 / backing - source.width * fit).abs() < 0.01);
            assert!((ph as f64 / backing - source.height * fit).abs() < 0.01);
            for (x, y) in [
                (source.x, source.y),
                (source.x + 400.0, source.y + 300.0),
                (source.x + 800.0, source.y + 600.0),
            ] {
                let actual = (
                    (w - pw as f64 / backing) / 2.0 + (x - source.x) * f64::from(scale) / backing,
                    (h - ph as f64 / backing) / 2.0 + (y - source.y) * f64::from(scale) / backing,
                );
                let expected = (
                    (w - source.width * fit) / 2.0 + (x - source.x) * fit,
                    (h - source.height * fit) / 2.0 + (y - source.y) * fit,
                );
                assert!(
                    (actual.0 - expected.0).abs() < 0.01 && (actual.1 - expected.1).abs() < 0.01
                );
            }
        }
    }

    #[test]
    fn lone_preview_is_untitled_and_several_count_from_one() {
        assert_eq!(title(0, 1), "Agent");
        assert_eq!(title(0, 3), "Agent 1");
        assert_eq!(title(2, 3), "Agent 3");
    }

    #[test]
    fn geometry_keeps_user_frame_on_available_display() {
        let screen = NSRect::new(NSPoint::new(-1440.0, 0.0), NSSize::new(1440.0, 900.0));
        let user = NSRect::new(NSPoint::new(-1200.0, 100.0), NSSize::new(420.0, 300.0));
        assert_eq!(constrain(user, &[screen]), user);
        let gone = NSRect::new(NSPoint::new(2000.0, 1000.0), NSSize::new(2000.0, 1200.0));
        assert_eq!(constrain(gone, &[screen]), screen);
    }
}

/// Window title. Preview IDs are private, monotonic, and never reused, so a
/// lone panel would otherwise read "Agent 3". Number only when several are live.
fn title(index: usize, total: usize) -> String {
    if total <= 1 {
        "Agent".to_owned()
    } else {
        format!("Agent {}", index + 1)
    }
}

fn constrain(mut frame: NSRect, screens: &[NSRect]) -> NSRect {
    let screen = screens
        .iter()
        .find(|s| {
            frame.origin.x >= s.origin.x
                && frame.origin.y >= s.origin.y
                && frame.origin.x + frame.size.width <= s.origin.x + s.size.width
                && frame.origin.y + frame.size.height <= s.origin.y + s.size.height
        })
        .or_else(|| {
            screens.iter().max_by(|a, b| {
                let overlap = |s: &&NSRect| {
                    ((frame.origin.x + frame.size.width).min(s.origin.x + s.size.width)
                        - frame.origin.x.max(s.origin.x))
                    .max(0.0)
                        * ((frame.origin.y + frame.size.height).min(s.origin.y + s.size.height)
                            - frame.origin.y.max(s.origin.y))
                        .max(0.0)
                };
                overlap(a).total_cmp(&overlap(b))
            })
        });
    if let Some(screen) = screen {
        frame.size.width = frame.size.width.min(screen.size.width);
        frame.size.height = frame.size.height.min(screen.size.height);
        frame.origin.x = frame.origin.x.clamp(
            screen.origin.x,
            screen.origin.x + screen.size.width - frame.size.width,
        );
        frame.origin.y = frame.origin.y.clamp(
            screen.origin.y,
            screen.origin.y + screen.size.height - frame.size.height,
        );
    }
    frame
}

unsafe fn text(value: &str) -> *mut AnyObject {
    let value = std::ffi::CString::new(value).unwrap_or_default();
    msg_send![class!(NSString), stringWithUTF8String: value.as_ptr() as *const u8]
}
unsafe fn screens() -> Vec<NSRect> {
    let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
    let count: usize = msg_send![screens, count];
    (0..count)
        .map(|i| {
            let screen: *mut AnyObject = msg_send![screens, objectAtIndex: i];
            msg_send![screen, visibleFrame]
        })
        .collect()
}
unsafe fn create(id: u64, index: usize, cfg: &PipConfig, screens: &[NSRect]) -> Option<Panel> {
    let screen = screens.first()?;
    let (w, h) = (
        f64::from(cfg.geometry.width),
        f64::from(cfg.geometry.height),
    );
    let stagger = (index % 12) as f64 * 24.0;
    let rect = constrain(
        NSRect::new(
            NSPoint::new(
                screen.origin.x
                    + cfg
                        .geometry
                        .x
                        .map(f64::from)
                        .unwrap_or(screen.size.width - w - 24.0)
                    - stagger,
                screen.origin.y + screen.size.height
                    - cfg.geometry.y.map(f64::from).unwrap_or(24.0)
                    - h
                    - stagger,
            ),
            NSSize::new(w, h),
        ),
        screens,
    );
    let alloc: *mut AnyObject = msg_send![class!(NSPanel), alloc];
    // Titled, closable, resizable, nonactivating. Native controls own geometry.
    let window: *mut AnyObject = msg_send![alloc, initWithContentRect: rect styleMask: (1u64 | 2 | 8 | 128) backing: 2u64 defer: false];
    if window.is_null() {
        return None;
    }
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
    let _: () = msg_send![window, setBecomesKeyOnlyIfNeeded: true];
    let _: () = msg_send![window, setLevel: 3i64];
    let _: () = msg_send![window, setCollectionBehavior: (1u64 | (1 << 8))];
    let _: () = msg_send![window, setTitle: text("Agent")];
    let _: () = msg_send![window, setContentMinSize: NSSize::new(180.0, 120.0)];
    let content: *mut AnyObject = msg_send![window, contentView];
    let color: *mut AnyObject = msg_send![class!(NSColor), colorWithCalibratedHue: ((id as f64 * 0.61803398875) % 1.0) saturation: 0.6f64 brightness: 0.5f64 alpha: 1.0f64];
    let _: () = msg_send![window, setBackgroundColor: color];
    let alloc: *mut AnyObject = msg_send![class!(NSImageView), alloc];
    let image: *mut AnyObject = msg_send![alloc, initWithFrame: NSRect::new(NSPoint::new(0.0, 24.0), NSSize::new(rect.size.width, (rect.size.height - 24.0).max(1.0)))];
    let _: () = msg_send![image, setImageScaling: 3u64];
    let _: () = msg_send![image, setEditable: false];
    let _: () = msg_send![image, setAutoresizingMask: (2u64 | 16)];
    let _: () = msg_send![content, addSubview: image];
    let _: () = msg_send![image, release];
    let alloc: *mut AnyObject = msg_send![class!(NSImageView), alloc];
    let cursor_image: *mut AnyObject = msg_send![alloc, initWithFrame: NSRect::new(NSPoint::new(0.0, 24.0), NSSize::new(rect.size.width, (rect.size.height - 24.0).max(1.0)))];
    let _: () = msg_send![cursor_image, setImageScaling: 3u64];
    let _: () = msg_send![cursor_image, setEditable: false];
    let _: () = msg_send![cursor_image, setAutoresizingMask: (2u64 | 16)];
    let _: () = msg_send![content, addSubview: cursor_image];
    let _: () = msg_send![cursor_image, release];
    let alloc: *mut AnyObject = msg_send![class!(NSTextField), alloc];
    let label: *mut AnyObject = msg_send![alloc, initWithFrame: NSRect::new(NSPoint::new(8.0, 2.0), NSSize::new((rect.size.width - 16.0).max(1.0), 20.0))];
    let _: () = msg_send![label, setBezeled: false];
    let _: () = msg_send![label, setDrawsBackground: false];
    let _: () = msg_send![label, setEditable: false];
    let _: () = msg_send![label, setSelectable: false];
    let _: () = msg_send![label, setAutoresizingMask: 2u64];
    let _: () = msg_send![label, setTextColor: { let c: *mut AnyObject = msg_send![class!(NSColor), whiteColor]; c }];
    let _: () = msg_send![content, addSubview: label];
    let _: () = msg_send![label, release];
    let _: () = msg_send![window, orderFrontRegardless];
    Some(Panel {
        window: window as usize,
        image: image as usize,
        label: label as usize,
        cursor_image: cursor_image as usize,
        cursor: cursor_state(id),
        generation: 0,
        order: 0,
        tick: std::time::Instant::now(),
        bounds: None,
    })
}

unsafe fn menu(ui: &mut Panels) {
    if ui.status == 0 {
        let bar: *mut AnyObject = msg_send![class!(NSStatusBar), systemStatusBar];
        let status: *mut AnyObject = msg_send![bar, statusItemWithLength: -1.0f64];
        let _: () = msg_send![status, retain];
        let button: *mut AnyObject = msg_send![status, button];
        let _: () = msg_send![button, setTitle: text("Previews")];
        ui.status = status as usize;
    }
    let menu: *mut AnyObject = msg_send![class!(NSMenu), new];
    let total = ui.entries.len();
    for (index, panel) in ui.entries.values().enumerate() {
        let alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let item: *mut AnyObject = msg_send![alloc, initWithTitle: text(&format!("Show {}", title(index, total))) action: sel!(orderFront:) keyEquivalent: text("")];
        let _: () = msg_send![item, setTarget: panel.window as *mut AnyObject];
        let _: () = msg_send![menu, addItem: item];
        let _: () = msg_send![item, release];
    }
    let _: () = msg_send![ui.status as *mut AnyObject, setMenu: menu];
    let _: () = msg_send![menu, release];
}

pub(super) unsafe fn render(
    snapshot: &ObserverSnapshot,
    draws: BTreeMap<u64, Draw<screencapturekit::CGImage>>,
) {
    assert!(objc2_foundation::MainThreadMarker::new().is_some());
    UI.with(|cell| {
        let mut ui = cell.borrow_mut();
        let screens = screens();
        let removed: Vec<_> = ui.entries.keys().filter(|id| !snapshot.previews.contains_key(id)).copied().collect();
        let mut changed = !removed.is_empty() || ui.status == 0;
        let mut animate = false;
        // Clear menu targets before releasing their windows.
        if !removed.is_empty() && ui.status != 0 {
            let _: () = msg_send![ui.status as *mut AnyObject, setMenu: std::ptr::null_mut::<AnyObject>()];
        }
        for id in removed {
            let panel = ui.entries.remove(&id).unwrap();
            let _: () = msg_send![panel.window as *mut AnyObject, close];
            let _: () = msg_send![panel.window as *mut AnyObject, release];
        }
        for (id, draw) in draws {
            if !ui.entries.contains_key(&id) {
                if let Some(panel) = create(id, ui.entries.len(), &ui.cfg, &screens) { ui.entries.insert(id, panel); changed = true; }
            }
            let Some(panel) = ui.entries.get_mut(&id) else { continue; };
            let win = panel.window as *mut AnyObject;
            let visible: bool = msg_send![win, isVisible];
            if let Some(state) = observer::presentation().lock().unwrap().entries.get_mut(&id) { state.hidden = !visible; }
            let old: NSRect = msg_send![win, frame];
            let frame = constrain(old, &screens);
            if old != frame { let _: () = msg_send![win, setFrame: frame display: true]; }
            let view = panel.image as *mut AnyObject;
            let valid = observer::current(id, draw.generation);
            if draw.clear || !valid { let _: () = msg_send![view, setImage: std::ptr::null_mut::<AnyObject>()]; }
            if valid {
                if let Some(frame) = draw.frame {
                    let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
                    let image: *mut AnyObject = msg_send![alloc, initWithCGImage: frame.as_ptr() as *mut NativeCGImage size: NSSize::new(0.0, 0.0)];
                    if !image.is_null() { let _: () = msg_send![view, setImage: image]; let _: () = msg_send![image, release]; }
                }
            }
            if !observer::current(id, draw.generation) { let _: () = msg_send![view, setImage: std::ptr::null_mut::<AnyObject>()]; }
            let _: () = msg_send![panel.label as *mut AnyObject, setStringValue: text(if valid { &draw.label } else { "Switching preview" })];
            animate |= render_cursor(id, panel, snapshot.previews.get(&id), if valid && visible { draw.bounds } else { None });
        }
        observer::animate_cursor(animate);
        if changed {
            let total = ui.entries.len();
            for (index, panel) in ui.entries.values().enumerate() {
                let _: () = msg_send![panel.window as *mut AnyObject, setTitle: text(&title(index, total))];
            }
            menu(&mut ui);
        }
    });
}

fn cursor_state(id: u64) -> cursor_overlay::RenderStateCore {
    let reduced = unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduced: bool = msg_send![workspace, accessibilityDisplayShouldReduceMotion];
        reduced
    };
    cursor_overlay::RenderStateCore::new(cursor_overlay::CursorConfig {
        cursor_id: id.to_string(),
        reduced_motion: if reduced {
            cursor_overlay::ReducedMotion::On
        } else {
            cursor_overlay::ReducedMotion::Off
        },
        ..Default::default()
    })
}

unsafe fn render_cursor(
    id: u64,
    panel: &mut Panel,
    update: Option<&pip_preview::observer::ObserverUpdate>,
    bounds: Option<cursor_overlay::DisplayBounds>,
) -> bool {
    let view = panel.cursor_image as *mut AnyObject;
    let Some((update, bounds)) = update.zip(bounds) else {
        if panel.bounds.take().is_some() {
            let _: () = msg_send![view, setImage: std::ptr::null_mut::<AnyObject>()];
        }
        return false;
    };
    let fresh = panel.bounds != Some(bounds)
        || panel.generation != update.generation
        || update.visuals.last().is_some_and(|v| v.order > panel.order);
    panel.bounds = Some(bounds);
    if !fresh && !panel.cursor.cursor_is_revealed() {
        return false;
    }
    if panel.generation != update.generation {
        panel.cursor = cursor_state(id);
        panel.generation = update.generation;
        panel.order = 0;
    }
    let now = std::time::Instant::now();
    for visual in &update.visuals {
        if visual.order > panel.order {
            panel
                .cursor
                .apply_visual_event(visual.event(now), Some(bounds), now);
            panel.order = visual.order;
        }
    }
    let dt = if fresh {
        0.0
    } else {
        now.duration_since(panel.tick).as_secs_f64()
    };
    panel.tick = now;
    panel.cursor.tick_motion_at(dt, now);
    // Both NSImageViews aspect-fit the identical source rectangle, including
    // letterboxing and backing-scale changes during native panel resizing.
    let viewport: NSRect = msg_send![view, bounds];
    let backing: f64 = msg_send![panel.window as *mut AnyObject, backingScaleFactor];
    let (width, height, scale) = cursor_dimensions(bounds, viewport.size, backing);
    let pixmap = cursor_overlay::render_frame(
        &panel.cursor,
        width,
        height,
        bounds.x,
        bounds.y,
        None,
        scale,
    );
    let Some(cg) = crate::cursor::overlay::pixmap_to_cgimage(pixmap) else {
        return false;
    };
    let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
    let image: *mut AnyObject =
        msg_send![alloc, initWithCGImage: cg as *mut NativeCGImage size: NSSize::new(0.0, 0.0)];
    if !image.is_null() {
        let _: () = msg_send![view, setImage: if observer::current(id, update.generation) { image } else { std::ptr::null_mut::<AnyObject>() }];
        let _: () = msg_send![image, release];
    }
    extern "C" {
        fn CGImageRelease(image: *mut std::ffi::c_void);
    }
    CGImageRelease(cg as *mut std::ffi::c_void);
    panel.cursor.cursor_is_revealed()
}

fn cursor_dimensions(
    source: cursor_overlay::DisplayBounds,
    viewport: NSSize,
    backing: f64,
) -> (u32, u32, f32) {
    let scale = (viewport.width / source.width).min(viewport.height / source.height) * backing;
    let scale = scale.min(4096.0 / source.width.max(source.height));
    (
        (source.width * scale).round().max(1.0) as u32,
        (source.height * scale).round().max(1.0) as u32,
        scale as f32,
    )
}
