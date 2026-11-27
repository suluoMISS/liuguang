//! 流光（Liuguang）壁纸 GUI（GTK4 + libadwaita）。
//!
//! 结构：
//! - [`App`] 持有全部页面与共享状态（GTK 主线程，`Rc`）；
//! - [`ipc`] 后台线程负责与守护进程通信，`thumbs` 线程负责 PNG 解码；
//! - 页面模块：`library` / `playlists` / `displays` / `settings` / `details`；
//! - GUI **只**通过 IPC 操作守护进程，崩了也只崩自己（独立进程、无共享内存）。

use gtk::{gdk, glib};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use liuguang_core::persona::{Persona, PersonaArg};
use liuguang_ipc::protocol::{
    CompatInfo, CompatState, EventKind, PrepareMediaResult, PrepareState, StatusResult,
};

mod details;
mod displays;
mod grid;
mod ipc;
mod library;
mod playlists;
mod self_test;
mod service;
mod settings;
mod shortcuts;
mod state;
mod thumbs;
mod ui;

use ipc::{ApplyRequest, Cmd, Upd};
use state::State;

/// 应用 id（与打包 desktop 文件的 `StartupWMClass` 对齐）。
const APP_ID: &str = "io.github.suluoMISS.Liuguang";
/// 应用图标的主题名（`packaging/icons/liuguang.svg` 安装成 hicolor 主题里的 `liuguang`）。
const APP_ICON: &str = "liuguang";

/// 命令行参数。
#[derive(Debug, Clone, Default)]
struct Args {
    /// `--persona auto|system|user`。
    persona: Option<String>,
    /// 覆盖控制 socket。
    socket: Option<PathBuf>,
    /// 覆盖配置文件（设置页读写、偏好文件同目录）。
    config: Option<PathBuf>,
    /// 跑一遍无窗口自检后退出。
    self_test: bool,
    /// `--demo-prepare <名称|路径>`：启动后自动选中并准备该素材（截图/演示用）。
    ///
    /// why：wayland 合成器没有输入注入工具（headless sway 的 seat 没有 pointer），
    /// 截图脚本无法点"准备（转码）"按钮；这个开关让"转码进度"这一屏可复现。
    demo_prepare: Option<String>,
    /// `--demo-apply <名称|路径>`：启动后自动对这个（可能是冷缓存的）素材点「应用」。
    ///
    /// 用于验证"NEEDS_PREPARATION → 自动准备 → 转码完成 → 不重复下发应用"这条链路：
    /// 没有输入注入工具时，只能由进程自己点这一下。
    demo_apply: Option<String>,
    /// 启动后直接打开某页（截图脚本用）。
    open_page: Option<String>,
    /// 启动后直接弹出快捷键窗口（截图脚本用：headless 环境没有按键注入）。
    show_shortcuts: bool,
}

/// 共享的应用上下文。
/// 一次被 `NEEDS_PREPARATION` 拦下的应用请求：转码完成后自动重试一次。
#[derive(Debug, Clone)]
pub struct PendingApply {
    request: ApplyRequest,
    /// 已经重试过一次；再失败就不再自动重试。
    tried: bool,
}

/// 底部提示栏右侧快捷动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAction {
    None,
    EnableUserService,
    OpenSettings,
}

pub struct App {
    /// 普通命令（列表刷新、缩略图、指标轮询…）。
    pub tx: std::sync::mpsc::Sender<Cmd>,
    /// 交互命令专用连接：不与缩略图排队（t40）。
    pub urgent: std::sync::mpsc::Sender<Cmd>,
    pub thumb_tx: std::sync::mpsc::Sender<thumbs::ThumbCmd>,
    pub state: RefCell<State>,
    pub window: adw::ApplicationWindow,
    pub toast: adw::ToastOverlay,
    pub search: gtk::SearchEntry,
    /// 只在「媒体库」页显示的控件（搜索框、视图切换、属性面板开关、库工具栏）。
    pub library_only: std::cell::RefCell<Vec<gtk::Widget>>,
    pub stack: gtk::Stack,
    pub page_title: adw::WindowTitle,
    pub cards: RefCell<HashMap<String, Vec<Rc<library::Card>>>>,
    pub textures: RefCell<HashMap<(String, u64), gdk::Texture>>,
    /// 纹理写入顺序（用于近似 LRU 淘汰，见 `store_texture`）。
    pub texture_order: RefCell<std::collections::VecDeque<(String, u64)>>,
    pub pending: RefCell<HashSet<(String, u64)>>,
    /// 应用历史（`←` 用来回退到上一张壁纸，进程内保留最近 64 条）。
    pub history: RefCell<Vec<String>>,
    /// 缩略图请求尺寸（物理像素）。随卡片宽度与窗口缩放一起算，HiDPI 下更清晰。
    pub thumb_max: std::cell::Cell<(u32, u32)>,
    pub library: RefCell<Option<Rc<library::LibraryView>>>,
    pub details: RefCell<Option<Rc<details::DetailsView>>>,
    pub playlists: RefCell<Option<Rc<playlists::PlaylistsView>>>,
    pub displays: RefCell<Option<Rc<displays::DisplaysView>>>,
    pub settings: RefCell<Option<Rc<settings::SettingsView>>>,
    pub about_decoder: RefCell<Option<gtk::Label>>,
    pub config_path: PathBuf,
    pub persona: Persona,
    pub socket: PathBuf,
    pub cpu: gtk::Label,
    pub mem: gtk::Label,
    pub fps: gtk::Label,
    pub status_left: gtk::Label,
    pub thumb_accents: RefCell<HashMap<String, (u8, u8, u8)>>,
    pub pause_button: gtk::Button,
    /// 每个媒体的兼容性判定（`get_media`）。`None` = 后端没给/还没取回。
    pub compat: RefCell<HashMap<String, Option<CompatInfo>>>,
    /// 正在等 `get_media` 回包的媒体，避免重复请求。
    pub compat_pending: RefCell<HashSet<String>>,
    /// 转码准备状态（`prepare_media`），按媒体 id（= 路径）索引。
    pub prepare: RefCell<HashMap<String, PrepareMediaResult>>,
    /// 被 `NEEDS_PREPARATION` 拦下的应用请求（转码完成后自动重试一次）。
    pub auto_apply: RefCell<Option<PendingApply>>,
    /// 错误通知去重：同一个错误码 5s 内只弹一次 toast。
    pub notify_seen: RefCell<HashMap<String, Instant>>,
    /// 常驻错误区（状态条上方）：完整错误文本，可换行/可选中复制，不靠 toast 承载长文本。
    pub notice_bar: gtk::Revealer,
    pub notice_label: gtk::Label,
    pub notice_action: gtk::Button,
    pub notice_action_kind: Cell<NoticeAction>,
    pub sidebar_top: gtk::ListBox,
    pub sidebar_bottom: gtk::ListBox,
    /// 启动时对 `NeedsConversion` 条目做过的状态查询（每个条目只查一次）。
    pub compat_probed: RefCell<HashSet<String>>,
    /// 是否已经用过启动时的那一次自动缓存查询（同时作为“已提示”标志）。
    pub probe_notice_shown: Cell<bool>,
    /// 上一次 Status 的指纹：只有变化才重刷界面，避免空闲时每秒重布局。
    pub last_status_key: RefCell<String>,
    /// 转码完成后被推迟的重试：等一次新的 `Status` 再决定（先看 daemon 是否已自动播缓存）。
    pub deferred_retry: RefCell<Option<(String, Option<String>)>>,
    /// 截图/演示：启动后自动准备的目标（名称或路径），只触发一次。
    pub demo_prepare: Option<String>,
    /// `demo_prepare` 是否已触发。
    pub demo_prepare_done: Cell<bool>,
    /// 截图/演示：启动后自动应用的素材（名称或路径），只触发一次。
    pub demo_apply: Option<String>,
    pub demo_apply_done: Cell<bool>,
    /// 这条卡片正在悬停预览的帧（`(id, at_ms)`）：离开即移除，回包按它决定是否采用。
    pub hover_keys: RefCell<HashSet<(String, u64)>>,
    /// 刚下发的应用请求（路径 + 时间）：下一次 `Status` 用来判断"已切换 / 已恢复播放"。
    pub apply_watch: RefCell<Option<(String, Instant)>>,
    /// 应用前是否处于暂停（`pause_reason` 非空）：是则状态转为播放后提示"已恢复播放"。
    pub resume_watch: Cell<Option<(bool, Instant)>>,
    /// 单击应用的去重：双击的第二次 press 不重复下发（同一 id 500ms 内）。
    pub last_click_apply: RefCell<Option<(String, Instant)>>,
}

impl App {
    /// 发一条命令给工作线程（线程退出时静默丢弃：GUI 不该因为 daemon 挂了而 panic）。
    ///
    /// 交互命令走独立连接（`Cmd::is_urgent`）：封面解码占满普通连接时，
    /// 「应用 / 暂停 / 继续 / 下一个」仍然能立刻被 daemon 读到。
    pub fn send(&self, cmd: Cmd) {
        let result = if cmd.is_urgent() {
            self.urgent.send(cmd)
        } else {
            self.tx.send(cmd)
        };
        let _ = result;
    }

    /// 请求一帧悬停预览（记录在 `hover_keys`，离开时整条撤掉）。
    pub fn request_hover_thumb(&self, id: &str, at_ms: u64) {
        self.hover_keys.borrow_mut().insert((id.to_owned(), at_ms));
        self.request_thumb(id, at_ms);
    }

    /// 离开卡片：撤销悬停请求。未回包的帧从 `pending` 里拿掉，下次悬停会重新请求；
    /// 已经回包但尚未贴上的帧由 `thumb_still_wanted` 丢弃。
    pub fn cancel_hover_thumbs(&self, id: &str) {
        let keys: Vec<(String, u64)> = self
            .hover_keys
            .borrow()
            .iter()
            .filter(|(media, _)| media == id)
            .cloned()
            .collect();
        for key in keys {
            self.hover_keys.borrow_mut().remove(&key);
            if !self.textures.borrow().contains_key(&key) {
                self.pending.borrow_mut().remove(&key);
            }
        }
    }

    /// 存一张封面纹理；超过上限时按写入顺序淘汰最旧的一张。
    ///
    /// 为什么要设上限：悬停预览会为每张卡片额外缓存 2 帧（`HOVER_PREVIEW_TIMES`），
    /// 大库（几千项）无上限缓存会把内存吃光；淘汰后卡片再次可见时
    /// `prime_picture` 拿不到纹理会走 `refresh_visible` 重新请求，行为一致。
    pub fn store_texture(&self, key: (String, u64), texture: gdk::Texture) {
        // 每张封面纹理 ≈ `max_width × max_height × 4` 字节（HiDPI 下 480×300 ≈ 0.55 MB）。
        // 40 张（≈22 MB）足以覆盖当前可视区与相邻滚动缓冲，
        // 更远的多余纹理淘汰后由 `refresh_visible` 重新请求（有磁盘缓存，代价只有 base64 解码）。
        const TEXTURE_CACHE_CAP: usize = 48;
        let mut textures = self.textures.borrow_mut();
        textures.insert(key.clone(), texture);
        let mut order = self.texture_order.borrow_mut();
        order.retain(|k| k != &key);
        order.push_back(key);
        while order.len() > TEXTURE_CACHE_CAP {
            if let Some(old) = order.pop_front() {
                textures.remove(&old);
            }
        }
    }

