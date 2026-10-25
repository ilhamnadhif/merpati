// @response: {"code": 200, "status": "OK", "headers": [["Content-Type", "application/json; charset=utf-8"], ["Content-Length", "13"]], "body": "{\"ok\": true}"}
// @expect: {"tests": [["helpers", true]], "errors": 0}
pm.test('helpers', () => {
    pm.expect(pm.response.reason()).to.equal('OK');
    pm.expect(pm.response.headers.toObject()).to.have.property('content-type');
    pm.expect(pm.response.headers.has('CONTENT-TYPE')).to.be.true;
    pm.expect(pm.response.headers.get('content-length')).to.equal('13');
    pm.expect(pm.response.text()).to.equal('{"ok": true}');
    pm.expect(pm.response.json()).to.eql({ ok: true });
    pm.expect(pm.response.contentInfo().mimeType).to.equal('application');
    pm.expect(pm.response.size().total).to.be.above(0);
    pm.expect(pm.response.stream.toString()).to.equal('{"ok": true}');
    pm.expect(pm.response.dataURI()).to.match(/^data:application\/json;base64,/);
    pm.expect(JSON.parse(JSON.stringify(pm.response)).code).to.equal(200);
});
