#include "driver/i2c_master.h"
#include "bindings.h"

#include "esp_video_init.h"
#include "esp_video_ioctl.h"
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <linux/videodev2.h>
#include <stdio.h>
#include <cstring>
#include <memory>
#include <cmath>
#include <sys/mman.h>

#include "esp_log.h"
#include "driver/gpio.h"
#include "driver/i2s_std.h"
#include "driver/i2c_master.h"
#include "esp_err.h"
#include "esp_check.h"
#include "esp_cache.h"
#include "esp_attr.h"

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

// Official Waveshare ESP32-P4-NANO BSP Headers
#include "esp_lcd_touch.h"
#include "esp_lcd_touch_gt911.h"
#include "bsp/esp32_p4_nano.h"
#include "bsp/display.h"
#include "bsp/touch.h"

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
}

namespace BoardAddr {
    constexpr uint8_t OV5647_SCCB = 0x36;
}

#define TAG_HW      "p4_hardware"
#define TAG_CAM     "p4_camera"
#define TAG_ETH     "p4_ethernet"
#define TAG_OTA     "p4_ota"
#define TAG_FACENET "ESP_DL_FACENET"
#define TAG_I2S     "I2S_WRAPPER"
#define TAG_LCD     "p4_lcd"
#define TAG_TOUCH   "p4_touch"

#define CAM_BUF_COUNT 2
#define C_LINE_SIZE 128  // ESP32-P4 L2 Cache Line Size (0x80)

// Global Hardware Handles
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;
static esp_lcd_touch_handle_t s_touch_handle = NULL;
static i2c_master_dev_handle_t s_gt911_i2c_dev = NULL;

static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;

static bool s_hardware_initialized = false;
static dl::Model *g_mobilefacenet_model = NULL;

static int s_video_fd = -1;
struct v4l2_frame_buffer_t s_cam_buffers[CAM_BUF_COUNT] = {};
static size_t s_cam_buf_lengths[CAM_BUF_COUNT] = {0};

extern "C" {
    i2c_master_bus_handle_t bsp_i2c_get_handle(void);
    esp_err_t bsp_i2c_init(void);
}

// -----------------------------------------------------------------------------
// Display Initialization & Helper Functions
// -----------------------------------------------------------------------------
static esp_err_t power_on_display_bridge_0x45(void) {
    // Acquire the pre-initialized shared I2C0 bus handle created by Kconfig/BSP
    i2c_master_bus_handle_t bus_handle = bsp_i2c_get_handle();
    if (!bus_handle) {
        ESP_LOGE(TAG_LCD, "Pre-initialized BSP I2C handle is NULL");
        return ESP_ERR_INVALID_STATE;
    }

    i2c_master_dev_handle_t dev_handle = NULL;
    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = 0x45;
    dev_cfg.scl_speed_hz = 100000;

    esp_err_t err = i2c_master_bus_add_device(bus_handle, &dev_cfg, &dev_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to register 0x45 I2C device on shared bus: 0x%x", err);
        return err;
    }

    ESP_LOGI(TAG_LCD, "Sending power-on sequence to display bridge IC @ 0x45...");
    uint8_t cmd1[2] = {0x95, 0x11};
    i2c_master_transmit(dev_handle, cmd1, 2, 1000);
    uint8_t cmd2[2] = {0x95, 0x17};
    i2c_master_transmit(dev_handle, cmd2, 2, 1000);
    uint8_t cmd3[2] = {0x96, 0x00};
    i2c_master_transmit(dev_handle, cmd3, 2, 1000);
    vTaskDelay(pdMS_TO_TICKS(100));

    // Assert power and wait for PMIC voltage rails to stabilize
    uint8_t cmd4[2] = {0x96, 0xFF};
    i2c_master_transmit(dev_handle, cmd4, 2, 1000);
    vTaskDelay(pdMS_TO_TICKS(1000));

    i2c_master_bus_rm_device(dev_handle);
    ESP_LOGI(TAG_LCD, "0x45 Power-on sequence complete & rails stabilized.");
    return ESP_OK;
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

int32_t init_display_with_bsp(void) {
    if (s_lcd_panel != NULL) return ESP_OK;

    ESP_LOGI(TAG_LCD, "Initializing 5-inch MIPI-DSI Display via Waveshare BSP...");

    // 1. Initialize display via BSP handles (uses unified bsp_i2c_get_handle internally)
    bsp_lcd_handles_t handles = {};
    bsp_display_config_t disp_cfg = {};
    
    esp_err_t err = bsp_display_new_with_handles(&disp_cfg, &handles);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_LCD, "Failed to create BSP display handles: 0x%x (%s)", err, esp_err_to_name(err));
        return err;
    }

    s_lcd_panel = handles.panel;
    s_lcd_io = handles.io;

    // 2. Force Backlight Pin HIGH via GPIO 26
    gpio_config_t bk_gpio_config = {};
    bk_gpio_config.pin_bit_mask = (1ULL << GPIO_NUM_26);
    bk_gpio_config.mode = GPIO_MODE_OUTPUT;
    gpio_config(&bk_gpio_config);
    gpio_set_level(GPIO_NUM_26, 1);

    ESP_LOGI(TAG_LCD, "BSP Display initialized successfully!");
    return ESP_OK;
}

