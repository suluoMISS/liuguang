//! liuguang 二进制入口。
//!
//! 逻辑全在库目标里（`src/lib.rs`），这里只做转调——这样
//! `cargo test -p liuguang --features vaapi --lib` 覆盖到的代码就是真跑的那份。

fn main() -> std::process::ExitCode {
    liuguang::main_entry()
}
