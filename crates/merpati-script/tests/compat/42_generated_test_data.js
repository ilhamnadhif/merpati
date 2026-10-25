// @event: prerequest
// @expect: {"tests": [["data generated", true]], "environment": {"newUser": "~@"}, "errors": 0}
const user = {
    firstName: pm.variables.replaceIn('{{$randomFirstName}}'),
    lastName: pm.variables.replaceIn('{{$randomLastName}}'),
    email: pm.variables.replaceIn('{{$randomEmail}}'),
    age: _.random(18, 99),
    id: pm.variables.replaceIn('{{$randomUUID}}'),
};
pm.environment.set('newUser', JSON.stringify(user));
pm.test('data generated', () => {
    pm.expect(user.firstName).to.be.a('string').that.is.not.empty;
    pm.expect(user.age).to.be.within(18, 99);
    pm.expect(user.id).to.match(/^[0-9a-f-]{36}$/);
});
