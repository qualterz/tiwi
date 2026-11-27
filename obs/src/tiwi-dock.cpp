#include "tiwi-dock.hpp"
#include "tiwi-output.hpp"

#include <QApplication>
#include <QClipboard>
#include <QDockWidget>
#include <QEvent>
#include <QFrame>
#include <QHBoxLayout>
#include <QIcon>
#include <QMainWindow>
#include <QMessageBox>
#include <QMetaObject>
#include <QPointer>
#include <QStyle>
#include <QVBoxLayout>

#include <obs-module.h>
#include <util/config-file.h>
#include <util/platform.h>

#include <cstring>
#include <functional>
#include <utility>

namespace {

QPixmap loadThemeIcon(const QString &primary, const QString &fallback, int size = 14) {
    QIcon icon(primary);
    if (icon.isNull() && !fallback.isEmpty()) {
        icon = QIcon(fallback);
    }
    return icon.pixmap(size, size);
}

QString formatUptime(qint64 totalSec) {
    const qint64 h = totalSec / 3600;
    const qint64 m = (totalSec % 3600) / 60;
    const qint64 s = totalSec % 60;
    return QString("%1:%2:%3")
        .arg(h, 2, 10, QChar('0'))
        .arg(m, 2, 10, QChar('0'))
        .arg(s, 2, 10, QChar('0'));
}

struct RecommendedEncoder {
    std::string obsId;
    std::string simpleId;
    QString displayName;
};

bool hasEncoderType(const char *target) {
    const char *id = nullptr;
    for (size_t idx = 0; obs_enum_encoder_types(idx, &id); ++idx) {
        if (id && std::strcmp(id, target) == 0) {
            return true;
        }
    }
    return false;
}

std::string findVideoToolboxH264() {
    const char *id = nullptr;
    std::string fallback_vt;
    for (size_t idx = 0; obs_enum_encoder_types(idx, &id); ++idx) {
        if (!id || std::strncmp(id, "com.apple.videotoolbox.videoencoder", 35) != 0) {
            continue;
        }
        const char *codec = obs_get_encoder_codec(id);
        if (!codec || std::strcmp(codec, "h264") != 0) {
            continue;
        }
        if (std::strstr(id, ".ave.") != nullptr || std::strstr(id, ".gva") != nullptr) {
            return id;
        }
        if (std::strstr(id, ".sw") == nullptr) {
            fallback_vt = id;
        } else if (fallback_vt.empty()) {
            fallback_vt = id;
        }
    }
    return fallback_vt;
}

RecommendedEncoder detectRecommendedVideoEncoder() {
    std::string obs_id;
    std::string simple_id;

    for (const char *candidate : {"obs_nvenc_h264_tex", "ffmpeg_nvenc", "jim_nvenc"}) {
        if (hasEncoderType(candidate)) {
            obs_id = candidate;
            simple_id = "nvenc";
            break;
        }
    }
    if (obs_id.empty() && hasEncoderType("h264_texture_amf")) {
        obs_id = "h264_texture_amf";
        simple_id = "amd";
    }
    if (obs_id.empty()) {
        for (const char *candidate : {"obs_qsv11_v2", "obs_qsv11"}) {
            if (hasEncoderType(candidate)) {
                obs_id = candidate;
                simple_id = "qsv";
                break;
            }
        }
    }
    if (obs_id.empty()) {
        if (const std::string vt = findVideoToolboxH264(); !vt.empty()) {
            obs_id = vt;
            simple_id = "apple_h264";
        }
    }
    if (obs_id.empty()) {
        for (const char *candidate : {"ffmpeg_vaapi_tex", "ffmpeg_vaapi"}) {
            if (hasEncoderType(candidate)) {
                obs_id = candidate;
                break;
            }
        }
    }
    if (obs_id.empty()) {
        obs_id = "obs_x264";
        simple_id = "x264";
    }

    const char *disp = obs_encoder_get_display_name(obs_id.c_str());
    return {obs_id, simple_id, QString::fromUtf8((disp && *disp) ? disp : obs_id.c_str())};
}

void applyWebRtcEncoderOverrides(const std::string &enc_id, obs_data_t *settings) {
    if (!settings) {
        return;
    }

    if (enc_id.rfind("com.apple.videotoolbox.", 0) == 0) {
        obs_data_set_bool(settings, "bframes", false);
    } else if (enc_id.rfind("obs_qsv11", 0) == 0) {
        obs_data_set_int(settings, "bframes", 0);
        obs_data_set_int(settings, "bf", 0);
    } else {
        obs_data_set_int(settings, "bf", 0);
    }

    obs_data_set_bool(settings, "repeat_headers", true);
    const long long keyint = obs_data_get_int(settings, "keyint_sec");
    if (keyint <= 0 || keyint > 2) {
        obs_data_set_int(settings, "keyint_sec", 2);
    }
}

void applyRecommendedObsSettings(const RecommendedEncoder &rec) {
    config_t *cfg = obs_frontend_get_profile_config();
    if (!cfg) {
        return;
    }

    const char *mode = config_get_string(cfg, "Output", "Mode");
    const bool is_advanced = mode && std::strcmp(mode, "Advanced") == 0;

    if (!is_advanced && !rec.simpleId.empty()) {
        config_set_string(cfg, "SimpleOutput", "StreamEncoder", rec.simpleId.c_str());
        config_set_int(cfg, "SimpleOutput", "VBitrate", 4000);
        config_set_int(cfg, "SimpleOutput", "ABitrate", 160);

        if (rec.simpleId == "nvenc") {
            config_set_string(cfg, "SimpleOutput", "NVENCPreset2", "p4");
        } else if (rec.simpleId == "amd") {
            config_set_string(cfg, "SimpleOutput", "AMDPreset", "balanced");
        } else if (rec.simpleId == "qsv") {
            config_set_string(cfg, "SimpleOutput", "QSVPreset", "balanced");
        } else if (rec.simpleId == "x264") {
            config_set_string(cfg, "SimpleOutput", "Preset", "veryfast");
        }
    } else {
        config_set_string(cfg, "Output", "Mode", "Advanced");
        config_set_string(cfg, "AdvOut", "Encoder", rec.obsId.c_str());

        long long track_idx = config_get_int(cfg, "AdvOut", "TrackIndex");
        if (track_idx < 1 || track_idx > MAX_AUDIO_MIXES) {
            track_idx = 1;
            config_set_int(cfg, "AdvOut", "TrackIndex", track_idx);
        }
        const std::string track_key = "Track" + std::to_string(track_idx) + "Bitrate";
        if (config_get_int(cfg, "AdvOut", track_key.c_str()) <= 0) {
            config_set_int(cfg, "AdvOut", track_key.c_str(), 160);
        }

        obs_data_t *enc_settings = obs_data_create();
        obs_data_set_string(enc_settings, "rate_control", "CBR");
        obs_data_set_int(enc_settings, "bitrate", 4000);
        applyWebRtcEncoderOverrides(rec.obsId, enc_settings);

        if (rec.obsId.rfind("obs_nvenc_", 0) == 0 || rec.obsId == "jim_nvenc" || rec.obsId == "ffmpeg_nvenc") {
            obs_data_set_string(enc_settings, "preset", "p4");
            obs_data_set_string(enc_settings, "preset2", "p4");
            obs_data_set_string(enc_settings, "tune", "ll");
            obs_data_set_string(enc_settings, "multipass", "disabled");
            obs_data_set_bool(enc_settings, "lookahead", false);
        } else if (rec.obsId.size() >= 4 && rec.obsId.compare(rec.obsId.size() - 4, 4, "_amf") == 0) {
            obs_data_set_string(enc_settings, "preset", "balanced");
        } else if (rec.obsId.rfind("obs_qsv11", 0) == 0) {
            obs_data_set_string(enc_settings, "latency", "ultra-low");
        } else if (rec.obsId == "obs_x264") {
            obs_data_set_string(enc_settings, "preset", "veryfast");
            obs_data_set_string(enc_settings, "tune", "zerolatency");
        }

        if (char *profile_dir = obs_frontend_get_current_profile_path()) {
            const std::string enc_path = std::string(profile_dir) + "/streamEncoder.json";
            obs_data_save_json_safe(enc_settings, enc_path.c_str(), "tmp", "bak");
            bfree(profile_dir);
        }
        obs_data_release(enc_settings);
    }

    config_save_safe(cfg, "tmp", nullptr);
}

std::string mapSimpleVideoEncoder(const char *simple_id) {
    if (!simple_id || !*simple_id) {
        return "obs_x264";
    }
    if (std::strcmp(simple_id, "x264") == 0 || std::strcmp(simple_id, "x264_lowcpu") == 0) {
        return "obs_x264";
    }
    if (std::strcmp(simple_id, "amd") == 0) {
        return "h264_texture_amf";
    }
    if (std::strcmp(simple_id, "amd_hevc") == 0) {
        return "h265_texture_amf";
    }
    if (std::strcmp(simple_id, "amd_av1") == 0) {
        return "av1_texture_amf";
    }
    if (std::strcmp(simple_id, "nvenc") == 0) {
        return hasEncoderType("obs_nvenc_h264_tex") ? "obs_nvenc_h264_tex" : "ffmpeg_nvenc";
    }
    if (std::strcmp(simple_id, "nvenc_hevc") == 0) {
        return hasEncoderType("obs_nvenc_hevc_tex") ? "obs_nvenc_hevc_tex" : "ffmpeg_hevc_nvenc";
    }
    if (std::strcmp(simple_id, "nvenc_av1") == 0) {
        return "obs_nvenc_av1_tex";
    }
    if (std::strcmp(simple_id, "qsv") == 0) {
        return "obs_qsv11_v2";
    }
    if (std::strcmp(simple_id, "qsv_hevc") == 0) {
        return "obs_qsv11_hevc";
    }
    if (std::strcmp(simple_id, "qsv_av1") == 0) {
        return "obs_qsv11_av1";
    }
    if (std::strcmp(simple_id, "apple_h264") == 0) {
        const std::string vt = findVideoToolboxH264();
        return vt.empty() ? "obs_x264" : vt;
    }
    if (std::strcmp(simple_id, "apple_hevc") == 0) {
        return "com.apple.videotoolbox.videoencoder.ave.hevc";
    }
    return simple_id;
}

void on_tiwi_output_stop_signal(void *data, calldata_t *) {
    auto *dock = static_cast<TiwiDock *>(data);
    QMetaObject::invokeMethod(dock, "onOutputStopped", Qt::QueuedConnection);
}

} // namespace

