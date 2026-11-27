# liuguang-display / liuguang-render 性能与死路径结论（t3）

> 所有数字都来自**可复现的命令**；真机 = `Fedora 44 测试主机`（i915 / DP-1 3840x2400@60 / 4 核）。

## 1. 结论速览

| 项目 | 优化前 | 优化后 | 证据 |
|---|---|---|---|
| 扫描缓冲分配/映射 | **每帧**一次 `create/map` 语义（旧代码每帧 `map_dumb_buffer` = `mmap`+`munmap`） | 恒为池深 3 次（715 次提交 → `allocations=3, mappings=3`） | 真机 daemon 日志 + 离线单测 |
| 4K NV12 上传拷贝 | 1.48 ms/帧（逐行循环） | **0.77 ms/帧**（单次 memcpy，1.93×） | 目标机 `upload_bench 500` |
| 4K XRGB 上传拷贝 | 4.01 ms/帧 | **2.53 ms/帧**（1.59×） | 同上 |
| 每帧重建 KMS 属性表 | 0.21 ms/帧（connector/crtc/plane 各一次 `get_properties` + 逐属性 `get_property`） | **0**（按对象缓存，仅绑定变化时重建） | 目标机 `present_bench legacy 240` |
| 每帧 `TEST_ONLY` 校验提交 | 每帧一次 | 仅模式变更时一次 | 源码 + `commit_stats` |
| **NV12 视频帧上屏** | **被 `present_cpu_fitted` 的 Xrgb8888 校验直接拒绝**（帧从未上屏，错误被 daemon 丢弃） | 12 441 600 B/帧上传 + 平面硬件缩放上屏 | 真机 `format=NV12 copied_bytes=12441600` |
| `atomic commit` 失败 | 5/179（`EBUSY`）+ legacy 回退 0 次成功 | **0/715**（`retried=0, legacy_fallback=0, failed=0`） | 真机 `commit_stats` |
| display 侧每帧 CPU（4K NV12） | — | **2.79 ms/帧**（`/proc/self/stat` 差值，240 帧） | 目标机 `present_bench 240 nv12` |
| display 侧每帧 CPU（4K XRGB） | — | 6.08 ms/帧 | 目标机 `present_bench 240 xrgb` |
| 静态图待机 CPU | — | **0.00%**（10 s 内 `/proc/<pid>/stat` 差值为 0 tick） | 真机 daemon `StaticIdle` |
| 4K 转场 CPU（每帧） | fade **120.9 ms**（本机同机对比） | fade **20.2 ms**（目标机）／slide 4.3 / wipe 4.3 / circle 4.2 ms | `transition_bench` |

## 2. 复现命令

```sh
# 目标机（<user>，需 /dev/dri/card1 空闲，即无其他 DRM master）
./.audit/rcp.sh target/release/examples/present_bench   liuguang:~/dispbench/
./.audit/rcp.sh target/release/examples/upload_bench    liuguang:~/dispbench/
./.audit/rcp.sh target/release/examples/transition_bench liuguang:~/dispbench/
./.audit/rbash.sh '~/dispbench/present_bench 240 nv12'          # 4K 视频路径端到端
./.audit/rbash.sh '~/dispbench/present_bench 240 xrgb'          # 4K 静态路径端到端
./.audit/rbash.sh '~/dispbench/present_bench legacy 240'        # 旧实现分量（每帧 mmap+逐行拷贝、属性枚举）
./.audit/rbash.sh '~/dispbench/upload_bench 500'                # 新旧拷贝路径对照（无 DRM）
./.audit/rbash.sh '~/dispbench/transition_bench 10'             # 4K 转场

# 离线（无需真机；CI 可跑）
CARGO_HOME=... cargo test -p liuguang-display    # 含"180 帧只分配/映射 3 次"等断言
```

## 3. 改了什么

### 3.1 `scanout.rs`（新）：池化 + 上传
- `ScanoutPool`：固定深度 3 的轮转池；**key（尺寸+扫描格式）不变时不分配、不映射**。
  `PoolStats{allocations,mappings,reuses,uploaded_bytes}` 使"不每帧分配/映射"可断言、可观测。
