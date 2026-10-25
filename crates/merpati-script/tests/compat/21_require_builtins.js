// @event: prerequest
// @expect: {"tests": [["fs blocked", true], ["unknown", true], ["uuid", true], ["atob/btoa", true], ["node: prefix", true]], "errors": 0}
pm.test('fs blocked', () => {
    pm.expect(() => require('fs')).to.throw("Cannot find module 'fs'. Node.js built-in modules are not available in Merpati scripts.");
    pm.expect(() => require('child_process')).to.throw(/not available/);
});
pm.test('unknown', () => pm.expect(() => require('left-pad')).to.throw("Cannot find module 'left-pad'"));
pm.test('uuid', () => {
    const uuid = require('uuid');
    pm.expect(uuid.v4()).to.match(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    pm.expect(uuid()).to.have.lengthOf(36);
});
pm.test('atob/btoa', () => {
    pm.expect(require('btoa')('hi')).to.equal('aGk=');
    pm.expect(atob('aGk=')).to.equal('hi');
});
pm.test('node: prefix', () => pm.expect(() => require('node:net')).to.throw(/not available/));
