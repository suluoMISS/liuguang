//! 文本宽度与截断工具：**按显示宽度**处理，禁止按字节切断 UTF-8/全角字符。
//!
//! 中文文件名、全角标点、组合字符都不能被"切一半"；因此所有截断都
//! 逐 `char` 累计 `unicode-width`，超宽时补 `…`（省略号本身也占 1 列）。

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 字符串的终端显示宽度（全角字符 = 2 列）。
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 按显示宽度截断到 `max_cols` 列；超长时以 `…` 结尾。
///
/// - `max_cols == 0` 返回空串；
/// - 不切断任何 `char`（全角字符要么完整保留，要么整体省略）；
/// - 结果宽度保证 `<= max_cols`。
pub fn truncate_display(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    // 留 1 列给省略号
    let budget = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// 右侧补空格到指定显示宽度（已超宽则原样返回，不做截断）。
pub fn pad_display(s: &str, cols: usize) -> String {
    let w = display_width(s);
    if w >= cols {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(cols - w))
}

/// 按显示宽度换行（用于完整路径展示；不丢字符）。
pub fn wrap_display(s: &str, cols: usize) -> Vec<String> {
    if cols == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > cols && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(c);
        used += w;
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn width_counts_fullwidth_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("中a"), 3);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn truncate_never_splits_a_char() {
        let name = "城市夜景_最终版_3840x2400.png";
        for cols in 1..40 {
            let out = truncate_display(name, cols);
            assert!(
                display_width(&out) <= cols,
                "cols={cols} 结果 `{out}` 宽度 {}",
                display_width(&out)
            );
            // 结果必须是原串的前缀（可能带省略号）——证明没有按字节切
            let body = out.trim_end_matches('…');
            assert!(name.starts_with(body), "`{out}` 不是 `{name}` 的前缀");
            // 不含替换字符（UTF-8 被切开的典型症状）
            assert!(!out.contains('\u{FFFD}'), "出现替换字符：{out}");
        }
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate_display("abc", 10), "abc");
        assert_eq!(truncate_display("中文", 4), "中文");
        assert_eq!(truncate_display("中文", 0), "");
        // 全角字符放不进 1 列时整体省略（不留半个字）
        assert_eq!(truncate_display("中", 1), "…");
    }

    #[test]
    fn wrap_display_preserves_all_chars() {
        let path = "/srv/wallpapers/收藏/2026/城市夜景_最终版.png";
        let lines = wrap_display(path, 8);
        assert!(lines.iter().all(|l| display_width(l) <= 8));
        assert_eq!(lines.concat(), path, "换行不得丢字符");
    }

    #[test]
    fn pad_display_accounts_for_fullwidth() {
        assert_eq!(display_width(&pad_display("中文", 6)), 6);
        assert_eq!(display_width(&pad_display("abcd", 4)), 4);
        assert_eq!(pad_display("abcdef", 3), "abcdef");
    }
}
