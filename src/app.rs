//! Application controller: owns the configuration and connects the Slint UI with the
//! execution worker and the hotkey service. Runs entirely on the UI thread; background
//! threads talk to it through a channel drained via `slint::invoke_from_event_loop`.

use crate::config::{ActivationMode, Config, ConfigStore, FocusLoss, Hotkeys};
use crate::engine::{Command, EngineEvent, EngineHandle, RunSpec, RunState, StopReason, X11Platform};
use crate::hotkeys::{HotkeyCommand, HotkeyEvent, HotkeyHandle};
use crate::input::BackendKind;
use crate::keys::KeyCombo;
use crate::profiles::{expand_home, Profile, ProfileStore};
use crate::sequence::{self, ActionKind, Preset, Step};
use crate::sequence_editor::{SequenceEditor, StepForm};
use crate::ui::{AppState, MainWindow, PreviewLine, StepEdit, StepRow};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const MAIN_SIZE: (f32, f32) = (380.0, 300.0);
const LARGE_SIZE: (f32, f32) = (440.0, 600.0);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
const PREVIEW_LINE_CHARS: usize = 50;
const PREVIEW_LINES: usize = 3;

enum UiEvent {
    Engine(EngineEvent),
    Hotkey(HotkeyEvent),
}

type Shared = Rc<RefCell<Controller>>;

thread_local! {
    static CONTROLLER: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

/// Process queued background events on the UI thread.
fn drain_events() {
    let ctrl = CONTROLLER.with(|c| c.borrow().clone());
    let Some(ctrl) = ctrl else { return };
    match ctrl.try_borrow_mut() {
        Ok(mut c) => c.process_events(),
        // Busy (should not happen); try again on the next event-loop iteration.
        Err(_) => slint::Timer::single_shot(Duration::from_millis(1), drain_events),
    };
}

struct Controller {
    ui: slint::Weak<MainWindow>,
    config: Config,
    store: ConfigStore,
    profiles: ProfileStore,
    editor: SequenceEditor,
    engine: EngineHandle,
    hotkeys: HotkeyHandle,
    events: mpsc::Receiver<UiEvent>,
    save_timer: slint::Timer,
    dirty: bool,
    run_state: RunState,
    registered: Option<(Hotkeys, ActivationMode)>,
    hotkey_problem: Option<String>,
    engine_message: Option<(String, bool)>,
    notice: Option<String>,
    profile_names: Vec<String>,
    main_size: (f32, f32),
    page: i32,
    steps_model: Rc<VecModel<StepRow>>,
    profiles_model: Rc<VecModel<SharedString>>,
    preview_model: Rc<VecModel<PreviewLine>>,
}

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

fn clamp_u32(v: i32, lo: u32, hi: u32) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(0).clamp(lo, hi)
}

impl Controller {
    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    // ------------------------------------------------------------------ syncing

    fn sync_all(&mut self) {
        self.sync_settings();
        self.sync_sequence();
        self.sync_profiles();
        self.sync_run_label();
        self.refresh_status();
    }

    fn sync_settings(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let c = &self.config;
        st.set_loop_enabled(c.looping.enabled);
        st.set_preset_index(c.preset.index() as i32);
        st.set_key_ms(c.timing.key_ms as i32);
        st.set_gap_ms(c.timing.gap_ms as i32);
        st.set_loop_ms(c.timing.loop_ms as i32);
        st.set_modifier_ms(c.timing.modifier_ms as i32);
        st.set_start_delay_ms(c.start_delay_ms as i32);
        st.set_repeat_count(c.looping.count.min(i32::MAX as u32) as i32);
        st.set_activation_index(c.activation.index() as i32);
        st.set_hotkey_start(s(c.hotkeys.start_stop.to_string()));
        st.set_hotkey_emergency(s(c.hotkeys.emergency.to_string()));
        st.set_backend_index(c.input.backend.index() as i32);
        st.set_focus_protection(c.input.focus_protection);
        st.set_focus_loss_index(c.input.focus_loss.index() as i32);
        st.set_target_class(s(&c.input.target_class));
        st.set_custom_enabled(c.advanced.custom_enabled);
        st.set_active_profile(s(c.advanced.active_profile.clone().unwrap_or_default()));
    }

