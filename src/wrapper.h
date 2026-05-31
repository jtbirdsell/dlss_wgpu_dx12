// bindgen entry point. The NGX D3D12 helpers (NGX_D3D12_CREATE_DLSS_EXT,
// NGX_D3D12_EVALUATE_DLSS_EXT, NGX_DLSS_GET_OPTIMAL_SETTINGS, NGX_DLSS_GET_STATS) and the
// shared structs live in nvsdk_ngx_helpers.h. Ray Reconstruction adds nvsdk_ngx_helpers_dlssd.h
// (NGX_D3D12_CREATE_DLSSD_EXT / NGX_D3D12_EVALUATE_DLSSD_EXT, NVSDK_NGX_DLSSD_*).
//
// Deliberately NOT included: <d3d12.h>, <windows.h>, <vulkan/vulkan.h>. The NGX headers
// forward-declare the COM interfaces as opaque structs, so libclang only needs $DLSS_SDK/include.
#include "nvsdk_ngx_helpers.h"
#include "nvsdk_ngx_helpers_dlssd.h"
