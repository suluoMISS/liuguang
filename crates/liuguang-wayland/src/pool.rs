//! 每输出的 `wl_shm` 三槽缓冲池。
//!
//! 生命周期：`Pool::new` 创建 memfd → `ftruncate` → mmap →
//! `wl_shm.create_pool` → 3 个 `wl_buffer`。`Drop` 显式 `destroy` 三个
//! buffer 与 pool（wayland-client 0.31 的代理 Drop **不**发送 destructor
//! 请求），再 munmap（memfd 由 `OwnedFd` Drop 关闭）。
//!
//! 槽位状态机：`free → busy（commit 提交）→ free（wl_buffer.release 事件）`。
//! 三槽保证合成器最多占用 1 帧 in-flight、CPU 正在写 1 帧时仍有空槽周转
//! （单帧 in-flight 背压由 `wl_surface.frame` 回调实现，见 `backend.rs`）。

// 本模块经 sys 封装触碰 mmap 切片；unsafe 集中在 sys.rs（见其模块文档），
// 这里的两个 unsafe 调用点逐条 SAFETY 注明。
#![allow(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use wayland_client::{
    Proxy, QueueHandle,
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};

use super::state::{BufferUdata, WlState};
use super::sys;

/// 池内槽位数。
pub(crate) const SLOT_COUNT: u32 = 3;

struct Slot {
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

/// 一个输出专属的共享内存池。不可 `Send`（wayland 代理跨线程语义由
/// wayland-backend 保证，但本后端是单线程使用，见 `WaylandBackend`）。
pub(crate) struct Pool {
    /// memfd 所有权句柄：仅用于持有（fd 已交给 wl_shm，但映射在
    /// `Drop`（munmap → close）前必须保持文件打开）。
    #[allow(dead_code)] // 所有权字段，构造后不再读取
    file: OwnedFd,
    ptr: *mut u8,
    len: usize,
    /// 单槽字节数 = `stride * height`。
    slot_bytes: usize,
    stride: u32,
    height: u32,
    /// 该池 buffer 的 `wl_shm` 格式。
    format: wl_shm::Format,
    pool: wl_shm_pool::WlShmPool,
    slots: Vec<Slot>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 ptr/file/pool（含裸指针与内部句柄，无调试价值）。
        f.debug_struct("Pool")
            .field("dims", &self.dims())
            .field("format", &self.format)
            .field("slot_bytes", &self.slot_bytes)
            .field("len", &self.len)
            .field("free_slots", &self.free_slot_count())
            .finish()
    }
}

