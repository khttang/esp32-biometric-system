#ifndef BIOMETRICS_WRAPPER_H
#define BIOMETRICS_WRAPPER_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// Shared Hardware Structs (Must match Rust #[repr(C)] layouts)
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
} p4_camera_frame_t;

// -----------------------------------------------------------------------------
// I2C & Touch Peripherals
// -----------------------------------------------------------------------------
int init_i2c_bus(void);
int init_tca9554(void);
int init_gt911_device(void);
int gt911_i2c_read(uint16_t reg, uint8_t *data, size_t len);
int gt911_i2c_write(uint16_t reg, uint8_t val);

// -----------------------------------------------------------------------------
// Audio (I2S MEMS Mic & Speaker TX)
// -----------------------------------------------------------------------------
int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio);
int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms);
int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio);
int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms);

// -----------------------------------------------------------------------------
// Ethernet Network Interface
// -----------------------------------------------------------------------------
int32_t init_p4_ethernet(void);

// -----------------------------------------------------------------------------
// Display, Camera & Unified Hardware
// -----------------------------------------------------------------------------
int32_t p4_hardware_init_all(const p4_hardware_config_t *config);
int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms);
void p4_display_draw_test_pattern(void);

// -----------------------------------------------------------------------------
// GPIO, Power & OTA Operations
// -----------------------------------------------------------------------------
int init_admin_button_gpio(void);
bool is_admin_button_pressed(void);
int32_t p4_perform_ota_update(const char *url);
void p4_mark_app_valid(void);

// -----------------------------------------------------------------------------
// Neural Engine (ESP-DL MobileFaceNet)
// -----------------------------------------------------------------------------
int32_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size);
int32_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len);
int run_face_inference(const uint8_t *frame_rgb888, size_t frame_len, const uint8_t *model_bytes, FaceEmbedding *embedding_out);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_H