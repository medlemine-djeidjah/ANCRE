fn main() {
    let mut events = ancre_chain::fixtures::chain(50_000);
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "tamper" {
        events[41_206].emitted.metrics.http_status = 500;
    } else if mode == "remove" {
        events.remove(999);
    }
    for e in &events {
        println!("{}", serde_json::to_string(e).unwrap());
    }
}
