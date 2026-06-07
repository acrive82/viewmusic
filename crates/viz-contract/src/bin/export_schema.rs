//! Exports the published artifact JSON Schema (generated from the typed model).
//!
//! Writes [`viz_contract::published_schema`] as pretty-printed JSON to the file
//! named by the first CLI argument, or to stdout when no argument is given.

use std::io::Write;

fn main() -> std::io::Result<()> {
    let schema = viz_contract::published_schema();
    let pretty = serde_json::to_string_pretty(schema).expect("schema serializes to JSON");

    match std::env::args().nth(1) {
        Some(path) => {
            std::fs::write(&path, pretty.as_bytes())?;
            eprintln!("wrote published artifact schema to {path}");
        }
        None => {
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle.write_all(pretty.as_bytes())?;
            handle.write_all(b"\n")?;
        }
    }
    Ok(())
}
