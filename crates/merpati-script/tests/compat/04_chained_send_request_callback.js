// @event: prerequest
// @variables: {"environment": {"authUrl": "https://auth.example.com/token", "clientId": "cid"}}
// @expect: {"environment": {"accessToken": "tok-123", "echoedMethod": "POST", "echoedForm": "cid"}, "errors": 0, "console_contains": ["token fetched"]}
pm.sendRequest({
    url: pm.environment.get("authUrl"),
    method: 'POST',
    header: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: { mode: 'urlencoded', urlencoded: [{ key: 'grant_type', value: 'client_credentials' }, { key: 'client_id', value: '{{clientId}}' }] }
}, function (err, res) {
    if (err) { throw err; }
    pm.environment.set("accessToken", res.json().access_token);
    console.log("token fetched", res.code);
});
pm.sendRequest({ url: 'https://x.test/echo', method: 'post', body: { mode: 'urlencoded', urlencoded: [{ key: 'client_id', value: pm.environment.get('clientId') }] } }, (err, res) => {
    const j = res.json();
    pm.environment.set('echoedMethod', j.method);
    pm.environment.set('echoedForm', j.form.client_id);
});
