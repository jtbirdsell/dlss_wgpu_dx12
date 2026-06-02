//! Public input types for DLSS Frame Generation: the per-frame resource tags ([`FgResources`] /
//! [`FgResource`]) and the per-frame common constants ([`FgConstants`]).
//!
//! These are the data the host hands to [`super::frame_gen::Frame::tag`] and
//! [`super::frame_gen::Frame::set_constants`] each frame. They are deliberately plain, owned data
//! (not raw pointers): the unsafe FFI translation into `sl::Resource` / `sl::ResourceTag` /
//! `sl::Constants` happens inside the frame-gen module, against the proven spike layout.

use super::types::{
    Boolean, BufferType, Constants, Float2, K_BUFFER_TYPE_DEPTH, K_BUFFER_TYPE_HUD_LESS_COLOR,
    K_BUFFER_TYPE_MOTION_VECTORS, K_BUFFER_TYPE_UI_ALPHA, K_BUFFER_TYPE_UI_COLOR_AND_ALPHA,
};
use glam::{UVec2, Vec2};

/// `D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE` — the proven default state for FG input textures.
///
/// This is the state Streamline expects each tagged input to be in when it consumes the tag during
/// the interposed `Present`. The spike validated `0x80` for textures wgpu created with
/// `TEXTURE_BINDING`. If the runtime reports `eErrorMissingResourceState` or you see corruption,
/// override it per-resource with [`FgResource::with_resource_state`] to match the state wgpu has
/// actually left the resource in.
pub const D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE: u32 = 0x80;

/// A single texture handed to DLSS Frame Generation, with the D3D12 resource state SL should assume.
///
/// Dimensions and native (DXGI) format are derived from the [`wgpu::Texture`] automatically; the
/// only knob is the D3D12 resource state, which defaults to the proven
/// [`D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE`].
#[derive(Clone, Copy)]
pub struct FgResource<'a> {
    pub(crate) texture: &'a wgpu::Texture,
    pub(crate) resource_state: u32,
}

impl<'a> FgResource<'a> {
    /// Wraps a texture with the default, proven `PIXEL_SHADER_RESOURCE` (`0x80`) state.
    pub fn new(texture: &'a wgpu::Texture) -> Self {
        Self {
            texture,
            resource_state: D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        }
    }

    /// Overrides the assumed D3D12 resource state (a `D3D12_RESOURCE_STATES` bitmask).
    ///
    /// Use this only if SL reports `eErrorMissingResourceState` or you observe corruption with the
    /// default `0x80` — the value must match the state wgpu has left the texture in at present time.
    pub fn with_resource_state(mut self, state: u32) -> Self {
        self.resource_state = state;
        self
    }

    /// Render-resolution dimensions, read from the texture.
    pub(crate) fn dimensions(&self) -> UVec2 {
        let size = self.texture.size();
        UVec2::new(size.width, size.height)
    }

    /// Native DXGI format, derived from the texture's wgpu format.
    pub(crate) fn native_format(&self) -> u32 {
        dxgi_format_of(self.texture.format())
    }
}

/// How the user interface layer is supplied to DLSS Frame Generation, if at all.
///
/// DLSS-G can recomposite a separate UI layer over the generated frame so the UI stays crisp. It
/// accepts either a combined color+alpha texture (tagged as `kBufferTypeUIColorAndAlpha`) or an
/// alpha-only mask (tagged as `kBufferTypeUIHintTagForHiResColor` / `UI_ALPHA`).
pub enum FgUi<'a> {
    /// A UI texture carrying both color and alpha (e.g. `Rgba8Unorm`).
    ColorAndAlpha(FgResource<'a>),
    /// An alpha-only UI hint mask.
    Alpha(FgResource<'a>),
}

/// The set of textures tagged for DLSS Frame Generation each frame.
///
/// `depth` and `motion_vectors` are required (DLSS-G interpolates from them). `hudless_color` and
/// `ui` are optional: provide `hudless_color` (the scene color *without* UI, matching the back
/// buffer's format) plus a [`FgUi`] layer when you want crisp UI recomposition over generated
/// frames; omit both to let DLSS-G interpolate the back buffer as-is.
pub struct FgResources<'a> {
    /// Depth buffer at render resolution.
    pub depth: FgResource<'a>,
    /// Screen-space motion vectors at render resolution (see [`FgConstants::with_pixel_motion`]).
    pub motion_vectors: FgResource<'a>,
    /// Optional HUD-less scene color (same format as the swapchain back buffer).
    pub hudless_color: Option<FgResource<'a>>,
    /// Optional UI layer for recomposition over the generated frame.
    pub ui: Option<FgUi<'a>>,
}

