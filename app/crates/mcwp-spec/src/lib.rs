//! MC 壁纸 · 规格层:一张壁纸的**数据**长什么样。
//!
//! ## 为什么这里只剩「照片」一种东西
//!
//! 早期这一层的主类型是 `WallpaperSpec` —— 一份"画面由本地着色器现算"的参数表
//! (预设 id + 三个滑杆 + 调色板 + 元素/叠加层清单),和它配套的还有 `presets.rs`
//! 里那 4 个手调预设。AI 那一环当时的产物也是**改这份 spec**,而不是一张图。
//!
//! 后来路子换成了"直接生成一张图"(`Photo`):模型画出来的东西比几个噪声函数叠出来的
//! 场景好看得多,而代价是**画面的每一处细节都不再由本地代码决定** —— 那些滑杆能调的
//! 只有"像不像原来那个场景",留着它们反而是误导。所以 `WallpaperSpec` / `Preset` /
//! `scene.wgsl` 一起退休了,**预设清零**指的就是这件事。
//!
//! 现在这一层只描述 [`Photo`]:图在哪、多宽多高、动效怎么走、调色板是哪几个色。
//! 动效有两层:[`MotionRecipe`](细颗粒的旋钮,K 帧层的推拉呼吸)和
//! [`MotionPlan`](元素级的"灯在闪、车在走",只喂给图生视频那句提示词)。
//! 详情见 [`photo`] 模块。

pub mod photo;

pub use photo::{
    AREA_NAMES, ClockStyle, MotionElement, MotionKind, MotionPlan, MotionRecipe, Photo,
    WallpaperEdit, palette_from_rgba,
};

/// 调色板的槽位数。
///
/// **这是一个跨模块的硬约定**(不是随手写的常量):`photo.wgsl` 的 uniform 里
/// `palette` 就是定长 5 个 `vec4`,Rust 侧的 `PhotoParams` 必须逐字段对齐,
/// 少一个、多一个都会让着色器读到隔壁字段上去。要改得两边一起改。
pub const PALETTE_SLOTS: usize = 5;

/// FNV-1a 64 位。把一段字节压成 id 里那半截 —— `Photo::new_id` 拿它压提示词,
/// 壁纸侧拿它压"抬进来的那张壁纸来自哪个文件"(见 `mcwp-wallpaper` 的 `adopted_id`)。
///
/// 不用 `DefaultHasher`:那个的算法**没有跨版本保证**,同一句提示词换个 Rust 版本
/// 就可能是另一个 id,而 id 会落进文件名 —— 库里的条目会因此对不上。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
