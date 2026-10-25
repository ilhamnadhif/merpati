// Socket.IO interop server for tests/real_servers.rs (works with socket.io
// v2 and v4). Run with NODE_PATH pointing at a node_modules folder holding
// `socket.io`; prints {"version": ..., "ports": {...}} once listening.
//
// Servers: "default" (polling + websocket), "polling" (polling only),
// "websocket" (websocket only), "slow-upgrade" (default, but upgrade probes
// wait a second so that a client stays on polling for a while) and, for v3+,
// "eio3" and "eio3-slow-upgrade" (allowEIO3: true).
'use strict';
const http = require('http');
const sio = require('socket.io');
const version = require('socket.io/package.json').version;
const major = parseInt(version.split('.')[0], 10);

// Received values with every Buffer as {buffer: "<hex>"}, to check that
// binary arguments arrive as Buffers with the exact bytes.
function describe(v) {
  if (Buffer.isBuffer(v)) return { buffer: v.toString('hex') };
  if (v instanceof ArrayBuffer) return { arrayBuffer: Buffer.from(v).toString('hex') };
  if (Array.isArray(v)) return v.map(describe);
  if (v && typeof v === 'object') {
    const out = {};
    for (const k of Object.keys(v)) out[k] = describe(v[k]);
    return out;
  }
  return v;
}

function handlers(socket, log) {
  socket.on('echo', (...args) => {
    const ack = typeof args[args.length - 1] === 'function' ? args.pop() : null;
    socket.emit('echo', ...args);
    if (ack) ack(...args);
  });
  socket.on('info', () => socket.emit('info', {
    nsp: socket.nsp.name,
    id: socket.id,
    query: socket.handshake.query,
    auth: socket.handshake.auth || null,
    xTest: socket.handshake.headers['x-test'] || null,
    cookie: socket.handshake.headers.cookie || null,
    transport: socket.conn.transport.name,
  }));
  socket.on('requests', (cb) => cb(log.filter((r) => r.sid === socket.conn.id)));
  socket.on('transport', (cb) => cb(socket.conn.transport.name));
  socket.on('binary', () => socket.emit('binary', Buffer.from([1, 2, 3]), 'text'));
  socket.on('binary-ack', (cb) => cb({ file: Buffer.from('hi') }));
  socket.on('ask', () => socket.emit('question', 'ready?', (...answer) => socket.emit('answered', ...answer)));
  socket.on('upload', (...args) => {
    const ack = typeof args[args.length - 1] === 'function' ? args.pop() : null;
    const seen = { transport: socket.conn.transport.name, args: describe(args) };
    if (ack) ack(seen);
    else socket.emit('uploaded', seen);
  });
  socket.on('ask-binary', () =>
    socket.emit('question-binary', 'send bytes', (...answer) => socket.emit('answered-binary', describe(answer))));
  socket.on('kick', () => socket.disconnect(true));
}

function create(opts, upgradeDelayMs = 0) {
  const server = http.createServer();
  const io = major >= 3 ? new sio.Server(server, opts) : sio(server, opts);
  if (upgradeDelayMs > 0) {
    // Delay engine.io's handling of upgrade probes (WebSocket requests with a sid).
    const engine = server.listeners('upgrade');
    server.removeAllListeners('upgrade');
    server.on('upgrade', (req, socket, head) => {
      const probe = new URL(req.url, 'http://x').searchParams.has('sid');
      setTimeout(() => engine.forEach((l) => l.call(server, req, socket, head)), probe ? upgradeDelayMs : 0);
    });
  }
  // Added after engine.io attached itself, so it sees every request too.
  const log = [];
  const record = (req) => {
    const url = new URL(req.url, 'http://x');
    log.push({
      sid: url.searchParams.get('sid'),
      transport: url.searchParams.get('transport'),
      method: req.method,
      cookie: req.headers.cookie || null,
      xTest: req.headers['x-test'] || null,
    });
  };
  server.on('request', record);
  // engine.io listens for upgrades only when WebSocket is enabled; an extra
  // listener on a polling-only server would leave upgrade attempts hanging.
  if (!opts.transports || opts.transports.includes('websocket')) server.on('upgrade', record);
  const setup = (socket) => handlers(socket, log);
  io.on('connection', setup);
  io.of('/admin').on('connection', setup);
  const priv = io.of('/private');
  priv.use((socket, next) => {
    const token = (socket.handshake.auth && socket.handshake.auth.token) || socket.handshake.query.token;
    if (token === 'secret') next();
    else next(new Error('not authorized'));
  });
  priv.on('connection', setup);
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)));
}

(async () => {
  const base = {
    pingInterval: 1000,
    pingTimeout: 2000,
    cookie: major >= 3 ? { name: 'sticky', path: '/', httpOnly: true } : 'sticky',
  };
  const ports = {
    default: await create({ ...base }),
    polling: await create({ ...base, transports: ['polling'] }),
    websocket: await create({ ...base, transports: ['websocket'] }),
    'slow-upgrade': await create({ ...base }, 1000),
  };
  if (major >= 3) {
    ports.eio3 = await create({ ...base, allowEIO3: true });
    ports['eio3-slow-upgrade'] = await create({ ...base, allowEIO3: true }, 1000);
  }
  console.log(JSON.stringify({ version, ports }));
})();
