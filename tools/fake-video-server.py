#!/usr/bin/env python3
r"""假的「图生视频」服务 —— 给会话里那颗「生成动图」按钮做离线自检。

为什么要有它:这条链路真要跑一次是 **1 块钱 + 几分钟**(见方案里 `--animate-save` 那段),
而且服务商那边排队多久完全不可控。按钮这种"点一下要立刻有反应"的交互,回归测试既不能
靠花钱、也不能靠等运气,所以把服务端换成一个行为完全确定的本端假服务。

它认两个端点(和 `mcwp_ai::Videos` 的 `submit` / `poll` 一一对应):

    POST /v1/videos/generations   -> {"id": "fake-1"}
    GET  /v1/async-result/fake-1  -> 先若干次 {"task_status": "RUNNING"},之后 SUCCESS

用法:

    python tools/fake-video-server.py --port 8799 --running 2
    # 另一个终端(环境变量覆盖 ai.json,别动用户的真配置):
    $env:MCWP_VIDEO_BASE_URL = "http://127.0.0.1:8799/v1"
    $env:RUST_LOG = "info"
    .\app\target\release\mcwp-wallpaper.exe --panel

`--running N` 是"先说 N 次还在跑" —— 真链路里轮询间隔 8 秒(`VIDEO_POLL_INTERVAL`),
所以 N=2 意味着"正在生成动图…"这个状态会**稳稳停住 16 秒**,够截图看清楚了。
`--hold` 则是永远不出片,专门用来把那个状态钉住。
`--video <mp4>` 指定出片时给哪一段(默认拿本机壁纸库里的第一段 mp4)——
走完之后主线程要解封面、入库,那条路只有给一段**真 mp4** 才能验到底。

请求体(几兆的 base64 图)不解析,只读掉收工 —— 这里要验的是客户端的行为,不是它的 payload。
收了多少字节、被叫了几次,都写进 `--log`(默认 fake-video.log),方便断言"到底打了几次"。
"""

import argparse
import glob
import json
import os
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# 被轮询了几次、有没有服务过视频,这些是跨请求的,得挂在模块上
COUNTS = {}
LOCK = threading.Lock()
ARGS = None


def library_sample():
    """默认出片用的那段 mp4:本机壁纸库里现成的一段。"""
    root = os.path.join(os.environ.get("APPDATA", ""), "MC壁纸", "photos")
    found = sorted(glob.glob(os.path.join(root, "*.mp4")))
    return found[0] if found else None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # 别把每条请求都糊到 stderr 上
        pass

    def _say(self, code, payload, ctype="application/json"):
        body = json.dumps(payload).encode() if ctype == "application/json" else payload
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _log(self, line):
        with open(ARGS.log, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        # 只读掉,不解析:请求体里是整张图的 base64,几兆,解析它没有意义
        if length:
            self.rfile.read(length)
        if not self.path.endswith("/videos/generations"):
            self._say(404, {"error": "no such endpoint"})
            return
        with LOCK:
            ARGS.seq += 1
            task = "fake-%d" % ARGS.seq
            COUNTS[task] = 0
        self._log("submit %s bytes=%d -> %s" % (self.path, length, task))
        self._say(200, {"id": task})

    def do_GET(self):
        match = re.search(r"/async-result/(.+)$", self.path)
        if match:
            task = match.group(1)
            with LOCK:
                seen = COUNTS.get(task, 0)
                COUNTS[task] = seen + 1
            if ARGS.hold or seen < ARGS.running:
                self._log("poll %s #%d -> RUNNING" % (task, seen + 1))
                self._say(200, {"id": task, "task_status": "RUNNING"})
                return
            url = "http://127.0.0.1:%d/media/sample.mp4" % ARGS.port
            self._log("poll %s #%d -> SUCCESS" % (task, seen + 1))
            self._say(200, {"id": task, "task_status": "SUCCESS",
                            "video_result": [{"url": url, "cover_image_url": ""}]})
            return
        if self.path.endswith("/media/sample.mp4"):
            if not ARGS.video or not os.path.exists(ARGS.video):
                self._say(500, {"error": "no sample video"})
                return
            with open(ARGS.video, "rb") as handle:
                blob = handle.read()
            self._log("serve video bytes=%d" % len(blob))
            self._say(200, blob, "video/mp4")
            return
        self._say(404, {"error": "no such endpoint"})


def main():
    global ARGS
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8799)
    parser.add_argument("--running", type=int, default=2,
                        help="先说几次「还在跑」,之后才出片")
    parser.add_argument("--hold", action="store_true", help="永远不出片(把状态钉住)")
    parser.add_argument("--video", default=None, help="出片时给哪一段 mp4")
    parser.add_argument("--log", default="fake-video.log")
    ARGS = parser.parse_args()
    if ARGS.video is None:
        ARGS.video = library_sample()
    ARGS.seq = 0
    if ARGS.video is None:
        print("注意:没找到可用的 mp4,出片那一步会 500(只验提交+轮询的话不影响)")
    else:
        print("出片用:%s" % ARGS.video)
    print("假的图生视频服务在 http://127.0.0.1:%d/v1 上等着" % ARGS.port)
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler).serve_forever()


if __name__ == "__main__":
    main()