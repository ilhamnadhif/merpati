// @response: {"code": 200, "status": "OK", "headers": [["Content-Type", "application/json"], ["X-Rate", "10"]], "body": "{\"id\": 5}", "responseTime": 30}
// @variables: {"environment": {"e1": "v1"}, "globals": {"g1": "gv"}}
// @expect: {"tests": [["Status code is 200", true], ["Body has id", true], ["Fast", true], ["Rate header", true], ["Env legacy read", true], ["fails", false]], "environment": {"legacyId": "5", "e1": null}, "globals": {"g2": "x"}, "errors": 0}
tests["Status code is 200"] = responseCode.code === 200;
var data = JSON.parse(responseBody);
tests["Body has id"] = data.id === 5;
tests["Fast"] = responseTime < 100;
tests["Rate header"] = responseHeaders["X-Rate"] === "10";
tests["Env legacy read"] = environment.e1 === "v1" && globals.g1 === "gv" && postman.getEnvironmentVariable("e1") === "v1";
tests["fails"] = false;
postman.setEnvironmentVariable("legacyId", data.id);
postman.clearEnvironmentVariable("e1");
postman.setGlobalVariable("g2", "x");
