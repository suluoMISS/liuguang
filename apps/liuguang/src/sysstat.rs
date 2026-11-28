//! 进程自观测（`get_metrics` 的 CPU/内存/线程/帧率来源）。
//!
//! 只读 `/proc/self/*`，不引入新依赖、不 spawn 子进程。
//!
//! **CPU% 的口径**（如实说明）：`/proc/self/stat` 的 utime/stime 单位是
//! `USER_HZ`。x86_64 Linux 上 `USER_HZ` 恒为 **100**（与内核 CONFIG_HZ 无关），
//! 因此这里用 [`USER_HZ`] 常量换算；若将来移植到 USER_HZ≠100 的平台，
//! 该常量必须改（错误表现为 CPU% 成比例偏差，不会静默为 0）。
//!
//! 帧率口径：`presented_ok`（**提交成功**的帧数）的增量 ÷ 墙钟间隔——
//! 不是"解码出队帧率"，所以"提交失败仍然计入 fps"这类假象不会出现。

use std::time::Instant;

/// `USER_HZ`：x86_64 Linux 恒为 100。
const USER_HZ: u64 = 100;

/// 一次采样的原始计数。
#[derive(Debug, Clone, Copy)]
struct Sample {
    cpu_ticks: u64,
    presented_ok: u64,
    at: Instant,
}

/// 自观测快照。
#[derive(Debug, Clone, Copy, Default)]
pub struct SysSnapshot {
    /// 进程 CPU 占用（**单核百分比**：100% = 跑满一个核）。
    pub cpu_percent: f32,
    /// 常驻内存（字节）。
    pub mem_bytes: u64,
    /// 线程数。
    pub threads: u32,
    /// 提交成功帧率（基于 `presented_ok` 增量）。
    pub fps: f32,
}

/// 采样器（保存上一次的计数用于求增量）。
#[derive(Debug, Default)]
pub struct SysStat {
    last: Option<Sample>,
}

impl SysStat {
    /// 采样一次；`presented_ok` 由调用方传入（显示侧累计提交成功帧数）。
    ///
    /// 第一次调用没有增量基线：CPU%/fps 返回 0（不假装有数据）。
    pub fn sample(&mut self, presented_ok: u64) -> SysSnapshot {
        let at = Instant::now();
        let cpu_ticks = read_cpu_ticks().unwrap_or(0);
        let (mem_bytes, threads) = read_mem_and_threads();
        let mut snap = SysSnapshot {
            cpu_percent: 0.0,
            mem_bytes,
            threads,
            fps: 0.0,
        };
        if let Some(last) = self.last {
            let wall = at.duration_since(last.at).as_secs_f64();
            if wall > 0.0 {
                let dt = cpu_ticks.saturating_sub(last.cpu_ticks) as f64 / USER_HZ as f64;
                // 单核百分比：可超过 100（多线程），与 top 的 %CPU 同口径。
                snap.cpu_percent = (dt / wall * 100.0) as f32;
                snap.fps = (presented_ok.saturating_sub(last.presented_ok) as f64 / wall) as f32;
            }
        }
        self.last = Some(Sample {
            cpu_ticks,
            presented_ok,
            at,
        });
        snap
    }
}

/// 读 `/proc/self/stat` 的 utime+stime（USER_HZ ticks）。
fn read_cpu_ticks() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    // comm 字段可能含空格/括号：从**最后一个** `)` 之后开始切。
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `)` 之后第一个字段是 state（原第 3 字段），utime/stime 是原第 14/15 字段。
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// 读当前进程常驻内存（`/proc/self/statm` 第 2 字段 × 页大小，字节）。
pub(crate) fn read_mem_bytes() -> u64 {
    let page = 4096_u64; // x86_64 默认页大小；statm 单位为页。
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|t| {
            t.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|pages| pages.saturating_mul(page))
        .unwrap_or(0)
}

/// 读常驻内存与线程数（`/proc/self/status`）。
fn read_mem_and_threads() -> (u64, u32) {
    let rss = read_mem_bytes();
    let threads = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0);
    (rss, threads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sample_reports_memory_and_threads() {
        let mut stat = SysStat::default();
        let snap = stat.sample(0);
        assert!(snap.mem_bytes > 0, "进程自身 RSS 不可能为 0");
        assert!(snap.threads >= 1, "至少有主线程");
        // 首次采样没有基线：不假装有 CPU%/fps。
        assert_eq!(snap.cpu_percent, 0.0);
        assert_eq!(snap.fps, 0.0);
    }

    #[test]
    fn second_sample_computes_positive_cpu_and_fps() {
        let mut stat = SysStat::default();
        // **T65**：基线必须在**忙等之前**采样。
        //
        // 原实现是"先忙等 120ms → 第一次采样 → `sleep(20ms)` → 第二次采样"：
        // 那 20ms 里本线程在睡，进程 CPU 增量≈0 ⇒ `cpu_percent > 0` 只能靠
        // **并行时别的测试在烧 CPU** 才成立（`--test-threads=1` 或单独跑必红，
        // 报"实际 0"）。这条断言把"门禁绿"绑在了执行方式上（t63 同族）。
        let before = stat.sample(0);
        assert_eq!(before.cpu_percent, 0.0, "首次采样没有基线，不得假装有 CPU%");
        let base_ticks = read_cpu_ticks().expect("必须能读 /proc/self/stat");
        // 忙等到**确实烧掉 ≥3 ticks（30ms CPU，USER_HZ=100）**。
        //
        // 不写"忙等 20ms 再祈祷至少走 2 个 tick"：tick 是 10ms 粒度，墙钟 20ms
        // 在繁忙机器上可能只对应 <1 个 tick（或被调度器饿死）——
        // 以"观察到的 CPU tick 增量"为终止条件，才与调度无关。
        let mut acc = 0u64;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            acc = acc.wrapping_add(1);
            let now_ticks = read_cpu_ticks().expect("必须能读 /proc/self/stat");
            if now_ticks.saturating_sub(base_ticks) >= 3 || Instant::now() >= deadline {
                break;
            }
        }
        assert!(acc > 0, "忙等循环必须真的执行过（不能被优化掉）");
        let second = stat.sample(250);
        assert!(
            second.cpu_percent > 0.0,
            "两次采样之间**本线程**确实烧了 ≥30ms CPU，必须报正数（实际 {}；基线 cpu={}）",
            second.cpu_percent,
            before.cpu_percent
        );
        assert!(
            second.cpu_percent.is_finite(),
            "CPU% 必须是有限值（实际 {}）",
            second.cpu_percent
        );
        // 采样窗口内 presented_ok 增 250 帧 ⇒ 公式必须为正且有限（不看具体数值：
        // 窗口长度取决于真实 CPU 时间，写死 fps 阈值只会再造一条时序断言）。
        assert!(second.fps > 0.0 && second.fps.is_finite());
    }

    #[test]
    fn cpu_ticks_are_readable() {
        let a = read_cpu_ticks().expect("必须能读 /proc/self/stat");
        let mut acc = 0u64;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(50) {
            acc = acc.wrapping_add(1);
        }
        assert!(acc > 0);
        let b = read_cpu_ticks().expect("必须能读 /proc/self/stat");
        assert!(b >= a, "CPU ticks 单调不减");
    }
}
