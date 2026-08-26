//! `openlogi diag touchpad` — probe the HID++ `0x6100 TouchpadRawXy` feature.
//!
//! Answers, against real hardware, the questions every touchpad-support
//! decision hangs on: does the device expose `0x6100`, what pad does it
//! describe (size, sensor DPI, origin, finger count), what raw-report mode is
//! it in, and — with `--seconds` — what raw touch frames actually arrive while
//! the mode is on. The device's raw-report mode is restored afterwards.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use openlogi_hid::write::{DualXyData, RawReportFlags, TouchPoint, TouchpadInfo};

use crate::cmd::diag::select_device;

/// How many leading frames to pretty-print after a sampled window.
const PRINTED_FRAMES: usize = 8;

#[derive(Debug, Args)]
pub struct TouchpadArgs {
    /// Sample raw touch frames for this many seconds. The touchpad's
    /// raw-report mode is switched on for the window and restored after.
    #[arg(long, value_name = "SECONDS")]
    pub seconds: Option<u64>,

    /// Run against the device whose name contains this string
    /// (case-insensitive) instead of auto-selecting.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
}

pub async fn run(args: TouchpadArgs) -> Result<()> {
    let (route, name) = select_device(args.device.as_deref(), &[0x6100]).await?;
    println!("device: {name} ({route})");

    let sample = args.seconds.map(Duration::from_secs);
    // Ctrl-C ends the window gracefully: the device-side restore runs before
    // the future returns, so ^C mid-sample never leaves raw mode enabled.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            println!("\n  (interrupted — restoring raw-report mode)");
            let _ = shutdown_tx.send(());
        }
    });
    let report = openlogi_hid::probe_touchpad(&route, sample, Some(shutdown_rx))
        .await
        .context("probe 0x6100 TouchpadRawXy")?;

    println!("feature 0x6100 TouchpadRawXy v{}", report.feature_version);
    print_info(&report.info);
    println!(
        "  raw report mode: 0x{:02x} ({})",
        report.state_before.bits(),
        raw_flags_label(report.state_before),
    );

    let Some(seconds) = args.seconds else {
        println!("  (pass --seconds N to sample raw touch frames)");
        return Ok(());
    };

    if let Some(confirmed) = report.state_after_set {
        println!(
            "  raw mode requested 0x01, device confirmed 0x{:02x} ({})",
            confirmed.bits(),
            raw_flags_label(confirmed),
        );
    } else {
        println!("  raw mode was already on — sampling without touching it");
    }

    print_frames(
        &report.frames,
        report.frames_received,
        &report.info,
        seconds,
    );

    if let Some(restored) = report.state_restored {
        println!(
            "  raw mode restored → 0x{:02x} ({})",
            restored.bits(),
            raw_flags_label(restored),
        );
    }
    Ok(())
}

fn print_info(info: &TouchpadInfo) {
    println!(
        "  pad {}x{} units, sensor {} dpi, origin {:?}, max {} fingers",
        info.x_size, info.y_size, info.dpi, info.origin, info.max_finger_count
    );
    println!(
        "  z_range=0x{:02x} area_range=0x{:02x} ts_unit={} (0.1 ms) mapping=v{} pen={}",
        info.z_data_range,
        info.area_data_range,
        info.timestamp_units,
        info.raw_report_mapping_version,
        if info.pen_support { "yes" } else { "no" }
    );
}

fn print_frames(frames: &[DualXyData], frames_received: usize, info: &TouchpadInfo, seconds: u64) {
    if frames.is_empty() {
        println!(
            "  sampled {seconds}s: {frames_received} frames received — touch the pad while sampling"
        );
        return;
    }

    let end_of_frames = frames.iter().filter(|f| f.end_of_frame).count();
    let max_fingers = frames.iter().map(|f| f.finger_count).max().unwrap_or(0);
    let button_frames = frames.iter().filter(|f| f.button).count();
    // Only in-contact slots count toward the observed ranges: an unused slot
    // is an id-preserving, zeroed hover placeholder (measured on Casa Touch),
    // and counting its (0, 0) would claim fingers swept the pad's corner.
    let contacts: Vec<(u16, u16)> = frames
        .iter()
        .flat_map(|f| [f.touch1, f.touch2])
        .filter(|p| p.contact_status == 1)
        .map(|p| (p.x, p.y))
        .collect();
    println!(
        "  sampled {seconds}s: {frames_received} frames received, {} distinct logical frames \
         ({end_of_frames} end-of-frame), max fingers {max_fingers}, button in {button_frames}",
        frames.len(),
    );
    if contacts.is_empty() {
        println!("  no in-contact samples — every slot hovered");
    } else {
        let x_min = contacts.iter().map(|(x, _)| *x).min().unwrap_or(0);
        let x_max = contacts.iter().map(|(x, _)| *x).max().unwrap_or(0);
        let y_min = contacts.iter().map(|(_, y)| *y).min().unwrap_or(0);
        let y_max = contacts.iter().map(|(_, y)| *y).max().unwrap_or(0);
        println!(
            "  x observed {x_min}..{x_max} of {}, y observed {y_min}..{y_max} of {}",
            info.x_size, info.y_size,
        );
    }
    println!("  first frames:");
    for frame in frames.iter().take(PRINTED_FRAMES) {
        println!("    {}", frame_line(frame));
    }
}

/// One frame as a single diagnostics line: timestamp, finger count, both raw
/// touch slots. A slot the device left zeroed still prints — a probe reports
/// what the wire said, not an interpretation of it.
fn frame_line(frame: &DualXyData) -> String {
    format!(
        "ts={:>5} fc={} t1=[{}] t2=[{}]{}{}",
        frame.timestamp,
        frame.finger_count,
        touch_line(&frame.touch1),
        touch_line(&frame.touch2),
        if frame.button { " btn" } else { "" },
        if frame.end_of_frame { " eof" } else { "" },
    )
}

fn touch_line(point: &TouchPoint) -> String {
    format!(
        "id{} c{} x={} y={} z={} a={}",
        point.finger_id, point.contact_status, point.x, point.y, point.z, point.area
    )
}

fn raw_flags_label(flags: RawReportFlags) -> String {
    const NAMED: [(RawReportFlags, &str); 7] = [
        (RawReportFlags::RAW, "raw"),
        (RawReportFlags::FORCE_ADD, "force"),
        (RawReportFlags::ENHANCED, "enhanced"),
        (RawReportFlags::WIDTH_HEIGHT, "w/h"),
        (RawReportFlags::NATIVE_GESTURE, "native-gest"),
        (RawReportFlags::MAJOR_MINOR, "maj/min"),
        (RawReportFlags::WIDTH_HEIGHT_8BIT, "w/h8"),
    ];
    if flags.is_empty() {
        return "off".to_owned();
    }
    let mut separator = "";
    let mut label = String::new();
    for (flag, name) in NAMED {
        if flags.contains(flag) {
            label.push_str(separator);
            label.push_str(name);
            separator = "+";
        }
    }
    label
}
