//! Classifier behavior over synthetic frame sequences — the same gestures a
//! Casa Touch reports, spelled as coordinates. Geometry matches the real pad
//! (2775 × 1786 @ upper-left origin, 0.1 ms frame ticks) so thresholds sit at
//! their production values: swipe minimum 462 × 297 units, pinch minimum 555,
//! tap ceiling 138 units of wander and 2500 ticks.

use super::TouchpadClassifier;
use super::frame::{TouchContact, TouchFrame};
use super::gesture::TouchpadGestureId as G;

const X: u16 = 2775;
const Y: u16 = 1786;
const TAP_TICKS: u16 = 2500;

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

/// A vertical 3-finger column around the pad centre, `fingers` wide.
fn fingers_at(n: u8, x: u16, ts: u16) -> TouchFrame {
    let base = 800;
    let contacts = (0..n)
        .map(|i| contact(i, x, base + u16::from(i) * 90))
        .collect();
    TouchFrame {
        timestamp: ts,
        contacts,
    }
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
fn three_finger_swipe_commits_mid_stroke_exactly_once() {
    let mut classifier = classifier();
    classifier.push(&fingers_at(3, 600, 100));
    // Slide right: at 1070 the mean travel (470) crosses the 462 minimum —
    // but only once the last finger has caught up, which frame 3 arranges.
    classifier.push(&fingers_at(3, 900, 200));
    assert_eq!(
        classifier.push(&fingers_at(3, 1070, 300)),
        Some(G::ThreeFingerSwipeRight)
    );
    // The rest of the stroke stays silent, lift included.
    assert_eq!(classifier.push(&fingers_at(3, 1400, 400)), None);
    assert_eq!(classifier.push(&frame(500, &[])), None);
}

#[test]
fn swipe_below_threshold_never_fires_and_is_no_tap() {
    // 400 units of rightward travel: under the 462 minimum, and well over the
    // 138-unit tap ceiling — so neither a swipe nor a tap may fire.
    assert_eq!(
        run(vec![
            fingers_at(3, 600, 100),
            fingers_at(3, 1000, 200),
            fingers_at(3, 1000, 300),
        ]),
        Vec::<G>::new()
    );
}

#[test]
fn vertical_swipe_direction_follows_the_dominant_axis() {
    // Straight up: the column slides from y≈1500 to y≈800, mean dy = -700
    // past the 297 vertical minimum.
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
    // 300 units of travel already clears the 297 vertical minimum.
    assert_eq!(classifier.push(&up(1200, 500)), Some(G::ThreeFingerSwipeUp));
    // Further travel in the committed stroke stays silent.
    assert_eq!(classifier.push(&up(800, 900)), None);
}

#[test]
fn diagonal_swipe_is_rejected() {
    // Mean (600, 400): horizontal candidate at 600 ≥ 462, but the 400 cross
    // axis exceeds half the dominant (300) — too diagonal to commit.
    let diagonal = |x: u16, y: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x, y),
                contact(1, x + 40, y + 40),
                contact(2, x + 80, y + 80),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&diagonal(600, 400, 100));
    assert_eq!(classifier.push(&diagonal(1200, 800, 300)), None);
}

#[test]
fn two_finger_drag_is_never_classified() {
    // Two fingers sweeping far right is the pad's native scroll — the
    // classifier must stay silent from start to lift.
    assert_eq!(
        run(vec![
            fingers_at(2, 300, 100),
            fingers_at(2, 1500, 200),
            fingers_at(2, 2600, 300),
        ]),
        Vec::<G>::new()
    );
}

#[test]
fn straggler_finger_blocks_the_swipe() {
    // Two fingers sweep right while the third drags left: the mean still
    // points right, but a finger fighting the mean means no swipe.
    let split = |x_right: u16, x_left: u16, ts: u16| {
        frame(
            ts,
            &[
                contact(0, x_right, 800),
                contact(1, x_right + 80, 890),
                contact(2, x_left, 980),
            ],
        )
    };
    let mut classifier = classifier();
    classifier.push(&split(800, 1400, 100));
    assert_eq!(classifier.push(&split(1800, 400, 300)), None);
}

#[test]
fn four_finger_swipe_commits() {
    let mut classifier = classifier();
    classifier.push(&fingers_at(4, 700, 100));
    assert_eq!(
        classifier.push(&fingers_at(4, 1300, 300)),
        Some(G::FourFingerSwipeRight)
    );
}

