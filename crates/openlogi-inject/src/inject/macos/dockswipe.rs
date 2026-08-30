#![expect(
    unsafe_code,
    reason = "the SkyLight HIDEvent bridge and the private HIDEvent class are only reachable \
              via dlopen/dlsym FFI and ObjC runtime lookup"
)]
//! Live DockSwipe streaming for macOS 27+.
//!
//! macOS 27 stopped reading the pre-27 CGEvent dock-swipe fields and reads the
//! event's attached IOHIDEvent instead. Building a `kIOHIDEventTypeDockSwipe`
//! HIDEvent, attaching it to a type-30 CGEvent via SkyLight's
//! `SLEventSetIOHIDEvent`, and posting to the session tap drives the native
//! finger-following Space-switch animation — no private entitlement, only the
//! Accessibility grant the agent already holds. Constants and semantics
//! follow Mac Mouse Fix's macOS 27 dock-swipe path (`TouchSimulator.m`,
//! noah-nuebling/mac-mouse-fix#1936); horizontal swipes are end-to-end
//! hardware-confirmed, vertical ones were confirmed in the same session.
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSProcessInfo;

use crate::inject::{DockSwipeMotion, DockSwipePhase};

// IOHIDEventTypes.h: kIOHIDEventTypeVelocity = 9, kIOHIDEventTypeDockSwipe
// = 23. A field id is (type << 16) | index; DockSwipe carries Motion=1,
// Progress=2, Flavor=5; Velocity carries X=0, Y=1, Z=2. The gesture phase
// rides in the options bits 24–31.
const HID_TYPE_VELOCITY: u32 = 9;
const HID_TYPE_DOCK_SWIPE: u32 = 23;
const fn field(event_type: u32, index: u32) -> u32 {
    (event_type << 16) | index
}
const FIELD_DOCK_SWIPE_MOTION: u32 = field(HID_TYPE_DOCK_SWIPE, 1);
const FIELD_DOCK_SWIPE_PROGRESS: u32 = field(HID_TYPE_DOCK_SWIPE, 2);
const FIELD_DOCK_SWIPE_FLAVOR: u32 = field(HID_TYPE_DOCK_SWIPE, 5);
const FIELD_VELOCITY_X: u32 = field(HID_TYPE_VELOCITY, 0);
const FIELD_VELOCITY_Y: u32 = field(HID_TYPE_VELOCITY, 1);
const FIELD_VELOCITY_Z: u32 = field(HID_TYPE_VELOCITY, 2);

/// kIOHIDGestureFlavorDockPrimary — the flavor Mac Mouse Fix uses for all
/// three dock-swipe motions.
const FLAVOR_DOCK_PRIMARY: isize = 3;

// kIOHIDEventPhaseBegan/Changed/Ended/Cancelled and
// kIOHIDEventEventOptionPhaseShift.
const PHASE_SHIFT: u32 = 24;
const PHASE_BEGAN: u32 = 1;
const PHASE_CHANGED: u32 = 2;
const PHASE_ENDED: u32 = 4;
const PHASE_CANCELLED: u32 = 8;

/// Type 30 (NSEventTypeMagnify) is the carrier CGEvent type both Mac
/// Mouse Fix and the verified prototype post DockSwipe events on.
const GESTURE_CG_EVENT_TYPE: c_uint = 30;
/// kCGSessionEventTap.
const SESSION_EVENT_TAP: c_uint = 1;

/// Mac Mouse Fix: the release velocity is roughly the last frame's delta
/// × 100 (×50 and ×300 also observed on real events).
const EXIT_VELOCITY_SCALE: f64 = 100.0;
/// Mac Mouse Fix re-posts the end events at 200 ms and 500 ms — the first
/// one can be dropped while the system is under load ("stuck bug").
const END_RESEND_DELAYS: [Duration; 2] = [Duration::from_millis(200), Duration::from_millis(500)];

/// The running gesture's owner (the capture session that opened it), its
/// accumulated progress and last delta, and a generation counter that
/// invalidates pending end-event resends when a new gesture begins. One
/// stream at a time — the DockSwipe system state is global — so a Began
/// from a new owner supersedes the previous animation and the previous
/// owner's later frames are rejected.
static STREAM: LazyLock<Mutex<Stream>> = LazyLock::new(|| Mutex::new(Stream::default()));