TiwiDock::TiwiDock(QWidget *parent) : QWidget(parent) {
    loadConfig();

    auto *root = new QVBoxLayout(this);
    root->setContentsMargins(0, 0, 0, 0);
    root->setSpacing(0);

    auto *controls = new QVBoxLayout();
    controls->setContentsMargins(4, 4, 4, 4);
    controls->setSpacing(4);

    toggleBtn_ = new QPushButton("Start Streaming", this);
    toggleBtn_->setCheckable(true);
    copyLinkBtn_ = new QPushButton("Copy Link", this);
    controls->addWidget(toggleBtn_);
    controls->addWidget(copyLinkBtn_);

    auto *tokenRow = new QHBoxLayout();
    tokenRow->setContentsMargins(0, 0, 0, 0);
    tokenRow->setSpacing(4);
    tokenEdit_ = new QLineEdit(QString::fromStdString(token_), this);
    tokenEdit_->setPlaceholderText("Stream token");
    tokenEdit_->setCursorPosition(0);
    regenBtn_ = new QToolButton(this);
    regenBtn_->setProperty("class", "icon-refresh");
    regenBtn_->setToolTip("Regenerate token");
    regenBtn_->setAccessibleName("Regenerate token");
    tokenRow->addWidget(tokenEdit_, 1);
    tokenRow->addWidget(regenBtn_);
    controls->addLayout(tokenRow);

    errorLabel_ = new QLabel(this);
    errorLabel_->setWordWrap(true);
    errorLabel_->setStyleSheet("color: #f87171;");
    errorLabel_->hide();
    controls->addWidget(errorLabel_);

    root->addLayout(controls);
    root->addStretch();

    // Match OBSBasicStatusBar / StatusBarWidget styling from OBS's Yami theme.
    auto *statusBar = new QFrame(this);
    statusBar->setObjectName("tiwiStatusBar");
    statusBar->setStyleSheet(
        "#tiwiStatusBar {"
        "  border-top: 1px solid #3C404D;"
        "  background: transparent;"
        "}"
        "#tiwiStatusBar > QFrame {"
        "  border: 0px solid #3C404D;"
        "  background: transparent;"
        "}"
        "#tiwiStatusBar > QFrame[tiwiSep=\"true\"] {"
        "  border-left-width: 1px;"
        "}"
    );
    auto *barLayout = new QHBoxLayout(statusBar);
    barLayout->setContentsMargins(0, 0, 0, 0);
    barLayout->setSpacing(0);

    auto makeSegment = [&](bool leftBorder, int stretch = 0) -> QHBoxLayout * {
        auto *cell = new QFrame(statusBar);
        if (leftBorder) {
            cell->setProperty("tiwiSep", true);
        }
        auto *lay = new QHBoxLayout(cell);
        lay->setContentsMargins(8, 3, 8, 3);
        lay->setSpacing(5);
        lay->setAlignment(Qt::AlignCenter);
        barLayout->addWidget(cell, stretch);
        return lay;
    };

    // Segment 1: Viewers count with OBS eye icon
    auto *viewersCell = makeSegment(false);
    viewersIconLabel_ = new QLabel(statusBar);
    viewersIconLabel_->setPixmap(loadThemeIcon("theme:Dark/visible.svg", ":/res/images/visible.svg", 14));
    viewersIconLabel_->setFixedSize(14, 14);
    viewersIconLabel_->setAlignment(Qt::AlignCenter);
    viewersIconLabel_->setToolTip("Viewers");
    viewersLabel_ = new QLabel("0", statusBar);
    viewersLabel_->setAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    viewersLabel_->setToolTip("Viewers");
    viewersCell->addWidget(viewersIconLabel_);
    viewersCell->addWidget(viewersLabel_);

    // Segment 2: Stream uptime with OBS streaming icon
    auto *uptimeCell = makeSegment(true);
    liveIconLabel_ = new QLabel(statusBar);
    liveIconLabel_->setFixedSize(14, 14);
    liveIconLabel_->setAlignment(Qt::AlignCenter);
    uptimeLabel_ = new QLabel("00:00:00", statusBar);
    uptimeLabel_->setAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    uptimeCell->addWidget(liveIconLabel_);
    uptimeCell->addWidget(uptimeLabel_);

    // Segment 3: Total upload bitrate with OBS network signal icon
    auto *outCell = makeSegment(true, 1);
    outIconLabel_ = new QLabel(statusBar);
    outIconLabel_->setFixedSize(14, 14);
    outIconLabel_->setAlignment(Qt::AlignCenter);
    outIconLabel_->setToolTip("Total upload bitrate");
    outLabel_ = new QLabel("0 kbps", statusBar);
    outLabel_->setAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    outLabel_->setToolTip("Total upload bitrate");
    outCell->addWidget(outIconLabel_);
    outCell->addWidget(outLabel_);

    // Segment 4: Max viewer RTT
    auto *rttCell = makeSegment(true);
    rttLabel_ = new QLabel("RTT: --", statusBar);
    rttLabel_->setAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    rttCell->addWidget(rttLabel_);

    root->addWidget(statusBar);
    updateStatusBarMinWidths();

    connect(toggleBtn_, &QPushButton::clicked, this, &TiwiDock::onToggleStream);
    connect(copyLinkBtn_, &QPushButton::clicked, this, &TiwiDock::onCopyLink);
    connect(regenBtn_, &QToolButton::clicked, this, &TiwiDock::onRegenerateToken);
    connect(tokenEdit_, &QLineEdit::editingFinished, this, &TiwiDock::onTokenEdited);

    restartEngine();
    streaming_ = false;
    updateUiState();

    pollTimer_ = new QTimer(this);
    connect(pollTimer_, &QTimer::timeout, this, &TiwiDock::onPollMetrics);
    pollTimer_->start(1000);
}

