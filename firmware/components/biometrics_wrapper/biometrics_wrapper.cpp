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
#include "esp_codec_dev.h"
#include "esp_codec_dev_defaults.h"
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
    // On-board ES8311 codec and NS4150B amplifier (Waveshare ESP32-P4-NANO schematic).
    namespace Audio {
        constexpr gpio_num_t MCLK      = GPIO_NUM_13;
        constexpr gpio_num_t BCLK      = GPIO_NUM_12;
        constexpr gpio_num_t WS        = GPIO_NUM_10;
        constexpr gpio_num_t DOUT      = GPIO_NUM_9;  // to the codec's DAC
        constexpr gpio_num_t DIN       = GPIO_NUM_11; // from the codec's ADC (on-board microphone)
        constexpr gpio_num_t PA_ENABLE = GPIO_NUM_53; // amplifier enable, active high
    }
}

// -----------------------------------------------------------------------------
// Video Pipeline & Display Layout Constants
// -----------------------------------------------------------------------------
namespace VideoConfig {
    // OV5647 Native Sensor Stream Dimensions (DO NOT CHANGE FROM 960)
    constexpr uint16_t SENSOR_WIDTH   = P4_SENSOR_WIDTH;
    constexpr uint16_t SENSOR_HEIGHT  = P4_SENSOR_HEIGHT;

    // Display Geometry (1280x720 Landscape)
    constexpr uint16_t DISPLAY_WIDTH  = 1280;
    constexpr uint16_t DISPLAY_HEIGHT = 720;  

    // Split-Screen Layout Dimensions
    // Camera canvas: exactly the 4:3 image area, centred vertically in the 640x720 left column
    // (black screen background above/below). Keeping the canvas to the image means LVGL only
    // re-renders/rotates image pixels when a new frame is presented.
    constexpr uint16_t VIEWPORT_WIDTH  = P4_VIEWPORT_WIDTH;
    constexpr uint16_t VIEWPORT_HEIGHT = P4_VIEWPORT_HEIGHT;
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
#define TAG_I2S     "I2S_WRAPPER"

#define CAM_BUF_COUNT 3
#define C_LINE_SIZE 128               // ESP32-P4 L2 Cache Line Size (0x80)

// Global Subsystem Handles
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static i2c_master_bus_handle_t s_i2c_bus_handle = NULL;
static esp_codec_dev_handle_t g_codec_dev = NULL;

namespace AudioConfig {
    constexpr int   SPEAKER_VOLUME = 70;    // 0..100
    constexpr float MIC_GAIN_DB    = 30.0f;
}

// Creates the shared I2C0 master bus (GPIO 7 SDA / GPIO 8 SCL) on first use: touch, IO
// expander, camera and audio codec all sit on it.
static esp_err_t ensure_i2c_bus(void) {
    if (s_i2c_bus_handle) return ESP_OK;
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
        ESP_LOGE("p4_i2c", "Failed to create I2C master bus: 0x%x", err);
        return err;
    }
    ESP_LOGI("p4_i2c", "I2C Master Bus (I2C_NUM_0) created successfully!");
    return ESP_OK;
}
static i2c_master_dev_handle_t s_gt911_i2c_dev = NULL;


static i2s_chan_handle_t g_i2s_tx_handle = NULL;
static i2s_chan_handle_t g_i2s_rx_handle = NULL;


static bool s_hardware_initialized = false;

static volatile bool s_camera_streaming = false;
static int s_video_fd = -1;
struct v4l2_frame_buffer_t s_cam_buffers[CAM_BUF_COUNT] = {};

static void *s_ui_canvas_buf = NULL;
static lv_obj_t *s_camera_canvas_obj = NULL;
static volatile bool s_ui_ready = false;

static volatile uint16_t s_touch_x = 0;
static volatile uint16_t s_touch_y = 0;
// Panel coordinates as reported by the GT911 (native 720x1280 portrait); LVGL rotates these.
static volatile uint16_t s_touch_raw_x = 0;
static volatile uint16_t s_touch_raw_y = 0;
static volatile bool s_touch_pressed = false;
static lv_obj_t *s_touch_label = NULL;
static lv_obj_t *s_face_boxes[P4_UI_MAX_FACE_BOXES] = {};

