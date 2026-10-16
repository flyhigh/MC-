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
    _pad: vec2<f32>,              // 136
    // 144 **元素级动效**:每格 (画什么, 在哪儿, 往哪边, 快慢),见 `mcwp_spec::element_slots`
    elements: array<vec4<f32>, 5>,
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

// ---------- 元素层:画面里**那块地方**上的**那个东西**在动 ----------
//
// 和上面那层的区别只有一个,但是关键:上面那层是"整张图套一个效果"(下雪 / 起雾),
// 这层是"**左上角那片灯在闪**"。数据来自看图分析(`MotionPlan`),一格四项:
// 画什么 / 在哪儿 / 往哪边 / 快慢(见 `mcwp_spec::element_slots`)。
//
// 为什么它**必然无缝**:它是时间的**连续函数**,根本没有"一段片子放完"这回事 ——
// 灯用 sin 明灭、云用 sin 来回飘、车灯用 sin 往返、水波是相位不同的正弦。
// 视频那条路上怎么也压不住的那个接缝,在这里压根不存在(对比见 §11.22)。

/// 元素层的整体强度。**比效果层克制**:这一层是"画面里有东西在动",不是"往画面上加特效"。
const ELEMENT_STRENGTH: f32 = 0.55;

/// 元素画在哪儿:九宫格 0~8,9(或更大)= 全图。返回 `(中心.xy, 半径.xy)`。
fn area_box(code: f32) -> vec4<f32> {
    if (code > 8.5) {
        return vec4<f32>(0.5, 0.5, 0.5, 0.5);
    }
    let cell = clamp(code, 0.0, 8.0);
    let row = floor(cell / 3.0);
    let column = cell - row * 3.0;
    // 半径给 0.20 而不是 1/6:铺满整格会让人看出"有个方框在动"
    return vec4<f32>((column + 0.5) / 3.0, (row + 0.5) / 3.0, 0.20, 0.20);
}

/// 一块柔和的遮罩:中心 1,边缘 0。方框的边必须看不出来 ——
/// 一个矩形亮斑在画面里飘,比整张图不动还假。
fn area_mask(uv: vec2<f32>, area: vec4<f32>) -> f32 {
    let d = abs(uv - area.xy) / max(area.zw, vec2<f32>(0.001));
    return smoothstep(1.0, 0.25, max(d.x, d.y));
}

/// 往哪边 → 横轴的符号(1 右 / 2 左);没写方向就是 0,由各元素自己决定怎么动。
fn dir_x(direction: f32) -> f32 {
    if (direction == 1.0) { return 1.0; }
    if (direction == 2.0) { return -1.0; }
    return 0.0;
}

/// 往哪边 → 竖轴的符号。注意 uv 的 y **向下**增长(y=0 是屏幕顶端),
/// 所以"向上"是 -1 —— 这里写反了,云会往下沉、雪会往天上飞。
fn dir_y(direction: f32) -> f32 {
    if (direction == 3.0) { return -1.0; }
    if (direction == 4.0) { return 1.0; }
    return 0.0;
}

/// 元素层里"**改坐标**"的那两个 —— 和上面水波一个道理,**必须在采样之前**做。
fn warp_element(e: vec4<f32>, uv: vec2<f32>, t: f32) -> vec2<f32> {
    let visual = u32(e.x + 0.5);
    if (visual == 0u) { return uv; }
    let mask = area_mask(uv, area_box(e.y));
    if (mask <= 0.0) { return uv; }
    let rate = max(e.w, 0.1);
    if (visual == 4u) {
        // 水:两个相位不同的正弦把采样点挪一挪 —— 像隔着一层水看
        let amp = 0.0055 * ELEMENT_STRENGTH * mask;
        return uv + vec2<f32>(
            sin(uv.y * 44.0 + t * 1.25 * rate) * amp,
            cos(uv.x * 36.0 + t * 1.05 * rate) * amp * 0.7,
        );
    }
    if (visual == 5u || visual == 6u) {
        // 树 / 衣摆:横向轻轻摇。幅度很小 —— 大了整片画面会像在抽搐
        let amp = 0.0022 * ELEMENT_STRENGTH * mask;
        return uv + vec2<f32>(sin(t * 0.5 * rate + uv.y * 5.5) * amp, 0.0);
    }
    return uv;
}

