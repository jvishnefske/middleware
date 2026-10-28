//! `jpeg-fixtures [--check] [DIR]`: writes the fake camera's JPEG frames
//! into `DIR` (default: the crate's `fixtures/cam`), or with `--check`
//! verifies that the files there are byte-identical to a fresh run.
//! Prints each file's size.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut check = false;
    let mut dir = jpeg_fixtures::default_dir();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: jpeg-fixtures [--check] [DIR]");
                return ExitCode::SUCCESS;
            }
            other => dir = other.into(),
        }
    }
    let mut ok = true;
    for (rel, jpg) in jpeg_fixtures::generate() {
        let path = dir.join(&rel);
        if check {
            let same = std::fs::read(&path).is_ok_and(|b| b == jpg);
            println!(
                "{} {} {}",
                if same { "ok  " } else { "DIFF" },
                rel.display(),
                jpg.len()
            );
            ok &= same;
        } else {
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, &jpg));
            if let Err(e) = written {
                eprintln!("{}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            println!("{} {}", rel.display(), jpg.len());
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