void p4_display_draw_test_pattern(void) {
    if (!s_lcd_panel) {
        ESP_LOGE(TAG_LCD, "Cannot draw test pattern: Display panel handle is NULL!");
        return;
    }

    size_t buffer_size = 720 * 1280 * sizeof(uint16_t);
    uint16_t *test_buf = (uint16_t *)heap_caps_malloc(buffer_size, MALLOC_CAP_SPIRAM);

    if (test_buf) {
        for (int i = 0; i < 720 * 1280; i++) {
            test_buf[i] = 0x07FF; // Solid Cyan (RGB565)
        }
        ESP_LOGI(TAG_HW, "Pushing Solid Cyan Test Frame to Display...");
        esp_lcd_panel_draw_bitmap(s_lcd_panel, 0, 0, 720, 1280, test_buf);
        heap_caps_free(test_buf);
    }
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
int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, -1, dout_gpio);
}

int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio) {
    return init_i2s_duplex_c(sample_rate, bclk_gpio, ws_gpio, din_gpio, -1);
}

int init_i2s_duplex_c(uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio, int dout_gpio) {
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

    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_0, I2S_ROLE_MASTER);
    
    i2s_chan_handle_t *p_tx = (dout_gpio >= 0) ? &g_i2s_tx_handle : NULL;
    i2s_chan_handle_t *p_rx = (din_gpio >= 0)  ? &g_i2s_rx_handle : NULL;

    esp_err_t ret = i2s_new_channel(&chan_cfg, p_tx, p_rx);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_I2S, "i2s_new_channel failed: 0x%x", ret);
        return (int)ret;
    }

    if (g_i2s_tx_handle && dout_gpio >= 0) {
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
        if (ret != ESP_OK) return (int)ret;
        ret = i2s_channel_enable(g_i2s_tx_handle);
        if (ret != ESP_OK) return (int)ret;
    }

    if (g_i2s_rx_handle && din_gpio >= 0) {
        i2s_std_config_t rx_cfg = {
            .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
            .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
            .gpio_cfg = {
                .mclk = I2S_GPIO_UNUSED,
                .bclk = (gpio_num_t)bclk_gpio,
                .ws   = (gpio_num_t)ws_gpio,
                .dout = I2S_GPIO_UNUSED,
                .din  = (gpio_num_t)din_gpio,
            },
        };
        ret = i2s_channel_init_std_mode(g_i2s_rx_handle, &rx_cfg);
        if (ret != ESP_OK) return (int)ret;
        ret = i2s_channel_enable(g_i2s_rx_handle);
        if (ret != ESP_OK) return (int)ret;
    }

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

void configure_camera_exposure_gain(int fd) {
    set_v4l2_control(fd, V4L2_CID_EXPOSURE_AUTO, V4L2_EXPOSURE_AUTO, "EXPOSURE_AUTO");
    set_v4l2_control(fd, V4L2_CID_AUTOGAIN, 1, "AUTOGAIN");
    set_v4l2_control(fd, V4L2_CID_AUTO_WHITE_BALANCE, 1, "AUTO_WHITE_BALANCE");
}

// -----------------------------------------------------------------------------
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

esp_cam_sensor_device_t *ov5647_detect(void *config);

int32_t p4_camera_init_v4l2_default(void) {
    return p4_camera_init_v4l2(CONFIG_BIOMETRICS_CAM_WIDTH, CONFIG_BIOMETRICS_CAM_HEIGHT);
}

