// Intl for the Merpati script sandbox.
//
// boa is built without its `intl` feature (ICU data would add megabytes to
// the app), so the prelude loads this polyfill on first use of `Intl` or of
// a locale-sensitive built-in (`toLocaleString`, `localeCompare`, ...).
//
// English (CLDR `en`, i.e. US English) is covered throughout, with output
// matching V8/ICU for the common cases. NumberFormat also knows de, es, fr,
// id, it, nl and pt; Collator the languages that use the root collation;
// PluralRules the same set as NumberFormat. Other locales fall back to
// en-US, and resolvedOptions().locale reports the locale actually used.
//
// `module.exports(env)` returns `{ Intl, numberToLocaleString,
// dateToLocaleString, localeCompare }`. `env.localTimeZone()` names
// the local IANA zone; `env.zoneOffset(name, epochMs)` returns
// `[offsetSeconds, abbreviation, canonicalName, isDst]` for named zones the
// host knows (system tz database), or undefined.
module.exports = function createIntl(env) {
  'use strict';

  const DEFAULT_LOCALE = 'en-US';
  const NBSP = ' ';
  const NNBSP = ' ';
  const THIN = ' ';
  const hasOwn = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const isObject = (v) => (typeof v === 'object' && v !== null) || typeof v === 'function';
  const pad2 = (n) => (n < 10 ? '0' : '') + n;
  const TYPE_RE = /^[a-z0-9]{3,8}(-[a-z0-9]{3,8})*$/i;

  // Defines non-enumerable properties (methods, getters) like built-ins.
  function define(obj, props) {
    for (const k of Reflect.ownKeys(props)) {
      const d = Object.getOwnPropertyDescriptor(props, k);
      d.enumerable = false;
      Object.defineProperty(obj, k, d);
    }
  }
  function tag(proto, name) {
    Object.defineProperty(proto, Symbol.toStringTag, { value: name, configurable: true });
  }
  function describe(v) {
    try { return typeof v === 'string' ? JSON.stringify(v) : String(v); } catch (e) { return typeof v; }
  }
  function slotsOf(map, obj, method) {
    const s = isObject(obj) ? map.get(obj) : undefined;
    if (!s) throw new TypeError('Method ' + method + ' called on incompatible receiver ' + describe(obj));
    return s;
  }

  // Memoizes `build()` per key in a small map (cleared when full).
  function memo(map, key, build) {
    let v = map.get(key);
    if (v === undefined) {
      if (map.size >= 64) map.clear();
      v = build();
      map.set(key, v);
    }
    return v;
  }

  // ---------------------------------------------------------------------
  // Options
  // ---------------------------------------------------------------------
  function coerceOptions(options) {
    if (options === undefined) return Object.create(null);
    if (options === null) throw new TypeError('Cannot convert undefined or null to object');
    return Object(options);
  }
  function optionsObject(options) {
    if (options === undefined) return Object.create(null);
    if (isObject(options)) return options;
    throw new TypeError('Options must be an object');
  }
  function getOption(opts, prop, type, values, fallback) {
    let v = opts[prop];
    if (v === undefined) return fallback;
    v = type === 'boolean' ? Boolean(v) : String(v);
    if (values !== undefined && values.indexOf(v) === -1) {
      throw new RangeError('Value ' + v + ' out of range for Intl options property ' + prop);
    }
    return v;
  }
  function defaultNumberOption(v, min, max, fallback, prop) {
    if (v === undefined) return fallback;
    v = Number(v);
    if (Number.isNaN(v) || v < min || v > max) throw new RangeError(prop + ' value is out of range.');
    return Math.floor(v);
  }
  const getNumberOption = (opts, prop, min, max, fallback) => defaultNumberOption(opts[prop], min, max, fallback, prop);
  const localeMatcher = (opts) => getOption(opts, 'localeMatcher', 'string', ['lookup', 'best fit'], 'best fit');

  // ---------------------------------------------------------------------
  // Language tags (BCP 47 / UTS 35)
  // ---------------------------------------------------------------------
  const LANGUAGE_ALIASES = { iw: 'he', in: 'id', ji: 'yi', jw: 'jv', mo: 'ro' };

  function parseTag(str) {
    if (typeof str !== 'string' || !/^[A-Za-z0-9]+(-[A-Za-z0-9]+)*$/.test(str)) return null;
    const parts = str.toLowerCase().split('-');
    const out = { language: parts[0], script: undefined, region: undefined, variants: [], extensions: [], privateuse: undefined };
    if (!/^([a-z]{2,3}|[a-z]{5,8})$/.test(out.language)) return null;
    let i = 1;
    if (i < parts.length && /^[a-z]{4}$/.test(parts[i])) out.script = parts[i][0].toUpperCase() + parts[i++].slice(1);
    if (i < parts.length && /^([a-z]{2}|\d{3})$/.test(parts[i])) out.region = parts[i++].toUpperCase();
    while (i < parts.length && /^([a-z\d]{5,8}|\d[a-z\d]{3})$/.test(parts[i])) {
      if (out.variants.indexOf(parts[i]) !== -1) return null;
      out.variants.push(parts[i++]);
    }
    const seen = Object.create(null);
    while (i < parts.length && /^[a-wyz\d]$/.test(parts[i])) {
      const singleton = parts[i++];
      if (seen[singleton]) return null;
      seen[singleton] = true;
      const sub = [];
      while (i < parts.length && /^[a-z\d]{2,8}$/.test(parts[i])) {
        if (singleton === 'u' && parts[i].length === 2 && !/^[a-z\d][a-z]$/.test(parts[i])) return null;
        sub.push(parts[i++]);
      }
      if (!sub.length) return null;
      out.extensions.push({ singleton, sub });
    }
    if (i < parts.length && parts[i] === 'x') {
      i++;
      const sub = [];
      while (i < parts.length && /^[a-z\d]{1,8}$/.test(parts[i])) sub.push(parts[i++]);
      if (!sub.length) return null;
      out.privateuse = sub;
    }
    return i === parts.length ? out : null;
  }

  // Attributes and `key -> type` keywords of a -u- extension.
  function parseUnicodeExtension(sub) {
    const attributes = [];
    const keywords = [];
    let i = 0;
    while (i < sub.length && sub[i].length > 2) attributes.push(sub[i++]);
    while (i < sub.length) {
      const key = sub[i++];
      const types = [];
      while (i < sub.length && sub[i].length > 2) types.push(sub[i++]);
      if (!keywords.some((k) => k[0] === key)) keywords.push([key, types.join('-')]);
    }
    return { attributes, keywords };
  }

  function tagToString(p) {
    let s = p.language;
    if (p.script) s += '-' + p.script;
    if (p.region) s += '-' + p.region;
    if (p.variants.length) s += '-' + p.variants.slice().sort().join('-');
    const exts = p.extensions.slice().sort((a, b) => (a.singleton < b.singleton ? -1 : 1));
    for (const e of exts) {
      let sub = e.sub;
      if (e.singleton === 'u') {
        const { attributes, keywords } = parseUnicodeExtension(sub);
        keywords.sort((a, b) => (a[0] < b[0] ? -1 : 1));
        sub = attributes.slice();
        for (const [k, v] of keywords) {
          sub.push(k);
          if (v && v !== 'true') sub.push(v);
        }
        if (!sub.length) continue;
      }
      s += '-' + e.singleton + '-' + sub.join('-');
    }
    if (p.privateuse) s += '-x-' + p.privateuse.join('-');
    return s;
  }

  function parseCanonical(str) {
    const p = parseTag(str);
    if (!p) throw new RangeError('Incorrect locale information provided');
    if (hasOwn(LANGUAGE_ALIASES, p.language)) p.language = LANGUAGE_ALIASES[p.language];
    return p;
  }
  const canonicalizeTag = (str) => tagToString(parseCanonical(str));

  function canonicalizeLocaleList(locales) {
    if (locales === undefined) return [];
    if (locales === null) throw new TypeError('Cannot convert undefined or null to object');
    const out = [];
    const list = typeof locales === 'string' || locales instanceof Locale ? [locales] : Object(locales);
    const len = Math.min(Math.max(0, Math.floor(Number(list.length)) || 0), 2 ** 32);
    for (let k = 0; k < len; k++) {
      if (!(k in list)) continue;
      const v = list[k];
      if (typeof v !== 'string' && !isObject(v)) throw new TypeError('Language ID should be string or object.');
      const t = v instanceof Locale ? LOCALE.get(v).tag : canonicalizeTag(String(v));
      if (out.indexOf(t) === -1) out.push(t);
    }
    return out;
  }

  // `{ base, keywords }`: the tag without its -u- extension, and the
  // extension's keywords (`kn` without a value is `true`).
  function splitExtension(t) {
    const parts = t.split('-');
    const keep = [parts[0]];
    const keywords = Object.create(null);
    for (let i = 1; i < parts.length;) {
      if (parts[i] === 'x') {
        keep.push(...parts.slice(i));
        break;
      }
      if (parts[i] === 'u') {
        let j = i + 1;
        const sub = [];
        while (j < parts.length && parts[j].length > 1) sub.push(parts[j++]);
        for (const [k, v] of parseUnicodeExtension(sub).keywords) keywords[k] = v === '' ? 'true' : v;
        i = j;
        continue;
      }
      keep.push(parts[i++]);
    }
    return { base: keep.join('-'), keywords };
  }

  function bestAvailable(available, t) {
    let candidate = t;
    for (;;) {
      if (available(candidate)) return candidate;
      let pos = candidate.lastIndexOf('-');
      if (pos < 0) return undefined;
      if (pos >= 2 && candidate.charAt(pos - 2) === '-') pos -= 2;
      candidate = candidate.slice(0, pos);
    }
  }

  // The first requested locale with data, or the default locale.
  function lookupLocale(available, requested) {
    for (const t of requested) {
      const { base, keywords } = splitExtension(t);
      const found = bestAvailable(available, base);
      if (found) return { locale: found, keywords };
    }
    return { locale: DEFAULT_LOCALE, keywords: Object.create(null) };
  }

  // Appends the extension keywords that were applied, in canonical order.
  function withKeywords(locale, applied) {
    const keys = Object.keys(applied).sort();
    if (!keys.length) return locale;
    return locale + '-u-' + keys.map((k) => (applied[k] === 'true' ? k : k + '-' + applied[k])).join('-');
  }

  function supportedLocalesOf(available) {
    return function supportedLocalesOf(locales, options) {
      const requested = canonicalizeLocaleList(locales);
      localeMatcher(coerceOptions(options));
      return requested.filter((t) => bestAvailable(available, splitExtension(t).base) !== undefined);
    };
  }

  const isEnglish = (t) => t === 'en' || t === 'en-US';
  const langOf = (locale) => locale.split('-')[0];
  // Languages with number data, by the regional variant the data is for.
  const NUMBER_REGIONS = { de: 'DE', es: 'ES', fr: 'FR', id: 'ID', it: 'IT', nl: 'NL', pt: 'BR' };
  const numberAvailable = (t) => {
    if (isEnglish(t)) return true;
    const p = t.split('-');
    return hasOwn(NUMBER_REGIONS, p[0]) && (p.length === 1 || (p.length === 2 && p[1] === NUMBER_REGIONS[p[0]]));
  };
  const COLLATION_LANGS = ['en', 'de', 'fr', 'id', 'it', 'ms', 'nl', 'pt'];
  const collatorAvailable = (t) => isEnglish(t) || (COLLATION_LANGS.indexOf(langOf(t)) !== -1 && t.split('-').length <= 2);

  // ---------------------------------------------------------------------
  // Number data (CLDR, as shipped with ICU 78)
  // ---------------------------------------------------------------------
  function table(spec) {
    const out = Object.create(null);
    for (const item of spec.split(' ')) {
      const i = item.indexOf('=');
      out[item.slice(0, i)] = item.slice(i + 1).replace(/_/g, NNBSP);
    }
    return out;
  }
  const CURRENCY_DIGITS = (() => {
    const out = Object.create(null);
    'AFN,ALL,BIF,CLP,COP,DJF,GNF,HUF,IDR,IQD,IRR,ISK,JPY,KMF,KPW,KRW,LAK,LBP,MGA,MMK,PKR,PYG,RWF,SLL,SOS,SYP,UGX,VND,VUV,XAF,XOF,XPF,YER'
      .split(',').forEach((c) => { out[c] = 0; });
    'BHD,JOD,KWD,LYD,OMR,TND'.split(',').forEach((c) => { out[c] = 3; });
    return out;
  })();
  const EN_SYMBOLS = table('AUD=A$ BRL=R$ CAD=CA$ CNY=CN¥ EUR=€ GBP=£ HKD=HK$ ILS=₪ INR=₹ JPY=¥ KRW=₩ MXN=MX$ NZD=NZ$ PHP=₱ ' +
    'TWD=NT$ USD=$ VND=₫ XAF=FCFA XCD=EC$ XCG=Cg. XOF=F_CFA XPF=CFPF');
  const NARROW_SYMBOLS = table('AFN=؋ AMD=֏ AOA=Kz ARS=$ AUD=$ AZN=₼ BAM=KM BBD=$ BDT=৳ BMD=$ BND=$ BOB=Bs BSD=$ BWP=P BZD=$ ' +
    'CAD=$ CLP=$ CNY=¥ COP=$ CRC=₡ CUC=$ CUP=$ CZK=Kč DKK=kr DOP=$ EGP=E£ FJD=$ FKP=£ GEL=₾ GHS=GH₵ GIP=£ GNF=FG GTQ=Q ' +
    'GYD=$ HKD=$ HNL=L HRK=kn HUF=Ft IDR=Rp ISK=kr JMD=$ KHR=៛ KMF=CF KPW=₩ KYD=$ KZT=₸ LAK=₭ LBP=L£ LKR=Rs LRD=$ ' +
    'MGA=Ar MMK=K MNT=₮ MUR=Rs MXN=$ MYR=RM NAD=$ NGN=₦ NIO=C$ NOK=kr NPR=Rs NZD=$ PKR=Rs PLN=zł PYG=₲ RON=lei RUB=₽ ' +
    'RWF=RF SBD=$ SEK=kr SGD=$ SHP=£ SRD=$ SSP=£ STN=Db SYP=£ THB=฿ TOP=T$ TRY=₺ TTD=$ TWD=$ UAH=₴ UYU=$ XCD=$ ZAR=R ZMW=ZK');
  const CURRENCY_NAMES_SPEC = 'AED=UAE dirham|UAE dirhams;ARS=Argentine peso|Argentine pesos;AUD=Australian dollar|Australian dollars;' +
    'BDT=Bangladeshi taka|Bangladeshi takas;BGN=Bulgarian lev|Bulgarian leva;BHD=Bahraini dinar|Bahraini dinars;' +
    'BRL=Brazilian real|Brazilian reals;CAD=Canadian dollar|Canadian dollars;CHF=Swiss franc|Swiss francs;' +
    'CLP=Chilean peso|Chilean pesos;CNY=Chinese yuan;COP=Colombian peso|Colombian pesos;CZK=Czech koruna|Czech korunas;' +
    'DKK=Danish krone|Danish kroner;EGP=Egyptian pound|Egyptian pounds;EUR=euro|euros;GBP=British pound|British pounds;' +
    'HKD=Hong Kong dollar|Hong Kong dollars;HUF=Hungarian forint|Hungarian forints;IDR=Indonesian rupiah|Indonesian rupiahs;' +
    'ILS=Israeli new shekel|Israeli new shekels;INR=Indian rupee|Indian rupees;ISK=Icelandic króna|Icelandic krónur;' +
    'JPY=Japanese yen;KES=Kenyan shilling|Kenyan shillings;KRW=South Korean won;KWD=Kuwaiti dinar|Kuwaiti dinars;' +
    'MXN=Mexican peso|Mexican pesos;MYR=Malaysian ringgit|Malaysian ringgits;NGN=Nigerian naira|Nigerian nairas;' +
    'NOK=Norwegian krone|Norwegian kroner;NZD=New Zealand dollar|New Zealand dollars;PEN=Peruvian sol|Peruvian soles;' +
    'PHP=Philippine peso|Philippine pesos;PKR=Pakistani rupee|Pakistani rupees;PLN=Polish zloty|Polish zlotys;' +
    'QAR=Qatari riyal|Qatari riyals;RON=Romanian leu|Romanian lei;RUB=Russian ruble|Russian rubles;' +
    'SAR=Saudi riyal|Saudi riyals;SEK=Swedish krona|Swedish kronor;SGD=Singapore dollar|Singapore dollars;THB=Thai baht;' +
    'TRY=Turkish lira|Turkish Lira;TWD=New Taiwan dollar|New Taiwan dollars;UAH=Ukrainian hryvnia|Ukrainian hryvnias;' +
    'USD=US dollar|US dollars;VND=Vietnamese dong;ZAR=South African rand';
  let currencyNames = null;
  function currencyName(code, one) {
    if (!currencyNames) {
      currencyNames = Object.create(null);
      for (const item of CURRENCY_NAMES_SPEC.split(';')) {
        const [c, names] = item.split('=');
        const [singular, plural] = names.split('|');
        currencyNames[c] = [singular, plural || singular];
      }
    }
    const n = currencyNames[code];
    return n ? n[one ? 0 : 1] : code;
  }

  // Per-language number symbols and patterns. `currency`: symbol before or
  // after the number, the space between them ('' = CLDR currency spacing:
  // a no-break space next to letters), and whether the sign sits between
  // the symbol and the number.
  const NUMBER_DATA = {
    en: { decimal: '.', group: ',', minGroup: 1, percent: '', before: true, space: '', symbols: EN_SYMBOLS },
    de: { decimal: ',', group: '.', minGroup: 1, percent: NBSP, before: false, space: NBSP,
      symbols: 'AUD=AU$ BRL=R$ CAD=CA$ CNY=CN¥ EUR=€ GBP=£ HKD=HK$ ILS=₪ INR=₹ JPY=¥ KRW=₩ MXN=MX$ NZD=NZ$ THB=฿ TWD=NT$ USD=$ VND=₫' },
    es: { decimal: ',', group: '.', minGroup: 2, percent: NBSP, before: false, space: NBSP, symbols: 'EUR=€ THB=฿ USD=US$ VND=₫' },
    fr: { decimal: ',', group: NNBSP, minGroup: 1, percent: NBSP, before: false, space: NBSP,
      symbols: 'ARS=$AR AUD=$AU BRL=R$ CAD=$CA CLP=$CL COP=$CO EUR=€ GBP=£GB ILS=₪ INR=₹ KRW=₩ MXN=$MX NZD=$NZ SGD=$SG USD=$US VND=₫' },
    id: { decimal: ',', group: '.', minGroup: 1, percent: '', before: true, space: '',
      symbols: 'AUD=AU$ BRL=R$ CAD=CA$ CNY=CN¥ EUR=€ GBP=£ HKD=HK$ IDR=Rp ILS=₪ INR=Rs JPY=JP¥ KRW=₩ MXN=MX$ NZD=NZ$ THB=฿ TWD=NT$ USD=US$ VND=₫' },
    it: { decimal: ',', group: '.', minGroup: 2, percent: '', before: false, space: NBSP,
      symbols: 'AUD=A$ CAD=CA$ CNY=CN¥ EUR=€ GBP=£ ILS=₪ NZD=NZ$ PHP=₱ THB=฿' },
    nl: { decimal: ',', group: '.', minGroup: 1, percent: '', before: true, space: NBSP, signAfterSymbol: true,
      symbols: 'AUD=AU$ BRL=R$ CAD=C$ CNY=CN¥ EUR=€ GBP=£ HKD=HK$ ILS=₪ INR=₹ JPY=JP¥ KRW=₩ MXN=MX$ NZD=NZ$ THB=฿ TWD=NT$ USD=US$ VND=₫' },
    pt: { decimal: ',', group: '.', minGroup: 1, percent: '', before: true, space: NBSP,
      symbols: 'AUD=AU$ BRL=R$ CAD=CA$ CNY=CN¥ EUR=€ GBP=£ HKD=HK$ ILS=₪ INR=₹ JPY=JP¥ KRW=₩ MXN=MX$ NZD=NZ$ THB=฿ TWD=NT$ USD=US$ VND=₫' },
  };
  function numberData(locale) {
    const d = NUMBER_DATA[langOf(locale)] || NUMBER_DATA.en;
    if (typeof d.symbols === 'string') d.symbols = table(d.symbols);
    return d;
  }

  // Units: short one/other, narrow one/other, long one/other.
  const UNIT_SPEC = 'acre={0} ac|{0} ac|{0}ac|{0}ac|{0} acre|{0} acres;bit={0} bit|{0} bit|{0}bit|{0}bit|{0} bit|{0} bits;' +
    'byte={0} byte|{0} byte|{0}B|{0}B|{0} byte|{0} bytes;celsius={0}°C|{0}°C|{0}°C|{0}°C|{0} degree Celsius|{0} degrees Celsius;' +
    'centimeter={0} cm|{0} cm|{0}cm|{0}cm|{0} centimeter|{0} centimeters;day={0} day|{0} days|{0}d|{0}d|{0} day|{0} days;' +
    'degree={0} deg|{0} deg|{0}°|{0}°|{0} degree|{0} degrees;' +
    'fahrenheit={0}°F|{0}°F|{0}°|{0}°|{0} degree Fahrenheit|{0} degrees Fahrenheit;' +
    'fluid-ounce={0} fl oz|{0} fl oz|{0}fl oz|{0}fl oz|{0} fluid ounce|{0} fluid ounces;' +
    'foot={0} ft|{0} ft|{0}′|{0}′|{0} foot|{0} feet;gallon={0} gal|{0} gal|{0}gal|{0}gal|{0} gallon|{0} gallons;' +
    'gigabit={0} Gb|{0} Gb|{0}Gb|{0}Gb|{0} gigabit|{0} gigabits;gigabyte={0} GB|{0} GB|{0}GB|{0}GB|{0} gigabyte|{0} gigabytes;' +
    'gram={0} g|{0} g|{0}g|{0}g|{0} gram|{0} grams;hectare={0} ha|{0} ha|{0}ha|{0}ha|{0} hectare|{0} hectares;' +
    'hour={0} hr|{0} hr|{0}h|{0}h|{0} hour|{0} hours;inch={0} in|{0} in|{0}″|{0}″|{0} inch|{0} inches;' +
    'kilobit={0} kb|{0} kb|{0}kb|{0}kb|{0} kilobit|{0} kilobits;kilobyte={0} kB|{0} kB|{0}kB|{0}kB|{0} kilobyte|{0} kilobytes;' +
    'kilogram={0} kg|{0} kg|{0}kg|{0}kg|{0} kilogram|{0} kilograms;' +
    'kilometer={0} km|{0} km|{0}km|{0}km|{0} kilometer|{0} kilometers;liter={0} L|{0} L|{0}L|{0}L|{0} liter|{0} liters;' +
    'megabit={0} Mb|{0} Mb|{0}Mb|{0}Mb|{0} megabit|{0} megabits;megabyte={0} MB|{0} MB|{0}MB|{0}MB|{0} megabyte|{0} megabytes;' +
    'meter={0} m|{0} m|{0}m|{0}m|{0} meter|{0} meters;' +
    'microsecond={0} μs|{0} μs|{0}μs|{0}μs|{0} microsecond|{0} microseconds;mile={0} mi|{0} mi|{0}mi|{0}mi|{0} mile|{0} miles;' +
    'mile-scandinavian={0} smi|{0} smi|{0}smi|{0}smi|{0} mile-scandinavian|{0} miles-scandinavian;' +
    'milliliter={0} mL|{0} mL|{0}mL|{0}mL|{0} milliliter|{0} milliliters;' +
    'millimeter={0} mm|{0} mm|{0}mm|{0}mm|{0} millimeter|{0} millimeters;' +
    'millisecond={0} ms|{0} ms|{0}ms|{0}ms|{0} millisecond|{0} milliseconds;' +
    'minute={0} min|{0} min|{0}m|{0}m|{0} minute|{0} minutes;month={0} mth|{0} mths|{0}m|{0}m|{0} month|{0} months;' +
    'nanosecond={0} ns|{0} ns|{0}ns|{0}ns|{0} nanosecond|{0} nanoseconds;ounce={0} oz|{0} oz|{0}oz|{0}oz|{0} ounce|{0} ounces;' +
    'percent={0}%|{0}%|{0}%|{0}%|{0} percent|{0} percent;petabyte={0} PB|{0} PB|{0}PB|{0}PB|{0} petabyte|{0} petabytes;' +
    'pound={0} lb|{0} lb|{0}#|{0}#|{0} pound|{0} pounds;second={0} sec|{0} sec|{0}s|{0}s|{0} second|{0} seconds;' +
    'stone={0} st|{0} st|{0}st|{0}st|{0} stone|{0} stones;terabit={0} Tb|{0} Tb|{0}Tb|{0}Tb|{0} terabit|{0} terabits;' +
    'terabyte={0} TB|{0} TB|{0}TB|{0}TB|{0} terabyte|{0} terabytes;week={0} wk|{0} wks|{0}w|{0}w|{0} week|{0} weeks;' +
    'yard={0} yd|{0} yd|{0}yd|{0}yd|{0} yard|{0} yards;year={0} yr|{0} yrs|{0}y|{0}y|{0} year|{0} years';
  let units = null;
  function unitTable() {
    if (!units) {
      units = Object.create(null);
      for (const item of UNIT_SPEC.split(';')) {
        const i = item.indexOf('=');
        units[item.slice(0, i)] = item.slice(i + 1).split('|');
      }
    }
    return units;
  }
  // The `/x` form of a unit after `per`.
  const PER_UNIT = { hour: 'h', second: 's', day: 'd', week: 'w', month: 'm', year: 'y', minute: 'min' };
  const SPECIAL_PER = { 'mile-per-hour': ['{0} mph', '{0}mph'] };
  function isWellFormedUnit(u) {
    const t = unitTable();
    if (hasOwn(t, u)) return true;
    const i = u.indexOf('-per-');
    return i > 0 && hasOwn(t, u.slice(0, i)) && hasOwn(t, u.slice(i + 5));
  }
  // Pattern with `{0}` for a unit, display and plural form.
  function unitPattern(unit, display, one) {
    const t = unitTable();
    const col = (display === 'short' ? 0 : display === 'narrow' ? 2 : 4) + (one ? 0 : 1);
    if (hasOwn(t, unit)) return t[unit][col];
    const i = unit.indexOf('-per-');
    const num = unit.slice(0, i);
    const den = unit.slice(i + 5);
    if (display === 'long') return t[num][col] + ' per ' + t[den][4].replace('{0} ', '');
    if (hasOwn(SPECIAL_PER, unit)) return SPECIAL_PER[unit][display === 'short' ? 0 : 1];
    const perSym = PER_UNIT[den] || t[den][2].replace('{0}', '');
    return t[num][col] + '/' + perSym;
  }

  // ---------------------------------------------------------------------
  // Exact decimal arithmetic on digit strings. A decimal is `{ digits, pos }`
  // with no leading or trailing zeros in `digits` ('' is zero) and `pos`
  // digits before the decimal point; numbers use their shortest round-trip
  // representation, as ICU does.
  // ---------------------------------------------------------------------
  const ZERO = { digits: '', pos: 1 };

  function parseDigits(mant, exp) {
    const dot = mant.indexOf('.');
    const int = dot === -1 ? mant : mant.slice(0, dot);
    let digits = int + (dot === -1 ? '' : mant.slice(dot + 1));
    let pos = int.length + exp;
    let a = 0;
    while (a < digits.length && digits.charCodeAt(a) === 48) a++;
    let b = digits.length;
    while (b > a && digits.charCodeAt(b - 1) === 48) b--;
    digits = digits.slice(a, b);
    pos -= a;
    return digits ? { digits, pos } : { digits: '', pos: 1 };
  }

  // ToIntlMathematicalValue: Number, BigInt or (exact) numeric string.
  function toIntlValue(value) {
    if (isObject(value)) {
      const prim = value[Symbol.toPrimitive] ? value[Symbol.toPrimitive]('number') : value.valueOf();
      value = isObject(prim) ? String(value) : prim;
    }
    if (typeof value === 'bigint' || typeof value === 'string') return value;
    return Number(value);
  }

  // `{ neg, digits, pos }` or `{ neg, special: 'nan' | 'inf' }`.
  function toDecimal(x) {
    if (typeof x === 'bigint') {
      const neg = x < 0n;
      return Object.assign(parseDigits((neg ? -x : x).toString(), 0), { neg });
    }
    if (typeof x === 'string') {
      const s = x.trim();
      if (s === '') return { digits: '', pos: 1, neg: false };
      const m = /^([+-]?)(\d+\.?\d*|\.\d+)(?:[eE]([+-]?\d+))?$/.exec(s);
      if (m && (!m[3] || Math.abs(Number(m[3])) < 10000)) {
        return Object.assign(parseDigits(m[2], m[3] ? parseInt(m[3], 10) : 0), { neg: m[1] === '-' });
      }
      if (/^[+-]?Infinity$/.test(s)) return { neg: s.charAt(0) === '-', special: 'inf' };
      x = Number(s);
    }
    if (Number.isNaN(x)) return { neg: false, special: 'nan' };
    const neg = x < 0 || Object.is(x, -0);
    if (!Number.isFinite(x)) return { neg, special: 'inf' };
    const m = /^(\d+(?:\.\d+)?)(?:e([+-]\d+))?$/.exec(String(Math.abs(x)));
    return Object.assign(parseDigits(m[1], m[2] ? parseInt(m[2], 10) : 0), { neg });
  }

  const shift = (d, k) => (d.digits ? { digits: d.digits, pos: d.pos + k } : ZERO);

  // Keeps the first `n` digits (n may be <= 0), rounding by `mode`.
  function roundDigits(d, n, mode, neg) {
    const digits = d.digits;
    if (!digits || n >= digits.length) return d;
    const first = n >= 0 ? digits.charCodeAt(n) - 48 : 0;
    const rest = n >= 0 ? digits.length > n + 1 : true;
    let up;
    switch (mode) {
      case 'ceil': up = !neg; break;
      case 'floor': up = neg; break;
      case 'expand': up = true; break;
      case 'trunc': up = false; break;
      default: {
        if (first > 5 || (first === 5 && rest)) up = true;
        else if (first < 5) up = false;
        else if (mode === 'halfExpand') up = true;
        else if (mode === 'halfTrunc') up = false;
        else if (mode === 'halfEven') up = n > 0 && (digits.charCodeAt(n - 1) - 48) % 2 === 1;
        else if (mode === 'halfCeil') up = !neg;
        else up = neg;
      }
    }
    let pos = d.pos;
    if (!up) {
      if (n <= 0) return ZERO;
      return parseDigits(digits.slice(0, n), pos - n);
    }
    if (n <= 0) return { digits: '1', pos: pos - n + 1 };
    const arr = digits.slice(0, n).split('');
    let i = n - 1;
    while (i >= 0 && arr[i] === '9') arr[i--] = '0';
    if (i < 0) {
      arr.unshift('1');
      pos++;
    } else arr[i] = String.fromCharCode(arr[i].charCodeAt(0) + 1);
    return parseDigits(arr.join(''), pos - arr.length);
  }

  function splitDecimal(d) {
    const { digits, pos } = d;
    if (!digits) return { int: '0', frac: '' };
    if (pos <= 0) return { int: '0', frac: '0'.repeat(-pos) + digits };
    if (pos >= digits.length) return { int: digits + '0'.repeat(pos - digits.length), frac: '' };
    return { int: digits.slice(0, pos), frac: digits.slice(pos) };
  }

  function toPrecision(d, minP, maxP, mode, neg) {
    const r = roundDigits(d, maxP, mode, neg);
    const s = splitDecimal(r);
    const shown = r.digits ? Math.max(r.digits.length, r.pos) : 1;
    if (shown < minP) s.frac += '0'.repeat(minP - shown);
    s.value = r;
    s.magnitude = (d.digits ? d.pos : 1) - maxP;
    return s;
  }

  function toFixed(d, minF, maxF, increment, mode, neg) {
    let r;
    if (increment === 1) r = roundDigits(d, d.pos + maxF, mode, neg);
    else r = roundToIncrement(d, maxF, increment, mode, neg);
    const s = splitDecimal(r);
    if (s.frac.length < minF) s.frac += '0'.repeat(minF - s.frac.length);
    s.value = r;
    s.magnitude = -maxF;
    return s;
  }

  // Rounds to a multiple of `increment` x 10^-maxF (exact, with BigInt).
  function roundToIncrement(d, maxF, increment, mode, neg) {
    if (!d.digits) return d;
    const scaled = splitDecimal(shift(d, maxF));
    const total = BigInt(scaled.int + scaled.frac);
    const unit = BigInt(increment) * 10n ** BigInt(scaled.frac.length);
    let q = total / unit;
    const rem = total % unit;
    if (rem !== 0n) {
      const twice = rem * 2n;
      const half = twice > unit ? 1 : twice === unit ? 0 : -1;
      let up;
      switch (mode) {
        case 'ceil': up = !neg; break;
        case 'floor': up = neg; break;
        case 'expand': up = true; break;
        case 'trunc': up = false; break;
        case 'halfTrunc': up = half > 0; break;
        case 'halfEven': up = half > 0 || (half === 0 && q % 2n === 1n); break;
        case 'halfCeil': up = half > 0 || (half === 0 && !neg); break;
        case 'halfFloor': up = half > 0 || (half === 0 && neg); break;
        default: up = half >= 0;
      }
      if (up) q += 1n;
    }
    return parseDigits(String(q * BigInt(increment)), -maxF);
  }

  // ---------------------------------------------------------------------
  // NumberFormat
  // ---------------------------------------------------------------------
  const NF = new WeakMap();
  const ROUNDING_INCREMENTS = [1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000];
  const ROUNDING_MODES = ['ceil', 'floor', 'expand', 'trunc', 'halfCeil', 'halfFloor', 'halfExpand', 'halfTrunc', 'halfEven'];

  // SetNumberFormatDigitOptions.
  function digitOptions(r, opts, mnfdDefault, mxfdDefault, notation) {
    const mnid = getNumberOption(opts, 'minimumIntegerDigits', 1, 21, 1);
    let mnfd = opts.minimumFractionDigits;
    let mxfd = opts.maximumFractionDigits;
    let mnsd = opts.minimumSignificantDigits;
    let mxsd = opts.maximumSignificantDigits;
    r.minimumIntegerDigits = mnid;
    const increment = getNumberOption(opts, 'roundingIncrement', 1, 5000, 1);
    if (ROUNDING_INCREMENTS.indexOf(increment) === -1) throw new RangeError('roundingIncrement value is out of range.');
    r.roundingIncrement = increment;
    r.roundingMode = getOption(opts, 'roundingMode', 'string', ROUNDING_MODES, 'halfExpand');
    const priority = getOption(opts, 'roundingPriority', 'string', ['auto', 'morePrecision', 'lessPrecision'], 'auto');
    r.trailingZeroDisplay = getOption(opts, 'trailingZeroDisplay', 'string', ['auto', 'stripIfInteger'], 'auto');
    if (increment !== 1) mxfdDefault = mnfdDefault;
    const hasSd = mnsd !== undefined || mxsd !== undefined;
    const hasFd = mnfd !== undefined || mxfd !== undefined;
    let needSd = true;
    let needFd = true;
    if (priority === 'auto') {
      needSd = hasSd;
      if (needSd || (!hasFd && notation === 'compact')) needFd = false;
    }
    if (needSd) {
      if (hasSd) {
        mnsd = defaultNumberOption(mnsd, 1, 21, 1, 'minimumSignificantDigits');
        mxsd = defaultNumberOption(mxsd, mnsd, 21, 21, 'maximumSignificantDigits');
      } else {
        mnsd = 1;
        mxsd = 21;
      }
      r.minimumSignificantDigits = mnsd;
      r.maximumSignificantDigits = mxsd;
    }
    if (needFd) {
      if (hasFd) {
        mnfd = defaultNumberOption(mnfd, 0, 100, undefined, 'minimumFractionDigits');
        mxfd = defaultNumberOption(mxfd, 0, 100, undefined, 'maximumFractionDigits');
        if (mnfd === undefined) mnfd = Math.min(mnfdDefault, mxfd);
        else if (mxfd === undefined) mxfd = Math.max(mxfdDefault, mnfd);
        else if (mnfd > mxfd) throw new RangeError('maximumFractionDigits value is out of range.');
      } else {
        mnfd = mnfdDefault;
        mxfd = mxfdDefault;
      }
      r.minimumFractionDigits = mnfd;
      r.maximumFractionDigits = mxfd;
    }
    if (!needSd && !needFd) {
      r.minimumFractionDigits = 0;
      r.maximumFractionDigits = 0;
      r.minimumSignificantDigits = 1;
      r.maximumSignificantDigits = 2;
      r.roundingType = 'morePrecision';
      r.roundingPriority = 'morePrecision';
    } else if (priority === 'auto') {
      r.roundingType = needSd ? 'significantDigits' : 'fractionDigits';
      r.roundingPriority = 'auto';
    } else {
      r.roundingType = priority;
      r.roundingPriority = priority;
    }
    if (increment !== 1) {
      if (r.roundingType !== 'fractionDigits') throw new TypeError('roundingIncrement requires fraction digits rounding');
      if (r.maximumFractionDigits !== r.minimumFractionDigits) {
        throw new RangeError('maximumFractionDigits and minimumFractionDigits must be equal with roundingIncrement');
      }
    }
  }

  // FormatNumericToString: `{ int, frac, value }` after rounding.
  function roundNumber(r, d, neg) {
    let res;
    if (r.roundingType === 'significantDigits') {
      res = toPrecision(d, r.minimumSignificantDigits, r.maximumSignificantDigits, r.roundingMode, neg);
    } else if (r.roundingType === 'fractionDigits') {
      res = toFixed(d, r.minimumFractionDigits, r.maximumFractionDigits, r.roundingIncrement, r.roundingMode, neg);
    } else {
      const s = toPrecision(d, r.minimumSignificantDigits, r.maximumSignificantDigits, r.roundingMode, neg);
      const f = toFixed(d, r.minimumFractionDigits, r.maximumFractionDigits, 1, r.roundingMode, neg);
      const fixedMorePrecise = s.magnitude > f.magnitude;
      res = (r.roundingType === 'morePrecision') === fixedMorePrecise ? f : s;
    }
    if (r.trailingZeroDisplay === 'stripIfInteger' && /^0*$/.test(res.frac)) res.frac = '';
    if (res.int.length < r.minimumIntegerDigits) res.int = '0'.repeat(r.minimumIntegerDigits - res.int.length) + res.int;
    return res;
  }

  function resolveNumberFormat(locales, options) {
    const requested = canonicalizeLocaleList(locales);
    const opts = coerceOptions(options);
    const r = {};
    localeMatcher(opts);
    const nu = getOption(opts, 'numberingSystem', 'string', undefined, undefined);
    if (nu !== undefined && !TYPE_RE.test(nu)) throw new RangeError('Invalid numberingSystem : ' + nu);
    r.style = getOption(opts, 'style', 'string', ['decimal', 'percent', 'currency', 'unit'], 'decimal');
    let currency = getOption(opts, 'currency', 'string', undefined, undefined);
    if (currency !== undefined && !/^[A-Za-z]{3}$/.test(currency)) throw new RangeError('Invalid currency code : ' + currency);
    const currencyDisplay = getOption(opts, 'currencyDisplay', 'string', ['code', 'symbol', 'narrowSymbol', 'name'], 'symbol');
    const currencySign = getOption(opts, 'currencySign', 'string', ['standard', 'accounting'], 'standard');
    const unit = getOption(opts, 'unit', 'string', undefined, undefined);
    if (unit !== undefined && !isWellFormedUnit(unit)) throw new RangeError('Invalid unit argument for Intl.NumberFormat() \'' + unit + '\'');
    const unitDisplay = getOption(opts, 'unitDisplay', 'string', ['short', 'narrow', 'long'], 'short');
    if (r.style === 'currency') {
      if (currency === undefined) throw new TypeError('Currency code is required with currency style.');
      currency = currency.toUpperCase();
      r.currency = currency;
      r.currencyDisplay = currencyDisplay;
      r.currencySign = currencySign;
    }
    if (r.style === 'unit') {
      if (unit === undefined) throw new TypeError('Unit is required with unit style.');
      r.unit = unit;
      r.unitDisplay = unitDisplay;
    }
    r.notation = getOption(opts, 'notation', 'string', ['standard', 'scientific', 'engineering', 'compact'], 'standard');
    let mnfdDefault = 0;
    let mxfdDefault = r.style === 'percent' ? 0 : 3;
    if (r.style === 'currency' && r.notation === 'standard') {
      mnfdDefault = mxfdDefault = hasOwn(CURRENCY_DIGITS, currency) ? CURRENCY_DIGITS[currency] : 2;
    }
    digitOptions(r, opts, mnfdDefault, mxfdDefault, r.notation);
    const compactDisplay = getOption(opts, 'compactDisplay', 'string', ['short', 'long'], 'short');
    if (r.notation === 'compact') r.compactDisplay = compactDisplay;
    const defaultGrouping = r.notation === 'compact' ? 'min2' : 'auto';
    let grouping = opts.useGrouping;
    if (grouping === undefined) grouping = defaultGrouping;
    else if (grouping === true) grouping = 'always';
    else if (!grouping) grouping = false;
    else {
      grouping = String(grouping);
      if (grouping === 'true' || grouping === 'false') grouping = defaultGrouping;
      else if (['min2', 'auto', 'always'].indexOf(grouping) === -1) {
        throw new RangeError('Value ' + grouping + ' out of range for Intl.NumberFormat options property useGrouping');
      }
    }
    r.useGrouping = grouping;
    r.signDisplay = getOption(opts, 'signDisplay', 'string', ['auto', 'never', 'always', 'exceptZero', 'negative'], 'auto');
    // Only English has compact, unit and currency-name data.
    const englishOnly = r.notation === 'compact' || r.style === 'unit' || (r.style === 'currency' && currencyDisplay === 'name');
    const found = lookupLocale(englishOnly ? isEnglish : numberAvailable, requested);
    const applied = {};
    if (found.keywords.nu === 'latn' && nu === undefined) applied.nu = 'latn';
    r.locale = withKeywords(found.locale, applied);
    r.numberingSystem = 'latn';
    r.data = numberData(found.locale);
    return r;
  }

  function groupInteger(int, r) {
    const data = r.data;
    const mode = r.useGrouping;
    if (mode === false) return [int];
    const min = mode === 'always' ? 1 : mode === 'min2' ? 2 : data.minGroup;
    if (int.length < 3 + min) return [int];
    const groups = [];
    let i = int.length;
    while (i > 3) {
      groups.unshift(int.slice(i - 3, i));
      i -= 3;
    }
    groups.unshift(int.slice(0, i));
    return groups;
  }

  function signOf(r, neg, zero) {
    switch (r.signDisplay) {
      case 'never': return '';
      case 'always': return neg ? '-' : '+';
      case 'exceptZero': return zero ? '' : neg ? '-' : '+';
      case 'negative': return neg && !zero ? '-' : '';
      default: return neg ? '-' : '';
    }
  }

  const COMPACT_SHORT = ['K', 'M', 'B', 'T'];
  const COMPACT_LONG = ['thousand', 'million', 'billion', 'trillion'];

  // The number parts (integer..exponent/compact), sign and plural form.
  function numberCore(r, x) {
    const d0 = toDecimal(x);
    const data = r.data;
    if (d0.special) {
      const parts = [d0.special === 'nan' ? { type: 'nan', value: 'NaN' } : { type: 'infinity', value: '∞' }];
      const sign = d0.special === 'nan' ? (r.signDisplay === 'always' ? '+' : '') : signOf(r, d0.neg, false);
      return { parts, sign, one: false };
    }
    let d = r.style === 'percent' ? shift(d0, 2) : d0;
    const neg = d0.neg;
    let exponent = null;
    let compact = 0;
    let res;
    if (r.notation === 'scientific' || r.notation === 'engineering') {
      const step = r.notation === 'engineering' ? 3 : 1;
      let e = d.digits ? Math.floor((d.pos - 1) / step) * step : 0;
      res = roundNumber(r, shift(d, -e), neg);
      if (res.value.digits && res.value.pos - 1 >= step) {
        e += step;
        res = roundNumber(r, shift(d, -e), neg);
      }
      exponent = e;
    } else if (r.notation === 'compact') {
      const m = d.digits ? d.pos - 1 : 0;
      let e = m < 3 ? 0 : Math.min(Math.floor(m / 3) * 3, 12);
      res = roundNumber(r, shift(d, -e), neg);
      if (e < 12 && res.value.digits && res.value.pos - 1 >= 3) {
        e += 3;
        res = roundNumber(r, shift(d, -e), neg);
      }
      compact = e;
    } else {
      res = roundNumber(r, d, neg);
    }
    const zero = !res.value.digits;
    const parts = [];
    groupInteger(res.int, r).forEach((g, i) => {
      if (i) parts.push({ type: 'group', value: data.group });
      parts.push({ type: 'integer', value: g });
    });
    if (res.frac) {
      parts.push({ type: 'decimal', value: data.decimal });
      parts.push({ type: 'fraction', value: res.frac });
    }
    if (exponent !== null) {
      parts.push({ type: 'exponentSeparator', value: 'E' });
      if (exponent < 0) parts.push({ type: 'exponentMinusSign', value: '-' });
      parts.push({ type: 'exponentInteger', value: String(Math.abs(exponent)) });
    }
    if (compact) {
      const i = compact / 3 - 1;
      if (r.compactDisplay === 'long') parts.push({ type: 'literal', value: ' ' }, { type: 'compact', value: COMPACT_LONG[i] });
      else parts.push({ type: 'compact', value: COMPACT_SHORT[i] });
    }
    const one = res.int === '1' && res.frac === '' && !compact && exponent === null;
    return { parts, sign: signOf(r, neg, zero), one };
  }

  function currencySymbol(r) {
    const code = r.currency;
    if (r.currencyDisplay === 'code') return code;
    const symbols = r.data.symbols;
    if (r.currencyDisplay === 'narrowSymbol') return NARROW_SYMBOLS[code] || symbols[code] || code;
    return symbols[code] || code;
  }

  // Splits a `{0}` pattern into literal and `type` parts around the number.
  function affixParts(text, type) {
    const out = [];
    const m = /^(\s*)([\s\S]*?)(\s*)$/.exec(text);
    if (m[1]) out.push({ type: 'literal', value: m[1] });
    if (m[2]) out.push({ type, value: m[2] });
    if (m[3]) out.push({ type: 'literal', value: m[3] });
    return out;
  }

  function partitionNumber(r, x) {
    const core = numberCore(r, x);
    const signPart = core.sign === '-' ? [{ type: 'minusSign', value: '-' }] : core.sign === '+' ? [{ type: 'plusSign', value: '+' }] : [];
    const num = core.parts;
    const data = r.data;
    switch (r.style) {
      case 'percent': {
        const pct = data.percent ? [{ type: 'literal', value: data.percent }] : [];
        return signPart.concat(num, pct, [{ type: 'percentSign', value: '%' }]);
      }
      case 'unit': {
        const pat = unitPattern(r.unit, r.unitDisplay, core.one);
        const at = pat.indexOf('{0}');
        return affixParts(pat.slice(0, at), 'unit').concat(signPart, num, affixParts(pat.slice(at + 3), 'unit'));
      }
      case 'currency': {
        if (r.currencyDisplay === 'name') {
          return signPart.concat(num, [{ type: 'literal', value: ' ' }, { type: 'currency', value: currencyName(r.currency, core.one) }]);
        }
        const sym = currencySymbol(r);
        const cur = [{ type: 'currency', value: sym }];
        if (!data.before) return signPart.concat(num, [{ type: 'literal', value: data.space }], cur);
        let space = data.space;
        if (!space && /[^\p{S}]$/u.test(sym) && num[0].type === 'integer') space = NBSP;
        const gap = space ? [{ type: 'literal', value: space }] : [];
        if (core.sign === '-' && r.currencySign === 'accounting' && langOf(r.locale) === 'en') {
          return [{ type: 'literal', value: '(' }].concat(cur, gap, num, [{ type: 'literal', value: ')' }]);
        }
        if (data.signAfterSymbol) return cur.concat(gap, signPart, num);
        return signPart.concat(cur, gap, num);
      }
      default:
        return signPart.concat(num);
    }
  }

  const joinParts = (parts) => parts.map((p) => p.value).join('');

  function NumberFormat(locales, options) {
    if (!new.target) return new NumberFormat(locales, options);
    NF.set(this, resolveNumberFormat(locales, options));
  }
  const nfSlots = (o, m) => slotsOf(NF, o, 'Intl.NumberFormat.prototype.' + m);

  function rangeParts(r, start, end) {
    if (start === undefined || end === undefined) throw new TypeError('start or end is undefined');
    const x = toIntlValue(start);
    const y = toIntlValue(end);
    if (toDecimal(x).special === 'nan' || toDecimal(y).special === 'nan') throw new RangeError('Invalid number range');
    const a = partitionNumber(r, x);
    const b = partitionNumber(r, y);
    if (joinParts(a) === joinParts(b)) {
      return [{ type: 'approximatelySign', value: '~', source: 'shared' }].concat(a.map((p) => Object.assign(p, { source: 'shared' })));
    }
    const sep = r.style === 'decimal' ? '–' : ' – ';
    return a.map((p) => Object.assign(p, { source: 'startRange' }))
      .concat([{ type: 'literal', value: sep, source: 'shared' }], b.map((p) => Object.assign(p, { source: 'endRange' })));
  }

  define(NumberFormat.prototype, {
    get format() {
      const r = nfSlots(this, 'format');
      if (!r.bound) r.bound = (value) => joinParts(partitionNumber(r, toIntlValue(value)));
      return r.bound;
    },
    formatToParts(value) { return partitionNumber(nfSlots(this, 'formatToParts'), toIntlValue(value)); },
    formatRange(start, end) { return joinParts(rangeParts(nfSlots(this, 'formatRange'), start, end)); },
    formatRangeToParts(start, end) { return rangeParts(nfSlots(this, 'formatRangeToParts'), start, end); },
    resolvedOptions() {
      const r = nfSlots(this, 'resolvedOptions');
      const out = { locale: r.locale, numberingSystem: r.numberingSystem, style: r.style };
      for (const k of ['currency', 'currencyDisplay', 'currencySign', 'unit', 'unitDisplay', 'minimumIntegerDigits',
        'minimumFractionDigits', 'maximumFractionDigits', 'minimumSignificantDigits', 'maximumSignificantDigits']) {
        if (r[k] !== undefined) out[k] = r[k];
      }
      out.useGrouping = r.useGrouping;
      out.notation = r.notation;
      if (r.compactDisplay !== undefined) out.compactDisplay = r.compactDisplay;
      out.signDisplay = r.signDisplay;
      out.roundingIncrement = r.roundingIncrement;
      out.roundingMode = r.roundingMode;
      out.roundingPriority = r.roundingPriority;
      out.trailingZeroDisplay = r.trailingZeroDisplay;
      return out;
    },
  });
  tag(NumberFormat.prototype, 'Intl.NumberFormat');
  define(NumberFormat, { supportedLocalesOf: supportedLocalesOf(numberAvailable) });

  const nfCache = new Map();
  let defaultNumberFormat = null;
  // A plain-object options bag as a cache key, or null (not cacheable).
  function optionsKey(locales, options) {
    if (locales !== undefined && typeof locales !== 'string') return null;
    if (options === undefined) return (locales || '') + '|';
    if (!isObject(options) || Object.getPrototypeOf(options) !== Object.prototype) return null;
    const keys = Reflect.ownKeys(options);
    for (const k of keys) {
      // Only plain data: getters must run once per formatter, as specified.
      const d = Object.getOwnPropertyDescriptor(options, k);
      if (typeof k !== 'string' || !d.enumerable || !('value' in d)) return null;
      if (isObject(d.value) || typeof d.value === 'symbol' || typeof d.value === 'bigint') return null;
    }
    return (locales || '') + '|' + JSON.stringify(options);
  }
  function numberToLocaleString(x, locales, options) {
    if (locales === undefined && options === undefined) {
      if (!defaultNumberFormat) defaultNumberFormat = NF.get(new NumberFormat());
      return joinParts(partitionNumber(defaultNumberFormat, x));
    }
    const key = optionsKey(locales, options);
    const r = key === null ? NF.get(new NumberFormat(locales, options)) : memo(nfCache, key, () => NF.get(new NumberFormat(locales, options)));
    return joinParts(partitionNumber(r, x));
  }

  // ---------------------------------------------------------------------
  // PluralRules
  // ---------------------------------------------------------------------
  const PR = new WeakMap();
  const million = (o) => o.i !== 0 && o.i % 1000000 === 0 && o.v === 0;
  const PLURAL_RULES = {
    en: {
      cardinal: (o) => (o.i === 1 && o.v === 0 ? 'one' : 'other'),
      ordinal: (o) => {
        if (o.v !== 0) return 'other';
        const n10 = o.i % 10;
        const n100 = o.i % 100;
        if (n10 === 1 && n100 !== 11) return 'one';
        if (n10 === 2 && n100 !== 12) return 'two';
        if (n10 === 3 && n100 !== 13) return 'few';
        return 'other';
      },
      categories: [['one', 'other'], ['few', 'one', 'two', 'other']],
    },
    de: { cardinal: (o) => (o.i === 1 && o.v === 0 ? 'one' : 'other'), ordinal: () => 'other', categories: [['one', 'other'], ['other']] },
    es: {
      cardinal: (o) => (o.n === 1 ? 'one' : million(o) ? 'many' : 'other'),
      ordinal: () => 'other',
      categories: [['many', 'one', 'other'], ['other']],
    },
    fr: {
      cardinal: (o) => (o.i === 0 || o.i === 1 ? 'one' : million(o) ? 'many' : 'other'),
      ordinal: (o) => (o.n === 1 ? 'one' : 'other'),
      categories: [['many', 'one', 'other'], ['one', 'other']],
    },
    id: { cardinal: () => 'other', ordinal: () => 'other', categories: [['other'], ['other']] },
    it: {
      cardinal: (o) => (o.i === 1 && o.v === 0 ? 'one' : million(o) ? 'many' : 'other'),
      ordinal: (o) => ([11, 8, 80, 800].indexOf(o.n) !== -1 ? 'many' : 'other'),
      categories: [['many', 'one', 'other'], ['many', 'other']],
    },
    pt: {
      cardinal: (o) => (o.i === 0 || o.i === 1 ? 'one' : million(o) ? 'many' : 'other'),
      ordinal: () => 'other',
      categories: [['many', 'one', 'other'], ['other']],
    },
  };
  PLURAL_RULES.nl = PLURAL_RULES.de;
  const slavic = (o) => {
    const i10 = o.i % 10;
    const i100 = o.i % 100;
    if (o.v !== 0) return 'other';
    if (i10 === 1 && i100 !== 11) return 'one';
    if (i10 >= 2 && i10 <= 4 && (i100 < 12 || i100 > 14)) return 'few';
    return 'many';
  };
  const OTHER_ONLY = { cardinal: () => 'other', ordinal: () => 'other', categories: [['other'], ['other']] };
  Object.assign(PLURAL_RULES, {
    ru: { cardinal: slavic, ordinal: () => 'other', categories: [['few', 'many', 'one', 'other'], ['other']] },
    uk: {
      cardinal: slavic,
      ordinal: (o) => (o.v === 0 && o.i % 10 === 3 && o.i % 100 !== 13 ? 'few' : 'other'),
      categories: [['few', 'many', 'one', 'other'], ['few', 'other']],
    },
    pl: {
      cardinal: (o) => {
        if (o.i === 1 && o.v === 0) return 'one';
        const r = slavic(o);
        return r === 'one' ? 'many' : r;
      },
      ordinal: () => 'other',
      categories: [['few', 'many', 'one', 'other'], ['other']],
    },
    cs: {
      cardinal: (o) => (o.v !== 0 ? 'many' : o.i === 1 ? 'one' : o.i >= 2 && o.i <= 4 ? 'few' : 'other'),
      ordinal: () => 'other',
      categories: [['few', 'many', 'one', 'other'], ['other']],
    },
    ja: OTHER_ONLY, ko: OTHER_ONLY, zh: OTHER_ONLY, th: OTHER_ONLY,
    vi: { cardinal: () => 'other', ordinal: (o) => (o.n === 1 ? 'one' : 'other'), categories: [['other'], ['one', 'other']] },
    ms: { cardinal: () => 'other', ordinal: (o) => (o.n === 1 ? 'one' : 'other'), categories: [['other'], ['one', 'other']] },
    tr: { cardinal: (o) => (o.n === 1 ? 'one' : 'other'), ordinal: () => 'other', categories: [['one', 'other'], ['other']] },
    nb: { cardinal: (o) => (o.n === 1 ? 'one' : 'other'), ordinal: () => 'other', categories: [['one', 'other'], ['other']] },
    sv: {
      cardinal: (o) => (o.i === 1 && o.v === 0 ? 'one' : 'other'),
      ordinal: (o) => ((o.n % 10 === 1 || o.n % 10 === 2) && o.n % 100 !== 11 && o.n % 100 !== 12 ? 'one' : 'other'),
      categories: [['one', 'other'], ['one', 'other']],
    },
  });
  const pluralAvailable = (t) => numberAvailable(t) || (hasOwn(PLURAL_RULES, langOf(t)) && t.split('-').length <= 2);

  function operands(int, frac) {
    return { n: Number(int + (frac ? '.' + frac : '')), i: Number(int), v: frac.length };
  }

  function PluralRules(locales, options) {
    if (!new.target) throw new TypeError("Constructor Intl.PluralRules requires 'new'");
    const requested = canonicalizeLocaleList(locales);
    const opts = coerceOptions(options);
    localeMatcher(opts);
    const r = { type: getOption(opts, 'type', 'string', ['cardinal', 'ordinal'], 'cardinal') };
    digitOptions(r, opts, 0, 3, 'standard');
    const found = lookupLocale(pluralAvailable, requested);
    r.locale = found.locale;
    r.rules = PLURAL_RULES[langOf(found.locale)] || PLURAL_RULES.en;
    PR.set(this, r);
  }
  const prSlots = (o, m) => slotsOf(PR, o, 'Intl.PluralRules.prototype.' + m);
  function pluralOf(r, n) {
    const x = Number(n);
    if (!Number.isFinite(x)) return 'other';
    const res = roundNumber(r, toDecimal(Math.abs(x)), false);
    const o = operands(res.int, res.frac);
    return r.type === 'ordinal' ? r.rules.ordinal(o) : r.rules.cardinal(o);
  }
  define(PluralRules.prototype, {
    select(n) { return pluralOf(prSlots(this, 'select'), n); },
    selectRange(start, end) {
      const r = prSlots(this, 'selectRange');
      if (start === undefined || end === undefined) throw new TypeError('start or end is undefined');
      if (Number.isNaN(Number(start)) || Number.isNaN(Number(end))) throw new RangeError('Invalid number range');
      return pluralOf(r, end);
    },
    resolvedOptions() {
      const r = prSlots(this, 'resolvedOptions');
      const out = { locale: r.locale, type: r.type, minimumIntegerDigits: r.minimumIntegerDigits };
      for (const k of ['minimumFractionDigits', 'maximumFractionDigits', 'minimumSignificantDigits', 'maximumSignificantDigits']) {
        if (r[k] !== undefined) out[k] = r[k];
      }
      out.pluralCategories = r.rules.categories[r.type === 'ordinal' ? 1 : 0].slice();
      out.roundingIncrement = r.roundingIncrement;
      out.roundingMode = r.roundingMode;
      out.roundingPriority = r.roundingPriority;
      out.trailingZeroDisplay = r.trailingZeroDisplay;
      return out;
    },
  });
  tag(PluralRules.prototype, 'Intl.PluralRules');
  define(PluralRules, { supportedLocalesOf: supportedLocalesOf(pluralAvailable) });

  // ---------------------------------------------------------------------
  // Collator: a three-level comparison in the spirit of the CLDR root
  // collation (base letters, then accents, then case and variants).
  // Punctuation and symbols are not ignorable and sort before digits and
  // letters, in CLDR order for the common ones.
  // ---------------------------------------------------------------------
  const COLL = new WeakMap();
  const ORDER_WS = '\t\n\u000b\u000c\r   ';
  const ORDER_PUNCT = '_-‐‑‒–—―,、;:!¡?¿.…。·\'"«»()[]{}§¶@*/\\&#%‰•′″';
  const ORDER_SYMBOL = '`´˜^¯¸ˆ°©®←→+±÷×<=>¬|¦~∞≤≥';
  const ORDER_CURRENCY = '¤¢$£¥₩€₹';
  // Characters sorting as a variant of another one.
  const VARIANT_OF = { '‘': "'", '’': "'", '‚': "'", '“': '"', '”': '"', '„': '"' };
  // Letters that do not decompose: [base letters, secondary mark].
  const LETTER_FOLD = {
    'ø': ['o', '̸'], 'ł': ['l', '̸'], 'đ': ['d', '̸'], 'ð': ['d', '̹'], 'ħ': ['h', '̸'],
    'æ': ['ae', '\u0001'], 'œ': ['oe', '\u0001'], 'ß': ['ss', '\u0001'], 'ŀ': ['l', '̀'],
  };
  // Letters with a primary weight of their own right after a base letter.
  const LETTER_AFTER = { 'ı': 'i', 'ŧ': 't', 'ŋ': 'n', 'ĸ': 'q', 'þ': 'z' };
  // Secondary (accent) order of common combining marks.
  const SECONDARY_ORDER = '̸̧̨̹́̀̆̂̌̊̈̋̃̇̀̄';
  const RE_MARK = /\p{M}/u;
  const RE_IGNORABLE = /[\p{Cc}\p{Cf}]/u;
  const RE_LETTER = /\p{L}/u;
  const RE_DIGIT = /\p{Nd}/u;
  const RE_WS = /\s/;
  const RE_PUNCT = /\p{P}/u;
  const RE_CURRENCY = /\p{Sc}/u;

  // Primary weights: one band per class (whitespace, punctuation, symbols,
  // currency, digits, letters), each wider than the code point range.
  const weight = (n) => String.fromCharCode(n >> 16, n & 0xffff);
  const BAND = 0x1000000;
  function primaryOf(ch, ignorePunctuation) {
    let i;
    if (RE_WS.test(ch)) return ignorePunctuation ? null : BAND + ((i = ORDER_WS.indexOf(ch)) >= 0 ? i : 0x100);
    if (RE_PUNCT.test(ch)) return ignorePunctuation ? null : 2 * BAND + ((i = ORDER_PUNCT.indexOf(ch)) >= 0 ? i : 0x100 + ch.codePointAt(0));
    if (RE_CURRENCY.test(ch)) return 4 * BAND + ((i = ORDER_CURRENCY.indexOf(ch)) >= 0 ? i : 0x100 + ch.codePointAt(0));
    if (RE_DIGIT.test(ch)) return 5 * BAND + (ch < ':' ? ch.charCodeAt(0) - 48 : 0x100 + ch.codePointAt(0));
    if (RE_LETTER.test(ch)) return 6 * BAND + ch.toLowerCase().codePointAt(0) * 4;
    return 3 * BAND + ((i = ORDER_SYMBOL.indexOf(ch)) >= 0 ? i : 0x100 + ch.codePointAt(0));
  }

  // Sort key: `p` (primary), `s` (accents, per element), `t` (case and
  // variants, per element).
  function collationKey(str, numeric, ignorePunctuation) {
    const src = str.normalize('NFD');
    let p = '';
    const secs = [];
    let t = '';
    const push = (w, upper, variant) => {
      p += weight(w);
      secs.push('');
      t += String.fromCharCode(48 + (upper ? 2 : 0) + (variant ? 1 : 0));
    };
    for (let i = 0; i < src.length;) {
      const cp = src.codePointAt(i);
      const ch = String.fromCodePoint(cp);
      i += ch.length;
      if (RE_MARK.test(ch)) {
        if (secs.length) secs[secs.length - 1] += ch;
        continue;
      }
      if (RE_IGNORABLE.test(ch)) continue;
      if (numeric && cp >= 48 && cp <= 57) {
        let j = i;
        while (j < src.length && src.charCodeAt(j) >= 48 && src.charCodeAt(j) <= 57) j++;
        const run = src.slice(i - 1, j);
        const digits = run.replace(/^0+(?=\d)/, '');
        // Longer numbers first compare greater, then digit by digit.
        p += weight(5 * BAND + 0x80 + digits.length);
        for (const c of digits) p += weight(5 * BAND + c.charCodeAt(0) - 48);
        secs.push('');
        t += String.fromCharCode(48 + Math.min(run.length - digits.length, 9));
        i = j;
        continue;
      }
      const lower = ch.toLowerCase();
      const upper = lower !== ch;
      if (hasOwn(LETTER_FOLD, lower)) {
        const [base, mark] = LETTER_FOLD[lower];
        for (const b of base) push(primaryOf(b, false), upper, false);
        secs[secs.length - 1] += mark;
        continue;
      }
      if (hasOwn(LETTER_AFTER, lower)) {
        push(primaryOf(LETTER_AFTER[lower], false) + 1, upper, false);
        continue;
      }
      if (hasOwn(VARIANT_OF, ch)) {
        push(primaryOf(VARIANT_OF[ch], false), false, true);
        continue;
      }
      const compat = ch.normalize('NFKD');
      if (compat !== ch) {
        // Compatibility characters (ligatures, full-width forms, ...)
        // sort as what they stand for, differing at the third level.
        for (const c of compat) {
          if (RE_MARK.test(c)) {
            if (secs.length) secs[secs.length - 1] += c;
            continue;
          }
          const w = primaryOf(c, ignorePunctuation);
          if (w !== null) push(w, c.toLowerCase() !== c, true);
        }
        continue;
      }
      const w = primaryOf(ch, ignorePunctuation);
      if (w !== null) push(w, upper, false);
    }
    const s = secs.map((x) => {
      let out = '';
      for (const m of x) {
        const k = SECONDARY_ORDER.indexOf(m);
        out += String.fromCharCode(k >= 0 ? 0x10 + k : 0x100 + m.charCodeAt(0));
      }
      return out + '\u0001';
    }).join('');
    return { p, s, t };
  }

  const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

  function compareStrings(r, a, b) {
    if (a === b) return 0;
    const ka = collationKeyCached(r, a);
    const kb = collationKeyCached(r, b);
    let c = cmp(ka.p, kb.p);
    if (c || r.sensitivity === 'base') return c;
    if (r.sensitivity !== 'case') {
      c = cmp(ka.s, kb.s);
      if (c || r.sensitivity === 'accent') return c;
    }
    let ta = ka.t;
    let tb = kb.t;
    if (r.caseFirst === 'upper') {
      const flip = (s) => s.replace(/[0-9]/g, (d) => String.fromCharCode(48 + (d >= '2' ? Number(d) - 2 : Number(d) + 2)));
      ta = flip(ta);
      tb = flip(tb);
    }
    if (r.sensitivity === 'case') {
      // Case only: ignore the variant bit.
      ta = ta.replace(/[13]/g, (d) => String(Number(d) - 1));
      tb = tb.replace(/[13]/g, (d) => String(Number(d) - 1));
    }
    return cmp(ta, tb);
  }

  function collationKeyCached(r, s) {
    let k = r.keys.get(s);
    if (k === undefined) {
      if (r.keys.size >= 2000) r.keys.clear();
      k = collationKey(s, r.numeric, r.ignorePunctuation);
      r.keys.set(s, k);
    }
    return k;
  }

  function Collator(locales, options) {
    if (!new.target) return new Collator(locales, options);
    const requested = canonicalizeLocaleList(locales);
    const opts = coerceOptions(options);
    const usage = getOption(opts, 'usage', 'string', ['sort', 'search'], 'sort');
    localeMatcher(opts);
    const collation = getOption(opts, 'collation', 'string', undefined, undefined);
    if (collation !== undefined && !TYPE_RE.test(collation)) throw new RangeError('Invalid collation : ' + collation);
    let numeric = getOption(opts, 'numeric', 'boolean', undefined, undefined);
    let caseFirst = getOption(opts, 'caseFirst', 'string', ['upper', 'lower', 'false'], undefined);
    const found = lookupLocale(collatorAvailable, requested);
    const kw = found.keywords;
    const applied = {};
    if (numeric === undefined && (kw.kn === 'true' || kw.kn === 'false')) {
      numeric = kw.kn === 'true';
      applied.kn = kw.kn;
    }
    if (caseFirst === undefined && ['upper', 'lower', 'false'].indexOf(kw.kf) !== -1) {
      caseFirst = kw.kf;
      applied.kf = kw.kf;
    }
    const sensitivity = getOption(opts, 'sensitivity', 'string', ['base', 'accent', 'case', 'variant'], 'variant');
    const ignorePunctuation = getOption(opts, 'ignorePunctuation', 'boolean', undefined, false);
    COLL.set(this, {
      locale: withKeywords(found.locale, applied), usage, sensitivity, ignorePunctuation,
      numeric: !!numeric, caseFirst: caseFirst || 'false', keys: new Map(),
    });
  }
  const collSlots = (o, m) => slotsOf(COLL, o, 'Intl.Collator.prototype.' + m);
  define(Collator.prototype, {
    get compare() {
      const r = collSlots(this, 'compare');
      if (!r.bound) r.bound = (a, b) => compareStrings(r, String(a), String(b));
      return r.bound;
    },
    resolvedOptions() {
      const r = collSlots(this, 'resolvedOptions');
      return {
        locale: r.locale, usage: r.usage, sensitivity: r.sensitivity, ignorePunctuation: r.ignorePunctuation,
        collation: 'default', numeric: r.numeric, caseFirst: r.caseFirst,
      };
    },
  });
  tag(Collator.prototype, 'Intl.Collator');
  define(Collator, { supportedLocalesOf: supportedLocalesOf(collatorAvailable) });

  const collatorCache = new Map();
  let defaultCollator = null;
  function localeCompare(a, b, locales, options) {
    let r;
    if (locales === undefined && options === undefined) {
      r = defaultCollator || (defaultCollator = COLL.get(new Collator()));
    } else {
      const key = optionsKey(locales, options);
      r = key === null ? COLL.get(new Collator(locales, options)) : memo(collatorCache, key, () => COLL.get(new Collator(locales, options)));
    }
    return compareStrings(r, a, b);
  }

  // ---------------------------------------------------------------------
  // Time zones
  // ---------------------------------------------------------------------
  const UTC_NAMES = ['UTC', 'ETC/UTC', 'GMT', 'ETC/GMT', 'UCT', 'ETC/UCT', 'UNIVERSAL', 'ETC/UNIVERSAL', 'ZULU', 'ETC/ZULU',
    'GREENWICH', 'ETC/GREENWICH', 'GMT0', 'ETC/GMT0', 'GMT+0', 'GMT-0', 'ETC/GMT+0', 'ETC/GMT-0'];
  // Metazones: [standard, daylight, generic] long names and short names.
  const METAZONES = {
    America_Eastern: ['Eastern Standard Time', 'Eastern Daylight Time', 'Eastern Time', 'EST', 'EDT', 'ET'],
    America_Central: ['Central Standard Time', 'Central Daylight Time', 'Central Time', 'CST', 'CDT', 'CT'],
    America_Mountain: ['Mountain Standard Time', 'Mountain Daylight Time', 'Mountain Time', 'MST', 'MDT', 'MT'],
    America_Pacific: ['Pacific Standard Time', 'Pacific Daylight Time', 'Pacific Time', 'PST', 'PDT', 'PT'],
    Alaska: ['Alaska Standard Time', 'Alaska Daylight Time', 'Alaska Time', 'AKST', 'AKDT', 'AKT'],
    Hawaii_Aleutian: ['Hawaii-Aleutian Standard Time', 'Hawaii-Aleutian Daylight Time', 'Hawaii-Aleutian Time', 'HST', 'HDT', 'HST'],
    Atlantic: ['Atlantic Standard Time', 'Atlantic Daylight Time', 'Atlantic Time', 'AST', 'ADT', 'AT'],
    GMT: ['Greenwich Mean Time', 'British Summer Time', 'Greenwich Mean Time', 'GMT'],
    Europe_Central: ['Central European Standard Time', 'Central European Summer Time', 'Central European Time'],
    Europe_Eastern: ['Eastern European Standard Time', 'Eastern European Summer Time', 'Eastern European Time'],
    Europe_Western: ['Western European Standard Time', 'Western European Summer Time', 'Western European Time'],
    Moscow: ['Moscow Standard Time', 'Moscow Summer Time', 'Moscow Time'],
    India: ['India Standard Time'],
    China: ['China Standard Time', 'China Daylight Time', 'China Time'],
    Hong_Kong: ['Hong Kong Standard Time', 'Hong Kong Summer Time', 'Hong Kong Time'],
    Taipei: ['Taipei Standard Time', 'Taipei Daylight Time', 'Taipei Time'],
    Japan: ['Japan Standard Time', 'Japan Daylight Time', 'Japan Time'],
    Korea: ['Korean Standard Time', 'Korean Daylight Time', 'Korean Time'],
    Indonesia_Western: ['Western Indonesia Time'],
    Indonesia_Central: ['Central Indonesia Time'],
    Indonesia_Eastern: ['Eastern Indonesia Time'],
    Singapore: ['Singapore Standard Time'],
    Malaysia: ['Malaysia Time'],
    Indochina: ['Indochina Time'],
    Philippines: ['Philippine Standard Time', 'Philippine Summer Time', 'Philippine Time'],
    Gulf: ['Gulf Standard Time'],
    Arabian: ['Arabian Standard Time', 'Arabian Daylight Time', 'Arabian Time'],
    Israel: ['Israel Standard Time', 'Israel Daylight Time', 'Israel Time'],
    Australia_Eastern: ['Australian Eastern Standard Time', 'Australian Eastern Daylight Time', 'Australian Eastern Time'],
    Australia_Central: ['Australian Central Standard Time', 'Australian Central Daylight Time', 'Australian Central Time'],
    Australia_Western: ['Australian Western Standard Time', 'Australian Western Daylight Time', 'Australian Western Time'],
    New_Zealand: ['New Zealand Standard Time', 'New Zealand Daylight Time', 'New Zealand Time'],
    Brasilia: ['Brasilia Standard Time', 'Brasilia Summer Time', 'Brasilia Time'],
    Argentina: ['Argentina Standard Time', 'Argentina Summer Time', 'Argentina Time'],
    Cuba: ['Cuba Standard Time', 'Cuba Daylight Time', 'Cuba Time'],
  };
  const ZONE_METAZONE_SPEC = 'America_Eastern:America/New_York,America/Detroit,America/Toronto,America/Indiana/Indianapolis,' +
    'America/Indianapolis,America/Kentucky/Louisville,America/Louisville,America/Montreal,America/Nassau,US/Eastern,EST5EDT,EST;' +
    'America_Central:America/Chicago,America/Winnipeg,America/Mexico_City,America/Indiana/Knox,America/Menominee,US/Central,CST6CDT;' +
    'America_Mountain:America/Denver,America/Phoenix,America/Edmonton,America/Boise,US/Mountain,US/Arizona,MST7MDT,MST;' +
    'America_Pacific:America/Los_Angeles,America/Vancouver,America/Tijuana,US/Pacific,PST8PDT;' +
    'Alaska:America/Anchorage,America/Juneau,America/Sitka,America/Nome,US/Alaska;' +
    'Hawaii_Aleutian:Pacific/Honolulu,America/Adak,US/Hawaii,HST;Atlantic:America/Halifax,America/Puerto_Rico,Atlantic/Bermuda;' +
    'GMT:Europe/London,Africa/Abidjan,Africa/Accra,Atlantic/Reykjavik,Africa/Dakar;' +
    'Europe_Central:Europe/Berlin,Europe/Paris,Europe/Madrid,Europe/Rome,Europe/Amsterdam,Europe/Brussels,Europe/Vienna,' +
    'Europe/Zurich,Europe/Stockholm,Europe/Oslo,Europe/Copenhagen,Europe/Warsaw,Europe/Prague,Europe/Budapest,' +
    'Europe/Belgrade,Europe/Luxembourg,Europe/Monaco,Africa/Algiers,Africa/Tunis,CET;' +
    'Europe_Eastern:Europe/Athens,Europe/Helsinki,Europe/Kiev,Europe/Kyiv,Europe/Bucharest,Europe/Sofia,Europe/Riga,' +
    'Europe/Vilnius,Europe/Tallinn,Africa/Cairo,Asia/Beirut,EET;Europe_Western:Europe/Lisbon,Atlantic/Canary,Atlantic/Madeira,WET;' +
    'Moscow:Europe/Moscow;India:Asia/Kolkata,Asia/Calcutta;China:Asia/Shanghai,Asia/Chongqing,Asia/Harbin,PRC;' +
    'Hong_Kong:Asia/Hong_Kong;Taipei:Asia/Taipei;Japan:Asia/Tokyo,Japan;Korea:Asia/Seoul;' +
    'Indonesia_Western:Asia/Jakarta,Asia/Pontianak;Indonesia_Central:Asia/Makassar;Indonesia_Eastern:Asia/Jayapura;' +
    'Singapore:Asia/Singapore;Malaysia:Asia/Kuala_Lumpur,Asia/Kuching;' +
    'Indochina:Asia/Bangkok,Asia/Ho_Chi_Minh,Asia/Saigon,Asia/Phnom_Penh,Asia/Vientiane;Philippines:Asia/Manila;' +
    'Gulf:Asia/Dubai,Asia/Muscat;Arabian:Asia/Riyadh,Asia/Baghdad,Asia/Qatar,Asia/Kuwait,Asia/Bahrain,Asia/Aden;' +
    'Israel:Asia/Jerusalem,Asia/Tel_Aviv;' +
    'Australia_Eastern:Australia/Sydney,Australia/Melbourne,Australia/Brisbane,Australia/Hobart,Australia/Canberra;' +
    'Australia_Central:Australia/Adelaide,Australia/Darwin;Australia_Western:Australia/Perth;New_Zealand:Pacific/Auckland,NZ;' +
    'Brasilia:America/Sao_Paulo;Argentina:America/Argentina/Buenos_Aires,America/Buenos_Aires;Cuba:America/Havana';
  let zoneMetazone = null;
  function metazoneOf(id) {
    if (!zoneMetazone) {
      zoneMetazone = Object.create(null);
      for (const item of ZONE_METAZONE_SPEC.split(';')) {
        const [mz, zones] = item.split(':');
        for (const z of zones.split(',')) zoneMetazone[z] = mz;
      }
    }
    return zoneMetazone[id];
  }

  let localZoneCache = null;
  function localZone() {
    if (!localZoneCache) {
      let id = '';
      try { id = String(env.localTimeZone() || ''); } catch (e) { id = ''; }
      if (!id) {
        const off = -new Date().getTimezoneOffset();
        id = off === 0 ? 'UTC' : (off < 0 ? '-' : '+') + pad2(Math.floor(Math.abs(off) / 60)) + ':' + pad2(Math.abs(off) % 60);
      }
      localZoneCache = { kind: 'local', id: UTC_NAMES.indexOf(id.toUpperCase()) !== -1 ? 'UTC' : id };
    }
    return localZoneCache;
  }

  function resolveTimeZone(tz) {
    if (tz === undefined) return localZone();
    const name = String(tz);
    if (UTC_NAMES.indexOf(name.toUpperCase()) !== -1) return { kind: 'utc', id: 'UTC' };
    const m = /^([+-])(\d{2})(?::?(\d{2}))?$/.exec(name);
    if (m) {
      const h = Number(m[2]);
      const mi = Number(m[3] || 0);
      if (h > 23 || mi > 59) throw new RangeError('Invalid time zone specified: ' + name);
      const sign = m[1] === '-' && h + mi > 0 ? '-' : '+';
      return { kind: 'offset', id: sign + pad2(h) + ':' + pad2(mi), offset: (sign === '-' ? -1 : 1) * (h * 60 + mi) * 60 };
    }
    const local = localZone();
    if (local.id.toLowerCase() === name.toLowerCase()) return local;
    let info;
    try { info = env.zoneOffset(name, Date.now()); } catch (e) { info = undefined; }
    if (info) return { kind: 'named', id: info[2] };
    const etc = /^etc\/gmt([+-])(\d{1,2})$/i.exec(name);
    if (etc && Number(etc[2]) <= 14) {
      return { kind: 'offset', id: 'Etc/GMT' + etc[1] + Number(etc[2]), offset: (etc[1] === '+' ? -1 : 1) * Number(etc[2]) * 3600 };
    }
    throw new RangeError('Invalid time zone specified: ' + name);
  }

  // `{ offset (seconds), abbr, dst }` of a zone at epoch milliseconds `t`.
  function zoneInfo(zone, t) {
    if (zone.kind === 'utc') return { offset: 0, abbr: 'UTC', dst: false };
    if (zone.kind === 'offset') return { offset: zone.offset, abbr: '', dst: false };
    let info;
    try { info = env.zoneOffset(zone.id, t); } catch (e) { info = undefined; }
    if (zone.kind === 'local') {
      const offset = -new Date(t).getTimezoneOffset() * 60;
      if (info && info[0] === offset) return { offset, abbr: info[1], dst: !!info[3] };
      const y = new Date(t).getFullYear();
      const jan = -new Date(y, 0, 1).getTimezoneOffset() * 60;
      const jul = -new Date(y, 6, 1).getTimezoneOffset() * 60;
      return { offset, abbr: '', dst: jan !== jul && offset === Math.max(jan, jul) };
    }
    return info ? { offset: info[0], abbr: info[1], dst: !!info[3] } : { offset: 0, abbr: '', dst: false };
  }

  function observesDst(zone, t) {
    if (zone.kind === 'utc' || zone.kind === 'offset') return false;
    const y = new Date(t).getUTCFullYear();
    return zoneInfo(zone, Date.UTC(y, 0, 15)).offset !== zoneInfo(zone, Date.UTC(y, 6, 15)).offset;
  }

  function gmtOffset(offset, long) {
    const sign = offset < 0 ? '-' : '+';
    const a = Math.abs(offset);
    const h = Math.floor(a / 3600);
    const m = Math.floor((a % 3600) / 60);
    if (long) return 'GMT' + sign + pad2(h) + ':' + pad2(m);
    return 'GMT' + sign + h + (m ? ':' + pad2(m) : '');
  }

  function timeZoneName(style, zone, t, info) {
    if (zone.kind === 'utc') {
      if (style === 'short') return 'UTC';
      if (style === 'long') return 'Coordinated Universal Time';
      return gmtOffset(0, style === 'longOffset' || style === 'longGeneric');
    }
    if (style === 'shortOffset' || style === 'longOffset') return gmtOffset(info.offset, style === 'longOffset');
    const mz = zone.kind === 'offset' ? undefined : METAZONES[metazoneOf(zone.id)];
    const long = style === 'long' || style === 'longGeneric';
    const generic = style === 'shortGeneric' || style === 'longGeneric';
    if (mz) {
      const dst = info.dst;
      let idx;
      if (generic && observesDst(zone, t)) idx = 2;
      else idx = dst && mz.length > 1 ? 1 : 0;
      if (zone.id === 'Europe/Dublin' && idx === 1) return long ? 'Irish Standard Time' : gmtOffset(info.offset, false);
      if (long) return mz[idx] || mz[0];
      if (mz.length > 3) {
        const short = mz[3 + idx] || (idx === 0 ? mz[3] : undefined);
        if (short && (mz !== METAZONES.GMT || idx === 0)) return short;
      }
    }
    if (generic && zone.kind !== 'offset' && /\//.test(zone.id) && !/^Etc\//.test(zone.id)) {
      return zone.id.slice(zone.id.lastIndexOf('/') + 1).replace(/_/g, ' ') + ' Time';
    }
    return gmtOffset(info.offset, long);
  }

  // ---------------------------------------------------------------------
  // DateTimeFormat (en-US patterns)
  // ---------------------------------------------------------------------
  const DTF = new WeakMap();
  const MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
  const DAYS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
  const DATE_FIELDS = ['weekday', 'era', 'year', 'month', 'day'];
  const TIME_FIELDS = ['dayPeriod', 'hour', 'minute', 'second', 'fractionalSecondDigits'];
  const DATE_STYLES = {
    full: { weekday: 'long', month: 'long', day: 'numeric', year: 'numeric' },
    long: { month: 'long', day: 'numeric', year: 'numeric' },
    medium: { month: 'short', day: 'numeric', year: 'numeric' },
    short: { month: 'numeric', day: 'numeric', year: '2-digit' },
  };
  const TIME_STYLES = {
    full: { hour: 'numeric', minute: '2-digit', second: '2-digit', timeZoneName: 'long' },
    long: { hour: 'numeric', minute: '2-digit', second: '2-digit', timeZoneName: 'short' },
    medium: { hour: 'numeric', minute: '2-digit', second: '2-digit' },
    short: { hour: 'numeric', minute: '2-digit' },
  };

  // Field/literal tokens for the date part: `[{ f, style } | { lit }]`.
  function datePattern(c) {
    const p = [];
    const f = (name) => p.push({ f: name, style: c[name] });
    const lit = (s) => p.push({ lit: s });
    const textMonth = c.month && c.month !== 'numeric' && c.month !== '2-digit';
    if (c.month) {
      if (c.weekday) {
        f('weekday');
        lit(', ');
      }
      f('month');
      if (textMonth) {
        if (c.day) {
          lit(' ');
          f('day');
        }
        if (c.year) {
          lit(c.day ? ', ' : ' ');
          f('year');
        }
      } else {
        if (c.day) {
          lit('/');
          f('day');
        }
        if (c.year) {
          lit('/');
          f('year');
        }
      }
    } else if (c.year) {
      f('year');
      if (c.day) {
        lit(' (day: ');
        f('day');
        lit(')');
      }
      if (c.weekday) {
        lit(' ');
        f('weekday');
      }
    } else if (c.day) {
      f('day');
      if (c.weekday) {
        lit(' ');
        f('weekday');
      }
    } else if (c.weekday) f('weekday');
    if (c.era) {
      if (p.length) lit(' ');
      f('era');
    }
    return p;
  }

  function timePattern(c, hc) {
    const p = [];
    const f = (name, style) => p.push({ f: name, style: style || c[name] });
    const lit = (s) => p.push({ lit: s });
    const h12 = hc === 'h11' || hc === 'h12';
    if (c.hour) {
      f('hour');
      if (c.minute) {
        lit(':');
        f('minute');
        if (c.second) {
          lit(':');
          f('second');
          if (c.fractionalSecondDigits) {
            lit('.');
            f('fractionalSecond', c.fractionalSecondDigits);
          }
        }
      }
      if (h12) {
        // ICU puts U+202F before AM/PM; V8 turns it into a space in
        // format() but not in formatToParts() or formatRange().
        lit(c.dayPeriod ? ' ' : NNBSP);
        f('dayPeriod', c.dayPeriod || 'ampm');
      }
      if (c.second && !c.minute) {
        lit(' (second: ');
        f('second');
        lit(')');
      }
    } else if (c.minute) {
      f('minute');
      if (c.second) {
        lit(':');
        f('second');
      }
    } else if (c.second) {
      f('second');
    }
    if (c.fractionalSecondDigits && !(c.hour && c.minute && c.second)) {
      if (c.second) lit('.');
      f('fractionalSecond', c.fractionalSecondDigits);
    }
    if (c.dayPeriod && !c.hour) f('dayPeriod');
    return p;
  }

  function buildPattern(r) {
    const c = r.components;
    const date = datePattern(c);
    const time = timePattern(c, r.hourCycle);
    let joiner = ', ';
    if (r.dateStyle) joiner = r.dateStyle === 'full' || r.dateStyle === 'long' ? ' at ' : ', ';
    else if (c.month === 'long') joiner = ' at ';
    else if (c.weekday && !c.year && !c.month && !c.day) joiner = ' ';
    const out = date.slice();
    r.dateEnd = date.length;
    if (date.length && time.length) out.push({ lit: joiner });
    out.push(...time);
    if (c.timeZoneName) {
      if (out.length) out.push({ lit: time.length ? ' ' : ', ' });
      out.push({ f: 'timeZoneName', style: c.timeZoneName });
    }
    return out;
  }

  function resolveDateTimeFormat(locales, options, required, defaults) {
    const requested = canonicalizeLocaleList(locales);
    const opts = coerceOptions(options);
    localeMatcher(opts);
    const calendar = getOption(opts, 'calendar', 'string', undefined, undefined);
    if (calendar !== undefined && !TYPE_RE.test(calendar)) throw new RangeError('Invalid calendar : ' + calendar);
    const nu = getOption(opts, 'numberingSystem', 'string', undefined, undefined);
    if (nu !== undefined && !TYPE_RE.test(nu)) throw new RangeError('Invalid numberingSystem : ' + nu);
    const hour12 = getOption(opts, 'hour12', 'boolean', undefined, undefined);
    let hourCycle = getOption(opts, 'hourCycle', 'string', ['h11', 'h12', 'h23', 'h24'], undefined);
    if (hour12 !== undefined) hourCycle = null;
    const found = lookupLocale(isEnglish, requested);
    const kw = found.keywords;
    const applied = {};
    if (calendar === undefined && (kw.ca === 'gregory' || kw.ca === 'iso8601')) applied.ca = kw.ca;
    if (nu === undefined && kw.nu === 'latn') applied.nu = 'latn';
    const r = { calendar: 'gregory', numberingSystem: 'latn' };
    r.zone = resolveTimeZone(opts.timeZone);
    const c = {};
    c.weekday = getOption(opts, 'weekday', 'string', ['narrow', 'short', 'long'], undefined);
    c.era = getOption(opts, 'era', 'string', ['narrow', 'short', 'long'], undefined);
    c.year = getOption(opts, 'year', 'string', ['2-digit', 'numeric'], undefined);
    c.month = getOption(opts, 'month', 'string', ['2-digit', 'numeric', 'narrow', 'short', 'long'], undefined);
    c.day = getOption(opts, 'day', 'string', ['2-digit', 'numeric'], undefined);
    c.dayPeriod = getOption(opts, 'dayPeriod', 'string', ['narrow', 'short', 'long'], undefined);
    c.hour = getOption(opts, 'hour', 'string', ['2-digit', 'numeric'], undefined);
    c.minute = getOption(opts, 'minute', 'string', ['2-digit', 'numeric'], undefined);
    c.second = getOption(opts, 'second', 'string', ['2-digit', 'numeric'], undefined);
    c.fractionalSecondDigits = getNumberOption(opts, 'fractionalSecondDigits', 1, 3, undefined);
    c.timeZoneName = getOption(opts, 'timeZoneName', 'string',
      ['short', 'long', 'shortOffset', 'longOffset', 'shortGeneric', 'longGeneric'], undefined);
    getOption(opts, 'formatMatcher', 'string', ['basic', 'best fit'], 'best fit');
    const dateStyle = getOption(opts, 'dateStyle', 'string', ['full', 'long', 'medium', 'short'], undefined);
    const timeStyle = getOption(opts, 'timeStyle', 'string', ['full', 'long', 'medium', 'short'], undefined);
    if (dateStyle !== undefined || timeStyle !== undefined) {
      for (const k of Object.keys(c)) {
        if (c[k] !== undefined) throw new TypeError("Can't set option " + k + ' when ' + (dateStyle !== undefined ? 'dateStyle' : 'timeStyle') + ' is used');
      }
      if (required === 'date' && timeStyle !== undefined) throw new TypeError('Invalid option : timeStyle');
      if (required === 'time' && dateStyle !== undefined) throw new TypeError('Invalid option : dateStyle');
      if (dateStyle) Object.assign(c, DATE_STYLES[dateStyle]);
      if (timeStyle) Object.assign(c, TIME_STYLES[timeStyle]);
      r.dateStyle = dateStyle;
      r.timeStyle = timeStyle;
    } else {
      let needDefaults = true;
      if (required === 'date' || required === 'any') {
        for (const k of ['weekday', 'year', 'month', 'day']) if (c[k] !== undefined) needDefaults = false;
      }
      if (required === 'time' || required === 'any') {
        for (const k of TIME_FIELDS) if (c[k] !== undefined) needDefaults = false;
      }
      if (needDefaults && (defaults === 'date' || defaults === 'all')) c.year = c.month = c.day = 'numeric';
      if (needDefaults && (defaults === 'time' || defaults === 'all')) c.hour = c.minute = c.second = 'numeric';
    }
    if (c.hour) {
      let hc = hourCycle || (['h11', 'h12', 'h23', 'h24'].indexOf(kw.hc) !== -1 ? kw.hc : 'h12');
      if (hourCycle === undefined && hc === kw.hc) applied.hc = kw.hc;
      if (hour12 !== undefined) hc = hour12 ? 'h12' : 'h23';
      r.hourCycle = hc;
      if (hc === 'h23' || hc === 'h24') c.hour = '2-digit';
      if (c.minute) c.minute = '2-digit';
      if (c.minute && c.second) c.second = '2-digit';
    } else if (c.minute && c.second) {
      c.minute = '2-digit';
      c.second = '2-digit';
    }
    r.components = c;
    r.locale = withKeywords(found.locale, applied);
    r.pattern = buildPattern(r);
    return r;
  }

  function toTime(date) {
    const t = date === undefined ? Date.now() : Number(date instanceof Date ? date.getTime() : date);
    if (!Number.isFinite(t) || Math.abs(t) > 8.64e15) throw new RangeError('Invalid time value');
    return Math.trunc(t) + 0;
  }

  // Calendar fields of `t` in the formatter's zone. The local zone uses the
  // Date getters (so it always agrees with them); its offset details are
  // only looked up when a time zone name is shown.
  function fieldsAt(r, t) {
    const zone = r.zone;
    let d;
    if (zone.kind === 'local') {
      d = new Date(t);
      return {
        year: d.getFullYear(), month: d.getMonth(), day: d.getDate(), weekday: d.getDay(), hour: d.getHours(),
        minute: d.getMinutes(), second: d.getSeconds(), ms: d.getMilliseconds(), info: null, t,
      };
    }
    const info = zoneInfo(zone, t);
    d = new Date(t + info.offset * 1000);
    return {
      year: d.getUTCFullYear(), month: d.getUTCMonth(), day: d.getUTCDate(), weekday: d.getUTCDay(), hour: d.getUTCHours(),
      minute: d.getUTCMinutes(), second: d.getUTCSeconds(), ms: d.getUTCMilliseconds(), info, t,
    };
  }

  function dayPeriodName(f, c) {
    const h = f.hour;
    if (h === 12 && (!c.minute || f.minute === 0) && (!c.second || f.second === 0) && (c.hour || f.minute === 0)) return 'noon';
    if (h < 12) return 'in the morning';
    if (h < 18) return 'in the afternoon';
    if (h < 21) return 'in the evening';
    return 'at night';
  }

  function formatField(r, name, style, f) {
    switch (name) {
      case 'weekday': return style === 'long' ? DAYS[f.weekday] : style === 'short' ? DAYS[f.weekday].slice(0, 3) : DAYS[f.weekday][0];
      case 'era': {
        const bc = f.year <= 0;
        if (style === 'long') return bc ? 'Before Christ' : 'Anno Domini';
        if (style === 'narrow') return bc ? 'B' : 'A';
        return bc ? 'BC' : 'AD';
      }
      case 'year': {
        const y = f.year <= 0 ? 1 - f.year : f.year;
        return style === '2-digit' ? pad2(y % 100) : String(y);
      }
      case 'month':
        if (style === 'numeric') return String(f.month + 1);
        if (style === '2-digit') return pad2(f.month + 1);
        if (style === 'long') return MONTHS[f.month];
        return style === 'short' ? MONTHS[f.month].slice(0, 3) : MONTHS[f.month][0];
      case 'day': return style === '2-digit' ? pad2(f.day) : String(f.day);
      case 'hour': {
        let h = f.hour;
        switch (r.hourCycle) {
          case 'h11': h %= 12; break;
          case 'h12': h = h % 12 || 12; break;
          case 'h24': h = h || 24; break;
        }
        return style === '2-digit' ? pad2(h) : String(h);
      }
      case 'minute': return style === '2-digit' ? pad2(f.minute) : String(f.minute);
      case 'second': return style === '2-digit' ? pad2(f.second) : String(f.second);
      case 'fractionalSecond': return String(f.ms).padStart(3, '0').slice(0, style);
      case 'dayPeriod': return style === 'ampm' ? (f.hour < 12 ? 'AM' : 'PM') : dayPeriodName(f, r.components);
      case 'timeZoneName': return timeZoneName(style, r.zone, f.t, f.info || zoneInfo(r.zone, f.t));
    }
    return '';
  }

  function dateParts(r, t) {
    const f = fieldsAt(r, t);
    const out = [];
    for (const tok of r.pattern) {
      if (tok.lit !== undefined) out.push({ type: 'literal', value: tok.lit });
      else out.push({ type: tok.f, value: formatField(r, tok.f, tok.style, f) });
    }
    return out;
  }
  const formatDate = (r, t) => joinParts(dateParts(r, t)).replace(/\u202f/g, ' ');

  function dateRangeParts(r, a, b) {
    if (a === undefined || b === undefined) throw new TypeError('startDate or endDate is undefined');
    const x = toTime(a);
    const y = toTime(b);
    const pa = dateParts(r, x);
    const pb = dateParts(r, y);
    if (joinParts(pa) === joinParts(pb)) {
      return pa.map((p) => Object.assign(p, { value: p.value === NNBSP ? ' ' : p.value, source: 'shared' }));
    }
    // Same calendar day with a time part: share the date (and the day
    // period when both times are in the same half of the day).
    const end = r.dateEnd ? r.dateEnd + 1 : 0;
    if (end < r.pattern.length && joinParts(pa.slice(0, end)) === joinParts(pb.slice(0, end))) {
      const shared = pa.slice(0, end).map((p) => Object.assign(p, { source: 'shared' }));
      let a = pa.slice(end);
      let tail = [];
      const last = a.length - 1;
      if (last > 1 && a[last].type === 'dayPeriod' && a[last].value === pb[pb.length - 1].value) {
        tail = a.slice(last - 1).map((p) => Object.assign(p, { source: 'shared' }));
        a = a.slice(0, last - 1);
        pb.splice(pb.length - 2, 2);
      }
      return shared.concat(a.map((p) => Object.assign(p, { source: 'startRange' })),
        [{ type: 'literal', value: THIN + '–' + THIN, source: 'shared' }],
        pb.slice(end).map((p) => Object.assign(p, { source: 'endRange' })), tail);
    }
    return pa.map((p) => Object.assign(p, { source: 'startRange' }))
      .concat([{ type: 'literal', value: THIN + '–' + THIN, source: 'shared' }], pb.map((p) => Object.assign(p, { source: 'endRange' })));
  }

  function DateTimeFormat(locales, options) {
    if (!new.target) return new DateTimeFormat(locales, options);
    DTF.set(this, resolveDateTimeFormat(locales, options, 'any', 'date'));
  }
  const dtfSlots = (o, m) => slotsOf(DTF, o, 'Intl.DateTimeFormat.prototype.' + m);
  define(DateTimeFormat.prototype, {
    get format() {
      const r = dtfSlots(this, 'format');
      if (!r.bound) r.bound = (date) => formatDate(r, toTime(date));
      return r.bound;
    },
    formatToParts(date) { return dateParts(dtfSlots(this, 'formatToParts'), toTime(date)); },
    formatRange(a, b) { return joinParts(dateRangeParts(dtfSlots(this, 'formatRange'), a, b)); },
    formatRangeToParts(a, b) { return dateRangeParts(dtfSlots(this, 'formatRangeToParts'), a, b); },
    resolvedOptions() {
      const r = dtfSlots(this, 'resolvedOptions');
      const out = { locale: r.locale, calendar: r.calendar, numberingSystem: r.numberingSystem, timeZone: r.zone.id };
      if (r.hourCycle && (!r.dateStyle || r.timeStyle)) {
        out.hourCycle = r.hourCycle;
        out.hour12 = r.hourCycle === 'h11' || r.hourCycle === 'h12';
      }
      if (r.dateStyle || r.timeStyle) {
        if (r.dateStyle) out.dateStyle = r.dateStyle;
        if (r.timeStyle) out.timeStyle = r.timeStyle;
        return out;
      }
      const c = r.components;
      for (const k of DATE_FIELDS.concat(TIME_FIELDS, ['timeZoneName'])) {
        if (c[k] !== undefined) out[k] = c[k];
      }
      return out;
    },
  });
  tag(DateTimeFormat.prototype, 'Intl.DateTimeFormat');
  define(DateTimeFormat, { supportedLocalesOf: supportedLocalesOf(isEnglish) });

  const dtfCache = new Map();
  function dateToLocaleString(t, kind, locales, options) {
    if (Number.isNaN(t)) return 'Invalid Date';
    const key = optionsKey(locales, options);
    const build = () => {
      const required = kind === 'any' ? 'any' : kind;
      const defaults = kind === 'any' ? 'all' : kind;
      return resolveDateTimeFormat(locales, options, required, defaults);
    };
    const r = key === null ? build() : memo(dtfCache, kind + '|' + key, build);
    return formatDate(r, t);
  }

  // ---------------------------------------------------------------------
  // RelativeTimeFormat
  // ---------------------------------------------------------------------
  const RTF = new WeakMap();
  const RT_UNITS = ['second', 'minute', 'hour', 'day', 'week', 'month', 'quarter', 'year'];
  // Per style: [singular, plural, separator before the unit].
  const RT_NAMES = {
    long: { year: ['year', 'years'], quarter: ['quarter', 'quarters'], month: ['month', 'months'], week: ['week', 'weeks'],
      day: ['day', 'days'], hour: ['hour', 'hours'], minute: ['minute', 'minutes'], second: ['second', 'seconds'] },
    short: { year: ['yr.', 'yr.'], quarter: ['qtr.', 'qtrs.'], month: ['mo.', 'mo.'], week: ['wk.', 'wk.'], day: ['day', 'days'],
      hour: ['hr.', 'hr.'], minute: ['min.', 'min.'], second: ['sec.', 'sec.'] },
    narrow: { year: ['y', 'y'], quarter: ['q', 'q'], month: ['mo', 'mo'], week: ['w', 'w'], day: ['d', 'd'],
      hour: ['h', 'h'], minute: ['m', 'm'], second: ['s', 's'] },
  };
  // numeric: 'auto' phrases for -1, 0, 1.
  const RT_AUTO_LONG = {
    year: ['last year', 'this year', 'next year'], quarter: ['last quarter', 'this quarter', 'next quarter'],
    month: ['last month', 'this month', 'next month'], week: ['last week', 'this week', 'next week'],
    day: ['yesterday', 'today', 'tomorrow'], hour: [null, 'this hour', null], minute: [null, 'this minute', null],
    second: [null, 'now', null],
  };
  const RT_AUTO_SHORT = Object.assign({}, RT_AUTO_LONG, {
    year: ['last yr.', 'this yr.', 'next yr.'], quarter: ['last qtr.', 'this qtr.', 'next qtr.'],
    month: ['last mo.', 'this mo.', 'next mo.'], week: ['last wk.', 'this wk.', 'next wk.'],
  });

  function RelativeTimeFormat(locales, options) {
    if (!new.target) throw new TypeError("Constructor Intl.RelativeTimeFormat requires 'new'");
    const requested = canonicalizeLocaleList(locales);
    const opts = coerceOptions(options);
    localeMatcher(opts);
    const nu = getOption(opts, 'numberingSystem', 'string', undefined, undefined);
    if (nu !== undefined && !TYPE_RE.test(nu)) throw new RangeError('Invalid numberingSystem : ' + nu);
    const found = lookupLocale(isEnglish, requested);
    const style = getOption(opts, 'style', 'string', ['long', 'short', 'narrow'], 'long');
    const numeric = getOption(opts, 'numeric', 'string', ['always', 'auto'], 'always');
    RTF.set(this, { locale: withKeywords(found.locale, found.keywords.nu === 'latn' && nu === undefined ? { nu: 'latn' } : {}), style, numeric, nf: NF.get(new NumberFormat('en-US')) });
  }
  const rtfSlots = (o, m) => slotsOf(RTF, o, 'Intl.RelativeTimeFormat.prototype.' + m);

  function relativeParts(r, value, unit) {
    let v = Number(value);
    if (!Number.isFinite(v)) throw new RangeError('Invalid value for relative time');
    let u = String(unit);
    if (/s$/.test(u) && RT_UNITS.indexOf(u.slice(0, -1)) !== -1) u = u.slice(0, -1);
    if (RT_UNITS.indexOf(u) === -1) throw new RangeError("Invalid unit argument for format() '" + String(unit) + "'");
    if (r.numeric === 'auto' && (v === -1 || v === 0 || v === 1)) {
      const phrases = (r.style === 'long' ? RT_AUTO_LONG : RT_AUTO_SHORT)[u];
      const phrase = phrases[v + 1];
      if (phrase) return [{ type: 'literal', value: phrase }];
    }
    const past = v < 0 || Object.is(v, -0);
    v = Math.abs(v);
    const num = partitionNumber(r.nf, v).map((p) => Object.assign(p, { unit: u }));
    const one = joinParts(num) === '1';
    const name = RT_NAMES[r.style][u][one ? 0 : 1];
    const sep = r.style === 'narrow' ? '' : ' ';
    if (past) return num.concat([{ type: 'literal', value: sep + name + ' ago' }]);
    return [{ type: 'literal', value: 'in ' }].concat(num, [{ type: 'literal', value: sep + name }]);
  }
  define(RelativeTimeFormat.prototype, {
    format(value, unit) { return joinParts(relativeParts(rtfSlots(this, 'format'), value, unit)); },
    formatToParts(value, unit) { return relativeParts(rtfSlots(this, 'formatToParts'), value, unit); },
    resolvedOptions() {
      const r = rtfSlots(this, 'resolvedOptions');
      return { locale: r.locale, style: r.style, numeric: r.numeric, numberingSystem: 'latn' };
    },
  });
  tag(RelativeTimeFormat.prototype, 'Intl.RelativeTimeFormat');
  define(RelativeTimeFormat, { supportedLocalesOf: supportedLocalesOf(isEnglish) });

  // ---------------------------------------------------------------------
  // ListFormat
  // ---------------------------------------------------------------------
  const LF = new WeakMap();
  // [between items, before the last of 3+, between 2].
  const LIST_PATTERNS = {
    conjunction: { long: [', ', ', and ', ' and '], short: [', ', ', & ', ' & '], narrow: [', ', ', ', ', '] },
    disjunction: { long: [', ', ', or ', ' or '], short: [', ', ', or ', ' or '], narrow: [', ', ', or ', ' or '] },
    unit: { long: [', ', ', ', ', '], short: [', ', ', ', ', '], narrow: [' ', ' ', ' '] },
  };
  function ListFormat(locales, options) {
    if (!new.target) throw new TypeError("Constructor Intl.ListFormat requires 'new'");
    const requested = canonicalizeLocaleList(locales);
    const opts = optionsObject(options);
    localeMatcher(opts);
    const found = lookupLocale(isEnglish, requested);
    const type = getOption(opts, 'type', 'string', ['conjunction', 'disjunction', 'unit'], 'conjunction');
    const style = getOption(opts, 'style', 'string', ['long', 'short', 'narrow'], 'long');
    LF.set(this, { locale: found.locale, type, style });
  }
  const lfSlots = (o, m) => slotsOf(LF, o, 'Intl.ListFormat.prototype.' + m);
  function stringList(list) {
    if (list === undefined) return [];
    const out = [];
    for (const v of list) {
      if (typeof v !== 'string') throw new TypeError('Iterable yielded ' + describe(v) + ' which is not a string');
      out.push(v);
    }
    return out;
  }
  function listParts(r, list) {
    const items = stringList(list);
    const [mid, last, pair] = LIST_PATTERNS[r.type][r.style];
    const out = [];
    items.forEach((s, i) => {
      if (i) out.push({ type: 'literal', value: items.length === 2 ? pair : i === items.length - 1 ? last : mid });
      out.push({ type: 'element', value: s });
    });
    return out;
  }
  define(ListFormat.prototype, {
    format(list) { return joinParts(listParts(lfSlots(this, 'format'), list)); },
    formatToParts(list) { return listParts(lfSlots(this, 'formatToParts'), list); },
    resolvedOptions() {
      const r = lfSlots(this, 'resolvedOptions');
      return { locale: r.locale, type: r.type, style: r.style };
    },
  });
  tag(ListFormat.prototype, 'Intl.ListFormat');
  define(ListFormat, { supportedLocalesOf: supportedLocalesOf(isEnglish) });

  // ---------------------------------------------------------------------
  // Segmenter: Unicode text segmentation (UAX #29) without dictionaries,
  // so Chinese/Japanese words split per character (kana runs stay
  // together) and Thai/Lao/Khmer by letter runs.
  // ---------------------------------------------------------------------
  const SEG = new WeakMap();
  const RE_EXTEND = /[\p{M}‌︀-️\u{1f3fb}-\u{1f3ff}\u{e0020}-\u{e007f}\u{e0100}-\u{e01ef}]/u;
  const RE_PICT = /\p{Extended_Pictographic}/u;
  const RE_CONTROL = /[\p{Cc}\p{Zl}\p{Zp}\p{Cf}]/u;
  const RE_RI = /\p{RI}/u;

  function hangul(cp) {
    if ((cp >= 0x1100 && cp <= 0x115f) || (cp >= 0xa960 && cp <= 0xa97c)) return 'L';
    if ((cp >= 0x1160 && cp <= 0x11a7) || (cp >= 0xd7b0 && cp <= 0xd7c6)) return 'V';
    if ((cp >= 0x11a8 && cp <= 0x11ff) || (cp >= 0xd7cb && cp <= 0xd7fb)) return 'T';
    if (cp >= 0xac00 && cp <= 0xd7a3) return (cp - 0xac00) % 28 === 0 ? 'LV' : 'LVT';
    return null;
  }

  function graphemeClass(cp, ch) {
    if (cp === 13) return 'CR';
    if (cp === 10) return 'LF';
    if (cp === 0x200d) return 'ZWJ';
    if (RE_EXTEND.test(ch)) return 'Extend';
    if (RE_CONTROL.test(ch)) return 'Control';
    if (RE_RI.test(ch)) return 'RI';
    const h = hangul(cp);
    if (h) return h;
    if (RE_PICT.test(ch)) return 'Pict';
    return 'Other';
  }

  // Grapheme cluster boundaries (code unit offsets, including 0 and length).
  function graphemeBreaks(s) {
    const out = [0];
    let prev = null;
    let ri = 0;
    let pictSeq = false;
    let zwjAfterPict = false;
    for (let i = 0; i < s.length;) {
      const cp = s.codePointAt(i);
      const ch = String.fromCodePoint(cp);
      const cls = graphemeClass(cp, ch);
      if (prev !== null) {
        let join = false;
        if (prev === 'CR' && cls === 'LF') join = true;
        else if (prev === 'CR' || prev === 'LF' || prev === 'Control' || cls === 'CR' || cls === 'LF' || cls === 'Control') join = false;
        else if (prev === 'L' && (cls === 'L' || cls === 'V' || cls === 'LV' || cls === 'LVT')) join = true;
        else if ((prev === 'LV' || prev === 'V') && (cls === 'V' || cls === 'T')) join = true;
        else if ((prev === 'LVT' || prev === 'T') && cls === 'T') join = true;
        else if (cls === 'Extend' || cls === 'ZWJ') join = true;
        else if (prev === 'ZWJ' && cls === 'Pict' && zwjAfterPict) join = true;
        else if (prev === 'RI' && cls === 'RI' && ri % 2 === 1) join = true;
        if (!join) out.push(i);
      }
      if (cls === 'RI') ri = prev === 'RI' ? ri + 1 : 1;
      else ri = 0;
      zwjAfterPict = cls === 'ZWJ' && pictSeq;
      if (cls === 'Pict') pictSeq = true;
      else if (cls !== 'Extend') pictSeq = false;
      prev = cls;
      i += ch.length;
    }
    if (s.length) out.push(s.length);
    return out;
  }

  const RE_HAN = /[\p{sc=Han}\p{sc=Hiragana}々〆〇]/u;
  const RE_KATAKANA = /[\p{sc=Katakana}ー〱-〵゛゜]/u;
  const RE_ALETTER = /[\p{Alphabetic}·˂-˅˒-˗]/u;
  const MID_LETTER = ':··՟״‧︓﹕：';
  const MID_NUM = ',;;։،؍٬߸⁄︐︔﹐﹔，；';
  const MID_NUM_LET = '.\'‘’․﹒＇．';

  function wordClass(ch) {
    const cp = ch.codePointAt(0);
    if (cp === 13) return 'CR';
    if (cp === 10) return 'LF';
    if (cp === 0x0b || cp === 0x0c || cp === 0x85 || cp === 0x2028 || cp === 0x2029) return 'Newline';
    if (cp === 0x200d) return 'ZWJ';
    if (RE_EXTEND.test(ch) || (RE_CONTROL.test(ch) && /\p{Cf}/u.test(ch))) return 'Extend';
    if (RE_RI.test(ch)) return 'RI';
    if (/\p{Nd}/u.test(ch)) return 'Numeric';
    if (RE_KATAKANA.test(ch)) return 'Katakana';
    if (RE_HAN.test(ch)) return 'Ideo';
    if (ch !== ':' && MID_LETTER.indexOf(ch) !== -1) return 'MidLetter';
    if (MID_NUM_LET.indexOf(ch) !== -1) return 'MidNumLet';
    if (MID_NUM.indexOf(ch) !== -1) return 'MidNum';
    if (/\p{Pc}/u.test(ch)) return 'ExtendNumLet';
    if (/\p{Zs}/u.test(ch)) return 'WSegSpace';
    if (RE_PICT.test(ch)) return 'Pict';
    if (RE_ALETTER.test(ch) && !/\p{P}/u.test(ch)) return 'ALetter';
    return 'Other';
  }

  // Word segments: `[start, end, isWordLike]`.
  function wordSegments(s) {
    // Units: a character plus attached Extend/Format/ZWJ characters (WB4).
    const units = [];
    for (let i = 0; i < s.length;) {
      const cp = s.codePointAt(i);
      const ch = String.fromCodePoint(cp);
      const cls = wordClass(ch);
      const last = units[units.length - 1];
      if ((cls === 'Extend' || cls === 'ZWJ') && last && last.cls !== 'CR' && last.cls !== 'LF' && last.cls !== 'Newline') {
        last.end = i + ch.length;
        last.zwj = cls === 'ZWJ';
      } else units.push({ start: i, end: i + ch.length, cls, zwj: cls === 'ZWJ' });
      i += ch.length;
    }
    const AH = (c) => c === 'ALetter';
    const midL = (c) => c === 'MidLetter' || c === 'MidNumLet';
    const midN = (c) => c === 'MidNum' || c === 'MidNumLet';
    const out = [];
    let start = 0;
    // Regional indicators pair up from the start of each run.
    const riRun = [];
    units.forEach((u, k) => { riRun[k] = u.cls === 'RI' ? (k && units[k - 1].cls === 'RI' ? riRun[k - 1] + 1 : 1) : 0; });
    for (let k = 1; k <= units.length; k++) {
      let join = false;
      if (k < units.length) {
        const a = units[k - 1].cls;
        const b = units[k].cls;
        const prev2 = k >= 2 ? units[k - 2].cls : null;
        const next = k + 1 < units.length ? units[k + 1].cls : null;
        if (a === 'CR' && b === 'LF') join = true;
        else if (a === 'CR' || a === 'LF' || a === 'Newline' || b === 'CR' || b === 'LF' || b === 'Newline') join = false;
        else if (units[k - 1].zwj && b === 'Pict') join = true;
        else if (a === 'WSegSpace' && b === 'WSegSpace') join = true;
        else if (AH(a) && AH(b)) join = true;
        else if (AH(a) && midL(b) && next && AH(next)) join = true;
        else if (midL(a) && AH(b) && prev2 && AH(prev2)) join = true;
        else if (a === 'Numeric' && b === 'Numeric') join = true;
        else if ((AH(a) && b === 'Numeric') || (a === 'Numeric' && AH(b))) join = true;
        else if (midN(a) && b === 'Numeric' && prev2 === 'Numeric') join = true;
        else if (a === 'Numeric' && midN(b) && next === 'Numeric') join = true;
        else if (a === 'Katakana' && b === 'Katakana') join = true;
        else if ((AH(a) || a === 'Numeric' || a === 'Katakana' || a === 'ExtendNumLet') && b === 'ExtendNumLet') join = true;
        else if (a === 'ExtendNumLet' && (AH(b) || b === 'Numeric' || b === 'Katakana')) join = true;
        else if (a === 'RI' && b === 'RI' && riRun[k - 1] % 2 === 1) join = true;
      }
      if (!join) {
        const seg = units.slice(start, k);
        const wordLike = seg.some((u) => u.cls === 'ALetter' || u.cls === 'Numeric' || u.cls === 'Katakana' || u.cls === 'Ideo');
        out.push([units[start].start, units[k - 1].end, wordLike]);
        start = k;
      }
    }
    return out;
  }

  // Sentence boundaries (simplified UAX #29: terminators, closing
  // punctuation, spaces, and no break before lower-case continuations).
  function sentenceBreaks(s) {
    const out = [0];
    const isSep = (c) => c === '\n' || c === '\r' || c === '\u0085' || c === ' ' || c === ' ';
    const isClose = (c) => /[\p{Pe}\p{Pf}\p{Pi}"'»”’]/u.test(c);
    const isSpace = (c) => /[\p{Zs}\t\u000b\u000c]/u.test(c);
    const STERM = '!?‼‽⁇⁈⁉。！？։؟۔।॥';
    for (let i = 0; i < s.length;) {
      const c = s[i];
      if (isSep(c)) {
        i += c === '\r' && s[i + 1] === '\n' ? 2 : 1;
        if (i < s.length) out.push(i);
        continue;
      }
      const aterm = c === '.' || c === '․' || c === '﹒' || c === '．';
      if (!aterm && STERM.indexOf(c) === -1) {
        i++;
        continue;
      }
      let j = i + 1;
      while (j < s.length && (s[j] === '.' || STERM.indexOf(s[j]) !== -1)) j++;
      if (aterm && j === i + 1) {
        const before = s[i - 1] || '';
        const after = s[j] || '';
        // 3.14, U.S.A
        if (/\p{Nd}/u.test(after) || (/\p{Lu}/u.test(after) && /\p{L}/u.test(before))) {
          i = j;
          continue;
        }
      }
      while (j < s.length && isClose(s[j])) j++;
      const spaceStart = j;
      while (j < s.length && isSpace(s[j])) j++;
      if (j < s.length && isSep(s[j])) {
        i = j;
        continue;
      }
      if (aterm || s.slice(i, spaceStart).indexOf('.') !== -1) {
        let k = j;
        while (k < s.length && !/[\p{L}\p{Nd}]/u.test(s[k]) && !isSep(s[k]) && STERM.indexOf(s[k]) === -1 && s[k] !== '.') k++;
        if (k < s.length && /\p{Ll}/u.test(s[k])) {
          i = j;
          continue;
        }
      }
      if (j === spaceStart && j < s.length && !isClose(s[j]) && aterm) {
        i = j;
        continue;
      }
      if (j < s.length) out.push(j);
      i = j;
    }
    if (s.length) out.push(s.length);
    return out.filter((v, k) => k === 0 || v !== out[k - 1]);
  }

  function segmentsOf(granularity, s) {
    if (granularity === 'word') return wordSegments(s);
    const b = granularity === 'sentence' ? sentenceBreaks(s) : graphemeBreaks(s);
    const out = [];
    for (let i = 1; i < b.length; i++) out.push([b[i - 1], b[i]]);
    return out;
  }

  function Segmenter(locales, options) {
    if (!new.target) throw new TypeError("Constructor Intl.Segmenter requires 'new'");
    const requested = canonicalizeLocaleList(locales);
    const opts = optionsObject(options);
    localeMatcher(opts);
    const found = lookupLocale(collatorAvailable, requested);
    const granularity = getOption(opts, 'granularity', 'string', ['grapheme', 'word', 'sentence'], 'grapheme');
    SEG.set(this, { locale: found.locale, granularity });
  }
  const segSlots = (o, m) => slotsOf(SEG, o, 'Intl.Segmenter.prototype.' + m);

  const SEGMENTS = new WeakMap();
  const SegmentsPrototype = {};
  const IteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  const SegmentIteratorPrototype = Object.create(IteratorPrototype);

  function segmentData(st, seg) {
    const out = { segment: st.input.slice(seg[0], seg[1]), index: seg[0], input: st.input };
    if (st.granularity === 'word') out.isWordLike = seg[2];
    return out;
  }
  function segmentList(st) {
    if (!st.list) st.list = segmentsOf(st.granularity, st.input);
    return st.list;
  }
  define(SegmentsPrototype, {
    containing(index) {
      const st = slotsOf(SEGMENTS, this, '%Segments.prototype%.containing');
      let n = Math.trunc(Number(index)) || 0;
      if (n < 0 || n >= st.input.length) return undefined;
      const list = segmentList(st);
      let lo = 0;
      let hi = list.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (list[mid][0] <= n) lo = mid;
        else hi = mid - 1;
      }
      return segmentData(st, list[lo]);
    },
    [Symbol.iterator]() {
      const st = slotsOf(SEGMENTS, this, '%Segments.prototype%[@@iterator]');
      const it = Object.create(SegmentIteratorPrototype);
      SEGMENTS.set(it, { st, i: 0 });
      return it;
    },
  });
  define(SegmentIteratorPrototype, {
    next() {
      const s = slotsOf(SEGMENTS, this, '%SegmentIterator.prototype%.next');
      const list = segmentList(s.st);
      if (s.i >= list.length) return { value: undefined, done: true };
      return { value: segmentData(s.st, list[s.i++]), done: false };
    },
  });
  tag(SegmentIteratorPrototype, 'Segmenter String Iterator');
  define(Segmenter.prototype, {
    segment(input) {
      const r = segSlots(this, 'segment');
      const segments = Object.create(SegmentsPrototype);
      SEGMENTS.set(segments, { input: String(input), granularity: r.granularity, list: null });
      return segments;
    },
    resolvedOptions() {
      const r = segSlots(this, 'resolvedOptions');
      return { locale: r.locale, granularity: r.granularity };
    },
  });
  tag(Segmenter.prototype, 'Intl.Segmenter');
  define(Segmenter, { supportedLocalesOf: supportedLocalesOf(collatorAvailable) });

  // ---------------------------------------------------------------------
  // Locale
  // ---------------------------------------------------------------------
  const LOCALE = new WeakMap();
  const LIKELY = {
    en: 'Latn-US', de: 'Latn-DE', fr: 'Latn-FR', es: 'Latn-ES', it: 'Latn-IT', nl: 'Latn-NL', pt: 'Latn-BR', id: 'Latn-ID',
    ms: 'Latn-MY', jv: 'Latn-ID', su: 'Latn-ID', ja: 'Jpan-JP', zh: 'Hans-CN', ko: 'Kore-KR', ru: 'Cyrl-RU', uk: 'Cyrl-UA',
    pl: 'Latn-PL', sv: 'Latn-SE', da: 'Latn-DK', nb: 'Latn-NO', no: 'Latn-NO', fi: 'Latn-FI', cs: 'Latn-CZ', tr: 'Latn-TR',
    el: 'Grek-GR', ar: 'Arab-EG', he: 'Hebr-IL', fa: 'Arab-IR', hi: 'Deva-IN', bn: 'Beng-BD', th: 'Thai-TH', vi: 'Latn-VN',
    fil: 'Latn-PH', ro: 'Latn-RO', hu: 'Latn-HU', und: 'Latn-US',
  };
  const RTL = ['ar', 'he', 'fa', 'ur', 'ps', 'sd', 'ug', 'yi', 'dv', 'ckb'];
  const SUNDAY_FIRST = ['US', 'CA', 'MX', 'BR', 'JP', 'ID', 'IN', 'IL', 'KR', 'PH', 'TW', 'HK', 'ZA', 'SA', 'AR', 'CO', 'PE', 'TH', 'PK', 'KE'];

  function maximize(p) {
    const q = Object.assign({}, p, { variants: p.variants.slice(), extensions: p.extensions.slice() });
    if (q.language === 'zh' && !q.script && ['TW', 'HK', 'MO'].indexOf(q.region) !== -1) q.script = 'Hant';
    if (q.language === 'zh' && q.script === 'Hant' && !q.region) q.region = 'TW';
    const likely = LIKELY[q.language];
    if (likely) {
      const [script, region] = likely.split('-');
      if (!q.script) q.script = script;
      if (!q.region) q.region = region;
    }
    if (q.language === 'und') q.language = 'en';
    return q;
  }
  function minimize(p) {
    const max = tagToString(maximize(p));
    const base = (o) => tagToString(Object.assign({}, p, o));
    for (const cand of [{ script: undefined, region: undefined }, { script: undefined }, { region: undefined }]) {
      if (tagToString(maximize(Object.assign({}, p, cand))) === max) return base(cand);
    }
    return tagToString(p);
  }

  function keywordsOf(p) {
    const ext = p.extensions.find((e) => e.singleton === 'u');
    const out = Object.create(null);
    if (ext) for (const [k, v] of parseUnicodeExtension(ext.sub).keywords) out[k] = v === '' ? 'true' : v;
    return out;
  }

  function Locale(tagArg, options) {
    if (!new.target) throw new TypeError("Constructor Intl.Locale requires 'new'");
    if (typeof tagArg !== 'string' && !isObject(tagArg)) throw new TypeError('First argument to Intl.Locale constructor can\'t be empty or missing');
    const p = parseCanonical(tagArg instanceof Locale ? LOCALE.get(tagArg).tag : String(tagArg));
    const opts = coerceOptions(options);
    const language = getOption(opts, 'language', 'string', undefined, undefined);
    if (language !== undefined) {
      if (!/^([a-z]{2,3}|[a-z]{5,8})$/i.test(language)) throw new RangeError('Incorrect locale information provided');
      p.language = language.toLowerCase();
    }
    const script = getOption(opts, 'script', 'string', undefined, undefined);
    if (script !== undefined) {
      if (!/^[a-z]{4}$/i.test(script)) throw new RangeError('Incorrect locale information provided');
      p.script = script[0].toUpperCase() + script.slice(1).toLowerCase();
    }
    const region = getOption(opts, 'region', 'string', undefined, undefined);
    if (region !== undefined) {
      if (!/^([a-z]{2}|\d{3})$/i.test(region)) throw new RangeError('Incorrect locale information provided');
      p.region = region.toUpperCase();
    }
    const kw = keywordsOf(p);
    const set = (key, value, re) => {
      if (value === undefined) return;
      if (re && !re.test(value)) throw new RangeError('Incorrect locale information provided');
      kw[key] = value.toLowerCase();
    };
    set('ca', getOption(opts, 'calendar', 'string', undefined, undefined), TYPE_RE);
    set('co', getOption(opts, 'collation', 'string', undefined, undefined), TYPE_RE);
    set('fw', getOption(opts, 'firstDayOfWeek', 'string', undefined, undefined), TYPE_RE);
    set('hc', getOption(opts, 'hourCycle', 'string', ['h11', 'h12', 'h23', 'h24'], undefined));
    set('kf', getOption(opts, 'caseFirst', 'string', ['upper', 'lower', 'false'], undefined));
    const numeric = getOption(opts, 'numeric', 'boolean', undefined, undefined);
    if (numeric !== undefined) kw.kn = String(numeric);
    set('nu', getOption(opts, 'numberingSystem', 'string', undefined, undefined), TYPE_RE);
    const keys = Object.keys(kw).sort();
    p.extensions = p.extensions.filter((e) => e.singleton !== 'u');
    if (keys.length) {
      const sub = [];
      for (const k of keys) {
        sub.push(k);
        if (kw[k] !== 'true') sub.push(...kw[k].split('-'));
      }
      p.extensions.push({ singleton: 'u', sub });
    }
    LOCALE.set(this, { p, kw, tag: tagToString(p) });
  }
  const locSlots = (o, m) => slotsOf(LOCALE, o, 'Intl.Locale.prototype.' + m);
  const baseNameOf = (p) => tagToString({ language: p.language, script: p.script, region: p.region, variants: p.variants, extensions: [] });
  const weekInfo = (p) => {
    const region = maximize(p).region;
    const sunday = SUNDAY_FIRST.indexOf(region) !== -1;
    const europe = ['DE', 'FR', 'GB', 'IT', 'ES', 'NL', 'BE', 'AT', 'CH', 'SE', 'NO', 'DK', 'FI', 'PL', 'CZ', 'IE', 'PT', 'RU'].indexOf(region) !== -1;
    return { firstDay: sunday ? 7 : 1, weekend: [6, 7], minimalDays: europe ? 4 : 1 };
  };
  const H12_REGIONS = ['US', 'CA', 'AU', 'NZ', 'IN', 'PH', 'KR', 'EG', 'SA', 'PK'];
  const hourCycles = (s) => [s.kw.hc || (H12_REGIONS.indexOf(maximize(s.p).region) !== -1 && s.p.language !== 'fr' ? 'h12' : 'h23')];
  define(Locale.prototype, {
    toString() { return locSlots(this, 'toString').tag; },
    maximize() { return new Locale(tagToString(maximize(locSlots(this, 'maximize').p))); },
    minimize() { return new Locale(minimize(locSlots(this, 'minimize').p)); },
    get baseName() { return baseNameOf(locSlots(this, 'baseName').p); },
    get language() { return locSlots(this, 'language').p.language; },
    get script() { return locSlots(this, 'script').p.script; },
    get region() { return locSlots(this, 'region').p.region; },
    get calendar() { return locSlots(this, 'calendar').kw.ca; },
    get caseFirst() { return locSlots(this, 'caseFirst').kw.kf; },
    get collation() { return locSlots(this, 'collation').kw.co; },
    get firstDayOfWeek() { return locSlots(this, 'firstDayOfWeek').kw.fw; },
    get hourCycle() { return locSlots(this, 'hourCycle').kw.hc; },
    get numeric() { const kn = locSlots(this, 'numeric').kw.kn; return kn === 'true'; },
    get numberingSystem() { return locSlots(this, 'numberingSystem').kw.nu; },
    get calendars() { return this.getCalendars(); },
    get collations() { return this.getCollations(); },
    get hourCycles() { return this.getHourCycles(); },
    get numberingSystems() { return this.getNumberingSystems(); },
    get textInfo() { return this.getTextInfo(); },
    get timeZones() { return this.getTimeZones(); },
    get weekInfo() { return this.getWeekInfo(); },
    getCalendars() { const s = locSlots(this, 'getCalendars'); return [s.kw.ca || 'gregory']; },
    getCollations() { locSlots(this, 'getCollations'); return ['emoji', 'eor']; },
    getHourCycles() { return hourCycles(locSlots(this, 'getHourCycles')); },
    getNumberingSystems() { const s = locSlots(this, 'getNumberingSystems'); return [s.kw.nu || 'latn']; },
    getTextInfo() { return { direction: RTL.indexOf(locSlots(this, 'getTextInfo').p.language) !== -1 ? 'rtl' : 'ltr' }; },
    getTimeZones() {
      const s = locSlots(this, 'getTimeZones');
      if (!s.p.region) return undefined;
      return s.p.region === 'ID' ? ['Asia/Jakarta', 'Asia/Jayapura', 'Asia/Makassar', 'Asia/Pontianak'] : [];
    },
    getWeekInfo() { return weekInfo(locSlots(this, 'getWeekInfo').p); },
  });
  tag(Locale.prototype, 'Intl.Locale');

  // ---------------------------------------------------------------------
  // Intl
  // ---------------------------------------------------------------------
  const Intl = {};
  define(Intl, {
    getCanonicalLocales(locales) { return canonicalizeLocaleList(locales); },
    supportedValuesOf(key) {
      switch (String(key)) {
        case 'calendar': return ['gregory', 'iso8601'];
        case 'collation': return [];
        case 'currency': {
          const set = new Set(Object.keys(CURRENCY_DIGITS).concat(Object.keys(EN_SYMBOLS), Object.keys(NARROW_SYMBOLS)));
          currencyName('USD', true);
          for (const c of Object.keys(currencyNames)) set.add(c);
          return Array.from(set).sort();
        }
        case 'numberingSystem': return ['latn'];
        case 'timeZone': {
          metazoneOf('UTC');
          const zones = new Set(['UTC']);
          const local = localZone().id;
          if (local.indexOf('/') !== -1) zones.add(local);
          for (const z of Object.keys(zoneMetazone)) {
            if (z.indexOf('/') === -1 || /^(US|Etc)\//.test(z)) continue;
            let info;
            try { info = env.zoneOffset(z, Date.now()); } catch (e) { info = undefined; }
            if (info && info[2] === z) zones.add(z);
          }
          return Array.from(zones).sort();
        }
        case 'unit': return Object.keys(unitTable()).sort();
        default: throw new RangeError('Invalid key : ' + String(key));
      }
    },
    Collator, DateTimeFormat, ListFormat, Locale, NumberFormat, PluralRules, RelativeTimeFormat, Segmenter,
  });
  tag(Intl, 'Intl');

  return { Intl, numberToLocaleString, dateToLocaleString, localeCompare };
};
