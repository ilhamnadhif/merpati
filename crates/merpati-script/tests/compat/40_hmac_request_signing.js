// @event: prerequest
// @request: {"method": "POST", "url": "{{baseUrl}}/orders?x=1", "headers": [{"key": "Content-Type", "value": "application/json"}], "body": {"mode": "raw", "raw": "{\"qty\":2}", "raw_language": "json"}}
// @variables: {"environment": {"baseUrl": "https://api.example.com", "apiSecret": "s3cr3t", "fixedTs": "1700000000"}}
// @expect: {"request": {"headers": {"X-Timestamp": "1700000000", "X-Signature": "e5e17b9b7ef146160444bb5907cd65c2356b767776f2966b226c6d6188fceaf1"}}, "errors": 0}
const ts = pm.environment.get('fixedTs');
const path = pm.request.url.getPathWithQuery();
const payload = [pm.request.method, path, ts, pm.request.body.raw].join('\n');
const signature = CryptoJS.HmacSHA256(payload, pm.environment.get('apiSecret')).toString(CryptoJS.enc.Hex);
pm.request.headers.upsert({ key: 'X-Timestamp', value: ts });
pm.request.headers.upsert({ key: 'X-Signature', value: signature });
console.log(payload);
