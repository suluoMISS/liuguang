//! 运行时偏好的**写回**（0.2.0-6）：改过的设置活过重启。
//!
//! ## 为什么需要这个模块
//!
//! `state.json` 只记"当前在放什么"（媒体、`pinned`、这次的 fit/zoom/offset）；
//! 而**用户偏好**（转场效果与时长、输出旋转）原本只活在内存里：
//! `handle_transition` 改的是 `self.transitions`，`handle_set_output_transform` 的
//! `rotation` 只写进 `self.config`——重启后一律回到 `config.toml` 的旧值。
//! 真机复现过："GUI 里把转场改成淡入淡出、重启服务，又变回滑动"。
//!
//! ## 为什么用 `toml_edit` 而不是 `serde` 重写整份配置
//!
//! `config.toml` 是**人写给人看**的文件：里面有注释、空行、字段顺序、以及本程序
//! 并不认识的键（用户自己留的备注）。用 `serde` 序列化整份 `Config` 会把它们全
//! 抹掉——那比"设置丢了"更糟，用户会以为配置文件被谁改坏了。`toml_edit` 保留
//! 原始格式，只动我们真正要改的那几个键。
//!
//! ## 口径（写什么、不写什么）
//!
//! - **只写用户显式改过的键**：`[outputs."<名字>"]` 的 `fit`/`zoom`/`offset_x`/
//!   `offset_y`/`rotation`；`[transition]` 的 `effect`/`duration_ms`/`easing`/`direction`。
//! - **不写** `enabled`、`[media]`、`[library]`、`[slideshow]` 等本次没动过的段：
//!   那些改动走显式的 `save_config`（它可以拒绝写入并解释原因）。
//! - **写失败不砸运行**：只读文件、属主不对、磁盘满 ⇒ 返回错误，调用方打一条
//!   WARN 就继续跑；本次运行的效果不受影响（内存里已经生效）。

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

use liuguang_core::config::{Config, TransitionDirection, TransitionEffect};
use liuguang_core::transform::Rotation;

/// 哪一类偏好需要写回。
///
/// 用 `BTreeSet<Preference>` 收集，是为了让"拖 SpinButton 十几下"最终只写一次：
/// 同一输出的 transform 合并成一项，重复的 set 自然去重。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Preference {
    /// 某个输出段的 `fit`/`zoom`/`offset_x`/`offset_y`/`rotation`。
    OutputTransform {
        /// 输出名（即 `config.toml` 里 `[outputs."<这个名字>"]`）。
        output: String,
    },
    /// `[transition]` 段。
    Transitions,
}

/// 写回失败的原因（调用方只用来打日志，不区分到用户文案）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigStoreError {
    /// 读不到现有配置（文件不存在时由调用方决定是否忽略）。
    #[error("读取配置失败: {0}")]
    Read(#[source] std::io::Error),
    /// 现有配置不是合法 TOML —— 拒绝覆盖（别人的文件不该被我们"修好"）。
    #[error("现有配置不是合法 TOML，拒绝写回: {0}")]
    Parse(String),
    /// 结构不符（例如 `outputs` 存在但不是表）—— 同样拒绝覆盖。
    #[error("配置结构不符合预期: {0}")]
    Shape(String),
    /// 原子写失败（临时文件或 rename）。
    #[error("写入配置失败: {0}")]
    Write(#[source] std::io::Error),
}

/// 把 `prefs` 指定的键从 `config` 写进 `path`，**其余内容原样保留**。
///
/// 语义是"覆盖式修改"而不是"合并"：这些键的权威值就是内存里的 `config`
/// （用户刚刚通过 IPC 改过它）。
pub fn persist(
    path: &Path,
    config: &Config,
    prefs: &BTreeSet<Preference>,
) -> Result<(), ConfigStoreError> {
    if prefs.is_empty() {
        return Ok(());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            String::new()
        }
        Err(err) => return Err(ConfigStoreError::Read(err)),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigStoreError::Parse(e.to_string()))?;

    for pref in prefs {
        match pref {
            Preference::OutputTransform { output } => {
                write_output_transform(&mut doc, output, config)?;
            }
            Preference::Transitions => write_transitions(&mut doc, config)?,
        }
    }

    write_atomic(path, &doc.to_string())
}

