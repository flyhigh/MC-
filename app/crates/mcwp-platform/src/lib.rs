/// 开机自启(注册表 `Run` 项)。
pub mod autostart;
pub mod desktop;
/// 无边框面板窗口的 DWM 收尾(圆角、暗色边框)与拖动。仅供面板这类普通窗口使用。
pub mod glass_window;
/// 任务栏托盘图标与右键菜单。
pub mod tray;
/// 读当前桌面壁纸是哪张图(生图之前的兜底画源)。
pub mod wallpaper;
/// 桌面窗口层级诊断。只在启用 `dev-tools` 特性时编译,正式运行路径不依赖它。
#[cfg(feature = "dev-tools")]
pub mod diag;
