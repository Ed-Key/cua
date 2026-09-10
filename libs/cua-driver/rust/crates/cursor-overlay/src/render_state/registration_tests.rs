//! Pixel oracles deliberately do not call registration geometry or readiness.
use super::*;

fn intent(now: Instant) -> VisualEvent {
    VisualEvent {
        id: VisualActionId {
            generation: 1,
            action: 1,
        },
        timestamp: now,
        target: Some((80.25, 70.75)),
        window: Some(42),
        bounds: None,
        action: CursorAction::Click,
        phase: VisualPhase::Intent,
        scroll_direction: None,
        modifiers: None,
    }
}
fn bounds() -> Option<DisplayBounds> {
    Some(DisplayBounds {
        x: -200.0,
        y: -200.0,
        width: 600.0,
        height: 600.0,
    })
}
fn reached(reduced: crate::ReducedMotion) -> (RenderStateCore, VisualEvent) {
    let event = intent(Instant::now());
    let mut core = RenderStateCore::new(CursorConfig::default());
    core.visual.reduced_motion = reduced;
    assert!(core.apply_click_approach(event.clone(), bounds(), event.timestamp));
    core.tick_swift_constants_at(0.14, event.timestamp + Duration::from_millis(140));
    assert!(core.path.is_none());
    (core, event)
}
fn raster(core: &RenderStateCore, scale: f32) -> tiny_skia::Pixmap {
    let mut pm = tiny_skia::Pixmap::new((256.0 * scale) as u32, (256.0 * scale) as u32).unwrap();
    paint_cursor_art(&mut pm, core, 0.0, 0.0, None, scale);
    pm
}
fn opaque_near(pm: &tiny_skia::Pixmap, point: (f64, f64), radius: f64) -> bool {
    let left = (point.0 - radius).floor().max(0.0) as u32;
    let top = (point.1 - radius).floor().max(0.0) as u32;
    let right = ((point.0 + radius).ceil().max(0.0) as u32).min(pm.width());
    let bottom = ((point.1 + radius).ceil().max(0.0) as u32).min(pm.height());
    (top..bottom).any(|y| {
        (left..right).any(|x| {
            pm.pixel(x, y).unwrap().alpha() >= 250
                && (x as f64 + 0.5 - point.0).hypot(y as f64 + 0.5 - point.1) <= radius
        })
    })
}

// The largest opaque connected component is the arrow body plus its outline.
// Glow and the separate white action marks must never certify arrival.
fn body_only(pm: &mut tiny_skia::Pixmap) {
    let w = pm.width() as usize;
    let mut remaining: std::collections::HashSet<usize> = pm
        .data()
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, p)| p[3] >= 250)
        .map(|(i, _)| i)
        .collect();
    let mut largest = Vec::new();
    while let Some(&start) = remaining.iter().next() {
        remaining.remove(&start);
        let mut component = vec![start];
        let mut cursor = 0;
        while cursor < component.len() {
            let i = component[cursor];
            for neighbor in [
                i.checked_sub(w),
                i.checked_add(w),
                (i % w > 0).then(|| i - 1),
                (i % w + 1 < w).then_some(i + 1),
            ]
            .into_iter()
            .flatten()
            {
                if remaining.remove(&neighbor) {
                    component.push(neighbor);
                }
            }
            cursor += 1;
        }
        if component.len() > largest.len() {
            largest = component;
        }
    }
    assert!(largest.len() > 80, "missing actual opaque arrow body");
    let body: std::collections::HashSet<_> = largest.into_iter().collect();
    for (i, p) in pm.data_mut().chunks_exact_mut(4).enumerate() {
        if p[3] < 250 || !body.contains(&i) {
            p.fill(0);
        }
    }
}
fn assert_painted_tip(core: &RenderStateCore, target: (f64, f64), scale: f32) {
    // Local crop follows the independently supplied expected point, including
    // negative-origin travel. It does not assume the path starts on display 0.
    let mut pm = tiny_skia::Pixmap::new((128.0 * scale) as u32, (128.0 * scale) as u32).unwrap();
    paint_cursor_art(&mut pm, core, target.0 - 64.0, target.1 - 64.0, None, scale);
    body_only(&mut pm);
    let target = (64.0 * scale as f64, 64.0 * scale as f64);
    assert!(
        opaque_near(&pm, target, 2.0),
        "no opaque body/outline within two native pixels: scale={scale}, phase={:?}, elapsed={}",
        core.visual.resolved_action,
        core.visual.elapsed_secs
    );
    // Independently define the rounded nose as the NW support of the body.
    // At rest body rotation is zero; its only outer rotation is the authored float.
    let float = if core.visual.reduced_motion == crate::ReducedMotion::On {
        0.0
    } else {
        2.5_f64.to_radians() * (core.visual.elapsed_secs * std::f64::consts::TAU / 4.0).cos()
    };
    let angle = core.heading - std::f64::consts::FRAC_PI_4 + float;
    let inward = (
        (angle + std::f64::consts::FRAC_PI_4).cos(),
        (angle + std::f64::consts::FRAC_PI_4).sin(),
    );
    assert!(
        !opaque_near(
            &pm,
            (target.0 - inward.0 * 3.0, target.1 - inward.1 * 3.0),
            1.0
        ),
        "target must be the outline boundary, not body interior"
    );
    assert!(
        opaque_near(
            &pm,
            (target.0 + inward.0 * 3.0, target.1 + inward.1 * 3.0),
            1.5
        ),
        "opaque body must follow the registered nose"
    );
}