TiwiDock::~TiwiDock() {
    shutdown();
}

void TiwiDock::changeEvent(QEvent *event) {
    QWidget::changeEvent(event);
    if (event->type() == QEvent::FontChange || event->type() == QEvent::StyleChange ||
        event->type() == QEvent::Polish) {
        updateStatusBarMinWidths();
    }
}

void TiwiDock::updateStatusBarMinWidths() {
    if (!uptimeLabel_) {
        return;
    }
    uptimeLabel_->ensurePolished();
    const QFontMetrics fm = uptimeLabel_->fontMetrics();
    uptimeLabel_->setMinimumWidth(fm.horizontalAdvance("00:00:00") + 6);
    viewersLabel_->setMinimumWidth(fm.horizontalAdvance("99") + 4);
    outLabel_->setMinimumWidth(fm.horizontalAdvance("999.9 Mbps") + 6);
    rttLabel_->setMinimumWidth(fm.horizontalAdvance("RTT: 999 ms") + 6);
}

void TiwiDock::onFrontendEvent(enum obs_frontend_event event) {
    if (event == OBS_FRONTEND_EVENT_EXIT) {
        shutdown();
    } else if (event == OBS_FRONTEND_EVENT_FINISHED_LOADING && !setupCompleted_) {
        if (auto *dock = qobject_cast<QDockWidget *>(parentWidget())) {
            auto *mainWindow = qobject_cast<QMainWindow *>(static_cast<QWidget *>(obs_frontend_get_main_window()));
            QDockWidget *controlsDock = mainWindow ? mainWindow->findChild<QDockWidget *>("controlsDock") : nullptr;
            QDockWidget *transitionsDock = mainWindow ? mainWindow->findChild<QDockWidget *>("transitionsDock") : nullptr;
            if (mainWindow && controlsDock && !controlsDock->isFloating() && controlsDock->isVisible()) {
                const Qt::DockWidgetArea area = mainWindow->dockWidgetArea(controlsDock);

                if (dock->isFloating()) {
                    dock->setFloating(false);
                }
                mainWindow->removeDockWidget(dock);
                mainWindow->addDockWidget(area, dock);
                if (transitionsDock && !transitionsDock->isFloating() && transitionsDock->isVisible() &&
                    mainWindow->dockWidgetArea(transitionsDock) == area) {
                    mainWindow->splitDockWidget(transitionsDock, dock, Qt::Horizontal);
                } else {
                    mainWindow->splitDockWidget(controlsDock, dock, Qt::Horizontal);
                    mainWindow->splitDockWidget(dock, controlsDock, Qt::Horizontal);
                }
                dock->show();

                QPointer<QMainWindow> mw(mainWindow);
                QPointer<QDockWidget> d(dock);
                QPointer<TiwiDock> self(this);
                auto applyMinWidth = [mw, d, self, area]() {
                    if (!mw || !d || !self || d->isFloating() || !d->isVisible() ||
                        mw->dockWidgetArea(d) != area) {
                        return;
                    }
                    if (QLayout *l = mw->layout()) {
                        l->activate();
                    }
                    d->ensurePolished();
                    self->updateStatusBarMinWidths();
                    const int tiwiWidth = qMax(d->minimumSizeHint().width(), 1);
                    const int curTiwiWidth = d->width();

                    QList<QDockWidget *> docks;
                    QList<int> sizes;
                    int widestIdx = -1;
                    for (QDockWidget *other : mw->findChildren<QDockWidget *>()) {
                        if (other == d || other->isFloating() || !other->isVisible() ||
                            mw->dockWidgetArea(other) != area) {
                            continue;
                        }
                        const int w = other->width();
                        if (w <= 0) {
                            continue;
                        }
                        if (widestIdx < 0 || w > sizes[widestIdx]) {
                            widestIdx = sizes.size();
                        }
                        docks.append(other);
                        sizes.append(w);
                    }
                    if (widestIdx >= 0 && curTiwiWidth > tiwiWidth) {
                        sizes[widestIdx] += (curTiwiWidth - tiwiWidth);
                    }
                    docks.append(d.data());
                    sizes.append(tiwiWidth);
                    mw->resizeDocks(docks, sizes, Qt::Horizontal);
                    if (QLayout *l = mw->layout()) {
                        l->activate();
                    }
                };

                class StartupResizeFilter : public QObject {
                public:
                    StartupResizeFilter(QPointer<QDockWidget> dock, std::function<void()> cb, QObject *parent)
                        : QObject(parent), dock_(std::move(dock)), cb_(std::move(cb)) {}
                    bool eventFilter(QObject *, QEvent *ev) override {
                        if (ev->type() == QEvent::Resize && dock_) {
                            QTimer::singleShot(0, dock_, cb_);
                        }
                        return false;
                    }

                private:
                    QPointer<QDockWidget> dock_;
                    std::function<void()> cb_;
                };

                QPointer<QObject> resizeFilter = new StartupResizeFilter(d, applyMinWidth, this);
                mainWindow->installEventFilter(resizeFilter);
                applyMinWidth();
                QTimer::singleShot(0, this, [this, mw, resizeFilter, applyMinWidth]() {
                    applyMinWidth();
                    showFirstRunDialog();
                    if (mw && resizeFilter) {
                        mw->removeEventFilter(resizeFilter);
                    }
                    if (resizeFilter) {
                        resizeFilter->deleteLater();
                    }
                });
                return;
            } else {
                dock->show();
                dock->resize(dock->minimumSizeHint());
                dock->raise();
            }
        }
        QTimer::singleShot(0, this, &TiwiDock::showFirstRunDialog);
    }
}

