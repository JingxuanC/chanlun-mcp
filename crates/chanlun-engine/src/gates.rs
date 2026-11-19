//! 交易执行闸门（对齐 Python 的 P1 可成交性 / P2 相对流动性）。
//!
//! - 涨停幅按板块：主板 10%、创业板/科创板 20%、北交所 30%、ST 5%；
//! - 一字板：`O == H == L`（容差 1e-6）；
//! - 涨停开盘：`open / prev_close - 1 >= 涨幅 - 0.2%` -> 买不进；
//! - 相对流动性：单笔金额 > 执行 bar 成交额 × 5% -> 不成交。
//!
//! ST 标记需要 point-in-time 表；生产版由 `st_flags.json` 提供，缺失时按非 ST 处理
//! （与 Python 一致）。

use chanlun_core::Bar;

/// 按板块与 ST 状态返回当日涨停幅（小数）。
pub fn board_limit(code: &str, is_st: bool) -> f64 {
    if is_st {
        return 0.05;
    }
    if code.starts_with("300")
        || code.starts_with("301")
        || code.starts_with("688")
        || code.starts_with("689")
    {
        return 0.20;
    }
    if code.starts_with("83") || code.starts_with("87") || code.starts_with("43") {
        return 0.30;
    }
    0.10
}

/// 是否一字板（O == H == L）。
pub fn is_one_word(bar: &Bar) -> bool {
    let max = bar.open.max(bar.high).max(bar.low);
    let min = bar.open.min(bar.high).min(bar.low);
    (max - min) <= 1e-6 * 1.0_f64.max(bar.open.abs())
}

/// 是否"开盘即涨停"（买不进）。
pub fn is_open_limit_up(bar: &Bar, prev_close: f64, limit: f64) -> bool {
    if prev_close <= 0.0 {
        return false;
    }
    let gap = bar.open / prev_close - 1.0;
    gap >= limit - 0.002
}

/// 相对流动性闸门：单笔金额是否超过执行 bar 成交额的 `pct`。
pub fn exceeds_relative_liquidity(notional: f64, bar_amount: f64, pct: f64) -> bool {
    bar_amount > 0.0 && notional > pct * bar_amount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(o: f64, h: f64, l: f64) -> Bar {
        Bar {
            date: 20261116,
            open: o,
            high: h,
            low: l,
            close: l,
            volume: 1.0,
            amount: 0.0,
        }
    }

    #[test]
    fn limits_by_board() {
        assert_eq!(board_limit("600000", false), 0.10);
        assert_eq!(board_limit("300750", false), 0.20);
        assert_eq!(board_limit("688981", false), 0.20);
        assert_eq!(board_limit("830799", false), 0.30);
        assert_eq!(board_limit("600000", true), 0.05);
    }

    #[test]
    fn one_word_detection() {
        assert!(is_one_word(&bar(10.0, 10.0, 10.0)));
        assert!(!is_one_word(&bar(10.0, 10.3, 9.9)));
    }

    #[test]
    fn open_limit_up_detection() {
        assert!(is_open_limit_up(&bar(11.0, 11.2, 10.9), 10.0, 0.10));
        assert!(!is_open_limit_up(&bar(10.5, 11.0, 10.4), 10.0, 0.10));
    }
}
