// @event: prerequest
// @expect: {"tests": [["es features", true]], "errors": 0}
pm.test('es features', () => {
    const { a, ...rest } = { a: 1, b: 2, c: 3 };
    pm.expect(rest).to.eql({ b: 2, c: 3 });
    pm.expect([1, 2, 3].includes(2)).to.be.true;
    pm.expect(Object.entries({ x: 1 })).to.eql([['x', 1]]);
    pm.expect(`t${1 + 1}`).to.equal('t2');
    pm.expect([1, [2, [3]]].flat(2)).to.eql([1, 2, 3]);
    pm.expect('abc'.padStart(5, '-')).to.equal('--abc');
    pm.expect(new Map([[1, 2]]).get(1)).to.equal(2);
    pm.expect(typeof Symbol()).to.equal('symbol');
    pm.expect(10n ** 3n).to.equal(1000n);
    pm.expect(null ?? 'd').to.equal('d');
    pm.expect(({ a: { b: 1 } }).a?.b).to.equal(1);
    pm.expect(Array.from({ length: 3 }, (_, i) => i)).to.eql([0, 1, 2]);
    pm.expect(JSON.stringify({ d: new Date(0) })).to.equal('{"d":"1970-01-01T00:00:00.000Z"}');
    class A { #p = 1; get p() { return this.#p; } }
    pm.expect(new A().p).to.equal(1);
});
