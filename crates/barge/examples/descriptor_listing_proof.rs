//! Narrow argv adapter for a separately activated targeted kernel proof.
//! The feature-gated library endpoint owns every observation and decision.

use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("descriptor listing proof: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [command, pid, root, rows, remaining_ms] = args.as_slice() else {
        return Err("expected inspect PID ROOT MAX_ROWS REMAINING_MS".into());
    };
    if command != "inspect" {
        return Err("expected inspect command".into());
    }
    let report = barge::os::MacOs.proof_descriptor_listing(
        pid.parse()?,
        Path::new(root),
        rows.parse()?,
        remaining_ms.parse()?,
    )?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
