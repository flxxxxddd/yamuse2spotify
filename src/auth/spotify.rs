//! the spotify side: pkce, because a desktop tool cannot keep a client secret.
//!
//! rspotify used to run this dance. it is here now because the client that
//! follows is librespot's session rather than a web-api client, and the only
//! thing the two share is the access token — so the ~120 lines below are the
//! whole of what was lost, and nothing else came with them.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use librespot_core::{Session, authentication::Credentials, config::SessionConfig};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{CLIENT_ID, REDIRECT_URI};
use crate::error::{Error, Result, io};

/// the scopes a migration needs.
///
/// read scopes are needed to tell "already there" from "not there yet", which
/// is what makes a rerun idempotent instead of duplicating.
const SCOPES: &str = "user-library-read user-library-modify playlist-read-private \
                      playlist-modify-private playlist-modify-public user-follow-read \
                      user-follow-modify streaming";

/// how long before expiry a cached token is treated as already stale.
///
/// a migration runs for a long time; renewing a token that has fifty seconds
/// left only to have it die mid-phase is worse than renewing early.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

/// what is kept between runs. spotify's reply, plus when it stops working.
#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    access_token: String,
    refresh_token: Option<String>,
    /// unix seconds.
    expires_at: u64,
}

/// spotify's reply to both the code exchange and the refresh.
#[derive(Debug, Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_expiry() -> u64 {
    3600
}

/// authorise against spotify and hand back a connected session.
///
/// there is nothing to configure: the client id is spotify's own, because the
/// internal api this connects to accepts no other kind.
pub async fn connect(cache_path: &Path) -> Result<Session> {
    let token = obtain(cache_path).await?;

    // the session talks to the internal api, which is the whole reason the web
    // api client is gone. it authenticates with the very token just obtained,
    // and names the same client id that token was issued to — login5 checks.
    let session = Session::new(
        SessionConfig {
            client_id: CLIENT_ID.to_owned(),
            ..SessionConfig::default()
        },
        None,
    );
    session
        .connect(Credentials::with_access_token(&token), true)
        .await
        .map_err(|e| Error::Config(format!("не удалось открыть сессию spotify: {e}")))?;

    Ok(session)
}

/// a usable access token: from the cache, by refreshing it, or by asking.
async fn obtain(cache_path: &Path) -> Result<String> {
    if let Some(cached) = read_cache(cache_path) {
        if !expired(&cached) {
            return Ok(cached.access_token);
        }

        // an expired token still carries the means to renew itself, and doing
        // so silently is the difference between a rerun that just works and one
        // that opens a browser every hour.
        if let Some(refresh) = &cached.refresh_token {
            match exchange(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CLIENT_ID),
            ])
            .await
            {
                Ok(reply) => {
                    let token = store(cache_path, reply, cached.refresh_token.clone())?;
                    return Ok(token);
                }
                Err(e) => {
                    // a refused refresh is not fatal: the browser path still
                    // works, and saying why beats a silent second login.
                    tracing::warn!(%e, "не удалось обновить токен, потребуется вход заново");
                }
            }
        }
    }

    let (verifier, challenge) = pkce();
    let state = random_hex();
    let url = authorize_url(&challenge, &state);

    match webbrowser::open(&url) {
        Ok(()) => println!("  открыт браузер для входа в spotify"),
        Err(_) => println!("  откройте в браузере:\n  {url}"),
    }

    let code = wait_for_code(&state)?;
    let reply = exchange(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", &verifier),
    ])
    .await?;

    store(cache_path, reply, None)
}

/// whether a cached token is too close to expiry to rely on.
fn expired(cached: &Cached) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    cached.expires_at <= (now + EXPIRY_MARGIN).as_secs()
}

fn read_cache(path: &Path) -> Option<Cached> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// persist the reply and hand back the access token.
///
/// `fallback_refresh` keeps the previous refresh token when a refresh reply
/// omits one — spotify does that, and dropping it would turn every subsequent
/// run back into a browser login.
fn store(path: &Path, reply: TokenReply, fallback_refresh: Option<String>) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let cached = Cached {
        access_token: reply.access_token.clone(),
        refresh_token: reply.refresh_token.or(fallback_refresh),
        expires_at: now + reply.expires_in,
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&cached)?).map_err(io(path))?;

    // the file is a live credential; it should not inherit the umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io(path))?;
    }

    Ok(reply.access_token)
}

/// post to the token endpoint and parse what comes back.
async fn exchange(form: &[(&str, &str)]) -> Result<TokenReply> {
    let response = reqwest::Client::new()
        .post("https://accounts.spotify.com/api/token")
        .form(form)
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;

    if !status.is_success() {
        return Err(Error::SpotifyStatus {
            status: status.as_u16(),
            message: body.chars().take(300).collect(),
            retry_after: None,
        });
    }

    Ok(serde_json::from_str(&body)?)
}

