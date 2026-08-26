//! Neutral logical touch frames — what the classifier consumes.
//!
//! These are host-neutral by design: `openlogi-device` converts HID++
//! `0x6100` events into them, so the classifier never sees a wire type and
//! stays testable without hardware.

/// One finger in contact with the pad, as reported by the touch controller.
///
/// Hover contacts (`contact_status != touch`), non-finger contact types and
/// palm detections are the converter's business — a [`TouchFrame`] carries
/// only fingers that count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchContact {
    /// Controller-assigned finger id, stable while the finger stays down.
    pub finger_id: u8,
    /// X in native pad units (+x = right on an upper-left-origin pad).
    pub x: u16,
    /// Y in native pad units (+y = down on an upper-left-origin pad).
    pub y: u16,
}

/// One logical touch frame: every finger touching the pad at one sampling
/// instant.
///
/// A frame with more than two fingers travels as several HID++ events sharing
/// a timestamp; the converter assembles them into this single unit before
/// classification (see `openlogi-device`'s logical-frame assembler).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TouchFrame {
    /// Running frame timestamp in pad tick units. Wraps (a 0.1 ms tick
    /// counter wraps every ~6.55 s); the classifier only ever compares two
    /// timestamps through wrapping subtraction.
    pub timestamp: u16,
    /// The fingers in contact this frame.
    pub contacts: Vec<TouchContact>,
}
