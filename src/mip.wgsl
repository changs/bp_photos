// Mip-chain generation: each level is a bilinear 2x2 average of the previous one.

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(src_tex, samp, in.uv, 0.0);
}
