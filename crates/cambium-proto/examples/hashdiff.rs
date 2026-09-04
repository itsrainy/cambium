//! Dumps the canonical rule protos and hashes from `streamed_proto` files.
//!
//! Handy when a target is unexpectedly (un)affected and you want to see what
//! the hash actually covers:
//!
//! ```sh
//! bazel query '//app:*' --output=streamed_proto --proto:locations=false \
//!   --proto:default_values=false --proto:flatten_selects=false > before.bin
//! # ...edit...
//! cargo run -p cambium-proto --example hashdiff -- before.bin after.bin
//! ```

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let verbose = args.peek().map(|a| a == "-v").unwrap_or(false);
    if verbose {
        args.next();
    }
    let files: Vec<String> = args.collect();
    if files.is_empty() {
        eprintln!("usage: hashdiff [-v] <streamed_proto file>...");
        std::process::exit(2);
    }
    for f in &files {
        let bytes = match std::fs::read(f) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{f}: {e}");
                std::process::exit(1);
            }
        };
        for t in cambium_proto::decode_all(&bytes[..]).expect("decoding stream") {
            if let Some(r) = &t.rule {
                println!("{f}\t{}\t{}", hex(&cambium_proto::rule_hash(r)), r.name);
                if verbose {
                    println!("  {:?}", cambium_proto::canonical_rule(r));
                }
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