/// 写 `[outputs."<output>"]` 段。
///
/// 段不存在就创建（用户可能从没为这个输出写过配置）；键已存在时**保留原有行内
/// 注释**（`zoom = 1.0  # (0, 10]` 里的注释不该因为一次改值而消失）。
fn write_output_transform(
    doc: &mut toml_edit::DocumentMut,
    output: &str,
    config: &Config,
) -> Result<(), ConfigStoreError> {
    let cfg = config.outputs.get(output);

    let outputs = doc
        .entry("outputs")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let outputs = outputs
        .as_table_mut()
        .ok_or_else(|| ConfigStoreError::Shape("[outputs] 不是表".to_string()))?;
    // `[outputs."eDP-1"]` 这种带点/带引号的键，用 `entry` 建出来就是"表"。
    let entry = outputs
        .entry(output)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = entry.as_table_mut().ok_or_else(|| {
        ConfigStoreError::Shape(format!("[outputs.\"{output}\"] 不是表（可能是内联写法）"))
    })?;

    let Some(cfg) = cfg else {
        // 内存里没有这个输出（已被移除）：保留文件原样，不要凭空写默认值。
        return Ok(());
    };
    set_str(table, "fit", cfg.fit.as_str());
    set_float(table, "zoom", cfg.zoom);
    set_float(table, "offset_x", cfg.offset_x);
    set_float(table, "offset_y", cfg.offset_y);
    set_str(table, "rotation", rotation_name(cfg.rotation));
    set_float(table, "brightness", cfg.brightness);
    Ok(())
}

/// 写 `[transition]` 段（注意配置键是**单数** `transition`）。
///
/// `enabled` 不动：那是用户开关转场总阀，不是本次 IPC 改的东西。
fn write_transitions(
    doc: &mut toml_edit::DocumentMut,
    config: &Config,
) -> Result<(), ConfigStoreError> {
    let transition = &config.transition;
    let item = doc
        .entry("transition")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = item
        .as_table_mut()
        .ok_or_else(|| ConfigStoreError::Shape("[transition] 不是表".to_string()))?;

    set_str(table, "effect", transition_effect_name(transition.effect));
    set_int(table, "duration_ms", i64::from(transition.duration_ms));
    set_str(table, "easing", easing_name(transition.easing));
    match transition.direction {
        Some(direction) => set_str(table, "direction", direction_name(direction)),
        // `direction` 是可选的：本次显式清空时把它删掉，而不是写个空字符串
        // （空字符串在反序列化时会直接报错，把用户的下一次启动弄崩）。
        None => {
            table.remove("direction");
        }
    }
    Ok(())
}

/// `Rotation` → 配置里的字符串（`"0" | "90" | "180" | "270"`）。
fn rotation_name(rotation: Rotation) -> &'static str {
    match rotation {
        Rotation::Deg0 => "0",
        Rotation::Deg90 => "90",
        Rotation::Deg180 => "180",
        Rotation::Deg270 => "270",
    }
}

/// `TransitionEffect` → 配置里的名字（与 serde rename 保持一致）。
fn transition_effect_name(effect: TransitionEffect) -> &'static str {
    match effect {
        TransitionEffect::Fade => "fade",
        TransitionEffect::Slide => "slide",
        TransitionEffect::Wipe => "wipe",
        TransitionEffect::Circle => "circle",
        TransitionEffect::Random => "random",
        TransitionEffect::None => "none",
    }
}

/// `Easing` → 配置里的名字。
fn easing_name(easing: liuguang_core::config::Easing) -> &'static str {
    match easing {
        liuguang_core::config::Easing::Linear => "linear",
        liuguang_core::config::Easing::EaseIn => "ease-in",
        liuguang_core::config::Easing::EaseOut => "ease-out",
        liuguang_core::config::Easing::EaseInOut => "ease-in-out",
    }
}

