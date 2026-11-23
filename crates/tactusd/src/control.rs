//! Async `tokio` control plane: state hub, MIDI hotplug coordinator, FD reaper, web UI, and config sync.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer};
use tactus_protocol::{
    AppState, EngineCommand, MidiPortInfo, RtCommand, TactusConfig, TelemetrySnapshot,
};
use tactus_sync_core::{MAX_VALID_BPM, MIN_VALID_BPM};
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::config_store::ConfigPersister;
use crate::log::{log_error, log_info, log_warn};
use crate::midi::hotplug::UeventMonitor;
use crate::midi::{close_fd_set, match_port, MidiBackend};
use crate::rt::{FdReturn, RtMessage, WakeSender};
use crate::web;

const TELEMETRY_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HOTPLUG_RESCAN_FALLBACK: Duration = Duration::from_secs(5);
const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(150);
const WEB_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Public control-plane handle shared with M4 (`axum`) and future M5 (`arduino-router`) tasks.
#[derive(Clone)]
pub struct ControlHandle {
    pub state_rx: watch::Receiver<AppState>,
    pub cmd_tx: mpsc::Sender<EngineCommand>,
}

/// Owned endpoints needed by [`run_control_plane`].
pub struct ControlPlaneArgs {
    pub config: TactusConfig,
    pub config_path: PathBuf,
    pub backend: Arc<dyn MidiBackend>,
    pub rt_cmd_tx: Producer<RtMessage>,
    pub tel_rx: Consumer<TelemetrySnapshot>,
    pub fd_ret_rx: Consumer<FdReturn>,
    pub wake_tx: WakeSender,
    pub rt_handle: JoinHandle<()>,
}

struct StateHub {
    config: TactusConfig,
    persister: ConfigPersister,
    backend: Arc<dyn MidiBackend>,
    rt_cmd_tx: Producer<RtMessage>,
    tel_rx: Consumer<TelemetrySnapshot>,
    fd_ret_rx: Consumer<FdReturn>,
    wake_tx: WakeSender,
    state_tx: watch::Sender<AppState>,
    attached_input_id: Option<String>,
    attached_output_id: Option<String>,
    fd_generation: u64,
    last_logged_snap: Option<TelemetrySnapshot>,
    last_ports: Vec<MidiPortInfo>,
    ports_logged_once: bool,
}

