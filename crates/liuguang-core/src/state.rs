//! 顶层运行状态机（规范 §8.3）。
//!
//! ```text
//! Boot
//!   → WaitingForSeat / WaitingForOutput
//!   → InitializingDisplay
//!   → StaticIdle / VideoPlaying
//!        ├── on switch → PreparingNext → Transitioning → StaticIdle / VideoPlaying
//!        │                （无转场时 PreparingNext → StaticIdle / VideoPlaying 直接成立）
//!        ├── on pause  → Paused
//!        ├── on unplug → HeadlessReady
//!        ├── on VT suspend → Suspended
//!        └── on error → DegradedStatic / Recovering
//! ```
//!
//! `PreparingNext` 与 `Transitioning` 都**接受** `Pause` 与 `VtSuspended`：
//! 加载与转场本身可能持续数百毫秒到 10 秒，期间用户的暂停意图、以及 VT 切换
//! 导致的设备失效，都不允许被"正在忙"吞掉（规范 §8.3/§11.2）。
//!
//! 所有转移都必须可记录（`last_transitions`）并可出现在日志与 CLI 状态中。
//! 竞态处理（暂停/恢复/热插拔/异步加载）由上层串行化事件后送入本状态机。

/// 运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// 进程启动。
    Boot,
    /// 等待获取本地 seat / DRM 权限。
    WaitingForSeat,
    /// 等待显示器连接。
    WaitingForOutput,
    /// 正在初始化显示（枚举、modeset）。
    InitializingDisplay,
    /// 静态画面稳定显示（无渲染循环）。
    StaticIdle,
    /// 视频播放中。
    VideoPlaying,
    /// 正在准备下一个媒体项（预加载/解码）。
    PreparingNext,
    /// 正在执行切换转场。
    Transitioning,
    /// 已暂停。
    Paused,
    /// 无显示器可用（服务在线，可热插拔恢复）。
    HeadlessReady,
    /// VT 切换/设备权限失效导致挂起。
    Suspended,
    /// 降级为静态/后备画面。
    DegradedStatic,
    /// 恢复中。
    Recovering,
}

impl State {
    /// 状态名（用于日志）。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Boot => "Boot",
            Self::WaitingForSeat => "WaitingForSeat",
            Self::WaitingForOutput => "WaitingForOutput",
            Self::InitializingDisplay => "InitializingDisplay",
            Self::StaticIdle => "StaticIdle",
            Self::VideoPlaying => "VideoPlaying",
            Self::PreparingNext => "PreparingNext",
            Self::Transitioning => "Transitioning",
            Self::Paused => "Paused",
            Self::HeadlessReady => "HeadlessReady",
            Self::Suspended => "Suspended",
            Self::DegradedStatic => "DegradedStatic",
            Self::Recovering => "Recovering",
        }
    }

    /// 是否处于稳定展示状态（不产生周期性渲染）。
    pub const fn is_stable_display(self) -> bool {
        matches!(self, Self::StaticIdle | Self::VideoPlaying)
    }
}

/// 触发状态转移的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    /// 尚未获得 seat / DRM 权限。
    SeatNotReady,
    /// seat / DRM 权限已获得。
    SeatReady,
    /// 没有可用输出。
    OutputMissing,
    /// 输出已连接。
    OutputConnected,
    /// 显示初始化完成。
    DisplayInitialized,
    /// 静态画面已开始显示。
    StaticShown,
    /// 视频已开始播放。
    VideoStarted,
    /// 请求切换媒体。
    SwitchRequested,
    /// 下一项准备完成。
    NextPrepared,
    /// 暂停。
    Pause,
    /// 恢复。
    Resume,
    /// 输出断开。
    OutputLost,
    /// VT 挂起。
    VtSuspended,
    /// VT 恢复。
    VtResumed,
    /// 发生错误。
    ErrorOccurred,
    /// 恢复成功。
    Recovered,
}

/// 一次成功转移的记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// 转移前状态。
    pub from: State,
    /// 触发事件。
    pub event: Event,
    /// 转移后状态。
    pub to: State,
}

