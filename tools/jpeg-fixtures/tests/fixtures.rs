//! The committed fixtures are exactly what the generator produces.

#[test]
fn committed_fixtures_match_a_fresh_run() {
    let dir = jpeg_fixtures::default_dir();
    let all = jpeg_fixtures::generate();
    assert_eq!(all.len(), 16);
    for (rel, jpg) in all {
        let committed = std::fs::read(dir.join(&rel)).expect("fixture present");
        assert!(
            committed == jpg,
            "{} differs from a fresh run",
            rel.display()
        );
    }
}

#[test]
fn generation_is_deterministic_and_frames_differ() {
    let a = jpeg_fixtures::generate();
    let b = jpeg_fixtures::generate();
    assert_eq!(a, b);
    for w in a.windows(2) {
        assert_ne!(w[0].1, w[1].1);
    }
}
