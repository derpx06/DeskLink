//! Input as the phone receives it: held modifiers ([`Mods`]) and evdev button
//! codes, the mouse, scroll and key packets, and the keymap cache the
//! worker's lookup fills.
//!
//! Sibling edges: none (the parent's limits only).

use super::*;

pub(super) fn mousepad(build: impl FnOnce(&mut NetworkPacket)) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
    build(&mut packet);
    packet
}

pub(super) fn scroll(dx: f64, dy: f64) -> NetworkPacket {
    mousepad(|packet| {
        packet.set("scroll", true);
        packet.set("dx", dx);
        packet.set("dy", dy);
    })
}

/// A key press for the phone: a special key's number or the text it types,
/// with the modifiers held.
pub(super) fn key_packet(key: gdk::Key, mods: Mods) -> Option<NetworkPacket> {
    let special = crate::remote_control::map_gdk_key_to_special(key);
    let text = key
        .to_unicode()
        .filter(|character| !character.is_control())
        .map(|character| character.to_string());
    if special == 0 && text.is_none() {
        return None;
    }
    Some(mousepad(|packet| {
        if special != 0 {
            packet.set("specialKey", special);
        } else if let Some(text) = text {
            packet.set("key", text);
        }
        packet.set("shift", mods.shift);
        packet.set("ctrl", mods.ctrl);
        packet.set("alt", mods.alt);
        packet.set("super", mods.meta);
    }))
}

/// What the keymap said for each (key, level), for [`KEY_TTL`] (the user may
/// switch layout), at most [`KEY_CACHE_MAX`] of them.
#[derive(Default)]
pub(super) struct KeyCache(pub(super) HashMap<(u32, Mods), (Option<u32>, Instant)>);

/// How long a keymap answer is kept.
pub(super) const KEY_TTL: Duration = Duration::from_secs(30);

impl KeyCache {
    pub(super) fn get(&self, key: (u32, Mods), now: Instant) -> Option<Option<u32>> {
        self.0
            .get(&key)
            .filter(|(_, at)| now.duration_since(*at) < KEY_TTL)
            .map(|(found, _)| *found)
    }

    pub(super) fn put(&mut self, key: (u32, Mods), found: Option<u32>, now: Instant) {
        if self.0.len() >= KEY_CACHE_MAX {
            self.0.clear();
        }
        self.0.insert(key, (found, now));
    }
}

/// Held modifiers, from the captured keys themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) struct Mods {
    pub(super) shift: bool,
    pub(super) ctrl: bool,
    pub(super) alt: bool,
    pub(super) altgr: bool,
    pub(super) meta: bool,
    pub(super) caps: bool,
}

/// evdev codes.
pub(super) const KEY_ESC: u32 = 1;
pub(super) const BTN_LEFT: u32 = 0x110;
pub(super) const BTN_RIGHT: u32 = 0x111;
pub(super) const BTN_MIDDLE: u32 = 0x112;
pub(super) const BTN_SIDE: u32 = 0x113;
pub(super) const BTN_EXTRA: u32 = 0x114;

impl Mods {
    /// Updates for a modifier key; false when `code` is not one.
    pub(super) fn update(&mut self, code: u32, pressed: bool) -> bool {
        let held = match code {
            42 | 54 => &mut self.shift,
            29 | 97 => &mut self.ctrl,
            56 => &mut self.alt,
            100 => &mut self.altgr,
            125 | 126 => &mut self.meta,
            58 => return true, // Caps Lock: its state comes as `Locked`.
            _ => return false,
        };
        *held = pressed;
        true
    }

    /// The GDK modifier state that picks a key's level.
    pub(super) fn level_state(self) -> gdk::ModifierType {
        let mut state = gdk::ModifierType::empty();
        if self.shift {
            state |= gdk::ModifierType::SHIFT_MASK;
        }
        if self.caps {
            state |= gdk::ModifierType::LOCK_MASK;
        }
        if self.altgr {
            // XKB's Mod5 is AltGr (ISO_Level3_Shift) on common layouts.
            state |= gdk::ModifierType::from_bits_retain(1 << 7);
        }
        state
    }
}
