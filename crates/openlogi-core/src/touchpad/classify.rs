//! Classify touchpad gestures from logical [`TouchFrame`]s.
//!
//! One [`TouchpadClassifier`] per capture session tracks fingers by id across
//! frames. Swipes commit **mid-stroke** on a *velocity-consistency* basis —
//! N consecutive frames of co-directed mean motion past a small travel floor
//! — and taps commit at lift-off. A committed gesture latches until every
//! finger lifts, so one physical stroke fires at most one gesture.
//!
//! The strategy and every threshold mirror Logitech Options+'s own gesture
//! engine (`NFingerHorzSwipeRecognizer` and kin, extracted from
//! `logioptionsplus_agent`'s `RegisterProperty` tables): swipes there are
//! gated by *minimum tracking frames* (5) and *minimum tracking time*
//! (50 ms horizontal / 35 ms vertical) rather than by absolute travel; the
//! cross-axis rule caps the span **between fingers** (0.4 of the pad axis,
//! resetting confidence rather than vetoing); a finger drifting against the
//! sweep lowers confidence instead of rejecting the gesture; and the
//! gesture stays valid while at least two fingers remain. Distances are
//! fractions of the pad's reported size, so one implementation tunes itself
//! across pads.
//!
//! The stroke unit is the all-fingers-lift boundary: while any finger stays
//! down the stroke continues, and the first empty frame ends it. A stream
//! that skips the empty frame (a finger swap) merges two strokes; merged
//! strokes still classify conservatively (the fresh finger's landing point
//! re-bases the swipe tracker).
//!
//! Coordinates follow the upper-left-origin convention the pads report
//! (`+x` right, `+y` down), the same convention as
//! [`detect_swipe`](crate::binding::detect_swipe).

use super::frame::{TouchContact, TouchFrame};
use super::gesture::TouchpadGestureId;

/// Consecutive co-directed frames before a swipe may commit
/// (`THREE_FINGER_*_SWIPE_MINIMUM_TRACKING_FRAMES_COUNT`, default 5).
const SWIPE_MIN_FRAMES: u32 = 5;
/// Swipe tracking window in milliseconds, derived from the horizontal
/// recognizer's `…MINIMUM_TRACKING_TIME` default (50 ms; the vertical one is
/// 35 ms — one value keeps the arithmetic simple and errs responsive).
pub const SWIPE_MIN_MS: u32 = 50;
/// Minimum accumulated mean travel for a swipe to commit, as the denominator
/// of the pad width (1/100 = 1 %, ~1 mm). Options+ has no travel floor —
/// its confidence engine plays that role; this floor is our substitute,
/// sized above sensor jitter and far below any deliberate sweep (the frame
/// and time gates carry the real filtering).
const SWIPE_TRAVEL_DENOM: i32 = 100;
/// Cross-axis span *between fingers* a swipe tolerates, as the denominator
/// of that axis's pad size (1/2.5 = 40 %,
/// `THREE_FINGER_*_SWIPE_MAXIMUM_VERTICAL_SPAN`, default 0.4).
const CROSS_SPAN_NUM: i32 = 2;
const CROSS_SPAN_DENOM: i32 = 5;
/// Per-frame mean motion below which nothing is learned — neither a run
/// starts nor resets. Absorbs sensor jitter in an otherwise still hand.
const FRAME_DEADZONE: f64 = 3.0;
/// A fast, short snap — a flick — may commit after only three frames,
/// skipping the time gate: `FLICK_FRAMES` frames carrying 8× the travel
/// floor is ~72 units/frame on this pad (≈ 40 cm/s), a genuine snap that
/// deliberate slow motion cannot fake while staying co-directed.
const FLICK_FRAMES: u32 = 3;
const FLICK_TRAVEL_MULT: f64 = 8.0;
/// How much further the average finger may move than the group's mean
/// motion before the frame reads as converge/spread rather than a
/// co-directed sweep (our stand-in for the engine's recognizer
/// arbitration — a pinch's fingers move far while its centroid barely does).
const CO_MOTION_MAX_RATIO: f64 = 2.5;
/// Change in inter-finger spread before a pinch commits, as the denominator
/// of the pad width (1/8 = 12.5 %; Options+ finishes pinches on
/// progress+speed thresholds we did not decode — this is our stand-in).
const PINCH_DENOM: i32 = 8;
/// How far any finger may wander for a lift to still count as a tap, as the
/// denominator of the pad width (1/25 = 4 %,
/// `THREE_FINGER_SINGLETAP_MAXIMUM_DISTANCE`, default 0.04).
const TAP_TRAVEL_DENOM: i32 = 25;
/// Tap time window in milliseconds (`…SINGLETAP_MAXIMUM_FINGER_DOWN_DURATION`
/// default 200 ms); the session converts it to the pad's frame ticks at
/// construction.
pub const TAP_MAX_MS: u32 = 200;

