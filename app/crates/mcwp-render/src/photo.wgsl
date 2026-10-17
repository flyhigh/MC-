// MC 壁纸 · 照片壁纸着色器
//
// 输入是「一张已经生好的图」+「一套动效配方」,输出是一帧画面。和 `scene.wgsl`(纯程序化场景)
// 是两条互不牵连的路:各有一套 uniform 和 bind group,共用同一个全屏三角形。
//
// 三件事按顺序发生,顺序本身就是设计:
//   1. **构图**:把图按 cover 贴满屏,叠上 Ken Burns 推拉(E0,所有壁纸都有);
//   2. **采样**:拿上一步算出的 uv 去读纹理 —— 水波(E6)在这一步之前扰动 uv;
//   3. **叠层**:雪 / 雨 / 雾 / 云影 / 光晕在**已采样的颜色之上**叠加,最后统一做调色、颗粒、暗角。
// 分开的原因是"改像素"和"改坐标"不能混:雪是往颜色上加东西,水波是把采样点挪走。

struct Params {
    resolution: vec2<f32>,        // 0
    time: f32,                    // 8
    brightness: f32,              // 12
    palette: array<vec4<f32>, 5>, // 16
    zoom: f32,                    // 96  Ken Burns 推近幅度
    pan: f32,                     // 100 平移幅度
    speed: f32,                   // 104
    intensity: f32,               // 108 效果层强度
    effect: f32,                  // 112 动效编号(= MotionKind 的下标,0 = still)
    image_aspect: f32,            // 116
    screen_aspect: f32,           // 120
    seed: f32,                    // 124
    vignette: f32,                // 128
    grain: f32,                   // 132
    contrast: f32,                // 136
    warmth: f32,                  // 140
    clock_style: f32,             // 144
    clock_opacity: f32,           // 148
    clock_time: vec2<f32>,        // 152
    clock_pos: vec2<f32>,         // 160
    _pad: vec2<f32>,              // 168
};

@group(0) @binding(0) var<uniform> P: Params;
@group(1) @binding(0) var photo: texture_2d<f32>;
@group(1) @binding(1) var photo_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // 覆盖全屏的大三角形,和 scene.wgsl 一致:不需要顶点缓冲,也就没有顶点格式要对齐
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = corners[index];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    // uv.y 翻过来:0 在屏幕顶端。纹理也是(0,0) 左上角,两边一致才不用再翻一次
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

// ---------- 噪声工具(和 scene.wgsl 各留一份:两个模块要能独立编译,共享一个 wgsl 反而要
// 引入 naga 之外的 include 机制,不值当) ----------

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 345.45));
    q = q + dot(q, q + 34.345);
    return fract(q.x * q.y);
}

/// 由 seed 派生的采样偏移:同一张图,效果层的花纹每次运行都落在同一处。
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

// ---------- 构图 ----------

/// 把「图」按 cover 贴进「屏」:比例不同就裁图的两头,而不是留黑边。
///
/// 壁纸留黑边比裁掉一点边难看得多;而且裁掉的那点边,本来就是生图模型自由发挥的地方,
/// 不是构图重点。返回的是"图上的 uv",所以是**乘小于 1 的系数**把范围收窄。
fn cover_uv(uv: vec2<f32>, image_aspect: f32, screen_aspect: f32) -> vec2<f32> {
    let shrink = min(screen_aspect / max(image_aspect, 0.01), image_aspect / max(screen_aspect, 0.01));
    // 图比屏宽 → 压 x(裁左右);屏比图宽 → 压 y(裁上下)
    let scale = select(
        vec2<f32>(1.0, shrink),
        vec2<f32>(shrink, 1.0),
        image_aspect > screen_aspect,
    );
    return (uv - vec2<f32>(0.5)) * scale + vec2<f32>(0.5);
}

// ---------- 效果层 ----------

