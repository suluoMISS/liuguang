//! 库页：网格 / 列表双视图 + 筛选排序 + 卡片悬停动效。
//!
//! 封面是**按可见性懒加载**的（t40）：重建卡片时只画缓存里已有的图，不向守护
//! 进程要任何缩略图；等控件布局完成后再算出当前视口内的卡片，最多请求 12 张
//! `at_ms=0` 封面。滚动到新区域时再补请求——38 项全量解码会把 daemon 打满
//! （实测 GUI 一打开 CPU 就从 10% 冲到 99%，持续约 50s）。
//!
//! 悬停动效同样限量：指针停留 ≥300ms 才启动预热，只取 2 帧（1s / 2s），
//! 离开立刻取消未完成的请求并回到封面帧。反复悬停同一张卡不会重复请求
//! 已经缓存的帧。

use gtk::glib;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use liuguang_ipc::protocol::MediaItem;

use crate::ipc::Cmd;
use crate::state;
use crate::{App, grid, ui};

/// 悬停预览的取样时刻（毫秒）：只 2 帧，1s / 2s。
pub const HOVER_PREVIEW_TIMES: [u64; 2] = [1000, 2000];

/// 悬停多久才启动预览（毫秒）。快速扫过卡片不该触发任何缩略图请求。
pub const HOVER_DELAY_MS: u64 = 300;

/// 单次可见性刷新最多请求多少张封面。
/// 单次可见性刷新最多请求多少张封面。
///
/// 视口里通常有 12–18 张（受卡片尺寸滑杆影响）；预算小于可见数时，排在最后
/// （按文件大小排序即"最大"）的卡片会一直空着，看起来像坏了。24 覆盖两列与
/// 三列布局的所有可见卡片，而滚动才会追加更多，不会把整库都拉起来解码。
pub const VISIBLE_THUMB_BUDGET: usize = 24;

/// 视口上下各留多少像素作为预取余量。
const VISIBLE_MARGIN: f64 = 240.0;

/// 库页的控件集合。
///
/// 由 `App` 持有；重建卡片时只动 `grid` / `list` 的子项，工具栏本身不重建
/// （否则滑杆拖到一半会因为控件被换掉而丢焦点）。
pub struct LibraryView {
    pub root: gtk::Widget,
    /// 媒体库专属工具栏（排序/筛选/卡片尺寸/计数）：切到别的页面时隐藏。
    pub toolbar: gtk::Box,
    /// content / empty 两页。
    pub outer: gtk::Stack,
    /// grid / list 两页。
    pub views: gtk::Stack,
    /// Win10 比例咬合 2D 磁贴网格。
    pub grid: gtk::Grid,
    /// 上次排布用的容器宽度：宽度变化时才重排（避免每次滚动都重算）。
    pub grid_width: Cell<i32>,
    pub list: gtk::ListBox,
    /// 两个滚动容器：可见性计算要读它们的 `vadjustment`。
    pub grid_scroller: gtk::ScrolledWindow,
    pub list_scroller: gtk::ScrolledWindow,
    pub empty: adw::StatusPage,
    pub empty_button: gtk::Button,
    pub count: gtk::Label,
    pub size_box: gtk::Box,
    pub size: gtk::Scale,
    /// 当前视口（含预取余量）内的媒体 id：只有这些卡片的封面回包会被采用。
    pub wanted: RefCell<HashSet<String>>,
    /// 是否已经排了一次可见性刷新（避免滚动时堆叠定时器）。
    pub refresh_scheduled: Cell<bool>,
    /// 布局刚开始时 `compute_bounds` 可能全为 `None`：有限次重试，等卡片真正分配尺寸。
    pub refresh_retries: Cell<u8>,
    /// 用户是否滚动过：没有滚动时首屏只加载 12 张（t40 预算），滚动后按需继续。
    pub user_scrolled: Cell<bool>,
    /// 首屏已经发过多少张封面请求（含在途），用于把冷启动请求卡在 12 以内。
    pub initial_requests: Cell<u32>,
}

/// 一张卡片（网格）或一行（列表）的运行期状态。
pub struct Card {
    pub id: String,
    pub root: gtk::Widget,
    pub picture: gtk::Picture,
    pub skeleton: Option<gtk::Widget>,
    /// 卡片上的"正在使用"徽标（常驻控件，按当前壁纸显隐，避免重建）。
    pub live: gtk::Label,
    /// 兼容性角标（`需转码` / `不支持`，Playable 时隐藏）。
    pub compat: gtk::Label,
    /// "准备（转码）"按钮：只有 NeedsConversion 时显示，跑起来后原地显示进度。
    pub prepare: gtk::Button,
    pub hover: Hover,
}

/// 悬停轮播状态。
#[derive(Default)]
pub struct Hover {
    /// 轮播定时器（每 500ms 切一帧）。
    timer: RefCell<Option<glib::SourceId>>,
    /// 300ms 启动延迟的定时器。
    delay: RefCell<Option<glib::SourceId>>,
    /// 代次号：延迟回调触发时若代次变了（指针已经离开），直接丢弃。
    generation: Cell<u64>,
    frame: Cell<usize>,
    active: Cell<bool>,
}

impl Hover {
    fn stop(&self) {
        self.active.set(false);
        // 轮播源是重复源，只要还在就安全 remove（回调 Break 前已把 slot 清空）。
        if let Some(source) = self.timer.borrow_mut().take() {
            source.remove();
        }
        // 延迟源是**一次性**源：触发后它已自我销毁，再按 id `remove()` 会 panic
        // （glib 的 `SourceId::remove()` 内部 unwrap）。这里只丢 id，迟到的回调靠
        // 代次号作废——见 `cancel_hover` 里的 `generation` 自增。
        crate::ui::cancel_once(&self.delay);
        self.frame.set(0);
    }

    fn is_active(&self) -> bool {
        self.active.get()
    }

    fn frame(&self) -> usize {
        self.frame.get()
    }
}

