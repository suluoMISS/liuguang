//! t36（T25-F1 的 UI 侧残留）：保存提示必须依据 `SaveConfigResult.verified`。
//!
//! `saved` 只代表"字节写出去了"；**`verified` 才代表"改动真的进了配置"**
//! （daemon 回读刚写出的文件与 `effective_sources()` 双向比较）。
//! 只看"调用是否 Ok"会把 `verified=false` 渲染成成功 —— 正是 T25-F1 的同一类问题。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::{Action, App, MockCall, MockControl, MockScenario, NotificationKind, Page};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render<C: liuguang_tui::Session>(app: &App<C>, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
    term.draw(|f| liuguang_tui::ui::draw(f, &app.state))
        .expect("draw");
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut skip = 0usize;
        for x in 0..buf.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if w > 1 {
                skip = w - 1;
            }
        }
        out.push('\n');
    }
    out
}

fn app_with(scenario: MockScenario) -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(scenario));
    app.refresh_all();
    app
}

fn notes(app: &App<MockControl>) -> String {
    app.state
        .notifications
        .iter()
        .map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// **核心用例**：`verified=false` 时界面必须显示失败，不得显示成功。
#[test]
fn verified_false_is_shown_as_failure_never_as_success() {
    let mut app = app_with(MockScenario {
        save_verified: false,
        save_merged_sources: 2, // 即使合并了来源，回读校验没过也**不能**说成功
        ..MockScenario::default()
    });
    app.state.dirty = true;
    app.state.sources_dirty = true;

    app.on_action(Action::Save);

    assert!(
        app.client.called_save_config(),
        "前提：确实调用了 save_config"
    );
    let text = notes(&app);
    assert!(
        text.contains("回读校验未通过"),
        "verified=false 必须显示明确失败：\n{text}"
    );
    assert!(
        !text.contains("已保存配置（回读校验通过）"),
        "verified=false **不得**渲染成成功：\n{text}"
    );
    assert_eq!(
        app.state.last_notification().map(|n| n.kind),
        Some(NotificationKind::Error),
        "应以错误级别呈现"
    );
    assert!(app.state.dirty, "校验未通过时不得清除未保存标记");
    assert!(
        app.state.sources_dirty,
        "校验未通过时不得清除来源未保存标记"
    );
    assert!(
        app.state
            .recent_errors
            .iter()
            .any(|e| e.contains("回读校验")),
        "应进入最近错误（系统页可见）"
    );

    // 系统页（只读诊断）也能看到这条失败
    app.on_action(Action::Goto(Page::System));
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("回读校验未通过") || shown.contains("回读校验"),
        "系统页应能看到失败原因：\n{shown}"
    );
}

/// `saved=false`（连字节都没写出去）同样不得显示成功。
#[test]
fn saved_false_is_shown_as_failure() {
    let mut app = app_with(MockScenario {
        save_saved: false,
        save_verified: false,
        ..MockScenario::default()
    });
    app.state.dirty = true;
    app.on_action(Action::Save);
    let text = notes(&app);
    assert!(text.contains("未写出"), "saved=false 必须明说：\n{text}");
    assert!(!text.contains("已保存配置（回读校验通过）"), "{text}");
    assert!(app.state.dirty);
}

/// 校验通过时才显示成功，并且**合并条数必须准确**（只说真合并过的）。
#[test]
fn verified_true_reports_success_and_accurate_merged_count() {
    let mut app = app_with(MockScenario {
        save_verified: true,
        save_merged_sources: 3,
        ..MockScenario::default()
    });
    app.state.dirty = true;
    app.state.sources_dirty = true;
    app.on_action(Action::Save);

    let text = notes(&app);
    assert!(text.contains("已保存配置（回读校验通过）"), "{text}");
    assert!(text.contains("已合并 3 项"), "合并条数必须准确：\n{text}");
    assert!(
        !text.contains("1 项") && !text.contains("2 项"),
        "不得出现与实际不符的条数：\n{text}"
    );
    assert!(!app.state.dirty, "校验通过后应清除未保存标记");
    assert!(
        !app.state.sources_dirty,
        "真合并了来源改动后应清除来源未保存标记"
    );
    assert_eq!(app.state.recent_errors.len(), 0, "成功不应进入最近错误");

    // 调用序列里确实只有一次 save_config（唯一落盘点）
    let saves = app
        .client
        .calls()
        .iter()
        .filter(|c| matches!(c, MockCall::SaveConfig))
        .count();
    assert_eq!(saves, 1);
}

/// 合并条数为 0 时不得编造"已合并 N 项"。
#[test]
fn no_merged_sources_means_no_merge_claim() {
    let mut app = app_with(MockScenario {
        save_verified: true,
        save_merged_sources: 0,
        ..MockScenario::default()
    });
    app.state.sources_dirty = true;
    app.on_action(Action::Save);
    let text = notes(&app);
    assert!(text.contains("已保存配置（回读校验通过）"), "{text}");
    assert!(
        !text.contains("已合并"),
        "没有来源改动时不得声称合并：\n{text}"
    );
    assert!(
        app.state.sources_dirty,
        "没有真的合并来源时不应清除 sources_dirty"
    );
}