/// `TransitionDirection` → 配置里的名字。
fn direction_name(direction: TransitionDirection) -> &'static str {
    match direction {
        TransitionDirection::Left => "left",
        TransitionDirection::Right => "right",
        TransitionDirection::Up => "up",
        TransitionDirection::Down => "down",
    }
}

/// 把一个字符串值写进表，**尽量保留原行内注释与缩进**。
fn set_str(table: &mut toml_edit::Table, key: &str, value: &str) {
    replace_value(table, key, toml_edit::Value::from(value));
}

/// 浮点值同上。
fn set_float(table: &mut toml_edit::Table, key: &str, value: f64) {
    replace_value(table, key, toml_edit::Value::from(value));
}

/// 整数值同上。
fn set_int(table: &mut toml_edit::Table, key: &str, value: i64) {
    replace_value(table, key, toml_edit::Value::from(value));
}

/// 就地替换值；已有值则**沿用它的 decor**（前缀缩进 + 行尾注释）。
fn replace_value(table: &mut toml_edit::Table, key: &str, mut value: toml_edit::Value) {
    if let Some(item) = table.get_mut(key) {
        if let Some(existing) = item.as_value() {
            *value.decor_mut() = existing.decor().clone();
        }
    }
    table.insert(key, toml_edit::Item::Value(value));
}