/// 构建库页。
pub fn build(app: &Rc<App>) -> Rc<LibraryView> {
    let outer = gtk::Stack::new();
    outer.set_transition_type(gtk::StackTransitionType::Crossfade);
    outer.set_transition_duration(180);

    // 曜晶画报墙：3px 极细拼接缝 + 蒙德里安自适应比例大中小格网。
    let grid = gtk::Grid::new();
    grid.set_row_spacing(3);
    grid.set_column_spacing(3);
    grid.set_column_homogeneous(true);
    grid.set_row_homogeneous(true);
    grid.add_css_class("tile-grid");
    grid.set_valign(gtk::Align::Start);
    grid.set_halign(gtk::Align::Start);
    grid.set_hexpand(false);
    grid.set_margin_top(4);
    grid.set_margin_bottom(4);
    grid.set_margin_start(4);
    grid.set_margin_end(4);

    let grid_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .min_content_width(280)
        .hexpand(true)
        .vexpand(true)
        .child(&grid)
        .build();
    grid_scroller.add_css_class("tile-wall-scroller");

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_valign(gtk::Align::Start);
    list.add_css_class("boxed-list");
    list.set_margin_top(6);
    list.set_margin_bottom(6);
    list.set_margin_start(6);
    list.set_margin_end(6);

    let list_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let views = gtk::Stack::new();
    views.set_transition_type(gtk::StackTransitionType::Crossfade);
    views.set_transition_duration(150);
    views.add_named(&grid_scroller, Some("grid"));
    views.add_named(&list_scroller, Some("list"));

    let empty_button = gtk::Button::with_label("重新扫描");
    empty_button.add_css_class("pill");
    empty_button.set_halign(gtk::Align::Center);
    let empty = adw::StatusPage::builder()
        .icon_name("folder-pictures-symbolic")
        .title("媒体库是空的")
        .child(&empty_button)
        .build();
    empty.set_vexpand(true);

    outer.add_named(&views, Some("content"));
    outer.add_named(&empty, Some("empty"));
    outer.set_visible_child_name("empty");

    // 工具栏（图 2 的排列）：排序胶囊 → 筛选胶囊 → 视图切换图标 → 尺寸滑杆 → 计数。
    let (sort_button, sort_label) = ui::toolbar_pill(
        "view-sort-descending-symbolic",
        state::SORTS[app.state.borrow().prefs.sort.min(state::SORTS.len() - 1)],
    );
    let sort_popover = gtk::Popover::new();
    {
        let list = gtk::ListBox::new();
        list.add_css_class("menu-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        for (index, name) in state::SORTS.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.0);
            label.set_margin_top(6);
            label.set_margin_bottom(6);
            label.set_margin_start(8);
            label.set_margin_end(8);
            row.set_child(Some(&label));
            row.set_widget_name(&format!("sort:{index}"));
            list.append(&row);
        }
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        sort_popover.set_child(Some(&list));
        let app_w = Rc::downgrade(app);
        let popover = sort_popover.clone();
        let label = sort_label.clone();
        list.connect_row_activated(move |_, row| {
            let Some(app) = app_w.upgrade() else {
                return;
            };
            let Some(index) = row
                .widget_name()
                .strip_prefix("sort:")
                .and_then(|raw| raw.parse::<usize>().ok())
            else {
                return;
            };
            popover.popdown();
            if app.state.borrow().prefs.sort == index {
                return;
            }
            app.state.borrow_mut().prefs.sort = index;
            label.set_text(state::SORTS[index]);
            app.save_prefs();
            if let Some(view) = app.library.borrow().clone() {
                rebuild(&app, &view);
            }
        });
    }
    sort_button.set_popover(Some(&sort_popover));
    sort_button.set_tooltip_text(Some("排序方式"));

    let (filter_button, filter_label) = ui::toolbar_pill(
        "view-filter-symbolic",
        state::FILTERS[app
            .state
            .borrow()
            .prefs
            .filter
            .min(state::FILTERS.len() - 1)],
    );
    let filter_popover = gtk::Popover::new();
    {
        let list = gtk::ListBox::new();
        list.add_css_class("menu-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        for (index, name) in state::FILTERS.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.0);
            label.set_margin_top(6);
            label.set_margin_bottom(6);
            label.set_margin_start(8);
            label.set_margin_end(8);
            row.set_child(Some(&label));
            row.set_widget_name(&format!("filter:{index}"));
            list.append(&row);
        }
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        filter_popover.set_child(Some(&list));
        let app_w = Rc::downgrade(app);
        let popover = filter_popover.clone();
        let label = filter_label.clone();
        list.connect_row_activated(move |_, row| {
            let Some(app) = app_w.upgrade() else {
                return;
            };
            let Some(index) = row
                .widget_name()
                .strip_prefix("filter:")
                .and_then(|raw| raw.parse::<usize>().ok())
            else {
                return;
            };
            popover.popdown();
            if app.state.borrow().prefs.filter == index {
                return;
            }
            app.state.borrow_mut().prefs.filter = index;
            label.set_text(state::FILTERS[index]);
            app.save_prefs();
            if let Some(view) = app.library.borrow().clone() {
                rebuild(&app, &view);
            }
        });
    }
    filter_button.set_popover(Some(&filter_popover));
    filter_button.set_tooltip_text(Some("只看某一类媒体"));

    let size = gtk::Scale::with_range(gtk::Orientation::Horizontal, 160.0, 360.0, 20.0);
    size.set_draw_value(false);
    size.set_size_request(132, -1);
    size.set_value(f64::from(app.state.borrow().prefs.card_size));
    size.set_tooltip_text(Some("磁贴尺寸（按素材比例排布）"));
    let size_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let zoom_icon = gtk::Image::from_icon_name("zoom-in-symbolic");
    zoom_icon.set_pixel_size(16);
    size_box.append(&zoom_icon);
    size_box.append(&size);
    size_box.set_visible(app.state.borrow().prefs.view != "list");

    let count = gtk::Label::new(None);
    count.add_css_class("dim");
    count.add_css_class("small");
    count.add_css_class("mono");
    count.set_hexpand(true);
    count.set_xalign(1.0);

    app.search.set_margin_start(4);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.add_css_class("toolbar");
    toolbar.add_css_class("library-toolbar");
    toolbar.set_margin_start(16);
    toolbar.set_margin_end(16);
    toolbar.set_margin_top(10);
    toolbar.set_margin_bottom(10);
    toolbar.append(&sort_button);
    toolbar.append(&filter_button);
    toolbar.append(&size_box);
    toolbar.append(&app.search);
    toolbar.append(&count);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&outer);

    let view = Rc::new(LibraryView {
        root: root.clone().upcast(),
        outer: outer.clone(),
        views: views.clone(),
        grid: grid.clone(),
        grid_width: Cell::new(0),
        list: list.clone(),
        grid_scroller: grid_scroller.clone(),
        list_scroller: list_scroller.clone(),
        toolbar: toolbar.clone(),
        empty: empty.clone(),
        empty_button: empty_button.clone(),
        count: count.clone(),
        size_box: size_box.clone(),
        size: size.clone(),
        wanted: RefCell::new(HashSet::new()),
        refresh_scheduled: Cell::new(false),
        refresh_retries: Cell::new(0),
        user_scrolled: Cell::new(false),
        initial_requests: Cell::new(0),
    });

    // 滚动时重新计算"哪些卡在视口里"，只给新露出来的卡补封面。
    // 防抖 100ms：拖动滚动条不会每一像素都算一遍。
    for scroller in [&grid_scroller, &list_scroller] {
        let adjustment = scroller.vadjustment();
        let app = Rc::downgrade(app);
        let view = Rc::downgrade(&view);
        let slot: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        let view_scrolled = view.clone();
        adjustment.connect_value_changed(move |_| {
            if let Some(view) = view_scrolled.upgrade() {
                view.user_scrolled.set(true);
            }
            let app = app.clone();
            let view = view.clone();
            let slot2 = slot.clone();
            ui::debounce(&slot2, 100, move || {
                if let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) {
                    refresh_visible(&app, &view);
                }
            });
        });
    }

    // 卡片尺寸滑杆：120ms 防抖后保存偏好、更新缩略图请求目标并重排磁贴。
    {
        let app = Rc::downgrade(app);
        let view_down = Rc::downgrade(&view);
        let slot: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        size.connect_value_changed(move |scale| {
            let next = scale.value().round() as i32;
            let app = app.clone();
            let view_down = view_down.clone();
            let slot2 = slot.clone();
            ui::debounce(&slot2, 120, move || {
                let (Some(app), Some(view)) = (app.upgrade(), view_down.upgrade()) else {
                    return;
                };
                if app.state.borrow().prefs.card_size == next {
                    return;
                }
                app.state.borrow_mut().prefs.card_size = next;
                app.save_prefs();
                crate::refresh_thumb_size(&app, &app.window);
                rebuild(&app, &view);
            });
        });
    }

    // 容器宽度变化（窗口拉宽/侧栏开关）→ 重新分行。
    // GTK4 去掉了 `size-allocate` 信号，这里用 120ms 轮询 scroller 的分配宽度：
    // 一次减法比较的成本可以忽略，而窗口缩放/侧栏开合都能在一个周期内跟上。
    {
        let app = Rc::downgrade(app);
        let view = Rc::downgrade(&view);
        glib::timeout_add_local(Duration::from_millis(120), move || {
            let (Some(app), Some(view)) = (app.upgrade(), view.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if view.views.visible_child_name().as_deref() == Some("list") {
                return glib::ControlFlow::Continue;
            }
            let scroller_w = view.grid_scroller.width();
            if scroller_w <= 60 {
                return glib::ControlFlow::Continue;
            }
            // 4px 左右边距 + 2px 余量 → 内容可用宽度。
            let available = (scroller_w - 10).max(280);
            let known = view.grid_width.get();
            if (available - known).abs() > 8 {
                let keep = view.grid_scroller.vadjustment().value();
                rebuild(&app, &view);
                let adjustment = view.grid_scroller.vadjustment();
                adjustment.set_value(keep.min(adjustment.upper() - adjustment.page_size()));
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let app = Rc::downgrade(app);
        empty_button.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.send(Cmd::Rescan);
                app.toast("已请求重新扫描媒体目录");
            }
        });
    }

    // 列表选中 → 属性面板；双击/回车 → 应用。
    {
        let app = Rc::downgrade(app);
        list.connect_row_selected(move |_, row| {
            if let (Some(app), Some(row)) = (app.upgrade(), row) {
                if let Some(id) = row.widget_name().strip_prefix("row:") {
                    let id = id.to_owned();
                    app.select(&id);
                }
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        list.connect_row_activated(move |_, row| {
            if let Some(app) = app.upgrade() {
                if let Some(id) = row.widget_name().strip_prefix("row:") {
                    let id = id.to_owned();
                    app.apply_media(&id, false);
                }
            }
        });
    }

    let is_list = app.state.borrow().prefs.view == "list";
    views.set_visible_child_name(if is_list { "list" } else { "grid" });
    view
}

/// 视图切换（网格/列表），并记住偏好。
pub fn set_view(app: &Rc<App>, view: &Rc<LibraryView>, list: bool) {
    view.views
        .set_visible_child_name(if list { "list" } else { "grid" });
    view.size_box.set_visible(!list);
    let changed = {
        let mut state = app.state.borrow_mut();
        let next = if list { "list" } else { "grid" }.to_owned();
        let changed = state.prefs.view != next;
        state.prefs.view = next;
        changed
    };
    if changed {
        app.save_prefs();
    }
    schedule_visible_refresh(app, view);
}

/// 缩略图到达后的"渐进补"入口：库页当前可见时才排一次可见性刷新。
///
/// 首轮请求按文件大小排序（小图先出），最大的几张可能落在预算外；每来一张
/// 就补一轮，`request_thumb` 自己会跳过已有/在途的键，不会重复请求。
pub fn refresh_visible_soon(app: &Rc<App>) {
    let view = app.library.borrow().clone();
    if let Some(view) = view {
        schedule_visible_refresh(app, &view);
    }
}

/// 排一次可见性刷新：等布局算完再读各卡片的位置。
pub fn schedule_visible_refresh(app: &Rc<App>, view: &Rc<LibraryView>) {
    if view.refresh_scheduled.replace(true) {
        return;
    }
    let app_down = Rc::downgrade(app);
    let view_down = Rc::downgrade(view);
    glib::timeout_add_local_once(Duration::from_millis(60), move || {
        let (Some(app), Some(view)) = (app_down.upgrade(), view_down.upgrade()) else {
            return;
        };
        view.refresh_scheduled.set(false);
        refresh_visible(&app, &view);
    });
}

/// 算出当前视口内的卡片，只给其中没有封面的补请求（单次最多 [`VISIBLE_THUMB_BUDGET`] 张）。
///
/// `wanted` 记下所有在视口（含余量）内的 id：缩略图回包时只有命中这个集合的
/// 才会被采用，滚出视口的卡片不会把图贴回来。
pub fn refresh_visible(app: &Rc<App>, view: &Rc<LibraryView>) {
    let is_list = view.views.visible_child_name().as_deref() == Some("list");
    let scroller = if is_list {
        &view.list_scroller
    } else {
        &view.grid_scroller
    };
    let adjustment = scroller.vadjustment();
    let top = adjustment.value();
    let bottom = top + adjustment.page_size();
    let anchor: gtk::Widget = if is_list {
        view.list.clone().upcast()
    } else {
        view.grid.clone().upcast()
    };

    let mut candidates: Vec<(String, f64)> = Vec::new();
    {
        let cards = app.cards.borrow();
        for (id, list) in cards.iter() {
            for card in list {
                let active_view = in_view(&card.root, is_list);
                if !active_view {
                    continue;
                }
                let Some(bounds) = card.root.compute_bounds(&anchor) else {
                    continue;
                };
                let y = f64::from(bounds.y());
                let height = f64::from(bounds.height());
                if in_visible_window(y, height, top, bottom, VISIBLE_MARGIN) {
                    let center = y + height / 2.0;
                    candidates.push((id.clone(), (center - (top + bottom) / 2.0).abs()));
                }
            }
        }
    }
    if std::env::var_os("LIUGUANG_GUI_DEBUG").is_some() {
        eprintln!(
            "[visible] list={is_list} top={top:.0} bottom={bottom:.0} cards={} candidates={}",
            app.cards.borrow().len(),
            candidates.len()
        );
    }
    if candidates.is_empty() {
        // 首帧布局还没算完（`compute_bounds` 全是 None）：稍后再试，最多 3 次。
        if view.refresh_retries.get() < 3 {
            view.refresh_retries.set(view.refresh_retries.get() + 1);
            schedule_visible_refresh(app, view);
        }
        return;
    }
    view.refresh_retries.set(0);
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));

    let wanted: HashSet<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
    *view.wanted.borrow_mut() = wanted;

    // 单次只补 12 张：首屏在全量解码下会把 daemon 打满，滚动时再按需补。
    // 冷启动只发视口内的卡（上限 `VISIBLE_THUMB_BUDGET`）：视口外的等滚动再加载，
    // 这样「打开 GUI → daemon 解码 32 张 8K 图」的雪崩不会发生。
    // 请求顺序按"文件最小优先"：大 PNG（几千万像素）单张要 1–2 秒，而小图几百
    // 毫秒；先把小图要过来，用户几秒内能看到成排的封面，而不是盯着一屏骨架。
    let state = app.state.borrow();
    let sizes: std::collections::HashMap<&str, u64> = state
        .rows
        .iter()
        .map(|item| (item.id.as_str(), item.size_bytes))
        .collect();
    let mut order = candidates.clone();
    order.sort_by(|a, b| {
        let size = |id: &str| sizes.get(id).copied().unwrap_or(u64::MAX);
        size(&a.0)
            .cmp(&size(&b.0))
            .then_with(|| a.1.total_cmp(&b.1))
    });
    let mut budget = VISIBLE_THUMB_BUDGET;
    for (id, _) in &order {
        if budget == 0 {
            break;
        }
        if !view.user_scrolled.get() && view.initial_requests.get() >= VISIBLE_THUMB_BUDGET as u32 {
            break;
        }
        let key = (id.clone(), 0u64);
        if app.textures.borrow().contains_key(&key) || app.pending.borrow().contains(&key) {
            continue;
        }
        app.request_thumb(id, 0);
        if !view.user_scrolled.get() {
            view.initial_requests.set(view.initial_requests.get() + 1);
        }
        budget -= 1;
    }
}

/// 卡片是否挂在当前活动的容器里（FlowBox 会额外包一层 `FlowBoxChild`）。
fn in_view(root: &gtk::Widget, list: bool) -> bool {
    let mut parent = root.parent();
    while let Some(widget) = parent {
        if list {
            if widget.is::<gtk::ListBox>() {
                return true;
            }
        } else if widget.has_css_class("tile-grid") {
            return true;
        }
        if widget.is::<gtk::ScrolledWindow>() {
            return false;
        }
        parent = widget.parent();
    }
    false
}

/// 卡片纵向范围是否与视口（含两侧余量）相交。
fn in_visible_window(y: f64, height: f64, top: f64, bottom: f64, margin: f64) -> bool {
    y + height >= top - margin && y <= bottom + margin
}

/// 重建卡片列表（筛选/排序/尺寸/搜索变化时调用）。
pub fn rebuild(app: &Rc<App>, view: &Rc<LibraryView>) {
    // 先停掉旧卡片的悬停定时器，否则会留下一堆每 500ms 跑一次的闭包。
    {
        let cards = app.cards.borrow();
        for card in cards.values().flatten() {
            card.hover.stop();
        }
    }
    app.cards.borrow_mut().clear();

    // 重建会丢掉所有旧控件；`app.cards` 若不同步清理，旧卡片（含纹理）会被
    // Rc 一直引用着，窗口缩放来回重建时会持续涨内存。
    while let Some(child) = view.grid.first_child() {
        view.grid.remove(&child);
    }
    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }

    let needle = app.search.text().to_string();
    let (rows, total, scanning, card_size) = {
        let state = app.state.borrow();
        (
            state.visible(&needle),
            state.rows.len(),
            state.scanning,
            state.prefs.card_size,
        )
    };

    let (current, favorites) = {
        let state = app.state.borrow();
        (state.current_media(), state.favorites())
    };

    // 曜晶画报墙：自适应蒙德里安母版，3px 极细拼接缝 + 大小错落节奏 + 100% 满格零黑洞。
    let container_width = {
        let measured = view.grid_scroller.width() - 10;
        if measured > 60 {
            measured.max(280)
        } else {
            view.grid_width.get().max(680)
        }
    };
    view.grid_width.set(container_width);
    let aspects: Vec<f32> = rows.iter().map(grid::aspect_of).collect();
    let layout = grid::plan_editorial(&aspects, container_width, card_size, 3);
    for tile in &layout.tiles {
        let Some(item) = rows.get(tile.index) else {
            continue;
        };
        let in_use = current.as_deref() == Some(item.id.as_str());
        let favorite = favorites.contains(&item.id);
        let card = make_card(
            app,
            item,
            tile.index + 1,
            tile.width,
            tile.height,
            in_use,
            favorite,
        );
        view.grid
            .attach(&card.root, tile.col, tile.row, tile.col_span, tile.row_span);
        let mut cards = app.cards.borrow_mut();
        cards.entry(item.id.clone()).or_default().push(card);
    }

    for item in &rows {
        let in_use = current.as_deref() == Some(item.id.as_str());
        let favorite = favorites.contains(&item.id);

        let row = make_row(app, item, in_use, favorite);
        view.list.append(&row.0);

        let mut cards = app.cards.borrow_mut();
        cards.entry(item.id.clone()).or_default().push(row.1);
    }

    // 空态 / 扫描态。
    let is_empty = rows.is_empty();
    if is_empty {
        let (title, description, show_button) = if scanning {
            (
                "正在扫描媒体目录…".to_owned(),
                "第一次扫描大目录需要几秒；完成后这里会自动出现封面。".to_owned(),
                false,
            )
        } else {
            let root = app
                .state
                .borrow()
                .library_root()
                .unwrap_or_else(|| "媒体目录".to_owned());
            (
                "媒体库是空的".to_owned(),
                format!("把图片或视频放进 {root}，然后点“重新扫描”。"),
                true,
            )
        };
        view.empty.set_title(&title);
        view.empty.set_description(Some(&description));
        view.empty_button.set_visible(show_button);
        view.outer.set_visible_child_name("empty");
    } else {
        view.outer.set_visible_child_name("content");
    }

    let count_text = if scanning {
        format!("{} 项 · 扫描中", format_count(rows.len(), total))
    } else if needle.trim().is_empty() {
        format_count(rows.len(), total)
    } else {
        format!("{} / {} 项", rows.len(), total)
    };
    view.count.set_text(&count_text);
    view.count.set_tooltip_text(Some("已显示 / 库内总数"));

    refresh_compat(app);
    refresh_selection(app);
    // 封面按可见性懒加载：布局完成后再算视口，最多请求 12 张。
    schedule_visible_refresh(app, view);
}

