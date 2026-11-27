//! 事件层：按键 → [`Action`] → 状态变更 → [`Effect`]（交给 IPC 执行）。
//!
//! **不碰终端、不碰 IPC**：映射与状态变更是纯函数，因此键位表、模态框捕获焦点、
//! `Apply`/`Save` 区分、受理/完成分离等都能直接单测。

use crate::model::{
    AppState, ApplyPhase, ApplyTarget, CommitKind, Focus, Modal, NotificationKind, Page,
    PendingState, SubPage, TextTarget, TransitionDraft,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 编辑操作（Unicode 安全：以 `char` 为单位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOp {
    /// 插入字符。
    Insert(char),
    /// 退格。
    Backspace,
    /// 删除。
    Delete,
    /// 光标左移。
    Left,
    /// 光标右移。
    Right,
    /// 行首。
    Home,
    /// 行尾。
    End,
}

/// 一次输入意图。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 退出 TUI（**不停 daemon**）。
    Quit,
    /// 切页。
    Goto(Page),
    /// 焦点移动。
    FocusMove(bool),
    /// 选中移动。
    Move(i32),
    /// 翻页。
    PageMove(i32),
    /// 小档子页切换。
    SubPageMove(bool),
    /// 确认/进入。
    Confirm,
    /// 取消/返回。
    Cancel,
    /// 编辑（搜索框或路径框）。
    Edit(EditOp),
    /// 开始搜索。
    StartSearch,
    /// 输入服务器绝对路径。
    InputPath,
    /// 来源管理。
    SourceManage,
    /// 播放列表：新建（进入编辑缓冲，不发请求）。
    PlaylistNew,
    /// 播放列表：编辑当前选中项。
    PlaylistEdit,
    /// 播放列表：顺序/随机切换。
    PlaylistCycleMode,
    /// 播放列表：间隔 ±（秒）。
    PlaylistInterval(i32),
    /// 播放列表：视频播完行为切换。
    PlaylistToggleVideoEnd,
    /// 播放列表：禁止连续重复开关。
    PlaylistToggleNoRepeat,
    /// 播放列表：输出绑定轮换。
    PlaylistCycleOutput,
    /// 播放列表：把当前选中的媒体项加入。
    PlaylistAddEntry,
    /// 播放列表：移除光标处条目。
    PlaylistRemoveEntry,
    /// 播放列表：编辑缓冲内条目光标移动。
    PlaylistEntryMove(i32),
    /// 播放列表：放弃编辑（不下发、不落盘）。
    PlaylistDiscard,
    /// 播放列表：`←/→` 切换**播放模式**（单视频循环 / 顺序循环 / 随机播放）。
    ///
    /// 载荷 = 方向（`true` = 向右）。三种模式各自一次性写好 `mode` + `on_video_end`
    /// 两个协议字段（见 `model::PlayMode::fields`）。
    PlaylistMode(bool),
    /// 播放列表/媒体库：`a` 把媒体库选中项加入**当前播放列表**。
    PlaylistAddToCurrent,
    /// 播放列表：`Enter` 播放光标处条目（编辑态）或列表第一条（非编辑态）。
    PlaylistPlayEntry,
    /// 静态壁纸页：`a` 把媒体库选中的**图片**设为静态壁纸。
    StaticSet,
    /// 静态壁纸页：`c` 清除静态壁纸并恢复播放列表。
    StaticClear,
    /// 转场页：试应用当前草稿（**只改运行时**；未实现的效果拒绝下发）。
    ApplyTransition,
    /// 重扫来源（当前高亮来源；`None` = 全部）。
    Rescan,
    /// 切换筛选。
    FilterCycle,
    /// 切换排序。
    SortCycle,
    /// **立即应用**（运行时）。
    Apply,
    /// **持久保存**（落盘）。
    Save,
    /// 应用壁纸（选中项 → 目标输出）。
    ApplyWallpaper,
    /// 切换应用目标（默认/指定输出/全部显示器）。
    CycleTarget,
    /// 复制完整路径。
    CopyPath,
    /// 切换 fit（Cover/Contain/Stretch/Center）。
    CycleFit(bool),
    /// 调整 zoom。
    Zoom(i32),
    /// 调整偏移。
    Offset(i32, i32),
    /// 循环旋转角。
    CycleRotation,
    /// 调整画面亮度（±5%）。
    Brightness(i32),
    /// 重置输出适配与亮度为默认值。
    ResetTransform,
    /// 下一张。
    NextMedia,
    /// 暂停/继续。
    TogglePause,
    /// 播放列表重命名（真正的文本输入）。
    PlaylistRename,
    /// 来源管理：新增来源（弹文本输入）。
    SourceAdd,
    /// 来源管理：删除当前高亮来源。
    SourceRemove,
    /// 来源管理：切换"递归扫描"。
    SourceToggleRecursive,
    /// 来源管理：切换新来源类型（image/video/all）。
    SourceCycleKind,
    /// 来源管理：**显式保存**（先确认再落盘）。
    SourceSave,
    /// 调整转场时长。
    AdjustDuration(i32),
    /// 切换效果/缓动。
    CycleEffect(bool),
    /// 切换缓动。
    CycleEasing(bool),
    /// 帮助。
    Help,
    /// 重连（断线恢复）。
    Reconnect,
    /// 指标刷新（≤2 Hz）。
    MetricsTick,
    /// 搜索防抖到期。
    SearchDue,
    /// 应用确认超时检查。
    ApplyTimeoutCheck,
    /// 尺寸变化。
    Resized,
}

/// [`Effect::PlaylistPatch`] 的来路（决定成功反馈文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchKind {
    /// 模式选择器：载荷是模式名。
    Mode(&'static str),
    /// 媒体库页 `a`：播放列表名字与**预期**条数（失败时 daemon 会拒绝，不谎报）。
    AddEntry {
        /// 播放列表名字。
        name: String,
        /// 加入后的条数。
        count: usize,
    },
}

/// IPC 副作用（由 `app.rs` 执行；事件层只描述"要做什么"）。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 退出事件循环。
    Quit,
    /// 全量刷新。
    RefreshAll,
    /// 重新连接（断线恢复）。
    Reconnect,
    /// 按当前查询重新拉取媒体。
    LoadMedia,
    /// 取选中项的完整元数据。
    LoadDetail(String),
    /// 拉取指标。
    LoadMetrics,
    /// 触发一次后台扫描。
    ScanSource(Option<String>),
    /// 设为壁纸（**服务器绝对路径**；`outputs` 为空 = 默认输出）。
    SetWallpaper {
        /// 路径。
        path: String,
        /// 目标输出列表（显式；空 = 默认）。
        outputs: Vec<Option<String>>,
    },
    /// 下一张。
    NextMedia(Option<String>),
    /// 暂停。
    Pause(Option<String>),
    /// 继续。
    Resume(Option<String>),
    /// 重载配置。
    Reload,
    /// 应用转场参数。
    SetTransition(TransitionParamsOwned),
    /// 更新/新建播放列表（**只改运行时**；落盘走 `SaveConfig`）。
    UpdatePlaylist(liuguang_ipc::protocol::UpdatePlaylistParams),
    /// **就地**更新播放列表（模式切换 / 媒体库页 `a` 加入条目）。
    ///
    /// 与 [`Self::UpdatePlaylist`] 的区别：**不退出编辑态、不清缓冲**，
    /// 且成功反馈按来路给出（模式名 / "已加入「名字」（N 条）"）。
    PlaylistPatch {
        /// 完整参数（`update_playlist` 是整体替换，必须带全字段）。
        params: liuguang_ipc::protocol::UpdatePlaylistParams,
        /// 成功后的反馈来路。
        kind: PatchKind,
    },
    /// 设为静态壁纸（**真实 IPC**：`set_static_wallpaper`）。
    ///
    /// daemon 若尚未实现该方法会回 `UNSUPPORTED_METHOD` ⇒ 由 `app.rs` 转成
    /// **可操作提示**（绝不当成成功）。
    SetStaticWallpaper {
        /// 服务器绝对路径（已通过 [`crate::model::AppState::static_set_guard`] 校验）。
        path: String,
    },
    /// 清除静态壁纸并恢复播放（**真实 IPC**：`clear_static_wallpaper`）。
    ClearStaticWallpaper,
    /// 立即应用输出变换（运行时）。
    ApplyTransform(String),
    /// 持久保存配置。
    SaveConfig,
    /// 新增来源（daemon 校验路径在允许根内）。
    AddSource {
        /// 类型（image/video/all）。
        kind: String,
        /// 服务器绝对路径。
        path: String,
        /// 是否递归。
        recursive: bool,
    },
    /// 删除来源。
    RemoveSource {
        /// 来源 id。
        source_id: String,
    },
    /// 复制路径到剪贴板（OSC52，尽力而为）。
    CopyPath(String),
}

