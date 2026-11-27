//! TestBackend 渲染测试：**四种尺寸 × 六页面 × 四档布局**。
//!
//! 覆盖 `docs/TUI_DESIGN.md` §8 的测试计划：
//! - `140x40`（宽档三栏）、`100x30`（中档两栏）、`70x22`（小档单栏）、`50x15`（极小档）；
//! - 0/1/3 个输出；`HeadlessReady` / 断线 / 播放中 / 暂停；
//! - 媒体 0 / 10 / 10000 条的分页；
//! - 中文长文件名与全角字符（**禁止按字节截断**）；
//! - `Resize` 保留选中项、页码、编辑框内容；
//! - 路径输入模态框（光标 / Unicode 编辑 / 关闭后恢复焦点）；
//! - 空闲不忙循环（`poll_timeout > 0`、指标 ≤2 Hz）。
//!
//! 全部用 **Mock IPC**：不依赖 daemon、不需要 DRM。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::model::{MEDIA_PAGE_SIZE, MediaFilter};
use liuguang_tui::ui::layout::{Tier, tier};
use liuguang_tui::{
    Action, App, AppState, Focus, MockControl, MockScenario, Page, SubPage, apply, ui,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

/// 设计文档要求覆盖的四种尺寸。
const SIZES: [(u16, u16); 4] = [(140, 40), (100, 30), (70, 22), (50, 15)];

fn terminal(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).unwrap()
}

fn app(scenario: MockScenario) -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(scenario));
    app.refresh_all();
    app
}

fn draw(term: &mut Terminal<TestBackend>, app: &mut App<MockControl>) {
    term.draw(|f| app.draw(f)).unwrap();
}

