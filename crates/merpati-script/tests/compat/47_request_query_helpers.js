// @event: prerequest
// @request: {"method": "GET", "url": "https://api.example.com/search?q=old&debug=1", "headers": [{"key": "Accept", "value": "application/json"}, {"key": "X-Off", "value": "1", "enabled": false}]}
// @expect: {"request": {"url": "https://api.example.com/search?q=old&page=2&size=50"}, "tests": [["headers", true]], "errors": 0}
pm.request.addQueryParams('page=2&size=50');
pm.request.removeQueryParams('debug');
pm.test('headers', () => {
    pm.expect(pm.request.getHeaders()).to.eql({ Accept: 'application/json', 'X-Off': '1' });
    pm.expect(pm.request.getHeaders({ enabled: true })).to.eql({ Accept: 'application/json' });
    pm.expect(pm.request.headers.toObject(true)).to.eql({ accept: 'application/json' });
    pm.expect(pm.request.headers.has('x-off')).to.be.true;
    pm.expect(pm.request.headers.get('x-off')).to.be.undefined;
    pm.request.to.have.header('Accept');
});