/// 转场参数副本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionParamsOwned {
    /// 效果名。
    pub effect: String,
    /// 时长（毫秒）。
    pub duration_ms: u32,
    /// 缓动。
    pub easing: Option<String>,
    /// 方向。
    pub direction: Option<String>,
}

impl From<&TransitionDraft> for TransitionParamsOwned {
    fn from(d: &TransitionDraft) -> Self {
        Self {
            effect: d.effect.clone(),
            duration_ms: d.duration_ms,
            easing: Some(d.easing.clone()),
            direction: d.direction.clone(),
        }
    }
}

/// 按键 → 意图（`docs/TUI_DESIGN.md` §6；优先级：模态框 > 搜索框 > 全局）。
pub fn action_for(state: &AppState, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // 0) 全局退出键（含 Ctrl+C）。
    if (ctrl && key.code == KeyCode::Char('c')) || (!ctrl && key.code == KeyCode::Char('Q')) {
        return Some(Action::Quit);
    }

    // 1) 模态框捕获焦点。
    if let Some(modal) = state.modal.clone() {
        return match modal {
            Modal::PathInput { .. } => match key.code {
                KeyCode::Esc => Some(Action::Cancel),
                KeyCode::Enter => Some(Action::Confirm),
                KeyCode::Backspace => Some(Action::Edit(EditOp::Backspace)),
                KeyCode::Delete => Some(Action::Edit(EditOp::Delete)),
                KeyCode::Left => Some(Action::Edit(EditOp::Left)),
                KeyCode::Right => Some(Action::Edit(EditOp::Right)),
                KeyCode::Home => Some(Action::Edit(EditOp::Home)),
                KeyCode::End => Some(Action::Edit(EditOp::End)),
                KeyCode::Char(c) if !ctrl => Some(Action::Edit(EditOp::Insert(c))),
                _ => None,
            },
            // 文本输入（重命名/新来源路径）：与路径输入共用同一套 Unicode 安全编辑器
            Modal::TextInput { .. } => match key.code {
                KeyCode::Esc => Some(Action::Cancel),
                KeyCode::Enter => Some(Action::Confirm),
                KeyCode::Backspace => Some(Action::Edit(EditOp::Backspace)),
                KeyCode::Delete => Some(Action::Edit(EditOp::Delete)),
                KeyCode::Left => Some(Action::Edit(EditOp::Left)),
                KeyCode::Right => Some(Action::Edit(EditOp::Right)),
                KeyCode::Home => Some(Action::Edit(EditOp::Home)),
                KeyCode::End => Some(Action::Edit(EditOp::End)),
                KeyCode::Char(c) if !ctrl => Some(Action::Edit(EditOp::Insert(c))),
                _ => None,
            },
            // 来源管理：`a` 新增（弹文本输入）、`d` 删除、`r` 递归、`k` 类型、`s` 保存（需确认）
            Modal::SourceManage => match key.code {
                KeyCode::Esc | KeyCode::Enter => Some(Action::Cancel),
                KeyCode::Up => Some(Action::Move(-1)),
                KeyCode::Down => Some(Action::Move(1)),
                KeyCode::Char('a') => Some(Action::SourceAdd),
                KeyCode::Char('d') => Some(Action::SourceRemove),
                KeyCode::Char('r') => Some(Action::SourceToggleRecursive),
                KeyCode::Char('k') => Some(Action::SourceCycleKind),
                KeyCode::Char('s') => Some(Action::SourceSave),
                KeyCode::Char('R') => Some(Action::Rescan),
                _ => None,
            },
            Modal::Confirm { .. } => match key.code {
                KeyCode::Esc => Some(Action::Cancel),
                KeyCode::Enter => Some(Action::Confirm),
                _ => None,
            },
            Modal::Help | Modal::PathText { .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter => Some(Action::Cancel),
                _ => None,
            },
        };
    }

    // 1.4) t24：转场页 `A` = **试应用转场**（只改运行时；未实现的效果不下发）。
    if state.page == Page::Effects && !ctrl && key.code == KeyCode::Char('A') {
        return Some(Action::ApplyTransition);
    }
    // 1.5) t24：播放列表页的编辑键（**仅该页**；模态框已在上面优先处理，故不冲突）。
    if state.page == Page::Static && !ctrl {
        match key.code {
            KeyCode::Char('a') => return Some(Action::StaticSet),
            KeyCode::Char('c') => return Some(Action::StaticClear),
            _ => {}
        }
    }
    if state.page == Page::Playlists && !ctrl {
        // t67：**主界面核心操作** = 选择 / ←→ 切模式 / a 加入 / Enter 播放；
        // 其余（m v r o + - x A S）是"高级操作"，仍在 keymap 里可用，
        // 但只由 `?` 的高级面板列出（`page_help`）。
        match key.code {
            // ←/→ = 切播放模式（本页不再走"子页切换"；Tab 仍可切焦点）
            KeyCode::Left => return Some(Action::PlaylistMode(false)),
            KeyCode::Right => return Some(Action::PlaylistMode(true)),
            KeyCode::Enter => return Some(Action::PlaylistPlayEntry),
            KeyCode::Char('a') => return Some(Action::PlaylistAddToCurrent),
            // Esc 与 `x` 等价：放弃编辑（不能让编辑态成为"关不掉"的陷阱）
            KeyCode::Esc if state.playlist_edit.is_some() => {
                return Some(Action::PlaylistDiscard);
            }
            _ => {}
        }
        if state.playlist_edit.is_some() {
            match key.code {
                KeyCode::Char('m') => return Some(Action::PlaylistCycleMode),
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    return Some(Action::PlaylistInterval(60));
                }
                KeyCode::Char('-') => return Some(Action::PlaylistInterval(-60)),
                KeyCode::Char('v') => return Some(Action::PlaylistToggleVideoEnd),
                KeyCode::Char('r') => return Some(Action::PlaylistToggleNoRepeat),
                KeyCode::Char('o') => return Some(Action::PlaylistCycleOutput),
                KeyCode::Char('p') => return Some(Action::PlaylistAddEntry),
                KeyCode::Char('d') => return Some(Action::PlaylistRemoveEntry),
                KeyCode::Char('k') => return Some(Action::PlaylistEntryMove(-1)),
                KeyCode::Char('j') => return Some(Action::PlaylistEntryMove(1)),
                KeyCode::Char('x') => return Some(Action::PlaylistDiscard),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('n') => return Some(Action::PlaylistNew),
                KeyCode::Char('N') => return Some(Action::PlaylistRename),
                KeyCode::Char('e') => return Some(Action::PlaylistEdit),
                _ => {}
            }
        }
    }

    // 1.6) t67：媒体库页 `a` = 把选中项加入**当前播放列表**（与播放列表页同一个动作）。
    if state.page == Page::Library && !ctrl && key.code == KeyCode::Char('a') {
        return Some(Action::PlaylistAddToCurrent);
    }

    // 2) 搜索框聚焦。
    if state.search_focus {
        return match key.code {
            KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Backspace => Some(Action::Edit(EditOp::Backspace)),
            KeyCode::Delete => Some(Action::Edit(EditOp::Delete)),
            KeyCode::Up => Some(Action::Move(-1)),
            KeyCode::Down => Some(Action::Move(1)),
            KeyCode::Char(c) if !ctrl => Some(Action::Edit(EditOp::Insert(c))),
            _ => None,
        };
    }

    // 3) 全局与页面键。
    match key.code {
        KeyCode::Tab => Some(Action::FocusMove(
            !key.modifiers.contains(KeyModifiers::SHIFT),
        )),
        KeyCode::BackTab => Some(Action::FocusMove(false)),
        KeyCode::Up => Some(Action::Move(-1)),
        KeyCode::Down => Some(Action::Move(1)),
        KeyCode::Char('k') => Some(Action::Move(-1)),
        KeyCode::Char('j') => Some(Action::Move(1)),
        KeyCode::PageUp => Some(Action::PageMove(-1)),
        KeyCode::PageDown => Some(Action::PageMove(1)),
        KeyCode::Left => Some(Action::SubPageMove(false)),
        KeyCode::Right => Some(Action::SubPageMove(true)),
        KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Char('?') => Some(Action::Help),
        KeyCode::Char('/') => Some(Action::StartSearch),
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Char(c) => match c {
            '1'..='7' => Page::from_hotkey(c).map(Action::Goto),
            'I' => Some(Action::InputPath),
            'S' => Some(match state.page {
                Page::Library => Action::SourceManage,
                _ => Action::Save,
            }),
            'A' => Some(Action::Apply),
            'W' | 'w' => Some(Action::ApplyWallpaper),
            'T' => Some(Action::CycleTarget),
            'Y' => Some(Action::CopyPath),
            'R' => Some(Action::Rescan),
            'N' | 'n' => Some(Action::NextMedia),
            ' ' | 'p' => Some(Action::TogglePause),
            'F' | 'f' => Some(Action::FilterCycle),
            'O' => Some(Action::SortCycle),
            'C' => Some(Action::CycleFit(true)),
            'V' => Some(Action::CycleFit(false)),
            '+' | '=' => Some(Action::Zoom(1)),
            '-' => Some(Action::Zoom(-1)),
            'h' => Some(Action::Offset(-1, 0)),
            'l' => Some(Action::Offset(1, 0)),
            'K' => Some(Action::Offset(0, -1)),
            'J' => Some(Action::Offset(0, 1)),
            'X' => Some(Action::CycleRotation),
            'b' => Some(Action::Brightness(-1)),
            'B' => Some(Action::Brightness(1)),
            '0' => Some(Action::ResetTransform),
            '<' | ',' => Some(Action::AdjustDuration(-1)),
            '>' | '.' => Some(Action::AdjustDuration(1)),
            '[' => Some(Action::CycleEffect(false)),
            ']' => Some(Action::CycleEffect(true)),
            '{' => Some(Action::CycleEasing(false)),
            '}' => Some(Action::CycleEasing(true)),
            'e' => Some(Action::Reconnect),
            _ => None,
        },
        _ => None,
    }
}

