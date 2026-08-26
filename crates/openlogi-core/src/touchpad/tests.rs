//! Classifier behavior over synthetic frame sequences — the same gestures a
//! Casa Touch reports, spelled as coordinates. Geometry matches the real pad
//! (2775 × 1786 @ upper-left origin, 0.1 ms frame ticks), so thresholds sit
//! at their production values: swipe commit needs 5 co-directed frames
//! spanning ≥ 500 ticks (50 ms) with ≥ 27 units of mean travel, a tap
//! tolerates 111 units of wander for ≤ 2000 ticks (200 ms), and a pinch
//! commits at 347 units of spread change.

use super::TouchpadClassifier;
use super::frame::{TouchContact, TouchFrame};
use super::gesture::TouchpadGestureId as G;

const X: u16 = 2775;
const Y: u16 = 1786;
/// 200 ms at 0.1 ms per tick.
const TAP_TICKS: u16 = 2000;

fn classifier() -> TouchpadClassifier {
    TouchpadClassifier::new(X, Y, TAP_TICKS)
}

fn contact(id: u8, x: u16, y: u16) -> TouchContact {
    TouchContact {
        finger_id: id,
        x,
        y,
    }
}

fn frame(ts: u16, contacts: &[TouchContact]) -> TouchFrame {
    TouchFrame {
        timestamp: ts,
        contacts: contacts.to_vec(),
    }
}

/// A relaxed three-finger column around the pad centre.
fn hand(x: u16, y: u16, ts: u16) -> TouchFrame {
    frame(
        ts,
        &[
            contact(0, x, y),
            contact(1, x + 40, y + 55),
            contact(2, x + 15, y + 110),
        ],
    )
}

/// Feed the whole down → travel → lift sequence, collecting gestures.
fn run(mut moves: Vec<TouchFrame>) -> Vec<G> {
    let mut classifier = classifier();
    let mut fired = Vec::new();
    let last_ts = moves.last().map_or(0, |f| f.timestamp);
    for frame in moves.drain(..) {
        if let Some(g) = classifier.push(&frame) {
            fired.push(g);
        }
    }
    if let Some(g) = classifier.push(&frame(last_ts, &[])) {
        fired.push(g);
    }
    fired
}

#[test]
fn three_finger_swipe_commits_after_five_consistent_frames() {
    // A modest sweep — 40 units per frame, 280 units total — far under the
    // v1 absolute threshold but well past Options+'s real gate: five
    // co-directed frames spanning 50 ms with travel over the 139-unit floor.
    let mut classifier = classifier();
    classifier.push(&hand(600, 700, 100));
    for (i, ts) in [225_u16, 350, 475, 600].into_iter().enumerate() {
        assert_eq!(
            classifier.push(&hand(
                600 + 40 * u16::try_from(i).unwrap_or(0) + 40,
                700,
                ts
            )),
            None,
            "frame {} has not yet passed the time gate",
            i + 1
        );
    }
    assert_eq!(
        classifier.push(&hand(880, 700, 725)),
        Some(G::ThreeFingerSwipeRight)
    );
    // The rest of the stroke stays silent, lift included.
    assert_eq!(classifier.push(&hand(960, 700, 800)), None);
    assert_eq!(classifier.push(&frame(800, &[])), None);
}

#[test]
fn a_straggler_finger_never_vetoes_the_sweep() {
    // Natural hand rotation: while the group sweeps right, the outermost
    // finger arcs back the other way. Options+ lowers confidence for that
    // finger; the sweep still commits. (The v1 dot-product veto rejected
    // exactly this shape.)
    let sweep = |x: u16, arc: i32, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x, 800),
                contact(1, x + 40, 855),
                contact(
                    2,
                    u16::try_from((i32::from(x) + 15 + arc).clamp(0, i32::from(X - 1)))
                        .unwrap_or(x),
                    910,
                ),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&sweep(1000, 0, 100));
    classifier.push(&sweep(1040, -20, 225));
    classifier.push(&sweep(1080, -35, 350));
    classifier.push(&sweep(1120, -30, 475));
    classifier.push(&sweep(1160, -10, 600));
    assert_eq!(
        classifier.push(&sweep(1200, 20, 725)),
        Some(G::ThreeFingerSwipeRight)
    );
}