impl<'a> FgResources<'a> {
    /// The `(FgResource, sl::BufferType)` pairs to tag this frame, in the spike's proven order:
    /// depth, motion vectors, then (if present) HUD-less color and UI.
    pub(crate) fn tags(&self) -> Vec<(FgResource<'a>, BufferType)> {
        // Gather the tagged resources in the proven order (depth, motion vectors, optional HUD-less,
        // optional UI), then zip them against [`tag_buffer_types`] — the pure ordering helper — so
        // the type order has a single source of truth that is unit-tested without a device.
        let mut resources: Vec<FgResource<'a>> = vec![self.depth, self.motion_vectors];
        if let Some(hudless) = self.hudless_color {
            resources.push(hudless);
        }
        match &self.ui {
            Some(FgUi::ColorAndAlpha(r)) | Some(FgUi::Alpha(r)) => resources.push(*r),
            None => {}
        }
        let types = tag_buffer_types(
            self.hudless_color.is_some(),
            self.ui.as_ref().map(UiTagKind::of),
        );
        resources.into_iter().zip(types).collect()
    }
}

/// Which UI buffer type a tagged [`FgUi`] layer maps to. Lets the pure [`tag_buffer_types`] ordering
/// helper stay independent of the `wgpu`-bearing [`FgUi`] so it can be unit-tested without a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiTagKind {
    ColorAndAlpha,
    Alpha,
}

impl UiTagKind {
    fn of(ui: &FgUi<'_>) -> Self {
        match ui {
            FgUi::ColorAndAlpha(_) => UiTagKind::ColorAndAlpha,
            FgUi::Alpha(_) => UiTagKind::Alpha,
        }
    }
}

/// The `sl::BufferType`s to tag this frame, in the spike's proven order: depth, motion vectors, then
/// (if present) HUD-less color and the UI layer. Pure (no `wgpu`), so it is unit-tested directly;
/// [`FgResources::tags`] zips its resources against this exact order.
pub(crate) fn tag_buffer_types(has_hudless: bool, ui: Option<UiTagKind>) -> Vec<BufferType> {
    let mut out = Vec::with_capacity(4);
    out.push(K_BUFFER_TYPE_DEPTH);
    out.push(K_BUFFER_TYPE_MOTION_VECTORS);
    if has_hudless {
        out.push(K_BUFFER_TYPE_HUD_LESS_COLOR);
    }
    match ui {
        Some(UiTagKind::ColorAndAlpha) => out.push(K_BUFFER_TYPE_UI_COLOR_AND_ALPHA),
        Some(UiTagKind::Alpha) => out.push(K_BUFFER_TYPE_UI_ALPHA),
        None => {}
    }
    out
}

