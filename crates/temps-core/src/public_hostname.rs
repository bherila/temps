use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

const DEFAULT_BASE_DOMAIN: &str = "localho.st";
const DNS_LABEL_MAX_LEN: usize = 63;
const SHORT_HASH_LEN: usize = 8;

/// Public hostname generation mode for Temps-managed preview routes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicHostnameStrategy {
    /// Preserve Temps' existing generated hostname layout.
    Standard,
    /// Force generated hostnames to one label below `preview_domain`.
    Flat,
}

impl Default for PublicHostnameStrategy {
    fn default() -> Self {
        Self::Standard
    }
}

/// Operator-configurable templates for generated public hostnames.
///
/// Templates may use `{base_domain}`, `{environment}`, `{service}`,
/// `{deployment}`, `{project}`, `{app}`, `{branch}`, `{preview_slug}`, and
/// `{short_hash}`. When `strategy = flat`, all generated labels before
/// `{base_domain}` are collapsed into a single DNS label.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(default)]
pub struct PublicHostnameSettings {
    pub strategy: PublicHostnameStrategy,
    pub environment_template: Option<String>,
    pub service_template: Option<String>,
    pub deployment_template: Option<String>,
}

impl Default for PublicHostnameSettings {
    fn default() -> Self {
        Self {
            strategy: PublicHostnameStrategy::Standard,
            environment_template: None,
            service_template: None,
            deployment_template: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PublicHostnameContext<'a> {
    pub app: Option<&'a str>,
    pub project: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub service: Option<&'a str>,
    pub deployment: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub preview_slug: Option<&'a str>,
}

impl PublicHostnameSettings {
    /// Normalize the configured preview domain into the base domain used for
    /// generated public hosts. Accepts both `example.com` and `*.example.com`.
    pub fn base_domain(&self, preview_domain: &str) -> String {
        normalize_base_domain(preview_domain)
    }

    pub fn environment_hostname(&self, preview_domain: &str, environment: &str) -> String {
        let template = self
            .environment_template
            .as_deref()
            .unwrap_or("{environment}.{base_domain}");
        self.render_hostname(
            preview_domain,
            template,
            PublicHostnameContext {
                environment: Some(environment),
                preview_slug: Some(environment),
                ..Default::default()
            },
        )
    }

    pub fn service_hostname(
        &self,
        preview_domain: &str,
        environment: &str,
        service: &str,
    ) -> String {
        let template = self
            .service_template
            .as_deref()
            .unwrap_or(match self.strategy {
                PublicHostnameStrategy::Standard => "{service}-{environment}.{base_domain}",
                PublicHostnameStrategy::Flat => "{environment}-{service}.{base_domain}",
            });
        self.render_hostname(
            preview_domain,
            template,
            PublicHostnameContext {
                environment: Some(environment),
                service: Some(service),
                preview_slug: Some(environment),
                ..Default::default()
            },
        )
    }

    pub fn deployment_hostname(&self, preview_domain: &str, deployment: &str) -> String {
        let template = self
            .deployment_template
            .as_deref()
            .unwrap_or("{deployment}.{base_domain}");
        self.render_hostname(
            preview_domain,
            template,
            PublicHostnameContext {
                deployment: Some(deployment),
                ..Default::default()
            },
        )
    }

    pub fn project_deployment_hostname(
        &self,
        preview_domain: &str,
        project: &str,
        environment: &str,
        deployment: &str,
    ) -> String {
        self.render_hostname(
            preview_domain,
            "{project}-{environment}-{deployment}.{base_domain}",
            PublicHostnameContext {
                app: Some(project),
                project: Some(project),
                environment: Some(environment),
                deployment: Some(deployment),
                preview_slug: Some(environment),
                ..Default::default()
            },
        )
    }

    pub fn render_hostname(
        &self,
        preview_domain: &str,
        template: &str,
        context: PublicHostnameContext<'_>,
    ) -> String {
        let base_domain = self.base_domain(preview_domain);
        let rendered = render_template(template, &base_domain, &context);
        normalize_hostname(
            &rendered,
            &base_domain,
            matches!(self.strategy, PublicHostnameStrategy::Flat),
        )
    }
}

fn normalize_base_domain(preview_domain: &str) -> String {
    let trimmed = preview_domain
        .trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if trimmed.is_empty() {
        DEFAULT_BASE_DOMAIN.to_string()
    } else {
        trimmed
    }
}

fn render_template(
    template: &str,
    base_domain: &str,
    context: &PublicHostnameContext<'_>,
) -> String {
    let seed = [
        base_domain,
        context.app.unwrap_or(""),
        context.project.unwrap_or(""),
        context.environment.unwrap_or(""),
        context.service.unwrap_or(""),
        context.deployment.unwrap_or(""),
        context.branch.unwrap_or(""),
        context.preview_slug.unwrap_or(""),
    ]
    .join("|");
    let short_hash = short_hash(&seed);

    let replacements = [
        ("{base_domain}", base_domain),
        ("{app}", context.app.or(context.project).unwrap_or("")),
        ("{project}", context.project.or(context.app).unwrap_or("")),
        ("{environment}", context.environment.unwrap_or("")),
        ("{env}", context.environment.unwrap_or("")),
        ("{service}", context.service.unwrap_or("")),
        ("{deployment}", context.deployment.unwrap_or("")),
        ("{branch}", context.branch.unwrap_or("")),
        (
            "{preview_slug}",
            context.preview_slug.or(context.environment).unwrap_or(""),
        ),
        (
            "{preview}",
            context.preview_slug.or(context.environment).unwrap_or(""),
        ),
        ("{short_hash}", short_hash.as_str()),
    ];

    replacements
        .iter()
        .fold(template.to_string(), |acc, (needle, value)| {
            acc.replace(needle, value)
        })
}

fn normalize_hostname(raw: &str, base_domain: &str, force_single_label: bool) -> String {
    let host = raw
        .trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let base_domain = normalize_base_domain(base_domain);
    let suffix = format!(".{base_domain}");

    let relative = if host == base_domain {
        String::new()
    } else if host.ends_with(&suffix) {
        host[..host.len() - suffix.len()].to_string()
    } else {
        host
    };

    let raw_labels: Vec<&str> = relative
        .split('.')
        .filter(|label| !label.is_empty())
        .collect();
    if raw_labels.is_empty() {
        return base_domain;
    }

    let labels = if force_single_label {
        vec![dns_label(&raw_labels.join("-"), &relative)]
    } else {
        raw_labels
            .iter()
            .map(|label| dns_label(label, label))
            .collect()
    };

    format!("{}.{}", labels.join("."), base_domain)
}

fn dns_label(label: &str, hash_seed: &str) -> String {
    let sanitized = sanitize_label(label);
    if sanitized.len() <= DNS_LABEL_MAX_LEN {
        return sanitized;
    }

    let suffix = format!("-{}", short_hash(hash_seed));
    let max_prefix_len = DNS_LABEL_MAX_LEN.saturating_sub(suffix.len());
    let prefix = sanitized
        .chars()
        .take(max_prefix_len)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string();

    if prefix.is_empty() {
        short_hash(hash_seed)
    } else {
        format!("{prefix}{suffix}")
    }
}

fn sanitize_label(label: &str) -> String {
    let mut output = String::new();
    let mut previous_hyphen = false;

    for ch in label.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            output.push(lower);
            previous_hyphen = false;
        } else if lower == '-' && !previous_hyphen {
            output.push('-');
            previous_hyphen = true;
        } else if !previous_hyphen {
            output.push('-');
            previous_hyphen = true;
        }
    }