#[test]
fn registered_default_reached_and_every_idle_click_frame_has_actual_painted_tip() {
    for reduced in [crate::ReducedMotion::Off, crate::ReducedMotion::On] {
        let (mut core, event) = reached(reduced);
        for scale in [1.0, 2.0] {
            assert_painted_tip(&core, event.target.unwrap(), scale);
            for action in [
                CursorAction::Idle,
                CursorAction::Click,
                CursorAction::Navigate,
            ] {
                core.visual.begin(action, None, None);
                for frame in 0..120 {
                    if reduced == crate::ReducedMotion::On && ![0, 29, 59, 119].contains(&frame) {
                        continue;
                    }
                    core.visual.elapsed_secs = frame as f64 / 30.0;
                    assert_painted_tip(&core, event.target.unwrap(), scale);
                }
            }
        }
    }
}

#[test]
fn registered_contact_idle_and_pulse_expiry_keep_tip_and_ring_independent() {
    for reduced in [crate::ReducedMotion::Off, crate::ReducedMotion::On] {
        for scale in [1.0, 2.0] {
            let (mut core, mut event) = reached(reduced);
            core.tick_swift_constants_at(0.05, event.timestamp + Duration::from_millis(190));
            assert_painted_tip(&core, event.target.unwrap(), scale);
            event.phase = VisualPhase::Contact;
            event.timestamp += Duration::from_millis(200);
            assert!(core.apply_visual_event(event.clone(), bounds(), event.timestamp));
            let mut previous = 0;
            for millis in [0, 33, 67, 100, 133, 149, 150, 200, 1000] {
                core.tick_swift_constants_at(
                    (millis - previous) as f64 / 1000.0,
                    event.timestamp + Duration::from_millis(millis),
                );
                previous = millis;
                assert_painted_tip(&core, event.target.unwrap(), scale);
                if let Some(contact) = core.contact {
                    assert_eq!(contact.target, event.target.unwrap());
                    let pm = raster(&core, scale);
                    let r = (12.0 + 20.0 * contact.progress) * scale as f64;
                    // Three unobscured cardinal samples locate the ring independently.
                    for (dx, dy) in [(-1.0, 0.0), (0.0, -1.0), (1.0, 0.0)] {
                        let (tx, ty) = contact.target;
                        let (x, y) = (
                            (tx * scale as f64 + dx * r).floor() as u32,
                            (ty * scale as f64 + dy * r).floor() as u32,
                        );
                        let pixel = pm.pixel(x, y).unwrap();
                        assert!(pixel.alpha() > 0, "missing ring at independent radius");
                    }
                }
            }
            assert!(core.contact.is_none());
        }
    }
}

#[test]
fn registered_rotation_and_live_glide_paint_the_moving_tip_without_a_second_wait() {
    for scale in [1.0, 2.0] {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.visual.reduced_motion = crate::ReducedMotion::Off;
        let event = intent(Instant::now());
        assert!(core.apply_click_approach(event.clone(), bounds(), event.timestamp));
        let mut positions = Vec::new();
        for millis in [0, 20, 40, 60, 80, 100, 120, 140] {
            core.tick_swift_constants_at(0.02, event.timestamp + Duration::from_millis(millis));
            let current = (
                core.pos.0 - 16.0 * core.heading.cos(),
                core.pos.1 - 16.0 * core.heading.sin(),
            );
            assert_painted_tip(&core, current, scale);
            positions.push(current);
        }
        assert!(
            positions
                .windows(2)
                .filter(|pair| pair[0] != pair[1])
                .count()
                >= 3
        );
        assert!(core.path.is_none());
        assert!(core.is_target_frame(&event));
        for heading in [0.0, 0.4, 1.2, 2.8, 4.7, 6.2] {
            core.heading = heading;
            core.visual.elapsed_secs = 0.233;
            let current = (
                core.pos.0 - 16.0 * heading.cos(),
                core.pos.1 - 16.0 * heading.sin(),
            );
            assert_painted_tip(&core, current, scale);
        }
    }
}

fn assert_legacy_pixels(core: &RenderStateCore) {
    for scale in [1.0, 2.0] {
        let actual = raster(core, scale);
        let mut expected = tiny_skia::Pixmap::new(actual.width(), actual.height()).unwrap();
        let theme = core.theme.as_deref().unwrap();
        crate::paint_compiled_theme_with_tint(
            &mut expected,
            theme,
            &core.visual,
            core.pos.0 as f32 * scale,
            core.pos.1 as f32 * scale,
            core.heading as f32,
            scale,
            core.idle_alpha as f32,
            (theme.id == crate::DEFAULT_THEME_ID)
                .then(|| crate::session_fill_rgba(&core.cfg.cursor_id)),
        );
        assert_eq!(
            actual.data(),
            expected.data(),
            "non-opted-in rendering changed"
        );
    }
}

