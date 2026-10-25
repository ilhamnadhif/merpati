// @event: prerequest
// @expect: {"environment": {"err": "connect ECONNREFUSED 127.0.0.1:1", "caught": "yes"}, "errors": 0}
pm.sendRequest('http://127.0.0.1:1/fail', (err, res) => {
    pm.environment.set('err', err ? err.message : 'none');
});
try {
    await pm.sendRequest('http://127.0.0.1:1/fail');
} catch (e) {
    pm.environment.set('caught', 'yes');
}
