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

// System Initialization APIs
int32_t p4_hardware_init_all(const p4_hardware_config_t *config);
int32_t init_display_system(void);
int32_t init_audio_system(void);
int32_t init_p4_ethernet(void);

// UI & Camera Operations
void setup_split_screen_ui(void);
// Shows `buf` (VIEWPORT_WIDTH x VIEWPORT_HEIGHT RGB565, caller-owned, must stay valid until the
// next call) in the camera canvas. NULL restores the internal buffer. lock_timeout_ms 0 = wait forever.
// Returns false (nothing changed) if the LVGL lock could not be taken in time.
bool p4_ui_present_camera(const void *buf, uint32_t lock_timeout_ms);

// Face overlay boxes drawn on top of the camera canvas.
#define P4_UI_MAX_FACE_BOXES 4

typedef struct {
    int16_t x;
    int16_t y;
    int16_t w;
    int16_t h;
} p4_ui_rect_t;

// Shows `count` boxes (canvas coordinates; at most P4_UI_MAX_FACE_BOXES are drawn) and hides the
// rest. count = 0 hides all. Returns false (nothing changed) if the LVGL lock was not taken in time.
bool p4_ui_show_faces(const p4_ui_rect_t *rects, size_t count, uint32_t lock_timeout_ms);
bool p4_touch_is_pressed(void);

// Camera V4L2 Driver FFI
int32_t p4_camera_init_v4l2(uint16_t width, uint16_t height);
int32_t p4_camera_capture_frame(p4_camera_frame_t *frame); // blocks until a frame is ready
int32_t p4_camera_release_frame(const p4_camera_frame_t *frame);

// Audio FFI
int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms);
int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms);

// OTA Update APIs
int32_t p4_perform_ota_update(const char *url);
void p4_mark_app_valid(void);

// Face inference (face_inference.cpp): ESP-DL human_face_detect + human_face_recognition.
// Not thread-safe: call all p4_face_* functions from a single thread.
#define P4_FACE_LANDMARKS 5

typedef struct {
    int32_t x0; // box corners in image pixels, inclusive
    int32_t y0;
    int32_t x1;
    int32_t y1;
    float score;
    bool has_landmarks;
    int32_t landmarks[2 * P4_FACE_LANDMARKS]; // (x, y) pairs as reported by the detector
} p4_face_t;

// Load the detection / feature model from its flash partition (human_face_det / human_face_feat).
// Idempotent. ESP-DL aborts on an unreadable partition, so verify it (Rust models.rs) first.
int32_t p4_face_init_detector(void);
int32_t p4_face_init_embedder(void);
// Embedding length of the loaded feature model (0 before p4_face_init_embedder).
size_t p4_face_embedding_len(void);
// Images are packed PPA RGB888 (ESP-IDF layout: B, G, R bytes per pixel).
// Detects faces in a packed RGB888 image. Writes up to `capacity` faces (highest score first) and
// stores the number written in `*count`.
int32_t p4_face_detect(const uint8_t *rgb888, uint16_t width, uint16_t height, p4_face_t *faces,
                       size_t capacity, size_t *count);
// Aligns `face` (requires landmarks) and writes its L2-normalised embedding; `len` must equal
// p4_face_embedding_len().
int32_t p4_face_embed(const uint8_t *rgb888, uint16_t width, uint16_t height, const p4_face_t *face,
                      float *embedding, size_t len);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_H