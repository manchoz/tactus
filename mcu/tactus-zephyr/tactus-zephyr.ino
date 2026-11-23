// SPDX-License-Identifier: MPL-2.0
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Tactus UNO Q STM32U585 Zephyr LED Matrix Companion Sketch
//
// Subscribes to low-rate state and bar-downbeat notifications from `tactusd`
// over `Arduino_RouterBridge` (never 24 PPQN MIDI clock ticks) and renders
// direction, lock status, play/stop state, and a smooth 140 ms downbeat
// heartbeat pulse on the onboard 8x13 LED matrix using 3-bit grayscale (0..7).

#include <Arduino.h>
#include <Arduino_LED_Matrix.h>
#include <Arduino_RouterBridge.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

// Uncomment (or compile with -DTACTUS_ENABLE_BUTTONS) for v2 hardware with
// physical momentary switches wired active-low to GND. Keep disabled on stock
// UNO Q boards so floating D2/D3 pins never emit spurious RPC notifications.
// #define TACTUS_ENABLE_BUTTONS

#ifdef TACTUS_ENABLE_BUTTONS
#ifndef TACTUS_PIN_BTN_RESYNC
#define TACTUS_PIN_BTN_RESYNC 2  // D2 -> GND (active-low INPUT_PULLUP)
#endif
#ifndef TACTUS_PIN_BTN_DIRECTION
#define TACTUS_PIN_BTN_DIRECTION 3  // D3 -> GND (active-low INPUT_PULLUP)
#endif
#endif

static constexpr uint8_t MATRIX_ROWS = 8;
static constexpr uint8_t MATRIX_COLS = 13;
static constexpr size_t MATRIX_PIXELS = static_cast<size_t>(MATRIX_ROWS) * static_cast<size_t>(MATRIX_COLS);
static constexpr uint8_t GRAYSCALE_BITS = 3;
static constexpr uint8_t MAX_GRAY = 7;

static constexpr uint8_t DIR_LINK_TO_MIDI = 0;
static constexpr uint8_t DIR_MIDI_TO_LINK = 1;

static constexpr uint8_t LOCK_UNLOCKED = 0;
static constexpr uint8_t LOCK_ACQUIRING = 1;
static constexpr uint8_t LOCK_LOCKED = 2;

static constexpr int32_t MIN_BPM_TENTHS = 200;   // 20.0 BPM
static constexpr int32_t MAX_BPM_TENTHS = 9990;  // 999.0 BPM

static constexpr uint32_t DOWNBEAT_DECAY_MS = 140UL;
static constexpr uint32_t ACQUIRING_PULSE_MS = 800UL;
static constexpr uint32_t STANDBY_PULSE_MS = 1600UL;
static constexpr uint32_t ANIM_INTERVAL_MS = 10UL;

static constexpr uint8_t ARROW_BRIGHT_STOPPED = 3;
static constexpr uint8_t ARROW_BRIGHT_PLAYING = 5;
static constexpr uint8_t PLAY_CORNER_IDLE = 1;
static constexpr uint8_t PLAY_BADGE_BASE = 5;
static constexpr uint8_t STOP_BADGE_DIM = 1;

Arduino_LED_Matrix matrix;

static uint8_t s_frame[MATRIX_PIXELS] = {0};
static uint8_t s_last_drawn_frame[MATRIX_PIXELS] = {0};
static bool s_has_drawn_once = false;
static uint32_t s_last_anim_ms = 0;

// Shared state updated by Bridge.provide_safe callbacks on the loop() thread.
static volatile int32_t s_bpm_tenths = 1200;
static volatile uint8_t s_direction = DIR_LINK_TO_MIDI;
static volatile uint8_t s_lock_state = LOCK_UNLOCKED;
static volatile bool s_is_playing = false;
static volatile bool s_has_state = false;
static volatile bool s_downbeat_active = false;
static volatile uint32_t s_downbeat_start_ms = 0;
static volatile bool s_state_dirty = true;

