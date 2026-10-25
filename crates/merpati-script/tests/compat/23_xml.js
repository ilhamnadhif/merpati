// @response: {"code": 200, "headers": [["Content-Type", "application/xml"]], "body": "<?xml version=\"1.0\"?><root a=\"1\"><item id=\"x\">one</item><item>two &amp; three</item><empty/><cdata><![CDATA[<raw>]]></cdata></root>"}
// @expect: {"tests": [["xml2Json", true], ["xml2js", true], ["is xml", true]], "errors": 0}
pm.test('xml2Json', () => {
    const j = xml2Json(responseBody);
    pm.expect(j.root.$.a).to.equal('1');
    pm.expect(j.root.item).to.have.lengthOf(2);
    pm.expect(j.root.item[0]._).to.equal('one');
    pm.expect(j.root.item[0].$.id).to.equal('x');
    pm.expect(j.root.item[1]).to.equal('two & three');
    pm.expect(j.root.cdata).to.equal('<raw>');
});
pm.test('xml2js', () => {
    let result;
    require('xml2js').parseString(pm.response.text(), (err, r) => { result = r; });
    pm.expect(result.root.item[1]).to.eql(['two & three'][0]);
    pm.expect(result.root.empty).to.eql(['']);
});
pm.test('is xml', () => pm.response.to.be.xml);
