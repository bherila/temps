//! Docker Compose deployment executor.
//!
//! Manages multi-container deployments using `docker compose` CLI commands.
//! After `compose up`, discovers running containers, applies Temps labels,
//! and returns per-service results that get inserted into `deployment_containers`.

use bollard::Docker;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum ComposeError {
    #[error("Compose command failed for project '{project}': {reason}")]
    CommandFailed { project: String, reason: String },

    #[error("Failed to write compose files to '{path}': {reason}")]
    FileWriteFailed { path: String, reason: String },

    #[error("Failed to discover containers for project '{project}': {reason}")]
    DiscoveryFailed { project: String, reason: String },

    #[error("Docker API error: {0}")]
    Docker(String),

    #[error("Compose security policy rejected {field} for service '{service}': {reason}")]
    SecurityPolicyViolation {
        service: String,
        field: String,
        reason: String,
    },

    #[error("Failed to parse compose YAML for '{compose_source}': {reason}")]
    InvalidComposeYaml {
        compose_source: String,
        reason: String,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Request to deploy a Docker Compose stack.
#[derive(Debug, Clone)]
pub struct ComposeDeployRequest {
    /// Compose project name (e.g., "temps-{project_id}-{env_id}")
    pub project_name: String,
    /// Compose file content (the YAML)
    pub compose_content: String,
    /// Optional .env file content
    pub env_content: Option<String>,
    /// Working directory where compose files are written
    pub work_dir: PathBuf,
    /// Path to compose file relative to work_dir (default: "docker-compose.yml")
    pub compose_path: Option<String>,
    /// Environment variables to inject (merged with .env)
    pub environment_vars: HashMap<String, String>,
    /// Temps labels to apply to all containers
    pub labels: HashMap<String, String>,
    /// Source repo directory (needed for compose files with build: directives)
    pub repo_dir: Option<PathBuf>,
    /// User-provided docker-compose.temps-override.yml content
    pub compose_override: Option<String>,
}

/// Result for a single compose service after deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeServiceResult {
    pub container_id: String,
    pub container_name: String,
    pub service_name: String,
    pub image_name: String,
    /// Ports published to the host (may be empty for internal services)
    pub ports: Vec<ComposePortBinding>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposePortBinding {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: String,
}

/// Docker Compose deployment executor.
#[derive(Debug)]
pub struct ComposeExecutor {
    docker: Arc<Docker>,
    /// Base directory for compose work dirs
    data_dir: PathBuf,
}

impl ComposeExecutor {
    pub fn new(docker: Arc<Docker>, data_dir: PathBuf) -> Self {
        Self { docker, data_dir }
    }

    /// Get the work directory for a compose project.
    fn project_dir(&self, project_name: &str) -> PathBuf {
        self.data_dir.join("compose").join(project_name)
    }

    /// Deploy a compose stack: write files, pull images, start containers,
    /// discover and label them. Returns one result per service.
    pub async fn deploy(
        &self,
        request: ComposeDeployRequest,
    ) -> Result<Vec<ComposeServiceResult>, ComposeError> {
        let project_dir = self.project_dir(&request.project_name);
        let project_name = request.project_name.clone();
        self.validate_compose_security_policy("compose file", &request.compose_content)?;
        if let Some(ref compose_override) = request.compose_override {
            self.validate_compose_security_policy("compose override", compose_override)?;
        }
        let has_build = self.has_build_directives(&request.compose_content);

        // Always use the repo checkout directory when available.
        // Compose files often reference local paths (bind mounts, configs,
        // build contexts) that only exist in the repo, not in the temps data dir.
        let effective_dir = request
            .repo_dir
            .clone()
            .unwrap_or_else(|| project_dir.clone());

        // 1. Write compose files + env overrides to disk
        self.write_compose_files(&effective_dir, &request).await?;

        let compose_file = request
            .compose_path
            .as_deref()
            .unwrap_or("docker-compose.yml");

        // 2. Build images if compose file has build: directives
        if has_build {
            self.compose_build(
                &effective_dir,
                &project_name,
                compose_file,
                &request.environment_vars,
            )
            .await?;
        }

        // 3. Remove any containers with hardcoded names that would conflict
        self.remove_conflicting_containers(&request.compose_content)
            .await;

        // 4. Run docker compose up (pulls pre-built images, starts built + pulled)
        self.compose_up(
            &effective_dir,
            &project_name,
            compose_file,
            &request.environment_vars,
        )
        .await?;

        // 4. Discover running containers
        let containers = self
            .discover_containers(&effective_dir, &project_name, compose_file)
            .await?;

        // 4. Apply Temps labels to each container
        for container in &containers {
            if let Err(e) = self
                .apply_labels(
                    &container.container_id,
                    &request.labels,
                    &container.service_name,
                )
                .await
            {
                warn!(
                    container_id = %container.container_id,
                    service = %container.service_name,
                    error = %e,
                    "Failed to apply Temps labels to container"
                );
            }
        }

        info!(
            project = %project_name,
            services = containers.len(),
            "Compose stack deployed"
        );

        Ok(containers)
    }

    /// Tear down containers before a redeploy. Preserves volumes (database data,
    /// uploads, etc.) so they survive between deployments.
    pub async fn teardown_for_redeploy(&self, project_name: &str) -> Result<(), ComposeError> {
        let project_dir = self.project_dir(project_name);

        if !project_dir.exists() {
            debug!(project = %project_name, "Project directory does not exist, nothing to tear down");
            return Ok(());
        }

        let compose_file = self.find_compose_file(&project_dir);

        // down WITHOUT --volumes: removes containers and networks, keeps volumes
        let output = tokio::process::Command::new("docker")
            .args(["compose", "-p", project_name])
            .args(["-f", &compose_file])
            .args(["down", "--remove-orphans"])
            .current_dir(&project_dir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(project = %project_name, stderr = %stderr, "docker compose down failed (best-effort)");
        }

        info!(project = %project_name, "Compose stack torn down (volumes preserved)");
        Ok(())
    }

    /// Fully destroy a compose stack including all volumes and data.
    /// Used when deleting a project/environment permanently.
    pub async fn destroy(&self, project_name: &str) -> Result<(), ComposeError> {
        let project_dir = self.project_dir(project_name);

        if !project_dir.exists() {
            debug!(project = %project_name, "Project directory does not exist, nothing to destroy");
            return Ok(());
        }

        let compose_file = self.find_compose_file(&project_dir);

        // down WITH --volumes: removes everything including persistent data
        let output = tokio::process::Command::new("docker")
            .args(["compose", "-p", project_name])
            .args(["-f", &compose_file])
            .args(["down", "--remove-orphans", "--volumes"])
            .current_dir(&project_dir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(project = %project_name, stderr = %stderr, "docker compose down failed");
        }

        // Clean up work directory
        if let Err(e) = tokio::fs::remove_dir_all(&project_dir).await {
            warn!(project = %project_name, error = %e, "Failed to clean up project directory");
        }

        info!(project = %project_name, "Compose stack destroyed (volumes removed)");
        Ok(())
    }

    /// Stop a compose stack without removing volumes.
    pub async fn stop(&self, project_name: &str) -> Result<(), ComposeError> {
        let project_dir = self.project_dir(project_name);
        let compose_file = self.find_compose_file(&project_dir);

        let output = tokio::process::Command::new("docker")
            .args(["compose", "-p", project_name])
            .args(["-f", &compose_file])
            .arg("stop")
            .current_dir(&project_dir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComposeError::CommandFailed {
                project: project_name.to_string(),
                reason: format!("docker compose stop failed: {}", stderr),
            });
        }

        Ok(())
    }

    // --- Internal methods ---

    async fn write_compose_files(
        &self,
        project_dir: &Path,
        request: &ComposeDeployRequest,
    ) -> Result<(), ComposeError> {
        tokio::fs::create_dir_all(project_dir).await.map_err(|e| {
            ComposeError::FileWriteFailed {
                path: project_dir.display().to_string(),
                reason: e.to_string(),
            }
        })?;

        let compose_file = request
            .compose_path
            .as_deref()
            .unwrap_or("docker-compose.yml");
        let compose_path = project_dir.join(compose_file);

        // Ensure parent directories exist (for nested paths like "subdir/docker-compose.yml")
        if let Some(parent) = compose_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ComposeError::FileWriteFailed {
                    path: parent.display().to_string(),
                    reason: e.to_string(),
                })?;
        }

        // If the user override defines ports for specific services, strip those
        // ports from the base compose file. Docker Compose merges (appends) port
        // arrays from override files, so without stripping, the original ports
        // remain alongside the override ports, causing conflicts.
        let compose_to_write = if let Some(ref user_override) = request.compose_override {
            let services_with_port_overrides = self.services_with_ports_in_override(user_override);
            if services_with_port_overrides.is_empty() {
                request.compose_content.clone()
            } else {
                self.strip_ports_for_services(
                    &request.compose_content,
                    &services_with_port_overrides,
                )
            }
        } else {
            request.compose_content.clone()
        };

        tokio::fs::write(&compose_path, &compose_to_write)
            .await
            .map_err(|e| ComposeError::FileWriteFailed {
                path: compose_path.display().to_string(),
                reason: e.to_string(),
            })?;

        // Write .env file (repo's original .env content if any)
        if let Some(ref env_content) = request.env_content {
            if !env_content.trim().is_empty() {
                let env_path = project_dir.join(".env");
                tokio::fs::write(&env_path, env_content.trim())
                    .await
                    .map_err(|e| ComposeError::FileWriteFailed {
                        path: env_path.display().to_string(),
                        reason: e.to_string(),
                    })?;
            }
        }

        // Write Temps system env vars to .env.temps
        // These include SENTRY_DSN, TEMPS_API_URL, TEMPS_API_TOKEN, OTEL vars, etc.
        if !request.environment_vars.is_empty() {
            let temps_env: String = request
                .environment_vars
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("\n");
            let temps_env_path = project_dir.join(".env.temps");
            tokio::fs::write(&temps_env_path, &temps_env)
                .await
                .map_err(|e| ComposeError::FileWriteFailed {
                    path: temps_env_path.display().to_string(),
                    reason: e.to_string(),
                })?;

            // Write Temps env override (auto-generated, injects .env.temps into every service)
            let temps_override_path = project_dir.join("docker-compose.temps-env.yml");
            let override_content =
                self.generate_env_override(&request.compose_content, ".env.temps");
            tokio::fs::write(&temps_override_path, &override_content)
                .await
                .map_err(|e| ComposeError::FileWriteFailed {
                    path: temps_override_path.display().to_string(),
                    reason: e.to_string(),
                })?;
        }

        // Write Temps security override (injects sandbox hardening into every service).
        let security_content = self.generate_security_override(&request.compose_content);
        if !security_content.is_empty() {
            let security_override_path = project_dir.join("docker-compose.temps-security.yml");
            tokio::fs::write(&security_override_path, &security_content)
                .await
                .map_err(|e| ComposeError::FileWriteFailed {
                    path: security_override_path.display().to_string(),
                    reason: e.to_string(),
                })?;
        }

        // Write Temps labels override (injects sh.temps.* labels into every service for log collection)
        if !request.labels.is_empty() {
            let labels_override_path = project_dir.join("docker-compose.temps-labels.yml");
            let labels_content =
                self.generate_labels_override(&request.compose_content, &request.labels);
            if !labels_content.is_empty() {
                tokio::fs::write(&labels_override_path, &labels_content)
                    .await
                    .map_err(|e| ComposeError::FileWriteFailed {
                        path: labels_override_path.display().to_string(),
                        reason: e.to_string(),
                    })?;
            }
        }

        // Write user-provided override if present (ports, volumes, commands, etc.)
        if let Some(ref user_override) = request.compose_override {
            if !user_override.trim().is_empty() {
                let override_path = project_dir.join("docker-compose.temps-override.yml");
                tokio::fs::write(&override_path, user_override)
                    .await
                    .map_err(|e| ComposeError::FileWriteFailed {
                        path: override_path.display().to_string(),
                        reason: e.to_string(),
                    })?;
            }
        }

        debug!(
            path = %compose_path.display(),
            "Wrote compose files"
        );

        Ok(())
    }

    /// Preflight security validation. Run this BEFORE tearing down the existing
    /// stack so a policy rejection does not cause downtime on the running deployment.
    pub fn preflight_validate(
        &self,
        compose_content: &str,
        compose_override: Option<&str>,
    ) -> Result<(), ComposeError> {
        self.validate_compose_security_policy("compose file", compose_content)?;
        if let Some(override_content) = compose_override {
            self.validate_compose_security_policy("compose override", override_content)?;
        }
        Ok(())
    }

    fn validate_compose_security_policy(
        &self,
        source: &str,
        compose_content: &str,
    ) -> Result<(), ComposeError> {
        if compose_content.trim().is_empty() {
            return Ok(());
        }

        let mut root: YamlValue = serde_yaml::from_str(compose_content).map_err(|e| {
            ComposeError::InvalidComposeYaml {
                compose_source: source.to_string(),
                reason: e.to_string(),
            }
        })?;

        // Expand YAML merge keys (`<<`) so settings inherited from an anchor
        // (privileged, devices, volumes, ...) are visible during validation
        // instead of hiding behind the raw `<<` key.
        let _ = root.apply_merge();

        // Top-level named volumes whose driver options bind a forbidden host
        // path (e.g. `driver_opts: {type: none, o: bind, device: /}`). Service
        // mounts that reference these by name are rejected below.
        let forbidden_named_volumes = Self::forbidden_named_volumes(&root);

        // Block host files exposed through top-level configs/secrets `file:` paths.
        self.validate_top_level_files(&root, "configs")?;
        self.validate_top_level_files(&root, "secrets")?;

        let Some(services) = root.get("services").and_then(YamlValue::as_mapping) else {
            return Ok(());
        };

        for (service_key, service_value) in services {
            let service_name = service_key.as_str().unwrap_or("<unknown>");
            let Some(service) = service_value.as_mapping() else {
                continue;
            };

            self.reject_bool(
                service,
                service_name,
                "privileged",
                true,
                "privileged containers can bypass the host sandbox",
            )?;
            self.reject_bool(
                service,
                service_name,
                "use_api_socket",
                true,
                "use_api_socket exposes the docker engine API socket to the container",
            )?;
            self.reject_present(
                service,
                service_name,
                "cap_add",
                "adding Linux capabilities is not allowed for compose deployments",
            )?;
            self.reject_present(
                service,
                service_name,
                "devices",
                "host device passthrough is not allowed for compose deployments",
            )?;
            self.reject_present(
                service,
                service_name,
                "device_cgroup_rules",
                "device cgroup rules can grant host device access",
            )?;
            self.reject_present(
                service,
                service_name,
                "security_opt",
                "custom security options can disable no-new-privileges or confinement",
            )?;
            self.reject_present(
                service,
                service_name,
                "gpus",
                "GPU device requests expose host accelerators and are not allowed",
            )?;
            self.reject_present(
                service,
                service_name,
                "extends",
                "extends can import privileged settings from another compose file; \
                 inline the service definition instead",
            )?;
            self.reject_host_namespace(service, service_name, "network_mode")?;
            self.reject_host_namespace(service, service_name, "pid")?;
            self.reject_host_namespace(service, service_name, "ipc")?;
            self.reject_host_namespace(service, service_name, "cgroup")?;
            self.reject_host_namespace(service, service_name, "uts")?;
            self.reject_host_namespace(service, service_name, "userns_mode")?;
            self.validate_build_options(service, service_name)?;
            self.validate_service_volumes(service, service_name, &forbidden_named_volumes)?;
        }

        Ok(())
    }

    /// Collect names of top-level named volumes whose `driver_opts.device`
    /// binds a forbidden host path. These are local-bind volumes that smuggle a
    /// host path past the service-source check.
    fn forbidden_named_volumes(root: &YamlValue) -> HashSet<String> {
        let mut forbidden = HashSet::new();
        let Some(volumes) = root.get("volumes").and_then(YamlValue::as_mapping) else {
            return forbidden;
        };
        for (name, def) in volumes {
            let Some(name) = name.as_str() else {
                continue;
            };
            let Some(def_map) = def.as_mapping() else {
                continue;
            };
            let Some(driver_opts) = def_map
                .get(YamlValue::String("driver_opts".to_string()))
                .and_then(YamlValue::as_mapping)
            else {
                continue;
            };
            if let Some(device) = driver_opts
                .get(YamlValue::String("device".to_string()))
                .and_then(YamlValue::as_str)
            {
                if Self::is_dangerous_host_path(device) {
                    forbidden.insert(name.to_string());
                }
            }
        }
        forbidden
    }

    /// Reject top-level `configs.*.file` / `secrets.*.file` entries that point at
    /// forbidden or project-escaping host paths (e.g. `/etc/passwd`).
    fn validate_top_level_files(&self, root: &YamlValue, key: &str) -> Result<(), ComposeError> {
        let Some(map) = root.get(key).and_then(YamlValue::as_mapping) else {
            return Ok(());
        };
        for (name, def) in map {
            let name = name.as_str().unwrap_or("<unknown>");
            let Some(def_map) = def.as_mapping() else {
                continue;
            };
            if let Some(file) = def_map
                .get(YamlValue::String("file".to_string()))
                .and_then(YamlValue::as_str)
            {
                if Self::is_dangerous_host_path(file) {
                    return Err(ComposeError::SecurityPolicyViolation {
                        service: format!("{key}.{name}"),
                        field: format!("{key}.file"),
                        reason: format!(
                            "host file '{file}' exposed through {key} is not allowed"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Reject privileged build options before `docker compose build` runs them.
    fn validate_build_options(
        &self,
        service: &serde_yaml::Mapping,
        service_name: &str,
    ) -> Result<(), ComposeError> {
        let Some(build) = service.get(YamlValue::String("build".to_string())) else {
            return Ok(());
        };
        // Short form (`build: .`) is just a context path and carries no options.
        let Some(build_map) = build.as_mapping() else {
            return Ok(());
        };

        if build_map
            .get(YamlValue::String("privileged".to_string()))
            .and_then(YamlValue::as_bool)
            == Some(true)
        {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: "build.privileged".to_string(),
                reason: "privileged build steps can escape the build sandbox".to_string(),
            });
        }
        if build_map.contains_key(YamlValue::String("entitlements".to_string())) {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: "build.entitlements".to_string(),
                reason: "build entitlements (e.g. security.insecure) grant host access".to_string(),
            });
        }
        if build_map
            .get(YamlValue::String("network".to_string()))
            .and_then(YamlValue::as_str)
            == Some("host")
        {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: "build.network".to_string(),
                reason: "host network during build is not allowed".to_string(),
            });
        }
        Ok(())
    }

    fn reject_bool(
        &self,
        service: &serde_yaml::Mapping,
        service_name: &str,
        field: &str,
        rejected: bool,
        reason: &str,
    ) -> Result<(), ComposeError> {
        if service
            .get(YamlValue::String(field.to_string()))
            .and_then(YamlValue::as_bool)
            == Some(rejected)
        {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: field.to_string(),
                reason: reason.to_string(),
            });
        }
        Ok(())
    }

    fn reject_present(
        &self,
        service: &serde_yaml::Mapping,
        service_name: &str,
        field: &str,
        reason: &str,
    ) -> Result<(), ComposeError> {
        if service.contains_key(YamlValue::String(field.to_string())) {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: field.to_string(),
                reason: reason.to_string(),
            });
        }
        Ok(())
    }

    fn reject_host_namespace(
        &self,
        service: &serde_yaml::Mapping,
        service_name: &str,
        field: &str,
    ) -> Result<(), ComposeError> {
        let Some(value) = service.get(YamlValue::String(field.to_string())) else {
            return Ok(());
        };
        if value.as_str().is_some_and(|v| v == "host") {
            return Err(ComposeError::SecurityPolicyViolation {
                service: service_name.to_string(),
                field: field.to_string(),
                reason: "host namespace sharing is not allowed for compose deployments".to_string(),
            });
        }
        Ok(())
    }

    fn validate_service_volumes(
        &self,
        service: &serde_yaml::Mapping,
        service_name: &str,
        forbidden_named_volumes: &HashSet<String>,
    ) -> Result<(), ComposeError> {
        let Some(volumes) = service.get(YamlValue::String("volumes".to_string())) else {
            return Ok(());
        };
        let Some(entries) = volumes.as_sequence() else {
            return Ok(());
        };

        for entry in entries {
            let Some(source) = Self::volume_source(entry) else {
                continue;
            };

            // Reject interpolation in bind sources. `${HOST_ROOT:-/}` cannot be
            // statically validated, so a `/`-style check is trivially bypassed.
            if Self::contains_interpolation(&source) {
                return Err(ComposeError::SecurityPolicyViolation {
                    service: service_name.to_string(),
                    field: "volumes".to_string(),
                    reason: format!(
                        "interpolation in bind mount source '{source}' is not allowed; \
                         it cannot be statically validated"
                    ),
                });
            }

            // A bare name (no path separators, not relative) is a named volume
            // reference, not a host bind. It is only dangerous if the named
            // volume binds a forbidden host path via driver_opts.
            if Self::is_named_volume_ref(&source) {
                if forbidden_named_volumes.contains(&source) {
                    return Err(ComposeError::SecurityPolicyViolation {
                        service: service_name.to_string(),
                        field: "volumes".to_string(),
                        reason: format!(
                            "named volume '{source}' binds a forbidden host path via driver_opts"
                        ),
                    });
                }
                continue;
            }

            // Host bind mount: normalize `..`/`.` and reject absolute host paths
            // outside the sandbox or relative paths that escape the project dir.
            if Self::is_dangerous_host_path(&source) {
                return Err(ComposeError::SecurityPolicyViolation {
                    service: service_name.to_string(),
                    field: "volumes".to_string(),
                    reason: format!("host bind mount source '{source}' is not allowed"),
                });
            }
        }

        Ok(())
    }

    fn volume_source(entry: &YamlValue) -> Option<String> {
        if let Some(value) = entry.as_str() {
            return value.split(':').next().map(str::to_string);
        }

        let mapping = entry.as_mapping()?;
        mapping
            .get(YamlValue::String("source".to_string()))
            .and_then(YamlValue::as_str)
            .map(str::to_string)
    }

    /// Whether a string contains compose/shell variable interpolation.
    fn contains_interpolation(value: &str) -> bool {
        value.contains("${") || value.contains("$(")
    }

    /// A bare volume name (no path separators and not relative) references a
    /// named volume rather than a host bind path.
    fn is_named_volume_ref(source: &str) -> bool {
        !source.contains('/') && !source.starts_with('.') && !source.is_empty()
    }

    /// Whether a host path is dangerous: it interpolates, resolves to a
    /// forbidden absolute host path, or escapes the compose project directory
    /// via `..`. Paths are normalized lexically first so `../../etc` and
    /// `/tmp/../etc` cannot bypass the absolute-path block.
    fn is_dangerous_host_path(source: &str) -> bool {
        if Self::contains_interpolation(source) {
            return true;
        }
        let normalized = Self::lexically_normalize(source);
        // Relative path that climbs above the project directory.
        if normalized == ".." || normalized.starts_with("../") {
            return true;
        }
        // Absolute host path outside the permitted /tmp sandbox.
        if normalized.starts_with('/') && !normalized.starts_with("/tmp/") {
            return true;
        }
        false
    }

    /// Lexically normalize a path: collapse `.` and resolve `..` without
    /// touching the filesystem. Relative `..` that escapes the base is kept as
    /// a leading `..` so callers can detect project-directory escape.
    fn lexically_normalize(source: &str) -> String {
        let is_absolute = source.starts_with('/');
        let mut stack: Vec<&str> = Vec::new();
        for comp in source.split('/') {
            match comp {
                "" | "." => {}
                ".." => match stack.last() {
                    Some(&last) if last != ".." => {
                        stack.pop();
                    }
                    _ => {
                        // For absolute paths, `..` at the root is a no-op.
                        if !is_absolute {
                            stack.push("..");
                        }
                    }
                },
                other => stack.push(other),
            }
        }
        let joined = stack.join("/");
        if is_absolute {
            format!("/{joined}")
        } else if joined.is_empty() {
            ".".to_string()
        } else {
            joined
        }
    }

    /// Check if a compose file contains build: directives (services that need building)
    fn has_build_directives(&self, compose_content: &str) -> bool {
        for line in compose_content.lines() {
            let trimmed = line.trim();
            if trimmed == "build:" || trimmed.starts_with("build:") {
                return true;
            }
        }
        false
    }

    /// Run docker compose build for services with build: directives
    async fn compose_build(
        &self,
        project_dir: &Path,
        project_name: &str,
        compose_file: &str,
        env_vars: &HashMap<String, String>,
    ) -> Result<(), ComposeError> {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["compose", "-p", project_name])
            .args(["-f", compose_file])
            .args(["build", "--pull"])
            .current_dir(project_dir)
            .env("PWD", project_dir.to_string_lossy().to_string());

        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        debug!(project = %project_name, "Running docker compose build");

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComposeError::CommandFailed {
                project: project_name.to_string(),
                reason: format!("docker compose build failed: {}", stderr),
            });
        }

        info!(project = %project_name, "docker compose build completed");
        Ok(())
    }

    /// Remove containers that would conflict with compose services.
    /// This handles the case where a container with a hardcoded `container_name:`
    /// already exists from a previous deployment (e.g., old stacks system).
    async fn remove_conflicting_containers(&self, compose_content: &str) {
        // Parse container_name: values from compose YAML
        let mut next_is_container_name = false;
        for line in compose_content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("container_name:") {
                let name = trimmed
                    .trim_start_matches("container_name:")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !name.is_empty() {
                    // Try to stop and remove this container if it exists
                    debug!(container = %name, "Removing conflicting container");
                    let _ = self.docker.stop_container(name, None).await;
                    let _ = self
                        .docker
                        .remove_container(
                            name,
                            Some(bollard::query_parameters::RemoveContainerOptions {
                                force: true,
                                ..Default::default()
                            }),
                        )
                        .await;
                }
            }
            // Handle multi-line container_name (unlikely but safe)
            if next_is_container_name {
                next_is_container_name = false;
            }
            if trimmed == "container_name:" {
                next_is_container_name = true;
            }
        }
    }

    async fn compose_up(
        &self,
        project_dir: &Path,
        project_name: &str,
        compose_file: &str,
        env_vars: &HashMap<String, String>,
    ) -> Result<(), ComposeError> {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["compose", "-p", project_name])
            .args(["-f", compose_file]);

        // Include Temps env override (auto-generated)
        let temps_override = project_dir.join("docker-compose.temps-env.yml");
        if temps_override.exists() {
            cmd.args(["-f", "docker-compose.temps-env.yml"]);
        }

        // Include Temps labels override (injects sh.temps.* labels for log collection)
        let labels_override = project_dir.join("docker-compose.temps-labels.yml");
        if labels_override.exists() {
            cmd.args(["-f", "docker-compose.temps-labels.yml"]);
        }

        // Include user-provided override (ports, volumes, etc.)
        let user_override = project_dir.join("docker-compose.temps-override.yml");
        if user_override.exists() {
            cmd.args(["-f", "docker-compose.temps-override.yml"]);
        }

        // Include Temps security override LAST so its sandbox hardening
        // (cap_drop, no-new-privileges, pids_limit, init) wins over anything a
        // user/preset override tried to weaken. Compose applies `-f` files in
        // order, with later files overriding earlier ones.
        let security_override = project_dir.join("docker-compose.temps-security.yml");
        if security_override.exists() {
            cmd.args(["-f", "docker-compose.temps-security.yml"]);
        }

        // Load .env.temps for YAML variable substitution (${VAR} in compose file)
        let temps_env_path = project_dir.join(".env.temps");
        if temps_env_path.exists() {
            cmd.args(["--env-file", ".env.temps"]);
        }

        // Also load repo .env if it exists
        let repo_env_path = project_dir.join(".env");
        if repo_env_path.exists() {
            cmd.args(["--env-file", ".env"]);
        }

        cmd.args([
            "up",
            "-d",
            "--pull",
            "always",
            "--remove-orphans",
            "--force-recreate",
        ])
        .current_dir(project_dir);

        // Set PWD so compose files using ${PWD} resolve correctly
        cmd.env("PWD", project_dir.to_string_lossy().to_string());

        // Pass environment variables for compose YAML substitution (process env)
        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        debug!(project = %project_name, "Running docker compose up");

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComposeError::CommandFailed {
                project: project_name.to_string(),
                reason: format!("docker compose up failed: {}", stderr),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!(project = %project_name, stdout = %stdout, "docker compose up completed");

        Ok(())
    }

    async fn discover_containers(
        &self,
        project_dir: &Path,
        project_name: &str,
        compose_file: &str,
    ) -> Result<Vec<ComposeServiceResult>, ComposeError> {
        // Use docker compose ps to list containers
        let output = tokio::process::Command::new("docker")
            .args(["compose", "-p", project_name])
            .args(["-f", compose_file])
            .args(["ps", "--format", "json", "--all"])
            .current_dir(project_dir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComposeError::DiscoveryFailed {
                project: project_name.to_string(),
                reason: format!("docker compose ps failed: {}", stderr),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut results = Vec::new();

        // docker compose ps --format json outputs one JSON object per line
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let ps_entry: ComposePsEntry =
                serde_json::from_str(line).map_err(|e| ComposeError::DiscoveryFailed {
                    project: project_name.to_string(),
                    reason: format!("Failed to parse compose ps output: {} (line: {})", e, line),
                })?;

            // Parse published ports
            let ports = self.parse_publishers(&ps_entry.publishers);

            // Resolve full container ID via Docker inspect (compose ps returns short IDs)
            let full_id = match self.docker.inspect_container(&ps_entry.id, None).await {
                Ok(info) => info.id.unwrap_or(ps_entry.id.clone()),
                Err(_) => ps_entry.id.clone(),
            };

            results.push(ComposeServiceResult {
                container_id: full_id,
                container_name: ps_entry.name,
                service_name: ps_entry.service,
                image_name: ps_entry.image,
                ports,
                status: ps_entry.state,
            });
        }

        debug!(
            project = %project_name,
            services = results.len(),
            "Discovered compose containers"
        );

        Ok(results)
    }

    fn parse_publishers(&self, publishers: &[ComposePsPublisher]) -> Vec<ComposePortBinding> {
        publishers
            .iter()
            .filter(|p| p.published_port > 0)
            .map(|p| ComposePortBinding {
                host_port: p.published_port,
                container_port: p.target_port,
                protocol: p.protocol.clone(),
            })
            .collect()
    }

    async fn apply_labels(
        &self,
        container_id: &str,
        base_labels: &HashMap<String, String>,
        service_name: &str,
    ) -> Result<(), ComposeError> {
        // Bollard doesn't support updating labels on a running container directly.
        // We need to use `docker container update` is also limited.
        // Instead, we verify the container exists and log the labels.
        // The labels were already set via compose labels or we use docker inspect
        // to verify the container is running.
        //
        // For Temps integration, we rely on:
        // 1. The compose project name (temps-{project_id}-{env_id}) for discovery
        // 2. The container IDs stored in deployment_containers table
        // 3. Container names for log aggregation
        //
        // The deployment pipeline inserts these containers into deployment_containers
        // with the correct project_id, environment_id, deployment_id, and service_name.
        // The proxy and monitoring systems use deployment_containers for lookup,
        // not Docker labels.

        let inspect = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| ComposeError::Docker(format!("inspect failed: {}", e)))?;

        let state = inspect
            .state
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{:?}", s))
            .unwrap_or_else(|| "unknown".to_string());

        debug!(
            container_id = %container_id,
            service = %service_name,
            state = %state,
            labels = ?base_labels.keys().collect::<Vec<_>>(),
            "Verified compose container"
        );

        Ok(())
    }

    /// Generate a docker-compose.temps-override.yml that adds env_file to every service.
    /// This injects Temps system env vars into all containers without modifying
    /// the original compose file.
    fn generate_env_override(&self, compose_content: &str, env_file: &str) -> String {
        // Parse service names from compose content (simple YAML parsing)
        let mut services = Vec::new();
        let mut in_services = false;
        let mut services_indent: usize = 0;
        let mut service_indent: Option<usize> = None; // indent of first service found

        for line in compose_content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let indent = line.len() - line.trim_start().len();

            if trimmed == "services:" || trimmed.starts_with("services:") {
                in_services = true;
                services_indent = indent;
                service_indent = None;
                continue;
            }

            if in_services {
                // If we go back to root level, stop
                if indent <= services_indent {
                    in_services = false;
                    continue;
                }

                // Service names are keys at the first level after services:
                if trimmed.ends_with(':') && !trimmed.contains(' ') && !trimmed.starts_with('-') {
                    match service_indent {
                        None => {
                            // First service found — set the indent level
                            service_indent = Some(indent);
                            services.push(trimmed.trim_end_matches(':').to_string());
                        }
                        Some(si) if indent == si => {
                            // Same indent as first service — it's a service
                            services.push(trimmed.trim_end_matches(':').to_string());
                        }
                        _ => {
                            // Deeper indent — it's a property (image:, ports:, etc.), skip
                        }
                    }
                }
            }
        }

        if services.is_empty() {
            return String::new();
        }

        let mut override_yaml = String::from("services:\n");
        for service in &services {
            override_yaml.push_str(&format!("  {}:\n", service));
            override_yaml.push_str("    env_file:\n");
            override_yaml.push_str(&format!("      - {}\n", env_file));
        }

        override_yaml
    }

    /// Generate a docker-compose override that applies the same baseline sandboxing
    /// used by the single-container Docker runtime.
    fn generate_security_override(&self, compose_content: &str) -> String {
        // Enumerate service names from the parsed YAML mapping so inline
        // mappings (`web: {image: nginx}`), anchors (`web: &app`), and merge
        // keys are all hardened, not just lines that end in `:`.
        let services = self.parse_service_names_yaml(compose_content);

        if services.is_empty() {
            return String::new();
        }

        let mut override_yaml = String::from("services:\n");
        for service in &services {
            override_yaml.push_str(&format!("  {}:\n", service));
            override_yaml.push_str("    cap_drop:\n");
            override_yaml.push_str("      - ALL\n");
            override_yaml.push_str("    security_opt:\n");
            override_yaml.push_str("      - no-new-privileges:true\n");
            override_yaml.push_str("    pids_limit: 512\n");
            override_yaml.push_str("    init: true\n");
        }

        override_yaml
    }

    /// Generate a docker-compose override that adds Temps labels to every service.
    /// These labels are required for log collection, monitoring, and container discovery.
    fn generate_labels_override(
        &self,
        compose_content: &str,
        labels: &HashMap<String, String>,
    ) -> String {
        // Reuse the same service parsing logic
        let services = self.parse_service_names(compose_content);

        if services.is_empty() || labels.is_empty() {
            return String::new();
        }

        let mut override_yaml = String::from("services:\n");
        for service in &services {
            override_yaml.push_str(&format!("  {}:\n", service));
            override_yaml.push_str("    labels:\n");
            for (key, value) in labels {
                override_yaml.push_str(&format!("      {}: \"{}\"\n", key, value));
            }
            // Per-service label: the compose service name
            override_yaml.push_str(&format!("      sh.temps.service: \"{}\"\n", service));
        }

        override_yaml
    }

    /// Enumerate service names from parsed compose YAML (with merge keys
    /// expanded). Falls back to the line-based parser if the content is not
    /// valid YAML or has no `services:` mapping.
    fn parse_service_names_yaml(&self, compose_content: &str) -> Vec<String> {
        let mut root: YamlValue = match serde_yaml::from_str(compose_content) {
            Ok(value) => value,
            Err(_) => return self.parse_service_names(compose_content),
        };
        let _ = root.apply_merge();
        match root.get("services").and_then(YamlValue::as_mapping) {
            Some(services) => {
                let names: Vec<String> = services
                    .keys()
                    .filter_map(|k| k.as_str().map(str::to_string))
                    .collect();
                if names.is_empty() {
                    self.parse_service_names(compose_content)
                } else {
                    names
                }
            }
            None => self.parse_service_names(compose_content),
        }
    }

    /// Parse service names from compose YAML content.
    fn parse_service_names(&self, compose_content: &str) -> Vec<String> {
        let mut services = Vec::new();
        let mut in_services = false;
        let mut services_indent: usize = 0;
        let mut service_indent: Option<usize> = None;

        for line in compose_content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let indent = line.len() - line.trim_start().len();

            if trimmed == "services:" || trimmed.starts_with("services:") {
                in_services = true;
                services_indent = indent;
                service_indent = None;
                continue;
            }

            if in_services {
                if indent <= services_indent {
                    in_services = false;
                    continue;
                }

                if trimmed.ends_with(':') && !trimmed.contains(' ') && !trimmed.starts_with('-') {
                    match service_indent {
                        None => {
                            service_indent = Some(indent);
                            services.push(trimmed.trim_end_matches(':').to_string());
                        }
                        Some(si) if indent == si => {
                            services.push(trimmed.trim_end_matches(':').to_string());
                        }
                        _ => {}
                    }
                }
            }
        }

        services
    }

    /// Parse a user override YAML and return the names of services that define `ports:`.
    fn services_with_ports_in_override(&self, override_content: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut in_services = false;
        let mut services_indent: usize = 0;
        let mut current_service: Option<(String, usize)> = None; // (name, indent)

        for line in override_content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let indent = line.len() - line.trim_start().len();

            if trimmed == "services:" || trimmed.starts_with("services:") {
                in_services = true;
                services_indent = indent;
                current_service = None;
                continue;
            }

            if !in_services {
                continue;
            }

            // Left of services block
            if indent <= services_indent && !trimmed.is_empty() {
                in_services = false;
                continue;
            }

            // Inside a service — check for ports: before checking service names
            if let Some((ref svc_name, svc_indent)) = current_service {
                if indent > svc_indent && (trimmed == "ports:" || trimmed.starts_with("ports:")) {
                    if !result.contains(svc_name) {
                        result.push(svc_name.clone());
                    }
                    continue;
                }
            }

            // Service-level key (direct child of services:)
            if trimmed.ends_with(':') && !trimmed.contains(' ') && !trimmed.starts_with('-') {
                let svc_name = trimmed.trim_end_matches(':').to_string();
                match &current_service {
                    None => {
                        current_service = Some((svc_name, indent));
                    }
                    Some((_, si)) if indent == *si => {
                        current_service = Some((svc_name, indent));
                    }
                    _ => {}
                }
            }
        }

        result
    }

    /// Strip `ports:` sections from the base compose content for the given services only.
    /// Other services keep their ports untouched.
    fn strip_ports_for_services(&self, compose_content: &str, services: &[String]) -> String {
        let mut output = String::new();
        let mut in_services_block = false;
        let mut services_indent: usize = 0;
        let mut current_service: Option<(String, usize)> = None;
        let mut service_indent: Option<usize> = None;
        let mut skipping_ports = false;
        let mut ports_indent: usize = 0;

        for line in compose_content.lines() {
            let trimmed = line.trim();
            let indent = line.len() - line.trim_start().len();

            // Track services: block
            if trimmed == "services:" || trimmed.starts_with("services:") {
                in_services_block = true;
                services_indent = indent;
                service_indent = None;
                current_service = None;
                skipping_ports = false;
                output.push_str(line);
                output.push('\n');
                continue;
            }

            // If currently skipping a ports block, check if we've exited it
            if skipping_ports {
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    // Skip blank lines and comments inside ports block
                    continue;
                }
                if indent > ports_indent {
                    // Still inside ports block (port entries are indented further)
                    continue;
                }
                // We've exited the ports block
                skipping_ports = false;
            }

            if in_services_block && !trimmed.is_empty() && indent <= services_indent {
                in_services_block = false;
                current_service = None;
                service_indent = None;
            }

            if in_services_block && !trimmed.is_empty() && !trimmed.starts_with('#') {
                // Detect service names
                if trimmed.ends_with(':') && !trimmed.contains(' ') && !trimmed.starts_with('-') {
                    match service_indent {
                        None => {
                            service_indent = Some(indent);
                            let name = trimmed.trim_end_matches(':').to_string();
                            current_service = Some((name, indent));
                        }
                        Some(si) if indent == si => {
                            let name = trimmed.trim_end_matches(':').to_string();
                            current_service = Some((name, indent));
                        }
                        _ => {}
                    }
                }

                // Check if this line is `ports:` inside a service we need to strip
                if let Some((ref svc_name, svc_indent)) = current_service {
                    if indent > svc_indent
                        && (trimmed == "ports:" || trimmed.starts_with("ports:"))
                        && services.contains(svc_name)
                    {
                        // If it's `ports:` with inline value like `ports: ["80:80"]`
                        if trimmed.starts_with("ports:") && trimmed != "ports:" {
                            // Single-line ports — just skip this line
                            continue;
                        }
                        // Block-style ports: — skip this line and subsequent indented lines
                        skipping_ports = true;
                        ports_indent = indent;
                        continue;
                    }
                }
            }

            output.push_str(line);
            output.push('\n');
        }

        output
    }

    fn find_compose_file(&self, project_dir: &Path) -> String {
        for name in &[
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yml",
            "compose.yaml",
        ] {
            if project_dir.join(name).exists() {
                return name.to_string();
            }
        }
        "docker-compose.yml".to_string()
    }
}

