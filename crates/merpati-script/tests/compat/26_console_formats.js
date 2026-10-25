// @event: prerequest
// @expect: {"console": ["plain", "obj { a: 1, b: [ 1, 2 ], c: { d: 'x' } }", "num 42 str", "fmt 7 and {\"a\":1}", "[ 1, 'two', null, undefined ]", "Error: bad thing", "warned"], "errors": 0}
console.log('plain');
console.log('obj', { a: 1, b: [1, 2], c: { d: 'x' } });
console.info('num', 42, 'str');
console.log('fmt %d and %j', 7, { a: 1 });
console.log([1, 'two', null, undefined]);
console.error(new Error('bad thing'));
console.warn('warned');
