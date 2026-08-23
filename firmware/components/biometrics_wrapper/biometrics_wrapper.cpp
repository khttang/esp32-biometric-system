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

#include "esp_ldo_regulator.h"
#include "esp_lcd_mipi_dsi.h"
#include "esp_lcd_hx8394.h"
#include "esp_lcd_touch_gt911.h"
#include "esp_lvgl_port.h"
#include "esp_lcd_touch.h"
#include "lvgl.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include "sdkconfig.h"
#include "biometrics_wrapper.h"

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

// -----------------------------------------------------------------------------
// Video Pipeline & Display Layout Constants
// -----------------------------------------------------------------------------
namespace VideoConfig {
    // OV5647 Native Sensor Stream Dimensions (DO NOT CHANGE FROM 960)
    constexpr uint16_t SENSOR_WIDTH   = 1280;
    constexpr uint16_t SENSOR_HEIGHT  = 960;

    // Display Geometry (1280x720 Landscape)
    constexpr uint16_t DISPLAY_WIDTH  = 1280;
    constexpr uint16_t DISPLAY_HEIGHT = 720;  

    // Split-Screen Layout Dimensions
    constexpr uint16_t VIEWPORT_WIDTH  = 640; // Left column width
    constexpr uint16_t VIEWPORT_HEIGHT = 480; // Centered 16:9 canvas height
    constexpr uint16_t PANEL_WIDTH    = 640; // Right control panel width
    constexpr uint16_t PANEL_HEIGHT   = 720; // Right control panel height
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

#define CAM_BUF_COUNT 3
#define C_LINE_SIZE 128               // ESP32-P4 L2 Cache Line Size (0x80)

// Global Subsystem Handles
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static i2c_master_bus_handle_t s_i2c_bus_handle = NULL;
static i2c_master_dev_handle_t s_gt911_i2c_dev = NULL;


static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;

static ppa_client_handle_t s_ppa_client = NULL;

static bool s_hardware_initialized = false;
static dl::Model *g_mobilefacenet_model = NULL;

static volatile bool s_camera_streaming = false;
static int s_video_fd = -1;
struct v4l2_frame_buffer_t s_cam_buffers[CAM_BUF_COUNT] = {};
static size_t s_cam_buf_lengths[CAM_BUF_COUNT] = {0};

static uint16_t *s_ppa_buf[2] = {NULL, NULL};
static uint8_t s_ppa_idx = 0;

static void *s_ui_canvas_buf = NULL;
static lv_obj_t *s_camera_canvas_obj = NULL;
static TaskHandle_t s_camera_task_handle = NULL;
static volatile bool s_ui_ready = false;

static volatile uint16_t s_touch_x = 0;
static volatile uint16_t s_touch_y = 0;
static volatile bool s_touch_pressed = false;
static lv_obj_t *s_touch_label = NULL;

extern "C" {
    i2c_master_bus_handle_t bsp_i2c_get_handle(void);
    esp_err_t bsp_i2c_init(void);
}

// 1. High-frequency non-blocking background touch worker
void touch_poll_task(void *pvParameters) {
    uint8_t status_reg[2] = {0x81, 0x4E};
    uint8_t point_reg[2]  = {0x81, 0x50};
    uint8_t clear_buf[3]  = {0x81, 0x4E, 0x00};

    while (1) {
        if (s_gt911_i2c_dev != NULL) {
            uint8_t status_val = 0;
            esp_err_t err = i2c_master_transmit_receive(s_gt911_i2c_dev, status_reg, 2, &status_val, 1, 10);

            if (err == ESP_OK && (status_val & 0x80)) {
                uint8_t touch_count = status_val & 0x0F;

                if (touch_count > 0 && touch_count <= 5) {
                    uint8_t point_buf[6] = {0};
                    if (i2c_master_transmit_receive(s_gt911_i2c_dev, point_reg, 2, point_buf, 6, 10) == ESP_OK) {
                        uint16_t raw_x = (uint16_t)(point_buf[0] | ((point_buf[1] & 0x0F) << 8));
                        uint16_t raw_y = (uint16_t)(point_buf[2] | ((point_buf[3] & 0x0F) << 8));

                        if (raw_x < 720 && raw_y < 1280) {
                            //s_touch_x = 1280 - raw_y;
                            //s_touch_y = raw_x;
                            // Flips origin (0,0) from Bottom-Right to Top-Left
                            s_touch_x = raw_y;
                            s_touch_y = 720 - raw_x;
                            s_touch_pressed = true;
                        } else {
                            s_touch_pressed = false;
                        }
                    } else {
                        s_touch_pressed = false;
                    }
                } else {
                    s_touch_pressed = false;
                }

                // Acknowledge read to clear touch register
                i2c_master_transmit(s_gt911_i2c_dev, clear_buf, 3, 10);
            }
        }
        vTaskDelay(pdMS_TO_TICKS(15)); // 66 Hz polling rate
    }
}

// -----------------------------------------------------------------------------
// LVGL 9 Callbacks & Task Loop
// -----------------------------------------------------------------------------

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

void process_camera_frame_task(void *pvParameters) {
    struct v4l2_buffer buf = {};
    buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = V4L2_MEMORY_MMAP;

    ESP_LOGI("CAM_TASK", "Camera capture task running on Core 1");

    while (s_camera_streaming) {
        if (ioctl(s_video_fd, VIDIOC_DQBUF, &buf) == 0) {
            void *cam_buf = s_cam_buffers[buf.index].start;

            if (s_ui_ready && s_camera_canvas_obj != NULL && s_ui_canvas_buf != NULL) {
                if (lvgl_port_lock(0)) {
                    ppa_srm_oper_config_t srm_cfg = {};

                    // 1. Input Image Configuration (OV5647 1280x720)
                    srm_cfg.in.buffer = cam_buf;
                    srm_cfg.in.pic_w = VideoConfig::SENSOR_WIDTH;
                    srm_cfg.in.pic_h = VideoConfig::SENSOR_HEIGHT;
                    srm_cfg.in.block_w = VideoConfig::SENSOR_WIDTH;
                    srm_cfg.in.block_h = VideoConfig::SENSOR_HEIGHT;
                    srm_cfg.in.block_offset_x = 0;
                    srm_cfg.in.block_offset_y = 0;
                    srm_cfg.in.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

                    // 2. Output Canvas Configuration (640x360 RGB565)
                    srm_cfg.out.buffer = s_ui_canvas_buf;
                    srm_cfg.out.buffer_size = VideoConfig::VIEWPORT_WIDTH * VideoConfig::VIEWPORT_HEIGHT * sizeof(uint16_t);
                    srm_cfg.out.pic_w = VideoConfig::VIEWPORT_WIDTH;
                    srm_cfg.out.pic_h = VideoConfig::VIEWPORT_HEIGHT;
                    srm_cfg.out.block_offset_x = 0;
                    srm_cfg.out.block_offset_y = 0;
                    srm_cfg.out.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

                    // 3. Precise 2:1 Scaling Ratios
                    srm_cfg.scale_x = (float)VideoConfig::VIEWPORT_WIDTH / (float)VideoConfig::SENSOR_WIDTH;   // 0.5f
                    srm_cfg.scale_y = (float)VideoConfig::VIEWPORT_HEIGHT / (float)VideoConfig::SENSOR_HEIGHT; // 0.5f
                    srm_cfg.rotation_angle = PPA_SRM_ROTATION_ANGLE_0;
                    srm_cfg.mirror_x = false;
                    srm_cfg.mirror_y = false;

                    // 4. Execute Hardware Scaling via ESP32-P4 PPA
                    esp_err_t ppa_err = ppa_do_scale_rotate_mirror(s_ppa_client, &srm_cfg);
                    if (ppa_err != ESP_OK) {
                        ESP_LOGE("CAM_TASK", "PPA scaling failed: 0x%x", ppa_err);
                    } else {
                        lv_obj_invalidate(s_camera_canvas_obj);
                    }

                    lvgl_port_unlock();
                }
            }
            // Re-queue buffer back to V4L2
            ioctl(s_video_fd, VIDIOC_QBUF, &buf);
        }

        vTaskDelay(pdMS_TO_TICKS(10));
    }

    vTaskDelete(NULL);
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

int32_t init_audio_system(void) {
    const uint32_t SAMPLE_RATE = 16000U;
    return init_i2s_duplex_c(SAMPLE_RATE, BoardPins::Audio::BCLK, BoardPins::Audio::WS, BoardPins::Audio::DIN, BoardPins::Audio::DOUT);
}

static void custom_touchpad_read(lv_indev_t *indev, lv_indev_data_t *data) {
    if (s_touch_pressed) {
        data->point.x = s_touch_x;
        data->point.y = s_touch_y;
        data->state = LV_INDEV_STATE_PRESSED;

        if (s_touch_label != NULL) {
            lv_label_set_text_fmt(s_touch_label, "Touch X: %d | Y: %d", s_touch_x, s_touch_y);
        }
    } else {
        data->state = LV_INDEV_STATE_RELEASED;
    }
}

int32_t init_display_system(void) {
    ESP_LOGI(TAG_LVGL, "Initializing Hardware via esp_lvgl_port...");

    // 1. Shared I2C0 Master Bus (GPIO 7 SDA / GPIO 8 SCL)
    if (!s_i2c_bus_handle) {
        i2c_master_bus_config_t i2c_bus_cfg = {
            .i2c_port = I2C_NUM_0,
            .sda_io_num = GPIO_NUM_7,
            .scl_io_num = GPIO_NUM_8,
            .clk_source = I2C_CLK_SRC_DEFAULT,
            .glitch_ignore_cnt = 7,
            .flags = { .enable_internal_pullup = true },
        };
        esp_err_t err = i2c_new_master_bus(&i2c_bus_cfg, &s_i2c_bus_handle);
        if (err != ESP_OK) {
            ESP_LOGE(TAG_LVGL, "Failed to create I2C master bus: 0x%x", err);
            return err;
        }
        ESP_LOGI(TAG_LVGL, "I2C Master Bus (I2C_NUM_0) created successfully!");
    }

    if (init_ppa_hardware_engine() != ESP_OK) {
        ESP_LOGE(TAG_LVGL, "Failed to initialize PPA hardware client!");
        return ESP_FAIL;
    }

    // 2. Power on MIPI-DSI PHY (2.5V on LDO Channel 3)
    esp_ldo_channel_handle_t ldo_mipi_phy = NULL;
    esp_ldo_channel_config_t ldo_cfg = {
        .chan_id = 3,
        .voltage_mv = 2500,
    };
    ESP_ERROR_CHECK(esp_ldo_acquire_channel(&ldo_cfg, &ldo_mipi_phy));
    vTaskDelay(pdMS_TO_TICKS(10));

    // 3. Initialize MIPI-DSI Bus
    esp_lcd_dsi_bus_handle_t dsi_bus = NULL;
    esp_lcd_dsi_bus_config_t bus_config = {
        .bus_id = 0,
        .num_data_lanes = 2,
        .phy_clk_src = MIPI_DSI_PHY_CLK_SRC_DEFAULT,
        .lane_bit_rate_mbps = 1000
    };
    ESP_ERROR_CHECK(esp_lcd_new_dsi_bus(&bus_config, &dsi_bus));

    // 4. Install MIPI DBI IO
    esp_lcd_panel_io_handle_t dbi_io = NULL;
    esp_lcd_dbi_io_config_t dbi_config = {
        .virtual_channel = 0,
        .lcd_cmd_bits = 8,
        .lcd_param_bits = 8,
    };
    ESP_ERROR_CHECK(esp_lcd_new_panel_io_dbi(dsi_bus, &dbi_config, &dbi_io));

    // 5. Configure DPI Timing
    esp_lcd_dpi_panel_config_t dpi_config = {};
    dpi_config.dpi_clk_src = MIPI_DSI_DPI_CLK_SRC_DEFAULT;
    dpi_config.dpi_clock_freq_mhz = 60;
    dpi_config.virtual_channel = 0;
    dpi_config.pixel_format = LCD_COLOR_PIXEL_FORMAT_RGB565;
    dpi_config.num_fbs = 2;
    dpi_config.flags.use_dma2d = true;
    dpi_config.video_timing.h_size = 720;
    dpi_config.video_timing.v_size = 1280;
    dpi_config.video_timing.hsync_back_porch = 40;
    dpi_config.video_timing.hsync_front_porch = 40;
    dpi_config.video_timing.hsync_pulse_width = 10;
    dpi_config.video_timing.vsync_back_porch = 16;
    dpi_config.video_timing.vsync_front_porch = 16;
    dpi_config.video_timing.vsync_pulse_width = 4;

    hx8394_vendor_config_t vendor_config = {};
    vendor_config.init_cmds = NULL;
    vendor_config.init_cmds_size = 0;
    vendor_config.mipi_config.dsi_bus = dsi_bus;
    vendor_config.mipi_config.dpi_config = &dpi_config;
    vendor_config.mipi_config.lane_num = 2;

    esp_lcd_panel_dev_config_t panel_dev_config = {};
    panel_dev_config.reset_gpio_num = -1;
    panel_dev_config.rgb_endian = LCD_RGB_ENDIAN_RGB;
    panel_dev_config.bits_per_pixel = 16;
    panel_dev_config.vendor_config = &vendor_config;

    // Power on & reset panel via IO expander at 0x45
    i2c_device_config_t io_exp_cfg = {};
    io_exp_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    io_exp_cfg.device_address = 0x45;
    io_exp_cfg.scl_speed_hz = 100000;

    i2c_master_dev_handle_t io_exp_dev = NULL;
    ESP_ERROR_CHECK(i2c_master_bus_add_device(s_i2c_bus_handle, &io_exp_cfg, &io_exp_dev));

    uint8_t write_buf[2];
    write_buf[0] = 0x95; write_buf[1] = 0x11;
    i2c_master_transmit(io_exp_dev, write_buf, 2, 100);

    write_buf[0] = 0x95; write_buf[1] = 0x17;
    i2c_master_transmit(io_exp_dev, write_buf, 2, 100);

    write_buf[0] = 0x96; write_buf[1] = 0x00;
    i2c_master_transmit(io_exp_dev, write_buf, 2, 100);

    vTaskDelay(pdMS_TO_TICKS(100));

    write_buf[0] = 0x96; write_buf[1] = 0xFF;
    i2c_master_transmit(io_exp_dev, write_buf, 2, 100);

    vTaskDelay(pdMS_TO_TICKS(100));

    i2c_master_bus_rm_device(io_exp_dev);

    ESP_ERROR_CHECK(esp_lcd_new_panel_hx8394(dbi_io, &panel_dev_config, &s_lcd_panel));
    ESP_ERROR_CHECK(esp_lcd_panel_reset(s_lcd_panel));
    ESP_ERROR_CHECK(esp_lcd_panel_init(s_lcd_panel));
    ESP_ERROR_CHECK(esp_lcd_panel_disp_on_off(s_lcd_panel, true));

    // 6. Initialize ESP-LVGL-PORT
    const lvgl_port_cfg_t port_cfg = ESP_LVGL_PORT_INIT_CONFIG();
    ESP_ERROR_CHECK(lvgl_port_init(&port_cfg));

    lvgl_port_display_cfg_t lvgl_disp_cfg = {};
    lvgl_disp_cfg.panel_handle = s_lcd_panel;
    lvgl_disp_cfg.io_handle = NULL;
    lvgl_disp_cfg.buffer_size = 720 * 40;
    lvgl_disp_cfg.double_buffer = true;
    lvgl_disp_cfg.hres = 720;
    lvgl_disp_cfg.vres = 1280;
    lvgl_disp_cfg.monochrome = false;
    lvgl_disp_cfg.flags.buff_spiram = true;
    lvgl_disp_cfg.flags.sw_rotate = true;

    lvgl_port_display_dsi_cfg_t dsi_cfg = {};
    dsi_cfg.flags.avoid_tearing = 0;

    lv_display_t *disp = lvgl_port_add_disp_dsi(&lvgl_disp_cfg, &dsi_cfg);
    if (!disp) {
        ESP_LOGE(TAG_LVGL, "Failed to create LVGL DSI display handle!");
        return ESP_FAIL;
    }

    lv_display_set_rotation(disp, LV_DISPLAY_ROTATION_270);

    // 7. Waveshare GT911 Device Setup
    if (s_gt911_i2c_dev == NULL) {
        i2c_device_config_t gt911_raw_cfg = {};
        gt911_raw_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
        gt911_raw_cfg.device_address = 0x5D;
        gt911_raw_cfg.scl_speed_hz = 100000;

        if (i2c_master_bus_add_device(s_i2c_bus_handle, &gt911_raw_cfg, &s_gt911_i2c_dev) == ESP_OK) {
            ESP_LOGI("GT911_TOUCH", "GT911 persistent I2C handle created successfully!");
            
            // Soft-reset payload
            uint8_t soft_reset_payload[3] = {0x80, 0x40, 0x02};
            i2c_master_transmit(s_gt911_i2c_dev, soft_reset_payload, 3, 100);
            vTaskDelay(pdMS_TO_TICKS(100));
        } else {
            ESP_LOGE("GT911_TOUCH", "Failed to add GT911 device to I2C bus!");
        }
    }

    // Register LVGL indev callback
    if (lvgl_port_lock(0)) {
        lv_indev_t *indev = lv_indev_create();
        if (indev) {
            lv_indev_set_type(indev, LV_INDEV_TYPE_POINTER);
            lv_indev_set_read_cb(indev, custom_touchpad_read);
            lv_indev_set_display(indev, disp);
            lv_indev_set_mode(indev, LV_INDEV_MODE_TIMER);

            ESP_LOGI("GT911_TOUCH", "Custom GT911 touch callback successfully registered!");
        }
        lvgl_port_unlock();
    }

    setup_split_screen_ui();

    // Spawn background poller on Core 1
    xTaskCreatePinnedToCore(touch_poll_task, "gt911_poller", 3072, NULL, 5, NULL, 1);

    return ESP_OK;
}

int32_t p4_camera_init_v4l2(uint16_t width, uint16_t height) {
    if (s_video_fd >= 0) return 0;

    ESP_LOGI(TAG_CAM, "Initializing OV5647 via esp_video at %dx%d...", width, height);

    if (!s_i2c_bus_handle) {
        ESP_LOGE(TAG_CAM, "I2C master bus not initialized! Run init_display_system first.");
        return -1;
    }

    // Hardware power-on and reset pulse for OV5647
    gpio_set_level(static_cast<gpio_num_t>(BoardPins::Camera::PWDN), 0);
    gpio_set_level(static_cast<gpio_num_t>(BoardPins::Camera::RESET), 0);
    vTaskDelay(pdMS_TO_TICKS(10));
    gpio_set_level(static_cast<gpio_num_t>(BoardPins::Camera::RESET), 1);
    vTaskDelay(pdMS_TO_TICKS(20));

    esp_video_init_csi_config_t csi_cfg = {};
    csi_cfg.sccb_config.init_sccb = false;
    csi_cfg.sccb_config.i2c_handle = s_i2c_bus_handle;
    csi_cfg.sccb_config.freq = 100000;
    csi_cfg.reset_pin = static_cast<gpio_num_t>(BoardPins::Camera::RESET);
    csi_cfg.pwdn_pin  = static_cast<gpio_num_t>(BoardPins::Camera::PWDN);
    csi_cfg.dont_init_ldo = false;

    esp_video_init_config_t cam_cfg = {};
    cam_cfg.csi = &csi_cfg;

    esp_err_t ret = esp_video_init(&cam_cfg);
    if (ret != ESP_OK) {
        ESP_LOGW(TAG_CAM, "esp_video_init failed: 0x%x. Continuing in headless camera mode...", ret);
        return 0; // Allows state machine loop to run
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

    // Register task on CPU 1 (Core 1) to leave CPU 0 free for LVGL/OS tasks
    BaseType_t res = xTaskCreatePinnedToCore(
        process_camera_frame_task, // Task function pointer
        "cam_frame_task",          // Task name
        8192,                      // Stack size in bytes
        NULL,                      // Task parameters
        5,                         // Priority (Higher priority for low-latency video)
        &s_camera_task_handle,     // Task handle pointer
        1                          // Core ID (CPU 1)
    );
    if (res != pdPASS) {
        ESP_LOGE("CAM_TASK", "Failed to spawn camera frame task!");
    }
    s_camera_streaming = true;

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
    frame->width = VideoConfig::SENSOR_WIDTH;   // Dynamically reads 1280
    frame->height = VideoConfig::SENSOR_HEIGHT; // Reads 720 or 960 from VideoConfig
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

    ret = p4_camera_init_v4l2(VideoConfig::SENSOR_WIDTH, VideoConfig::SENSOR_HEIGHT);
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
    if (lvgl_port_lock(100)) {
        lv_obj_t *scr = lv_screen_active();
        
        lv_obj_clean(scr);
        lv_obj_set_size(scr, VideoConfig::DISPLAY_WIDTH, VideoConfig::DISPLAY_HEIGHT);
        lv_obj_set_style_pad_all(scr, 0, LV_PART_MAIN);

        // Allocate PSRAM canvas buffer for 640x720 RGB565
        const size_t raw_buf_size = VideoConfig::VIEWPORT_WIDTH * VideoConfig::VIEWPORT_HEIGHT * sizeof(uint16_t);
        size_t aligned_canvas_buf_size = (raw_buf_size + C_LINE_SIZE - 1) & ~(C_LINE_SIZE - 1);

        if (!s_ui_canvas_buf) {
            s_ui_canvas_buf = heap_caps_aligned_alloc(C_LINE_SIZE, aligned_canvas_buf_size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
            if (s_ui_canvas_buf) {
                // Clear buffer to solid black (0x0000) to prevent white background artifacts
                memset(s_ui_canvas_buf, 0, aligned_canvas_buf_size);
            }
        }

        if (!s_ui_canvas_buf) {
            ESP_LOGE("UI", "Failed to allocate canvas buffer in PSRAM!");
            lvgl_port_unlock();
            return;
        }

        // 1. Create Left Viewport Canvas (640x720 spanning entire left half)
        s_camera_canvas_obj = lv_canvas_create(scr);
        lv_canvas_set_buffer(s_camera_canvas_obj, s_ui_canvas_buf, VideoConfig::VIEWPORT_WIDTH, VideoConfig::VIEWPORT_HEIGHT, LV_COLOR_FORMAT_RGB565);
        lv_obj_set_size(s_camera_canvas_obj, VideoConfig::VIEWPORT_WIDTH, VideoConfig::VIEWPORT_HEIGHT);
        lv_obj_set_pos(s_camera_canvas_obj, 0, 0);

        // 2. Create Right System Control Panel (640x720 at x=640)
        lv_obj_t *panel = lv_obj_create(scr);
        lv_obj_set_size(panel, VideoConfig::PANEL_WIDTH, VideoConfig::PANEL_HEIGHT);
        lv_obj_set_pos(panel, VideoConfig::VIEWPORT_WIDTH, 0);

        lv_obj_remove_flag(panel, LV_OBJ_FLAG_SCROLLABLE);
        lv_obj_set_style_pad_all(panel, 0, LV_PART_MAIN);
        lv_obj_set_style_border_width(panel, 0, LV_PART_MAIN);
        lv_obj_set_style_radius(panel, 0, LV_PART_MAIN);
        lv_obj_set_style_bg_color(panel, lv_color_hex(0x181818), LV_PART_MAIN);
        lv_obj_set_style_bg_opa(panel, LV_OPA_COVER, LV_PART_MAIN);

        // Title Label
        lv_obj_t *title = lv_label_create(panel);
        lv_label_set_text(title, "MULTIMODAL BIOMETRICS");
        lv_obj_set_style_text_color(title, lv_color_hex(0xFFFFFF), LV_PART_MAIN);
        lv_obj_set_style_text_opa(title, LV_OPA_COVER, LV_PART_MAIN);
        lv_obj_align(title, LV_ALIGN_TOP_MID, 0, 40);

        // Touch Label
        s_touch_label = lv_label_create(panel);
        if (s_touch_label) {
            lv_label_set_text(s_touch_label, "Touch: Idle");
            lv_obj_align(s_touch_label, LV_ALIGN_TOP_MID, 0, 15);
            
            static lv_style_t style_label;
            lv_style_init(&style_label);
            lv_style_set_text_color(&style_label, lv_color_hex(0x00FF00));
            lv_style_set_text_font(&style_label, LV_FONT_DEFAULT);
            lv_obj_add_style(s_touch_label, &style_label, 0);
        }

        s_ui_ready = true;
        lvgl_port_unlock();
        ESP_LOGI("UI", "Split-screen UI setup complete. Canvas Obj: %p", (void*)s_camera_canvas_obj);
    } else {
        ESP_LOGE("UI", "Failed to acquire LVGL port lock for UI setup!");
    }
}

void update_camera_viewport(const p4_camera_frame_t *frame) {
    if (!frame || !frame->data || !s_ui_ready || !s_camera_canvas_obj || !s_ui_canvas_buf) {
        return;
    }

    if (lvgl_port_lock(0)) {
        ppa_srm_oper_config_t srm_cfg = {};

        uint32_t in_w = VideoConfig::SENSOR_WIDTH;
        uint32_t in_h = VideoConfig::SENSOR_HEIGHT;

        // 1. Input Image Configuration (1280x720)
        srm_cfg.in.buffer = frame->data;
        srm_cfg.in.pic_w = in_w;          
        srm_cfg.in.pic_h = in_h;
        srm_cfg.in.block_w = in_h;          // crop height to be the same as width
        srm_cfg.in.block_h = in_h;
        srm_cfg.in.block_offset_x = (in_w - in_h) / 2;  // Offset X = 160 (crops 160px from left & right)
        srm_cfg.in.block_offset_y = 0;                  // Full vertical coverage
        srm_cfg.in.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

        // 2. Output Canvas Configuration (640x360)
        srm_cfg.out.buffer = s_ui_canvas_buf;
        srm_cfg.out.buffer_size = VideoConfig::VIEWPORT_WIDTH * VideoConfig::VIEWPORT_HEIGHT * sizeof(uint16_t);
        srm_cfg.out.pic_w = VideoConfig::VIEWPORT_WIDTH;
        srm_cfg.out.pic_h = VideoConfig::VIEWPORT_HEIGHT;
        srm_cfg.out.block_offset_x = 0;
        srm_cfg.out.block_offset_y = 0;
        srm_cfg.out.srm_cm = PPA_SRM_COLOR_MODE_RGB565;

        // 3. Precise Hardware Scaling Ratios (1280->640 [0.5x] and 960->360 [0.375x])
        srm_cfg.scale_x = (float)VideoConfig::VIEWPORT_WIDTH / (float)in_h;   // 0.666f
        srm_cfg.scale_y = (float)VideoConfig::VIEWPORT_HEIGHT / (float)in_h;  // 0.500f
        srm_cfg.rotation_angle = PPA_SRM_ROTATION_ANGLE_0;
        srm_cfg.mirror_x = false;
        srm_cfg.mirror_y = false;

        // 4. Run PPA hardware scale step
        esp_err_t ppa_err = ppa_do_scale_rotate_mirror(s_ppa_client, &srm_cfg);
        if (ppa_err == ESP_OK) {
            lv_obj_invalidate(s_camera_canvas_obj);
        } else {
            ESP_LOGE("PPA_VIEWPORT", "Scaling failed: 0x%x", ppa_err);
        }

        lvgl_port_unlock();
    }
}

} // extern "C"