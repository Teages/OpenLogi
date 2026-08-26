//! Default bindings for a fresh device / gesture map.

use crate::touchpad::TouchpadGestureId;

use super::action::Action;
use super::button::ButtonId;
use super::gesture::GestureDirection;
use super::key_combo::KeyCombo;
use super::value::Binding;

/// Sensible defaults for a fresh device so the panel isn't empty on first run.
///
/// `GestureButton` matches what Logi Options+ ships for MX-line devices:
/// gesture button → Mission Control, captured per-direction (see
/// [`default_gesture_binding`]).
///
/// `GestureButton`'s entry here is vestigial: in the merged [`Binding`] model
/// the gesture button defaults to [`Binding::Gesture`] (see
/// [`default_binding_for`]), so this single-action value is never the source of
/// truth for it. It is retained only so the per-button-`Action` callers (the
/// hook map, scroll defaults, labels) stay total.
///
/// [`ButtonId::Thumbwheel`] — the wheel's capacitive tap — is deliberately
/// inert. The mouse model surfaces the wheel as one paired rotation control,
/// so the tap has no GUI hotspot to discover or clear it, while the firmware
/// reports a tap from incidental thumb contact (including mid-roll, from the
/// ridges alone). Seeding it with a real action therefore fired that action
/// for users who only changed the rotation bindings or the sensitivity — the
/// two settings that divert the wheel over `0x2150` in the first place. A tap
/// bound explicitly in the config still dispatches; only the seed is inert.
#[must_use]
pub fn default_binding(button: ButtonId) -> Action {
    match button {
        ButtonId::LeftClick => Action::LeftClick,
        ButtonId::RightClick => Action::RightClick,
        ButtonId::MiddleClick => Action::MiddleClick,
        // The main wheel's tilt scrolls horizontally in firmware. Seeding each
        // side with the matching scroll action is what keeps a tilt the user
        // never touched native: the capture plan diverts a control only when
        // its binding leaves this default (see `capture_plan`), so an untouched
        // tilt is never diverted and its firmware scroll is untouched.
        #[expect(
            clippy::match_same_arms,
            reason = "the tilt and the thumb wheel are separate physical controls that happen to \
                      scroll the same way; merging their arms would tie two independent defaults \
                      together"
        )]
        ButtonId::WheelTiltLeft => Action::HorizontalScrollLeft,
        #[expect(
            clippy::match_same_arms,
            reason = "see the left tilt above — same control pair, mirrored direction"
        )]
        ButtonId::WheelTiltRight => Action::HorizontalScrollRight,
        ButtonId::Back => Action::BrowserBack,
        ButtonId::Forward => Action::BrowserForward,
        ButtonId::DpiToggle => Action::CycleDpiPresets,
        #[expect(
            clippy::match_same_arms,
            reason = "the tap is inert because its captured events are noise (see above), \
                      not because the control stays native like the keyboard arm below"
        )]
        ButtonId::Thumbwheel => Action::None,
        // The thumb wheel scrolls horizontally by default: rotating it produces
        // continuous horizontal scroll, with "up" → right and "down" → left.
        // The wheel watcher renders these two actions as smooth, sensitivity-
        // scaled scrolling rather than the discrete per-press burst a button
        // would get (see `watchers::gesture`).
        ButtonId::ThumbwheelScrollUp => Action::HorizontalScrollRight,
        ButtonId::ThumbwheelScrollDown => Action::HorizontalScrollLeft,
        ButtonId::GestureButton => Action::MissionControl,
        ButtonId::HapticPanel => Action::ShowActionsRing,
        // Keyboard keys stay on their native firmware function until the user
        // explicitly binds them; an unbound key is never diverted, so a
        // `None` default keeps the projection total without capturing anything.
        ButtonId::KeySearch
        | ButtonId::KeyDictation
        | ButtonId::KeyEmoji
        | ButtonId::KeyScreenCapture
        | ButtonId::KeyMicMute
        | ButtonId::KeyPlayPause
        | ButtonId::KeyMute
        | ButtonId::KeyVolumeDown
        | ButtonId::KeyVolumeUp => Action::None,
    }
}

/// Per-direction defaults for the gesture button. These are captured live over
/// HID++ `0x1b04` (raw-XY diversion) and dispatched like any other binding; the
/// defaults give the picker something sensible to show on first run.
#[must_use]
pub fn default_gesture_binding(direction: GestureDirection) -> Action {
    match direction {
        GestureDirection::Up => Action::MissionControl,
        GestureDirection::Down => Action::ShowDesktop,
        GestureDirection::Left => Action::PrevTab,
        GestureDirection::Right => Action::NextTab,
        GestureDirection::Click => Action::AppExpose,
    }
}

