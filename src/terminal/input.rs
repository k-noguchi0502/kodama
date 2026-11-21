//! Translates key presses into the byte sequences terminal applications expect.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use crate::terminal::Modes;

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(mods: ModifiersState) -> u8 {
    1 + mods.shift_key() as u8 + 2 * mods.alt_key() as u8 + 4 * mods.control_key() as u8
}

/// `CSI 1;m X` / `SS3 X` style keys (arrows, Home/End, F1-F4).
fn letter_key(letter: char, mods: ModifiersState, app_mode: bool) -> Vec<u8> {
    let m = modifier_param(mods);
    if m > 1 {
        format!("\x1b[1;{m}{letter}").into_bytes()
    } else if app_mode {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    }
}

/// `CSI n;m ~` style keys (Insert/Delete/PageUp/PageDown, F5-F12).
fn tilde_key(code: u8, mods: ModifiersState) -> Vec<u8> {
    let m = modifier_param(mods);
    if m > 1 {
        format!("\x1b[{code};{m}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn with_alt(mut bytes: Vec<u8>, mods: ModifiersState) -> Vec<u8> {
    if mods.alt_key() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// Kitty keyboard protocol flags. Flag 1 (disambiguate) is what any
/// non-zero set of flags gets at least.
pub mod kitty {
    pub const EVENT_TYPES: u16 = 2;
    pub const ALTERNATE_KEYS: u16 = 4;
    pub const ALL_KEYS: u16 = 8;
    pub const TEXT: u16 = 16;
}

/// Kitty's number and final byte for a non-text key.
fn functional(named: NamedKey, location: KeyLocation) -> Option<(u32, char)> {
    let right = location == KeyLocation::Right;
    let side = |left: u32| if right { left + 6 } else { left };
    Some(match named {
        NamedKey::Escape => (27, 'u'),
        NamedKey::Enter => (13, 'u'),
        NamedKey::Tab => (9, 'u'),
        NamedKey::Backspace => (127, 'u'),
        NamedKey::Insert => (2, '~'),
        NamedKey::Delete => (3, '~'),
        NamedKey::ArrowLeft => (1, 'D'),
        NamedKey::ArrowRight => (1, 'C'),
        NamedKey::ArrowUp => (1, 'A'),
        NamedKey::ArrowDown => (1, 'B'),
        NamedKey::PageUp => (5, '~'),
        NamedKey::PageDown => (6, '~'),
        NamedKey::Home => (1, 'H'),
        NamedKey::End => (1, 'F'),
        NamedKey::F1 => (1, 'P'),
        NamedKey::F2 => (1, 'Q'),
        NamedKey::F3 => (13, '~'),
        NamedKey::F4 => (1, 'S'),
        NamedKey::F5 => (15, '~'),
        NamedKey::F6 => (17, '~'),
        NamedKey::F7 => (18, '~'),
        NamedKey::F8 => (19, '~'),
        NamedKey::F9 => (20, '~'),
        NamedKey::F10 => (21, '~'),
        NamedKey::F11 => (23, '~'),
        NamedKey::F12 => (24, '~'),
        NamedKey::CapsLock => (57358, 'u'),
        NamedKey::ScrollLock => (57359, 'u'),
        NamedKey::NumLock => (57360, 'u'),
        NamedKey::PrintScreen => (57361, 'u'),
        NamedKey::Pause => (57362, 'u'),
        NamedKey::ContextMenu => (57363, 'u'),
        NamedKey::Shift => (side(57441), 'u'),
        NamedKey::Control => (side(57442), 'u'),
        NamedKey::Alt => (side(57443), 'u'),
        NamedKey::Super => (side(57444), 'u'),
        NamedKey::Hyper => (side(57445), 'u'),
        NamedKey::Meta => (side(57446), 'u'),
        _ => return None,
    })
}

/// Encodes a key press, repeat or release with the kitty keyboard protocol
/// (`flags` from [`kitty`]). `None` means nothing is sent.
fn encode_kitty(event: &KeyEvent, mods: ModifiersState, flags: u16) -> Option<Vec<u8>> {
    let all = flags & kitty::ALL_KEYS != 0;
    let event_type = match (event.state, event.repeat) {
        (ElementState::Released, _) => 3,
        (_, true) => 2,
        _ => 1,
    };
    if event_type == 3 && flags & kitty::EVENT_TYPES == 0 {
        return None;
    }
    let m = 1
        + mods.shift_key() as u32
        + 2 * mods.alt_key() as u32
        + 4 * mods.control_key() as u32
        + 8 * mods.super_key() as u32;
    let with_event = |m: u32| {
        if flags & kitty::EVENT_TYPES != 0 && event_type != 1 {
            format!("{m}:{event_type}")
        } else {
            m.to_string()
        }
    };

    let named = match &event.logical_key {
        Key::Named(NamedKey::Space) => None,
        Key::Named(named) => Some(*named),
        _ => None,
    };
    if let Some(named) = named {
        let (code, last) = functional(named, event.location)?;
        if code >= 57441 && !all {
            return None;
        }
        // Without modifiers Enter, Tab and Backspace keep their legacy bytes
        // (and report no release) unless every key is to be escaped.
        if !all && m == 1 && matches!(code, 13 | 9 | 127) {
            return (event_type != 3).then(|| match code {
                13 => b"\r".to_vec(),
                9 => b"\t".to_vec(),
                _ => vec![0x7f],
            });
        }
        let mods = with_event(m);
        let bytes = match last {
            'u' | '~' if mods == "1" => format!("\x1b[{code}{last}"),
            'u' | '~' => format!("\x1b[{code};{mods}{last}"),
            _ if mods == "1" => format!("\x1b[{last}"),
            _ => format!("\x1b[1;{mods}{last}"),
        };
        return Some(bytes.into_bytes());
    }

    let base = match event.key_without_modifiers() {
        Key::Character(s) => s.chars().next()?.to_lowercase().next()?,
        Key::Named(NamedKey::Space) => ' ',
        _ => return None,
    };
    let text = event
        .text
        .as_deref()
        .filter(|t| !t.chars().any(char::is_control))
        .unwrap_or("");
    let plain = !mods.control_key() && !mods.alt_key() && !mods.super_key();
    if !all {
        if event_type == 3 {
            return None;
        }
        if plain && !text.is_empty() {
            return Some(text.as_bytes().to_vec());
        }
        if plain {
            return None;
        }
    }
    let mut key = (base as u32).to_string();
    if flags & kitty::ALTERNATE_KEYS != 0
        && mods.shift_key()
        && let Some(shifted) = text.chars().next().filter(|&c| c != base)
    {
        key = format!("{key}:{}", shifted as u32);
    }
    let mut out = format!("\x1b[{key}");
    let report_text = all && flags & kitty::TEXT != 0 && event_type != 3 && !text.is_empty();
    if m != 1 || event_type != 1 || report_text {
        out.push(';');
        out.push_str(&with_event(m));
    }
    if report_text {
        let codes: Vec<String> = text.chars().map(|c| (c as u32).to_string()).collect();
        out.push(';');
        out.push_str(&codes.join(":"));
    }
    out.push('u');
    Some(out.into_bytes())
}

pub fn encode_key(
    event: &KeyEvent,
    mods: ModifiersState,
    modes: &Modes,
    kitty_flags: u16,
) -> Option<Vec<u8>> {
    if kitty_flags != 0 {
        return encode_kitty(event, mods, kitty_flags);
    }
    if event.state == ElementState::Released {
        return None;
    }
    if let Key::Named(named) = &event.logical_key {
        let bytes = match named {
            NamedKey::Enter => with_alt(b"\r".to_vec(), mods),
            NamedKey::Backspace => with_alt(
                if mods.control_key() {
                    vec![0x08]
                } else {
                    vec![0x7f]
                },
                mods,
            ),
            NamedKey::Tab if mods.shift_key() => b"\x1b[Z".to_vec(),
            NamedKey::Tab => with_alt(b"\t".to_vec(), mods),
            NamedKey::Escape => with_alt(b"\x1b".to_vec(), mods),
            NamedKey::Space if mods.control_key() => with_alt(vec![0], mods),
            NamedKey::Space => with_alt(b" ".to_vec(), mods),
            NamedKey::ArrowUp => letter_key('A', mods, modes.app_cursor),
            NamedKey::ArrowDown => letter_key('B', mods, modes.app_cursor),
            NamedKey::ArrowRight => letter_key('C', mods, modes.app_cursor),
            NamedKey::ArrowLeft => letter_key('D', mods, modes.app_cursor),
            NamedKey::Home => letter_key('H', mods, modes.app_cursor),
            NamedKey::End => letter_key('F', mods, modes.app_cursor),
            NamedKey::Insert => tilde_key(2, mods),
            NamedKey::Delete => tilde_key(3, mods),
            NamedKey::PageUp => tilde_key(5, mods),
            NamedKey::PageDown => tilde_key(6, mods),
            NamedKey::F1 => letter_key('P', mods, true),
            NamedKey::F2 => letter_key('Q', mods, true),
            NamedKey::F3 => letter_key('R', mods, true),
            NamedKey::F4 => letter_key('S', mods, true),
            NamedKey::F5 => tilde_key(15, mods),
            NamedKey::F6 => tilde_key(17, mods),
            NamedKey::F7 => tilde_key(18, mods),
            NamedKey::F8 => tilde_key(19, mods),
            NamedKey::F9 => tilde_key(20, mods),
            NamedKey::F10 => tilde_key(21, mods),
            NamedKey::F11 => tilde_key(23, mods),
            NamedKey::F12 => tilde_key(24, mods),
            _ => return None,
        };
        return Some(bytes);
    }

    if mods.control_key() {
        // Use the unshifted key so Ctrl+Shift+letter etc. still map sensibly.
        if let Key::Character(s) = event.key_without_modifiers() {
            let mut chars = s.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                let code = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => Some(c as u8 & 0x1f),
                    '@' | '2' | ' ' => Some(0x00),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '^' | '6' => Some(0x1e),
                    '_' | '-' | '7' => Some(0x1f),
                    '/' => Some(0x1f),
                    '8' | '?' => Some(0x7f),
                    _ => None,
                };
                if let Some(code) = code {
                    return Some(with_alt(vec![code], mods));
                }
            }
        }
    }

    let text = event.text.as_ref()?;
    if text.is_empty() {
        return None;
    }
    if mods.alt_key() {
        // Alt sends ESC + the unmodified character (meta key behaviour).
        if let Key::Character(s) = event.key_without_modifiers() {
            let s = if mods.shift_key() {
                s.to_uppercase()
            } else {
                s.to_string()
            };
            return Some(with_alt(s.into_bytes(), mods));
        }
    }
    Some(text.as_bytes().to_vec())
}
