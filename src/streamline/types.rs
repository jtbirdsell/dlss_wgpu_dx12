//! `#[repr(C)]` mirrors of the Streamline 2.11.1 structs we touch.
//!
//! Everything here is transcribed by hand from the SL headers (the headers are C++ and cannot be
//! bindgen'd). The single most dangerous detail is `BaseStructure`:
//!
//! ```c
//! struct BaseStructure {
//!     BaseStructure* next;     // 8 bytes
//!     StructType     structType; // 16 bytes (GUID)
//!     size_t         structVersion; // 8 bytes  <-- size_t / usize, NOT u32
//! };
//! ```
//!
//! `structVersion` being `size_t` (8 bytes) rather than `u32` is the classic silent-corruption
//! trap: a `u32` there misaligns every subsequent field. We model it as `usize`.
//!
//! `StructType` is `{ u32 data1; u16 data2; u16 data3; u8 data4[8] }` (a Windows-style GUID). We
//! keep those four fields so the in-memory layout matches the C++ struct exactly (16 bytes, 4-byte
//! aligned).
//!
//! The const-asserts at the bottom of this file cross-check every struct's `size_of` against the
//! MSVC `sizeof()` of the real `sl::` struct. They have already caught real ABI mistakes (a `u32`
//! where a `size_t` belonged, a missing trailing version field) — do not remove them.

#![allow(non_snake_case)]
#![allow(dead_code)]

use core::ffi::c_void;

/// Errors raised by the Streamline substrate (interposer loader + signature verification).
///
/// Mirrors the `thiserror` style of [`crate::nvsdk_ngx::DlssError`]: each variant carries enough
/// context to diagnose a load/verify failure without a debugger. The FFI layer returns
/// `Result<_, StreamlineError>` instead of the spike's ad-hoc `String` errors.
#[derive(thiserror::Error, Debug)]
pub enum StreamlineError {
    /// The `STREAMLINE_SDK` environment variable was not set, so the interposer path could not be
    /// resolved at runtime.
    #[error(
        "the STREAMLINE_SDK environment variable is not set; cannot locate sl.interposer.dll (expected at $STREAMLINE_SDK/bin/x64/sl.interposer.dll)"
    )]
    SdkPathNotSet,

    /// The resolved interposer path does not exist on disk.
    #[error("sl.interposer.dll was not found at the expected path: {0}")]
    InterposerNotFound(std::path::PathBuf),

    /// The interposer's Authenticode signature failed verification (untrusted, unsigned, revoked,
    /// or not chaining to a trusted root). Loading is hard-gated on this passing.
    #[error("sl.interposer.dll signature verification failed: {0}")]
    SignatureVerificationFailed(String),

    /// The interposer is validly signed, but the signer subject does not contain "NVIDIA".
    #[error("sl.interposer.dll is signed, but the signer subject is not NVIDIA (subject: {0:?})")]
    UntrustedSigner(String),

    /// `LoadLibrary`/`libloading` failed to load the (verified) interposer DLL.
    #[error("failed to load sl.interposer.dll from {path}: {source}")]
    LibraryLoadFailed {
        /// The interposer path that failed to load.
        path: std::path::PathBuf,
        /// The underlying `libloading` error.
        #[source]
        source: libloading::Error,
    },

    /// A required exported `sl*` symbol was missing from the interposer.
    #[error("missing exported Streamline symbol {symbol:?}: {source}")]
    MissingExport {
        /// The exported `sl*` symbol name that was missing.
        symbol: String,
        /// The underlying `libloading` error.
        #[source]
        source: libloading::Error,
    },

    /// `slGetFeatureFunction` failed to *resolve* a feature-level function (the resolution itself
    /// returned non-Ok or a null pointer). Reserved for resolution failures only; a *resolved*
    /// function that later returns non-Ok surfaces as [`StreamlineError::SlCall`].
    #[error("slGetFeatureFunction(feature={feature}, {function:?}) failed: {detail}")]
    FeatureFunctionUnavailable {
        /// The Streamline feature the function belongs to.
        feature: Feature,
        /// The feature-function name that failed to resolve.
        function: String,
        /// Detail about why resolution failed.
        detail: String,
    },

    /// A *resolved* Streamline function returned a non-Ok [`SlResult`]. Carries the typed result so
    /// callers can match on specific conditions (e.g. [`SlResult::ErrorOSDisabledHWS`]).
    #[error("{function} returned {result:?}")]
    SlCall {
        /// The `sl*` function name that failed (e.g. `"slInit"`).
        function: String,
        /// The typed result the interposer returned.
        result: SlResult,
    },

    /// A [`crate::FrameGenerationContext`] was already created from this [`crate::Streamline`]
    /// handle (it yields exactly one context). The original handle is left intact and reusable.
    #[error("a FrameGenerationContext was already created from this Streamline handle")]
    ContextAlreadyCreated,

    /// `slIsFeatureSupported(kFeatureDLSS_G)` reported that DLSS Frame Generation is not supported on
    /// this adapter/driver/OS. Carries the typed result for diagnosis.
    #[error("DLSS Frame Generation is not supported on this system: slIsFeatureSupported returned {0:?}")]
    FeatureNotSupported(SlResult),

    /// `surface.get_current_texture()` returned a non-presentable status (`Outdated`/`Lost`/other).
    /// This is a recoverable, per-frame condition — the caller should reconfigure the surface and
    /// retry on the next frame. (`Suboptimal` is intentionally passed through, not surfaced here.)
    #[error("the wgpu surface is unavailable this frame ({status}); reconfigure the surface and retry")]
    SurfaceUnavailable {
        /// The `wgpu::CurrentSurfaceTexture` status that was not `Success`/`Suboptimal`.
        status: String,
    },
}

