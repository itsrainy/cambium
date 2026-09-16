//! Temporary: prints the loading-layer half of the rebuild oracle for two
//! index files named by the environment.

#[test]
#[ignore]
fn dbg() {
    let (Ok(a), Ok(b)) = (std::env::var("APPLIED"), std::env::var("FRESH")) else {
        eprintln!("set APPLIED and FRESH");
        return;
    };
    let a = cambium_index::IndexFile::open(std::path::Path::new(&a)).unwrap();
    let b = cambium_index::IndexFile::open(std::path::Path::new(&b)).unwrap();
    let d = cambium_bench::oracles::rebuild::compare_by_label(a.view(), b.view(), 200);
    eprintln!("{}", d.summary());
    for l in &d.sky_mismatch {
        eprintln!("  {l}");
    }
    eprintln!(
        "applied layer {} nodes {} edges; fresh {} {}",
        a.view().sky.len(),
        a.view().sky.num_edges(),
        b.view().sky.len(),
        b.view().sky.num_edges()
    );
}
