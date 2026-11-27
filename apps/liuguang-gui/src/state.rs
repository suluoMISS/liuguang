//! GUI 侧的模型：媒体行的筛选/排序、可持久化的界面偏好、运行期状态快照。
//!
//! 这里刻意不碰 GTK：筛选与排序是纯函数，能在没有显示器的机器上单测
//! （CI 与 `--self-test` 都跑不了窗口）。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use liuguang_core::config::Config;
use liuguang_ipc::protocol::{
    ListSourcesResult, MediaDetails, MediaItem, MetricsResult, OutputStatus, Playlist, StatusResult,
};
use serde::{Deserialize, Serialize};

/// 筛选条件的显示顺序（索引即下拉框序号，也是 `Prefs.filter` 存的值）。
pub const FILTERS: [&str; 8] = [
    "全部",
    "图片",
    "视频",
    "1080p 及以上",
    "1440p 及以上",
    "2160p 及以上",
    "4320p 及以上",
    "收藏",
];

/// 排序方式的显示顺序（索引即 `Prefs.sort`）。
pub const SORTS: [&str; 5] = ["名称", "大小", "修改时间", "分辨率", "帧率"];

/// 库的筛选条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Image,
    Video,
    Hd1080,
    Hd1440,
    Uhd2160,
    Uhd4320,
    Favorite,
}

impl Filter {
    /// 从下拉框序号还原（越界按"全部"处理，老配置文件里可能出现更大值）。
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Image,
            2 => Self::Video,
            3 => Self::Hd1080,
            4 => Self::Hd1440,
            5 => Self::Uhd2160,
            6 => Self::Uhd4320,
            7 => Self::Favorite,
            _ => Self::All,
        }
    }

    /// 是否命中（分辨率按长边算，竖屏壁纸也能正确归档）。
    #[must_use]
    pub fn matches(self, item: &MediaItem, favorites: &BTreeSet<String>) -> bool {
        match self {
            Self::All => true,
            Self::Image => item.kind == "image",
            Self::Video => item.kind == "video",
            Self::Favorite => favorites.contains(&item.id),
            Self::Hd1080 => long_edge(item).is_some_and(|px| px >= 1920),
            Self::Hd1440 => long_edge(item).is_some_and(|px| px >= 2560),
            Self::Uhd2160 => long_edge(item).is_some_and(|px| px >= 3840),
            Self::Uhd4320 => long_edge(item).is_some_and(|px| px >= 7680),
        }
    }
}

/// 排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Size,
    Modified,
    Resolution,
    Fps,
}

impl Sort {
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Size,
            2 => Self::Modified,
            3 => Self::Resolution,
            4 => Self::Fps,
            _ => Self::Name,
        }
    }
}

/// 长边像素（没有探测结果的条目返回 `None`，排序/筛选时排在后面）。
#[must_use]
pub fn long_edge(item: &MediaItem) -> Option<u32> {
    item.details
        .as_ref()
        .map(|d| d.width.max(d.height))
        .filter(|px| *px > 0)
}

/// 帧率（毫赫兹 → 浮点 fps）。
#[must_use]
pub fn fps(item: &MediaItem) -> Option<f64> {
    item.details
        .as_ref()
        .and_then(|d| d.fps_millihz)
        .map(|m| f64::from(m) / 1000.0)
        .filter(|f| *f > 0.0)
}

/// 过滤 + 排序。`needle` 为空表示不搜索。
#[must_use]
pub fn visible_items(
    items: &[MediaItem],
    needle: &str,
    filter: Filter,
    sort: Sort,
    favorites: &BTreeSet<String>,
) -> Vec<MediaItem> {
    let needle = needle.trim().to_lowercase();
    let mut rows: Vec<MediaItem> = items
        .iter()
        .filter(|item| filter.matches(item, favorites))
        .filter(|item| {
            needle.is_empty()
                || item.name.to_lowercase().contains(&needle)
                || item.path.to_lowercase().contains(&needle)
                || item.kind.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| match sort {
        Sort::Name => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path)),
        Sort::Size => b.size_bytes.cmp(&a.size_bytes),
        Sort::Modified => b.modified_unix.cmp(&a.modified_unix),
        Sort::Resolution => long_edge(b).unwrap_or(0).cmp(&long_edge(a).unwrap_or(0)),
        Sort::Fps => fps(b).unwrap_or(0.0).total_cmp(&fps(a).unwrap_or(0.0)),
    });
    rows
}

/// 可持久化的界面偏好（跟着用户走，而不是跟着 daemon 配置走）。
///
/// 存 `~/.config/liuguang/gui.json`：与 `config.toml` 同目录，卸载时一起可见；
/// 读写失败一律静默降级为默认值——界面偏好不值得弹错误框。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// 收藏的媒体 id。
    pub favorites: Vec<String>,
    /// `grid` / `list`。
    pub view: String,
    pub sort: usize,
    pub filter: usize,
    /// 卡片宽度（像素）。
    pub card_size: i32,
    /// 属性面板是否显示。
    pub show_details: bool,
    /// 上次停留的页面（library/playlists/displays/settings/about）。
    pub show_page: String,
    /// 主窗口宽度（逻辑像素）；下次启动按这个尺寸打开。
    #[serde(default = "default_window_width")]
    pub window_width: i32,
    /// 主窗口高度（逻辑像素）。
    #[serde(default = "default_window_height")]
    pub window_height: i32,
    /// 上次退出时是否最大化。
    #[serde(default)]
    pub window_maximized: bool,
}