void TiwiDock::shutdown() {
    if (pollTimer_) {
        pollTimer_->stop();
    }
    if (output_) {
        signal_handler_t *sh = obs_output_get_signal_handler(output_);
        if (sh) {
            signal_handler_disconnect(sh, "stop", on_tiwi_output_stop_signal, this);
        }
        if (obs_output_active(output_)) {
            obs_output_force_stop(output_);
        }
    }
    releaseOutput();
    streaming_ = false;
    tiwi_output_set_engine(nullptr);
    if (engine_) {
        tiwi_engine_destroy(engine_);
        engine_ = nullptr;
    }
}

void TiwiDock::loadConfig() {
    char *path = obs_module_config_path("config.json");
    obs_data_t *data = path ? obs_data_create_from_json_file(path) : nullptr;
    bfree(path);

    if (data) {
        if (const char *t = obs_data_get_string(data, "token"); t && *t)
            token_ = t;
        if (const char *v = obs_data_get_string(data, "viewer_url"); v && *v)
            viewerUrl_ = v;
        if (const char *m = obs_data_get_string(data, "mqtt_url"); m && *m)
            mqttUrl_ = m;
        if (const char *s = obs_data_get_string(data, "stun_server"); s && *s)
            stunServer_ = s;
        setupCompleted_ = obs_data_get_bool(data, "setup_completed");
        obs_data_release(data);
    }

    if (token_.empty()) {
        char buf[128] = {};
        if (tiwi_generate_token(buf, sizeof(buf))) {
            token_ = buf;
            saveConfig();
        }
    }
}

