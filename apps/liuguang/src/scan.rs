//! 有界、可取消的**后台**媒体扫描（t22）。
//!
//! 契约（规范 §8 / t22 验收）：
//! - 扫描**绝不**在运行时线程上跑：daemon 不得因 TUI 连接、扫描或搜索而停顿；
//! - 结果经事件（`SCAN_PROGRESS` / `SCAN_FINISHED`）返回，`scan_id` 用于对账；
//! - 条目数 / 耗时 / 错误数 / 深度**全部有上限**，不得把数万条元数据塞进内存；
//! - 每条路径都过 [`MediaRoot::resolve`]（安全边界在 daemon，不在 TUI）；
//! - **不跟随符号链接**（指向根外的链接不会被遍历到）。
//!
//! 线程模型：调用方 [`spawn`] 起一条命名线程，worker 结束时把 [`ScanMsg::Finished`]
//! 发到 channel；运行时线程在 tick 里非阻塞 `try_recv` 收结果，只做 O(1) 搬运。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use liuguang_core::pathguard::MediaRoot;
use liuguang_ipc::protocol::MediaItem;

/// 单次扫描默认超时（到时**带部分结果**结束，并如实标记 `timed_out`）。
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
/// 进度事件最小间隔（避免事件刷屏把运行时拖慢）。
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// 目录遍历深度上限（含来源目录本身为第 1 层）。
pub const MAX_DEPTH: usize = 8;
/// 最多保留多少条错误（内存有界；超出的只计数）。
pub const MAX_ERRORS: usize = 32;

/// 体积超限样例的上限（样例要能读懂，不需要全量清单）。
const OVERSIZE_EXAMPLES: usize = 3;

/// 一次扫描的输入参数（由运行时线程在**起线程之前**算好，避免线程里读配置）。
pub struct ScanSpec {
    /// 扫描代次（事件里原样回传；新代次会让旧代次的结果被丢弃）。
    pub scan_id: u64,
    /// 媒体根（路径安全边界）。
    pub root: MediaRoot,
    /// 来源：`(source_id, 目录绝对路径, 是否递归)`。
    pub sources: Vec<(String, PathBuf, bool)>,
    /// 允许的扩展名（小写、无前导点）。
    pub extensions: Vec<String>,
    /// 条目上限。
    pub max_entries: usize,
    /// 时间上限。
    pub timeout: Duration,
    /// 取消标志（运行时可随时置位；新扫描会取消旧扫描）。
    pub cancel: Arc<AtomicBool>,
}

impl ScanSpec {
    /// 构造（`root`/`sources`/`extensions` 由调用方从配置算好）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scan_id: u64,
        root: MediaRoot,
        sources: Vec<(String, PathBuf, bool)>,
        extensions: Vec<String>,
        max_entries: usize,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            scan_id,
            root,
            sources,
            extensions,
            max_entries,
            timeout: SCAN_TIMEOUT,
            cancel,
        }
    }
}

/// worker → 运行时 的消息。
pub enum ScanMsg {
    /// 进度（已发现条目数）。
    Progress {
        /// 扫描代次。
        scan_id: u64,
        /// 已发现条目数。
        found: u64,
    },
    /// 结束（成功/超时/取消/截断都可能，如实标记）。
    Finished(Box<ScanOutcome>),
}

