// @event: prerequest
// @expect: {"environment": {"hmac": "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8", "hmacB64": "97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg=", "md5": "5d41402abc4b2a76b9719d911017c592", "sha1": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", "b64": "aGVsbG8=", "aesRoundtrip": "secret message"}, "errors": 0}
const sig = CryptoJS.HmacSHA256("The quick brown fox jumps over the lazy dog", "key");
pm.environment.set('hmac', sig.toString());
pm.environment.set('hmacB64', CryptoJS.enc.Base64.stringify(sig));
pm.environment.set('md5', CryptoJS.MD5('hello').toString());
pm.environment.set('sha1', CryptoJS.SHA1('hello').toString(CryptoJS.enc.Hex));
pm.environment.set('sha256', require('crypto-js').SHA256('hello').toString());
pm.environment.set('b64', CryptoJS.enc.Base64.stringify(CryptoJS.enc.Utf8.parse('hello')));
const encrypted = CryptoJS.AES.encrypt('secret message', 'passphrase').toString();
pm.environment.set('aesRoundtrip', CryptoJS.AES.decrypt(encrypted, 'passphrase').toString(CryptoJS.enc.Utf8));
