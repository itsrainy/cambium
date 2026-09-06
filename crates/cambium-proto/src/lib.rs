//! Bazel `blaze_query` proto types, the streamed_proto reader, and the
//! canonical rule hash.
//!
//! The proto is vendored under `proto/build.proto` (see its header for the
//! Bazel version and the two local edits) and compiled by `protox`, a pure-Rust
//! protobuf compiler, so no `protoc` is needed to build cambium.

pub mod blaze_query {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/blaze_query.rs"));
}

pub mod convert;
pub mod hash;
pub mod reader;

pub use convert::{convert, ConvertOptions, ParsedTarget};
pub use hash::{canonical_rule, rule_hash, RULE_HASH_VERSION};
pub use reader::{decode_all, encode_stream, read_frames, ReadError, DEFAULT_BATCH};