/// 失败（Err）路径同样不得显示成功，且保留未保存标记（回归保护）。
#[test]
fn error_path_keeps_dirty_and_reports_failure() {
    let mut app = app_with(MockScenario::default());
    // 让 save 走 Err：断线场景下所有调用都失败
    app.client.scenario.offline = Some("daemon 未运行".to_string());
    app.state.dirty = true;
    app.on_action(Action::Save);
    let text = notes(&app);
    assert!(text.contains("保存失败"), "{text}");
    assert!(!text.contains("已保存配置（回读校验通过）"), "{text}");
    assert!(app.state.dirty);
}

// ───────── 真机 F4（t37 归因修正）：来源被**配置容器**覆盖 ⇒ 运行时删不掉，UI 必须解释清楚 ─────────

fn key(app: &mut App<MockControl>, code: crossterm::event::KeyCode) {
    app.on_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
}

fn open_source_manage(app: &mut App<MockControl>) {
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    assert!(app.state.modal_open(), "来源管理应打开");
}

/// 删除「被配置容器覆盖」的来源（t37 的 F4 场景）⇒ **立刻**解释"删不掉"，且**不得**在保存后说"已保存"。
#[test]
fn root_derived_source_delete_is_explained_and_never_reported_as_saved() {
    let mut app = app_with(MockScenario {
        root_derived: true, // 与真机一致：运行时删除后它仍在列表里，save 回读必失败
        ..MockScenario::default()
    });
    open_source_manage(&mut app);
    let before = app.state.sources.sources.len();

    key(&mut app, crossterm::event::KeyCode::Char('d'));

    // ① 立刻用权威列表纠正：来源还在 + 解释原因（不能只说"已删除"）
    assert_eq!(
        app.state.sources.sources.len(),
        before,
        "被配置容器覆盖的来源删不掉：列表长度不应变化"
    );
    let text = notes(&app);
    assert!(
        text.contains("配置容器") && text.contains("运行时删除无效"),
        "必须解释'删不掉'的原因（配置容器覆盖，t37 已证伪 root 派生）：\n{text}"
    );
    assert!(
        !text.contains("来源已删除（运行时）"),
        "不得谎称已删除：\n{text}"
    );
    assert!(
        app.state
            .recent_errors
            .iter()
            .any(|e| e.contains("配置容器")),
        "应进入最近错误"
    );

    // ② 注意：此处 `sources_dirty` **保持 false** 是正确的（没有任何改动真的发生）；
    //    因此 `s` 只会提示"没有待保存的来源改动"——不得出现"已保存"。
    key(&mut app, crossterm::event::KeyCode::Char('s'));
    let text = notes(&app);
    assert!(
        !text.contains("已保存配置"),
        "没有改动时更不得声称已保存：\n{text}"
    );

    // ③ 构造"确有来源改动 + daemon 回读校验失败"（真机 F4 的保存路径）：
    //    只有这时才应走 save_config，并且**必须**报告校验失败。
    app.state.sources_dirty = true;
    key(&mut app, crossterm::event::KeyCode::Char('s')); // 弹确认
    key(&mut app, crossterm::event::KeyCode::Enter); // 确认
    let text = notes(&app);
    assert!(
        text.contains("回读校验未通过"),
        "保存必须如实报告校验失败：\n{text}"
    );
    assert!(
        text.contains("配置容器") || text.contains("conflicts"),
        "失败原因里应给出可操作提示（容器/conflicts）：\n{text}"
    );
    assert!(
        !text.contains("已保存配置"),
        "**不得**出现'已保存'：\n{text}"
    );
    assert!(
        !text.contains("已保存配置（回读校验通过）"),
        "不得渲染成成功：\n{text}"
    );
    assert!(
        app.state.sources_dirty,
        "校验失败不得清除来源未保存标记（改动仍只在运行时）"
    );

    // ④ 屏幕上（系统页只读诊断）也要看得到失败
    app.on_action(Action::Cancel);
    app.on_action(Action::Goto(Page::System));
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("回读校验未通过"),
        "系统页应能看到失败：\n{shown}"
    );
}

/// 新增来源后，光标必须移到**刚新增的那一项**（否则用户下一步选不中它）。
#[test]
fn add_source_moves_cursor_to_the_new_entry() {
    let mut app = app_with(MockScenario::default());
    open_source_manage(&mut app);
    let before = app.state.sources.sources.len();

    key(&mut app, crossterm::event::KeyCode::Char('a')); // 新增（文本输入）
    app.state.editor.set_text("/srv/wallpapers/新目录");
    key(&mut app, crossterm::event::KeyCode::Enter);

    assert_eq!(
        app.state.sources.sources.len(),
        before + 1,
        "运行时应真的多一个来源"
    );
    let selected = app
        .state
        .sources
        .sources
        .get(app.state.sources_cursor)
        .expect("有选中项");
    assert_eq!(
        selected.name, "新目录",
        "光标必须落在刚新增的那一项（当前索引 {}）",
        app.state.sources_cursor
    );

    // 新增后弹窗关闭；用户重新打开来源管理（`S`，真实路径），此时按 `d` 应删**刚新增的那一项**
    assert!(!app.state.modal_open(), "提交后弹窗应关闭");
    key(&mut app, crossterm::event::KeyCode::Char('S'));
    assert!(app.state.modal_open(), "S 应重新打开来源管理");
    key(&mut app, crossterm::event::KeyCode::Char('d'));
    assert!(
        app.client
            .calls()
            .iter()
            .any(|c| matches!(c, MockCall::RemoveSource(id) if id == "/srv/wallpapers/新目录")),
        "删除应针对刚新增的那一项：{:?}",
        app.client.calls()
    );
}
