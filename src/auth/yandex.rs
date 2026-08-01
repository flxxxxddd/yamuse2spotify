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
        let client = Client::builder().token(token).build()?;
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

    let client = Client::builder().token(token).build()?;
    client.init().await?;
    Ok(client)
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
