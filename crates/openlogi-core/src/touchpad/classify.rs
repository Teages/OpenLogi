//! Classify touchpad gestures from logical [`TouchFrame`]s.
//!
//! One [`TouchpadClassifier`] per capture session tracks fingers by id across
//! frames and commits a gesture the moment its travel crosses a threshold —
//! mid-stroke, like a native trackpad feels — or, for taps, at lift-off. A
//! committed gesture latches until every finger lifts, so one physical stroke
//! fires at most one gesture.
//!
//! The stroke unit is the all-fingers-lift boundary: while any finger stays
//! down the stroke continues (a finger landing or lifting mid-stroke merely
//! re-shapes the tracked set), and the first empty frame ends it. A frame
//! stream that skips the empty frame — a finger swap — therefore merges two
//! strokes into one; the pad's own frame stream reliably reports the gap, and
//! merged strokes still classify conservatively (the new finger's fresh
//! baseline drags the mean displacement back toward zero).
//!
//! All thresholds are derived from the pad's reported size, so one
//! implementation tunes itself across pads. Coordinates follow the
//! upper-left-origin convention the pads report (`+x` right, `+y` down), the
//! same convention as [`detect_swipe`](crate::binding::detect_swipe).

use super::frame::{TouchContact, TouchFrame};
use super::gesture::TouchpadGestureId;

/// Dominant-axis travel before a swipe commits, as the denominator of that
/// axis's pad size (1/6 ≈ 17 % of the pad — a deliberate third-of-a-swipe
/// sweep, matching how little travel a native trackpad asks for).
const SWIPE_DENOM: i32 = 6;
/// Change in inter-finger spread before a pinch commits, as the denominator
/// of the pad width (1/5 = 20 %).
const PINCH_DENOM: i32 = 5;
/// How far any finger may wander for a lift to still count as a tap, as the
/// denominator of the pad width (1/20 = 5 %).
const TAP_TRAVEL_DENOM: i32 = 20;
/// Tap time window in milliseconds; the session converts it to the pad's
/// frame-timestamp ticks at construction. 250 ms is the deliberate-press
/// ceiling a native trackpad uses to separate a tap from a long press.
pub const TAP_MAX_MS: u32 = 250;

/// One finger tracked from its landing point.
#[derive(Clone, Copy, Debug)]
struct TrackedFinger {
    id: u8,
    start_x: i32,
    start_y: i32,
    x: i32,
    y: i32,
}

impl TrackedFinger {
    fn landing(contact: &TouchContact) -> Self {
        Self {
            id: contact.finger_id,
            start_x: i32::from(contact.x),
            start_y: i32::from(contact.y),
            x: i32::from(contact.x),
            y: i32::from(contact.y),
        }
    }

    fn travel(&self) -> i32 {
        (self.x - self.start_x)
            .abs()
            .max((self.y - self.start_y).abs())
    }
}

/// Host-side gesture classifier over logical touch frames.
///
/// Feed every frame the pad reports (idle repeats included — they move
/// nothing) via [`Self::push`]; each recognized gesture is returned exactly
/// once, from the frame that committed it.
#[derive(Debug)]
pub struct TouchpadClassifier {
    /// Dominant-axis minimum for a horizontal swipe (pad units).
    swipe_min_x: i32,
    /// Dominant-axis minimum for a vertical swipe (pad units).
    swipe_min_y: i32,
    /// Minimum spread change for a pinch (pad units).
    pinch_min: i32,
    /// Maximum per-finger travel for a tap (pad units).
    tap_travel_max: i32,
    /// Tap time window in frame ticks (see
    /// [`TAP_MAX_MS`](TAP_MAX_MS)) — computed by the session layer from the
    /// pad's timestamp unit.
    tap_max_ticks: u16,
    /// Fingers currently down, in landing order.
    tracked: Vec<TrackedFinger>,
    /// Highest simultaneous finger count of the stroke in progress (0 while
    /// idle) — the count a tap or swipe is named by.
    stroke_fingers: usize,
    /// Timestamp of the stroke's first touch.
    stroke_start_ts: u16,
    /// The gesture this stroke already committed, if any — latched so the
    /// rest of the stroke stays silent.
    fired: Option<TouchpadGestureId>,
}

