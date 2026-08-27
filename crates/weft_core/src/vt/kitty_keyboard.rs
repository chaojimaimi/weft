//! v1.11.4 (PLAN_v1114 §1): kitty keyboard protocol negotiation state.
//!
//! Decision source: docs/PLAN_v1114_KITTY_KEYBOARD.md v2 — the ONLY contract
//! for this module. Divergences from the kitty spec are deliberate and are
//! pinned here so nobody "fixes" them back to spec:
//!
//! 1. **0b100 ReportAlternateKeys is NOT supported** — requested flags are
//!    AND-ed with [`SUPPORTED_FLAG_MASK`] (foot's strategy) and the encoder
//!    never emits a `:sk` shifted sub-segment (M5).
//! 2. **Ctrl+letter stays C0 at every level** (Weft explicit deviation) —
//!    `Effect::InterruptPty`'s `[0x03]` atomic interrupt chain + shell
//!    experience. kitty/foot/wezterm CSI-ify ctrl+keys at L1.
//! 3. **L4 pure-modifier presses are still dropped** (explicit deviation) —
//!    nvim/Helix don't consume them; actual impact is small.
//!
//! Semantics (v2 table, M1-M8 folded in):
//! - `CSI > flags u` push onto the CURRENT screen's stack (masked);
//!   omitted flags default to 0; full stack evicts the oldest entry.
//! - `CSI < [count] u` pop count times — **count is the first parameter**
//!   (`CSI < 5 u` arrives as params=[5]); an empty stack stays 0.
//! - `CSI = flags ; mode u` **rewrites the stack top in place** (M1) and
//!   never changes stack depth; mode 1 = full assignment, 2 = set bits,
//!   3 = clear bits; an empty stack materializes one entry first.
//!   The golden regression `push F; set G; pop ⇒ 0` pins this.
//! - `CSI ? [flags] u` answers the current flags with `CSI ? <flags> u`;
//!   a request parameter is ignored (no conditional-answer concept).
//! - The two screens (primary/alternate) keep independent stacks; DEC
//!   1049/47 swaps never move flags (kitty spec).
//!
//! The perform.rs `'u'` arm with non-empty intermediates dispatches here
//! through [`Terminal::kitty_keyboard_op`]; the bare `CSI u` DECRC alias
//! arm is untouched. Disabled (`[compat] kitty_keyboard = false`) swallows
//! all four ops (v1.11.3 silence).

use std::collections::VecDeque;

use super::param;
use super::replies;
use super::Terminal;

/// Maximum stack depth; a full stack evicts the OLDEST entry (kitty spec).
pub const STACK_CAP: usize = 10;

/// Flags this terminal implements: 0b1 Disambiguate | 0b10 ReportEventTypes
/// | 0b10000 ReportAssociatedText. 0b100 ReportAlternateKeys is deliberately
/// absent (module doc, §1), and 0b1000 ReportAllKeysAsEscapeCodes is
/// masked-out pending spec verification of the functional-key L4 forms
/// (rust-reviewer v1.11.4 Blocker-2, downgrade clause — Home/End/F-key
/// terminator shapes must be verified against `kitten show-key` before the
/// bit is unmasked; the encoder's L4 rows are unreachable while masked).
pub const SUPPORTED_FLAG_MASK: u8 = 0b1_0011;

/// kitty keyboard-protocol flag bits (the LSB of each feature group).
pub const FLAG_DISAMBIGUATE: u8 = 0b1;
pub const FLAG_REPORT_EVENT_TYPES: u8 = 0b10;
pub const FLAG_REPORT_ALL_KEYS: u8 = 0b1000;
pub const FLAG_REPORT_ASSOCIATED_TEXT: u8 = 0b10000;

/// Negotiation state: one independent stack per screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KittyKeyboardState {
    /// `[main, alternate]` — index by `alt_active as usize` at call sites;
    /// swap_alt never moves data between stacks (module doc).
    stacks: [VecDeque<u8>; 2],
}

impl KittyKeyboardState {
    /// `CSI > flags u`: push the masked flags onto the live screen's stack,
    /// evicting the oldest entry when the stack is full.
    pub fn push(&mut self, alt_active: bool, flags: u8) {
        let stack = &mut self.stacks[alt_active as usize];
        let masked = flags & SUPPORTED_FLAG_MASK;
        if stack.len() == STACK_CAP {
            stack.pop_front();
        }
        stack.push_back(masked);
    }

