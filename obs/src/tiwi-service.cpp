#include "tiwi-service.hpp"

#include <obs-module.h>

namespace {

const char *tiwi_service_get_name(void *) {
    return "Tiwi";
}

void *tiwi_service_create(obs_data_t *, obs_service_t *) {
    return new int(0);
}

void tiwi_service_destroy(void *data) {
    delete static_cast<int *>(data);
}

const char *tiwi_service_get_output_type(void *) {
    return "tiwi_output";
}

const char *tiwi_service_get_protocol(void *) {
    return "TIWI";
}

const char **tiwi_service_supported_video_codecs(void *) {
    static const char *codecs[] = {"h264", "hevc", "av1", nullptr};
    return codecs;
}

const char **tiwi_service_supported_audio_codecs(void *) {
    static const char *codecs[] = {"opus", nullptr};
    return codecs;
}

bool tiwi_service_can_try_to_connect(void *) {
    return true;
}

void tiwi_service_apply_encoder_settings(
    void *,
    obs_data_t *video_encoder_settings,
    obs_data_t *
) {
    if (!video_encoder_settings) {
        return;
    }
    // WebRTC requires decode order == presentation order (no B-frames) and periodic keyframes.
    obs_data_set_int(video_encoder_settings, "bf", 0);
    obs_data_set_bool(video_encoder_settings, "repeat_headers", true);
    if (obs_data_get_int(video_encoder_settings, "keyint_sec") <= 0 ||
        obs_data_get_int(video_encoder_settings, "keyint_sec") > 2) {
        obs_data_set_int(video_encoder_settings, "keyint_sec", 2);
    }
}

} // namespace

void tiwi_service_register() {
    obs_service_info info = {};
    info.id = "tiwi_service";
    info.get_name = tiwi_service_get_name;
    info.create = tiwi_service_create;
    info.destroy = tiwi_service_destroy;
    info.get_output_type = tiwi_service_get_output_type;
    info.get_protocol = tiwi_service_get_protocol;
    info.get_supported_video_codecs = tiwi_service_supported_video_codecs;
    info.get_supported_audio_codecs = tiwi_service_supported_audio_codecs;
    info.can_try_to_connect = tiwi_service_can_try_to_connect;
    info.apply_encoder_settings = tiwi_service_apply_encoder_settings;
    obs_register_service(&info);
}
