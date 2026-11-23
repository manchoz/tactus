(() => {
  "use strict";

  const BACKOFF_MS = [250, 500, 1000, 2000];
  const STALE_TIMEOUT_MS = 6000;
  const WATCHDOG_INTERVAL_MS = 2000;
  const DRAG_SETTLE_MS = 250;
  const OFFSET_THROTTLE_MS = 50;
  const MAX_MIDI_OUTPUTS = 8;
  const PHASE_SNAP_MS = 35;
  const PHASE_BLEND_ALPHA = 0.35;
  const BACKWARD_GUARD_MS = 25;

  const localUiHash =
    document.querySelector('meta[name="tactus-ui-hash"]')?.getAttribute("content") || "";

  const els = {
    connPill: document.getElementById("conn-pill"),
    peersPill: document.getElementById("peers-pill"),
    transportBadge: document.getElementById("transport-badge"),
    lockBadge: document.getElementById("lock-badge"),
    bpmBtn: document.getElementById("bpm-btn"),
    bpmValue: document.getElementById("bpm-value"),
    bpmEditor: document.getElementById("bpm-editor"),
    bpmInput: document.getElementById("bpm-input"),
    bpmCancel: document.getElementById("bpm-cancel"),
    bpmNudgeRow: document.getElementById("bpm-nudge-row"),
    midiDiagRow: document.getElementById("midi-diag-row"),
    diagLong: document.getElementById("diag-long"),
    diagFast: document.getElementById("diag-fast"),
    diagPhase: document.getElementById("diag-phase"),
    beatStrip: document.getElementById("beat-strip"),
    phaseBarFill: document.getElementById("phase-bar-fill"),
    playStopBtn: document.getElementById("play-stop-btn"),
    resyncBtn: document.getElementById("resync-btn"),
    dirButtons: Array.from(document.querySelectorAll("[data-dir]")),
    bpmStepButtons: Array.from(document.querySelectorAll("[data-bpm-delta]")),
    quantumButtons: Array.from(document.querySelectorAll("[data-quantum]")),
    quantumCustom: document.getElementById("quantum-custom"),
    startStopToggle: document.getElementById("start-stop-toggle"),
    clockThruBlock: document.getElementById("clock-thru-block"),
    clockThruToggle: document.getElementById("clock-thru-toggle"),
    roundBpmBlock: document.getElementById("round-bpm-block"),
    roundBpmToggle: document.getElementById("round-bpm-toggle"),
    offsetReadout: document.getElementById("offset-readout"),
    offsetReset: document.getElementById("offset-reset"),
    offsetMinus: document.getElementById("offset-minus"),
    offsetPlus: document.getElementById("offset-plus"),
    offsetSlider: document.getElementById("offset-slider"),
    midiInSelect: document.getElementById("midi-in"),
    midiOutList: document.getElementById("midi-out-list"),
    midiInStatus: document.getElementById("midi-in-status"),
    midiOutStatus: document.getElementById("midi-out-status"),
    midiEmptyHint: document.getElementById("midi-empty-hint"),
  };

  let ws = null;
  let paused = false;
  let reconnectAttempt = 0;
  let reconnectTimer = null;
  let lastMessageAt = 0;
  let latestState = null;

  let isDraggingOffset = false;
  let offsetSettleTimer = null;
  let lastOffsetSentAt = 0;
  let lastRenderedBeats = 0;
  let lastActiveBeatIdx = -1;
  let phaseRafId = null;
  const phaseClock = {
    valid: false,
    anchorPhase: 0,
    anchorTimeMs: 0,
    bpm: 120,
    quantum: 4,
    beatCount: 4,
  };
  let lastInSignature = null;
  let lastOutSignature = null;
  let resyncTimer = null;
  let wakeLockSentinel = null;
  let wakeLockBusy = false;

  function supportsWakeLock() {
    return Boolean(
      window.isSecureContext &&
        navigator.wakeLock &&
        typeof navigator.wakeLock.request === "function"
    );
  }

  async function requestScreenWakeLock() {
    if (
      !supportsWakeLock() ||
      paused ||
      document.hidden ||
      wakeLockSentinel ||
      wakeLockBusy
    ) {
      return;
    }
    wakeLockBusy = true;
    try {
      const sentinel = await navigator.wakeLock.request("screen");
      if (document.hidden || paused) {
        try {
          await sentinel.release();
        } catch (_err) {
          // Ignore release failures when hidden.
        }
        return;
      }
      wakeLockSentinel = sentinel;
      sentinel.addEventListener("release", () => {
        if (wakeLockSentinel === sentinel) {
          wakeLockSentinel = null;
        }
      });
    } catch (_err) {
      // Silently ignore unsupported, non-HTTPS, or gesture-gated rejections.
    } finally {
      wakeLockBusy = false;
    }
  }

  async function releaseScreenWakeLock() {
    const sentinel = wakeLockSentinel;
    wakeLockSentinel = null;
    if (!sentinel) {
      return;
    }
    try {
      await sentinel.release();
    } catch (_err) {
      // Ignore release failures during background transitions.
    }
  }

  function setConnectionStatus(status) {
    els.connPill.className = "pill";
    if (status === "live") {
      els.connPill.textContent = "LIVE";
      els.connPill.classList.add("pill-live");
    } else if (status === "reconnecting") {
      els.connPill.textContent = "RECONNECTING";
      els.connPill.classList.add("pill-reconnecting");
    } else {
      els.connPill.textContent = "OFFLINE";
      els.connPill.classList.add("pill-offline");
    }
  }

  function clearReconnectTimer() {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function scheduleReconnect(immediate) {
    if (paused || document.hidden) {
      return;
    }
    clearReconnectTimer();
    const delay = immediate
      ? 0
      : BACKOFF_MS[Math.min(reconnectAttempt, BACKOFF_MS.length - 1)];
    if (!immediate) {
      reconnectAttempt += 1;
    }
    setConnectionStatus("reconnecting");
    reconnectTimer = setTimeout(connectWebSocket, delay);
  }

  function closeSocketCleanly() {
    stopPhaseLoop(true);
    if (ws) {
      const stale = ws;
      ws = null;
      stale.onopen = null;
      stale.onmessage = null;
      stale.onerror = null;
      stale.onclose = null;
      try {
        stale.close(1000, "lifecycle");
      } catch (_err) {
        // Ignore close errors during background transitions.
      }
    }
  }

  async function checkUiVersion() {
    if (!localUiHash) {
      return;
    }
    try {
      const resp = await fetch("/healthz", { cache: "no-store" });
      if (!resp.ok) {
        return;
      }
      const data = await resp.json();
      const remoteHash = typeof data?.ui_hash === "string" ? data.ui_hash : "";
      if (!remoteHash || remoteHash === localUiHash) {
        return;
      }
      const reloadKey = `tactus-reloaded-${remoteHash}`;
      let alreadyTried = false;
      try {
        alreadyTried = window.sessionStorage.getItem(reloadKey) === "1";
        if (!alreadyTried) {
          window.sessionStorage.setItem(reloadKey, "1");
        }
      } catch (_err) {
        // Ignore storage restrictions and reload once.
      }
      if (!alreadyTried) {
        window.location.reload();
      }
    } catch (_err) {
      // Ignore transient network errors during reconnect.
    }
  }

  function connectWebSocket() {
    if (paused || document.hidden) {
      return;
    }
    clearReconnectTimer();
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    closeSocketCleanly();

    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${window.location.host}/ws`;
    setConnectionStatus("reconnecting");

    const socket = new WebSocket(url);
    ws = socket;

    socket.onopen = () => {
      if (ws !== socket) {
        return;
      }
      reconnectAttempt = 0;
      lastMessageAt = Date.now();
      setConnectionStatus("live");
      void checkUiVersion();
    };

    socket.onmessage = (event) => {
      if (ws !== socket) {
        return;
      }
      lastMessageAt = Date.now();
      try {
        const state = JSON.parse(event.data);
        renderState(state);
      } catch (_err) {
        // Ignore malformed payloads without tearing down the live view.
      }
    };

    socket.onerror = () => {
      if (ws === socket) {
        setConnectionStatus("reconnecting");
      }
    };

    socket.onclose = () => {
      if (ws !== socket) {
        return;
      }
      ws = null;
      stopPhaseLoop(true);
      if (paused || document.hidden) {
        setConnectionStatus("offline");
      } else {
        scheduleReconnect(false);
      }
    };
  }

  function sendCommand(cmd) {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return false;
    }
    try {
      ws.send(JSON.stringify(cmd));
      return true;
    } catch (_err) {
      return false;
    }
  }

  function sendMidiPorts(input, outputs) {
    const sent = sendCommand({
      select_midi_ports: {
        input,
        outputs,
      },
    });
    if (sent && latestState) {
      latestState.midi_input_match = input;
      latestState.midi_output_matches = outputs.slice();
    }
    return sent;
  }

  function formatSignedMs(value) {
    const rounded = Math.round(value * 2) / 2;
    const prefix = rounded > 0 ? "+" : "";
    return `${prefix}${rounded.toFixed(1)} ms`;
  }

  function snapHalfMs(value) {
    const clamped = Math.max(-50, Math.min(50, value));
    return Math.round(clamped * 2) / 2;
  }

  function clampBpm(value) {
    return Math.max(20, Math.min(300, Math.round(value * 10) / 10));
  }

  function matchesRule(port, rule) {
    const needle = rule.trim().toLowerCase();
    if (!needle) {
      return false;
    }
    if (port.id === rule.trim()) {
      return true;
    }
    const combined = `${port.card_name} ${port.port_name}`.toLowerCase();
    return (
      port.id.toLowerCase().includes(needle) ||
      port.card_name.toLowerCase().includes(needle) ||
      port.port_name.toLowerCase().includes(needle) ||
      combined.includes(needle)
    );
  }

  function persistentMatchValue(port, eligiblePorts) {
    const candidates = [
      port.port_name,
      `${port.card_name} ${port.port_name}`,
      port.id,
    ];
    for (const candidate of candidates) {
      const trimmed = candidate.trim();
      if (!trimmed) {
        continue;
      }
      const matchCount = eligiblePorts.filter((p) => matchesRule(p, trimmed)).length;
      if (matchCount === 1) {
        return trimmed;
      }
    }
    return port.id;
  }

  function findMatchedPort(eligiblePorts, rule) {
    const trimmed = rule.trim();
    if (!trimmed) {
      return null;
    }
    const exact = eligiblePorts.find((p) => p.id === trimmed);
    if (exact) {
      return exact;
    }
    return eligiblePorts.find((p) => matchesRule(p, trimmed)) || null;
  }

  function formatPortTitle(port) {
    const card = port.card_name.trim();
    const name = port.port_name.trim();
    if (!card || name.toLowerCase().startsWith(card.toLowerCase())) {
      return name || port.id;
    }
    return `${card} — ${name}`;
  }

  function formatPortLabel(port) {
    return `${formatPortTitle(port)} (${port.id})`;
  }

  function rebuildBeatStrip(beatCount) {
    els.beatStrip.innerHTML = "";
    els.beatStrip.style.gridTemplateColumns = `repeat(${Math.min(beatCount, 8)}, 1fr)`;
    for (let i = 0; i < beatCount; i += 1) {
      const cell = document.createElement("div");
      cell.className = "beat-cell";
      cell.textContent = String(i + 1);
      els.beatStrip.appendChild(cell);
    }
    lastRenderedBeats = beatCount;
    lastActiveBeatIdx = -1;
  }

  function wrapPhase(phase, quantum) {
    return ((phase % quantum) + quantum) % quantum;
  }

  function shortestPhaseDelta(target, current, quantum) {
    let delta = wrapPhase(target - current, quantum);
    if (delta > quantum * 0.5) {
      delta -= quantum;
    }
    return delta;
  }

  function predictPhase(nowMs) {
    const dtMs = Math.max(0, nowMs - phaseClock.anchorTimeMs);
    const deltaBeats = dtMs * (phaseClock.bpm / 60000);
    return wrapPhase(phaseClock.anchorPhase + deltaBeats, phaseClock.quantum);
  }

  function syncPhaseClock(t, nowMs) {
    const quantum = Number.isFinite(t.quantum) && t.quantum > 0 ? t.quantum : 4;
    const beatCount = Math.max(1, Math.min(16, Math.round(quantum)));
    const bpm = Number.isFinite(t.bpm_link) && t.bpm_link > 0 ? t.bpm_link : 120;
    const serverPhase = wrapPhase(Number.isFinite(t.beat_phase) ? t.beat_phase : 0, quantum);

    if (
      !phaseClock.valid ||
      Math.abs(quantum - phaseClock.quantum) > 1e-6 ||
      Math.abs(bpm - phaseClock.bpm) > 0.05
    ) {
      phaseClock.valid = true;
      phaseClock.anchorPhase = serverPhase;
      phaseClock.anchorTimeMs = nowMs;
      phaseClock.bpm = bpm;
      phaseClock.quantum = quantum;
      phaseClock.beatCount = beatCount;
      return;
    }

    const predicted = predictPhase(nowMs);
    const errBeats = shortestPhaseDelta(serverPhase, predicted, quantum);
    const errMs = (errBeats * 60000) / bpm;
    const predBeat = Math.floor(predicted) % beatCount;
    const serverBeat = Math.floor(serverPhase) % beatCount;

    let nextPhase;
    if (Math.abs(errMs) > PHASE_SNAP_MS || (errBeats > 0 && serverBeat !== predBeat)) {
      nextPhase = serverPhase;
    } else if (errBeats < 0 && serverBeat !== predBeat && Math.abs(errMs) <= BACKWARD_GUARD_MS) {
      nextPhase = predBeat;
    } else {
      nextPhase = wrapPhase(predicted + errBeats * PHASE_BLEND_ALPHA, quantum);
    }

    phaseClock.anchorPhase = nextPhase;
    phaseClock.anchorTimeMs = nowMs;
    phaseClock.bpm = bpm;
    phaseClock.quantum = quantum;
    phaseClock.beatCount = beatCount;
  }

  function paintPhase(phase) {
    if (phaseClock.beatCount !== lastRenderedBeats) {
      rebuildBeatStrip(phaseClock.beatCount);
    }
    const activeIdx = Math.min(phaseClock.beatCount - 1, Math.max(0, Math.floor(phase)));
    if (activeIdx !== lastActiveBeatIdx) {
      const cells = els.beatStrip.children;
      for (let i = 0; i < cells.length; i += 1) {
        const cell = cells[i];
        cell.classList.toggle("active", i === activeIdx);
        cell.classList.toggle("downbeat", i === 0 && activeIdx === 0);
      }
      lastActiveBeatIdx = activeIdx;
    }
    const progress = Math.max(0, Math.min(1, phase / phaseClock.quantum));
    els.phaseBarFill.style.transform = `scaleX(${progress.toFixed(4)})`;
  }

  function stepPhaseFrame(nowMs) {
    phaseRafId = null;
    if (paused || document.hidden || !phaseClock.valid || !ws || ws.readyState !== WebSocket.OPEN) {
      return;
    }
    paintPhase(predictPhase(nowMs));
    phaseRafId = window.requestAnimationFrame(stepPhaseFrame);
  }

  function startPhaseLoop() {
    if (
      phaseRafId !== null ||
      paused ||
      document.hidden ||
      !phaseClock.valid ||
      !ws ||
      ws.readyState !== WebSocket.OPEN
    ) {
      return;
    }
    phaseRafId = window.requestAnimationFrame(stepPhaseFrame);
  }

  function stopPhaseLoop(invalidate) {
    if (phaseRafId !== null) {
      window.cancelAnimationFrame(phaseRafId);
      phaseRafId = null;
    }
    if (invalidate) {
      phaseClock.valid = false;
    }
  }

  function renderInputPortSelect(state) {
    const allPorts = state.available_midi_ports || [];
    const eligible = allPorts.filter((p) => p.can_read);
    const selectedId = state.selected_midi_input || null;
    const matchRule = state.midi_input_match || "";

    if (selectedId) {
      els.midiInStatus.textContent = `Attached (${selectedId})`;
      els.midiInStatus.className = "port-status port-ok";
    } else if (matchRule) {
      els.midiInStatus.textContent = `Waiting for "${matchRule}"`;
      els.midiInStatus.className = "port-status port-wait";
    } else {
      els.midiInStatus.textContent = "Disabled";
      els.midiInStatus.className = "port-status port-off";
    }

    const sig = JSON.stringify({
      eligible: eligible.map((p) => [p.id, p.card_name, p.port_name]),
      selectedId,
      matchRule,
    });
    if (sig === lastInSignature) {
      return;
    }
    lastInSignature = sig;

    els.midiInSelect.innerHTML = "";

    const noneOpt = document.createElement("option");
    noneOpt.value = "";
    noneOpt.textContent = "Disabled (None)";
    els.midiInSelect.appendChild(noneOpt);

    let chosenValue = "";
    let matchedOptionFound = false;

    for (const port of eligible) {
      const opt = document.createElement("option");
      const val = persistentMatchValue(port, eligible);
      opt.value = val;
      opt.textContent = formatPortLabel(port);
      if (selectedId && port.id === selectedId) {
        chosenValue = val;
        matchedOptionFound = true;
      }
      els.midiInSelect.appendChild(opt);
    }

    if (!matchedOptionFound && matchRule) {
      const waitOpt = document.createElement("option");
      waitOpt.value = matchRule;
      waitOpt.textContent = `Waiting for "${matchRule}" (unplugged)`;
      els.midiInSelect.appendChild(waitOpt);
      chosenValue = matchRule;
    }

    els.midiInSelect.value = chosenValue;
  }

  function renderOutputPortList(state) {
    const allPorts = state.available_midi_ports || [];
    const eligible = allPorts.filter((p) => p.can_write);
    const selectedIds = state.selected_midi_outputs || [];
    const matchRules = state.midi_output_matches || [];
    const isMidiToLink = state.telemetry?.direction === "midi_to_link";
    const clockThru = Boolean(state.telemetry?.clock_thru);
    const selectedInputId = state.selected_midi_input || null;

    const matchedByRule = matchRules.map((rule) => findMatchedPort(eligible, rule));
    const unmatchedRules = [];
    matchRules.forEach((rule, idx) => {
      if (!matchedByRule[idx]) {
        unmatchedRules.push({ rule, idx });
      }
    });

    if (selectedIds.length > 0 && unmatchedRules.length > 0) {
      els.midiOutStatus.textContent = `${selectedIds.length} attached, ${unmatchedRules.length} waiting`;
      els.midiOutStatus.className = "port-status port-ok";
    } else if (selectedIds.length > 0) {
      els.midiOutStatus.textContent = `${selectedIds.length} attached`;
      els.midiOutStatus.className = "port-status port-ok";
    } else if (matchRules.length > 0) {
      els.midiOutStatus.textContent = `Waiting (${matchRules.length})`;
      els.midiOutStatus.className = "port-status port-wait";
    } else {
      els.midiOutStatus.textContent = "Disabled";
      els.midiOutStatus.className = "port-status port-off";
    }

    const sig = JSON.stringify({
      eligible: eligible.map((p) => [p.id, p.card_name, p.port_name]),
      selectedIds,
      matchRules,
      selectedInputId,
      isMidiToLink,
      clockThru,
    });
    if (sig === lastOutSignature) {
      return;
    }
    lastOutSignature = sig;

    els.midiOutList.innerHTML = "";

    if (eligible.length === 0 && unmatchedRules.length === 0) {
      const empty = document.createElement("div");
      empty.className = "control-sub";
      empty.textContent = "No writable MIDI outputs connected.";
      els.midiOutList.appendChild(empty);
      return;
    }

    for (const port of eligible) {
      const isAttached = selectedIds.includes(port.id);
      const isSelected =
        isAttached || matchedByRule.some((matched) => matched && matched.id === port.id);
      const atOutputLimit = !isSelected && matchRules.length >= MAX_MIDI_OUTPUTS;

      const row = document.createElement("button");
      row.type = "button";
      row.className = `port-row${isSelected ? " selected" : ""}${atOutputLimit ? " disabled" : ""}`;
      row.setAttribute("aria-pressed", String(isSelected));
      row.disabled = atOutputLimit;

      const main = document.createElement("div");
      main.className = "port-row-main";

      const title = document.createElement("span");
      title.className = "port-row-name";
      title.textContent = formatPortTitle(port);

      const sub = document.createElement("span");
      sub.className = "port-row-sub";
      sub.textContent = port.id;

      main.appendChild(title);
      main.appendChild(sub);

      const meta = document.createElement("div");
      meta.className = "port-row-meta";

      if (isAttached) {
        const tag = document.createElement("span");
        if (isMidiToLink && selectedInputId && port.id === selectedInputId) {
          tag.className = "port-tag tag-loop";
          tag.textContent = "IN · loop blocked";
        } else if (isMidiToLink && clockThru) {
          tag.className = "port-tag tag-relay";
          tag.textContent = "Relay";
        } else if (isMidiToLink) {
          tag.className = "port-tag tag-idle";
          tag.textContent = "Idle";
        } else {
          tag.className = "port-tag tag-active";
          tag.textContent = "Active";
        }
        meta.appendChild(tag);
      } else if (isSelected) {
        const tag = document.createElement("span");
        tag.className = "port-tag tag-wait";
        tag.textContent = "Attaching";
        meta.appendChild(tag);
      } else if (atOutputLimit) {
        const tag = document.createElement("span");
        tag.className = "port-tag tag-idle";
        tag.textContent = "Max 8";
        meta.appendChild(tag);
      }

      const check = document.createElement("span");
      check.className = "port-check";
      check.setAttribute("aria-hidden", "true");
      check.textContent = "✓";
      meta.appendChild(check);

      row.appendChild(main);
      row.appendChild(meta);

      row.addEventListener("click", () => {
        const input = latestState ? latestState.midi_input_match || "" : "";
        const currentRules = latestState ? latestState.midi_output_matches || [] : matchRules;
        const currentEligible = (
          latestState ? latestState.available_midi_ports || eligible : eligible
        ).filter((p) => p.can_write);
        const alreadySelected = currentRules.some((rule) => {
          const matched = findMatchedPort(currentEligible, rule);
          return matched && matched.id === port.id;
        });
        let nextOutputs;
        if (alreadySelected) {
          nextOutputs = currentRules.filter((rule) => {
            const matched = findMatchedPort(currentEligible, rule);
            return !matched || matched.id !== port.id;
          });
        } else {
          if (currentRules.length >= MAX_MIDI_OUTPUTS) {
            return;
          }
          const val = persistentMatchValue(port, currentEligible);
          nextOutputs = [...currentRules, val];
        }
        sendMidiPorts(input, nextOutputs);
      });

      els.midiOutList.appendChild(row);
    }

    for (const item of unmatchedRules) {
      const row = document.createElement("div");
      row.className = "port-row unplugged";

      const main = document.createElement("div");
      main.className = "port-row-main";

      const title = document.createElement("span");
      title.className = "port-row-name";
      title.textContent = item.rule;

      const sub = document.createElement("span");
      sub.className = "port-row-sub";
      sub.textContent = "Unplugged — auto-attaches when reconnected";

      main.appendChild(title);
      main.appendChild(sub);

      const meta = document.createElement("div");
      meta.className = "port-row-meta";

      const tag = document.createElement("span");
      tag.className = "port-tag tag-wait";
      tag.textContent = "Waiting";
      meta.appendChild(tag);

      const removeBtn = document.createElement("button");
      removeBtn.type = "button";
      removeBtn.className = "port-remove-btn";
      removeBtn.textContent = "Remove";
      removeBtn.addEventListener("click", () => {
        const input = latestState ? latestState.midi_input_match || "" : "";
        const currentRules = latestState ? latestState.midi_output_matches || [] : matchRules;
        const nextOutputs = currentRules.filter((rule) => rule !== item.rule);
        sendMidiPorts(input, nextOutputs);
      });
      meta.appendChild(removeBtn);

      row.appendChild(main);
      row.appendChild(meta);
      els.midiOutList.appendChild(row);
    }
  }

  function renderState(state) {
    if (!state || !state.telemetry) {
      return;
    }
    latestState = state;
    const t = state.telemetry;

    els.peersPill.textContent = `${t.link_peers} LINK`;
    els.peersPill.className = t.link_peers > 0 ? "pill pill-live" : "pill pill-neutral";

    els.transportBadge.textContent = t.is_playing ? "PLAYING" : "STOPPED";
    els.transportBadge.className = t.is_playing
      ? "badge badge-playing"
      : "badge badge-stopped";

    const isLinkToMidi = t.direction === "link_to_midi";

    if (isLinkToMidi) {
      els.lockBadge.textContent = "LINK MASTER";
      els.lockBadge.className = "badge badge-master";
      els.bpmBtn.classList.add("editable");
      els.bpmBtn.classList.remove("readonly");
      els.bpmNudgeRow.classList.remove("hidden");
      els.midiDiagRow.classList.add("hidden");
    } else {
      els.bpmBtn.classList.remove("editable");
      els.bpmBtn.classList.add("readonly");
      els.bpmEditor.classList.add("hidden");
      els.bpmBtn.classList.remove("hidden");
      els.bpmNudgeRow.classList.add("hidden");
      els.midiDiagRow.classList.remove("hidden");

      if (t.lock_state === "locked") {
        els.lockBadge.textContent = "LOCKED";
        els.lockBadge.className = "badge badge-locked";
      } else if (t.lock_state === "acquiring") {
        els.lockBadge.textContent = "ACQUIRING";
        els.lockBadge.className = "badge badge-acquiring";
      } else {
        els.lockBadge.textContent = "UNLOCKED";
        els.lockBadge.className = "badge badge-unlocked";
      }

      els.diagLong.textContent =
        typeof t.bpm_midi_long === "number" ? t.bpm_midi_long.toFixed(2) : "--.--";
      els.diagFast.textContent =
        typeof t.bpm_midi_fast === "number" ? t.bpm_midi_fast.toFixed(2) : "--.--";
      const errPrefix = t.phase_error_ms > 0 ? "+" : "";
      els.diagPhase.textContent = `${errPrefix}${t.phase_error_ms.toFixed(1)} ms`;
    }

    els.bpmValue.textContent = t.bpm_link.toFixed(1);

    syncPhaseClock(t, performance.now());
    paintPhase(phaseClock.anchorPhase);
    startPhaseLoop();

    for (const btn of els.dirButtons) {
      btn.classList.toggle("active", btn.dataset.dir === t.direction);
    }

    els.playStopBtn.classList.toggle("hidden", !isLinkToMidi);
    els.playStopBtn.textContent = t.is_playing ? "Stop" : "Play";
    els.playStopBtn.classList.toggle("playing", Boolean(t.is_playing));
    els.playStopBtn.setAttribute("aria-pressed", String(Boolean(t.is_playing)));

    els.startStopToggle.setAttribute("aria-checked", String(Boolean(t.start_stop_sync)));
    els.clockThruBlock.classList.toggle("hidden", isLinkToMidi);
    els.clockThruToggle.setAttribute("aria-checked", String(Boolean(t.clock_thru)));
    els.roundBpmBlock.classList.toggle("hidden", isLinkToMidi);
    els.roundBpmToggle.setAttribute("aria-checked", String(Boolean(t.round_bpm)));

    if (!isDraggingOffset) {
      const snapped = snapHalfMs(t.midi_offset_ms || 0);
      els.offsetSlider.value = String(snapped);
      els.offsetReadout.textContent = formatSignedMs(snapped);
    }

    const qRounded = Math.round(t.quantum * 10) / 10;
    let standardMatch = false;
    for (const btn of els.quantumButtons) {
      const matches = Number(btn.dataset.quantum) === qRounded;
      btn.classList.toggle("active", matches);
      if (matches) {
        standardMatch = true;
      }
    }
    els.quantumCustom.textContent = standardMatch ? "" : `Custom: ${qRounded}`;

    const ports = state.available_midi_ports || [];
    els.midiEmptyHint.classList.toggle("hidden", ports.length > 0);
    renderInputPortSelect(state);
    renderOutputPortList(state);
  }

  function applyOffsetValue(rawValue, forceSend) {
    const snapped = snapHalfMs(Number(rawValue));
    els.offsetSlider.value = String(snapped);
    els.offsetReadout.textContent = formatSignedMs(snapped);
    const now = Date.now();
    if (forceSend || now - lastOffsetSentAt >= OFFSET_THROTTLE_MS) {
      lastOffsetSentAt = now;
      sendCommand({ set_offset_ms: snapped });
    }
  }

  function beginOffsetDrag() {
    isDraggingOffset = true;
    if (offsetSettleTimer !== null) {
      clearTimeout(offsetSettleTimer);
      offsetSettleTimer = null;
    }
  }

  function endOffsetDrag() {
    if (offsetSettleTimer !== null) {
      clearTimeout(offsetSettleTimer);
    }
    offsetSettleTimer = setTimeout(() => {
      isDraggingOffset = false;
      offsetSettleTimer = null;
    }, DRAG_SETTLE_MS);
  }

  // Event bindings
  for (const btn of els.dirButtons) {
    btn.addEventListener("click", () => {
      sendCommand({ set_direction: btn.dataset.dir });
    });
  }

  els.startStopToggle.addEventListener("click", () => {
    const current = els.startStopToggle.getAttribute("aria-checked") === "true";
    const next = !current;
    if (sendCommand({ set_start_stop_sync: next })) {
      els.startStopToggle.setAttribute("aria-checked", String(next));
    }
  });

  els.clockThruToggle.addEventListener("click", () => {
    const current = els.clockThruToggle.getAttribute("aria-checked") === "true";
    const next = !current;
    if (sendCommand({ set_clock_thru: next })) {
      els.clockThruToggle.setAttribute("aria-checked", String(next));
    }
  });

  els.roundBpmToggle.addEventListener("click", () => {
    const current = els.roundBpmToggle.getAttribute("aria-checked") === "true";
    const next = !current;
    if (sendCommand({ set_round_bpm: next })) {
      els.roundBpmToggle.setAttribute("aria-checked", String(next));
    }
  });

  els.playStopBtn.addEventListener("click", () => {
    if (!latestState || latestState.telemetry.direction !== "link_to_midi") {
      return;
    }
    sendCommand({ set_link_playing: !latestState.telemetry.is_playing });
  });

  els.resyncBtn.addEventListener("click", () => {
    if (!sendCommand("resync_downbeat")) {
      return;
    }
    els.resyncBtn.classList.add("confirmed");
    els.resyncBtn.textContent = "Resync Sent";
    if (resyncTimer !== null) {
      clearTimeout(resyncTimer);
    }
    resyncTimer = setTimeout(() => {
      els.resyncBtn.classList.remove("confirmed");
      els.resyncBtn.textContent = "Resync Downbeat";
      resyncTimer = null;
    }, 650);
  });

  for (const btn of els.bpmStepButtons) {
    btn.addEventListener("click", () => {
      if (!latestState || latestState.telemetry.direction !== "link_to_midi") {
        return;
      }
      const delta = Number(btn.dataset.bpmDelta);
      const next = clampBpm(latestState.telemetry.bpm_link + delta);
      sendCommand({ set_link_tempo_manual: next });
    });
  }

  els.bpmBtn.addEventListener("click", () => {
    if (!latestState || latestState.telemetry.direction !== "link_to_midi") {
      return;
    }
    els.bpmInput.value = latestState.telemetry.bpm_link.toFixed(1);
    els.bpmBtn.classList.add("hidden");
    els.bpmEditor.classList.remove("hidden");
    els.bpmInput.focus();
    els.bpmInput.select();
  });

  els.bpmCancel.addEventListener("click", () => {
    els.bpmEditor.classList.add("hidden");
    els.bpmBtn.classList.remove("hidden");
  });

  els.bpmEditor.addEventListener("submit", (event) => {
    event.preventDefault();
    const parsed = Number(els.bpmInput.value);
    if (Number.isFinite(parsed)) {
      sendCommand({ set_link_tempo_manual: clampBpm(parsed) });
    }
    els.bpmEditor.classList.add("hidden");
    els.bpmBtn.classList.remove("hidden");
  });

  els.offsetSlider.addEventListener("pointerdown", beginOffsetDrag);
  els.offsetSlider.addEventListener("touchstart", beginOffsetDrag, { passive: true });
  els.offsetSlider.addEventListener("pointerup", endOffsetDrag);
  els.offsetSlider.addEventListener("pointercancel", endOffsetDrag);
  els.offsetSlider.addEventListener("touchend", endOffsetDrag);
  els.offsetSlider.addEventListener("input", (event) => {
    applyOffsetValue(event.target.value, false);
  });
  els.offsetSlider.addEventListener("change", (event) => {
    applyOffsetValue(event.target.value, true);
    endOffsetDrag();
  });

  els.offsetReset.addEventListener("click", () => {
    isDraggingOffset = false;
    applyOffsetValue(0, true);
  });

  els.offsetMinus.addEventListener("click", () => {
    beginOffsetDrag();
    const base = Number(els.offsetSlider.value);
    applyOffsetValue(base - 0.5, true);
    endOffsetDrag();
  });

  els.offsetPlus.addEventListener("click", () => {
    beginOffsetDrag();
    const base = Number(els.offsetSlider.value);
    applyOffsetValue(base + 0.5, true);
    endOffsetDrag();
  });

  for (const btn of els.quantumButtons) {
    btn.addEventListener("click", () => {
      const quantum = Number(btn.dataset.quantum);
      if (Number.isFinite(quantum) && quantum > 0) {
        sendCommand({ set_quantum: quantum });
      }
    });
  }

  els.midiInSelect.addEventListener("change", () => {
    const outputs = latestState ? latestState.midi_output_matches || [] : [];
    sendMidiPorts(els.midiInSelect.value, outputs);
  });

  // Page Visibility / iOS Lock-Unlock / Network Resume silent recovery
  document.addEventListener(
    "pointerdown",
    () => {
      if (!wakeLockSentinel) {
        void requestScreenWakeLock();
      }
    },
    { passive: true }
  );

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      paused = true;
      clearReconnectTimer();
      closeSocketCleanly();
      setConnectionStatus("offline");
      void releaseScreenWakeLock();
    } else {
      paused = false;
      reconnectAttempt = 0;
      scheduleReconnect(true);
      void requestScreenWakeLock();
    }
  });

  window.addEventListener("pageshow", () => {
    if (!document.hidden) {
      paused = false;
      reconnectAttempt = 0;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        scheduleReconnect(true);
      }
      void requestScreenWakeLock();
    }
  });

  window.addEventListener("online", () => {
    if (!document.hidden) {
      paused = false;
      reconnectAttempt = 0;
      scheduleReconnect(true);
    }
  });

  window.addEventListener("offline", () => {
    stopPhaseLoop(true);
    setConnectionStatus("offline");
  });

  setInterval(() => {
    if (
      !paused &&
      !document.hidden &&
      ws &&
      ws.readyState === WebSocket.OPEN &&
      lastMessageAt > 0 &&
      Date.now() - lastMessageAt > STALE_TIMEOUT_MS
    ) {
      closeSocketCleanly();
      scheduleReconnect(true);
    }
  }, WATCHDOG_INTERVAL_MS);

  rebuildBeatStrip(4);
  connectWebSocket();
  void requestScreenWakeLock();
})();
