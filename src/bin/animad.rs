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
    use_compositor_gpu();
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

/// On hybrid laptops a shell may force NVIDIA offloading for everything. Our
/// overlay is redrawn every frame and the compositor usually runs on the iGPU,
/// so rendering on the dGPU means a full-screen copy between GPUs per frame.
/// Stay on Mesa unless ANIMA_GPU=nvidia.
fn use_compositor_gpu() {
    if std::env::var("ANIMA_GPU").is_ok_and(|v| v == "nvidia") {
        return;
    }
    let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if !std::path::Path::new(mesa).exists() {
        return;
    }
    // still single-threaded here
    unsafe {
        std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
        std::env::remove_var("__NV_PRIME_RENDER_OFFLOAD");
        std::env::remove_var("__GLX_VENDOR_LIBRARY_NAME");
    }
}