impl StateHub {
    async fn reconcile_midi(&mut self, force_reopen: bool) {
        let _ = self.drain_rt_returns();
        let backend = Arc::clone(&self.backend);
        let ports = match tokio::task::spawn_blocking(move || backend.enumerate_ports()).await {
            Ok(Ok(ports)) => ports,
            Ok(Err(err)) => {
                log_warn!("MIDI port enumeration failed: {err}");
                return;
            }
            Err(err) => {
                log_error!("MIDI port enumeration task panicked: {err}");
                return;
            }
        };

        if !self.ports_logged_once || ports != self.last_ports {
            if ports.is_empty() {
                log_info!("ALSA RawMIDI ports: none detected");
            } else {
                let summary: Vec<String> = ports
                    .iter()
                    .map(|p| {
                        let rw = match (p.can_read, p.can_write) {
                            (true, true) => "RW",
                            (true, false) => "R",
                            (false, true) => "W",
                            (false, false) => "-",
                        };
                        format!("{} ({}: {} [{rw}])", p.id, p.card_name, p.port_name)
                    })
                    .collect();
                log_info!("ALSA RawMIDI ports: {}", summary.join(", "));
            }
            self.last_ports.clone_from(&ports);
            self.ports_logged_once = true;
        }

        let matched_in =
            match_port(&ports, &self.config.midi.input_port_match, true, false).cloned();
        let matched_out =
            match_port(&ports, &self.config.midi.output_port_match, false, true).cloned();

        let new_in_id = matched_in.as_ref().map(|p| p.id.clone());
        let new_out_id = matched_out.as_ref().map(|p| p.id.clone());

        self.state_tx.send_if_modified(|state| {
            let changed = state.available_midi_ports != ports
                || state.selected_midi_input != new_in_id
                || state.selected_midi_output != new_out_id
                || state.midi_input_match != self.config.midi.input_port_match
                || state.midi_output_match != self.config.midi.output_port_match;
            if changed {
                state.available_midi_ports = ports;
                state.selected_midi_input.clone_from(&new_in_id);
                state.selected_midi_output.clone_from(&new_out_id);
                state
                    .midi_input_match
                    .clone_from(&self.config.midi.input_port_match);
                state
                    .midi_output_match
                    .clone_from(&self.config.midi.output_port_match);
            }
            changed
        });

        let selection_changed = force_reopen
            || new_in_id != self.attached_input_id
            || new_out_id != self.attached_output_id;
        if !selection_changed {
            return;
        }

        let next_gen = self.fd_generation.wrapping_add(1);
        let backend = Arc::clone(&self.backend);
        let in_clone = matched_in.clone();
        let out_clone = matched_out.clone();

        let opened = tokio::task::spawn_blocking(move || {
            backend.open_ports(in_clone.as_ref(), out_clone.as_ref(), next_gen)
        })
        .await;

        let new_fds = match opened {
            Ok(Ok(fds)) => fds,
            Ok(Err(err)) => {
                log_warn!(
                    "failed to open MIDI ports (in={:?}, out={:?}): {err}",
                    new_in_id,
                    new_out_id
                );
                return;
            }
            Err(err) => {
                log_error!("MIDI open task panicked: {err}");
                return;
            }
        };

        if self.rt_cmd_tx.push(RtMessage::AttachMidi(new_fds)).is_err() {
            close_fd_set(new_fds);
            log_warn!("RT command ring full while attaching MIDI FDs; will retry");
            return;
        }

        self.fd_generation = next_gen;
        self.attached_input_id = new_in_id.clone();
        self.attached_output_id = new_out_id.clone();
        self.wake_tx.wake();

        if new_fds.is_empty() {
            log_info!(
                "MIDI endpoints detached (match input={:?}, output={:?})",
                self.config.midi.input_port_match,
                self.config.midi.output_port_match
            );
        } else {
            log_info!(
                "attached RawMIDI endpoints (gen={next_gen}, in={:?}, out={:?}, shared_duplex={})",
                new_in_id,
                new_out_id,
                new_fds.in_fd >= 0 && new_fds.in_fd == new_fds.out_fd
            );
        }
    }

