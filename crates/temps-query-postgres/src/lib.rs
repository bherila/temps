//! PostgreSQL driver for temps-query
//!
//! Implements DataSource, Introspect, and Queryable traits for PostgreSQL.

use async_trait::async_trait;
use futures_util::{pin_mut, TryStreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use temps_query::{
    BoundedRows, Capability, ContainerCapabilities, ContainerInfo, ContainerPath, ContainerType,
    DataError, DataSource, DatasetSchema, EntityCountHint, EntityInfo, FieldDef, FieldType,
    Introspect, QueryBudget, QueryOptions, QueryResult, QueryStats, Queryable, Result,
};
use tokio_postgres::{types::ToSql, Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{debug, error, warn};

const MAX_QUERY_LIMIT: usize = 100;

/// Escape a SQL identifier by doubling any internal double-quote characters.
/// Prevents identifier injection when used inside `"..."` quoting.
fn escape_ident(name: &str) -> String {
    name.replace('"', "\"\"")
}

#[derive(Clone, Debug)]
struct PgQueryColumn {
    name: String,
    data_type: String,
    udt_kind: Option<String>,
}

fn pg_column_admission(column: &PgQueryColumn, row_budget: usize) -> Option<String> {
    let value = format!("__temps_source.\"{}\"", escape_ident(&column.name));
    let compression_aware_size = || {
        format!(
            "CASE WHEN {value} IS NULL THEN 4 WHEN PG_COLUMN_COMPRESSION({value}) IS NOT NULL \
             THEN {} ELSE PG_COLUMN_SIZE({value})::bigint * 8 + 64 END",
            row_budget.saturating_add(1)
        )
    };
    let expression = match column.data_type.as_str() {
        "character varying" | "character" | "text" | "xml" => {
            format!("COALESCE(OCTET_LENGTH({value})::bigint * 6 + 2, 4)")
        }
        "bytea" => format!("COALESCE(OCTET_LENGTH({value})::bigint * 2 + 8, 4)"),
        "json" => format!("COALESCE(OCTET_LENGTH({value}::text)::bigint * 6 + 8, 4)"),
        "jsonb" | "ARRAY" => compression_aware_size(),
        "boolean"
        | "smallint"
        | "integer"
        | "bigint"
        | "real"
        | "double precision"
        | "numeric"
        | "decimal"
        | "date"
        | "time without time zone"
        | "time with time zone"
        | "interval"
        | "timestamp without time zone"
        | "timestamp with time zone"
        | "uuid"
        | "money"
        | "inet"
        | "cidr"
        | "macaddr"
        | "macaddr8"
        | "bit"
        | "bit varying"
        | "oid"
        | "regclass"
        | "regtype"
        | "tsvector"
        | "tsquery"
        | "int4range"
        | "int8range"
        | "numrange"
        | "tsrange"
        | "tstzrange"
        | "daterange"
        | "int4multirange"
        | "int8multirange"
        | "nummultirange"
        | "tsmultirange"
        | "tstzmultirange"
        | "datemultirange" => {
            // Several apparently scalar PostgreSQL types are varlena and may
            // be TOAST-compressed (notably bit varying, ranges, tsvector, and
            // extension-backed domains). A compressed on-disk size is not a
            // safe upper bound for JSON expansion, so fail closed before
            // TO_JSONB just as we do for JSONB and arrays.
            compression_aware_size()
        }
        // PostgreSQL enum labels are limited to NAMEDATALEN (normally 63
        // bytes). Their JSON representation is therefore safely bounded
        // without invoking an arbitrary user-defined output function.
        "USER-DEFINED" if column.udt_kind.as_deref() == Some("e") => "386".to_string(),
        _ => return None,
    };
    Some(expression)
}

/// Admit rows from non-materializing per-column upper bounds. `TO_JSONB` is
/// located only in the admitted CASE arm, so rejected values are never encoded.
fn with_wire_row_budget(
    sql: &str,
    columns: &[PgQueryColumn],
    budget: QueryBudget,
) -> Result<String> {
    let admissions = columns
        .iter()
        .map(|column| pg_column_admission(column, budget.max_bytes))
        .collect::<Vec<_>>();
    // Preserve queryability when an extension defines an output type whose
    // expansion cannot be bounded safely. Only that field becomes JSON null;
    // supported fields in the same row remain available. The raw unsupported
    // value is never referenced by the projection, so PostgreSQL does not run
    // its arbitrary output function or detoast it for JSON conversion.
    let safe_source_sql = if columns.is_empty() {
        sql.to_string()
    } else {
        let projection = columns
            .iter()
            .zip(&admissions)
            .map(|(column, admission)| {
                let name = escape_ident(&column.name);
                if admission.is_some() {
                    format!("__temps_raw.\"{name}\" AS \"{name}\"")
                } else {
                    format!("NULL::text AS \"{name}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT {projection} FROM ({sql}) AS __temps_raw")
    };
    let estimates = admissions
        .into_iter()
        .map(|admission| admission.unwrap_or_else(|| "4".to_string()))
        .collect::<Vec<_>>();
    let max_cell = if estimates.is_empty() {
        "0".to_string()
    } else {
        format!("GREATEST({})", estimates.join(", "))
    };
    let key_overhead = columns.iter().fold(2usize, |total, column| {
        total.saturating_add(column.name.len().saturating_mul(6).saturating_add(4))
    });
    let row_size = if estimates.is_empty() {
        key_overhead.to_string()
    } else {
        format!("{key_overhead} + {}", estimates.join(" + "))
    };

    Ok(format!(
        "SELECT CASE WHEN __temps_max_cell <= {} AND __temps_row_size <= {} \
             THEN TO_JSONB(__temps_source) ELSE NULL END AS __temps_row, \
             __temps_row_size AS __temps_size, __temps_max_cell \
         FROM ({safe_source_sql}) AS __temps_source \
         CROSS JOIN LATERAL (SELECT {row_size}::bigint AS __temps_row_size, \
             {max_cell}::bigint AS __temps_max_cell) AS __temps_admission",
        budget.max_cell_bytes, budget.max_bytes
    ))
}

/// A certificate verifier that accepts all server certificates (including self-signed).
///
/// SECURITY: this verifies nothing — with it, TLS gives encryption against a
/// passive observer and no protection at all against an active one, who can
/// present any certificate and receive this service's admin password. It exists
/// because Temps-managed PostgreSQL clusters are brought up with self-signed
/// certificates that no root store can validate.
///
/// It is therefore the *second* thing tried, not the first: [`connect_with_tls`]
/// attempts real verification against the webpki roots and only falls back here
/// when that fails, so any server presenting a properly-issued certificate —
/// every managed provider, anything reached over the public internet — is now
/// genuinely authenticated. Do not call this path directly.
#[derive(Debug)]
struct AcceptAllVerifier;

impl rustls::client::danger::ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// PostgreSQL data source implementation
pub struct PostgresSource {
    client: Arc<Client>,
    database_name: String,
    /// `statement_timeout` is session-scoped. Serialize the SET + query pair so
    /// concurrent row/count calls cannot inherit one another's timeout.
    query_timeout_lock: Arc<tokio::sync::Mutex<()>>,
}

/// True when `input` begins with `keyword` on a token boundary.
///
/// Free function so the shared structural checks below can use it without a
/// `PostgresSource`.
fn starts_with_sql_keyword(input: &str, keyword: &str) -> bool {
    let Some(rest) = input.trim_start().strip_prefix(keyword) else {
        return false;
    };

    match rest.chars().next() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()),
    }
}

impl PostgresSource {
    /// Reject a database name that isn't a plausible PostgreSQL identifier.
    ///
    /// The typed `Config` above already makes injection impossible; this
    /// exists so a malformed segment fails fast with a clear message instead
    /// of a confusing connection error, and so the same rule is enforced no
    /// matter how a future caller builds its connection.
    ///
    /// Deliberately permissive about non-ASCII (PostgreSQL allows UTF-8
    /// identifiers) while rejecting the characters that carry meaning in a
    /// libpq keyword/value string: whitespace, `=`, `'` and `\`.
    fn validate_database_identifier(name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(DataError::InvalidQuery(
                "Database name cannot be empty".to_string(),
            ));
        }
        if name.len() > 63 {
            return Err(DataError::InvalidQuery(format!(
                "Database name is too long ({} bytes, max 63): {name}",
                name.len()
            )));
        }
        if let Some(bad) = name
            .chars()
            .find(|c| c.is_whitespace() || matches!(c, '=' | '\'' | '\\' | '\0'))
        {
            return Err(DataError::InvalidQuery(format!(
                "Database name contains an illegal character {bad:?}: {name}"
            )));
        }
        Ok(())
    }

    /// Create a new PostgreSQL data source
    pub async fn connect(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        Self::connect_with_policy(host, port, username, password, database, false).await
    }

    /// Create a PostgreSQL data source for a managed endpoint that must remain
    /// on the operator's private network.
    pub async fn connect_private(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        Self::connect_with_policy(host, port, username, password, database, true).await
    }

    async fn connect_with_policy(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        database: &str,
        private_only: bool,
    ) -> Result<Self> {
        // SECURITY: build the config with typed setters, never `format!`.
        //
        // This used to interpolate into a libpq keyword/value string:
        //
        //     format!("host={host} port={port} user={user} password={pw} dbname={db}")
        //
        // `database` is the first segment of a browser URL path, unvalidated,
        // and libpq strings are whitespace-separated key=value pairs where
        // `host` *appends* rather than replaces. So a path segment of
        // `nosuchdb host=evil.tld` produced a config listing a second host;
        // tokio-postgres tries hosts in order and falls through when the first
        // rejects the (nonexistent) database, handing the attacker-controlled
        // server this service's admin password in cleartext — worse here
        // because the TLS verifier accepts any certificate and there is a
        // plaintext fallback below.
        //
        // `Config` setters take values, not syntax, so no segment can inject a
        // parameter. The identifier check is belt-and-braces for the error
        // messages and to reject nonsense early.
        Self::validate_database_identifier(database)?;

        debug!(
            "Connecting to PostgreSQL: {}@{}:{}/{}",
            username, host, port, database
        );
        let client = if private_only {
            connect_with_private_tls_ladder(host, port, username, password, database).await?
        } else {
            connect_with_tls_ladder(host, port, username, password, database).await?
        };

        debug!(
            "Successfully connected to PostgreSQL database: {}",
            database
        );

        Ok(Self {
            client: Arc::new(client),
            database_name: database.to_string(),
            query_timeout_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Execute a raw SQL statement (no result rows expected).
    /// Used for DDL/admin operations like creating roles and granting privileges.
    pub async fn execute_raw(&self, sql: &str) -> Result<()> {
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| DataError::QueryFailed(format!("Execute failed: {}", e)))?;
        Ok(())
    }
}

/// Open a `tokio_postgres::Client` against a libpq-style connection string,
/// negotiating TLS with rustls + a verifier that accepts any server cert
/// (self-signed included). The spawned connection task is detached and
/// runs until the returned `Client` is dropped.
///
/// Public so probes (cluster health-checks, `pg_auto_failover` monitor
/// reads, etc.) outside this crate can reuse the same TLS posture without
/// re-implementing the verifier or wrestling with `MakeRustlsConnect`
/// directly.
/// Walk an error's `source()` chain and join messages with `: `.
/// `tokio_postgres::Error` displays only `"db error"` at the top level
/// — the actual reason (e.g. "password authentication failed",
/// "channel binding required") is one or two layers deeper.
fn format_chain<E: std::error::Error>(err: &E) -> String {
    let mut out = err.to_string();
    let mut cause: Option<&dyn std::error::Error> = err.source();
    while let Some(c) = cause {
        let s = c.to_string();
        if !s.is_empty() && !out.ends_with(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cause = c.source();
    }
    out
}

/// Reject subqueries and function-call syntax in an already string-stripped,
/// whitespace-normalised, lower-cased SQL fragment.
///
/// Shared by the PostgreSQL and MariaDB filter validators. It lives here, and
/// is called rather than copied, because the MariaDB backend previously kept a
/// hand-rolled variant that drifted: it looked only at the byte immediately
/// before `(` (so `sleep (5)` passed) and only checked `select` as a subquery
/// starter (so `IN (TABLE mysql.user)` passed on MySQL 8.0.19+). One
/// implementation, one set of tests, no drift.
pub fn reject_subqueries_and_function_calls(without_strings: &str) -> Result<()> {
    if without_strings.contains('(') {
        // PostgreSQL query expressions can start with SELECT, TABLE,
        // VALUES, or WITH. `IN (TABLE private_ids)` is a valid subquery and
        // must not be mistaken for a simple IN-list. Check every opening
        // parenthesis because query expressions can be nested in grouping
        // parentheses. Keyword boundaries avoid rejecting quoted columns
        // such as `"table"` or identifiers such as `selected_id`.
        const QUERY_EXPRESSION_STARTERS: [&str; 4] = ["select", "table", "values", "with"];
        let paren_starts_query_expression = without_strings.match_indices('(').any(|(idx, _)| {
            let after_paren = &without_strings[idx + 1..];
            QUERY_EXPRESSION_STARTERS
                .iter()
                .any(|keyword| starts_with_sql_keyword(after_paren, keyword))
        });
        if paren_starts_query_expression {
            return Err(DataError::InvalidQuery(
                "Subqueries are not allowed in the data browser".to_string(),
            ));
        }

        // Block function-call syntax. A keyword denylist cannot be complete:
        // data-returning functions such as query_to_xml/database_to_xml/
        // table_to_xml take their SQL payload as a *string literal*, which is
        // stripped before the denylist runs, so `1=1 AND query_to_xml('select
        // ... from users', true, false, '') IS NOT NULL` slips through and
        // exfiltrates other tables. Reject any identifier immediately
        // preceding `(` (i.e. a function call); only grouping parens and
        // `IN (...)`/`AND (...)`/`OR (...)`/`NOT (...)` are permitted. This
        // blocks explicit function-call syntax; PostgreSQL operators and
        // casts remain available to ordinary WHERE expressions.
        const PAREN_ALLOWED_PREFIXES: [&str; 4] = ["in", "and", "or", "not"];
        for (idx, _) in without_strings.match_indices('(') {
            let preceding = without_strings[..idx].trim_end();

            // A closing double quote immediately before `(` terminates a
            // quoted PostgreSQL identifier. A closing single quote can
            // occur here after string stripping when a Unicode-escaped
            // identifier uses PostgreSQL's optional `UESCAPE 'x'` clause.
            // Both forms are function calls. Do not allow quoted versions
            // of IN/AND/OR/NOT: quoted names are identifiers, never SQL
            // keywords.
            if matches!(preceding.chars().last(), Some('"' | '\'')) {
                return Err(DataError::InvalidQuery(
                    "Function calls are not allowed in the data browser".to_string(),
                ));
            }

            // PostgreSQL permits `$` and non-ASCII characters in unquoted
            // identifier continuations. Include them here so identifiers
            // such as `evil$function(...)` and `fünction(...)` cannot evade
            // the function-call check.
            let ident_rev: String = preceding
                .chars()
                .rev()
                .take_while(|c| {
                    c.is_ascii_alphanumeric() || *c == '_' || *c == '$' || !c.is_ascii()
                })
                .collect();
            let ident: String = ident_rev.chars().rev().collect();
            let before_ident = preceding[..preceding.len() - ident.len()].trim_end();
            let is_qualified = before_ident.ends_with('.');
            if !ident.is_empty()
                && (is_qualified || !PAREN_ALLOWED_PREFIXES.contains(&ident.as_str()))
            {
                return Err(DataError::InvalidQuery(
                    "Function calls are not allowed in the data browser".to_string(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResolvedHost {
    UnixSocket(String),
    Tcp {
        hostname: String,
        addresses: Vec<std::net::IpAddr>,
    },
}

impl ResolvedHost {
    fn permits_unverified_transport(&self) -> bool {
        match self {
            Self::UnixSocket(_) => true,
            Self::Tcp { addresses, .. } => {
                !addresses.is_empty() && addresses.iter().all(|address| ip_is_private(*address))
            }
        }
    }
}

/// Resolve a PostgreSQL host exactly once for the whole TLS ladder.
///
/// The resulting addresses are installed in `Config::hostaddr`, so
/// tokio-postgres dials the addresses approved here instead of resolving the
/// hostname again after the private-network decision. The original hostname
/// remains in `Config::host` for certificate/SNI verification.
async fn resolve_host_once(host: &str, port: u16) -> Result<ResolvedHost> {
    let host = host.trim();
    if host.is_empty() {
        return Err(DataError::ConnectionFailed(
            "PostgreSQL host cannot be empty".to_string(),
        ));
    }

    if host.starts_with('/') {
        return Ok(ResolvedHost::UnixSocket(host.to_string()));
    }

    let hostname = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_string();
    let resolved = tokio::net::lookup_host((hostname.as_str(), port))
        .await
        .map_err(|error| {
            DataError::ConnectionFailed(format!(
                "Failed to resolve PostgreSQL host '{hostname}' on port {port}: {error}"
            ))
        })?;

    let mut addresses = Vec::new();
    for address in resolved {
        if !addresses.contains(&address.ip()) {
            addresses.push(address.ip());
        }
    }
    if addresses.is_empty() {
        return Err(DataError::ConnectionFailed(format!(
            "PostgreSQL host '{hostname}' resolved to no addresses"
        )));
    }

    Ok(ResolvedHost::Tcp {
        hostname,
        addresses,
    })
}

/// Build the connection config for a resolved host and a given SSL mode.
///
/// Each original hostname is paired with one pre-resolved `hostaddr`. This
/// preserves hostname-based certificate verification while preventing a
/// second DNS lookup from changing the address between authorization and use.
/// Named rather than inlined so security regression tests assert against the
/// production config instead of a hand-rolled copy.
fn connect_config_with_ssl_mode(
    resolved_host: &ResolvedHost,
    port: u16,
    username: &str,
    password: &str,
    database: &str,
    ssl_mode: tokio_postgres::config::SslMode,
) -> tokio_postgres::Config {
    let mut cfg = tokio_postgres::Config::new();
    match resolved_host {
        ResolvedHost::UnixSocket(path) => {
            cfg.host(path);
        }
        ResolvedHost::Tcp {
            hostname,
            addresses,
        } => {
            for address in addresses {
                cfg.host(hostname).hostaddr(*address);
            }
        }
    }
    cfg.port(port)
        .user(username)
        .password(password)
        .dbname(database)
        .ssl_mode(ssl_mode);
    cfg
}

/// The config the TLS ladder's first rung uses. Test-facing wrapper over
/// [`connect_config_with_ssl_mode`] so a test cannot accidentally assert
/// against a different SSL mode than production uses.
#[cfg(test)]
fn connect_config_for(
    resolved_host: &ResolvedHost,
    port: u16,
    username: &str,
    password: &str,
    database: &str,
) -> tokio_postgres::Config {
    connect_config_with_ssl_mode(
        resolved_host,
        port,
        username,
        password,
        database,
        tokio_postgres::config::SslMode::Require,
    )
}

/// Connect with the credential-safe PostgreSQL transport ladder.
///
/// The hostname is resolved once, then the exact approved addresses are used
/// for every rung: verified TLS, self-signed TLS, and (for private addresses
/// only) cleartext. This prevents DNS rebinding between the private-network
/// check and the credential-bearing connection.
pub async fn connect_with_tls_ladder(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: &str,
) -> Result<Client> {
    connect_with_tls_ladder_policy(
        host,
        port,
        username,
        password,
        database,
        TransportPolicy::VerifiedPublicAllowed,
    )
    .await
}

/// Connect to a managed PostgreSQL endpoint that must remain private even when
/// it presents a publicly trusted certificate.
///
/// Managed cluster credentials never need to leave the operator's own host or
/// mesh. Rejecting public addresses before the first TLS rung prevents stored
/// topology mistakes or forged control-plane data from turning verified TLS
/// into authorization to release those credentials.
pub async fn connect_with_private_tls_ladder(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: &str,
) -> Result<Client> {
    connect_with_tls_ladder_policy(
        host,
        port,
        username,
        password,
        database,
        TransportPolicy::PrivateOnly,
    )
    .await
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TransportPolicy {
    VerifiedPublicAllowed,
    PrivateOnly,
}

async fn connect_with_tls_ladder_policy(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: &str,
    policy: TransportPolicy,
) -> Result<Client> {
    let resolved_host = resolve_host_once(host, port).await?;
    if policy == TransportPolicy::PrivateOnly && !resolved_host.permits_unverified_transport() {
        return Err(DataError::ConnectionFailed(format!(
            "Managed PostgreSQL endpoint '{host}' resolved outside the private network; refusing \
             to send cluster credentials even over verified TLS"
        )));
    }
    let config = |ssl_mode| {
        connect_config_with_ssl_mode(&resolved_host, port, username, password, database, ssl_mode)
    };

    // SECURITY: every TLS rung must carry `SslMode::Require`.
    // tokio-postgres defaults to `Prefer`, which silently accepts a raw socket
    // when a server refuses TLS and would bypass the explicit downgrade gate.
    let tls_config = config(tokio_postgres::config::SslMode::Require);

    match connect_with_verified_tls(&tls_config).await {
        Ok(client) => {
            debug!(host = %host, "Connected to PostgreSQL with verified TLS");
            Ok(client)
        }
        Err(verified_error) => {
            // Both weaker rungs are restricted to the exact addresses resolved
            // above. The driver receives those same addresses through
            // `hostaddr`, so DNS cannot change the destination after this gate.
            if !resolved_host.permits_unverified_transport() {
                return Err(DataError::ConnectionFailed(format!(
                    "PostgreSQL at '{host}' did not present a certificate that validates \
                     against the system roots, and at least one resolved address is outside \
                     the private network. Refusing to fall back to an unverified certificate \
                     or to cleartext, because either would hand this service's password to \
                     whoever answered. Install a valid certificate, or reach the database over \
                     a private address or the Temps mesh. (error: {})",
                    format_chain(&verified_error),
                )));
            }

            match connect_with_self_signed_tls_config(&tls_config).await {
                Ok(client) => {
                    warn!(
                        host = %host,
                        "PostgreSQL certificate did not validate against the system roots ({}); \
                         connected with an unverified (self-signed) certificate. Traffic is \
                         encrypted but the server is NOT authenticated. Permitted only because \
                         every resolved address is loopback or private.",
                        format_chain(&verified_error)
                    );
                    Ok(client)
                }
                Err(tls_error) => {
                    warn!(
                        host = %host,
                        "PostgreSQL refused TLS ({}); falling back to an UNENCRYPTED connection. \
                         Permitted only because every resolved address is loopback or private.",
                        format_chain(&tls_error)
                    );

                    let plain_config = config(tokio_postgres::config::SslMode::Disable);
                    let (client, connection) =
                        plain_config.connect(NoTls).await.map_err(|error| {
                            DataError::ConnectionFailed(format!(
                                "PostgreSQL connection to '{host}:{port}/{database}' failed \
                             (TLS error: {}, plain error: {})",
                                format_chain(&tls_error),
                                format_chain(&error),
                            ))
                        })?;

                    tokio::spawn(async move {
                        if let Err(error) = connection.await {
                            error!("PostgreSQL connection error: {}", error);
                        }
                    });
                    Ok(client)
                }
            }
        }
    }
}

/// Whether `token` appears in `sql` as a whole SQL token.
///
/// Shared by the PostgreSQL and MariaDB denylists, which previously used a raw
/// `contains(" keyword ")`. Substring matching produced false *rejections* on
/// ordinary columns — `payload = 'x'` matched `"load "`, `charset = 'utf8'`
/// matched `"set "`, and any Postgres column literally named `commit`, `update`
/// or `copy` was unfilterable. That is not a cosmetic problem: a validator that
/// blocks legitimate queries is a validator someone eventually switches off.
///
/// Expects already-lowercased, whitespace-normalised, string-stripped input.
/// A token boundary is anything that cannot continue an identifier, matching
/// the rule `starts_with_sql_keyword` uses at the other end.
pub fn contains_sql_token(sql: &str, token: &str) -> bool {
    let is_ident_char =
        |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();

    let mut from = 0;
    while let Some(found) = sql[from..].find(token) {
        let start = from + found;
        let end = start + token.len();

        // SECURITY: a numeric literal does not extend into the keyword after it.
        //
        // Digits are identifier *continuation* characters but cannot *start* an
        // identifier, so a run like `1e0` before `union` is a number and the
        // keyword after it is a separate token — which is exactly how MySQL's
        // and pre-15 PostgreSQL's lexers read `1e0union select ...`. Testing
        // only the single preceding character missed that, and because the old
        // `contains("union ")` form did catch it, switching to token matching
        // reopened a UNION injection. Walk the whole preceding identifier run
        // and treat it as a boundary when it cannot be an identifier at all.
        let mut preceding = sql[..start].chars().rev().take_while(|c| is_ident_char(*c));
        let before_ok = match preceding.next() {
            // Nothing identifier-ish before the token: a clean boundary.
            None => true,
            // Something is there. It is only a real identifier if the run's
            // FIRST character could start one; `preceding` is reversed, so the
            // run's first character is whatever it yields last.
            Some(nearest) => {
                let first = preceding.last().unwrap_or(nearest);
                first.is_ascii_digit()
            }
        };
        let after_ok = sql[end..].chars().next().is_none_or(|c| !is_ident_char(c));

        if before_ok && after_ok {
            return true;
        }
        from = start + token.len().max(1);
        if from >= sql.len() {
            break;
        }
    }
    false
}

/// Collapse every run of whitespace to a single ASCII space.
///
/// SQL lexers treat space, tab, newline, carriage return, form feed and
/// vertical tab identically, so any keyword denylist that matches
/// `"keyword "` must see normalised input or it can be stepped around with a
/// different separator. Shared by the Postgres and MariaDB validators.
pub fn normalize_sql_whitespace(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_space = false;
    for ch in sql.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out
}

pub async fn connect_with_self_signed_tls(
    config: &str,
) -> std::result::Result<Client, tokio_postgres::Error> {
    let parsed: tokio_postgres::Config = config.parse()?;
    connect_with_self_signed_tls_config(&parsed).await
}

/// As [`connect_with_self_signed_tls`], but takes an already-built
/// [`tokio_postgres::Config`].
///
/// Preferred whenever any component of the connection is caller-supplied: a
/// `Config` carries values, so nothing in it can be mistaken for connection
/// syntax the way an interpolated keyword/value string can.
/// Whether an address is one the operator's own machine or private network.
///
/// Split out from [`ResolvedHost::permits_unverified_transport`] so the classification itself is
/// synchronous and unit-testable without a resolver.
fn ip_is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                // Carrier-grade NAT (100.64.0.0/10) — Tailscale and several
                // mesh VPNs hand out addresses here.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique local (fc00::/7) and link-local (fe80::/10).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: classify by the embedded v4 address rather than
                // treating ::ffff:8.8.8.8 as an opaque v6 address.
                || v6.to_ipv4_mapped().is_some_and(|v4| ip_is_private(v4.into()))
        }
    }
}

/// Open a client with genuine certificate verification against the webpki root
/// store.
///
/// This is the rung above [`connect_with_self_signed_tls_config`]: a server
/// presenting a properly-issued certificate is authenticated here, so an active
/// attacker on the path cannot substitute their own and collect the password.
/// Only when this fails does the caller drop to the accept-anything verifier
/// that Temps-managed self-signed clusters need.
async fn connect_with_verified_tls(
    config: &tokio_postgres::Config,
) -> std::result::Result<Client, tokio_postgres::Error> {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let rustls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let tls = MakeRustlsConnect::new(rustls_config);
    let (client, connection) = config.connect(tls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("PostgreSQL TLS connection error: {}", e);
        }
    });

    Ok(client)
}

pub async fn connect_with_self_signed_tls_config(
    config: &tokio_postgres::Config,
) -> std::result::Result<Client, tokio_postgres::Error> {
    // `ClientConfig::builder()` panics when no process-wide CryptoProvider
    // is installed AND the rustls features can't auto-pick one (e.g. both
    // `aws-lc-rs` and `ring` enabled, or neither). The main binary
    // installs ring's provider during setup, but tests and short-lived
    // tools that exercise this code path without going through setup
    // would crash. Install lazily here — `install_default` returns Err if
    // a provider is already installed, which we deliberately ignore.
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let rustls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier))
        .with_no_client_auth();

    let tls = MakeRustlsConnect::new(rustls_config);
    let (client, connection) = config.connect(tls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("PostgreSQL TLS connection error: {}", e);
        }
    });

    Ok(client)
}

impl PostgresSource {
    /// Validate that a sort_by field name is a safe SQL identifier.
    /// Only allows alphanumeric characters, underscores, and optionally
    /// double-quoted identifiers.
    fn validate_sort_field(sort_by: &str) -> Result<()> {
        let trimmed = sort_by.trim();
        if trimmed.is_empty() {
            return Err(DataError::InvalidQuery(
                "Sort field cannot be empty".to_string(),
            ));
        }

        // Allow double-quoted identifiers.
        //
        // The length guard is load-bearing: for the single character `"` both
        // `starts_with` and `ends_with` are true, so the slice below became
        // `&s[1..0]` and panicked — a remote panic reachable from
        // `?sort_by=%22` with only ExternalServicesRead, and from the agent's
        // --sort_by flag, making it prompt-injectable.
        if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            let inner = &trimmed[1..trimmed.len() - 1];
            // PostgreSQL has no zero-length delimited identifier, so `""`
            // could only ever reach the server as a syntax error.
            if inner.is_empty() || inner.contains('"') {
                return Err(DataError::InvalidQuery(
                    "Sort field identifier contains invalid characters".to_string(),
                ));
            }
            return Ok(());
        }

        // Allow only valid unquoted SQL identifiers: [a-zA-Z_][a-zA-Z0-9_]*
        // Also allow schema.column format
        for part in trimmed.split('.') {
            if part.is_empty() {
                return Err(DataError::InvalidQuery(
                    "Sort field contains empty path segment".to_string(),
                ));
            }
            if !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(DataError::InvalidQuery(format!(
                    "Sort field '{}' contains invalid characters. Only alphanumeric characters, underscores, and dots are allowed",
                    sort_by
                )));
            }
        }

        Ok(())
    }

    /// Reject raw SQL filter fragments. The public query API accepts JSON from
    /// low-privileged readers, so PostgreSQL must not concatenate caller-provided
    /// expressions into WHERE clauses. A future typed filter DSL can translate
    /// validated operators into bound parameters here.
    fn validate_filters(filters: Option<&serde_json::Value>) -> Result<()> {
        match filters {
            None => Ok(()),
            Some(value) if value.as_object().is_some_and(|object| object.is_empty()) => Ok(()),
            Some(_) => Err(DataError::InvalidQuery(
                "PostgreSQL data browsing does not accept raw SQL filters".to_string(),
            )),
        }
    }

    fn clamp_limit(limit: Option<usize>) -> usize {
        limit.unwrap_or(MAX_QUERY_LIMIT).min(MAX_QUERY_LIMIT)
    }

    /// Map PostgreSQL type to FieldType
    fn map_pg_type(pg_type: &str) -> FieldType {
        match pg_type {
            "boolean" | "bool" => FieldType::Boolean,
            "smallint" | "int2" => FieldType::Int32,
            "integer" | "int" | "int4" => FieldType::Int32,
            "bigint" | "int8" => FieldType::Int64,
            "real" | "float4" => FieldType::Float32,
            "double precision" | "float8" => FieldType::Float64,
            "numeric" | "decimal" => FieldType::Float64,
            "character varying" | "varchar" | "character" | "char" | "text" => FieldType::String,
            "bytea" => FieldType::Bytes,
            "date" => FieldType::Date,
            "timestamp"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "timestamptz" => FieldType::Timestamp,
            "json" | "jsonb" => FieldType::Json,
            "uuid" => FieldType::Uuid,
            _ => FieldType::String, // Default fallback
        }
    }

    async fn query_columns(
        &self,
        schema_name: &str,
        entity_name: &str,
    ) -> Result<Vec<PgQueryColumn>> {
        let rows = self
            .client
            .query(
                "SELECT columns.column_name, columns.data_type, types.typtype::text \
                 FROM information_schema.columns AS columns \
                 LEFT JOIN pg_catalog.pg_namespace AS namespaces \
                   ON namespaces.nspname = columns.udt_schema \
                 LEFT JOIN pg_catalog.pg_type AS types \
                   ON types.typnamespace = namespaces.oid AND types.typname = columns.udt_name \
                 WHERE columns.table_schema = $1 AND columns.table_name = $2 \
                 ORDER BY columns.ordinal_position",
                &[&schema_name, &entity_name],
            )
            .await
            .map_err(|_error| {
                DataError::SchemaError(format!(
                    "Failed to inspect PostgreSQL query columns for '{}.{}'",
                    schema_name, entity_name
                ))
            })?;
        Ok(rows
            .into_iter()
            .map(|row| PgQueryColumn {
                name: row.get(0),
                data_type: row.get(1),
                udt_kind: row.get(2),
            })
            .collect())
    }
}

#[async_trait]
impl DataSource for PostgresSource {
    fn source_type(&self) -> &'static str {
        "postgres"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Sql, Capability::TextSearch]
    }

    async fn list_containers(&self, path: &ContainerPath) -> Result<Vec<ContainerInfo>> {
        let client = &self.client;

        match path.depth() {
            // Depth 0: List databases
            0 => {
                debug!("Listing PostgreSQL databases");

                let query = r#"
                    SELECT
                        datname,
                        pg_database_size(datname) as size_bytes,
                        pg_get_userbyid(datdba) as owner,
                        pg_encoding_to_char(encoding) as encoding
                    FROM pg_database
                    WHERE datistemplate = false
                    ORDER BY datname
                "#;

                let rows = client.query(query, &[]).await.map_err(|e| {
                    DataError::QueryFailed(format!("Failed to list databases: {}", e))
                })?;

                let databases: Vec<ContainerInfo> = rows
                    .iter()
                    .map(|row| {
                        let name: String = row.get(0);
                        let size_bytes: Option<i64> = row.try_get(1).ok();
                        let owner: Option<String> = row.try_get(2).ok();
                        let encoding: Option<String> = row.try_get(3).ok();

                        let mut metadata = HashMap::new();
                        if let Some(size) = size_bytes {
                            metadata.insert("size_bytes".to_string(), serde_json::json!(size));
                        }
                        if let Some(own) = owner {
                            metadata.insert("owner".to_string(), serde_json::json!(own));
                        }
                        if let Some(enc) = encoding {
                            metadata.insert("encoding".to_string(), serde_json::json!(enc));
                        }

                        ContainerInfo {
                            name,
                            container_type: ContainerType::Database,
                            capabilities: ContainerCapabilities {
                                can_contain_containers: true,
                                can_contain_entities: false,
                                child_container_type: Some(ContainerType::Schema),
                                entity_type_label: None,
                                entity_count_hint: None,
                            },
                            metadata,
                        }
                    })
                    .collect();

                debug!("Found {} databases", databases.len());
                Ok(databases)
            }

            // Depth 1: List schemas in a database
            1 => {
                let database_name = &path.segments[0];

                // Check if we're connected to the right database
                if database_name != &self.database_name {
                    return Err(DataError::OperationNotSupported(format!(
                        "Cannot list schemas from database '{}' while connected to '{}'. Create a connection to that database.",
                        database_name, self.database_name
                    )));
                }

                debug!("Listing PostgreSQL schemas in database: {}", database_name);

                let query = r#"
                    SELECT
                        schema_name,
                        COUNT(table_name) as table_count
                    FROM information_schema.schemata
                    LEFT JOIN information_schema.tables
                        ON information_schema.tables.table_schema = information_schema.schemata.schema_name
                        -- Must match `list_entities`' filter exactly. This
                        -- counted BASE TABLE only, so once views became
                        -- browsable a schema advertised "3 tables" in the
                        -- container overview and then listed 4 entities.
                        AND table_type IN ('BASE TABLE', 'VIEW')
                    WHERE schema_name NOT IN ('information_schema')
                    GROUP BY schema_name
                    ORDER BY schema_name
                "#;

                let rows = client.query(query, &[]).await.map_err(|e| {
                    DataError::QueryFailed(format!("Failed to list schemas: {}", e))
                })?;

                let schemas: Vec<ContainerInfo> = rows
                    .iter()
                    .map(|row| {
                        let name: String = row.get(0);
                        let entity_count: i64 = row.try_get(1).unwrap_or(0);

                        let mut metadata = HashMap::new();
                        metadata
                            .insert("entity_count".to_string(), serde_json::json!(entity_count));

                        ContainerInfo {
                            name,
                            container_type: ContainerType::Schema,
                            capabilities: ContainerCapabilities {
                                can_contain_containers: false,
                                can_contain_entities: true,
                                child_container_type: None,
                                entity_type_label: Some("table".to_string()),
                                entity_count_hint: Some(EntityCountHint::Small),
                            },
                            metadata,
                        }
                    })
                    .collect();

                debug!(
                    "Found {} schemas in database '{}'",
                    schemas.len(),
                    database_name
                );
                Ok(schemas)
            }

            // Depth >= 2: Not supported
            _ => Err(DataError::InvalidQuery(format!(
                "PostgreSQL hierarchy only supports 2 levels (database/schema). Path depth: {}",
                path.depth()
            ))),
        }
    }

    async fn get_container_info(&self, path: &ContainerPath) -> Result<ContainerInfo> {
        let client = &self.client;

        match path.depth() {
            // Depth 1: Get database info
            1 => {
                let database_name = &path.segments[0];

                let query = r#"
                    SELECT
                        datname,
                        pg_database_size(datname) as size_bytes,
                        pg_get_userbyid(datdba) as owner,
                        pg_encoding_to_char(encoding) as encoding
                    FROM pg_database
                    WHERE datname = $1
                "#;

                let row = client
                    .query_one(query, &[database_name])
                    .await
                    .map_err(|e| {
                        DataError::NotFound(format!(
                            "Database '{}' not found: {}",
                            database_name, e
                        ))
                    })?;

                let name: String = row.get(0);
                let size_bytes: Option<i64> = row.try_get(1).ok();
                let owner: Option<String> = row.try_get(2).ok();
                let encoding: Option<String> = row.try_get(3).ok();

                let mut metadata = HashMap::new();
                if let Some(size) = size_bytes {
                    metadata.insert("size_bytes".to_string(), serde_json::json!(size));
                }
                if let Some(own) = owner {
                    metadata.insert("owner".to_string(), serde_json::json!(own));
                }
                if let Some(enc) = encoding {
                    metadata.insert("encoding".to_string(), serde_json::json!(enc));
                }

                Ok(ContainerInfo {
                    name,
                    container_type: ContainerType::Database,
                    capabilities: ContainerCapabilities {
                        can_contain_containers: true,
                        can_contain_entities: false,
                        child_container_type: Some(ContainerType::Schema),
                        entity_type_label: None,
                        entity_count_hint: None,
                    },
                    metadata,
                })
            }

            // Depth 2: Get schema info
            2 => {
                let database_name = &path.segments[0];
                let schema_name = &path.segments[1];

                if database_name != &self.database_name {
                    return Err(DataError::OperationNotSupported(format!(
                        "Cannot get schema info from database '{}' while connected to '{}'",
                        database_name, self.database_name
                    )));
                }

                let query = r#"
                    SELECT COUNT(*)
                    FROM information_schema.tables
                    WHERE table_schema = $1 AND table_type = 'BASE TABLE'
                "#;

                let row = client.query_one(query, &[schema_name]).await.map_err(|e| {
                    DataError::NotFound(format!("Schema '{}' not found: {}", schema_name, e))
                })?;

                let entity_count: i64 = row.get(0);

                let mut metadata = HashMap::new();
                metadata.insert("entity_count".to_string(), serde_json::json!(entity_count));

                Ok(ContainerInfo {
                    name: schema_name.clone(),
                    container_type: ContainerType::Schema,
                    capabilities: ContainerCapabilities {
                        can_contain_containers: false,
                        can_contain_entities: true,
                        child_container_type: None,
                        entity_type_label: Some("table".to_string()),
                        entity_count_hint: Some(EntityCountHint::Small),
                    },
                    metadata,
                })
            }

            _ => Err(DataError::InvalidQuery(format!(
                "Invalid path depth for get_container_info: {}",
                path.depth()
            ))),
        }
    }

    async fn list_entities(&self, container_path: &ContainerPath) -> Result<Vec<EntityInfo>> {
        // Must be at depth 2 (database/schema)
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery(format!(
                "list_entities requires path depth 2 (database/schema), got {}",
                container_path.depth()
            )));
        }

        let database_name = &container_path.segments[0];
        let schema_name = &container_path.segments[1];

        if database_name != &self.database_name {
            return Err(DataError::OperationNotSupported(format!(
                "Cannot list entities from database '{}' while connected to '{}'",
                database_name, self.database_name
            )));
        }

        let client = &self.client;

        debug!("Listing tables in schema: {}", schema_name);

        // Row counts and sizes come from the catalog in the same pass, so the
        // list can show them without N+1 per-table COUNT(*) queries. The
        // MariaDB backend already did this via information_schema; Postgres
        // returned nulls, so the same UI showed real numbers on one engine and
        // em dashes on the other.
        //
        // Views are included: they are browsable like tables and omitting them
        // made schemas look emptier than they are. reltuples/size are
        // meaningless for a view, so they are nulled out below rather than
        // reported as zero.
        let query = r#"
            SELECT
                t.table_schema,
                t.table_name,
                t.table_type,
                c.reltuples,
                pg_total_relation_size(c.oid) AS total_bytes
            FROM information_schema.tables t
            LEFT JOIN pg_namespace n ON n.nspname = t.table_schema
            LEFT JOIN pg_class c ON c.relname = t.table_name AND c.relnamespace = n.oid
            WHERE t.table_schema = $1
              AND t.table_type IN ('BASE TABLE', 'VIEW')
            ORDER BY t.table_name
        "#;

        let rows = client.query(query, &[schema_name]).await.map_err(|e| {
            DataError::QueryFailed(format!(
                "Failed to list tables in schema '{}': {}",
                schema_name, e
            ))
        })?;

        let entities: Vec<EntityInfo> = rows
            .iter()
            .map(|row| {
                let schema: String = row.get(0);
                let table_name: String = row.get(1);
                let table_type: String = row.get(2);
                let is_view = table_type == "VIEW";

                // A view has no heap of its own: reltuples is 0 and
                // pg_total_relation_size is 0. Reporting those as real figures
                // would claim "0 rows, 0 B" about something that may return
                // millions, so leave them unknown instead.
                let row_count = if is_view {
                    None
                } else {
                    row.try_get::<_, f32>(3)
                        .ok()
                        .filter(|estimate| *estimate >= 0.0)
                        .map(|estimate| estimate as usize)
                };
                let size_bytes = if is_view {
                    None
                } else {
                    row.try_get::<_, i64>(4).ok().map(|s| s.max(0) as u64)
                };

                EntityInfo {
                    namespace: schema,
                    name: table_name,
                    entity_type: table_type,
                    row_count,
                    size_bytes,
                    schema: None,
                    metadata: None,
                }
            })
            .collect();

        debug!(
            "Found {} tables in schema '{}'",
            entities.len(),
            schema_name
        );

        Ok(entities)
    }

    async fn get_entity_info(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<EntityInfo> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery(format!(
                "get_entity_info requires path depth 2 (database/schema), got {}",
                container_path.depth()
            )));
        }

        let database_name = &container_path.segments[0];
        let schema_name = &container_path.segments[1];

        if database_name != &self.database_name {
            return Err(DataError::OperationNotSupported(format!(
                "Cannot get entity info from database '{}' while connected to '{}'",
                database_name, self.database_name
            )));
        }

        let client = &self.client;

        let query = r#"
            SELECT table_type
            FROM information_schema.tables
            WHERE table_schema = $1 AND table_name = $2
        "#;

        let row = client
            .query_one(query, &[schema_name, &entity_name])
            .await
            .map_err(|e| {
                DataError::NotFound(format!(
                    "Table '{}.{}' not found: {}",
                    schema_name, entity_name, e
                ))
            })?;

        let table_type: String = row.get(0);

        // Row count + on-disk size. Both come from planner statistics for
        // anything large — see `row_count_and_size`.
        let (row_count, size_bytes) = self.row_count_and_size(schema_name, entity_name).await;

        Ok(EntityInfo {
            namespace: schema_name.clone(),
            name: entity_name.to_string(),
            entity_type: table_type,
            row_count,
            size_bytes,
            schema: Some(self.get_schema(container_path, entity_name).await?),
            metadata: None,
        })
    }

    async fn get_schema(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<DatasetSchema> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery(format!(
                "get_schema requires path depth 2 (database/schema), got {}",
                container_path.depth()
            )));
        }

        let database_name = &container_path.segments[0];
        let schema_name = &container_path.segments[1];

        if database_name != &self.database_name {
            return Err(DataError::OperationNotSupported(format!(
                "Cannot get schema from database '{}' while connected to '{}'",
                database_name, self.database_name
            )));
        }

        let client = &self.client;

        debug!("Getting schema for table: {}.{}", schema_name, entity_name);

        let query = r#"
            SELECT
                column_name,
                data_type,
                is_nullable,
                column_default
            FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = $2
            ORDER BY ordinal_position
        "#;

        let rows = client
            .query(query, &[schema_name, &entity_name])
            .await
            .map_err(|e| {
                DataError::SchemaError(format!(
                    "Failed to get schema for table '{}.{}': {}",
                    schema_name, entity_name, e
                ))
            })?;

        let fields: Vec<FieldDef> = rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let data_type: String = row.get(1);
                let is_nullable: String = row.get(2);
                let _column_default: Option<String> = row.get(3);

                FieldDef {
                    name,
                    field_type: Self::map_pg_type(&data_type),
                    nullable: is_nullable == "YES",
                    description: None,
                }
            })
            .collect();

        debug!(
            "Found {} columns for table '{}.{}'",
            fields.len(),
            schema_name,
            entity_name
        );

        Ok(DatasetSchema {
            fields,
            partitions: None,
            primary_key: None,
        })
    }

    async fn close(&self) -> Result<()> {
        debug!("Closing PostgreSQL connection");
        // Connection cleanup handled by Drop
        Ok(())
    }
}

