//! Unified CSS filter and effect routing across native ArkUI attributes,
//! direct GLES texture export (`Composite`), and surface-level `Scene` drawing.

use nsh_core::{BlendMode, Filter, ViewStyle};

/// Whether `filter` is a mathematical identity (produces no pixel or bounds change).
pub(crate) fn is_noop_filter(filter: &Filter) -> bool {
    match *filter {
        Filter::Blur(radius) => radius.0 <= 0.0,
        Filter::Brightness(v) | Filter::Contrast(v) | Filter::Saturate(v) => {
            (v - 1.0).abs() <= f32::EPSILON
        }
        Filter::Grayscale(v) | Filter::Sepia(v) | Filter::Invert(v) => v <= 0.0,
        Filter::HueRotate(angle) => {
            let rem = angle.0.rem_euclid(std::f32::consts::TAU);
            rem <= f32::EPSILON || (std::f32::consts::TAU - rem) <= f32::EPSILON
        }
        Filter::Opacity(v) => v >= 1.0,
        Filter::DropShadow { color, .. } => (color.0 & 0xff) == 0,
    }
}

pub(crate) fn active_filter_count(filters: &[Filter]) -> usize {
    filters
        .iter()
        .filter(|filter| !is_noop_filter(filter))
        .count()
}

pub(crate) fn has_active_filters(filters: &[Filter]) -> bool {
    filters.iter().any(|filter| !is_noop_filter(filter))
}

pub(crate) fn has_non_blur_filter(filters: &[Filter]) -> bool {
    filters
        .iter()
        .any(|filter| !is_noop_filter(filter) && !matches!(filter, Filter::Blur(_)))
}

/// Computes a node's combined visual opacity from its animation binding (if
/// present) or static style opacity, multiplied by presentation transitions.
/// Both the native visual layer and `Scene` composition consume this single source.
pub(crate) fn resolve_visual_opacity(
    style_opacity: f32,
    animated_opacity: Option<f32>,
    presentation: f32,
    extra: f32,
    navigation: f32,
) -> f32 {
    animated_opacity.unwrap_or(style_opacity) * presentation * extra * navigation
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EffectsContext {
    pub(crate) has_shaders: bool,
    /// Keeps a shader-capable node (or `ROOT_ID` while a shader page/scope is
    /// mounted and visible) in `Composite` identity-passthrough mode when its
    /// active shader layers are toggled off, avoiding `DirectRoot <-> TextureContainer`
    /// cutovers across toggles.
    pub(crate) shader_keep_alive: bool,
    pub(crate) shader_samples_backdrop: bool,
    pub(crate) navigation_capture: bool,
    pub(crate) scene_active: bool,
    pub(crate) composite_active: bool,
    pub(crate) has_native_paint: bool,
}

/// Single-source projection of where a node's filters, backdrop filters, and
/// blend mode execute.
///
/// Invariants:
/// - `native_blur > 0.0` is mutually exclusive with `requires_texture` and `scene_active`.
/// - `native_backdrop_blur > 0.0` is mutually exclusive with `requires_texture` and `scene_active`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EffectsPlan {
    /// Layer blur written to `visual_node()` (`Attribute::Blur`).
    pub(crate) native_blur: f32,
    /// Content backdrop blur written to `self.native` (`Attribute::BackdropBlur`).
    pub(crate) native_backdrop_blur: f32,
    /// Layer blend mode written to `visual_node()` (`Attribute::BlendMode`).
    pub(crate) native_blend: BlendMode,
    /// Whether this node's own style/shaders require surface `Scene` composition.
    pub(crate) requires_scene: bool,
    /// Whether this node requires offscreen texture export (`Composite`).
    pub(crate) requires_texture: bool,
}