static inline void apply_shared_state(int bpm_tenths, int direction, int lock_state, bool is_playing) {
  int32_t clamped_bpm = static_cast<int32_t>(bpm_tenths);
  if (clamped_bpm < MIN_BPM_TENTHS) {
    clamped_bpm = MIN_BPM_TENTHS;
  } else if (clamped_bpm > MAX_BPM_TENTHS) {
    clamped_bpm = MAX_BPM_TENTHS;
  }

  uint8_t norm_dir = (direction == static_cast<int>(DIR_MIDI_TO_LINK)) ? DIR_MIDI_TO_LINK : DIR_LINK_TO_MIDI;

  uint8_t norm_lock = LOCK_UNLOCKED;
  if (lock_state == static_cast<int>(LOCK_LOCKED)) {
    norm_lock = LOCK_LOCKED;
  } else if (lock_state == static_cast<int>(LOCK_ACQUIRING)) {
    norm_lock = LOCK_ACQUIRING;
  }

  s_bpm_tenths = clamped_bpm;
  s_direction = norm_dir;
  s_lock_state = norm_lock;
  s_is_playing = is_playing;
  s_has_state = true;
  s_state_dirty = true;
}

void on_tactus_state(int bpm_tenths, int direction, int lock_state, bool is_playing) {
  apply_shared_state(bpm_tenths, direction, lock_state, is_playing);
  if (!is_playing) {
    s_downbeat_active = false;
  }
}

void on_tactus_downbeat(int bpm_tenths, int direction, int lock_state, bool is_playing) {
  apply_shared_state(bpm_tenths, direction, lock_state, is_playing);
  if (is_playing) {
    s_downbeat_start_ms = millis();
    s_downbeat_active = true;
  } else {
    s_downbeat_active = false;
  }
}

static inline void set_pixel(uint8_t *frame, uint8_t row, uint8_t col, uint8_t level) {
  if (row >= MATRIX_ROWS || col >= MATRIX_COLS) {
    return;
  }
  uint8_t clamped = (level > MAX_GRAY) ? MAX_GRAY : level;
  size_t idx = static_cast<size_t>(row) * static_cast<size_t>(MATRIX_COLS) + static_cast<size_t>(col);
  if (clamped > frame[idx]) {
    frame[idx] = clamped;
  }
}

static uint8_t pulse_triangle(uint32_t now_ms, uint32_t period_ms, uint8_t min_level, uint8_t max_level) {
  if (period_ms < 2UL || min_level >= max_level) {
    return (min_level > MAX_GRAY) ? MAX_GRAY : min_level;
  }
  uint32_t half = period_ms / 2UL;
  uint32_t phase = now_ms % period_ms;
  uint32_t pos = (phase < half) ? phase : (period_ms - phase);
  uint32_t span = static_cast<uint32_t>(max_level - min_level);
  uint32_t level = static_cast<uint32_t>(min_level) + ((pos * span + (half / 2UL)) / half);
  if (level > MAX_GRAY) {
    level = MAX_GRAY;
  }
  return static_cast<uint8_t>(level);
}

static uint8_t compute_downbeat_level(uint32_t now_ms) {
  if (!s_downbeat_active) {
    return 0;
  }
  uint32_t elapsed = now_ms - s_downbeat_start_ms;
  if (elapsed >= DOWNBEAT_DECAY_MS) {
    // Clear active flag immediately to avoid millis() 49.7-day rollover phantom flashes.
    s_downbeat_active = false;
    return 0;
  }
  uint32_t drop = (elapsed * static_cast<uint32_t>(MAX_GRAY)) / DOWNBEAT_DECAY_MS;
  return static_cast<uint8_t>(static_cast<uint32_t>(MAX_GRAY) - drop);
}