/// 元素层里"**叠颜色**"的那几个 —— 采样之后做。
fn apply_element(
    e: vec4<f32>,
    uv: vec2<f32>,
    t: f32,
    color_in: vec3<f32>,
    offset: vec2<f32>,
) -> vec3<f32> {
    let visual = u32(e.x + 0.5);
    if (visual == 0u) { return color_in; }
    let area = area_box(e.y);
    let mask = area_mask(uv, area);
    if (mask <= 0.0) { return color_in; }
    let rate = max(e.w, 0.1);
    let strength = ELEMENT_STRENGTH * mask;
    var color = color_in;

    if (visual == 1u) {
        // 灯:一片一片地明灭。
        // **不要用格子**:`floor(uv * 18)` 那种切法里,同一格整块同亮同暗,边界立刻
        // 变成看得见的方块(实测就是一层 18×18 的马赛克糊在画面上)。换成 fbm 当相位和
        // 速率,空间上连续 —— 相邻像素只是相位差一点,于是看到的是"一片街区的灯在呼吸"。
        // 再乘画面自己的亮度:只让本来就亮的地方闪,否则整片夜空会跟着一起亮。
        let n1 = fbm(uv * 3.6, offset);
        let n2 = fbm(uv * 7.5 + vec2<f32>(3.3, 8.1), offset + vec2<f32>(1.9, 4.2));
        let phase = (n1 * 0.7 + n2 * 0.3) * 6.2832;
        let blink = 0.5 + 0.5 * sin(t * (0.5 + 0.9 * n1) * rate + phase);
        let luma = dot(color_in, vec3<f32>(0.299, 0.587, 0.114));
        let on_light = smoothstep(0.18, 0.55, luma);
        let amount = (0.25 + 0.75 * on_light) * (0.4 + 1.0 * blink) * strength;
        color = color + color * amount;
    } else if (visual == 11u) {
        // 星:一格一颗,闪的相位各不同
        let cell = floor(uv * vec2<f32>(24.0));
        let seed = hash21(cell + offset);
        let lit = step(0.82, seed);
        let twinkle = 0.25 + 0.75 * (0.5 + 0.5 * sin(t * (0.6 + 1.6 * seed) * rate + seed * 6.2832));
        let dot = smoothstep(0.42, 0.0, length(fract(uv * vec2<f32>(24.0)) - vec2<f32>(0.5)));
        color = color + vec3<f32>(1.0) * lit * twinkle * dot * strength;
    } else if (visual == 2u) {
        // 云:在那一块里**来回**飘(不是一路飘走),形状是 fbm 云团
        let drift = sin(t * 0.055 * rate) * 0.55;
        let shape = fbm(
            uv * vec2<f32>(2.6, 4.0) + vec2<f32>(drift, 0.0),
            offset + vec2<f32>(7.3, 1.1),
        );
        let amount = smoothstep(0.42, 0.92, shape) * strength;
        // 云是"提亮 + 往浅色靠":夜里那片云会被灯光照亮,白天就是白云
        color = mix(color, max(color, P.palette[4].rgb) * 1.15, amount * 0.55);
    } else if (visual == 3u) {
        // 雾 / 烟:两层反向慢滚
        let low = fbm(uv * 3.4 + vec2<f32>(t * 0.03 * rate, 0.0), offset);
        let high = fbm(uv * 7.2 - vec2<f32>(t * 0.02 * rate, 0.0), offset + vec2<f32>(4.1, 2.7));
        let amount = smoothstep(0.30, 0.95, low * 0.7 + high * 0.3) * strength;
        color = mix(color, mix(color, P.palette[4].rgb, 0.45), amount * 0.6);
    } else if (visual == 7u) {
        // 车灯:几盏灯在**同一条车道**上来回跑。往返而不是一路开走 ——
        // 一路开走的话它们跑到头就得瞬移回来,那一瞬就是一记跳
        let side = select(1.0, dir_x(e.z), dir_x(e.z) != 0.0);
        let lane = smoothstep(0.018, 0.0, abs(uv.y - (area.y + area.w * 0.35)));
        var heads = 0.0;
        for (var i = 0; i < 3; i = i + 1) {
            let phase = f32(i) * 2.1;
            let pos = 0.5 + 0.5 * sin(t * 0.19 * rate * side + phase);
            heads = heads + smoothstep(0.13, 0.015, abs(uv.x - (area.x + (pos - 0.5) * area.z * 2.2)));
        }
        color = color + vec3<f32>(1.0, 0.86, 0.62) * heads * lane * strength;
    } else if (visual == 8u) {
        // 鸟:几个亮点慢慢飞(两个频率不同的正弦 = 一条不重复的轨迹)
        let flip = select(1.0, dir_y(e.z), dir_y(e.z) != 0.0);
        var dots = 0.0;
        for (var i = 0; i < 3; i = i + 1) {
            let phase = f32(i) * 1.7;
            let p = area.xy + vec2<f32>(
                sin(t * 0.085 * rate + phase) * area.z * 0.7,
                sin(t * 0.12 * rate * flip + phase * 1.9) * area.w * 0.30,
            );
            dots = dots + smoothstep(0.012, 0.0, length((uv - p) * vec2<f32>(1.0, 1.7)));
        }
        color = color + vec3<f32>(0.95, 0.95, 0.9) * dots * strength;
    } else if (visual == 9u) {
        // 火 / 烛:暖色跳,比灯更快也更碎。
        // 空间上也要碎(向上舔的 fbm):否则一块区域整块同亮同暗,又是一片"方形的火"。
        let flicker = 0.55 + 0.45 * sin(t * 2.7 * rate) * (0.55 + 0.45 * sin(t * 6.1 * rate + 1.7));
        let lick = fbm(uv * 9.0 + vec2<f32>(0.0, -t * 0.22 * rate), offset + vec2<f32>(2.2, 5.5));
        let amount = smoothstep(0.42, 0.95, lick) * flicker * strength;
        color = color + vec3<f32>(1.0, 0.52, 0.16) * amount * 0.85;
    } else if (visual == 10u) {
        // 雪:复用上面那套雪粒,只是裁在这一块里
        let down = select(1.0, dir_y(e.z), dir_y(e.z) != 0.0);
        var snow_uv = uv;
        snow_uv.y = select(snow_uv.y, 1.0 - snow_uv.y, down < 0.0);
        color = mix(color, vec3<f32>(1.0), snow(snow_uv, t * rate, 0.75, offset) * strength * 0.5);
    }
    return color;
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

    // 2b · 元素层里改坐标的那几个(水波 / 摇曳):同样必须在采样之前。
    //      五个格子**手工摊开**:对着 uniform 数组做动态下标在部分后端上会直接被拒,
    //      而槽位本来就是定长 5,摊开既稳又清楚。
    uv = warp_element(P.elements[0], uv, t);
    uv = warp_element(P.elements[1], uv, t);
    uv = warp_element(P.elements[2], uv, t);
    uv = warp_element(P.elements[3], uv, t);
    uv = warp_element(P.elements[4], uv, t);

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

    // 3b · 元素层:画面里"那块地方上的那个东西"在动(见上面那一段的说明)。
    //      和效果层**互斥**:有元素的图,`PhotoParams` 会把 effect 清零(见 `from_photo`)。
    color = apply_element(P.elements[0], uv, t, color, offset);
    color = apply_element(P.elements[1], uv, t, color, offset);
    color = apply_element(P.elements[2], uv, t, color, offset);
    color = apply_element(P.elements[3], uv, t, color, offset);
    color = apply_element(P.elements[4], uv, t, color, offset);

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

    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
