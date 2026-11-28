//! 属性面板（右侧）：文件信息 + 适配参数 + "只放这一项" / 应用到输出。
//!
//! 面板永远显示当前选中项；没有选中时是一张引导页。适配参数的初值取自
//! 配置文件里该输出的存档（`fit/zoom/offset/rotation`），用户改完点"应用"
//! 才真正下发——这样面板本身是"预览 + 提交"，不会一边拖滑杆一边让壁纸乱跳。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use liuguang_core::config::Config;

use crate::state::State;
use crate::{App, ui};

/// 属性面板的控件集合。
pub struct DetailsView {
    pub root: gtk::Widget,
    pub stack: gtk::Stack,
    pub preview: gtk::Picture,
    pub title: gtk::Label,
    pub path: gtk::Label,
    pub current: gtk::Label,
    pub res: gtk::Label,
    pub fps: gtk::Label,
    pub duration: gtk::Label,
    pub codec: gtk::Label,
    pub pix_fmt: gtk::Label,
    pub size: gtk::Label,
    pub kind: gtk::Label,
    pub modified: gtk::Label,
    pub fit: gtk::DropDown,
    pub zoom: gtk::SpinButton,
    pub offset_x: gtk::SpinButton,
    pub offset_y: gtk::SpinButton,
    pub rotation: gtk::DropDown,
    pub brightness: gtk::Scale,
    pub brightness_value: gtk::Label,
    /// 亮度预设分段控件（外部值变化时同步高亮）。
    pub brightness_presets: Vec<gtk::Button>,
    pub output: gtk::DropDown,
    pub output_ids: RefCell<Vec<Option<String>>>,
    /// 当前回填进控件的是"哪个媒体的哪个输出"的存档值。
    ///
    /// 只有它变化（换选中项/换输出/配置下发/用户点重置）才重新读配置；
    /// Status/Media 的周期刷新只更新文件信息，否则用户改到一半的适配参数
    /// 会被后台刷新悄悄冲掉。
    pub loaded: RefCell<Option<(String, Option<String>)>>,
    /// 上一次真正回填进控件的存档值。
    ///
    /// 仅凭 `loaded` 判断不够：周期刷新/输出列表刷新可能带着**同一** key 重新
    /// 触发回填，但如果配置里的存档值在这期间被外部改过（设置页保存、daemon
    /// 重载），仍应更新面板。反过来，存档值没变时绝不覆盖用户未提交的编辑。
    pub last_filled: RefCell<Option<SavedTransform>>,
    /// 正在用配置回填控件（屏蔽实时联动推流）。
    pub live_syncing: std::cell::Cell<bool>,
    /// 实时亮度/变换去抖定时器（只保留未触发的 SourceId，触发前清掉避免 remove panic）。
    pub live_timer: RefCell<Option<glib::SourceId>>,
    pub reset: gtk::Button,
    /// 仅在需要转码/不支持/转码进行中显示的可操作说明区；可直播时整块收起，避免占空间。
    pub compat_section: gtk::Box,
    /// 兼容性状态角标（Playable 时隐藏）。
    pub compat: gtk::Label,
    /// 可直接播放的绿色角标（NeedsConversion/Unsupported 时隐藏）。
    pub compat_ok: gtk::Label,
    /// 兼容性原因 / 转码目标说明（完整文本，自动换行）。
    pub compat_note: gtk::Label,
    /// "准备（转码）"按钮：转码中原地显示百分比。
    pub prepare: gtk::Button,
    /// 转码进度条（0–1000 permille → 0.0–1.0）。
    pub progress: gtk::ProgressBar,
    /// 当前输出的解码告警 / 状态提示（完整文本，不截断）。
    pub alert: gtk::Label,
    /// "状态提示"整段（标题 + 正文 + 分隔线）：没有内容时整段隐藏，
    /// 否则面板底部会留一个空标题，看起来像少渲染了一块。
    pub alert_section: gtk::Box,
    pub status_badges: gtk::Box,
    pub apply: gtk::Button,
    pub favorite: gtk::Button,
    pub open: gtk::Button,
}

fn value_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("details-value");
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_selectable(true);
    label.set_wrap(true);
    label
}

