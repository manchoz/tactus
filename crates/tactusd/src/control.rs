//! Async `tokio` control plane: state hub, MIDI hotplug coordinator, FD reaper, web UI, and config sync.

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer};
use tactus_protocol::{
    normalize_output_matches, AppState, EngineCommand, MidiPortInfo, RtCommand, TactusConfig,
    TelemetrySnapshot,
};
use tactus_sync_core::{MAX_VALID_BPM, MIN_VALID_BPM};
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::config_store::ConfigPersister;
use crate::log::{log_error, log_info, log_warn};
use crate::mcu;
use crate::midi::hotplug::UeventMonitor;
use crate::midi::{close_fd_set, match_output_ports, match_port, MidiBackend, MidiFdSet};
use crate::rt::{FdReturn, RtMessage, WakeSender};
use crate::rt_os::{AsyncWakeReceiver, WakeReceiver};
use crate::web;

#[cfg(target_os = "linux")]
const TELEMETRY_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(not(target_os = "linux"))]
const TELEMETRY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const HOTPLUG_RESCAN_FALLBACK: Duration = Duration::from_secs(5);
const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(150);
const WEB_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const RT_DETACH_POLL_INTERVAL: Duration = Duration::from_millis(2);
const RT_DETACH_MAX_POLLS: usize = 50;

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
    pub rt_notify_rx: WakeReceiver,
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
    attached_input: Option<(String, RawFd)>,
    attached_outputs: Vec<(String, RawFd)>,
    fd_generation: u64,
    last_open_attempt: Option<(Option<String>, Vec<String>)>,
    last_logged_snap: Option<TelemetrySnapshot>,
    last_ports: Vec<MidiPortInfo>,
    ports_logged_once: bool,
}

impl StateHub {
    fn publish_midi_state(&self, ports: Option<Vec<MidiPortInfo>>) {
        let attached_in = self.attached_input.as_ref().map(|(id, _)| id.clone());
        let attached_outs: Vec<String> = self
            .attached_outputs
            .iter()
            .map(|(id, _)| id.clone())
            .collect();

        self.state_tx.send_if_modified(|state| {
            let ports_changed = ports
                .as_ref()
                .is_some_and(|next| state.available_midi_ports != *next);
            let changed = ports_changed
                || state.selected_midi_input != attached_in
                || state.selected_midi_outputs != attached_outs
                || state.midi_input_match != self.config.midi.input_port_match
                || state.midi_output_matches != self.config.midi.output_port_matches;
            if changed {
                if let Some(next_ports) = ports {
                    state.available_midi_ports = next_ports;
                }
                state.selected_midi_input = attached_in;
                state.selected_midi_outputs = attached_outs;
                state
                    .midi_input_match
                    .clone_from(&self.config.midi.input_port_match);
                state
                    .midi_output_matches
                    .clone_from(&self.config.midi.output_port_matches);
            }
            changed
        });
    }

    fn handle_replaced(&mut self, fds: MidiFdSet) -> bool {
        let gen_id = fds.generation;
        close_fd_set(fds);
        if gen_id == self.fd_generation
            && (self.attached_input.is_some() || !self.attached_outputs.is_empty())
        {
            self.attached_input = None;
            self.attached_outputs.clear();
            self.last_open_attempt = None;
            return true;
        }
        false
    }

    async fn detach_active_midi_and_close(&mut self) -> bool {
        if self.attached_input.is_none() && self.attached_outputs.is_empty() {
            return true;
        }

        let prev_gen = self.fd_generation;
        if self
            .rt_cmd_tx
            .push(RtMessage::AttachMidi(MidiFdSet::EMPTY))
            .is_err()
        {
            log_warn!("RT command ring full while detaching MIDI FDs; will retry");
            return false;
        }
        self.wake_tx.wake();

        for _ in 0..RT_DETACH_MAX_POLLS {
            while let Ok(ret) = self.fd_ret_rx.pop() {
                match ret {
                    FdReturn::Replaced(fds) => {
                        let gen_id = fds.generation;
                        let _ = self.handle_replaced(fds);
                        if gen_id == prev_gen {
                            return true;
                        }
                    }
                    FdReturn::DetachedOnError { fds, errno } => {
                        let _ = self.handle_detached_on_error(fds, errno);
                        if self.attached_input.is_none() && self.attached_outputs.is_empty() {
                            return true;
                        }
                    }
                }
            }
            time::sleep(RT_DETACH_POLL_INTERVAL).await;
        }

        log_warn!(
            "timed out waiting for RT thread to release MIDI FDs (gen={prev_gen}); deferring reopen"
        );
        false
    }

