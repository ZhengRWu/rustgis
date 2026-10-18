//! 波段统计：流式累积 min/max/mean/std，并对有界抽样样本建直方图，
//! 从而在只解码一遍影像的前提下得到可用的百分比截断值。

use super::BandStatistics;

/// 直方图桶数（与 [`BandStatistics::quantile`] 的分位计算保持一致）。
const BINS: usize = 4096;
/// 直方图样本上限：约 4 MiB，随统计结果之后释放。
const SAMPLE_CAP: usize = 1 << 20;

/// 增量式统计累加器：逐值 `add`，最后 `finish`。
pub(crate) struct StatsAccumulator {
    min: f64,
    max: f64,
    sum: f64,
    sum_sq: f64,
    valid_count: u64,
    /// 抽样储备（reservoir），用于直方图与分位值。
    reservoir: Vec<f32>,
    seen: u64,
    rng: u64,
}

impl StatsAccumulator {
    pub fn new() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            sum: 0.0,
            sum_sq: 0.0,
            valid_count: 0,
            reservoir: Vec::new(),
            seen: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// 添加一个样本；`nodata` 为 NoData 值（相等则跳过），非有限值也跳过。
    pub fn add(&mut self, value: f64, nodata: Option<f64>) {
        if !value.is_finite() {
            return;
        }
        if let Some(nodata) = nodata
            && value == nodata
        {
            return;
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value;
        self.sum_sq += value * value;
        self.valid_count += 1;
        self.seen += 1;

        // 水塘抽样：样本数达到上限后按概率替换，保证样本均匀且内存有界。
        if self.reservoir.len() < SAMPLE_CAP {
            self.reservoir.push(value as f32);
        } else {
            let index = self.next_random() % self.seen;
            if index < SAMPLE_CAP as u64 {
                self.reservoir[index as usize] = value as f32;
            }
        }
    }

    /// 汇总结果。
    pub fn finish(mut self) -> BandStatistics {
        if self.valid_count == 0 || !self.min.is_finite() || !self.max.is_finite() {
            return BandStatistics {
                min: 0.0,
                max: 0.0,
                mean: 0.0,
                std_dev: 0.0,
                valid_count: 0,
                from_file: false,
                estimated: false,
                histogram: Vec::new(),
                percent: None,
            };
        }
        let count = self.valid_count as f64;
        let mean = self.sum / count;
        let variance = (self.sum_sq / count - mean * mean).max(0.0);
        let (histogram, percent) = self.build_histogram();
        BandStatistics {
            min: self.min,
            max: self.max,
            mean,
            std_dev: variance.sqrt(),
            valid_count: self.valid_count,
            from_file: false,
            estimated: false,
            histogram,
            percent,
        }
    }

    /// 由水塘样本构建直方图，并给出 2% / 98% 分位值。
    fn build_histogram(&mut self) -> (Vec<u32>, Option<(f64, f64)>) {
        if self.reservoir.is_empty() || self.max <= self.min {
            return (Vec::new(), None);
        }
        let span = self.max - self.min;
        let mut histogram = vec![0u32; BINS];
        for &value in &self.reservoir {
            let normalized = ((f64::from(value) - self.min) / span).clamp(0.0, 1.0);
            let bin = (normalized * (BINS - 1) as f64).round() as usize;
            histogram[bin.min(BINS - 1)] += 1;
        }
        self.reservoir.sort_unstable_by(f32::total_cmp);
        let percent = Some((
            quantile_of(&self.reservoir, 0.02),
            quantile_of(&self.reservoir, 0.98),
        ));
        (histogram, percent)
    }

    /// 线性同余伪随机数（统计抽样无需密码学强度）。
    fn next_random(&mut self) -> u64 {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.rng >> 11
    }
}

impl Default for StatsAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// 已排序样本的分位值（线性插值）。
fn quantile_of(sorted: &[f32], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let q = q.clamp(0.0, 1.0);
    let position = q * (sorted.len() - 1) as f64;
    let low = position.floor() as usize;
    let high = (low + 1).min(sorted.len() - 1);
    let weight = position - low as f64;
    f64::from(sorted[low]) * (1.0 - weight) + f64::from(sorted[high]) * weight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_basic_statistics() {
        let mut acc = StatsAccumulator::new();
        for value in [1.0, 2.0, 3.0, 4.0] {
            acc.add(value, None);
        }
        let stats = acc.finish();
        assert_eq!(stats.valid_count, 4);
        assert!((stats.min - 1.0).abs() < 1e-9);
        assert!((stats.max - 4.0).abs() < 1e-9);
        assert!((stats.mean - 2.5).abs() < 1e-9);
        assert!((stats.std_dev - 1.118_033_988_749_895).abs() < 1e-6);
        assert_eq!(stats.histogram.len(), BINS);
        let (low, high) = stats.percent.expect("应有百分比分位值");
        assert!(low < high && low >= stats.min && high <= stats.max);
    }

    #[test]
    fn skips_nodata_and_non_finite() {
        let mut acc = StatsAccumulator::new();
        acc.add(10.0, Some(-9999.0));
        acc.add(-9999.0, Some(-9999.0));
        acc.add(f64::NAN, None);
        acc.add(20.0, Some(-9999.0));
        let stats = acc.finish();
        assert_eq!(stats.valid_count, 2);
        assert!((stats.min - 10.0).abs() < 1e-9);
        assert!((stats.max - 20.0).abs() < 1e-9);
        assert!(stats.histogram.iter().all(|count| *count == 0) || stats.max > stats.min);
    }

    #[test]
    fn empty_statistics_are_zeroed() {
        let stats = StatsAccumulator::new().finish();
        assert_eq!(stats.valid_count, 0);
        assert_eq!(stats.min, 0.0);
        assert_eq!(stats.max, 0.0);
        assert!(stats.percent.is_none());
    }

    #[test]
    fn clipped_uses_histogram_quantiles() {
        let mut acc = StatsAccumulator::new();
        for value in 0..=1000 {
            acc.add(f64::from(value), None);
        }
        let stats = acc.finish();
        let (low, high) = stats.clipped(0.5);
        assert!(low > 0.0 && low < 100.0, "low={low}");
        assert!(high > 900.0 && high < 1000.0, "high={high}");
    }
}
