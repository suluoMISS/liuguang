//! 设置页：媒体来源、轮换间隔、性能开关、RGB/VA-API、开机自启、关于。
//!
//! 所有改动先攒在控件里，点"应用设置"才 `Config::load` → 改字段 →
//! `save_atomic` → 通知守护进程 `reload`。这样：
//! - 不碰守护进程内存里那份配置（它自己负责落盘与重载）；
//! - 写失败时文件保持原样（`save_atomic` 是原子替换），不会写坏一半；
//! - 播放列表/性能/间隔都走同一个 reload 路径，行为可预测。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use liuguang_core::config::Config;

use crate::ipc::Cmd;
use crate::{App, ui};

/// 设置页控件。
pub struct SettingsView {
    pub root: gtk::Widget,
    pub sources_list: gtk::ListBox,
    pub source_path: adw::EntryRow,
    pub source_kind: adw::ComboRow,
    pub add_source: gtk::Button,
    pub rescan: gtk::Button,
    pub interval: adw::SpinRow,
    pub pause_fullscreen: adw::SwitchRow,
    pub pause_screen_off: adw::SwitchRow,
    pub reduce_load: adw::SwitchRow,
    /// 游戏进程让路（`performance.detect_games`）。
    pub pause_games: adw::SwitchRow,
    /// 笔记本使用电池时自动暂停（`performance.pause_on_battery`）。
    pub pause_battery: adw::SwitchRow,
    /// 自定义游戏进程特征（`performance.game_processes`）。
    pub game_processes: adw::EntryRow,
    /// 明确排除的进程特征（`performance.game_ignore_processes`）。
    pub game_ignore: adw::EntryRow,
    /// 显存暂停阈值（`performance.pause_when_gpu_memory_above`，0 = 关闭）。
    pub gpu_above: adw::SpinRow,
    /// 显存恢复阈值（`performance.resume_when_gpu_memory_below`）。
    pub gpu_below: adw::SpinRow,
    pub rgb: adw::ComboRow,
    pub vaapi: adw::EntryRow,
    pub autostart: adw::SwitchRow,
    pub autostart_status: adw::ActionRow,
    pub apply: gtk::Button,
    pub reload: gtk::Button,
    pub about: gtk::Button,
    pub status: gtk::Label,
    /// 用户改过、还没应用。
    pub dirty: Cell<bool>,
    /// 正在用配置回填控件（不要把回填当用户改动）。
    pub syncing: Cell<bool>,
}

const RGB_IDS: [&str; 3] = ["auto", "always", "never"];
const SOURCE_KINDS: [(&str, &str); 2] = [("image", "图片目录"), ("video", "视频目录")];

