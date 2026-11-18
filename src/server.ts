import {
  RTCPeerConnection,
  MediaStreamTrack,
  RTCRtpCodecParameters,
  type RTCRtpReceiver,
} from "werift";
import mqtt from "mqtt";

// 1. Generate secret token & derive public streamId via HMAC-SHA256 + AES-GCM key
const token = Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString(
  "base64url",
);
const tokenBytes = new TextEncoder().encode(token);

const hmacKey = await crypto.subtle.importKey(
  "raw",
  tokenBytes,
  { name: "HMAC", hash: "SHA-256" },
  false,
  ["sign"],
);
const streamId = Buffer.from(
  await crypto.subtle.sign(
    "HMAC",
    hmacKey,
    new TextEncoder().encode("stream-id"),
  ),
).toString("base64url");

const aesKey = await crypto.subtle.importKey(
  "raw",
  await crypto.subtle.digest("SHA-256", tokenBytes),
  { name: "AES-GCM" },
  false,
  ["encrypt", "decrypt"],
);

// 2. Candidate codecs so OBS can connect with AV1, H.264, HEVC, VP9, or VP8
const rtcpFeedback = [
  { type: "nack" },
  { type: "nack", parameter: "pli" },
  { type: "ccm", parameter: "fir" },
];

const supportedCodecs = {
  video: [
    new RTCRtpCodecParameters({
      mimeType: "video/AV1",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/H264",
      clockRate: 90000,
      rtcpFeedback,
      parameters:
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/H265",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/VP9",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/VP8",
      clockRate: 90000,
      rtcpFeedback,
    }),
  ],
  audio: [
    new RTCRtpCodecParameters({
      mimeType: "audio/opus",
      clockRate: 48000,
      channels: 2,
    }),
  ],
};

// 3. Shared outbound media tracks fed by OBS + dynamically resolved video codec
let activeVideoCodec = supportedCodecs.video[0]!;
const sharedVideoTrack = new MediaStreamTrack({
  kind: "video",
  codec: activeVideoCodec,
});
const sharedAudioTrack = new MediaStreamTrack({
  kind: "audio",
  codec: supportedCodecs.audio[0],
});
const peers = new Map<string, RTCPeerConnection>();

let obsVideoReceiver: RTCRtpReceiver | null = null;
let obsVideoSsrc: number | undefined;

// 4. Start local WHIP server for OBS (http://127.0.0.1:8080/whip)
Bun.serve({
  port: 8080,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);

    if (req.method === "POST" && url.pathname === "/whip") {
      const obsPc = new RTCPeerConnection({ codecs: supportedCodecs });

      obsPc.onTrack.subscribe((track) => {
        if (track.kind === "video") {
          const videoTransceiver = obsPc
            .getTransceivers()
            .find((t) => t.kind === "video");

          obsVideoReceiver = videoTransceiver?.receiver ?? null;

          track.onReceiveRtp.subscribe((rtp) => {
            if (obsVideoSsrc !== rtp.header.ssrc) {
              obsVideoSsrc = rtp.header.ssrc;
              const matched = videoTransceiver?.codecs.find(
                (c) => c.payloadType === rtp.header.payloadType,
              );
              if (matched) {
                activeVideoCodec = matched;
                sharedVideoTrack.codec = matched;
                console.info(
                  `[OBS] Dynamically resolved video codec: ${matched.mimeType} (PT=${matched.payloadType})`,
                );
              }
            }
            sharedVideoTrack.writeRtp(rtp);
          });
        }

        if (track.kind === "audio") {
          track.onReceiveRtp.subscribe((rtp) => {
            sharedAudioTrack.writeRtp(rtp);
          });
        }
      });

      await obsPc.setRemoteDescription({
        type: "offer",
        sdp: await req.text(),
      });
      await obsPc.setLocalDescription(await obsPc.createAnswer());

      const videoTransceiver = obsPc
        .getTransceivers()
        .find((t) => t.kind === "video");

      if (videoTransceiver?.codecs[0]) {
        activeVideoCodec = videoTransceiver.codecs[0];
        sharedVideoTrack.codec = activeVideoCodec;
        console.info(
          `[OBS] Connected via WHIP (${activeVideoCodec.mimeType})! Streaming live...`,
        );
      }

      return new Response(obsPc.localDescription!.sdp, {
        status: 201,
        headers: {
          "Content-Type": "application/sdp",
          Location: "/whip/session",
        },
      });
    }

    return new Response("Not Found", { status: 404 });
  },
});

