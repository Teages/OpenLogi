use std::sync::Arc;
use std::time::Duration;

use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::CreatableFeature,
    feature::EmittingFeature,
    feature::FeatureType,
    feature::battery_status::BatteryStatusFeature,
    feature::device_information::{
        DeviceEntityFirmwareInfo, DeviceEntityType, DeviceInformationFeature,
    },
    feature::feature_set::FeatureSetFeature,
    feature::touchpad_raw_xy::{
        DualXyData, RawReportFlags, TouchpadInfo, TouchpadRawEvent, TouchpadRawXyFeature,
    },
    feature::unified_battery::UnifiedBatteryFeature,
    protocol::v20::Hidpp20Error,
};
use tokio::sync::oneshot;

use crate::backend::HidBackend;
use crate::channel::route::DeviceRoute;
use crate::reprog_controls::{self, CidFlags, CidInfo, ReprogControlsV4};
use crate::touchpad::{group_open, same_contact};
use crate::write::{HidppOperation, WriteError, classify_hidpp_error, open_feature, with_route};

/// Snapshot of one HID++ feature exposed by a device: protocol ID +
/// version. Returned by [`dump_features`] for diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct FeatureEntry {
    /// HID++ feature ID.
    pub id: u16,
    /// Feature version reported by the device.
    pub version: u8,
    /// Obsolete / hidden / engineering flags the device advertises alongside
    /// the feature.
    pub typ: FeatureType,
}

/// Snapshot of one HID++ `0x1b04` reprogrammable control. Returned by
/// [`dump_reprog_controls`] for diagnostics so new device controls can be
/// identified before OpenLogi maps them to a first-class button.
#[derive(Debug, Clone, Copy)]
pub struct ReprogControlEntry {
    /// HID++ control ID.
    pub cid: u16,
    /// Default task ID assigned to the control.
    pub task_id: u16,
    /// Capability and classification flags for the control.
    pub flags: CidFlags,
}

impl From<CidInfo> for ReprogControlEntry {
    fn from(info: CidInfo) -> Self {
        Self {
            cid: info.cid.into(),
            task_id: info.task_id.0,
            flags: info.flags,
        }
    }
}

/// Enumerate every HID++ feature the device on `route` reports — used by
/// `openlogi diag features` to confirm which DPI / SmartShift / etc.
/// feature IDs a given peripheral actually exposes (e.g. whether a mouse
/// speaks `0x2201 AdjustableDpi`, `0x2202 ExtendedAdjustableDpi`, or both —
/// `write::dpi` drives either).
pub async fn dump_features(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<Vec<FeatureEntry>, WriteError> {
    let index = route.device_index();
    with_route(backend, route, move |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), index)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { index })?;
        // The root feature exposes the FeatureSet (0x0001) at a fixed
        // address; we look it up directly rather than going through
        // `enumerate_features` so the iteration is observable.
        let feature_set_info = device
            .root()
            .get_feature(FeatureSetFeature::ID)
            .await
            .map_err(|e| {
                classify_hidpp_error(e, HidppOperation::DumpFeatures, FeatureSetFeature::ID)
            })?
            .ok_or(WriteError::FeatureUnsupported {
                feature_hex: FeatureSetFeature::ID,
            })?;
        let feature_set = device.add_feature::<FeatureSetFeature>(feature_set_info.index);
        let count = feature_set.count().await.map_err(|e| {
            classify_hidpp_error(e, HidppOperation::DumpFeatures, FeatureSetFeature::ID)
        })?;
        let mut entries = Vec::with_capacity(usize::from(count));
        for i in 0..=count {
            let info = feature_set.get_feature(i).await.map_err(|e| {
                classify_hidpp_error(e, HidppOperation::DumpFeatures, FeatureSetFeature::ID)
            })?;
            entries.push(FeatureEntry {
                id: info.id,
                version: info.version,
                typ: info.typ,
            });
        }
        Ok(entries)
    })
    .await
}

