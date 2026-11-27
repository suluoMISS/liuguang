//! 输出/模式选择的纯逻辑（无 DRM 依赖，可完整单测）。
//!
//! 真实枚举数据在 [`crate::drm_kms`] 中转换为这里的描述类型后调用，
//! 保证选择策略（connector、模式、CRTC/plane 候选）在无硬件环境可验证。

use crate::DisplayError;
use liuguang_core::config::ModeSelection;

/// 模式描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeDesc {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 刷新率（毫赫兹）。
    pub refresh_millihz: u32,
    /// EDID 是否标记为首选模式。
    pub preferred: bool,
}

impl ModeDesc {
    /// 便捷构造。
    pub fn new(width: u32, height: u32, refresh_hz: u32, preferred: bool) -> Self {
        Self {
            width,
            height,
            refresh_millihz: refresh_hz.saturating_mul(1000),
            preferred,
        }
    }
}

/// connector 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorDesc {
    /// KMS connector 名称（如 `HDMI-A-1`）。
    pub name: String,
    /// 是否已连接显示器。
    pub connected: bool,
    /// 支持的模式。
    pub modes: Vec<ModeDesc>,
}

/// 选择目标 connector。
///
/// - `wanted = Some(name)`：精确匹配该名称（不存在则报错）。
/// - `wanted = None`：选择第一个已连接的 connector。
///
/// # Errors
///
/// 名称不存在或没有任何已连接输出时返回 [`DisplayError`]。
pub fn select_connector(
    connectors: &[ConnectorDesc],
    wanted: Option<&str>,
) -> Result<usize, DisplayError> {
    match wanted {
        Some(name) => connectors
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| {
                DisplayError::DeviceUnavailable(format!(
                    "找不到输出 {name}；可用输出: {}",
                    list_names(connectors)
                ))
            }),
        None => connectors
            .iter()
            .position(|c| c.connected && !c.modes.is_empty())
            .ok_or_else(|| {
                DisplayError::DeviceUnavailable(format!(
                    "没有任何已连接的显示器输出（枚举到: {}）",
                    list_names(connectors)
                ))
            }),
    }
}

