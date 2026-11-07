//! Execution-engine behaviour against a mock platform that records every input event
//! with a timestamp: ordering, timing, cancellation, focus protection and cleanup.

use mentos_macro::config::{FocusLoss, Hotkeys, InputConfig, LoopConfig};
use mentos_macro::engine::{Command, EngineEvent, EngineHandle, Platform, RunSpec, RunState, StopReason};
use mentos_macro::input::{BackendKind, InputBackend, InputResult, KeyLoc, KeyResolver};
use mentos_macro::keys::{KeyCombo, Keysym, MouseButton, XK_TAB};
use mentos_macro::safety::WindowInfo;
use mentos_macro::sequence::{ActionKind, Sequence, Step, Timing};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Key(u8, bool),
    Button(MouseButton, bool),
}

type Log = Arc<Mutex<Vec<(Instant, Ev)>>>;

struct MockBackend {
    log: Log,
}

impl InputBackend for MockBackend {
    fn key(&mut self, keycode: u8, down: bool) -> InputResult {
        self.log.lock().unwrap().push((Instant::now(), Ev::Key(keycode, down)));
        Ok(())
    }
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        self.log.lock().unwrap().push((Instant::now(), Ev::Button(button, down)));
        Ok(())
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Xtest
    }
    fn is_ready(&mut self) -> InputResult<bool> {
        Ok(true)
    }
}

/// keycode = low byte of the keysym; 0xdead is "not on the keyboard".
struct FakeResolver;
impl KeyResolver for FakeResolver {
    fn resolve(&self, sym: Keysym) -> Option<KeyLoc> {
        (sym != 0xdead).then_some(KeyLoc { keycode: (sym & 0xff) as u8, needs_shift: false })
    }
}

const OWN_PID: u32 = 1;

struct MockPlatform {
    log: Log,
    focus: Arc<Mutex<Option<WindowInfo>>>,
    backend: Option<MockBackend>,
}

