// @response: {"code": 200, "body": "{}"}
// @expect: {"tests": [["sync", true], ["async ok", true], ["async fail", false], ["done ok", true], ["done fail", false], ["timer", true]], "test_errors": {"async fail": "expected 1 to equal 2", "done fail": "boom"}, "errors": 0, "max_ms": 2000}
pm.test('sync', () => {});
pm.test('async ok', async () => {
    const r = await pm.sendRequest('https://x.test/token');
    pm.expect(r.code).to.equal(200);
});
pm.test('async fail', async () => {
    await Promise.resolve();
    pm.expect(1).to.equal(2);
});
pm.test('done ok', (done) => { setTimeout(() => done(), 10); });
pm.test('done fail', (done) => { setTimeout(() => done(new Error('boom')), 5); });
setTimeout(() => {
    pm.test('timer', () => pm.expect(true).to.be.true);
}, 50);
