//! `liuguang-wayland` — Wayland（wlroots 系合成器）显示后端。
//!
//! 通过 `zwlr_layer_shell_v1` 的 Background 层把 CPU 帧推上合成器：
//!
//! - 每输出一个 `wl_shm` 三槽池，单帧 in-flight 背压
//!   （`wl_surface.frame` 回调）；
//! - 输出键 = `wl_output.name`（与 DRM connector 名一致，如 `eDP-1`）；
//! - daemon 侧 `present_fitted` 的 16.16 定点 `src` → 模式像素 `dst`
//!   由 blit 模块双线性重采样（实际运行多为 identity 整行拷贝）；
//! - 合成器断连 → `outputs()` 为空 → daemon 进入 HeadlessReady。
//!
//! 仅 Linux（wayland-client 动态加载 libwayland）；非 Linux 平台为空 crate。

#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod blit;
#[cfg(target_os = "linux")]
mod pool;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod sys;

#[cfg(target_os = "linux")]
pub use backend::WaylandBackend;