/// 构建设置页。
pub fn build(app: &Rc<App>) -> Rc<SettingsView> {
    let sources_list = gtk::ListBox::new();
    sources_list.set_selection_mode(gtk::SelectionMode::None);
    sources_list.add_css_class("boxed-list");

    let source_path = adw::EntryRow::new();
    source_path.set_title("媒体目录（绝对路径）");
    let browse_source = ui::icon_button("folder-open-symbolic", "浏览文件夹");
    browse_source.set_valign(gtk::Align::Center);
    source_path.add_suffix(&browse_source);
    let source_kind = adw::ComboRow::new();
    source_kind.set_title("类型");
    source_kind.set_model(Some(&gtk::StringList::new(&["图片目录", "视频目录"])));
    source_kind.set_selected(0);

    let add_source = ui::action_button("添加目录", false);
    let rescan = ui::action_button("重新扫描", false);
    let source_actions = adw::ActionRow::new();
    source_actions.set_title("来源操作");
    source_actions.set_subtitle("添加后自动保存配置并扫描媒体目录");
    source_actions.set_activatable(false);
    let source_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    source_buttons.append(&rescan);
    source_buttons.append(&add_source);
    source_actions.add_suffix(&source_buttons);

    let sources_group = adw::PreferencesGroup::new();
    sources_group.set_title("媒体来源");
    sources_group.set_description(Some("库页与轮换都只使用这些目录（以及它们的子目录）。"));
    sources_group.add(&sources_list);
    sources_group.add(&source_path);
    sources_group.add(&source_kind);
    sources_group.add(&source_actions);

    let interval = adw::SpinRow::new(
        Some(&gtk::Adjustment::new(
            600.0, 1.0, 86_400.0, 30.0, 120.0, 0.0,
        )),
        30.0,
        0,
    );
    interval.set_title("轮换间隔");
    interval.set_subtitle("秒；输出自己的 interval_seconds 会覆盖这个值");
    let playback_group = adw::PreferencesGroup::new();
    playback_group.set_title("播放");
    playback_group.add(&interval);

    let pause_fullscreen = adw::SwitchRow::new();
    pause_fullscreen.set_title("全屏时让路");
    pause_fullscreen.set_subtitle("检测到全屏窗口时暂停壁纸（省电、避免抢 GPU）");
    let pause_screen_off = adw::SwitchRow::new();
    pause_screen_off.set_title("熄屏时暂停");
    pause_screen_off.set_subtitle("显示器关闭/断开后停止解码");
    let reduce_load = adw::SwitchRow::new();
    reduce_load.set_title("CPU 压力时降载");
    reduce_load.set_subtitle("按 PSI/负载自动降低帧率或停掉视频");
    let pause_games = adw::SwitchRow::new();
    pause_games.set_title("游戏运行时让路");
    pause_games
        .set_subtitle("识别 Steam / Proton / Wine / Lutris / 模拟器进程（约 4s 触发，8s 恢复）");
    let pause_battery = adw::SwitchRow::new();
    pause_battery.set_title("电池供电时自动暂停");
    pause_battery.set_subtitle("笔记本拔掉电源切换至电池供电时暂停动态壁纸，插回电源自动恢复");
    let game_processes = adw::EntryRow::new();
    game_processes.set_title("附加游戏特征（逗号分隔，不区分大小写）");
    game_processes.set_tooltip_text(Some("例如: factorio, dota2, cs2"));
    let game_ignore = adw::EntryRow::new();
    game_ignore.set_title("排除非游戏特征（逗号分隔，优先于全部规则）");
    game_ignore.set_tooltip_text(Some("例如: QQ, wechat, electron"));
    let gpu_above = adw::SpinRow::with_range(0.0, 100.0, 5.0);
    gpu_above.set_title("显存高于");
    gpu_above.set_subtitle(
        "NVIDIA（nvidia-smi）与 AMD sysfs 显存占用达到该百分比时暂停壁纸（0 = 关闭）",
    );
    gpu_above.set_digits(0);
    let gpu_below = adw::SpinRow::with_range(0.0, 100.0, 5.0);
    gpu_below.set_title("显存低于");
    gpu_below.set_subtitle("降到该百分比后自动恢复播放（应小于上面的暂停阈值）");
    gpu_below.set_digits(0);
    let performance_group = adw::PreferencesGroup::new();
    performance_group.set_title("性能");
    performance_group.add(&pause_fullscreen);
    performance_group.add(&pause_screen_off);
    performance_group.add(&pause_games);
    performance_group.add(&pause_battery);
    performance_group.add(&game_processes);
    performance_group.add(&game_ignore);
    performance_group.add(&reduce_load);
    performance_group.add(&gpu_above);
    performance_group.add(&gpu_below);

    let rgb = adw::ComboRow::new();
    rgb.set_title("RGB 输出策略");
    rgb.set_subtitle("dmabuf-RGB：自动 / 总是 / 从不");
    rgb.set_model(Some(&gtk::StringList::new(&[
        "自动",
        "总是用 dmabuf-RGB",
        "从不用",
    ])));
    rgb.set_selected(0);
    let vaapi = adw::EntryRow::new();
    vaapi.set_title("VA-API 节点");
    vaapi.set_tooltip_text(Some("auto / off / /dev/dri/renderD128 之类的绝对路径"));
    let media_group = adw::PreferencesGroup::new();
    media_group.set_title("解码");
    media_group.add(&rgb);
    media_group.add(&vaapi);

    let autostart = adw::SwitchRow::new();
    autostart.set_title("开机自启");
    autostart.set_subtitle("登录后在后台启动图形界面（不启动守护进程）");
    let autostart_status = adw::ActionRow::new();
    autostart_status.set_title("自启文件");
    autostart_status.set_activatable(false);
    let autostart_group = adw::PreferencesGroup::new();
    autostart_group.set_title("桌面集成");
    autostart_group.add(&autostart);
    autostart_group.add(&autostart_status);

    let about = ui::action_button("关于流光", false);
    let reload = ui::action_button("重新读取配置", false);
    reload.set_tooltip_text(Some("丢弃未应用的改动，重新从磁盘读配置并让守护进程重载"));
    let apply = ui::action_button("应用设置", true);
    apply.set_sensitive(false);
    let actions_row = adw::ActionRow::new();
    actions_row.set_title("保存");
    actions_row.set_subtitle("写入配置文件后通知守护进程 reload");
    actions_row.set_activatable(false);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.append(&reload);
    buttons.append(&apply);
    actions_row.add_suffix(&buttons);

    let about_row = adw::ActionRow::new();
    about_row.set_title("版本");
    about_row.set_subtitle(env!("CARGO_PKG_VERSION"));
    about_row.set_activatable(false);
    about_row.add_suffix(&about);

    let status = gtk::Label::new(None);
    status.add_css_class("dim");
    status.add_css_class("small");
    status.set_xalign(0.0);
    status.set_margin_start(16);
    status.set_margin_end(16);
    status.set_margin_bottom(8);
    status.set_wrap(true);

    // `adw::PreferencesPage` 自带滚动容器；外层再包 ScrolledWindow 会破坏焦点滚屏。
    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    page.add(&sources_group);
    page.add(&playback_group);
    page.add(&performance_group);
    page.add(&media_group);
    page.add(&autostart_group);
    let misc = adw::PreferencesGroup::new();
    misc.set_title("其他");
    misc.add(&actions_row);
    misc.add(&about_row);
    page.add(&misc);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(&page);
    content.append(&status);

    let view = Rc::new(SettingsView {
        root: content.upcast(),
        sources_list,
        source_path,
        source_kind,
        add_source,
        rescan,
        interval,
        pause_fullscreen,
        pause_screen_off,
        reduce_load,
        pause_games,
        pause_battery,
        game_processes,
        game_ignore,
        gpu_above,
        gpu_below,
        rgb,
        vaapi,
        autostart,
        autostart_status,
        apply,
        reload,
        about,
        status,
        dirty: Cell::new(false),
        syncing: Cell::new(false),
    });

    // —— 控件变化 → 标脏 ——
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        let mark = move || {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                mark_dirty(&app, &view);
            }
        };
        let mark_interval = mark.clone();
        view.interval.connect_value_notify(move |_| mark_interval());
        let mark_rgb = mark.clone();
        view.rgb.connect_selected_notify(move |_| mark_rgb());
        let mark_fs = mark.clone();
        view.pause_fullscreen
            .connect_active_notify(move |_| mark_fs());
        let mark_off = mark.clone();
        view.pause_screen_off
            .connect_active_notify(move |_| mark_off());
        let mark_load = mark.clone();
        view.reduce_load.connect_active_notify(move |_| mark_load());
        let mark_games = mark.clone();
        view.pause_games
            .connect_active_notify(move |_| mark_games());
        let mark_battery = mark.clone();
        view.pause_battery
            .connect_active_notify(move |_| mark_battery());
        let mark_gproc = mark.clone();
        view.game_processes.connect_changed(move |_| mark_gproc());
        let mark_gign = mark.clone();
        view.game_ignore.connect_changed(move |_| mark_gign());
        let mark_gpu = mark.clone();
        view.gpu_above.connect_value_notify(move |_| mark_gpu());
        let mark_gpu_below = mark.clone();
        view.gpu_below
            .connect_value_notify(move |_| mark_gpu_below());
        let mark_vaapi = mark.clone();
        view.vaapi.connect_changed(move |_| mark_vaapi());
    }

    // —— 目录选择器 ——
    {
        let app_down = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        browse_source.connect_clicked(move |_| {
            let (Some(app), Some(view)) = (app_down.upgrade(), view_down.upgrade()) else {
                return;
            };
            let dialog = gtk::FileDialog::builder()
                .title("选择媒体目录")
                .modal(true)
                .build();
            let view_inner = Rc::downgrade(&view);
            dialog.select_folder(
                Some(&app.window),
                None::<&gtk::gio::Cancellable>,
                move |res| {
                    if let Ok(file) = res
                        && let Some(path) = file.path()
                        && let Some(view) = view_inner.upgrade()
                    {
                        view.source_path.set_text(&path.display().to_string());
                    }
                },
            );
        });
    }

    // —— 添加/移除/扫描来源 ——
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.add_source.connect_clicked(move |_| {
            let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) else {
                return;
            };
            let path = view.source_path.text().trim().to_owned();
            if path.is_empty() {
                app.toast("先填一个绝对路径");
                return;
            }
            if !path.starts_with('/') {
                app.toast("媒体目录必须是绝对路径");
                return;
            }
            let kind = SOURCE_KINDS[view.source_kind.selected() as usize]
                .0
                .to_owned();
            app.send(Cmd::AddSource {
                kind,
                path: path.clone(),
                recursive: true,
            });
            app.send(Cmd::SaveConfig);
            app.send(Cmd::Sources);
            app.send(Cmd::Rescan);
            view.source_path.set_text("");
        });
    }
    {
        let app = Rc::downgrade(app);
        view.rescan.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.send(Cmd::ScanSource { source_id: None });
                app.send(Cmd::Sources);
                app.toast("已请求重新扫描全部来源");
            }
        });
    }
    // —— 保存 / 重新读取 / 关于 ——
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.apply.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                save(&app, &view);
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.reload.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                app.send(Cmd::Reload);
                app.send(Cmd::Config);
                app.send(Cmd::Sources);
                view.dirty.set(false);
                view.apply.set_sensitive(false);
                view.status.set_text("已请求重新读取配置");
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        view.about.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                show_about(&app);
            }
        });
    }
    // —— 开机自启开关（立即写/删 desktop 文件）——
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.autostart.connect_active_notify(move |row| {
            let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) else {
                return;
            };
            if view.syncing.get() {
                return;
            }
            let state = row.is_active();
            match set_autostart(state) {
                Ok(path) => {
                    view.autostart_status
                        .set_subtitle(&path.display().to_string());
                    view.status.set_text(if state {
                        "已启用开机自启"
                    } else {
                        "已关闭开机自启"
                    });
                }
                Err(error) => {
                    app.toast(&format!("自启设置失败：{error}"));
                    // 写失败就把开关拨回去（此刻 syncing=false，避免递归写文件）。
                    view.syncing.set(true);
                    row.set_active(!state);
                    view.syncing.set(false);
                }
            }
        });
    }

    view
}

