// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod escpos;
mod qr;
mod sample;
mod server;

use std::path::{Path, PathBuf};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn main() {
    let (mut cfg, headless) = server::Config::from_args();

    if headless {
        match server::start(cfg, false) {
            Ok(shared) => {
                shared.print_banner();
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    tauri::Builder::default()
        .setup(move |app| {
            // Remember the printer port chosen in the UI (unless --port/PORT was given).
            if let Ok(dir) = app.path().app_config_dir() {
                let file = dir.join("settings.json");
                if !cfg.port_from_cli {
                    if let Some(p) = server::load_saved_port(&file) {
                        cfg.port = p;
                    }
                }
                cfg.settings_file = Some(file);
            }

            // The window shows the same viewer that is served to browsers.
            let shared = server::start(cfg.clone(), true)?;
            let url: tauri::Url = format!("http://127.0.0.1:{}/", shared.viewer_port).parse()?;
            let downloads = app.path().download_dir().ok();
            let log = shared.clone();

            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("ESC/POS Virtual Printer")
                .inner_size(1240.0, 900.0)
                .min_inner_size(720.0, 480.0)
                // window.open (e.g. clicking a QR URL) → system browser
                .on_new_window(|url, _features| {
                    if matches!(url.scheme(), "http" | "https" | "mailto") {
                        open_external(url.as_str());
                    }
                    NewWindowResponse::Deny
                })
                // "download .bin" → ~/Downloads/receipt-N.bin
                .on_download(move |_webview, event| {
                    if let DownloadEvent::Requested { url, destination } = event {
                        if let Some(dir) = &downloads {
                            let id = url.path_segments().and_then(|mut s| s.nth(2)).unwrap_or("job").to_string();
                            *destination = unique_path(dir, &format!("receipt-{id}"), "bin");
                            log.log(format!("saved {}", destination.display()));
                        }
                    }
                    true
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the application");
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    path
}

fn open_external(url: &str) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    let _ = r;
}
