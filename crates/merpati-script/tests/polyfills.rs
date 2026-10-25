//! The `Intl` and Web Streams polyfills (js/intl.js, js/streams.js).
//! Expected values match Node.js 22 (ICU 78) unless noted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use merpati_core::script::*;
use merpati_script::{BoaEngine, EngineConfig};

fn job(code: &str) -> ScriptJob {
    let mut j = ScriptJob::new(
        ScriptEvent::PreRequest,
        vec![ScriptSource { location: "request \"t\"".into(), code: code.into() }],
        ScriptRequest { method: "GET".into(), url: "https://x".into(), ..Default::default() },
    );
    j.timeout = Duration::from_secs(20);
    j
}

fn run_ok(engine: &BoaEngine, code: &str) -> ScriptOutcome {
    let out = engine.run(job(code));
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let failed: Vec<_> = out.tests.iter().filter(|t| !t.passed).map(|t| (&t.name, &t.error)).collect();
    assert!(failed.is_empty(), "failed tests: {failed:?}");
    assert!(!out.tests.is_empty(), "no tests ran; console: {:?}", out.console);
    out
}

#[test]
fn number_format() {
    run_ok(
        &BoaEngine::new(),
        r#"
        const nf = (o, l) => new Intl.NumberFormat(l || 'en-US', o);
        pm.test('decimal', () => {
            pm.expect(nf().format(1234567.891)).to.equal('1,234,567.891');
            pm.expect(nf().format(-0)).to.equal('-0');
            pm.expect(nf().format(1.005)).to.equal('1.005');
            pm.expect(nf({ maximumFractionDigits: 2 }).format(1.005)).to.equal('1.01');
            pm.expect(nf({ maximumFractionDigits: 0 }).format(2.5)).to.equal('3');
            pm.expect(nf({ maximumFractionDigits: 0, roundingMode: 'halfEven' }).format(2.5)).to.equal('2');
            pm.expect(nf({ minimumFractionDigits: 2 }).format(1)).to.equal('1.00');
            pm.expect(nf({ maximumSignificantDigits: 3 }).format(123456)).to.equal('123,000');
            pm.expect(nf({ minimumIntegerDigits: 3 }).format(5)).to.equal('005');
            pm.expect(nf({ useGrouping: false }).format(12345)).to.equal('12345');
            pm.expect(nf().format(1e21)).to.equal('1,000,000,000,000,000,000,000');
            pm.expect(nf().format(1.5e-7)).to.equal('0');
            pm.expect([NaN, Infinity, -Infinity].map(nf().format)).to.eql(['NaN', '∞', '-∞']);
            pm.expect(nf().format(12345678901234567890n)).to.equal('12,345,678,901,234,567,890');
            pm.expect(nf({ maximumFractionDigits: 20 }).format('12345678901234567890.123456789')).to.equal('12,345,678,901,234,567,890.123456789');
            pm.expect(nf({ roundingIncrement: 5, maximumFractionDigits: 2, minimumFractionDigits: 2 }).format(1.234)).to.equal('1.25');
            pm.expect(nf({ trailingZeroDisplay: 'stripIfInteger', minimumFractionDigits: 2 }).format(5)).to.equal('5');
        });
        pm.test('percent, currency, unit', () => {
            pm.expect(nf({ style: 'percent' }).format(0.256)).to.equal('26%');
            pm.expect(nf({ style: 'currency', currency: 'USD' }).format(-1234.567)).to.equal('-$1,234.57');
            pm.expect(nf({ style: 'currency', currency: 'USD', currencySign: 'accounting' }).format(-1234.5)).to.equal('($1,234.50)');
            pm.expect(nf({ style: 'currency', currency: 'EUR', currencyDisplay: 'code' }).format(1)).to.equal('EUR 1.00');
            pm.expect(nf({ style: 'currency', currency: 'JPY' }).format(1234.5)).to.equal('¥1,235');
            pm.expect(nf({ style: 'currency', currency: 'IDR' }).format(1234.5)).to.equal('IDR 1,235');
            pm.expect(nf({ style: 'currency', currency: 'USD', currencyDisplay: 'name' }).format(1)).to.equal('1.00 US dollars');
            pm.expect(nf({ style: 'currency', currency: 'USD', currencyDisplay: 'name', maximumFractionDigits: 0 }).format(1)).to.equal('1 US dollar');
            pm.expect(nf({ style: 'unit', unit: 'kilometer-per-hour' }).format(50)).to.equal('50 km/h');
            pm.expect(nf({ style: 'unit', unit: 'megabyte', unitDisplay: 'long' }).format(1.5)).to.equal('1.5 megabytes');
            pm.expect(nf({ style: 'unit', unit: 'celsius', unitDisplay: 'narrow' }).format(21)).to.equal('21°C');
        });
        pm.test('notation and sign', () => {
            const compact = nf({ notation: 'compact' });
            pm.expect([999, 1234, 9999, 999999, 1234567, 1e15, 0.123].map(compact.format)).to.eql(['999', '1.2K', '10K', '1M', '1.2M', '1000T', '0.12']);
            pm.expect(nf({ notation: 'compact', compactDisplay: 'long' }).format(1234567)).to.equal('1.2 million');
            pm.expect(nf({ notation: 'compact', style: 'currency', currency: 'USD' }).format(1234)).to.equal('$1.2K');
            pm.expect(nf({ notation: 'scientific' }).format(123456)).to.equal('1.235E5');
            pm.expect(nf({ notation: 'engineering' }).format(0.123)).to.equal('123E-3');
            pm.expect(nf({ signDisplay: 'always' }).format(0)).to.equal('+0');
            pm.expect(nf({ signDisplay: 'exceptZero' }).format(-0)).to.equal('0');
            pm.expect(nf({ signDisplay: 'always' }).format(NaN)).to.equal('+NaN');
        });
        pm.test('parts, ranges, options', () => {
            pm.expect(nf({ style: 'currency', currency: 'USD' }).formatToParts(-1234.5)).to.eql([
                { type: 'minusSign', value: '-' }, { type: 'currency', value: '$' }, { type: 'integer', value: '1' },
                { type: 'group', value: ',' }, { type: 'integer', value: '234' }, { type: 'decimal', value: '.' },
                { type: 'fraction', value: '50' },
            ]);
            pm.expect(nf().formatRange(3, 5)).to.equal('3–5');
            pm.expect(nf().formatRange(3, 3)).to.equal('~3');
            pm.expect(nf({ notation: 'compact' }).resolvedOptions()).to.eql({
                locale: 'en-US', numberingSystem: 'latn', style: 'decimal', minimumIntegerDigits: 1, minimumFractionDigits: 0,
                maximumFractionDigits: 0, minimumSignificantDigits: 1, maximumSignificantDigits: 2, useGrouping: 'min2',
                notation: 'compact', compactDisplay: 'short', signDisplay: 'auto', roundingIncrement: 1, roundingMode: 'halfExpand',
                roundingPriority: 'morePrecision', trailingZeroDisplay: 'auto',
            });
            pm.expect([1000, 2000].map(Intl.NumberFormat('en').format)).to.eql(['1,000', '2,000']);
            pm.expect(() => nf({ style: 'currency' })).to.throw(TypeError);
            pm.expect(() => nf({ maximumFractionDigits: 101 })).to.throw(RangeError);
            pm.expect(() => nf({ style: 'unit', unit: 'parsec' })).to.throw(RangeError);
        });
        pm.test('other languages', () => {
            pm.expect(nf({}, 'de-DE').format(-1234567.891)).to.equal('-1.234.567,891');
            pm.expect(nf({ style: 'currency', currency: 'EUR' }, 'de-DE').format(-1234.5)).to.equal('-1.234,50 €');
            pm.expect(nf({ style: 'currency', currency: 'IDR' }, 'id-ID').format(1234.5)).to.equal('Rp 1.235');
            pm.expect(nf({ style: 'currency', currency: 'USD' }, 'id-ID').format(1234.5)).to.equal('US$1.234,50');
            pm.expect(nf({}, 'fr-FR').format(1234.5)).to.equal('1 234,5');
            pm.expect(nf({}, 'es').format(1234.5)).to.equal('1234,5');
            pm.expect(nf({ style: 'currency', currency: 'EUR' }, 'nl').format(-1234.5)).to.equal('€ -1.234,50');
            pm.expect(nf({ style: 'percent' }, 'de').format(0.5)).to.equal('50 %');
        });
        pm.test('toLocaleString', () => {
            pm.expect((1234.5).toLocaleString()).to.equal('1,234.5');
            pm.expect((1234.5).toLocaleString('de')).to.equal('1.234,5');
            pm.expect((0.5).toLocaleString(undefined, { style: 'percent' })).to.equal('50%');
            pm.expect((12345n).toLocaleString()).to.equal('12,345');
            pm.expect(Number.prototype.toLocaleString.length).to.equal(0);
            pm.expect(() => Number.prototype.toLocaleString.call('x')).to.throw(TypeError);
        });
        "#,
    );
}