/// The canonical default [`Binding`] for a fresh button in the merged model.
///
/// [`ButtonId::GestureButton`] defaults to [`Binding::Gesture`] populated from
/// [`default_gesture_binding`] — preserving the existing per-direction swipe
/// behavior — so the GUI mode toggle and the runtime agree it starts in gesture
/// mode. Every other button defaults to [`Binding::Single`] of its
/// [`default_binding`].
///
/// This is the seed when a button is first promoted to a gesture binding (see
/// [`Config::set_gesture_direction`](crate::config::Config::set_gesture_direction)),
/// so a freshly-customized gesture button always carries a full default
/// direction map — including a [`GestureDirection::Click`] — rather than a sparse
/// map whose click would project to a no-op [`Action::None`].
#[must_use]
pub fn default_binding_for(button: ButtonId) -> Binding {
    match button {
        ButtonId::GestureButton => Binding::Gesture(
            GestureDirection::ALL
                .into_iter()
                .map(|d| (d, default_gesture_binding(d)))
                .collect(),
        ),
        other => Binding::Single(default_binding(other)),
    }
}

/// Default bindings for the touchpad gesture map. The three- and four-finger
/// slots mirror what Logitech Options+ ships for a Casa Touch on macOS (its
/// `defaults_gesture_osx.json`: 3-finger swipe up = Mission Control, down =
/// App Exposé, sideways = switch desktops, 4-finger pinch = show desktop /
/// Launchpad, pinch = zoom).
///
/// The taps deliberately stay [`Action::None`]: the pad's firmware already
/// owns the touch baseline — a two-finger tap right-clicks in firmware, and
/// classifying one on top would double-fire. Binding a tap explicitly in the
/// config still dispatches; only the default is inert. Four-finger swipes
/// have no Options+ default and follow the same rule.
#[must_use]
#[expect(
    clippy::match_same_arms,
    reason = "the inert arms are distinct vocabulary groups (taps stay firmware-owned, \
              four-finger swipes have no Options+ default); merging them would tie two \
              independent default decisions together"
)]
pub fn default_touchpad_gesture(gesture: TouchpadGestureId) -> Action {
    match gesture {
        TouchpadGestureId::TwoFingerTap
        | TouchpadGestureId::ThreeFingerTap
        | TouchpadGestureId::FourFingerTap => Action::None,
        TouchpadGestureId::ThreeFingerSwipeUp => Action::MissionControl,
        TouchpadGestureId::ThreeFingerSwipeDown => Action::AppExpose,
        TouchpadGestureId::ThreeFingerSwipeLeft => Action::PreviousDesktop,
        TouchpadGestureId::ThreeFingerSwipeRight => Action::NextDesktop,
        TouchpadGestureId::FourFingerSwipeUp
        | TouchpadGestureId::FourFingerSwipeDown
        | TouchpadGestureId::FourFingerSwipeLeft
        | TouchpadGestureId::FourFingerSwipeRight => Action::None,
        TouchpadGestureId::TwoFingerPinchOut => zoom_chord("Cmd+="),
        TouchpadGestureId::TwoFingerPinchIn => zoom_chord("Cmd+-"),
        TouchpadGestureId::FourFingerPinchOut => Action::LaunchpadShow,
        TouchpadGestureId::FourFingerPinchIn => Action::ShowDesktop,
    }
}

/// The zoom chords behind the pinch defaults. Both spellings parse (locked by
/// the `pinch_default_chords_parse` test); the fallback only exists because
/// [`KeyCombo`]'s constructor is a parse, and stays dead while that test is
/// green.
fn zoom_chord(chord: &str) -> Action {
    chord
        .parse::<KeyCombo>()
        .map_or(Action::None, Action::CustomShortcut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinch_default_chords_parse() {
        // `zoom_chord`'s `Action::None` fallback exists only because the
        // constructor is a parse; if these spellings ever stop parsing the
        // pinch defaults would silently go inert, so lock them live here.
        for gesture in [
            TouchpadGestureId::TwoFingerPinchOut,
            TouchpadGestureId::TwoFingerPinchIn,
        ] {
            assert!(
                matches!(default_touchpad_gesture(gesture), Action::CustomShortcut(_)),
                "{gesture}'s default must be a real chord"
            );
        }
    }
}
