//! Quick exact-float32 versus TurboVec speed/quality comparison.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use turbovec::IdMapIndex;

fn rows(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut state = seed | 1;
    let mut output = vec![0.0_f32; n * dim];
    for row in output.chunks_exact_mut(dim) {
        let mut norm = 0.0_f32;
        for value in row.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *value = ((state >> 40) as f32 / (1_u64 << 23) as f32) - 0.5;
            norm += *value * *value;
        }
        let inverse_norm = norm.sqrt().recip();
        for value in row {
            *value *= inverse_norm;
        }
    }
    output
}

fn exact_search(database: &[f32], queries: &[f32], dim: usize, k: usize) -> Vec<u64> {
    queries
        .par_chunks_exact(dim)
        .flat_map_iter(|query| {
            let mut scores: Vec<(f32, u64)> = database
                .chunks_exact(dim)
                .enumerate()
                .map(|(index, vector)| {
                    let score = query.iter().zip(vector).map(|(a, b)| a * b).sum();
                    (score, index as u64 + 1)
                })
                .collect();
            scores.select_nth_unstable_by(k - 1, |a, b| {
                b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1))
            });
            scores[..k].sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            scores.into_iter().take(k).map(|(_, id)| id)
        })
        .collect()
}

fn timed<T>(mut operation: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut result = None;
    for _ in 0..3 {
        let start = Instant::now();
        let value = operation();
        best = best.min(start.elapsed());
        result = Some(value);
    }
    (best, result.expect("three benchmark iterations"))
}

fn recall(expected: &[u64], actual: &[u64], k: usize) -> f64 {
    let matches: usize = expected
        .chunks_exact(k)
        .zip(actual.chunks_exact(k))
        .map(|(expected_row, actual_row)| {
            let expected: HashSet<_> = expected_row.iter().copied().collect();
            actual_row.iter().filter(|id| expected.contains(id)).count()
        })
        .sum();
    matches as f64 / expected.len() as f64
}

fn argument(index: usize, default: usize) -> usize {
    std::env::args()
        .nth(index)
        .map(|value| value.parse().expect("arguments must be positive integers"))
        .unwrap_or(default)
}

fn main() {
    let n = argument(1, 10_000);
    let dim = argument(2, 256);
    let nq = argument(3, 100);
    let k = argument(4, 10);
    assert!(n >= 2 && k > 0 && k <= n);
    assert!(dim >= 8 && dim.is_multiple_of(8));

    let database = rows(n, dim, 0x5eed);
    let queries = rows(nq, dim, 0xcafe);
    let ids: Vec<u64> = (1..=n as u64).collect();
    let raw_bytes = database.len() * size_of::<f32>();

    let (exact_time, expected) = timed(|| exact_search(&database, &queries, dim, k));

    println!("n={n}, dim={dim}, queries={nq}, k={k}");
    println!();
    println!("| index | build ms | size MiB | compression | query ms | speedup | recall@{k} |");
    println!("|---|---:|---:|---:|---:|---:|---:|");
    println!(
        "| exact float32 | - | {:.2} | 1.0x | {:.2} | 1.0x | 1.000 |",
        raw_bytes as f64 / 1_048_576.0,
        exact_time.as_secs_f64() * 1_000.0,
    );

    for bit_width in [4, 2] {
        let build_start = Instant::now();
        let mut index = IdMapIndex::new(dim, bit_width).expect("valid index geometry");
        let sample_rows = n.min(1024);
        index
            .calibrate(&database[..sample_rows * dim])
            .expect("representative calibration sample");
        index
            .add_with_ids(&database, &ids)
            .expect("valid vectors and unique ids");
        index.prepare();
        let build_time = build_start.elapsed();
        let index_bytes = index.to_bytes().len();

        let (query_time, result) = timed(|| index.try_search(&queries, k).expect("valid query"));
        println!(
            "| TurboVec {bit_width}-bit | {:.2} | {:.2} | {:.1}x | {:.2} | {:.2}x | {:.3} |",
            build_time.as_secs_f64() * 1_000.0,
            index_bytes as f64 / 1_048_576.0,
            raw_bytes as f64 / index_bytes as f64,
            query_time.as_secs_f64() * 1_000.0,
            exact_time.as_secs_f64() / query_time.as_secs_f64(),
            recall(&expected, &result.ids, k),
        );
    }
}
