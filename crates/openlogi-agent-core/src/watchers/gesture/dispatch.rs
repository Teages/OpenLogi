//! Resolve captured HID++ inputs against the active per-device plan.

mod wheel;

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use openlogi_core::binding::{Action, Binding, ButtonId, GestureDirection, default_binding};
use openlogi_core::config::ThumbwheelSensitivity;
use openlogi_core::touchpad::{
    GestureRecognition, TouchContact, TouchFrame, TouchpadGestureRecognizer,
};
use openlogi_hid::CapturedInput;
use tracing::debug;

use self::wheel::{ScrollScale, WheelAccumulators, WheelOutput, WheelRotation};
use super::GestureOutputs;
use crate::capture_plan::DispatchPlan;
use crate::runtime::{HidppSessionId, PressToken};

/// Effective thumb-wheel configuration whose continuity is tied to one
/// dispatch plan. A binding or sensitivity update clears accumulated state
/// without cycling an unchanged HID++ diversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WheelConfiguration {
    up: Action,
    down: Action,
    sensitivity: ThumbwheelSensitivity,
}

impl WheelConfiguration {
    /// Resolve both directional bindings and their shared sensitivity.
    pub(super) fn for_plan(plan: &DispatchPlan) -> Self {
        let action = |button| {
            plan.bindings
                .get(&button)
                .map_or_else(|| default_binding(button), Binding::click_action)
        };
        Self {
            up: action(ButtonId::ThumbwheelScrollUp),
            down: action(ButtonId::ThumbwheelScrollDown),
            sensitivity: plan.thumbwheel_sensitivity,
        }
    }

    fn action(&self, rotation: WheelRotation) -> &Action {
        match rotation.button() {
            ButtonId::ThumbwheelScrollUp => &self.up,
            ButtonId::ThumbwheelScrollDown => &self.down,
            _ => unreachable!("wheel rotations only map to thumb-wheel directions"),
        }
    }
}

/// Correlates completed HID++ gesture semantics with the exact physical press
/// token admitted by the shared button runtime. The runtime remains the sole
/// authority on whether the token is still active.
#[derive(Default)]
struct GesturePresses {
    tokens: HashMap<(HidppSessionId, ButtonId), PressToken>,
}

impl GesturePresses {
    fn start(&mut self, session: &HidppSessionId, button: ButtonId, press: PressToken) {
        self.tokens.insert((session.clone(), button), press);
    }

    fn get(&self, session: &HidppSessionId, button: ButtonId) -> Option<&PressToken> {
        self.tokens.get(&(session.clone(), button))
    }

    fn end(&mut self, session: &HidppSessionId, button: ButtonId) {
        self.tokens.remove(&(session.clone(), button));
    }

    fn cancel_session(&mut self, session: &HidppSessionId) {
        self.tokens.retain(|(candidate, _), _| candidate != session);
    }
}

/// Wheel state scoped to exact capture-session incarnations. Keying by session
/// rather than device prevents a replacement epoch from inheriting progress or
/// having its state removed by a stale completion from the previous epoch.
#[derive(Default)]
struct SessionWheels(HashMap<HidppSessionId, WheelAccumulators>);

impl SessionWheels {
    fn for_session(&mut self, session: &HidppSessionId) -> &mut WheelAccumulators {
        self.0.entry(session.clone()).or_default()
    }

    fn cancel_session(&mut self, session: &HidppSessionId) {
        self.0.remove(session);
    }
}

use openlogi_inject::{DockSwipeMotion, DockSwipePhase};

#[derive(Default)]
struct TouchpadRuntime {
    recognizer: TouchpadGestureRecognizer,
    frozen_bindings: Option<BTreeMap<ButtonId, Action>>,
    frozen_actions_enabled: bool,
    stream: Option<ActiveSwipe>,
}

