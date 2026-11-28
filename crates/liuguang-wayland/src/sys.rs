//! Linux syscall 封装：memfd / ftruncate / mmap / munmap / poll。
//!
//! workspace 对 `unsafe_code` 为 `deny`；本模块是 `liuguang-wayland` **唯一的**
//! `unsafe` 使用点（`#![allow(unsafe_code)]` 局部覆盖），与
//! `liuguang-render/src/gles.rs`、`liuguang-display/src/frame_probe.rs` 同一约束
//! 模式。所有 unsafe 块的 SAFETY 不变量逐条写在调用处；对外只暴露安全签名。

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// 创建匿名内存文件（`memfd_create`，`MFD_CLOEXEC`）。
///
/// # Errors
///
/// 内核拒绝（如 fd 耗尽）时返回 `io::Error`。
pub(crate) fn memfd_owned(name: &[u8]) -> io::Result<OwnedFd> {
    debug_assert!(name.ends_with(b"\0"), "memfd 名字必须 NUL 结尾");
    // SAFETY: `name` 是有效 NUL 结尾字节串（本函数私有调用方全部满足）；
    // `memfd_create` 除返回 fd 外不写任何指针。
    let rc = unsafe { libc::memfd_create(name.as_ptr().cast(), libc::MFD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `rc` 刚由 `memfd_create` 返回，是本进程唯一所有权的描述符；
    // 移交给 `OwnedFd`（其 Drop 负责 close），不重复关闭。
    Ok(unsafe { OwnedFd::from_raw_fd(rc) })
}

/// 设置文件长度（`ftruncate`）。
///
/// # Errors
///
/// fd 非法或长度非法时返回 `io::Error`。
pub(crate) fn ftruncate(fd: RawFd, len: usize) -> io::Result<()> {
    // SAFETY: fd 是本模块创建并由 `OwnedFd` 持有的有效描述符（调用方传
    // `owned.as_raw_fd()`，借用期内不会被 close）。
    let rc = unsafe { libc::ftruncate(fd, len as libc::off_t) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 创建可读写共享映射（`mmap`，`MAP_SHARED`）。
///
/// # Errors
///
/// 映射失败（fd/长度非法、内存不足）时返回 `io::Error`；
/// `MAP_FAILED` 绝不解引用。
pub(crate) fn mmap_shared_rw(fd: RawFd, len: usize) -> io::Result<*mut u8> {
    // SAFETY: `len > 0` 由调用方保证（`Pool::new` 的 `slot_bytes * 3`，
    // 宽高已验证非 0）；fd 已完成 `ftruncate`；失败路径在返回前检查
    // `MAP_FAILED`，不产生任何悬挂指针。
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(ptr.cast())
}

/// 解除映射。
///
/// # Safety
///
/// 调用方必须保证：`ptr/len` 来自本模块的 [`mmap_shared_rw`]，
/// 且同一映射只 munmap 一次（`Pool::Drop` 是唯一调用点）。
pub(crate) unsafe fn munmap(ptr: *mut u8, len: usize) {
    // SAFETY: 不变量由调用方持有。
    unsafe { libc::munmap(ptr.cast(), len) };
}

/// 由裸指针构造可写切片。
///
/// # Safety
///
/// 调用方必须保证 `[ptr, ptr+len)` 在借用期内可写且无别名：
/// 单线程访问、写窗口只在 present 提交阶段、池重建先 Drop 旧池。
pub(crate) unsafe fn slice_mut<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    // SAFETY: 不变量由调用方持有。
    unsafe { std::slice::from_raw_parts_mut(ptr, len) }
}

/// 轮询 fd 直到可读或超时（毫秒）。返回是否可读。
///
/// `EINTR` / 超时都视为未就绪，由调用方的下一轮循环重试。
pub(crate) fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` 是本栈上有效的单个 `pollfd`。
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
}

/// 读取 `zwp_linux_dmabuf_feedback_v1.format_table` 提供的只读格式表 `(fourcc, modifier)`。
///
/// 按协议约定：表项紧致排列、每项 16 字节（u32 format + u32 padding + u64 modifier），
/// 必须使用 `MAP_PRIVATE | PROT_READ` 只读映射。
pub(crate) fn read_dmabuf_format_table(fd: RawFd, size: usize) -> io::Result<Vec<(u32, u64)>> {
    const MAX_FORMAT_TABLE_BYTES: usize = 4 * 1024 * 1024;
    if size == 0 || size > MAX_FORMAT_TABLE_BYTES || size % 16 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("DMA-BUF format_table 尺寸非法: {size}"),
        ));
    }
    // SAFETY: 调用方持有 fd 所有权，只读 MAP_PRIVATE 映射 size 字节并在返回前检查 MAP_FAILED 与 munmap。
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `[ptr, ptr + size)` 为刚刚成功映射的有效只读内存区域，且长度是 16 的整数倍。
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };
    let mut entries = Vec::with_capacity(size / 16);
    for chunk in bytes.chunks_exact(16) {
        let format = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let modifier = u64::from_ne_bytes([
            chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
        ]);
        entries.push((format, modifier));
    }
    // SAFETY: `ptr/size` 来自上面成功的 `mmap`，只释放一次。
    unsafe {
        libc::munmap(ptr, size);
    }
    Ok(entries)
}

/// 通过 Unix socket `SO_PEERCRED` 获取对端合成器进程的 PID。
pub(crate) fn peer_pid(fd: RawFd) -> Option<u32> {
    let mut ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `ucred` 与 `len` 均为本栈有效变量，传给只读查询选项 `SO_PEERCRED`。
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut ucred).cast(),
            &raw mut len,
        )
    };
    if rc == 0 && ucred.pid > 0 {
        u32::try_from(ucred.pid).ok()
    } else {
        None
    }
}

/// 系统页大小（至少 4096 字节），用于对齐 memfd 打洞区间防止跨槽误清。
fn page_size() -> usize {
    static PAGE_SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: `sysconf(_SC_PAGESIZE)` 无指针入参、无全局副作用。
        let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(raw)
            .ok()
            .filter(|&n| n >= 4096 && n.is_power_of_two())
            .unwrap_or(4096)
    })
}

/// 将 memfd 区间 `[offset, offset + len)` 内**严格对齐在边界内侧**的整页归还给内核
/// （`fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`），保留文件大小与映射地址不变。
pub(crate) fn punch_hole_range(fd: RawFd, offset: usize, len: usize) -> io::Result<()> {
    let Some(end) = offset.checked_add(len) else {
        return Ok(());
    };
    let page = page_size();
    let aligned_start = offset.next_multiple_of(page);
    let aligned_end = end - (end % page);
    if aligned_end <= aligned_start {
        return Ok(());
    }
    let aligned_len = aligned_end - aligned_start;
    // SAFETY: `fd` 为调用方 `Pool` 持有的有效 memfd；`aligned_start..aligned_end` 严格落在该槽位内部，
    // 绝不会越界清零相邻槽位像素。
    let rc = unsafe {
        libc::fallocate(
            fd,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            aligned_start as libc::off_t,
            aligned_len as libc::off_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn punch_hole_range_only_clears_target_slot_even_when_unaligned() {
        let slot_bytes = 5464; // 1366x1x4，刻意非 4096 页对齐
        let total = slot_bytes * 3;
        let fd = memfd_owned(b"liuguang-hole-test\0").unwrap();
        ftruncate(fd.as_raw_fd(), total).unwrap();
        let ptr = mmap_shared_rw(fd.as_raw_fd(), total).unwrap();
        // SAFETY: `ptr..ptr+total` 独占映射，测试结束后 munmap。
        unsafe {
            let buf = slice_mut(ptr, total);
            buf.fill(0xAA);
            punch_hole_range(fd.as_raw_fd(), slot_bytes, slot_bytes).unwrap();
            assert!(
                buf[..slot_bytes].iter().all(|&b| b == 0xAA),
                "槽 0 边界不得被邻槽打洞破坏"
            );
            assert!(
                buf[slot_bytes * 2..].iter().all(|&b| b == 0xAA),
                "槽 2 边界不得被邻槽打洞破坏"
            );
            // 中间页对齐部分应已被内核清零。
            let page = page_size();
            let mid_start = slot_bytes.next_multiple_of(page);
            let mid_end = (slot_bytes * 2) - ((slot_bytes * 2) % page);
            if mid_end > mid_start {
                assert!(buf[mid_start..mid_end].iter().all(|&b| b == 0));
            }
            munmap(ptr, total);
        }
    }
}
