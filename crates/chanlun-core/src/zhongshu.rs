//! 中枢识别（对齐 Python `find_zhongshus`）。
//!
//! 算法（`code/chanlun.py:360-429`）：
//! - 从笔 `i` 起，用 `i`、`i+1` 两笔取重叠区间：`ZG = min(h_i, h_{i+1})`、`ZD = max(l_i, l_{i+1})`；
//!   `ZG <= ZD` 说明无重叠 -> `i += 1` 重来。
//! - 后续笔 `j`：若"完整跌破"（`l_j < ZD && h_j < ZD`）或"完整升破"（`h_j > ZG && l_j > ZG`）则中枢结束；
//!   若有重叠（`!(l_j > ZG || h_j < ZD)`）则纳入并 `ZG=min(ZG, h_j)`、`ZD=max(ZD, l_j)`；
//!   其它情况结束。
//! - 纳入笔数 >= `min_segments_in_zhongshu`(3) 才记录；记录后 `i = j`（离开段/结束笔之后重新找）。
//! - 笔极值：`h = max(start_price, end_price)`、`l = min(start_price, end_price)`。
//!
//! 注意：Python 的 `i + 2 >= n` 提前 break 意味着"剩余不足 3 笔"时直接结束整个扫描
//! （不是 `i += 1` 继续），这里保持一致。

use crate::types::{AnalyzerConfig, Bi, Zhongshu};

/// 由连续笔的重叠区间识别中枢。
pub fn find_zhongshus(bis: &[Bi], cfg: &AnalyzerConfig) -> Vec<Zhongshu> {
    let mut zhongshus: Vec<Zhongshu> = Vec::new();
    let n = bis.len();
    let mut i = 0usize;
    while i < n {
        if i + 2 >= n {
            // 至少需要 3 笔才能构成一个中枢；剩余不足直接结束（与 Python 一致）。
            break;
        }
        let mut zs_bis: Vec<usize> = vec![i];
        // 用第 i、i+1 两笔重叠区间作为初始 [ZD, ZG]。
        let h0 = bi_high(&bis[i]);
        let l0 = bi_low(&bis[i]);
        let mut j = i + 1;
        let h1 = bi_high(&bis[j]);
        let l1 = bi_low(&bis[j]);
        let zp_high = h0.min(h1);
        let zp_low = l0.max(l1);
        if zp_high <= zp_low {
            // 前两笔无重叠，无法起中枢。
            i += 1;
            continue;
        }
        zs_bis.push(j);
        let mut min_high = zp_high;
        let mut max_low = zp_low;
        j += 1;
        while j < n {
            let hj = bi_high(&bis[j]);
            let lj = bi_low(&bis[j]);
            let cur_zg = min_high;
            let cur_zd = max_low;
            // 笔完整跌破 ZD -> 下跌破位（第三类卖点），中枢结束。
            if lj < cur_zd && hj < cur_zd {
                break;
            }
            // 笔完整升破 ZG -> 上涨破位（第三类买点），中枢结束。
            if hj > cur_zg && lj > cur_zg {
                break;
            }
            // 笔与中枢区间有重叠 -> 延伸。
            if !(lj > cur_zg || hj < cur_zd) {
                min_high = min_high.min(hj);
                max_low = max_low.max(lj);
                zs_bis.push(j);
                j += 1;
                continue;
            }
            // 其他情况（部分重叠界定模糊）不纳入，直接结束。
            break;
        }
        if zs_bis.len() >= cfg.min_segments_in_zhongshu {
            let zs_start = zs_bis[0];
            let zs_end = *zs_bis.last().expect("zs_bis 非空");
            zhongshus.push(Zhongshu {
                start_idx: zs_start,
                end_idx: zs_end,
                start_raw: bis[zs_start].start_raw,
                end_raw: bis[zs_end].end_raw,
                start_date: bis[zs_start].start_date,
                end_date: bis[zs_end].end_date,
                zg: min_high,
                zd: max_low,
            });
            // 从中枢结束后的下一笔（离开段/普通结束笔）开始寻找下一个中枢。
            i = j;
        } else {
            i += 1;
        }
    }
    zhongshus
}

/// 一笔的价格极值：高点 = max(start_price, end_price)，低点 = min(start_price, end_price)。
#[inline]
pub fn bi_high(bi: &Bi) -> f64 {
    bi.start_price.max(bi.end_price)
}

#[inline]
pub fn bi_low(bi: &Bi) -> f64 {
    bi.start_price.min(bi.end_price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;

    fn bi(a: f64, b: f64) -> Bi {
        let direction = if b < a {
            Direction::Down
        } else {
            Direction::Up
        };
        Bi {
            start_idx: 0,
            end_idx: 1,
            start_raw: 0,
            end_raw: 1,
            start_date: 20240101,
            end_date: 20240102,
            start_price: a,
            end_price: b,
            direction,
            momentum: 0.0,
            is_confirmed: true,
        }
    }

    #[test]
    fn three_overlapping_bis_form_zhongshu() {
        // 三笔都在 [10, 12] 内重叠：10->12, 12->10.5, 10.5->11.8
        let bis = vec![bi(10.0, 12.0), bi(12.0, 10.5), bi(10.5, 11.8)];
        let cfg = AnalyzerConfig::default();
        let zs = find_zhongshus(&bis, &cfg);
        assert_eq!(zs.len(), 1);
        assert_eq!(zs[0].start_idx, 0);
        assert_eq!(zs[0].end_idx, 2);
        assert!((zs[0].zg - 11.8).abs() < 1e-12); // min(12, 12, 11.8)
        assert!((zs[0].zd - 10.5).abs() < 1e-12); // max(10, 10.5, 10.5)
    }

    #[test]
    fn fewer_than_three_bis_gives_none() {
        let bis = vec![bi(10.0, 12.0), bi(12.0, 10.5)];
        let cfg = AnalyzerConfig::default();
        assert!(find_zhongshus(&bis, &cfg).is_empty());
    }
}
