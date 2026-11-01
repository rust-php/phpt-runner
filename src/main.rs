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
    let outfile = env::temp_dir().join(format!("phpt_{}_{id}.out", std::process::id()));
    let Ok(out_handle) = fs::File::create(&outfile) else {
        let _ = fs::remove_file(&tmp);
        return Outcome::Skip;
    };
    let status = Command::new("timeout")
        .args(["-k", "1", &timeout.as_secs().to_string()])
        .arg(php)
        .arg(&tmp)
        .current_dir(file.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_handle))
        .stderr(Stdio::null())
        .status();
    let timed_out = matches!(&status, Ok(s) if s.code() == Some(124) || s.code() == Some(137));
    let out = fs::read(&outfile).unwrap_or_default();
    let _ = fs::remove_file(&outfile);
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
        if env::var("PHPT_DIFF").is_ok() {
            let want = exact.or(fmt).unwrap_or("").trim().to_string();
            let is_fmt = exact.is_none();
            let (gl, wl): (Vec<&str>, Vec<&str>) = (got.trim().lines().collect(), want.lines().collect());
            for i in 0..gl.len().max(wl.len()) {
                let (g, w) = (gl.get(i).copied().unwrap_or("<EOF>"), wl.get(i).copied().unwrap_or("<EOF>"));
                let same = if is_fmt { expectf_regex(w).is_some_and(|re| re.is_match(g)) } else { g == w };
                if !same {
                    eprintln!("DIFF {} | got: {} | want: {}", file.display(), g.chars().take(80).collect::<String>(), w.chars().take(80).collect::<String>());
                    break;
                }
            }
            if timed_out {
                eprintln!("DIFF {} | TIMEOUT", file.display());
            }
        }
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
    let php_abs = fs::canonicalize(&args[1]).map(|p| p.display().to_string()).unwrap_or_else(|_| args[1].clone());
    let mut args = args;
    args[1] = php_abs;
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