#[test]
fn date_time_format() {
    run_ok(
        &BoaEngine::new(),
        r#"
        const D = Date.UTC(2026, 9, 23, 21, 5, 3, 45);
        const f = (o) => new Intl.DateTimeFormat('en-US', Object.assign({ timeZone: 'UTC' }, o));
        pm.test('defaults and styles', () => {
            pm.expect(f().format(D)).to.equal('10/23/2026');
            pm.expect(f({ dateStyle: 'full' }).format(D)).to.equal('Friday, October 23, 2026');
            pm.expect(f({ dateStyle: 'medium', timeStyle: 'short' }).format(D)).to.equal('Oct 23, 2026, 9:05 PM');
            pm.expect(f({ dateStyle: 'long', timeStyle: 'long' }).format(D)).to.equal('October 23, 2026 at 9:05:03 PM UTC');
            pm.expect(f({ dateStyle: 'short' }).format(D)).to.equal('10/23/26');
            pm.expect(f({ timeStyle: 'full' }).format(D)).to.equal('9:05:03 PM Coordinated Universal Time');
            pm.expect(f({ timeStyle: 'short', hour12: false }).format(D)).to.equal('21:05');
        });
        pm.test('components', () => {
            pm.expect(f({ weekday: 'long', year: 'numeric', month: 'long', day: 'numeric' }).format(D)).to.equal('Friday, October 23, 2026');
            pm.expect(f({ year: 'numeric', month: '2-digit', day: '2-digit' }).format(D)).to.equal('10/23/2026');
            pm.expect(f({ month: 'short', day: 'numeric' }).format(D)).to.equal('Oct 23');
            pm.expect(f({ month: 'long', year: 'numeric' }).format(D)).to.equal('October 2026');
            pm.expect(f({ hour: '2-digit', minute: '2-digit' }).format(D)).to.equal('09:05 PM');
            pm.expect(f({ hour: 'numeric', minute: 'numeric', second: 'numeric', fractionalSecondDigits: 3 }).format(D)).to.equal('9:05:03.045 PM');
            pm.expect(f({ weekday: 'short', hour: 'numeric' }).format(D)).to.equal('Fri 9 PM');
            pm.expect(f({ month: 'long', day: 'numeric', hour: 'numeric', minute: '2-digit' }).format(D)).to.equal('October 23 at 9:05 PM');
            pm.expect(f({ era: 'long', year: 'numeric' }).format(D)).to.equal('2026 Anno Domini');
            pm.expect(f({ hour: 'numeric', dayPeriod: 'long' }).format(D)).to.equal('9 at night');
            pm.expect(f({ hourCycle: 'h24', hour: 'numeric', minute: 'numeric' }).format(Date.UTC(2026, 0, 1, 0, 30))).to.equal('24:30');
            pm.expect(f({ timeZoneName: 'short' }).format(D)).to.equal('10/23/2026, UTC');
        });
        pm.test('parts and ranges', () => {
            const parts = f({ hour: 'numeric', minute: 'numeric' }).formatToParts(D);
            // V8 keeps ICU's narrow no-break space before AM/PM in parts.
            pm.expect(parts.map((p) => p.type)).to.eql(['hour', 'literal', 'minute', 'literal', 'dayPeriod']);
            pm.expect(parts[3].value).to.equal(' ');
            pm.expect(f({ timeStyle: 'short' }).formatRange(D, D + 3600000)).to.equal('9:05 – 10:05 PM');
            pm.expect(f().formatRange(D, D + 3 * 86400000)).to.equal('10/23/2026 – 10/26/2026');
            pm.expect(f().formatRange(D, D)).to.equal('10/23/2026');
        });
        pm.test('resolvedOptions and errors', () => {
            pm.expect(f({ hour: 'numeric', minute: 'numeric' }).resolvedOptions()).to.eql({
                locale: 'en-US', calendar: 'gregory', numberingSystem: 'latn', timeZone: 'UTC', hourCycle: 'h12', hour12: true,
                hour: 'numeric', minute: '2-digit',
            });
            pm.expect(f({ timeZone: 'Etc/UTC' }).resolvedOptions().timeZone).to.equal('UTC');
            pm.expect(f({ timeZone: '+05:30', timeZoneName: 'shortOffset' }).format(D)).to.equal('10/24/2026, GMT+5:30');
            pm.expect(() => f({ timeZone: 'Mars/Olympus' })).to.throw(RangeError);
            pm.expect(() => f({ dateStyle: 'full', hour: 'numeric' })).to.throw(TypeError);
            pm.expect(() => f().format(NaN)).to.throw(RangeError);
        });
        pm.test('Date.prototype.toLocale*', () => {
            const d = new Date(D);
            pm.expect(d.toLocaleString('en-US', { timeZone: 'UTC' })).to.equal('10/23/2026, 9:05:03 PM');
            pm.expect(d.toLocaleDateString('en-US', { timeZone: 'UTC', weekday: 'long' })).to.equal('Friday');
            pm.expect(d.toLocaleTimeString('en-US', { timeZone: 'UTC', hour12: false })).to.equal('21:05:03');
            pm.expect(new Date(NaN).toLocaleString()).to.equal('Invalid Date');
            // The default time zone is the local one, like the Date getters.
            pm.expect(d.toLocaleDateString()).to.equal((d.getMonth() + 1) + '/' + d.getDate() + '/' + d.getFullYear());
            pm.expect(Intl.DateTimeFormat().resolvedOptions().timeZone).to.be.a('string').and.not.empty;
        });
        "#,
    );
}