    async fn reconcile_midi(&mut self) {
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
            self.last_open_attempt = None;
        }

        let matched_in =
            match_port(&ports, &self.config.midi.input_port_match, true, false).cloned();
        let matched_outs = match_output_ports(&ports, &self.config.midi.output_port_matches);

        let new_in_id = matched_in.as_ref().map(|p| p.id.clone());
        let new_out_ids: Vec<String> = matched_outs.iter().map(|p| p.id.clone()).collect();
        let cur_in_id = self.attached_input.as_ref().map(|(id, _)| id.clone());
        let cur_out_ids: Vec<String> = self
            .attached_outputs
            .iter()
            .map(|(id, _)| id.clone())
            .collect();

        let selection_changed = new_in_id != cur_in_id || new_out_ids != cur_out_ids;
        if !selection_changed {
            self.last_open_attempt = None;
            self.publish_midi_state(Some(ports));
            return;
        }

        let desired = (new_in_id.clone(), new_out_ids.clone());
        let has_active_ports = self.attached_input.is_some() || !self.attached_outputs.is_empty();
        if has_active_ports && self.last_open_attempt.as_ref() == Some(&desired) {
            self.publish_midi_state(Some(ports));
            return;
        }

        if !self.detach_active_midi_and_close().await {
            self.publish_midi_state(Some(ports));
            return;
        }

        if matched_in.is_none() && matched_outs.is_empty() {
            self.last_open_attempt = None;
            self.publish_midi_state(Some(ports));
            log_info!(
                "MIDI endpoints detached (match input={:?}, outputs={:?})",
                self.config.midi.input_port_match,
                self.config.midi.output_port_matches
            );
            return;
        }

        self.last_open_attempt = Some(desired);

        let next_gen = self.fd_generation.wrapping_add(1);
        let backend = Arc::clone(&self.backend);
        let in_clone = matched_in.clone();
        let outs_clone = matched_outs.clone();

        let opened = match tokio::task::spawn_blocking(move || {
            backend.open_ports(in_clone.as_ref(), &outs_clone, next_gen)
        })
        .await
        {
            Ok(Ok(opened)) => opened,
            Ok(Err(err)) => {
                log_warn!(
                    "failed to open MIDI ports (in={:?}, outs={:?}): {err}",
                    new_in_id,
                    new_out_ids
                );
                self.publish_midi_state(Some(ports));
                return;
            }
            Err(err) => {
                log_error!("MIDI open task panicked: {err}");
                self.publish_midi_state(Some(ports));
                return;
            }
        };

        if opened.fds.is_empty() {
            self.publish_midi_state(Some(ports));
            log_warn!(
                "no requested MIDI endpoints could be opened (wanted in={:?}, outs={:?})",
                new_in_id,
                new_out_ids
            );
            return;
        }

        if self
            .rt_cmd_tx
            .push(RtMessage::AttachMidi(opened.fds))
            .is_err()
        {
            close_fd_set(opened.fds);
            log_warn!("RT command ring full while attaching MIDI FDs; will retry");
            self.publish_midi_state(Some(ports));
            return;
        }

        self.fd_generation = next_gen;
        self.attached_input = opened
            .attached_input_id
            .as_ref()
            .filter(|_| opened.fds.in_fd >= 0)
            .map(|id| (id.clone(), opened.fds.in_fd));
        self.attached_outputs = opened
            .attached_output_ids
            .iter()
            .zip(opened.fds.active_outputs().iter().copied())
            .map(|(id, fd)| (id.clone(), fd))
            .collect();
        self.wake_tx.wake();
        self.publish_midi_state(Some(ports));