/// 状态机错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("非法状态转移: {from:?} + {event:?}")]
pub struct StateError {
    /// 当前状态。
    pub from: State,
    /// 被拒绝的事件。
    pub event: Event,
}

/// 计算 `(from, event)` 的合法转移目标；非法返回 `None`。
const fn next_state(from: State, event: Event) -> Option<State> {
    use Event as E;
    use State as S;
    match (from, event) {
        // Boot
        (S::Boot, E::SeatNotReady) => Some(S::WaitingForSeat),
        (S::Boot, E::OutputMissing) => Some(S::WaitingForOutput),
        (S::Boot, E::DisplayInitialized) => Some(S::InitializingDisplay),
        // WaitingForSeat
        (S::WaitingForSeat, E::SeatReady) => Some(S::InitializingDisplay),
        (S::WaitingForSeat, E::OutputMissing) => Some(S::WaitingForOutput),
        // WaitingForOutput
        (S::WaitingForOutput, E::OutputConnected) => Some(S::InitializingDisplay),
        (S::WaitingForOutput, E::SeatNotReady) => Some(S::WaitingForSeat),
        // 已经"没有输出可等"了还收到输出丢失：等价于保持 HeadlessReady。
        // 热插拔复查是周期性的，重复送达不得被当成接线错误（否则 WARN 噪声会
        // 掩盖真正的非法转移）。
        (S::WaitingForOutput, E::OutputLost) => Some(S::HeadlessReady),
        // InitializingDisplay
        (S::InitializingDisplay, E::StaticShown) => Some(S::StaticIdle),
        (S::InitializingDisplay, E::VideoStarted) => Some(S::VideoPlaying),
        (S::InitializingDisplay, E::ErrorOccurred) => Some(S::Recovering),
        (S::InitializingDisplay, E::OutputMissing) => Some(S::HeadlessReady),
        // **真机缺陷（2026-10-28，Fedora 44 / 无显示器）**：启动时
        // `DisplayInitialized` 把状态置为 `InitializingDisplay`；若此时根本没有
        // 已连接输出，周期复查报的是 `OutputLost` 而不是 `OutputMissing`。
        // 旧表只允许后者 ⇒ 转移被拒、状态永久卡在 `InitializingDisplay`
        // （规范 §4.3 要求"无 connector 可用时进入 HeadlessReady"）。
        // 首次上屏前丢失输出与"从未有过输出"语义相同，必须落 HeadlessReady。
        (S::InitializingDisplay, E::OutputLost) => Some(S::HeadlessReady),
        (S::InitializingDisplay, E::VtSuspended) => Some(S::Suspended),
        // 稳定展示
        (S::StaticIdle | S::VideoPlaying, E::SwitchRequested) => Some(S::PreparingNext),
        (S::StaticIdle | S::VideoPlaying, E::Pause) => Some(S::Paused),
        (S::StaticIdle | S::VideoPlaying, E::OutputLost) => Some(S::HeadlessReady),
        (S::StaticIdle | S::VideoPlaying, E::VtSuspended) => Some(S::Suspended),
        (S::StaticIdle | S::VideoPlaying, E::ErrorOccurred) => Some(S::DegradedStatic),
        // PreparingNext：加载期间用户仍可暂停、VT 仍可切走（规范 §8.3 —— 用户的
        // 暂停意图与 VT 挂起不得被"正在加载"吃掉）；加载完成后可直接宣布新画面生效，
        // 不必先经过 Transitioning（无转场时这条路径必须合法）。
        (S::PreparingNext, E::NextPrepared) => Some(S::Transitioning),
        (S::PreparingNext, E::StaticShown) => Some(S::StaticIdle),
        (S::PreparingNext, E::VideoStarted) => Some(S::VideoPlaying),
        (S::PreparingNext, E::Pause) => Some(S::Paused),
        (S::PreparingNext, E::VtSuspended) => Some(S::Suspended),
        (S::PreparingNext, E::ErrorOccurred) => Some(S::DegradedStatic),
        (S::PreparingNext, E::OutputLost) => Some(S::HeadlessReady),
        // Transitioning：转场结束即新画面生效（§8.3 → StaticIdle / VideoPlaying）；
        // 转场期间（最长 10s）暂停与 VT 挂起同样必须被接受。
        (S::Transitioning, E::StaticShown) => Some(S::StaticIdle),
        (S::Transitioning, E::VideoStarted) => Some(S::VideoPlaying),
        (S::Transitioning, E::Pause) => Some(S::Paused),
        (S::Transitioning, E::VtSuspended) => Some(S::Suspended),
        (S::Transitioning, E::ErrorOccurred) => Some(S::DegradedStatic),
        (S::Transitioning, E::OutputLost) => Some(S::HeadlessReady),
        // Paused
        (S::Paused, E::Resume) => Some(S::PreparingNext),
        (S::Paused, E::StaticShown) => Some(S::StaticIdle),
        (S::Paused, E::VideoStarted) => Some(S::VideoPlaying),
        // 暂停期间请求切换：直接进入准备（切换不解除暂停，暂停语义由调用方保证）
        (S::Paused, E::SwitchRequested) => Some(S::PreparingNext),
        (S::Paused, E::OutputLost) => Some(S::HeadlessReady),
        (S::Paused, E::VtSuspended) => Some(S::Suspended),
        // HeadlessReady
        (S::HeadlessReady, E::OutputConnected) => Some(S::InitializingDisplay),
        (S::HeadlessReady, E::VtSuspended) => Some(S::Suspended),
        // Suspended
        (S::Suspended, E::VtResumed) => Some(S::InitializingDisplay),
        (S::Suspended, E::OutputLost) => Some(S::HeadlessReady),
        // DegradedStatic
        (S::DegradedStatic, E::Recovered) => Some(S::StaticIdle),
        (S::DegradedStatic, E::ErrorOccurred) => Some(S::Recovering),
        (S::DegradedStatic, E::SwitchRequested) => Some(S::PreparingNext),
        (S::DegradedStatic, E::VtSuspended) => Some(S::Suspended),
        (S::DegradedStatic, E::Pause) => Some(S::Paused),
        (S::DegradedStatic, E::OutputLost) => Some(S::HeadlessReady),
        // Recovering
        (S::Recovering, E::Recovered) => Some(S::InitializingDisplay),
        (S::Recovering, E::ErrorOccurred) => Some(S::DegradedStatic),
        (S::Recovering, E::OutputLost) => Some(S::HeadlessReady),
        _ => None,
    }
}

