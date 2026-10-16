//! rw-rollup: run the RollupJob / ReleaseHealthRollupJob ports against a
//! tenant file, for the Rails-equivalence scripts and timings.
//!
//!   rw-rollup FILE all [--seed N]            # every hour with raw rows
//!   rw-rollup FILE HOUR_EPOCH... [--seed N]  # given hours
//!
//! --seed N: a Ruby MT stream per hour, seeded like `srand(N + hour index)`,
//! which is what bench/diff/rollup_rails.rb does on the Rails side. Without
//! it each group gets its own stream (the server's mode).

use std::time::Instant;

use railwatch_server::rollup::random::RubyRandom;
use railwatch_server::rollup::{release_health_rollup_job, rollup_job};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = args.first().expect("usage: rw-rollup FILE all|HOUR... [--seed N]");
    let mut seed = None;
    let mut hours_arg = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--seed" {
            seed = Some(args[i + 1].parse::<u32>().expect("seed"));
            i += 2;
        } else {
            hours_arg.push(args[i].clone());
            i += 1;
        }
    }
    let conn = railwatch_server::tenants::open_tenant(std::path::Path::new(file)).expect("open tenant");
    let hours: Vec<i64> = if hours_arg == ["all"] {
        rollup_job::hours_with_rows(&conn).expect("hours")
    } else {
        hours_arg.iter().map(|h| h.parse().expect("hour epoch")).collect()
    };
    let started = Instant::now();
    let mut groups = 0;
    for (i, h) in hours.iter().enumerate() {
        let mut shared = seed.map(|s| RubyRandom::new(s.wrapping_add(i as u32)));
        let t = Instant::now();
        let mut rng = match shared.as_mut() {
            Some(r) => rollup_job::Rng::Shared(r),
            None => rollup_job::Rng::PerGroup,
        };
        let n = rollup_job::perform(&conn, &conn, *h, &mut rng).expect("rollup");
        let rh = release_health_rollup_job::perform(&conn, &conn, *h, i64::MIN).expect("release health");
        groups += n;
        eprintln!("hour {h}: {n} rollup rows, {} release_health rows, {:.1} ms", rh.unwrap_or(0), t.elapsed().as_secs_f64() * 1e3);
    }
    println!("{} hours, {groups} rollup rows, {:.1} ms", hours.len(), started.elapsed().as_secs_f64() * 1e3);
}