        let shared_duplex =
            opened.fds.in_fd >= 0 && opened.fds.active_outputs().contains(&opened.fds.in_fd);
        log_info!(
            "attached RawMIDI endpoints (gen={next_gen}, in={:?}, outs={:?}, shared_duplex={shared_duplex})",
            self.attached_input.as_ref().map(|(id, _)| id.as_str()),
            opened.attached_output_ids
        );
    }

    async fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SelectMidiPorts { input, outputs } => {
                self.config.midi.input_port_match = input.trim().to_string();
                self.config.midi.output_port_matches = normalize_output_matches(outputs);
                self.last_open_attempt = None;
                self.publish_midi_state(None);
                self.persister.schedule_save(self.config.clone());
                self.reconcile_midi().await;
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
                    RtCommand::SetClockThru(enabled) => {
                        self.config.sync.clock_thru = enabled;
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
                        if self.config.sync.direction == tactus_protocol::Direction::LinkToMidi {
                            self.config.sync.initial_bpm = bpm;
                            persist = true;
                        }
                    }
                    RtCommand::SetLinkPlaying(_) => {}
                    RtCommand::SetRoundBpm(enabled) => {
                        self.config.sync.round_bpm = enabled;
                        persist = true;
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

    fn handle_detached_on_error(&mut self, fds: MidiFdSet, errno: i32) -> bool {
        let mut out_list = [-1 as RawFd; tactus_protocol::MAX_MIDI_OUTPUTS];
        let out_slice = fds.active_outputs();
        out_list[..out_slice.len()].copy_from_slice(out_slice);
        let out_len = out_slice.len();
        let in_fd = fds.in_fd;
        let gen_id = fds.generation;

        close_fd_set(fds);

        let mut changed = false;
        if gen_id == self.fd_generation {
            if in_fd >= 0
                && self
                    .attached_input
                    .as_ref()
                    .is_some_and(|(_, fd)| *fd == in_fd)
            {
                self.attached_input = None;
                changed = true;
            }
            if out_len > 0 {
                let detached_outs = &out_list[..out_len];
                let before = self.attached_outputs.len();
                self.attached_outputs
                    .retain(|(_, fd)| !detached_outs.contains(fd));
                if self.attached_outputs.len() != before {
                    changed = true;
                }
            }
            if changed {
                self.last_open_attempt = None;
            }
        }

        let os_err = std::io::Error::from_raw_os_error(errno);
        log_warn!(
            "RT thread detached MIDI FD (gen={gen_id}, in_fd={in_fd}, out_fds={:?}, reason={os_err})",
            &out_list[..out_len]
        );
        changed
    }

    fn drain_rt_returns(&mut self) -> bool {
        let mut needs_reconcile = false;
        while let Ok(ret) = self.fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) => {
                    if self.handle_replaced(fds) {
                        needs_reconcile = true;
                    }
                }
                FdReturn::DetachedOnError { fds, errno } => {
                    if self.handle_detached_on_error(fds, errno) {
                        needs_reconcile = true;
                    }
                }
            }
        }
        if needs_reconcile {
            self.publish_midi_state(None);
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
                    || prev.clock_thru != snap.clock_thru
                    || prev.round_bpm != snap.round_bpm
                    || (prev.bpm_link - snap.bpm_link).abs() >= 0.05
            }
        };

        if discrete_changed {
            log_info!(
                "sync state: dir={:?}, lock={:?}, playing={}, peers={}, bpm_link={:.2}, start_stop={}, clock_thru={}, round_bpm={}",
                snap.direction,
                snap.lock_state,
                snap.is_playing,
                snap.link_peers,
                snap.bpm_link,
                snap.start_stop_sync,
                snap.clock_thru,
                snap.round_bpm
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
            clock_thru: args.config.sync.clock_thru,
            bpm_link: args.config.sync.initial_bpm,
            quantum: args.config.sync.quantum,
            midi_offset_ms: args.config.sync.midi_offset_ms,
            round_bpm: args.config.sync.round_bpm,
            ..TelemetrySnapshot::default()
        },
        midi_input_match: args.config.midi.input_port_match.clone(),
        midi_output_matches: args.config.midi.output_port_matches.clone(),
        ..AppState::default()
    };

    let (state_tx, state_rx) = watch::channel(initial_state);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(64);
    let control_handle = ControlHandle { state_rx, cmd_tx };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let web_handle = web::spawn_web_server(
        &args.config.web.listen_addr,
        control_handle.clone(),
        shutdown_rx.clone(),
    )
    .await;
    let mcu_handle = mcu::spawn_mcu_bridge(&args.config.mcu_bridge, control_handle, shutdown_rx);

    let uevent = match UeventMonitor::new() {
        Ok(mon) => Some(mon),
        Err(err) => {
            log_warn!("NETLINK_KOBJECT_UEVENT unavailable ({err}); relying on 5s periodic rescan");
            None
        }
    };
    let rt_notify = match AsyncWakeReceiver::new(args.rt_notify_rx) {
        Ok(rx) => Some(rx),
        Err(err) => {
            log_warn!(
                "RT notification wakeup registration failed ({err}); relying on periodic poll"
            );
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
        attached_input: None,
        attached_outputs: Vec::new(),
        fd_generation: 0,
        last_open_attempt: None,
        last_logged_snap: None,
        last_ports: Vec::new(),
        ports_logged_once: false,
    };

    hub.reconcile_midi().await;

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
            res = wait_rt_notify_opt(rt_notify.as_ref()) => {
                if let Err(err) = res {
                    log_warn!("RT notification wait failed: {err}");
                }
                if hub.drain_rt_returns() {
                    hotplug_deadline = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                }
                hub.drain_telemetry();
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
                hub.reconcile_midi().await;
            }
            _ = rescan_tick.tick() => {
                hub.reconcile_midi().await;
            }
            _ = telemetry_tick.tick() => {
                if let Some(ref notify) = rt_notify {
                    notify.drain_fallback();
                }
                if hub.drain_rt_returns() {
                    hotplug_deadline = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                }
                hub.drain_telemetry();
            }
        }
    }

    let _ = shutdown_tx.send(true);
    let stop_web = async {
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
    };
    let stop_mcu = async {
        if let Some(handle) = mcu_handle {
            if tokio::time::timeout(WEB_SHUTDOWN_TIMEOUT, handle)
                .await
                .is_err()
            {
                log_warn!(
                    "MCU bridge task did not stop within {:?}; continuing shutdown",
                    WEB_SHUTDOWN_TIMEOUT
                );
            }
        }
    };
    tokio::join!(stop_web, stop_mcu);

    hub.persister.shutdown().await;

    let _ = hub.drain_rt_returns();
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