    /// 这张缩略图现在还需要吗？（回包在途时用户可能已经滚走/移开）
    ///
    /// - `at_ms == 0`（封面）：只有仍在当前视口集合里，或它是当前选中项时才采用；
    /// - `at_ms > 0`（悬停帧）：只有该卡片的悬停预览还开着、且正是这一帧才采用。
    #[must_use]
    pub fn thumb_still_wanted(&self, id: &str, at_ms: u64) -> bool {
        if at_ms == 0 {
            let wanted = self
                .library
                .borrow()
                .as_ref()
                .is_some_and(|view| view.wanted.borrow().contains(id));
            if wanted {
                return true;
            }
            // 选中项或正在显示的壁纸封面始终有用（属性面板 / 悬浮 Dock / 多屏预览立即可见并提取氛围色）。
            let state = self.state.borrow();
            if state.selected.as_deref() == Some(id) || state.current_media().as_deref() == Some(id)
            {
                return true;
            }
            return state
                .status
                .as_ref()
                .is_some_and(|st| st.channels.iter().any(|ch| ch.media.as_deref() == Some(id)));
        }
        self.hover_keys.borrow().contains(&(id.to_owned(), at_ms))
    }

    /// 单击卡片：立即应用（对标 Wallpaper Engine），同一张卡 500ms 内的第二次
    /// press（双击）不重复下发。
    pub fn apply_on_click(self: &Rc<Self>, id: &str) {
        let now = Instant::now();
        {
            let mut last = self.last_click_apply.borrow_mut();
            if !click_should_apply(last.as_ref().map(|(id, at)| (id.as_str(), *at)), id, now) {
                return;
            }
            *last = Some((id.to_owned(), now));
        }
        self.apply_media(id, false);
    }

    /// 右上角 toast。
    pub fn toast(&self, message: &str) {
        if std::env::var_os("LIUGUANG_GUI_DEBUG").is_some() {
            eprintln!("[toast] {message}");
        }
        self.toast.add_toast(adw::Toast::new(message));
    }

    /// 带全文 tooltip 的短提示：长错误不靠这条 5s 的 toast 承载，
    /// 但用户把鼠标停在 toast 上应该能读到全文（常驻错误区里也有）。
    pub fn toast_hint(&self, hint: &str, full: &str) {
        let label = gtk::Label::new(Some(hint));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(72);
        label.set_tooltip_text(Some(full));
        let toast = adw::Toast::new(hint);
        toast.set_custom_title(Some(&label));
        toast.set_timeout(5);
        if std::env::var_os("LIUGUANG_GUI_DEBUG").is_some() {
            eprintln!("[toast] {hint}");
        }
        self.toast.add_toast(toast);
    }

    /// 落盘偏好（收藏、视图模式、排序/筛选、卡片尺寸、属性面板开关）。
    pub fn save_prefs(&self) {
        let (prefs, path) = {
            let state = self.state.borrow();
            (state.prefs.clone(), state.prefs_path.clone())
        };
        if let Err(error) = prefs.save(&path) {
            self.toast(&format!("偏好保存失败：{error}"));
        }
    }

    /// 请求一张缩略图（带去重：缓存命中或已在途就不发）。
    pub fn request_thumb(&self, id: &str, at_ms: u64) {
        let key = (id.to_owned(), at_ms);
        if self.textures.borrow().contains_key(&key) || self.pending.borrow().contains(&key) {
            return;
        }
        if std::env::var_os("LIUGUANG_GUI_DEBUG").is_some() {
            eprintln!("[thumb] request {id}@{at_ms}");
        }
        self.pending.borrow_mut().insert(key);
        let (max_width, max_height) = self.thumb_max.get();
        self.send(Cmd::Thumbnail {
            id: id.to_owned(),
            at_ms,
            max_width,
            max_height,
        });
    }

    /// 选中一张媒体（库页单选）。
    pub fn select(self: &Rc<Self>, id: &str) {
        self.state.borrow_mut().selected = Some(id.to_owned());
        library::refresh_selection(self);
        if let Some(view) = self.details.borrow().as_ref() {
            details::refresh(self, view);
        }
    }

    /// 把"当前正在播的那张"压进历史栈（`←` 回退用）；同一张不重复压。
    fn record_history(&self, next: &str) {
        if let Some(current) = self.state.borrow().status.as_ref().and_then(|status| {
            status
                .channels
                .first()
                .and_then(|channel| channel.media.clone())
        }) {
            if current == next {
                return;
            }
            let mut history = self.history.borrow_mut();
            if history.last() != Some(&current) {
                history.push(current);
            }
            while history.len() > 64 {
                history.remove(0);
            }
        }
    }

    /// 应用媒体：适配参数取属性面板当前值，输出取面板选择。
    pub fn apply_media(self: &Rc<Self>, id: &str, pin: bool) {
        let path = self.state.borrow().item(id).map(|item| item.path.clone());
        let Some(path) = path else {
            self.toast("这个媒体已经不在库里了");
            return;
        };
        self.record_history(&path);
        let snapshot = self
            .details
            .borrow()
            .as_ref()
            .map(|view| details::transform_snapshot(view));
        let (fit, zoom, offset_x, offset_y, rotation, brightness, output) = snapshot.map_or_else(
            || ("cover".to_owned(), 1.0, 0.0, 0.0, 0, 1.0, None),
            |snapshot| {
                let output = self
                    .details
                    .borrow()
                    .as_ref()
                    .and_then(|view| details::selected_output(view));
                (
                    snapshot.fit,
                    snapshot.zoom,
                    snapshot.offset_x,
                    snapshot.offset_y,
                    snapshot.rotation,
                    snapshot.brightness,
                    output,
                )
            },
        );
        self.apply_request(ApplyRequest {
            path,
            output,
            fit,
            zoom,
            offset_x,
            offset_y,
            rotation,
            brightness,
            pin,
        });
    }

    /// 下发一次应用；`Unsupported` 素材在前端就拦掉（应用必然失败）。
    pub fn apply_request(self: &Rc<Self>, request: ApplyRequest) {
        if self
            .compat_of(&request.path)
            .is_some_and(|compat| compat.state == CompatState::Unsupported)
        {
            let reason = self
                .compat_of(&request.path)
                .map(|compat| ui::compat_tooltip(&compat))
                .unwrap_or_else(|| "这个素材无法播放".to_owned());
            self.toast(&format!("不能应用：{reason}"));
            return;
        }
        // 即时反馈：用户点下去先看到"正在切换…"，不必等 1s 心跳或封面解码。
        let was_paused = self.state.borrow().status.as_ref().is_some_and(|status| {
            status.paused
                || status
                    .channels
                    .iter()
                    .any(|channel| channel.pause_reason.is_some())
        });
        self.apply_watch
            .borrow_mut()
            .replace((request.path.clone(), Instant::now()));
        self.resume_watch.set(Some((was_paused, Instant::now())));
        self.status_left.set_text("正在切换…");
        self.send(Cmd::Apply(Box::new(request)));
    }

    /// 把某个媒体应用到指定输出（显示器页"一键应用"）。
    pub fn apply_media_to_output(self: &Rc<Self>, path: &str, output: Option<&str>) {
        self.record_history(path);
        let (fit, zoom, offset_x, offset_y, rotation, brightness) = {
            let state = self.state.borrow();
            let config = state.config.as_ref();
            let output_config =
                output.and_then(|name| config.and_then(|config| config.outputs.get(name)));
            output_config.map_or_else(
                || ("cover".to_owned(), 1.0, 0.0, 0.0, 0, 1.0),
                |config| {
                    (
                        config.fit.as_str().to_owned(),
                        config.zoom,
                        config.offset_x,
                        config.offset_y,
                        details::rotation_degrees(config.rotation),
                        config.brightness,
                    )
                },
            )
        };
        self.apply_request(ApplyRequest {
            path: path.to_owned(),
            output: output.map(str::to_owned),
            fit,
            zoom,
            offset_x,
            offset_y,
            rotation,
            brightness,
            pin: false,
        });
    }

    /// 某个媒体的兼容性判定（`None` = 还没取回/后端没给）。
    pub fn compat_of(&self, id: &str) -> Option<CompatInfo> {
        self.compat.borrow().get(id).cloned().flatten()
    }

    /// 请求一个媒体的兼容性（去重：已缓存/已在途就不发）。
    pub fn request_compat(self: &Rc<Self>, id: &str) {
        if self.compat.borrow().contains_key(id)
            || !self.compat_pending.borrow_mut().insert(id.to_owned())
        {
            return;
        }
        self.send(Cmd::MediaCompat { id: id.to_owned() });
    }

    /// 库里所有视频都补一次兼容性（图片永远是 Playable，不必查）。
    pub fn sync_compat(self: &Rc<Self>) {
        let ids: Vec<String> = self
            .state
            .borrow()
            .rows
            .iter()
            .filter(|item| item.kind == "video")
            .map(|item| item.id.clone())
            .collect();
        // 库变短时清掉已不存在条目的缓存，避免 map 越滚越大。
        {
            let present: HashSet<&String> = ids.iter().collect();
            self.compat
                .borrow_mut()
                .retain(|id, _| present.contains(id));
            self.prepare
                .borrow_mut()
                .retain(|id, _| present.contains(id));
        }
        for id in ids {
            self.request_compat(&id);
        }
    }

    /// 对 `NeedsConversion` 的条目做一次**状态查询**，让热缓存重启后直接显示“已转换”。
    ///
    /// `prepare_media(force=false)` 对磁盘上已有缓存的素材由 daemon 直接回 Ready
    /// （`transcode::submit` 命中 `spec.cache_path` 后不再转码），所以这是无副作用的查询；
    /// 但**冷素材会真的排队转码**。t28 F3 实测：旧实现启动时一次补 4 条，冷库直接连发
    /// 4–5 个 ffmpeg 把 CPU 打满。现在：
    /// * 只查**当前选中 + 正在使用的**那条（都是"当前页可见"的语义），不扫全库；
    /// * 整个进程只自动查询 1 条（冷素材会触发真实转码），`compat_probed` 去重，
    ///   用户自己点过「准备」的不再重复提交；
    /// * 第一次自动查询时 toast 一次，告诉用户其余条目需要手动点「准备（转码）」。
    pub fn probe_prepared(self: &Rc<Self>) {
        // 整个进程只自动查询一次：冷素材的查询会触发真实转码，启动阶段不能变成
        // "每个 compat 回包都排一个 ffmpeg"（t28 F3 实测 4–5 个就够把 CPU 打满）。
        if self.probe_notice_shown.get() {
            return;
        }
        let candidates: Vec<String> = {
            let state = self.state.borrow();
            let compat = self.compat.borrow();
            let cards = self.cards.borrow();
            let selected = state.selected.clone();
            let current = state.current_media();
            state
                .rows
                .iter()
                .filter(|item| item.kind == "video")
                .filter(|item| {
                    compat
                        .get(&item.id)
                        .cloned()
                        .flatten()
                        .is_some_and(|info| info.state == CompatState::NeedsConversion)
                })
                // 只查"当前页看得见"的两类：选中的、以及正在当壁纸用的。
                .filter(|item| {
                    selected.as_deref() == Some(item.id.as_str())
                        || current.as_deref() == Some(item.path.as_str())
                })
                .filter(|item| cards.contains_key(&item.id))
                .map(|item| item.id.clone())
                .collect()
        };
        let mut budget = 1usize;
        for id in candidates {
            if budget == 0 {
                break;
            }
            if self.prepare.borrow().contains_key(&id) {
                continue;
            }
            if !self.compat_probed.borrow_mut().insert(id.clone()) {
                continue;
            }
            budget -= 1;
            self.send(Cmd::Prepare {
                path: id,
                force: false,
            });
            if !self.probe_notice_shown.get() {
                self.probe_notice_shown.set(true);
                self.toast("启动只自动查询 1 个素材的缓存状态；其余需要转码的请点「准备（转码）」");
            }
        }
    }

