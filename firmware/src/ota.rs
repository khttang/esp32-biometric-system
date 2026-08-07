use anyhow::{bail, Context, Result};
use esp_idf_svc::http::client::{Configuration, EspHttpConnection, Method};
use esp_idf_svc::ota::EspOta;
use log::{info, warn};
use std::io::Read;

pub fn execute_rust_ota(firmware_url: &str) -> Result<()> {
    info!("[OTA] Connecting to {}...", firmware_url);

    let config = Configuration {
        buffer_size: Some(4096),
        ..Default::default()
    };
    let mut connection = EspHttpConnection::new(&config)
        .context("Failed to initialize EspHttpConnection")?;

    connection
        .initiate_request(Method::Get, firmware_url, &[])
        .context("Failed to initiate GET request")?;
    
    connection
        .initiate_response()
        .context("Failed to receive HTTP response header")?;

    let status = connection.status();
    if status != 200 {
        bail!("HTTP request failed with status code: {}", status);
    }

    let mut ota = EspOta::new().context("Failed to initialize EspOta driver")?;
    let mut ota_update = ota.initiate_update().context("Failed to initiate OTA update partition")?;

    let mut buffer = [0u8; 4096];
    let mut total_bytes = 0;

    loop {
        let read = connection.read(&mut buffer).context("Error reading HTTP stream")?;
        if read == 0 {
            break; // EOF
        }
        ota_update.write(&buffer[..read]).context("Failed writing chunk to OTA partition")?;
        total_bytes += read;
    }

    info!("[OTA] Flashed {} bytes successfully!", total_bytes);
    ota_update.complete().context("Failed to finalize OTA update")?;

    warn!("[OTA] Rebooting into new partition in 2 seconds...");
    std::thread::sleep(std::time::Duration::from_secs(2));

    unsafe { esp_idf_sys::esp_restart() };
}