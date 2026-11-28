//! 播放列表页：左侧列表 + 右侧编辑器（条目拖拽排序、模式/间隔/结束行为/绑定输出）。
//!
//! 编辑器是**草稿**：所有修改只落在 `draft` 里，点"保存"才发 `update_playlist`。
//! 这样拖动排序、删条目不会每动一下就往守护进程打一次请求，也不会出现
//! "拖到一半守护进程已经开始按新顺序轮换"的半成品状态。

use gtk::{gdk, glib};

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use liuguang_ipc::protocol::Playlist;

use crate::ipc::{Cmd, PlaylistEdit};
use crate::{App, ui};

/// 播放列表页的控件集合。
pub struct PlaylistsView {
    pub root: gtk::Widget,
    pub list: gtk::ListBox,
    /// 左侧列表为空时显示的占位页（有播放列表就切回列表）。
    pub left_stack: gtk::Stack,
    pub create: gtk::Button,
    pub editor: gtk::Stack,
    pub name: gtk::Entry,
    pub entries: gtk::ListBox,
    pub add_selected: gtk::Button,
    pub clear_entries: gtk::Button,
    pub mode: gtk::DropDown,
    pub interval: gtk::SpinButton,
    pub on_end: gtk::DropDown,
    pub output: gtk::DropDown,
    pub output_ids: RefCell<Vec<Option<String>>>,
    pub no_repeat: gtk::Switch,
    pub save: gtk::Button,
    pub delete: gtk::Button,
    pub rename: gtk::Button,
    /// 当前编辑的播放列表 id（`None` = 草稿/未选）。
    pub current: RefCell<Option<String>>,
    /// 条目草稿（媒体 id，顺序即播放顺序）。
    pub draft: RefCell<Vec<String>>,
    /// 列表数据快照（id → 名称），用于渲染草稿条目。
    pub names: RefCell<std::collections::HashMap<String, String>>,
    /// 编辑器里有未保存修改（防止后台 15s/5s 刷新覆写用户草稿）。
    pub dirty: Cell<bool>,
    /// 正在由代码回填控件（不把程序设值当成用户修改，也不触发选行重载）。
    pub syncing: Cell<bool>,
}

