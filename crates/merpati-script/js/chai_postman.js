// Postman-style response/request assertions as a chai plugin, so that
// `pm.response.to.have.status(200)` and friends behave like chai-postman.
(function (G) {
  'use strict';

  Object.defineProperty(G, '__mp_install_chai_postman', {
    configurable: true,
    writable: true,
    enumerable: false,
    value: function install(chai, deps) {
      const A = chai.Assertion;
      const flag = chai.util.flag;

      const isProtocolResponse = (o) => !!deps.ProtocolResponse && o instanceof deps.ProtocolResponse;
      const isProtocolRequest = (o) => !!deps.ProtocolRequest && o instanceof deps.ProtocolRequest;
      const isResponse = (o) => o instanceof deps.Response || isProtocolResponse(o) || (!!o && typeof o.code === 'number' && !!o.headers && typeof o.text === 'function');
      const isRequest = (o) => o instanceof deps.Request || isProtocolRequest(o) || (!!o && typeof o.method === 'string' && !!o.url && !!o.headers && !isResponse(o));
      const kind = (o) => (isRequest(o) ? 'request' : 'response');

      function requireMessage(a, allowRequest) {
        const o = a._obj;
        const ok = isResponse(o) || (allowRequest && isRequest(o));
        if (!ok) {
          throw new chai.AssertionError(
            'expecting a ' + (allowRequest ? 'request or response' : 'response') + ' object but got ' + chai.util.objDisplay(o),
            undefined,
            A
          );
        }
        return o;
      }

      function bodyText(o) {
        if (typeof o.text === 'function') return o.text();
        if (o.body && typeof o.body.toString === 'function') return o.body.toString();
        return '';
      }

      A.addProperty('postmanResponse', function () {
        this.assert(isResponse(this._obj), 'expecting a postman response object but got #{this}', 'not expecting a postman response object');
      });
      A.addProperty('postmanRequest', function () {
        this.assert(isRequest(this._obj), 'expecting a postman request object but got #{this}', 'not expecting a postman request object');
      });
      A.addProperty('postmanRequestOrResponse', function () {
        this.assert(isRequest(this._obj) || isResponse(this._obj), 'expecting a postman request or response object but got #{this}', 'not expecting a postman request or response object');
      });

      // Status code classes: 1XX .. 5XX.
      const classes = { info: 1, success: 2, redirection: 3, clientError: 4, serverError: 5 };
      Object.keys(classes).forEach((name) => {
        const cls = classes[name];
        A.addProperty(name, function () {
          const r = requireMessage(this);
          this.assert(Math.floor(r.code / 100) === cls, 'expected response code to be ' + cls + 'XX but found #{act}', 'expected response code to not be ' + cls + 'XX', cls + 'XX', r.code);
        });
      });
      A.addProperty('error', function () {
        const r = requireMessage(this);
        const c = Math.floor(r.code / 100);
        this.assert(c === 4 || c === 5, 'expected response code to be 4XX or 5XX but found #{act}', 'expected response code to not be 4XX or 5XX', '4XX or 5XX', r.code);
      });

      const exact = {
        accepted: 202, withoutContent: 204, badRequest: 400, unauthorised: 401, unauthorized: 401, forbidden: 403,
        notFound: 404, notAcceptable: 406, rateLimited: 429,
      };
      Object.keys(exact).forEach((name) => {
        const code = exact[name];
        A.addProperty(name, function () {
          const r = requireMessage(this);
          this.assert(r.code === code, 'expected response code to be ' + code + ' but found #{act}', 'expected response code to not be ' + code, code, r.code);
        });
      });
      // `ok` already exists in chai (truthiness); only change it for responses.
      A.overwriteProperty('ok', function (_super) {
        return function () {
          if (isProtocolResponse(this._obj)) {
            const r = this._obj;
            const shown = r.statusCode !== undefined ? 'status code ' + r.statusCode : (r.isError ? 'an error result' : 'an error');
            this.assert(r.isOk(), 'expected response to be ok but it has ' + shown, 'expected response to not be ok');
          } else if (isResponse(this._obj)) {
            const r = this._obj;
            this.assert(r.code === 200, 'expected response code to be 200 but found #{act}', 'expected response code to not be 200', 200, r.code);
          } else {
            _super.call(this);
          }
        };
      });

      function statusAssert(code) {
        const r = requireMessage(this);
        if (isProtocolResponse(r) && typeof code !== 'string') {
          const actual = r.statusCode === undefined ? undefined : Number(r.statusCode);
          // Spelled out: chai would print a status of 0 as "+0".
          this.assert(actual === Number(code), 'expected response to have status code ' + Number(code) + ' but got ' + actual, 'expected response to not have status code ' + actual, Number(code), actual);
          return;
        }
        if (typeof code === 'string') {
          this.assert(r.status === code, 'expected response to have status reason #{exp} but got #{act}', 'expected response to not have status reason #{act}', code, r.status);
        } else {
          this.assert(r.code === Number(code), 'expected response to have status code #{exp} but got #{act}', 'expected response to not have status code #{act}', Number(code), r.code);
        }
      }
      A.addMethod('status', statusAssert);
      A.addMethod('statusCode', function (code) { statusAssert.call(this, Number(code)); });
      A.addMethod('code', function (code) { statusAssert.call(this, Number(code)); });
      A.addMethod('statusReason', function (reason) { statusAssert.call(this, String(reason)); });
      A.addMethod('statusCodeClass', function (cls) {
        const r = requireMessage(this);
        this.assert(Math.floor(r.code / 100) === Number(cls), 'expected response code to be #{exp}XX but found #{act}', 'expected response code to not be #{exp}XX', Number(cls), r.code);
      });

      // header / metadata / trailer: `(key)` checks presence, `(key, value)` the value (string or RegExp).
      function listAssert(a, list, what, key, value, withValue) {
        const o = a._obj;
        const who = kind(o);
        const has = !!list && list.has(key);
        if (!withValue) {
          a.assert(has, 'expected ' + who + ' to have ' + what + " with key '" + key + "'", 'expected ' + who + ' to not have ' + what + " with key '" + key + "'", key);
          return;
        }
        const actual = has ? list.get(key) : undefined;
        const matches = has && (value instanceof RegExp ? value.test(String(actual)) : String(actual) === String(value));
        a.assert(matches, "expected '" + key + "' " + who + ' ' + what + " to be '" + value + "' but got '" + actual + "'", "expected '" + key + "' " + who + ' ' + what + " to not be '" + value + "'", value, actual);
      }

      A.addMethod('header', function (key, value) {
        const o = requireMessage(this, true);
        listAssert(this, o.headers || o.metadata, 'header', key, value, arguments.length >= 2);
      });

      A.addMethod('metadata', function (key, value) {
        const o = requireMessage(this, true);
        listAssert(this, o.metadata, 'metadata', key, value, arguments.length >= 2);
      });

      A.addMethod('trailer', function (key, value) {
        const o = requireMessage(this);
        listAssert(this, o.trailers, 'trailer', key, value, arguments.length >= 2);
      });

      A.addMethod('headers', function (obj) {
        const o = requireMessage(this, true);
        const list = o.headers || o.metadata;
        Object.keys(obj || {}).forEach((k) => {
          const actual = list ? list.get(k) : undefined;
          this.assert(String(actual) === String(obj[k]), "expected '" + k + "' header to be '" + obj[k] + "' but got '" + actual + "'", "expected '" + k + "' header to not be '" + obj[k] + "'");
        });
      });

      A.addProperty('withBody', function () {
        const o = requireMessage(this, true);
        const text = bodyText(o);
        this.assert(!!text, 'expected ' + kind(o) + ' to have content in body', 'expected ' + kind(o) + ' to not have content in body');
      });

      function mime(o) {
        const ct = String((o.headers && o.headers.get('content-type')) || '').toLowerCase();
        return ct.split(';')[0].trim();
      }
      [['json', /json/], ['html', /html/], ['xml', /xml/], ['text', /^text\//]].forEach((pair) => {
        A.addProperty(pair[0], function () {
          const o = requireMessage(this, true);
          const m = mime(o);
          this.assert(pair[1].test(m), 'expected ' + kind(o) + " to be '" + pair[0] + "' but found '" + (m || 'unknown') + "'", 'expected ' + kind(o) + " to not be '" + pair[0] + "'");
        });
      });

      A.addMethod('body', function (expected) {
        const o = requireMessage(this, true);
        const text = bodyText(o);
        const who = kind(o);
        if (arguments.length === 0) {
          this.assert(!!text, 'expected ' + who + ' to have content in body', 'expected ' + who + ' to not have content in body');
        } else if (expected instanceof RegExp) {
          this.assert(expected.test(text), 'expected ' + who + ' body text #{act} to match #{exp}', 'expected ' + who + ' body text #{act} to not match #{exp}', expected, text);
        } else if (typeof expected === 'string') {
          this.assert(text === expected, 'expected ' + who + ' body to equal #{exp} but got #{act}', 'expected ' + who + ' body to not equal #{exp}', expected, text);
        } else {
          let json;
          try { json = JSON.parse(text); } catch (e) { json = undefined; }
          this.assert(chai.util.eql(json, expected), 'expected ' + who + ' body json to equal #{exp} but got #{act}', 'expected ' + who + ' body json to not equal #{exp}', expected, json, true);
        }
      });

      function getPath(obj, path) {
        const parts = String(path).replace(/\[(\w+)\]/g, '.$1').replace(/^\./, '').split('.').filter((p) => p !== '');
        let cur = obj;
        for (const p of parts) {
          if (cur === null || cur === undefined || !Object.prototype.hasOwnProperty.call(Object(cur), p)) return { found: false };
          cur = cur[p];
        }
        return { found: true, value: cur };
      }

      A.addMethod('jsonBody', function (path, value) {
        const o = requireMessage(this, true);
        let json, err;
        try { json = JSON.parse(bodyText(o)); } catch (e) { err = e; }
        const who = kind(o);
        if (arguments.length === 0) {
          this.assert(!err, 'expected ' + who + ' body to be a valid json but got error #{act}', 'expected ' + who + ' body to not be a valid json', undefined, err && err.message);
          return;
        }
        if (err) {
          this.assert(flag(this, 'negate'), 'expected ' + who + ' body to be a valid json but got error ' + err.message, '');
          return;
        }
        if (typeof path !== 'string') {
          this.assert(chai.util.eql(json, path), 'expected ' + who + ' body json to equal #{exp} but got #{act}', 'expected ' + who + ' body json to not equal #{exp}', path, json, true);
          return;
        }
        const at = getPath(json, path);
        if (arguments.length === 1) {
          this.assert(at.found, 'expected #{act} in ' + who + " to contain property '" + path + "'", 'expected #{act} in ' + who + " to not contain property '" + path + "'", path, json);
        } else {
          this.assert(at.found && chai.util.eql(at.value, value), 'expected ' + who + " body json at '" + path + "' to contain #{exp} but got #{act}", 'expected ' + who + " body json at '" + path + "' to not contain #{exp}", value, at.value, true);
        }
      });

      A.addMethod('jsonSchema', function (schema) {
        let data = this._obj;
        if (isResponse(data) || isRequest(data)) {
          try { data = JSON.parse(bodyText(data)); } catch (e) { data = undefined; }
        }
        const res = deps.tv4().validateMultiple(data, schema, false, false);
        const errors = (res.errors || []).map((e) => 'data' + (e.dataPath || '') + ' ' + e.message);
        this.assert(res.valid, 'expected data to satisfy schema but found following errors: \n' + errors.join(', '), 'expected data to not satisfy schema', schema, data);
      });

      A.addMethod('cookie', function (name, value) {
        const r = requireMessage(this);
        const c = r.cookies.one(name);
        if (arguments.length < 2) {
          this.assert(!!c, "expected response to have cookie '" + name + "'", "expected response to not have cookie '" + name + "'");
        } else {
          const actual = c ? c.value : undefined;
          this.assert(!!c && actual === value, "expected cookie '" + name + "' to have value '" + value + "' but got '" + actual + "'", "expected cookie '" + name + "' to not have value '" + value + "'");
        }
      });

      A.addChainableMethod('responseTime', function (value) {
        this.assert(this._obj === value, 'expected response to have response time #{exp} but got #{act}', 'expected response to not have response time #{act}', value, this._obj);
      }, function () {
        const r = requireMessage(this);
        flag(this, 'object', r.responseTime);
      });

      A.addChainableMethod('responseSize', function (value) {
        this.assert(this._obj === value, 'expected response to have response size #{exp} but got #{act}', 'expected response to not have response size #{act}', value, this._obj);
      }, function () {
        const r = requireMessage(this);
        flag(this, 'object', r.responseSize);
      });

      A.addMethod('method', function (m) {
        const o = this._obj;
        this.assert(isRequest(o) && String(o.method).toUpperCase() === String(m).toUpperCase(), "expected request to have method '" + m + "' but got '" + (o && o.method) + "'", "expected request to not have method '" + m + "'");
      });

      A.addMethod('url', function (u) {
        const o = this._obj;
        const actual = o && o.url ? o.url.toString() : undefined;
        this.assert(isRequest(o) && actual === String(u), "expected request to have url '" + u + "' but got '" + actual + "'", "expected request to not have url '" + u + "'");
      });
    },
  });
})(globalThis);
