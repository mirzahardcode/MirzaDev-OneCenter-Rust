use serde::Deserialize;

use super::models::ApkRelease;

#[derive(Debug, Deserialize)]
pub struct GitHubRelease {
    pub name: Option<String>,
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
}

pub fn map_releases(releases: Vec<GitHubRelease>) -> Vec<ApkRelease> {
    releases
        .into_iter()
        .flat_map(|release| {
            let release_name = release
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| release.tag_name.clone());
            release
                .assets
                .into_iter()
                .filter(|asset| asset.name.to_ascii_lowercase().ends_with(".apk"))
                .map(move |asset| ApkRelease {
                    name: release_name.clone(),
                    version: release.tag_name.clone(),
                    size: format!("{:.1} MB", asset.size as f64 / (1024.0 * 1024.0)),
                    download_url: asset.browser_download_url,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{GitHubAsset, GitHubRelease, map_releases};

    #[test]
    fn maps_only_apk_assets_and_uses_tag_when_release_name_is_blank() {
        let releases = vec![GitHubRelease {
            name: Some(" ".into()),
            tag_name: "1.2.0".into(),
            assets: vec![
                GitHubAsset {
                    name: "release.APK".into(),
                    size: 1_572_864,
                    browser_download_url: "https://example.test/app.apk".into(),
                },
                GitHubAsset {
                    name: "checksums.txt".into(),
                    size: 12,
                    browser_download_url: "https://example.test/checksums.txt".into(),
                },
            ],
        }];

        let mapped = map_releases(releases);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].name, "1.2.0");
        assert_eq!(mapped[0].version, "1.2.0");
        assert_eq!(mapped[0].size, "1.5 MB");
    }
}
