//! The touchpad gesture vocabulary — binding keys for the
//! [`touchpad_gestures`](crate::config::DeviceConfig::touchpad_gestures) map.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One host-classified touchpad gesture: the binding key of a touchpad's
/// gesture map, resolved to an [`Action`](crate::binding::Action) by the
/// capture dispatcher.
///
/// Logitech touchpads such as the Casa Touch carry no firmware gesture
/// feature — Logitech Options+ classifies raw multi-touch frames on the host,
/// and OpenLogi mirrors that pipeline (see
/// [`TouchpadClassifier`](super::TouchpadClassifier)). This vocabulary covers
/// what a host classifier can recognize from raw frames:
///
/// - taps with two to four fingers,
/// - three- and four-finger swipes in four directions,
/// - two-finger pinch (zoom) and four-finger pinch (show desktop / Launchpad).
///
/// Two-finger *drags* are deliberately absent: the pad's firmware already
/// turns them into native scrolling, and classifying them as swipes on top
/// would double-fire. The one- and two-finger baseline (move, scroll,
/// tap-to-click, two-finger-tap right-click) likewise stays in firmware —
/// which is why the tap defaults leave it that way.
///
/// Variant identifiers are TOML-stable, append-only, exactly like
/// [`ButtonId`](crate::binding::ButtonId): the config and the IPC wire encode
/// them verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TouchpadGestureId {
    /// A short two-finger touch. Native firmware already right-clicks on it;
    /// the default binding stays `None` so the two never double-fire.
    TwoFingerTap,
    /// A short three-finger touch.
    ThreeFingerTap,
    /// A short four-finger touch.
    FourFingerTap,
    /// Three fingers swept toward the top of the pad.
    ThreeFingerSwipeUp,
    /// Three fingers swept toward the bottom of the pad.
    ThreeFingerSwipeDown,
    /// Three fingers swept toward the left edge.
    ThreeFingerSwipeLeft,
    /// Three fingers swept toward the right edge.
    ThreeFingerSwipeRight,
    /// Four fingers swept toward the top of the pad.
    FourFingerSwipeUp,
    /// Four fingers swept toward the bottom of the pad.
    FourFingerSwipeDown,
    /// Four fingers swept toward the left edge.
    FourFingerSwipeLeft,
    /// Four fingers swept toward the right edge.
    FourFingerSwipeRight,
    /// Two fingers spread apart (the zoom-in half of a pinch).
    TwoFingerPinchOut,
    /// Two fingers brought together (the zoom-out half of a pinch).
    TwoFingerPinchIn,
    /// Four fingers spread apart (macOS Launchpad shape).
    FourFingerPinchOut,
    /// Four fingers brought together (macOS show-desktop shape).
    FourFingerPinchIn,
}

impl TouchpadGestureId {
    /// Every gesture slot, in vocabulary order. Iterated to seed the effective
    /// touchpad gesture map — see
    /// [`touchpad_gestures_for`](crate::bindings::touchpad_gestures_for).
    pub const ALL: [Self; 15] = [
        Self::TwoFingerTap,
        Self::ThreeFingerTap,
        Self::FourFingerTap,
        Self::ThreeFingerSwipeUp,
        Self::ThreeFingerSwipeDown,
        Self::ThreeFingerSwipeLeft,
        Self::ThreeFingerSwipeRight,
        Self::FourFingerSwipeUp,
        Self::FourFingerSwipeDown,
        Self::FourFingerSwipeLeft,
        Self::FourFingerSwipeRight,
        Self::TwoFingerPinchOut,
        Self::TwoFingerPinchIn,
        Self::FourFingerPinchOut,
        Self::FourFingerPinchIn,
    ];

    /// Human-readable label for the gesture list in the GUI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TwoFingerTap => "Two-finger tap",
            Self::ThreeFingerTap => "Three-finger tap",
            Self::FourFingerTap => "Four-finger tap",
            Self::ThreeFingerSwipeUp => "Three-finger swipe up",
            Self::ThreeFingerSwipeDown => "Three-finger swipe down",
            Self::ThreeFingerSwipeLeft => "Three-finger swipe left",
            Self::ThreeFingerSwipeRight => "Three-finger swipe right",
            Self::FourFingerSwipeUp => "Four-finger swipe up",
            Self::FourFingerSwipeDown => "Four-finger swipe down",
            Self::FourFingerSwipeLeft => "Four-finger swipe left",
            Self::FourFingerSwipeRight => "Four-finger swipe right",
            Self::TwoFingerPinchOut => "Two-finger pinch out",
            Self::TwoFingerPinchIn => "Two-finger pinch in",
            Self::FourFingerPinchOut => "Four-finger pinch out",
            Self::FourFingerPinchIn => "Four-finger pinch in",
        }
    }
}

impl fmt::Display for TouchpadGestureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}