/// 雪粒:三层不同尺度 / 速度的点阵往下飘,每粒按格子哈希给不同的半径与亮度。
fn snow(uv: vec2<f32>, t: f32, density: f32, offset: vec2<f32>) -> f32 {
    var acc = 0.0;
    for (var i = 0; i < 3; i = i + 1) {
        let layer = f32(i);
        let cells = 22.0 + 30.0 * layer;
        let sway = vec2<f32>(sin(t * (0.31 + 0.11 * layer) + layer) * 0.5, 0.0);
        let p = uv * vec2<f32>(cells, cells * 0.62)
            + vec2<f32>(0.0, -t * (0.5 + 0.32 * layer))
            + sway
            + offset;
        let cell = floor(p);
        let jitter = vec2<f32>(hash21(cell + offset), hash21(cell + offset + 7.3)) - vec2<f32>(0.5);
        let local = fract(p) - vec2<f32>(0.5) - jitter * 0.7;
        let radius = (0.10 + 0.045 * layer) * (0.55 + 0.7 * hash21(cell + offset + 3.1));
        let brightness = 0.35 + 0.65 * hash21(cell + offset + 11.7);
        acc = acc + smoothstep(radius, 0.0, length(local)) * brightness;
    }
    return acc * density;
}

/// 雨丝:两层不同粗细的斜线往下走,每条线的相位由列哈希决定。
fn rain(uv: vec2<f32>, t: f32, density: f32, seed: f32) -> f32 {
    var acc = 0.0;
    for (var i = 0; i < 2; i = i + 1) {
        let layer = f32(i);
        let columns = 110.0 + 90.0 * layer;
        // 斜一点:整条雨幕带一个固定倾角,比纯垂直的雨更有"风"的感觉
        let skewed = uv.x * columns + uv.y * 22.0;
        let phase = hash21(vec2<f32>(floor(skewed), layer + seed)) * 4.0;
        let fall = fract(uv.y * (2.0 + 3.0 * layer) - t * (1.7 + 1.1 * layer) + phase);
        let thin = 1.0 - smoothstep(0.0, 0.05, abs(fract(skewed) - 0.5));
        acc = acc + smoothstep(0.72, 1.0, fall) * thin;
    }
    return acc * density;
}

/// 雾气:两层反向漂移的 fbm 叠出缓慢翻滚的浓度场。
fn fog(uv: vec2<f32>, t: f32, density: f32, offset: vec2<f32>) -> f32 {
    let drift = vec2<f32>(t * 0.035, t * 0.012);
    let low = fbm(uv * 3.2 + drift, offset);
    let high = fbm(uv * 7.4 - drift * 1.7, offset + vec2<f32>(4.1, 2.7));
    return smoothstep(0.32, 0.95, low * 0.72 + high * 0.28) * density;
}

/// 云影:大块的明暗横扫,压出"天上正有云飘过"的体感。
fn cloud(uv: vec2<f32>, t: f32, density: f32, offset: vec2<f32>) -> f32 {
    let shape = fbm(uv * vec2<f32>(1.5, 3.2) + vec2<f32>(t * 0.05, 0.0), offset);
    return smoothstep(0.34, 0.92, shape) * density;
}

/// 光晕:两团柔光随时间呼吸明灭。位置由 seed 定,所以同一张图每次都落在同一处。
fn glow(uv: vec2<f32>, t: f32, density: f32, offset: vec2<f32>) -> f32 {
    var acc = 0.0;
    for (var i = 0; i < 2; i = i + 1) {
        let layer = f32(i);
        let center = vec2<f32>(
            0.20 + 0.58 * hash21(offset + layer * 3.7),
            0.16 + 0.52 * hash21(offset + layer * 5.3 + 1.9),
        );
        let radius = 0.26 + 0.20 * hash21(offset + layer * 7.1 + 2.3);
        let pulse = 0.55 + 0.45 * sin(t * 0.42 + layer * 2.1);
        acc = acc + blob(uv, center + vec2<f32>(0.02 * sin(t * 0.2 + layer), 0.0), radius) * pulse;
    }
    return acc * density;
}

