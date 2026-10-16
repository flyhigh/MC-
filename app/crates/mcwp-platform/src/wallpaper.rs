//! 读「现在桌面上那张壁纸是哪张图」。
//!
//! 为什么要它:生图要花钱、要等十几秒,而用户机器上本来就压着一张静态壁纸 ——
//! 把那张图直接抬进渲染管线,当场就得到一张「能动的」壁纸(推拉 + 效果层),
//! 不必先等 AI 出图。它同时也是**库空着时的兜底**:我们的窗口是盖在桌面上的,
//! 什么都不画就是一屏黑,而"把用户原来那张抬上来"看不出接缝。
//!
//! 拿的是**文件路径**,不是像素:Windows 把用户选的那张图记在
//! `HKCU\Control Panel\Desktop\Wallpaper`,这里读的是同一个值(`SPI_GETDESKWALLPAPER`)。
//! 解码交给调用方(壁纸侧有 `mcwp-image`),这个模块只管"是哪张"。
//!
//! 坑(逐个踩过):
//!   * 纯色背景 / 幻灯片 / 被组策略接管时,返回的是空串 —— **那不是错误,是"没有图"**,
//!     所以这里返回 `None` 而不是 `Err`,调用方也就少一层错误分支。
//!   * 返回的路径**不保证还存在**:用户删了源文件、或者那张图在外接盘上没插。
//!     调用方要自己处理"文件读不出来"。
//!   * 从「设置」里选的壁纸,系统会把原图**转码**成一份 `…\Themes\TranscodedWallpaper`,
//!     而返回的就是**它** —— 这个文件**没有扩展名**。所以调用方绝不能按扩展名猜格式
//!     (只有"按文件头认格式"那条路走得通),也别指望文件名能当标题用。
//!   * 有些机器上存的是**带环境变量**的路径(`%SystemRoot%\web\wallpaper\…`),Windows 读出来
//!     原样给你。这里顺手展开一次:`is_file()` 拿到 `%…%` 只会返回 false,而"文件不见了"
//!     和"路径没展开"在调用方看是同一个症状,排查起来很费劲。
//!   * 也可以是 **.jpg / .png / .bmp / .webp** 里的任意一种(Windows 自己转码,
//!     原文件是什么格式就返回什么格式)—— 所以解码要走"按文件头认格式"那条路,
//!     不能按扩展名猜。
//!   * `SPI_GETDESKWALLPAPER` 的 `uiParam` 要的是**缓冲区的字符数**(不是字节数),
//!     而它**不会**告诉你真实长度,只会把字符串填进去 —— 靠找 NUL 截断。
//!   * 路径是 UTF-16。用 `from_utf16_lossy` 转:带 emoji 的路径会掉字,
//!     但那种路径本来就打不开,掉字比直接报错更有用。

use std::path::PathBuf;

use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// `MAX_PATH` 的宽字符数。Windows 的壁纸路径不会超过它(系统自己就写不进更长的值)。
const BUF_CHARS: usize = 260;

/// 当前壁纸的文件路径。没有(纯色背景)、读不到(策略锁定)、路径为空 —— 一律 `None`。
pub fn current_image_path() -> Option<PathBuf> {
    let mut buffer = vec![0u16; BUF_CHARS];
    // `fWinIni` 在这个 action 下不用,填 0。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return None;
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    if len == 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buffer[..len]);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(expand_vars(text)))
}

/// 展开路径里的 `%NAME%`。没有 `%` 就原样返回(绝大多数机器都走这一条)。
///
/// 自己展开而不是调 `ExpandEnvironmentStringsW`,是因为要处理的就这一种形式、也只做一次 ——
/// 为它拉一个 Win32 调用不划算,而且那样还得处理第二个返回值(填进缓冲区的长度)。
/// 变量名按**不区分大小写**找,因为 Windows 的变量本来就不区分(`%systemroot%` 也得认)——
/// 这一点由 `std::env::var` 在 Windows 上自带,这里不用自己绕。
fn expand_vars(text: &str) -> String {
    if !text.contains('%') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            // 落单的 `%`:它不是一个变量,原样留着
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            // 认不出来的变量**原样留着**:让它自己"找不到文件",比在这里悄悄吞掉一段更好查
            _ => {
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只验"能调通、不 panic、路径格式像样"。
    ///
    /// 不去断言"一定有值":CI 机器上可能是纯色背景(那时它就该是 `None`),
    /// 而"必须有值"的断言会把这种正常情形判成失败。
    ///
    /// **故意不断言"带扩展名"**:从「设置」里选的壁纸,返回的是转码件
    /// `…\Themes\TranscodedWallpaper` —— 它没有扩展名(这条踩过,见文件头)。
    #[test]
    fn reads_without_panicking() {
        match current_image_path() {
            Some(path) => {
                assert!(path.is_absolute(), "壁纸路径应该是绝对路径:{path:?}");
                assert!(
                    !path.to_string_lossy().contains('%'),
                    "路径里的环境变量应该已经展开:{path:?}"
                );
            }
            None => {
                // 纯色背景 / 无桌面会话:合法状态,不是失败
            }
        }
    }

    #[test]
    fn 展开环境变量() {
        assert_eq!(expand_vars("C:\\plain.jpg"), "C:\\plain.jpg");
        let expanded = expand_vars("%SystemRoot%\\web\\wallpaper\\img0.jpg");
        assert!(expanded.ends_with("\\web\\wallpaper\\img0.jpg"));
        assert!(!expanded.starts_with('%'), "认得出的变量要展开:{expanded}");
        // 落单的 `%` 和不认识的变量都原样留着,不能吞掉
        assert_eq!(expand_vars("50%"), "50%");
        assert_eq!(expand_vars("%NOPE_NOT_SET%\\a"), "%NOPE_NOT_SET%\\a");
    }
}
