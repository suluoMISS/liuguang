# 开发与贡献指南

## 1. 环境准备

- **Rust**：`1.85+`（edition 2024）
- **工具链与系统依赖**：`pkg-config`、`wayland-client`、`libdrm`、GStreamer 开发包；推荐补充 `libva` + `ffmpeg`（零拷贝与转码）以及 `gtk4` + `libadwaita`（图形界面）。

建议将缓存与构建产物集中放在仓库工作目录下，保持外部环境整洁：

```bash
export CARGO_HOME=$PWD/.cargo-home
export CARGO_TARGET_DIR=$PWD/.work/build
```

---

## 2. 构建命令

```bash
# 1) 守护进程与控制端 CLI/TUI（带 VA-API 零拷贝与 GStreamer 视频链路）
cargo build --release -p liuguang -p liuguangctl --features liuguang/vaapi

# 2) 不含 VA-API 的纯 GStreamer 构型（用于极简依赖环境或 RPM 打包）
cargo build --release -p liuguang -p liuguangctl --features liuguang/gstreamer

# 3) GTK4 + libadwaita 图形界面（独立 workspace）
./scripts/build-gui.sh --release
./scripts/build-gui.sh --release --install   # 安装到 ~/.local/bin 与桌面图标目录
```

---

## 3. Feature 与门禁测试

直接运行不带 `--features` 的 `cargo test --workspace` 只覆盖核心状态机、IPC、TUI 等默认构型；**提交代码前必须同时验证非默认视频/GPU 渲染 feature 矩阵以及图形界面**：

```bash
# 完整工作区门禁（默认构型 + gstreamer / gles 矩阵）
./scripts/ci.sh

# 零拷贝模块与启用 vaapi 的守护进程测试
cargo test -p liuguang-vaapi
cargo test -p liuguang --features vaapi --lib

# GTK4 GUI 检查与单测
(cd apps/liuguang-gui && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test)
```

---

## 4. 调试环境变量

仅限开发排障时使用：

| 变量 | 用途 |
|---|---|
| `LIUGUANG_VAAPI_PROFILE=1` | 打印 VA-API 冷启动、节点挑选与首帧导出耗时 |
| `LIUGUANG_RGB_STATS=1` | 周期性打印 GPU NV12→RGB 转换各阶段耗时 |
| `LIUGUANG_VIDEO_PREFETCH=0..4` | 调整视频解码预读缓冲队列深度（`0` 退回同步拉帧） |
| `LIUGUANG_DMABUF_IMMED=0` | 禁用 `zwp_linux_dmabuf_v1` 同步立即导入 |
| `LIUGUANG_VIDEO_DMABUF=off` | 强制视频回退系统内存共享路径，排查合成器兼容问题 |
| `LIUGUANG_GUI_DEBUG=1` | 输出 GUI 详细交互与缩略图缓存日志 |

---

## 5. 代码约束

1. **`unsafe_code = "deny"`**：工作区及 GUI 默认禁止 `unsafe`；仅 FFI 与少量底层的底层模块按模块/函数作用域申请并附安全注释。
2. **严禁业务代码 `unwrap()` / `expect()`**：非 `#[cfg(test)]` 代码开启了 Clippy `unwrap_used = "deny"` 与 `expect_used = "deny"`，所有运行时错误均应通过 `Result` 优雅降级并向用户返回可操作提示。
3. **`liuguang-core` 纯净边界**：禁止引入 GTK、DRM、GStreamer、Wayland 等重型系统依赖，保证纯逻辑可跨环境快速测试。
4. **中文交互文案**：用户可见提示、状态说明与错误提示统一使用清晰中文，明确告知原因与后续可操作动作。
