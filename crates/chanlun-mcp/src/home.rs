//! 默认目录：`$CHANLUN_HOME`，未设置时用 `~/.chanlun`（不依赖当前工作目录）。

use std::path::PathBuf;

/// 返回 chanlun 的 home 目录（数据/产物/jobs 的根）。
pub fn chanlun_home() -> PathBuf {
    if let Ok(h) = std::env::var("CHANLUN_HOME") {
        return PathBuf::from(h);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".chanlun");
    }
    PathBuf::from(".chanlun")
}
