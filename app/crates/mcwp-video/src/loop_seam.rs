//! 离线循环接缝分析。
//!
//! 目标不是“把末帧和首帧硬接上”，而是找出视频里**真正相似的一对帧**，
//! 并估计这段时间里画面内容整体移动了多少。播放器拿到这组数之后，
//! 只对**有变化的区域**沿位移做对齐混合，静止的山、房子不会被一起推走。
//!
//! 这里刻意不依赖 `mcwp-image` / `mcwp-spec`：解码 crate 仍然只认识像素与帧号。
//! 分析在后台/命令行跑一次，结果由上层落进壁纸元数据。

use std::path::Path;

use anyhow::Result;

use crate::Player;

/// 相似帧搜索用的工作宽度。再大只是在重复解码器已经做过的缩放，
/// 再小云层的低频纹理就分不开了。
const SEARCH_WIDTH: u32 = 96;
/// 最多分析多少帧。壁纸视频通常 5~10 秒；超过这个长度就不再往后吞内存。
const MAX_FRAMES: usize = 600;
/// 循环至少这么长，避免找出“相邻两帧很像”这种没有意义的解。
const MIN_LOOP_SECS: f32 = 1.2;
/// 对齐后平均差异超过这个值就不认为找到了可用的接缝。
const MAX_ACCEPT_SCORE: f32 = 0.30;

/// 一段视频的循环接缝。
///
/// `dx` / `dy` 是**从起点到终点**的内容位移比例（正 = 向右/向下）。
/// 播放端按自己的解码尺寸乘回去；不依赖分析时的分辨率。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopSeam {
    pub start_frame: u32,
    /// 不包含这一帧。
    pub end_frame: u32,
    pub dx: f32,
    pub dy: f32,
    /// 对齐并只统计运动区域后的平均差异，0 最好。
    pub score: f32,
}