/// 把缓冲区读成**可见文本**（每行一个字符串，行尾无填充）。
///
/// 注意：ratatui 把全角字符写在一个格子里，其后的"续格"仍是默认空格，
/// 因此必须按显示宽度跳过续格，否则会读成"总 览"这种带空格的形式。
fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            let mut line = String::new();
            let mut skip = 0usize;
            for x in 0..area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                let w = unicode_width::UnicodeWidthStr::width(sym);
                if w > 1 {
                    skip = w - 1;
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn buffer_text(term: &Terminal<TestBackend>) -> String {
    buffer_lines(term).join("\n")
}

/// 一行里某个子串出现的次数。
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

// ───────────────────────── 1. 四尺寸 × 六页面：不 panic、有内容 ─────────────────────────

#[test]
fn all_pages_render_at_all_four_sizes_without_panic() {
    for (w, h) in SIZES {
        for page in Page::ALL {
            let mut term = terminal(w, h);
            let mut app = app(MockScenario::default());
            app.state.page = page;
            app.state.focus = Focus::List;
            draw(&mut term, &mut app); // 不得 panic
            let text = buffer_text(&term);
            assert!(!text.is_empty(), "{w}x{h} {page:?} 渲染为空");

            if tier(Rect::new(0, 0, w, h)) == Tier::Tiny {
                assert!(text.contains("终端过小"), "{w}x{h} {page:?} 极小档提示缺失");
                assert!(text.contains("Q 退出"), "{w}x{h} {page:?} 最小快捷键缺失");
            } else {
                // 当前页必须在界面里可见（导航高亮或 Tab 或页面标题）
                assert!(
                    text.contains(page.title()),
                    "{w}x{h} {page:?} 页面标识缺失：\n{text}"
                );
            }
        }
    }
}

#[test]
fn current_page_is_persistently_highlighted_in_nav_or_tabs() {
    for (w, h) in [(140, 40), (100, 30)] {
        for page in Page::ALL {
            let mut term = terminal(w, h);
            let mut app = app(MockScenario::default());
            app.on_action(Action::Goto(page));
            draw(&mut term, &mut app);
            let text = buffer_text(&term);
            if h >= 28 && w >= 120 {
                // 宽档：左导航用 ▸ 标记当前页
                assert!(
                    text.contains(&format!("▸ {} {}", page.hotkey(), page.title())),
                    "{w}x{h} 宽档导航未高亮 {page:?}：\n{text}"
                );
            } else {
                // 中档：Tab 用 title* 标记当前页
                assert!(
                    text.contains(&format!("{}:{}*", page.hotkey(), page.title())),
                    "{w}x{h} 中档 Tab 未高亮 {page:?}：\n{text}"
                );
            }
        }
    }
}

// ───────────────────────── 2. 宽档：三栏 + 2 行栏 + 未保存标记 ─────────────────────────

#[test]
fn wide_layout_has_three_columns_two_row_bars_and_unsaved_marker() {
    let (w, h) = (140u16, 40u16);
    let mut term = terminal(w, h);
    let mut app = app(MockScenario::playing());
    app.state.page = Page::Library;
    apply(&mut app.state, Action::InputPath); // 造一个"未保存"上下文
    app.state.dirty = false;
    app.state.close_modal();
    draw(&mut term, &mut app);
    let clean = buffer_text(&term);
    assert!(
        !clean.contains(" *"),
        "无改动时不应出现未保存标记：\n{clean}"
    );

    app.state.dirty = true;
    draw(&mut term, &mut app);
    let lines = buffer_lines(&term);
    let top_two = lines[..2].join("\n");
    assert!(
        top_two.contains('*'),
        "未保存标记 `*` 必须出现在顶栏 2 行内：\n{top_two}"
    );
    assert!(top_two.contains("媒体库"), "顶栏应显示当前页：\n{top_two}");

    // 三栏证据：**同一行**上同时能看到 左导航（高亮项）+ 中栏内容 + 右栏内容
    let triple = lines
        .iter()
        .find(|l| l.contains("▸ 2 媒体库"))
        .expect("找不到被高亮的导航行");
    assert!(
        triple.contains("筛选 全部"),
        "同一行应有中栏列表内容：{triple}"
    );
    assert!(
        triple.contains("图片") || triple.contains("视频"),
        "同一行应有右栏详情内容：{triple}"
    );
    // 左导航必须**持久**列出全部 6 页
    let all_nav = lines.join("\n");
    for page in Page::ALL {
        if page == Page::Library {
            continue; // 当前页已在上面按 ▸ 标记验证
        }
        assert!(
            all_nav.contains(page.title()),
            "左导航应包含 {}：\n{all_nav}",
            page.title()
        );
    }
}

#[test]
fn bottom_bar_keys_change_with_page_and_focus() {
    let (w, h) = (140u16, 40u16);
    let mut term = terminal(w, h);
    let mut app = app(MockScenario::default());

    // 媒体库 + 列表焦点
    app.state.page = Page::Library;
    app.state.focus = Focus::List;
    draw(&mut term, &mut app);
    let lib = buffer_text(&term);
    assert!(lib.contains("输入路径"), "媒体库应提示 I 输入路径：\n{lib}");
    assert!(lib.contains("来源管理"), "媒体库应提示 S 来源管理：\n{lib}");

    // 媒体库 + 详情焦点：提示应改变
    app.state.focus = Focus::Detail;
    draw(&mut term, &mut app);
    let detail = buffer_text(&term);
    assert!(
        detail.contains("返回列表"),
        "详情焦点应提示 Esc 返回列表：\n{detail}"
    );
    assert_ne!(lib, detail, "焦点变化必须改变底栏提示");

    // 系统页：只有"保存配置"，没有"立即应用"
    app.on_action(Action::Goto(Page::System));
    draw(&mut term, &mut app);
    let sys = buffer_text(&term);
    assert!(sys.contains("保存配置"), "系统页应提示 S 保存配置：\n{sys}");
    for line in sys.lines() {
        assert!(
            !line.contains("立即应用"),
            "系统页不应出现立即应用（语义分离），该行：{line}"
        );
    }
}

// ───────────────────────── 3. 中档：顶部 Tab + 两栏 ─────────────────────────

#[test]
fn medium_layout_has_tabs_and_two_columns() {
    let (w, h) = (100u16, 30u16);
    let mut term = terminal(w, h);
    let mut app = app(MockScenario::default());
    app.state.page = Page::Library;
    draw(&mut term, &mut app);
    let lines = buffer_lines(&term);
    assert!(lines[0].contains("1:总览"), "首行应为 Tab：{}", lines[0]);
    assert!(lines[0].contains("2:媒体库*"), "当前页应带 *：{}", lines[0]);
    // 两栏证据：存在一行，其左列是媒体条目、右列是详情里的完整路径
    let two = lines
        .iter()
        .find(|l| l.contains("/srv/wallpapers") && (l.contains("图片") || l.contains("视频")))
        .unwrap_or_else(|| panic!("找不到左右两栏同时有内容的一行：\n{}", lines.join("\n")));
    assert!(
        unicode_width::UnicodeWidthStr::width(two.as_str()) > 55,
        "右列内容必须越过列表列宽（55 列）：{two}"
    );
    // 列表表头（筛选/页码）必须可见
    let text = lines.join("\n");
    assert!(text.contains("筛选 全部"), "缺少列表表头：\n{text}");
    assert!(text.contains("媒体库 · 全部"), "缺少列表区标题：\n{text}");
}

// ───────────────────────── 4. 小档：单栏 + 左右键切子页 ─────────────────────────

#[test]
fn small_layout_is_single_column_and_arrows_switch_subpage() {
    let (w, h) = (70u16, 22u16);
    let mut term = terminal(w, h);
    let mut app = app(MockScenario::default());
    app.state.page = Page::Library;
    draw(&mut term, &mut app);
    let first = buffer_text(&term);
    assert!(first.contains("[列表]"), "小档应显示当前子页：\n{first}");

    // 右箭头 → 详情子页
    app.on_action(Action::SubPageMove(true));
    assert_eq!(app.state.subpage, SubPage::Detail);
    draw(&mut term, &mut app);
    let second = buffer_text(&term);
    assert!(second.contains("[详情]"), "小档应切到详情：\n{second}");
    assert_ne!(first, second, "子页切换必须改变内容");
    // 详情内容出现（完整路径在详情里）
    assert!(
        second.contains("/srv/wallpapers") || second.contains("服务器绝对路径"),
        "详情子页应显示路径信息：\n{second}"
    );

    // 再右箭头 → 操作子页
    app.on_action(Action::SubPageMove(true));
    assert_eq!(app.state.subpage, SubPage::Actions);
    draw(&mut term, &mut app);
    let third = buffer_text(&term);
    assert!(third.contains("[操作]"), "小档应切到操作：\n{third}");
    assert!(third.contains("可用操作"), "操作子页应列出动作：\n{third}");
}

// ───────────────────────── 5. 极小档 ─────────────────────────

#[test]
fn tiny_layout_shows_only_minimum_information() {
    let (w, h) = (50u16, 15u16);
    let mut term = terminal(w, h);
    let mut app = app(MockScenario::playing());
    app.state.page = Page::Library;
    draw(&mut term, &mut app);
    let text = buffer_text(&term);
    assert!(text.contains("终端过小"), "应提示终端过小：\n{text}");
    assert!(text.contains("50x15"), "应显示当前尺寸：\n{text}");
    assert!(text.contains("60x18"), "应显示最小要求：\n{text}");
    assert!(text.contains("Q 退出"), "应有最小快捷键：\n{text}");
    assert!(text.contains("1-6"), "应有切页提示：\n{text}");
    // 极小档不应尝试渲染三栏内容
    assert!(!text.contains("筛选"), "极小档不应挤列表：\n{text}");
}

#[test]
fn boundary_sizes_do_not_panic() {
    for (w, h) in [
        (60, 18),
        (59, 17),
        (80, 24),
        (79, 23),
        (120, 28),
        (119, 27),
        (1, 1),
        (0, 0),
        (200, 5),
        (40, 100),
    ] {
        let mut term = terminal(w, h);
        let mut app = app(MockScenario::default());
        for page in Page::ALL {
            app.state.page = page;
            draw(&mut term, &mut app);
        }
    }
}

// ───────────────────────── 6. 中文长文件名 / 全角字符 ─────────────────────────

#[test]
fn long_chinese_names_never_break_by_bytes_or_overflow() {
    for (w, h) in SIZES {
        let mut term = terminal(w, h);
        let mut app = app(MockScenario::default().with_media(10));
        app.state.page = Page::Library;
        // 让详情也显示长中文名
        app.state.focus = Focus::Detail;
        draw(&mut term, &mut app);
        let lines = buffer_lines(&term);
        for (i, line) in lines.iter().enumerate() {
            let width = unicode_width::UnicodeWidthStr::width(line.as_str());
            assert!(
                width <= w as usize,
                "{w}x{h} 第 {i} 行超出终端宽度（{width} > {w}）：{line}"
            );
            assert!(
                !line.contains('\u{FFFD}'),
                "{w}x{h} 第 {i} 行出现替换字符（按字节截断的典型症状）：{line}"
            );
        }
        let text = lines.join("\n");
        if tier(Rect::new(0, 0, w, h)) != Tier::Tiny {
            // 中文文件名必须能出现（没有被整行丢弃）
            assert!(
                text.contains("城市夜景") || text.contains("…"),
                "{w}x{h} 应显示中文文件名或其截断形式：\n{text}"
            );
        }
    }
}

#[test]
fn selected_row_stays_within_view_at_page_end() {
    let mut term = terminal(140, 40);
    let mut app = app(MockScenario::default().with_media(100));
    app.state.page = Page::Library;
    app.state.media_cursor = MEDIA_PAGE_SIZE - 1; // 本页最后一项
    draw(&mut term, &mut app);
    let text = buffer_text(&term);
    assert!(text.contains("▍"), "应能看到选中标记：\n{text}");
    assert!(text.contains("第 1–50 项"), "页内范围提示应在：\n{text}");
}

// ───────────────────────── 7. 焦点正确性 ─────────────────────────

#[test]
fn tab_cycles_focus_and_ui_reflects_it() {
    let mut term = terminal(140, 40);
    let mut app = app(MockScenario::default());
    app.state.page = Page::Library;
    app.state.focus = Focus::Nav;
    let mut seen = Vec::new();
    for _ in 0..4 {
        draw(&mut term, &mut app);
        seen.push(buffer_text(&term));
        app.on_action(Action::FocusMove(true));
    }
    assert_eq!(app.state.focus, Focus::Nav, "四次 Tab 应回到起点");
    // 每个焦点下的底栏都应显示当前焦点名
    for (i, text) in seen.iter().enumerate() {
        let expect = match i {
            0 => "导航",
            1 => "列表",
            2 => "详情",
            _ => "操作",
        };
        assert!(
            text.contains(&format!("焦点 {expect}")) || text.contains(expect),
            "第 {i} 次焦点应显示 {expect}：\n{text}"
        );
    }
}

// ───────────────────────── 8. Resize 保留状态 ─────────────────────────

#[test]
fn resize_preserves_selection_page_and_editor_content() {
    let mut app = app(MockScenario::default().with_media(100));
    app.state.page = Page::Library;
    app.state.media_cursor = 7;
    app.state.media.page = 1;
    apply(&mut app.state, Action::InputPath);
    app.state.editor.set_text("/srv/wallpapers/城市夜景.png");
    app.state.focus = Focus::Detail;

    let before = (
        app.state.media_cursor,
        app.state.media.page,
        app.state.editor.text(),
        app.state.focus,
        app.state.page,
    );

    for (w, h) in [(70u16, 22u16), (100, 30), (50, 15), (140, 40)] {
        app.on_resize(w, h);
        let mut t = terminal(w, h);
        draw(&mut t, &mut app);
        assert_eq!(
            (
                app.state.media_cursor,
                app.state.media.page,
                app.state.editor.text(),
                app.state.focus,
                app.state.page,
            ),
            before,
            "Resize 到 {w}x{h} 后状态必须保留"
        );
        if tier(Rect::new(0, 0, w, h)) != Tier::Tiny {
            // 弹窗与编辑内容仍可见
            assert!(
                buffer_text(&t).contains("城市夜景"),
                "{w}x{h} 编辑框内容应可见"
            );
        }
    }
}

// ───────────────────────── 9. 路径输入模态框 ─────────────────────────

#[test]
fn path_modal_renders_cursor_and_closes_back_to_previous_focus() {
    let mut term = terminal(140, 40);
    let mut app = app(MockScenario::default());
    app.state.page = Page::Library;
    app.state.focus = Focus::List;
    app.on_action(Action::InputPath);
    for c in "/srv/壁纸/城市夜景.png".chars() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        );
        app.on_key(ev);
    }
    draw(&mut term, &mut app);
    let text = buffer_text(&term);
    assert!(
        text.contains("输入服务器绝对路径"),
        "弹窗标题缺失：\n{text}"
    );
    assert!(
        text.contains("/srv/壁纸/城市夜景.png"),
        "编辑内容缺失：\n{text}"
    );
    assert!(text.contains("Esc 取消"), "取消提示缺失：\n{text}");
    assert!(
        text.contains("路径校验由 daemon 执行"),
        "必须写明校验责任在 daemon：\n{text}"
    );

    // 光标：编辑器内容被白色背景块标出（在缓冲区里表现为反显单元格）
    let buf = term.backend().buffer();
    let mut has_cursor = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y))
                && cell.bg == ratatui::style::Color::White
            {
                has_cursor = true;
            }
        }
    }
    assert!(has_cursor, "路径输入框必须显示光标");

    // Esc 关闭并恢复原焦点
    app.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.state.modal_open());
    assert_eq!(app.state.focus, Focus::List, "关闭弹窗必须恢复焦点");
    draw(&mut term, &mut app);
    assert!(
        !buffer_text(&term).contains("输入服务器绝对路径"),
        "关闭后弹窗不应残留"
    );
}

