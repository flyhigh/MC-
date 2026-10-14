"""生成 MC 壁纸的应用图标(液态玻璃风格)。

为什么用脚本画而不是丢一张位图进来:图标要在 16px(任务栏/列表)到 256px(桌面大图标)
之间都立得住,手工描一张 PNG 很难同时满足;而且它是产品的一部分,和面板一样该可复现、
可微调。配色直接沿用面板的「环境底」色系(底色 #2E4A8A → #080C1A、强调色 #5B8CFF),
所以图标和面板是一套语言。

产物:
    app/crates/mcwp-ui/assets/mcwp-icon-256.png   # 给 panel.slint 当窗口图标(内嵌进 exe)
    app/crates/mcwp-ui/assets/mcwp-icon.ico       # 多尺寸,给桌面快捷方式用

用法:
    python tools/make-icon.py
"""

import os

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "app", "crates", "mcwp-ui", "assets")

S = 1024  # 超采样画布;各尺寸都从它 LANCZOS 缩下去,边缘才干净
RADIUS = int(S * 0.2237)  # iOS 风格圆角(约 22.4%)
PANE = int(S * 0.115)  # 玻璃块离边的距离
PANE_RADIUS = int(S * 0.15)

TOP = (58, 96, 190)  # #3A60BE,比面板的环境底亮一档 —— 图标缩到 16px 还得立得住
BOTTOM = (9, 14, 32)  # #090E20
ACCENT = (110, 168, 255)  # #6EA8FF


def base_layer() -> Image.Image:
    """对角渐变 + 一团蓝色光斑,和面板的环境底同一个配方。"""
    yy, xx = np.mgrid[0:S, 0:S].astype(np.float32)
    u = xx / (S - 1)
    v = yy / (S - 1)

    t = np.clip(0.5 * (u + v) * 1.2 - 0.08, 0.0, 1.0)[..., None]
    top = np.array(TOP, np.float32)[None, None, :]
    bottom = np.array(BOTTOM, np.float32)[None, None, :]
    img = top + (bottom - top) * t

    glow_r = 0.68 * S
    dist = np.sqrt((xx - 0.32 * S) ** 2 + (yy - 0.26 * S) ** 2) / glow_r
    glow = (np.clip(1.0 - dist, 0.0, 1.0) ** 1.8 * 0.85)[..., None]
    accent = np.array(ACCENT, np.float32)[None, None, :]
    img = img + (accent - img) * glow

    return Image.fromarray(np.clip(img, 0, 255).astype(np.uint8), "RGB").convert("RGBA")


def glass_layer() -> tuple:
    """玻璃块:薄填充 + 发丝边;再单独给一层顶部高光(要模糊,所以分开)。"""
    pane = (PANE, PANE, S - PANE, S - PANE)
    rect = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rect).rounded_rectangle(
        pane,
        radius=PANE_RADIUS,
        fill=(255, 255, 255, 0x16),
        outline=(255, 255, 255, 0x52),
        width=max(2, int(S * 0.008)),
    )

    spec = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(spec).ellipse(
        (int(S * 0.13), int(S * 0.07), int(S * 0.87), int(S * 0.36)),
        fill=(255, 255, 255, 0x66),
    )
    # 斜着扫过去,比正着来一道更像液体受光
    spec = spec.rotate(-18, resample=Image.BICUBIC, center=(S / 2, S * 0.24))
    spec = spec.filter(ImageFilter.GaussianBlur(S * 0.04))

    # 高光和填充都只在玻璃块内生效,不然会糊到圆角外面去
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(pane, radius=PANE_RADIUS, fill=255)
    spec.putalpha(Image.fromarray(
        (np.asarray(spec.getchannel("A"), np.float32) * (np.asarray(mask, np.float32) / 255.0))
        .astype(np.uint8)
    ))
    return rect, spec


def build() -> Image.Image:
    img = base_layer()
    rect, spec = glass_layer()
    img = Image.alpha_composite(img, rect)
    img = Image.alpha_composite(img, spec)

    # 底缘暗线:玻璃的厚度,和小尺寸下的分隔感
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle(
        (PANE, PANE, S - PANE, S - PANE),
        radius=PANE_RADIUS,
        outline=(0, 0, 0, 0x55),
        width=max(2, int(S * 0.008)),
    )
    img = Image.alpha_composite(img, rim)

    # 圆角外裁掉,留透明角
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, S - 1, S - 1), radius=RADIUS, fill=255)
    img.putalpha(mask)
    return img


def main() -> None:
    os.makedirs(OUT_DIR, exist_ok=True)
    icon = build()

    png = os.path.join(OUT_DIR, "mcwp-icon-256.png")
    icon.resize((256, 256), Image.LANCZOS).save(png)

    ico = os.path.join(OUT_DIR, "mcwp-icon.ico")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    icon.save(ico, format="ICO", sizes=[(s, s) for s in sizes])

    for path in (png, ico):
        print(f"{os.path.relpath(path, ROOT)}  {os.path.getsize(path) / 1024:.1f} KB")


if __name__ == "__main__":
    main()