impl TouchpadClassifier {
    /// Build a classifier for a pad of `x_size` × `y_size` native units whose
    /// frame timestamps tick `tap_max_ticks` times per
    /// [`TAP_MAX_MS`](TAP_MAX_MS) window.
    ///
    /// `tap_max_ticks` comes from the pad's
    /// [`TouchpadInfo::timestamp_units`](https://docs.rs/openlogi-hidpp) —
    /// each tick is `timestamp_units` × 0.1 ms — and is computed by the
    /// capture session, keeping this type free of wire vocabulary.
    #[must_use]
    pub fn new(x_size: u16, y_size: u16, tap_max_ticks: u16) -> Self {
        let width = i32::from(x_size.max(1));
        let height = i32::from(y_size.max(1));
        Self {
            swipe_min_x: width / SWIPE_DENOM,
            swipe_min_y: height / SWIPE_DENOM,
            pinch_min: width / PINCH_DENOM,
            tap_travel_max: width / TAP_TRAVEL_DENOM,
            tap_max_ticks,
            tracked: Vec::new(),
            stroke_fingers: 0,
            stroke_start_ts: 0,
            fired: None,
        }
    }

    /// Feed one logical frame; returns the gesture it committed, if any.
    ///
    /// Swipes and pinches commit mid-stroke, the frame the threshold crosses;
    /// taps commit on the empty frame that lifts the last finger. Any other
    /// frame returns `None`.
    pub fn push(&mut self, frame: &TouchFrame) -> Option<TouchpadGestureId> {
        if frame.contacts.is_empty() {
            return self.end_stroke(frame.timestamp);
        }
        for contact in &frame.contacts {
            match self.tracked.iter_mut().find(|t| t.id == contact.finger_id) {
                Some(tracked) => {
                    tracked.x = i32::from(contact.x);
                    tracked.y = i32::from(contact.y);
                }
                None => {
                    if self.tracked.is_empty() {
                        // A fresh stroke begins. `fired` should already be
                        // clear (the empty frame ended the last one); clearing
                        // again keeps a skipped-empty-frame stream from
                        // carrying a stale latch across strokes.
                        self.stroke_start_ts = frame.timestamp;
                        self.fired = None;
                        self.stroke_fingers = 0;
                    }
                    self.tracked.push(TrackedFinger::landing(contact));
                }
            }
        }
        // A finger absent from a non-empty frame has lifted.
        self.tracked
            .retain(|t| frame.contacts.iter().any(|c| c.finger_id == t.id));
        self.stroke_fingers = self.stroke_fingers.max(self.tracked.len());
        if self.fired.is_none()
            && let Some(gesture) = self.try_swipe().or_else(|| self.try_pinch())
        {
            self.fired = Some(gesture);
            return Some(gesture);
        }
        None
    }

    /// The stroke's last finger lifted: classify a tap if the stroke earned
    /// nothing mid-motion, then reset for the next stroke.
    fn end_stroke(&mut self, lift_ts: u16) -> Option<TouchpadGestureId> {
        if self.tracked.is_empty() {
            return None;
        }
        let gesture = if self.fired.is_none() {
            self.tap(lift_ts)
        } else {
            None
        };
        self.tracked.clear();
        self.stroke_fingers = 0;
        self.fired = None;
        gesture
    }

    /// Whether this stroke's lift is a tap: short window, fingers still near
    /// their landing points, and a two-to-four-finger count.
    fn tap(&self, lift_ts: u16) -> Option<TouchpadGestureId> {
        let id = match self.stroke_fingers {
            2 => TouchpadGestureId::TwoFingerTap,
            3 => TouchpadGestureId::ThreeFingerTap,
            4 => TouchpadGestureId::FourFingerTap,
            _ => return None,
        };
        // Wrapping subtraction keeps the window honest across the counter's
        // wrap; a bogus backward gap lands far above any plausible window and
        // simply fails it.
        if lift_ts.wrapping_sub(self.stroke_start_ts) > self.tap_max_ticks {
            return None;
        }
        if self
            .tracked
            .iter()
            .any(|t| t.travel() > self.tap_travel_max)
        {
            return None;
        }
        Some(id)
    }

