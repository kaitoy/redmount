/// One line on stdout with the UTC time. Unlike `println!`, a closed stdout (broken pipe) doesn't panic:
/// this runs inside filesystem callbacks.
macro_rules! log {
    ($($t:tt)*) => {{
        use std::io::Write;
        let s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            % 86_400;
        let _ = writeln!(
            std::io::stdout(),
            "{:02}:{:02}:{:02}Z {}",
            s / 3600,
            s / 60 % 60,
            s % 60,
            format_args!($($t)*)
        );
    }};
}

mod fs;
#[cfg(unix)]
mod fuse;
mod markdown;
mod redmine;
#[cfg(windows)]
mod win;

use anyhow::Context;
use clap::Parser;

/// Mount a Redmine project as a drive (REDMINE_API_KEY env var = your API key).
#[derive(Parser)]
struct Args {
    /// e.g. https://redmine.example.com
    #[arg(long)]
    url: String,
    /// project identifier
    #[arg(long)]
    project: String,
    /// drive letter (Windows) or an empty directory (Linux/macOS)
    #[cfg_attr(windows, arg(long, default_value = "R:"))]
    #[cfg_attr(not(windows), arg(long))]
    mount: String,
    /// extra Redmine issue filter for listings, e.g. "assigned_to_id=me&fixed_version_id=3"
    #[arg(long, default_value = "")]
    filter: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let key = std::env::var("REDMINE_API_KEY")
        .context("Please set the REDMINE_API_KEY environment variable")?;
    let api = redmine::Api::new(&args.url, &key)?;
    let meta = api.meta(&args.project)?;
    let trackers: Vec<_> = meta.trackers.iter().map(|t| t.name.as_str()).collect();
    log!(
        "project #{} trackers: {} ({} users, {} statuses, {} versions, wiki: {})",
        meta.project_id,
        trackers.join(", "),
        meta.users.len(),
        meta.statuses.len(),
        meta.versions.len(),
        if meta.wiki_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    let fs = fs::RedmineFs::new(api, meta, args.filter.trim_matches('&').to_string());
    #[cfg(windows)]
    return win::run(fs, &args.mount);
    #[cfg(unix)]
    return fuse::run(fs, &args.mount);
}

/// Blocks until Enter; with no console (stdin closed), until killed.
fn wait() -> anyhow::Result<()> {
    if std::io::stdin().read_line(&mut String::new())? == 0 {
        loop {
            std::thread::park();
        }
    }
    Ok(())
}
