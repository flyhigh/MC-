//! 把 `WallpaperSpec` 的调色板翻译成面板的玻璃色。
//!
//! HIG 里的 Liquid Glass 是「自适应」的:玻璃会跟着背后的内容调明暗和染色。我们没有真背景
//! 模糊,但可以让玻璃的染色、环境光斑跟着当前壁纸的调色板走 —— 换一张壁纸,面板的气色也跟着变。
//!
//! 一条设计约束:**极性固定为深色玻璃**。壁纸调色板可能是浅色(比如「极简纸感」是米白),
//! 如果玻璃也跟着变浅,面板里的文字、发丝线、高光就都得再来一套浅色版本。所以这里只取壁纸的
//! **色相**,亮度统一压到深色区间,再交给面板的白纱/高光去提亮。

use slint::Color;

/// iOS 26 系统蓝(dark 变体),面板主色。
const SYSTEM_BLUE: (u8, u8, u8) = (0x0A, 0x84, 0xFF);
/// 玻璃底色(壁纸调色板取不到值时兜底)。
const FALLBACK_TINT: (u8, u8, u8) = (0x10, 0x14, 0x1F);

/// 面板要用的五个颜色,全部由 `[String; 5]` 调色板推出来。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassColors {
    /// 环境底(内容层)的起始色,偏深
    pub ambient_a: (u8, u8, u8),
    /// 环境底的主光斑色,取壁纸的强调色
    pub ambient_b: (u8, u8, u8),
    /// 环境底的收尾色,最深
    pub ambient_c: (u8, u8, u8),
    /// 玻璃本体的染色
    pub glass_tint: (u8, u8, u8),
    /// 控件强调色:系统蓝 + 壁纸强调色
    pub accent: (u8, u8, u8),
}