/// 标记有未应用改动。
pub fn mark_dirty(app: &Rc<App>, view: &Rc<SettingsView>) {
    if view.syncing.get() {
        return;
    }
    view.dirty.set(true);
    view.apply.set_sensitive(true);
    view.status.set_text("有未应用的改动");
    let _ = app;
}

/// 用最新状态/配置刷新设置页（`Upd::Config`、`Upd::Sources` 到达时调用）。
pub fn refresh(app: &Rc<App>, view: &Rc<SettingsView>) {
    refresh_sources(app, view);
    let config = app.state.borrow().config.clone();
    if let Some(config) = config {
        if !view.dirty.get() {
            fill(view, &config);
        }
    }
    let enabled = autostart_enabled();
    view.syncing.set(true);
    view.autostart.set_active(enabled);
    view.syncing.set(false);
    view.autostart_status.set_subtitle(&match autostart_path() {
        Some(path) => {
            if enabled {
                format!("已写入 {}", path.display())
            } else {
                format!("未启用（{}）", path.display())
            }
        }
        None => "找不到 HOME/XDG_CONFIG_HOME，无法写自启文件".to_owned(),
    });
}

fn fill(view: &Rc<SettingsView>, config: &Config) {
    view.syncing.set(true);
    view.interval
        .set_value(config.slideshow.interval_seconds as f64);
    view.pause_fullscreen
        .set_active(config.performance.pause_on_fullscreen);
    view.pause_screen_off
        .set_active(config.performance.pause_when_display_off);
    view.reduce_load
        .set_active(config.performance.reduce_load_on_cpu_pressure);
    view.pause_games.set_active(config.performance.detect_games);
    view.pause_battery
        .set_active(config.performance.pause_on_battery);
    view.game_processes
        .set_text(&format_csv_list(&config.performance.game_processes));
    view.game_ignore
        .set_text(&format_csv_list(&config.performance.game_ignore_processes));
    view.gpu_above
        .set_value(f64::from(config.performance.pause_when_gpu_memory_above));
    view.gpu_below
        .set_value(f64::from(config.performance.resume_when_gpu_memory_below));
    view.rgb
        .set_selected(rgb_index(&config.media.video_dmabuf_rgb));
    view.vaapi.set_text(&config.media.video_vaapi);
    view.syncing.set(false);
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split([',', '，']) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(trimmed))
        {
            out.push(trimmed.to_owned());
        }
    }
    out
}