    /// `CSI < [count] u`: pop up to `count` entries. Popping an empty stack
    /// is a no-op — the query still answers 0 (弹空清零 by construction:
    /// `flags()` returns 0 for an empty stack).
    pub fn pop(&mut self, alt_active: bool, count: usize) {
        let stack = &mut self.stacks[alt_active as usize];
        for _ in 0..count {
            if stack.pop_back().is_none() {
                break;
            }
        }
    }

    /// `CSI = flags ; mode u`: rewrite the stack top IN PLACE (M1 — the
    /// stack depth is owned solely by push/pop). An empty stack materializes
    /// a single entry before the rewrite.
    pub fn set(&mut self, alt_active: bool, flags: u8, mode: u8) {
        let stack = &mut self.stacks[alt_active as usize];
        let masked = flags & SUPPORTED_FLAG_MASK;
        if let Some(top) = stack.back_mut() {
            *top = match mode {
                2 => *top | masked,
                3 => *top & !masked,
                // mode 1 (and anything else): full assignment — set the
                // given bits, clear the others.
                _ => masked,
            };
        } else {
            stack.push_back(masked);
        }
    }

    /// Current flags = stack top; empty stack = 0 (弹空清零).
    pub fn flags(&self, alt_active: bool) -> u8 {
        self.stacks[alt_active as usize]
            .back()
            .copied()
            .unwrap_or(0)
    }

    /// Clear BOTH stacks (reset hooks: OSC 133;D and PtyExit; also RIS).
    pub fn reset(&mut self) {
        self.stacks[0].clear();
        self.stacks[1].clear();
    }
}

