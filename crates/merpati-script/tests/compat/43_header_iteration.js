// @response: {"code": 200, "headers": [["Content-Type", "application/json"], ["Cache-Control", "no-store"], ["X-Frame-Options", "DENY"], ["Strict-Transport-Security", "max-age=31536000"]], "body": "{}"}
// @expect: {"tests": [["security headers", true], ["header count", true]], "environment": {"names": "content-type,cache-control,x-frame-options,strict-transport-security"}, "errors": 0}
const names = [];
pm.response.headers.each((h) => names.push(h.key.toLowerCase()));
pm.environment.set('names', names.join(','));
pm.test('security headers', () => {
    ['Cache-Control', 'X-Frame-Options', 'Strict-Transport-Security'].forEach((h) => pm.response.to.have.header(h));
    pm.expect(pm.response.headers.get('cache-control')).to.include('no-store');
    pm.expect(postman.getResponseHeader('X-Frame-Options')).to.equal('DENY');
});
pm.test('header count', () => pm.expect(pm.response.headers.count()).to.equal(4));