/// JSON output from `docker compose ps --format json`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ComposePsEntry {
    #[serde(alias = "ID")]
    id: String,
    name: String,
    service: String,
    image: String,
    state: String,
    #[serde(default)]
    publishers: Vec<ComposePsPublisher>,
}

/// Port publisher from `docker compose ps --format json`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ComposePsPublisher {
    #[serde(default)]
    published_port: u16,
    #[serde(default)]
    target_port: u16,
    #[serde(default)]
    protocol: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compose_ps_json() {
        let json = r#"{"ID":"abc123","Name":"myapp-web-1","Service":"web","Image":"nginx:latest","State":"running","Publishers":[{"URL":"0.0.0.0","TargetPort":80,"PublishedPort":8080,"Protocol":"tcp"}]}"#;

        let entry: ComposePsEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "abc123");
        assert_eq!(entry.service, "web");
        assert_eq!(entry.state, "running");
        assert_eq!(entry.publishers.len(), 1);
        assert_eq!(entry.publishers[0].published_port, 8080);
        assert_eq!(entry.publishers[0].target_port, 80);
    }

    #[test]
    fn test_parse_compose_ps_no_ports() {
        let json = r#"{"ID":"def456","Name":"myapp-redis-1","Service":"redis","Image":"redis:7","State":"running","Publishers":[]}"#;

        let entry: ComposePsEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.service, "redis");
        assert!(entry.publishers.is_empty());
    }

    #[test]
    fn test_parse_publishers() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            // Can still test parse_publishers without Docker
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let publishers = vec![
            ComposePsPublisher {
                published_port: 8080,
                target_port: 80,
                protocol: "tcp".to_string(),
            },
            ComposePsPublisher {
                published_port: 0, // Not published
                target_port: 6379,
                protocol: "tcp".to_string(),
            },
        ];

        let ports = executor.parse_publishers(&publishers);
        assert_eq!(ports.len(), 1); // Only the published port
        assert_eq!(ports[0].host_port, 8080);
        assert_eq!(ports[0].container_port, 80);
    }

    #[test]
    fn test_generate_env_override() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"
