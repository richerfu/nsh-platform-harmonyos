//! Finite capture bounds in surface units, including projected native overflow.
use nsh_core::{Filter, Matrix, Points, Size, ViewStyle};

/// Bound allocations uniformly; logical boxes never change with resolution.
pub(crate) fn target_size(width: f32, height: f32, scale: f32) -> (u32, u32, f32) {
    if !width.is_finite()
        || !height.is_finite()
        || !scale.is_finite()
        || width <= 0.0
        || height <= 0.0
        || scale <= 0.0
    {
        return (0, 0, 0.0);
    }
    let raw_w = (f64::from(width) * f64::from(scale)).ceil().max(1.0);
    let raw_h = (f64::from(height) * f64::from(scale)).ceil().max(1.0);
    let ratio = (4096.0 / raw_w.max(raw_h)).min(1.0);
    let w = (raw_w * ratio).round().clamp(1.0, 4096.0) as u32;
    let h = (raw_h * ratio).round().clamp(1.0, 4096.0) as u32;
    (w, h, scale * ratio as f32)
}

/// Logical coverage of a picture, independent of its pixel resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Coverage {
    pub(crate) box_size: Size<Points>,
    pub(crate) insets: [f32; 4],
}
impl Coverage {
    pub(crate) fn shader(box_size: Size<Points>, overflow: f32) -> Self {
        Self {
            box_size,
            insets: [overflow.max(0.0); 4],
        }
    }
    pub(crate) fn size(self) -> (f32, f32) {
        (
            self.box_size.width.0 + self.insets[0] + self.insets[2],
            self.box_size.height.0 + self.insets[1] + self.insets[3],
        )
    }
    pub(crate) fn pixels(self, scale: f32) -> (u32, u32, f32) {
        let (width, height) = self.size();
        target_size(width, height, scale)
    }
    /// Where the source picture lies in a destination pixel grid.
    pub(crate) fn destination(self, source: Self, size: (u32, u32)) -> [i32; 4] {
        let (width, height) = self.size();
        let sx = size.0 as f32 / width;
        let sy = size.1 as f32 / height;
        let left = self.insets[0] - source.insets[0];
        let top = self.insets[1] - source.insets[1];
        let (sw, sh) = source.size();
        [
            (left * sx).round() as i32,
            (top * sy).round() as i32,
            ((left + sw) * sx).round() as i32,
            ((top + sh) * sy).round() as i32,
        ]
    }
}

/// CSS `blur(<radius>)` standard deviation in physical pixels (`radius * scale`).
pub(crate) fn blur_sigma(radius: f32, scale: f32) -> f32 {
    radius.max(0.0) * scale
}

/// ArkUI/Rosen `DrawingDecorationPainter::ConvertRadiusToSigma` slope (`1 / sqrt(3)`).
const ARKUI_BLUR_SIGMA_SCALE: f32 = 0.57735;
const ARKUI_BLUR_SIGMA_OFFSET: f32 = 0.5;

