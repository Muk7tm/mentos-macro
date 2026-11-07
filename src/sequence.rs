//! Sequence model, validation and compilation into an immutable execution plan.
//!
//! A [`Sequence`] is what users edit (steps such as "Press Shift+E"). Before it runs it is
//! validated and compiled into a [`Plan`]: a flat list of key/button transitions and waits
//! with every timing resolved. The worker only ever executes a `Plan`, which it owns, so
//! editing the sequence while a macro runs can never affect the running execution.

use crate::input::KeyResolver;
use crate::keys::{self, KeyCombo, Keysym, Mods, MouseButton};
use crate::timing::ms;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

pub const MAX_STEPS: usize = 500;
pub const MAX_DURATION_MS: u32 = 60_000;
/// A looping cycle shorter than this would flood the X server.
pub const MIN_CYCLE: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Key (or combination) down, hold for the key duration, up.
    #[default]
    Press,
    KeyDown,
    KeyUp,
    /// Like `Press` but with an explicit hold duration.
    Hold,
    MousePress,
    MouseDown,
    MouseUp,
    MouseHold,
    Wait,
}

impl ActionKind {
    pub const ALL: [ActionKind; 9] = [
        ActionKind::Press,
        ActionKind::KeyDown,
        ActionKind::KeyUp,
        ActionKind::Hold,
        ActionKind::MousePress,
        ActionKind::MouseDown,
        ActionKind::MouseUp,
        ActionKind::MouseHold,
        ActionKind::Wait,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Press => "Press key",
            ActionKind::KeyDown => "Key down",
            ActionKind::KeyUp => "Key up",
            ActionKind::Hold => "Hold key",
            ActionKind::MousePress => "Click mouse",
            ActionKind::MouseDown => "Mouse down",
            ActionKind::MouseUp => "Mouse up",
            ActionKind::MouseHold => "Hold mouse",
            ActionKind::Wait => "Wait",
        }
    }
    pub fn index(self) -> usize {
        ActionKind::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Self {
        ActionKind::ALL.get(i).copied().unwrap_or_default()
    }
    pub fn uses_key(self) -> bool {
        matches!(self, ActionKind::Press | ActionKind::KeyDown | ActionKind::KeyUp | ActionKind::Hold)
    }
    pub fn uses_button(self) -> bool {
        matches!(self, ActionKind::MousePress | ActionKind::MouseDown | ActionKind::MouseUp | ActionKind::MouseHold)
    }
    /// Kinds whose `duration_ms` field is required.
    pub fn uses_duration(self) -> bool {
        matches!(self, ActionKind::Hold | ActionKind::MouseHold | ActionKind::Wait)
    }
    /// Kinds that hold something for the (overridable) global key duration.
    pub fn uses_hold_override(self) -> bool {
        matches!(self, ActionKind::Press | ActionKind::MousePress)
    }
    pub fn uses_post_delay(self) -> bool {
        self != ActionKind::Wait
    }
}

/// One editable step. Fields that do not apply to the action are ignored (and not saved).
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Step {
    pub action: ActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyCombo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<MouseButton>,
    /// Hold length for Hold/MouseHold, wait length for Wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// Overrides the global key-press duration for Press/MousePress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
    /// Overrides the global delay after this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u32>,
}

impl Step {
    pub fn press(key: KeyCombo) -> Step {
        Step { action: ActionKind::Press, key: Some(key), ..Step::default() }
    }
    pub fn key_down(key: KeyCombo) -> Step {
        Step { action: ActionKind::KeyDown, key: Some(key), ..Step::default() }
    }
    pub fn key_up(key: KeyCombo) -> Step {
        Step { action: ActionKind::KeyUp, key: Some(key), ..Step::default() }
    }
    pub fn hold(key: KeyCombo, duration_ms: u32) -> Step {
        Step { action: ActionKind::Hold, key: Some(key), duration_ms: Some(duration_ms), ..Step::default() }
    }
    pub fn mouse(action: ActionKind, button: MouseButton) -> Step {
        Step { action, button: Some(button), ..Step::default() }
    }
    pub fn wait(duration_ms: u32) -> Step {
        Step { action: ActionKind::Wait, duration_ms: Some(duration_ms), ..Step::default() }
    }

    /// A fresh step of `kind` with sensible defaults, used by the editor's "Add".
    pub fn new_of_kind(kind: ActionKind) -> Step {
        let mut s = Step { action: kind, ..Step::default() };
        if kind.uses_key() {
            s.key = Some(KeyCombo::new(0x65)); // E
        }
        if kind.uses_button() {
            s.button = Some(MouseButton::Left);
        }
        if kind.uses_duration() {
            s.duration_ms = Some(if kind == ActionKind::Wait { 50 } else { 250 });
        }
        s
    }

