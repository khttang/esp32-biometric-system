#include <stdio.h>
#include <string.h>
#include <math.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <linux/videodev2.h>

#include "driver/i2c_master.h"
#include "driver/gpio.h"
#include "driver/i2s_std.h"
#include "driver/ppa.h"

#include "esp_video_init.h"
#include "esp_video_ioctl.h"
#include "esp_log.h"
#include "esp_err.h"
#include "esp_check.h"
#include "esp_cache.h"
#include "esp_attr.h"
#include "esp_timer.h"

#include "esp_cam_sensor_detect.h"
#include "esp_lcd_mipi_dsi.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_panel_io.h"
#include "esp_heap_caps.h"
#include "esp_eth.h"
#include "esp_eth_mac_esp.h"
#include "esp_eth_phy_802_3.h"
#include "esp_ota_ops.h"
#include "esp_https_ota.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/semphr.h"

// Waveshare ESP32-P4-NANO BSP & Touch Headers
#include "esp_lcd_touch.h"
#include "esp_lcd_touch_gt911.h"
#include "bsp/esp32_p4_nano.h"
#include "bsp/display.h"
#include "bsp/touch.h"
#include "lvgl.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include "sdkconfig.h"
#include "biometrics_wrapper.h"

#ifndef CONFIG_BIOMETRICS_CAM_WIDTH
#define CONFIG_BIOMETRICS_CAM_WIDTH 1280
#endif

#ifndef CONFIG_BIOMETRICS_CAM_HEIGHT
#define CONFIG_BIOMETRICS_CAM_HEIGHT 720
#endif

struct v4l2_frame_buffer_t {
    void  *start;
    size_t length;
};

// -----------------------------------------------------------------------------
// Hardware Pinout Definitions
// -----------------------------------------------------------------------------
namespace BoardPins {
    namespace Ethernet {
        constexpr gpio_num_t MDC   = GPIO_NUM_31;
        constexpr gpio_num_t MDIO  = GPIO_NUM_52;
        constexpr gpio_num_t CLK   = GPIO_NUM_50;
        constexpr gpio_num_t RESET = GPIO_NUM_53;
        
        namespace RMII {
            constexpr gpio_num_t TX_EN  = GPIO_NUM_49;
            constexpr gpio_num_t TXD0   = GPIO_NUM_34;
            constexpr gpio_num_t TXD1   = GPIO_NUM_35;
            constexpr gpio_num_t CRS_DV = GPIO_NUM_28;
            constexpr gpio_num_t RXD0   = GPIO_NUM_29;
            constexpr gpio_num_t RXD1   = GPIO_NUM_30;
        }
    }
    namespace Camera {
        constexpr gpio_num_t SDA      = GPIO_NUM_7;  // Shared I2C0 SDA (Initialized by BSP)
        constexpr gpio_num_t SCL      = GPIO_NUM_8;  // Shared I2C0 SCL (Initialized by BSP)
        constexpr gpio_num_t PWDN     = GPIO_NUM_5;  // Camera Power Down Pin (Active LOW)
        constexpr gpio_num_t RESET    = GPIO_NUM_6;  // Camera Reset Pin (Active LOW)
        constexpr i2c_port_num_t PORT = I2C_NUM_0;
    }
    namespace System {
        constexpr gpio_num_t ADMIN_BTN = GPIO_NUM_0;
    }
    namespace Audio {
        constexpr gpio_num_t BCLK      = GPIO_NUM_12;
        constexpr gpio_num_t WS        = GPIO_NUM_13;
        constexpr gpio_num_t DIN       = GPIO_NUM_11;
        constexpr gpio_num_t DOUT      = GPIO_NUM_14;
    }
}

#define TAG_HW      "p4_hardware"
#define TAG_CAM     "p4_camera"
#define TAG_ETH     "p4_ethernet"
#define TAG_TOUCH   "p4_touch"
#define TAG_LVGL    "p4_lvgl"
#define TAG_AUDIO   "p4_audio"
#define TAG_OTA     "p4_ota"
#define TAG_FACENET "ESP_DL_FACENET"
#define TAG_I2S     "I2S_WRAPPER"

#define CAM_BUF_COUNT 2
#define C_LINE_SIZE 128  // ESP32-P4 L2 Cache Line Size (0x80)

// Global Subsystem Handles
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;
static esp_lcd_touch_handle_t s_touch_handle = NULL;

static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;

static ppa_client_handle_t s_ppa_client = NULL;

static bool s_hardware_initialized = false;
static dl::Model *g_mobilefacenet_model = NULL;

static int s_video_fd = -1;
struct v4l2_frame_buffer_t s_cam_buffers[CAM_BUF_COUNT] = {};
static size_t s_cam_buf_lengths[CAM_BUF_COUNT] = {0};

static SemaphoreHandle_t s_lvgl_mutex = NULL;

// LVGL Camera Image Descriptor Handles
static lv_obj_t *s_camera_img = NULL;
static lv_image_dsc_t s_camera_dsc = {};
static lv_obj_t *s_status_label = NULL;

static uint16_t *s_ppa_buf[2] = {NULL, NULL};
static uint8_t s_ppa_idx = 0;