/// `sl::StructType` — a 16-byte GUID. Layout matches the C++ `{u32; u16; u16; u8[8]}`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructType {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl StructType {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// `sl::BaseStructure`. 32 bytes: ptr(8) + GUID(16) + size_t(8).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BaseStructure {
    pub next: *mut c_void,
    pub struct_type: StructType,
    pub struct_version: usize,
}

impl BaseStructure {
    /// Build a base header for a struct with the given GUID + version, `next == null`.
    pub const fn new(struct_type: StructType, struct_version: usize) -> Self {
        Self {
            next: core::ptr::null_mut(),
            struct_type,
            struct_version,
        }
    }
}

// --- Struct version constants (sl_struct.h: kStructVersionN == N) -------------------------------
pub const K_STRUCT_VERSION_1: usize = 1;
pub const K_STRUCT_VERSION_2: usize = 2;
pub const K_STRUCT_VERSION_3: usize = 3;
pub const K_STRUCT_VERSION_4: usize = 4;
pub const K_STRUCT_VERSION_5: usize = 5;

// --- GUIDs, transcribed verbatim from the headers ----------------------------------------------
// Preferences   {1CA10965-BF8E-432B-8DA1-6716D879FB14}  (sl_core_types.h)
pub const GUID_PREFERENCES: StructType = StructType::new(
    0x1ca1_0965,
    0xbf8e,
    0x432b,
    [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14],
);
// Constants     {DCD35AD7-4E4A-4BAD-A90C-E0C49EB23AFE}  (sl_consts.h, kStructVersion2)
pub const GUID_CONSTANTS: StructType = StructType::new(
    0xdcd3_5ad7,
    0x4e4a,
    0x4bad,
    [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe],
);
// Resource      {3A9D70CF-2418-4B72-8391-13F8721C7261}  (sl_core_types.h, kStructVersion1)
pub const GUID_RESOURCE: StructType = StructType::new(
    0x3a9d_70cf,
    0x2418,
    0x4b72,
    [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61],
);
// ResourceTag   {4C6A5AAD-B445-496C-87FF-1AF3845BE653}  (sl_core_types.h, kStructVersion1)
pub const GUID_RESOURCE_TAG: StructType = StructType::new(
    0x4c6a_5aad,
    0xb445,
    0x496c,
    [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53],
);
// ViewportHandle {171B6435-9B3C-4FC8-9994-FBE52569AAA4} (sl_core_types.h, kStructVersion1)
pub const GUID_VIEWPORT_HANDLE: StructType = StructType::new(
    0x171b_6435,
    0x9b3c,
    0x4fc8,
    [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4],
);
// AdapterInfo   {0677315F-A746-4492-9F42-CB6142C9C3D4}  (sl_core_types.h, kStructVersion1)
pub const GUID_ADAPTER_INFO: StructType = StructType::new(
    0x0677_315f,
    0xa746,
    0x4492,
    [0x9f, 0x42, 0xcb, 0x61, 0x42, 0xc9, 0xc3, 0xd4],
);
// DLSSGOptions  {FAC5F1CB-2DFD-4F36-A1E6-3A9E865256C5}  (sl_dlss_g.h, kStructVersion5)
pub const GUID_DLSSG_OPTIONS: StructType = StructType::new(
    0xfac5_f1cb,
    0x2dfd,
    0x4f36,
    [0xa1, 0xe6, 0x3a, 0x9e, 0x86, 0x52, 0x56, 0xc5],
);
// DLSSGState    {CC8AC8E1-A179-44F5-97FA-E74112F9BC61}  (sl_dlss_g.h, kStructVersion4)
pub const GUID_DLSSG_STATE: StructType = StructType::new(
    0xcc8a_c8e1,
    0xa179,
    0x44f5,
    [0x97, 0xfa, 0xe7, 0x41, 0x12, 0xf9, 0xbc, 0x61],
);
// ReflexOptions {F03AF81A-6D0B-4902-A651-C4965E215434}  (sl_reflex.h, kStructVersion1)
pub const GUID_REFLEX_OPTIONS: StructType = StructType::new(
    0xf03a_f81a,
    0x6d0b,
    0x4902,
    [0xa6, 0x51, 0xc4, 0x96, 0x5e, 0x21, 0x54, 0x34],
);

