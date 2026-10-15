"""把面板用到的字符抽出来,从 Noto Sans SC(OFL)子集化出一份内嵌字体。

为什么要内嵌子集字体:Slint 的软件渲染器用 fontdue 按 fontdb 查到的字面**一次性**解析
整份字体的全部字形轮廓,开销远大于字体文件本身。实测微软雅黑(18.8 MB)当默认字体会让
面板进程多占 ~320 MB;换成只含面板字符的子集(几十 KB)后可以忽略。

同时把字体族改名成 `MCWP Sans`:
  * 避免和用户机器上已装的 Noto Sans SC 撞名(fontdb 可能挑到那份 10 MB 的完整字体);
  * OFL 要求修改版不要沿用上游族名。

重新生成(改过面板文案后要跑一次,脚本会顺便检查有没有缺字):
    python tools/subset-panel-font.py

产物:
    app/crates/mcwp-ui/fonts/panel-cjk-regular.ttf
    app/crates/mcwp-ui/fonts/panel-cjk-bold.ttf
    app/crates/mcwp-ui/fonts/panel-charset.txt     # 覆盖到的字符,Rust 侧查缺字用
"""

import glob
import os
import re
import sys
import urllib.request

from fontTools.ttLib import TTFont
from fontTools.subset import main as subset_main

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "app")
OUT_DIR = os.path.join(APP, "crates", "mcwp-ui", "fonts")
CACHE = os.path.join(ROOT, ".fontcache")

FAMILY = "MCWP Sans"
WEIGHTS = {"regular": (400, "Regular"), "bold": (700, "Bold")}

SOURCE = "https://cdn.jsdelivr.net/npm/@fontsource/noto-sans-sc@5.0.18/files/noto-sans-sc-chinese-simplified-{}-normal.woff2"
LICENSE_URL = "https://cdn.jsdelivr.net/npm/@fontsource/noto-sans-sc@5.0.18/LICENSE"

# 面板文案一定用得到的西文/符号。
EXTRA = (
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    " .,:;!?()[]{}<>+-*/=_%#@&|\\'\"~^`$\u3000\u00b7\u2014\u2026\u3001\u3002\uff0c\uff1a\uff1b\uff01\uff1f\u300c\u300d\u3010\u3011\u00d7"
)

# 为 M1/M2 预留的状态词。多放几个字只多几百字节,但能少一次"缺字→重跑脚本"。
RESERVE = (
    "已为保存失败恢复默认当前预设场景参数关键词指纹亮度动势桌面环境工作状态"
    "对话记录分析正在加载中请稍候完成成功错误重试连接网络超时未找到无法生成"
    "替换应用关闭打开设置退出帮助关于版本更新检查主题模式浅色深色自动时间"
    "日期天气温度心情情绪安静专注放松学习阅读写作编程会议休息清晨上午下午"
    "傍晚夜晚深夜今天明天昨天分钟小时秒钟第个共余项张幅次回车确认取消是否可用"
    "墙壁纸库管理导入导出推荐随机切换刷新同步服务器密钥模型回复提问发送消息"
)

def cjk_base() -> set:
    """GB2312 一级汉字(3755 个常用字)。

    为什么整表塞进来:面板上有个输入框,用户敲什么字都得画得出来 —— 子集字体缺字是
    静默不画(不报错、不回落),聊天框里冒空白比多占 1 MB 难查得多。GB2312 一级字是
    "常用汉字"最省事的定义:不用联网拉字频表,`gb2312` 编解码器就在标准库里,
    覆盖日常中文绰绰有余。生僻字(人名、化学名)仍会缺,缺字由 Rust 侧的
    `missing_chars()` 记进日志。
    """
    chars = set()
    for high in range(0xB0, 0xD8):
        for low in range(0xA1, 0xFF):
            try:
                chars.add(bytes([high, low]).decode("gb2312"))
            except UnicodeDecodeError:
                continue
    return chars


# 只出现在日志/注释里、面板不会显示的字符。源字体没有也不影响面板,列在这里免得脚本报错。
IGNORE_MISSING = "\u2192\u2190\u2264\u2265\u00d7"

# 面板上会显示、但文案写在 Rust 里的源文件。
RUST_SOURCES = [
    "crates/mcwp-spec/src/presets.rs",
    "crates/mcwp-spec/src/lib.rs",
    "crates/mcwp-ui/src/lib.rs",
    "crates/mcwp-ai/src/lib.rs",
    "crates/mcwp-wallpaper/src/main.rs",
]


def slint_charset() -> set:
    """面板 .slint 里所有字符串字面量 + 注释(注释里也常写了给用户看的文案)。"""
    path = os.path.join(APP, "crates", "mcwp-ui", "ui", "panel.slint")
    text = open(path, encoding="utf-8").read()
    chars = set()
    for literal in re.findall(r'"((?:[^"\\]|\\.)*)"', text):
        chars.update(literal)
    for comment in re.findall(r"//[^\n]*", text):
        chars.update(comment)
    return chars


def rust_charset() -> set:
    """Rust 侧的字符串字面量(预设标题、状态提示这些会原样送到面板上)。"""
    chars = set()
    for rel in RUST_SOURCES:
        path = os.path.join(APP, rel)
        if not os.path.exists(path):
            continue
        text = open(path, encoding="utf-8").read()
        for literal in re.findall(r'"((?:[^"\\]|\\.)*)"', text):
            chars.update(literal)
    return chars


