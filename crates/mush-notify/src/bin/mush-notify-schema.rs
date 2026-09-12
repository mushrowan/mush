fn main() -> Result<(), serde_json::Error> {
    serde_json::to_writer_pretty(
        std::io::stdout(),
        &schemars::schema_for!(mush_notify::Config),
    )
}
