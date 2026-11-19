//! Hotkey matching: converts `HotkeyBinding` strings to crossterm `KeyEvent` matches.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_config::HotkeyBinding;

/// Parse a `HotkeyBinding` string (e.g. "ctrl+t", "alt+g", "f1") into
/// `(KeyCode, KeyModifiers)`, or `None` if the binding is unbound.
pub(crate) fn parse_binding(binding: &HotkeyBinding) -> Option<(KeyCode, KeyModifiers)> {
    if binding.is_none() {
        return None;
    }

    let s = binding.as_str();
    let parts: Vec<&str> = s.split('+').collect();

    let mut modifiers = KeyModifiers::NONE;
    let key_part = match parts.len() {
        1 => parts[0],
        2 => {
            match parts[0] {
                "ctrl" => modifiers |= KeyModifiers::CONTROL,
                "alt" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                _ => return None,
            }
            parts[1]
        }
        3 => {
            for &modifier in &parts[..2] {
                match modifier {
                    "ctrl" => modifiers |= KeyModifiers::CONTROL,
                    "alt" => modifiers |= KeyModifiers::ALT,
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    _ => return None,
                }
            }
            parts[2]
        }
        _ => return None,
    };

    let key_code = parse_key_code(key_part)?;
    Some((key_code, modifiers))
}

