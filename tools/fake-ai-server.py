#!/usr/bin/env python3
r"""假的 OpenAI 兼容模型服务 —— 给「对话 → 壁纸规格」这条链路做离线自检。

为什么要有它:真模型要 key、要外网,而且同一句话两次给的结果还不一样,回归测试没法指望它。
这个服务只认 `POST /chat/completions`,把提示词里的关键词翻成一份固定形状的 spec,行为完全确定。

用法:

    python tools/fake-ai-server.py --port 8787

    # 另一个终端:不用建窗口,直接看模型给回来的东西
    $env:MCWP_AI_KEY = "fake"
    $env:MCWP_AI_BASE_URL = "http://127.0.0.1:8787/v1"
    $env:MCWP_AI_MODEL = "fake-1"
    .\app\target\release\mcwp-wallpaper.exe --ai-selftest "把壁纸调冷一点"

还能让它演坏(用来验重试 / 报错 / 降级三条路):

    --mode good    正常返回(默认)
    --mode broken  先返回半截 JSON,第二次才正常 —— 验"带错误重试一次"
    --mode html    返回一坨 HTML(像被网关挡住那样)—— 验报错与本地降级
    --mode slow    每次都慢 3 秒 —— 验等待态
    --mode bad     返回字段越界 / 预设不存在的 spec —— 验 `normalize()` 夹紧

`--mode broken` 这类会数请求次数,写进 `--log` 指定的文件(默认 fake-ai.log),方便断言"到底打了几次"。
"""

import argparse
import json
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# 四个预设各自的参数名。写死在这里是故意的:它就是要当"照着契约回话"的对端,
# 真接上模型后这份契约由 mcwp-spec 的 catalog() 维护。
PRESETS = {
    "rainy_window_city": ("雨夜城市", ["rain", "blur", "parallax"]),
    "minimal_paper": ("极简纸感", ["warmth", "grain", "softness"]),
    "nebula_drift": ("星云漂流", ["density", "swirl", "glow"]),
    "focus_grid": ("专注网格", ["grid", "scan", "contrast"]),
}

# 关键词 → 预设。和 Rust 侧 `match_preset()` 的降级规则故意保持一致,
# 这样"模型这条路"和"降级那条路"在测试里能互相印证。
RULES = [
    (["雨", "夜", "城市", "窗", "rain", "city", "night"], "rainy_window_city"),
    (["星", "宇宙", "银河", "极光", "云", "space", "nebula", "galaxy"], "nebula_drift"),
    # 别写单字"纸":"壁纸"里就带一个,一写就把所有请求都判成极简纸感
    (["极简", "留白", "素色", "纸感", "纸张", "几何", "minimal", "paper"], "minimal_paper"),
    (["专注", "网格", "写代码", "工作", "学习", "focus", "grid", "code"], "focus_grid"),
]

PALETTES = {
    "rainy_window_city": ["#0B1020", "#1B2A4A", "#4F7CFF", "#9BB8FF", "#E6ECFF"],
    "minimal_paper": ["#F3EEE4", "#E4D9C6", "#C9B79A", "#8A7A63", "#4A4238"],
    "nebula_drift": ["#05060F", "#14103A", "#5B2E9E", "#C46BD8", "#F2D6FF"],
    "focus_grid": ["#070B12", "#12202E", "#2E6F8E", "#7FD4E8", "#DFF6FF"],
}


def pick_preset(text: str) -> str:
    lowered = text.lower()
    for words, preset in RULES:
        if any(word in lowered for word in words):
            return preset
    return "rainy_window_city"


def pick_mode(text: str) -> str:
    tweak_words = ["一点", "稍微", "别那么", "调", "改", "更"]
    return "tweak" if any(word in text for word in tweak_words) else "create"


