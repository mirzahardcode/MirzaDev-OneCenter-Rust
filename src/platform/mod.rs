#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "android")]
pub mod android;

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return windows::open_url(url);
    }

    #[cfg(target_os = "android")]
    {
        return android::open_url(url);
    }

    #[allow(unreachable_code)]
    Err("Pembukaan tautan belum tersedia pada platform ini.".to_owned())
}
