const room = document.querySelector('#room');
const turnUser = document.querySelector('#turn-user');
const turnPassword = document.querySelector('#turn-password');
const logBox = document.querySelector('#log');
const button = document.querySelector('#start');
const me = Array.from(crypto.getRandomValues(new Uint8Array(32)), byte => byte.toString(16).padStart(2, '0')).join('');
const peers = new Map();
let socket;
let pendingMessages = Promise.resolve();

function log(message) {
  logBox.textContent += `${new Date().toLocaleTimeString()} ${message}\n`;
}

function send(type, to, payload) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type, to, payload }));
}

function iceServers() {
  const servers = [{ urls: 'stun:stun.cloudflare.com:3478' }];
  if (turnUser.value && turnPassword.value) {
    servers.push({ urls: [
      'turn:turn.cloudflare.com:3478?transport=udp',
      'turn:turn.cloudflare.com:443?transport=udp',
      'turn:turn.cloudflare.com:3478?transport=tcp',
      'turns:turn.cloudflare.com:5349?transport=tcp',
    ], username: turnUser.value, credential: turnPassword.value });
  }
  return servers;
}

async function pathReport(pc) {
  const stats = await pc.getStats();
  const transport = [...stats.values()].find(item => item.type === 'transport' && item.selectedCandidatePairId);
  const pair = transport && stats.get(transport.selectedCandidatePairId);
  const local = pair && stats.get(pair.localCandidateId);
  const remote = pair && stats.get(pair.remoteCandidateId);
  return local && remote ? `${local.candidateType} → ${remote.candidateType}${local.candidateType === 'relay' || remote.candidateType === 'relay' ? ' (TURN中継)' : ' (直接接続)'}` : '経路情報を取得できません';
}

function wireChannel(channel, remote) {
  channel.binaryType = 'arraybuffer';
  channel.onopen = async () => {
    log(`${remote.slice(0, 8)} DataChannel接続。${await pathReport(peers.get(remote).pc)}`);
    channel.send(new TextEncoder().encode(`ping:${Date.now()}\n`));
  };
  channel.onmessage = event => {
    const text = typeof event.data === 'string' ? event.data : new TextDecoder().decode(event.data);
    if (text.startsWith('ping:')) channel.send(new TextEncoder().encode(`pong:${text.slice(5).trim()}\n`));
    else if (text.startsWith('pong:')) log(`${remote.slice(0, 8)} 応答 ${Date.now() - Number(text.slice(5).trim())} ms`);
  };
  channel.onclose = () => log(`${remote.slice(0, 8)} DataChannel切断`);
}

function connection(remote) {
  if (peers.has(remote)) return peers.get(remote);
  const pc = new RTCPeerConnection({ iceServers: iceServers() });
  const state = { pc, pending: [] };
  peers.set(remote, state);
  pc.onicecandidate = event => {
    if (event.candidate) send('candidate', remote, event.candidate.toJSON());
  };
  pc.ondatachannel = event => wireChannel(event.channel, remote);
  pc.onconnectionstatechange = () => log(`${remote.slice(0, 8)}: ${pc.connectionState}`);
  return state;
}

async function addCandidate(state, candidate) {
  if (state.pc.remoteDescription) await state.pc.addIceCandidate(candidate);
  else state.pending.push(candidate);
}

async function flushCandidates(state) {
  for (const candidate of state.pending) await state.pc.addIceCandidate(candidate);
  state.pending.length = 0;
}

async function initiate(remote) {
  const state = connection(remote);
  wireChannel(state.pc.createDataChannel('p2p-diagnostic', { ordered: true }), remote);
  await state.pc.setLocalDescription(await state.pc.createOffer());
  send('offer', remote, state.pc.localDescription.toJSON());
}

async function handle(message) {
  if (message.type === 'peers') {
    for (const peer of message.peers) if (me < peer) await initiate(peer);
    return;
  }
  if (message.type === 'joined') {
    if (me < message.peer) await initiate(message.peer);
    return;
  }
  if (message.type === 'left') {
    peers.get(message.peer)?.pc.close();
    peers.delete(message.peer);
    return;
  }
  if (!message.from || message.from === me) return;
  const state = connection(message.from);
  if (message.type === 'offer') {
    await state.pc.setRemoteDescription(message.payload);
    await flushCandidates(state);
    await state.pc.setLocalDescription(await state.pc.createAnswer());
    send('answer', message.from, state.pc.localDescription.toJSON());
  } else if (message.type === 'answer') {
    await state.pc.setRemoteDescription(message.payload);
    await flushCandidates(state);
  } else if (message.type === 'candidate') {
    await addCandidate(state, message.payload);
  }
}

button.onclick = () => {
  if (!/^[a-f0-9]{32}$/.test(room.value)) { log('ルーム値は16進数32桁で入力してください。'); return; }
  button.disabled = true;
  const url = new URL('/signal', location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('room', room.value);
  url.searchParams.set('peer', me);
  socket = new WebSocket(url);
  socket.onopen = () => log(`待機中: この端末 ${me.slice(0, 8)}`);
  socket.onmessage = event => {
    pendingMessages = pendingMessages.then(() => handle(JSON.parse(event.data)))
      .catch(error => log(`接続処理エラー: ${error.message}`));
  };
  socket.onclose = event => {
    log(`シグナリング切断: ${event.code}`);
    button.disabled = false;
    for (const state of peers.values()) state.pc.close();
    peers.clear();
  };
  socket.onerror = () => log('シグナリング接続エラー');
};
