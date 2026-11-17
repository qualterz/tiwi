import mqtt from "mqtt";

const statusEl = document.getElementById("status")!;
const videoEl = document.getElementById("player") as HTMLVideoElement;

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

  // 2. Setup RTCPeerConnection & bind tracks to HTML5 <video>
  const viewerId = crypto.randomUUID();
  const pc = new RTCPeerConnection({
    iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
  });
  const mediaStream = new MediaStream();
  videoEl.srcObject = mediaStream;

  pc.ontrack = (e) => {
    mediaStream.addTrack(e.track);
    videoEl.play().catch(() => {});
  };

  pc.onconnectionstatechange = () => {
    statusEl.textContent =
      pc.connectionState === "connected"
        ? "Live • Connected (Click unmute on player for audio)"
        : `Connection: ${pc.connectionState}`;
  };

  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  await pc.setLocalDescription(await pc.createOffer());

  if (pc.iceGatheringState !== "complete") {
    await new Promise<void>((resolve) => {
      const timeout = setTimeout(resolve, 1500);
      pc.onicegatheringstatechange = () => {
        if (pc.iceGatheringState === "complete") {
          clearTimeout(timeout);
          resolve();
        }
      };
    });
  }

  // 3. Exchange encrypted Offer/Answer over public MQTT
  const client = mqtt.connect("wss://broker.emqx.io:8084/mqtt");
  const answerTopic = `stream/${streamId}/answer/${viewerId}`;

  client.on("connect", async () => {
    statusEl.textContent = `Connecting to stream ${streamId.slice(0, 8)}...`;
    client.subscribe(answerTopic);

    const iv = crypto.getRandomValues(new Uint8Array(12));
    const cipher = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv },
      aesKey,
      new TextEncoder().encode(
        JSON.stringify({ ts: Date.now(), sdp: pc.localDescription!.sdp }),
      ),
    );

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
    const { iv, data } = JSON.parse(payload.toString());
    const decrypted = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: fromBase64Url(iv) },
      aesKey,
      fromBase64Url(data),
    );
    const { sdp: answerSdp } = JSON.parse(new TextDecoder().decode(decrypted));

    await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
    client.end();
  });
} else {
  statusEl.textContent =
    "Invalid link: restart Bun and open the #<token> URL it prints.";
}
