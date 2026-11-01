use reqwest::{Client, header::ACCEPT};

use crate::{
    domain::{
        apk_release::{GitHubRelease, map_releases},
        models::ApkRelease,
    },
    error::{AppError, AppResult},
};

const RELEASES_URL: &str = "https://api.github.com/repos/mirzahardcode/cracking-exam-app/releases";

pub async fn fetch_apks(client: &Client) -> AppResult<Vec<ApkRelease>> {
    let response = client
        .get(RELEASES_URL)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(AppError::GitHubStatus(response.status()));
    }

    let releases = response.json::<Vec<GitHubRelease>>().await?;
    Ok(map_releases(releases))
}

pub fn http_client() -> AppResult<Client> {
    Client::builder()
        .user_agent(concat!("MirzaDev-OneCenter/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(AppError::Http)
}