#[test]
fn swipe_survives_an_early_finger_lift() {
    // The third finger lifts mid-sweep — the stroke began with three, and
    // the gesture stays valid while two remain (the engine "finishes when
    // finger count drops to 1"). One rebase frame, then the run continues.
    let mut classifier = classifier();
    classifier.push(&hand(600, 700, 100));
    classifier.push(&hand(640, 700, 225));
    classifier.push(&hand(680, 700, 350));
    // Finger 2 lifts; the mean re-bases for one frame (ts 475 teaches
    // nothing), then the pair sweeps on.
    let pair = |x: u16, ts: u16| frame(ts, &[contact(0, x, 800), contact(1, x + 40, 855)]);
    classifier.push(&pair(720, 475));
    classifier.push(&pair(760, 600));
    classifier.push(&pair(800, 725));
    assert_eq!(
        classifier.push(&pair(840, 850)),
        Some(G::ThreeFingerSwipeRight)
    );
    assert_eq!(classifier.push(&pair(880, 975)), None);
}

#[test]
fn direction_flip_resets_the_run() {
    // Four frames right, then a full reversal: the run must restart, so the
    // leftward sweep needs its own five frames before committing.
    let mut classifier = classifier();
    classifier.push(&hand(600, 700, 100));
    classifier.push(&hand(640, 700, 225));
    classifier.push(&hand(680, 700, 350));
    classifier.push(&hand(720, 700, 475));
    // Reversal starts a fresh run at ts 600.
    classifier.push(&hand(680, 700, 600));
    classifier.push(&hand(640, 700, 725));
    classifier.push(&hand(600, 700, 850));
    classifier.push(&hand(560, 700, 975));
    assert_eq!(
        classifier.push(&hand(520, 700, 1100)),
        Some(G::ThreeFingerSwipeLeft),
        "the fifth leftward frame passes the gate"
    );
    assert_eq!(classifier.push(&hand(480, 700, 1225)), None);
}

#[test]
fn cross_axis_span_between_fingers_resets_the_run() {
    // Fingers stacked 800 units apart vertically (past 40 % of the pad's
    // 1786-unit height = 714) while sweeping right: the span gate resets the
    // run every frame, so no swipe may commit however long the sweep.
    let tower = |x: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x, 400),
                contact(1, x + 20, 800),
                contact(2, x + 40, 1200),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&tower(600, 100));
    for k in 1..=8_u16 {
        assert_eq!(
            classifier.push(&tower(600 + 40 * k, 100 + 125 * k)),
            None,
            "the vertical span exceeds the gate on every frame"
        );
    }
}

#[test]
fn vertical_swipe_direction_follows_the_dominant_axis() {
    let up = |y: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, 1000, y),
                contact(1, 1060, y + 60),
                contact(2, 1120, y + 120),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&up(1500, 100));
    for k in 1..=4_u16 {
        assert_eq!(classifier.push(&up(1500 - 60 * k, 100 + 125 * k)), None);
    }
    assert_eq!(classifier.push(&up(1200, 725)), Some(G::ThreeFingerSwipeUp));
    assert_eq!(classifier.push(&up(1140, 850)), None);
}

#[test]
fn a_wide_diagonal_tilt_still_commits_on_its_dominant_axis() {
    // A sweep at ~30° off horizontal is a horizontal swipe to the engine —
    // the cross-axis rule bounds finger span, not the travel ratio. (The v1
    // travel-ratio gate rejected this.)
    let diagonal = |x: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x, 700),
                contact(1, x + 40, 755),
                contact(2, x + 15, 810),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&diagonal(600, 100));
    for k in 1..=4_u16 {
        assert_eq!(
            classifier.push(&diagonal(600 + 50 * k, 100 + 125 * k)),
            None
        );
    }
    assert_eq!(
        classifier.push(&diagonal(900, 725)),
        Some(G::ThreeFingerSwipeRight)
    );
}

#[test]
fn two_finger_drag_is_never_classified_as_a_swipe() {
    // Two fingers sweeping far right is the pad's native scroll — the
    // classifier must stay silent from start to lift (a pinch it may be;
    // this drag keeps its pair distance constant so it is nothing at all).
    let pair = |x: u16, ts: u16| frame(ts, &[contact(0, x, 800), contact(1, x + 300, 800)]);
    assert_eq!(
        run(vec![
            pair(300, 100),
            pair(900, 300),
            pair(2400, 500),
            pair(2400, 700),
        ]),
        Vec::<G>::new()
    );
}

