//! Real-time sync engine thread (`SCHED_FIFO`, `mlockall`, Core 3, zero allocations).

use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use rtrb::{Consumer, Producer};
use tactus_link_sys::{AblLink, SessionState};
use tactus_protocol::{Direction, RtCommand, SyncConfig, TelemetrySnapshot};
use tactus_sync_core::{LinkSession, MidiEvent, MidiOutputBatch, SyncEngine};

use crate::midi::MidiFdSet;
use crate::rt_os::{create_wake_pair, harden_current_thread, RtHardeningReport, WakeReceiver};

/// Target CPU core for the real-time sync engine (`ARCHITECTURE.md §2`).
pub const RT_PREFERRED_CORE: usize = 3;

/// `SCHED_FIFO` real-time priority (`MILESTONES.md` M3).
pub const RT_SCHED_FIFO_PRIO: i32 = 75;

/// Target periodic telemetry cadence (`10 Hz` = `100 ms`; beats also push immediately).
pub const TELEMETRY_INTERVAL_NS: i64 = 100_000_000;

const MAX_MIDI_BATCH_BYTES: usize = 8;
const MAX_READ_CHUNKS_PER_WAKE: usize = 16;

/// Strictly `Copy` commands sent from the async control plane to the RT thread over `rtrb`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RtMessage {
    /// Sync engine configuration or transport command.
    Command(RtCommand),
    /// Swap in newly opened non-blocking MIDI file descriptors.
    AttachMidi(MidiFdSet),
    /// Flush clean transport stop (`0xFC`), return open descriptors, and terminate the RT loop.
    Shutdown,
}

/// Strictly `Copy` descriptor ownership returns from RT back to async for safe `close()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdReturn {
    /// Old descriptors displaced by a newer [`RtMessage::AttachMidi`] or [`RtMessage::Shutdown`].
    Replaced(MidiFdSet),
    /// Descriptor(s) detached immediately by RT after `POLLERR` / `POLLHUP` / `ENODEV` / EOF.
    DetachedOnError { fds: MidiFdSet, errno: i32 },
}

/// SPSC ring endpoints owned by the RT thread.
pub struct RtLoopChannels {
    pub cmd_rx: Consumer<RtMessage>,
    pub tel_tx: Producer<TelemetrySnapshot>,
    pub fd_ret_tx: Producer<FdReturn>,
    pub wake_rx: WakeReceiver,
    pub rt_notify_tx: WakeSender,
}

/// Production [`LinkSession`] backed by pre-allocated `abl_link` `SessionState`.
pub struct ProductionLinkSession {
    state: SessionState,
    num_peers: usize,
    dirty: bool,
}

impl ProductionLinkSession {
    fn new() -> Self {
        Self {
            state: SessionState::new(),
            num_peers: 0,
            dirty: false,
        }
    }
}

impl LinkSession for ProductionLinkSession {
    fn num_peers(&self) -> usize {
        self.num_peers
    }

    fn tempo(&self) -> f64 {
        self.state.tempo()
    }

    fn beat_at_time(&self, time_us: i64, quantum: f64) -> f64 {
        self.state.beat_at_time(time_us, quantum)
    }

    fn phase_at_time(&self, time_us: i64, quantum: f64) -> f64 {
        self.state.phase_at_time(time_us, quantum)
    }

    fn time_at_beat(&self, beat: f64, quantum: f64) -> i64 {
        self.state.time_at_beat(beat, quantum)
    }

    fn is_playing(&self) -> bool {
        self.state.is_playing()
    }

    fn set_tempo(&mut self, bpm: f64, at_time_us: i64) {
        self.state.set_tempo(bpm, at_time_us);
        self.dirty = true;
    }

    fn request_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64) {
        self.state.request_beat_at_time(beat, time_us, quantum);
        self.dirty = true;
    }

    fn force_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64) {
        self.state.force_beat_at_time(beat, time_us, quantum);
        self.dirty = true;
    }

    fn set_is_playing(&mut self, is_playing: bool, at_time_us: i64) {
        self.state.set_is_playing(is_playing, at_time_us);
        self.dirty = true;
    }

    fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time_us: i64,
        beat: f64,
        quantum: f64,
    ) {
        self.state
            .set_is_playing_and_request_beat_at_time(is_playing, time_us, beat, quantum);
        self.dirty = true;
    }
}

/// Link driver boundary allowing `run_rt_loop` to run identically against real `abl_link`
/// or deterministic in-process mock `LinkSession` instances.
pub trait RtLinkDriver {
    type Session: LinkSession;

    /// Read the Link monotonic clock in nanoseconds (`CLOCK_MONOTONIC_RAW` on Linux).
    fn now_link_ns(&self) -> i64;

    /// Mirror `start_stop_sync` enablement onto the Link peer.
    fn enable_start_stop_sync(&mut self, enabled: bool);

    /// Capture the current audio-thread session view (zero-alloc).
    fn capture(&mut self) -> &mut Self::Session;

    /// Commit modified session state iff mutated during this wakeup.
    fn commit_if_dirty(&mut self);
}

/// Real `abl_link` driver used by `tactusd` in production.
pub struct AblLinkDriver {
    link: AblLink,
    session: ProductionLinkSession,
}

impl AblLinkDriver {
    /// Construct and enable an `AblLink` instance initialized at `initial_bpm`.
    #[must_use]
    pub fn new(initial_bpm: f64, start_stop_sync: bool) -> Self {
        let link = AblLink::new(initial_bpm);
        link.enable_start_stop_sync(start_stop_sync);
        link.enable(true);
        Self {
            link,
            session: ProductionLinkSession::new(),
        }
    }
}

impl Drop for AblLinkDriver {
    fn drop(&mut self) {
        self.link.enable(false);
    }
}

impl RtLinkDriver for AblLinkDriver {
    type Session = ProductionLinkSession;

    fn now_link_ns(&self) -> i64 {
        #[cfg(target_os = "linux")]
        {
            crate::clock::now_raw_ns()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.link.clock_micros().saturating_mul(1_000)
        }
    }

    fn enable_start_stop_sync(&mut self, enabled: bool) {
        self.link.enable_start_stop_sync(enabled);
    }