/// Per-frame common constants handed to DLSS Frame Generation via `slSetConstants`.
///
/// Camera matrices default to identity, which is correct for the common case where *all* motion
/// (object and camera) is baked into the motion-vector buffer. The fields most likely to need
/// tuning are [`Self::jitter_offset`], [`Self::mvec_scale`], [`Self::reset`],
/// [`Self::depth_inverted`], and [`Self::camera_motion_included`].
///
/// # Motion-vector scaling (a proven requirement)
/// Motion vectors must be normalized to the `[-1, 1]` range. If your motion-vector buffer stores
/// per-pixel displacement in **render-resolution pixels** (the usual case), set
/// `mvec_scale = (1/width, 1/height)` — use the [`Self::with_pixel_motion`] helper. The spike
/// proved that leaving this at `(1, 1)` (so DLSS-G reads an 8-pixel mvec as "8 screens/frame") is
/// what kept `numFramesActuallyPresented` stuck at 1; the correct scale flipped it to 2.
#[derive(Clone, Copy, Debug)]
pub struct FgConstants {
    /// Camera view-to-clip (projection) matrix, row-major. Identity by default.
    pub camera_view_to_clip: [[f32; 4]; 4],
    /// Inverse of [`Self::camera_view_to_clip`]. Identity by default.
    pub clip_to_camera_view: [[f32; 4]; 4],
    /// Reprojection: current clip space to previous-frame clip space. Identity by default.
    pub clip_to_prev_clip: [[f32; 4]; 4],
    /// Reprojection: previous-frame clip space to current clip space. Identity by default.
    pub prev_clip_to_clip: [[f32; 4]; 4],
    /// Subpixel jitter applied to the camera this frame. `(0, 0)` if not jittering.
    pub jitter_offset: Vec2,
    /// Scale that normalizes motion-vector values to `[-1, 1]`. See [`Self::with_pixel_motion`].
    pub mvec_scale: Vec2,
    /// Reset temporal history (set `true` on the first frame and on camera cuts / discontinuities).
    pub reset: bool,
    /// Whether the depth buffer is reversed (near = 1.0). `false` for standard `[0,1]` depth.
    pub depth_inverted: bool,
    /// Whether the motion-vector buffer already encodes full (object + camera) motion. When `true`,
    /// DLSS-G uses the mvecs verbatim and does not synthesize camera motion from the matrices —
    /// this is the proven setting when your mvec buffer carries complete motion.
    pub camera_motion_included: bool,
    /// Near plane distance (a sane, non-degenerate frustum value is required even with identity
    /// matrices).
    pub camera_near: f32,
    /// Far plane distance.
    pub camera_far: f32,
    /// Vertical field of view, in radians.
    pub camera_fov: f32,
    /// Aspect ratio (width / height).
    pub camera_aspect_ratio: f32,
}

impl FgConstants {
    /// Identity-camera defaults: identity matrices, no jitter, `mvec_scale = (1, 1)`, history reset
    /// off, standard depth, full motion in the mvec buffer.
    ///
    /// Remember to call [`Self::with_pixel_motion`] if your motion vectors are in pixels, and to set
    /// [`Self::reset`] `true` on the first frame.
    pub fn new() -> Self {
        Self {
            camera_view_to_clip: IDENTITY4,
            clip_to_camera_view: IDENTITY4,
            clip_to_prev_clip: IDENTITY4,
            prev_clip_to_clip: IDENTITY4,
            jitter_offset: Vec2::ZERO,
            mvec_scale: Vec2::ONE,
            reset: false,
            depth_inverted: false,
            camera_motion_included: true,
            camera_near: 0.1,
            camera_far: 1000.0,
            camera_fov: 1.0,
            camera_aspect_ratio: 16.0 / 9.0,
        }
    }

    /// Sets [`Self::mvec_scale`] to `(1/width, 1/height)` — the proven normalization for a
    /// motion-vector buffer that stores per-pixel displacement in render-resolution pixels.
    ///
    /// This is requirement (3) of DLSS-G: pixel-space mvecs must be normalized to `[-1, 1]`.
    pub fn with_pixel_motion(mut self, render_resolution: UVec2) -> Self {
        self.mvec_scale = Vec2::new(
            1.0 / render_resolution.x.max(1) as f32,
            1.0 / render_resolution.y.max(1) as f32,
        );
        self
    }

    /// Derives the FG per-frame constants from the **same** [`crate::DlssRenderParameters`] used for
    /// the NGX Super Resolution evaluate, so SR and FG share one source of truth for jitter,
    /// motion-vector scale, and history reset (a common source of SR↔FG drift bugs when combining
    /// the two). `render_resolution` is the resolution the motion-vector buffer is in.
    ///
    /// **It converts the motion-vector convention**, which differs between the two features: NGX SR
    /// consumes motion vectors in **render-resolution pixels** (multiplied by
    /// `DlssRenderParameters::motion_vector_scale`, default `(1, 1)`), whereas Streamline FG's
    /// `mvec_scale` **normalizes** them to `[-1, 1]`. Feeding the *same* buffer to both therefore
    /// requires `fg.mvec_scale = sr.motion_vector_scale / render_resolution` — which this computes.
    ///
    /// Camera matrices, [`Self::depth_inverted`], and [`Self::camera_motion_included`] are **not**
    /// derivable from `DlssRenderParameters`; they keep [`Self::new`]'s defaults (identity camera,
    /// standard depth, full motion in the mvec buffer). Set them on the returned value if your scene
    /// needs otherwise (e.g. `depth_inverted` for reversed-Z). `reset` is also auto-forced on frame 0
    /// by [`super::frame_gen::Frame::set_constants`].
    pub fn from_render_parameters(
        params: &crate::DlssRenderParameters,
        render_resolution: UVec2,
    ) -> Self {
        Self::derive(
            params.motion_vector_scale,
            params.jitter_offset,
            params.reset,
            render_resolution,
        )
    }