int32_t p4_camera_init_v4l2(uint16_t width, uint16_t height) {
    if (s_video_fd >= 0) return 0;

    ESP_LOGI(TAG_CAM, "Initializing OV5647 via esp_video (Pre-initialized I2C0 Bus)...");

    // 1. Get Shared I2C0 Bus Handle from BSP
    i2c_master_bus_handle_t shared_i2c_bus = bsp_i2c_get_handle();
    if (!shared_i2c_bus) {
        ESP_LOGE(TAG_CAM, "bsp_i2c_get_handle() returned NULL!");
        return -1;
    }

    // 2. Hardware Probe Test on I2C 0x36
    esp_err_t probe_ret = i2c_master_probe(shared_i2c_bus, 0x36, 100);
    if (probe_ret == ESP_OK) {
        ESP_LOGI(TAG_CAM, "SUCCESS: OV5647 Camera ACKed on I2C address 0x36!");
    } else {
        ESP_LOGE(TAG_CAM, "ERROR: OV5647 did NOT respond on I2C 0x36! (err: %s)", esp_err_to_name(probe_ret));
        return -1;
    }

    (void)ov5647_detect;

    // 3. Configure esp_video CSI parameters with explicit BoardPins
    esp_video_init_csi_config_t csi_cfg = {};
    csi_cfg.sccb_config.init_sccb = false;
    csi_cfg.sccb_config.i2c_handle = shared_i2c_bus;
    csi_cfg.sccb_config.freq = 100000;

    csi_cfg.reset_pin = static_cast<gpio_num_t>(BoardPins::Camera::RESET);
    csi_cfg.pwdn_pin  = static_cast<gpio_num_t>(BoardPins::Camera::PWDN);
    csi_cfg.dont_init_ldo = false;

    esp_video_init_config_t cam_cfg = {};
    cam_cfg.csi = &csi_cfg;

    // 4. Initialize esp_video
    esp_err_t ret = esp_video_init(&cam_cfg);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_CAM, "esp_video_init failed: %s (0x%x)", esp_err_to_name(ret), ret);
        return ret;
    }

    // 5. Open V4L2 Character Device
    s_video_fd = open("/dev/video0", O_RDWR|O_NONBLOCK);
    if (s_video_fd < 0) {
        ESP_LOGE(TAG_CAM, "Failed to open /dev/video0 (errno %d: %s)", errno, strerror(errno));
        return -1;
    }

    // 6. Query & Set Active Format
    struct v4l2_format fmt = {};
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(s_video_fd, VIDIOC_G_FMT, &fmt) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to get active format from /dev/video0");
        close(s_video_fd);
        s_video_fd = -1;
        return -1;
    }

    fmt.fmt.pix.pixelformat = V4L2_PIX_FMT_RGB565;
    if (ioctl(s_video_fd, VIDIOC_S_FMT, &fmt) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to set RGB565 format (errno %d: %s)", errno, strerror(errno));
        close(s_video_fd);
        s_video_fd = -1;
        return -1;
    }

    configure_camera_exposure_gain(s_video_fd);

    // 7. Request MMAP Buffers
    struct v4l2_requestbuffers req = {};
    req.count = CAM_BUF_COUNT;
    req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    req.memory = V4L2_MEMORY_MMAP;
    if (ioctl(s_video_fd, VIDIOC_REQBUFS, &req) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to request V4L2 buffers");
        close(s_video_fd);
        s_video_fd = -1;
        return -1;
    }

    // 8. Map and ENQUEUE all allocated buffers into the CSI DMA ring
    for (int i = 0; i < CAM_BUF_COUNT; i++) {
        struct v4l2_buffer buf = {};
        buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = V4L2_MEMORY_MMAP;
        buf.index = i;

        if (ioctl(s_video_fd, VIDIOC_QUERYBUF, &buf) < 0) {
            ESP_LOGE(TAG_CAM, "Failed to query buffer %d", i);
            return -1;
        }

        s_cam_buf_lengths[i] = buf.length;
        s_cam_buffers[i].start = mmap(NULL, buf.length, PROT_READ | PROT_WRITE, MAP_SHARED, s_video_fd, buf.m.offset);
        if (s_cam_buffers[i].start == MAP_FAILED) {
            ESP_LOGE(TAG_CAM, "Failed to mmap buffer %d", i);
            return -1;
        }

        if (ioctl(s_video_fd, VIDIOC_QBUF, &buf) < 0) {
            ESP_LOGE(TAG_CAM, "Failed to qbuf (enqueue) buffer %d", i);
            return -1;
        }
    }

    // 9. Start Streaming
    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(s_video_fd, VIDIOC_STREAMON, &type) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to start V4L2 stream (errno %d: %s)", errno, strerror(errno));
        close(s_video_fd);
        s_video_fd = -1;
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
        static uint32_t err_count = 0;
        if (++err_count % 60 == 1) {
            ESP_LOGI("CAM_DQBUF", "ioctl(VIDIOC_DQBUF) failed (ret: %d, errno: %d -> %s)", 
                     ret, errno, strerror(errno));
        }
        return (errno == EAGAIN || errno == EWOULDBLOCK) ? -2 : -1;
    }

    frame->data = (uint8_t *)s_cam_buffers[buf.index].start;
    frame->data_len = buf.bytesused;
    frame->width = 1280;
    frame->height = 960;
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

    esp_err_t ret = init_display_with_bsp();
    if (ret != ESP_OK) return ret;

    ret = p4_camera_init_v4l2(CONFIG_BIOMETRICS_CAM_WIDTH, CONFIG_BIOMETRICS_CAM_HEIGHT);
    if (ret != ESP_OK) return ret;

    ESP_LOGI(TAG_HW, "Display Systems Initialized Successfully!");
    s_hardware_initialized = true;

    p4_display_draw_test_pattern();
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

