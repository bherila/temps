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
    /// Per-process assertion secret for authenticating proxied requests.
    pub auth_secret: String,
    /// Route prefixes the plugin authenticates itself, from its manifest.
    /// See [`temps_core::external_plugin::manifest::PluginManifest::public_paths`].
    pub public_paths: Vec<String>,
    /// Instance key material for minting the caller's actor token.
    ///
    /// A *shared slot*, not a snapshot, and that distinction is the whole
    /// point: the proxy router is built once during plugin initialization,
    /// while the crypto is installed later, when the console router that
    /// serves plugin API calls finally exists. A cloned `Option` captured at
    /// build time is `None` forever — which is exactly the bug this replaced,
    /// and it presented as "no platform actor is available" with nothing in
    /// the logs, because minting was skipped silently.
    ///
    /// Empty on builds that never wired it: the plugin then gets no actor
    /// header and its API calls fail closed with `Unauthenticated`, rather
    /// than the proxy inventing an identity.
    pub actor_crypto: ActorCryptoSlot,
    /// Whether this plugin asked for any platform API access at all. A
    /// plugin that declares no capability is never handed an actor token —
    /// there is nothing it could legitimately do with one.
    pub wants_api: bool,
}

impl PluginProxy {
    pub fn new(socket_path: PathBuf, plugin_name: String, auth_secret: String) -> Self {
        Self {
            socket_path,
            plugin_name,
            auth_secret,
            public_paths: Vec::new(),
            actor_crypto: ActorCryptoSlot::default(),
            wants_api: false,
        }
    }

    /// Supply the key material used to mint actor tokens, and whether this
    /// plugin declared any API capability.
    pub fn with_actor_minting(mut self, crypto: ActorCryptoSlot, wants_api: bool) -> Self {
        self.actor_crypto = crypto;
        self.wants_api = wants_api;
        self
    }

    /// Declare the manifest's self-authenticating routes.
    pub fn with_public_paths(mut self, public_paths: Vec<String>) -> Self {
        self.public_paths = public_paths;
        self
    }

