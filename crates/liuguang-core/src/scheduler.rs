//! 定时轮换调度器（M3）：基于 monotonic 时间的间隔触发。
//!
//! 全部时间由调用方以 [`std::time::Instant`] 注入（[`Self::poll`]、
//! [`Self::set_interval`]、[`Self::resume`]），不读取墙上时钟、不 `sleep`，
//! 因此行为可完全单元测试。
//!
//! 语义要点：
//! - [`Self::poll`] 到期返回 `true`，并**从 `now` 起**按 `interval` 重新排期；
//!   单次 `poll` 最多触发一次，**不追赶积压**（长时间未轮询也只补一次）。
//! - 暂停中 [`Self::poll`] 恒返回 `false`；[`Self::resume`] 从给定时刻重新排期。
//! - `interval` 为零是合法输入：此时每次 `poll` 都返回 `true` 且**不排期**
//!   （[`Self::next_due`] 返回 `None`），即"每帧都轮换"。
//! - [`Self::new`] 以 `Instant::now()` 排期首次到期；需要注入时间的测试
//!   请使用 [`Self::set_interval`] 或 [`Self::pause`] + [`Self::resume`] 重排。

use std::time::{Duration, Instant};

/// 单调时钟间隔调度器。
#[derive(Debug, Clone)]
pub struct IntervalScheduler {
    /// 轮换间隔。
    interval: Duration,
    /// 下一次到期时刻；`None` 表示未排期（暂停中或零间隔）。
    next_due: Option<Instant>,
    /// 是否暂停。
    paused: bool,
}

/// 把 `interval` 叠加到 `base` 上，防止超大 `Duration` 触发 `Instant + Duration` 溢出 panic。
fn safe_due(base: Instant, interval: Duration) -> Option<Instant> {
    if interval.is_zero() {
        return None;
    }
    const MAX_CAP: Duration = Duration::from_secs(31_536_000);
    let clamped = interval.min(MAX_CAP);
    base.checked_add(clamped)
        .or_else(|| base.checked_add(Duration::from_secs(86_400)))
}

impl IntervalScheduler {
    /// 创建调度器，首次到期时刻为 `Instant::now() + interval`。
    ///
    /// `interval` 为 [`Duration::ZERO`] 时合法：见 [`Self::poll`] 的零间隔语义。
    pub fn new(interval: Duration) -> Self {
        let next_due = safe_due(Instant::now(), interval);
        Self {
            interval,
            next_due,
            paused: false,
        }
    }

    /// 修改轮换间隔，并从 `now` 重新排期（`interval` 为零时清除排期）。
    pub fn set_interval(&mut self, interval: Duration, now: Instant) {
        self.interval = interval;
        self.next_due = safe_due(now, interval);
    }

    /// 暂停：[`Self::poll`] 恒返回 `false`，排期清除。
    pub fn pause(&mut self) {
        self.paused = true;
        self.next_due = None;
    }

    /// 恢复：从 `now` 起按当前 `interval` 重新排期。
    pub fn resume(&mut self, now: Instant) {
        self.paused = false;
        self.next_due = safe_due(now, self.interval);
    }

