// Draw one frame into an arbitrary convex quadrilateral.
//
// The whole trick is in the fragment stage: instead of moving vertices to the
// quad's corners and hoping the texture coordinates interpolate sensibly, this
// covers the entire target with one triangle and asks, for each pixel, "which
// texel of the picture belongs here?" — by pushing the pixel back through the
// quad's inverse homography.
//
// The divide by w below is the perspective correction. Doing it per pixel is
// what a two-triangle mesh cannot do, and its absence is the crease that shows
// up down the diagonal of every keystoned projection.
//
// A clip's look rides in the same pass: a 2×3 transform of the texture
// coordinates (crop, zoom, turn, flip), a chroma key on the alpha, and a 4×4
// colour matrix with a gamma. Each costs a few multiplies per pixel, and each
// is the identity for a clip with no look.

struct Uniforms {
    // Rows of the inverse homography: target pixels -> the unit square.
    inverse0: vec4<f32>,
    inverse1: vec4<f32>,
    inverse2: vec4<f32>,
    // Rows of the YUV -> RGB matrix. The w component is the offset, which
    // carries both the limited-range pedestal and chroma's neutral point.
    color0: vec4<f32>,
    color1: vec4<f32>,
    color2: vec4<f32>,
    target_size: vec2<f32>,
    opacity: f32,
    mode: u32,
    // Rows of the 2×3 map from the unit square to texture coordinates. The
    // translation is in w.
    transform: array<vec4<f32>, 2>,
    // Rows of the colour matrix, applied to (r, g, b, 1).
    grade: array<vec4<f32>, 4>,
    // Chroma of the key colour (Cb, Cr), tolerance, softness.
    key: vec4<f32>,
    // Exponent for the picture after the colour matrix: 1 / gamma.
    gamma: f32,
    keyed: u32,
    look_pad: vec2<f32>,
}

const MODE_RGBA: u32 = 0u;
const MODE_PLANAR: u32 = 1u;
const MODE_BIPLANAR: u32 = 2u;

@group(0) @binding(0) var<uniform> settings: Uniforms;
@group(0) @binding(1) var plane0: texture_2d<f32>;
@group(0) @binding(2) var plane1: texture_2d<f32>;
@group(0) @binding(3) var plane2: texture_2d<f32>;
@group(0) @binding(4) var texture_sampler: sampler;

// One oversized triangle rather than two triangles making a quad: fewer
// vertices, no shared edge, and no vertex buffer to bind.
@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(index) / 2) * 4.0 - 1.0;
    let y = f32(i32(index) & 1) * 4.0 - 1.0;

    return vec4<f32>(x, y, 0.0, 1.0);
}

// Transparent where the pixel is close to the key colour, opaque where it is
// not. Chroma (Cb, Cr) rather than RGB, so a shadow on a green screen still keys.
fn chroma_key(rgb: vec3<f32>) -> f32 {
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let chroma = vec2<f32>((rgb.b - luma) / 1.8556, (rgb.r - luma) / 1.5748);
    let distance = length(chroma - settings.key.xy);
    return smoothstep(settings.key.z, settings.key.z + max(settings.key.w, 0.0001), distance);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // The pixel's centre, in the same coordinates the quad was given in.
    let pixel = vec3<f32>(position.x, position.y, 1.0);

    let mapped = vec3<f32>(
        dot(settings.inverse0.xyz, pixel),
        dot(settings.inverse1.xyz, pixel),
        dot(settings.inverse2.xyz, pixel),
    );

    // Behind the horizon of the projective map: there is no texel there.
    let visible_w = step(0.00001, mapped.z);
    let quad_coordinates = mapped.xy / max(mapped.z, 0.00001);

    // The look's transform: crop, zoom, turn and flip, from the quad's unit
    // square to the picture's own texture coordinates.
    let quad_point = vec3<f32>(quad_coordinates, 1.0);
    let coordinates = vec2<f32>(
        dot(settings.transform[0].xyz, quad_point),
        dot(settings.transform[1].xyz, quad_point),
    );

    // Outside the picture is transparent rather than clamped, so a quad smaller
    // than the target leaves the rest of the surface alone, and a crop or zoom
    // never shows the picture's edge repeated. Computed as a factor rather than
    // a `discard` so that sampling stays in uniform control flow — a discarded
    // fragment must not change what its neighbours may do.
    let inside = step(0.0, coordinates.x)
        * step(coordinates.x, 1.0)
        * step(0.0, coordinates.y)
        * step(coordinates.y, 1.0)
        * visible_w;

    var color = vec3<f32>(0.0);
    var alpha = 1.0;

    if settings.mode == MODE_RGBA {
        let texel = textureSample(plane0, texture_sampler, coordinates);
        color = texel.rgb;
        alpha = texel.a;
    } else {
        var yuv: vec3<f32>;

        if settings.mode == MODE_PLANAR {
            yuv = vec3<f32>(
                textureSample(plane0, texture_sampler, coordinates).r,
                textureSample(plane1, texture_sampler, coordinates).r,
                textureSample(plane2, texture_sampler, coordinates).r,
            );
        } else {
            // NV12 and friends: chroma is interleaved in one two-channel plane.
            let chroma = textureSample(plane1, texture_sampler, coordinates).rg;
            yuv = vec3<f32>(
                textureSample(plane0, texture_sampler, coordinates).r,
                chroma.r,
                chroma.g,
            );
        }

        let sample = vec4<f32>(yuv, 1.0);
        color = vec3<f32>(
            dot(settings.color0, sample),
            dot(settings.color1, sample),
            dot(settings.color2, sample),
        );
    }

    // The key tests the picture as it came in, before any colour control.
    var key_alpha = 1.0;
    if settings.keyed == 1u {
        key_alpha = chroma_key(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)));
    }

    // Colour controls: one matrix with its offset, then gamma. The matrix can
    // push a channel below zero, which a power cannot take, so it is floored first.
    let lit = vec4<f32>(color, 1.0);
    var graded = vec3<f32>(
        dot(settings.grade[0], lit),
        dot(settings.grade[1], lit),
        dot(settings.grade[2], lit),
    );
    graded = pow(max(graded, vec3<f32>(0.0)), vec3<f32>(settings.gamma));

    let final_alpha = alpha * settings.opacity * inside * key_alpha;

    // Premultiplied out, to match the blend state the pipeline is built with.
    return vec4<f32>(clamp(graded, vec3<f32>(0.0), vec3<f32>(1.0)) * final_alpha, final_alpha);
}