void TiwiDock::saveConfig() const {
    char *dir = obs_module_config_path("");
    if (dir) {
        os_mkdirs(dir);
        bfree(dir);
    }
    char *path = obs_module_config_path("config.json");
    if (!path) {
        return;
    }

    obs_data_t *data = obs_data_create();
    obs_data_set_string(data, "token", token_.c_str());
    if (!viewerUrl_.empty())
        obs_data_set_string(data, "viewer_url", viewerUrl_.c_str());
    if (!mqttUrl_.empty())
        obs_data_set_string(data, "mqtt_url", mqttUrl_.c_str());
    if (!stunServer_.empty())
        obs_data_set_string(data, "stun_server", stunServer_.c_str());
    obs_data_set_bool(data, "setup_completed", setupCompleted_);
    obs_data_save_json_safe(data, path, "tmp", "bak");
    obs_data_release(data);
    bfree(path);
}

void TiwiDock::applyTokenIfChanged() {
    if (streaming_) {
        return;
    }
    const std::string entered = tokenEdit_->text().trimmed().toStdString();
    if (entered.empty()) {
        tokenEdit_->setText(QString::fromStdString(token_));
        tokenEdit_->setCursorPosition(0);
        return;
    }
    if (entered != token_) {
        token_ = entered;
        saveConfig();
        restartEngine();
    }
}

