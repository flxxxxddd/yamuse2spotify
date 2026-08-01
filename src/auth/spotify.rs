//! the spotify side: pkce, because a desktop tool cannot keep a client secret.

use std::path::Path;

use rspotify::clients::OAuthClient;
use rspotify::{AuthCodePkceSpotify, Config, Credentials, OAuth, scopes};

use crate::error::{Error, Result};

/// authorise against spotify and hand back a ready client.
///
/// `client_id` and `redirect_uri` come from [`Config::spotify_app`], which
/// answers with the built-in application unless one of the caller's own was
/// configured — so the common path registers nothing at developer.spotify.com.
///
/// the token is cached under the run directory rather than rspotify's default
/// `.spotify_token_cache.json` in the cwd, so two migrations in two directories
/// do not overwrite each other's session.
///
/// [`Config::spotify_app`]: crate::config::Config::spotify_app
pub async fn connect(
    client_id: &str,
    redirect_uri: &str,
    cache_path: &Path,
) -> Result<AuthCodePkceSpotify> {
    // only reachable through `--spotify-client-id ''`; the resolved default is
    // never empty.
    if client_id.trim().is_empty() {
        return Err(Error::Config(
            "пустой spotify client id — уберите --spotify-client-id, чтобы \
             авторизоваться через встроенное приложение"
                .into(),
        ));
    }

    let creds = Credentials::new_pkce(client_id);

    let oauth = OAuth {
        redirect_uri: redirect_uri.to_owned(),
        // read scopes are needed to tell "already there" from "not there yet",
        // which is what makes a rerun idempotent instead of duplicating.
        scopes: scopes!(
            "user-library-read",
            "user-library-modify",
            "playlist-read-private",
            "playlist-modify-private",
            "playlist-modify-public",
            "user-follow-read",
            "user-follow-modify"
        ),
        ..Default::default()
    };

    let config = Config {
        cache_path: cache_path.to_path_buf(),
        token_cached: true,
        token_refreshing: true,
        ..Default::default()
    };

    let mut client = AuthCodePkceSpotify::with_config(creds, oauth, config);
    let url = client.get_authorize_url(None)?;

    // `prompt_for_token` reuses the cache when it can and only opens a browser
    // otherwise. its callback listener is a blocking `TcpListener`, so it parks
    // a worker thread for as long as the user takes — acceptable because it is
    // the one point in the run where nothing else is in flight.
    client.prompt_for_token(&url).await?;

    // the token is deliberately *not* proved here. `Spotify::new` resolves the
    // current user as its first act anyway, so a check would be the same
    // request twice in a row — and `/v1/me` is exactly where a shared client id
    // runs into its rate limit, so the duplicate is the one that trips it.
    Ok(client)
}