/// the url the browser is sent to.
fn authorize_url(challenge: &str, state: &str) -> String {
    let q = |v: &str| percent_encode(v);
    format!(
        "https://accounts.spotify.com/authorize?response_type=code&client_id={}\
         &redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        q(CLIENT_ID),
        q(REDIRECT_URI),
        q(challenge),
        q(state),
        q(SCOPES),
    )
}

/// serve exactly one request on the redirect uri's port and read the code out.
///
/// blocking rather than async because it is the one point in a run where
/// nothing else is in flight, and the user is the slow part regardless.
fn wait_for_code(expected_state: &str) -> Result<String> {
    let addr = socket_addr(REDIRECT_URI)?;
    let listener = TcpListener::bind(addr)
        .map_err(|e| Error::Config(format!("не удалось занять {addr} для ответа spotify: {e}")))?;

    let (mut stream, _) = listener
        .accept()
        .map_err(|e| Error::Config(format!("spotify не вернулся в браузер: {e}")))?;

    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| Error::Config(format!("не удалось прочитать ответ spotify: {e}")))?;

    let reply = "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\n\r\n\
                 готово, можно вернуться в терминал";
    let _ = stream.write_all(reply.as_bytes());

    // "GET /login?code=…&state=… HTTP/1.1"
    let target = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::Config("spotify прислал непонятный ответ".into()))?;

    let (code, state) = parse_callback(target);

    if let Some(state) = state
        && state != expected_state
    {
        // the state is what makes the code this run's rather than someone
        // else's; a mismatch is the one case where proceeding is wrong.
        return Err(Error::Config(
            "spotify вернул чужой state — вход не подтверждён".into(),
        ));
    }

    code.ok_or_else(|| Error::Config(format!("spotify не вернул код авторизации: {target}")))
}

/// pull `code` and `state` out of the callback path.
fn parse_callback(target: &str) -> (Option<String>, Option<String>) {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let mut code = None;
    let mut state = None;

    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", v)) => code = Some(v.to_owned()),
            Some(("state", v)) => state = Some(v.to_owned()),
            _ => {}
        }
    }

    (code, state)
}

/// the address a loopback redirect uri asks us to listen on.
fn socket_addr(redirect_uri: &str) -> Result<std::net::SocketAddr> {
    let rest = redirect_uri
        .strip_prefix("http://")
        .ok_or_else(|| Error::Config(format!("redirect uri не loopback: {redirect_uri}")))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| Error::Config(format!("в redirect uri нет порта: {redirect_uri}")))?;

    format!("{host}:{port}")
        .parse()
        .map_err(|e| Error::Config(format!("некорректный redirect uri {redirect_uri}: {e}")))
}

/// a pkce verifier and its challenge.
fn pkce() -> (String, String) {
    let verifier: String = {
        let mut bytes = [0u8; 64];
        rand::rng().fill(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_hex() -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// percent-encode everything that is not unreserved.
///
/// hand-rolled because the only strings that go through it are a client id, a
/// loopback uri, base64url text and a space-separated scope list — and pulling
/// in a url crate for that is more surface than the four lines it replaces.
fn percent_encode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_is_the_hash_of_its_verifier_not_the_verifier() {
        let (verifier, challenge) = pkce();
        assert_ne!(verifier, challenge);
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        // base64url, so nothing that would need escaping in a query string.
        assert!(!challenge.contains(['+', '/', '=']));
    }

    #[test]
    fn two_runs_do_not_share_a_verifier() {
        assert_ne!(pkce().0, pkce().0);
    }

    #[test]
    fn the_code_is_read_out_of_the_callback_whatever_the_order() {
        assert_eq!(
            parse_callback("/login?code=abc&state=xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_callback("/login?state=xyz&code=abc"),
            (Some("abc".into()), Some("xyz".into()))
        );
    }

    #[test]
    fn a_refusal_carries_no_code() {
        let (code, _) = parse_callback("/login?error=access_denied&state=xyz");
        assert!(code.is_none());
    }

    #[test]
    fn the_listener_port_comes_from_the_redirect_uri() {
        assert_eq!(
            socket_addr("http://127.0.0.1:8898/login").unwrap().port(),
            8898
        );
        assert_eq!(
            socket_addr("http://127.0.0.1:8888/callback")
                .unwrap()
                .port(),
            8888
        );
        assert!(socket_addr("https://example.com/cb").is_err());
    }

    #[test]
    fn a_token_about_to_expire_is_treated_as_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nearly = Cached {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: now + 30,
        };
        let fresh = Cached {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: now + 3600,
        };
        // renewing early beats dying halfway through a phase.
        assert!(expired(&nearly));
        assert!(!expired(&fresh));
    }

    #[test]
    fn scopes_and_uris_survive_encoding() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(
            percent_encode("http://127.0.0.1:8898/login"),
            "http%3A%2F%2F127.0.0.1%3A8898%2Flogin"
        );
        assert_eq!(percent_encode("azAZ09-._~"), "azAZ09-._~");
    }
}