#[test]
fn path_modal_is_usable_at_every_size_without_overflow() {
    for (w, h) in SIZES {
        let mut term = terminal(w, h);
        let mut app = app(MockScenario::default());
        app.state.page = Page::Library;
        app.on_action(Action::InputPath);
        app.state
            .editor
            .set_text("/srv/wallpapers/一个非常长的中文目录名字/城市夜景_最终版.png");
        draw(&mut term, &mut app);
        for line in buffer_lines(&term) {
            assert!(
                unicode_width::UnicodeWidthStr::width(line.as_str()) <= w as usize,
                "{w}x{h} 弹窗越界：{line}"
            );
        }
    }
}

// ───────────────────────── 10. Mock 场景：输出/状态/媒体规模 ─────────────────────────

#[test]
fn mock_output_scenarios_render() {
    for (scenario, expect) in [
        (
            MockScenario::default().with_outputs(0),
            "（daemon 未报告任何输出",
        ),
        (MockScenario::default().with_outputs(1), "DP-1"),
        (MockScenario::three_outputs(), "HDMI-A-1"),
    ] {
        let mut term = terminal(140, 40);
        let mut app = app(scenario);
        app.state.page = Page::Outputs;
        draw(&mut term, &mut app);
        let text = buffer_text(&term);
        assert!(text.contains(expect), "输出场景缺少 `{expect}`：\n{text}");
    }
}

