//! Per-server credentials for the Rust clients: `mindex/credentials.toml`,
//! XDG-resolved, keyed by server URL.
//!
//! It exists because the three homes a token was otherwise going to find are all
//! wrong. The committed `.mindex` is wrong twice over — it is in version control,
//! and it is shared by everyone who clones the repository, while a credential
//! belongs to one caller; the tell is that `.mindex` carries no server URL either,
//! because it describes *what the project is*, not how a particular machine
//! reaches a server. A flag is wrong because its value is visible in `ps` to every
//! user on the host. `$MINDEX_TOKEN` is right and stays the preferred spelling —
//! this file is for the case the environment cannot cover: several servers, or a
//! long-lived `mindex-watch` started by something that does not set it.
//!
//! ```toml
//! # ~/.config/mindex/credentials.toml, mode 0600
//! ["https://mindex.example:44343"]
//! token = "eyJhbGciOi..."
//!
//! ["https://127.0.0.1:11111"]
//! token = "eyJhbGciOi..."
//! ```
//!
//! One credential per server, and it is the only one: a bearer token minted by
//! mindex itself. There used to be a second — an `X-Api-Key` for a gateway in
//! front of the server — and removing it is what this file's shape records. Two
//! credentials meant an agent handed a token still could not reach a gated
//! deployment without also being handed the shared key, which is precisely the
//! secret the token was introduced to keep out of a model's context.
//!
//! Keyed by **server**, never by project: a token routinely covers several
//! projects, and one project is reachable through more than one URL.
//!
//! Both `mindex-index` and `mindex-watch` read this through the same code. A
//! verbatim copy in each — the arrangement `tools/*/src/scanner.rs` lives with — is
//! not acceptable here: the copies would drift on the permission check below, and a
//! permission check that is present in one client and absent in the other is a
//! defect that reports nothing at all.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name under the XDG config directory's `mindex/`.
pub const FILE_NAME: &str = "credentials.toml";

/// One server's entry. `deny_unknown_fields` for the reason the `.mindex` parser
/// gives: a mistyped `toekn =` that is silently ignored sends no header, and the
/// server's answer to a credential-less request is a 401 naming a missing token,
/// which reads as "this file was not found" rather than as a typo inside it.
///
/// It is deliberately still a table rather than a bare string: `api_key` used to
/// live beside `token` here, and the shape is what lets a future second field
/// arrive without every existing file becoming unparseable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    token: Option<String>,
}

/// The whole file: a table per server URL.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
struct File {
    servers: HashMap<String, Entry>,
}

/// Trailing-slash-insensitive comparison key. `--server https://host:44343/` and
/// the file's `"https://host:44343"` name one deployment; nothing else about the
/// URL is normalized, because guessing that `localhost` and `127.0.0.1` are the
/// same host is how a credential silently goes to the wrong place.
fn url_key(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

fn candidate_paths() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("MINDEX_CREDENTIALS") {
        return vec![PathBuf::from(p)];
    }
    let mut paths = Vec::new();
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(home) = config_home {
        paths.push(home.join("mindex").join(FILE_NAME));
    }
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    for dir in config_dirs.split(':').filter(|d| !d.is_empty()) {
        paths.push(PathBuf::from(dir).join("mindex").join(FILE_NAME));
    }
    paths
}

/// Refuses a file any other account can read.
///
/// A credentials store that does not check its own mode teaches the habit that
/// loses the key: `chmod 644` is what `umask 022` produces for anyone who creates
/// the file with a redirect. The check is unix-only because the mode is —
/// elsewhere it is skipped rather than approximated, and this is stated in the
/// error the caller would otherwise expect. Group is included alongside other:
/// a shared group is exactly the arrangement where "only my group" means "the
/// build agent too".
#[cfg(unix)]
fn refuse_if_readable_by_others(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .with_context(|| format!("cannot stat credentials file {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "credentials file {} is mode {:04o} — readable by other accounts on this host; \
             run `chmod 600 {}` and rotate the key if the host is shared",
            path.display(),
            mode & 0o7777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_if_readable_by_others(_path: &Path) -> Result<()> {
    Ok(())
}

/// Environment variable naming a file whose whole contents are the token.
pub const TOKEN_FILE_ENV: &str = "MINDEX_TOKEN_FILE";

/// The token the environment supplies: `$MINDEX_TOKEN`, else the contents of
/// `$MINDEX_TOKEN_FILE`.
///
/// The second spelling exists for a caller that configures a child process by
/// writing an environment block into a configuration file — an editor's MCP
/// server list is the case that produced it. Putting the token itself there puts
/// a bearer credential into a plaintext JSON file that no permission check
/// governs and, in some editors, a sync service copies; putting a *path* there
/// leaves the credential in a 0600 file, which is the arrangement
/// `credentials.toml` already has. The file is held to the same mode check, for
/// the same reason.
///
/// Errors are reserved for a file that was named and cannot be trusted. A named
/// file that is unreadable must not degrade to "no token": the request then fails
/// with a 401 that names neither the file nor the mistake.
pub fn token_from_env() -> Result<Option<String>> {
    if let Some(v) = std::env::var_os("MINDEX_TOKEN")
        && !v.is_empty()
    {
        return Ok(Some(v.to_string_lossy().into_owned()));
    }
    let Some(path) = std::env::var_os(TOKEN_FILE_ENV).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    refuse_if_readable_by_others(&path)
        .with_context(|| format!("{TOKEN_FILE_ENV} names {}", path.display()))?;
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read the token file {} ({TOKEN_FILE_ENV})",
            path.display()
        )
    })?;
    // Trailing newline is what every way of writing this file produces, and it is
    // not part of the token: sent as-is it makes an invalid header value.
    let token = text.trim().to_string();
    if token.is_empty() {
        bail!(
            "the token file {} named by {TOKEN_FILE_ENV} is empty — mint one with \
             `mindex mint-token` and write it there, or unset the variable",
            path.display()
        );
    }
    eprintln!("credentials: token read from {}", path.display());
    Ok(Some(token))
}