- `upload()`：连续行时走**单次 `copy_from_slice`（memcpy）**，否则逐行拷贝
  （处理源 stride / 目标 pitch 填充、NV12 双平面）。全部边界检查，**不再 panic**。
- `upload_rowwise_reference()`（`#[cfg(test)]`）：优化前语义的**正确性 oracle**，
  测试断言快路径与它逐字节一致。

### 3.2 `drm_kms.rs`：提交路径
- 槽位工厂 `DrmScanoutFactory`：`create` 时分配一次、`AddFB` 一次、**`mmap` 一次**，
  映射随槽位长期持有（`DumbBuffer` 是 `Copy` 小描述符，放进进程级 arena 换取 `'static`，
  内核 GEM 仍由槽位拷贝在 `destroy` 中释放，**不泄漏内核资源**）。
- 像素格式显式映射到扫描格式：`Xrgb8888/Argb8888 → XR24`、`Rgba8888 → AB24`（AddFB2，
  遗留 `AddFB` 无法表达 AB24）、`Nv12 → NV12`。**修掉了 NV12 被一律拒绝的 P0**
  （视频帧此前从未上屏）。
- KMS 属性句柄按对象缓存；`MODE_ID` blob 复用；`TEST_ONLY` 只在模式变更时做。
- 提交改为**阻塞式 page-flip**（不带 `NONBLOCK`）+ `EBUSY` 阻塞重试一次；
  `CommitStats{ok,retried,legacy_fallback,failed}` 暴露给诊断。legacy 回退彻底失败时
  `tracing::error!` 明确报"该帧未上屏"。
- 日志措辞如实化：`帧已提交（平面硬件缩放 + CPU 上传拷贝）`，并输出
  `copied_bytes` / `pool_reused` / `pool_allocations` / `pool_mappings` / `pool_reuses`。

### 3.3 `transition.rs`：4K 转场的两个问题
1. **panic 修复**：`slide` 原实现 `sx.round() as u32` 未做上界钳制，
   `sx ∈ [w-0.5, w)` 时越界读取 → 4K 真机路径上必现 panic（`transition_bench` 首次运行即复现）。
2. **性能**：逐像素浮点 → 行级 memcpy（slide/wipe/circle）+ 整数定点双像素混合（fade）；
   `wipe` 边界用二分（谓词与参考实现完全一致），`circle` 用 f64 候选 + 谓词校正 + 兜底逐像素。
   输出缓冲改**双缓冲轮转**（上层持有一帧时不再每帧分配 36.9 MB）。
   与逐像素参考实现的一致性由测试断言（fade 允许 ±1 舍入差，其余逐字节相等）。

## 4. 如实报告边界（规范 §13.1：不凭空承诺指标）

1. **VA surface → 系统内存的下载开销不在本 crate 范围内**：hw-verifier 实测 iHD ≈ 4.5 ms/帧
   （60 fps 时即 ~27% 单核），这是**应用侧无法回避的下限**（本 crate 必须拿到 CPU 可见的 NV12）。
   本 crate 的贡献是在此之后把每帧应用侧成本压到 4K NV12 **2.79 ms/帧**。
2. **4K60 的 16.7 ms 预算**：
   - 显示侧 2.79 ms；媒体侧 iHD 下载 ~4.5 ms；解码本身（VA-API）约 0.3 s/3 s 素材 ≈ 硬件引擎；
   - 剩余预算取决于 media 侧泵送/转换实现（video-engineer 负责）；
   - **真机当前实测：display 提交稳定在 ~30 次/秒**（`30 帧/s × 2.79 ms ≈ 8.4% 单核`），
     与 BRIEF 更新后的**稳定 30fps** 目标一致；60fps 需要 media 侧把每帧总成本压到 ~9 ms 以内。