impl TouchpadRuntime {
    fn update(
        &mut self,
        frame: &TouchFrame,
        current_bindings: &BTreeMap<ButtonId, Action>,
        actions_enabled: bool,
        native_streaming: bool,
    ) -> TouchpadOutcome {
        if self.frozen_bindings.is_none() {
            self.frozen_bindings = Some(current_bindings.clone());
            self.frozen_actions_enabled = actions_enabled;
        }
        let mut outcome = TouchpadOutcome::default();
        match self.recognizer.update(frame) {
            GestureRecognition::Gesture(trigger)
                if self.frozen_actions_enabled && actions_enabled =>
            {
                if let Some((trigger, action)) = self.action(trigger) {
                    match (native_streaming, native_stream_plan(trigger, &action)) {
                        // DockSwipe End commits the action; Began waits for first non-zero travel.
                        (true, Some(motion)) => {
                            self.stream = Some(ActiveSwipe::new(frame, motion, (trigger, action)));
                        }
                        (_, _) => outcome.action = Some((trigger, action)),
                    }
                }
            }
            GestureRecognition::Pending
            | GestureRecognition::NativeScroll
            | GestureRecognition::Gesture(_) => {}
        }
        if let Some(swipe) = &mut self.stream
            && let Some(step) = swipe.advance(frame)
        {
            outcome.stream = step;
        }
        outcome
    }

    fn end(&mut self, actions_enabled: bool) -> TouchpadOutcome {
        let action = self
            .recognizer
            .end()
            .filter(|_| self.frozen_actions_enabled && actions_enabled)
            .and_then(|trigger| self.action(trigger));
        let stream = self
            .stream
            .take()
            .map_or(SwipeOutput::Idle, |swipe| swipe.finish(SwipeEnd::AtRelease));
        self.frozen_bindings = None;
        self.frozen_actions_enabled = false;
        TouchpadOutcome { action, stream }
    }

    /// Cancel the stroke but not a running animation: dropped-frame cancels
    /// fire mid-stroke, and the real end still arrives as `TouchpadEnd`.
    fn cancel(&mut self) {
        self.recognizer.cancel();
        self.frozen_bindings = None;
        self.frozen_actions_enabled = false;
    }

    /// Spring back an opened animation; unopened streams remain silent.
    fn terminate(&mut self) -> SwipeOutput {
        self.cancel();
        self.stream
            .take()
            .map_or(SwipeOutput::Idle, |swipe| swipe.finish(SwipeEnd::Cancelled))
    }

    /// Close the failed stream and recover its suppressed discrete action.
    fn begin_failed(&mut self) -> Option<(ButtonId, Action)> {
        self.stream.take().map(|swipe| swipe.fallback)
    }

    fn action(&self, trigger: ButtonId) -> Option<(ButtonId, Action)> {
        self.frozen_bindings
            .as_ref()?
            .get(&trigger)
            .cloned()
            .map(|action| (trigger, action))
    }
}

#[derive(Debug, Default, PartialEq)]
struct TouchpadOutcome {
    action: Option<(ButtonId, Action)>,
    stream: SwipeOutput,
}

#[derive(Debug, Default, PartialEq)]
enum SwipeOutput {
    #[default]
    Idle,
    /// `progress` is the opening frame's travel; later frames stream deltas.
    Begin {
        motion: DockSwipeMotion,
        progress: f64,
    },
    Advance {
        motion: DockSwipeMotion,
        delta: f64,
    },
    Finish {
        motion: DockSwipeMotion,
        end: SwipeEnd,
    },
}

/// Release lets the injector's sign rule commit or spring back; an abort always springs back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwipeEnd {
    AtRelease,
    Cancelled,
}

struct ActiveSwipe {
    motion: DockSwipeMotion,
    opened: bool,
    /// The action suppressed at commit, dispatched if Began fails.
    fallback: (ButtonId, Action),
    contact_ids: Box<[u8]>,
    centroid_um: (i64, i64),
}

impl ActiveSwipe {
    fn new(frame: &TouchFrame, motion: DockSwipeMotion, fallback: (ButtonId, Action)) -> Self {
        Self {
            motion,
            opened: false,
            fallback,
            contact_ids: frame.contacts().iter().map(|c| c.id).collect(),
            centroid_um: frame_centroid(frame.contacts()),
        }
    }

    /// An unopened stream posted nothing, so it ends without a Finish.
    fn finish(self, end: SwipeEnd) -> SwipeOutput {
        if self.opened {
            SwipeOutput::Finish {
                motion: self.motion,
                end,
            }
        } else {
            SwipeOutput::Idle
        }
    }