    /// Ray Reconstruction counterpart of [`Self::from_render_parameters`]: derives the FG constants
    /// from the same [`crate::DlssRayReconstructionParameters`] used for the NGX RR evaluate, with
    /// the identical motion-vector-scale conversion.
    #[cfg(feature = "ray-reconstruction")]
    pub fn from_ray_reconstruction_parameters(
        params: &crate::DlssRayReconstructionParameters,
        render_resolution: UVec2,
    ) -> Self {
        Self::derive(
            params.motion_vector_scale,
            params.jitter_offset,
            params.reset,
            render_resolution,
        )
    }

    /// Shared core of the SR/RR → FG constant bridges: converts the NGX render-resolution-pixel
    /// motion-vector scale into Streamline's normalized `mvec_scale` and carries jitter + reset.
    fn derive(
        motion_vector_scale: Option<Vec2>,
        jitter_offset: Vec2,
        reset: bool,
        render_resolution: UVec2,
    ) -> Self {
        let sr_mv_scale = motion_vector_scale.unwrap_or(Vec2::ONE);
        let mut c = Self::new();
        c.mvec_scale = Vec2::new(
            sr_mv_scale.x / render_resolution.x.max(1) as f32,
            sr_mv_scale.y / render_resolution.y.max(1) as f32,
        );
        c.jitter_offset = jitter_offset;
        c.reset = reset;
        c
    }

    /// Builds the `#[repr(C)]` [`Constants`] handed to `slSetConstants`, copying every field into
    /// the spike-proven layout. Camera basis vectors and the remaining `sl::Constants` fields keep
    /// the validated defaults from [`Constants::new`].
    pub(crate) fn to_sl(self) -> Constants {
        let mut c = Constants::new();
        c.camera_view_to_clip = to_float4x4(self.camera_view_to_clip);
        c.clip_to_camera_view = to_float4x4(self.clip_to_camera_view);
        c.clip_to_prev_clip = to_float4x4(self.clip_to_prev_clip);
        c.prev_clip_to_clip = to_float4x4(self.prev_clip_to_clip);
        c.jitter_offset = Float2 {
            x: self.jitter_offset.x,
            y: self.jitter_offset.y,
        };
        c.mvec_scale = Float2 {
            x: self.mvec_scale.x,
            y: self.mvec_scale.y,
        };
        c.depth_inverted = bool_to_sl(self.depth_inverted);
        c.camera_motion_included = bool_to_sl(self.camera_motion_included);
        c.reset = bool_to_sl(self.reset);
        c.camera_near = self.camera_near;
        c.camera_far = self.camera_far;
        c.camera_fov = self.camera_fov;
        c.camera_aspect_ratio = self.camera_aspect_ratio;
        c
    }
}

impl Default for FgConstants {
    fn default() -> Self {
        Self::new()
    }
}

const IDENTITY4: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn bool_to_sl(b: bool) -> Boolean {
    if b { Boolean::True } else { Boolean::False }
}

fn to_float4x4(m: [[f32; 4]; 4]) -> super::types::Float4x4 {
    use super::types::Float4;
    super::types::Float4x4 {
        row: [
            Float4 {
                x: m[0][0],
                y: m[0][1],
                z: m[0][2],
                w: m[0][3],
            },
            Float4 {
                x: m[1][0],
                y: m[1][1],
                z: m[1][2],
                w: m[1][3],
            },
            Float4 {
                x: m[2][0],
                y: m[2][1],
                z: m[2][2],
                w: m[2][3],
            },
            Float4 {
                x: m[3][0],
                y: m[3][1],
                z: m[3][2],
                w: m[3][3],
            },
        ],
    }
}