/// Enumerate the device's HID++ `0x1b04` reprogrammable controls. This is a
/// diagnostics-only probe used to discover controls for newly released devices.
/// For example, MX Master 4 has both a Gesture Button and a separate Haptic
/// Sense Panel in the thumb area; this probe lets us identify the panel's CID
/// and capabilities before wiring it into the capture/remapping model.
pub async fn dump_reprog_controls(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<Vec<ReprogControlEntry>, WriteError> {
    let index = route.device_index();
    with_route(backend, route, move |channel| async move {
        let device = Device::new(Arc::clone(&channel), index)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { index })?;
        let info = device
            .root()
            .get_feature(reprog_controls::FEATURE_ID)
            .await
            .map_err(|e| {
                classify_hidpp_error(e, HidppOperation::DumpFeatures, reprog_controls::FEATURE_ID)
            })?
            .ok_or(WriteError::FeatureUnsupported {
                feature_hex: reprog_controls::FEATURE_ID,
            })?;
        let rc = ReprogControlsV4::new(Arc::clone(&channel), index, info.index);
        let count = rc.get_count().await.map_err(|e| {
            classify_hidpp_error(e, HidppOperation::DumpFeatures, reprog_controls::FEATURE_ID)
        })?;
        let mut entries = Vec::with_capacity(usize::from(count));
        for i in 0..count {
            let control = rc.get_cid_info(i).await.map_err(|e| {
                classify_hidpp_error(e, HidppOperation::DumpFeatures, reprog_controls::FEATURE_ID)
            })?;
            entries.push(control.into());
        }
        Ok(entries)
    })
    .await
}

/// Diagnostic read of the device's raw battery report — the unified `0x1004`
/// fields, or the legacy `0x1000` `discharge_level`/`next_level`/`status`. For
/// `openlogi diag battery`: surfaces exactly what the firmware reports so a
/// claim like "MX2S shows 0% while charging" can be confirmed against the wire
/// instead of guessed (the GUI only ever shows the mapped value).
pub async fn read_battery_raw(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<String, WriteError> {
    let index = route.device_index();
    with_route(backend, route, move |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), index)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { index })?;

        match open_feature::<UnifiedBatteryFeature>(&mut device).await {
            Ok(feature) => {
                let info = feature
                    .get_battery_info()
                    .await
                    .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
                return Ok(format!(
                    "0x1004 UnifiedBattery: percentage={} level={:?} status={:?}",
                    info.charging_percentage, info.level, info.status
                ));
            }
            Err(WriteError::FeatureUnsupported { .. }) => {}
            Err(e) => return Err(e),
        }

        match open_feature::<BatteryStatusFeature>(&mut device).await {
            Ok(feature) => {
                let info = feature
                    .get_battery_level_status()
                    .await
                    .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
                return Ok(format!(
                    "0x1000 BatteryStatus: discharge_level={} next_level={} status={:?}",
                    info.discharge_level, info.next_level, info.status
                ));
            }
            Err(WriteError::FeatureUnsupported { .. }) => {}
            Err(e) => return Err(e),
        }

        // Reached only when neither 0x1004 nor 0x1000 is present; report the
        // preferred feature rather than implying 0x1000 was specifically absent.
        Err(WriteError::FeatureUnsupported {
            feature_hex: 0x1004,
        })
    })
    .await
}

/// Everything a touchpad told the `0x6100 TouchpadRawXy` probe: pad
/// characteristics, the raw-report mode around the probe's own write, and the
/// raw touch frames captured while the mode was on.
///
/// The hidpp feature types are re-exported verbatim (`TouchpadInfo`,
/// `RawReportFlags`, [`DualXyData`]) rather than copied: a probe exists to
/// surface wire values, and a second struct family would invite a field being
/// dropped from one copy but not the other.
#[derive(Debug, Clone)]
pub struct TouchpadProbeReport {
    /// Version of `0x6100` the device advertises.
    pub feature_version: u8,
    /// Pad characteristics from GetTouchpadInfo.
    pub info: TouchpadInfo,
    /// Raw-report mode before the probe touched anything.
    pub state_before: RawReportFlags,
    /// What the device reported immediately after the probe's
    /// `set_raw_report_state(RAW)` — `None` when the probe never wrote (the
    /// device was already in raw mode, or no sampling window was requested).
    ///
    /// The device is free to mask unsupported bits, so the difference between
    /// the requested and confirmed modes is itself a probe finding.
    pub state_after_set: Option<RawReportFlags>,
    /// Touch states captured while raw mode was on, oldest first, as complete
    /// logical frames: each entry is one raw event, and a frame with more than
    /// two fingers contributes several events sharing a timestamp whose last
    /// one carries `end_of_frame`.
    ///
    /// The sensor resends an unchanged hand at its report rate (~130 Hz
    /// measured on Casa Touch), so a logical frame identical to the previous
    /// one is suppressed — `frames` holds what changed,
    /// [`Self::frames_received`] counts what actually arrived.
    pub frames: Vec<DualXyData>,
    /// Total `DualXy` events received during the window, repetitions included.
    pub frames_received: usize,
    /// What the device reported after the probe restored the pre-probe mode —
    /// `None` when there was nothing to restore.
    pub state_restored: Option<RawReportFlags>,
}