impl EffectsPlan {
    pub(crate) fn compute(style: &ViewStyle, ctx: EffectsContext) -> Self {
        let effects = style.effects.as_ref();
        let filters = effects.map_or(&[][..], |e| e.filters.as_slice());
        let backdrop_filters = effects.map_or(&[][..], |e| e.backdrop_filters.as_slice());
        let has_masks = effects.is_some_and(|e| !e.mask_images.is_empty());

        let non_blur_filter = has_non_blur_filter(filters);
        let active_backdrops = active_filter_count(backdrop_filters);
        let non_blur_backdrop = has_non_blur_filter(backdrop_filters);

        // When a node exports its own subtree to `RENDER_TYPE_TEXTURE` (for
        // non-blur CSS filters, active shader layers, or shader keep-alive
        // passthrough), `self.native` renders into an isolated transparent
        // buffer and cannot sample the window backdrop via `Attribute::BackdropBlur`;
        // `Scene` must composite the backdrop instead.
        let own_texture =
            non_blur_filter || ctx.has_shaders || ctx.shader_keep_alive || ctx.navigation_capture;
        let requires_scene = style.blend_mode == BlendMode::PlusDarker
            || ctx.shader_samples_backdrop
            || has_masks
            || active_backdrops > 1
            || non_blur_backdrop
            || (active_backdrops > 0 && own_texture);

        let requires_texture = ctx.navigation_capture
            || (ctx.scene_active && ctx.has_native_paint)
            || non_blur_filter
            || ctx.has_shaders
            || ctx.shader_keep_alive;

        let native_active = !ctx.scene_active && !requires_texture && !ctx.composite_active;
        let mut native_blur = 0.0;
        let mut native_backdrop_blur = 0.0;
        if native_active {
            for filter in filters {
                if let Filter::Blur(radius) = filter
                    && radius.0 > 0.0
                {
                    native_blur += radius.0;
                }
            }
            if active_backdrops <= 1 && !non_blur_backdrop {
                for filter in backdrop_filters {
                    if let Filter::Blur(radius) = filter
                        && radius.0 > 0.0
                    {
                        native_backdrop_blur += radius.0;
                    }
                }
            }
        }

        let native_blend = if ctx.scene_active {
            BlendMode::Normal
        } else {
            style.blend_mode
        };

        Self {
            native_blur,
            native_backdrop_blur,
            native_blend,
            requires_scene,
            requires_texture,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nsh_core::{Color, Effects, Points, prelude::Radians};

    use super::*;

    #[test]
    fn noop_filters_cover_all_identity_variants() {
        assert!(is_noop_filter(&Filter::Brightness(1.0)));
        assert!(is_noop_filter(&Filter::Contrast(1.0)));
        assert!(is_noop_filter(&Filter::Saturate(1.0)));
        assert!(is_noop_filter(&Filter::Grayscale(0.0)));
        assert!(is_noop_filter(&Filter::Sepia(0.0)));
        assert!(is_noop_filter(&Filter::Invert(0.0)));
        assert!(is_noop_filter(&Filter::Opacity(1.0)));
        assert!(is_noop_filter(&Filter::Blur(Points(0.0))));
        assert!(is_noop_filter(&Filter::Blur(Points(-2.0))));
        assert!(is_noop_filter(&Filter::HueRotate(Radians(0.0))));
        assert!(is_noop_filter(&Filter::HueRotate(Radians(
            std::f32::consts::TAU
        ))));
        assert!(is_noop_filter(&Filter::DropShadow {
            offset_x: Points(4.0),
            offset_y: Points(4.0),
            blur: Points(8.0),
            color: Color::TRANSPARENT,
        }));

        assert!(!is_noop_filter(&Filter::Brightness(0.95)));
        assert!(!is_noop_filter(&Filter::Blur(Points(2.0))));
        assert!(!is_noop_filter(&Filter::Opacity(0.5)));
        assert!(!is_noop_filter(&Filter::DropShadow {
            offset_x: Points(0.0),
            offset_y: Points(2.0),
            blur: Points(4.0),
            color: Color::BLACK,
        }));
    }

    #[test]
    fn pure_blur_with_noop_filters_stays_on_native_without_texture_or_scene() {
        let style = ViewStyle {
            effects: Some(Arc::new(Effects {
                filters: vec![Filter::Blur(Points(6.0)), Filter::Brightness(1.0)],
                backdrop_filters: vec![Filter::Brightness(1.0), Filter::Blur(Points(10.0))],
                ..Default::default()
            })),
            ..ViewStyle::default()
        };
        let plan = EffectsPlan::compute(&style, EffectsContext::default());
        assert_eq!(plan.native_blur, 6.0);
        assert_eq!(plan.native_backdrop_blur, 10.0);
        assert!(!plan.requires_texture);
        assert!(!plan.requires_scene);
    }

    #[test]
    fn blur_is_not_double_applied_when_shaders_or_non_blur_filters_export_texture() {
        let style = ViewStyle {
            effects: Some(Arc::new(Effects {
                filters: vec![Filter::Blur(Points(8.0))],
                ..Default::default()
            })),
            ..ViewStyle::default()
        };
        let plan = EffectsPlan::compute(
            &style,
            EffectsContext {
                has_shaders: true,
                ..EffectsContext::default()
            },
        );
        assert!(plan.requires_texture);
        assert_eq!(plan.native_blur, 0.0);

        let retiring = EffectsPlan::compute(
            &style,
            EffectsContext {
                composite_active: true,
                ..EffectsContext::default()
            },
        );
        assert!(!retiring.requires_texture);
        assert_eq!(retiring.native_blur, 0.0);
    }

    #[test]
    fn single_backdrop_blur_combined_with_texture_filter_promotes_to_scene() {
        let style = ViewStyle {
            effects: Some(Arc::new(Effects {
                filters: vec![Filter::Brightness(0.9)],
                backdrop_filters: vec![Filter::Blur(Points(10.0))],
                ..Default::default()
            })),
            ..ViewStyle::default()
        };
        let pre_scene = EffectsPlan::compute(&style, EffectsContext::default());
        assert!(pre_scene.requires_scene);
        assert!(pre_scene.requires_texture);
        assert_eq!(pre_scene.native_backdrop_blur, 0.0);
        assert_eq!(pre_scene.native_blur, 0.0);

        let in_scene = EffectsPlan::compute(
            &style,
            EffectsContext {
                scene_active: true,
                has_native_paint: true,
                ..EffectsContext::default()
            },
        );
        assert!(in_scene.requires_scene);
        assert!(in_scene.requires_texture);
        assert_eq!(in_scene.native_backdrop_blur, 0.0);
        assert_eq!(in_scene.native_blend, BlendMode::Normal);
    }

    #[test]
    fn scene_requirements_follow_current_style_and_shaders() {
        let mut style = ViewStyle {
            blend_mode: BlendMode::PlusDarker,
            ..ViewStyle::default()
        };
        assert!(EffectsPlan::compute(&style, EffectsContext::default()).requires_scene);
        style.blend_mode = BlendMode::Normal;
        assert!(!EffectsPlan::compute(&style, EffectsContext::default()).requires_scene);

        let mut effects = Effects {
            backdrop_filters: vec![Filter::Blur(Points(4.0)), Filter::Blur(Points(8.0))],
            ..Default::default()
        };
        style.effects = Some(Arc::new(effects.clone()));
        assert!(EffectsPlan::compute(&style, EffectsContext::default()).requires_scene);

        effects.backdrop_filters.pop();
        style.effects = Some(Arc::new(effects));
        assert!(!EffectsPlan::compute(&style, EffectsContext::default()).requires_scene);
        assert!(
            EffectsPlan::compute(
                &style,
                EffectsContext {
                    has_shaders: true,
                    ..EffectsContext::default()
                }
            )
            .requires_scene
        );
    }

    #[test]
    fn idle_navigation_shader_keeps_root_and_backdrop_blur_native_until_capture_demanded() {
        let backdrop_style = ViewStyle {
            effects: Some(Arc::new(Effects {
                backdrop_filters: vec![Filter::Blur(Points(12.0))],
                ..Default::default()
            })),
            ..ViewStyle::default()
        };

        // Idle NavShader configuration sets `navigation_capture: false`: zero
        // persistent texture exports and single backdrop-blur stays native.
        let idle_root = EffectsPlan::compute(&ViewStyle::default(), EffectsContext::default());
        assert!(!idle_root.requires_texture);
        assert!(!idle_root.requires_scene);

        let idle_backdrop = EffectsPlan::compute(&backdrop_style, EffectsContext::default());
        assert!(!idle_backdrop.requires_texture);
        assert!(!idle_backdrop.requires_scene);
        assert_eq!(idle_backdrop.native_backdrop_blur, 12.0);

        // Active transition capture demand enables export only for the capture window.
        let demanded = EffectsPlan::compute(
            &backdrop_style,
            EffectsContext {
                navigation_capture: true,
                ..EffectsContext::default()
            },
        );
        assert!(demanded.requires_texture);
        assert!(demanded.requires_scene);
        assert_eq!(demanded.native_backdrop_blur, 0.0);

        // Releasing the last demand restores the native path.
        let restored = EffectsPlan::compute(&backdrop_style, EffectsContext::default());
        assert_eq!(restored, idle_backdrop);
    }

    #[test]
    fn shader_keep_alive_maintains_texture_export_in_passthrough_and_retires_when_cleared() {
        let plain = ViewStyle::default();
        let passthrough = EffectsPlan::compute(
            &plain,
            EffectsContext {
                shader_keep_alive: true,
                composite_active: true,
                ..EffectsContext::default()
            },
        );
        assert!(passthrough.requires_texture);
        assert!(!passthrough.requires_scene);

        // If the shader-capable node itself also carries a pure blur filter, GLES
        // continues owning the blur while passthrough Composite stays active.
        let blurred = ViewStyle {
            effects: Some(Arc::new(Effects {
                filters: vec![Filter::Blur(Points(6.0))],
                ..Default::default()
            })),
            ..ViewStyle::default()
        };
        let passthrough_blur = EffectsPlan::compute(
            &blurred,
            EffectsContext {
                shader_keep_alive: true,
                composite_active: true,
                ..EffectsContext::default()
            },
        );
        assert!(passthrough_blur.requires_texture);
        assert_eq!(passthrough_blur.native_blur, 0.0);

        // Unmount / visibility-gone / background clears `shader_keep_alive`,
        // retiring the export back to native.
        let retired = EffectsPlan::compute(&plain, EffectsContext::default());
        assert!(!retired.requires_texture);
        assert!(!retired.requires_scene);
    }

    #[test]
    fn visual_opacity_uses_animated_value_across_native_and_scene_paths() {
        assert!((resolve_visual_opacity(0.8, None, 1.0, 1.0, 1.0) - 0.8).abs() < 1e-6);
        assert!((resolve_visual_opacity(0.8, Some(0.25), 1.0, 1.0, 1.0) - 0.25).abs() < 1e-6);
        assert!((resolve_visual_opacity(0.8, Some(0.5), 0.5, 0.8, 0.5) - 0.1).abs() < 1e-6);
        // Unbinding restores the static style opacity without losing presentation factors.
        assert!((resolve_visual_opacity(0.6, None, 0.5, 1.0, 1.0) - 0.3).abs() < 1e-6);
    }
}
