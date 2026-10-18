//! Encodes and decodes a test pattern with this machine's codecs.
//!
//!   cargo run -p ghostportals-media --example selftest

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    println!("{}", ghostportals_media::selftest(3)?);
    Ok(())
}