#[test]
fn named_time_zones() {
    if !std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists() {
        return;
    }
    run_ok(
        &BoaEngine::new(),
        r#"
        const summer = Date.UTC(2026, 6, 4, 16);
        const winter = Date.UTC(2026, 0, 15, 12);
        const f = (tz, o) => new Intl.DateTimeFormat('en-US', Object.assign({ timeZone: tz }, o));
        pm.test('offsets and DST', () => {
            pm.expect(f('America/New_York', { timeStyle: 'short' }).format(summer)).to.equal('12:00 PM');
            pm.expect(f('America/New_York', { timeStyle: 'short' }).format(winter)).to.equal('7:00 AM');
            pm.expect(f('Asia/Kolkata', { timeStyle: 'short' }).format(winter)).to.equal('5:30 PM');
            pm.expect(f('Australia/Sydney', { dateStyle: 'short', timeStyle: 'short' }).format(winter)).to.equal('1/15/26, 11:00 PM');
            pm.expect(f('Etc/GMT+5', { timeStyle: 'short' }).format(winter)).to.equal('7:00 AM');
            pm.expect(new Date(summer).toLocaleString('en-US', { timeZone: 'America/New_York' })).to.equal('7/4/2026, 12:00:00 PM');
        });
        pm.test('names', () => {
            const name = (tz, style, t) => f(tz, { timeZoneName: style }).formatToParts(t).find((p) => p.type === 'timeZoneName').value;
            pm.expect([name('America/New_York', 'short', winter), name('America/New_York', 'short', summer)]).to.eql(['EST', 'EDT']);
            pm.expect(name('America/Los_Angeles', 'long', summer)).to.equal('Pacific Daylight Time');
            pm.expect(name('America/Chicago', 'shortGeneric', summer)).to.equal('CT');
            pm.expect(name('Asia/Jakarta', 'short', summer)).to.equal('GMT+7');
            pm.expect(name('Asia/Jakarta', 'long', summer)).to.equal('Western Indonesia Time');
            pm.expect(name('Europe/Berlin', 'long', summer)).to.equal('Central European Summer Time');
            pm.expect(name('Europe/London', 'short', winter)).to.equal('GMT');
            pm.expect(name('Asia/Kolkata', 'longOffset', summer)).to.equal('GMT+05:30');
            pm.expect(f('america/new_york').resolvedOptions().timeZone).to.equal('America/New_York');
            pm.expect(Intl.supportedValuesOf('timeZone')).to.include('Asia/Jakarta');
        });
        "#,
    );
}

