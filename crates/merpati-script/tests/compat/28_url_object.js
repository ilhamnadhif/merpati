// @event: prerequest
// @request: {"method": "GET", "url": "https://api.example.com:8443/v1/users/:id?active=true&sort=name#frag", "headers": []}
// @expect: {"tests": [["url parts", true]], "request": {"url": "https://api.example.com:8443/v1/users/:id?active=true&sort=name#frag"}, "errors": 0}
pm.test('url parts', () => {
    const u = pm.request.url;
    pm.expect(u.protocol).to.equal('https');
    pm.expect(u.getHost()).to.equal('api.example.com');
    pm.expect(u.port).to.equal('8443');
    pm.expect(u.getPath()).to.equal('/v1/users/:id');
    pm.expect(u.path).to.eql(['v1', 'users', ':id']);
    pm.expect(u.getQueryString()).to.equal('active=true&sort=name');
    pm.expect(u.query.get('sort')).to.equal('name');
    pm.expect(u.query.toObject()).to.eql({ active: 'true', sort: 'name' });
    pm.expect(u.getRemote()).to.equal('api.example.com:8443');
    pm.expect(u.getPathWithQuery()).to.equal('/v1/users/:id?active=true&sort=name');
    pm.expect(u.toString()).to.equal('https://api.example.com:8443/v1/users/:id?active=true&sort=name#frag');
    pm.expect(u.hash).to.equal('frag');
});
