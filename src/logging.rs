pub fn init() -> Result<(), log::SetLoggerError> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.format_timestamp_secs();
    builder.try_init()
}
