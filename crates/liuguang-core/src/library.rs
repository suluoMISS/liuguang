//! `[library]` 媒体库配置（规范 §10 第 4 步 / `docs/TUI_DESIGN.md` §3）。
//!
//! 职责：
//! 1. 描述**来源集合**（`root` + `image_dirs`/`video_dirs`），并保证它们都落在 `root` 内；
//! 2. 与旧的 `[slideshow].directory` **合并去重**（旧键继续生效，不破坏兼容）；
//! 3. 目录缺失只 WARN 跳过（**不阻断 daemon 启动**）；
//! 4. 校验失败时必须能报出**具体字段路径**（如 `library.image_dirs[1]`）。

use crate::pathguard::{MediaRoot, PathError};
use std::path::{Path, PathBuf};

/// 默认允许的图片扩展名。
pub const DEFAULT_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif"];
/// 默认允许的视频扩展名。
pub const DEFAULT_VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov"];

/// 来源由**哪个配置容器**派生（诊断文案必须说准，不能笼统说"root 派生"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceContainer {
    /// `[library].image_dirs`
    ImageDirs,
    /// `[library].video_dirs`
    VideoDirs,
    /// `[[library.sources]]`（运行期新增后落盘的条目）
    Sources,
    /// 旧的 `[slideshow].directory`
    LegacySlideshow,
}

impl SourceContainer {
    /// 面向人与日志的准确名字（直接写进诊断文案）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ImageDirs => "[library].image_dirs",
            Self::VideoDirs => "[library].video_dirs",
            Self::Sources => "[[library.sources]]",
            Self::LegacySlideshow => "[slideshow].directory",
        }
    }
}

/// 一个**已落盘**的来源条目（`[[library.sources]]`）。
///
/// 为什么需要它：`image_dirs`/`video_dirs` 只有"按类型的两串目录"，表达不了
/// **运行期 `add_source` 带进来的 `kind` + `recursive`**。若强行映射进 dirs，
/// 会把 `recursive` 丢掉、把 `all` 二元化成 image/video，重启后语义就变了。
/// 因此运行期新增的来源在保存时写进本表，与 dirs **并列**生效（两者都进
/// `effective_sources()`），删除则从对应容器里移除。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LibrarySource {
    /// 目录（绝对路径，或相对 `root` 的路径）。
    pub path: PathBuf,
    /// `image` / `video` / `all`（非法值在 `validate()` 里被拒）。
    #[serde(default = "default_source_kind")]
    pub kind: String,
    /// 是否递归扫描该目录。
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

fn default_source_kind() -> String {
    "all".to_string()
}

const fn default_recursive() -> bool {
    true
}

/// 媒体库配置（`[library]`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryConfig {
    /// 媒体根目录（绝对路径；所有来源必须落在其内）。
    pub root: PathBuf,
    /// 图片目录（相对 `root` 或绝对路径；必须落在 `root` 内）。
    pub image_dirs: Vec<PathBuf>,
    /// 视频目录（同上）。
    pub video_dirs: Vec<PathBuf>,
    /// 是否递归扫描子目录。
    pub recursive: bool,
    /// 允许的扩展名（小写、无点）。为空时按类型取默认值。
    pub allowed_extensions: Vec<String>,
    /// 单文件最大字节数（0 = 不限制）。
    ///
    /// 默认 **2 GiB**（t6 从 512 MiB 上调）：8K 壁纸素材动辄 900 MB–1.5 GB，
    /// 旧默认会把它们静默挡在索引外（表现为"库里没有这个文件"）。
    /// 仍然有上限是为了挡住误挂载的巨型文件；真超限的文件会出现在
    /// `list_media.skipped_oversize` / `skipped_examples` 里，而不是无声消失。
    pub max_file_bytes: u64,
    /// **运行期新增并已保存**的来源（`[[library.sources]]`）。
    ///
    /// 与 `image_dirs`/`video_dirs` **并列**生效：两者都进 `effective_sources()`。
    /// 由 `save_config` 在落盘前把 `add_source`/`remove_source` 的改动合并进来
    /// （T25-F1：旧实现不合并 ⇒ 用户新增的来源重启后丢失）。
    pub sources: Vec<LibrarySource>,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/srv/wallpapers"),
            image_dirs: vec![PathBuf::from("images")],
            video_dirs: vec![PathBuf::from("videos")],
            recursive: true,
            allowed_extensions: Vec::new(),
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            sources: Vec::new(),
        }
    }
}

