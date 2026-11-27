//! 链接 `libgstva-1.0`（`t96` 实测：本机只装了运行时的 `libgstva-1.0.so.0`，
//! **没有** `-devel` 提供的 `libgstva-1.0.so` 符号链接）。
//!
//! 因此**不能用** `#[link(name = "gstva-1.0")]`（那会要求 `.so` 存在 ⇒ 链接失败）；
//! 改用 `-l:libgstva-1.0.so.0` **按精确文件名**链接，GNU ld 支持这种写法。
//!
//! 若目标机没有该库（例如非 VAAPI 环境），链接参数**仍然会传入** ⇒ 需要时由
//! `#[cfg]`/构建开关控制；当前仅在需要 FFI 时由源码使用这些符号。
fn main() {
    // 只在 Linux 上尝试；找不到库时不应让整个构建炸掉别的东西。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-l:libgstva-1.0.so.0");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