    /// 开始（或重试）后台转码；`force = false` 时是幂等查询。
    pub fn prepare_media(self: &Rc<Self>, id: &str) {
        let path = self
            .state
            .borrow()
            .item(id)
            .map_or_else(|| id.to_owned(), |item| item.path.clone());
        if !self.prepare.borrow().contains_key(&path) {
            self.prepare.borrow_mut().insert(
                path.clone(),
                PrepareMediaResult {
                    state: PrepareState::Queued,
                    cache_path: None,
                    progress_permille: 0,
                    message: None,
                },
            );
        }
        self.send(Cmd::Prepare {
            path: path.clone(),
            force: false,
        });
        library::refresh_compat(self);
        if let Some(view) = self.details.borrow().as_ref() {
            details::refresh(self, view);
        }
        self.toast("已开始准备（后台转码，源文件不改）");
    }

    /// 错误通知：全文进常驻错误区，toast 只提示第一行；同一错误码 5s 内不重复弹。
    ///
    /// 为什么按错误码而不是按消息文本去重：同一次失败会同时产生命令错误
    /// （`Upd::Error`，包装成「壁纸应用失败：…」）和 daemon 的 `EventKind::Error`
    /// （原始消息），按文本比永远不相等——用户会看到两条。归一化后同一个
    /// 码只提示一次，完整信息仍都在常驻区里。
    pub fn notify_error(self: &Rc<Self>, code: &str, message: &str) {
        let (display_msg, action_kind, action_label) = if message.contains("连不上 liuguang")
            && self.persona == Persona::User
        {
            (
                "后台壁纸服务尚未运行，点右侧按钮即可一键初始化配置并启动服务。".to_owned(),
                NoticeAction::EnableUserService,
                Some("一键启动并启用服务"),
            )
        } else if message.contains("未配置 [library]") {
            (
                "尚未配置壁纸目录，点右侧前往「设置 → 媒体来源」添加图片或视频文件夹。".to_owned(),
                NoticeAction::OpenSettings,
                Some("前往设置"),
            )
        } else {
            (message.to_owned(), NoticeAction::None, None)
        };

        self.notice_label.set_text(&display_msg);
        self.notice_label.set_tooltip_text(Some(message));
        self.notice_action_kind.set(action_kind);
        if let Some(label) = action_label {
            self.notice_action.set_label(label);
            self.notice_action.set_sensitive(true);
            self.notice_action.set_visible(true);
        } else {
            self.notice_action.set_visible(false);
        }
        self.notice_bar.set_reveal_child(true);
        let now = Instant::now();
        if should_notify(&mut self.notify_seen.borrow_mut(), code, now) {
            let hint = first_line(&display_msg);
            self.toast_hint(&hint, message);
        }
    }

    /// 收藏/取消收藏（立即落盘）。
    pub fn toggle_favorite(self: &Rc<Self>, id: &str) {
        let on = self.state.borrow_mut().toggle_favorite(id);
        self.save_prefs();
        if let Some(view) = self.library.borrow().as_ref() {
            library::rebuild(self, view);
        }
        library::refresh_selection(self);
        if let Some(view) = self.details.borrow().as_ref() {
            details::refresh(self, view);
        }
        self.toast(if on {
            "已加入收藏"
        } else {
            "已取消收藏"
        });
    }

    /// 切页（侧栏/命令行共用）。
    pub fn set_page(self: &Rc<Self>, page: &str) {
        let (title, subtitle) = match page {
            "playlists" => ("播放列表", "把媒体排成序列，按间隔轮换"),
            "displays" => ("显示器", "每个输出的实时状态"),
            "settings" => ("设置", "媒体来源、性能与桌面集成"),
            "about" => ("关于", "关于流光"),
            _ => ("媒体库", "悬停预览 · 单击应用"),
        };
        self.stack.set_visible_child_name(page);
        // 搜索框/排序/筛选/视图切换只对媒体库有意义；留在别的页面上既占地方也误导。
        for widget in self.library_only.borrow().iter() {
            widget.set_visible(page == "library");
        }
        self.page_title.set_title(title);
        self.page_title.set_subtitle(subtitle);
        if page == "library" {
            // 库页每次回来都刷一下选中态（别的页面可能改过壁纸）。
            library::refresh_selection(self);
        }
        if page == "displays" {
            if let Some(view) = self.displays.borrow().as_ref() {
                displays::refresh(self, view);
            }
        }
        if page == "settings" {
            if let Some(view) = self.settings.borrow().as_ref() {
                settings::refresh(self, view);
            }
        }
        if page == "playlists" {
            if let Some(view) = self.playlists.borrow().as_ref() {
                playlists::refresh(self, view);
            }
        }
        self.state.borrow_mut().prefs.show_page = page.to_owned();
    }
}

thread_local! {
    /// 让 [`App`] 活到进程退出。
    ///
    /// GTK 只持有控件树：控件回调里存的是 `Weak<App>`（否则 `App` ↔ 控件互相
    /// 强引用会泄漏），所以 `build_ui` 返回后必须有人继续持有这个 `Rc`，
    /// 否则 IPC/缩略图消费循环下一次 `upgrade()` 就失败、界面永远停在初始态。
    static APP_KEEPALIVE: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// 按当前卡片宽度与窗口缩放，算出缩略图该请求多大（上限 768 物理像素）。
pub fn refresh_thumb_size(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let scale = f64::from(window.scale_factor()).max(1.0);
    let width = f64::from(app.state.borrow().prefs.card_size).clamp(160.0, 360.0);
    // ×1.8：磁贴宽度由行高和素材比例决定，2.35:1 的宽卡能到 ~1.8×card_size。
    let max_w = (width * 1.8 * scale).round().clamp(320.0, 768.0);
    let max_h = (max_w * 0.625).round();
    app.thumb_max.set((max_w as u32, max_h as u32));
}

fn main() -> glib::ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return glib::ExitCode::FAILURE;
        }
    };
    if args.self_test {
        let persona = resolve_persona(args.persona.as_deref());
        let socket = args.socket.clone().unwrap_or_else(|| persona.socket_path());
        // 与 GUI 同一优先级：显式 `--config` 优先，否则用 persona 默认配置。
        let config_path = args.config.clone().unwrap_or_else(|| persona.config_path());
        std::process::exit(self_test::run(&socket, &config_path));
    }
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.connect_startup(|_| ui::install_css());
    application.connect_activate(move |application| {
        build_ui(application, args.clone());
    });
    // 我们自己在 `parse_args` 里解析参数，但 `g_application_run` 也会扫一遍
    // argv —— 不把选项登记给 GApplication，`--socket` 就会被当成"未知选项"，
    // 应用还没激活就退出。这里把四个带值选项声明成字符串选项，让 GLib 放过它们。
    for name in [
        "persona",
        "socket",
        "config",
        "open-page",
        "demo-prepare",
        "demo-apply",
    ] {
        application.add_main_option(
            name,
            glib::Char::from(0),
            glib::OptionFlags::NONE,
            glib::OptionArg::String,
            "",
            None,
        );
    }
    application.add_main_option(
        "show-shortcuts",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "",
        None,
    );
    application.add_main_option(
        "self-test",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "",
        None,
    );
    application.run()
}

fn parse_args<I: Iterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--persona" => {
                parsed.persona = Some(
                    iter.next()
                        .ok_or_else(|| "--persona 需要一个取值（auto/system/user）".to_owned())?,
                );
            }
            "--socket" => {
                parsed.socket = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--socket 需要一个路径".to_owned())?,
                ));
            }
            "--config" => {
                parsed.config = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--config 需要一个路径".to_owned())?,
                ));
            }
            "--demo-prepare" => {
                parsed.demo_prepare = Some(
                    iter.next()
                        .ok_or_else(|| "--demo-prepare 需要一个名称或路径".to_owned())?,
                );
            }
            "--demo-apply" => {
                parsed.demo_apply = Some(
                    iter.next()
                        .ok_or_else(|| "--demo-apply 需要一个名称或路径".to_owned())?,
                );
            }
            "--open-page" => {
                parsed.open_page = Some(
                    iter.next()
                        .ok_or_else(|| "--open-page 需要一个页面名".to_owned())?,
                );
            }
            "--show-shortcuts" => parsed.show_shortcuts = true,
            "--self-test" => parsed.self_test = true,
            "-h" | "--help" | "help" => return Err(help_text().to_owned()),
            other => return Err(format!("未知参数 {other}\n\n{}", help_text())),
        }
    }
    Ok(parsed)
}

fn help_text() -> &'static str {
    "流光 · 动态壁纸 GUI

用法: liuguang-gui [选项]

选项:
  --persona <auto|system|user>  人格（默认按环境判定）
  --socket <PATH>               控制 socket（默认按人格）
  --config <PATH>               配置文件（设置页读写、偏好文件同目录）
  --open-page <library|playlists|displays|settings|about>  启动后打开的页面
  --show-shortcuts              启动后弹出快捷键说明（截图/演示用）
  --demo-prepare <名称|路径>    启动后自动选中并准备该素材（截图/演示用）
  --demo-apply <名称|路径>      启动后自动对该素材点「应用」（验证转码后重试链路）
  --self-test                   无窗口自检（会恢复原壁纸/pin/暂停状态）
  -h, --help                    显示帮助"
}

fn resolve_persona(value: Option<&str>) -> Persona {
    value
        .map(|value| match value {
            "auto" => PersonaArg::Auto,
            "system" | "sys" | "root" => PersonaArg::System,
            "user" | "session" | "desktop" => PersonaArg::User,
            other => {
                eprintln!("未知人格 {other}，按 auto 处理");
                PersonaArg::Auto
            }
        })
        .map_or_else(|| Persona::detect(None), |arg| arg.resolve(None))
}

