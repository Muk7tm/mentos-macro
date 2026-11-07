//! UI-independent model of the Advanced Sequence Editor.
//!
//! The editor works on its own copy of the steps. The application stores the result as the
//! custom sequence and hands the engine a fresh immutable snapshot; a running macro is
//! never affected by edits.

use crate::keys::{KeyCombo, MouseButton};
use crate::sequence::{ActionKind, Sequence, Step, Timing, MAX_DURATION_MS, MAX_STEPS};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceEditor {
    steps: Vec<Step>,
    selected: Option<usize>,
}

impl SequenceEditor {
    pub fn new(seq: &Sequence) -> Self {
        let selected = (!seq.steps.is_empty()).then_some(0);
        SequenceEditor { steps: seq.steps.clone(), selected }
    }

    pub fn sequence(&self) -> Sequence {
        Sequence { steps: self.steps.clone() }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_step(&self) -> Option<&Step> {
        self.selected.and_then(|i| self.steps.get(i))
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index.filter(|i| *i < self.steps.len());
    }

    /// Insert a new step after the selection (or at the end) and select it.
    pub fn add(&mut self, kind: ActionKind) -> Result<usize, String> {
        if self.steps.len() >= MAX_STEPS {
            return Err(format!("A sequence can have at most {MAX_STEPS} steps"));
        }
        let at = self.selected.map(|i| i + 1).unwrap_or(self.steps.len());
        self.steps.insert(at, Step::new_of_kind(kind));
        self.selected = Some(at);
        Ok(at)
    }

    pub fn delete_selected(&mut self) {
        let Some(i) = self.selected else { return };
        if i < self.steps.len() {
            self.steps.remove(i);
        }
        self.selected = if self.steps.is_empty() { None } else { Some(i.min(self.steps.len() - 1)) };
    }

    pub fn duplicate_selected(&mut self) -> Result<(), String> {
        let Some(step) = self.selected_step().cloned() else { return Ok(()) };
        if self.steps.len() >= MAX_STEPS {
            return Err(format!("A sequence can have at most {MAX_STEPS} steps"));
        }
        let at = self.selected.unwrap_or(0) + 1;
        self.steps.insert(at, step);
        self.selected = Some(at);
        Ok(())
    }

    /// Move the selected step by `delta` positions (clamped).
    pub fn move_selected(&mut self, delta: i32) {
        let Some(i) = self.selected else { return };
        let len = self.steps.len() as i64;
        let target = (i as i64 + i64::from(delta)).clamp(0, len - 1) as usize;
        if target != i {
            let step = self.steps.remove(i);
            self.steps.insert(target, step);
            self.selected = Some(target);
        }
    }

    pub fn replace_selected(&mut self, step: Step) {
        if let Some(slot) = self.selected.and_then(|i| self.steps.get_mut(i)) {
            *slot = step.normalized();
        }
    }

    pub fn reset_default(&mut self) {
        *self = SequenceEditor::new(&Sequence::default_sequence());
    }
}

/// Flat representation of one step for the edit form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepForm {
    pub kind: usize,
    pub key_text: String,
    pub button: usize,
    pub duration_ms: u32,
    pub hold_override: bool,
    pub hold_ms: u32,
    pub delay_override: bool,
    pub delay_ms: u32,
}

impl StepForm {
    pub fn from_step(step: &Step, timing: &Timing) -> StepForm {
        StepForm {
            kind: step.action.index(),
            key_text: step.key.map(|k| k.to_string()).unwrap_or_default(),
            button: step.button.unwrap_or_default().index(),
            duration_ms: step.duration_ms.unwrap_or(if step.action == ActionKind::Wait { 50 } else { 250 }),
            hold_override: step.hold_ms.is_some(),
            hold_ms: step.hold_ms.unwrap_or(timing.key_ms),
            delay_override: step.delay_ms.is_some(),
            delay_ms: step.delay_ms.unwrap_or(timing.gap_ms),
        }
    }

