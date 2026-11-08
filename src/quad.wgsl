// A tiny renderer that draws solid axis-aligned rectangles in screen space.
// Used for cell backgrounds, the cursor block, and (later) selection.
//
// It exists because glyphon only draws glyphs; a terminal also needs colored
// rectangles behind them. Ghostty's renderer draws these in its Metal/OpenGL
// shaders, we do it with a dedicated instanced pipeline.

struct Uniforms {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct Instance {
    // x, y, width, height in physical pixels
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
};

struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let px = inst.rect.xy + c * inst.rect.zw;
    let ndc = vec2<f32>(
        px.x / u.screen.x * 2.0 - 1.0,
        1.0 - px.y / u.screen.y * 2.0,
    );
    var out: VOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    return in.color;
}
