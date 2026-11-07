//! The execution worker: one long-lived thread that owns the input backend and runs
//! validated plans.
//!
//! State machine:
//!
//! ```text
//!   Idle ──start──▶ Arming (start delay) ──▶ WaitingForTarget ──▶ Running ──▶ Idle
//!                     │                         │                  │  ▲
//!                     └─────── stop ────────────┴──── stop ────────┤  │ refocus
//!                                                                  ▼  │
//!                                                         Paused (focus lost, pause mode)
//! ```
//!
//! Every wait is a `recv_timeout` on the command channel, so stop/emergency commands are
//! handled within microseconds even in the middle of a key hold. Whatever ends a session
//! (completion, stop, focus loss, error, shutdown, panic) releases all held inputs.

use crate::config::{Config, FocusLoss, Hotkeys, InputConfig, LoopConfig};
use crate::input::{self, BackendKind, InputBackend, KeyResolver, Keymap};
use crate::keys::MouseButton;
use crate::safety::{FocusProbe, HeldInputs, TargetPolicy, WindowInfo, XFocus};
use crate::sequence::{self, Op, Plan, Sequence, Timing};
use crate::timing::{self, ms, Scheduler, TimingStats};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often focus is re-checked during long waits.
const FOCUS_SLICE: Duration = Duration::from_millis(10);
/// How long a new uinput device may take to appear in the X server.
const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(3);
/// Lowercase WM_CLASS fragment of this application's own window.
pub const OWN_WINDOW_CLASS: &str = "mentos-macro";

/// Immutable snapshot of everything a run needs. Built by the UI from the configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSpec {
    pub sequence: Sequence,
    pub timing: Timing,
    pub looping: LoopConfig,
    pub start_delay_ms: u32,
    pub input: InputConfig,
    pub hotkeys: Hotkeys,
}

impl RunSpec {
    pub fn from_config(c: &Config) -> RunSpec {
        RunSpec {
            sequence: c.active_sequence(),
            timing: c.timing,
            looping: c.looping,
            start_delay_ms: c.start_delay_ms,
            input: c.input.clone(),
            hotkeys: c.hotkeys,
        }
    }
}

#[derive(Debug)]
pub enum Command {
    /// New configuration snapshot; used from the next run on (a running plan is never changed).
    Update(Arc<RunSpec>),
    /// Start if idle, stop otherwise.
    Toggle,
    Start,
    Stop,
    /// Hold-to-run hotkey pressed / released.
    HoldPress,
    HoldRelease,
    /// Stop immediately and release every key/button the plan could have pressed.
    Emergency,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Arming,
    WaitingForTarget,
    Running,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Stopped,
    Emergency,
    FocusLost,
    Failed,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    State { state: RunState, detail: String },
    Finished { reason: StopReason, cycles: u64, stats: TimingStats, message: String },
}

