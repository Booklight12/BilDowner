mod api;
mod auth;
mod cli;
mod download;

use anyhow::Result;
use clap::Parser;
use cli::{AuthCommand, Cli, Command};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = auth::AuthStore::new()?;

    match cli.command {
        Command::Auth { command } => match command {
            AuthCommand::Qr => auth::qr_login(&store).await,
            AuthCommand::Set {
                cookie,
                cookie_file,
            } => auth::set_cookie(&store, cookie, cookie_file),
            AuthCommand::Status => auth::status(&store).await,
            AuthCommand::Clear => auth::clear(&store),
        },
        Command::Info { input, page } => {
            let client = api::BiliClient::new(store.load()?)?;
            api::print_info(&client, &input, page).await
        }
        Command::Download(args) => {
            let client = api::BiliClient::new(store.load()?)?;
            download::run(&client, args).await
        }
    }
}