services:
  web:
    image: nginx
    ports:
      - "8080:80"
  redis:
    image: redis:7
  postgres:
    image: postgres:17
"#;

        let override_yaml = executor.generate_env_override(compose, ".env.temps");
        assert!(override_yaml.contains("web:"));
        assert!(override_yaml.contains("redis:"));
        assert!(override_yaml.contains("postgres:"));
        assert!(override_yaml.contains(".env.temps"));
        // Each service should have env_file
        assert_eq!(override_yaml.matches("env_file:").count(), 3);
    }

    #[test]
    fn test_has_build_directives() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        // No build
        assert!(!executor.has_build_directives("services:\n  web:\n    image: nginx\n"));

        // build: with context
        assert!(executor.has_build_directives("services:\n  web:\n    build: .\n"));

        // build: block
        assert!(executor.has_build_directives(
            "services:\n  web:\n    build:\n      context: .\n      dockerfile: Dockerfile\n"
        ));
    }

    #[test]
    fn test_generate_env_override_empty() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = "version: '3'\n";
        let override_yaml = executor.generate_env_override(compose, ".env.temps");
        assert!(override_yaml.is_empty());
    }

    #[test]
    fn test_services_with_ports_in_override() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let override_content = r#"
services:
  clickhouse:
    ports:
      - '127.0.0.1:28123:8123'
      - '127.0.0.1:29001:9000'
