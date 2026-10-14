//! lince: a resident filename index for Linux desktops.
//!
//!   lince serve  [--config F] [--root DIR]... [--exclude P]... [--socket S]
//!                [--snapshot F] [--no-watch] [--max-watches N] [--exit-with-parent]
//!   lince search [--socket S] [--dirs|--files] [--hidden] [--limit N] [--json] TERMS...
//!   lince status [--socket S]
//!   lince query  [--socket S]        raw protocol: request lines on stdin
//!   lince oneshot --root DIR... --query Q --kind d|f [filters]   index + search
//!                in one process, no daemon (tests, scripts)
//!   lince stats  --root DIR... [--exclude P]... [--snapshot F]   measurements

mod config;
mod fold;
mod index;
mod persist;
mod query;
mod server;
mod watch;

use std::path::PathBuf;
use std::time::Instant;

const USAGE: &str = "usage: lince serve|search|status|query|oneshot|stats [options] (see README)";

/// Flags that take a value; everything else is a switch or a positional.
const VALUED: &[&str] = &[
    "--config", "--root", "--exclude", "--socket", "--snapshot", "--limit", "--query", "--kind",
    "--max-results", "--max-watches", "--extensions", "--scope", "--max-depth", "--select",
];

struct Args(Vec<String>);

impl Args {
    fn value(&self, flag: &str) -> Option<String> {
        self.0.iter().position(|a| a == flag).and_then(|i| self.0.get(i + 1).cloned())
    }
    fn values(&self, flag: &str) -> Vec<String> {
        self.0.windows(2).filter(|w| w[0] == flag).map(|w| w[1].clone()).collect()
    }
    fn flag(&self, flag: &str) -> bool {
        self.0.iter().any(|a| a == flag)
    }
    fn required(&self, flag: &str) -> Result<String, String> {
        self.value(flag).ok_or_else(|| format!("missing {flag}"))
    }
    fn positionals(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut it = self.0.iter().skip(1);
        while let Some(a) = it.next() {
            if VALUED.contains(&a.as_str()) {
                it.next();
            } else if !a.starts_with("--") {
                out.push(a.clone());
            }
        }
        out
    }
    fn socket(&self) -> Result<PathBuf, String> {
        self.value("--socket").map(|s| Ok(config::expand(&s))).unwrap_or_else(config::default_socket)
    }
}

pub fn threads() -> usize {
    std::env::var("LINCE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8))
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

fn roots(args: &Args) -> Result<Vec<PathBuf>, String> {
    let roots: Vec<PathBuf> = args.values("--root").iter().map(|r| config::expand(r)).collect();
    if roots.is_empty() {
        return Err("missing --root".into());
    }
    Ok(roots)
}

fn cmd_serve(args: &Args) -> Result<(), String> {
    let excludes = args.values("--exclude");
    let cfg = config::load(config::Overrides {
        config: args.value("--config").map(PathBuf::from),
        roots: args.values("--root"),
        excludes: if excludes.is_empty() && !args.flag("--no-excludes") { None } else { Some(excludes) },
        socket: args.value("--socket"),
        snapshot: args.value("--snapshot"),
        no_watch: args.flag("--no-watch"),
        max_watches: match args.value("--max-watches") {
            Some(v) => Some(v.parse().map_err(|_| format!("--max-watches: not a number: {v}"))?),
            None => None,
        },
        exit_with_parent: args.flag("--exit-with-parent"),
    })?;
    server::serve(cfg)
}

fn cmd_search(args: &Args) -> Result<(), String> {
    let terms = args.positionals();
    let kinds: Vec<&str> = if args.flag("--dirs") {
        vec!["d"]
    } else if args.flag("--files") {
        vec!["f"]
    } else {
        vec!["d", "f"]
    };
    let limit: usize = args.value("--limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let resp = server::ask(
        &args.socket()?,
        &serde_json::json!({
            "query": terms.join(" "), "kinds": kinds, "hidden": args.flag("--hidden"),
            "max_results": limit.max(1),
        }),
    )?;
    if let Some(err) = resp.get("error") {
        return Err(err.as_str().unwrap_or("error").to_string());
    }
    if args.flag("--json") {
        println!("{resp}");
        return Ok(());
    }
    let mut out = String::new();
    for key in ["dirs", "files"] {
        for p in resp[key].as_array().into_iter().flatten().filter_map(|p| p.as_str()).take(limit) {
            out.push_str(p);
            out.push('\n');
        }
    }
    print!("{out}");
    Ok(())
}

fn cmd_status(args: &Args) -> Result<(), String> {
    let resp = server::ask(&args.socket()?, &serde_json::json!({ "op": "status" }))?;
    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
    Ok(())
}

fn cmd_oneshot(args: &Args) -> Result<(), String> {
    let excludes = args.values("--exclude");
    let index = index::build(&roots(args)?, &excludes, threads(), &index::NoHook)?;
    let req = query::Request {
        op: None,
        id: 0,
        query: args.required("--query")?,
        kinds: vec![args.required("--kind")?],
        max_results: args.value("--max-results").and_then(|v| v.parse().ok()).unwrap_or(2000),
        extensions: args
            .value("--extensions")
            .map(|v| v.split(',').filter(|e| !e.is_empty()).map(String::from).collect())
            .unwrap_or_default(),
        exclude: excludes,
        hidden: args.flag("--hidden"),
        roots: Vec::new(),
        scopes: args.values("--scope"),
        max_depth: args.value("--max-depth").and_then(|v| v.parse().ok()),
        mtimes: 0,
        select: args.value("--select"),
    };
    let resp = query::search(&index, &req)?;
    let mut out = String::new();
    for path in resp.dirs.iter().chain(&resp.files) {
        out.push_str(path);
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

fn cmd_stats(args: &Args) -> Result<(), String> {
    let roots = roots(args)?;
    let excludes = args.values("--exclude");
    let rss0 = rss_kib();
    let t = Instant::now();
    let index = index::build(&roots, &excludes, threads(), &index::NoHook)?;
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        serde_json::json!({
            "build_ms": (build_ms * 10.0).round() / 10.0, "entries": index.live - index.roots.len(),
            "dirs": index.live_dirs().count(), "index_bytes": index.memory_bytes(),
            "rss_delta_kib": rss_kib().saturating_sub(rss0), "threads": threads(),
        })
    );
    if let Some(snap) = args.value("--snapshot") {
        let snap = PathBuf::from(snap);
        let t = Instant::now();
        persist::save(&index, &snap)?;
        let save_ms = t.elapsed().as_secs_f64() * 1000.0;
        let size = std::fs::metadata(&snap).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        let mut loaded = persist::load(&snap, &roots, &excludes)?;
        let load_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let changed = persist::refresh(&mut loaded, threads(), &index::NoHook);
        let validate_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{}",
            serde_json::json!({
                "save_ms": save_ms, "snapshot_bytes": size, "load_ms": load_ms,
                "validate_ms": validate_ms, "changed_dirs": changed,
            })
        );
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else { return Err(USAGE.into()) };
    let args = Args(argv);
    match cmd.as_str() {
        "serve" => cmd_serve(&args),
        "search" => cmd_search(&args),
        "status" => cmd_status(&args),
        "query" => server::client(&args.socket()?),
        "oneshot" => cmd_oneshot(&args),
        "stats" => cmd_stats(&args),
        "--version" | "version" => {
            println!("lince {} (protocol {})", env!("CARGO_PKG_VERSION"), query::PROTOCOL);
            Ok(())
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unknown command {cmd}\n{USAGE}")),
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("lince: {err}");
        std::process::exit(1);
    }
}