/// Everything the engine needs from the outside world; mocked in tests.
pub trait Platform {
    /// Fresh keymap snapshot for resolving keysyms.
    fn resolver(&mut self) -> Result<Box<dyn KeyResolver>, String>;
    /// The backend of `kind`, creating (or re-creating) it if needed.
    fn backend(&mut self, kind: BackendKind) -> Result<&mut (dyn InputBackend + 'static), String>;
    /// The current backend if one exists (used for cleanup).
    fn existing_backend(&mut self) -> Option<&mut (dyn InputBackend + 'static)>;
    /// Discard the backend after an error so the next run starts clean.
    fn reset_backend(&mut self);
    fn focus(&mut self) -> Result<Option<WindowInfo>, String>;
    fn own_pid(&self) -> u32 {
        std::process::id()
    }
}

/// Production platform: X11 focus tracking + XTest/uinput backends.
#[derive(Default)]
pub struct X11Platform {
    focus: Option<XFocus>,
    backend: Option<Box<dyn InputBackend>>,
}

impl X11Platform {
    fn focus_conn(&mut self) -> Result<&mut XFocus, String> {
        if self.focus.is_none() {
            self.focus = Some(XFocus::connect()?);
        }
        Ok(self.focus.as_mut().expect("just set"))
    }
}

impl Platform for X11Platform {
    fn resolver(&mut self) -> Result<Box<dyn KeyResolver>, String> {
        let f = self.focus_conn()?;
        match Keymap::load(f.conn()) {
            Ok(k) => Ok(Box::new(k)),
            Err(e) => {
                self.focus = None;
                Err(e)
            }
        }
    }

    fn backend(&mut self, kind: BackendKind) -> Result<&mut (dyn InputBackend + 'static), String> {
        if self.backend.as_ref().is_some_and(|b| b.kind() != kind) {
            self.backend = None;
        }
        if self.backend.is_none() {
            self.backend = Some(input::create_backend(kind).map_err(|e| e.0)?);
        }
        Ok(self.backend.as_deref_mut().expect("just set"))
    }

    fn existing_backend(&mut self) -> Option<&mut (dyn InputBackend + 'static)> {
        self.backend.as_deref_mut()
    }

    fn reset_backend(&mut self) {
        self.backend = None;
    }

    fn focus(&mut self) -> Result<Option<WindowInfo>, String> {
        let result = self.focus_conn()?.active();
        if result.is_err() {
            self.focus = None;
        }
        result
    }
}

enum Interrupt {
    Stop,
    Emergency,
    Shutdown,
    FocusLost,
    Failed(String),
}

/// Where input may go during a running session.
struct Guard {
    protection: bool,
    target: Option<u32>,
    policy: TargetPolicy,
}

pub struct Engine<P: Platform> {
    platform: P,
    rx: Receiver<Command>,
    events: Box<dyn Fn(EngineEvent) + Send>,
    spec: Option<Arc<RunSpec>>,
    held: HeldInputs,
    state: RunState,
    detail: String,
    backend_kind: BackendKind,
    /// Keycodes/buttons of the plan being executed (for emergency release).
    plan_inputs: (Vec<u8>, Vec<MouseButton>),
}

impl<P: Platform> Engine<P> {
    pub fn new(platform: P, rx: Receiver<Command>, events: Box<dyn Fn(EngineEvent) + Send>) -> Self {
        Engine {
            platform,
            rx,
            events,
            spec: None,
            held: HeldInputs::default(),
            state: RunState::Idle,
            detail: String::new(),
            backend_kind: BackendKind::Xtest,
            plan_inputs: (Vec::new(), Vec::new()),
        }
    }

    /// Process commands until `Shutdown` or until every sender is dropped.
    pub fn run(mut self) {
        timing::tighten_timer_slack();
        while let Ok(cmd) = self.rx.recv() {
            match cmd {
                Command::Update(spec) => self.spec = Some(spec),
                Command::Start | Command::Toggle | Command::HoldPress => {
                    if self.session() == StopReason::Shutdown {
                        break;
                    }
                }
                Command::Stop | Command::HoldRelease => {}
                Command::Emergency => self.release_all(true),
                Command::Shutdown => break,
            }
        }
        self.release_all(false);
    }

    fn emit(&self, ev: EngineEvent) {
        (self.events)(ev);
    }

    fn set_state(&mut self, state: RunState, detail: impl Into<String>) {
        let detail = detail.into();
        if state != self.state || detail != self.detail {
            self.state = state;
            self.detail = detail.clone();
            self.emit(EngineEvent::State { state, detail });
        }
    }

    /// One complete run, from validation to cleanup.
    fn session(&mut self) -> StopReason {
        let mut stats = TimingStats::default();
        let mut cycles = 0;
        let outcome = self.session_inner(&mut stats, &mut cycles);
        let (reason, message) = match outcome {
            Ok(()) => (StopReason::Completed, String::new()),
            Err(Interrupt::Stop) => (StopReason::Stopped, String::new()),
            Err(Interrupt::Emergency) => (StopReason::Emergency, "Emergency stop".to_string()),
            Err(Interrupt::Shutdown) => (StopReason::Shutdown, String::new()),
            Err(Interrupt::FocusLost) => (StopReason::FocusLost, "Stopped: the target window lost focus".to_string()),
            Err(Interrupt::Failed(e)) => (StopReason::Failed, e),
        };
        self.release_all(reason == StopReason::Emergency);
        if reason == StopReason::Failed {
            tracing::warn!("run failed: {message}");
            self.platform.reset_backend();
        }
        tracing::debug!(?reason, cycles, ?stats, "session finished");
        self.plan_inputs = (Vec::new(), Vec::new());
        self.set_state(RunState::Idle, "");
        self.emit(EngineEvent::Finished { reason, cycles, stats, message });
        reason
    }

    fn session_inner(&mut self, stats_out: &mut TimingStats, cycles: &mut u64) -> Result<(), Interrupt> {
        let spec = self.spec.clone().ok_or_else(|| Interrupt::Failed("Not configured yet".into()))?;
        let resolver = self.platform.resolver().map_err(Interrupt::Failed)?;
        let plan = sequence::compile(&spec.sequence, &spec.timing, spec.looping.enabled, &spec.hotkeys.reserved(), &*resolver)
            .map_err(|e| Interrupt::Failed(e.to_string()))?;
        drop(resolver);
        self.plan_inputs = (plan.keycodes.clone(), plan.buttons.clone());
        self.backend_kind = spec.input.backend;
        self.platform.backend(spec.input.backend).map_err(Interrupt::Failed)?;

        let start_at = Instant::now() + ms(spec.start_delay_ms);
        if start_at > Instant::now() {
            self.set_state(RunState::Arming, format!("{} ms", spec.start_delay_ms));
            self.wait_until(start_at, None)?;
        }
        self.wait_backend_ready(spec.input.backend)?;

        let policy = TargetPolicy {
            protection: spec.input.focus_protection,
            class_filter: spec.input.target_class.clone(),
            own_pid: self.platform.own_pid(),
            own_class: OWN_WINDOW_CLASS,
        };
        let target = self.acquire_target(&policy, &spec)?;
        let guard = Guard { protection: spec.input.focus_protection, target: Some(target.id), policy };
        self.set_state(RunState::Running, target.class.clone());

        let mut sched = Scheduler::new(Instant::now());
        let result = self.run_cycles(&plan, &spec, &guard, &mut sched, cycles);
        *stats_out = sched.stats();
        result
    }

    fn run_cycles(
        &mut self,
        plan: &Plan,
        spec: &RunSpec,
        guard: &Guard,
        sched: &mut Scheduler,
        cycles: &mut u64,
    ) -> Result<(), Interrupt> {
        loop {
            let more = spec.looping.enabled && (spec.looping.count == 0 || *cycles + 1 < u64::from(spec.looping.count));
            match self.run_cycle(plan, guard, sched, more) {
                Ok(()) => {}
                Err(Interrupt::FocusLost) if spec.input.focus_loss == FocusLoss::Pause => {
                    self.release_all(false);
                    self.set_state(RunState::Paused, "Waiting for the target window");
                    self.wait_for_refocus(guard)?;
                    self.set_state(RunState::Running, guard_label(guard));
                    sched.reset(Instant::now());
                    continue; // restart the interrupted cycle from its first step
                }
                Err(e) => return Err(e),
            }
            *cycles += 1;
            if !more {
                return Ok(());
            }
        }
    }

    fn run_cycle(&mut self, plan: &Plan, guard: &Guard, sched: &mut Scheduler, trailing_wait: bool) -> Result<(), Interrupt> {
        for op in &plan.ops {
            match *op {
                Op::Wait(d) => {
                    let deadline = sched.advance(d);
                    self.wait_until(deadline, Some(guard))?;
                }
                Op::Key { keycode, down, .. } => {
                    if down {
                        self.check_focus(guard)?;
                        self.held.key_down(keycode);
                    }
                    let kind = self.backend_kind;
                    self.platform.backend(kind).map_err(Interrupt::Failed)?.key(keycode, down).map_err(|e| Interrupt::Failed(e.0))?;
                    if !down {
                        self.held.key_up(keycode);
                    }
                    sched.mark_emitted(Instant::now());
                }
                Op::Button { button, down } => {
                    if down {
                        self.check_focus(guard)?;
                        self.held.button_down(button);
                    }
                    let kind = self.backend_kind;
                    self.platform.backend(kind).map_err(Interrupt::Failed)?.button(button, down).map_err(|e| Interrupt::Failed(e.0))?;
                    if !down {
                        self.held.button_up(button);
                    }
                    sched.mark_emitted(Instant::now());
                }
            }
        }
        if trailing_wait {
            let deadline = sched.advance(plan.loop_wait);
            self.wait_until(deadline, Some(guard))?;
        }
        Ok(())
    }

    /// Handle a command that arrives while a session is active.
    fn on_busy_command(&mut self, cmd: Command) -> Result<(), Interrupt> {
        match cmd {
            Command::Update(spec) => {
                self.spec = Some(spec);
                Ok(())
            }
            Command::Start | Command::HoldPress => Ok(()), // already running: never a second worker
            Command::Toggle | Command::Stop | Command::HoldRelease => Err(Interrupt::Stop),
            Command::Emergency => Err(Interrupt::Emergency),
            Command::Shutdown => Err(Interrupt::Shutdown),
        }
    }

    /// Sleep until `deadline` while staying responsive to commands and (optionally) focus.
    fn wait_until(&mut self, deadline: Instant, guard: Option<&Guard>) -> Result<(), Interrupt> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let slice = (deadline - now).min(FOCUS_SLICE);
            match self.rx.recv_timeout(slice) {
                Ok(cmd) => self.on_busy_command(cmd)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(Interrupt::Shutdown),
            }
            if let Some(g) = guard {
                if Instant::now() < deadline {
                    self.check_focus(g)?;
                }
            }
        }
    }

