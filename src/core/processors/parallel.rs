#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeConfig {
    pub workers: usize,
    pub serial_threshold: usize,
}

impl Default for ComputeConfig {
    fn default() -> Self {
        Self { workers: 4, serial_threshold: 1024 }
    }
}

pub struct ComputePool {
    pool: Option<rayon::ThreadPool>,
    config: ComputeConfig,
}

impl std::fmt::Debug for ComputePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputePool").field("config", &self.config).finish()
    }
}

impl Default for ComputePool {
    fn default() -> Self {
        Self::new(ComputeConfig::default())
    }
}

impl ComputePool {
    pub fn new(config: ComputeConfig) -> Self {
        let pool = if config.workers > 1 {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(config.workers)
                    .thread_name(|i| format!("ec-compute-{i}"))
                    .build()
                    .expect("failed to build compute thread pool"),
            )
        } else {
            None
        };
        Self { pool, config }
    }

    pub fn config(&self) -> &ComputeConfig {
        &self.config
    }

    pub fn map<X, T>(&self, items: &[X], map_one: impl Fn(&X) -> T + Sync) -> Vec<T>
    where
        X: Sync,
        T: Send,
    {
        match &self.pool {
            Some(pool) if items.len() >= self.config.serial_threshold => {
                use rayon::prelude::*;
                pool.install(|| items.par_iter().map(&map_one).collect())
            }
            _ => items.iter().map(&map_one).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(items: &[i64], workers: usize, threshold: usize) -> Vec<i64> {
        let pool = ComputePool::new(ComputeConfig { workers, serial_threshold: threshold });
        pool.map(items, |&x| x * x)
    }

    #[test]
    fn map_empty_and_single() {
        assert!(square(&[], 4, 0).is_empty());
        assert_eq!(square(&[7], 4, 0), vec![49]);
    }

    #[test]
    fn map_preserves_input_order_serial_and_parallel() {
        let input: Vec<i64> = (0..10_000).collect();
        let expect: Vec<i64> = input.iter().map(|&x| x * x).collect();
        assert_eq!(square(&input, 1, 1024), expect, "workers=1 serial");
        assert_eq!(square(&input, 8, 0), expect, "workers=8 parallel, threshold=0");
    }

    #[test]
    fn serial_threshold_forces_serial_below_n() {
        let input: Vec<i64> = (0..500).collect();
        assert_eq!(square(&input, 8, 100_000), square(&input, 8, 0));
    }

    #[test]
    fn threshold_boundary_at_len_takes_parallel_below_takes_serial() {
        // The branch is `items.len() >= serial_threshold`: len == threshold goes parallel,
        // len == threshold-1 stays serial. Both must equal the canonical ordered result --
        // the boundary must not drop or reorder anything.
        let threshold = 64usize;
        let at: Vec<i64> = (0..threshold as i64).collect();
        let below: Vec<i64> = (0..threshold as i64 - 1).collect();
        let expect_at: Vec<i64> = at.iter().map(|&x| x * x).collect();
        let expect_below: Vec<i64> = below.iter().map(|&x| x * x).collect();
        assert_eq!(square(&at, 8, threshold), expect_at, "len == threshold -> parallel path, still ordered");
        assert_eq!(square(&below, 8, threshold), expect_below, "len == threshold-1 -> serial path, still ordered");
    }

    #[test]
    fn workers_le_one_is_always_serial_even_with_zero_threshold() {
        // workers <= 1 => no pool built => serial regardless of a large input and threshold=0.
        // workers == 0 must degrade gracefully (no panic), not attempt to build an empty pool.
        let input: Vec<i64> = (0..5_000).collect();
        let expect: Vec<i64> = input.iter().map(|&x| x * x).collect();
        assert_eq!(square(&input, 1, 0), expect, "workers=1 stays serial");
        assert_eq!(square(&input, 0, 0), expect, "workers=0 degrades to serial without panicking");
    }

    #[test]
    fn identity_map_preserves_order_across_worker_matrix() {
        // Identity map on 0..N: the output must be exactly the input, in order. A collect that
        // lost rayon's positional ordering would scramble this for any workers > 1 -- x*x is
        // monotonic on 0..N so this identity form is a stricter order check than `square`.
        let input: Vec<i64> = (0..10_000).collect();
        for workers in [1usize, 2, 3, 4, 8, 16] {
            let pool = ComputePool::new(ComputeConfig { workers, serial_threshold: 0 });
            assert_eq!(pool.map(&input, |&x| x), input, "workers={workers}: identity map must preserve input order");
        }
    }
}

const _: () = {
    fn assert_sync<T: Sync>() {}
    let _ = assert_sync::<crate::core::common::user_profile::UserProfile>;
    let _ = assert_sync::<crate::core::common::symbol_position_record::SymbolPositionRecord>;
    let _ = assert_sync::<crate::core::processors::symbol_specification_provider::SymbolSpecificationProvider>;
    let _ = assert_sync::<crate::core::processors::loan::loan_service::LoanService>;
    let _ = assert_sync::<crate::core::common::last_price_cache_record::LastPriceCacheRecord>;
};
