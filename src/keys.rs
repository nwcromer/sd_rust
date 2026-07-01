//! Keyboard-macro playback via a uinput virtual keyboard (`evdev`).
//!
//! A macro is an ordered sequence of **chords** (modifier+key, e.g.
//! `ctrl+shift+t`) and **literal text** (e.g. `chicken`). We create one virtual
//! keyboard at startup and replay events into it.
//!
//! SECURITY: a uinput virtual keyboard can inject keystrokes into any focused
//! application. That power is inherent to the feature; the trust boundary is the
//! config file, which defines exactly what gets typed. The `/dev/uinput` access
//! grant is scoped by the udev rule (see `udev/70-sd_rust.rules`) and documented
//! in the README. We map characters with a fixed **US QWERTY** layout — the
//! kernel/compositor maps our keycodes through the *active* layout, so on a
//! non-US layout some symbols may differ.
//!
//! Char→keycode uses the US layout; the active keyboard layout is applied by the
//! compositor downstream.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, InputEvent, KeyCode, KeyEvent};

use crate::config::MacroStep;

/// Delay between a key press and release, and between steps. Long enough for
/// applications to register discrete events, short enough to feel instant.
const KEY_DELAY: Duration = Duration::from_millis(8);

pub struct MacroKeyboard {
    device: VirtualDevice,
}

impl MacroKeyboard {
    /// Create the virtual keyboard. Fails if `/dev/uinput` isn't accessible
    /// (run `--create-udev-rules` and ensure an active local session).
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in supported_keys() {
            keys.insert(key);
        }
        let device = VirtualDevice::builder()
            .context(
                "failed to open /dev/uinput — is it accessible? \
                 (install the udev rules: sudo sd_rust --create-udev-rules)",
            )?
            .name("sd_rust virtual keyboard")
            .with_keys(&keys)
            .context("failed to register keys on the virtual keyboard")?
            .build()
            .context("failed to build the uinput virtual keyboard")?;
        Ok(Self { device })
    }

    /// Play a macro: each step is a chord or a run of literal text.
    pub fn play(&mut self, steps: &[MacroStep]) -> Result<()> {
        for step in steps {
            match step {
                MacroStep::Chord { chord } => self.play_chord(chord)?,
                MacroStep::Text { text } => self.type_text(text)?,
            }
            sleep(KEY_DELAY);
        }
        Ok(())
    }

    /// Press a chord: hold all modifiers + the main key, then release in
    /// reverse. `emit()` auto-appends a SYN_REPORT, so one call = one atomic
    /// report.
    fn play_chord(&mut self, chord: &str) -> Result<()> {
        let (mods, key) = parse_chord(chord)?;

        let mut down: Vec<InputEvent> = mods.iter().map(|m| key_event(*m, 1)).collect();
        down.push(key_event(key, 1));
        self.device.emit(&down).context("failed to emit chord press")?;
        sleep(KEY_DELAY);

        let mut up: Vec<InputEvent> = vec![key_event(key, 0)];
        up.extend(mods.iter().rev().map(|m| key_event(*m, 0)));
        self.device.emit(&up).context("failed to emit chord release")?;
        Ok(())
    }

    /// Type literal text, one character at a time (with shift where needed).
    fn type_text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let (key, shift) = key_for_char(c)
                .with_context(|| format!("character {c:?} has no US-layout key mapping"))?;
            let mut down = Vec::new();
            if shift {
                down.push(key_event(KeyCode::KEY_LEFTSHIFT, 1));
            }
            down.push(key_event(key, 1));
            self.device.emit(&down).context("failed to emit key press")?;
            sleep(KEY_DELAY);

            let mut up = vec![key_event(key, 0)];
            if shift {
                up.push(key_event(KeyCode::KEY_LEFTSHIFT, 0));
            }
            self.device.emit(&up).context("failed to emit key release")?;
            sleep(KEY_DELAY);
        }
        Ok(())
    }
}

fn key_event(code: KeyCode, value: i32) -> InputEvent {
    *KeyEvent::new(code, value)
}

