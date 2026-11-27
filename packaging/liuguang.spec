%global debug_package %{nil}

# 为什么 %%{nil}：
#   Rust 二进制默认会被 rpmbuild 抽成 debuginfo 子包，但本项目 release 构建未开
#   debug=2 的完整符号策略，抽出来的包几乎为空却会让构建多一步。
#   若日后需要调试符号，删掉这行并加 BuildRequires: rust-debugger-common。

Name:           liuguang
Version:        0.3.0
Release:        14%{?dist}
Summary:        Dynamic wallpaper engine: Wayland layer (desktop) and DRM/KMS (headless)

License:        MIT
URL:            https://github.com/suluoMISS/liuguang
Source0:        %{name}-%{version}.tar.gz

# ---- 构建依赖 ------------------------------------------------------------
# 库由 pkg-config 定位；名字用 pkgconfig(...) 形式，rpm 会自动换算成 -devel 包。
BuildRequires:  rust >= 1.85
BuildRequires:  cargo
BuildRequires:  gcc-c++
BuildRequires:  pkgconfig(libdrm)
BuildRequires:  pkgconfig(gstreamer-1.0)
BuildRequires:  pkgconfig(gstreamer-video-1.0)
BuildRequires:  pkgconfig(gstreamer-allocators-1.0)
# crates/liuguang-media/build.rs 通过 -l:libgstva-1.0.so.0 链接（系统无开发符号链接），
# 因此【构建时】就必须存在这个运行库本身：
BuildRequires:  gstreamer1-plugins-bad-free
BuildRequires:  systemd-rpm-macros

# ---- 运行依赖 ------------------------------------------------------------
Requires:       gstreamer1-plugins-base
Requires:       gstreamer1-plugins-bad-free
# MP4 demux（filesrc ! qtdemux）在 plugins-good；缺它 ⇒ 新装机器 MP4 完全不可播。
Requires:       gstreamer1-plugins-good
# 硬件解码走 VAAPI；Intel 平台用 intel-media-driver（iHD），旧平台可换 libva-intel-driver。
# 这里只做弱依赖建议，避免在非 Intel 机器上强拉：
Recommends:     intel-media-driver
# 软解回退（hardware_decode=auto/软件兜底）：openh264dec 来自 Fedora 官方仓库；
# avdec_h264 作为备选（包更大，不强拉）。
Recommends:     gstreamer1-plugin-openh264
Suggests:       gstreamer1-plugin-libav
Requires:       systemd
# systemd 单元用 SupplementaryGroups=video 访问 /dev/dri/card*
Requires(pre):  shadow-utils

%description
Liuguang is a headless wallpaper engine for Fedora Server: it drives the display
directly through DRM/KMS, with no X11, no Wayland and no desktop session.

Features:
  * video wallpapers decoded on the GPU (VAAPI) and scanned out zero-copy via DMA-BUF
  * images, playlists, transitions, automatic rotation
  * a terminal UI and a CLI (liuguangctl) for management, usable over SSH
  * a systemd service that keeps the wallpaper alive across SSH logouts

%package -n liuguangctl
Summary:        Command line control tool for Liuguang
Requires:       %{name}%{?_isa} = %{version}-%{release}

%description -n liuguangctl
Command line client for the Liuguang wallpaper daemon.

%prep
%setup -q

%build
# --offline 配合 tarball 内的 vendor/ 与 .cargo/config.toml，构建机无需联网。
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo-home}"
cargo build --release --offline \
    -p liuguang -p liuguangctl \
    --features liuguang/gstreamer

%install
install -d %{buildroot}%{_bindir}
install -m 0755 target/release/liuguang    %{buildroot}%{_bindir}/liuguang
install -m 0755 target/release/liuguangctl %{buildroot}%{_bindir}/liuguangctl

install -d %{buildroot}%{_unitdir}
# 单元文件里 ExecStart 写的是 /usr/bin/liuguang；RPM 用 %{_bindir} 替换，
# 这样二进制在 /usr/bin 与 /usr/local/bin 两种布局下都能对上。
sed 's|/usr/bin/liuguang|%{_bindir}/liuguang|g' \
    packaging/liuguang.service > %{buildroot}%{_unitdir}/liuguang.service
