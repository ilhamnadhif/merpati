// @event: prerequest
// @request: {"method": "POST", "url": "https://x.test/form", "headers": [], "body": {"mode": "urlencoded", "urlencoded": [{"key": "a", "value": "1"}, {"key": "b", "value": "2", "enabled": false}]}}
// @expect: {"tests": [["body", true]], "request": {"urlencoded": {"a": "1", "c": "3"}, "body_mode": "urlencoded"}, "errors": 0}
pm.test('body', () => {
    pm.expect(pm.request.body.mode).to.equal('urlencoded');
    pm.expect(pm.request.body.urlencoded.get('a')).to.equal('1');
    pm.expect(pm.request.body.urlencoded.toObject(true)).to.eql({ a: '1' });
    pm.expect(pm.request.body.toString()).to.equal('a=1');
});
pm.request.body.urlencoded.remove('b');
pm.request.body.urlencoded.add({ key: 'c', value: '3' });