fn build_ui(gtk_app: &adw::Application, args: Args) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);

    let persona = resolve_persona(args.persona.as_deref());
    let socket = args.socket.clone().unwrap_or_else(|| persona.socket_path());
    let config_path = args.config.clone().unwrap_or_else(|| persona.config_path());
    let prefs_path = config_path.parent().map_or_else(
        || PathBuf::from("gui.json"),
        |parent| parent.join("gui.json"),
    );
    let state = State::new(prefs_path);
    let prefs = state.prefs.clone();

    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("流光")
        .default_width(prefs.window_width.clamp(720, 8000))
        .default_height(prefs.window_height.clamp(480, 8000))
        .build();
    if prefs.window_maximized {
        window.maximize();
    }
    // CSS 里 `window.liuguang` 的背景规则依赖这个 class（没有它会静默不生效）。
    window.add_css_class("liuguang");

    // —— 顶栏 ——
    let page_title = adw::WindowTitle::new("媒体库", "悬停预览 · 单击应用");
    let content_header = adw::HeaderBar::new();
    content_header.set_title_widget(Some(&page_title));

    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("flat");
    refresh.set_tooltip_text(Some("重新扫描媒体目录（F5）"));
    content_header.pack_start(&refresh);

    let pause_button = gtk::Button::from_icon_name("media-playback-pause-symbolic");
    pause_button.add_css_class("flat");
    pause_button.set_tooltip_text(Some("暂停/继续（Space）"));
    content_header.pack_start(&pause_button);

    let next_button = gtk::Button::from_icon_name("media-skip-forward-symbolic");
    next_button.add_css_class("flat");
    next_button.set_tooltip_text(Some("切换到下一项（→）"));
    content_header.pack_start(&next_button);

    let status_left = gtk::Label::new(Some("正在连接守护进程…"));
    status_left.add_css_class("small");
    status_left.set_xalign(0.0);
    status_left.set_wrap(true);
    status_left.set_max_width_chars(36);
    status_left.set_selectable(true);

    let cpu = gtk::Label::new(Some("CPU —"));
    let mem = gtk::Label::new(Some("内存 —"));
    let fps = gtk::Label::new(Some("— fps"));
    for label in [&cpu, &mem, &fps] {
        label.add_css_class("mono");
        label.add_css_class("small");
        label.set_xalign(0.0);
    }

    let telemetry_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    telemetry_box.set_margin_top(6);
    telemetry_box.set_margin_bottom(6);
    telemetry_box.set_margin_start(8);
    telemetry_box.set_margin_end(8);
    telemetry_box.append(&ui::section_title("播放状态与性能"));
    telemetry_box.append(&status_left);
    telemetry_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    telemetry_box.append(&cpu);
    telemetry_box.append(&mem);
    let telemetry_pop = gtk::Popover::new();
    telemetry_pop.set_child(Some(&telemetry_box));
    let telemetry_btn = gtk::MenuButton::new();
    telemetry_btn.set_child(Some(&fps));
    telemetry_btn.set_popover(Some(&telemetry_pop));
    telemetry_btn.add_css_class("toolbar-pill");
    telemetry_btn.add_css_class("telemetry-pill");
    telemetry_btn.set_tooltip_text(Some("查看当前壁纸状态与 CPU/内存占用"));
    content_header.pack_start(&telemetry_btn);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("搜索媒体…（Ctrl+F）"));
    search.set_width_chars(16);
    search.set_max_width_chars(24);
    search.set_hexpand(false);
    search.set_tooltip_text(Some("按名称/路径/类型过滤（Esc 清空）"));

    let details_button = gtk::ToggleButton::new();
    details_button.set_icon_name("sidebar-show-right-symbolic");
    details_button.set_tooltip_text(Some("显示/隐藏属性面板"));
    details_button.set_active(prefs.show_details);
    content_header.pack_end(&details_button);

    let grid_button = gtk::ToggleButton::new();
    grid_button.set_icon_name("view-grid-symbolic");
    grid_button.set_tooltip_text(Some("网格视图"));
    let list_button = gtk::ToggleButton::new();
    list_button.set_icon_name("view-list-symbolic");
    list_button.set_tooltip_text(Some("列表视图"));
    list_button.set_group(Some(&grid_button));
    if prefs.view == "list" {
        list_button.set_active(true);
    } else {
        grid_button.set_active(true);
    }
    let view_switch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    view_switch.add_css_class("linked");
    view_switch.append(&grid_button);
    view_switch.append(&list_button);
    content_header.pack_end(&view_switch);

    // —— 侧栏（主导航置顶 + 设置/关于沉底）——
    let sidebar_top = gtk::ListBox::new();
    sidebar_top.set_selection_mode(gtk::SelectionMode::Single);
    sidebar_top.add_css_class("navigation-sidebar");
    for (page, title, icon) in [
        ("library", "媒体库", "view-grid-symbolic"),
        ("playlists", "播放列表", "view-list-symbolic"),
        ("displays", "显示器", "video-display-symbolic"),
    ] {
        sidebar_top.append(&make_sidebar_row(page, title, icon));
    }

    let sidebar_bottom = gtk::ListBox::new();
    sidebar_bottom.set_selection_mode(gtk::SelectionMode::Single);
    sidebar_bottom.add_css_class("navigation-sidebar");
    sidebar_bottom.set_margin_bottom(6);
    for (page, title, icon) in [
        ("settings", "设置", "emblem-system-symbolic"),
        ("about", "关于", "help-about-symbolic"),
    ] {
        sidebar_bottom.append(&make_sidebar_row(page, title, icon));
    }

    let sidebar_fill = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_fill.set_vexpand(true);
    let sidebar_divider = gtk::Separator::new(gtk::Orientation::Horizontal);
    sidebar_divider.set_margin_start(12);
    sidebar_divider.set_margin_end(12);
    sidebar_divider.set_margin_bottom(4);

    let sidebar_header = adw::HeaderBar::new();
    let brand = gtk::Label::new(Some("流光"));
    brand.add_css_class("brand");
    sidebar_header.set_title_widget(Some(&brand));
    let sidebar_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_spacer.add_css_class("sidebar-pane");
    sidebar_spacer.append(&sidebar_header);
    sidebar_spacer.append(&sidebar_top);
    sidebar_spacer.append(&sidebar_fill);
    sidebar_spacer.append(&sidebar_divider);
    sidebar_spacer.append(&sidebar_bottom);

    // —— 常驻错误区（底部按需弹出，平时高度为 0）——
    // toast 只活 5s 且单行，长错误（解码器日志、ffmpeg 参数）必须有个地方完整留着。
    let notice_label = gtk::Label::new(None);
    notice_label.add_css_class("small");
    notice_label.add_css_class("alert-text");
    notice_label.set_xalign(0.0);
    notice_label.set_wrap(true);
    notice_label.set_selectable(true);
    notice_label.set_hexpand(true);
    let notice_close = gtk::Button::from_icon_name("window-close-symbolic");
    notice_close.add_css_class("flat");
    notice_close.set_tooltip_text(Some("关闭错误提示"));
    let notice_action = ui::action_button("一键处理", true);
    notice_action.set_valign(gtk::Align::Center);
    notice_action.set_visible(false);
    let notice_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    notice_row.add_css_class("notice-row");
    notice_row.append(&notice_close);
    notice_row.append(&notice_label);
    notice_row.append(&notice_action);
    let notice_bar = gtk::Revealer::new();
    notice_bar.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    notice_bar.set_child(Some(&notice_row));

    // —— 内容 stack ——
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    stack.set_vexpand(true);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&content_header);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&notice_bar);

    let content_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_page.append(&toolbar);

    let split = adw::NavigationSplitView::new();
    split.set_sidebar_width_fraction(0.16);
    split.set_min_sidebar_width(184.0);
    split.set_max_sidebar_width(220.0);
    let sidebar_page = adw::NavigationPage::new(&sidebar_spacer, "流光");
    let content_nav = adw::NavigationPage::new(&content_page, "内容");
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&content_nav));

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&split));
    window.set_content(Some(&toast));

    // —— 工作线程 ——
    let (tx, urgent, updates) = ipc::spawn(socket.clone(), config_path.clone());
    let (thumb_tx, thumbs_rx) = thumbs::spawn();

    let app = Rc::new(App {
        tx,
        urgent,
        thumb_tx,
        state: RefCell::new(state),
        window: window.clone(),
        toast: toast.clone(),
        search: search.clone(),
        library_only: std::cell::RefCell::new(Vec::new()),
        stack: stack.clone(),
        page_title: page_title.clone(),
        cards: RefCell::new(HashMap::new()),
        textures: RefCell::new(HashMap::new()),
        texture_order: RefCell::new(std::collections::VecDeque::new()),
        pending: RefCell::new(HashSet::new()),
        history: RefCell::new(Vec::new()),
        thumb_max: std::cell::Cell::new((400, 250)),
        library: RefCell::new(None),
        details: RefCell::new(None),
        playlists: RefCell::new(None),
        displays: RefCell::new(None),
        settings: RefCell::new(None),
        about_decoder: RefCell::new(None),
        config_path: config_path.clone(),
        persona,
        socket: socket.clone(),
        cpu: cpu.clone(),
        mem: mem.clone(),
        fps: fps.clone(),
        status_left: status_left.clone(),
        thumb_accents: RefCell::new(HashMap::new()),
        compat: RefCell::new(HashMap::new()),
        compat_pending: RefCell::new(HashSet::new()),
        prepare: RefCell::new(HashMap::new()),
        auto_apply: RefCell::new(None),
        notify_seen: RefCell::new(HashMap::new()),
        notice_bar: notice_bar.clone(),
        notice_label: notice_label.clone(),
        notice_action: notice_action.clone(),
        notice_action_kind: Cell::new(NoticeAction::None),
        sidebar_top: sidebar_top.clone(),
        sidebar_bottom: sidebar_bottom.clone(),
        compat_probed: RefCell::new(HashSet::new()),
        probe_notice_shown: Cell::new(false),
        last_status_key: RefCell::new(String::new()),
        deferred_retry: RefCell::new(None),
        demo_prepare: args.demo_prepare.clone(),
        demo_prepare_done: Cell::new(false),
        demo_apply: args.demo_apply.clone(),
        demo_apply_done: Cell::new(false),
        hover_keys: RefCell::new(HashSet::new()),
        apply_watch: RefCell::new(None),
        resume_watch: Cell::new(None),
        last_click_apply: RefCell::new(None),
        pause_button: pause_button.clone(),
    });

    {
        let weak = Rc::downgrade(&app);
        notice_close.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.notice_bar.set_reveal_child(false);
                app.notice_action_kind.set(NoticeAction::None);
                app.notice_action.set_visible(false);
            }
        });
    }
    {
        let weak = Rc::downgrade(&app);
        notice_action.connect_clicked(move |btn| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            match app.notice_action_kind.get() {
                NoticeAction::None => {}
                NoticeAction::OpenSettings => {
                    app.notice_bar.set_reveal_child(false);
                    app.notice_action_kind.set(NoticeAction::None);
                    app.notice_action.set_visible(false);
                    select_page(&app, &[&app.sidebar_top, &app.sidebar_bottom], "settings");
                }
                NoticeAction::EnableUserService => {
                    btn.set_sensitive(false);
                    btn.set_label("正在启动…");
                    let config_path = app.config_path.clone();
                    let (tx, rx) = async_channel::bounded(1);
                    std::thread::spawn(move || {
                        let res = service::run_action(
                            service::ServiceAction::EnableAndStart,
                            &config_path,
                        );
                        let _ = tx.send_blocking(res);
                    });
                    let app_weak = Rc::downgrade(&app);
                    glib::spawn_future_local(async move {
                        let Ok(result) = rx.recv().await else { return };
                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };
                        match result {
                            Ok((msg, _status)) => {
                                app.notice_bar.set_reveal_child(false);
                                app.notice_action_kind.set(NoticeAction::None);
                                app.notice_action.set_visible(false);
                                app.toast(&msg);
                                if let Some(view) = app.settings.borrow().as_ref() {
                                    settings::refresh_service(&app, view);
                                }
                                for cmd in [
                                    Cmd::Status,
                                    Cmd::Media,
                                    Cmd::Sources,
                                    Cmd::Playlists,
                                    Cmd::Config,
                                ] {
                                    app.send(cmd);
                                }
                            }
                            Err(error) => {
                                app.notify_error("service", &error);
                            }
                        }
                    });
                }
            }
        });
    }

    // —— 页面 ——
    let library_view = library::build(&app);
    let details_view = details::build(&app);
    let playlists_view = playlists::build(&app);
    let displays_view = displays::build(&app);
    let settings_view = settings::build(&app);

    library_view.root.set_hexpand(true);
    details_view.root.set_hexpand(false);
    let library_page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    library_page.append(&library_view.root);
    library_page.append(&details_view.root);

    stack.add_named(&library_page, Some("library"));
    stack.add_named(&playlists_view.root, Some("playlists"));
    stack.add_named(&displays_view.root, Some("displays"));
    stack.add_named(&settings_view.root, Some("settings"));
    stack.add_named(&about_page(&app), Some("about"));
    stack.set_visible_child_name("library");

    {
        let mut only = app.library_only.borrow_mut();
        only.push(view_switch.clone().upcast());
        only.push(details_button.clone().upcast());
    }
    *app.library.borrow_mut() = Some(library_view.clone());
    *app.details.borrow_mut() = Some(details_view.clone());
    *app.playlists.borrow_mut() = Some(playlists_view.clone());
    *app.displays.borrow_mut() = Some(displays_view.clone());
    *app.settings.borrow_mut() = Some(settings_view.clone());

    details::set_visible(&app, &details_view, prefs.show_details);
    library::set_view(&app, &library_view, prefs.view == "list");
    library_view.size.set_value(f64::from(prefs.card_size));

    // —— 交互接线 ——
    {
        let app = Rc::downgrade(&app);
        refresh.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.send(Cmd::Rescan);
                app.send(Cmd::Sources);
                app.toast("正在重新扫描媒体目录…");
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let view = Rc::downgrade(&library_view);
        grid_button.connect_toggled(move |button| {
            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                return;
            };
            if button.is_active() {
                library::set_view(&app, &view, false);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let view = Rc::downgrade(&library_view);
        list_button.connect_toggled(move |button| {
            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                return;
            };
            if button.is_active() {
                library::set_view(&app, &view, true);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let view = Rc::downgrade(&details_view);
        details_button.connect_toggled(move |button| {
            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                return;
            };
            details::set_visible(&app, &view, button.is_active());
        });
    }
    {
        let app = Rc::downgrade(&app);
        let view = Rc::downgrade(&library_view);
        let slot = Rc::new(Cell::new(None::<glib::SourceId>));
        search.connect_search_changed(move |_| {
            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                return;
            };
            let app2 = app.clone();
            let view2 = view.clone();
            ui::debounce(&slot, 180, move || library::rebuild(&app2, &view2));
        });
    }
    {
        let app = Rc::downgrade(&app);
        pause_button.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                let paused = app
                    .state
                    .borrow()
                    .status
                    .as_ref()
                    .is_some_and(|status| status.paused);
                app.send(if paused { Cmd::Resume } else { Cmd::Pause });
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        next_button.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.send(Cmd::Next { output: None });
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let other = sidebar_bottom.clone();
        sidebar_top.connect_row_selected(move |_, row| {
            if let (Some(app), Some(row)) = (app.upgrade(), row) {
                other.unselect_all();
                let page = row.widget_name().to_string();
                app.set_page(&page);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let other = sidebar_top.clone();
        sidebar_bottom.connect_row_selected(move |_, row| {
            if let (Some(app), Some(row)) = (app.upgrade(), row) {
                other.unselect_all();
                let page = row.widget_name().to_string();
                app.set_page(&page);
            }
        });
    }

    // 快捷键。
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let app = Rc::downgrade(&app);
        let sidebar_top_keys = sidebar_top.clone();
        let sidebar_bottom_keys = sidebar_bottom.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(app) = app.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::f if ctrl => {
                    if app.stack.visible_child_name().as_deref() != Some("library") {
                        select_page(&app, &[&sidebar_top_keys, &sidebar_bottom_keys], "library");
                    }
                    app.search.grab_focus();
                    app.search.select_region(0, -1);
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    if app.search.text().is_empty() {
                        glib::Propagation::Proceed
                    } else {
                        app.search.set_text("");
                        glib::Propagation::Stop
                    }
                }
                gdk::Key::space => {
                    if focus_is_editable(&app.window) {
                        return glib::Propagation::Proceed;
                    }
                    let paused = app
                        .state
                        .borrow()
                        .status
                        .as_ref()
                        .is_some_and(|status| status.paused);
                    app.send(if paused { Cmd::Resume } else { Cmd::Pause });
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    if focus_is_editable(&app.window) {
                        return glib::Propagation::Proceed;
                    }
                    app.send(Cmd::Next { output: None });
                    glib::Propagation::Stop
                }
                gdk::Key::Left => {
                    if focus_is_editable(&app.window) {
                        return glib::Propagation::Proceed;
                    }
                    let previous = app.history.borrow_mut().pop();
                    match previous {
                        Some(path) => {
                            app.apply_media_to_output(&path, None);
                            app.toast("已回到上一张壁纸");
                        }
                        None => app.toast("没有更早的壁纸可回退"),
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::F1 => {
                    shortcuts::show(&app.window);
                    glib::Propagation::Stop
                }
                gdk::Key::F5 => {
                    app.send(Cmd::Rescan);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    window.add_controller(keys);

    // —— 1s 心跳 ——
    {
        let app = Rc::downgrade(&app);
        let tick = Rc::new(Cell::new(0u32));
        glib::timeout_add_seconds_local(1, move || {
            let Some(app) = app.upgrade() else {
                return glib::ControlFlow::Break;
            };
            app.send(Cmd::Status);
            app.send(Cmd::Metrics);
            let now = tick.get() + 1;
            tick.set(now);
            if now % 5 == 0 {
                app.send(Cmd::Outputs);
            }
            if now % 15 == 0 {
                app.send(Cmd::Sources);
                app.send(Cmd::Playlists);
                app.send(Cmd::Config);
            }
            if app.state.borrow().scanning {
                app.send(Cmd::Media);
            }
            glib::ControlFlow::Continue
        });
    }

    // —— 500ms：转码进度轮询（进度条要"原地"走，不能等 1s 心跳）——
    {
        let app = Rc::downgrade(&app);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(app) = app.upgrade() else {
                return glib::ControlFlow::Break;
            };
            poll_prepare(&app);
            glib::ControlFlow::Continue
        });
    }

    // —— IPC 更新消费 ——
    {
        let app = Rc::downgrade(&app);
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                let Some(app) = app.upgrade() else { break };
                handle_update(&app, update);
            }
        });
    }
    // —— 缩略图解码结果 ——
    {
        let app = Rc::downgrade(&app);
        glib::spawn_future_local(async move {
            while let Ok(thumb) = thumbs_rx.recv().await {
                let Some(app) = app.upgrade() else { break };
                let Some((id, at)) = thumb.key.rsplit_once('@') else {
                    continue;
                };
                let Ok(at_ms) = at.parse::<u64>() else {
                    continue;
                };
                // 解码期间用户可能已经滚走/移开：不采用，也不放进缓存。
                if !app.thumb_still_wanted(id, at_ms) {
                    app.pending.borrow_mut().remove(&(id.to_owned(), at_ms));
                    continue;
                }
                let Some(texture) = ui::texture_from_rgba(thumb.width, thumb.height, &thumb.rgba)
                else {
                    app.pending.borrow_mut().remove(&(id.to_owned(), at_ms));
                    continue;
                };
                app.store_texture((id.to_owned(), at_ms), texture);
                app.pending.borrow_mut().remove(&(id.to_owned(), at_ms));
                if at_ms == 0 {
                    app.thumb_accents
                        .borrow_mut()
                        .insert(id.to_owned(), thumb.accent);
                }
                library::on_thumb(&app, id, at_ms);
                if at_ms == 0 {
                    if app.state.borrow().selected.as_deref() == Some(id) {
                        if let Some(view) = app.details.borrow().as_ref() {
                            details::refresh_preview(&app, view, id);
                        }
                    }
                    if app.state.borrow().current_media().as_deref() == Some(id) {
                        sync_liuguang_theme(&app, id);
                    }
                    refresh_displays(&app);
                }
                // 渐进补：首轮请求是按大小排序的，最大的几张可能落在预算之外；
                // 每来一张就检查一次可见区（`request_thumb` 自己会跳过已有/在途）。
                if at_ms == 0 && app.pending.borrow().len() < 2 {
                    library::refresh_visible_soon(&app);
                }
            }
        });
    }

    // 首屏请求。
    for cmd in [
        Cmd::Status,
        Cmd::Media,
        Cmd::Metrics,
        Cmd::Outputs,
        Cmd::Sources,
        Cmd::Playlists,
        Cmd::Config,
    ] {
        app.send(cmd);
    }

    // 缩略图按"卡片逻辑宽度 × 显示缩放"请求：HiDPI（scale=1.5）下 400×250 的
    // 图会被拉大到 ~483 物理像素，肉眼可见发糊。
    refresh_thumb_size(&app, &window);
    window.connect_scale_factor_notify({
        let app = app.clone();
        move |window| refresh_thumb_size(&app, window)
    });
    {
        let app = app.clone();
        window.connect_close_request(move |window| {
            remember_window(&app, window);
            glib::Propagation::Proceed
        });
    }
    window.present();
    if std::env::var_os("LIUGUANG_GUI_DEBUG").is_some() {
        eprintln!(
            "[window] 恢复 {}x{} maximized={} 实际={}x{}",
            prefs.window_width,
            prefs.window_height,
            prefs.window_maximized,
            window.width(),
            window.height()
        );
    }
    APP_KEEPALIVE.with(|slot| *slot.borrow_mut() = Some(app.clone()));
    // 退出应用（Ctrl+Q / 关闭最后一个窗口）时也落盘一次几何信息。
    {
        let app = app.clone();
        let window = window.clone();
        gtk_app.connect_shutdown(move |_| remember_window(&app, &window));
    }
    let page = args
        .open_page
        .as_deref()
        .unwrap_or(prefs.show_page.as_str());
    select_page(&app, &[&sidebar_top, &sidebar_bottom], page);
    if args.show_shortcuts {
        crate::shortcuts::show(&window);
    }
}

/// 退出时把窗口尺寸/最大化状态写回偏好。
fn remember_window(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let (width, height) = (window.width(), window.height());
    {
        let mut state = app.state.borrow_mut();
        state.prefs.window_maximized = window.is_maximized();
        if !window.is_maximized() && width > 200 && height > 200 {
            state.prefs.window_width = width;
            state.prefs.window_height = height;
        }
    }
    app.save_prefs();
}

fn about_page(app: &Rc<App>) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 14);
    column.set_halign(gtk::Align::Center);
    column.set_valign(gtk::Align::Start);
    column.set_margin_top(28);
    column.set_margin_bottom(28);
    column.set_width_request(620);

    let icon = gtk::Image::from_icon_name(APP_ICON);
    icon.set_pixel_size(88);
    column.append(&icon);

    let title = gtk::Label::new(Some("流光"));
    title.add_css_class("title-1");
    column.append(&title);

    let subtitle = gtk::Label::new(Some("动态壁纸：Wayland 背景层 + 解码/转换都在守护进程里"));
    subtitle.add_css_class("dim");
    subtitle.add_css_class("small");
    column.append(&subtitle);

    let rows = gtk::ListBox::new();
    rows.set_selection_mode(gtk::SelectionMode::None);
    rows.add_css_class("boxed-list");
    row(&rows, "界面版本", env!("CARGO_PKG_VERSION"));
    row(
        &rows,
        "构建",
        if cfg!(debug_assertions) {
            "debug（带调试信息，较慢）"
        } else {
            "release"
        },
    );
    row(
        &rows,
        "仓库",
        option_env!("CARGO_PKG_REPOSITORY").unwrap_or("https://github.com/suluoMISS/liuguang"),
    );
    row(
        &rows,
        "运行时",
        &format!(
            "GTK {}.{}.{} · libadwaita {}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version(),
            adw::major_version(),
            adw::minor_version()
        ),
    );
    let initial_decoder = app
        .state
        .borrow()
        .status
        .as_ref()
        .and_then(|s| s.channels.first())
        .map_or_else(|| "—".to_owned(), |c| c.decoder.clone());
    let decoder_label = row(&rows, "解码器", &initial_decoder);
    *app.about_decoder.borrow_mut() = Some(decoder_label);
    row(&rows, "配置目录", &app.config_path.display().to_string());
    row(&rows, "控制套接字", &app.socket.display().to_string());
    let prefs_path = app.state.borrow().prefs_path.clone();
    row(&rows, "界面偏好", &prefs_path.display().to_string());
    column.append(&rows);

    let note = gtk::Label::new(Some(
        "界面与守护进程是两个进程：界面崩溃不影响正在播放的壁纸；\n关闭窗口只是关掉界面，壁纸照旧播放。",
    ));
    note.add_css_class("dim");
    note.add_css_class("small");
    note.set_justify(gtk::Justification::Center);
    note.set_wrap(true);
    column.append(&note);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Center);

    let shortcuts_button = gtk::Button::with_label("快捷键（F1）");
    {
        let window = app.window.clone();
        shortcuts_button.connect_clicked(move |_| crate::shortcuts::show(&window));
    }
    buttons.append(&shortcuts_button);

    let about = gtk::Button::with_label("关于窗口");
    about.add_css_class("pill");
    {
        let app = Rc::downgrade(app);
        about.connect_clicked(move |_| {
            let Some(app) = app.upgrade() else { return };
            let dialog = adw::AboutWindow::new();
            dialog.set_application_name("流光");
            dialog.set_application_icon(APP_ICON);
            dialog.set_version(env!("CARGO_PKG_VERSION"));
            dialog.set_developer_name("流光（Liuguang）");
            dialog.set_comments(
                "媒体库 / 播放列表 / 显示器 / 设置\n壁纸渲染在守护进程（liuguang.service）里",
            );
            dialog.set_website("https://github.com/suluoMISS/liuguang");
            dialog.set_issue_url("https://github.com/suluoMISS/liuguang/issues");
            dialog.add_link("项目主页", "https://github.com/suluoMISS/liuguang");
            dialog.set_transient_for(Some(&app.window));
            dialog.present();
        });
    }
    buttons.append(&about);
    column.append(&buttons);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&column)
        .build();
    scroller.upcast()
}