#[async_trait]
impl Introspect for PostgresSource {
    async fn inspect_fields(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<Vec<FieldDef>> {
        let schema = self.get_schema(container_path, entity_name).await?;
        Ok(schema.fields)
    }

    async fn field_exists(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        field: &str,
    ) -> Result<bool> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery("Invalid path depth".to_string()));
        }

        let schema_name = &container_path.segments[1];
        let client = &self.client;

        let query = r#"
            SELECT COUNT(*)
            FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = $2 AND column_name = $3
        "#;

        let row = client
            .query_one(query, &[schema_name, &entity_name, &field])
            .await
            .map_err(|e| {
                DataError::QueryFailed(format!("Failed to check field existence: {}", e))
            })?;

        let count: i64 = row.get(0);
        Ok(count > 0)
    }

    async fn get_field_type(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        field: &str,
    ) -> Result<FieldType> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery("Invalid path depth".to_string()));
        }

        let schema_name = &container_path.segments[1];
        let client = &self.client;

        let query = r#"
            SELECT data_type
            FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = $2 AND column_name = $3
        "#;

        let row = client
            .query_one(query, &[schema_name, &entity_name, &field])
            .await
            .map_err(|e| {
                DataError::NotFound(format!(
                    "Field '{}' not found in table '{}.{}': {}",
                    field, schema_name, entity_name, e
                ))
            })?;

        let data_type: String = row.get(0);
        Ok(Self::map_pg_type(&data_type))
    }
}