/// Upper bound on *recorded* frames a single probe window collects, so a
/// gesture rehearsal cannot balloon the report without bound.
const MAX_PROBE_FRAMES: usize = 20_000;

/// Commit one assembled logical frame into `frames`: appended whole, unless
/// every part repeats the frame before it — the tail of `frames`, where whole
/// groups are always contiguous. A resting hand therefore records once and is
/// then suppressed at report rate until its contact state actually changes.
fn commit_logical(frames: &mut Vec<DualXyData>, parts: &mut Vec<DualXyData>) {
    if parts.is_empty() {
        return;
    }
    let n = parts.len();
    let repeats = frames.len() >= n
        && frames[frames.len() - n..]
            .iter()
            .zip(parts.iter())
            .all(|(a, b)| same_contact(a, b));
    if repeats {
        parts.clear();
    } else {
        frames.append(parts);
    }
}

/// Probe the HID++ `0x6100 TouchpadRawXy` feature of the device on `route`:
/// read the pad characteristics and raw-report mode, and when `sample` is
/// `Some`, enable raw reporting for that window and collect the touch frames
/// the device emits.
///
/// The pre-probe raw-report mode is always restored afterwards; a device that
/// was already reporting raw (`Options+` mid-gesture, a stale session) is only
/// listened to, never re-written. Read results surface even when sampling is
/// declined by the caller: the pad characteristics alone answer "does this
/// touchpad speak 0x6100 and how many fingers does it track".
///
/// Once raw mode has been enabled, every exit path runs the restore first —
/// errors before it are remembered, not returned early, so an interrupted
/// probe can never leave the device reporting raw to nobody. The hardware
/// caveat this cannot address: on a Bolt receiver the transport node is
/// shared, and another open holder of it (Options+, a running agent) splits
/// off input reports — a production gesture pipeline needs exclusive claim
/// over the node, which is exactly why production uses a capture session.
pub async fn probe_touchpad(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
    sample: Option<Duration>,
    shutdown: Option<oneshot::Receiver<()>>,
) -> Result<TouchpadProbeReport, WriteError> {
    let index = route.device_index();
    with_route(backend, route, move |channel| async move {
        probe_touchpad_on_channel(&channel, index, sample, shutdown).await
    })
    .await
}