#[test]
fn registered_cleanup_theme_changes_and_session_ownership_do_not_leak_registration() {
    for cleanup in [
        "clear",
        "end",
        "disable",
        "theme",
        "legacy",
        "new_owner",
        "begin_action",
    ] {
        let (mut core, event) = reached(crate::ReducedMotion::On);
        assert!(core.is_target_frame(&event));
        match cleanup {
            "clear" => core.clear_visual_presentation(),
            "end" => {
                let mut end = event.clone();
                end.phase = VisualPhase::End;
                assert!(core.apply_visual_event(end, bounds(), event.timestamp));
            }
            "disable" => {
                core.apply_command_base(OverlayCommand::SetEnabled(false), true, false);
            }
            "theme" => {
                core.apply_command_base(
                    OverlayCommand::SetTheme {
                        theme_id: crate::DEFAULT_THEME_ID.into(),
                        reduced_motion: crate::ReducedMotion::On,
                    },
                    true,
                    false,
                );
            }
            "legacy" => {
                core.apply_command_base(crate::track_pointer_command(80.25, 70.75), true, false);
            }
            "new_owner" => {
                let mut next = event.clone();
                next.id.action += 1;
                next.action = CursorAction::Scroll;
                assert!(core.apply_visual_event(next, bounds(), event.timestamp));
            }
            _ => {
                core.apply_command_base(
                    OverlayCommand::BeginAction {
                        action: CursorAction::Text,
                        delivery: None,
                        target: None,
                    },
                    true,
                    false,
                );
            }
        }
        assert!(
            !core.is_target_frame(&event),
            "{cleanup} retained readiness"
        );
        assert!(!core.registered_target, "{cleanup} retained registration");
        if cleanup == "disable" {
            assert!(raster(&core, 2.0).data().iter().all(|b| *b == 0));
        } else {
            assert_legacy_pixels(&core);
        }
        let (other, other_event) = reached(crate::ReducedMotion::On);
        assert!(other.is_target_frame(&other_event));
        assert_painted_tip(&other, other_event.target.unwrap(), 2.0);
    }
}

#[test]
fn registered_legacy_and_custom_theme_rendering_remain_byte_identical() {
    for custom in [false, true] {
        for action in [
            CursorAction::Idle,
            CursorAction::Navigate,
            CursorAction::Scroll,
            CursorAction::Text,
            CursorAction::Drag,
        ] {
            let mut core = RenderStateCore::new(CursorConfig::default());
            core.apply_command_base(crate::track_pointer_command(80.25, 70.75), true, false);
            if custom {
                let mut theme = (*crate::embedded_default_theme()).clone();
                theme.id = "test.custom".into();
                theme.hotspot = [1, 127];
                core.theme = Some(Arc::new(theme));
            }
            core.visual.begin(action, None, None);
            core.visual.elapsed_secs = 0.233;
            assert_legacy_pixels(&core);
            assert!(!core.registered_target);
            if custom {
                assert!(!core.target_registration_supported());
            }
        }
    }
}

#[test]
fn registered_fractional_negative_origin_and_edge_bounds_preserve_all_art() {
    for scale in [1.0, 2.0] {
        for target in [(0.125, 0.375), (-99.75, -99.125), (199.875, 199.625)] {
            let event = VisualEvent {
                target: Some(target),
                ..intent(Instant::now())
            };
            let mut core = RenderStateCore::new(CursorConfig::default());
            core.visual.reduced_motion = crate::ReducedMotion::On;
            assert!(core.apply_click_approach(event.clone(), bounds(), event.timestamp));
            let radius = core.paint_radius().ceil();
            let origin = (core.pos.0 - radius, core.pos.1 - radius);
            let size = (radius * 2.0 * scale as f64).ceil() as u32;
            let mut cropped = tiny_skia::Pixmap::new(size, size).unwrap();
            paint_cursor_art(&mut cropped, &core, origin.0, origin.1, None, scale);
            let margin = 32;
            let mut padded = tiny_skia::Pixmap::new(size + margin * 2, size + margin * 2).unwrap();
            paint_cursor_art(
                &mut padded,
                &core,
                origin.0 - margin as f64 / scale as f64,
                origin.1 - margin as f64 / scale as f64,
                None,
                scale,
            );
            for y in 0..padded.height() {
                for x in 0..padded.width() {
                    let pixel = padded.pixel(x, y).unwrap();
                    if x >= margin && y >= margin && x < size + margin && y < size + margin {
                        assert_eq!(pixel, cropped.pixel(x - margin, y - margin).unwrap());
                    } else {
                        assert_eq!(pixel.alpha(), 0, "paint escaped bounds");
                    }
                }
            }
            body_only(&mut cropped);
            assert!(opaque_near(
                &cropped,
                (
                    (target.0 - origin.0) * scale as f64,
                    (target.1 - origin.1) * scale as f64
                ),
                2.0
            ));
        }
    }
}
