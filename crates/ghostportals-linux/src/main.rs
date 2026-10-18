#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod proxy;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    daemon::main().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ghostportals-linux only runs on Linux");
    std::process::exit(1);
}
