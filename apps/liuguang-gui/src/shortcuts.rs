//! 快捷键一览窗口。
//!
//! 这张表必须与 `main.rs` 里 `EventControllerKey` 的实际绑定一一对应——
//! 加/改快捷键时两处一起改，否则用户看到的说明会跟行为不符。
//!
//! 用普通窗口 + 列表自己排，而不是 `gtk::ShortcutsWindow`：后者的
//! section/group 装配 API 在 gtk4-rs 里与 GTK3 不同，且不易与应用的
//! CSS/字号保持一致。

use gtk::prelude::*;

/// 一条快捷键：`(分组, 按键显示, 说明)`。
const SHORTCUTS: &[(&str, &str, &str)] = &[
    (
        "媒体库",
        "Ctrl+F",
        "聚焦搜索框（若当前在其它页面则先切回媒体库；再按全选）",
    ),
    ("媒体库", "Esc", "清空搜索"),
    ("媒体库", "F5", "重新扫描媒体库"),
    ("播放", "Space", "暂停 / 继续（焦点在输入控件时不生效）"),
    ("播放", "→", "切到下一张壁纸（焦点在输入控件时不生效）"),
    (
        "播放",
        "←",
        "回到上一张壁纸（本次运行内，焦点在输入控件时不生效）",
    ),
    ("通用", "F1", "打开这份快捷键说明"),
];

/// 弹出快捷键窗口（`F1` 与「关于」页的按钮都走这里）。
pub fn show(parent: &impl IsA<gtk::Window>) {
    let window = gtk::Window::builder()
        .title("快捷键")
        .transient_for(parent)
        .modal(false)
        .default_width(520)
        .default_height(480)
        .build();

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("快捷键", "在这些按键之外，卡片上单击即应用");
    header.set_title_widget(Some(&title));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list.set_margin_top(16);
    list.set_margin_bottom(16);
    list.set_margin_start(16);
    list.set_margin_end(16);

    let mut last_group = "";
    for (group, keys, description) in SHORTCUTS {
        if *group != last_group {
            let heading = gtk::Label::new(Some(group));
            heading.set_xalign(0.0);
            heading.set_margin_top(if last_group.is_empty() { 0 } else { 14 });
            heading.add_css_class("dim");
            heading.add_css_class("small");
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_selectable(false);
            row.set_child(Some(&heading));
            list.append(&row);
            last_group = group;
        }
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.set_margin_top(8);
        line.set_margin_bottom(8);
        line.set_margin_start(6);
        line.set_margin_end(6);
        let description_label = gtk::Label::new(Some(description));
        description_label.set_xalign(0.0);
        description_label.set_hexpand(true);
        let key_label = gtk::Label::new(Some(keys));
        key_label.add_css_class("kbd-chip");
        key_label.set_xalign(1.0);
        line.append(&description_label);
        line.append(&key_label);
        row.set_child(Some(&line));
        list.append(&row);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&header);
    body.append(&scroller);
    window.set_child(Some(&body));
    window.present();
}
