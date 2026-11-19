//! G-1 core golden 对拍：Python 参考实现 vs Rust，逐项 0 差异。
//!
//! fixture 由 `scripts/golden_core.py` 生成（Python `chanlun.py` 在确定性合成 60min
//! 序列上跑出的 merged / fractals / macd / bis / zhongshus / b1 / b2 / sell）。
//! 本测试读取同一份输入 bar 与配置，重跑 Rust 实现并逐字段比较。
//!
//! 覆盖配置（见 fixture meta.config_cases）：
//! - adjacent / pivot 两种背驰口径；
//! - break_zd_confirm = false / true（跌破即触发 vs 三卖回抽确认）；
//! - momentum_mode = area / peak（交叉组合共 6 组）。
//!
//! 浮点比较容差 1e-9（fixture 生成时已量化到 1e-12）。

use chanlun_core::{
    Analysis, AnalyzerConfig, Bar, Bi, BuySignal, Direction, Fractal, Macd, MergedKline, Zhongshu,
    analyze, latest_sell_advice,
};
use serde::Deserialize;
use std::path::PathBuf;

/// Python 与 Rust 浮点结果允许的最大绝对偏差（验收要求：量化到 1e-9 后 0 差异）。
const FLOAT_TOL: f64 = 1e-9;

#[derive(Debug, Deserialize)]
struct Fixture {
    meta: FixtureMeta,
    datasets: Vec<Dataset>,
}

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    n_bars: usize,
    config_cases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    name: String,
    bars: Vec<Bar>,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    merged: Vec<MergedKline>,
    fractals: Vec<Fractal>,
    macd: Macd,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    config: AnalyzerConfig,
    bis: Vec<Bi>,
    zhongshus: Vec<Zhongshu>,
    b1: Vec<BuySignal>,
    b2: Vec<BuySignal>,
    sell: Vec<SellCase>,
}

#[derive(Debug, Deserialize)]
struct SellCase {
    entry_raw: usize,
    entry_price: f64,
    top_divergence: bool,
    break_zd: bool,
    note: String,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("golden_core.json")
}

fn load_fixture() -> Fixture {
    let path = fixture_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "读不到 golden fixture {}（先用捆绑 python 跑 scripts/golden_core.py 生成）：{e}",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("fixture JSON 解析失败: {e}"))
}

/// 逐项比较器：累计不一致项，最后一次性报告（避免第一条失败就看不到全貌）。
#[derive(Default)]
struct Cmp {
    checks: usize,
    max_abs_diff: f64,
    diffs: Vec<String>,
}

impl Cmp {
    fn f64(&mut self, ctx: &str, got: f64, want: f64) {
        self.checks += 1;
        let d = (got - want).abs();
        if d.is_finite() {
            self.max_abs_diff = self.max_abs_diff.max(d);
        }
        if !(d <= FLOAT_TOL) {
            self.diffs
                .push(format!("{ctx}: got {got:?} want {want:?} (diff {d:e})"));
        }
    }

    fn usize(&mut self, ctx: &str, got: usize, want: usize) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got} want {want}"));
        }
    }

    fn opt_usize(&mut self, ctx: &str, got: Option<usize>, want: Option<usize>) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got:?} want {want:?}"));
        }
    }

    fn bool(&mut self, ctx: &str, got: bool, want: bool) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got} want {want}"));
        }
    }

    fn str(&mut self, ctx: &str, got: &str, want: &str) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got:?} want {want:?}"));
        }
    }

    fn direction(&mut self, ctx: &str, got: Direction, want: Direction) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got:?} want {want:?}"));
        }
    }

    fn kind(&mut self, ctx: &str, got: chanlun_core::SignalKind, want: chanlun_core::SignalKind) {
        self.checks += 1;
        if got != want {
            self.diffs.push(format!("{ctx}: got {got:?} want {want:?}"));
        }
    }
}

fn cmp_merged(c: &mut Cmp, prefix: &str, got: &[MergedKline], want: &[MergedKline]) {
    c.usize(&format!("{prefix}.len"), got.len(), want.len());
    for i in 0..got.len().min(want.len()) {
        let (g, w) = (&got[i], &want[i]);
        let ctx = format!("{prefix}[{i}]");
        c.usize(&format!("{ctx}.idx"), g.idx, w.idx);
        c.usize(&format!("{ctx}.start"), g.start, w.start);
        c.usize(&format!("{ctx}.end"), g.end, w.end);
        c.usize(
            &format!("{ctx}.date_start"),
            g.date_start as usize,
            w.date_start as usize,
        );
        c.usize(
            &format!("{ctx}.date_end"),
            g.date_end as usize,
            w.date_end as usize,
        );
        c.f64(&format!("{ctx}.open"), g.open, w.open);
        c.f64(&format!("{ctx}.high"), g.high, w.high);
        c.f64(&format!("{ctx}.low"), g.low, w.low);
        c.f64(&format!("{ctx}.close"), g.close, w.close);
    }
}