fn info_grid() -> (gtk::Grid, [gtk::Label; 8], [&'static str; 8]) {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(6);
    grid.set_margin_top(4);
    let keys = [
        "分辨率",
        "帧率",
        "时长",
        "编码",
        "像素格式",
        "文件大小",
        "类型",
        "修改时间",
    ];
    let values: [gtk::Label; 8] = std::array::from_fn(|_| value_label());
    for (row, key) in keys.iter().enumerate() {
        let key_label = gtk::Label::new(Some(key));
        key_label.add_css_class("details-key");
        key_label.set_xalign(0.0);
        key_label.set_halign(gtk::Align::Start);
        grid.attach(&key_label, 0, i32::try_from(row).unwrap_or(0), 1, 1);
        grid.attach(&values[row], 1, i32::try_from(row).unwrap_or(0), 1, 1);
    }
    (grid, values, keys)
}

/// 构建属性面板。
pub fn build(app: &Rc<App>) -> Rc<DetailsView> {
    let preview_base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    preview_base.set_size_request(300, 168);
    preview_base.set_hexpand(true);
    preview_base.set_vexpand(true);

    let preview = gtk::Picture::new();
    preview.set_content_fit(gtk::ContentFit::Cover);
    preview.set_can_shrink(true);
    preview.set_hexpand(true);
    preview.set_vexpand(true);
    preview.set_halign(gtk::Align::Fill);
    preview.set_valign(gtk::Align::Fill);
    preview.set_paintable(Some(&ui::blank_texture(300, 168)));

    let preview_overlay = gtk::Overlay::new();
    preview_overlay.set_child(Some(&preview_base));
    preview_overlay.add_overlay(&preview);

    let title = gtk::Label::new(Some("未选择媒体"));
    title.add_css_class("hero-title");
    title.set_xalign(0.0);
    title.set_halign(gtk::Align::Start);
    title.set_wrap(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_lines(2);

    let path = gtk::Label::new(None);
    path.add_css_class("path-label");
    path.set_xalign(0.0);
    path.set_halign(gtk::Align::Start);
    path.set_selectable(true);
    path.set_wrap(true);

    let current = gtk::Label::new(Some("● 正在使用"));
    current.add_css_class("status-chip");
    current.add_css_class("live");
    current.set_xalign(0.0);
    current.set_halign(gtk::Align::Start);
    current.set_visible(false);

    let (grid, values, _keys) = info_grid();
    let [res, fps, duration, codec, pix_fmt, size, kind, modified] = values;
    for label in [
        &res, &fps, &duration, &codec, &pix_fmt, &size, &kind, &modified,
    ] {
        label.set_text("—");
    }

    // 实时亮度与适配参数。
    let brightness = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        liuguang_core::transform::MIN_BRIGHTNESS,
        liuguang_core::transform::MAX_BRIGHTNESS,
        0.05,
    );
    brightness.set_value(1.0);
    brightness.set_hexpand(true);
    brightness.set_draw_value(false);
    brightness.set_tooltip_text(Some(
        "实时压暗壁纸（100% = 原始亮度，GPU 着色器零重启生效）",
    ));
    let brightness_value = gtk::Label::new(Some("100%"));
    brightness_value.add_css_class("mono");
    brightness_value.add_css_class("small");
    brightness_value.set_width_chars(5);
    brightness_value.set_xalign(1.0);

    // 亮度预设：等宽分段控件（与滑杆双向同步；选中态只改样式，不反向拖滑杆）。
    let (brightness_chips_box, brightness_chips) =
        ui::segmented(&[("50%", 0.50f64), ("75%", 0.75f64), ("100%", 1.00f64)], {
            let scale = brightness.clone();
            move |value| scale.set_value(value)
        });
    brightness_chips_box.set_homogeneous(true);
    brightness_chips_box.set_halign(gtk::Align::Fill);
    brightness_chips_box.set_hexpand(true);
    ui::select_segment(&brightness_chips, 2);

    let brightness_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let brightness_label = gtk::Label::new(Some("亮度"));
    brightness_label.add_css_class("details-key");
    brightness_label.set_xalign(0.0);
    brightness_label.set_hexpand(true);
    brightness_head.append(&brightness_label);
    brightness_head.append(&brightness_value);

    let brightness_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    brightness_box.append(&brightness_head);
    brightness_box.append(&brightness);
    brightness_box.append(&brightness_chips_box);

    let fit = gtk::DropDown::from_strings(&ui::FIT_LABELS);
    fit.set_hexpand(true);
    fit.set_tooltip_text(Some("适配方式"));
    let zoom = gtk::SpinButton::with_range(0.25, 4.0, 0.05);
    zoom.set_digits(2);
    zoom.set_value(1.0);
    zoom.set_hexpand(true);
    zoom.set_tooltip_text(Some("缩放（1.0 = 原始）"));
    let offset_x = gtk::SpinButton::with_range(-4096.0, 4096.0, 8.0);
    offset_x.set_digits(0);
    offset_x.set_hexpand(true);
    offset_x.set_tooltip_text(Some("水平偏移（像素）"));
    let offset_y = gtk::SpinButton::with_range(-4096.0, 4096.0, 8.0);
    offset_y.set_digits(0);
    offset_y.set_hexpand(true);
    offset_y.set_tooltip_text(Some("垂直偏移（像素）"));
    let rotation = gtk::DropDown::from_strings(&["0°", "90°", "180°", "270°"]);
    rotation.set_hexpand(true);
    rotation.set_tooltip_text(Some("顺时针旋转"));

    let transform_grid = gtk::Grid::new();
    transform_grid.set_column_spacing(12);
    transform_grid.set_row_spacing(8);
    let fields: [(&str, &gtk::Widget); 5] = [
        ("适配", fit.upcast_ref()),
        ("缩放", zoom.upcast_ref()),
        ("水平偏移", offset_x.upcast_ref()),
        ("垂直偏移", offset_y.upcast_ref()),
        ("旋转", rotation.upcast_ref()),
    ];
    for (row, (label, widget)) in fields.iter().enumerate() {
        let key = gtk::Label::new(Some(label));
        key.add_css_class("details-key");
        key.set_xalign(0.0);
        key.set_halign(gtk::Align::Start);
        key.set_valign(gtk::Align::Center);
        let row = i32::try_from(row).unwrap_or(0);
        transform_grid.attach(&key, 0, row, 1, 1);
        transform_grid.attach(*widget, 1, row, 1, 1);
    }

    // 输出选择：第一项"全部输出"。
    let output = gtk::DropDown::from_strings(&["全部输出"]);
    output.set_tooltip_text(Some("应用到哪个输出（默认全部）"));

    let apply = ui::action_button("应用壁纸", true);
    apply.set_hexpand(true);
    apply.set_tooltip_text(Some("把当前选中媒体立即应用为桌面壁纸"));
    let favorite = ui::action_button("收藏", false);
    favorite.set_hexpand(true);
    favorite.set_tooltip_text(Some("加入收藏筛选"));
    let open = ui::action_button("打开目录", false);
    open.set_hexpand(true);
    open.set_tooltip_text(Some("在文件管理器中显示源文件"));
    let reset = ui::action_button("重置适配", false);
    reset.add_css_class("flat");
    reset.set_tooltip_text(Some("恢复默认适配、居中与 100% 亮度并立即生效"));

    // 兼容性：统一胶囊风格，按需显示。
    let compat = gtk::Label::new(None);
    compat.add_css_class("status-chip");
    compat.add_css_class("warn");
    compat.set_halign(gtk::Align::Start);
    compat.set_visible(false);

    let compat_ok = gtk::Label::new(Some("✓ 可直接播放"));
    compat_ok.add_css_class("status-chip");
    compat_ok.add_css_class("ok");
    compat_ok.set_halign(gtk::Align::Start);
    compat_ok.set_visible(false);

    let compat_note = gtk::Label::new(None);
    compat_note.add_css_class("small");
    compat_note.add_css_class("dim");
    compat_note.set_xalign(0.0);
    compat_note.set_halign(gtk::Align::Start);
    compat_note.set_wrap(true);
    compat_note.set_selectable(true);
    compat_note.set_visible(false);

    let prepare = ui::action_button("准备（转码）", false);
    prepare.set_tooltip_text(Some(
        "在后台转成可硬解的 8bit HEVC 缓存（源文件不改；完成后自动应用）",
    ));
    prepare.set_visible(false);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(false);
    progress.set_visible(false);

    let alert = gtk::Label::new(None);
    alert.add_css_class("alert-text");
    alert.add_css_class("small");
    alert.set_xalign(0.0);
    alert.set_halign(gtk::Align::Start);
    alert.set_wrap(true);
    alert.set_selectable(true);
    alert.set_visible(false);

    let primary_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    primary_actions.append(&apply);

    let secondary_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    secondary_actions.set_homogeneous(true);
    secondary_actions.append(&favorite);
    secondary_actions.append(&open);

    let meta_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    meta_box.set_margin_top(6);
    meta_box.append(&path);
    meta_box.append(&grid);

    let meta_expander = gtk::Expander::new(Some("文件详情与路径"));
    meta_expander.set_expanded(false);
    meta_expander.set_child(Some(&meta_box));

    let preview_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    preview_frame.add_css_class("hero-preview");
    preview_frame.set_overflow(gtk::Overflow::Hidden);
    preview_frame.set_size_request(300, 168);
    preview_frame.append(&preview_overlay);

    let status_badges = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status_badges.append(&current);
    status_badges.append(&compat_ok);
    status_badges.append(&compat);
    status_badges.set_visible(false);

    let compat_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    compat_section.append(&ui::section_title("兼容性提示"));
    compat_section.append(&compat_note);
    compat_section.append(&prepare);
    compat_section.append(&progress);
    compat_section.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    compat_section.set_visible(false);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_bottom(16);
    content.set_margin_start(14);
    content.set_margin_end(14);
    content.append(&preview_frame);
    content.append(&status_badges);
    content.append(&title);
    content.append(&primary_actions);
    content.append(&secondary_actions);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&compat_section);
    content.append(&ui::section_title("实时画质与适配"));
    content.append(&output);
    content.append(&brightness_box);
    content.append(&transform_grid);
    content.append(&reset);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let alert_section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    alert_section.append(&ui::section_title("状态提示"));
    alert_section.append(&alert);
    alert_section.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    alert_section.set_visible(false);
    content.append(&alert_section);
    content.append(&meta_expander);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("未选择媒体")
        .description("从库里点一张图片或视频，这里会显示它的属性。")
        .build();
    empty.set_vexpand(true);

    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("item"));
    stack.add_named(&empty, Some("empty"));
    stack.set_visible_child_name("empty");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("details-pane");
    root.set_width_request(332);
    root.set_hexpand(false);
    root.append(&stack);

    let view = Rc::new(DetailsView {
        root: root.upcast(),
        stack,
        preview,
        title,
        path,
        current,
        res,
        fps,
        duration,
        codec,
        pix_fmt,
        size,
        kind,
        modified,
        fit,
        zoom,
        offset_x,
        offset_y,
        rotation,
        brightness,
        brightness_value,
        brightness_presets: brightness_chips,
        output,
        output_ids: RefCell::new(vec![None]),
        loaded: RefCell::new(None),
        last_filled: RefCell::new(None),
        live_syncing: std::cell::Cell::new(false),
        live_timer: RefCell::new(None),
        reset,
        compat_section: compat_section.clone(),
        compat,
        compat_ok,
        compat_note,
        prepare,
        progress,
        alert,
        alert_section: alert_section.clone(),
        status_badges: status_badges.clone(),
        apply,
        favorite,
        open,
    });

    // 重置：恢复默认适配、居中与 100% 亮度，并立即通过实时管线推送。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.reset.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                let defaults = SavedTransform::default();
                fill_saved(&view, &defaults);
                *view.last_filled.borrow_mut() = Some(defaults);
                *view.loaded.borrow_mut() = app
                    .state
                    .borrow()
                    .selected
                    .clone()
                    .map(|id| (id, selected_output(&view)));
                schedule_live_push(&app, &view);
                app.toast("已重置为默认适配与亮度");
            }
        });
    }

    // 准备（转码）：只对 NeedsConversion 显示；进度由 refresh 原地更新。
    {
        let app = Rc::downgrade(app);
        view.prepare.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                let selected = app.state.borrow().selected.clone();
                if let Some(id) = selected {
                    app.prepare_media(&id);
                }
            }
        });
    }

    // 应用 / 收藏 / 打开目录。
    {
        let app = Rc::downgrade(app);
        view.apply.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                let selected = app.state.borrow().selected.clone();
                if let Some(id) = selected {
                    app.apply_media(&id, false);
                }
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        view.favorite.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                let selected = app.state.borrow().selected.clone();
                if let Some(id) = selected {
                    app.toggle_favorite(&id);
                }
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        view.open.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                let path = app
                    .state
                    .borrow()
                    .selected
                    .clone()
                    .and_then(|id| app.state.borrow().item(&id).map(|item| item.path.clone()));
                if let Some(path) = path {
                    ui::open_in_file_manager(&path);
                }
            }
        });
    }
    // 切换输出 ⇒ 载入该输出在配置里的适配参数。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.output.connect_selected_notify(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                sync_transform_if_needed(&app, &view);
            }
        });
    }
    // 实时无闪烁推送：拖动亮度或变换滑杆 65ms 去抖后即时生效，无需重开管线。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        let on_change = move || {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                view.brightness_value
                    .set_text(&format!("{:.0}%", view.brightness.value() * 100.0));
                let index = ui::nearest_index(&[0.50, 0.75, 1.00], view.brightness.value());
                ui::select_segment(&view.brightness_presets, index);
                if !view.live_syncing.get() {
                    schedule_live_push(&app, &view);
                }
            }
        };
        let c1 = on_change.clone();
        view.brightness.connect_value_changed(move |_| c1());
        let c2 = on_change.clone();
        view.fit.connect_selected_notify(move |_| c2());
        let c3 = on_change.clone();
        view.zoom.connect_value_changed(move |_| c3());
        let c4 = on_change.clone();
        view.offset_x.connect_value_changed(move |_| c4());
        let c5 = on_change.clone();
        view.offset_y.connect_value_changed(move |_| c5());
        view.rotation.connect_selected_notify(move |_| on_change());
    }

    view
}