// Right-hand panel widgets (created in setup_split_screen_ui, used under the LVGL lock)
static lv_obj_t *s_status_label = NULL;
static lv_obj_t *s_idle_view = NULL;
static lv_obj_t *s_admin_view = NULL;
static lv_obj_t *s_name_input = NULL;
static lv_obj_t *s_member_roller = NULL;
static size_t s_member_count = 0;
// Touch events for the Rust state machine: sent by LVGL callbacks, drained by p4_ui_poll_event.
static QueueHandle_t s_ui_events = NULL;
constexpr UBaseType_t UI_EVENT_QUEUE_DEPTH = 4;

extern "C" {
    i2c_master_bus_handle_t bsp_i2c_get_handle(void);
    esp_err_t bsp_i2c_init(void);
}

// 1. High-frequency non-blocking background touch worker
static void touch_poll_task(void *pvParameters) {
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
                            s_touch_raw_x = raw_x;
                            s_touch_raw_y = raw_y;
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
static int init_i2s_duplex_c(uint32_t sample_rate, int mclk_gpio, int bclk_gpio, int ws_gpio, int din_gpio, int dout_gpio) {
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
            .mclk = (gpio_num_t)mclk_gpio,
            .bclk = (gpio_num_t)bclk_gpio,
            .ws   = (gpio_num_t)ws_gpio,
            .dout = (gpio_num_t)dout_gpio,
            .din  = (gpio_num_t)din_gpio,
        },
    };
    ret = i2s_channel_init_std_mode(g_i2s_tx_handle, &tx_cfg);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_AUDIO, "Failed to init I2S TX channel: 0x%x", ret);
        return (int)ret;
    }

    // 4. Configure RX Channel (codec ADC data; clocks shared with TX)
    i2s_std_config_t rx_cfg = {
        .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
        .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
        // The same pins as TX: both directions of one I2S port share its clocks.
        .gpio_cfg = {
            .mclk = (gpio_num_t)mclk_gpio,
            .bclk = (gpio_num_t)bclk_gpio,
            .ws   = (gpio_num_t)ws_gpio,
            .dout = (gpio_num_t)dout_gpio,
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

// Configures the ES8311 over I2C (clocking from MCLK, DAC and ADC on, amplifier enabled) and
// opens it for 16-bit mono at `sample_rate`. The I2S channels must already exist.
static int init_codec(uint32_t sample_rate) {
    if (g_codec_dev) return 0;
    audio_codec_i2s_cfg_t i2s_cfg = {};
    i2s_cfg.port = I2S_NUM_0;
    i2s_cfg.rx_handle = g_i2s_rx_handle;
    i2s_cfg.tx_handle = g_i2s_tx_handle;
    const audio_codec_data_if_t *data_if = audio_codec_new_i2s_data(&i2s_cfg);

    audio_codec_i2c_cfg_t i2c_cfg = {};
    i2c_cfg.port = I2C_NUM_0;
    i2c_cfg.addr = ES8311_CODEC_DEFAULT_ADDR;
    i2c_cfg.bus_handle = s_i2c_bus_handle;
    const audio_codec_ctrl_if_t *ctrl_if = audio_codec_new_i2c_ctrl(&i2c_cfg);
    const audio_codec_gpio_if_t *gpio_if = audio_codec_new_gpio();
    if (!data_if || !ctrl_if || !gpio_if) {
        ESP_LOGE(TAG_AUDIO, "Failed to create the codec interfaces");
        return ESP_FAIL;
    }

    es8311_codec_cfg_t es_cfg = {};
    es_cfg.ctrl_if = ctrl_if;
    es_cfg.gpio_if = gpio_if;
    es_cfg.codec_mode = ESP_CODEC_DEV_WORK_MODE_BOTH;
    es_cfg.pa_pin = BoardPins::Audio::PA_ENABLE;
    es_cfg.use_mclk = true;
    es_cfg.hw_gain.pa_voltage = 5.0f;
    es_cfg.hw_gain.codec_dac_voltage = 3.3f;
    const audio_codec_if_t *codec_if = es8311_codec_new(&es_cfg);
    if (!codec_if) {
        ESP_LOGE(TAG_AUDIO, "ES8311 did not answer on I2C");
        return ESP_ERR_NOT_FOUND;
    }

    esp_codec_dev_cfg_t dev_cfg = {};
    dev_cfg.dev_type = ESP_CODEC_DEV_TYPE_IN_OUT;
    dev_cfg.codec_if = codec_if;
    dev_cfg.data_if = data_if;
    esp_codec_dev_handle_t dev = esp_codec_dev_new(&dev_cfg);
    if (!dev) return ESP_FAIL;

    esp_codec_dev_sample_info_t fs = {};
    fs.bits_per_sample = 16;
    fs.channel = 1;
    fs.sample_rate = sample_rate;
    int ret = esp_codec_dev_open(dev, &fs);
    if (ret != ESP_CODEC_DEV_OK) {
        ESP_LOGE(TAG_AUDIO, "Opening the codec failed: %d", ret);
        return ESP_FAIL;
    }
    esp_codec_dev_set_out_vol(dev, AudioConfig::SPEAKER_VOLUME);
    esp_codec_dev_set_in_gain(dev, AudioConfig::MIC_GAIN_DB);
    g_codec_dev = dev;
    ESP_LOGI(TAG_AUDIO, "ES8311 codec ready: volume %d, microphone gain %.0f dB", AudioConfig::SPEAKER_VOLUME,
             (double)AudioConfig::MIC_GAIN_DB);
    return 0;
}

// Blocks until `samples_to_read` samples have been read from the microphone.
int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms) {
    (void)i2s_port;
    (void)timeout_ms;
    if (!g_codec_dev || !out_buffer || !bytes_read) return -1;
    const int len = (int)(samples_to_read * sizeof(int16_t));
    int ret = esp_codec_dev_read(g_codec_dev, out_buffer, len);
    *bytes_read = ret == ESP_CODEC_DEV_OK ? (uint32_t)len : 0;
    return ret;
}

// Blocks until all samples have been handed to the I2S driver.
int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms) {
    (void)i2s_port;
    (void)timeout_ms;
    if (!g_codec_dev || !buffer) return -1;
    // esp_codec_dev takes a non-const buffer but does not modify it.
    return esp_codec_dev_write(g_codec_dev, const_cast<int16_t *>(buffer), (int)(sample_count * sizeof(int16_t)));
}

