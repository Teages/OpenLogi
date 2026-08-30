//! Resolve captured HID++ inputs against the active per-device plan.

mod wheel;

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use openlogi_core::binding::{Action, Binding, ButtonId, GestureDirection, default_binding};
use openlogi_core::config::ThumbwheelSensitivity;
use openlogi_core::touchpad::{GestureRecognition, TouchFrame, TouchpadGestureRecognizer};
use openlogi_hid::CapturedInput;
use openlogi_inject::SmoothScrollPhase;
use tracing::debug;

use self::wheel::{ScrollScale, WheelAccumulators, WheelOutput, WheelRotation};
use super::{GestureOutputs, TouchpadScrollTuning};
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

/// One routed outcome of feeding a frame (or a stroke boundary) to a
/// session's touchpad runtime.
#[derive(Debug, PartialEq)]
enum TouchpadOutput {
    /// Nothing to dispatch for this frame.
    Idle,
    /// A committed gesture trigger with its resolved action.
    Action { trigger: ButtonId, action: Action },
    /// One synthesized two-finger scroll frame: the centroid's travel in
    /// micrometres plus its position in the scroll gesture's phase stream.
    Scroll {
        dx_um: i64,
        dy_um: i64,
        phase: SmoothScrollPhase,
    },
}

#[derive(Default)]
struct TouchpadRuntime {
    recognizer: TouchpadGestureRecognizer,
    frozen_bindings: Option<BTreeMap<ButtonId, Action>>,
    frozen_actions_enabled: bool,
    /// Whether this stroke already opened its scroll stream, so the next
    /// delta knows to continue rather than begin one.
    scroll_streaming: bool,
}

impl TouchpadRuntime {
    fn update(
        &mut self,
        frame: &TouchFrame,
        current_bindings: &BTreeMap<ButtonId, Action>,
        actions_enabled: bool,
    ) -> TouchpadOutput {
        if self.frozen_bindings.is_none() {
            self.frozen_bindings = Some(current_bindings.clone());
            self.frozen_actions_enabled = actions_enabled;
        }
        match self.recognizer.update(frame) {
            GestureRecognition::Gesture(trigger)
                if self.frozen_actions_enabled && actions_enabled =>
            {
                self.action(trigger)
                    .map_or(TouchpadOutput::Idle, |(trigger, action)| {
                        TouchpadOutput::Action { trigger, action }
                    })
            }
            GestureRecognition::Scroll { dx_um, dy_um } => {
                // Scrolling replaces the firmware translation the capture
                // switched off, so it flows regardless of action bindings.
                let phase = if self.scroll_streaming {
                    SmoothScrollPhase::Changed
                } else {
                    SmoothScrollPhase::Began
                };
                self.scroll_streaming = true;
                TouchpadOutput::Scroll {
                    dx_um,
                    dy_um,
                    phase,
                }
            }
            GestureRecognition::Pending | GestureRecognition::Gesture(_) => TouchpadOutput::Idle,
        }
    }

    fn end(&mut self, actions_enabled: bool) -> TouchpadOutput {
        let action = self
            .recognizer
            .end()
            .filter(|_| self.frozen_actions_enabled && actions_enabled)
            .and_then(|trigger| self.action(trigger));
        let terminal = self.close_scroll_stream(SmoothScrollPhase::Ended);
        self.frozen_bindings = None;
        self.frozen_actions_enabled = false;
        terminal.unwrap_or_else(|| {
            action.map_or(TouchpadOutput::Idle, |(trigger, action)| {
                TouchpadOutput::Action { trigger, action }
            })
        })
    }

    fn cancel(&mut self) -> TouchpadOutput {
        let terminal = self.close_scroll_stream(SmoothScrollPhase::Cancelled);
        self.recognizer.cancel();
        self.frozen_bindings = None;
        self.frozen_actions_enabled = false;
        terminal.unwrap_or(TouchpadOutput::Idle)
    }

    /// Terminate an open scroll stream, if any. Scrolling travelled past the
    /// tap limits, so a scrolled stroke can never also resolve a tap and the
    /// two outcomes never compete.
    fn close_scroll_stream(&mut self, phase: SmoothScrollPhase) -> Option<TouchpadOutput> {
        self.scroll_streaming.then(|| {
            self.scroll_streaming = false;
            TouchpadOutput::Scroll {
                dx_um: 0,
                dy_um: 0,
                phase,
            }
        })
    }

    fn action(&self, trigger: ButtonId) -> Option<(ButtonId, Action)> {
        self.frozen_bindings
            .as_ref()?
            .get(&trigger)
            .cloned()
            .map(|action| (trigger, action))
    }
}

#[derive(Default)]
struct SessionTouchpads(HashMap<HidppSessionId, TouchpadRuntime>);

impl SessionTouchpads {
    fn for_session(&mut self, session: &HidppSessionId) -> &mut TouchpadRuntime {
        self.0.entry(session.clone()).or_default()
    }

    fn cancel_session(&mut self, session: &HidppSessionId) -> Option<TouchpadOutput> {
        self.0.remove(session).map(|mut runtime| runtime.cancel())
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
        self.outputs.cancel_session(session);
        self.wheels.cancel_session(session);
        self.gesture_presses.cancel_session(session);
        if let Some(outcome) = self.touchpads.cancel_session(session) {
            Self::route_touchpad_output(
                &self.outputs,
                TouchpadScrollTuning::NEUTRAL,
                session.device_key(),
                outcome,
            );
        }
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
                let tuning = TouchpadScrollTuning::from_plan(plan);
                let outcome = self.touchpads.for_session(session).update(
                    &frame,
                    &plan.touchpad_bindings,
                    touchpad_actions_enabled,
                );
                Self::route_touchpad_output(&self.outputs, tuning, key, outcome);
            }
            CapturedInput::TouchpadEnd => {
                let tuning = TouchpadScrollTuning::from_plan(plan);
                let outcome = self
                    .touchpads
                    .for_session(session)
                    .end(touchpad_actions_enabled);
                Self::route_touchpad_output(&self.outputs, tuning, key, outcome);
            }
            CapturedInput::TouchpadCancel => {
                let tuning = TouchpadScrollTuning::from_plan(plan);
                let outcome = self.touchpads.for_session(session).cancel();
                Self::route_touchpad_output(&self.outputs, tuning, key, outcome);
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

    fn route_touchpad_output(
        outputs: &GestureOutputs,
        tuning: TouchpadScrollTuning,
        key: &str,
        outcome: TouchpadOutput,
    ) {
        match outcome {
            TouchpadOutput::Idle => {}
            TouchpadOutput::Action { trigger, action } => {
                debug!(key, %trigger, action = %action.label(), "touchpad gesture → action");
                outputs.actions.dispatch(&action, Some(key));
            }
            TouchpadOutput::Scroll {
                dx_um,
                dy_um,
                phase,
            } => super::post_touchpad_scroll(tuning, dx_um, dy_um, phase),
        }
    }
}

#[cfg(test)]
mod tests;