fn default_window_width() -> i32 {
    1440
}

fn default_window_height() -> i32 {
    920
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            favorites: Vec::new(),
            view: "grid".to_owned(),
            sort: 0,
            filter: 0,
            card_size: 240,
            show_details: true,
            show_page: "library".to_owned(),
            window_width: default_window_width(),
            window_height: default_window_height(),
            window_maximized: false,
        }
    }
}

impl Prefs {
    /// 读偏好；文件不存在/坏掉都退回默认值（并保留一次可读的日志）。
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(prefs) => prefs,
            Err(e) => {
                eprintln!("liuguang-gui: 界面偏好解析失败（{}）：{e}", path.display());
                Self::default()
            }
        }
    }

    /// 写偏好（原子写：先写临时文件再 rename）。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned());
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// GUI 的完整运行期状态（单线程：GTK 主线程独占，用 `RefCell` 包在上下文里）。
#[derive(Debug)]
pub struct State {
    pub rows: Vec<MediaItem>,
    pub total: usize,
    pub scanning: bool,
    pub status: Option<StatusResult>,
    pub metrics: Option<MetricsResult>,
    pub outputs: Vec<OutputStatus>,
    pub sources: Option<ListSourcesResult>,
    pub playlists: Vec<Playlist>,
    pub config: Option<Config>,
    pub prefs: Prefs,
    pub prefs_path: PathBuf,
    /// 当前选中的媒体 id（路径）。
    pub selected: Option<String>,
    /// 最近一次错误（状态栏常驻，直到下一次成功操作）。
    pub last_error: Option<String>,
    /// 媒体库为空时的引导文案（"把图片/视频放进 <目录>"）。
    pub library_hint: String,
}

impl State {
    #[must_use]
    pub fn new(prefs_path: PathBuf) -> Self {
        let prefs = Prefs::load(&prefs_path);
        Self {
            rows: Vec::new(),
            total: 0,
            scanning: false,
            status: None,
            metrics: None,
            outputs: Vec::new(),
            sources: None,
            playlists: Vec::new(),
            config: None,
            prefs,
            prefs_path,
            selected: None,
            last_error: None,
            library_hint: "把图片或视频放进媒体目录，然后点右上角的刷新。".to_owned(),
        }
    }

    #[must_use]
    pub fn favorites(&self) -> BTreeSet<String> {
        self.prefs.favorites.iter().cloned().collect()
    }

    #[must_use]
    pub fn visible(&self, needle: &str) -> Vec<MediaItem> {
        visible_items(
            &self.rows,
            needle,
            Filter::from_index(self.prefs.filter),
            Sort::from_index(self.prefs.sort),
            &self.favorites(),
        )
    }

    #[must_use]
    pub fn item(&self, id: &str) -> Option<&MediaItem> {
        self.rows.iter().find(|item| item.id == id)
    }

    #[must_use]
    pub fn is_favorite(&self, id: &str) -> bool {
        self.prefs.favorites.iter().any(|f| f == id)
    }

    /// 切换收藏并落盘；返回切换后的状态。
    pub fn toggle_favorite(&mut self, id: &str) -> bool {
        let now = if let Some(pos) = self.prefs.favorites.iter().position(|f| f == id) {
            self.prefs.favorites.remove(pos);
            false
        } else {
            self.prefs.favorites.push(id.to_owned());
            true
        };
        if let Err(e) = self.prefs.save(&self.prefs_path) {
            eprintln!(
                "liuguang-gui: 保存界面偏好失败（{}）：{e}",
                self.prefs_path.display()
            );
        }
        now
    }

    /// 当前正在播放的媒体（取主输出通道）。
    #[must_use]
    pub fn current_media(&self) -> Option<String> {
        self.status
            .as_ref()
            .and_then(|s| s.channels.first())
            .and_then(|c| c.media.clone())
            .or_else(|| self.status.as_ref().and_then(|s| s.media.clone()))
    }

    /// 媒体库根目录（优先用 daemon 报告的来源根）。
    #[must_use]
    pub fn library_root(&self) -> Option<String> {
        if let Some(sources) = &self.sources {
            if !sources.root.is_empty() {
                return Some(sources.root.clone());
            }
        }
        self.config
            .as_ref()
            .and_then(|cfg| cfg.library.as_ref())
            .map(|lib| lib.root.display().to_string())
    }