/// Parse a key name string into a `KeyCode`.
fn parse_key_code(s: &str) -> Option<KeyCode> {
    match s {
        "enter" => Some(KeyCode::Enter),
        "esc" => Some(KeyCode::Esc),
        "backspace" => Some(KeyCode::Backspace),
        "tab" => Some(KeyCode::Tab),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" => Some(KeyCode::PageUp),
        "pagedown" => Some(KeyCode::PageDown),
        "delete" => Some(KeyCode::Delete),
        "insert" => Some(KeyCode::Insert),
        "space" => Some(KeyCode::Char(' ')),
        "f1" => Some(KeyCode::F(1)),
        "f2" => Some(KeyCode::F(2)),
        "f3" => Some(KeyCode::F(3)),
        "f4" => Some(KeyCode::F(4)),
        "f5" => Some(KeyCode::F(5)),
        "f6" => Some(KeyCode::F(6)),
        "f7" => Some(KeyCode::F(7)),
        "f8" => Some(KeyCode::F(8)),
        "f9" => Some(KeyCode::F(9)),
        "f10" => Some(KeyCode::F(10)),
        "f11" => Some(KeyCode::F(11)),
        "f12" => Some(KeyCode::F(12)),
        s if s.len() == 1 => s.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

/// Check if a `KeyEvent` matches a `HotkeyBinding`.
pub(crate) fn matches_binding(binding: &HotkeyBinding, event: &KeyEvent) -> bool {
    if event.kind != KeyEventKind::Press && event.kind != KeyEventKind::Repeat {
        return false;
    }

    let Some((key_code, modifiers)) = parse_binding(binding) else {
        return false;
    };

    if is_nul_for_ctrl_space(event) {
        return key_code == KeyCode::Char(' ') && modifiers == KeyModifiers::CONTROL;
    }

    // A terminal that reports the shifted key (kitty's alternate keys) gives `N` for ctrl+shift+n.
    let same_key = match (event.code, key_code) {
        (KeyCode::Char(typed), KeyCode::Char(bound)) => {
            typed.to_lowercase().eq(bound.to_lowercase())
        }
        (typed, bound) => typed == bound,
    };
    same_key && modifiers_match(event, modifiers)
}

/// Check if a `KeyEvent` matches the Redraw binding, accepting `ctrl+shift+l` as a fallback
/// when `Redraw` is at its default (`ctrl+l`) or unbound (`none`).
pub(crate) fn matches_redraw_binding(binding: &HotkeyBinding, event: &KeyEvent) -> bool {
    if matches_binding(binding, event) {
        return true;
    }
    (binding.is_none() || binding.as_str() == "ctrl+l")
        && matches_binding(&HotkeyBinding::from_str("ctrl+shift+l"), event)
}

/// Ctrl+Space is not a character a terminal can type: it sends NUL, which terminals and crossterm
/// report as space with control, as a bare `Null`, or as `@` with control (NUL is `^@`).
fn is_nul_for_ctrl_space(event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::Null => true,
        KeyCode::Char('@') => event.modifiers == KeyModifiers::CONTROL,
        _ => false,
    }
}

/// Whether the event's modifiers are the binding's. Some terminals report ctrl+shift+n as an
/// uppercase `N` with only ctrl set, dropping the shift they just folded into the letter, so a
/// binding that needs shift also accepts an uppercase letter without it.
fn modifiers_match(event: &KeyEvent, wanted: KeyModifiers) -> bool {
    if event.modifiers == wanted {
        return true;
    }
    matches!(event.code, KeyCode::Char(typed) if typed.is_uppercase())
        && wanted.contains(KeyModifiers::SHIFT)
        && event.modifiers | KeyModifiers::SHIFT == wanted
}

/// Convert a `KeyEvent` to a `HotkeyBinding` string for persistence.
/// Only handles Press events with Ctrl/Alt/Shift modifiers + a key.
pub(crate) fn key_event_to_binding(event: &KeyEvent) -> Option<HotkeyBinding> {
    if event.kind != KeyEventKind::Press {
        return None;
    }

    let key_name = match event.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Null => return Some(HotkeyBinding::from_str("ctrl+space")),
        KeyCode::Char(c) => c.to_lowercase().to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    };

    let mut parts = Vec::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    parts.push(&key_name);

    Some(HotkeyBinding::from_str(&parts.join("+")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn parse_ctrl_t() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        let parsed = parse_binding(&binding);
        assert_eq!(parsed, Some((KeyCode::Char('t'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn parse_alt_g() {
        let binding = HotkeyBinding::from_str("alt+g");
        let parsed = parse_binding(&binding);
        assert_eq!(parsed, Some((KeyCode::Char('g'), KeyModifiers::ALT)));
    }

    #[test]
    fn parse_shift_f1() {
        let binding = HotkeyBinding::from_str("shift+f1");
        let parsed = parse_binding(&binding);
        assert_eq!(parsed, Some((KeyCode::F(1), KeyModifiers::SHIFT)));
    }

    #[test]
    fn parse_ctrl_shift_a() {
        let binding = HotkeyBinding::from_str("ctrl+shift+a");
        let parsed = parse_binding(&binding);
        assert_eq!(
            parsed,
            Some((
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
        );
    }

    #[test]
    fn parse_plain_enter() {
        let binding = HotkeyBinding::from_str("enter");
        let parsed = parse_binding(&binding);
        assert_eq!(parsed, Some((KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn parse_f12() {
        let binding = HotkeyBinding::from_str("f12");
        let parsed = parse_binding(&binding);
        assert_eq!(parsed, Some((KeyCode::F(12), KeyModifiers::NONE)));
    }

    #[test]
    fn parse_none_returns_none() {
        let binding = HotkeyBinding::none();
        assert_eq!(parse_binding(&binding), None);
    }

    #[test]
    fn parse_invalid_modifier_returns_none() {
        let binding = HotkeyBinding::from_str("super+t");
        assert_eq!(parse_binding(&binding), None);
    }

    #[test]
    fn matches_ctrl_t_event() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        let event = press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(matches_binding(&binding, &event));
    }

    #[test]
    fn does_not_match_wrong_key() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        let event = press(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(!matches_binding(&binding, &event));
    }

    #[test]
    fn does_not_match_wrong_modifier() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        let event = press(KeyCode::Char('t'), KeyModifiers::ALT);
        assert!(!matches_binding(&binding, &event));
    }

    #[test]
    fn unbound_matches_nothing() {
        let binding = HotkeyBinding::none();
        let event = press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!matches_binding(&binding, &event));
    }

    #[test]
    fn does_not_match_release_events() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        let mut event = press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        event.kind = KeyEventKind::Release;
        assert!(!matches_binding(&binding, &event));
    }

    #[test]
    fn key_event_to_binding_ctrl_t() {
        let event = press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let binding = key_event_to_binding(&event).unwrap();
        assert_eq!(binding, HotkeyBinding::from_str("ctrl+t"));
    }

    #[test]
    fn key_event_to_binding_alt_g() {
        let event = press(KeyCode::Char('g'), KeyModifiers::ALT);
        let binding = key_event_to_binding(&event).unwrap();
        assert_eq!(binding, HotkeyBinding::from_str("alt+g"));
    }

    #[test]
    fn key_event_to_binding_f5() {
        let event = press(KeyCode::F(5), KeyModifiers::NONE);
        let binding = key_event_to_binding(&event).unwrap();
        assert_eq!(binding, HotkeyBinding::from_str("f5"));
    }

    #[test]
    fn key_event_to_binding_ctrl_shift_a() {
        let event = press(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let binding = key_event_to_binding(&event).unwrap();
        assert_eq!(binding, HotkeyBinding::from_str("ctrl+shift+a"));
    }

    #[test]
    fn key_event_to_binding_release_returns_none() {
        let mut event = press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        event.kind = KeyEventKind::Release;
        assert!(key_event_to_binding(&event).is_none());
    }

    #[test]
    fn ctrl_space_matches_however_the_terminal_reports_nul() {
        let binding = HotkeyBinding::from_str("ctrl+space");
        for event in [
            press(KeyCode::Char(' '), KeyModifiers::CONTROL),
            press(KeyCode::Null, KeyModifiers::NONE),
            press(KeyCode::Null, KeyModifiers::CONTROL),
            press(KeyCode::Char('@'), KeyModifiers::CONTROL),
        ] {
            assert!(matches_binding(&binding, &event), "{event:?}");
        }
        assert!(!matches_binding(
            &binding,
            &press(KeyCode::Char(' '), KeyModifiers::NONE)
        ));
        assert!(!matches_binding(
            &binding,
            &press(KeyCode::Char('@'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn nul_does_not_match_other_bindings() {
        let binding = HotkeyBinding::from_str("ctrl+t");
        assert!(!matches_binding(
            &binding,
            &press(KeyCode::Null, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn space_bindings_round_trip() {
        let event = press(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let binding = key_event_to_binding(&event).unwrap();
        assert_eq!(binding, HotkeyBinding::from_str("ctrl+space"));
        assert_eq!(
            key_event_to_binding(&press(KeyCode::Null, KeyModifiers::NONE)),
            Some(HotkeyBinding::from_str("ctrl+space"))
        );
    }

    #[test]
    fn roundtrip_parse_and_match() {
        // Parse a binding, create a matching KeyEvent, and verify it matches
        let binding = HotkeyBinding::from_str("ctrl+g");
        let (code, mods) = parse_binding(&binding).unwrap();
        let event = press(code, mods);
        assert!(matches_binding(&binding, &event));

        // Also verify the reverse: key_event_to_binding produces the same binding
        let roundtripped = key_event_to_binding(&event).unwrap();
        assert_eq!(roundtripped, binding);
    }

    #[test]
    fn a_binding_with_shift_accepts_an_uppercase_letter_that_lost_its_shift_flag() {
        let binding = HotkeyBinding::from_str("ctrl+shift+n");
        let press = |code, modifiers| {
            let mut event = KeyEvent::new(code, modifiers);
            event.kind = KeyEventKind::Press;
            event
        };
        assert!(matches_binding(
            &binding,
            &press(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(matches_binding(
            &binding,
            &press(
                KeyCode::Char('N'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(
            matches_binding(&binding, &press(KeyCode::Char('N'), KeyModifiers::CONTROL)),
            "uppercase with ctrl only is ctrl+shift"
        );
        assert!(
            !matches_binding(&binding, &press(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            "plain ctrl+n stays ctrl+n"
        );
        let plain = HotkeyBinding::from_str("ctrl+n");
        assert!(matches_binding(
            &plain,
            &press(KeyCode::Char('n'), KeyModifiers::CONTROL)
        ));
        assert!(
            !matches_binding(
                &plain,
                &press(
                    KeyCode::Char('N'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )
            ),
            "ctrl+n must not fire for ctrl+shift+n"
        );
    }
}
