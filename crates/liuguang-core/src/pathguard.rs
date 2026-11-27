//! 媒体路径安全边界（规范 §2/§8：**权限与路径检查必须在 daemon 再次执行**）。
//!
//! TUI 的本地校验只是前置提示，**不构成安全边界**。本模块是唯一权威：
//! daemon 收到任何媒体路径后必须经 [`MediaRoot::resolve`] 规范化与边界校验。
//!
//! 拒绝形态（错误码稳定、机器可读，可直接映射到 IPC 的 `ErrorKind`）：
//!
//! | 情形 | 错误码 |
//! |---|---|
//! | 空串 / 超长 / 非绝对路径（`~/a.png`、`./a.png`） | `INVALID_PARAMS` |
//! | 含 `..` 或规范化后落在根外 | `PATH_OUTSIDE_ROOT` |
//! | 符号链接指向根外（**逃逸**） | `PATH_OUTSIDE_ROOT` |
//! | 路径不存在 | `NOT_FOUND` |
//! | 不是普通文件 / 目录 | `NOT_A_FILE` |
//! | 扩展名不在白名单 | `EXTENSION_NOT_ALLOWED` |
//! | 超过最大文件大小 | `TOO_LARGE` |
//! | 不可读（权限位） | `UNREADABLE` |
//!
//! 说明：`..` 会在**规范化之前**被显式拒绝（即便它最终仍落在根内），
//! 因为"能写 `..` 的路径"本身就是危险信号，且不同平台的规范化语义不一致。

use std::path::{Path, PathBuf};

/// `PATH_MAX` 的保守上界（Linux 4096）；超过即视为非法输入。
pub const MAX_PATH_LEN: usize = 4096;

/// 路径校验失败原因（错误码稳定，供 IPC 直接透出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathErrorCode {
    /// 输入非法（空、超长、非绝对路径等）。
    InvalidParams,
    /// 路径落在媒体根目录之外（含 `..` 与符号链接逃逸）。
    PathOutsideRoot,
    /// 路径不存在。
    NotFound,
    /// 不是普通文件。
    NotAFile,
    /// 扩展名不在允许列表。
    ExtensionNotAllowed,
    /// 文件超过最大大小。
    TooLarge,
    /// 不可读（权限位不允许）。
    Unreadable,
}

impl PathErrorCode {
    /// 稳定的线上错误码字符串（供 IPC `ErrorKind` 使用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidParams => "INVALID_PARAMS",
            Self::PathOutsideRoot => "PATH_OUTSIDE_ROOT",
            Self::NotFound => "NOT_FOUND",
            Self::NotAFile => "NOT_A_FILE",
            Self::ExtensionNotAllowed => "EXTENSION_NOT_ALLOWED",
            Self::TooLarge => "TOO_LARGE",
            Self::Unreadable => "UNREADABLE",
        }
    }
}

/// 带机器可读错误码的路径校验错误（`Display` 面向人，`code` 面向程序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    /// 机器可读错误码。
    pub code: PathErrorCode,
    /// 人类可读说明（含被拒绝的原始输入）。
    pub message: String,
}

impl PathError {
    fn new(code: PathErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 构造一个"参数非法"错误（供上层模块校验自己的字段时复用）。
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(PathErrorCode::InvalidParams, message)
    }
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for PathError {}

/// 媒体根目录 + 白名单策略。
#[derive(Debug, Clone)]
pub struct MediaRoot {
    root: PathBuf,
    allowed_extensions: Vec<String>,
    max_bytes: u64,
}

impl MediaRoot {
    /// 构造：`root` 会被规范化（必须已存在）；扩展名统一小写并去掉前导点。
    ///
    /// # Errors
    ///
    /// 根目录不存在或不是目录时返回错误（调用方应把它当作配置错误）。
    pub fn new(
        root: &Path,
        allowed_extensions: &[String],
        max_bytes: u64,
    ) -> Result<Self, PathError> {
        let canonical = std::fs::canonicalize(root).map_err(|_| {
            PathError::new(
                PathErrorCode::NotFound,
                format!("媒体根目录不存在: {}", root.display()),
            )
        })?;
        if !canonical.is_dir() {
            return Err(PathError::new(
                PathErrorCode::NotAFile,
                format!("媒体根目录不是目录: {}", canonical.display()),
            ));
        }
        Ok(Self {
            root: canonical,
            allowed_extensions: allowed_extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            max_bytes,
        })
    }

