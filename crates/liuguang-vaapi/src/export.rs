//! `vaExportSurfaceHandle` 的封装：把解码表面变成若干 dma-buf fd + 分层描述。
//!
//! 关键事实（真机实测，见 `docs/VERIFICATION_2026-11-21_DMABUF.md`）：
//! - NVIDIA 的 NVDEC 解码表面导出为 **NV12 + 两个 object**（Y 一个、UV 一个），
//!   两个 object 的 modifier 都是 block-linear（实测 `0x0300000000606014`）；
//! - Intel iHD 常见**单 object**（两个平面落在同一对象的不同偏移）；
//! - 所以消费侧必须按 `object_index[plane]` 取 fd，不能假设"一个 fd 全包"。
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use liuguang_core::types::{DmaBufFrame, FrameKeepalive, PixelFormat};

use crate::{VaapiError, ffi};

/// DRM fourcc：NV12（`DRM_FORMAT_NV12`）。
pub const DRM_FORMAT_NV12: u32 = 0x3231_564e;
/// DRM fourcc：XR24（`DRM_FORMAT_XRGB8888`）。
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// DRM fourcc：P010（10bit 4:2:0，`DRM_FORMAT_P010`）。
///
/// 10bit 素材解码后就是这个格式；整条转换链只支持 8bit NV12，所以导出到显示层之前
/// 会给出明确拒绝（而不是让上层拿到一个无法转换的表面）。
pub const DRM_FORMAT_P010: u32 = 0x3031_3050;

/// 一个平面在描述符里的位置（来自某个 layer 的第 0 个平面）。
#[derive(Debug, Clone, Copy)]
pub struct ExportedPlane {
    /// 落在哪个 object 上（`objects` 的下标）。
    pub object: usize,
    /// object 内字节偏移。
    pub offset: u32,
    /// 行距。
    pub pitch: u32,
}

/// 已导出的表面（持有 fd；`Drop` 时关闭）。
#[derive(Debug)]
pub struct ExportedSurface {
    /// 整体 fourcc。
    pub fourcc: u32,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
    /// 每个 object 的 fd 与该 object 的 modifier。
    pub objects: Vec<(OwnedFd, u64)>,
    /// 平面表（NV12 两个：Y、UV）。
    pub planes: Vec<ExportedPlane>,
}

impl ExportedSurface {
    /// 转成显示后端能吃的 [`DmaBufFrame`]。
    ///
    /// `keepalive` 用于延长解码表面的寿命：合成器 `release` 之前不能回收该表面，
    /// 否则会拿到正在被扫描输出的缓冲（撕裂/闪烁）。
    ///
    /// # Errors
    ///
    /// fourcc 不是本项目支持的 NV12/XRGB8888，或描述符自相矛盾（平面数/对象越界）时返回错误。
    pub fn into_frame(self, keepalive: Option<FrameKeepalive>) -> Result<DmaBufFrame, VaapiError> {
        let format = match self.fourcc {
            DRM_FORMAT_NV12 => PixelFormat::Nv12,
            DRM_FORMAT_XRGB8888 => PixelFormat::Xrgb8888,
            DRM_FORMAT_P010 => {
                return Err(VaapiError::Unsupported(
                    "解码表面是 P010（10bit 4:2:0），当前 RGB 转换链只支持 8bit NV12：\
                     请改用 8bit 版本，或先转成 8bit（例如 \
                     `ffmpeg -i in.mp4 -pix_fmt yuv420p out.mp4`）"
                        .into(),
                ));
            }
            other => {
                return Err(VaapiError::Unsupported(format!(
                    "导出表面的 fourcc=0x{other:08X} 不在支持列表（NV12/XRGB8888）"
                )));
            }
        };
        let ExportedSurface {
            width,
            height,
            objects,
            planes,
            ..
        } = self;
        if objects.is_empty() {
            return Err(VaapiError::Unsupported("导出描述符里没有 object".into()));
        }
        if planes.is_empty() {
            return Err(VaapiError::Unsupported("导出描述符里没有平面".into()));
        }
        if planes.len() > 4 {
            return Err(VaapiError::Unsupported(format!(
                "导出描述符里有 {} 个平面，超过 DmaBufFrame 上限 4",
                planes.len()
            )));
        }
        let mut object_index = [0_u8; 4];
        let mut strides = [0_u32; 4];
        let mut offsets = [0_u32; 4];
        for (index, plane) in planes.iter().enumerate() {
            let Some(_) = objects.get(plane.object) else {
                return Err(VaapiError::Unsupported(format!(
                    "平面 {index} 指向 object {}，但描述符只有 {} 个 object",
                    plane.object,
                    objects.len()
                )));
            };
            let object_u8 = u8::try_from(plane.object).map_err(|_| {
                VaapiError::Unsupported(format!("object 下标 {} 超出 u8 范围", plane.object))
            })?;
            object_index[index] = object_u8;
            strides[index] = plane.pitch;
            offsets[index] = plane.offset;
        }
        let modifier = objects.first().map_or(0, |(_, modifier)| *modifier);
        let fds: Vec<Arc<OwnedFd>> = objects
            .into_iter()
            .map(|(fd, _modifier)| Arc::new(fd))
            .collect();
        Ok(DmaBufFrame {
            width,
            height,
            stride: strides[0],
            format,
            fds,
            modifier,
            fourcc: self_fourcc(format),
            strides,
            offsets,
            object_index,
            keepalive,
        })
    }
}

