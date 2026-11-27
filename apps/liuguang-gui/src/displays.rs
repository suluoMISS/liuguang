//! 显示器页：每个输出一张卡片（分辨率/刷新率/当前壁纸/暂停原因/上屏计数/CPU），
//! 以及"应用当前壁纸到此输出""下一张""取消只放这一项"。
//!
//! 卡片是**增量更新**的：`Status` 每秒来一次、`Metrics` 也每秒一次，
//! 每次重建整棵控件树会让滚动位置和按钮焦点丢失；这里只在输出集合变化时重建，
//! 其余情况只改标签文本。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::ipc::Cmd;
use crate::{App, ui};

/// 一个输出的卡片控件。
pub struct OutputCard {
    pub name: String,
    pub root: gtk::Widget,
    pub preview: gtk::Picture,
    pub media_id: RefCell<Option<String>>,
    pub state: gtk::Label,
    pub mode: gtk::Label,
    pub media: gtk::Label,
    pub reason: gtk::Label,
    pub perf: gtk::Label,
    pub apply: gtk::Button,
    pub next: gtk::Button,
    pub unpin: gtk::Button,
}

/// 显示器页。
pub struct DisplaysView {
    pub root: gtk::Widget,
    pub flow: gtk::FlowBox,
    pub cards: RefCell<Vec<Rc<OutputCard>>>,
    pub subtitle: gtk::Label,
    pub empty: gtk::Stack,
}

/// 构建显示器页。
pub fn build(app: &Rc<App>) -> Rc<DisplaysView> {
    let title = ui::section_title("显示器");
    let subtitle = gtk::Label::new(Some("等待守护进程报告通道…"));
    subtitle.add_css_class("dim");
    subtitle.add_css_class("small");
    subtitle.set_xalign(0.0);

    let refresh = ui::icon_button("view-refresh-symbolic", "立即刷新状态与指标");
    {
        let app = Rc::downgrade(app);
        refresh.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.send(Cmd::Status);
                app.send(Cmd::Outputs);
                app.send(Cmd::Metrics);
            }
        });
    }

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.set_margin_top(12);
    head.set_margin_start(16);
    head.set_margin_end(16);
    let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);
    titles.append(&subtitle);
    head.append(&titles);
    head.append(&refresh);

    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_valign(gtk::Align::Start);
    flow.set_halign(gtk::Align::Start);
    flow.set_column_spacing(14);
    flow.set_row_spacing(14);
    flow.set_margin_top(12);
    flow.set_margin_bottom(16);
    flow.set_margin_start(16);
    flow.set_margin_end(16);
    flow.set_max_children_per_line(4);
    flow.set_min_children_per_line(1);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&flow)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("video-display-symbolic")
        .title("还没有输出通道")
        .description("守护进程没有报告任何输出；确认它已经启动并连上了合成器。")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("outputs"));
    stack.add_named(&empty, Some("empty"));
    stack.set_visible_child_name("empty");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&head);
    body.append(&stack);

    Rc::new(DisplaysView {
        root: body.upcast(),
        flow,
        cards: RefCell::new(Vec::new()),
        subtitle,
        empty: stack,
    })
}

/// 用最新状态刷新卡片（`Status`/`Metrics`/`Outputs` 到达时调用）。
pub fn refresh(app: &Rc<App>, view: &Rc<DisplaysView>) {
    let state = app.state.borrow();
    let status = state.status.clone();
    let metrics = state.metrics.clone();
    let outputs = state.outputs.clone();
    drop(state);

    // 输出集合 = 通道并集 + 已连接但没通道的输出。
    let mut names: Vec<String> = Vec::new();
    if let Some(status) = &status {
        for channel in &status.channels {
            if !names.contains(&channel.output) {
                names.push(channel.output.clone());
            }
        }
    }
    for output in &outputs {
        if output.connected && !names.contains(&output.name) {
            names.push(output.name.clone());
        }
    }

    let current_names: Vec<String> = view.cards.borrow().iter().map(|c| c.name.clone()).collect();
    if current_names != names {
        while let Some(child) = view.flow.first_child() {
            view.flow.remove(&child);
        }
        let mut cards = Vec::new();
        for name in &names {
            let card = build_card(app, name);
            view.flow.append(&card.root);
            cards.push(card);
        }
        *view.cards.borrow_mut() = cards;
    }

    if names.is_empty() {
        view.empty.set_visible_child_name("empty");
        view.subtitle.set_text("没有输出通道");
        return;
    }
    view.empty.set_visible_child_name("outputs");

    let paused_count = status
        .as_ref()
        .map_or(0, |s| s.channels.iter().filter(|c| c.paused).count());
    let pinned = status
        .as_ref()
        .map_or(0, |s| s.channels.iter().filter(|c| c.pinned).count());
    let mut summary = format!("{} 个输出 · {} 个暂停", names.len(), paused_count);
    if pinned > 0 {
        summary.push_str(&format!(" · {} 个只放这一项", pinned));
    }
    view.subtitle.set_text(&summary);

    for card in view.cards.borrow().iter() {
        let channel = status.as_ref().and_then(|status| {
            status
                .channels
                .iter()
                .find(|channel| channel.output == card.name)
        });
        let output = outputs.iter().find(|output| output.name == card.name);
        update_card(app, card, channel, output, metrics.as_ref());
    }
}