    /// Defer Began until non-zero travel because vertical DockSwipe ignores zero-progress Began.
    #[expect(
        clippy::cast_precision_loss,
        reason = "centroid deltas are bounded by the pad size; f64 precision is ample"
    )]
    fn advance(&mut self, frame: &TouchFrame) -> Option<SwipeOutput> {
        let contact_ids: Vec<u8> = frame.contacts().iter().map(|c| c.id).collect();
        let centroid = frame_centroid(frame.contacts());
        let mut delta = 0.0;
        if contact_ids.as_slice() == &*self.contact_ids {
            let (dx, dy) = (
                centroid.0 - self.centroid_um.0,
                centroid.1 - self.centroid_um.1,
            );
            // Window y grows downward, but vertical DockSwipe progress is positive upward.
            let raw = match self.motion {
                DockSwipeMotion::Horizontal => dx,
                DockSwipeMotion::Vertical => -dy,
            };
            let travel = match self.motion {
                DockSwipeMotion::Horizontal => HORIZONTAL_PAD_TRAVEL_UM,
                DockSwipeMotion::Vertical => VERTICAL_PAD_TRAVEL_UM,
            };
            delta = raw as f64 / travel;
        }
        self.contact_ids = contact_ids.into_boxed_slice();
        self.centroid_um = centroid;
        if delta == 0.0 {
            return None;
        }
        if !self.opened {
            self.opened = true;
            return Some(SwipeOutput::Begin {
                motion: self.motion,
                progress: delta,
            });
        }
        Some(SwipeOutput::Advance {
            motion: self.motion,
            delta,
        })
    }
}

/// One pad-width of travel equals one progress unit; the constants mirror the
/// target Casa Touch pad (2775 × 1786 @ 600 dpi ≈ 117 × 76 mm) until real
/// geometry is plumbed through.
const HORIZONTAL_PAD_TRAVEL_UM: f64 = 117_000.0;
const VERTICAL_PAD_TRAVEL_UM: f64 = 75_600.0;

/// Stream a committed swipe only when the bound action lives on the trigger's
/// axis; anything else keeps the discrete dispatch.
fn native_stream_plan(trigger: ButtonId, action: &Action) -> Option<DockSwipeMotion> {
    let motion = match trigger {
        ButtonId::TouchpadThreeFingerSwipeRight
        | ButtonId::TouchpadFourFingerSwipeRight
        | ButtonId::TouchpadThreeFingerSwipeLeft
        | ButtonId::TouchpadFourFingerSwipeLeft => DockSwipeMotion::Horizontal,
        ButtonId::TouchpadThreeFingerSwipeUp
        | ButtonId::TouchpadFourFingerSwipeUp
        | ButtonId::TouchpadThreeFingerSwipeDown
        | ButtonId::TouchpadFourFingerSwipeDown => DockSwipeMotion::Vertical,
        _ => return None,
    };
    let action_fits = match motion {
        DockSwipeMotion::Horizontal => {
            matches!(action, Action::NextDesktop | Action::PreviousDesktop)
        }
        DockSwipeMotion::Vertical => matches!(
            action,
            Action::MissionControl | Action::AppExpose | Action::ShowDesktop
        ),
    };
    action_fits.then_some(motion)
}

fn frame_centroid(contacts: &[TouchContact]) -> (i64, i64) {
    let count = i64::try_from(contacts.len()).unwrap_or(1);
    let sum = contacts.iter().fold((0_i64, 0_i64), |(sx, sy), contact| {
        (sx + i64::from(contact.x_um), sy + i64::from(contact.y_um))
    });
    (sum.0 / count, sum.1 / count)
}

#[derive(Default)]
struct SessionTouchpads(HashMap<HidppSessionId, TouchpadRuntime>);

impl SessionTouchpads {
    fn for_session(&mut self, session: &HidppSessionId) -> &mut TouchpadRuntime {
        self.0.entry(session.clone()).or_default()
    }

    fn begin_failed(&mut self, session: &HidppSessionId) -> Option<(ButtonId, Action)> {
        self.0
            .get_mut(session)
            .and_then(TouchpadRuntime::begin_failed)
    }

    fn cancel_session(&mut self, session: &HidppSessionId) -> SwipeOutput {
        self.0
            .remove(session)
            .map_or(SwipeOutput::Idle, |mut runtime| runtime.terminate())
    }
}