fn format_count(visible: usize, total: usize) -> String {
    if visible == total {
        format!("{total} 项")
    } else {
        format!("{visible} / {total} 项")
    }
}

/// 封面占位：缓存里有就直接贴，没有就留骨架——**不在建卡时请求**。
///
/// 真正的请求由 [`refresh_visible`] 在布局完成后按视口发起（t40：避免一打开
/// GUI 就对全部 38 项触发解码）。
fn prime_picture(app: &Rc<App>, id: &str, picture: &gtk::Picture, skeleton: Option<&gtk::Widget>) {
    if let Some(texture) = app.textures.borrow().get(&(id.to_owned(), 0u64)).cloned() {
        picture.set_paintable(Some(&texture));
        if let Some(skeleton) = skeleton {
            skeleton.set_visible(false);
        }
    }
}

/// 构建一张画报墙磁贴卡片。
fn make_card(
    app: &Rc<App>,
    item: &MediaItem,
    display_index: usize,
    width: i32,
    height: i32,
    in_use: bool,
    favorite: bool,
) -> Rc<Card> {
    let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    base.set_size_request(width, height);
    base.set_hexpand(false);
    base.set_halign(gtk::Align::Start);

    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_halign(gtk::Align::Fill);
    picture.set_valign(gtk::Align::Fill);
    picture.add_css_class("thumb");
    picture.set_paintable(Some(&ui::blank_texture(width, height)));

    let skeleton = gtk::Box::new(gtk::Orientation::Vertical, 0);
    skeleton.add_css_class("skeleton");
    skeleton.set_size_request(width, height);

    let veil = gtk::Box::new(gtk::Orientation::Vertical, 0);
    veil.add_css_class("card-veil");
    veil.set_hexpand(true);
    veil.set_vexpand(true);
    veil.set_halign(gtk::Align::Fill);
    veil.set_valign(gtk::Align::Fill);
    veil.set_can_target(false);

    // Picture 放进 Overlay（默认不参与 measure），防止 HiDPI 物理像素尺寸反撑格子宽度。
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&base));
    overlay.add_overlay(&picture);
    overlay.add_overlay(&skeleton);
    overlay.add_overlay(&veil);

    // 顶行：画册编号 + 悬停显隐星标（左） ｜ 视频帧率/兼容性/正在使用微标（右）。
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    top.set_margin_top(8);
    top.set_margin_start(8);
    top.set_margin_end(8);
    top.set_valign(gtk::Align::Start);

    let index_tag = gtk::Label::new(Some(&format!("{display_index:02}")));
    index_tag.add_css_class("index-tag");
    index_tag.set_valign(gtk::Align::Center);
    top.append(&index_tag);

    let fav_btn = gtk::Button::with_label(if favorite { "★" } else { "☆" });
    fav_btn.add_css_class("fav-btn");
    if favorite {
        fav_btn.add_css_class("active");
    }
    fav_btn.set_tooltip_text(Some(if favorite {
        "从收藏中移除"
    } else {
        "加入收藏"
    }));
    {
        let app = Rc::downgrade(app);
        let id = item.id.clone();
        fav_btn.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.toggle_favorite(&id);
            }
        });
    }
    top.append(&fav_btn);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top.append(&spacer);
    if item.kind == "video" {
        let video_tag = crate::state::fps(item).map_or_else(
            || "▶ 视频".to_owned(),
            |fps| format!("▶ {:.0}", fps.round()),
        );
        let video_chip = gtk::Label::new(Some(&video_tag));
        video_chip.add_css_class("video-chip");
        video_chip.set_valign(gtk::Align::Center);
        top.append(&video_chip);
    }
    let compat = ui::badge("");
    compat.set_visible(false);
    top.append(&compat);
    let live = gtk::Label::new(Some("● 正在使用"));
    live.add_css_class("status-chip");
    live.add_css_class("live");
    live.set_valign(gtk::Align::Center);
    live.set_visible(in_use);
    top.append(&live);
    overlay.add_overlay(&top);

    // 底部杂志画报双行排版：上行清晰大标题，下行细腻等宽规格（含条件转码按钮）。
    let name = gtk::Label::new(Some(display_stem(&item.name)));
    name.add_css_class("card-title-overlay");
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_xalign(0.0);
    name.set_tooltip_text(Some(&item.path));

    let meta = gtk::Label::new(Some(&editorial_meta(item)));
    meta.add_css_class("card-meta-overlay");
    meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    meta.set_xalign(0.0);
    meta.set_hexpand(true);

    let prepare = gtk::Button::new();
    prepare.add_css_class("brightness-chip");
    prepare.set_label("转码");
    prepare.set_tooltip_text(Some("在后台转成可硬解的 8bit HEVC 缓存（源文件不改）"));
    prepare.set_halign(gtk::Align::End);
    prepare.set_visible(false);
    {
        let app = Rc::downgrade(app);
        let id = item.id.clone();
        prepare.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.prepare_media(&id);
            }
        });
    }

    let meta_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    meta_row.append(&meta);
    meta_row.append(&prepare);

    let scrim = gtk::Box::new(gtk::Orientation::Vertical, 2);
    scrim.add_css_class("card-scrim");
    scrim.set_valign(gtk::Align::End);
    scrim.set_halign(gtk::Align::Fill);
    scrim.append(&name);
    scrim.append(&meta_row);
    overlay.add_overlay(&scrim);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("media-card");
    root.set_overflow(gtk::Overflow::Hidden);
    // 固定卡片宽度：由 `base` 决定逻辑自然尺寸，避免 HiDPI 纹理回包时重排列数。
    root.set_size_request(width, height);
    if in_use {
        root.add_css_class("in-use");
    }
    root.append(&overlay);

    let card = Rc::new(Card {
        id: item.id.clone(),
        root: root.clone().upcast(),
        picture: picture.clone(),
        skeleton: Some(skeleton.clone().upcast()),
        live: live.clone(),
        compat: compat.clone(),
        prepare: prepare.clone(),
        hover: Hover::default(),
    });

    let skeleton_widget: gtk::Widget = skeleton.clone().upcast();
    prime_picture(app, &item.id, &picture, Some(&skeleton_widget));
    attach_click(app, item, &root);
    if item.kind == "video" {
        bind_hover(app, &card, &overlay);
    }
    card
}

