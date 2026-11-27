# RPM 打包与部署（Fedora）

本文档说明如何从源码生成 Fedora RPM 源码包与二进制包（`.rpm`），并完成无桌面展示屏（`system` 模式）安装与验收。

---

## 1. RPM 打包优势

| 能力 | 原生 RPM 管理 |
|---|---|
| 安装与升级 | `sudo dnf install ./x.rpm` · `dnf upgrade` |
| 状态查询 | `rpm -q liuguang` · `rpm -qi` · `rpm -ql` |
| 完整性校验 | `rpm -V` 自动比对安装文件与权限 |
| 系统集成 | 声明式 `sysusers`、`%systemd_*` 宏、`%config(noreplace)` |

---

## 2. 构建（在 Fedora 上，普通用户即可）

### 2.1 一次性依赖

```bash
sudo dnf install rpm-build rpmdevtools rust cargo gcc-c++ pkgconf-pkg-config \
    gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel \
    gstreamer1-plugins-bad-free libdrm-devel systemd-devel
```

> `vapostproc` 在 `gstreamer1-plugins-bad-free` 里；`crates/liuguang-media/build.rs` 用
> `-l:libgstva-1.0.so.0` 链接那个库，所以**构建时**就必须存在（不需要 `-devel`，
> 系统没有 `libgstva-1.0.so` 开发符号链接 —— 这是实测过的，见 git 历史 t96/t97）。

### 2.2 构建

```bash
bash scripts/rpm-build.sh          # 生成 tarball + rpmbuild -ba
```

产物：

```
.rpmbuild/SRPMS/liuguang-0.1.0-1.fc44.src.rpm
.rpmbuild/RPMS/x86_64/liuguang-0.1.0-1.fc44.x86_64.rpm
.rpmbuild/RPMS/x86_64/liuguangctl-0.1.0-1.fc44.x86_64.rpm
```

其他用法：

```bash
bash scripts/rpm-build.sh --tarball   # 只生成源码 tarball（可在无 rpmbuild 的机器上跑）
bash scripts/rpm-build.sh --srpm      # 只出 SRPM（投 COPR / mock 用）
RPM_TOPDIR=/var/tmp/rpm bash scripts/rpm-build.sh   # 指定 _topdir
```

### 2.3 构建结构说明（几个不明显但重要的点）

- **源码包支持离线 vendored crates**（`.rpm-vendor/`）：
  `%build` 使用 `cargo build --offline` ⇒ 隔离构建环境无需外网即可复现。
- **入包内容采用白名单归档**（`scripts/rpm-build.sh`），避免把本地构建缓存（`target/`、`dist/` 等）打进 SRPM。
- **`.cargo/config.toml` 只写进打包暂存目录**，不会污染工作区源码树。
- **`_topdir` 会在 `$HOME` 不可写时自动回退到工作区 `.rpmbuild/`**。

---

## 3. 安装 / 升级 / 卸载

### 3.1 安装

```bash
sudo dnf install .rpmbuild/RPMS/x86_64/liuguang-0.1.0-1.fc44.x86_64.rpm \
                 .rpmbuild/RPMS/x86_64/liuguangctl-0.1.0-1.fc44.x86_64.rpm
```

`%pre` 会用 sysusers 创建系统用户 `liuguang` 与组 `liuguang-ctl`；
`%post` 会 `systemctl daemon-reload`（**不自动 enable/start**，见 §4）。

### 3.2 启用与启动

```bash
sudo systemctl enable --now liuguang
systemctl status liuguang
sudo liuguangctl status          # 或在 liuguang-ctl 组内免 sudo
```

### 3.3 升级

```bash
# 改 spec 的 Release（如 1 → 2）后重打，然后：
sudo dnf upgrade ./liuguang-0.1.0-2.fc44.x86_64.rpm
```

`%config(noreplace)` 保证 `/etc/liuguang/config.toml` **不会被覆盖**（你的改动保留，
新版本示例以 `.rpmnew` 落盘）。

### 3.4 卸载

```bash
sudo systemctl disable --now liuguang
sudo dnf remove liuguang liuguangctl
```

**卸载后仍残留、需要手动清理的**（RPM 不会删用户数据）：