#[async_trait]
impl Queryable for PostgresSource {
    async fn query(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        filters: Option<serde_json::Value>,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery(
                "Invalid path depth for query".to_string(),
            ));
        }

        let schema_name = &container_path.segments[1];
        let columns = self.query_columns(schema_name, entity_name).await?;

        let start = std::time::Instant::now();

        Self::validate_filters(filters.as_ref())?;
        let schema = self.get_schema(container_path, entity_name).await?;

        // Build SQL query with escaped identifiers only. PostgreSQL cannot bind
        // table/column identifiers as parameters, so all identifier inputs are
        // either escaped or validated against the discovered table schema.
        let mut sql = format!(
            "SELECT * FROM \"{}\".\"{}\"",
            escape_ident(schema_name),
            escape_ident(entity_name)
        );

        // Add ORDER BY
        if let Some(sort_by) = &options.sort_by {
            // SECURITY: trim ONCE, then validate and quote the same value.
            //
            // `validate_sort_field` trims internally, but the quoting decision
            // below used the raw string. A leading space therefore split them:
            // ` "a ASC,1 FROM pg_shadow--" ` validated as an already-quoted
            // identifier, but `starts_with('"')` was false on the untrimmed
            // value, so it got wrapped again and the payload landed *outside*
            // any quote pair, lexed as SQL by the server. No working exploit was
            // demonstrated — the double-wrap leaves unresolvable identifiers on
            // either side — but a validator and its emitter disagreeing about
            // the value on a raw-concatenation path is not something to leave
            // resting on that.
            let sort_by = sort_by.trim();
            Self::validate_sort_field(sort_by)?;
            if !schema.fields.iter().any(|field| field.name == *sort_by) {
                return Err(DataError::InvalidQuery(format!(
                    "Sort field '{}' is not present in table '{}.{}'",
                    sort_by, schema_name, entity_name
                )));
            }
            let sort_order = match options.sort_order.as_deref() {
                Some("desc") | Some("DESC") => "DESC",
                _ => "ASC",
            };
            sql.push_str(&format!(
                " ORDER BY \"{}\" {}",
                escape_ident(sort_by),
                sort_order
            ));
        }

        // Add LIMIT and OFFSET. Enforce a hard maximum so callers cannot request
        // unbounded result sets through the data browser endpoint.
        let limit = Self::clamp_limit(options.limit);
        let offset = options.offset.unwrap_or(0);
        sql.push_str(&format!(" LIMIT {} OFFSET {}", limit, offset));
        let sql = with_wire_row_budget(&sql, &columns, options.budget)?;

        debug!(
            entity = entity_name,
            limit, offset, "executing PostgreSQL data query"
        );

        // Safety: SQL injection is prevented by rejecting raw WHERE fragments,
        // escaping table identifiers, and validating sort columns against schema.
        // The database user should be read-only as defense-in-depth.
        let client = &self.client;
        let _timeout_guard = self.query_timeout_lock.lock().await;

        // SECURITY / AVAILABILITY: bound the query server-side before running it.
        //
        // The caller's tokio deadline frees the control-plane task, but dropping
        // the future does not stop PostgreSQL: the backend keeps executing and
        // keeps holding its share of the operator's database until it finishes.
        // `statement_timeout` is what actually cancels it. This matters most for
        // the two cheapest ways to build an expensive query here — a filter that
        // forces a sequential scan, and a large OFFSET, whose cost is O(offset)
        // and therefore unaffected by the LIMIT clamp above.
        //
        // Set on the session rather than in a transaction because this client is
        // reused: every later query on this connection inherits the ceiling,
        // which is the behaviour we want for a browser. The value is a u64 we
        // computed, never caller text, so it cannot carry SQL.
        let timeout_ms = options.timeout_ms.unwrap_or(30_000);
        if let Err(e) = client
            .batch_execute(&format!("SET statement_timeout = {timeout_ms}"))
            .await
        {
            // Not fatal on its own — the outer deadline still bounds the
            // request — but it means this query is unbounded server-side, which
            // an operator debugging a slow database needs to be able to see.
            warn!("Failed to set statement_timeout for data browser query: {e}");
        }

        let stream = client
            .query_raw(&sql, std::iter::empty::<&dyn ToSql>())
            .await
            .map_err(|_error| {
                error!(entity = entity_name, limit, "PostgreSQL query failed");
                DataError::BackendQueryFailed {
                    backend: "PostgreSQL",
                    entity: entity_name.to_string(),
                }
            })?;
        pin_mut!(stream);
        let mut bounded = BoundedRows::new(options.budget);
        while let Some(row) = stream.try_next().await.map_err(|_error| {
            error!(entity = entity_name, limit, "PostgreSQL row stream failed");
            DataError::BackendQueryFailed {
                backend: "PostgreSQL",
                entity: entity_name.to_string(),
            }
        })? {
            let observed = row.try_get::<_, i64>("__temps_size").map_err(|_error| {
                error!(
                    entity = entity_name,
                    limit, "PostgreSQL bounded row size decode failed"
                );
                DataError::BackendQueryFailed {
                    backend: "PostgreSQL",
                    entity: entity_name.to_string(),
                }
            })?;
            let observed_cell = row
                .try_get::<_, i64>("__temps_max_cell")
                .map_err(|_error| DataError::BackendQueryFailed {
                    backend: "PostgreSQL",
                    entity: entity_name.to_string(),
                })?;
            let payload = row
                .try_get::<_, Option<serde_json::Value>>("__temps_row")
                .map_err(|_error| {
                    error!(
                        entity = entity_name,
                        limit, "PostgreSQL bounded row decode failed"
                    );
                    DataError::BackendQueryFailed {
                        backend: "PostgreSQL",
                        entity: entity_name.to_string(),
                    }
                })?
                .ok_or_else(|| {
                    let observed_cell = usize::try_from(observed_cell).unwrap_or(usize::MAX);
                    let (limit_kind, limit, observed) =
                        if observed_cell > options.budget.max_cell_bytes {
                            (
                                "wire_cell_bytes",
                                options.budget.max_cell_bytes,
                                observed_cell,
                            )
                        } else {
                            (
                                "wire_row_bytes",
                                options.budget.max_bytes,
                                usize::try_from(observed).unwrap_or(usize::MAX),
                            )
                        };
                    DataError::ResultLimitExceeded {
                        entity: entity_name.to_string(),
                        limit_kind,
                        limit,
                        observed,
                    }
                })?;
            let data_row = match payload {
                serde_json::Value::Object(values) => values.into_iter().collect(),
                _ => {
                    return Err(DataError::SerializationError(format!(
                        "PostgreSQL bounded row for entity '{}' was not an object",
                        entity_name
                    )))
                }
            };
            if !bounded.push(entity_name, data_row)? {
                break;
            }
        }
        let (data_rows, truncated) = bounded.into_parts();

        let execution_ms = start.elapsed().as_millis() as u64;
        let row_count = data_rows.len();

        debug!("Query returned {} rows in {}ms", row_count, execution_ms);

        Ok(QueryResult {
            schema,
            rows: data_rows,
            stats: QueryStats {
                row_count,
                total_rows: None,
                execution_ms,
                has_more: row_count >= limit,
                next_cursor: None,
                truncated,
            },
        })
    }

    async fn count(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        filters: Option<serde_json::Value>,
    ) -> Result<u64> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery(
                "Invalid path depth for count".to_string(),
            ));
        }

        let schema_name = &container_path.segments[1];

        Self::validate_filters(filters.as_ref())?;
        let _schema = self.get_schema(container_path, entity_name).await?;

        let sql = format!(
            "SELECT COUNT(*) FROM \"{}\".\"{}\"",
            escape_ident(schema_name),
            escape_ident(entity_name)
        );

        let client = &self.client;
        let _timeout_guard = self.query_timeout_lock.lock().await;

        // SECURITY: bound this the same way `query` is bounded.
        //
        // The timeout work originally covered the row-reading path only, which
        // left the more expensive operation unbounded: `COUNT(*)` is a full
        // scan on PostgreSQL, it takes a caller-supplied WHERE clause, and it is
        // reached from `get_entity_info` — an endpoint the AI agent can call.
        // "Check the row count of every table" is then a denial of service
        // against the customer's production database with nothing to stop it.
        if let Err(e) = client
            .batch_execute(&format!(
                "SET statement_timeout = {DEFAULT_COUNT_TIMEOUT_MS}"
            ))
            .await
        {
            warn!("Failed to set statement_timeout for data browser count: {e}");
        }

        let row = client
            .query_one(&sql, &[])
            .await
            .map_err(|e| DataError::QueryFailed(format!("Count query failed: {}", e)))?;

        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn entity_exists(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<bool> {
        if container_path.depth() != 2 {
            return Err(DataError::InvalidQuery("Invalid path depth".to_string()));
        }

        let schema_name = &container_path.segments[1];
        let client = &self.client;

        let query = r#"
            SELECT COUNT(*)
            FROM information_schema.tables
            WHERE table_schema = $1 AND table_name = $2
        "#;

        let row = client
            .query_one(query, &[schema_name, &entity_name])
            .await
            .map_err(|e| DataError::QueryFailed(format!("Entity existence check failed: {}", e)))?;

        let count: i64 = row.get(0);
        Ok(count > 0)
    }
}