#[test]
fn collator_plural_relative_list() {
    run_ok(
        &BoaEngine::new(),
        r#"
        pm.test('collation', () => {
            const words = ['b', 'a', 'B', 'A', 'á', 'ab', 'a-b', 'a b', '10', '2', '_a', '$', 'ä', 'æ', 'ß', 'ss', 'résumé', 'resume', 'Resume', 'α', 'z'];
            pm.expect(words.slice().sort(new Intl.Collator('en').compare)).to.eql(
                ['_a', '$', '10', '2', 'a', 'A', 'á', 'ä', 'a b', 'a-b', 'ab', 'æ', 'b', 'B', 'resume', 'Resume', 'résumé', 'ss', 'ß', 'z', 'α']);
            pm.expect(['item 10', 'item 2', 'Item 1'].sort(new Intl.Collator('en', { numeric: true }).compare)).to.eql(['Item 1', 'item 2', 'item 10']);
            pm.expect(['a', 'A', 'b'].sort(new Intl.Collator('en', { caseFirst: 'upper' }).compare)).to.eql(['A', 'a', 'b']);
            const pairs = [['a', 'A'], ['a', 'á'], ['ß', 'ss'], ['é', 'é'], ['a-b', 'ab'], ['ﬁ', 'fi']];
            pm.expect(pairs.map(([x, y]) => new Intl.Collator('en', { sensitivity: 'base' }).compare(x, y))).to.eql([0, 0, 0, 0, -1, 0]);
            pm.expect(pairs.map(([x, y]) => new Intl.Collator('en', { sensitivity: 'accent' }).compare(x, y))).to.eql([0, -1, 1, 0, -1, 0]);
            pm.expect(pairs.map(([x, y]) => new Intl.Collator('en').compare(x, y))).to.eql([-1, -1, 1, 0, -1, 1]);
            pm.expect(new Intl.Collator('en', { ignorePunctuation: true }).compare('a-b', 'ab')).to.equal(0);
            pm.expect(new Intl.Collator('en-u-kn-kf-upper').resolvedOptions()).to.eql({
                locale: 'en-u-kf-upper-kn', usage: 'sort', sensitivity: 'variant', ignorePunctuation: false, collation: 'default',
                numeric: true, caseFirst: 'upper',
            });
            pm.expect(['a'.localeCompare('B'), 'résumé'.localeCompare('resume', 'en', { sensitivity: 'base' }), '10'.localeCompare('2', undefined, { numeric: true })]).to.eql([-1, 0, 1]);
        });
        pm.test('plural rules', () => {
            const card = new Intl.PluralRules('en');
            const ord = new Intl.PluralRules('en', { type: 'ordinal' });
            pm.expect([0, 1, 2, 1.5, '1.0'].map((n) => card.select(n))).to.eql(['other', 'one', 'other', 'other', 'one']);
            pm.expect([1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 101, 111].map((n) => ord.select(n)))
                .to.eql(['one', 'two', 'few', 'other', 'other', 'other', 'other', 'one', 'two', 'few', 'one', 'other']);
            pm.expect(ord.resolvedOptions().pluralCategories).to.eql(['few', 'one', 'two', 'other']);
            pm.expect(new Intl.PluralRules('en', { minimumFractionDigits: 1 }).select(1)).to.equal('other');
            pm.expect([1, 2, 5, 21].map((n) => new Intl.PluralRules('ru').select(n))).to.eql(['one', 'few', 'many', 'one']);
            pm.expect([0, 1, 1e6].map((n) => new Intl.PluralRules('fr').select(n))).to.eql(['one', 'one', 'many']);
            pm.expect(new Intl.PluralRules('id').select(1)).to.equal('other');
        });
        pm.test('relative time', () => {
            const rtf = new Intl.RelativeTimeFormat('en');
            const auto = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
            pm.expect([rtf.format(-1, 'day'), rtf.format(2, 'weeks'), rtf.format(-0, 'second'), rtf.format(1.5, 'hour')])
                .to.eql(['1 day ago', 'in 2 weeks', '0 seconds ago', 'in 1.5 hours']);
            pm.expect([auto.format(-1, 'day'), auto.format(0, 'year'), auto.format(1, 'month'), auto.format(0, 'second'), auto.format(-1, 'hour')])
                .to.eql(['yesterday', 'this year', 'next month', 'now', '1 hour ago']);
            pm.expect(new Intl.RelativeTimeFormat('en', { style: 'short' }).format(-3, 'quarter')).to.equal('3 qtrs. ago');
            pm.expect(new Intl.RelativeTimeFormat('en', { style: 'narrow' }).format(-3, 'day')).to.equal('3d ago');
            pm.expect(rtf.formatToParts(-1234.5, 'day')).to.eql([
                { type: 'integer', value: '1', unit: 'day' }, { type: 'group', value: ',', unit: 'day' },
                { type: 'integer', value: '234', unit: 'day' }, { type: 'decimal', value: '.', unit: 'day' },
                { type: 'fraction', value: '5', unit: 'day' }, { type: 'literal', value: ' days ago' },
            ]);
            pm.expect(() => rtf.format(1, 'decade')).to.throw(RangeError);
        });
        pm.test('lists', () => {
            pm.expect(new Intl.ListFormat('en').format(['A', 'B', 'C'])).to.equal('A, B, and C');
            pm.expect(new Intl.ListFormat('en', { style: 'short' }).format(['A', 'B'])).to.equal('A & B');
            pm.expect(new Intl.ListFormat('en', { type: 'disjunction' }).format(['A', 'B', 'C'])).to.equal('A, B, or C');
            pm.expect(new Intl.ListFormat('en', { type: 'unit', style: 'narrow' }).format(['A', 'B', 'C'])).to.equal('A B C');
            pm.expect(new Intl.ListFormat('en').formatToParts(['x', 'y'])).to.eql([
                { type: 'element', value: 'x' }, { type: 'literal', value: ' and ' }, { type: 'element', value: 'y' },
            ]);
            pm.expect(() => new Intl.ListFormat('en').format(['a', 1])).to.throw(TypeError);
        });
        "#,
    );
}