    async fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SelectMidiPorts { input, output } => {
                self.config.midi.input_port_match = input;
                self.config.midi.output_port_match = output;
                self.state_tx.send_if_modified(|state| {
                    let changed = state.midi_input_match != self.config.midi.input_port_match
                        || state.midi_output_match != self.config.midi.output_port_match;
                    if changed {
                        state
                            .midi_input_match
                            .clone_from(&self.config.midi.input_port_match);
                        state
                            .midi_output_match
                            .clone_from(&self.config.midi.output_port_match);
                    }
                    changed
                });
                self.persister.schedule_save(self.config.clone());
                self.reconcile_midi(true).await;
            }
            other => {
                let Ok(rt_cmd) = RtCommand::try_from(&other) else {
                    return;
                };

                let mut persist = false;
                match rt_cmd {
                    RtCommand::SetDirection(dir) => {
                        self.config.sync.direction = dir;
                        persist = true;
                    }
                    RtCommand::SetStartStopSync(enabled) => {
                        self.config.sync.start_stop_sync = enabled;
                        persist = true;
                    }
                    RtCommand::SetOffsetMs(offset_ms) => {
                        if !offset_ms.is_finite() || !(-50.0..=50.0).contains(&offset_ms) {
                            log_warn!("ignoring out-of-bounds SetOffsetMs({offset_ms})");
                            return;
                        }
                        self.config.sync.midi_offset_ms = offset_ms;
                        persist = true;
                    }
                    RtCommand::SetQuantum(quantum) => {
                        if !quantum.is_finite() || quantum <= 0.0 {
                            log_warn!("ignoring invalid SetQuantum({quantum})");
                            return;
                        }
                        self.config.sync.quantum = quantum;
                        persist = true;
                    }
                    RtCommand::SetLinkTempoManual(bpm) => {
                        if !bpm.is_finite() || !(MIN_VALID_BPM..=MAX_VALID_BPM).contains(&bpm) {
                            log_warn!("ignoring out-of-bounds SetLinkTempoManual({bpm})");
                            return;
                        }
                    }
                    RtCommand::ResyncDownbeat => {}
                }

                if self.rt_cmd_tx.push(RtMessage::Command(rt_cmd)).is_err() {
                    log_warn!("RT command ring full; dropping {rt_cmd:?}");
                    return;
                }
                self.wake_tx.wake();

                if persist {
                    self.persister.schedule_save(self.config.clone());
                }
            }
        }
    }

    fn drain_rt_returns(&mut self) -> bool {
        let mut needs_reconcile = false;
        while let Ok(ret) = self.fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) => {
                    close_fd_set(fds);
                }
                FdReturn::DetachedOnError { fds, errno } => {
                    close_fd_set(fds);
                    if fds.generation == self.fd_generation {
                        if fds.in_fd >= 0 {
                            self.attached_input_id = None;
                        }
                        if fds.out_fd >= 0 {
                            self.attached_output_id = None;
                        }
                        needs_reconcile = true;
                    }
                    let os_err = std::io::Error::from_raw_os_error(errno);
                    log_warn!(
                        "RT thread detached MIDI FD (gen={}, in_fd={}, out_fd={}, reason={os_err})",
                        fds.generation,
                        fds.in_fd,
                        fds.out_fd
                    );
                }
            }
        }
        needs_reconcile
    }

    fn drain_telemetry(&mut self) {
        let mut latest = None;
        while let Ok(snap) = self.tel_rx.pop() {
            latest = Some(snap);
        }

        let Some(snap) = latest else {
            return;
        };

        self.state_tx.send_if_modified(|state| {
            if state.telemetry != snap {
                state.telemetry = snap;
                true
            } else {
                false
            }
        });

        let discrete_changed = match self.last_logged_snap {
            None => true,
            Some(prev) => {
                prev.direction != snap.direction
                    || prev.lock_state != snap.lock_state
                    || prev.is_playing != snap.is_playing
                    || prev.link_peers != snap.link_peers
                    || prev.start_stop_sync != snap.start_stop_sync
                    || (prev.bpm_link - snap.bpm_link).abs() >= 0.05
            }
        };

        if discrete_changed {
            log_info!(
                "sync state: dir={:?}, lock={:?}, playing={}, peers={}, bpm_link={:.2}, start_stop={}",
                snap.direction,
                snap.lock_state,
                snap.is_playing,
                snap.link_peers,
                snap.bpm_link,
                snap.start_stop_sync
            );
            self.last_logged_snap = Some(snap);
        }
    }
}

