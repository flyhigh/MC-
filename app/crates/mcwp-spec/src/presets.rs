//! 内置场景预设:每个预设 = 一个着色器分支 + 一组「有取值范围的参数」+ 一套默认配色。
//!
//! 加新预设的完整流程:这里加枚举分支 → `scene.wgsl` 加一个 `case` → 重跑一遍截图看观感。

use super::{Lighting, Mood, Motion, Scene, Source, WallpaperSpec};

/// 一个场景参数的取值范围与默认值。
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub key: &'static str,
    /// 面板里显示的中文名
    pub label: &'static str,
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

const fn p(key: &'static str, label: &'static str, default: f32) -> ParamSpec {
    ParamSpec {
        key,
        label,
        default,
        min: 0.0,
        max: 1.0,
    }
}

static RAINY_PARAMS: [ParamSpec; 3] = [
    p("rain", "雨量", 0.65),
    p("blur", "雾面", 0.35),
    p("parallax", "视差", 0.25),
];
static PAPER_PARAMS: [ParamSpec; 3] = [
    p("warmth", "暖度", 0.55),
    p("grain", "纸纤维", 0.30),
    p("softness", "柔度", 0.65),
];
static NEBULA_PARAMS: [ParamSpec; 3] = [
    p("density", "云密度", 0.55),
    p("swirl", "漩涡", 0.45),
    p("glow", "亮核", 0.60),
];
static GRID_PARAMS: [ParamSpec; 3] = [
    p("grid", "网格", 0.40),
    p("scan", "扫描线", 0.30),
    p("contrast", "对比", 0.55),
];

/// 内置场景预设。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// 雨夜城市:冷蓝、低饱和、有雨雾与城市灯光
    RainyWindowCity,
    /// 极简纸感:暖色、低对比、大留白、几乎不动
    MinimalPaper,
    /// 星云漂流:深色底 + 缓慢流动的彩色云带
    NebulaDrift,
    /// 专注网格:深色底 + 细网格 + 缓慢扫描线,给写代码的人用
    FocusGrid,
}

/// 全部预设,顺序 = 面板里的展示顺序。
pub const PRESETS: [Preset; 4] = [
    Preset::RainyWindowCity,
    Preset::MinimalPaper,
    Preset::NebulaDrift,
    Preset::FocusGrid,
];

impl Preset {
    /// 无指定时的默认预设。
    pub const DEFAULT: Preset = Preset::RainyWindowCity;