/// 构建播放列表页。
pub fn build(app: &Rc<App>) -> Rc<PlaylistsView> {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    list.set_margin_top(6);
    list.set_margin_bottom(6);
    list.set_margin_start(6);
    list.set_margin_end(6);

    let create = ui::action_button("新建", true);
    let left_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    left_header.set_margin_top(10);
    left_header.set_margin_start(10);
    left_header.set_margin_end(10);
    let left_title = gtk::Label::new(Some("播放列表"));
    left_title.add_css_class("side-title");
    left_title.set_hexpand(true);
    left_title.set_xalign(0.0);
    left_header.append(&left_title);
    left_header.append(&create);

    let left_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let left_empty = adw::StatusPage::builder()
        .icon_name("view-list-symbolic")
        .title("还没有播放列表")
        .description("点右上角的「新建」开始")
        .build();
    let left_stack = gtk::Stack::new();
    left_stack.set_vexpand(true);
    left_stack.add_named(&left_scroller, Some("list"));
    left_stack.add_named(&left_empty, Some("empty"));
    left_stack.set_visible_child_name("empty");

    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.set_width_request(280);
    left.append(&left_header);
    left.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    left.append(&left_stack);

    // 编辑器。
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("播放列表名称"));
    name.set_hexpand(true);
    let rename = ui::action_button("改名", false);
    let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    name_row.append(&name);
    name_row.append(&rename);

    let entries = gtk::ListBox::new();
    entries.set_selection_mode(gtk::SelectionMode::None);
    entries.add_css_class("boxed-list");
    entries.set_valign(gtk::Align::Start);

    let add_selected = ui::action_button("加入所选媒体", false);
    add_selected.set_tooltip_text(Some("把库页当前选中的图片/视频加到列表末尾"));
    let clear_entries = ui::action_button("清空", false);
    let entries_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    entries_buttons.append(&add_selected);
    entries_buttons.append(&clear_entries);

    let mode = gtk::DropDown::from_strings(&["顺序播放", "随机播放"]);
    let interval = gtk::SpinButton::with_range(1.0, 86_400.0, 30.0);
    interval.set_digits(0);
    interval.set_value(600.0);
    interval.set_tooltip_text(Some("每项停留秒数（1–86400）"));
    let on_end = gtk::DropDown::from_strings(&["播完切下一个", "单曲循环"]);
    let no_repeat = gtk::Switch::new();
    no_repeat.set_active(true);
    no_repeat.set_valign(gtk::Align::Center);

    let output = gtk::DropDown::from_strings(&["不绑定（跟随默认）"]);

    let form = gtk::Grid::new();
    form.set_column_spacing(12);
    form.set_row_spacing(8);
    let rows: [(&str, &gtk::Widget); 5] = [
        ("顺序模式", mode.upcast_ref()),
        ("间隔（秒）", interval.upcast_ref()),
        ("视频结束", on_end.upcast_ref()),
        ("避免重复", no_repeat.upcast_ref()),
        ("绑定输出", output.upcast_ref()),
    ];
    for (row, (label, widget)) in rows.iter().enumerate() {
        let key = gtk::Label::new(Some(label));
        key.add_css_class("details-key");
        key.set_xalign(0.0);
        let row = i32::try_from(row).unwrap_or(0);
        form.attach(&key, 0, row, 1, 1);
        form.attach(*widget, 1, row, 1, 1);
    }

    let save = ui::action_button("保存", true);
    let delete = ui::action_button("删除", false);
    delete.add_css_class("destructive-action");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(&delete);
    actions.append(&save);

    let editor_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    editor_box.set_margin_top(12);
    editor_box.set_margin_bottom(16);
    editor_box.set_margin_start(14);
    editor_box.set_margin_end(14);
    editor_box.append(&name_row);
    editor_box.append(&ui::section_title("条目（拖动左侧手柄可排序）"));
    editor_box.append(&entries);
    editor_box.append(&entries_buttons);
    editor_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    editor_box.append(&ui::section_title("播放设置"));
    editor_box.append(&form);
    editor_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    editor_box.append(&actions);

    let editor_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&editor_box)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("view-list-symbolic")
        .title("还没有选择播放列表")
        .description("在左侧新建一个，或者点一个已有的开始编辑。")
        .build();

    let editor = gtk::Stack::new();
    editor.add_named(&editor_scroller, Some("editor"));
    editor.add_named(&empty, Some("empty"));
    editor.set_visible_child_name("empty");

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&left));
    paned.set_end_child(Some(&editor));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(280);

    let view = Rc::new(PlaylistsView {
        root: paned.upcast(),
        list,
        left_stack: left_stack.clone(),
        create,
        editor,
        name,
        entries,
        add_selected,
        clear_entries,
        mode,
        interval,
        on_end,
        output,
        output_ids: RefCell::new(vec![None]),
        no_repeat,
        save,
        delete,
        rename,
        current: RefCell::new(None),
        draft: RefCell::new(Vec::new()),
        names: RefCell::new(std::collections::HashMap::new()),
        dirty: Cell::new(false),
        syncing: Cell::new(false),
    });

    // 控件改动标脏（防止后台定时刷新覆盖草稿）。
    {
        let mark = {
            let view_down = Rc::downgrade(&view);
            move || {
                if let Some(view) = view_down.upgrade()
                    && !view.syncing.get()
                {
                    view.dirty.set(true);
                }
            }
        };
        let m = mark.clone();
        view.name.connect_changed(move |_| m());
        let m = mark.clone();
        view.mode.connect_selected_notify(move |_| m());
        let m = mark.clone();
        view.interval.connect_value_notify(move |_| m());
        let m = mark.clone();
        view.on_end.connect_selected_notify(move |_| m());
        let m = mark.clone();
        view.output.connect_selected_notify(move |_| m());
        let m = mark;
        view.no_repeat.connect_active_notify(move |_| m());
    }

    // 新建。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.create.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                prompt_name(
                    &app,
                    "新建播放列表",
                    "给播放列表起个名字",
                    "",
                    {
                        let app = Rc::downgrade(&app);
                        let view = Rc::downgrade(&view);
                        move |name| {
                            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                                return;
                            };
                            let edit = PlaylistEdit {
                                playlist_id: None,
                                name,
                                entries: Vec::new(),
                                mode: "sequential".to_owned(),
                                interval_seconds: 600,
                                on_video_end: "next".to_owned(),
                                output: None,
                                no_repeat: true,
                            };
                            app.send(Cmd::UpdatePlaylist(Box::new(edit)));
                            view.current.borrow_mut().take();
                        }
                    },
                );
            }
        });
    }

    // 改名。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.rename.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                let current = view.current.borrow().clone();
                let Some(current) = current else {
                    app.toast("先选中一个播放列表");
                    return;
                };
                let old = view.name.text().to_string();
                prompt_name(&app, "重命名播放列表", "新的名称", &old, {
                    let app = Rc::downgrade(&app);
                    let view = Rc::downgrade(&view);
                    move |name| {
                        let (Some(app), Some(_view)) = (app.upgrade(), view.upgrade()) else {
                            return;
                        };
                        let playlist = app
                            .state
                            .borrow()
                            .playlists
                            .iter()
                            .find(|p| p.id == current)
                            .cloned();
                        let Some(playlist) = playlist else {
                            app.toast("这个播放列表已经不存在了");
                            return;
                        };
                        let edit = PlaylistEdit {
                            playlist_id: Some(playlist.id.clone()),
                            name,
                            entries: playlist
                                .entries
                                .iter()
                                .map(|e| e.media_id.clone())
                                .collect(),
                            mode: playlist.mode.clone(),
                            interval_seconds: playlist.interval_seconds,
                            on_video_end: playlist.on_video_end.clone(),
                            output: playlist.output.clone(),
                            no_repeat: playlist.no_repeat,
                        };
                        app.send(Cmd::UpdatePlaylist(Box::new(edit)));
                    }
                });
            }
        });
    }

    // 保存。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.save.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                let name = view.name.text().trim().to_owned();
                if name.is_empty() {
                    app.toast("播放列表需要一个名字");
                    return;
                }
                let mode = if view.mode.selected() == 1 {
                    "random".to_owned()
                } else {
                    "sequential".to_owned()
                };
                let on_video_end = if view.on_end.selected() == 1 {
                    "loop".to_owned()
                } else {
                    "next".to_owned()
                };
                let output = {
                    let index = view.output.selected() as usize;
                    view.output_ids.borrow().get(index).cloned().flatten()
                };
                let edit = PlaylistEdit {
                    playlist_id: view.current.borrow().clone(),
                    name,
                    entries: view.draft.borrow().clone(),
                    mode,
                    interval_seconds: view.interval.value().round() as u32,
                    on_video_end,
                    output,
                    no_repeat: view.no_repeat.is_active(),
                };
                app.send(Cmd::UpdatePlaylist(Box::new(edit)));
                app.toast("正在保存播放列表…");
            }
        });
    }

    // 删除。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.delete.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                let current = view.current.borrow().clone();
                let Some(current) = current else {
                    app.toast("先选中一个播放列表");
                    return;
                };
                let name = view
                    .name
                    .text()
                    .trim()
                    .to_owned();
                // 删除是不可撤销的（虽然不碰媒体文件），必须二次确认。
                let dialog = adw::AlertDialog::builder()
                    .heading("删除这个播放列表？")
                    .body(format!(
                        "将删除播放列表「{name}」的定义。\n\n媒体文件不会被删除，之后仍可在媒体库里找到它们。"
                    ))
                    .build();
                dialog.add_response("cancel", "取消");
                dialog.add_response("delete", "删除");
                dialog.set_response_appearance(
                    "delete",
                    adw::ResponseAppearance::Destructive,
                );
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let weak = Rc::downgrade(&app);
                dialog.connect_response(None, move |_, response| {
                    if response != "delete" {
                        return;
                    }
                    if let Some(app) = weak.upgrade() {
                        app.send(Cmd::DeletePlaylist {
                            playlist_id: current.clone(),
                        });
                    }
                });
                dialog.present(Some(&app.window));
            }
        });
    }

    // 加入所选媒体 / 清空。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.add_selected.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                let selected = app.state.borrow().selected.clone();
                let Some(id) = selected else {
                    app.toast("先在库页选一张图片或视频");
                    return;
                };
                if view.draft.borrow().iter().any(|entry| entry == &id) {
                    app.toast("这一项已经在列表里了");
                    return;
                }
                view.draft.borrow_mut().push(id);
                view.dirty.set(true);
                render_entries(&app, &view);
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.clear_entries.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                view.draft.borrow_mut().clear();
                view.dirty.set(true);
                render_entries(&app, &view);
            }
        });
    }
    // 列表选中 → 载入编辑器（忽略代码回填引发的选中事件）。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        view.list.connect_row_selected(move |_, row| {
            if let (Some(app), Some(view), Some(row)) = (app.upgrade(), view_down.upgrade(), row) {
                if view.syncing.get() {
                    return;
                }
                if let Some(id) = row.widget_name().strip_prefix("playlist:") {
                    let id = id.to_owned();
                    load_playlist(&app, &view, &id);
                }
            }
        });
    }

    view
}