/// 扫描结果。
pub struct ScanOutcome {
    /// 扫描代次。
    pub scan_id: u64,
    /// 条目（已去重、已过安全边界）。
    pub items: Vec<MediaItem>,
    /// 已发现条目数（= `items.len()`）。
    pub found: u64,
    /// 错误（最多 [`MAX_ERRORS`] 条）。
    pub errors: Vec<String>,
    /// 错误总条数（可能大于 `errors.len()`）。
    pub error_count: u64,
    /// 是否因条目上限被截断。
    pub truncated: bool,
    /// 是否超时。
    pub timed_out: bool,
    /// 是否被取消（新扫描取代 / daemon 退出）。
    pub cancelled: bool,
    /// 耗时。
    pub elapsed: Duration,
    /// 访问过的目录数。
    pub dirs_visited: u64,
    /// 考察过的候选文件数（扩展名过滤之前）。
    pub files_considered: u64,
    /// 因**超过单文件体积上限**被跳过的文件数（`[library].max_file_bytes`）。
    ///
    /// 单独计数（而不是静默 `continue`）：8K 原始素材常在 1 GB 上下，
    /// 旧实现把它们无声丢出索引，用户只看到"库里没有这个文件"。
    pub oversize_skipped: u64,
    /// 超限样例（最多 [`OVERSIZE_EXAMPLES`] 条，含实际上限，便于改配置）。
    pub oversize_examples: Vec<String>,
}

impl ScanOutcome {
    /// 是否"干净"结束（无超时/取消/截断）。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.timed_out && !self.cancelled && !self.truncated
    }
}

/// 起一条后台扫描线程（detached；结果经 `tx` 回来）。
///
/// # Errors
///
/// 线程创建失败（资源耗尽）时返回 I/O 错误，调用方应回 `INTERNAL`。
pub fn spawn(spec: ScanSpec, tx: Sender<ScanMsg>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("liuguang-scan".to_string())
        .spawn(move || {
            let outcome = run(&spec, &tx);
            // 接收端已关闭（daemon 退出）时忽略。
            let _ = tx.send(ScanMsg::Finished(Box::new(outcome)));
        })
        .map(|_handle| ())
}