3. **4K 转场**：fade 20.2 ms/帧 > 16.7 ms（60 Hz 会掉帧），但 < 33 ms（**30 fps 预算内**）；
   slide/wipe/circle 4.2–4.4 ms 在 60 fps 预算内。结论：**转场不导致 30fps 播放卡顿；
   若坚持 60fps，fade 需降级（缩短时长/降帧率/GPU shader）**。
4. 真机复测前提：`/dev/dri/card1` 不能有第二个 DRM master（其他成员跑 daemon 时必须让出）。

## 5. 未验证路径的取舍（gles.rs / DMA-BUF）

> **明确声明：`liuguang-render::gles`（GPU shader 转场）与
> `liuguang-display::drm_kms::present_dmabuf`（DMA-BUF 零拷贝）两条路径
> 未经验证、不构成任何交付承诺，不得在文档/状态/交付说明中当作可用功能。
> 真机验证通过并写入实测报告之前，它们只是"隔离保留的参考实现"。**

**结论：两条路径都保留，但明确"隔离 + 未验证"标注；不参与验收。**

- `liuguang-render/src/gles.rs`（1126 行，feature `gles`，非默认）：
  - 保留理由：(a) 规范 §7.2 要求的 GPU shader 参考实现仅此一份，删除会丢掉 shader/EGL 结构；
    (b) 完全在非默认 feature 后面，**默认构建不编译、运行时零成本**；
    (c) `scripts/ci.sh` 会 `cargo test -p liuguang-render --features gles`（跨范围文件，删除需同步改动）；
    (d) 模块文档**已经**自述"真机未验证（阻塞 B1）"，删掉它并不能消除"过度声称"，反而丢失实现。
  - 采取的措施：**强化隔离标注**（见模块文档），明确写入"不参与任何验收、启用前必须先完成真机 B1 验证"。
- `drm_kms::present_dmabuf`（DMA-BUF 零拷贝，规范 §6.2 列为第二阶段）：
  - 保留理由：只能由 media 侧显式 `LIUGUANG_VIDEO_DMABUF` 打开；失败**返回明确错误**（不静默降级）；
    它是第二阶段唯一的零拷贝路径（可省掉本 crate 的 2.79 ms/帧上传）。
  - 采取的措施：首次使用时 `tracing::warn!` 明确"DMA-BUF 路径真机未验证"；
    `CommitStats`/错误路径与 CPU 路径共用，失败可见。
- **被删除的死代码**：`plane_for_format` 的无效分支（`let _ = (fourcc, wanted)`）、
  重复的 `plane_has_format`、`present_cpu_fitted` 中**永不可达**的 NV12 分支
  （被前置的 Xrgb8888 校验挡死——这正是"视频不上屏"的根因）。

## 5.5 让"帧未上屏"可外部观测

- `DisplayBackend::commit_stats() -> Option<CommitStats>`（trait 级，默认 `None`）：
  `CommitStats{ok, retried, legacy_fallback, failed}`，其中 `failed` 即**该帧未上屏**的次数；
  `has_dropped_frames()` 便于上层直接判断。
- 真实后端已实现；`failed > 0` 时同时 `tracing::error!("该帧未上屏")`（不再静默）。
- **接进 `liuguangctl status` 需要改范围外的文件**（`apps/liuguang/src/daemon.rs` 取字段、
  `crates/liuguang-ipc/src/protocol.rs` 加字段、`apps/liuguangctl` 显示）；
  上层只需一行 `self.backend.commit_stats()`，建议由对应 owner 接线（已告知 Lead）。

## 6. 后续建议（下一阶段）

1. **零拷贝移交**：让 media 侧把帧直接写进本 crate 提供的扫描缓冲（或走 DMA-BUF），
   可再省 2.79 ms/帧 + 一次 12.4 MB 拷贝；需要 daemon/media 配合改接口（当前范围外）。
2. **fade 的 GPU 路径**或非临时存储写（需要 `unsafe` 或 SIMD），否则 4K60 的 fade 会掉帧。
3. 若坚持 60fps：把 media 侧每帧成本压到 ≤ 9 ms（iHD 下载 4.5 ms 已占一半）。