/// Calibrates a CSS logical blur radius (`sigma = css_radius` in points) to the raw `f32`
/// expected by ArkUI C-API `NODE_BLUR` (`Attribute::Blur`) and `NODE_BACKDROP_BLUR`
/// (`Attribute::BackdropBlur`).
///
/// OpenHarmony 6.0 pipeline (`OpenHarmony-6.0-Release`):
/// 1. `arkui_ace_engine/interfaces/native/node/style_modifier.cpp` (`SetBlur` / `SetBackdropBlur`)
///    forwards `item->value[0].f32` to `frameworks/core/interfaces/native/node/node_common_modifier.cpp`
///    (`SetBlur` / `SetNodeBackdropBlur`), which wraps the float in
///    `CalcDimension(blur, DimensionUnit::PX)` — physical pixels, ignoring `LengthMetricUnit::Vp`
///    (despite `native_node.h`'s `NODE_BLUR` comment saying `vp`).
/// 2. `frameworks/core/components_ng/render/adapter/rosen_render_context.cpp`
///    (`SetFrontBlurFilter` / `SetBackBlurFilter`) converts that pixel radius via
///    `DrawingDecorationPainter::ConvertRadiusToSigma(r)` in `drawing_decoration_painter.h`:
///    `r > 0.0 ? 0.57735 * r + 0.5 : 0.0`.
/// 3. `graphic_graphic_2d/rosen/modules/render_service_base/src/render/rs_blur_filter.cpp`
///    passes the converted value directly as Gaussian `(sigmaX, sigmaY)` to
///    `Drawing::ImageFilter::CreateBlurImageFilter`.
///
/// Inverting `0.57735 * r + 0.5 = css_radius * scale` aligns native ArkUI blur with CSS
/// `filter: blur(R)` / `backdrop-filter: blur(R)` and our GLES/Scene `blur_sigma` path.
pub(crate) fn native_blur_value(css_radius: f32, scale: f32) -> f32 {
    if !css_radius.is_finite() || !scale.is_finite() || css_radius <= 0.0 || scale <= 0.0 {
        return 0.0;
    }
    let sigma_px = blur_sigma(css_radius, scale);
    if sigma_px <= ARKUI_BLUR_SIGMA_OFFSET {
        0.0
    } else {
        (sigma_px - ARKUI_BLUR_SIGMA_OFFSET) / ARKUI_BLUR_SIGMA_SCALE
    }
}