fn update_card(
    app: &Rc<App>,
    card: &OutputCard,
    channel: Option<&liuguang_ipc::protocol::ChannelStatus>,
    output: Option<&liuguang_ipc::protocol::OutputStatus>,
    metrics: Option<&liuguang_ipc::protocol::MetricsResult>,
) {
    let (state_text, css) = match channel {
        Some(channel) if channel.paused => ("已暂停".to_owned(), "pill warn"),
        Some(channel) => (ui::state_text(&channel.state), "pill live"),
        None if output.is_some_and(|output| output.connected) => ("未接管".to_owned(), "pill"),
        None => ("已断开".to_owned(), "pill dim"),
    };
    card.state.set_text(&state_text);
    card.state.set_css_classes(&["pill"]);
    for class in css.split_whitespace().filter(|class| *class != "pill") {
        card.state.add_css_class(class);
    }

    let mode = channel
        .and_then(|channel| channel.mode.clone())
        .or_else(|| output.and_then(|output| output.mode.clone()));
    card.mode
        .set_text(&mode.unwrap_or_else(|| "未知模式".to_owned()));
    card.mode
        .set_tooltip_text(Some("分辨率与刷新率（来自当前显示模式）"));

    match channel.and_then(|channel| channel.media.clone()) {
        Some(path) => {
            *card.media_id.borrow_mut() = Some(path.clone());
            card.media.set_text(&basename(&path));
            card.media.set_tooltip_text(Some(&path));
            if let Some(texture) = app.textures.borrow().get(&(path.clone(), 0u64)).cloned() {
                card.preview.set_paintable(Some(&texture));
            } else {
                card.preview
                    .set_paintable(Some(&ui::blank_texture(320, 180)));
                app.request_thumb(&path, 0);
            }
        }
        None => {
            *card.media_id.borrow_mut() = None;
            card.media.set_text("（没有画面）");
            card.media.set_tooltip_text(None);
            card.preview
                .set_paintable(Some(&ui::blank_texture(320, 180)));
        }
    }

    let reason = channel
        .and_then(|channel| channel.pause_reason.as_deref())
        .map_or_else(
            || "未暂停".to_owned(),
            |reason| ui::pause_reason_text(Some(reason)),
        );
    card.reason.set_text(&reason);

    let fps = metrics.map_or(f64::NAN, |metrics| f64::from(metrics.fps));
    let cpu = metrics.map_or(f64::NAN, |metrics| f64::from(metrics.cpu_percent));
    let presented = metrics
        .and_then(|metrics| {
            metrics
                .channels
                .iter()
                .find(|channel| channel.output == card.name)
        })
        .map_or_else(
            || "上屏计数不可用".to_owned(),
            |m| format!("上屏 {}/{}", m.present_ok, m.presented),
        );
    let decoder = channel
        .map(|channel| channel.decoder.as_str())
        .unwrap_or("—");
    card.perf.set_text(&format!(
        "{presented} · {:.1} fps · CPU {:.0}% · {decoder}",
        if fps.is_nan() { 0.0 } else { fps },
        if cpu.is_nan() { 0.0 } else { cpu }
    ));

    let pinned = channel.is_some_and(|channel| channel.pinned);
    card.unpin.set_visible(pinned);
    let has_media = app.state.borrow().current_media().is_some();
    card.apply.set_sensitive(has_media);
}