extern "C" {
    i2c_master_bus_handle_t bsp_i2c_get_handle(void);
    esp_err_t bsp_i2c_init(void);
}

// -----------------------------------------------------------------------------
// LVGL 9 Callbacks & Task Loop
// -----------------------------------------------------------------------------

// Asynchronous MIPI-DSI DMA completion callback
static bool on_color_trans_done(esp_lcd_panel_handle_t panel, esp_lcd_dpi_panel_event_data_t *edata, void *user_ctx) {
    lv_display_t *disp = (lv_display_t *)user_ctx;
    if (disp) {
        lv_display_flush_ready(disp);
    }
    return false;
}

// Display Flush Callback (initiates async DSI DMA transfer)
static void lvgl_display_flush_cb(lv_display_t *disp, const lv_area_t *area, uint8_t *px_map) {
    esp_lcd_panel_handle_t panel = (esp_lcd_panel_handle_t)lv_display_get_user_data(disp);

    int x1 = area->x1;
    int x2 = area->x2;
    int y1 = area->y1;
    int y2 = area->y2;

    // Draw directly to translated physical coordinates
    esp_lcd_panel_draw_bitmap(panel, x1, y1, x2 + 1, y2 + 1, px_map);
}

// Touch Input Read Callback (reads GT911 coordinates)
void lvgl_touch_read_cb(lv_indev_t *indev, lv_indev_data_t *data) {
    esp_lcd_touch_handle_t touch_handle = (esp_lcd_touch_handle_t)lv_indev_get_user_data(indev);
    uint16_t touch_x[1], touch_y[1];
    uint8_t touch_cnt = 0;

    esp_lcd_touch_read_data(touch_handle);
    bool pressed = esp_lcd_touch_get_coordinates(touch_handle, touch_x, touch_y, NULL, &touch_cnt, 1);

    if (pressed && touch_cnt > 0) {
        data->state = LV_INDEV_STATE_PRESSED;
        // Map portrait hardware touch (720x1280) to landscape canvas (1280x720)
        data->point.x = touch_y[0];
        data->point.y = 719 - touch_x[0];
    } else {
        data->state = LV_INDEV_STATE_RELEASED;
    }
}

// Background FreeRTOS task handling LVGL timer ticks
static void lvgl_port_task(void *arg) {
    while (1) {
        if (xSemaphoreTake(s_lvgl_mutex, pdMS_TO_TICKS(20)) == pdTRUE) {
            uint32_t task_delay = lv_timer_handler();
            xSemaphoreGive(s_lvgl_mutex);
            
            // Sleep for the dynamic time requested by LVGL (capped between 1ms and 30ms)
            vTaskDelay(pdMS_TO_TICKS(task_delay > 0 ? (task_delay > 30 ? 30 : task_delay) : 1));
        } else {
            vTaskDelay(pdMS_TO_TICKS(1));
        }
    }
}

// Helper to apply V4L2 controls
static void set_v4l2_control(int fd, uint32_t id, int32_t value, const char *name) {
    struct v4l2_control ctrl = {};
    ctrl.id = id;
    ctrl.value = value;
    if (ioctl(fd, VIDIOC_S_CTRL, &ctrl) < 0) {
        ESP_LOGW(TAG_CAM, "Failed to set V4L2 ctrl %s (0x%08" PRIx32 "): errno %d (%s)", 
                 name, id, errno, strerror(errno));
    } else {
        ESP_LOGI(TAG_CAM, "V4L2 Ctrl %s set to %" PRId32, name, value);
    }
}

esp_err_t init_ppa_hardware_engine(void) {
    ppa_client_config_t ppa_cfg = {};
    ppa_cfg.oper_type = PPA_OPERATION_SRM; // Scaling, Rotation, Mirroring Engine
    
    esp_err_t err = ppa_register_client(&ppa_cfg, &s_ppa_client);
    if (err == ESP_OK) {
        ESP_LOGI("PPA_SYS", "ESP32-P4 PPA Hardware Accelerator Initialized Successfully.");
    }
    return err;
}

void configure_camera_exposure_gain(int fd) {
    set_v4l2_control(fd, V4L2_CID_EXPOSURE_AUTO, V4L2_EXPOSURE_AUTO, "EXPOSURE_AUTO");
    set_v4l2_control(fd, V4L2_CID_AUTOGAIN, 1, "AUTOGAIN");
    set_v4l2_control(fd, V4L2_CID_AUTO_WHITE_BALANCE, 1, "AUTO_WHITE_BALANCE");
}

