use clap::Parser;
use mercury::cli::{self, Cli, Cmd};
use mercury::config::{Config, Paths};

fn main() {
    let cli = Cli::parse();
    let root = Paths::resolve();
    let paths = root.with_profile(&mercury::profiles::resolve(&root, cli.profile.as_deref()));
    let wants_gui = match &cli.command {
        Some(Cmd::Gui) => true,
        None => cli::has_display(),
        _ => false,
    };
    let result = if wants_gui { gui(paths) } else { cli::run(cli, paths) };
    if let Err(e) = result {
        eprintln!("mercury: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(feature = "gui")]
fn gui(paths: Paths) -> anyhow::Result<()> {
    paths.ensure()?;
    let config = Config::load_profile(&paths)?;
    mercury::gui::run(paths, config)
}

#[cfg(not(feature = "gui"))]
fn gui(paths: Paths) -> anyhow::Result<()> {
    let _ = Config::default();
    cli::run(Cli { profile: None, command: None }, paths)
}
