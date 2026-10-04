// Face detection and embedding with Espressif's pretrained ESP-DL models, loaded from flash
// partitions so they can be updated without reflashing the firmware.
//
//   face_msr_a/b   MSR proposal network        (human_face_detect_msr_s8_v1)
//   face_mnp_a/b   MNP refinement + landmarks  (human_face_detect_mnp_s8_v1)
//   face_feat_a/b  MobileFaceNet embedding     (human_face_feat_mfn_s8_v1)
//
// The Rust side chooses the slot of each model, verifies its manifest and SHA-256, and passes
// the partition label to the init functions here (firmware/src/models.rs); ESP-DL itself
// aborts on an unmappable partition and does not check integrity. Partition images are built
// by crates/model-packer.
//
// The model setup below (pre/post-processing parameters, the two-stage MSR -> MNP flow) is
// adapted from Espressif's human_face_detect 0.4.2 and human_face_recognition 0.3.2 components
// (MIT licence, https://github.com/espressif/esp-dl/tree/master/models). Those components load
// from fixed partition labels that only an ESP-IDF partition table can provide, which the
// esp-idf-sys build does not use; constructing the models here lets us choose the labels.
//
// Threading: not thread-safe. All functions must be called from one thread (the Rust inference
// thread), which also owns the image buffers passed in.

#include <cstring>
#include <list>
#include <new>
#include <vector>

#include "dl_detect_base.hpp"
#include "dl_detect_mnp_postprocessor.hpp"
#include "dl_detect_msr_postprocessor.hpp"
#include "dl_feat_base.hpp"
#include "dl_feat_image_preprocessor.hpp"
#include "dl_feat_postprocessor.hpp"
#include "dl_image_preprocessor.hpp"
#include "dl_model_base.hpp"
#include "esp_err.h"
#include "esp_log.h"
#include "mbedtls/sha256.h"

#include "biometrics_wrapper.h"

namespace {

constexpr const char *TAG = "p4_face";

// Detection thresholds (Espressif's defaults for MSR+MNP).
constexpr float MSR_SCORE_THR = 0.5f;
constexpr float MSR_NMS_THR = 0.5f;
constexpr float MNP_SCORE_THR = 0.5f;
constexpr float MNP_NMS_THR = 0.5f;

dl::Model *load_model(const char *partition)
{
    // A single model per partition; parameters are copied to RAM (param_copy defaults to true).
    dl::Model *model = new (std::nothrow) dl::Model(partition, fbs::MODEL_LOCATION_IN_FLASH_PARTITION);
    if (model) {
        model->minimize();
    }
    return model;
}

// Stage 1: proposes face candidates.
class Msr : public dl::detect::DetectImpl {
public:
    explicit Msr(dl::Model *model)
    {
        m_model = model;
        m_image_preprocessor = new dl::image::ImagePreprocessor(m_model, {0, 0, 0}, {1, 1, 1}, true);
        m_postprocessor = new dl::detect::MSRPostprocessor(
            m_model, m_image_preprocessor, MSR_SCORE_THR, MSR_NMS_THR, 10,
            {{8, 8, 9, 9, {{16, 16}, {32, 32}}}, {16, 16, 9, 9, {{64, 64}, {128, 128}}}});
    }
};

// Stage 2: refines each candidate and adds 5 landmarks.
class Mnp {
public:
    explicit Mnp(dl::Model *model) :
        m_model(model),
        m_image_preprocessor(new dl::image::ImagePreprocessor(m_model, {0, 0, 0}, {1, 1, 1}, true)),
        m_postprocessor(new dl::detect::MNPPostprocessor(m_model, m_image_preprocessor, MNP_SCORE_THR,
                                                         MNP_NMS_THR, 10, {{1, 1, 0, 0, {{48, 48}}}}))
    {
    }
    ~Mnp()
    {
        delete m_postprocessor;
        delete m_image_preprocessor;
        delete m_model;
    }
    Mnp(const Mnp &) = delete;
    Mnp &operator=(const Mnp &) = delete;

