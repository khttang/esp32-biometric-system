#ifndef BIOMETRICS_WRAPPER_BINDINGS_H
#define BIOMETRICS_WRAPPER_BINDINGS_H

// Pull in all unified hardware, camera, neural, and audio declarations
#include "biometrics_wrapper.h"

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// Standalone / Legacy Peripherals (if still referenced by Rust)
// -----------------------------------------------------------------------------
int init_wifi_hosted_c(const char *ssid, const char *password, uint32_t timeout_ms);
bool is_wifi_connected_c(void);
int stop_wifi_hosted_c(void);

int32_t init_mipi_csi_camera_c(void);
int32_t init_mipi_dsi_display_c(uint16_t width, uint16_t height);

#ifdef __cplusplus
}
#endif

#endif // BIOMETRICS_WRAPPER_BINDINGS_H