impl Platform for MockPlatform {
    fn resolver(&mut self) -> Result<Box<dyn KeyResolver>, String> {
        Ok(Box::new(FakeResolver))
    }
    fn backend(&mut self, _kind: BackendKind) -> Result<&mut (dyn InputBackend + 'static), String> {
        if self.backend.is_none() {
            self.backend = Some(MockBackend { log: self.log.clone() });
        }
        Ok(self.backend.as_mut().unwrap())
    }
    fn existing_backend(&mut self) -> Option<&mut (dyn InputBackend + 'static)> {
        self.backend.as_mut().map(|b| b as &mut (dyn InputBackend + 'static))
    }
    fn reset_backend(&mut self) {
        self.backend = None;
    }
    fn focus(&mut self) -> Result<Option<WindowInfo>, String> {
        Ok(self.focus.lock().unwrap().clone())
    }
    fn own_pid(&self) -> u32 {
        OWN_PID
    }
}

fn game() -> WindowInfo {
    WindowInfo { id: 100, pid: Some(500), class: "sober org.vinegarhq.Sober".into() }
}
fn other_window() -> WindowInfo {
    WindowInfo { id: 200, pid: Some(600), class: "code Code".into() }
}
fn own_window() -> WindowInfo {
    WindowInfo { id: 300, pid: Some(OWN_PID), class: "mentos-macro".into() }
}

struct Harness {
    engine: EngineHandle,
    events: Receiver<EngineEvent>,
    log: Log,
    focus: Arc<Mutex<Option<WindowInfo>>>,
}

impl Harness {
    fn new(initial_focus: WindowInfo) -> Harness {
        let log: Log = Arc::default();
        let focus = Arc::new(Mutex::new(Some(initial_focus)));
        let (tx, events) = mpsc::channel();
        let (l, f) = (log.clone(), focus.clone());
        let engine = EngineHandle::spawn(move || MockPlatform { log: l, focus: f, backend: None }, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        Harness { engine, events, log, focus }
    }

    fn update(&self, spec: RunSpec) {
        self.engine.send(Command::Update(Arc::new(spec)));
    }

    fn set_focus(&self, w: Option<WindowInfo>) {
        *self.focus.lock().unwrap() = w;
    }

    fn wait_event(&self, timeout: Duration, pred: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for engine event"),
            }
        }
    }

    fn wait_state(&self, state: RunState) {
        self.wait_event(Duration::from_secs(3), |e| matches!(e, EngineEvent::State { state: s, .. } if *s == state));
    }

    fn wait_finished(&self, timeout: Duration) -> (StopReason, u64, String) {
        match self.wait_event(timeout, |e| matches!(e, EngineEvent::Finished { .. })) {
            EngineEvent::Finished { reason, cycles, message, .. } => (reason, cycles, message),
            _ => unreachable!(),
        }
    }

    fn wait_log_len(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.log.lock().unwrap().len() < n {
            assert!(Instant::now() < deadline, "timed out waiting for {n} input events");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn events(&self) -> Vec<(Instant, Ev)> {
        self.log.lock().unwrap().clone()
    }
}

fn spec(sequence: Sequence, timing: Timing, looping: LoopConfig) -> RunSpec {
    RunSpec { sequence, timing, looping, start_delay_ms: 0, input: InputConfig::default(), hotkeys: Hotkeys::default() }
}

fn k(t: &str) -> KeyCombo {
    KeyCombo::parse(t).unwrap()
}

const A: u8 = 0x61;
const ONCE: LoopConfig = LoopConfig { enabled: false, count: 0 };

#[test]
fn default_sequence_runs_in_order_with_configured_timing() {
    let h = Harness::new(game());
    h.update(spec(Sequence::default_sequence(), Timing::default(), ONCE));
    h.engine.send(Command::Start);
    let (reason, cycles, _) = h.wait_finished(Duration::from_secs(3));
    assert_eq!((reason, cycles), (StopReason::Completed, 1));

    let events = h.events();
    let keys: Vec<Ev> = events.iter().map(|(_, e)| *e).collect();
    let tab = (XK_TAB & 0xff) as u8;
    let expected: Vec<Ev> = b"567890"
        .iter()
        .copied()
        .flat_map(|d| [Ev::Key(d, true), Ev::Key(d, false), Ev::Key(tab, true), Ev::Key(tab, false)])
        .collect();
    assert_eq!(keys, expected);

    // Each hold >= 25 ms and each gap >= 15 ms (1 ms absorb tolerance), without large drift.
    for pair in events.windows(2) {
        let dt = pair[1].0 - pair[0].0;
        let (min, label) = match pair[0].1 {
            Ev::Key(_, true) => (Duration::from_millis(24), "hold"),
            _ => (Duration::from_millis(14), "gap"),
        };
        assert!(dt >= min, "{label} too short: {dt:?}");
        assert!(dt < min + Duration::from_millis(20), "{label} too long: {dt:?}");
    }
    let total = events.last().unwrap().0 - events[0].0;
    let nominal = Duration::from_millis(12 * 25 + 11 * 15);
    assert!(total >= nominal - Duration::from_millis(2) && total < nominal + Duration::from_millis(40), "total {total:?}");
}

#[test]
fn finite_loop_runs_exact_cycle_count() {
    let h = Harness::new(game());
    let t = Timing { key_ms: 2, gap_ms: 1, loop_ms: 3, modifier_ms: 0 };
    h.update(spec(Sequence { steps: vec![Step::press(k("A")), Step::press(k("B"))] }, t, LoopConfig { enabled: true, count: 3 }));
    h.engine.send(Command::Start);
    let (reason, cycles, _) = h.wait_finished(Duration::from_secs(3));
    assert_eq!((reason, cycles), (StopReason::Completed, 3));
    assert_eq!(h.events().len(), 3 * 4);
}

#[test]
fn stop_during_hold_releases_immediately() {
    let h = Harness::new(game());
    let hold = Sequence { steps: vec![Step::hold(k("A"), 5_000)] };
    h.update(spec(hold, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    let stop_at = Instant::now();
    h.engine.send(Command::Stop);
    let (reason, _, _) = h.wait_finished(Duration::from_millis(500));
    assert_eq!(reason, StopReason::Stopped);
    let events = h.events();
    assert_eq!(events.iter().map(|(_, e)| *e).collect::<Vec<_>>(), [Ev::Key(A, true), Ev::Key(A, false)]);
    let latency = events[1].0.saturating_duration_since(stop_at);
    assert!(latency < Duration::from_millis(20), "release took {latency:?}");
}

#[test]
fn stop_during_long_wait_is_immediate() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::press(k("A")), Step::wait(10_000), Step::press(k("B"))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_log_len(2);
    let t = Instant::now();
    h.engine.send(Command::Toggle);
    let (reason, _, _) = h.wait_finished(Duration::from_millis(500));
    assert_eq!(reason, StopReason::Stopped);
    assert!(t.elapsed() < Duration::from_millis(50));
    assert_eq!(h.events().len(), 2, "B must never be pressed");
}

#[test]
fn focus_loss_stops_and_releases() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::mouse(ActionKind::MouseHold, MouseButton::Right)].into_iter().map(|mut s| {
        s.duration_ms = Some(5_000);
        s
    }).collect() }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.set_focus(Some(other_window()));
    let (reason, _, message) = h.wait_finished(Duration::from_millis(500));
    assert_eq!(reason, StopReason::FocusLost);
    assert!(message.contains("focus"));
    assert_eq!(
        h.events().iter().map(|(_, e)| *e).collect::<Vec<_>>(),
        [Ev::Button(MouseButton::Right, true), Ev::Button(MouseButton::Right, false)]
    );
}

#[test]
fn pause_mode_releases_and_resumes_on_refocus() {
    let h = Harness::new(game());
    let mut s = spec(Sequence { steps: vec![Step::hold(k("A"), 300)] }, Timing::default(), LoopConfig { enabled: true, count: 0 });
    s.input.focus_loss = FocusLoss::Pause;
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.set_focus(Some(other_window()));
    h.wait_state(RunState::Paused);
    assert_eq!(h.events().last().unwrap().1, Ev::Key(A, false), "released while paused");
    let paused_len = h.events().len();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(h.events().len(), paused_len, "no input while paused");
    h.set_focus(Some(game()));
    h.wait_state(RunState::Running);
    h.wait_log_len(paused_len + 1);
    h.engine.send(Command::Stop);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).0, StopReason::Stopped);
    assert_eq!(h.events().last().unwrap().1, Ev::Key(A, false));
}