chmod 0644 %{buildroot}%{_unitdir}/liuguang.service

install -d %{buildroot}%{_sysusersdir}
install -m 0644 packaging/liuguang.sysusers %{buildroot}%{_sysusersdir}/liuguang.conf

# 默认启用（fresh install 时 systemd_post 宏会执行 systemctl preset）。
install -d %{buildroot}%{_prefix}/lib/systemd/system-preset
install -m 0644 packaging/liuguang.preset %{buildroot}%{_prefix}/lib/systemd/system-preset/90-liuguang.preset

# 用户会话单元：**装到用户单元目录**（不是文档目录）。
# RPM 不能替某个登录用户 `enable`，但可以把它装到系统级用户单元目录，
# 这样 `systemctl --user enable --now liuguang` 开箱即用 —— 0.2.0-6 之前它是
# 文档文件，用户得先跑安装脚本，属于"看起来没这个功能"。
install -d %{buildroot}%{_userunitdir}
install -m 0644 packaging/liuguang-user.service %{buildroot}%{_userunitdir}/liuguang.service
install -d %{buildroot}%{_docdir}/liuguang
install -m 0755 scripts/install.sh %{buildroot}%{_docdir}/liuguang/install.sh
install -m 0755 scripts/install-user-session.sh %{buildroot}%{_docdir}/liuguang/install-user-session.sh
install -m 0755 scripts/build-gui.sh %{buildroot}%{_docdir}/liuguang/build-gui.sh

install -d %{buildroot}%{_sysconfdir}/liuguang
install -m 0644 config/example.toml %{buildroot}%{_sysconfdir}/liuguang/config.toml

# 默认媒体根（示例配置里的 slideshow.root / image_dirs / video_dirs）必须真实存在，
# 否则装完首次启动就报 INVALID_CONFIG「媒体根目录不存在」，用户被迫先读配置。
install -d %{buildroot}/srv/wallpapers/images
install -d %{buildroot}/srv/wallpapers/videos

%pre
# 创建系统用户 liuguang 与组 liuguang-ctl（idempotent；已存在则跳过）
%sysusers_create_compat %{_sysusersdir}/liuguang.conf

%post
%systemd_post liuguang.service
if [ "$1" -eq 1 ]; then
    # 首次安装：preset/systemd_post 宏只 enable 不 start（systemd-update-helper
    # install-system-units 语义），而"装完即用"是装饰屏的产品预期。
    # --no-block 避免 rpm 事务被服务启动阻塞；失败不阻断安装，管理员可手动 start。
    systemctl --no-block start liuguang.service >/dev/null 2>&1 || :
fi

%preun
%systemd_preun liuguang.service

%postun
%systemd_postun_with_restart liuguang.service

%files
%license LICENSE
%doc README.md
%doc docs/ARCHITECTURE.md
%doc docs/USAGE.md
%doc docs/CONFIGURATION.md
%doc docs/DEPLOYMENT.md
%doc docs/GUI.md
%doc docs/IPC_PROTOCOL.md
%doc docs/VERIFICATION.md
%doc docs/PROGRESS.md
%doc docs/DEVELOPMENT.md
%doc docs/TUI.md
%{_userunitdir}/liuguang.service
%{_docdir}/liuguang/install.sh
%{_docdir}/liuguang/install-user-session.sh
%{_docdir}/liuguang/build-gui.sh
%{_bindir}/liuguang
%{_bindir}/liuguang
%{_unitdir}/liuguang.service
%{_sysusersdir}/liuguang.conf
%{_prefix}/lib/systemd/system-preset/90-liuguang.preset
%dir %{_sysconfdir}/liuguang
%config(noreplace) %{_sysconfdir}/liuguang/config.toml