#[test]
fn segmenter_locale_and_fallbacks() {
    run_ok(
        &BoaEngine::new(),
        r#"
        const seg = (g, s) => [...new Intl.Segmenter('en', { granularity: g }).segment(s)];
        pm.test('graphemes', () => {
            pm.expect(seg('grapheme', 'é👍🏽👨‍👩‍👧🇮🇩🇺🇸\r\nx').map((s) => s.segment))
                .to.eql(['é', '👍🏽', '👨‍👩‍👧', '🇮🇩', '🇺🇸', '\r\n', 'x']);
        });
        pm.test('words', () => {
            const words = seg('word', "It's 3.14, e.g. U.S.A. ok_go foo@bar.com").map((s) => [s.segment, s.isWordLike]);
            pm.expect(words.filter((w) => w[1]).map((w) => w[0])).to.eql(["It's", '3.14', 'e.g', 'U.S.A', 'ok_go', 'foo', 'bar.com']);
            pm.expect(words[1]).to.eql([' ', false]);
            const s = new Intl.Segmenter('en', { granularity: 'word' }).segment('Hello world');
            pm.expect(s.containing(7)).to.eql({ segment: 'world', index: 6, input: 'Hello world', isWordLike: true });
            pm.expect(s.containing(99)).to.equal(undefined);
        });
        pm.test('sentences', () => {
            pm.expect(seg('sentence', 'Mr. Smith went. He said "Hi!" Then left? Yes... ok. 3.5 apples. Next\nline.').map((s) => s.segment))
                .to.eql(['Mr. ', 'Smith went. ', 'He said "Hi!" ', 'Then left? ', 'Yes... ok. ', '3.5 apples. ', 'Next\n', 'line.']);
        });
        pm.test('locales', () => {
            pm.expect(Intl.getCanonicalLocales(['EN-us', 'zh-hant-tw', 'iw', 'de-DE-u-co-phonebk-kn-true', 'en-u-ca-gregory-hc-h23']))
                .to.eql(['en-US', 'zh-Hant-TW', 'he', 'de-DE-u-co-phonebk-kn', 'en-u-ca-gregory-hc-h23']);
            pm.expect(() => Intl.getCanonicalLocales('en_US')).to.throw(RangeError);
            const l = new Intl.Locale('en-Latn-US-u-ca-gregory-hc-h12-kn-nu-latn');
            pm.expect([l.baseName, l.language, l.script, l.region, l.calendar, l.hourCycle, l.numeric, l.numberingSystem])
                .to.eql(['en-Latn-US', 'en', 'Latn', 'US', 'gregory', 'h12', true, 'latn']);
            pm.expect([new Intl.Locale('id').maximize().toString(), new Intl.Locale('en-Latn-US').minimize().toString()]).to.eql(['id-Latn-ID', 'en']);
            pm.expect(new Intl.Locale('en', { region: 'GB', hourCycle: 'h23' }).toString()).to.equal('en-GB-u-hc-h23');
            pm.expect(Intl.supportedValuesOf('calendar')).to.include('gregory');
            pm.expect(() => Intl.supportedValuesOf('nope')).to.throw(RangeError);
            pm.expect(Object.prototype.toString.call(Intl)).to.equal('[object Intl]');
        });
        pm.test('unsupported locales fall back to en-US and say so', () => {
            const d = Date.UTC(2026, 9, 23);
            const dtf = new Intl.DateTimeFormat('fr-FR', { timeZone: 'UTC' });
            pm.expect(dtf.resolvedOptions().locale).to.equal('en-US');
            pm.expect(dtf.format(d)).to.equal('10/23/2026');
            pm.expect(new Intl.DateTimeFormat('en-GB').resolvedOptions().locale).to.equal('en');
            pm.expect(new Intl.NumberFormat('tlh').resolvedOptions().locale).to.equal('en-US');
            pm.expect(new Intl.NumberFormat('de-AT').resolvedOptions().locale).to.equal('de');
            // Compact notation has English data only.
            pm.expect(new Intl.NumberFormat('de', { notation: 'compact' }).resolvedOptions().locale).to.equal('en-US');
            pm.expect(Intl.NumberFormat.supportedLocalesOf(['en-GB', 'tlh', 'id-ID'])).to.eql(['en-GB', 'id-ID']);
        });
        "#,
    );
}

