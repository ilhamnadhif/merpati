// @event: prerequest
// @expect: {"environment": {"a": "tok-123", "b": "true", "c": "404"}, "errors": 0}
const r1 = await pm.sendRequest('https://auth.example.com/token');
pm.environment.set('a', r1.json().access_token);
const r2 = await pm.sendRequest({ url: 'https://x.test/anything', method: 'GET' });
pm.environment.set('b', r2.json().ok);
const r3 = await pm.sendRequest('https://x.test/status/404');
pm.environment.set('c', r3.code);
