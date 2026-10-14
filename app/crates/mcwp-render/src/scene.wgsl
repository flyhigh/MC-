// MC 壁纸 · 参数化场景着色器
//
// 4 个预设共用一套 uniform,`P.scene` 选分支;具体观感由 density / softness / accent 等槽位驱动。
// 参数含义与取值区间见 `mcwp-spec/src/presets.rs`,Rust 侧布局见 `SceneParams` —— 两边必须逐字段对齐。

struct Params {
    resolution: vec2<f32>,        // 0
    time: f32,                    // 8
    brightness: f32,              // 12
    palette: array<vec4<f32>, 5>, // 16
    motion: f32,                  // 96
    intensity: f32,               // 100
    speed: f32,                   // 104
    grain: f32,                   // 108
    density: f32,                 // 112
    softness: f32,                // 116
    light_key: f32,               // 120
    accent: f32,                  // 124
    vignette: f32,                // 128
    seed: f32,                    // 132
    scene: u32,                   // 136
    _pad: f32,                    // 140
};

@group(0) @binding(0) var<uniform> P: Params;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // 一个覆盖全屏的大三角形,不需要顶点缓冲
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = corners[index];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

// ---------- 公共工具 ----------

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 345.45));
    q = q + dot(q, q + 34.345);
    return fract(q.x * q.y);
}

/// 由 seed 派生的采样偏移:同一份 spec 永远得到同一张画面。
fn seed_offset() -> vec2<f32> {
    let s = fract(P.seed * 0.0000131) * 137.0;
    return vec2<f32>(s, fract(P.seed * 0.0000271) * 219.0);
}

fn noise(p: vec2<f32>, offset: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i + offset);
    let b = hash21(i + vec2<f32>(1.0, 0.0) + offset);
    let c = hash21(i + vec2<f32>(0.0, 1.0) + offset);
    let d = hash21(i + vec2<f32>(1.0, 1.0) + offset);
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p: vec2<f32>, offset: vec2<f32>) -> f32 {
    var value = 0.0;
    var amplitude = 0.5;
    var q = p;
    for (var i = 0; i < 4; i = i + 1) {
        value = value + amplitude * noise(q, offset);
        q = q * 2.03;
        amplitude = amplitude * 0.5;
    }
    return value;
}

/// 柔和光斑:中心亮、边缘衰减
fn blob(p: vec2<f32>, center: vec2<f32>, radius: f32) -> f32 {
    return smoothstep(radius, 0.0, length(p - center));
}

fn aspect_uv(uv: vec2<f32>, aspect: f32) -> vec2<f32> {
    return vec2<f32>((uv.x - 0.5) * aspect + 0.5, uv.y);
}

// ---------- 0 · 雨夜城市 ----------

fn scene_rainy(uv: vec2<f32>, aspect: f32, t: f32) -> vec3<f32> {
    let p = aspect_uv(uv, aspect);

    // 1. 垂直渐变基底
    var color = mix(P.palette[0].rgb, P.palette[1].rgb, smoothstep(0.0, 1.0, uv.y));

    // 2. 缓慢横向漂移的色带,雨夜雾面感
    let band = 0.5 + 0.5 * sin(uv.x * 3.1 + t * 1.7 + sin(uv.y * 2.3 - t * 0.9));
    color = mix(color, P.palette[2].rgb, 0.30 * band * smoothstep(0.15, 1.0, uv.y));

    // 3. 两团缓慢移动的城市灯光,视差参数拉开两者的速度差
    let parallax = P.accent * 0.6;
    let light_a = vec2<f32>(0.35 + (0.22 + parallax) * sin(t * 1.3), 0.34 + 0.10 * cos(t * 0.9));
    let light_b = vec2<f32>(0.74 + (0.10 + parallax * 0.5) * cos(t * 1.1), 0.72 + 0.09 * sin(t * 0.7));
    color = color + P.palette[3].rgb * (0.30 + 0.45 * P.light_key) * blob(p, light_a, 0.55);
    color = color + P.palette[4].rgb * (0.18 + 0.30 * P.light_key) * blob(p, light_b, 0.45);

    // 4. 雨丝:斜向细纹,密度跟着 rain 参数走
    if (P.density > 0.01) {
        let drop = fract(uv.y * (55.0 + 130.0 * P.density) + uv.x * 3.0 + t * 2.4);
        let streak = smoothstep(0.93, 1.0, drop) * P.density;
        color = color + P.palette[4].rgb * streak * 0.18;
    }

    // 5. 雾面柔化:一层低频噪声压低对比
    let haze = fbm(p * 2.4 + vec2<f32>(t * 0.05, 0.0), seed_offset());
    color = mix(color, mix(color, P.palette[1].rgb, 0.55), P.softness * haze);
    return color;
}

// ---------- 1 · 极简纸感 ----------