#[cfg(test)]
mod wire_budget_tests {
    use super::{with_wire_row_budget, PgQueryColumn};
    use temps_query::QueryBudget;

    #[test]
    fn generated_query_guards_encoded_row_before_wire_transfer() {
        let columns = vec![
            PgQueryColumn {
                name: "bio".to_string(),
                data_type: "text".to_string(),
                udt_kind: Some("b".to_string()),
            },
            PgQueryColumn {
                name: "payload".to_string(),
                data_type: "jsonb".to_string(),
                udt_kind: Some("b".to_string()),
            },
        ];
        let budget = QueryBudget {
            max_bytes: 262_144,
            max_cell_bytes: 65_536,
            ..QueryBudget::default()
        };
        let sql = with_wire_row_budget("SELECT * FROM public.users", &columns, budget)
            .expect("supported schema should build a bounded query");
        assert!(sql.contains("OCTET_LENGTH(__temps_source.\"bio\")::bigint * 6"));
        assert!(sql.contains("PG_COLUMN_COMPRESSION(__temps_source.\"payload\")"));
        assert!(sql.contains("__temps_max_cell <= 65536"));
        assert!(sql.contains("__temps_row_size <= 262144"));
        assert!(sql.contains("THEN TO_JSONB(__temps_source) ELSE NULL"));
        assert!(!sql.contains("TO_JSONB(__temps_source)::text"));
        assert_eq!(sql.matches("TO_JSONB(__temps_source)").count(), 1);
        assert!(sql.contains("SELECT * FROM public.users"));
    }

