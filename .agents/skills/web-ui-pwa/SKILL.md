---
name: web-ui-pwa
description: >-
  Embedded mobile Web UI (iOS Home Screen PWA) cache-busting and low-latency
  metronome synchronization rules for tactusd.
  Use when editing crates/tactusd/web/ or crates/tactusd/src/web.rs.
---

# Embedded Web UI & Metronome Rules (iOS Home Screen PWA)

## 1. Cache-Busting (Isolated iOS WebKit Cache)
- Serve `/`, `/style.css`, `/app.js`, and `/manifest.webmanifest` with:
  - `Cache-Control: no-store, no-cache, must-revalidate`
  - `Pragma: no-cache`
  - `Expires: 0`
- Hash **all** embedded assets (including `icon.svg` and `apple-touch-icon.png`) in `ui_asset_hash()`.
- Version `/style.css`, `/app.js`, `/manifest.webmanifest`, and icon URLs with `?v=<ui_hash>` in both `index.html` and rendered `/manifest.webmanifest`.
- Preserve the `sessionStorage`-guarded `/healthz` `ui_hash` auto-reload in `crates/tactusd/web/app.js`.

## 2. Low-Latency Metronome Sync
- Wake `rt_notify_tx` on every integer `beat_index` crossing, discrete state change, and `10 Hz` heartbeat (never 24 PPQN).
- Keep `TCP_NODELAY` enabled on `axum` via `ListenerExt::tap_io`.
- Drive the UI phase bar via `requestAnimationFrame` + GPU `transform: scaleX(...)` with `0 ms` attack on `.beat-cell.active`.