/// 构建一行列表。
fn make_row(
    app: &Rc<App>,
    item: &MediaItem,
    in_use: bool,
    favorite: bool,
) -> (gtk::ListBoxRow, Rc<Card>) {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(104, 64);
    picture.set_can_shrink(true);
    picture.add_css_class("thumb");
    picture.set_paintable(Some(&ui::blank_texture(104, 64)));

    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&format!("row:{}", item.id));
    row.set_activatable(true);
    row.add_css_class("media-row");
    if in_use {
        row.add_css_class("in-use");
    }

    let name = gtk::Label::new(Some(&item.name));
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_xalign(0.0);
    let meta = gtk::Label::new(Some(&meta_line(item)));
    meta.add_css_class("dim");
    meta.add_css_class("small");
    meta.add_css_class("mono");
    meta.set_xalign(0.0);

    let badges = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    badges.set_valign(gtk::Align::Center);
    badges.append(&ui::badge(&ui::resolution_label(item)));
    if let Some(fps) = ui::fps_label(item) {
        badges.append(&ui::badge(&fps));
    }
    let compat = ui::badge("");
    compat.set_visible(false);
    badges.append(&compat);
    let live = ui::badge("正在使用");
    live.add_css_class("live");
    live.set_visible(in_use);
    badges.append(&live);
    if favorite {
        badges.append(&ui::badge("★"));
    }
    let prepare = gtk::Button::new();
    prepare.add_css_class("flat");
    prepare.add_css_class("small");
    prepare.set_label("准备（转码）");
    prepare.set_tooltip_text(Some("在后台转成可硬解的 8bit HEVC 缓存（源文件不改）"));
    prepare.set_visible(false);
    {
        let app = Rc::downgrade(app);
        let id = item.id.clone();
        prepare.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.prepare_media(&id);
            }
        });
    }
    badges.append(&prepare);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.append(&name);
    text.append(&meta);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    line.set_margin_top(6);
    line.set_margin_bottom(6);
    line.set_margin_start(8);
    line.set_margin_end(10);
    line.append(&picture);
    line.append(&text);
    line.append(&badges);
    row.set_child(Some(&line));

    let card = Rc::new(Card {
        id: item.id.clone(),
        root: row.clone().upcast(),
        picture: picture.clone(),
        skeleton: None,
        live: live.clone(),
        compat: compat.clone(),
        prepare: prepare.clone(),
        hover: Hover::default(),
    });

    prime_picture(app, &item.id, &picture, None);
    attach_click(app, item, &row);
    (row, card)
}

