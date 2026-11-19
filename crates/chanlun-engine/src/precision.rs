//! 数值精度：复刻 numpy float32 的 pairwise summation + pandas `Series.mean()`。
//!
//! # 为什么需要
//!
//! Python 参考实现把 OHLCV `downcast` 成 float32；MA13 用 `df["close"].mean()`，
//! numpy 对 float32 走 pairwise 求和（8 个累加器），与"转 f64 再平均"在 1 ulp
//! 量级上不同。在 close 恰好与均线相差 1 ulp 的边界 bar 上，这会翻转 `<` 判定，
//! 改变卖出日 —— 实测 600158 在 2021-07-16：
//!
//! - `close`            = 11.762499809265137
//! - numpy f32 mean     = 11.762500762939453  -> close < ma13 = true（Python 卖出）
//! - f64 平均再转 f32   = 11.762499809265137  -> close < ma13 = false（Rust 曾因此晚卖一天）
//!
//! G-2 逐笔对拍要求完全一致，所以这里按 numpy 的算法逐位复刻。
//!
//! 算法（numpy `loops.c.src::pairwise_sum_FLOAT`）：
//! - `n < 8`：顺序累加；
//! - `8 <= n <= 128`：前 8 个值装入 r[0..8]，之后每 8 个一组累加进 r；先按
//!   `((r0+r1)+(r2+r3))+((r4+r5)+(r6+r7))` 归并，再顺序加尾部余数；
//! - 平均：`sum / n`（float32 除法，与 numpy `_mean` 一致）。

/// 复刻 numpy float32 的 pairwise 求和。
pub fn sum_f32_numpy(vals: &[f32]) -> f32 {
    let n = vals.len();
    if n == 0 {
        return 0.0;
    }
    if n < 8 {
        let mut r = 0.0f32;
        for &x in vals {
            r += x;
        }
        return r;
    }

    let mut r = [
        vals[0], vals[1], vals[2], vals[3], vals[4], vals[5], vals[6], vals[7],
    ];
    let mut i = 8usize;
    let limit = n - (n % 8);
    while i < limit {
        for k in 0..8 {
            r[k] += vals[i + k];
        }
        i += 8;
    }
    let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
    while i < n {
        res += vals[i];
        i += 1;
    }
    res
}

/// 复刻 numpy float32 `mean`（pandas float32 Series 的 `.mean()`）。
pub fn mean_f32_numpy(vals: &[f32]) -> f32 {
    if vals.is_empty() {
        return 0.0;
    }
    sum_f32_numpy(vals) / vals.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 600158 在 2021-07-16 15:00 的 13 根 close（f32 位精确值）。
    const MA13_600158_20210716: [f32; 13] = [
        11.822_056_770,
        11.663_237_572,
        11.712_868_690,
        11.712_868_690,
        11.712_868_690,
        11.653_311_729,
        11.931_243_896,
        11.861_761_093,
        11.911_392_212,
        11.683_090_210,
        11.772_425_652,
        11.712_868_690,
        11.762_499_809,
    ];

    #[test]
    fn matches_numpy_float32_mean_on_boundary_case() {
        let close = MA13_600158_20210716[12];
        let ma13 = mean_f32_numpy(&MA13_600158_20210716);
        // numpy: np.mean(np.float32 array) = 11.762500762939453
        assert_eq!(ma13, 11.762_500_762_939_453_f32);
        // Python 判定：close < ma13 -> 卖出
        assert!(close < ma13);
        // 若用 f64 平均再转 f32，会得到 11.762499809265137（= close），判定翻转
        let f64_mean = MA13_600158_20210716
            .iter()
            .map(|&x| x as f64)
            .sum::<f64>()
            / 13.0;
        assert_eq!(f64_mean as f32, close);
    }

    #[test]
    fn short_arrays_are_sequential() {
        let v = [1.0f32, 2.0, 3.0];
        assert_eq!(sum_f32_numpy(&v), 6.0);
        assert_eq!(mean_f32_numpy(&v), 2.0);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(sum_f32_numpy(&[]), 0.0);
        assert_eq!(mean_f32_numpy(&[]), 0.0);
    }
}