fn cmp_fractals(c: &mut Cmp, prefix: &str, got: &[Fractal], want: &[Fractal]) {
    c.usize(&format!("{prefix}.len"), got.len(), want.len());
    for i in 0..got.len().min(want.len()) {
        let (g, w) = (&got[i], &want[i]);
        let ctx = format!("{prefix}[{i}]");
        c.usize(&format!("{ctx}.idx"), g.idx, w.idx);
        c.usize(&format!("{ctx}.raw_index"), g.raw_index, w.raw_index);
        c.usize(&format!("{ctx}.date"), g.date as usize, w.date as usize);
        c.f64(&format!("{ctx}.high"), g.high, w.high);
        c.f64(&format!("{ctx}.low"), g.low, w.low);
        c.checks += 1;
        if g.kind != w.kind {
            c.diffs
                .push(format!("{ctx}.kind: got {:?} want {:?}", g.kind, w.kind));
        }
    }
}

fn cmp_macd(c: &mut Cmp, prefix: &str, got: &Macd, want: &Macd) {
    for (name, g, w) in [
        ("dif", &got.dif, &want.dif),
        ("dea", &got.dea, &want.dea),
        ("hist", &got.hist, &want.hist),
    ] {
        c.usize(&format!("{prefix}.{name}.len"), g.len(), w.len());
        for i in 0..g.len().min(w.len()) {
            c.f64(&format!("{prefix}.{name}[{i}]"), g[i], w[i]);
        }
    }
}

fn cmp_bis(c: &mut Cmp, prefix: &str, got: &[Bi], want: &[Bi]) {
    c.usize(&format!("{prefix}.len"), got.len(), want.len());
    for i in 0..got.len().min(want.len()) {
        let (g, w) = (&got[i], &want[i]);
        let ctx = format!("{prefix}[{i}]");
        c.usize(&format!("{ctx}.start_idx"), g.start_idx, w.start_idx);
        c.usize(&format!("{ctx}.end_idx"), g.end_idx, w.end_idx);
        c.usize(&format!("{ctx}.start_raw"), g.start_raw, w.start_raw);
        c.usize(&format!("{ctx}.end_raw"), g.end_raw, w.end_raw);
        c.usize(
            &format!("{ctx}.start_date"),
            g.start_date as usize,
            w.start_date as usize,
        );
        c.usize(
            &format!("{ctx}.end_date"),
            g.end_date as usize,
            w.end_date as usize,
        );
        c.f64(&format!("{ctx}.start_price"), g.start_price, w.start_price);
        c.f64(&format!("{ctx}.end_price"), g.end_price, w.end_price);
        c.direction(&format!("{ctx}.direction"), g.direction, w.direction);
        c.f64(&format!("{ctx}.momentum"), g.momentum, w.momentum);
        c.bool(
            &format!("{ctx}.is_confirmed"),
            g.is_confirmed,
            w.is_confirmed,
        );
    }
}

fn cmp_zhongshus(c: &mut Cmp, prefix: &str, got: &[Zhongshu], want: &[Zhongshu]) {
    c.usize(&format!("{prefix}.len"), got.len(), want.len());
    for i in 0..got.len().min(want.len()) {
        let (g, w) = (&got[i], &want[i]);
        let ctx = format!("{prefix}[{i}]");
        c.usize(&format!("{ctx}.start_idx"), g.start_idx, w.start_idx);
        c.usize(&format!("{ctx}.end_idx"), g.end_idx, w.end_idx);
        c.usize(&format!("{ctx}.start_raw"), g.start_raw, w.start_raw);
        c.usize(&format!("{ctx}.end_raw"), g.end_raw, w.end_raw);
        c.usize(
            &format!("{ctx}.start_date"),
            g.start_date as usize,
            w.start_date as usize,
        );
        c.usize(
            &format!("{ctx}.end_date"),
            g.end_date as usize,
            w.end_date as usize,
        );
        c.f64(&format!("{ctx}.zg"), g.zg, w.zg);
        c.f64(&format!("{ctx}.zd"), g.zd, w.zd);
    }
}

