//! 应用外壳：终端生命周期 + **事件驱动**事件循环 + IPC 副作用执行 + 事件订阅。
//!
//! 性能预算（规范 §8）：事件驱动渲染（`needs_render`）、指标 ≤2 Hz、
//! 空闲阻塞在 `event::poll` 上（超时可测，无忙循环）。
//!
//! 安全边界：TUI **不打开 `/dev/dri`**、不解码视频、**不扫描本地目录**；
//! 媒体数据、路径校验、状态判定全部来自 daemon。

use crate::event::{Action, Effect, PatchKind, action_for, apply};
use crate::ipc_client::{
    IpcControl, MediaQuery, MockControl, Session, SetWallpaperAck, StaticCallError,
};
use crate::model::{
    AppState, ApplyPhase, Connection, MEDIA_PAGE_SIZE, METRICS_INTERVAL, NotificationKind,
    POLL_TIMEOUT, Page, PendingState,
};
use crate::ui;
use crate::{TuiError, TuiOptions};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use liuguang_ipc::protocol::{EventKind, EventMessage, SetWallpaperParams};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::Backend;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// 应用外壳。
pub struct App<C: Session> {
    /// 状态模型（渲染层只读）。
    pub state: AppState,
    /// 控制客户端。
    pub client: C,
    /// 事件流订阅（断线后重建）。
    event_stream: Option<Box<dyn crate::ipc_client::EventSource>>,
    /// 尚未收到最终结果的壁纸请求：`request_id` → 目标输出。
    ///
    /// 用于在 daemon 广播 `Applied`/`Failed` 时把结果归属到具体输出。
    pending_wallpaper_by_id: BTreeMap<String, Option<String>>,
    /// 渲染脏标记：模型/订阅更新后置位，`draw` 后清零。
    ///
    /// 事件循环据此避免在空闲期无意义重绘。
    needs_render: bool,
    last_metrics: Option<Instant>,
}

impl<C: Session> App<C> {
    /// 以客户端构造。
    pub fn new(client: C) -> Self {
        let info = client.info();
        let mut state = AppState::new();
        state.session_kind = info.kind.to_string();
        if let Some(socket) = info.socket {
            state.socket_path = socket;
        }
        Self {
            state,
            client,
            event_stream: None,
            pending_wallpaper_by_id: BTreeMap::new(),
            needs_render: true,
            last_metrics: None,
        }
    }

    /// 以给定状态构造。
    pub fn with_state(client: C, mut state: AppState) -> Self {
        let info = client.info();
        state.session_kind = info.kind.to_string();
        if let Some(socket) = info.socket {
            state.socket_path = socket;
        }
        Self {
            state,
            client,
            event_stream: None,
            pending_wallpaper_by_id: BTreeMap::new(),
            needs_render: true,
            last_metrics: None,
        }
    }

    /// 是否需要重绘（事件驱动渲染的核心）。
    pub fn needs_render(&self) -> bool {
        self.needs_render
    }

    /// 是否已订阅事件流。
    pub fn subscribed(&self) -> bool {
        self.event_stream.is_some()
    }

    /// 处理一个意图。
    pub fn on_action(&mut self, action: Action) {
        let effects = apply(&mut self.state, action);
        self.needs_render = true;
        self.execute(effects);
    }

