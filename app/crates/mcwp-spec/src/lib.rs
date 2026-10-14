//! MC 壁纸 · `WallpaperSpec` —— LLM、控制面板、壁纸库与渲染器之间的唯一契约。
//!
//! 设计要点(见方案 3.1 / 3.6):
//! - spec 就是「真相源」:一张壁纸 = 一份 1~3 KB 的 JSON,可分享、可确定性复现(get-go 一致);
//! - 所有数值参数都有取值范围,`normalize()` 负责夹紧,LLM 输出越界也不会把渲染搞崩;
//! - `spec_hash()` 用于壁纸库去重:规范化之后取 hash,重复的 spec 只更新时间不占空间。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod presets;

pub use presets::{ParamSpec, Preset, PRESETS};

/// spec 结构版本。加字段不算破坏性改动,改语义时才 +1。
pub const SCHEMA_VERSION: u32 = 1;

/// 关键词最多保留多少个(超出丢弃,避免 LLM 刷屏)。
const MAX_KEYWORDS: usize = 12;

/// 调色板固定 5 槽 —— 着色器按 `array<vec4<f32>, 5>` 读取。
pub const PALETTE_SLOTS: usize = 5;

/// 壁纸来源,决定它怎么进壁纸库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 内置预设
    #[default]
    Builtin,
    /// 对话生成(T3 显式创作)
    Conversation,
    /// 用户导入的 spec 文件
    Imported,
    /// 状态快照(面板上的「存为壁纸」)
    Snapshot,
    /// 环境自动生成(L0)
    Auto,
}

/// 情绪向量。当前只用于「降级路径选场景 + 参数微调」,不直接驱动画面。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mood {
    pub calm: f32,
    pub focus: f32,
    pub energy: f32,
}

impl Default for Mood {
    fn default() -> Self {
        Self {
            calm: 0.5,
            focus: 0.5,
            energy: 0.5,
        }
    }
}

/// 场景:预设 id + 该预设自己的参数表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scene {
    pub preset: String,
    /// 参数名 → 值。只有该预设声明过的键会被保留(见 `WallpaperSpec::normalize`)。
    pub params: BTreeMap<String, f32>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            preset: Preset::DEFAULT.id().to_string(),
            params: BTreeMap::new(),
        }
    }
}

/// 动势:放大到画面就是「飘得多快、晃得多明显」。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Motion {
    /// 0 = 近乎静止,1 = 明显运动
    pub intensity: f32,
    /// 0 = 很慢,1 = 偏快(上限仍然刻意压得比较慢,壁纸不该抢注意力)
    pub speed: f32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            intensity: 0.45,
            speed: 0.6,
        }
    }
}

/// 光照:是否跟随桌面环境亮度。M1 阶段亮度采样尚未接入,先只吃 `key_light`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Lighting {
    pub mode: String,
    /// 主光强度 0~1
    #[serde(alias = "key_light")]
    pub key_light: f32,
    /// 是否让整体亮度跟随桌面平均亮度(方案第四节)
    #[serde(alias = "follow_desktop_brightness")]
    pub follow_desktop_brightness: bool,
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            mode: "auto_ambient".to_string(),
            key_light: 0.5,
            follow_desktop_brightness: true,
        }
    }
}

/// 附加元素。M1 只保留结构化信息,视觉效果由 `density` 等参数近似;
/// 真正的粒子/网格独立渲染留给 M2。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub count: u32,
}

/// 叠加层(时钟 / 一句话)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub clock: bool,
    pub quote: Option<String>,
}

/// 一份完整的壁纸规格。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WallpaperSpec {
    pub schema: u32,
    pub id: String,
    pub title: String,
    pub source: Source,
    pub keywords: Vec<String>,
    pub mood: Mood,
    pub palette: Vec<String>,
    pub scene: Scene,
    pub motion: Motion,
    pub lighting: Lighting,
    pub elements: Vec<Element>,
    pub overlay: Overlay,
    /// 确定性复现的随机种子
    pub seed: u32,
}

impl Default for WallpaperSpec {
    fn default() -> Self {
        Preset::DEFAULT.default_spec()
    }
}