/// 应用一个意图：修改状态并返回需要执行的 IPC 副作用（纯逻辑）。
pub fn apply(state: &mut AppState, action: Action) -> Vec<Effect> {
    let mut effects = Vec::new();
    match action {
        Action::Quit => {
            state.quit = true;
            effects.push(Effect::Quit);
        }
        Action::Goto(page) => {
            let same = state.page == page;
            state.goto(page);
            if same {
                state.focus = state.focus.next(true);
            }
            if page == Page::Library {
                effects.push(Effect::LoadMedia);
            }
        }
        Action::FocusMove(forward) => {
            state.focus = state.focus.next(forward);
        }
        Action::Move(delta) => match state.page {
            Page::Library => {
                let before = state.media_cursor;
                state.move_media_cursor(delta);
                if state.media_cursor != before
                    && let Some(item) = state.selected_media()
                {
                    effects.push(Effect::LoadDetail(item.id.clone()));
                }
            }
            Page::Outputs => state.move_outputs_cursor(delta),
            Page::Playlists => state.move_playlists_cursor(delta),
            Page::Effects => {}
            _ => {}
        },
        Action::PageMove(delta) => {
            if state.page == Page::Library {
                let next = (state.media.page as i32 + delta).max(0) as usize;
                let clamped =
                    crate::model::clamp_page(next, state.media.total, state.media.page_size);
                if clamped != state.media.page {
                    state.media.page = clamped;
                    state.media_cursor = 0;
                    effects.push(Effect::LoadMedia);
                }
            }
        }
        Action::SubPageMove(forward) => {
            state.subpage = state.subpage.shift(forward);
            state.focus = match state.subpage {
                SubPage::List => Focus::List,
                SubPage::Detail => Focus::Detail,
                SubPage::Actions => Focus::Actions,
            };
        }
        Action::Confirm => match state.modal.clone() {
            Some(Modal::PathInput { .. }) => {
                let path = state.editor.text();
                state.close_modal();
                let trimmed = path.trim().to_string();
                if trimmed.is_empty() {
                    state.notify(NotificationKind::Warn, "路径为空：未提交任何请求");
                } else {
                    // 只做**形状**提示；白名单/规范化判定在 daemon（规范 §8）
                    if !is_absolute_server_path(&trimmed) {
                        state.notify(
                            NotificationKind::Warn,
                            format!("不是服务器绝对路径，仍交给 daemon 判定：{trimmed}"),
                        );
                    }
                    let outputs = targets(state).into_iter().collect::<Vec<Option<String>>>();
                    effects.push(Effect::SetWallpaper {
                        path: trimmed,
                        outputs,
                    });
                }
            }
            Some(Modal::TextInput { target, .. }) => {
                let text = state.editor.text().trim().to_string();
                state.close_modal();
                match target {
                    TextTarget::PlaylistName => {
                        if text.is_empty() {
                            state.notify(NotificationKind::Warn, "名称不能为空：未改动");
                        } else if let Some(edit) = state.playlist_edit.as_mut() {
                            edit.name = text.clone();
                            state.dirty = true;
                            state.notify(
                                NotificationKind::Info,
                                format!("名称已改为「{text}」——按 `A` 试用 / `S` 保存"),
                            );
                        }
                    }
                    TextTarget::SourceNewPath => {
                        if text.is_empty() {
                            state.notify(NotificationKind::Warn, "路径为空：未提交");
                        } else {
                            if !is_absolute_server_path(&text) {
                                state.notify(
                                    NotificationKind::Warn,
                                    format!("不是服务器绝对路径，仍交给 daemon 判定：{text}"),
                                );
                            }
                            let kind = state.source_kind.clone();
                            let recursive = state.source_recursive;
                            effects.push(Effect::AddSource {
                                kind,
                                path: text,
                                recursive,
                            });
                        }
                    }
                }
            }
            Some(Modal::SourceManage) => {
                // 点 Enter/Esc 只关窗：**来源改动属运行时**，持久化必须显式 `s` + 确认
                state.close_modal();
                if state.sources_dirty {
                    state.notify(
                        NotificationKind::Warn,
                        "来源改动只在运行时生效；按 `S`（媒体库页）重新打开并 `s` 显式保存",
                    );
                }
            }
            Some(Modal::Confirm { action, .. }) => {
                state.close_modal();
                if action == "source_save" {
                    state.begin_op("save", "保存配置（含来源改动）");
                    effects.push(Effect::SaveConfig);
                }
            }
            Some(_) => state.close_modal(),
            None => {
                if state.search_focus {
                    state.search_focus = false;
                    effects.push(Effect::LoadMedia);
                } else if state.page == Page::Library {
                    state.focus = Focus::Detail;
                    if let Some(item) = state.selected_media() {
                        effects.push(Effect::LoadDetail(item.id.clone()));
                    }
                }
            }
        },
        Action::Cancel => {
            if state.modal_open() {
                // t38：`Esc` 必须**彻底关闭**模态——此前"取消保存确认后回到来源管理"
                // 会让用户觉得 Esc 关不掉弹窗（被困住）。契约优先于流程便利。
                state.close_modal();
            } else if state.search_focus {
                state.search_focus = false;
            } else if state.page == Page::Library && state.focus == Focus::Detail {
                state.focus = Focus::List;
            }
        }
        Action::Edit(op) => {
            if state.search_focus {
                apply_search_edit(state, op);
                if matches!(op, EditOp::Insert(_) | EditOp::Backspace | EditOp::Delete) {
                    state.mark_search_dirty(std::time::Instant::now());
                }
            } else if state.modal_open() {
                apply_editor(state, op);
            }
        }
        Action::StartSearch => {
            state.search_focus = true;
            state.focus = Focus::List;
        }
        Action::InputPath => {
            if state.page == Page::Library {
                state.open_modal(Modal::PathInput {
                    purpose: "设为壁纸（服务器绝对路径）".to_string(),
                });
            } else {
                state.notify(NotificationKind::Info, "路径输入在 `媒体库` 页（按 `2`）");
            }
        }
        Action::SourceManage => {
            if state.page == Page::Library {
                state.open_modal(Modal::SourceManage);
            }
        }
        Action::Rescan => {
            let source_id = state
                .sources
                .sources
                .get(state.sources_cursor)
                .map(|s| s.id.clone());
            state.scan = crate::model::ScanState::Requested { scan_id: 0 };
            effects.push(Effect::ScanSource(source_id));
        }
        Action::FilterCycle => {
            state.filter = state.filter.next();
            state.media.page = 0;
            state.media_cursor = 0;
            effects.push(Effect::LoadMedia);
        }
        Action::SortCycle => {
            state.sort = state.sort.next();
            state.media.page = 0;
            state.media_cursor = 0;
            effects.push(Effect::LoadMedia);
        }
        Action::ApplyTransition => {
            let name = state.transition.effect.clone();
            if !crate::model::effect_is_available(&name) {
                // 规范 §11：未实现的能力不得假装。**拒绝下发**，并说明原因与可选值。
                state.notify(
                    NotificationKind::Warn,
                    format!(
                        "`{name}` 尚未实现（daemon 会把它回退成 fade）：请选择 {} 再应用",
                        crate::model::EFFECTS_IMPLEMENTED.join("/")
                    ),
                );
            } else {
                state.begin_op("apply", format!("应用转场 {name}"));
                state.dirty = true;
                effects.push(Effect::SetTransition(TransitionParamsOwned {
                    effect: name,
                    duration_ms: state.transition.duration_ms,
                    easing: Some(state.transition.easing.clone()),
                    direction: state.transition.direction.clone(),
                }));
            }
        }
        Action::PlaylistNew => {
            state.playlist_begin_new();
        }
        Action::PlaylistEdit => state.playlist_begin_edit(),
        Action::PlaylistCycleMode => state.playlist_cycle_mode(),
        Action::PlaylistInterval(delta) => state.playlist_interval_delta(delta),
        Action::PlaylistToggleVideoEnd => state.playlist_toggle_video_end(),
        Action::PlaylistToggleNoRepeat => state.playlist_toggle_no_repeat(),
        Action::PlaylistCycleOutput => state.playlist_cycle_output(),
        Action::PlaylistAddEntry => {
            if !state.playlist_add_selected() {
                state.notify(
                    crate::model::NotificationKind::Warn,
                    "先在媒体库里选中一项，再按 p 加入播放列表",
                );
            }
        }
        Action::PlaylistRemoveEntry => state.playlist_remove_entry(),
        Action::PlaylistEntryMove(delta) => state.playlist_move_entry_cursor(delta),
        Action::PlaylistDiscard => state.playlist_discard(),
        // ---- t67：播放模式选择器 / 加入 / 播放 ----
        Action::PlaylistMode(forward) => match state.playlist_mode() {
            None => state.notify(
                NotificationKind::Warn,
                "还没有播放列表：先按 `n` 新建（编辑完按 `A` 应用），再用 ←/→ 切模式",
            ),
            Some(current) => {
                let next = state.playlist_shift_mode(forward).unwrap_or(current);
                if state.playlist_edit.is_some() {
                    // 编辑态：只改缓冲（不得偷偷下发未确认的改动）
                    state.notify(
                        NotificationKind::Info,
                        format!(
                            "播放模式：{}（编辑缓冲；按高级面板里的 `A` 应用）",
                            next.label()
                        ),
                    );
                } else if let Some(params) = state.playlist_patch_for_mode(next) {
                    // 非编辑态：**立即**下发整条列表（两个字段一起写）
                    effects.push(Effect::PlaylistPatch {
                        params,
                        kind: PatchKind::Mode(next.label()),
                    });
                }
            }
        },
        Action::PlaylistAddToCurrent => {
            if state.playlist_edit.is_some() {
                // 编辑态：加进缓冲（`A` 才下发）
                if state.playlist_add_selected() {
                    let n = state
                        .playlist_edit
                        .as_ref()
                        .map(|e| e.entries.len())
                        .unwrap_or(0);
                    state.notify(
                        NotificationKind::Info,
                        format!("已加入编辑缓冲（{n} 条）——按 `A` 应用"),
                    );
                } else {
                    state.notify(
                        NotificationKind::Warn,
                        "媒体库还没有选中条目：先到【媒体库】页用 ↑↓ 选中一项",
                    );
                }
            } else {
                match state.playlist_append_selected() {
                    Ok((name, count, params)) => effects.push(Effect::PlaylistPatch {
                        params,
                        kind: PatchKind::AddEntry { name, count },
                    }),
                    Err(msg) => state.notify(NotificationKind::Warn, msg),
                }
            }
        }
        Action::StaticSet => match state.static_set_guard() {
            Ok(path) => effects.push(Effect::SetStaticWallpaper { path }),
            // 拒绝理由必须**可操作**（去媒体库选图片 / 等扫描），且不下发任何东西
            Err(msg) => state.notify(NotificationKind::Warn, msg),
        },
        Action::StaticClear => effects.push(Effect::ClearStaticWallpaper),
        Action::PlaylistPlayEntry => {
            let entry = if let Some(e) = state.playlist_edit.as_ref() {
                e.entries.get(e.cursor).cloned()
            } else {
                state
                    .playlists
                    .get(state.playlists_cursor)
                    .and_then(|p| p.entries.first().map(|e| e.media_id.clone()))
            };
            match entry {
                Some(path) => {
                    let outputs = targets(state).into_iter().collect::<Vec<Option<String>>>();
                    state.notify(
                        NotificationKind::Info,
                        format!(
                            "播放：{}",
                            crate::ui::widgets::text::truncate_display(&path, 48)
                        ),
                    );
                    effects.push(Effect::SetWallpaper { path, outputs });
                }
                None => state.notify(
                    NotificationKind::Warn,
                    "这个播放列表还没有条目：到【媒体库】页选中一项后按 `a` 加入",
                ),
            }
        }
        Action::Apply => {
            // t24：正在编辑播放列表时，`A` 下发 `update_playlist`（只改运行时）。
            if let Some(edit) = state.playlist_edit.clone() {
                state.dirty = true;
                effects.push(Effect::UpdatePlaylist(edit.to_params()));
                return effects;
            }
            let Some(name) = state.selected_output.clone() else {
                state.notify(
                    NotificationKind::Warn,
                    "先选目标输出（显示器页按 `4` / `↑↓`）",
                );
                return effects;
            };
            state.transform.output = name.clone();
            state.begin_op("apply", format!("应用变换到 {name}"));
            state.dirty = true;
            effects.push(Effect::ApplyTransform(name));
        }
        Action::Save => {
            state.begin_op("save", "保存配置");
            effects.push(Effect::SaveConfig);
        }
        Action::ApplyWallpaper => {
            let path = state
                .selected_media()
                .map(|m| m.id.clone())
                .or_else(|| state.detail.as_ref().map(|d| d.id.clone()));
            match path {
                Some(path) => {
                    if state.outputs.outputs.is_empty() {
                        state.notify(NotificationKind::Warn, "未检测到显示器：无法应用");
                    } else {
                        let outputs = targets(state).into_iter().collect::<Vec<_>>();
                        effects.push(Effect::SetWallpaper { path, outputs });
                    }
                }
                None => state.notify(NotificationKind::Warn, "没有选中的媒体"),
            }
        }
        Action::CycleTarget => {
            let names: Vec<String> = state
                .outputs
                .outputs
                .iter()
                .map(|o| o.name.clone())
                .collect();
            state.apply_target = match &state.apply_target {
                ApplyTarget::Default => names
                    .first()
                    .map(|n| ApplyTarget::Output(n.clone()))
                    .unwrap_or(ApplyTarget::Default),
                ApplyTarget::Output(current) => {
                    let idx = names.iter().position(|n| n == current).unwrap_or(0);
                    match names.get(idx + 1) {
                        Some(next) => ApplyTarget::Output(next.clone()),
                        None => ApplyTarget::AllOutputs,
                    }
                }
                ApplyTarget::AllOutputs => ApplyTarget::Default,
            };
            state.notify(
                NotificationKind::Info,
                format!("应用目标：{}", state.apply_target.label()),
            );
        }
        Action::CopyPath => {
            let path = state
                .selected_media()
                .map(|m| m.id.clone())
                .or_else(|| state.detail.as_ref().map(|d| d.id.clone()));
            match path {
                Some(p) => effects.push(Effect::CopyPath(p)),
                None => state.notify(NotificationKind::Warn, "没有选中的媒体"),
            }
        }
        Action::CycleFit(forward) => {
            state.transform.cycle_fit(forward);
            state.dirty = true;
        }
        Action::Zoom(dir) => {
            state
                .transform
                .adjust_zoom(if dir >= 0 { 0.1 } else { -0.1 });
            state.dirty = true;
        }
        Action::Offset(dx, dy) => {
            state.transform.adjust_offset_x(dx as f32 * 10.0);
            state.transform.adjust_offset_y(dy as f32 * 10.0);
            state.dirty = true;
        }
        Action::CycleRotation => {
            state.transform.cycle_rotation();
            state.dirty = true;
        }
        Action::Brightness(dir) => {
            state
                .transform
                .adjust_brightness(if dir >= 0 { 0.05 } else { -0.05 });
            state.dirty = true;
            state.notify(
                NotificationKind::Info,
                format!(
                    "亮度草稿：{}%（按 A 立即应用）",
                    state.transform.brightness_percent()
                ),
            );
        }
        Action::ResetTransform => {
            state.transform.reset_defaults();
            state.dirty = true;
            state.notify(
                NotificationKind::Info,
                "已重置适配与亮度为默认值（按 A 立即应用）",
            );
        }
        Action::NextMedia => {
            let output = state.selected_output.clone();
            effects.push(Effect::NextMedia(output));
        }
        Action::TogglePause => {
            let output = state.selected_output.clone();
            if state.status.paused {
                effects.push(Effect::Resume(output));
            } else {
                effects.push(Effect::Pause(output));
            }
        }
        Action::PlaylistRename => match state.playlist_edit.as_ref().map(|e| e.id.clone()) {
            None => state.notify(
                NotificationKind::Warn,
                "先新建或选中一个播放列表（`n`/`e`）",
            ),
            Some(_) => {
                let current = state
                    .playlist_edit
                    .as_ref()
                    .map(|e| e.name.clone())
                    .unwrap_or_default();
                state.open_modal(Modal::TextInput {
                    title: "重命名播放列表".to_string(),
                    target: TextTarget::PlaylistName,
                    hint: "Enter 提交 · Esc 取消（不写配置）".to_string(),
                });
                state.editor.set_text(&current);
            }
        },
        Action::SourceAdd => {
            state.open_modal(Modal::TextInput {
                title: format!("新增来源（{}）", state.source_kind),
                target: TextTarget::SourceNewPath,
                hint: "服务器绝对路径，由 daemon 校验是否在允许根内".to_string(),
            });
        }
        Action::SourceRemove => match state.sources.sources.get(state.sources_cursor) {
            Some(src) => {
                let (id, name) = (src.id.clone(), src.name.clone());
                effects.push(Effect::RemoveSource { source_id: id });
                state.notify(
                    NotificationKind::Info,
                    format!("已请求删除来源「{name}」（运行时；需显式保存才落盘）"),
                );
            }
            None => state.notify(NotificationKind::Warn, "没有可删除的来源"),
        },
        Action::SourceToggleRecursive => {
            state.source_recursive = !state.source_recursive;
            state.notify(
                NotificationKind::Info,
                format!(
                    "新增来源将{}递归（daemon 侧深度上限 8）",
                    if state.source_recursive { "" } else { "不" }
                ),
            );
        }
        Action::SourceCycleKind => {
            const KINDS: [&str; 3] = ["image", "video", "all"];
            let i = KINDS
                .iter()
                .position(|k| *k == state.source_kind)
                .unwrap_or(0);
            state.source_kind = KINDS[(i + 1) % KINDS.len()].to_string();
            state.notify(
                NotificationKind::Info,
                format!("新来源类型：{}", state.source_kind),
            );
        }
        Action::SourceSave => {
            if !state.sources_dirty {
                state.notify(NotificationKind::Info, "没有待保存的来源改动");
            } else {
                // **持久化必须显式确认**：先弹确认，再走 save_config
                state.open_modal(Modal::Confirm {
                    question: match state.config_path.as_deref() {
                        Some(path) => format!(
                            "把来源改动写入 {path}？（需要写权限；路径来自 daemon 回传）"
                        ),
                        None => "把来源改动写入**配置文件**？（路径将在本次保存后由 daemon 回传；需要写权限）"
                            .to_string(),
                    }
                        .to_string(),
                    action: "source_save".to_string(),
                });
            }
        }
        Action::AdjustDuration(delta) => {
            state.transition.adjust_duration(delta);
            state.dirty = true;
        }
        Action::CycleEffect(forward) => {
            state.transition.cycle_effect(forward);
            state.dirty = true;
        }
        Action::CycleEasing(forward) => {
            state.transition.cycle_easing(forward);
            state.dirty = true;
        }
        Action::Help => state.open_modal(Modal::Help),
        Action::Reconnect => {
            state.connection = crate::model::Connection::Reconnecting;
            state.reconnect_attempts += 1;
            effects.push(Effect::Reconnect);
        }
        Action::MetricsTick => effects.push(Effect::LoadMetrics),
        Action::SearchDue => {
            state.search_dirty_at = None;
            effects.push(Effect::LoadMedia);
        }
        Action::ApplyTimeoutCheck => {
            if let Some(ApplyPhase::Accepted | ApplyPhase::Sending) = state.apply_phase {
                let now = std::time::Instant::now();
                if !state.apply_waiting(now) {
                    state.notify(
                        NotificationKind::Warn,
                        "已受理但未收到 WALLPAPER_APPLIED：**不能**视为已上屏（按 `W` 重试或查 System 页）",
                    );
                }
            }
        }
        Action::Resized => {}
    }
    effects
}