void TiwiDock::restartEngine() {
    tiwi_output_set_engine(nullptr);
    if (engine_) {
        tiwi_engine_destroy(engine_);
        engine_ = nullptr;
    }

    TiwiConfig cfg = {};
    cfg.token = token_.c_str();
    cfg.viewer_url = viewerUrl_.c_str();
    cfg.mqtt_url = mqttUrl_.c_str();
    cfg.stun_server = stunServer_.c_str();

    engine_ = tiwi_engine_create(&cfg);
    tiwi_output_set_engine(engine_);
}

void TiwiDock::releaseOutput() {
    if (output_) {
        signal_handler_t *sh = obs_output_get_signal_handler(output_);
        if (sh) {
            signal_handler_disconnect(sh, "stop", on_tiwi_output_stop_signal, this);
        }
        obs_output_release(output_);
        output_ = nullptr;
    }
    if (videoEncoder_) {
        obs_encoder_release(videoEncoder_);
        videoEncoder_ = nullptr;
    }
    if (audioEncoder_) {
        obs_encoder_release(audioEncoder_);
        audioEncoder_ = nullptr;
    }
    if (service_) {
        obs_service_release(service_);
        service_ = nullptr;
    }
}

void TiwiDock::startStandaloneStream() {
    clearError();
    applyTokenIfChanged();
    if (!engine_) {
        restartEngine();
    }
    releaseOutput();

    std::string venc_id = "obs_x264";
    obs_data_t *venc_settings = nullptr;
    size_t audio_track = 0;
    int audio_bitrate = 160;

    config_t *cfg = obs_frontend_get_profile_config();
    const char *mode = cfg ? config_get_string(cfg, "Output", "Mode") : nullptr;
    const bool is_advanced = mode && std::strcmp(mode, "Advanced") == 0;

    if (is_advanced) {
        if (const char *id = config_get_string(cfg, "AdvOut", "Encoder"); id && *id) {
            venc_id = id;
        }
        char *profile_dir = obs_frontend_get_current_profile_path();
        if (profile_dir) {
            const std::string enc_path = std::string(profile_dir) + "/streamEncoder.json";
            bfree(profile_dir);
            venc_settings = obs_data_create_from_json_file(enc_path.c_str());
        }
        const long long track_1based = config_get_int(cfg, "AdvOut", "TrackIndex");
        if (track_1based >= 1 && track_1based <= MAX_AUDIO_MIXES) {
            audio_track = static_cast<size_t>(track_1based - 1);
        }
        const std::string track_key = "Track" + std::to_string(audio_track + 1) + "Bitrate";
        const long long ab = config_get_int(cfg, "AdvOut", track_key.c_str());
        if (ab > 0) {
            audio_bitrate = static_cast<int>(ab);
        }
    } else if (cfg) {
        const char *simple_enc = config_get_string(cfg, "SimpleOutput", "StreamEncoder");
        venc_id = mapSimpleVideoEncoder(simple_enc);
        venc_settings = obs_data_create();
        const long long vb = config_get_int(cfg, "SimpleOutput", "VBitrate");
        if (vb > 0) {
            obs_data_set_int(venc_settings, "bitrate", vb);
            obs_data_set_string(venc_settings, "rate_control", "CBR");
        }
        const char *preset_type = "Preset";
        if (simple_enc) {
            if (std::strcmp(simple_enc, "qsv") == 0 || std::strcmp(simple_enc, "qsv_av1") == 0) {
                preset_type = "QSVPreset";
            } else if (std::strcmp(simple_enc, "amd") == 0 || std::strcmp(simple_enc, "amd_hevc") == 0) {
                preset_type = "AMDPreset";
            } else if (std::strcmp(simple_enc, "amd_av1") == 0) {
                preset_type = "AMDAV1Preset";
            } else if (std::strcmp(simple_enc, "nvenc") == 0 || std::strcmp(simple_enc, "nvenc_hevc") == 0 ||
                       std::strcmp(simple_enc, "nvenc_av1") == 0) {
                preset_type = "NVENCPreset2";
            }
        }
        if (const char *preset = config_get_string(cfg, "SimpleOutput", preset_type); preset && *preset) {
            const char *preset_key =
                (venc_id.rfind("ffmpeg_", 0) == 0 && std::strcmp(preset_type, "NVENCPreset2") == 0)
                    ? "preset2"
                    : "preset";
            obs_data_set_string(venc_settings, preset_key, preset);
        }
        if (config_get_bool(cfg, "SimpleOutput", "UseAdvanced")) {
            if (const char *x264_opts = config_get_string(cfg, "SimpleOutput", "x264Settings");
                x264_opts && *x264_opts) {
                obs_data_set_string(venc_settings, "x264opts", x264_opts);
            }
        }
        const long long ab = config_get_int(cfg, "SimpleOutput", "ABitrate");
        if (ab > 0) {
            audio_bitrate = static_cast<int>(ab);
        }
    }

    if (!venc_settings) {
        venc_settings = obs_data_create();
        obs_data_set_int(venc_settings, "bitrate", 6000);
        obs_data_set_string(venc_settings, "rate_control", "CBR");
    }

    obs_data_t *aenc_settings = obs_data_create();
    obs_data_set_int(aenc_settings, "bitrate", audio_bitrate);

    service_ = obs_service_create_private("tiwi_service", "Tiwi", nullptr);
    if (service_) {
        obs_service_apply_encoder_settings(service_, venc_settings, aenc_settings);
    }
    applyWebRtcEncoderOverrides(venc_id, venc_settings);

    videoEncoder_ = obs_video_encoder_create(venc_id.c_str(), "tiwi_video_encoder", venc_settings, nullptr);
    audioEncoder_ = obs_audio_encoder_create("ffmpeg_opus", "tiwi_audio_encoder", aenc_settings, audio_track, nullptr);

    obs_data_release(venc_settings);
    obs_data_release(aenc_settings);

    if (!service_ || !videoEncoder_ || !audioEncoder_) {
        blog(LOG_ERROR, "[tiwi] Failed to create encoders/service for '%s'", venc_id.c_str());
        releaseOutput();
        streaming_ = false;
        updateUiState();
        showError("Failed to start stream. Check OBS log for details.");
        return;
    }

    obs_encoder_set_video(videoEncoder_, obs_get_video());
    obs_encoder_set_audio(audioEncoder_, obs_get_audio());

    output_ = obs_output_create("tiwi_output", "Tiwi", nullptr, nullptr);
    if (!output_) {
        blog(LOG_ERROR, "[tiwi] Failed to create tiwi_output");
        releaseOutput();
        streaming_ = false;
        updateUiState();
        showError("Failed to start stream. Check OBS log for details.");
        return;
    }

    obs_output_set_service(output_, service_);
    obs_output_set_video_encoder(output_, videoEncoder_);
    obs_output_set_audio_encoder(output_, audioEncoder_, 0);

    signal_handler_t *sh = obs_output_get_signal_handler(output_);
    if (sh) {
        signal_handler_connect(sh, "stop", on_tiwi_output_stop_signal, this);
    }

    if (!obs_output_start(output_)) {
        blog(LOG_ERROR, "[tiwi] Failed to start standalone Tiwi output with '%s'", venc_id.c_str());
        releaseOutput();
        streaming_ = false;
        updateUiState();
        showError("Failed to start stream. Check OBS log for details.");
        return;
    }

    streaming_ = true;
    streamTimer_.restart();
    updateUiState();
}