"#;
        let result = executor.services_with_ports_in_override(override_content);
        assert_eq!(result, vec!["clickhouse"]);

        // No ports override
        let override_no_ports = r#"
services:
  clickhouse:
    environment:
      - FOO=bar
"#;
        let result = executor.services_with_ports_in_override(override_no_ports);
        assert!(result.is_empty());

        // Multiple services, only one with ports
        let override_mixed = r#"
services:
  web:
    ports:
      - '8080:80'
  redis:
    environment:
      - REDIS_PASSWORD=secret
"#;
        let result = executor.services_with_ports_in_override(override_mixed);
        assert_eq!(result, vec!["web"]);
    }

    #[test]
    fn test_strip_ports_for_services() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"version: '3.8'
services:
  clickhouse:
    image: clickhouse/clickhouse-server:23.4
    ports:
      - '8123:8123'
      - '9000:9000'
    volumes:
      - ./data:/var/lib/clickhouse
  keeper:
    image: clickhouse/clickhouse-keeper:23.4-alpine
    ports:
      - '9181:9181'
"#;

        // Strip ports only for clickhouse, keep keeper's ports
        let result = executor.strip_ports_for_services(compose, &["clickhouse".to_string()]);
        assert!(!result.contains("8123:8123"));
        assert!(!result.contains("9000:9000"));
        assert!(result.contains("9181:9181")); // keeper untouched
        assert!(result.contains("volumes:")); // other sections preserved
        assert!(result.contains("./data:/var/lib/clickhouse"));

        // Strip ports for both
        let result = executor
            .strip_ports_for_services(compose, &["clickhouse".to_string(), "keeper".to_string()]);
        assert!(!result.contains("8123:8123"));
        assert!(!result.contains("9000:9000"));
        assert!(!result.contains("9181:9181"));
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_privileged_host_escape() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"
services:
  pwn:
    image: alpine
    privileged: true
    network_mode: host
    pid: host
    cap_add:
      - SYS_ADMIN
    devices:
      - /dev/kmsg:/dev/kmsg
    volumes:
      - /:/host:rw
