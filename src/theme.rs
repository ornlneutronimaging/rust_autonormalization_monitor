//! Coefficient design-system tokens and egui theming.
//!
//! Values follow ORNL's "Coefficient" LLM knowledge base (see
//! `llms-full.txt.rtf`): the ORNL Green brand (`#0F8723`) as the primary
//! interactive color, neutral surfaces as the foundation for backgrounds, and
//! the semantic status roles (Success / Danger / Warning / Info). Because this
//! is a desktop egui app rendered in dark mode, the neutral ramp uses dark
//! surfaces with light text — the design system's "dark theme parity".
//!
//! Guidance applied from the knowledge base:
//!  - Primary color denotes interactivity / the most important action.
//!  - Neutrals are the foundation; color is reserved for emphasis & status.
//!  - Type ramp establishes hierarchy: Header (structure) vs Body (reading).
//!  - Accessibility: white-on-`#0F8723` clears WCAG AA (~4.6:1); focus visible.

// This is a design-token palette: the full semantic set (e.g. `INFO`) and the
// complete spacing scale are defined for consistent reuse even where not yet
// referenced by the current screens.
#![allow(dead_code)]

use eframe::egui::{self, Color32, Stroke};

// --- Brand / primary (ORNL Green) ---
/// `--primary-rich`: brand green. Header background & primary button fill.
pub const PRIMARY_RICH: Color32 = Color32::from_rgb(0x0F, 0x87, 0x23);
/// Interactive/hover shade of primary.
pub const PRIMARY: Color32 = Color32::from_rgb(0x12, 0x9B, 0x2B);
/// `--primary-text-emphasis`: bright green for selected/interactive text on dark.
pub const PRIMARY_STRONG: Color32 = Color32::from_rgb(0x3D, 0xD1, 0x60);

// --- Neutral surfaces (dark) ---
/// `--surface-base`: page background.
pub const SURFACE_BASE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1C);
/// `--surface-weak`: raised panels.
pub const SURFACE_WEAK: Color32 = Color32::from_rgb(0x1D, 0x20, 0x27);
/// Container surface for list boxes and input fields.
pub const SURFACE_CONTAINER: Color32 = Color32::from_rgb(0x23, 0x27, 0x30);
/// `--neutral-border-subtle`: default borders & dividers.
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(0x3A, 0x3F, 0x4B);

// --- Neutral text ---
/// `--neutral-text-strong`: primary copy.
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xF2, 0xF3, 0xF5);
/// `--neutral-text-emphasis`: secondary text / metadata.
pub const TEXT_EMPHASIS: Color32 = Color32::from_rgb(0xA8, 0xB0, 0xBD);
/// `--text-white`: foreground on the branded (green) header & primary button.
pub const TEXT_WHITE: Color32 = Color32::WHITE;

// --- Semantic status ---
pub const SUCCESS: Color32 = Color32::from_rgb(0x2E, 0xA0, 0x43);
pub const DANGER: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);
pub const WARNING: Color32 = Color32::from_rgb(0xD1, 0x86, 0x16);
pub const INFO: Color32 = Color32::from_rgb(0x3B, 0x82, 0xF6);

// --- Spacing scale ---
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
pub const SPACE_MD: f32 = 12.0;
pub const SPACE_LG: f32 = 16.0;

/// Install the Coefficient theme (tokens, type ramp, spacing) onto the context.
/// Call once at startup; egui persists the style across frames.
pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // Font sizes and spacing are left at egui's defaults (matching the original
    // portal's density); hierarchy comes from weight (strong), color tokens, and
    // the branded header rather than an enlarged type ramp or looser spacing. The
    // design system specifies Mulish/Roboto, but we keep the default sans to
    // avoid bundling a font.

    let mut v = egui::Visuals::dark();
    v.panel_fill = SURFACE_BASE;
    v.window_fill = SURFACE_BASE;
    v.extreme_bg_color = SURFACE_CONTAINER; // text-edit background
    // Selected state uses primary (design system: primary for selected states).
    v.selection.bg_fill = PRIMARY;
    v.selection.stroke = Stroke::new(1.0, TEXT_WHITE);
    v.hyperlink_color = PRIMARY_STRONG;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
    v.widgets.inactive.bg_fill = SURFACE_WEAK;
    v.widgets.inactive.weak_bg_fill = SURFACE_WEAK;
    v.widgets.hovered.bg_fill = SURFACE_CONTAINER;
    v.widgets.hovered.weak_bg_fill = SURFACE_CONTAINER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    style.visuals = v;

    ctx.set_style(style);
}

/// A framed container surface (neutral fill + subtle border) used for the IPTS
/// and application lists — separation via surface & border tokens.
pub fn container_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE_CONTAINER)
        .stroke(Stroke::new(1.0, BORDER_SUBTLE))
        .corner_radius(4.0)
        .inner_margin(SPACE_XS)
}

/// A section heading (Header type role): weight + color structure content at
/// the default body size (no enlargement).
pub fn section_heading(text: &str) -> egui::RichText {
    egui::RichText::new(text).strong().color(TEXT_STRONG)
}

/// The single primary action button: ORNL Green fill with a white, title-case
/// label. Per the design system, reserve this for the most important action.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .color(TEXT_WHITE)
            .strong(),
    )
    .fill(PRIMARY_RICH)
    .min_size(egui::vec2(240.0, 36.0))
}