#[test]
fn polyfills_are_shared_by_jobs_and_restored() {
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 1, ..Default::default() });
    run_ok(&e, "pm.test('first use', () => pm.expect((1234).toLocaleString()).to.equal('1,234'));");
    // Once loaded, the globals are plain data properties.
    run_ok(
        &e,
        "pm.test('data property', () => pm.expect(Object.getOwnPropertyDescriptor(globalThis, 'Intl').writable).to.equal(true));",
    );
    // Scripts that break the globals only affect their own job.
    run_ok(
        &e,
        "Intl.NumberFormat = null; Intl = 5; ReadableStream = null; Number.prototype.toLocaleString = () => 'x';\n\
         pm.test('broken here', () => pm.expect(Intl).to.equal(5));",
    );
    run_ok(
        &e,
        "pm.test('restored', () => {\n\
             pm.expect(new Intl.NumberFormat('en').format(1e3)).to.equal('1,000');\n\
             pm.expect((1e3).toLocaleString()).to.equal('1,000');\n\
             pm.expect(typeof ReadableStream).to.equal('function');\n\
         });",
    );
    run_ok(
        &e,
        "pm.test('streams settle as globals', () => {\n\
             new ReadableStream();\n\
             pm.expect(typeof ReadableStream).to.equal('function');\n\
         });",
    );
    run_ok(
        &e,
        "pm.test('later jobs', () => {\n\
             pm.expect(typeof ReadableStreamDefaultController).to.equal('function');\n\
             pm.expect(Object.getOwnPropertyDescriptor(globalThis, 'ReadableStream').writable).to.equal(true);\n\
             pm.expect(Object.keys(globalThis)).to.not.include('ReadableStream');\n\
         });",
    );
    // First use inside a callback (a getter that loads a module mid-expression).
    run_ok(
        &BoaEngine::new(),
        "[1, 2].forEach((i) => pm.test('in callback ' + i, () => pm.expect(new Intl.NumberFormat().format(i * 1000)).to.equal(i + ',000')));",
    );
}