// ---------- 时钟 ----------

/// 圆角矩形的有符号距离：负数在内部。
fn rounded_box_sdf(p: vec2<f32>, center: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p - center) - half_size + vec2<f32>(radius);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

fn rounded_box_mask(p: vec2<f32>, center: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    return 1.0 - smoothstep(-0.012, 0.012, rounded_box_sdf(p, center, half_size, radius));
}

/// 七段数字。`p` 是数字框内的归一化坐标（0..1，左上角）。
fn clock_digit_mask(digit: f32, p: vec2<f32>) -> f32 {
    if (p.x < -0.08 || p.x > 1.08 || p.y < -0.08 || p.y > 1.08) {
        return 0.0;
    }
    // 位序：a=1 b=2 c=4 d=8 e=16 f=32 g=64
    var masks = array<u32, 10>(63u, 6u, 91u, 79u, 102u, 109u, 125u, 7u, 127u, 111u);
    let bits = masks[u32(clamp(digit, 0.0, 9.0) + 0.5)];
    let horizontal = vec2<f32>(0.25, 0.042);
    let vertical = vec2<f32>(0.042, 0.17);
    var value = 0.0;
    if ((bits & 1u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.5, 0.07), horizontal, 0.035));
    }
    if ((bits & 2u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.93, 0.27), vertical, 0.035));
    }
    if ((bits & 4u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.93, 0.73), vertical, 0.035));
    }
    if ((bits & 8u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.5, 0.93), horizontal, 0.035));
    }
    if ((bits & 16u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.07, 0.73), vertical, 0.035));
    }
    if ((bits & 32u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.07, 0.27), vertical, 0.035));
    }
    if ((bits & 64u) != 0u) {
        value = max(value, rounded_box_mask(p, vec2<f32>(0.5, 0.5), horizontal, 0.035));
    }
    return value;
}

struct ClockMasks {
    digits: f32,
    panel: f32,
}