// -----------------------------------------------------------------------------
// Ethernet Event Handlers
// -----------------------------------------------------------------------------
static void eth_event_handler(void *arg, esp_event_base_t event_base,
                              int32_t event_id, void *event_data) {
    uint8_t mac_addr[6] = {0};
    esp_eth_handle_t eth_handle = *(esp_eth_handle_t *)event_data;

    switch (event_id) {
    case ETHERNET_EVENT_CONNECTED:
        esp_eth_ioctl(eth_handle, ETH_CMD_G_MAC_ADDR, mac_addr);
        ESP_LOGI(TAG_ETH, "Ethernet Link Up. MAC: %02x:%02x:%02x:%02x:%02x:%02x",
                 mac_addr[0], mac_addr[1], mac_addr[2],
                 mac_addr[3], mac_addr[4], mac_addr[5]);
        break;
    case ETHERNET_EVENT_DISCONNECTED:
        ESP_LOGI(TAG_ETH, "Ethernet Link Down");
        break;
    case ETHERNET_EVENT_START:
        ESP_LOGI(TAG_ETH, "Ethernet Driver Started");
        break;
    case ETHERNET_EVENT_STOP:
        ESP_LOGI(TAG_ETH, "Ethernet Driver Stopped");
        break;
    default:
        break;
    }
}

// -----------------------------------------------------------------------------
// Audio Peripheral Drivers
// -----------------------------------------------------------------------------

/*
* Initialize I2S TX and RX channels together.
* Outcome:
*  - TX channel (g_i2s_tx_handle): I2S_TX, 16-bit data, left-justified, mono, no DMA, no clock divider.
*  - RX channel (g_i2s_rx_handle): I2S_RX, 16-bit data, left-justified, mono, no DMA, no clock divider.
*/
int init_i2s_duplex_c(uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio, int dout_gpio) {
    // 1. Clean up existing channels if re-initialized
    if (g_i2s_tx_handle) {
        i2s_channel_disable(g_i2s_tx_handle);
        i2s_del_channel(g_i2s_tx_handle);
        g_i2s_tx_handle = NULL;
    }
    if (g_i2s_rx_handle) {
        i2s_channel_disable(g_i2s_rx_handle);
        i2s_del_channel(g_i2s_rx_handle);
        g_i2s_rx_handle = NULL;
    }

    // 2. Allocate full-duplex channel pair on I2S_NUM_0
    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_0, I2S_ROLE_MASTER);
    esp_err_t ret = i2s_new_channel(&chan_cfg, &g_i2s_tx_handle, &g_i2s_rx_handle);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_AUDIO, "Failed to allocate I2S channels: 0x%x", ret);
        return (int)ret;
    }

    // 3. Configure TX Channel (Drives Speaker DOUT & Physical Clocks BCLK/WS)
    i2s_std_config_t tx_cfg = {
        .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
        .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
        .gpio_cfg = {
            .mclk = I2S_GPIO_UNUSED,
            .bclk = (gpio_num_t)bclk_gpio,
            .ws   = (gpio_num_t)ws_gpio,
            .dout = (gpio_num_t)dout_gpio,
            .din  = I2S_GPIO_UNUSED,
        },
    };
    ret = i2s_channel_init_std_mode(g_i2s_tx_handle, &tx_cfg);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_AUDIO, "Failed to init I2S TX channel: 0x%x", ret);
        return (int)ret;
    }

    // 4. Configure RX Channel (Reads Microphone DIN & Shares Clock Internal Lines)
    i2s_std_config_t rx_cfg = {
        .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
        .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
        .gpio_cfg = {
            .mclk = I2S_GPIO_UNUSED,
            .bclk = I2S_GPIO_UNUSED, // Uses internal clock routed from TX
            .ws   = I2S_GPIO_UNUSED, // Uses internal clock routed from TX
            .dout = I2S_GPIO_UNUSED,
            .din  = (gpio_num_t)din_gpio,
        },
    };
    ret = i2s_channel_init_std_mode(g_i2s_rx_handle, &rx_cfg);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_AUDIO, "Failed to init I2S RX channel: 0x%x", ret);
        return (int)ret;
    }

    // 5. Enable both channels
    ret = i2s_channel_enable(g_i2s_tx_handle);
    if (ret != ESP_OK) return (int)ret;

    ret = i2s_channel_enable(g_i2s_rx_handle);
    if (ret != ESP_OK) return (int)ret;

    ESP_LOGI(TAG_AUDIO, "I2S_NUM_0 Duplex audio initialized successfully (%u Hz)", sample_rate);
    return 0;
}

int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms) {
    if (!g_i2s_rx_handle) return -1;
    size_t r_bytes = 0;
    esp_err_t ret = i2s_channel_read(g_i2s_rx_handle, out_buffer, samples_to_read * sizeof(int16_t), &r_bytes, pdMS_TO_TICKS(timeout_ms));
    if (bytes_read) *bytes_read = (uint32_t)r_bytes;
    return (int)ret;
}

int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms) {
    if (!g_i2s_tx_handle) return -1;
    size_t w_bytes = 0;
    esp_err_t ret = i2s_channel_write(g_i2s_tx_handle, buffer, sample_count * sizeof(int16_t), &w_bytes, pdMS_TO_TICKS(timeout_ms));
    return (int)ret;
}

// -----------------------------------------------------------------------------
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

esp_cam_sensor_device_t *ov5647_detect(void *config);

// Thread-safe LVGL Mutex Helpers
bool lvgl_lock(uint32_t timeout_ms) {
    if (!s_lvgl_mutex) return false;
    return xSemaphoreTake(s_lvgl_mutex, pdMS_TO_TICKS(timeout_ms)) == pdTRUE;
}

