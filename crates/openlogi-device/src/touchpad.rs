//! The `0x6100 TouchpadRawXy` half of the capture session: assembling raw
//! HID++ events into logical frames and classifying them into gestures.
//!
//! A frame with more than two fingers arrives as several [`DualXyData`]
//! events sharing a timestamp (the last carrying `end_of_frame`), and a
//! resting hand re-sends the whole group at report rate.
//! [`LogicalFrameAssembler`] folds both behaviors away — one
//! [`TouchFrame`] out per distinct
//! physical frame — so the core
//! [`TouchpadClassifier`] sees a
//! clean contact stream. [`TouchpadCaptureState`] is the two glued together:
//! feed it every event, take a gesture when one commits.

use std::sync::Arc;

use hidpp::feature::EmittingFeature;
use hidpp::feature::touchpad_raw_xy::{DualXyData, RawReportFlags, TouchpadInfo};
use openlogi_core::touchpad::{
    TAP_MAX_MS, TouchContact, TouchFrame, TouchpadClassifier, TouchpadGestureId,
};

/// Whether a pending group still accepts more events: it runs until the part
/// that carries `end_of_frame`.
pub(crate) fn group_open(parts: &[DualXyData]) -> bool {
    !parts.last().is_some_and(|part| part.end_of_frame)
}

/// Whether two raw events repeat the same contact state (up to the running
/// timestamp), i.e. one is the device resending an unchanged hand.
pub(crate) fn same_contact(a: &DualXyData, b: &DualXyData) -> bool {
    a.touch1 == b.touch1
        && a.touch2 == b.touch2
        && a.button == b.button
        && a.finger_count == b.finger_count
        && a.end_of_frame == b.end_of_frame
}

/// Assembles raw [`DualXyData`] events into logical
/// [`TouchFrame`]s.
///
/// A fresh group starts whenever the timestamp moves on or the previous group
/// already ended in an `end_of_frame`; otherwise the event continues the
/// pending group. A group that repeats the contact state of the frame before
/// it (the resting-hand re-send) is dropped whole — per-event comparison
/// would let the idle stream flood right back through.
#[derive(Default)]
pub struct LogicalFrameAssembler {
    pending_ts: Option<u16>,
    pending: Vec<DualXyData>,
    /// Contact state of the last committed frame, for idle re-send
    /// suppression. Contacts only, never the timestamp — the re-send's
    /// whole point is a fresh ts on an unchanged hand.
    last: Option<Vec<TouchContact>>,
}

impl LogicalFrameAssembler {
    /// Feed one raw event; returns a completed logical frame when this event
    /// proved the *previous* group finished (lazy commit, like the probe: a
    /// group ends at its `end_of_frame`, but only the next event — or
    /// [`Self::flush`] — hands it over). One frame of latency at ~130 Hz
    /// reports; `None` while a group is still open or its predecessor was a
    /// suppressed re-send.
    pub fn push(&mut self, event: DualXyData) -> Option<TouchFrame> {
        // Commit the previous group *before* starting the next — the incoming
        // event always joins the new pending group, never dropped: the
        // classifier's stroke continuity depends on seeing every frame.
        let frame = if !self.pending.is_empty()
            && (Some(event.timestamp) != self.pending_ts || !group_open(&self.pending))
        {
            self.commit()
        } else {
            None
        };
        self.pending_ts = Some(event.timestamp);
        self.pending.push(event);
        frame
    }

    /// Commit whatever group is still open (shutdown time); `None` when
    /// nothing was pending.
    pub fn flush(&mut self) -> Option<TouchFrame> {
        self.commit()
    }

    fn commit(&mut self) -> Option<TouchFrame> {
        if self.pending.is_empty() {
            return None;
        }
        let contacts: Vec<TouchContact> = self.pending.iter().flat_map(part_contacts).collect();
        let timestamp = self.pending_ts.unwrap_or_default();
        self.pending.clear();
        if self.last.as_ref() == Some(&contacts) {
            return None;
        }
        self.last = Some(contacts.clone());
        Some(TouchFrame {
            timestamp,
            contacts,
        })
    }
}

/// The touching fingers one DualXy part carries: hover contacts, non-finger
/// contact types and empty slots drop out here, before classification.
fn part_contacts(part: &DualXyData) -> impl Iterator<Item = TouchContact> {
    [part.touch1, part.touch2]
        .into_iter()
        .filter(|touch| touch.contact_type == 0 && touch.contact_status == 1)
        .map(|touch| TouchContact {
            finger_id: touch.finger_id,
            x: touch.x,
            y: touch.y,
        })
}

/// The tap window expressed in the pad's own frame-timestamp ticks: one tick
/// is `timestamp_units` × 0.1 ms, so the [`TAP_MAX_MS`] window is
/// `ms × 10 / units` ticks. A zero unit field (none of the pads report it)
/// degenerates to the 0.1 ms reading rather than dividing by zero.
fn tap_max_ticks(timestamp_units: u8) -> u16 {
    let units = u32::from(timestamp_units.max(1));
    u16::try_from(TAP_MAX_MS.saturating_mul(10) / units).unwrap_or(u16::MAX)
}