void TiwiDock::stopStandaloneStream() {
    if (!output_) {
        streaming_ = false;
        updateUiState();
        return;
    }
    toggleBtn_->setText("Stopping...");
    toggleBtn_->setEnabled(false);
    obs_output_stop(output_);
}

void TiwiDock::onOutputStopped() {
    releaseOutput();
    streaming_ = false;
    streamTimer_.invalidate();
    toggleBtn_->setEnabled(true);
    updateUiState();
}

void TiwiDock::onToggleStream() {
    if (streaming_) {
        stopStandaloneStream();
    } else {
        startStandaloneStream();
    }
}

void TiwiDock::onCopyLink() {
    applyTokenIfChanged();
    if (!engine_) {
        return;
    }
    char url[512] = {};
    if (tiwi_engine_watch_url(engine_, url, sizeof(url))) {
        QApplication::clipboard()->setText(QString::fromUtf8(url));
        copyLinkBtn_->setText("Link Copied");
        QTimer::singleShot(1500, this, [this]() {
            if (copyLinkBtn_) {
                copyLinkBtn_->setText("Copy Link");
            }
        });
    }
}

void TiwiDock::onRegenerateToken() {
    if (streaming_) {
        return;
    }
    const auto reply = QMessageBox::question(
        this,
        "Regenerate Token",
        "Generate a new stream token? Existing watch links will stop working.",
        QMessageBox::Yes | QMessageBox::No,
        QMessageBox::No
    );
    if (reply != QMessageBox::Yes) {
        return;
    }
    char buf[128] = {};
    if (tiwi_generate_token(buf, sizeof(buf))) {
        token_ = buf;
        tokenEdit_->setText(QString::fromStdString(token_));
        tokenEdit_->setCursorPosition(0);
        saveConfig();
        restartEngine();
    }
}

void TiwiDock::onTokenEdited() {
    applyTokenIfChanged();
}

