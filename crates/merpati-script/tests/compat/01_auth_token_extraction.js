// @response: {"code": 200, "status": "OK", "headers": [["Content-Type", "application/json; charset=utf-8"]], "body": "{\"data\": {\"token\": \"abc.def.ghi\", \"user\": {\"id\": 7, \"roles\": [\"admin\", \"dev\"]}}}"}
// @variables: {"environment": {"baseUrl": "https://api.example.com"}, "environment_name": "Staging"}
// @expect: {"tests": [["Status code is 200", true], ["Response has a token", true]], "environment": {"token": "abc.def.ghi", "userId": "7", "roles": "[\"admin\",\"dev\"]", "baseUrl": "https://api.example.com"}, "errors": 0}
pm.test("Status code is 200", function () {
    pm.response.to.have.status(200);
});

const jsonData = pm.response.json();
pm.test("Response has a token", () => {
    pm.expect(jsonData.data).to.have.property('token');
    pm.expect(jsonData.data.token).to.be.a('string').and.not.be.empty;
});
pm.environment.set("token", jsonData.data.token);
pm.environment.set("userId", jsonData.data.user.id);
pm.environment.set("roles", jsonData.data.user.roles);
if (pm.environment.name !== 'Staging') throw new Error('wrong env name ' + pm.environment.name);