int init_admin_button_gpio() {
    gpio_config_t io_conf = {};
    io_conf.pin_bit_mask = (1ULL << BoardPins::System::ADMIN_BTN);
    io_conf.mode = GPIO_MODE_INPUT;
    io_conf.pull_up_en = GPIO_PULLUP_ENABLE;
    io_conf.pull_down_en = GPIO_PULLDOWN_DISABLE;
    io_conf.intr_type = GPIO_INTR_DISABLE;

    return (int)gpio_config(&io_conf);
}

bool is_admin_button_pressed() {
    return gpio_get_level(BoardPins::System::ADMIN_BTN) == 0;
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

    try {
        g_mobilefacenet_model = new dl::Model(
            (const char *)model_buf,
            model_size
        );

        if (!g_mobilefacenet_model) {
            ESP_LOGE(TAG_FACENET, "Failed to allocate dl::Model");
            return ESP_FAIL;
        }

        ESP_LOGI(TAG_FACENET, "MobileFaceNet loaded successfully! (Flash addr: %p)", model_buf);
        return ESP_OK;

    } catch (const std::exception &e) {
        ESP_LOGE(TAG_FACENET, "Exception during ESP-DL model construction: %s", e.what());
        return ESP_FAIL;
    } catch (...) {
        ESP_LOGE(TAG_FACENET, "Unknown exception during ESP-DL model construction");
        return ESP_FAIL;
    }
}

int32_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len) {
    if (!g_mobilefacenet_model) {
        ESP_LOGE(TAG_FACENET, "Model not initialized! Call dl_mobilefacenet_init first.");
        return ESP_ERR_INVALID_STATE;
    }

    if (!crop_rgb888 || !out_embedding || embedding_len < 512) {
        return ESP_ERR_INVALID_ARG;
    }

    try {
        std::map<std::string, dl::TensorBase *> &inputs = g_mobilefacenet_model->get_inputs();
        if (inputs.empty()) return ESP_FAIL;

        dl::TensorBase *input_tensor = inputs.begin()->second;
        void *tensor_buf = input_tensor->get_element_ptr();
        if (!tensor_buf) return ESP_FAIL;

        size_t input_bytes = input_tensor->get_bytes();
        std::memcpy(tensor_buf, crop_rgb888, input_bytes);

        g_mobilefacenet_model->run();

        std::map<std::string, dl::TensorBase *> &outputs = g_mobilefacenet_model->get_outputs();
        if (outputs.empty()) return ESP_FAIL;

        dl::TensorBase *output_tensor = outputs.begin()->second;
        int8_t *quant_data = (int8_t *)output_tensor->get_element_ptr();

        int exponent = output_tensor->get_exponent();
        float scale = std::pow(2.0f, exponent);

        float sum_squares = 0.0f;
        for (size_t i = 0; i < 512; i++) {
            float dequant_val = (float)quant_data[i] * scale;
            out_embedding[i] = dequant_val;
            sum_squares += dequant_val * dequant_val;
        }

        float l2_norm = std::sqrt(sum_squares);
        if (l2_norm > 1e-6f) {
            float inv_norm = 1.0f / l2_norm;
            for (size_t i = 0; i < 512; i++) {
                out_embedding[i] *= inv_norm;
            }
        }

        return ESP_OK;

    } catch (const std::exception &e) {
        ESP_LOGE(TAG_FACENET, "Exception during inference: %s", e.what());
        return ESP_FAIL;
    } catch (...) {
        ESP_LOGE(TAG_FACENET, "Unknown exception during inference");
        return ESP_FAIL;
    }
}

// -----------------------------------------------------------------------------
// Touch Subsystem Entry Points
// -----------------------------------------------------------------------------

