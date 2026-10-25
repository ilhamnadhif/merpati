// @event: prerequest
// @variables: {"environment": {"user": "ada", "pass": "s3cr3t"}}
// @expect: {"request": {"headers": {"Authorization": "Basic YWRhOnMzY3IzdA=="}}, "tests": [["buffer", true], ["utf8", true], ["url", true]], "errors": 0}
const token = Buffer.from(pm.environment.get('user') + ':' + pm.environment.get('pass')).toString('base64');
pm.request.headers.upsert({ key: 'Authorization', value: 'Basic ' + token });
pm.test('buffer', () => {
    pm.expect(Buffer.from('aGVsbG8=', 'base64').toString()).to.equal('hello');
    pm.expect(Buffer.from('hello').toString('hex')).to.equal('68656c6c6f');
    pm.expect(Buffer.from('68656c6c6f', 'hex').toString('utf8')).to.equal('hello');
    pm.expect(Buffer.byteLength('héllo')).to.equal(6);
});
pm.test('utf8', () => {
    pm.expect(Buffer.from('😀 é').toString()).to.equal('😀 é');
    pm.expect(new TextDecoder().decode(new TextEncoder().encode('ünï'))).to.equal('ünï');
    pm.expect(encodeURIComponent('a b&c')).to.equal('a%20b%26c');
});
pm.test('url', () => {
    const u = new URL('https://user:pw@example.com:8443/a/b?x=1&y=two#h');
    pm.expect(u.hostname).to.equal('example.com');
    pm.expect(u.port).to.equal('8443');
    pm.expect(u.searchParams.get('y')).to.equal('two');
    u.searchParams.append('z', '3');
    pm.expect(u.toString()).to.equal('https://user:pw@example.com:8443/a/b?x=1&y=two&z=3#h');
    pm.expect(new URLSearchParams({ a: '1 2' }).toString()).to.equal('a=1+2');
});
