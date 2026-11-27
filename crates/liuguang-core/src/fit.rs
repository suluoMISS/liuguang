//! 显示适配数学：把任意媒体尺寸映射到真实输出像素尺寸。
//!
//! 覆盖 `cover / contain / stretch / center` 及手动 `zoom`、`offset_x`、`offset_y`；
//! 坐标基于真实输出像素，不假设 16:9。纯函数实现，可在无硬件环境完整测试。
//!
//! 公式（规范 §5.2）：
//! ```text
//! cover_scale   = max(ow / iw, oh / ih)
//! contain_scale = min(ow / iw, oh / ih)
//! render_scale  = base_scale * user_zoom
//! ```
//! `offset_x` / `offset_y` 单位为输出像素；`center` 模式不强制缩放（缩放恒为
//! `user_zoom`），溢出部分由渲染层裁切。输出旋转不属于本模块（由 KMS plane
//! 或 GPU 单侧处理，不得重复旋转）。

/// 显示适配模式。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    /// 保持比例填满屏幕，超出裁切（默认）。
    #[default]
    Cover,
    /// 保持比例完整显示，允许空白背景。
    Contain,
    /// 忽略比例填满。
    Stretch,
    /// 原始像素居中，不强制缩放，溢出裁切。
    Center,
}

impl FitMode {
    /// 稳定的小写名字（CLI / IPC / 持久化共用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Stretch => "stretch",
            Self::Center => "center",
        }
    }
}

/// 适配参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitParams {
    /// 适配模式。
    pub mode: FitMode,
    /// 用户缩放倍数（必须有限且 > 0）。
    pub zoom: f64,
    /// 水平偏移（输出像素）。
    pub offset_x: f64,
    /// 垂直偏移（输出像素）。
    pub offset_y: f64,
}