/// 刷新播放列表列表（`Upd::Playlists` 到达时调用）。
pub fn refresh(app: &Rc<App>, view: &Rc<PlaylistsView>) {
    let playlists = app.state.borrow().playlists.clone();
    {
        let mut names = view.names.borrow_mut();
        names.clear();
        for playlist in &playlists {
            for entry in &playlist.entries {
                names
                    .entry(entry.media_id.clone())
                    .or_insert_with(|| short_name(&entry.media_id));
            }
        }
    }

    let was_syncing = view.syncing.get();
    view.syncing.set(true);
    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }
    view.left_stack
        .set_visible_child_name(if playlists.is_empty() {
            "empty"
        } else {
            "list"
        });
    let current = view.current.borrow().clone();
    for playlist in &playlists {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&format!("playlist:{}", playlist.id));
        row.set_activatable(true);

        let title = gtk::Label::new(Some(&playlist.name));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let meta = gtk::Label::new(Some(&summarize(playlist)));
        meta.add_css_class("dim");
        meta.add_css_class("small");
        meta.set_xalign(0.0);
        meta.add_css_class("mono");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_margin_top(6);
        text.set_margin_bottom(6);
        text.set_margin_start(10);
        text.set_margin_end(10);
        text.append(&title);
        text.append(&meta);
        row.set_child(Some(&text));
        view.list.append(&row);
        if current.as_deref() == Some(playlist.id.as_str()) {
            view.list.select_row(Some(&row));
        }
    }
    view.syncing.set(was_syncing);

    // 输出下拉（保持"不绑定"在第一项并保留当前选择）。
    sync_outputs(app, view);

    // 当前编辑的播放列表被删掉了 ⇒ 清空编辑器；若仍在且未标脏，同步守护进程最新内容。
    let current = view.current.borrow().clone();
    if let Some(current) = current {
        if !playlists.iter().any(|p| p.id == current) {
            view.current.borrow_mut().take();
            view.dirty.set(false);
            view.editor.set_visible_child_name("empty");
        } else if !view.dirty.get() {
            load_playlist(app, view, &current);
        }
    }

    // 默认打开第一个播放列表：进页面就能看到条目，而不是先点一下。
    if view.current.borrow().is_none() {
        if let Some(first) = playlists.first() {
            load_playlist(app, view, &first.id);
            let wanted = format!("playlist:{}", first.id);
            let was_syncing = view.syncing.get();
            view.syncing.set(true);
            let mut child = view.list.first_child();
            while let Some(widget) = child {
                let next = widget.next_sibling();
                if widget.widget_name() == wanted {
                    if let Ok(row) = widget.clone().downcast::<gtk::ListBoxRow>() {
                        view.list.select_row(Some(&row));
                    }
                    break;
                }
                child = next;
            }
            view.syncing.set(was_syncing);
        }
    }
}