fn format_csv_list(items: &[String]) -> String {
    items.join(", ")
}

fn refresh_sources(app: &Rc<App>, view: &Rc<SettingsView>) {
    while let Some(child) = view.sources_list.first_child() {
        view.sources_list.remove(&child);
    }
    let sources = app.state.borrow().sources.clone();
    let Some(sources) = sources else {
        return;
    };
    for source in &sources.sources {
        let row = adw::ActionRow::new();
        row.set_title(&source.name);
        let kind = match source.kind.as_str() {
            "images" | "image" => "图片",
            "videos" | "video" => "视频",
            _ => "历史目录",
        };
        row.set_subtitle(&format!(
            "{kind} · {}{}",
            source.id,
            if source.available {
                ""
            } else {
                " · 不可用"
            }
        ));
        row.set_activatable(false);
        let scan = ui::icon_button("view-refresh-symbolic", "只扫描这个来源");
        {
            let app = Rc::downgrade(app);
            let id = source.id.clone();
            scan.connect_clicked(move |_| {
                if let Some(app) = app.upgrade() {
                    app.send(Cmd::ScanSource {
                        source_id: Some(id.clone()),
                    });
                }
            });
        }
        let remove = ui::icon_button("user-trash-symbolic", "从配置移除这个来源（不删文件）");
        {
            let app = Rc::downgrade(app);
            let id = source.id.clone();
            remove.connect_clicked(move |_| {
                if let Some(app) = app.upgrade() {
                    app.send(Cmd::RemoveSource {
                        source_id: id.clone(),
                    });
                    app.send(Cmd::SaveConfig);
                    app.send(Cmd::Sources);
                    app.send(Cmd::Media);
                }
            });
        }
        row.add_suffix(&scan);
        row.add_suffix(&remove);
        view.sources_list.append(&row);
    }
    if sources.sources.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("还没有媒体来源");
        row.set_subtitle("在下面填一个绝对路径并点“添加目录”");
        row.set_activatable(false);
        view.sources_list.append(&row);
    }
}

