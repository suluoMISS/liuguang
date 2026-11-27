//! `liuguang-core` 的错误类型。各领域错误独立定义，向上用 `Result` 传播，
//! 不在热路径使用 `unwrap()`/`expect()`。

use crate::fit::FitError;
use crate::state::StateError;
use std::path::PathBuf;

/// 配置解析与校验错误。字段级错误携带配置字段路径（如 `transition.duration_ms`）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 配置文件无法读取。
    #[error("配置文件读取失败: {path}: {source}")]
    Io {
        /// 配置文件路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: std::io::Error,
    },
    /// TOML 语法/类型错误（`toml` 的错误信息自带行列位置）。
    #[error("配置解析失败（含行列位置）: {0}")]
    Parse(#[from] toml::de::Error),
    /// 语义校验失败，字段路径 + 原因。
    #[error("配置字段 {field} 非法: {message}")]
    InvalidField {
        /// 出错字段的配置路径。
        field: String,
        /// 具体原因。
        message: String,
    },
    /// 序列化为 TOML 失败（`save_config` 用；正常配置不会发生）。
    #[error("配置序列化失败: {0}")]
    Serialize(String),
}

impl ConfigError {
    /// 构造字段级校验错误。
    pub fn invalid_field(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidField {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// `liuguang-core` 伞形错误，便于应用层统一上报。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 配置错误。
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// 状态机错误。
    #[error(transparent)]
    State(#[from] StateError),
    /// 显示适配计算错误。
    #[error(transparent)]
    Fit(#[from] FitError),
}