    /// Remove fields that do not apply to the action so saved files stay clean.
    pub fn normalized(mut self) -> Step {
        let k = self.action;
        if !k.uses_key() {
            self.key = None;
        }
        if !k.uses_button() {
            self.button = None;
        }
        if !k.uses_duration() {
            self.duration_ms = None;
        }
        if !k.uses_hold_override() {
            self.hold_ms = None;
        }
        if !k.uses_post_delay() {
            self.delay_ms = None;
        }
        self
    }

    /// One-line description such as "Press Shift+E" or "Wait 50 ms".
    pub fn title(&self) -> String {
        let key = self.key.map(|k| k.to_string()).unwrap_or_else(|| "?".into());
        let button = self.button.map(|b| b.label()).unwrap_or("?");
        let dur = self.duration_ms.map(|d| d.to_string()).unwrap_or_else(|| "?".into());
        match self.action {
            ActionKind::Press => format!("Press {key}"),
            ActionKind::KeyDown => format!("Key down {key}"),
            ActionKind::KeyUp => format!("Key up {key}"),
            ActionKind::Hold => format!("Hold {key} for {dur} ms"),
            ActionKind::MousePress => format!("Click {button} mouse"),
            ActionKind::MouseDown => format!("{button} mouse down"),
            ActionKind::MouseUp => format!("{button} mouse up"),
            ActionKind::MouseHold => format!("Hold {button} mouse for {dur} ms"),
            ActionKind::Wait => format!("Wait {dur} ms"),
        }
    }

    /// Timing summary for the editor list, e.g. "hold 25 ms · then 15 ms". For the last
    /// step the delay shown is the loop delay, which replaces the gap between cycles.
    /// Values marked `*` are per-step overrides.
    pub fn timing_summary(&self, timing: &Timing, last: bool) -> String {
        let mut parts = Vec::new();
        if self.action.uses_hold_override() {
            match self.hold_ms {
                Some(h) => parts.push(format!("hold {h} ms*")),
                None => parts.push(format!("hold {} ms", timing.key_ms)),
            }
        }
        if self.action.uses_post_delay() {
            match (self.delay_ms, last) {
                (Some(d), _) => parts.push(format!("then {d} ms*")),
                (None, true) => parts.push(format!("loop {} ms", timing.loop_ms)),
                (None, false) => parts.push(format!("then {} ms", timing.gap_ms)),
            }
        } else if last {
            parts.push(format!("loop {} ms", timing.loop_ms));
        }
        parts.join(" · ")
    }
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Sequence {
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Sequence {
    /// `5 → Tab → 6 → Tab → 7 → Tab → 8 → Tab → 9 → Tab → 0 → Tab`
    pub fn default_sequence() -> Sequence {
        let tab = KeyCombo::new(keys::XK_TAB);
        let steps = ['5', '6', '7', '8', '9', '0']
            .into_iter()
            .flat_map(|c| [Step::press(KeyCombo::new(c as Keysym)), Step::press(tab)])
            .collect();
        Sequence { steps }
    }

    fn preview_tokens(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|s| match s.action {
                ActionKind::Press => s.key.map(|k| k.to_string()).unwrap_or_else(|| "?".into()),
                _ => s.title(),
            })
            .collect()
    }

    /// Compact "5 › Tab › 6 …" preview for the main window.
    pub fn preview(&self) -> String {
        self.preview_tokens().join("  ›  ")
    }

    /// The preview broken into at most `max_lines` lines of roughly `line_chars`
    /// characters. Tokens are never split and "›" always ends a line rather than starting
    /// one; anything that does not fit is summarized as "… (+N)".
    pub fn preview_wrapped(&self, line_chars: usize, max_lines: usize) -> String {
        let tokens = self.preview_tokens();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut used = 0;
        for (i, tok) in tokens.iter().enumerate() {
            let piece = if i + 1 < tokens.len() { format!("{tok}  ›") } else { tok.clone() };
            let needed = if current.is_empty() { piece.chars().count() } else { current.chars().count() + 2 + piece.chars().count() };
            if !current.is_empty() && needed > line_chars {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines {
                    break;
                }
            }
            if !current.is_empty() {
                current.push_str("  ");
            }
            current.push_str(&piece);
            used = i + 1;
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(current);
        }
        let rest = tokens.len() - used;
        if rest > 0 {
            if let Some(last) = lines.last_mut() {
                last.push_str(&format!("  …  (+{rest})"));
            }
        }
        lines.join("\n")
    }
}

/// Global timing values (milliseconds).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Timing {
    /// How long each key (and mouse click) is held down.
    pub key_ms: u32,
    /// Delay after each step.
    pub gap_ms: u32,
    /// Delay between the last step of a cycle and the first step of the next.
    pub loop_ms: u32,
    /// Delay between pressing a combination's modifiers and its key.
    pub modifier_ms: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Preset::Balanced.timing()
    }
}