    fn capture(&mut self) -> &mut Self::Session {
        self.link
            .capture_audio_session_state(&mut self.session.state);
        self.session.num_peers = self.link.num_peers() as usize;
        self.session.dirty = false;
        &mut self.session
    }

    fn commit_if_dirty(&mut self) {
        if self.session.dirty {
            self.link.commit_audio_session_state(&self.session.state);
            self.session.dirty = false;
        }
    }
}

/// Spawn the dedicated real-time sync thread and return its handle + hardening report.
pub fn spawn_rt_thread(
    config: SyncConfig,
    channels: RtLoopChannels,
) -> io::Result<(JoinHandle<()>, RtHardeningReport)> {
    let (report_tx, report_rx) = std_mpsc::sync_channel::<io::Result<RtHardeningReport>>(1);

    let handle = thread::Builder::new()
        .name("rt-sync".to_string())
        .spawn(move || {
            let mut driver = AblLinkDriver::new(config.initial_bpm, config.start_stop_sync);
            let mut engine = SyncEngine::new(config);

            #[cfg(target_os = "linux")]
            let timer = match crate::rt_os::TimerFd::new() {
                Ok(t) => Some(t),
                Err(err) => {
                    let _ = report_tx.send(Err(err));
                    return;
                }
            };
            #[cfg(not(target_os = "linux"))]
            let timer: Option<()> = None;

            // Harden only after Link spawns its background Asio thread so Link I/O stays SCHED_OTHER.
            let hardening = harden_current_thread(RT_PREFERRED_CORE, RT_SCHED_FIFO_PRIO);
            let _ = report_tx.send(Ok(hardening));
            run_rt_loop_with_timer(&mut engine, &mut driver, channels, timer);
        })?;

    let report = report_rx
        .recv()
        .map_err(|_| io::Error::other("rt-sync thread exited before reporting initialization"))??;
    Ok((handle, report))
}

/// Execute the zero-allocation RT loop (exposed for deterministic pipe/mock integration tests).
#[cfg(test)]
pub fn run_rt_loop<D: RtLinkDriver>(
    engine: &mut SyncEngine,
    driver: &mut D,
    channels: RtLoopChannels,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let timer = Some(crate::rt_os::TimerFd::new()?);
    #[cfg(not(target_os = "linux"))]
    let timer: Option<()> = None;

    run_rt_loop_with_timer(engine, driver, channels, timer);
    Ok(())
}

#[cfg(target_os = "linux")]
type PlatformTimer = Option<crate::rt_os::TimerFd>;
#[cfg(not(target_os = "linux"))]
type PlatformTimer = Option<()>;

struct FdReturnOutbox<'a> {
    tx: &'a mut Producer<FdReturn>,
    notify_tx: &'a WakeSender,
    pending: Option<FdReturn>,
}

impl<'a> FdReturnOutbox<'a> {
    fn new(tx: &'a mut Producer<FdReturn>, notify_tx: &'a WakeSender) -> Self {
        Self {
            tx,
            notify_tx,
            pending: None,
        }
    }

    const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn flush(&mut self) {
        if let Some(ret) = self.pending {
            if self.tx.push(ret).is_ok() {
                self.pending = None;
                self.notify_tx.wake();
            }
        }
    }

    fn push(&mut self, ret: FdReturn) {
        self.flush();
        if self.pending.is_none() {
            match self.tx.push(ret) {
                Ok(()) => self.notify_tx.wake(),
                Err(rtrb::PushError::Full(unsent)) => {
                    self.pending = Some(unsent);
                    self.notify_tx.wake();
                }
            }
            return;
        }

        if let (
            Some(FdReturn::DetachedOnError {
                fds: ref mut existing,
                ..
            }),
            FdReturn::DetachedOnError { fds: next, .. },
        ) = (&mut self.pending, ret)
        {
            if existing.generation == next.generation {
                if next.in_fd >= 0 && existing.in_fd < 0 {
                    existing.in_fd = next.in_fd;
                }
                for &fd in next.active_outputs() {
                    if !existing.active_outputs().contains(&fd) {
                        let _ = existing.push_output(fd);
                    }
                }
                self.notify_tx.wake();
            }
        }
    }
}