/// 当前应用目标展开成"实际下发的输出列表"。
///
/// **默认目标只下发一个（daemon 默认输出），绝不偷偷广播到所有屏幕**；
/// 只有显式选了 `全部显示器` 才逐屏下发。
fn targets(state: &AppState) -> Vec<Option<String>> {
    match &state.apply_target {
        ApplyTarget::Default => vec![None],
        ApplyTarget::Output(name) => vec![Some(name.clone())],
        ApplyTarget::AllOutputs => state
            .outputs
            .outputs
            .iter()
            .map(|o| Some(o.name.clone()))
            .collect(),
    }
}

/// 只做"形状"检查：**不是**安全边界（白名单/规范化在 daemon）。
pub fn is_absolute_server_path(path: &str) -> bool {
    let t = path.trim();
    t.starts_with('/') && !t.starts_with("//") && !t.contains('\0')
}

fn apply_editor(state: &mut AppState, op: EditOp) {
    let e = &mut state.editor;
    match op {
        EditOp::Insert(c) => e.insert(c),
        EditOp::Backspace => e.backspace(),
        EditOp::Delete => e.delete(),
        EditOp::Left => e.move_left(),
        EditOp::Right => e.move_right(),
        EditOp::Home => e.home(),
        EditOp::End => e.end(),
    }
}