#[derive(Debug, Default)]
struct Stream {
    owner: u64,
    progress: f64,
    last_delta: f64,
    generation: u64,
}

pub(in crate::inject) fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // The class lookup must follow the SkyLight load: the verified
        // prototype only resolved `HIDEvent` after the framework was
        // mapped into the process.
        if NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            < 27
        {
            return false;
        }
        sky_light().is_some() && AnyClass::get(c"HIDEvent").is_some()
    })
}

pub(in crate::inject) fn post(
    owner: u64,
    motion: DockSwipeMotion,
    phase: DockSwipePhase,
    delta: f64,
) -> bool {
    if phase == DockSwipePhase::Began {
        return post_began(owner, motion, delta);
    }
    let motion_id = motion_id(motion);
    let Ok(mut stream) = STREAM.lock() else {
        tracing::warn!("dock swipe stream mutex poisoned");
        return false;
    };
    let event_plan = if phase == DockSwipePhase::Changed {
        stream.advance(owner, delta)
    } else {
        stream.finish(owner, phase)
    };
    let Some(event_plan) = event_plan else {
        // Zero-delta continuations are skipped (Mac Mouse Fix skips them
        // too), and frames from a superseded owner are rejected.
        return false;
    };
    drop(stream);
    let posted = post_event(motion_id, &event_plan);
    if posted && let DockSwipePhase::End | DockSwipePhase::Cancel = phase {
        schedule_end_resend(motion_id, event_plan);
    }
    if !posted {
        tracing::warn!(?motion, ?phase, "dock swipe event could not be posted");
    }
    posted
}

/// The IOHIDEventGestureMotion value of a swipe: HorizontalX (1) or
/// VerticalY (2).
fn motion_id(motion: DockSwipeMotion) -> isize {
    match motion {
        DockSwipeMotion::Horizontal => 1,
        DockSwipeMotion::Vertical => 2,
    }
}

/// Open a gesture as one transaction: the Began event is delivered while
/// the stream lock is held, and ownership (generation, progress) is
/// committed only on success. A failed delivery therefore leaves the
/// previous owner's gesture fully alive — its frames and its end —
/// instead of stranding it.
fn post_began(owner: u64, motion: DockSwipeMotion, delta: f64) -> bool {
    let Ok(mut stream) = STREAM.lock() else {
        tracing::warn!("dock swipe stream mutex poisoned");
        return false;
    };
    post_began_with(&mut stream, owner, delta, |plan| {
        post_event(motion_id(motion), &plan)
    })
}

/// The Began transaction: deliver the event, and commit the new owner's
/// state only on a successful delivery — a failed one leaves the previous
/// owner's gesture fully alive. Delivery is injected so tests can drive
/// both outcomes without posting real events.
fn post_began_with(
    stream: &mut Stream,
    owner: u64,
    delta: f64,
    deliver: impl FnOnce(EventPlan) -> bool,
) -> bool {
    let plan = stream.began_plan(delta);
    if deliver(plan) {
        stream.commit_began(owner, &plan);
        true
    } else {
        false
    }
}

/// One ready-to-post event: options bits, accumulated progress, and the
/// exit velocity attached on end phases.
#[derive(Clone, Copy)]
struct EventPlan {
    options: u32,
    progress: f64,
    velocity: Option<f64>,
    generation: u64,
}

impl Stream {
    /// Plan (without posting or committing) the Began event for `owner`.
    fn began_plan(&self, delta: f64) -> EventPlan {
        // Mac Mouse Fix's Began carries the stroke's first delta, which seeds
        // the accumulator.
        EventPlan {
            options: PHASE_BEGAN << PHASE_SHIFT,
            progress: delta,
            velocity: None,
            generation: self.generation.wrapping_add(1),
        }
    }

    /// Commit a successfully delivered Began: `owner` takes the stream
    /// over and the delivered event's progress seeds the accumulator.
    /// Taking the plan makes an owner/progress mismatch with the
    /// delivered event unrepresentable.
    fn commit_began(&mut self, owner: u64, plan: &EventPlan) {
        self.owner = owner;
        self.generation = plan.generation;
        self.progress = plan.progress;
        self.last_delta = plan.progress;
    }