fn run_rt_loop_with_timer<D: RtLinkDriver>(
    engine: &mut SyncEngine,
    driver: &mut D,
    mut ch: RtLoopChannels,
    timer: PlatformTimer,
) {
    let mut active_fds = MidiFdSet::EMPTY;
    let mut prev_snap = engine.snapshot();
    let mut tel_initialized = false;
    let mut fd_outbox = FdReturnOutbox::new(&mut ch.fd_ret_tx, &ch.rt_notify_tx);

    let init_now_ns = driver.now_link_ns();
    let mut next_telemetry_ns = init_now_ns.saturating_add(TELEMETRY_INTERVAL_NS);

    #[cfg(target_os = "linux")]
    let mut bridge = {
        let mut b = crate::clock::ClockBridge::new();
        b.sample_os();
        b
    };

    let init_poll = {
        let session = driver.capture();
        engine.poll(init_now_ns, session)
    };
    let init_snap = engine.snapshot();
    driver.commit_if_dirty();
    write_midi_batch(&mut active_fds, &init_poll.midi_out, false, &mut fd_outbox);
    if ch.tel_tx.push(init_snap).is_ok() {
        prev_snap = init_snap;
        tel_initialized = true;
        ch.rt_notify_tx.wake();
    }
    let mut next_engine_deadline = init_poll.next_deadline_ns;
    let mut next_wakeup_raw_ns = min_deadline(next_engine_deadline, Some(next_telemetry_ns))
        .unwrap_or_else(|| init_now_ns.saturating_add(TELEMETRY_INTERVAL_NS));

    loop {
        fd_outbox.flush();

        let mut pfds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; 3];
        pfds[0] = libc::pollfd {
            fd: ch.wake_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        #[cfg(target_os = "linux")]
        let base_len = if let Some(ref tfd) = timer {
            pfds[1] = libc::pollfd {
                fd: tfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            2
        } else {
            1
        };
        #[cfg(not(target_os = "linux"))]
        let base_len = {
            let _ = &timer;
            1
        };

        let midi_idx = if active_fds.in_fd >= 0 {
            pfds[base_len] = libc::pollfd {
                fd: active_fds.in_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            Some(base_len)
        } else {
            None
        };
        let nfds = base_len + usize::from(midi_idx.is_some());

        #[cfg(target_os = "linux")]
        let poll_timeout_ms: libc::c_int = if let Some(ref tfd) = timer {
            let now_raw = driver.now_link_ns();
            let now_mono = crate::clock::now_mono_ns();
            bridge.maybe_refresh_os(now_raw, false);
            let mut target_mono = bridge.raw_to_mono_ns(next_wakeup_raw_ns);
            if target_mono <= now_mono {
                bridge.maybe_refresh_os(now_raw, true);
                target_mono = bridge.raw_to_mono_ns(next_wakeup_raw_ns);
            }
            if target_mono <= now_mono {
                0
            } else {
                let _ = tfd.arm_abstime_ns(target_mono);
                -1
            }
        } else {
            compute_poll_timeout_ms(driver.now_link_ns(), next_wakeup_raw_ns)
        };

        #[cfg(not(target_os = "linux"))]
        let poll_timeout_ms: libc::c_int =
            compute_poll_timeout_ms(driver.now_link_ns(), next_wakeup_raw_ns);

        let ready = unsafe { libc::poll(pfds.as_mut_ptr(), nfds as libc::nfds_t, poll_timeout_ms) };
        if ready < 0 {
            if last_errno() == libc::EINTR {
                continue;
            }
            break;
        }

        let now_raw_ns = driver.now_link_ns();

        #[cfg(target_os = "linux")]
        if let Some(ref tfd) = timer {
            if (pfds[1].revents & libc::POLLIN) != 0 {
                tfd.drain();
                // If CLOCK_MONOTONIC slewed forward enough to wake >200 us early in RAW, re-anchor.
                if is_early_timer_wakeup(now_raw_ns, next_wakeup_raw_ns) {
                    bridge.maybe_refresh_os(now_raw_ns, true);
                }
            }
        }

        if (pfds[0].revents & libc::POLLIN) != 0 {
            ch.wake_rx.drain();
        }

        let mut shutdown_requested = false;
        let mut pending_start_stop_sync: Option<bool> = None;

        {
            let session = driver.capture();

            // 1. Drain incoming MIDI bytes first to minimize input timestamp latency.
            if let Some(idx) = midi_idx {
                let revents = pfds[idx].revents;
                if revents != 0 {
                    let mut detach_err = None;
                    if (revents & libc::POLLIN) != 0 {
                        detach_err = read_midi_input(
                            &mut active_fds,
                            now_raw_ns,
                            engine,
                            session,
                            &mut fd_outbox,
                        );
                    }
                    if detach_err.is_none()
                        && (revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
                    {
                        detach_err = Some(if (revents & libc::POLLERR) != 0 {
                            libc::ENODEV
                        } else {
                            libc::EPIPE
                        });
                    }
                    if let Some(errno) = detach_err {
                        let ret = detach_input_fd(&mut active_fds, errno);
                        fd_outbox.push(ret);
                    }
                }
            }

            // 2. Drain non-blocking RT commands & FD hot-swaps.
            loop {
                if fd_outbox.has_pending()
                    && matches!(ch.cmd_rx.peek(), Ok(RtMessage::AttachMidi(_)))
                {
                    break;
                }
                let Ok(msg) = ch.cmd_rx.pop() else {
                    break;
                };
                match msg {
                    RtMessage::Command(cmd) => {
                        if let RtCommand::SetStartStopSync(enabled) = cmd {
                            pending_start_stop_sync = Some(enabled);
                        }
                        let exclude_input = matches!(cmd, RtCommand::SetClockThru(_))
                            || (matches!(cmd, RtCommand::SetDirection(Direction::LinkToMidi))
                                && engine.config().direction == Direction::MidiToLink);
                        let batch = engine.apply_command(cmd, now_raw_ns, session);
                        write_midi_batch(&mut active_fds, &batch, exclude_input, &mut fd_outbox);
                    }
                    RtMessage::AttachMidi(new_fds) => {
                        if !active_fds.is_empty() {
                            fd_outbox.push(FdReturn::Replaced(active_fds));
                        }
                        active_fds = new_fds;
                    }
                    RtMessage::Shutdown => {
                        shutdown_requested = true;
                        if engine.config().direction == Direction::LinkToMidi {
                            let batch = engine.apply_command(
                                RtCommand::SetDirection(Direction::MidiToLink),
                                now_raw_ns,
                                session,
                            );
                            write_midi_batch(&mut active_fds, &batch, false, &mut fd_outbox);
                        } else if engine.config().clock_thru {
                            let batch = engine.apply_command(
                                RtCommand::SetClockThru(false),
                                now_raw_ns,
                                session,
                            );
                            write_midi_batch(&mut active_fds, &batch, true, &mut fd_outbox);
                        }
                        break;
                    }
                }
            }

            // 3. Run scheduled 24 PPQN output or MIDI timeout check.
            if !shutdown_requested {
                let poll_res = engine.poll(now_raw_ns, session);
                next_engine_deadline = poll_res.next_deadline_ns;
                write_midi_batch(&mut active_fds, &poll_res.midi_out, false, &mut fd_outbox);
            }
        }

        let snap = engine.snapshot();
        driver.commit_if_dirty();
        if let Some(enabled) = pending_start_stop_sync.take() {
            driver.enable_start_stop_sync(enabled);
        }

        if shutdown_requested {
            if !active_fds.is_empty() {
                fd_outbox.push(FdReturn::Replaced(active_fds));
            }
            fd_outbox.flush();
            break;
        }

        let discrete_change = is_discrete_telemetry_change(tel_initialized, prev_snap, snap);
        let periodic_due = now_raw_ns >= next_telemetry_ns;
        if (discrete_change || periodic_due) && ch.tel_tx.push(snap).is_ok() {
            prev_snap = snap;
            tel_initialized = true;
            next_telemetry_ns = now_raw_ns.saturating_add(TELEMETRY_INTERVAL_NS);
            ch.rt_notify_tx.wake();
        } else if periodic_due {
            next_telemetry_ns = now_raw_ns.saturating_add(TELEMETRY_INTERVAL_NS);
        }

        next_wakeup_raw_ns = min_deadline(next_engine_deadline, Some(next_telemetry_ns))
            .unwrap_or_else(|| now_raw_ns.saturating_add(TELEMETRY_INTERVAL_NS));
        if fd_outbox.has_pending() {
            next_wakeup_raw_ns = next_wakeup_raw_ns.min(now_raw_ns.saturating_add(5_000_000));
        }
    }
}

fn read_midi_input<S: LinkSession>(
    active_fds: &mut MidiFdSet,
    now_raw_ns: i64,
    engine: &mut SyncEngine,
    session: &mut S,
    fd_outbox: &mut FdReturnOutbox<'_>,
) -> Option<i32> {
    let in_fd = active_fds.in_fd;
    if in_fd < 0 {
        return None;
    }
    let mut buf = [0u8; 64];
    for _ in 0..MAX_READ_CHUNKS_PER_WAKE {
        let n = unsafe { libc::read(in_fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            for &byte in &buf[..n as usize] {
                if let Some(ev) = MidiEvent::from_byte(byte) {
                    let relay = engine.on_midi_event(ev, now_raw_ns, session);
                    if !relay.is_empty() {
                        write_midi_batch(active_fds, &relay, true, fd_outbox);
                    }
                }
            }
            continue;
        }
        if n == 0 {
            return Some(0);
        }
        let err = last_errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return None;
        }
        if err == libc::EINTR {
            continue;
        }
        return Some(err);
    }
    None
}

fn write_midi_batch(
    active_fds: &mut MidiFdSet,
    batch: &MidiOutputBatch,
    exclude_input: bool,
    fd_outbox: &mut FdReturnOutbox<'_>,
) {
    if batch.is_empty() || active_fds.out_count == 0 {
        return;
    }

    let mut bytes = [0u8; MAX_MIDI_BATCH_BYTES];
    let len = batch.len().min(MAX_MIDI_BATCH_BYTES);
    for (dst, scheduled) in bytes.iter_mut().zip(batch.as_slice().iter()) {
        *dst = scheduled.event.to_byte();
    }

    let mut idx = 0usize;
    while idx < (active_fds.out_count as usize).min(crate::midi::MAX_MIDI_OUTPUTS) {
        let out_fd = active_fds.out_fds[idx];
        if out_fd < 0 || (exclude_input && out_fd == active_fds.in_fd) {
            idx += 1;
            continue;
        }

        let mut offset = 0;
        let mut detached = false;
        while offset < len {
            let n =
                unsafe { libc::write(out_fd, bytes[offset..len].as_ptr().cast(), len - offset) };
            if n > 0 {
                offset += n as usize;
                continue;
            }
            if n == 0 {
                break;
            }
            let err = last_errno();
            if err == libc::EINTR {
                continue;
            }
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                break;
            }
            let ret = detach_output_fd_at(active_fds, idx, err);
            fd_outbox.push(ret);
            detached = true;
            break;
        }

        if !detached {
            idx += 1;
        }
    }
}

fn detach_input_fd(active_fds: &mut MidiFdSet, errno: i32) -> FdReturn {
    let in_fd = active_fds.in_fd;
    active_fds.in_fd = -1;
    let mut detached = MidiFdSet::single_input(in_fd, active_fds.generation);

    // If this descriptor was shared full-duplex with an output slot, detach that output slot too.
    if in_fd >= 0 {
        let mut i = 0usize;
        while i < active_fds.out_count as usize {
            if active_fds.out_fds[i] == in_fd {
                let _ = active_fds.remove_output_at(i);
                detached.push_output(in_fd);
            } else {
                i += 1;
            }
        }
    }

    FdReturn::DetachedOnError {
        fds: detached,
        errno,
    }
}

fn detach_output_fd_at(active_fds: &mut MidiFdSet, idx: usize, errno: i32) -> FdReturn {
    let Some(out_fd) = active_fds.remove_output_at(idx) else {
        return FdReturn::DetachedOnError {
            fds: MidiFdSet {
                generation: active_fds.generation,
                ..MidiFdSet::EMPTY
            },
            errno,
        };
    };
    let mut detached = MidiFdSet::single_output(out_fd, active_fds.generation);
    if out_fd >= 0 && active_fds.in_fd == out_fd {
        active_fds.in_fd = -1;
        detached.in_fd = out_fd;
    }
    FdReturn::DetachedOnError {
        fds: detached,
        errno,
    }
}

#[inline]
fn beat_index(beat_phase: f64) -> i64 {
    beat_phase.max(0.0).floor() as i64
}

fn is_discrete_telemetry_change(
    initialized: bool,
    prev: TelemetrySnapshot,
    curr: TelemetrySnapshot,
) -> bool {
    !initialized
        || curr.direction != prev.direction
        || curr.lock_state != prev.lock_state
        || curr.start_stop_sync != prev.start_stop_sync
        || curr.clock_thru != prev.clock_thru
        || curr.round_bpm != prev.round_bpm
        || curr.is_playing != prev.is_playing
        || curr.link_peers != prev.link_peers
        || curr.midi_offset_ms != prev.midi_offset_ms
        || curr.quantum != prev.quantum
        || (curr.bpm_link - prev.bpm_link).abs() >= 0.05
        || beat_index(curr.beat_phase) != beat_index(prev.beat_phase)
        || curr.beat_phase + 0.5 < prev.beat_phase
}

#[cfg(any(target_os = "linux", test))]
fn is_early_timer_wakeup(now_raw_ns: i64, armed_wakeup_raw_ns: i64) -> bool {
    now_raw_ns < armed_wakeup_raw_ns.saturating_sub(200_000)
}

fn min_deadline(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

#[allow(dead_code)]
fn compute_poll_timeout_ms(now_raw_ns: i64, next_wakeup_raw_ns: i64) -> libc::c_int {
    let delta_ns = next_wakeup_raw_ns.saturating_sub(now_raw_ns);
    if delta_ns <= 0 {
        0
    } else {
        ((delta_ns.saturating_add(999_999)) / 1_000_000).clamp(1, 50) as libc::c_int
    }
}

fn last_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub use crate::rt_os::WakeSender;

/// Construct a wake pair for the RT thread.
pub fn create_rt_wake_pair() -> io::Result<(WakeSender, WakeReceiver)> {
    create_wake_pair()
}

#[cfg(test)]
mod tests {
    use std::os::fd::RawFd;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rtrb::RingBuffer;
    use tactus_protocol::LockState;

    use super::*;
    use crate::midi::close_fd_set;

    #[derive(Debug, Clone)]
    struct MockLinkSession {
        tempo: f64,
        is_playing: bool,
        beat_origin: f64,
        origin_time_us: i64,
        num_peers: usize,
    }

    impl MockLinkSession {
        fn new(tempo: f64) -> Self {
            Self {
                tempo,
                is_playing: false,
                beat_origin: 0.0,
                origin_time_us: 0,
                num_peers: 1,
            }
        }
    }

    impl LinkSession for MockLinkSession {
        fn num_peers(&self) -> usize {
            self.num_peers
        }

        fn tempo(&self) -> f64 {
            self.tempo
        }

        fn beat_at_time(&self, time_us: i64, _quantum: f64) -> f64 {
            let dt_s = (time_us - self.origin_time_us) as f64 / 1_000_000.0;
            self.beat_origin + dt_s * (self.tempo / 60.0)
        }

        fn phase_at_time(&self, time_us: i64, quantum: f64) -> f64 {
            self.beat_at_time(time_us, quantum).rem_euclid(quantum)
        }

        fn time_at_beat(&self, beat: f64, _quantum: f64) -> i64 {
            let dt_s = (beat - self.beat_origin) / (self.tempo / 60.0);
            self.origin_time_us + (dt_s * 1_000_000.0).round() as i64
        }

        fn is_playing(&self) -> bool {
            self.is_playing
        }

        fn set_tempo(&mut self, bpm: f64, at_time_us: i64) {
            let beat = self.beat_at_time(at_time_us, 4.0);
            self.beat_origin = beat;
            self.origin_time_us = at_time_us;
            self.tempo = bpm;
        }

        fn request_beat_at_time(&mut self, beat: f64, time_us: i64, _quantum: f64) {
            self.beat_origin = beat;
            self.origin_time_us = time_us;
        }

        fn force_beat_at_time(&mut self, beat: f64, time_us: i64, _quantum: f64) {
            self.beat_origin = beat;
            self.origin_time_us = time_us;
        }

        fn set_is_playing(&mut self, is_playing: bool, _at_time_us: i64) {
            self.is_playing = is_playing;
        }

        fn set_is_playing_and_request_beat_at_time(
            &mut self,
            is_playing: bool,
            time_us: i64,
            beat: f64,
            quantum: f64,
        ) {
            self.is_playing = is_playing;
            self.request_beat_at_time(beat, time_us, quantum);
        }
    }

    struct MockLinkDriver {
        session: MockLinkSession,
        clock_ns: Arc<AtomicI64>,
        start_instant: Instant,
        use_wall_clock: bool,
    }

    impl MockLinkDriver {
        fn new_wall(tempo: f64) -> Self {
            Self {
                session: MockLinkSession::new(tempo),
                clock_ns: Arc::new(AtomicI64::new(0)),
                start_instant: Instant::now(),
                use_wall_clock: true,
            }
        }
    }

    impl RtLinkDriver for MockLinkDriver {
        type Session = MockLinkSession;

        fn now_link_ns(&self) -> i64 {
            if self.use_wall_clock {
                self.start_instant.elapsed().as_nanos() as i64
            } else {
                self.clock_ns.load(Ordering::Relaxed)
            }
        }

        fn enable_start_stop_sync(&mut self, _enabled: bool) {}

        fn capture(&mut self) -> &mut Self::Session {
            &mut self.session
        }

        fn commit_if_dirty(&mut self) {}
    }

    fn make_pipe() -> (RawFd, RawFd) {
        let mut fds = [-1 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
        }
        (fds[0], fds[1])
    }

    fn drain_pipe_bytes(fd: RawFd, out: &mut Vec<u8>) {
        let mut buf = [0u8; 256];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            } else {
                break;
            }
        }
    }

    #[test]
    fn link_to_midi_emits_start_clocks_and_stop_on_direction_switch() {
        let (mut cmd_tx, cmd_rx) = RingBuffer::new(32);
        let (tel_tx, mut tel_rx) = RingBuffer::new(64);
        let (fd_ret_tx, mut fd_ret_rx) = RingBuffer::new(32);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");
        let (rt_notify_tx, _rt_notify_rx) = create_rt_wake_pair().expect("rt notify pair");

        let (out_read, out_write) = make_pipe();
        let mut driver = MockLinkDriver::new_wall(120.0);
        driver.session.is_playing = true;
        // Start slightly before quantum=4 boundary (~10 ms away) so Start (0xFA) fires quickly.
        driver.session.beat_origin = 3.98;
        driver.session.origin_time_us = 0;

        let mut engine = SyncEngine::new(SyncConfig {
            direction: Direction::LinkToMidi,
            start_stop_sync: true,
            ..SyncConfig::default()
        });

        let handle = thread::spawn(move || {
            run_rt_loop(
                &mut engine,
                &mut driver,
                RtLoopChannels {
                    cmd_rx,
                    tel_tx,
                    fd_ret_tx,
                    wake_rx,
                    rt_notify_tx,
                },
            )
            .expect("rt loop");
        });

        let attached = MidiFdSet::single_output(out_write, 1);
        cmd_tx.push(RtMessage::AttachMidi(attached)).unwrap();
        wake_tx.wake();

        let mut captured = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline && captured.len() < 8 {
            drain_pipe_bytes(out_read, &mut captured);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            captured.contains(&0xFA),
            "expected Start (0xFA) in output bytes, got {captured:02X?}"
        );
        assert!(
            captured.contains(&0xF8),
            "expected Clock (0xF8) in output bytes, got {captured:02X?}"
        );

        cmd_tx
            .push(RtMessage::Command(RtCommand::SetDirection(
                Direction::MidiToLink,
            )))
            .unwrap();
        wake_tx.wake();

        let stop_deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < stop_deadline && !captured.contains(&0xFC) {
            drain_pipe_bytes(out_read, &mut captured);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            captured.contains(&0xFC),
            "expected Stop (0xFC) when leaving LinkToMidi while playing: {captured:02X?}"
        );

        cmd_tx.push(RtMessage::Shutdown).unwrap();
        wake_tx.wake();
        handle.join().expect("rt thread joins cleanly");

        let mut saw_returned = false;
        while let Ok(ret) = fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) => {
                    assert_eq!(fds, attached);
                    close_fd_set(fds);
                    saw_returned = true;
                }
                FdReturn::DetachedOnError { fds, .. } => close_fd_set(fds),
            }
        }
        assert!(
            saw_returned,
            "expected active output FD returned on shutdown"
        );
        assert!(tel_rx.pop().is_ok(), "expected telemetry emitted");

        unsafe {
            libc::close(out_read);
        }
    }

    #[test]
    fn midi_to_link_filters_noise_locks_and_recovers_from_pipe_unplug() {
        let (mut cmd_tx, cmd_rx) = RingBuffer::new(32);
        let (tel_tx, mut tel_rx) = RingBuffer::new(128);
        let (fd_ret_tx, mut fd_ret_rx) = RingBuffer::new(32);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");
        let (rt_notify_tx, _rt_notify_rx) = create_rt_wake_pair().expect("rt notify pair");

        let (in_read_1, in_write_1) = make_pipe();
        let (in_read_2, in_write_2) = make_pipe();

        let mut driver = MockLinkDriver::new_wall(120.0);
        let mut engine = SyncEngine::new(SyncConfig {
            direction: Direction::MidiToLink,
            fast_window_secs: 0.25,
            ols_window_secs: 1.0,
            ..SyncConfig::default()
        });

        let handle = thread::spawn(move || {
            run_rt_loop(
                &mut engine,
                &mut driver,
                RtLoopChannels {
                    cmd_rx,
                    tel_tx,
                    fd_ret_tx,
                    wake_rx,
                    rt_notify_tx,
                },
            )
            .expect("rt loop");
        });

        let fds_1 = MidiFdSet::single_input(in_read_1, 1);
        cmd_tx.push(RtMessage::AttachMidi(fds_1)).unwrap();
        wake_tx.wake();

        // Feed Note-On, Active Sensing (0xFE), SysEx noise alongside 150 BPM 24 PPQN ticks (16.667 ms).
        let _ = unsafe {
            libc::write(
                in_write_1,
                [0xFA, 0x90, 0x3C, 0x7F, 0xFE].as_ptr().cast(),
                5,
            )
        };
        let pace_start = Instant::now();
        for i in 0..28u64 {
            let noise_and_tick: &[u8] = if i % 4 == 0 {
                &[0xF0, 0x7E, 0x7F, 0x06, 0xF7, 0xF8]
            } else {
                &[0xFE, 0xF8]
            };
            let _ = unsafe {
                libc::write(
                    in_write_1,
                    noise_and_tick.as_ptr().cast(),
                    noise_and_tick.len(),
                )
            };
            let next_tick_at = pace_start + Duration::from_micros((i + 1) * 16_667);
            if let Some(wait) = next_tick_at.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }

        let mut latest = None;
        let lock_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < lock_deadline {
            while let Ok(s) = tel_rx.pop() {
                latest = Some(s);
            }
            if latest.is_some_and(|s| s.lock_state == LockState::Locked) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let locked = latest.expect("expected telemetry snapshots");
        assert_eq!(locked.lock_state, LockState::Locked);
        assert!(
            (138.0..=165.0).contains(&locked.bpm_link),
            "expected tracked ~150 BPM, got {}",
            locked.bpm_link
        );

        // Simulate USB unplug by closing write end -> POLLHUP/EOF detach without restarting loop.
        unsafe {
            libc::close(in_write_1);
        }

        let mut detached = None;
        let detach_deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < detach_deadline {
            if let Ok(ret) = fd_ret_rx.pop() {
                match ret {
                    FdReturn::DetachedOnError { fds, .. } => {
                        detached = Some(fds);
                        close_fd_set(fds);
                        break;
                    }
                    FdReturn::Replaced(fds) => close_fd_set(fds),
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(detached, Some(fds_1));

        // Simulate hotplug replug: attach second pipe and ensure clock ticks lock again.
        let fds_2 = MidiFdSet::single_input(in_read_2, 2);
        cmd_tx.push(RtMessage::AttachMidi(fds_2)).unwrap();
        wake_tx.wake();

        for _ in 0..26 {
            let _ = unsafe { libc::write(in_write_2, [0xF8].as_ptr().cast(), 1) };
            thread::sleep(Duration::from_millis(20));
        }

        let mut recovered = None;
        let rec_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < rec_deadline {
            while let Ok(s) = tel_rx.pop() {
                recovered = Some(s);
            }
            if recovered.is_some_and(|s| s.lock_state == LockState::Locked) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            recovered.map(|s| s.lock_state),
            Some(LockState::Locked),
            "expected hotplug replug to re-acquire lock cleanly"
        );

        cmd_tx.push(RtMessage::Shutdown).unwrap();
        wake_tx.wake();
        handle.join().expect("rt loop shuts down cleanly");

        while let Ok(ret) = fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) | FdReturn::DetachedOnError { fds, .. } => {
                    close_fd_set(fds)
                }
            }
        }
        unsafe {
            libc::close(in_write_2);
        }
    }

    fn make_unix_duplex_pair() -> (RawFd, RawFd) {
        let mut sv = [-1 as RawFd; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
            0
        );
        for fd in sv {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
        }
        (sv[0], sv[1])
    }

    #[test]
    fn link_to_midi_fans_out_to_multiple_outputs_and_isolates_unplug() {
        let (mut cmd_tx, cmd_rx) = RingBuffer::new(32);
        let (tel_tx, _tel_rx) = RingBuffer::new(64);
        let (fd_ret_tx, mut fd_ret_rx) = RingBuffer::new(32);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");
        let (rt_notify_tx, _rt_notify_rx) = create_rt_wake_pair().expect("rt notify pair");

        let (out1_r, out1_w) = make_pipe();
        let (out2_r, out2_w) = make_pipe();

        let mut driver = MockLinkDriver::new_wall(120.0);
        driver.session.is_playing = true;
        driver.session.beat_origin = 3.98;
        driver.session.origin_time_us = 0;

        let mut engine = SyncEngine::new(SyncConfig {
            direction: Direction::LinkToMidi,
            start_stop_sync: true,
            ..SyncConfig::default()
        });

        let handle = thread::spawn(move || {
            run_rt_loop(
                &mut engine,
                &mut driver,
                RtLoopChannels {
                    cmd_rx,
                    tel_tx,
                    fd_ret_tx,
                    wake_rx,
                    rt_notify_tx,
                },
            )
            .expect("rt loop");
        });

        let attached = MidiFdSet::from_parts(-1, &[out1_w, out2_w], 1);
        cmd_tx.push(RtMessage::AttachMidi(attached)).unwrap();
        wake_tx.wake();

        let mut cap1 = Vec::new();
        let mut cap2 = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline && (cap1.len() < 4 || cap2.len() < 4) {
            drain_pipe_bytes(out1_r, &mut cap1);
            drain_pipe_bytes(out2_r, &mut cap2);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            cap1.contains(&0xFA) && cap1.contains(&0xF8),
            "out1={cap1:02X?}"
        );
        assert!(
            cap2.contains(&0xFA) && cap2.contains(&0xF8),
            "out2={cap2:02X?}"
        );

        // Unplug output 1 only; output 2 must continue uninterrupted.
        unsafe {
            libc::close(out1_r);
        }

        let mut detached1 = None;
        let detach_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < detach_deadline {
            if let Ok(FdReturn::DetachedOnError { fds, .. }) = fd_ret_rx.pop() {
                detached1 = Some(fds);
                close_fd_set(fds);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(detached1, Some(MidiFdSet::single_output(out1_w, 1)));

        cap2.clear();
        cmd_tx
            .push(RtMessage::Command(RtCommand::SetDirection(
                Direction::MidiToLink,
            )))
            .unwrap();
        wake_tx.wake();

        let stop_deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < stop_deadline && !cap2.contains(&0xFC) {
            drain_pipe_bytes(out2_r, &mut cap2);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            cap2.contains(&0xFC),
            "expected healthy output 2 to stay attached and receive Stop (0xFC): {cap2:02X?}"
        );

        cmd_tx.push(RtMessage::Shutdown).unwrap();
        wake_tx.wake();
        handle.join().expect("rt thread joins cleanly");

        while let Ok(ret) = fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) | FdReturn::DetachedOnError { fds, .. } => {
                    close_fd_set(fds)
                }
            }
        }
        unsafe {
            libc::close(out2_r);
        }
    }

    #[test]
    fn midi_to_link_clock_thru_relays_to_other_outputs_and_blocks_input_loop() {
        let (mut cmd_tx, cmd_rx) = RingBuffer::new(32);
        let (tel_tx, _tel_rx) = RingBuffer::new(64);
        let (fd_ret_tx, mut fd_ret_rx) = RingBuffer::new(32);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");
        let (rt_notify_tx, _rt_notify_rx) = create_rt_wake_pair().expect("rt notify pair");

        let (duplex_rt, duplex_peer) = make_unix_duplex_pair();
        let (other_r, other_w) = make_pipe();

        let mut driver = MockLinkDriver::new_wall(120.0);
        let mut engine = SyncEngine::new(SyncConfig {
            direction: Direction::MidiToLink,
            start_stop_sync: true,
            clock_thru: true,
            ..SyncConfig::default()
        });

        let handle = thread::spawn(move || {
            run_rt_loop(
                &mut engine,
                &mut driver,
                RtLoopChannels {
                    cmd_rx,
                    tel_tx,
                    fd_ret_tx,
                    wake_rx,
                    rt_notify_tx,
                },
            )
            .expect("rt loop");
        });

        // Attach duplex port as BOTH input and output 0, plus secondary output 1.
        let attached = MidiFdSet::from_parts(duplex_rt, &[duplex_rt, other_w], 1);
        cmd_tx.push(RtMessage::AttachMidi(attached)).unwrap();
        wake_tx.wake();

        let seq = [0xFAu8, 0xF8, 0xF8, 0xFC];
        let written = unsafe { libc::write(duplex_peer, seq.as_ptr().cast(), seq.len()) };
        assert_eq!(written, seq.len() as isize);

        let mut relayed = Vec::new();
        let mut echoed_back = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline && relayed.len() < seq.len() {
            drain_pipe_bytes(other_r, &mut relayed);
            drain_pipe_bytes(duplex_peer, &mut echoed_back);
            thread::sleep(Duration::from_millis(5));
        }

        // Secondary output receives the exact relayed sequence; input peer receives zero echo.
        assert_eq!(relayed, seq);
        drain_pipe_bytes(duplex_peer, &mut echoed_back);
        assert!(
            echoed_back.is_empty(),
            "clock thru must never echo back to input port: {echoed_back:02X?}"
        );

        // Restart relayed playback, then switch direction to LinkToMidi: secondary output must
        // receive Stop (0xFC) while the duplex input port still receives zero Stop echo.
        relayed.clear();
        assert_eq!(
            unsafe { libc::write(duplex_peer, [0xFAu8].as_ptr().cast(), 1) },
            1
        );
        let restart_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < restart_deadline && !relayed.contains(&0xFA) {
            drain_pipe_bytes(other_r, &mut relayed);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(relayed, vec![0xFA]);
        drain_pipe_bytes(duplex_peer, &mut echoed_back);
        assert!(
            echoed_back.is_empty(),
            "clock thru must never echo Start back to input port: {echoed_back:02X?}"
        );

        relayed.clear();
        cmd_tx
            .push(RtMessage::Command(RtCommand::SetDirection(
                Direction::LinkToMidi,
            )))
            .unwrap();
        wake_tx.wake();

        let switch_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < switch_deadline && !relayed.contains(&0xFC) {
            drain_pipe_bytes(other_r, &mut relayed);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            relayed.contains(&0xFC),
            "expected Stop (0xFC) on relayed output when leaving MidiToLink: {relayed:02X?}"
        );
        drain_pipe_bytes(duplex_peer, &mut echoed_back);
        assert!(
            !echoed_back.contains(&0xFC),
            "direction-switch Stop must never echo back to input port: {echoed_back:02X?}"
        );

        cmd_tx.push(RtMessage::Shutdown).unwrap();
        wake_tx.wake();
        handle.join().expect("rt thread joins cleanly");

        while let Ok(ret) = fd_ret_rx.pop() {
            match ret {
                FdReturn::Replaced(fds) | FdReturn::DetachedOnError { fds, .. } => {
                    close_fd_set(fds)
                }
            }
        }
        unsafe {
            libc::close(duplex_peer);
            libc::close(other_r);
        }
    }

    #[test]
    fn early_timer_wakeup_checks_armed_wakeup_deadline() {
        // In MidiToLink, timerfd is armed for next_telemetry_ns (e.g. 66.7 ms) while
        // next_engine_deadline may be 500 ms away. Waking at next_telemetry_ns is NOT early.
        let armed_wakeup_ns = 66_666_667_i64;
        assert!(!is_early_timer_wakeup(armed_wakeup_ns, armed_wakeup_ns));
        assert!(!is_early_timer_wakeup(
            armed_wakeup_ns - 150_000,
            armed_wakeup_ns
        ));
        assert!(is_early_timer_wakeup(
            armed_wakeup_ns - 250_000,
            armed_wakeup_ns
        ));
    }

    #[test]
    fn pending_fd_return_retries_without_dropping_fds_and_wakes_notify() {
        let (mut cmd_tx, cmd_rx) = RingBuffer::new(32);
        let (tel_tx, _tel_rx) = RingBuffer::new(64);
        // 1-slot ring forces FdReturnOutbox::pending to retain the second return until drained.
        let (fd_ret_tx, mut fd_ret_rx) = RingBuffer::new(1);
        let (wake_tx, wake_rx) = create_rt_wake_pair().expect("wake pair");
        let (rt_notify_tx, rt_notify_rx) = create_rt_wake_pair().expect("rt notify pair");

        let (r1, w1) = make_pipe();
        let (r2, w2) = make_pipe();
        let (r3, w3) = make_pipe();

        let mut driver = MockLinkDriver::new_wall(120.0);
        let mut engine = SyncEngine::new(SyncConfig::default());
        let handle = thread::spawn(move || {
            run_rt_loop(
                &mut engine,
                &mut driver,
                RtLoopChannels {
                    cmd_rx,
                    tel_tx,
                    fd_ret_tx,
                    wake_rx,
                    rt_notify_tx,
                },
            )
            .expect("rt loop");
        });

        let set1 = MidiFdSet::single_output(w1, 1);
        let set2 = MidiFdSet::single_output(w2, 2);
        let set3 = MidiFdSet::single_output(w3, 3);
        cmd_tx.push(RtMessage::AttachMidi(set1)).unwrap();
        cmd_tx.push(RtMessage::AttachMidi(set2)).unwrap();
        cmd_tx.push(RtMessage::AttachMidi(set3)).unwrap();
        wake_tx.wake();

        // Drain initial wakeups, wait for first Replaced(set1) while set2 sits in pending_fd_return.
        let mut returned = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && returned.len() < 2 {
            while let Ok(ret) = fd_ret_rx.pop() {
                if let FdReturn::Replaced(fds) = ret {
                    returned.push(fds);
                    close_fd_set(fds);
                }
            }
            rt_notify_rx.drain();
            thread::sleep(Duration::from_millis(5));
        }

        // Verify rt_notify_rx receives discrete wakeup signal on state transitions and FD returns.
        let mut pfd = libc::pollfd {
            fd: rt_notify_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        cmd_tx
            .push(RtMessage::Command(RtCommand::SetRoundBpm(true)))
            .unwrap();
        wake_tx.wake();
        let notify_ready = unsafe { libc::poll(&raw mut pfd, 1, 200) };
        assert_eq!(notify_ready, 1);
        rt_notify_rx.drain();

        cmd_tx.push(RtMessage::Shutdown).unwrap();
        wake_tx.wake();
        handle.join().expect("rt loop joins");

        while let Ok(ret) = fd_ret_rx.pop() {
            if let FdReturn::Replaced(fds) = ret {
                returned.push(fds);
                close_fd_set(fds);
            }
        }
        assert_eq!(returned, vec![set1, set2, set3]);

        unsafe {
            libc::close(r1);
            libc::close(r2);
            libc::close(r3);
        }
    }

    #[test]
    fn discrete_telemetry_triggers_on_every_beat_boundary_not_only_downbeat() {
        let base = TelemetrySnapshot {
            quantum: 4.0,
            beat_phase: 0.10,
            ..TelemetrySnapshot::default()
        };

        // Same beat (sub-beat progression within beat 1): NOT discrete.
        let sub_beat = TelemetrySnapshot {
            beat_phase: 0.85,
            ..base
        };
        assert!(!is_discrete_telemetry_change(true, base, sub_beat));

        // Crossing into beat 2, beat 3, beat 4, and wrapping back to beat 1: ALL discrete.
        let beat2 = TelemetrySnapshot {
            beat_phase: 1.02,
            ..base
        };
        let beat3 = TelemetrySnapshot {
            beat_phase: 2.01,
            ..base
        };
        let beat4 = TelemetrySnapshot {
            beat_phase: 3.03,
            ..base
        };
        let beat1_wrap = TelemetrySnapshot {
            beat_phase: 0.01,
            ..base
        };
        assert!(is_discrete_telemetry_change(true, sub_beat, beat2));
        assert!(is_discrete_telemetry_change(true, beat2, beat3));
        assert!(is_discrete_telemetry_change(true, beat3, beat4));
        assert!(is_discrete_telemetry_change(true, beat4, beat1_wrap));

        // Quantum = 1.0 wrap (beat_phase stays < 1.0, wraps e.g. 0.9 -> 0.05): still discrete.
        let q1_end = TelemetrySnapshot {
            quantum: 1.0,
            beat_phase: 0.90,
            ..base
        };
        let q1_wrap = TelemetrySnapshot {
            quantum: 1.0,
            beat_phase: 0.05,
            ..base
        };
        assert!(is_discrete_telemetry_change(true, q1_end, q1_wrap));
    }
}
