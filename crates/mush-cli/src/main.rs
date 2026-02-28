fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    println!("mush v{}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