/// 把控件值写回配置文件，然后让守护进程 reload。
pub fn save(app: &Rc<App>, view: &Rc<SettingsView>) {
    let gpu_above = view.gpu_above.value().round() as u8;
    let gpu_below = view.gpu_below.value().round() as u8;
    if gpu_above > 0 && gpu_above <= gpu_below {
        app.toast("显存高于阈值必须大于低于阈值（或设为 0 关闭显存让路）");
        return;
    }

    let mut config = match Config::load(&app.config_path) {
        Ok(config) => config,
        Err(error) => {
            if app.config_path.exists() {
                app.toast(&format!("读取配置失败，未写入：{error}"));
                return;
            }
            Config::default()
        }
    };
    config.slideshow.interval_seconds = view.interval.value().round() as u64;
    config.performance.pause_on_fullscreen = view.pause_fullscreen.is_active();
    config.performance.pause_when_display_off = view.pause_screen_off.is_active();
    config.performance.reduce_load_on_cpu_pressure = view.reduce_load.is_active();
    config.performance.detect_games = view.pause_games.is_active();
    config.performance.pause_on_battery = view.pause_battery.is_active();
    config.performance.game_processes = parse_csv_list(&view.game_processes.text());
    config.performance.game_ignore_processes = parse_csv_list(&view.game_ignore.text());
    config.performance.pause_when_gpu_memory_above = gpu_above;
    config.performance.resume_when_gpu_memory_below = gpu_below;
    config.media.video_dmabuf_rgb = RGB_IDS[view.rgb.selected().min(2) as usize].to_owned();
    let vaapi = view.vaapi.text().trim().to_owned();
    config.media.video_vaapi = if vaapi.is_empty() {
        "auto".to_owned()
    } else {
        vaapi
    };
    if let Err(error) = config.validate() {
        app.toast(&format!("配置不合法：{error}"));
        return;
    }
    match config.save_atomic(&app.config_path) {
        Ok(bytes) => {
            view.dirty.set(false);
            view.apply.set_sensitive(false);
            view.status.set_text(&format!(
                "已保存 {} 字节到 {}",
                bytes,
                app.config_path.display()
            ));
            app.send(Cmd::Reload);
            app.send(Cmd::Config);
            app.send(Cmd::Sources);
            app.toast("设置已保存，守护进程正在重载");
        }
        Err(error) => {
            app.toast(&format!("写入配置失败：{error}"));
        }
    }
}

