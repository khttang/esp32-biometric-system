#include "driver/i2c_master.h"
#include "bindings.h"

#include <stdio.h>
#include <cstring>
#include "esp_log.h"
#include "driver/gpio.h"
#include "driver/i2s_std.h"
#include "driver/i2c_master.h"
#include "driver/isp_core.h"
#include "driver/ledc.h"
#include "esp_err.h"
#include "esp_check.h"
#include "esp_ldo_regulator.h"
#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_csi.h"
#include "esp_cam_ctlr_types.h"
#include "esp_cam_sensor.h"
#include "esp_lcd_mipi_dsi.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_panel_io.h"
#include "esp_lcd_panel_vendor.h"
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
#include "hal/color_types.h"

// Core esp-dl Headers
#include "dl_model_base.hpp"
#include "dl_tensor_base.hpp"

#include "biometrics_wrapper.h"

#define TAG_HW  "p4_hardware"
#define TAG_ETH "p4_ethernet"
#define TAG_CAM "p4_camera"
#define TAG_LCD "p4_display"
#define TAG_SCAN "i2c_scanner"
#define TAG_OTA "p4_ota"
#define TAG_FACENET "ESP_DL_FACENET"

// Shared Hardware Pinout (Waveshare ESP32-P4-NANO Schematic)
#define I2C_SDA_GPIO        GPIO_NUM_7
#define I2C_SCL_GPIO        GPIO_NUM_8
#define DSI_BL_PWM_GPIO     GPIO_NUM_6       // Backlight LEDC PWM
#define LCD_RST_GPIO        GPIO_NUM_27      // Direct Display Reset Pin on P4-NANO (Non-Expander revision)
#define PHY_RESET_GPIO      GPIO_NUM_53
#define ADMIN_BUTTON_GPIO   GPIO_NUM_0

#define TCA9554_I2C_ADDR         0x45
#define DISPLAY_BACKLIGHT_ADDR   0x45
#define TCA9554_CONFIG_REG_ADDR  0x03
#define TCA9554_OUTPUT_REG_ADDR  0x01
#define OV5647_SCCB_ADDR         0x36
#define GT911_I2C_ADDR           0x5D  // GT911 7-bit I2C address (0x5D is default; 0x14 if INT pin was held LOW on boot)

// Global Static Handles
static i2c_master_bus_handle_t s_i2c_bus = NULL;
static i2c_master_dev_handle_t s_tca9554_handle = NULL;
static i2c_master_dev_handle_t s_cam_sccb_handle = NULL;
static i2c_master_dev_handle_t s_backlight_handle = NULL;
static i2c_master_dev_handle_t s_gt911_handle = NULL;

static esp_ldo_channel_handle_t s_ldo_vo3_handle = NULL;
static esp_lcd_dsi_bus_handle_t s_dsi_bus_handle = NULL;
static esp_lcd_panel_handle_t s_lcd_panel = NULL;
static esp_lcd_panel_io_handle_t s_lcd_io = NULL;

static esp_cam_ctlr_handle_t s_csi_cam_handle = NULL;
static uint8_t *s_cam_frame_buffer = NULL;
static size_t s_cam_fb_size = 0;

static i2s_chan_handle_t rx_handle = NULL;
static i2s_chan_handle_t tx_handle = NULL;

static bool s_hardware_initialized = false;

static dl::Model *g_mobilefacenet_model = NULL;

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

static void got_ip_event_handler(void *arg, esp_event_base_t event_base,
                                 int32_t event_id, void *event_data) {
    ip_event_got_ip_t *event = (ip_event_got_ip_t *)event_data;
    const esp_netif_ip_info_t *ip_info = &event->ip_info;

    ESP_LOGI(TAG_ETH, "Ethernet Got IP Address:");
    ESP_LOGI(TAG_ETH, "  IP     : " IPSTR, IP2STR(&ip_info->ip));
    ESP_LOGI(TAG_ETH, "  Netmask: " IPSTR, IP2STR(&ip_info->netmask));
    ESP_LOGI(TAG_ETH, "  GW     : " IPSTR, IP2STR(&ip_info->gw));
}

// -----------------------------------------------------------------------------
// Audio Peripheral Drivers
// -----------------------------------------------------------------------------
int init_i2s_mic_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int din_gpio) {
    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG((i2s_port_t)i2s_port, I2S_ROLE_MASTER);
    chan_cfg.auto_clear = true;

    esp_err_t err = i2s_new_channel(&chan_cfg, NULL, &rx_handle);
    if (err != ESP_OK) return (int)err;

    i2s_std_config_t std_cfg = {
        .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
        .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_MONO),
        .gpio_cfg = {
            .mclk = I2S_GPIO_UNUSED,
            .bclk = (gpio_num_t)bclk_gpio,
            .ws   = (gpio_num_t)ws_gpio,
            .dout = I2S_GPIO_UNUSED,
            .din  = (gpio_num_t)din_gpio,
            .invert_flags = {
                .mclk_inv = false,
                .bclk_inv = false,
                .ws_inv   = false,
            },
        },
    };

    std_cfg.slot_cfg.slot_mask = I2S_STD_SLOT_LEFT;
    err = i2s_channel_init_std_mode(rx_handle, &std_cfg);
    if (err != ESP_OK) return (int)err;

    return (int)i2s_channel_enable(rx_handle);
}