    /// 处理一个按键。
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        // 终端可能上报**松开**事件（kitty 键盘协议 / 增强键盘上报）。若不忽略，
        // 同一个键会被处理两次（"输入被吃掉/重复"、弹窗按 Esc 看似无效）——只处理按下与重复。
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        if let Some(action) = action_for(&self.state, key) {
            self.on_action(action);
        }
    }

    /// 终端尺寸变化（**保留**选中项/页码/编辑框内容）。
    pub fn on_resize(&mut self, width: u16, height: u16) {
        self.state.size = (width, height);
        self.needs_render = true;
    }

    /// 时间推进：指标 ≤2 Hz、搜索防抖、应用确认超时。
    pub fn tick(&mut self, now: Instant) {
        if self.metrics_due(now) {
            self.last_metrics = Some(now);
            self.on_action(Action::MetricsTick);
        }
        if self.state.search_debounce_due(now) {
            self.state.search_dirty_at = None;
            self.on_action(Action::SearchDue);
        }
        self.on_action(Action::ApplyTimeoutCheck);
        self.drain_events();
    }

    fn metrics_due(&self, now: Instant) -> bool {
        match self.last_metrics {
            None => true,
            Some(at) => now.duration_since(at) >= METRICS_INTERVAL,
        }
    }

    /// 事件循环下一次 poll 的超时（恒 > 0 ⇒ 空闲不是忙循环）。
    pub fn poll_timeout(&self, now: Instant) -> Duration {
        let mut t = POLL_TIMEOUT;
        if let Some(at) = self.last_metrics {
            t = t.min(METRICS_INTERVAL.saturating_sub(now.duration_since(at)));
        }
        if let Some(at) = self.state.search_dirty_at {
            t = t.min(crate::model::SEARCH_DEBOUNCE.saturating_sub(now.duration_since(at)));
        }
        t.max(Duration::from_millis(1))
    }

    /// 全量刷新（进入 TUI、`R`、重连成功）。
    pub fn refresh_all(&mut self) {
        self.execute(vec![Effect::RefreshAll]);
    }

    /// 订阅事件流（幂等；失败置 `Offline`）。
    pub fn subscribe(&mut self) {
        match self.client.subscribe() {
            Ok(src) => self.event_stream = Some(src),
            Err(e) => {
                self.state.connection = Connection::Offline(e.clone());
                self.state
                    .notify(NotificationKind::Error, format!("订阅失败：{e}"));
            }
        }
    }

    /// 消费事件流（非阻塞，单次有界）。
    pub fn drain_events(&mut self) {
        let Some(src) = self.event_stream.as_mut() else {
            return;
        };
        match src.try_next() {
            Ok(events) => {
                let supported: Vec<EventMessage> = events
                    .into_iter()
                    .filter(crate::ipc_client::event_is_supported)
                    .collect();
                for ev in supported {
                    self.handle_event(ev);
                }
            }
            Err(e) => {
                self.event_stream = None;
                self.state.connection = Connection::Offline(e.clone());
                self.state
                    .notify(NotificationKind::Error, format!("事件流断开：{e}"));
                self.needs_render = true;
            }
        }
    }

    /// 处理一条事件（受理/完成分离、扫描进度、输出变化、错误）。
    pub fn handle_event(&mut self, ev: EventMessage) {
        self.needs_render = true;
        match ev.kind {
            EventKind::WallpaperApplied => {
                let rid = ev.request_id.clone().unwrap_or_default();
                let output = ev
                    .output
                    .clone()
                    .or_else(|| self.pending_wallpaper_by_id.get(&rid).cloned().flatten());
                let ok = ev.ok.unwrap_or(true);
                let batch = self.pending_wallpaper_by_id.len() > 1;
                let who = output.clone().unwrap_or_else(|| "默认输出".to_string());
                // T25-F2：**失败必须一律推通知**（此前只有批量才推 ⇒ 单目标失败在操作页不可见，
                // 用户会以为"还在处理"，实际已经失败）。成功则按批/单分别给出终态文案。
                if !ok {
                    let reason = ev
                        .error
                        .clone()
                        .unwrap_or_else(|| "daemon 未提供原因".to_string());
                    self.state
                        .push_error(format!("壁纸应用失败（{who}）：{reason}"));
                } else if batch {
                    self.state
                        .notify(NotificationKind::Info, format!("{who}：已应用"));
                } else {
                    self.state.notify(
                        NotificationKind::Info,
                        format!("已应用（{who}）：daemon 已确认上屏"),
                    );
                }
                self.pending_wallpaper_by_id.remove(&rid);
                // 只有**全部**目标都收到事件（且成功）才把状态置为 Applied
                if ok {
                    if self.pending_wallpaper_by_id.is_empty() {
                        self.state.apply_phase = Some(ApplyPhase::Applied);
                    } else {
                        self.state.apply_phase = Some(ApplyPhase::Accepted);
                    }
                } else {
                    self.state.apply_phase = Some(ApplyPhase::Failed(
                        ev.error.clone().unwrap_or_else(|| "应用失败".to_string()),
                    ));
                }
                self.state.set_op_state(
                    "set_wallpaper",
                    if ok {
                        PendingState::Applied
                    } else {
                        PendingState::Failed(ev.error.clone().unwrap_or_default())
                    },
                );
            }
            EventKind::ScanProgress => {
                self.state.scan = crate::model::ScanState::Running {
                    scan_id: ev.scan_id.unwrap_or(0),
                };
            }
            EventKind::ScanFinished => {
                self.state.scan = crate::model::ScanState::Finished {
                    scan_id: ev.scan_id.unwrap_or(0),
                    found: ev.found.unwrap_or(0) as usize,
                };
                self.execute(vec![Effect::LoadMedia]);
            }
            EventKind::OutputChanged => {
                self.state.notify(
                    NotificationKind::Info,
                    format!(
                        "输出 {} {} {}",
                        ev.output.clone().unwrap_or_default(),
                        if ev.connected.unwrap_or(true) {
                            "已连接"
                        } else {
                            "已断开"
                        },
                        ev.mode.clone().unwrap_or_default()
                    ),
                );
                self.execute(vec![Effect::RefreshAll]);
            }
            EventKind::Error => {
                let code = ev.code.clone().unwrap_or_default();
                let msg = ev
                    .message
                    .clone()
                    .unwrap_or_else(|| "daemon 报错".to_string());
                // 在途的壁纸请求收到 ERROR ⇒ **明确的终态失败**，
                // 不能让界面停在"已受理…等待"（t34 的 daemon 侧会额外推 Error 兜底）。
                if !self.pending_wallpaper_by_id.is_empty() {
                    let who = ev.output.clone().unwrap_or_else(|| "默认输出".to_string());
                    self.pending_wallpaper_by_id.clear();
                    self.state.apply_phase = Some(ApplyPhase::Failed(format!("{who}：{msg}")));
                    self.state
                        .set_op_state("set_wallpaper", PendingState::Failed(msg.clone()));
                    self.state
                        .push_error(format!("壁纸应用失败（{who}）：{code} {msg}"));
                } else {
                    self.state
                        .notify(NotificationKind::Error, format!("{code}: {msg}"));
                }
            }
            // t15：自动转码完成（daemon 把"硬件解不动"的素材转成 8bit HEVC 缓存）。
            // TUI 不展示转码队列——进度条与角标归 GUI；这里只做一件事：告诉用户
            // 可以重新应用了，否则"点了没反应"会被当成故障。
            EventKind::MediaPrepared => {
                let source = ev.media.clone().unwrap_or_default();
                self.state.notify(
                    NotificationKind::Info,
                    format!("自动转码完成，现在可以应用了：{source}"),
                );
            }
            EventKind::StateChanged | EventKind::MediaChanged => {
                // 用 daemon 的权威状态刷新（不猜）
                if let Ok(status) = self.client.status() {
                    self.state.status = status;
                }
            }
        }
    }

    /// 渲染一帧。
    pub fn draw(&mut self, f: &mut Frame) {
        ui::draw(f, &self.state);
        self.needs_render = false;
    }

    /// 执行 IPC 副作用（**只有这里**与客户端交互）。
    pub fn execute(&mut self, effects: Vec<Effect>) {
        for e in effects {
            self.execute_one(e);
        }
        self.needs_render = true;
    }

    fn mark_offline(&mut self, what: &str, err: String) {
        self.state.connection = Connection::Offline(err.clone());
        self.state
            .notify(NotificationKind::Error, format!("{what} 失败：{err}"));
    }

    fn execute_one(&mut self, effect: Effect) {
        match effect {
            Effect::Quit => self.state.quit = true,
            Effect::Reconnect => match self.client.reconnect() {
                Ok(()) => {
                    self.state.connection = Connection::Online;
                    self.state
                        .notify(NotificationKind::Info, "已重新连接 daemon");
                    self.subscribe();
                    self.refresh_all();
                }
                Err(e) => {
                    self.state.connection = Connection::Offline(e.clone());
                    self.state
                        .notify(NotificationKind::Error, format!("重连失败：{e}"));
                }
            },
            Effect::RefreshAll => {
                match self.client.status() {
                    Ok(s) => {
                        self.state.status = s;
                        self.state.connection = Connection::Online;
                    }
                    Err(e) => self.mark_offline("读取状态", e),
                }
                match self.client.outputs() {
                    Ok(o) => {
                        if self.state.selected_output.is_none() {
                            self.state.selected_output = o
                                .outputs
                                .iter()
                                .find(|x| x.connected)
                                .map(|x| x.name.clone());
                        }
                        self.state.outputs = o;
                    }
                    Err(e) => self.mark_offline("枚举输出", e),
                }
                match self.client.list_sources() {
                    Ok(s) => {
                        self.state.media_dir_hint = s.root.clone();
                        self.state.sources = s;
                    }
                    Err(e) => {
                        if self.state.connection.is_online() {
                            self.state
                                .notify(NotificationKind::Warn, format!("来源：{e}"));
                        }
                    }
                }
                match self.client.list_playlists() {
                    Ok(p) => self.state.playlists = p,
                    Err(e) => {
                        if self.state.connection.is_online() {
                            self.state
                                .notify(NotificationKind::Warn, format!("播放列表：{e}"));
                        }
                    }
                }
                if self.event_stream.is_none() {
                    self.subscribe();
                }
                self.execute_one(Effect::LoadMetrics);
                self.execute_one(Effect::LoadMedia);
            }
            Effect::LoadMedia => {
                let query = MediaQuery {
                    page: self.state.media.page,
                    page_size: MEDIA_PAGE_SIZE.min(crate::model::MAX_MEDIA_PAGE_SIZE),
                    filter: self.state.filter,
                    sort: self.state.sort,
                    search: self.state.search.clone(),
                    source_id: None,
                };
                match self.client.list_media(&query) {
                    Ok(m) => {
                        self.state.media = m;
                        self.state.media_cursor = self
                            .state
                            .media_cursor
                            .min(self.state.media.items.len().saturating_sub(1));
                        self.state.connection = Connection::Online;
                        if let Some(item) = self.state.selected_media() {
                            let id = item.id.clone();
                            self.execute_one(Effect::LoadDetail(id));
                        }
                    }
                    Err(e) => {
                        if matches!(self.state.connection, Connection::Online) {
                            self.state.connection = Connection::Reconnecting;
                        }
                        self.state
                            .notify(NotificationKind::Error, format!("媒体列表：{e}"));
                    }
                }
            }
            Effect::LoadDetail(id) => match self.client.get_media_compat(&id) {
                Ok((item, compat)) => {
                    self.state.detail = Some(item);
                    self.state.detail_compat = compat;
                    self.state.detail_error = None;
                }
                Err(e) => {
                    self.state.detail = None;
                    self.state.detail_compat = None;
                    self.state.detail_error = Some(e.clone());
                }
            },
            Effect::LoadMetrics => match self.client.metrics() {
                Ok(m) => self.state.metrics = m,
                Err(e) => {
                    if self.state.connection.is_online() {
                        self.state
                            .notify(NotificationKind::Warn, format!("指标：{e}"));
                    }
                }
            },
            Effect::ScanSource(source_id) => match self.client.scan_source(source_id, false) {
                Ok(ack) => {
                    self.state.scan = crate::model::ScanState::Requested {
                        scan_id: ack.scan_id,
                    };
                    self.state.notify(
                        NotificationKind::Info,
                        format!("扫描已受理（scan {}，后台执行）", ack.scan_id),
                    );
                }
                Err(e) => {
                    self.state.scan = crate::model::ScanState::Failed(e.clone());
                    self.state
                        .notify(NotificationKind::Error, format!("扫描失败：{e}"));
                }
            },
            Effect::SetWallpaper { path, outputs } => {
                self.state.apply_phase = Some(ApplyPhase::Sending);
                self.state.apply_started_at = Some(Instant::now());
                self.state.begin_op("set_wallpaper", format!("应用 {path}"));
                self.pending_wallpaper_by_id.clear();
                let total = outputs.len().max(1);
                let mut accepted = 0;
                for output in outputs {
                    let params = SetWallpaperParams {
                        path: path.clone(),
                        output: output.clone(),
                        fit: Some(self.state.transform.fit.clone()),
                        zoom: Some(f64::from(self.state.transform.zoom)),
                        offset_x: Some(f64::from(self.state.transform.offset_x)),
                        offset_y: Some(f64::from(self.state.transform.offset_y)),
                        rotation: Some(self.state.transform.rotation),
                        brightness: Some(f64::from(self.state.transform.brightness)),
                        pin: None,
                    };
                    match self.client.set_wallpaper(params) {
                        Ok(SetWallpaperAck {
                            accepted: true,
                            request_id,
                        }) => {
                            accepted += 1;
                            if let Some(rid) = request_id {
                                self.pending_wallpaper_by_id.insert(rid, output);
                            }
                        }
                        Ok(SetWallpaperAck {
                            accepted: false, ..
                        }) => {
                            self.state.notify(NotificationKind::Error, "daemon 未受理");
                        }
                        Err(e) => {
                            // 失败：**当前壁纸不变**（不写 state.status.media）
                            self.state.apply_phase = Some(ApplyPhase::Failed(e.clone()));
                            self.state
                                .set_op_state("set_wallpaper", PendingState::Failed(e.clone()));
                            self.state
                                .notify(NotificationKind::Error, format!("设置失败：{e}"));
                        }
                    }
                }
                if accepted > 0 {
                    // 受理 ≠ 已上屏：进入 Accepted，等 WALLPAPER_APPLIED
                    if self.pending_wallpaper_by_id.is_empty() {
                        // 没有 request_id ⇒ 无法对账；明确提示而不是当成已应用
                        self.state.apply_phase = Some(ApplyPhase::Accepted);
                        self.state.notify(
                            NotificationKind::Warn,
                            "已受理但响应未带 request_id：无法对账完成事件",
                        );
                    } else {
                        self.state.apply_phase = Some(ApplyPhase::Accepted);
                        self.state.notify(
                            NotificationKind::Info,
                            format!(
                                "已受理（{accepted}/{total}）：等待 daemon 确认上屏；按 `W` 可重试",
                            ),
                        );
                    }
                }
            }
            Effect::NextMedia(output) => {
                let r = self.client.next_media(output);
                self.report("下一张", r);
            }
            Effect::Pause(output) => {
                let r = self.client.pause(output);
                self.report("暂停", r);
            }
            Effect::Resume(output) => {
                let r = self.client.resume(output);
                self.report("继续", r);
            }
            Effect::Reload => {
                let r = self.client.reload();
                self.report("重载配置", r);
            }
            Effect::SetTransition(p) => {
                let params = liuguang_ipc::TransitionParams {
                    effect: p.effect,
                    duration_ms: p.duration_ms,
                    easing: p.easing,
                    direction: p.direction,
                };
                let r = self.client.set_transition(params);
                self.report("转场", r);
            }
            Effect::ApplyTransform(output) => {
                let params = self.state.transform.to_params(Some(output.clone()));
                match self.client.set_output_transform(&params) {
                    Ok(()) => self.state.notify(
                        NotificationKind::Info,
                        format!("已立即应用（{output}，运行时；`S` 才写配置）"),
                    ),
                    Err(e) => self
                        .state
                        .notify(NotificationKind::Error, format!("应用失败：{e}")),
                }
            }
            Effect::UpdatePlaylist(params) => {
                let playlist_id = params.playlist_id.clone();
                match self.client.update_playlist(params) {
                    Ok(pl) => {
                        self.state.playlist_edit = None;
                        self.state.notify(
                            NotificationKind::Info,
                            format!(
                                "播放列表已更新（运行时）：{}（{} 项，{}，{}s）；`S` 才写配置",
                                pl.name,
                                pl.entries.len(),
                                pl.mode,
                                pl.interval_seconds
                            ),
                        );
                        let _ = playlist_id;
                        self.execute(vec![Effect::RefreshAll]);
                    }
                    Err(e) => self
                        .state
                        .notify(NotificationKind::Error, format!("更新播放列表失败：{e}")),
                }
            }
            // t67：就地更新（模式选择器 / 媒体库页 `a` 加入）。
            // 与 `UpdatePlaylist` 的区别：**不退出编辑态、不清缓冲**，反馈按来路给。
            Effect::PlaylistPatch { params, kind } => match self.client.update_playlist(params) {
                Ok(pl) => {
                    let msg = match kind {
                        PatchKind::Mode(label) => {
                            format!("播放模式：{label}（已下发到 daemon；`S` 才落盘）")
                        }
                        PatchKind::AddEntry { .. } => {
                            format!("已加入「{}」（{} 条）", pl.name, pl.entries.len())
                        }
                    };
                    self.state.notify(NotificationKind::Info, msg);
                    // 只刷新清单（不 Reset 光标以外的东西）：RefreshAll 会重取播放列表
                    self.execute_one(Effect::RefreshAll);
                }
                Err(e) => self
                    .state
                    .notify(NotificationKind::Error, format!("更新播放列表失败：{e}")),
            },
            // 静态壁纸（t70）：真实 IPC。daemon 未实现 ⇒ `UNSUPPORTED_METHOD`
            // ⇒ 转成**可操作**提示；绝不静默、绝不假装成功。
            Effect::SetStaticWallpaper { path } => match self.client.set_static_wallpaper(&path) {
                Ok(v) => {
                    self.state.static_supported = Some(true);
                    self.state.static_wallpaper = Some(path.clone());
                    // 刚做完这件事，TUI 是当事人 ⇒ 播放源直接记 "static"。
                    // TODO：daemon 的 `status` 提供 `playback_source` 后，以它为准（当前按通道暂停状态推断）。
                    self.state.playback_source = Some("static".to_string());
                    self.state.notify(
                        NotificationKind::Info,
                        format!("已设为静态壁纸：{path}（暂停播放，静止显示）"),
                    );
                    self.execute_one(Effect::RefreshAll);
                    let _ = v;
                }
                Err(StaticCallError::Unsupported(msg)) => {
                    self.state.static_supported = Some(false);
                    // 通知栏只有一行：短句 + **可操作动作**放最前；完整报文进"最近错误"
                    self.state.notify(
                        NotificationKind::Error,
                        "daemon 拒绝（不是你的操作问题，旧版本未实现该接口）；临时：媒体库选图片后按 W",
                    );
                    self.state.record_error(format!(
                        "set_static_wallpaper 不被本版 liuguang 支持：{msg}"
                    ));
                }
                Err(StaticCallError::Failed(e)) => self
                    .state
                    .notify(NotificationKind::Error, format!("设为静态壁纸失败：{e}")),
            },
            Effect::ClearStaticWallpaper => match self.client.clear_static_wallpaper() {
                Ok(v) => {
                    self.state.static_supported = Some(true);
                    self.state.static_wallpaper = None;
                    // 恢复到哪个源由 daemon 说了算（未来的 `status.playback_source`）；
                    // 现在没有该字段 ⇒ 清空，让页面按 status/播放列表**标注推断**。
                    self.state.playback_source = None;
                    self.state.notify(
                        NotificationKind::Info,
                        "已清除静态壁纸：恢复播放列表（从上次位置继续）".to_string(),
                    );
                    self.execute_one(Effect::RefreshAll);
                    let _ = v;
                }
                Err(StaticCallError::Unsupported(msg)) => {
                    self.state.static_supported = Some(false);
                    self.state.notify(
                        NotificationKind::Error,
                        "daemon 拒绝（不是你的操作问题，旧版本未实现该接口）；临时：到【播放列表】按 Enter",
                    );
                    self.state.record_error(format!(
                        "clear_static_wallpaper 不被本版 liuguang 支持：{msg}"
                    ));
                }
                Err(StaticCallError::Failed(e)) => self
                    .state
                    .notify(NotificationKind::Error, format!("清除静态壁纸失败：{e}")),
            },
            // T25-F1 的 UI 侧残留：`saved` 只代表"字节写出去了"，
            // **`verified` 才代表"改动真的进了配置"** ⇒ 提示必须依据 `verified`。
            Effect::SaveConfig => match self.client.save_config() {
                // 无论成功/校验失败，都记住 daemon 回传的**实际路径**（供确认框与系统页使用）
                Ok(r) if !r.saved => {
                    self.state.config_path = Some(r.path.clone());
                    self.state.push_error(format!(
                        "配置**未写出**（saved=false）：{} —— 改动仍在运行时，重启会丢失",
                        r.path
                    ));
                }
                Ok(r) if !r.verified => {
                    self.state.config_path = Some(r.path.clone());
                    // 不得显示成功：保持 dirty，明确告知"写出去了但回读校验没过"
                    // 不猜测原因：具体是哪些配置容器仍在覆盖由 daemon 的 conflicts 说明
                    // （t37 已证伪「配置根会派生来源」的说法，此处不得再出现）
                    let hint = if self.state.sources_dirty {
                        "（若本次涉及来源改动，请按 `S` 重新保存并查看 daemon 报出的 conflicts 容器名）"
                    } else {
                        ""
                    };
                    self.state.push_error(format!(
                        "配置已写出但**回读校验未通过**（改动可能未生效）：{}{hint}，改动仍在运行时，请查看 daemon 日志后重试",
                        r.path
                    ));
                    // 立刻用权威列表纠正屏幕：不让用户以为改动已生效
                    if let Ok(list) = self.client.list_sources() {
                        self.state.sources = list;
                    }
                }
                Ok(r) => {
                    self.state.config_path = Some(r.path.clone());
                    self.state.dirty = false;
                    if r.merged_sources > 0 {
                        self.state.sources_dirty = false;
                    }
                    let merged = if r.merged_sources > 0 {
                        format!("；已合并 {} 项运行时来源改动", r.merged_sources)
                    } else {
                        String::new()
                    };
                    self.state.notify(
                        NotificationKind::Info,
                        format!(
                            "已保存配置（回读校验通过）：{} · {} 字节{merged}",
                            r.path, r.bytes
                        ),
                    );
                }
                Err(e) => self.state.push_error(format!("保存失败：{e}")),
            },
            Effect::AddSource {
                kind,
                path,
                recursive,
            } => match self.client.add_source(&kind, &path, recursive) {
                Ok(()) => {
                    self.state.sources_dirty = true;
                    self.state.notify(
                        NotificationKind::Info,
                        format!("来源已添加（运行时）：{path}（{kind}，递归={recursive}）"),
                    );
                    // 运行时生效：立刻重列来源（真实数据），并把光标移到**刚新增**的那一项，
                    // 否则用户下一步想删它/重扫它却选不中（可用性缺陷）。
                    if let Ok(list) = self.client.list_sources() {
                        self.state.sources = list;
                        let want = source_file_name(&path);
                        if let Some(idx) = self
                            .state
                            .sources
                            .sources
                            .iter()
                            .position(|s| s.name == want && s.kind == kind)
                        {
                            self.state.sources_cursor = idx;
                        } else {
                            self.state.notify(
                                NotificationKind::Warn,
                                format!("已新增但未在列表中定位到「{want}」：请用 ↑↓ 选择"),
                            );
                        }
                    }
                    self.state.notify(
                        NotificationKind::Warn,
                        "改动未落盘：在来源管理里按 `s` 显式保存",
                    );
                }
                Err(e) => self.state.push_error(format!("新增来源失败：{e}")),
            },
            Effect::RemoveSource { source_id } => match self.client.remove_source(&source_id) {
                Ok(()) => {
                    // 删除「真的生效了吗」必须看 daemon 的**权威列表**，不能凭调用 Ok 就宣布成功
                    let still_there = match self.client.list_sources() {
                        Ok(list) => {
                            let still = list.sources.iter().any(|s| s.id == source_id);
                            self.state.sources = list;
                            self.state.sources_cursor = self
                                .state
                                .sources_cursor
                                .min(self.state.sources.sources.len().saturating_sub(1));
                            still
                        }
                        Err(e) => {
                            self.state
                                .push_error(format!("删除后无法确认来源列表：{e}"));
                            return;
                        }
                    };
                    if still_there {
                        // 真机 F4：来源被其它**配置容器**覆盖时，运行时删除无效（容器名以 daemon 的 conflicts 为准）
                        self.state.push_error(
                            "该来源仍被**配置容器**覆盖（如 `[slideshow].directory`），**运行时删除无效** ⇒ 需直接修改配置文件；保存时 daemon 会回报具体容器（conflicts）",
                        );
                        self.state.notify(
                            NotificationKind::Info,
                            format!("未做任何改动（该来源仍存在）：{source_id}"),
                        );
                    } else {
                        self.state.sources_dirty = true;
                        self.state.notify(
                            NotificationKind::Info,
                            format!("来源已删除（运行时）：{source_id}"),
                        );
                        self.state.notify(
                            NotificationKind::Warn,
                            "改动未落盘：在来源管理里按 `s` 显式保存",
                        );
                    }
                }
                Err(e) => self.state.push_error(format!("删除来源失败：{e}")),
            },
            Effect::CopyPath(path) => {
                if !write_osc52_clipboard(&path) {
                    // 回退：可选中文本弹窗（不得假设复制成功）
                    self.state.open_modal(crate::model::Modal::PathText {
                        title: "复制路径（终端不支持 OSC52）".to_string(),
                        text: path,
                    });
                    self.state
                        .notify(NotificationKind::Warn, "OSC52 复制不可用：已显示可选中文本");
                } else {
                    self.state
                        .notify(NotificationKind::Info, "已尝试复制路径（OSC52）");
                }
            }
        }
    }

    fn report(&mut self, what: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self
                .state
                .notify(NotificationKind::Info, format!("{what}：已受理")),
            Err(e) => self
                .state
                .notify(NotificationKind::Error, format!("{what} 失败：{e}")),
        }
    }
}