fn rgb_index(value: &str) -> u32 {
    match value {
        "always" => 1,
        "never" => 2,
        _ => 0,
    }
}

fn config_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// 自启 desktop 文件路径。
#[must_use]
pub fn autostart_path() -> Option<PathBuf> {
    config_home().map(|home| home.join("autostart").join("liuguang-gui.desktop"))
}

/// 当前是否已启用开机自启（文件存在且没被标记禁用）。
#[must_use]
pub fn autostart_enabled() -> bool {
    autostart_path().is_some_and(|path| path.exists())
}

/// 写/删自启文件。
pub fn set_autostart(enabled: bool) -> Result<PathBuf, String> {
    let path = autostart_path().ok_or_else(|| "没有 HOME/XDG_CONFIG_HOME".to_owned())?;
    if !enabled {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let exec = std::env::current_exe()
        .ok()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|| "liuguang-gui".to_owned());
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=流光\nName[zh_CN]=流光\nName[en_US]=Liuguang\nComment=流光 · 动态壁纸库与播放列表\nExec={exec}\nIcon=liuguang\nTerminal=false\nCategories=Graphics;\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path)
}

fn show_about(app: &Rc<App>) {
    let about = adw::AboutWindow::new();
    about.set_application_name("流光");
    about.set_application_icon(crate::APP_ICON);
    about.set_version(env!("CARGO_PKG_VERSION"));
    about.set_developer_name("流光（Liuguang）");
    about.set_license_type(gtk::License::MitX11);
    about.set_comments("GTK4 + libadwaita 的壁纸库：库 / 播放列表 / 显示器 / 设置。");
    about.set_website("https://github.com/suluoMISS/liuguang");
    about.set_transient_for(Some(&app.window));
    about.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_indices_map_both_ways() {
        assert_eq!(rgb_index("auto"), 0);
        assert_eq!(rgb_index("always"), 1);
        assert_eq!(rgb_index("never"), 2);
        assert_eq!(RGB_IDS[0], "auto");
    }

    #[test]
    fn autostart_path_is_under_config_home() {
        if let Some(path) = autostart_path() {
            assert!(path.ends_with("autostart/liuguang-gui.desktop"));
        }
    }

    #[test]
    fn csv_list_splits_trims_and_dedups() {
        assert_eq!(
            parse_csv_list(" factorio , Dota2，CS2, dota2 ,, "),
            vec!["factorio", "Dota2", "CS2"]
        );
        assert_eq!(
            format_csv_list(&["QQ".to_owned(), "wechat".to_owned()]),
            "QQ, wechat"
        );
    }
}
