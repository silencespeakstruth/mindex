//! Scoped bearer tokens: what a caller may touch travels inside the credential.
//!
//! # Why this exists at all
//!
//! Until this module, mindex authenticated nothing: TLS was the only transport
//! security and a deployment needing a credential put an nginx gate in front that
//! checked a shared `X-Api-Key`. The gate still runs and is still what stands
//! between this server and the open internet, but it no longer checks a key —
//! that credential is gone, because what it could never do is *authorize*. A key
//! that passed it reached every project, since `GET /projects` enumerates them in
//! a response **body**, which no proxy can filter without parsing, and since a
//! project GUID is a bearer identifier — holding one grants that project's whole
//! data plane. Keeping it beside the token would also have meant an agent handed
//! a token still needed the shared secret, which is the leak this closes.
//!
//! The second problem was the one that shaped this design. A credential is only
//! ever unsafe to paste into a model's context because of what it can do; an agent
//! handed a URL and `/llms.txt` has no process holding a key for it, so the only
//! way it can send one is for a human to type it into a chat. Making that
//! acceptable means making the credential narrow and expiring **by construction**,
//! which is what a signed token is: the allowed projects and the allowed actions
//! ride inside it, signed, so the server answers from the token alone.
//!
//! That is why there is no schema change anywhere near this feature. The token
//! *is* the mapping. An earlier design put a `tenant_id` column on `projects` and
//! it needed a table rebuild, a trigger pinning one tenant per GUID, an in-process
//! cache, a startup warm, a rule for pre-existing rows and an in-transaction
//! re-read to stop two callers racing for ownership through
//! `ON CONFLICT DO NOTHING`. None of it survives here, and one bug class goes with
//! it: only a caller whose token already names a GUID can create that project, so
//! `POST /index` stops being an existence oracle.
//!
//! # The invariant this breaks, deliberately
//!
//! `CLAUDE.md`, the OpenAPI description and `llms_doc.md` all said the server
//! authenticates nothing. With `[auth].enabled` that is no longer true, and those
//! passages were rewritten rather than left to rot. The break is bounded and the
//! bound is the point: there is no user table, no password, no session, no
//! server-side state of any kind behind a request. One signature check, and every
//! fact the decision needs is in the token.
//!
//! # Choices a reader will want justified
//!
//! **HS256, not a public-key algorithm.** The same process mints and verifies, so
//! asymmetry buys nothing and costs a key format. The TLS certificate's key is
//! deliberately *not* reused: a key serving two protocols is a well-known
//! anti-pattern, and the certificate rotates on a schedule that has nothing to do
//! with token lifetimes.
//!
//! **Written here rather than taken from a JWT crate.** Not invented-here: the
//! requirement was that the secret's handling in memory be controlled, and a
//! library that copies the key into its own `EncodingKey`/`DecodingKey` puts a
//! copy beyond reach of any zeroing this module can do. Owning the ~100 lines
//! means owning every byte. It also closes the algorithm-confusion family by
//! construction rather than by configuration — see [`verify`], which never reads
//! `alg` to decide anything.
//!
//! **Revocation is by expiry or by deleting a `kid`.** A signed token is valid
//! until it expires; that is JWT's one real weakness and it must not be papered
//! over. The keyring holds several keys by id, so a guest minted under its own
//! `kid` is revoked by deleting one line, without touching the working tokens. A
//! `jti` denylist is the next rung and is deliberately not built: it reintroduces
//! exactly the per-request state this design removed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit as _, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

/// The `prj` value that means "every project". Legal, and it must be spelled:
/// an empty list is not a wildcard, it is a token that can reach nothing. The
/// distinction is what stops a mis-built minter handing out full access by
/// omission, and it is pinned by a test.
pub const WILDCARD_PROJECT: &str = "*";

/// Issuer, checked on every token. A constant rather than a config key: it
/// identifies the software, not the deployment.
const ISSUER: &str = "mindex";

/// Bytes in a generated signing secret. 256 bits, matching the HMAC's output —
/// a longer secret is folded by the block padding and buys nothing.
const SECRET_BYTES: usize = 32;

/// Refuses a token whose serialized form is absurd before any parsing work.
/// A JWT carrying a project list stays well under this; the bound exists so a
/// malicious `Authorization` header cannot make the server allocate.
const MAX_TOKEN_BYTES: usize = 8 * 1024;

type HmacSha256 = Hmac<Sha256>;

// ─── Actions ─────────────────────────────────────────────────────────────────

/// What a token permits. A closed set, because every value is a label the server
/// defines and a token is client-supplied: an open vocabulary here would be an
/// unbounded label reaching the policy table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Reads over indexed content: search, symbols, file and project inventory,
    /// and the read-only drift check.
    Search,
    /// Running research and challenges, and browsing the stored corpus.
    Research,
    /// Anything that writes chunks: indexing, history, cancel, retry.
    Index,
    /// Anything that destroys: files, projects, history, stored runs.
    Delete,
    /// The global operator surfaces — `/gc`, `/status`, `/metrics`.
    ///
    /// Global by construction rather than by choice, which is why there is no
    /// separate `gc` action: `POST /gc` holds the process-wide `GcGuard`, walks
    /// every collection and names other callers' collections in `failed_phases`.
    /// A project list cannot describe it, so scoping it per project would be a
    /// promise the endpoint cannot keep.
    Admin,
    /// Minting further tokens. Held apart from `admin` so a deployment can hand
    /// out an issuing credential without handing out `/gc` — and constrained by
    /// [`Claims::may_mint`] so it can never widen what its holder already has.
    Mint,
}

impl Action {
    /// Every action, for validation and for the discovery documents.
    pub const ALL: &'static [Action] = &[
        Action::Search,
        Action::Research,
        Action::Index,
        Action::Delete,
        Action::Admin,
        Action::Mint,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Search => "search",
            Action::Research => "research",
            Action::Index => "index",
            Action::Delete => "delete",
            Action::Admin => "admin",
            Action::Mint => "mint",
        }
    }

    /// Parses the spelling used on the wire and on the `mint-token` command line.
    pub fn parse(s: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.as_str() == s)
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Audience ────────────────────────────────────────────────────────────────

/// The kind of holder a token was minted for.
///
/// # This is not an authority axis, and the distinction is the whole design
///
/// `act` and `prj` say what a credential may *do*; `aud` says who should be
/// *holding* it. The server cannot check the second one and does not pretend to:
/// nothing about an HTTP request identifies the process behind it, a `User-Agent`
/// is a client-supplied string, and a token is a bearer credential by
/// construction. So `aud` is enforced **by the client that reads it** — the VS
/// Code extension refuses to use a token minted for an agent, and says why.
///
/// What that buys is real but narrow: it catches the mistake of pasting the
/// wrong credential into the wrong place, which is the likeliest way one of these
/// leaks. It stops an accident, never an attacker — an attacker holding the token
/// simply does not run the check, and every action it names still works. Anything
/// that must actually be refused belongs in `act` or `prj`, where the server
/// decides.
///
/// Two consequences follow, and both are pinned by tests:
///
/// - **An empty list means every audience**, not none. Backwards compatibility is
///   the smaller half; the larger is that a token nobody labelled must keep
///   working everywhere, or a label added later silently locks holders out.
/// - **[`Claims::may_mint`] does not contain it.** A wider audience is not more
///   authority, and the primary use of delegation is a person minting an *agent*
///   token from their *editor* token — containment here would refuse exactly the
///   thing the mechanism exists for.
///
/// There is deliberately no `Claims::intended_for` here. A predicate on this side
/// would have no production caller and would read, to anyone finding it later, as
/// a check the server performs. The clients hold their own — `mindexfile`'s
/// `audience_refusal` for the Rust CLIs, `token.ts`'s `audienceRefusal` for the
/// extension — because the client is the only party that knows what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// The command-line clients and anything else run from a shell:
    /// `mindex-index`, `mindex-watch`, `mindex-search.sh`, the post-commit hook.
    Cli,
    /// The VS Code extension.
    Vscode,
    /// A model given the credential directly — the MCP servers, and anything
    /// pasted into a context. The class the whole scoping mechanism exists for.
    Agent,
}