/// Parses the file's text. Split out from [`credentials_for`] so the format has
/// tests that touch no filesystem.
fn parse_str(text: &str) -> Result<File> {
    toml::from_str(text).context(
        "the file must be TOML: one `[\"<server url>\"]` table per server, each with an \
         optional `token = \"...\"`",
    )
}

/// What this file says about `server_url`.
#[derive(Debug, Clone, Default)]
pub struct ServerCredentials {
    /// Sent as `Authorization: Bearer`, for a server with `[auth].enabled`.
    pub token: Option<String>,
}

/// The credentials configured for `server_url`, or an empty set when no file
/// exists, no entry matches, or the entry names nothing.
///
/// Errors are reserved for a file that exists and cannot be trusted: bad
/// permissions, unparseable TOML, an unknown key inside an entry. Those must not
/// degrade to "no credential", because the resulting failure — a 401 from the
/// server — names neither the file nor the mistake.
pub fn credentials_for(server_url: &str) -> Result<ServerCredentials> {
    let Some(path) = candidate_paths().into_iter().find(|p| p.is_file()) else {
        return Ok(ServerCredentials::default());
    };
    refuse_if_readable_by_others(&path)?;

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read credentials file {}", path.display()))?;
    let file = parse_str(&text)
        .with_context(|| format!("invalid credentials file at {}", path.display()))?;

    let wanted = url_key(server_url);
    let entry = file
        .servers
        .iter()
        .find(|(url, _)| url_key(url) == wanted)
        .map(|(_, e)| e);

    let found = ServerCredentials {
        token: entry
            .and_then(|e| e.token.clone())
            .filter(|t| !t.trim().is_empty()),
    };

    // The URL is not a secret and the miss is the case worth explaining: an entry
    // spelled with a trailing path, or a `--server` that resolved elsewhere, is
    // otherwise indistinguishable from having no file at all. What is reported is
    // *that* a token was found, never its value.
    if found.token.is_some() {
        eprintln!(
            "credentials: token for {wanted} read from {}",
            path.display()
        );
    } else {
        eprintln!("credentials: {} has no entry for {wanted}", path.display());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_matched_regardless_of_a_trailing_slash() {
        let f = parse_str("[\"https://h:1\"]\ntoken = \"t\"\n").expect("parses");
        assert_eq!(url_key("https://h:1/"), "https://h:1");
        assert!(f.servers.contains_key("https://h:1"));
    }

    /// The failure this pins is silent by construction: serde ignores unknown
    /// fields by default, so `toekn =` would parse into `token: None`, send no
    /// header, and surface as the server refusing an unauthenticated request.
    #[test]
    fn a_mistyped_key_name_is_refused_rather_than_read_as_no_credential() {
        let err = parse_str("[\"https://h:1\"]\ntoekn = \"t\"\n").expect_err("must refuse");
        assert!(
            format!("{err:#}").contains("token"),
            "the error must name the key it wanted, got: {err:#}"
        );
    }

    /// `api_key` was a real field until the gateway stopped checking one. A file
    /// carrying the old spelling must say so rather than silently send nothing:
    /// the request then fails with a 401 that names a missing token, and the
    /// operator looks at a file that appears to hold a credential.
    #[test]
    fn the_retired_api_key_field_is_named_rather_than_ignored() {
        let err = parse_str("[\"https://h:1\"]\napi_key = \"k\"\n").expect_err("must refuse");
        assert!(
            format!("{err:#}").contains("api_key"),
            "the error must name the field it found, got: {err:#}"
        );
    }

    #[test]
    fn an_empty_file_is_a_valid_file_with_no_entries() {
        assert!(parse_str("").expect("parses").servers.is_empty());
    }

    /// Several servers is the whole reason this file exists beside
    /// `$MINDEX_TOKEN`, which can only hold one.
    #[test]
    fn several_servers_each_keep_their_own_token() {
        let f =
            parse_str("[\"https://a:1\"]\ntoken = \"ta\"\n\n[\"https://b:2\"]\ntoken = \"tb\"\n")
                .expect("parses");
        assert_eq!(f.servers["https://a:1"].token.as_deref(), Some("ta"));
        assert_eq!(f.servers["https://b:2"].token.as_deref(), Some("tb"));
    }

    /// An entry may exist and name nothing — that is a file being edited, not a
    /// parse error, and it must read as "no credential here" rather than fail.
    #[test]
    fn an_entry_naming_nothing_is_not_an_error() {
        let f = parse_str("[\"https://a:1\"]\n").expect("parses");
        assert!(f.servers["https://a:1"].token.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_file_is_refused_by_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "[\"https://h:1\"]\ntoken = \"t\"\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = refuse_if_readable_by_others(&path).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("chmod 600"), "must name the remedy: {msg}");
        assert!(msg.contains("0644"), "must name the mode found: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_file_is_accepted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        refuse_if_readable_by_others(&path).expect("0600 is the intended mode");
    }
}
