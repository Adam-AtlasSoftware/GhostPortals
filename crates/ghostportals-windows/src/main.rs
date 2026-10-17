#[cfg(windows)]
mod daemon;
#[cfg(windows)]
mod win32;

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    daemon::main().await
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ghostportals-windows only runs on Windows");
    std::process::exit(1);
}