    #[test]
    fn unknown_types_null_only_the_unsupported_field() {
        let sql = with_wire_row_budget(
            "SELECT * FROM public.widgets",
            &[
                PgQueryColumn {
                    name: "id".to_string(),
                    data_type: "bigint".to_string(),
                    udt_kind: Some("b".to_string()),
                },
                PgQueryColumn {
                    name: "shape".to_string(),
                    data_type: "USER-DEFINED".to_string(),
                    udt_kind: Some("b".to_string()),
                },
            ],
            QueryBudget::default(),
        )
        .expect("an unsupported field must not make the whole relation unqueryable");
        assert!(sql.contains("__temps_raw.\"id\" AS \"id\""));
        assert!(sql.contains("NULL::text AS \"shape\""));
        assert!(!sql.contains("__temps_raw.\"shape\""));
    }

    #[test]
    fn common_builtin_and_enum_types_remain_browsable() {
        for (data_type, udt_kind) in [
            ("inet", Some("b")),
            ("interval", Some("b")),
            ("time with time zone", Some("b")),
            ("bit varying", Some("b")),
            ("USER-DEFINED", Some("e")),
        ] {
            let sql = with_wire_row_budget(
                "SELECT * FROM public.compatibility_types",
                &[PgQueryColumn {
                    name: "value".to_string(),
                    data_type: data_type.to_string(),
                    udt_kind: udt_kind.map(str::to_string),
                }],
                QueryBudget::default(),
            )
            .expect("common bounded PostgreSQL types should remain browsable");
            assert!(sql.contains("TO_JSONB(__temps_source)"));
        }
    }