    /// Plan a continuation frame; `None` for zero-delta frames (Mac Mouse Fix
    /// skips them) and for frames from a superseded owner, whose delivery
    /// must not touch the new owner's animation.
    fn advance(&mut self, owner: u64, delta: f64) -> Option<EventPlan> {
        if self.owner != owner {
            return None;
        }
        if delta == 0.0 {
            return None;
        }
        self.progress += delta;
        self.last_delta = delta;
        Some(EventPlan {
            options: PHASE_CHANGED << PHASE_SHIFT,
            progress: self.progress,
            velocity: None,
            generation: self.generation,
        })
    }

    /// Plan the end-of-stream event. The release sign rule decides
    /// commit-vs-spring-back: a release still moving along the accumulated
    /// travel commits (`ENDED`), one moving against it springs back
    /// (`CANCELLED`).
    fn finish(&mut self, owner: u64, phase: DockSwipePhase) -> Option<EventPlan> {
        if self.owner != owner {
            return None;
        }
        let velocity = self.last_delta * EXIT_VELOCITY_SCALE;
        Some(EventPlan {
            options: release_phase(phase, self.progress, self.last_delta) << PHASE_SHIFT,
            progress: self.progress,
            velocity: Some(velocity),
            generation: self.generation,
        })
    }
}

/// Commit-vs-spring-back decision for a release, Mac Mouse Fix's rule: a
/// release still moving along the accumulated travel commits (`ENDED`),
/// one moving against it springs back (`CANCELLED`), and a release with
/// no travel or no progress cannot commit anything.
fn release_phase(phase: DockSwipePhase, progress: f64, last_delta: f64) -> u32 {
    let commits = match phase {
        DockSwipePhase::Cancel => false,
        _ => progress != 0.0 && last_delta.signum() == progress.signum(),
    };
    if commits {
        PHASE_ENDED
    } else {
        PHASE_CANCELLED
    }
}

fn post_event(motion_id: isize, plan: &EventPlan) -> bool {
    let Some(bridge) = sky_light() else {
        tracing::warn!("SkyLight SLEventSetIOHIDEvent unavailable");
        return false;
    };
    autoreleasepool(|_| {
        // SAFETY: CGEventCreate returns a +1 CFTypeRef, released below.
        // CGEventCreate(NULL) with no event source is what Mac Mouse Fix
        // and the verified prototype both post with. Created first so the
        // HIDEvent can carry the carrier's own timestamp, exactly like
        // the working mmf27fix shim (CGEventGetTimestamp pass-through).
        let cg_event = unsafe { CGEventCreate(std::ptr::null()) };
        if cg_event.is_null() {
            return false;
        }
        // SAFETY: cg_event is a live +1 CGEventRef.
        let carrier_timestamp = unsafe { CGEventGetTimestamp(cg_event) };
        let Some(hid_event) = build_hid_event(motion_id, plan, carrier_timestamp) else {
            // SAFETY: balance the +1 from CGEventCreate above.
            unsafe { CFRelease(cg_event) };
            tracing::warn!("private HIDEvent class unavailable");
            return false;
        };
        // SAFETY: cg_event is a live +1 CGEventRef; the bridge attaches
        // the HIDEvent for the event's lifetime; the tap id is the
        // documented kCGSessionEventTap.
        unsafe {
            CGEventSetType(cg_event, GESTURE_CG_EVENT_TYPE);
            (bridge.set_hid_event)(cg_event, Retained::as_ptr(&hid_event).cast());
            CGEventPost(SESSION_EVENT_TAP, cg_event);
            CFRelease(cg_event);
        }
        tracing::debug!(
            motion = motion_id,
            options = plan.options,
            progress = plan.progress,
            velocity = plan.velocity,
            "dock swipe posted"
        );
        true
    })
}

