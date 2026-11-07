//! End-to-end tests against a real X server. They are ignored by default because they
//! need an X11 session and briefly take keyboard focus. Run them with:
//!
//! ```text
//! cargo test --test x11_delivery -- --ignored --test-threads=1
//! ```
//!
//! and do not touch the keyboard or mouse for a few seconds. Safety: the macro runs with
//! focus protection restricted to the WM_CLASS of a window created by the test, and clicks
//! additionally require the pointer over that window (the test moves it there and back),
//! so if the window is not focused nothing is sent anywhere (the test then fails instead).

use mentos_macro::config::{ActivationMode, Hotkeys, InputConfig, LoopConfig};
use mentos_macro::engine::{Command, EngineEvent, EngineHandle, RunSpec, StopReason, X11Platform};
use mentos_macro::hotkeys::{HotkeyCommand, HotkeyEvent, HotkeyHandle};
use mentos_macro::input::{BackendKind, Keymap};
use mentos_macro::keys::{self, KeyCombo, Keysym, MouseButton};
use mentos_macro::sequence::{ActionKind, Sequence, Step, Timing};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const TEST_CLASS: &str = "mentos-x11-test";

struct TestWindow {
    conn: RustConnection,
    root: Window,
    win: Window,
    keymap: Keymap,
    pointer_home: (i16, i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XEv {
    Key(Keysym, bool, u32),
    Button(u8, bool, u32),
}

impl TestWindow {
    fn open() -> TestWindow {
        let (conn, screen) = x11rb::connect(None).expect("X11 display required");
        let s = &conn.setup().roots[screen];
        let (root, visual, black) = (s.root, s.root_visual, s.black_pixel);
        let win = conn.generate_id().unwrap();
        let mask = EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::ENTER_WINDOW
            | EventMask::STRUCTURE_NOTIFY;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            200,
            200,
            320,
            220,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().background_pixel(black).event_mask(mask),
        )
        .unwrap();
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"mentos-x11-test\0MentosX11Test\0")
            .unwrap();
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"Mentos X11 delivery test").unwrap();
        conn.map_window(win).unwrap();
        conn.flush().unwrap();
        let keymap = Keymap::load(&conn).unwrap();
        let home = conn.query_pointer(root).unwrap().reply().unwrap();
        let tw = TestWindow { conn, root, win, keymap, pointer_home: (home.root_x, home.root_y) };
        tw.activate();
        tw.pointer_into();
        tw
    }

    /// Mouse events go to the window under the pointer: move it into the test window and
    /// require the EnterNotify that proves the window is really under it.
    fn pointer_into(&self) {
        self.conn.warp_pointer(x11rb::NONE, self.win, 0, 0, 0, 0, 160, 110).unwrap();
        self.conn.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            while let Some(ev) = self.conn.poll_for_event().unwrap() {
                if matches!(ev, Event::EnterNotify(e) if e.event == self.win) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.restore_pointer();
        panic!("pointer is not over the test window; nothing was sent");
    }

    fn restore_pointer(&self) {
        let _ = self.conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, self.pointer_home.0, self.pointer_home.1);
        let _ = self.conn.flush();
    }

    fn atom(&self, name: &str) -> Atom {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    fn active_window(&self) -> Option<Window> {
        let a = self.atom("_NET_ACTIVE_WINDOW");
        let r = self.conn.get_property(false, self.root, a, AtomEnum::WINDOW, 0, 1).unwrap().reply().unwrap();
        r.value32().and_then(|mut v| v.next())
    }

    /// Ask the window manager to activate the window and wait until it is focused.
    fn activate(&self) {
        let net_active = self.atom("_NET_ACTIVE_WINDOW");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let ev = ClientMessageEvent::new(32, self.win, net_active, [2u32, 0, 0, 0, 0]);
            self.conn
                .send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev)
                .unwrap();
            self.conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let focus = self.conn.get_input_focus().unwrap().reply().unwrap().focus;
            if self.active_window() == Some(self.win) && focus == self.win {
                return;
            }
        }
        panic!("the window manager did not focus the test window; nothing was sent");
    }

    fn collect(&self, settle: Duration) -> Vec<XEv> {
        let end = Instant::now() + settle;
        let mut out = Vec::new();
        while Instant::now() < end {
            while let Some(ev) = self.conn.poll_for_event().unwrap() {
                match ev {
                    Event::KeyPress(e) if e.event == self.win => out.push(XEv::Key(self.keymap.keysym(e.detail, 0), true, e.time)),
                    Event::KeyRelease(e) if e.event == self.win => out.push(XEv::Key(self.keymap.keysym(e.detail, 0), false, e.time)),
                    Event::ButtonPress(e) if e.event == self.win => out.push(XEv::Button(e.detail, true, e.time)),
                    Event::ButtonRelease(e) if e.event == self.win => out.push(XEv::Button(e.detail, false, e.time)),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.restore_pointer();
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

fn protected_input() -> InputConfig {
    InputConfig { target_class: TEST_CLASS.into(), ..InputConfig::default() }
}

fn run_once(spec: RunSpec) -> (StopReason, String) {
    let (tx, rx) = mpsc::channel();
    let mut engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    engine.send(Command::Update(Arc::new(spec)));
    engine.send(Command::Start);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(EngineEvent::Finished { reason, message, .. }) => {
                engine.shutdown();
                return (reason, message);
            }
            Ok(_) => {}
            Err(_) => panic!("engine did not finish"),
        }
    }
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn default_sequence_reaches_the_focused_window_in_order() {
    default_sequence_via(BackendKind::Xtest);
}

#[test]
#[ignore = "needs an X11 session and write access to /dev/uinput; run with --ignored"]
fn default_sequence_via_uinput_backend() {
    default_sequence_via(BackendKind::Uinput);
}

fn default_sequence_via(backend: BackendKind) {
    let tw = TestWindow::open();
    tw.collect(Duration::from_millis(100)); // discard anything pending
    let spec = RunSpec {
        sequence: Sequence::default_sequence(),
        timing: Timing::default(),
        looping: LoopConfig { enabled: false, count: 0 },
        start_delay_ms: 0,
        input: InputConfig { backend, ..protected_input() },
        hotkeys: Hotkeys::default(),
    };
    let (reason, message) = run_once(spec);
    assert_eq!(reason, StopReason::Completed, "{message}");
    let events = tw.collect(Duration::from_millis(150));

    let got: Vec<XEv> = events
        .iter()
        .map(|e| match *e {
            XEv::Key(k, d, _) => XEv::Key(k, d, 0),
            XEv::Button(b, d, _) => XEv::Button(b, d, 0),
        })
        .collect();
    let expected: Vec<XEv> = ['5', '6', '7', '8', '9', '0']
        .into_iter()
        .flat_map(|c| {
            let c = c as Keysym;
            [XEv::Key(c, true, 0), XEv::Key(c, false, 0), XEv::Button(1, true, 0), XEv::Button(1, false, 0)]
        })
        .collect();
    assert_eq!(got, expected, "keys and left clicks received by the X client");

    // X server timestamps (ms): holds >= 25 ms and gaps >= 15 ms (1 ms clock granularity).
    let times: Vec<(bool, u32)> = events
        .iter()
        .map(|e| match *e {
            XEv::Key(_, d, t) | XEv::Button(_, d, t) => (d, t),
        })
        .collect();
    let mut holds = Vec::new();
    let mut gaps = Vec::new();
    for w in times.windows(2) {
        let dt = w[1].1.wrapping_sub(w[0].1);
        if w[0].0 { holds.push(dt) } else { gaps.push(dt) }
    }
    println!("[{backend:?}] server-measured holds (ms): {holds:?}");
    println!("[{backend:?}] server-measured gaps  (ms): {gaps:?}");
    assert!(holds.iter().all(|h| (24..=40).contains(h)), "holds {holds:?}");
    assert!(gaps.iter().all(|g| (14..=30).contains(g)), "gaps {gaps:?}");
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn combinations_and_mouse_holds_are_delivered() {
    let tw = TestWindow::open();

    let mut hold_left = Step::mouse(ActionKind::MouseHold, MouseButton::Left);
    hold_left.duration_ms = Some(80);
    let spec = RunSpec {
        sequence: Sequence {
            steps: vec![
                Step::press(KeyCombo::parse("Shift+E").unwrap()),
                hold_left,
                Step::mouse(ActionKind::MousePress, MouseButton::Right),
            ],
        },
        timing: Timing::default(),
        looping: LoopConfig { enabled: false, count: 0 },
        start_delay_ms: 0,
        input: protected_input(),
        hotkeys: Hotkeys::default(),
    };
    let (reason, message) = run_once(spec);
    let events = tw.collect(Duration::from_millis(150));
    assert_eq!(reason, StopReason::Completed, "{message}");

    let shape: Vec<String> = events
        .iter()
        .map(|e| match e {
            XEv::Key(k, d, _) => format!("{}{}", keys::key_label(*k), if *d { "+" } else { "-" }),
            XEv::Button(b, d, _) => format!("B{b}{}", if *d { "+" } else { "-" }),
        })
        .collect();
    assert_eq!(shape, ["Shift+", "E+", "E-", "Shift-", "B1+", "B1-", "B3+", "B3-"]);
    let t = |i: usize| match events[i] {
        XEv::Key(_, _, t) | XEv::Button(_, _, t) => t,
    };
    let left_hold = t(5).wrapping_sub(t(4));
    println!("server-measured left-button hold: {left_hold} ms");
    assert!((79..=95).contains(&left_hold), "left hold {left_hold} ms");
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn hotkey_toggle_starts_and_stops_and_input_reaches_target() {
    let tw = TestWindow::open();
    let (etx, erx) = mpsc::channel();
    let mut engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        let _ = etx.send(ev);
    })
    .unwrap();
    let (htx, hrx) = mpsc::channel();
    let mut hotkeys = HotkeyHandle::spawn(engine.sender(), move |ev| {
        let _ = htx.send(ev);
    })
    .unwrap();
    let hk = Hotkeys::default();
    hotkeys.send(HotkeyCommand::Configure { hotkeys: hk, mode: ActivationMode::Toggle });
    match hrx.recv_timeout(Duration::from_secs(2)).expect("registration result") {
        HotkeyEvent::Registered { ok: true, .. } => {}
        other => panic!("hotkeys not registered (is Mentos Macro running?): {other:?}"),
    }
    let t = Timing { key_ms: 10, gap_ms: 10, loop_ms: 10, modifier_ms: 0 };
    engine.send(Command::Update(Arc::new(RunSpec {
        sequence: Sequence { steps: vec![Step::press(KeyCombo::parse("A").unwrap())] },
        timing: t,
        looping: LoopConfig { enabled: true, count: 0 },
        start_delay_ms: 0,
        input: protected_input(),
        hotkeys: hk,
    })));
    tw.collect(Duration::from_millis(50));

    let f6 = tw.keymap.keycodes_for(hk.start_stop.key)[0];
    let tap = |down_ms: u64| {
        tw.conn.xtest_fake_input(KEY_PRESS_EVENT, f6, 0, tw.root, 0, 0, 0).unwrap();
        tw.conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(down_ms));
        tw.conn.xtest_fake_input(KEY_RELEASE_EVENT, f6, 0, tw.root, 0, 0, 0).unwrap();
        tw.conn.flush().unwrap();
    };

    // First press: start. The macro's keys must arrive at the focused window even though
    // the hotkey is still physically held (i.e. the keyboard grab was released).
    tw.conn.xtest_fake_input(KEY_PRESS_EVENT, f6, 0, tw.root, 0, 0, 0).unwrap();
    tw.conn.flush().unwrap();
    let during_hold = tw.collect(Duration::from_millis(250));
    tw.conn.xtest_fake_input(KEY_RELEASE_EVENT, f6, 0, tw.root, 0, 0, 0).unwrap();
    tw.conn.flush().unwrap();
    let a_presses = during_hold.iter().filter(|e| matches!(e, XEv::Key(0x61, true, _))).count();
    assert!(a_presses >= 3, "macro input reached the target while the hotkey was held: {during_hold:?}");

    std::thread::sleep(Duration::from_millis(100));
    tap(30); // second press: stop
    let finished = loop {
        if let EngineEvent::Finished { reason, .. } = erx.recv_timeout(Duration::from_secs(2)).expect("engine stopped by hotkey") {
            break reason;
        }
    };
    assert_eq!(finished, StopReason::Stopped);
    let rest = tw.collect(Duration::from_millis(100));
    let all: Vec<XEv> = during_hold.into_iter().chain(rest).collect();
    let downs = all.iter().filter(|e| matches!(e, XEv::Key(0x61, true, _))).count();
    let ups = all.iter().filter(|e| matches!(e, XEv::Key(0x61, false, _))).count();
    assert_eq!(downs, ups, "every press was released");
    assert!(!all.iter().any(|e| matches!(e, XEv::Key(k, true, _) if *k == hk.start_stop.key)), "F6 press never reaches the target");

    hotkeys.shutdown();
    engine.shutdown();
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn closing_the_target_window_stops_cleanly() {
    let tw = TestWindow::open();
    tw.collect(Duration::from_millis(50));
    let (tx, rx) = mpsc::channel();
    let mut engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    engine.send(Command::Update(Arc::new(RunSpec {
        sequence: Sequence { steps: vec![Step::hold(KeyCombo::parse("A").unwrap(), 5_000)] },
        timing: Timing::default(),
        looping: LoopConfig { enabled: false, count: 0 },
        start_delay_ms: 0,
        input: protected_input(),
        hotkeys: Hotkeys::default(),
    })));
    engine.send(Command::Start);
    let pressed = tw.collect(Duration::from_millis(150));
    assert!(matches!(pressed.first(), Some(XEv::Key(0x61, true, _))), "{pressed:?}");
    tw.conn.destroy_window(tw.win).unwrap();
    tw.conn.flush().unwrap();
    let reason = loop {
        if let EngineEvent::Finished { reason, message, .. } = rx.recv_timeout(Duration::from_secs(2)).expect("engine stopped") {
            break (reason, message);
        }
    };
    engine.shutdown();
    assert_eq!(reason.0, StopReason::FocusLost, "a closed window is a lost target, not an error: {}", reason.1);
}