// --- Enums --------------------------------------------------------------------------------------

/// `sl::Result` (sl_result.h) — `enum class` (4 bytes). `eOk == 0`.
///
/// Named `SlResult` (not `Result`) so it does not shadow `std::result::Result` when this module is
/// glob-imported.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlResult {
    Ok = 0,
    ErrorIO,
    ErrorDriverOutOfDate,
    ErrorOSOutOfDate,
    ErrorOSDisabledHWS,
    ErrorDeviceNotCreated,
    ErrorNoSupportedAdapterFound,
    ErrorAdapterNotSupported,
    ErrorNoPlugins,
    ErrorVulkanAPI,
    ErrorDXGIAPI,
    ErrorD3DAPI,
    ErrorNRDAPI,
    ErrorNVAPI,
    ErrorReflexAPI,
    ErrorNGXFailed,
    ErrorJSONParsing,
    ErrorMissingProxy,
    ErrorMissingResourceState,
    ErrorInvalidIntegration,
    ErrorMissingInputParameter,
    ErrorNotInitialized,
    ErrorComputeFailed,
    ErrorInitNotCalled,
    ErrorExceptionHandler,
    ErrorInvalidParameter,
    ErrorMissingConstants,
    ErrorDuplicatedConstants,
    ErrorMissingOrInvalidAPI,
    ErrorCommonConstantsMissing,
    ErrorUnsupportedInterface,
    ErrorFeatureMissing,
    ErrorFeatureNotSupported,
    ErrorFeatureMissingHooks,
    ErrorFeatureFailedToLoad,
    ErrorFeatureWrongPriority,
    ErrorFeatureMissingDependency,
    ErrorFeatureManagerInvalidState,
    ErrorInvalidState,
    WarnOutOfVRAM,
}

impl SlResult {
    pub fn is_ok(self) -> bool {
        self == SlResult::Ok
    }
}

/// `sl::Feature` is a `uint32_t` typedef.
pub type Feature = u32;
pub const K_FEATURE_DLSS: Feature = 0;
pub const K_FEATURE_NIS: Feature = 2;
pub const K_FEATURE_REFLEX: Feature = 3;
pub const K_FEATURE_PCL: Feature = 4;
pub const K_FEATURE_DLSS_G: Feature = 1000;

/// `sl::BufferType` is a `uint32_t` typedef.
pub type BufferType = u32;
pub const K_BUFFER_TYPE_DEPTH: BufferType = 0;
pub const K_BUFFER_TYPE_MOTION_VECTORS: BufferType = 1;
pub const K_BUFFER_TYPE_HUD_LESS_COLOR: BufferType = 2;
pub const K_BUFFER_TYPE_UI_COLOR_AND_ALPHA: BufferType = 23;
/// `sl::kBufferTypeBackbuffer` (sl_core_types.h: `constexpr BufferType kBufferTypeBackbuffer = 53;`).
///
/// Tags the swapchain back buffer; used for optional subrect tagging when the host renders to a
/// sub-region of the back buffer rather than the whole surface.
pub const K_BUFFER_TYPE_BACKBUFFER: BufferType = 53;
pub const K_BUFFER_TYPE_UI_ALPHA: BufferType = 68;

/// `sl::RenderAPI : uint32_t` (sl_device_wrappers.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum RenderAPI {
    D3D11 = 0,
    D3D12 = 1,
    Vulkan = 2,
}

/// `sl::EngineType : uint32_t` (sl_appidentity.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum EngineType {
    Custom = 0,
    Unreal = 1,
    Unity = 2,
}

