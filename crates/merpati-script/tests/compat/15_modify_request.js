// @event: prerequest
// @request: {"id": "r1", "name": "Create", "method": "POST", "url": "{{baseUrl}}/users?draft=true", "headers": [{"key": "Content-Type", "value": "application/json"}, {"key": "X-Remove", "value": "1"}], "body": {"mode": "raw", "raw": "{\"name\": \"x\"}", "raw_language": "json"}}
// @variables: {"environment": {"baseUrl": "https://api.example.com"}}
// @expect: {"request": {"url": "{{baseUrl}}/users?draft=true&ts=1", "headers": {"X-Trace": "t-1", "X-Remove": null, "Content-Type": "application/json; charset=utf-8"}, "body_raw": "{\"name\":\"x\",\"signed\":true}", "body_mode": "raw"}, "errors": 0}
pm.request.headers.add({ key: 'X-Trace', value: 't-1' });
pm.request.headers.remove('x-remove');
pm.request.headers.upsert({ key: 'Content-Type', value: 'application/json; charset=utf-8' });
pm.request.url.query.add({ key: 'ts', value: '1' });
const body = JSON.parse(pm.request.body.raw);
body.signed = true;
pm.request.body.raw = JSON.stringify(body);
if (pm.request.url.getHost() !== '{{baseUrl}}') throw new Error('host ' + pm.request.url.getHost());
if (pm.request.method !== 'POST') throw new Error('method');