fn cmp_signals(c: &mut Cmp, prefix: &str, got: &[BuySignal], want: &[BuySignal]) {
    c.usize(&format!("{prefix}.len"), got.len(), want.len());
    for i in 0..got.len().min(want.len()) {
        let (g, w) = (&got[i], &want[i]);
        let ctx = format!("{prefix}[{i}]");
        c.kind(&format!("{ctx}.kind"), g.kind, w.kind);
        c.usize(&format!("{ctx}.bi_idx"), g.bi_idx, w.bi_idx);
        c.usize(&format!("{ctx}.raw_index"), g.raw_index, w.raw_index);
        c.usize(&format!("{ctx}.date"), g.date as usize, w.date as usize);
        c.f64(&format!("{ctx}.price"), g.price, w.price);
        c.f64(&format!("{ctx}.low"), g.low, w.low);
        c.opt_usize(
            &format!("{ctx}.first_buy_bi_idx"),
            g.first_buy_bi_idx,
            w.first_buy_bi_idx,
        );
        c.opt_usize(
            &format!("{ctx}.zhongshu_idx"),
            g.zhongshu_idx,
            w.zhongshu_idx,
        );
    }
}

fn cmp_sell(c: &mut Cmp, prefix: &str, got: &chanlun_core::SellAdvice, want: &SellCase) {
    c.bool(
        &format!("{prefix}(entry_raw={}).top_divergence", want.entry_raw),
        got.top_divergence,
        want.top_divergence,
    );
    c.bool(
        &format!("{prefix}(entry_raw={}).break_zd", want.entry_raw),
        got.break_zd,
        want.break_zd,
    );
    c.str(
        &format!("{prefix}(entry_raw={}).note", want.entry_raw),
        &got.note,
        &want.note,
    );
}

#[test]
fn golden_core_python_parity() {
    let fx = load_fixture();
    assert_eq!(
        fx.datasets.len(),
        3,
        "验收要求至少 3 组不同走势，fixture 实际 {}",
        fx.datasets.len()
    );
    // 验收要求：至少覆盖 adjacent/pivot × break_zd_confirm 0/1。
    for required in [
        "adjacent_area_noconfirm",
        "pivot_area_noconfirm",
        "adjacent_area_confirm",
        "pivot_area_confirm",
    ] {
        assert!(
            fx.meta.config_cases.iter().any(|n| n == required),
            "fixture 缺少配置组合 {required}; 实际 = {:?}",
            fx.meta.config_cases
        );
    }

    let mut c = Cmp::default();
    let mut case_count = 0usize;
    for ds in &fx.datasets {
        assert_eq!(
            ds.bars.len(),
            fx.meta.n_bars,
            "{}: bars 数与 meta.n_bars 不符",
            ds.name
        );
        for case in &ds.expected.cases {
            case_count += 1;
            let prefix = format!("{}/{}", ds.name, case.name);
            let analysis: Analysis = analyze(&ds.bars, &case.config)
                .unwrap_or_else(|e| panic!("{prefix}: Rust analyze 失败: {e}"));
            cmp_merged(
                &mut c,
                &format!("{prefix}/merged"),
                &analysis.merged,
                &ds.expected.merged,
            );
            cmp_fractals(
                &mut c,
                &format!("{prefix}/fractals"),
                &analysis.fractals,
                &ds.expected.fractals,
            );
            cmp_macd(
                &mut c,
                &format!("{prefix}/macd"),
                &analysis.macd,
                &ds.expected.macd,
            );
            cmp_bis(&mut c, &format!("{prefix}/bis"), &analysis.bis, &case.bis);
            cmp_zhongshus(
                &mut c,
                &format!("{prefix}/zhongshus"),
                &analysis.zhongshus,
                &case.zhongshus,
            );
            cmp_signals(&mut c, &format!("{prefix}/b1"), &analysis.b1, &case.b1);
            cmp_signals(&mut c, &format!("{prefix}/b2"), &analysis.b2, &case.b2);
            for sc in &case.sell {
                let adv = latest_sell_advice(
                    &analysis,
                    sc.entry_price,
                    sc.entry_raw,
                    ds.bars.len(),
                    &case.config,
                );
                cmp_sell(&mut c, &format!("{prefix}/sell"), &adv, sc);
            }
        }
    }

    println!(
        "golden: datasets={} cases={} checks={} max_abs_diff={:e}",
        fx.datasets.len(),
        case_count,
        c.checks,
        c.max_abs_diff
    );
    if !c.diffs.is_empty() {
        let shown = c
            .diffs
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ");
        panic!(
            "golden 对拍失败：{} 项不一致 / 共 {} 项检查（仅列前 30 条）\n  {}",
            c.diffs.len(),
            c.checks,
            shown
        );
    }
}
