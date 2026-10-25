// Merpati script sandbox prelude.
//
// Defines the Postman-compatible `pm` API and the legacy sandbox globals on
// top of a handful of host functions (`__mp_host_*`). Evaluated once per
// worker; every job is set up by `__mp.begin` and torn down by `__mp.finish`,
// which also removes globals created by user code.
(function (G) {
  'use strict';

  const host = {
    check: G.__mp_host_check,
    body: G.__mp_host_body,
    bodyBase64: G.__mp_host_body_base64,
    dynamic: G.__mp_host_dynamic,
    random: G.__mp_host_random,
    send: G.__mp_host_send,
    load: G.__mp_host_load,
    pkgResolve: G.__mp_host_pkg_resolve,
    pkgLoad: G.__mp_host_pkg_load,
    tzLocal: G.__mp_host_tz_local,
    tzOffset: G.__mp_host_tz_offset,
  };
  ['__mp_host_check', '__mp_host_body', '__mp_host_body_base64', '__mp_host_dynamic', '__mp_host_random',
    '__mp_host_send', '__mp_host_load', '__mp_host_pkg_resolve', '__mp_host_pkg_load', '__mp_host_tz_local',
    '__mp_host_tz_offset'].forEach(function (k) { try { delete G[k]; } catch (e) { /* ignore */ } });

  // Captured before any user code runs, so scripts that patch built-ins
  // cannot break the sandbox bookkeeping.
  const getNames = Object.getOwnPropertyNames;
  const getDesc = Object.getOwnPropertyDescriptor;
  const defProp = Object.defineProperty;
  const ownKeys = Reflect.ownKeys;
  const getProto = Object.getPrototypeOf;
  const setProto = Object.setPrototypeOf;
  const isExtensible = Object.isExtensible;
  const jsonParse = JSON.parse;
  const jsonStringify = JSON.stringify;
  const hasOwn = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const isObject = (v) => v !== null && typeof v === 'object';
  const define = (obj, name, value) =>
    Object.defineProperty(obj, name, { value, writable: true, configurable: true, enumerable: false });
  // Variable scopes are collected with the original `Map.prototype.forEach`:
  // a script that patches it must not wipe the variables it hands back.
  const mapForEach = Map.prototype.forEach;
  const apply = Reflect.apply;
  const mapEach = (m, fn) => apply(mapForEach, m, [fn]);
  // `o[k] = v` that also works for a variable named `__proto__`.
  const setKey = (o, k, v) => {
    if (k === '__proto__') defProp(o, k, { value: v, writable: true, configurable: true, enumerable: true });
    else o[k] = v;
  };

  // ---------------------------------------------------------------------
  // Per-job state
  // ---------------------------------------------------------------------
  let S = null;

  function freshState(input) {
    return {
      input,
      event: input.event,
      info: input.info || {},
      locations: input.locations || [],
      location: '',
      tests: [],
      legacyTests: {},
      console: [],
      errors: [],
      next: undefined,
      skip: false,
      visualization: null,
      timers: new Map(),
      timerSeq: 0,
      // console.count / console.time labels.
      counters: Object.create(null),
      timeLabels: Object.create(null),
      bodyText: undefined,
      scopes: null,
      request: null,
      requestBaseline: null,
      response: null,
      cookies: null,
      // Non-HTTP protocol jobs (FR-S7).
      protocol: null,
      protocolInput: null,
      protocolBaseline: null,
      message: null,
      // Package Library modules loaded by this job.
      packages: !!input.packages,
      localPkgs: Object.create(null),
    };
  }

  // ---------------------------------------------------------------------
  // Formatting (a small util.inspect)
  // ---------------------------------------------------------------------
  function inspect(value, opts) {
    const depthLimit = opts && opts.depth !== undefined ? opts.depth : 2;
    const seen = [];
    function fmt(v, depth, top) {
      switch (typeof v) {
        case 'string':
          return top ? v : "'" + v.replace(/\\/g, '\\\\').replace(/'/g, "\\'").replace(/\n/g, '\\n') + "'";
        case 'number':
          return Object.is(v, -0) ? '-0' : String(v);
        case 'bigint':
          return v + 'n';
        case 'boolean':
        case 'undefined':
          return String(v);
        case 'symbol':
          return v.toString();
        case 'function':
          return v.name ? '[Function: ' + v.name + ']' : '[Function (anonymous)]';
      }
      if (v === null) return 'null';
      if (seen.indexOf(v) !== -1) return '[Circular]';
      if (v instanceof Error) return v.stack && typeof v.stack === 'string' && v.stack.indexOf(v.message) !== -1 ? v.stack : (v.name || 'Error') + ': ' + v.message;
      if (v instanceof Date) return isNaN(v.getTime()) ? 'Invalid Date' : v.toISOString();
      if (v instanceof RegExp) return v.toString();
      if (typeof v.toJSON === 'function' && (v instanceof PropertyList || v instanceof Url || v instanceof Property)) {
        return fmt(v.toJSON(), depth, false);
      }
      if (depth > depthLimit) return Array.isArray(v) ? '[Array]' : '[Object]';
      seen.push(v);
      let out;
      if (Array.isArray(v)) {
        const items = v.slice(0, 100).map((x) => fmt(x, depth + 1, false));
        if (v.length > 100) items.push('... ' + (v.length - 100) + ' more items');
        out = items.length ? '[ ' + items.join(', ') + ' ]' : '[]';
      } else if (v instanceof Map) {
        const items = [];
        v.forEach((val, key) => items.push(fmt(key, depth + 1, false) + ' => ' + fmt(val, depth + 1, false)));
        out = 'Map(' + v.size + ') {' + (items.length ? ' ' + items.join(', ') + ' ' : '') + '}';
      } else if (v instanceof Set) {
        const items = [];
        v.forEach((val) => items.push(fmt(val, depth + 1, false)));
        out = 'Set(' + v.size + ') {' + (items.length ? ' ' + items.join(', ') + ' ' : '') + '}';
      } else if (typeof Buffer !== 'undefined' && v instanceof Buffer) {
        const hex = [];
        for (let i = 0; i < Math.min(v.length, 50); i++) hex.push(v[i].toString(16).padStart(2, '0'));
        out = '<Buffer ' + hex.join(' ') + (v.length > 50 ? ' ...' : '') + '>';
      } else {
        const keys = Object.keys(v);
        const items = keys.slice(0, 100).map((k) => {
          const key = /^[A-Za-z_$][\w$]*$/.test(k) ? k : "'" + k + "'";
          let val;
          try { val = fmt(v[k], depth + 1, false); } catch (e) { val = '[Getter]'; }
          return key + ': ' + val;
        });
        const ctor = v.constructor && v.constructor.name && v.constructor !== Object ? v.constructor.name + ' ' : '';
        out = items.length ? ctor + '{ ' + items.join(', ') + ' }' : ctor + '{}';
      }
      seen.pop();
      return out;
    }
    return fmt(value, 0, true);
  }

  function format() {
    const args = Array.prototype.slice.call(arguments);
    if (!args.length) return '';
    let first = args[0];
    let rest = args.slice(1);
    let out;
    if (typeof first === 'string' && first.indexOf('%') !== -1) {
      out = first.replace(/%([sdifjoOc%])/g, function (m, f) {
        if (f === '%') return '%';
        if (!rest.length) return m;
        const a = rest.shift();
        switch (f) {
          case 's': return typeof a === 'string' ? a : inspect(a, { depth: 0 });
          case 'd': case 'i': return String(f === 'i' ? parseInt(a, 10) : Number(a));
          case 'f': return String(parseFloat(a));
          case 'j': try { return JSON.stringify(a); } catch (e) { return '[Circular]'; }
          case 'o': case 'O': return inspect(a, { depth: 4 });
          case 'c': return '';
        }
        return m;
      });
    } else {
      out = inspect(first);
    }
    for (const a of rest) out += ' ' + (typeof a === 'string' ? a : inspect(a));
    return out;
  }

  function errorMessage(e) {
    if (e instanceof Error || (isObject(e) && 'message' in e)) {
      const name = e.name || 'Error';
      return e.message ? name + ': ' + e.message : name;
    }
    return String(e);
  }

  // " (line N)" for the innermost stack frame inside user code.
  function lineSuffix(e) {
    const stack = isObject(e) ? e.stack : undefined;
    if (typeof stack !== 'string') return '';
    const m = /\(user-script:(\d+):(\d+)\)/.exec(stack);
    if (!m) return '';
    // boa reports some errors (e.g. undefined variables) at the function
    // start, which is the wrapper; a line number would only mislead.
    if (m[1] === '1' && S && Number(m[2]) === S.prefixLen) return '';
    return ' (line ' + m[1] + ')';
  }

  function recordError(location, message) {
    if (!S) return;
    S.errors.push({ location: location || S.location || 'script', message: String(message) });
    S.console.push({ level: 'error', message: String(message) });
  }

  // ---------------------------------------------------------------------
  // Variable values
  // ---------------------------------------------------------------------
  function storeString(v) {
    if (typeof v === 'string') return v;
    if (v === undefined) return '';
    if (isObject(v)) {
      try { return JSON.stringify(v); } catch (e) { return String(v); }
    }
    return String(v);
  }

  const MAX_DEPTH = 10;
  function resolveTemplate(str, lookup, depth) {
    if (typeof str !== 'string' || str.indexOf('{{') === -1) return str;
    depth = depth || 0;
    return str.replace(/\{\{([^{}]+)\}\}/g, function (m, rawName) {
      const name = rawName.trim();
      let v = lookup(name);
      if (v === undefined && name.charAt(0) === '$') v = host.dynamic(name);
      if (v === undefined) return m;
      v = storeString(v);
      if (depth < MAX_DEPTH && v.indexOf('{{') !== -1) return resolveTemplate(v, lookup, depth + 1);
      return v;
    });
  }

  // `replaceIn` of postman-collection also substitutes the strings inside
  // arrays and plain objects (on a copy).
  function replaceAll(template, lookup) {
    const seen = [];
    const walk = (v) => {
      if (typeof v === 'string') return resolveTemplate(v, lookup);
      if (!isObject(v) || seen.indexOf(v) !== -1) return v;
      const array = Array.isArray(v);
      const proto = getProto(v);
      if (!array && proto !== Object.prototype && proto !== null) return v;
      seen.push(v);
      let out;
      if (array) out = v.map(walk);
      else {
        out = {};
        for (const k of Object.keys(v)) setKey(out, k, walk(v[k]));
      }
      seen.pop();
      return out;
    };
    return walk(template);
  }

  // Variable types (postman-collection `Variable.types`). Rust sends every
  // value as its text plus, for the others than plain strings, a type:
  // `string`/`number`/`boolean` variables convert values set on them
  // (`Variable#castIn`); `any` text is the JSON of a value that is not a
  // string. Other variables keep whatever is set.
  const CASTS = { string: String, number: Number, boolean: Boolean };
  function decodeVar(text, type) {
    switch (type) {
      case 'number': return CASTS.number(text);
      case 'boolean': return text === 'true' ? true : text === 'false' ? false : text !== '';
      case 'any':
        if (text === 'NaN' || text === 'Infinity' || text === '-Infinity') return CASTS.number(text);
        try { return jsonParse(text); } catch (e) { return text; }
      default: return text;
    }
  }

  // `VariableScope#set(key, value, type)`: a known type given with the
  // value becomes the variable's type, then the value is converted by it.
  function setVar(scope, key, value, type) {
    if (typeof type === 'string') {
      const t = type.toLowerCase();
      if (t === 'any') scope._types.delete(key);
      else if (hasOwn(CASTS, t)) scope._types.set(key, t);
    }
    const t = scope._types.get(key);
    scope._values.set(key, t === undefined ? value : CASTS[t](value));
  }

  class VariableScope {
    constructor(values, opts) {
      const map = new Map(Object.entries(values || {}));
      const declared = new Map();
      const types = opts && opts.types;
      if (types) {
        for (const k of Object.keys(types)) {
          if (!map.has(k)) continue;
          const t = types[k];
          map.set(k, decodeVar(map.get(k), t));
          if (hasOwn(CASTS, t)) declared.set(k, t);
        }
      }
      define(this, '_values', map);
      define(this, '_types', declared);
      define(this, '_readonly', !!(opts && opts.readonly));
      if (opts && opts.name !== undefined) define(this, '_name', opts.name);
    }
    get name() { return this._name; }
    get(key) {
      host.check();
      key = String(key);
      return this._values.has(key) ? this._values.get(key) : undefined;
    }
    set(key, value, type) {
      host.check();
      if (this._readonly) throw new TypeError('this variable scope is read-only');
      if (key === undefined || key === null) return;
      setVar(this, String(key), value, type);
    }
    unset(key) {
      if (this._readonly) throw new TypeError('this variable scope is read-only');
      this._values.delete(String(key));
      this._types.delete(String(key));
    }
    has(key) { return this._values.has(String(key)); }
    clear() {
      if (this._readonly) throw new TypeError('this variable scope is read-only');
      this._values.clear();
      this._types.clear();
    }
    toObject() {
      const o = {};
      mapEach(this._values, (v, k) => setKey(o, k, v));
      return o;
    }
    toJSON() {
      const values = [];
      this._values.forEach((v, k) =>
        values.push({ key: k, value: v, type: this._types.get(k) || (typeof v === 'string' ? 'string' : 'any') }));
      return { values };
    }
    replaceIn(template) {
      return replaceAll(template, (n) => this._values.has(n) ? this._values.get(n) : undefined);
    }
    // postman-collection compatibility
    syncVariablesTo(target) {
      const o = target || {};
      this._values.forEach((v, k) => { o[k] = v; });
      return o;
    }
    get values() { return new PropertyList(Property, Array.from(this._values, ([key, value]) => ({ key, value }))); }
    _strings() {
      const o = {};
      mapEach(this._values, (v, k) => setKey(o, k, storeString(v)));
      return o;
    }
    // Types of the values for Rust (`VarType`), `undefined` when all are
    // plain strings.
    _typeNames() {
      const declared = {};
      mapEach(this._types, (t, k) => setKey(declared, k, t));
      let out;
      mapEach(this._values, (v, k) => {
        const tv = typeof v;
        const t = hasOwn(declared, k) ? declared[k] : tv === 'number' || tv === 'boolean' || tv === 'object' ? 'any' : undefined;
        if (t !== undefined) setKey(out || (out = {}), k, t);
      });
      return out;
    }
  }

  function lookupAll(name) {
    const sc = S.scopes;
    for (const scope of [sc.local, sc.data, sc.environment, sc.collection, sc.globals]) {
      if (scope._values.has(name)) return scope._values.get(name);
    }
    return undefined;
  }

  const variables = {
    get(key) { host.check(); return lookupAll(String(key)); },
    set(key, value, type) { host.check(); setVar(S.scopes.local, String(key), value, type); },
    has(key) {
      key = String(key);
      const sc = S.scopes;
      return [sc.local, sc.data, sc.environment, sc.collection, sc.globals].some((s) => s._values.has(key));
    },
    unset(key) { S.scopes.local._values.delete(String(key)); S.scopes.local._types.delete(String(key)); },
    clear() { S.scopes.local._values.clear(); S.scopes.local._types.clear(); },
    toObject() {
      const sc = S.scopes;
      const o = {};
      for (const scope of [sc.globals, sc.collection, sc.environment, sc.data, sc.local]) {
        scope._values.forEach((v, k) => setKey(o, k, v));
      }
      return o;
    },
    replaceIn(template) { return replaceAll(template, lookupAll); },
  };

  // ---------------------------------------------------------------------
  // Property / PropertyList (postman-collection like)
  // ---------------------------------------------------------------------
  class Property {
    constructor(o) {
      if (typeof o === 'string') o = { key: o };
      o = o || {};
      this.key = o.key === undefined ? null : o.key;
      this.value = o.value === undefined ? '' : o.value;
      if (o.disabled) this.disabled = true;
      if (o.description) this.description = o.description;
      if (o.type) this.type = o.type;
    }
    update(o) {
      if (!o) return;
      if ('key' in o) this.key = o.key;
      if ('value' in o) this.value = o.value;
      if ('disabled' in o) this.disabled = !!o.disabled;
      if ('description' in o) this.description = o.description;
    }
    valueOf() { return this.value; }
    toString() { return this.key + ': ' + this.value; }
    toJSON() {
      const o = { key: this.key, value: this.value };
      if (this.disabled) o.disabled = true;
      if (this.description) o.description = this.description;
      if (this.type) o.type = this.type;
      return o;
    }
  }

  class Header extends Property {
    constructor(o, name) {
      if (typeof o === 'string' && name !== undefined) o = { key: name, value: o };
      else if (typeof o === 'string') o = Header.parseSingle(o);
      super(o);
    }
    static parseSingle(str) {
      const idx = String(str).indexOf(':');
      if (idx === -1) return { key: String(str).trim(), value: '' };
      return { key: str.slice(0, idx).trim(), value: str.slice(idx + 1).trim() };
    }
    static parse(str) {
      return String(str).split(/\r?\n/).filter((l) => l.trim()).map((l) => Header.parseSingle(l));
    }
    static unparse(list, sep) {
      return (list.all ? list.all() : list).filter((h) => !h.disabled).map((h) => h.key + ': ' + h.value).join(sep || '\n');
    }
    static isHeader(o) { return o instanceof Header; }
  }

  class QueryParam extends Property {
    constructor(o) {
      if (typeof o === 'string') o = QueryParam.parseSingle(o);
      super(o);
      if (o && o.value === null) this.value = null;
    }
    static parseSingle(s) {
      const i = s.indexOf('=');
      return i === -1 ? { key: s, value: null } : { key: s.slice(0, i), value: s.slice(i + 1) };
    }
    static parse(q) {
      q = String(q || '');
      if (q.charAt(0) === '?') q = q.slice(1);
      return q ? q.split('&').filter((x) => x !== '').map(QueryParam.parseSingle) : [];
    }
    static unparseSingle(p) {
      if (p.key === null || p.key === undefined) return p.value === null || p.value === undefined ? '' : String(p.value);
      return p.value === null || p.value === undefined ? String(p.key) : p.key + '=' + p.value;
    }
    static unparse(list) {
      return (list.all ? list.all() : list).filter((p) => !p.disabled).map(QueryParam.unparseSingle).join('&');
    }
    toString() { return QueryParam.unparseSingle(this); }
  }

  class FormParam extends Property {
    constructor(o) {
      super(o);
      o = o || {};
      this.type = o.type || 'text';
      if (o.src !== undefined) this.src = o.src;
      if (o.contentType) this.contentType = o.contentType;
    }
    toJSON() {
      const j = super.toJSON();
      if (this.src !== undefined) j.src = this.src;
      if (this.contentType) j.contentType = this.contentType;
      return j;
    }
  }

  class Cookie {
    constructor(o) {
      o = o || {};
      this.name = o.name;
      this.value = o.value === undefined ? '' : o.value;
      this.domain = o.domain;
      this.path = o.path;
      this.secure = !!o.secure;
      this.httpOnly = !!(o.httpOnly || o.http_only);
      this.hostOnly = !!o.hostOnly;
      this.session = o.expires === null || o.expires === undefined;
      if (o.expires !== null && o.expires !== undefined) this.expires = typeof o.expires === 'number' ? new Date(o.expires * 1000) : new Date(o.expires);
      if (o.maxAge !== undefined) this.maxAge = o.maxAge;
    }
    get key() { return this.name; }
    valueOf() { return this.value; }
    toString() { return this.name + '=' + this.value; }
    toJSON() {
      const j = { name: this.name, value: this.value, domain: this.domain, path: this.path, secure: this.secure, httpOnly: this.httpOnly };
      if (this.expires) j.expires = this.expires.toISOString();
      return j;
    }
    static parse(str) {
      const parts = String(str).split(';').map((p) => p.trim()).filter(Boolean);
      const first = parts.shift() || '';
      const i = first.indexOf('=');
      const c = { name: i === -1 ? first : first.slice(0, i), value: i === -1 ? '' : first.slice(i + 1) };
      for (const p of parts) {
        const j = p.indexOf('=');
        const k = (j === -1 ? p : p.slice(0, j)).toLowerCase();
        const v = j === -1 ? true : p.slice(j + 1);
        if (k === 'domain') c.domain = v;
        else if (k === 'path') c.path = v;
        else if (k === 'secure') c.secure = true;
        else if (k === 'httponly') c.httpOnly = true;
        else if (k === 'expires') c.expires = v;
        else if (k === 'max-age') c.maxAge = Number(v);
      }
      return c;
    }
  }

  class PropertyList {
    constructor(Type, items, opts) {
      define(this, 'Type', Type || Property);
      define(this, '_ci', !!(opts && opts.caseInsensitive));
      define(this, 'members', []);
      if (items) this.populate(items);
    }
    _k(key) { return this._ci && typeof key === 'string' ? key.toLowerCase() : key; }
    _keyOf(m) { return m && (m.key !== undefined ? m.key : m.name); }
    _make(item) {
      if (item instanceof this.Type) return item;
      return new this.Type(item);
    }
    populate(items) {
      if (items instanceof PropertyList) items = items.all();
      if (typeof items === 'string' && this.Type === Header) items = Header.parse(items);
      if (typeof items === 'string' && this.Type === QueryParam) items = QueryParam.parse(items);
      if (!Array.isArray(items) && isObject(items)) items = Object.keys(items).map((k) => ({ key: k, value: items[k] }));
      if (Array.isArray(items)) items.forEach((i) => this.add(i));
    }
    repopulate(items) { this.clear(); this.populate(items); }
    add(item) {
      if (item === undefined || item === null) return;
      this.members.push(this._make(item));
    }
    append(item) { this.add(item); }
    prepend(item) {
      if (item === undefined || item === null) return;
      this.members.unshift(this._make(item));
    }
    insert(item, before) {
      const m = this._make(item);
      const idx = before === undefined ? -1 : this.members.findIndex((x) => x === before || this._k(this._keyOf(x)) === this._k(before));
      if (idx === -1) this.members.push(m);
      else this.members.splice(idx, 0, m);
    }
    insertAfter(item, after) {
      const m = this._make(item);
      const idx = this.members.findIndex((x) => x === after || this._k(this._keyOf(x)) === this._k(after));
      if (idx === -1) this.members.push(m);
      else this.members.splice(idx + 1, 0, m);
    }
    upsert(item) {
      if (item === undefined || item === null) return null;
      const m = this._make(item);
      const key = this._k(this._keyOf(m));
      const existing = this.members.find((x) => this._k(this._keyOf(x)) === key);
      if (existing) {
        if (typeof existing.update === 'function') existing.update(isObject(item) && !(item instanceof this.Type) ? item : m.toJSON ? m.toJSON() : m);
        else Object.assign(existing, m);
        return false;
      }
      this.members.push(m);
      return true;
    }
    remove(predicate, context) {
      let match;
      if (typeof predicate === 'function') match = (m) => predicate.call(context, m);
      else if (typeof predicate === 'string') {
        const k = this._k(predicate);
        match = (m) => this._k(this._keyOf(m)) === k;
      } else if (isObject(predicate)) {
        match = (m) => m === predicate || (this._keyOf(predicate) !== undefined && this._k(this._keyOf(m)) === this._k(this._keyOf(predicate)));
      } else return;
      for (let i = this.members.length - 1; i >= 0; i--) if (match(this.members[i])) this.members.splice(i, 1);
    }
    clear() { this.members.length = 0; }
    one(key) {
      const k = this._k(key);
      let found;
      for (const m of this.members) {
        if (this._k(this._keyOf(m)) === k) {
          if (!m.disabled) found = m;
          else if (found === undefined) found = m;
        }
      }
      return found;
    }
    get(key) {
      const m = this.one(key);
      return m === undefined || m.disabled ? undefined : m.valueOf();
    }
    has(key, value) {
      if (isObject(key)) return this.members.indexOf(key) !== -1;
      const k = this._k(key);
      return this.members.some((m) => this._k(this._keyOf(m)) === k && (value === undefined || m.valueOf() === value));
    }
    idx(i) { return this.members[i]; }
    indexOf(item) {
      if (typeof item === 'string') return this.members.findIndex((m) => this._k(this._keyOf(m)) === this._k(item));
      return this.members.indexOf(item);
    }
    count() { return this.members.length; }
    get length() { return this.members.length; }
    all() { return this.members.slice(); }
    each(fn, context) { this.members.forEach((m, i) => fn.call(context, m, i)); }
    forEach(fn, context) { this.each(fn, context); }
    map(fn, context) { return this.members.map((m, i) => fn.call(context, m, i)); }
    filter(fn, context) { return this.members.filter((m, i) => fn.call(context, m, i)); }
    find(fn, context) { return this.members.find((m, i) => fn.call(context, m, i)); }
    reduce(fn, acc, context) { return this.members.reduce((a, m, i) => fn.call(context, a, m, i), acc); }
    toObject(excludeDisabled, caseSensitive, multiValue) {
      const o = {};
      for (const m of this.members) {
        if (excludeDisabled && m.disabled) continue;
        let key = this._keyOf(m);
        if (key === null || key === undefined) continue;
        if (this._ci && !caseSensitive) key = String(key).toLowerCase();
        const v = m.valueOf();
        if (multiValue && hasOwn(o, key)) {
          if (!Array.isArray(o[key])) o[key] = [o[key]];
          o[key].push(v);
        } else o[key] = v;
      }
      return o;
    }
    toJSON() { return this.members.map((m) => (m.toJSON ? m.toJSON() : m)); }
    toString() {
      if (this.Type === Header) return Header.unparse(this);
      if (this.Type === QueryParam) return QueryParam.unparse(this);
      return this.members.map(String).join('\n');
    }
    assimilate(source, prune) {
      const items = source instanceof PropertyList ? source.all() : source;
      if (prune) this.clear();
      (items || []).forEach((i) => this.upsert(i));
    }
    eachParent() {}
    [Symbol.iterator]() { return this.members[Symbol.iterator](); }
    static isPropertyList(o) { return o instanceof PropertyList; }
  }

  // ---------------------------------------------------------------------
  // Url
  // ---------------------------------------------------------------------
  class Url {
    constructor(options) {
      this.update(options);
    }
    update(url) {
      const o = typeof url === 'string' || url === undefined || url === null ? Url.parse(String(url || '')) : url;
      this.protocol = o.protocol || undefined;
      this.auth = o.auth || undefined;
      this.host = Array.isArray(o.host) ? o.host.slice() : (o.host ? String(o.host).split('.') : undefined);
      this.port = o.port === undefined || o.port === null || o.port === '' ? undefined : String(o.port);
      this.path = Array.isArray(o.path) ? o.path.slice() : (o.path ? String(o.path).replace(/^\//, '').split('/') : undefined);
      this.hash = o.hash || undefined;
      const q = o.query instanceof PropertyList ? o.query.all() : o.query;
      this.query = new PropertyList(QueryParam, q || []);
      const vars = o.variable || o.variables;
      this.variables = new PropertyList(Property, vars instanceof PropertyList ? vars.all() : vars || []);
    }
    static parse(raw) {
      let s = String(raw).trim();
      const out = { raw: s };
      let i = s.indexOf('#');
      if (i !== -1) { out.hash = s.slice(i + 1); s = s.slice(0, i); }
      i = queryIndex(s);
      if (i !== -1) { out.query = QueryParam.parse(s.slice(i + 1)); s = s.slice(0, i); }
      const pm = /^([^:/?#{}]+):\/\//.exec(s);
      if (pm) { out.protocol = pm[1]; s = s.slice(pm[0].length); }
      // authority ends at the first "/" that is not inside {{ }}
      let end = s.length;
      for (let j = 0, depth = 0; j < s.length; j++) {
        if (s[j] === '{' && s[j + 1] === '{') { depth++; j++; continue; }
        if (s[j] === '}' && s[j + 1] === '}' && depth) { depth--; j++; continue; }
        if (s[j] === '/' && !depth) { end = j; break; }
      }
      let authority = s.slice(0, end);
      const path = s.slice(end);
      const at = authority.lastIndexOf('@');
      if (at !== -1) {
        const cred = authority.slice(0, at);
        authority = authority.slice(at + 1);
        const c = cred.indexOf(':');
        out.auth = c === -1 ? { user: cred } : { user: cred.slice(0, c), password: cred.slice(c + 1) };
      }
      const pi = authority.lastIndexOf(':');
      if (pi !== -1) {
        const hostPart = authority.slice(0, pi);
        const portPart = authority.slice(pi + 1);
        const insideVar = hostPart.lastIndexOf('{{') > hostPart.lastIndexOf('}}');
        const ipv6NoPort = authority.charAt(authority.length - 1) === ']';
        if (!insideVar && !ipv6NoPort && /^(\d+|\{\{[^{}]+\}\})$/.test(portPart)) {
          out.port = portPart;
          authority = hostPart;
        }
      }
      out.host = authority ? (authority.indexOf('{{') !== -1 ? [authority] : authority.split('.')) : undefined;
      if (path) out.path = path.slice(1).split('/');
      return out;
    }
    getHost() { return this.host ? this.host.join('.') : ''; }
    getPath(unresolved) {
      if (!this.path) return '/';
      let segs = this.path;
      if (!unresolved) {
        segs = segs.map((seg) => {
          if (typeof seg === 'string' && seg.charAt(0) === ':') {
            const v = this.variables.one(seg.slice(1));
            if (v && v.value !== undefined && v.value !== null && v.value !== '') return String(v.value);
          }
          return seg;
        });
      }
      return '/' + segs.join('/');
    }
    getQueryString(options) {
      const ignoreDisabled = !(options && options.ignoreDisabled === false);
      return this.query.members.filter((p) => !(ignoreDisabled && p.disabled)).map(QueryParam.unparseSingle).join('&');
    }
    getPathWithQuery(unresolved) {
      const q = this.getQueryString();
      return this.getPath(unresolved) + (q ? '?' + q : '');
    }
    getRemote(options) {
      const forcePort = typeof options === 'boolean' ? options : options && options.forcePort;
      let port = this.port;
      if (!port && forcePort) port = this.protocol === 'https' ? '443' : '80';
      return this.getHost() + (port ? ':' + port : '');
    }
    getRaw() { return this.toString(); }
    getOAuth1BaseUrl() {
      const protocol = (this.protocol || 'http').toLowerCase();
      let port = this.port;
      if ((protocol === 'http' && port === '80') || (protocol === 'https' && port === '443')) port = undefined;
      return protocol + '://' + this.getHost().toLowerCase() + (port ? ':' + port : '') + this.getPath(true);
    }
    addQueryParams(params) {
      if (typeof params === 'string') params = QueryParam.parse(params);
      else if (!Array.isArray(params) && isObject(params)) params = Object.keys(params).map((k) => ({ key: k, value: params[k] }));
      (params || []).forEach((p) => this.query.add(p));
    }
    removeQueryParams(params) {
      if (!Array.isArray(params)) params = [params];
      params.forEach((p) => this.query.remove(typeof p === 'string' ? p : p && p.key));
    }
    toString(forceProtocol) {
      let s = '';
      if (this.protocol) s += this.protocol + '://';
      else if (forceProtocol) s += 'http://';
      if (this.auth && (this.auth.user || this.auth.password)) {
        s += (this.auth.user || '') + (this.auth.password !== undefined ? ':' + this.auth.password : '') + '@';
      }
      s += this.getHost();
      if (this.port !== undefined) s += ':' + this.port;
      if (this.path) s += '/' + this.path.join('/');
      const q = this.getQueryString();
      if (q || (this.query.count() && this.query.members.some((m) => !m.disabled))) s += '?' + q;
      if (this.hash) s += '#' + this.hash;
      return s;
    }
    toJSON() {
      const o = { protocol: this.protocol, host: this.host, port: this.port, path: this.path, query: this.query.toJSON(), variable: this.variables.toJSON() };
      if (this.hash) o.hash = this.hash;
      if (this.auth) o.auth = this.auth;
      return o;
    }
    clone() { return new Url(this.toString()); }
    static isUrl(o) { return o instanceof Url; }
  }

  function queryIndex(s) {
    for (let j = 0, depth = 0; j < s.length; j++) {
      if (s[j] === '{' && s[j + 1] === '{') { depth++; j++; continue; }
      if (s[j] === '}' && s[j + 1] === '}' && depth) { depth--; j++; continue; }
      if (s[j] === '?' && !depth) return j;
    }
    return -1;
  }

  // ---------------------------------------------------------------------
  // Request body / Request / Response
  // ---------------------------------------------------------------------
  class RequestBody {
    constructor(o) { this.update(o || {}); }
    update(o) {
      if (typeof o === 'string') o = { mode: 'raw', raw: o };
      this.mode = o.mode;
      if ('raw' in o) this.raw = o.raw === undefined || o.raw === null ? '' : (typeof o.raw === 'string' ? o.raw : JSON.stringify(o.raw));
      else if (o.mode === 'raw' && this.raw === undefined) this.raw = '';
      if (o.urlencoded !== undefined || o.mode === 'urlencoded') this.urlencoded = new PropertyList(QueryParam, o.urlencoded instanceof PropertyList ? o.urlencoded.all() : o.urlencoded || []);
      if (o.formdata !== undefined || o.mode === 'formdata') this.formdata = new PropertyList(FormParam, o.formdata instanceof PropertyList ? o.formdata.all() : o.formdata || []);
      if (o.file !== undefined) this.file = typeof o.file === 'string' ? { src: o.file } : o.file;
      if (o.graphql !== undefined) this.graphql = o.graphql;
      if (o.options !== undefined) this.options = o.options;
      if (o.disabled !== undefined) this.disabled = o.disabled;
    }
    isEmpty() {
      switch (this.mode) {
        case 'raw': return !this.raw;
        case 'urlencoded': return !this.urlencoded || !this.urlencoded.count();
        case 'formdata': return !this.formdata || !this.formdata.count();
        case 'file': return !this.file || !this.file.src;
        case 'graphql': return !this.graphql || !this.graphql.query;
        default: return true;
      }
    }
    toString() {
      switch (this.mode) {
        case 'raw': return this.raw || '';
        case 'urlencoded': return this.urlencoded ? QueryParam.unparse(this.urlencoded) : '';
        case 'graphql': return JSON.stringify(this.graphql || {});
        case 'file': return (this.file && this.file.src) || '';
        default: return '';
      }
    }
    toJSON() {
      const o = { mode: this.mode };
      if (this.raw !== undefined) o.raw = this.raw;
      if (this.urlencoded) o.urlencoded = this.urlencoded.toJSON();
      if (this.formdata) o.formdata = this.formdata.toJSON();
      if (this.file) o.file = this.file;
      if (this.graphql) o.graphql = this.graphql;
      if (this.options) o.options = this.options;
      if (this.disabled) o.disabled = true;
      return o;
    }
  }

  const MODE_TO_PM = { raw: 'raw', urlencoded: 'urlencoded', formdata: 'formdata', binary: 'file', graphql: 'graphql' };
  const MODE_FROM_PM = { raw: 'raw', urlencoded: 'urlencoded', formdata: 'formdata', file: 'binary', graphql: 'graphql' };

  function bodyFromCore(b) {
    if (!b || !b.mode || b.mode === 'none') return undefined;
    const mode = MODE_TO_PM[b.mode];
    const o = { mode };
    if (mode === 'raw') {
      o.raw = b.raw || '';
      o.options = { raw: { language: b.raw_language || 'json' } };
    } else if (mode === 'urlencoded') {
      o.urlencoded = (b.urlencoded || []).map((f) => ({ key: f.key, value: f.value, disabled: f.enabled === false, description: f.description }));
    } else if (mode === 'formdata') {
      o.formdata = (b.form_data || []).map((f) => {
        const file = f.kind === 'file';
        const r = { key: f.key, type: file ? 'file' : 'text', disabled: f.enabled === false, description: f.description };
        if (file) r.src = f.files && f.files.length === 1 ? f.files[0] : f.files || [];
        else r.value = f.value || '';
        if (f.content_type) r.contentType = f.content_type;
        return r;
      });
    } else if (mode === 'file') {
      o.file = { src: b.binary_path || '' };
    } else if (mode === 'graphql') {
      const g = b.graphql || {};
      o.graphql = { query: g.query || '', variables: g.variables || '' };
      if (g.operation_name) o.graphql.operationName = g.operation_name;
    }
    return new RequestBody(o);
  }

  function kvToCore(list) {
    return (list ? list.all() : []).map((p) => {
      const o = { key: p.key === null || p.key === undefined ? '' : String(p.key), value: p.value === null || p.value === undefined ? '' : storeString(p.value) };
      if (p.disabled) o.enabled = false;
      if (p.description) o.description = typeof p.description === 'string' ? p.description : String(p.description.content || '');
      return o;
    });
  }

  function bodyToCore(body, original) {
    const b = Object.assign({}, original || {});
    if (!body || !body.mode) {
      b.mode = 'none';
      return b;
    }
    b.mode = MODE_FROM_PM[body.mode] || 'none';
    if (body.mode === 'raw') {
      b.raw = body.raw === undefined || body.raw === null ? '' : String(body.raw);
      const lang = body.options && body.options.raw && body.options.raw.language;
      if (lang && ['json', 'text', 'xml', 'html', 'javascript'].indexOf(lang) !== -1) b.raw_language = lang;
    } else if (body.mode === 'urlencoded') {
      b.urlencoded = kvToCore(body.urlencoded);
    } else if (body.mode === 'formdata') {
      b.form_data = (body.formdata ? body.formdata.all() : []).map((p) => {
        const file = p.type === 'file';
        const o = { key: String(p.key === null || p.key === undefined ? '' : p.key) };
        if (file) {
          o.kind = 'file';
          o.files = Array.isArray(p.src) ? p.src.map(String) : (p.src ? [String(p.src)] : []);
        } else o.value = storeString(p.value);
        if (p.contentType) o.content_type = p.contentType;
        if (p.disabled) o.enabled = false;
        if (p.description) o.description = String(p.description);
        return o;
      });
    } else if (body.mode === 'file') {
      b.binary_path = (body.file && body.file.src) || '';
    } else if (body.mode === 'graphql') {
      const g = body.graphql || {};
      b.graphql = {
        query: String(g.query || ''),
        variables: typeof g.variables === 'string' ? g.variables : (g.variables ? JSON.stringify(g.variables) : ''),
      };
      if (g.operationName) b.graphql.operation_name = String(g.operationName);
    }
    return b;
  }

  // Lists a script replaced with a plain array, object or string
  // (`pm.request.headers = [...]`) become property lists again, as
  // postman-collection rebuilds them from the request JSON.
  function asList(v, Type, opts) {
    return v instanceof PropertyList ? v : new PropertyList(Type, v === undefined || v === null ? [] : v, opts);
  }
  function normalizeLists(req) {
    req.headers = asList(req.headers, Header, { caseInsensitive: true });
    if (req.url && !(req.url.query instanceof PropertyList)) req.url.query = asList(req.url.query, QueryParam);
    const b = req.body;
    if (b) {
      if (b.urlencoded !== undefined && !(b.urlencoded instanceof PropertyList)) b.urlencoded = asList(b.urlencoded, QueryParam);
      if (b.formdata !== undefined && !(b.formdata instanceof PropertyList)) b.formdata = asList(b.formdata, FormParam);
    }
  }

  class Request {
    constructor(o) {
      o = typeof o === 'string' ? { url: o } : o || {};
      define(this, '_url', null);
      define(this, '_body', undefined);
      this.id = o.id;
      this.name = o.name;
      this.method = String(o.method || 'GET').toUpperCase();
      this.url = o.url || '';
      this.headers = new PropertyList(Header, o.header || o.headers || [], { caseInsensitive: true });
      this.body = o.body;
      if (o.auth) this.auth = o.auth;
      if (o.description) this.description = o.description;
    }
    get url() { return this._url; }
    set url(v) { this._url = v instanceof Url ? v : new Url(typeof v === 'string' ? v : (isObject(v) && v.raw && !v.host ? v.raw : v)); }
    get body() { return this._body; }
    set body(v) { this._body = v === undefined || v === null ? undefined : (v instanceof RequestBody ? v : new RequestBody(v)); }
    getHeaders(options) {
      const o = {};
      const ignoreCase = options && options.ignoreCase;
      const enabled = options && options.enabled;
      for (const h of this.headers.members) {
        if (enabled && h.disabled) continue;
        const k = ignoreCase ? String(h.key).toLowerCase() : h.key;
        if (hasOwn(o, k)) o[k] = Array.isArray(o[k]) ? o[k].concat(h.value) : [o[k], h.value];
        else o[k] = h.value;
      }
      return o;
    }
    forEachHeader(fn, context) { this.headers.each(fn, context); }
    addHeader(h) { this.headers.add(h); }
    upsertHeader(h) { this.headers.upsert(h); }
    removeHeader(name, options) {
      const ignoreCase = !options || options.ignoreCase !== false;
      const n = isObject(name) ? name.key : name;
      this.headers.remove((h) => ignoreCase ? String(h.key).toLowerCase() === String(n).toLowerCase() : h.key === n);
    }
    addQueryParams(p) { this.url.addQueryParams(p); }
    removeQueryParams(p) { this.url.removeQueryParams(p); }
    update(o) {
      if (!o) return;
      if (o.method) this.method = String(o.method).toUpperCase();
      if (o.url) this.url = o.url;
      if (o.header || o.headers) this.headers.repopulate(o.header || o.headers);
      if ('body' in o) this.body = o.body;
    }
    size() {
      const body = this.body ? this.body.toString().length : 0;
      return { body, header: Header.unparse(this.headers).length, total: body };
    }
    clone() { return new Request(this.toJSON()); }
    toJSON() {
      const o = { method: this.method, url: this.url.toString(), header: this.headers.toJSON() };
      if (this.id) o.id = this.id;
      if (this.name) o.name = this.name;
      if (this.body) o.body = this.body.toJSON();
      if (this.auth) o.auth = this.auth;
      return o;
    }
    get to() { return chai().expect(this).to; }
    static isRequest(o) { return o instanceof Request; }
  }

  const REASONS = {
    100: 'Continue', 101: 'Switching Protocols', 200: 'OK', 201: 'Created', 202: 'Accepted', 203: 'Non-Authoritative Information',
    204: 'No Content', 205: 'Reset Content', 206: 'Partial Content', 301: 'Moved Permanently', 302: 'Found', 303: 'See Other',
    304: 'Not Modified', 307: 'Temporary Redirect', 308: 'Permanent Redirect', 400: 'Bad Request', 401: 'Unauthorized',
    402: 'Payment Required', 403: 'Forbidden', 404: 'Not Found', 405: 'Method Not Allowed', 406: 'Not Acceptable',
    408: 'Request Timeout', 409: 'Conflict', 410: 'Gone', 411: 'Length Required', 412: 'Precondition Failed',
    413: 'Payload Too Large', 414: 'URI Too Long', 415: 'Unsupported Media Type', 418: "I'm a teapot",
    422: 'Unprocessable Entity', 423: 'Locked', 425: 'Too Early', 426: 'Upgrade Required', 428: 'Precondition Required',
    429: 'Too Many Requests', 431: 'Request Header Fields Too Large', 451: 'Unavailable For Legal Reasons',
    500: 'Internal Server Error', 501: 'Not Implemented', 502: 'Bad Gateway', 503: 'Service Unavailable',
    504: 'Gateway Timeout', 505: 'HTTP Version Not Supported',
  };

  class Response {
    constructor(o, bodyLoader) {
      o = o || {};
      this.id = o.id;
      this.code = Number(o.code) || 0;
      this.status = o.status || REASONS[this.code] || '';
      const hs = (o.headers || o.header || []).map((h) => Array.isArray(h) ? { key: h[0], value: h[1] } : h);
      this.headers = new PropertyList(Header, hs, { caseInsensitive: true });
      this.responseTime = o.responseTime === undefined ? 0 : Math.round(Number(o.responseTime) || 0);
      this.responseSize = Number(o.responseSize) || 0;
      define(this, '_loader', bodyLoader || (() => (o.body === undefined ? '' : String(o.body))));
      define(this, '_text', undefined);
      define(this, '_cookies', undefined);
    }
    text() {
      if (this._text === undefined) this._text = this._loader();
      return this._text;
    }
    json(reviver, strict) {
      let t = this.text();
      if (t.charCodeAt(0) === 0xfeff) t = t.slice(1);
      try {
        return JSON.parse(t, reviver);
      } catch (e) {
        const err = new SyntaxError((e && e.message) || 'Unexpected token in JSON');
        err.name = 'JSONError';
        throw err;
      }
    }
    reason() { return this.status; }
    get body() { return this.text(); }
    get cookies() {
      if (!this._cookies) {
        const list = new PropertyList(Cookie, []);
        this.headers.members.filter((h) => String(h.key).toLowerCase() === 'set-cookie').forEach((h) => list.add(Cookie.parse(h.value)));
        this._cookies = list;
      }
      return this._cookies;
    }
    size() {
      const header = this.headers.members.reduce((n, h) => n + String(h.key).length + String(h.value).length + 4, 0);
      const body = this.responseSize ? Math.max(this.responseSize - header, 0) : this.text().length;
      return { body, header, total: body + header };
    }
    contentInfo() {
      const ct = String(this.headers.get('content-type') || '');
      const mime = ct.split(';')[0].trim() || 'text/plain';
      const cs = /charset=([^;]+)/i.exec(ct);
      const sub = mime.split('/')[1] || 'plain';
      return { mimeType: mime.split('/')[0], mimeFormat: sub.indexOf('json') !== -1 ? 'json' : sub, charset: cs ? cs[1].trim() : 'utf8', fileExtension: sub, fileName: 'response.' + sub };
    }
    dataURI() {
      const ct = String(this.headers.get('content-type') || 'text/plain').split(';')[0];
      return 'data:' + ct + ';base64,' + (this === (S && S.response) ? host.bodyBase64() : btoa(unescape(encodeURIComponent(this.text()))));
    }
    get stream() {
      if (this === (S && S.response)) return Buffer.from(host.bodyBase64(), 'base64');
      return Buffer.from(this.text(), 'utf8');
    }
    toJSON() {
      return { code: this.code, status: this.status, header: this.headers.toJSON(), body: this.text(), responseTime: this.responseTime, responseSize: this.responseSize };
    }
    get to() { return chai().expect(this).to; }
    static isResponse(o) { return o instanceof Response; }
  }

  // ---------------------------------------------------------------------
  // Non-HTTP protocols: pm.request / pm.response / pm.message built from
  // the job's protocol JSON (shapes in docs/scripting.md).
  // ---------------------------------------------------------------------
  // Key/value lists that become PropertyLists (get/has/add/upsert/remove/toObject).
  const KV_KEYS = ['headers', 'metadata', 'params', 'env', 'trailers'];
  // Method names a protocol object keeps even if the JSON has such fields.
  const PROTOCOL_RESERVED = ['text', 'json', 'to', 'toJSON'];

  function kvList(items) {
    const list = (items || []).map((h) => (Array.isArray(h)
      ? { key: h[0], value: h[1] }
      : { key: h.key, value: h.value, disabled: h.enabled === false || h.disabled === true, description: h.description }));
    return new PropertyList(Header, list, { caseInsensitive: true });
  }

  function parseJsonText(text) {
    let t = String(text);
    if (t.charCodeAt(0) === 0xfeff) t = t.slice(1);
    // Model answers often wrap JSON in a Markdown code fence.
    const fenced = /^\s*```[a-zA-Z]*\s*\n([\s\S]*?)\n?```\s*$/.exec(t);
    if (fenced) t = fenced[1];
    try {
      return JSON.parse(t);
    } catch (e) {
      const err = new SyntaxError((e && e.message) || 'Unexpected token in JSON');
      err.name = 'JSONError';
      throw err;
    }
  }

  function plainValue(v) {
    if (v instanceof PropertyList) return kvToCore(v);
    if (v instanceof MessageList) return Array.prototype.map.call(v, plainValue);
    if (v instanceof ProtocolMessage) return v.toJSON();
    return v;
  }

  function copyFields(target, o, convert) {
    for (const k of Object.keys(o)) {
      if (PROTOCOL_RESERVED.indexOf(k) !== -1) continue;
      target[k] = convert(k, o[k]);
    }
  }

  function ownJson(obj) {
    const o = {};
    for (const k of Object.keys(obj)) o[k] = plainValue(obj[k]);
    return o;
  }

  class ProtocolMessage {
    constructor(o) {
      o = isObject(o) && !Array.isArray(o) ? o : { data: o };
      copyFields(this, o, (k, v) => (KV_KEYS.indexOf(k) !== -1 && Array.isArray(v) ? kvList(v) : v));
      if (!('data' in o)) {
        if ('payload' in o) define(this, 'data', o.payload);
        else if ('args' in o) define(this, 'data', o.args);
      }
    }
    text() {
      const d = this.data;
      if (typeof d === 'string') return d;
      return d === undefined || d === null ? '' : JSON.stringify(d);
    }
    json() {
      const d = this.data;
      return typeof d === 'string' ? parseJsonText(d) : d;
    }
    toJSON() { return ownJson(this); }
    get to() { return chai().expect(this).to; }
  }

  // An array of messages with the postman-collection list helpers.
  class MessageList extends Array {
    static from_(items) {
      const l = new MessageList();
      (items || []).forEach((m) => l.add(m));
      return l;
    }
    static get [Symbol.species]() { return Array; }
    idx(i) { return this[i]; }
    count() { return this.length; }
    all() { return Array.prototype.slice.call(this); }
    each(fn, context) { Array.prototype.forEach.call(this, (m, i) => fn.call(context, m, i)); }
    add(m) { this.push(m instanceof ProtocolMessage ? m : new ProtocolMessage(m)); }
    append(m) { this.add(m); }
    clear() { this.length = 0; }
    remove(predicate) {
      for (let i = this.length - 1; i >= 0; i--) {
        if (typeof predicate === 'function' ? predicate(this[i], i) : this[i] === predicate) this.splice(i, 1);
      }
    }
    toJSON() { return Array.prototype.map.call(this, plainValue); }
  }

  class ProtocolRequest {
    constructor(kind, o) {
      define(this, '_kind', kind);
      o = isObject(o) ? o : {};
      copyFields(this, o, (k, v) => {
        if (KV_KEYS.indexOf(k) !== -1 && Array.isArray(v)) return kvList(v);
        if (k === 'messages' && kind === 'grpc' && Array.isArray(v)) return MessageList.from_(v);
        return v;
      });
    }
    get protocol() { return this._kind; }
    toJSON() { return ownJson(this); }
    toString() { return String(this.url === undefined ? '' : this.url); }
    get to() { return chai().expect(this).to; }
  }

  class ProtocolResponse {
    constructor(kind, o) {
      define(this, '_kind', kind);
      o = isObject(o) ? o : {};
      copyFields(this, o, (k, v) => {
        if (KV_KEYS.indexOf(k) !== -1 && Array.isArray(v)) return kvList(v);
        if (k === 'messages' && Array.isArray(v)) return MessageList.from_(v);
        return v;
      });
      if (kind === 'grpc') {
        if (this.statusCode === undefined) this.statusCode = 0;
        if (!(this.metadata instanceof PropertyList)) this.metadata = kvList([]);
        if (!(this.trailers instanceof PropertyList)) this.trailers = kvList([]);
      }
      if (!(this.messages instanceof MessageList)) this.messages = MessageList.from_([]);
      this.responseTime = Number(o.responseTime) || 0;
      if (!('code' in this) && this.statusCode !== undefined) define(this, 'code', Number(this.statusCode));
      if (!('status' in this)) define(this, 'status', o.statusMessage !== undefined ? String(o.statusMessage) : '');
      if (!('headers' in this)) define(this, 'headers', this.metadata instanceof PropertyList ? this.metadata : kvList([]));
    }
    get protocol() { return this._kind; }
    // `to.be.ok`: gRPC status 0, MCP without isError, otherwise 2xx (or no status).
    isOk() {
      if (this._kind === 'grpc') return Number(this.statusCode) === 0;
      if (this._kind === 'mcp') return !this.isError && !this.error;
      if (this.statusCode !== undefined) return Number(this.statusCode) >= 200 && Number(this.statusCode) < 300;
      return !this.error;
    }
    text() {
      if (typeof this.body === 'string') return this.body;
      if (typeof this.content === 'string') return this.content;
      if (this.result !== undefined) {
        const r = this.result;
        if (typeof r === 'string') return r;
        const items = r && Array.isArray(r.content) ? r.content : null;
        if (items && items.length && items.every((c) => c && c.type === 'text')) return items.map((c) => c.text).join('\n');
        return JSON.stringify(r);
      }
      if (this.data !== undefined) return typeof this.data === 'string' ? this.data : JSON.stringify(this.data);
      if (this.messages.length) return this.messages[0] instanceof ProtocolMessage ? this.messages[0].text() : JSON.stringify(this.messages[0]);
      return '';
    }
    json() {
      if (this.result !== undefined && typeof this.result !== 'string') return this.result;
      if (this.data !== undefined && typeof this.data !== 'string') return this.data;
      const hasText = typeof this.body === 'string' || typeof this.content === 'string' || this.result !== undefined || this.data !== undefined;
      if (!hasText && this.messages.length) return this.messages[0] instanceof ProtocolMessage ? this.messages[0].json() : this.messages[0];
      return parseJsonText(this.text());
    }
    toJSON() { return ownJson(this); }
    get to() { return chai().expect(this).to; }
  }

  // ---------------------------------------------------------------------
  // Assertions (chai is loaded lazily)
  // ---------------------------------------------------------------------
  let chaiInstance = null;
  // js/chai_postman.js, captured by `__mp_snapshot`.
  let installChaiPostman = null;
  function chai() {
    if (!chaiInstance) {
      const c = loadModule('chai');
      c.config.truncateThreshold = 0;
      if (typeof installChaiPostman === 'function') {
        installChaiPostman(c, { Response, Request, ProtocolResponse, ProtocolRequest, ProtocolMessage, tv4: () => loadModule('tv4') });
      }
      trackLibrary('chai', [c, c.Assertion, c.config, c.expect]);
      chaiInstance = c;
    }
    markUsed('chai');
    return chaiInstance;
  }

  // ---------------------------------------------------------------------
  // Modules
  // ---------------------------------------------------------------------
  const moduleCache = Object.create(null);
  const TRACKED_LIBRARIES = ['lodash', 'moment', 'crypto-js', 'tv4'];
  function loadModule(name) {
    if (!(name in moduleCache)) {
      // Libraries probe globals such as `root._` while loading; the
      // placeholder stops the lazy global getters from recursing.
      moduleCache[name] = undefined;
      try {
        moduleCache[name] = host.load(name);
      } catch (e) {
        delete moduleCache[name];
        throw e;
      }
      // `_.mixin(...)`, `CryptoJS.HmacSHA256 = ...` and the like are undone
      // after jobs that used the library (top-level properties only).
      const m = moduleCache[name];
      if (TRACKED_LIBRARIES.indexOf(name) !== -1 && m) trackLibrary(name, [m]);
    }
    markUsed(name);
    return moduleCache[name];
  }

  const NODE_BUILTINS = ['fs', 'child_process', 'net', 'http', 'https', 'http2', 'os', 'dns', 'tls', 'dgram', 'cluster',
    'worker_threads', 'vm', 'process', 'readline', 'repl', 'module', 'v8', 'inspector', 'perf_hooks', 'zlib', 'crypto',
    'stream', 'string_decoder', 'async_hooks', 'diagnostics_channel', 'fs/promises', 'tty', 'domain', 'punycode'];
  const UNSUPPORTED = ['cheerio', 'csv-parse', 'csv-parse/lib/sync', 'postman-collection', 'chai-json-schema'];

  function makeUuid() {
    const b = host.random(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const h = b.map((x) => (x + 0x100).toString(16).slice(1));
    return h.slice(0, 4).join('') + '-' + h.slice(4, 6).join('') + '-' + h.slice(6, 8).join('') + '-' + h.slice(8, 10).join('') + '-' + h.slice(10).join('');
  }
  const uuidModule = function () { return makeUuid(); };
  uuidModule.v4 = makeUuid;
  uuidModule.v1 = function () {
    const t = Date.now().toString(16).padStart(12, '0');
    const r = makeUuid();
    return t.slice(4, 12) + '-' + t.slice(0, 4) + '-1' + r.slice(15, 18) + r.slice(18);
  };
  uuidModule.NIL = '00000000-0000-0000-0000-000000000000';
  uuidModule.validate = (s) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(String(s));
  uuidModule.version = (s) => parseInt(String(s).charAt(14), 16);

  function makeAjv() {
    function Ajv(opts) {
      if (!(this instanceof Ajv)) return new Ajv(opts);
      this.opts = opts || {};
      this.errors = null;
      this._schemas = {};
    }
    function convert(res) {
      return (res.errors || []).map((e) => ({
        instancePath: e.dataPath || '', dataPath: e.dataPath || '', schemaPath: e.schemaPath || '',
        keyword: (e.schemaPath || '').split('/').pop() || 'schema', params: e.params || {}, message: e.message,
      }));
    }
    Ajv.prototype.compile = function (schema) {
      const self = this;
      const validate = function (data) {
        const res = loadModule('tv4').validateMultiple(data, schema, false, false);
        validate.errors = res.valid ? null : convert(res);
        self.errors = validate.errors;
        return res.valid;
      };
      validate.schema = schema;
      validate.errors = null;
      return validate;
    };
    Ajv.prototype.validate = function (schema, data) {
      if (typeof schema === 'string') schema = this._schemas[schema];
      return this.compile(schema)(data);
    };
    Ajv.prototype.addSchema = function (schema, key) {
      this._schemas[key || schema.$id || schema.id] = schema;
      loadModule('tv4').addSchema(key || schema.$id || schema.id, schema);
      return this;
    };
    Ajv.prototype.getSchema = function (key) { return this._schemas[key] ? this.compile(this._schemas[key]) : undefined; };
    Ajv.prototype.addFormat = function () { return this; };
    Ajv.prototype.addKeyword = function () { return this; };
    Ajv.prototype.errorsText = function (errors) {
      errors = errors || this.errors;
      return errors && errors.length ? errors.map((e) => 'data' + e.instancePath + ' ' + e.message).join(', ') : 'No errors';
    };
    Ajv.default = Ajv;
    Ajv.Ajv = Ajv;
    return Ajv;
  }

  const builtinModules = {
    chai: () => chai(),
    lodash: () => loadModule('lodash'),
    moment: () => loadModule('moment'),
    'crypto-js': () => loadModule('crypto-js'),
    tv4: () => loadModule('tv4'),
    ajv: () => makeAjv(),
    uuid: () => uuidModule,
    'uuid/v4': () => makeUuid,
    'uuid/v1': () => uuidModule.v1,
    atob: () => G.atob,
    btoa: () => G.btoa,
    xml2js: () => xml2js,
    buffer: () => ({ Buffer: Buffer }),
    url: () => ({ URL: G.URL, URLSearchParams: G.URLSearchParams, parse: (u) => new G.URL(u), format: (u) => String(u) }),
    querystring: () => querystring,
    util: () => utilModule,
    events: () => ({ EventEmitter, default: EventEmitter }),
    timers: () => ({ setTimeout: G.setTimeout, clearTimeout: G.clearTimeout, setInterval: G.setInterval, clearInterval: G.clearInterval, setImmediate: G.setImmediate, clearImmediate: G.clearImmediate }),
    assert: () => assertModule,
    path: undefined,
  };

  // ---------------------------------------------------------------------
  // Packages: npm/JSR modules (namespaces kept per worker) and Package
  // Library files (loaded fresh per job), resolved by the host.
  // ---------------------------------------------------------------------
  const remotePkgs = Object.create(null);

  // What `require` returns for an ES module: its default export when that
  // is all there is or when the default already carries the named exports
  // (CommonJS-style packages), otherwise the namespace.
  function interop(ns) {
    if (!isObject(ns)) return ns;
    if (!('default' in ns)) return ns;
    const names = Object.keys(ns).filter((k) => k !== 'default' && k !== '__esModule');
    const d = ns.default;
    if (d === null || (typeof d !== 'object' && typeof d !== 'function')) return names.length ? ns : d;
    return names.every((k) => k in d) ? d : ns;
  }

  // host.pkgLoad links ES modules, which in boa 0.22 leaves values on the
  // VM stack of the calling frame until that function returns. It is
  // therefore only called as a whole statement, never inside a larger
  // expression whose operands are still on the stack.
  function loadPackage(id, kind, spec) {
    const value = host.pkgLoad(id, kind, spec);
    return value;
  }

  function requirePackage(spec, parent) {
    const r = jsonParse(host.pkgResolve(spec, parent || ''));
    if (r.kind === 'remote') {
      if (!hasOwn(remotePkgs, r.id)) {
        const ns = loadPackage(r.id, 'remote', spec);
        remotePkgs[r.id] = interop(ns);
      }
      return remotePkgs[r.id];
    }
    const cache = S.localPkgs;
    if (hasOwn(cache, r.id)) return cache[r.id].exports;
    const module = { exports: {}, id: r.id, filename: r.id, loaded: false };
    cache[r.id] = module;
    let loaded;
    try {
      loaded = loadPackage(r.id, r.kind, spec);
    } catch (e) {
      delete cache[r.id];
      throw e;
    }
    try {
      if (r.kind === 'esm') {
        module.exports = interop(loaded);
      } else {
        const dirname = r.id.replace(/[\\/][^\\/]*$/, '');
        loaded.call(module.exports, module.exports, (s) => requireModule(s, r.id), module, r.id, dirname);
      }
    } catch (e) {
      delete cache[r.id];
      throw e;
    }
    module.loaded = true;
    return module.exports;
  }

  const NOT_FOUND = "Cannot find module '";

  function requireModule(name, parent) {
    name = String(name);
    const packages = !!(S && S.packages);
    let bare = name.replace(/^node:/, '');
    const pkgSpec = /^(npm|jsr):/.test(name);
    if (pkgSpec && packages) return requirePackage(name, parent);
    const npm = /^npm:((?:@[^/@]+\/)?[^@]+)(?:@.*)?$/.exec(bare);
    if (npm) bare = npm[1];
    if (hasOwn(builtinModules, bare) && builtinModules[bare]) return builtinModules[bare]();
    if (NODE_BUILTINS.indexOf(bare) !== -1 || name.indexOf('node:') === 0) {
      throw new Error("Cannot find module '" + name + "'. Node.js built-in modules are not available in Merpati scripts.");
    }
    if (packages && !pkgSpec) {
      try {
        return requirePackage(name, parent);
      } catch (e) {
        if (!(e && typeof e.message === 'string' && e.message.indexOf(NOT_FOUND + name + "'") === 0) || UNSUPPORTED.indexOf(bare) === -1) throw e;
      }
    }
    if (UNSUPPORTED.indexOf(bare) !== -1) {
      throw new Error("Cannot find module '" + name + "'. The '" + bare + "' library is not bundled with Merpati.");
    }
    if (pkgSpec) throw new Error("Cannot find module '" + name + "'. npm and JSR packages are not enabled for this run.");
    throw new Error("Cannot find module '" + name + "'");
  }
  const requireFromScript = function require(name) { return requireModule(name); };

  // ---------------------------------------------------------------------
  // Small Node-ish utilities: querystring, util, events, assert
  // ---------------------------------------------------------------------
  const querystring = {
    escape: (s) => encodeURIComponent(s),
    unescape: (s) => { try { return decodeURIComponent(String(s).replace(/\+/g, ' ')); } catch (e) { return s; } },
    parse(str, sep, eq) {
      sep = sep || '&'; eq = eq || '=';
      const o = {};
      String(str || '').split(sep).filter(Boolean).forEach((p) => {
        const i = p.indexOf(eq);
        const k = querystring.unescape(i === -1 ? p : p.slice(0, i));
        const v = i === -1 ? '' : querystring.unescape(p.slice(i + eq.length));
        if (hasOwn(o, k)) o[k] = [].concat(o[k], v); else o[k] = v;
      });
      return o;
    },
    stringify(obj, sep, eq) {
      sep = sep || '&'; eq = eq || '=';
      return Object.keys(obj || {}).map((k) => [].concat(obj[k]).map((v) => encodeURIComponent(k) + eq + encodeURIComponent(v === undefined || v === null ? '' : v)).join(sep)).join(sep);
    },
  };
  querystring.decode = querystring.parse;
  querystring.encode = querystring.stringify;

  const utilModule = {
    format, inspect,
    isArray: Array.isArray,
    isString: (v) => typeof v === 'string',
    isNumber: (v) => typeof v === 'number',
    isObject: (v) => v !== null && typeof v === 'object',
    isFunction: (v) => typeof v === 'function',
    isUndefined: (v) => v === undefined,
    isDeepStrictEqual: (a, b) => deepEqual(a, b, true),
    promisify: (fn) => function () {
      const args = Array.prototype.slice.call(arguments);
      return new Promise((resolve, reject) => fn.apply(this, args.concat([(err, val) => (err ? reject(err) : resolve(val))])));
    },
    inherits(ctor, superCtor) { Object.setPrototypeOf(ctor.prototype, superCtor.prototype); },
  };

  class EventEmitter {
    constructor() { define(this, '_events', {}); }
    on(name, fn) { (this._events[name] = this._events[name] || []).push(fn); return this; }
    addListener(name, fn) { return this.on(name, fn); }
    once(name, fn) {
      const w = (...a) => { this.off(name, w); fn.apply(this, a); };
      w.listener = fn;
      return this.on(name, w);
    }
    off(name, fn) {
      const l = this._events[name];
      if (l) this._events[name] = l.filter((x) => x !== fn && x.listener !== fn);
      return this;
    }
    removeListener(name, fn) { return this.off(name, fn); }
    removeAllListeners(name) { if (name === undefined) define(this, '_events', {}); else delete this._events[name]; return this; }
    emit(name, ...args) {
      const l = (this._events[name] || []).slice();
      l.forEach((fn) => fn.apply(this, args));
      return l.length > 0;
    }
    listeners(name) { return (this._events[name] || []).slice(); }
    listenerCount(name) { return (this._events[name] || []).length; }
  }

  function deepEqual(a, b, strict) {
    if (strict ? Object.is(a, b) : a == b) return true; // eslint-disable-line eqeqeq
    if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
    if (a instanceof Date && b instanceof Date) return a.getTime() === b.getTime();
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
    const ka = Object.keys(a), kb = Object.keys(b);
    if (ka.length !== kb.length) return false;
    return ka.every((k) => hasOwn(b, k) && deepEqual(a[k], b[k], strict));
  }

  class AssertionError extends Error {
    constructor(o) {
      super(o.message);
      this.name = 'AssertionError';
      this.actual = o.actual;
      this.expected = o.expected;
      this.operator = o.operator;
    }
  }
  function assertFail(message, actual, expected, operator, dflt) {
    if (message instanceof Error) throw message;
    throw new AssertionError({ message: message || dflt, actual, expected, operator });
  }
  const assertModule = function (v, m) { if (!v) assertFail(m, v, true, '==', 'The expression evaluated to a falsy value'); };
  Object.assign(assertModule, {
    AssertionError,
    ok: assertModule,
    equal: (a, b, m) => { if (a != b) assertFail(m, a, b, '==', inspect(a) + ' == ' + inspect(b)); }, // eslint-disable-line eqeqeq
    notEqual: (a, b, m) => { if (a == b) assertFail(m, a, b, '!=', inspect(a) + ' != ' + inspect(b)); }, // eslint-disable-line eqeqeq
    strictEqual: (a, b, m) => { if (!Object.is(a, b)) assertFail(m, a, b, 'strictEqual', 'Expected values to be strictly equal:\n' + inspect(a) + ' !== ' + inspect(b)); },
    notStrictEqual: (a, b, m) => { if (Object.is(a, b)) assertFail(m, a, b, 'notStrictEqual', 'Expected "actual" to be strictly unequal to: ' + inspect(b)); },
    deepEqual: (a, b, m) => { if (!deepEqual(a, b, false)) assertFail(m, a, b, 'deepEqual', 'Expected values to be loosely deep-equal'); },
    deepStrictEqual: (a, b, m) => { if (!deepEqual(a, b, true)) assertFail(m, a, b, 'deepStrictEqual', 'Expected values to be strictly deep-equal'); },
    notDeepEqual: (a, b, m) => { if (deepEqual(a, b, false)) assertFail(m, a, b, 'notDeepEqual', 'Expected "actual" not to be loosely deep-equal to "expected"'); },
    notDeepStrictEqual: (a, b, m) => { if (deepEqual(a, b, true)) assertFail(m, a, b, 'notDeepStrictEqual', 'Expected "actual" not to be strictly deep-equal to "expected"'); },
    fail: (m) => assertFail(m, undefined, undefined, 'fail', 'Failed'),
    throws(fn, expected, m) {
      try { fn(); } catch (e) {
        if (typeof expected === 'function' && expected.prototype !== undefined && !(e instanceof expected)) throw e;
        if (expected instanceof RegExp && !expected.test(String(e && e.message))) assertFail(m, e, expected, 'throws', 'The error message did not match');
        return;
      }
      assertFail(typeof expected === 'string' ? expected : m, undefined, expected, 'throws', 'Missing expected exception.');
    },
    doesNotThrow(fn, m) { try { fn(); } catch (e) { assertFail(m, e, undefined, 'doesNotThrow', 'Got unwanted exception: ' + errorMessage(e)); } },
    match: (s, re, m) => { if (!re.test(s)) assertFail(m, s, re, 'match', 'The input did not match the regular expression ' + re); },
  });
  assertModule.strict = assertModule;

  // ---------------------------------------------------------------------
  // Encoding: btoa/atob, UTF-8, Buffer, TextEncoder/TextDecoder
  // ---------------------------------------------------------------------
  const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const B64_LOOKUP = (() => {
    const t = {};
    for (let i = 0; i < B64.length; i++) t[B64[i]] = i;
    t['-'] = 62; t._ = 63;
    return t;
  })();

  function bytesToBase64(bytes) {
    let out = '';
    let i = 0;
    for (; i + 2 < bytes.length; i += 3) {
      const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
      out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63] + B64[(n >> 6) & 63] + B64[n & 63];
    }
    if (i < bytes.length) {
      const n = (bytes[i] << 16) | ((i + 1 < bytes.length ? bytes[i + 1] : 0) << 8);
      out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63] + (i + 1 < bytes.length ? B64[(n >> 6) & 63] : '=') + '=';
    }
    return out;
  }

  function base64ToBytes(str) {
    const s = String(str).replace(/[\s=]/g, '');
    const out = [];
    let buf = 0, bits = 0;
    for (let i = 0; i < s.length; i++) {
      const v = B64_LOOKUP[s[i]];
      if (v === undefined) continue;
      buf = (buf << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out.push((buf >> bits) & 0xff);
      }
    }
    return out;
  }

  function utf8Encode(str) {
    const out = [];
    str = String(str);
    for (let i = 0; i < str.length; i++) {
      let c = str.charCodeAt(i);
      if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
        const d = str.charCodeAt(i + 1);
        if (d >= 0xdc00 && d <= 0xdfff) {
          c = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00);
          i++;
        } else c = 0xfffd;
      } else if (c >= 0xd800 && c <= 0xdfff) c = 0xfffd;
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
      else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
      else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
    return out;
  }

  // `fatal`: throw a TypeError on malformed input instead of using U+FFFD.
  function utf8Decode(bytes, start, end, fatal) {
    start = start || 0;
    end = end === undefined ? bytes.length : end;
    let out = '';
    const bad = () => {
      if (fatal) throw new TypeError('The encoded data was not valid for encoding utf-8');
      return '�';
    };
    for (let i = start; i < end;) {
      const b = bytes[i];
      let c, n;
      if (b < 0x80) { c = b; n = 1; } else if (b >> 5 === 6) { c = b & 31; n = 2; } else if (b >> 4 === 14) { c = b & 15; n = 3; } else if (b >> 3 === 30) { c = b & 7; n = 4; } else { out += bad(); i++; continue; }
      if (i + n > end) { out += bad(); break; }
      let ok = true;
      for (let k = 1; k < n; k++) {
        const x = bytes[i + k];
        if ((x & 0xc0) !== 0x80) { ok = false; break; }
        c = (c << 6) | (x & 63);
      }
      if (!ok) { out += bad(); i++; continue; }
      i += n;
      if (c >= 0x10000) {
        c -= 0x10000;
        out += String.fromCharCode(0xd800 + (c >> 10), 0xdc00 + (c & 1023));
      } else out += String.fromCharCode(c);
    }
    return out;
  }

  function btoa(s) {
    s = String(s);
    const bytes = [];
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (c > 255) {
        const e = new Error("Failed to execute 'btoa': The string to be encoded contains characters outside of the Latin1 range.");
        e.name = 'InvalidCharacterError';
        throw e;
      }
      bytes.push(c);
    }
    return bytesToBase64(bytes);
  }

  function atob(s) {
    return base64ToBytes(String(s)).map((b) => String.fromCharCode(b)).join('');
  }

  function stringBytes(value, encoding) {
    const enc = String(encoding || 'utf8').toLowerCase();
    let bytes;
    if (enc === 'base64' || enc === 'base64url') bytes = base64ToBytes(value);
    else if (enc === 'hex') {
      bytes = [];
      for (let i = 0; i + 1 < value.length; i += 2) {
        const b = parseInt(value.substr(i, 2), 16);
        if (isNaN(b)) break;
        bytes.push(b);
      }
    } else if (enc === 'ascii' || enc === 'latin1' || enc === 'binary') {
      bytes = [];
      for (let i = 0; i < value.length; i++) bytes.push(value.charCodeAt(i) & 0xff);
    } else if (enc === 'utf16le' || enc === 'ucs2') {
      bytes = [];
      for (let i = 0; i < value.length; i++) { const c = value.charCodeAt(i); bytes.push(c & 0xff, c >> 8); }
    } else bytes = utf8Encode(value);
    return bytes;
  }

  class Buffer extends Uint8Array {
    // The deprecated `new Buffer(string[, encoding])`, still common in old
    // Postman scripts (`new Buffer(user + ':' + pass).toString('base64')`).
    constructor(arg, encodingOrOffset, length) {
      if (typeof arg === 'string') {
        const bytes = stringBytes(arg, encodingOrOffset);
        super(bytes.length);
        this.set(bytes);
      } else super(arg, encodingOrOffset, length);
    }
    static from(value, encodingOrOffset, length) {
      if (typeof value === 'string') return new Buffer(value, encodingOrOffset);
      if (value instanceof ArrayBuffer) {
        const off = encodingOrOffset || 0;
        return new Buffer(value.slice(off, length === undefined ? undefined : off + length));
      }
      if (Array.isArray(value) || ArrayBuffer.isView(value)) {
        const b = new Buffer(value.length);
        for (let i = 0; i < value.length; i++) b[i] = value[i] & 0xff;
        return b;
      }
      if (isObject(value) && value.type === 'Buffer' && Array.isArray(value.data)) return Buffer.from(value.data);
      throw new TypeError('The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array.');
    }
    static alloc(size, fill, encoding) {
      const b = new Buffer(size);
      if (typeof fill === 'string') {
        // Repeats the encoded string, as Node does.
        const bytes = stringBytes(fill, encoding);
        if (bytes.length) for (let i = 0; i < b.length; i++) b[i] = bytes[i % bytes.length];
      } else if (fill !== undefined) b.fill(fill);
      return b;
    }
    static allocUnsafe(size) { return new Buffer(size); }
    static isBuffer(b) { return b instanceof Buffer; }
    static byteLength(s, enc) { return typeof s === 'string' ? Buffer.from(s, enc).length : s.length; }
    static concat(list, total) {
      const len = total === undefined ? list.reduce((n, b) => n + b.length, 0) : total;
      const out = new Buffer(len);
      let off = 0;
      for (const b of list) {
        out.set(b.subarray(0, Math.min(b.length, len - off)), off);
        off += b.length;
        if (off >= len) break;
      }
      return out;
    }
    static isEncoding(e) { return ['utf8', 'utf-8', 'hex', 'base64', 'base64url', 'ascii', 'latin1', 'binary', 'ucs2', 'utf16le'].indexOf(String(e).toLowerCase()) !== -1; }
    toString(encoding, start, end) {
      const enc = String(encoding || 'utf8').toLowerCase();
      const s = start || 0;
      const e = end === undefined ? this.length : Math.min(end, this.length);
      const slice = Array.prototype.slice.call(this, s, e);
      if (enc === 'base64') return bytesToBase64(slice);
      if (enc === 'base64url') return bytesToBase64(slice).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
      if (enc === 'hex') return slice.map((b) => (b + 0x100).toString(16).slice(1)).join('');
      if (enc === 'ascii' || enc === 'latin1' || enc === 'binary') return slice.map((b) => String.fromCharCode(enc === 'ascii' ? b & 0x7f : b)).join('');
      if (enc === 'utf16le' || enc === 'ucs2') { let r = ''; for (let i = 0; i + 1 < slice.length; i += 2) r += String.fromCharCode(slice[i] | (slice[i + 1] << 8)); return r; }
      return utf8Decode(slice);
    }
    toJSON() { return { type: 'Buffer', data: Array.prototype.slice.call(this) }; }
    equals(other) { return this.length === other.length && this.every((b, i) => b === other[i]); }
    slice(start, end) { return Buffer.from(Array.prototype.slice.call(this, start, end)); }
    write(string, offset, length, encoding) {
      const b = Buffer.from(string, encoding);
      const off = offset || 0;
      const n = Math.min(b.length, length === undefined ? this.length - off : length);
      for (let i = 0; i < n; i++) this[off + i] = b[i];
      return n;
    }
  }

  // ES2024 `ArrayBuffer.prototype.transfer` (boa has it only behind a
  // feature flag; packages such as @std/encoding use it). The copy cannot
  // detach the original buffer.
  if (typeof ArrayBuffer.prototype.transfer !== 'function') {
    const transfer = function transfer(newLength) {
      const len = newLength === undefined ? this.byteLength : Math.max(0, Math.trunc(Number(newLength)) || 0);
      const out = new ArrayBuffer(len);
      new Uint8Array(out).set(new Uint8Array(this, 0, Math.min(len, this.byteLength)));
      return out;
    };
    define(ArrayBuffer.prototype, 'transfer', transfer);
    define(ArrayBuffer.prototype, 'transferToFixedLength', transfer);
  }

  // ES2024 `Array.fromAsync` (missing in boa 0.22; the usual way to
  // collect a ReadableStream or async generator).
  if (typeof Array.fromAsync !== 'function') {
    define(Array, 'fromAsync', async function fromAsync(items) {
      const mapFn = arguments[1];
      const thisArg = arguments[2];
      if (mapFn !== undefined && typeof mapFn !== 'function') throw new TypeError('Array.fromAsync: mapper is not a function');
      if (items === undefined || items === null) throw new TypeError('Array.fromAsync requires an array-like or iterable object');
      const out = [];
      const add = async (v) => { out.push(mapFn ? await mapFn.call(thisArg, v, out.length) : v); };
      if (typeof items[Symbol.asyncIterator] === 'function') {
        for await (const v of items) await add(v);
      } else if (typeof items[Symbol.iterator] === 'function') {
        for (const v of items) await add(await v);
      } else {
        const len = Math.min(Math.max(0, Math.floor(Number(Object(items).length)) || 0), 2 ** 32 - 1);
        for (let k = 0; k < len; k++) await add(await items[k]);
      }
      return out;
    });
  }

  // Minimal in-memory Blob/File (esm.sh's `buffer` polyfill and many
  // packages reference them).
  class Blob {
    constructor(parts, options) {
      const chunks = [];
      for (const p of parts || []) {
        if (p instanceof Blob) chunks.push(p._bytes);
        else if (p instanceof ArrayBuffer) chunks.push(new Uint8Array(p));
        else if (ArrayBuffer.isView(p)) chunks.push(new Uint8Array(p.buffer, p.byteOffset, p.byteLength));
        else chunks.push(Uint8Array.from(utf8Encode(String(p))));
      }
      const bytes = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
      let off = 0;
      for (const c of chunks) {
        bytes.set(c, off);
        off += c.length;
      }
      define(this, '_bytes', bytes);
      define(this, '_type', options && options.type ? String(options.type).toLowerCase() : '');
    }
    get size() { return this._bytes.length; }
    get type() { return this._type; }
    slice(start, end, type) {
      const b = new Blob([this._bytes.slice(start, end)]);
      define(b, '_type', type ? String(type).toLowerCase() : '');
      return b;
    }
    text() { return Promise.resolve(utf8Decode(this._bytes)); }
    arrayBuffer() { return Promise.resolve(this._bytes.slice().buffer); }
    bytes() { return Promise.resolve(this._bytes.slice()); }
    stream() {
      const bytes = this._bytes.slice();
      return new (streamsLib().ReadableStream)({ type: 'bytes', start(c) { if (bytes.length) c.enqueue(bytes); c.close(); } });
    }
    get [Symbol.toStringTag]() { return 'Blob'; }
  }

  class File extends Blob {
    constructor(parts, name, options) {
      super(parts, options);
      define(this, '_name', String(name));
      define(this, '_lastModified', options && options.lastModified !== undefined ? Number(options.lastModified) : Date.now());
    }
    get name() { return this._name; }
    get lastModified() { return this._lastModified; }
    get [Symbol.toStringTag]() { return 'File'; }
  }

  class TextEncoder {
    get encoding() { return 'utf-8'; }
    encode(s) { const a = utf8Encode(s === undefined ? '' : s); const u = new Uint8Array(a.length); u.set(a); return u; }
    encodeInto(s, dest) {
      const a = utf8Encode(s);
      let written = Math.min(a.length, dest.length);
      // Never split a character.
      while (written < a.length && written > 0 && (a[written] & 0xc0) === 0x80) written--;
      for (let i = 0; i < written; i++) dest[i] = a[i];
      return { read: utf8Decode(a, 0, written).length, written };
    }
  }
  const UTF8_LABELS = ['utf-8', 'utf8', 'unicode-1-1-utf-8', 'unicode11utf8', 'unicode20utf8', 'x-unicode20utf8'];
  // UTF-8 only (other labels decode as UTF-8 too); `stream: true` keeps an
  // incomplete trailing sequence for the next call.
  class TextDecoder {
    constructor(label, options) {
      const l = label === undefined ? 'utf-8' : String(label).trim().toLowerCase();
      define(this, '_label', UTF8_LABELS.indexOf(l) !== -1 ? 'utf-8' : l);
      define(this, '_fatal', !!(options && options.fatal));
      define(this, '_ignoreBOM', !!(options && options.ignoreBOM));
      define(this, '_pending', null);
      define(this, '_started', false);
    }
    get encoding() { return this._label; }
    get fatal() { return this._fatal; }
    get ignoreBOM() { return this._ignoreBOM; }
    decode(buf, options) {
      let u;
      if (buf === undefined || buf === null) u = new Uint8Array(0);
      else if (buf instanceof ArrayBuffer) u = new Uint8Array(buf);
      else if (ArrayBuffer.isView(buf)) u = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
      else u = buf;
      const stream = !!(options && options.stream);
      if (this._pending) {
        const joined = new Uint8Array(this._pending.length + u.length);
        joined.set(this._pending);
        joined.set(u, this._pending.length);
        u = joined;
        this._pending = null;
      }
      let end = u.length;
      if (stream) {
        // Hold back an incomplete multi-byte sequence at the end.
        let lead = end - 1;
        while (lead >= 0 && lead > end - 4 && (u[lead] & 0xc0) === 0x80) lead--;
        if (lead >= 0 && u[lead] >= 0xc0 && u[lead] < 0xf8) {
          const need = u[lead] >= 0xf0 ? 4 : u[lead] >= 0xe0 ? 3 : 2;
          if (end - lead < need) end = lead;
        }
        if (end < u.length) this._pending = Uint8Array.prototype.slice.call(u, end);
      }
      let start = 0;
      if (!this._started && end > 0) {
        if (!this._ignoreBOM && u[0] === 0xef && u[1] === 0xbb && u[2] === 0xbf) start = 3;
        this._started = true;
      }
      const out = utf8Decode(u, start, end, this._fatal);
      if (!stream) {
        this._pending = null;
        this._started = false;
      }
      return out;
    }
  }

  // ---------------------------------------------------------------------
  // URL / URLSearchParams (WHATWG-ish)
  // ---------------------------------------------------------------------
  class URLSearchParams {
    constructor(init) {
      define(this, '_list', []);
      if (typeof init === 'string') {
        (init.charAt(0) === '?' ? init.slice(1) : init).split('&').filter(Boolean).forEach((p) => {
          const i = p.indexOf('=');
          this._list.push([querystring.unescape(i === -1 ? p : p.slice(0, i)), querystring.unescape(i === -1 ? '' : p.slice(i + 1))]);
        });
      } else if (Array.isArray(init)) init.forEach((p) => this._list.push([String(p[0]), String(p[1])]));
      else if (isObject(init)) Object.keys(init).forEach((k) => this._list.push([k, String(init[k])]));
    }
    append(k, v) { this._list.push([String(k), String(v)]); this._update(); }
    delete(k) { define(this, '_list', this._list.filter((p) => p[0] !== k)); this._update(); }
    get(k) { const p = this._list.find((x) => x[0] === k); return p ? p[1] : null; }
    getAll(k) { return this._list.filter((x) => x[0] === k).map((x) => x[1]); }
    has(k) { return this._list.some((x) => x[0] === k); }
    set(k, v) {
      const i = this._list.findIndex((x) => x[0] === k);
      if (i === -1) this._list.push([String(k), String(v)]);
      else { this._list[i][1] = String(v); define(this, '_list', this._list.filter((x, j) => j <= i || x[0] !== k)); }
      this._update();
    }
    sort() { this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); this._update(); }
    forEach(fn, ctx) { this._list.forEach((p) => fn.call(ctx, p[1], p[0], this)); }
    keys() { return this._list.map((p) => p[0])[Symbol.iterator](); }
    values() { return this._list.map((p) => p[1])[Symbol.iterator](); }
    entries() { return this._list.map((p) => [p[0], p[1]])[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    get size() { return this._list.length; }
    toString() {
      // application/x-www-form-urlencoded: only `*-._` and alphanumerics stay.
      const enc = (s) => encodeURIComponent(s).replace(/%20/g, '+').replace(/[!'()~]/g, (c) => '%' + c.charCodeAt(0).toString(16).toUpperCase());
      return this._list.map((p) => enc(p[0]) + '=' + enc(p[1])).join('&');
    }
    _update() { if (this._url) this._url._search = this._list.length ? '?' + this.toString() : ''; }
  }

  class URL {
    constructor(url, base) {
      let s = String(url);
      if (base !== undefined && !/^[a-zA-Z][a-zA-Z\d+\-.]*:/.test(s)) {
        const b = new URL(base);
        if (s.charAt(0) === '/') s = b.origin + s;
        else s = b.origin + b.pathname.replace(/[^/]*$/, '') + s;
      }
      const m = /^([a-zA-Z][a-zA-Z\d+\-.]*:)(?:\/\/(?:([^:@/]*)(?::([^@/]*))?@)?(\[[^\]]+\]|[^:/?#]*)(?::(\d*))?)?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(s);
      if (!m) throw new TypeError('Invalid URL: ' + s);
      define(this, '_protocol', m[1].toLowerCase());
      define(this, '_username', m[2] || '');
      define(this, '_password', m[3] || '');
      define(this, '_hostname', (m[4] || '').toLowerCase());
      define(this, '_port', m[5] || '');
      define(this, '_pathname', m[6] || (m[4] !== undefined ? '/' : ''));
      define(this, '_search', m[7] && m[7] !== '?' ? m[7] : '');
      define(this, '_hash', m[8] && m[8] !== '#' ? m[8] : '');
      const defaults = { 'http:': '80', 'https:': '443', 'ws:': '80', 'wss:': '443', 'ftp:': '21' };
      if (defaults[this._protocol] === this._port) this._port = '';
      define(this, '_sp', new URLSearchParams(this._search));
      define(this._sp, '_url', this);
    }
    get protocol() { return this._protocol; } set protocol(v) { this._protocol = String(v).replace(/:?$/, ':'); }
    get username() { return this._username; } set username(v) { this._username = String(v); }
    get password() { return this._password; } set password(v) { this._password = String(v); }
    get hostname() { return this._hostname; } set hostname(v) { this._hostname = String(v); }
    get port() { return this._port; } set port(v) { this._port = String(v); }
    get host() { return this._hostname + (this._port ? ':' + this._port : ''); }
    set host(v) { const p = String(v).split(':'); this._hostname = p[0]; this._port = p[1] || ''; }
    get origin() { return this._protocol + '//' + this.host; }
    get pathname() { return this._pathname; } set pathname(v) { this._pathname = String(v).charAt(0) === '/' ? String(v) : '/' + v; }
    get search() { return this._search; }
    set search(v) { v = String(v); this._search = v && v !== '?' ? (v.charAt(0) === '?' ? v : '?' + v) : ''; define(this, '_sp', new URLSearchParams(this._search)); define(this._sp, '_url', this); }
    get searchParams() { return this._sp; }
    get hash() { return this._hash; } set hash(v) { v = String(v); this._hash = v && v !== '#' ? (v.charAt(0) === '#' ? v : '#' + v) : ''; }
    get href() {
      const cred = this._username ? this._username + (this._password ? ':' + this._password : '') + '@' : '';
      return this._protocol + '//' + cred + this.host + this._pathname + this._search + this._hash;
    }
    set href(v) {
      const u = new URL(v);
      for (const k of ['_protocol', '_username', '_password', '_hostname', '_port', '_pathname', '_hash']) define(this, k, u[k]);
      this.search = u._search;
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
    static canParse(u, b) { try { new URL(u, b); return true; } catch (e) { return false; } } // eslint-disable-line no-new
  }

  // ---------------------------------------------------------------------
  // XML -> JSON (xml2js compatible subset)
  // ---------------------------------------------------------------------
  function decodeEntities(s) {
    return s.replace(/&(#x[0-9a-fA-F]+|#\d+|lt|gt|amp|quot|apos);/g, (m, e) => {
      switch (e) {
        case 'lt': return '<';
        case 'gt': return '>';
        case 'amp': return '&';
        case 'quot': return '"';
        case 'apos': return "'";
      }
      const code = e.charAt(1) === 'x' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
      return String.fromCodePoint(code);
    });
  }

  function parseXmlTree(xml) {
    const root = { name: '#root', attrs: {}, children: [], text: '' };
    const stack = [root];
    let i = 0;
    const s = String(xml);
    const re = /<!--[\s\S]*?-->|<!\[CDATA\[([\s\S]*?)\]\]>|<\?[\s\S]*?\?>|<!DOCTYPE[^>[]*(\[[^\]]*\])?[^>]*>|<\/\s*([^\s>]+)\s*>|<([^\s/>]+)((?:\s+[^\s=/>]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?)*)\s*(\/?)>/g;
    let m;
    while ((m = re.exec(s))) {
      const top = stack[stack.length - 1];
      if (m.index > i) top.text += decodeEntities(s.slice(i, m.index));
      i = re.lastIndex;
      if (m[1] !== undefined) {
        top.text += m[1];
        top.cdata = true;
      } else if (m[3] !== undefined) {
        if (stack.length > 1) stack.pop();
      } else if (m[4] !== undefined) {
        const node = { name: m[4], attrs: {}, children: [], text: '' };
        const ar = /([^\s=/>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g;
        let a;
        while ((a = ar.exec(m[5] || ''))) node.attrs[a[1]] = decodeEntities(a[2] !== undefined ? a[2] : a[3] !== undefined ? a[3] : a[4] !== undefined ? a[4] : '');
        top.children.push(node);
        if (!m[6]) stack.push(node);
      }
    }
    if (stack.length > 1) throw new Error('Unclosed root tag');
    const elements = root.children;
    if (!elements.length) throw new Error('Non-whitespace before first tag.');
    return elements[0];
  }

  function xmlToJs(node, opts) {
    const explicitArray = opts.explicitArray !== false;
    const attrkey = opts.attrkey || '$';
    const charkey = opts.charkey || '_';
    const trim = !!opts.trim;
    const mergeAttrs = !!opts.mergeAttrs;
    const ignoreAttrs = !!opts.ignoreAttrs;
    const emptyTag = opts.emptyTag !== undefined ? opts.emptyTag : '';
    function conv(n) {
      let text = n.text;
      if (trim) text = text.trim();
      const hasAttrs = !ignoreAttrs && Object.keys(n.attrs).length > 0;
      const hasChildren = n.children.length > 0;
      if (!hasAttrs && !hasChildren) return text === '' ? emptyTag : text;
      const o = {};
      if (hasAttrs) {
        if (mergeAttrs) Object.keys(n.attrs).forEach((k) => { o[k] = explicitArray ? [n.attrs[k]] : n.attrs[k]; });
        else o[attrkey] = Object.assign({}, n.attrs);
      }
      for (const c of n.children) {
        const v = conv(c);
        if (hasOwn(o, c.name)) {
          if (!Array.isArray(o[c.name])) o[c.name] = [o[c.name]];
          o[c.name].push(v);
        } else o[c.name] = explicitArray ? [v] : v;
      }
      if (text.trim() !== '') o[charkey] = text;
      return o;
    }
    const out = conv(node);
    return opts.explicitRoot === false ? out : { [node.name]: out };
  }

  const xml2js = {
    parseString(xml, opts, cb) {
      if (typeof opts === 'function') { cb = opts; opts = {}; }
      let result, err = null;
      // Like xml2js, empty input is not an error: the result is null.
      if (String(xml).trim() === '') result = null;
      else try { result = xmlToJs(parseXmlTree(xml), opts || {}); } catch (e) { err = e; }
      if (cb) cb(err, result);
    },
    parseStringPromise(xml, opts) {
      return new Promise((resolve, reject) => xml2js.parseString(xml, opts || {}, (e, r) => (e ? reject(e) : resolve(r))));
    },
    Parser: function Parser(opts) {
      this.options = opts || {};
      this.parseString = (xml, cb) => xml2js.parseString(xml, this.options, cb);
      this.parseStringPromise = (xml) => xml2js.parseStringPromise(xml, this.options);
    },
  };

  // Postman's xml2Json swallows parse errors: invalid XML gives undefined,
  // an empty body null.
  function xml2Json(xml) {
    let out;
    xml2js.parseString(xml, { explicitArray: false, trim: true, mergeAttrs: false }, (e, r) => { out = r; });
    return out;
  }

  // ---------------------------------------------------------------------
  // Timers
  // ---------------------------------------------------------------------
  function addTimer(fn, ms, args, interval) {
    if (typeof fn !== 'function') throw new TypeError('The "callback" argument must be of type function');
    const id = ++S.timerSeq;
    let delay = Math.max(0, Number(ms) || 0);
    // Like Node: delays that do not fit in 32 bits (`Infinity`) mean 1 ms.
    if (delay > 2147483647) delay = 1;
    S.timers.set(id, { id, fn, args, due: Date.now() + delay, interval: interval ? Math.max(1, delay) : 0, location: S.location });
    return id;
  }
  const timerFns = {
    setTimeout: (fn, ms, ...args) => addTimer(fn, ms, args, false),
    setInterval: (fn, ms, ...args) => addTimer(fn, ms, args, true),
    setImmediate: (fn, ...args) => addTimer(fn, 0, args, false),
    clearTimeout: (id) => { if (S) S.timers.delete(typeof id === 'object' && id ? id.id : id); },
  };
  timerFns.clearInterval = timerFns.clearTimeout;
  timerFns.clearImmediate = timerFns.clearTimeout;

  // ---------------------------------------------------------------------
  // console
  // ---------------------------------------------------------------------
  function log(level) {
    return function () {
      host.check();
      if (S) S.console.push({ level, message: format.apply(null, arguments) });
    };
  }
  const consoleObj = {
    log: log('log'), info: log('info'), warn: log('warn'), error: log('error'), debug: log('debug'),
    trace: log('debug'),
    dir: (o) => log('log')(inspect(o, { depth: 4 })),
    table: (o) => log('log')(inspect(o, { depth: 3 })),
    assert: (cond, ...args) => { if (!cond) log('error')('Assertion failed' + (args.length ? ': ' + format.apply(null, args) : '')); },
    count: (label) => { const counters = S.counters; label = label === undefined ? 'default' : label; counters[label] = (counters[label] || 0) + 1; log('log')(label + ': ' + counters[label]); },
    countReset: (label) => { S.counters[label === undefined ? 'default' : label] = 0; },
    time: (label) => { S.timeLabels[label === undefined ? 'default' : label] = Date.now(); },
    timeLog: (label) => { label = label === undefined ? 'default' : label; log('log')(label + ': ' + (Date.now() - (S.timeLabels[label] || Date.now())) + 'ms'); },
    timeEnd: (label) => { label = label === undefined ? 'default' : label; log('log')(label + ': ' + (Date.now() - (S.timeLabels[label] || Date.now())) + 'ms'); delete S.timeLabels[label]; },
    group: log('log'), groupCollapsed: log('log'), groupEnd: () => {}, clear: () => {},
  };

  // ---------------------------------------------------------------------
  // Tests
  // ---------------------------------------------------------------------
  // `thrown`: `err` was thrown or rejected, so even `undefined` and `null`
  // fail the test (a `done` callback passes them for success).
  function settle(t, err, thrown) {
    if (!t.pending) return;
    t.pending = false;
    if (thrown || (err !== undefined && err !== null)) {
      t.passed = false;
      t.error = errorMessage(err);
    }
  }

  function test(name, fn) {
    host.check();
    const t = { name: String(name), passed: true, error: null, skipped: false, pending: true };
    S.tests.push(t);
    if (typeof fn !== 'function') {
      t.pending = false;
      t.skipped = true;
      return pm;
    }
    try {
      if (fn.length > 0) {
        // done-callback form
        fn((err) => settle(t, err));
      } else {
        const r = fn();
        if (r && typeof r.then === 'function') {
          r.then(() => settle(t), (e) => settle(t, e === undefined ? new Error('Promise rejected') : e, true));
        } else settle(t);
      }
    } catch (e) {
      settle(t, e, true);
    }
    return pm;
  }
  test.skip = function (name) {
    S.tests.push({ name: String(name), passed: true, error: null, skipped: true, pending: false });
    return pm;
  };

  // ---------------------------------------------------------------------
  // pm.sendRequest
  // ---------------------------------------------------------------------
  function authHeader(auth) {
    if (!auth || !auth.type) return null;
    const p = (k) => {
      const list = auth[auth.type];
      if (Array.isArray(list)) { const e = list.find((x) => x.key === k); return e ? e.value : undefined; }
      return list ? list[k] : undefined;
    };
    if (auth.type === 'basic') return { key: 'Authorization', value: 'Basic ' + bytesToBase64(utf8Encode((p('username') || '') + ':' + (p('password') || ''))) };
    if (auth.type === 'bearer') return { key: 'Authorization', value: 'Bearer ' + (p('token') || '') };
    if (auth.type === 'apikey' && (p('in') || 'header') === 'header') return { key: p('key') || 'X-API-Key', value: p('value') || '' };
    return null;
  }

  function toCoreRequest(r) {
    const req = r instanceof Request ? r : new Request(r);
    const headers = kvToCore(req.headers);
    const ah = authHeader(isObject(r) && !(r instanceof Request) ? r.auth : req.auth);
    if (ah && !headers.some((h) => h.key.toLowerCase() === ah.key.toLowerCase())) headers.push(ah);
    if (isObject(r) && r.auth && r.auth.type === 'apikey') {
      const list = r.auth.apikey;
      const get = (k) => (Array.isArray(list) ? (list.find((x) => x.key === k) || {}).value : list && list[k]);
      if (get('in') === 'query') req.url.addQueryParams([{ key: get('key'), value: get('value') }]);
    }
    let body = bodyToCore(req.body, { mode: 'none' });
    if (req.body && req.body.mode === 'raw' && !(req.body.options && req.body.options.raw && req.body.options.raw.language)) body.raw_language = 'text';
    return { id: req.id || null, name: req.name || null, method: req.method || 'GET', url: req.url.toString(), headers, body };
  }

  function sendRequest(req, callback) {
    host.check();
    let coreReq;
    try {
      coreReq = toCoreRequest(typeof req === 'string' ? { url: req, method: 'GET' } : req);
    } catch (e) {
      coreReq = null;
      const err = e;
      if (typeof callback === 'function') Promise.resolve().then(() => guard(() => callback(err, undefined)));
      return Promise.reject(err);
    }
    const raw = host.send(JSON.stringify(coreReq), JSON.stringify(scopesSnapshot()));
    const result = JSON.parse(raw);
    let err = null, res;
    if (result.ok) res = new Response(result.response);
    else err = new Error(result.error);
    const location = S.location;
    if (typeof callback === 'function') {
      Promise.resolve().then(() => guard(() => callback(err, res), location));
      return undefined;
    }
    return err ? Promise.reject(err) : Promise.resolve(res);
  }

  function guard(fn, location) {
    try {
      fn();
    } catch (e) {
      recordError(location, errorMessage(e) + lineSuffix(e));
    }
  }

  // ---------------------------------------------------------------------
  // Cookies
  // ---------------------------------------------------------------------
  function cookieJar() {
    const list = () => S.cookies;
    const done = (cb, err, v) => { if (typeof cb === 'function') Promise.resolve().then(() => guard(() => cb(err, v))); };
    return {
      get(url, name, cb) { const c = list().one(name); done(cb, null, c ? c.value : undefined); },
      getAll(url, opts, cb) { if (typeof opts === 'function') cb = opts; done(cb, null, list().all().map((c) => c.toJSON())); },
      set(url, name, value, cb) {
        if (typeof value === 'function') { cb = value; value = undefined; }
        const c = isObject(name) ? name : { name, value };
        list().upsert(new Cookie(c));
        done(cb, null, c);
      },
      unset(url, name, cb) { list().remove(name); done(cb, null); },
      clear(url, cb) { list().clear(); done(cb, null); },
    };
  }

  // ---------------------------------------------------------------------
  // pm
  // ---------------------------------------------------------------------
  const pm = {};
  Object.defineProperties(pm, {
    environment: { get: () => S.scopes.environment, enumerable: true },
    globals: { get: () => S.scopes.globals, enumerable: true },
    collectionVariables: { get: () => S.scopes.collection, enumerable: true },
    iterationData: { get: () => S.scopes.data, enumerable: true },
    variables: { value: variables, enumerable: true },
    request: {
      get: () => S.request,
      set: (v) => {
        if (S.protocol) S.request = v instanceof ProtocolRequest ? v : new ProtocolRequest(S.protocol, isObject(v) && typeof v.toJSON === 'function' ? v.toJSON() : v);
        else S.request = v instanceof Request ? v : new Request(v);
      },
      enumerable: true,
    },
    response: { get: () => S.response || undefined, enumerable: true },
    cookies: { get: () => { const c = S.cookies; if (!c.jar) define(c, 'jar', cookieJar); return c; }, enumerable: true },
    info: {
      get: () => ({
        eventName: S.event,
        iteration: S.info.iteration || 0,
        iterationCount: S.info.iteration_count || 1,
        requestName: S.info.request_name || '',
        requestId: S.info.request_id || '',
        protocol: S.protocol || 'http',
      }),
      enumerable: true,
    },
    test: { value: test, enumerable: true },
    expect: { get: () => chai().expect, enumerable: true },
    sendRequest: { value: sendRequest, enumerable: true },
    require: { value: requireFromScript, enumerable: true },
    message: { get: () => (S && S.message) || undefined, enumerable: true },
    execution: {
      value: {
        setNextRequest(n) {
          if (n === null) S.next = 'Stop';
          else if (n !== undefined) S.next = { Named: String(n && n.name ? n.name : n) };
        },
        skipRequest() { if (S.event === 'prerequest') S.skip = true; },
        get location() {
          const loc = [S.info.collection_name || '', S.info.request_name || ''].filter(Boolean);
          loc.current = S.location;
          return loc;
        },
      },
      enumerable: true,
    },
    visualizer: {
      value: {
        set(template, data) { S.visualization = { template: String(template), data: data === undefined ? null : JSON.parse(JSON.stringify(data)) }; },
        clear() { S.visualization = null; },
      },
      enumerable: true,
    },
  });

  // The legacy setters store `value.toString()` for `true` and other truthy
  // values (postman-sandbox's legacy interface); `false`, `0`, `''`, `null`
  // and `undefined` are kept.
  const legacyValue = (v) => (v && typeof v.toString === 'function' ? v.toString() : v);

  // Legacy `postman` object.
  const postmanLegacy = {
    setNextRequest: (n) => pm.execution.setNextRequest(n),
    getEnvironmentVariable: (k) => S.scopes.environment.get(k),
    setEnvironmentVariable: (k, v) => S.scopes.environment.set(k, legacyValue(v)),
    clearEnvironmentVariable: (k) => S.scopes.environment.unset(k),
    clearEnvironmentVariables: () => S.scopes.environment.clear(),
    getGlobalVariable: (k) => S.scopes.globals.get(k),
    setGlobalVariable: (k, v) => S.scopes.globals.set(k, legacyValue(v)),
    clearGlobalVariable: (k) => S.scopes.globals.unset(k),
    clearGlobalVariables: () => S.scopes.globals.clear(),
    getVariable: (k) => variables.get(k),
    setVariable: (k, v) => variables.set(k, v),
    getResponseHeader: (k) => (S.response ? S.response.headers.get(k) : undefined),
    getResponseCookie: (k) => {
      if (!S.response) return undefined;
      const c = S.response.cookies.one(k);
      return c ? c.toJSON() : undefined;
    },
  };

  function legacyRequest() {
    const r = S.request;
    if (S.protocol) return r && typeof r.toJSON === 'function' ? r.toJSON() : r;
    let data = {};
    if (r.body) {
      if (r.body.mode === 'raw') data = r.body.raw;
      else if (r.body.mode === 'urlencoded') data = r.body.urlencoded.toObject(true);
      else if (r.body.mode === 'formdata') data = r.body.formdata.toObject(true);
    }
    return { id: r.id, name: r.name, url: r.url.toString(), method: r.method, headers: r.headers.toObject(true, true), data };
  }

  function scopesSnapshot() {
    const sc = S.scopes;
    const types = {};
    let t;
    if ((t = sc.globals._typeNames())) types.globals = t;
    if ((t = sc.collection._typeNames())) types.collection = t;
    if ((t = sc.environment._typeNames())) types.environment = t;
    if ((t = sc.local._typeNames())) types.local = t;
    return {
      globals: sc.globals._strings(),
      collection: sc.collection._strings(),
      environment: sc.environment._strings(),
      environment_name: sc.environment._name === undefined ? null : sc.environment._name,
      data: sc.data.toObject(),
      local: sc.local._strings(),
      types,
    };
  }

  // ---------------------------------------------------------------------
  // Globals
  // ---------------------------------------------------------------------
  const lazyGlobal = (name, loader) => {
    Object.defineProperty(G, name, {
      configurable: true,
      enumerable: false,
      get: loader,
      set(v) { Object.defineProperty(G, name, { value: v, writable: true, configurable: true, enumerable: false }); },
    });
  };

  const accessor = (name, get, set) => Object.defineProperty(G, name, { get, set: set || function () {}, configurable: true, enumerable: false });

  define(G, 'pm', pm);
  define(G, 'postman', postmanLegacy);
  define(G, 'console', consoleObj);
  define(G, 'require', requireFromScript);
  define(G, 'btoa', btoa);
  define(G, 'atob', atob);
  define(G, 'Buffer', Buffer);
  define(G, 'URL', URL);
  define(G, 'URLSearchParams', URLSearchParams);
  define(G, 'TextEncoder', TextEncoder);
  define(G, 'TextDecoder', TextDecoder);
  define(G, 'Blob', Blob);
  define(G, 'File', File);
  define(G, 'xml2Json', xml2Json);
  define(G, 'structuredClone', (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v))));
  const INTEGER_ARRAYS = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array,
    typeof BigInt64Array === 'function' ? BigInt64Array : Int8Array, typeof BigUint64Array === 'function' ? BigUint64Array : Int8Array];
  const domError = (name, message) => {
    const e = new Error(message);
    e.name = name;
    return e;
  };
  const cryptoObj = {
    getRandomValues(arr) {
      if (!INTEGER_ARRAYS.some((T) => arr instanceof T)) {
        throw domError('TypeMismatchError', "Failed to execute 'getRandomValues' on 'Crypto': The provided ArrayBufferView is not an integer array type.");
      }
      // The host hands out at most 64 KiB at once, like the Web Crypto API.
      if (arr.byteLength > 65536) {
        throw domError('QuotaExceededError', "Failed to execute 'getRandomValues' on 'Crypto': The ArrayBufferView's byte length (" +
          arr.byteLength + ') exceeds the number of bytes of entropy available via this API (65536).');
      }
      const b = host.random(arr.byteLength);
      const view = new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength);
      for (let i = 0; i < view.length; i++) view[i] = b[i];
      return arr;
    },
    randomUUID: makeUuid,
  };
  define(G, 'crypto', cryptoObj);
  for (const k of Object.keys(timerFns)) define(G, k, timerFns[k]);
  define(G, 'queueMicrotask', function queueMicrotask(callback) {
    if (typeof callback !== 'function') throw new TypeError('The "callback" argument must be of type function');
    const location = S ? S.location : '';
    Promise.resolve().then(() => guard(callback, location));
  });
  define(G, 'global', G);
  lazyGlobal('_', () => loadModule('lodash'));
  lazyGlobal('CryptoJS', () => loadModule('crypto-js'));
  lazyGlobal('tv4', () => loadModule('tv4'));
  lazyGlobal('moment', () => loadModule('moment'));
  accessor('cheerio', () => { throw new Error('cheerio is not bundled with Merpati'); });

  // Intl and Web Streams are JavaScript polyfills (js/intl.js,
  // js/streams.js) installed on first use, so scripts that never touch
  // them pay nothing. Like the bundled libraries they live as long as the
  // worker; only the `Intl` namespace is restored after every job (walking
  // every polyfill prototype would slow down all later jobs). Once loaded,
  // their globals become plain data properties, which are cheaper to check
  // when the job's globals are restored. That happens when the job ends:
  // redefining a global while its getter runs confuses boa's VM.
  const settled = [];
  function settleGlobal(name, value) {
    settled.push({ name, desc: plainDesc({ value, writable: true, configurable: true, enumerable: false }) });
  }
  function applySettled() {
    for (let s = 0; s < settled.length; s++) {
      const { name, desc } = settled[s];
      defProp(G, name, desc);
      let i = baseline.index[name];
      if (i === undefined) {
        i = baseline.names.length;
        baseline.names[i] = name;
        baseline.index[name] = i;
      }
      baseline.descs[i] = desc;
    }
    settled.length = 0;
    flatten(baseline);
  }

  let intlApi = null;
  function intlLib() {
    if (!intlApi) {
      intlApi = loadModule('intl')({ localTimeZone: () => host.tzLocal(), zoneOffset: (name, t) => host.tzOffset(name, t) });
      settleGlobal('Intl', intlApi.Intl);
      trackBuiltins([intlApi.Intl]);
    }
    return intlApi;
  }
  lazyGlobal('Intl', () => intlLib().Intl);
  const thisNumber = Number.prototype.valueOf;
  const thisBigInt = BigInt.prototype.valueOf;
  const thisTime = Date.prototype.getTime;
  define(Number.prototype, 'toLocaleString', function toLocaleString() {
    return intlLib().numberToLocaleString(thisNumber.call(this), arguments[0], arguments[1]);
  });
  define(BigInt.prototype, 'toLocaleString', function toLocaleString() {
    return intlLib().numberToLocaleString(thisBigInt.call(this), arguments[0], arguments[1]);
  });
  define(Date.prototype, 'toLocaleString', function toLocaleString() {
    return intlLib().dateToLocaleString(thisTime.call(this), 'any', arguments[0], arguments[1]);
  });
  define(Date.prototype, 'toLocaleDateString', function toLocaleDateString() {
    return intlLib().dateToLocaleString(thisTime.call(this), 'date', arguments[0], arguments[1]);
  });
  define(Date.prototype, 'toLocaleTimeString', function toLocaleTimeString() {
    return intlLib().dateToLocaleString(thisTime.call(this), 'time', arguments[0], arguments[1]);
  });
  define(String.prototype, 'localeCompare', function localeCompare(that) {
    if (this === undefined || this === null) throw new TypeError('String.prototype.localeCompare called on null or undefined');
    return intlLib().localeCompare(String(this), String(that), arguments[1], arguments[2]);
  });

  // The controller and BYOB classes cannot be constructed directly, so they
  // are only defined once streams are in use; the rest are lazy.
  const STREAM_GLOBALS = ['ReadableStream', 'ReadableStreamDefaultReader', 'WritableStream', 'WritableStreamDefaultWriter',
    'TransformStream', 'TextEncoderStream', 'TextDecoderStream', 'CountQueuingStrategy', 'ByteLengthQueuingStrategy'];
  const STREAM_INTERNALS = ['ReadableStreamBYOBReader', 'ReadableStreamDefaultController', 'ReadableByteStreamController',
    'ReadableStreamBYOBRequest', 'WritableStreamDefaultController', 'TransformStreamDefaultController'];
  let streamsApi = null;
  function streamsLib() {
    if (!streamsApi) {
      streamsApi = loadModule('streams')({ TextEncoder, TextDecoder });
      for (const name of STREAM_GLOBALS) settleGlobal(name, streamsApi[name]);
      // New names (not the accessor being read) can be added right away.
      for (const name of STREAM_INTERNALS) {
        if (!(name in G)) define(G, name, streamsApi[name]);
        settleGlobal(name, streamsApi[name]);
      }
    }
    return streamsApi;
  }
  STREAM_GLOBALS.forEach((name) => lazyGlobal(name, () => streamsLib()[name]));

  // Legacy sandbox globals, all bound to the current job.
  accessor('tests', () => S && S.legacyTests, (v) => { if (S) S.legacyTests = v; });
  accessor('responseBody', () => (S && S.response ? S.response.text() : undefined));
  accessor('responseCode', () => (S && S.response ? { code: S.response.code, name: S.response.status, detail: S.response.status } : undefined));
  accessor('responseHeaders', () => (S && S.response ? S.response.headers.toObject(false, true) : undefined));
  accessor('responseTime', () => (S && S.response ? S.response.responseTime : undefined));
  accessor('responseCookies', () => (S && S.response ? S.response.cookies.all().map((c) => c.toJSON()) : undefined));
  accessor('request', () => (S ? legacyRequest() : undefined));
  accessor('environment', () => (S ? S.scopes.environment.toObject() : undefined));
  accessor('globals', () => (S ? S.scopes.globals.toObject() : undefined));
  accessor('data', () => (S ? S.scopes.data.toObject() : undefined));
  accessor('iteration', () => (S ? S.info.iteration || 0 : undefined));

  // ---------------------------------------------------------------------
  // Hooks driven by the Rust worker
  // ---------------------------------------------------------------------
  let baseline = null;

  const mp = {
    begin(json) {
      const input = jsonParse(json);
      S = freshState(input);
      const v = input.variables || {};
      const types = v.types || {};
      S.scopes = {
        globals: new VariableScope(v.globals, { types: types.globals }),
        collection: new VariableScope(v.collection, { types: types.collection }),
        environment: new VariableScope(v.environment, {
          name: v.environment_name === null ? undefined : v.environment_name,
          types: types.environment,
        }),
        data: new VariableScope(v.data),
        local: new VariableScope(v.local, { types: types.local }),
      };
      const r = input.request || {};
      S.requestInput = r;
      S.cookies = new PropertyList(Cookie, (input.cookies || []).map((c) => new Cookie(c)));
      const proto = input.protocol;
      if (proto) {
        S.protocol = proto.kind;
        S.protocolInput = proto.request === undefined ? null : proto.request;
        S.request = new ProtocolRequest(proto.kind, proto.request);
        S.protocolBaseline = jsonStringify(S.request.toJSON());
        S.response = proto.response ? new ProtocolResponse(proto.kind, proto.response) : null;
        S.message = proto.message !== undefined && proto.message !== null ? new ProtocolMessage(proto.message) : null;
        return;
      }
      S.request = new Request({ id: r.id, name: r.name, method: r.method, url: r.url, header: (r.headers || []).map((h) => ({ key: h.key, value: h.value, disabled: h.enabled === false, description: h.description })), body: bodyFromCore(r.body) });
      S.requestBaseline = {
        url: S.request.url.toString(),
        body: JSON.stringify(S.request.body ? S.request.body.toJSON() : null),
        headers: JSON.stringify(S.request.headers.toJSON()),
        method: S.request.method,
      };
      S.response = input.response ? new Response(input.response, () => host.body()) : null;
    },
    beginScript(location, prefixLen) {
      S.location = location;
      S.prefixLen = prefixLen;
      S.legacyTests = {};
    },
    exec(fn) {
      const location = S.location;
      try {
        // Direct call (not fn.call): native calls leave the budgeted VM
        // loop that lets the worker interrupt runaway scripts.
        const r = fn();
        if (r && typeof r.then === 'function') r.then(null, (e) => recordError(location, errorMessage(e) + lineSuffix(e)));
      } catch (e) {
        recordError(location, errorMessage(e) + lineSuffix(e));
      }
    },
    endScript() {
      const t = S.legacyTests;
      if (t && typeof t === 'object') {
        for (const k of Object.keys(t)) S.tests.push({ name: k, passed: !!t[k], error: t[k] ? null : 'AssertionError: expected false to be truthy', skipped: false, pending: false });
      }
      S.legacyTests = {};
    },
    recordError,
    nextTimer() {
      if (!S || !S.timers.size) return -1;
      let due = Infinity;
      S.timers.forEach((t) => { if (t.due < due) due = t.due; });
      return Math.max(0, due - Date.now());
    },
    fireTimers() {
      const now = Date.now();
      const due = [];
      S.timers.forEach((t) => { if (t.due <= now) due.push(t); });
      due.sort((a, b) => a.due - b.due || a.id - b.id);
      for (const t of due) {
        if (!S.timers.has(t.id)) continue;
        if (t.interval) t.due = now + t.interval;
        else S.timers.delete(t.id);
        guard(() => t.fn(...t.args), t.location);
      }
    },
    clearTimers() { if (S) S.timers.clear(); },
    // `import x from 'lodash'` / CommonJS files in Package Library ES modules.
    importValue(spec, parent) {
      const value = requireModule(spec, parent || undefined);
      const names = isObject(value) || typeof value === 'function'
        ? Object.keys(value).filter((k) => k !== 'default' && /^[A-Za-z_$][\w$]*$/.test(k))
        : [];
      return { value, names };
    },
    finish() {
      let out = { request: null, variables: null, tests: [], console: [], errors: [], next_request: null, skip_request: false, visualization: null };
      try {
        const input = S.requestInput || {};
        try {
          if (S.protocol) {
            out.request = input;
            const cur = S.request && typeof S.request.toJSON === 'function' ? S.request.toJSON() : S.request;
            const text = jsonStringify(cur === undefined ? null : cur);
            out.protocol_request = text === S.protocolBaseline ? S.protocolInput : jsonParse(text);
          } else {
            const req = S.request instanceof Request ? S.request : new Request(S.request);
            normalizeLists(req);
            const base = S.requestBaseline;
            const url = req.url.toString();
            const bodyJson = JSON.stringify(req.body ? req.body.toJSON() : null);
            out.request = {
              id: input.id === undefined ? null : input.id,
              name: input.name === undefined ? null : input.name,
              method: req.method === base.method ? input.method : String(req.method),
              url: url === base.url ? input.url : url,
              headers: JSON.stringify(req.headers.toJSON()) === base.headers ? input.headers || [] : kvToCore(req.headers),
              body: bodyJson === base.body ? input.body || { mode: 'none' } : bodyToCore(req.body, input.body),
            };
          }
        } catch (e) {
          // Keep the tests, console and variables: send the request as it was.
          recordError('', 'Could not apply the changes to pm.request: ' + errorMessage(e));
          if (S.protocol) out.protocol_request = S.protocolInput;
          else {
            out.request = {
              id: input.id === undefined ? null : input.id,
              name: input.name === undefined ? null : input.name,
              method: input.method,
              url: input.url,
              headers: input.headers || [],
              body: input.body || { mode: 'none' },
            };
          }
        }
        out.variables = scopesSnapshot();
        out.tests = S.tests.map((t) => ({
          name: t.name,
          passed: t.pending ? false : t.passed,
          error: t.pending ? 'Error: test did not complete (async test never finished)' : t.error,
          skipped: t.skipped,
        }));
        out.console = S.console;
        out.errors = S.errors;
        out.next_request = S.next === undefined ? null : S.next;
        out.skip_request = S.skip;
        out.visualization = S.visualization;
        out = jsonStringify(out);
      } finally {
        S = null;
        // After collecting the results, which can run code from the job
        // (toJSON, getters).
        restoreGlobals();
      }
      return out;
    },
    // True (once) when a job changed a restored object in a way that cannot
    // be undone; the worker then rebuilds its context.
    takeTainted() {
      const t = tainted;
      tainted = false;
      return t;
    },
  };
  // Frozen and non-writable: user scripts are compiled as `__mp.exec(...)`,
  // so a replaced hook would see every later job of the worker.
  Object.freeze(mp);
  defProp(G, '__mp', { value: mp, writable: false, configurable: false, enumerable: false });

  // Built-in objects whose own properties are restored after every job.
  const INTRINSICS = [Object, Object.prototype, Function.prototype, Array, Array.prototype, String, String.prototype,
    Number, Number.prototype, Boolean.prototype, Symbol, JSON, Math, Date, Date.prototype, Promise, Promise.prototype,
    RegExp, RegExp.prototype, Error, Error.prototype, TypeError.prototype, Map.prototype, Set.prototype,
    WeakMap.prototype, Reflect, BigInt.prototype, Uint8Array.prototype];
  // The sandbox API and its classes live as long as the worker too, so a
  // patch such as `console.log = ...` or a change to a class prototype is
  // undone the same way.
  const API_OBJECTS = [pm, variables, test, pm.execution, pm.visualizer, postmanLegacy, consoleObj, cryptoObj,
    querystring, utilModule, assertModule, uuidModule, xml2js, requireFromScript];
  [VariableScope, Property, Header, QueryParam, FormParam, Cookie, PropertyList, Url, RequestBody, Request, Response,
    ProtocolMessage, MessageList, ProtocolRequest, ProtocolResponse, Buffer, Blob, File, TextEncoder, TextDecoder,
    URLSearchParams, URL, EventEmitter, AssertionError].forEach((C) => API_OBJECTS.push(C, C.prototype));
  let intrinsicBaseline = null;
  let tainted = false;

  // Adds lazily installed built-ins (polyfills) to the restored set.
  function trackBuiltins(objs) {
    if (intrinsicBaseline) for (const o of objs) intrinsicBaseline.push(snapshotEntry(o));
  }

  // Bundled libraries are only checked after jobs that used them: they are
  // large, and only reachable through `loadModule` / `chai()`.
  const libraries = Object.create(null);
  const usedLibraries = [];
  function trackLibrary(name, objs) {
    const entries = [];
    for (let i = 0; i < objs.length; i++) entries[i] = snapshotEntry(objs[i]);
    libraries[name] = { entries, used: false };
  }
  function markUsed(name) {
    const lib = libraries[name];
    if (lib && !lib.used) {
      lib.used = true;
      usedLibraries[usedLibraries.length] = lib;
    }
  }

  // A descriptor without a prototype, so `Object.prototype.get` and the like
  // cannot leak into `defineProperty`.
  function plainDesc(d) {
    setProto(d, null);
    return d;
  }

  // `===`, except that NaN (`Number.NaN`, `globalThis.NaN`) equals itself.
  const same = (a, b) => a === b || (a !== a && b !== b); // eslint-disable-line no-self-compare

  function sameDescriptor(a, b) {
    return same(a.value, b.value) && a.get === b.get && a.set === b.set && a.writable === b.writable;
  }

  // The restore code only uses built-ins captured when the prelude ran and
  // plain loops: scripts may have patched any method (`Map.prototype.forEach`,
  // the array iterator) of the objects being restored.
  function restoreOwn(entry) {
    const o = entry.o;
    if (getProto(o) !== entry.proto) {
      try { setProto(o, entry.proto); } catch (e) { tainted = true; }
    }
    const keys = ownKeys(o);
    for (let i = 0; i < keys.length; i++) {
      if (entry.index[keys[i]] === undefined) {
        try { delete o[keys[i]]; } catch (e) { tainted = true; }
      }
    }
    for (let i = 0; i < entry.names.length; i++) {
      const desc = entry.descs[i];
      const cur = getDesc(o, entry.names[i]);
      if (!cur || !sameDescriptor(cur, desc)) {
        try { defProp(o, entry.names[i], desc); } catch (e) { tainted = true; }
      }
    }
    if (entry.ext && !isExtensible(o)) tainted = true;
  }

  // Cheap check: same prototype, extensibility, number of own properties and
  // identical data values. Only objects that fail it are restored.
  function changed(entry) {
    const o = entry.o;
    if (getProto(o) !== entry.proto || isExtensible(o) !== entry.ext) return true;
    if (ownKeys(o).length !== entry.names.length) return true;
    const dataNames = entry.dataNames;
    const dataValues = entry.dataValues;
    for (let i = 0; i < dataNames.length; i++) {
      const v = o[dataNames[i]];
      // `!==`, except that NaN (`Number.NaN`, `globalThis.NaN`) equals itself.
      if (v !== dataValues[i] && (v === v || dataValues[i] === dataValues[i])) return true; // eslint-disable-line no-self-compare
    }
    const accessors = entry.accessors;
    for (let i = 0; i < accessors.length; i++) {
      const d = entry.descs[accessors[i]];
      const cur = getDesc(o, entry.names[accessors[i]]);
      if (!cur || cur.get !== d.get || cur.set !== d.set) return true;
    }
    return false;
  }

  function snapshotEntry(o) {
    const names = ownKeys(o);
    const descs = [];
    const index = Object.create(null);
    for (let i = 0; i < names.length; i++) {
      descs[i] = plainDesc(getDesc(o, names[i]));
      index[names[i]] = i;
    }
    return flatten({ o, names, descs, index, proto: getProto(o), ext: isExtensible(o) });
  }

  // Flat lists for `changed`, which runs for every object after every job.
  function flatten(entry) {
    entry.dataNames = [];
    entry.dataValues = [];
    entry.accessors = [];
    for (let i = 0; i < entry.names.length; i++) {
      const d = entry.descs[i];
      if (d.get === undefined && d.set === undefined) {
        entry.dataNames[entry.dataNames.length] = entry.names[i];
        entry.dataValues[entry.dataValues.length] = d.value;
      } else entry.accessors[entry.accessors.length] = i;
    }
    return entry;
  }

  function restoreGlobals() {
    if (!baseline) return;
    try {
      if (settled.length) applySettled();
      for (let i = 0; i < intrinsicBaseline.length; i++) {
        if (changed(intrinsicBaseline[i])) restoreOwn(intrinsicBaseline[i]);
      }
      for (let l = 0; l < usedLibraries.length; l++) {
        const lib = usedLibraries[l];
        lib.used = false;
        for (let i = 0; i < lib.entries.length; i++) {
          if (changed(lib.entries[i])) restoreOwn(lib.entries[i]);
        }
      }
      usedLibraries.length = 0;
      if (changed(baseline)) restoreOwn(baseline);
    } catch (e) {
      // A getter installed by the job threw.
      tainted = true;
    }
  }

  define(G, '__mp_snapshot', () => {
    delete G.__mp_snapshot;
    // Taken out of reach of scripts: chai is set up lazily, in some job.
    installChaiPostman = G.__mp_install_chai_postman;
    delete G.__mp_install_chai_postman;
    baseline = snapshotEntry(G);
    intrinsicBaseline = INTRINSICS.concat(API_OBJECTS).map(snapshotEntry);
  });
})(globalThis);