// -----------------------------------------------------------------------------
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

esp_cam_sensor_device_t *ov5647_detect(void *config);

bool p4_touch_is_pressed(void) {
    return s_touch_pressed;
}

int32_t init_audio_system(void) {
    const uint32_t SAMPLE_RATE = 16000U;
    // The codec is configured over the shared I2C bus.
    esp_err_t err = ensure_i2c_bus();
    if (err != ESP_OK) return err;
    int ret = init_i2s_duplex_c(SAMPLE_RATE, BoardPins::Audio::MCLK, BoardPins::Audio::BCLK, BoardPins::Audio::WS,
                                BoardPins::Audio::DIN, BoardPins::Audio::DOUT);
    if (ret != 0) return ret;
    return init_codec(SAMPLE_RATE);
}

static void custom_touchpad_read(lv_indev_t *indev, lv_indev_data_t *data) {
    if (s_touch_pressed) {
        // LVGL applies the display rotation to pointer input itself (lv_display_rotate_point),
        // so it needs the panel's own coordinates; the rotated ones are only for the label.
        data->point.x = s_touch_raw_x;
        data->point.y = s_touch_raw_y;
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
    esp_err_t bus_err = ensure_i2c_bus();
    if (bus_err != ESP_OK) return bus_err;

    // 2. Power on MIPI-DSI PHY (2.5V on LDO Channel 3)
    esp_ldo_channel_handle_t ldo_mipi_phy = NULL;
    esp_ldo_channel_config_t ldo_cfg = {
        .chan_id = 3,
        .voltage_mv = 2500,
    };
    ESP_RETURN_ON_ERROR(esp_ldo_acquire_channel(&ldo_cfg, &ldo_mipi_phy), TAG_LVGL, "esp_ldo_acquire_channel failed");
    vTaskDelay(pdMS_TO_TICKS(10));

    // 3. Initialize MIPI-DSI Bus
    // Lane rate, pixel clock and porches below are the HX8394 driver's own values
    // (HX8394_PANEL_BUS_DSI_2CH_CONFIG / HX8394_720_1280_PANEL_30HZ_DPI_CONFIG in
    // esp_lcd_hx8394.h). With other values (1000 Mbps, 60 MHz, porches 40/10/40 and 16/4/16) the
    // panel showed the frame about 150 columns off, with a dark strip along one edge.
    esp_lcd_dsi_bus_handle_t dsi_bus = NULL;
    esp_lcd_dsi_bus_config_t bus_config = {
        .bus_id = 0,
        .num_data_lanes = 2,
        .phy_clk_src = MIPI_DSI_PHY_CLK_SRC_DEFAULT,
        .lane_bit_rate_mbps = 700
    };
    ESP_RETURN_ON_ERROR(esp_lcd_new_dsi_bus(&bus_config, &dsi_bus), TAG_LVGL, "esp_lcd_new_dsi_bus failed");

    // 4. Install MIPI DBI IO
    esp_lcd_panel_io_handle_t dbi_io = NULL;
    esp_lcd_dbi_io_config_t dbi_config = {
        .virtual_channel = 0,
        .lcd_cmd_bits = 8,
        .lcd_param_bits = 8,
    };
    ESP_RETURN_ON_ERROR(esp_lcd_new_panel_io_dbi(dsi_bus, &dbi_config, &dbi_io), TAG_LVGL, "esp_lcd_new_panel_io_dbi failed");

    // 5. Configure DPI Timing
    esp_lcd_dpi_panel_config_t dpi_config = {};
    dpi_config.dpi_clk_src = MIPI_DSI_DPI_CLK_SRC_DEFAULT;
    dpi_config.dpi_clock_freq_mhz = 58;
    dpi_config.virtual_channel = 0;
    dpi_config.pixel_format = LCD_COLOR_PIXEL_FORMAT_RGB565;
    dpi_config.num_fbs = 2;
    dpi_config.flags.use_dma2d = true;
    dpi_config.video_timing.h_size = 720;
    dpi_config.video_timing.v_size = 1280;
    dpi_config.video_timing.hsync_back_porch = 20;
    dpi_config.video_timing.hsync_front_porch = 40;
    dpi_config.video_timing.hsync_pulse_width = 20;
    dpi_config.video_timing.vsync_back_porch = 10;
    dpi_config.video_timing.vsync_front_porch = 24;
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
    ESP_RETURN_ON_ERROR(i2c_master_bus_add_device(s_i2c_bus_handle, &io_exp_cfg, &io_exp_dev), TAG_LVGL, "i2c_master_bus_add_device failed");

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

    ESP_RETURN_ON_ERROR(esp_lcd_new_panel_hx8394(dbi_io, &panel_dev_config, &s_lcd_panel), TAG_LVGL, "esp_lcd_new_panel_hx8394 failed");
    ESP_RETURN_ON_ERROR(esp_lcd_panel_reset(s_lcd_panel), TAG_LVGL, "esp_lcd_panel_reset failed");
    ESP_RETURN_ON_ERROR(esp_lcd_panel_init(s_lcd_panel), TAG_LVGL, "esp_lcd_panel_init failed");
    ESP_RETURN_ON_ERROR(esp_lcd_panel_disp_on_off(s_lcd_panel, true), TAG_LVGL, "esp_lcd_panel_disp_on_off failed");

    // 6. Initialize ESP-LVGL-PORT
    lvgl_port_cfg_t port_cfg = ESP_LVGL_PORT_INIT_CONFIG();
    port_cfg.task_affinity = 0; // keep LVGL on Core 0; Core 1 runs the camera/inference pipeline
    ESP_RETURN_ON_ERROR(lvgl_port_init(&port_cfg), TAG_LVGL, "lvgl_port_init failed");

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
        ESP_LOGE(TAG_CAM, "esp_video_init failed: 0x%x", ret);
        return ret;
    }

    s_video_fd = open("/dev/video0", O_RDWR); // blocking DQBUF: the Rust camera thread sleeps until a frame arrives
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

        s_cam_buffers[i].start = mmap(NULL, buf.length, PROT_READ | PROT_WRITE, MAP_SHARED, s_video_fd, buf.m.offset);
        if (s_cam_buffers[i].start == MAP_FAILED) return -1;
        s_cam_buffers[i].length = buf.length;
        if (ioctl(s_video_fd, VIDIOC_QBUF, &buf) < 0) return -1;
    }

    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(s_video_fd, VIDIOC_STREAMON, &type) < 0) {
        ESP_LOGE(TAG_CAM, "Failed to start V4L2 stream");
        return -1;
    }

    s_camera_streaming = true;

    ESP_LOGI(TAG_CAM, "OV5647 Camera streaming successfully on /dev/video0 (%dx%d RGB565)!", 
             fmt.fmt.pix.width, fmt.fmt.pix.height);
    return 0;
}

