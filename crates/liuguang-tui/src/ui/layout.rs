//! 布局：四档自适应（`docs/TUI_DESIGN.md` §4）。
//!
//! 纯函数：给定面积返回各区域矩形。**所有计算都用饱和运算**，
//! 保证任何尺寸（含极小/零面积）都不会出现负宽度、越界或 panic。

use ratatui::layout::Rect;

/// 布局档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 宽：`w>=120 && h>=28` —— 顶栏 2 行 + 三栏 + 底栏 2 行。
    Wide,
    /// 中：`80<=w<120` —— 顶部单行 Tab + 列表/详情两栏。
    Medium,
    /// 小：`w<80 || h<24` —— 单栏 + 左右键切子页。
    Small,
    /// 极小：`w<60 || h<18` —— 只显示"终端过小 + 当前状态 + 最小快捷键"。
    Tiny,
}

/// 判定档位（阈值与设计文档一致）。
pub fn tier(area: Rect) -> Tier {
    if area.width < 60 || area.height < 18 {
        Tier::Tiny
    } else if area.width < 80 || area.height < 24 {
        Tier::Small
    } else if area.width < 120 || area.height < 28 {
        Tier::Medium
    } else {
        Tier::Wide
    }
}

/// 三栏宽度的下界/上界（设计文档 §4：左 18–22 列、右 28–36 列、中 55–65%）。
/// 当终端极宽、三者无法同时满足时，**优先保证中栏 ≤65%**（把余量给右栏）。
fn columns(width: u16) -> (u16, u16, u16) {
    let w = u32::from(width);
    // 左导航：18–22 列（宽度不够时不得超出可用宽度）
    let left = (w / 7).clamp(18, 22).min(w);
    // 右详情：28–36 列（作为期望值；超宽终端上允许增长以保住中栏比例）
    let right_pref = (w / 5).clamp(28, 36);
    // 中栏上限 65%（下界 55% 由"右栏不小于 28"自然满足）
    let cap = (w * 65 / 100).max(1);

    let middle = w.saturating_sub(left + right_pref).min(cap);
    // 三栏必须正好占满宽度：右栏吸收余量（超宽终端上会大于 36 列，
    // 这是为了让中栏比例保持在 55–65%；在常用宽度上右栏仍在 28–36 列）
    let right = w.saturating_sub(left + middle);
    (left as u16, middle as u16, right as u16)
}

/// 各区域矩形（未使用的区域为零面积矩形，而不是负数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Regions {
    /// 顶栏（宽档 2 行；中/小档 1 行；极小档 0 行）。
    pub top: Rect,
    /// 左侧导航（仅宽档非空）。
    pub nav: Rect,
    /// 顶部 Tab（仅中档非空）。
    pub tabs: Rect,
    /// 主工作区（列表）。
    pub main: Rect,
    /// 详情区（宽/中档非空）。
    pub detail: Rect,
    /// 底栏（宽档 2 行；其余 1 行；极小档 1 行）。
    pub bottom: Rect,
}

