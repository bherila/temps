//! HTTP reverse proxy from Temps to external plugin processes over Unix socket.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use temps_auth::AuthContext;
use tracing::{debug, error};

type HmacSha256 = Hmac<Sha256>;

const HEADER_PLUGIN: &str = "x-temps-plugin";
const HEADER_USER_ID: &str = "x-temps-user-id";
const HEADER_USER_EMAIL: &str = "x-temps-user-email";
const HEADER_USER_ROLE: &str = "x-temps-user-role";
const HEADER_REQUEST_ID: &str = "x-temps-request-id";
const HEADER_AUTH_SIGNATURE: &str = "x-temps-auth-signature";

/// Proxy configuration for a single external plugin.
#[derive(Debug, Clone)]
pub struct PluginProxy {
    /// Unix socket path to the plugin
    pub socket_path: PathBuf,
    /// Plugin name for header injection
    pub plugin_name: String,
    /// HMAC secret for authenticating proxied requests
    pub auth_secret: String,
}

impl PluginProxy {
    pub fn new(socket_path: PathBuf, plugin_name: String, auth_secret: String) -> Self {
        Self {
            socket_path,
            plugin_name,
            auth_secret,
        }
    }
}

/// Create an axum Router that proxies all requests to an external plugin.
///
/// Mounts at `/x/{plugin_name}` — strips the prefix before forwarding.
/// Adds Temps headers (user context, request ID, auth signature).
///
/// Uses explicit wildcard routes instead of `.fallback()` because axum
/// fallbacks are lost when routers are `.merge()`d together.
pub fn create_plugin_proxy_router(proxy: PluginProxy) -> Router {
    use axum::routing::any;

    Router::new()
        .route("/", any(proxy_handler))
        .route("/{*rest}", any(proxy_handler))
        .with_state(proxy)
}

/// The actual proxy handler — forwards requests to the plugin over Unix socket.
async fn proxy_handler(State(proxy): State<PluginProxy>, request: Request) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();

    debug!(
        plugin = %proxy.plugin_name,
        method = %method,
        path = %path,
        "Proxying request to external plugin"
    );

    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(path);

    match forward_to_unix_socket(&proxy, request, path_and_query).await {
        Ok(response) => response,
        Err(e) => {
            error!(
                plugin = %proxy.plugin_name,
                path = %path,
                "Failed to proxy request to plugin: {}", e
            );
            (
                StatusCode::BAD_GATEWAY,
                format!("Plugin '{}' unavailable: {}", proxy.plugin_name, e),
            )
                .into_response()
        }
    }
}

/// Forward an HTTP request to a plugin over its Unix domain socket.
async fn forward_to_unix_socket(
    proxy: &PluginProxy,
    original_request: Request,
    path_and_query: &str,
) -> Result<Response, String> {
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&proxy.socket_path).await.map_err(|e| {
        format!(
            "Cannot connect to plugin socket {}: {}",
            proxy.socket_path.display(),
            e
        )
    })?;

    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| format!("HTTP handshake failed: {}", e))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            // hyper reports "error shutting down connection" when the Unix
            // socket closes without a clean TCP shutdown — this is expected
            // for short-lived HTTP/1.1 connections over Unix sockets.
            debug!("Plugin proxy connection closed: {}", e);
        }
    });

    let auth = original_request
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or_else(|| "Authentication required for external plugin proxy".to_string())?;

    let request_id = uuid::Uuid::new_v4().to_string();
    let (mut parts, body) = original_request.into_parts();

    // Never forward caller-controlled Temps protocol headers. They are trusted
    // by plugin SDKs, so the proxy must derive them only from Temps auth state.
    parts.headers.remove(HEADER_USER_ID);
    parts.headers.remove(HEADER_USER_EMAIL);
    parts.headers.remove(HEADER_USER_ROLE);
    parts.headers.remove(HEADER_REQUEST_ID);
    parts.headers.remove(HEADER_AUTH_SIGNATURE);
    parts.headers.remove(HEADER_PLUGIN);

    parts.headers.insert(
        HEADER_PLUGIN,
        proxy
            .plugin_name
            .parse()
            .unwrap_or_else(|_| hyper::header::HeaderValue::from_static("unknown")),
    );
    if let Some(user) = auth.user.as_ref() {
        parts.headers.insert(
            HEADER_USER_ID,
            HeaderValue::from_str(&user.id.to_string()).map_err(|e| {
                format!(
                    "Invalid authenticated user ID header for plugin proxy: {}",
                    e
                )
            })?,
        );
        parts.headers.insert(
            HEADER_USER_EMAIL,
            HeaderValue::from_str(&user.email).map_err(|e| {
                format!(
                    "Invalid authenticated user email header for plugin proxy: {}",
                    e
                )
            })?,
        );
    }
    let role = auth.effective_role.to_string();
    parts.headers.insert(
        HEADER_USER_ROLE,
        HeaderValue::from_str(&role).map_err(|e| {
            format!(
                "Invalid authenticated user role header for plugin proxy: {}",
                e
            )
        })?,
    );
    parts.headers.insert(
        HEADER_REQUEST_ID,
        HeaderValue::from_str(&request_id)
            .map_err(|e| format!("Invalid request ID header for plugin proxy: {}", e))?,
    );

    let target_uri: hyper::Uri = path_and_query
        .parse()
        .map_err(|e| format!("Invalid URI '{}': {}", path_and_query, e))?;
    parts.uri = target_uri;

    let user_id = auth
        .user
        .as_ref()
        .map(|user| user.id.to_string())
        .unwrap_or_default();
    let signature = sign_plugin_request(
        &proxy.auth_secret,
        parts.method.as_str(),
        path_and_query,
        &request_id,
        &user_id,
        &role,
    )?;
    parts.headers.insert(
        HEADER_AUTH_SIGNATURE,
        HeaderValue::from_str(&signature)
            .map_err(|e| format!("Invalid auth signature header for plugin proxy: {}", e))?,
    );

    parts.headers.insert(
        hyper::header::HOST,
        hyper::header::HeaderValue::from_static("localhost"),
    );

    let forwarded_request = Request::from_parts(parts, body);

    let response = sender
        .send_request(forwarded_request)
        .await
        .map_err(|e| format!("Plugin request failed: {}", e))?;

    let (parts, body) = response.into_parts();
    let body = Body::new(body);
    Ok(Response::from_parts(parts, body))
}

fn sign_plugin_request(
    auth_secret: &str,
    method: &str,
    path_and_query: &str,
    request_id: &str,
    user_id: &str,
    role: &str,
) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(auth_secret.as_bytes())
        .map_err(|e| format!("Invalid plugin auth secret for HMAC signing: {}", e))?;
    mac.update(method.as_bytes());
    mac.update(b"\n");
    mac.update(path_and_query.as_bytes());
    mac.update(b"\n");
    mac.update(request_id.as_bytes());
    mac.update(b"\n");
    mac.update(user_id.as_bytes());
    mac.update(b"\n");
    mac.update(role.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_proxy_creation() {
        let proxy = PluginProxy::new(
            PathBuf::from("/tmp/test.sock"),
            "test-plugin".to_string(),
            "secret".to_string(),
        );
        assert_eq!(proxy.plugin_name, "test-plugin");
        assert_eq!(proxy.socket_path, PathBuf::from("/tmp/test.sock"));
    }
}