int read_i2s_mic_c(int i2s_port, int16_t *out_buffer, uint32_t samples_to_read, uint32_t *bytes_read, uint32_t timeout_ms) {
    if (!rx_handle) return -1;
    size_t length_bytes = samples_to_read * sizeof(int16_t);
    size_t read_bytes = 0;

    esp_err_t err = i2s_channel_read(rx_handle, out_buffer, length_bytes, &read_bytes, timeout_ms / portTICK_PERIOD_MS);
    if (bytes_read) *bytes_read = (uint32_t)read_bytes;
    return (int)err;
}

int init_i2s_tx_c(int i2s_port, uint32_t sample_rate, int bclk_gpio, int ws_gpio, int dout_gpio) {
    i2s_chan_config_t chan_cfg = I2S_CHANNEL_DEFAULT_CONFIG((i2s_port_t)i2s_port, I2S_ROLE_MASTER);
    chan_cfg.auto_clear = true;

    esp_err_t err = i2s_new_channel(&chan_cfg, &tx_handle, NULL);
    if (err != ESP_OK) return (int)err;

    i2s_std_config_t std_cfg = {
        .clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(sample_rate),
        .slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_STEREO),
        .gpio_cfg = {
            .mclk = GPIO_NUM_NC,
            .bclk = (gpio_num_t)bclk_gpio,
            .ws   = (gpio_num_t)ws_gpio,
            .dout = (gpio_num_t)dout_gpio,
            .din  = GPIO_NUM_NC,
            .invert_flags = {
                .mclk_inv = false,
                .bclk_inv = false,
                .ws_inv   = false,
            },
        },
    };

    err = i2s_channel_init_std_mode(tx_handle, &std_cfg);
    if (err != ESP_OK) return (int)err;

    return (int)i2s_channel_enable(tx_handle);
}

int write_i2s_tx_c(int i2s_port, const int16_t *buffer, uint32_t sample_count, uint32_t timeout_ms) {
    if (!tx_handle) return -1;
    size_t length_bytes = sample_count * sizeof(int16_t);
    size_t bytes_written = 0;
    return (int)i2s_channel_write(tx_handle, buffer, length_bytes, &bytes_written, timeout_ms / portTICK_PERIOD_MS);
}

// -----------------------------------------------------------------------------
// Power & Hardware Initialization
// -----------------------------------------------------------------------------
static esp_err_t init_internal_ldo(void) {
    if (s_ldo_vo3_handle != NULL) return ESP_OK;

    ESP_LOGI(TAG_HW, "Acquiring internal LDO Channel 3 (2.5V) for MIPI D-PHY...");
    esp_ldo_channel_config_t ldo_cfg = {};
    ldo_cfg.chan_id = 3;
    ldo_cfg.voltage_mv = 2500;

    esp_err_t ret = esp_ldo_acquire_channel(&ldo_cfg, &s_ldo_vo3_handle);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to acquire LDO VO3: 0x%x (%s)", ret, esp_err_to_name(ret));
        return ret;
    }

    ESP_LOGI(TAG_HW, "LDO_VO3 acquired successfully!");
    return ESP_OK;
}

void scan_i2c_bus(i2c_master_bus_handle_t bus_handle) {
    if (bus_handle == NULL) {
        ESP_LOGE(TAG_SCAN, "Cannot scan: I2C master bus handle is NULL!");
        return;
    }

    ESP_LOGI(TAG_SCAN, "==================================================");
    ESP_LOGI(TAG_SCAN, "Starting I2C Bus Scan (0x01 to 0x7F)...");
    ESP_LOGI(TAG_SCAN, "==================================================");

    uint8_t devices_found = 0;

    for (uint8_t addr = 0x01; addr < 0x7F; addr++) {
        // Probe each address using esp_i2c_master_probe with a 50ms timeout
        esp_err_t ret = i2c_master_probe(bus_handle, addr, 50);

        if (ret == ESP_OK) {
            devices_found++;
            
            // Print known Waveshare P4-NANO devices if address matches
            const char *desc = "";
            switch (addr) {
                case 0x20: desc = " (Possible TCA9554 Expander / GT911 Touch)"; break;
                case 0x36: desc = " (OV5647 Camera SCCB)"; break;
                case 0x38: desc = " (Alternative TCA9554 / Touch Address)"; break;
                case 0x45: desc = " (Waveshare Panel Controller / Backlight IC)"; break;
                case 0x5D: desc = " (GT911 Touch Controller)"; break;
                case 0x14: desc = " (GT911 Alt Touch Address)"; break;
                default:   desc = " (Unknown Device)"; break;
            }

            ESP_LOGI(TAG_SCAN, " -> Found device at 7-bit address: 0x%02X%s", addr, desc);
        }
    }

    ESP_LOGI(TAG_SCAN, "==================================================");
    if (devices_found == 0) {
        ESP_LOGW(TAG_SCAN, "No I2C devices responded! Check SDA/SCL pins, pull-ups, or 3.3V power rail.");
    } else {
        ESP_LOGI(TAG_SCAN, "Scan complete: Total %d device(s) detected.", devices_found);
    }
    ESP_LOGI(TAG_SCAN, "==================================================");
}

static esp_err_t tca9554_write_reg(i2c_master_dev_handle_t dev, uint8_t reg, uint8_t val) {
    uint8_t buf[2] = {reg, val};
    return i2c_master_transmit(dev, buf, sizeof(buf), 100);
}

