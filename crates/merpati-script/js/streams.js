// Web Streams for the Merpati script sandbox.
//
// A compact implementation of the WHATWG Streams Standard (ReadableStream
// with default and BYOB readers, WritableStream, TransformStream, the
// queuing strategies, TextEncoderStream and TextDecoderStream), following
// the reference algorithms. All asynchronous work is promise jobs, so it
// runs in the job's event loop and stops with its timeout or cancellation.
//
// Byte streams are simplified: enqueued chunks are copied rather than
// transferred, and `byobRequest` exists while a BYOB read (or an
// `autoAllocateChunkSize` pull) is waiting for data.
//
// `module.exports(env)` (env: TextEncoder, TextDecoder) returns the
// constructors.
module.exports = function createStreams(env) {
  'use strict';

  const isObject = (v) => (typeof v === 'object' && v !== null) || typeof v === 'function';
  const resolved = (v) => Promise.resolve(v);
  const rejected = (e) => Promise.reject(e);
  const noop = () => {};
  const queueMicrotask = (fn) => { Promise.resolve().then(fn); };
  const setHandled = (p) => { p.then(undefined, noop); };
  const SLOTS = Symbol('slots');

  function define(obj, props) {
    for (const k of Reflect.ownKeys(props)) {
      const d = Object.getOwnPropertyDescriptor(props, k);
      d.enumerable = typeof k === 'string';
      Object.defineProperty(obj, k, d);
    }
  }
  function tag(proto, name) {
    Object.defineProperty(proto, Symbol.toStringTag, { value: name, configurable: true });
  }
  function slots(obj, kind, what) {
    const s = isObject(obj) ? obj[SLOTS] : undefined;
    if (!s || s.kind !== kind) throw new TypeError((what || 'This method') + ' called on an object that is not a ' + kind);
    return s;
  }
  function setSlots(obj, s) {
    Object.defineProperty(obj, SLOTS, { value: s });
    s.api = obj;
    return s;
  }

  // A promise with its resolve/reject functions and state.
  function deferred() {
    const d = { state: 'pending' };
    d.promise = new Promise((resolve, reject) => {
      d.resolve = (v) => { if (d.state === 'pending') { d.state = 'fulfilled'; resolve(v); } };
      d.reject = (e) => { if (d.state === 'pending') { d.state = 'rejected'; reject(e); } };
    });
    return d;
  }
  function resolvedDeferred(v) { const d = deferred(); d.resolve(v); return d; }
  function rejectedDeferred(e) { const d = deferred(); d.reject(e); setHandled(d.promise); return d; }

  function uponPromise(p, onFulfilled, onRejected) {
    p.then(onFulfilled, onRejected).then(undefined, noop);
  }
  // Calls `method` on `target`, turning exceptions into a rejected promise.
  function promiseCall(method, target, args) {
    try {
      return resolved(method.apply(target, args));
    } catch (e) {
      return rejected(e);
    }
  }
  function getMethod(source, name, what) {
    const m = source == null ? undefined : source[name];
    if (m !== undefined && typeof m !== 'function') throw new TypeError(what + '.' + name + ' must be a function');
    return m;
  }
  function abortError(reason) {
    if (reason !== undefined) return reason;
    const e = new Error('This operation was aborted');
    e.name = 'AbortError';
    return e;
  }

  // ---------------------------------------------------------------------
  // Queue with sizes and strategies
  // ---------------------------------------------------------------------
  function resetQueue(c) {
    c.queue = [];
    c.queueTotalSize = 0;
  }
  function enqueueValueWithSize(c, value, size) {
    if (typeof size !== 'number' || Number.isNaN(size) || size < 0 || size === Infinity) {
      throw new RangeError('Size must be a finite, non-NaN, non-negative number.');
    }
    c.queue.push({ value, size });
    c.queueTotalSize += size;
  }
  function dequeueValue(c) {
    const pair = c.queue.shift();
    c.queueTotalSize -= pair.size;
    if (c.queueTotalSize < 0) c.queueTotalSize = 0;
    return pair.value;
  }
  function extractHighWaterMark(strategy, fallback) {
    const h = strategy == null ? undefined : strategy.highWaterMark;
    if (h === undefined) return fallback;
    const n = Number(h);
    if (Number.isNaN(n) || n < 0) throw new RangeError('Invalid highWaterMark');
    return n;
  }
  function extractSizeAlgorithm(strategy) {
    const size = strategy == null ? undefined : strategy.size;
    if (size === undefined) return () => 1;
    if (typeof size !== 'function') throw new TypeError('strategy.size must be a function');
    return (chunk) => size(chunk);
  }

  const countSize = function size() { return 1; };
  const byteLengthSize = function size(chunk) { return chunk.byteLength; };
  function strategyInit(init, name) {
    if (!isObject(init) || init.highWaterMark === undefined) {
      throw new TypeError("Failed to construct '" + name + "': required member highWaterMark is undefined.");
    }
    return Number(init.highWaterMark);
  }
  class CountQueuingStrategy {
    constructor(init) { setSlots(this, { kind: 'CountQueuingStrategy', hwm: strategyInit(init, 'CountQueuingStrategy') }); }
    get highWaterMark() { return slots(this, 'CountQueuingStrategy').hwm; }
    get size() { slots(this, 'CountQueuingStrategy'); return countSize; }
  }
  class ByteLengthQueuingStrategy {
    constructor(init) { setSlots(this, { kind: 'ByteLengthQueuingStrategy', hwm: strategyInit(init, 'ByteLengthQueuingStrategy') }); }
    get highWaterMark() { return slots(this, 'ByteLengthQueuingStrategy').hwm; }
    get size() { slots(this, 'ByteLengthQueuingStrategy'); return byteLengthSize; }
  }
  tag(CountQueuingStrategy.prototype, 'CountQueuingStrategy');
  tag(ByteLengthQueuingStrategy.prototype, 'ByteLengthQueuingStrategy');

  // A minimal AbortSignal for WritableStreamDefaultController.signal.
  class StreamAbortSignal {
    constructor() {
      this.aborted = false;
      this.reason = undefined;
      this.onabort = null;
      Object.defineProperty(this, '_listeners', { value: [] });
    }
    addEventListener(type, fn) { if (type === 'abort' && typeof fn === 'function') this._listeners.push(fn); }
    removeEventListener(type, fn) {
      const i = this._listeners.indexOf(fn);
      if (i !== -1) this._listeners.splice(i, 1);
    }
    throwIfAborted() { if (this.aborted) throw this.reason; }
    _abort(reason) {
      if (this.aborted) return;
      this.aborted = true;
      this.reason = abortError(reason);
      const event = { type: 'abort', target: this };
      if (typeof this.onabort === 'function') this.onabort(event);
      for (const fn of this._listeners.slice()) fn.call(this, event);
    }
  }
  tag(StreamAbortSignal.prototype, 'AbortSignal');

  // ---------------------------------------------------------------------
  // ReadableStream
  // ---------------------------------------------------------------------
  function initReadable(api) {
    return setSlots(api, { kind: 'ReadableStream', state: 'readable', reader: undefined, storedError: undefined, disturbed: false, controller: undefined });
  }
  const isLocked = (s) => s.reader !== undefined;

  // CreateReadableStream with algorithms (for tee, from, TransformStream).
  function createReadable(startAlgorithm, pullAlgorithm, cancelAlgorithm, hwm, sizeAlgorithm) {
    const api = Object.create(ReadableStream.prototype);
    const s = initReadable(api);
    const c = setSlots(Object.create(ReadableStreamDefaultController.prototype), { kind: 'ReadableStreamDefaultController' });
    setUpDefaultController(s, c, startAlgorithm, pullAlgorithm, cancelAlgorithm, hwm === undefined ? 1 : hwm, sizeAlgorithm || (() => 1));
    return s;
  }

  function rsClose(s) {
    s.state = 'closed';
    const r = s.reader;
    if (!r) return;
    r.closed.resolve(undefined);
    if (r.kind === 'ReadableStreamDefaultReader') {
      const requests = r.readRequests;
      r.readRequests = [];
      for (const q of requests) q.close();
    }
  }
  function rsError(s, e) {
    s.state = 'errored';
    s.storedError = e;
    const r = s.reader;
    if (!r) return;
    r.closed.reject(e);
    setHandled(r.closed.promise);
    if (r.kind === 'ReadableStreamDefaultReader') {
      const requests = r.readRequests;
      r.readRequests = [];
      for (const q of requests) q.error(e);
    } else {
      const requests = r.readIntoRequests;
      r.readIntoRequests = [];
      for (const q of requests) q.error(e);
    }
  }
  function rsCancel(s, reason) {
    s.disturbed = true;
    if (s.state === 'closed') return resolved(undefined);
    if (s.state === 'errored') return rejected(s.storedError);
    rsClose(s);
    const r = s.reader;
    if (r && r.kind === 'ReadableStreamBYOBReader') {
      const requests = r.readIntoRequests;
      r.readIntoRequests = [];
      for (const q of requests) q.close(undefined);
    }
    return s.controller.cancelSteps(reason).then(() => undefined);
  }
  const numReadRequests = (s) => (s.reader && s.reader.kind === 'ReadableStreamDefaultReader' ? s.reader.readRequests.length : 0);
  const numReadIntoRequests = (s) => (s.reader && s.reader.kind === 'ReadableStreamBYOBReader' ? s.reader.readIntoRequests.length : 0);
  function fulfillReadRequest(s, chunk, done) {
    const q = s.reader.readRequests.shift();
    if (done) q.close();
    else q.chunk(chunk);
  }

  // Default controller.
  function setUpDefaultController(s, c, startAlgorithm, pullAlgorithm, cancelAlgorithm, hwm, sizeAlgorithm) {
    c.stream = s;
    resetQueue(c);
    c.started = c.closeRequested = c.pullAgain = c.pulling = false;
    c.strategySizeAlgorithm = sizeAlgorithm;
    c.strategyHWM = hwm;
    c.pullAlgorithm = pullAlgorithm;
    c.cancelAlgorithm = cancelAlgorithm;
    c.cancelSteps = (reason) => {
      resetQueue(c);
      const result = c.cancelAlgorithm(reason);
      clearAlgorithms(c);
      return result;
    };
    c.pullSteps = (readRequest) => {
      if (c.queue.length > 0) {
        const chunk = dequeueValue(c);
        if (c.closeRequested && c.queue.length === 0) {
          clearAlgorithms(c);
          rsClose(s);
        } else callPullIfNeeded(c);
        readRequest.chunk(chunk);
      } else {
        s.reader.readRequests.push(readRequest);
        callPullIfNeeded(c);
      }
    };
    c.releaseSteps = noop;
    s.controller = c;
    const startResult = startAlgorithm();
    uponPromise(resolved(startResult), () => {
      c.started = true;
      callPullIfNeeded(c);
    }, (e) => controllerError(c, e));
  }
  function clearAlgorithms(c) {
    c.pullAlgorithm = () => resolved();
    c.cancelAlgorithm = () => resolved();
    c.strategySizeAlgorithm = () => 1;
  }
  const canCloseOrEnqueue = (c) => !c.closeRequested && c.stream.state === 'readable';
  function desiredSize(c) {
    const state = c.stream.state;
    if (state === 'errored') return null;
    if (state === 'closed') return 0;
    return c.strategyHWM - c.queueTotalSize;
  }
  function shouldCallPull(c) {
    const s = c.stream;
    if (!canCloseOrEnqueue(c) || !c.started) return false;
    if (isLocked(s) && (numReadRequests(s) > 0 || numReadIntoRequests(s) > 0)) return true;
    return desiredSize(c) > 0;
  }
  function callPullIfNeeded(c) {
    if (!shouldCallPull(c)) return;
    if (c.pulling) {
      c.pullAgain = true;
      return;
    }
    c.pulling = true;
    uponPromise(c.pullAlgorithm(), () => {
      c.pulling = false;
      if (c.pullAgain) {
        c.pullAgain = false;
        callPullIfNeeded(c);
      }
    }, (e) => controllerError(c, e));
  }
  function controllerClose(c) {
    if (!canCloseOrEnqueue(c)) return;
    c.closeRequested = true;
    if (c.queue.length === 0) {
      clearAlgorithms(c);
      rsClose(c.stream);
    }
  }
  function controllerEnqueue(c, chunk) {
    if (!canCloseOrEnqueue(c)) return;
    const s = c.stream;
    if (c.bytes) {
      byteEnqueue(c, chunk);
      return;
    }
    if (isLocked(s) && numReadRequests(s) > 0) fulfillReadRequest(s, chunk, false);
    else {
      let size;
      try {
        size = c.strategySizeAlgorithm(chunk);
        enqueueValueWithSize(c, chunk, size);
      } catch (e) {
        controllerError(c, e);
        throw e;
      }
    }
    callPullIfNeeded(c);
  }
  function controllerError(c, e) {
    const s = c.stream;
    if (s.state !== 'readable') return;
    resetQueue(c);
    if (c.bytes) clearPendingBYOB(c);
    clearAlgorithms(c);
    rsError(s, e);
  }
  const hasBackpressure = (c) => !shouldCallPull(c);

  function setUpDefaultControllerFromSource(s, source, hwm, sizeAlgorithm) {
    const c = setSlots(Object.create(ReadableStreamDefaultController.prototype), { kind: 'ReadableStreamDefaultController' });
    const start = getMethod(source, 'start', 'underlyingSource');
    const pull = getMethod(source, 'pull', 'underlyingSource');
    const cancel = getMethod(source, 'cancel', 'underlyingSource');
    setUpDefaultController(s, c,
      start ? () => start.call(source, c.api) : () => undefined,
      pull ? () => promiseCall(pull, source, [c.api]) : () => resolved(),
      cancel ? (reason) => promiseCall(cancel, source, [reason]) : () => resolved(),
      hwm, sizeAlgorithm);
  }

  class ReadableStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get desiredSize() { return desiredSize(slots(this, 'ReadableStreamDefaultController')); }
    close() {
      const c = slots(this, 'ReadableStreamDefaultController');
      if (!canCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits close');
      controllerClose(c);
    }
    enqueue(chunk) {
      const c = slots(this, 'ReadableStreamDefaultController');
      if (!canCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits enqueue');
      controllerEnqueue(c, chunk);
    }
    error(e) { controllerError(slots(this, 'ReadableStreamDefaultController'), e); }
  }
  tag(ReadableStreamDefaultController.prototype, 'ReadableStreamDefaultController');

  // Byte controller (simplified: chunks are copied, see the header).
  function toView(chunk, what) {
    if (!ArrayBuffer.isView(chunk)) throw new TypeError(what + ': chunk must be an ArrayBuffer view');
    if (chunk.byteLength === 0) throw new TypeError(what + ': chunk must have non-zero byteLength');
    return new Uint8Array(chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength));
  }
  function clearPendingBYOB(c) {
    c.byobRequest = null;
    c.pendingAuto = null;
  }
  // Copies queued bytes into a pending BYOB read; returns true when done.
  function fillReadInto(c, req) {
    while (c.queue.length && req.filled < req.view.byteLength) {
      const head = c.queue[0].value;
      const n = Math.min(head.byteLength, req.view.byteLength - req.filled);
      new Uint8Array(req.view.buffer, req.view.byteOffset + req.filled, n).set(head.subarray(0, n));
      req.filled += n;
      if (n === head.byteLength) dequeueValue(c);
      else {
        c.queue[0].value = head.subarray(n);
        c.queue[0].size -= n;
        c.queueTotalSize -= n;
      }
    }
    return req.filled >= req.min;
  }
  function viewOf(req, filled) {
    const Ctor = req.view.constructor;
    const elem = Ctor.BYTES_PER_ELEMENT || 1;
    return new Ctor(req.view.buffer, req.view.byteOffset, Math.floor(filled / elem));
  }
  function processReadIntoRequests(c) {
    const s = c.stream;
    const r = s.reader;
    while (r && r.kind === 'ReadableStreamBYOBReader' && r.readIntoRequests.length) {
      const req = r.readIntoRequests[0];
      if (!fillReadInto(c, req)) break;
      r.readIntoRequests.shift();
      req.chunk(viewOf(req, req.filled));
    }
    if (c.closeRequested && c.queue.length === 0) {
      clearAlgorithms(c);
      rsClose(s);
    }
  }
  function byteEnqueue(c, chunk) {
    const s = c.stream;
    const bytes = toView(chunk, 'ReadableByteStreamController.enqueue');
    c.byobRequest = null;
    if (isLocked(s) && numReadRequests(s) > 0) fulfillReadRequest(s, bytes, false);
    else {
      enqueueValueWithSize(c, bytes, bytes.byteLength);
      processReadIntoRequests(c);
    }
    callPullIfNeeded(c);
  }
  function makeBYOBRequest(c, view, respond) {
    const req = setSlots(Object.create(ReadableStreamBYOBRequest.prototype), { kind: 'ReadableStreamBYOBRequest', view, respond });
    return req.api;
  }
  function setUpByteController(s, source, hwm) {
    const c = setSlots(Object.create(ReadableByteStreamController.prototype), { kind: 'ReadableByteStreamController' });
    c.bytes = true;
    c.byobRequest = null;
    const autoAllocate = source && source.autoAllocateChunkSize !== undefined ? Number(source.autoAllocateChunkSize) : undefined;
    if (autoAllocate !== undefined && (!Number.isInteger(autoAllocate) || autoAllocate <= 0)) {
      throw new TypeError('autoAllocateChunkSize must be a positive integer');
    }
    const start = getMethod(source, 'start', 'underlyingSource');
    const pull = getMethod(source, 'pull', 'underlyingSource');
    const cancel = getMethod(source, 'cancel', 'underlyingSource');
    const pullWithRequest = () => {
      const r = s.reader;
      if (!c.byobRequest && c.queue.length === 0) {
        if (r && r.kind === 'ReadableStreamBYOBReader' && r.readIntoRequests.length) {
          const req = r.readIntoRequests[0];
          const view = new Uint8Array(req.view.buffer, req.view.byteOffset + req.filled, req.view.byteLength - req.filled);
          c.byobRequest = makeBYOBRequest(c, view, (n) => {
            c.byobRequest = null;
            req.filled += n;
            if (req.filled >= req.min || c.closeRequested) {
              r.readIntoRequests.shift();
              req.chunk(viewOf(req, req.filled));
            }
          });
        } else if (autoAllocate !== undefined && numReadRequests(s) > 0) {
          const view = new Uint8Array(autoAllocate);
          c.byobRequest = makeBYOBRequest(c, view, (n) => {
            c.byobRequest = null;
            if (n > 0) byteEnqueue(c, view.subarray(0, n));
          });
        }
      }
      return pull ? promiseCall(pull, source, [c.api]) : resolved();
    };
    setUpDefaultController(s, c,
      start ? () => start.call(source, c.api) : () => undefined,
      pullWithRequest,
      cancel ? (reason) => promiseCall(cancel, source, [reason]) : () => resolved(),
      hwm, (chunk) => chunk.byteLength);
    c.bytes = true;
    const defaultPull = c.pullSteps;
    c.pullSteps = (readRequest) => defaultPull(readRequest);
    c.readIntoSteps = (req) => {
      if (s.state === 'closed') {
        req.close(viewOf(req, 0));
        return;
      }
      s.reader.readIntoRequests.push(req);
      processReadIntoRequests(c);
      callPullIfNeeded(c);
    };
  }

  class ReadableByteStreamController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get byobRequest() { return slots(this, 'ReadableByteStreamController').byobRequest; }
    get desiredSize() { return desiredSize(slots(this, 'ReadableByteStreamController')); }
    close() {
      const c = slots(this, 'ReadableByteStreamController');
      if (!canCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits close');
      c.closeRequested = true;
      const r = c.stream.reader;
      if (r && r.kind === 'ReadableStreamBYOBReader') {
        // Pending reads complete with what they have.
        for (const req of r.readIntoRequests.splice(0)) req.close(viewOf(req, req.filled));
      }
      if (c.queue.length === 0) {
        clearAlgorithms(c);
        rsClose(c.stream);
      }
    }
    enqueue(chunk) {
      const c = slots(this, 'ReadableByteStreamController');
      if (!canCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits enqueue');
      byteEnqueue(c, chunk);
    }
    error(e) { controllerError(slots(this, 'ReadableByteStreamController'), e); }
  }
  tag(ReadableByteStreamController.prototype, 'ReadableByteStreamController');

  class ReadableStreamBYOBRequest {
    constructor() { throw new TypeError('Illegal constructor'); }
    get view() { return slots(this, 'ReadableStreamBYOBRequest').view; }
    respond(bytesWritten) {
      const r = slots(this, 'ReadableStreamBYOBRequest');
      const n = Number(bytesWritten);
      if (!Number.isInteger(n) || n < 0 || n > r.view.byteLength) throw new RangeError('bytesWritten out of range');
      if (r.done) throw new TypeError('This BYOB request has already been responded to');
      r.done = true;
      r.respond(n);
    }
    respondWithNewView(view) {
      const r = slots(this, 'ReadableStreamBYOBRequest');
      if (!ArrayBuffer.isView(view)) throw new TypeError('view must be an ArrayBuffer view');
      if (view.byteLength > r.view.byteLength) throw new RangeError('view is too large');
      new Uint8Array(r.view.buffer, r.view.byteOffset, view.byteLength).set(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
      this.respond(view.byteLength);
    }
  }
  tag(ReadableStreamBYOBRequest.prototype, 'ReadableStreamBYOBRequest');

  // Readers.
  function readerInit(r, s) {
    r.stream = s;
    s.reader = r;
    if (s.state === 'readable') r.closed = deferred();
    else if (s.state === 'closed') r.closed = resolvedDeferred(undefined);
    else r.closed = rejectedDeferred(s.storedError);
  }
  function readerRelease(r) {
    const s = r.stream;
    const err = new TypeError('Reader was released and can no longer be used to monitor the stream\'s closedness');
    if (s.state === 'readable') r.closed.reject(err);
    else r.closed = rejectedDeferred(err);
    setHandled(r.closed.promise);
    s.controller.releaseSteps();
    s.reader = undefined;
    r.stream = undefined;
    const released = new TypeError('Reader was released');
    if (r.kind === 'ReadableStreamDefaultReader') {
      for (const q of r.readRequests.splice(0)) q.error(released);
    } else {
      for (const q of r.readIntoRequests.splice(0)) q.error(released);
    }
  }
  function readerRead(r, readRequest) {
    const s = r.stream;
    s.disturbed = true;
    if (s.state === 'closed') readRequest.close();
    else if (s.state === 'errored') readRequest.error(s.storedError);
    else s.controller.pullSteps(readRequest);
  }
  function acquireDefaultReader(s) {
    const api = Object.create(ReadableStreamDefaultReader.prototype);
    const r = setSlots(api, { kind: 'ReadableStreamDefaultReader', readRequests: [] });
    if (isLocked(s)) throw new TypeError('ReadableStreamDefaultReader constructor can only accept readable streams that are not yet locked to a reader');
    readerInit(r, s);
    return r;
  }
  function readPromise(r) {
    const d = deferred();
    readerRead(r, {
      chunk: (value) => d.resolve({ value, done: false }),
      close: () => d.resolve({ value: undefined, done: true }),
      error: (e) => d.reject(e),
    });
    return d.promise;
  }

  class ReadableStreamDefaultReader {
    constructor(stream) {
      const s = slots(stream, 'ReadableStream', 'ReadableStreamDefaultReader');
      if (isLocked(s)) throw new TypeError('ReadableStreamDefaultReader constructor can only accept readable streams that are not yet locked to a reader');
      readerInit(setSlots(this, { kind: 'ReadableStreamDefaultReader', readRequests: [] }), s);
    }
    get closed() {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamDefaultReader') return rejected(new TypeError('Illegal invocation'));
      return r.closed.promise;
    }
    read() {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamDefaultReader') return rejected(new TypeError('Illegal invocation'));
      if (!r.stream) return rejected(new TypeError('Cannot read from a released reader'));
      return readPromise(r);
    }
    releaseLock() {
      const r = slots(this, 'ReadableStreamDefaultReader');
      if (r.stream) readerRelease(r);
    }
    cancel(reason) {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamDefaultReader') return rejected(new TypeError('Illegal invocation'));
      if (!r.stream) return rejected(new TypeError('Cannot cancel a released reader'));
      return rsCancel(r.stream, reason);
    }
  }
  tag(ReadableStreamDefaultReader.prototype, 'ReadableStreamDefaultReader');

  class ReadableStreamBYOBReader {
    constructor(stream) {
      const s = slots(stream, 'ReadableStream', 'ReadableStreamBYOBReader');
      if (isLocked(s)) throw new TypeError('ReadableStreamBYOBReader constructor can only accept readable streams that are not yet locked to a reader');
      if (!s.controller.bytes) throw new TypeError('Cannot construct a ReadableStreamBYOBReader for a stream not constructed with a byte source');
      readerInit(setSlots(this, { kind: 'ReadableStreamBYOBReader', readIntoRequests: [] }), s);
    }
    get closed() {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamBYOBReader') return rejected(new TypeError('Illegal invocation'));
      return r.closed.promise;
    }
    read(view, options) {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamBYOBReader') return rejected(new TypeError('Illegal invocation'));
      if (!ArrayBuffer.isView(view)) return rejected(new TypeError('view must be an ArrayBuffer view'));
      if (view.byteLength === 0) return rejected(new TypeError('view must have non-zero byteLength'));
      if (!r.stream) return rejected(new TypeError('Cannot read from a released reader'));
      const elem = view.constructor.BYTES_PER_ELEMENT || 1;
      const minElems = options && options.min !== undefined ? Number(options.min) : 1;
      if (!Number.isInteger(minElems) || minElems <= 0 || minElems * elem > view.byteLength) {
        return rejected(new RangeError('options.min is out of range'));
      }
      const s = r.stream;
      s.disturbed = true;
      if (s.state === 'errored') return rejected(s.storedError);
      const d = deferred();
      const req = {
        view, filled: 0, min: minElems * elem,
        chunk: (v) => d.resolve({ value: v, done: false }),
        close: (v) => d.resolve({ value: v, done: true }),
        error: (e) => d.reject(e),
      };
      s.controller.readIntoSteps(req);
      return d.promise;
    }
    releaseLock() {
      const r = slots(this, 'ReadableStreamBYOBReader');
      if (r.stream) readerRelease(r);
    }
    cancel(reason) {
      const r = this && this[SLOTS];
      if (!r || r.kind !== 'ReadableStreamBYOBReader') return rejected(new TypeError('Illegal invocation'));
      if (!r.stream) return rejected(new TypeError('Cannot cancel a released reader'));
      return rsCancel(r.stream, reason);
    }
  }
  tag(ReadableStreamBYOBReader.prototype, 'ReadableStreamBYOBReader');

  // Async iteration.
  const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype);
  const ReadableStreamAsyncIteratorPrototype = Object.create(AsyncIteratorPrototype);
  function createAsyncIterator(s, preventCancel) {
    const reader = acquireDefaultReader(s);
    let ongoing;
    let finished = false;
    const nextSteps = () => {
      if (finished) return resolved({ value: undefined, done: true });
      const d = deferred();
      readerRead(reader, {
        chunk: (value) => queueMicrotask(() => d.resolve({ value, done: false })),
        close: () => {
          finished = true;
          readerRelease(reader);
          d.resolve({ value: undefined, done: true });
        },
        error: (e) => {
          finished = true;
          readerRelease(reader);
          d.reject(e);
        },
      });
      return d.promise;
    };
    const returnSteps = (value) => {
      if (finished) return resolved({ value, done: true });
      finished = true;
      if (!reader.stream) return resolved({ value, done: true });
      if (!preventCancel) {
        const result = rsCancel(reader.stream, value);
        readerRelease(reader);
        return result.then(() => ({ value, done: true }));
      }
      readerRelease(reader);
      return resolved({ value, done: true });
    };
    const it = Object.create(ReadableStreamAsyncIteratorPrototype);
    setSlots(it, {
      kind: 'ReadableStreamAsyncIterator',
      next() {
        ongoing = ongoing ? ongoing.then(nextSteps, nextSteps) : nextSteps();
        return ongoing;
      },
      return(value) {
        const steps = () => returnSteps(value);
        ongoing = ongoing ? ongoing.then(steps, steps) : steps();
        return ongoing;
      },
    });
    return it;
  }
  define(ReadableStreamAsyncIteratorPrototype, {
    next() {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'ReadableStreamAsyncIterator') return rejected(new TypeError('Illegal invocation'));
      return s.next();
    },
    return(value) {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'ReadableStreamAsyncIterator') return rejected(new TypeError('Illegal invocation'));
      return s.return(value);
    },
  });
  tag(ReadableStreamAsyncIteratorPrototype, 'ReadableStream AsyncIterator');

  function asyncIteratorOf(obj) {
    if (obj == null) throw new TypeError('ReadableStream.from: argument is not iterable');
    const asyncMethod = obj[Symbol.asyncIterator];
    if (asyncMethod != null) {
      const iterator = asyncMethod.call(obj);
      if (!isObject(iterator)) throw new TypeError('The iterator method must return an object');
      return { iterator, next: iterator.next };
    }
    const syncMethod = obj[Symbol.iterator];
    if (typeof syncMethod !== 'function') throw new TypeError('ReadableStream.from: argument is not iterable');
    const sync = syncMethod.call(obj);
    if (!isObject(sync)) throw new TypeError('The iterator method must return an object');
    // CreateAsyncFromSyncIterator.
    const iterator = {
      next() {
        const r = sync.next();
        return resolved(r.value).then((value) => ({ value, done: !!r.done }));
      },
      return(value) {
        const m = sync.return;
        if (m == null) return resolved({ value, done: true });
        const r = m.call(sync, value);
        return resolved(r.value).then((v) => ({ value: v, done: true }));
      },
    };
    return { iterator, next: iterator.next };
  }

  function readableFrom(asyncIterable) {
    const record = asyncIteratorOf(asyncIterable);
    let s;
    const pull = () => {
      let result;
      try {
        result = record.next.call(record.iterator);
      } catch (e) {
        return rejected(e);
      }
      return resolved(result).then((r) => {
        if (!isObject(r)) throw new TypeError('The promise returned by the iterator.next() method must fulfill with an object');
        if (r.done) controllerClose(s.controller);
        else controllerEnqueue(s.controller, r.value);
      });
    };
    const cancel = (reason) => {
      let ret;
      try {
        ret = record.iterator.return;
      } catch (e) {
        return rejected(e);
      }
      if (ret == null) return resolved();
      return promiseCall(ret, record.iterator, [reason]).then((r) => {
        if (!isObject(r)) throw new TypeError('The promise returned by the iterator.return() method must fulfill with an object');
      });
    };
    s = createReadable(() => undefined, pull, cancel, 0);
    return s.api;
  }

  function readableTee(s) {
    const reader = acquireDefaultReader(s);
    let reading = false;
    let readAgain = false;
    let canceled1 = false;
    let canceled2 = false;
    let reason1;
    let reason2;
    let branch1;
    let branch2;
    const cancelPromise = deferred();
    const clone = s.controller.bytes ? (chunk) => chunk.slice() : (chunk) => chunk;
    const pullAlgorithm = () => {
      if (reading) {
        readAgain = true;
        return resolved();
      }
      reading = true;
      readerRead(reader, {
        chunk: (chunk) => {
          queueMicrotask(() => {
            readAgain = false;
            if (!canceled1) controllerEnqueue(branch1.controller, chunk);
            if (!canceled2) controllerEnqueue(branch2.controller, clone(chunk));
            reading = false;
            if (readAgain) pullAlgorithm();
          });
        },
        close: () => {
          reading = false;
          if (!canceled1) controllerClose(branch1.controller);
          if (!canceled2) controllerClose(branch2.controller);
          if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
        },
        error: () => { reading = false; },
      });
      return resolved();
    };
    const cancel1 = (reason) => {
      canceled1 = true;
      reason1 = reason;
      if (canceled2) cancelPromise.resolve(rsCancel(s, [reason1, reason2]));
      return cancelPromise.promise;
    };
    const cancel2 = (reason) => {
      canceled2 = true;
      reason2 = reason;
      if (canceled1) cancelPromise.resolve(rsCancel(s, [reason1, reason2]));
      return cancelPromise.promise;
    };
    branch1 = createReadable(() => undefined, pullAlgorithm, cancel1);
    branch2 = createReadable(() => undefined, pullAlgorithm, cancel2);
    uponPromise(reader.closed.promise, noop, (r) => {
      controllerError(branch1.controller, r);
      controllerError(branch2.controller, r);
      if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
    });
    return [branch1.api, branch2.api];
  }

  function readablePipeTo(source, dest, preventClose, preventAbort, preventCancel, signal) {
    const reader = acquireDefaultReader(source);
    const writer = acquireWriter(dest);
    source.disturbed = true;
    let shuttingDown = false;
    let currentWrite = resolved();
    const done = deferred();
    let abortAlgorithm;

    function waitForWritesToFinish() {
      const old = currentWrite;
      return currentWrite.then(() => (old !== currentWrite ? waitForWritesToFinish() : undefined));
    }
    function finalize(isError, error) {
      writerRelease(writer);
      readerRelease(reader);
      if (signal !== undefined && typeof signal.removeEventListener === 'function') signal.removeEventListener('abort', abortAlgorithm);
      if (isError) done.reject(error);
      else done.resolve(undefined);
    }
    function shutdownWithAction(action, originalIsError, originalError) {
      if (shuttingDown) return;
      shuttingDown = true;
      const doTheRest = () => uponPromise(action(), () => finalize(originalIsError, originalError), (e) => finalize(true, e));
      if (dest.state === 'writable' && !closeQueuedOrInFlight(dest)) uponPromise(waitForWritesToFinish(), doTheRest, noop);
      else doTheRest();
    }
    function shutdown(isError, error) {
      if (shuttingDown) return;
      shuttingDown = true;
      if (dest.state === 'writable' && !closeQueuedOrInFlight(dest)) {
        uponPromise(waitForWritesToFinish(), () => finalize(isError, error), noop);
      } else finalize(isError, error);
    }
    function pipeStep() {
      if (shuttingDown) return resolved(true);
      return writer.ready.promise.then(() => new Promise((resolveRead, rejectRead) => {
        readerRead(reader, {
          chunk: (chunk) => {
            currentWrite = writerWrite(writer, chunk).then(undefined, noop);
            resolveRead(false);
          },
          close: () => resolveRead(true),
          error: rejectRead,
        });
      }));
    }
    function pipeLoop() {
      return new Promise((resolveLoop, rejectLoop) => {
        const next = (stop) => {
          if (stop) resolveLoop();
          else pipeStep().then(next, rejectLoop);
        };
        next(false);
      });
    }

    if (signal !== undefined) {
      abortAlgorithm = () => {
        const error = abortError(signal.reason);
        const actions = [];
        if (!preventAbort) actions.push(() => (dest.state === 'writable' ? writableAbort(dest, error) : resolved()));
        if (!preventCancel) actions.push(() => (source.state === 'readable' ? rsCancel(source, error) : resolved()));
        shutdownWithAction(() => Promise.all(actions.map((a) => a())), true, error);
      };
      if (signal.aborted) {
        abortAlgorithm();
        return done.promise;
      }
      if (typeof signal.addEventListener === 'function') signal.addEventListener('abort', abortAlgorithm);
    }

    // Errors and closing propagate forward and backward.
    const sourceClosed = reader.closed.promise;
    const destClosed = writer.closed.promise;
    if (source.state === 'errored') {
      if (!preventAbort) shutdownWithAction(() => writableAbort(dest, source.storedError), true, source.storedError);
      else shutdown(true, source.storedError);
    } else {
      uponPromise(sourceClosed, noop, (e) => {
        if (!preventAbort) shutdownWithAction(() => writableAbort(dest, e), true, e);
        else shutdown(true, e);
      });
    }
    if (dest.state === 'errored') {
      if (!preventCancel) shutdownWithAction(() => rsCancel(source, dest.storedError), true, dest.storedError);
      else shutdown(true, dest.storedError);
    } else {
      uponPromise(destClosed, noop, (e) => {
        if (!preventCancel) shutdownWithAction(() => rsCancel(source, e), true, e);
        else shutdown(true, e);
      });
    }
    const closeForward = () => {
      if (!preventClose) shutdownWithAction(() => writerCloseWithErrorPropagation(writer));
      else shutdown();
    };
    if (source.state === 'closed') closeForward();
    else uponPromise(sourceClosed, closeForward, noop);
    if (closeQueuedOrInFlight(dest) || dest.state === 'closed') {
      const e = new TypeError('the destination writable stream closed before all data could be piped to it');
      if (!preventCancel) shutdownWithAction(() => rsCancel(source, e), true, e);
      else shutdown(true, e);
    }
    setHandled(pipeLoop());
    return done.promise;
  }

  function pipeOptions(options) {
    const o = options === undefined ? {} : options;
    if (!isObject(o)) throw new TypeError('options must be an object');
    const signal = o.signal;
    if (signal !== undefined && (!isObject(signal) || !('aborted' in signal))) throw new TypeError('options.signal must be an AbortSignal');
    return { preventAbort: !!o.preventAbort, preventCancel: !!o.preventCancel, preventClose: !!o.preventClose, signal };
  }

  class ReadableStream {
    constructor(underlyingSource, strategy) {
      const source = underlyingSource === undefined ? null : underlyingSource;
      if (source !== null && !isObject(source)) throw new TypeError('underlyingSource must be an object');
      const strat = strategy === undefined ? {} : strategy;
      const s = initReadable(this);
      const type = source ? source.type : undefined;
      if (type !== undefined && String(type) === 'bytes') {
        if (strat.size !== undefined) throw new RangeError('The strategy for a byte stream cannot have a size function');
        setUpByteController(s, source, extractHighWaterMark(strat, 0));
      } else if (type !== undefined) {
        throw new TypeError("Failed to construct 'ReadableStream': The provided value '" + String(type) + "' is not a valid enum value of type ReadableStreamType.");
      } else {
        setUpDefaultControllerFromSource(s, source, extractHighWaterMark(strat, 1), extractSizeAlgorithm(strat));
      }
    }
    get locked() { return isLocked(slots(this, 'ReadableStream')); }
    cancel(reason) {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'ReadableStream') return rejected(new TypeError('Illegal invocation'));
      if (isLocked(s)) return rejected(new TypeError('Cannot cancel a stream that already has a reader'));
      return rsCancel(s, reason);
    }
    getReader(options) {
      const s = slots(this, 'ReadableStream');
      const mode = options == null ? undefined : options.mode;
      if (mode === undefined) return acquireDefaultReader(s).api;
      if (String(mode) !== 'byob') throw new TypeError("Failed to execute 'getReader' on 'ReadableStream': The provided value '" + String(mode) + "' is not a valid enum value of type ReadableStreamReaderMode.");
      return new ReadableStreamBYOBReader(this);
    }
    pipeThrough(transform, options) {
      const s = slots(this, 'ReadableStream');
      if (!isObject(transform)) throw new TypeError('pipeThrough: transform must be an object');
      const readable = transform.readable;
      const writable = transform.writable;
      const rs = readable && readable[SLOTS];
      const ws = writable && writable[SLOTS];
      if (!rs || rs.kind !== 'ReadableStream') throw new TypeError('pipeThrough: transform.readable must be a ReadableStream');
      if (!ws || ws.kind !== 'WritableStream') throw new TypeError('pipeThrough: transform.writable must be a WritableStream');
      const o = pipeOptions(options);
      if (isLocked(s)) throw new TypeError('ReadableStream.prototype.pipeThrough cannot be used on a locked ReadableStream');
      if (ws.writer !== undefined) throw new TypeError('ReadableStream.prototype.pipeThrough cannot be used on a locked WritableStream');
      setHandled(readablePipeTo(s, ws, o.preventClose, o.preventAbort, o.preventCancel, o.signal));
      return readable;
    }
    pipeTo(destination, options) {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'ReadableStream') return rejected(new TypeError('Illegal invocation'));
      const ws = destination && destination[SLOTS];
      if (!ws || ws.kind !== 'WritableStream') return rejected(new TypeError('ReadableStream.prototype.pipeTo\'s first argument must be a WritableStream'));
      let o;
      try {
        o = pipeOptions(options);
      } catch (e) {
        return rejected(e);
      }
      if (isLocked(s)) return rejected(new TypeError('ReadableStream.prototype.pipeTo cannot be used on a locked ReadableStream'));
      if (ws.writer !== undefined) return rejected(new TypeError('ReadableStream.prototype.pipeTo cannot be used on a locked WritableStream'));
      return readablePipeTo(s, ws, o.preventClose, o.preventAbort, o.preventCancel, o.signal);
    }
    tee() { return readableTee(slots(this, 'ReadableStream')); }
    values(options) {
      const s = slots(this, 'ReadableStream');
      return createAsyncIterator(s, !!(options && options.preventCancel));
    }
    static from(asyncIterable) { return readableFrom(asyncIterable); }
  }
  Object.defineProperty(ReadableStream.prototype, Symbol.asyncIterator, {
    value: ReadableStream.prototype.values, writable: true, configurable: true,
  });
  tag(ReadableStream.prototype, 'ReadableStream');

  // ---------------------------------------------------------------------
  // WritableStream
  // ---------------------------------------------------------------------
  const CLOSE_SENTINEL = {};
  function initWritable(api) {
    return setSlots(api, {
      kind: 'WritableStream', state: 'writable', storedError: undefined, writer: undefined, controller: undefined,
      inFlightWriteRequest: undefined, closeRequest: undefined, inFlightCloseRequest: undefined,
      pendingAbortRequest: undefined, writeRequests: [], backpressure: false,
    });
  }
  function createWritable(startAlgorithm, writeAlgorithm, closeAlgorithm, abortAlgorithm, hwm, sizeAlgorithm) {
    const s = initWritable(Object.create(WritableStream.prototype));
    const c = setSlots(Object.create(WritableStreamDefaultController.prototype), { kind: 'WritableStreamDefaultController' });
    setUpWritableController(s, c, startAlgorithm, writeAlgorithm, closeAlgorithm, abortAlgorithm, hwm, sizeAlgorithm);
    return s;
  }
  const closeQueuedOrInFlight = (s) => s.closeRequest !== undefined || s.inFlightCloseRequest !== undefined;
  const hasOperationMarkedInFlight = (s) => s.inFlightWriteRequest !== undefined || s.inFlightCloseRequest !== undefined;

  function writableAbort(s, reason) {
    if (s.state === 'closed' || s.state === 'errored') return resolved(undefined);
    s.controller.signal._abort(reason);
    const state = s.state;
    if (state === 'closed' || state === 'errored') return resolved(undefined);
    if (s.pendingAbortRequest) return s.pendingAbortRequest.promise.promise;
    let wasAlreadyErroring = false;
    if (state === 'erroring') {
      wasAlreadyErroring = true;
      reason = undefined;
    }
    const d = deferred();
    s.pendingAbortRequest = { promise: d, reason, wasAlreadyErroring };
    if (!wasAlreadyErroring) startErroring(s, reason);
    return d.promise;
  }
  function writableClose(s) {
    const state = s.state;
    if (state === 'closed' || state === 'errored') {
      return rejected(new TypeError('The stream (in ' + state + ' state) is not in the writable state and cannot be closed'));
    }
    const d = deferred();
    s.closeRequest = d;
    const writer = s.writer;
    if (writer && s.backpressure && state === 'writable') writer.ready.resolve(undefined);
    enqueueValueWithSize(s.controller, CLOSE_SENTINEL, 0);
    advanceQueueIfNeeded(s.controller);
    return d.promise;
  }
  function dealWithRejection(s, error) {
    if (s.state === 'writable') {
      startErroring(s, error);
      return;
    }
    finishErroring(s);
  }
  function startErroring(s, reason) {
    const c = s.controller;
    s.state = 'erroring';
    s.storedError = reason;
    const writer = s.writer;
    if (writer) writerEnsureReadyRejected(writer, reason);
    if (!hasOperationMarkedInFlight(s) && c.started) finishErroring(s);
  }
  function finishErroring(s) {
    s.state = 'errored';
    resetQueue(s.controller);
    const storedError = s.storedError;
    for (const req of s.writeRequests) req.reject(storedError);
    s.writeRequests = [];
    if (!s.pendingAbortRequest) {
      rejectCloseAndClosedIfNeeded(s);
      return;
    }
    const abortRequest = s.pendingAbortRequest;
    s.pendingAbortRequest = undefined;
    if (abortRequest.wasAlreadyErroring) {
      abortRequest.promise.reject(storedError);
      rejectCloseAndClosedIfNeeded(s);
      return;
    }
    const c = s.controller;
    const result = c.abortAlgorithm(abortRequest.reason);
    clearWritableAlgorithms(c);
    uponPromise(result, () => {
      abortRequest.promise.resolve(undefined);
      rejectCloseAndClosedIfNeeded(s);
    }, (reason) => {
      abortRequest.promise.reject(reason);
      rejectCloseAndClosedIfNeeded(s);
    });
  }
  function finishInFlightWrite(s) {
    s.inFlightWriteRequest.resolve(undefined);
    s.inFlightWriteRequest = undefined;
  }
  function finishInFlightWriteWithError(s, error) {
    s.inFlightWriteRequest.reject(error);
    s.inFlightWriteRequest = undefined;
    dealWithRejection(s, error);
  }
  function finishInFlightClose(s) {
    s.inFlightCloseRequest.resolve(undefined);
    s.inFlightCloseRequest = undefined;
    if (s.state === 'erroring') {
      s.storedError = undefined;
      if (s.pendingAbortRequest) {
        s.pendingAbortRequest.promise.resolve(undefined);
        s.pendingAbortRequest = undefined;
      }
    }
    s.state = 'closed';
    if (s.writer) s.writer.closed.resolve(undefined);
  }
  function finishInFlightCloseWithError(s, error) {
    s.inFlightCloseRequest.reject(error);
    s.inFlightCloseRequest = undefined;
    if (s.pendingAbortRequest) {
      s.pendingAbortRequest.promise.reject(error);
      s.pendingAbortRequest = undefined;
    }
    dealWithRejection(s, error);
  }
  function rejectCloseAndClosedIfNeeded(s) {
    if (s.closeRequest) {
      s.closeRequest.reject(s.storedError);
      s.closeRequest = undefined;
    }
    const writer = s.writer;
    if (writer) {
      writer.closed.reject(s.storedError);
      setHandled(writer.closed.promise);
    }
  }
  function updateBackpressure(s, backpressure) {
    const writer = s.writer;
    if (writer && backpressure !== s.backpressure) {
      if (backpressure) writer.ready = deferred();
      else writer.ready.resolve(undefined);
    }
    s.backpressure = backpressure;
  }

  // Writer.
  function writerInit(w, s) {
    if (s.writer !== undefined) throw new TypeError('This stream has already been locked for exclusive writing by another writer');
    w.stream = s;
    s.writer = w;
    const state = s.state;
    if (state === 'writable') {
      w.ready = !closeQueuedOrInFlight(s) && s.backpressure ? deferred() : resolvedDeferred(undefined);
      w.closed = deferred();
    } else if (state === 'erroring') {
      w.ready = rejectedDeferred(s.storedError);
      w.closed = deferred();
    } else if (state === 'closed') {
      w.ready = resolvedDeferred(undefined);
      w.closed = resolvedDeferred(undefined);
    } else {
      w.ready = rejectedDeferred(s.storedError);
      w.closed = rejectedDeferred(s.storedError);
    }
  }
  function acquireWriter(s) {
    const w = setSlots(Object.create(WritableStreamDefaultWriter.prototype), { kind: 'WritableStreamDefaultWriter' });
    writerInit(w, s);
    return w;
  }
  function writerDesiredSize(w) {
    const s = w.stream;
    if (s.state === 'errored' || s.state === 'erroring') return null;
    if (s.state === 'closed') return 0;
    return s.controller.strategyHWM - s.controller.queueTotalSize;
  }
  function writerEnsureReadyRejected(w, error) {
    if (w.ready.state === 'pending') w.ready.reject(error);
    else w.ready = rejectedDeferred(error);
    setHandled(w.ready.promise);
  }
  function writerEnsureClosedRejected(w, error) {
    if (w.closed.state === 'pending') w.closed.reject(error);
    else w.closed = rejectedDeferred(error);
    setHandled(w.closed.promise);
  }
  function writerRelease(w) {
    const s = w.stream;
    const error = new TypeError('Writer was released and can no longer be used to monitor the stream\'s closedness');
    writerEnsureReadyRejected(w, error);
    writerEnsureClosedRejected(w, error);
    s.writer = undefined;
    w.stream = undefined;
  }
  function writerWrite(w, chunk) {
    const s = w.stream;
    const c = s.controller;
    let chunkSize;
    try {
      chunkSize = c.strategySizeAlgorithm(chunk);
    } catch (e) {
      if (s.state === 'writable') controllerErrorWritable(c, e);
      chunkSize = 1;
    }
    if (s !== w.stream) return rejected(new TypeError('Cannot write to a stream using a released writer'));
    const state = s.state;
    if (state === 'errored') return rejected(s.storedError);
    if (closeQueuedOrInFlight(s) || state === 'closed') return rejected(new TypeError('The stream is closing or closed and cannot be written to'));
    if (state === 'erroring') return rejected(s.storedError);
    const d = deferred();
    s.writeRequests.push(d);
    try {
      enqueueValueWithSize(c, chunk, chunkSize);
    } catch (e) {
      if (s.state === 'writable') controllerErrorWritable(c, e);
      return d.promise;
    }
    if (!closeQueuedOrInFlight(s) && s.state === 'writable') updateBackpressure(s, writableBackpressure(c));
    advanceQueueIfNeeded(c);
    return d.promise;
  }
  function writerCloseWithErrorPropagation(w) {
    const s = w.stream;
    const state = s.state;
    if (closeQueuedOrInFlight(s) || state === 'closed') return resolved(undefined);
    if (state === 'errored') return rejected(s.storedError);
    return writableClose(s);
  }

  class WritableStreamDefaultWriter {
    constructor(stream) {
      const s = slots(stream, 'WritableStream', 'WritableStreamDefaultWriter');
      writerInit(setSlots(this, { kind: 'WritableStreamDefaultWriter' }), s);
    }
    get closed() {
      const w = this && this[SLOTS];
      if (!w || w.kind !== 'WritableStreamDefaultWriter') return rejected(new TypeError('Illegal invocation'));
      return w.closed.promise;
    }
    get desiredSize() {
      const w = slots(this, 'WritableStreamDefaultWriter');
      if (!w.stream) throw new TypeError('Writer is released');
      return writerDesiredSize(w);
    }
    get ready() {
      const w = this && this[SLOTS];
      if (!w || w.kind !== 'WritableStreamDefaultWriter') return rejected(new TypeError('Illegal invocation'));
      return w.ready.promise;
    }
    abort(reason) {
      const w = this && this[SLOTS];
      if (!w || w.kind !== 'WritableStreamDefaultWriter') return rejected(new TypeError('Illegal invocation'));
      if (!w.stream) return rejected(new TypeError('Writer is released'));
      return writableAbort(w.stream, reason);
    }
    close() {
      const w = this && this[SLOTS];
      if (!w || w.kind !== 'WritableStreamDefaultWriter') return rejected(new TypeError('Illegal invocation'));
      if (!w.stream) return rejected(new TypeError('Writer is released'));
      if (closeQueuedOrInFlight(w.stream)) return rejected(new TypeError('Cannot close an already-closing stream'));
      return writableClose(w.stream);
    }
    releaseLock() {
      const w = slots(this, 'WritableStreamDefaultWriter');
      if (w.stream) writerRelease(w);
    }
    write(chunk) {
      const w = this && this[SLOTS];
      if (!w || w.kind !== 'WritableStreamDefaultWriter') return rejected(new TypeError('Illegal invocation'));
      if (!w.stream) return rejected(new TypeError('Cannot write to a stream using a released writer'));
      return writerWrite(w, chunk);
    }
  }
  tag(WritableStreamDefaultWriter.prototype, 'WritableStreamDefaultWriter');

  // Controller.
  function setUpWritableController(s, c, startAlgorithm, writeAlgorithm, closeAlgorithm, abortAlgorithm, hwm, sizeAlgorithm) {
    c.stream = s;
    s.controller = c;
    resetQueue(c);
    c.signal = new StreamAbortSignal();
    c.started = false;
    c.strategySizeAlgorithm = sizeAlgorithm;
    c.strategyHWM = hwm;
    c.writeAlgorithm = writeAlgorithm;
    c.closeAlgorithm = closeAlgorithm;
    c.abortAlgorithm = abortAlgorithm;
    updateBackpressure(s, writableBackpressure(c));
    const startResult = startAlgorithm();
    uponPromise(resolved(startResult), () => {
      c.started = true;
      advanceQueueIfNeeded(c);
    }, (r) => {
      c.started = true;
      dealWithRejection(s, r);
    });
  }
  function setUpWritableControllerFromSink(s, sink, hwm, sizeAlgorithm) {
    const c = setSlots(Object.create(WritableStreamDefaultController.prototype), { kind: 'WritableStreamDefaultController' });
    const start = getMethod(sink, 'start', 'underlyingSink');
    const write = getMethod(sink, 'write', 'underlyingSink');
    const close = getMethod(sink, 'close', 'underlyingSink');
    const abort = getMethod(sink, 'abort', 'underlyingSink');
    setUpWritableController(s, c,
      start ? () => start.call(sink, c.api) : () => undefined,
      write ? (chunk) => promiseCall(write, sink, [chunk, c.api]) : () => resolved(),
      close ? () => promiseCall(close, sink, []) : () => resolved(),
      abort ? (reason) => promiseCall(abort, sink, [reason]) : () => resolved(),
      hwm, sizeAlgorithm);
  }
  function clearWritableAlgorithms(c) {
    c.writeAlgorithm = () => resolved();
    c.closeAlgorithm = () => resolved();
    c.abortAlgorithm = () => resolved();
    c.strategySizeAlgorithm = () => 1;
  }
  const writableBackpressure = (c) => c.strategyHWM - c.queueTotalSize <= 0;
  function controllerErrorWritable(c, error) {
    clearWritableAlgorithms(c);
    startErroring(c.stream, error);
  }
  function advanceQueueIfNeeded(c) {
    const s = c.stream;
    if (!c.started || s.inFlightWriteRequest !== undefined) return;
    if (s.state === 'erroring') {
      finishErroring(s);
      return;
    }
    if (c.queue.length === 0) return;
    const value = c.queue[0].value;
    if (value === CLOSE_SENTINEL) {
      s.inFlightCloseRequest = s.closeRequest;
      s.closeRequest = undefined;
      dequeueValue(c);
      const sinkClose = c.closeAlgorithm();
      clearWritableAlgorithms(c);
      uponPromise(sinkClose, () => finishInFlightClose(s), (r) => finishInFlightCloseWithError(s, r));
      return;
    }
    s.inFlightWriteRequest = s.writeRequests.shift();
    uponPromise(c.writeAlgorithm(value), () => {
      finishInFlightWrite(s);
      const state = s.state;
      dequeueValue(c);
      if (!closeQueuedOrInFlight(s) && state === 'writable') updateBackpressure(s, writableBackpressure(c));
      advanceQueueIfNeeded(c);
    }, (reason) => {
      if (s.state === 'writable') clearWritableAlgorithms(c);
      finishInFlightWriteWithError(s, reason);
    });
  }

  class WritableStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get signal() { return slots(this, 'WritableStreamDefaultController').signal; }
    error(e) {
      const c = slots(this, 'WritableStreamDefaultController');
      if (c.stream.state !== 'writable') return;
      controllerErrorWritable(c, e);
    }
  }
  tag(WritableStreamDefaultController.prototype, 'WritableStreamDefaultController');

  class WritableStream {
    constructor(underlyingSink, strategy) {
      const sink = underlyingSink === undefined ? null : underlyingSink;
      if (sink !== null && !isObject(sink)) throw new TypeError('underlyingSink must be an object');
      if (sink && sink.type !== undefined) throw new RangeError('Invalid type is specified');
      const strat = strategy === undefined ? {} : strategy;
      const s = initWritable(this);
      setUpWritableControllerFromSink(s, sink, extractHighWaterMark(strat, 1), extractSizeAlgorithm(strat));
    }
    get locked() { return slots(this, 'WritableStream').writer !== undefined; }
    abort(reason) {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'WritableStream') return rejected(new TypeError('Illegal invocation'));
      if (s.writer !== undefined) return rejected(new TypeError('Cannot abort a stream that already has a writer'));
      return writableAbort(s, reason);
    }
    close() {
      const s = this && this[SLOTS];
      if (!s || s.kind !== 'WritableStream') return rejected(new TypeError('Illegal invocation'));
      if (s.writer !== undefined) return rejected(new TypeError('Cannot close a stream that already has a writer'));
      if (closeQueuedOrInFlight(s)) return rejected(new TypeError('Cannot close an already-closing stream'));
      return writableClose(s);
    }
    getWriter() { return acquireWriter(slots(this, 'WritableStream')).api; }
  }
  tag(WritableStream.prototype, 'WritableStream');

  // ---------------------------------------------------------------------
  // TransformStream
  // ---------------------------------------------------------------------
  function setBackpressure(t, backpressure) {
    if (t.backpressureChangePromise) t.backpressureChangePromise.resolve(undefined);
    t.backpressureChangePromise = deferred();
    t.backpressure = backpressure;
  }
  function transformError(t, e) {
    controllerError(t.readable.controller, e);
    errorWritableAndUnblockWrite(t, e);
  }
  function errorWritableAndUnblockWrite(t, e) {
    clearTransformAlgorithms(t.controller);
    if (t.writable.state === 'writable') controllerErrorWritable(t.writable.controller, e);
    if (t.backpressure) setBackpressure(t, false);
  }
  function clearTransformAlgorithms(c) {
    c.transformAlgorithm = () => resolved();
    c.flushAlgorithm = () => resolved();
    c.cancelAlgorithm = () => resolved();
  }
  function transformEnqueue(c, chunk) {
    const t = c.stream;
    const rc = t.readable.controller;
    if (!canCloseOrEnqueue(rc)) throw new TypeError('Readable side is not in a state that permits enqueue');
    try {
      controllerEnqueue(rc, chunk);
    } catch (e) {
      errorWritableAndUnblockWrite(t, e);
      throw t.readable.storedError;
    }
    if (hasBackpressure(rc) !== t.backpressure) setBackpressure(t, true);
  }
  function performTransform(c, chunk) {
    return c.transformAlgorithm(chunk).then(undefined, (r) => {
      transformError(c.stream, r);
      throw r;
    });
  }
  function finishWith(c, promise, onOk, onError) {
    c.finishPromise = deferred();
    uponPromise(promise, () => onOk(c.finishPromise), (r) => onError(c.finishPromise, r));
    return c.finishPromise.promise;
  }

  class TransformStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get desiredSize() { return desiredSize(slots(this, 'TransformStreamDefaultController').stream.readable.controller); }
    enqueue(chunk) { transformEnqueue(slots(this, 'TransformStreamDefaultController'), chunk); }
    error(reason) { transformError(slots(this, 'TransformStreamDefaultController').stream, reason); }
    terminate() {
      const c = slots(this, 'TransformStreamDefaultController');
      const t = c.stream;
      controllerClose(t.readable.controller);
      errorWritableAndUnblockWrite(t, new TypeError('TransformStream terminated'));
    }
  }
  tag(TransformStreamDefaultController.prototype, 'TransformStreamDefaultController');

  class TransformStream {
    constructor(transformer, writableStrategy, readableStrategy) {
      const tr = transformer === undefined ? null : transformer;
      if (tr !== null && !isObject(tr)) throw new TypeError('transformer must be an object');
      if (tr && tr.readableType !== undefined) throw new RangeError('Invalid readableType specified');
      if (tr && tr.writableType !== undefined) throw new RangeError('Invalid writableType specified');
      const ws = writableStrategy === undefined ? {} : writableStrategy;
      const rs = readableStrategy === undefined ? {} : readableStrategy;
      const readableHWM = extractHighWaterMark(rs, 0);
      const readableSize = extractSizeAlgorithm(rs);
      const writableHWM = extractHighWaterMark(ws, 1);
      const writableSize = extractSizeAlgorithm(ws);
      const t = setSlots(this, { kind: 'TransformStream', backpressure: undefined, backpressureChangePromise: undefined });
      const startPromise = deferred();
      const startAlgorithm = () => startPromise.promise;
      const c = setSlots(Object.create(TransformStreamDefaultController.prototype), { kind: 'TransformStreamDefaultController' });
      c.stream = t;
      c.finishPromise = undefined;
      t.controller = c;
      t.writable = createWritable(startAlgorithm,
        (chunk) => {
          if (t.backpressure) {
            return t.backpressureChangePromise.promise.then(() => {
              if (t.writable.state === 'erroring') throw t.writable.storedError;
              return performTransform(c, chunk);
            });
          }
          return performTransform(c, chunk);
        },
        () => {
          if (c.finishPromise) return c.finishPromise.promise;
          const readable = t.readable;
          const flush = c.flushAlgorithm();
          clearTransformAlgorithms(c);
          return finishWith(c, flush, (d) => {
            if (readable.state === 'errored') d.reject(readable.storedError);
            else {
              controllerClose(readable.controller);
              d.resolve(undefined);
            }
          }, (d, r) => {
            controllerError(readable.controller, r);
            d.reject(r);
          });
        },
        (reason) => {
          if (c.finishPromise) return c.finishPromise.promise;
          const readable = t.readable;
          const cancel = c.cancelAlgorithm(reason);
          clearTransformAlgorithms(c);
          return finishWith(c, cancel, (d) => {
            if (readable.state === 'errored') d.reject(readable.storedError);
            else {
              controllerError(readable.controller, reason);
              d.resolve(undefined);
            }
          }, (d, r) => {
            controllerError(readable.controller, r);
            d.reject(r);
          });
        },
        writableHWM, writableSize);
      t.readable = createReadable(startAlgorithm,
        () => {
          setBackpressure(t, false);
          return t.backpressureChangePromise.promise;
        },
        (reason) => {
          if (c.finishPromise) return c.finishPromise.promise;
          const writable = t.writable;
          const cancel = c.cancelAlgorithm(reason);
          clearTransformAlgorithms(c);
          return finishWith(c, cancel, (d) => {
            if (writable.state === 'errored') d.reject(writable.storedError);
            else {
              if (writable.state === 'writable') controllerErrorWritable(writable.controller, reason);
              if (t.backpressure) setBackpressure(t, false);
              d.resolve(undefined);
            }
          }, (d, r) => {
            if (writable.state === 'writable') controllerErrorWritable(writable.controller, r);
            if (t.backpressure) setBackpressure(t, false);
            d.reject(r);
          });
        },
        readableHWM, readableSize);
      setBackpressure(t, true);
      const transform = getMethod(tr, 'transform', 'transformer');
      const flush = getMethod(tr, 'flush', 'transformer');
      const cancel = getMethod(tr, 'cancel', 'transformer');
      c.transformAlgorithm = transform
        ? (chunk) => promiseCall(transform, tr, [chunk, c.api])
        : (chunk) => {
          try {
            transformEnqueue(c, chunk);
            return resolved();
          } catch (e) {
            return rejected(e);
          }
        };
      c.flushAlgorithm = flush ? () => promiseCall(flush, tr, [c.api]) : () => resolved();
      c.cancelAlgorithm = cancel ? (reason) => promiseCall(cancel, tr, [reason]) : () => resolved();
      const start = getMethod(tr, 'start', 'transformer');
      startPromise.resolve(start ? start.call(tr, c.api) : undefined);
    }
    get readable() { return slots(this, 'TransformStream').readable.api; }
    get writable() { return slots(this, 'TransformStream').writable.api; }
  }
  tag(TransformStream.prototype, 'TransformStream');

  // ---------------------------------------------------------------------
  // Encoding streams
  // ---------------------------------------------------------------------
  class TextEncoderStream {
    constructor() {
      const encoder = new env.TextEncoder();
      let pending = '';
      const ts = new TransformStream({
        transform(chunk, c) {
          let s = pending + String(chunk);
          pending = '';
          const last = s.charCodeAt(s.length - 1);
          if (last >= 0xd800 && last <= 0xdbff) {
            pending = s.slice(-1);
            s = s.slice(0, -1);
          }
          if (s) c.enqueue(encoder.encode(s));
        },
        flush(c) {
          if (pending) c.enqueue(Uint8Array.of(0xef, 0xbf, 0xbd));
        },
      });
      setSlots(this, { kind: 'TextEncoderStream', ts });
    }
    get encoding() { slots(this, 'TextEncoderStream'); return 'utf-8'; }
    get readable() { return slots(this, 'TextEncoderStream').ts.readable; }
    get writable() { return slots(this, 'TextEncoderStream').ts.writable; }
  }
  tag(TextEncoderStream.prototype, 'TextEncoderStream');

  class TextDecoderStream {
    constructor(label, options) {
      const decoder = new env.TextDecoder(label, options);
      const ts = new TransformStream({
        transform(chunk, c) {
          if (!(chunk instanceof ArrayBuffer) && !ArrayBuffer.isView(chunk)) {
            throw new TypeError('TextDecoderStream: chunk must be an ArrayBuffer or ArrayBuffer view');
          }
          const s = decoder.decode(chunk, { stream: true });
          if (s) c.enqueue(s);
        },
        flush(c) {
          const s = decoder.decode();
          if (s) c.enqueue(s);
        },
      });
      setSlots(this, { kind: 'TextDecoderStream', ts, decoder });
    }
    get encoding() { return slots(this, 'TextDecoderStream').decoder.encoding; }
    get fatal() { return slots(this, 'TextDecoderStream').decoder.fatal; }
    get ignoreBOM() { return slots(this, 'TextDecoderStream').decoder.ignoreBOM; }
    get readable() { return slots(this, 'TextDecoderStream').ts.readable; }
    get writable() { return slots(this, 'TextDecoderStream').ts.writable; }
  }
  tag(TextDecoderStream.prototype, 'TextDecoderStream');

  const api = {
    ReadableStream, ReadableStreamDefaultReader, ReadableStreamBYOBReader, ReadableStreamDefaultController,
    ReadableByteStreamController, ReadableStreamBYOBRequest, WritableStream, WritableStreamDefaultWriter,
    WritableStreamDefaultController, TransformStream, TransformStreamDefaultController, TextEncoderStream,
    TextDecoderStream, CountQueuingStrategy, ByteLengthQueuingStrategy,
  };
  return api;
};
