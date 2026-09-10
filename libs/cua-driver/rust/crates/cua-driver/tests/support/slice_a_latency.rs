//! Pure, test-local acceptance calculations. No native calls or generated evidence.

use serde::{Deserialize, Serialize};
#[path = "slice_a_provenance.rs"]
pub mod provenance;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Warmup {
    pub enabled: bool,
    pub ns: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub block: usize,
    pub pair: usize,
    pub enabled_first: bool,
    pub enabled_ns: f64,
    pub disabled_ns: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Samples {
    pub mode: TimingMode,
    pub warmups: Vec<Warmup>,
    pub pairs: Vec<Pair>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingMode {
    SamePoint,
    DifferentTarget,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ratios {
    #[serde(rename = "median_ratio")]
    pub median: f64,
    #[serde(rename = "p95_ratio")]
    pub p95: f64,
    pub enabled_ns: [f64; 2],
    pub disabled_ns: [f64; 2],
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub mode: TimingMode,
    pub aggregate: Ratios,
    pub blocks: Vec<Ratios>,
}

pub fn latency_report(samples: &Samples) -> Result<Report, String> {
    if samples.warmups.len() != 10 || samples.pairs.len() != 90 {
        return Err("require exactly 10 warmup actions and 3 blocks of 30 complete pairs".into());
    }
    for (i, warmup) in samples.warmups.iter().enumerate() {
        if !positive(warmup.ns) || warmup.enabled != (i % 2 == 0) {
            return Err("invalid warmup duration or alternating order".into());
        }
    }
    for (i, pair) in samples.pairs.iter().enumerate() {
        if pair.block != i / 30
            || pair.pair != i % 30
            || pair.enabled_first != ((i / 30) % 2 == 0)
            || !positive(pair.enabled_ns)
            || !positive(pair.disabled_ns)
        {
            return Err(
                "invalid duration, incomplete pair, or incorrect block/order identity".into(),
            );
        }
    }
    let blocks: Vec<_> = samples.pairs.chunks_exact(30).map(estimate).collect();
    let aggregate = estimate(&samples.pairs);
    if blocks
        .iter()
        .chain([&aggregate])
        .any(|r| !positive(r.median) || !positive(r.p95))
    {
        return Err("nonfinite latency ratio".into());
    }
    Ok(Report {
        mode: samples.mode,
        aggregate,
        blocks,
    })
}

fn positive(v: f64) -> bool {
    v.is_finite() && v > 0.0
}

fn quantiles(values: &mut [f64]) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let median = if n % 2 == 0 {
        values[n / 2 - 1] / 2.0 + values[n / 2] / 2.0
    } else {
        values[n / 2]
    };
    (median, values[(95 * n).div_ceil(100) - 1])
}

fn estimate(pairs: &[Pair]) -> Ratios {
    let mut on: Vec<_> = pairs.iter().map(|p| p.enabled_ns).collect();
    let mut off: Vec<_> = pairs.iter().map(|p| p.disabled_ns).collect();
    let (m1, p1) = quantiles(&mut on);
    let (m0, p0) = quantiles(&mut off);
    Ratios {
        median: m1 / m0,
        p95: p1 / p0,
        enabled_ns: [m1, p1],
        disabled_ns: [m0, p0],
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub scale: f64,
    pub display_bounds: [f64; 4],
    pub native_bounds: [f64; 4],
    pub control_bounds: [f64; 4],
    pub screenshot_size: [u32; 2],
    pub resize_ratio: f64,
    pub request: [f64; 2],
    pub registry: [f64; 2],
}

pub fn parse_geometry(bytes: &[u8]) -> Result<Geometry, String> {
    let g: Geometry = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !positive(g.scale)
        || !positive(g.resize_ratio)
        || !rect(g.native_bounds)
        || !rect(g.control_bounds)
        || !rect(g.display_bounds)
        || g.screenshot_size.contains(&0)
    {
        return Err("missing or invalid physical geometry".into());
    }
    for i in 0..2 {
        if (g.native_bounds[i + 2] * g.scale / g.resize_ratio - f64::from(g.screenshot_size[i]))
            .abs()
            > 1.0
        {
            return Err(
                "screenshot size disagrees with native bounds, scale, or resize ratio".into(),
            );
        }
        let target = g.control_bounds[i] + g.control_bounds[i + 2] / 2.0;
        let converted = g.native_bounds[i] + g.request[i] * g.resize_ratio / g.scale;
        if !g.request[i].is_finite()
            || !g.registry[i].is_finite()
            || g.request[i] < 0.0
            || g.request[i] >= g.screenshot_size[i] as f64
            || (target - converted).abs() > 1.0
            || (target - g.registry[i]).abs() > 1.0
        {
            return Err(
                "registry or screenshot conversion differs from native control center".into(),
            );
        }
        if target < g.display_bounds[i] || target >= g.display_bounds[i] + g.display_bounds[i + 2] {
            return Err("native target is outside selected display".into());
        }
    }
    Ok(g)
}

fn rect(r: [f64; 4]) -> bool {
    r.iter().all(|v| v.is_finite())
        && positive(r[2])
        && positive(r[3])
        && (r[0] + r[2]).is_finite()
        && (r[1] + r[3]).is_finite()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub index: usize,
    pub pts: f64,
    pub image: Artifact,
    /// Empty means the annotator inspected this frame and found no artwork.
    pub tip: Vec<f64>,
    /// Empty means the annotator inspected this frame and found no contact ring.
    pub pulse_center: Vec<f64>,
    #[serde(deserialize_with = "required_counter")]
    pub counter: Option<u64>,
}

fn required_counter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(d)
}

pub fn validate_session_settings(
    state: &serde_json::Value,
    config: &serde_json::Value,
) -> Result<(), String> {
    if state["theme"]["reduced_motion"].as_str() != Some("off") {
        return Err("effective named-session reduced_motion must be enum off".into());
    }
    if config["max_image_dimension"].as_u64() != Some(4096) {
        return Err("effective named-session max_image_dimension must be 4096".into());
    }
    Ok(())
}
pub fn validate_capture_size(g: &Geometry, dimension: u32) -> Result<(), String> {
    if !positive(g.resize_ratio)
        || !positive(g.scale)
        || !rect(g.native_bounds)
        || g.screenshot_size.contains(&0)
    {
        return Err("invalid capture dimensions or ratio".into());
    }
    if dimension == 4096 {
        if (g.resize_ratio - 1.0).abs() > 0.001
            || (0..2).any(|i| {
                (g.native_bounds[i + 2] * g.scale - g.screenshot_size[i] as f64).abs() > 1.0
            })
        {
            return Err("baseline must be actual unresized native pixels".into());
        }
    } else if dimension != 600
        || !positive(g.resize_ratio)
        || g.resize_ratio <= 1.0
        || g.screenshot_size.iter().copied().max() != Some(600)
    {
        return Err("resized row must have actual 600-pixel long edge and ratio above one".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VisualEvidence {
    pub trace_sha256: String,
    pub stage: String,
    pub video: Artifact,
    pub capture_log: Artifact,
    pub video_started_epoch_ms: f64,
    pub clock_uncertainty_ms: f64,
    pub first_frame: usize,
    pub annotation_method: String,
    pub crop_bounds: [f64; 4],
    pub pixel_size: [u32; 2],
    pub scale: f64,
    pub frames: Vec<Frame>,
}

pub fn parse_visual(bytes: &[u8]) -> Result<VisualEvidence, String> {
    let v: VisualEvidence = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !hex_hash(&v.trace_sha256)
        || !artifact(&v.video)
        || !artifact(&v.capture_log)
        || v.stage.trim().is_empty()
        || v.annotation_method.trim().is_empty()
        || !rect(v.crop_bounds)
        || !positive(v.scale)
        || !positive(v.video_started_epoch_ms)
        || !v.clock_uncertainty_ms.is_finite()
        || !(0.0..=10.0).contains(&v.clock_uncertainty_ms)
        || v.pixel_size.contains(&0)
        || v.frames.len() < 3
    {
        return Err("incomplete visual artifact metadata".into());
    }
    for i in 0..2 {
        if (v.crop_bounds[i + 2] * v.scale - v.pixel_size[i] as f64).abs() > 1.0 {
            return Err("crop must preserve original native pixel scale".into());
        }
    }
    for (i, f) in v.frames.iter().enumerate() {
        if f.index != v.first_frame + i
            || !f.pts.is_finite()
            || f.pts < 0.0
            || (i > 0 && f.pts <= v.frames[i - 1].pts)
            || !artifact(&f.image)
        {
            return Err("missing, out-of-order, or malformed decoded frame".into());
        }
        for point in [&f.tip, &f.pulse_center] {
            if !point.is_empty()
                && (point.len() != 2
                    || point.iter().enumerate().any(|(axis, p)| {
                        !p.is_finite() || *p < 0.0 || *p >= v.pixel_size[axis] as f64
                    }))
            {
                return Err("invalid independently measured pixel point".into());
            }
        }
    }
    Ok(v)
}

pub fn validate_capture_window(
    v: &VisualEvidence,
    started: f64,
    returned: f64,
) -> Result<(), String> {
    if !positive(started)
        || !positive(returned)
        || returned <= started
        || !positive(v.video_started_epoch_ms)
        || !v.clock_uncertainty_ms.is_finite()
        || !(0.0..=10.0).contains(&v.clock_uncertainty_ms)
    {
        return Err("missing or uncertain capture clock binding".into());
    }
    let first = v.frames.first().ok_or("no frames")?.pts * 1000.0 + v.video_started_epoch_ms;
    let last = v.frames.last().unwrap().pts * 1000.0 + v.video_started_epoch_ms;
    if first + v.clock_uncertainty_ms > started - 50.0
        || last - v.clock_uncertainty_ms < returned + 100.0
    {
        return Err("capture must include pre-call frames and post-result presentation".into());
    }
    Ok(())
}

pub fn hex_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn artifact(a: &Artifact) -> bool {
    !a.path.trim().is_empty() && hex_hash(&a.sha256)
}

pub fn target_pixels(v: &VisualEvidence, target: [f64; 2]) -> [f64; 2] {
    [
        (target[0] - v.crop_bounds[0]) * v.scale,
        (target[1] - v.crop_bounds[1]) * v.scale,
    ]
}

pub fn near(point: &[f64], target: [f64; 2]) -> bool {
    point.len() == 2 && (point[0] - target[0]).hypot(point[1] - target[1]) <= 2.0
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderingEvidence {
    ArrivalBeforeCounter,
    Indeterminate,
}

#[derive(Debug, Serialize)]
pub struct PlaybackEvidence {
    pub ordering: OrderingEvidence,
    pub reason: String,
    pub first_visible_frame: usize,
    pub arrival_frame: usize,
    pub counter_change_frame: Option<usize>,
    pub measured_max_frame_gap_ms: f64,
    pub travel_interval_ms: [f64; 2],
    pub pulse_interval_ms: [f64; 2],
}

pub fn validate_playback(v: &VisualEvidence, target: [f64; 2]) -> Result<PlaybackEvidence, String> {
    let target = target_pixels(v, target);
    let first = v
        .frames
        .iter()
        .position(|f| !f.tip.is_empty())
        .ok_or("no painted arrow")?;
    if first == 0 {
        return Err("missing captured empty frame before first placement".into());
    }
    let arrival = v
        .frames
        .iter()
        .position(|f| near(&f.tip, target))
        .ok_or("arrow never reaches target within 2 native pixels")?;
    let pulse = v
        .frames
        .iter()
        .position(|f| !f.pulse_center.is_empty())
        .ok_or("no target pulse")?;
    let end = (pulse..v.frames.len())
        .find(|&i| v.frames[i].pulse_center.is_empty())
        .ok_or("missing pulse expiry frame")?;
    let first_tip = &v.frames[first].tip;
    let distance = (first_tip[0] - target[0]).hypot(first_tip[1] - target[1]) / v.scale;
    if distance > 200.0 || pulse < arrival {
        return Err("wrong first placement or pulse precedes arrow arrival".into());
    }
    for f in &v.frames[first..arrival] {
        if f.tip.is_empty() {
            return Err("arrow disappears during approach".into());
        }
    }
    for f in &v.frames[arrival..=end] {
        if !near(&f.tip, target) || (!f.pulse_center.is_empty() && !near(&f.pulse_center, target)) {
            return Err("painted arrow or ring leaves resolved target".into());
        }
    }
    if v.frames[end..].iter().any(|f| !f.pulse_center.is_empty()) {
        return Err("pulse reappears after expiry".into());
    }
    let initial = v.frames[0].counter;
    let mut previous = initial;
    let mut change = None;
    for (i, f) in v.frames.iter().enumerate() {
        if let (Some(before), Some(now)) = (initial, f.counter) {
            if now != before && before.checked_add(1) != Some(now) {
                return Err("counter does not advance exactly once".into());
            }
            if previous.is_some_and(|p| now < p) {
                return Err("counter goes backwards".into());
            }
            if now != before {
                change.get_or_insert(i);
            }
        }
        if f.counter.is_some() {
            previous = f.counter;
        }
    }
    if change.is_some_and(|i| i < arrival) {
        return Err("counter visibly changes before target arrival".into());
    }
    let gap = v
        .frames
        .windows(2)
        .map(|p| p[1].pts - p[0].pts)
        .fold(0.0_f64, f64::max);
    let travel = [
        (v.frames[arrival.saturating_sub(1)].pts - v.frames[first].pts).max(0.0) * 1000.0,
        (v.frames[arrival].pts - v.frames[first - 1].pts) * 1000.0,
    ];
    let pulse_interval = [
        (v.frames[end - 1].pts - v.frames[pulse].pts) * 1000.0,
        (v.frames[end].pts - v.frames[pulse - 1].pts) * 1000.0,
    ];
    let complete_counter = v.frames.iter().all(|f| f.counter.is_some());
    let ordering = change.is_some_and(|i| i > arrival) && complete_counter;
    // Each annotation may vary by 2 native pixels. Require separation greater
    // than the combined endpoint uncertainty at an intermediate captured point.
    let start_tip = &v.frames[first].tip;
    let intermediate = v.frames[first..arrival].iter().skip(1).any(|frame| {
        let tip = &frame.tip;
        (tip[0] - start_tip[0]).hypot(tip[1] - start_tip[1]) > 4.0
            && (tip[0] - target[0]).hypot(tip[1] - target[1]) > 4.0
    });
    let sampled_motion =
        arrival >= first + 2 && intermediate && distance > 2.0 / v.scale && gap <= 0.040;
    // Desired glide configuration is not a wall-clock acceptance range.
    // Report the observed travel interval; keep sampled motion/order and the
    // independent contact-pulse evidence requirements.
    let pulse_timing_consistent = pulse_interval[0] <= 150.0 && pulse_interval[1] >= 150.0;
    let established = ordering && sampled_motion && pulse_timing_consistent;
    Ok(PlaybackEvidence {
        ordering: if established { OrderingEvidence::ArrivalBeforeCounter } else { OrderingEvidence::Indeterminate },
        reason: if established { "Arrow at target is visible in an earlier captured frame than the counter change. This is sampled UI ordering, not a physical scanout or native receipt timestamp." }
            else { "Insufficient sampled ordering or timing: same-frame change, unreadable counter, missing travel samples, or inconsistent pulse timing. Native acceptance remains pending." }.into(),
        first_visible_frame:v.frames[first].index,arrival_frame:v.frames[arrival].index,
        counter_change_frame:change.map(|i| v.frames[i].index),measured_max_frame_gap_ms:gap*1000.0,
        travel_interval_ms:travel,pulse_interval_ms:pulse_interval,
    })
}

#[derive(Debug, Serialize)]
pub struct ApproachTiming {
    pub action_id: String,
    pub record_epoch_ms: [f64; 3],
    pub rpc_bracket: provenance::RpcBracket,
    pub clock_tolerance_ms: f64,
    pub first_frame_ms: f64,
    pub target_frame_ms: f64,
    pub submission_ms: f64,
    pub acknowledgement_ms: f64,
    pub rpc_outside_registered_approach_ms: f64,
}

pub fn parse_approach_timing(
    log: &str,
    bracket: provenance::RpcBracket,
    enabled: bool,
) -> Result<Option<ApproachTiming>, String> {
    let rpc_ms = bracket.elapsed_ms;
    if !positive(rpc_ms) {
        return Err("invalid RPC duration".into());
    }
    let select = |stage: &str| {
        log.lines()
            .filter(|line| {
                line.contains("cua_cursor_approach:")
                    && line.contains(&format!("stage=\"{stage}\""))
            })
            .collect::<Vec<_>>()
    };
    let registered = select("registered");
    let ack = select("ack_received");
    let released = select("released");
    if !enabled {
        return if registered.is_empty() && ack.is_empty() && released.is_empty() {
            Ok(None)
        } else {
            Err("disabled sample has unexpected approach records".into())
        };
    }
    if registered.len() != 1 || ack.len() != 1 || released.len() != 1 {
        return Err(
            "enabled sample needs exactly one complete registered/ack_received/released record set"
                .into(),
        );
    }
    fn id(line: &str) -> Result<&str, String> {
        line.split_once("id=VisualActionId { ")
            .and_then(|(_, s)| s.split_once(" }"))
            .map(|(id, _)| id)
            .ok_or_else(|| "missing private action identity".into())
    }
    provenance::validate_record_order(log, [registered[0], ack[0], released[0]])?;
    let epoch_ms = provenance::validate_rpc_records([registered[0], ack[0], released[0]], bracket)?;
    let action_id = id(registered[0])?;
    if id(ack[0])? != action_id || id(released[0])? != action_id {
        return Err("mixed private action identities".into());
    }
    let line = released[0];
    for name in ["geometry_matches", "route_matches", "surface_matches"] {
        if !line.contains(&format!("{name}: true")) {
            return Err(format!("unconfirmed {name}"));
        }
    }
    let number = |name: &str| -> Result<f64, String> {
        let marker = format!("{name}: Some(");
        if line.matches(&marker).count() != 1 {
            return Err(format!("missing or duplicate {name}"));
        }
        let value = line
            .split_once(&marker)
            .and_then(|(_, s)| s.split_once(')'))
            .map(|(s, _)| s)
            .ok_or_else(|| format!("missing {name}"))?
            .parse::<f64>()
            .map_err(|e| e.to_string())?;
        if !value.is_finite() || value < 0.0 {
            return Err(format!("invalid {name}"));
        }
        Ok(value)
    };
    let first_frame_ms = number("first_frame_ms")?;
    let target_frame_ms = number("target_frame_ms")?;
    let submission_ms = number("submission_ms")?;
    let acknowledgement_ms = number("acknowledgement_ms")?;
    if !(first_frame_ms <= target_frame_ms
        && target_frame_ms <= submission_ms
        && submission_ms <= acknowledgement_ms
        && acknowledgement_ms <= rpc_ms)
    {
        return Err("inconsistent private approach timing".into());
    }
    Ok(Some(ApproachTiming {
        action_id: action_id.into(),
        record_epoch_ms: epoch_ms,
        rpc_bracket: bracket,
        clock_tolerance_ms: provenance::RPC_CLOCK_TOLERANCE_MS,
        first_frame_ms,
        target_frame_ms,
        submission_ms,
        acknowledgement_ms,
        rpc_outside_registered_approach_ms: rpc_ms - acknowledgement_ms,
    }))
}

#[cfg(test)]
mod slice_a_approach_log_tests {
    use super::*;
    // Synthetic parser fixtures, never native observations.
    const LOG: &str = r#"2026-01-01T00:00:00.000Z DEBUG cua_cursor_approach: click approach registered stage="registered" id=VisualActionId { generation: 2, action: 3 }
2026-01-01T00:00:00.150Z DEBUG cua_cursor_approach: click target submission acknowledged stage="ack_received" id=VisualActionId { generation: 2, action: 3 } age_ms=150.2
2026-01-01T00:00:00.260Z DEBUG cua_cursor_approach: click approach frame evidence stage="released" id=VisualActionId { generation: 2, action: 3 } timing=ApproachTiming { first_frame_ms: Some(10.0), target_frame_ms: Some(110.0), submission_ms: Some(130.0), acknowledgement_ms: Some(150.0), geometry_matches: true, route_matches: true, surface_matches: true } elapsed_ms=260.0"#;

    fn parse_approach_timing(
        log: &str,
        rpc_ms: f64,
        enabled: bool,
    ) -> Result<Option<ApproachTiming>, String> {
        let start = provenance::timestamp_ms("2026-01-01T00:00:00Z").unwrap();
        super::parse_approach_timing(
            log,
            provenance::RpcBracket {
                started_epoch_ms: start,
                returned_epoch_ms: start + rpc_ms,
                elapsed_ms: rpc_ms,
            },
            enabled,
        )
    }

    #[test]
    fn separates_recorded_approach_from_remaining_rpc_without_guessed_duration() {
        let t = parse_approach_timing(LOG, 280.0, true).unwrap().unwrap();
        assert_eq!(t.acknowledgement_ms, 150.0);
        assert_eq!(t.rpc_outside_registered_approach_ms, 130.0);
        assert!(parse_approach_timing("", 100.0, false).unwrap().is_none());
    }

    #[test]
    fn correction_reversed_approach_records_are_rejected() {
        let lines: Vec<_> = LOG.lines().collect();
        assert!(parse_approach_timing(
            &lines.into_iter().rev().collect::<Vec<_>>().join("\n"),
            280.,
            true
        )
        .is_err());
    }

    #[test]
    fn missing_mismatched_duplicate_and_invalid_timing_evidence_fails() {
        for log in [
            String::new(),
            LOG.replace("Some(150.0)", "None"),
            LOG.replace("Some(150.0)", "Some(NaN)"),
            LOG.replace(
                "stage=\"ack_received\" id=VisualActionId { generation: 2, action: 3 }",
                "stage=\"ack_received\" id=VisualActionId { generation: 2, action: 4 }",
            ),
            format!("{LOG}\n{LOG}"),
            LOG.replace("geometry_matches: true", "geometry_matches: false"),
        ] {
            assert!(parse_approach_timing(&log, 280.0, true).is_err());
        }
        assert!(parse_approach_timing(LOG, 100.0, true).is_err());
        assert!(parse_approach_timing(LOG, 280.0, false).is_err());
    }
}

#[cfg(test)]
mod slice_a_evidence_tests {
    use super::*;
    use serde_json::json;

    fn clip() -> VisualEvidence {
        VisualEvidence {
            trace_sha256: "a".repeat(64),
            stage: "first-click".into(),
            video: Artifact {
                path: "clip.mov".into(),
                sha256: "b".repeat(64),
            },
            capture_log: Artifact {
                path: "capture.json".into(),
                sha256: "d".repeat(64),
            },
            video_started_epoch_ms: 1000.0,
            clock_uncertainty_ms: 2.0,
            first_frame: 0,
            annotation_method: "manual tip and ring centers on every original decoded frame".into(),
            crop_bounds: [800.0, 100.0, 600.0, 700.0],
            pixel_size: [1200, 1400],
            scale: 2.0,
            frames: (0..42)
                .map(|i| Frame {
                    counter: Some(if i < 14 { 0 } else { 1 }),
                    index: i,
                    pts: i as f64 * 0.01,
                    image: Artifact {
                        path: format!("frame-{i}.png"),
                        sha256: "c".repeat(64),
                    },
                    tip: if i < 2 {
                        vec![]
                    } else {
                        vec![300.0 - (12_i32 - i as i32).max(0) as f64 * 16.0, 240.0]
                    },
                    pulse_center: if (14..29).contains(&i) {
                        vec![300.0, 240.0]
                    } else {
                        vec![]
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn quick_approach_arrival_is_observed_before_counter_change() {
        let parsed = parse_visual(&serde_json::to_vec(&clip()).unwrap()).unwrap();
        let result =
            serde_json::to_value(validate_playback(&parsed, [950.0, 220.0]).unwrap()).unwrap();
        assert_eq!(result["ordering"], "arrival_before_counter");
    }

    #[test]
    fn presentation_jitter_travel_duration_is_descriptive_with_visible_ordering() {
        for frame_seconds in [0.005, 0.030] {
            let mut v = clip();
            // Only the approach/counter prefix is retimed. Preserve pulse
            // samples and all original synthetic geometry and ordering.
            for f in &mut v.frames {
                f.pts = if f.index <= 14 {
                    f.index as f64 * frame_seconds
                } else {
                    14.0 * frame_seconds + (f.index - 14) as f64 * 0.01
                };
            }
            let result = validate_playback(&v, [950.0, 220.0]).unwrap();
            assert!(matches!(
                result.ordering,
                OrderingEvidence::ArrivalBeforeCounter
            ));
            assert!((result.travel_interval_ms[0] - 9.0 * frame_seconds * 1000.0).abs() < 1e-6);
            assert!((result.travel_interval_ms[1] - 11.0 * frame_seconds * 1000.0).abs() < 1e-6);
        }
    }

    #[test]
    fn correction_stationary_then_jump_is_not_glide() {
        let mut v = clip();
        let start = v.frames[2].tip.clone();
        for frame in &mut v.frames[2..12] {
            frame.tip = start.clone();
        }
        let result = validate_playback(&v, [950.0, 220.0]).unwrap();
        assert!(matches!(result.ordering, OrderingEvidence::Indeterminate));
    }
    #[test]
    fn correction_disabled_empty_crop_cannot_hide_painted_region() {
        assert!(provenance::validate_disabled_coverage(
            [0., 0., 10., 10.],
            2.,
            [0., 0., 1200., 800.],
            2.
        )
        .is_err());
        assert!(provenance::validate_disabled_coverage(
            [0., 0., 1200., 800.],
            1.,
            [0., 0., 1200., 800.],
            2.
        )
        .is_err());
        assert!(provenance::validate_disabled_coverage(
            [0., 0., 1200., 800.],
            2.,
            [0., 0., 1200., 800.],
            2.
        )
        .is_ok());
    }

    #[test]
    fn quick_approach_same_frame_ordering_is_indeterminate() {
        let mut v = clip();
        for f in &mut v.frames {
            f.counter = Some(u64::from(f.index >= 12));
        }
        let result = serde_json::to_value(validate_playback(&v, [950.0, 220.0]).unwrap()).unwrap();
        assert_eq!(result["ordering"], "indeterminate");
    }

    #[test]
    fn quick_approach_counter_before_arrival_is_rejected() {
        let mut v = clip();
        for f in &mut v.frames {
            f.counter = Some(u64::from(f.index >= 6));
        }
        assert!(validate_playback(&v, [950.0, 220.0]).is_err());
    }

    #[test]
    fn quick_approach_unreadable_or_sparse_sampling_is_indeterminate() {
        for sparse in [false, true] {
            let mut v = clip();
            if sparse {
                for f in &mut v.frames {
                    f.pts *= 5.0;
                }
            } else {
                v.frames[11].counter = None;
            }
            let result =
                serde_json::to_value(validate_playback(&v, [950.0, 220.0]).unwrap()).unwrap();
            assert_eq!(result["ordering"], "indeterminate");
        }
    }

    #[test]
    fn rejects_empty_missing_malformed_and_incomplete_artifact_metadata() {
        let good = serde_json::to_value(clip()).unwrap();
        for field in [
            "video",
            "scale",
            "frames",
            "trace_sha256",
            "annotation_method",
        ] {
            let mut value = good.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(parse_visual(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        for (field, value) in [
            ("frames", json!([])),
            ("scale", json!(0)),
            ("trace_sha256", json!("old")),
            ("annotation_method", json!("")),
        ] {
            let mut row = good.clone();
            row[field] = value;
            assert!(parse_visual(&serde_json::to_vec(&row).unwrap()).is_err());
        }
        let mut row = good;
        row["frames"][1].as_object_mut().unwrap().remove("tip");
        assert!(parse_visual(&serde_json::to_vec(&row).unwrap()).is_err());
    }

    #[test]
    fn rejects_wrong_tip_early_pulse_and_incomplete_frames() {
        for variant in 0..5 {
            let mut v = clip();
            match variant {
                0 => v.frames[4].pulse_center = vec![300.0, 240.0],
                1 => {
                    for f in &mut v.frames[12..] {
                        f.tip = vec![310.0, 240.0];
                    }
                }
                2 => {
                    v.frames.remove(0);
                }
                3 => v.frames[3].pts = v.frames[2].pts,
                _ => v.frames[2].tip = vec![1199.0, 1399.0],
            }
            assert!(parse_visual(&serde_json::to_vec(&v).unwrap())
                .and_then(|p| validate_playback(&p, [950.0, 220.0]))
                .is_err());
        }
    }

    #[test]
    fn capture_clock_must_cover_this_call_not_another_or_a_truncated_clip() {
        let v = clip();
        validate_capture_window(&v, 1060.0, 1100.0).unwrap();
        for (start, end) in [
            (0.0, 1100.0),
            (1060.0, 2000.0),
            (900.0, 1000.0),
            (1100.0, 1060.0),
        ] {
            assert!(validate_capture_window(&v, start, end).is_err());
        }
        let mut missing = v.clone();
        missing.clock_uncertainty_ms = 100.0;
        assert!(validate_capture_window(&missing, 1060.0, 1100.0).is_err());
    }
}

#[cfg(test)]
mod slice_a_latency_report_tests {
    use super::*;
    use serde_json::json;

    fn samples(ratio: f64) -> Samples {
        Samples {
            mode: TimingMode::SamePoint,
            warmups: (0..10)
                .map(|i| Warmup {
                    enabled: i % 2 == 0,
                    ns: 100.0,
                })
                .collect(),
            pairs: (0..90)
                .map(|i| Pair {
                    block: i / 30,
                    pair: i % 30,
                    enabled_first: (i / 30) % 2 == 0,
                    enabled_ns: 100.0 * ratio,
                    disabled_ns: 100.0,
                })
                .collect(),
        }
    }

    #[test]
    fn quick_approach_latency_is_descriptive_without_a_threshold() {
        for ratio in [1.0, 1.04, 1.06, 1.4] {
            let report = latency_report(&samples(ratio)).unwrap();
            assert!((report.aggregate.median - ratio).abs() < 1e-12);
            assert!((report.aggregate.p95 - ratio).abs() < 1e-12);
            let value = serde_json::to_value(report).unwrap();
            assert!(
                value.get("verdict").is_none(),
                "intentional approach has no ratio pass gate"
            );
            assert!(
                value.get("resamples").is_none(),
                "obsolete significance machinery"
            );
        }
    }

    #[test]
    fn nearest_rank_p95_and_even_median_are_not_interpolated_quantiles() {
        let mut data = samples(1.0);
        for pair in &mut data.pairs {
            pair.enabled_ns = (pair.pair + 1) as f64;
            pair.disabled_ns = 10.0;
        }
        let report = latency_report(&data).unwrap();
        assert_eq!(report.aggregate.median, 1.55);
        assert_eq!(report.aggregate.p95, 2.9);
    }

    #[test]
    fn empty_incomplete_nonfinite_and_mispaired_samples_fail() {
        for change in 0..9 {
            let mut data = samples(1.0);
            match change {
                0 => data.pairs.clear(),
                1 => {
                    data.pairs.pop();
                }
                2 => {
                    data.warmups.pop();
                }
                3 => data.pairs[1].enabled_ns = f64::NAN,
                4 => data.pairs[1].disabled_ns = 0.0,
                5 => data.pairs[1].enabled_ns = f64::INFINITY,
                6 => data.pairs[30].enabled_first = true,
                7 => data.pairs[1].pair = 0,
                _ => data.warmups[0].ns = -1.0,
            }
            assert!(latency_report(&data).is_err(), "invalid variant {change}");
        }
        for bytes in ["", "{}", "{\"pairs\":[]}"] {
            assert!(serde_json::from_str::<Samples>(bytes).is_err());
        }
    }

    fn geometry() -> serde_json::Value {
        json!({"scale":2.0,"display_bounds":[0.0,0.0,1512.0,982.0],
            "native_bounds":[800.0,100.0,600.0,700.0],"control_bounds":[900.0,200.0,100.0,40.0],
            "screenshot_size":[600,700],"resize_ratio":2.0,"request":[150.0,120.0],"registry":[950.0,220.0]})
    }

    #[test]
    fn geometry_requires_scale_native_bounds_and_complete_numeric_evidence() {
        assert!(parse_geometry(&serde_json::to_vec(&geometry()).unwrap()).is_ok());
        for field in [
            "scale",
            "native_bounds",
            "control_bounds",
            "screenshot_size",
            "resize_ratio",
            "registry",
        ] {
            let mut value = geometry();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                parse_geometry(&serde_json::to_vec(&value).unwrap()).is_err(),
                "missing {field}"
            );
        }
        for bytes in [b"".as_slice(), b"{}", b"null", b"[]"] {
            assert!(parse_geometry(bytes).is_err());
        }
    }

    #[test]
    fn geometry_rejects_bad_scale_bounds_projection_and_unknown_fields() {
        for (field, value) in [
            ("scale", json!(0)),
            ("scale", json!("2")),
            ("native_bounds", json!([800, 100, 0, 700])),
            ("registry", json!([150, 120])),
            ("screenshot_size", json!([0, 700])),
            ("resize_ratio", json!(1)),
            ("extra", json!(true)),
        ] {
            let mut row = geometry();
            row[field] = value;
            assert!(
                parse_geometry(&serde_json::to_vec(&row).unwrap()).is_err(),
                "invalid {row}"
            );
        }
    }
    #[test]
    fn quick_approach_settings_require_enum_off_and_effective_session_cap() {
        let state = json!({"theme":{"reduced_motion":"off"}});
        validate_session_settings(&state, &json!({"max_image_dimension":4096})).unwrap();
        for value in [
            json!(false),
            json!(true),
            json!("auto"),
            json!("on"),
            json!(null),
        ] {
            assert!(validate_session_settings(
                &json!({"theme":{"reduced_motion":value}}),
                &json!({"max_image_dimension":4096})
            )
            .is_err());
        }
        for value in [json!(null), json!(1568), json!("4096")] {
            assert!(
                validate_session_settings(&state, &json!({"max_image_dimension":value})).is_err()
            );
        }
    }

    #[test]
    fn capture_size_rejects_nonfinite_or_nonpositive_ratios() {
        let mut g: Geometry = serde_json::from_value(geometry()).unwrap();
        g.screenshot_size = [1200, 1400];
        for ratio in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            g.resize_ratio = ratio;
            assert!(validate_capture_size(&g, 4096).is_err());
        }
    }

    #[test]
    fn quick_approach_capture_size_rejects_an_already_resized_baseline() {
        let mut g: Geometry = serde_json::from_value(geometry()).unwrap();
        assert!(validate_capture_size(&g, 4096).is_err());
        g.screenshot_size = [1200, 1400];
        g.resize_ratio = 1.0;
        validate_capture_size(&g, 4096).unwrap();
        assert!(validate_capture_size(&g, 600).is_err());
        g.screenshot_size = [514, 600];
        g.resize_ratio = 1200.0 / 514.0;
        validate_capture_size(&g, 600).unwrap();
        g.screenshot_size = [500, 584];
        assert!(validate_capture_size(&g, 600).is_err());
    }
}
