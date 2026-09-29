//! Print how far the agent cursor strays from a straight line.
//!
//! Replays a few moves through the real overlay state machine (the macOS
//! `tick_swift_constants` path) and prints, for each `turn_radius`, the max
//! perpendicular deviation from the straight segment, the glide time, and the
//! largest per-frame arrow rotation.
//!
//! `cargo run -p cursor-overlay --example cursor_path [-- --points] [-- R...]`

use cursor_overlay::{CursorConfig, MotionConfig, OverlayCommand, RenderStateCore};
use std::f64::consts::{FRAC_PI_4, PI};

const DT: f64 = 1.0 / 240.0;

struct Move {
    dev: f64,
    dist: f64,
    glide_s: f64,
    max_turn_per_frame_deg: f64,
    points: Vec<(f64, f64)>,
}

/// Glide from the cursor's current pose to `(x, y)`; the recorded path covers
/// the glide itself (until arrival), not the short settle spring after it.
fn glide(core: &mut RenderStateCore, x: f64, y: f64) -> Move {
    let (x0, y0) = core.pos;
    core.apply_command_base(
        OverlayCommand::MoveTo {
            x,
            y,
            end_heading_radians: FRAC_PI_4,
        },
        true,
        true,
    );
    // The planner aims at the anchor that puts the tip on the click point.
    let (x1, y1) = cursor_overlay::default_anchor_for_tip((x, y), FRAC_PI_4);
    let dist = (x1 - x0).hypot(y1 - y0);
    let (mut dev, mut t, mut max_turn) = (0.0f64, 0.0, 0.0f64);
    let mut points = vec![(x0, y0)];
    let mut last_frame_heading = core.heading;
    let mut ticks = 0u32;
    loop {
        let arrived = core.tick_swift_constants(DT);
        t += DT;
        ticks += 1;
        let (px, py) = core.pos;
        dev = dev.max(((x1 - x0) * (py - y0) - (y1 - y0) * (px - x0)).abs() / dist);
        if ticks % 4 == 0 || arrived {
            // Per 60 Hz frame.
            let mut d = (core.heading - last_frame_heading).rem_euclid(2.0 * PI);
            if d > PI {
                d = 2.0 * PI - d;
            }
            max_turn = max_turn.max(d.to_degrees());
            last_frame_heading = core.heading;
            points.push((px, py));
        }
        if arrived || t > 10.0 {
            break;
        }
    }
    // Let the settle spring finish so the next move starts at rest.
    for _ in 0..240 {
        core.tick_swift_constants(DT);
    }
    Move {
        dev,
        dist,
        glide_s: t,
        max_turn_per_frame_deg: max_turn,
        points,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_points = args.iter().any(|a| a == "--points");
    let mut radii: Vec<f64> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let default_r = MotionConfig::default().turn_radius;
    if radii.is_empty() {
        radii = vec![80.0, default_r];
    }
    // Calculator-sized hops (1 -> 9 -> AC -> 5) then long screen diagonals.
    let targets = [
        (400.0, 500.0),
        (514.0, 400.0),
        (400.0, 350.0),
        (457.0, 450.0),
        (1200.0, 850.0),
        (200.0, 150.0),
        (1100.0, 200.0),
    ];
    for r in radii {
        let label = if r == default_r { " (default)" } else { "" };
        println!("turn_radius = {r}{label}");
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.motion.turn_radius = r;
        core.motion.idle_hide_ms = 0.0;
        core.pos = (200.0, 700.0);
        let mut worst: f64 = 0.0;
        for &(x, y) in &targets {
            let m = glide(&mut core, x, y);
            worst = worst.max(m.dev / m.dist);
            println!(
                "  to ({x:>6.1},{y:>6.1})  dist {:>6.1}  max dev {:>6.1} pt = {:>5.1}%  glide {:>4.0} ms  max arrow turn/frame {:>5.1} deg",
                m.dist,
                m.dev,
                100.0 * m.dev / m.dist,
                m.glide_s * 1000.0,
                m.max_turn_per_frame_deg
            );
            if show_points {
                let pts: Vec<String> = m
                    .points
                    .iter()
                    .map(|(x, y)| format!("({x:.0},{y:.0})"))
                    .collect();
                println!("    {}", pts.join(" "));
            }
        }
        println!("  worst deviation {:.1}% of move distance\n", 100.0 * worst);
    }
}