/// Arming state for a touchpad's raw-report mode: what to hand back to the
/// firmware on disarm.
pub(crate) struct TouchpadArmed {
    /// The feature accessor; holds the event listener registration alive.
    pub feature: Arc<hidpp::feature::touchpad_raw_xy::TouchpadRawXyFeature>,
    /// The raw-report mode observed before the session turned raw on; the
    /// byte to restore. Only meaningful when `wrote` is true.
    pub original: RawReportFlags,
    /// Whether this session actually enabled raw reporting. A device already
    /// reporting raw is owned by whoever turned it on (Options+ mid-gesture,
    /// a stale session) — the session listens but never re-writes, and
    /// therefore never "restores" another owner's mode away.
    pub wrote: bool,
    /// Pad characteristics, captured at arming for the classifier's geometry.
    pub info: TouchpadInfo,
}

/// Assembler + classifier for one capture session: feed raw events, take
/// recognized gestures.
pub struct TouchpadCaptureState {
    assembler: LogicalFrameAssembler,
    classifier: TouchpadClassifier,
}

impl TouchpadCaptureState {
    /// Build the capture state for an armed pad: its sensor geometry and its
    /// frame-timestamp unit (in 0.1 ms ticks, from `TouchpadInfo`).
    #[must_use]
    pub fn new(x_size: u16, y_size: u16, timestamp_units: u8) -> Self {
        Self {
            assembler: LogicalFrameAssembler::default(),
            classifier: TouchpadClassifier::new(x_size, y_size, tap_max_ticks(timestamp_units)),
        }
    }

    /// Feed one raw event; returns the gesture it committed, if any.
    pub fn feed(&mut self, event: DualXyData) -> Option<TouchpadGestureId> {
        let frame = self.assembler.push(event)?;
        self.classifier.push(&frame)
    }

    /// Flush a still-open frame group at shutdown and classify it. An idle
    /// stream's pending group is the resting hand re-send and commits
    /// nothing.
    pub fn flush(&mut self) -> Option<TouchpadGestureId> {
        let frame = self.assembler.flush()?;
        self.classifier.push(&frame)
    }
}

/// The live half of an armed touchpad: its raw-event stream plus the state
/// classifying it. `TouchpadArmed` owns the firmware side (mode write and
/// restore); this owns the consuming side once the session starts listening.
pub struct TouchpadCapture {
    events: async_channel::Receiver<hidpp::feature::touchpad_raw_xy::TouchpadRawEvent>,
    state: TouchpadCaptureState,
}

impl TouchpadCapture {
    /// Start consuming an armed touchpad's events.
    #[must_use]
    pub(crate) fn armed(armed: &TouchpadArmed) -> Self {
        Self {
            events: armed.feature.listen(),
            state: TouchpadCaptureState::new(
                armed.info.x_size,
                armed.info.y_size,
                armed.info.timestamp_units,
            ),
        }
    }

    /// Await the next raw event; `None` once the event source drops (the
    /// channel is going away — the session's liveness watchdog is what
    /// eventually acts on that).
    pub async fn next_event(
        &mut self,
    ) -> Option<hidpp::feature::touchpad_raw_xy::TouchpadRawEvent> {
        self.events.recv().await.ok()
    }