# 默认媒体根：属主 liuguang，组为控制组 liuguang-ctl。
# 2770 = 组可写 + setgid（新文件继承组），liuguang-ctl 成员无需 sudo 即可放素材；
# 守护进程（属主）与同组成员均可读写。不要再降回 0750（那会让"加组即用"失效）。
%dir %attr(2770,liuguang,liuguang-ctl) /srv/wallpapers
%dir %attr(2770,liuguang,liuguang-ctl) /srv/wallpapers/images
%dir %attr(2770,liuguang,liuguang-ctl) /srv/wallpapers/videos

%files -n liuguangctl
%{_bindir}/liuguangctl
%{_bindir}/liuguangctl

%changelog
* Fri Nov 27 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-14
- perf(mem): thumbnail workers 6 → 2 + malloc_trim after the queue drains,
  RGB converter allocated directly at display size (no rebuild after first frame),
  EGLImage cache 16 → 10, MALLOC_ARENA_MAX=2 in the user unit
- perf(mem): wl_shm pool created lazily — pure DMA-BUF playback no longer keeps
  16–48 MiB of memfd shared memory resident
- feat(gui): justified tile wall laid out by each item's real aspect ratio,
  pill/capsule toolbar with current-value labels, unified control shape language
  (.toolbar-pill / .icon-btn / .action-btn / .segmented)
* Mon Nov 24 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-13
- perf(daemon,media,vaapi,gui): reduce resident memory —— PNG streaming downscale
  (61 MP peak 244 MB → 16 MB), in-memory thumbnail cache capped at 48 entries /
  24 MB, GUI texture cache 240 → 64 entries, VA-API RGB slots 6 → 5 and prefetch
  depth 3 → 2 (configurable: LIUGUANG_RGB_SLOTS, LIUGUANG_VIDEO_PREFETCH).

* Tue Nov 24 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-12
- fix(vaapi): RgbConverter/SlotBuffer 共享 gbm Device（合成器仍握帧时不再释放设备）；
  部分构造失败、rebuild 与 Drop 统一清理槽位；cover 裁切按可见尺寸而非 16 对齐编码尺寸
- fix(wayland): DMA-BUF 导入失败/超时/早退路径清空 keepalive 与 serial，避免保活泄漏
- fix(daemon): 多重暂停原因（用户/全屏/熄屏/游戏/显存）叠加不再互相覆盖；新视频在
  全局暂停期间立即保持暂停；sync_performance 热重载同步所有开关；SetWallpaper 支持 rotation
- fix(game-watch): 识别 Steam reaper AppId、proton waitforexitandrun、主机模拟器与
  Minecraft Java；从 cmdline 还原被截断的 *.exe；排除僵尸/内核线程/自身二进制
- feat(gpu-pressure): AMD sysfs 显存兜底、采样线程幂等启动与 reconfigure
- fix(gui): 右键菜单恢复、缩略图淘汰后能重新解码、业务错误不再断连、播放列表草稿不被
  后台刷新覆盖、快捷键在输入框内不误触发、About 页解码器实时更新、设置页目录选择与
  gpu 阈值校验；状态刷新去掉每帧计数（不再每秒全页重建）
* Tue Nov 24 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-11
- perf(thumb): 缩略图 6 个并行 worker（可用 LIUGUANG_THUMB_WORKERS 调 1–8），
  冷缓存 12 张从约 15s 降到 3.5s；队列上限 8 → 32
- fix(gui): 封面占位纹理与真实纹理同尺寸，打开时不再从 3 列跳成 2 列；
  缩略图按 HiDPI 尺寸请求（不再放大发糊），首批按文件大小升序
- feat(gui): 详情面板时间显示本地时区、空的状态提示段隐藏

* Tue Nov 24 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-10
- fix(game-watch): Steam 运行时（pv-runtime/srt-*/pv-adverb）不再被当成游戏，
  修复"开着 Steam 客户端、一开机壁纸就暂停"

* Sun Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-9
- change: 显存让路阈值默认降为 85% 暂停 / 75% 恢复（提前让路，避免再次撞驱动断言）

* Sun Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-8
- fix(game-watch): 用户模式下 environ 被 Yama 挡住，改用 cmdline/comm 判据
- feat(game-watch): Lutris（lutris-wrapper/LUTRIS_GAME_UUID）与 wine 基础设施排除

