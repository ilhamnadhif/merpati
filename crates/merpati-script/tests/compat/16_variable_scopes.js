// @event: prerequest
// @variables: {"globals": {"host": "global", "g": "1"}, "collection": {"host": "collection", "c": "2"}, "environment": {"host": "env", "e": "3", "url": "https://{{host}}/{{path}}", "path": "v1"}, "data": {"host": "data", "d": 4}, "local": {}}
// @expect: {"tests": [["precedence", true], ["replaceIn", true], ["dynamic", true], ["has/unset", true]], "local": {"host": "local"}, "collection": {"c": null, "c2": "new"}, "errors": 0}
pm.test('precedence', () => {
    pm.expect(pm.variables.get('host')).to.equal('data');
    pm.variables.set('host', 'local');
    pm.expect(pm.variables.get('host')).to.equal('local');
    pm.expect(pm.globals.get('host')).to.equal('global');
    pm.expect(pm.collectionVariables.get('host')).to.equal('collection');
    pm.expect(pm.iterationData.get('d')).to.equal(4);
    pm.expect(pm.variables.toObject()).to.include({ g: '1', c: '2', e: '3', d: 4, host: 'local' });
});
pm.test('replaceIn', () => {
    pm.expect(pm.variables.replaceIn('{{url}}/users')).to.equal('https://local/v1/users');
    pm.expect(pm.environment.replaceIn('{{e}}-{{nope}}')).to.equal('3-{{nope}}');
});
pm.test('dynamic', () => {
    pm.expect(pm.variables.replaceIn('{{$guid}}')).to.match(/^[0-9a-f-]{36}$/);
    pm.expect(Number(pm.variables.replaceIn('{{$timestamp}}'))).to.be.above(1600000000);
    pm.expect(pm.variables.replaceIn('{{$randomEmail}}')).to.include('@');
});
pm.test('has/unset', () => {
    pm.expect(pm.collectionVariables.has('c')).to.be.true;
    pm.collectionVariables.unset('c');
    pm.expect(pm.collectionVariables.has('c')).to.be.false;
    pm.collectionVariables.set('c2', 'new');
    pm.expect(pm.variables.has('e')).to.be.true;
});
