// @event: prerequest
// @cookies: [{"name": "session", "value": "abc", "domain": "api.example.com", "path": "/", "secure": true, "http_only": true, "expires": null}, {"name": "theme", "value": "dark", "domain": "api.example.com", "path": "/", "secure": false, "http_only": false, "expires": 1900000000}]
// @expect: {"tests": [["cookies", true]], "environment": {"jar": "abc"}, "errors": 0}
pm.test('cookies', () => {
    pm.expect(pm.cookies.has('session')).to.be.true;
    pm.expect(pm.cookies.get('theme')).to.equal('dark');
    pm.expect(pm.cookies.toObject()).to.eql({ session: 'abc', theme: 'dark' });
    pm.expect(pm.cookies.count()).to.equal(2);
});
const jar = pm.cookies.jar();
jar.get('https://api.example.com', 'session', (err, value) => {
    pm.environment.set('jar', value);
});