console.info("OBS WHIP Server: http://127.0.0.1:8080/whip");
console.info(`Stream Public ID (HMAC): ${streamId}`);
console.info(
  `\nShare this 1-click private link with peers:\nhttp://localhost:3000/#${token}\n`,
);

// 5. Listen for encrypted peer Offers over public MQTT
const client = mqtt.connect("wss://broker.emqx.io:8084/mqtt");
const offerTopic = `stream/${streamId}/offer`;

client.on("connect", () => {
  client.subscribe(offerTopic, (err) => {
    if (err) console.error("[MQTT] Subscribe error:", err);
  });
  console.info("[MQTT] Connected and listening for peers...");
});

client.on("error", (err) => console.error("[MQTT] Error:", err));

client.on("message", async (_topic, rawPayload) => {
  try {
    const { viewerId, iv, data } = JSON.parse(rawPayload.toString());
    if (!viewerId) return;
    if (peers.has(viewerId) || peers.size >= 10) {
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Rejected (duplicate or max peers ${peers.size}/10)`,
      );
      return;
    }

    const decrypted = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: Buffer.from(iv, "base64url") },
      aesKey,
      Buffer.from(data, "base64url"),
    );
    const { ts, sdp: offerSdp } = JSON.parse(
      new TextDecoder().decode(decrypted),
    );
    const skewMs = Date.now() - ts;
    if (Math.abs(skewMs) > 30_000) {
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Rejected offer due to clock skew (${skewMs}ms)`,
      );
      return;
    }

    const pc = new RTCPeerConnection({
      codecs: {
        video: [activeVideoCodec],
        audio: supportedCodecs.audio,
      },
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
    });
    peers.set(viewerId, pc);
    pc.addTrack(sharedVideoTrack);
    pc.addTrack(sharedAudioTrack);

    pc.onIceCandidate.subscribe((c) => {
      if (c)
        console.debug(
          `[Peer ${viewerId.slice(0, 6)}] ICE candidate:`,
          c.candidate,
        );
    });

    pc.connectionStateChange.subscribe((state) => {
      console.info(
        `[Peer ${viewerId.slice(0, 6)}] ${state} (${peers.size}/10)`,
      );
      if (state === "connected" && obsVideoReceiver && obsVideoSsrc) {
        obsVideoReceiver.sendRtcpPLI(obsVideoSsrc);
        setTimeout(() => obsVideoReceiver?.sendRtcpPLI(obsVideoSsrc!), 500);
      }
      if (
        state === "closed" ||
        state === "failed" ||
        state === "disconnected"
      ) {
        pc.close();
        peers.delete(viewerId);
      }
    });

    await pc.setRemoteDescription({ type: "offer", sdp: offerSdp });
    await pc.setLocalDescription(await pc.createAnswer());

    if (pc.iceGatheringState !== "complete") {
      await new Promise<void>((resolve) => {
        pc.iceGatheringStateChange.subscribe((state) => {
          if (state === "complete") resolve();
        });
      });
    }

    const ansIv = crypto.getRandomValues(new Uint8Array(12));
    const ansCipher = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv: ansIv },
      aesKey,
      new TextEncoder().encode(
        JSON.stringify({ sdp: pc.localDescription!.sdp }),
      ),
    );

    client.publish(
      `stream/${streamId}/answer/${viewerId}`,
      JSON.stringify({
        iv: Buffer.from(ansIv).toString("base64url"),
        data: Buffer.from(ansCipher).toString("base64url"),
      }),
    );
  } catch (err) {
    console.warn("[MQTT] Ignored invalid or unauthorized offer:", err);
  }
});