impl WallpaperSpec {
    /// 解析 JSON。字段缺失会自动补默认值,多余字段忽略(向前兼容)。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let mut spec: Self = serde_json::from_str(text)?;
        spec.normalize();
        Ok(spec)
    }

    /// 用内置预设造一份 spec。
    pub fn from_preset(preset: Preset) -> Self {
        preset.default_spec()
    }

    /// 解析出的预设;未知 id 一律回退到默认预设。
    pub fn preset(&self) -> Preset {
        Preset::from_id(&self.scene.preset).unwrap_or(Preset::DEFAULT)
    }

    /// 取某个场景参数(已规范化)。未知键返回该预设的默认值。
    pub fn param(&self, key: &str) -> f32 {
        if let Some(value) = self.scene.params.get(key) {
            return *value;
        }
        self.preset()
            .params()
            .iter()
            .find(|spec| spec.key == key)
            .map(|spec| spec.default)
            .unwrap_or(0.0)
    }

    /// 夹紧所有数值、补齐调色板与前缀、丢弃未知场景参数。
    ///
    /// 这是「LLM 怎么胡说都不会渲染崩」的兜底:任何越界值都在这里被拉回区间。
    pub fn normalize(&mut self) {
        self.schema = SCHEMA_VERSION;

        let preset = self.preset();
        // 未知预设的 id 直接改写成默认预设,避免下游反复判断。
        if Preset::from_id(&self.scene.preset).is_none() {
            self.scene.preset = Preset::DEFAULT.id().to_string();
        }

        if self.title.trim().is_empty() {
            self.title = preset.title().to_string();
        }

        // 关键词:去空白、去空串、去重、限量。
        let mut keywords: Vec<String> = Vec::new();
        for raw in std::mem::take(&mut self.keywords) {
            let word = raw.trim();
            if word.is_empty() {
                continue;
            }
            if !keywords.iter().any(|existing| existing == word) {
                keywords.push(word.to_string());
            }
            if keywords.len() >= MAX_KEYWORDS {
                break;
            }
        }
        self.keywords = keywords;

        self.mood.calm = clamp01(self.mood.calm);
        self.mood.focus = clamp01(self.mood.focus);
        self.mood.energy = clamp01(self.mood.energy);

        // 调色板:固定 5 槽,非法色值用预设的对应槽位兜底。
        let fallback = preset.palette();
        let mut palette: Vec<String> = Vec::with_capacity(PALETTE_SLOTS);
        for (slot, default) in fallback.iter().enumerate() {
            let value = self
                .palette
                .get(slot)
                .and_then(|raw| normalize_hex(raw))
                .unwrap_or_else(|| (*default).to_string());
            palette.push(value);
        }
        self.palette = palette;

        // 场景参数:只保留该预设声明过的键,并夹到各自区间。
        let mut params = BTreeMap::new();
        for spec in preset.params() {
            let raw = self.scene.params.get(spec.key).copied().unwrap_or(spec.default);
            params.insert(spec.key.to_string(), clamp_range(raw, spec.min, spec.max));
        }
        self.scene.params = params;

        self.motion.intensity = clamp01(self.motion.intensity);
        self.motion.speed = clamp01(self.motion.speed);
        self.lighting.key_light = clamp01(self.lighting.key_light);
        if self.lighting.mode.trim().is_empty() {
            self.lighting.mode = "auto_ambient".to_string();
        }
        self.elements.truncate(16);
    }

    /// 规范化 JSON —— 字段顺序固定、`params` 用 BTreeMap 有序,所以同一份 spec 永远得到同一串字节。
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 给人类看的 JSON。
    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 壁纸库去重用的指纹(FNV-1a 64,规范化 JSON 上取值)。
    pub fn spec_hash(&self) -> String {
        // `id` 是"文件名",不参与指纹 —— 否则同一份 spec 存两次会算出两个 hash,去重就废了。
        let mut probe = self.clone();
        probe.id.clear();
        format!("{:016x}", fnv1a64(probe.canonical_json().as_bytes()))
    }

    /// 关键词回显,用来在桌宠气泡里提示「我提取到了什么」。
    pub fn keyword_hint(&self) -> String {
        if self.keywords.is_empty() {
            self.title.clone()
        } else {
            self.keywords.join(" / ")
        }
    }
}

fn clamp01(value: f32) -> f32 {
    clamp_range(value, 0.0, 1.0)
}

