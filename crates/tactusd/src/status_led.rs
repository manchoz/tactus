//! Async Linux `LED1` (`unoq:user-{red,green,blue}1`) connectivity & fault status indicator.
//!
//! Runs exclusively on the `tokio` control plane (never inside the RT loop) and yields
//! whenever any `/run/tactus/fault.*` sentinel exists so `tactus-led-fault@.service` owns the
//! fast-blinking red alarm even if another unit restarts. On non-Linux hosts or boards lacking
//! `unoq:user-*1`, this module cleanly no-ops.

#[cfg(any(target_os = "linux", test))]
use std::fs;
#[cfg(any(target_os = "linux", test))]
use std::io;
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::Duration;

use tactus_protocol::AppState;
use tokio::sync::watch;
use tokio::task::JoinHandle;
#[cfg(target_os = "linux")]
use tokio::time::{self, MissedTickBehavior};

#[cfg(target_os = "linux")]
use crate::log::log_warn;

#[cfg(target_os = "linux")]
const LED_RED_PATH: &str = "/sys/class/leds/unoq:user-red1";
#[cfg(target_os = "linux")]
const LED_GREEN_PATH: &str = "/sys/class/leds/unoq:user-green1";
#[cfg(target_os = "linux")]
const LED_BLUE_PATH: &str = "/sys/class/leds/unoq:user-blue1";

#[cfg(target_os = "linux")]
const FAULT_SENTINEL_PATHS: [&str; 3] = [
    "/run/tactus/fault.tactus-ap.service",
    "/run/tactus/fault.tactus-dhcp.service",
    "/run/tactus/fault.tactusd.service",
];
#[cfg(target_os = "linux")]
const AP_READY_SENTINEL_PATH: &str = "/run/tactus/ap-ready";
#[cfg(target_os = "linux")]
const AP_WANTS_SYMLINK_PATH: &str = "/etc/systemd/system/multi-user.target.wants/tactus-ap.service";

