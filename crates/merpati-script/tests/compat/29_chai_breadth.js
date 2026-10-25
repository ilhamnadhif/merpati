// @event: prerequest
// @expect: {"tests": [["types", true], ["numbers", true], ["collections", true], ["objects", true], ["strings", true], ["throw", true], ["misc", true]], "errors": 0}
pm.test('types', () => {
    pm.expect('x').to.be.a('string');
    pm.expect([]).to.be.an('array');
    pm.expect({}).to.be.an('object');
    pm.expect(null).to.be.null;
    pm.expect(undefined).to.be.undefined;
    pm.expect(NaN).to.be.NaN;
    pm.expect(1).to.exist;
    pm.expect(true).to.be.true;
    pm.expect(false).to.be.false;
    pm.expect(new Date()).to.be.instanceOf(Date);
});
pm.test('numbers', () => {
    pm.expect(5).to.be.above(4).and.below(6);
    pm.expect(5).to.be.within(1, 10);
    pm.expect(5).to.be.at.least(5);
    pm.expect(5).to.be.at.most(5);
    pm.expect(1.5).to.be.closeTo(1.4, 0.2);
    pm.expect(3).to.be.oneOf([1, 2, 3]);
});
pm.test('collections', () => {
    pm.expect([1, 2, 3]).to.include(2);
    pm.expect([1, 2, 3]).to.have.members([3, 2, 1]);
    pm.expect([1, 2, 3]).to.include.members([1, 2]);
    pm.expect([{ a: 1 }]).to.deep.include({ a: 1 });
    pm.expect([1, 2]).to.have.lengthOf(2);
    pm.expect([]).to.be.empty;
    pm.expect(new Set([1])).to.have.lengthOf(1);
});
pm.test('objects', () => {
    pm.expect({ a: 1, b: { c: 2 } }).to.have.property('a', 1);
    pm.expect({ a: 1, b: { c: 2 } }).to.have.nested.property('b.c', 2);
    pm.expect({ a: 1, b: 2 }).to.have.all.keys('a', 'b');
    pm.expect({ a: 1, b: 2 }).to.have.any.keys('a', 'z');
    pm.expect({ a: 1 }).to.deep.equal({ a: 1 });
    pm.expect({ a: 1, b: 2 }).to.include({ a: 1 });
    pm.expect({ a: 1 }).to.have.own.property('a');
    pm.expect({}).to.be.empty;
});
pm.test('strings', () => {
    pm.expect('hello world').to.include('world');
    pm.expect('hello').to.match(/^he/);
    pm.expect('hello').to.have.string('ell');
    pm.expect('hello').to.have.lengthOf(5);
    pm.expect('').to.be.empty;
});
pm.test('throw', () => {
    pm.expect(() => { throw new TypeError('bad'); }).to.throw(TypeError, 'bad');
    pm.expect(() => {}).to.not.throw();
});
pm.test('misc', () => {
    pm.expect(4).to.satisfy((n) => n % 2 === 0);
    pm.expect(1).to.not.equal(2);
    pm.expect({ a: [1, { b: 2 }] }).to.eql({ a: [1, { b: 2 }] });
    pm.expect('a').to.be.ok;
    pm.expect(0).to.not.be.ok;
    pm.expect.fail === undefined || pm.expect(typeof pm.expect.fail).to.equal('function');
});
