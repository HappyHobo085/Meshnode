//! `Authorization: AegisSig {account}.{token hex}.{sig hex}`: header parsing, the core's
//! stateless `check_token` at the WASI wall clock, the route's admit rule, then the replay
//! nonce, in the native `verify_auth` order.
use aegis_sync_core::auth::{AuthToken, MAX_TTL_MS, check_token, unhex};
use aegis_sync_core::devices::is_member;

use crate::keys;
use crate::meshnode::kv::local;
use crate::store;

/// An authenticated caller.
pub struct Caller {
    pub account: String,
    #[allow(dead_code)] // read by the devices endpoints (Task 8)
    pub device: String,
}

/// The node's wall clock (`wasi:clocks/wall-clock`) in ms since the UNIX epoch.
pub fn now_ms() -> i64 {
    let t = wasip2::clocks::wall_clock::now();
    let ms = u128::from(t.seconds) * 1000 + u128::from(t.nanoseconds / 1_000_000);
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Authenticates `header` and runs `admit(account, device)`; the `Err` is the status to
/// answer with (`401`, the admit rule's own, or `429` when the nonce store is full).
///
/// The nonce is recorded only AFTER `admit` passes: anyone can mint a device key and sign a
/// token with it, so spending nonce slots for callers about to be refused would let a stranger
/// fill the store (the native comment on `verify_auth` explains the attack).
pub fn authenticate(
    header: Option<&str>,
    admit: impl FnOnce(&str, &str) -> Result<(), u16>,
) -> Result<Caller, u16> {
    let rest = header
        .and_then(|h| h.strip_prefix("AegisSig "))
        .ok_or(401u16)?;
    let mut parts = rest.splitn(3, '.');
    let (Some(account), Some(token_hex), Some(sig_hex)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(401);
    };
    let token: AuthToken = unhex(token_hex)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(401u16)?;
    if token.account_id != account {
        return Err(401);
    }
    let now = now_ms();
    let ok = check_token(&token, now, sig_hex).map_err(|_| 401u16)?;
    admit(&ok.account_id, &ok.device_id)?;

    // Replay defense: a (device, nonce) pair is single-use on this node. The entry must
    // outlive the token: `check_token` admits a token until `now > expires_ms`, and a token
    // issued up to MAX_FUTURE_SKEW_MS ahead can expire more than MAX_TTL_MS from now. So the
    // ttl is MAX_TTL_MS, extended to just past `expires_ms` when that is later.
    let key = keys::nonce_key(&ok.device_id, &ok.nonce);
    if local::get(&key).is_some() {
        return Err(401);
    }
    let ttl = ok
        .expires_ms
        .saturating_sub(now)
        .saturating_add(1)
        .max(MAX_TTL_MS);
    // A refused write is the store's cap (100 000 live keys): refuse the request rather than
    // admit an unrecorded nonce, as the native nonce-map cap does (`429`).
    local::set(&key, b"1", ttl as u64).map_err(|_| 429u16)?;
    Ok(Caller {
        account: ok.account_id,
        device: ok.device_id,
    })
}

/// The data endpoints' admit rule (the native `require_registered`): only a member of the
/// account (registered, not pending, not revoked) gets through; anyone else is a `403`.
pub fn member(account: &str, device: &str) -> Result<(), u16> {
    if is_member(&store::account_facts(account), device) {
        Ok(())
    } else {
        Err(403)
    }
}
