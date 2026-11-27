//! 合成器能力画像：谁的 NV12 DMA-BUF 只能采样到亮度。
//!
//! 背景（见 `docs/ARCHITECTURE.md` §8.4.2）：把**多平面 YUV** 的 `EGLImage`
//! 当成一张普通纹理去采样时，某些渲染栈只取到 plane 0（Y），于是整屏只有
//! 红色通道有内容。这不是协议层能力缺失——出问题的合成器照样广播
//! `NV12`/`P010` 等 49 种 fourcc——所以**不能靠探测格式列表判断**，只能按
//! 已知实现登记，并允许用户用配置强制覆盖。
//!
//! 已知案例：
//! - `denial` 0.6.x（Flutter/Impeller）：实测 `(109,0,0)` 单色红，
//!   同一块 buffer 在 niri（Smithay GL，每平面单独纹理 + shader）显示正常。
//!
//! 判定口径故意分两层：
//! 1. `KNOWN_LUMA_ONLY` 是"我们已确认有此缺陷的合成器名单"——新增条目
//!    必须附复现证据（像素统计或截图），不要凭猜测登记；
//! 2. 用户可以在 `[media] video_dmabuf_rgb` 写 `always`/`never` 覆盖，
//!    因为名单永远不可能穷尽，而且合成器修好后我们也不该继续降级。

use std::collections::BTreeSet;

/// 已知"多平面 YUV 只取到亮度"的合成器（小写、无空格）。
pub const KNOWN_LUMA_ONLY: &[&str] = &["denial"];

/// `[media] video_dmabuf_rgb` 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbConversion {
    /// 按名单自动判断（默认）。
    Auto,
    /// 一律先转 RGB（用于名单没收录但同样画错的合成器）。
    Always,
    /// 一律不转（用于名单已过期、合成器已修好的情况）。
    Never,
}

impl RgbConversion {
    /// 解析配置值；未知值返回 `None`（调用方打 WARN 后按 `Auto` 处理）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "always" | "on" | "1" | "yes" | "true" | "rgb" => Some(Self::Always),
            "never" | "off" | "0" | "no" | "false" | "native" => Some(Self::Never),
            _ => None,
        }
    }
}

/// 会话里用来识别合成器的字符串（`XDG_CURRENT_DESKTOP` 等），已规范化。
pub fn compositor_names<I, S>(values: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = BTreeSet::new();
    for value in values {
        for part in value.as_ref().split(':') {
            let name = part.trim().to_ascii_lowercase();
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

/// 该名字是否在"只取亮度"名单里。
pub fn known_luma_only(name: &str) -> bool {
    KNOWN_LUMA_ONLY.contains(&name)
}

/// 是否需要先转 RGB 再交给合成器。
///
/// `names` 用 [`compositor_names`] 规范化；`forced` 来自配置/环境变量。
pub fn needs_rgb_conversion(names: &BTreeSet<String>, forced: RgbConversion) -> bool {
    match forced {
        RgbConversion::Always => true,
        RgbConversion::Never => false,
        RgbConversion::Auto => names.iter().any(|name| known_luma_only(name)),
    }
}

/// 从进程环境读合成器名字集合（三个 XDG 变量 + `XDG_SESSION_TYPE` 不参与）。
pub fn detect_names() -> BTreeSet<String> {
    let values = [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok());
    compositor_names(values)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn splits_colon_separated_desktop_values() {
        let names = compositor_names(["niri:Denial", "denial", "GNOME-Classic:KDE"]);
        assert!(names.contains("niri"));
        assert!(names.contains("denial"));
        assert!(names.contains("gnome-classic"));
        assert!(names.contains("kde"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn empty_and_blank_values_are_ignored() {
        let names = compositor_names(["", "  ", "::", "niri"]);
        assert_eq!(names.len(), 1, "只有 niri 应留下：{names:?}");
    }

    #[test]
    fn known_list_matches_documented_compositor() {
        assert!(known_luma_only("denial"));
        assert!(!known_luma_only("niri"));
        assert!(!known_luma_only("sway"));
    }

    #[test]
    fn auto_uses_the_known_list() {
        let denial = compositor_names(["Denial"]);
        let niri = compositor_names(["niri"]);
        assert!(needs_rgb_conversion(&denial, RgbConversion::Auto));
        assert!(!needs_rgb_conversion(&niri, RgbConversion::Auto));
    }

    #[test]
    fn forced_values_win_over_the_list() {
        let niri = compositor_names(["niri"]);
        let denial = compositor_names(["denial"]);
        assert!(needs_rgb_conversion(&niri, RgbConversion::Always));
        assert!(!needs_rgb_conversion(&denial, RgbConversion::Never));
    }

    #[test]
    fn config_values_parse() {
        assert_eq!(RgbConversion::parse("auto"), Some(RgbConversion::Auto));
        assert_eq!(RgbConversion::parse(""), Some(RgbConversion::Auto));
        assert_eq!(
            RgbConversion::parse(" ALWAYS "),
            Some(RgbConversion::Always)
        );
        assert_eq!(RgbConversion::parse("rgb"), Some(RgbConversion::Always));
        assert_eq!(RgbConversion::parse("never"), Some(RgbConversion::Never));
        assert_eq!(RgbConversion::parse("native"), Some(RgbConversion::Never));
        assert_eq!(RgbConversion::parse("sometimes"), None);
    }
}