/// `sl::LogLevel : uint32_t` (sl_core_types.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Off = 0,
    Default = 1,
    Verbose = 2,
}

/// `sl::PreferenceFlags : uint64_t` (sl_core_types.h).
pub mod preference_flags {
    pub const DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
    pub const DISABLE_DEBUG_TEXT: u64 = 1 << 1;
    pub const USE_MANUAL_HOOKING: u64 = 1 << 2;
    pub const ALLOW_OTA: u64 = 1 << 3;
    pub const BYPASS_OS_VERSION_CHECK: u64 = 1 << 4;
    pub const USE_DXGI_FACTORY_PROXY: u64 = 1 << 5;
    pub const LOAD_DOWNLOADED_PLUGINS: u64 = 1 << 6;
    pub const USE_FRAME_BASED_RESOURCE_TAGGING: u64 = 1 << 7;
}

/// `sl::ResourceType : char` (sl_core_types.h).
#[repr(i8)]
#[derive(Clone, Copy, Debug)]
pub enum ResourceType {
    Tex2d = 0,
    Buffer = 1,
    CommandQueue = 2,
    CommandBuffer = 3,
    CommandPool = 4,
    Fence = 5,
    Swapchain = 6,
    HostFence = 7,
    Unknown = 8,
}

/// `sl::ResourceLifecycle` (plain C enum, 4 bytes) (sl_core_types.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum ResourceLifecycle {
    OnlyValidNow = 0,
    ValidUntilPresent = 1,
    ValidUntilEvaluate = 2,
}

/// `sl::Boolean : char` (sl_consts.h).
#[repr(i8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boolean {
    False = 0,
    True = 1,
    Invalid = 2,
}

/// `sl::DLSSGMode : uint32_t` (sl_dlss_g.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DLSSGMode {
    Off = 0,
    On = 1,
    Auto = 2,
    Dynamic = 3,
}

/// `sl::DLSSGQueueParallelismMode : uint32_t` (sl_dlss_g.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum DLSSGQueueParallelismMode {
    BlockPresentingClientQueue = 0,
    BlockNoClientQueues = 1,
}

/// `sl::DLSSGStatus : uint32_t` (sl_dlss_g.h). Bit flags.
pub mod dlssg_status {
    pub const OK: u32 = 0;
    pub const FAIL_RESOLUTION_TOO_LOW: u32 = 1 << 0;
    pub const FAIL_REFLEX_NOT_DETECTED_AT_RUNTIME: u32 = 1 << 1;
    pub const FAIL_HDR_FORMAT_NOT_SUPPORTED: u32 = 1 << 2;
    pub const FAIL_COMMON_CONSTANTS_INVALID: u32 = 1 << 3;
    pub const FAIL_GET_CURRENT_BACK_BUFFER_INDEX_NOT_CALLED: u32 = 1 << 4;
    pub const RESERVED5: u32 = 1 << 5;

    /// Decode a status bitfield into a human-readable string.
    pub fn decode(status: u32) -> String {
        if status == OK {
            return "eOk".to_string();
        }
        let mut parts = Vec::new();
        if status & FAIL_RESOLUTION_TOO_LOW != 0 {
            parts.push("eFailResolutionTooLow");
        }
        if status & FAIL_REFLEX_NOT_DETECTED_AT_RUNTIME != 0 {
            parts.push("eFailReflexNotDetectedAtRuntime");
        }
        if status & FAIL_HDR_FORMAT_NOT_SUPPORTED != 0 {
            parts.push("eFailHDRFormatNotSupported");
        }
        if status & FAIL_COMMON_CONSTANTS_INVALID != 0 {
            parts.push("eFailCommonConstantsInvalid");
        }
        if status & FAIL_GET_CURRENT_BACK_BUFFER_INDEX_NOT_CALLED != 0 {
            parts.push("eFailGetCurrentBackBufferIndexNotCalled");
        }
        if status & RESERVED5 != 0 {
            parts.push("eReserved5");
        }
        if parts.is_empty() {
            return format!("<unknown status 0x{status:08x}>");
        }
        parts.join(" | ")
    }
}

/// `sl::ReflexMode` (plain C enum, 4 bytes) (sl_reflex.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflexMode {
    Off = 0,
    LowLatency = 1,
    LowLatencyWithBoost = 2,
}

