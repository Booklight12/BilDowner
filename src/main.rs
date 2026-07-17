mod api;
mod auth;
mod cli;
mod douyin;
mod download;
mod http_download;
mod mux;
#[cfg(windows)]
mod wininet;
mod x;

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

    match cli.command {
        Command::Auth { command } => {
            let store = auth::AuthStore::new()?;
            match command {
                AuthCommand::Qr => auth::qr_login(&store).await,
                AuthCommand::Set {
                    cookie,
                    cookie_file,
                } => auth::set_cookie(&store, cookie, cookie_file),
                AuthCommand::Status => auth::status(&store).await,
                AuthCommand::Clear => auth::clear(&store),
            }
        }
        Command::Info { input, page } => {
            if douyin::is_douyin_input(&input) {
                douyin::print_info(&input).await
            } else if x::is_x_input(&input) {
                x::print_info(&input).await
            } else {
                let store = auth::AuthStore::new()?;
                let client = api::BiliClient::new(store.load()?)?;
                api::print_info(&client, &input, page).await
            }
        }
        Command::Download(args) => {
            if douyin::is_douyin_input(&args.input) {
                douyin::download(args).await
            } else if x::is_x_input(&args.input) {
                x::download(args).await
            } else {
                let store = auth::AuthStore::new()?;
                let client = api::BiliClient::new(store.load()?)?;
                download::run(&client, args).await
            }
        }
    }
}
