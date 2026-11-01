//! Minimal phpt runner: runs each --FILE-- with a php binary and compares to --EXPECT--.
use std::{env, fs, path::{Path, PathBuf}, process::Command};

fn section<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let tag = format!("--{name}--\n");
    let start = src.find(&tag)? + tag.len();
    let rest = &src[start..];
    let end = rest.find("\n--").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "phpt") {
            out.push(p);
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: phpt-runner <php-binary> <tests-dir>");
        std::process::exit(2);
    }
    let mut files = vec![];
    walk(Path::new(&args[2]), &mut files);
    files.sort();
    let (mut pass, mut total) = (0u32, 0u32);
    for f in &files {
        let Ok(src) = fs::read_to_string(f) else { continue };
        let (Some(code), Some(exp)) = (section(&src, "FILE"), section(&src, "EXPECT")) else {
            continue;
        };
        total += 1;
        let tmp = env::temp_dir().join(format!("phpt_{total}.php"));
        if fs::write(&tmp, code).is_err() {
            continue;
        }
        if let Ok(o) = Command::new(&args[1]).arg(&tmp).output() {
            if String::from_utf8_lossy(&o.stdout).trim() == exp.trim() {
                pass += 1;
            }
        }
    }
    println!("{{\"pass\":{pass},\"total\":{total}}}");
}