/// One finger tracked from its landing point.
#[derive(Clone, Copy, Debug)]
struct TrackedFinger {
    id: u8,
    start_x: i32,
    start_y: i32,
    x: i32,
    y: i32,
    prev_x: i32,
    prev_y: i32,
}

impl TrackedFinger {
    fn landing(contact: TouchContact) -> Self {
        Self {
            id: contact.finger_id,
            start_x: i32::from(contact.x),
            start_y: i32::from(contact.y),
            x: i32::from(contact.x),
            y: i32::from(contact.y),
            prev_x: i32::from(contact.x),
            prev_y: i32::from(contact.y),
        }
    }

    fn travel(&self) -> i32 {
        (self.x - self.start_x)
            .abs()
            .max((self.y - self.start_y).abs())
    }

    fn current(self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn start(self) -> (i32, i32) {
        (self.start_x, self.start_y)
    }
}

/// The locked axis and sign of a swipe run in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwipeDirection {
    Right,
    Left,
    Down,
    Up,
}

impl SwipeDirection {
    fn horizontal(self) -> bool {
        matches!(self, Self::Right | Self::Left)
    }
}

/// Mid-stroke swipe state: a run of consecutive co-directed frames.
///
/// Tracks the *per-frame mean displacement* (velocity), not displacement
/// from each finger's landing — a straggler finger dragging backwards
/// shifts the mean a little but never vetoes, exactly like the confidence
/// model Options+ runs.
#[derive(Default, Debug)]
struct SwipeTracker {
    /// The run's locked direction, once motion is unambiguous.
    locked: Option<SwipeDirection>,
    /// Consecutive frames that agreed with [`Self::locked`].
    frames: u32,
    /// Timestamp of the run's first frame (the time gate).
    start_ts: u16,
    /// Accumulated mean displacement while the run held.
    acc: (f64, f64),
    /// Mean tracked-finger position of the previous frame; `None` for one
    /// frame after the tracked set changes composition (a lift re-bases, an
    /// added finger restarts the run — see [`Self::rebase`]).
    last_mean: Option<(f64, f64)>,
}

impl SwipeTracker {
    /// Full reset: direction flipped, cross-span exceeded, or a finger
    /// landed (attribution of the new finger's motion is unclear).
    fn restart(&mut self) {
        self.locked = None;
        self.frames = 0;
        self.acc = (0.0, 0.0);
    }

    /// Soft rebase after a finger lifted: the mean jumps by the departed
    /// finger's offset, so skip exactly one frame's delta but keep the run.
    fn rebase(&mut self) {
        self.last_mean = None;
    }
}

/// Host-side gesture classifier over logical touch frames.
///
/// Feed every frame the pad reports (idle repeats included — they hold, not
/// reset) via [`Self::push`]; each recognized gesture is returned exactly
/// once, from the frame that committed it.
#[derive(Debug)]
pub struct TouchpadClassifier {
    /// Minimum accumulated mean travel for a swipe commit (pad units).
    swipe_travel_floor: i32,
    /// Cross-axis span between fingers a horizontal swipe tolerates.
    cross_span_max_y: i32,
    /// Cross-axis span between fingers a vertical swipe tolerates.
    cross_span_max_x: i32,
    /// Minimum spread change for a pinch (pad units).
    pinch_min: i32,
    /// Maximum per-finger wander for a tap (pad units).
    tap_travel_max: i32,
    /// Tap time window in frame ticks.
    tap_max_ticks: u16,
    /// Swipe time gate in frame ticks.
    swipe_min_ticks: u16,
    /// Fingers currently down, in landing order.
    tracked: Vec<TrackedFinger>,
    /// Highest simultaneous finger count of the stroke in progress (0 while
    /// idle) — the count a tap or swipe is named by.
    stroke_fingers: usize,
    /// Timestamp of the stroke's first touch.
    stroke_start_ts: u16,
    /// Widest per-finger travel seen during the stroke (survives lifts, so
    /// a tap whose fingers lifted early is still judged on their wander).
    max_wander: i32,
    /// The gesture this stroke already committed, if any — latched so the
    /// rest of the stroke stays silent.
    fired: Option<TouchpadGestureId>,
    swipe: SwipeTracker,
}

