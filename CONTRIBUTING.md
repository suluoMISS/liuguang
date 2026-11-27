# 参与贡献 · Contributing to Liuguang

感谢关注与贡献 **流光（Liuguang）**！本项目采用 **MIT License (`MIT`)** 开源，所有提交的补丁与新增代码均按此协议授权。

---

## 1. 代码库结构

仓库分为**核心 Workspace** 与**独立图形界面 Crate**：

```text
├── crates/
│   ├── liuguang-core/      # 配置解析、状态模型、媒体扫描、转码兼容性判定
│   ├── liuguang-ipc/       # Unix Socket + NDJSON 双向协议、同组权限鉴权
│   ├── liuguang-display/   # DRM/KMS 原子显示抽象与输出排布
│   ├── liuguang-wayland/   # wlr-layer-shell 壁纸层 + zwp_linux_dmabuf_v1 导入
│   ├── liuguang-render/    # 静态图缩放与可选 GLES2 转场渲染
│   ├── liuguang-media/     # 媒体解码管线（GStreamer + VA-API 适配）
│   ├── liuguang-vaapi/     # FFmpeg VA-API 硬解 + libva DMA-BUF 导出 + GLES RGB 转换
│   └── liuguang-tui/       # 终端仪表盘（Ratatui，兼容 SSH 远程控制台）
├── apps/
│   ├── liuguang/           # 核心守护进程
│   ├── liuguangctl/        # CLI 与 TUI 入口
│   └── liuguang-gui/       # 独立 GTK4 + libadwaita 图形界面（不在根 workspace 内）
├── packaging/              # systemd 单元、sysusers、Arch PKGBUILD、Fedora RPM spec、图标
└── scripts/                # CI 门禁、安装、发行包与通用端到端验证脚本
```

`apps/liuguang-gui` 故意独立于根 workspace，避免无桌面环境构建 `liuguang` / `liuguangctl` 时拉入 GTK4 依赖。

---

## 2. 编码规范（CI 硬性门禁）

根 workspace 已配置严格 lint（详见根 [`Cargo.toml`](Cargo.toml)）：

- **`unsafe_code = "deny"`**：核心架构层禁止 `unsafe`（仅 `liuguang-vaapi` / `liuguang-render` 等受控 FFI 边界按 crate 声明管理）。
- **`clippy::unwrap_used = "deny"` 与 `clippy::expect_used = "deny"`**：生产代码不得使用 `.unwrap()` 或 `.expect()`（单测内可使用），必须使用 `?`、`ok_or*` 或显式错误恢复。
- **零警告要求**：所有 Clippy 检查必须以 `-D warnings` 通过。
- **行为契约**：IPC 写操作不可静默忽略错误或“假受理”；配置与权限变更需保持桌面模式（`user`）与无桌面模式（`system`）的清晰隔离。

---

## 3. 提交前本地检查清单

```bash
# 1. 核心 workspace 默认构型 + gstreamer / gles feature 矩阵
./scripts/ci.sh

# 2. 硬件 VA-API crate 与 daemon VA-API 链路测试
cargo test -p liuguang-vaapi
cargo test -p liuguang --features vaapi --lib

# 3. 独立 GTK4 图形界面格式、Clippy 与自检单测
(cd apps/liuguang-gui && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test)

# 4. 脚本与 systemd 单元基本校验
bash -n scripts/install.sh
systemd-analyze verify packaging/liuguang.service
systemd-analyze --user verify packaging/liuguang-user.service
```

---

## 4. 问题反馈

提交 Bug 或功能建议时，请前往 <https://github.com/suluoMISS/liuguang/issues> 并尽量附上：

1. 运行模式：**桌面模式（`--persona user`）** 还是 **无桌面模式（`--persona system`）**
2. 合成器 / 显卡驱动信息（如 `niri` / `sway`、`vainfo` 输出）
3. `liuguangctl status` 输出与服务日志（`journalctl --user -u liuguang -n 100` 或 `journalctl -u liuguang -n 100`）