/// Maps a [`wgpu::TextureFormat`] to its numeric `DXGI_FORMAT` value (dxgiformat.h), for SL's
/// `sl::Resource::nativeFormat` and the `DLSSGOptions::*BufferFormat` fields.
///
/// Covers the formats an FG integration realistically uses for its tagged inputs (depth, motion
/// vectors, HUD-less color, UI) and common swapchain formats; anything unmapped falls back to
/// `DXGI_FORMAT_UNKNOWN` (0) with a warning, which SL tolerates (it then infers the format).
pub(crate) fn dxgi_format_of(format: wgpu::TextureFormat) -> u32 {
    use wgpu::TextureFormat as F;
    match format {
        // Color / swapchain
        F::Bgra8Unorm => 87,     // DXGI_FORMAT_B8G8R8A8_UNORM
        F::Bgra8UnormSrgb => 91, // DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        F::Rgba8Unorm => 28,     // DXGI_FORMAT_R8G8B8A8_UNORM
        F::Rgba8UnormSrgb => 29, // DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        F::Rgba16Float => 10,    // DXGI_FORMAT_R16G16B16A16_FLOAT
        F::Rgb10a2Unorm => 24,   // DXGI_FORMAT_R10G10B10A2_UNORM
        // Depth
        F::R32Float => 41,                             // DXGI_FORMAT_R32_FLOAT
        F::Depth32Float => 40,                         // DXGI_FORMAT_D32_FLOAT
        F::Depth24Plus | F::Depth24PlusStencil8 => 45, // DXGI_FORMAT_D24_UNORM_S8_UINT
        // Motion vectors
        F::Rg16Float => 34, // DXGI_FORMAT_R16G16_FLOAT
        F::Rg32Float => 16, // DXGI_FORMAT_R32G32_FLOAT
        // UI alpha-only mask
        F::R8Unorm => 61,  // DXGI_FORMAT_R8_UNORM
        F::R16Float => 54, // DXGI_FORMAT_R16_FLOAT
        other => {
            log::warn!(
                "dxgi_format_of: unmapped wgpu format {other:?}; using DXGI_FORMAT_UNKNOWN(0)"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::Boolean;
    use super::{
        FgConstants, K_BUFFER_TYPE_DEPTH, K_BUFFER_TYPE_HUD_LESS_COLOR,
        K_BUFFER_TYPE_MOTION_VECTORS, K_BUFFER_TYPE_UI_ALPHA, K_BUFFER_TYPE_UI_COLOR_AND_ALPHA,
        UiTagKind, dxgi_format_of, tag_buffer_types,
    };
    use glam::{UVec2, Vec2};

    fn approx(a: Vec2, b: Vec2) {
        assert!(
            (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6,
            "expected {b:?}, got {a:?}"
        );
    }

    #[test]
    fn with_pixel_motion_is_reciprocal_resolution() {
        let c = FgConstants::new().with_pixel_motion(UVec2::new(800, 600));
        approx(c.mvec_scale, Vec2::new(1.0 / 800.0, 1.0 / 600.0));
    }

    #[test]
    fn from_sr_params_converts_pixel_mvec_scale_to_normalized() {
        // SR motion-vector scale (2,2) in render-resolution pixels, render 1000x500 → FG normalized
        // mvec_scale = sr_scale / render_resolution = (2/1000, 2/500) = (0.002, 0.004). Jitter and
        // reset are carried through verbatim.
        let c = FgConstants::derive(
            Some(Vec2::new(2.0, 2.0)),
            Vec2::new(0.3, 0.7),
            true,
            UVec2::new(1000, 500),
        );
        approx(c.mvec_scale, Vec2::new(0.002, 0.004));
        approx(c.jitter_offset, Vec2::new(0.3, 0.7));
        assert!(c.reset);
    }

    #[test]
    fn from_sr_params_none_scale_defaults_to_one_over_resolution() {
        // No explicit SR scale (mvecs already in render-res pixels) → FG (1/w, 1/h), i.e. identical
        // to with_pixel_motion.
        let c = FgConstants::derive(None, Vec2::ZERO, false, UVec2::new(1920, 1080));
        approx(c.mvec_scale, Vec2::new(1.0 / 1920.0, 1.0 / 1080.0));
        assert!(!c.reset);
    }

    #[test]
    fn dxgi_format_of_maps_known_formats() {
        use wgpu::TextureFormat as F;
        // Color / swapchain, depth, motion-vector, and UI formats an FG integration realistically
        // tags — the exact DXGI_FORMAT numbers SL keys off.
        assert_eq!(dxgi_format_of(F::Bgra8Unorm), 87);
        assert_eq!(dxgi_format_of(F::Bgra8UnormSrgb), 91);
        assert_eq!(dxgi_format_of(F::Rgba8Unorm), 28);
        assert_eq!(dxgi_format_of(F::Rgba8UnormSrgb), 29);
        assert_eq!(dxgi_format_of(F::Rgba16Float), 10);
        assert_eq!(dxgi_format_of(F::Rgb10a2Unorm), 24);
        assert_eq!(dxgi_format_of(F::R32Float), 41);
        assert_eq!(dxgi_format_of(F::Depth32Float), 40);
        assert_eq!(dxgi_format_of(F::Depth24Plus), 45);
        assert_eq!(dxgi_format_of(F::Depth24PlusStencil8), 45);
        assert_eq!(dxgi_format_of(F::Rg16Float), 34);
        assert_eq!(dxgi_format_of(F::Rg32Float), 16);
        assert_eq!(dxgi_format_of(F::R8Unorm), 61);
        assert_eq!(dxgi_format_of(F::R16Float), 54);
    }

    #[test]
    fn dxgi_format_of_unmapped_is_unknown_zero() {
        // An unmapped format falls back to DXGI_FORMAT_UNKNOWN (0), which SL tolerates.
        assert_eq!(dxgi_format_of(wgpu::TextureFormat::Rgba32Float), 0);
    }

    #[test]
    fn tag_buffer_types_orders_depth_mvec_then_optionals() {
        assert_eq!(
            tag_buffer_types(false, None),
            vec![K_BUFFER_TYPE_DEPTH, K_BUFFER_TYPE_MOTION_VECTORS]
        );
        assert_eq!(
            tag_buffer_types(true, None),
            vec![
                K_BUFFER_TYPE_DEPTH,
                K_BUFFER_TYPE_MOTION_VECTORS,
                K_BUFFER_TYPE_HUD_LESS_COLOR,
            ]
        );
        assert_eq!(
            tag_buffer_types(true, Some(UiTagKind::ColorAndAlpha)),
            vec![
                K_BUFFER_TYPE_DEPTH,
                K_BUFFER_TYPE_MOTION_VECTORS,
                K_BUFFER_TYPE_HUD_LESS_COLOR,
                K_BUFFER_TYPE_UI_COLOR_AND_ALPHA,
            ]
        );
        assert_eq!(
            tag_buffer_types(true, Some(UiTagKind::Alpha)),
            vec![
                K_BUFFER_TYPE_DEPTH,
                K_BUFFER_TYPE_MOTION_VECTORS,
                K_BUFFER_TYPE_HUD_LESS_COLOR,
                K_BUFFER_TYPE_UI_ALPHA,
            ]
        );
        // The two optionals are independent: UI present without HUD-less.
        assert_eq!(
            tag_buffer_types(false, Some(UiTagKind::Alpha)),
            vec![
                K_BUFFER_TYPE_DEPTH,
                K_BUFFER_TYPE_MOTION_VECTORS,
                K_BUFFER_TYPE_UI_ALPHA,
            ]
        );
    }

    #[test]
    fn to_sl_translates_booleans_and_copies_scales() {
        // Pin the hand-copied FgConstants -> sl::Constants translation (the silent-corruption class
        // the module warns about): bool -> sl::Boolean and the mvec/jitter scale copy.
        let mut c = FgConstants::new();
        c.reset = true;
        c.depth_inverted = true;
        c.camera_motion_included = false;
        c.mvec_scale = Vec2::new(0.5, 0.25);
        c.jitter_offset = Vec2::new(0.125, 0.0625);
        let sl = c.to_sl();
        assert_eq!(sl.reset, Boolean::True);
        assert_eq!(sl.depth_inverted, Boolean::True);
        assert_eq!(sl.camera_motion_included, Boolean::False);
        approx(
            Vec2::new(sl.mvec_scale.x, sl.mvec_scale.y),
            Vec2::new(0.5, 0.25),
        );
        approx(
            Vec2::new(sl.jitter_offset.x, sl.jitter_offset.y),
            Vec2::new(0.125, 0.0625),
        );
    }

    #[test]
    fn new_has_proven_defaults() {
        let c = FgConstants::new();
        approx(c.mvec_scale, Vec2::ONE);
        assert!(!c.reset);
        assert!(c.camera_motion_included);
        assert!(!c.depth_inverted);
    }
}