/// [`probe_touchpad`] against an already-open channel, the shape the tests
/// drive a scripted device through.
pub(crate) async fn probe_touchpad_on_channel(
    channel: &Arc<HidppChannel>,
    index: u8,
    sample: Option<Duration>,
    shutdown: Option<oneshot::Receiver<()>>,
) -> Result<TouchpadProbeReport, WriteError> {
    let mut device = Device::new(Arc::clone(channel), index)
        .await
        .map_err(|_| WriteError::DeviceUnreachable { index })?;
    let feature_info = device
        .root()
        .get_feature(TouchpadRawXyFeature::ID)
        .await
        .map_err(|e| {
            classify_hidpp_error(e, HidppOperation::ResolveFeature, TouchpadRawXyFeature::ID)
        })?
        .ok_or(WriteError::FeatureUnsupported {
            feature_hex: TouchpadRawXyFeature::ID,
        })?;
    let feature = device.add_feature::<TouchpadRawXyFeature>(feature_info.index);

    let info = feature
        .get_touchpad_info()
        .await
        .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
    let state_before = feature
        .get_raw_report_state()
        .await
        .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;

    let mut report = TouchpadProbeReport {
        feature_version: feature_info.version,
        info,
        state_before,
        state_after_set: None,
        frames: Vec::new(),
        frames_received: 0,
        state_restored: None,
    };
    let Some(sample) = sample else {
        return Ok(report);
    };

    // Remembered rather than returned immediately: from the moment raw mode is
    // enabled below, the restore at the end outranks every mid-probe error.
    let mut failure: Option<WriteError> = None;

    // A device already reporting raw is owned by whoever turned it on
    // (Options+ mid-gesture, a stale session); listen only, restore nothing.
    let wrote_mode = !report.state_before.contains(RawReportFlags::RAW);
    if wrote_mode {
        if let Err(e) = feature.set_raw_report_state(RawReportFlags::RAW).await {
            return Err(WriteError::Hidpp(format!("{e:?}")));
        }
        match feature.get_raw_report_state().await {
            Ok(confirmed) => report.state_after_set = Some(confirmed),
            Err(e) => failure = Some(WriteError::Hidpp(format!("{e:?}"))),
        }
    }

    // The EventSource listener is registered in the feature constructor, so
    // frames emitted before this receiver exists would be lost — but raw mode
    // was only just enabled, so there is no earlier frame to lose.
    sample_touch_frames(
        &feature,
        &mut report.frames,
        &mut report.frames_received,
        sample,
        shutdown,
    )
    .await;

    if wrote_mode {
        if let Err(e) = feature.set_raw_report_state(state_before).await {
            return Err(WriteError::Hidpp(format!(
                "raw-report restore failed ({failure:?}): {e:?}"
            )));
        }
        match feature.get_raw_report_state().await {
            Ok(restored) => report.state_restored = Some(restored),
            // A read-back miss after a successful restore write is odd but
            // does not outweigh everything the probe already learned.
            Err(e) => {
                if failure.is_none() {
                    failure = Some(WriteError::Hidpp(format!("{e:?}")));
                }
            }
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// Collect raw touch frames for one sampling window: `frames` gains the
/// recorded logical frames and `*received` counts every DualXy event,
/// repetitions included. Ends at the window, at [`MAX_PROBE_FRAMES`], at an
/// external shutdown, or when the event source drops — whichever first.
///
/// Raw events assemble into *logical* frames: a frame with more than two
/// fingers arrives as several events sharing a timestamp, the last one
/// carrying `end_of_frame`. Deduplication works on the assembled unit — a
/// resting multi-finger hand repeats the same 2–3-event group at report rate,
/// and per-event comparison would let it flood right back.
async fn sample_touch_frames(
    feature: &TouchpadRawXyFeature,
    frames: &mut Vec<DualXyData>,
    received: &mut usize,
    sample: Duration,
    mut shutdown: Option<oneshot::Receiver<()>>,
) {
    let events = feature.listen();
    let deadline = tokio::time::Instant::now() + sample;
    let mut pending_ts: Option<u16> = None;
    let mut pending_parts: Vec<DualXyData> = Vec::new();
    loop {
        if tokio::time::Instant::now() >= deadline || frames.len() >= MAX_PROBE_FRAMES {
            commit_logical(frames, &mut pending_parts);
            break;
        }
        // An external shutdown resolves here; with `None` this arm never
        // wakes and the window (or the frame cap) ends the loop instead.
        let shutdown_wait = async {
            match shutdown.as_mut() {
                Some(rx) => {
                    let _ = rx.await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(shutdown_wait);
        tokio::select! {
            () = &mut shutdown_wait => {
                commit_logical(frames, &mut pending_parts);
                break;
            }
            // Window over.
            () = tokio::time::sleep_until(deadline) => {
                commit_logical(frames, &mut pending_parts);
                break;
            }
            event = events.recv() => match event {
                Ok(TouchpadRawEvent::DualXy(frame)) => {
                    *received += 1;
                    // A fresh group starts whenever the timestamp moves on or
                    // the previous group already ended in an end-of-frame;
                    // otherwise this event continues the pending group.
                    if !pending_parts.is_empty()
                        && (Some(frame.timestamp) != pending_ts || !group_open(&pending_parts))
                    {
                        commit_logical(frames, &mut pending_parts);
                    }
                    pending_ts = Some(frame.timestamp);
                    pending_parts.push(frame);
                }
                Ok(_) => {}
                // Event source dropped: the channel is going away.
                Err(_) => break,
            },
        }
    }
}

/// Firmware fields for one entity whose record the device answered and this
/// parser decoded.
///
/// Owned, constructible data converted from `hidpp`'s
/// `DeviceEntityFirmwareInfo`, the same way [`ReprogControlEntry`] is
/// converted from `CidInfo`: consumers get the structured record and decide
/// how to render it, rather than being handed a pre-formatted string with the
/// rest of the fields dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareEntityInfo {
    /// What the entity is: main application, bootloader, radio stack, and so
    /// on.
    pub kind: DeviceEntityType,
    /// Three-letter prefix of the firmware name, e.g. `MPM`.
    pub prefix: String,
    /// Firmware number, BCD-decoded by the protocol layer.
    pub number: u8,
    /// Firmware revision, BCD-decoded by the protocol layer.
    pub revision: u8,
    /// Firmware build, BCD-decoded by the protocol layer.
    pub build: u16,
    /// Whether this is the entity currently running.
    pub active: bool,
    /// USB or wireless product ID the entity runs under. A bootloader entity
    /// reports the PID the device enumerates as while in DFU mode; only the
    /// active entity is required to report a real value, so an inactive one
    /// may be zero.
    pub transport_pid: u16,
    /// Optional extra versioning bytes. Device-specific and usually all zero,
    /// carried verbatim because a device that does populate them is exactly
    /// the device a report is being collected for.
    pub extra_version: [u8; 5],
}

impl From<DeviceEntityFirmwareInfo> for FirmwareEntityInfo {
    fn from(info: DeviceEntityFirmwareInfo) -> Self {
        Self {
            kind: info.entity_type,
            prefix: info.firmware_prefix,
            number: info.firmware_number,
            revision: info.revision,
            build: info.build,
            active: info.active,
            transport_pid: info.transport_pid,
            extra_version: info.extra_version,
        }
    }
}

/// One firmware entity a device reports through HID++ `0x0003` function 1.
/// Returned by [`dump_firmware_entities`] so a device report can name the
/// exact firmware it is running.
///
/// There are two states and only two: the device answered with a record that
/// decoded, or it did not. An enum makes "a version with no kind" and "an
/// error alongside a version" unrepresentable rather than merely unreachable.
#[derive(Debug, Clone)]
pub enum FirmwareEntity {
    /// The entity's record was read and decoded.
    Readable {
        /// Index of the entity in the device's own table.
        index: u8,
        /// The decoded firmware record.
        info: FirmwareEntityInfo,
    },
    /// The device declared the entity, but its record could not be read.
    ///
    /// Reported rather than dropped: omitting the row would claim the device
    /// has fewer firmware images than it says it has, and a device that cannot
    /// describe one of its own images is what a bug report needs to say.
    Unreadable {
        /// Index of the entity in the device's own table.
        index: u8,
        /// Why the record could not be read.
        error: WriteError,
    },
}

/// Read every firmware entity the device on `route` reports.
///
/// A device lists its main application firmware alongside its bootloader and,
/// on many models, a separate radio stack. `openlogi diag features` prints
/// them so a bug report names the firmware that produced the behaviour rather
/// than just the model.
///
/// A single entity the *device* declined or answered unparseably does not fail
/// the call — see [`FirmwareEntity::Unreadable`]. A channel failure does: the
/// route is gone, not the entity.
pub async fn dump_firmware_entities(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<Vec<FirmwareEntity>, WriteError> {
    let index = route.device_index();
    with_route(backend, route, move |channel| async move {
        dump_firmware_entities_on_channel(&channel, index).await
    })
    .await
}

/// [`dump_firmware_entities`] against an already-open channel, the shape the
/// tests drive a scripted device through.
pub(crate) async fn dump_firmware_entities_on_channel(
    channel: &Arc<HidppChannel>,
    index: u8,
) -> Result<Vec<FirmwareEntity>, WriteError> {
    let mut device = Device::new(Arc::clone(channel), index)
        .await
        .map_err(|_| WriteError::DeviceUnreachable { index })?;
    let feature = open_feature::<DeviceInformationFeature>(&mut device).await?;
    let info = feature.get_device_info().await.map_err(|e| {
        classify_hidpp_error(
            e,
            HidppOperation::DumpFeatures,
            DeviceInformationFeature::ID,
        )
    })?;

    let mut entries = Vec::with_capacity(usize::from(info.entity_count));
    for entity in 0..info.entity_count {
        match feature.get_fw_info(entity).await {
            Ok(fw) => entries.push(FirmwareEntity::Readable {
                index: entity,
                info: fw.into(),
            }),
            // The device answered about *this* entity and the answer was no:
            // it refused the read, or it sent a record this parser cannot
            // decode (a G502's radio stack reports a build field that is not
            // valid BCD). The rest of the table is still worth reading.
            Err(e @ (Hidpp20Error::Feature(_) | Hidpp20Error::UnsupportedResponse)) => {
                entries.push(FirmwareEntity::Unreadable {
                    index: entity,
                    error: classify_hidpp_error(
                        e,
                        HidppOperation::DumpFeatures,
                        DeviceInformationFeature::ID,
                    ),
                });
            }
            // A channel failure says nothing about the entity — the route
            // disappeared. Carrying on would spend a timeout per remaining
            // entity and then print malformed-firmware rows for a disconnect.
            Err(e) => {
                return Err(classify_hidpp_error(
                    e,
                    HidppOperation::DumpFeatures,
                    DeviceInformationFeature::ID,
                ));
            }
        }
    }
    Ok(entries)
}