    fn sequence_issues(&self) -> Vec<sequence::Issue> {
        let c = &self.config;
        sequence::validate(&c.active_sequence(), &c.timing, c.looping.enabled, &c.hotkeys.reserved())
    }

    fn custom_issues(&self) -> Vec<sequence::Issue> {
        let c = &self.config;
        sequence::validate(&c.advanced.custom_sequence, &c.timing, c.looping.enabled, &c.hotkeys.reserved())
    }

    fn sync_sequence(&mut self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let c = &self.config;

        // Main window preview
        let active = c.active_sequence();
        let title = if c.advanced.custom_enabled {
            match &c.advanced.active_profile {
                Some(p) => format!("Custom sequence · {p} · {} steps", active.steps.len()),
                None => format!("Custom sequence · {} steps", active.steps.len()),
            }
        } else {
            "Default sequence".to_string()
        };
        st.set_sequence_title(s(title));
        let mut shown = active.preview_wrapped(PREVIEW_LINE_CHARS, PREVIEW_LINES);
        if shown.is_empty() {
            shown = "No steps".into();
        }
        st.set_sequence_preview(s(shown));

        // Editor
        let issues = self.custom_issues();
        let rows: Vec<StepRow> = self
            .editor
            .steps()
            .iter()
            .enumerate()
            .map(|(i, step)| StepRow {
                title: s(step.title()),
                detail: s(step.timing_summary(&c.timing, i + 1 == self.editor.steps().len())),
                error: issues.iter().any(|issue| issue.step == Some(i)),
            })
            .collect();
        self.steps_model.set_vec(rows);
        st.set_selected_step(self.editor.selected().map(|i| i as i32).unwrap_or(-1));
        self.sync_edit_form();
        let valid = issues.is_empty();
        st.set_sequence_valid(valid);
        let cycle = sequence::cycle_duration(&c.advanced.custom_sequence, &c.timing);
        let n = self.editor.steps().len();
        let validation = if valid {
            format!("{n} steps · one cycle ≈ {} ms · valid", cycle.as_millis())
        } else {
            let more = if issues.len() > 1 { format!(" (+{} more)", issues.len() - 1) } else { String::new() };
            format!("{}{more}", issues[0])
        };
        st.set_validation_text(s(&validation));
        let summary = if c.advanced.custom_enabled {
            format!("In use · {validation}")
        } else {
            format!("Not in use · {validation}")
        };
        st.set_custom_summary(s(summary));
    }

    fn sync_edit_form(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        if let Some(step) = self.editor.selected_step() {
            let f = StepForm::from_step(step, &self.config.timing);
            st.set_edit(StepEdit {
                kind: f.kind as i32,
                key: s(&f.key_text),
                button: f.button as i32,
                duration: f.duration_ms as i32,
                hold_override: f.hold_override,
                hold_ms: f.hold_ms as i32,
                delay_override: f.delay_override,
                delay_ms: f.delay_ms as i32,
            });
        }
    }

    fn sync_profiles(&mut self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        self.profile_names = self.profiles.list();
        self.profiles_model.set_vec(self.profile_names.iter().map(s).collect::<Vec<_>>());
        let idx = st.get_profile_index();
        if idx >= self.profile_names.len() as i32 {
            st.set_profile_index(-1);
        }
        st.set_active_profile(s(self.config.advanced.active_profile.clone().unwrap_or_default()));
    }