/// 顶层状态机。事件单线程串行处理；转移历史有界保存供诊断。
#[derive(Debug)]
pub struct StateMachine {
    current: State,
    history: Vec<Transition>,
    history_cap: usize,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    /// 从 [`State::Boot`] 启动，保留最近 64 条转移记录。
    pub fn new() -> Self {
        Self {
            current: State::Boot,
            history: Vec::new(),
            history_cap: 64,
        }
    }

    /// 当前状态。
    pub fn current(&self) -> State {
        self.current
    }

    /// 最近转移记录（旧→新）。
    pub fn history(&self) -> &[Transition] {
        &self.history
    }

    /// 处理事件；非法转移返回 [`StateError`] 且状态不变。
    ///
    /// # Errors
    ///
    /// 当前状态不接受该事件时返回 [`StateError`]。
    pub fn handle(&mut self, event: Event) -> Result<Transition, StateError> {
        match next_state(self.current, event) {
            Some(to) => {
                let t = Transition {
                    from: self.current,
                    event,
                    to,
                };
                self.current = to;
                if self.history.len() >= self.history_cap {
                    self.history.remove(0);
                }
                self.history.push(t);
                Ok(t)
            }
            None => Err(StateError {
                from: self.current,
                event,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_paths() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.current(), State::Boot);
        assert_eq!(
            sm.handle(Event::SeatNotReady).unwrap().to,
            State::WaitingForSeat
        );
        assert_eq!(
            sm.handle(Event::SeatReady).unwrap().to,
            State::InitializingDisplay
        );

        let mut sm = StateMachine::new();
        assert_eq!(
            sm.handle(Event::OutputMissing).unwrap().to,
            State::WaitingForOutput
        );
        assert_eq!(
            sm.handle(Event::OutputConnected).unwrap().to,
            State::InitializingDisplay
        );
    }

    #[test]
    fn static_and_video_display() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
        assert!(sm.current().is_stable_display());

        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        assert_eq!(
            sm.handle(Event::VideoStarted).unwrap().to,
            State::VideoPlaying
        );
        assert!(sm.current().is_stable_display());
    }

    #[test]
    fn switch_flow_prepares_then_transitions_to_next() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        assert_eq!(
            sm.handle(Event::SwitchRequested).unwrap().to,
            State::PreparingNext
        );
        assert_eq!(
            sm.handle(Event::NextPrepared).unwrap().to,
            State::Transitioning
        );
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
    }

