//! Work partitioning for the renderer's parallel phases (the threads are
//! [`vrt::pool::ThreadPool`]'s).

use alloc::vec::Vec;

pub use vrt::pool::ThreadPool;

/// Splits `weights` into `parts` contiguous ranges of similar total weight.
pub(crate) fn partition(weights: &[u32], parts: usize, out: &mut Vec<(usize, usize)>) {
    out.clear();
    let parts = parts.max(1);
    let total: u64 = weights.iter().map(|&w| w as u64).sum();
    let mut start = 0usize;
    let mut acc = 0u64;
    for p in 0..parts {
        let goal = total * (p as u64 + 1) / parts as u64;
        let mut end = start;
        while end < weights.len() && (acc + weights[end] as u64 <= goal || p == parts - 1) {
            acc += weights[end] as u64;
            end += 1;
        }
        // Make sure progress is made when one item outweighs a share.
        if end == start && end < weights.len() && p < parts - 1 && acc < goal {
            acc += weights[end] as u64;
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
}