/// 载入一个播放列表到编辑器。
pub fn load_playlist(app: &Rc<App>, view: &Rc<PlaylistsView>, id: &str) {
    let playlist = app
        .state
        .borrow()
        .playlists
        .iter()
        .find(|p| p.id == id)
        .cloned();
    let Some(playlist) = playlist else { return };
    let was_syncing = view.syncing.get();
    view.syncing.set(true);
    *view.current.borrow_mut() = Some(playlist.id.clone());
    view.name.set_text(&playlist.name);
    *view.draft.borrow_mut() = playlist
        .entries
        .iter()
        .map(|e| e.media_id.clone())
        .collect();
    view.mode
        .set_selected(if playlist.mode.as_str() == "random" {
            1
        } else {
            0
        });
    view.interval
        .set_value(f64::from(playlist.interval_seconds));
    view.on_end
        .set_selected(if playlist.on_video_end == "loop" {
            1
        } else {
            0
        });
    view.no_repeat.set_active(playlist.no_repeat);
    if let Some(output) = &playlist.output {
        if let Some(index) = view
            .output_ids
            .borrow()
            .iter()
            .position(|id| id.as_deref() == Some(output.as_str()))
        {
            view.output.set_selected(u32::try_from(index).unwrap_or(0));
        } else {
            view.output.set_selected(0);
        }
    } else {
        view.output.set_selected(0);
    }
    view.editor.set_visible_child_name("editor");
    render_entries(app, view);
    view.dirty.set(false);
    view.syncing.set(was_syncing);
}

