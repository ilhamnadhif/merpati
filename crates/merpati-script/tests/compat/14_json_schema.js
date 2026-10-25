// @response: {"code": 200, "headers": [["Content-Type", "application/json"]], "body": "{\"id\": 1, \"name\": \"Ada\", \"email\": \"ada@example.com\"}"}
// @expect: {"tests": [["schema ok", true], ["schema bad", false], ["tv4", true], ["ajv", true]], "test_errors": {"schema bad": "expected data to satisfy schema"}, "errors": 0}
const schema = {
    type: 'object',
    required: ['id', 'name'],
    properties: { id: { type: 'integer' }, name: { type: 'string' }, email: { type: 'string' } }
};
pm.test('schema ok', () => pm.response.to.have.jsonSchema(schema));
pm.test('schema bad', () => pm.response.to.have.jsonSchema({ type: 'object', required: ['missing'] }));
pm.test('tv4', () => pm.expect(tv4.validate(pm.response.json(), schema)).to.be.true);
pm.test('ajv', () => {
    const Ajv = require('ajv');
    const ajv = new Ajv({ allErrors: true });
    const validate = ajv.compile(schema);
    pm.expect(validate(pm.response.json())).to.be.true;
    pm.expect(validate({ id: 'x' })).to.be.false;
    pm.expect(validate.errors.length).to.be.above(0);
});