static void draw_border(uint8_t *frame, uint8_t edge_level, uint8_t corner_level) {
  if (edge_level > 0) {
    for (uint8_t col = 0; col < MATRIX_COLS; ++col) {
      set_pixel(frame, 0, col, edge_level);
      set_pixel(frame, MATRIX_ROWS - 1, col, edge_level);
    }
    for (uint8_t row = 1; row + 1 < MATRIX_ROWS; ++row) {
      set_pixel(frame, row, 0, edge_level);
      set_pixel(frame, row, MATRIX_COLS - 1, edge_level);
    }
  }
  if (corner_level > 0) {
    set_pixel(frame, 0, 0, corner_level);
    set_pixel(frame, 0, MATRIX_COLS - 1, corner_level);
    set_pixel(frame, MATRIX_ROWS - 1, 0, corner_level);
    set_pixel(frame, MATRIX_ROWS - 1, MATRIX_COLS - 1, corner_level);
  }
}

static void draw_direction_arrow(uint8_t *frame, uint8_t direction, uint8_t level) {
  for (uint8_t col = 3; col <= 9; ++col) {
    set_pixel(frame, 3, col, level);
  }
  if (direction == DIR_LINK_TO_MIDI) {
    // Right arrow (Link -> MIDI)
    set_pixel(frame, 1, 7, level);
    set_pixel(frame, 2, 8, level);
    set_pixel(frame, 4, 8, level);
    set_pixel(frame, 5, 7, level);
  } else {
    // Left arrow (MIDI -> Link)
    set_pixel(frame, 1, 5, level);
    set_pixel(frame, 2, 4, level);
    set_pixel(frame, 4, 4, level);
    set_pixel(frame, 5, 5, level);
  }
}

static void draw_lock_indicator(uint8_t *frame, uint8_t direction, uint8_t lock_state, bool is_playing,
                                uint32_t now_ms) {
  if (direction == DIR_LINK_TO_MIDI) {
    // LinkToMidi has no external clock estimator lock; indicate Link clock ready/active.
    uint8_t lvl = is_playing ? 6 : 4;
    set_pixel(frame, 6, 2, lvl);
    set_pixel(frame, 6, 3, lvl);
    return;
  }

  if (lock_state == LOCK_LOCKED) {
    set_pixel(frame, 6, 2, 6);
    set_pixel(frame, 6, 3, 6);
  } else if (lock_state == LOCK_ACQUIRING) {
    uint8_t pulse = pulse_triangle(now_ms, ACQUIRING_PULSE_MS, 1, 6);
    set_pixel(frame, 6, 2, pulse);
    set_pixel(frame, 6, 3, pulse);
  } else {
    set_pixel(frame, 6, 2, 1);
  }
}

static void draw_play_indicator(uint8_t *frame, bool is_playing, uint8_t pulse_level) {
  if (is_playing) {
    uint8_t lvl = static_cast<uint8_t>(PLAY_BADGE_BASE + ((pulse_level + 1U) / 2U));
    if (lvl > MAX_GRAY) {
      lvl = MAX_GRAY;
    }
    set_pixel(frame, 6, 9, lvl);
    set_pixel(frame, 6, 10, lvl);
  } else {
    set_pixel(frame, 6, 10, STOP_BADGE_DIM);
  }
}

static void draw_standby(uint8_t *frame, uint32_t now_ms) {
  uint8_t outer = pulse_triangle(now_ms, STANDBY_PULSE_MS, 1, 4);
  uint8_t core = (outer < MAX_GRAY) ? static_cast<uint8_t>(outer + 1U) : MAX_GRAY;
  set_pixel(frame, 3, 5, outer);
  set_pixel(frame, 3, 6, core);
  set_pixel(frame, 3, 7, outer);
}

static void render_frame(uint32_t now_ms) {
  bool has_state = s_has_state;
  uint8_t direction = s_direction;
  uint8_t lock_state = s_lock_state;
  bool is_playing = s_is_playing;

  memset(s_frame, 0, sizeof(s_frame));

  if (!has_state) {
    draw_standby(s_frame, now_ms);
  } else {
    uint8_t pulse_level = is_playing ? compute_downbeat_level(now_ms) : 0;
    uint8_t corner_level = is_playing ? ((pulse_level > PLAY_CORNER_IDLE) ? pulse_level : PLAY_CORNER_IDLE) : 0;
    draw_border(s_frame, pulse_level, corner_level);

    uint8_t arrow_base = is_playing ? ARROW_BRIGHT_PLAYING : ARROW_BRIGHT_STOPPED;
    uint8_t arrow_level = static_cast<uint8_t>(arrow_base + ((pulse_level + 1U) / 2U));
    if (arrow_level > MAX_GRAY) {
      arrow_level = MAX_GRAY;
    }
    draw_direction_arrow(s_frame, direction, arrow_level);
    draw_lock_indicator(s_frame, direction, lock_state, is_playing, now_ms);
    draw_play_indicator(s_frame, is_playing, pulse_level);
  }

  if (!s_has_drawn_once || memcmp(s_frame, s_last_drawn_frame, MATRIX_PIXELS) != 0) {
    memcpy(s_last_drawn_frame, s_frame, MATRIX_PIXELS);
    s_has_drawn_once = true;
    matrix.draw(s_frame);
  }
}

