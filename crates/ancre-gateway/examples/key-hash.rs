//! Print the `api_keys.key_hash` for an API key.
//!
//! The registry stores the hash and never the key, so populating it needs this
//! — and it calls `auth::key_hash`, the same function the gateway calls on
//! every request. That is the whole point of it being here rather than a line
//! of shell: the hash is the workspace primitive, BLAKE3 by default and
//! SHA-256 under `hash-sha256`, so a hand-rolled `sha256sum` silently produces
//! a key that authenticates nothing.
//!
//! ```sh
//! cargo run -p ancre-gateway --example key-hash -- demo-key
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(key) = args.next() else {
        eprintln!("usage: key-hash <api-key>");
        eprintln!();
        eprintln!("Prints the hex key_hash to insert into api_keys:");
        eprintln!(
            "  INSERT INTO api_keys (key_hash, tenant_id, system_id)\n    \
             VALUES (decode('<hash>','hex'), 'acme', 'hr-screening');"
        );
        std::process::exit(2);
    };

    println!("{}", ancre_gateway::auth::key_hash(&key).to_hex());
}