#[derive(Clone)]
struct GrayFrame {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

/// 打开视频、解到工作分辨率、找一对最像的首尾帧。
///
/// `limit` 是解码上限；分析只需要 320p 左右，传屏幕尺寸会白白多解几倍像素。
pub fn analyze_loop(path: &Path, limit: (u32, u32)) -> Result<Option<LoopSeam>> {
    let mut player = Player::open(path, limit)?;
    let info = player.info();
    let mut frames = Vec::new();
    while let Some(rgba) = player.next_frame()? {
        frames.push(downsample_gray(rgba, info.width, info.height));
        if frames.len() >= MAX_FRAMES {
            break;
        }
    }
    Ok(find_seam(&frames, info.fps))
}

/// 纯帧序列版本，方便单测和以后从别的帧源（比如已缓存的解码帧）复用。
fn find_seam(frames: &[GrayFrame], fps: f32) -> Option<LoopSeam> {
    let count = frames.len();
    if count < 8 {
        return None;
    }
    let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
    let min_lag = ((fps * MIN_LOOP_SECS).round() as usize)
        .max(4)
        .min(count.saturating_sub(1));
    if min_lag == 0 || min_lag >= count {
        return None;
    }

    // 对每个周期都做一次位移对齐评分。只看零位移相似度会把“云已经飘走、
    // 但整体结构相同”的长周期误判成不像，结果只敢选短循环。
    let mut periods: Vec<(usize, usize, i32, i32, f32)> = Vec::new();
    for lag in min_lag..count {
        let mut best_start = 0usize;
        let mut best_zero = f32::INFINITY;
        for start in 0..count - lag {
            let score = frame_distance(&frames[start], &frames[start + lag]);
            if score < best_zero {
                best_zero = score;
                best_start = start;
            }
        }
        let (dx, dy, aligned) = estimate_translation(
            &frames[best_start],
            &frames[best_start + lag],
        );
        let shifted = aligned.is_finite() && aligned + 0.002 < best_zero;
        let (dx, dy, score) = if shifted {
            (dx, dy, aligned)
        } else {
            (0, 0, best_zero)
        };
        periods.push((lag, best_start, dx, dy, score));
    }
    let best_score = periods
        .iter()
        .map(|(_, _, _, _, score)| *score)
        .fold(f32::INFINITY, f32::min);
    if best_score > MAX_ACCEPT_SCORE {
        return None;
    }

    // 在“分数和最佳值差不多”的周期里挑**最长**的那个。
    // 短周期往往只是云刚好回到相似位置，接缝看不出来但重复感很明显。
    let acceptable = best_score + 0.012;
    let selected = periods
        .iter()
        .rev()
        .find(|(_, _, _, _, score)| *score <= acceptable)?;
    let lag = selected.0;

    // 选中的周期再用多几个起点细挑一次；之前每个周期只看了零位移最好的那个起点。
    let mut starts: Vec<(f32, usize)> = (0..count - lag)
        .map(|index| (frame_distance(&frames[index], &frames[index + lag]), index))
        .collect();
    starts.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    starts.truncate(8);
    let mut best = (selected.4, selected.1, selected.2, selected.3);
    for (zero_score, index) in starts {
        let (dx, dy, aligned_score) = estimate_translation(&frames[index], &frames[index + lag]);
        let shifted = aligned_score.is_finite() && aligned_score + 0.002 < zero_score;
        let (dx, dy, score) = if shifted {
            (dx, dy, aligned_score)
        } else {
            (0, 0, zero_score)
        };
        if score < best.0 {
            best = (score, index, dx, dy);
        }
    }

    let (score, start, dx, dy) = best;
    if score > MAX_ACCEPT_SCORE {
        return None;
    }
    Some(LoopSeam {
        start_frame: start as u32,
        end_frame: (start + lag) as u32,
        dx: dx as f32 / frames[start].width.max(1) as f32,
        dy: dy as f32 / frames[start].height.max(1) as f32,
        score,
    })
}

/// 解出来的 RGBA → 缩小后的灰度帧。块平均比最近邻稳，云层不会因为跳采样出现假纹理。
fn downsample_gray(rgba: &[u8], src_width: u32, src_height: u32) -> GrayFrame {
    let src_width = src_width.max(1);
    let src_height = src_height.max(1);
    let width = SEARCH_WIDTH.min(src_width).max(2) as usize;
    let height = ((src_height as u64 * width as u64 / src_width as u64) as u32)
        .max(2)
        .min(240) as usize;
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        let sy0 = (y as u64 * src_height as u64 / height as u64) as u32;
        let sy1 = (((y + 1) as u64 * src_height as u64 / height as u64) as u32)
            .max(sy0 + 1)
            .min(src_height);
        for x in 0..width {
            let sx0 = (x as u64 * src_width as u64 / width as u64) as u32;
            let sx1 = (((x + 1) as u64 * src_width as u64 / width as u64) as u32)
                .max(sx0 + 1)
                .min(src_width);
            let mut red = 0u64;
            let mut green = 0u64;
            let mut blue = 0u64;
            let mut sampled = 0u64;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let offset = ((sy * src_width + sx) * 4) as usize;
                    if offset + 2 >= rgba.len() {
                        continue;
                    }
                    red += rgba[offset] as u64;
                    green += rgba[offset + 1] as u64;
                    blue += rgba[offset + 2] as u64;
                    sampled += 1;
                }
            }
            let gray = if sampled == 0 {
                0
            } else {
                let red = red / sampled;
                let green = green / sampled;
                let blue = blue / sampled;
                ((red * 77 + green * 150 + blue * 29) >> 8).min(255) as u8
            };
            pixels.push(gray);
        }
    }
    GrayFrame {
        width,
        height,
        pixels,
    }
}

/// 两帧的平均绝对灰度差，0~1。用于找周期，不用于最终位移打分。
fn frame_distance(left: &GrayFrame, right: &GrayFrame) -> f32 {
    if left.width != right.width || left.height != right.height || left.pixels.is_empty() {
        return 1.0;
    }
    let mut sum = 0u64;
    for (a, b) in left.pixels.iter().zip(&right.pixels) {
        sum += a.abs_diff(*b) as u64;
    }
    sum as f32 / (left.pixels.len().max(1) as f32 * 255.0)
}

/// 估计 `left` 到 `right` 的整体位移：`right(x + dx, y + dy) ≈ left(x, y)`。
///
/// 先构造“变化区域”权重，只在云/水/车流这些真的动过的地方比较；
/// 静止的山体权重接近 0，不会把位移拉回 0。
fn estimate_translation(left: &GrayFrame, right: &GrayFrame) -> (i32, i32, f32) {
    if left.width != right.width || left.height != right.height {
        return (0, 0, 1.0);
    }
    let motion = motion_weights(left, right);
    let total_weight = motion
        .as_ref()
        .map(|weights| weights.iter().map(|value| *value as u64).sum::<u64>())
        .unwrap_or_else(|| (left.width * left.height) as u64 * 255);
    let max_dx = (left.width as i32 / 4).max(1);
    let max_dy = (left.height as i32 / 4).max(1);
    let coarse = 4i32;

    let mut best = (0i32, 0i32, f32::INFINITY);
    let mut dy = -max_dy;
    while dy <= max_dy {
        let mut dx = -max_dx;
        while dx <= max_dx {
            let score = shift_distance(left, right, dx, dy, motion.as_deref(), total_weight);
            if score < best.2 {
                best = (dx, dy, score);
            }
            dx += coarse;
        }
        dy += coarse;
    }

    // 粗搜到 4 像素，再在附近按 1 像素细搜一遍。
    let (coarse_dx, coarse_dy, _) = best;
    for dy in (coarse_dy - coarse)..=(coarse_dy + coarse) {
        for dx in (coarse_dx - coarse)..=(coarse_dx + coarse) {
            let score = shift_distance(left, right, dx, dy, motion.as_deref(), total_weight);
            if score < best.2 {
                best = (dx, dy, score);
            }
        }
    }

    let zero = shift_distance(left, right, 0, 0, motion.as_deref(), total_weight);
    if best.2.is_finite() && zero.is_finite() && best.2 + 0.002 < zero {
        best
    } else {
        (0, 0, zero)
    }
}