def build_spec(text: str, broken_values: bool) -> dict:
    preset = pick_preset(text)
    title, params = PRESETS[preset]
    params_out = (
        # 越界值:验证 normalize() 会夹回 0~1
        {key: 4.2 for key in params}
        if broken_values
        else {key: 0.5 for key in params}
    )
    spec = {
        "schema": 1,
        "title": f"{title} · 改",
        "source": "conversation",
        "keywords": [word for word in re.split(r"[\s，,。.、!！?？]+", text) if word][:4],
        "mood": {"calm": 0.6, "focus": 0.5, "energy": 0.3},
        "palette": PALETTES[preset],
        "scene": {"preset": preset, "params": params_out},
        "motion": {"intensity": 0.4, "speed": 0.5},
        "lighting": {"mode": "auto_ambient", "keyLight": 0.5, "followDesktopBrightness": True},
        "elements": [{"type": "particles", "count": 80}],
        "overlay": {"clock": False},
        "seed": 20261015,
    }
    if broken_values:
        # 预设名也写错:normalize() 该把它拉回默认预设
        spec["scene"]["preset"] = "no_such_preset"
        spec["palette"] = ["red", "#GGGGGG"]  # 非法色值走预设兜底
    return spec


def build_reply(text: str, mode: str, broken_values: bool) -> dict:
    spec = build_spec(text, broken_values)
    body = {
        "reply": f"按你说的「{text[:12]}」换了一张。",
        "mode": pick_mode(text),
        "image_prompt": f"{spec['title']},冷色调,雨夜氛围,电影感",
        "spec": spec,
    }
    return body


class Handler(BaseHTTPRequestHandler):
    mode = "good"
    log_path = "fake-ai.log"
    requests = 0

    def log_message(self, *args):  # 别把访问日志刷到终端上
        pass

    def count(self, note: str):
        Handler.requests += 1
        with open(self.log_path, "a", encoding="utf-8") as handle:
            handle.write(f"{Handler.requests}\t{note}\n")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode("utf-8", "replace")
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            payload = {}
        messages = payload.get("messages", [])
        users = [m.get("content", "") for m in messages if m.get("role") == "user"]
        # 重试那轮会多一条 user 消息(内容是"上面那份 JSON 有问题…"),拿它判断这是第几次
        is_retry = len(users) > 1 and "有问题" in users[-1]
        # 用户原话永远在**第一条** user 消息里;后几条都是我们在追问
        first = users[0] if users else ""
        original = first.split("【用户说】")[-1].strip() or first
        original = original.split("\n")[0]

        if self.mode == "slow":
            time.sleep(3.0)

        if self.mode == "html":
            self.count("html")
            body = b"<html><body>502 Bad Gateway</body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if self.mode == "broken" and not is_retry:
            self.count("broken-first")
            # 半截 JSON:正好落进"提取出来但解析不了"那条路
            content = '{"reply": "好", "spec": {"title": "半截'
        else:
            note = "retry-ok" if (self.mode == "broken" and is_retry) else self.mode
            self.count(note)
            content = json.dumps(
                build_reply(original, self.mode, broken_values=(self.mode == "bad")),
                ensure_ascii=False,
            )

        body = json.dumps(
            {
                "id": "fake-1",
                "object": "chat.completion",
                "model": payload.get("model", "fake-1"),
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            },
            ensure_ascii=False,
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description="假的 OpenAI 兼容模型服务(自检用)")
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument(
        "--mode",
        default="good",
        choices=["good", "broken", "html", "slow", "bad"],
        help="good 正常 / broken 第一次半截 JSON / html 返回 HTML / slow 慢 3 秒 / bad 越界的 spec",
    )
    parser.add_argument("--log", default="fake-ai.log", help="请求计数写这儿")
    args = parser.parse_args()

    Handler.mode = args.mode
    Handler.log_path = args.log
    Handler.requests = 0
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"假模型服务已起:http://127.0.0.1:{args.port}/v1  mode={args.mode}  日志={args.log}")
    print("按 Ctrl+C 退出")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n再见")


if __name__ == "__main__":
    main()
