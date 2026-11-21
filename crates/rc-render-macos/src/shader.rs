//! 呈现用的 Metal Shading Language 源码（纯常量，无 unsafe）。
//!
//! 着色器在 [`GpuContext`](crate::ffi::GpuContext) 构造期编译一次，
//! 不进呈现路径。
//!
//! # 职责边界（任务卡 T05「不要做什么」）
//! - **色彩空间转换在 GPU shader 里做**（NV12 → BGRA，BT.709），
//!   不在呈现路径上做 CPU 转换；
//! - **缩放用双线性**（`filter::linear`），不实现额外缩放算法；
//! - **光标混合（D7 的真实混合）在这里**：`vs_cursor` / `fs_cursor`
//!   在视频 quad 之后叠加光标矩形，alpha 混合（管线在 `ffi` 里配
//!   SourceAlpha / OneMinusSourceAlpha）。

/// 光标叠加的顶点参数（vertex bytes，`#[repr(C)]` 与 MSL 侧布局一致）。
///
/// - `rect`：NDC 矩形 `(x_left, y_bottom, x_right, y_top)`（y 向上）；
/// - `uv`：对应 uv `(u_left, v_bottom, u_right, v_top)`（v 向下，
///   已含越界裁剪后的子矩形，见 `rc_render::cursor::cursor_rect`）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CursorUniforms {
    /// NDC 矩形。
    pub rect: [f32; 4],
    /// uv 子矩形。
    pub uv: [f32; 4],
}

/// 全屏 quad + NV12/BGRA 采样 + 光标叠加的 MSL 源。
pub(crate) const MSL_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

// 全屏 quad 顶点输出：NDC 位置 + 纹理坐标。
struct QuadOut {
    float4 position [[position]];
    float2 uv;
};

// 光标叠加的顶点参数（vertex bytes，与 Rust 侧 CursorUniforms 布局一致）。
struct CursorUniforms {
    float4 rect; // NDC (x_left, y_bottom, x_right, y_top)
    float4 uv;   // (u_left, v_bottom, u_right, v_top)
};

// 由 vertex_id 展开的全屏 quad（无顶点缓冲，TriangleStrip 4 顶点）。
// NDC y 向上；纹理 v 向下（原点左上）→ v 翻转。
vertex QuadOut vs_quad(uint vid [[vertex_id]]) {
    const float2 quad[4] = {
        float2(-1.0, -1.0), float2(1.0, -1.0),
        float2(-1.0,  1.0), float2(1.0,  1.0),
    };
    QuadOut out;
    float2 p = quad[vid];
    out.position = float4(p, 0.0, 1.0);
    out.uv = float2(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

// D7 光标本地叠加：矩形由 vertex bytes 传入（位置在 CPU 侧用「当下」
// 光标缓存算出，不进视频管线），四角在 mix 中展开。
vertex QuadOut vs_cursor(uint vid [[vertex_id]],
                         constant CursorUniforms& u [[buffer(0)]]) {
    const float2 quad[4] = {
        float2(-1.0, -1.0), float2(1.0, -1.0),
        float2(-1.0,  1.0), float2(1.0,  1.0),
    };
    float2 p = quad[vid];
    float2 t = p * 0.5 + 0.5; // [0,1]²：t=(0,0) 左下角、(1,1) 右上角
    QuadOut out;
    out.position = float4(mix(u.rect.xy, u.rect.zw, t), 0.0, 1.0);
    out.uv = mix(u.uv.xy, u.uv.zw, t);
    return out;
}

// D7：光标 BGRA 直通 alpha 采样。混合系数在管线里配
// （SourceAlpha / OneMinusSourceAlpha），这里只出直通 alpha 颜色。
fragment float4 fs_cursor(QuadOut in [[stage_in]],
                          texture2d<float> cursor [[texture(0)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    return cursor.sample(s, in.uv);
}


// BGRA 直通（单平面，plane 0）。
fragment float4 fs_bgra(QuadOut in [[stage_in]],
                        texture2d<float> bgra [[texture(0)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    return bgra.sample(s, in.uv);
}

// BT.709 YCbCr → RGB。video range（'420v'）：Y∈[16,235] Cb/Cr∈[16,240]。
fragment float4 fs_nv12_video(QuadOut in [[stage_in]],
                              texture2d<float> luma   [[texture(0)]],
                              texture2d<float> chroma [[texture(1)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float y  = luma.sample(s, in.uv).r;
    float2 cc = chroma.sample(s, in.uv).rg;
    float yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float cb = (cc.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (cc.y - 128.0 / 255.0) * (255.0 / 224.0);
    float r = yy + 1.5748 * cr;
    float g = yy - 0.1873 * cb - 0.4681 * cr;
    float b = yy + 1.8556 * cb;
    return float4(r, g, b, 1.0);
}

// BT.709 full range（'420f'）：只减偏置，不缩放。
fragment float4 fs_nv12_full(QuadOut in [[stage_in]],
                             texture2d<float> luma   [[texture(0)]],
                             texture2d<float> chroma [[texture(1)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float y  = luma.sample(s, in.uv).r;
    float2 cc = chroma.sample(s, in.uv).rg;
    float yy = y;
    float cb = cc.x - 128.0 / 255.0;
    float cr = cc.y - 128.0 / 255.0;
    float r = yy + 1.5748 * cr;
    float g = yy - 0.1873 * cb - 0.4681 * cr;
    float b = yy + 1.8556 * cb;
    return float4(r, g, b, 1.0);
}
"#;
