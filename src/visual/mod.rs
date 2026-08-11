//! TideWM's visual identity, animation, and compositor-owned UI.
//!
//! The modules remain re-exported at the crate root so established paths such
//! as `crate::ripple` and `crate::toast` do not change as part of this physical
//! source-tree reorganization.

pub(crate) mod animation;
pub(crate) mod backdrop;
pub(crate) mod cascade_transition;
pub(crate) mod caustics;
pub(crate) mod compass;
pub(crate) mod decoration;
pub(crate) mod depth;
pub(crate) mod depth_deck;
pub(crate) mod depth_transition;
pub(crate) mod error_overlay;
pub(crate) mod float_physics;
pub(crate) mod frost_glass;
pub(crate) mod minimap;
pub(crate) mod ocean_canvas;
pub(crate) mod overview;
pub(crate) mod ripple;
pub(crate) mod shadow;
#[cfg(feature = "screencast")]
pub(crate) mod source_picker;
pub(crate) mod sway;
pub(crate) mod swim;
pub(crate) mod tab_strip;
pub(crate) mod text;
pub(crate) mod toast;
pub(crate) mod ui_theme;
pub(crate) mod viscosity;
pub(crate) mod wallpaper;
pub(crate) mod water_glass;
pub(crate) mod welcome;
pub(crate) mod window_animation;
pub(crate) mod workspace_transition;