    /// Consume one raw event; returns the gesture it committed, if any.
    pub fn feed(
        &mut self,
        event: hidpp::feature::touchpad_raw_xy::TouchpadRawEvent,
    ) -> Option<TouchpadGestureId> {
        match event {
            hidpp::feature::touchpad_raw_xy::TouchpadRawEvent::DualXy(frame) => {
                self.state.feed(frame)
            }
            // `TouchpadRawEvent` is `#[non_exhaustive]`; a future variant the
            // assembler can't consume yet is inert, not an error.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use hidpp::feature::touchpad_raw_xy::TouchPoint;

    use super::*;

    fn touch(id: u8, x: u16, y: u16) -> TouchPoint {
        TouchPoint {
            contact_type: 0,
            contact_status: 1,
            x,
            y,
            finger_id: id,
            z: 0,
            area: 0,
        }
    }

    /// One DualXy event: two slots, `end_of_frame`, and the frame's finger
    /// count. A hover slot carries a stale id at zero coordinates — the
    /// reserved empty slot the pad pads partial frames with.
    fn part(ts: u16, slot1: TouchPoint, slot2: TouchPoint, fingers: u8, end: bool) -> DualXyData {
        DualXyData {
            timestamp: ts,
            touch1: slot1,
            touch2: slot2,
            button: false,
            end_of_frame: end,
            finger_count: fingers,
        }
    }

    fn hover(touch: TouchPoint) -> TouchPoint {
        TouchPoint {
            contact_status: 0,
            ..touch
        }
    }

    #[test]
    fn split_frame_events_assemble_into_one_logical_frame() {
        // A four-finger frame arrives as two events sharing ts. The group
        // closes on its end_of_frame part but commits lazily — the next
        // event (here: flush) hands it over as ONE frame.
        let mut assembler = LogicalFrameAssembler::default();
        assert_eq!(
            assembler.push(part(100, touch(0, 500, 800), touch(1, 600, 800), 4, false)),
            None,
            "an unterminated group must not commit"
        );
        assert_eq!(
            assembler.push(part(100, touch(2, 700, 800), touch(3, 800, 800), 4, true)),
            None,
            "the closing part itself does not commit — the next event does"
        );
        let frame = assembler
            .flush()
            .expect("flush hands the closed group over");
        assert_eq!(frame.timestamp, 100);
        assert_eq!(frame.contacts.len(), 4);
        assert_eq!(frame.contacts[2].finger_id, 2);
    }

    #[test]
    fn resting_hand_resends_are_dropped_whole() {
        // Same contacts at a fresh timestamp = the idle re-send; after the
        // hand's first frame commits, every unchanged re-send is suppressed,
        // and real motion comes through again.
        let mut assembler = LogicalFrameAssembler::default();
        assembler.push(part(100, touch(0, 500, 800), touch(1, 600, 800), 2, true));
        // The re-send at ts 177 lazily commits the ts-100 group.
        assert!(
            assembler
                .push(part(177, touch(0, 500, 800), touch(1, 600, 800), 2, true))
                .is_some(),
            "the first physical frame commits when the re-send arrives"
        );
        assert!(
            assembler
                .push(part(254, touch(0, 500, 800), touch(1, 600, 800), 2, true))
                .is_none(),
            "an unchanged hand re-sent at report rate must be suppressed"
        );
        assert!(
            assembler
                .push(part(331, touch(0, 520, 800), touch(1, 620, 800), 2, true))
                .is_none(),
            "the still-resting group before the move suppresses too"
        );
        assert!(
            assembler
                .push(part(408, touch(0, 520, 800), touch(1, 620, 800), 2, true))
                .is_some(),
            "the moved frame comes through one event later (lazy commit)"
        );
    }

    #[test]
    fn hover_contacts_do_not_reach_the_frame() {
        let mut assembler = LogicalFrameAssembler::default();
        assembler.push(part(
            100,
            touch(0, 500, 800),
            hover(touch(1, 600, 800)),
            1,
            true,
        ));
        let frame = assembler
            .flush()
            .expect("flush commits the single-event frame");
        assert_eq!(
            frame.contacts,
            vec![TouchContact {
                finger_id: 0,
                x: 500,
                y: 800
            }],
            "a hover contact must be filtered before classification"
        );
    }

    #[test]
    fn end_to_end_three_finger_swipe_through_the_capture_state() {
        // DualXy events in, TouchpadGestureId out — the shape the session's
        // select loop drives. Casa Touch geometry, three fingers sweeping
        // right past the swipe minimum (2775/6 = 462). The trailing slot of
        // each 3-finger frame is the reserved empty hover placeholder.
        // Commit is lazy, so the swipe surfaces on the event *after* the
        // frame that crossed the threshold — here, the lift (empty frame).
        let empty = |ts: u16| part(ts, hover(touch(0, 0, 0)), hover(touch(0, 0, 0)), 0, true);
        let mut state = TouchpadCaptureState::new(2775, 1786, 1);
        assert_eq!(
            state.feed(part(100, touch(0, 600, 800), touch(1, 620, 860), 3, false)),
            None
        );
        assert_eq!(
            state.feed(part(
                100,
                touch(2, 640, 920),
                hover(touch(0, 0, 0)),
                3,
                true
            )),
            None,
            "the landing frame must not commit anything"
        );
        // Slide all three right by 500 units across two frames.
        assert_eq!(
            state.feed(part(
                400,
                touch(0, 1100, 800),
                touch(1, 1120, 860),
                3,
                false
            )),
            None
        );
        assert_eq!(
            state.feed(part(
                400,
                touch(2, 1140, 920),
                hover(touch(0, 0, 0)),
                3,
                true
            )),
            None,
            "the crossing frame itself is still pending"
        );
        assert_eq!(
            state.feed(empty(700)),
            Some(TouchpadGestureId::ThreeFingerSwipeRight)
        );
    }

    #[test]
    fn tap_window_converts_timestamp_units_to_ticks() {
        // 1 × 0.1 ms ticks: 250 ms = 2500 ticks. 2 × 0.1 ms: 1250.
        assert_eq!(tap_max_ticks(1), 2500);
        assert_eq!(tap_max_ticks(2), 1250);
        assert_eq!(tap_max_ticks(10), 250);
        // A zero unit field degenerates to the 0.1 ms reading, not /0.
        assert_eq!(tap_max_ticks(0), 2500);
    }
}