#ifdef TACTUS_ENABLE_BUTTONS
static constexpr uint32_t BUTTON_DEBOUNCE_MS = 30UL;

struct DebouncedButton {
  uint8_t pin;
  const char *rpc_method;
  bool stable_pressed;
  bool last_raw_pressed;
  uint32_t last_change_ms;
};

// Optional v2 tactile buttons. Additional supported MPU RPC commands:
// "tactus/cmd/toggle_play", "tactus/cmd/set_direction" (0|1),
// "tactus/cmd/set_playing" (bool), "tactus/cmd/adjust_bpm" (delta), "tactus/cmd/set_bpm" (bpm).
static DebouncedButton s_buttons[] = {
    {TACTUS_PIN_BTN_RESYNC, "tactus/cmd/resync_downbeat", false, false, 0},
    {TACTUS_PIN_BTN_DIRECTION, "tactus/cmd/toggle_direction", false, false, 0},
};

static void init_buttons() {
  uint32_t now_ms = millis();
  size_t count = sizeof(s_buttons) / sizeof(s_buttons[0]);
  for (size_t i = 0; i < count; ++i) {
    pinMode(s_buttons[i].pin, INPUT_PULLUP);
    bool pressed = (digitalRead(s_buttons[i].pin) == LOW);
    s_buttons[i].stable_pressed = pressed;
    s_buttons[i].last_raw_pressed = pressed;
    s_buttons[i].last_change_ms = now_ms;
  }
}

static void poll_buttons(uint32_t now_ms) {
  size_t count = sizeof(s_buttons) / sizeof(s_buttons[0]);
  for (size_t i = 0; i < count; ++i) {
    DebouncedButton &btn = s_buttons[i];
    bool raw_pressed = (digitalRead(btn.pin) == LOW);
    if (raw_pressed != btn.last_raw_pressed) {
      btn.last_raw_pressed = raw_pressed;
      btn.last_change_ms = now_ms;
      continue;
    }
    if ((now_ms - btn.last_change_ms) < BUTTON_DEBOUNCE_MS) {
      continue;
    }
    if (raw_pressed != btn.stable_pressed) {
      btn.stable_pressed = raw_pressed;
      if (btn.stable_pressed) {
        Bridge.notify(btn.rpc_method);
      }
    }
  }
}
#else
static inline void init_buttons() {}
static inline void poll_buttons(uint32_t /*now_ms*/) {}
#endif

void setup() {
  matrix.begin();
  matrix.setGrayscaleBits(GRAYSCALE_BITS);
  matrix.clear();

  init_buttons();
  render_frame(millis());

  Bridge.begin();
  Bridge.provide_safe("tactus/state", on_tactus_state);
  Bridge.provide_safe("tactus/downbeat", on_tactus_downbeat);
}

void loop() {
  uint32_t now_ms = millis();

  poll_buttons(now_ms);

  bool dirty = s_state_dirty;
  bool animating = !s_has_state || s_downbeat_active ||
                   (s_direction == DIR_MIDI_TO_LINK && s_lock_state == LOCK_ACQUIRING);

  if (dirty || (animating && (now_ms - s_last_anim_ms) >= ANIM_INTERVAL_MS)) {
    s_state_dirty = false;
    s_last_anim_ms = now_ms;
    render_frame(now_ms);
  }

  yield();
}