    /// 是否处于暂停状态。
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 当前轮换间隔。
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 轮询是否到期。
    ///
    /// - 暂停中恒返回 `false`；
    /// - `interval` 为零时每次返回 `true` 且不排期；
    /// - 到期返回 `true` 并从 `now` 起重新排期（单次最多触发一次，不追赶积压）；
    /// - 未到期返回 `false`。
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.paused {
            return false;
        }
        if self.interval.is_zero() {
            return true;
        }
        match self.next_due {
            Some(due) if now >= due => {
                self.next_due = safe_due(now, self.interval);
                true
            }
            Some(_) => false,
            None => {
                // 未排期（零间隔之外的防御分支）：视为到期并排期。
                self.next_due = safe_due(now, self.interval);
                true
            }
        }
    }

    /// 下一次到期时刻；暂停中或零间隔（未排期）时为 `None`。
    pub fn next_due(&self) -> Option<Instant> {
        self.next_due
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn fires_when_due_and_reschedules_from_now() {
        let mut s = IntervalScheduler::new(Duration::from_secs(10));
        let t0 = base();
        s.set_interval(Duration::from_secs(10), t0);
        assert_eq!(s.next_due(), Some(t0 + Duration::from_secs(10)));

        assert!(!s.poll(t0 + Duration::from_secs(9)));
        assert!(s.poll(t0 + Duration::from_secs(10)));
        // 触发后从 now 重新排期。
        assert_eq!(s.next_due(), Some(t0 + Duration::from_secs(20)));
        assert!(!s.poll(t0 + Duration::from_secs(15)));
        assert!(s.poll(t0 + Duration::from_secs(20)));
    }

    #[test]
    fn zero_interval_fires_every_poll_without_scheduling() {
        let mut s = IntervalScheduler::new(Duration::ZERO);
        assert_eq!(s.next_due(), None);
        let t0 = base();
        assert!(s.poll(t0));
        assert!(s.poll(t0 + Duration::from_secs(1)));
        assert!(s.poll(t0 + Duration::from_secs(3600)));
        assert_eq!(s.next_due(), None);
        assert!(!s.is_paused());
    }

    #[test]
    fn paused_scheduler_never_fires() {
        let mut s = IntervalScheduler::new(Duration::from_secs(5));
        let t0 = base();
        s.set_interval(Duration::from_secs(5), t0);
        s.pause();
        assert!(s.is_paused());
        assert_eq!(s.next_due(), None);
        assert!(!s.poll(t0 + Duration::from_secs(10_000)));

        s.resume(t0 + Duration::from_secs(100));
        assert!(!s.is_paused());
        assert_eq!(s.next_due(), Some(t0 + Duration::from_secs(105)));
        assert!(!s.poll(t0 + Duration::from_secs(104)));
        assert!(s.poll(t0 + Duration::from_secs(105)));
    }

    #[test]
    fn pause_wins_over_zero_interval() {
        let mut s = IntervalScheduler::new(Duration::ZERO);
        s.pause();
        assert!(!s.poll(base()));
        s.resume(base());
        assert!(s.poll(base()));
    }

    #[test]
    fn no_catch_up_on_long_gap() {
        let mut s = IntervalScheduler::new(Duration::from_secs(10));
        let t0 = base();
        s.set_interval(Duration::from_secs(10), t0);

        // 长时间未轮询：单次 poll 最多触发一次。
        assert!(s.poll(t0 + Duration::from_secs(10_000)));
        // 且从触发时刻（now）重新排期，而不是补发积压。
        assert_eq!(s.next_due(), Some(t0 + Duration::from_secs(10_010)));
        assert!(!s.poll(t0 + Duration::from_secs(10_005)));
    }

    #[test]
    fn set_interval_reschedules_from_injected_now() {
        let mut s = IntervalScheduler::new(Duration::from_secs(60));
        let t0 = base();
        s.set_interval(Duration::from_secs(60), t0);
        s.set_interval(Duration::from_secs(1), t0 + Duration::from_secs(10));
        assert_eq!(s.next_due(), Some(t0 + Duration::from_secs(11)));
        assert!(!s.poll(t0 + Duration::from_secs(10)));
        assert!(s.poll(t0 + Duration::from_secs(11)));
    }

    #[test]
    fn set_zero_interval_clears_schedule() {
        let mut s = IntervalScheduler::new(Duration::from_secs(30));
        let t0 = base();
        s.set_interval(Duration::ZERO, t0);
        assert_eq!(s.next_due(), None);
        assert!(s.poll(t0));
    }

    #[test]
    fn new_arms_first_tick_without_sleep() {
        let mut s = IntervalScheduler::new(Duration::from_secs(3600));
        let now = base();
        assert!(!s.is_paused());
        let due = s.next_due().expect("构造后排期存在");
        assert!(due > now, "首次到期应在构造时刻之后");
        assert!(!s.poll(now));
        assert!(!s.poll(now + Duration::from_secs(3599)));
        assert!(s.poll(due));
    }
}
