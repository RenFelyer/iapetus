//! Источник выпусков Claude Code: канал `stable`, манифест `<версия>/manifest.json` с подписью Anthropic и бинарники
//! `<версия>/<платформа>/claude` в каталоге выпусков.

use std::collections::HashMap;

use anyhow::{Context, ensure};
use nerve::protocol::{Agent, AgentRelease, Artifact, ArtifactFormat, Digest, Platform};
use nerve::release;
use serde::Deserialize;

use super::Source;

/// Ключ, которым Anthropic подписывает манифесты выпусков, и отпечаток его первичного ключа.
const ANTHROPIC_KEY: &str = include_str!("claude-code.asc");
const ANTHROPIC_FINGERPRINT: &str = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE";

/// Каталог выпусков.
const RELEASES_URL: &str = "https://downloads.claude.ai/claude-code-releases";

/// Платформы образа session (Debian с glibc) и их имена в выпуске.
const PLATFORMS: [(Platform, &str); 2] = [(Platform::LinuxX64, "linux-x64"), (Platform::LinuxArm64, "linux-arm64")];

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    platforms: HashMap<String, ManifestPlatform>,
}

#[derive(Debug, Deserialize)]
struct ManifestPlatform {
    checksum: String,
}

pub(super) fn source() -> Source {
    Source {
        base_url: RELEASES_URL.to_owned(),
        key: ANTHROPIC_KEY,
        fingerprint: ANTHROPIC_FINGERPRINT,
        channel: "stable",
        manifest: |version| format!("{version}/manifest.json"),
        release: release_from,
    }
}

/// Выпуск из манифеста `manifest` (подпись уже проверена) с артефактами платформ образа, у которых запись и сумма
/// верны; остальные пропускаются. Версия в манифесте должна быть той, что запрошена: манифест другого выпуска с
/// верной подписью не годится. Бинарники лежат в `base_url`.
fn release_from(manifest: &[u8], version: &str, base_url: &str) -> anyhow::Result<AgentRelease> {
    let manifest: Manifest = serde_json::from_slice(manifest).context("манифест не разобран")?;
    ensure!(
        manifest.version == version,
        "манифест версии {} получен вместо {version}",
        manifest.version
    );
    let mut artifacts = Vec::new();
    for (platform, name) in PLATFORMS {
        let Some(entry) = manifest.platforms.get(name) else {
            tracing::warn!(platform = name, version, "в манифесте нет платформы: она пропущена");
            continue;
        };
        if !release::is_sha256(&entry.checksum) {
            tracing::warn!(
                platform = name,
                version,
                "контрольная сумма платформы не sha256: платформа пропущена"
            );
            continue;
        }
        artifacts.push(Artifact {
            platform,
            url: format!("{base_url}/{version}/{name}/claude"),
            digest: Digest::Sha256(entry.checksum.clone()),
            format: ArtifactFormat::Binary,
        });
    }
    ensure!(
        !artifacts.is_empty(),
        "в манифесте нет ни одной платформы образа с верной контрольной суммой"
    );
    Ok(AgentRelease {
        agent: Agent::ClaudeCode,
        version: version.to_owned(),
        artifacts,
    })
}

#[cfg(test)]
#[path = "../../../tests/server/update/claude.rs"]
mod tests;
