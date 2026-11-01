pub fn open_url(url: &str) -> Result<(), String> {
    if url.chars().any(char::is_control) {
        return Err("Tautan berisi karakter yang tidak diizinkan.".to_owned());
    }

    let is_https = url.starts_with("https://");
    let is_email = url.starts_with("mailto:") && url.len() > "mailto:".len();
    if !(is_https || is_email) {
        return Err("Tautan dengan skema tersebut tidak diizinkan.".to_owned());
    }

    open::that_detached(url).map_err(|error| format!("Tidak dapat membuka tautan: {error}"))
}