fn build_card(app: &Rc<App>, name: &str) -> Rc<OutputCard> {
    let screen_base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    screen_base.set_size_request(320, 180);
    screen_base.set_hexpand(true);

    let preview = gtk::Picture::new();
    preview.set_content_fit(gtk::ContentFit::Cover);
    preview.set_can_shrink(true);
    preview.set_hexpand(true);
    preview.set_vexpand(true);
    preview.set_halign(gtk::Align::Fill);
    preview.set_valign(gtk::Align::Fill);
    preview.add_css_class("monitor-screen");
    preview.set_paintable(Some(&ui::blank_texture(320, 180)));

    let title = gtk::Label::new(Some(name));
    title.add_css_class("badge");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_tooltip_text(Some(name));

    let state = ui::pill("…");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.set_margin_top(8);
    head.set_margin_start(8);
    head.set_margin_end(8);
    head.set_valign(gtk::Align::Start);
    head.append(&title);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    head.append(&spacer);
    head.append(&state);

    let screen_overlay = gtk::Overlay::new();
    screen_overlay.set_overflow(gtk::Overflow::Hidden);
    screen_overlay.set_child(Some(&screen_base));
    screen_overlay.add_overlay(&preview);
    screen_overlay.add_overlay(&head);

    let bezel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bezel.add_css_class("monitor-bezel");
    bezel.append(&screen_overlay);

    let stand = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stand.add_css_class("monitor-stand");
    stand.set_halign(gtk::Align::Center);

    let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    base.add_css_class("monitor-base");
    base.set_halign(gtk::Align::Center);
    base.set_margin_bottom(4);

    let mode = gtk::Label::new(Some("—"));
    mode.add_css_class("mono");
    mode.add_css_class("small");
    mode.set_xalign(0.0);

    let media = gtk::Label::new(Some("—"));
    media.add_css_class("card-title");
    media.set_xalign(0.0);
    media.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let reason = gtk::Label::new(Some("未暂停"));
    reason.add_css_class("dim");
    reason.add_css_class("small");
    reason.set_xalign(0.0);

    let perf = gtk::Label::new(Some("…"));
    perf.add_css_class("mono");
    perf.add_css_class("small");
    perf.add_css_class("dim");
    perf.set_xalign(0.0);

    let apply = ui::action_button("应用当前壁纸", true);
    let next = ui::action_button("下一张", false);
    let unpin = ui::action_button("取消只放这一项", false);
    unpin.set_visible(false);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.append(&apply);
    actions.append(&next);
    actions.append(&unpin);

    let card_body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card_body.add_css_class("media-card");
    card_body.add_css_class("pad");
    card_body.set_size_request(360, -1);
    card_body.append(&bezel);
    card_body.append(&stand);
    card_body.append(&base);
    card_body.append(&media);
    card_body.append(&mode);
    card_body.append(&reason);
    card_body.append(&perf);
    card_body.append(&actions);

    {
        let app_ref = Rc::downgrade(app);
        let name = name.to_owned();
        apply.connect_clicked(move |_| {
            if let Some(app) = app_ref.upgrade() {
                let path = app.state.borrow().current_media();
                match path {
                    Some(path) => app.apply_media_to_output(&path, Some(&name)),
                    None => app.toast("当前没有正在播放的壁纸"),
                }
            }
        });
    }
    {
        let app_ref = Rc::downgrade(app);
        let name = name.to_owned();
        next.connect_clicked(move |_| {
            if let Some(app) = app_ref.upgrade() {
                app.send(Cmd::Next {
                    output: Some(name.clone()),
                });
            }
        });
    }
    {
        let app_ref = Rc::downgrade(app);
        let name = name.to_owned();
        unpin.connect_clicked(move |_| {
            if let Some(app) = app_ref.upgrade() {
                app.send(Cmd::SetPinned {
                    pinned: false,
                    output: Some(name.clone()),
                });
                app.toast(&format!("{name}：已取消“只放这一项”"));
            }
        });
    }

    Rc::new(OutputCard {
        name: name.to_owned(),
        root: card_body.upcast(),
        preview,
        media_id: RefCell::new(None),
        state,
        mode,
        media,
        reason,
        perf,
        apply,
        next,
        unpin,
    })
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basenames_strip_directories() {
        assert_eq!(basename("/a/b/c.mp4"), "c.mp4");
        assert_eq!(basename("c.mp4"), "c.mp4");
    }
}
