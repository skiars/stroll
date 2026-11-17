struct Params {
    bounds: vec4f,
    corners: vec4f,
    origin: vec2f,
    extent: vec2f,
    filtered_extent: vec2f,
    direction: vec2f,
    mode: u32,
    scale: u32,
    pairs: u32,
    center: f32,
    opacity: f32,
    saturation: f32,
    sample_origin: vec2f,
    taps: array<vec4f, 16>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var original: texture_2d<f32>;
@group(0) @binding(2) var linear_sampler: sampler;
@group(0) @binding(3) var<uniform> params: Params;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let points = array<vec2f, 3>(vec2f(-1., -1.), vec2f(3., -1.), vec2f(-1., 3.));
    return vec4f(points[index], 0., 1.);
}

@fragment
fn fragment(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let size = vec2f(textureDimensions(source));
    if params.mode == 0u {
        // One bilinear fetch averages a 2x2 block. Average the full footprint
        // before decimation so small text cannot alias into the blur.
        let start = floor(position.xy) * f32(params.scale);
        let pairs = params.scale / 2u;
        var result = vec4f(0.);
        for (var y = 0u; y < pairs; y++) {
            for (var x = 0u; x < pairs; x++) {
                let pixel = min(start + vec2f(vec2u(2u*x + 1u, 2u*y + 1u)), params.extent - 0.5);
                result += textureSampleLevel(source, linear_sampler, (pixel + params.sample_origin) / size, 0.);
            }
        }
        return result / f32(pairs * pairs);
    }
    let min_uv = vec2f(0.5) / size;
    let max_uv = (params.filtered_extent - vec2f(0.5)) / size;
    if params.mode == 1u {
        let uv = position.xy / size;
        var result = params.center * textureSampleLevel(source, linear_sampler, uv, 0.);
        for (var i = 0u; i < params.pairs; i++) {
            let tap = params.taps[i];
            let offset = params.direction * tap.x / size;
            result += tap.y * (
                textureSampleLevel(source, linear_sampler, clamp(uv + offset, min_uv, max_uv), 0.) +
                textureSampleLevel(source, linear_sampler, clamp(uv - offset, min_uv, max_uv), 0.)
            );
        }
        return result;
    }

    let direct = params.mode == 3u;
    let original_position = select(position.xy - params.origin, position.xy, direct);
    let unfiltered = textureLoad(original, vec2i(original_position), 0);
    if direct && (any(position.xy < params.extent) || any(position.xy >= params.direction)) {
        return unfiltered;
    }
    let local = position.xy - params.origin;
    let uv = clamp(local / (f32(params.scale) * size), min_uv, max_uv);
    let blurred = textureSampleLevel(source, linear_sampler, uv, 0.);
    let half_size = (params.bounds.zw - params.bounds.xy) * 0.5;
    let point = position.xy - (params.bounds.xy + half_size);
    let top = select(params.corners.x, params.corners.y, point.x >= 0.);
    let bottom = select(params.corners.w, params.corners.z, point.x >= 0.);
    let radius = select(top, bottom, point.y >= 0.);
    let edge = abs(point) - half_size + vec2f(radius);
    let distance = length(max(edge, vec2f(0.))) + min(max(edge.x, edge.y), 0.) - radius;
    let coverage = clamp(0.5 - distance, 0., 1.);
    let luma = dot(blurred.rgb, vec3f(0.213, 0.715, 0.072));
    let color = mix(vec3f(luma), blurred.rgb, params.saturation);
    return mix(unfiltered, vec4f(clamp(color, vec3f(0.), vec3f(1.)), blurred.a), coverage * params.opacity);
}