    /// 规范化后的根目录。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 判定"目录型来源"是否落在根内（配置校验用；不要求目录存在）。
    ///
    /// # Errors
    ///
    /// 相对路径 / 超长 / 落在根外时返回对应错误码。
    pub fn check_dir_in_root(&self, dir: &Path) -> Result<PathBuf, PathError> {
        let text = dir.to_string_lossy().into_owned();
        if text.is_empty() {
            return Err(PathError::new(PathErrorCode::InvalidParams, "空路径"));
        }
        if !dir.is_absolute() {
            return Err(PathError::new(
                PathErrorCode::InvalidParams,
                format!("必须是绝对路径: {text}"),
            ));
        }
        if dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PathError::new(
                PathErrorCode::PathOutsideRoot,
                format!("路径不得包含 ..: {text}"),
            ));
        }
        // 目录可能尚不存在：先规范化其存在的最长前缀，再拼接剩余部分。
        let mut existing = dir.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !existing.exists() {
            let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            tail.push(name);
            if !existing.pop() {
                break;
            }
        }
        let base = std::fs::canonicalize(&existing)
            .map_err(|_| PathError::new(PathErrorCode::NotFound, format!("路径不存在: {text}")))?;
        let mut resolved = base;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        if !resolved.starts_with(&self.root) {
            return Err(PathError::new(
                PathErrorCode::PathOutsideRoot,
                format!("目录落在媒体根之外（root={}）: {text}", self.root.display()),
            ));
        }
        Ok(resolved)
    }

    /// 规范化并校验一个**媒体文件**路径（daemon 的唯一入口）。
    ///
    /// # Errors
    ///
    /// 见 [`PathErrorCode`]；不改变任何运行时状态（调用方在失败时保持当前壁纸）。
    pub fn resolve(&self, candidate: &str) -> Result<PathBuf, PathError> {
        if candidate.is_empty() {
            return Err(PathError::new(PathErrorCode::InvalidParams, "空路径"));
        }
        if candidate.len() > MAX_PATH_LEN {
            return Err(PathError::new(
                PathErrorCode::InvalidParams,
                format!("路径过长（>{} 字节）", MAX_PATH_LEN),
            ));
        }
        let path = Path::new(candidate);
        // 不做 `~` 展开、不接受相对路径：daemon 只接受经过规范化的绝对路径，
        // 否则 TUI 的工作目录会悄悄改变语义。
        if candidate.starts_with('~') || !path.is_absolute() {
            return Err(PathError::new(
                PathErrorCode::InvalidParams,
                format!("必须是绝对路径（不接受 ~ 或相对路径）: {candidate}"),
            ));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PathError::new(
                PathErrorCode::PathOutsideRoot,
                format!("路径不得包含 ..: {candidate}"),
            ));
        }
        let canonical = std::fs::canonicalize(path).map_err(|_| {
            PathError::new(PathErrorCode::NotFound, format!("路径不存在: {candidate}"))
        })?;
        // 符号链接与 `..` 都在这里被一并拿下：`canonicalize` 会解析全部符号链接，
        // 因此"指向根外的链接"必然落在这里被拒。
        if !canonical.starts_with(&self.root) {
            return Err(PathError::new(
                PathErrorCode::PathOutsideRoot,
                format!(
                    "路径落在媒体根之外（root={}）: {}",
                    self.root.display(),
                    canonical.display()
                ),
            ));
        }
        let ext = canonical
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if ext.is_empty() || !self.allowed_extensions.contains(&ext) {
            return Err(PathError::new(
                PathErrorCode::ExtensionNotAllowed,
                format!("不允许的扩展名（.{}）: {}", ext, canonical.display()),
            ));
        }
        let meta = std::fs::metadata(&canonical).map_err(|_| {
            PathError::new(
                PathErrorCode::NotFound,
                format!("无法读取元数据: {}", canonical.display()),
            )
        })?;
        if !meta.is_file() {
            return Err(PathError::new(
                PathErrorCode::NotAFile,
                format!("不是普通文件: {}", canonical.display()),
            ));
        }
        if self.max_bytes > 0 && meta.len() > self.max_bytes {
            return Err(PathError::new(
                PathErrorCode::TooLarge,
                format!("文件过大（{} > {} 字节）", meta.len(), self.max_bytes),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o444 == 0 {
                return Err(PathError::new(
                    PathErrorCode::Unreadable,
                    format!("文件不可读（权限位）: {}", canonical.display()),
                ));
            }
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "liuguang-pathguard-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn policy(root: &Path) -> MediaRoot {
        MediaRoot::new(
            root,
            &["png".to_string(), "jpg".to_string(), "mp4".to_string()],
            10 * 1024 * 1024,
        )
        .expect("root")
    }

    #[test]
    fn accepts_legal_absolute_media_path() {
        let root = tmpdir("ok");
        let file = root.join("a.png");
        std::fs::write(&file, b"x").expect("write");
        let p = policy(&root);
        let resolved = p.resolve(file.to_str().expect("utf8")).expect("应接受");
        assert!(resolved.starts_with(p.root()));
        assert!(resolved.ends_with("a.png"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_empty_and_relative_paths() {
        let root = tmpdir("rel");
        let p = policy(&root);
        assert_eq!(
            p.resolve("").unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        assert_eq!(
            p.resolve("~/a.png").unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        assert_eq!(
            p.resolve("./a.png").unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        assert_eq!(
            p.resolve("a.png").unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_non_media_extension_and_missing_and_dir() {
        let root = tmpdir("ext");
        std::fs::write(root.join("notes.txt"), b"x").expect("write");
        let p = policy(&root);
        assert_eq!(
            p.resolve(root.join("notes.txt").to_str().expect("utf8"))
                .unwrap_err()
                .code,
            PathErrorCode::ExtensionNotAllowed
        );
        assert_eq!(
            p.resolve(root.join("missing.png").to_str().expect("utf8"))
                .unwrap_err()
                .code,
            PathErrorCode::NotFound
        );
        // 目录即使扩展名合法也不是媒体文件。
        let sub = root.join("dir.png");
        std::fs::create_dir_all(&sub).expect("mkdir");
        assert_eq!(
            p.resolve(sub.to_str().expect("utf8")).unwrap_err().code,
            PathErrorCode::NotAFile
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_too_long_path() {
        let root = tmpdir("long");
        let p = policy(&root);
        let long = format!("/{}/a.png", "x".repeat(MAX_PATH_LEN));
        assert_eq!(
            p.resolve(&long).unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_traversal_and_outside_root() {
        let root = tmpdir("escape");
        let outside = tmpdir("outside");
        let outside_file = outside.join("b.png");
        std::fs::write(&outside_file, b"x").expect("write");
        let p = policy(&root);
        // 根外文件（绝对路径）
        assert_eq!(
            p.resolve(outside_file.to_str().expect("utf8"))
                .unwrap_err()
                .code,
            PathErrorCode::PathOutsideRoot
        );
        // 目录穿越（即使最终落到根内也必须拒绝）
        let traversal = format!("{}/../{}/b.png", root.display(), "..");
        assert_eq!(
            p.resolve(&traversal).unwrap_err().code,
            PathErrorCode::PathOutsideRoot
        );
        // 超大小
        let big = root.join("big.png");
        std::fs::write(&big, vec![0u8; 11 * 1024 * 1024]).expect("write");
        assert_eq!(
            p.resolve(big.to_str().expect("utf8")).unwrap_err().code,
            PathErrorCode::TooLarge
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_and_accepts_inside_symlink() {
        let root = tmpdir("symlink");
        let outside = tmpdir("symlink-outside");
        let secret = outside.join("secret.png");
        std::fs::write(&secret, b"x").expect("write");
        let inside = root.join("inside.png");
        std::fs::write(&inside, b"x").expect("write");
        std::os::unix::fs::symlink(&secret, root.join("escape.png")).expect("symlink out");
        std::os::unix::fs::symlink(&inside, root.join("alias.png")).expect("symlink in");
        let p = policy(&root);
        // 指向根外的符号链接必须被拒（canonicalize 后落在外）。
        assert_eq!(
            p.resolve(root.join("escape.png").to_str().expect("utf8"))
                .unwrap_err()
                .code,
            PathErrorCode::PathOutsideRoot
        );
        // 指向根内的符号链接允许（规范化后仍在根内）。
        let ok = p
            .resolve(root.join("alias.png").to_str().expect("utf8"))
            .expect("根内链接应允许");
        assert!(ok.starts_with(p.root()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn dir_source_must_be_inside_root() {
        let root = tmpdir("dirs");
        let outside = tmpdir("dirs-outside");
        let p = policy(&root);
        assert!(p.check_dir_in_root(&root.join("images")).is_ok());
        assert_eq!(
            p.check_dir_in_root(&outside).unwrap_err().code,
            PathErrorCode::PathOutsideRoot
        );
        assert_eq!(
            p.check_dir_in_root(Path::new("images")).unwrap_err().code,
            PathErrorCode::InvalidParams
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn error_codes_are_stable_strings() {
        assert_eq!(PathErrorCode::PathOutsideRoot.as_str(), "PATH_OUTSIDE_ROOT");
        assert_eq!(PathErrorCode::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(PathErrorCode::InvalidParams.as_str(), "INVALID_PARAMS");
        assert_eq!(PathErrorCode::NotAFile.as_str(), "NOT_A_FILE");
    }
}
