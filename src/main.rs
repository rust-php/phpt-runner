//! phpt runner: runs --FILE-- with a php binary in parallel and compares to --EXPECT--.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

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

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
}

fn run_one(php: &str, file: &Path, id: usize, timeout: Duration) -> Outcome {
    let Ok(src) = fs::read_to_string(file) else { return Outcome::Skip };
    let Some(code) = section(&src, "FILE") else { return Outcome::Skip };
    let exact = section(&src, "EXPECT");
    let fmt = section(&src, "EXPECTF");
    if exact.is_none() && fmt.is_none() {
        return Outcome::Skip;
    }
    let tmp = env::temp_dir().join(format!("phpt_{}_{id}.php", std::process::id()));
    if fs::write(&tmp, code).is_err() {
        return Outcome::Skip;
    }
    let Ok(mut child) = Command::new(php).arg(&tmp).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        let _ = fs::remove_file(&tmp);
        return Outcome::Fail;
    };
    let start = Instant::now();
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = std::io::Read::read_to_end(s, &mut buf);
        }
        buf
    });
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    let out = reader.join().unwrap_or_default();
    let _ = fs::remove_file(&tmp);
    let got = String::from_utf8_lossy(&out).replace("\r\n", "\n");
    let ok = match (exact, fmt) {
        (Some(e), _) => got.trim() == e.trim(),
        (None, Some(f)) => expectf_regex(f.trim()).is_some_and(|re| re.is_match(got.trim())),
        _ => false,
    };
    if !timed_out && ok {
        Outcome::Pass
    } else {
        Outcome::Fail
    }
}

fn expectf_regex(f: &str) -> Option<regex::Regex> {
    let mut re = String::from("(?s)^");
    let mut it = f.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            re.push_str(&regex::escape(&c.to_string()));
            continue;
        }
        match it.next() {
            Some('s') => re.push_str("[^\\r\\n]+"),
            Some('S') => re.push_str("[^\\r\\n]*"),
            Some('a') => re.push_str(".+"),
            Some('A') => re.push_str(".*"),
            Some('w') => re.push_str("\\s*"),
            Some('i') => re.push_str("[+-]?\\d+"),
            Some('d') => re.push_str("\\d+"),
            Some('x') => re.push_str("[0-9a-fA-F]+"),
            Some('f') => re.push_str("[+-]?\\.?\\d+\\.?\\d*(?:[Ee][+-]?\\d+)?"),
            Some('c') => re.push('.'),
            Some('e') => re.push_str("[\\\\/]"),
            Some('%') => re.push('%'),
            Some(o) => {
                re.push_str(&regex::escape("%"));
                re.push_str(&regex::escape(&o.to_string()));
            }
            None => re.push_str("%"),
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: phpt-runner <php-binary> <tests-dir> [--list-failures]");
        std::process::exit(2);
    }
    let list = args.iter().any(|a| a == "--list-failures");
    let mut files = vec![];
    walk(Path::new(&args[2]), &mut files);
    files.sort();
    if let Some(i) = args.iter().position(|a| a == "--only") {
        let keep: std::collections::HashSet<String> = fs::read_to_string(&args[i + 1])
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim_start_matches("PASS ").to_string())
            .collect();
        files.retain(|f| keep.contains(&f.display().to_string()));
    }
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let results: Vec<(usize, Outcome)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..workers)
            .map(|_| {
                sc.spawn(|| {
                    let mut local = vec![];
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() {
                            return local;
                        }
                        local.push((i, run_one(&args[1], &files[i], i, Duration::from_secs(10))));
                    }
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let pass = results.iter().filter(|r| r.1 == Outcome::Pass).count();
    let fail = results.iter().filter(|r| r.1 == Outcome::Fail).count();
    let skip = results.iter().filter(|r| r.1 == Outcome::Skip).count();
    if list {
        for (i, o) in &results {
            match o {
                Outcome::Fail => println!("FAIL {}", files[*i].display()),
                Outcome::Pass => println!("PASS {}", files[*i].display()),
                Outcome::Skip => {}
            }
        }
    }
    println!("{{\"pass\":{pass},\"fail\":{fail},\"skip\":{skip},\"total\":{}}}", pass + fail);
}