impl Terminal {
    /// v1.11.4 (PLAN_v1114 §1.2): dispatch the kitty keyboard-protocol CSI
    /// `u` ops (`>`, `<`, `=`, `?` intermediates). The perform.rs arm that
    /// calls this is the ONLY `!intermediates.is_empty()` `u` handling; the
    /// DEC private-mode gate above stays h/l-only, so `CSI ? u` never leaks
    /// into `handle_dec_private_mode`.
    pub(crate) fn kitty_keyboard_op(&mut self, intermediates: &[u8], params: &vte::Params) {
        if !self.kitty_protocol_enabled {
            // Disabled ([compat] kitty_keyboard=false): swallow all four
            // ops — the pre-v1.11.4 silence, byte for byte.
            return;
        }
        let alt = self.capabilities.alt_active;
        match intermediates.first().copied() {
            Some(b'>') => self.kitty.push(alt, param(params, 0, 0) as u8),
            Some(b'<') => self.kitty.pop(alt, param(params, 0, 1) as usize),
            Some(b'=') => {
                let flags = param(params, 0, 0) as u8;
                let mode = param(params, 1, 1) as u8;
                self.kitty.set(alt, flags, mode);
            }
            Some(b'?') => {
                let flags = self.kitty.flags(alt);
                self.respond(&replies::kitty_flags_reply(flags));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> KittyKeyboardState {
        KittyKeyboardState::default()
    }

    // ── core semantics ──────────────────────────────────────────────

    #[test]
    fn empty_stack_flags_zero() {
        let s = state();
        assert_eq!(s.flags(false), 0);
        assert_eq!(s.flags(true), 0);
    }

    #[test]
    fn push_masks_unsupported_flags() {
        let mut s = state();
        s.push(false, 0b11111111);
        assert_eq!(s.flags(false), SUPPORTED_FLAG_MASK);
        s.push(false, 0b100); // ReportAlternateKeys — cut
                              // stack top is now 0b100 & mask = 0
        assert_eq!(s.flags(false), 0);
    }

    #[test]
    fn push_without_flags_is_zero() {
        let mut s = state();
        s.push(false, 0);
        assert_eq!(s.flags(false), 0);
    }

    #[test]
    fn pop_returns_to_previous_flags() {
        let mut s = state();
        s.push(false, 0b1);
        s.push(false, 0b1_0011);
        assert_eq!(s.flags(false), 0b1_0011);
        s.pop(false, 1);
        assert_eq!(s.flags(false), 0b1);
        s.pop(false, 1);
        assert_eq!(s.flags(false), 0, "empty stack ⇒ flags=0");
    }

    #[test]
    fn pop_count_pops_multiple() {
        let mut s = state();
        s.push(false, 0b1);
        s.push(false, 0b10);
        s.push(false, 0b1000);
        s.pop(false, 2);
        assert_eq!(s.flags(false), 0b1);
    }

    #[test]
    fn pop_empty_stack_stays_zero() {
        let mut s = state();
        s.pop(false, 5);
        assert_eq!(s.flags(false), 0);
        s.push(false, 0b1);
        s.pop(false, 99); // oversized count drains whole stack
        assert_eq!(s.flags(false), 0);
    }

    // ── M1: set rewrites the top in place, depth unchanged ──────────

    /// The v2 golden regression: `push F; set G; pop ⇒ 0`. With v1's
    /// push-a-new-entry semantics the pop would leave F behind — kitty/foot/
    /// wezterm rewrite the TOP; the stack is owned by push/pop only.
    #[test]
    fn m1_set_rewrites_top_in_place_and_pop_drains_to_zero() {
        let mut s = state();
        s.push(false, 0b1); // F
        assert_eq!(s.flags(false), 0b1);
        s.set(false, 0b1_0000, 1); // G (full assignment)
        assert_eq!(s.flags(false), 0b1_0000);
        assert_eq!(s.stacks[0].len(), 1, "set must not change stack depth");
        s.pop(false, 1);
        assert_eq!(s.flags(false), 0, "push F; set G; pop ⇒ 0");
    }

    #[test]
    fn set_mode_1_is_full_assignment() {
        let mut s = state();
        s.push(false, 0b11111);
        s.set(false, 0b1_0000, 1);
        assert_eq!(s.flags(false), 0b1_0000, "mode 1 replaces the whole value");
    }

    #[test]
    fn set_mode_2_only_sets_bits() {
        let mut s = state();
        s.push(false, 0b1);
        s.set(false, 0b1010, 2);
        assert_eq!(s.flags(false), 0b11);
    }

    #[test]
    fn set_mode_3_only_clears_bits() {
        let mut s = state();
        s.push(false, 0b11111);
        assert_eq!(s.flags(false), 0b10011, "push masks to SUPPORTED");
        s.set(false, 0b1010, 3);
        // 0b10011 & !0b1010 = 0b10001
        assert_eq!(s.flags(false), 0b10001);
    }

    #[test]
    fn set_on_empty_stack_materializes_one_entry() {
        let mut s = state();
        s.set(false, 0b1_0001, 1);
        assert_eq!(s.stacks[0].len(), 1);
        assert_eq!(s.flags(false), 0b1_0001);
        s.pop(false, 1);
        assert_eq!(s.flags(false), 0);
    }

    // ── eviction ────────────────────────────────────────────────────

    #[test]
    fn full_stack_evicts_oldest() {
        let mut s = state();
        for i in 0..STACK_CAP as u8 {
            s.push(false, 1 << (i % 4) | 0b1);
        }
        assert_eq!(s.stacks[0].len(), STACK_CAP);
        s.push(false, 0b1000);
        assert_eq!(s.stacks[0].len(), STACK_CAP, "cap is hard");
        // The oldest entries (1<<0 | 1, 1<<1 | 1) were evicted; the newest
        // is 0b1000.
        s.pop(false, STACK_CAP);
        assert_eq!(s.flags(false), 0);
        s.push(false, 0b1);
        assert_eq!(s.flags(false), 0b1);
    }

    // ── per-screen isolation ─────────────────────────────────────────

    #[test]
    fn screens_keep_independent_stacks() {
        let mut s = state();
        s.push(false, 0b1);
        s.push(true, 0b1011);
        assert_eq!(s.flags(false), 0b1);
        assert_eq!(s.flags(true), 0b11);
        // Entering/leaving the alt screen never moves data (no swap hook).
        s.pop(true, 1);
        assert_eq!(s.flags(false), 0b1, "alt pops must not touch main");
        assert_eq!(s.flags(true), 0);
    }

    #[test]
    fn alt_set_pops_isolated_from_main() {
        let mut s = state();
        s.push(false, 0b11111);
        s.push(true, 0b1);
        s.set(true, 0b1000, 3); // clear bit 3: 0b1 & !0b1000 = 0b1
        assert_eq!(s.flags(true), 0b1);
        s.pop(true, 1);
        assert_eq!(s.flags(true), 0);
        assert_eq!(s.flags(false), 0b10011);
    }

    #[test]
    fn reset_clears_both_stacks() {
        let mut s = state();
        s.push(false, 0b1);
        s.push(true, 0b1011);
        s.reset();
        assert_eq!(s.flags(false), 0);
        assert_eq!(s.flags(true), 0);
    }

    // ── flags accessor honours the enabled gate ─────────────────────

    #[test]
    fn keyboard_protocol_flags_zero_when_disabled() {
        let mut t = Terminal::new(3, 10);
        // enabled by default
        t.process(b"\x1b[>1u\x1b[?u");
        assert_eq!(t.take_response(), b"\x1b[?1u");
        t.process(b"\x1b[?u");
        assert_eq!(t.take_response(), b"\x1b[?1u");
        t.set_kitty_protocol_enabled(false);
        assert_eq!(t.keyboard_protocol_flags(), 0);
    }
}