/// Parse a chord like `ctrl+shift+t` or `alt+F4` into (modifiers, main key).
/// Modifier aliases: ctrl/control, shift, alt/option, super/meta/win/cmd.
fn parse_chord(chord: &str) -> Result<(Vec<KeyCode>, KeyCode)> {
    let parts: Vec<&str> = chord.split('+').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        bail!("empty chord");
    }
    let (key_name, mod_names) = parts.split_last().unwrap();

    let mut mods = Vec::new();
    for m in mod_names {
        let code = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyCode::KEY_LEFTCTRL,
            "shift" => KeyCode::KEY_LEFTSHIFT,
            "alt" | "option" => KeyCode::KEY_LEFTALT,
            "super" | "meta" | "win" | "cmd" => KeyCode::KEY_LEFTMETA,
            other => bail!("unknown modifier {other:?} in chord {chord:?}"),
        };
        mods.push(code);
    }

    let key = key_by_name(key_name)
        .with_context(|| format!("unknown key {key_name:?} in chord {chord:?}"))?;
    Ok((mods, key))
}

/// Resolve a key name (named key or single character) to a keycode. Used for a
/// chord's main key, where shift is expressed as an explicit modifier.
fn key_by_name(name: &str) -> Option<KeyCode> {
    // Single character: map via the char table (ignoring its shift flag — the
    // user writes shift explicitly in a chord).
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next())
        && let Some((key, _shift)) = key_for_char(c)
    {
        return Some(key);
    }

    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::KEY_ENTER,
        "tab" => KeyCode::KEY_TAB,
        "space" => KeyCode::KEY_SPACE,
        "esc" | "escape" => KeyCode::KEY_ESC,
        "backspace" => KeyCode::KEY_BACKSPACE,
        "delete" | "del" => KeyCode::KEY_DELETE,
        "insert" | "ins" => KeyCode::KEY_INSERT,
        "home" => KeyCode::KEY_HOME,
        "end" => KeyCode::KEY_END,
        "pageup" | "pgup" => KeyCode::KEY_PAGEUP,
        "pagedown" | "pgdn" => KeyCode::KEY_PAGEDOWN,
        "up" => KeyCode::KEY_UP,
        "down" => KeyCode::KEY_DOWN,
        "left" => KeyCode::KEY_LEFT,
        "right" => KeyCode::KEY_RIGHT,
        "f1" => KeyCode::KEY_F1,
        "f2" => KeyCode::KEY_F2,
        "f3" => KeyCode::KEY_F3,
        "f4" => KeyCode::KEY_F4,
        "f5" => KeyCode::KEY_F5,
        "f6" => KeyCode::KEY_F6,
        "f7" => KeyCode::KEY_F7,
        "f8" => KeyCode::KEY_F8,
        "f9" => KeyCode::KEY_F9,
        "f10" => KeyCode::KEY_F10,
        "f11" => KeyCode::KEY_F11,
        "f12" => KeyCode::KEY_F12,
        _ => return None,
    })
}

/// US-QWERTY character → (keycode, needs-shift). `KeyCode` is a struct of
/// associated consts (not an enum), so we alias it to `K` and qualify each.
fn key_for_char(c: char) -> Option<(KeyCode, bool)> {
    use evdev::KeyCode as K;
    let mapping = match c {
        'a'..='z' => return Some((letter_key(c), false)),
        'A'..='Z' => return Some((letter_key(c.to_ascii_lowercase()), true)),
        '1' => (K::KEY_1, false),
        '2' => (K::KEY_2, false),
        '3' => (K::KEY_3, false),
        '4' => (K::KEY_4, false),
        '5' => (K::KEY_5, false),
        '6' => (K::KEY_6, false),
        '7' => (K::KEY_7, false),
        '8' => (K::KEY_8, false),
        '9' => (K::KEY_9, false),
        '0' => (K::KEY_0, false),
        '!' => (K::KEY_1, true),
        '@' => (K::KEY_2, true),
        '#' => (K::KEY_3, true),
        '$' => (K::KEY_4, true),
        '%' => (K::KEY_5, true),
        '^' => (K::KEY_6, true),
        '&' => (K::KEY_7, true),
        '*' => (K::KEY_8, true),
        '(' => (K::KEY_9, true),
        ')' => (K::KEY_0, true),
        ' ' => (K::KEY_SPACE, false),
        '\t' => (K::KEY_TAB, false),
        '\n' => (K::KEY_ENTER, false),
        '-' => (K::KEY_MINUS, false),
        '_' => (K::KEY_MINUS, true),
        '=' => (K::KEY_EQUAL, false),
        '+' => (K::KEY_EQUAL, true),
        '[' => (K::KEY_LEFTBRACE, false),
        '{' => (K::KEY_LEFTBRACE, true),
        ']' => (K::KEY_RIGHTBRACE, false),
        '}' => (K::KEY_RIGHTBRACE, true),
        '\\' => (K::KEY_BACKSLASH, false),
        '|' => (K::KEY_BACKSLASH, true),
        ';' => (K::KEY_SEMICOLON, false),
        ':' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_APOSTROPHE, false),
        '"' => (K::KEY_APOSTROPHE, true),
        '`' => (K::KEY_GRAVE, false),
        '~' => (K::KEY_GRAVE, true),
        ',' => (K::KEY_COMMA, false),
        '<' => (K::KEY_COMMA, true),
        '.' => (K::KEY_DOT, false),
        '>' => (K::KEY_DOT, true),
        '/' => (K::KEY_SLASH, false),
        '?' => (K::KEY_SLASH, true),
        _ => return None,
    };
    Some(mapping)
}