void TiwiDock::showFirstRunDialog() {
    if (setupCompleted_) {
        return;
    }
    const RecommendedEncoder rec = detectRecommendedVideoEncoder();
    QWidget *parent = static_cast<QWidget *>(obs_frontend_get_main_window());
    QMessageBox box(parent ? parent : this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle("Tiwi Setup");
    box.setText("Configure OBS streaming settings for Tiwi?");
    box.setInformativeText(
        QString(
            "Tiwi uses your OBS streaming settings (Settings -> Output -> Streaming).\n\n"
            "Would you like to update your current OBS profile to recommended defaults (%1, 4000 kbps CBR, low latency)?\n\n"
            "You can adjust these anytime in OBS Settings -> Output."
        )
            .arg(rec.displayName)
    );
    QPushButton *applyBtn = box.addButton("Apply Recommended Defaults", QMessageBox::AcceptRole);
    QPushButton *keepBtn = box.addButton("Keep Current Settings", QMessageBox::RejectRole);
    box.setDefaultButton(applyBtn);
    box.setEscapeButton(keepBtn);
    box.exec();

    if (box.clickedButton() == applyBtn) {
        applyRecommendedObsSettings(rec);
    }
    setupCompleted_ = true;
    saveConfig();
}

void TiwiDock::showError(const QString &msg) {
    if (!errorLabel_) {
        return;
    }
    errorLabel_->setText(msg);
    errorLabel_->show();
}

void TiwiDock::clearError() {
    if (!errorLabel_) {
        return;
    }
    errorLabel_->clear();
    errorLabel_->hide();
}

void TiwiDock::onPollMetrics() {
    if (streaming_ && streamTimer_.isValid()) {
        uptimeLabel_->setText(formatUptime(streamTimer_.elapsed() / 1000));
    }

    if (!engine_) {
        return;
    }
    TiwiMetrics m = {};
    if (!tiwi_engine_get_metrics(engine_, &m)) {
        return;
    }

    if (!streaming_ || !m.live) {
        if (!streaming_) {
            uptimeLabel_->setText("00:00:00");
            viewersLabel_->setText("0");
            outLabel_->setText("0 kbps");
            outIconLabel_->setToolTip("Total upload bitrate");
            outLabel_->setToolTip("Total upload bitrate");
            rttLabel_->setText("RTT: --");
            rttLabel_->setToolTip("");
        }
        return;
    }

    viewersLabel_->setText(QString::number(m.viewers));
    if (m.out_kbps >= 1000.0) {
        outLabel_->setText(QString("%1 Mbps").arg(m.out_kbps / 1000.0, 0, 'f', 1));
    } else {
        outLabel_->setText(QString("%1 kbps").arg(static_cast<int>(m.out_kbps + 0.5)));
    }
    const char *netIcon = ":/res/images/network-excellent.svg";
    if (m.congested || m.loss_pct >= 5.0f) {
        netIcon = ":/res/images/network-bad.svg";
    } else if (m.loss_pct >= 2.0f) {
        netIcon = ":/res/images/network-mediocre.svg";
    } else if (m.loss_pct >= 0.5f) {
        netIcon = ":/res/images/network-good.svg";
    }
    outIconLabel_->setPixmap(loadThemeIcon(netIcon, "theme:Dark/network-inactive.svg", 14));
    const QString rttStr = m.rtt_ms >= 0 ? QString("%1 ms").arg(m.rtt_ms) : QString("--");
    rttLabel_->setText(QString("RTT: %1").arg(rttStr));
    const QString statsTip = QString("Loss: %1% | Codec: %2%3")
        .arg(m.loss_pct, 0, 'f', 1)
        .arg(QString::fromUtf8(m.codec))
        .arg(m.congested ? " | Congested" : "");
    outIconLabel_->setToolTip(statsTip);
    outLabel_->setToolTip(statsTip);
    rttLabel_->setToolTip(statsTip);
}

void TiwiDock::updateUiState() {
    toggleBtn_->setText(streaming_ ? "Stop Streaming" : "Start Streaming");
    toggleBtn_->setChecked(streaming_);
    tokenEdit_->setEnabled(!streaming_);
    regenBtn_->setEnabled(!streaming_);

    if (streaming_) {
        if (!streamTimer_.isValid()) {
            streamTimer_.start();
        }
        liveIconLabel_->setPixmap(
            loadThemeIcon(":/res/images/streaming-active.svg", "theme:Dark/streaming-inactive.svg", 14)
        );
        outIconLabel_->setPixmap(
            loadThemeIcon(":/res/images/network-excellent.svg", "theme:Dark/network-inactive.svg", 14)
        );
    } else {
        streamTimer_.invalidate();
        uptimeLabel_->setText("00:00:00");
        liveIconLabel_->setPixmap(loadThemeIcon("theme:Dark/streaming-inactive.svg", "", 14));
        outIconLabel_->setPixmap(loadThemeIcon("theme:Dark/network-inactive.svg", "", 14));
    }

    for (QLabel *lbl : {uptimeLabel_, viewersLabel_, outLabel_, rttLabel_}) {
        lbl->setProperty("class", streaming_ ? "" : "text-muted");
        lbl->style()->unpolish(lbl);
        lbl->style()->polish(lbl);
    }
}
