//! Sparse measured heights: bounded independently of transcript length.
use std::collections::BTreeMap;
const ESTIMATE: f32 = 120.0;
const MAX_MEASUREMENTS: usize = 2048;
#[derive(Default)]
pub(crate) struct Heights {
    measured: BTreeMap<usize, (usize, f32)>,
}
impl Heights {
    pub fn clear(&mut self) {
        self.measured.clear();
    }
    pub fn set(&mut self, index: usize, id: usize, height: f32) -> bool {
        let height = height.max(32.0);
        let changed = self
            .measured
            .get(&index)
            .is_none_or(|(old, h)| *old != id || (h - height).abs() > 0.5);
        if self.measured.len() >= MAX_MEASUREMENTS && !self.measured.contains_key(&index) {
            // Keep the measurements closest to the current viewport.
            let first = *self.measured.first_key_value().unwrap().0;
            let last = *self.measured.last_key_value().unwrap().0;
            self.measured
                .remove(&if index.abs_diff(first) > index.abs_diff(last) {
                    first
                } else {
                    last
                });
        }
        self.measured.insert(index, (id, height));
        changed
    }
    pub fn matches(&self, index: usize, id: usize) -> bool {
        self.measured.get(&index).is_none_or(|(old, _)| *old == id)
    }
    pub fn top(&self, index: usize) -> f32 {
        index as f32 * ESTIMATE
            + self
                .measured
                .range(..index)
                .map(|(_, (_, h))| h - ESTIMATE)
                .sum::<f32>()
    }
    pub fn at(&self, offset: f32, total: usize) -> usize {
        let (mut low, mut high) = (0, total);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.top(mid + 1) <= offset {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low.min(total.saturating_sub(1))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_heights_locate_short_and_long_blocks_and_remain_bounded() {
        let mut h = Heights::default();
        h.set(0, 1, 40.0);
        h.set(1, 2, 600.0);
        assert_eq!(h.at(39.0, 100), 0);
        assert_eq!(h.at(40.0, 100), 1);
        assert_eq!(h.at(639.0, 100), 1);
        assert_eq!(h.at(640.0, 100), 2);
        for i in 0..10_000 {
            h.set(i, i, 80.0);
        }
        assert_eq!(h.measured.len(), MAX_MEASUREMENTS);
        assert_eq!(h.at(h.top(9000), 10_000), 9000);
    }
}