#[test]
fn waits_while_own_window_is_focused_then_locks_target() {
    let h = Harness::new(own_window());
    h.update(spec(Sequence { steps: vec![Step::press(k("A"))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_state(RunState::WaitingForTarget);
    std::thread::sleep(Duration::from_millis(40));
    assert!(h.events().is_empty(), "nothing is sent to the app's own window");
    h.set_focus(Some(game()));
    assert_eq!(h.wait_finished(Duration::from_secs(1)).0, StopReason::Completed);
    assert_eq!(h.events().len(), 2);
}

#[test]
fn class_filter_selects_the_target_window() {
    let h = Harness::new(other_window());
    let mut s = spec(Sequence { steps: vec![Step::press(k("A"))] }, Timing::default(), ONCE);
    s.input.target_class = "sober".into();
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_state(RunState::WaitingForTarget);
    std::thread::sleep(Duration::from_millis(40));
    assert!(h.events().is_empty());
    h.set_focus(Some(game()));
    assert_eq!(h.wait_finished(Duration::from_secs(1)).0, StopReason::Completed);
}

#[test]
fn invalid_sequence_fails_without_sending_input() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::key_down(k("R"))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    let (reason, _, message) = h.wait_finished(Duration::from_secs(1));
    assert_eq!(reason, StopReason::Failed);
    assert!(message.contains("never released"), "{message}");
    assert!(h.events().is_empty());
}

#[test]
fn unmapped_key_fails_cleanly() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::press(KeyCombo::new(0xdead))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    let (reason, _, message) = h.wait_finished(Duration::from_secs(1));
    assert_eq!(reason, StopReason::Failed);
    assert!(message.contains("keyboard layout"), "{message}");
}

#[test]
fn hotkey_conflict_in_sequence_is_rejected() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::press(k("F6"))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    let (reason, _, message) = h.wait_finished(Duration::from_secs(1));
    assert_eq!(reason, StopReason::Failed);
    assert!(message.contains("hotkey"), "{message}");
}

#[test]
fn repeated_start_never_creates_a_second_run() {
    let h = Harness::new(game());
    let t = Timing { key_ms: 5, gap_ms: 5, loop_ms: 5, modifier_ms: 0 };
    h.update(spec(Sequence { steps: vec![Step::press(k("A"))] }, t, LoopConfig { enabled: true, count: 0 }));
    h.engine.send(Command::Start);
    h.wait_state(RunState::Running);
    for _ in 0..5 {
        h.engine.send(Command::Start);
        h.engine.send(Command::HoldPress);
    }
    std::thread::sleep(Duration::from_millis(60));
    h.engine.send(Command::Toggle);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).0, StopReason::Stopped);
    // Strictly alternating down/up proves a single, sequential worker.
    for (i, (_, ev)) in h.events().iter().enumerate() {
        assert_eq!(*ev, Ev::Key(A, i % 2 == 0), "event {i}");
    }
}

