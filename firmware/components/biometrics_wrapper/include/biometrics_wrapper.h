#pragma once

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint16_t display_width;   // e.g. 720
    uint16_t display_height;  // e.g. 1280
    uint16_t camera_width;    // e.g. 1280
    uint16_t camera_height;   // e.g. 720
} p4_hardware_config_t;

typedef struct {
    uint8_t *data;
    size_t data_len;
    uint16_t width;
    uint16_t height;
} p4_camera_frame_t;

// Initializes all shared MIPI hardware (I2C bus, D-PHY LDO, Display, Camera).
int32_t p4_hardware_init_all(const p4_hardware_config_t *config);

// Captures a single frame from the MIPI-CSI camera into PSRAM.
int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms);

// ESP-DL MobileFaceNet flatbuffer initializer
esp_err_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size);

// Forward pass & 512-element L2 normalized embedding extraction
esp_err_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len);

#ifdef __cplusplus
}
#endif