    /// Whether `path` is one the plugin gates itself.
    ///
    /// Prefix match, but only at a segment boundary: `/hooks/incoming` must
    /// not also open `/hooks/incoming-admin`.
    fn is_public(&self, path: &str) -> bool {
        self.public_paths.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
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

    // These endpoints accept assertions that only the platform may make.
    // They remain reachable over the plugin's private Unix socket for event
    // delivery and channel setup, but must never be exposed through the
    // wildcard user-facing proxy — doing so would let an external caller
    // borrow the proxy's trusted signature.
    if is_reserved_internal_path(path) {
        warn!(
            plugin = %proxy.plugin_name,
            method = %method,
            path = %path,
            "Rejecting external request to reserved plugin transport endpoint"
        );
        return problemdetails::new(StatusCode::NOT_FOUND)
            .with_title("Not Found")
            .with_detail("The requested plugin route does not exist.")
            .into_response();
    }

    // Authenticate here, because nothing else will. Ordinary routes enforce
    // auth per-handler via the `RequireAuth` extractor; a proxied plugin
    // route has no handler of ours to put that extractor in. The auth
    // middleware that runs ahead of us only *populates* `AuthContext` for
    // callers who presented a credential — it lets anonymous requests
    // through for the public ingest endpoints to handle themselves. So
    // without this check every `/api/x/<plugin>/…` path is reachable
    // unauthenticated, whatever the plugin behind it does.
    let auth = request.extensions().get::<AuthContext>().cloned();
    if auth.is_none() && !proxy.is_public(path) {
        warn!(
            plugin = %proxy.plugin_name,
            method = %method,
            path = %path,
            "Rejecting unauthenticated request to external plugin"
        );
        return problemdetails::new(StatusCode::UNAUTHORIZED)
            .with_title("Authentication Required")
            .with_detail(format!(
                "Plugin '{}' is only reachable by an authenticated caller",
                proxy.plugin_name
            ))
            .into_response();
    }

    debug!(
        plugin = %proxy.plugin_name,
        method = %method,
        path = %path,
        user_id = ?auth.as_ref().and_then(|a| a.user.as_ref()).map(|u| u.id),
        role = ?auth.as_ref().map(|a| a.effective_role.to_string()),
        "Proxying request to external plugin"
    );

    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(path);

    match forward_to_unix_socket(&proxy, auth.as_ref(), request, path_and_query).await {
        Ok(response) => response,
        Err(e) => {
            error!(
                plugin = %proxy.plugin_name,
                path = %path,
                "Failed to proxy request to plugin: {}", e
            );
            problemdetails::new(StatusCode::BAD_GATEWAY)
                .with_title("Plugin Unavailable")
                .with_detail("The plugin could not complete the request.")
                .into_response()
        }
    }
}

/// Whether a user-facing proxy path targets a platform-only transport route.
///
/// Descendants are reserved as well so future nested transport endpoints do
/// not become reachable accidentally. Segment-boundary matching keeps benign
/// routes such as `/_events-calendar` available to plugins.
fn is_reserved_internal_path(path: &str) -> bool {
    [PLUGIN_EVENTS_PATH, PLUGIN_CHANNEL_PATH]
        .into_iter()
        .any(|reserved| {
            path == reserved
                || path
                    .strip_prefix(reserved)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// Forward an HTTP request to a plugin over its Unix domain socket.
async fn forward_to_unix_socket(
    proxy: &PluginProxy,
    auth: Option<&AuthContext>,
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
    use temps_auth::permissions::Role;
    use tower::ServiceExt;

    fn test_proxy() -> PluginProxy {
        PluginProxy::new(
            PathBuf::from("/tmp/test.sock"),
            "example-plugin".to_string(),
            "s3cr3t".to_string(),
        )
    }

    fn test_user(id: i32, email: &str) -> temps_entities::users::Model {
        let now = chrono::Utc::now();
        temps_entities::users::Model {
            id,
            name: "Test User".to_string(),
            email: email.to_string(),
            password_hash: None,
            email_verified: true,
            email_verification_token: None,
            email_verification_expires: None,
            password_reset_token: None,
            password_reset_expires: None,
            must_change_password: false,
            deleted_at: None,
            mfa_secret: None,
            mfa_enabled: false,
            mfa_recovery_codes: None,
            oidc_subject: None,
            oidc_provider_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn header(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// The bug this file's `actor_crypto` doc describes, pinned.
    ///
    /// A proxy is built during plugin initialization; the signing key is
    /// installed later, when the console router exists. If the proxy held a
    /// snapshot it would mint nothing forever — and it would do so silently,
    /// surfacing to the user only as "no platform actor is available" from
    /// inside the plugin, with nothing on the platform side to explain it.
    #[test]
    fn a_key_installed_after_the_proxy_was_built_is_still_used() {
        let slot = ActorCryptoSlot::default();
        // Built first, exactly as `build_proxy_router_from` does.
        let proxy = test_proxy().with_actor_minting(slot.clone(), true);
        let auth =
            AuthContext::new_persisted_session(test_user(42, "dev@temps.sh"), Role::Admin, 7);

        let mut before = hyper::HeaderMap::new();
        inject_temps_headers(&mut before, &proxy, Some(&auth));
        assert!(
            header(&before, headers::ACTOR_TOKEN).is_none(),
            "no key installed yet, so there is nothing to sign with"
        );

        // ...and only now does the console install the key.
        slot.set(std::sync::Arc::new(
            temps_core::CookieCrypto::new("0123456789abcdef0123456789abcdef").expect("test key"),
        ));

        let mut after = hyper::HeaderMap::new();
        inject_temps_headers(&mut after, &proxy, Some(&auth));
        let token = header(&after, headers::ACTOR_TOKEN)
            .expect("the same proxy must mint once the key arrives");
        let verified = temps_core::external_plugin::actor::verify_plugin_actor_token(
            &temps_core::CookieCrypto::new("0123456789abcdef0123456789abcdef").expect("test key"),
            &token,
            &proxy.plugin_name,
            std::time::SystemTime::now(),
        )
        .expect("the minted token must verify");
        assert_eq!(verified.user_id, 42);
        assert_eq!(
            verified.principal,
            temps_core::external_plugin::actor::ActorPrincipal::Session { session_id: 7 }
        );
    }

    /// A plugin that declared no capability gets no token even with a key
    /// installed: there is nothing it could legitimately do with one.
    #[test]
    fn a_plugin_without_a_capability_is_never_given_an_actor() {
        let slot = ActorCryptoSlot::default();
        slot.set(std::sync::Arc::new(
            temps_core::CookieCrypto::new("0123456789abcdef0123456789abcdef").expect("test key"),
        ));
        let proxy = test_proxy().with_actor_minting(slot, false);
        let auth = AuthContext::new_session(test_user(42, "dev@temps.sh"), Role::Admin);

        let mut headers = hyper::HeaderMap::new();
        inject_temps_headers(&mut headers, &proxy, Some(&auth));
        assert!(header(&headers, headers::ACTOR_TOKEN).is_none());
    }

    #[test]
    fn injects_identity_from_auth_context() {
        let auth = AuthContext::new_session(test_user(42, "dev@temps.sh"), Role::Admin);
        let mut headers = hyper::HeaderMap::new();

        inject_temps_headers(&mut headers, &test_proxy(), Some(&auth));

        assert_eq!(header(&headers, headers::USER_ID).as_deref(), Some("42"));
        assert_eq!(
            header(&headers, headers::USER_EMAIL).as_deref(),
            Some("dev@temps.sh")
        );
        assert_eq!(
            header(&headers, headers::USER_ROLE).as_deref(),
            Some("admin")
        );
        assert!(header(&headers, headers::USER_PERMISSIONS)
            .is_some_and(|permissions| permissions.contains("projects:write")));
        assert_eq!(
            header(&headers, headers::PLUGIN_NAME).as_deref(),
            Some("example-plugin")
        );
        assert_eq!(
            header(&headers, headers::AUTH_SIGNATURE).as_deref(),
            Some("s3cr3t")
        );
        assert!(header(&headers, headers::REQUEST_ID).is_some());
    }

    #[test]
    fn client_supplied_identity_cannot_survive() {
        // A reader trying to reach the plugin as an admin user.
        let auth = AuthContext::new_session(test_user(7, "reader@temps.sh"), Role::Reader);
        let mut headers = hyper::HeaderMap::new();
        headers.insert(headers::USER_ID, "1".parse().unwrap());
        headers.insert(headers::USER_EMAIL, "admin@temps.sh".parse().unwrap());
        headers.insert(headers::USER_ROLE, "admin".parse().unwrap());
        headers.insert(headers::AUTH_SIGNATURE, "guessed".parse().unwrap());
        headers.insert(
            headers::USER_PERMISSIONS,
            "projects:write,users:manage".parse().unwrap(),
        );

        inject_temps_headers(&mut headers, &test_proxy(), Some(&auth));

        assert_eq!(header(&headers, headers::USER_ID).as_deref(), Some("7"));
        assert_eq!(
            header(&headers, headers::USER_EMAIL).as_deref(),
            Some("reader@temps.sh")
        );
        assert_eq!(
            header(&headers, headers::USER_ROLE).as_deref(),
            Some("reader")
        );
        assert_eq!(
            header(&headers, headers::AUTH_SIGNATURE).as_deref(),
            Some("s3cr3t")
        );
        let permissions = header(&headers, headers::USER_PERMISSIONS)
            .expect("the proxy must inject the reader's resolved permissions");
        assert!(permissions.split(',').any(|value| value == "projects:read"));
        assert!(!permissions
            .split(',')
            .any(|value| value == "projects:write"));
        assert!(!permissions.split(',').any(|value| value == "users:manage"));
    }

    /// The proxy may authenticate a request from either the browser's session
    /// cookie or an API key, but the plugin must never receive that original,
    /// platform-wide credential. It receives only the scoped identity headers
    /// (and, when enabled, its plugin-bound actor token).
    #[test]
    fn caller_credentials_are_stripped_before_proxying_to_a_plugin() {
        let auth = AuthContext::new_session(test_user(7, "reader@temps.sh"), Role::Reader);
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::COOKIE,
            "temps_session=platform-secret".parse().unwrap(),
        );
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Bearer platform-api-key".parse().unwrap(),
        );
        headers.insert(
            hyper::header::PROXY_AUTHORIZATION,
            "Basic proxy-secret".parse().unwrap(),
        );
        headers.insert("x-request-trace", "kept".parse().unwrap());

        inject_temps_headers(&mut headers, &test_proxy(), Some(&auth));

        assert!(!headers.contains_key(hyper::header::COOKIE));
        assert!(!headers.contains_key(hyper::header::AUTHORIZATION));
        assert!(!headers.contains_key(hyper::header::PROXY_AUTHORIZATION));
        assert_eq!(header(&headers, "x-request-trace").as_deref(), Some("kept"));
        assert_eq!(header(&headers, headers::USER_ID).as_deref(), Some("7"));
    }

    #[test]
    fn unknown_temps_prefixed_headers_are_stripped() {
        let auth = AuthContext::new_session(test_user(7, "reader@temps.sh"), Role::Reader);
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-temps-not-a-real-header", "hello".parse().unwrap());
        headers.insert("x-other-header", "kept".parse().unwrap());

        inject_temps_headers(&mut headers, &test_proxy(), Some(&auth));

        assert!(header(&headers, "x-temps-not-a-real-header").is_none());
        assert_eq!(header(&headers, "x-other-header").as_deref(), Some("kept"));
    }

    #[test]
    fn deployment_token_forwards_role_but_no_user() {
        // Deployment tokens carry no user row; the plugin must be able to
        // tell "authenticated as nobody in particular" from "user 0".
        let auth = AuthContext::new_deployment_token(1, Some(2), Some(3), 4, "ci".into(), vec![]);
        let mut headers = hyper::HeaderMap::new();

        inject_temps_headers(&mut headers, &test_proxy(), Some(&auth));

        assert!(header(&headers, headers::USER_ID).is_none());
        assert!(header(&headers, headers::USER_EMAIL).is_none());
        assert!(header(&headers, headers::USER_ROLE).is_some());
    }

    #[test]
    fn anonymous_route_forwards_no_identity_at_all() {
        // A self-authenticating route gets no identity headers rather than a
        // defaulted one — the plugin must not mistake anonymous for a reader.
        let mut headers = hyper::HeaderMap::new();
        headers.insert(headers::USER_ID, "1".parse().unwrap());
        headers.insert(headers::USER_ROLE, "admin".parse().unwrap());

        inject_temps_headers(&mut headers, &test_proxy(), None);

        assert!(header(&headers, headers::USER_ID).is_none());
        assert!(header(&headers, headers::USER_EMAIL).is_none());
        assert!(header(&headers, headers::USER_ROLE).is_none());
        // Still tagged as coming from the proxy.
        assert_eq!(
            header(&headers, headers::PLUGIN_NAME).as_deref(),
            Some("example-plugin")
        );
    }

    #[test]
    fn public_paths_match_on_segment_boundaries() {
        let proxy = test_proxy().with_public_paths(vec!["/hooks/incoming".into()]);

        assert!(proxy.is_public("/hooks/incoming"));
        assert!(proxy.is_public("/hooks/incoming/jobs"));
        // The obvious trap: a sibling route whose name merely starts the same.
        assert!(!proxy.is_public("/hooks/incoming-admin"));
        assert!(!proxy.is_public("/hooks/outgoing"));
    }

    #[test]
    fn no_public_paths_means_everything_is_gated() {
        let proxy = test_proxy();
        assert!(!proxy.is_public("/hooks/incoming/jobs"));
        assert!(!proxy.is_public("/"));
    }

    #[test]
    fn trailing_slash_in_manifest_does_not_change_matching() {
        let proxy = test_proxy().with_public_paths(vec!["/hooks/incoming/".into()]);
        assert!(proxy.is_public("/hooks/incoming"));
        assert!(proxy.is_public("/hooks/incoming/jobs"));
        assert!(!proxy.is_public("/hooks/incoming-admin"));
    }

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

    #[test]
    fn reserved_transport_paths_match_exactly_and_by_segment() {
        for path in [
            PLUGIN_EVENTS_PATH,
            "/_events/forge",
            PLUGIN_CHANNEL_PATH,
            "/_temps/channel/upgrade",
        ] {
            assert!(is_reserved_internal_path(path), "{path}");
        }

        for path in [
            "/_events-calendar",
            "/_temps/channel-status",
            "/_temps/channels",
            "/events",
        ] {
            assert!(!is_reserved_internal_path(path), "{path}");
        }
    }

    #[tokio::test]
    async fn external_proxy_rejects_reserved_transport_routes_before_forwarding() {
        // Even declaring these as public cannot override the platform-only
        // reservation. The socket does not exist; a 404 therefore proves the
        // handler rejected before attempting UDS forwarding/signature injection.
        let router = create_plugin_proxy_router(test_proxy().with_public_paths(vec![
            PLUGIN_EVENTS_PATH.to_string(),
            PLUGIN_CHANNEL_PATH.to_string(),
        ]));

        for uri in [
            "/_events",
            "/_events/forge?event=deployment.succeeded",
            "/_temps/channel",
            "/_temps/channel/upgrade?transport=websocket",
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("reserved-path request should build"),
                )
                .await
                .expect("proxy router should respond");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn direct_uds_delivery_still_reaches_reserved_event_endpoint() {
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use tokio::net::UnixListener;

        // Unix socket paths are capped at roughly 100 bytes on macOS; its
        // per-user temp directory is already long, so keep this unique path
        // under the short system temp root there. Other platforms use their
        // configured temp directory rather than assuming macOS' `/private`.
        #[cfg(target_os = "macos")]
        let socket_root = PathBuf::from("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let socket_root = std::env::temp_dir();
        let socket_path = socket_root.join(format!("tp-{}.sock", uuid::Uuid::new_v4()));
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping direct UDS delivery test: sandbox forbids socket bind at {}",
                    socket_path.display()
                );
                return;
            }
            Err(error) => panic!("test UDS should bind at {}: {error}", socket_path.display()),
        };
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let observed_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(observed_tx)));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("direct UDS connection");
            let _connection_result = hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        let observed_tx = observed_tx.clone();
                        async move {
                            let observed = (
                                request.uri().path().to_string(),
                                request
                                    .headers()
                                    .get(headers::AUTH_SIGNATURE)
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_string),
                            );
                            if let Some(tx) = observed_tx
                                .lock()
                                .expect("observation lock should remain available")
                                .take()
                            {
                                let _ = tx.send(observed);
                            }
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(StatusCode::NO_CONTENT)
                                    .body(Body::empty())
                                    .expect("test response should build"),
                            )
                        }
                    }),
                )
                .await;
        });

        let mut proxy = test_proxy();
        proxy.socket_path = socket_path.clone();
        let request = Request::builder()
            .uri(PLUGIN_EVENTS_PATH)
            .body(Body::empty())
            .expect("direct request should build");
        let response = forward_to_unix_socket(&proxy, None, request, PLUGIN_EVENTS_PATH)
            .await
            .expect("platform-internal UDS delivery should remain available");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let (observed_path, observed_signature) = observed_rx
            .await
            .expect("direct UDS server should observe the request");
        assert_eq!(observed_path, PLUGIN_EVENTS_PATH);
        assert_eq!(observed_signature.as_deref(), Some("s3cr3t"));
        server.await.expect("test UDS server should finish");
        tokio::fs::remove_file(&socket_path)
            .await
            .expect("test UDS socket should be removed");
    }
}