impl Pool {
    /// 创建 `width × height`、三槽、指定格式的池。
    ///
    /// `frame_pool` 只用于把 buffer 的 `release` 事件路由回正确的池：
    /// 整屏合成池与快路径帧池可能同时存在且槽位编号会重名。
    ///
    /// # Errors
    ///
    /// memfd/mmap 失败或尺寸溢出时返回错误消息（映射到
    /// `DisplayError::Failed`）。
    pub(crate) fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WlState>,
        global: u32,
        width: u32,
        height: u32,
        format: wl_shm::Format,
        frame_pool: bool,
    ) -> Result<Self, String> {
        let Some(stride) = width.checked_mul(4) else {
            return Err(format!("宽度 {width} 溢出（stride 计算）"));
        };
        let Some(slot_bytes) = (stride as usize).checked_mul(height as usize) else {
            return Err(format!("{width}x{height} 溢出（槽字节数计算）"));
        };
        let Some(len) = slot_bytes.checked_mul(SLOT_COUNT as usize) else {
            return Err(format!("{width}x{height} 溢出（池字节数计算）"));
        };
        let file =
            sys::memfd_owned(b"liuguang-wlshm\0").map_err(|e| format!("memfd_create 失败: {e}"))?;
        sys::ftruncate(file.as_raw_fd(), len).map_err(|e| format!("ftruncate 失败: {e}"))?;
        let ptr =
            sys::mmap_shared_rw(file.as_raw_fd(), len).map_err(|e| format!("mmap 失败: {e}"))?;

        // BorrowedFd 借用只存在于本次调用；fd 所有权仍在 `file`。
        let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
        let mut slots = Vec::with_capacity(SLOT_COUNT as usize);
        for slot in 0..SLOT_COUNT {
            let offset = slot as usize * slot_bytes;
            let buffer = pool.create_buffer(
                offset as i32,
                width as i32,
                height as i32,
                stride as i32,
                // 格式由调用方决定：整屏合成走 `Xrgb8888`；视频快路径直接用与
                // 帧内存字节序一致的格式（`Abgr8888` = 内存 [R,G,B,A]），
                // 这样每帧只剩一次行拷贝，缩放交给合成器 GPU。
                format,
                qh,
                BufferUdata {
                    global,
                    slot,
                    frame_pool,
                },
            );
            slots.push(Slot {
                buffer,
                busy: false,
            });
        }
        Ok(Self {
            file,
            ptr,
            len,
            slot_bytes,
            stride,
            height,
            format,
            pool,
            slots,
        })
    }

    /// 池尺寸（像素）。
    pub(crate) fn dims(&self) -> (u32, u32) {
        (self.stride / 4, self.height)
    }

    /// 池内 buffer 的 `wl_shm` 格式。
    pub(crate) fn format(&self) -> wl_shm::Format {
        self.format
    }

    /// 取第一个空闲槽位序号；全部 in-flight 时返回 `None`。
    pub(crate) fn free_slot(&self) -> Option<u32> {
        self.slots.iter().position(|s| !s.busy).map(|i| i as u32)
    }

    /// 空闲槽位数（仅诊断）。
    pub(crate) fn free_slot_count(&self) -> usize {
        self.slots.iter().filter(|s| !s.busy).count()
    }

    /// 标记槽位为 in-flight（commit 阶段调用）。
    pub(crate) fn mark_busy(&mut self, slot: u32) {
        if let Some(s) = self.slots.get_mut(slot as usize) {
            s.busy = true;
        }
    }

    /// `wl_buffer.release` 事件回收槽位。
    pub(crate) fn release_slot(&mut self, slot: u32) {
        if let Some(s) = self.slots.get_mut(slot as usize) {
            s.busy = false;
        }
    }

    /// 槽位的 `wl_buffer` 代理（attach 用）。
    pub(crate) fn buffer(&self, slot: u32) -> Option<&wl_buffer::WlBuffer> {
        self.slots.get(slot as usize).map(|s| &s.buffer)
    }

    /// 槽位的可写映射窗口（长度 `slot_bytes`）。
    ///
    /// 内部约定：`slot` 必须来自 [`Self::free_slot`]（< [`SLOT_COUNT`]），
    /// 开发期由 `debug_assert` 兜底；违反约定是本 crate 内部逻辑错误，
    /// 不是运行期可恢复状态。
    pub(crate) fn slot_data(&mut self, slot: u32) -> &mut [u8] {
        let start = slot as usize * self.slot_bytes;
        debug_assert!(start + self.slot_bytes <= self.len, "槽位越界：slot={slot}");
        // SAFETY: [ptr, ptr+len) 是本 Pool 独占的 MAP_SHARED 映射：
        // 单线程（后端无跨线程访问）、写窗口只在 present 提交阶段、
        // 池重建先 Drop 旧池（映射先解除再新建，无别名窗口）。
        // slice 借用期不跨过任何其他 slot_data 调用。
        unsafe { sys::slice_mut(self.ptr.add(start), self.slot_bytes) }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for slot in &self.slots {
            if slot.buffer.is_alive() {
                slot.buffer.destroy();
            }
        }
        if self.pool.is_alive() {
            self.pool.destroy();
        }
        // SAFETY: ptr/len 来自 sys::mmap_shared_rw，本 Drop 是唯一 munmap 点；
        // 所有切片借用已在结构体销毁前结束。
        unsafe { sys::munmap(self.ptr, self.len) };
        // `file` 由 OwnedFd Drop 关闭。
    }
}