/// 变化区域权重：静止背景给 0，动得越明显权重越高。
fn motion_weights(left: &GrayFrame, right: &GrayFrame) -> Option<Vec<u8>> {
    let mut weights = vec![0u8; left.pixels.len()];
    let mut moving = 0usize;
    for (index, (a, b)) in left.pixels.iter().zip(&right.pixels).enumerate() {
        let diff = a.abs_diff(*b);
        if diff >= 4 {
            // 差 4 以下基本是压缩噪声；差 32 左右就接近满权重。
            weights[index] = (((diff - 3) as u16 * 9).min(255)) as u8;
            moving += 1;
        }
    }
    if moving * 100 < weights.len().max(1) {
        None
    } else {
        Some(weights)
    }
}

fn shift_distance(
    left: &GrayFrame,
    right: &GrayFrame,
    dx: i32,
    dy: i32,
    weights: Option<&[u8]>,
    total_weight: u64,
) -> f32 {
    let width = left.width as i32;
    let height = left.height as i32;
    let x0 = (-dx).max(0);
    let x1 = (width - dx).min(width);
    let y0 = (-dy).max(0);
    let y1 = (height - dy).min(height);
    if x1 <= x0 || y1 <= y0 {
        return f32::INFINITY;
    }
    let mut sum = 0u64;
    let mut weight_sum = 0u64;
    for y in y0..y1 {
        let row = y as usize * left.width;
        let shifted_row = (y + dy) as usize * right.width;
        for x in x0..x1 {
            let index = row + x as usize;
            let weight = weights.map_or(255u64, |values| values[index] as u64);
            if weight == 0 {
                continue;
            }
            let shifted = shifted_row + (x + dx) as usize;
            let diff = left.pixels[index].abs_diff(right.pixels[shifted]) as u64;
            sum += diff * weight;
            weight_sum += weight;
        }
    }
    // 大位移下只比较了一小块：样本太少时宁可判为不可用，别拿角落的巧合当接缝。
    if weight_sum == 0 || weight_sum * 4 < total_weight {
        return f32::INFINITY;
    }
    sum as f32 / (weight_sum as f32 * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: usize, height: usize, shift: i32) -> GrayFrame {
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let source_x = (x as i32 - shift).rem_euclid(width as i32);
                // 横向周期 16：平移 16px 后图案完全相同，方便测“找到周期”。
                let value = (((source_x.rem_euclid(16)) * 15 + (y as i32 % 8) * 7) % 251) as u8;
                pixels.push(value);
            }
        }
        GrayFrame {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn 位移估计能找到整体平移() {
        let left = pattern(64, 36, 0);
        let right = pattern(64, 36, 5);
        let (dx, dy, score) = estimate_translation(&left, &right);
        assert_eq!((dx, dy), (5, 0));
        assert!(score < 0.02, "score={score}");
    }

    #[test]
    fn 能找到重复周期() {
        // 10fps、每帧平移 1px、16 帧一个周期。
        let frames: Vec<GrayFrame> = (0..80).map(|index| pattern(64, 36, index)).collect();
        let seam = find_seam(&frames, 10.0).expect("should find a loop");
        let lag = seam.end_frame - seam.start_frame;
        // 位移补偿允许“差了半个周期”的接缝也对上，所以不要求 lag 正好是 16 的倍数。
        assert!(lag >= 16, "lag={lag}");
        assert!(seam.score < 0.05, "score={}", seam.score);
    }

    #[test]
    fn 静止画面不会硬凑出位移() {
        let frame = pattern(64, 36, 0);
        let (dx, dy, _) = estimate_translation(&frame, &frame);
        assert_eq!((dx, dy), (0, 0));
        assert!(motion_weights(&frame, &frame).is_none());
    }
}