/// 计算布局。
///
/// 不变量（由单测保证）：
/// - 所有矩形都在 `area` 内；
/// - 任何尺寸下都不 panic、不出现负宽度（`u16` 饱和运算）；
/// - 宽档左边栏 ∈ [18,22]、右详情 ∈ [28,36]、中栏 ∈ [55%,65%]（在可能时）。
pub fn compute(area: Rect, tier: Tier) -> Regions {
    let empty = Rect {
        x: area.x,
        y: area.y,
        width: 0,
        height: 0,
    };
    match tier {
        Tier::Wide => {
            let top_h = 2.min(area.height);
            let bottom_h = 2.min(area.height.saturating_sub(top_h));
            let body_y = area.y.saturating_add(top_h);
            let body_h = area.height.saturating_sub(top_h).saturating_sub(bottom_h);
            let (left, middle, right) = columns(area.width);
            let body = Rect {
                x: area.x,
                y: body_y,
                width: area.width,
                height: body_h,
            };
            let nav = Rect {
                width: left,
                ..body
            };
            let main = Rect {
                x: body.x.saturating_add(left),
                width: middle,
                ..body
            };
            let detail = Rect {
                x: body.x.saturating_add(left).saturating_add(middle),
                width: right,
                ..body
            };
            Regions {
                top: Rect {
                    height: top_h,
                    ..area
                },
                nav,
                tabs: empty,
                main,
                detail,
                bottom: Rect {
                    y: body_y.saturating_add(body_h),
                    height: bottom_h,
                    ..area
                },
            }
        }
        Tier::Medium => {
            let top_h = 1.min(area.height);
            let bottom_h = 1.min(area.height.saturating_sub(top_h));
            let body_y = area.y.saturating_add(top_h);
            let body_h = area.height.saturating_sub(top_h).saturating_sub(bottom_h);
            let body = Rect {
                x: area.x,
                y: body_y,
                width: area.width,
                height: body_h,
            };
            let list_w = (u32::from(area.width) * 55 / 100) as u16;
            Regions {
                top: empty,
                nav: empty,
                tabs: Rect {
                    height: top_h,
                    ..area
                },
                main: Rect {
                    width: list_w,
                    ..body
                },
                detail: Rect {
                    x: body.x.saturating_add(list_w),
                    width: area.width.saturating_sub(list_w),
                    ..body
                },
                bottom: Rect {
                    y: body_y.saturating_add(body_h),
                    height: bottom_h,
                    ..area
                },
            }
        }
        Tier::Small => {
            let top_h = 1.min(area.height);
            let bottom_h = 1.min(area.height.saturating_sub(top_h));
            let body_y = area.y.saturating_add(top_h);
            let body_h = area.height.saturating_sub(top_h).saturating_sub(bottom_h);
            Regions {
                top: Rect {
                    height: top_h,
                    ..area
                },
                nav: empty,
                tabs: empty,
                main: Rect {
                    x: area.x,
                    y: body_y,
                    width: area.width,
                    height: body_h,
                },
                detail: empty,
                bottom: Rect {
                    y: body_y.saturating_add(body_h),
                    height: bottom_h,
                    ..area
                },
            }
        }
        Tier::Tiny => Regions {
            top: empty,
            nav: empty,
            tabs: empty,
            main: area,
            detail: empty,
            bottom: empty,
        },
    }
}