/// `sl::PCLMarker : uint32_t` (sl_pcl.h).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PCLMarker {
    SimulationStart = 0,
    SimulationEnd = 1,
    RenderSubmitStart = 2,
    RenderSubmitEnd = 3,
    PresentStart = 4,
    PresentEnd = 5,
    TriggerFlash = 7,
    PCLatencyPing = 8,
}

// --- Vector/matrix helper types (sl_consts.h) ---------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Float2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Float3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Float4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

/// `sl::float4x4` — row-major, `float4 row[4]`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Float4x4 {
    pub row: [Float4; 4],
}

impl Float4x4 {
    pub const fn identity() -> Self {
        Self {
            row: [
                Float4 { x: 1.0, y: 0.0, z: 0.0, w: 0.0 },
                Float4 { x: 0.0, y: 1.0, z: 0.0, w: 0.0 },
                Float4 { x: 0.0, y: 0.0, z: 1.0, w: 0.0 },
                Float4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 },
            ],
        }
    }
}

/// `sl::Extent` — note the field order is `{ top, left, width, height }` (sl_consts.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Extent {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

// --- The chained input/output structs -----------------------------------------------------------

/// `sl::ViewportHandle` (sl_core_types.h, v1).
///
/// Header layout: BaseStructure + `uint32_t value` (private). We expose a constructor that fills
/// the base header. The trailing u32 leaves the struct at 40 bytes (32 + 4 + 4 pad).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ViewportHandle {
    pub base: BaseStructure,
    pub value: u32,
}

impl ViewportHandle {
    pub fn new(value: u32) -> Self {
        Self {
            base: BaseStructure::new(GUID_VIEWPORT_HANDLE, K_STRUCT_VERSION_1),
            value,
        }
    }
}

/// `sl::Preferences` (sl_core_types.h, v1).
#[repr(C)]
pub struct Preferences {
    pub base: BaseStructure,
    pub show_console: bool, // C++ `bool` == 1 byte
    pub log_level: LogLevel,
    pub paths_to_plugins: *const *const u16, // const wchar_t**
    pub num_paths_to_plugins: u32,
    pub path_to_logs_and_data: *const u16, // const wchar_t*
    pub allocate_callback: *mut c_void,
    pub release_callback: *mut c_void,
    pub log_message_callback: *mut c_void,
    pub flags: u64, // PreferenceFlags : uint64_t
    pub features_to_load: *const Feature,
    pub num_features_to_load: u32,
    pub application_id: u32,
    pub engine: EngineType,
    pub engine_version: *const u8, // const char*
    pub project_id: *const u8,     // const char*
    pub render_api: RenderAPI,
}

impl Preferences {
    pub fn new() -> Self {
        Self {
            base: BaseStructure::new(GUID_PREFERENCES, K_STRUCT_VERSION_1),
            show_console: false,
            log_level: LogLevel::Default,
            paths_to_plugins: core::ptr::null(),
            num_paths_to_plugins: 0,
            path_to_logs_and_data: core::ptr::null(),
            allocate_callback: core::ptr::null_mut(),
            release_callback: core::ptr::null_mut(),
            log_message_callback: core::ptr::null_mut(),
            flags: 0,
            features_to_load: core::ptr::null(),
            num_features_to_load: 0,
            application_id: 0,
            engine: EngineType::Custom,
            engine_version: core::ptr::null(),
            project_id: core::ptr::null(),
            render_api: RenderAPI::D3D12,
        }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self::new()
    }
}

/// `sl::AdapterInfo` (sl_core_types.h, v1).
#[repr(C)]
pub struct AdapterInfo {
    pub base: BaseStructure,
    pub device_luid: *mut u8,
    pub device_luid_size_in_bytes: u32,
    pub vk_physical_device: *mut c_void,
}

impl AdapterInfo {
    pub fn new() -> Self {
        Self {
            base: BaseStructure::new(GUID_ADAPTER_INFO, K_STRUCT_VERSION_1),
            device_luid: core::ptr::null_mut(),
            device_luid_size_in_bytes: 0,
            vk_physical_device: core::ptr::null_mut(),
        }
    }
}

