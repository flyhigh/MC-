//! 「从文件导入」用的系统文件选择框。
//!
//! 为什么用它、而不是自己画一个:选文件在 Windows 上有既定的交互(最近位置、快速访问、
//! 搜索、侧栏),自己画一个只会更难用;而 COM 那套 `IFileOpenDialog` 要多一次 COM 初始化
//! 和几个接口样板,我们不需要它的额外能力(自定义控件、收藏夹)。
//!
//! ## 两个坑
//!
//! - **它是个模态对话框**。`GetOpenFileNameW` 自己跑一个消息循环,直到用户选完才返回:
//!   这段时间主循环不转,桌面壁纸停在最后一帧。用户此刻本来就在挑文件,冻结是可以接受的;
//!   但**别在自动化的路径上调它** —— 没人点它就会一直挂着。
//! - **`lpstrFile` 那块缓冲既进又出**:进去时是"默认文件名"(我们不放),出来的是**完整路径**。
//!   给小了会直接失败,所以按 `MAX_PATH` 的四倍给(路径可能很长)。
//!
//! `OFN_NOCHANGEDIR` 是必须的:不加的话这个对话框会把**进程的当前目录**改成用户最后浏览的
//! 那个目录,之后任何相对路径都会莫名其妙地跑到别处去。

use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

/// 对话框里的文件类型。`\0` 分隔"说明 / 通配符"两组一对,最后再补一个空串收尾。
///
/// 只列 PNG / JPEG:**解码器也只认这两种**(见 `mcwp-image` 的依赖说明),把 webp / bmp
/// 列进来只会让用户选完了才收到一句"读不出来"。
const FILTER: &str = "图片 (*.png;*.jpg;*.jpeg)\0*.png;*.jpg;*.jpeg\0所有文件 (*.*)\0*.*\0";

/// 让用户挑一张图。取消 / 出错都返回 `None`。
pub fn pick_image() -> Option<PathBuf> {
    let mut file: Vec<u16> = vec![0; 4096];
    let filter: Vec<u16> = FILTER
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "挑一张图片当壁纸"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };

    // 这里必须 unsafe 地传可变指针:上面那几个字段都是裸指针,而赋值时借用检查看不到
    // "它们指向的缓冲活得比 ofn 长"这层关系(实际的保证写在注释和这份缓冲的作用域里)。
    let picked = unsafe { GetOpenFileNameW(&mut ofn) };
    if !picked.as_bool() {
        // 取消和出错都是 false。**不区分**:这里报错也帮不上用户(窗口都关了),
        // 真正的失败(比如路径不存在)在调用方那次 `std::fs::read` 上会说得更清楚。
        return None;
    }

    let len = file.iter().position(|ch| *ch == 0).unwrap_or(file.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&file[..len])))
}