#[test]
fn headless_disconnected_paused_and_playing_are_visible() {
    let cases = [
        (MockScenario::headless(), "HeadlessReady"),
        (MockScenario::disconnected(), "断线"),
        (MockScenario::paused(), "Paused"),
    ];
    for (scenario, expect) in cases {
        let mut term = terminal(140, 40);
        let mut app = app(scenario);
        app.state.page = Page::Dashboard;
        draw(&mut term, &mut app);
        let text = buffer_text(&term);
        assert!(text.contains(expect), "场景缺少 `{expect}`：\n{text}");
    }
}

#[test]
fn media_scale_0_10_10000_uses_paging() {
    for (count, expect_total) in [(0usize, 0usize), (10, 10), (10_000, 10_000)] {
        let mut term = terminal(140, 40);
        let mut app = app(MockScenario::default().with_media(count));
        app.state.page = Page::Library;
        draw(&mut term, &mut app);
        let text = buffer_text(&term);
        assert!(
            text.contains(&format!("共 {expect_total} 项")),
            "媒体 {count} 条时应显示总数：\n{text}"
        );
        if count == 0 {
            assert!(text.contains("空"), "空列表应给出空状态：\n{text}");
        }
        if count == 10_000 {
            assert!(text.contains("第 1/200 页"), "10k 条必须分页：\n{text}");
            let shown = count_occurrences(&text, "城市夜景");
            assert!(
                shown <= MEDIA_PAGE_SIZE + 2,
                "一次渲染不得超过一页（显示了 {shown} 项）"
            );
        }
        // 筛选切换后仍能渲染
        app.state.filter = MediaFilter::Video;
        app.on_action(Action::FilterCycle);
        draw(&mut term, &mut app);
    }
}

