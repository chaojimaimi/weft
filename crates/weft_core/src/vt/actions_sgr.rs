//! ``handle_sgr` dispatch` bodies for the Terminal facade. vt/mod.rs keeps the struct,
//! `process()`, and core accessors (v1.13.8 S2 zero-behavior file-budget
//! split; `impl Terminal` cross-file blocks per the screen_exit /
//! kitty_keyboard precedent). Bodies moved verbatim.
use super::{sgr_underline, Attrs, Terminal};

impl Terminal {
    /// Handle SGR (Select Graphic Rendition) — CSI m.
    ///
    /// vte parses `CSI 38;5;196m` as three separate param groups:
    ///   iter → [38], [5], [196]
    /// We flatten all sub-param first-values into a `Vec<u16>`, then walk it
    /// with an index so we can consume 1–4 values for color sequences.
    ///
    /// v1.11.3 (PLAN_v1113 §2.1): colon groups (`4:x`, `58:x`) are dispatched
    /// whole BEFORE flattening (handlers in `sgr_underline.rs`). Probe
    /// verdict (PLAN_v1113 step 1): vte materializes an empty `:` tail as
    /// an explicit `0` subparam — `4:` ≡ `4:0` ≡ clear underline; there is
    /// no "missing subparam" shape.
    pub(super) fn handle_sgr(&mut self, params: &vte::Params) {
        if params.is_empty() {
            self.attrs = Attrs::default();
            return;
        }

        // v1.0 perf: Use a stack-allocated array instead of Vec<u16>.
        // SGR sequences rarely exceed 16 params (truecolor: 38;2;R;G;B = 5).
        // The old `Vec<u16>::collect()` allocated on every SGR dispatch —
        // a hot path for colored output (e.g. `ls --color`, `rg`).
        const MAX_SGR_PARAMS: usize = 32;
        let mut buf = [0u16; MAX_SGR_PARAMS];
        let mut len = 0usize;
        for sub in params.iter() {
            if len >= MAX_SGR_PARAMS {
                break;
            }
            // v1.11.3: whole-group semantics so `4:3` never misparses as
            // `4` + DIM(`3`); handlers live in sgr_underline.rs.
            if sub.len() > 1 && sub[0] == 4 {
                sgr_underline::handle_underline_group(&mut self.attrs, sub);
                continue;
            }
            if sub.len() > 1 && sub[0] == 58 {
                sgr_underline::handle_underline_color_group(&mut self.attrs, sub);
                continue;
            }
            buf[len] = sub.first().copied().unwrap_or(0);
            len += 1;
        }
        let vals: &[u16] = &buf[..len];

        // v1.11.3: the flat walk (38/48/58 colors, attribute arms) lives in
        // sgr_underline.rs with the colon-group handlers — one SGR home,
        // and this facade stays within its architecture-gate budget.
        sgr_underline::apply_flat_sgr(&mut self.attrs, vals);
    }
}
