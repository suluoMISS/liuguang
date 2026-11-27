//! t35（T25-F2）：单目标应用失败必须在**操作页（媒体库）**可见，不能停在"已受理…等待"。
//!
//! 背景（t25 真机）：单目标 `WALLPAPER_APPLIED{ok=false}` 此前**不推通知**（只有批量推），
//! 媒体库页一直停在"已受理…等待"，只有总览页能看到 Failed ⇒ 用户以为还在处理，其实已失败。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::{Action, App, ApplyPhase, MockControl, MockScenario, NotificationKind, Page};
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

/// 单输出（= 单目标）场景，选中一项媒体并停在媒体库页。
fn app_single_target(scenario: MockScenario) -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        outputs: 1,
        media: 3,
        ..scenario
    }));
    app.refresh_all();
    app.on_action(Action::Goto(Page::Library));
    assert_eq!(
        app.state.apply_target,
        liuguang_tui::ApplyTarget::Default,
        "默认目标是单目标（不得偷偷广播）"
    );
    app
}

/// 真正的单目标失败：媒体库页必须显示**失败**，且不能停在"已受理…等待"。
#[test]
fn single_target_failure_is_visible_on_library_page() {
    let mut app = app_single_target(MockScenario {
        apply_fails: true,
        ..MockScenario::default()
    });
    app.on_action(Action::ApplyWallpaper); // [A] 单目标应用
    app.tick(std::time::Instant::now()); // 消费 WALLPAPER_APPLIED{ok=false}

    // 状态本身是**明确的失败终态**（不是中间态、也不是 Applied）
    match &app.state.apply_phase {
        Some(ApplyPhase::Failed(_)) => {}
        other => panic!("期望 Failed 终态，得到 {other:?}"),
    }
    assert!(!app.state.apply_phase.as_ref().unwrap().is_applied());

    // 通知里必须**能看到**失败（此前单目标不推通知）
    let notes: String = app
        .state
        .notifications
        .iter()
        .map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notes.contains("失败"), "单目标失败必须推通知：\n{notes}");
    assert!(
        app.state.recent_errors.iter().any(|e| e.contains("失败")),
        "失败应进入最近错误（系统页可见）"
    );

    // **媒体库页自身**必须显示失败，且不得停在"已受理…等待"
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("应用失败"),
        "媒体库页必须显示失败态：\n{shown}"
    );
    assert!(
        !shown.contains("已受理，等待"),
        "失败后不得仍停在'已受理…等待'：\n{shown}"
    );
    assert!(
        !shown.contains("已应用（daemon 已确认）"),
        "失败不得显示成已应用：\n{shown}"
    );
}

/// t34 的 daemon 侧兜底：只推 `ERROR`（没有终态事件）时，界面同样必须收敛为**失败**。
#[test]
fn error_event_without_terminal_event_still_fails_visibly() {
    let mut app = app_single_target(MockScenario {
        swallow_applied_event: true, // 模拟终态事件丢失，只有 ERROR
        ..MockScenario::default()
    });
    app.on_action(Action::ApplyWallpaper);
    app.tick(std::time::Instant::now());
    assert!(
        matches!(app.state.apply_phase, Some(ApplyPhase::Accepted)),
        "前提：此时应处于已受理（等待）"
    );

    app.client
        .push_error("WALLPAPER_APPLY_FAILED", "解码失败：UNSUPPORTED_CODEC");
    app.tick(std::time::Instant::now());

    match &app.state.apply_phase {
        Some(ApplyPhase::Failed(reason)) => assert!(reason.contains("解码失败"), "{reason}"),
        other => panic!("ERROR 必须收敛为失败终态，得到 {other:?}"),
    }
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("应用失败"),
        "媒体库页必须显示失败：\n{shown}"
    );
    assert!(!shown.contains("已受理，等待"), "{shown}");
}

/// 未收到事件前：只能显示"已受理…等待"，**不得**显示成已应用。
#[test]
fn accepted_is_never_rendered_as_applied() {
    let mut app = app_single_target(MockScenario {
        swallow_applied_event: true,
        ..MockScenario::default()
    });
    app.on_action(Action::ApplyWallpaper);
    app.tick(std::time::Instant::now());

    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("已受理，等待"),
        "未确认时必须显示'已受理，等待'：\n{shown}"
    );
    assert!(
        !shown.contains("已应用（daemon 已确认）"),
        "未收到确认不得显示已应用：\n{shown}"
    );
    assert!(!app.state.apply_phase.as_ref().unwrap().is_applied());
}

/// 收到 `ok=true` 后：媒体库页显示"已应用（daemon 已确认）"。
#[test]
fn applied_is_shown_on_library_page_after_confirmation() {
    let mut app = app_single_target(MockScenario::default());
    app.on_action(Action::ApplyWallpaper);
    app.tick(std::time::Instant::now());
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("已应用（daemon 已确认）"),
        "确认后媒体库页应显示已应用：\n{shown}"
    );
    assert!(app.state.apply_phase.as_ref().unwrap().is_applied());
    // 成功不应污染"最近错误"
    assert!(app.state.recent_errors.is_empty());
    assert_ne!(
        app.state.last_notification().map(|n| n.kind),
        Some(NotificationKind::Error)
    );
}

/// 批量（显式"全部显示器"）失败同样逐屏可见（回归保护）。
#[test]
fn batch_failure_still_reports_per_output() {
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        outputs: 2,
        media: 3,
        apply_fails: true,
        ..MockScenario::default()
    }));
    app.refresh_all();
    app.on_action(Action::Goto(Page::Library));
    app.state.apply_target = liuguang_tui::ApplyTarget::AllOutputs;
    app.on_action(Action::ApplyWallpaper);
    app.tick(std::time::Instant::now());

    let notes: String = app
        .state
        .notifications
        .iter()
        .map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notes.contains("DP-1"), "批量失败应逐屏反馈：\n{notes}");
    assert!(notes.contains("HDMI-A-1"), "批量失败应逐屏反馈：\n{notes}");
    let shown = render(&app, 140, 40);
    assert!(shown.contains("应用失败"), "{shown}");
}