#[test]
fn offline_scenario_marks_data_as_stale() {
    let mut term = terminal(140, 40);
    let mut app = app(MockScenario::disconnected());
    app.state.page = Page::Library;
    draw(&mut term, &mut app);
    let text = buffer_text(&term);
    assert!(text.contains("断线"), "必须显示断线态：\n{text}");
    assert!(
        text.contains("最近一次") || text.contains("过期"),
        "断线时必须说明数据可能过期：\n{text}"
    );
}

// ───────────────────────── 11. 性能预算：无忙循环、指标 ≤2 Hz ─────────────────────────

#[test]
fn idle_app_has_no_busy_loop_and_metrics_are_throttled() {
    let mut app = app(MockScenario::default());
    let t0 = std::time::Instant::now();
    // 空闲时 poll 超时必须 > 0（否则就是忙循环）
    assert!(
        app.poll_timeout(t0) > std::time::Duration::ZERO,
        "空闲超时必须为正"
    );
    assert!(app.poll_timeout(t0) <= liuguang_tui::model::POLL_TIMEOUT);

    // 模拟 3 秒、每 50ms 一次 tick：指标刷新不得超过 2 Hz（3 秒 ≤ 7 次，含首次）
    let before = app.client.calls().len();
    let mut metrics_calls = 0usize;
    for step in 0..60 {
        let now = t0 + std::time::Duration::from_millis(50 * (step + 1));
        let calls_before = app.client.calls().len();
        app.tick(now);
        let added = &app.client.calls()[calls_before..];
        if added
            .iter()
            .any(|c| matches!(c, liuguang_tui::MockCall::Metrics))
        {
            metrics_calls += 1;
        }
    }
    assert!(
        metrics_calls <= 7,
        "3 秒内指标刷新 {metrics_calls} 次，超过 2 Hz 上限"
    );
    assert!(app.client.calls().len() > before);
}