static esp_err_t set_display_backlight_i2c(uint8_t brightness_255) {
    if (!s_backlight_handle) {
        ESP_LOGE(TAG_HW, "Backlight handle is NULL!");
        return ESP_ERR_INVALID_STATE;
    }

    // Write 100% duty cycle (0xFF) to Reg 0x96 with infinite wait (-1 / portMAX_DELAY)
    uint8_t write_buf[2] = {0x96, brightness_255};
    esp_err_t ret = i2c_master_transmit(s_backlight_handle, write_buf, sizeof(write_buf), -1);
    
    if (ret == ESP_OK) {
        ESP_LOGI(TAG_HW, "Successfully sent Backlight duty 0x%02X to I2C 0x45 Reg 0x96", brightness_255);
    } else {
        ESP_LOGE(TAG_HW, "Failed to transmit backlight brightness over I2C: 0x%x (%s)", ret, esp_err_to_name(ret));
    }
    return ret;
}

static esp_err_t enable_board_power_rails(void) {
    ESP_LOGI(TAG_HW, "Configuring LEDC PWM for Waveshare PH2.0 Backlight Driver...");

    // 1. Configure LEDC Timer (5 kHz, 8-bit resolution)
    ledc_timer_config_t ledc_timer = {};
    ledc_timer.speed_mode       = LEDC_LOW_SPEED_MODE;
    ledc_timer.timer_num        = LEDC_TIMER_0;
    ledc_timer.duty_resolution  = LEDC_TIMER_8_BIT;
    ledc_timer.freq_hz          = 5000; // 5 kHz PWM
    ledc_timer.clk_cfg          = LEDC_AUTO_CLK;
    ESP_ERROR_CHECK(ledc_timer_config(&ledc_timer));

    // Candidate GPIO pins for Waveshare ESP32-P4-NANO display backlight
    const gpio_num_t bl_candidate_gpios[] = {
        GPIO_NUM_6,   // Standard DSI BL pin
        GPIO_NUM_27,  // Alternate DSI control pin
        GPIO_NUM_39,  // Waveshare P4-NANO schematic pin
        GPIO_NUM_40,  // Waveshare P4-NANO schematic pin
        GPIO_NUM_41   // Waveshare P4-NANO schematic pin
    };

    // 2. Drive PWM across all candidate backlight pins simultaneously
    for (size_t i = 0; i < sizeof(bl_candidate_gpios) / sizeof(bl_candidate_gpios[0]); i++) {
        ledc_channel_config_t ledc_channel = {};
        ledc_channel.speed_mode     = LEDC_LOW_SPEED_MODE;
        ledc_channel.channel        = (ledc_channel_t)i;
        ledc_channel.timer_sel      = LEDC_TIMER_0;
        ledc_channel.intr_type      = LEDC_INTR_DISABLE;
        ledc_channel.gpio_num       = bl_candidate_gpios[i];
        ledc_channel.duty           = 255; // 100% duty cycle (Full Brightness)
        ledc_channel.hpoint         = 0;
        
        esp_err_t ret = ledc_channel_config(&ledc_channel);
        if (ret == ESP_OK) {
            ESP_LOGI(TAG_HW, "LEDC PWM active on GPIO %d (Channel %d)", bl_candidate_gpios[i], i);
        }
    }

    ESP_LOGI(TAG_HW, "Backlight PWM signals initialized. Waiting for power rail stabilization...");
    vTaskDelay(pdMS_TO_TICKS(100));

    return ESP_OK;
}

// -----------------------------------------------------------------------------
// MIPI-DSI Display Initialization (HX8394)
// -----------------------------------------------------------------------------
static esp_err_t hx8394_write_cmd(esp_lcd_panel_io_handle_t io, uint8_t cmd, const uint8_t *param, size_t param_len) {
    return esp_lcd_panel_io_tx_param(io, cmd, param, param_len);
}

static esp_err_t hx8394_init_vendor_registers(esp_lcd_panel_io_handle_t io) {
    ESP_LOGI(TAG_HW, "Sending Full HX8394-D Power, Gamma & Timing Sequence...");

    // 1. SETEXTC: Enable Extension Command Access (Password: 0x11, 0x83, 0x94)
    const uint8_t extc[] = {0x11, 0x83, 0x94};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB9, extc, sizeof(extc)));

    // 2. SETMIPI: Configure 2 D-PHY Lanes & LP/HS Rx Parameters
    const uint8_t mipi_cfg[] = {0x63, 0x03, 0x68, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x40};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xBA, mipi_cfg, sizeof(mipi_cfg)));

    // 3. SETPOWER: Power Control & Internal LDO Voltages (VGH/VGL/VCOM)
    const uint8_t pwr_cfg[] = {
        0x48, 0x12, 0x8B, 0x0D, 0x01, 0x00, 0x08, 0x08, 
        0x00, 0x0F, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x01
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB1, pwr_cfg, sizeof(pwr_cfg)));

    // 4. SETDISP: Display Timing & Waveform Control
    const uint8_t disp_cfg[] = {
        0x00, 0x18, 0xC8, 0x05, 0x70, 0x00, 0x01, 0x00, 
        0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB2, disp_cfg, sizeof(disp_cfg)));

    // 5. SETCYC: Panel Driving Cycle / Clock Divider
    const uint8_t cyc_cfg[] = {0x80, 0x00, 0x00, 0x00, 0x1A, 0x1A, 0x88, 0x01};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xB4, cyc_cfg, sizeof(cyc_cfg)));

    // 6. SETPTBA: Panel Resolution Mapping (720 x 1280 Gate/Source assignment)
    const uint8_t panel_cfg[] = {0x00, 0x00, 0x10, 0x80, 0x00, 0x10, 0x00, 0x00};
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xD2, panel_cfg, sizeof(panel_cfg)));

    // 7. SETGAMMA: 2.2 Gamma Curve Correction for 24-bit/16-bit RGB IPS
    const uint8_t gamma_cfg[] = {
        0x00, 0x0A, 0x15, 0x1B, 0x1E, 0x21, 0x24, 0x22, 
        0x47, 0x56, 0x65, 0x66, 0x6E, 0x7C, 0x82, 0x88, 
        0x93, 0x9A, 0x9E, 0x00, 0x0A, 0x15, 0x1B, 0x1E, 
        0x21, 0x24, 0x22, 0x47, 0x56, 0x65, 0x66, 0x6E, 
        0x7C, 0x82, 0x88, 0x93, 0x9A, 0x9E
    };
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0xE0, gamma_cfg, sizeof(gamma_cfg)));

    // 8. Sleep Out (0x11) & Mandatory Power Stabilization Delay
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0x11, NULL, 0));
    vTaskDelay(pdMS_TO_TICKS(150)); // 150ms allows charge pumps to reach VGH (+15V) and VGL (-10V)

    // 9. Display On (0x29)
    ESP_ERROR_CHECK(hx8394_write_cmd(io, 0x29, NULL, 0));
    vTaskDelay(pdMS_TO_TICKS(50));

    ESP_LOGI(TAG_HW, "HX8394-D initialization sequence complete!");
    return ESP_OK;
}

