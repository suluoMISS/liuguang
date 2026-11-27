//! VA-API 设备：把一个 DRM 渲染节点变成可解码、可导出表面的 `VADisplay`。
//!
//! 为什么自己开显示而不是让 ffmpeg 代劳：
//! 解码出的 `AV_PIX_FMT_VAAPI` 帧里的表面 id **只在创建它的那个 `VADisplay` 内有效**。
//! 想对同一块表面调用 `vaExportSurfaceHandle`，就必须持有同一个显示句柄。
//! （`vashare` spike 已验证：外部传入的 `VADisplay` 能被 ffmpeg 接受。）
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use crate::{VaapiError, ffi};

/// 一个已初始化的 VA-API 显示（持有渲染节点的 fd，`Drop` 时 `vaTerminate`）。
#[derive(Debug)]
pub struct VaDevice {
    display: ffi::VaDisplay,
    /// 显示句柄绑定的节点；出错信息里要报出来。
    node: PathBuf,
    /// 持有 fd：`VADisplay` 的生命周期绑定在它上面。
    _fd: OwnedFd,
}

// SAFETY: `VADisplay` 是 libva 的不透明句柄；libva 的显示操作在单线程内按顺序调用
// （本 crate 的调用方是媒体线程）。fd 是 `OwnedFd`，跨线程移动是安全的。
unsafe impl Send for VaDevice {}

impl VaDevice {
    /// 打开 `node`（通常是 `/dev/dri/renderD128` 之类）并初始化 VA-API 显示。
    ///
    /// # Errors
    ///
    /// 节点打不开、驱动不支持 VA-API（`vaGetDisplayDRM` 返回空）、或 `vaInitialize`
    /// 失败时返回 [`VaapiError`]。
    pub fn open(node: &Path) -> Result<Self, VaapiError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|source| VaapiError::OpenNode {
                path: node.to_path_buf(),
                source,
            })?;
        let fd: OwnedFd = file.into();
        // SAFETY: fd 有效且在本结构体内保持打开；`vaGetDisplayDRM` 只记录 fd。
        let display = unsafe { ffi::vaGetDisplayDRM(std::os::fd::AsRawFd::as_raw_fd(&fd)) };
        if display.is_null() {
            return Err(VaapiError::NoDisplay(node.to_path_buf()));
        }
        let mut major: std::ffi::c_int = 0;
        let mut minor: std::ffi::c_int = 0;
        // SAFETY: display 非空且由本函数创建；两个 out 指针指向本地变量。
        let status = unsafe { ffi::vaInitialize(display, &raw mut major, &raw mut minor) };
        if status != ffi::VA_STATUS_SUCCESS {
            return Err(VaapiError::Init {
                node: node.to_path_buf(),
                message: va_status_text(status),
            });
        }
        tracing::info!(
            node = %node.display(),
            version = %format!("{major}.{minor}"),
            "VA-API 显示已初始化"
        );
        Ok(Self {
            display,
            node: node.to_path_buf(),
            _fd: fd,
        })
    }

    /// 底层 `VADisplay`（传给 ffmpeg，也用于导出表面）。
    #[must_use]
    pub fn display(&self) -> ffi::VaDisplay {
        self.display
    }

    /// 该显示绑定的节点。
    #[must_use]
    pub fn node(&self) -> &Path {
        &self.node
    }
}

impl Drop for VaDevice {
    fn drop(&mut self) {
        // SAFETY: display 由 `open` 创建且未重复终止；`Drop` 只跑一次。
        unsafe {
            ffi::vaTerminate(self.display);
        }
    }
}

/// `VAStatus` → 可读文本。
#[must_use]
pub fn va_status_text(status: ffi::VaStatus) -> String {
    // SAFETY: vaErrorStr 对任意状态码都返回静态字符串（不拥有内存）。
    let raw = unsafe { ffi::vaErrorStr(status) };
    if raw.is_null() {
        return format!("VAStatus={status}");
    }
    // SAFETY: 上面判空；libva 保证是 NUL 结尾的静态串。
    let text = unsafe { std::ffi::CStr::from_ptr(raw) };
    format!("{}（VAStatus={status}）", text.to_string_lossy())
}
