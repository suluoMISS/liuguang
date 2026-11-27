//! 系统 System：运行指标、对账计数、路径（数据全部来自 daemon）。

use crate::model::AppState;
use crate::ui::widgets::{disabled_note, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;

/// 主体：指标（刷新 ≤2 Hz；**计数来自 daemon**）。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    // 本页键普遍较长（「硬解（daemon status）」显示宽 21）：统一 22 列，
    // 既对齐又不截断该键（截断会丢掉 "status" 来源标注）。
    let kv = |k: &str, v: &str, c: usize| crate::ui::widgets::kv_w(k, v, c, 22);
    let m = &st.metrics;
    let mut lines = vec![
        kv("CPU", &format!("{:.1} %", m.cpu_percent), cols),
        kv(
            "内存",
            &format!("{:.1} MiB", m.mem_bytes as f64 / (1024.0 * 1024.0)),
            cols,
        ),
        kv("线程", &m.threads.to_string(), cols),
        kv(
            "帧率",
            &crate::model::format_fps_int(f64::from(m.fps)),
            cols,
        ),
        kv(
            "预读缓存",
            &format!(
                "{} 项 / {:.1} MiB",
                m.preload_cache_items,
                m.preload_cache_bytes as f64 / (1024.0 * 1024.0)
            ),
            cols,
        ),
        kv(
            "暂停状态",
            st.active_pause_reason().unwrap_or("运行中"),
            cols,
        ),
        kv("运行", &format!("{} 秒", m.uptime_seconds), cols),
        text_line("", cols),
    ];
    match &m.commit {
        Some(c) => {
            lines.push(kv("提交成功", &c.commit_success_total.to_string(), cols));
            lines.push(kv("提交尝试", &c.attempts.to_string(), cols));
            if c.failed > 0 {
                lines.push(text_line(" ⚠ 有提交失败：画面可能没有真正更新", cols));
            }
        }
        None => lines.push(text_line(" 提交计数：daemon 未提供（后端不适用）", cols)),
    }
    match &m.master {
        Some(ms) => {
            lines.push(kv(
                "DRM master",
                match ms.master {
                    Some(true) => "yes",
                    Some(false) => "NO —— 画面不会更新",
                    None => "未知（结果待首帧提交确认）",
                },
                cols,
            ));
            if !ms.remedy_hint.is_empty() {
                let hint = ms.remedy_hint.as_str();
                lines.push(text_line(&format!(" remedy: {hint}"), cols));
            }
        }
        None => lines.push(text_line(" DRM master：daemon 未提供", cols)),
    }
    // 实际硬解方式：**取自 daemon 的 status**，绝不按配置文件推断（规范 §11）。
    let decoders: Vec<String> = st
        .status
        .channels
        .iter()
        .map(|c| c.decoder.clone())
        .filter(|d| !d.is_empty())
        .collect();
    let decoder_text = if decoders.is_empty() {
        let top = st.status.decoder.clone();
        if top.is_empty() {
            "（daemon 未提供：未连接或无通道）".to_string()
        } else {
            top
        }
    } else {
        decoders.join(", ")
    };
    lines.push(kv("硬解（daemon status）", &decoder_text, cols));
    lines.push(text_line(
        &format!(
            " IPC：{} · 事件流 {} · 会话 {} · 重连尝试 {}",
            if st.connection.is_online() {
                "在线"
            } else {
                "断开（按 e 重连）"
            },
            if st.connection.is_online() {
                "已订阅"
            } else {
                "未订阅"
            },
            st.session_kind,
            st.reconnect_attempts
        ),
        cols,
    ));
    if st.session_kind == "ipc" {
        lines.push(text_line(&format!(" socket：{}", st.socket_path), cols));
    }
    lines.push(text_line(
        &format!(
            " 协议 v{} · daemon 版本：daemon 未提供该字段（不猜）",
            liuguang_ipc::PROTOCOL_VERSION
        ),
        cols,
    ));
    if st.recent_errors.is_empty() {
        lines.push(text_line(" 最近错误：（无）", cols));
    } else {
        lines.push(text_line(
            &format!(
                " 最近错误（最多 {} 条，最新在前）：",
                crate::model::MAX_RECENT_ERRORS
            ),
            cols,
        ));
        for err in st.recent_errors.iter().rev().take(3) {
            lines.push(text_line(&format!("  · {err}"), cols));
        }
    }
    lines.push(text_line("", cols));
    lines.push(text_line(
        " 指标刷新 ≤2 Hz；`A` 只改运行时 · `S` 才落盘（语义分离）",
        cols,
    ));
    section(f, area, "系统", lines);
}

/// 详情栏：路径、来源、扫描与对账说明。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = vec![
        text_line("当前连接与路径", cols),
        text_line(&format!("  socket      {}", st.socket_path), cols),
        text_line(
            &match st.config_path.as_deref() {
                Some(path) => format!("  配置        {path}（daemon 回传）"),
                None => "  配置        （尚未保存过；按 `S` 后 daemon 回传实际路径）".to_string(),
            },
            cols,
        ),
        text_line("  系统状态    /var/lib/liuguang/state.json", cols),
        text_line(&format!("  媒体 root   {}", st.media_dir_hint), cols),
        text_line("", cols),
        text_line(
            &format!(
                "媒体来源 {} 个（缺失 {}）",
                st.sources.sources.len(),
                st.sources.missing
            ),
            cols,
        ),
        text_line(&format!("扫描状态 {}", st.scan.label()), cols),
        text_line(
            &format!(
                "事件流 {}",
                if st.connection.is_online() {
                    "已订阅/在线"
                } else {
                    "断开"
                }
            ),
            cols,
        ),
        disabled_note(
            "`S` 保存配置（daemon 原子写入；非 root 写 root 配置会 PERMISSION_DENIED）",
            cols,
        ),
    ];
    if !st.connection.is_online() {
        lines.insert(
            0,
            text_line(
                &format!(" 断线：{}（按 e 重连）", st.connection.label()),
                cols,
            ),
        );
    }
    section(f, area, "路径与来源", lines);
}
