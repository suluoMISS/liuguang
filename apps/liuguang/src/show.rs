//! `liuguang show` — M1 最小垂直切片：
//! 加载 PNG → fit 变换到输出尺寸 → DRM dumb buffer + Atomic KMS 显示 →
//! 等待退出信号（**无渲染循环**）→ 释放设备。
//!
//! 服务器共存原则：显示失败/设备被占用时给出明确错误并退出，不重试风暴；
//! 静止期间 CPU 不产生任何周期性提交。

use liuguang_core::config::{Config, FitMode, ModeSelection};
use liuguang_core::fit::FitParams;
use liuguang_core::types::DisplayFrame;
use liuguang_display::DisplayBackend;
use liuguang_display::drm_kms::DrmKmsBackend;
use liuguang_media::png_load::load_png;
use liuguang_render::cpu::render_fit;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `show` 的参数（CLI 解析后传入）。
pub struct ShowOptions {
    /// PNG 路径。
    pub image: PathBuf,
    /// 目标输出名（`None` = 自动选择已连接输出）。
    pub output: Option<String>,
    /// DRM 设备（`auto` 或 `/dev/dri/cardN`）。
    pub device: String,
    /// 模式选择字符串（`preferred` 或 `宽x高[@Hz]`）。
    pub mode: String,
    /// 适配模式（`None` = 回退配置/默认 cover）。
    pub fit: Option<FitMode>,
    /// 用户缩放。
    pub zoom: f64,
    /// X 偏移（输出像素）。
    pub offset_x: f64,
    /// Y 偏移（输出像素）。
    pub offset_y: f64,
}

/// 状态复查间隔（秒）——热插拔检测；不是渲染循环，不产生提交。
const STATUS_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// 连接器复查周期数（每 N 次唤醒复查一次）。
const CONNECTOR_CHECK_EVERY: u64 = 5;

/// 执行静态显示直到收到 SIGINT/SIGTERM。
///
/// # Errors
///
/// 设备、解码、渲染或提交失败时返回错误消息（不 panic）。
pub fn run(options: ShowOptions, config: Option<&Config>) -> Result<(), String> {
    // 配置合并：CLI 显式参数优先，缺省回退配置文件的 display/outputs 段。
    let (device, mode_str, fit_params) = merge_settings(&options, config);

    tracing::info!(
        image = %options.image.display(),
        device = %device,
        mode = %mode_str,
        "开始静态显示（M1）"
    );

    // 1. 先解码 PNG（受限缓冲；坏文件不触碰显示设备）。
    let rgba = load_png(&options.image)
        .map_err(|e| format!("PNG 解码失败（{}）: {e}", options.image.display()))?;
    tracing::info!(width = rgba.width, height = rgba.height, "PNG 已解码");
    // 2. 打开显示设备（动态发现，不写死 cardN）。
    let mut backend = DrmKmsBackend::open(&device).map_err(|e| format!("打开显示设备失败: {e}"))?;
    tracing::info!(
        path = %backend.path().display(),
        driver = backend.driver(),
        "DRM 后端就绪"
    );

    // 3. 选择输出与模式。
    let output_id = backend
        .default_output(options.output.as_deref())
        .map_err(|e| format!("选择输出失败: {e}"))?;
    let mode_selection =
        ModeSelection::parse(&mode_str).map_err(|e| format!("模式参数非法: {e}"))?;
    let mode = backend
        .resolve_mode(&output_id, &mode_selection)
        .map_err(|e| format!("选择显示模式失败: {e}"))?;
    backend
        .set_mode(output_id.clone(), mode)
        .map_err(|e| format!("设置显示模式失败: {e}"))?;
    tracing::info!(
        output = %output_id,
        width = mode.width,
        height = mode.height,
        refresh_hz = mode.refresh_hz(),
        "输出与模式已选定"
    );

    let scanout = render_fit(&rgba, (mode.width, mode.height), &fit_params, [0, 0, 0])
        .map_err(|e| format!("渲染适配失败: {e}"))?;

    // 5. 提交扫描输出。
    let token = backend
        .present(output_id.clone(), DisplayFrame::Cpu(scanout.clone()))
        .map_err(|e| format!("提交扫描输出失败: {e}"))?;
    tracing::info!(
        sequence = token.sequence,
        "画面已上屏；进入等待（无渲染循环）"
    );

    // 6. 等待退出信号；周期性复查连接器（不提交任何帧）。
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|e| format!("注册信号处理器失败: {e}"))?;
    }

    let mut ticks: u64 = 0;
    let mut connected = true;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(STATUS_CHECK_INTERVAL);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        ticks += 1;
        if ticks % CONNECTOR_CHECK_EVERY != 0 {
            continue;
        }
        match backend.connector_connected(output_id.as_str()) {
            Ok(true) => {
                if !connected {
                    tracing::info!(output = %output_id, "显示器重新接入，恢复画面");
                    let token = backend
                        .present(output_id.clone(), DisplayFrame::Cpu(scanout.clone()))
                        .map_err(|e| format!("重新提交失败: {e}"))?;
                    tracing::info!(sequence = token.sequence, "画面已恢复");
                }
                connected = true;
            }
            Ok(false) => {
                if connected {
                    tracing::warn!(
                        output = %output_id,
                        "显示器断开：保持静默等待（不做周期性提交）"
                    );
                }
                connected = false;
            }
            Err(e) => {
                tracing::warn!(output = %output_id, error = %e, "连接器状态复查失败");
            }
        }
    }

    // 7. 正常退出：释放 dumb buffer / framebuffer / mode blob。
    drop(backend);
    tracing::info!("已收到退出信号，设备资源已释放，退出");
    Ok(())
}