    pub fn id(self) -> &'static str {
        match self {
            Preset::RainyWindowCity => "rainy_window_city",
            Preset::MinimalPaper => "minimal_paper",
            Preset::NebulaDrift => "nebula_drift",
            Preset::FocusGrid => "focus_grid",
        }
    }

    /// 面板里显示的中文名。
    pub fn title(self) -> &'static str {
        match self {
            Preset::RainyWindowCity => "雨夜城市",
            Preset::MinimalPaper => "极简纸感",
            Preset::NebulaDrift => "星云漂流",
            Preset::FocusGrid => "专注网格",
        }
    }

    /// 一句话说明,给面板和 LLM 提示词共用。
    pub fn summary(self) -> &'static str {
        match self {
            Preset::RainyWindowCity => "冷蓝雨夜,城市灯火在雾气里缓慢漂移",
            Preset::MinimalPaper => "暖色纸感,低对比大留白,几乎静止",
            Preset::NebulaDrift => "深空底色,彩色星云缓慢流淌",
            Preset::FocusGrid => "深色底 + 细网格 + 缓慢扫描线,适合长时间专注",
        }
    }

    /// 传给着色器的场景编号,必须与 `scene.wgsl` 的 `case` 一一对应。
    pub fn scene_index(self) -> u32 {
        match self {
            Preset::RainyWindowCity => 0,
            Preset::MinimalPaper => 1,
            Preset::NebulaDrift => 2,
            Preset::FocusGrid => 3,
        }
    }

    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            Preset::RainyWindowCity => &RAINY_PARAMS,
            Preset::MinimalPaper => &PAPER_PARAMS,
            Preset::NebulaDrift => &NEBULA_PARAMS,
            Preset::FocusGrid => &GRID_PARAMS,
        }
    }

    /// 5 槽默认调色板(暗 → 亮)。
    pub fn palette(self) -> [&'static str; 5] {
        match self {
            Preset::RainyWindowCity => ["#0B1020", "#1B2A4A", "#4F7CFF", "#9BB8FF", "#E6ECFF"],
            Preset::MinimalPaper => ["#F3EEE4", "#E4D9C6", "#C9B79A", "#8A7A63", "#4A4238"],
            Preset::NebulaDrift => ["#05060F", "#14103A", "#5B2E9E", "#C46BD8", "#F2D6FF"],
            Preset::FocusGrid => ["#070B12", "#12202E", "#2E6F8E", "#7FD4E8", "#DFF6FF"],
        }
    }

    /// 默认动势:纸感刻意压到近乎静止,星云放开一点。
    pub fn default_motion(self) -> Motion {
        match self {
            Preset::RainyWindowCity => Motion {
                intensity: 0.45,
                speed: 0.60,
            },
            Preset::MinimalPaper => Motion {
                intensity: 0.12,
                speed: 0.25,
            },
            Preset::NebulaDrift => Motion {
                intensity: 0.65,
                speed: 0.45,
            },
            Preset::FocusGrid => Motion {
                intensity: 0.25,
                speed: 0.35,
            },
        }
    }

    /// 默认情绪倾向,用于「状态 → 场景」匹配的初始值(方案 3.7)。
    pub fn default_mood(self) -> Mood {
        match self {
            Preset::RainyWindowCity => Mood {
                calm: 0.65,
                focus: 0.70,
                energy: 0.35,
            },
            Preset::MinimalPaper => Mood {
                calm: 0.85,
                focus: 0.60,
                energy: 0.10,
            },
            Preset::NebulaDrift => Mood {
                calm: 0.55,
                focus: 0.35,
                energy: 0.75,
            },
            Preset::FocusGrid => Mood {
                calm: 0.60,
                focus: 0.90,
                energy: 0.25,
            },
        }
    }

    pub fn from_id(id: &str) -> Option<Preset> {
        let id = id.trim();
        PRESETS.into_iter().find(|preset| preset.id() == id)
    }

    /// 该类壁纸的默认 spec(所有参数取默认值)。
    pub fn default_spec(self) -> WallpaperSpec {
        WallpaperSpec {
            schema: super::SCHEMA_VERSION,
            id: String::new(),
            title: self.title().to_string(),
            source: Source::Builtin,
            keywords: vec![self.title().to_string()],
            mood: self.default_mood(),
            palette: self
                .palette()
                .iter()
                .map(|hex| (*hex).to_string())
                .collect(),
            scene: Scene {
                preset: self.id().to_string(),
                params: self
                    .params()
                    .iter()
                    .map(|spec| (spec.key.to_string(), spec.default))
                    .collect(),
            },
            motion: self.default_motion(),
            lighting: Lighting::default(),
            elements: Vec::new(),
            overlay: Default::default(),
            seed: 20261014,
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id())
    }
}

/// 预设目录的纯文本形式,给 `--preset list` 和以后拼 LLM 提示词复用。
pub fn catalog() -> String {
    let mut out = String::from("可用场景预设:\n");
    for preset in PRESETS {
        out.push_str(&format!(
            "  {:<18} {:<8} {}\n",
            preset.id(),
            preset.title(),
            preset.summary()
        ));
        for spec in preset.params() {
            out.push_str(&format!(
                "      - {:<10} {:<8} 默认 {:.2}  范围 {:.0}~{:.0}\n",
                spec.key, spec.label, spec.default, spec.min, spec.max
            ));
        }
    }
    out
}