static esp_err_t init_display(uint16_t width, uint16_t height) {
    ESP_LOGI(TAG_HW, "Initializing HX8394 MIPI-DSI Display (%dx%d)...", width, height);

    ESP_ERROR_CHECK(init_internal_ldo());
    ESP_ERROR_CHECK(init_i2c_bus());
    ESP_ERROR_CHECK(init_tca9554());
    ESP_ERROR_CHECK(enable_board_power_rails());

    // 1. DSI Bus Setup: 500 Mbps PHY lane rate for stable PLL lock
    esp_lcd_dsi_bus_config_t bus_config = {};
    bus_config.bus_id = 0;
    bus_config.num_data_lanes = 2;
    bus_config.phy_clk_src = MIPI_DSI_PHY_CLK_SRC_DEFAULT;
    bus_config.lane_bit_rate_mbps = 500; // Increased from 400 to 500 Mbps

    ESP_ERROR_CHECK(esp_lcd_new_dsi_bus(&bus_config, &s_dsi_bus_handle));

    // 2. DBI IO Setup for DCS command initialization
    esp_lcd_dbi_io_config_t dbi_config = {};
    dbi_config.virtual_channel = 0;
    dbi_config.lcd_cmd_bits = 8;
    dbi_config.lcd_param_bits = 8;

    ESP_ERROR_CHECK(esp_lcd_new_panel_io_dbi(s_dsi_bus_handle, &dbi_config, &s_lcd_io));
    ESP_ERROR_CHECK(hx8394_init_vendor_registers(s_lcd_io));

    // 3. DPI Video Streaming Setup: 27 MHz clock & HX8394 standard porches
    esp_lcd_dpi_panel_config_t dpi_config = {};
    dpi_config.virtual_channel = 0;
    dpi_config.dpi_clk_src = MIPI_DSI_DPI_CLK_SRC_DEFAULT;
    dpi_config.dpi_clock_freq_mhz = 27; // Increased from 20 to 27 MHz (~60Hz refresh)
    dpi_config.pixel_format = LCD_COLOR_PIXEL_FORMAT_RGB565;
    dpi_config.in_color_format = LCD_COLOR_FMT_RGB565;
    dpi_config.out_color_format = LCD_COLOR_FMT_RGB565;
    dpi_config.num_fbs = 1;

    // Correct HX8394 Porch Timings
    dpi_config.video_timing.h_size = width;            // 720
    dpi_config.video_timing.v_size = height;           // 1280
    dpi_config.video_timing.hsync_back_porch = 44;    // HBP
    dpi_config.video_timing.hsync_front_porch = 46;   // HFP
    dpi_config.video_timing.hsync_pulse_width = 10;   // HSA
    dpi_config.video_timing.vsync_back_porch = 16;    // VBP
    dpi_config.video_timing.vsync_front_porch = 18;   // VFP
    dpi_config.video_timing.vsync_pulse_width = 4;    // VSA

    ESP_ERROR_CHECK(esp_lcd_new_panel_dpi(s_dsi_bus_handle, &dpi_config, &s_lcd_panel));
    ESP_ERROR_CHECK(esp_lcd_panel_init(s_lcd_panel));

    ESP_LOGI(TAG_HW, "HX8394 MIPI-DSI Panel Active & Streaming!");
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// MIPI-CSI Camera Initialization
// -----------------------------------------------------------------------------
static bool IRAM_ATTR csi_on_trans_finished_cb(esp_cam_ctlr_handle_t handle, esp_cam_ctlr_trans_t *trans, void *user_data) {
    return false;
}

static esp_err_t init_camera(uint16_t width, uint16_t height) {
    ESP_LOGI(TAG_HW, "Initializing MIPI-CSI Camera (%dx%d)...", width, height);

    if (s_i2c_bus == NULL) {
        ESP_LOGE(TAG_HW, "I2C bus not initialized!");
        return ESP_ERR_INVALID_STATE;
    }

    if (s_cam_sccb_handle == NULL) {
        i2c_device_config_t cam_sccb_cfg = {};
        cam_sccb_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
        cam_sccb_cfg.device_address = OV5647_SCCB_ADDR;
        cam_sccb_cfg.scl_speed_hz = 100000;

        esp_err_t ret = i2c_master_bus_add_device(s_i2c_bus, &cam_sccb_cfg, &s_cam_sccb_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_HW, "Failed to add OV5647 SCCB device: 0x%x", ret);
            return ret;
        }
    }

    s_cam_fb_size = width * height * 2;
    if (s_cam_frame_buffer == NULL) {
        s_cam_frame_buffer = (uint8_t *)heap_caps_malloc(s_cam_fb_size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
        if (!s_cam_frame_buffer) {
            ESP_LOGE(TAG_CAM, "Failed to allocate camera framebuffer in PSRAM!");
            return ESP_ERR_NO_MEM;
        }
    }

    if (s_csi_cam_handle == NULL) {
        esp_cam_ctlr_csi_config_t csi_config = {};
        csi_config.ctlr_id = 0;
        csi_config.clk_src = MIPI_CSI_PHY_CLK_SRC_DEFAULT;
        csi_config.h_res = width;
        csi_config.v_res = height;
        csi_config.data_lane_num = 2;
        csi_config.lane_bit_rate_mbps = 800;
        csi_config.input_data_color_type = CAM_CTLR_COLOR_RAW10;
        csi_config.output_data_color_type = CAM_CTLR_COLOR_RAW10;
        csi_config.queue_items = 2;
        csi_config.byte_swap_en = false;

        esp_err_t ret = esp_cam_new_csi_ctlr(&csi_config, &s_csi_cam_handle);
        if (ret != ESP_OK) {
            ESP_LOGE(TAG_CAM, "esp_cam_new_csi_ctlr failed: 0x%x (%s)", ret, esp_err_to_name(ret));
            return ret;
        }

        esp_cam_ctlr_evt_cbs_t cbs = {
            .on_get_new_trans = NULL,
            .on_trans_finished = csi_on_trans_finished_cb,
        };
        ESP_ERROR_CHECK(esp_cam_ctlr_register_event_callbacks(s_csi_cam_handle, &cbs, NULL));

        ESP_ERROR_CHECK(esp_cam_ctlr_enable(s_csi_cam_handle));
        ESP_ERROR_CHECK(esp_cam_ctlr_start(s_csi_cam_handle));
    }

    ESP_LOGI(TAG_HW, "OV5647 MIPI-CSI Pipeline Active & Frame Buffer Ready!");
    return ESP_OK;
}

// -----------------------------------------------------------------------------
// Public Rust FFI Exports
// -----------------------------------------------------------------------------
extern "C" {

int init_i2c_bus(void) {
    if (s_i2c_bus != NULL) {
        return ESP_OK; // Idempotent check
    }

    ESP_LOGI(TAG_HW, "Initializing I2C Master Bus (I2C0)...");

    i2c_master_bus_config_t bus_config = {};
    bus_config.i2c_port = I2C_NUM_0;
    bus_config.sda_io_num = (gpio_num_t)I2C_SDA_GPIO;
    bus_config.scl_io_num = (gpio_num_t)I2C_SCL_GPIO;
    bus_config.clk_source = I2C_CLK_SRC_DEFAULT;
    bus_config.glitch_ignore_cnt = 7;
    bus_config.flags.enable_internal_pullup = true;

    ESP_ERROR_CHECK(i2c_new_master_bus(&bus_config, &s_i2c_bus));
    scan_i2c_bus(s_i2c_bus);

    return ESP_OK;
}

int init_tca9554(void) {
    if (s_i2c_bus == NULL) {
        ESP_LOGE(TAG_HW, "Cannot init TCA9554: I2C bus is NULL! Call init_i2c_bus() first.");
        return (int)-1;
    }
    if (s_tca9554_handle != NULL) {
        return ESP_OK; // Already initialized
    }

    ESP_LOGI(TAG_HW, "Attaching TCA9554 I/O Expander to I2C bus...");

    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = TCA9554_I2C_ADDR;
    dev_cfg.scl_speed_hz = 400000;

    esp_err_t err = i2c_master_bus_add_device(s_i2c_bus, &dev_cfg, &s_tca9554_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to add TCA9554 device to bus: 0x%x", err);
        return (int)err;
    }

    // Example TCA9554 configuration:
    // Reg 0x03 (Configuration): set pins 0-3 as outputs (0), pins 4-7 as inputs (1)
    uint8_t config_cmd[2] = {0x03, 0xF0};
    err = i2c_master_transmit(s_tca9554_handle, config_cmd, sizeof(config_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to set TCA9554 config register: 0x%x", err);
        return (int)err;
    }

    // Reg 0x01 (Output Port): drive pins 0-3 HIGH (turns on power/resets for display & camera)
    uint8_t output_cmd[2] = {0x01, 0x0F};
    err = i2c_master_transmit(s_tca9554_handle, output_cmd, sizeof(output_cmd), 1000);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to set TCA9554 output register: 0x%x", err);
        return (int)err;
    }

    ESP_LOGI(TAG_HW, "TCA9554 I/O Expander initialized successfully.");
    return (int)ESP_OK;
}

int init_gt911_device(void) {
    if (s_i2c_bus == NULL) {
        ESP_LOGE(TAG_HW, "Cannot init GT911: I2C bus is NULL! Call init_i2c_bus() first.");
        return (int)ESP_ERR_INVALID_STATE;
    }
    if (s_gt911_handle != NULL) {
        return (int)ESP_OK; // Already added
    }

    ESP_LOGI(TAG_HW, "Attaching GT911 Touch Controller to shared I2C bus...");

    i2c_device_config_t dev_cfg = {};
    dev_cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev_cfg.device_address = GT911_I2C_ADDR;
    dev_cfg.scl_speed_hz = 400000;

    esp_err_t err = i2c_master_bus_add_device(s_i2c_bus, &dev_cfg, &s_gt911_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG_HW, "Failed to add GT911 device to bus: 0x%x", err);
        return (int)err;
    }

    ESP_LOGI(TAG_HW, "GT911 touch device successfully attached to I2C bus.");
    return (int)ESP_OK;
}

int gt911_i2c_read(uint16_t reg, uint8_t *data, size_t len) {
    if (s_gt911_handle == NULL) {
        ESP_LOGE(TAG_HW, "GT911 device not initialized for read");
        return (int)ESP_ERR_INVALID_STATE;
    }
    if (data == NULL || len == 0) {
        return (int)ESP_ERR_INVALID_ARG;
    }

    // GT911 uses 16-bit register addresses in big-endian order
    uint8_t reg_buf[2] = {
        (uint8_t)(reg >> 8),
        (uint8_t)(reg & 0xFF)
    };

    // Write 2-byte register address, then read 'len' bytes back
    esp_err_t err = i2c_master_transmit_receive(
        s_gt911_handle,
        reg_buf,
        sizeof(reg_buf),
        data,
        len,
        1000 // 1 sec timeout
    );

    return (int)err;
}

int gt911_i2c_write(uint16_t reg, uint8_t val) {
    if (s_gt911_handle == NULL) {
        ESP_LOGE(TAG_HW, "GT911 device not initialized for write");
        return (int)ESP_ERR_INVALID_STATE;
    }

    // Packet: [Reg High, Reg Low, Value]
    uint8_t tx_buf[3] = {
        (uint8_t)(reg >> 8),
        (uint8_t)(reg & 0xFF),
        val
    };

    esp_err_t err = i2c_master_transmit(
        s_gt911_handle,
        tx_buf,
        sizeof(tx_buf),
        1000 // 1 sec timeout
    );

    return (int)err;
}

esp_err_t p4_perform_ota_update(const char *url) {
    if (url == NULL) {
        ESP_LOGE(TAG_OTA, "Invalid OTA URL parameter");
        return ESP_ERR_INVALID_ARG;
    }

    ESP_LOGI(TAG_OTA, "Starting Over-The-Air (OTA) update from: %s", url);

    // 1. Configure the HTTP/HTTPS client for OTA
    esp_http_client_config_t http_config = {};
    http_config.url = url;
    http_config.timeout_ms = 15000;
    http_config.keep_alive_enable = true;

    esp_https_ota_config_t ota_config = {};
    ota_config.http_config = &http_config;

    // 2. Perform the OTA stream and write process
    esp_err_t ret = esp_https_ota(&ota_config);
    if (ret == ESP_OK) {
        ESP_LOGI(TAG_OTA, "OTA Update successfully written to flash!");
        ESP_LOGI(TAG_OTA, "Rebooting into new firmware partition in 2 seconds...");
        vTaskDelay(pdMS_TO_TICKS(2000));
        esp_restart(); // Reboot into the newly updated partition
    } else {
        ESP_LOGE(TAG_OTA, "OTA Update failed with error code: 0x%x (%s)", ret, esp_err_to_name(ret));
    }

    return ret;
}

// Call this on app startup to validate the new image and prevent automatic rollback
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

void p4_display_draw_test_pattern(void) {
    if (!s_lcd_panel) return;

    size_t buffer_size = 720 * 1280 * sizeof(uint16_t);
    uint16_t *test_buf = (uint16_t *)heap_caps_malloc(buffer_size, MALLOC_CAP_SPIRAM);

    if (test_buf) {
        // Bright Solid Cyan (RGB565: 0x07FF)
        for (int i = 0; i < 720 * 1280; i++) {
            test_buf[i] = 0x07FF; 
        }

        ESP_LOGI(TAG_HW, "Pushing Solid Cyan Test Frame to Display...");
        esp_lcd_panel_draw_bitmap(s_lcd_panel, 0, 0, 720, 1280, test_buf);
        heap_caps_free(test_buf);
    }
}

int32_t p4_hardware_init_all(const p4_hardware_config_t *config) {
    ESP_LOGI(TAG_HW, "Starting Unified Hardware Bring-up...");

    if (s_hardware_initialized) {
        ESP_LOGW(TAG_HW, "Hardware already initialized, skipping duplicate init.");
        return ESP_OK;
    }

    if (!config) {
        ESP_LOGE(TAG_HW, "Null configuration pointer passed!");
        return -1;
    }

    // 1. Bring up Display
    esp_err_t ret = init_display(config->display_width, config->display_height);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_HW, "Display bring-up failed!");
        return ret;
    }

    // 2. Bring up Camera
    ret = init_camera(config->camera_width, config->camera_height);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_HW, "Camera bring-up failed!");
        return ret;
    }

    ESP_LOGI(TAG_HW, "All Systems Initialized Successfully!");
    s_hardware_initialized = true;

    p4_display_draw_test_pattern();
    return ESP_OK;
}

