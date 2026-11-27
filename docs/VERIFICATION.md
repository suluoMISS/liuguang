# 验收与基准指标

本文记录流光（Liuguang）v0.3.3 的核心验收口径、实测表现与本地复现方式。

## 1. 验收判据

| 维度 | 达标标准 |
|---|---|
| **解码路径** | `liuguangctl status` 报告零拷贝路径（`vaapi:nvdec(dmabuf)` 或 `vaapi:nvdec(dmabuf-rgb)`） |
| **帧率与丢帧** | 实测帧率偏差 ≤5%；稳态 `dropped_late` ≈ 0，`dropped_queue_full = 0` |
| **颜色准确性** | 与 FFmpeg 参考帧逐像素平均绝对误差（MAE）≤ 2.0 |
| **提交健康度** | `present.failures = 0`、`keepalive_len` 有界不泄漏 |
| **单核 CPU 预算** | 1080p60 ≤8%、1440p60 ≤12%、4K60 ≤19%、8K60 ≤35% |
| **图形界面** | 4/6 列曜晶画报墙任意数量（`1..=35` 章节组合）无空洞；`liuguang-gui --self-test` 全绿 |

---

## 2. 实测结果摘要

- **桌面 Wayland 分数缩放**：通过 `scripts/e2e/wayland_layer_check.py` 像素级验证，1.5× 缩放下背景层铺满全输出无黑边；
- **4K60 / 4K120 零拷贝播放**：1080p60 单核约 4.5–6%，4K60 单核约 12–19%（`dropped_late=0`），120Hz+ 输出实测可达 **118.6 fps**；
- **GPU NV12→RGB 色彩修正**：针对仅读取亮度平面的合成器，GPU 着色器输出实测 MAE **1.214**（颜色正确且单核保持 ~13.7%）；
- **自动让路叠加仲裁**：全屏、游戏进程、显存阈值（85%/75%）、电池供电及熄屏均独立生效、恢复不误放；
- **内存与冷加载优化**：缩略图 worker 并发 + `malloc_trim` 堆回收、稳态 DMA-BUF 期间释放闲置 `wl_shm` 内存池、长时间暂停 20s 自动卸载视频管线，UI 交互延迟保持流畅；
- **全套自动化门禁**：`./scripts/ci.sh`（默认构型 + `gstreamer` / `gles` feature 矩阵）、`liuguang-vaapi`、`liuguang --features vaapi` 及 `liuguang-gui`（64 项单测）全部通过。

---

## 3. 快速复现验证

```bash
export CARGO_HOME=$PWD/.cargo-home CARGO_TARGET_DIR=$PWD/.work/build

# 1. 工作区 + Feature 矩阵门禁
./scripts/ci.sh

# 2. GUI 编译、Clippy 与全部单测
(cd apps/liuguang-gui && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test)

# 3. 首次安装脚本体检与沙箱模拟
./scripts/install.sh --check
./scripts/install.sh --mode user --dry-run

# 4. systemd 单元语法校验
systemd-analyze verify packaging/liuguang.service
systemd-analyze --user verify packaging/liuguang-user.service
```

---

## 4. 已知支持边界

- **合成器支持范围**：桌面模式依赖 `zwlr_layer_shell_v1`（niri / sway / Hyprland 等），原生 GNOME（Mutter）与 KDE Plasma 不支持该协议；无桌面设备请使用 `--mode system`（DRM/KMS）。
- **显卡硬件**：端到端高帧率与零拷贝路径已重点在 NVIDIA + Intel 环境下验证；AMD 显存压力读取已通过 sysfs 接入。
- **RPM 构建**：`packaging/liuguang.spec` 随版本同步维护，请在具备 `rpmbuild` 的 Fedora/RHEL 环境构建实测。
