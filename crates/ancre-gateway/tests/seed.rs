//! The seeded registry's hashes are checked, not trusted.
//!
//! `deploy/compose/init/postgres/002_seed.sql` contains two 32-byte constants
//! that Postgres cannot compute for itself: the hash of the demo API key and
//! the content hash of the demo prompt body. Both are produced by the
//! workspace hash primitive, and both are typed into a SQL file by hand.
//!
//! The failure mode if one drifts is quiet and expensive. A wrong key hash
//! authenticates nothing, and the symptom is a 401 that reads as "the
//! quickstart is broken" ten minutes into someone's first evaluation. A wrong
//! prompt hash makes `GET /v1/prompts/{hash}` 404 for a prompt every event in
//! the chain pins — the digest is there and the thing it is a digest of cannot
//! be produced, which is the exact failure an evidence system exists to
//! prevent.
//!
//! So this test recomputes both from the same functions the gateway calls, and
//! reads the prompt body out of the SQL rather than from a copy kept here: a
//! second copy of the body would drift from the first, and the test would
//! prove that the copy is consistent with itself.

use std::path::PathBuf;

const DEMO_KEY: &str = "ancre-demo-key";

fn seed_sql() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/compose/init/postgres/002_seed.sql");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every `decode('<hex>', 'hex')` literal in the file, in order.
fn hex_literals(sql: &str) -> Vec<String> {
    sql.match_indices("decode('")
        .filter_map(|(i, m)| {
            let rest = &sql[i + m.len()..];
            rest.find('\'').map(|end| rest[..end].to_string())
        })
        .collect()
}

/// The single-quoted argument to `convert_to(...)`, with SQL's doubled quotes
/// unescaped back to the bytes Postgres will actually store.
fn convert_to_argument(sql: &str) -> String {
    let start = sql.find("convert_to(").expect("no convert_to( in the seed");
    let rest = &sql[start..];
    let open = rest.find('\'').expect("convert_to with no string argument") + 1;
    let body = &rest[open..];

    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
        } else if chars.peek() == Some(&'\'') {
            chars.next();
            out.push('\'');
        } else {
            return out;
        }
    }
    panic!("unterminated string literal in the seed");
}

#[test]
fn the_seeded_key_hash_is_the_hash_the_gateway_will_compute() {
    let sql = seed_sql();
    let expected = ancre_gateway::auth::key_hash(DEMO_KEY).to_hex();

    assert!(
        hex_literals(&sql).contains(&expected),
        "no decode('{expected}','hex') in 002_seed.sql — the demo key's hash has \
         drifted from `auth::key_hash({DEMO_KEY:?})`, so the seeded key \
         authenticates nothing. Regenerate it with:\n  \
         cargo run -p ancre-gateway --example key-hash -- {DEMO_KEY}"
    );
}

#[test]
fn the_seeded_prompt_hashes_to_the_key_it_is_stored_under() {
    let sql = seed_sql();
    let body = convert_to_argument(&sql);
    let expected = ancre_canon::hash_bytes(body.as_bytes()).to_hex();

    assert!(
        hex_literals(&sql).contains(&expected),
        "the seeded prompt body hashes to {expected}, which appears nowhere in \
         002_seed.sql. The body and the hash it is filed under have diverged, \
         so `GET /v1/prompts/{{hash}}` will 404 for a prompt the chain pins"
    );
}

/// `prompt_version` is the human-facing name of the same hash, and the two
/// live in different columns of the same row. A route whose `prompt_version`
/// names one prompt and whose `prompt_hash` names another puts a pin in every
/// event that disagrees with the body an auditor is served.
#[test]
fn the_seeded_prompt_version_names_the_seeded_prompt_hash() {
    let sql = seed_sql();
    let hash = ancre_canon::hash_bytes(convert_to_argument(&sql).as_bytes()).to_hex();
    let expected = format!("b3:{}", &hash[..12]);

    assert!(
        sql.contains(&format!("'{expected}'")),
        "the seeded routes should pin prompt_version '{expected}', derived from \
         the body's own hash"
    );
}
