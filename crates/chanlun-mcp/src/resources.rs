//! MCP Resources：把"策略口径"当成只读资源暴露给客户端，防止模型自由发挥。
//!
//! | URI | 内容 |
//! |---|---|
//! | `chanlun://strategy/doc` | v1.0 策略文档（md 全文，含口径声明/缺陷清单） |
//! | `chanlun://strategy/params` | 生产参数（附录 B 的机器可读版） |
//! | `chanlun://stats/{mode}` | 绩效统计（legacy/dynpos） |
//! | `chanlun://nav/{mode}` | 净值序列 CSV |

pub const RESOURCE_URIS: &[&str] = &[
    "chanlun://strategy/doc",
    "chanlun://strategy/params",
    "chanlun://stats/legacy",
    "chanlun://stats/dynpos",
    "chanlun://nav/legacy",
    "chanlun://nav/dynpos",
];
