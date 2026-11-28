---
name: web-ui-pwa
description: >-
  Embedded mobile Web UI (iOS Home Screen PWA) cache-busting, low-latency
  metronome synchronization, tac•tus brand/icon tokens, and headless Chrome
  SVG/screenshot rendering rules. Use when editing crates/tactusd/web/,
  crates/tactusd/src/web.rs, or brand/UI assets in docs/images/.
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

## 3. Brand Identity, Icons & Asset Hygiene
- **Wordmark & Header:** Lowercase `tac•tus` (`tac<span id="brand-dot" class="brand-dot"></span>tus`) where the central amber `•` replaces any left brand dot and pulses on Beat 1 while playing (`0 ms` attack, `90 ms` decay); clear `.downbeat` on stop/disconnect.
- **Palette:** Obsidian `#0d0f14`, card `#151922`, surface `#1b212e`, border `#232a38`, warm amber `#f59e0b` (downbeat / `•`), electric teal `#2dd4bf` (beats / active controls), off-white `#eef2f7`.
- **Icons (Precision Dial):** Keep `crates/tactusd/web/icon.svg` rounded (`rx="36"`) for browser favicons, but render `crates/tactusd/web/apple-touch-icon.png` (`180×180`) from a **full-bleed square (`rx="0"`)** SVG source so iOS squircle masking shows no double border. Never leave extra or temporary files in `crates/tactusd/web/` (`web::tests::ui_asset_hash_covers_every_file_in_web_dir` enforces the exact embedded file list).
- **Copy & Notices:** Use `LINK CLOCK` (not `LINK MASTER`) for the `Link → MIDI` status badge, nominative `"for Ableton Link"` in `manifest.webmanifest` and banners, and preserve `<footer class="app-footer">` with the Ableton AG non-affiliation/trademark notice.

## 4. Rendering Banners & Capturing UI Screenshots (macOS)
- **Avoid `qlmanage -t` for non-square SVGs** (it forces a square viewport and clips wide banners) and note `sips`/`ffmpeg` cannot decode SVGs. Use Google Chrome headless instead (requires `BypassSandbox: true` on macOS due to Crashpad/AppKit sandbox extensions):
  - `docs/images/banner.png`: `--headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 --force-device-scale-factor=2 --window-size=1280,360` (`2560×720`)
  - `docs/images/social-preview.png`: `--force-device-scale-factor=1 --window-size=1280,640` (`1280×640`)
  - `crates/tactusd/web/apple-touch-icon.png`: `--force-device-scale-factor=1 --window-size=180,180` (`180×180`)
- **Offline `docs/images/ui-*.png` Capture (`780×1688` = `390×844` `@2x`):** Desktop Chrome headless enforces a ~500 px minimum layout width and does not reliably apply `window.scrollTo` before `--screenshot`. Build self-contained scratch HTML fixtures from the real `crates/tactusd/web/{index.html,style.css,app.js}` with:
  1. CSS overrides pinning `html, body { width: 390px !important; height: 844px !important; overflow: hidden !important; }` and `.app { width: 390px !important; max-width: 390px !important; margin: 0 !important; }`.
  2. A mocked `WebSocket` pushing the target `AppState` snapshot plus a no-op `requestAnimationFrame` so beat/phase bars freeze at the desired position.
  3. For bottom-scrolled views (`ui-midi-ports.png`), shift `.app` via `app.style.transform = 'translateY(-' + Math.max(0, app.scrollHeight - 844) + 'px)'` instead of `window.scrollTo`.
- **Verification:** Always confirm dimensions via `sips -g pixelWidth -g pixelHeight` and visually inspect every rendered PNG with `read_file` to catch clipping or typography drift.