    fn sync_run_label(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let hk = self.config.hotkeys.start_stop.to_string();
        let label = match self.run_state {
            RunState::Idle => match self.config.activation {
                ActivationMode::Toggle => format!("Start  ·  {hk}"),
                ActivationMode::Hold => format!("Start  ·  hold {hk}"),
            },
            RunState::Arming => "Starting…  ·  click to cancel".to_string(),
            RunState::WaitingForTarget => "Waiting for target window…".to_string(),
            RunState::Running => format!("Stop  ·  {hk}"),
            RunState::Paused => format!("Paused  ·  {hk} stops"),
        };
        st.set_run_label(s(label));
        st.set_run_state(match self.run_state {
            RunState::Idle => 0,
            RunState::Arming => 1,
            RunState::WaitingForTarget => 2,
            RunState::Running => 3,
            RunState::Paused => 4,
        });
    }

    /// One unobtrusive status line: engine messages, then problems, then notices.
    fn refresh_status(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let (text, error) = if let Some((m, e)) = &self.engine_message {
            (m.clone(), *e)
        } else if let Some(issue) = self.sequence_issues().first() {
            let prefix = if self.config.advanced.custom_enabled { "Custom sequence: " } else { "" };
            (format!("{prefix}{issue}"), true)
        } else if let Some(p) = &self.hotkey_problem {
            (format!("Hotkeys: {p}"), true)
        } else if let Some(n) = &self.notice {
            (n.clone(), false)
        } else {
            (String::new(), false)
        };
        st.set_status_text(s(text));
        st.set_status_error(error);
    }

    // ------------------------------------------------------------------ changes

    /// Call after every configuration change.
    fn changed(&mut self) {
        self.engine.send(Command::Update(Arc::new(RunSpec::from_config(&self.config))));
        let wanted = (self.config.hotkeys, self.config.activation);
        if self.registered != Some(wanted) {
            self.registered = Some(wanted);
            self.hotkeys.send(HotkeyCommand::Configure { hotkeys: wanted.0, mode: wanted.1 });
        }
        self.schedule_save();
        self.sync_settings();
        self.sync_sequence();
        self.sync_run_label();
        self.refresh_status();
    }

    fn schedule_save(&mut self) {
        self.dirty = true;
        self.save_timer.start(slint::TimerMode::SingleShot, SAVE_DEBOUNCE, || {
            let ctrl = CONTROLLER.with(|c| c.borrow().clone());
            if let Some(ctrl) = ctrl {
                if let Ok(mut c) = ctrl.try_borrow_mut() {
                    c.save_now();
                }
            }
        });
    }

    fn save_now(&mut self) {
        if !self.dirty {
            return;
        }
        match self.store.save(&self.config) {
            Ok(()) => self.dirty = false,
            Err(e) => {
                tracing::error!("saving configuration failed: {e}");
                self.notice = Some(format!("Could not save settings: {e}"));
                self.refresh_status();
            }
        }
    }

    fn set_int(&mut self, key: &str, v: i32) {
        let c = &mut self.config;
        match key {
            "key" => {
                c.timing.key_ms = clamp_u32(v, 1, sequence::Timing::KEY_RANGE.1);
                c.refresh_preset();
            }
            "gap" => {
                c.timing.gap_ms = clamp_u32(v, 0, sequence::Timing::GAP_RANGE.1);
                c.refresh_preset();
            }
            "loop" => {
                c.timing.loop_ms = clamp_u32(v, 0, sequence::Timing::LOOP_RANGE.1);
                c.refresh_preset();
            }
            "modifier" => c.timing.modifier_ms = clamp_u32(v, 0, sequence::Timing::MODIFIER_RANGE.1),
            "repeat" => c.looping.count = clamp_u32(v, 0, crate::config::MAX_REPEAT_COUNT),
            "start-delay" => c.start_delay_ms = clamp_u32(v, 0, crate::config::MAX_START_DELAY_MS),
            "activation" => c.activation = ActivationMode::from_index(v.max(0) as usize),
            "backend" => c.input.backend = BackendKind::from_index(v.max(0) as usize),
            "focus-loss" => c.input.focus_loss = FocusLoss::from_index(v.max(0) as usize),
            other => {
                tracing::warn!("unknown int setting {other}");
                return;
            }
        }
        self.changed();
    }