/// 「关于」页里的「键 → 值」一行（值可选中复制；返回右侧值标签以供动态刷新）。
fn row(list: &gtk::ListBox, key: &str, value: &str) -> gtk::Label {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_top(8);
    box_.set_margin_bottom(8);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    let key_label = gtk::Label::new(Some(key));
    key_label.set_xalign(0.0);
    key_label.set_width_chars(10);
    key_label.add_css_class("dim");
    let value_label = gtk::Label::new(Some(value));
    value_label.set_xalign(0.0);
    value_label.set_hexpand(true);
    value_label.set_selectable(true);
    value_label.set_wrap(true);
    value_label.add_css_class("mono");
    value_label.add_css_class("small");
    box_.append(&key_label);
    box_.append(&value_label);
    row.set_child(Some(&box_));
    list.append(&row);
    value_label
}

fn make_sidebar_row(page: &str, title: &str, icon: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(page);
    row.set_activatable(true);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    line.set_margin_top(8);
    line.set_margin_bottom(8);
    line.set_margin_start(10);
    line.set_margin_end(10);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    line.append(&image);
    line.append(&label);
    row.set_child(Some(&line));
    row
}

fn select_page(app: &Rc<App>, sidebars: &[&gtk::ListBox], page: &str) {
    let mut matched = false;
    for sidebar in sidebars {
        let mut index = 0;
        let mut hit = false;
        while let Some(row) = sidebar.row_at_index(index) {
            if row.widget_name() == page {
                sidebar.select_row(Some(&row));
                hit = true;
                matched = true;
                break;
            }
            index += 1;
        }
        if !hit {
            sidebar.unselect_all();
        }
    }
    if !matched {
        app.set_page(page);
    }
}

