// @event: prerequest
// @expect: {"environment": {"ticks": "3"}, "errors": 0, "max_ms": 2000}
let n = 0;
const id = setInterval(() => {
    n++;
    if (n === 3) {
        clearInterval(id);
        pm.environment.set('ticks', n);
    }
}, 5);
