# MirzaDev OneCenter — Rust + Slint

Rewrite desktop awal MirzaDev OneCenter, dengan UI Slint dan core Rust modular.
Toolchain Windows GNU dan pin toolchain di `rust-toolchain.toml` tetap menjadi
konfigurasi pengembangan.

## Status implementasi

- Empat screen dasar: Utama, Aplikasi, Bocoran, dan Pengaturan.
- Navigasi tab dan preferensi mode kaca tersimpan sebagai preferences lokal.
- Daftar APK dimuat dari GitHub Releases publik secara asynchronous.
- Kontak admin dan unduhan APK dibuka melalui handler URL Windows.
- Konten Home dan login Private belum dihubungkan ke backend privat.
- Private sengaja dinonaktifkan di Windows sampai backend authorization dan
  kebijakan device binding lintas platform disepakati.

Tidak ada GitHub token, Firebase Admin credential, service-account JSON, atau
signing key yang dibundel. Data private perlu diakses melalui layanan backend
terotorisasi sebelum integrasi klien dibuat.

## Menjalankan di Windows

```powershell
rustup run stable-x86_64-pc-windows-gnu cargo run
```

Jaringan berjalan melalui satu Tokio multi-thread runtime. Hasil service
dikirim kembali ke event loop Slint sebelum properti UI atau model diperbarui.