def spec_charset() -> set:
    """示例 spec 里的标题 / 关键词 / 情绪词,AI 生成壁纸时容易复用。"""
    chars = set()
    for path in glob.glob(os.path.join(APP, "specs", "*.json")):
        chars.update(open(path, encoding="utf-8").read())
    return chars


def charset() -> set:
    """regular 字面要整份:面板上有输入框,用户敲什么字都得画得出来(见 cjk_base 的说明)。"""
    return {c for c in (static_charset() | cjk_base()) if c.isprintable()}


def static_charset() -> set:
    """面板自己的文案(静态):.slint 的字面量与注释、Rust 侧字面量、预留状态词、示例 spec。

    这份是 bold 字面的全部内容 —— bold 只用来画标题/标签这些写死的字。
    动态文案(会话名、消息、输入框、壁纸名)一律走 regular(见 panel.slint 顶部的字重约定),
    所以 bold 不必背 GB2312 整表:一个字体面会被整份解析进内存,收窄它就是十几 MB。
    """
    chars = (
        set(EXTRA)
        | set(RESERVE)
        | slint_charset()
        | rust_charset()
        | spec_charset()
    )
    return {c for c in chars if c.isprintable()}


def cmap_of(font: TTFont) -> set:
    cmap = set()
    for table in font["cmap"].tables:
        cmap.update(table.cmap.keys())
    return cmap


def fetch(url: str, dest: str) -> str:
    if os.path.exists(dest):
        return dest
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    with urllib.request.urlopen(url, timeout=120) as response:
        data = response.read()
    with open(dest, "wb") as handle:
        handle.write(data)
    return dest


def source_ttf(weight: int) -> str:
    """拿到上游完整字体(woff2 解成 ttf),顺便缓存下来给下次用。"""
    cached = fetch(SOURCE.format(weight), os.path.join(CACHE, f"noto-sans-sc-{weight}.woff2"))
    font = TTFont(cached)
    ttf = os.path.join(CACHE, f"noto-sans-sc-{weight}.ttf")
    font.flavor = None
    font.save(ttf)
    return ttf


def rename(ttf_path: str, weight: int, style: str) -> None:
    """把字体改名成 FAMILY,顺便把 name 表收敛成一套记录。"""
    font = TTFont(ttf_path)
    wanted = {
        1: FAMILY,
        2: style,
        3: f"{FAMILY}:{style}",
        4: f"{FAMILY} {style}",
        6: f"MCWPSans-{style}",
        16: FAMILY,
        17: style,
    }
    for record in font["name"].names:
        if record.nameID in wanted:
            # platform 0/3 是 UTF-16BE,platform 1(Mac)用 latin-1(这些名字都是纯 ASCII)。
            encoding = "latin-1" if record.platformID == 1 else "utf-16-be"
            record.string = wanted[record.nameID].encode(encoding)
    font["OS/2"].usWeightClass = weight
    font.save(ttf_path)


def main() -> int:
    # regular 画动态文案(整份),bold 只画面板自己的静态文案 —— 见 static_charset() 的说明。
    per_weight = {"regular": charset(), "bold": static_charset()}
    chars = per_weight["regular"]
    os.makedirs(OUT_DIR, exist_ok=True)
    print(f"面板字符集:{len(chars)} 个字符")
    print(f"  其中 bold(静态文案)要 {len(per_weight['bold'])} 个")

    # 源字体里没有的字符没法子集化,提前查出来:除了明确忽略的,其余都是硬错误。
    sources = {name: source_ttf(weight) for name, (weight, _) in WEIGHTS.items()}
    available = cmap_of(TTFont(sources["regular"]))
    # 常用字表按"源字体里有没有"过滤:差的那几个异体字不值得为它中止一次构建。
    skipped = [c for c in chars if c not in IGNORE_MISSING and ord(c) not in available]
    if skipped:
        print(f"   (源字体没有的 {len(skipped)} 个字符已跳过:{''.join(sorted(skipped)[:20])}")
    subset_chars = sorted(c for c in chars if ord(c) in available)

    for name, (weight, style) in WEIGHTS.items():
        weight_chars = sorted(c for c in per_weight[name] if ord(c) in available)
        out = os.path.join(OUT_DIR, f"panel-cjk-{name}.ttf")
        subset_main([
            sources[name],
            f"--text={''.join(weight_chars)}",
            "--output-file=" + out,
            "--layout-features=",          # 面板不需要连字/字距特性
            "--no-hinting",
            "--desubroutinize",
            "--drop-tables+=DSIG",
            "--name-IDs=*",
            "--recalc-bounds",
        ])
        rename(out, weight, style)

        font = TTFont(out)
        covered = cmap_of(font)
        missing = [c for c in weight_chars if ord(c) not in covered]
        if missing:
            print(f"  !! {os.path.basename(out)} 缺字 {len(missing)} 个:{''.join(missing[:40])}")
            return 1
        print(f"  {os.path.basename(out)}:{font['maxp'].numGlyphs} 字形 / "
              f"{os.path.getsize(out) / 1024:.1f} KB / 族名={font['name'].getDebugName(1)!r}")

    # Rust 侧查缺字用:面板字体到底覆盖了哪些字符。
    charset_txt = os.path.join(OUT_DIR, "panel-charset.txt")
    with open(charset_txt, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("".join(subset_chars))
    print(f"覆盖字符表:{charset_txt}")

    license_path = os.path.join(OUT_DIR, "OFL.txt")
    if not os.path.exists(license_path):
        with urllib.request.urlopen(LICENSE_URL, timeout=120) as response:
            data = response.read()
        with open(license_path, "wb") as handle:
            handle.write(data)
    print(f"许可文件:{license_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
