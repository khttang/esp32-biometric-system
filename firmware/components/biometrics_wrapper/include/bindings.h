#ifndef BIOMETRICS_WRAPPER_BINDINGS_H
#define BIOMETRICS_WRAPPER_BINDINGS_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    float values[512];
} FaceEmbedding;

int init_i2c_bus(void);
int init_tca9554(void);

int init_gt911_device(void);
int gt911_i2c_read(uint16_t reg, uint8_t *data, size_t len);
int gt911_i2c_write(uint16_t reg, uint8_t val);

// esp_err_t in ESP-IDF is typedef int esp_err_t (0 = ESP_OK)
int32_t init_p4_ethernet(void);

// Initialize I2S RX for INMP441 Microphone on ESP32-P4
int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio);

// Read raw PCM samples into buffer (Mono, 16-bit)
int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms);

// TX (Playback) Functions
int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio);
int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms);

// Networking / ESP-Hosted Wi-Fi Functions
int init_wifi_hosted_c(const char *ssid, const char *password, uint32_t timeout_ms);
bool is_wifi_connected_c(void);
int stop_wifi_hosted_c(void);

int32_t init_mipi_csi_camera_c(void);
int32_t init_mipi_dsi_display_c(uint16_t width, uint16_t height);

int run_face_inference(
    const uint8_t* frame_rgb888, 
    size_t frame_len, 
    const uint8_t* model_bytes, 
    FaceEmbedding* embedding_out
);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_BINDINGS_H