/// fourcc 与实际 `PixelFormat` 对齐（NV12 的搬运格式由显示后端按 format 解释）。
fn self_fourcc(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Nv12 => DRM_FORMAT_NV12,
        _ => DRM_FORMAT_XRGB8888,
    }
}

/// 导出 `surface` 的 DRM PRIME 2 描述符。
///
/// # Errors
///
/// `vaExportSurfaceHandle` 返回非 0 时给出状态文本；描述符自相矛盾（object/layer 越界）时给出
/// 该矛盾的具体字段。
// 指针只透传给 C：Rust 侧不解引用它，所以不需要把整个函数标 unsafe
// （把 unsafe 推给调用方等于让"指针是否有效"重新变成调用方的负担，
// 而有效性的真正保证是 `VaDevice` 的 Drop 顺序）。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn export_surface(
    display: ffi::VaDisplay,
    surface: ffi::VaSurfaceId,
) -> Result<ExportedSurface, VaapiError> {
    let mut descriptor = ffi::VaDrmPrimeSurfaceDescriptor::new();
    let flags = ffi::VA_EXPORT_SURFACE_READ_ONLY | ffi::VA_EXPORT_SURFACE_SEPARATE_LAYERS;
    // SAFETY: display 由 `VaDevice` 持有且仍存活；surface 来自同一显示解出的帧；
    // descriptor 是本地可写结构，字段长度与 libva 定义一致（见模块文档）。
    let status = unsafe {
        ffi::vaExportSurfaceHandle(
            display,
            surface,
            ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            flags,
            &raw mut descriptor,
        )
    };
    if status != ffi::VA_STATUS_SUCCESS {
        return Err(VaapiError::Export {
            surface,
            message: crate::device::va_status_text(status),
        });
    }
    let object_count = descriptor.num_objects as usize;
    let layer_count = descriptor.num_layers as usize;
    let clamped_objects = object_count.min(ffi::VA_MAX_DRM_OBJECTS);
    let mut objects = Vec::with_capacity(clamped_objects);
    let mut negative_fd = false;
    for object in descriptor.objects.iter().take(clamped_objects) {
        if object.fd < 0 {
            negative_fd = true;
            continue;
        }
        // SAFETY: libva 把 fd 的所有权交给调用方（本项目负责关闭）；先全部接管再校验，防泄漏。
        let owned = unsafe { OwnedFd::from_raw_fd(object.fd) };
        objects.push((owned, object.drm_format_modifier));
    }
    if object_count == 0 || object_count > ffi::VA_MAX_DRM_OBJECTS {
        return Err(VaapiError::Export {
            surface,
            message: format!("描述符里 num_objects={object_count} 非法"),
        });
    }
    if negative_fd {
        return Err(VaapiError::Export {
            surface,
            message: "描述符里有负的 dma-buf fd".into(),
        });
    }
    if layer_count == 0 || layer_count > ffi::VA_MAX_LAYERS {
        return Err(VaapiError::Export {
            surface,
            message: format!("描述符里 num_layers={layer_count} 非法"),
        });
    }
    let mut planes = Vec::with_capacity(layer_count);
    for layer in descriptor.layers.iter().take(layer_count) {
        let plane_count = (layer.num_planes as usize).min(4);
        for plane_idx in 0..plane_count {
            let object = layer.object_index[plane_idx] as usize;
            if object >= object_count {
                // objects 已由 OwnedFd 托管，提前返回会自动关闭所有 fd。
                return Err(VaapiError::Export {
                    surface,
                    message: format!(
                        "layer 的 object_index[{plane_idx}]={object} 越界（num_objects={object_count}）"
                    ),
                });
            }
            planes.push(ExportedPlane {
                object,
                offset: layer.offset[plane_idx],
                pitch: layer.pitch[plane_idx],
            });
        }
    }
    let modifier = objects.first().map_or(0, |(_, modifier)| *modifier);
    let exported = ExportedSurface {
        fourcc: descriptor.fourcc,
        width: descriptor.width,
        height: descriptor.height,
        objects,
        planes,
    };
    // 把真实 modifier 记录进日志一次：跨 GPU 场景下这是唯一的排查线索。
    tracing::debug!(
        fourcc = format!("0x{:08X}", exported.fourcc),
        width = exported.width,
        height = exported.height,
        objects = exported.objects.len(),
        planes = exported.planes.len(),
        modifier = format!("0x{modifier:016X}"),
        "已导出解码表面为 DMA-BUF"
    );
    Ok(exported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn fake_surface(fourcc: u32, planes: Vec<ExportedPlane>) -> ExportedSurface {
        let file = std::fs::File::open("/dev/null").unwrap();
        ExportedSurface {
            fourcc,
            width: 1920,
            height: 1200,
            objects: vec![(OwnedFd::from(file), 0x0300_0000_0060_6014)],
            planes,
        }
    }

    #[test]
    fn nv12_two_planes_map_to_their_objects() {
        let surface = ExportedSurface {
            fourcc: DRM_FORMAT_NV12,
            width: 1920,
            height: 1200,
            objects: {
                let a = std::fs::File::open("/dev/null").unwrap();
                let b = std::fs::File::open("/dev/null").unwrap();
                vec![(a.into(), 7), (b.into(), 7)]
            },
            planes: vec![
                ExportedPlane {
                    object: 0,
                    offset: 0,
                    pitch: 1920,
                },
                ExportedPlane {
                    object: 1,
                    offset: 0,
                    pitch: 1920,
                },
            ],
        };
        let frame = surface.into_frame(None).unwrap();
        assert_eq!(frame.format, PixelFormat::Nv12);
        assert_eq!(frame.object_index, [0, 1, 0, 0]);
        assert_eq!(frame.strides, [1920, 1920, 0, 0]);
        assert_eq!(frame.object_count(), 2);
        assert!(frame.is_multi_object());
    }

    #[test]
    fn single_object_two_planes_share_one_fd() {
        let surface = fake_surface(
            DRM_FORMAT_NV12,
            vec![
                ExportedPlane {
                    object: 0,
                    offset: 0,
                    pitch: 1920,
                },
                ExportedPlane {
                    object: 0,
                    offset: 1920 * 1200,
                    pitch: 1920,
                },
            ],
        );
        let frame = surface.into_frame(None).unwrap();
        assert_eq!(frame.object_index, [0, 0, 0, 0]);
        assert_eq!(frame.offsets, [0, 2_304_000, 0, 0]);
        assert_eq!(frame.object_count(), 1);
        assert!(!frame.is_multi_object());
        // 同一 object 的两个平面必须解析到同一个 fd。
        let a = frame.fd_for_plane(0).unwrap();
        let b = frame.fd_for_plane(1).unwrap();
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn unknown_fourcc_is_rejected() {
        let surface = fake_surface(
            0xdead_beef,
            vec![ExportedPlane {
                object: 0,
                offset: 0,
                pitch: 4,
            }],
        );
        let error = surface.into_frame(None).unwrap_err();
        assert!(error.to_string().contains("0xDEADBEEF"), "实际: {error}");
    }

    #[test]
    fn plane_pointing_past_objects_is_rejected() {
        let surface = fake_surface(
            DRM_FORMAT_NV12,
            vec![ExportedPlane {
                object: 3,
                offset: 0,
                pitch: 1920,
            }],
        );
        let error = surface.into_frame(None).unwrap_err();
        assert!(
            error.to_string().contains("只有 1 个 object"),
            "实际: {error}"
        );
    }

    #[test]
    fn p010_fourcc_matches_drm_macro_layout() {
        // little-endian fourcc：'P''0''1''0'
        assert_eq!(DRM_FORMAT_P010, u32::from_le_bytes(*b"P010"));
        assert_eq!(DRM_FORMAT_NV12, u32::from_le_bytes(*b"NV12"));
    }

    #[test]
    fn p010_surfaces_get_an_actionable_10bit_message() {
        let surface = fake_surface(
            DRM_FORMAT_P010,
            vec![ExportedPlane {
                object: 0,
                offset: 0,
                pitch: 3840,
            }],
        );
        let error = surface.into_frame(None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("10bit"), "实际: {text}");
        assert!(text.contains("8bit"), "实际: {text}");
        assert!(text.contains("yuv420p"), "没有给出可操作的转码建议: {text}");
    }
}