fn schedule_live_push(app: &Rc<App>, view: &Rc<DetailsView>) {
    if let Some(prev) = view.live_timer.borrow_mut().take() {
        prev.remove();
    }
    let app_weak = Rc::downgrade(app);
    let view_weak = Rc::downgrade(view);
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(65), move || {
        let Some(view) = view_weak.upgrade() else {
            return;
        };
        view.live_timer.borrow_mut().take();
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        let has_active_media = app
            .state
            .borrow()
            .status
            .as_ref()
            .is_some_and(|st| st.channels.iter().any(|c| c.media.is_some()));
        if !has_active_media {
            return;
        }
        let snap = transform_snapshot(&view);
        let output = selected_output(&view);
        app.send(crate::ipc::Cmd::SetOutputTransform(Box::new(
            crate::ipc::TransformRequest {
                output,
                fit: snap.fit,
                zoom: snap.zoom,
                offset_x: snap.offset_x,
                offset_y: snap.offset_y,
                rotation: snap.rotation,
                brightness: snap.brightness,
            },
        )));
    });
    *view.live_timer.borrow_mut() = Some(id);
}

/// 选中输出名（"全部输出" = `None`）。
#[must_use]
pub fn selected_output(view: &DetailsView) -> Option<String> {
    let index = view.output.selected() as usize;
    view.output_ids.borrow().get(index).cloned().flatten()
}

