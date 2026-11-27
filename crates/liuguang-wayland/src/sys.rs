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
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}