impl Timing {
    pub const KEY_RANGE: (u32, u32) = (1, 10_000);
    pub const GAP_RANGE: (u32, u32) = (0, 10_000);
    pub const LOOP_RANGE: (u32, u32) = (0, MAX_DURATION_MS);
    pub const MODIFIER_RANGE: (u32, u32) = (0, 1_000);

    /// Clamp every value into its valid range; returns true if anything changed.
    pub fn sanitize(&mut self) -> bool {
        let before = *self;
        self.key_ms = self.key_ms.clamp(Self::KEY_RANGE.0, Self::KEY_RANGE.1);
        self.gap_ms = self.gap_ms.clamp(Self::GAP_RANGE.0, Self::GAP_RANGE.1);
        self.loop_ms = self.loop_ms.clamp(Self::LOOP_RANGE.0, Self::LOOP_RANGE.1);
        self.modifier_ms = self.modifier_ms.clamp(Self::MODIFIER_RANGE.0, Self::MODIFIER_RANGE.1);
        before != *self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Fast,
    #[default]
    Balanced,
    Stable,
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Fast, Preset::Balanced, Preset::Stable, Preset::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Fast",
            Preset::Balanced => "Balanced",
            Preset::Stable => "Stable",
            Preset::Custom => "Custom",
        }
    }

    /// Key/gap/loop values of a preset. Modifier lead is kept at 5 ms for all presets.
    pub fn timing(self) -> Timing {
        let (key_ms, gap_ms, loop_ms) = match self {
            Preset::Fast => (15, 8, 20),
            Preset::Balanced | Preset::Custom => (25, 15, 40),
            Preset::Stable => (40, 30, 80),
        };
        Timing { key_ms, gap_ms, loop_ms, modifier_ms: 5 }
    }

    /// Which named preset (if any) matches the given timing.
    pub fn detect(t: &Timing) -> Preset {
        [Preset::Fast, Preset::Balanced, Preset::Stable]
            .into_iter()
            .find(|p| {
                let pt = p.timing();
                pt.key_ms == t.key_ms && pt.gap_ms == t.gap_ms && pt.loop_ms == t.loop_ms
            })
            .unwrap_or(Preset::Custom)
    }

    pub fn index(self) -> usize {
        Preset::ALL.iter().position(|p| *p == self).unwrap_or(1)
    }
    pub fn from_index(i: usize) -> Self {
        Preset::ALL.get(i).copied().unwrap_or_default()
    }
}

/// A problem found during validation. `step` is the zero-based step index, if specific.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub step: Option<usize>,
    pub message: String,
}

