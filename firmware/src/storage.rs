use anyhow::{bail, Context, Result};
use esp_idf_sys::{
    esp_partition_erase_range, esp_partition_find_first, esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_SPIFFS, esp_partition_t, esp_partition_type_t_ESP_PARTITION_TYPE_DATA, esp_partition_write, ESP_OK
};
use log::info;
use std::ffi::CString;

pub struct ModelPartitionWriter;

impl ModelPartitionWriter {
    pub fn write_model_weights(buffer: &[u8]) -> Result<()> {
        let name = CString::new("model").context("Failed to construct CString for partition name")?;

        unsafe {
            // Updated to bindgen enum variant names
            let part: *const esp_partition_t = esp_partition_find_first(
                esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_SPIFFS,
                name.as_ptr(),
            );

            if part.is_null() {
                bail!("Could not find 'model' partition in current partition table!");
            }

            let part_ref = &*part;
            if (buffer.len() as u32) > part_ref.size {
                bail!(
                    "Model binary size ({} bytes) exceeds partition limit ({} bytes)!",
                    buffer.len(),
                    part_ref.size
                );
            }

            info!(
                "[Flash] Found 'model' partition at 0x{:X} (Size: {} bytes)",
                part_ref.address, part_ref.size
            );

            // Sector erase (4096-byte aligned)
            let erase_size: u32 = ((buffer.len() as u32 + 4095) / 4096) * 4096;
            info!("[Flash] Erasing {} bytes of sector flash...", erase_size);

            if esp_partition_erase_range(part, 0, erase_size.try_into().unwrap()) != ESP_OK {
                bail!("Failed to erase flash partition range");
            }

            info!("[Flash] Flashing {} bytes of model weights...", buffer.len());
            if esp_partition_write(part, 0, buffer.as_ptr() as *const _, buffer.len()) != ESP_OK {
                bail!("Failed to write buffer into flash partition");
            }

            info!("[Flash] Model partition write successfully verified!");
            Ok(())
        }
    }
}