    std::list<dl::detect::result_t> &run(const dl::image::img_t &img, std::list<dl::detect::result_t> &candidates)
    {
        m_postprocessor->clear_result();
        for (dl::detect::result_t &candidate : candidates) {
            // Square crop around each candidate, as MNP was trained on square inputs.
            const int center_x = (candidate.box[0] + candidate.box[2]) >> 1;
            const int center_y = (candidate.box[1] + candidate.box[3]) >> 1;
            const int side = DL_MAX(candidate.box[2] - candidate.box[0], candidate.box[3] - candidate.box[1]);
            candidate.box[0] = center_x - (side >> 1);
            candidate.box[1] = center_y - (side >> 1);
            candidate.box[2] = candidate.box[0] + side;
            candidate.box[3] = candidate.box[1] + side;
            candidate.limit_box(img.width, img.height);
            m_image_preprocessor->preprocess(img, candidate.box);
            m_model->run();
            m_postprocessor->postprocess();
        }
        m_postprocessor->nms();
        return m_postprocessor->get_result(img.width, img.height);
    }

private:
    dl::Model *m_model;
    dl::image::ImagePreprocessor *m_image_preprocessor;
    dl::detect::MNPPostprocessor *m_postprocessor;
};

// Feature model: aligns the face from its landmarks and computes an L2-normalised embedding.
// Espressif's MobileFaceNet (MFN) and its larger MBF model share this pre- and post-processing.
class Mfn : public dl::feat::FeatImpl {
public:
    explicit Mfn(dl::Model *model)
    {
        m_model = model;
        m_image_preprocessor =
            new dl::image::FeatImagePreprocessor(m_model, {127.5, 127.5, 127.5}, {127.5, 127.5, 127.5}, true);
        m_postprocessor = new dl::feat::FeatPostprocessor(m_model);
    }
};

Msr *s_msr = nullptr;
Mnp *s_mnp = nullptr;
Mfn *s_feat = nullptr;
// A second feature model run next to the active one, e.g. to compare the two.
Mfn *s_candidate = nullptr;

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

size_t feat_len(Mfn *feat)
{
    const int len = feat ? feat->get_feat_len() : 0;
    return len > 0 ? static_cast<size_t>(len) : 0;
}

int32_t init_feat(Mfn *&slot, const char *partition, const char *what)
{
    if (slot) {
        return ESP_OK;
    }
    if (!partition) {
        return ESP_ERR_INVALID_ARG;
    }
    dl::Model *model = load_model(partition);
    Mfn *feat = model ? new (std::nothrow) Mfn(model) : nullptr;
    if (!feat) {
        delete model;
        ESP_LOGE(TAG, "Failed to allocate the %s", what);
        return ESP_ERR_NO_MEM;
    }
    slot = feat;
    ESP_LOGI(TAG, "%s loaded from %s (embedding length %d)", what, partition, slot->get_feat_len());
    return ESP_OK;
}

int32_t embed_with(Mfn *feat_model, const uint8_t *rgb888, uint16_t width, uint16_t height, const p4_face_t *face,
                   float *embedding, size_t len)
{
    if (!feat_model) {
        return ESP_ERR_INVALID_STATE;
    }
    if (!rgb888 || !face || !embedding || !face->has_landmarks || width == 0 || height == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    if (len != feat_len(feat_model)) {
        return ESP_ERR_INVALID_SIZE;
    }

    const dl::image::img_t img = make_ppa_rgb888_image(rgb888, width, height);
    for (size_t i = 0; i < 2 * P4_FACE_LANDMARKS; ++i) {
        s_landmarks[i] = face->landmarks[i];
    }

    // Aligns the face from the landmarks, runs the model and L2-normalises the output.
    dl::TensorBase *feat = feat_model->run(img, s_landmarks);
    if (!feat || !feat->data || feat->dtype != dl::DATA_TYPE_FLOAT) {
        ESP_LOGE(TAG, "Unexpected feature tensor");
        return ESP_FAIL;
    }
    std::memcpy(embedding, feat->data, len * sizeof(float));
    return ESP_OK;
}

} // namespace

extern "C" {

int32_t p4_model_golden(const char *partition, uint8_t digest[32])
{
    if (!partition || !digest) {
        return ESP_ERR_INVALID_ARG;
    }
    dl::Model *model = new (std::nothrow) dl::Model(partition, fbs::MODEL_LOCATION_IN_FLASH_PARTITION);
    if (!model) {
        return ESP_ERR_NO_MEM;
    }
    // ESP-DL does not fail on data that is not a model; it just ends up with an empty graph.
    if (model->get_inputs().empty() || model->get_outputs().empty()) {
        ESP_LOGE(TAG, "%s does not hold a runnable model", partition);
        delete model;
        return ESP_ERR_INVALID_STATE;
    }
    // Fixed pseudo-random input (xorshift32), written as raw bytes whatever the tensor's type.
    uint32_t state = 0x9E3779B9u;
    for (auto &[name, tensor] : model->get_inputs()) {
        auto *bytes = static_cast<uint8_t *>(tensor->data);
        for (int i = 0, n = tensor->get_bytes(); i < n; ++i) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            bytes[i] = static_cast<uint8_t>(state);
        }
    }
    model->run();

    // Outputs are quantised integers, so the result is exact; std::map iterates in name order.
    mbedtls_sha256_context sha;
    mbedtls_sha256_init(&sha);
    mbedtls_sha256_starts(&sha, 0);
    for (auto &[name, tensor] : model->get_outputs()) {
        mbedtls_sha256_update(&sha, reinterpret_cast<const uint8_t *>(name.data()), name.size());
        mbedtls_sha256_update(&sha, static_cast<const uint8_t *>(tensor->data), tensor->get_bytes());
    }
    mbedtls_sha256_finish(&sha, digest);
    mbedtls_sha256_free(&sha);
    delete model;
    return ESP_OK;
}

int32_t p4_face_init_detector(const char *msr_partition, const char *mnp_partition)
{
    if (s_msr && s_mnp) {
        return ESP_OK;
    }
    if (!msr_partition || !mnp_partition) {
        return ESP_ERR_INVALID_ARG;
    }
    dl::Model *msr_model = load_model(msr_partition);
    dl::Model *mnp_model = load_model(mnp_partition);
    Msr *msr = msr_model ? new (std::nothrow) Msr(msr_model) : nullptr;
    Mnp *mnp = mnp_model ? new (std::nothrow) Mnp(mnp_model) : nullptr;
    if (!msr || !mnp) {
        ESP_LOGE(TAG, "Failed to allocate the face detector");
        // Wrappers own their model once constructed; free whatever is left unowned.
        if (msr) {
            delete msr;
        } else {
            delete msr_model;
        }
        if (mnp) {
            delete mnp;
        } else {
            delete mnp_model;
        }
        return ESP_ERR_NO_MEM;
    }
    s_msr = msr;
    s_mnp = mnp;
    ESP_LOGI(TAG, "Face detector loaded from %s + %s", msr_partition, mnp_partition);
    return ESP_OK;
}

int32_t p4_face_init_embedder(const char *partition)
{
    return init_feat(s_feat, partition, "Face embedder");
}

int32_t p4_face_init_candidate_embedder(const char *partition)
{
    return init_feat(s_candidate, partition, "Candidate face embedder");
}

size_t p4_face_embedding_len(void)
{
    return feat_len(s_feat);
}

size_t p4_face_candidate_embedding_len(void)
{
    return feat_len(s_candidate);
}

int32_t p4_face_detect(const uint8_t *rgb888, uint16_t width, uint16_t height, p4_face_t *faces,
                       size_t capacity, size_t *count)
{
    if (!s_msr || !s_mnp) {
        return ESP_ERR_INVALID_STATE;
    }
    if (!rgb888 || !count || (capacity > 0 && !faces) || width == 0 || height == 0) {
        return ESP_ERR_INVALID_ARG;
    }

    const dl::image::img_t img = make_ppa_rgb888_image(rgb888, width, height);
    std::list<dl::detect::result_t> &candidates = s_msr->run(img);
    std::list<dl::detect::result_t> &results = s_mnp->run(img, candidates);

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
    return embed_with(s_feat, rgb888, width, height, face, embedding, len);
}

int32_t p4_face_embed_candidate(const uint8_t *rgb888, uint16_t width, uint16_t height,
                                const p4_face_t *face, float *embedding, size_t len)
{
    return embed_with(s_candidate, rgb888, width, height, face, embedding, len);
}

} // extern "C"
