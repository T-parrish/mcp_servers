//! The browser half of Spotify's Authorization Code flow with PKCE.
//!
//! No client secret is involved: a random *verifier* is kept in memory, its
//! SHA-256 *challenge* travels in the authorize URL, and the verifier is
//! replayed when redeeming the code (see [`SpotifyClient::exchange_code`]).
//!
//! Spotify only permits plain `http` redirect URIs on the loopback interface, so
//! the callback is captured by a one-shot listener bound to the host and port of
//! the configured redirect URI. Token exchange itself lives in
//! [`crate::spotify`], which owns the HTTP client, rate limiter and telemetry.
//!
//! [`SpotifyClient::exchange_code`]: crate::spotify::SpotifyClient::exchange_code

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rand::RngCore;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";

/// Page shown in the browser once the redirect has been captured.
const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Authorized</title>\
    <body style=\"font-family:system-ui;text-align:center;padding-top:4rem\">\
    <h2>Spotify authorization complete</h2><p>You can close this tab and return to your \
    assistant.</p>";

/// A PKCE verifier and its derived challenge.
pub(crate) struct Pkce {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
}

/// Generate a fresh PKCE pair: 32 random bytes as the base64url verifier, and
/// its SHA-256 digest (also base64url) as the `S256` challenge.
pub(crate) fn pkce() -> Pkce {
    let verifier = random_b64(32);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// A random, URL-safe string used as the CSRF `state` parameter.
pub(crate) fn state() -> String {
    random_b64(16)
}

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    B64.encode(buf)
}

/// Build the URL the user opens to grant access.
pub(crate) fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<String> {
    let mut url = Url::parse(AUTHORIZE_URL).expect("the authorize URL is a valid constant");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("state", state);
    Ok(url.into())
}

/// Ask the OS to open `url` in the default browser. Best effort — the caller
/// always also surfaces the URL so the user can open it by hand.
pub(crate) fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };

    let status = command
        .arg(url)
        // The child must not inherit stdout: that is the MCP protocol stream.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("could not launch the browser")?;
    if !status.success() {
        bail!("browser launcher exited with {status}");
    }
    Ok(())
}

/// Bind the loopback listener for `redirect_uri`.
///
/// Separate from [`wait_for_code`] so the port is already held when the
/// authorize URL is handed to the browser — otherwise a fast redirect could
/// arrive before anything is listening.
pub(crate) async fn bind_callback(redirect_uri: &str) -> Result<TcpListener> {
    let url = Url::parse(redirect_uri).context("SPOTIFY_REDIRECT_URI is not a valid URL")?;
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url
        .port()
        .context("SPOTIFY_REDIRECT_URI must include an explicit port")?;
    TcpListener::bind((host, port))
        .await
        .with_context(|| format!("could not listen on {host}:{port} for the OAuth redirect"))
}

/// Serve loopback requests until Spotify's redirect arrives, and return its
/// `code`. Requests for any other path (favicon, stray tabs) are answered with a
/// 404 and ignored.
#[tracing::instrument(
    name = "oauth callback",
    skip_all,
    fields(otel.kind = "server", timeout_secs = timeout.as_secs()),
    err,
)]
pub(crate) async fn wait_for_code(
    listener: TcpListener,
    redirect_uri: &str,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let path = Url::parse(redirect_uri)
        .context("SPOTIFY_REDIRECT_URI is not a valid URL")?
        .path()
        .to_string();

    tokio::time::timeout(timeout, async {
        loop {
            let (mut socket, peer) = listener.accept().await.context("accepting the redirect")?;
            let Some(target) = read_request_target(&mut socket).await else {
                continue;
            };
            tracing::debug!(%peer, %target, "loopback request");

            // The request target is origin-form ("/callback?code=..."); resolve
            // it against a dummy base to reuse the URL query parser.
            let url = match Url::parse("http://127.0.0.1").unwrap().join(&target) {
                Ok(url) if url.path() == path => url,
                _ => {
                    let _ = respond(&mut socket, "404 Not Found", "not found").await;
                    continue;
                }
            };

            let param = |k: &str| {
                url.query_pairs()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.into_owned())
            };
            if let Some(error) = param("error") {
                let _ = respond(&mut socket, "200 OK", "Authorization denied.").await;
                bail!("Spotify returned an authorization error: {error}");
            }
            if param("state").as_deref() != Some(expected_state) {
                let _ = respond(&mut socket, "400 Bad Request", "state mismatch").await;
                bail!("the redirect carried an unexpected `state`; ignoring it");
            }
            let code = param("code").context("the redirect carried no `code`")?;
            let _ = respond(&mut socket, "200 OK", DONE_PAGE).await;
            tracing::info!("captured the authorization code");
            return Ok(code);
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out after {}s waiting for the redirect",
            timeout.as_secs()
        )
    })?
}