#[test]
fn needs_render_is_event_driven() {
    let mut app = app(MockScenario::default());
    let mut term = terminal(140, 40);
    draw(&mut term, &mut app); // 首帧：渲染后应清掉脏标记
    assert!(!app.needs_render(), "渲染后不应再标记为脏");
    // 无事件时不重绘
    for _ in 0..100 {
        assert!(!app.needs_render());
    }
    // 有事件时重绘
    app.on_action(Action::Goto(Page::System));
    assert!(app.needs_render(), "状态变化后必须重绘");
}

// ───────────────────────── 12. Apply / Save 语义分离（UI 层） ─────────────────────────

#[test]
fn abandoning_edits_never_writes_config_in_ui_flow() {
    let mut app = app(MockScenario::default());
    app.state.page = Page::Outputs;
    app.state.selected_output = Some("DP-1".to_string());
    // 编辑但放弃（Esc / 关闭弹窗 / 切页）→ 不得调用 save_config
    app.on_action(Action::InputPath);
    app.state.editor.set_text("/srv/壁纸/x.png");
    app.on_action(Action::Cancel);
    app.on_action(Action::Goto(Page::Effects));
    app.on_action(Action::CycleEffect(true));
    app.on_action(Action::Cancel);
    assert!(
        !app.client.called_save_config(),
        "放弃编辑不得写入配置（save_config 不应被调用）"
    );
    // Apply → 只调用 apply_transform
    app.on_action(Action::Apply);
    assert!(app.client.called_apply(), "A 应触发『立即应用』");
    assert!(!app.client.called_save_config(), "Apply 不得顺手保存");
    // Save → 才调用 save_config
    app.on_action(Action::Save);
    assert!(app.client.called_save_config(), "S 应触发保存");
    assert!(!app.state.dirty, "保存后不应再有未保存标记");
}