/// 单击应用的去重判据（纯函数，便于单测）：同一 id 在 500ms 内的重复 press
/// （双击的第二下）不再下发；换一张卡则立即允许。
fn click_should_apply(last: Option<(&str, Instant)>, id: &str, now: Instant) -> bool {
    last.is_none_or(|(last_id, at)| {
        last_id != id || now.duration_since(at) >= Duration::from_millis(500)
    })
}

fn focus_is_editable(window: &adw::ApplicationWindow) -> bool {
    let mut widget = gtk::prelude::GtkWindowExt::focus(window);
    while let Some(current) = widget {
        if current.is::<gtk::Editable>()
            || current.is::<gtk::TextView>()
            || current.is::<gtk::Scale>()
        {
            return true;
        }
        widget = current.parent();
    }
    false
}

fn handle_update(app: &Rc<App>, update: Upd) {
    match update {
        Upd::Status(status) => {
            if app.notice_bar.reveals_child()
                && app.notice_action_kind.get() == NoticeAction::EnableUserService
            {
                app.notice_bar.set_reveal_child(false);
                app.notice_action_kind.set(NoticeAction::None);
                app.notice_action.set_visible(false);
                if let Some(view) = app.settings.borrow().as_ref() {
                    settings::refresh_service(app, view);
                }
            }
            let was_paused = status.paused
                || status
                    .channels
                    .iter()
                    .any(|channel| channel.pause_reason.is_some());
            if let Some(label) = app.about_decoder.borrow().as_ref() {
                let decoder = status
                    .channels
                    .first()
                    .map_or("—", |channel| channel.decoder.as_str());
                label.set_text(decoder);
            }
            // 每次心跳都重刷库/属性/显示器会在空闲时持续触发 GTK 布局与重绘，
            // 实测让 GUI 空闲 CPU 升到 3–4%。只在状态指纹变化时刷新。
            let key = status_key(&status);
            let changed = {
                let mut last = app.last_status_key.borrow_mut();
                let changed = *last != key;
                *last = key;
                changed
            };
            app.state.borrow_mut().status = Some(*status);
            update_statusbar(app);
            if changed {
                library::refresh_selection(app);
                refresh_details(app);
                refresh_displays(app);
            }
            // 应用后的状态反馈：daemon t39 会在 user 暂停时自动恢复播放。
            if let Some((expect_resume, at)) = app.resume_watch.get() {
                if !was_paused {
                    if expect_resume && at.elapsed() < Duration::from_secs(10) {
                        app.toast("已恢复播放");
                    }
                    app.resume_watch.set(None);
                } else if at.elapsed() > Duration::from_secs(10) {
                    app.resume_watch.set(None);
                }
            }
            if let Some((path, at)) = app.apply_watch.borrow().as_ref() {
                if at.elapsed() > Duration::from_secs(10) {
                    let _ = path;
                    app.apply_watch.borrow_mut().take();
                }
            }
            // 转码完成后的重试等到这里才做：先看新状态里 daemon 是不是已经自动播了缓存。
            if let Some((path, cache)) = app.deferred_retry.borrow_mut().take() {
                if already_playing(app, &path, cache.as_deref()) {
                    app.auto_apply.borrow_mut().take();
                    app.toast(&format!(
                        "已转换为 {}，daemon 已自动播放缓存副本",
                        converted_target(app, &path)
                    ));
                } else {
                    maybe_retry_after_prepare(app, &path);
                }
            }
        }
        Upd::Media {
            items,
            total,
            scanning,
        } => {
            {
                let mut state = app.state.borrow_mut();
                state.rows = items;
                state.total = total;
                state.scanning = scanning;
                state.last_error = None;
                if app.notice_bar.reveals_child()
                    && app.notice_action_kind.get() == NoticeAction::OpenSettings
                {
                    app.notice_bar.set_reveal_child(false);
                    app.notice_action_kind.set(NoticeAction::None);
                    app.notice_action.set_visible(false);
                }
                let root = state.library_root();
                if let Some(root) = root {
                    state.library_hint = format!("把图片或视频放进 {root}，然后点右上角的“刷新”。");
                }
                // 默认选中正在使用的壁纸（没有就选第一项），
                // 这样右侧属性面板一打开就有内容，而不用先点一下卡片。
                let selected_still_present = state
                    .selected
                    .as_ref()
                    .is_some_and(|id| state.rows.iter().any(|item| &item.id == id));
                if !selected_still_present {
                    let playing = state.current_media();
                    let playing =
                        playing.filter(|path| state.rows.iter().any(|item| &item.id == path));
                    state.selected =
                        playing.or_else(|| state.rows.first().map(|item| item.id.clone()));
                }
            }
            if let Some(view) = app.library.borrow().as_ref() {
                library::rebuild(app, view);
            }
            library::refresh_selection(app);
            refresh_details(app);
            app.sync_compat();
            app.probe_prepared();
            maybe_demo_apply(app);
            maybe_demo_prepare(app);
        }
        Upd::Metrics(metrics) => {
            app.state.borrow_mut().metrics = Some(*metrics);
            update_statusbar(app);
            refresh_displays(app);
        }
        Upd::Sources(sources) => {
            let changed = {
                let mut state = app.state.borrow_mut();
                let changed = state.sources.as_ref().is_none_or(|prev| {
                    prev.root != sources.root
                        || prev.sources.len() != sources.sources.len()
                        || prev.sources.iter().zip(&sources.sources).any(|(a, b)| {
                            a.id != b.id
                                || a.name != b.name
                                || a.kind != b.kind
                                || a.available != b.available
                        })
                });
                let root = sources.root.clone();
                state.sources = Some(*sources);
                if !root.is_empty() {
                    state.library_hint = format!("把图片或视频放进 {root}，然后点右上角的“刷新”。");
                }
                changed
            };
            if changed {
                if let Some(view) = app.settings.borrow().as_ref() {
                    settings::refresh(app, view);
                }
                if let Some(view) = app.library.borrow().as_ref() {
                    library::rebuild(app, view);
                }
            }
        }
        Upd::Playlists(playlists) => {
            app.state.borrow_mut().playlists = playlists;
            if let Some(view) = app.playlists.borrow().as_ref() {
                playlists::refresh(app, view);
            }
        }
        Upd::Config(config) => {
            app.state.borrow_mut().config = Some((*config).clone());
            if let Some(view) = app.settings.borrow().as_ref() {
                settings::refresh(app, view);
            }
            if let Some(view) = app.details.borrow().as_ref() {
                details::apply_config(app, view, &config);
            }
        }
        Upd::Outputs(outputs) => {
            app.state.borrow_mut().outputs = outputs;
            if let Some(view) = app.details.borrow().as_ref() {
                details::sync_outputs(app, view);
                details::refresh(app, view);
            }
            refresh_displays(app);
            if let Some(view) = app.playlists.borrow().as_ref() {
                playlists::sync_outputs(app, view);
            }
        }
        Upd::Applied(path) => {
            let name = app
                .state
                .borrow()
                .rows
                .iter()
                .find(|item| item.path == path)
                .map_or_else(|| short(&path), |item| item.name.clone());
            app.toast_hint(&format!("已切换：{name}"), &path);
            app.status_left.set_text(&format!("已切换：{name}"));
            app.apply_watch.borrow_mut().take();
            app.send(Cmd::Status);
        }
        Upd::TransformApplied => {}
        Upd::Paused(paused) => {
            app.toast(if paused { "已暂停" } else { "已继续" });
            app.send(Cmd::Status);
        }
        Upd::Pinned(pinned) => {
            app.toast(if pinned {
                "只放这一项：开"
            } else {
                "只放这一项：关"
            });
            app.send(Cmd::Status);
        }
        Upd::Transition(message) => app.toast(&message),
        Upd::PlaylistSaved(playlist) => {
            app.toast(&format!("播放列表“{}”已保存", playlist.name));
            if let Some(view) = app.playlists.borrow().as_ref() {
                *view.current.borrow_mut() = Some(playlist.id.clone());
                view.dirty.set(false);
            }
            app.send(Cmd::SaveConfig);
            app.send(Cmd::Playlists);
        }
        Upd::PlaylistDeleted(id) => {
            app.toast("播放列表已删除（媒体文件保留）");
            if let Some(view) = app.playlists.borrow().as_ref() {
                if view.current.borrow().as_deref() == Some(id.as_str()) {
                    view.current.borrow_mut().take();
                    view.dirty.set(false);
                    view.editor.set_visible_child_name("empty");
                }
            }
            app.send(Cmd::SaveConfig);
            app.send(Cmd::Playlists);
        }
        Upd::MediaRemoved {
            path,
            removed,
            removed_count,
        } => {
            if removed {
                app.toast(&format!(
                    "已从库中移除 {}（清理 {removed_count} 条索引）",
                    short(&path)
                ));
            } else {
                app.toast("这项已经不在库里");
            }
            let removed_id = path.clone();
            app.state.borrow_mut().selected = None;
            app.textures
                .borrow_mut()
                .retain(|(id, _), _| id != &removed_id);
            app.send(Cmd::Media);
            app.send(Cmd::Sources);
        }
        Upd::ScanStarted {
            scan_id,
            superseded,
        } => {
            app.toast(if superseded {
                "已开始重新扫描（替换了进行中的扫描）"
            } else {
                "已开始重新扫描"
            });
            let _ = scan_id;
            app.state.borrow_mut().scanning = true;
            app.send(Cmd::Media);
        }
        Upd::Notice(message) => app.toast(&message),
        Upd::Reloaded => {
            app.send(Cmd::Status);
            app.send(Cmd::Media);
            app.send(Cmd::Config);
            app.send(Cmd::Sources);
            app.send(Cmd::Playlists);
        }
        Upd::Error(message) => {
            app.state.borrow_mut().last_error = Some(message.clone());
            let code = error_code(&message);
            app.notify_error(code, &message);
            update_statusbar(app);
        }
        Upd::Failure {
            code,
            detail_code,
            message,
        } => {
            // 结构化错误：`code` 是协议契约（如 UNSUPPORTED_CODEC / CONVERSION_UNAVAILABLE），
            // toast 去重直接用它；`detail_code` 目前只在这里收着，供后续按原因给建议。
            app.state.borrow_mut().last_error = Some(message.clone());
            let _ = detail_code;
            app.notify_error(&code, &message);
            update_statusbar(app);
        }
        Upd::Compat { id, compat } => {
            app.compat_pending.borrow_mut().remove(&id);
            app.compat.borrow_mut().insert(id.clone(), compat);
            app.probe_prepared();
            library::refresh_compat(app);
            if app.state.borrow().selected.as_deref() == Some(id.as_str()) {
                refresh_details(app);
            }
        }
        Upd::Prepared { path, result } => {
            let state = result.state;
            let message = result.message.clone().unwrap_or_default();
            let cache_path = result.cache_path.clone();
            app.prepare.borrow_mut().insert(path.clone(), result);
            library::refresh_compat(app);
            if app.state.borrow().selected.as_deref() == Some(path.as_str()) {
                refresh_details(app);
            }
            match state {
                PrepareState::Ready => {
                    // 角标/详情跟着变（缓存就绪后 daemon 会透明地播缓存副本）。
                    app.request_compat(&path);
                    if already_playing(app, &path, cache_path.as_deref()) {
                        // t18 F5：daemon 已经自动播了缓存副本，再发 set_wallpaper 会重复建通道。
                        app.auto_apply.borrow_mut().take();
                        app.toast(&format!(
                            "已转换为 {}，daemon 已自动播放缓存副本",
                            converted_target(app, &path)
                        ));
                    } else {
                        if app.state.borrow().selected.as_deref() == Some(path.as_str()) {
                            app.toast(&format!(
                                "已转换为 {}，正在使用缓存副本",
                                converted_target(app, &path)
                            ));
                        }
                        // status 是 1s 刷新一次的快照，可能还看不到 daemon 的自动播放；
                        // 延迟一次再问，避免重复下发 set_wallpaper。
                        schedule_retry_after_prepare(app, &path, cache_path.clone());
                    }
                }
                PrepareState::Failed => {
                    app.toast(&format!("准备失败：{message}"));
                    let waiting = app
                        .auto_apply
                        .borrow()
                        .as_ref()
                        .is_some_and(|pending| pending.request.path == path);
                    if waiting {
                        app.auto_apply.borrow_mut().take();
                    }
                }
                PrepareState::Queued | PrepareState::Running => {}
            }
        }
        Upd::NeedsPreparation { request, message } => {
            let path = request.path.clone();
            let already_tried = app
                .auto_apply
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending.request.path == path && pending.tried);
            if already_tried {
                // 只自动重试一次：重试后又被要求转码 ⇒ 明确告诉用户，不再循环。
                app.auto_apply.borrow_mut().take();
                app.toast(&format!("自动重试仍要求转码：{message}"));
                refresh_details(app);
                return;
            }
            app.toast(&format!("这个素材需要先转码，已自动开始准备：{message}"));
            *app.auto_apply.borrow_mut() = Some(PendingApply {
                request: *request,
                tried: false,
            });
            app.prepare
                .borrow_mut()
                .entry(path.clone())
                .or_insert(PrepareMediaResult {
                    state: PrepareState::Queued,
                    cache_path: None,
                    progress_permille: 0,
                    message: None,
                });
            app.send(Cmd::Prepare {
                path: path.clone(),
                force: false,
            });
            library::refresh_compat(app);
            refresh_details(app);
        }
        Upd::Event(event) => match event.kind {
            EventKind::MediaPrepared => {
                if let Some(media) = event.media.clone() {
                    // 事件只说明"完成了"；权威状态用幂等查询补齐（含进度/缓存路径）。
                    app.send(Cmd::Prepare {
                        path: media.clone(),
                        force: false,
                    });
                    app.request_compat(&media);
                    if already_playing(app, &media, event.cache_path.as_deref()) {
                        // daemon 已经自动应用了缓存副本：只更新界面，别再发一次 set_wallpaper。
                        app.auto_apply.borrow_mut().take();
                        app.toast(&format!(
                            "已转换为 {}，daemon 已自动播放缓存副本",
                            converted_target(app, &media)
                        ));
                    } else {
                        if app.state.borrow().selected.as_deref() == Some(media.as_str()) {
                            app.toast(&format!(
                                "已转换为 {}，正在使用缓存副本",
                                converted_target(app, &media)
                            ));
                        }
                        schedule_retry_after_prepare(app, &media, event.cache_path.clone());
                    }
                }
            }
            EventKind::Error => {
                if let Some(message) = event.message.clone() {
                    // 预计中的“等转码”不是错误：daemon 对 NEEDS_PREPARATION 也会发一条
                    // Error 事件（文案带“完成后会自动播放”），而 GUI 已经自动接管了
                    // 准备流程；再弹常驻红条会把正常等待渲染成故障（t27 F3 复核）。
                    if !is_expected_prepare_wait(&message) {
                        let code = event.code.clone().unwrap_or_else(|| "EVENT".to_owned());
                        app.notify_error(&code, &message);
                    }
                }
            }
            _ => {}
        },
        Upd::Thumbnail { id, at_ms, png_b64 } => {
            app.pending.borrow_mut().remove(&(id.clone(), at_ms));
            if png_b64.is_empty() {
                return;
            }
            // 已经滚走/移开：不浪费 CPU 解码，也不占纹理缓存。
            if !app.thumb_still_wanted(&id, at_ms) {
                return;
            }
            // base64 + PNG 解码都在缩略图线程完成（t40）：主线程只收 RGBA 贴纹理。
            let _ = app.thumb_tx.send(thumbs::ThumbCmd::LoadPng {
                key: thumbs::key(&id, at_ms),
                png_b64,
            });
        }
    }
}

