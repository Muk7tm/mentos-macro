# Mentos Macro

Keyboard-and-click sequence automation for Linux X11. By default it presses
`5 → click → 6 → click → 7 → click → 8 → click → 9 → click → 0 → click` in a loop
(each click is a left mouse click), with configurable timing. An optional step-based **Advanced Sequence Editor** lets you build
your own sequences of keys, key combinations, mouse buttons and waits.

Mentos Macro is a standalone project. It shares nothing (code, assets, configuration or
runtime) with any other application.

---

## Contents

1. [Requirements](#requirements)
2. [Building](#building)
3. [Installing and uninstalling](#installing-and-uninstalling)
4. [Using Mentos Macro](#using-mentos-macro)
5. [Advanced Sequence Editor](#advanced-sequence-editor)
6. [Timing model](#timing-model)
7. [Hotkeys](#hotkeys)
8. [Focus protection and safety](#focus-protection-and-safety)
9. [Input backends (XTest and uinput)](#input-backends)
10. [Configuration and profiles](#configuration-and-profiles)
11. [Sober / Roblox compatibility](#sober--roblox-compatibility)
12. [Testing](#testing)
13. [Limitations](#limitations)
14. [Project layout](#project-layout)

---

## Requirements

* Linux x86-64 with an **X11** session. Wayland sessions are not supported: under XWayland,
  global hotkeys and synthetic input reach at most other XWayland applications.
* An X server with the XTEST extension (every standard Xorg server has it).
* Runtime libraries: `libxkbcommon`, `fontconfig`, `freetype`, and OpenGL (Mesa). If
  OpenGL is unavailable, start with `SLINT_BACKEND=winit-software` to use the software
  renderer.
* To build: Rust 1.85 or newer (`rustup` or your distribution's package) and a C toolchain.

## Building

```sh
cd mentos-macro
cargo build --release --locked
./target/release/mentos-macro
```

The first build compiles the Slint UI toolkit and takes a few minutes. The binary has no
runtime dependency on this source tree; the fonts and icon are embedded.

## Installing and uninstalling

```sh
./packaging/install.sh            # installs to ~/.local (binary, icon, desktop entry)
PREFIX=/usr/local sudo -E ./packaging/install.sh   # system-wide
./packaging/uninstall.sh          # removes the program, keeps settings
./packaging/uninstall.sh --purge  # also deletes ~/.config/mentos-macro
```

`install.sh` uses a prebuilt `dist/mentos-macro` if present, otherwise it builds one.
Only files named `mentos-macro*` are installed.

## Using Mentos Macro

The main window shows only:

* the **sequence preview** (the default sequence, or your custom sequence when enabled);
* **Loop**: on repeats until stopped; off runs the sequence once;
* **Timing**: the Fast / Balanced / Stable / Custom preset, plus a slider for the delay
  between keys;
* **Start / Stop** (also on the hotkey, **F6** by default);
* the small **settings** button.

Typical use: press **F6** while the game window is focused and the mouse pointer is over
it (the clicks go to the window under the pointer). Press **F6** again to stop.
**F9** is an emergency stop. If you click **Start** in the window instead, Mentos waits
until you focus another window, then locks onto it.

Settings are grouped in tabs:

| Tab      | Contents |
|----------|----------|
| General  | Preset, key hold, delay between keys, delay between loops, modifier lead, loop, repetitions (0 = until stopped), start delay, activation (toggle / hold to run), reset to defaults |
| Hotkeys  | Start/stop and emergency-stop hotkeys, typed or captured |
| Input    | Backend (XTest / uinput), focus protection, stop or pause on focus loss, optional target window class with **Detect** |
| Profiles | Save / load / rename / duplicate / delete sequence profiles, import and export |
| Advanced | Use custom sequence (off by default), open the sequence editor |

Changes are saved automatically. Changing settings while a macro runs does not affect
that run; the new settings apply from the next start.

## Advanced Sequence Editor

*Settings → Advanced → Edit sequence.* The editor is disabled by default. When **Use
custom sequence** is off, the default sequence runs. Turning it off never deletes your
custom steps.

The editor shows the steps as a list. Select a step to edit it below the list. **Add step**
inserts after the selection; **Duplicate**, **Up**, **Down** and **Delete** act on the
selected step; **Reset** restores the default sequence; **Preview** shows the exact
execution order with timestamps.

| Action      | What it does |
|-------------|--------------|
| Press key   | Key or combination down, held for *key hold*, then up |
| Key down    | Presses and keeps holding (must be released by a later *Key up*) |
| Key up      | Releases a key pressed by an earlier *Key down* |
| Hold key    | Down, held for the step's own duration, then up |
| Click mouse | Mouse button down, held for *key hold*, then up |
| Mouse down / Mouse up | Press / release a mouse button |
| Hold mouse  | Mouse button held for the step's duration |
| Wait        | Explicit pause |

Each Press/Click step can override the global hold time, and every step except Wait can
override the delay after it ("Own hold time", "Own delay after"). Overridden values are
marked with `*` in the list.

**Key capture:** click **Capture** and press the key or combination. While capturing,
the keyboard is grabbed, so the application's own hotkeys cannot fire. Esc cancels (type
`Escape` if you need the Esc key itself). You can also type key names:

* letters and digits: `E`, `5`; punctuation: `-`, `=`, `[`, `;`, `'`, `,`, `.`, `/`, `` ` ``
* `Tab`, `Enter`, `Space`, `Esc`, `Backspace`, `Delete`, `Insert`, `Home`, `End`,
  `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `PrintScreen`, `Pause`, `Menu`
* `F1` … `F24`
* numpad: `Num0` … `Num9`, `Num+`, `Num-`, `Num*`, `Num/`, `Num.`, `NumEnter`
* modifiers on their own: `Shift`, `RShift`, `Ctrl`, `RCtrl`, `Alt`, `RAlt`, `AltGr`, `Super`
* combinations: `Shift+E`, `Ctrl+Alt+F5`, `Ctrl+Num+`
* any X11 keysym name (`KP_Add`, `Prior`) or a raw keysym in hex (`0xffbe`)

**Validation** runs continuously and before every start. A sequence cannot run if it:
has no steps, has a missing key/button/duration, has a duration out of range, releases a
key that was never pressed, presses a key that is already held, leaves a key or button
held at the end, uses a key that is one of the app's hotkeys, or (when looping) would
complete a cycle in under 5 ms. Problem steps are marked in the list.

The example from the specification looks like this as steps:

```text
1  Press F
2  Wait 50 ms
3  Press Shift+E
4  Wait 100 ms
5  Key down R
6  Wait 250 ms
7  Key up R
8  Press Tab            (repeats if Loop is on)
```

Steps 5 to 7 could also be one *Hold key R for 250 ms* step.

## Timing model

For each step: `[modifiers down → modifier lead] → key down → hold → key up → [modifiers up] → delay after`.
The delay after the **last** step is the **delay between loops** (unless the last step
overrides its delay). With Balanced timing (25 ms hold, 15 ms gap, 40 ms loop) one cycle of
the default sequence takes 505 ms.

| Preset   | Key hold | Delay between keys | Delay between loops |
|----------|---------:|-------------------:|--------------------:|
| Fast     | 15 ms    | 8 ms               | 20 ms               |
| Balanced | 25 ms    | 15 ms              | 40 ms               |
| Stable   | 40 ms    | 30 ms              | 80 ms               |

Presets are starting points for calibration, not guaranteed optimal values for any game.
Very short holds can be missed by games that poll input once per frame (16.7 ms at 60 FPS),
so test faster settings in the game itself.

Implementation: a single long-lived worker thread runs a precompiled, immutable plan. Waits
use monotonic deadlines (the schedule absorbs wake-up overshoot instead of accumulating
drift) and the thread's timer slack is reduced to 1 µs. If the system stalls by more than
1 ms, the schedule is re-anchored rather than firing overdue events in a burst, so holds
are never shortened. Every XTest event is a checked request: the worker continues only
after the X server has processed it. Waits are `recv_timeout` calls on the command
channel, so stop commands are handled within microseconds, even during long holds. Idle
CPU use is zero.

Measured on the development machine (X server timestamps, 1 ms resolution, Balanced
preset, both backends, default sequence with clicks): key and click holds 24–26 ms, gaps
14–17 ms (the server clock has 1 ms resolution).

## Hotkeys

Defaults: **F6** start/stop, **F9** emergency stop. Change them under *Settings → Hotkeys*,
by typing (`F8`, `Ctrl+F6`) or capturing. Hotkeys work regardless of CapsLock, NumLock and
ScrollLock.

* **Toggle** mode: press to start, press again to stop.
* **Hold to run** mode: runs only while the start key is held down.
* **Emergency stop** stops in any mode and additionally sends a release for every key and
  button the running sequence can press.

How it works: hotkeys are passive X key grabs. When one fires, the X server would route
*all* keyboard input, including the macro's own events, to Mentos for as long as the key
is held. Mentos releases that grab immediately and detects the physical release by polling
the key-state bitmap, so the macro's keys reach the game even while the hotkey is still
held. This also makes key auto-repeat harmless.

If another application already owns a hotkey, the Hotkeys tab reports the conflict.
Prefer hotkeys without modifiers. A physically held Ctrl/Shift/Alt is combined by the game
with the macro's keys.

## Focus protection and safety

* **Focus protection** (on by default): the macro locks onto the window that is focused
  when it starts (after the start delay) and only sends input while that window is focused.
  Mouse events go to the window *under the pointer*, so a sequence that clicks (the default
  one does) additionally requires the pointer to stay over the target window; it waits for
  that before starting. If focus or the pointer moves away it **stops** (default) or
  **pauses** (releases everything and resumes from the start of the cycle when the window
  is back). Keyboard-only sequences do not care where the pointer is.
* **Target window** (optional): a WM_CLASS substring such as `sober`. With it set, the
  macro only starts on, and only types into, a matching window. **Detect** fills it in from
  the window you focus within 3 seconds. Find a class manually with `xprop WM_CLASS`.
* Mentos never sends input while its own window is focused, even with protection off.
* Every key and button the macro holds is tracked and released on stop, emergency stop,
  focus loss, errors, window close, `SIGINT`/`SIGTERM`/`SIGHUP`, and worker panics. A
  target window that is closed while the macro runs counts as a lost target, not an error.
* Only one instance can run at a time (lock file in `$XDG_RUNTIME_DIR`).
* Start requests while running are ignored, so there is never a second worker.

## Input backends

**XTest** (default) sends synthetic events through the XTEST extension. It needs no
permissions and no root.

**uinput** creates a virtual kernel keyboard/mouse ("Mentos Macro virtual input …").
Events look exactly like a physical device to the X server and to applications. Use it only
if a program ignores XTest input. It requires write access to `/dev/uinput`:

```sh
ls -l /dev/uinput                       # check current permissions
sudo cp packaging/60-mentos-macro-uinput.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger /dev/uinput
```

(or add yourself to a group that owns `/dev/uinput`, often `input`). Mentos creates the
device on the first uinput run and waits until the X server lists it before sending
anything. If that takes longer than 3 seconds the run fails with a clear message instead
of silently dropping keys.

Neither backend lets the application tell synthetic input from your own physical input.
For example, a key you hold physically stays held when the macro releases its copy.

## Configuration and profiles

* Settings: `$XDG_CONFIG_HOME/mentos-macro/config.toml` (default
  `~/.config/mentos-macro/config.toml`)
* Profiles: `~/.config/mentos-macro/profiles/*.toml`

The file is human-readable TOML with a `version` field. Writes are atomic (temp file,
fsync, rename). Missing values take defaults, out-of-range values are clamped, and an
unparsable file is renamed to `config.toml.corrupt-<time>` and replaced with defaults. The
status line reports this. *Reset settings to defaults* keeps your custom sequence and
profiles.

A **profile** stores a custom sequence together with its timing and loop settings. *Save*
writes the current custom sequence under a name; *Load* makes a profile the active custom
sequence (and enables it). *Export* writes the selected profile to a file or folder;
*Import* validates a file and adds it, renaming on name clashes. Profile format:

```toml
format = "mentos-sequence"
version = 1
name = "Example"

[timing]
key_ms = 25
gap_ms = 15
loop_ms = 40
modifier_ms = 5

[[steps]]
action = "press"          # press | key_down | key_up | hold | mouse_press |
key = "Shift+E"           # mouse_down | mouse_up | mouse_hold | wait
delay_ms = 100            # optional override

[[steps]]
action = "hold"
key = "R"
duration_ms = 250
```

## Sober / Roblox compatibility

**Status: not verified in a game.** Mentos delivers key events to X11 clients correctly:
order, hold times and focus handling are tested against a real X server, with both
backends (see [Testing](#testing)). Whether a particular game *acts* on synthetic input,
and at which minimum timings, can only be confirmed in the game. Successful event
submission is not proof of game compatibility.

Suggested check with Sober:

1. Start Sober and join an experience where the keys 5–0 and a left click have a visible
   effect.
2. In Mentos, set *Target window* via **Detect** while Sober is focused.
3. Start with the **Stable** preset and Loop off; press F6 in the game and confirm that
   every key in the sequence registered.
4. Switch to Balanced, then Fast, and lower the timings until inputs start to be missed.
   Keep a safety margin above that point.
5. If no input registers with XTest, try the uinput backend.

## Testing

```sh
cargo test                       # unit tests + engine behaviour tests (no X server needed)
cargo test --test x11_delivery -- --ignored --test-threads=1
```

The default tests (70) cover key parsing, the timing scheduler, validation and
compilation, configuration recovery, profiles, the editor model, and the execution engine
against a mock backend: order, hold/gap timing, stop latency during holds and waits, focus
loss (stop and pause), target locking, hotkey conflicts, single-worker guarantees,
hold-to-run, emergency release, snapshot isolation, start-delay cancellation, shutdown
cleanup, and the pointer rules for sequences that click.

The ignored `x11_delivery` tests need an X11 session. They open a test window, let the
window manager focus it, and verify what the X server actually delivers:

* the default sequence (keys and left clicks) with both backends, including server-side
  hold and gap timing;
* combinations and mouse holds;
* the full hotkey path (F6 starts, keys reach the target while F6 is held, F6 stops,
  everything released);
* closing the target window mid-hold (a clean "target lost" stop, not an error).

They use focus protection restricted to the test window's class, so nothing can reach
other windows. Don't touch the keyboard or mouse while they run (about 4 seconds).

## Limitations

* X11 only. No Wayland support.
* A `SIGKILL` (or power loss) cannot be intercepted. If the process is killed while it
  holds a key, that key may stay logically pressed until you press and release it.
* Numpad keys are sent as the physical keypad key. Their meaning (digit or navigation)
  depends on your NumLock state.
* Shifted symbols (e.g. `!`) are produced by adding Shift automatically, based on the
  current keyboard layout.
* The pointer check compares top-level windows, so a separate popup window of the game
  counts as "another window" for clicking sequences.
* Focus detection uses `_NET_ACTIVE_WINDOW`. This works with every EWMH window manager;
  without one, the X input focus is used.
* Running together with other macro tools works as long as hotkeys differ. Each
  application validates only its own hotkeys.

## Project layout

```text
src/
  main.rs            CLI entry, logging, instance lock, signal mask
  app.rs             UI controller (Slint ↔ engine/hotkeys/config/profiles)
  engine.rs          execution worker, state machine, cleanup
  timing.rs          deadline scheduler, timer slack
  sequence.rs        sequence model, validation, compilation, preview
  sequence_editor.rs editor model (UI independent)
  input.rs           keymap resolution, XTest and uinput backends
  hotkeys.rs         global hotkeys, key capture, window detection
  safety.rs          focus tracking, held-input tracking, lock file, signals
  keys.rs            keysyms, modifiers, key names
  config.rs          configuration, XDG paths, atomic save, recovery
  profiles.rs        sequence profiles, import/export
ui/                  theme.slint, widgets.slint, state.slint, main/settings/sequence_editor.slint
assets/              icon, embedded Noto Sans fonts (SIL OFL 1.1)
packaging/           install/uninstall scripts, desktop entry, uinput udev rule
tests/               engine behaviour tests, real-X11 delivery tests
```

License: MIT (see `LICENSE`). The bundled Noto Sans fonts are under the SIL Open Font
License 1.1 (`assets/fonts/OFL.txt`).
