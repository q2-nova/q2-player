//! Shared explicit-proxy, nonredirecting HTTPS-service client policy.
use std::time::Duration;

pub(crate) fn agent(proxy: Option<&str>, contact_url: &str) -> Result<crate::http::Agent, ()> {
    let contact = url::Url::parse(contact_url).map_err(|_| ())?;
    if contact.scheme() != "https"
        || contact.host_str().is_none()
        || !contact.username().is_empty()
        || contact.password().is_some()
        || contact.fragment().is_some()
        || contact.query().is_some()
    {
        return Err(());
    }
    let user_agent = format!("q2nd/{} ({contact})", env!("CARGO_PKG_VERSION"));
    let mut builder = crate::http::AgentBuilder::new().try_proxy_from_env(false);
    if let Some(proxy) = proxy {
        // ureq accepts some malformed proxy strings (including a path)
        // as a different host/port. Require an unambiguous proxy authority.
        let parsed = url::Url::parse(proxy).map_err(|_| ())?;
        if !matches!(
            parsed.scheme(),
            "http" | "socks" | "socks4" | "socks4a" | "socks5"
        ) || parsed.host_str().is_none()
            || !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(());
        }
        builder = builder.proxy(crate::http::Proxy::new(proxy).map_err(|_| ())?);
    }
    Ok(builder
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .timeout_connect(Duration::from_secs(8))
        .user_agent(&user_agent)
        .build())
}

pub(crate) enum BodyError {
    Transport,
    Protocol,
}

/// Preserve the services' decoded-size limit and error distinction. Socket
/// waits own only the async body; gzip and JSON are finite worker jobs.
pub(crate) async fn json(response: crate::http::Response, limit: usize) -> Result<serde_json::Value, BodyError> {
    let mut body = response.into_decoded();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.chunk().await {
        let chunk = chunk.map_err(|_| BodyError::Transport)?;
        if chunk.len() > limit.saturating_sub(bytes.len()) { return Err(BodyError::Protocol); }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes).map_err(|_| BodyError::Protocol))
        .await.map_err(|_| BodyError::Transport)?
}

#[cfg(test)]
pub(crate) fn wait<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}