impl Default for AdapterInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// `sl::Constants` (sl_consts.h, v2).
///
/// Field order copied directly from the header. Boolean fields are 1-byte `char`s; Rust packs them
/// the same way C++ does (the size const-assert checks `sizeof`).
#[repr(C)]
pub struct Constants {
    pub base: BaseStructure,
    pub camera_view_to_clip: Float4x4,
    pub clip_to_camera_view: Float4x4,
    pub clip_to_lens_clip: Float4x4,
    pub clip_to_prev_clip: Float4x4,
    pub prev_clip_to_clip: Float4x4,
    pub jitter_offset: Float2,
    pub mvec_scale: Float2,
    pub camera_pinhole_offset: Float2,
    pub camera_pos: Float3,
    pub camera_up: Float3,
    pub camera_right: Float3,
    pub camera_fwd: Float3,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov: f32,
    pub camera_aspect_ratio: f32,
    pub motion_vectors_invalid_value: f32,
    pub depth_inverted: Boolean,
    pub camera_motion_included: Boolean,
    pub motion_vectors_3d: Boolean,
    pub reset: Boolean,
    pub orthographic_projection: Boolean,
    pub motion_vectors_dilated: Boolean,
    pub motion_vectors_jittered: Boolean,
    // kStructVersion2:
    pub min_relative_linear_depth_object_separation: f32,
}

impl Constants {
    /// A reasonable jitter-free, identity-ish set of constants.
    pub fn new() -> Self {
        const INVALID_FLOAT: f32 = 3.402_823_5e38;
        Self {
            base: BaseStructure::new(GUID_CONSTANTS, K_STRUCT_VERSION_2),
            camera_view_to_clip: Float4x4::identity(),
            clip_to_camera_view: Float4x4::identity(),
            clip_to_lens_clip: Float4x4::identity(),
            clip_to_prev_clip: Float4x4::identity(),
            prev_clip_to_clip: Float4x4::identity(),
            jitter_offset: Float2 { x: 0.0, y: 0.0 },
            mvec_scale: Float2 { x: 1.0, y: 1.0 },
            camera_pinhole_offset: Float2 { x: 0.0, y: 0.0 },
            camera_pos: Float3 { x: 0.0, y: 0.0, z: 0.0 },
            camera_up: Float3 { x: 0.0, y: 1.0, z: 0.0 },
            camera_right: Float3 { x: 1.0, y: 0.0, z: 0.0 },
            camera_fwd: Float3 { x: 0.0, y: 0.0, z: 1.0 },
            camera_near: 0.1,
            camera_far: 10000.0,
            camera_fov: std::f32::consts::FRAC_PI_2,
            camera_aspect_ratio: 1920.0 / 1080.0,
            motion_vectors_invalid_value: INVALID_FLOAT,
            depth_inverted: Boolean::False,
            // Object/scene motion lives in the mvec buffer; no camera motion is baked in.
            camera_motion_included: Boolean::False,
            motion_vectors_3d: Boolean::False,
            reset: Boolean::False,
            orthographic_projection: Boolean::False,
            motion_vectors_dilated: Boolean::False,
            motion_vectors_jittered: Boolean::False,
            min_relative_linear_depth_object_separation: 40.0,
        }
    }
}

impl Default for Constants {
    fn default() -> Self {
        Self::new()
    }
}

/// `sl::Resource` (sl_core_types.h, v1).
#[repr(C)]
pub struct Resource {
    pub base: BaseStructure,
    pub resource_type: ResourceType, // char -> i8
    // NOTE: `native` follows `type` (a 1-byte char). The next member is a pointer, which is
    // 8-byte aligned, so the compiler inserts 7 bytes of padding after `type`. Both C++ and
    // Rust #[repr(C)] insert that same padding identically.
    pub native: *mut c_void,
    pub memory: *mut c_void,
    pub view: *mut c_void,
    pub state: u32, // D3D12_RESOURCE_STATES
    pub width: u32,
    pub height: u32,
    pub native_format: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub gpu_virtual_address: u64,
    pub flags: u32,
    pub usage: u32,
    pub reserved: u32,
}

impl Resource {
    /// Build a D3D12 tex2d resource tag payload.
    pub fn new_tex2d(
        native: *mut c_void,
        state: u32,
        width: u32,
        height: u32,
        native_format: u32,
    ) -> Self {
        Self {
            base: BaseStructure::new(GUID_RESOURCE, K_STRUCT_VERSION_1),
            resource_type: ResourceType::Tex2d,
            native,
            memory: core::ptr::null_mut(),
            view: core::ptr::null_mut(),
            state,
            width,
            height,
            native_format,
            mip_levels: 1,
            array_layers: 1,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            reserved: 0,
        }
    }
}

/// `sl::ResourceTag` (sl_core_types.h, v1).
#[repr(C)]
pub struct ResourceTag {
    pub base: BaseStructure,
    pub resource: *mut Resource,
    pub buffer_type: BufferType,
    pub lifecycle: ResourceLifecycle,
    pub extent: Extent,
}