---

# t8：DMA-BUF 直通扫描输出（2026-10-18 真机实测）

## 1. 结论

**显示侧已能扫描输出线性 DMA-BUF**（bench 自建 dumb buffer → `DRM_IOCTL_PRIME_HANDLE_TO_FD`
导出的 **线性** modifier 缓冲）**并原子提交，全程不触碰 CPU 像素**。
**解码器导出的 tiled 缓冲（`NV12:0x0100000000000002`）端到端仍待 t9**（媒体侧协商/preroll），
其平面能力已在 `IN_FORMATS` 层面确认（见 §2）。

| 指标 | 值 | 证据 |
|---|---|---|
| 真实 DMA-BUF（**线性** NV12 3840x2160，2 平面）直通上屏 | **900/900 帧提交成功，failed=0** | 真机 bench `commit_stats{commit_ok=901,retried_after_busy=0,legacy_fallback=0,failed=0}`（字段名 t18 起更明确：**提交**成功≠上屏） |
| 上屏节奏 | **16.667 ms/帧 = 60.00 fps** | `wall_ms_per_frame=16.667`（600 帧 10.000 s） |
| 显示路径每帧 CPU **上界**（零绘制相位） | **≤ 0.367 ms/帧** | 静态相位 300 帧 `cpu_ms_per_frame=0.3667`；**每帧仍重画 64 行带（≈245 KB）未扣除，故为保守上界**（t17 审计 A.8 ⑤） |
| 对照：CPU 上传路径（本文件第 1 节） | 2.792 ms/帧 | `present_bench 240 nv12` |
| 像素在动（非静止画面） | **i915 pipe CRC 417 行 / 互异 CRC 296（≈71%，≈59.4 行/秒）** | `crc12.txt`，帧计数逐行 +1（`0x7466d → 0x7466e`）；互异数取**第 2 字段**（见下方口径更正） |
| 失败路径 | 明确报错 + 提示回退，**不 panic、不静默降级** | DRM master 被占时日志：`DMA-BUF 提交失败（NV12 + modifier 0x0，平面 plane::Handle(35)）… 调用方应回退 CPU 上传路径` |

> **口径更正（2026-10-18 复核，重要）**：本表旧值写作"416 行 / **417 互异**"，那是
> **`wc -l`（416）配"整行 distinct"（417）**——`crtc-N/crc/data` 每行是
> `<帧计数> <CRC> 0 0 0 0>`，**帧计数逐行 +1 会把"整行 distinct"顶满**，
> 于是"417 互异"几乎等于行数的同义反复，**不是 CRC 互异数**。
> 用 `$2`（真正的 CRC 字段）重取同一条件（4K60+DMA-BUF，7s）：
> `wc -l=416 / 非空行 417 / distinct($2)=**296** / distinct(整行)=417`。
> ⇒ 真实内容变化率是 **296/417 ≈ 71%**，而不是"每行都不同"；`416/7 = 59.4 行·秒⁻¹` 的速率不受影响。
> 另一条判据也一并说明：`$NF` 在本平台**恒为 `0x00000000`**（保留字段），任何"互异 CRC"
> 都必须取 `$2`，**不能用 `$NF`，也不能用整行 distinct**。

→ 每帧 CPU 相对上传路径降低 **≥7.6×**（0.367 ms 是上界，真实显示路径开销只会更低）；且这是**唯一**能让 4K60 预算里那 4.5 ms/帧
VA surface 下载消失的路径（下载是 `capsfilter(video/x-raw,format=NV12)` 造成的，非硬件限制）。

## 2. 平台事实（真机读取，`dmabuf_probe`）