/// Run the headless `tokio` control plane until `SIGTERM` or `SIGINT`.
pub async fn run_control_plane(args: ControlPlaneArgs) {
    let initial_state = AppState {
        telemetry: TelemetrySnapshot {
            direction: args.config.sync.direction,
            start_stop_sync: args.config.sync.start_stop_sync,
            quantum: args.config.sync.quantum,
            midi_offset_ms: args.config.sync.midi_offset_ms,
            ..TelemetrySnapshot::default()
        },
        midi_input_match: args.config.midi.input_port_match.clone(),
        midi_output_match: args.config.midi.output_port_match.clone(),
        ..AppState::default()
    };

    let (state_tx, state_rx) = watch::channel(initial_state);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(64);
    let control_handle = ControlHandle { state_rx, cmd_tx };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let web_handle =
        web::spawn_web_server(&args.config.web.listen_addr, control_handle, shutdown_rx).await;

    let uevent = match UeventMonitor::new() {
        Ok(mon) => Some(mon),
        Err(err) => {
            log_warn!("NETLINK_KOBJECT_UEVENT unavailable ({err}); relying on 5s periodic rescan");
            None
        }
    };

    let mut hub = StateHub {
        config: args.config,
        persister: ConfigPersister::spawn(args.config_path),
        backend: args.backend,
        rt_cmd_tx: args.rt_cmd_tx,
        tel_rx: args.tel_rx,
        fd_ret_rx: args.fd_ret_rx,
        wake_tx: args.wake_tx,
        state_tx,
        attached_input_id: None,
        attached_output_id: None,
        fd_generation: 0,
        last_logged_snap: None,
        last_ports: Vec::new(),
        ports_logged_once: false,
    };

    hub.reconcile_midi(false).await;

    let mut telemetry_tick = time::interval(TELEMETRY_POLL_INTERVAL);
    telemetry_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut rescan_tick = time::interval(HOTPLUG_RESCAN_FALLBACK);
    rescan_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut hotplug_deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                log_info!("shutdown signal received");
                break;
            }
            Some(cmd) = cmd_rx.recv() => {
                hub.handle_command(cmd).await;
            }
            res = wait_uevent_opt(uevent.as_ref()) => {
                match res {
                    Ok(()) => {
                        hotplug_deadline = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                    }
                    Err(err) => {
                        log_warn!("netlink uevent read failed: {err}");
                        hotplug_deadline = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                    }
                }
            }
            () = async {
                if let Some(deadline) = hotplug_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                hotplug_deadline = None;
                hub.reconcile_midi(false).await;
            }
            _ = rescan_tick.tick() => {
                hub.reconcile_midi(false).await;
            }
            _ = telemetry_tick.tick() => {
                if hub.drain_rt_returns() {
                    hotplug_deadline = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                }
                hub.drain_telemetry();
            }
        }
    }

    let _ = shutdown_tx.send(true);
    if let Some(handle) = web_handle {
        if tokio::time::timeout(WEB_SHUTDOWN_TIMEOUT, handle)
            .await
            .is_err()
        {
            log_warn!(
                "web UI server did not stop within {:?}; continuing shutdown",
                WEB_SHUTDOWN_TIMEOUT
            );
        }
    }

    hub.persister.shutdown().await;

    if hub.rt_cmd_tx.push(RtMessage::Shutdown).is_err() {
        log_warn!("RT command ring full while sending Shutdown");
    }
    hub.wake_tx.wake();

    let rt_handle = args.rt_handle;
    if let Err(err) = tokio::task::spawn_blocking(move || {
        let _ = rt_handle.join();
    })
    .await
    {
        log_error!("failed joining rt-sync thread: {err}");
    }

    let _ = hub.drain_rt_returns();
    hub.drain_telemetry();
    log_info!("shutdown complete");
}