fn build_hid_event(
    motion_id: isize,
    plan: &EventPlan,
    carrier_timestamp: u64,
) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"HIDEvent")?;
    // SAFETY: HIDEvent responds to `alloc` (+1 result). The class is
    // opaque, so every exchange below runs on raw pointers.
    let alloc: *mut AnyObject = unsafe { msg_send![class, alloc] };
    if alloc.is_null() {
        return None;
    }
    // SAFETY: init-family selector declared on the prototype's HIDEvent
    // interface as `(uint32_t)type (uint64_t)timestamp (uint64_t)senderID`;
    // it consumes the alloc's +1 and returns +1 (or nil).
    let event: *mut AnyObject = unsafe {
        msg_send![alloc, initWithType: HID_TYPE_DOCK_SWIPE, timestamp: carrier_timestamp, senderID: 0_u64]
    };
    // SAFETY: the +1 from the init above transfers into the Retained; the
    // parent must be owned before any fallible child step, or a failure
    // below would leak it.
    let event: Retained<AnyObject> = unsafe { Retained::from_raw(event) }?;
    // SAFETY: `setIntegerValue:forField:` is `(NSInteger, uint32_t)` on the
    // prototype's HIDEvent interface; every selector below is verified
    // end-to-end by the prototype's payload round-trip.
    unsafe {
        let () = msg_send![&event, setIntegerValue: motion_id, forField: FIELD_DOCK_SWIPE_MOTION];
        let () = msg_send![&event, setIntegerValue: FLAVOR_DOCK_PRIMARY, forField: FIELD_DOCK_SWIPE_FLAVOR];
        let () =
            msg_send![&event, setDoubleValue: plan.progress, forField: FIELD_DOCK_SWIPE_PROGRESS];
        let () = msg_send![&event, setOptions: plan.options];
    }
    if let Some(velocity) = plan.velocity {
        let child = velocity_event(velocity, carrier_timestamp)?;
        // SAFETY: `appendEvent:` takes an object pointer and retains it;
        // the raw pointer is live for the call's duration.
        unsafe {
            let () = msg_send![&event, appendEvent: Retained::as_ptr(&child)];
        }
    }
    Some(event)
}

fn velocity_event(velocity: f64, carrier_timestamp: u64) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"HIDEvent")?;
    // SAFETY: same opaque alloc/init pair as `build_hid_event`, with the
    // Velocity event type.
    let alloc: *mut AnyObject = unsafe { msg_send![class, alloc] };
    if alloc.is_null() {
        return None;
    }
    // SAFETY: init-family selector consumes the alloc's +1 and returns
    // +1 (or nil).
    let event: *mut AnyObject = unsafe {
        msg_send![alloc, initWithType: HID_TYPE_VELOCITY, timestamp: carrier_timestamp, senderID: 0_u64]
    };
    if event.is_null() {
        return None;
    }
    // SAFETY: `setDoubleValue:forField:` is `(double, uint32_t)` on the
    // prototype's HIDEvent interface. Real events carry the speed on X
    // and Y with Z at zero.
    unsafe {
        let () = msg_send![event, setDoubleValue: velocity, forField: FIELD_VELOCITY_X];
        let () = msg_send![event, setDoubleValue: velocity, forField: FIELD_VELOCITY_Y];
        let () = msg_send![event, setDoubleValue: 0.0, forField: FIELD_VELOCITY_Z];
    }
    // SAFETY: the +1 from the init above transfers into the Retained;
    // the nil case already returned above.
    unsafe { Retained::from_raw(event) }
}

/// Re-post the end event after short delays: Mac Mouse Fix's workaround
/// for the WindowServer occasionally dropping the first end event under
/// load ("stuck bug"). A gesture begun in the meantime (generation bump)
/// cancels the pending resends.
fn schedule_end_resend(motion_id: isize, plan: EventPlan) {
    let result = std::thread::Builder::new()
        .name("dockswipe-resend".into())
        .spawn(move || {
            let mut previous = Duration::ZERO;
            for delay in END_RESEND_DELAYS {
                std::thread::sleep(delay.saturating_sub(previous));
                previous = delay;
                let Ok(stream) = STREAM.lock() else {
                    return;
                };
                if stream.generation != plan.generation {
                    return;
                }
                drop(stream);
                // Resends replay the exact event already decided — no
                // state mutation.
                if !post_event(motion_id, &plan) {
                    return;
                }
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "dock swipe end resend could not be scheduled");
    }
}

struct SkyLightBridge {
    set_hid_event: unsafe extern "C" fn(*const c_void, *const c_void),
}

fn sky_light() -> Option<&'static SkyLightBridge> {
    static BRIDGE: OnceLock<Option<SkyLightBridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| {
            const RTLD_LAZY: c_int = 0x1;
            const SKY_LIGHT: &CStr =
                c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
            // SAFETY: `dlopen`/`dlsym` come from libSystem; SKY_LIGHT and
            // the symbol name are valid C strings. The handle is cached
            // and intentionally never closed.
            let set_hid_event = unsafe {
                let handle = dlopen(SKY_LIGHT.as_ptr(), RTLD_LAZY);
                if handle.is_null() {
                    return None;
                }
                dlsym(handle, c"SLEventSetIOHIDEvent".as_ptr())
            };
            if set_hid_event.is_null() {
                return None;
            }
            Some(SkyLightBridge {
                set_hid_event: unsafe {
                    // SAFETY: the symbol, when present, has the documented
                    // SLEventSetIOHIDEvent(CGEventRef, CFTypeRef) signature.
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const c_void, *const c_void),
                    >(set_hid_event)
                },
            })
        })
        .as_ref()
}

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *const c_void;
    fn CGEventGetTimestamp(event: *const c_void) -> u64;
    fn CGEventSetType(event: *const c_void, event_type: c_uint);
    fn CGEventPost(tap: c_uint, event: *const c_void);
    fn CFRelease(cf: *const c_void);
}