/// 扫描主体（同步；只在后台线程里跑）。
fn run(spec: &ScanSpec, tx: &Sender<ScanMsg>) -> ScanOutcome {
    let start = Instant::now();
    let mut items: Vec<MediaItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut errors: Vec<String> = Vec::new();
    let mut error_count: u64 = 0;
    let mut oversize_skipped: u64 = 0;
    let mut oversize_examples: Vec<String> = Vec::new();
    let mut dirs_visited: u64 = 0;
    let mut files_considered: u64 = 0;
    let mut truncated = false;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut last_progress = start;

    // 显式栈（不用递归：深度有界，但栈深度由配置决定，显式栈更可控）。
    let mut stack: Vec<(PathBuf, usize, bool)> = spec
        .sources
        .iter()
        .map(|(_, dir, recursive)| (dir.clone(), 1usize, *recursive))
        .collect();
    // 来源目录 → source_id，用于给条目归属（规范化后比对）。
    let source_of = |path: &std::path::Path| -> String {
        spec.sources
            .iter()
            .find(|(_, dir, _)| path.starts_with(dir))
            .map(|(id, _, _)| id.clone())
            .unwrap_or_default()
    };

    'walk: while let Some((dir, depth, recursive)) = stack.pop() {
        if spec.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if start.elapsed() > spec.timeout {
            timed_out = true;
            break;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                error_count += 1;
                if errors.len() < MAX_ERRORS {
                    errors.push(format!("{}: {e}", dir.display()));
                }
                continue;
            }
        };
        dirs_visited += 1;
        for entry in read {
            if spec.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break 'walk;
            }
            if start.elapsed() > spec.timeout {
                timed_out = true;
                break 'walk;
            }
            let Ok(entry) = entry else {
                error_count += 1;
                if errors.len() < MAX_ERRORS {
                    errors.push(format!("{}: 目录项读取失败", dir.display()));
                }
                continue;
            };
            // `file_type()` **不跟随**符号链接：指向根外的链接在这里被丢弃。
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if recursive && depth < MAX_DEPTH {
                    stack.push((entry.path(), depth + 1, recursive));
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            files_considered += 1;
            let path = entry.path();
            let Some(ext) = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
            else {
                continue;
            };
            if !spec.extensions.iter().any(|allowed| allowed == &ext) {
                continue;
            }
            // 每一条都过 daemon 的安全边界（符号链接逃逸/根外/超大/不可读）。
            let canonical = match spec.root.resolve(&path.to_string_lossy()) {
                Ok(canonical) => canonical,
                Err(error) => {
                    // 体积超限不是"解析失败"：它是**配置边界**，必须可见。
                    // 其余拒绝（根外/符号链接/不可读）保持原来的静默跳过语义。
                    if error.code == liuguang_core::pathguard::PathErrorCode::TooLarge {
                        oversize_skipped += 1;
                        if oversize_examples.len() < OVERSIZE_EXAMPLES {
                            oversize_examples.push(format!("{}: {error}", path.display()));
                        }
                    }
                    continue;
                }
            };
            let key = canonical.to_string_lossy().into_owned();
            if !seen.insert(key.clone()) {
                continue; // 同一文件被多个来源覆盖（去重）
            }
            let Ok(meta) = std::fs::metadata(&canonical) else {
                continue;
            };
            let kind = if liuguang_core::library::DEFAULT_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                "image"
            } else {
                "video"
            };
            items.push(MediaItem {
                id: key.clone(),
                source_id: source_of(&canonical),
                name: canonical
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: key,
                kind: kind.to_string(),
                size_bytes: meta.len(),
                modified_unix: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                // 头部探测：只在扫描线程里做（有界读取），失败即 `None` ——
                // 详情缺失不该让这个文件从索引里消失。
                details: crate::probe::probe(&canonical, kind),
            });
            if items.len() >= spec.max_entries {
                truncated = true;
                break 'walk;
            }
            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                last_progress = Instant::now();
                // 接收端已关闭时忽略（daemon 正在退出）。
                let _ = tx.send(ScanMsg::Progress {
                    scan_id: spec.scan_id,
                    found: items.len() as u64,
                });
            }
        }
    }

    ScanOutcome {
        scan_id: spec.scan_id,
        found: items.len() as u64,
        items,
        errors,
        error_count,
        truncated,
        timed_out,
        cancelled,
        elapsed: start.elapsed(),
        dirs_visited,
        files_considered,
        oversize_skipped,
        oversize_examples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "liuguang-scan-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec_for(root: &std::path::Path, sources: Vec<PathBuf>, recursive: bool) -> ScanSpec {
        let media = MediaRoot::new(root, &["png".to_string(), "mp4".to_string()], 1 << 20).unwrap();
        ScanSpec::new(
            7,
            media,
            sources
                .into_iter()
                .map(|p| (p.to_string_lossy().into_owned(), p, recursive))
                .collect(),
            vec!["png".to_string(), "mp4".to_string()],
            100,
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn oversize_files_are_skipped_but_counted() {
        // 8K 原始素材的典型场景：文件超过 max_file_bytes，旧实现静默丢弃。
        let root = scratch("oversize");
        std::fs::create_dir_all(root.join("videos")).unwrap();
        std::fs::write(root.join("videos/small.png"), b"x").unwrap();
        let big = root.join("videos/8k.mp4");
        std::fs::write(&big, vec![0u8; (1 << 20) + 16]).unwrap();

        let spec = spec_for(&root, vec![root.join("videos")], false);
        let (tx, _rx) = channel();
        let outcome = run(&spec, &tx);
        let names: Vec<&str> = outcome.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["small.png"], "超限文件不该进索引");
        assert_eq!(outcome.oversize_skipped, 1, "超限必须被计数");
        assert_eq!(outcome.oversize_examples.len(), 1);
        assert!(
            outcome.oversize_examples[0].contains("8k.mp4"),
            "样例要能指认文件: {:?}",
            outcome.oversize_examples
        );
        assert!(
            outcome.oversize_examples[0].contains("文件过大")
                && outcome.oversize_examples[0].contains("1048576"),
            "样例要带上限数值（可操作）: {:?}",
            outcome.oversize_examples
        );
    }

    #[test]
    fn walk_finds_only_allowed_files_and_skips_symlinks() {
        let root = scratch("walk");
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::create_dir_all(root.join("videos")).unwrap();
        std::fs::write(root.join("images/a.png"), b"x").unwrap();
        std::fs::write(root.join("images/notes.txt"), b"x").unwrap();
        std::fs::write(root.join("videos/b.mp4"), b"x").unwrap();
        // 指向根外的符号链接：不得被遍历到。
        let outside = scratch("outside");
        std::fs::write(outside.join("evil.png"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("images/link")).unwrap();

        let spec = spec_for(&root, vec![root.join("images"), root.join("videos")], true);
        let (tx, _rx) = channel();
        let outcome = run(&spec, &tx);
        let mut names: Vec<&str> = outcome.items.iter().map(|i| i.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["a.png", "b.mp4"]);
        assert_eq!(outcome.found, 2);
        assert!(outcome.is_clean(), "不该有超时/取消/截断");
        assert_eq!(outcome.error_count, 0);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn entry_cap_truncates_and_marks_it() {
        let root = scratch("cap");
        for i in 0..10 {
            std::fs::write(root.join(format!("{i}.png")), b"x").unwrap();
        }
        let media = MediaRoot::new(&root, &["png".to_string()], 1 << 20).unwrap();
        let mut spec = ScanSpec::new(
            1,
            media,
            vec![(root.to_string_lossy().into_owned(), root.clone(), true)],
            vec!["png".to_string()],
            3,
            Arc::new(AtomicBool::new(false)),
        );
        spec.timeout = Duration::from_secs(10);
        let (tx, _rx) = channel();
        let outcome = run(&spec, &tx);
        assert_eq!(outcome.items.len(), 3);
        assert!(outcome.truncated, "达到条目上限必须标记 truncated");
        assert!(!outcome.is_clean());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_flag_stops_walk() {
        let root = scratch("cancel");
        for i in 0..5 {
            std::fs::write(root.join(format!("{i}.png")), b"x").unwrap();
        }
        let media = MediaRoot::new(&root, &["png".to_string()], 1 << 20).unwrap();
        let cancel = Arc::new(AtomicBool::new(true)); // 一开始就取消
        let spec = ScanSpec::new(
            2,
            media,
            vec![(root.to_string_lossy().into_owned(), root.clone(), true)],
            vec!["png".to_string()],
            100,
            Arc::clone(&cancel),
        );
        let (tx, _rx) = channel();
        let outcome = run(&spec, &tx);
        assert!(outcome.cancelled);
        assert!(outcome.items.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_source_dir_is_reported_not_fatal() {
        let root = scratch("missing");
        let missing = root.join("nope");
        let spec = spec_for(&root, vec![missing], true);
        let (tx, _rx) = channel();
        let outcome = run(&spec, &tx);
        assert!(outcome.items.is_empty());
        assert_eq!(outcome.error_count, 1, "缺目录要如实计数，但不 panic");
        assert!(outcome.errors[0].contains("nope"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn progress_messages_carry_scan_id() {
        let root = scratch("progress");
        std::fs::write(root.join("a.png"), b"x").unwrap();
        let mut spec = spec_for(&root, vec![root.clone()], true);
        spec.timeout = Duration::from_secs(10);
        // 让进度阈值立刻到期：把 INTERVAL 视为 0 无法直接改常量，
        // 因此这里只验证 Finished 带回同一个 scan_id（进度受节流，不强求出现）。
        let (tx, rx) = channel();
        let outcome = run(&spec, &tx);
        for msg in rx.try_iter() {
            if let ScanMsg::Progress { scan_id, .. } = msg {
                assert_eq!(scan_id, 7);
            }
        }
        assert_eq!(outcome.scan_id, 7);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_delivers_finished_on_channel() {
        let root = scratch("spawn");
        std::fs::write(root.join("a.png"), b"x").unwrap();
        let spec = spec_for(&root, vec![root.clone()], true);
        let (tx, rx) = channel();
        spawn(spec, tx).unwrap();
        let msg = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("后台扫描必须返回结果");
        let ScanMsg::Finished(outcome) = msg else {
            panic!("第一条消息必须是 Finished");
        };
        assert_eq!(outcome.items.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