| 残留 | 清理命令 |
|---|---|
| 状态目录 `/var/lib/liuguang/` | `sudo rm -rf /var/lib/liuguang` |
| 配置 `/etc/liuguang/`（含 `%config` 保留） | `sudo rm -rf /etc/liuguang` |
| 用户 `liuguang`、组 `liuguang-ctl` | `sudo userdel liuguang; sudo groupdel liuguang-ctl` |

---

## 4. 从手工部署迁移到 RPM（重要）

当前机器上是**手工部署**：
`/usr/local/bin/liuguang` + `/etc/systemd/system/liuguang.service`。
RPM 装到 `/usr/bin` + `/usr/lib/systemd/system`。

**⚠️ 必须按顺序操作，否则两个单元/两个二进制并存，来源不明：**

```bash
# 1) 停服务并禁用（避免升级时被旧单元拉起）
sudo systemctl disable --now liuguang

# 2) 移除手工单元（RPM 用 /usr/lib/systemd/system 里的那份）
sudo rm -f /etc/systemd/system/liuguang.service
sudo systemctl daemon-reload

# 3) 备份现有配置（RPM 用 %config(noreplace)，但手工那份要先留底）
sudo cp -a /etc/liuguang/config.toml /root/config.toml.bak-manual

# 4) 清掉手工部署的二进制与调试备份（Lead 已盘点：约 126 MB）
sudo rm -f /usr/local/bin/liuguang /usr/local/bin/liuguangctl \
            /usr/local/bin/liuguang.debug.bak /usr/local/bin/liuguangctl.debug.bak \
            /usr/local/bin/liuguang.bak-*

# 5) 安装 RPM
sudo dnf install .rpmbuild/RPMS/x86_64/liuguang-*.rpm .rpmbuild/RPMS/x86_64/liuguangctl-*.rpm

# 6) 把配置放回（RPM 已装了一份示例作为 %config）
sudo cp -a /root/config.toml.bak-manual /etc/liuguang/config.toml

# 7) 启动并验收
sudo systemctl enable --now liuguang
sudo liuguangctl status      # 期望: state=VideoPlaying
```

### 验收（缺一不可）

```bash
systemctl is-active liuguang                                   # active
sudo grep -c 'master=y' /sys/kernel/debug/dri/1/clients        # ≥1（内核判据，唯一可信）
sudo liuguangctl status | grep -E 'state:|decoder:|presented='  # VideoPlaying + presented 在涨
rpm -q liuguang liuguangctl                                     # 两个包都在
rpm -V liuguang                                                # 无输出 = 文件未被改动
ls -Z /usr/bin/liuguang                                        # system_u:object_r:bin_t:s0
```

> `presented` 只说明"已出队"，**不是上屏证据**；上屏要用内核 `master=y` + 提交计数增长
> + （需要时）i915 pipe CRC 三者交叉验证。

---

## 5. 已知限制（如实）

- ~~**`.spec` 与 `rpm-build.sh` 尚未在真实 `rpmbuild` 上跑过**~~ —— **已于 2026-10-20 在
  Fedora 44 上实测通过**：`bash scripts/rpm-build.sh` 一次成功，产出
  `liuguang-0.1.0-1.fc44.x86_64.rpm` / `liuguangctl-0.1.0-1.fc44.x86_64.rpm` /
  `liuguang-0.1.0-1.fc44.src.rpm`，`dnf install` 后服务正常、硬解与零拷贝均生效。
- **开源许可证为 `MIT`** —— 根目录 `LICENSE` 文件与 `spec` 的 `License:`、`Cargo.toml` 保持同步。
- **平台**：构建产物与硬件解码路径面向 **linux-x64 + Intel VAAPI（iHD）**；
  其他架构/驱动不在验证范围内。
- **不自动 `systemctl enable`**：RPM 只做安装，启用时机交给管理员
  （壁纸守护进程会接管显示，不该在安装事务里悄悄点亮屏幕）。

---

## 6. 日常运维命令（2026-10-20 实测）

以下命令全部在 **Fedora 44 + i915 + DP-1 3840x2400@60** 上验证过。

### 6.1 构建

```bash
cd ~/Liuguang
bash scripts/rpm-build.sh                          # 完整构建（tarball + rpmbuild -ba）
bash scripts/rpm-build.sh --tarball                # 只出 tarball（无 rpmbuild 的机器）
bash scripts/rpm-build.sh --srpm                   # 只出 SRPM（投 COPR / mock）
RPM_TOPDIR=/somewhere bash scripts/rpm-build.sh    # 指定 _topdir
```

