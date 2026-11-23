import mqtt from "mqtt";

export type ViewerState =
  | "invalid"
  | "paused"
  | "waiting"
  | "connecting"
  | "live"
  | "disconnected"
  | "no-internet"
  | "server-offline"
  | "timed-out"
  | "unreachable"
  | "stream-error";

type StandbyState =
  | "waiting"
  | "no-internet"
  | "server-offline"
  | "timed-out"
  | "unreachable"
  | "stream-error";

const isStandby = (s: ViewerState | null): s is StandbyState =>
  s === "waiting" ||
  s === "no-internet" ||
  s === "server-offline" ||
  s === "timed-out" ||
  s === "unreachable" ||
  s === "stream-error";

const MQTT_URL = "wss://broker.emqx.io:8084/mqtt";
const METRICS_INTERVAL_MS = 5000;
const ICE_GATHER_TIMEOUT_MS = 5000;
const OFFER_TIMEOUT_MS = 10_000;
const HANDSHAKE_TIMEOUT_MS = 15_000;
const DISCONNECT_GRACE_MS = 5000;

const toBase64Url = (buf: Uint8Array) =>
  btoa(String.fromCharCode(...buf))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");

const fromBase64Url = (str: string) => {
  const b64 = str.replace(/-/g, "+").replace(/_/g, "/");
  const padded = b64.padEnd(b64.length + ((4 - (b64.length % 4)) % 4), "=");
  return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
};

type PrevRtp = {
  ts: number;
  bytes: number;
  packets: number;
  jbDelay: number;
  jbTarget: number;
  jbEmitted: number;
  decodeTime: number;
  processingDelay: number;
  framesDecoded: number;
  assemblyTime: number;
  framesAssembled: number;
  playoutDelay: number;
  playoutSamples: number;
};

export type ViewerController = {
  play: () => void;
  pause: () => void;
  destroy: () => void;
};

