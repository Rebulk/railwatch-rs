//! Reading a stored t-digest's centroids, as Telemetry::Rollup.centroids does
//! (both encodings of the `tdigest` gem). Self-contained: the encoder lives
//! with the rollup job.
//!
//! Layout (native little-endian, `unpack("LdL")`): u32 format, f64
//! compression, u32 size, then
//! - VERBOSE (1): `size` f64 means, then `size` u32 counts;
//! - SMALL (2): `size` f32 mean deltas (running sum, accumulated in a Ruby
//!   Float, i.e. f64), then the weights: one byte each when there are exactly
//!   `size` bytes, else 7-bit varints (low group first).

pub const VERBOSE_ENCODING: u32 = 1;
pub const SMALL_ENCODING: u32 = 2;

#[derive(Debug, PartialEq)]
pub struct UnknownEncoding(pub u32);

/// Appends one digest's centroid means and weights.
pub fn centroids(bytes: &[u8], means: &mut Vec<f64>, counts: &mut Vec<u64>) -> Result<(), UnknownEncoding> {
    if bytes.len() < 16 {
        return Ok(()); // unpack yields nils; Ruby would raise later on nil size
    }
    let format = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    match format {
        VERBOSE_ENCODING => {
            for i in 0..size {
                let at = 16 + 8 * i;
                means.push(bytes.get(at..at + 8).map_or(0.0, |b| f64::from_le_bytes(b.try_into().unwrap())));
            }
            for i in 0..size {
                let at = 16 + 8 * size + 4 * i;
                counts.push(bytes.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as u64));
            }
        }
        SMALL_ENCODING => {
            let mut mean = 0.0f64;
            for i in 0..size {
                let at = 16 + 4 * i;
                let delta = bytes.get(at..at + 4).map_or(0.0, |b| f32::from_le_bytes(b.try_into().unwrap()));
                mean += delta as f64;
                means.push(mean);
            }
            let weights = bytes.get(16 + 4 * size..).unwrap_or(&[]);
            if weights.len() == size {
                counts.extend(weights.iter().map(|&b| b as u64));
            } else {
                let mut at = 0;
                for _ in 0..size {
                    let mut byte = weights.get(at).copied().unwrap_or(0) as u64;
                    at += 1;
                    let mut weight = byte & 0x7f;
                    let mut shift = 7;
                    while byte & 0x80 != 0 {
                        byte = weights.get(at).copied().unwrap_or(0) as u64;
                        at += 1;
                        weight += (byte & 0x7f) << shift;
                        shift += 7;
                    }
                    counts.push(weight);
                }
            }
        }
        other => return Err(UnknownEncoding(other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(means: &[f32], weights: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(2u32.to_le_bytes());
        b.extend(0.01f64.to_le_bytes());
        b.extend((means.len() as u32).to_le_bytes());
        let mut prev = 0.0f32;
        for m in means {
            b.extend((m - prev).to_le_bytes());
            prev = *m;
        }
        b.extend(weights);
        b
    }

    #[test]
    fn small_one_byte_weights() {
        let (mut m, mut c) = (vec![], vec![]);
        centroids(&small(&[10.0, 20.0, 35.0], &[1, 2, 3]), &mut m, &mut c).unwrap();
        assert_eq!(m, vec![10.0, 20.0, 35.0]);
        assert_eq!(c, vec![1, 2, 3]);
    }

    #[test]
    fn small_varint_weights() {
        // 300 = 0b1_0010_1100 -> [0xAC, 0x02]
        let (mut m, mut c) = (vec![], vec![]);
        centroids(&small(&[1.0, 2.0], &[0xAC, 0x02, 5]), &mut m, &mut c).unwrap();
        assert_eq!(c, vec![300, 5]);
    }

    #[test]
    fn verbose() {
        let mut b = Vec::new();
        b.extend(1u32.to_le_bytes());
        b.extend(0.01f64.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(5.5f64.to_le_bytes());
        b.extend(7.25f64.to_le_bytes());
        b.extend(3u32.to_le_bytes());
        b.extend(4u32.to_le_bytes());
        let (mut m, mut c) = (vec![], vec![]);
        centroids(&b, &mut m, &mut c).unwrap();
        assert_eq!((m, c), (vec![5.5, 7.25], vec![3, 4]));
    }
}