impl Default for FitParams {
    fn default() -> Self {
        Self {
            mode: FitMode::Cover,
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

/// 计算结果：目标矩形在输出像素坐标系中的位置与大小。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitResult {
    /// X 方向缩放系数（每媒体像素对应输出像素数）。
    pub scale_x: f64,
    /// Y 方向缩放系数。
    pub scale_y: f64,
    /// 目标矩形左上角 X（输出像素）。
    pub dst_x: f64,
    /// 目标矩形左上角 Y（输出像素）。
    pub dst_y: f64,
    /// 目标矩形宽（输出像素，可大于输出宽（cover/center 溢出））。
    pub dst_width: f64,
    /// 目标矩形高（输出像素）。
    pub dst_height: f64,
}

impl FitResult {
    /// 目标矩形是否完全覆盖输出（允许溢出）。
    pub fn covers_output(&self, output: (u32, u32)) -> bool {
        self.dst_x <= 1e-9
            && self.dst_y <= 1e-9
            && self.dst_x + self.dst_width >= f64::from(output.0) - 1e-9
            && self.dst_y + self.dst_height >= f64::from(output.1) - 1e-9
    }

    /// 目标矩形是否完整落在输出内（不允许溢出）。
    pub fn fits_inside_output(&self, output: (u32, u32)) -> bool {
        self.dst_x >= -1e-9
            && self.dst_y >= -1e-9
            && self.dst_x + self.dst_width <= f64::from(output.0) + 1e-9
            && self.dst_y + self.dst_height <= f64::from(output.1) + 1e-9
    }
}

/// 适配计算错误。
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FitError {
    /// 媒体或输出尺寸为 0。
    #[error("{what} 尺寸必须大于 0，实际 {width}x{height}")]
    InvalidSize {
        /// 出问题的对象（`媒体` / `输出`）。
        what: &'static str,
        /// 实际宽。
        width: u32,
        /// 实际高。
        height: u32,
    },
    /// zoom 非法（非有限、<= 0 或超出上限）。
    #[error("zoom 非法: {zoom}（要求有限、> 0 且 <= 10）")]
    InvalidZoom {
        /// 实际 zoom。
        zoom: f64,
    },
    /// 偏移非有限。
    #[error("偏移必须为有限数值: offset_x={offset_x}, offset_y={offset_y}")]
    NonFiniteOffset {
        /// 实际 x 偏移。
        offset_x: f64,
        /// 实际 y 偏移。
        offset_y: f64,
    },
}

/// zoom 合法上限（与配置校验保持一致）。
pub const MAX_ZOOM: f64 = 10.0;

/// 计算把 `(iw, ih)` 媒体适配到 `(ow, oh)` 输出时的目标矩形。
///
/// # Errors
///
/// 尺寸为 0、zoom 非法或偏移非有限时返回 [`FitError`]。
pub fn compute_fit(
    source: (u32, u32),
    output: (u32, u32),
    params: &FitParams,
) -> Result<FitResult, FitError> {
    let (iw, ih) = source;
    let (ow, oh) = output;
    if iw == 0 || ih == 0 {
        return Err(FitError::InvalidSize {
            what: "媒体",
            width: iw,
            height: ih,
        });
    }
    if ow == 0 || oh == 0 {
        return Err(FitError::InvalidSize {
            what: "输出",
            width: ow,
            height: oh,
        });
    }
    if !(params.zoom.is_finite() && params.zoom > 0.0 && params.zoom <= MAX_ZOOM) {
        return Err(FitError::InvalidZoom { zoom: params.zoom });
    }
    if !(params.offset_x.is_finite() && params.offset_y.is_finite()) {
        return Err(FitError::NonFiniteOffset {
            offset_x: params.offset_x,
            offset_y: params.offset_y,
        });
    }

    let iw_f = f64::from(iw);
    let ih_f = f64::from(ih);
    let ow_f = f64::from(ow);
    let oh_f = f64::from(oh);

    let (scale_x, scale_y) = match params.mode {
        FitMode::Cover => {
            let s = (ow_f / iw_f).max(oh_f / ih_f) * params.zoom;
            (s, s)
        }
        FitMode::Contain => {
            let s = (ow_f / iw_f).min(oh_f / ih_f) * params.zoom;
            (s, s)
        }
        FitMode::Stretch => (ow_f / iw_f * params.zoom, oh_f / ih_f * params.zoom),
        FitMode::Center => (params.zoom, params.zoom),
    };

    let dst_width = iw_f * scale_x;
    let dst_height = ih_f * scale_y;
    // 默认居中，再叠加用户偏移。
    let dst_x = (ow_f - dst_width) / 2.0 + params.offset_x;
    let dst_y = (oh_f - dst_height) / 2.0 + params.offset_y;

    Ok(FitResult {
        scale_x,
        scale_y,
        dst_x,
        dst_y,
        dst_width,
        dst_height,
    })
}

/// 硬件平面缩放的可见矩形（M5 性能路径）。
///
/// 与 [`compute_fit`] 同一数学：`dst` 为裁剪到输出内部的可见区域
/// （像素浮点），`src` 为对应源区域（像素浮点）。显示驱动把 `src`
/// 拉伸到 `dst`，未覆盖区域为背景色——CPU 逐像素缩放不再需要。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitRects {
    /// 可见源区 x（源像素，浮点）。
    pub src_x: f64,
    /// 可见源区 y。
    pub src_y: f64,
    /// 可见源区宽。
    pub src_w: f64,
    /// 可见源区高。
    pub src_h: f64,
    /// 输出区 x（模式像素，浮点）。
    pub dst_x: f64,
    /// 输出区 y。
    pub dst_y: f64,
    /// 输出区宽。
    pub dst_w: f64,
    /// 输出区高。
    pub dst_h: f64,
}

/// 计算硬件缩放可见矩形。
///
/// # Errors
///
/// 参数非法时返回 [`FitError`]（同 [`compute_fit`]）。
pub fn fit_visible_rects(
    source: (u32, u32),
    output: (u32, u32),
    params: &FitParams,
) -> Result<FitRects, FitError> {
    let fit = compute_fit(source, output, params)?;
    let (iw, ih) = (f64::from(source.0), f64::from(source.1));
    let (ow, oh) = (f64::from(output.0), f64::from(output.1));

    // 目标矩形与输出求交（可见部分）。
    let dx0 = fit.dst_x.max(0.0);
    let dy0 = fit.dst_y.max(0.0);
    let dx1 = (fit.dst_x + fit.dst_width).min(ow);
    let dy1 = (fit.dst_y + fit.dst_height).min(oh);
    let dst_w = dx1 - dx0;
    let dst_h = dy1 - dy0;

    // 逆映射回源坐标，并裁剪到源边界。
    let mut sx0 = (dx0 - fit.dst_x) / fit.scale_x;
    let mut sy0 = (dy0 - fit.dst_y) / fit.scale_y;
    let mut sx1 = (dx1 - fit.dst_x) / fit.scale_x;
    let mut sy1 = (dy1 - fit.dst_y) / fit.scale_y;
    sx0 = sx0.clamp(0.0, iw);
    sy0 = sy0.clamp(0.0, ih);
    sx1 = sx1.clamp(0.0, iw);
    sy1 = sy1.clamp(0.0, ih);

    Ok(FitRects {
        src_x: sx0,
        src_y: sy0,
        src_w: (sx1 - sx0).max(0.0),
        src_h: (sy1 - sy0).max(0.0),
        dst_x: dx0,
        dst_y: dy0,
        dst_w: dst_w.max(0.0),
        dst_h: dst_h.max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(mode: FitMode) -> FitParams {
        FitParams {
            mode,
            ..FitParams::default()
        }
    }

    const RATIOS: [((u32, u32), (u32, u32)); 10] = [
        ((1920, 1080), (1920, 1080)), // 16:9 → 16:9
        ((1080, 1920), (1920, 1080)), // 竖屏 → 横屏
        ((1920, 1200), (2560, 1440)), // 16:10 → 16:9
        ((2560, 1080), (3440, 1440)), // 21:9 → 21:9
        ((5120, 1440), (3840, 2160)), // 32:9 → 4K
        ((3840, 2160), (1920, 1080)), // 4K → 1080p
        ((1234, 567), (1920, 1080)),  // 非整数比率
        ((777, 2001), (800, 600)),    // 非整数比率
        ((100, 100), (2160, 3840)),   // 方形 → 竖 4K
        ((1, 1), (1280, 800)),        // 极端小图
    ];

    #[test]
    fn cover_covers_every_output() {
        for (source, output) in RATIOS {
            let r = compute_fit(source, output, &params(FitMode::Cover)).unwrap();
            assert!(
                r.covers_output(output),
                "cover {source:?}->{output:?} 未覆盖输出: {r:?}"
            );
        }
    }

    #[test]
    fn contain_fits_inside_every_output() {
        for (source, output) in RATIOS {
            let r = compute_fit(source, output, &params(FitMode::Contain)).unwrap();
            assert!(
                r.fits_inside_output(output),
                "contain {source:?}->{output:?} 溢出输出: {r:?}"
            );
        }
    }

    #[test]
    fn cover_and_contain_preserve_aspect() {
        for (source, output) in RATIOS {
            for mode in [FitMode::Cover, FitMode::Contain] {
                let r = compute_fit(source, output, &params(mode)).unwrap();
                let expect = f64::from(source.0) / f64::from(source.1);
                let got = r.dst_width / r.dst_height;
                assert!(
                    (expect - got).abs() < 1e-9,
                    "{mode:?} {source:?}->{output:?} 宽高比被破坏: {r:?}"
                );
                assert!((r.scale_x - r.scale_y).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn stretch_fills_output_exactly() {
        for (source, output) in RATIOS {
            let r = compute_fit(source, output, &params(FitMode::Stretch)).unwrap();
            assert!((r.dst_width - f64::from(output.0)).abs() < 1e-9);
            assert!((r.dst_height - f64::from(output.1)).abs() < 1e-9);
            assert!(r.covers_output(output));
        }
    }

    #[test]
    fn center_keeps_original_pixels_and_centers() {
        let r = compute_fit((300, 200), (1000, 800), &params(FitMode::Center)).unwrap();
        assert_eq!(r.scale_x, 1.0);
        assert_eq!(r.scale_y, 1.0);
        assert_eq!(r.dst_width, 300.0);
        assert_eq!(r.dst_height, 200.0);
        assert!((r.dst_x - 350.0).abs() < 1e-9);
        assert!((r.dst_y - 300.0).abs() < 1e-9);

        // 溢出裁切：小输出时矩形可以超出输出。
        let r = compute_fit((300, 200), (100, 100), &params(FitMode::Center)).unwrap();
        assert!(r.dst_x < 0.0 && r.dst_y < 0.0);
    }

    #[test]
    fn zoom_scales_render_size() {
        let base = compute_fit((1920, 1080), (1920, 1080), &params(FitMode::Contain)).unwrap();
        let zoomed = compute_fit(
            (1920, 1080),
            (1920, 1080),
            &FitParams {
                zoom: 2.0,
                ..FitParams::default()
            },
        )
        .unwrap();
        assert!((zoomed.dst_width - base.dst_width * 2.0).abs() < 1e-9);
        assert!((zoomed.dst_height - base.dst_height * 2.0).abs() < 1e-9);
    }

    #[test]
    fn offsets_shift_in_output_pixels() {
        let r = compute_fit(
            (1920, 1080),
            (1920, 1080),
            &FitParams {
                offset_x: 30.0,
                offset_y: -20.0,
                ..FitParams::default()
            },
        )
        .unwrap();
        assert!((r.dst_x - 30.0).abs() < 1e-9);
        assert!((r.dst_y + 20.0).abs() < 1e-9);
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        assert!(matches!(
            compute_fit((0, 100), (100, 100), &FitParams::default()),
            Err(FitError::InvalidSize { what: "媒体", .. })
        ));
        assert!(matches!(
            compute_fit((100, 100), (100, 0), &FitParams::default()),
            Err(FitError::InvalidSize { what: "输出", .. })
        ));
        for zoom in [0.0, -1.0, f64::NAN, f64::INFINITY, 10.5] {
            assert!(matches!(
                compute_fit(
                    (100, 100),
                    (100, 100),
                    &FitParams {
                        zoom,
                        ..FitParams::default()
                    }
                ),
                Err(FitError::InvalidZoom { .. })
            ));
        }
        assert!(matches!(
            compute_fit(
                (100, 100),
                (100, 100),
                &FitParams {
                    offset_x: f64::NAN,
                    ..FitParams::default()
                }
            ),
            Err(FitError::NonFiniteOffset { .. })
        ));
    }

    #[test]
    fn cover_21_9_into_16_9_crops_horizontally() {
        let r = compute_fit((2560, 1080), (1920, 1080), &params(FitMode::Cover)).unwrap();
        // 高度恰好填满，宽度溢出。
        assert!((r.dst_height - 1080.0).abs() < 1e-9);
        assert!(r.dst_width > 1920.0);
        assert!(r.dst_x < 0.0);
    }

    #[test]
    fn contain_portrait_into_landscape_pillarboxes_horizontally() {
        let r = compute_fit((1080, 1920), (1920, 1080), &params(FitMode::Contain)).unwrap();
        // 高度恰好填满，左右留白。
        assert!((r.dst_height - 1080.0).abs() < 1e-9);
        assert!(r.dst_width < 1920.0);
        assert!(r.dst_x > 0.0);
    }
}