/// `#RRGGBB` / `#RGB` → RGB。解析不了返回 `None`(`WallpaperSpec` 已经规范化过,这里只是兜底)。
pub fn parse_hex(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.trim().trim_start_matches('#');
    let expand = |c: u8| c * 17; // #abc → #aabbcc
    match hex.len() {
        3 => {
            let mut it = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8));
            Some((expand(it.next()??), expand(it.next()??), expand(it.next()??)))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

/// 感知亮度(Rec.709),0.0~1.0。
pub fn luma(rgb: (u8, u8, u8)) -> f32 {
    (0.2126 * rgb.0 as f32 + 0.7152 * rgb.1 as f32 + 0.0722 * rgb.2 as f32) / 255.0
}

/// 三通道等比缩放:保住色相,只改亮度。
fn scale_rgb(rgb: (u8, u8, u8), scale: f32) -> (u8, u8, u8) {
    let apply = |c: u8| (c as f32 * scale).round().clamp(0.0, 255.0) as u8;
    (
        apply(rgb.0),
        apply(rgb.1),
        apply(rgb.2),
    )
}

/// 乘性压暗:只减不增,保住色相。
pub fn darken(rgb: (u8, u8, u8), factor: f32) -> (u8, u8, u8) {
    scale_rgb(rgb, factor.clamp(0.0, 1.0))
}

/// 按权重混合两色,`factor = 0` 取 `a`。
pub fn mix(a: (u8, u8, u8), b: (u8, u8, u8), factor: f32) -> (u8, u8, u8) {
    let f = factor.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// 把颜色压进"深色玻璃"区间:亮度高于 `max_luma` 就整体等比压暗。
pub fn clamp_to_dark(rgb: (u8, u8, u8), max_luma: f32) -> (u8, u8, u8) {
    let current = luma(rgb);
    if current <= max_luma || current <= f32::EPSILON {
        rgb
    } else {
        darken(rgb, max_luma / current)
    }
}

/// 把颜色整体缩放(保住色相)到指定亮度,该亮就亮、该暗就暗。
///
/// `clamp_to_dark` 只压不抬,碰上深色壁纸(比如「雨夜城市」底色 #0B1020)会得到一块近乎纯黑的
/// 面板 —— 玻璃背后没东西可"透",叠上去就只剩一片死黑。环境底改用这个把壁纸的**色相**放进
/// 设计好的亮度档位:亮壁纸压暗、暗壁纸提亮,面板的气色因此不跟着壁纸一起沉下去。
pub fn fit_luma(rgb: (u8, u8, u8), target: f32) -> (u8, u8, u8) {
    let current = luma(rgb);
    if current <= f32::EPSILON {
        // 纯黑没有色相可提,提亮只会得到一块灰
        return rgb;
    }
    let mut scale = target.max(0.0) / current;
    // 提亮到有通道撞顶就整色一起退,免得某一路先饱和、把色相带偏
    let peak = rgb.0.max(rgb.1).max(rgb.2) as f32 * scale;
    if peak > 255.0 {
        scale *= 255.0 / peak;
    }
    scale_rgb(rgb, scale)
}

/// 从壁纸调色板推出面板的五个颜色。
///
/// 约定取前四个槽位(`WallpaperSpec` 的调色板固定 5 槽、由深到浅):底色 / 中间色 / 强调色 /
/// 高光色。缺槽位就用兜底色,保证永远有结果。
pub fn glass_colors(palette: &[String]) -> GlassColors {
    let slot = |index: usize| -> Option<(u8, u8, u8)> {
        palette.get(index).and_then(|text| parse_hex(text))
    };
    let base = slot(0).unwrap_or(FALLBACK_TINT);
    let mid = slot(1).unwrap_or(base);
    let accent_source = slot(2).unwrap_or(base);

    GlassColors {
        // 环境底:底色定调,亮部拿壁纸的强调色(混一点中间色),两边各自归一到设计亮度档位。
        // 这三档就是"玻璃后面那张照片"的明暗范围,拉开差距玻璃才有厚度。
        ambient_a: fit_luma(base, 0.15),
        ambient_b: fit_luma(mix(mid, accent_source, 0.6), 0.28),
        ambient_c: fit_luma(darken(base, 0.45), 0.06),
        // 玻璃染色:仍然压得很暗,这是白字可读的底线
        glass_tint: fit_luma(darken(base, 0.75), 0.09),
        // 强调色:系统蓝占大头,壁纸强调色只是"染一点"
        accent: clamp_to_dark(mix(SYSTEM_BLUE, accent_source, 0.35), 0.80),
    }
}

impl GlassColors {
    pub fn ambient_a(&self) -> Color {
        rgb(self.ambient_a)
    }
    pub fn ambient_b(&self) -> Color {
        rgb(self.ambient_b)
    }
    pub fn ambient_c(&self) -> Color {
        rgb(self.ambient_c)
    }
    pub fn glass_tint(&self) -> Color {
        rgb(self.glass_tint)
    }
    pub fn accent(&self) -> Color {
        rgb(self.accent)
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::from_rgb_u8(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palettes() -> Vec<Vec<String>> {
        vec![
            // 雨夜城市(冷蓝,深)
            vec!["#0B1020", "#1B2A4A", "#4F7CFF", "#9BB8FF", "#E6ECFF"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            // 极简纸感(米白,浅) —— 浅色壁纸也得落到深色玻璃区间
            vec!["#F3EEE4", "#E4D9C6", "#C9B79A", "#8A7A63", "#4A4238"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        ]
    }

    #[test]
    fn parse_hex_handles_both_forms() {
        assert_eq!(parse_hex("#0A84FF"), Some((0x0A, 0x84, 0xFF)));
        assert_eq!(parse_hex("abc"), Some((0xAA, 0xBB, 0xCC)));
        assert_eq!(parse_hex("#GG0000"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn glass_colors_stay_dark_for_any_palette() {
        for palette in palettes() {
            let colors = glass_colors(&palette);
            for (name, value) in [
                ("ambient_a", colors.ambient_a),
                ("ambient_b", colors.ambient_b),
                ("ambient_c", colors.ambient_c),
                ("glass_tint", colors.glass_tint),
            ] {
                assert!(luma(value) <= 0.43, "{name} 太亮:{value:?}");
            }
            // 强调色必须还看得出是蓝的:蓝分量最高
            let (r, g, b) = colors.accent;
            assert!(b > r && b >= g, "强调色跑偏:{:?}", colors.accent);
        }
    }

    #[test]
    fn glass_colors_keep_wallpaper_hue() {
        let blue = glass_colors(&palettes()[0]);
        let paper = glass_colors(&palettes()[1]);
        // 冷色壁纸的环境底,蓝分量应当高于暖色壁纸的
        assert!(blue.ambient_a.2 > paper.ambient_a.2);
        assert!(blue.ambient_b.2 >= blue.ambient_b.0);
        assert!(paper.ambient_a.0 >= paper.ambient_a.2);
    }

    #[test]
    fn empty_palette_falls_back() {
        let colors = glass_colors(&[]);
        assert_eq!(
            colors.glass_tint,
            fit_luma(darken(FALLBACK_TINT, 0.75), 0.09)
        );
        assert!(luma(colors.accent) > 0.0);
    }

    #[test]
    fn ambient_luma_is_normalized() {
        // 不管壁纸是深是浅,环境底都落在设计好的档位上 —— 深壁纸被提亮,浅壁纸被压暗
        for palette in palettes() {
            let colors = glass_colors(&palette);
            assert!(
                (luma(colors.ambient_a) - 0.15).abs() < 0.02,
                "ambient_a 没归到位:{:?}",
                colors.ambient_a
            );
            assert!(
                (luma(colors.ambient_b) - 0.28).abs() < 0.02,
                "ambient_b 没归到位:{:?}",
                colors.ambient_b
            );
            // 玻璃一定比环境底暗,否则白字读不出来
            assert!(luma(colors.glass_tint) < luma(colors.ambient_a));
        }
    }
}