    fn set_bool(&mut self, key: &str, v: bool) {
        match key {
            "loop" => self.config.looping.enabled = v,
            "focus-protection" => self.config.input.focus_protection = v,
            "custom" => self.config.advanced.custom_enabled = v,
            other => {
                tracing::warn!("unknown bool setting {other}");
                return;
            }
        }
        self.changed();
    }

    fn set_text(&mut self, key: &str, text: &str) {
        match key {
            "hotkey-start" | "hotkey-emergency" => match KeyCombo::parse(text) {
                Ok(combo) => self.set_hotkey(key == "hotkey-start", combo),
                Err(e) => {
                    self.set_hotkey_status(false, &e);
                    self.sync_settings();
                }
            },
            "target-class" => {
                let t: String = text.trim().chars().take(crate::config::MAX_TARGET_CLASS_LEN).collect();
                if t != self.config.input.target_class {
                    self.config.input.target_class = t;
                    self.changed();
                }
            }
            other => tracing::warn!("unknown text setting {other}"),
        }
    }

    fn set_hotkey(&mut self, start: bool, combo: KeyCombo) {
        let mut hk = self.config.hotkeys;
        if start {
            hk.start_stop = combo;
        } else {
            hk.emergency = combo;
        }
        if hk == self.config.hotkeys {
            self.sync_settings();
            return;
        }
        if let Some(problem) = hk.validate().into_iter().next() {
            self.set_hotkey_status(false, &problem);
            self.sync_settings();
            return;
        }
        self.config.hotkeys = hk;
        self.changed();
    }