/// 同步输出列表下拉选项，同时尽量保留用户当前选中的输出（包括暂未连接的输出）。
pub fn sync_outputs(app: &Rc<App>, view: &Rc<PlaylistsView>) {
    let prev_selected = {
        let index = view.output.selected() as usize;
        view.output_ids.borrow().get(index).cloned().flatten()
    };
    let bound_selected = view.current.borrow().as_ref().and_then(|cur_id| {
        app.state
            .borrow()
            .playlists
            .iter()
            .find(|p| &p.id == cur_id)
            .and_then(|p| p.output.clone())
    });
    let preserve = prev_selected.clone().or(bound_selected);

    let outputs = app.state.borrow().outputs.clone();
    let mut entries = vec![String::from("不绑定（跟随默认）")];
    let mut ids: Vec<Option<String>> = vec![None];
    for output in &outputs {
        if output.connected {
            entries.push(output.name.clone());
        } else {
            entries.push(format!("{}（未连接）", output.name));
        }
        ids.push(Some(output.name.clone()));
    }
    if let Some(sel) = preserve.as_deref()
        && !ids.iter().any(|id| id.as_deref() == Some(sel))
    {
        entries.push(format!("{sel}（未连接）"));
        ids.push(Some(sel.to_owned()));
    }

    let model_same = *view.output_ids.borrow() == ids
        && view
            .output
            .model()
            .and_then(|m| m.downcast::<gtk::StringList>().ok())
            .is_some_and(|list| {
                list.n_items() as usize == entries.len()
                    && entries
                        .iter()
                        .enumerate()
                        .all(|(i, s)| list.string(i as u32).as_deref() == Some(s.as_str()))
            });

    let was_syncing = view.syncing.get();
    view.syncing.set(true);
    if !model_same {
        let strings: Vec<&str> = entries.iter().map(String::as_str).collect();
        view.output.set_model(Some(&gtk::StringList::new(&strings)));
        *view.output_ids.borrow_mut() = ids.clone();
    }
    let restored = prev_selected
        .as_deref()
        .and_then(|sel| ids.iter().position(|id| id.as_deref() == Some(sel)))
        .unwrap_or(0);
    view.output
        .set_selected(u32::try_from(restored).unwrap_or(0));
    view.syncing.set(was_syncing);
}