    /// Whether the tracked fingers now form a committed swipe — every finger
    /// pushing the same way, mean travel past the axis minimum, and the mean
    /// reasonably axial.
    ///
    /// Two fingers never swipe: their drags are the pad's native scroll, and
    /// a classifier gesture on top would double-fire.
    fn try_swipe(&self) -> Option<TouchpadGestureId> {
        let fingers = self.tracked.len();
        if !(3..=4).contains(&fingers) {
            return None;
        }
        let mut sum_x = 0;
        let mut sum_y = 0;
        for t in &self.tracked {
            sum_x += t.x - t.start_x;
            sum_y += t.y - t.start_y;
        }
        let mean_x = sum_x / fingers as i32;
        let mean_y = sum_y / fingers as i32;
        // A straggler pulling against the mean means this is a settle or a
        // pinch, not a swipe — every finger must contribute.
        if self
            .tracked
            .iter()
            .any(|t| (t.x - t.start_x) * mean_x + (t.y - t.start_y) * mean_y < 0)
        {
            return None;
        }
        let (abs_x, abs_y) = (mean_x.abs(), mean_y.abs());
        let horizontal = abs_x >= abs_y;
        let (dominant, cross, minimum) = if horizontal {
            (abs_x, abs_y, self.swipe_min_x)
        } else {
            (abs_y, abs_x, self.swipe_min_y)
        };
        if dominant < minimum || cross > dominant / 2 {
            return None;
        }
        let rightward = horizontal && mean_x > 0;
        let downward = !horizontal && mean_y > 0;
        match fingers {
            3 => Some(match (horizontal, rightward, downward) {
                (true, true, _) => TouchpadGestureId::ThreeFingerSwipeRight,
                (true, false, _) => TouchpadGestureId::ThreeFingerSwipeLeft,
                (false, _, true) => TouchpadGestureId::ThreeFingerSwipeDown,
                (false, _, false) => TouchpadGestureId::ThreeFingerSwipeUp,
            }),
            4 => Some(match (horizontal, rightward, downward) {
                (true, true, _) => TouchpadGestureId::FourFingerSwipeRight,
                (true, false, _) => TouchpadGestureId::FourFingerSwipeLeft,
                (false, _, true) => TouchpadGestureId::FourFingerSwipeDown,
                (false, _, false) => TouchpadGestureId::FourFingerSwipeUp,
            }),
            _ => None,
        }
    }

    /// Whether the tracked fingers' spread has committed a pinch: two-finger
    /// spread by pair distance, four-finger spread by mean distance from the
    /// group's centroid.
    fn try_pinch(&self) -> Option<TouchpadGestureId> {
        match self.tracked.len() {
            2 => {
                let [a, b] = [self.tracked[0], self.tracked[1]];
                let spread = distance(a.x, a.y, b.x, b.y)
                    - distance(a.start_x, a.start_y, b.start_x, b.start_y);
                if spread.abs() < f64::from(self.pinch_min) {
                    return None;
                }
                Some(if spread > 0.0 {
                    TouchpadGestureId::TwoFingerPinchOut
                } else {
                    TouchpadGestureId::TwoFingerPinchIn
                })
            }
            4 => {
                let spread = mean_radius(&self.tracked, TrackedFinger::current)
                    - mean_radius(&self.tracked, TrackedFinger::start);
                if spread.abs() < f64::from(self.pinch_min) {
                    return None;
                }
                Some(if spread > 0.0 {
                    TouchpadGestureId::FourFingerPinchOut
                } else {
                    TouchpadGestureId::FourFingerPinchIn
                })
            }
            _ => None,
        }
    }
}

impl TrackedFinger {
    fn current(self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn start(self) -> (i32, i32) {
        (self.start_x, self.start_y)
    }
}

fn distance(ax: i32, ay: i32, bx: i32, by: i32) -> f64 {
    let (dx, dy) = (f64::from(ax - bx), f64::from(ay - by));
    (dx * dx + dy * dy).sqrt()
}

/// Mean distance of every finger's point from the group's centroid.
fn mean_radius(fingers: &[TrackedFinger], point: fn(TrackedFinger) -> (i32, i32)) -> f64 {
    let points: Vec<(i32, i32)> = fingers.iter().copied().map(point).collect();
    let count = points.len() as f64;
    let cx = points.iter().map(|&(x, _)| x).sum::<i32>() as f64 / count;
    let cy = points.iter().map(|&(_, y)| y).sum::<i32>() as f64 / count;
    points
        .iter()
        .map(|&(x, y)| distance(x, y, cx as i32, cy as i32))
        .sum::<f64>()
        / count
}
