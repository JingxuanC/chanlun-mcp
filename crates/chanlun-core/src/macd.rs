//! MACD 计算（对齐 Python `_compute_macd`）。
//!
//! EMA 用 `adjust=False` 的递归定义：
//! `ema[0] = close[0]; ema[i] = a*close[i] + (1-a)*ema[i-1]`，`a = 2/(span+1)`。
//! DIF = EMA12 - EMA26；DEA = EMA9(DIF)；HIST = DIF - DEA。

use crate::types::{Bar, Macd};

/// 计算 MACD。返回三条与输入等长的序列。
pub fn compute_macd(bars: &[Bar], fast: usize, slow: usize, signal: usize) -> Macd {
    let n = bars.len();
    let mut dif = vec![0.0_f64; n];
    let mut dea = vec![0.0_f64; n];
    let mut hist = vec![0.0_f64; n];
    if n == 0 {
        return Macd { dif, dea, hist };
    }

    let alpha_fast = 2.0 / (fast as f64 + 1.0);
    let alpha_slow = 2.0 / (slow as f64 + 1.0);
    let alpha_signal = 2.0 / (signal as f64 + 1.0);

    // pandas ewm(adjust=False) 的起点是序列第一个值
    let mut ema_fast = bars[0].close;
    let mut ema_slow = bars[0].close;
    dif[0] = ema_fast - ema_slow;
    let mut dea_prev = dif[0];
    dea[0] = dea_prev;
    hist[0] = dif[0] - dea_prev;

    for i in 1..n {
        let c = bars[i].close;
        ema_fast = alpha_fast * c + (1.0 - alpha_fast) * ema_fast;
        ema_slow = alpha_slow * c + (1.0 - alpha_slow) * ema_slow;
        dif[i] = ema_fast - ema_slow;
        dea_prev = alpha_signal * dif[i] + (1.0 - alpha_signal) * dea_prev;
        dea[i] = dea_prev;
        hist[i] = dif[i] - dea_prev;
    }

    Macd { dif, dea, hist }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: f64) -> Bar {
        Bar {
            date: 20260101,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            amount: 0.0,
        }
    }

    #[test]
    fn flat_series_has_zero_macd() {
        let bars: Vec<Bar> = (0..50).map(|_| bar(10.0)).collect();
        let m = compute_macd(&bars, 12, 26, 9);
        assert_eq!(m.dif.len(), 50);
        assert!(m.dif.iter().all(|v| v.abs() < 1e-12));
        assert!(m.hist.iter().all(|v| v.abs() < 1e-12));
    }

    #[test]
    fn ema_matches_recursive_definition() {
        // 单点校验：EMA_1 = a*x1 + (1-a)*EMA_0, a=2/(span+1)
        let bars = vec![bar(10.0), bar(20.0)];
        let m = compute_macd(&bars, 3, 3, 2);
        // fast==slow -> dif 恒为 0
        assert!(m.dif.iter().all(|v| v.abs() < 1e-12));
        // signal=2 -> a=2/3, dea[1] = 2/3*0 + 1/3*0 = 0
        assert!(m.dea.iter().all(|v| v.abs() < 1e-12));

        let m2 = compute_macd(&bars, 1, 5, 1);
        // a_fast = 1.0 -> ema_fast = close；a_slow = 1/3
        // ema_slow[1] = 20/3 + 2/3*10 = 13.3333...
        let expected_dif1 = 20.0 - (20.0 / 3.0 + (2.0 / 3.0) * 10.0);
        assert!((m2.dif[1] - expected_dif1).abs() < 1e-12);
    }
}