int32_t p4_camera_capture_frame(p4_camera_frame_t *frame, uint32_t timeout_ms) {
    if (!s_csi_cam_handle || !s_cam_frame_buffer || !frame) {
        ESP_LOGE(TAG_CAM, "Capture failed: handles or frame buffer NULL!");
        return -1;
    }

    esp_cam_ctlr_trans_t trans = {};
    trans.buffer = s_cam_frame_buffer;
    trans.buflen = s_cam_fb_size;

    esp_err_t ret = esp_cam_ctlr_receive(s_csi_cam_handle, &trans, pdMS_TO_TICKS(timeout_ms));

    if (ret == ESP_OK) {
        frame->data = (uint8_t *)trans.buffer;
        frame->data_len = (trans.received_size > 0) ? trans.received_size : s_cam_fb_size;
        frame->width = 1280;
        frame->height = 720;
        
        ESP_LOGI(TAG_CAM, "Captured frame: %zu bytes from OV5647!", frame->data_len);
        return 0;
    } else {
        ESP_LOGE(TAG_CAM, "Camera frame capture timed out / failed: 0x%x (%s)", ret, esp_err_to_name(ret));
        return (int32_t)ret;
    }
}

int run_face_inference(
    const uint8_t* frame_rgb888, 
    size_t frame_len, 
    const uint8_t* model_bytes, 
    FaceEmbedding* embedding_out
) {
    if (!frame_rgb888 || !model_bytes || !embedding_out) return -1;

    dl::Model *model = new dl::Model((const char*)model_bytes);
    auto inputs = model->get_inputs();
    if (inputs.empty()) {
        delete model;
        return -2;
    }

    dl::TensorBase *input_tensor = inputs.begin()->second;
    std::memcpy(input_tensor->data, frame_rgb888, frame_len);

    model->run();

    auto outputs = model->get_outputs();
    if (outputs.empty()) {
        delete model;
        return -3;
    }

    dl::TensorBase *output_tensor = outputs.begin()->second;
    float *output_data = (float*)output_tensor->data;

    size_t elem_count = output_tensor->get_size();
    if (elem_count > 512) elem_count = 512;
    std::memcpy(embedding_out->values, output_data, elem_count * sizeof(float));

    delete model;
    return 0;
}

