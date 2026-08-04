//! Reading what a bearer token says about itself, for the clients that hold one.
//!
//! # This never verifies anything, and that is the design
//!
//! The signature is checked by the server and by nothing else. A client that
//! "validated" a token would be asserting a fact only the holder of the signing
//! key can establish, and the failure mode is the bad one: a client convinced a
//! token is fine stops sending the request that would have been told otherwise.
//! So every function here reads the payload as a *hint*, and a token this module
//! cannot read at all is not an error — it is simply a token nothing can be said
//! about, which is exactly how an opaque credential from some future version must
//! behave.
//!
//! What it is actually for is the `aud` claim. `mindex mint-token --for` labels a
//! token with the kind of holder it was issued to, the server enforces none of it
//! (nothing about an HTTP request identifies the process behind it), and the
//! clients honour it. That catches one specific and likely mistake — the editor's
//! credential pasted into a shell profile, the agent's into `credentials.toml` —
//! and it catches it at the client, with a sentence naming both audiences,
//! instead of at the server with a 403 about an action.
//!
//! It stops an accident, never an attacker: anything holding the token can simply
//! not run this check, and every action the token names still works. Whatever
//! must genuinely be refused belongs in `act` and `prj`, where the server decides.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

/// The audience spelling for the command-line clients — `mindex-index`,
/// `mindex-watch`, and anything else run from a shell.
///
/// A constant rather than a literal at each call site: the string has to match
/// what `mindex mint-token --for` writes, and two clients disagreeing about it
/// would refuse tokens meant for them.
pub const AUDIENCE_CLI: &str = "cli";

/// The `aud` claim, or `None` when the token does not carry one — which means
/// **every audience**, not none.
///
/// The distinction is load-bearing in the safe direction: a token minted before
/// the claim existed, or by an operator who did not label it, must keep working
/// everywhere. Reading an absent list as an empty allow-list would lock out every
/// holder on the day the field shipped.
fn audiences_of(token: &str) -> Option<Vec<String>> {
    let payload = token.trim().split('.').nth(1)?;
    let bytes = B64.decode(payload.as_bytes()).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let list = value.get("aud")?;
    // `aud` is a string or an array of strings in RFC 7519, and mindex writes the
    // array. The scalar form is accepted because a token minted by hand or by
    // another tool is still a token this client may legitimately be holding.
    match list {
        serde_json::Value::String(s) => Some(vec![s.clone()]),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// Why this client should not use `token`, as a sentence, or `None` when there is
/// no reason it can see.
///
/// Returns `None` for everything it cannot read — not a JWT, an unparseable
/// payload, no `aud` — for the reason in the module note. The refusal names both
/// audiences because the remedy depends on which token went where, and "wrong
/// audience" alone leaves the user guessing which of their two credentials is in
/// this file.
pub fn audience_refusal(token: &str, whoami: &str) -> Option<String> {
    let aud = audiences_of(token)?;
    // An `aud` that is present but empty is the same statement as an absent one.
    // Treating a `"aud": []` as "reaches nobody" would make a token that is merely
    // oddly serialized unusable, and no minting path in this project produces it.
    if aud.is_empty() || aud.iter().any(|a| a == whoami) {
        return None;
    }
    Some(format!(
        "this token was minted for {} and this is {whoami}. It is a label the server does not \
         check, so the request would very likely succeed — which is why it is refused here \
         instead: a credential in the wrong place is usually the wrong credential. Mint one \
         with `mindex mint-token --for {whoami}`, or re-mint this one without `--for`.",
        aud.join(" + "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a token-shaped string whose payload is `claims`. The signature is
    /// nonsense on purpose: nothing in this module may depend on one.
    fn token_with(claims: serde_json::Value) -> String {
        format!(
            "{}.{}.{}",
            B64.encode(br#"{"alg":"HS256","typ":"JWT","kid":"k"}"#),
            B64.encode(serde_json::to_vec(&claims).unwrap()),
            B64.encode(b"not-a-signature"),
        )
    }

    #[test]
    fn a_token_labelled_for_this_client_passes() {
        let t = token_with(serde_json::json!({ "sub": "x", "aud": ["cli", "vscode"] }));
        assert_eq!(audience_refusal(&t, AUDIENCE_CLI), None);
    }

    #[test]
    fn a_token_labelled_for_another_client_is_refused_naming_both() {
        let t = token_with(serde_json::json!({ "sub": "x", "aud": ["vscode"] }));
        let refusal = audience_refusal(&t, AUDIENCE_CLI).expect("refused");
        assert!(refusal.contains("vscode"), "{refusal}");
        assert!(refusal.contains("cli"), "{refusal}");
    }

    /// The whole backwards-compatibility story, and the one that must not
    /// regress: an unlabelled token is for everybody. Getting this backwards
    /// locks every existing holder out at once, with a message about audiences
    /// they have never heard of.
    #[test]
    fn a_token_with_no_audience_is_for_every_client() {
        for claims in [
            serde_json::json!({ "sub": "x" }),
            serde_json::json!({ "sub": "x", "aud": [] }),
        ] {
            let t = token_with(claims.clone());
            assert_eq!(
                audience_refusal(&t, AUDIENCE_CLI),
                None,
                "refused an unlabelled token: {claims}"
            );
        }
    }

    /// A client that refused what it could not parse would break the moment a
    /// server started issuing an opaque credential — and it would break with a
    /// message about audiences, which is not what happened.
    #[test]
    fn nothing_unreadable_is_ever_refused() {
        for bad in [
            "",
            "not-a-token",
            "a.b",
            "a.b.c.d",
            "x.!!!!.z",
            // Valid base64url that is not JSON, and JSON that is not an object.
            &format!("x.{}.z", B64.encode(b"plain text")),
            &format!("x.{}.z", B64.encode(b"[1,2,3]")),
            // An `aud` of a type RFC 7519 does not define.
            &format!("x.{}.z", B64.encode(br#"{"aud":7}"#)),
        ] {
            assert_eq!(
                audience_refusal(bad, AUDIENCE_CLI),
                None,
                "{bad:?} produced a refusal"
            );
        }
    }

    /// RFC 7519 allows the scalar spelling and something other than mindex may
    /// have minted the token this client holds.
    #[test]
    fn the_scalar_spelling_of_aud_is_read_as_a_one_element_list() {
        let mine = token_with(serde_json::json!({ "aud": "cli" }));
        assert_eq!(audience_refusal(&mine, AUDIENCE_CLI), None);

        let theirs = token_with(serde_json::json!({ "aud": "vscode" }));
        assert!(audience_refusal(&theirs, AUDIENCE_CLI).is_some());
    }
}