impl ResourceTag {
    pub fn new(
        resource: *mut Resource,
        buffer_type: BufferType,
        lifecycle: ResourceLifecycle,
    ) -> Self {
        Self {
            base: BaseStructure::new(GUID_RESOURCE_TAG, K_STRUCT_VERSION_1),
            resource,
            buffer_type,
            lifecycle,
            // null extent == use the entire resource
            extent: Extent::default(),
        }
    }
}

/// `sl::DLSSGOptions` (sl_dlss_g.h, v5).
#[repr(C)]
pub struct DLSSGOptions {
    pub base: BaseStructure,
    pub mode: DLSSGMode,
    pub num_frames_to_generate: u32,
    pub flags: u32, // DLSSGFlags : uint32_t
    pub dynamic_res_width: u32,
    pub dynamic_res_height: u32,
    pub num_back_buffers: u32,
    pub mvec_depth_width: u32,
    pub mvec_depth_height: u32,
    pub color_width: u32,
    pub color_height: u32,
    pub color_buffer_format: u32,
    pub mvec_buffer_format: u32,
    pub depth_buffer_format: u32,
    pub hud_less_buffer_format: u32,
    pub ui_buffer_format: u32,
    pub on_error_callback: *mut c_void, // PFunOnAPIErrorCallback*
    // v2:
    pub b_reserved15: Boolean,
    // v3:
    pub queue_parallelism_mode: DLSSGQueueParallelismMode,
    // v4:
    pub enable_user_interface_recomposition: Boolean,
    // v5:
    pub dynamic_target_frame_rate: f32,
}

impl DLSSGOptions {
    pub fn new() -> Self {
        Self {
            base: BaseStructure::new(GUID_DLSSG_OPTIONS, K_STRUCT_VERSION_5),
            mode: DLSSGMode::Off,
            num_frames_to_generate: 1,
            flags: 0,
            dynamic_res_width: 0,
            dynamic_res_height: 0,
            num_back_buffers: 0,
            mvec_depth_width: 0,
            mvec_depth_height: 0,
            color_width: 0,
            color_height: 0,
            color_buffer_format: 0,
            mvec_buffer_format: 0,
            depth_buffer_format: 0,
            hud_less_buffer_format: 0,
            ui_buffer_format: 0,
            on_error_callback: core::ptr::null_mut(),
            b_reserved15: Boolean::Invalid,
            queue_parallelism_mode: DLSSGQueueParallelismMode::BlockPresentingClientQueue,
            enable_user_interface_recomposition: Boolean::False,
            dynamic_target_frame_rate: 0.0,
        }
    }
}

impl Default for DLSSGOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// `sl::DLSSGState` (sl_dlss_g.h, v4).
#[repr(C)]
pub struct DLSSGState {
    pub base: BaseStructure,
    pub estimated_vram_usage_in_bytes: u64,
    pub status: u32, // DLSSGStatus : uint32_t
    pub min_width_or_height: u32,
    pub num_frames_actually_presented: u32,
    // v2:
    pub num_frames_to_generate_max: u32,
    pub b_reserved4: Boolean,
    pub b_is_vsync_support_available: Boolean,
    // (6 bytes padding here before the next pointer; identical in C++ and Rust repr(C))
    pub inputs_processing_completion_fence: *mut c_void,
    pub last_present_inputs_processing_completion_fence_value: u64,
    // v4:
    pub b_is_dynamic_mfg_supported: Boolean,
}

impl DLSSGState {
    pub fn new() -> Self {
        Self {
            base: BaseStructure::new(GUID_DLSSG_STATE, K_STRUCT_VERSION_4),
            estimated_vram_usage_in_bytes: 0,
            status: 0,
            min_width_or_height: 0,
            num_frames_actually_presented: 0,
            num_frames_to_generate_max: 0,
            b_reserved4: Boolean::Invalid,
            b_is_vsync_support_available: Boolean::Invalid,
            inputs_processing_completion_fence: core::ptr::null_mut(),
            last_present_inputs_processing_completion_fence_value: 0,
            b_is_dynamic_mfg_supported: Boolean::Invalid,
        }
    }
}

impl Default for DLSSGState {
    fn default() -> Self {
        Self::new()
    }
}

/// `sl::ReflexOptions` (sl_reflex.h, v1).
#[repr(C)]
pub struct ReflexOptions {
    pub base: BaseStructure,
    pub mode: ReflexMode,
    pub frame_limit_us: u32,
    pub use_markers_to_optimize: bool, // C++ bool, 1 byte
    // u16 follows a bool: 1 byte pad to 2-byte align.
    pub virtual_key: u16,
    pub id_thread: u32,
}

