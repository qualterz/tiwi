import mqtt from "mqtt";

const METRICS_INTERVAL_MS = 5000;

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

window.onhashchange = () => location.reload();

export async function startViewer(
  videoEl: HTMLVideoElement,
  setStatus: (status: string) => void,
) {
  videoEl.onerror = () => {
    console.error("[Video] Playback error:", videoEl.error);
  };

  const token = location.hash.slice(1);

  if (token) {
    // 1. Derive public streamId via HMAC-SHA256 and AES-GCM key from token
    const tokenBytes = new TextEncoder().encode(token);

    const hmacKey = await crypto.subtle.importKey(
      "raw",
      tokenBytes,
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign"],
    );
    const streamId = toBase64Url(
      new Uint8Array(
        await crypto.subtle.sign(
          "HMAC",
          hmacKey,
          new TextEncoder().encode("stream-id"),
        ),
      ),
    );

    const aesKey = await crypto.subtle.importKey(
      "raw",
      await crypto.subtle.digest("SHA-256", tokenBytes),
      { name: "AES-GCM" },
      false,
      ["encrypt", "decrypt"],
    );

    let activePc: RTCPeerConnection | null = null;
    let activeClient: mqtt.MqttClient | null = null;
    let activeStatsTimer: ReturnType<typeof setTimeout> | undefined;

    const disconnect = (reason: string) => {
      clearTimeout(activeStatsTimer);
      activeStatsTimer = undefined;
      if (activeClient) {
        activeClient.end();
        activeClient = null;
      }
      if (activePc) {
        activePc.onconnectionstatechange = null;
        activePc.close();
        activePc = null;
      }
      videoEl.srcObject = null;
      setStatus(reason);
    };

    videoEl.onpause = () => {
      if (activePc) disconnect("Paused");
    };
    videoEl.onplay = () => {
      if (!activePc) void connect();
    };

    const connect = async () => {
    // 2. Setup RTCPeerConnection & bind tracks to HTML5 <video>
    const viewerId = crypto.randomUUID();
    console.info("[Init] Stream:", { streamId, viewerId });

    const pc = new RTCPeerConnection({
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
    });
    activePc = pc;
    const mediaStream = new MediaStream();
    videoEl.srcObject = mediaStream;

    let activeProfile = "stable";

    pc.ontrack = (e) => {
      console.info("[WebRTC] Track received:", e.track.kind);
      if (activeProfile === "stable" && "jitterBufferTarget" in e.receiver) {
        (
          e.receiver as RTCRtpReceiver & { jitterBufferTarget: number }
        ).jitterBufferTarget = 300;
      }
      mediaStream.addTrack(e.track);
      videoEl
        .play()
        .catch((err) => console.warn("[Video] Autoplay blocked:", err));
    };

    pc.onicecandidate = (e) => {
      if (e.candidate) {
        console.debug("[ICE] Candidate:", e.candidate.candidate);
      } else {
        console.info("[ICE] Gathering complete");
      }
    };

    const startedAtMs = performance.now();
    const metrics = {
      streamId,
      viewerId,
      startedAt: new Date().toISOString(),
      uptimeSec: 0,
      connectionState: pc.connectionState,
      iceConnectionState: pc.iceConnectionState,
      iceGatheringState: pc.iceGatheringState,
      signaling: {
        iceGatherDurationMs: 0,
        mqttConnectedAt: null as string | null,
        offerPublishedAt: null as string | null,
        answerReceivedAt: null as string | null,
        connectedAt: null as string | null,
        iceCandidateErrors: 0,
      },
      transport: {
        localCandidate: null as string | null,
        remoteCandidate: null as string | null,
        candidateType: null as string | null,
        protocol: null as string | null,
        currentRttMs: null as number | null,
        avgRttMs: null as number | null,
        oneWayPingMs: null as number | null,
        availableIncomingBitrateKbps: null as number | null,
        bytesReceived: 0,
        packetsReceived: 0,
      },
      video: {
        ssrc: null as number | null,
        codec: null as string | null,
        width: 0,
        height: 0,
        fps: 0,
        kbps: 0,
        peakKbps: 0,
        avgKbps: 0,
        pps: 0,
        latency: {
          jitterMs: 0,
          jitterBufferMs: 0,
          avgJitterBufferMs: 0,
          jitterBufferTargetMs: 0,
          jitterBufferMinMs: 0,
          assemblyMs: 0,
          decodeMs: 0,
          avgDecodeMs: 0,
          processingMs: 0,
          avgProcessingMs: 0,
          interFrameDelayMs: 0,
          estimatedEndToEndMs: 0,
        },
        packetsReceived: 0,
        bytesReceived: 0,
        packetsLost: 0,
        framesReceived: 0,
        framesDecoded: 0,
        keyFramesDecoded: 0,
        framesDropped: 0,
        freezeCount: 0,
        totalFreezesDurationSec: 0,
        nackCount: 0,
        pliCount: 0,
        firCount: 0,
        retransmittedPacketsReceived: 0,
        decoderImplementation: null as string | null,
        powerEfficientDecoder: null as boolean | null,
      },
      audio: {
        ssrc: null as number | null,
        codec: null as string | null,
        kbps: 0,
        peakKbps: 0,
        avgKbps: 0,
        pps: 0,
        latency: {
          jitterMs: 0,
          jitterBufferMs: 0,
          avgJitterBufferMs: 0,
          jitterBufferTargetMs: 0,
          jitterBufferMinMs: 0,
          playoutDelayMs: null as number | null,
          estimatedEndToEndMs: 0,
        },
        packetsReceived: 0,
        bytesReceived: 0,
        packetsLost: 0,
        concealedSamples: 0,
        concealmentEvents: 0,
        totalSamplesReceived: 0,
      },
    };

    let statsTimer: ReturnType<typeof setTimeout> | undefined;
    const prevRtp = new Map<
      string,
      {
        ts: number;
        bytes: number;
        packets: number;
        jbDelay: number;
        jbTarget: number;
        jbMin: number;
        jbEmitted: number;
        decodeTime: number;
        processingDelay: number;
        interFrameDelay: number;
        framesDecoded: number;
        assemblyTime: number;
        framesAssembled: number;
        playoutDelay: number;
        playoutSamples: number;
      }
    >();
    const startRtp = new Map<string, { ts: number; bytes: number }>();

    const updateAndLogMetrics = async (label = "[Metrics]") => {
      const report = await pc.getStats();
      metrics.uptimeSec = Math.round((performance.now() - startedAtMs) / 1000);
      metrics.connectionState = pc.connectionState;
      metrics.iceConnectionState = pc.iceConnectionState;
      metrics.iceGatheringState = pc.iceGatheringState;

      // 1. Update transport ping / RTT first so media end-to-end latency can include oneWayPingMs
      for (const s of report.values()) {
        if (
          s.type === "candidate-pair" &&
          ((s as RTCIceCandidatePairStats).nominated ||
            (s as RTCIceCandidatePairStats).state === "succeeded")
        ) {
          const pair = s as RTCIceCandidatePairStats & Record<string, unknown>;
          const local = report.get(pair.localCandidateId) as
            Record<string, unknown> | undefined;
          const remote = report.get(pair.remoteCandidateId) as
            Record<string, unknown> | undefined;
          metrics.transport.localCandidate = local
            ? `${local.address ?? local.ip}:${local.port} (${local.candidateType})`
            : null;
          metrics.transport.remoteCandidate = remote
            ? `${remote.address ?? remote.ip}:${remote.port} (${remote.candidateType})`
            : null;
          metrics.transport.candidateType =
            (local?.candidateType as string | undefined) ?? null;
          metrics.transport.protocol =
            (local?.protocol as string | undefined) ?? null;

          const rttMs =
            pair.currentRoundTripTime != null
              ? Math.round(pair.currentRoundTripTime * 1000)
              : null;
          const rttMeasurements =
            (pair.roundTripTimeMeasurements as number | undefined) ?? 0;
          const totalRttSec =
            (pair.totalRoundTripTime as number | undefined) ?? 0;

          metrics.transport.currentRttMs = rttMs;
          metrics.transport.avgRttMs =
            rttMeasurements > 0
              ? Math.round((totalRttSec / rttMeasurements) * 1000)
              : rttMs;
          metrics.transport.oneWayPingMs =
            rttMs != null ? Math.round(rttMs / 2) : null;
          metrics.transport.availableIncomingBitrateKbps =
            pair.availableIncomingBitrate != null
              ? Math.round(pair.availableIncomingBitrate / 1000)
              : null;
          metrics.transport.bytesReceived = pair.bytesReceived ?? 0;
          metrics.transport.packetsReceived =
            (pair.packetsReceived as number | undefined) ?? 0;
        }
      }

      const oneWayMs = metrics.transport.oneWayPingMs ?? 0;

      // 2. Update inbound RTP + media-playout latency & throughput
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
          metrics.audio.latency.playoutDelayMs =
            dSamples > 0 ? Math.round((dDelay / dSamples) * 1000) : null;
          prevRtp.set(s.id, {
            ts: s.timestamp,
            bytes: 0,
            packets: 0,
            jbDelay: 0,
            jbTarget: 0,
            jbMin: 0,
            jbEmitted: 0,
            decodeTime: 0,
            processingDelay: 0,
            interFrameDelay: 0,
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
          const jbMin = (r.jitterBufferMinimumDelay as number | undefined) ?? 0;
          const jbEmitted =
            (r.jitterBufferEmittedCount as number | undefined) ?? 0;
          const decodeTime = (r.totalDecodeTime as number | undefined) ?? 0;
          const processingDelay =
            (r.totalProcessingDelay as number | undefined) ?? 0;
          const interFrameDelay =
            (r.totalInterFrameDelay as number | undefined) ?? 0;
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
          const avgJitterBufferMs =
            jbEmitted > 0 ? Math.round((jbDelay / jbEmitted) * 1000) : 0;
          const jitterBufferTargetMs =
            dEmitted > 0
              ? Math.round(
                  ((jbTarget - (prev?.jbTarget ?? 0)) / dEmitted) * 1000,
                )
              : jbEmitted > 0
                ? Math.round((jbTarget / jbEmitted) * 1000)
                : 0;
          const jitterBufferMinMs =
            dEmitted > 0
              ? Math.round(((jbMin - (prev?.jbMin ?? 0)) / dEmitted) * 1000)
              : jbEmitted > 0
                ? Math.round((jbMin / jbEmitted) * 1000)
                : 0;

          prevRtp.set(r.id, {
            ts: r.timestamp,
            bytes,
            packets,
            jbDelay,
            jbTarget,
            jbMin,
            jbEmitted,
            decodeTime,
            processingDelay,
            interFrameDelay,
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
            const avgDecodeMs =
              framesDecoded > 0
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
            const avgProcessingMs =
              framesDecoded > 0
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
            const interFrameDelayMs =
              dDecoded > 0
                ? Math.round(
                    ((interFrameDelay - (prev?.interFrameDelay ?? 0)) /
                      dDecoded) *
                      1000,
                  )
                : 0;
            const pipelineMs =
              processingMs > 0
                ? processingMs
                : jitterBufferMs + assemblyMs + decodeMs;

            metrics.video.ssrc = r.ssrc;
            metrics.video.codec = codecStat?.mimeType ?? metrics.video.codec;
            metrics.video.width = r.frameWidth ?? videoEl.videoWidth;
            metrics.video.height = r.frameHeight ?? videoEl.videoHeight;
            metrics.video.fps =
              r.framesPerSecond != null
                ? Math.round(r.framesPerSecond)
                : dtSec > 0
                  ? Math.round(dDecoded / dtSec)
                  : 0;
            metrics.video.kbps = kbps;
            metrics.video.peakKbps = Math.max(metrics.video.peakKbps, kbps);
            metrics.video.avgKbps = avgKbps;
            metrics.video.pps = pps;
            metrics.video.latency = {
              jitterMs,
              jitterBufferMs,
              avgJitterBufferMs,
              jitterBufferTargetMs,
              jitterBufferMinMs,
              assemblyMs,
              decodeMs,
              avgDecodeMs,
              processingMs,
              avgProcessingMs,
              interFrameDelayMs,
              estimatedEndToEndMs: oneWayMs + pipelineMs,
            };
            metrics.video.packetsReceived = packets;
            metrics.video.bytesReceived = bytes;
            metrics.video.packetsLost = r.packetsLost ?? 0;
            metrics.video.framesReceived =
              (r.framesReceived as number | undefined) ?? 0;
            metrics.video.framesDecoded = framesDecoded;
            metrics.video.keyFramesDecoded = r.keyFramesDecoded ?? 0;
            metrics.video.framesDropped = r.framesDropped ?? 0;
            metrics.video.freezeCount =
              (r.freezeCount as number | undefined) ?? 0;
            metrics.video.totalFreezesDurationSec = Number(
              ((r.totalFreezesDuration as number | undefined) ?? 0).toFixed(2),
            );
            metrics.video.nackCount = r.nackCount ?? 0;
            metrics.video.pliCount = r.pliCount ?? 0;
            metrics.video.firCount = r.firCount ?? 0;
            metrics.video.retransmittedPacketsReceived =
              (r.retransmittedPacketsReceived as number | undefined) ?? 0;
            metrics.video.decoderImplementation =
              (r.decoderImplementation as string | undefined) ?? null;
            metrics.video.powerEfficientDecoder =
              (r.powerEfficientDecoder as boolean | undefined) ?? null;
          } else if (r.kind === "audio") {
            metrics.audio.ssrc = r.ssrc;
            metrics.audio.codec = codecStat?.mimeType ?? metrics.audio.codec;
            metrics.audio.kbps = kbps;
            metrics.audio.peakKbps = Math.max(metrics.audio.peakKbps, kbps);
            metrics.audio.avgKbps = avgKbps;
            metrics.audio.pps = pps;
            metrics.audio.latency = {
              ...metrics.audio.latency,
              jitterMs,
              jitterBufferMs,
              avgJitterBufferMs,
              jitterBufferTargetMs,
              jitterBufferMinMs,
              estimatedEndToEndMs:
                oneWayMs +
                jitterBufferMs +
                (metrics.audio.latency.playoutDelayMs ?? 0),
            };
            metrics.audio.packetsReceived = packets;
            metrics.audio.bytesReceived = bytes;
            metrics.audio.packetsLost = r.packetsLost ?? 0;
            metrics.audio.concealedSamples =
              (r.concealedSamples as number | undefined) ?? 0;
            metrics.audio.concealmentEvents =
              (r.concealmentEvents as number | undefined) ?? 0;
            metrics.audio.totalSamplesReceived =
              (r.totalSamplesReceived as number | undefined) ?? 0;
          }
        }
      }

      const v = metrics.video;
      const a = metrics.audio;
      const t = metrics.transport;
      console.info(
        [
          `${label} up=${metrics.uptimeSec}s profile=${activeProfile} [${metrics.connectionState}/${metrics.iceConnectionState}] rtt=${t.currentRttMs ?? "-"}ms (avg=${t.avgRttMs ?? "-"}ms ping=${t.oneWayPingMs ?? "-"}ms) ${t.protocol ?? "-"} ${t.localCandidate ?? "-"} -> ${t.remoteCandidate ?? "-"}`,
          `  video ${v.codec ?? "-"} (${v.decoderImplementation ?? "-"}${v.powerEfficientDecoder != null ? ` hw=${v.powerEfficientDecoder}` : ""}) ${v.width}x${v.height}@${v.fps}fps | rate=${v.kbps}kbps (avg=${v.avgKbps} peak=${v.peakKbps}) ${v.pps}pps | e2e=${v.latency.estimatedEndToEndMs}ms (jb=${v.latency.jitterBufferMs}ms target=${v.latency.jitterBufferTargetMs}ms dec=${v.latency.decodeMs}ms proc=${v.latency.processingMs}ms jitter=${v.latency.jitterMs}ms) | dec=${v.framesDecoded} key=${v.keyFramesDecoded} drop=${v.framesDropped} lost=${v.packetsLost} rtx=${v.retransmittedPacketsReceived} freeze=${v.freezeCount} (${v.totalFreezesDurationSec}s) nack=${v.nackCount} pli=${v.pliCount}`,
          `  audio ${a.codec ?? "-"} | rate=${a.kbps}kbps ${a.pps}pps | e2e=${a.latency.estimatedEndToEndMs}ms (jb=${a.latency.jitterBufferMs}ms jitter=${a.latency.jitterMs}ms) | lost=${a.packetsLost}`,
        ].join("\n"),
      );
    };

    const scheduleStats = () => {
      statsTimer = setTimeout(
        async () => {
          if (pc.connectionState !== "connected") return;
          await updateAndLogMetrics("[Metrics]");
          if (pc.connectionState === "connected") scheduleStats();
        },
        METRICS_INTERVAL_MS - (Date.now() % METRICS_INTERVAL_MS),
      );
    };

    pc.onicecandidateerror = (e) => {
      metrics.signaling.iceCandidateErrors++;
      const err = e as RTCPeerConnectionIceErrorEvent;
      console.warn(
        "[ICE] Candidate error:",
        err.url,
        err.errorCode,
        err.errorText,
      );
    };

    pc.oniceconnectionstatechange = () => {
      metrics.iceConnectionState = pc.iceConnectionState;
      console.info("[ICE] State:", pc.iceConnectionState);
    };

    pc.onconnectionstatechange = async () => {
      const state = pc.connectionState;
      metrics.connectionState = state;
      console.info("[WebRTC] Connection state:", state);
      setStatus(
        state === "connected"
          ? `Live • Connected (${activeProfile}) — Click unmute on player for audio`
          : `Connection: ${state}`,
      );

      if (state === "connected") {
        metrics.signaling.connectedAt ??= new Date().toISOString();
        if (!statsTimer) scheduleStats();
      }

      if (
        state === "failed" ||
        state === "disconnected" ||
        state === "closed"
      ) {
        clearTimeout(statsTimer);
        statsTimer = undefined;
        await updateAndLogMetrics(
          "[WebRTC] Connection lost/failed. Final metrics:",
        );
      }
    };

    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
    const iceGatherStartMs = performance.now();
    await pc.setLocalDescription(await pc.createOffer());

    if (pc.iceGatheringState !== "complete") {
      await new Promise<void>((resolve) => {
        pc.onicegatheringstatechange = () => {
          if (pc.iceGatheringState === "complete") resolve();
        };
      });
    }
    metrics.signaling.iceGatherDurationMs = Math.round(
      performance.now() - iceGatherStartMs,
    );

    // 3. Exchange encrypted Offer/Answer over public MQTT
    const client = mqtt.connect("wss://broker.emqx.io:8084/mqtt");
    activeClient = client;
    const answerTopic = `stream/${streamId}/answer/${viewerId}`;

    client.on("error", (err) => console.error("[MQTT] Error:", err));

    client.on("connect", async () => {
      metrics.signaling.mqttConnectedAt = new Date().toISOString();
      console.info("[MQTT] Connected, publishing offer...");
      setStatus(`Connecting to stream ${streamId.slice(0, 8)}...`);
      client.subscribe(answerTopic, (err) => {
        if (err) console.error("[MQTT] Subscribe failed:", err);
      });

      const iv = crypto.getRandomValues(new Uint8Array(12));
      const cipher = await crypto.subtle.encrypt(
        { name: "AES-GCM", iv },
        aesKey,
        new TextEncoder().encode(
          JSON.stringify({ ts: Date.now(), sdp: pc.localDescription!.sdp }),
        ),
      );

      metrics.signaling.offerPublishedAt = new Date().toISOString();
      client.publish(
        `stream/${streamId}/offer`,
        JSON.stringify({
          viewerId,
          iv: toBase64Url(iv),
          data: toBase64Url(new Uint8Array(cipher)),
        }),
      );
    });

    client.on("message", async (_topic, payload) => {
      try {
        const { iv, data } = JSON.parse(payload.toString());
        const decrypted = await crypto.subtle.decrypt(
          { name: "AES-GCM", iv: fromBase64Url(iv) },
          aesKey,
          fromBase64Url(data),
        );
        const { sdp: answerSdp, profile } = JSON.parse(
          new TextDecoder().decode(decrypted),
        );
        activeProfile = profile ?? "stable";

        metrics.signaling.answerReceivedAt = new Date().toISOString();
        console.info(
          `[MQTT] Answer received (profile=${activeProfile}), setting remote description`,
        );
        await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
        client.end();
      } catch (err) {
        console.error("[MQTT] Failed to process answer:", err);
      }
    });
    };

    void connect();
  } else {
    console.warn("[Init] Missing #<token> in URL hash");
    setStatus(
      "Invalid link: restart daemon and open the #<token> URL it prints.",
    );
  }
}
