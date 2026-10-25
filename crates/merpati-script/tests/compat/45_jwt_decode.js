// @response: {"code": 200, "body": "{\"access_token\": \"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyLCJyb2xlcyI6WyJhZG1pbiJdfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"}"}
// @expect: {"tests": [["jwt claims", true]], "environment": {"sub": "1234567890"}, "errors": 0}
function decodeJwt(token) {
    const payload = token.split('.')[1].replace(/-/g, '+').replace(/_/g, '/');
    return JSON.parse(CryptoJS.enc.Utf8.stringify(CryptoJS.enc.Base64.parse(payload)));
}
const claims = decodeJwt(pm.response.json().access_token);
pm.environment.set('sub', claims.sub);
pm.test('jwt claims', () => {
    pm.expect(claims.name).to.equal('John Doe');
    pm.expect(claims.roles).to.include('admin');
    pm.expect(JSON.parse(atob(pm.response.json().access_token.split('.')[0])).alg).to.equal('HS256');
    pm.expect(JSON.parse(Buffer.from(pm.response.json().access_token.split('.')[1], 'base64').toString()).iat).to.equal(1516239022);
});