async fn wait_uevent_opt(mon: Option<&UeventMonitor>) -> std::io::Result<()> {
    if let Some(monitor) = mon {
        monitor.wait_sound_event().await
    } else {
        std::future::pending().await
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sigint = signal(SignalKind::interrupt()).ok();

    match (sigterm.as_mut(), sigint.as_mut()) {
        (Some(term), Some(int)) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        (Some(term), None) => {
            let _ = term.recv().await;
        }
        (None, Some(int)) => {
            let _ = int.recv().await;
        }
        (None, None) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rtrb::RingBuffer;

    use super::*;
    use crate::midi::MidiFdSet;
    use crate::rt::create_rt_wake_pair;

    struct FakeMidiBackend {
        ports: Mutex<Vec<MidiPortInfo>>,
    }

    impl FakeMidiBackend {
        fn new(ports: Vec<MidiPortInfo>) -> Self {
            Self {
                ports: Mutex::new(ports),
            }
        }

        fn set_ports(&self, ports: Vec<MidiPortInfo>) {
            *self.ports.lock().expect("lock fake ports") = ports;
        }
    }

    impl MidiBackend for FakeMidiBackend {
        fn enumerate_ports(&self) -> std::io::Result<Vec<MidiPortInfo>> {
            Ok(self.ports.lock().expect("lock fake ports").clone())
        }

        fn open_ports(
            &self,
            _input: Option<&MidiPortInfo>,
            _output: Option<&MidiPortInfo>,
            generation: u64,
        ) -> std::io::Result<MidiFdSet> {
            Ok(MidiFdSet {
                generation,
                ..MidiFdSet::EMPTY
            })
        }
    }

    #[tokio::test]
    async fn reconcile_and_select_ports_propagate_match_rules_to_app_state() {
        let temp_dir =
            std::env::temp_dir().join(format!("tactus-control-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).expect("create temp test dir");
        let cfg_path = temp_dir.join("config.toml");

        let config = TactusConfig::default();
        let initial_state = AppState {
            midi_input_match: config.midi.input_port_match.clone(),
            midi_output_match: config.midi.output_port_match.clone(),
            ..AppState::default()
        };

        let backend = Arc::new(FakeMidiBackend::new(Vec::new()));
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (rt_cmd_tx, mut rt_cmd_rx) = RingBuffer::new(16);
        let (_tel_tx, tel_rx) = RingBuffer::new(16);
        let (_fd_ret_tx, fd_ret_rx) = RingBuffer::new(16);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");

        let mut hub = StateHub {
            config,
            persister: ConfigPersister::spawn(cfg_path),
            backend: Arc::clone(&backend) as Arc<dyn MidiBackend>,
            rt_cmd_tx,
            tel_rx,
            fd_ret_rx,
            wake_tx,
            state_tx,
            attached_input_id: None,
            attached_output_id: None,
            fd_generation: 0,
            last_logged_snap: None,
            last_ports: Vec::new(),
            ports_logged_once: false,
        };

        // Empty device list preserves saved match rules while selected IDs stay None.
        hub.reconcile_midi(false).await;
        {
            let state = state_rx.borrow();
            assert_eq!(state.midi_input_match, "WIDI");
            assert_eq!(state.midi_output_match, "WIDI");
            assert_eq!(state.selected_midi_input, None);
            assert_eq!(state.selected_midi_output, None);
        }

        // Plugging in matching WIDI port selects it.
        backend.set_ports(vec![MidiPortInfo {
            id: "hw:1,0,0".to_string(),
            card_name: "WIDI Uhost".to_string(),
            port_name: "WIDI Uhost MIDI 1".to_string(),
            can_read: true,
            can_write: true,
        }]);
        hub.reconcile_midi(false).await;
        while rt_cmd_rx.pop().is_ok() {}
        {
            let state = state_rx.borrow();
            assert_eq!(state.selected_midi_input.as_deref(), Some("hw:1,0,0"));
            assert_eq!(state.selected_midi_output.as_deref(), Some("hw:1,0,0"));
            assert_eq!(state.midi_input_match, "WIDI");
            assert_eq!(state.midi_output_match, "WIDI");
        }

        // Changing rules updates persisted matches immediately even if new rule has no hardware match.
        hub.handle_command(EngineCommand::SelectMidiPorts {
            input: "Rev2".to_string(),
            output: String::new(),
        })
        .await;
        while rt_cmd_rx.pop().is_ok() {}
        {
            let state = state_rx.borrow();
            assert_eq!(state.midi_input_match, "Rev2");
            assert_eq!(state.midi_output_match, "");
            assert_eq!(state.selected_midi_input, None);
            assert_eq!(state.selected_midi_output, None);
        }

        drop(wake_rx);
        hub.persister.shutdown().await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
