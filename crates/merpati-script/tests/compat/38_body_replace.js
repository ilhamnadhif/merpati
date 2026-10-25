// @event: prerequest
// @request: {"method": "GET", "url": "https://x.test/", "headers": []}
// @expect: {"request": {"method": "POST", "body_mode": "raw", "body_raw": "{\"x\":1}", "url": "https://x.test/v2"}, "errors": 0}
pm.request.method = 'POST';
pm.request.body = { mode: 'raw', raw: JSON.stringify({ x: 1 }), options: { raw: { language: 'json' } } };
pm.request.url = 'https://x.test/v2';