/// 刷新输出下拉框（列表未变化时不重建模型；保持原选择且不误触发实时联动）。
pub fn sync_outputs(app: &Rc<App>, view: &Rc<DetailsView>) {
    let outputs = app.state.borrow().outputs.clone();
    let mut entries = vec![String::from("全部输出")];
    let mut ids: Vec<Option<String>> = vec![None];
    for output in &outputs {
        if !output.connected {
            continue;
        }
        entries.push(output.name.clone());
        ids.push(Some(output.name.clone()));
    }
    if *view.output_ids.borrow() == ids {
        return;
    }
    let current = selected_output(view);
    let prev = view.live_syncing.replace(true);
    let strings: Vec<&str> = entries.iter().map(String::as_str).collect();
    *view.output_ids.borrow_mut() = ids;
    view.output.set_model(Some(&gtk::StringList::new(&strings)));
    if let Some(current) = current {
        let matched = view
            .output_ids
            .borrow()
            .iter()
            .position(|id| id.as_deref() == Some(current.as_str()));
        if let Some(index) = matched {
            view.output.set_selected(u32::try_from(index).unwrap_or(0));
        }
    }
    view.live_syncing.set(prev);
}

/// `Rotation`（0/90/180/270 度）→ 协议里的角度值。
///
/// `Rotation::quarter_turns()` 是 0..=3 的象限数；daemon 的 `SetOutputTransform`
/// 收的是角度，直接下发会被拒绝（`rotation 必须是 0/90/180/270`），
/// 所以 GUI 在构造 `Cmd::Apply` 之前必须走这个换算。
#[must_use]
pub fn rotation_degrees(rotation: liuguang_core::transform::Rotation) -> u32 {
    rotation.quarter_turns() * 90
}

