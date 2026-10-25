// @event: prerequest
// @variables: {"data": {"username": "ada", "age": 36, "admin": true}}
// @info: {"request_name": "Login", "request_id": "r-9", "iteration": 2, "iteration_count": 5, "collection_name": "API"}
// @expect: {"tests": [["data", true], ["info", true]], "errors": 0}
pm.test('data', () => {
    pm.expect(pm.iterationData.get('username')).to.equal('ada');
    pm.expect(pm.iterationData.get('age')).to.equal(36);
    pm.expect(pm.iterationData.toObject()).to.eql({ username: 'ada', age: 36, admin: true });
    pm.expect(data.username).to.equal('ada');
    pm.expect(pm.variables.get('admin')).to.equal(true);
});
pm.test('info', () => {
    pm.expect(pm.info.iteration).to.equal(2);
    pm.expect(pm.info.iterationCount).to.equal(5);
    pm.expect(pm.info.requestName).to.equal('Login');
    pm.expect(pm.info.requestId).to.equal('r-9');
    pm.expect(pm.info.eventName).to.equal('prerequest');
    pm.expect(iteration).to.equal(2);
});