/// 单击选中并**立即应用**（对标 Wallpaper Engine：点一张壁纸就切过去），
/// 右键菜单，双击保持幂等。
fn attach_click(app: &Rc<App>, item: &MediaItem, target: &impl IsA<gtk::Widget>) {
    let gesture = gtk::GestureClick::new();
    // 监听所有鼠标按键，右键（button=3）才能弹上下文菜单。
    gesture.set_button(0);
    let app_down = Rc::downgrade(app);
    let id = item.id.clone();
    let anchor: gtk::Widget = target.clone().upcast();
    gesture.connect_pressed(move |gesture, n_press, x, y| {
        let Some(app) = app_down.upgrade() else {
            return;
        };
        let button = gesture.current_button();
        if button == 3 {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_menu(&app, &id, &anchor);
            return;
        }
        if button != 1 {
            return;
        }
        // 卡片里的小按钮（准备/转码、收藏星标等）自己处理点击，不要顺手切壁纸。
        if over_button(&anchor, x, y) {
            return;
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
        app.select(&id);
        // 单击即应用；双击的第二次 press 由 `apply_on_click` 的 500ms 去重吸收。
        if n_press >= 1 {
            app.apply_on_click(&id);
        }
    });
    target.add_controller(gesture);
}

/// 指针位置是否落在按钮（或按钮内部控件）上。
fn over_button(target: &gtk::Widget, x: f64, y: f64) -> bool {
    let mut picked = target.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = picked {
        if widget.is::<gtk::Button>() || widget.is::<gtk::ToggleButton>() {
            return true;
        }
        if widget == *target {
            break;
        }
        picked = widget.parent();
    }
    false
}

/// 悬停动效：指针停留 ≥300ms 才开始预览（只 2 帧），离开立刻取消。
pub fn bind_hover(app: &Rc<App>, card: &Rc<Card>, overlay: &gtk::Overlay) {
    let motion = gtk::EventControllerMotion::new();
    let app_enter = Rc::downgrade(app);
    let card_enter = card.clone();
    motion.connect_enter(move |_, _, _| {
        let Some(app) = app_enter.upgrade() else {
            return;
        };
        schedule_hover(&app, &card_enter);
    });
    let app_leave = Rc::downgrade(app);
    let card_leave = card.clone();
    motion.connect_leave(move |_| {
        cancel_hover(app_leave.upgrade().as_ref(), &card_leave);
        if let Some(app) = app_leave.upgrade() {
            restore_cover(&app, &card_leave);
        }
    });
    overlay.add_controller(motion);
}

/// 300ms 启动延迟：快速扫过的卡片不会发任何请求。
fn schedule_hover(app: &Rc<App>, card: &Rc<Card>) {
    let generation = card.hover.generation.get() + 1;
    card.hover.generation.set(generation);
    card.hover.active.set(false);
    // 上一个延迟源可能已经触发（一次性源已销毁），不能按 id remove。
    crate::ui::cancel_once(&card.hover.delay);
    let app_down = Rc::downgrade(app);
    let card_down = Rc::downgrade(card);
    let source = glib::timeout_add_local_once(Duration::from_millis(HOVER_DELAY_MS), move || {
        let (Some(app), Some(card)) = (app_down.upgrade(), card_down.upgrade()) else {
            return;
        };
        // 本源已经触发，先把自己的 id 丢掉（否则之后 cancel 会 remove 一个已销毁的源）。
        crate::ui::cancel_once(&card.hover.delay);
        // 代次号变了 ⇒ 指针已经离开（或又进来了），这次回调作废。
        if card.hover.generation.get() != generation {
            return;
        }
        start_hover(&app, &card);
    });
    *card.hover.delay.borrow_mut() = Some(source);
}

fn start_hover(app: &Rc<App>, card: &Rc<Card>) {
    if card.hover.is_active() {
        return;
    }
    card.hover.active.set(true);
    // 只预热 2 帧；同一张卡反复悬停时，`request_thumb` 命中缓存/在途就不再发。
    for at in HOVER_PREVIEW_TIMES {
        app.request_hover_thumb(&card.id, at);
    }
    let card2 = card.clone();
    let app_down = Rc::downgrade(app);
    let source = glib::timeout_add_local(Duration::from_millis(500), move || {
        if !card2.hover.is_active() {
            card2.hover.timer.borrow_mut().take();
            return glib::ControlFlow::Break;
        }
        let next = (card2.hover.frame() + 1) % HOVER_PREVIEW_TIMES.len();
        card2.hover.frame.set(next);
        if let Some(app) = app_down.upgrade() {
            let at_ms = HOVER_PREVIEW_TIMES[next];
            let texture = app
                .textures
                .borrow()
                .get(&(card2.id.clone(), at_ms))
                .cloned();
            if let Some(texture) = texture {
                card2.picture.set_paintable(Some(&texture));
                if let Some(skeleton) = &card2.skeleton {
                    skeleton.set_visible(false);
                }
            }
        }
        glib::ControlFlow::Continue
    });
    *card.hover.timer.borrow_mut() = Some(source);
}

/// 离开：作废延迟/轮播定时器，并通知 `App` 丢掉这条卡片的悬停帧请求。
fn cancel_hover(app: Option<&Rc<App>>, card: &Card) {
    card.hover.generation.set(card.hover.generation.get() + 1);
    card.hover.stop();
    if let Some(app) = app {
        app.cancel_hover_thumbs(&card.id);
    }
}

/// 回到封面帧（缓存里有才贴；没有就继续显示上一帧，不留白）。
fn restore_cover(app: &Rc<App>, card: &Card) {
    let texture = app.textures.borrow().get(&(card.id.clone(), 0u64)).cloned();
    if let Some(texture) = texture {
        card.picture.set_paintable(Some(&texture));
        if let Some(skeleton) = &card.skeleton {
            skeleton.set_visible(false);
        }
    }
}

/// 剥离常见图片/视频后缀，让画廊卡片标题更干净。
#[must_use]
fn display_stem(name: &str) -> &str {
    const EXTS: [&str; 10] = [
        ".png", ".jpg", ".jpeg", ".webp", ".bmp", ".gif", ".mp4", ".mkv", ".webm", ".mov",
    ];
    let lower = name.to_ascii_lowercase();
    for ext in EXTS {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

/// 画报墙左下角副标题：`4K · 60 fps · 80.8 MB` 或 `4K · 8.4 MB`。
#[must_use]
fn editorial_meta(item: &MediaItem) -> String {
    let size = ui::human_size(item.size_bytes);
    let tier = item.details.as_ref().and_then(|d| {
        let max_side = d.width.max(d.height);
        if max_side >= 3840 {
            Some("4K")
        } else if max_side >= 2560 {
            Some("2K")
        } else if max_side >= 1920 {
            Some("1080p")
        } else {
            None
        }
    });
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(tag) = tier {
        parts.push(tag.to_owned());
    }
    if item.kind == "video" {
        if let Some(fps) = crate::state::fps(item) {
            parts.push(format!("{:.0} fps", fps.round()));
        } else {
            parts.push("视频".to_owned());
        }
    }
    parts.push(size);
    parts.join(" · ")
}

/// 悬停取样时刻表（测试/文档共用）。
#[must_use]
fn meta_line(item: &MediaItem) -> String {
    let kind = if item.kind == "video" {
        "视频"
    } else {
        "图片"
    };
    format!("{kind} · {}", ui::human_size(item.size_bytes))
}

/// 右键菜单：应用 / 只放这一项 / 收藏 / 文件管理器 / 移出库。
fn show_menu(app: &Rc<App>, id: &str, anchor: &gtk::Widget) {
    let Some(item) = app.state.borrow().item(id).cloned() else {
        return;
    };
    let favorite = app.state.borrow().is_favorite(id);

    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(4);
    menu.set_margin_bottom(4);
    menu.set_margin_start(4);
    menu.set_margin_end(4);

    type MenuAction = Box<dyn Fn(&Rc<App>)>;
    let mut entries: Vec<(String, MenuAction)> = vec![
        ("应用".to_owned(), {
            let id = id.to_owned();
            Box::new(move |app: &Rc<App>| app.apply_media(&id, false))
        }),
        ("只放这一项".to_owned(), {
            let id = id.to_owned();
            Box::new(move |app: &Rc<App>| app.apply_media(&id, true))
        }),
        (
            if favorite {
                "取消收藏".to_owned()
            } else {
                "收藏".to_owned()
            },
            {
                let id = id.to_owned();
                Box::new(move |app: &Rc<App>| app.toggle_favorite(&id))
            },
        ),
        ("在文件管理器中显示".to_owned(), {
            let path = item.path.clone();
            Box::new(move |_app: &Rc<App>| ui::open_in_file_manager(&path))
        }),
        ("从库中移除（不删文件）".to_owned(), {
            let path = item.path.clone();
            Box::new(move |app: &Rc<App>| {
                app.send(Cmd::DeleteMedia { path: path.clone() });
                app.toast("已请求从媒体库移除（磁盘文件保留）");
            })
        }),
    ];
    // 需要转码的素材：菜单里也给一个"准备"入口（卡片上的按钮之外）。
    let needs_conversion = app
        .compat
        .borrow()
        .get(id)
        .cloned()
        .flatten()
        .is_some_and(|info| info.state == liuguang_ipc::protocol::CompatState::NeedsConversion);
    if needs_conversion {
        entries.insert(
            2,
            ("准备（转码）".to_owned(), {
                let id = id.to_owned();
                Box::new(move |app: &Rc<App>| app.prepare_media(&id))
            }),
        );
    }

    for (label, action) in entries {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        let label_widget = gtk::Label::new(Some(&label));
        label_widget.set_xalign(0.0);
        button.set_child(Some(&label_widget));
        let app_down = Rc::downgrade(app);
        let popover2 = popover.clone();
        let action = Rc::new(action);
        button.connect_clicked(move |_| {
            if let Some(app) = app_down.upgrade() {
                action(&app);
            }
            popover2.popdown();
        });
        menu.append(&button);
    }
    popover.set_child(Some(&menu));

    let popover3 = popover.clone();
    popover.connect_closed(move |_| popover3.unparent());
    popover.set_parent(anchor);
    popover.popup();
}

/// 兼容性角标 / 准备按钮的原地刷新（不重建卡片）。
///
/// 兼容性是逐项异步取回的（`get_media`），转码进度也是每 500ms 刷一次；
/// 如果每次都重建整面墙，用户的滚动位置、悬停状态都会被清掉。
pub fn refresh_compat(app: &Rc<App>) {
    let cards = app.cards.borrow();
    for (id, list) in cards.iter() {
        let compat = app.compat.borrow().get(id).cloned().flatten();
        let prepare = app.prepare.borrow().get(id).cloned();
        let badge = compat.as_ref().and_then(ui::compat_badge);
        let tooltip = compat.as_ref().map(ui::compat_tooltip);
        let target = compat
            .as_ref()
            .and_then(|info| info.target.as_deref())
            .map(str::to_owned);

        for card in list {
            // 角标：Playable/未知 ⇒ 隐藏；否则显示状态并提供原因 tooltip。
            match badge {
                Some((text, class)) => {
                    card.compat.set_text(text);
                    for old in ["warn", "danger", "ok"] {
                        card.compat.remove_css_class(old);
                    }
                    card.compat.add_css_class(class);
                    card.compat.set_tooltip_text(tooltip.as_deref());
                    card.compat.set_visible(true);
                }
                None => card.compat.set_visible(false),
            }

            // 准备按钮：只有 NeedsConversion 显示；转码中原地变百分比并禁用。
            // 已经在准备/已转码的素材也要显示状态：兼容性查询是异步的，
            // 用户点"准备"时 `compat` 可能还没回来，不能因此把进度藏起来。
            let needs = prepare.is_some()
                || compat.as_ref().is_some_and(|info| {
                    info.state == liuguang_ipc::protocol::CompatState::NeedsConversion
                });
            if !needs {
                card.prepare.set_visible(false);
                continue;
            }
            let (label, sensitive, tip) = match &prepare {
                Some(result) => {
                    use liuguang_ipc::protocol::PrepareState;
                    let label = ui::prepare_label(
                        &result.state,
                        result.progress_permille,
                        target.as_deref(),
                    );
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
                    "点击开始后台转码".to_owned(),
                ),
            };
            card.prepare.set_label(&label);
            card.prepare.set_sensitive(sensitive);
            card.prepare.set_tooltip_text(Some(&tip));
            card.prepare.set_visible(true);
        }
    }
}

/// 选中态 / "正在使用"高亮刷新（不重建卡片）。
pub fn refresh_selection(app: &Rc<App>) {
    let (selected, current) = {
        let state = app.state.borrow();
        (state.selected.clone(), state.current_media())
    };
    let cards = app.cards.borrow();
    for (id, list) in cards.iter() {
        let is_selected = selected.as_deref() == Some(id.as_str());
        let in_use = current.as_deref() == Some(id.as_str());
        for card in list {
            toggle_class(&card.root, "selected", is_selected);
            toggle_class(&card.root, "in-use", in_use);
            // 徽标与高亮一起同步：状态刷新改了当前壁纸时，卡片上的
            // "正在使用"必须跟着走，而不是等下次整表重建。
            card.live.set_visible(in_use);
        }
    }
}

fn toggle_class(widget: &gtk::Widget, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// 缩略图到达：贴到所有引用它的卡片上（网格与列表各一份）。
pub fn on_thumb(app: &Rc<App>, id: &str, at_ms: u64) {
    let Some(texture) = app.textures.borrow().get(&(id.to_owned(), at_ms)).cloned() else {
        return;
    };
    let cards = app.cards.borrow();
    let Some(list) = cards.get(id) else { return };
    for card in list {
        let show = card.hover.is_active()
            && HOVER_PREVIEW_TIMES
                .get(card.hover.frame())
                .is_some_and(|at| *at == at_ms);
        if at_ms == 0 || show {
            card.picture.set_paintable(Some(&texture));
            if let Some(skeleton) = &card.skeleton {
                skeleton.set_visible(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Filter, Sort};

    #[test]
    fn hover_previews_two_frames_after_a_delay() {
        assert_eq!(HOVER_PREVIEW_TIMES, [1000, 2000]);
        assert_eq!(HOVER_DELAY_MS, 300);
        // 预算要覆盖"两列 + 三列布局各自的首屏可见卡片"，但不能大到把整库拉起来解码。
        const { assert!(VISIBLE_THUMB_BUDGET <= 24 && VISIBLE_THUMB_BUDGET >= 12) };
    }

    #[test]
    fn visible_window_keeps_margin_and_excludes_far_rows() {
        assert!(in_visible_window(300.0, 100.0, 500.0, 900.0, 240.0));
        assert!(in_visible_window(1100.0, 100.0, 500.0, 900.0, 240.0));
        assert!(!in_visible_window(1200.0, 100.0, 500.0, 900.0, 240.0));
        assert!(!in_visible_window(100.0, 100.0, 500.0, 900.0, 240.0));
    }

    #[test]
    fn meta_line_names_kind_and_size() {
        let item = MediaItem {
            id: "/m/a.mp4".into(),
            source_id: "videos".into(),
            name: "a.mp4".into(),
            path: "/m/a.mp4".into(),
            kind: "video".into(),
            size_bytes: 2048,
            modified_unix: 0,
            details: None,
        };
        assert_eq!(meta_line(&item), "视频 · 2.0 KB");
    }

    #[test]
    fn filters_and_sorts_share_dropdown_order() {
        assert_eq!(Filter::from_index(0), Filter::All);
        assert_eq!(Sort::from_index(0), Sort::Name);
    }

    /// 回归：悬停延迟用的是**一次性** GLib 源，触发后源即销毁。
    /// 之前的 `Hover::stop()` 会对它的 id 再调 `SourceId::remove()`，触发
    /// `glib` 内部的 `unwrap()` → SIGABRT（真机表现：开着 GUI 用一阵子就崩）。
    #[test]
    fn hover_delay_can_fire_then_stop_without_panicking() {
        let context = glib::MainContext::default();
        let _guard = context.acquire();

        let hover = Hover::default();
        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        let setter = fired.clone();
        let source = glib::timeout_add_local_once(std::time::Duration::from_millis(1), move || {
            setter.set(true);
        });
        *hover.delay.borrow_mut() = Some(source);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !fired.get() && std::time::Instant::now() < deadline {
            context.iteration(true);
        }
        assert!(fired.get(), "一次性延迟源应当已经触发");

        // 这正是崩溃路径：源已触发/销毁，再 stop() 不能 panic。
        hover.stop();
        hover.stop(); // 幂等
    }

    #[test]
    fn gallery_stem_strips_media_extensions() {
        assert_eq!(display_stem("20260101_0001.png"), "20260101_0001");
        assert_eq!(display_stem("Clip.MP4"), "Clip");
        assert_eq!(display_stem("no_ext"), "no_ext");
    }
}