impl TouchpadClassifier {
    /// Build a classifier for a pad of `x_size` × `y_size` native units whose
    /// frame timestamps tick `tap_max_ticks` times per the
    /// [`TAP_MAX_MS`] window (computed by the capture session from the pad's
    /// timestamp unit, keeping this type free of wire vocabulary).
    #[must_use]
    pub fn new(x_size: u16, y_size: u16, tap_max_ticks: u16) -> Self {
        let width = i32::from(x_size.max(1));
        let height = i32::from(y_size.max(1));
        // 50 ms of the 200 ms window, in the pad's own ticks.
        let swipe_min_ticks = u16::try_from(u32::from(tap_max_ticks) * SWIPE_MIN_MS / TAP_MAX_MS)
            .unwrap_or(1)
            .max(1);
        Self {
            swipe_travel_floor: width / SWIPE_TRAVEL_DENOM,
            cross_span_max_y: height * CROSS_SPAN_NUM / CROSS_SPAN_DENOM,
            cross_span_max_x: width * CROSS_SPAN_NUM / CROSS_SPAN_DENOM,
            pinch_min: width / PINCH_DENOM,
            tap_travel_max: width / TAP_TRAVEL_DENOM,
            tap_max_ticks,
            swipe_min_ticks,
            tracked: Vec::new(),
            stroke_fingers: 0,
            stroke_start_ts: 0,
            max_wander: 0,
            fired: None,
            swipe: SwipeTracker::default(),
        }
    }

    /// Feed one logical frame; returns the gesture it committed, if any.
    ///
    /// Swipes and pinches commit mid-stroke, the frame their gates pass;
    /// taps commit on the empty frame that lifts the last finger. Any other
    /// frame returns `None`.
    pub fn push(&mut self, frame: &TouchFrame) -> Option<TouchpadGestureId> {
        if frame.contacts.is_empty() {
            return self.end_stroke(frame.timestamp);
        }
        let before: Vec<u8> = self.tracked.iter().map(|t| t.id).collect();
        for contact in &frame.contacts {
            if let Some(tracked) = self.tracked.iter_mut().find(|t| t.id == contact.finger_id) {
                tracked.prev_x = tracked.x;
                tracked.prev_y = tracked.y;
                tracked.x = i32::from(contact.x);
                tracked.y = i32::from(contact.y);
            } else {
                if self.tracked.is_empty() {
                    // A fresh stroke begins. `fired` should already be clear
                    // (the empty frame ended the last one); clearing again
                    // keeps a skipped-empty-frame stream from carrying a
                    // stale latch across strokes.
                    self.stroke_start_ts = frame.timestamp;
                    self.fired = None;
                    self.stroke_fingers = 0;
                    self.max_wander = 0;
                    self.swipe.restart();
                }
                self.tracked.push(TrackedFinger::landing(*contact));
            }
        }
        // A finger absent from a non-empty frame has lifted.
        self.tracked
            .retain(|t| frame.contacts.iter().any(|c| c.finger_id == t.id));
        if before != self.tracked.iter().map(|t| t.id).collect::<Vec<_>>() {
            // Composition changed. A lift re-bases the mean for one frame
            // (the run survives — Options+ finishes swipes while ≥2 fingers
            // remain); a landing restarts the run outright.
            let landed = self.tracked.len() > before.len();
            if landed {
                self.swipe.restart();
            }
            self.swipe.rebase();
        }
        self.stroke_fingers = self.stroke_fingers.max(self.tracked.len());
        self.max_wander = self.max_wander.max(
            self.tracked
                .iter()
                .map(TrackedFinger::travel)
                .max()
                .unwrap_or(0),
        );
        if self.fired.is_none() {
            let gesture = self.try_swipe(frame.timestamp).or_else(|| self.try_pinch());
            if let Some(gesture) = gesture {
                self.fired = Some(gesture);
                return Some(gesture);
            }
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
        self.max_wander = 0;
        self.fired = None;
        self.swipe.restart();
        self.swipe.rebase();
        gesture
    }

    /// Whether this stroke's lift is a tap: short window, no finger wandered
    /// far, and a two-to-four-finger count.
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
        if self.max_wander > self.tap_travel_max {
            return None;
        }
        Some(id)
    }