/// Input routing plus the per-session state retained between
/// captured events. Capture-session lifecycle remains owned by the parent.
pub(super) struct InputDispatcher {
    outputs: GestureOutputs,
    wheels: SessionWheels,
    gesture_presses: GesturePresses,
    touchpads: SessionTouchpads,
}

impl InputDispatcher {
    /// Build a dispatcher for session-owned capture-plan snapshots.
    pub(super) fn new(outputs: GestureOutputs) -> Self {
        Self {
            outputs,
            wheels: SessionWheels::default(),
            gesture_presses: GesturePresses::default(),
            touchpads: SessionTouchpads::default(),
        }
    }

    /// Cancel every input lifecycle retained for one capture session.
    pub(super) fn cancel_session(&mut self, session: &HidppSessionId) {
        let terminal = self.touchpads.cancel_session(session);
        self.outputs.cancel_session(session);
        self.wheels.cancel_session(session);
        self.gesture_presses.cancel_session(session);
        Self::execute_touchpad_stream(session.epoch(), &terminal, session.device_key());
    }

    /// Route one captured input from `session` to its bound action or
    /// re-synthesised scroll output.
    pub(super) fn dispatch(
        &mut self,
        session: &HidppSessionId,
        plan: &DispatchPlan,
        input: CapturedInput,
        touchpad_actions_enabled: bool,
    ) {
        let key = session.device_key();
        match input {
            CapturedInput::Gesture(button, direction) => {
                Self::dispatch_gesture(
                    &self.gesture_presses,
                    &self.outputs,
                    session,
                    plan,
                    button,
                    direction,
                );
            }
            CapturedInput::ButtonDown(button) => {
                // A raw-XY gesture source owns its click/swipe map; its physical
                // lifecycle is still tracked, but it must not also fire the
                // single-action projection on down.
                let is_gesture = plan.gesture_bindings.contains_key(&button)
                    || plan.side_gesture_bindings.contains_key(&button);
                let binding = (!is_gesture).then(|| plan.bindings.get(&button)).flatten();
                if let Some(binding) = binding {
                    debug!(key, ?button, action = %binding.click_action().label(), "HID++ button → binding");
                } else {
                    debug!(key, ?button, "HID++ button with no binding — ignored");
                }
                let press = self
                    .outputs
                    .actions
                    .try_hidpp_button_down(session, button, binding);
                if is_gesture {
                    if let Some(press) = press {
                        self.gesture_presses.start(session, button, press);
                    } else {
                        self.gesture_presses.end(session, button);
                    }
                }
            }
            CapturedInput::ButtonUp(button) => {
                self.outputs.actions.try_hidpp_button_up(session, button);
                self.gesture_presses.end(session, button);
            }
            CapturedInput::ButtonPulse(button) => {
                Self::dispatch_button_pulse(&self.outputs, session, plan, button);
            }
            CapturedInput::Scroll {
                increments,
                resolution,
            } => {
                let Some(rotation) = WheelRotation::from_increments(increments) else {
                    return;
                };
                let button = rotation.button();
                let configuration = WheelConfiguration::for_plan(plan);
                let action = configuration.action(rotation);
                let wheels = self.wheels.for_session(session);
                match wheels.advance(
                    rotation,
                    action,
                    ScrollScale::new(resolution, configuration.sensitivity),
                    Instant::now(),
                ) {
                    WheelOutput::Idle => {}
                    WheelOutput::Scroll(delta) => self.outputs.post_scroll(session, delta),
                    WheelOutput::FireAction => {
                        debug!(key, ?button, action = %action.label(), "thumb wheel → action");
                        self.outputs.actions.dispatch(action, Some(key));
                    }
                }
            }
            CapturedInput::TouchpadFrame(frame) => {
                let native_streaming = openlogi_inject::dock_swipe_supported();
                let outcome = self.touchpads.for_session(session).update(
                    &frame,
                    &plan.touchpad_bindings,
                    touchpad_actions_enabled,
                    native_streaming,
                );
                Self::execute_touchpad_outcome(
                    &self.outputs,
                    &mut self.touchpads,
                    session,
                    &outcome,
                    key,
                );
            }
            CapturedInput::TouchpadEnd => {
                let outcome = self
                    .touchpads
                    .for_session(session)
                    .end(touchpad_actions_enabled);
                Self::execute_touchpad_outcome(
                    &self.outputs,
                    &mut self.touchpads,
                    session,
                    &outcome,
                    key,
                );
            }
            CapturedInput::TouchpadCancel => {
                self.touchpads.for_session(session).cancel();
            }
            CapturedInput::TouchpadDroppedFrames(_) => {}
        }
    }