#[test]
fn four_finger_swipe_commits() {
    let quad = |x: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x, 700),
                contact(1, x, 780),
                contact(2, x, 860),
                contact(3, x, 940),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&quad(700, 100));
    for k in 1..=4_u16 {
        assert_eq!(classifier.push(&quad(700 + 45 * k, 100 + 125 * k)), None);
    }
    assert_eq!(
        classifier.push(&quad(925, 725)),
        Some(G::FourFingerSwipeRight)
    );
}

#[test]
fn tap_fires_at_lift_once() {
    // Three fingers land, sit briefly with a little jitter, and lift inside
    // the 200 ms window — a ThreeFingerTap, and only the lift commits it.
    let mut classifier = classifier();
    classifier.push(&hand(1000, 800, 100));
    assert_eq!(classifier.push(&hand(1050, 800, 1500)), None);
    assert_eq!(classifier.push(&frame(1900, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn slow_press_is_not_a_tap() {
    // Same shape, but the lift arrives 2600 ticks in — past the 2000-tick
    // window.
    let mut classifier = classifier();
    classifier.push(&hand(1000, 800, 100));
    assert_eq!(classifier.push(&frame(2700, &[])), None);
}

#[test]
fn wandering_press_is_not_a_tap_even_after_early_finger_lifts() {
    // Inside the time window, but one finger wandered 200 units — past the
    // 111-unit ceiling. The wander is remembered even though that finger
    // lifted before the stroke ended.
    let mut classifier = classifier();
    classifier.push(&hand(1000, 800, 100));
    let drifted = frame(
        800,
        &[
            contact(0, 1000, 800),
            contact(1, 1200, 855),
            contact(2, 1000, 910),
        ],
    );
    classifier.push(&drifted);
    classifier.push(&frame(1000, &[contact(0, 1000, 800)]));
    assert_eq!(classifier.push(&frame(1100, &[])), None);
}

#[test]
fn single_finger_strokes_are_native() {
    // The pointer is the firmware's business; a one-finger tap-and-drag
    // sequence must classify nothing at any point.
    assert_eq!(
        run(vec![
            frame(100, &[contact(0, 1000, 800)]),
            frame(500, &[contact(0, 1800, 800)]),
            frame(900, &[contact(0, 1800, 800)]),
        ]),
        Vec::<G>::new()
    );
}

#[test]
fn tap_window_survives_the_timestamp_wrap() {
    // The stroke starts at 65000 and lifts at 300 — 836 ticks later through
    // the u16 wrap. The tap must still land.
    let mut classifier = classifier();
    classifier.push(&hand(1000, 800, 65000));
    assert_eq!(classifier.push(&frame(300, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn two_finger_pinch_out_and_in() {
    // Fingers 550→700 units apart grows the pair distance past 347: out.
    let mut spreader = classifier();
    spreader.push(&frame(100, &[contact(0, 1100, 900), contact(1, 1200, 900)]));
    assert_eq!(
        spreader.push(&frame(300, &[contact(0, 700, 900), contact(1, 1600, 900)])),
        Some(G::TwoFingerPinchOut)
    );
    // The reverse closes a wide pair: in.
    let mut closer = classifier();
    closer.push(&frame(100, &[contact(0, 600, 900), contact(1, 2200, 900)]));
    assert_eq!(
        closer.push(&frame(300, &[contact(0, 1300, 900), contact(1, 1400, 900)])),
        Some(G::TwoFingerPinchIn)
    );
}

#[test]
fn small_spread_change_does_not_pinch() {
    // 300 units of spread: under the 347-unit minimum, so a two-finger
    // scroll with slight divergence stays unclassified.
    let mut classifier = classifier();
    classifier.push(&frame(100, &[contact(0, 1100, 900), contact(1, 1200, 900)]));
    assert_eq!(
        classifier.push(&frame(300, &[contact(0, 950, 900), contact(1, 1350, 900)])),
        None
    );
}

#[test]
fn four_finger_pinch_in_recognized_by_spread_from_centroid() {
    // Four fingers closing on their centroid: mean radius drops by more
    // than the 347-unit minimum.
    let wide = |scale: u16, ts: u16| {
        let (cx, cy) = (1400_i32, 900_i32);
        let d = i32::from(scale);
        let pts = [
            (cx - d, cy - d),
            (cx + d, cy - d),
            (cx - d, cy + d),
            (cx + d, cy + d),
        ];
        TouchFrame {
            timestamp: ts,
            contacts: pts
                .iter()
                .enumerate()
                .map(|(i, &(x, y))| TouchContact {
                    finger_id: u8::try_from(i).unwrap_or(0),
                    x: u16::try_from(x).unwrap_or(X - 1),
                    y: u16::try_from(y).unwrap_or(Y - 1),
                })
                .collect(),
        }
    };
    let mut classifier = classifier();
    classifier.push(&wide(600, 100));
    assert_eq!(classifier.push(&wide(50, 300)), Some(G::FourFingerPinchIn));
}

#[test]
fn an_asymmetric_pinch_never_locks_a_swipe() {
    // Two fingers pull away twice as fast as the other two close in — the
    // group's centroid drifts right, which without the co-motion gate would
    // read as a rightward sweep five frames in. The first gesture to fire
    // must be the pinch.
    let asymmetric = |k: u16, ts: u16| {
        let (a, b) = (250 * i32::from(k), -125 * i32::from(k));
        let clamp = |v: i32| u16::try_from(v.clamp(0, i32::from(X - 1))).unwrap_or(0);
        frame(
            ts,
            &[
                contact(0, clamp(1000 + a), 800),
                contact(1, clamp(1040 + a), 855),
                contact(2, clamp(1200 + b), 910),
                contact(3, clamp(1240 + b), 965),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&asymmetric(0, 100));
    let mut first = None;
    for k in 1..=6_u16 {
        if let Some(g) = classifier.push(&asymmetric(k, 100 + 125 * k)) {
            first = Some(g);
            break;
        }
    }
    assert_eq!(
        first,
        Some(G::FourFingerPinchOut),
        "the spreading group must commit as a pinch, never a swipe"
    );
}

#[test]
fn a_fresh_stroke_after_a_committed_one_classifies_again() {
    // Swipe right, lift, then a clean three-finger tap: the tap must fire —
    // proving the latch and stroke state reset at the empty frame.
    let mut classifier = classifier();
    classifier.push(&hand(600, 700, 100));
    for k in 1..=6_u16 {
        classifier.push(&hand(600 + 40 * k, 700, 100 + 125 * k));
    }
    assert!(classifier.push(&frame(850, &[])).is_none());
    classifier.push(&hand(1000, 800, 900));
    assert_eq!(classifier.push(&frame(1900, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn idle_frame_repeats_are_inert() {
    // A resting hand re-sends the same logical frame with fresh timestamps;
    // feeding them straight through must neither fire nor corrupt the
    // stroke. Sub-deadzone motion holds the swipe run without extending it.
    let mut classifier = classifier();
    for ts in [100_u16, 177, 254, 331, 408, 485, 562, 700] {
        classifier.push(&hand(1000, 800, ts));
    }
    assert_eq!(classifier.push(&frame(900, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn finger_landing_mid_stroke_restarts_the_swipe_run() {
    // Two fingers land and start sweeping, the third joins mid-motion: the
    // run restarts (the new finger's motion is unattributed), then the
    // three-finger sweep commits on its own five frames.
    let pair = |x: u16, ts: u16| frame(ts, &[contact(0, x, 800), contact(1, x + 40, 855)]);
    let mut classifier = classifier();
    classifier.push(&pair(600, 100));
    classifier.push(&pair(640, 225));
    // The third finger lands; the first frame after teaches nothing.
    classifier.push(&hand(680, 700, 350));
    for k in 1..=4_u16 {
        classifier.push(&hand(680 + 40 * k, 700, 350 + 125 * k));
    }
    assert_eq!(
        classifier.push(&hand(920, 700, 1000)),
        Some(G::ThreeFingerSwipeRight)
    );
}

#[test]
fn classifier_survives_coordinates_at_the_extremes() {
    // Corner coordinates (0 and max) exercise the mean/delta math; a sweep
    // along the very top edge must classify, not panic.
    let edge = |x: u16, ts: u16| {
        frame(
            ts,
            &[contact(0, x, 0), contact(1, x, 60), contact(2, x, 120)],
        )
    };
    let mut classifier = classifier();
    classifier.push(&edge(0, 100));
    for k in 1..=4_u16 {
        assert_eq!(classifier.push(&edge(40 * k, 100 + 125 * k)), None);
    }
    assert_eq!(
        classifier.push(&edge(200, 725)),
        Some(G::ThreeFingerSwipeRight)
    );
}
