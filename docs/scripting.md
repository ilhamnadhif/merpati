# Scripting

Merpati runs Postman-style JavaScript in a sandbox built on the pure-Rust
[boa](https://boajs.dev) engine (`crates/merpati-script`). Scripts cannot touch
the file system or the network except through the APIs below. This page covers
the `pm` API for HTTP requests, scripts for the other protocols (PRD FR-S7), and
packages: npm, JSR and the local Package Library (PRD section 8).

- [Execution model](#execution-model)
- [HTTP requests](#http-requests)
- [Other protocols](#other-protocols)
- [Intl and Web Streams](#intl-and-web-streams)
- [Packages](#packages)
- [Host integration](#host-integration)
- [Limitations](#limitations)

## Execution model

Each request, folder and collection has three scripts (`Scripts` in
`merpati-core`):

| Script | `pm.info.eventName` | Runs |
| --- | --- | --- |
| `pre_request` | `prerequest` | before the request is sent (before connect / invoke / call) |
| `post_response` | `test` | after the response (after the call, or after a connection closed) |
| `on_message` | `message` | for every message of a streaming protocol |

Scripts of one event run in order collection → folders (outer to inner) →
request, in one job. An error in one script is reported with its location and
the next script still runs. A job has a wall-clock timeout (settings: script
timeout, default 30 s); runaway loops are stopped by a loop-iteration limit.

In the app and the CLI, jobs run in a separate worker process (the same
executable, started with `--merpati-script-worker`, kept for reuse). A
script that crashes the JavaScript engine (for example `JSON.stringify` of
data nested hundreds of thousands of levels deep), keeps running in native
code after its timeout (a catastrophic regular expression), or uses more
memory than the limit (settings: script memory limit, default 1024 MB) only
loses its worker: the job fails with a clear error and the next job gets a
fresh worker. Cancelling a run stops the job at once.

## HTTP requests

The `pm` object follows Postman's API:

- **Variables**: `pm.variables` (all scopes, local writes), `pm.environment`,
  `pm.globals`, `pm.collectionVariables`, `pm.iterationData`; each has
  `get/set/unset/has/clear/toObject/replaceIn`. Values keep their type like
  in Postman: `get` and `toObject` return numbers, booleans and objects, and
  a value set by a script keeps its type in later scripts of the run. A
  variable typed `string`, `number` or `boolean` (imported from Postman, or
  `set(key, value, type)`) converts values set on it with `String`, `Number`
  or `Boolean`; others keep whatever is set. `{{name}}` always inserts the
  text (`5`, `true`, objects as JSON). The legacy
  `postman.setEnvironmentVariable`/`setGlobalVariable` store `value.toString()`
  as Postman does.
- **Request**: `pm.request` with `url` (postman-collection `Url`), `method`,
  `headers` (a `PropertyList`: `get/has/add/upsert/remove/toObject/each`),
  `body` (`mode` plus `raw`/`urlencoded`/`formdata`/`file`/`graphql`). Changes
  made in pre-request scripts are sent; unchanged parts stay byte-for-byte as
  saved.
- **Response** (post-response): `pm.response.code`, `status`, `headers`,
  `responseTime`, `responseSize`, `text()`, `json()`, `cookies`, `size()`,
  `dataURI()`.
- **Tests**: `pm.test(name, fn)` (sync, async or `done` callback),
  `pm.test.skip`, `pm.expect` (Chai BDD), and the chai-postman assertions:
  `pm.response.to.have.status(200)`, `.to.be.ok`, `.to.have.header(k[, v])`,
  `.to.have.jsonBody(path[, value])`, `.to.have.jsonSchema(schema)`,
  `.to.have.responseTime.below(ms)`, `.to.be.json`, ...
- **Other**: `pm.sendRequest(req[, callback])` (callback or promise),
  `pm.cookies` (+ `jar()`), `pm.info`, `pm.execution.setNextRequest(name|null)`,
  `pm.execution.skipRequest()`, `pm.visualizer.set(template, data)`,
  `console.*`, timers, `Buffer`, `URL`, `URLSearchParams`, `TextEncoder`,
  `TextDecoder`, `Blob`, `File`, `atob`/`btoa`, `crypto.getRandomValues`,
  `crypto.randomUUID`, `structuredClone`, `queueMicrotask`, `Array.fromAsync`,
  and `Intl` and Web Streams (see [Intl and Web Streams](#intl-and-web-streams)).
- **Legacy sandbox**: `tests[...]`, `postman.*`, `responseBody`,
  `responseCode`, `responseHeaders`, `responseTime`, `request`, `environment`,
  `globals`, `data`, `iteration`, `xml2Json`.
- **Built-in modules** (`require` or `pm.require`): `chai`, `lodash`, `moment`,
  `crypto-js`, `tv4`, `ajv` (on tv4), `uuid`, `atob`, `btoa`, `xml2js`,
  `buffer`, `url`, `querystring`, `util`, `events`, `timers`, `assert`. Node
  built-ins such as `fs`, `child_process`, `net` or `http` throw
  `Cannot find module 'fs'. Node.js built-in modules are not available in Merpati scripts.`

GraphQL requests are HTTP requests: `pm.request.body.graphql` holds `query`,
`variables` and `operationName`, and the usual pre-request/post-response
scripts run before the query and after the response.

## Other protocols

For gRPC, WebSocket, Socket.IO, MQTT, MCP and AI requests the job carries a
protocol context. Everything from the HTTP API that is not tied to HTTP works
the same: variables, `pm.test`, `pm.expect`, `console`, `pm.sendRequest`,
`pm.info` (plus `pm.info.protocol`: `grpc`, `websocket`, `socketio`, `mqtt`,
`mcp`, `ai`, or `http`), `pm.execution.skipRequest()` and `pm.visualizer`.

The protocol objects are built from JSON supplied by the host:

- `pm.request` holds every field of the request JSON as a property. Lists
  named `headers`, `metadata`, `params`, `env` or `trailers` become
  `PropertyList`s (`get/has/add/upsert/remove/toObject`, case-insensitive
  keys), everything else stays plain JSON you can change in place.
  Pre-request scripts may change `pm.request` (or assign a whole new object);
  the result goes back to the host. Other events see it read-only.
- `pm.response` (post-response) holds the result. Besides its fields it has
  `responseTime`, `text()`, `json()`, `messages` (a message list) and the
  assertions `to.be.ok`, `to.have.statusCode(n)`, `to.have.metadata(key[, value])`,
  `to.have.trailer(key[, value])` and `to.have.responseTime.below(ms)`.
  Values may be strings or regular expressions.
- `pm.message` (on-message) is one message: `data`, `timestamp` (Unix ms),
  `direction` (`incoming` or `outgoing`) plus protocol fields, with `text()`
  (strings as-is, anything else as JSON) and `json()` (parses strings).

A message list (`pm.request.messages` for gRPC, `pm.response.messages`) is an
array with the postman-collection helpers `idx(i)`, `count()`, `all()`,
`each(fn)`, `add(m)`, `remove(fn)` and `clear()`; its items are messages as
above.

### gRPC

Follows Postman's gRPC scripting API. Before invoke, on message, after response.

| Object | Fields |
| --- | --- |
| `pm.request` | `url` (`grpc://host:port` or `grpcs://...`), `service`, `method`, `methodPath` (`package.Service/Method`), `metadata` (list), `messages` (list of `{data}`, the message being sent), `deadlineMs`, `auth` |
| `pm.response` | `statusCode` (gRPC code, 0 = OK), `statusMessage`, `metadata`, `trailers`, `messages`, `responseTime`; `json()` is the first message's data |
| `pm.message` | `data`, `timestamp`, `direction` |

```js
// Before invoke
pm.request.metadata.upsert({ key: 'authorization', value: 'Bearer ' + pm.environment.get('token') });
pm.request.messages.idx(0).data.name = pm.variables.replaceIn('{{user}}');
pm.request.methodPath = 'helloworld.Greeter/SayHelloAgain';

// On message (server streaming)
pm.test('progress is monotonic', () => {
  pm.expect(pm.message.data.percent).to.be.at.least(Number(pm.collectionVariables.get('last') || 0));
});
pm.collectionVariables.set('last', pm.message.data.percent);

// After response
pm.test('OK', () => pm.response.to.have.statusCode(0));
pm.test('fast', () => pm.response.to.have.responseTime.below(200));
pm.test('has trailer', () => pm.response.to.have.trailer('x-trace-id'));
pm.test('greets', () => pm.expect(pm.response.messages.idx(0).data.message).to.include('Hello'));
```

`to.be.ok` means status code 0 for gRPC.

### WebSocket

| Object | Fields |
| --- | --- |
| `pm.request` | `url`, `headers`, `subprotocols`; when the host runs the pre-request script for an outgoing message also `message: {data, type}` (`type` is `text` or `binary`; binary bodies keep their saved text encoding) |
| `pm.message` | `data` (the frame text), `type`, `timestamp`, `direction` |
| `pm.response` (after close) | `closeCode`, `closeReason`, `messages`, `responseTime` |

```js
// On message
const msg = pm.message.json();
pm.test('no error frames', () => pm.expect(msg.type).to.not.equal('error'));
```

### Socket.IO

| Object | Fields |
| --- | --- |
| `pm.request` | `url`, `path`, `namespace`, `headers`, `params`, `auth` (the parsed handshake payload), `listeners` (event names); for an outgoing emit `message: {event, args, ack}` (binary arguments as base64 strings, file arguments as their path) |
| `pm.message` | `event`, `args` (array; binary attachments as base64 strings), `data` (the first argument), `timestamp`, `direction` |
| `pm.response` (after disconnect) | `messages`, `responseTime` |

### MQTT

| Object | Fields |
| --- | --- |
| `pm.request` | `url`, `clientId`, `version`, `keepAlive`, `cleanSession`, `subscriptions` (`[{topic, qos}]`); for a publish `message: {topic, payload, qos, retain}` |
| `pm.message` | `topic`, `payload`, `data` (same as `payload`), `qos`, `retain`, `timestamp`, `direction`, `userProperties` (MQTT 5 in the app: `[{key, value}]`) |
| `pm.response` (after disconnect) | `messages`, `responseTime` |

```js
// On message
pm.test('temperature in range', () => pm.expect(pm.message.json().celsius).to.be.within(-40, 60));
```

### Sessions in the Collection Runner

The Collection Runner and `merpati run` run WebSocket, Socket.IO and MQTT
requests as scripted sessions: the pre-request script runs once before
connecting, the saved messages are sent in order (MQTT subscribes to the
enabled subscriptions first; Socket.IO waits for each acknowledgement it asked
for), the on-message script runs for every incoming message, and the
post-response script gets a summary. The session stops at the first of: a
number of incoming messages, a matching message, silence after the last send,
the session timeout, or the server closing the connection. The request's
Runner section (stored as `runner` in the request file) sets these.

| Object | Fields |
| --- | --- |
| `pm.request` | the protocol fields above plus `messages` (the saved messages, in the shape of `message` above) and `runner`: `{waitMs, maxMessages, untilEvent, timeoutMs, ackReply}`. A pre-request script may change both, the URL and (MQTT) `subscriptions`. |
| `pm.message` | one incoming message, as above |
| `pm.response` | `messages` (incoming), `sent` (outgoing; Socket.IO emits carry `ackArgs` once acknowledged), `receivedCount`, `sentCount`, `stopReason` (`max_messages`, `until_event`, `silence`, `timeout`, `closed`, `error`, `cancelled`), `error`, timings in ms (`connectTime`, `firstMessageTime`, `responseTime` without the final silence, `duration`); WebSocket `handshakeStatus`, `subprotocol`, `closeCode`, `closeReason`; Socket.IO `sid`, `namespace`; MQTT `clientId`, `sessionPresent`, `subscriptions` |

`untilEvent` matches the event name (Socket.IO), a topic filter with `+`/`#`
(MQTT), or the message text or its JSON `type`/`event` field (WebSocket).
`ackReply` (Socket.IO) answers every acknowledgement the server asks for with
these arguments (a JSON array, or one value); without it they stay unanswered.

```js
// Pre-request: send one more message and wait for "done"
pm.request.messages.push({ event: 'burst', args: [3] });
pm.request.runner.untilEvent = 'done';

// Post-response
pm.test('finished normally', () => pm.expect(pm.response.stopReason).to.equal('until_event'));
pm.test('three ticks', () => pm.expect(pm.response.messages.filter((m) => m.event === 'tick')).to.have.length(3));
```

### MCP

| Object | Fields |
| --- | --- |
| `pm.request` | `transport` (`http` or `stdio`), `url` and `headers`, or `command`, `args`, `env`, `cwd`; for a call `kind` (`tool`, `prompt`, `resource`), `name` (tool/prompt name or resource URI), `arguments` (object) |
| `pm.response` | `result` (the MCP result object), `isError`, `responseTime`; `text()` joins the text content items, `json()` is `result` |

`to.be.ok` means `isError` is false.

```js
pm.test('tool succeeded', () => pm.response.to.be.ok);
pm.test('structured output', () => pm.expect(pm.response.json().structuredContent.temp).to.be.a('number'));
```

### AI model requests

| Object | Fields |
| --- | --- |
| `pm.request` | `provider` (`open_ai`, `anthropic`, `google`, `open_ai_compatible`), `url` (base URL), `model`, `system`, `messages` (`[{role, content}]`, a plain array), `temperature`, `maxTokens`, `stream`, `extra` (object merged into the provider request) |
| `pm.message` (streaming, if the host runs on-message per chunk) | `data` (the text delta), `timestamp`, `direction` |
| `pm.response` | `content` (assistant text), `model`, `finishReason`, `usage` (e.g. `{inputTokens, outputTokens}`), `statusCode` (HTTP), `responseTime`; `text()` is `content`, `json()` parses it (a Markdown code fence around the JSON is ignored) |

```js
// Pre-request
pm.request.messages.push({ role: 'user', content: 'Answer as JSON: ' + pm.iterationData.get('question') });
pm.request.temperature = 0;

// Post-response
pm.test('valid JSON answer', () => pm.expect(pm.response.json()).to.have.property('answer'));
pm.test('cheap', () => pm.expect(pm.response.usage.outputTokens).to.be.below(500));
```

`to.be.ok` means a 2xx status code (or no status code and no `error` field).

## Intl and Web Streams

boa is built without ICU (its data would add megabytes to the app), so `Intl`
and the Web Streams API are JavaScript polyfills bundled with Merpati
(`crates/merpati-script/js/intl.js`, `js/streams.js`). They are installed on
first use (`Intl`, `toLocaleString`, `localeCompare`, `ReadableStream`,
`Blob.prototype.stream()`, ...): scripts that do not use them pay nothing, and
the first use costs about 20 ms per script worker.

### Intl

English (CLDR `en`, i.e. US English) output matches Node.js and Chrome for:

- `Intl.NumberFormat`: styles decimal, percent, currency (`symbol`,
  `narrowSymbol`, `code`, `name`, accounting sign) and unit (simple units and
  `x-per-y`); integer, fraction and significant digits, `roundingMode`,
  `roundingIncrement`, `roundingPriority`, `trailingZeroDisplay`,
  `useGrouping`, `signDisplay`; notation standard, scientific, engineering and
  compact (short/long). `format` takes numbers, BigInts and exact numeric
  strings; also `formatToParts`, `formatRange`, `formatRangeToParts`,
  `resolvedOptions`.
- `Intl.DateTimeFormat`: `dateStyle`/`timeStyle`, the component options,
  `hour12`/`hourCycle`, `dayPeriod`, `fractionalSecondDigits`, all six
  `timeZoneName` styles; `formatToParts`, `formatRange` (basic: a shared date
  and AM/PM are merged, nothing else), `resolvedOptions`.
- Time zones: the local zone (the default, consistent with the `Date`
  getters), `UTC`, offsets (`+05:30`) and IANA names (`America/New_York`) with
  daylight saving time, read from the system tz database (macOS, Linux), or
  from a copy built into Merpati where the system has none (Windows, slim
  containers). Common zones have long
  names (`Eastern Daylight Time`, `Western Indonesia Time`); others use
  `GMT+07:00`.
- `Intl.Collator`: base letters, then accents, then case, in CLDR root order;
  `sensitivity`, `numeric`, `caseFirst`, `ignorePunctuation`.
- `Intl.PluralRules` (cardinal, ordinal, `selectRange`),
  `Intl.RelativeTimeFormat` (long/short/narrow, `numeric: 'auto'`,
  `formatToParts`), `Intl.ListFormat`, `Intl.Segmenter` (grapheme clusters,
  words and sentences per UAX #29), `Intl.Locale` (`maximize`, `minimize`,
  week/text info), `Intl.getCanonicalLocales`, `Intl.supportedValuesOf`.
- `Number.prototype.toLocaleString`, `BigInt.prototype.toLocaleString`,
  `Date.prototype.toLocaleString`/`toLocaleDateString`/`toLocaleTimeString`
  and `String.prototype.localeCompare` use the same code.

Other languages: NumberFormat also formats `de`, `es`, `fr`, `id`, `it`, `nl`
and `pt` (Brazilian); PluralRules knows those plus `ru`, `uk`, `pl`, `cs`,
`sv`, `nb`, `tr`, `ja`, `ko`, `zh`, `th`, `vi` and `ms`; Collator uses the root
order for `de`, `fr`, `id`, `it`, `ms`, `nl` and `pt`. Every other locale falls
back to en-US, and `resolvedOptions().locale` says which locale was used:

```js
new Intl.NumberFormat('id-ID', { style: 'currency', currency: 'IDR' }).format(1234.5); // 'Rp 1.235'
new Intl.DateTimeFormat('id-ID').resolvedOptions().locale; // 'en-US': dates are formatted in English
new Intl.NumberFormat('en-GB').resolvedOptions().locale;   // 'en'
```

As in Chrome and Node.js, `format()` puts a normal space before AM/PM, while
`formatToParts()` and `formatRange()` keep ICU's narrow no-break space
(U+202F).

Not covered: `Intl.DisplayNames` and `Intl.DurationFormat`; non-Gregorian
calendars and non-Latin digits; date patterns and month or day names of other
languages; compact notation, unit names and currency names outside English;
dictionary-based word breaks (Chinese and Japanese break per character, with
katakana runs kept together; Thai per run of letters); language-specific collation (Spanish ñ, Swedish å/ä/ö,
German phonebook); ICU's renaming of time zone aliases (`Asia/Calcutta` is
reported as given).

### Web Streams

`ReadableStream` (default and BYOB readers, async iteration and
`values({ preventCancel })`, `ReadableStream.from`, `tee`, `pipeTo` with
`preventClose`/`preventAbort`/`preventCancel`/`signal`, `pipeThrough`, byte
streams with `byobRequest` and `autoAllocateChunkSize`), `WritableStream`
(writer, backpressure, `abort`, the controller's `signal`), `TransformStream`,
`TextEncoderStream`, `TextDecoderStream`, `CountQueuingStrategy` and
`ByteLengthQueuingStrategy` follow the WHATWG Streams Standard. Related:
`Blob.prototype.stream()`, `TextDecoder` with `{ stream: true }`, `fatal` and
`ignoreBOM`, `TextEncoder.encodeInto`, `queueMicrotask` and `Array.fromAsync`.

All stream work runs as promise jobs in the script's event loop, so the script
timeout and cancellation also stop endless streams.

```js
const { TextLineStream } = pm.require('jsr:@std/streams@1.0.13');
const lines = await Array.fromAsync(ReadableStream.from([pm.response.text()]).pipeThrough(new TextLineStream()));
pm.test('has lines', () => pm.expect(lines).to.not.be.empty);
```

Not covered: `fetch`, `Request` and `Response` (use `pm.sendRequest`; this
also means helpers built on `new Response(stream)`, such as `toBlob` in
`@std/streams`, fail), `CompressionStream`/`DecompressionStream`, and a global
`AbortController` (`pipeTo` accepts any object with `aborted` and
`addEventListener` as its `signal`). Byte streams copy enqueued chunks instead
of transferring them, and `tee()` of a byte stream gives two default streams.

Like the bundled libraries, the polyfills stay loaded for the lifetime of a
script worker. The `Intl` object and the patched built-in methods are restored
after every job; changes to the polyfills' prototypes are not.

## Packages

`pm.require(spec)` (and plain `require`) loads, in this order:

1. **Built-in modules** (see above), by bare name: `require('lodash')`.
2. **npm and JSR packages**: `npm:<name>[@<version>][/<subpath>]` and
   `jsr:@<scope>/<name>[@<version>][/<subpath>]`, e.g.
   `pm.require('npm:dayjs@1.11.13')`, `pm.require('jsr:@std/encoding@1')`.
3. **The Package Library**: any other name, from the workspace's `packages/`
   folder.

Without a package context (for example a script run outside a workspace),
only built-ins load; `npm:<builtin>` specifiers such as `npm:lodash@4.17.21`
then fall back to the bundled library, as before.

### npm and JSR

Packages are pure-JavaScript ES modules from an esm.sh-compatible CDN (setting
`package_registry`, default `https://esm.sh`):

| Specifier | Entry URL |
| --- | --- |
| `npm:lodash-es@4.17.21` | `https://esm.sh/lodash-es@4.17.21?bundle&target=es2022` |
| `npm:@scope/pkg@^2/sub` | `https://esm.sh/@scope/pkg@%5E2/sub?bundle&target=es2022` |
| `jsr:@std/encoding@1` | `https://esm.sh/jsr/@std/encoding@1?bundle&target=es2022` |

`pm.require` returns what Postman scripts expect from a CommonJS `require`:
the default export when the module has only a default export, or when the
default already carries all named exports (CommonJS-style packages such as
dayjs or lodash), otherwise the module namespace (`const { v4 } = pm.require('npm:uuid@9.0.1')`).
In an `async` script, `await import('npm:...')` also works and returns the
namespace.

How fetching works:

- Fetching happens in Rust, never from script code. Only the registry's host
  (same scheme, host and port) is contacted; redirects or imports pointing
  elsewhere are refused.
- Limits: 5 MB per module, 30 MB per `pm.require` (the whole module graph),
  30 s per module (and never past the script timeout). A 5xx answer is retried
  once.
- Imports inside fetched modules may be absolute paths on the registry
  (`/pkg@1/es2022/x.mjs`), relative paths, or `npm:`/`jsr:` specifiers.
- Node built-ins: esm.sh replaces them with polyfills. Harmless ones (`buffer`,
  `process`, `util`, `events`, `stream`, `path`, `url`, `tty`, `os`, ...) load;
  packages that need `fs`, `child_process`, `net`, `tls`, `dgram`, `dns`,
  `http`, `https`, `http2`, `cluster`, `worker_threads`, `inspector`, `vm`,
  `repl` or `readline` fail with
  `... needs the Node.js built-in module 'fs', which is not available in Merpati scripts`.
- Every fetched module is cached on disk (the host's cache directory, by
  default `<config dir>/Merpati/package-cache`), so later runs work offline.
  With network access turned off, a module missing from the cache fails with
  `... is not in the package cache (...) and network access is off`.

Parsed modules stay loaded in a script worker, so the second `pm.require` of
a package is nearly free. They are shared by later jobs on that worker: treat
package exports as read-only.

### Lockfile

`<workspace>/packages.lock.json` pins the SHA-256 of every module URL ever
loaded, in a sorted, diff-friendly format meant to be committed:

```json
{
  "lockfileVersion": 1,
  "packages": {
    "npm:dayjs@1.11.13": "https://esm.sh/dayjs@1.11.13?bundle&target=es2022"
  },
  "modules": {
    "https://esm.sh/dayjs@1.11.13/es2022/dayjs.bundle.mjs": "sha256-1ixzrlWpZ9apLpDDAKmsSZLusiUCNUx4biN7/0oM6cg=",
    "https://esm.sh/dayjs@1.11.13?bundle&target=es2022": "sha256-+hJLP5aGz958unsYS63omBJqSaWgTkdQpQfu/yesCTM="
  }
}
```

New modules are added automatically (pins of a failed load are not). Content
that does not match its pin is refused (a cached copy that does not match is
fetched again first, when network access is allowed):
`integrity mismatch for <url>: packages.lock.json pins sha256-... but the content has sha256-...`.
To accept new content on purpose, delete the affected entries.

Use exact versions. A range such as `jsr:@std/encoding@1` works, but its entry
module changes on the CDN when a new version is published, so a machine
without the cached copy gets an integrity mismatch until the pin is removed.

### Package Library

Shared scripts live in `<workspace>/packages/` and are committed with the
workspace:

| `pm.require(...)` | File |
| --- | --- |
| `'@team/auth'` | `packages/@team/auth.js` (or `.mjs`, `.cjs`, or `packages/@team/auth/index.js`) |
| `'helpers'` | `packages/helpers.js` (same alternatives) |
| `'@team/auth/sign'` | `packages/@team/auth/sign.js` |

- Files may be CommonJS (`module.exports`, `exports.x`, `require`,
  `__filename`, `__dirname`) or ES modules (`import`/`export`). `.mjs` is
  always an ES module and `.cjs` always CommonJS; for `.js` files, top-level
  `import`/`export` statements without `module.exports`/`exports.` mean ES
  module.
- Inside a package, `require('./other')` and `import ... from './other.js'`
  resolve relative to the file; `npm:`/`jsr:` specifiers, built-ins and other
  library names work too, from both kinds of file.
- Built-in module names win over library files of the same name.
- Paths cannot leave `packages/` (also not through symlinks).
- Library files are loaded fresh for every job, so edits apply at once and
  module-level state does not leak between requests.

```js
// packages/@team/auth.js
const CryptoJS = require('crypto-js');
module.exports.sign = (secret, body) => CryptoJS.HmacSHA256(body, secret).toString();

// a request's pre-request script
const { sign } = pm.require('@team/auth');
pm.request.headers.upsert({ key: 'X-Signature', value: sign(pm.environment.get('secret'), pm.request.body.raw) });
```

## Host integration

The contract lives in `merpati_core::script` and is implemented by
`merpati_script::BoaEngine`.

**HTTP** (unchanged): `ScriptJob::new(event, scripts, ScriptRequest)`, plus
`response` for post-response jobs. Set `packages` to enable packages.

**Other protocols**: build the context with the helpers in
`merpati_core::script::protocol` and run
`ScriptJob::for_protocol(event, scripts, ctx)`; fill `info`, `variables`,
`http`, `timeout`, `cancel` and `packages` as for HTTP. The scripts are
`Scripts::code(event)` along the collection → folder → request chain.

| Protocol | Request JSON | Outgoing message (set `ctx.request["message"]`) | Apply pre-request changes |
| --- | --- | --- | --- |
| gRPC | `protocol::grpc(&GrpcRequest)` | (in `messages`) | `protocol::apply_grpc` |
| WebSocket | `protocol::websocket(&WsRequest)` | `protocol::ws_message(&WsSavedMessage)` | `protocol::apply_websocket`; read `message.data` |
| Socket.IO | `protocol::socketio(&SocketIoRequest)` | `protocol::socketio_message(&SocketIoMessage)` | `protocol::apply_socketio`; read `message.event/args` |
| MQTT | `protocol::mqtt(&MqttRequest)` | `protocol::mqtt_message(&MqttMessage)` | `protocol::apply_mqtt`, `protocol::apply_mqtt_message` |
| MCP | `protocol::mcp(&McpRequest, Option<&McpSavedCall>)` | — | `protocol::apply_mcp` |
| AI | `protocol::ai(&AiRequest)` | — | `protocol::apply_ai` |

- Responses: `ctx.with_response(...)` with `protocol::grpc_response(...)`,
  `protocol::mcp_response(...)`, `protocol::ai_response(...)`, or a JSON object
  of the documented shape (`{messages, closeCode, closeReason, responseTime}`
  for connection summaries).
- Messages: `ctx.with_message(protocol::message(data, Direction::Incoming, unix_ms))`;
  add protocol fields to the returned object (`topic`, `qos`, `event`, `args`,
  `type`).
- The modified request comes back in `ScriptOutcome::protocol_request`; it
  equals the input when no script changed it, also after errors, timeouts and
  for empty scripts. `skip_request`, tests, console, errors and variables work
  as for HTTP.
- As for HTTP, pass the request as saved (with `{{variables}}` unresolved) to
  pre-request scripts and resolve variables after applying their changes.

**Packages**: set `job.packages = Some(PackageContext::for_workspace(&workspace_dir, &settings.package_registry))`.
Its fields are `library_dir`, `cache_dir`, `lockfile_path`, `registry_base`
and `allow_network` (`false` = cache only). `None` means built-ins only. The
size and time limits are `EngineConfig::package_module_limit`,
`package_graph_limit` and `package_fetch_timeout`.

## Limitations

- A worker that is killed (memory limit, runaway native code) or crashes
  loses the results of that job; a timeout or cancellation at a `pm.*`,
  console or timer call keeps the results so far.
- The memory limit applies on macOS, Linux and Windows. It is checked every
  25 ms against the worker's physical memory (resident size; on macOS at
  least the physical footprint, on Windows the working set), so a fast
  allocation can overshoot it briefly, and memory the system has paged out is
  not counted on Linux and Windows.
- `Intl` and Web Streams are polyfills with the coverage and gaps listed in
  [Intl and Web Streams](#intl-and-web-streams). There is no `fetch` or
  `Response`; use `pm.sendRequest`.
- Packages that rely on Node.js internals beyond what esm.sh's polyfills
  provide do not work (for example `npm:jsonwebtoken`, which needs Node's
  `crypto`/`stream`); browser-oriented alternatives usually do (`npm:jose`,
  `crypto-js`).
- Package fetching uses the system proxy and the system's trusted
  certificates; the custom proxy and CA settings for requests do not apply.
- Fetched packages run with the same sandbox limits as scripts, but their
  top-level code is not interrupted by the script timeout (the loop-iteration
  limit still applies).
- A package whose top-level `await` does not settle while the job runs fails
  to load.

Packages measured against the real esm.sh: lodash-es, uuid, dayjs, zod,
date-fns, nanoid, qs, ajv, jose, @faker-js/faker, chance, papaparse,
fast-xml-parser, crypto-js, yaml, semver, `jsr:@std/encoding` work. Using
`Intl` or Web Streams: `jsr:@std/assert` (`assertEquals` with its diff),
`jsr:@std/fmt` (bytes with a locale, duration, colors), `jsr:@std/streams`,
`jsr:@std/csv` (including `CsvParseStream`), date-fns (`intlFormat`,
`intlFormatDistance`), luxon (zones, `toLocaleString`, `toRelative`),
intl-messageformat, pretty-bytes (with `locale`), dinero.js, numeral,
currency.js and eventsource-parser (`EventSourceParserStream`).