async fn wait_rt_notify_opt(notify: Option<&AsyncWakeReceiver>) -> std::io::Result<()> {
    if let Some(rx) = notify {
        rx.wait().await
    } else {
        std::future::pending().await
    }
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
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Mutex;

    use rtrb::RingBuffer;

    use super::*;
    use crate::midi::{MidiFdSet, OpenedMidiPorts};
    use crate::rt::create_rt_wake_pair;

    struct ExclusiveFakeMidiBackend {
        ports: Mutex<Vec<MidiPortInfo>>,
        open_leases: Mutex<Vec<(String, OwnedFd)>>,
        failing_port_ids: Mutex<Vec<String>>,
    }

    impl ExclusiveFakeMidiBackend {
        fn new(ports: Vec<MidiPortInfo>) -> Self {
            Self {
                ports: Mutex::new(ports),
                open_leases: Mutex::new(Vec::new()),
                failing_port_ids: Mutex::new(Vec::new()),
            }
        }

        fn set_ports(&self, ports: Vec<MidiPortInfo>) {
            *self.ports.lock().expect("lock fake ports") = ports;
        }

        fn set_failing_port_ids(&self, ids: Vec<String>) {
            *self.failing_port_ids.lock().expect("lock failing port ids") = ids;
        }

        fn alloc_open_fd() -> (RawFd, OwnedFd) {
            let mut fds = [-1 as RawFd; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            (fds[0], write_end)
        }

        fn prune_closed(leases: &mut Vec<(String, OwnedFd)>) {
            leases.retain(|(_, write_fd)| {
                let mut pfd = libc::pollfd {
                    fd: write_fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
                ret >= 0 && (pfd.revents & (libc::POLLERR | libc::POLLHUP)) == 0
            });
        }
    }

    impl MidiBackend for ExclusiveFakeMidiBackend {
        fn enumerate_ports(&self) -> std::io::Result<Vec<MidiPortInfo>> {
            Ok(self.ports.lock().expect("lock fake ports").clone())
        }

        fn open_ports(
            &self,
            input: Option<&MidiPortInfo>,
            outputs: &[MidiPortInfo],
            generation: u64,
        ) -> std::io::Result<OpenedMidiPorts> {
            let mut leases = self.open_leases.lock().expect("lock leases");
            Self::prune_closed(&mut leases);
            let failing = self
                .failing_port_ids
                .lock()
                .expect("lock failing port ids")
                .clone();

            // Simulate ALSA RawMIDI EBUSY if a requested device is still held open by RT.
            if let Some(in_port) = input {
                if !failing.contains(&in_port.id) && leases.iter().any(|(id, _)| id == &in_port.id)
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
                }
            }
            for out_port in outputs {
                if !failing.contains(&out_port.id)
                    && leases.iter().any(|(id, _)| id == &out_port.id)
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
                }
            }

            let mut fds = MidiFdSet {
                generation,
                ..MidiFdSet::EMPTY
            };
            let mut attached_input_id = None;
            let mut attached_output_ids = Vec::new();
            let mut shared_id_fd: Option<(String, RawFd)> = None;

            if let Some(in_port) = input {
                if !failing.contains(&in_port.id) {
                    let (fd, write_fd) = Self::alloc_open_fd();
                    fds.in_fd = fd;
                    attached_input_id = Some(in_port.id.clone());
                    leases.push((in_port.id.clone(), write_fd));
                    if outputs.iter().any(|o| o.id == in_port.id) {
                        shared_id_fd = Some((in_port.id.clone(), fd));
                    }
                }
            }

            for out_port in outputs {
                if failing.contains(&out_port.id) {
                    continue;
                }
                if input.is_some_and(|in_port| in_port.id == out_port.id) && shared_id_fd.is_none()
                {
                    continue;
                }
                if let Some((ref shared_id, shared_fd)) = shared_id_fd {
                    if *shared_id == out_port.id {
                        if fds.push_output(shared_fd) {
                            attached_output_ids.push(out_port.id.clone());
                        }
                        continue;
                    }
                }
                let (fd, write_fd) = Self::alloc_open_fd();
                if fds.push_output(fd) {
                    attached_output_ids.push(out_port.id.clone());
                    leases.push((out_port.id.clone(), write_fd));
                } else {
                    unsafe {
                        libc::close(fd);
                    }
                }
            }

            Ok(OpenedMidiPorts {
                fds,
                attached_input_id,
                attached_output_ids,
            })
        }
    }

    fn sample_port(id: &str, card_name: &str, port_name: &str) -> MidiPortInfo {
        MidiPortInfo {
            id: id.to_string(),
            card_name: card_name.to_string(),
            port_name: port_name.to_string(),
            can_read: true,
            can_write: true,
        }
    }

    struct ControlTestRig {
        temp_dir: PathBuf,
        backend: Arc<ExclusiveFakeMidiBackend>,
        state_rx: watch::Receiver<AppState>,
        sim_unplug_tx: std::sync::mpsc::Sender<RawFd>,
        hub: StateHub,
        wake_rx: Option<crate::rt_os::WakeReceiver>,
        rt_sim: Option<JoinHandle<()>>,
    }

    impl ControlTestRig {
        fn spawn(name: &str, config: TactusConfig, initial_ports: Vec<MidiPortInfo>) -> Self {
            let temp_dir = std::env::temp_dir()
                .join(format!("tactus-control-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::fs::create_dir_all(&temp_dir).expect("create temp test dir");
            let cfg_path = temp_dir.join("config.toml");

            let initial_state = AppState {
                midi_input_match: config.midi.input_port_match.clone(),
                midi_output_matches: config.midi.output_port_matches.clone(),
                ..AppState::default()
            };

            let backend = Arc::new(ExclusiveFakeMidiBackend::new(initial_ports));
            let (state_tx, state_rx) = watch::channel(initial_state);
            let (rt_cmd_tx, mut rt_cmd_rx) = RingBuffer::new(32);
            let (_tel_tx, tel_rx) = RingBuffer::new(16);
            let (mut fd_ret_tx, fd_ret_rx) = RingBuffer::new(32);
            let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");

            let (sim_unplug_tx, sim_unplug_rx) = std::sync::mpsc::channel::<RawFd>();
            let rt_sim = std::thread::spawn(move || {
                let mut active = MidiFdSet::EMPTY;
                loop {
                    while let Ok(msg) = rt_cmd_rx.pop() {
                        match msg {
                            RtMessage::AttachMidi(new_fds) => {
                                if !active.is_empty() {
                                    let _ = fd_ret_tx.push(FdReturn::Replaced(active));
                                }
                                active = new_fds;
                            }
                            RtMessage::Shutdown => {
                                if !active.is_empty() {
                                    let _ = fd_ret_tx.push(FdReturn::Replaced(active));
                                }
                                return;
                            }
                            RtMessage::Command(_) => {}
                        }
                    }
                    while let Ok(unplug_fd) = sim_unplug_rx.try_recv() {
                        let mut detached = MidiFdSet {
                            generation: active.generation,
                            ..MidiFdSet::EMPTY
                        };
                        if active.in_fd == unplug_fd {
                            active.in_fd = -1;
                            detached.in_fd = unplug_fd;
                        }
                        let mut i = 0;
                        while i < active.out_count as usize {
                            if active.out_fds[i] == unplug_fd {
                                let _ = active.remove_output_at(i);
                                detached.push_output(unplug_fd);
                            } else {
                                i += 1;
                            }
                        }
                        if !detached.is_empty() {
                            let _ = fd_ret_tx.push(FdReturn::DetachedOnError {
                                fds: detached,
                                errno: libc::ENODEV,
                            });
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            });

            let hub = StateHub {
                config,
                persister: ConfigPersister::spawn(cfg_path),
                backend: Arc::clone(&backend) as Arc<dyn MidiBackend>,
                rt_cmd_tx,
                tel_rx,
                fd_ret_rx,
                wake_tx,
                state_tx,
                attached_input: None,
                attached_outputs: Vec::new(),
                fd_generation: 0,
                last_open_attempt: None,
                last_logged_snap: None,
                last_ports: Vec::new(),
                ports_logged_once: false,
            };

            Self {
                temp_dir,
                backend,
                state_rx,
                sim_unplug_tx,
                hub,
                wake_rx: Some(wake_rx),
                rt_sim: Some(rt_sim),
            }
        }

        async fn shutdown(mut self) {
            let _ = self.hub.rt_cmd_tx.push(RtMessage::Shutdown);
            self.hub.wake_tx.wake();
            if let Some(rt_sim) = self.rt_sim.take() {
                rt_sim.join().expect("mock RT responder joins");
            }
            let _ = self.hub.drain_rt_returns();
            drop(self.wake_rx.take());
            self.hub.persister.shutdown().await;
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }

    #[tokio::test]
    async fn reconcile_and_select_ports_avoid_ebusy_and_isolate_unplug() {
        let mut rig = ControlTestRig::spawn("ebusy", TactusConfig::default(), Vec::new());

        // 1. Empty device list preserves saved match rules while selected IDs stay empty.
        rig.hub.reconcile_midi().await;
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.midi_input_match, "WIDI");
            assert_eq!(state.midi_output_matches, vec!["WIDI".to_string()]);
            assert_eq!(state.selected_midi_input, None);
            assert!(state.selected_midi_outputs.is_empty());
        }

        // 2. Plugging in WIDI + OP-XY attaches duplex WIDI initially.
        let widi = sample_port("hw:1,0,0", "WIDI Uhost", "WIDI Uhost MIDI 1");
        let opxy = sample_port("hw:2,0,0", "OP-XY", "OP-XY MIDI 1");
        rig.backend.set_ports(vec![widi.clone(), opxy.clone()]);
        rig.hub.reconcile_midi().await;
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.selected_midi_input.as_deref(), Some("hw:1,0,0"));
            assert_eq!(state.selected_midi_outputs, vec!["hw:1,0,0".to_string()]);
        }

        // 3. Switching from duplex WIDI -> input OP-XY + outputs [WIDI, OP-XY] MUST detach/close
        //    WIDI before reopening so ExclusiveFakeMidiBackend never hits EBUSY.
        rig.hub
            .handle_command(EngineCommand::SelectMidiPorts {
                input: "OP-XY MIDI 1".to_string(),
                outputs: vec!["WIDI Uhost MIDI 1".to_string(), "OP-XY MIDI 1".to_string()],
            })
            .await;
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.midi_input_match, "OP-XY MIDI 1");
            assert_eq!(
                state.midi_output_matches,
                vec!["WIDI Uhost MIDI 1".to_string(), "OP-XY MIDI 1".to_string()]
            );
            assert_eq!(state.selected_midi_input.as_deref(), Some("hw:2,0,0"));
            assert_eq!(
                state.selected_midi_outputs,
                vec!["hw:1,0,0".to_string(), "hw:2,0,0".to_string()]
            );
        }

        // 4. Unplugging OP-XY while keeping WIDI: RT detaches OP-XY only,
        //    and reconcile_midi() preserves WIDI without reopening.
        let opxy_fd = rig.hub.attached_input.as_ref().expect("OP-XY attached").1;
        let widi_fd = rig.hub.attached_outputs[0].1;
        rig.sim_unplug_tx.send(opxy_fd).expect("send sim unplug");
        let mut saw_detach = false;
        for _ in 0..50 {
            if rig.hub.drain_rt_returns() {
                saw_detach = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(saw_detach, "expected OP-XY unplug detach");
        rig.backend.set_ports(vec![widi]);
        let gen_before = rig.hub.fd_generation;
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, gen_before);
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.selected_midi_input, None);
            assert_eq!(state.selected_midi_outputs, vec!["hw:1,0,0".to_string()]);
        }
        assert_ne!(unsafe { libc::fcntl(widi_fd, libc::F_GETFD) }, -1);

        // 5. Missing rule preserves configured rules with empty attached IDs.
        rig.hub
            .handle_command(EngineCommand::SelectMidiPorts {
                input: "Rev2".to_string(),
                outputs: Vec::new(),
            })
            .await;
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.midi_input_match, "Rev2");
            assert!(state.midi_output_matches.is_empty());
            assert_eq!(state.selected_midi_input, None);
            assert!(state.selected_midi_outputs.is_empty());
        }

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn removing_unplugged_output_rule_preserves_attached_ports() {
        let mut config = TactusConfig::default();
        config.midi.output_port_matches = vec!["WIDI".to_string(), "Rev2".to_string()];

        let widi = sample_port("hw:1,0,0", "WIDI Uhost", "WIDI Uhost MIDI 1");
        let mut rig = ControlTestRig::spawn("unplugged-remove", config, vec![widi]);

        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 1);
        let widi_fd = rig.hub.attached_outputs[0].1;

        rig.hub
            .handle_command(EngineCommand::SelectMidiPorts {
                input: "WIDI".to_string(),
                outputs: vec!["WIDI".to_string()],
            })
            .await;

        assert_eq!(
            rig.hub.fd_generation, 1,
            "removing an unplugged rule must not detach/reopen live ports"
        );
        assert_ne!(unsafe { libc::fcntl(widi_fd, libc::F_GETFD) }, -1);
        assert_eq!(
            rig.state_rx.borrow().midi_output_matches,
            vec!["WIDI".to_string()]
        );

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn partial_open_failure_skips_redundant_reopen_until_retry_or_hotplug() {
        let mut config = TactusConfig::default();
        config.midi.output_port_matches = vec!["WIDI".to_string(), "Broken".to_string()];

        let widi = sample_port("hw:1,0,0", "WIDI Uhost", "WIDI Uhost MIDI 1");
        let broken = sample_port("hw:2,0,0", "Broken Synth", "Broken Synth MIDI 1");
        let extra = sample_port("hw:3,0,0", "Daisy Seed", "Daisy Seed MIDI 1");
        let mut rig =
            ControlTestRig::spawn("partial-open", config, vec![widi.clone(), broken.clone()]);

        rig.backend
            .set_failing_port_ids(vec!["hw:2,0,0".to_string()]);
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 1);
        let widi_fd = rig.hub.attached_outputs[0].1;
        assert_eq!(
            rig.state_rx.borrow().selected_midi_outputs,
            vec!["hw:1,0,0".to_string()]
        );

        // Periodic fallback rescan with unchanged ports must not tear down healthy WIDI.
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 1);
        assert_ne!(unsafe { libc::fcntl(widi_fd, libc::F_GETFD) }, -1);

        // Hotplug port-list change clears last_open_attempt and retries once Broken recovers.
        rig.backend.set_failing_port_ids(Vec::new());
        rig.backend
            .set_ports(vec![widi.clone(), broken.clone(), extra]);
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 2);
        assert_eq!(
            rig.state_rx.borrow().selected_midi_outputs,
            vec!["hw:1,0,0".to_string(), "hw:2,0,0".to_string()]
        );

        // Unplug Broken, then replug it while failing: partial open attaches WIDI at gen=4,
        // periodic rescan skips reopen, and SelectMidiPorts clears last_open_attempt for retry.
        rig.backend.set_ports(vec![widi.clone()]);
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 3);

        rig.backend.set_ports(vec![widi, broken]);
        rig.backend
            .set_failing_port_ids(vec!["hw:2,0,0".to_string()]);
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 4);
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 4);

        rig.backend.set_failing_port_ids(Vec::new());
        rig.hub
            .handle_command(EngineCommand::SelectMidiPorts {
                input: "WIDI".to_string(),
                outputs: vec!["WIDI".to_string(), "Broken".to_string()],
            })
            .await;
        assert_eq!(rig.hub.fd_generation, 5);
        assert_eq!(
            rig.state_rx.borrow().selected_midi_outputs,
            vec!["hw:1,0,0".to_string(), "hw:2,0,0".to_string()]
        );

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn late_replaced_return_clears_attached_state_and_unblocks_reconcile() {
        let widi = sample_port("hw:1,0,0", "WIDI Uhost", "WIDI Uhost MIDI 1");
        let mut rig = ControlTestRig::spawn("late-replaced", TactusConfig::default(), vec![widi]);

        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 1);
        assert!(rig.hub.attached_input.is_some());

        // Emulate a detach that timed out in StateHub: RT swaps in EMPTY and emits Replaced later.
        rig.hub
            .rt_cmd_tx
            .push(RtMessage::AttachMidi(MidiFdSet::EMPTY))
            .unwrap();
        rig.hub.wake_tx.wake();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(rig.hub.attached_input.is_some());
        assert!(rig.hub.drain_rt_returns());
        assert!(rig.hub.attached_input.is_none());
        assert!(rig.hub.attached_outputs.is_empty());
        {
            let state = rig.state_rx.borrow();
            assert_eq!(state.selected_midi_input, None);
            assert!(state.selected_midi_outputs.is_empty());
        }

        // Subsequent reconcile immediately attaches at gen=2 instead of deadlocking on detach.
        rig.hub.reconcile_midi().await;
        assert_eq!(rig.hub.fd_generation, 2);
        assert_eq!(
            rig.state_rx.borrow().selected_midi_input.as_deref(),
            Some("hw:1,0,0")
        );

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn persists_initial_bpm_and_round_bpm_without_persisting_link_playing() {
        let mut rig = ControlTestRig::spawn("persist-sync-flags", TactusConfig::default(), vec![]);
        let cfg_path = rig.temp_dir.join("config.toml");

        // SetLinkPlaying alone must forward to RT without creating/saving config.toml.
        rig.hub
            .handle_command(EngineCommand::SetLinkPlaying(true))
            .await;
        rig.hub.persister.shutdown().await;
        assert!(!cfg_path.exists());
        rig.shutdown().await;

        let mut rig2 =
            ControlTestRig::spawn("persist-sync-flags-2", TactusConfig::default(), vec![]);
        let cfg_path2 = rig2.temp_dir.join("config.toml");

        rig2.hub
            .handle_command(EngineCommand::SetLinkTempoManual(134.5))
            .await;
        rig2.hub
            .handle_command(EngineCommand::SetRoundBpm(true))
            .await;
        assert_eq!(rig2.hub.config.sync.initial_bpm, 134.5);
        assert!(rig2.hub.config.sync.round_bpm);

        // SetLinkTempoManual while in MidiToLink must NOT overwrite persisted initial_bpm.
        rig2.hub
            .handle_command(EngineCommand::SetDirection(
                tactus_protocol::Direction::MidiToLink,
            ))
            .await;
        rig2.hub
            .handle_command(EngineCommand::SetLinkTempoManual(150.0))
            .await;
        assert_eq!(rig2.hub.config.sync.initial_bpm, 134.5);

        // Flush debounced config writes before `rig2.shutdown()` removes `temp_dir`.
        rig2.hub.persister.shutdown().await;

        let (reloaded, existed) =
            crate::config_store::load_config(&cfg_path2).expect("load flushed config.toml");
        assert!(existed);
        assert_eq!(reloaded.sync.initial_bpm, 134.5);
        assert!(reloaded.sync.round_bpm);

        rig2.shutdown().await;
    }
}
