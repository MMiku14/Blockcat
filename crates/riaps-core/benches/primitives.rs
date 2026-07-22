//! Benchmarks for the lock-free primitives in `riaps-core`.
//!
//! Run with: `cargo bench -p riaps-core`

mod support;

use std::mem::MaybeUninit;
use std::time::Instant;

use riaps_core::{DropVTable, EmergencySlot, RcuHandle, SpscRing};
use support::{benchmark, benchmark_samples, print_header, report};

const SAMPLES: usize = 120;

fn main() {
    print_header("riaps-core lock-free primitives");
    bench_spsc_push();
    bench_spsc_pop();
    bench_spsc_pop_slice();
    bench_emergency_slot_push();
    bench_rcu_load();
    bench_rcu_swap();

    println!();
    println!("Reference budgets require a pinned CPU, fixed frequency, and an idle host.");
}

fn bench_spsc_push() {
    const OPS: u64 = 4_096;
    let ring = SpscRing::<u64, 4_096>::new();
    // SAFETY: this benchmark owns the only producer and consumer.
    let (mut producer, mut consumer) = unsafe { ring.split() };

    let result = benchmark_samples("spsc_push_u64", SAMPLES, OPS, || {
        let started = Instant::now();
        for value in 0..OPS {
            assert!(producer.push(value).is_ok());
        }
        let elapsed = started.elapsed();

        for expected in 0..OPS {
            assert_eq!(consumer.pop(), Some(expected));
        }
        elapsed
    });
    report(&result, Some(25.0));
}

fn bench_spsc_pop() {
    const OPS: u64 = 4_096;
    let ring = SpscRing::<u64, 4_096>::new();
    // SAFETY: this benchmark owns the only producer and consumer.
    let (mut producer, mut consumer) = unsafe { ring.split() };

    let result = benchmark_samples("spsc_pop_u64", SAMPLES, OPS, || {
        for value in 0..OPS {
            assert!(producer.push(value).is_ok());
        }

        let mut checksum = 0_u64;
        let started = Instant::now();
        for _ in 0..OPS {
            checksum ^= consumer.pop().expect("the ring was prefilled");
        }
        let elapsed = started.elapsed();

        std::hint::black_box(checksum);
        assert!(ring.is_empty());
        elapsed
    });
    report(&result, Some(25.0));
}

fn bench_spsc_pop_slice() {
    const WIDTH: usize = 16;
    const OPS: u64 = 256;
    let ring = SpscRing::<f32, 4_096>::new();
    // SAFETY: this benchmark owns the only producer and consumer.
    let (mut producer, mut consumer) = unsafe { ring.split() };
    let mut destination = [MaybeUninit::<f32>::uninit(); WIDTH];

    let result = benchmark_samples("spsc_pop_slice_f32_x16", SAMPLES, OPS, || {
        for value in 0..(OPS as usize * WIDTH) {
            assert!(producer.push(value as f32).is_ok());
        }

        let started = Instant::now();
        for _ in 0..OPS {
            consumer
                .pop_slice_copy(&mut destination)
                .expect("the ring was prefilled");
            std::hint::black_box(&destination);
        }
        let elapsed = started.elapsed();

        assert!(ring.is_empty());
        elapsed
    });
    report(&result, Some(35.0));
}

fn bench_emergency_slot_push() {
    const OPS: u64 = 512;
    let slots: Vec<EmergencySlot> = (0..OPS).map(|_| EmergencySlot::new()).collect();
    let pointers: Vec<*mut ()> = (0..OPS)
        .map(|value| Box::into_raw(Box::new(value)).cast::<()>())
        .collect();
    let vtable = DropVTable::of::<u64>();

    let result = benchmark_samples("emergency_slot_push", SAMPLES, OPS, || {
        let started = Instant::now();
        for (slot, &pointer) in slots.iter().zip(&pointers) {
            // SAFETY: every slot has one writer and is vacant at sample start;
            // every pointer came from `Box::into_raw` with the matching vtable.
            assert!(unsafe { slot.push(pointer, vtable) });
        }
        let elapsed = started.elapsed();

        for (slot, &expected) in slots.iter().zip(&pointers) {
            // SAFETY: this benchmark is the only slot consumer.
            let (actual, _) = unsafe { slot.drain() }.expect("the slot was filled");
            assert_eq!(actual, expected);
        }
        elapsed
    });
    report(&result, Some(15.0));

    for pointer in pointers {
        // SAFETY: all slots were drained and each allocation is reclaimed once.
        unsafe { drop(Box::from_raw(pointer.cast::<u64>())) };
    }
}

fn bench_rcu_load() {
    let pointer = Box::into_raw(Box::new(42_u64));
    let handle = RcuHandle::new(pointer);
    let result = benchmark("rcu_load", SAMPLES, 8_192, || {
        std::hint::black_box(handle.load());
    });
    report(&result, Some(5.0));

    // SAFETY: the handle never owned deallocation; this is the original pointer.
    unsafe { drop(Box::from_raw(pointer)) };
}

fn bench_rcu_swap() {
    let first = Box::into_raw(Box::new(1_u64));
    let second = Box::into_raw(Box::new(2_u64));
    let handle = RcuHandle::new(first);
    let mut standby = second;

    let result = benchmark("rcu_swap", SAMPLES, 8_192, || {
        // SAFETY: there are no concurrent readers. The returned allocation is
        // retained as `standby` and becomes the next published pointer.
        standby = unsafe { handle.swap(standby) };
        std::hint::black_box(standby);
    });
    report(&result, Some(10.0));

    let current = handle.load().cast_mut();
    assert_ne!(current, standby);
    // SAFETY: `current` and `standby` are the two distinct Box allocations.
    unsafe {
        drop(Box::from_raw(current));
        drop(Box::from_raw(standby));
    }
}