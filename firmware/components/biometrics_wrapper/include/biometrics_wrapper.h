#ifndef BIOMETRICS_WRAPPER_H
#define BIOMETRICS_WRAPPER_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// Shared Hardware Structures (Must match Rust #[repr(C)] layouts)
// -----------------------------------------------------------------------------
typedef struct {
    uint16_t display_width;
    uint16_t display_height;
    uint16_t camera_width;
    uint16_t camera_height;
} p4_hardware_config_t;

typedef struct {
    uint8_t *data;
    size_t data_len;
    uint16_t width;
    uint16_t height;
    uint32_t buffer_index;
} p4_camera_frame_t;

typedef struct {
    uint16_t x;
    uint16_t y;
    uint16_t strength;
    uint8_t points;
    bool touched;
} p4_touch_data_t;

// System Initialization APIs
int32_t p4_hardware_init_all(const p4_hardware_config_t *config);
int32_t init_display_system(void);
int32_t init_audio_system(void);
int32_t init_p4_ethernet(void);

// LVGL Thread Safety Wrappers
bool lvgl_lock(uint32_t timeout_ms);
void lvgl_unlock(void);

// UI & Camera Operations
void setup_split_screen_ui(void);
void update_camera_viewport(const p4_camera_frame_t *frame);

// Camera V4L2 Driver FFI
int32_t p4_camera_init_v4l2_default(void);
int32_t p4_camera_init_v4l2(uint16_t width, uint16_t height);
int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms);
int32_t p4_camera_release_frame(const p4_camera_frame_t *frame);
void camera_stream_task(void *pvParameters);

// Audio FFI
int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms);
int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms);

// OTA Update APIs
int32_t p4_perform_ota_update(const char *url);
void p4_mark_app_valid(void);

// ESP-DL MobileFaceNet FFI
int32_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size);
int32_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_H