```
DP-1 → encoder 95 → crtc 59（crtc_index 0 = **pipe A**，不在 pipe C ✓）
plane 35 Primary possible_crtcs=[0] nv12=true
plane 44 Overlay possible_crtcs=[0] nv12=true
  IN_FORMATS: 69 组合；NV12 支持 modifier =
    0x0100000000000003(Yf_TILED), 0x0100000000000002(Y_TILED), 0x0100000000000001(X_TILED), 0x0(LINEAR)
plane 53 Cursor  nv12=false（只有 AR24）
```
解码器（iHD `vah264dec`）实测导出 `NV12:0x0100000000000002`（Y_TILED）——**与 plane 44/35 的能力匹配**，
且 DP-1 落在 pipe A，满足 i915 gen9「NV12 仅 PLANE_1/2、非 pipe C」的约束。

## 3. 本次修掉的两个真实缺陷（都有真机复现证据）

### 3.1 modifier/flags 断言 panic（潜伏但必炸）
`drm@0.15.0::add_planar_framebuffer` 断言
`(FbCmd2Flags::MODIFIERS && modifier().is_some()) || (!MODIFIERS && modifier().is_none())`。
旧实现 `modifier()` **恒**返回 `Some(..)`（`modifier = 0` 时即 `Some(DrmModifier::Linear)`），
无 modifier 时却传 `FbCmd2Flags::empty()` → 断言失败 panic。
- **复现**：`dmabuf_probe /dev/dri/card1 DP-1 legacy` → `panicked at drm-0.15.0/src/control/mod.rs:360:
  assertion failed: (has_modifier && modifier.is_some()) || (!has_modifier && modifier.is_none())`
- **修复**：`fb_modifier()` 规范化 —— `0` → `None` + 不带 `MODIFIERS`；`DRM_FORMAT_MOD_INVALID` → 明确报错；
  未收录的 modifier **保真透传**（`drm-fourcc` 用 `Unrecognized(x)` 保留原值，不丢信息）。
- **验证**：同一缓冲走修复后的组合 → `AddFB2 成功`；离线单测 `fb_modifier_pairing_satisfies_drm_assert` 锁定该不变量。

### 3.2 NV12 只给 1 个 AddFB2 平面 → 内核 EINVAL
NV12 是半平面：Y/UV 两个平面**共用同一 GEM 句柄**、以 pitch/offset 区分；只给 1 个平面时内核直接 `EINVAL`。
- **复现**：`dmabuf_probe` → `nv12-1plane: 按预期失败（NV12 需要 2 个平面）: Invalid argument (os error 22)`
- **修复**：`is_semi_planar_fourcc()`（NV12/NV21/NV16/NV61/P010/P012/P016 → 2 平面），
  且半平面要求 `strides[1]`/`offsets[1]` 可用，否则**明确报错**（多 DRM object 的 DMA-BUF 当前 `DmaBufFrame`
  表达不了，不能误画）。
- **验证**：修复后 `AddFB2(NV12 双平面, 无 modifier) 成功 fb=framebuffer::Handle(119)`；真机 900 帧直通成功。

### 3.3 另外两处按任务要求补齐
- **fourcc 不再硬编码**：`format()` 改用 `frame.fourcc`（`DrmFourcc::try_from`，失败给可读错误）。
- **选平面前查 `IN_FORMATS`**：新模块 [`in_formats`](src/in_formats.rs) 解析 `drm_format_modifier_blob`
  （位掩码 + offset 配对、越界/版本校验），`plane_for_dmabuf()` 要求
  `fourcc ∈ formats()` **且** `(fourcc, modifier) ∈ IN_FORMATS`；blob 不可用时明确告警并降级为"仅比 fourcc"。
  7 个离线单测覆盖解析与边界。
- **`preflight_dmabuf()`**（新公开 API）：`PRIME 导入 + AddFB2 + atomic TEST_ONLY`，
  **不需要 DRM master**，可在切换直通路径前预检状态（真机排障/CI 均可用）。

## 4. 复现命令