fn scene_paper(uv: vec2<f32>, t: f32) -> vec3<f32> {
    // 暖度 = palette[0] 与 palette[2] 的混合比例
    let warm = mix(P.palette[0].rgb, P.palette[2].rgb, P.density * 0.55);
    var color = mix(warm, P.palette[1].rgb, smoothstep(0.0, 1.0, uv.y) * 0.45);

    // 左上受光,右下略微下沉 —— 给平面一点体积
    let d = length((uv - vec2<f32>(0.30, 0.24)) * vec2<f32>(1.0, 1.5));
    color = color * (1.0 - 0.18 * smoothstep(0.15, 1.05, d));

    // 纸纤维:低频斑驳 + 细颗粒
    let fiber = fbm(uv * 3.4 + vec2<f32>(t * 0.010, 0.0), seed_offset());
    color = color * (1.0 - 0.18 * P.grain * (fiber - 0.5) * 2.0);

    // 柔度:把对比再压一点,越软越"糊"
    let mean = (color.r + color.g + color.b) / 3.0;
    color = mix(color, vec3<f32>(mean), P.softness * 0.22);
    return color;
}

// ---------- 2 · 星云漂流 ----------

fn scene_nebula(uv: vec2<f32>, aspect: f32, t: f32) -> vec3<f32> {
    let p = aspect_uv(uv, aspect);
    let offset = seed_offset();

    var color = mix(P.palette[0].rgb, P.palette[1].rgb, smoothstep(0.0, 1.0, uv.y) * 0.85);

    // 两层反向流动的 fbm 作为「形变场」,再采样主轴密度
    let warp_x = fbm(p * 1.7 + vec2<f32>(t * 0.030, 0.0), offset);
    let warp_y = fbm(p * 1.7 + vec2<f32>(0.0, t * 0.021), offset + vec2<f32>(7.3, 2.1));
    let flow = p + (vec2<f32>(warp_x, warp_y) - vec2<f32>(0.5)) * (0.55 + 1.15 * P.accent);

    let cloud = fbm(flow * 2.2 + vec2<f32>(t * 0.020, t * 0.012), offset);
    let detail = fbm(flow * 4.8 - vec2<f32>(t * 0.026, 0.0), offset + vec2<f32>(3.7, 9.1));

    let mass = smoothstep(0.34, 0.86, cloud * (0.55 + 0.45 * detail)) * (0.35 + 0.65 * P.density);
    color = mix(color, P.palette[2].rgb, mass * 0.85);
    color = mix(color, P.palette[3].rgb, mass * 0.55);

    // 亮核:密度最高处透出对比色
    let core = pow(smoothstep(0.55, 1.0, detail), 2.0) * P.softness;
    color = color + P.palette[4].rgb * core * (0.35 + 0.65 * P.light_key);

    // 星点:按像素哈希,越暗处越明显
    let star = step(0.9985, hash21(floor(uv * P.resolution) + offset));
    color = color + vec3<f32>(star) * 0.55;
    return color;
}

// ---------- 3 · 专注网格 ----------

fn scene_grid(uv: vec2<f32>, aspect: f32, t: f32) -> vec3<f32> {
    var color = mix(P.palette[0].rgb, P.palette[1].rgb, smoothstep(1.0, 0.0, uv.y) * 0.8);

    // 细网格:48px 一格,线宽固定在 1px 量级(用分辨率算,任何屏幕都细腻)
    let cell = 48.0;
    let f = fract(uv * P.resolution / cell);
    let edge = min(0.5 - abs(f.x - 0.5), 0.5 - abs(f.y - 0.5));
    let width = 1.2 / cell;
    let line = (1.0 - smoothstep(width, width * 2.6, edge)) * P.softness;
    color = color + P.palette[2].rgb * line * 0.30;

    // 扫描线:一条缓慢上下移动的亮带
    let scan_y = fract(t * 0.05 + 0.5);
    let scan = smoothstep(0.14, 0.0, abs(uv.y - scan_y)) * P.accent;
    color = color + P.palette[3].rgb * scan * 0.28;

    // 中心柔光,避免整屏死黑
    let glow = smoothstep(0.80, 0.0, length(aspect_uv(uv, aspect) - vec2<f32>(0.5)));
    color = color + P.palette[2].rgb * glow * 0.14 * P.density;
    return color;
}

// ---------- 主入口 ----------

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let aspect = P.resolution.x / max(P.resolution.y, 1.0);
    let t = P.time * (0.05 + 0.55 * P.motion * P.speed);

    var color = scene_grid(uv, aspect, t);
    if (P.scene == 0u) {
        color = scene_rainy(uv, aspect, t);
    } else if (P.scene == 1u) {
        color = scene_paper(uv, t);
    } else if (P.scene == 2u) {
        color = scene_nebula(uv, aspect, t);
    }

    // 细微颗粒,抑制渐变色带
    let grain = hash21(uv * P.resolution + vec2<f32>(t * 60.0, t * 30.0)) - 0.5;
    color = color + vec3<f32>(grain) * (0.010 + 0.05 * P.grain);

    // 暗角
    let vignette = smoothstep(1.25, 0.25, length(aspect_uv(uv, aspect) - vec2<f32>(0.5)) * 1.4);
    color = color * mix(1.0, vignette, P.vignette);

    // 亮度跟随桌面环境
    color = color * (0.35 + 1.35 * P.brightness * (0.6 + 0.4 * P.intensity));

    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}