/// 配置里某个输出的适配参数快照（用于判断是否需要重新回填）。
#[derive(Debug, Clone, PartialEq)]
pub struct SavedTransform {
    pub fit: String,
    pub zoom: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub rotation: u32,
    pub brightness: f64,
}

impl Default for SavedTransform {
    fn default() -> Self {
        Self {
            fit: "cover".to_owned(),
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: 0,
            brightness: 1.0,
        }
    }
}

impl SavedTransform {
    fn from_output(saved: &liuguang_core::config::OutputConfig) -> Self {
        Self {
            fit: saved.fit.as_str().to_owned(),
            zoom: saved.zoom,
            offset_x: saved.offset_x,
            offset_y: saved.offset_y,
            rotation: rotation_degrees(saved.rotation),
            brightness: saved.brightness,
        }
    }
}

/// 面板「当前输出」：显式选择优先，否则跟随第一个通道（“全部输出”的落点）。
fn resolved_output(app: &App, view: &DetailsView) -> Option<String> {
    selected_output(view).or_else(|| {
        app.state
            .borrow()
            .status
            .as_ref()
            .and_then(|s| s.channels.first().map(|c| c.output.clone()))
    })
}

/// 某个输出在配置里的适配参数（无存档时回到默认值）。
fn archived_transform(app: &App, view: &DetailsView) -> SavedTransform {
    let output = resolved_output(app, view);
    let config = app.state.borrow().config.clone();
    if let (Some(name), Some(config)) = (output, config) {
        if let Some(saved) = config.outputs.get(&name) {
            return SavedTransform::from_output(saved);
        }
    }
    // 没有存档：回到默认（配置解析失败时也不要让面板停在陌生值上）。
    SavedTransform::default()
}