fn apply_search_edit(state: &mut AppState, op: EditOp) {
    match op {
        EditOp::Insert(c) => state.search.push(c),
        EditOp::Backspace => {
            state.search.pop();
        }
        EditOp::Delete => state.search.clear(),
        EditOp::Left | EditOp::Right | EditOp::Home | EditOp::End => {}
    }
}

/// `?` 帮助页的**按页键位表**（与 `docs/TUI_DESIGN.md` §6 一致；各页不冲突）。
///
/// 返回 `(键, 说明)`；全局键在每页末尾统一列出。
pub fn page_help(st: &AppState) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    match st.page {
        Page::Dashboard => {
            out.push(("Space / p", "暂停 / 继续（下发 daemon）"));
            out.push(("N / n", "下一张"));
            out.push(("R", "重载配置"));
            out.push(("W", "应用选中媒体到当前目标输出"));
        }
        Page::Library => {
            out.push(("↑↓ / j k", "移动选中（异步取完整元数据）"));
            out.push(("Enter", "打开详情"));
            out.push(("/", "搜索（防抖 250ms，服务端过滤）"));
            out.push(("F", "筛选：全部/图片/视频（下推）"));
            out.push(("O", "排序：名称/大小/修改时间（下推）"));
            out.push(("PgUp/PgDn", "翻页（服务端分页，页大小 ≤200）"));
            out.push(("I", "输入路径（须为服务器绝对路径，daemon 校验）"));
            out.push((
                "S",
                "来源管理（a 新增 / d 删除 / r 递归 / k 类型 / s 保存）",
            ));
            out.push((
                "a",
                "把选中项加入当前播放列表（未选中/无列表时给可操作提示）",
            ));
            out.push(("Y", "复制完整路径（OSC52，失败回退文本弹窗）"));
            out.push(("W", "应用选中项到当前目标输出"));
            out.push(("T", "切换应用目标（默认/指定输出/全部显示器）"));
        }
        Page::Playlists => {
            // ---- 核心操作（底栏也会提示）----
            out.push(("↑↓ / j k", "选择播放列表"));
            out.push((
                "← / →",
                "切换播放模式：单视频循环 / 顺序循环 / 随机播放（立即生效）",
            ));
            out.push(("a", "把媒体库选中项加入当前播放列表"));
            out.push(("Enter", "播放（编辑态：光标那条；否则：列表第一条）"));
            out.push(("n", "新建播放列表（运行时）"));
            out.push(("e", "编辑选中（进入编辑态）"));
            out.push(("d", "（编辑态）移除光标条目"));
            out.push(("k / j", "（编辑态）条目上移 / 下移"));
            out.push(("Esc", "（编辑态）放弃编辑：不下发、不落盘"));
            // ---- 高级操作（t67：从主界面移入；仍全部可用）----
            out.push(("——", "高级操作（主界面不再提示，但都能用）"));
            out.push(("m", "只切 顺序/随机（要分开设置字段时才用）"));
            out.push(("v", "只切 播完 next/loop（同上）"));
            out.push(("r", "禁止连续重复 开/关"));
            out.push(("o", "绑定输出（默认/DP-1/…）"));
            out.push(("+ / -", "轮换间隔 ±60s"));
            out.push(("p", "加条目（等价于 a）"));
            out.push(("x", "放弃编辑（等价于 Esc）"));
            out.push(("N", "重命名（文本输入）"));
            out.push(("A", "应用编辑（update_playlist，只改运行时）"));
            out.push(("S", "保存配置（落盘；只有这里会写文件）"));
        }
        Page::Outputs => {
            out.push(("↑↓", "选择输出"));
            out.push(("C / V", "fit：裁切填充/完整显示/拉伸铺满/居中原始"));
            out.push(("+ / -", "zoom（daemon 单位）"));
            out.push(("h / l", "水平偏移 ∓10"));
            out.push(("K / J", "垂直偏移 ∓10"));
            out.push(("X", "旋转 90°"));
            out.push(("b / B", "画面亮度 ∓5%（10%–100%）"));
            out.push(("0", "重置适配与亮度为默认值"));
            out.push(("A", "立即应用（只改运行时）"));
            out.push(("S", "保存配置（落盘）"));
        }
        Page::Effects => {
            out.push(("A", "试用效果（只改运行时；未实现的效果会被拒绝）"));
            out.push(("[ / ]", "切换效果"));
            out.push(("{ / }", "切换缓动"));
            out.push(("< / >", "时长 ±100ms（默认 fade 350ms）"));
            out.push(("S", "保存配置（落盘）"));
        }
        Page::System => {
            out.push(("↑↓", "滚动"));
            out.push(("e", "Re-connect（重建 IPC 连接并重订阅）"));
            out.push(("S", "保存配置（落盘）"));
            out.push(("R", "重扫来源 / 重载配置"));
        }
        Page::Static => {
            out.push(("a", "把媒体库选中项设为静态壁纸（**只允许图片**）"));
            out.push(("c", "清除静态壁纸：恢复播放列表（从暂停处继续）"));
        }
    }
    // 全局键（每页都有，放在后面）
    out.push(("1-7", "切换页面"));
    out.push(("Tab / Shift+Tab", "焦点：导航/列表/详情/操作"));
    out.push(("← / →", "小档：切换 列表/详情/操作 子页"));
    out.push(("?", "本帮助页"));
    out.push(("Q / Ctrl+C", "退出 TUI（**不停 daemon**，恢复终端）"));
    out
}