/// 原子写：同目录临时文件 → `rename`。
///
/// 与 `save_state_file` 同一套做法，理由也一样：**半个文件比旧文件更糟**——
/// 断电/写满时用户至少还能启动。
fn write_atomic(path: &Path, contents: &str) -> Result<(), ConfigStoreError> {
    let tmp = path.with_extension("toml.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(ConfigStoreError::Write)?;
        file.write_all(contents.as_bytes())
            .map_err(ConfigStoreError::Write)?;
        // 落盘顺序：先把内容刷到盘上，再让目录项改名指向它。
        file.sync_all().map_err(ConfigStoreError::Write)?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ConfigStoreError::Write(e)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 每次用独立临时目录，避免测试之间互相影响（不引第三方 crate）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "liuguang-config-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    const BASE: &str = r#"# 顶部注释：不该被删
[display]
backend = "auto"   # 行内注释

[transition]
enabled = true
effect = "slide"      # 我喜欢的
duration_ms = 350
easing = "ease-in-out"

[outputs."eDP-1"]
fit = "cover"
zoom = 1.0          # (0, 10]
offset_x = 0.0
offset_y = 0.0
rotation = "0"

# 用户自己留的未知段
[my_notes]
remember = "别删我"
"#;

    #[test]
    fn writes_only_requested_keys_and_keeps_comments() {
        let dir = temp_dir("keep");
        let path = write_config(&dir, BASE);

        let mut config = Config::default();
        let mut out = config.outputs.get("eDP-1").cloned().unwrap_or_default();
        out.fit = liuguang_core::config::FitMode::Contain;
        out.zoom = 1.25;
        out.rotation = Rotation::Deg90;
        config.outputs.insert("eDP-1".to_string(), out);
        config.transition.effect = TransitionEffect::Fade;
        config.transition.duration_ms = 800;

        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::OutputTransform {
            output: "eDP-1".to_string(),
        });
        prefs.insert(Preference::Transitions);
        persist(&path, &config, &prefs).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // 注释与未知段都还在
        assert!(text.contains("# 顶部注释：不该被删"), "{text}");
        assert!(text.contains("# 行内注释"), "{text}");
        assert!(text.contains("# (0, 10]"), "行内注释应保留: {text}");
        assert!(
            text.contains("[my_notes]") && text.contains("别删我"),
            "{text}"
        );
        // 改过的键是新值
        assert!(text.contains("fit = \"contain\""), "{text}");
        assert!(text.contains("zoom = 1.25"), "{text}");
        assert!(text.contains("rotation = \"90\""), "{text}");
        assert!(text.contains("effect = \"fade\""), "{text}");
        assert!(text.contains("duration_ms = 800"), "{text}");
        // 没动的键保持原值
        assert!(text.contains("backend = \"auto\""), "{text}");
        assert!(text.contains("easing = \"ease-in-out\""), "{text}");
    }

    #[test]
    fn written_file_still_loads() {
        let dir = temp_dir("reload");
        // 不带未知段（`Config` 是 `deny_unknown_fields`，未知顶层段本来就读不进来）
        let path = write_config(&dir, BASE.split("# 用户自己留的未知段").next().unwrap());

        let mut config = Config::default();
        let mut out = config.outputs.get("eDP-1").cloned().unwrap_or_default();
        out.fit = liuguang_core::config::FitMode::Contain;
        out.rotation = Rotation::Deg270;
        config.outputs.insert("eDP-1".to_string(), out);
        config.transition.effect = TransitionEffect::Wipe;
        config.transition.duration_ms = 900;
        config.transition.direction = Some(TransitionDirection::Left);

        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::OutputTransform {
            output: "eDP-1".to_string(),
        });
        prefs.insert(Preference::Transitions);
        persist(&path, &config, &prefs).unwrap();

        let parsed = Config::load(&path).unwrap();
        assert_eq!(parsed.outputs["eDP-1"].rotation, Rotation::Deg270);
        assert_eq!(parsed.outputs["eDP-1"].fit.as_str(), "contain");
        assert_eq!(parsed.transition.duration_ms, 900);
        assert_eq!(parsed.transition.effect, TransitionEffect::Wipe);
        assert_eq!(parsed.transition.direction, Some(TransitionDirection::Left));
    }

    #[test]
    fn creates_missing_output_section() {
        let dir = temp_dir("create");
        let path = write_config(&dir, "[display]\nbackend = \"auto\"\n");

        let mut config = Config::default();
        config.outputs.insert(
            "HDMI-A-1".to_string(),
            liuguang_core::config::OutputConfig::default(),
        );
        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::OutputTransform {
            output: "HDMI-A-1".to_string(),
        });
        persist(&path, &config, &prefs).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[outputs"), "{text}");
        let parsed = Config::load(&path).unwrap();
        assert!(parsed.outputs.contains_key("HDMI-A-1"), "{text}");
    }

    #[test]
    fn clears_direction_when_none() {
        let dir = temp_dir("direction");
        let path = write_config(
            &dir,
            "[transition]\nenabled = true\neffect = \"slide\"\nduration_ms = 300\neasing = \"linear\"\ndirection = \"left\"\n",
        );
        let mut config = Config::default();
        config.transition.effect = TransitionEffect::Slide;
        config.transition.direction = None;
        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::Transitions);
        persist(&path, &config, &prefs).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("direction"), "direction 应被删除: {text}");
        // 清空后仍然能解析（写空字符串会炸）
        Config::load(&path).unwrap();
    }

    #[test]
    fn rejects_invalid_toml_without_touching_the_file() {
        let dir = temp_dir("invalid");
        let path = write_config(&dir, "这不是 TOML [[[\n");
        let before = std::fs::read_to_string(&path).unwrap();

        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::Transitions);
        let err = persist(&path, &Config::default(), &prefs).unwrap_err();
        assert!(matches!(err, ConfigStoreError::Parse(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn empty_preferences_is_a_noop() {
        let dir = temp_dir("noop");
        let path = write_config(&dir, BASE);
        persist(&path, &Config::default(), &BTreeSet::new()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BASE);
    }

    #[test]
    fn missing_file_bootstraps_and_directory_reports_read_error() {
        let dir = temp_dir("missing");
        let path = dir.join("nope.toml");
        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::Transitions);
        persist(&path, &Config::default(), &prefs).unwrap();
        assert!(path.exists(), "缺失文件应自动按空配置初始化并落盘");

        let dir_as_cfg = dir.join("is_dir.toml");
        std::fs::create_dir_all(&dir_as_cfg).unwrap();
        let err = persist(&dir_as_cfg, &Config::default(), &prefs).unwrap_err();
        assert!(matches!(err, ConfigStoreError::Read(_)), "{err:?}");
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = temp_dir("tmp");
        let path = write_config(&dir, BASE);
        let mut prefs = BTreeSet::new();
        prefs.insert(Preference::Transitions);
        persist(&path, &Config::default(), &prefs).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不得留下临时文件: {leftovers:?}");
    }
}