impl Audience {
    pub const ALL: &'static [Audience] = &[Audience::Cli, Audience::Vscode, Audience::Agent];

    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Cli => "cli",
            Audience::Vscode => "vscode",
            Audience::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Audience> {
        Audience::ALL.iter().copied().find(|a| a.as_str() == s)
    }
}

impl std::fmt::Display for Audience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Claims ──────────────────────────────────────────────────────────────────

/// The signed payload. Short names because it travels in a header on every
/// request, and because `iss`/`sub`/`exp` are the registered JWT spellings — a
/// token this server issues stays readable by any ordinary JWT debugger, which is
/// worth more than prose field names in a credential nobody edits by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Always [`ISSUER`]; checked, so a token minted by something else that
    /// happens to share the secret is still refused.
    pub iss: String,
    /// Free-text label naming the holder, for the operator's benefit — it appears
    /// in logs and in `mint-token`'s output and is never used for a decision.
    pub sub: String,
    /// Unique id. Not consulted today (there is no denylist, by design); it is
    /// here because adding one later must not require re-minting every token.
    pub jti: String,
    pub iat: u64,
    pub nbf: u64,
    /// Absent means **no expiry**, and that is a deliberate, narrow escape
    /// hatch rather than an oversight.
    ///
    /// It exists for machine-local credentials that no context ever sees and
    /// that nothing would renew: the metrics scraper is the motivating case, and
    /// an expiry that silently blanks every dashboard at 3am is worse than the
    /// exposure it prevents. Everything else must expire — with no denylist by
    /// design, `exp` is the main bound on a leak.
    ///
    /// Mintable **only by the local `mint-token` command**, never over
    /// `POST /auth/tokens`: a network-reachable way to issue an eternal
    /// credential is a different and much worse thing than a local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
    /// Simple-form (dashless) project GUIDs, or exactly `["*"]`.
    pub prj: Vec<String>,
    pub act: Vec<Action>,
    /// Which kinds of holder this token is for. **Empty means every kind** — see
    /// [`Audience`] for why that default is the safe one and why the server
    /// enforces none of it.
    ///
    /// `aud` is the registered JWT spelling and is reused deliberately: an
    /// ordinary debugger already renders it, and a bespoke `for` claim would say
    /// the same thing in a dialect nothing else reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aud: Vec<Audience>,
}

impl Claims {
    /// Whether this token reaches `guid`.
    ///
    /// Compares the **simple** form on both sides, so the dashed and dashless
    /// spellings of one GUID cannot disagree — the server already treats them as
    /// one project, and a scope check that did not would refuse access to a
    /// project the caller demonstrably owns depending on how a client wrote it.
    pub fn covers(&self, guid: &Uuid) -> bool {
        let simple = guid.simple().to_string();
        self.prj
            .iter()
            .any(|p| p == WILDCARD_PROJECT || p.eq_ignore_ascii_case(&simple))
    }

    /// Whether this token permits `action`.
    pub fn permits(&self, action: Action) -> bool {
        self.act.contains(&action)
    }

    /// Whether this token reaches every project.
    pub fn is_wildcard(&self) -> bool {
        self.prj.iter().any(|p| p == WILDCARD_PROJECT)
    }

