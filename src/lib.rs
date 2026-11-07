//! Mentos Macro: keyboard sequence automation for Linux X11.
//!
//! Module overview:
//! * [`keys`]: keysyms, modifiers and key-combination parsing
//! * [`input`]: keymap resolution and the XTest / uinput backends
//! * [`timing`]: deadline-based scheduler
//! * [`sequence`]: sequence model, validation and compilation into an immutable plan
//! * [`sequence_editor`]: UI-independent editor model
//! * [`engine`]: the execution worker and its state machine
//! * [`hotkeys`]: global hotkeys, key capture and window detection
//! * [`safety`]: focus tracking, held-input bookkeeping, instance lock, signals
//! * [`config`] and [`profiles`]: persistent settings and sequence profiles
//! * [`app`]: the Slint UI controller

pub mod app;
pub mod config;
pub mod engine;
pub mod hotkeys;
pub mod input;
pub mod keys;
pub mod profiles;
pub mod safety;
pub mod sequence;
pub mod sequence_editor;
pub mod timing;

/// Generated Slint UI types.
pub mod ui {
    slint::include_modules!();
}
