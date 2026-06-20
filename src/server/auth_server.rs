//! Account auth stub (consolidates the legacy Go `audioshare_site` login).
//!
//! The iOS app gates its device-discovery/pairing screen behind a login: it
//! POSTs `{email, password}` to `/authenticateUser` (and `/createUser`) and only
//! proceeds when the JSON reply has `"success": true`. The real device session
//! is established later by the X25519 handshake on 50505 — pairing is the actual
//! security boundary (see the 2026-06 pivot in CLAUDE.md), so accounts carry no
//! value the rest of the flow uses.
//!
//! Rather than run a second binary (Go + PostgreSQL) just to answer "yes", this
//! thread serves those two endpoints from the device server itself and always
//! reports success. It is intentionally a stub: no storage, no password check.
//! If real multi-user accounts ever become a product goal, replace this with a
//! backed implementation.

use warp::Filter;

/// Port the iOS app expects the account endpoints on (matches the legacy Go
/// service's default and the app's hardcoded base URL).
const AUTH_PORT: u16 = 8080;

/// Serves `/authenticateUser` and `/createUser`, always returning success.
pub async fn start_server() {
    // The app ignores `users_id` (it mis-casts it and discards the value), but
    // the field is included so the reply matches the legacy Go shape exactly.
    let authenticate = warp::post()
        .and(warp::path("authenticateUser"))
        .and(warp::path::end())
        .map(|| warp::reply::json(&serde_json::json!({ "success": true, "users_id": 1 })));

    let create_user = warp::post()
        .and(warp::path("createUser"))
        .and(warp::path::end())
        .map(|| warp::reply::json(&serde_json::json!({ "success": true })));

    let routes = authenticate.or(create_user);

    println!("Account auth stub listening on port {}", AUTH_PORT);
    warp::serve(routes).run(([0, 0, 0, 0], AUTH_PORT)).await;
}
