(() => {
  "use strict";

  const BACKOFF_MS = [250, 500, 1000, 2000];
  const STALE_TIMEOUT_MS = 6000;
  const WATCHDOG_INTERVAL_MS = 2000;
  const DRAG_SETTLE_MS = 250;
  const OFFSET_THROTTLE_MS = 50;

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
    resyncBtn: document.getElementById("resync-btn"),
    dirButtons: Array.from(document.querySelectorAll("[data-dir]")),
    bpmStepButtons: Array.from(document.querySelectorAll("[data-bpm-delta]")),
    quantumButtons: Array.from(document.querySelectorAll("[data-quantum]")),
    quantumCustom: document.getElementById("quantum-custom"),
    startStopToggle: document.getElementById("start-stop-toggle"),
    offsetReadout: document.getElementById("offset-readout"),
    offsetReset: document.getElementById("offset-reset"),
    offsetMinus: document.getElementById("offset-minus"),
    offsetPlus: document.getElementById("offset-plus"),
    offsetSlider: document.getElementById("offset-slider"),
    midiInSelect: document.getElementById("midi-in"),
    midiOutSelect: document.getElementById("midi-out"),
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
  let lastInSignature = null;
  let lastOutSignature = null;
  let resyncTimer = null;

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

  function formatPortLabel(port) {
    const card = port.card_name.trim();
    const name = port.port_name.trim();
    if (!card || name.toLowerCase().startsWith(card.toLowerCase())) {
      return `${name || port.id} (${port.id})`;
    }
    return `${card} — ${name} (${port.id})`;
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
  }

  function renderPortSelect(selectEl, statusEl, isInput, state) {
    const allPorts = state.available_midi_ports || [];
    const eligible = allPorts.filter((p) => (isInput ? p.can_read : p.can_write));
    const selectedId = isInput ? state.selected_midi_input : state.selected_midi_output;
    const matchRule = (isInput ? state.midi_input_match : state.midi_output_match) || "";

    if (selectedId) {
      statusEl.textContent = `Attached (${selectedId})`;
      statusEl.className = "port-status port-ok";
    } else if (matchRule) {
      statusEl.textContent = `Waiting for "${matchRule}"`;
      statusEl.className = "port-status port-wait";
    } else {
      statusEl.textContent = "Disabled";
      statusEl.className = "port-status port-off";
    }

    const sig = JSON.stringify({
      eligible: eligible.map((p) => [p.id, p.card_name, p.port_name]),
      selectedId,
      matchRule,
    });
    const prevSig = isInput ? lastInSignature : lastOutSignature;
    if (sig === prevSig) {
      return;
    }
    if (isInput) {
      lastInSignature = sig;
    } else {
      lastOutSignature = sig;
    }

    selectEl.innerHTML = "";

    const noneOpt = document.createElement("option");
    noneOpt.value = "";
    noneOpt.textContent = "Disabled (None)";
    selectEl.appendChild(noneOpt);

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
      selectEl.appendChild(opt);
    }

    if (!matchedOptionFound && matchRule) {
      const waitOpt = document.createElement("option");
      waitOpt.value = matchRule;
      waitOpt.textContent = `Waiting for "${matchRule}" (unplugged)`;
      selectEl.appendChild(waitOpt);
      chosenValue = matchRule;
    }

    selectEl.value = chosenValue;
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

    const beatCount = Math.max(1, Math.min(16, Math.round(t.quantum || 4)));
    if (beatCount !== lastRenderedBeats) {
      rebuildBeatStrip(beatCount);
    }
    const wrappedPhase = ((t.beat_phase % beatCount) + beatCount) % beatCount;
    const activeIdx = Math.floor(wrappedPhase);
    const cells = els.beatStrip.children;
    for (let i = 0; i < cells.length; i += 1) {
      const cell = cells[i];
      cell.classList.toggle("active", i === activeIdx);
      cell.classList.toggle("downbeat", i === 0 && activeIdx === 0);
    }
    const progressPct = Math.max(0, Math.min(100, (wrappedPhase / beatCount) * 100));
    els.phaseBarFill.style.width = `${progressPct.toFixed(1)}%`;

    for (const btn of els.dirButtons) {
      btn.classList.toggle("active", btn.dataset.dir === t.direction);
    }

    els.startStopToggle.setAttribute("aria-checked", String(Boolean(t.start_stop_sync)));

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
    renderPortSelect(els.midiInSelect, els.midiInStatus, true, state);
    renderPortSelect(els.midiOutSelect, els.midiOutStatus, false, state);
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
    els.startStopToggle.setAttribute("aria-checked", String(next));
    sendCommand({ set_start_stop_sync: next });
  });

  els.resyncBtn.addEventListener("click", () => {
    sendCommand("resync_downbeat");
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
    const output = latestState ? latestState.midi_output_match || "" : "";
    sendCommand({
      select_midi_ports: {
        input: els.midiInSelect.value,
        output,
      },
    });
  });

  els.midiOutSelect.addEventListener("change", () => {
    const input = latestState ? latestState.midi_input_match || "" : "";
    sendCommand({
      select_midi_ports: {
        input,
        output: els.midiOutSelect.value,
      },
    });
  });

  // Page Visibility / iOS Lock-Unlock / Network Resume silent recovery
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      paused = true;
      clearReconnectTimer();
      closeSocketCleanly();
      setConnectionStatus("offline");
    } else {
      paused = false;
      reconnectAttempt = 0;
      scheduleReconnect(true);
    }
  });

  window.addEventListener("pageshow", () => {
    if (!document.hidden) {
      paused = false;
      reconnectAttempt = 0;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        scheduleReconnect(true);
      }
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
})();