#[cfg(test)]
mod tests {
    use super::{
        FIELD_DOCK_SWIPE_FLAVOR, FIELD_DOCK_SWIPE_MOTION, FIELD_DOCK_SWIPE_PROGRESS,
        FIELD_VELOCITY_X, PHASE_BEGAN, PHASE_CANCELLED, PHASE_ENDED, PHASE_SHIFT, Stream,
        post_began_with, release_phase,
    };
    use crate::inject::DockSwipePhase;

    #[test]
    fn failed_began_delivery_leaves_the_previous_owner_intact() {
        let mut stream = Stream::default();
        assert!(post_began_with(&mut stream, 1, 0.1, |_| true));
        assert!(stream.advance(1, 0.1).is_some());

        // Owner 2's Began delivery FAILS: nothing commits, so owner 1's
        // gesture stays alive — its frames and its end still work — while
        // owner 2's frames stay rejected.
        assert!(!post_began_with(&mut stream, 2, 0.2, |_| false));
        assert!(stream.advance(1, -0.05).is_some());
        assert!(stream.finish(1, DockSwipePhase::End).is_some());
        assert!(stream.advance(2, 0.2).is_none());
        assert!(stream.finish(2, DockSwipePhase::Cancel).is_none());
    }

    #[test]
    fn delivered_began_commits_the_new_owner() {
        let mut stream = Stream::default();
        assert!(post_began_with(&mut stream, 2, 0.2, |_| true));

        assert_eq!(stream.owner, 2);
        assert!((stream.progress - 0.2).abs() < 1e-9);
        assert!((stream.last_delta - 0.2).abs() < 1e-9);
        // The previous owner (1) is rejected after the handover.
        assert!(stream.advance(1, 0.1).is_none());
        assert!(stream.advance(2, 0.1).is_some());
    }

    #[test]
    fn dock_swipe_field_ids_match_iohid_event_types() {
        assert_eq!(FIELD_DOCK_SWIPE_MOTION, 23 << 16 | 1);
        assert_eq!(FIELD_DOCK_SWIPE_PROGRESS, 23 << 16 | 2);
        assert_eq!(FIELD_DOCK_SWIPE_FLAVOR, 23 << 16 | 5);
        assert_eq!(FIELD_VELOCITY_X, 9 << 16);
    }

    #[test]
    fn release_along_travel_commits_and_against_springs_back() {
        let released = u64::from(PHASE_ENDED) << PHASE_SHIFT;
        let cancelled = u64::from(PHASE_CANCELLED) << PHASE_SHIFT;
        assert_eq!(
            u64::from(release_phase(DockSwipePhase::End, 0.5, 0.1)) << PHASE_SHIFT,
            released
        );
        assert_eq!(
            u64::from(release_phase(DockSwipePhase::End, -0.5, -0.1)) << PHASE_SHIFT,
            released
        );
        assert_eq!(
            u64::from(release_phase(DockSwipePhase::End, 0.5, -0.1)) << PHASE_SHIFT,
            cancelled,
            "a release moving against the travel springs back"
        );
        assert_eq!(
            u64::from(release_phase(DockSwipePhase::End, 0.0, 0.1)) << PHASE_SHIFT,
            cancelled,
            "no accumulated progress cannot commit"
        );
        assert_eq!(
            u64::from(release_phase(DockSwipePhase::Cancel, 0.5, 0.1)) << PHASE_SHIFT,
            cancelled,
            "an explicit cancel always springs back"
        );
        assert_eq!(u64::from(PHASE_BEGAN) << PHASE_SHIFT, 1 << 24);
    }
}
