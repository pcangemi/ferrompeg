//! Spreading independent pieces of work over the CPU cores.
//!
//! Work is cut into more chunks than there are threads and every thread
//! takes the next free chunk until none is left. On processors that mix
//! fast and slow cores this keeps the fast ones busy instead of waiting for
//! the slow ones to finish an equal share. Each chunk writes only its own
//! part of the output, so results never depend on the thread count.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Thread count chosen with `-threads`, 0 for one per core.
static THREADS: AtomicUsize = AtomicUsize::new(0);

/// Sets the number of threads used for parallel work, 0 for automatic.
pub fn set_threads(n: usize) {
    THREADS.store(n, Ordering::Relaxed);
}

/// Number of threads parallel work may use, at least 1.
pub fn threads() -> usize {
    match THREADS.load(Ordering::Relaxed) {
        0 => thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
}

/// Splits `data` into chunks of `chunk_len` items and calls
/// `f(chunk_index, chunk)` for each, on several threads when there is more
/// than one chunk. Chunk `i` starts at item `i * chunk_len`.
pub fn for_each_chunk<T, F>(data: &mut [T], chunk_len: usize, f: F)
where
    T: Send,
    F: Fn(usize, &mut [T]) + Sync,
{
    let chunk_len = chunk_len.max(1);
    let workers = threads().min(data.len().div_ceil(chunk_len));
    if workers <= 1 {
        for (i, chunk) in data.chunks_mut(chunk_len).enumerate() {
            f(i, chunk);
        }
        return;
    }
    let queue = Mutex::new(data.chunks_mut(chunk_len).enumerate());
    let work = || {
        loop {
            // A panic in `f` poisons nothing that matters here: the
            // iterator is still valid and the scope re-raises the panic.
            let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
            match next {
                Some((i, chunk)) => f(i, chunk),
                None => break,
            }
        }
    };
    thread::scope(|s| {
        for _ in 1..workers {
            s.spawn(work);
        }
        work();
    });
}

/// Applies `f` to every item, in parallel, keeping the order.
pub fn map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let mut out: Vec<Option<R>> = items.iter().map(|_| None).collect();
    for_each_chunk(&mut out, 1, |i, slot| slot[0] = Some(f(&items[i])));
    out.into_iter()
        .map(|r| r.expect("every slot is filled"))
        .collect()
}

/// Rows per chunk when splitting `rows` rows of `row_cost` units of work,
/// so that a chunk is worth the cost of handing it to a thread and there
/// are still several chunks per thread to balance the load.
pub fn rows_per_chunk(rows: usize, row_cost: usize) -> usize {
    /// Below this much work per chunk, splitting further does not pay.
    const MIN_CHUNK_COST: usize = 1 << 16;
    let min_rows = MIN_CHUNK_COST.div_ceil(row_cost.max(1));
    let balanced = rows.div_ceil(threads() * 4);
    balanced.max(min_rows).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_everything_once() {
        let mut data = vec![0u32; 1000];
        for_each_chunk(&mut data, 7, |i, chunk| {
            for (k, v) in chunk.iter_mut().enumerate() {
                *v += (i * 7 + k) as u32;
            }
        });
        assert!(data.iter().enumerate().all(|(i, &v)| v == i as u32));
    }

    #[test]
    fn map_keeps_order() {
        let items: Vec<u64> = (0..500).collect();
        assert_eq!(
            map(&items, |x| x * x),
            items.iter().map(|x| x * x).collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_input() {
        let mut data: Vec<u8> = Vec::new();
        for_each_chunk(&mut data, 4, |_, _| panic!("no chunk expected"));
        assert!(map(&data, |&x| x).is_empty());
    }

    #[test]
    fn row_chunks_are_never_empty() {
        assert!(rows_per_chunk(0, 0) >= 1);
        assert!(rows_per_chunk(720, 1280) >= 1);
        assert!(rows_per_chunk(10, 1) >= 10);
    }
}