int32_t p4_camera_capture_frame(p4_camera_frame_t *frame) {
    if (s_video_fd < 0 || !frame) return -1;

    struct v4l2_buffer buf = {};
    buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = V4L2_MEMORY_MMAP;

    int ret = ioctl(s_video_fd, VIDIOC_DQBUF, &buf);
    if (ret < 0) {
        return -1;
    }

    frame->data = (uint8_t *)s_cam_buffers[buf.index].start;
    // Never report more than was mapped: Rust builds a slice of this length.
    frame->data_len = buf.bytesused < s_cam_buffers[buf.index].length ? buf.bytesused : s_cam_buffers[buf.index].length;
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

int32_t p4_hardware_init_all(void) {
    ESP_LOGI(TAG_HW, "Starting Unified Hardware Bring-up...");

    if (s_hardware_initialized) return ESP_OK;

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

    // No reset pulse: the PHY is reset over MDIO by its driver. GPIO 53, which this code used
    // to pulse, is the audio amplifier's enable pin on this board.
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

    ret = esp_event_handler_register(ETH_EVENT, ESP_EVENT_ANY_ID, &eth_event_handler, NULL);
    if (ret != ESP_OK) return ret;

    return esp_eth_start(eth_handle);
}

// Queues a touch event for Rust. Runs in LVGL callbacks, so it must not block: if Rust has
// stopped draining the queue the event is dropped.
static void post_ui_event(uint8_t kind) {
    p4_ui_event_t event = {};
    event.kind = kind;
    if (kind == P4_UI_EVENT_ENROLL && s_name_input) {
        strlcpy(event.name, lv_textarea_get_text(s_name_input), sizeof(event.name));
    }
    if (kind == P4_UI_EVENT_DELETE) {
        if (s_member_count == 0 || !s_member_roller) {
            return; // the roller shows a placeholder, nothing to delete
        }
        event.selected = (uint16_t)lv_roller_get_selected(s_member_roller);
    }
    if (s_ui_events) {
        xQueueSend(s_ui_events, &event, 0);
    }
}

static void ui_event_cb(lv_event_t *e) {
    post_ui_event((uint8_t)(uintptr_t)lv_event_get_user_data(e));
}

static lv_obj_t *make_view(lv_obj_t *parent, int32_t y, int32_t height) {
    lv_obj_t *view = lv_obj_create(parent);
    lv_obj_remove_style_all(view);
    lv_obj_set_size(view, VideoConfig::PANEL_WIDTH, height);
    lv_obj_set_pos(view, 0, y);
    lv_obj_remove_flag(view, LV_OBJ_FLAG_SCROLLABLE);
    return view;
}

static void make_button(lv_obj_t *parent, const char *text, int32_t x, int32_t y, int32_t w,
                        uint8_t event_kind) {
    lv_obj_t *button = lv_button_create(parent);
    lv_obj_set_size(button, w, 56);
    lv_obj_set_pos(button, x, y);
    lv_obj_add_event_cb(button, ui_event_cb, LV_EVENT_CLICKED, (void *)(uintptr_t)event_kind);
    lv_obj_t *label = lv_label_create(button);
    lv_label_set_text(label, text);
    lv_obj_center(label);
}

// Status line plus the idle and admin views of the right-hand panel. Called under the LVGL lock.
static void build_control_panel(lv_obj_t *panel) {
    constexpr int32_t MARGIN = 20;
    constexpr int32_t STATUS_Y = 64;   // two lines of status text fit above the views
    constexpr int32_t VIEWS_Y = 140;
    constexpr int32_t VIEWS_HEIGHT = VideoConfig::PANEL_HEIGHT - VIEWS_Y;
    constexpr int32_t ROW = 68;        // button height plus gap
    constexpr int32_t KEYBOARD_Y = 3 * ROW;
    constexpr int32_t BUTTON_X = 430;
    constexpr int32_t BUTTON_WIDTH = VideoConfig::PANEL_WIDTH - BUTTON_X - MARGIN;
    constexpr int32_t FIELD_WIDTH = BUTTON_X - 2 * MARGIN;

    if (!s_ui_events) {
        s_ui_events = xQueueCreate(UI_EVENT_QUEUE_DEPTH, sizeof(p4_ui_event_t));
    }

    s_status_label = lv_label_create(panel);
    lv_label_set_long_mode(s_status_label, LV_LABEL_LONG_MODE_WRAP);
    lv_obj_set_width(s_status_label, VideoConfig::PANEL_WIDTH - 2 * MARGIN);
    lv_obj_set_pos(s_status_label, MARGIN, STATUS_Y);
    lv_obj_set_style_text_color(s_status_label, lv_color_hex(0xFFFFFF), LV_PART_MAIN);
#if LV_FONT_MONTSERRAT_28
    lv_obj_set_style_text_font(s_status_label, &lv_font_montserrat_28, LV_PART_MAIN);
#endif
    lv_label_set_text(s_status_label, "Starting...");

    // Idle view: just the way into the admin view.
    s_idle_view = make_view(panel, VIEWS_Y, VIEWS_HEIGHT);
    make_button(s_idle_view, "Admin", BUTTON_X, 0, BUTTON_WIDTH, P4_UI_EVENT_ADMIN);

    // Admin view: name field and member list on the left, Enroll / Delete / Done on the right,
    // keyboard along the bottom.
    s_admin_view = make_view(panel, VIEWS_Y, VIEWS_HEIGHT);
    lv_obj_add_flag(s_admin_view, LV_OBJ_FLAG_HIDDEN);

    s_name_input = lv_textarea_create(s_admin_view);
    lv_textarea_set_one_line(s_name_input, true);
    lv_textarea_set_max_length(s_name_input, P4_UI_NAME_MAX);
    lv_textarea_set_placeholder_text(s_name_input, "Name (optional)");
    lv_obj_set_size(s_name_input, FIELD_WIDTH, 56);
    lv_obj_set_pos(s_name_input, MARGIN, 0);
    make_button(s_admin_view, "Enroll", BUTTON_X, 0, BUTTON_WIDTH, P4_UI_EVENT_ENROLL);

    s_member_roller = lv_roller_create(s_admin_view);
    lv_roller_set_options(s_member_roller, "(no members)", LV_ROLLER_MODE_NORMAL);
    lv_obj_set_size(s_member_roller, FIELD_WIDTH, 2 * ROW - 12);
    lv_obj_set_pos(s_member_roller, MARGIN, ROW);
    make_button(s_admin_view, "Delete", BUTTON_X, ROW, BUTTON_WIDTH, P4_UI_EVENT_DELETE);
    make_button(s_admin_view, "Done", BUTTON_X, 2 * ROW, BUTTON_WIDTH, P4_UI_EVENT_EXIT);

    lv_obj_t *keyboard = lv_keyboard_create(s_admin_view);
    lv_obj_set_size(keyboard, VideoConfig::PANEL_WIDTH, VIEWS_HEIGHT - KEYBOARD_Y);
    lv_obj_align(keyboard, LV_ALIGN_BOTTOM_MID, 0, 0);
    lv_keyboard_set_textarea(keyboard, s_name_input);
    // The keyboard's OK key enrolls, like the Enroll button.
    lv_obj_add_event_cb(keyboard, ui_event_cb, LV_EVENT_READY,
                        (void *)(uintptr_t)P4_UI_EVENT_ENROLL);
}

void setup_split_screen_ui(void) {
    if (lvgl_port_lock(100)) {
        lv_obj_t *scr = lv_screen_active();
        
        lv_obj_clean(scr);
        lv_obj_set_size(scr, VideoConfig::DISPLAY_WIDTH, VideoConfig::DISPLAY_HEIGHT);
        lv_obj_set_style_pad_all(scr, 0, LV_PART_MAIN);
        // Black behind the camera canvas (fills the left column above/below the image).
        lv_obj_set_style_bg_color(scr, lv_color_hex(0x000000), LV_PART_MAIN);
        lv_obj_set_style_bg_opa(scr, LV_OPA_COVER, LV_PART_MAIN);

        // Allocate PSRAM canvas buffer for the 640x480 RGB565 image
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

        // 1. Create the camera canvas, centred vertically in the left column
        s_camera_canvas_obj = lv_canvas_create(scr);
        lv_canvas_set_buffer(s_camera_canvas_obj, s_ui_canvas_buf, VideoConfig::VIEWPORT_WIDTH, VideoConfig::VIEWPORT_HEIGHT, LV_COLOR_FORMAT_RGB565);
        lv_obj_set_size(s_camera_canvas_obj, VideoConfig::VIEWPORT_WIDTH, VideoConfig::VIEWPORT_HEIGHT);
        lv_obj_set_pos(s_camera_canvas_obj, 0, (VideoConfig::DISPLAY_HEIGHT - VideoConfig::VIEWPORT_HEIGHT) / 2);

        // Face overlay boxes: children of the canvas (so they are clipped to it), hidden until used.
        for (lv_obj_t *&box : s_face_boxes) {
            box = lv_obj_create(s_camera_canvas_obj);
            lv_obj_remove_style_all(box);
            lv_obj_set_style_border_width(box, 3, LV_PART_MAIN);
            lv_obj_set_style_border_color(box, lv_color_hex(0x00FF00), LV_PART_MAIN);
            lv_obj_set_style_border_opa(box, LV_OPA_COVER, LV_PART_MAIN);
            lv_obj_set_style_bg_opa(box, LV_OPA_TRANSP, LV_PART_MAIN);
            lv_obj_remove_flag(box, LV_OBJ_FLAG_CLICKABLE);
            lv_obj_add_flag(box, LV_OBJ_FLAG_HIDDEN);
        }

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
        lv_obj_align(title, LV_ALIGN_TOP_MID, 0, 32);

        // Touch Label
        s_touch_label = lv_label_create(panel);
        if (s_touch_label) {
            lv_label_set_text(s_touch_label, "Touch: Idle");
            lv_obj_align(s_touch_label, LV_ALIGN_TOP_MID, 0, 6);
            
            static lv_style_t style_label;
            lv_style_init(&style_label);
            lv_style_set_text_color(&style_label, lv_color_hex(0x00FF00));
            lv_style_set_text_font(&style_label, LV_FONT_DEFAULT);
            lv_obj_add_style(s_touch_label, &style_label, 0);
        }

        build_control_panel(panel);

        s_ui_ready = true;
        lvgl_port_unlock();
        ESP_LOGI("UI", "Split-screen UI setup complete. Canvas Obj: %p", (void*)s_camera_canvas_obj);
    } else {
        ESP_LOGE("UI", "Failed to acquire LVGL port lock for UI setup!");
    }
}

bool p4_ui_present_camera(const void *buf, uint32_t lock_timeout_ms) {
    if (!s_ui_ready || !s_camera_canvas_obj) {
        return false;
    }
    if (!lvgl_port_lock(lock_timeout_ms)) {
        return false; // LVGL busy: caller keeps filling the same back buffer
    }
    // Swap only the canvas' buffer pointer; LVGL renders under this lock, so the previously
    // presented buffer is no longer read once we return.
    void *target = buf ? const_cast<void *>(buf) : s_ui_canvas_buf;
    lv_canvas_set_buffer(s_camera_canvas_obj, target, VideoConfig::VIEWPORT_WIDTH,
                         VideoConfig::VIEWPORT_HEIGHT, LV_COLOR_FORMAT_RGB565);
    lv_obj_invalidate(s_camera_canvas_obj);
    lvgl_port_unlock();
    return true;
}

bool p4_ui_show_faces(const p4_ui_rect_t *rects, size_t count, uint32_t lock_timeout_ms) {
    if (!s_ui_ready || (count > 0 && !rects)) {
        return false;
    }
    if (!lvgl_port_lock(lock_timeout_ms)) {
        return false;
    }
    for (size_t i = 0; i < P4_UI_MAX_FACE_BOXES; ++i) {
        lv_obj_t *box = s_face_boxes[i];
        if (!box) {
            continue;
        }
        if (i < count) {
            lv_obj_set_pos(box, rects[i].x, rects[i].y);
            lv_obj_set_size(box, rects[i].w, rects[i].h);
            lv_obj_remove_flag(box, LV_OBJ_FLAG_HIDDEN);
        } else {
            lv_obj_add_flag(box, LV_OBJ_FLAG_HIDDEN);
        }
    }
    lvgl_port_unlock();
    return true;
}

bool p4_ui_poll_event(p4_ui_event_t *event) {
    return event && s_ui_events && xQueueReceive(s_ui_events, event, 0) == pdTRUE;
}

bool p4_ui_set_status(const char *text, uint32_t lock_timeout_ms) {
    if (!s_ui_ready || !s_status_label || !text) {
        return false;
    }
    if (!lvgl_port_lock(lock_timeout_ms)) {
        return false;
    }
    lv_label_set_text(s_status_label, text);
    lvgl_port_unlock();
    return true;
}

bool p4_ui_set_admin_mode(bool enabled, uint32_t lock_timeout_ms) {
    if (!s_ui_ready || !s_admin_view || !s_idle_view) {
        return false;
    }
    if (!lvgl_port_lock(lock_timeout_ms)) {
        return false;
    }
    lv_textarea_set_text(s_name_input, "");
    lv_obj_set_flag(s_admin_view, LV_OBJ_FLAG_HIDDEN, !enabled);
    lv_obj_set_flag(s_idle_view, LV_OBJ_FLAG_HIDDEN, enabled);
    lvgl_port_unlock();
    return true;
}

bool p4_ui_set_members(const char *names, size_t count, uint32_t lock_timeout_ms) {
    if (!s_ui_ready || !s_member_roller) {
        return false;
    }
    if (!lvgl_port_lock(lock_timeout_ms)) {
        return false;
    }
    const bool empty = count == 0 || !names || names[0] == '\0';
    s_member_count = empty ? 0 : count;
    lv_roller_set_options(s_member_roller, empty ? "(no members)" : names, LV_ROLLER_MODE_NORMAL);
    lv_textarea_set_text(s_name_input, "");
    lvgl_port_unlock();
    return true;
}

} // extern "C"