/// 从服务器绝对路径取文件名（**纯字符串**，不访问文件系统）。
fn source_file_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// OSC52 剪贴板写入（尽力而为；终端不支持时返回 `false`）。
fn write_osc52_clipboard(text: &str) -> bool {
    let mut out = std::io::stdout();
    let payload = base64_encode(text.as_bytes());
    write!(out, "\x1b]52;c;{payload}\x07").is_ok() && out.flush().is_ok()
}

/// 极简 base64（只用于 OSC52 载荷）。
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 终端恢复守卫：离开作用域时恢复 raw mode 与主屏。
pub struct TerminalGuard {
    restored: bool,
}

impl TerminalGuard {
    /// 进入备用屏 + raw mode。
    ///
    /// # Errors
    ///
    /// 终端不支持时返回 [`TuiError::Io`]。
    pub fn enter() -> Result<Self, TuiError> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self { restored: false })
    }

    /// 显式恢复（幂等）。
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = stdout.flush();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// 安装 panic 钩子：**尽力**恢复终端（退出 raw mode、恢复主屏与光标）。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = stdout.flush();
        previous(info);
    }));
}

/// 进入 TUI（真实 IPC；**惰性连接**：daemon 未运行也能进，`e` 可重连）。
///
/// # Errors
///
/// 终端初始化/事件循环失败返回 [`TuiError`]。
pub fn run(options: TuiOptions) -> Result<(), TuiError> {
    run_with_client(
        IpcControl::connect(&options.socket_path),
        &options.media_dir,
    )
}

