//! 冲击成本滑点模型（对齐 Python `slippage_for`，2026-11-17 重校准：base 5 万）。
//!
//! `amt <= base` -> 0；`amt >= cap` -> max_slip；中间线性插值。
//! 买卖双边、按实际成交金额、迭代 2 轮收敛（迭代逻辑在引擎撮合里）。

/// 单笔金额对应的滑点比例。
pub fn slippage_for(amt: f64, base_amt: f64, cap_amt: f64, max_slip: f64) -> f64 {
    if amt <= base_amt || max_slip <= 0.0 {
        return 0.0;
    }
    if amt >= cap_amt {
        return max_slip;
    }
    (amt - base_amt) / (cap_amt - base_amt) * max_slip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_below_base() {
        assert_eq!(slippage_for(49_999.0, 50_000.0, 2_000_000.0, 0.01), 0.0);
        assert_eq!(slippage_for(50_000.0, 50_000.0, 2_000_000.0, 0.01), 0.0);
    }

    #[test]
    fn linear_between_base_and_cap() {
        let s = slippage_for(1_025_000.0, 50_000.0, 2_000_000.0, 0.01);
        assert!((s - 0.005).abs() < 1e-12);
    }

    #[test]
    fn capped_at_max() {
        assert_eq!(slippage_for(2_000_000.0, 50_000.0, 2_000_000.0, 0.01), 0.01);
        assert_eq!(slippage_for(9_999_999.0, 50_000.0, 2_000_000.0, 0.01), 0.01);
    }
}