/// CLI 参数与配置文件合并（CLI 优先）。
fn merge_settings(options: &ShowOptions, config: Option<&Config>) -> (String, String, FitParams) {
    let default_device = "auto".to_string();
    let default_mode = "preferred".to_string();
    let (device, mode) = match config {
        Some(cfg) => (
            if options.device == default_device {
                cfg.display.device.clone()
            } else {
                options.device.clone()
            },
            if options.mode == default_mode {
                cfg.display.mode.clone()
            } else {
                options.mode.clone()
            },
        ),
        None => (options.device.clone(), options.mode.clone()),
    };

    // 输出段配置作为 fit/zoom/offset 的默认值，CLI 显式值优先。
    let out_cfg = config.and_then(|c| options.output.as_ref().and_then(|n| c.outputs.get(n)));
    let mut params = FitParams {
        mode: options.fit.unwrap_or_default(),
        zoom: options.zoom,
        offset_x: options.offset_x,
        offset_y: options.offset_y,
    };
    if let Some(out_cfg) = out_cfg {
        if options.fit.is_none() {
            params.mode = out_cfg.fit;
        }
        if options.zoom == 1.0 {
            params.zoom = out_cfg.zoom;
        }
        if options.offset_x == 0.0 {
            params.offset_x = out_cfg.offset_x;
        }
        if options.offset_y == 0.0 {
            params.offset_y = out_cfg.offset_y;
        }
    }
    (device, mode, params)
}

/// 校验 `show` 的输入路径存在且是普通文件。
///
/// # Errors
///
/// 返回错误消息。
pub fn check_image_path(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("无法读取 {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} 不是普通文件", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn opts() -> ShowOptions {
        ShowOptions {
            image: PathBuf::from("/srv/wallpapers/x.png"),
            output: Some("HDMI-A-1".into()),
            device: "auto".into(),
            mode: "preferred".into(),
            fit: None,
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }

    #[test]
    fn merge_prefers_explicit_cli_over_config() {
        let mut cfg = Config::default();
        cfg.display.device = "/dev/dri/card7".into();
        cfg.display.mode = "1280x720".into();
        let mut o = opts();
        o.device = "/dev/dri/card3".into();
        o.mode = "1920x1080".into();
        o.fit = Some(FitMode::Stretch);
        let (device, mode, params) = merge_settings(&o, Some(&cfg));
        assert_eq!(device, "/dev/dri/card3");
        assert_eq!(mode, "1920x1080");
        assert_eq!(params.mode, FitMode::Stretch);
    }

    #[test]
    fn merge_takes_fit_from_output_config() {
        let mut cfg = Config::default();
        cfg.outputs.insert(
            "HDMI-A-1".into(),
            liuguang_core::config::OutputConfig {
                fit: FitMode::Contain,
                zoom: 2.0,
                offset_x: 10.0,
                offset_y: -5.0,
                fallback_image: None,
                ..liuguang_core::config::OutputConfig::default()
            },
        );
        let o = opts(); // fit=None，zoom/offset 为哨兵默认
        let (_, _, params) = merge_settings(&o, Some(&cfg));
        assert_eq!(params.mode, FitMode::Contain);
        assert_eq!(params.zoom, 2.0);
        assert_eq!(params.offset_x, 10.0);
        assert_eq!(params.offset_y, -5.0);
    }

    #[test]
    fn merge_uses_config_when_cli_is_default() {
        let mut cfg = Config::default();
        cfg.display.device = "/dev/dri/card7".into();
        cfg.display.mode = "1280x720@60".into();
        let o = opts();
        let (device, mode, params) = merge_settings(&o, Some(&cfg));
        assert_eq!(device, "/dev/dri/card7");
        assert_eq!(mode, "1280x720@60");
        assert_eq!(params.mode, FitMode::Cover);
    }

    #[test]
    fn merge_without_config_uses_cli() {
        let o = opts();
        let (device, mode, params) = merge_settings(&o, None);
        assert_eq!(device, "auto");
        assert_eq!(mode, "preferred");
        assert_eq!(params.zoom, 1.0);
    }

    #[test]
    fn image_path_check() {
        assert!(check_image_path(Path::new("/nonexistent.png")).is_err());
        assert!(check_image_path(Path::new("/dev")).is_err(), "目录应拒绝");
    }
}