export function startViewer(
  videoEl: HTMLVideoElement,
  setState: (state: ViewerState) => void,
): ViewerController {
  const onHashChange = () => location.reload();
  const onVideoError = () =>
    console.error("[Video] Playback error:", videoEl.error);

  window.addEventListener("hashchange", onHashChange);
  videoEl.addEventListener("error", onVideoError);

  const token = location.hash.slice(1);
  if (!token) {
    console.warn("[State] init -> invalid (missing #<token>)");
    setState("invalid");
    return {
      play: () => {},
      pause: () => {},
      destroy: () => {
        window.removeEventListener("hashchange", onHashChange);
        videoEl.removeEventListener("error", onVideoError);
      },
    };
  }

  let destroyed = false;
  let state: ViewerState | null = null;
  let gen = 0;
  let activePc: RTCPeerConnection | null = null;
  let mqttClient: mqtt.MqttClient | null = null;
  let activeAnswerTopic: string | null = null;
  let onMqttConnected: (() => void) | null = null;
  let onAnswerMessage: ((topic: string, payload: Buffer) => void) | null = null;
  let statsTimer: ReturnType<typeof setTimeout> | undefined;
  let phaseTimer: ReturnType<typeof setTimeout> | undefined;

  const seenEventIvs = new Set<string>();
  let streamId = "";
  let eventsTopic = "";
  let aesKey: CryptoKey | null = null;

  const transition = (next: ViewerState, reason?: string) => {
    const prev = state ?? "init";
    console.info(`[State] ${prev} -> ${next}${reason ? ` (${reason})` : ""}`);
    state = next;
    setState(next);
  };

  const transportInfo = (pc: RTCPeerConnection) => {
    const dtls =
      pc.getTransceivers()[0]?.receiver?.transport?.state ?? "unknown";
    return `ice=${pc.iceConnectionState}, dtls=${dtls}, online=${navigator.onLine}`;
  };

  const clearTimers = () => {
    clearTimeout(statsTimer);
    statsTimer = undefined;
    clearTimeout(phaseTimer);
    phaseTimer = undefined;
  };

  const clearSignalingHooks = () => {
    onMqttConnected = null;
    onAnswerMessage = null;
    if (mqttClient && activeAnswerTopic) {
      const topic = activeAnswerTopic;
      activeAnswerTopic = null;
      mqttClient.unsubscribe(topic);
    }
  };

  const closeMqtt = () => {
    clearSignalingHooks();
    if (!mqttClient) return;
    const c = mqttClient;
    mqttClient = null;
    c.removeAllListeners();
    c.on("error", () => {});
    c.end(true);
  };

  const ensureMqtt = () => {
    if (mqttClient) return mqttClient;
    const c = mqtt.connect(MQTT_URL, { reconnectPeriod: 2000 });
    mqttClient = c;
    c.on("connect", () => {
      if (mqttClient !== c || destroyed) return;
      c.subscribe(eventsTopic, (err) => {
        if (mqttClient !== c || destroyed) return;
        if (err) {
          console.warn("[MQTT] Events subscribe failed:", err.message);
        } else {
          console.info("[MQTT] Connected");
        }
      });
      if (isStandby(state)) {
        void connect("mqtt-reconnect");
        return;
      }
      onMqttConnected?.();
    });
    c.on("message", async (topic, payload) => {
      if (mqttClient !== c || destroyed) return;
      if (topic === eventsTopic) {
        if (state === "paused" || !aesKey) return;
        try {
          const { iv, data } = JSON.parse(payload.toString()) as {
            iv?: string;
            data?: string;
          };
          if (!iv || !data || seenEventIvs.has(iv)) return;
          const decrypted = await crypto.subtle.decrypt(
            { name: "AES-GCM", iv: fromBase64Url(iv) },
            aesKey,
            fromBase64Url(data),
          );
          if (mqttClient !== c || destroyed) return;
          seenEventIvs.add(iv);
          const { type } = JSON.parse(new TextDecoder().decode(decrypted)) as {
            type?: string;
          };
          if (type === "start") {
            void connect("event:start");
          } else if (type === "stop") {
            waitForStream("event:stop");
          }
        } catch {
          console.warn("[MQTT] Ignored invalid event");
        }
        return;
      }
      onAnswerMessage?.(topic, payload);
    });
    c.on("error", (err) => {
      if (mqttClient !== c || destroyed) return;
      console.warn("[MQTT] Error:", err.message);
    });
    c.on("offline", () => {
      if (mqttClient !== c || destroyed) return;
      console.warn("[MQTT] Offline");
    });
    return c;
  };

  const closePc = () => {
    if (!activePc) return;
    const pc = activePc;
    activePc = null;
    pc.onconnectionstatechange = null;
    pc.oniceconnectionstatechange = null;
    pc.onicegatheringstatechange = null;
    pc.onicecandidateerror = null;
    pc.ontrack = null;
    pc.close();
  };

  const pause = () => {
    if (destroyed || state === "paused") return;
    gen++;
    clearTimers();
    closeMqtt();
    closePc();
    videoEl.pause();
    transition("paused");
  };

  const play = () => {
    if (destroyed) return;
    if (state === "paused" || isStandby(state)) {
      void connect();
    } else if (videoEl.paused && videoEl.srcObject) {
      videoEl.play().catch((err: unknown) => {
        if (err instanceof DOMException && err.name === "AbortError") return;
        console.warn("[Video] Play failed:", err);
      });
    }
  };

  const waitForStream = (
    reason: string,
    nextState: StandbyState = "waiting",
  ) => {
    if (destroyed || state === "paused" || !aesKey) return;
    gen++;
    clearTimers();
    clearSignalingHooks();
    closePc();
    transition(nextState, reason);
    ensureMqtt();
  };

  const connect = async (reason?: string) => {
    if (destroyed || !aesKey) return;
    const key = aesKey;
    const myGen = ++gen;
    const viewerId = crypto.randomUUID();
    clearTimers();
    clearSignalingHooks();
    closePc();
    transition("connecting", reason);

    const pc = new RTCPeerConnection({
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
    });
    activePc = pc;
    const mediaStream = new MediaStream();

    pc.ontrack = (e) => {
      if (gen !== myGen) return;
      mediaStream.addTrack(e.track);
      if (videoEl.srcObject !== mediaStream) {
        videoEl.srcObject = mediaStream;
        videoEl.play().catch((err: unknown) => {
          if (err instanceof DOMException && err.name === "AbortError") return;
          console.warn("[Video] Autoplay blocked:", err);
        });
      }
    };

    const startedAtMs = performance.now();
    const prevRtp = new Map<string, PrevRtp>();
    const startRtp = new Map<string, { ts: number; bytes: number }>();
    let videoPeakKbps = 0;
    let videoCodec: string | null = null;
    let audioCodec: string | null = null;
    let playoutDelayMs: number | null = null;

    const logMetrics = async (label = "[Metrics]") => {
      const report = await pc.getStats();
      const uptimeSec = Math.round((performance.now() - startedAtMs) / 1000);

      let selectedPairId: string | undefined;
      let fallbackPair:
        (RTCIceCandidatePairStats & Record<string, unknown>) | undefined;
      for (const s of report.values()) {
        if (s.type === "transport") {
          const id = (s as { selectedCandidatePairId?: string })
            .selectedCandidatePairId;
          if (id) selectedPairId = id;
        } else if (s.type === "candidate-pair") {
          const cp = s as RTCIceCandidatePairStats & Record<string, unknown>;
          const isSelected = Boolean(cp.selected || cp.nominated);
          const prevSelected = Boolean(
            fallbackPair?.selected || fallbackPair?.nominated,
          );
          if (
            !prevSelected &&
            (isSelected || (!fallbackPair && cp.state === "succeeded"))
          ) {
            fallbackPair = cp;
          }
        }
      }

      const pair =
        (selectedPairId
          ? (report.get(selectedPairId) as
              (RTCIceCandidatePairStats & Record<string, unknown>) | undefined)
          : undefined) ?? fallbackPair;

      let localCandidate = "-";
      let remoteCandidate = "-";
      let protocol = "-";
      let currentRttMs: number | null = null;
      let avgRttMs: number | null = null;
      let oneWayPingMs: number | null = null;

      if (pair) {
        const local = report.get(pair.localCandidateId) as
          Record<string, unknown> | undefined;
        const remote = report.get(pair.remoteCandidateId) as
          Record<string, unknown> | undefined;
        if (local) {
          localCandidate = `${local.address ?? local.ip}:${local.port} (${local.candidateType})`;
          protocol = (local.protocol as string | undefined) ?? "-";
        }
        if (remote) {
          remoteCandidate = `${remote.address ?? remote.ip}:${remote.port} (${remote.candidateType})`;
        }
        currentRttMs =
          pair.currentRoundTripTime != null
            ? Math.round(pair.currentRoundTripTime * 1000)
            : null;
        const rttMeasurements =
          (pair.roundTripTimeMeasurements as number | undefined) ?? 0;
        const totalRttSec =
          (pair.totalRoundTripTime as number | undefined) ?? 0;
        avgRttMs =
          rttMeasurements > 0
            ? Math.round((totalRttSec / rttMeasurements) * 1000)
            : currentRttMs;
        oneWayPingMs =
          currentRttMs != null ? Math.round(currentRttMs / 2) : null;
      }

      const oneWayMs = oneWayPingMs ?? 0;
      let videoLine = "  video -";
      let audioLine = "  audio -";

      for (const s of report.values()) {
        if (s.type === "media-playout") {
          const p = s as unknown as Record<string, unknown>;
          const totalDelay = (p.totalPlayoutDelay as number | undefined) ?? 0;
          const totalSamples = (p.totalSamplesCount as number | undefined) ?? 0;
          const prev = prevRtp.get(s.id);
          const dSamples = prev
            ? totalSamples - prev.playoutSamples
            : totalSamples;
          const dDelay = prev ? totalDelay - prev.playoutDelay : totalDelay;
          playoutDelayMs =
            dSamples > 0 ? Math.round((dDelay / dSamples) * 1000) : null;
          prevRtp.set(s.id, {
            ts: s.timestamp,
            bytes: 0,
            packets: 0,
            jbDelay: 0,
            jbTarget: 0,
            jbEmitted: 0,
            decodeTime: 0,
            processingDelay: 0,
            framesDecoded: 0,
            assemblyTime: 0,
            framesAssembled: 0,
            playoutDelay: totalDelay,
            playoutSamples: totalSamples,
          });
        } else if (s.type === "inbound-rtp") {
          const r = s as RTCInboundRtpStreamStats & Record<string, unknown>;
          const bytes = r.bytesReceived ?? 0;
          const packets = r.packetsReceived ?? 0;
          const prev = prevRtp.get(r.id);
          const first = startRtp.get(r.id) ?? { ts: r.timestamp, bytes };
          if (!startRtp.has(r.id)) startRtp.set(r.id, first);

          const dtSec = prev ? (r.timestamp - prev.ts) / 1000 : 0;
          const totalSec = (r.timestamp - first.ts) / 1000;
          const kbps =
            dtSec > 0
              ? Math.round(((bytes - prev!.bytes) * 8) / dtSec / 1000)
              : 0;
          const pps =
            dtSec > 0 ? Math.round((packets - prev!.packets) / dtSec) : 0;
          const avgKbps =
            totalSec > 0
              ? Math.round(((bytes - first.bytes) * 8) / totalSec / 1000)
              : kbps;

          const jbDelay = (r.jitterBufferDelay as number | undefined) ?? 0;
          const jbTarget =
            (r.jitterBufferTargetDelay as number | undefined) ?? 0;
          const jbEmitted =
            (r.jitterBufferEmittedCount as number | undefined) ?? 0;
          const decodeTime = (r.totalDecodeTime as number | undefined) ?? 0;
          const processingDelay =
            (r.totalProcessingDelay as number | undefined) ?? 0;
          const framesDecoded = r.framesDecoded ?? 0;
          const assemblyTime = (r.totalAssemblyTime as number | undefined) ?? 0;
          const framesAssembled =
            (r.framesAssembledFromMultiplePackets as number | undefined) ?? 0;

          const dEmitted = prev ? jbEmitted - prev.jbEmitted : jbEmitted;
          const dDecoded = prev
            ? framesDecoded - prev.framesDecoded
            : framesDecoded;
          const dAssembled = prev
            ? framesAssembled - prev.framesAssembled
            : framesAssembled;

          const jitterMs = Math.round((r.jitter ?? 0) * 1000);
          const jitterBufferMs =
            dEmitted > 0
              ? Math.round(((jbDelay - (prev?.jbDelay ?? 0)) / dEmitted) * 1000)
              : jbEmitted > 0
                ? Math.round((jbDelay / jbEmitted) * 1000)
                : 0;
          const jitterBufferTargetMs =
            dEmitted > 0
              ? Math.round(
                  ((jbTarget - (prev?.jbTarget ?? 0)) / dEmitted) * 1000,
                )
              : jbEmitted > 0
                ? Math.round((jbTarget / jbEmitted) * 1000)
                : 0;

          prevRtp.set(r.id, {
            ts: r.timestamp,
            bytes,
            packets,
            jbDelay,
            jbTarget,
            jbEmitted,
            decodeTime,
            processingDelay,
            framesDecoded,
            assemblyTime,
            framesAssembled,
            playoutDelay: 0,
            playoutSamples: 0,
          });

          const codecStat = r.codecId
            ? (report.get(r.codecId) as { mimeType?: string } | undefined)
            : undefined;

          if (r.kind === "video") {
            const decodeMs =
              dDecoded > 0
                ? Math.round(
                    ((decodeTime - (prev?.decodeTime ?? 0)) / dDecoded) * 1000,
                  )
                : framesDecoded > 0
                  ? Math.round((decodeTime / framesDecoded) * 1000)
                  : 0;
            const processingMs =
              dDecoded > 0
                ? Math.round(
                    ((processingDelay - (prev?.processingDelay ?? 0)) /
                      dDecoded) *
                      1000,
                  )
                : framesDecoded > 0
                  ? Math.round((processingDelay / framesDecoded) * 1000)
                  : 0;
            const assemblyMs =
              dAssembled > 0
                ? Math.round(
                    ((assemblyTime - (prev?.assemblyTime ?? 0)) / dAssembled) *
                      1000,
                  )
                : framesAssembled > 0
                  ? Math.round((assemblyTime / framesAssembled) * 1000)
                  : 0;
            const pipelineMs =
              processingMs > 0
                ? processingMs
                : jitterBufferMs + assemblyMs + decodeMs;

            videoCodec = codecStat?.mimeType ?? videoCodec;
            videoPeakKbps = Math.max(videoPeakKbps, kbps);
            const width = r.frameWidth ?? videoEl.videoWidth;
            const height = r.frameHeight ?? videoEl.videoHeight;
            const fps =
              r.framesPerSecond != null
                ? Math.round(r.framesPerSecond)
                : dtSec > 0
                  ? Math.round(dDecoded / dtSec)
                  : 0;
            const decoder =
              (r.decoderImplementation as string | undefined) ?? "-";
            const hw = r.powerEfficientDecoder as boolean | undefined;
            const freezeSec = Number(
              ((r.totalFreezesDuration as number | undefined) ?? 0).toFixed(2),
            );

            videoLine = `  video ${videoCodec ?? "-"} (${decoder}${hw != null ? ` hw=${hw}` : ""}) ${width}x${height}@${fps}fps | rate=${kbps}kbps (avg=${avgKbps} peak=${videoPeakKbps}) ${pps}pps | e2e=${oneWayMs + pipelineMs}ms (jb=${jitterBufferMs}ms target=${jitterBufferTargetMs}ms dec=${decodeMs}ms proc=${processingMs}ms jitter=${jitterMs}ms) | dec=${framesDecoded} key=${r.keyFramesDecoded ?? 0} drop=${r.framesDropped ?? 0} lost=${r.packetsLost ?? 0} rtx=${(r.retransmittedPacketsReceived as number | undefined) ?? 0} freeze=${(r.freezeCount as number | undefined) ?? 0} (${freezeSec}s) nack=${r.nackCount ?? 0} pli=${r.pliCount ?? 0}`;
          } else if (r.kind === "audio") {
            audioCodec = codecStat?.mimeType ?? audioCodec;
            const e2e = oneWayMs + jitterBufferMs + (playoutDelayMs ?? 0);
            audioLine = `  audio ${audioCodec ?? "-"} | rate=${kbps}kbps ${pps}pps | e2e=${e2e}ms (jb=${jitterBufferMs}ms jitter=${jitterMs}ms) | lost=${r.packetsLost ?? 0}`;
          }
        }
      }

      console.info(
        [
          `${label} up=${uptimeSec}s [${pc.connectionState}/${pc.iceConnectionState}] rtt=${currentRttMs ?? "-"}ms (avg=${avgRttMs ?? "-"}ms ping=${oneWayPingMs ?? "-"}ms) ${protocol} ${localCandidate} -> ${remoteCandidate}`,
          videoLine,
          audioLine,
        ].join("\n"),
      );
    };

    const scheduleStats = () => {
      statsTimer = setTimeout(
        async () => {
          if (gen !== myGen || pc.connectionState !== "connected") return;
          await logMetrics("[Metrics]");
          if (gen === myGen && pc.connectionState === "connected") {
            scheduleStats();
          }
        },
        METRICS_INTERVAL_MS - (Date.now() % METRICS_INTERVAL_MS),
      );
    };

    let wasLive = false;
    let failing = false;
    const handleConnectionLost = async (trigger: string) => {
      if (gen !== myGen || failing) return;
      failing = true;
      const reason = `${trigger}; ${transportInfo(pc)}`;
      clearTimeout(statsTimer);
      statsTimer = undefined;
      try {
        await logMetrics("[Metrics] Final");
      } catch {}
      if (gen !== myGen) return;
      if (wasLive && navigator.onLine !== false) {
        void connect(reason);
      } else {
        waitForStream(
          reason,
          navigator.onLine === false ? "no-internet" : "unreachable",
        );
      }
    };

    pc.onicecandidateerror = (e) => {
      if (gen !== myGen) return;
      const err = e as RTCPeerConnectionIceErrorEvent;
      console.warn(
        "[ICE] Candidate error:",
        err.url,
        err.errorCode,
        err.errorText,
      );
    };

    pc.oniceconnectionstatechange = () => {
      if (gen !== myGen) return;
      if (pc.iceConnectionState === "failed") {
        void handleConnectionLost("ice-failed");
      }
    };

    pc.onconnectionstatechange = () => {
      if (gen !== myGen) return;
      const s = pc.connectionState;

      if (s === "connected") {
        wasLive = true;
        clearTimeout(phaseTimer);
        phaseTimer = undefined;
        transition("live");
        if (!statsTimer) scheduleStats();
      } else if (s === "disconnected") {
        clearTimeout(statsTimer);
        statsTimer = undefined;
        transition("disconnected", transportInfo(pc));
        clearTimeout(phaseTimer);
        phaseTimer = setTimeout(() => {
          if (gen !== myGen || pc.connectionState !== "disconnected") return;
          void handleConnectionLost("disconnected-timeout");
        }, DISCONNECT_GRACE_MS);
      } else if (s === "failed" || s === "closed") {
        void handleConnectionLost(`connection-${s}`);
      }
    };

    try {
      pc.addTransceiver("video", { direction: "recvonly" });
      pc.addTransceiver("audio", { direction: "recvonly" });
      const gatherStartMs = performance.now();
      await pc.setLocalDescription(await pc.createOffer());
      if (gen !== myGen) return;

      let gatherTimedOut = false;
      if (pc.iceGatheringState !== "complete") {
        await new Promise<void>((resolve) => {
          const timer = setTimeout(() => {
            gatherTimedOut = true;
            pc.onicegatheringstatechange = null;
            resolve();
          }, ICE_GATHER_TIMEOUT_MS);
          pc.onicegatheringstatechange = () => {
            if (pc.iceGatheringState === "complete" || gen !== myGen) {
              clearTimeout(timer);
              pc.onicegatheringstatechange = null;
              resolve();
            }
          };
        });
        if (gen !== myGen) return;
      }
      console.info(
        `[ICE] Gathered in ${Math.round(performance.now() - gatherStartMs)}ms (${gatherTimedOut ? "timeout" : pc.iceGatheringState})`,
      );
    } catch (err) {
      if (gen !== myGen) return;
      waitForStream(
        `offer-failed: ${err instanceof Error ? err.message : String(err)}`,
        "stream-error",
      );
      return;
    }

    const client = ensureMqtt();
    const answerTopic = `stream/${streamId}/answer/${viewerId}`;
    let offerSent = false;
    let offerPublishedAtMs: number | null = null;
    let answerApplied = false;

    phaseTimer = setTimeout(() => {
      if (gen !== myGen) return;
      waitForStream(
        `signaling-timeout; mqtt=${client.connected}, offerSent=${offerPublishedAtMs != null}, online=${navigator.onLine}`,
        navigator.onLine === false
          ? "no-internet"
          : !client.connected
            ? "server-offline"
            : "waiting",
      );
    }, OFFER_TIMEOUT_MS);

    onAnswerMessage = async (topic, payload) => {
      if (topic !== answerTopic || gen !== myGen || answerApplied) return;
      try {
        const { iv, data } = JSON.parse(payload.toString());
        const decrypted = await crypto.subtle.decrypt(
          { name: "AES-GCM", iv: fromBase64Url(iv) },
          key,
          fromBase64Url(data),
        );
        if (gen !== myGen || answerApplied) return;
        const { sdp: answerSdp } = JSON.parse(
          new TextDecoder().decode(decrypted),
        );

        answerApplied = true;
        clearSignalingHooks();
        clearTimeout(phaseTimer);
        phaseTimer = setTimeout(() => {
          if (gen !== myGen || pc.connectionState === "connected") return;
          waitForStream(`handshake-timeout; ${transportInfo(pc)}`, "timed-out");
        }, HANDSHAKE_TIMEOUT_MS);

        const rttNote =
          offerPublishedAtMs != null
            ? ` in ${Math.round(performance.now() - offerPublishedAtMs)}ms`
            : "";
        console.info(`[MQTT] Answer received${rttNote}`);
        await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
      } catch (err) {
        if (gen !== myGen) return;
        if (answerApplied) {
          waitForStream(
            `answer-failed: ${err instanceof Error ? err.message : String(err)}`,
            "stream-error",
          );
        } else {
          console.warn("[MQTT] Ignored invalid answer:", err);
        }
      }
    };

    const sendOffer = () => {
      if (gen !== myGen || offerSent) return;
      offerSent = true;
      activeAnswerTopic = answerTopic;
      client.subscribe(answerTopic, async (err) => {
        if (gen !== myGen) return;
        if (err) {
          waitForStream(`subscribe-failed: ${err.message}`, "server-offline");
          return;
        }
        try {
          const iv = crypto.getRandomValues(new Uint8Array(12));
          const cipher = await crypto.subtle.encrypt(
            { name: "AES-GCM", iv },
            key,
            new TextEncoder().encode(
              JSON.stringify({
                ts: Date.now(),
                sdp: pc.localDescription!.sdp,
              }),
            ),
          );
          if (gen !== myGen) return;
          offerPublishedAtMs = performance.now();
          client.publish(
            `stream/${streamId}/offer`,
            JSON.stringify({
              viewerId,
              iv: toBase64Url(iv),
              data: toBase64Url(new Uint8Array(cipher)),
            }),
          );
          console.info(`[MQTT] Offer published (${viewerId.slice(0, 6)})`);
        } catch (encErr) {
          if (gen !== myGen) return;
          waitForStream(
            `publish-failed: ${encErr instanceof Error ? encErr.message : String(encErr)}`,
            "server-offline",
          );
        }
      });
    };

    if (client.connected) {
      sendOffer();
    } else {
      onMqttConnected = sendOffer;
    }
  };

  const onPause = () => {
    if (!videoEl.paused || state !== "live") return;
    const stream = videoEl.srcObject as MediaStream | null;
    if (!stream?.getTracks().some((t) => t.readyState === "live")) return;
    pause();
  };

  const onPlay = () => {
    if (state === "paused") {
      play();
    }
  };

  const onOnline = () => {
    if (isStandby(state)) {
      void connect("online");
    } else {
      console.info(`[Network] Online (state=${state})`);
    }
  };

  const onOffline = () => {
    console.warn(`[Network] Offline (state=${state})`);
    if (isStandby(state) && state !== "no-internet") {
      transition("no-internet", "network-offline");
    }
  };

  const onPageHide = () => {
    gen++;
    clearTimers();
    closeMqtt();
    closePc();
  };

  const onPageShow = (e: PageTransitionEvent) => {
    if (e.persisted && state !== "paused") {
      void connect("bfcache");
    }
  };

  videoEl.addEventListener("pause", onPause);
  videoEl.addEventListener("play", onPlay);
  window.addEventListener("online", onOnline);
  window.addEventListener("offline", onOffline);
  window.addEventListener("pagehide", onPageHide);
  window.addEventListener("pageshow", onPageShow);

  const initGen = gen;
  void (async () => {
    const tokenBytes = new TextEncoder().encode(token);
    const hmacKey = await crypto.subtle.importKey(
      "raw",
      tokenBytes,
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign"],
    );
    streamId = toBase64Url(
      new Uint8Array(
        await crypto.subtle.sign(
          "HMAC",
          hmacKey,
          new TextEncoder().encode("stream-id"),
        ),
      ),
    );
    eventsTopic = `stream/${streamId}/events`;
    aesKey = await crypto.subtle.importKey(
      "raw",
      await crypto.subtle.digest("SHA-256", tokenBytes),
      { name: "AES-GCM" },
      false,
      ["encrypt", "decrypt"],
    );
    if (gen !== initGen) return;
    void connect();
  })();

  return {
    play,
    pause,
    destroy: () => {
      destroyed = true;
      gen++;
      clearTimers();
      closeMqtt();
      closePc();
      videoEl.srcObject = null;
      videoEl.removeEventListener("error", onVideoError);
      videoEl.removeEventListener("pause", onPause);
      videoEl.removeEventListener("play", onPlay);
      window.removeEventListener("hashchange", onHashChange);
      window.removeEventListener("online", onOnline);
      window.removeEventListener("offline", onOffline);
      window.removeEventListener("pagehide", onPageHide);
      window.removeEventListener("pageshow", onPageShow);
    },
  };
}
