//! Async Linux `LED1` (`unoq:user-{red,green,blue}1`) connectivity & fault status indicator.
//!
//! Runs exclusively on the `tokio` control plane (never inside the RT loop) and yields
//! whenever `/run/tactus/fault` exists so `tactus-led-fault.service` owns the fast-blinking red
//! alarm even if `tactusd` is restarting. On non-Linux hosts or boards lacking `unoq:user-*1`,
//! this module cleanly no-ops.

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
const FAULT_SENTINEL_PATH: &str = "/run/tactus/fault";
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
    let channels = LedChannels::unoq_default();
    if !channels.exists() {
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
#[derive(Debug, Clone)]
struct LedChannels {
    red: PathBuf,
    green: PathBuf,
    blue: PathBuf,
}

#[cfg(any(target_os = "linux", test))]
impl LedChannels {
    #[cfg(target_os = "linux")]
    fn unoq_default() -> Self {
        Self {
            red: PathBuf::from(LED_RED_PATH),
            green: PathBuf::from(LED_GREEN_PATH),
            blue: PathBuf::from(LED_BLUE_PATH),
        }
    }

    #[cfg(target_os = "linux")]
    fn exists(&self) -> bool {
        self.red.is_dir() && self.green.is_dir() && self.blue.is_dir()
    }
}

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
    let Some([red_out, green_out, blue_out]) = mode_outputs(mode) else {
        return Ok(());
    };
    write_channel(&channels.red, red_out)?;
    write_channel(&channels.green, green_out)?;
    write_channel(&channels.blue, blue_out)?;
    Ok(())
}

#[cfg(target_os = "linux")]
struct Sentinels {
    fault_active: bool,
    ap_expected: bool,
    ap_ready: bool,
}

#[cfg(target_os = "linux")]
fn read_sentinels() -> Sentinels {
    Sentinels {
        fault_active: Path::new(FAULT_SENTINEL_PATH).exists(),
        ap_expected: Path::new(AP_WANTS_SYMLINK_PATH).exists(),
        ap_ready: Path::new(AP_READY_SENTINEL_PATH).exists(),
    }
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
            let s = read_sentinels();
            compute_status_led_mode(&state, s.fault_active, s.ap_expected, s.ap_ready)
        };

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
        let red = root.join("unoq:user-red1");
        let green = root.join("unoq:user-green1");
        let blue = root.join("unoq:user-blue1");
        for dir in [&red, &green, &blue] {
            fs::create_dir_all(dir).expect("create led dir");
            fs::write(dir.join("trigger"), "init\n").expect("seed trigger");
            fs::write(dir.join("brightness"), "9\n").expect("seed brightness");
        }
        (root, LedChannels { red, green, blue })
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
        assert_eq!(
            fs::read_to_string(channels.red.join("trigger")).unwrap(),
            "none\n"
        );
        assert_eq!(
            fs::read_to_string(channels.red.join("brightness")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(channels.green.join("trigger")).unwrap(),
            "none\n"
        );
        assert_eq!(
            fs::read_to_string(channels.green.join("brightness")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(channels.blue.join("trigger")).unwrap(),
            "timer\n"
        );

        apply_mode(&channels, StatusLedMode::ApReadyIdle).expect("ap ready");
        assert_eq!(
            fs::read_to_string(channels.blue.join("trigger")).unwrap(),
            "none\n"
        );
        assert_eq!(
            fs::read_to_string(channels.blue.join("brightness")).unwrap(),
            "1\n"
        );

        apply_mode(&channels, StatusLedMode::MidiOnly).expect("midi only");
        assert_eq!(
            fs::read_to_string(channels.red.join("brightness")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(channels.green.join("brightness")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(channels.blue.join("brightness")).unwrap(),
            "0\n"
        );

        apply_mode(&channels, StatusLedMode::LinkOnly).expect("link only");
        assert_eq!(
            fs::read_to_string(channels.red.join("brightness")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(channels.green.join("brightness")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(channels.blue.join("brightness")).unwrap(),
            "0\n"
        );

        apply_mode(&channels, StatusLedMode::LinkAndMidi).expect("link and midi");
        assert_eq!(
            fs::read_to_string(channels.red.join("brightness")).unwrap(),
            "0\n"
        );
        assert_eq!(
            fs::read_to_string(channels.green.join("brightness")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(channels.blue.join("brightness")).unwrap(),
            "1\n"
        );

        // Sentinel values must remain untouched when FaultYield is applied.
        fs::write(channels.red.join("trigger"), "timer\n").unwrap();
        fs::write(channels.red.join("brightness"), "1\n").unwrap();
        apply_mode(&channels, StatusLedMode::FaultYield).expect("fault yield");
        assert_eq!(
            fs::read_to_string(channels.red.join("trigger")).unwrap(),
            "timer\n"
        );
        assert_eq!(
            fs::read_to_string(channels.red.join("brightness")).unwrap(),
            "1\n"
        );

        let _ = fs::remove_dir_all(root);
    }
}