impl Issue {
    fn at(step: usize, message: impl Into<String>) -> Issue {
        Issue { step: Some(step), message: message.into() }
    }
    fn general(message: impl Into<String>) -> Issue {
        Issue { step: None, message: message.into() }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.step {
            Some(i) => write!(f, "Step {}: {}", i + 1, self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Keys the sequence must never press because they would trigger this app's hotkeys.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReservedKeys {
    pub start_stop: Option<KeyCombo>,
    pub emergency: Option<KeyCombo>,
}

impl ReservedKeys {
    fn conflict(&self, sym: Keysym) -> Option<&'static str> {
        if self.start_stop.is_some_and(|k| k.key == sym) {
            Some("start/stop")
        } else if self.emergency.is_some_and(|k| k.key == sym) {
            Some("emergency stop")
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Held {
    Key(Keysym),
    Button(MouseButton),
}

fn held_name(h: Held) -> String {
    match h {
        Held::Key(k) => keys::key_label(k),
        Held::Button(b) => format!("{} mouse", b.label()),
    }
}

/// Validate structure, timing values, key/button balance and hotkey conflicts.
pub fn validate(seq: &Sequence, timing: &Timing, looping: bool, reserved: &ReservedKeys) -> Vec<Issue> {
    let mut issues = Vec::new();
    if seq.steps.is_empty() {
        issues.push(Issue::general("The sequence has no steps"));
        return issues;
    }
    if seq.steps.len() > MAX_STEPS {
        issues.push(Issue::general(format!("The sequence has more than {MAX_STEPS} steps")));
    }
    let mut held: Vec<(Held, usize)> = Vec::new();
    let is_held = |held: &Vec<(Held, usize)>, h: Held| held.iter().find(|(x, _)| *x == h).map(|(_, i)| *i);

    for (i, step) in seq.steps.iter().enumerate() {
        let kind = step.action;
        let key = if kind.uses_key() {
            match step.key {
                Some(k) => Some(k),
                None => {
                    issues.push(Issue::at(i, "no key selected"));
                    None
                }
            }
        } else {
            None
        };
        let button = if kind.uses_button() {
            match step.button {
                Some(b) => Some(b),
                None => {
                    issues.push(Issue::at(i, "no mouse button selected"));
                    None
                }
            }
        } else {
            None
        };
        if kind.uses_duration() {
            match step.duration_ms {
                None => issues.push(Issue::at(i, "no duration set")),
                Some(0) if kind != ActionKind::Wait => issues.push(Issue::at(i, "hold duration must be at least 1 ms")),
                Some(d) if d > MAX_DURATION_MS => {
                    issues.push(Issue::at(i, format!("duration exceeds {MAX_DURATION_MS} ms")))
                }
                _ => {}
            }
        }
        if let Some(h) = step.hold_ms.filter(|_| kind.uses_hold_override()) {
            if h == 0 || h > Timing::KEY_RANGE.1 {
                issues.push(Issue::at(i, format!("hold override must be 1–{} ms", Timing::KEY_RANGE.1)));
            }
        }
        if let Some(d) = step.delay_ms.filter(|_| kind.uses_post_delay()) {
            if d > MAX_DURATION_MS {
                issues.push(Issue::at(i, format!("delay override exceeds {MAX_DURATION_MS} ms")));
            }
        }

        if let Some(combo) = key {
            for sym in combo.keysyms() {
                if let Some(which) = reserved.conflict(sym) {
                    issues.push(Issue::at(
                        i,
                        format!("{} is the {which} hotkey; pressing it would toggle the macro", keys::key_label(sym)),
                    ));
                }
            }
            let syms: Vec<Held> = combo.keysyms().into_iter().map(Held::Key).collect();
            match kind {
                ActionKind::Press | ActionKind::Hold | ActionKind::KeyDown => {
                    for h in &syms {
                        if let Some(at) = is_held(&held, *h) {
                            issues.push(Issue::at(
                                i,
                                format!("{} is already held down by step {}", held_name(*h), at + 1),
                            ));
                        }
                    }
                    if kind == ActionKind::KeyDown {
                        for h in syms {
                            if is_held(&held, h).is_none() {
                                held.push((h, i));
                            }
                        }
                    }
                }
                ActionKind::KeyUp => {
                    for h in syms {
                        match held.iter().position(|(x, _)| *x == h) {
                            Some(p) => {
                                held.remove(p);
                            }
                            None => issues.push(Issue::at(
                                i,
                                format!("releases {} which no earlier step pressed down", held_name(h)),
                            )),
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(b) = button {
            let h = Held::Button(b);
            match kind {
                ActionKind::MousePress | ActionKind::MouseHold | ActionKind::MouseDown => {
                    if let Some(at) = is_held(&held, h) {
                        issues.push(Issue::at(i, format!("{} is already held down by step {}", held_name(h), at + 1)));
                    } else if kind == ActionKind::MouseDown {
                        held.push((h, i));
                    }
                }
                ActionKind::MouseUp => match held.iter().position(|(x, _)| *x == h) {
                    Some(p) => {
                        held.remove(p);
                    }
                    None => issues.push(Issue::at(i, format!("releases {} which no earlier step pressed down", held_name(h)))),
                },
                _ => {}
            }
        }
    }
    for (h, at) in held {
        issues.push(Issue::at(at, format!("{} is pressed down but never released", held_name(h))));
    }
    if issues.is_empty() && looping {
        let cycle = cycle_duration(seq, timing);
        if cycle < MIN_CYCLE {
            issues.push(Issue::general(format!(
                "One loop takes only {} ms; looping needs at least {} ms per cycle",
                cycle.as_millis(),
                MIN_CYCLE.as_millis()
            )));
        }
    }
    issues
}

/// Nominal length of one cycle including the loop delay.
pub fn cycle_duration(seq: &Sequence, timing: &Timing) -> Duration {
    let ops = compile_ops(seq, timing, &PreviewResolver).unwrap_or_default();
    let waits: Duration = ops.iter().filter_map(|op| if let Op::Wait(d) = op { Some(*d) } else { None }).sum();
    waits + ms(timing.loop_ms)
}

/// One primitive the worker executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Key { keycode: u8, sym: Keysym, down: bool },
    Button { button: MouseButton, down: bool },
    Wait(Duration),
}

/// Validated, fully resolved, immutable execution plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Ops of one cycle. The delay after the last step is *not* included.
    pub ops: Vec<Op>,
    /// Delay between cycles when looping.
    pub loop_wait: Duration,
    /// Every keycode the plan can press (for emergency release).
    pub keycodes: Vec<u8>,
    pub buttons: Vec<MouseButton>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    Invalid(Vec<Issue>),
    UnmappedKey { step: usize, key: String },
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Invalid(issues) => {
                let first = issues.first().map(|i| i.to_string()).unwrap_or_default();
                if issues.len() > 1 {
                    write!(f, "{first} (+{} more)", issues.len() - 1)
                } else {
                    f.write_str(&first)
                }
            }
            CompileError::UnmappedKey { step, key } => {
                write!(f, "Step {}: key {key} does not exist on the current keyboard layout", step + 1)
            }
        }
    }
}

/// Resolver used for previews and cycle-length estimates (no X server required). It
/// assumes a US layout for which shifted punctuation needs Shift.
pub struct PreviewResolver;

impl KeyResolver for PreviewResolver {
    fn resolve(&self, sym: Keysym) -> Option<crate::input::KeyLoc> {
        let needs_shift = (0x41..=0x5a).contains(&sym) || "!\"#$%&()*+:<>?@^_{|}~".chars().any(|c| c as Keysym == sym);
        Some(crate::input::KeyLoc { keycode: 0, needs_shift })
    }
}

struct Builder<'a> {
    ops: Vec<Op>,
    resolver: &'a dyn KeyResolver,
    timing: &'a Timing,
    step: usize,
}

impl Builder<'_> {
    fn resolve(&self, sym: Keysym) -> Result<crate::input::KeyLoc, CompileError> {
        self.resolver
            .resolve(sym)
            .ok_or_else(|| CompileError::UnmappedKey { step: self.step, key: keys::key_label(sym) })
    }

    fn wait(&mut self, d: Duration) {
        if !d.is_zero() {
            self.ops.push(Op::Wait(d));
        }
    }

    fn key(&mut self, sym: Keysym, down: bool) -> Result<(), CompileError> {
        let loc = self.resolve(sym)?;
        self.ops.push(Op::Key { keycode: loc.keycode, sym, down });
        Ok(())
    }

    /// Modifiers (plus Shift if the key lives on the shifted level) for a combination.
    fn modifiers_for(&self, combo: &KeyCombo) -> Result<Vec<Keysym>, CompileError> {
        let mut mods = combo.mods;
        let loc = self.resolve(combo.key)?;
        if loc.needs_shift && keys::modifier_of(combo.key).is_none() {
            mods.insert(Mods::SHIFT);
        }
        Ok(mods.iter().map(Mods::keysym).collect())
    }

    fn combo_down(&mut self, combo: &KeyCombo) -> Result<(), CompileError> {
        let mods = self.modifiers_for(combo)?;
        for m in &mods {
            self.key(*m, true)?;
        }
        if !mods.is_empty() {
            self.wait(ms(self.timing.modifier_ms));
        }
        self.key(combo.key, true)
    }

    fn combo_up(&mut self, combo: &KeyCombo) -> Result<(), CompileError> {
        let mods = self.modifiers_for(combo)?;
        self.key(combo.key, false)?;
        for m in mods.iter().rev() {
            self.key(*m, false)?;
        }
        Ok(())
    }
}

fn compile_ops(seq: &Sequence, timing: &Timing, resolver: &dyn KeyResolver) -> Result<Vec<Op>, CompileError> {
    let mut b = Builder { ops: Vec::new(), resolver, timing, step: 0 };
    let last = seq.steps.len().saturating_sub(1);
    for (i, step) in seq.steps.iter().enumerate() {
        b.step = i;
        let key = step.key.unwrap_or(KeyCombo::new(0));
        let button = step.button.unwrap_or_default();
        let duration = ms(step.duration_ms.unwrap_or(0));
        let hold = ms(step.hold_ms.unwrap_or(timing.key_ms));
        match step.action {
            ActionKind::Press => {
                b.combo_down(&key)?;
                b.wait(hold);
                b.combo_up(&key)?;
            }
            ActionKind::Hold => {
                b.combo_down(&key)?;
                b.wait(duration);
                b.combo_up(&key)?;
            }
            ActionKind::KeyDown => b.combo_down(&key)?,
            ActionKind::KeyUp => b.combo_up(&key)?,
            ActionKind::MousePress => {
                b.ops.push(Op::Button { button, down: true });
                b.wait(hold);
                b.ops.push(Op::Button { button, down: false });
            }
            ActionKind::MouseHold => {
                b.ops.push(Op::Button { button, down: true });
                b.wait(duration);
                b.ops.push(Op::Button { button, down: false });
            }
            ActionKind::MouseDown => b.ops.push(Op::Button { button, down: true }),
            ActionKind::MouseUp => b.ops.push(Op::Button { button, down: false }),
            ActionKind::Wait => b.wait(duration),
        }
        // The delay after the final step is replaced by the loop delay.
        if step.action.uses_post_delay() && i != last {
            b.wait(ms(step.delay_ms.unwrap_or(timing.gap_ms)));
        }
    }
    Ok(b.ops)
}

/// Validate and compile `seq` into a plan using the live keymap in `resolver`.
pub fn compile(
    seq: &Sequence,
    timing: &Timing,
    looping: bool,
    reserved: &ReservedKeys,
    resolver: &dyn KeyResolver,
) -> Result<Plan, CompileError> {
    let issues = validate(seq, timing, looping, reserved);
    if !issues.is_empty() {
        return Err(CompileError::Invalid(issues));
    }
    let ops = compile_ops(seq, timing, resolver)?;
    let mut keycodes = Vec::new();
    let mut buttons = Vec::new();
    for op in &ops {
        match *op {
            Op::Key { keycode, .. } if !keycodes.contains(&keycode) => keycodes.push(keycode),
            Op::Button { button, .. } if !buttons.contains(&button) => buttons.push(button),
            _ => {}
        }
    }
    let last_delay = seq.steps.last().and_then(|s| s.delay_ms.filter(|_| s.action.uses_post_delay()));
    let loop_wait = ms(last_delay.unwrap_or(timing.loop_ms));
    Ok(Plan { ops, loop_wait, keycodes, buttons })
}

/// Execution order with cumulative timestamps for the editor preview: `(time, action)`.
pub fn preview_lines(seq: &Sequence, timing: &Timing, looping: bool) -> Vec<(String, String)> {
    let ops = match compile_ops(seq, timing, &PreviewResolver) {
        Ok(ops) => ops,
        Err(e) => return vec![(String::new(), e.to_string())],
    };
    let mut lines = Vec::new();
    let mut t = Duration::ZERO;
    let edge = |down: bool| if down { "down" } else { "up" };
    for op in &ops {
        match *op {
            Op::Key { sym, down, .. } => {
                lines.push((format!("{} ms", t.as_millis()), format!("{} {}", keys::key_label(sym), edge(down))))
            }
            Op::Button { button, down } => {
                lines.push((format!("{} ms", t.as_millis()), format!("{} mouse {}", button.label(), edge(down))))
            }
            Op::Wait(d) => t += d,
        }
    }
    let last_delay = seq.steps.last().and_then(|s| s.delay_ms.filter(|_| s.action.uses_post_delay()));
    let loop_ms = last_delay.unwrap_or(timing.loop_ms);
    if looping {
        lines.push((format!("{} ms", (t + ms(loop_ms)).as_millis()), format!("repeat (after {loop_ms} ms loop delay)")));
    } else {
        lines.push((format!("{} ms", t.as_millis()), "end".to_string()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::KeyLoc;

    fn seq(steps: Vec<Step>) -> Sequence {
        Sequence { steps }
    }

    /// keycode = keysym & 0xff, never needs shift
    struct FakeResolver;
    impl KeyResolver for FakeResolver {
        fn resolve(&self, sym: Keysym) -> Option<KeyLoc> {
            (sym != 0xdead).then_some(KeyLoc { keycode: (sym & 0xff) as u8, needs_shift: sym == 0x21 })
        }
    }

    fn k(text: &str) -> KeyCombo {
        KeyCombo::parse(text).unwrap()
    }

    #[test]
    fn default_sequence_order() {
        let s = Sequence::default_sequence();
        assert_eq!(s.steps.len(), 12);
        assert_eq!(s.preview(), "5  ›  Tab  ›  6  ›  Tab  ›  7  ›  Tab  ›  8  ›  Tab  ›  9  ›  Tab  ›  0  ›  Tab");
    }

    #[test]
    fn wrapped_preview_keeps_separators_at_line_ends() {
        let s = Sequence::default_sequence();
        let text = s.preview_wrapped(36, 3);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2 && lines.len() <= 3, "{text}");
        for (i, l) in lines.iter().enumerate() {
            assert!(!l.trim_start().starts_with('›'), "{text}");
            assert!(l.chars().count() <= 36, "{l}");
            if i + 1 < lines.len() {
                assert!(l.ends_with('›'), "{text}");
            }
        }
        assert!(lines.last().unwrap().ends_with("Tab"));
        let long = Sequence { steps: vec![Step::wait(10); 40] };
        let t = long.preview_wrapped(36, 2);
        assert_eq!(t.lines().count(), 2);
        assert!(t.contains("(+"), "{t}");
    }

    #[test]
    fn default_sequence_compiles_to_down_up_pairs() {
        let t = Timing::default();
        let plan = compile(&Sequence::default_sequence(), &t, true, &ReservedKeys::default(), &FakeResolver).unwrap();
        let keys: Vec<(Keysym, bool)> =
            plan.ops.iter().filter_map(|o| if let Op::Key { sym, down, .. } = o { Some((*sym, *down)) } else { None }).collect();
        let expected: Vec<(Keysym, bool)> = ['5', '6', '7', '8', '9', '0']
            .into_iter()
            .flat_map(|c| {
                let c = c as Keysym;
                [(c, true), (c, false), (keys::XK_TAB, true), (keys::XK_TAB, false)]
            })
            .collect();
        assert_eq!(keys, expected);
        // 12 holds of 25 ms + 11 gaps of 15 ms; the 12th gap is the 40 ms loop delay
        let waits: Vec<Duration> =
            plan.ops.iter().filter_map(|o| if let Op::Wait(d) = o { Some(*d) } else { None }).collect();
        assert_eq!(waits.len(), 23);
        assert_eq!(waits.iter().sum::<Duration>(), Duration::from_millis(12 * 25 + 11 * 15));
        assert_eq!(plan.loop_wait, Duration::from_millis(40));
    }

    #[test]
    fn combo_presses_modifiers_first_and_releases_in_reverse() {
        let t = Timing { modifier_ms: 5, ..Timing::default() };
        let plan = compile(&seq(vec![Step::press(k("Ctrl+Shift+E"))]), &t, false, &ReservedKeys::default(), &FakeResolver).unwrap();
        let order: Vec<String> = plan
            .ops
            .iter()
            .map(|o| match o {
                Op::Key { sym, down, .. } => format!("{}{}", if *down { "+" } else { "-" }, keys::key_label(*sym)),
                Op::Wait(d) => format!("w{}", d.as_millis()),
                Op::Button { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(order, ["+Ctrl", "+Shift", "w5", "+E", "w25", "-E", "-Shift", "-Ctrl"]);
    }

    #[test]
    fn shifted_symbol_gets_shift_automatically() {
        let plan = compile(&seq(vec![Step::press(k("!"))]), &Timing::default(), false, &ReservedKeys::default(), &FakeResolver).unwrap();
        assert!(matches!(plan.ops[0], Op::Key { sym: keys::XK_SHIFT_L, down: true, .. }));
    }

    #[test]
    fn per_step_overrides_apply() {
        let mut a = Step::press(k("F"));
        a.hold_ms = Some(70);
        a.delay_ms = Some(100);
        let plan =
            compile(&seq(vec![a, Step::press(k("G"))]), &Timing::default(), false, &ReservedKeys::default(), &FakeResolver).unwrap();
        let waits: Vec<u128> =
            plan.ops.iter().filter_map(|o| if let Op::Wait(d) = o { Some(d.as_millis()) } else { None }).collect();
        assert_eq!(waits, [70, 100, 25]);
    }

    #[test]
    fn example_sequence_from_spec_is_valid() {
        let s = seq(vec![
            Step::press(k("F")),
            Step::wait(50),
            Step::press(k("Shift+E")),
            Step::wait(100),
            Step::key_down(k("R")),
            Step::wait(250),
            Step::key_up(k("R")),
            Step::press(k("Tab")),
        ]);
        assert!(validate(&s, &Timing::default(), true, &ReservedKeys::default()).is_empty());
    }

    #[test]
    fn detects_unbalanced_keys() {
        let issues = validate(&seq(vec![Step::key_down(k("R"))]), &Timing::default(), false, &ReservedKeys::default());
        assert!(issues[0].message.contains("never released"), "{issues:?}");
        let issues = validate(&seq(vec![Step::key_up(k("R"))]), &Timing::default(), false, &ReservedKeys::default());
        assert!(issues[0].message.contains("no earlier step"), "{issues:?}");
        let issues = validate(
            &seq(vec![Step::key_down(k("R")), Step::press(k("R")), Step::key_up(k("R"))]),
            &Timing::default(),
            false,
            &ReservedKeys::default(),
        );
        assert!(issues[0].message.contains("already held"), "{issues:?}");
        let issues = validate(
            &seq(vec![Step::mouse(ActionKind::MouseDown, MouseButton::Left)]),
            &Timing::default(),
            false,
            &ReservedKeys::default(),
        );
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn held_modifier_then_plain_press_is_fine() {
        let s = seq(vec![Step::key_down(k("Shift")), Step::press(k("E")), Step::key_up(k("Shift"))]);
        assert!(validate(&s, &Timing::default(), false, &ReservedKeys::default()).is_empty());
        let s = seq(vec![Step::key_down(k("Shift")), Step::press(k("Shift+E")), Step::key_up(k("Shift"))]);
        assert!(!validate(&s, &Timing::default(), false, &ReservedKeys::default()).is_empty());
    }

    #[test]
    fn detects_hotkey_conflicts() {
        let reserved = ReservedKeys { start_stop: Some(k("F6")), emergency: Some(k("F9")) };
        let issues = validate(&seq(vec![Step::press(k("F6"))]), &Timing::default(), false, &reserved);
        assert!(issues[0].message.contains("start/stop"));
        let issues = validate(&seq(vec![Step::press(k("Ctrl+F9"))]), &Timing::default(), false, &reserved);
        assert!(issues[0].message.contains("emergency"));
    }

    #[test]
    fn rejects_missing_fields_and_bad_durations() {
        let s = seq(vec![
            Step { action: ActionKind::Press, ..Step::default() },
            Step { action: ActionKind::Wait, ..Step::default() },
            Step { action: ActionKind::Hold, key: Some(k("A")), duration_ms: Some(0), ..Step::default() },
            Step { action: ActionKind::MouseHold, button: Some(MouseButton::Left), duration_ms: Some(MAX_DURATION_MS + 1), ..Step::default() },
        ]);
        let issues = validate(&s, &Timing::default(), false, &ReservedKeys::default());
        assert_eq!(issues.len(), 4, "{issues:?}");
        assert!(validate(&Sequence::default(), &Timing::default(), false, &ReservedKeys::default())[0].message.contains("no steps"));
    }

    #[test]
    fn rejects_zero_length_loops() {
        let t = Timing { key_ms: 1, gap_ms: 0, loop_ms: 0, modifier_ms: 0 };
        let issues = validate(&seq(vec![Step::press(k("A"))]), &t, true, &ReservedKeys::default());
        assert!(issues[0].message.contains("per cycle"));
        assert!(validate(&seq(vec![Step::press(k("A"))]), &t, false, &ReservedKeys::default()).is_empty());
    }

    #[test]
    fn unmapped_key_is_reported() {
        let s = seq(vec![Step::press(KeyCombo::new(0xdead))]);
        let err = compile(&s, &Timing::default(), false, &ReservedKeys::default(), &FakeResolver).unwrap_err();
        assert!(matches!(err, CompileError::UnmappedKey { step: 0, .. }));
    }

    #[test]
    fn preview_lists_cumulative_times() {
        let lines = preview_lines(&seq(vec![Step::press(k("A")), Step::press(k("B"))]), &Timing::default(), true);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], ("0 ms".to_string(), "A down".to_string()));
        assert_eq!(lines[1], ("25 ms".to_string(), "A up".to_string()));
        assert_eq!(lines[2], ("40 ms".to_string(), "B down".to_string()));
        assert_eq!(lines[4].0, "105 ms");
        assert!(lines[4].1.starts_with("repeat"));
    }

    #[test]
    fn presets_detect() {
        assert_eq!(Preset::detect(&Preset::Fast.timing()), Preset::Fast);
        assert_eq!(Preset::detect(&Timing::default()), Preset::Balanced);
        assert_eq!(Preset::detect(&Timing { key_ms: 33, ..Timing::default() }), Preset::Custom);
    }

    #[test]
    fn toml_round_trip_and_normalization() {
        let mut s = Step::press(k("Shift+E"));
        s.duration_ms = Some(5); // not applicable to Press
        let s = Sequence { steps: vec![s.normalized(), Step::wait(50), Step::mouse(ActionKind::MouseHold, MouseButton::Right)] };
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("key = \"Shift+E\""), "{text}");
        assert!(!text.contains("duration_ms = 5\n"), "{text}");
        let back: Sequence = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }
}