/// 转码完成后自动重试一次被拦下的应用；失败给明确提示，不再自动循环。
/// `--demo-apply`：媒体首次加载后选中目标并点一下「应用」（验证转码后重试链路）。
fn maybe_demo_apply(app: &Rc<App>) {
    if app.demo_apply_done.get() {
        return;
    }
    let Some(target) = app.demo_apply.clone() else {
        return;
    };
    let id = app
        .state
        .borrow()
        .rows
        .iter()
        .find(|item| item.name == target || item.path == target || item.path.ends_with(&target))
        .map(|item| item.id.clone());
    let Some(id) = id else {
        return;
    };
    app.demo_apply_done.set(true);
    app.select(&id);
    app.apply_media(&id, false);
}

/// `--demo-prepare`：媒体首次加载后选中目标并开始准备（截图脚本用，只触发一次）。
fn maybe_demo_prepare(app: &Rc<App>) {
    if app.demo_prepare_done.get() {
        return;
    }
    let Some(target) = app.demo_prepare.clone() else {
        return;
    };
    let id = app
        .state
        .borrow()
        .rows
        .iter()
        .find(|item| item.name == target || item.path == target || item.path.ends_with(&target))
        .map(|item| item.id.clone());
    let Some(id) = id else {
        return;
    };
    app.demo_prepare_done.set(true);
    app.select(&id);
    app.prepare_media(&id);
}

/// 转码目标的界面文案（例如「 8bit HEVC 」），取不到时给通用说法。
///
/// daemon 给的 `target` 是协议值（`hevc 8bit`）；界面显示成「8bit HEVC」，
/// 并明确这是**缓存副本**，不是原文件被原生支持。
/// 同一个错误码 5s 内只弹一次 toast（GUI 侧去重，daemon 仍按自己的节奏发布事件）。
fn should_notify(
    seen: &mut std::collections::HashMap<String, Instant>,
    code: &str,
    now: Instant,
) -> bool {
    seen.retain(|_, at| now.duration_since(*at) < Duration::from_secs(5));
    if seen.contains_key(code) {
        return false;
    }
    seen.insert(code.to_owned(), now);
    true
}

/// 把一条错误消息归类成稳定的错误码（toast 去重用）。
///
/// `Upd::Error` 从协议层拿不到 `error.code`（IPC 封装只保留了 message），
/// 但同一类失败的前缀是稳定的；而 daemon 的 `EventKind::Error` 自带 code。
/// 两边归一到同一个签名后，同一次失败只提示一次。
/// daemon 在“素材需要转码”时回 `NEEDS_PREPARATION`，同时也会发一条 `Error` 事件；
/// 这条事件的文案带“完成后会自动播放”，属于正常等待而不是故障。
fn is_expected_prepare_wait(message: &str) -> bool {
    message.contains("完成后会自动播放")
}

fn error_code(message: &str) -> &'static str {
    const MAP: [(&str, &str); 7] = [
        ("壁纸应用失败", "apply"),
        ("视频打开失败", "video-open"),
        ("媒体读取或解码失败", "decode"),
        ("抽取缩略图", "thumbnail"),
        ("准备失败", "prepare"),
        ("转码", "transcode"),
        ("配置", "config"),
    ];
    MAP.iter()
        .find(|(needle, _)| message.contains(needle))
        .map_or("other", |(_, code)| code)
}

/// toast 上只放第一行（长错误进常驻区）；按字符截断，避免切断 UTF-8。
fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or_default();
    if line.chars().count() <= 96 {
        line.to_owned()
    } else {
        let mut text: String = line.chars().take(96).collect();
        text.push('…');
        text
    }
}