* Sun Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-6
- Renamed the user-facing commands to liuguang / liuguangctl (symlinks keep liuguang /
  liuguangctl working); systemd units, config/state/cache/socket paths and the app name
  follow the new name, with a one-shot startup migration from the old layout (old dirs kept)
- New application icon and the "流光 / Liuguang" display name for the GUI package

* Sun Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-5
- fix(gui): a fired one-shot GLib source was removed by id, aborting the GUI (SIGABRT; all
  six core dumps share the same glib "Failed to remove source" panic). One-shot sources now
  only drop their SourceId; late callbacks are invalidated by generation. Regression test
  included (fails on the old code).

* Mon Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-4
- VA-API device reuse per render node: switching a wallpaper no longer pays 220-400ms of
  vaInitialize/driver context creation per media (1080p/2.7K open 300ms -> 15-40ms, 4K 590ms -> ~200ms)
- input probing budget tightened (probesize 5MB -> 32KB); 10bit detection and fps detection unchanged
- thumbnail disk cache actually persists: the user unit exports ReadWritePaths=%h/.cache/liuguang
  (ProtectHome=read-only used to make write_cache fail silently, so every restart lost the cache)
* Mon Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-2
- GUI responsiveness: thumbnail cache + queue limit (daemon CPU no longer saturates for ~50s when
  the GUI opens; repeat thumbnails 440ms -> 0.4ms), IPC accept loop 100ms -> 5ms, lazy/hover-limited
  cover frames, click-a-card-to-apply, per-output thumbnail metrics
- applying a wallpaper is accepted immediately (IPC receipt 700-1300ms -> 9-36ms) and the pipeline is
  built from the pending queue; fullscreen pause now keeps the top-level state consistent
- crash fix when switching/pausing video (VADisplay double teardown + EGLImage destruction order)
- PSI-based CPU load shedding actually uses PSI (was falling back to LoadAvg on this kernel)
- packaging: spec release 2; Arch pkgrel 2 (liuguang, liuguang-gui)
* Mon Nov 23 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-3
- user unit: ReadWritePaths=%h/.cache/liuguang so the thumbnail disk cache can be written
  (ProtectHome=read-only made ~/.cache read-only; the GUI re-decoded the whole library
  after every reboot)
- packaging: spec release 3; Arch pkgrel 3 (liuguang; gui unchanged)
* Sat Nov 22 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.3.0-1
- 0.3.0: Miyoho material matrix (1080p/2K/4K/8K x 30/60/120 fps) and GTK4/libadwaita GUI
  rework (library / playlists / properties / settings, hover frames, dark theme)
- auto compatibility: capability classify + background transcode cache; 8K H.264, 10bit
  and other sources the hardware cannot decode play through an 8bit cache copy
- 120fps presentation (frame-predicate pump), decode prefetch, PSI-based CPU load shedding
- compat/transcode/playback share one per-output decode consumer (VA-API vs GStreamer-only)
- packaging: spec release 1; Arch pkgrel 1 (liuguang, liuguang-gui)
* Sat Nov 22 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.2.0-13
- decode prefetch thread: 4K60 presentation 58.96 -> 60.0 fps with dropped_late 0.895/s -> 0
  (bounded channel depth 3, LIUGUANG_VIDEO_PREFETCH=0 keeps the old synchronous path)
- fps gate no longer accumulates wake-up lateness (was dropping one frame every 3-6s)
- VA-API node arbitration waits up to 500ms for the first frame (non-blocking producer)
- docs: ARCHITECTURE 8.6.1, PROGRESS, VERIFICATION_2026-11-22_GPU_RGB.md; spec release 13
* Sat Nov 22 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.2.0-10
- GPU NV12→XRGB8888 conversion for luma-only compositors: correct colours with
  zero-copy dma-buf output and no CPU readback (19% single core at 4K60 vs 92% CPU path)
- prefer the render node of the GPU driving the display when media.video_vaapi = auto
  (cross-GPU import capped the compositor at 20fps)
