#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::error::Error;

use mirzadev_onecenter::{app, logging, runtime::AsyncRuntime};

fn main() -> Result<(), Box<dyn Error>> {
    logging::init()?;
    let runtime = AsyncRuntime::new()?;
    let result = app::run(runtime.handle().clone());
    runtime.shutdown();
    result?;
    Ok(())
}
