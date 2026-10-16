//! The tdigest port against vectors produced by the real tdigest 0.2.1 gem
//! under Ruby 4.0.0 (bench/diff/tdigest_vectors.rb, run in rw-baseline):
//! Ruby's MT19937 and Array#shuffle, then as_small_bytes and percentiles for
//! digests built from the same values after the same `srand`.

use railwatch_server::rollup::random::RubyRandom;
use railwatch_server::rollup::tdigest::{Num, TDigest};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/tdigest_vectors.json")).unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn ints(v: &Value) -> Vec<i64> {
    v.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect()
}

#[test]
fn mersenne_twister_and_shuffle_match_ruby() {
    let v = vectors();
    for (seed, want) in v["mt"].as_object().unwrap() {
        let mut r = RubyRandom::new(seed.parse().unwrap());
        let got: Vec<i64> = (0..want.as_array().unwrap().len()).map(|_| r.next_u32() as i64).collect();
        assert_eq!(got, ints(want), "srand({seed})");
    }
    let mut r = RubyRandom::new(42);
    let mut a: Vec<i64> = (1..=300).collect();
    r.shuffle(&mut a);
    assert_eq!(a, ints(&v["shuffle"]["42"]));
}

#[test]
fn small_bytes_and_percentiles_match_ruby() {
    for case in vectors()["digests"].as_array().unwrap() {
        let mut rng = RubyRandom::new(case["seed"].as_u64().unwrap() as u32);
        let mut d = TDigest::new(0.01);
        let values = ints(&case["values"]);
        for x in &values {
            d.push_int(*x, &mut rng);
        }
        d.compress(&mut rng);
        assert_eq!(hex(&d.as_small_bytes()), case["bytes"].as_str().unwrap(), "{} values, seed {}", values.len(), case["seed"]);
        let p: Vec<i64> = [0.5, 0.95, 0.99].iter().map(|p| d.percentile(*p).unwrap().to_i()).collect();
        assert_eq!(p, ints(&case["p"]));
    }
}

#[test]
fn from_bytes_then_absorb_matches_ruby() {
    for case in vectors()["absorb"].as_array().unwrap() {
        let mut rng = RubyRandom::new(case["seed"].as_u64().unwrap() as u32);
        let mut d = TDigest::from_bytes(&unhex(case["base"].as_str().unwrap()), &mut rng).unwrap();
        for x in ints(&case["values"]) {
            d.push(Num::I(x as i128), 1, &mut rng);
        }
        d.compress(&mut rng);
        assert_eq!(hex(&d.as_small_bytes()), case["bytes"].as_str().unwrap());
        let p: Vec<i64> = [0.5, 0.95, 0.99].iter().map(|p| d.percentile(*p).unwrap().to_i()).collect();
        assert_eq!(p, ints(&case["p"]));
    }
}