/// daemon 是否已经在该素材（或它的缓存副本）上出画面。
///
/// t18 F5：`MEDIA_PREPARED` 之后 daemon 会自动起播缓存；GUI 若再发一次 set_wallpaper，
/// 日志里会看到两条“视频通道已建立”。有这个判据就只更新界面，不重复下命令。
fn already_playing(app: &App, source: &str, cache: Option<&str>) -> bool {
    let state = app.state.borrow();
    let Some(status) = state.status.as_ref() else {
        return false;
    };
    status.channels.iter().any(|channel| {
        let media_matches = channel.media.as_deref() == Some(source)
            || cache.is_some_and(|cache| channel.media.as_deref() == Some(cache));
        media_matches
            && channel
                .video
                .as_ref()
                .is_some_and(|video| video.presented > 0)
    })
}

fn converted_target(app: &App, id: &str) -> String {
    app.compat_of(id)
        .and_then(|compat| compat.target)
        .map_or_else(
            || "转码后的格式".to_owned(),
            |target| ui::target_label(&target),
        )
}

/// 推迟一次重试：转码刚完成时 GUI 手里的 status 还是旧快照，
/// 立刻重发 `set_wallpaper` 会和 daemon 的"自动播缓存"各建一条通道（t18 F5）。
/// 等 700ms 再请求一次 Status，由 `Upd::Status` 判断是"已自动播"还是"该重试"。
fn schedule_retry_after_prepare(app: &Rc<App>, path: &str, cache: Option<String>) {
    if app.auto_apply.borrow().is_none() {
        return;
    }
    if app.deferred_retry.borrow().is_some() {
        return;
    }
    *app.deferred_retry.borrow_mut() = Some((path.to_owned(), cache));
    let weak = Rc::downgrade(app);
    glib::timeout_add_local_once(Duration::from_millis(700), move || {
        if let Some(app) = weak.upgrade() {
            app.send(Cmd::Status);
        }
    });
}

fn maybe_retry_after_prepare(app: &Rc<App>, path: &str) {
    let pending = app.auto_apply.borrow().clone();
    let Some(mut pending) = pending else {
        return;
    };
    if pending.request.path != path {
        return;
    }
    if pending.tried {
        app.auto_apply.borrow_mut().take();
        app.toast("转码完成，但自动重新应用没有成功；请手动点“应用”再试一次");
        return;
    }
    pending.tried = true;
    *app.auto_apply.borrow_mut() = Some(pending.clone());
    app.toast("转码完成，正在重新应用…");
    app.send(Cmd::Apply(Box::new(pending.request)));
}

/// 每 500ms 对进行中的转码发一次幂等查询，进度条/按钮文案原地更新。
fn poll_prepare(app: &Rc<App>) {
    let running: Vec<String> = app
        .prepare
        .borrow()
        .iter()
        .filter(|(_, result)| matches!(result.state, PrepareState::Queued | PrepareState::Running))
        .map(|(path, _)| path.clone())
        .collect();
    for path in running {
        app.send(Cmd::Prepare { path, force: false });
    }
}

fn refresh_details(app: &Rc<App>) {
    if let Some(view) = app.details.borrow().as_ref() {
        details::refresh(app, view);
    }
}

fn refresh_displays(app: &Rc<App>) {
    if app.stack.visible_child_name().as_deref() != Some("displays") {
        return;
    }
    if let Some(view) = app.displays.borrow().as_ref() {
        displays::refresh(app, view);
    }
}

/// Status 中会影响界面的字段指纹（忽略 `presented`/`present_ok` 等每帧自增计数，避免空闲每秒重排）。
fn status_key(status: &StatusResult) -> String {
    use std::fmt::Write as _;
    let mut key = String::with_capacity(128);
    let _ = write!(
        key,
        "{}|{}|{}|{}|{}",
        status.state,
        status.output.as_deref().unwrap_or(""),
        status.media.as_deref().unwrap_or(""),
        status.paused,
        status.state_hint.as_deref().unwrap_or("")
    );
    for channel in &status.channels {
        let _ = write!(
            key,
            "|{}:{}:{}:{}:{}:{}:{}:{}:{}",
            channel.output,
            channel.state,
            channel.media.as_deref().unwrap_or(""),
            channel.decoder,
            channel.paused,
            channel.pinned,
            channel.pause_reason.as_deref().unwrap_or(""),
            channel
                .video
                .as_ref()
                .map_or(0, |video| video.present_failures),
            channel
                .video
                .as_ref()
                .and_then(|video| video.decode_alert.as_deref())
                .unwrap_or("")
        );
    }
    key
}

fn update_statusbar(app: &Rc<App>) {
    let (status, metrics, error, active_media_name) = {
        let state = app.state.borrow();
        let active_media_name = state
            .status
            .as_ref()
            .and_then(|st| st.media.as_deref())
            .map(|path| {
                state
                    .rows
                    .iter()
                    .find(|item| item.path == path)
                    .map_or_else(|| short(path), |item| item.name.clone())
            });
        (
            state.status.clone(),
            state.metrics.clone(),
            state.last_error.clone(),
            active_media_name,
        )
    };
    if let Some(metrics) = &metrics {
        app.cpu
            .set_text(&format!("CPU {:.0}%", metrics.cpu_percent));
        app.mem
            .set_text(&format!("内存 {}", ui::human_size(metrics.mem_bytes)));
        app.fps.set_text(&format!("{:.1} fps", metrics.fps));
    }
    let paused = status.as_ref().is_some_and(|status| status.paused);
    app.pause_button.set_icon_name(if paused {
        "media-playback-start-symbolic"
    } else {
        "media-playback-pause-symbolic"
    });
    app.pause_button.set_tooltip_text(Some(if paused {
        "继续播放（Space）"
    } else {
        "暂停播放（Space）"
    }));

    if let Some(id) = status.as_ref().and_then(|st| st.media.as_deref()) {
        sync_liuguang_theme(app, id);
    }

    let (text, tooltip) = if let Some(error) = error {
        app.status_left.add_css_class("error");
        (format!("错误：{error}"), format!("错误：{error}"))
    } else {
        app.status_left.remove_css_class("error");
        match &status {
            Some(status) => {
                let media = active_media_name.unwrap_or_else(|| "（没有画面）".to_owned());
                let pinned = status
                    .channels
                    .first()
                    .is_some_and(|channel| channel.pinned);
                let hint = status
                    .state_hint
                    .as_deref()
                    .map_or_else(String::new, |hint| format!("（{hint}）"));
                let pause = status
                    .channels
                    .iter()
                    .find_map(|channel| channel.pause_reason.as_deref())
                    .map(|reason| format!(" · 暂停原因：{}", ui::pause_reason_text(Some(reason))));
                let line = format!(
                    "{}{}{}{} · {} · {}",
                    ui::state_text(&status.state),
                    if pinned { " · 只放这一项" } else { "" },
                    pause.unwrap_or_default(),
                    hint,
                    media,
                    status.mode.as_deref().unwrap_or("—"),
                );
                // tooltip：完整状态 + 每个通道的解码告警（多行，不截断）。
                let mut detail = line.clone();
                for channel in &status.channels {
                    if let Some(video) = &channel.video {
                        if let Some(alert) = &video.decode_alert {
                            detail.push('\n');
                            detail.push_str(&format!("{} 解码告警：{alert}", channel.output));
                        }
                    }
                }
                (line, detail)
            }
            None => (
                "正在连接守护进程…".to_owned(),
                "正在连接守护进程…".to_owned(),
            ),
        }
    };
    app.status_left.set_text(&text);
    app.status_left.set_tooltip_text(if tooltip.is_empty() {
        None
    } else {
        Some(&tooltip)
    });
}

fn short(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn sync_liuguang_theme(app: &Rc<App>, id: &str) {
    if !app.textures.borrow().contains_key(&(id.to_owned(), 0u64)) {
        app.request_thumb(id, 0);
    }
    if let Some(&accent) = app.thumb_accents.borrow().get(id) {
        ui::update_liuguang_css(accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_click_applies_and_double_click_is_idempotent() {
        let t0 = Instant::now();
        assert!(click_should_apply(None, "a", t0));
        assert!(!click_should_apply(
            Some(("a", t0)),
            "a",
            t0 + Duration::from_millis(120)
        ));
        assert!(click_should_apply(
            Some(("a", t0)),
            "b",
            t0 + Duration::from_millis(120)
        ));
        assert!(click_should_apply(
            Some(("a", t0)),
            "a",
            t0 + Duration::from_millis(600)
        ));
    }

    #[test]
    fn error_notifications_are_deduped_by_code_for_five_seconds() {
        let mut seen = std::collections::HashMap::new();
        let t0 = Instant::now();
        assert!(should_notify(&mut seen, "decode", t0));
        // 同一个错误码在 5s 窗口内只提示一次（包装消息与 daemon 原始事件算同一码）。
        assert!(!should_notify(
            &mut seen,
            "decode",
            t0 + Duration::from_secs(4)
        ));
        // 不同错误码不共享窗口。
        assert!(should_notify(
            &mut seen,
            "apply",
            t0 + Duration::from_secs(1)
        ));
        // 窗口过期后同一个码可以再次提醒。
        assert!(should_notify(
            &mut seen,
            "decode",
            t0 + Duration::from_secs(6)
        ));
    }

    #[test]
    fn prepare_wait_is_not_reported_as_error() {
        assert!(is_expected_prepare_wait(
            "壁纸应用失败: 该素材是 10bit（yuv420p10le）…；已排队，完成后会自动播放：/x.mp4"
        ));
        assert!(!is_expected_prepare_wait(
            "壁纸应用失败: 视频打开失败（坏.mp4）"
        ));
    }

    #[test]
    fn errors_are_classified_by_stable_prefixes() {
        assert_eq!(error_code("壁纸应用失败：视频打开失败（坏.mp4）"), "apply");
        assert_eq!(
            error_code("视频打开失败：atom has bogus size"),
            "video-open"
        );
        assert_eq!(error_code("媒体读取或解码失败：抽取缩略图超时"), "decode");
        assert_eq!(error_code("转码失败：ffmpeg 退出码 1"), "transcode");
        assert_eq!(error_code("某条没见过的错误"), "other");
        // toast 只放第一行，且按字符截断（不切 UTF-8）。
        let hint = first_line("第一行\n第二行");
        assert_eq!(hint, "第一行");
        let long = "错".repeat(200);
        let hint = first_line(&long);
        assert_eq!(hint.chars().count(), 97);
        assert!(hint.ends_with('…'));
    }

    #[test]
    fn args_parse_persona_aliases() {
        assert_eq!(resolve_persona(Some("sys")), Persona::System);
        assert_eq!(resolve_persona(Some("session")), Persona::User);
        assert_eq!(resolve_persona(Some("auto")), Persona::detect(None));
    }

    #[test]
    fn args_require_values() {
        let mut iter = ["--socket"].into_iter().map(str::to_owned);
        assert!(parse_args(&mut iter).is_err());
    }

    #[test]
    fn unknown_args_fail() {
        let mut iter = ["--nope"].into_iter().map(str::to_owned);
        assert!(parse_args(&mut iter).is_err());
    }

    #[test]
    fn help_is_an_error_channel() {
        let mut iter = ["--help"].into_iter().map(str::to_owned);
        let error = parse_args(&mut iter).unwrap_err();
        assert!(error.contains("用法"));
    }

    #[test]
    fn short_names_strip_directories() {
        assert_eq!(short("/a/b/c.mp4"), "c.mp4");
    }
}