#[test]
fn quit_action_does_not_call_any_backend_command() {
    let mut app = app(MockScenario::default());
    let before = app.client.calls().len();
    app.on_action(Action::Quit);
    assert!(app.should_quit());
    let added = &app.client.calls()[before..];
    assert!(
        added.is_empty(),
        "退出 TUI 不得向 daemon 发任何命令（不停 daemon）：{added:?}"
    );
}

// ───────────────────────── 13. 布局库与渲染一致 ─────────────────────────

#[test]
fn layout_regions_are_used_consistently_with_tier() {
    for (w, h) in SIZES {
        let area = Rect::new(0, 0, w, h);
        let t = tier(area);
        let regions = ui::layout::compute(area, t);
        // 所有区域都在终端内
        for r in [
            regions.top,
            regions.nav,
            regions.tabs,
            regions.main,
            regions.detail,
            regions.bottom,
        ] {
            assert!(r.x.saturating_add(r.width) <= w);
            assert!(r.y.saturating_add(r.height) <= h);
        }
        if t == Tier::Wide {
            assert!(regions.nav.width > 0 && regions.detail.width > 0);
            assert_eq!(regions.top.height, 2);
            assert_eq!(regions.bottom.height, 2);
        }
    }
}

#[test]
fn app_state_page_titles_match_spec() {
    assert_eq!(Page::ALL.len(), 7);
    let titles: Vec<&str> = Page::ALL.iter().map(|p| p.title()).collect();
    assert_eq!(
        titles,
        vec![
            "总览",
            "媒体库",
            "播放列表",
            "显示器",
            "转场",
            "系统",
            "静态壁纸"
        ]
    );
    let _ = AppState::new();
}

// ───────── 14. 帧残留回归：内容变短时不得留下上一帧的字符 ─────────

#[test]
fn shorter_frame_does_not_leave_residue_from_previous_frame() {
    // 真实缺陷：终端缓冲区不会自动清空，若新一帧的行比上一帧短，
    // 旧字符会残留（真机上表现为"上一页的文字碎片"）。
    // 该测试在去掉 `ui::draw` 开头的 Clear 后**必然失败**。
    let mut term = terminal(140, 40);
    let mut app = app(MockScenario::default().with_media(10));

    // 第一帧：媒体库（底栏含 "输入路径/来源管理"，正文含媒体行）
    app.on_action(Action::Goto(Page::Library));
    draw(&mut term, &mut app);
    let long = buffer_text(&term);
    assert!(
        long.contains("输入路径"),
        "媒体库底栏应有 I 输入路径：\n{long}"
    );
    assert!(long.contains("城市夜景"), "媒体库应列出媒体：\n{long}");

    // 第二帧：系统页（正文/底栏都更短，且不含这些词）
    app.on_action(Action::Goto(Page::System));
    draw(&mut term, &mut app);
    let short = buffer_text(&term);
    assert!(short.contains("系统"), "应已切到系统页：\n{short}");
    assert!(
        !short.contains("输入路径"),
        "短帧残留了上一帧的字符（Clear 被移除？）：\n{short}"
    );
    assert!(
        !short.contains("城市夜景"),
        "短帧残留了上一帧的媒体行：\n{short}"
    );

    // 再切回并缩小终端：重排后同样不得残留
    app.on_action(Action::Goto(Page::Library));
    draw(&mut term, &mut app);
    let mut small = terminal(70, 22);
    draw(&mut small, &mut app);
    let small_text = buffer_text(&small);
    assert!(
        small_text.contains("媒体库"),
        "小档应显示媒体库：\n{small_text}"
    );
    assert!(
        !small_text.contains("焦点 导航"),
        "小档残留了宽档底栏文字：\n{small_text}"
    );
}
