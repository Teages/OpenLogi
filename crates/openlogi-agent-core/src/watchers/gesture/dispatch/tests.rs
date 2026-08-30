use openlogi_core::touchpad::{TouchContact, TouchFrame};
use openlogi_hid::thumbwheel::WheelResolution;
use openlogi_inject::DockSwipeMotion;

use super::wheel::{ScrollScale, WheelOutput, WheelRotation};
use super::*;

fn contact(id: u8, x_um: u32, y_um: u32) -> TouchContact {
    TouchContact { id, x_um, y_um }
}

fn frame(timestamp_us: u64, contacts: Vec<TouchContact>) -> TouchFrame {
    TouchFrame::new(timestamp_us, false, contacts).expect("test contacts have unique ids")
}

/// Three- or four-finger stroke moving right, the same shape the core
/// recognizer tests use to commit a swipe.
fn translated_frame(
    timestamp_us: u64,
    count: u8,
    horizontal_um: i32,
    vertical_um: i32,
) -> TouchFrame {
    let contacts = (0..count)
        .map(|id| {
            let x = 50_000_i32 + i32::from(id) * 10_000 + horizontal_um;
            let y = 50_000_i32 + vertical_um;
            contact(
                id + 1,
                u32::try_from(x).expect("test x stays positive"),
                u32::try_from(y).expect("test y stays positive"),
            )
        })
        .collect();
    frame(timestamp_us, contacts)
}

fn idle() -> TouchpadOutcome {
    TouchpadOutcome::default()
}

fn rotation(magnitude: i32) -> WheelRotation {
    let increments = i16::try_from(magnitude).expect("test magnitude fits in i16");
    WheelRotation::from_increments(increments).expect("non-zero test rotation")
}

fn scale() -> ScrollScale {
    ScrollScale::new(WheelResolution::UNKNOWN, ThumbwheelSensitivity::DEFAULT)
}

#[test]
fn replacement_session_does_not_inherit_progress_or_cooldown() {
    let old = HidppSessionId::with_epoch("mouse-a", 7);
    let replacement = HidppSessionId::with_epoch("mouse-a", 8);
    let threshold = ThumbwheelSensitivity::DEFAULT.action_threshold();
    let now = Instant::now();
    let mut wheels = SessionWheels::default();

    assert_eq!(
        wheels
            .for_session(&old)
            .advance(rotation(threshold), &Action::VolumeUp, scale(), now,),
        WheelOutput::FireAction
    );
    assert_eq!(
        wheels.for_session(&replacement).advance(
            rotation(threshold),
            &Action::VolumeUp,
            scale(),
            now,
        ),
        WheelOutput::FireAction,
        "a new session must not inherit the old session's cooldown"
    );

    wheels.cancel_session(&old);
    assert!(
        wheels.0.contains_key(&replacement),
        "canceling a stale epoch must not erase its replacement's state"
    );
}

#[test]
fn replacement_session_does_not_inherit_partial_progress() {
    let old = HidppSessionId::with_epoch("mouse-a", 7);
    let replacement = HidppSessionId::with_epoch("mouse-a", 8);
    let threshold = ThumbwheelSensitivity::DEFAULT.action_threshold();
    let now = Instant::now();
    let mut wheels = SessionWheels::default();

    assert_eq!(
        wheels
            .for_session(&old)
            .advance(rotation(threshold - 1), &Action::VolumeUp, scale(), now,),
        WheelOutput::Idle
    );
    assert_eq!(
        wheels
            .for_session(&replacement)
            .advance(rotation(1), &Action::VolumeUp, scale(), now,),
        WheelOutput::Idle,
        "a new session must start with no action progress"
    );
}

#[test]
fn touchpad_stroke_freezes_bindings_from_its_first_frame() {
    use openlogi_core::touchpad::TouchContact;

    let frame = TouchFrame::new(
        1_000,
        false,
        vec![
            TouchContact {
                id: 1,
                x_um: 10_000,
                y_um: 10_000,
            },
            TouchContact {
                id: 2,
                x_um: 20_000,
                y_um: 10_000,
            },
        ],
    )
    .expect("valid frame");
    let trigger = ButtonId::TouchpadTwoFingerTap;
    let mut runtime = TouchpadRuntime::default();
    let first_profile = BTreeMap::from([(trigger, Action::Copy)]);
    let replacement_profile = BTreeMap::from([(trigger, Action::Paste)]);

    assert_eq!(runtime.update(&frame, &first_profile, true, false), idle());
    // A foreground-app change can replace the live plan before lift. The tap
    // must still resolve against the profile active when the stroke began.
    assert_eq!(
        runtime.end(true).action,
        Some((ButtonId::TouchpadTwoFingerTap, Action::Copy))
    );

    assert_eq!(
        runtime.update(&frame, &replacement_profile, true, false),
        idle()
    );
    assert_eq!(
        runtime.end(true).action,
        Some((ButtonId::TouchpadTwoFingerTap, Action::Paste))
    );
}