fn clamp_range(value: f32, min: f32, max: f32) -> f32 {
    if value.is_nan() {
        return min;
    }
    value.clamp(min, max)
}

/// `#RGB` / `#RRGGBB` → 统一成 `#RRGGBB` 大写;非法返回 `None`。
fn normalize_hex(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('#');
    let expanded = match trimmed.len() {
        3 => trimmed
            .chars()
            .flat_map(|c| [c, c])
            .collect::<String>(),
        6 => trimmed.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", expanded.to_ascii_uppercase()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 越界参数会被夹紧() {
        let mut spec = WallpaperSpec::from_preset(Preset::RainyWindowCity);
        spec.motion.intensity = 9.0;
        spec.scene.params.insert("rain".into(), -3.0);
        spec.normalize();
        assert_eq!(spec.motion.intensity, 1.0);
        assert_eq!(spec.param("rain"), 0.0);
    }

    #[test]
    fn 调色板补齐到五槽且色值规范化() {
        let mut spec = WallpaperSpec::default();
        spec.palette = vec!["#abc".into(), "zzz".into()];
        spec.normalize();
        assert_eq!(spec.palette.len(), PALETTE_SLOTS);
        assert_eq!(spec.palette[0], "#AABBCC");
        assert_eq!(spec.palette[1], Preset::DEFAULT.palette()[1]);
    }

    #[test]
    fn 未知预设回退到默认预设() {
        let mut spec = WallpaperSpec::default();
        spec.scene.preset = "cyberpunk_city_9000".into();
        spec.normalize();
        assert_eq!(spec.scene.preset, Preset::DEFAULT.id());
    }

    #[test]
    fn 未知场景参数被丢弃() {
        let mut spec = WallpaperSpec::default();
        spec.scene.params.insert("不存在".into(), 0.5);
        spec.normalize();
        assert!(spec.scene.params.keys().all(|k| k == "rain" || k == "blur" || k == "parallax"));
    }

    #[test]
    fn 同一份概念得到同一个指纹() {
        let a = WallpaperSpec::from_preset(Preset::NebulaDrift);
        let mut b = WallpaperSpec::from_preset(Preset::NebulaDrift);
        b.palette.reverse();
        b.normalize();
        assert_eq!(a.spec_hash(), a.spec_hash());
        assert_ne!(a.spec_hash(), b.spec_hash());
    }

    #[test]
    fn 缺字段的_json_能解析并补默认值() {
        let spec = WallpaperSpec::parse(r#"{ "scene": { "preset": "focus_grid" } }"#).unwrap();
        assert_eq!(spec.preset(), Preset::FocusGrid);
        assert_eq!(spec.palette.len(), PALETTE_SLOTS);
        assert_eq!(spec.motion.intensity, 0.45);
    }

    #[test]
    fn 恶劣输入被全面兜底() {
        // 这份 JSON 里几乎每个字段都是错的,normalize 之后必须完全可控。
        let spec = WallpaperSpec::parse(
            r##"{
              "keywords": ["雨", "夜", "雨", "  "],
              "palette": ["#0B1020", "#abc", "不是颜色"],
              "scene": { "preset": "cyberpunk_9000", "params": { "rain": 5.0, "blur": -2, "不存在": 0.9 } },
              "motion": { "intensity": 3.0, "speed": 0.6 },
              "lighting": { "mode": "auto_ambient", "keyLight": 0.5, "followDesktopBrightness": true }
            }"##,
        )
        .unwrap();
        assert_eq!(spec.keywords, vec!["雨", "夜"]);
        assert_eq!(spec.palette[1], "#AABBCC");
        assert_eq!(spec.palette.len(), PALETTE_SLOTS);
        assert_eq!(spec.preset(), Preset::RainyWindowCity);
        assert_eq!(spec.param("rain"), 1.0);
        assert_eq!(spec.param("blur"), 0.0);
        assert_eq!(spec.param("不存在"), 0.0);
        assert_eq!(spec.scene.params.len(), 3);
        assert_eq!(spec.motion.intensity, 1.0);
        // camelCase 与 snake_case 两种写法都要能读进来。
        assert_eq!(spec.lighting.key_light, 0.5);
        assert!(spec.lighting.follow_desktop_brightness);
    }
}
