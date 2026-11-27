#include <obs-module.h>
#include <obs-frontend-api.h>

#include "tiwi-dock.hpp"
#include "tiwi-output.hpp"
#include "tiwi-service.hpp"
#include "tiwi_obs.h"

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("tiwi-obs", "en-US")

namespace {

TiwiDock *g_dock = nullptr;

void tiwi_obs_log_callback(int level, const char *message) {
    if (!message) {
        return;
    }
    int obs_level = LOG_INFO;
    switch (level) {
    case 0:
        obs_level = LOG_ERROR;
        break;
    case 1:
        obs_level = LOG_WARNING;
        break;
    case 2:
        obs_level = LOG_INFO;
        break;
    default:
        obs_level = LOG_DEBUG;
        break;
    }
    blog(obs_level, "[tiwi] %s", message);
}

void on_frontend_event(enum obs_frontend_event event, void *) {
    if (g_dock) {
        g_dock->onFrontendEvent(event);
    }
}

} // namespace

bool obs_module_load(void) {
    tiwi_set_log_callback(tiwi_obs_log_callback);
    tiwi_output_register();
    tiwi_service_register();

    g_dock = new TiwiDock();
    obs_frontend_add_dock_by_id("tiwi_dock", "Tiwi", g_dock);
    obs_frontend_add_event_callback(on_frontend_event, nullptr);

    blog(LOG_INFO, "[tiwi] OBS plugin loaded");
    return true;
}

void obs_module_unload(void) {
    obs_frontend_remove_event_callback(on_frontend_event, nullptr);
    if (g_dock) {
        g_dock->shutdown();
        g_dock = nullptr;
    }
    tiwi_set_log_callback(nullptr);
    blog(LOG_INFO, "[tiwi] OBS plugin unloaded");
}