/// CSS `drop-shadow` / `box-shadow` blur radius to Gaussian sigma (`radius * scale * 0.5`),
/// matching W3C shadow blur and upstream Android/Apple/Windows implementations.
pub(crate) fn shadow_blur_sigma(radius: f32, scale: f32) -> f32 {
    radius.max(0.0) * scale * 0.5
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds(pub(crate) [f32; 4]);
impl Bounds {
    pub(crate) fn new(width: f32, height: f32) -> Self {
        Self([0.0, 0.0, width, height])
    }
    pub(crate) fn union(&mut self, other: Self) {
        self.0 = [
            self.0[0].min(other.0[0]),
            self.0[1].min(other.0[1]),
            self.0[2].max(other.0[2]),
            self.0[3].max(other.0[3]),
        ];
    }
    fn extend(&mut self, dx: f32, dy: f32, pad: f32) {
        let [a, b, c, d] = self.0;
        self.union(Self([
            a + dx - pad,
            b + dy - pad,
            c + dx + pad,
            d + dy + pad,
        ]));
    }
    pub(crate) fn shaders(
        mut self,
        fill_overflow: f32,
        layers: &[(
            nsh_core::NodeId,
            std::sync::Arc<nsh_core::shader::ShaderLayer>,
        )],
    ) -> Self {
        let overflow = layers
            .iter()
            .map(|(_, layer)| layer.overflow.0.max(0.0))
            .fold(fill_overflow.max(0.0), f32::max);
        self.extend(0.0, 0.0, overflow);
        self
    }
    pub(crate) fn own(mut self, style: &ViewStyle) -> Self {
        let border = self;
        if style.outline.is_visible() {
            self.extend(
                0.0,
                0.0,
                (style.outline.width.0 + style.outline.offset.0).max(0.0),
            );
        }
        if let Some(effects) = style.effects.as_ref() {
            for shadow in effects
                .box_shadows
                .iter()
                .filter(|s| !s.inset && s.is_visible())
            {
                let mut bounds = border;
                bounds.extend(
                    shadow.offset_x.0,
                    shadow.offset_y.0,
                    shadow.spread.0 + shadow.blur.0.max(0.0) * 1.5,
                );
                self.union(bounds);
            }
        }
        self
    }
    pub(crate) fn filters(mut self, filters: &[Filter]) -> Self {
        for filter in filters {
            if crate::render::effects::is_noop_filter(filter) {
                continue;
            }
            match *filter {
                Filter::Blur(radius) => self.extend(0.0, 0.0, blur_sigma(radius.0, 1.0) * 3.0),
                Filter::DropShadow {
                    offset_x,
                    offset_y,
                    blur,
                    ..
                } => self.extend(offset_x.0, offset_y.0, shadow_blur_sigma(blur.0, 1.0) * 3.0),
                _ => {}
            }
        }
        self
    }
    pub(crate) fn transformed(self, matrix: Matrix) -> Option<Self> {
        let m = matrix.0;
        let [a, b, c, d] = self.0;
        let mut result = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for (x, y) in [(a, b), (c, b), (c, d), (a, d)] {
            let (x, y) = (x as f64, y as f64);
            let w = x * m[3] + y * m[7] + m[15];
            if !w.is_finite() || w <= 1e-6 {
                return None;
            }
            let (x, y) = (
                (x * m[0] + y * m[4] + m[12]) / w,
                (x * m[1] + y * m[5] + m[13]) / w,
            );
            if !x.is_finite() || !y.is_finite() || x.abs() > 1e7 || y.abs() > 1e7 {
                return None;
            }
            result[0] = result[0].min(x as f32);
            result[1] = result[1].min(y as f32);
            result[2] = result[2].max(x as f32);
            result[3] = result[3].max(y as f32);
        }
        Some(Self(result))
    }
    pub(crate) fn insets(self, width: f32, height: f32, scale: f32) -> [u32; 4] {
        [
            (-self.0[0]).max(0.0),
            (-self.0[1]).max(0.0),
            (self.0[2] - width).max(0.0),
            (self.0[3] - height).max(0.0),
        ]
        .map(|v| (v * scale).ceil() as u32)
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{Color, Points, TransformKind};

    use super::*;
    #[test]
    fn shader_capture_uses_maximum_nonnegative_overflow() {
        use nsh_core::shader::{Shader, ShaderLayer};
        let shader =
            Shader::compile("fn shade(uv: vec2f) -> vec4f { return vec4f(uv, 0.0, 1.0); }")
                .unwrap();
        let layer = |overflow| {
            std::sync::Arc::new(
                ShaderLayer::new(shader.clone(), &[], &mut Vec::new())
                    .with_overflow(Points(overflow)),
            )
        };
        let layers = [(2, layer(6.0)), (4, layer(12.0)), (6, layer(-5.0))];
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(0.0, &layers).0,
            [-12.0, -12.0, 112.0, 62.0]
        );
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(0.0, &[]).0,
            [0.0, 0.0, 100.0, 50.0]
        );
        // Fills live in Shaders, separately from the effect stack. Native
        // texture export and the CSS scene must keep their overflow too.
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(10.0, &[]).0,
            [-10.0, -10.0, 110.0, 60.0]
        );
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(20.0, &layers).0,
            [-20.0, -20.0, 120.0, 70.0]
        );
    }
    #[test]
    fn oversized_targets_keep_the_box_and_aspect_ratio() {
        let coverage = Coverage::shader(Size::new(Points(3000.0), Points(1500.0)), 100.0);
        let (w, h, scale) = coverage.pixels(3.0);
        assert_eq!((w, h), (4096, 2176));
        assert!((scale - 1.28).abs() < 0.0001);
        assert_eq!(w as f32 / h as f32, 3200.0 / 1700.0);
        let words = nsh_core::shader::builtins(
            coverage.box_size,
            0.0,
            scale,
            nsh_core::geometry::Percentage(0.0),
            Points(100.0),
        );
        assert_eq!(
            [f32::from_bits(words[0]), f32::from_bits(words[1])],
            [3000.0, 1500.0]
        );
        assert_eq!(f32::from_bits(words[3]), scale);
        let (w, h, scale) = target_size(1.0, 10000.0, 1.0);
        assert_eq!((w, h), (1, 4096));
        assert!((scale - 0.4096).abs() < 0.0001);
        assert_eq!(target_size(0.0, 100.0, 3.0), (0, 0, 0.0));
        assert_eq!(target_size(100.0, 100.0, f32::INFINITY), (0, 0, 0.0));
    }
    #[test]
    fn asymmetric_capture_reframes_around_the_view_box() {
        let box_size = Size::new(Points(100.0), Points(50.0));
        let capture = Coverage {
            box_size,
            insets: [3.0, 7.0, 21.0, 11.0],
        };
        let shader = Coverage::shader(box_size, 10.0);
        // Source origin at (-3, -7), shader origin at (-10, -10), at 2 px/pt.
        assert_eq!(shader.destination(capture, (240, 140)), [14, 6, 262, 142]);
        assert_eq!(capture.destination(shader, (248, 136)), [-14, -6, 226, 134]);
        let first = nsh_core::shader::builtins(
            capture.box_size,
            0.0,
            2.0,
            nsh_core::geometry::Percentage(0.0),
            Points(10.0),
        );
        let second = nsh_core::shader::builtins(
            shader.box_size,
            0.0,
            2.0,
            nsh_core::geometry::Percentage(0.0),
            Points(10.0),
        );
        assert_eq!(first, second);
        assert_eq!(f32::from_bits(second[0]), 100.0);
        assert_eq!(f32::from_bits(second[1]), 50.0);
    }
    #[test]
    fn css_blur_radius_uses_full_sigma_while_drop_shadow_halves_radius() {
        assert_eq!(blur_sigma(8.0, 3.0), 24.0);
        assert_eq!(blur_sigma(-8.0, 3.0), 0.0);
        assert_eq!(shadow_blur_sigma(8.0, 3.0), 12.0);
        assert_eq!(shadow_blur_sigma(-8.0, 3.0), 0.0);
        assert_eq!(
            Bounds::new(10.0, 10.0)
                .filters(&[Filter::Blur(Points(8.0))])
                .0,
            [-24.0, -24.0, 34.0, 34.0]
        );
    }
    #[test]
    fn native_blur_value_inverts_rosen_convert_radius_to_sigma_in_device_pixels() {
        let rosen_sigma = |r: f32| {
            if r > 0.0 { 0.57735 * r + 0.5 } else { 0.0 }
        };
        for scale in [1.0_f32, 2.0, 3.0, 3.25] {
            for css_radius in [1.0_f32, 4.0, 8.0, 16.0, 24.0] {
                let raw = native_blur_value(css_radius, scale);
                let expected_sigma = blur_sigma(css_radius, scale);
                assert!(
                    (rosen_sigma(raw) - expected_sigma).abs() < 1e-3,
                    "css_radius={css_radius} scale={scale} raw={raw}"
                );
            }
        }
        assert_eq!(native_blur_value(0.0, 3.0), 0.0);
        assert_eq!(native_blur_value(-4.0, 3.0), 0.0);
        assert_eq!(native_blur_value(4.0, 0.0), 0.0);
        assert_eq!(native_blur_value(f32::NAN, 3.0), 0.0);
        assert_eq!(native_blur_value(4.0, f32::INFINITY), 0.0);
        // Sub-0.5px target sigma clamps cleanly to 0 rather than producing negative radii
        // or jumping to Rosen's >0.5px floor.
        assert_eq!(native_blur_value(0.2, 2.0), 0.0);
        assert_eq!(native_blur_value(0.25, 2.0), 0.0);
        assert!(native_blur_value(0.3, 2.0) > 0.0);
    }
    #[test]
    fn nested_shadow_overflow_and_negative_translation() {
        let child = Bounds::new(40.0, 20.0).filters(&[Filter::DropShadow {
            offset_x: Points(8.0),
            offset_y: Points(4.0),
            blur: Points(2.0),
            color: Color::BLACK,
        }]);
        let child = child
            .transformed(Matrix::of(TransformKind::TranslateX, -15.0))
            .unwrap();
        let mut parent = Bounds::new(30.0, 10.0);
        parent.union(child);
        assert_eq!(parent.insets(30.0, 10.0, 2.0), [30, 0, 12, 34]);
    }
    #[test]
    fn perspective_crossing_camera_has_no_finite_capture() {
        let mut matrix = Matrix::IDENTITY;
        matrix.0[3] = -0.1;
        assert!(Bounds::new(20.0, 10.0).transformed(matrix).is_none());
    }
    #[test]
    fn identity_filters_do_not_extend_bounds() {
        assert_eq!(
            Bounds::new(20.0, 20.0)
                .filters(&[
                    Filter::Brightness(1.0),
                    Filter::Blur(Points(0.0)),
                    Filter::DropShadow {
                        offset_x: Points(10.0),
                        offset_y: Points(10.0),
                        blur: Points(8.0),
                        color: Color::TRANSPARENT,
                    },
                ])
                .0,
            [0.0, 0.0, 20.0, 20.0]
        );
    }
}