#[cfg(target_os = "linux")]
const SENTINEL_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusLedMode {
    FaultYield,
    Booting,
    ApReadyIdle,
    MidiOnly,
    LinkOnly,
    LinkAndMidi,
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn compute_status_led_mode(
    state: &AppState,
    fault_active: bool,
    ap_expected: bool,
    ap_ready: bool,
) -> StatusLedMode {
    if fault_active {
        return StatusLedMode::FaultYield;
    }

    let has_link = state.telemetry.link_peers > 0;
    let has_midi = state.selected_midi_input.is_some() || !state.selected_midi_outputs.is_empty();

    match (has_link, has_midi) {
        (true, true) => StatusLedMode::LinkAndMidi,
        (true, false) => StatusLedMode::LinkOnly,
        (false, true) => StatusLedMode::MidiOnly,
        (false, false) => {
            if ap_expected && !ap_ready {
                StatusLedMode::Booting
            } else {
                StatusLedMode::ApReadyIdle
            }
        }
    }
}

/// Spawn the optional Linux `LED1` status indicator task on the async control plane.
#[cfg(target_os = "linux")]
pub fn spawn_status_led(
    state_rx: watch::Receiver<AppState>,
    shutdown_rx: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    let channels: LedChannels = [
        PathBuf::from(LED_RED_PATH),
        PathBuf::from(LED_GREEN_PATH),
        PathBuf::from(LED_BLUE_PATH),
    ];
    if !channels.iter().all(|dir| dir.is_dir()) {
        return None;
    }
    Some(tokio::spawn(run_status_led_loop(
        channels,
        state_rx,
        shutdown_rx,
    )))
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_status_led(
    _state_rx: watch::Receiver<AppState>,
    _shutdown_rx: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    None
}

#[cfg(any(target_os = "linux", test))]
type LedChannels = [PathBuf; 3];

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelOutput {
    Solid(bool),
    Timer,
}

#[cfg(any(target_os = "linux", test))]
fn mode_outputs(mode: StatusLedMode) -> Option<[ChannelOutput; 3]> {
    use ChannelOutput::{Solid, Timer};
    match mode {
        StatusLedMode::FaultYield => None,
        StatusLedMode::Booting => Some([Solid(false), Solid(false), Timer]),
        StatusLedMode::ApReadyIdle => Some([Solid(false), Solid(false), Solid(true)]),
        StatusLedMode::MidiOnly => Some([Solid(true), Solid(true), Solid(false)]),
        StatusLedMode::LinkOnly => Some([Solid(false), Solid(true), Solid(false)]),
        StatusLedMode::LinkAndMidi => Some([Solid(false), Solid(true), Solid(true)]),
    }
}

#[cfg(any(target_os = "linux", test))]
fn write_channel(dir: &Path, output: ChannelOutput) -> io::Result<()> {
    let trigger_path = dir.join("trigger");
    let brightness_path = dir.join("brightness");
    match output {
        ChannelOutput::Solid(on) => {
            fs::write(&trigger_path, "none\n")?;
            fs::write(&brightness_path, if on { "1\n" } else { "0\n" })?;
        }
        ChannelOutput::Timer => {
            fs::write(&trigger_path, "timer\n")?;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn apply_mode(channels: &LedChannels, mode: StatusLedMode) -> io::Result<()> {
    let Some(outputs) = mode_outputs(mode) else {
        return Ok(());
    };
    for (dir, output) in channels.iter().zip(outputs) {
        write_channel(dir, output)?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn red_has_timer_trigger(red_dir: &Path) -> bool {
    fs::read_to_string(red_dir.join("trigger")).is_ok_and(|s| s.contains("[timer]"))
}

#[cfg(target_os = "linux")]
async fn run_status_led_loop(
    channels: LedChannels,
    mut state_rx: watch::Receiver<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut last_applied: Option<StatusLedMode> = None;
    let mut write_err_logged = false;
    let mut tick = time::interval(SENTINEL_POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        let mode = {
            let state = state_rx.borrow_and_update().clone();
            let fault_active = FAULT_SENTINEL_PATHS.iter().any(|p| Path::new(p).exists());
            let ap_expected = Path::new(AP_WANTS_SYMLINK_PATH).exists();
            let ap_ready = Path::new(AP_READY_SENTINEL_PATH).exists();
            compute_status_led_mode(&state, fault_active, ap_expected, ap_ready)
        };

        // Re-apply if a transient fault blinked red and cleared between polls.
        if mode != StatusLedMode::FaultYield
            && last_applied == Some(mode)
            && red_has_timer_trigger(&channels[0])
        {
            last_applied = None;
        }

        if last_applied != Some(mode) {
            match apply_mode(&channels, mode) {
                Ok(()) => {
                    last_applied = Some(mode);
                    write_err_logged = false;
                }
                Err(err) => {
                    if !write_err_logged {
                        log_warn!("status LED update failed ({err}); continuing without LED1");
                        write_err_logged = true;
                    }
                }
            }
        }

        tokio::select! {
            biased;
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow_and_update() {
                    break;
                }
            }
            res = state_rx.changed() => {
                if res.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_led_channels() -> (PathBuf, LedChannels) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tactus-status-led-test-{}-{}",
            std::process::id(),
            nanos
        ));
        let channels = [
            root.join("unoq:user-red1"),
            root.join("unoq:user-green1"),
            root.join("unoq:user-blue1"),
        ];
        for dir in &channels {
            fs::create_dir_all(dir).expect("create led dir");
            fs::write(dir.join("trigger"), "init\n").expect("seed trigger");
            fs::write(dir.join("brightness"), "9\n").expect("seed brightness");
        }
        (root, channels)
    }

    fn assert_led_state(channels: &LedChannels, expected: [(&str, &str); 3]) {
        for (dir, (exp_trig, exp_bright)) in channels.iter().zip(expected) {
            assert_eq!(fs::read_to_string(dir.join("trigger")).unwrap(), exp_trig);
            assert_eq!(
                fs::read_to_string(dir.join("brightness")).unwrap(),
                exp_bright
            );
        }
    }

    #[test]
    fn computes_all_status_led_modes_and_fault_precedence() {
        let mut state = AppState::default();

        assert_eq!(
            compute_status_led_mode(&state, false, true, false),
            StatusLedMode::Booting
        );
        assert_eq!(
            compute_status_led_mode(&state, false, false, false),
            StatusLedMode::ApReadyIdle
        );
        assert_eq!(
            compute_status_led_mode(&state, false, true, true),
            StatusLedMode::ApReadyIdle
        );

        state.selected_midi_input = Some("hw:1,0,0".to_string());
        assert_eq!(
            compute_status_led_mode(&state, false, true, true),
            StatusLedMode::MidiOnly
        );

        state.selected_midi_input = None;
        state.selected_midi_outputs = vec!["hw:2,0,0".to_string()];
        assert_eq!(
            compute_status_led_mode(&state, false, true, true),
            StatusLedMode::MidiOnly
        );

        state.telemetry.link_peers = 2;
        assert_eq!(
            compute_status_led_mode(&state, false, true, true),
            StatusLedMode::LinkAndMidi
        );

        state.selected_midi_outputs.clear();
        assert_eq!(
            compute_status_led_mode(&state, false, true, true),
            StatusLedMode::LinkOnly
        );

        // Fault overrides Link + MIDI so kernel red blink is never overwritten.
        assert_eq!(
            compute_status_led_mode(&state, true, true, true),
            StatusLedMode::FaultYield
        );
    }

    #[test]
    fn apply_mode_writes_expected_sysfs_files_and_yields_on_fault() {
        let (root, channels) = make_temp_led_channels();

        apply_mode(&channels, StatusLedMode::Booting).expect("booting");
        assert_led_state(
            &channels,
            [("none\n", "0\n"), ("none\n", "0\n"), ("timer\n", "9\n")],
        );

        apply_mode(&channels, StatusLedMode::ApReadyIdle).expect("ap ready");
        assert_led_state(
            &channels,
            [("none\n", "0\n"), ("none\n", "0\n"), ("none\n", "1\n")],
        );

        apply_mode(&channels, StatusLedMode::MidiOnly).expect("midi only");
        assert_led_state(
            &channels,
            [("none\n", "1\n"), ("none\n", "1\n"), ("none\n", "0\n")],
        );

        apply_mode(&channels, StatusLedMode::LinkOnly).expect("link only");
        assert_led_state(
            &channels,
            [("none\n", "0\n"), ("none\n", "1\n"), ("none\n", "0\n")],
        );

        apply_mode(&channels, StatusLedMode::LinkAndMidi).expect("link and midi");
        assert_led_state(
            &channels,
            [("none\n", "0\n"), ("none\n", "1\n"), ("none\n", "1\n")],
        );

        // Sentinel values must remain untouched when FaultYield is applied,
        // and active kernel [timer] is detected for fast recovery.
        fs::write(channels[0].join("trigger"), "none [timer] heartbeat\n").unwrap();
        fs::write(channels[0].join("brightness"), "1\n").unwrap();
        assert!(red_has_timer_trigger(&channels[0]));
        apply_mode(&channels, StatusLedMode::FaultYield).expect("fault yield");
        assert_eq!(
            fs::read_to_string(channels[0].join("trigger")).unwrap(),
            "none [timer] heartbeat\n"
        );
        assert_eq!(
            fs::read_to_string(channels[0].join("brightness")).unwrap(),
            "1\n"
        );

        let _ = fs::remove_dir_all(root);
    }
}