#[test]
fn hold_to_run_stops_on_release() {
    let h = Harness::new(game());
    let t = Timing { key_ms: 5, gap_ms: 5, loop_ms: 5, modifier_ms: 0 };
    h.update(spec(Sequence { steps: vec![Step::press(k("A"))] }, t, LoopConfig { enabled: true, count: 0 }));
    h.engine.send(Command::HoldPress);
    h.wait_state(RunState::Running);
    std::thread::sleep(Duration::from_millis(30));
    h.engine.send(Command::HoldRelease);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).0, StopReason::Stopped);
    assert_eq!(h.events().last().unwrap().1, Ev::Key(A, false));
}

#[test]
fn emergency_stop_releases_held_keys() {
    let h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::key_down(k("Shift+E")), Step::wait(5_000), Step::key_up(k("Shift+E"))] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_log_len(2);
    h.engine.send(Command::Emergency);
    let (reason, _, _) = h.wait_finished(Duration::from_millis(500));
    assert_eq!(reason, StopReason::Emergency);
    let evs: Vec<Ev> = h.events().iter().map(|(_, e)| *e).collect();
    // Shift (0xe1) down, E (0x65) down, then released in reverse order.
    assert_eq!(&evs[..4], [Ev::Key(0xe1, true), Ev::Key(0x65, true), Ev::Key(0x65, false), Ev::Key(0xe1, false)]);
}

#[test]
fn configuration_updates_never_affect_a_running_plan() {
    let h = Harness::new(game());
    let t = Timing { key_ms: 3, gap_ms: 3, loop_ms: 3, modifier_ms: 0 };
    let looping = LoopConfig { enabled: true, count: 0 };
    h.update(spec(Sequence { steps: vec![Step::press(k("A"))] }, t, looping));
    h.engine.send(Command::Start);
    h.wait_log_len(4);
    h.update(spec(Sequence { steps: vec![Step::press(k("B"))] }, t, looping));
    std::thread::sleep(Duration::from_millis(40));
    h.engine.send(Command::Stop);
    h.wait_finished(Duration::from_millis(500));
    assert!(h.events().iter().all(|(_, e)| matches!(e, Ev::Key(A, _))), "running plan stayed A");
    let before = h.events().len();
    h.engine.send(Command::Start);
    h.wait_log_len(before + 2);
    h.engine.send(Command::Stop);
    h.wait_finished(Duration::from_millis(500));
    assert_eq!(h.events()[before].1, Ev::Key(b'b', true), "next run uses the new snapshot");
}

#[test]
fn start_delay_is_cancellable() {
    let h = Harness::new(game());
    let mut s = spec(Sequence { steps: vec![Step::press(k("A"))] }, Timing::default(), ONCE);
    s.start_delay_ms = 10_000;
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_state(RunState::Arming);
    h.engine.send(Command::Toggle);
    assert_eq!(h.wait_finished(Duration::from_millis(200)).0, StopReason::Stopped);
    assert!(h.events().is_empty());
}

#[test]
fn shutdown_mid_run_releases_input() {
    let mut h = Harness::new(game());
    h.update(spec(Sequence { steps: vec![Step::hold(k("A"), 5_000)] }, Timing::default(), ONCE));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.engine.shutdown();
    assert_eq!(h.events().iter().map(|(_, e)| *e).collect::<Vec<_>>(), [Ev::Key(A, true), Ev::Key(A, false)]);
}
