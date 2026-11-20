// Signaling carries only WebRTC offers, answers, and ICE candidates.
// File data must never be sent through this Worker.
export interface Env { ROOM: DurableObjectNamespace }

const MAX_MESSAGE = 16 * 1024;
const MAX_PEERS = 16;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname !== "/signal" || request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("WebSocket signaling only", { status: 404 });
    }
    const room = url.searchParams.get("room");
    const peer = url.searchParams.get("peer");
    if (!room || !peer || !/^[a-f0-9]{32}$/.test(room) || !/^[a-f0-9]{64}$/.test(peer)) {
      return new Response("Invalid room or peer", { status: 400 });
    }
    return env.ROOM.get(env.ROOM.idFromName(room)).fetch(request);
  },
};

export class SignalRoom implements DurableObject {
  constructor(private state: DurableObjectState) {}

  async fetch(request: Request): Promise<Response> {
    const peer = new URL(request.url).searchParams.get("peer")!;
    const sockets = this.state.getWebSockets();
    if (sockets.length >= MAX_PEERS || sockets.some(socket => socket.deserializeAttachment() === peer)) {
      return new Response("Room full or peer already connected", { status: 409 });
    }
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    server.serializeAttachment(peer);
    this.state.acceptWebSocket(server);
    server.send(JSON.stringify({ type: "peers", peers: sockets.map(socket => socket.deserializeAttachment()) }));
    for (const socket of sockets) socket.send(JSON.stringify({ type: "joined", peer }));
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(socket: WebSocket, data: string | ArrayBuffer): Promise<void> {
    if (typeof data !== "string" || data.length > MAX_MESSAGE) { socket.close(1009, "Message too large"); return; }
    let message: { type?: unknown; to?: unknown; payload?: unknown };
    try { message = JSON.parse(data); } catch { socket.close(1003, "Invalid JSON"); return; }
    if (!["offer", "answer", "candidate"].includes(String(message.type)) ||
        typeof message.to !== "string" || !/^[a-f0-9]{64}$/.test(message.to) ||
        typeof message.payload !== "object" || message.payload === null) {
      socket.close(1003, "Invalid signal"); return;
    }
    const target = this.state.getWebSockets().find(item => item.deserializeAttachment() === message.to);
    if (target) target.send(JSON.stringify({ type: message.type, from: socket.deserializeAttachment(), payload: message.payload }));
  }

  async webSocketClose(socket: WebSocket): Promise<void> {
    const peer = socket.deserializeAttachment();
    for (const other of this.state.getWebSockets()) {
      if (other !== socket) other.send(JSON.stringify({ type: "left", peer }));
    }
  }
}
