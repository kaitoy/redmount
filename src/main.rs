mod fs;
mod markdown;
mod redmine;

use anyhow::Context;
use clap::Parser;
use winfsp::host::{FileSystemHost, VolumeParams};

/// Mount a Redmine project as a drive (REDMINE_API_KEY env var = your API key).
#[derive(Parser)]
struct Args {
    /// e.g. https://redmine.example.com
    #[arg(long)]
    url: String,
    /// project identifier
    #[arg(long)]
    project: String,
    #[arg(long, default_value = "R:")]
    mount: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let key = std::env::var("REDMINE_API_KEY").context("Please set the REDMINE_API_KEY environment variable")?;
    let api = redmine::Api::new(&args.url, &key)?;
    let meta = api.meta(&args.project)?;
    let trackers: Vec<_> = meta.trackers.iter().map(|t| t.name.as_str()).collect();
    println!("project #{} trackers: {}", meta.project_id, trackers.join(", "));

    // winfsp_init loads winfsp-x64.dll from the DLL search path.
    // ponytail: assumes the default WinFsp install dir; read HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir if it moves
    let bin = format!(r"{}\WinFsp\bin", std::env::var("ProgramFiles(x86)").unwrap_or_default());
    // SAFETY: still single-threaded here.
    unsafe { std::env::set_var("PATH", format!("{bin};{}", std::env::var("PATH").unwrap_or_default())) };
    let _init = winfsp::winfsp_init().context("Failed to load WinFsp. Please install WinFsp")?;
    let mut vp = VolumeParams::new();
    vp.sector_size(4096)
        .sectors_per_allocation_unit(1)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .post_cleanup_when_modified_only(true)
        // file contents change on the server; don't let the kernel serve what we wrote last time
        .flush_and_purge_on_cleanup(true)
        .file_info_timeout(1000)
        .filesystem_name("redmount");
    let mut host: FileSystemHost<fs::RedmineFs> = FileSystemHost::new(vp, fs::RedmineFs::new(api, meta)?)?;
    host.mount(args.mount.as_str())?;
    host.start()?;
    println!("Mounted on {}. Press Enter to unmount.", args.mount);
    if std::io::stdin().read_line(&mut String::new())? == 0 {
        // no console (stdin closed): stay mounted until killed
        loop {
            std::thread::park();
        }
    }
    host.stop();
    host.unmount();
    Ok(())
}
