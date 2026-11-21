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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let key = std::env::var("REDMINE_API_KEY").context("Please set the REDMINE_API_KEY environment variable")?;
    let api = redmine::Api::new(&args.url, &key)?;
    let meta = api.meta(&args.project)?;
    let trackers: Vec<_> = meta.trackers.iter().map(|t| t.name.as_str()).collect();
    println!("project #{} trackers: {}", meta.project_id, trackers.join(", "));
    let fs = fs::RedmineFs::new(api, meta);
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
