#ifndef TIWI_OBS_H
#define TIWI_OBS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TiwiEngine TiwiEngine;

/* Log levels passed to TiwiLogCallback: 0=error, 1=warn, 2=info, 3=debug. */
typedef void (*TiwiLogCallback)(int level, const char *message);

/* Registers (or clears with NULL) a callback receiving Tiwi Rust tracing logs. */
void tiwi_set_log_callback(TiwiLogCallback cb);

typedef enum TiwiVideoCodec {
    TIWI_CODEC_H264 = 0,
    TIWI_CODEC_H265 = 1,
    TIWI_CODEC_AV1 = 2,
} TiwiVideoCodec;

typedef struct TiwiConfig {
    /* Optional UTF-8 null-terminated strings; NULL or "" uses defaults. */
    const char *token;
    const char *viewer_url;
    const char *mqtt_url;
    const char *stun_server;
    const char *public_ip;
    const char *logs_dir;
    uint16_t media_port;
    uint32_t max_peers;
    double max_uplink_mbps;
} TiwiConfig;

typedef struct TiwiMetrics {
    bool live;
    bool congested;
    uint32_t viewers;
    double in_kbps;
    double out_kbps;
    int64_t rtt_ms;   /* -1 when unavailable */
    float loss_pct;
    char codec[16];
} TiwiMetrics;

/* Generates a random URL-safe stream token into out_buf (null-terminated). */
bool tiwi_generate_token(char *out_buf, size_t out_len);

/* Starts the embedded Tiwi relay engine on a background thread. Returns NULL on error. */
TiwiEngine *tiwi_engine_create(const TiwiConfig *cfg);

/* Stops the relay, flushes StreamStop over MQTT, and frees all resources. Safe on NULL. */
void tiwi_engine_destroy(TiwiEngine *engine);

/* Copies the full "<viewer_url>#<token>" watch URL into out_buf (null-terminated). */
bool tiwi_engine_watch_url(const TiwiEngine *engine, char *out_buf, size_t out_len);

/* Announces stream start and initializes the RTP packetizer for the active video codec. */
bool tiwi_engine_start_stream(
    TiwiEngine *engine,
    TiwiVideoCodec codec,
    const uint8_t *extra_data,
    size_t extra_len
);

/* Pushes an encoded video or Opus audio frame without blocking the OBS encoder thread. */
bool tiwi_engine_push_frame(
    TiwiEngine *engine,
    bool is_video,
    bool is_keyframe,
    int64_t pts,
    uint32_t timebase_num,
    uint32_t timebase_den,
    const uint8_t *data,
    size_t len
);

/* Tears down all viewer sessions and announces stream stop over signaling. */
bool tiwi_engine_stop_stream(TiwiEngine *engine);

/* Populates out with the latest 1-second metrics snapshot. Returns false if engine is NULL. */
bool tiwi_engine_get_metrics(const TiwiEngine *engine, TiwiMetrics *out);

#ifdef __cplusplus
}
#endif

#endif /* TIWI_OBS_H */