    /// 媒体详情（选中项没有探测结果时返回 `None`）。
    #[must_use]
    pub fn selected_details(&self) -> Option<&MediaDetails> {
        self.selected
            .as_deref()
            .and_then(|id| self.item(id))
            .and_then(|item| item.details.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_without_window_fields_fall_back_to_defaults() {
        // 0.3.0-11 之前写出的 gui.json 没有 window_* 字段，读回来必须仍能打开窗口。
        let prefs: Prefs = serde_json::from_str(
            r#"{"favorites":[],"view":"grid","sort":0,"filter":0,"card_size":240,"show_details":true,"show_page":"library"}"#,
        )
        .expect("旧版偏好应能解析");
        assert_eq!(prefs.window_width, default_window_width());
        assert_eq!(prefs.window_height, default_window_height());
        assert!(!prefs.window_maximized);
    }

    #[test]
    fn prefs_window_geometry_round_trips() {
        let prefs = Prefs {
            window_width: 1720,
            window_height: 1080,
            window_maximized: true,
            ..Prefs::default()
        };
        let text = serde_json::to_string(&prefs).expect("序列化");
        let back: Prefs = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back.window_width, 1720);
        assert_eq!(back.window_height, 1080);
        assert!(back.window_maximized);
    }

    fn item(
        name: &str,
        kind: &str,
        width: u32,
        height: u32,
        fps_millihz: Option<u32>,
    ) -> MediaItem {
        MediaItem {
            id: format!("/m/{name}"),
            source_id: "images".into(),
            name: name.to_owned(),
            path: format!("/m/{name}"),
            kind: kind.to_owned(),
            size_bytes: u64::from(width) * u64::from(height),
            modified_unix: i64::from(width),
            details: Some(MediaDetails {
                width,
                height,
                fps_millihz,
                duration_ms: None,
                codec: None,
                pix_fmt: None,
            }),
        }
    }

    fn sample() -> Vec<MediaItem> {
        vec![
            item("b.png", "image", 1920, 1080, None),
            item("a.mp4", "video", 3840, 2160, Some(60_000)),
            item("c.mp4", "video", 1280, 720, Some(24_000)),
            item("d.png", "image", 7680, 4320, None),
        ]
    }

    #[test]
    fn filter_by_kind_and_resolution() {
        let favorites = BTreeSet::new();
        let images: Vec<_> = sample()
            .into_iter()
            .filter(|i| Filter::Image.matches(i, &favorites))
            .collect();
        assert_eq!(images.len(), 2);
        let uhd: Vec<_> = sample()
            .into_iter()
            .filter(|i| Filter::Uhd2160.matches(i, &favorites))
            .collect();
        assert_eq!(uhd.len(), 2, "3840 与 7680 都应命中 2160p+");
        assert!(Filter::Uhd4320.matches(&sample()[3], &favorites));
        assert!(!Filter::Uhd4320.matches(&sample()[1], &favorites));
    }

    #[test]
    fn filter_favorites_uses_set() {
        let mut favorites = BTreeSet::new();
        favorites.insert("/m/a.mp4".to_owned());
        let hit: Vec<_> = sample()
            .into_iter()
            .filter(|i| Filter::Favorite.matches(i, &favorites))
            .collect();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "a.mp4");
    }

    #[test]
    fn sort_by_name_size_and_resolution() {
        let favorites = BTreeSet::new();
        let by_name = visible_items(&sample(), "", Filter::All, Sort::Name, &favorites);
        let names: Vec<&str> = by_name.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["a.mp4", "b.png", "c.mp4", "d.png"]);

        let by_size = visible_items(&sample(), "", Filter::All, Sort::Size, &favorites);
        assert_eq!(by_size[0].name, "d.png");

        let by_res = visible_items(&sample(), "", Filter::All, Sort::Resolution, &favorites);
        assert_eq!(by_res[0].name, "d.png");
        assert_eq!(by_res[3].name, "c.mp4");
    }

    #[test]
    fn sort_by_fps_puts_unknown_last() {
        let favorites = BTreeSet::new();
        let rows = visible_items(&sample(), "", Filter::Video, Sort::Fps, &favorites);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a.mp4");
        assert_eq!(rows[1].name, "c.mp4");
    }

    #[test]
    fn search_matches_name_path_kind() {
        let favorites = BTreeSet::new();
        assert_eq!(
            visible_items(&sample(), "PNG", Filter::All, Sort::Name, &favorites).len(),
            2
        );
        assert_eq!(
            visible_items(&sample(), "video", Filter::All, Sort::Name, &favorites).len(),
            2
        );
        assert!(visible_items(&sample(), "nope", Filter::All, Sort::Name, &favorites).is_empty());
    }

    #[test]
    fn prefs_round_trip() {
        let dir = std::env::temp_dir().join(format!("liuguang-gui-prefs-{}", std::process::id()));
        let path = dir.join("gui.json");
        let mut prefs = Prefs::default();
        prefs.favorites.push("/a.png".into());
        prefs.card_size = 280;
        prefs.view = "list".into();
        prefs.save(&path).expect("写偏好");
        let back = Prefs::load(&path);
        assert_eq!(back.favorites, vec!["/a.png".to_owned()]);
        assert_eq!(back.card_size, 280);
        assert_eq!(back.view, "list");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_prefs_falls_back_to_defaults() {
        let prefs = Prefs::load(Path::new("/nonexistent/liuguang/gui.json"));
        assert_eq!(prefs.card_size, 240);
        assert_eq!(prefs.filter, 0);
    }
}
