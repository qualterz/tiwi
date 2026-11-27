#pragma once

#include <QWidget>
#include <QElapsedTimer>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QTimer>
#include <QToolButton>

#include <obs-frontend-api.h>
#include "tiwi_obs.h"

#include <string>

class TiwiDock : public QWidget {
    Q_OBJECT

public:
    explicit TiwiDock(QWidget *parent = nullptr);
    ~TiwiDock() override;

    void onFrontendEvent(enum obs_frontend_event event);
    void shutdown();

protected:
    void changeEvent(QEvent *event) override;

private slots:
    void onToggleStream();
    void onCopyLink();
    void onRegenerateToken();
    void onTokenEdited();
    void onPollMetrics();
    void onOutputStopped();

private:
    void loadConfig();
    void saveConfig() const;
    void applyTokenIfChanged();
    void restartEngine();
    void startStandaloneStream();
    void stopStandaloneStream();
    void releaseOutput();
    void updateUiState();
    void updateStatusBarMinWidths();
    void showFirstRunDialog();
    void showError(const QString &msg);
    void clearError();

    std::string token_;
    std::string viewerUrl_;
    std::string mqttUrl_;
    std::string stunServer_;
    bool setupCompleted_ = false;

    TiwiEngine *engine_ = nullptr;
    obs_output_t *output_ = nullptr;
    obs_service_t *service_ = nullptr;
    obs_encoder_t *videoEncoder_ = nullptr;
    obs_encoder_t *audioEncoder_ = nullptr;
    bool streaming_ = false;
    QElapsedTimer streamTimer_;

    QLabel *liveIconLabel_ = nullptr;
    QLabel *uptimeLabel_ = nullptr;
    QLabel *viewersIconLabel_ = nullptr;
    QLabel *viewersLabel_ = nullptr;
    QLabel *outIconLabel_ = nullptr;
    QLabel *outLabel_ = nullptr;
    QLabel *rttLabel_ = nullptr;
    QLabel *errorLabel_ = nullptr;
    QLineEdit *tokenEdit_ = nullptr;
    QToolButton *regenBtn_ = nullptr;
    QPushButton *toggleBtn_ = nullptr;
    QPushButton *copyLinkBtn_ = nullptr;
    QTimer *pollTimer_ = nullptr;
};