```sh
./.audit/rcp.sh target/release/examples/dmabuf_probe         liuguang:~/dispbench/
./.audit/rcp.sh target/release/examples/dmabuf_present_bench liuguang:~/dispbench/
# 平面/pipe/IN_FORMATS + panic 复现 + 修复验证（不需要 master）
./.audit/rbash.sh '~/dispbench/dmabuf_probe /dev/dri/card1 DP-1 both'
# 直通上屏 + 每帧 CPU（需要 master：先 systemctl stop liuguang，确认 fuser /dev/dri/card1 为空）
./.audit/rbash.sh '~/dispbench/dmabuf_present_bench 900 nv12 bar'
# CRC（root；**必须先设源再开 reader**，否则 EBUSY）
./.audit/rootsudo.sh 'D=/sys/kernel/debug/dri/1/crtc-0/crc; echo pipe > $D/control;   (timeout 8 cat $D/data > /tmp/crc.txt) & sleep 7; kill %1; echo none > $D/control;   echo "distinct=$(sort -u /tmp/crc.txt | wc -l) lines=$(wc -l < /tmp/crc.txt)"'
```

## 5. 如实说明未完成/未验证的部分（规范 §13.1）

1. **与解码器直连的端到端仍缺一环（媒体侧，t9）**：`LIUGUANG_VIDEO_DMABUF=1` 当前在真机失败 ——
   `vah264dec` + `mode=DmaBuf` → `qtdemux … Internal data stream error`（与 hw-verifier 早前
   "能协商但无法 preroll" 同一现象）。本节的直通验证用的是**真实 DMA-BUF**（dumb buffer →
   `PRIME_HANDLE_TO_FD` 导出，线性 modifier），因此 `(fourcc, modifier=Y_TILED, 双平面)` 这一组合
   在**平面能力（IN_FORMATS）层面已确认可用**，但"解码器 tiled 缓冲直通"要等 t9 打通后复跑本 bench。
2. **`fit/zoom/offset`**：直通路径与 CPU 路径**共用** `build_scanout_request`（`SRC_*`/`CRTC_*` 16.16 平面矩形），
   本次以 cover 适配矩形（src 192,0,3456x2160 → dst 0,0,3840x2400）连续提交 900 帧成功；
   未单独跑 zoom/offset 变体。
3. **每帧 CPU 的残余 0.367 ms 是上界**（每帧仍重画 64 行带 ≈245 KB 未扣除），来源是
   `prime_fd_to_buffer` + `AddFB2` + 原子提交三类 ioctl（每帧各一次）。
   可进一步用 fb 缓存（同一 GEM 句柄复用 framebuffer）压到更低，本次未做（收益 <0.3 ms/帧）。
4. 页面/CRC 采样窗口与其他成员共享：本次 CRC 采样前需清掉**自己**遗留的 `cat …/crc/data`（否则
   `control` 写入返回 EBUSY，i915 语义是"有 reader 时不可改源"）。

---

# t16：显式获取 DRM master（消除"启动顺序决定能否点亮屏幕"的静默黑屏）

## 1. 问题

内核只在 `dev->master == NULL` 时把 DRM master 授予**第一个打开**主设备节点的进程。
liuguang 以前既不显式调用 `drmSetMaster()`，也不检查自己是否拿到 master，于是：

- 启动时已有别人占着 `card1` ⇒ 提交全部 `EACCES`、**画面不更新**，但 `presented` 照涨
  （进程自认为在放视频）——**静默黑屏**；
- 失败时逐帧告警，实测 **~140 行/秒**，把真正的原因淹没。

## 2. 修复（真机实测）

| 项 | 修复前 | 修复后（实测） |
|---|---|---|
| master 获取 | 不调用，也不检测 | 打开设备后**显式** `drmSetMaster`，并解释结果（成功 / EINVAL=已是 master / EACCES=拿不到） |
| 启动可观测性 | 无 | 竞争时**启动阶段** ERROR：给出**占用者 PID/命令名** + 可操作建议（`systemctl stop liuguang` / `pkill -f liuguang`） |
| 失败日志 | ~140 行/秒 | **4.9 行/秒**（15 秒共 73 行；前 3 条逐条 + 之后每 2 秒一条，并附"已抑制 N 次"） |
| 每帧多余 ioctl | 模式变更帧每帧一次 `TEST_ONLY` | 限速为每 5 秒最多一次 |
| 竞争解除后 | 永远黑屏（只能重启服务） | **自愈**：每 2 秒尝试重新打开设备取得 master，成功即自动恢复并打印日志 |