fn list_names(connectors: &[ConnectorDesc]) -> String {
    if connectors.is_empty() {
        return "<无>".to_string();
    }
    connectors
        .iter()
        .map(|c| {
            format!(
                "{}({})",
                c.name,
                if c.connected {
                    "connected"
                } else {
                    "disconnected"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 选择显示模式。
///
/// - `preferred`：EDID 首选模式，缺失时回退第一个模式。
/// - `宽x高[@Hz]`：精确匹配分辨率；给出刷新率时选最接近者。
///
/// # Errors
///
/// 模式列表为空或没有匹配项时返回 [`DisplayError`]。
pub fn pick_mode(modes: &[ModeDesc], selection: &ModeSelection) -> Result<usize, DisplayError> {
    if modes.is_empty() {
        return Err(DisplayError::DeviceUnavailable(
            "该输出没有可用显示模式".to_string(),
        ));
    }
    match selection {
        ModeSelection::Preferred => Ok(modes.iter().position(|m| m.preferred).unwrap_or(0)),
        ModeSelection::Manual {
            width,
            height,
            refresh_millihz,
        } => {
            let candidates: Vec<usize> = modes
                .iter()
                .enumerate()
                .filter(|(_, m)| m.width == *width && m.height == *height)
                .map(|(i, _)| i)
                .collect();
            match candidates.as_slice() {
                [] => Err(DisplayError::ModeNotSupported {
                    output: liuguang_core::types::OutputId::new("<output>"),
                    mode: liuguang_core::types::DisplayMode {
                        width: *width,
                        height: *height,
                        refresh_millihz: refresh_millihz.unwrap_or(0),
                    },
                }),
                [only] => Ok(*only),
                many => Ok(match refresh_millihz {
                    Some(target) => *many
                        .iter()
                        .min_by_key(|&&i| modes[i].refresh_millihz.abs_diff(*target))
                        .unwrap_or(&many[0]),
                    None => many[0],
                }),
            }
        }
    }
}

/// 从候选 CRTC 列表中选择第一个空闲且兼容的 CRTC。
///
/// `possible` 是 encoder/plane 的 possible_crtcs 过滤结果（已按索引对齐）。
/// 优先选择未被占用（`current_crtc == None`）的候选。
///
/// # Errors
///
/// 候选为空时返回 [`DisplayError`]。
pub fn pick_crtc(candidates: &[CrtcDesc]) -> Result<usize, DisplayError> {
    if candidates.is_empty() {
        return Err(DisplayError::DeviceUnavailable(
            "没有与该 connector 兼容的 CRTC（possible_crtcs 为空）".to_string(),
        ));
    }
    // 优先级：当前绑定（真机 i915 上空闲 crtc 可能无法驱动该 connector）
    // > 空闲 > 其他；全部不符时回退第一个。
    Ok(candidates
        .iter()
        .position(|c| c.current)
        .or_else(|| candidates.iter().position(|c| !c.busy))
        .unwrap_or(0))
}

/// CRTC 候选描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrtcDesc {
    /// 是否已被其他输出占用。
    pub busy: bool,
    /// 是否为该 connector 当前编码器绑定的 crtc（最优先）。
    pub current: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conns() -> Vec<ConnectorDesc> {
        vec![
            ConnectorDesc {
                name: "DP-1".into(),
                connected: false,
                modes: vec![ModeDesc::new(1920, 1080, 60, true)],
            },
            ConnectorDesc {
                name: "HDMI-A-1".into(),
                connected: true,
                modes: vec![
                    ModeDesc::new(1920, 1080, 60, false),
                    ModeDesc::new(1280, 720, 60, true),
                ],
            },
            ConnectorDesc {
                name: "HDMI-A-2".into(),
                connected: true,
                modes: vec![],
            },
        ]
    }

    #[test]
    fn select_by_name() {
        assert_eq!(
            select_connector(&conns(), Some("DP-1")).unwrap(),
            0,
            "按名称可选中断开的输出"
        );
        assert!(matches!(
            select_connector(&conns(), Some("VGA-1")),
            Err(DisplayError::DeviceUnavailable(_))
        ));
    }

    #[test]
    fn select_first_connected_with_modes() {
        // HDMI-A-1：已连接且有模式；HDMI-A-2 已连接但无模式。
        assert_eq!(select_connector(&conns(), None).unwrap(), 1);
    }

    #[test]
    fn select_none_connected_fails() {
        let mut c = conns();
        for x in &mut c {
            x.connected = false;
        }
        assert!(matches!(
            select_connector(&c, None),
            Err(DisplayError::DeviceUnavailable(_))
        ));
    }

    #[test]
    fn pick_preferred_mode() {
        let c = conns();
        let modes = &c[1].modes;
        assert_eq!(
            pick_mode(modes, &ModeSelection::Preferred).unwrap(),
            1,
            "EDID 首选是 1280x720"
        );
    }

    #[test]
    fn pick_preferred_falls_back_to_first() {
        let modes = vec![ModeDesc::new(800, 600, 60, false)];
        assert_eq!(pick_mode(&modes, &ModeSelection::Preferred).unwrap(), 0);
    }

    #[test]
    fn pick_manual_mode_and_nearest_refresh() {
        let modes = vec![
            ModeDesc::new(1920, 1080, 60, false),
            ModeDesc::new(1920, 1080, 144, false),
            ModeDesc::new(1280, 720, 60, true),
        ];
        assert_eq!(
            pick_mode(
                &modes,
                &ModeSelection::Manual {
                    width: 1280,
                    height: 720,
                    refresh_millihz: None
                }
            )
            .unwrap(),
            2
        );
        assert_eq!(
            pick_mode(
                &modes,
                &ModeSelection::Manual {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: Some(120_000)
                }
            )
            .unwrap(),
            1,
            "120Hz 选最接近的 144"
        );
        assert!(matches!(
            pick_mode(
                &modes,
                &ModeSelection::Manual {
                    width: 100,
                    height: 100,
                    refresh_millihz: None
                }
            ),
            Err(DisplayError::ModeNotSupported { .. })
        ));
    }

    #[test]
    fn pick_mode_empty_list_fails() {
        assert!(matches!(
            pick_mode(&[], &ModeSelection::Preferred),
            Err(DisplayError::DeviceUnavailable(_))
        ));
    }

    #[test]
    fn pick_crtc_prefers_current_then_idle() {
        // 当前绑定优先于空闲（i915 真机教训）。
        let candidates = [
            CrtcDesc {
                busy: false,
                current: false,
            },
            CrtcDesc {
                busy: true,
                current: true,
            },
        ];
        assert_eq!(pick_crtc(&candidates).unwrap(), 1);
        // 无当前绑定时空闲优先。
        let candidates = [
            CrtcDesc {
                busy: true,
                current: false,
            },
            CrtcDesc {
                busy: false,
                current: false,
            },
        ];
        assert_eq!(pick_crtc(&candidates).unwrap(), 1);
        // 全忙时回退第一个。
        let all_busy = [CrtcDesc {
            busy: true,
            current: false,
        }];
        assert_eq!(pick_crtc(&all_busy).unwrap(), 0);
        assert!(matches!(
            pick_crtc(&[]),
            Err(DisplayError::DeviceUnavailable(_))
        ));
    }
}