产物位置（默认）：

```
~/rpmbuild/RPMS/x86_64/liuguang-0.1.0-1.fc44.x86_64.rpm
~/rpmbuild/RPMS/x86_64/liuguangctl-0.1.0-1.fc44.x86_64.rpm
~/rpmbuild/SRPMS/liuguang-0.1.0-1.fc44.src.rpm
```

### 6.2 安装 / 升级 / 卸载

```bash
sudo dnf install ./liuguang-0.1.0-1.fc44.x86_64.rpm ./liuguangctl-0.1.0-1.fc44.x86_64.rpm
sudo systemctl enable --now liuguang

# 升级：先把 spec 的 Release 加一（1 → 2），重打，再
sudo dnf upgrade ./liuguang-0.1.0-2.fc44.x86_64.rpm

# 卸载（含配置：加 --noscripts 不会删配置，见 6.4）
sudo dnf remove liuguang liuguangctl
```

### 6.3 验收（缺一不可）

```bash
# 服务活着
systemctl is-active liuguang && systemctl is-enabled liuguang

# 跑的是 RPM 装的那一份：RPM 记录哈希 == 运行中二进制哈希
rpm -q --dump liuguang | grep '/usr/bin/liuguang' | awk '{print $4}'
p=$(systemctl show liuguang -p MainPID --value); sha256sum /proc/$p/exe

# 硬解 + 零拷贝
liuguangctl status | grep -E 'decoder|state'
#   decoder:    vaapi:vah264dec(dmabuf)

# 平面几何（cover 满屏）
sudo grep -E 'crtc-pos=|src-pos=' /sys/kernel/debug/dri/1/state | head -2
#   crtc-pos=3840x2400+0+0
#   src-pos=3520.000000x2160.000000+128.000000+0.000000
#                                              ^^^ 128 的倍数（Y_TILED 对齐要求）

# CPU
p=$(systemctl show liuguang -p MainPID --value)
a=$(awk '{print $14+$15+$16+$17}' /proc/$p/stat); sleep 15
b=$(awk '{print $14+$15+$16+$17}' /proc/$p/stat)
awk -v d=$((b-a)) -v h=$(getconf CLK_TCK) 'BEGIN{printf "%.1f%%\n", d*100/h/15}'
```

期望：`dmabuf` 路径、`src-pos` 偏移是 128 的倍数、CPU 个位数（1080p ≈ 2%、4K60 ≈ 7%）。

### 6.4 疑难速查（都是真实踩过的）

| 现象 | 原因 | 处理 |
|---|---|---|
| `liuguangctl: 没有那个文件或目录`（`/usr/local/sbin/...`） | bash **命令缓存**指向已删除的旧路径 | `hash -r`，或开新终端 |
| 升级后配置没变，多出 `config.toml.rpmnew` | `%config(noreplace)`：保留你的配置，新版另存 | 需要新选项时手动合并 `.rpmnew` |
| 装了 RPM 但行为像旧版 | `/etc/systemd/system/liuguang.service` 里的**手写 unit 覆盖**了 RPM 的 | 删掉手写 unit 后 `systemctl daemon-reload` |
| 服务起不来，`liuguang` 用户不存在 | 手工部署没跑过 `sysusers` | `sudo systemd-sysusers` 或重装 RPM |
| 画面有横带/重影 | DMA-BUF Y-tiled `src-pos` X 偏移未按 128 对齐 | 先确认 `src-pos` 偏移是 128 的倍数 |
| CPU 回到 65% | DMA-BUF 协商失败，回退到 CPU 捕获 | `journalctl -u liuguang \| grep root_cause` 看原因 |

### 6.5 彻底清理（确认不再使用时）

```bash
sudo systemctl disable --now liuguang
sudo dnf remove liuguang liuguangctl
sudo rm -f /etc/liuguang/config.toml /etc/liuguang/config.toml.rpmnew
sudo rm -rf /var/lib/liuguang /run/liuguang
# 校验：不应再有 liuguang* 残留
find /usr/local /usr/bin /etc/systemd/system -maxdepth 1 -name 'liuguang*' 2>/dev/null
```