"#;

        let error = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert!(matches!(
            error,
            ComposeError::SecurityPolicyViolation { field, .. } if field == "privileged"
        ));
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_docker_socket_mount() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"
services:
  worker:
    image: alpine
    volumes:
      - type: bind
        source: /var/run/docker.sock
        target: /var/run/docker.sock
"#;

        let error = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert!(matches!(
            error,
            ComposeError::SecurityPolicyViolation { field, .. } if field == "volumes"
        ));
    }

    #[test]
    fn test_generate_security_override() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"
services:
  web:
    image: nginx
  worker:
    image: alpine
"#;
        let override_yaml = executor.generate_security_override(compose);

        assert_eq!(override_yaml.matches("cap_drop:").count(), 2);
        assert_eq!(override_yaml.matches("no-new-privileges:true").count(), 2);
        assert_eq!(override_yaml.matches("pids_limit: 512").count(), 2);
        assert_eq!(override_yaml.matches("init: true").count(), 2);
    }

    /// Build an executor for tests, skipping when Docker is unavailable.
    fn test_executor() -> Option<ComposeExecutor> {
        let docker = Docker::connect_with_defaults().ok()?;
        Some(ComposeExecutor::new(
            Arc::new(docker),
            PathBuf::from("/tmp/test"),
        ))
    }

    fn violation_field(err: ComposeError) -> String {
        match err {
            ComposeError::SecurityPolicyViolation { field, .. } => field,
            other => panic!("expected SecurityPolicyViolation, got {other:?}"),
        }
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_interpolated_bind_source() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = r#"
services:
  pwn:
    image: alpine
    volumes:
      - "${HOST_ROOT:-/}:/host:rw"
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "volumes");
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_extends() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = r#"
services:
  app:
    image: alpine
    extends:
      file: malicious.yml
      service: privileged_base
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "extends");
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_use_api_socket() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = r#"
services:
  app:
    image: alpine
    use_api_socket: true
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "use_api_socket");
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_relative_escape_bind() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = r#"
services:
  pwn:
    image: alpine
    volumes:
      - ../../../../etc:/host:rw
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "volumes");

        // A relative path that stays inside the project dir is allowed.
        let ok = r#"