#[test]
fn diagnostic_touchpad_stroke_cannot_fire_if_management_enables_mid_stroke() {
    use openlogi_core::touchpad::TouchContact;

    let frame = TouchFrame::new(
        1_000,
        false,
        vec![
            TouchContact {
                id: 1,
                x_um: 10_000,
                y_um: 10_000,
            },
            TouchContact {
                id: 2,
                x_um: 20_000,
                y_um: 10_000,
            },
        ],
    )
    .expect("valid frame");
    let trigger = ButtonId::TouchpadTwoFingerTap;
    let bindings = BTreeMap::from([(trigger, Action::Copy)]);
    let mut runtime = TouchpadRuntime::default();

    assert_eq!(runtime.update(&frame, &bindings, false, false), idle());
    assert_eq!(runtime.end(true).action, None);

    assert_eq!(runtime.update(&frame, &bindings, true, false), idle());
    assert_eq!(runtime.end(true).action, Some((trigger, Action::Copy)));
}

#[test]
fn native_swipe_streams_progress_instead_of_dispatching() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();

    assert_eq!(
        runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true),
        idle()
    );
    // The commit frame only seeds the stream; the animation opens on the
    // first frame with actual travel, whose delta becomes the Began progress
    // (the vertical consumer ignores a zero-progress Began). The discrete
    // action stays suppressed either way — the ended DockSwipe event commits.
    assert_eq!(
        runtime.update(
            &translated_frame(60_000, 3, 15_000, 0),
            &bindings,
            true,
            true
        ),
        idle()
    );
    let outcome = runtime.update(
        &translated_frame(90_000, 3, 25_000, 0),
        &bindings,
        true,
        true,
    );
    assert!(outcome.action.is_none());
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Horizontal,
            progress: 10_000.0 / 117_000.0,
        }
    );
    let outcome = runtime.update(
        &translated_frame(120_000, 3, 30_000, 0),
        &bindings,
        true,
        true,
    );
    assert_eq!(
        outcome.stream,
        SwipeOutput::Advance {
            motion: DockSwipeMotion::Horizontal,
            delta: 5_000.0 / 117_000.0,
        }
    );

    let outcome = runtime.end(true);
    assert_eq!(outcome.action, None);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Finish {
            motion: DockSwipeMotion::Horizontal,
            end: SwipeEnd::AtRelease,
        }
    );
}

#[test]
fn left_swipes_stream_negative_progress() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeLeft;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, -15_000, 0),
        &bindings,
        true,
        true,
    );

    // Fingers moving left are negative travel and must render as negative
    // progress — the hardware-verified system convention (positive progress
    // is rightward content travel).
    let outcome = runtime.update(
        &translated_frame(90_000, 3, -25_000, 0),
        &bindings,
        true,
        true,
    );
    assert_eq!(outcome.action, None);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Horizontal,
            progress: -10_000.0 / 117_000.0,
        }
    );
}

#[test]
fn unsupported_platform_keeps_discrete_swipe_dispatch() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, false);

    let outcome = runtime.update(
        &translated_frame(60_000, 3, 15_000, 0),
        &bindings,
        true,
        false,
    );
    assert_eq!(outcome.action, Some((trigger, Action::NextDesktop)));
    assert_eq!(outcome.stream, SwipeOutput::Idle);
}

#[test]
fn vertical_up_swipes_stream_positive_progress() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeUp;
    let bindings = BTreeMap::from([(trigger, Action::MissionControl)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, 0, -15_000),
        &bindings,
        true,
        true,
    );

    // An upward Mission Control pull streams positive progress: the y axis
    // grows downward, so the vertical delta negates dy. The first traveling
    // frame opens the stream with its delta as the Began progress.
    let outcome = runtime.update(
        &translated_frame(90_000, 3, 0, -25_000),
        &bindings,
        true,
        true,
    );
    assert_eq!(outcome.action, None);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Vertical,
            progress: 10_000.0 / 75_600.0,
        }
    );
    let outcome = runtime.update(
        &translated_frame(120_000, 3, 0, -30_000),
        &bindings,
        true,
        true,
    );
    assert_eq!(
        outcome.stream,
        SwipeOutput::Advance {
            motion: DockSwipeMotion::Vertical,
            delta: 5_000.0 / 75_600.0,
        }
    );
}

#[test]
fn vertical_down_swipes_stream_negative_progress() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeDown;
    let bindings = BTreeMap::from([(trigger, Action::AppExpose)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, 0, 15_000),
        &bindings,
        true,
        true,
    );

    let outcome = runtime.update(
        &translated_frame(90_000, 3, 0, 25_000),
        &bindings,
        true,
        true,
    );
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Vertical,
            progress: -10_000.0 / 75_600.0,
        }
    );
}

