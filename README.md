# MC 壁纸

Windows 桌面动态壁纸与个性化工具。壁纸窗口挂在桌面图标之下的「壁纸层」(`WorkerW`),常驻托盘;
控制面板是一块 Slint 写的 iOS 液态玻璃**生图界面**(左栏历史对话 + 右栏会话,壁纸参数 / 壁纸库 /
设置挂在左栏底部),窗口顶部是一条 36px 的安全区(自绘标题栏:程序名 + 最小化 / 放大 / 关闭,
兼作拖动区、双击放大还原);目标形态是「按工作状态 / 对话生成壁纸」+ 可交互桌宠。

- **语言**:Rust(edition 2024)
- **渲染**:wgpu → DXGI 合成交换链(`CreateSwapChainForComposition` + `IDCompositionVisual`)
- **界面**:Slint,进程内按需建窗、关窗即释放;字体子集内嵌,不依赖系统字体
- **系统集成**:Win32 原生(`Shell_NotifyIconW` 托盘 / `HKCU\...\Run` 开机自启 / DWM 窗口属性),无第三方封装

进度:M0(桌面挂载 + 着色器场景)、M1 的一部分(壁纸库、控制面板、托盘)已完成。面板已改成生图
界面,发消息、切会话、壁纸库列表都能跑通,左栏的会话**右键可以重命名 / 置顶 / 标未读 / 删除**;
壁纸库的列表由壁纸侧扫 `%APPDATA%\MC壁纸\library` 自动灌进来,点一张就切过去。AI 那一环(M3)还没接
—— 回复位现在是占位文案;会话与壁纸库都落在 `%APPDATA%\MC壁纸`,退出再开历史还在。
技术细节与实测数据见 `docxs/MC壁纸-需求与技术方案.md`。

## 构建

```powershell
cd app
cargo build --release --workspace
```

需要 Rust 1.85+。首次构建要拉 wgpu / slint,耗时和体积都不小。

## 运行

```powershell
.\target\release\mcwp-wallpaper.exe                # 常驻运行,操作入口在托盘右键菜单
.\target\release\mcwp-wallpaper.exe --panel        # 顺便打开控制面板
.\target\release\mcwp-wallpaper.exe --preset list  # 列出内置场景预设
```

其余参数(`--spec` / `--attach` / `--fps` 等,以及 `--window` / `--dump` / `--gdi-only` 这类诊断开关,
后者需要 `--features dev-tools`)见 `app/crates/mcwp-wallpaper/src/main.rs` 顶部的用法注释。

## 目录

| 路径 | 说明 |
| --- | --- |
| `app/crates/mcwp-spec` | `WallpaperSpec` 契约、校验器、内置场景预设 |
| `app/crates/mcwp-render` | wgpu 渲染器(合成交换链,离屏回读) |
| `app/crates/mcwp-platform` | 桌面挂载、托盘、开机自启、DWM 玻璃窗口 |
| `app/crates/mcwp-ui` | Slint 控制面板 + 内嵌子集字体 |
| `app/crates/mcwp-wallpaper` | 主程序:宿主窗口 + 主循环 |
| `app/specs/` | 四个内置预设导出的示例 spec |
| `docxs/` | 需求与技术方案、开源壁纸软件 / 桌宠项目调研 |
| `tools/` | 图标生成、面板字体子集化、拖动与截图回归脚本 |

## 开发备注

- 改过 `app/crates/mcwp-ui/ui/panel.slint` 里的面板文案后要重跑
  `python tools/subset-panel-font.py`。面板用的是子集字体,缺字不会报错、只会静静地画不出来。
- 面板字体分两档:**regular 是全字集**(GB2312 一级汉字,画消息 / 输入框这类动态文案),
  **bold 只收面板自己的静态文案**。所以动态文案一律不能用 `font-weight: 600`,否则静默缺字。
- 面板自己不存任何东西:会话、壁纸库、当前 spec 都在 `mcwp-wallpaper` 侧,面板只是它们的投影。
  调试时把 `RUST_LOG` 设成 `info,mcwp_wallpaper=debug` 能看到面板发回来的每个事件。
- `app/.cargo/config.toml` 把 crates.io 换成了 USTC 镜像(本机直连官方源约 16 KB/s);删掉即回退官方源。