int32_t init_p4_ethernet(void) {
    ESP_LOGI(TAG_ETH, "Initializing Waveshare ESP32-P4-NANO EMAC Ethernet...");

    // 1. Hardware Reset PHY (Toggle GPIO 53 LOW -> HIGH)
    gpio_config_t rst_cfg = {};
    rst_cfg.pin_bit_mask = (1ULL << PHY_RESET_GPIO);
    rst_cfg.mode = GPIO_MODE_OUTPUT;
    gpio_config(&rst_cfg);

    gpio_set_level(PHY_RESET_GPIO, 0);
    vTaskDelay(pdMS_TO_TICKS(50));
    gpio_set_level(PHY_RESET_GPIO, 1);
    vTaskDelay(pdMS_TO_TICKS(100));

    // 2. Initialize Netif & Event Loop
    esp_err_t ret = esp_netif_init();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) {
        ESP_LOGE(TAG_ETH, "Failed to init netif: %s", esp_err_to_name(ret));
        return ret;
    }

    ret = esp_event_loop_create_default();
    if (ret != ESP_OK && ret != ESP_ERR_INVALID_STATE) {
        ESP_LOGE(TAG_ETH, "Failed to create event loop: %s", esp_err_to_name(ret));
        return ret;
    }

    esp_netif_config_t cfg = ESP_NETIF_DEFAULT_ETH();
    esp_netif_t *eth_netif = esp_netif_new(&cfg);

    // 3. Waveshare ESP32-P4-NANO Physical Wiring Configuration
    eth_mac_config_t mac_config = ETH_MAC_DEFAULT_CONFIG();
    eth_esp32_emac_config_t emac_config = {};

    // Management Interface (SMI)
    emac_config.smi_gpio.mdc_num  = GPIO_NUM_31;
    emac_config.smi_gpio.mdio_num = GPIO_NUM_52;

    // Automatically pull up whichever pin is assigned to MDIO:
    gpio_set_pull_mode((gpio_num_t)emac_config.smi_gpio.mdio_num, GPIO_PULLUP_ONLY);

    // RMII Reference Clock (50MHz External Oscillator on GPIO 52)
    emac_config.interface = EMAC_DATA_INTERFACE_RMII;
    emac_config.clock_config.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config.rmii.clock_gpio = (emac_rmii_clock_gpio_t)GPIO_NUM_50;

    // DMA & Interrupts
    emac_config.dma_burst_len = ETH_DMA_BURST_LEN_32;
    emac_config.intr_priority = 0;

    // RMII Data Lines (Physical Trace Pins)
    emac_config.emac_dataif_gpio.rmii.tx_en_num  = GPIO_NUM_49;
    emac_config.emac_dataif_gpio.rmii.txd0_num   = GPIO_NUM_34;
    emac_config.emac_dataif_gpio.rmii.txd1_num   = GPIO_NUM_35;
    emac_config.emac_dataif_gpio.rmii.crs_dv_num = GPIO_NUM_28;
    emac_config.emac_dataif_gpio.rmii.rxd0_num   = GPIO_NUM_29;
    emac_config.emac_dataif_gpio.rmii.rxd1_num   = GPIO_NUM_30;

    // Fallback Clock Input/Output
    emac_config.clock_config_out_in.rmii.clock_mode = EMAC_CLK_EXT_IN;
    emac_config.clock_config_out_in.rmii.clock_gpio = -1;

    // 4. Create ESP32 EMAC Instance
    esp_eth_mac_t *mac = esp_eth_mac_new_esp32(&emac_config, &mac_config);
    if (!mac) {
        ESP_LOGE(TAG_ETH, "Failed to create EMAC instance");
        return -1;
    }

    /*
    // DIRECT HARDWARE SMI BUS SCAN
    ESP_LOGI(TAG_ETH, "Direct SMI Register Read Scan (MDC=31, MDIO=27)...");
    for (int addr = 0; addr < 32; addr++) {
        uint32_t phy_id1 = 0;
        if (mac->read_phy_reg(mac, addr, 2, &phy_id1) == ESP_OK) {
            if (phy_id1 != 0xFFFF && phy_id1 != 0x0000) {
                ESP_LOGI(TAG_ETH, ">>> REAL PHY DETECTED at address %d! Reg 0x02 = 0x%04X <<<", 
                        addr, (unsigned int)phy_id1);
            }
        }
    }
    */

    // 5. Create PHY Instance (P4-NANO uses IP101GRI)
    eth_phy_config_t phy_config = ETH_PHY_DEFAULT_CONFIG();
    phy_config.phy_addr = 1;
    phy_config.reset_gpio_num = -1; // Already toggled manually above on GPIO 53

    esp_eth_phy_t *phy = esp_eth_phy_new_ip101(&phy_config);
    if (!phy) {
        ESP_LOGE(TAG_ETH, "Failed to create IP101 PHY instance");
        return -1;
    }

    // 6. Install & Start Ethernet Driver
    esp_eth_config_t eth_config = ETH_DEFAULT_CONFIG(mac, phy);
    esp_eth_handle_t eth_handle = NULL;
    ret = esp_eth_driver_install(&eth_config, &eth_handle);
    if (ret != ESP_OK) {
        ESP_LOGE(TAG_ETH, "Driver install failed: %s", esp_err_to_name(ret));
        return ret;
    }

    ret = esp_netif_attach(eth_netif, esp_eth_new_netif_glue(eth_handle));
    if (ret != ESP_OK) return ret;

    return esp_eth_start(eth_handle);
}