void lvgl_unlock(void) {
    if (s_lvgl_mutex) {
        xSemaphoreGive(s_lvgl_mutex);
    }
}

void camera_stream_task(void *pvParameters) {
    //(void)pvParameters;
    p4_camera_frame_t frame = {};
    while (1) {
        // Dequeue frame captured by MIPI-CSI ISP DMA
        if (p4_camera_capture_frame(&frame, 100) == 0) {
            update_camera_viewport(&frame);
            p4_camera_release_frame(&frame);
        }
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}

int32_t init_audio_system(void) {
    const uint32_t SAMPLE_RATE = 16000U;
    return init_i2s_duplex_c(SAMPLE_RATE, BoardPins::Audio::BCLK, BoardPins::Audio::WS, BoardPins::Audio::DIN, BoardPins::Audio::DOUT);
}

// Unified Display, Touch & LVGL 9 System Initialization
int32_t init_display_system(void) {
    ESP_LOGI(TAG_LVGL, "Initializing Display Hardware (1280x720 Landscape)...");

    s_lvgl_mutex = xSemaphoreCreateMutex();

    // 0. Initialize PPA Hardware Accelerator Client FIRST
    if (init_ppa_hardware_engine() != ESP_OK) {
        ESP_LOGE(TAG_LVGL, "Failed to initialize PPA hardware client!");
        return ESP_FAIL;
    }

    // 1. Acquire raw LCD panel handles from BSP
    bsp_lcd_handles_t handles = {};
    esp_err_t err = bsp_display_new_with_handles(NULL, &handles);
    if (err != ESP_OK) return err;

    s_lcd_panel = handles.panel;
    s_lcd_io = handles.io;

    esp_lcd_panel_disp_on_off(handles.panel, true);

    // 2. Acquire GT911 Touch handle from BSP
    esp_lcd_touch_handle_t touch_handle = NULL;
    if (bsp_touch_new(NULL, &touch_handle) == ESP_OK) {
        s_touch_handle = touch_handle;
    }

    // 3. Initialize LVGL 9
    lv_init();

    lv_tick_set_cb([]() -> uint32_t {
        return (uint32_t)(esp_timer_get_time() / 1000);
    });

    // 4. Create Display (720x1280 physical panel rotated to 1280x720)
    lv_display_t *disp = lv_display_create(720, 1280);
    lv_display_set_user_data(disp, handles.panel);
    
    // Rotate 720x1280 physical panel into 1280x720 Landscape
    lv_display_set_rotation(disp, LV_DISPLAY_ROTATION_90);

    // 5. Allocate draw buffers in PSRAM to avoid internal SRAM fragmentation
    size_t buf_size = 1280 * 20 * sizeof(uint16_t); // 51.2 KB per buffer

    void *buf1 = heap_caps_malloc(buf_size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    void *buf2 = heap_caps_malloc(buf_size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);

    if (!buf1 || !buf2) {
        ESP_LOGE(TAG_LVGL, "Failed to allocate PSRAM draw buffers!");
        return ESP_ERR_NO_MEM;
    }

    lv_display_set_buffers(disp, buf1, buf2, buf_size, LV_DISPLAY_RENDER_MODE_PARTIAL);
    lv_display_set_flush_cb(disp, lvgl_display_flush_cb);

    // Register async DSI DMA callback
    esp_lcd_dpi_panel_event_callbacks_t dsi_cbs = {};
    dsi_cbs.on_color_trans_done = on_color_trans_done;
    esp_lcd_dpi_panel_register_event_callbacks(handles.panel, &dsi_cbs, disp);

    // 6. Touch Input Device
    lv_indev_t *indev = lv_indev_create();
    lv_indev_set_type(indev, LV_INDEV_TYPE_POINTER);
    lv_indev_set_user_data(indev, touch_handle);
    lv_indev_set_read_cb(indev, lvgl_touch_read_cb);

    // 7. Start LVGL Task
    xTaskCreatePinnedToCore(lvgl_port_task, "lvgl_task", 8192, NULL, 5, NULL, 0);

    // 8. Construct 1280x720 UI Layout
    setup_split_screen_ui();

    // 9. Spawn camera streaming worker thread on Core 1
    xTaskCreatePinnedToCore(
        camera_stream_task,  // Task function entry point
        "camera_task",       // Task name
        8192,                // Stack size in bytes (V4L2 driver needs ~8 KB)
        NULL,                // Task input parameter
        5,                   // Priority (matching or higher than main worker loop)
        NULL,                // Task handle reference
        1                    // Core ID (Core 1 isolates V4L2 from LVGL on Core 0)
    );

    return 0;
}

int32_t p4_camera_init_v4l2_default(void) {
    return p4_camera_init_v4l2(CONFIG_BIOMETRICS_CAM_WIDTH, CONFIG_BIOMETRICS_CAM_HEIGHT);
}

int32_t p4_camera_init_v4l2(uint16_t width, uint16_t height) {
    if (s_video_fd >= 0) return 0;

    ESP_LOGI(TAG_CAM, "Initializing OV5647 via esp_video at %dx%d...", width, height);

    i2c_master_bus_handle_t shared_i2c_bus = bsp_i2c_get_handle();
    if (!shared_i2c_bus) {
        ESP_LOGE(TAG_CAM, "bsp_i2c_get_handle() returned NULL!");
        return -1;
    }

    esp_video_init_csi_config_t csi_cfg = {};
    csi_cfg.sccb_config.init_sccb = false;
    csi_cfg.sccb_config.i2c_handle = shared_i2c_bus;
    csi_cfg.sccb_config.freq = 100000;
    csi_cfg.reset_pin = static_cast<gpio_num_t>(BoardPins::Camera::RESET);
    csi_cfg.pwdn_pin  = static_cast<gpio_num_t>(BoardPins::Camera::PWDN);
    csi_cfg.dont_init_ldo = false;

    esp_video_init_config_t cam_cfg = {};
    cam_cfg.csi = &csi_cfg;

    esp_err_t ret = esp_video_init(&cam_cfg);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_CAM, "esp_video_init failed: 0x%x", ret);
        return ret;
    }

    s_video_fd = open("/dev/video0", O_RDWR | O_NONBLOCK);
    if (s_video_fd < 0) {
        ESP_LOGE(TAG_CAM, "Failed to open /dev/video0");
        return -1;
    }

    // Set target resolution (1280x720) explicitly
    struct v4l2_format fmt = {};
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    fmt.fmt.pix.width = width;
    fmt.fmt.pix.height = height;
    fmt.fmt.pix.pixelformat = V4L2_PIX_FMT_RGB565;

    if (ioctl(s_video_fd, VIDIOC_S_FMT, &fmt) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to set RGB565 %dx%d format: errno %d (%s)", 
                 width, height, errno, strerror(errno));
        close(s_video_fd);
        s_video_fd = -1;
        return -1;
    }

    // Request MMAP Buffers
    struct v4l2_requestbuffers req = {};
    req.count = CAM_BUF_COUNT;
    req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    req.memory = V4L2_MEMORY_MMAP;
    if (ioctl(s_video_fd, VIDIOC_REQBUFS, &req) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to request V4L2 buffers");
        return -1;
    }

    for (int i = 0; i < CAM_BUF_COUNT; i++) {
        struct v4l2_buffer buf = {};
        buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = V4L2_MEMORY_MMAP;
        buf.index = i;

        if (ioctl(s_video_fd, VIDIOC_QUERYBUF, &buf) < 0) return -1;

        s_cam_buf_lengths[i] = buf.length;
        s_cam_buffers[i].start = mmap(NULL, buf.length, PROT_READ | PROT_WRITE, MAP_SHARED, s_video_fd, buf.m.offset);
        if (s_cam_buffers[i].start == MAP_FAILED) return -1;
        if (ioctl(s_video_fd, VIDIOC_QBUF, &buf) < 0) return -1;
    }

    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(s_video_fd, VIDIOC_STREAMON, &type) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to start V4L2 stream");
        return -1;
    }

    ESP_LOGI(TAG_CAM, "OV5647 Camera streaming successfully on /dev/video0 (%dx%d RGB565)!", 
             fmt.fmt.pix.width, fmt.fmt.pix.height);
    return 0;
}

