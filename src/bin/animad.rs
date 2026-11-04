use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use anima::backend::wayland::{self, RunOptions};
use anima::ipc::{self, Request};
use anima::util;

#[derive(Parser, Debug)]
#[command(
    name = "animad",
    version,
    about = "Anima live wallpaper daemon (Wayland background layer)"
)]
struct Args {
    /// Path to config.toml (defaults to ~/.config/anima/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override the configured frame rate.
    #[arg(long)]
    fps: Option<u32>,

    /// Enable debug logging.
    #[arg(long, short)]
    verbose: bool,

    /// Stop an already running daemon and take over.
    #[arg(long)]
    replace: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let filter = if args.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter))
        .format_timestamp_millis()
        .init();

    let socket = util::socket_path();
    if ipc::request(&socket, &Request::Ping).is_ok() {
        if args.replace {
            let _ = ipc::request(&socket, &Request::Quit);
            std::thread::sleep(std::time::Duration::from_millis(400));
        } else {
            eprintln!(
                "animad is already running (socket {}). Use --replace to restart.",
                socket.display()
            );
            std::process::exit(1);
        }
    }

    wayland::run(RunOptions {
        config_path: args.config,
        fps: args.fps,
        verbose: args.verbose,
    })
}
