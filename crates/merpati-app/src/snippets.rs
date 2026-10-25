//! Ready-made script snippets (FR-S6).

pub struct Snippet {
    pub title: &'static str,
    pub code: &'static str,
    /// Only relevant after a response.
    pub post_only: bool,
}

pub const SNIPPETS: &[Snippet] = &[
    Snippet {
        title: "Status code is 200",
        code: "pm.test(\"Status code is 200\", function () {\n    pm.response.to.have.status(200);\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Response time is below 500ms",
        code: "pm.test(\"Response time is less than 500ms\", function () {\n    pm.expect(pm.response.responseTime).to.be.below(500);\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Response body: JSON value check",
        code: "pm.test(\"Your test name\", function () {\n    const jsonData = pm.response.json();\n    pm.expect(jsonData.value).to.eql(100);\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Response body: contains string",
        code: "pm.test(\"Body matches string\", function () {\n    pm.expect(pm.response.text()).to.include(\"string_you_want_to_search\");\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Content-Type header is present",
        code: "pm.test(\"Content-Type is present\", function () {\n    pm.response.to.have.header(\"Content-Type\");\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Successful POST request",
        code: "pm.test(\"Successful POST request\", function () {\n    pm.expect(pm.response.code).to.be.oneOf([201, 202]);\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Use Tiny Validator for JSON data",
        code: "const schema = {\n    type: \"object\",\n    required: [\"id\"],\n    properties: { id: { type: \"number\" } }\n};\n\npm.test(\"Schema is valid\", function () {\n    pm.response.to.have.jsonSchema(schema);\n});\n",
        post_only: true,
    },
    Snippet {
        title: "Save token from response to environment",
        code: "const json = pm.response.json();\npm.environment.set(\"token\", json.token);\n",
        post_only: true,
    },
    Snippet {
        title: "Set an environment variable",
        code: "pm.environment.set(\"variable_key\", \"variable_value\");\n",
        post_only: false,
    },
    Snippet {
        title: "Get an environment variable",
        code: "const value = pm.environment.get(\"variable_key\");\n",
        post_only: false,
    },
    Snippet {
        title: "Set a global variable",
        code: "pm.globals.set(\"variable_key\", \"variable_value\");\n",
        post_only: false,
    },
    Snippet {
        title: "Set a collection variable",
        code: "pm.collectionVariables.set(\"variable_key\", \"variable_value\");\n",
        post_only: false,
    },
    Snippet {
        title: "Clear an environment variable",
        code: "pm.environment.unset(\"variable_key\");\n",
        post_only: false,
    },
    Snippet {
        title: "Add a request header",
        code: "pm.request.headers.add({ key: \"X-Request-Id\", value: pm.variables.replaceIn(\"{{$guid}}\") });\n",
        post_only: false,
    },
    Snippet {
        title: "Send a request",
        code: "pm.sendRequest(\"https://postman-echo.com/get\", function (err, response) {\n    console.log(response.json());\n});\n",
        post_only: false,
    },
    Snippet {
        title: "HMAC signature with CryptoJS",
        code: "const signature = CryptoJS.HmacSHA256(pm.request.url.toString(), pm.environment.get(\"secret\")).toString(CryptoJS.enc.Hex);\npm.request.headers.upsert({ key: \"X-Signature\", value: signature });\n",
        post_only: false,
    },
    Snippet { title: "Skip this request", code: "pm.execution.skipRequest();\n", post_only: false },
    Snippet {
        title: "Set next request (runner)",
        code: "pm.execution.setNextRequest(\"Request name\");\n",
        post_only: false,
    },
    Snippet { title: "Stop the run", code: "pm.execution.setNextRequest(null);\n", post_only: false },
    Snippet {
        title: "Visualize response as a table",
        code: "const template = `<table>{{#each items}}<tr><td>{{name}}</td></tr>{{/each}}</table>`;\npm.visualizer.set(template, { items: pm.response.json() });\n",
        post_only: true,
    },
];

/// Members offered after `pm.` in the script editor.
pub const PM_COMPLETIONS: &[(&str, &str)] = &[
    ("pm.test(name, fn)", "Define a test"),
    ("pm.expect(value)", "Chai BDD assertion"),
    ("pm.response.json()", "Parse the response body as JSON"),
    ("pm.response.text()", "Response body as text"),
    ("pm.response.code", "Status code"),
    ("pm.response.status", "Status text"),
    ("pm.response.headers.get(name)", "Response header"),
    ("pm.response.responseTime", "Response time in ms"),
    ("pm.response.to.have.status(200)", "Status assertion"),
    ("pm.request.headers.add({key, value})", "Add a request header"),
    ("pm.request.url.toString()", "Request URL"),
    ("pm.environment.get(key)", "Read an environment variable"),
    ("pm.environment.set(key, value)", "Write an environment variable"),
    ("pm.collectionVariables.get(key)", "Read a collection variable"),
    ("pm.collectionVariables.set(key, value)", "Write a collection variable"),
    ("pm.globals.get(key)", "Read a global variable"),
    ("pm.globals.set(key, value)", "Write a global variable"),
    ("pm.variables.get(key)", "Resolve a variable across scopes"),
    ("pm.variables.set(key, value)", "Set a local variable"),
    ("pm.variables.replaceIn(text)", "Resolve {{variables}} in text"),
    ("pm.iterationData.get(key)", "Data file value"),
    ("pm.sendRequest(request, callback)", "Send another request"),
    ("pm.cookies.get(name)", "Cookie for the request URL"),
    ("pm.info.requestName", "Current request name"),
    ("pm.info.iteration", "Current iteration"),
    ("pm.execution.setNextRequest(name)", "Runner flow control"),
    ("pm.execution.skipRequest()", "Skip the request"),
    ("pm.visualizer.set(template, data)", "Visualize the response"),
];
