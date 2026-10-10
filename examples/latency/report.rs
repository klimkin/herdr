#[derive(Debug, serde::Serialize)]
pub struct Summary {
    pub completed: usize,
    pub missing: usize,
    pub p50_ns: Option<u64>,
    pub p95_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub p999_ns: Option<u64>,
    pub max_ns: Option<u64>,
    pub deadline_misses: usize,
}

pub fn summarize(values: &[u64], expected: usize, budget_ns: u64) -> Summary {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| {
        let rank = (sorted.len() * numerator).div_ceil(denominator);
        rank.checked_sub(1)
            .and_then(|index| sorted.get(index))
            .copied()
    };
    let missing = expected.saturating_sub(sorted.len());
    Summary {
        completed: sorted.len(),
        missing,
        p50_ns: percentile(50, 100),
        p95_ns: percentile(95, 100),
        p99_ns: percentile(99, 100),
        // At least ten expected observations in the upper 0.1% tail.
        p999_ns: (sorted.len() >= 10_000)
            .then(|| percentile(999, 1000))
            .flatten(),
        max_ns: sorted.last().copied(),
        deadline_misses: sorted.iter().filter(|value| **value > budget_ns).count() + missing,
    }
}
