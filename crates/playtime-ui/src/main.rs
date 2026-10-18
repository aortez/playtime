use clap::Parser;
use playtime_client::config::Options;
use playtime_ui::ui::Workbench;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Native Playtime workbench; connects to an existing authenticated service"
)]
struct Args {
    #[arg(long, conflicts_with_all = ["endpoint", "token_file"])]
    connection_file: Option<PathBuf>,
    #[arg(long, requires = "token_file")]
    endpoint: Option<String>,
    #[arg(long, requires = "endpoint")]
    token_file: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}
fn main() -> eframe::Result {
    let args = Args::parse();
    let options = Options {
        connection_file: args.connection_file,
        endpoint: args.endpoint,
        token_file: args.token_file,
        config: args.config,
        state_dir: args.state_dir,
    };
    eframe::run_native(
        "Playtime",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1440.0, 900.0])
                .with_min_inner_size([900.0, 600.0]),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(Workbench::desktop(cc.egui_ctx.clone(), options)))),
    )
}