/// 把存档值写进控件。
fn fill_saved(view: &DetailsView, saved: &SavedTransform) {
    view.live_syncing.set(true);
    view.fit.set_selected(ui::fit_index(&saved.fit));
    view.zoom.set_value(saved.zoom);
    view.offset_x.set_value(saved.offset_x);
    view.offset_y.set_value(saved.offset_y);
    let rotation_index = match saved.rotation {
        90 => 1,
        180 => 2,
        270 => 3,
        _ => 0,
    };
    view.rotation.set_selected(rotation_index);
    let b = saved.brightness.clamp(
        liuguang_core::transform::MIN_BRIGHTNESS,
        liuguang_core::transform::MAX_BRIGHTNESS,
    );
    view.brightness.set_value(b);
    view.brightness_value
        .set_text(&format!("{:.0}%", b * 100.0));
    ui::select_segment(
        &view.brightness_presets,
        ui::nearest_index(&[0.50, 0.75, 1.00], b),
    );
    view.live_syncing.set(false);
}

/// 从配置里读该输出的适配参数并回填控件，同时记录 `last_filled`。
fn load_transform(app: &Rc<App>, view: &Rc<DetailsView>) {
    let saved = archived_transform(app, view);
    fill_saved(view, &saved);
    *view.last_filled.borrow_mut() = Some(saved);
}

/// 按需回填适配参数（`loaded` + `last_filled` 双门控）。
///
/// 只有以下情况才重新读配置：
/// * 面板当前回填的对象（选中项 + 输出）变了；
/// * 同一对象下配置里的存档值确实变了（设置页保存、daemon 重载）。
///
/// 单纯的周期刷新（Status/Media/Outputs，5–15 秒一次）不会覆盖用户未提交的编辑。
pub fn sync_transform_if_needed(app: &Rc<App>, view: &Rc<DetailsView>) {
    let Some(selected) = app.state.borrow().selected.clone() else {
        return;
    };
    let key = (selected, selected_output(view));
    let saved = archived_transform(app, view);
    let loaded = view.loaded.borrow().clone();
    let last_filled = view.last_filled.borrow().clone();
    if !needs_reload(loaded.as_ref(), last_filled.as_ref(), &key, &saved) {
        return;
    }
    load_transform(app, view);
    *view.loaded.borrow_mut() = Some(key);
}

/// 回填门控：对象变了，或同一对象下存档值变了。
fn needs_reload(
    loaded: Option<&(String, Option<String>)>,
    last_filled: Option<&SavedTransform>,
    key: &(String, Option<String>),
    saved: &SavedTransform,
) -> bool {
    loaded != Some(key) || last_filled != Some(saved)
}

/// 属性面板的兼容性区块：角标、原因、准备按钮、转码进度。
fn refresh_compat(app: &App, view: &DetailsView, id: &str) {
    use liuguang_ipc::protocol::{CompatState, PrepareState};

    let compat = app.compat.borrow().get(id).cloned().flatten();
    let prepare = app.prepare.borrow().get(id).cloned();

    match compat.as_ref().and_then(ui::compat_badge) {
        Some((text, class)) => {
            view.compat.set_text(text);
            for old in ["warn", "danger", "ok"] {
                view.compat.remove_css_class(old);
            }
            view.compat.add_css_class(class);
            view.compat.set_visible(true);
            view.compat_ok.set_visible(false);
        }
        None => {
            view.compat.set_visible(false);
            // 仅已明确取回 Playable 详情时显示绿色可播标记；待查或图片保留隐藏避免误报。
            view.compat_ok.set_visible(compat.is_some());
        }
    }
    let actionable_compat = compat
        .as_ref()
        .is_some_and(|info| info.state != CompatState::Playable);
    let note = if actionable_compat {
        compat.as_ref().map(ui::compat_tooltip).unwrap_or_default()
    } else {
        String::new()
    };
    view.compat_note.set_text(&note);
    view.compat_note.set_visible(!note.is_empty());
    view.compat_note
        .set_tooltip_text(if note.is_empty() { None } else { Some(&note) });

    let needs = prepare.is_some()
        || compat
            .as_ref()
            .is_some_and(|info| info.state == CompatState::NeedsConversion);
    let target = compat
        .as_ref()
        .and_then(|info| info.target.as_deref())
        .map(str::to_owned);
    view.prepare.set_visible(needs);
    if needs {
        let (label, busy, tip) = match &prepare {
            Some(result) => {
                let label =
                    ui::prepare_label(&result.state, result.progress_permille, target.as_deref());
                let done = matches!(result.state, PrepareState::Ready | PrepareState::Failed);
                let tip = result
                    .message
                    .clone()
                    .unwrap_or_else(|| "点击开始后台转码".to_owned());
                (label, done, tip)
            }
            None => (
                "准备（转码）".to_owned(),
                true,
                "在后台转成可硬解的 8bit HEVC 缓存（源文件不改）".to_owned(),
            ),
        };
        view.prepare.set_label(&label);
        view.prepare.set_sensitive(busy);
        view.prepare.set_tooltip_text(Some(&tip));
    }

    let running = prepare
        .as_ref()
        .is_some_and(|result| matches!(result.state, PrepareState::Queued | PrepareState::Running));
    view.progress.set_visible(running);
    if let Some(result) = &prepare {
        view.progress
            .set_fraction(f64::from(result.progress_permille.min(1000)) / 1000.0);
    }
    view.compat_section
        .set_visible(!note.is_empty() || needs || running);

    // Unsupported：禁用"应用壁纸"按钮并说明原因。
    let unsupported = compat
        .as_ref()
        .is_some_and(|info| info.state == CompatState::Unsupported);
    let reason = compat
        .as_ref()
        .map(ui::compat_tooltip)
        .unwrap_or_else(|| "这个素材无法播放".to_owned());
    view.apply.set_sensitive(!unsupported);
    view.apply
        .set_tooltip_text(if unsupported { Some(&reason) } else { None });
    view.status_badges.set_visible(
        view.current.is_visible() || view.compat_ok.is_visible() || view.compat.is_visible(),
    );
}