    /// Build a step from the form. `previous` supplies values for fields that do not apply
    /// to the newly chosen action, so switching action back and forth loses nothing.
    pub fn to_step(&self, previous: Option<&Step>) -> Result<Step, String> {
        let action = ActionKind::from_index(self.kind);
        let key = if action.uses_key() {
            let text = self.key_text.trim();
            if text.is_empty() {
                previous.and_then(|p| p.key).or(Some(KeyCombo::new(0x65)))
            } else {
                Some(KeyCombo::parse(text)?)
            }
        } else {
            None
        };
        let button = action.uses_button().then(|| MouseButton::from_index(self.button));
        let duration_ms = action.uses_duration().then_some(self.duration_ms.min(MAX_DURATION_MS));
        let hold_ms = (action.uses_hold_override() && self.hold_override).then_some(self.hold_ms.clamp(1, Timing::KEY_RANGE.1));
        let delay_ms = (action.uses_post_delay() && self.delay_override).then_some(self.delay_ms.min(MAX_DURATION_MS));
        Ok(Step { action, key, button, duration_ms, hold_ms, delay_ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(e: &SequenceEditor) -> Vec<String> {
        e.steps().iter().map(Step::title).collect()
    }

    #[test]
    fn add_inserts_after_selection() {
        let mut e = SequenceEditor::new(&Sequence::default());
        assert_eq!(e.selected(), None);
        e.add(ActionKind::Wait).unwrap();
        e.add(ActionKind::Press).unwrap();
        e.select(Some(0));
        e.add(ActionKind::MouseHold).unwrap();
        assert_eq!(titles(&e), ["Wait 50 ms", "Hold Left mouse for 250 ms", "Press E"]);
        assert_eq!(e.selected(), Some(1));
    }

    #[test]
    fn move_duplicate_delete() {
        let mut e = SequenceEditor::new(&Sequence::default_sequence());
        e.select(Some(0));
        e.move_selected(1);
        assert_eq!(&titles(&e)[..2], ["Click Left mouse", "Press 5"]);
        assert_eq!(e.selected(), Some(1));
        e.move_selected(-10);
        assert_eq!(e.selected(), Some(0));
        assert_eq!(titles(&e)[0], "Press 5");
        e.duplicate_selected().unwrap();
        assert_eq!(e.steps().len(), 13);
        assert_eq!(&titles(&e)[..2], ["Press 5", "Press 5"]);
        e.select(Some(12));
        e.delete_selected();
        assert_eq!(e.selected(), Some(11));
        e.reset_default();
        assert_eq!(e.sequence(), Sequence::default_sequence());
    }

    #[test]
    fn delete_last_remaining_clears_selection() {
        let mut e = SequenceEditor::new(&Sequence { steps: vec![Step::wait(5)] });
        e.delete_selected();
        assert!(e.steps().is_empty());
        assert_eq!(e.selected(), None);
        e.delete_selected();
        e.move_selected(1);
    }

    #[test]
    fn form_round_trip() {
        let t = Timing::default();
        let mut step = Step::press(KeyCombo::parse("Ctrl+F").unwrap());
        step.delay_ms = Some(80);
        let form = StepForm::from_step(&step, &t);
        assert_eq!(form.key_text, "Ctrl+F");
        assert!(!form.hold_override && form.hold_ms == 25);
        assert!(form.delay_override && form.delay_ms == 80);
        assert_eq!(form.to_step(Some(&step)).unwrap(), step);
    }

    #[test]
    fn form_rejects_unknown_keys_and_switches_kinds() {
        let t = Timing::default();
        let step = Step::press(KeyCombo::parse("R").unwrap());
        let mut form = StepForm::from_step(&step, &t);
        form.key_text = "Nope".into();
        assert!(form.to_step(Some(&step)).is_err());
        form.key_text = "R".into();
        form.kind = ActionKind::Wait.index();
        form.duration_ms = 120;
        let w = form.to_step(Some(&step)).unwrap();
        assert_eq!(w, Step::wait(120));
        form.kind = ActionKind::MouseDown.index();
        form.button = 2;
        let m = form.to_step(Some(&w)).unwrap();
        assert_eq!(m.button, Some(MouseButton::Right));
        assert_eq!(m.duration_ms, None);
    }
}