    /// Feed the swipe tracker one frame; returns the swipe the frame
    /// committed, if any.
    ///
    /// Eligible while at least two fingers remain down and the stroke peaked
    /// at three or four — a sweep whose third finger lifts early is still
    /// the sweep the stroke began as.
    fn try_swipe(&mut self, ts: u16) -> Option<TouchpadGestureId> {
        if self.tracked.len() < 2 || !(3..=4).contains(&self.stroke_fingers) {
            return None;
        }
        let count = f64::from(u32::try_from(self.tracked.len()).unwrap_or(u32::MAX));
        let mean = (
            f64::from(self.tracked.iter().map(|t| t.x).sum::<i32>()) / count,
            f64::from(self.tracked.iter().map(|t| t.y).sum::<i32>()) / count,
        );
        let Some(last) = self.swipe.last_mean.replace(mean) else {
            return None; // first frame after a rebase — no honest delta
        };
        let (dx, dy) = (mean.0 - last.0, mean.1 - last.1);
        let (abs_x, abs_y) = (dx.abs(), dy.abs());
        let horizontal = abs_x >= abs_y;
        let dominant = abs_x.max(abs_y);
        let frame_dir = if dominant < FRAME_DEADZONE {
            // Jitter in an otherwise still hand: hold the run, learn nothing.
            return None;
        } else if horizontal {
            if dx > 0.0 {
                SwipeDirection::Right
            } else {
                SwipeDirection::Left
            }
        } else if dy > 0.0 {
            SwipeDirection::Down
        } else {
            SwipeDirection::Up
        };
        // Co-motion gate: a swipe translates its fingers together, so the
        // per-finger speeds average barely more than the group's mean speed.
        // Fingers moving much further than the mean are converging or
        // spreading (a pinch or rotate) — that must not lock a swipe, and
        // one arcing straggler (the ratio stays near 1) must not break one.
        let mean_speed = f64::from(
            self.tracked
                .iter()
                .map(|t| (t.x - t.prev_x).abs().max((t.y - t.prev_y).abs()))
                .sum::<i32>(),
        ) / count;
        if mean_speed > dominant * CO_MOTION_MAX_RATIO {
            return None;
        }
        match self.swipe.locked {
            Some(locked) if locked == frame_dir => {
                self.swipe.frames += 1;
                self.swipe.acc.0 += dx;
                self.swipe.acc.1 += dy;
            }
            Some(_) => {
                // Direction flipped or the dominant axis swung: confidence
                // reset, like the engine's span/direction resets.
                self.swipe.restart();
                self.swipe.locked = Some(frame_dir);
                self.swipe.start_ts = ts;
                self.swipe.frames = 1;
                self.swipe.acc = (dx, dy);
                return None;
            }
            None => {
                // Options+ commits swipes on tracking time and frame count
                // rather than absolute travel; the cross-axis rule bounds
                // the span BETWEEN fingers, not the travel ratio.
                if self.cross_span_ok(frame_dir) {
                    self.swipe.locked = Some(frame_dir);
                    self.swipe.start_ts = ts;
                    self.swipe.frames = 1;
                    self.swipe.acc = (dx, dy);
                }
                return None;
            }
        }
        if !self.cross_span_ok(frame_dir) {
            self.swipe.restart();
            return None;
        }
        let travel = if frame_dir.horizontal() {
            self.swipe.acc.0.abs()
        } else {
            self.swipe.acc.1.abs()
        };
        let held = ts.wrapping_sub(self.swipe.start_ts);
        // The ordinary gate is frames + time + floor; a flick substitutes
        // speed for time — the same travel in a third of the frames.
        let steady = self.swipe.frames >= SWIPE_MIN_FRAMES
            && held >= self.swipe_min_ticks
            && travel >= f64::from(self.swipe_travel_floor);
        let flick = self.swipe.frames >= FLICK_FRAMES
            && travel >= f64::from(self.swipe_travel_floor) * FLICK_TRAVEL_MULT;
        if steady || flick {
            let fingers = match self.stroke_fingers {
                3 | 4 => u8::try_from(self.stroke_fingers).unwrap_or(3),
                _ => return None,
            };
            return Some(swipe_id(fingers, frame_dir));
        }
        None
    }