- spec release 10
* Sat Nov 22 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.2.0-9
- 为仅采样亮度的 Wayland 合成器（NV12 多平面导入异常）提供自动回避与 GPU RGB 转换支持
- export_probe 新增 --ppm（把导出的 dma-buf 写回 PPM，用于取证）；修其 NV12/NV21 色度顺序

* Sat Nov 22 2026 Liuguang Maintainers <maintainers@liuguang.invalid> - 0.2.0-8
- 60fps 快路径：DMA-BUF 同步导入（create_immed），4K60 源从 36fps 拉到 60fps
- 修同步导入路径的延长器泄漏（真机表现：跑 7 帧后供给停摆）
- 新增 LIUGUANG_DMABUF_IMMED 逃生开关

* Sat Nov 21 2026 Liuguang contributors <dev@liuguang.invalid> - 0.2.0-7
- Fix: liuguang-gui --self-test 会改掉正在使用的壁纸（apply 那一步真的换了画面，还把
  pinned 清零；set 还会把 fit 写回配置文件）。现在自检先记现场（壁纸/pinned/暂停），
  结束时按原样恢复，apply 的适配模式取该输出配置里的现值。
- 0.2.0-6 的三件事不变（运行时偏好写回、全屏应用让路、install.sh 双模式）。

* Sat Nov 21 2026 Liuguang contributors <dev@liuguang.invalid> - 0.2.0-6
- feat(config): 运行时偏好写回配置文件（`toml_edit`，只改用户动过的键、保留注释）
  - 适配/缩放/偏移/转场/旋转改完重启仍生效；写失败只告警一次，`state.json` 升 v3 兜底
- feat(power): 检测到全屏窗口（游戏/播放器）时暂停视频供给，退出后自动恢复
  - 协议 `zwlr_foreign_toplevel_management_unstable_v1`（niri/sway 有，GNOME/KDE 降级）
  - 新增 `[performance] pause_on_fullscreen`；`status` 报告 `pause_reason` 与 `fullscreen`
- feat(install): `scripts/install.sh --mode user|system`（`--check/--dry-run/--uninstall`）
  - 用户单元改为装到 `%{_userunitdir}/liuguang.service`：`systemctl --user enable --now liuguang` 开箱可用
  - system 模式幂等创建 `liuguang` 用户 / `liuguang-ctl` 组，并把调用者加入该组
- docs: ARCHITECTURE §8.7/§9.10、PROGRESS 0.2.0-6、VERIFICATION_2026-11-21_TOPLEVEL

* Sat Nov 21 2026 Liuguang contributors <dev@liuguang.invalid> - 0.2.0-5
- feat: 「只放这一项」——单视频循环 / 单张图片常驻，不再被 interval 轮换换走
  - 协议：`set_wallpaper` 新增可选 `pin`；新增 `set_pinned`（恢复轮换不用重发路径）
  - 状态：`status` 的每通道新增 `pinned`；`state.json` 持久化并在重启后恢复
  - GUI：「只放这一项（不轮换）」勾选框；CLI：`set --pin` / `pin on|off`

* Fri Nov 21 2026 Liuguang maintainers <dev@liuguang.local> - 0.2.0-4
- feat(core): DmaBufFrame 支持多 object/fd 与帧寿命延长器（NV12 在 NVIDIA 上是 2 个 fd）
- feat(vaapi): 新 crate liuguang-vaapi——libva 导出 + ffmpeg 解码，NV12 零拷贝直通
  （**本 spec 不启用** 该 feature：Fedora 主仓无 ffmpeg/libva 开发包）