## 3. 真机证据（内核判据，不用 `presented`）

**① 无竞争启动**（先 `systemctl stop liuguang`，确认 `fuser /dev/dri/card1` 为空）：
```
INFO  DRM master：drmSetMaster 成功（显式取得；本进程可提交显示状态）  device=/dev/dri/card1
INFO  首帧/模式变更帧已提交扫描输出  output=DP-1 sequence=1 format="NV12" plane=plane::Handle(35)
      copied_bytes=12441600 pool_allocations=1
（175 条提交日志，0 条失败）
```

**② 有竞争启动**（同一个用户先用 `dmabuf_present_bench` 打开 card1 并成为 master，再启动 daemon）：
```
WARN  检测到其它进程已打开该 DRM 设备（可能持有 master）  holders=["PID 96326 (dmabuf_present_)"]
ERROR 启动时无法取得 DRM master（画面不会更新）：检测到其它进程正打开该设备：PID 96326 (dmabuf_present_)；
      请先停止它们（例如 `systemctl stop liuguang` 或 `pkill -f liuguang`），再启动本服务
      result=drmSetMaster 失败（Permission denied (os error 13)）：内核拒绝（该 ioctl 标记为 DRM_ROOT_ONLY，
      非 root 调用必然 EACCES；也可能已被他人持有）
```
内核 `clients`（root）——**唯一可信判据**：
```
     command  tgid dev master a   uid
dmabuf_present_ 96326   1   y    y  1000     ← 占用者 = master
   liuguang-t16 96332   1   n    n  1000     ← 我们的 daemon = 非 master（画面确实没上屏）
```
15 秒日志总量 **73 行（≈4.9 行/秒）**，其中失败信息受"前 3 条 + 每 2 秒 1 条"限速
（单条日志携带 `suppressed=109`，即该条已代表 109 次被抑制的事件）。

**③ 竞争解除后的自愈**（杀掉占用者，不重启 daemon）：
```
WARN  已重新打开 DRM 设备以尝试恢复 master（资源已重建，下一次提交重新 modeset） master=Some(true) holders=[]
```
之后 **2491 次提交成功**、仅 9 条失败日志（转换期，已限速）；内核 `clients` 显示
`liuguang-t16 tgid=95473 dev=1 master=y`。

## 4. `drmSetMaster` 的内核语义（如实说明，§13.1）

- 本机实测：**无竞争时 `drmSetMaster` 返回成功**（`INFO DRM master：drmSetMaster 成功`）——
  因此"显式获取"在真机上是**真的生效**，不是只做检测。
- 有竞争时返回 `EACCES`：既有该 ioctl 的 `DRM_ROOT_ONLY`（非 root 调用受限）语义，
  也有"已被他人持有"的可能，**无法仅凭返回值区分**；因此本实现把它记为"未确定"，
  由**首帧提交结果**给出确定结论（成功 ⇒ 是 master；`EACCES` ⇒ 不是），并同步更新
  `master_status()` 供上层查询。
- 非 root 进程**不能"抢"master**：竞争状态下的自愈走的是"**重新打开设备**"
  （内核在无 master 时才授予第一个打开者），这也是 `try_reacquire_master()` 的实现方式。
  重开会重建资源（扫描池重新分配、下一次提交重新 modeset），日志明确可观测。

## 5. 复现命令