/// 重绘条目草稿（不触达守护进程）。
pub fn render_entries(app: &Rc<App>, view: &Rc<PlaylistsView>) {
    while let Some(child) = view.entries.first_child() {
        view.entries.remove(&child);
    }
    let entries = view.draft.borrow().clone();
    let names = view.names.borrow().clone();
    for (index, media_id) in entries.iter().enumerate() {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);

        let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
        handle.add_css_class("drag-handle");
        handle.set_tooltip_text(Some("拖动排序"));

        let order = gtk::Label::new(Some(&format!("#{}", index + 1)));
        order.add_css_class("dim");
        order.add_css_class("mono");
        order.add_css_class("small");
        order.set_width_chars(3);

        let name = gtk::Label::new(Some(
            names
                .get(media_id)
                .map_or_else(|| short_name(media_id), Clone::clone)
                .as_str(),
        ));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

        let remove = gtk::Button::from_icon_name("list-remove-symbolic");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some("从列表移除"));
        {
            let app = Rc::downgrade(app);
            let view_down = Rc::downgrade(view);
            let media_id = media_id.clone();
            remove.connect_clicked(move |_| {
                if let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) {
                    view.draft.borrow_mut().retain(|id| id != &media_id);
                    view.dirty.set(true);
                    render_entries(&app, &view);
                }
            });
        }

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.set_margin_top(4);
        line.set_margin_bottom(4);
        line.set_margin_start(8);
        line.set_margin_end(8);
        line.append(&handle);
        line.append(&order);
        line.append(&name);
        line.append(&remove);
        row.set_child(Some(&line));

        // 拖拽源：把草稿下标包成 ContentProvider。
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::MOVE);
        let index_value = i32::try_from(index).unwrap_or(0);
        source.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&index_value.to_value()))
        });
        row.add_controller(source);

        // 拖放目标：把源下标移动到本行。
        let target = gtk::DropTarget::new(glib::Type::I32, gdk::DragAction::MOVE);
        let app_drop = Rc::downgrade(app);
        let view_drop = Rc::downgrade(view);
        target.connect_drop(move |_, value, _x, _y| {
            let (Some(app), Some(view)) = (app_drop.upgrade(), view_drop.upgrade()) else {
                return false;
            };
            let Some(from) = value.get::<i32>().ok() else {
                return false;
            };
            let from = usize::try_from(from).unwrap_or(0);
            reorder(&app, &view, from, index);
            true
        });
        row.add_controller(target);

        view.entries.append(&row);
    }
    if entries.is_empty() {
        let hint = gtk::Label::new(Some("列表还是空的：在库页选中媒体，点“加入所选媒体”。"));
        hint.add_css_class("dim");
        hint.add_css_class("small");
        hint.set_margin_top(6);
        hint.set_margin_bottom(6);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&hint));
        view.entries.append(&row);
    }
}

/// 草稿重排（拖拽 / 上下按钮共用）。
pub fn reorder(app: &Rc<App>, view: &Rc<PlaylistsView>, from: usize, to: usize) {
    {
        let mut draft = view.draft.borrow_mut();
        if from >= draft.len() {
            return;
        }
        let item = draft.remove(from);
        let to = to.min(draft.len());
        draft.insert(to, item);
    }
    view.dirty.set(true);
    render_entries(app, view);
}

/// 提示输入名称的小对话框（`adw::AlertDialog` + `Entry`）。
fn prompt_name<F>(app: &Rc<App>, heading: &str, body: &str, initial: &str, on_ok: F)
where
    F: Fn(String) + 'static,
{
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "取消");
    dialog.add_response("ok", "确定");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let on_ok = Rc::new(on_ok);
    dialog.connect_response(None, move |_dialog, response| {
        if response != "ok" {
            return;
        }
        let text = entry_text(&entry);
        if !text.trim().is_empty() {
            on_ok(text.trim().to_owned());
        }
    });
    dialog.present(Some(&app.window));
}

fn entry_text(entry: &gtk::Entry) -> String {
    entry.text().to_string()
}

fn short_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// 模式名（协议里是 `sequential` / `random` 字符串）。
#[must_use]
pub fn mode_label(mode: &str) -> &'static str {
    match mode {
        "random" => "随机",
        _ => "顺序",
    }
}

/// 播放列表条目数摘要（测试用）。
#[must_use]
pub fn summarize(playlist: &Playlist) -> String {
    format!(
        "{} 项 · {} 秒 · {}",
        playlist.entries.len(),
        playlist.interval_seconds,
        mode_label(&playlist.mode)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_strip_directories() {
        assert_eq!(short_name("/a/b/c.mp4"), "c.mp4");
        assert_eq!(short_name("c.mp4"), "c.mp4");
    }

    #[test]
    fn mode_labels_are_localized() {
        assert_eq!(mode_label("sequential"), "顺序");
        assert_eq!(mode_label("random"), "随机");
    }
}
