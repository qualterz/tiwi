#include "tiwi-output.hpp"

#include <atomic>
#include <cstring>

namespace {

std::atomic<TiwiEngine *> g_active_engine{nullptr};

struct TiwiOutput {
    obs_output_t *output = nullptr;
    bool warned_bframes = false;
};

bool map_video_codec(const char *codec_id, TiwiVideoCodec *out) {
    if (!codec_id) {
        return false;
    }
    if (std::strcmp(codec_id, "h264") == 0) {
        *out = TIWI_CODEC_H264;
        return true;
    }
    if (std::strcmp(codec_id, "hevc") == 0 || std::strcmp(codec_id, "h265") == 0) {
        *out = TIWI_CODEC_H265;
        return true;
    }
    if (std::strcmp(codec_id, "av1") == 0) {
        *out = TIWI_CODEC_AV1;
        return true;
    }
    return false;
}

const char *tiwi_output_get_name(void *) {
    return "Tiwi Output";
}

void *tiwi_output_create(obs_data_t *, obs_output_t *output) {
    auto *ctx = new TiwiOutput();
    ctx->output = output;
    return ctx;
}

void tiwi_output_destroy(void *data) {
    delete static_cast<TiwiOutput *>(data);
}

bool tiwi_output_start(void *data) {
    auto *ctx = static_cast<TiwiOutput *>(data);
    ctx->warned_bframes = false;

    TiwiEngine *engine = g_active_engine.load(std::memory_order_acquire);
    if (!engine) {
        blog(LOG_ERROR, "[tiwi] Cannot start output: engine not initialized");
        return false;
    }

    if (!obs_output_can_begin_data_capture(ctx->output, 0)) {
        return false;
    }
    if (!obs_output_initialize_encoders(ctx->output, 0)) {
        return false;
    }

    obs_encoder_t *venc = obs_output_get_video_encoder(ctx->output);
    if (!venc) {
        blog(LOG_ERROR, "[tiwi] No video encoder attached");
        return false;
    }

    const char *codec_str = obs_encoder_get_codec(venc);
    TiwiVideoCodec codec = TIWI_CODEC_H264;
    if (!map_video_codec(codec_str, &codec)) {
        blog(LOG_ERROR, "[tiwi] Unsupported video codec for WebRTC: %s", codec_str ? codec_str : "(null)");
        return false;
    }

    uint8_t *extra_data = nullptr;
    size_t extra_size = 0;
    obs_encoder_get_extra_data(venc, &extra_data, &extra_size);

    if (!tiwi_engine_start_stream(engine, codec, extra_data, extra_size)) {
        blog(LOG_ERROR, "[tiwi] Failed to start Tiwi relay stream");
        return false;
    }

    obs_output_begin_data_capture(ctx->output, 0);
    blog(LOG_INFO, "[tiwi] Direct output started (codec=%s, extra=%zu bytes)", codec_str, extra_size);
    return true;
}

void tiwi_output_stop(void *data, uint64_t) {
    auto *ctx = static_cast<TiwiOutput *>(data);
    obs_output_end_data_capture(ctx->output);

    if (TiwiEngine *engine = g_active_engine.load(std::memory_order_acquire)) {
        tiwi_engine_stop_stream(engine);
    }
    blog(LOG_INFO, "[tiwi] Direct output stopped");
}

void tiwi_output_encoded_packet(void *data, struct encoder_packet *packet) {
    if (!packet || !packet->data || packet->size == 0) {
        return;
    }

    auto *ctx = static_cast<TiwiOutput *>(data);
    TiwiEngine *engine = g_active_engine.load(std::memory_order_acquire);
    if (!engine) {
        return;
    }

    const bool is_video = packet->type == OBS_ENCODER_VIDEO;
    if (is_video && packet->pts != packet->dts && !ctx->warned_bframes) {
        ctx->warned_bframes = true;
        blog(LOG_WARNING, "[tiwi] B-frames detected (pts != dts); set max_b_frames=0 for smooth WebRTC playback");
    }

    tiwi_engine_push_frame(
        engine,
        is_video,
        packet->keyframe,
        packet->pts,
        packet->timebase_num,
        packet->timebase_den,
        packet->data,
        packet->size
    );
}

} // namespace

void tiwi_output_set_engine(TiwiEngine *engine) {
    g_active_engine.store(engine, std::memory_order_release);
}

void tiwi_output_register() {
    obs_output_info info = {};
    info.id = "tiwi_output";
    info.flags = OBS_OUTPUT_AV | OBS_OUTPUT_ENCODED | OBS_OUTPUT_SERVICE;
    info.protocols = "TIWI";
    info.get_name = tiwi_output_get_name;
    info.create = tiwi_output_create;
    info.destroy = tiwi_output_destroy;
    info.start = tiwi_output_start;
    info.stop = tiwi_output_stop;
    info.encoded_packet = tiwi_output_encoded_packet;
    info.encoded_video_codecs = "h264;hevc;av1";
    info.encoded_audio_codecs = "opus";
    obs_register_output(&info);
}