#[test]
fn web_streams() {
    run_ok(
        &BoaEngine::new(),
        r#"
        const readAll = (rs) => Array.fromAsync(rs);
        const tick = () => new Promise((r) => setTimeout(r, 0));
        pm.test('read, pull and backpressure', async () => {
            const ev = [];
            let n = 0;
            const rs = new ReadableStream({ pull(c) { ev.push('pull ' + c.desiredSize); c.enqueue(n++); if (n === 4) c.close(); } },
                new CountQueuingStrategy({ highWaterMark: 2 }));
            await tick();
            ev.push('started');
            const r = rs.getReader();
            pm.expect(rs.locked).to.equal(true);
            pm.expect(await r.read()).to.eql({ value: 0, done: false });
            r.releaseLock();
            ev.push(...(await readAll(rs)));
            pm.expect(ev).to.eql(['pull 2', 'pull 1', 'started', 'pull 1', 'pull 1', 1, 2, 3]);
        });
        pm.test('errors and cancel', async () => {
            const failing = new ReadableStream({ start(c) { c.error(new TypeError('boom')); } });
            let err;
            try { await failing.getReader().read(); } catch (e) { err = e; }
            pm.expect(err).to.be.instanceOf(TypeError);
            const ev = [];
            let n = 0;
            const rs = new ReadableStream({ pull(c) { c.enqueue(n++); }, cancel(reason) { ev.push('cancel ' + reason); } });
            for await (const x of rs) { ev.push(x); if (x >= 1) break; }
            pm.expect(ev).to.eql([0, 1, 'cancel undefined']);
            pm.expect(rs.locked).to.equal(false);
        });
        pm.test('from, tee, pipes and transforms', async () => {
            async function* gen() { yield 1; await tick(); yield 2; }
            pm.expect(await readAll(ReadableStream.from(gen()))).to.eql([1, 2]);
            const [a, b] = ReadableStream.from(['x', 'y']).tee();
            pm.expect([await readAll(a), await readAll(b)]).to.eql([['x', 'y'], ['x', 'y']]);
            const ts = new TransformStream({ transform(chunk, c) { c.enqueue(chunk * 10); }, flush(c) { c.enqueue('end'); } });
            const written = [];
            await ReadableStream.from([1, 2]).pipeThrough(ts).pipeTo(new WritableStream({ write(c) { written.push(c); } }));
            pm.expect(written).to.eql([10, 20, 'end']);
            const ev = [];
            const src = new ReadableStream({ pull(c) { c.enqueue(1); }, cancel(r) { ev.push('cancel ' + r.message); } });
            try { await src.pipeTo(new WritableStream({ write() { throw new Error('sink'); } })); } catch (e) { ev.push('rejected ' + e.message); }
            pm.expect(ev).to.eql(['cancel sink', 'rejected sink']);
        });
        pm.test('writable streams', async () => {
            const ev = [];
            const ws = new WritableStream({ async write(chunk) { ev.push(chunk); await tick(); }, close() { ev.push('close'); } }, { highWaterMark: 2 });
            const w = ws.getWriter();
            w.write('a');
            w.write('b');
            pm.expect(w.desiredSize).to.equal(0);
            await w.close();
            pm.expect(ev).to.eql(['a', 'b', 'close']);
            let ctrl;
            const aborted = new WritableStream({ start(c) { ctrl = c; }, abort(r) { ev.push('abort ' + r); } });
            await aborted.abort('stop');
            pm.expect(ctrl.signal.aborted).to.equal(true);
            pm.expect(ev).to.include('abort stop');
        });
        pm.test('encoding streams and TextDecoder', async () => {
            const bytes = await readAll(ReadableStream.from(['a\ud83d', '\ude00é']).pipeThrough(new TextEncoderStream()));
            pm.expect(bytes.map((b) => Array.from(b))).to.eql([[97], [240, 159, 152, 128, 195, 169]]);
            const text = await readAll(ReadableStream.from([new Uint8Array([0xef, 0xbb, 0xbf, 0x68, 0xc3]), new Uint8Array([0xa9])]).pipeThrough(new TextDecoderStream()));
            pm.expect(text).to.eql(['h', 'é']);
            const d = new TextDecoder();
            pm.expect(d.decode(new Uint8Array([0xe2, 0x82]), { stream: true }) + d.decode(new Uint8Array([0xac]))).to.equal('€');
            pm.expect(() => new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xff]))).to.throw(TypeError);
            pm.expect(new TextDecoder('utf-8', { ignoreBOM: true }).decode(new Uint8Array([0xef, 0xbb, 0xbf]))).to.equal('﻿');
        });
        pm.test('byte streams', async () => {
            const rs = new ReadableStream({ type: 'bytes', start(c) { c.enqueue(new Uint8Array([1, 2, 3, 4, 5])); c.close(); } });
            const r = rs.getReader({ mode: 'byob' });
            const first = await r.read(new Uint8Array(2));
            pm.expect([Array.from(first.value), first.done]).to.eql([[1, 2], false]);
            const rest = await r.read(new Uint8Array(8));
            pm.expect(Array.from(rest.value)).to.eql([3, 4, 5]);
            pm.expect((await r.read(new Uint8Array(8))).done).to.equal(true);
            const auto = new ReadableStream({ type: 'bytes', autoAllocateChunkSize: 4, pull(c) { c.byobRequest.view[0] = 42; c.byobRequest.respond(1); c.close(); } });
            pm.expect((await readAll(auto)).map((u) => Array.from(u))).to.eql([[42]]);
            pm.expect((await readAll(new Blob(['hi']).stream())).map((u) => Array.from(u))).to.eql([[104, 105]]);
        });
        pm.test('queueMicrotask runs before timers', async () => {
            const ev = [];
            setTimeout(() => ev.push('timeout'), 0);
            queueMicrotask(() => ev.push('micro'));
            ev.push('sync');
            await new Promise((r) => setTimeout(r, 5));
            pm.expect(ev).to.eql(['sync', 'micro', 'timeout']);
        });
        "#,
    );
}

#[test]
fn stream_loops_respect_timeout_and_cancel() {
    let e = BoaEngine::new();
    let endless = "const rs = new ReadableStream({ pull(c) { c.enqueue(1); } });\nfor await (const x of rs) {}";
    let mut j = job(endless);
    j.timeout = Duration::from_millis(300);
    let t = Instant::now();
    let out = e.run(j);
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(out.errors.iter().any(|e| e.message.contains("timed out")), "{:?}", out.errors);
    // The worker is still usable.
    run_ok(&e, "pm.test('after', () => pm.expect(new ReadableStream().locked).to.equal(false));");

    let mut j = job(endless);
    let cancel = Arc::new(AtomicBool::new(false));
    j.cancel = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Relaxed);
    });
    let t = Instant::now();
    let out = e.run(j);
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(out.errors.iter().any(|e| e.message.contains("cancelled")), "{:?}", out.errors);
}