services:
  app:
    image: alpine
    volumes:
      - ./data:/data:rw
"#;
        assert!(executor
            .validate_compose_security_policy("compose file", ok)
            .is_ok());
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_privileged_build_options() {
        let Some(executor) = test_executor() else {
            return;
        };
        for compose in [
            "services:\n  app:\n    build:\n      context: .\n      privileged: true\n",
            "services:\n  app:\n    build:\n      context: .\n      network: host\n",
            "services:\n  app:\n    build:\n      context: .\n      entitlements:\n        - security.insecure\n",
        ] {
            let err = executor
                .validate_compose_security_policy("compose file", compose)
                .unwrap_err();
            assert!(violation_field(err).starts_with("build."));
        }
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_named_volume_driver_device() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = r#"
services:
  pwn:
    image: alpine
    volumes:
      - hostroot:/host
volumes:
  hostroot:
    driver_opts:
      type: none
      o: bind
      device: /
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "volumes");
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_configs_and_secrets_files() {
        let Some(executor) = test_executor() else {
            return;
        };
        let configs = r#"
services:
  app:
    image: alpine
configs:
  hostfile:
    file: /etc/passwd
"#;
        assert_eq!(
            violation_field(
                executor
                    .validate_compose_security_policy("compose file", configs)
                    .unwrap_err()
            ),
            "configs.file"
        );

        let secrets = r#"
services:
  app:
    image: alpine
secrets:
  hostsecret:
    file: ../../../../etc/shadow
"#;
        assert_eq!(
            violation_field(
                executor
                    .validate_compose_security_policy("compose file", secrets)
                    .unwrap_err()
            ),
            "secrets.file"
        );
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_remaining_host_namespaces() {
        let Some(executor) = test_executor() else {
            return;
        };
        for (field, compose) in [
            ("cgroup", "services:\n  app:\n    image: alpine\n    cgroup: host\n"),
            (
                "userns_mode",
                "services:\n  app:\n    image: alpine\n    userns_mode: \"host\"\n",
            ),
            ("uts", "services:\n  app:\n    image: alpine\n    uts: \"host\"\n"),
        ] {
            let err = executor
                .validate_compose_security_policy("compose file", compose)
                .unwrap_err();
            assert_eq!(violation_field(err), field);
        }
    }

    #[test]
    fn test_validate_compose_security_policy_rejects_gpus() {
        let Some(executor) = test_executor() else {
            return;
        };
        let compose = "services:\n  app:\n    image: alpine\n    gpus: all\n";
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "gpus");
    }

    #[test]
    fn test_validate_compose_security_policy_resolves_merge_keys() {
        let Some(executor) = test_executor() else {
            return;
        };
        // The privileged setting is inherited via a `<<` merge key from an anchor.
        let compose = r#"
x-base: &base
  privileged: true
services:
  app:
    image: alpine
    <<: *base
"#;
        let err = executor
            .validate_compose_security_policy("compose file", compose)
            .unwrap_err();
        assert_eq!(violation_field(err), "privileged");
    }

    #[test]
    fn test_generate_security_override_inline_and_anchor_services() {
        let Some(executor) = test_executor() else {
            return;
        };
        // Inline mapping and anchor service definitions that the old
        // line-based parser missed.
        let compose = r#"
services:
  web: { image: nginx }
  worker: &app
    image: alpine
"#;
        let override_yaml = executor.generate_security_override(compose);
        assert!(override_yaml.contains("web:"));
        assert!(override_yaml.contains("worker:"));
        assert_eq!(override_yaml.matches("cap_drop:").count(), 2);
        assert_eq!(override_yaml.matches("init: true").count(), 2);
    }

    #[test]
    fn test_lexically_normalize() {
        assert_eq!(
            ComposeExecutor::lexically_normalize("../../../../etc"),
            "../../../../etc"
        );
        assert_eq!(ComposeExecutor::lexically_normalize("/tmp/../etc"), "/etc");
        assert_eq!(ComposeExecutor::lexically_normalize("./data"), "data");
        assert_eq!(ComposeExecutor::lexically_normalize("/"), "/");
        assert!(ComposeExecutor::is_dangerous_host_path("/tmp/../etc"));
        assert!(ComposeExecutor::is_dangerous_host_path("../escape"));
        assert!(!ComposeExecutor::is_dangerous_host_path("./data"));
        assert!(!ComposeExecutor::is_dangerous_host_path("/tmp/ok"));
    }

    #[test]
    fn test_strip_ports_no_services_matched() {
        let docker = Docker::connect_with_defaults();
        if docker.is_err() {
            return;
        }
        let executor = ComposeExecutor::new(Arc::new(docker.unwrap()), PathBuf::from("/tmp/test"));

        let compose = r#"services:
  web:
    image: nginx
    ports:
      - '80:80'
"#;

        // No services to strip — output should be identical
        let result = executor.strip_ports_for_services(compose, &[]);
        assert!(result.contains("80:80"));
    }
}