    #[test]
    fn arrays_use_compression_aware_admission_before_json_generation() {
        for name in [
            "token_tok_live_secret",
            "large_numeric_array",
            "custom_type_array",
        ] {
            let sql = with_wire_row_budget(
                "SELECT * FROM public.array_payloads",
                &[PgQueryColumn {
                    name: name.to_string(),
                    data_type: "ARRAY".to_string(),
                    udt_kind: Some("b".to_string()),
                }],
                QueryBudget::default(),
            )
            .expect("array columns should remain browsable through bounded admission");
            assert!(sql.contains("PG_COLUMN_COMPRESSION"));
            assert!(sql.contains("PG_COLUMN_SIZE"));
            assert_eq!(sql.matches("TO_JSONB(__temps_source)").count(), 1);
        }
    }
}

impl temps_query::QuerySchemaProvider for PostgresSource {
    fn get_filter_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "title": "PostgreSQL Query Filters",
            "description": "PostgreSQL data browsing currently disables raw SQL filters for security. Use sorting and pagination to inspect rows.",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn get_sort_schema(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<serde_json::Value> {
        // Get entity schema to know available fields
        let schema_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { self.get_schema(container_path, entity_name).await })
        });

        let schema = schema_result?;

        // Build enum of available fields
        let field_names: Vec<String> = schema.fields.iter().map(|f| f.name.clone()).collect();

        Ok(serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "title": "Sort Options",
            "description": "Specify how to sort query results",
            "properties": {
                "sort_by": {
                    "type": "string",
                    "title": "Sort By",
                    "description": "Field to sort by",
                    "enum": field_names,
                    "x-ui-widget": "select"
                },
                "sort_order": {
                    "type": "string",
                    "title": "Sort Order",
                    "description": "Sort direction",
                    "enum": ["asc", "desc"],
                    "default": "asc",
                    "x-ui-widget": "select"
                }
            }
        }))
    }

    fn get_filter_ui_schema(&self) -> Option<serde_json::Value> {
        // No longer needed - UI hints are embedded in filter_schema
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::{
        core::{ContainerPort, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };

    fn container_runtime_unavailable(error: &str) -> bool {
        let message = error.to_ascii_lowercase();
        [
            "hyper legacy client: client error (connect)",
            "failed to connect to docker",
            "error connecting to docker",
            "docker daemon is unavailable",
            "docker client is unavailable",
            "could not find docker environment",
            "docker socket",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }

    #[test]
    fn ip_is_private_allows_local_and_mesh_addresses() {
        // These are where a Temps-managed database actually lives: a container
        // on the same host, or a peer on the private mesh. Cleartext there
        // never leaves infrastructure the operator controls, so the fallback
        // must keep working — refusing these would break every existing
        // self-hosted deployment whose Postgres has no TLS.
        for ip in [
            "127.0.0.1",
            "::1",
            "10.0.0.5",
            "172.16.4.9",
            "192.168.1.20",
            "fd00::1",
            "169.254.1.1",
            // Carrier-grade NAT — Tailscale and similar meshes live here.
            "100.64.0.1",
            // IPv4-mapped private address must classify by the embedded v4.
            "::ffff:10.0.0.5",
        ] {
            assert!(
                ip_is_private(ip.parse().expect("test address should parse")),
                "should be treated as private: {ip}"
            );
        }
    }

    #[test]
    fn ip_is_private_rejects_public_addresses() {
        // Falling back to an unverified certificate or to cleartext for these
        // would put the service's admin password on the open internet.
        for ip in [
            "203.0.113.10",
            "8.8.8.8",
            "2606:4700:4700::1111",
            // The integer form of 8.8.8.8 resolves publicly; classifying the
            // *string* used to call it private because it has no dot.
            "::ffff:8.8.8.8",
        ] {
            assert!(
                !ip_is_private(ip.parse().expect("test address should parse")),
                "should NOT be treated as private: {ip}"
            );
        }
    }

    #[tokio::test]
    async fn resolved_host_classifies_the_addresses_that_will_be_dialed() {
        // Loopback and unix sockets stay private...
        for host in ["127.0.0.1", "localhost", "/var/run/postgresql"] {
            assert!(
                resolve_host_once(host, 5432)
                    .await
                    .expect("private test host should resolve")
                    .permits_unverified_transport(),
                "should permit private host: {host}"
            );
        }

        // ...and the numeric-IPv4 forms that defeated string classification are
        // now resolved and rejected. `134744072` == 0x08080808 == 8.8.8.8:
        // `IpAddr::parse` rejects it, so the old "no dot means container name"
        // rule called it private, while getaddrinfo resolves it publicly.
        for host in ["134744072", "0x08080808"] {
            assert!(
                !resolve_host_once(host, 5432)
                    .await
                    .expect("numeric public host should resolve")
                    .permits_unverified_transport(),
                "should reject public host: {host}"
            );
        }

        // Unresolvable is a refusal, not a pass: this decides whether a
        // password may cross the network in cleartext.
        assert!(resolve_host_once("no-such-host.invalid", 5432)
            .await
            .is_err());
        assert!(resolve_host_once("", 5432).await.is_err());
    }

    #[test]
    fn connection_config_pins_every_preapproved_address() {
        let addresses = vec![
            "10.0.0.8".parse().expect("test IPv4 address should parse"),
            "fd00::8".parse().expect("test IPv6 address should parse"),
        ];
        let resolved = ResolvedHost::Tcp {
            hostname: "database.internal".to_string(),
            addresses: addresses.clone(),
        };

        let config = connect_config_for(&resolved, 6432, "admin", "secret", "postgres");

        assert_eq!(config.get_hostaddrs(), addresses.as_slice());
        assert_eq!(config.get_hosts().len(), addresses.len());
        assert!(config.get_hosts().iter().all(|host| {
            matches!(host, tokio_postgres::config::Host::Tcp(name) if name == "database.internal")
        }));
        assert_eq!(config.get_ports(), &[6432]);
    }

    #[test]
    fn connection_config_treats_credentials_as_values_and_requires_tls() {
        let resolved = ResolvedHost::Tcp {
            hostname: "database.internal".to_string(),
            addresses: vec!["10.0.0.8".parse().expect("test IPv4 address should parse")],
        };
        let username = "admin host=attacker.example sslmode=disable";
        let password = "secret dbname=postgres host=attacker.example";
        let database = "postgres sslmode=disable";

        let config = connect_config_for(&resolved, 5432, username, password, database);

        assert_eq!(config.get_user(), Some(username));
        assert_eq!(config.get_password(), Some(password.as_bytes()));
        assert_eq!(config.get_dbname(), Some(database));
        assert_eq!(
            config.get_ssl_mode(),
            tokio_postgres::config::SslMode::Require
        );
        assert_eq!(config.get_hosts().len(), 1);
        assert_eq!(config.get_hostaddrs().len(), 1);
    }

    #[tokio::test]
    async fn private_ladder_rejects_a_public_address_before_connecting() {
        let error = connect_with_private_tls_ladder(
            "203.0.113.10",
            5432,
            "cluster-admin",
            "secret",
            "postgres",
        )
        .await
        .expect_err("a managed-cluster credential must never be sent to a public address");

        assert!(
            error
                .to_string()
                .contains("refusing to send cluster credentials even over verified TLS"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validate_sql_rejects_regex_operators() {
        // The pattern lives entirely inside a string literal, so the stripper
        // removes it and every keyword check sees `1=1 and '' ~ ''`. PostgreSQL's
        // regex engine backtracks, so this is catastrophic backtracking — the
        // same CPU burn the function-call guard exists to stop, through a door
        // the guard cannot see.
        assert!(PostgresSource::validate_sql(
            "1=1 AND 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ~ '^(a+)+$'"
        )
        .is_err());
        assert!(PostgresSource::validate_sql("name !~ 'x'").is_err());
        assert!(PostgresSource::validate_sql("name ~* 'x'").is_err());
    }

    #[test]
    fn validate_sql_rejects_parenless_information_functions() {
        // The function-call guard keys on an identifier before `(`, so
        // niladic-callable functions walk straight past it and give blind
        // boolean extraction through an ordinary filter.
        for clause in [
            "1=1 AND current_user = 'postgres'",
            "1=1 AND current_schema = 'public'",
            "1=1 AND session_user like 'a%'",
            "'pg_shadow'::regclass::text = 'x'",
        ] {
            assert!(
                PostgresSource::validate_sql(clause).is_err(),
                "should have been rejected: {clause}"
            );
        }
    }

    #[test]
    fn validate_sql_still_accepts_columns_containing_keywords() {
        // The denylist used raw substring matching, so any column whose name
        // merely contained a keyword was unfilterable. A validator that blocks
        // legitimate queries is a validator someone eventually switches off.
        for clause in [
            "payload = 'x'",
            "updated_at > '2025-01-01'",
            "deleted_at IS NULL",
            "copy_count > 2",
            "created_by = 'me' AND status = 'active'",
        ] {
            assert!(
                PostgresSource::validate_sql(clause).is_ok(),
                "should have been accepted: {clause}"
            );
        }
    }

    #[test]
    fn contains_sql_token_matches_only_whole_tokens() {
        assert!(contains_sql_token("a union b", "union"));
        assert!(contains_sql_token("union b", "union"));
        assert!(contains_sql_token("a union", "union"));
        // Not a token: embedded in a longer identifier.
        assert!(!contains_sql_token("reunion = 1", "union"));
        assert!(!contains_sql_token("union_id = 1", "union"));
        assert!(!contains_sql_token("payload = 1", "load"));
    }

    #[test]
    fn contains_sql_token_treats_numeric_literals_as_boundaries() {
        // A keyword abutting the tail of a numeric literal is a SEPARATE token
        // to the server's lexer: MySQL and pre-15 PostgreSQL both read
        // `1e0union select ...` as `1e0` followed by `UNION`. Digits are
        // identifier continuation characters but cannot start an identifier, so
        // checking only the single preceding character called this a
        // non-boundary and let the injection through — a regression against the
        // old `contains("union ")` form, which caught it.
        assert!(contains_sql_token("1e0union select 1,2,3", "union"));
        assert!(contains_sql_token("1.0union select 1", "union"));
        assert!(contains_sql_token("0x1union select 1", "union"));
        assert!(contains_sql_token("id=1union select 1", "union"));

        // Still not a match when the run really is an identifier: it starts
        // with a letter or underscore, so the keyword is part of the name.
        assert!(!contains_sql_token("a1union = 1", "union"));
        assert!(!contains_sql_token("_1union = 1", "union"));
        assert!(!contains_sql_token("col2payload = 1", "load"));
    }

    #[test]
    fn validate_sql_rejects_numeric_prefixed_union() {
        // End-to-end form of the above, through the real validator.
        assert!(PostgresSource::validate_sql("1e0union all select 1,2,3").is_err());
        assert!(PostgresSource::validate_sql("id=1.0union select 1").is_err());
    }

    #[test]
    fn test_pg_type_mapping() {
        assert_eq!(PostgresSource::map_pg_type("integer"), FieldType::Int32);
        assert_eq!(PostgresSource::map_pg_type("bigint"), FieldType::Int64);
        assert_eq!(PostgresSource::map_pg_type("text"), FieldType::String);
        assert_eq!(
            PostgresSource::map_pg_type("timestamp"),
            FieldType::Timestamp
        );
        assert_eq!(PostgresSource::map_pg_type("uuid"), FieldType::Uuid);
        assert_eq!(PostgresSource::map_pg_type("jsonb"), FieldType::Json);
    }

    // ── Filter validation tests ──────────────────────────────────────

    #[test]
    fn test_postgres_filters_reject_raw_where_fragments() {
        let filters = serde_json::json!({"where": "status = 'active'"});
        let result = PostgresSource::validate_filters(Some(&filters));

        assert!(
            result.is_err(),
            "raw SQL WHERE fragments must not be accepted from API callers"
        );
    }

    #[test]
    fn test_postgres_filters_allow_absent_or_empty_filters() {
        assert!(PostgresSource::validate_filters(None).is_ok());

        let filters = serde_json::json!({});
        assert!(PostgresSource::validate_filters(Some(&filters)).is_ok());
    }

    #[test]
    fn test_postgres_limit_is_capped() {
        assert_eq!(PostgresSource::clamp_limit(None), MAX_QUERY_LIMIT);
        assert_eq!(PostgresSource::clamp_limit(Some(25)), 25);
        assert_eq!(PostgresSource::clamp_limit(Some(10_000)), MAX_QUERY_LIMIT);
    }

    // ── Sort field validation tests ──────────────────────────────────

    #[test]
    fn test_sort_field_valid_simple() {
        assert!(PostgresSource::validate_sort_field("created_at").is_ok());
        assert!(PostgresSource::validate_sort_field("id").is_ok());
        assert!(PostgresSource::validate_sort_field("user_name").is_ok());
    }

    #[test]
    fn test_sort_field_valid_quoted() {
        assert!(PostgresSource::validate_sort_field("\"created_at\"").is_ok());
    }

    #[test]
    fn test_sort_field_valid_schema_qualified() {
        assert!(PostgresSource::validate_sort_field("schema.column").is_ok());
    }

    #[test]
    fn test_sort_field_injection_blocked() {
        assert!(PostgresSource::validate_sort_field("id; DROP TABLE users--").is_err());
        assert!(PostgresSource::validate_sort_field("").is_err());
        assert!(PostgresSource::validate_sort_field("id OR 1=1").is_err());
    }

    #[test]
    fn test_sort_field_quoted_injection_blocked() {
        // Double quotes inside a quoted identifier should be rejected
        assert!(PostgresSource::validate_sort_field("\"id\"; DROP TABLE users--\"").is_err());
    }
}
