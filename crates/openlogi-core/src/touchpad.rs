//! Touchpad gesture vocabulary and the host-side classifier that recognizes
//! gestures from raw multi-touch frames.
//!
//! Logitech touchpads such as the Casa Touch expose no firmware gesture
//! feature over HID++; Logitech Options+ instead enables the pad's raw
//! reporting (`0x6100 TouchpadRawXy`) and classifies the raw frames on the
//! host. OpenLogi mirrors that pipeline: the capture session assembles raw
//! events into logical [`TouchFrame`]s and feeds them to
//! [`TouchpadClassifier`], which emits [`TouchpadGestureId`]s — bound to
//! actions through the config's `touchpad_gestures` map and dispatched like
//! any other captured input.
//!
//! Everything here is pure input processing over neutral types, the touchpad
//! counterpart of [`binding::detect_swipe`](crate::binding::detect_swipe) —
//! no I/O, no wire vocabulary, no clock.

mod classify;
mod frame;
mod gesture;

#[cfg(test)]
mod tests;

pub use classify::{TAP_MAX_MS, TouchpadClassifier};
pub use frame::{TouchContact, TouchFrame};
pub use gesture::TouchpadGestureId;
