// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Conversion performance benchmarks

use criterion::{criterion_group, criterion_main, Criterion};

fn conversion_benchmark(_c: &mut Criterion) {
    // To be implemented in Phase 6.
}

criterion_group!(benches, conversion_benchmark);
criterion_main!(benches);
