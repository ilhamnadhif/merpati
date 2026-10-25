// @event: prerequest
// @variables: {"globals": {"utils": "(function () { return { greet: function (n) { return 'hi ' + n; }, add: (a, b) => a + b }; })()"}}
// @expect: {"environment": {"greeting": "hi ada", "sum": "5"}, "errors": 0}
const utils = eval(pm.globals.get('utils'));
pm.environment.set('greeting', utils.greet('ada'));
pm.environment.set('sum', utils.add(2, 3));