static uint8_t g_gt911_addr = 0x5D;
static i2c_master_dev_handle_t s_gt911_dev_handle = NULL;

esp_err_t power_on_bridge_legacy(i2c_port_t port)
{
    i2c_master_bus_handle_t bus_handle = bsp_i2c_get_handle();
    if (bus_handle == NULL) {
        bsp_i2c_init();
        bus_handle = bsp_i2c_get_handle();
    }
    if (bus_handle == NULL) {
        return ESP_FAIL;
    }

    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = 0x45,
        .scl_speed_hz = 100000,
    };
    i2c_master_dev_handle_t dev_handle = NULL;
    esp_err_t ret = i2c_master_bus_add_device(bus_handle, &dev_cfg, &dev_handle);
    if (ret != ESP_OK) {
        return ret;
    }

    uint8_t cmd1[2] = {0x95, 0x11};
    i2c_master_transmit(dev_handle, cmd1, 2, 100);

    uint8_t cmd2[2] = {0x95, 0x17};
    i2c_master_transmit(dev_handle, cmd2, 2, 100);

    uint8_t cmd3[2] = {0x96, 0x00};
    i2c_master_transmit(dev_handle, cmd3, 2, 100);

    vTaskDelay(pdMS_TO_TICKS(100));

    uint8_t cmd4[2] = {0x96, 0xFF};
    i2c_master_transmit(dev_handle, cmd4, 2, 100);

    i2c_master_bus_rm_device(dev_handle);
    return ESP_OK;
}

int32_t init_touch_with_bsp(void) {
    i2c_master_bus_handle_t bus_handle = bsp_i2c_get_handle();
    if (bus_handle == NULL) {
        bsp_i2c_init();
        bus_handle = bsp_i2c_get_handle();
    }
    if (bus_handle == NULL) {
        ESP_LOGE(TAG_TOUCH, "Failed to obtain BSP I2C master bus handle");
        return ESP_FAIL;
    }

    // Configure GT911 device on BSP's I2C bus
    if (s_gt911_dev_handle == NULL) {
        i2c_device_config_t dev_cfg = {};
        dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
        dev_cfg.device_address = g_gt911_addr;
        dev_cfg.scl_speed_hz = 100000;

        esp_err_t err = i2c_master_bus_add_device(bus_handle, &dev_cfg, &s_gt911_dev_handle);
        if (err != ESP_OK) {
            ESP_LOGE(TAG_TOUCH, "Failed to add GT911 device to master bus: 0x%x", err);
            return err;
        }
    }

    // Read product ID from register 0x8140
    uint8_t test_reg[2] = {0x81, 0x40};
    uint8_t product_id[4] = {0};
    esp_err_t err = i2c_master_transmit_receive(s_gt911_dev_handle, test_reg, 2, product_id, 4, 100);
    if (err == ESP_OK) {
        ESP_LOGI(TAG_TOUCH, "GT911 Touch detected! Product ID: %.4s", product_id);
        return ESP_OK;
    } else {
        ESP_LOGE(TAG_TOUCH, "GT911 Touch probe failed at address 0x%02X: 0x%x", g_gt911_addr, err);
        return err;
    }
}

bool p4_touch_read(p4_touch_data_t *data) {
    if (data == NULL || s_gt911_dev_handle == NULL) {
        return false;
    }

    // Read GT911 touch status and coordinates (Reg 0x814E, length 7 bytes)
    uint8_t reg_addr[2] = {0x81, 0x4E};
    uint8_t touch_buf[7] = {0};

    esp_err_t err = i2c_master_transmit_receive(s_gt911_dev_handle, reg_addr, 2, touch_buf, sizeof(touch_buf), 50);
    uint8_t clear_cmd[3] = {0x81, 0x4E, 0x00};

    if (err != ESP_OK) {
        i2c_master_transmit(s_gt911_dev_handle, clear_cmd, 3, 20);
        return false;
    }

    uint8_t touch_status = touch_buf[0];
    uint8_t touch_num = touch_status & 0x0F;

    if ((touch_status & 0x80) && touch_num > 0) {
        data->x = touch_buf[2] | (touch_buf[3] << 8);
        data->y = touch_buf[4] | (touch_buf[5] << 8);
        data->touched = true;

        // Clear status register
        i2c_master_transmit(s_gt911_dev_handle, clear_cmd, 3, 20);
        return true;
    }

    // Clear buffer ready bit when no touches detected
    i2c_master_transmit(s_gt911_dev_handle, clear_cmd, 3, 20);
    data->touched = false;
    return false;
}

} // extern "C"