    #[test]
    fn pause_resume_paths() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        assert_eq!(sm.handle(Event::Pause).unwrap().to, State::Paused);
        assert_eq!(sm.handle(Event::Resume).unwrap().to, State::PreparingNext);
        sm.handle(Event::NextPrepared).unwrap();
        assert_eq!(
            sm.handle(Event::VideoStarted).unwrap().to,
            State::VideoPlaying
        );

        // 暂停期间可以直接宣布新画面生效。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::VideoStarted).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
    }

    #[test]
    fn unplug_and_hotplug() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        assert_eq!(
            sm.handle(Event::OutputLost).unwrap().to,
            State::HeadlessReady
        );
        assert_eq!(
            sm.handle(Event::OutputConnected).unwrap().to,
            State::InitializingDisplay
        );
    }

    /// 真机回归（2026-10-28，Fedora 44 / 无显示器）：
    /// 启动时 `DisplayInitialized` 已把状态置为 `InitializingDisplay`；若此时
    /// 根本没有已连接输出，周期复查报的是 `OutputLost`。旧表只接受
    /// `OutputMissing` ⇒ 状态永久卡在 `InitializingDisplay`（实测 25h 未恢复），
    /// 违反规范 §4.3"无 connector 可用 ⇒ HeadlessReady"。
    #[test]
    fn output_lost_before_first_paint_reaches_headless_ready() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        assert_eq!(
            sm.handle(Event::OutputLost).unwrap().to,
            State::HeadlessReady,
            "首次上屏前丢失输出必须进入 HeadlessReady"
        );
        // 热插拔重连 → 恢复初始化；再次断开（仍未首帧）→ 仍须可再次落 HeadlessReady。
        assert_eq!(
            sm.handle(Event::OutputConnected).unwrap().to,
            State::InitializingDisplay
        );
        assert_eq!(
            sm.handle(Event::OutputLost).unwrap().to,
            State::HeadlessReady,
            "重复的 OutputLost 不得把状态机卡死"
        );
    }

    /// `WaitingForOutput` 期间的重复 `OutputLost` 同样必须被接受。
    #[test]
    fn repeated_output_lost_while_waiting_is_accepted() {
        let mut sm = StateMachine::new();
        sm.handle(Event::OutputMissing).unwrap();
        assert_eq!(sm.current(), State::WaitingForOutput);
        assert_eq!(
            sm.handle(Event::OutputLost).unwrap().to,
            State::HeadlessReady
        );
    }

    #[test]
    fn vt_suspend_and_resume() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        assert_eq!(sm.handle(Event::VtSuspended).unwrap().to, State::Suspended);
        assert_eq!(
            sm.handle(Event::VtResumed).unwrap().to,
            State::InitializingDisplay
        );
    }

    #[test]
    fn error_and_recovery_paths() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        assert_eq!(
            sm.handle(Event::ErrorOccurred).unwrap().to,
            State::DegradedStatic
        );
        assert_eq!(sm.handle(Event::Recovered).unwrap().to, State::StaticIdle);

        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        sm.handle(Event::ErrorOccurred).unwrap();
        assert_eq!(
            sm.handle(Event::ErrorOccurred).unwrap().to,
            State::Recovering
        );
        assert_eq!(
            sm.handle(Event::Recovered).unwrap().to,
            State::InitializingDisplay
        );
        assert_eq!(
            sm.handle(Event::ErrorOccurred).unwrap().to,
            State::Recovering
        );
        assert_eq!(
            sm.handle(Event::ErrorOccurred).unwrap().to,
            State::DegradedStatic
        );
    }

    #[test]
    fn illegal_transitions_are_rejected_without_state_change() {
        let mut sm = StateMachine::new();
        for event in [Event::Resume, Event::Pause, Event::NextPrepared] {
            let err = sm.handle(event).unwrap_err();
            assert_eq!(
                err,
                StateError {
                    from: State::Boot,
                    event
                }
            );
        }
        assert_eq!(sm.current(), State::Boot);
        assert!(sm.history().is_empty());

        sm.handle(Event::DisplayInitialized).unwrap();
        let err = sm.handle(Event::VtResumed).unwrap_err();
        assert_eq!(
            err,
            StateError {
                from: State::InitializingDisplay,
                event: Event::VtResumed
            }
        );
    }

    #[test]
    fn history_is_bounded() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        for _ in 0..100 {
            sm.handle(Event::ErrorOccurred).unwrap();
            sm.handle(Event::Recovered).unwrap();
        }
        assert!(sm.history().len() <= 64);
        assert_eq!(sm.current(), State::StaticIdle);
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(State::Boot.name(), "Boot");
        assert_eq!(State::HeadlessReady.name(), "HeadlessReady");
    }

    /// 回归：`resume` 之后顶层状态机不得卡在 `PreparingNext`（审计 P0-3）。
    ///
    /// `apps/liuguang::handle_resume` 的实际序列是
    /// `Resume` →（静态图）`StaticShown` / （视频）`VideoStarted`；
    /// 若 `PreparingNext` 不接受这两个事件，`status.state` 会永久停在中间态。
    #[test]
    fn resume_then_shown_reaches_stable_state() {
        // 静态图路径
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert_eq!(sm.current(), State::Paused);
        sm.handle(Event::Resume).unwrap();
        assert_eq!(sm.current(), State::PreparingNext);
        let t = sm
            .handle(Event::StaticShown)
            .expect("resume 后宣布静态画面生效必须合法");
        assert_eq!(t.to, State::StaticIdle);
        assert!(sm.current().is_stable_display());

        // 视频路径
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::VideoStarted).unwrap();
        sm.handle(Event::Pause).unwrap();
        sm.handle(Event::Resume).unwrap();
        assert_eq!(
            sm.handle(Event::VideoStarted).expect("视频路径同理").to,
            State::VideoPlaying
        );
        assert!(sm.current().is_stable_display());
    }

    /// 回归：加载/转场期间按暂停、以及 VT 切走，都必须被接受（审计 P2-16）。
    #[test]
    fn pause_and_vt_are_accepted_while_busy() {
        for busy in [State::PreparingNext, State::Transitioning] {
            let mut sm = StateMachine::new();
            sm.handle(Event::DisplayInitialized).unwrap();
            sm.handle(Event::StaticShown).unwrap();
            sm.handle(Event::SwitchRequested).unwrap();
            if busy == State::Transitioning {
                sm.handle(Event::NextPrepared).unwrap();
            }
            assert_eq!(sm.current(), busy, "前置条件");

            assert_eq!(
                sm.handle(Event::Pause)
                    .expect("忙碌期间的暂停不得被丢弃")
                    .to,
                State::Paused
            );
            assert_eq!(
                sm.handle(Event::VtSuspended)
                    .expect("忙碌期间的 VT 挂起")
                    .to,
                State::Suspended
            );
        }

        // 初始化期间 VT 挂起（显示器 modeset 到一半被切走）。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        assert_eq!(sm.handle(Event::VtSuspended).unwrap().to, State::Suspended);
    }

    /// 回归：无转场路径（`next` 且 transition.effect = "none"）必须能从
    /// `PreparingNext` 直接落到稳定态，而不是只能经 `Transitioning`。
    #[test]
    fn no_transition_switch_can_finish_from_preparing_next() {
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        sm.handle(Event::SwitchRequested).unwrap();
        assert_eq!(sm.current(), State::PreparingNext);
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
    }
}