#[test]
fn cross_axis_binding_keeps_discrete_dispatch() {
    // A vertical action on a horizontal trigger has no matching animation;
    // the discrete hotkey is the only honest dispatch.
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::MissionControl)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);

    let outcome = runtime.update(
        &translated_frame(60_000, 3, 15_000, 0),
        &bindings,
        true,
        true,
    );
    assert_eq!(outcome.action, Some((trigger, Action::MissionControl)));
    assert_eq!(outcome.stream, SwipeOutput::Idle);
}

#[test]
fn dropped_frame_cancel_keeps_the_stream_running() {
    let trigger = ButtonId::TouchpadFourFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 4, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 4, 15_000, 0),
        &bindings,
        true,
        true,
    );

    // A liftoff or mid-stroke drop surfaces as TouchpadCancel while the
    // stroke continues, so the animation must survive it.
    runtime.cancel();
    let outcome = runtime.update(
        &translated_frame(90_000, 4, 25_000, 0),
        &bindings,
        true,
        true,
    );
    assert!(matches!(outcome.stream, SwipeOutput::Begin { .. }));

    let outcome = runtime.end(true);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Finish {
            motion: DockSwipeMotion::Horizontal,
            end: SwipeEnd::AtRelease,
        }
    );
}

#[test]
fn contact_set_change_does_not_jump_progress() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, 15_000, 0),
        &bindings,
        true,
        true,
    );

    // One finger lifts: the centroid moves, but the stroke was rebased, so
    // this frame must not stream a phantom delta.
    let rebased = frame(
        90_000,
        vec![contact(1, 65_000, 50_000), contact(2, 75_000, 50_000)],
    );
    let outcome = runtime.update(&rebased, &bindings, true, true);
    assert_eq!(outcome.stream, SwipeOutput::Idle);

    // The next frame with the new contact set opens the stream (it is still
    // latent) from the new baseline.
    let moved = frame(
        120_000,
        vec![contact(1, 70_000, 50_000), contact(2, 80_000, 50_000)],
    );
    let outcome = runtime.update(&moved, &bindings, true, true);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Horizontal,
            progress: 5_000.0 / 117_000.0,
        }
    );
}

#[test]
fn session_teardown_cancels_the_running_animation() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeLeft;
    let bindings = BTreeMap::from([(trigger, Action::PreviousDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, -15_000, 0),
        &bindings,
        true,
        true,
    );
    runtime.update(
        &translated_frame(90_000, 3, -25_000, 0),
        &bindings,
        true,
        true,
    );

    assert_eq!(
        runtime.terminate(),
        SwipeOutput::Finish {
            motion: DockSwipeMotion::Horizontal,
            end: SwipeEnd::Cancelled,
        }
    );
}

#[test]
fn unopened_stream_ends_silently() {
    // A stroke that commits but never travels past the opening frame posted
    // no Began, so its end must not post a Finish either.
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, 15_000, 0),
        &bindings,
        true,
        true,
    );

    let outcome = runtime.end(true);
    assert_eq!(outcome.action, None);
    assert_eq!(outcome.stream, SwipeOutput::Idle);
}

#[test]
fn four_finger_swipes_stream_like_three_finger_ones() {
    let trigger = ButtonId::TouchpadFourFingerSwipeLeft;
    let bindings = BTreeMap::from([(trigger, Action::PreviousDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 4, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 4, -15_000, 0),
        &bindings,
        true,
        true,
    );
    let outcome = runtime.update(
        &translated_frame(90_000, 4, -25_000, 0),
        &bindings,
        true,
        true,
    );
    assert_eq!(outcome.action, None);
    assert_eq!(
        outcome.stream,
        SwipeOutput::Begin {
            motion: DockSwipeMotion::Horizontal,
            progress: -10_000.0 / 117_000.0,
        }
    );
}

#[test]
fn begin_failure_falls_back_to_discrete_action() {
    let trigger = ButtonId::TouchpadThreeFingerSwipeRight;
    let bindings = BTreeMap::from([(trigger, Action::NextDesktop)]);
    let mut runtime = TouchpadRuntime::default();
    runtime.update(&translated_frame(0, 3, 0, 0), &bindings, true, true);
    runtime.update(
        &translated_frame(60_000, 3, 15_000, 0),
        &bindings,
        true,
        true,
    );
    runtime.update(
        &translated_frame(90_000, 3, 25_000, 0),
        &bindings,
        true,
        true,
    );

    // The Began post failed: the stream closes and commit's suppressed
    // action is returned for dispatch, per the API fallback contract.
    assert_eq!(runtime.begin_failed(), Some((trigger, Action::NextDesktop)));

    // Closed: later frames and the stroke end post nothing.
    let outcome = runtime.update(
        &translated_frame(120_000, 3, 30_000, 0),
        &bindings,
        true,
        true,
    );
    assert_eq!(outcome.stream, SwipeOutput::Idle);
    let outcome = runtime.end(true);
    assert_eq!(outcome.stream, SwipeOutput::Idle);
}

#[test]
fn begin_failed_without_a_stream_returns_none() {
    let mut runtime = TouchpadRuntime::default();
    assert_eq!(runtime.begin_failed(), None);
}