    /// The cross-axis span gate: a horizontal sweep keeps its fingers within
    /// 40 % of the pad's height (vertical: 40 % of the width). Measured
    /// between the outermost fingers — the sweep's own cross travel is not
    /// what this bounds.
    fn cross_span_ok(&self, direction: SwipeDirection) -> bool {
        let span = |xs: &[i32]| xs.iter().max().unwrap_or(&0) - xs.iter().min().unwrap_or(&0);
        if direction.horizontal() {
            span(&self.tracked.iter().map(|t| t.y).collect::<Vec<_>>()) <= self.cross_span_max_y
        } else {
            span(&self.tracked.iter().map(|t| t.x).collect::<Vec<_>>()) <= self.cross_span_max_x
        }
    }

    /// Whether the tracked fingers' spread has committed a pinch: two-finger
    /// spread by pair distance, four-finger spread by mean distance from the
    /// group's centroid.
    fn try_pinch(&self) -> Option<TouchpadGestureId> {
        match self.tracked.len() {
            2 => {
                let [a, b] = [self.tracked[0], self.tracked[1]];
                let spread = distance(
                    f64::from(a.x),
                    f64::from(a.y),
                    f64::from(b.x),
                    f64::from(b.y),
                ) - distance(
                    f64::from(a.start_x),
                    f64::from(a.start_y),
                    f64::from(b.start_x),
                    f64::from(b.start_y),
                );
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

fn swipe_id(fingers: u8, direction: SwipeDirection) -> TouchpadGestureId {
    let three = fingers == 3;
    match direction {
        SwipeDirection::Right => {
            if three {
                TouchpadGestureId::ThreeFingerSwipeRight
            } else {
                TouchpadGestureId::FourFingerSwipeRight
            }
        }
        SwipeDirection::Left => {
            if three {
                TouchpadGestureId::ThreeFingerSwipeLeft
            } else {
                TouchpadGestureId::FourFingerSwipeLeft
            }
        }
        SwipeDirection::Down => {
            if three {
                TouchpadGestureId::ThreeFingerSwipeDown
            } else {
                TouchpadGestureId::FourFingerSwipeDown
            }
        }
        SwipeDirection::Up => {
            if three {
                TouchpadGestureId::ThreeFingerSwipeUp
            } else {
                TouchpadGestureId::FourFingerSwipeUp
            }
        }
    }
}

fn distance(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let (dx, dy) = (ax - bx, ay - by);
    (dx * dx + dy * dy).sqrt()
}

/// Mean distance of every finger's point from the group's centroid.
fn mean_radius(fingers: &[TrackedFinger], point: fn(TrackedFinger) -> (i32, i32)) -> f64 {
    let points: Vec<(i32, i32)> = fingers.iter().copied().map(point).collect();
    let count = f64::from(u32::try_from(points.len()).unwrap_or(u32::MAX));
    let cx = f64::from(points.iter().map(|&(x, _)| x).sum::<i32>()) / count;
    let cy = f64::from(points.iter().map(|&(_, y)| y).sum::<i32>()) / count;
    points
        .iter()
        .map(|&(x, y)| distance(f64::from(x), f64::from(y), cx, cy))
        .sum::<f64>()
        / count
}
