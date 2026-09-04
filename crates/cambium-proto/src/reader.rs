//! Reader for Bazel's `--output=streamed_proto`: a sequence of
//! length-delimited `blaze_query.Target` messages.
//!
//! Frames are collected into a slab and handed to the caller in batches so the
//! decode can be parallelised without one allocation per target and without
//! buffering a multi-gigabyte query result in memory.

use std::io::Read;

use prost::Message;

use crate::blaze_query::Target;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("reading query output: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoding target message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("truncated stream: {0} trailing bytes after the last complete message")]
    Truncated(usize),
    #[error("message length {0} exceeds the {1} byte limit")]
    TooLarge(u64, u64),
}

/// Largest single message we will accept. Real targets are kilobytes; this is
/// a guard against a desynchronised stream eating all memory.
const MAX_MESSAGE: u64 = 512 << 20;

/// Default frames per batch. Large enough to amortise rayon's fork/join, small
/// enough to keep peak memory bounded.
pub const DEFAULT_BATCH: usize = 16_384;

/// Reads length-delimited frames, invoking `on_batch` with the slab and the
/// frame ranges within it.
pub fn read_frames<R: Read>(
    mut r: R,
    batch_frames: usize,
    mut on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> Result<(), ReadError>,
) -> Result<usize, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(batch_frames);
    let mut cursor = 0usize; // start of the first unparsed frame
    let mut eof = false;
    let mut total = 0usize;

    loop {
        // Parse as many complete frames as the buffer holds.
        loop {
            let Some((len, header)) = read_varint(&buf[cursor..]) else {
                break;
            };
            if len > MAX_MESSAGE {
                return Err(ReadError::TooLarge(len, MAX_MESSAGE));
            }
            let start = cursor + header;
            let end = start + len as usize;
            if end > buf.len() {
                break;
            }
            ranges.push((start, end));
            cursor = end;
            total += 1;
            if ranges.len() >= batch_frames {
                break;
            }
        }

        if ranges.len() >= batch_frames || (eof && cursor == buf.len()) {
            if !ranges.is_empty() {
                on_batch(&buf, &ranges)?;
                ranges.clear();
            }
            if eof && cursor == buf.len() {
                return Ok(total);
            }
            buf.drain(..cursor);
            cursor = 0;
            continue;
        }

        if eof {
            // No more input and an incomplete frame is left over.
            if !ranges.is_empty() {
                on_batch(&buf, &ranges)?;
                ranges.clear();
            }
            let left = buf.len() - cursor;
            return if left == 0 {
                Ok(total)
            } else {
                Err(ReadError::Truncated(left))
            };
        }

        let n = r.read(&mut chunk)?;
        if n == 0 {
            eof = true;
        } else {
            buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Decodes a varint from the front of `b`, returning `(value, bytes_consumed)`.
fn read_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in b.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

/// Convenience wrapper that decodes every frame serially. Used by tests and by
/// small queries where the rayon fan-out is not worth it.
pub fn decode_all<R: Read>(r: R) -> Result<Vec<Target>, ReadError> {
    let mut out = Vec::new();
    read_frames(r, DEFAULT_BATCH, |slab, ranges| {
        for &(s, e) in ranges {
            out.push(Target::decode(&slab[s..e])?);
        }
        Ok(())
    })?;
    Ok(out)
}

/// Encodes targets in the streamed_proto framing. Used by the synthetic proto
/// emitter and by tests.
pub fn encode_stream(targets: &[Target]) -> Vec<u8> {
    let mut out = Vec::new();
    for t in targets {
        let len = t.encoded_len();
        prost::encoding::encode_varint(len as u64, &mut out);
        t.encode(&mut out).expect("Vec never fails to grow");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blaze_query::{target, Rule, Target};

    fn rule_target(name: &str) -> Target {
        Target {
            r#type: target::Discriminator::Rule as i32,
            rule: Some(Rule {
                name: name.to_string(),
                rule_class: "cc_library".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_a_stream() {
        let targets: Vec<Target> = (0..1000).map(|i| rule_target(&format!("//p:t{i}"))).collect();
        let bytes = encode_stream(&targets);
        let got = decode_all(&bytes[..]).unwrap();
        assert_eq!(got.len(), 1000);
        assert_eq!(got[999].rule.as_ref().unwrap().name, "//p:t999");
    }

    #[test]
    fn batches_respect_the_frame_limit() {
        let targets: Vec<Target> = (0..100).map(|i| rule_target(&format!("//p:t{i}"))).collect();
        let bytes = encode_stream(&targets);
        let mut sizes = Vec::new();
        let n = read_frames(&bytes[..], 32, |_, ranges| {
            sizes.push(ranges.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 100);
        assert_eq!(sizes, vec![32, 32, 32, 4]);
    }

    #[test]
    fn handles_reads_that_split_frames() {
        struct Dribble<'a>(&'a [u8]);
        impl std::io::Read for Dribble<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(out.len()).min(3);
                out[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let targets: Vec<Target> = (0..50).map(|i| rule_target(&format!("//p:t{i}"))).collect();
        let bytes = encode_stream(&targets);
        let got = decode_all(Dribble(&bytes)).unwrap();
        assert_eq!(got.len(), 50);
    }

    #[test]
    fn rejects_a_truncated_stream() {
        let bytes = encode_stream(&[rule_target("//p:t")]);
        let err = decode_all(&bytes[..bytes.len() - 2]).unwrap_err();
        assert!(matches!(err, ReadError::Truncated(_)));
    }

    #[test]
    fn empty_stream_is_empty() {
        assert!(decode_all(&[][..]).unwrap().is_empty());
    }
}