    fn dispatch_gesture(
        gesture_presses: &GesturePresses,
        outputs: &GestureOutputs,
        session: &HidppSessionId,
        plan: &DispatchPlan,
        button: ButtonId,
        direction: GestureDirection,
    ) {
        let key = session.device_key();
        let Some(press) = gesture_presses.get(session, button) else {
            debug!(key, %button, ?direction, "gesture from a canceled button lifecycle — ignored");
            return;
        };
        let Some(action) = plan
            .gesture_bindings
            .get(&button)
            .or_else(|| plan.side_gesture_bindings.get(&button))
            .and_then(|map| map.get(&direction))
        else {
            debug!(key, %button, ?direction, "gesture with no binding — ignored");
            return;
        };
        debug!(key, %button, ?direction, action = %action.label(), "gesture → action");
        if !outputs.actions.try_dispatch_while_pressed(press, action) {
            debug!(key, %button, ?direction, "gesture press no longer active — ignored");
        }
    }

    fn dispatch_button_pulse(
        outputs: &GestureOutputs,
        session: &HidppSessionId,
        plan: &DispatchPlan,
        button: ButtonId,
    ) {
        let key = session.device_key();
        let binding = plan.bindings.get(&button);
        if let Some(binding) = binding {
            debug!(key, ?button, action = %binding.click_action().label(), "HID++ button pulse → binding");
        } else {
            debug!(key, ?button, "HID++ button pulse with no binding — ignored");
        }
        outputs
            .actions
            .dispatch_hidpp_button_pulse(session, button, binding);
    }

    fn execute_touchpad_outcome(
        outputs: &GestureOutputs,
        touchpads: &mut SessionTouchpads,
        session: &HidppSessionId,
        outcome: &TouchpadOutcome,
        key: &str,
    ) {
        let owner = session.epoch();
        match &outcome.stream {
            SwipeOutput::Begin { motion, progress } => {
                if openlogi_inject::post_dock_swipe(
                    owner,
                    *motion,
                    DockSwipePhase::Began,
                    *progress,
                ) {
                    debug!(key, ?motion, "touchpad swipe → native DockSwipe animation");
                } else {
                    tracing::warn!(key, ?motion, "native dock swipe failed to begin");
                    if let Some((trigger, action)) = touchpads.begin_failed(session) {
                        debug!(key, %trigger, action = %action.label(), "touchpad swipe → discrete fallback");
                        outputs.actions.dispatch(&action, Some(key));
                    }
                }
            }
            stream => Self::execute_touchpad_stream(owner, stream, key),
        }
        let Some((trigger, action)) = outcome.action.as_ref() else {
            return;
        };
        debug!(key, %trigger, action = %action.label(), "touchpad gesture → action");
        outputs.actions.dispatch(action, Some(key));
    }

    /// Use the capture-session epoch as the global DockSwipe owner.
    fn execute_touchpad_stream(owner: u64, stream: &SwipeOutput, key: &str) {
        match *stream {
            SwipeOutput::Idle => {}
            SwipeOutput::Begin { .. } => unreachable!("handled by execute_touchpad_outcome"),
            SwipeOutput::Advance { motion, delta } => {
                openlogi_inject::post_dock_swipe(owner, motion, DockSwipePhase::Changed, delta);
            }
            SwipeOutput::Finish {
                motion,
                end: SwipeEnd::AtRelease,
            } => {
                openlogi_inject::post_dock_swipe(owner, motion, DockSwipePhase::End, 0.0);
            }
            SwipeOutput::Finish {
                motion,
                end: SwipeEnd::Cancelled,
            } => {
                debug!(key, ?motion, "touchpad swipe animation cancelled");
                openlogi_inject::post_dock_swipe(owner, motion, DockSwipePhase::Cancel, 0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests;
