use std::any::Any;

use divan::{
    Bencher,
    counter::{BytesCount, ItemsCount},
};

const COUNTS: &[usize] = &[32, 1_024];

fn main() {
    divan::main();
}

#[allow(dead_code)]
enum InlineClosed {
    Unit,
    Scalar(u64),
    Pair([u64; 2]),
}

#[allow(dead_code)]
enum PaddedClosed {
    Unit,
    Scalar(u64),
    Pair([u64; 2]),
    Large([u8; 256]),
}

#[allow(dead_code)]
enum SelectivelyBoxed {
    Unit,
    Scalar(u64),
    Pair([u64; 2]),
    Large(Box<[u8; 256]>),
}

#[allow(dead_code)]
struct ErasedEnvelope {
    id: u32,
    payload: Box<dyn Any + Send + Sync>,
}

fn inline_small(index: usize) -> InlineClosed {
    match index % 3 {
        0 => InlineClosed::Unit,
        1 => InlineClosed::Scalar(index as u64),
        _ => InlineClosed::Pair([index as u64; 2]),
    }
}

fn padded_small(index: usize) -> PaddedClosed {
    match index % 3 {
        0 => PaddedClosed::Unit,
        1 => PaddedClosed::Scalar(index as u64),
        _ => PaddedClosed::Pair([index as u64; 2]),
    }
}

fn padded_256(index: usize) -> PaddedClosed {
    PaddedClosed::Large([index as u8; 256])
}

fn selectively_boxed_small(index: usize) -> SelectivelyBoxed {
    match index % 3 {
        0 => SelectivelyBoxed::Unit,
        1 => SelectivelyBoxed::Scalar(index as u64),
        _ => SelectivelyBoxed::Pair([index as u64; 2]),
    }
}

fn selectively_boxed_256(index: usize) -> SelectivelyBoxed {
    SelectivelyBoxed::Large(Box::new([index as u8; 256]))
}

fn erased_small(index: usize) -> ErasedEnvelope {
    ErasedEnvelope {
        id: (index % 3) as u32,
        payload: Box::new([index as u64; 2]),
    }
}

fn erased_256(index: usize) -> ErasedEnvelope {
    ErasedEnvelope {
        id: 3,
        payload: Box::new([index as u8; 256]),
    }
}

fn reserved_queue<T>(count: usize, make: fn(usize) -> T) -> Vec<T> {
    let mut queue = Vec::with_capacity(count);
    for index in 0..count {
        queue.push(make(divan::black_box(index)));
    }
    queue
}

fn enqueue_reserved<T>(bencher: Bencher<'_, '_>, count: usize, make: fn(usize) -> T) {
    bencher
        .counter(ItemsCount::new(count))
        // This is the queue's inline footprint; boxed payload bytes are intentionally excluded.
        .counter(BytesCount::of_many::<T>(count))
        .with_inputs(|| Vec::<T>::with_capacity(count))
        .bench_local_refs(|queue| {
            for index in 0..count {
                queue.push(make(divan::black_box(index)));
            }
            divan::black_box(queue.as_slice());
        });
}

fn enqueue_cold<T>(bencher: Bencher<'_, '_>, count: usize, make: fn(usize) -> T) {
    bencher
        .counter(ItemsCount::new(count))
        .counter(BytesCount::of_many::<T>(count))
        .bench_local(|| {
            let mut queue = Vec::new();
            for index in 0..count {
                queue.push(make(divan::black_box(index)));
            }
            divan::black_box(queue)
        });
}

fn drain<T>(bencher: Bencher<'_, '_>, count: usize, make: fn(usize) -> T) {
    bencher
        .counter(ItemsCount::new(count))
        .counter(BytesCount::of_many::<T>(count))
        .with_inputs(|| reserved_queue(count, make))
        .bench_local_values(|mut queue| {
            for event in queue.drain(..) {
                divan::black_box(event);
            }
            divan::black_box(queue)
        });
}

fn drop_queue<T>(bencher: Bencher<'_, '_>, count: usize, make: fn(usize) -> T) {
    bencher
        .counter(ItemsCount::new(count))
        .counter(BytesCount::of_many::<T>(count))
        .with_inputs(|| reserved_queue(count, make))
        .bench_local_values(|queue| drop(divan::black_box(queue)));
}

macro_rules! representation_benches {
    ($module:ident, $ty:ty, $make:path) => {
        mod $module {
            use super::*;

            #[divan::bench(args = COUNTS)]
            fn enqueue_reserved(bencher: Bencher<'_, '_>, count: usize) {
                super::enqueue_reserved::<$ty>(bencher, count, $make);
            }

            #[divan::bench(args = COUNTS)]
            fn enqueue_cold(bencher: Bencher<'_, '_>, count: usize) {
                super::enqueue_cold::<$ty>(bencher, count, $make);
            }

            #[divan::bench(args = COUNTS)]
            fn drain(bencher: Bencher<'_, '_>, count: usize) {
                super::drain::<$ty>(bencher, count, $make);
            }

            #[divan::bench(args = COUNTS)]
            fn drop(bencher: Bencher<'_, '_>, count: usize) {
                super::drop_queue::<$ty>(bencher, count, $make);
            }
        }
    };
}

representation_benches!(small_inline_closed, InlineClosed, inline_small);
representation_benches!(small_padded_closed, PaddedClosed, padded_small);
representation_benches!(
    small_selectively_boxed,
    SelectivelyBoxed,
    selectively_boxed_small
);
representation_benches!(small_registered_erased, ErasedEnvelope, erased_small);
representation_benches!(payload_256_padded_closed, PaddedClosed, padded_256);
representation_benches!(
    payload_256_selectively_boxed,
    SelectivelyBoxed,
    selectively_boxed_256
);
representation_benches!(payload_256_registered_erased, ErasedEnvelope, erased_256);
