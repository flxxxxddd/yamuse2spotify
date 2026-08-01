//! the yandex side: a stored token, or the device flow that produces one.

use yamuse::{Client, DeviceAuthOptions};

use crate::config::Store;
use crate::error::{Error, Result};

/// return a yandex client that has already proven its token works.
///
/// a stored token that no longer authenticates is dropped and the device flow
/// runs again, because the alternative is every later phase failing one call at
/// a time with the same underlying cause.
pub async fn connect(store: &mut Store) -> Result<Client> {
    if let Some(token) = store.config.yandex_token.clone() {
        let client = build(token)?;
        match client.init().await {
            Ok(_) => return Ok(client),
            Err(e) => {
                tracing::warn!(%e, "the stored yandex token no longer works, re-authenticating");
                store.config.yandex_token = None;
            }
        }
    }

    let token = device_flow().await?;
    store.config.yandex_token = Some(token.clone());
    store.save()?;

    let client = build(token)?;
    client.init().await?;
    Ok(client)
}

/// build a client with drift reporting wired up.
///
/// yamuse repairs a field whose type has changed rather than failing the call,
/// which is what keeps a private api usable — but silently. without this hook a
/// model that no longer matches the wire shows up as an empty section of the
/// library and nothing else, which is exactly as confusing as it sounds.
fn build(token: String) -> Result<Client> {
    Ok(Client::builder()
        .token(token)
        .on_field_repair(|repair| {
            if repair.is_lossy() {
                tracing::warn!(
                    path = %repair.path,
                    reason = %repair.reason,
                    "dropped a field whose type no longer matches the model"
                );
            } else {
                tracing::debug!(path = %repair.path, "narrowed a number");
            }
        })
        .build()?)
}

/// run the device flow, printing the code for the user to enter.
async fn device_flow() -> Result<String> {
    println!("\n  яндекс.музыку нужно авторизовать один раз.\n");

    let client = Client::anonymous()?;
    let token = client
        .device_auth(DeviceAuthOptions::default(), |code| {
            let url = code
                .verification_url
                .as_deref()
                .unwrap_or("https://ya.ru/device");
            let user_code = code.user_code.as_deref().unwrap_or("?");
            println!("  1. откройте  {url}");
            println!("  2. введите   {user_code}");
            println!("\n  жду подтверждения…");
        })
        .await?;

    token
        .access_token
        .ok_or_else(|| Error::Config("yandex returned no access token".into()))
}