/// Map a lowercase ASCII letter to its keycode (`a` → `KEY_A`, contiguous).
fn letter_key(c: char) -> KeyCode {
    use evdev::KeyCode as K;
    const LETTERS: [KeyCode; 26] = [
        K::KEY_A, K::KEY_B, K::KEY_C, K::KEY_D, K::KEY_E, K::KEY_F, K::KEY_G, K::KEY_H, K::KEY_I,
        K::KEY_J, K::KEY_K, K::KEY_L, K::KEY_M, K::KEY_N, K::KEY_O, K::KEY_P, K::KEY_Q, K::KEY_R,
        K::KEY_S, K::KEY_T, K::KEY_U, K::KEY_V, K::KEY_W, K::KEY_X, K::KEY_Y, K::KEY_Z,
    ];
    LETTERS[(c as u8 - b'a') as usize]
}

/// Every keycode the virtual device must declare so it can emit them: all
/// chars from the char table + modifiers + named keys.
fn supported_keys() -> Vec<KeyCode> {
    use evdev::KeyCode as K;
    let mut keys = vec![K::KEY_LEFTCTRL, K::KEY_LEFTSHIFT, K::KEY_LEFTALT, K::KEY_LEFTMETA];
    // Printable ASCII covers every entry in `key_for_char`.
    for byte in 0x20u8..0x7f {
        if let Some((key, _)) = key_for_char(byte as char) {
            keys.push(key);
        }
    }
    // Named keys reachable via `key_by_name`.
    keys.extend([
        K::KEY_ENTER, K::KEY_TAB, K::KEY_SPACE, K::KEY_ESC, K::KEY_BACKSPACE, K::KEY_DELETE,
        K::KEY_INSERT, K::KEY_HOME, K::KEY_END, K::KEY_PAGEUP, K::KEY_PAGEDOWN, K::KEY_UP,
        K::KEY_DOWN, K::KEY_LEFT, K::KEY_RIGHT, K::KEY_F1, K::KEY_F2, K::KEY_F3, K::KEY_F4,
        K::KEY_F5, K::KEY_F6, K::KEY_F7, K::KEY_F8, K::KEY_F9, K::KEY_F10, K::KEY_F11, K::KEY_F12,
    ]);
    keys.sort_unstable_by_key(|k| k.0);
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_chords() {
        let (mods, key) = parse_chord("ctrl+shift+t").unwrap();
        assert_eq!(mods, vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT]);
        assert_eq!(key, KeyCode::KEY_T);

        let (mods, key) = parse_chord("alt+F4").unwrap();
        assert_eq!(mods, vec![KeyCode::KEY_LEFTALT]);
        assert_eq!(key, KeyCode::KEY_F4);

        let (mods, key) = parse_chord("super+l").unwrap();
        assert_eq!(mods, vec![KeyCode::KEY_LEFTMETA]);
        assert_eq!(key, KeyCode::KEY_L);
    }

    #[test]
    fn rejects_unknown_modifier_and_key() {
        assert!(parse_chord("hyper+x").is_err());
        assert!(parse_chord("ctrl+nope").is_err());
        assert!(parse_chord("").is_err());
    }

    #[test]
    fn char_mapping_shift_flags() {
        assert_eq!(key_for_char('a'), Some((KeyCode::KEY_A, false)));
        assert_eq!(key_for_char('A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(key_for_char('!'), Some((KeyCode::KEY_1, true)));
        assert_eq!(key_for_char(':'), Some((KeyCode::KEY_SEMICOLON, true)));
        assert_eq!(key_for_char(' '), Some((KeyCode::KEY_SPACE, false)));
        assert!(key_for_char('€').is_none());
    }

    #[test]
    fn every_supported_key_is_unique() {
        let keys = supported_keys();
        let mut sorted = keys.clone();
        sorted.sort_unstable_by_key(|k| k.0);
        sorted.dedup();
        assert_eq!(keys.len(), sorted.len(), "supported_keys must be deduped");
    }
}