int init_admin_button_gpio() {
    gpio_config_t io_conf = {};
    io_conf.pin_bit_mask = (1ULL << ADMIN_BUTTON_GPIO);
    io_conf.mode = GPIO_MODE_INPUT;
    io_conf.pull_up_en = GPIO_PULLUP_ENABLE;
    io_conf.pull_down_en = GPIO_PULLDOWN_DISABLE;
    io_conf.intr_type = GPIO_INTR_DISABLE;

    return (int)gpio_config(&io_conf);
}

bool is_admin_button_pressed() {
    return gpio_get_level(ADMIN_BUTTON_GPIO) == 0;
}

esp_err_t dl_mobilefacenet_init(const uint8_t *model_buf, size_t model_size) {
    if (!model_buf || model_size == 0) {
        ESP_LOGE(TAG_FACENET, "Invalid model buffer or size");
        return ESP_ERR_INVALID_ARG;
    }

    try {
        // Construct ESP-DL model directly from memory-mapped flash pointer
        // Note: Default parameter automatically uses FLASH_RODATA
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

esp_err_t dl_mobilefacenet_run(const uint8_t *crop_rgb888, float *out_embedding, size_t embedding_len) {
    if (!g_mobilefacenet_model) {
        ESP_LOGE(TAG_FACENET, "Model not initialized! Call dl_mobilefacenet_init first.");
        return ESP_ERR_INVALID_STATE;
    }

    if (!crop_rgb888 || !out_embedding || embedding_len < 512) {
        return ESP_ERR_INVALID_ARG;
    }

    // Step A: Get reference to input tensor (Shape: [1, 112, 112, 3])
    std::map<std::string, dl::TensorBase *> &inputs = g_mobilefacenet_model->get_inputs();
    if (inputs.empty()) {
        ESP_LOGE(TAG_FACENET, "Model has no input tensors!");
        return ESP_FAIL;
    }

    dl::TensorBase *input_tensor = inputs.begin()->second;

    void *tensor_buf = input_tensor->get_element_ptr();
    if (!tensor_buf) {
        ESP_LOGE(TAG_FACENET, "Input tensor memory buffer is null!");
        return ESP_FAIL;
    }

    // Fix 2: Use get_bytes() to get total tensor memory size
    size_t input_bytes = input_tensor->get_bytes();
    std::memcpy(tensor_buf, crop_rgb888, input_bytes);

    // Step B: Run hardware-accelerated forward pass on ESP32-P4 PIE engine
    g_mobilefacenet_model->run();

    // Step C: Retrieve output tensor
    std::map<std::string, dl::TensorBase *> &outputs = g_mobilefacenet_model->get_outputs();
    if (outputs.empty()) {
        ESP_LOGE(TAG_FACENET, "Model has no output tensors!");
        return ESP_FAIL;
    }

    dl::TensorBase *output_tensor = outputs.begin()->second;
    int8_t *quant_data = (int8_t *)output_tensor->get_element_ptr();

    int exponent = output_tensor->get_exponent();
    float scale = std::pow(2.0f, exponent);

    // Step D: Dequantize INT8 values to float & calculate L2 norm squared
    float sum_squares = 0.0f;
    for (size_t i = 0; i < 512; i++) {
        float dequant_val = (float)quant_data[i] * scale;
        out_embedding[i] = dequant_val;
        sum_squares += dequant_val * dequant_val;
    }

    // Step E: Apply L2 Normalization
    float l2_norm = std::sqrt(sum_squares);
    if (l2_norm > 1e-6f) {
        float inv_norm = 1.0f / l2_norm;
        for (size_t i = 0; i < 512; i++) {
            out_embedding[i] *= inv_norm;
        }
    }

    return ESP_OK;
}

}