int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms) {
    if (s_video_fd < 0 || !frame) return -1;

    struct v4l2_buffer buf = {};
    buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = V4L2_MEMORY_MMAP;

    int ret = ioctl(s_video_fd, VIDIOC_DQBUF, &buf);
    if (ret < 0) {
        return (errno == EAGAIN || errno == EWOULDBLOCK) ? -2 : -1;
    }

    frame->data = (uint8_t *)s_cam_buffers[buf.index].start;
    frame->data_len = buf.bytesused;
    frame->width = CONFIG_BIOMETRICS_CAM_WIDTH;   // 1280
    frame->height = CONFIG_BIOMETRICS_CAM_HEIGHT; // 720
    frame->buffer_index = buf.index;
    return 0;
}

int32_t p4_camera_release_frame(const p4_camera_frame_t *frame) {
    if (s_video_fd < 0 || !frame) {
        return -1;
    }

    struct v4l2_buffer buf = {};
    buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = V4L2_MEMORY_MMAP;
    buf.index = frame->buffer_index;

    if (ioctl(s_video_fd, VIDIOC_QBUF, &buf) < 0) {
        ESP_LOGE(TAG_CAM, "VIDIOC_QBUF failed on release for index %u", buf.index);
        return -1;
    }

    return 0;
}

int32_t p4_hardware_init_all(const p4_hardware_config_t *config) {
    ESP_LOGI(TAG_HW, "Starting Unified Hardware Bring-up...");

    if (s_hardware_initialized) return ESP_OK;
    if (!config) return -1;

    esp_err_t ret = init_audio_system();
    if (ret != ESP_OK) return ret;

    ret = init_display_system();
    if (ret != ESP_OK) return ret;

    ret = p4_camera_init_v4l2(CONFIG_BIOMETRICS_CAM_WIDTH, CONFIG_BIOMETRICS_CAM_HEIGHT);
    if (ret != ESP_OK) return ret;
    ESP_LOGI(TAG_HW, "Display Systems Initialized Successfully!");

    ret = init_p4_ethernet();
    if (ret != ESP_OK) return ret;
    ESP_LOGI(TAG_HW, "Ethernet Initialized Successfully!");

    s_hardware_initialized = true;
    return ESP_OK;
}