```sh
# ① 无竞争
./.audit/rootsudo.sh 'systemctl stop liuguang; sleep 1; fuser /dev/dri/card1'
./.audit/rbash.sh 'cd ~/dispbench && RUST_LOG=liuguang=info,liuguang_display=info   nohup ./liuguang-t16 --config ~/dispbench/config-t16.toml --socket ~/dispbench/ctl-t16.sock   --state-file ~/dispbench/state-t16.json > run.log 2>&1 & sleep 4'
./.audit/rootsudo.sh 'cat /sys/kernel/debug/dri/1/clients | head -4'   # 期望 master=y

# ② 有竞争（同用户占住设备）
./.audit/rbash.sh 'cd ~/dispbench && nohup ./dmabuf_present_bench 9000 nv12 black > holder.log 2>&1 & sleep 3;   pgrep -f dmabuf_present_bench;   nohup ./liuguang-t16 --config ~/dispbench/config-t16.toml --socket ~/dispbench/ctl-t16.sock   --state-file ~/dispbench/state-t16.json > run2.log 2>&1 & sleep 15; wc -l run2.log'
./.audit/rootsudo.sh 'cat /sys/kernel/debug/dri/1/clients | head -4'   # 期望本 daemon master=n

# ③ 自愈：杀掉占用者后再看
./.audit/rbash.sh 'pkill -f dmabuf_present_bench; sleep 7; grep -E "重新打开 DRM 设备|master" ~/dispbench/run2.log | tail -3'
./.audit/rootsudo.sh 'cat /sys/kernel/debug/dri/1/clients | head -4'   # 期望 master=y
./.audit/rootsudo.sh 'systemctl start liuguang'                        # 恢复用户画面
```

## 6. 未做/边界

1. `master_status()` 已经由 `crates/liuguang-display` 暴露，但 **daemon 侧尚未把它写进 IPC 状态**
   （`apps/liuguang/src/daemon.rs` 与协议在本任务范围外）——上层暂时只能从日志看到。
2. 自愈的代价是重建资源（扫描池重新分配、下次提交重新 modeset）；日志会明确说明，
   但 `pool_allocations` 会因此增长（正常现象，见 t3 章节的池化说明）。
3. 若占用者是**其它用户**的进程，`/proc/<pid>/fd` 不可读，占用者列表为空；
   此时错误信息引导运维用 root 读 `/sys/kernel/debug/dri/<n>/clients`（提示文本里已给出）。

---

## 6. t29：DMA-BUF 平面日志降噪（实测）

**问题**（hw-verifier t10 报告，本任务复现并修复）：`plane_for_dmabuf()` 每帧都会跑，
命中 IN_FORMATS 时**每帧打一条 INFO**。4K@60 视频下 = ≈60 行/秒，
真机 30 秒观测到 **1750 行**，既淹没有效日志（t14 的 F6 就是同一类问题）又浪费 CPU。

**修复**：抽出纯函数 `plane_choice_is_new(previous, current)`，只在
`(plane, fourcc, modifier)` **变化**时打 INFO（首次/切换），重复降 `trace!`。
失败类日志（提交失败 / 回退 / 注入）一条没有降级。

**真机对照**（同一台机、同一 4K60 素材、`--backend drm`、单实例）：

| 指标 | 修复前（t10 实测） | 修复后（t29 实测） |
|---|---|---|
| 30s 日志行数（视频播放中） | **1750** | **41**（≈42× 降幅） |
| 其中 DMA-BUF 平面行 | ≈1750 | **0** |

**回归测试**（不需要 DRM 设备）：
- `plane_choice_logs_only_on_change`：首帧打、稳态沉默、三要素任一变化必打；
- `steady_state_plane_choice_is_silent_over_many_frames`：600 帧稳态**只能有 1 条**日志
  （旧实现是 600 条）。

**顺带回答"光标是否污染 CRC"**（静态画面 + `crtc-0` 管道 CRC，root）：
`cursor_blink=1` 与 `=0` 结果**完全相同** —— 178 行、**CRC 互异 = 1**（`0x9276f8ae`，取**第 2 字段**）。
即本平台我们的 plane 覆盖后 fbcon 光标不进 pipe CRC；因此后端**不**强制禁用 cursor plane
（避免与 fbcon 抢状态），改为要求验收脚本测 CRC 前 `cursor_blink=0` 并记录原值。
采集协议见 `docs/DEPLOYMENT_FEDORA.md` §8.3.3（先写 control 源名再开 reader、reader 必须
`timeout` 包住、读完写回 `none`）。