/// 用给定客户端跑事件循环（测试与 `--mock` 路径使用）。
///
/// # Errors
///
/// 终端初始化或 I/O 失败返回 [`TuiError`]。
pub fn run_with_client<C: Session>(client: C, media_dir: &Path) -> Result<(), TuiError> {
    install_panic_hook();
    let mut guard = TerminalGuard::enter()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(client);
    app.state.media_dir_hint = media_dir.display().to_string();
    app.refresh_all();
    let result = event_loop(&mut terminal, &mut app);
    guard.restore();
    result
}

/// 事件驱动事件循环（只在 `needs_render` 时重绘）。
fn event_loop<B: Backend, C: Session>(
    terminal: &mut Terminal<B>,
    app: &mut App<C>,
) -> Result<(), TuiError> {
    loop {
        if app.state.quit {
            return Ok(());
        }
        if app.needs_render() {
            terminal.draw(|f| app.draw(f))?;
        }
        let now = Instant::now();
        let timeout = app.poll_timeout(now);
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) => app.on_key(k),
                Event::Resize(w, h) => app.on_resize(w, h),
                _ => {}
            }
        }
        app.tick(Instant::now());
    }
}

/// 便捷：纯 Mock 的 TUI（不接触 daemon）。
///
/// # Errors
///
/// 终端初始化失败返回 [`TuiError`]。
pub fn run_mock(media_dir: &Path) -> Result<(), TuiError> {
    run_with_client(MockControl::new(), media_dir)
}

impl<C: Session> App<C> {
    /// 当前页。
    pub fn page(&self) -> Page {
        self.state.page
    }

    /// 是否请求退出。
    pub fn should_quit(&self) -> bool {
        self.state.quit
    }
}