#[test]
fn tap_fires_at_lift_once() {
    // Three fingers land, sit briefly with a little jitter, and lift inside
    // the 250 ms window — a ThreeFingerTap, and only the lift commits it.
    let mut classifier = classifier();
    classifier.push(&fingers_at(3, 1000, 100));
    assert_eq!(classifier.push(&fingers_at(3, 1050, 1500)), None);
    assert_eq!(classifier.push(&frame(2000, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn slow_press_is_not_a_tap() {
    // Same shape, but the lift arrives 2600 ticks in — past the 2500 window.
    let mut classifier = classifier();
    classifier.push(&fingers_at(2, 1000, 100));
    assert_eq!(classifier.push(&frame(2700, &[])), None);
}

#[test]
fn wandering_press_is_not_a_tap() {
    // Inside the time window, but one finger wandered 200 units — past the
    // 138-unit ceiling.
    let mut classifier = classifier();
    classifier.push(&fingers_at(3, 1000, 100));
    let drifted = frame(
        800,
        &[
            contact(0, 1000, 800),
            contact(1, 1200, 890),
            contact(2, 1000, 980),
        ],
    );
    classifier.push(&drifted);
    assert_eq!(classifier.push(&frame(1000, &[])), None);
}

#[test]
fn single_finger_strokes_are_native() {
    // The pointer is the firmware's business; a one-finger tap-and-drag
    // sequence must classify nothing at any point.
    assert_eq!(
        run(vec![
            fingers_at(1, 1000, 100),
            fingers_at(1, 1800, 500),
            fingers_at(1, 1800, 900),
        ]),
        Vec::<G>::new()
    );
}

#[test]
fn tap_window_survives_the_timestamp_wrap() {
    // The stroke starts at 65000 and lifts at 300 — 836 ticks later through
    // the u16 wrap. The tap must still land.
    let mut classifier = classifier();
    classifier.push(&fingers_at(3, 1000, 65000));
    assert_eq!(classifier.push(&frame(300, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn two_finger_pinch_out_and_in() {
    // Fingers 550→700 units apart grows the pair distance past 555: out.
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
    // 300 units of spread: under the 555 minimum, so a two-finger scroll
    // with slight divergence stays unclassified.
    let mut classifier = classifier();
    classifier.push(&frame(100, &[contact(0, 1100, 900), contact(1, 1200, 900)]));
    assert_eq!(
        classifier.push(&frame(300, &[contact(0, 950, 900), contact(1, 1350, 900)])),
        None
    );
}

#[test]
fn four_finger_pinch_in_recognized_by_spread_from_centroid() {
    // Four fingers closing on their centroid: mean radius drops by more than
    // the 555-unit minimum.
    let wide = |scale: u16, ts: u16| {
        let (cx, cy) = (1400_i32, 900_i32);
        let d = i32::from(scale);
        let pts = [
            (cx as i32 - d, cy as i32 - d),
            (cx as i32 + d, cy as i32 - d),
            (cx as i32 - d, cy as i32 + d),
            (cx as i32 + d, cy as i32 + d),
        ];
        TouchFrame {
            timestamp: ts,
            contacts: pts
                .iter()
                .enumerate()
                .map(|(i, &(x, y))| TouchContact {
                    finger_id: i as u8,
                    x: x.clamp(0, i32::from(X) - 1) as u16,
                    y: y.clamp(0, i32::from(Y) - 1) as u16,
                })
                .collect(),
        }
    };
    let mut classifier = classifier();
    classifier.push(&wide(600, 100));
    assert_eq!(classifier.push(&wide(50, 300)), Some(G::FourFingerPinchIn));
}

#[test]
fn a_fresh_stroke_after_a_committed_one_classifies_again() {
    // Swipe right, lift, then a clean three-finger tap: the tap must fire —
    // proving the latch and stroke state reset at the empty frame.
    let mut classifier = classifier();
    classifier.push(&fingers_at(3, 600, 100));
    classifier.push(&fingers_at(3, 1200, 300));
    assert!(classifier.push(&frame(400, &[])).is_none());
    classifier.push(&fingers_at(3, 1000, 500));
    assert_eq!(classifier.push(&frame(1500, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn idle_frame_repeats_are_inert() {
    // A resting hand re-sends the same logical frame with fresh timestamps;
    // feeding them straight through must neither fire nor corrupt the stroke.
    let mut classifier = classifier();
    for ts in [100_u16, 177, 254, 331] {
        classifier.push(&fingers_at(3, 1000, ts));
    }
    assert_eq!(classifier.push(&frame(500, &[])), Some(G::ThreeFingerTap));
}

#[test]
fn finger_landing_mid_stroke_joins_the_tracked_set() {
    // Two fingers land, the third joins before any travel: the whole stroke
    // classifies as three-fingered, not two.
    let mut classifier = classifier();
    classifier.push(&fingers_at(2, 800, 100));
    classifier.push(&fingers_at(3, 800, 150));
    assert_eq!(
        classifier.push(&fingers_at(3, 1500, 400)),
        Some(G::ThreeFingerSwipeRight)
    );
}

#[test]
fn classifier_survives_coordinates_at_the_extremes() {
    // Corner coordinates (0 and max) exercise the i32 displacement math; a
    // swipe from the far left edge to the far right must classify, not panic.
    let edge = |x: u16, ts: u16| {
        frame(
            ts,
            &[contact(0, x, 0), contact(1, x, Y - 1), contact(2, x, 900)],
        )
    };
    let mut classifier = classifier();
    classifier.push(&edge(0, 100));
    assert_eq!(
        classifier.push(&edge(X - 1, 400)),
        Some(G::ThreeFingerSwipeRight)
    );
}