    /// Wait (cancellably) until the backend delivers events.
    fn wait_backend_ready(&mut self, kind: BackendKind) -> Result<(), Interrupt> {
        let deadline = Instant::now() + BACKEND_READY_TIMEOUT;
        loop {
            let ready = self.platform.backend(kind).map_err(Interrupt::Failed)?.is_ready().map_err(|e| Interrupt::Failed(e.0))?;
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Interrupt::Failed(
                    "The X server did not pick up the uinput device within 3 s; use the XTest backend".into(),
                ));
            }
            self.set_state(RunState::Arming, "Preparing the virtual input device");
            self.wait_until(Instant::now() + Duration::from_millis(20), None)?;
        }
    }

    fn check_focus(&mut self, guard: &Guard) -> Result<(), Interrupt> {
        let active = self.platform.focus().map_err(Interrupt::Failed)?;
        let ok = match (&active, guard.protection) {
            (Some(w), true) => Some(w.id) == guard.target,
            (None, true) => false,
            // Without protection only this app's own window is off limits.
            (Some(w), false) => !guard.policy.is_own(w),
            (None, false) => true,
        };
        if ok {
            Ok(())
        } else {
            Err(Interrupt::FocusLost)
        }
    }

    /// Wait until a window the policy accepts is focused and return it.
    fn acquire_target(&mut self, policy: &TargetPolicy, spec: &RunSpec) -> Result<WindowInfo, Interrupt> {
        loop {
            if let Some(w) = self.platform.focus().map_err(Interrupt::Failed)? {
                if policy.eligible(&w) {
                    return Ok(w);
                }
            }
            let detail = if spec.input.focus_protection && !spec.input.target_class.trim().is_empty() {
                format!("Waiting for a \"{}\" window", spec.input.target_class.trim())
            } else {
                "Waiting for the target window".to_string()
            };
            self.set_state(RunState::WaitingForTarget, detail);
            self.wait_until(Instant::now() + Duration::from_millis(25), None)?;
        }
    }

    fn wait_for_refocus(&mut self, guard: &Guard) -> Result<(), Interrupt> {
        loop {
            let focused = self.platform.focus().map_err(Interrupt::Failed)?;
            let back = match (&focused, guard.protection) {
                (Some(w), true) => Some(w.id) == guard.target,
                (Some(w), false) => !guard.policy.is_own(w),
                (None, protection) => !protection,
            };
            if back {
                return Ok(());
            }
            self.wait_until(Instant::now() + Duration::from_millis(25), None)?;
        }
    }

    /// Release everything the macro holds. With `emergency`, additionally send a release
    /// for every key/button the current plan uses (the X server ignores releases of keys
    /// that are not down).
    fn release_all(&mut self, emergency: bool) {
        let (keys, buttons) = self.held.drain_reverse();
        let (mut extra_keys, mut extra_buttons) =
            if emergency { self.plan_inputs.clone() } else { (Vec::new(), Vec::new()) };
        extra_keys.retain(|k| !keys.contains(k));
        extra_buttons.retain(|b| !buttons.contains(b));
        if keys.is_empty() && buttons.is_empty() && extra_keys.is_empty() && extra_buttons.is_empty() {
            return;
        }
        let Some(backend) = self.platform.existing_backend() else {
            tracing::warn!("cannot release held inputs: no backend");
            return;
        };
        for k in keys.iter().chain(extra_keys.iter()) {
            if let Err(e) = backend.key(*k, false) {
                tracing::warn!("failed to release keycode {k}: {e}");
            }
        }
        for b in buttons.iter().chain(extra_buttons.iter()) {
            if let Err(e) = backend.button(*b, false) {
                tracing::warn!("failed to release {} mouse: {e}", b.label());
            }
        }
    }
}

impl<P: Platform> Drop for Engine<P> {
    fn drop(&mut self) {
        // Covers panics inside the worker: never leave keys stuck down.
        self.release_all(false);
    }
}

fn guard_label(g: &Guard) -> String {
    g.target.map(|t| format!("window 0x{t:x}")).unwrap_or_default()
}

/// Owner-side handle of the worker thread.
pub struct EngineHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    /// Spawn the worker. `make_platform` runs on the worker thread.
    pub fn spawn<P, F>(make_platform: F, events: impl Fn(EngineEvent) + Send + 'static) -> std::io::Result<EngineHandle>
    where
        P: Platform + 'static,
        F: FnOnce() -> P + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let events: Box<dyn Fn(EngineEvent) + Send> = Box::new(events);
        let join = std::thread::Builder::new().name("macro-engine".into()).spawn(move || {
            Engine::new(make_platform(), rx, events).run();
        })?;
        Ok(EngineHandle { tx, join: Some(join) })
    }

    pub fn sender(&self) -> Sender<Command> {
        self.tx.clone()
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_err() {
            tracing::error!("execution worker is not running");
        }
    }

    /// Stop any run, release inputs and join the worker.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            if j.join().is_err() {
                tracing::error!("execution worker panicked");
            }
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
