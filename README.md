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
壁纸库的列表由壁纸侧扫 `%APPDATA%\MC壁纸\library` 自动灌进来,点一张就切过去。M3 的第一段也接上了:
**一句话 → 模型给一份 `WallpaperSpec` → 本地着色器渲染**(模型不画图);会话与壁纸库都落在
`%APPDATA%\MC壁纸`,退出再开历史还在。模型要自己配一个 key(见下),没配也能开、也能聊,只是回复位
会提示去哪儿配。
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

## 配模型(生图那一环)

任何 **OpenAI 兼容**的服务都能用,换服务商只改一行。写一份 `%APPDATA%\MC壁纸\ai.json`:

```json
{ "base_url": "https://open.bigmodel.cn/api/paas/v4", "model": "glm-4-flash-250414", "api_key": "你的key" }
```

以上**就是内置默认**(2026-10-15 定的:先拿智谱的免费档跑)。key 去
[bigmodel.cn](https://www.bigmodel.cn/) 注册后拿,不要钱。没有这份 `ai.json` 也能正常启动 —— 只在你
发消息时回一句"还没配模型",不会拦着面板。

智谱另有一个**也免费**的 `glm-4.7-flash`,更新更强,但它属于默认开深度思考的 4.7 系列;换过去请把
`"thinking": false` 一起写上,否则一句"换张壁纸"要等十几秒:

```json
{ "base_url": "https://open.bigmodel.cn/api/paas/v4", "model": "glm-4.7-flash", "api_key": "你的key", "thinking": false }
```

`thinking` **只在显式写了才发**(`true` / `false` 都认)——别家不认这个字段,不写就完全不出现在请求体里,
和 `response_format` 同一个道理。

其他几家也都是同一套 OpenAI 兼容接口,换个 `base_url` / `model` 就行:硅基流动(注册送额度)、
OpenRouter / Groq(免费档,要能连外网)、DeepSeek(按量计费,`https://api.deepseek.com` +
`deepseek-chat`)。临时试一家也可以只设环境变量 `MCWP_AI_KEY` / `MCWP_AI_BASE_URL` / `MCWP_AI_MODEL` /
`MCWP_AI_THINKING`(优先级高于 `ai.json`)。

不用 key、不花钱也能回归这条链路:

```powershell
python tools/fake-ai-server.py --port 8787            # 假的 OpenAI 兼容服务,还能 --mode broken/bad/html/slow 演坏
python tools/fake-ai-server.py --dump req.jsonl       # 加 --dump 把收到的请求体逐条落下来,用来核对发了哪些字段
$env:MCWP_AI_KEY="fake"; $env:MCWP_AI_BASE_URL="http://127.0.0.1:8787/v1"; $env:MCWP_AI_MODEL="fake-1"
.\app\target\release\mcwp-wallpaper.exe --ai-selftest "把壁纸调冷一点"   # 只问模型一次,打印规格
.\app\target\release\mcwp-wallpaper.exe --panel --ai-ask "雨夜的城市"   # 开面板并直接发这句话
```

## 目录

| 路径 | 说明 |
| --- | --- |
| `app/crates/mcwp-spec` | `WallpaperSpec` 契约、校验器、内置场景预设 |
| `app/crates/mcwp-render` | wgpu 渲染器(合成交换链,离屏回读) |
| `app/crates/mcwp-ai` | 对话 → `WallpaperSpec`:OpenAI 兼容客户端、提示词、校验与本地降级 |
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
- 模型调用**不能写在主循环里同步等**(秒级往返 vs 60 fps):`mcwp-wallpaper` 那侧是"发请求丢后台线程 →
  结果从 `ai_rx` 回主循环落地"。面板回归别指望合成键盘输入 —— 本机中文输入法会把字母吃掉,用
  `--ai-ask` 或 `--ai-selftest`。