/// 宽档三栏宽度（供测试断言与文档核对）。
pub fn wide_columns(width: u16) -> (u16, u16, u16) {
    columns(width)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// 设计要求覆盖的四种尺寸 + 一批边界尺寸。
    const SIZES: &[(u16, u16)] = &[
        (140, 40),
        (100, 30),
        (70, 22),
        (50, 15),
        (0, 0),
        (1, 1),
        (59, 17),
        (60, 18),
        (79, 23),
        (80, 24),
        (119, 27),
        (120, 28),
        (200, 60),
        (300, 100),
        (120, 5),
        (5, 120),
    ];

    #[test]
    fn tiers_match_design_thresholds() {
        assert_eq!(tier(area(140, 40)), Tier::Wide);
        assert_eq!(tier(area(120, 28)), Tier::Wide);
        assert_eq!(tier(area(100, 30)), Tier::Medium);
        assert_eq!(tier(area(80, 24)), Tier::Medium);
        assert_eq!(tier(area(119, 40)), Tier::Medium);
        assert_eq!(tier(area(70, 22)), Tier::Small);
        assert_eq!(tier(area(79, 40)), Tier::Small, "w<80 走小档");
        assert_eq!(tier(area(200, 23)), Tier::Small, "h<24 走小档");
        assert_eq!(tier(area(50, 15)), Tier::Tiny);
        assert_eq!(tier(area(59, 40)), Tier::Tiny);
        assert_eq!(tier(area(200, 17)), Tier::Tiny);
    }

    #[test]
    fn no_negative_or_overflowing_rects_at_any_size() {
        for &(w, h) in SIZES {
            let a = area(w, h);
            for t in [Tier::Wide, Tier::Medium, Tier::Small, Tier::Tiny] {
                let f = compute(a, t);
                for (name, r) in [
                    ("top", f.top),
                    ("nav", f.nav),
                    ("tabs", f.tabs),
                    ("main", f.main),
                    ("detail", f.detail),
                    ("bottom", f.bottom),
                ] {
                    assert!(
                        r.x.saturating_add(r.width) <= w,
                        "{w}x{h} {t:?} {name} 横向越界: {r:?}"
                    );
                    assert!(
                        r.y.saturating_add(r.height) <= h,
                        "{w}x{h} {t:?} {name} 纵向越界: {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn wide_columns_respect_design_ranges() {
        // 设计范围在常用宽度上必须成立；极宽时优先保证中栏 ≤65%
        for w in [120u16, 128, 140, 160, 192, 256, 320] {
            let (left, middle, right) = wide_columns(w);
            assert!((18..=22).contains(&left), "w={w} 左栏 {left} 应为 18–22");
            assert!(
                (28..=36).contains(&right) || w > 180,
                "w={w} 右栏 {right} 应为 28–36（超宽时允许增长来保证中栏比例）"
            );
            let pct = f64::from(middle) / f64::from(w) * 100.0;
            assert!(
                (54.9..=65.1).contains(&pct),
                "w={w} 中栏 {middle} = {pct:.1}% 应在 55–65%"
            );
            assert_eq!(left + middle + right, w, "三栏必须占满宽度");
        }
    }

    #[test]
    fn wide_layout_has_two_row_bars_and_three_columns() {
        let f = compute(area(140, 40), Tier::Wide);
        assert_eq!(f.top.height, 2, "顶栏 2 行");
        assert_eq!(f.bottom.height, 2, "底栏 2 行");
        assert!(f.nav.width >= 18 && f.nav.width <= 22);
        assert!(f.detail.width >= 28 && f.detail.width <= 36);
        assert_eq!(f.nav.height, f.main.height);
        assert_eq!(f.main.height, f.detail.height);
        assert_eq!(f.nav.y, f.main.y);
        assert_eq!(f.main.x, f.nav.x + f.nav.width);
        assert_eq!(f.detail.x, f.main.x + f.main.width);
        // 三段 + 两个栏条 = 整高
        assert_eq!(f.top.height + f.main.height + f.bottom.height, 40);
    }

    #[test]
    fn medium_layout_is_two_columns_with_single_row_tabs() {
        let f = compute(area(100, 30), Tier::Medium);
        assert_eq!(f.tabs.height, 1);
        assert_eq!(f.bottom.height, 1);
        assert_eq!(f.main.width + f.detail.width, 100);
        assert_eq!(f.tabs.height + f.main.height + f.bottom.height, 30);
        assert_eq!(f.nav.width, 0, "中档不显示左导航");
    }

    #[test]
    fn small_layout_is_single_column() {
        let f = compute(area(70, 22), Tier::Small);
        assert_eq!(f.main.width, 70, "小档单栏占满宽度");
        assert_eq!(f.detail.width, 0);
        assert_eq!(f.nav.width, 0);
        assert_eq!(f.top.height, 1);
        assert_eq!(f.bottom.height, 1);
    }

    #[test]
    fn tiny_layout_uses_whole_area() {
        let f = compute(area(50, 15), Tier::Tiny);
        assert_eq!(f.main.width, 50);
        assert_eq!(f.main.height, 15);
        assert_eq!(f.bottom.width, 0);
    }

    #[test]
    fn degenerate_heights_do_not_underflow() {
        // 高度比顶栏+底栏还小：不得 panic / 不得把 body 算成负高度
        for h in 0u16..6 {
            for t in [Tier::Wide, Tier::Medium, Tier::Small, Tier::Tiny] {
                let f = compute(area(140, h), t);
                assert!(f.top.height <= h);
                assert!(f.bottom.height <= h);
                assert!(f.main.y.saturating_add(f.main.height) <= h);
            }
        }
        for w in 0u16..20 {
            for t in [Tier::Wide, Tier::Medium, Tier::Small, Tier::Tiny] {
                let f = compute(area(w, 40), t);
                assert!(f.nav.width <= w);
                assert!(f.main.width <= w);
                assert!(f.detail.width <= w);
            }
        }
    }
}
