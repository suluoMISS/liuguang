//! `liuguangctl` 退出码的**进程级**回归测试。
//!
//! 为什么不用单元测试：审计 §5 指出"只调用库函数"的测试覆盖不到真实进程行为——
//! 退出码是 CLI 对外的契约（脚本 `|| fallback` 依赖它），必须用真进程验证。
//! 约定（`docs/IPC_PROTOCOL.md` §7）：
//!
//! | 码 | 含义 |
//! |---|---|
//! | 0 | 成功 |
//! | 1 | 服务端拒绝 / 一般错误 |
//! | 2 | 命令行用法错误（含参数越界） |
//! | 3 | liuguang 不可达 |
//!
//! 回归覆盖：`--duration 99999` 参数越界、`--zoom nan` 非法浮点与 `--zoom -1` 负数传参。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_liuguangctl");

/// 一个必然不存在的 socket 路径（用于触发"不可达 = 3"）。
fn dead_socket() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "liuguangctl-exit-test-{}-nonexistent.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("无法启动 liuguangctl");
    let code = out.status.code().unwrap_or(-1);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (code, text)
}

#[test]
fn help_exits_zero() {
    let (code, text) = run(&["--help"]);
    assert_eq!(code, 0, "`--help` 应退出 0: {text}");
    assert!(text.contains("liuguangctl"), "help 应打印用法: {text}");
}

#[test]
fn out_of_range_duration_exits_2_not_1() {
    let (code, text) = run(&["transition", "fade", "--duration", "99999"]);
    assert_eq!(
        code, 2,
        "越界 --duration 是用法错误（2），审计 P2-13；实际 {code}: {text}"
    );
    assert!(
        text.contains("10000") || text.contains("duration"),
        "报错应指出范围: {text}"
    );
}

#[test]
fn invalid_effect_exits_2() {
    let (code, text) = run(&["transition", "bogus"]);
    assert_eq!(code, 2, "非法枚举是用法错误: {code}: {text}");
}

#[test]
fn non_finite_zoom_exits_2() {
    for zoom in ["nan", "inf"] {
        let (code, text) = run(&["set", "/tmp/a.png", "--zoom", zoom]);
        assert_eq!(code, 2, "--zoom {zoom} 应退出 2；实际 {code}: {text}");
    }
}

#[test]
fn negative_zoom_exits_2_with_value_message() {
    let (code, text) = run(&["set", "/tmp/a.png", "--zoom", "-1"]);
    assert_eq!(code, 2, "--zoom -1 应退出 2；实际 {code}: {text}");
    // 关键：必须是"值非法"而不是 clap 的 `unexpected argument '-1'`
    // （后者在审计 P2-13 中被记为"不可传参"）。
    assert!(
        !text.contains("unexpected argument") && !text.contains("意外参数"),
        "--zoom -1 应作为值被校验，而不是当作未知选项: {text}"
    );
}

#[test]
fn unreachable_daemon_exits_3() {
    let socket = dead_socket();
    let status = run(&["--socket", socket.to_str().unwrap(), "status"]);
    assert_eq!(status.0, 3, "socket 不存在应退出 3: {status:?}");
    assert!(
        status.1.contains("无法连接"),
        "应说明连接失败: {}",
        status.1
    );
}

/// 用法错误（2）优先于连接错误（3）：参数没写完没资格去连守护进程。
#[test]
fn usage_error_wins_over_unreachable_daemon() {
    let socket = dead_socket();
    let (code, _) = run(&[
        "--socket",
        socket.to_str().unwrap(),
        "transition",
        "fade",
        "--duration",
        "99999",
    ]);
    assert_eq!(code, 2, "用法错误应先于连接被报出");
}