int32_t p4_perform_ota_update(const char *url) {
    if (url == NULL) return ESP_ERR_INVALID_ARG;

    ESP_LOGI(TAG_OTA, "Starting OTA update from: %s", url);

    esp_http_client_config_t http_config = {};
    http_config.url = url;
    http_config.timeout_ms = 15000;
    http_config.keep_alive_enable = true;

    esp_https_ota_config_t ota_config = {};
    ota_config.http_config = &http_config;

    esp_err_t ret = esp_https_ota(&ota_config);
    if (ret == ESP_OK) {
        ESP_LOGI(TAG_OTA, "OTA Update complete! Rebooting in 2 seconds...");
        vTaskDelay(pdMS_TO_TICKS(2000));
        esp_restart();
    } else {
        ESP_LOGE(TAG_OTA, "OTA Update failed: 0x%x (%s)", ret, esp_err_to_name(ret));
    }
    return ret;
}

void p4_mark_app_valid(void) {
    esp_ota_img_states_t ota_state;
    const esp_partition_t *running = esp_ota_get_running_partition();

    if (esp_ota_get_state_partition(running, &ota_state) == ESP_OK) {
        if (ota_state == ESP_OTA_IMG_PENDING_VERIFY) {
            ESP_LOGI(TAG_OTA, "New app booted cleanly! Marking partition as VALID.");
            esp_ota_mark_app_valid_cancel_rollback();
        }
    }
}

int32_t init_p4_ethernet(void) {
    ESP_LOGI(TAG_ETH, "Initializing Waveshare ESP32-P4-NANO EMAC Ethernet...");

    gpio_config_t rst_cfg = {};
    rst_cfg.pin_bit_mask = (1ULL << BoardPins::Ethernet::RESET);
    rst_cfg.mode = GPIO_MODE_OUTPUT;
    gpio_config(&rst_cfg);

    gpio_set_level(BoardPins::Ethernet::RESET, 0);
    vTaskDelay(pdMS_TO_TICKS(50));
    gpio_set_level(BoardPins::Ethernet::RESET, 1);
    vTaskDelay(pdMS_TO_TICKS(100));

    esp_err_t ret = esp_netif_init();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    ret = esp_event_loop_create_default();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) return ret;

    esp_netif_config_t cfg = ESP_NETIF_DEFAULT_ETH();
    esp_netif_t *eth_netif = esp_netif_new(&cfg);

    eth_mac_config_t mac_config = ETH_MAC_DEFAULT_CONFIG();
    eth_esp32_emac_config_t emac_config = {};

    emac_config.smi_gpio.mdc_num  = BoardPins::Ethernet::MDC;
    emac_config.smi_gpio.mdio_num = BoardPins::Ethernet::MDIO;
    gpio_set_pull_mode(BoardPins::Ethernet::MDIO, GPIO_PULLUP_ONLY);

    emac_config.interface = EMAC_DATA_INTERFACE_RMII;
    emac_config.clock_config.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config.rmii.clock_gpio = (emac_rmii_clock_gpio_t)BoardPins::Ethernet::CLK;

    emac_config.dma_burst_len = ETH_DMA_BURST_LEN_32;
    emac_config.intr_priority = 0;

    emac_config.emac_dataif_gpio.rmii.tx_en_num  = BoardPins::Ethernet::RMII::TX_EN;
    emac_config.emac_dataif_gpio.rmii.txd0_num   = BoardPins::Ethernet::RMII::TXD0;
    emac_config.emac_dataif_gpio.rmii.txd1_num   = BoardPins::Ethernet::RMII::TXD1;
    emac_config.emac_dataif_gpio.rmii.crs_dv_num = BoardPins::Ethernet::RMII::CRS_DV;
    emac_config.emac_dataif_gpio.rmii.rxd0_num   = BoardPins::Ethernet::RMII::RXD0;
    emac_config.emac_dataif_gpio.rmii.rxd1_num   = BoardPins::Ethernet::RMII::RXD1;

    emac_config.clock_config_out_in.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config_out_in.rmii.clock_gpio = -1;

    esp_eth_mac_t *mac = esp_eth_mac_new_esp32(&emac_config, &mac_config);
    if (!mac) return -1;

    eth_phy_config_t phy_config = ETH_PHY_DEFAULT_CONFIG();
    phy_config.phy_addr = 1;
    phy_config.reset_gpio_num = -1;

    esp_eth_phy_t *phy = esp_eth_phy_new_ip101(&phy_config);
    if (!phy) {
        mac->del(mac);
        return -1;
    }

    esp_eth_config_t eth_config = ETH_DEFAULT_CONFIG(mac, phy);
    esp_eth_handle_t eth_handle = NULL;
    ret = esp_eth_driver_install(&eth_config, &eth_handle);
    if (ret != ESP_OK) {
        mac->del(mac);
        phy->del(phy);
        return ret;
    }

    ret = esp_netif_attach(eth_netif, esp_eth_new_netif_glue(eth_handle));
    if (ret != ESP_OK) return ret;

    return esp_eth_start(eth_handle);
}