/// 应用/保存的语义说明（UI 与测试共用）。
pub fn commit_hint(kind: CommitKind) -> &'static str {
    match kind {
        CommitKind::Apply => "立即应用（只改运行时，不写配置）",
        CommitKind::Save => "持久保存（写入配置文件）",
    }
}

/// 标记一次操作已被服务端受理。
pub fn note_accepted(state: &mut AppState, id: &str) {
    state.set_op_state(id, PendingState::Accepted);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::{PlayMode, PlaylistEdit};
    use crate::ui::widgets::bars::shortcuts;
    use liuguang_ipc::protocol::{MediaItem, OutputStatus, OutputsResult};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state_with_media(n: usize) -> AppState {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.focus = Focus::List;
        st.media.items = (0..n)
            .map(|i| MediaItem {
                id: format!("/srv/wallpapers/城市夜景_{i}.png"),
                source_id: "src-1".into(),
                name: format!("城市夜景_{i}.png"),
                path: format!("/srv/wallpapers/城市夜景_{i}.png"),
                kind: "image".into(),
                size_bytes: 1024,
                modified_unix: 0,
                details: None,
            })
            .collect();
        st.media.total = n;
        st
    }

    /// 把一个提示键串展开成**具体的按键**。
    ///
    /// 支持提示栏里出现的所有写法：单键（`P`/`n`）、组合（`↑↓`、`PgUp/PgDn`、
    /// `[ ]`、`{ }`、`< >`）、区间（`1-6`）。**每个分量都会被逐一检查**，
    /// 所以"提示了但只绑了一半"同样会被抓住。
    fn keys_of(hint: &str) -> Vec<KeyEvent> {
        fn ch(c: char) -> KeyEvent {
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
        }
        let mut out = Vec::new();
        for part in hint.split(['/', ' ']) {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match p {
                "↑↓" => {
                    out.push(key(KeyCode::Up));
                    out.push(key(KeyCode::Down));
                }
                "←→" => {
                    out.push(key(KeyCode::Left));
                    out.push(key(KeyCode::Right));
                }
                "↑" => out.push(key(KeyCode::Up)),
                "↓" => out.push(key(KeyCode::Down)),
                "←" => out.push(key(KeyCode::Left)),
                "→" => out.push(key(KeyCode::Right)),
                "PgUp" => out.push(key(KeyCode::PageUp)),
                "PgDn" => out.push(key(KeyCode::PageDown)),
                "Tab" => out.push(key(KeyCode::Tab)),
                "Space" => out.push(ch(' ')),
                "Esc" => out.push(key(KeyCode::Esc)),
                "Enter" => out.push(key(KeyCode::Enter)),
                // 任意数字区间（`1-7`；t70 把页数从 6 提到 7）
                other
                    if other.len() == 3
                        && other.as_bytes()[1] == b'-'
                        && other.chars().next().unwrap().is_ascii_digit()
                        && other.chars().nth(2).unwrap().is_ascii_digit() =>
                {
                    let a = other.chars().next().unwrap();
                    let b = other.chars().nth(2).unwrap();
                    for c in a..=b {
                        out.push(ch(c));
                    }
                }
                other => {
                    let mut it = other.chars();
                    match (it.next(), it.next()) {
                        (Some(c), None) => out.push(ch(c)),
                        _ => panic!("测试不认识这个提示键写法: {other:?}"),
                    }
                }
            }
        }
        out
    }

    /// **不变量（t58）**：提示栏里出现的**每一个键**都必须真的被 keymap 处理。
    ///
    /// 为什么需要它：用户抱怨"播放列表不能用"，真因是提示栏宣传了大写 `P`
    /// （该页**根本没绑定**）而真正的入口 `n`/`e` 一个都没提。提示栏是用户唯一
    /// 的发现途径，它与 keymap 不一致就等于"功能不存在"。
    ///
    /// 变异检查：往任何一页加一条假提示（如 `Hint::new("P", …)`）⇒ 本用例变红。
    #[test]
    fn every_advertised_hint_key_is_actually_handled() {
        let mut bad: Vec<String> = Vec::new();
        for page in Page::ALL {
            for focus in [Focus::Nav, Focus::List, Focus::Detail, Focus::Actions] {
                for editing in [false, true] {
                    let mut st = AppState::new();
                    st.page = page;
                    st.focus = focus;
                    if editing {
                        st.playlist_edit = Some(PlaylistEdit::new_default());
                    }
                    for hint in shortcuts(&st) {
                        for k in keys_of(&hint.key) {
                            if action_for(&st, k).is_none() {
                                bad.push(format!(
                                    "page={:?} focus={:?} editing={} 提示 [{}] {} ⇒ {:?} 未绑定",
                                    page, focus, editing, hint.key, hint.label, k.code
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "提示栏宣传了**未绑定**的键（用户按了没反应）：\n{}",
            bad.join("\n")
        );
    }

    // ---- t67：播放模式选择器 / 加入 / 高级面板 ----

    /// 测试用播放列表。
    fn playlist(
        id: &str,
        mode: &str,
        end: &str,
        entries: &[&str],
    ) -> liuguang_ipc::protocol::Playlist {
        liuguang_ipc::protocol::Playlist {
            id: id.to_string(),
            name: format!("列表 {id}"),
            entries: entries
                .iter()
                .map(|m| liuguang_ipc::protocol::PlaylistEntry {
                    media_id: (*m).to_string(),
                })
                .collect(),
            mode: mode.to_string(),
            interval_seconds: 600,
            on_video_end: end.to_string(),
            output: None,
            no_repeat: true,
        }
    }

    /// **t67 核心**：三种模式各自**一次性**写好 `mode` + `on_video_end` 两个字段。
    ///
    /// **变异**：只改 `mode`、不改 `on_video_end`（或反之）⇒ 本测试变红
    /// ——"顺序循环 vs 随机播放"只差 `mode`，"单视频循环"只差 `on_video_end`。
    #[test]
    fn mode_selector_sets_both_protocol_fields() {
        let mut st = AppState::new();
        st.page = Page::Playlists;
        st.playlists = vec![playlist("pl-1", "sequential", "next", &["/srv/a.png"])];
        assert_eq!(st.playlist_mode(), Some(PlayMode::SequentialLoop));
        // 从"顺序循环"按 → 依次到达：随机播放 → 单视频循环 → 顺序循环
        let expected = [
            (PlayMode::Shuffle, ("random", "next")),
            (PlayMode::SingleLoop, ("sequential", "loop")),
            (PlayMode::SequentialLoop, ("sequential", "next")),
        ];
        for (want, (want_mode, want_end)) in expected {
            let effects = apply(&mut st, Action::PlaylistMode(true));
            let params = effects
                .iter()
                .find_map(|e| match e {
                    Effect::PlaylistPatch {
                        params,
                        kind: PatchKind::Mode(label),
                    } => {
                        assert_eq!(*label, want.label(), "反馈里的模式名必须与状态一致");
                        Some(params)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("{want:?} 必须**立即下发**（PlaylistPatch），实际 {effects:?}")
                });
            assert_eq!(params.playlist_id.as_deref(), Some("pl-1"), "必须带列表 id");
            assert_eq!(params.mode, want_mode, "{want:?} 的 mode 字段必须一次写对");
            assert_eq!(
                params.on_video_end, want_end,
                "{want:?} 的 on_video_end 字段必须一次写对"
            );
            assert_eq!(params.entries.len(), 1, "整条列表一起下发（不得清空条目）");
            assert_eq!(params.interval_seconds, 600, "不得把间隔重置成默认值");
            // 模拟 daemon 回写 + `RefreshAll` 拉回（真机上由 app.rs 完成）：
            // 状态只有**从 daemon 拉回**才更新，本地不擅自假设成功。
            let p = &mut st.playlists[0];
            p.mode = params.mode.clone();
            p.on_video_end = params.on_video_end.clone();
            assert_eq!(st.playlist_mode(), Some(want), "→ 应切到 {want:?}");
        }
        // ← 反向：从顺序循环回到单视频循环
        let effects = apply(&mut st, Action::PlaylistMode(false));
        assert!(
            matches!(
                effects.first(),
                Some(Effect::PlaylistPatch { params, .. })
                    if params.mode == "sequential" && params.on_video_end == "loop"
            ),
            "← 应回到单视频循环，实际 {effects:?}"
        );
    }

    /// 编辑态下 ←/→ **只改缓冲**（不得偷偷下发未确认的改动），且缓冲两个字段都对。
    #[test]
    fn mode_selector_in_edit_mode_only_touches_the_buffer() {
        let mut st = AppState::new();
        st.page = Page::Playlists;
        st.playlist_edit = Some(PlaylistEdit::new_default());
        let effects = apply(&mut st, Action::PlaylistMode(true));
        assert!(effects.is_empty(), "编辑态不得下发，实际 {effects:?}");
        let edit = st.playlist_edit.as_ref().expect("仍在编辑");
        assert_eq!(
            (edit.mode.as_str(), edit.on_video_end.as_str()),
            ("random", "next")
        );
        assert!(st.dirty, "缓冲已改 ⇒ 必须有未保存标记");
    }

    /// t67：媒体库页 `a` ⇒ 条目 +1 且内容正确；重复加入与"没有列表"都给可操作提示。
    #[test]
    fn add_from_library_appends_entry() {
        let mut st = state_with_media(2);
        st.media_cursor = 1;
        st.playlists = vec![playlist(
            "pl-1",
            "random",
            "next",
            &["/srv/wallpapers/城市夜景_0.png"],
        )];
        assert_eq!(
            action_for(&st, key(KeyCode::Char('a'))),
            Some(Action::PlaylistAddToCurrent),
            "媒体库页 `a` 必须被识别"
        );
        let effects = apply(&mut st, Action::PlaylistAddToCurrent);
        let (params, kind) = effects
            .iter()
            .find_map(|e| match e {
                Effect::PlaylistPatch { params, kind } => Some((params, kind)),
                _ => None,
            })
            .expect("必须下发一次 PlaylistPatch");
        let ids: Vec<&str> = params.entries.iter().map(|e| e.media_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "/srv/wallpapers/城市夜景_0.png",
                "/srv/wallpapers/城市夜景_1.png"
            ],
            "新条目必须追加在末尾"
        );
        assert_eq!(params.mode, "random", "加入条目不得顺手改模式");
        assert_eq!(params.on_video_end, "next");
        assert!(
            matches!(kind, PatchKind::AddEntry { count: 2, .. }),
            "反馈必须带新条数，实际 {kind:?}"
        );

        // 重复加入（光标回到已在列表里的那条）⇒ 不静默、不下发
        st.media_cursor = 0;
        let effects = apply(&mut st, Action::PlaylistAddToCurrent);
        assert!(effects.is_empty(), "重复条目不得下发");
        let text = st
            .last_notification()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(text.contains("已经有这条"), "应说明重复，实际: {text}");
    }

    /// t67：没有播放列表时 `a` 必须给**可操作**提示（不是静默失败）。
    #[test]
    fn add_without_playlist_is_actionable() {
        let mut st = state_with_media(1);
        let effects = apply(&mut st, Action::PlaylistAddToCurrent);
        assert!(effects.is_empty(), "无列表时不得下发");
        let text = st
            .last_notification()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        for k in ["n", "播放列表"] {
            assert!(text.contains(k), "提示应可操作（含 {k:?}），实际: {text}");
        }
    }

    /// t67：旧键**仍在 keymap 里可用**，但主提示栏不再宣传；`?`（`page_help`）列出它们。
    #[test]
    fn advanced_keys_are_moved_out_of_the_main_bar() {
        let mut st = AppState::new();
        st.page = Page::Playlists;
        st.playlist_edit = Some(PlaylistEdit::new_default());
        let main: Vec<String> = shortcuts(&st).into_iter().map(|h| h.key).collect();
        for k in ["m", "v", "r", "o", "+/-", "p", "x", "A", "S"] {
            assert!(
                !main.iter().any(|m| m == k),
                "主提示栏不得再出现高级键 {k:?}，实际 {main:?}"
            );
        }
        // 但 keymap 仍然接受它们（是"收起来"而不是"删功能"）
        for (k, want) in [
            ('m', Action::PlaylistCycleMode),
            ('v', Action::PlaylistToggleVideoEnd),
            ('r', Action::PlaylistToggleNoRepeat),
            ('o', Action::PlaylistCycleOutput),
            ('+', Action::PlaylistInterval(60)),
            ('-', Action::PlaylistInterval(-60)),
            ('p', Action::PlaylistAddEntry),
            ('x', Action::PlaylistDiscard),
            ('A', Action::Apply),
        ] {
            assert_eq!(
                action_for(&st, key(KeyCode::Char(k))),
                Some(want),
                "高级键 {k:?} 必须仍然可用"
            );
        }
        // `?` 的高级面板必须把它们列出来（可发现性不能丢）
        let help = page_help(&st);
        let keys: Vec<&str> = help.iter().map(|(k, _)| *k).collect();
        for k in ["m", "v", "r", "o", "+ / -", "p", "x", "A", "S"] {
            assert!(keys.contains(&k), "高级面板必须列出 {k:?}，实际 {keys:?}");
        }
    }

    #[test]
    fn hotkeys_1_to_6_switch_pages() {
        let mut st = AppState::new();
        for page in Page::ALL {
            let a = action_for(&st, key(KeyCode::Char(page.hotkey()))).unwrap();
            assert_eq!(a, Action::Goto(page));
            apply(&mut st, a);
            assert_eq!(st.page, page);
        }
    }

    #[test]
    fn quit_keys_do_not_touch_backend() {
        for k in [KeyCode::Char('Q'), KeyCode::Char('c')] {
            let mut st = AppState::new();
            let ev = if k == KeyCode::Char('c') {
                KeyEvent::new(k, KeyModifiers::CONTROL)
            } else {
                key(k)
            };
            let action = action_for(&st, ev).unwrap();
            let effects = apply(&mut st, action);
            assert!(st.quit);
            assert_eq!(effects, vec![Effect::Quit]);
        }
    }

    #[test]
    fn apply_and_save_are_distinct() {
        let mut st = AppState::new();
        st.page = Page::Library;
        assert_eq!(
            action_for(&st, key(KeyCode::Char('S'))).unwrap(),
            Action::SourceManage
        );
        st.page = Page::System;
        assert_eq!(
            action_for(&st, key(KeyCode::Char('S'))).unwrap(),
            Action::Save
        );
        // Apply 需要先选输出
        assert!(apply(&mut st, Action::Apply).is_empty());
        assert!(st.last_notification().is_some());
        st.selected_output = Some("DP-1".to_string());
        assert_eq!(
            apply(&mut st, Action::Apply),
            vec![Effect::ApplyTransform("DP-1".to_string())]
        );
        assert!(st.dirty);
        assert_eq!(apply(&mut st, Action::Save), vec![Effect::SaveConfig]);
    }

    #[test]
    fn modal_captures_keys_and_restores_focus() {
        let mut st = state_with_media(3);
        st.focus = Focus::Detail;
        assert!(apply(&mut st, Action::InputPath).is_empty());
        assert!(st.modal_open());
        let a = action_for(&st, key(KeyCode::Char('3'))).unwrap();
        assert_eq!(a, Action::Edit(EditOp::Insert('3')));
        for c in "/srv/壁纸/a.png".chars() {
            let a = action_for(&st, key(KeyCode::Char(c))).unwrap();
            apply(&mut st, a);
        }
        assert_eq!(st.editor.text(), "/srv/壁纸/a.png");
        let a = action_for(&st, key(KeyCode::Esc)).unwrap();
        apply(&mut st, a);
        assert!(!st.modal_open());
        assert_eq!(st.focus, Focus::Detail);
    }

    #[test]
    fn path_submit_is_sent_to_daemon_even_when_shape_looks_odd() {
        // 相对路径：TUI 只给提示，仍然下发给 daemon 判定（安全边界在 daemon）
        let mut st = state_with_media(1);
        apply(&mut st, Action::InputPath);
        st.editor.set_text("~/a.png");
        let effects = apply(&mut st, Action::Confirm);
        match &effects[0] {
            Effect::SetWallpaper { path, outputs } => {
                assert_eq!(path, "~/a.png");
                assert_eq!(outputs, &vec![None], "默认目标只下发 daemon 默认输出");
            }
            other => panic!("期望 SetWallpaper，得到 {other:?}"),
        }
        assert!(
            st.last_notification()
                .unwrap()
                .text
                .contains("仍交给 daemon")
        );
        // 空路径：不提交
        apply(&mut st, Action::InputPath);
        st.editor.set_text("   ");
        assert!(apply(&mut st, Action::Confirm).is_empty());
    }

    #[test]
    fn apply_target_never_broadcasts_silently() {
        let mut st = state_with_media(1);
        st.outputs = OutputsResult {
            outputs: vec![
                OutputStatus {
                    name: "DP-1".into(),
                    connected: true,
                    mode: None,
                    mode_count: 1,
                },
                OutputStatus {
                    name: "HDMI-A-1".into(),
                    connected: true,
                    mode: None,
                    mode_count: 1,
                },
            ],
        };
        // 默认：单目标
        assert_eq!(apply(&mut st, Action::ApplyWallpaper).len(), 1);
        match &apply(&mut st, Action::ApplyWallpaper)[0] {
            Effect::SetWallpaper { outputs, .. } => assert_eq!(outputs.len(), 1),
            other => panic!("{other:?}"),
        }
        // 显式全部：逐屏下发
        st.apply_target = ApplyTarget::AllOutputs;
        match &apply(&mut st, Action::ApplyWallpaper)[0] {
            Effect::SetWallpaper { outputs, .. } => {
                assert_eq!(outputs.len(), 2);
                assert_eq!(outputs[0].as_deref(), Some("DP-1"));
                assert_eq!(outputs[1].as_deref(), Some("HDMI-A-1"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_outputs_blocks_apply_with_clear_notice() {
        let mut st = state_with_media(1);
        st.outputs.outputs.clear();
        assert!(apply(&mut st, Action::ApplyWallpaper).is_empty());
        assert!(
            st.last_notification()
                .unwrap()
                .text
                .contains("未检测到显示器"),
            "{:?}",
            st.last_notification()
        );
    }

    #[test]
    fn moving_selection_requests_detail() {
        let mut st = state_with_media(3);
        let effects = apply(&mut st, Action::Move(1));
        assert_eq!(st.media_cursor, 1);
        assert!(matches!(effects[0], Effect::LoadDetail(_)));
        // 到边界不动 → 不重复请求
        st.media_cursor = 2;
        assert!(apply(&mut st, Action::Move(1)).is_empty());
    }

    #[test]
    fn filter_sort_and_paging_push_down() {
        let mut st = state_with_media(3);
        st.media.total = 10_000;
        let e = apply(&mut st, Action::FilterCycle);
        assert_eq!(st.filter, crate::model::MediaFilter::Image);
        assert_eq!(e, vec![Effect::LoadMedia]);
        let e = apply(&mut st, Action::SortCycle);
        assert_eq!(st.sort, crate::model::MediaSort::Size);
        assert_eq!(e, vec![Effect::LoadMedia]);
        st.media.page = 199;
        apply(&mut st, Action::PageMove(1));
        assert_eq!(st.media.page, 199, "末页不越界");
        apply(&mut st, Action::PageMove(-1));
        assert_eq!(st.media.page, 198);
    }

    #[test]
    fn reconnect_action_is_visible() {
        let mut st = AppState::new();
        let e = apply(&mut st, Action::Reconnect);
        assert_eq!(e, vec![Effect::Reconnect]);
        assert_eq!(st.reconnect_attempts, 1);
        assert_eq!(st.connection, crate::model::Connection::Reconnecting);
    }

    #[test]
    fn rescan_pushes_source_id() {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.sources.sources = vec![liuguang_ipc::protocol::MediaSource {
            id: "src-9".into(),
            name: "x".into(),
            kind: "image".into(),
            available: true,
        }];
        let e = apply(&mut st, Action::Rescan);
        assert_eq!(e, vec![Effect::ScanSource(Some("src-9".to_string()))]);
        assert!(matches!(st.scan, crate::model::ScanState::Requested { .. }));
    }

    #[test]
    fn copy_path_effect_and_fallback_hint() {
        let st = state_with_media(1);
        let mut st = st;
        let e = apply(&mut st, Action::CopyPath);
        assert!(matches!(e[0], Effect::CopyPath(_)));
    }

    #[test]
    fn keys_do_not_conflict_across_pages() {
        let mut st = AppState::new();
        for page in Page::ALL {
            st.page = page;
            for p in Page::ALL {
                assert_eq!(
                    action_for(&st, key(KeyCode::Char(p.hotkey()))).unwrap(),
                    Action::Goto(p)
                );
            }
            assert_eq!(action_for(&st, key(KeyCode::Char('?'))), Some(Action::Help));
        }
    }

    #[test]
    fn resize_and_apply_timeout_are_safe() {
        let mut st = state_with_media(2);
        st.media_cursor = 1;
        assert!(apply(&mut st, Action::Resized).is_empty());
        assert_eq!(st.media_cursor, 1);
        st.apply_phase = Some(ApplyPhase::Accepted);
        st.apply_started_at = Some(std::time::Instant::now());
        assert!(apply(&mut st, Action::ApplyTimeoutCheck).is_empty());
    }

    #[test]
    fn commit_hints_explicit() {
        assert!(commit_hint(CommitKind::Apply).contains("立即应用"));
        assert!(commit_hint(CommitKind::Save).contains("持久保存"));
    }

    #[test]
    fn brightness_and_reset_transform_keys_work() {
        let mut st = AppState::new();
        st.page = Page::Outputs;
        assert_eq!(
            action_for(&st, key(KeyCode::Char('b'))),
            Some(Action::Brightness(-1))
        );
        assert_eq!(
            action_for(&st, key(KeyCode::Char('B'))),
            Some(Action::Brightness(1))
        );
        assert_eq!(
            action_for(&st, key(KeyCode::Char('0'))),
            Some(Action::ResetTransform)
        );
        apply(&mut st, Action::Brightness(-1));
        assert_eq!(st.transform.brightness_percent(), 95);
        assert!(st.dirty);
        apply(&mut st, Action::CycleFit(true));
        apply(&mut st, Action::ResetTransform);
        assert_eq!(st.transform.brightness_percent(), 100);
        assert_eq!(st.transform.fit, "cover");
    }
}
