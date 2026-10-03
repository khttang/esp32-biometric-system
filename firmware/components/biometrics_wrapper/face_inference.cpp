// Face detection and embedding via Espressif's pretrained ESP-DL models.
//
//   human_face_detect      MSR + MNP two-stage detector -> boxes + 5 facial landmarks
//   human_face_recognition MFN (MobileFaceNet) feature model -> L2-normalised embedding
//
// The models are selected and embedded via Kconfig (see sdkconfig.defaults). This file only
// adapts their C++ API to the C API in biometrics_wrapper.h. Matching and enrollment live in
// Rust (crates/biometric-core); HumanFaceRecognizer's own database is intentionally not used.
//
// Threading: not thread-safe. All functions must be called from one thread (the Rust inference
// thread), which also owns the RGB888 image buffers passed in.

#include <cstring>
#include <new>
#include <vector>

#include "esp_err.h"
#include "esp_log.h"
#include "human_face_detect.hpp"
#include "human_face_recognition.hpp"

#include "biometrics_wrapper.h"

namespace {

constexpr const char *TAG = "p4_face";

HumanFaceDetect *s_detect = nullptr;
HumanFaceFeat *s_feat = nullptr;

// Reused across calls to avoid a heap allocation per embedding (single-threaded use only).
std::vector<int> s_landmarks(2 * P4_FACE_LANDMARKS);

dl::image::img_t make_ppa_rgb888_image(const uint8_t *data, uint16_t width, uint16_t height)
{
    dl::image::img_t img = {};
    // ESP-DL takes a non-const pointer but does not write to the input image.
    img.data = const_cast<uint8_t *>(data);
    img.width = width;
    img.height = height;
    // The PPA's "RGB888" is ESP-IDF's color_pixel_rgb888_data_t, stored B, G, R in memory
    // (hal/color_types.h; confirmed on the device by comparing PPA output with the RGB565 source).
    // ESP-DL's RGB888 means R, G, B in memory, so this buffer is BGR888 in ESP-DL terms.
    img.pix_type = dl::image::DL_IMAGE_PIX_TYPE_BGR888;
    return img;
}

} // namespace

extern "C" {

int32_t p4_face_init(void)
{
    if (s_detect && s_feat) {
        return ESP_OK;
    }
    // lazy_load = false: load and allocate both models now rather than on the first frame.
    s_detect = new (std::nothrow) HumanFaceDetect(
        static_cast<HumanFaceDetect::model_type_t>(CONFIG_DEFAULT_HUMAN_FACE_DETECT_MODEL), false);
    s_feat = new (std::nothrow)
        HumanFaceFeat(static_cast<HumanFaceFeat::model_type_t>(CONFIG_DEFAULT_HUMAN_FACE_FEAT_MODEL), false);
    if (!s_detect || !s_feat) {
        ESP_LOGE(TAG, "Failed to allocate face models");
        delete s_detect;
        delete s_feat;
        s_detect = nullptr;
        s_feat = nullptr;
        return ESP_ERR_NO_MEM;
    }
    ESP_LOGI(TAG, "Face models loaded (detect model %d, feat model %d, embedding length %d)",
             CONFIG_DEFAULT_HUMAN_FACE_DETECT_MODEL, CONFIG_DEFAULT_HUMAN_FACE_FEAT_MODEL,
             s_feat->get_feat_len());
    return ESP_OK;
}

size_t p4_face_embedding_len(void)
{
    if (!s_feat) {
        return 0;
    }
    const int len = s_feat->get_feat_len();
    return len > 0 ? static_cast<size_t>(len) : 0;
}

int32_t p4_face_detect(const uint8_t *rgb888, uint16_t width, uint16_t height, p4_face_t *faces,
                       size_t capacity, size_t *count)
{
    if (!s_detect) {
        return ESP_ERR_INVALID_STATE;
    }
    if (!rgb888 || !count || (capacity > 0 && !faces) || width == 0 || height == 0) {
        return ESP_ERR_INVALID_ARG;
    }

    const dl::image::img_t img = make_ppa_rgb888_image(rgb888, width, height);
    std::list<dl::detect::result_t> &results = s_detect->run(img);

    size_t n = 0;
    for (const dl::detect::result_t &r : results) {
        if (n == capacity) {
            break;
        }
        if (r.box.size() != 4) {
            continue;
        }
        p4_face_t &face = faces[n];
        face.x0 = r.box[0];
        face.y0 = r.box[1];
        face.x1 = r.box[2];
        face.y1 = r.box[3];
        face.score = r.score;
        face.has_landmarks = r.keypoint.size() == 2 * P4_FACE_LANDMARKS;
        for (size_t i = 0; i < 2 * P4_FACE_LANDMARKS; ++i) {
            face.landmarks[i] = face.has_landmarks ? r.keypoint[i] : 0;
        }
        ++n;
    }
    *count = n;
    return ESP_OK;
}

int32_t p4_face_embed(const uint8_t *rgb888, uint16_t width, uint16_t height, const p4_face_t *face,
                      float *embedding, size_t len)
{
    if (!s_feat) {
        return ESP_ERR_INVALID_STATE;
    }
    if (!rgb888 || !face || !embedding || !face->has_landmarks || width == 0 || height == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    if (len != p4_face_embedding_len()) {
        return ESP_ERR_INVALID_SIZE;
    }

    const dl::image::img_t img = make_ppa_rgb888_image(rgb888, width, height);
    for (size_t i = 0; i < 2 * P4_FACE_LANDMARKS; ++i) {
        s_landmarks[i] = face->landmarks[i];
    }

    // Aligns the face from the landmarks, runs the model and L2-normalises the output.
    dl::TensorBase *feat = s_feat->run(img, s_landmarks);
    if (!feat || !feat->data || feat->dtype != dl::DATA_TYPE_FLOAT) {
        ESP_LOGE(TAG, "Unexpected feature tensor");
        return ESP_FAIL;
    }
    std::memcpy(embedding, feat->data, len * sizeof(float));
    return ESP_OK;
}

} // extern "C"
