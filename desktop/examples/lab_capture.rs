//! Drive the native lab for documentation screenshots without opening a desktop window.
//! Run with `cargo run -p podbag-desktop --example lab_capture`. Send one JSON
//! object per line, e.g. `{"action":"start"}`. Closing stdin ends the lab.
use podbag_desktop_core::lab;

use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead};

#[derive(Deserialize)]
struct Input {
    action: lab::Action,
    value: Option<String>,
}

fn apply(
    lab: &mut lab::Lab,
    line: &str,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let input: Input = serde_json::from_str(line)?;
    Ok(json!({ "state": lab.run(input.action, input.value.as_deref())? }))
}

fn main() -> io::Result<()> {
    let mut lab = lab::Lab::default();
    for line in io::stdin().lock().lines() {
        let response =
            apply(&mut lab, &line?).unwrap_or_else(|e| json!({ "error": e.to_string() }));
        println!("{response}");
    }
    Ok(())
}
