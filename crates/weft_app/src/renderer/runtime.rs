//! Runtime configuration, geometry, and display-scale methods for `MetalRenderer`.

use core_graphics_types::geometry::CGSize;
use tracing::info;
use winit::window::Window;

use crate::glyph::GlyphAtlas;
use crate::macos_window::set_layer_opaque;
use crate::renderer::MetalRenderer;
use weft_core::config::{FontConfig, Theme};

impl MetalRenderer {
    /// Swap the active theme. Recolors the whole screen on the next draw
    /// (colors are resolved per-frame from cells' color-origins + this theme +
    /// the terminal palette, so no rebuild is needed).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        // Colors are baked into cached vertices — force a full rebuild.
        self.force_full_grid.set(true);
        // v1.4.1: invalidate styled-line cache — theme colors are baked into
        // cached block-view vertices, so a theme change requires re-rendering
        // all styled lines from scratch.
        self.styled_line_cache.borrow_mut().bump_generation();
    }

    pub fn set_minimum_contrast(&mut self, minimum_contrast: f32) {
        let minimum_contrast =
            crate::settings_validation::runtime_minimum_contrast(minimum_contrast);
        if (self.minimum_contrast - minimum_contrast).abs() <= f32::EPSILON {
            return;
        }
        self.minimum_contrast = minimum_contrast;
        self.force_full_grid.set(true);
        self.styled_line_cache.borrow_mut().bump_generation();
    }

    pub fn set_semantic_output_enabled(&mut self, enabled: bool) {
        if self.semantic_output_enabled == enabled {
            return;
        }
        self.semantic_output_enabled = enabled;
        self.styled_line_cache.borrow_mut().bump_generation();
    }

    /// v1.0 P0-b: Force a full grid redraw on the next draw. Call on resize,
    /// tab switch, selection change, or any event that invalidates the
    /// per-row vertex cache. Takes `&self` (not `&mut self`) because it only
    /// touches `Cell` fields — needed so it can be called from within `draw()`
    /// while a `drawable` borrow is alive.
    pub fn force_full_grid_redraw(&self) {
        self.force_full_grid.set(true);
        // v1.0 P1.5-B2: a forced redraw means instances WILL change — clear
        // the unchanged flag so draw() doesn't skip the instance draw call.
        self.instances_unchanged.set(false);
    }

    /// Set popup dimensions (from App drag state).
    pub fn set_popup_size(&mut self, width_scale: f32, max_rows: usize) {
        self.popup_width_scale = width_scale;
        self.popup_max_rows = max_rows;
    }

    /// Content padding in physical pixels (logical config × scale).
    pub fn padding_x(&self) -> f32 {
        self.padding_x
    }

    /// Content padding in physical pixels (logical config × scale).
    pub fn padding_y(&self) -> f32 {
        self.padding_y
    }

    /// Update content padding (logical config px). Caller must recompute the
    /// grid layout afterwards — padding changes the usable rows/cols.
    pub fn set_padding(&mut self, padding_logical: (u32, u32)) {
        self.padding_x = padding_logical.0 as f32 * self.scale as f32;
        self.padding_y = padding_logical.1 as f32 * self.scale as f32;
    }

    /// Update window background opacity [0,1]. Recolors the next frame (bg
    /// alpha is resolved per-frame); also flips the CAMetalLayer opaque flag.
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = crate::settings_validation::runtime_opacity(opacity);
        // SAFETY: layer is a valid CAMetalLayer; setOpaque: is its property setter.
        unsafe {
            set_layer_opaque(&self.layer, self.opacity >= 1.0);
        }
        // v1.4.1: invalidate styled-line cache — background alpha is baked
        // into cached block-view vertices via `resolve_cell_color`, so an
        // opacity change requires re-rendering all styled lines.
        self.styled_line_cache.borrow_mut().bump_generation();
    }

    /// Current resolved theme (for applying to new tabs etc.).
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Viewport dimensions (width, height) in physical pixels.
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    /// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): consume the
    /// one-shot stage-4 RESIZE_PROBE gate armed by `resize()`; returns the
    /// resize→present elapsed time the caller logs once per resize.
    pub(crate) fn take_resize_present_probe(&self) -> Option<std::time::Instant> {
        self.resize_present_probe.take()
    }

    pub fn resize(&mut self, window: &Window, size: winit::dpi::PhysicalSize<u32>) {
        // Use physical pixels for viewport to match drawable_size and grid dimensions
        let vp_w = size.width as f32;
        let vp_h = size.height as f32;
        self.viewport = (vp_w, vp_h);
        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): arm the
        // stage-4 RESIZE_PROBE gate so the next draw logs the first present
        // after this resize (quantifies the blank interval / omp latency).
        self.resize_present_probe
            .set(Some(std::time::Instant::now()));
        // IMPORTANT: drawable_size must be in PHYSICAL PIXELS
        self.layer
            .set_drawable_size(CGSize::new(size.width as f64, size.height as f64));
        // v1.0 fix: invalidate per-row vertex cache on viewport change.
        // Without this, macOS live-resize can render a frame with old-cache
        // vertices at the new viewport size before ensure_offscreen_texture()
        // detects the change → text misalignment / flicker.
        self.force_full_grid_redraw();
        window.request_redraw();
    }

    /// Cell width in physical pixels (for terminal size calculation).
    pub fn cell_width(&self) -> u32 {
        self.atlas.cell_width
    }

    /// Cell height in physical pixels (for terminal size calculation).
    pub fn cell_height(&self) -> u32 {
        self.atlas.cell_height
    }

    /// Viewport width in physical pixels.
    pub fn viewport_width(&self) -> f32 {
        self.viewport.0
    }

    /// Backing-store scale (Retina factor).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Refresh every DPI-derived renderer value after a window moves between
    /// displays. Winit reports physical window sizes, while font size and
    /// configured padding are logical, so keeping an old scale would make the
    /// PTY geometry and Metal placement diverge again on the new display.
    pub fn update_scale(&mut self, scale: f64, padding_logical: (u32, u32)) -> bool {
        if !scale.is_finite() || scale <= 0.0 || (self.scale - scale).abs() < f64::EPSILON {
            return false;
        }

        self.scale = scale;
        self.padding_x = padding_logical.0 as f32 * scale as f32;
        self.padding_y = padding_logical.1 as f32 * scale as f32;
        self.atlas = GlyphAtlas::new(&self.device, &self.font_config, scale);

        // CAMetalLayer contentsScale is not exposed by metal-rs. Keep the raw
        // Objective-C message contained and unwind-protected per project rule.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            use objc2::msg_send;
            let layer_ptr: *mut objc2::runtime::AnyObject =
                (&*self.layer) as *const _ as *mut objc2::runtime::AnyObject;
            let _: () = msg_send![layer_ptr, setContentsScale: scale];
        }));

        self.force_full_grid_redraw();
        // v1.4.1: atlas rebuild invalidates glyph UVs baked into cached
        // block-view vertices, and the new scale changes pixel positions.
        self.styled_line_cache.borrow_mut().bump_generation();
        info!(
            scale,
            cell_width = self.atlas.cell_width,
            cell_height = self.atlas.cell_height,
            "renderer scale factor updated"
        );
        true
    }

    /// How many block-view content rows (at `pitch = ch * 1.1`) fit in the
    /// block region — the area above the prompt input box (editor mode) or the
    /// full viewport (command-executing mode). The scroll handler clamps
    /// `block_scroll_offset` to `total - visible`, so this must match the
    /// renderer's actual capacity to avoid blank space when scrolling.
    pub fn block_visible_rows(
        &self,
        prompt_lines: Option<usize>,
        cwd_header_active: bool,
    ) -> usize {
        self.layout_ctx
            .map(|ctx| crate::layout::block_visible_rows(&ctx, prompt_lines, cwd_header_active))
            .unwrap_or(1)
    }

    pub fn block_header_rows(&self) -> usize {
        let pitch = self.cell_height() as f32 * 1.1;
        crate::ui_tokens::compact_control_row_span(pitch, self.scale)
    }

    /// Block-view scroll metrics `(total, visible, max_scroll)` computed live
    /// from the layout cache. Shared by the redraw path (scrollbar thumb) and the
    /// wheel handler (scroll clamping) so the wheel never clamps against the
    /// per-frame cached value — which is `None` during grid-mode execution and
    /// would otherwise kill the just-entered `primary_history_view` (scroll-up
    /// during a running primary-screen TUI became a no-op). History blocks are
    /// stable while a command runs, so the retained cache yields the correct
    /// scrollable extent on demand.
    pub fn block_scroll_metrics(
        &self,
        terminal: &weft_core::vt::Terminal,
        pane_session_id: u64,
    ) -> (usize, usize, usize) {
        let cols = terminal.grid().num_cols;
        let header_rows = self.block_header_rows();
        let prompt_lines = crate::block_component::block_prompt_lines(terminal);
        let cwd_header = crate::layout::block_cwd_header_active(
            terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
            terminal.cwd().is_some(),
        );
        let visible = self.block_visible_rows(prompt_lines, cwd_header);
        let blocks = terminal.block_tracker().session_blocks();
        // v1.10.23 change 2: exact memo — the key covers every input, so the
        // wheel handler and the same-frame redraw scrollbar share one
        // computation instead of two O(n_blocks) scans per scroll tick.
        // `pane_session_id` scopes the key (the memo is renderer-global and
        // per-Tab version counters start at 0); `cwd_present` mirrors the
        // `+usize::from(live.cwd.or(terminal.cwd()).is_some())` term of
        // `block_content_metrics_with_cache` so an OSC 7 cwd change
        // mid-command invalidates the memo.
        let in_flight = terminal.block_tracker().in_flight();
        let (live_version, cwd_present) = match &in_flight {
            Some(live) => (Some(live.version), live.cwd.or(terminal.cwd()).is_some()),
            None => (None, false),
        };
        let key = crate::paint::live_cache::BlockScrollMetricsKey {
            pane_session_id,
            cols: cols as u32,
            num_rows: terminal.grid().num_rows as u32,
            live_version,
            blocks_len: blocks.len() as u32,
            first_block_id: blocks.first().map(|b| b.id.0),
            last_block_id: blocks.last().map(|b| b.id.0),
            cache_rebuilds: self.block_layout_cache.borrow().total_misses(),
            header_rows: header_rows as u32,
            visible: visible as u32,
            cwd_present,
        };
        if let Some((cached_key, cached)) = self.scroll_metrics_memo.get() {
            if cached_key == key {
                return cached;
            }
        }
        let cache = self.block_layout_cache.borrow();
        let mut live_cache = self.live_layout_cache.borrow_mut();
        if in_flight.is_some() {
            let live = terminal.block_tracker().in_flight().expect("just checked");
            live_cache.sync(live.output, pane_session_id, live.version, cols);
        }
        let (total, _) = crate::block_component::block_content_metrics_with_cache(
            terminal,
            cols,
            header_rows,
            Some(&*cache),
            Some(&live_cache),
        );
        let max_scroll = total.saturating_sub(visible);
        let result = (total, visible, max_scroll);
        self.scroll_metrics_memo.set(Some((key, result)));
        result
    }

    /// Rebuild the glyph atlas from a (possibly changed) font config — used on
    /// live config reload when font family/size/line-height changes. Returns
    /// the new cell dimensions so the caller can recompute grid rows/cols and
    /// PTY size.
    pub fn rebuild_atlas(&mut self, font_config: FontConfig) -> (u32, u32) {
        self.font_config = crate::settings_validation::runtime_atlas_font_config(&font_config);
        self.atlas = GlyphAtlas::new(&self.device, &self.font_config, self.scale);
        // v1.4.1: font/line-height change invalidates glyph UVs and cell
        // geometry baked into cached block-view vertices.
        self.styled_line_cache.borrow_mut().bump_generation();
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    /// v0.9 H1: Tab bar height in physical pixels. Only drawn when there
    /// are 2+ tabs. Roughly 1.5× cell height, clamped to [28, 40] logical
    /// pixels (× scale for physical). v1.1: lower bound raised 24→28 to
    /// comfortably fit the macOS traffic-light buttons (~28pt) when the tab
    /// bar doubles as the (transparent) titlebar.
    pub fn tab_bar_height(&self) -> f32 {
        let logical = self.cell_height() as f32 / self.scale as f32 * 1.5;
        let metrics = crate::ui_tokens::UiMetrics::for_scale(self.scale);
        (logical.clamp(28.0, 40.0) * self.scale as f32).max(metrics.control_compact)
    }

    /// v1.1: Reserved top space for the macOS titlebar (traffic lights) in
    /// physical pixels. Used as the minimum chrome_top even with a single tab
    /// so the transparent titlebar never overlaps terminal content. The native
    /// traffic lights are ~28pt tall; we reserve the same height here.
    pub fn titlebar_height(&self) -> f32 {
        28.0 * self.scale as f32
    }

    /// v1.1: Width reserved for the macOS traffic-light buttons at the top-left,
    /// in physical pixels. Tabs and content start to the right of this so they
    /// don't collide with the close/minimize/maximize buttons.
    pub fn traffic_lights_width(&self) -> f32 {
        72.0 * self.scale as f32
    }

    /// v0.9 W5: Left sidebar width in physical pixels (240 logical px × scale).
    /// Used as `chrome_left` when the history panel is in sidebar mode.
    /// F3-3: when a user override is set (via drag), it takes precedence over
    /// the responsive `SidebarMetrics` default.
    pub fn sidebar_width(&self) -> f32 {
        self.logical_sidebar_width() * self.scale as f32
    }

    /// Width reserved beside the terminal. In compact windows the history
    /// panel becomes an overlay drawer, so it keeps its visual width without
    /// shrinking the PTY or pushing tab controls outside the viewport.
    /// F3-3: respects the user override in Regular/Wide mode; Compact windows
    /// always push 0 (overlay drawer).
    pub fn sidebar_push_width(&self) -> f32 {
        let logical_viewport = self.viewport.0 / self.scale as f32;
        let class = crate::ui_tokens::ResponsiveClass::from_logical_width(logical_viewport);
        if class == crate::ui_tokens::ResponsiveClass::Compact {
            return 0.0;
        }
        self.logical_sidebar_width() * self.scale as f32
    }

    /// F3-3: The effective logical sidebar width — user override if set,
    /// otherwise the responsive `SidebarMetrics` default.
    fn logical_sidebar_width(&self) -> f32 {
        let logical_viewport = self.viewport.0 / self.scale as f32;
        crate::ui_tokens::sidebar_visual_width(logical_viewport, self.sidebar_width_override)
    }

    /// F3-3: Set the user sidebar width override (logical points). Clamped to
    /// [SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH]. Pass `None` to clear and fall
    /// back to the responsive default.
    pub fn set_sidebar_width(&mut self, width: Option<f32>) {
        self.sidebar_width_override = crate::settings_validation::runtime_sidebar_width(width)
            .map(crate::ui_tokens::clamp_sidebar_width);
    }

    /// v1.5.3: Set the brief config error hint shown in the bottom-left
    /// status badge when Settings is closed. Pass `None` to clear (e.g.
    /// after a successful reload). The renderer truncates to fit the
    /// available width so the caller can pass a full message.
    ///
    /// When `Some`, this takes priority over the terminal-state passthrough
    /// hint — a stale/broken config is more important to surface than the
    /// "passthrough" badge.
    pub fn set_config_status_hint(&mut self, hint: Option<String>) {
        self.config_status_hint = hint;
    }
}