int32_t p4_display_draw_frame(const uint16_t *frame_buffer, uint16_t width, uint16_t height) {
    if (!s_lcd_panel || !frame_buffer) return ESP_ERR_INVALID_ARG;

    esp_err_t ret = esp_lcd_panel_draw_bitmap(s_lcd_panel, 0, 0, width, height, frame_buffer);
    return (int32_t)ret;
}

int32_t p4_display_draw_bitmap(uint16_t x_start, uint16_t y_start, uint16_t x_end, uint16_t y_end, const uint16_t *data) {
    if (!s_lcd_panel || !data) return -1;

    uintptr_t addr = (uintptr_t)data;
    size_t len = (x_end - x_start) * (y_end - y_start) * sizeof(uint16_t);

    uintptr_t aligned_addr = addr & ~(C_LINE_SIZE - 1);
    size_t aligned_len = (addr + len - aligned_addr + C_LINE_SIZE - 1) & ~(C_LINE_SIZE - 1);

    esp_cache_msync((void *)aligned_addr, aligned_len, ESP_CACHE_MSYNC_FLAG_DIR_C2M);

    return (int32_t)esp_lcd_panel_draw_bitmap(s_lcd_panel, x_start, y_start, x_end, y_end, data);
}

// -----------------------------------------------------------------------------
// ESP-DL Model Inference Entry Points
// -----------------------------------------------------------------------------
int32_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size) {
    if (!model_buf || model_size == 0) {
        ESP_LOGE(TAG_FACENET, "Invalid model buffer or size");
        return ESP_ERR_INVALID_ARG;
    }

    g_mobilefacenet_model = new (std::nothrow) dl::Model(
        (const char *)model_buf,
        model_size
    );

    if (!g_mobilefacenet_model) {
        ESP_LOGE(TAG_FACENET, "Failed to allocate dl::Model");
        return ESP_FAIL;
    }

    ESP_LOGI(TAG_FACENET, "MobileFaceNet loaded successfully! (Flash addr: %p)", model_buf);
    return ESP_OK;
}

int32_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len) {
    if (!g_mobilefacenet_model) {
        ESP_LOGE(TAG_FACENET, "Model not initialized!");
        return ESP_ERR_INVALID_STATE;
    }

    if (!crop_rgb888 || !out_embedding || embedding_len < 512) {
        return ESP_ERR_INVALID_ARG;
    }

    std::map<std::string, dl::TensorBase *> &inputs = g_mobilefacenet_model->get_inputs();
    if (inputs.empty()) return ESP_FAIL;

    dl::TensorBase *input_tensor = inputs.begin()->second;
    void *tensor_buf = input_tensor->get_element_ptr();
    if (!tensor_buf) return ESP_FAIL;

    size_t input_bytes = input_tensor->get_bytes();
    memcpy(tensor_buf, crop_rgb888, input_bytes);

    g_mobilefacenet_model->run();

    std::map<std::string, dl::TensorBase *> &outputs = g_mobilefacenet_model->get_outputs();
    if (outputs.empty()) return ESP_FAIL;

    dl::TensorBase *output_tensor = outputs.begin()->second;
    int8_t *quant_data = (int8_t *)output_tensor->get_element_ptr();
    if (!quant_data) return ESP_FAIL;

    int exponent = output_tensor->get_exponent();
    float scale = powf(2.0f, (float)exponent);

    float sum_squares = 0.0f;
    for (size_t i = 0; i < 512; i++) {
        float dequant_val = (float)quant_data[i] * scale;
        out_embedding[i] = dequant_val;
        sum_squares += dequant_val * dequant_val;
    }

    float l2_norm = sqrtf(sum_squares);
    if (l2_norm > 1e-6f) {
        float inv_norm = 1.0f / l2_norm;
        for (size_t i = 0; i < 512; i++) {
            out_embedding[i] *= inv_norm;
        }
    }

    return ESP_OK;
}

void setup_split_screen_ui(void) {
    if (!lvgl_lock(200)) return;

    lv_obj_t *scr = lv_screen_active();
    lv_obj_set_style_bg_color(scr, lv_color_hex(0x121212), LV_PART_MAIN);
    lv_obj_set_style_bg_opa(scr, LV_OPA_COVER, LV_PART_MAIN);

    // Left Column: 640x720 Video Viewport
    s_camera_img = lv_image_create(scr);
    lv_obj_set_size(s_camera_img, 640, 720);
    lv_obj_align(s_camera_img, LV_ALIGN_LEFT_MID, 0, 0);

    // Right Column: 640x720 Control Panel
    lv_obj_t *panel = lv_obj_create(scr);
    lv_obj_set_size(panel, 640, 720);
    lv_obj_align(panel, LV_ALIGN_RIGHT_MID, 0, 0);
    
    // Explicit dark styling to replace default LVGL light-grey box
    lv_obj_set_style_bg_color(panel, lv_color_hex(0x1E1E1E), LV_PART_MAIN);
    lv_obj_set_style_bg_opa(panel, LV_OPA_COVER, LV_PART_MAIN);
    lv_obj_set_style_border_width(panel, 0, LV_PART_MAIN);
    lv_obj_set_style_radius(panel, 0, LV_PART_MAIN);

    lv_obj_t *title = lv_label_create(panel);
    lv_label_set_text(title, "MULTIMODAL BIOMETRICS");
    lv_obj_align(title, LV_ALIGN_TOP_MID, 0, 40);

    lvgl_unlock();
}