- feat(daemon): media.video_vaapi（auto|off|节点路径）与按首帧修饰符选渲染节点
- feat(config): 媒体库统一到 ~/Pictures/Wallpapers（图片与视频同目录）
- docs: ARCHITECTURE §8.6、PROGRESS 0.2.0-4、VERIFICATION_2026-11-21_VAAPI_ZEROCOPY
* Fri Nov 21 2026 Liuguang maintainers <dev@liuguang.local> - 0.2.0-3
- feat(gui): 视频封面由守护进程抽帧（IPC get_thumbnail，PNG+base64），GUI 卡片显示真实帧
- feat(gui): 目标输出下拉（多显示器）、转场效果与时长控件
- fix(gui): 媒体库按页取全（此前只画第一页，路径靠后的视频根本找不到）
- test: liuguang-gui --self-test 扩到九项（outputs / thumbnail / transition）
- docs: ARCHITECTURE §9.6/§9.7、PROGRESS D4.1、D4 验证记录增补

* Fri Nov 21 2026 Liuguang maintainers <liuguang@example.invalid> - 0.2.0-2
- 视频零拷贝：合成器支持 zwp_linux_dmabuf_v1 且解码/渲染同卡时，DMA-BUF 直接导入成
  wl_buffer（嵌套 headless sway 实测 1080p60 单核 10% vs CPU 路径 49%，4K60 11% vs 80%）；
  跨 GPU 时如实拒绝并回退 CPU 路径，不再把视频降级成静态图
- wl_shm 后端不再让 vapostproc 缩放到模式尺寸（缩放交给合成器的 wp_viewport）：
  1920x1200@60 → 2560x1600 的 CPU 从 53% 降到 45%
- 新增 scripts/e2e/nested_sway_bench.sh（零拷贝对照基准）与回归验证记录

* Fri Nov 21 2026 Liuguang maintainers <liuguang@example.invalid> - 0.2.0-1
- 0.2 桌面转向：新增 Wayland 壁纸后端（wlr-layer-shell Background 层 + wl_shm 三槽池 +
  wp_fractional_scale_v1/wp_viewporter 分数缩放），`backend = "auto" | "wayland" | "drm" | "mock"`
- 视频快路径：整帧零转换上传 + 合成器 GPU 缩放（1080p30 的 commit 从 105-293ms 降到 11ms）
- 人格判定（system/user）：桌面会话用 XDG 路径、免 root、免 socket 参数；新增用户级
  systemd 单元与 scripts/install-user-session.sh
- 新增 GTK4 图形界面 apps/liuguang-gui（独立构建，不在本包内；scripts/build-gui.sh）
- 新增真机回归脚本：wayland_layer_check.py / user_persona_check.py / gui --self-test

* Wed Oct 28 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-12
- Log-only: restore_persisted explains why a persisted channel is skipped (output not connected)

* Wed Oct 28 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-11
- Fix: headless startup no longer sticks in InitializingDisplay; enters HeadlessReady (§4.3)
- Fix: do not create channels / decode pipelines for disconnected connectors on startup
- Fix: hotplug no longer claims StaticIdle without a frame actually shown
- Fix: config gap list no longer reports display.vsync=true as unimplemented (blocking commit = vsync)

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-9
- Demote per-frame DMA-BUF present log to debug (journal flooding ~20 lines/s)

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-8
- Escape macro references in spec comments (rpmbuild expanded them into bogus unit names)

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-7
- Start the service on first install (preset only enables; fresh installs stayed inactive)

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-6
- Ship systemd preset 90-liuguang.preset so fresh installs start on boot

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-5
- Require gstreamer1-plugins-good: MP4 demux (qtdemux) is mandatory, not optional
- Recommend openh264/libav software H.264 fallback decoders
- Make /srv/wallpapers group-writable (2770, setgid) for liuguang-ctl members
- Ship docs/DEPLOYMENT.md referenced by the systemd unit
- liuguangctl: hint about joining liuguang-ctl when the socket is inaccessible

* Thu Oct 22 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-4
- Fix persisted display fit casing (legacy "Cover" was rejected on restore)
- Silence rpmbuild macro-expansion warning in spec comment

* Wed Oct 21 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-3
- t69: implement set_static_wallpaper / clear_static_wallpaper

* Wed Oct 21 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-2
- Create the default media root /srv/wallpapers/{images,videos}

* Tue Oct 20 2026 Liuguang maintainers <liuguang@example.invalid> - 0.1.0-1
- Initial RPM packaging (source build with vendored crates)
