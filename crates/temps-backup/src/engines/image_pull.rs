//! Shared helper for ensuring a Docker image is locally cached before
//! `create_container`.
//!
//! Without this, fresh prod hosts 404 on `create_container` with
//! "No such image: …" because the engine assumed Docker would auto-pull
//! (it doesn't — bare `create_container` against a missing image fails).

use bollard::Docker;
use futures::stream::StreamExt as FuturesStreamExt;
use tracing::{debug, info};

use temps_backup_core::engine_v2::BackupError;

/// Ensure `image_tag` is pulled and available locally. No-op if Docker
/// already has the image cached; otherwise streams a pull and returns
/// after the daemon reports completion.
///
/// Pull failures are mapped to `BackupError::Failed` so the engine's caller
/// surfaces a useful message ("failed to pull '…': …") instead of a generic
/// 404 on the subsequent `create_container` call.
///
/// `engine` is the engine key (e.g. `"control_plane"`, `"postgres_pgdump"`);
/// errors carry it in logs so failures land on the right engine in the UI.
pub async fn ensure_image_pulled_v2(image_tag: &str, engine: &str) -> Result<(), BackupError> {
    let docker = connect_to_docker("ensure_image_pulled_v2", engine)?;

    if docker.inspect_image(image_tag).await.is_ok() {
        return Ok(());
    }

    info!(
        image_tag,
        engine, "ensure_image_pulled_v2: image not cached, pulling"
    );
    pull_image(&docker, image_tag, engine).await
}

/// Pull `image_tag` from its registry even when Docker already has a local image
/// with the same tag. Use this for sidecars that execute with sensitive
/// credentials, where trusting a pre-existing local tag would allow cache
/// poisoning on shared Docker daemons.
pub async fn force_pull_image_v2(image_tag: &str, engine: &str) -> Result<(), BackupError> {
    let docker = connect_to_docker("force_pull_image_v2", engine)?;

    info!(
        image_tag,
        engine, "force_pull_image_v2: refreshing image from registry"
    );
    pull_image(&docker, image_tag, engine).await
}

fn connect_to_docker(operation: &str, engine: &str) -> Result<Docker, BackupError> {
    Docker::connect_with_local_defaults().map_err(|e| BackupError::Failed {
        reason: format!(
            "{} ({}): failed to connect to Docker: {}",
            operation, engine, e
        ),
    })
}

fn split_image_tag(image_tag: &str) -> (&str, &str) {
    match image_tag.rsplit_once(':') {
        Some((image, tag)) if !tag.contains('/') => (image, tag),
        _ => (image_tag, "latest"),
    }
}

async fn pull_image(docker: &Docker, image_tag: &str, engine: &str) -> Result<(), BackupError> {
    use bollard::query_parameters::CreateImageOptionsBuilder;
    let (image, tag) = split_image_tag(image_tag);
    let options = CreateImageOptionsBuilder::new()
        .from_image(image)
        .tag(tag)
        .build();

    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(result) = FuturesStreamExt::next(&mut stream).await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    debug!(image_tag, engine, "Docker pull: {}", status);
                }
            }
            Err(e) => {
                return Err(BackupError::Failed {
                    reason: format!("failed to pull '{}': {}", image_tag, e),
                });
            }
        }
    }

    info!(image_tag, engine, "Docker pull complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_image_tag;

    #[test]
    fn split_image_tag_uses_explicit_tag() {
        assert_eq!(
            split_image_tag("minio/mc:RELEASE.2025-08-13T08-35-41Z"),
            ("minio/mc", "RELEASE.2025-08-13T08-35-41Z")
        );
    }

    #[test]
    fn split_image_tag_defaults_missing_tag_to_latest() {
        assert_eq!(split_image_tag("minio/mc"), ("minio/mc", "latest"));
    }

    #[test]
    fn split_image_tag_preserves_registry_port_without_explicit_tag() {
        assert_eq!(
            split_image_tag("registry.example.com:5000/minio/mc"),
            ("registry.example.com:5000/minio/mc", "latest")
        );
    }
}