/// 状态提示区块：`decode_alert` + `state_hint` 全文（可选中、可换行）。
fn refresh_alert(app: &App, view: &DetailsView) {
    let status = app.state.borrow().status.clone();
    let Some(status) = status else {
        view.alert.set_visible(false);
        return;
    };
    let output = resolved_output(app, view);
    let mut parts: Vec<String> = Vec::new();
    for channel in &status.channels {
        if output.as_deref().is_some_and(|name| name != channel.output) {
            continue;
        }
        if let Some(video) = &channel.video {
            if let Some(alert) = &video.decode_alert {
                parts.push(alert.clone());
            }
        }
        if let Some(reason) = &channel.pause_reason {
            parts.push(format!("暂停原因：{}", ui::pause_reason_text(Some(reason))));
        }
    }
    if let Some(hint) = &status.state_hint {
        parts.push(hint.clone());
    }
    let text = parts.join("\n");
    view.alert.set_text(&text);
    view.alert.set_visible(!text.is_empty());
    view.alert_section.set_visible(!text.is_empty());
    view.alert
        .set_tooltip_text(if text.is_empty() { None } else { Some(&text) });
}

/// 同步属性面板顶部预览图（在切换选中或收到新缩略图时调用）。
pub fn refresh_preview(app: &Rc<App>, view: &DetailsView, id: &str) {
    if let Some(texture) = app.textures.borrow().get(&(id.to_owned(), 0u64)).cloned() {
        view.preview.set_paintable(Some(&texture));
    } else {
        view.preview
            .set_paintable(Some(&ui::blank_texture(312, 176)));
        app.request_thumb(id, 0);
    }
}

/// 刷新属性面板（选中项变化 / 状态刷新时调用）。
pub fn refresh(app: &Rc<App>, view: &Rc<DetailsView>) {
    let selected = app.state.borrow().selected.clone();
    let Some(id) = selected else {
        view.stack.set_visible_child_name("empty");
        return;
    };
    let item = app.state.borrow().item(&id).cloned();
    let Some(item) = item else {
        view.stack.set_visible_child_name("empty");
        return;
    };

    view.stack.set_visible_child_name("item");
    refresh_preview(app, view, &id);
    view.title.set_text(&item.name);
    view.path.set_text(&item.path);
    let is_current = app.state.borrow().current_media().as_deref() == Some(item.id.as_str());
    view.current.set_visible(is_current);
    view.current.set_text("● 正在使用");
    view.status_badges.set_visible(
        view.current.is_visible() || view.compat_ok.is_visible() || view.compat.is_visible(),
    );

    let details = item.details.as_ref();
    view.res
        .set_text(&details.map_or_else(|| "—".to_owned(), |d| format!("{}×{}", d.width, d.height)));
    view.fps
        .set_text(&details.and_then(|d| d.fps_millihz).map_or_else(
            || "—".to_owned(),
            |m| format!("{:.2} fps", f64::from(m) / 1000.0),
        ));
    view.duration.set_text(
        &details
            .and_then(|d| d.duration_ms)
            .map_or_else(|| "—".to_owned(), ui::duration_label),
    );
    view.codec.set_text(
        details
            .and_then(|d| d.codec.clone())
            .as_deref()
            .unwrap_or("—"),
    );
    view.pix_fmt.set_text(
        details
            .and_then(|d| d.pix_fmt.clone())
            .as_deref()
            .unwrap_or("—"),
    );
    view.size.set_text(&ui::human_size(item.size_bytes));
    view.kind.set_text(if item.kind == "video" {
        "视频"
    } else {
        "图片"
    });
    view.modified.set_text(&format_unix(item.modified_unix));

    // 兼容性：角标 + 原因/目标 + 准备按钮 + 进度。
    refresh_compat(app, view, &id);

    // 当前输出的解码告警 / 状态提示：完整显示，绝不截断。
    refresh_alert(app, view);

    let favorite = app.state.borrow().is_favorite(&id);
    view.favorite
        .set_label(if favorite { "取消收藏" } else { "收藏" });
    view.favorite.set_tooltip_text(Some(if favorite {
        "从收藏中移除"
    } else {
        "加入收藏筛选"
    }));

    // 只有"选中项 / 输出 / 配置存档值"变化时才回填适配参数；
    // 周期刷新保留用户未提交的编辑。
    sync_transform_if_needed(app, view);
}