void update_camera_viewport(const p4_camera_frame_t *frame) {
    if (!s_ppa_client) {
        ESP_LOGE("PPA_SYS", "PPA client handle is NULL!");
        return;
    }
    if (!s_camera_img || !frame || !frame->data) return;

    const size_t cache_line_size = 128;
    const uint16_t out_w = 640;
    const uint16_t out_h = 480;
    const uint32_t out_stride = out_w * sizeof(uint16_t); // 1280 bytes
    const size_t raw_out_bytes = out_stride * out_h;      // 614,400 bytes

    // Round output size up to 128-byte cache line boundary
    const size_t out_bytes = (raw_out_bytes + cache_line_size - 1) & ~(cache_line_size - 1);

    // 128-byte aligned PSRAM buffers for PPA DMA
    if (!s_ppa_buf[0]) {
        s_ppa_buf[0] = (uint16_t *)heap_caps_aligned_alloc(
            128, out_bytes, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT
        );
        s_ppa_buf[1] = (uint16_t *)heap_caps_aligned_alloc(
            128, out_bytes, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT
        );
        if (!s_ppa_buf[0] || !s_ppa_buf[1]) {
            ESP_LOGE("PPA_SYS", "Failed to allocate 128-byte aligned PSRAM buffers!");
            return;
        }
    }

    uint16_t *current_ppa_out = s_ppa_buf[s_ppa_idx];
    s_ppa_idx ^= 1;

    uint32_t in_width = (frame->width > 0) ? frame->width : 1280;
    uint32_t in_height = (frame->height > 0) ? frame->height : 960;
    uint32_t bytesperline = (frame->data_len > 0 && in_height > 0) 
                            ? (frame->data_len / in_height) 
                            : (in_width * sizeof(uint16_t));
    uint32_t in_stride_pixels = bytesperline / sizeof(uint16_t);

    ppa_srm_oper_config_t srm_cfg = {};

    // --- INPUT PICTURE CONFIGURATION ---
    srm_cfg.in.buffer = frame->data;
    srm_cfg.in.pic_w = in_stride_pixels;
    srm_cfg.in.pic_h = in_height;
    srm_cfg.in.block_w = in_width;
    srm_cfg.in.block_h = in_height;
    srm_cfg.in.block_offset_x = 0;
    srm_cfg.in.block_offset_y = 0;
    srm_cfg.in.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

    // --- OUTPUT PICTURE CONFIGURATION ---
    srm_cfg.out.buffer = current_ppa_out;
    srm_cfg.out.buffer_size = out_bytes;       // Explicit output buffer size in bytes
    srm_cfg.out.pic_w = out_w;
    srm_cfg.out.pic_h = out_h;
    srm_cfg.out.block_offset_x = 0;
    srm_cfg.out.block_offset_y = 0;
    srm_cfg.out.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

    // --- TRANSFORMATIONS ---
    srm_cfg.rotation_angle = PPA_SRM_ROTATION_ANGLE_0;
    srm_cfg.scale_x = 0.5f;
    srm_cfg.scale_y = 0.5f;
    srm_cfg.mirror_x = false;
    srm_cfg.mirror_y = false;
    srm_cfg.rgb_swap = false;
    srm_cfg.byte_swap = false;
    srm_cfg.mode = PPA_TRANS_MODE_BLOCKING;

    // Execute PPA hardware scaling
    esp_err_t err = ppa_do_scale_rotate_mirror(s_ppa_client, &srm_cfg);
    if (err != ESP_OK) {
        static uint32_t err_cnt = 0;
        if (++err_cnt % 30 == 1) {
            ESP_LOGE("PPA_SYS", "ppa_do_scale_rotate_mirror failed: 0x%x", err);
        }
        return;
    }

    // --- PASS HARDWARE-SCALED BUFFER TO LVGL 9 ---
    if (lvgl_lock(16)) {
        s_camera_dsc.header.cf = LV_COLOR_FORMAT_RGB565;
        s_camera_dsc.header.w = out_w; 
        s_camera_dsc.header.h = out_h;
        s_camera_dsc.header.stride = out_stride;
        s_camera_dsc.data_size = raw_out_bytes;
        s_camera_dsc.data = (const uint8_t *)current_ppa_out;

        lv_image_set_src(s_camera_img, NULL);
        lv_image_set_src(s_camera_img, &s_camera_dsc);
        lv_image_set_scale(s_camera_img, 256);

        lv_obj_move_foreground(s_camera_img);
        lv_obj_invalidate(s_camera_img);
        
        lvgl_unlock();
    }
}

} // extern "C"