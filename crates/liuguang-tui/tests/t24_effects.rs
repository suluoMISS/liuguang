//! t24：转场页的**诚实标注**与试用/保存。
//!
//! 事实来源：`apps/liuguang` 只映射 `fade|slide|wipe|circle`（其它名字**回退成 fade**），
//! `crates/liuguang-render` 实现这四种。因此 `random`/`none` 必须标 `Disabled`，
//! 且**不得**被下发——否则用户以为选了 random，实际播的是 fade。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::model::{EFFECTS, EFFECTS_IMPLEMENTED, effect_is_available, next_random_effect};
use liuguang_tui::{Action, App, Effect, MockCall, MockControl, MockScenario, Page, apply};

fn app() -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(MockScenario::default()));
    app.state.page = Page::Effects;
    app
}

#[test]
fn availability_matrix_matches_daemon_reality() {
    assert_eq!(
        EFFECTS_IMPLEMENTED,
        ["fade", "slide", "wipe", "circle"],
        "可用集合必须与 daemon 的效果名映射一致"
    );
    for e in EFFECTS_IMPLEMENTED {
        assert!(effect_is_available(e), "{e} 必须标可用");
        assert!(EFFECTS.contains(&e), "{e} 必须在候选列表里");
    }
    for e in ["random", "none"] {
        assert!(
            !effect_is_available(e),
            "{e} 未实现，必须标 Disabled（不得假装可用）"
        );
    }
}

/// 默认值：`fade` + 350ms（规范 §5.4）。
#[test]
fn default_transition_is_fade_350ms() {
    let st = liuguang_tui::AppState::new();
    assert_eq!(st.transition.effect, "fade");
    assert_eq!(st.transition.duration_ms, 350);
}

/// `A`：可用效果 ⇒ 真的下发 `set_transition`（试用，只改运行时）。
#[test]
fn apply_transition_sends_when_available() {
    let mut app = app();
    app.state.transition.effect = "wipe".to_string();
    app.state.transition.duration_ms = 420;
    app.on_action(Action::ApplyTransition);
    let sent = app.client.calls.iter().any(
        |c| matches!(c, MockCall::SetTransition(p) if p.effect == "wipe" && p.duration_ms == 420),
    );
    assert!(sent, "可用效果必须真的下发：{:?}", app.client.calls);
    assert!(
        !app.client.called_save_config(),
        "试用（A）不得顺手写配置（Apply/Save 分离）"
    );
    app.on_action(Action::Save);
    assert!(app.client.called_save_config(), "S 才落盘");
}

/// `A`：**未实现**的效果必须拒绝下发并给出原因（不得假装）。
#[test]
fn apply_transition_refuses_unimplemented_effect() {
    let mut app = app();
    app.state.transition.effect = "random".to_string();
    app.on_action(Action::ApplyTransition);
    assert!(
        !app.client
            .calls
            .iter()
            .any(|c| matches!(c, MockCall::SetTransition(_))),
        "未实现的效果不得下发（daemon 只会回退成 fade）"
    );
    assert!(
        app.state
            .notifications
            .iter()
            .any(|n| n.text.contains("尚未实现")),
        "必须明确告知未实现：{:?}",
        app.state.notifications
    );
    // 效果级：直接调用也必须是被拒的（防止将来绕过 UI 接线）。
    let mut st = liuguang_tui::AppState::new();
    st.transition.effect = "none".to_string();
    let fx = apply(&mut st, Action::ApplyTransition);
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::SetTransition(_))),
        "未实现效果不得产生 SetTransition：{fx:?}"
    );
}

/// "随机效果"只从**已实现且可用**的列表里选。
#[test]
fn random_effect_only_from_implemented() {
    for seed in 0..64u64 {
        let picked = next_random_effect(seed);
        assert!(
            EFFECTS_IMPLEMENTED.contains(&picked),
            "随机选到了未实现的效果：{picked}（seed={seed}）"
        );
    }
    // 覆盖全部实现项（否则"随机"退化成一个）。
    let picked: std::collections::BTreeSet<_> = (0..64u64).map(next_random_effect).collect();
    assert_eq!(picked.len(), EFFECTS_IMPLEMENTED.len());
}