/// Read the request target out of the HTTP request line (`GET <target> HTTP/1.1`).
/// The request line always fits in the first read, so the body is never consumed.
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = [0u8; 4096];
    let n = socket.read(&mut buf).await.ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET").then(|| target.to_string())
}

async fn respond(socket: &mut tokio::net::TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    /// Bind an ephemeral port and return the listener plus its redirect URI.
    async fn listener() -> (TcpListener, String) {
        let listener = bind_callback("http://127.0.0.1:0/callback").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("http://127.0.0.1:{port}/callback"))
    }

    /// Play the browser: send one GET and read the response back.
    async fn get(redirect_uri: &str, target: &str) -> String {
        let addr = Url::parse(redirect_uri).unwrap();
        let mut socket = TcpStream::connect((addr.host_str().unwrap(), addr.port().unwrap()))
            .await
            .unwrap();
        socket
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn captures_the_code_and_ignores_unrelated_requests() {
        let (listener, uri) = listener().await;
        let waiter = tokio::spawn({
            let uri = uri.clone();
            async move { wait_for_code(listener, &uri, "st4te", Duration::from_secs(5)).await }
        });

        // A stray request on another path must not end the wait.
        assert!(get(&uri, "/favicon.ico").await.contains("404"));
        assert!(
            get(&uri, "/callback?code=abc123&state=st4te")
                .await
                .contains("200 OK")
        );

        assert_eq!(waiter.await.unwrap().unwrap(), "abc123");
    }

    #[tokio::test]
    async fn rejects_a_redirect_with_the_wrong_state() {
        let (listener, uri) = listener().await;
        let waiter = tokio::spawn({
            let uri = uri.clone();
            async move { wait_for_code(listener, &uri, "st4te", Duration::from_secs(5)).await }
        });
        get(&uri, "/callback?code=abc123&state=forged").await;
        assert!(waiter.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn surfaces_a_denied_authorization() {
        let (listener, uri) = listener().await;
        let waiter = tokio::spawn({
            let uri = uri.clone();
            async move { wait_for_code(listener, &uri, "st4te", Duration::from_secs(5)).await }
        });
        get(&uri, "/callback?error=access_denied&state=st4te").await;
        let error = waiter.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("access_denied"), "{error}");
    }

    #[test]
    fn the_challenge_is_the_base64url_sha256_of_the_verifier() {
        let pkce = pkce();
        assert_ne!(pkce.verifier, pkce.challenge);
        assert_eq!(
            pkce.challenge,
            B64.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
        // RFC 7636 requires a verifier of 43–128 characters.
        assert!((43..=128).contains(&pkce.verifier.len()));
    }

    #[test]
    fn the_authorize_url_carries_the_pkce_challenge() {
        let pkce = pkce();
        let url = authorize_url(
            "cid",
            "http://127.0.0.1:8888/callback",
            "scope-a",
            &pkce,
            "st",
        )
        .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let param = |k: &str| {
            parsed
                .query_pairs()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(param("client_id").as_deref(), Some("cid"));
        assert_eq!(param("response_type").as_deref(), Some("code"));
        assert_eq!(param("code_challenge_method").as_deref(), Some("S256"));
        assert_eq!(param("code_challenge"), Some(pkce.challenge));
        assert_eq!(param("state").as_deref(), Some("st"));
    }
}