    let trimmed = output.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "x".to_string()
    } else {
        trimmed
    }
}

fn short_hash(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    format!("{digest:x}").chars().take(SHORT_HASH_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_domain_strips_wildcard_prefix() {
        let settings = PublicHostnameSettings::default();
        assert_eq!(settings.base_domain("*.Example.COM."), "example.com");
    }

    #[test]
    fn standard_service_hostname_preserves_existing_order() {
        let settings = PublicHostnameSettings::default();
        assert_eq!(
            settings.service_hostname("*.example.com", "staging", "files"),
            "files-staging.example.com"
        );
    }

    #[test]
    fn flat_service_hostname_uses_environment_first() {
        let settings = PublicHostnameSettings {
            strategy: PublicHostnameStrategy::Flat,
            ..Default::default()
        };
        assert_eq!(
            settings.service_hostname("example.com", "staging", "files"),
            "staging-files.example.com"
        );
    }

    #[test]
    fn flat_strategy_collapses_nested_template_to_one_label() {
        let settings = PublicHostnameSettings {
            strategy: PublicHostnameStrategy::Flat,
            service_template: Some("{service}.{environment}.{base_domain}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            settings.service_hostname("example.com", "preview-123", "api"),
            "api-preview-123.example.com"
        );
    }

    #[test]
    fn long_generated_label_gets_stable_hash_suffix() {
        let settings = PublicHostnameSettings {
            strategy: PublicHostnameStrategy::Flat,
            ..Default::default()
        };
        let host = settings.service_hostname(
            "example.com",
            "preview-this-branch-name-is-deliberately-long-and-keeps-going",
            "extremely-long-service-name-that-would-overflow-the-dns-label",
        );
        let label = host.split('.').next().unwrap();
        assert!(label.len() <= DNS_LABEL_MAX_LEN);
        assert_eq!(
            host,
            settings.service_hostname(
                "example.com",
                "preview-this-branch-name-is-deliberately-long-and-keeps-going",
                "extremely-long-service-name-that-would-overflow-the-dns-label",
            )
        );
    }
}
