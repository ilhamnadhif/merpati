// @event: prerequest
// @variables: {"globals": {"a": "1", "b": "2"}, "environment": {"e": "x", "f": "y"}}
// @expect: {"tests": [["toObject", true]], "globals": {"a": null, "b": null, "fresh": "1"}, "environment": {"e": "x", "f": null}, "errors": 0}
pm.test('toObject', () => {
    pm.expect(pm.environment.toObject()).to.eql({ e: 'x', f: 'y' });
    pm.expect(Object.keys(pm.globals.toObject())).to.have.members(['a', 'b']);
});
pm.globals.clear();
pm.globals.set('fresh', 1);
pm.environment.unset('f');
