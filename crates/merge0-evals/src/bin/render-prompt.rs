//! Render the exact agent-facing work-order JSON the generated workflow
//! feeds to `claude -p` — through the REAL sanitizer, so an order that
//! would be refused for credential markers is refused here too.
//!
//! ```sh
//! cargo run -p merge0-evals --bin render-prompt -- evals/fixtures/districts/work-order.json
//! ```

use merge0_runner::sanitized_payload;
use merge0_signal::WorkOrder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: render-prompt <work-order.json>")?;
    let order: WorkOrder = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let payload = sanitized_payload(&order, "http://localhost:8080/runner/callback", None)?;
    println!("{}", serde_json::to_string_pretty(&payload["work_order"])?);
    Ok(())
}