    /// Whether this token may mint `wanted`, and the containment rule that makes
    /// `mint` safe to delegate.
    ///
    /// A minter may never issue more than it holds — not a wider action set, not
    /// a wider project list, not a later expiry. Without this, handing someone a
    /// read-only `mint` token would hand them `admin` one call later, which is
    /// the whole privilege-escalation shape `mint` invites.
    pub fn may_mint(&self, wanted: &Claims) -> Result<(), AuthError> {
        if !self.permits(Action::Mint) {
            return Err(AuthError::ActionNotPermitted(Action::Mint));
        }
        if let Some(a) = wanted.act.iter().find(|a| !self.permits(**a)) {
            return Err(AuthError::MintWouldExceedMinter(format!(
                "the requested action `{a}` is not held by the minting token"
            )));
        }
        if !self.is_wildcard() {
            if wanted.is_wildcard() {
                return Err(AuthError::MintWouldExceedMinter(
                    "the minting token is scoped to named projects and cannot issue a wildcard"
                        .to_string(),
                ));
            }
            if let Some(p) = wanted
                .prj
                .iter()
                .find(|p| !self.prj.iter().any(|h| h.eq_ignore_ascii_case(p)))
            {
                return Err(AuthError::MintWouldExceedMinter(format!(
                    "project {p} is not held by the minting token"
                )));
            }
        }
        // `aud` is deliberately absent from this rule. It is not authority — it
        // names the intended holder — and delegation is a change of holder by
        // definition: the motivating case is a person minting an `agent` token
        // from their `vscode` one, which containment would refuse outright.
        //
        // An eternal minter may issue anything; an expiring one may not issue a
        // token outliving itself — and "no expiry at all" is the extreme case of
        // outliving it, not an exemption from the rule.
        if let Some(mine) = self.exp {
            match wanted.exp {
                None => {
                    return Err(AuthError::MintWouldExceedMinter(
                        "the minting token expires, so it cannot issue one that never does"
                            .to_string(),
                    ));
                }
                Some(w) if w > mine => {
                    return Err(AuthError::MintWouldExceedMinter(
                        "the requested expiry is later than the minting token's own".to_string(),
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Why a token was refused. Converted to `ApiError` at the extractor, which is
/// where the decision about what a *client* is told lives — this enum is allowed
/// to be specific because it also feeds the operator's log.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("the token is malformed: {0}")]
    Malformed(&'static str),
    #[error("the token's signature does not verify")]
    BadSignature,
    #[error("the token names key id {0:?}, which this server does not hold")]
    UnknownKeyId(String),
    #[error("the token expired")]
    Expired,
    #[error("the token is not valid yet")]
    NotYetValid,
    #[error("the token does not permit `{0}`")]
    ActionNotPermitted(Action),
    #[error("{0}")]
    MintWouldExceedMinter(String),
    #[error("{0}")]
    InvalidProject(String),
}

// ─── The secret ──────────────────────────────────────────────────────────────

/// One signing secret.
///
/// The `Debug` impl is hand-written and prints nothing but a marker. That is the
/// load-bearing half of "handle the secret safely in memory": a derived `Debug`
/// would put the bytes into any `error = ?e` that ever carried a struct holding
/// one, and this codebase logs errors as fields everywhere by convention.
///
/// [`Zeroizing`] wipes the bytes on drop. On the server that is worth less than
/// it looks — the keyring lives as long as the process — but it is genuinely
/// load-bearing in the two places a secret is short-lived: the `mint-token`
/// command, and a key dropped from the ring during rotation.
///
/// What is deliberately **not** done is `mlock`. Keeping one copy out of swap
/// while the kernel is free to page the rest of the process is a partial measure
/// that reads as a complete one; if the threat model ever includes swap, it wants
/// an encrypted swap device, not a syscall here.
pub struct SigningSecret(Zeroizing<Vec<u8>>);

impl std::fmt::Debug for SigningSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningSecret(<redacted>)")
    }
}

impl SigningSecret {
    fn mac(&self) -> HmacSha256 {
        // `new_from_slice` on HMAC accepts any length: the construction hashes an
        // over-long key and zero-pads a short one, so this cannot fail here.
        HmacSha256::new_from_slice(&self.0).expect("HMAC accepts a key of any length")
    }
}

// ─── The keyring ─────────────────────────────────────────────────────────────

/// The on-disk key file. TOML, `deny_unknown_fields` for the reason every other
/// format in this repository uses it: a mistyped key here is a security setting
/// that silently did not apply.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyFileOnDisk {
    /// Which key id new tokens are signed with. Every key in `keys` still
    /// verifies — that asymmetry is what makes rotation possible without
    /// invalidating tokens already in the field.
    active: String,
    keys: BTreeMap<String, KeyEntryOnDisk>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyEntryOnDisk {
    /// Base64url, no padding.
    secret: String,
    /// Free-text note for the operator: what this key id is for, so the file
    /// still explains itself when the decision to revoke has to be made months
    /// later. Never read by the code.
    #[serde(default)]
    note: String,
}

/// Every key this server will verify, and the one it signs with.
pub struct Keyring {
    keys: BTreeMap<String, SigningSecret>,
    active: String,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .field("active", &self.active)
            .finish()
    }
}

impl Keyring {
    /// Loads the key file, creating it with one fresh key when absent.
    ///
    /// Creation is `O_EXCL` at mode 0600, so two servers starting at once cannot
    /// both believe they wrote the file — the loser reads what the winner wrote
    /// rather than silently signing with a secret the other will refuse.
    pub fn load_or_create(path: &Path) -> Result<Keyring, KeyFileError> {
        if !path.exists() {
            create_key_file(path)?;
        }
        Self::load(path)
    }

    /// Loads the key file, refusing one any other account can read.
    pub fn load(path: &Path) -> Result<Keyring, KeyFileError> {
        refuse_if_readable_by_others(path)?;

        let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
            KeyFileError::Unreadable {
                path: path.to_path_buf(),
                source: e,
            }
        })?);
        let parsed: KeyFileOnDisk =
            toml::from_str(&text).map_err(|e| KeyFileError::Unparseable {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;

        if parsed.keys.is_empty() {
            return Err(KeyFileError::Invalid {
                path: path.to_path_buf(),
                detail: "the file holds no keys; delete it and restart to have one generated"
                    .to_string(),
            });
        }
        if !parsed.keys.contains_key(&parsed.active) {
            return Err(KeyFileError::Invalid {
                path: path.to_path_buf(),
                detail: format!(
                    "active = {:?} names no key in this file; every token would fail to sign",
                    parsed.active
                ),
            });
        }

        let mut keys = BTreeMap::new();
        for (kid, entry) in parsed.keys {
            let raw = B64
                .decode(entry.secret.as_bytes())
                .map_err(|_| KeyFileError::Invalid {
                    path: path.to_path_buf(),
                    detail: format!("the secret for key {kid:?} is not base64url"),
                })?;
            if raw.len() < 16 {
                return Err(KeyFileError::Invalid {
                    path: path.to_path_buf(),
                    detail: format!(
                        "the secret for key {kid:?} is {} bytes; at least 16 are required",
                        raw.len()
                    ),
                });
            }
            keys.insert(kid, SigningSecret(Zeroizing::new(raw)));
        }

        Ok(Keyring {
            keys,
            active: parsed.active,
        })
    }

    /// Builds a ring in memory, for tests.
    #[cfg(test)]
    pub fn from_secret(kid: &str, secret: Vec<u8>) -> Keyring {
        let mut keys = BTreeMap::new();
        keys.insert(kid.to_string(), SigningSecret(Zeroizing::new(secret)));
        Keyring {
            keys,
            active: kid.to_string(),
        }
    }

    /// The key ids this ring verifies, for the operator-facing log line at
    /// startup. Ids are not secret; the point of printing them is that a token
    /// refused for an unknown `kid` can be diagnosed without reading the file.
    pub fn key_ids(&self) -> Vec<&str> {
        self.keys.keys().map(String::as_str).collect()
    }
}

/// Adds a freshly generated key under `kid`, leaving `active` and every existing
/// key alone.
///
/// The revocation story rests on per-holder key ids — deleting one line withdraws
/// one credential and nothing else — and that story is only real if creating an
/// id is one flag. Requiring an operator to hand-edit base64 into a TOML is how a
/// mechanism ends up documented and unused.
///
/// Refuses an id that already exists rather than replacing it: overwriting a key
/// silently invalidates every token signed under it, which is the destructive
/// half of rotation performed by a typo.
pub fn add_key(path: &Path, kid: &str, note: &str) -> Result<(), KeyFileError> {
    if kid.trim().is_empty() {
        return Err(KeyFileError::Invalid {
            path: path.to_path_buf(),
            detail: "a key id may not be empty".to_string(),
        });
    }
    refuse_if_readable_by_others(path)?;

    let text =
        Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| KeyFileError::Unreadable {
                path: path.to_path_buf(),
                source: e,
            })?,
        );
    let mut parsed: KeyFileOnDisk =
        toml::from_str(&text).map_err(|e| KeyFileError::Unparseable {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

    if parsed.keys.contains_key(kid) {
        return Err(KeyFileError::Invalid {
            path: path.to_path_buf(),
            detail: format!(
                "key id {kid:?} already exists; replacing it would invalidate every token \
                 already signed under it"
            ),
        });
    }

    let secret = Zeroizing::new(random_secret()?);
    parsed.keys.insert(
        kid.to_string(),
        KeyEntryOnDisk {
            secret: B64.encode(&*secret),
            note: note.to_string(),
        },
    );

    let body =
        Zeroizing::new(
            toml::to_string_pretty(&parsed).map_err(|e| KeyFileError::Invalid {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?,
        );
    // Written through a temporary file in the same directory and renamed, so an
    // interrupted write cannot leave the key file truncated — which would lock
    // every client out at once, including the one that would fix it.
    let tmp = path.with_extension("toml.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write as _;
        let mut f = opts.open(&tmp).map_err(|e| KeyFileError::Uncreatable {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(body.as_bytes())
            .map_err(|e| KeyFileError::Uncreatable {
                path: tmp.clone(),
                source: e,
            })?;
    }
    std::fs::rename(&tmp, path).map_err(|e| KeyFileError::Uncreatable {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Why a key file could not be used. Every variant names the path, because the
/// resolved path is the one thing an operator cannot guess from the message.
#[derive(Debug, thiserror::Error)]
pub enum KeyFileError {
    #[error("cannot read the signing key file at {path}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create the signing key file at {path}")]
    Uncreatable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the signing key file at {path} is not valid TOML: {detail}")]
    Unparseable { path: PathBuf, detail: String },
    #[error("the signing key file at {path} is unusable: {detail}")]
    Invalid { path: PathBuf, detail: String },
    #[error(
        "the signing key file at {path} is mode {mode:04o} — readable by other accounts on this \
         host; run `chmod 600 {path}` and rotate the key, since anything that could read it can \
         mint a token for any project"
    )]
    TooPermissive { path: PathBuf, mode: u32 },
}

/// Refuses a key file any other account can read.
///
/// Group is checked alongside other for the reason a shared group exists at all:
/// "only my group" routinely means "and the CI runner". Unix-only because the
/// mode is; elsewhere the check is skipped rather than approximated, since a
/// permission check that reports a value it did not measure is worse than none.
#[cfg(unix)]
fn refuse_if_readable_by_others(path: &Path) -> Result<(), KeyFileError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .map_err(|e| KeyFileError::Unreadable {
            path: path.to_path_buf(),
            source: e,
        })?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(KeyFileError::TooPermissive {
            path: path.to_path_buf(),
            mode: mode & 0o7777,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_if_readable_by_others(_path: &Path) -> Result<(), KeyFileError> {
    Ok(())
}

/// Writes a fresh key file with one generated key.
fn create_key_file(path: &Path) -> Result<(), KeyFileError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| KeyFileError::Uncreatable {
            path: path.to_path_buf(),
            source: e,
        })?;
    }

    let secret = Zeroizing::new(random_secret()?);
    let file = KeyFileOnDisk {
        active: "default".to_string(),
        keys: BTreeMap::from([(
            "default".to_string(),
            KeyEntryOnDisk {
                secret: B64.encode(&*secret),
                note: "generated on first start".to_string(),
            },
        )]),
    };
    let body =
        Zeroizing::new(
            toml::to_string_pretty(&file).map_err(|e| KeyFileError::Invalid {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?,
        );

    // `create_new` is `O_EXCL`: two servers racing to first start must not both
    // believe they own the secret, because the loser would sign tokens the winner
    // refuses. The mode is set in the same `open` on unix rather than by a
    // follow-up `chmod`, which would leave the secret world-readable for the
    // width of that window.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| KeyFileError::Uncreatable {
        path: path.to_path_buf(),
        source: e,
    })?;
    use std::io::Write as _;
    f.write_all(body.as_bytes())
        .map_err(|e| KeyFileError::Uncreatable {
            path: path.to_path_buf(),
            source: e,
        })
}

fn random_secret() -> Result<Vec<u8>, KeyFileError> {
    let mut buf = vec![0u8; SECRET_BYTES];
    getrandom::fill(&mut buf).map_err(|e| KeyFileError::Invalid {
        path: PathBuf::new(),
        detail: format!("the operating system refused to supply randomness: {e}"),
    })?;
    Ok(buf)
}

// ─── Mint and verify ─────────────────────────────────────────────────────────

/// The JOSE header. `kid` is the only field read on the way back in.
#[derive(Serialize, Deserialize)]
struct Header {
    alg: String,
    typ: String,
    kid: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Normalizes one `prj` entry, refusing anything that is neither the wildcard
/// nor a GUID.
///
/// Both halves matter. Storing the **simple** form is what lets [`Claims::covers`]
/// be a plain comparison: a caller who wrote the hyphenated spelling into a mint
/// request would otherwise hold a token that names a project it can never reach,
/// and the resulting 404 would look exactly like a scope decision. Refusing
/// anything unparseable is the other half — a typo'd GUID silently produces a
/// token that reaches nothing, which is discovered later, by someone else.
fn normalize_project(raw: &str) -> Result<String, AuthError> {
    let raw = raw.trim();
    if raw == WILDCARD_PROJECT {
        return Ok(WILDCARD_PROJECT.to_string());
    }
    Uuid::parse_str(raw)
        .map(|u| u.simple().to_string())
        .map_err(|_| {
            AuthError::InvalidProject(format!(
                "{raw:?} is neither a project GUID nor {WILDCARD_PROJECT:?}"
            ))
        })
}

/// Builds claims for a token valid for `days`, then signs them with the ring's
/// active key. Production callers name a key id; this is the tests' shorthand.
#[cfg(test)]
pub fn mint(
    ring: &Keyring,
    sub: &str,
    projects: Vec<String>,
    actions: Vec<Action>,
    days: u64,
) -> Result<(String, Claims), AuthError> {
    mint_with_key(ring, None, sub, projects, actions, vec![], days)
}

/// [`mint`], signing under a named key id.
///
/// The named-key form is the revocation mechanism rather than a convenience:
/// with no denylist by design, giving a guest its own key id is what makes
/// "revoke this one credential" a single line deleted from the key file, instead
/// of a rotation that logs out every client at once.
pub fn mint_with_key(
    ring: &Keyring,
    kid: Option<&str>,
    sub: &str,
    projects: Vec<String>,
    actions: Vec<Action>,
    audiences: Vec<Audience>,
    days: u64,
) -> Result<(String, Claims), AuthError> {
    let now = now_secs();
    let prj = projects
        .iter()
        .map(|p| normalize_project(p))
        .collect::<Result<Vec<_>, _>>()?;
    let claims = Claims {
        iss: ISSUER.to_string(),
        sub: sub.to_string(),
        jti: Uuid::new_v4().to_string(),
        iat: now,
        nbf: now,
        // `0` is the no-expiry spelling, and it has to be spelled: an omitted
        // `--days` is 30, never "forever".
        exp: (days > 0).then(|| now + days.saturating_mul(86_400)),
        prj,
        act: actions,
        aud: {
            // Sorted and deduplicated, so `--for vscode,cli` and `--for cli,vscode`
            // produce the same claim: a client comparing audiences must not be
            // able to disagree with another client because of argument order.
            let mut aud = audiences;
            aud.sort_unstable();
            aud.dedup();
            aud
        },
    };
    let token = sign_with(ring, kid, &claims)?;
    Ok((token, claims))
}

/// Signs `claims` with the ring's active key.
#[cfg(test)]
pub fn sign(ring: &Keyring, claims: &Claims) -> Result<String, AuthError> {
    sign_with(ring, None, claims)
}

fn sign_with(ring: &Keyring, kid: Option<&str>, claims: &Claims) -> Result<String, AuthError> {
    let kid = kid.unwrap_or(&ring.active);
    let secret = ring
        .keys
        .get(kid)
        .ok_or_else(|| AuthError::UnknownKeyId(kid.to_string()))?;

    let header = Header {
        alg: "HS256".to_string(),
        typ: "JWT".to_string(),
        kid: kid.to_string(),
    };
    let h = B64.encode(
        serde_json::to_vec(&header)
            .map_err(|_| AuthError::Malformed("the header could not be serialized"))?,
    );
    let c = B64.encode(
        serde_json::to_vec(claims)
            .map_err(|_| AuthError::Malformed("the claims could not be serialized"))?,
    );

    let signing_input = format!("{h}.{c}");
    let mut mac = secret.mac();
    mac.update(signing_input.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{sig}"))
}

/// Verifies a token and returns its claims.
///
/// # Why `alg` is never dispatched on
///
/// The JWT vulnerability family everyone knows — `alg: none`, and RS256 tokens
/// re-signed as HS256 with the public key as the secret — exists because
/// libraries read the *attacker-supplied* header to decide how to verify. This
/// function reads `kid` and nothing else that matters: verification is
/// unconditionally HMAC-SHA256 against a key this server holds, so a header
/// claiming any other algorithm simply fails the signature check. `alg` is
/// checked afterwards only so a token that is confusing to a human debugger is
/// refused with a clear reason rather than a bare signature failure.
///
/// The comparison is constant-time. It matters less here than in a password
/// check — an attacker who can time a signature comparison can usually do
/// better — but a variable-time `==` on a MAC is the kind of thing that is
/// correct until the code around it changes.
pub fn verify(ring: &Keyring, token: &str, leeway_secs: u64) -> Result<Claims, AuthError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(AuthError::Malformed("the token is implausibly long"));
    }

    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AuthError::Malformed(
            "a JWT has exactly three dot-separated parts",
        ));
    };

    let header_bytes = B64
        .decode(h.as_bytes())
        .map_err(|_| AuthError::Malformed("the header is not base64url"))?;
    let header: Header = serde_json::from_slice(&header_bytes)
        .map_err(|_| AuthError::Malformed("the header is not the expected JSON object"))?;

    let secret = ring
        .keys
        .get(&header.kid)
        .ok_or_else(|| AuthError::UnknownKeyId(header.kid.clone()))?;

    let presented = B64
        .decode(s.as_bytes())
        .map_err(|_| AuthError::Malformed("the signature is not base64url"))?;
    let mut mac = secret.mac();
    mac.update(format!("{h}.{c}").as_bytes());
    let expected = mac.finalize().into_bytes();
    if presented.ct_eq(&expected).unwrap_u8() != 1 {
        return Err(AuthError::BadSignature);
    }

    // Only now, with the signature established, is anything in the token worth
    // reading as a statement. `alg` included: before this line it is attacker
    // input, and after it, it is merely redundant.
    if header.alg != "HS256" {
        return Err(AuthError::Malformed(
            "this server signs only HS256, and the header says otherwise",
        ));
    }

    let claims_bytes = B64
        .decode(c.as_bytes())
        .map_err(|_| AuthError::Malformed("the payload is not base64url"))?;
    let claims: Claims = serde_json::from_slice(&claims_bytes)
        .map_err(|_| AuthError::Malformed("the payload is not a claim set this server issued"))?;

    if claims.iss != ISSUER {
        return Err(AuthError::Malformed(
            "the token was issued by something else",
        ));
    }
    let now = now_secs();
    if let Some(exp) = claims.exp
        && exp.saturating_add(leeway_secs) < now
    {
        return Err(AuthError::Expired);
    }
    if claims.nbf > now.saturating_add(leeway_secs) {
        return Err(AuthError::NotYetValid);
    }

    Ok(claims)
}

/// Pulls the bearer token out of an `Authorization` header value.
///
/// The scheme match is ASCII-case-insensitive because RFC 7235 says it is, and
/// clients spell it `Bearer`, `bearer` and occasionally `BEARER`.
pub fn bearer_from_header(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> Keyring {
        Keyring::from_secret("test", vec![7u8; 32])
    }

    fn claims_for(prj: &[&str], act: &[Action], exp_in: i64) -> Claims {
        let now = now_secs();
        Claims {
            iss: ISSUER.to_string(),
            sub: "t".to_string(),
            jti: Uuid::new_v4().to_string(),
            iat: now,
            nbf: now,
            exp: Some((now as i64 + exp_in) as u64),
            prj: prj.iter().map(|s| s.to_string()).collect(),
            act: act.to_vec(),
            aud: vec![],
        }
    }

    #[test]
    fn a_minted_token_verifies_and_round_trips_its_scope() {
        let r = ring();
        let guid = Uuid::new_v4();
        let (token, _) = mint(
            &r,
            "someone",
            vec![guid.simple().to_string()],
            vec![Action::Search],
            30,
        )
        .expect("mints");

        let back = verify(&r, &token, 60).expect("verifies");
        assert!(back.covers(&guid));
        assert!(back.permits(Action::Search));
        assert!(!back.permits(Action::Delete));
    }

    /// The dashed and dashless spellings address one project everywhere else in
    /// the server, so a token that told them apart would refuse a caller access
    /// to a project it demonstrably holds, depending only on how somebody typed
    /// the GUID into the mint request. `mint` normalizes; `covers` additionally
    /// ignores case, since the two together are cheaper than one bug.
    #[test]
    fn how_a_guid_was_spelled_never_decides_access() {
        let r = ring();
        let guid = Uuid::new_v4();

        for spelling in [
            guid.hyphenated().to_string(),
            guid.simple().to_string(),
            guid.hyphenated().to_string().to_uppercase(),
        ] {
            let (token, _) =
                mint(&r, "t", vec![spelling.clone()], vec![Action::Search], 1).expect("mints");
            assert!(
                verify(&r, &token, 60).unwrap().covers(&guid),
                "{spelling} produced a token that cannot reach its own project"
            );
        }
    }

    /// A typo'd GUID would otherwise mint a perfectly valid token that reaches
    /// nothing — discovered later, by somebody else, as a 404 indistinguishable
    /// from a scope decision.
    #[test]
    fn minting_refuses_a_project_that_is_neither_a_guid_nor_the_wildcard() {
        let r = ring();
        for bad in ["", "all", "**", "c2d7e2c1-3165-42f5-9366", "project-one"] {
            assert!(
                mint(&r, "t", vec![bad.to_string()], vec![Action::Search], 1).is_err(),
                "{bad:?} was minted into a token"
            );
        }
    }

    /// The classic JWT break, and the reason `verify` reads `kid` and nothing
    /// else before checking the MAC. Reintroducing header-driven dispatch is what
    /// this test is meant to catch.
    #[test]
    fn the_algorithm_header_cannot_select_the_algorithm() {
        let r = ring();
        let c = claims_for(&[WILDCARD_PROJECT], &[Action::Search], 600);

        for alg in ["none", "RS256", "HS512"] {
            let header = serde_json::json!({"alg": alg, "typ": "JWT", "kid": "test"});
            let h = B64.encode(serde_json::to_vec(&header).unwrap());
            let payload = B64.encode(serde_json::to_vec(&c).unwrap());

            // Unsigned, the `alg: none` shape.
            let err = verify(&r, &format!("{h}.{payload}."), 60).expect_err("must refuse");
            assert!(
                matches!(err, AuthError::Malformed(_) | AuthError::BadSignature),
                "{alg} unsigned was accepted: {err}"
            );

            // Correctly HMAC'd but claiming another algorithm: the signature
            // passes, and the token is still refused.
            let mut mac = HmacSha256::new_from_slice(&[7u8; 32]).unwrap();
            mac.update(format!("{h}.{payload}").as_bytes());
            let sig = B64.encode(mac.finalize().into_bytes());
            let err = verify(&r, &format!("{h}.{payload}.{sig}"), 60).expect_err("must refuse");
            if alg != "HS256" {
                assert!(
                    matches!(err, AuthError::Malformed(_)),
                    "{alg} was accepted after signing: {err}"
                );
            }
        }
    }

    #[test]
    fn a_token_from_another_key_is_refused() {
        let mine = ring();
        let theirs = Keyring::from_secret("test", vec![9u8; 32]);
        let (token, _) = mint(&theirs, "x", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();
        assert!(matches!(
            verify(&mine, &token, 60),
            Err(AuthError::BadSignature)
        ));
    }

    #[test]
    fn an_unknown_key_id_is_its_own_refusal() {
        let mine = ring();
        let theirs = Keyring::from_secret("rotated-away", vec![9u8; 32]);
        let (token, _) = mint(&theirs, "x", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();
        assert!(matches!(
            verify(&mine, &token, 60),
            Err(AuthError::UnknownKeyId(k)) if k == "rotated-away"
        ));
    }

    #[test]
    fn an_expired_token_is_refused_with_its_own_code() {
        let r = ring();
        let c = claims_for(&[WILDCARD_PROJECT], &[Action::Search], -7200);
        let token = sign(&r, &c).unwrap();
        assert!(matches!(verify(&r, &token, 60), Err(AuthError::Expired)));
    }

    /// Leeway exists for clock skew between the minting host and this one, and
    /// must apply to both ends: a token whose `nbf` is a few seconds in the
    /// future is the ordinary result of two machines disagreeing, not an attack.
    #[test]
    fn leeway_covers_skew_at_both_ends() {
        let r = ring();

        let mut c = claims_for(&[WILDCARD_PROJECT], &[], 0);
        c.exp = Some(now_secs() - 30);
        assert!(verify(&r, &sign(&r, &c).unwrap(), 60).is_ok());

        let mut c = claims_for(&[WILDCARD_PROJECT], &[], 600);
        c.nbf = now_secs() + 30;
        assert!(verify(&r, &sign(&r, &c).unwrap(), 60).is_ok());

        let mut c = claims_for(&[WILDCARD_PROJECT], &[], 600);
        c.nbf = now_secs() + 3600;
        assert!(matches!(
            verify(&r, &sign(&r, &c).unwrap(), 60),
            Err(AuthError::NotYetValid)
        ));
    }

    /// An empty project list is a token that reaches nothing. It must never be
    /// read as "unrestricted": that reading turns a minter's omitted argument
    /// into full access, silently.
    #[test]
    fn a_wildcard_must_be_spelled_and_an_empty_list_is_not_one() {
        let guid = Uuid::new_v4();
        assert!(!claims_for(&[], &[], 600).covers(&guid));
        assert!(!claims_for(&[], &[], 600).is_wildcard());
        assert!(claims_for(&[WILDCARD_PROJECT], &[], 600).covers(&guid));
    }

    #[test]
    fn a_minted_token_can_never_exceed_its_minter() {
        let a = Uuid::new_v4().simple().to_string();
        let b = Uuid::new_v4().simple().to_string();
        let minter = claims_for(&[&a], &[Action::Mint, Action::Search], 3600);

        minter
            .may_mint(&claims_for(&[&a], &[Action::Search], 1800))
            .expect("a strictly narrower token is fine");

        for (wanted, why) in [
            (claims_for(&[&a], &[Action::Admin], 1800), "a wider action"),
            (
                claims_for(&[&b], &[Action::Search], 1800),
                "another project",
            ),
            (
                claims_for(&[WILDCARD_PROJECT], &[Action::Search], 1800),
                "a wildcard from a named minter",
            ),
            (claims_for(&[&a], &[Action::Search], 7200), "a later expiry"),
        ] {
            assert!(
                matches!(
                    minter.may_mint(&wanted),
                    Err(AuthError::MintWouldExceedMinter(_))
                ),
                "{why} was allowed"
            );
        }

        let no_mint = claims_for(&[&a], &[Action::Search], 3600);
        assert!(matches!(
            no_mint.may_mint(&claims_for(&[&a], &[Action::Search], 60)),
            Err(AuthError::ActionNotPermitted(Action::Mint))
        ));
    }

    /// Every action, in both directions, rather than the one that reads worst.
    ///
    /// The previous version of this check tried `admin` and concluded the rule
    /// held. That is the escalation a reviewer imagines, and it is not the one a
    /// bug would produce: a containment rule written with a `matches!` or a
    /// hard-coded "dangerous actions" list refuses `admin` and waves `delete`
    /// through. Driving the table from `Action::ALL` is what keeps this
    /// exhaustive as the vocabulary grows — a seventh action is covered on the
    /// day it is added, in both roles.
    #[test]
    fn no_action_can_be_minted_by_a_token_that_lacks_it() {
        let p = Uuid::new_v4().simple().to_string();
        for held in Action::ALL {
            // A minter holding exactly `mint` + one other action.
            let minter = claims_for(&[&p], &[Action::Mint, *held], 3600);
            for wanted in Action::ALL {
                let result = minter.may_mint(&claims_for(&[&p], &[*wanted], 1800));
                let allowed = *wanted == *held || *wanted == Action::Mint;
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "a minter holding [mint, {held}] {} issue {wanted}",
                    if allowed { "must" } else { "must not" }
                );
            }
        }
    }

    /// A token may not smuggle a wider action past the check by asking for it
    /// alongside ones it does hold — the rule is per-action, not per-request.
    #[test]
    fn a_mixed_request_is_refused_for_the_one_action_it_may_not_have() {
        let p = Uuid::new_v4().simple().to_string();
        let minter = claims_for(
            &[&p],
            &[Action::Mint, Action::Search, Action::Research],
            3600,
        );

        minter
            .may_mint(&claims_for(&[&p], &[Action::Search, Action::Research], 900))
            .expect("both held actions together are fine");

        let err = minter
            .may_mint(&claims_for(
                &[&p],
                &[Action::Search, Action::Research, Action::Delete],
                900,
            ))
            .expect_err("one unheld action must sink the whole request");
        assert!(
            format!("{err}").contains("delete"),
            "the refusal must name the offending action, got: {err}"
        );
    }

    /// The expiry axis, including the shape that reads as an exemption.
    ///
    /// "No expiry" is not a smaller number than the minter's — it is the largest
    /// one there is, so an expiring minter must refuse it. That case is the one a
    /// naive `wanted.exp > mine` comparison lets through, because `None` is not
    /// greater than anything.
    #[test]
    fn expiry_is_capped_by_the_minter_and_forever_is_the_largest_expiry() {
        let p = Uuid::new_v4().simple().to_string();
        let minter = claims_for(&[&p], &[Action::Mint, Action::Search], 3600);

        minter
            .may_mint(&claims_for(&[&p], &[Action::Search], 3600))
            .expect("the same expiry is not a later one");
        assert!(
            minter
                .may_mint(&claims_for(&[&p], &[Action::Search], 3601))
                .is_err(),
            "one second past the minter's own expiry was allowed"
        );

        let mut forever = claims_for(&[&p], &[Action::Search], 60);
        forever.exp = None;
        let err = minter
            .may_mint(&forever)
            .expect_err("an expiring minter must not issue an eternal token");
        assert!(
            format!("{err}").contains("never does"),
            "the refusal must say what it refused, got: {err}"
        );

        // The other direction: an eternal minter is bounded by nothing, which is
        // what makes a machine-local credential able to issue working ones.
        let mut eternal = claims_for(&[&p], &[Action::Mint, Action::Search], 60);
        eternal.exp = None;
        eternal
            .may_mint(&forever)
            .expect("an eternal minter may issue an eternal token");
        eternal
            .may_mint(&claims_for(&[&p], &[Action::Search], 86_400))
            .expect("an eternal minter may issue an expiring token");
    }

    /// The project axis, in every combination of wildcard and named.
    #[test]
    fn projects_are_capped_by_the_minter_in_both_directions() {
        let a = Uuid::new_v4().simple().to_string();
        let b = Uuid::new_v4().simple().to_string();
        let acts = &[Action::Mint, Action::Search];

        let named = claims_for(&[&a], acts, 3600);
        named
            .may_mint(&claims_for(&[&a], &[Action::Search], 900))
            .expect("its own project");
        assert!(
            named
                .may_mint(&claims_for(&[&b], &[Action::Search], 900))
                .is_err(),
            "a project the minter does not hold was allowed"
        );
        assert!(
            named
                .may_mint(&claims_for(&[&a, &b], &[Action::Search], 900))
                .is_err(),
            "a superset containing one unheld project was allowed"
        );
        assert!(
            named
                .may_mint(&claims_for(&[WILDCARD_PROJECT], &[Action::Search], 900))
                .is_err(),
            "a wildcard from a named minter was allowed"
        );

        let two = claims_for(&[&a, &b], acts, 3600);
        two.may_mint(&claims_for(&[&b], &[Action::Search], 900))
            .expect("a subset of a two-project minter");

        let wild = claims_for(&[WILDCARD_PROJECT], acts, 3600);
        wild.may_mint(&claims_for(&[&a, &b], &[Action::Search], 900))
            .expect("a wildcard minter reaches every project");
        wild.may_mint(&claims_for(&[WILDCARD_PROJECT], &[Action::Search], 900))
            .expect("a wildcard minter may pass the wildcard on");
    }

    /// A GUID spelled with dashes names the same project as one without, and the
    /// containment check must agree with the collection namer about that. If it
    /// did not, a minter scoped to `a` would refuse `a` written the other way —
    /// or, far worse, a check comparing raw strings would let a *different*
    /// spelling look like a different project and pass.
    #[test]
    fn a_projects_spelling_cannot_evade_the_project_check() {
        let raw = Uuid::new_v4();
        // Two days, so the expiry axis cannot fire before the project one: the
        // token below is minted for one day, and a shorter-lived minter would
        // make this test pass or fail for the wrong reason.
        let minter = Claims {
            prj: vec![raw.simple().to_string()],
            ..claims_for(&[], &[Action::Mint, Action::Search], 2 * 86_400)
        };
        // `mint` normalizes, so this is the shape the rule actually sees.
        let (_, wanted) = mint(
            &ring(),
            "t",
            vec![raw.hyphenated().to_string().to_uppercase()],
            vec![Action::Search],
            1,
        )
        .expect("mints");
        minter
            .may_mint(&wanted)
            .expect("the same GUID in another spelling is the same project");
    }

    /// Delegation is transitive, so the rule has to hold across a chain rather
    /// than only at one hop: the danger is a token laundering privilege through
    /// an intermediate one. It holds by construction — C ≤ B ≤ A — and this is
    /// the test that says so out loud, because "by construction" is the claim
    /// that stops being true when someone adds a special case.
    #[test]
    fn delegation_cannot_widen_across_a_chain() {
        let a = Uuid::new_v4().simple().to_string();
        let b = Uuid::new_v4().simple().to_string();

        let root = claims_for(
            &[&a, &b],
            &[Action::Mint, Action::Search, Action::Delete],
            7200,
        );
        let middle = claims_for(&[&a], &[Action::Mint, Action::Search], 3600);
        root.may_mint(&middle).expect("the middle link is narrower");

        // Everything the root could have issued, asked for one hop down.
        for (wanted, why) in [
            (
                claims_for(&[&a], &[Action::Delete], 900),
                "an action the middle dropped",
            ),
            (
                claims_for(&[&b], &[Action::Search], 900),
                "a project the middle dropped",
            ),
            (
                claims_for(&[&a], &[Action::Search], 7200),
                "the root's expiry",
            ),
        ] {
            assert!(
                middle.may_mint(&wanted).is_err(),
                "{why} was recovered through the intermediate token"
            );
        }
    }

    /// `mint` is itself an action, so a minter without it cannot delegate — and
    /// one *with* it can pass it on, which is what makes the chain above a real
    /// shape rather than a hypothetical.
    #[test]
    fn minting_requires_the_mint_action_and_can_be_delegated() {
        let p = Uuid::new_v4().simple().to_string();

        let without = claims_for(&[&p], &[Action::Search, Action::Admin], 3600);
        assert!(matches!(
            without.may_mint(&claims_for(&[&p], &[Action::Search], 60)),
            Err(AuthError::ActionNotPermitted(Action::Mint))
        ));

        let with = claims_for(&[&p], &[Action::Mint, Action::Search], 3600);
        with.may_mint(&claims_for(&[&p], &[Action::Mint, Action::Search], 900))
            .expect("mint is delegable like any other action it holds");
    }

    /// A token reaching nothing is the floor, and it must not be reachable by
    /// omission: an empty project list is refused at mint, so it can never
    /// become the `is_wildcard()`-adjacent special case that reads as "all".
    #[test]
    fn a_token_that_reaches_nothing_is_not_a_token_that_reaches_everything() {
        let empty = claims_for(&[], &[Action::Search], 3600);
        assert!(!empty.is_wildcard());
        assert!(!empty.covers(&Uuid::new_v4()));
    }

    // ── The audience claim ───────────────────────────────────────────────────

    /// An older token carries no `aud` at all, and a client reading "labelled for
    /// nobody" as "usable by nobody" would lock every existing holder out the day
    /// the field shipped. `#[serde(default)]` is what prevents it; this pins the
    /// round trip rather than the attribute, since the attribute can be present
    /// and the field still be renamed out from under it.
    #[test]
    fn a_token_minted_before_audiences_existed_still_verifies_and_names_none() {
        let r = ring();
        let (token, _) = mint(&r, "old", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();
        let back = verify(&r, &token, 60).expect("verifies");
        assert!(back.aud.is_empty());

        // And the serialized form of such a token must not carry the key at all:
        // a client keying on presence would otherwise see `"aud": []` and read it
        // as an explicit empty allow-list, which is the dangerous reading.
        let payload = token.split('.').nth(1).unwrap();
        let json = String::from_utf8(B64.decode(payload).unwrap()).unwrap();
        assert!(
            !json.contains("aud"),
            "an unlabelled token spelled aud: {json}"
        );
    }

    #[test]
    fn the_audience_round_trips_and_is_normalized_so_argument_order_cannot_matter() {
        let r = ring();
        let (a, _) = mint_with_key(
            &r,
            None,
            "t",
            vec![WILDCARD_PROJECT.into()],
            vec![Action::Search],
            vec![Audience::Vscode, Audience::Cli, Audience::Vscode],
            1,
        )
        .unwrap();
        let (b, _) = mint_with_key(
            &r,
            None,
            "t",
            vec![WILDCARD_PROJECT.into()],
            vec![Action::Search],
            vec![Audience::Cli, Audience::Vscode],
            1,
        )
        .unwrap();

        let (a, b) = (verify(&r, &a, 60).unwrap(), verify(&r, &b, 60).unwrap());
        assert_eq!(a.aud, vec![Audience::Cli, Audience::Vscode]);
        assert_eq!(a.aud, b.aud, "argument order changed the claim");
    }

    /// Every audience must survive a round trip under the exact spelling clients
    /// compare against — a renamed variant would silently stop matching, and the
    /// symptom is a client refusing a token that is perfectly good.
    #[test]
    fn every_audience_survives_the_wire_under_its_own_spelling() {
        let r = ring();
        for a in Audience::ALL {
            let (token, _) = mint_with_key(
                &r,
                None,
                "t",
                vec![WILDCARD_PROJECT.into()],
                vec![Action::Search],
                vec![*a],
                1,
            )
            .unwrap();
            let back = verify(&r, &token, 60).unwrap();
            assert_eq!(back.aud, vec![*a]);

            let payload = token.split('.').nth(1).unwrap();
            let json = String::from_utf8(B64.decode(payload).unwrap()).unwrap();
            assert!(
                json.contains(&format!("\"{}\"", a.as_str())),
                "{a} is not spelled {:?} on the wire: {json}",
                a.as_str()
            );
            assert_eq!(Audience::parse(a.as_str()), Some(*a));
        }
    }

    /// The rule this pins is the one a reviewer will try to "fix": `may_mint`
    /// contains actions, projects and expiry, so containing the audience too
    /// looks like consistency. It would break the mechanism's primary use — the
    /// VS Code button issuing an agent token — and it would do so with an error
    /// message about exceeding the minter, which reads like a security refusal.
    #[test]
    fn the_audience_is_not_an_authority_axis_and_does_not_bind_delegation() {
        let p = Uuid::new_v4().simple().to_string();
        let mut minter = claims_for(&[&p], &[Action::Mint, Action::Search], 7200);
        minter.aud = vec![Audience::Vscode];

        for wanted in Audience::ALL {
            let mut c = claims_for(&[&p], &[Action::Search], 3600);
            c.aud = vec![*wanted];
            assert!(
                minter.may_mint(&c).is_ok(),
                "a vscode token was refused minting for {wanted}"
            );
        }

        // Including the unlabelled case, which is the *widest* of all.
        let unlabelled = claims_for(&[&p], &[Action::Search], 3600);
        assert!(unlabelled.aud.is_empty());
        assert!(minter.may_mint(&unlabelled).is_ok());
    }

    /// The audience must not become a way to smuggle authority past the checks
    /// that do bind. Same table as the action rule, one axis moved.
    #[test]
    fn an_audience_does_not_widen_what_a_minted_token_may_do() {
        let p = Uuid::new_v4().simple().to_string();
        let mut minter = claims_for(&[&p], &[Action::Mint, Action::Search], 7200);
        minter.aud = vec![Audience::Vscode];

        let mut wanted = claims_for(&[&p], &[Action::Delete], 3600);
        wanted.aud = vec![Audience::Vscode];
        assert!(minter.may_mint(&wanted).is_err());

        let mut wanted = claims_for(&[WILDCARD_PROJECT], &[Action::Search], 3600);
        wanted.aud = vec![Audience::Vscode];
        assert!(minter.may_mint(&wanted).is_err());
    }

    #[test]
    fn an_unknown_audience_spelling_is_not_parsed_into_one_that_exists() {
        for bad in ["", "CLI", "vs-code", "ai", "editor", "*", "agent "] {
            assert_eq!(Audience::parse(bad), None, "{bad:?} parsed");
        }
    }

    #[test]
    fn a_malformed_token_is_refused_without_panicking() {
        let r = ring();
        for bad in [
            "",
            ".",
            "..",
            "a.b",
            "a.b.c.d",
            "!!!.???.***",
            "eyJ.eyJ.",
            &"x".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            assert!(verify(&r, bad, 60).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn the_bearer_scheme_is_parsed_case_insensitively_and_nothing_else_is() {
        assert_eq!(bearer_from_header("Bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("BEARER  abc "), Some("abc"));
        assert_eq!(bearer_from_header("Basic abc"), None);
        assert_eq!(bearer_from_header("abc"), None);
        assert_eq!(bearer_from_header("Bearer "), None);
    }

    /// The secret must not be renderable, because this codebase logs errors as
    /// `error = ?e` by convention and a derived `Debug` anywhere up the chain
    /// would carry the bytes into a log file.
    #[test]
    fn a_secret_never_renders_itself() {
        let s = SigningSecret(Zeroizing::new(b"super-secret-value".to_vec()));
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");

        let r = Keyring::from_secret("kid-name", b"super-secret-value".to_vec());
        let rendered = format!("{r:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(
            rendered.contains("kid-name"),
            "key ids are not secret and are what makes a refusal diagnosable: {rendered}"
        );
    }

    #[test]
    fn a_key_file_is_created_private_and_reloads_identically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.toml");

        let first = Keyring::load_or_create(&path).expect("creates");
        assert_eq!(first.key_ids(), vec!["default"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "created world-readable");
        }

        // A token signed before the reload must verify after it, or every client
        // is logged out whenever the server restarts.
        let (token, _) = mint(&first, "x", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();
        let second = Keyring::load_or_create(&path).expect("reloads");
        assert!(verify(&second, &token, 60).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_key_file_is_refused_by_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.toml");
        Keyring::load_or_create(&path).expect("creates");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = Keyring::load(&path).expect_err("must refuse");
        let msg = format!("{err}");
        assert!(msg.contains("chmod 600"), "must name the remedy: {msg}");
        assert!(
            msg.contains("mint a token"),
            "must say why it matters: {msg}"
        );
    }

    /// A file whose `active` names nothing would sign no token at all — a server
    /// that starts and then refuses every request, which is the failure shape
    /// this whole module is meant not to have.
    #[test]
    fn a_key_file_that_could_never_sign_is_refused_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.toml");
        std::fs::write(
            &path,
            "active = \"missing\"\n[keys.present]\nsecret = \"AAAAAAAAAAAAAAAAAAAAAA\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = Keyring::load(&path).expect_err("must refuse");
        assert!(format!("{err}").contains("active"), "{err}");
    }

    /// Several keys is the whole revocation story: a guest minted under its own
    /// key id is revoked by deleting that entry, and the working tokens signed
    /// under another id keep verifying.
    #[test]
    fn dropping_one_key_id_revokes_only_its_tokens() {
        let guest = Keyring::from_secret("guest-a", vec![1u8; 32]);
        let (guest_token, _) = mint(&guest, "g", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();

        let mut keys = BTreeMap::new();
        keys.insert(
            "working".to_string(),
            SigningSecret(Zeroizing::new(vec![2u8; 32])),
        );
        keys.insert(
            "guest-a".to_string(),
            SigningSecret(Zeroizing::new(vec![1u8; 32])),
        );
        let both = Keyring {
            keys,
            active: "working".to_string(),
        };
        let (working_token, _) =
            mint(&both, "w", vec![WILDCARD_PROJECT.into()], vec![], 1).unwrap();

        assert!(verify(&both, &guest_token, 60).is_ok());
        assert!(verify(&both, &working_token, 60).is_ok());

        let after = Keyring::from_secret("working", vec![2u8; 32]);
        assert!(matches!(
            verify(&after, &guest_token, 60),
            Err(AuthError::UnknownKeyId(_))
        ));
        assert!(verify(&after, &working_token, 60).is_ok());
    }
}