impl ReflexOptions {
    pub fn new(mode: ReflexMode) -> Self {
        Self {
            base: BaseStructure::new(GUID_REFLEX_OPTIONS, K_STRUCT_VERSION_1),
            mode,
            frame_limit_us: 0,
            use_markers_to_optimize: false,
            virtual_key: 0,
            id_thread: 0,
        }
    }
}

/// Opaque `sl::FrameToken`. NEVER construct this in Rust — it is polymorphic (has a vtable). We
/// only ever hold the `*mut FrameToken` returned by `slGetNewFrameToken` and pass it by reference.
#[repr(C)]
pub struct FrameToken {
    _private: [u8; 0],
}

// --- Rust-side size + offset sanity assertions --------------------------------------------------
// These belt-and-suspenders checks catch obvious ABI mistakes at compile time. Sizes confirmed
// against MSVC sizeof() of the real sl:: structs.
const _: () = {
    assert!(core::mem::size_of::<StructType>() == 16);
    assert!(core::mem::size_of::<BaseStructure>() == 32);
    assert!(core::mem::size_of::<Float4x4>() == 64);
    assert!(core::mem::size_of::<Extent>() == 16);
    assert!(core::mem::size_of::<ViewportHandle>() == 40);
    assert!(core::mem::size_of::<AdapterInfo>() == 56);
    assert!(core::mem::size_of::<Resource>() == 112);
    assert!(core::mem::size_of::<ResourceTag>() == 64);
    assert!(core::mem::size_of::<Constants>() == 456);
    assert!(core::mem::size_of::<DLSSGOptions>() == 120);
    assert!(core::mem::size_of::<DLSSGState>() == 88);
    assert!(core::mem::size_of::<ReflexOptions>() == 48);
    // Preferences is the first and most ABI-sensitive struct passed to slInit (a 1-byte bool, several
    // u32-width enums, a u64 flags, and six raw pointers — exactly where a transcription slip
    // silently corrupts every later field). Pin its size like the rest.
    assert!(core::mem::size_of::<Preferences>() == 144);

    // Offset asserts on the highest-value fields. Sizes alone cannot catch a transposition of two
    // equally-sized fields or a padding miscalculation; these pin the exact byte offset (verified
    // against the MSVC C++ layout) of the fields most prone to silent reordering/padding bugs —
    // notably the pointer members that sit after a run of small/odd-sized fields.
    assert!(core::mem::offset_of!(Resource, native) == 40);
    assert!(core::mem::offset_of!(DLSSGOptions, on_error_callback) == 96);
    assert!(core::mem::offset_of!(DLSSGState, inputs_processing_completion_fence) == 64);
    assert!(core::mem::offset_of!(Constants, min_relative_linear_depth_object_separation) == 452);
    // Preferences: pin the scalar/pointer boundaries (the first pointer member after the small
    // header, the u64 flags, and the trailing render_api enum).
    assert!(core::mem::offset_of!(Preferences, paths_to_plugins) == 40);
    assert!(core::mem::offset_of!(Preferences, flags) == 88);
    assert!(core::mem::offset_of!(Preferences, render_api) == 136);
};

#[cfg(test)]
mod tests {
    use super::dlssg_status;

    // `dlssg_status::decode` turns the DLSS-G status bitfield into the operator-facing diagnostic
    // string — often the only signal for why FG silently isn't generating on hardware a maintainer
    // may not have. Guard its multi-bit join and unknown-bit fallback, not just the single-bit path.
    #[test]
    fn decode_ok_is_eok() {
        assert_eq!(dlssg_status::decode(dlssg_status::OK), "eOk");
    }

    #[test]
    fn decode_joins_multiple_bits() {
        let s = dlssg_status::decode(
            dlssg_status::FAIL_RESOLUTION_TOO_LOW | dlssg_status::FAIL_HDR_FORMAT_NOT_SUPPORTED,
        );
        assert!(s.contains("eFailResolutionTooLow"), "{s}");
        assert!(s.contains("eFailHDRFormatNotSupported"), "{s}");
        assert!(s.contains(" | "), "{s}");
    }

    #[test]
    fn decode_unknown_bit_is_unknown_form() {
        // A high/undocumented bit no token covers falls back to the hex "unknown" form.
        let s = dlssg_status::decode(1 << 20);
        assert!(s.starts_with("<unknown status"), "{s}");
    }
}