/// 右上角的 HH:MM。单位是「数字高度」：1.0 = 一个数字高。
fn clock_masks(uv: vec2<f32>) -> ClockMasks {
    let digit_height = 0.072;
    let digit_width = 0.56;
    let total_width = 2.66;
    let anchor = vec2<f32>(P.clock_pos.x * P.screen_aspect, P.clock_pos.y);
    let base = anchor - vec2<f32>(total_width * digit_height, 0.0);
    let p = (uv * vec2<f32>(P.screen_aspect, 1.0) - base) / digit_height;

    let hour = floor(P.clock_time.x + 0.5);
    let minute = floor(P.clock_time.y + 0.5);
    let h1 = floor(hour / 10.0);
    let h0 = hour - h1 * 10.0;
    let m1 = floor(minute / 10.0);
    let m0 = minute - m1 * 10.0;

    var digits = 0.0;
    digits = max(digits, clock_digit_mask(h1, (p - vec2<f32>(0.0, 0.0)) / vec2<f32>(digit_width, 1.0)));
    digits = max(digits, clock_digit_mask(h0, (p - vec2<f32>(0.64, 0.0)) / vec2<f32>(digit_width, 1.0)));
    digits = max(digits, clock_digit_mask(m1, (p - vec2<f32>(1.40, 0.0)) / vec2<f32>(digit_width, 1.0)));
    digits = max(digits, clock_digit_mask(m0, (p - vec2<f32>(2.04, 0.0)) / vec2<f32>(digit_width, 1.0)));
    let colon = max(
        rounded_box_mask(p, vec2<f32>(1.30, 0.34), vec2<f32>(0.042, 0.042), 0.042),
        rounded_box_mask(p, vec2<f32>(1.30, 0.66), vec2<f32>(0.042, 0.042), 0.042),
    );
    digits = max(digits, colon);

    let panel = rounded_box_mask(
        p,
        vec2<f32>(1.32, 0.50),
        vec2<f32>(1.48, 0.62),
        0.16,
    );
    return ClockMasks(digits, panel);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // 壁纸的时钟故意走得慢:0.03 是"几乎不动"的底噪,速度拉满也只有 0.33。
    // 这段乘在时间上比乘在位移上更划算 —— 效果层的相位、推拉的周期会一起慢下来。
    let t = P.time * (0.03 + 0.30 * P.speed);
    let offset = seed_offset();
    let effect = u32(P.effect + 0.5);

    // 1 · Ken Burns:一条 0→1→0 的往返(用 cos 而不是线性循环,免得走到头时"跳"回去)
    let breathe = 0.5 - 0.5 * cos(t * 0.35);
    var uv = (in.uv - vec2<f32>(0.5)) / (1.0 + P.zoom * breathe) + vec2<f32>(0.5);
    uv = uv + vec2<f32>(sin(t * 0.23 + 1.7), cos(t * 0.19)) * P.pan * 0.04;
    uv = cover_uv(uv, P.image_aspect, P.screen_aspect);

    // 2 · 水波:把采样点挪走(改坐标),所以必须在采样之前
    if (effect == 6u) {
        uv.x = uv.x + sin(uv.y * 15.0 + t * 1.1) * 0.010 * P.intensity;
        uv.y = uv.y + cos(uv.x * 11.0 + t * 0.9) * 0.007 * P.intensity;
    }

    let sampled = textureSample(photo, photo_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)));
    var color = sampled.rgb;

    // 3 · 叠层:雪和雨是"往亮处加",雾是"往亮处提",云影是"往暗处压",光晕是"加一点暖光"
    if (effect == 1u) {
        color = mix(color, vec3<f32>(1.0), snow(uv, t, P.intensity, offset) * 0.55);
    } else if (effect == 2u) {
        color = mix(color, vec3<f32>(1.0), rain(uv, t, P.intensity, P.seed) * 0.42);
    } else if (effect == 3u) {
        color = mix(color, P.palette[4].rgb, fog(uv, t, P.intensity, offset) * 0.45);
    } else if (effect == 4u) {
        color = color * (1.0 - cloud(uv, t, P.intensity, offset) * 0.38);
    } else if (effect == 5u) {
        color = color + P.palette[3].rgb * glow(uv, t, P.intensity, offset) * 0.45;
    }

    // 4 · 统一收尾:压一点对比 + 稍微往中性色靠 —— 桌面图标常年压在壁纸上,
    //     生图那种高饱和高对比会让图标糊成一片
    let mean = (color.r + color.g + color.b) / 3.0;
    color = mix(color, vec3<f32>(mean), 0.06);

    let grain_noise = hash21(uv * P.resolution + vec2<f32>(t * 61.0, t * 37.0)) - 0.5;
    color = color + vec3<f32>(grain_noise) * (0.008 + 0.03 * P.grain);

    let vignette = smoothstep(
        1.30,
        0.30,
        length((in.uv - vec2<f32>(0.5)) * vec2<f32>(P.screen_aspect, 1.0)) * 1.35,
    );
    color = color * mix(1.0, vignette, P.vignette);

    color = color * (0.45 + 0.85 * P.brightness);
    // 对比度绕 0.5 中灰旋转，冷暖只动 R/B 两端 —— 比整体加色更不容易把肤色带脏
    color = (color - vec3<f32>(0.5)) * P.contrast + vec3<f32>(0.5);
    color = color + vec3<f32>(P.warmth * 0.045, P.warmth * 0.012, -P.warmth * 0.045);

    // 时钟最后画：它不该被暗角压暗，也不该跟着画面一起被曝光拉低
    if (P.clock_style > 0.5) {
        let clock = clock_masks(in.uv);
        color = mix(color, vec3<f32>(0.0), clock.panel * 0.30 * P.clock_opacity);
        color = mix(color, vec3<f32>(1.0), clock.digits * P.clock_opacity);
    }

    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