/// 时间戳 → 本地可读时间（不引入 chrono：daemon 已经给了 unix 秒）。
fn format_unix(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_owned();
    }
    // 用 glib 拿本机当前 UTC 偏移，把 unix 秒换成本地时间再走 civil 算法。
    // 不引 chrono：偏移只要"现在"这一个值，且 GUI 本来就依赖 glib。
    let offset = local_offset_secs();
    let local = unix + offset;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC{sign}{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        abs / 3600,
        (abs % 3600) / 60
    )
}

/// 本机当前 UTC 偏移（秒）。取不到就按 UTC 处理。
fn local_offset_secs() -> i64 {
    glib::DateTime::now_local()
        .ok()
        .map_or(0, |now| now.utc_offset().as_seconds())
}

/// Howard Hinnant 的 days → civil 算法（无分支、无查表）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// 面板的可见性（偏好 `show_details`）。
pub fn set_visible(app: &Rc<App>, view: &Rc<DetailsView>, visible: bool) {
    view.root.set_visible(visible);
    app.state.borrow_mut().prefs.show_details = visible;
    app.save_prefs();
}

/// 选中项的适配参数快照（供 `App::apply_media` 读取）。
#[must_use]
pub fn transform_snapshot(view: &DetailsView) -> TransformSnapshot {
    let fit_index = usize::try_from(view.fit.selected()).unwrap_or(0);
    TransformSnapshot {
        fit: ui::FIT_IDS[fit_index.min(ui::FIT_IDS.len() - 1)].to_owned(),
        zoom: view.zoom.value(),
        offset_x: view.offset_x.value(),
        offset_y: view.offset_y.value(),
        rotation: ui::ROTATIONS[usize::try_from(view.rotation.selected())
            .unwrap_or(0)
            .min(3)],
        brightness: view.brightness.value(),
    }
}

/// 面板里当前的适配参数。
#[derive(Debug, Clone)]
pub struct TransformSnapshot {
    pub fit: String,
    pub zoom: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub rotation: u32,
    pub brightness: f64,
}

/// 应用配置到面板（设置页保存配置后调用）。
pub fn apply_config(app: &Rc<App>, view: &Rc<DetailsView>, config: &Config) {
    let _ = config;
    sync_transform_if_needed(app, view);
}

/// 状态栏/详情共用的媒体标签。
#[must_use]
#[allow(dead_code)]
pub fn media_label(state: &State, id: &str) -> String {
    state.item(id).map_or_else(
        || id.rsplit('/').next().unwrap_or(id).to_owned(),
        |i| i.name.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    #[test]
    fn unix_formatting_handles_zero() {
        assert_eq!(format_unix(0), "—");
        let text = format_unix(86_400);
        assert!(text.starts_with("1970-01-0"), "{text}");
        assert!(text.contains("UTC"), "要标出时区: {text}");
    }

    #[test]
    fn media_labels_fall_back_to_basename() {
        let state = State::new(std::env::temp_dir().join("nope.json"));
        assert_eq!(media_label(&state, "/a/b.mp4"), "b.mp4");
    }

    #[test]
    fn rotation_degrees_maps_quarter_turns_to_protocol_degrees() {
        use liuguang_core::transform::Rotation;
        assert_eq!(rotation_degrees(Rotation::Deg0), 0);
        assert_eq!(rotation_degrees(Rotation::Deg90), 90);
        assert_eq!(rotation_degrees(Rotation::Deg180), 180);
        assert_eq!(rotation_degrees(Rotation::Deg270), 270);
    }

    #[test]
    fn transform_reload_is_gated_by_key_and_archived_values() {
        let key = ("/a/b.mp4".to_owned(), Some("eDP-1".to_owned()));
        let saved = SavedTransform::default();
        // 还没回填过 ⇒ 必须回填。
        assert!(needs_reload(None, None, &key, &saved));
        // 同一对象、同一存档值 ⇒ 周期刷新不得覆盖未提交的编辑。
        assert!(!needs_reload(Some(&key), Some(&saved), &key, &saved));
        // 同一对象，但配置里的存档值变了（外部保存/重载）⇒ 回填。
        let mut changed = saved.clone();
        changed.fit = "contain".to_owned();
        assert!(needs_reload(Some(&key), Some(&saved), &key, &changed));
        // 换了选中项/输出 ⇒ 回填。
        let other = ("/a/c.png".to_owned(), None);
        assert!(needs_reload(Some(&key), Some(&saved), &other, &saved));
    }
}