impl LibraryConfig {
    /// 把一个来源目录解析成**绝对路径**（相对路径相对于 `root`）。
    ///
    /// 只做拼接，不做存在性/边界检查（边界检查见 [`Self::validate`] 与
    /// [`crate::pathguard::MediaRoot::check_dir_in_root`]）。
    #[must_use]
    pub fn abs_dir(&self, dir: &std::path::Path) -> std::path::PathBuf {
        if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            self.root.join(dir)
        }
    }

    /// 全部允许扩展名（配置为空时合并默认图片+视频扩展名）。
    #[must_use]
    pub fn extensions(&self) -> Vec<String> {
        if !self.allowed_extensions.is_empty() {
            return self
                .allowed_extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect();
        }
        DEFAULT_IMAGE_EXTENSIONS
            .iter()
            .chain(DEFAULT_VIDEO_EXTENSIONS.iter())
            .map(|e| (*e).to_string())
            .collect()
    }

    /// 校验并解析为 [`MediaRoot`]。
    ///
    /// 校验顺序：`image_dirs`/`video_dirs` 逐项必须在 `root` 内经 [`MediaRoot::check_dir_in_root`]
    /// （相对路径按 `root` 拼接，因此 `images` 这种写法是合法的）。
    ///
    /// # Errors
    ///
    /// 返回 `(字段路径, 原因)`：字段路径形如 `library.image_dirs[1]`，
    /// 便于配置报错时直接指到出错的那一行。
    pub fn validate(&self) -> Result<MediaRoot, (String, PathError)> {
        let root = MediaRoot::new(&self.root, &self.extensions(), self.max_file_bytes)
            .map_err(|e| ("library.root".to_string(), e))?;
        for (field, dirs) in [
            ("library.image_dirs", &self.image_dirs),
            ("library.video_dirs", &self.video_dirs),
        ] {
            for (i, dir) in dirs.iter().enumerate() {
                let candidate = if dir.is_absolute() {
                    dir.clone()
                } else {
                    root.root().join(dir)
                };
                root.check_dir_in_root(&candidate)
                    .map_err(|e| (format!("{field}[{i}]"), e))?;
            }
        }
        for (i, src) in self.sources.iter().enumerate() {
            if !matches!(src.kind.as_str(), "image" | "video" | "all") {
                return Err((
                    format!("library.sources[{i}].kind"),
                    PathError::invalid(format!(
                        "来源类型必须是 image/video/all（收到 {:?}）",
                        src.kind
                    )),
                ));
            }
            let candidate = if src.path.is_absolute() {
                src.path.clone()
            } else {
                root.root().join(&src.path)
            };
            root.check_dir_in_root(&candidate)
                .map_err(|e| (format!("library.sources[{i}].path"), e))?;
        }
        Ok(root)
    }

    /// 该配置**派生的来源**，并标明是哪个容器派生的（诊断用）。
    ///
    /// **重要**：`library.root` **不是**来源——它只是 `image_dirs`/`video_dirs`
    /// 相对路径的拼接基准。曾经的诊断文案说"该来源由 `library.root` 派生"，
    /// 那是错的（`effective_sources()` 从不把 root 当来源）。
    ///
    /// 运行期的 `add_source`/`remove_source` **不在**本函数的视野内：它是**纯配置层**
    /// 的派生结果，正是"保存后重启会看到什么"的预演依据。
    #[must_use]
    pub fn derived_sources(
        &self,
        slideshow_directory: Option<&Path>,
    ) -> Vec<(SourceContainer, PathBuf)> {
        // **刻意不去重**：这是"容器 → 路径"的映射，不是来源列表。
        // 诊断时必须能报出**所有**仍覆盖某路径的容器（例如同一个目录同时出现在
        // `image_dirs` 与 `[slideshow].directory` 里）——去重会让冲突报告漏掉一个，
        // 于是"为什么删不掉"又变成谜。去重是 `effective_sources()` 的事。
        let mut out: Vec<(SourceContainer, PathBuf)> = Vec::new();
        for dir in &self.image_dirs {
            out.push((SourceContainer::ImageDirs, self.abs_dir(dir)));
        }
        for dir in &self.video_dirs {
            out.push((SourceContainer::VideoDirs, self.abs_dir(dir)));
        }
        for src in &self.sources {
            out.push((SourceContainer::Sources, self.abs_dir(&src.path)));
        }
        if let Some(legacy) = slideshow_directory {
            if !legacy.as_os_str().is_empty() {
                out.push((SourceContainer::LegacySlideshow, legacy.to_path_buf()));
            }
        }
        out
    }

    /// 与 `[slideshow].directory` 合并后的**来源集合**（去重、保持稳定顺序）。
    ///
    /// 旧的 `slideshow.directory` 继续生效：它被并入来源集合，且**不改变其语义**
    /// （仍作为轮换目录使用）。去重按规范化后的路径比较。
    #[must_use]
    pub fn merge_sources(&self, slideshow_directory: Option<&Path>) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for dir in self.image_dirs.iter().chain(self.video_dirs.iter()) {
            let candidate = if dir.is_absolute() {
                dir.clone()
            } else {
                self.root.join(dir)
            };
            let key = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if !out
                .iter()
                .any(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == key)
            {
                out.push(candidate);
            }
        }
        if let Some(legacy) = slideshow_directory {
            let key = std::fs::canonicalize(legacy).unwrap_or_else(|_| legacy.to_path_buf());
            if !out
                .iter()
                .any(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == key)
            {
                out.push(legacy.to_path_buf());
            }
        }
        out
    }

    /// 目录存在性检查：把来源集合拆成「存在的目录」与「缺失/非目录」两部分。
    ///
    /// **不阻断启动**：调用方（daemon）对缺失项记 WARN 后跳过即可；
    /// 本模块不依赖日志框架（core 层只做纯逻辑）。
    #[must_use]
    pub fn split_sources(
        &self,
        slideshow_directory: Option<&Path>,
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut existing = Vec::new();
        let mut missing = Vec::new();
        for dir in self.merge_sources(slideshow_directory) {
            if dir.is_dir() {
                existing.push(dir);
            } else {
                missing.push(dir);
            }
        }
        (existing, missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("liuguang-library-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images")).expect("mk images");
        std::fs::create_dir_all(dir.join("videos")).expect("mk videos");
        dir
    }

    /// **T37**：来源派生必须能报出**准确容器名**（诊断文案不许说"由 root 派生"）。
    #[test]
    fn derived_sources_name_the_actual_container() {
        let lib = LibraryConfig {
            root: PathBuf::from("/srv/w"),
            image_dirs: vec![PathBuf::from("images")],
            video_dirs: vec![PathBuf::from("videos")],
            sources: vec![LibrarySource {
                path: PathBuf::from("/srv/w/extra"),
                kind: "image".into(),
                recursive: true,
            }],
            ..LibraryConfig::default()
        };
        let d = lib.derived_sources(Some(Path::new("/srv/w/images")));
        // images 同时被 image_dirs 与 legacy 覆盖：**两个容器都要报出来**（去重后仍两条）。
        let names: Vec<&str> = d.iter().map(|(c, _)| c.name()).collect();
        assert!(names.contains(&"[library].image_dirs"), "实际 {names:?}");
        assert!(names.contains(&"[slideshow].directory"), "实际 {names:?}");
        assert!(names.contains(&"[library].video_dirs"), "实际 {names:?}");
        assert!(names.contains(&"[[library.sources]]"), "实际 {names:?}");
        // `library.root` **不是容器**：任何一条都不许声称来自 root。
        assert!(
            names.iter().all(|n| !n.contains("root")),
            "library.root 不派生来源，不得出现在容器名里"
        );
        // 空 legacy 不贡献来源。
        assert_eq!(lib.derived_sources(Some(Path::new(""))).len(), 3);
        assert_eq!(lib.derived_sources(None).len(), 3);
        // 同一路径被两个容器覆盖 ⇒ 两条都要出来（冲突报告不能漏）。
        assert_eq!(d.len(), 4, "去重会漏掉冲突容器");
    }

    #[test]
    fn accepts_defaults_inside_root_and_reports_field_path() {
        let root = tmp("ok");
        let cfg = LibraryConfig {
            root: root.clone(),
            ..LibraryConfig::default()
        };
        let media = cfg.validate().expect("默认配置应通过");
        assert_eq!(media.root(), std::fs::canonicalize(&root).expect("canon"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_dir_outside_root_with_field_path() {
        let root = tmp("outside");
        let cfg = LibraryConfig {
            root: root.clone(),
            image_dirs: vec![PathBuf::from("images"), PathBuf::from("/etc")],
            ..LibraryConfig::default()
        };
        let (field, err) = cfg.validate().expect_err("根外目录必须被拒");
        assert_eq!(field, "library.image_dirs[1]");
        assert_eq!(err.code.as_str(), "PATH_OUTSIDE_ROOT");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merges_legacy_slideshow_directory_without_duplicates() {
        let root = tmp("merge");
        let legacy = root.join("videos");
        let cfg = LibraryConfig {
            root: root.clone(),
            ..LibraryConfig::default()
        };
        let sources = cfg.merge_sources(Some(&legacy));
        // images + videos + legacy(videos) ⇒ 去重后恰 2 个。
        assert_eq!(sources.len(), 2, "重复来源必须去重: {sources:?}");
        let with_legacy_dup = cfg.merge_sources(Some(&root.join("images")));
        assert_eq!(with_legacy_dup.len(), 2, "旧键与现有来源重复时不得重复计数");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_dirs_are_skipped_not_fatal() {
        let root = tmp("missing");
        std::fs::remove_dir_all(root.join("videos")).expect("rm videos");
        let cfg = LibraryConfig {
            root: root.clone(),
            ..LibraryConfig::default()
        };
        // 校验通过（目录存在与否不影响配置合法性）…
        cfg.validate().expect("缺失目录不得让校验失败");
        // …而实际可用来源只剩存在的那个。
        let (existing, missing) = cfg.split_sources(None);
        assert_eq!(
            missing.len(),
            1,
            "缺失目录必须报告给调用方（由其 WARN 并跳过）"
        );
        assert_eq!(existing.len(), 1);
        assert!(existing[0].ends_with("images"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extension_defaults_cover_images_and_videos() {
        let cfg = LibraryConfig::default();
        let exts = cfg.extensions();
        assert!(exts.contains(&"png".to_string()));
        assert!(exts.contains(&"mp4".to_string()));
        // 显式配置优先（不再合并默认值）。
        let cfg = LibraryConfig {
            allowed_extensions: vec![".PNG".to_string()],
            ..LibraryConfig::default()
        };
        assert_eq!(cfg.extensions(), vec!["png".to_string()]);
    }
}
