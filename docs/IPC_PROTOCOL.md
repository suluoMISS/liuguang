# IPC 协议规范（v1）

守护进程（`liuguang`）与客户端（`liuguangctl`、`liuguang-tui`、`liuguang-gui` 及第三方脚本）通过 Unix Domain Socket 通信。协议定义在 [`crates/liuguang-ipc/`](../crates/liuguang-ipc)，服务端实现位于 [`apps/liuguang/src/daemon.rs`](../apps/liuguang/src/daemon.rs)。

## 1. 传输与授权

- **Socket 路径**：
  - 桌面模式（`user`）：`$XDG_RUNTIME_DIR/liuguang/control.sock`
  - 无桌面模式（`system`）：`/run/liuguang/control.sock`
- **编码格式**：NDJSON（每行一条 UTF-8 JSON 消息，以 `\n` 分隔），单条上限 **1 MiB**（`MAX_MESSAGE_BYTES`）。
- **权限校验**：Socket 文件权限 `0660`，服务端通过 `SO_PEERCRED` 校验对端 UID 与补充组（读取 `/proc/<pid>/status` 的 `Groups:`），仅放行 root、守护进程同 UID 用户或 `liuguang-ctl` 组成员。

---

## 2. 消息格式

### 请求（Request）

```json
{"v":1,"id":"req-1","method":"set_wallpaper","params":{"path":"/srv/wallpapers/a.mp4","output":"DP-1","fit":"cover","brightness":0.85}}
```

### 响应（Response）

成功：
```json
{"v":1,"id":"req-1","ok":true,"result":{"accepted":true,"request_id":"req-1"}}
```

失败：
```json
{"v":1,"id":"req-1","ok":false,"error":{"kind":"NOT_CONFIGURED","message":"未配置 [library]（媒体库功能未启用）","detail_code":"NOT_CONFIGURED"}}
```

主要错误码（`ErrorKind`）：`UNSUPPORTED_VERSION`、`INVALID_MESSAGE`、`UNSUPPORTED_METHOD`、`MESSAGE_TOO_LARGE`、`INVALID_PARAMS`、`UNKNOWN_OUTPUT`、`UNSUPPORTED_CODEC`、`NEEDS_PREPARATION`、`FILE_UNAVAILABLE`、`DEVICE_UNAVAILABLE`、`INTERNAL`、`SERVER_BUSY`、`BUSY`、`PERMISSION_DENIED`、`NOT_CONFIGURED`。

### 服务端推送事件（Event）

调用 `subscribe` 后，服务端在同一连接按事件推送无 `id` 的 JSON 帧：

| 事件 `kind` | 触发时机 | 关键字段 |
|---|---|---|
| `STATE_CHANGED` | 顶层状态机变更 | `state` |
| `OUTPUT_CHANGED` | 显示器热插拔 / 模式变化 | `output`、`connected`、`mode` |
| `MEDIA_CHANGED` | 输出通道当前媒体切换 | `output`、`media` |
| `WALLPAPER_APPLIED` | `set_wallpaper` 真正上屏或失败确认 | `request_id`、`output`、`ok`、`error` |
| `SCAN_PROGRESS` / `SCAN_FINISHED` | 媒体库后台扫描进度与完成 | `scan_id`、`found` |
| `MEDIA_PREPARED` | 自动兼容转码进度与完成 | `media`、`cache_path` |
| `ERROR` | 运行时异步故障（支持 `conflicts` 字段） | `code`、`message` |

---

## 3. 方法总览

- **状态与输出**：`status`、`outputs`、`get_metrics`、`subscribe`
- **壁纸控制**：`set_wallpaper`、`next`、`set_pinned`、`pause`、`resume`、`transition`、`set_output_transform`（支持实时 `fit`/`zoom`/`offset_x`/`offset_y`/`rotation`/`brightness`）、`set_static_wallpaper`、`clear_static_wallpaper`
- **媒体库**：`list_sources`、`add_source`、`remove_source`、`scan_source`、`rescan_library`、`list_media`、`get_media`、`get_thumbnail`、`prepare_media`、`delete_media`
- **播放列表**：`list_playlists`、`update_playlist`、`delete_playlist`
- **配置管理**：`reload`、`save_config`

---

## 4. Python 调用示例

```python
import json, os, socket

sock = os.path.join(os.environ["XDG_RUNTIME_DIR"], "liuguang", "control.sock")
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(sock)

    def call(method, params=None, req_id="1"):
        msg = {"v": 1, "id": req_id, "method": method, "params": params or {}}
        s.sendall((json.dumps(msg) + "\n").encode("utf-8"))
        buf = b""
        while not buf.endswith(b"\n"):
            buf += s.recv(65536)
        return json.loads(buf)

    print(call("status")["result"]["state"])
    print(call("set_output_transform", {"output": "eDP-1", "brightness": 0.75}))
```