    fn set_hotkey_status(&self, ok: bool, message: &str) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_hotkey_ok(ok);
            st.set_hotkey_status(s(message));
        }
    }

    fn select_preset(&mut self, i: i32) {
        self.config.apply_preset(Preset::from_index(i.max(0) as usize));
        self.changed();
    }

    fn reset_defaults(&mut self) {
        let keep = self.config.advanced.clone();
        self.config = Config::default();
        self.config.advanced.custom_sequence = keep.custom_sequence;
        self.config.advanced.active_profile = keep.active_profile;
        self.notice = Some("Settings reset to defaults".into());
        self.changed();
    }

    fn navigate(&mut self, page: i32) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        if !st.get_capture_target().is_empty() {
            self.hotkeys.send(HotkeyCommand::CancelCapture);
        }
        let window = ui.window();
        let scale = window.scale_factor();
        let current = window.size().to_logical(scale);
        if self.page == 0 && page != 0 {
            self.main_size = (current.width, current.height);
        }
        let from_editor = self.page == 2;
        self.page = page;
        st.set_page(page);
        st.set_show_preview(false);
        if page == 1 && from_editor {
            st.set_settings_tab(4);
        }
        let (w, h) = if page == 0 { self.main_size } else { (current.width.max(LARGE_SIZE.0), current.height.max(LARGE_SIZE.1)) };
        window.set_size(slint::LogicalSize::new(w, h));
    }

    // ------------------------------------------------------------------ editor

    fn commit_editor(&mut self) {
        self.config.advanced.custom_sequence = self.editor.sequence();
        self.changed();
    }

    fn set_edit_error(&self, msg: &str) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_edit_error(s(msg));
        }
    }

    fn editor_edit(&mut self, e: StepEdit) {
        let Some(prev) = self.editor.selected_step().cloned() else { return };
        let new_kind = ActionKind::from_index(e.kind.max(0) as usize);
        let step = if new_kind != prev.action {
            // Switching action type: start from that type's defaults, keep what carries over.
            let mut s = Step::new_of_kind(new_kind);
            if new_kind.uses_key() {
                s.key = prev.key.or(s.key);
            }
            if new_kind.uses_button() {
                s.button = prev.button.or(s.button);
            }
            if new_kind.uses_duration() {
                s.duration_ms = prev.duration_ms.or(s.duration_ms);
            }
            s.hold_ms = prev.hold_ms;
            s.delay_ms = prev.delay_ms;
            Ok(s.normalized())
        } else {
            let form = StepForm {
                kind: e.kind.max(0) as usize,
                key_text: e.key.to_string(),
                button: e.button.max(0) as usize,
                duration_ms: e.duration.max(0) as u32,
                hold_override: e.hold_override,
                hold_ms: e.hold_ms.max(1) as u32,
                delay_override: e.delay_override,
                delay_ms: e.delay_ms.max(0) as u32,
            };
            form.to_step(Some(&prev))
        };
        match step {
            Ok(step) => {
                self.set_edit_error("");
                self.editor.replace_selected(step);
                self.commit_editor();
            }
            Err(err) => self.set_edit_error(&err),
        }
    }

    fn editor_key_text(&mut self, text: &str) {
        let Some(mut step) = self.editor.selected_step().cloned() else { return };
        if !step.action.uses_key() {
            return;
        }
        match KeyCombo::parse(text) {
            Ok(combo) => {
                self.set_edit_error("");
                if step.key != Some(combo) {
                    step.key = Some(combo);
                    self.editor.replace_selected(step);
                    self.commit_editor();
                } else {
                    self.sync_edit_form();
                }
            }
            Err(e) => {
                self.set_edit_error(&format!("{e}. Use names like E, 5, Tab, F6, Num5, Shift+E, Ctrl+Alt+F1."));
                if let Some(ui) = self.ui() {
                    let st = ui.global::<AppState>();
                    let mut edit = st.get_edit();
                    edit.key = s(text);
                    st.set_edit(edit);
                }
            }
        }
    }

    fn editor_preview(&self) {
        let Some(ui) = self.ui() else { return };
        let c = &self.config;
        let lines = sequence::preview_lines(&c.advanced.custom_sequence, &c.timing, c.looping.enabled);
        self.preview_model
            .set_vec(lines.into_iter().map(|(time, action)| PreviewLine { time: s(time), action: s(action) }).collect::<Vec<_>>());
        ui.global::<AppState>().set_show_preview(true);
    }

    // ------------------------------------------------------------------ profiles

    fn profile_message(&self, ok: bool, msg: &str) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_profile_error(!ok);
            st.set_profile_message(s(msg));
        }
    }

    fn profile_name(&self, index: i32) -> Option<String> {
        usize::try_from(index).ok().and_then(|i| self.profile_names.get(i).cloned())
    }

    fn select_profile_named(&mut self, name: &str) {
        self.sync_profiles();
        if let Some(ui) = self.ui() {
            let idx = self.profile_names.iter().position(|n| n == name).map(|i| i as i32).unwrap_or(-1);
            ui.global::<AppState>().set_profile_index(idx);
        }
    }

    fn profile_save(&mut self, name: &str) {
        let c = &self.config;
        let profile = Profile::new(name, &c.advanced.custom_sequence, Some(c.timing), Some(c.looping));
        match self.profiles.save(&profile) {
            Ok(()) => {
                let stored = crate::profiles::validate_name(name).unwrap_or_else(|_| name.to_string());
                self.config.advanced.active_profile = Some(stored.clone());
                self.changed();
                self.select_profile_named(&stored);
                self.profile_message(true, &format!("Saved the custom sequence as \"{stored}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_load(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.load(&name) {
            Ok(p) => {
                self.config.advanced.custom_sequence = p.sequence();
                if let Some(t) = p.timing {
                    self.config.timing = t;
                    self.config.refresh_preset();
                }
                if let Some(l) = p.looping {
                    self.config.looping = l;
                }
                self.config.advanced.custom_enabled = true;
                self.config.advanced.active_profile = Some(p.name.clone());
                self.editor = SequenceEditor::new(&self.config.advanced.custom_sequence);
                self.changed();
                self.sync_profiles();
                self.profile_message(true, &format!("Loaded \"{}\"; it is now the active custom sequence", p.name));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_rename(&mut self, index: i32, new: &str) {
        let Some(old) = self.profile_name(index) else { return };
        match self.profiles.rename(&old, new) {
            Ok(stored) => {
                if self.config.advanced.active_profile.as_deref() == Some(old.as_str()) {
                    self.config.advanced.active_profile = Some(stored.clone());
                    self.changed();
                }
                self.select_profile_named(&stored);
                self.profile_message(true, &format!("Renamed to \"{stored}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_duplicate(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.duplicate(&name) {
            Ok(copy) => {
                self.select_profile_named(&copy);
                self.profile_message(true, &format!("Created \"{copy}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_delete(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.delete(&name) {
            Ok(()) => {
                if self.config.advanced.active_profile.as_deref() == Some(name.as_str()) {
                    self.config.advanced.active_profile = None;
                    self.changed();
                }
                self.sync_profiles();
                if let Some(ui) = self.ui() {
                    ui.global::<AppState>().set_profile_index(-1);
                }
                self.profile_message(true, &format!("Deleted \"{name}\" (the current custom sequence is unchanged)"));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_export(&mut self, index: i32, path: &str) {
        let Some(name) = self.profile_name(index) else { return };
        if path.trim().is_empty() {
            self.profile_message(false, "Enter a file or folder path to export to");
            return;
        }
        match self.profiles.export(&name, &expand_home(path)) {
            Ok(out) => self.profile_message(true, &format!("Exported to {}", out.display())),
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_import(&mut self, path: &str) {
        match self.profiles.import(&expand_home(path)) {
            Ok(name) => {
                self.select_profile_named(&name);
                self.profile_message(true, &format!("Imported as \"{name}\". Select it and click Load to use it."));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    // ------------------------------------------------------------------ capture / detect

    fn capture(&mut self, target: &str) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_capture_target(s(target));
        }
        self.hotkeys.send(HotkeyCommand::Capture { purpose: target.to_string() });
    }

    fn on_captured(&mut self, purpose: &str, result: Result<KeyCombo, String>) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_capture_target(s(""));
        }
        match (purpose, result) {
            ("start", Ok(combo)) => self.set_hotkey(true, combo),
            ("emergency", Ok(combo)) => self.set_hotkey(false, combo),
            ("step", Ok(combo)) => self.editor_key_text(&combo.to_string()),
            ("step", Err(e)) => self.set_edit_error(if e == "Capture cancelled" { "" } else { &e }),
            (_, Err(e)) if e != "Capture cancelled" => self.set_hotkey_status(false, &e),
            _ => {}
        }
    }

    fn detect_target(&mut self) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_detecting(true);
            st.set_input_message(s("Focus the game window now…"));
        }
        self.hotkeys.send(HotkeyCommand::DetectWindow { delay: Duration::from_secs(3) });
    }

    // ------------------------------------------------------------------ background events

    fn process_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                UiEvent::Engine(e) => self.on_engine(e),
                UiEvent::Hotkey(e) => self.on_hotkey(e),
            }
        }
    }

    fn on_engine(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::State { state, detail } => {
                self.run_state = state;
                self.engine_message = match state {
                    RunState::Idle | RunState::Running => None,
                    RunState::Arming if detail.ends_with(" ms") => Some((format!("Starting in {detail}…"), false)),
                    RunState::Arming => Some((format!("{detail}…"), false)),
                    RunState::WaitingForTarget | RunState::Paused => Some((format!("{detail}…"), false)),
                };
                self.sync_run_label();
                self.refresh_status();
            }
            EngineEvent::Finished { reason, cycles, message, .. } => {
                self.run_state = RunState::Idle;
                self.engine_message = match reason {
                    StopReason::Completed => Some((format!("Finished after {cycles} cycle{}", if cycles == 1 { "" } else { "s" }), false)),
                    StopReason::Stopped | StopReason::Shutdown => None,
                    StopReason::Emergency | StopReason::FocusLost => Some((message, false)),
                    StopReason::Failed => Some((message, true)),
                };
                self.sync_run_label();
                self.refresh_status();
            }
        }
    }

    fn on_hotkey(&mut self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Registered { ok, message } => {
                self.hotkey_problem = (!ok).then(|| message.clone());
                self.set_hotkey_status(ok, &if ok { format!("Registered: {message}") } else { message });
                self.refresh_status();
            }
            HotkeyEvent::Captured { purpose, result } => self.on_captured(&purpose, result),
            HotkeyEvent::Detected(result) => {
                let msg = match result {
                    Ok(w) => {
                        let class = w.class.split_whitespace().next().unwrap_or_default().to_string();
                        let msg = format!("Detected \"{}\"", w.class);
                        if class != self.config.input.target_class {
                            self.config.input.target_class = class;
                            self.changed();
                        }
                        msg
                    }
                    Err(e) => e,
                };
                if let Some(ui) = self.ui() {
                    let st = ui.global::<AppState>();
                    st.set_detecting(false);
                    st.set_input_message(s(msg));
                }
            }
            HotkeyEvent::Fatal(e) => {
                self.hotkey_problem = Some(e.clone());
                self.set_hotkey_status(false, &e);
                self.refresh_status();
            }
        }
    }
}

/// Build the window, start the worker threads and run the UI until the window closes.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = crate::config::default_config_dir()?;
    let store = ConfigStore::new(&config_dir);
    let loaded = store.load();
    for n in &loaded.notices {
        tracing::warn!("{n}");
    }
    let profiles = ProfileStore::new(store.profiles_dir());

    let ui = MainWindow::new()?;
    let (tx, rx) = mpsc::channel::<UiEvent>();

    let engine_tx = tx.clone();
    let engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        if engine_tx.send(UiEvent::Engine(ev)).is_ok() {
            let _ = slint::invoke_from_event_loop(drain_events);
        }
    })?;
    let hotkey_tx = tx;
    let hotkeys = HotkeyHandle::spawn(engine.sender(), move |ev| {
        if hotkey_tx.send(UiEvent::Hotkey(ev)).is_ok() {
            let _ = slint::invoke_from_event_loop(drain_events);
        }
    })?;

    // SIGINT/SIGTERM/SIGHUP: stop and release input, then leave the event loop normally so
    // that the configuration is saved and threads are joined.
    let signal_engine = engine.sender();
    crate::safety::spawn_signal_watcher(move |_| {
        let _ = signal_engine.send(Command::Emergency);
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    })?;

    let steps_model = Rc::new(VecModel::<StepRow>::default());
    let profiles_model = Rc::new(VecModel::<SharedString>::default());
    let preview_model = Rc::new(VecModel::<PreviewLine>::default());
    {
        let st = ui.global::<AppState>();
        st.set_steps(ModelRc::from(steps_model.clone()));
        st.set_profiles(ModelRc::from(profiles_model.clone()));
        st.set_preview_lines(ModelRc::from(preview_model.clone()));
        let labels: Vec<SharedString> = ActionKind::ALL.iter().map(|k| s(k.label())).collect();
        st.set_action_labels(ModelRc::new(VecModel::from(labels)));
        st.set_config_path(s(store.config_path().display().to_string()));
        st.set_profiles_dir(s(store.profiles_dir().display().to_string()));
    }

    let editor = SequenceEditor::new(&loaded.config.advanced.custom_sequence);
    let ctrl: Shared = Rc::new(RefCell::new(Controller {
        ui: ui.as_weak(),
        config: loaded.config,
        store,
        profiles,
        editor,
        engine,
        hotkeys,
        events: rx,
        save_timer: slint::Timer::default(),
        dirty: false,
        run_state: RunState::Idle,
        registered: None,
        hotkey_problem: None,
        engine_message: None,
        notice: loaded.notices.first().cloned(),
        profile_names: Vec::new(),
        main_size: MAIN_SIZE,
        page: 0,
        steps_model,
        profiles_model,
        preview_model,
    }));
    CONTROLLER.with(|c| *c.borrow_mut() = Some(ctrl.clone()));
    register_callbacks(&ui, &ctrl);

    {
        let mut c = ctrl.borrow_mut();
        c.changed();
        c.dirty = false; // nothing to save yet; `changed` only pushed state to the threads
        c.sync_all();
    }

    ui.run()?;

    // Orderly shutdown: release input and join threads, then persist settings.
    let ctrl = CONTROLLER.with(|c| c.borrow_mut().take()).unwrap_or(ctrl);
    let mut c = ctrl.borrow_mut();
    c.save_timer.stop();
    c.engine.shutdown();
    c.hotkeys.shutdown();
    c.save_now();
    Ok(())
}

fn register_callbacks(ui: &MainWindow, ctrl: &Shared) {
    let st = ui.global::<AppState>();

    macro_rules! on {
        ($setter:ident, |$c:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
            let weak = Rc::downgrade(ctrl);
            st.$setter(move |$($arg: $ty),*| {
                if let Some(rc) = weak.upgrade() {
                    match rc.try_borrow_mut() {
                        Ok(mut guard) => {
                            let $c: &mut Controller = &mut guard;
                            $body;
                        }
                        Err(_) => tracing::warn!(concat!(stringify!($setter), " ignored: controller busy")),
                    }
                }
            });
        }};
    }

    on!(on_navigate, |c, page: i32| c.navigate(page));
    on!(on_toggle_run, |c| c.engine.send(Command::Toggle));
    on!(on_set_int, |c, key: SharedString, v: i32| c.set_int(&key, v));
    on!(on_set_bool, |c, key: SharedString, v: bool| c.set_bool(&key, v));
    on!(on_set_text, |c, key: SharedString, v: SharedString| c.set_text(&key, &v));
    on!(on_select_preset, |c, i: i32| c.select_preset(i));
    on!(on_capture, |c, target: SharedString| c.capture(&target));
    on!(on_cancel_capture, |c| c.hotkeys.send(HotkeyCommand::CancelCapture));
    on!(on_detect_target, |c| c.detect_target());
    on!(on_reset_defaults, |c| c.reset_defaults());

    on!(on_profile_save, |c, name: SharedString| c.profile_save(&name));
    on!(on_profile_load, |c, i: i32| c.profile_load(i));
    on!(on_profile_rename, |c, i: i32, name: SharedString| c.profile_rename(i, &name));
    on!(on_profile_duplicate, |c, i: i32| c.profile_duplicate(i));
    on!(on_profile_delete, |c, i: i32| c.profile_delete(i));
    on!(on_profile_export, |c, i: i32, path: SharedString| c.profile_export(i, &path));
    on!(on_profile_import, |c, path: SharedString| c.profile_import(&path));

    on!(on_editor_add, |c, i: i32| {
        match c.editor.add(ActionKind::from_index(i.max(0) as usize)) {
            Ok(_) => {
                c.set_edit_error("");
                c.commit_editor();
            }
            Err(e) => c.set_edit_error(&e),
        }
    });
    on!(on_editor_select, |c, i: i32| {
        c.editor.select(usize::try_from(i).ok());
        c.set_edit_error("");
        c.sync_sequence();
    });
    on!(on_editor_delete, |c| {
        c.editor.delete_selected();
        c.commit_editor();
    });
    on!(on_editor_duplicate, |c| {
        match c.editor.duplicate_selected() {
            Ok(()) => c.commit_editor(),
            Err(e) => c.set_edit_error(&e),
        }
    });
    on!(on_editor_move, |c, delta: i32| {
        c.editor.move_selected(delta);
        c.commit_editor();
    });
    on!(on_editor_reset, |c| {
        c.editor.reset_default();
        c.set_edit_error("");
        c.commit_editor();
    });
    on!(on_editor_edit, |c, e: StepEdit| c.editor_edit(e));
    on!(on_editor_key_text, |c, text: SharedString| c.editor_key_text(&text));
    on!(on_editor_preview, |c| c.editor_preview());
}
