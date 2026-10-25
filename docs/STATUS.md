# Status implementasi PRD Merpati

Dokumen ini memetakan setiap persyaratan di PRD ke kode yang mengerjakannya, berikut
hasil benchmark dan keputusan atas pertanyaan terbuka. Status: ✅ selesai dan teruji,
🟡 sebagian, ⛔ belum dikerjakan.

## Keputusan atas pertanyaan terbuka (bagian 15)

| Pertanyaan | Keputusan |
| --- | --- |
| Arti "100 persen" | Fitur klien (kelompok A dan B). Fitur server (sinkronisasi tim, monitor cloud, webhook publik, API Network, governance, SSO) tidak dibangun; padanan lokalnya: workspace berupa folder Git, `merpati schedule`, listener webhook lokal. |
| Mesin JavaScript | `boa_engine` (Rust murni) di balik trait `ScriptEngine`. Gate A lulus: korpus 50 skrip Postman realistis lulus 50/50, job hangat ~3,4 ms. Chai, lodash, moment, crypto-js, tv4 asli di-bundle dan dimuat malas. QuickJS tidak diperlukan. |
| Framework UI | `egui`/`eframe` 0.37 dengan backend wgpu (Metal di macOS). Editor kode dan penampil respons ditulis sendiri di atas egui (penyorotan sintaks, tampilan virtual untuk respons besar). |
| Platform | macOS sebagai target utama dan yang diukur; kode lintas platform (CI juga menguji Linux dan mengompilasi Windows). |
| Format penyimpanan, lisensi, nama | JSON rapi, satu berkas per request. Lisensi MIT OR Apache-2.0. Nama kerja Merpati dipertahankan; ketersediaan nama di crates.io/domain belum diperiksa. |

## Target terukur (bagian 2)

Diukur pada build `dist` (fat LTO, `opt-level = "s"`) di MacBook (Apple Silicon, layar Retina, jendela 1280×820),
diulang dengan `scripts/bench.sh`. "Footprint" adalah memori fisik seperti di Activity
Monitor (termasuk buffer GPU); RSS dari `ps`. Skrip berjalan di proses pekerja terpisah (lihat FR-S2); memorinya dicatat
terpisah dan hanya ada setelah skrip pertama berjalan.

| Metrik | Target | Hasil |
| --- | --- | --- |
| Memori idle, 1 workspace | < 100 MB | ✅ 72–88 MB footprint (RSS 50–70 MB; 77 MB pada pengukuran terakhir); 83 MB dengan tab MCP, Socket.IO, AI dan webhook terhubung sekaligus |
| Memori, 5 tab request aktif | < 150 MB | ✅ 89–93 MB footprint aplikasi + proses pekerja skrip 33 MB RSS (request-nya punya skrip) = ±125 MB |
| Waktu start dingin | < 1 detik | ✅ ~40 ms sampai frame pertama (berkas di cache); 60–240 ms pada peluncuran pertama binary baru (pemeriksaan keamanan macOS) |
| Ukuran binary | < 40 MB | ✅ app 31,7 MiB / 33,2 MB (arm64) dengan profil `dist`, termasuk mesin skrip dengan polyfill Intl/Streams dan basis data zona waktu, PAC, daftar public suffix, semua protokol, proxy, Flows dan penampil HTML; CLI 19,9 MiB. Proses pekerja skrip memakai binary yang sama, jadi tidak menambah ukuran. Dengan `opt-level = 3` app ±40 MB: start sama, runner ~10% lebih cepat, terlalu dekat batas |
| Workspace 10.000 request | < 1 detik | ✅ frame pertama 276–315 ms (cache hangat), ~750 ms pada peluncuran pertama binary baru; pohon penuh 95–109 MB footprint |
| Respons 100 MB | UI responsif, di-stream | ✅ diterima 73 ms (lokal), disimpan di berkas sementara, 550.741 baris tampil virtual; footprint 82–99 MB |
| Runner 1.000 iterasi | Tanpa pertumbuhan linear | ✅ RSS maks CLI 21,5 / 21,1 / 21,5 MB dan pekerja skrip 31,8 / 29,0 / 30,4 MB untuk 100 / 1.000 / 3.000 iterasi |
| Overhead per request | < 5 ms | ✅ 1,7–2,0 ms per request termasuk skrip JS (di proses pekerja) dan server HTTP lokal (Python) |
| CPU idle | ~0% | ✅ 0,01 detik CPU dalam 15 detik idle (render hanya saat ada perubahan) |
| Impor 10.000 request | — | 1,5–1,9 detik |

## Persyaratan fungsional

### 5.1 Request builder HTTP

| ID | Status | Catatan |
| --- | --- | --- |
| FR-R1 | ✅ | Semua metode + metode kustom; URL tanpa skema diberi `http://` (`urlutil::ensure_scheme`). |
| FR-R2 | ✅ | Params dan path variables (`:id`) tersinkron dua arah dengan URL, baris bisa dinonaktifkan. |
| FR-R3 | ✅ | Tabel header dengan autocomplete nama; header otomatis bisa dilihat dan dimatikan. |
| FR-R4 | ✅ | none, form-data (teks/berkas), urlencoded, raw (JSON/XML/teks/JS/HTML), binary, GraphQL; penyorotan dan Beautify JSON (aman untuk `{{var}}`). |
| FR-R5 | ✅ | Kirim (⌘↩), batal, simpan otomatis (debounce 0,5 dtk, thread latar); status, waktu (TTFB/total), ukuran. |
| FR-R6 | ✅ | Redirect (dengan batas, metode asli, header Authorization), verifikasi SSL, encoding URL, timeout, HTTP/1.1 / HTTP/2. |
| FR-R7 | ✅ | Cookie jar RFC 6265 per domain, bisa dilihat/diubah/dihapus, disimpan lokal. |
| FR-R8 | ✅ | CA tambahan, sertifikat klien per host (PEM atau PKCS#12 `.p12/.pfx`, termasuk enkripsi lama RC2/3DES; frasa sandi disimpan di Keychain, pengaturan hanya berisi `{{vault:settings.…}}`), proxy sistem dan kustom (HTTP/HTTPS/SOCKS5/SOCKS4) dengan bypass (`*.domain` didukung); kata sandi proxy juga disimpan di Keychain (pengaturan lama dipindahkan otomatis). Proxy auto-config (PAC): skrip `FindProxyForURL` dari pengaturan OS (macOS: URL PAC atau skrip inline; Windows: `AutoConfigURL`) atau dari kolom URL PAC proxy kustom (`http(s)://` atau `file://`) dijalankan di konteks JavaScript terpisah tanpa akses jaringan/berkas, dengan semua fungsi bantu standar (`shExpMatch`, `isInNet`, `dnsResolve`, `myIpAddress`, `weekdayRange`/`dateRange`/`timeRange`, varian `Ex` Microsoft) dan hasil di-cache per host; URL HTTPS diberikan ke skrip tanpa path/query seperti Chrome. Penemuan proxy otomatis (WPAD lewat DNS) bila OS memintanya tanpa URL PAC (macOS "Auto Proxy Discovery", Windows "Automatically detect settings"): `http://wpad.<domain>/wpad.dat` dicoba untuk tiap domain pencarian DNS apa adanya, tanpa turun ke domain induk (seperti Chrome dan Firefox kini), domain yang berupa public suffix (`com`, `co.uk`) dan `local` dilewati, 2 detik per kandidat, hasilnya (juga kegagalan) di-cache dan pengaturan sistem dibaca ulang di thread latar sehingga request tidak menunggu. Hasil PAC dengan beberapa entri (`PROXY a:8080; PROXY b:8080; DIRECT`) dipakai seperti browser: tunnel beralih ke entri berikutnya bila proxy tak terjangkau, mesin HTTP mengulang request lewat pilihan berikutnya, dan proxy yang gagal dicoba terakhir selama 5 menit (seperti Chrome). Tunnel CONNECT menjawab `Proxy-Authenticate: Digest` (RFC 7616, termasuk `-sess`/SHA-256) selain Basic. Proxy SOCKS dari pengaturan macOS juga dipakai. CA tambahan, sertifikat klien per host dan proxy juga dipakai WebSocket (termasuk subscription GraphQL), Socket.IO (polling dan WebSocket), MQTT (TCP/TLS/WS/WSS) dan gRPC, di tab maupun runner, dengan aturan yang sama seperti HTTP: proxy kustom bila aktif, lalu proxy sistem (`HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY` dan pengaturan OS; seperti browser, `localhost`/loopback dan di macOS daftar pengecualian serta nama host sederhana dari System Settings selalu langsung). TLS ke server tetap end-to-end di dalam tunnel CONNECT/SOCKS, dan galat proxy (407, kredensial SOCKS ditolak, proxy tak terjangkau) dijelaskan tanpa menampilkan kata sandi. |
| FR-R9 | ✅ | Riwayat yang bisa dicari, dibuka ulang, dihapus; nilai rahasia disamarkan. |
| FR-R10 | ✅ | 18 generator kode (cURL, HTTP, HTTPie, wget, fetch, axios, Python, Go, Rust, Java x2, Kotlin, C#, PHP, Swift, Ruby, Dart, PowerShell). |

### 5.2 Autentikasi — ✅ FR-A1 s.d. FR-A4

Semua tipe di FR-A1 ada, diverifikasi dengan test vector resmi bila tersedia (RFC 7616
Digest, OAuth 1.0 contoh Twitter, AWS SigV4 test suite, Hawk, RFC 7636 PKCE, MS-NLMP
NTLMv2, JWT jwt.io). OAuth 2.0: authorization code (+PKCE), implicit, password, client
credentials, refresh otomatis, token manual; token disimpan di Keychain, tidak di berkas; endpoint token yang mewajibkan mutual TLS menerima sertifikat klien yang diatur untuk host-nya (aplikasi, runner, CLI).
Pewarisan koleksi → folder → request. NTLM kini mengirim MsvAvTimestamp, MIC dan key
exchange; hasilnya identik byte demi byte dengan pyspnego 0.12.4 (8 vector) dan diterima
acceptor-nya. EdgeGrid lolos 15 kasus `testcases.json` edgegrid-python plus data uji node/php.
ASAP identik dengan atlassian-jwt-auth 22.0.1 (RS256/384/512) dan lolos verifier-nya.
NTLM lewat HTTPS mengikat pesan AUTHENTICATE ke sertifikat server (channel binding `tls-server-end-point`, RFC 5929), sehingga diterima server dengan Extended Protection "Required"; pemilihan hash sertifikat sama dengan requests-ntlm. JWT dan ASAP mendukung ES512 (P-521); token juga diverifikasi dengan OpenSSL CLI. Lewat proxy HTTP/HTTPS/SOCKS (reqwest tidak melaporkan sertifikat koneksi tunnel) mesin HTTP membaca sertifikat server lewat handshake TLS tersendiri melalui proxy yang sama, hanya untuk respons 401 yang meminta NTLM/Negotiate, sehingga channel binding juga terkirim di balik proxy.

### 5.3 Respons

| ID | Status | Catatan |
| --- | --- | --- |
| FR-P1 | ✅ | Body (Pretty/Raw/Preview/Tree), Header, Cookie, Hasil Tes, Details; pencarian dalam respons. |
| FR-P2 | ✅ | JSON/XML/HTML dengan penyorotan, pelipatan lewat tampilan Tree, respons besar di-stream ke berkas dan tampil virtual. |
| FR-P3 | ✅ | Simpan respons ke berkas; simpan sebagai example. |
| FR-P4 | ✅ | `pm.visualizer.set` dirender di tab Visualize di dalam aplikasi (`merpati-html`: Handlebars, HTML dan subset CSS termasuk grid, flexbox, float (teks mengalir kembali di bawah float), positioning (sticky menempel saat panel digulir, fixed tetap di viewport) dan z-index, `line-height` px/em diwarisi sebagai panjang, tabel tervirtualisasi; posisi kotak cocok dengan Chrome dalam ±1 px pada templat dasbor, profil dan cuaca). Halaman yang memakai skrip/sumber eksternal (mis. Chart.js) bisa dibuka di browser tanpa CDN. |

### 5.4 Koleksi — ✅ FR-C1 s.d. FR-C5

Folder bertingkat, drag-and-drop (pindah/urut, juga antar-koleksi), duplikasi, rename,
hapus; auth/skrip/variabel/deskripsi per koleksi dan folder; satu berkas per request;
impor/ekspor Postman v2.0/v2.1 (uji round-trip identik), cURL, OpenAPI 3/Swagger 2,
Insomnia, HAR, Hoppscotch, Thunder Client, WSDL; pencarian nama/URL/isi.
Impor cURL mengikuti curl untuk `--data-urlencode`/`--url-query` (`nama=isi` di-encode, `nama@berkas`,
`+` apa adanya), parameter `-F` (kutip, `type=`, `headers=`), prompt `$ curl`/`$curl`, dan tidak lagi
mengira data POSIX berisi `^"` sebagai sintaks cmd; isi berkas yang baru dibaca curl saat dijalankan
disimpan sebagai placeholder dengan peringatan. Impor HAR tidak mendekode dua kali params urlencoded
yang sudah didekode (Firefox; dicocokkan dengan `postData.text`) dan memperingatkan status > 65535.
Ekspor OpenAPI tidak menulis kredensial (nama/nilai mirip `Authorization`, API key, token, JWT, nilai
variabel secret) sebagai `example`; contoh dari skema yang saling merujuk dibatasi 5.000 nilai.

Fork lokal: menu koleksi → Fork… membuat salinan dengan ID sendiri (supaya token di Keychain
tidak saling menimpa) dan relasi di `.merpati/forks.json`. "Tarik perubahan dari induk" dan
"Gabungkan ke induk" menghitung perbedaan 3-arah per request/folder/pengaturan koleksi,
menampilkan diff, dan meminta pilihan untuk konflik.

### 5.5 Variabel — ✅ FR-V1 s.d. FR-V6

Lima cakupan dengan prioritas Postman; `{{var}}` disorot (merah bila tak terdefinisi),
tooltip nilai, autocomplete saat mengetik `{{`; nilai awal vs nilai saat ini (lokal);
tipe secret disamarkan dan tidak ditulis ke berkas; 118 variabel dinamis; Vault di
Keychain via `{{vault:nama}}`.

Integrasi secret manager (fase 3): nama vault bisa ditautkan ke HashiCorp Vault (token /
AppRole, KV v1/v2), AWS Secrets Manager (SigV4, kunci statis atau rantai bawaan), Azure Key
Vault (service principal atau Azure CLI) dan 1Password (CLI `op` atau Connect). Kredensial
penyedia ada di Keychain, nilai di-cache di memori dengan TTL dan tidak pernah ditulis ke
berkas; riwayat menyamarkannya. Diuji dengan server Vault sungguhan (Docker) dan mock untuk
tiga penyedia lainnya.

### 6.1 Skrip dan tes

| ID | Status | Catatan |
| --- | --- | --- |
| FR-S1 | ✅ | Pre-request dan post-response di koleksi, folder, request dengan urutan Postman. |
| FR-S2 | ✅ | Objek `pm` kompatibel, sandbox terisolasi, batas waktu dan pembatalan. Di aplikasi dan CLI skrip berjalan di proses pekerja terpisah (executable yang sama, dipakai ulang): skrip yang membuat mesin JavaScript crash (mis. data bersarang ratusan ribu tingkat), macet di kode native setelah batas waktu (regex katastrofik), atau memakai memori melebihi batas (Pengaturan → batas memori skrip, bawaan 1024 MB; berlaku di macOS, Linux dan Windows) hanya mematikan prosesnya; job gagal dengan pesan jelas dan aplikasi tetap jalan. Overhead ±0,03–0,1 ms per job. |
| FR-S3 | ✅ | `pm.test`/`pm.expect` (Chai asli), hasil per tes. |
| FR-S4 | ✅ | Baca/tulis semua cakupan, ubah request, `pm.sendRequest` (callback dan `await`). |
| FR-S5 | ✅ | Console (log skrip, baris jaringan, galat) dengan filter. |
| FR-S6 | ✅ | Snippet, autocomplete `pm.` di editor (Tab untuk menerima), jalankan ulang tes tanpa mengirim ulang. |
| FR-S7 | ✅ | Skrip per titik eksekusi protokol: gRPC (sebelum invoke, saat pesan, setelah respons), WebSocket/Socket.IO/MQTT (sebelum connect, saat pesan masuk), MCP (sebelum/ setelah tiap panggilan), AI (sebelum kirim/setelah respons). `pm.request` bisa diubah, `pm.message`/`pm.response` dengan asersi gRPC (`statusCode`, `metadata`, `trailer`). Dokumentasi: `docs/scripting.md`. |

### 6.2–6.4 Runner, CLI, Mock

| ID | Status | Catatan |
| --- | --- | --- |
| FR-U1–U5 | ✅ | Runner dengan environment, iterasi, jeda, cookie, data CSV/JSON, `setNextRequest`/`skipRequest` (target dicari per id lalu per nama; nama ganda memilih item terakhir, seperti `prepareLookupHash` postman-runtime), hasil per request/iterasi, ekspor JSON/JUnit/HTML, latar belakang, batal, memori datar. Runner dan CLI menjalankan semua protokol beserta skripnya: gRPC, AI, MCP, serta WebSocket/Socket.IO/MQTT sebagai sesi (connect, subscribe MQTT dulu, kirim pesan tersimpan berurutan dengan menunggu ack Socket.IO, skrip on-message tiap pesan masuk, ringkasan untuk post-response; berhenti setelah N pesan, saat event/topik tertentu tiba, setelah hening `wait_ms`, atau batas waktu; diatur di bagian Runner tab masing-masing; ack yang diminta server Socket.IO bisa dijawab otomatis dengan argumen yang ditentukan). Diuji dengan server lokal dan socket.io 4.8.4 sungguhan. |
| FR-U6 | ✅ | Urutan kustom dan simulasi kegagalan jaringan (persentase request yang digagalkan) di runner; simulasi kegagalan juga di mock server. |
| FR-U7 | ✅ | Performance test (VU, ramp-up, latensi p50–p99, throughput, galat) di UI dan CLI. |
| FR-L1–L2 | ✅ | CLI kompatibel newman (termasuk `--timeout`, `--folder` berisi nama request, `--timeout-script 0` = tanpa batas, `--bail [failure,folder]`, `-x/--suppress-exit-code`, `--cookie-jar` yang hanya dibaca (JSON tough-cookie newman atau jar Merpati) dan `--export-cookie-jar` (JSON tough-cookie), `--export-collection` dengan nilai akhir variabel koleksi, `--ssl-client-cert/-key/-passphrase` dan `--ssl-client-cert-list` format newman (diuji dengan server mTLS), `--color on\|off\|auto`, `--disable-unicode`), `mock --fail-with` yang tidak valid ditolak, reporter CLI/JSON/JUnit/HTML, kode keluar 0/1/2. Kredensial (header Authorization/Cookie/API key, kata sandi di URL, parameter token di query) disamarkan di laporan dan keluaran terminal; `--show-secrets` mempertahankannya. |
| FR-L3 | ✅ | `validate`, `convert`, Dockerfile (image dibangun dan dijalankan lokal: koleksi dengan tes, variabel bertipe, crash skrip yang terisolasi dan laporan JUnit; image 180 MB), GitHub Action (belum diuji di runner GitHub sungguhan). |
| FR-L4 | ✅ | `merpati schedule --every 5m`, hasil disimpan per run. |
| FR-M1–M4 | ✅ | Mock dari example, pencocokan gaya Postman, templating dinamis, simulasi kondisi jaringan (latensi, jitter, batas bandwidth, preset 4G/3G/satelit), kegagalan (status, putus koneksi, timeout), log, ganti `{{baseUrl}}` satu klik; juga lewat `merpati mock --network/--jitter/--bandwidth/--fail-rate`. |

### 7.1 Protokol lain

| ID | Status | Catatan |
| --- | --- | --- |
| FR-X1 GraphQL | ✅ | Query/mutation/subscription (graphql-transport-ws lewat WebSocket), variabel, introspeksi, penjelajah skema, autocomplete di editor, skeleton. |
| FR-X2 gRPC | ✅ | Skema dari berkas `.proto` (dikompilasi di proses dengan `protox`, mendukung import path) atau server reflection v1/v1alpha; unary, client-streaming, server-streaming, bidi; pesan protobuf-JSON dengan templat contoh; metadata, deadline, TLS dan mTLS (sertifikat klien PEM/PKCS#12 dari pengaturan). `google.protobuf.Any` berisi tipe yang tidak ada di skema ditampilkan seperti grpcurl (`{"@type", "@error", "@value": <base64>}`), bagian pesan lainnya tetap terlihat. Contoh server: `cargo run -p merpati-grpc --example greeter_server`. |
| FR-X3 WebSocket | ✅ | Header, subprotocol, teks/biner, log berstempel waktu, pesan tersimpan. Log pesan WebSocket, Socket.IO, MQTT, gRPC dan MCP tampil virtual (satu baris per pesan, pesan terpilih utuh di bawahnya), jadi puluhan ribu pesan tetap lancar. |
| FR-X4 Socket.IO | ✅ | Klien Socket.IO v2/v3/v4 (Engine.IO 3 dan 4) ditulis sendiri: long-polling dengan upgrade ke WebSocket seperti klien resmi, atau WebSocket/polling saja; namespace, payload `auth`, header/query/cookie, event yang didengar, emit dengan argumen dan ack, membalas ack dari server, event biner masuk dan keluar (BINARY_EVENT/BINARY_ACK dengan placeholder, juga biner bersarang di objek/array; argumen pesan tersimpan per jenis: JSON, teks, biner hex/base64, atau berkas). Diuji dengan socket.io 4.8.4 dan 2.5.1 sungguhan di ketiga transport: server menerima Buffer dengan byte yang persis, termasuk balasan ack biner dan emit biner sebelum dan sesudah upgrade. Server yang nakal tidak bisa menghabiskan memori: respons polling, pesan WebSocket dan lampiran biner per paket dibatasi 64 MiB (maks. 10.000 lampiran), lewat batas sesi diakhiri dengan pesan jelas. Belum: JSONP polling (sudah dihapus sejak Socket.IO v3). |
| FR-X5 MQTT | ✅ | MQTT 3.1.1 dan 5 lewat TCP, TLS, WebSocket, WSS (`rumqttc`); subscribe per QoS, publish dengan QoS dan retain, last will, client id/keep-alive/clean session, username/password (sandi di Keychain), log pesan berstempel waktu, pesan tersimpan. Diuji dengan mosquitto 2.1.2 sungguhan di empat transport. User properties MQTT 5 (kirim dan terima) tersedia di API crate (`publish_with_properties`, `connect_with_properties`): di tab MQTT (tabel User properties saat versi 5, juga untuk pesan tersimpan), di runner/CLI untuk pesan tersimpan, dan ditampilkan pada pesan masuk serta tersedia bagi skrip on-message (`pm.message.userProperties`). |
| FR-X6 SOAP | ✅ | Impor WSDL menjadi request dengan amplop dan SOAPAction. |
| FR-X7 Unix socket | ✅ | `http://unix:/path.sock:/path` dan `http+unix://`; named pipe Windows `http://npipe:docker_engine:/path` / `http+npipe://` (lolos kompilasi silang Windows; belum dijalankan di mesin Windows sungguhan). |
| FR-X8 MCP | ✅ | Klien MCP sendiri: STDIO, Streamable HTTP dan HTTP+SSE lama (otomatis atau dipilih); OAuth (discovery RFC 9728/8414, registrasi dinamis RFC 7591, PKCE, `resource`, refresh; token di Keychain); sampling (persetujuan pengguna lalu dijawab model dari request AI), elicitation (form dari skema); permintaan yang datang bersamaan antre dan dijawab berurutan, dan yang belum dijawab ditolak saat tab ditutup atau koneksi putus, roots (folder workspace); tools/resources/prompts, log traffic, ekspor konfigurasi. Auth statis lewat HTTP: Bearer, Basic, API key di header atau query, juga yang diwarisi dari folder/koleksi; request yang ditinggalkan dibatalkan di server (`notifications/cancelled`). Pesan dari server dibatasi 64 MiB (baris stdio, body JSON, event SSE); POST notifikasi/respons Streamable HTTP punya timeout dan yang macet menahan pesan berikutnya paling lama 3 dtk; `retry` SSE dipatuhi; stream respons yang putus dilanjutkan lewat GET dengan `Last-Event-ID`. Callback OAuth mengabaikan request dengan `state` lain (dijawab halaman galat, login tetap menunggu) dan halaman "selesai" baru tampil setelah token didapat. Diuji dengan server-everything resmi (stdio, streamable HTTP, SSE) dan contoh server OAuth dari SDK. |
| FR-X9 Request AI | ✅ | OpenAI, Anthropic, Google Gemini, dan endpoint kompatibel OpenAI (Ollama, LM Studio, vLLM): system prompt, percakapan multi-giliran, temperature/max tokens (default Anthropic per keluarga model: 4096 untuk Claude 3, 8192 untuk Claude 3.5, 16000 untuk 3.7 ke atas), body tambahan, streaming SSE dengan waktu token pertama dan jumlah token, daftar model, tools dari request MCP (loop pemanggilan tool), dan bisa dijalankan di Collection Runner/CLI. Respons dibatasi 64 MiB (body maupun seluruh stream satu putaran). Diuji dengan Ollama sungguhan (qwen3.5, termasuk pemanggilan tool MCP) dan mock untuk OpenAI/Anthropic/Gemini; belum dengan kunci API cloud sungguhan. |
| FR-X10 Webhook lokal | ✅ | Listener HTTP lokal (menu New → New webhook listener): mencatat metode, path, query, header, body; respons bisa diatur; replay event ke URL lain. URL publik butuh tunnel buatan pengguna (ngrok, cloudflared). |

### 7.2 Desain API dan dokumentasi

| ID | Status | Catatan |
| --- | --- | --- |
| FR-D1 | ✅ | Spec Hub (tab APIs): spesifikasi di `specs/` (OpenAPI 3/Swagger 2, AsyncAPI, Protobuf, GraphQL SDL, Smithy IDL 2.0 dan JSON AST), editor dengan validasi langsung (Smithy dengan baris:kolom), templat OpenAPI dan Smithy, dan pratinjau dokumentasi live di samping editor. Smithy: diuji dengan 31 berkas protocol-test resmi (dirakit menjadi satu model) dan model AWS SQS/Lambda/S3; model boleh terdiri dari banyak berkas IDL 2.0 dan/atau JSON AST (namespace dan `use` per berkas, referensi dan `apply` lintas berkas, penggabungan metadata, deteksi shape ganda dengan posisi kedua berkas). Di Spec Hub sebuah berkas divalidasi, dipratinjau, dan diimpor bersama berkas dari `sources`/`imports` milik `smithy-build.json` terdekat, atau tanpa itu bersama berkas Smithy se-folder yang terhubung lewat referensi shape (berkas yang terbuka di tab lain dengan perubahan belum disimpan dipakai seperti yang tampil di tab itu); CLI: `merpati import <folder|berkas...>` (folder dengan `smithy-build.json` memakai daftarnya). ID shape yang hanya beda huruf besar/kecil ditolak. Dependensi Maven (`maven.dependencies`) bisa dibaca dari jar yang sudah ada di repositori Maven lokal (`~/.m2/repository` atau `localRepository` di `settings.xml`; model lewat `META-INF/smithy/manifest`, dependensi transitif dari `.pom`) tanpa mengunduh, dan jar yang tidak ada dilaporkan dengan koordinatnya (`smithy::resolve_maven_dependencies`). Spec Hub dan `merpati import` merakit model dependensi itu setelah berkas proyek, menampilkan peringatannya di daftar masalah, dan membaca jar lagi hanya bila `smithy-build.json` berubah. |
| FR-D2 | ✅ | Spec (OpenAPI atau Smithy) → koleksi (buat baru atau perbarui: tambah/ubah request, pertahankan skrip dan tes) dan koleksi → OpenAPI, dipicu manual dari Spec Hub. |
| FR-D3 | ✅ | Dokumentasi dari koleksi: pratinjau di aplikasi, ekspor Markdown atau situs HTML statis (indeks, satu halaman per request, contoh kode dalam beberapa bahasa), `merpati docs`. Variabel rahasia disamarkan. |
| FR-D4 | ✅ | Flows lokal (`merpati-flows`): kanvas visual (geser blok, tarik port untuk menyambung, zoom/pan/fit, inspektor per jenis blok) dengan blok Start, Request (tersimpan atau inline), Condition, Loop, Parallel, Join, Script, AI, Delay, Set Variable, Template, Output, Log; cabang paralel berjalan bersamaan (request/AI/delay/skrip tumpang-tindih; tiap cabang mulai dari salinan state dan digabung menurut urutan cabang), Join menunggu cabang yang bisa mencapainya (ditentukan dari sambungan di kanvas, bukan dari waktu) lalu berjalan sekali dengan output gabungan (`results`, `items`, `sources`, `count`, `branches`); Join yang hanya dicapai sebagian cabang langsung berjalan saat cabang-cabang itu selesai, sementara cabang lain tetap berjalan; batas cabang serentak per flow (`settings.max_concurrent`, 0 = tanpa batas); tipe variabel (angka, boolean, ...) terbawa antarblok dan antarcabang; blok AI bisa streaming (jawaban tampil langsung di kanvas, inspektor, log run, dan `--verbose` di CLI); status per blok saat berjalan (beberapa blok sekaligus), validasi (termasuk Join tanpa sambungan, Parallel kurang dari 2 cabang, Join di luar cabang paralel), disimpan di `flows/*.flow.json`, dijalankan dari aplikasi atau `merpati flow run/list/validate`. |

### Fitur fase 3 lain dari tabel lingkup (bagian 4)

| Area | Status | Catatan |
| --- | --- | --- |
| Penangkapan lalu lintas lewat proxy | ✅ | Tab Capture (menu New): proxy HTTP/HTTPS lokal (`merpati-proxy`) dengan CA buatan sendiri yang kuncinya disimpan di Keychain, dekripsi HTTPS per host (HTTP/1.1 dan HTTP/2), tunnel untuk host yang dikecualikan, filter host/URL, daftar langsung, detail header/body (gzip/br/deflate didekode)/timing/WebSocket, simpan ke koleksi, ekspor HAR (pesan WebSocket dalam format `_webSocketMessages` Chrome). `OPTIONS http://host` diteruskan sebagai `OPTIONS *`; klien yang hanya menawarkan h2 saat HTTP/2 ke klien dimatikan di-tunnel dengan catatan jelas (tidak dianggap menolak CA); body yang masih mengalir ikut dihitung dalam batas memori total. Diuji dengan curl ke example.com/httpbin.org. Belum: HTTP/3, aplikasi dengan certificate pinning. |
| Paket npm dan JSR, Package Library lokal | ✅ | `pm.require('npm:paket@versi')` dan `pm.require('jsr:@scope/paket@versi')` lewat registry kompatibel esm.sh (bisa diganti di Pengaturan → Paket), di-cache untuk offline dan dikunci SHA-256 di `packages.lock.json`; Package Library di `packages/` (`pm.require('@team/nama')`). Diuji dengan lodash-es, uuid, dayjs, zod, date-fns, `jsr:@std/encoding`. `Intl` dan Web Streams tersedia lewat polyfill JavaScript yang baru dimuat saat pertama dipakai (skrip biasa tidak melambat): format angka/mata uang/tanggal (termasuk zona waktu IANA dari basis data tz sistem), Collator/`localeCompare`, PluralRules, RelativeTimeFormat, ListFormat, Segmenter, Locale, serta ReadableStream/WritableStream/TransformStream dan TextEncoderStream/TextDecoderStream. Diuji juga dengan `jsr:@std/assert`, `jsr:@std/fmt`, `jsr:@std/streams`, `jsr:@std/csv`, luxon, intl-messageformat, pretty-bytes dan eventsource-parser. Zona waktu bernama dibaca dari basis data tz sistem atau salinan bawaan (Windows, container ramping). Batasan: data lokal lengkap hanya bahasa Inggris (format angka juga de/es/fr/id/it/nl/pt; lokal lain memakai en-US dan `resolvedOptions().locale` menyebutkannya), tidak ada `fetch`/`Response`; modul Node bawaan ditolak dengan pesan jelas. |
| Simulasi kondisi jaringan di mock | ✅ | Lihat FR-M1–M4. |
| Fork lokal koleksi | ✅ | Lihat bagian 5.4. |
| Integrasi secret manager | ✅ | Lihat bagian 5.5. |

## Audit 24 Oktober 2026

Seluruh kode diaudit ulang per area (HTTP/auth, mesin skrip, runner/CLI/mock, core/penyimpanan/rahasia, impor-ekspor Postman, konverter lain, aplikasi, gRPC/Socket.IO/MQTT/AI, MCP/Flows, capture proxy/visualizer). Setiap bug dibuktikan dulu dengan test yang gagal, lalu diperbaiki, dan test-nya dipertahankan sebagai test regresi. Sekitar 160 bug diperbaiki; yang terpenting:

- **Rahasia**: auth pada example, snapshot fork, riwayat, kata sandi proxy dan frasa sandi sertifikat (kini di Keychain, berlaku di semua workspace), laporan run CLI, log MCP, serta `convert` yang mengabaikan `--include-secrets` tidak lagi menulis kredensial polos; isolasi antar-job skrip (skrip tidak bisa lagi membajak `__mp`/`pm` untuk membaca variabel job lain).
- **Kehilangan data**: penyimpanan tidak lagi menghapus berkas yang gagal dimuat, berkas bernama huruf besar di sistem berkas tak peka huruf (APFS), atau berkas salinan dengan id sama; isi body tidak hilang saat mode "none"; perubahan params tidak tertimpa Bulk Edit; tab request baru yang diedit tidak tertutup tanpa konfirmasi; event sesi lama tidak lagi memutus koneksi baru.
- **Crash**: emoji/aksara non-ASCII di editor skrip dan pencarian respons, CSS/HTML visualizer yang ekstrem, impor dengan referensi melingkar (Thunder, Insomnia, WSDL, Smithy, GraphQL), durasi CLI tak wajar, delay Flows sangat besar.
- **Kompatibilitas Postman/newman**: data iterasi berlebih memakai baris terakhir, `pm.variables.set` bertahan selama run, CSV/JSON dengan BOM dan spasi di header, `--folder` berisi nama request, `--timeout`, script beberapa event yang sama digabung, `disableBodyPruning`, URL dari bagian-bagiannya, cookie public suffix ditolak, SigV4 S3/presigned, Digest setelah redirect, `xml2Json`, `new Buffer()`, `replaceIn` pada objek, dan lain-lain.
- **Protokol**: filter unsubscribe/last will MQTT divalidasi, skema tool rekursif Gemini tidak meledak, token halaman daftar model di-encode, riwayat tool-call AI selalu JSON valid, MCP mewarisi auth koleksi dan bisa dibatalkan, Socket.IO mewarisi auth koleksi.

### Putaran kedua (24 Oktober 2026, sore)

Sisa batasan yang bisa diselesaikan di kode ditutup: opsi newman (`--bail failure|folder`, `-x`, `--export-cookie-jar`, `--ssl-client-*`, `--color`, `--disable-unicode`, `--export-collection`; `--cookie-jar` hanya dibaca), variabel bertipe Postman (number/boolean/any) di skrip dan ekspor, batas memori skrip di Windows, WPAD lewat DNS, failover PAC dan cache proxy rusak, auth Digest untuk tunnel proxy, channel binding NTLM lewat proxy, sertifikat klien untuk endpoint token OAuth, batas pembacaan 64 MiB dari server (SSE, MCP, Socket.IO, AI), timeout POST dan `Last-Event-ID` MCP, callback OAuth MCP yang tahan `state` asing, `max_tokens` Anthropic per model, `Any` gRPC yang tidak dikenal, batas cabang paralel dan Join lebih awal di Flows, user properties MQTT 5, celah impor cURL/HAR, contoh OpenAPI yang dibatasi dan tanpa kredensial, dependensi Maven Smithy dari repositori lokal, float/sticky/fixed/`line-height` visualizer, capture HAR dengan pesan WebSocket, Spec Hub membaca perubahan yang belum disimpan, antrean sampling MCP, log pesan tervirtualisasi, dan ±140 teks UI yang belum diterjemahkan.

## Batasan yang masih ada

- **Penandatanganan dan notarisasi macOS** butuh akun Apple Developer; tanpa itu DMG ditandatangani ad-hoc sehingga Gatekeeper di Mac lain meminta izin.
- **Belum diuji dengan layanan cloud sungguhan**: kunci API OpenAI/Anthropic/Gemini, AWS Secrets Manager, Azure Key Vault, 1Password, dan penyedia OAuth komersial untuk MCP diuji dengan mock (yang diuji sungguhan: Ollama, HashiCorp Vault, server-everything, socket.io, mosquitto, greeter gRPC).
- **Windows**: seluruh workspace (aplikasi, CLI, named pipe) lolos `cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets -- -D warnings` lewat kompilasi silang di Docker (MinGW), dan job CI Windows di GitHub Actions (`windows-latest`, MSVC) lolos clippy; workflow rilis membangun aplikasi dan CLI untuk Windows. Aplikasinya belum dicoba di mesin Windows sungguhan.
- **Proxy**: WPAD hanya lewat DNS (opsi DHCP 252 tidak dipakai) dan hanya bila diaktifkan di pengaturan macOS/Windows. Linux hanya membaca variabel lingkungan. Request HTTP biasa (reqwest) hanya mengirim autentikasi proxy Basic, sehingga lewat proxy yang mewajibkan Digest gagal dengan 407 (tunnel WebSocket/Socket.IO/MQTT/gRPC mendukung Digest); NTLM/Negotiate ke proxy tidak didukung. Mesin HTTP beralih ke entri PAC berikutnya hanya bila koneksi ke proxy ditolak atau tak terjangkau (bukan bila menggantung sampai timeout). MQTT lewat proxy memakai jembatan lokal di 127.0.0.1 (token acak per sesi) karena rumqttc tidak bisa memakai koneksi buatan sendiri. Sertifikat klien tidak dikirim ke proxy HTTPS itu sendiri.
- **Flows**: cabang berbagi satu task, jadi pekerjaan murni CPU (templat besar) tidak berjalan paralel, sedangkan I/O, timer dan skrip berjalan bersamaan; batas cabang serentak berlaku per flow, belum per blok Parallel; Join yang sudah berjalan lebih awal tidak dibatalkan oleh `break` dari cabang lain.
- **Visualizer**: subset HTML/CSS: grid, flexbox, float/clear dan positioning (relative/absolute/fixed/sticky, z-index) didukung dengan penyederhanaan (paragraf berisi gambar inline/inline-block di samping float tetap diletakkan seluruhnya di ruang samping float; sticky tidak berlaku untuk `tr`/`thead` dan mulai menempel satu frame setelah pertama digambar; `min()`/`max()` dengan lebih dari dua jenis suku ditolak); media query lebar mengikuti lebar panel, `min()`/`max()`/`clamp()` dan baris `fr` didukung; halaman dengan skrip dibuka di browser.
- **Smithy**: dependensi Maven hanya dibaca dari repositori Maven lokal (tidak diunduh; rentang versi, POM induk dan versi terkelola tidak diikuti) dan plugin/projection `smithy-build.json` tidak dipakai, jadi shape dari jar yang tidak ada (selain prelude dan trait AWS/alloy yang dikenal) dianggap tidak dikenal.
- **Skrip**: batas memori proses pekerja di Windows (working set lewat `GetProcessMemoryInfo`) baru dikompilasi silang, belum diuji di mesin Windows; job yang dihentikan paksa (batas memori, kode native yang macet) atau crash kehilangan hasil sementaranya (timeout/pembatalan di panggilan `pm.*` tetap menyimpannya). Isolasi antar-job di dalam satu pekerja tidak mencakup plugin `chai.use`, `moment.fn`/lokal moment dan registri skema tv4.
- **Kompatibilitas Postman/newman yang belum**: `--ssl-client-cert-list` memilih sertifikat per host dan port saja (path pada pola `matches` diabaikan; entri paling spesifik menang, bukan entri pertama yang cocok); `--folder` yang tidak ada selalu galat (kode 2) sehingga `--bail folder` tidak mengubah apa pun, dan galat pemakaian tetap keluar dengan 2 walau ada `-x`; impor cURL menyimpan isi berkas yang dibaca curl saat dijalankan (`--data-urlencode name@file`, `-F n=<file`, `-d @file` di antara data lain) sebagai placeholder `@file`/`<file` dengan peringatan, karena model body tidak punya rujukan berkas untuk itu.
- **NTLM** di balik proxy: sertifikat untuk channel binding dibaca lewat koneksi TLS kedua ke server, jadi server di belakang load balancer yang menyajikan sertifikat berbeda per koneksi bisa menolak binding-nya; tanpa proxy sertifikat diambil dari koneksi request itu sendiri.

## Non-fungsional (bagian 9)

| Kategori | Status |
| --- | --- |
| Performa, memori | ✅ lihat tabel benchmark |
| Penyimpanan data | ✅ JSON per berkas, rahasia di Keychain |
| Keamanan | ✅ TLS terverifikasi default, sandbox skrip, rahasia tidak di berkas koleksi/ekspor/riwayat/pengaturan/laporan run; tautan di visualizer, dokumentasi spec dan alur OAuth/MCP hanya membuka `http(s):`/`mailto:` (bukan berkas atau aplikasi lokal) |
| Privasi | ✅ tanpa akun dan telemetri; satu-satunya koneksi yang tidak dipicu pengguna adalah pemeriksaan pembaruan saat aplikasi dibuka (mengambil rilis terbaru dari GitHub Releases dan hanya mengirim versi aplikasi sebagai User-Agent; bisa dimatikan di Pengaturan › Umum, tidak berjalan di mode otomasi) |
| Keandalan | ✅ penulisan atomik; sesi tab (termasuk tab yang belum disimpan, protokol, spec, flow) ditulis ~2 dtk setelah ada perubahan sehingga pulih setelah penutupan paksa (diuji dengan `kill -9`); kredensial literal di tab yang belum disimpan tidak ditulis ke berkas sesi; menutup tab request baru yang sudah diedit atau spec yang belum disimpan meminta konfirmasi (Simpan…/Jangan simpan/Batal; Enter = Simpan…, Esc = Batal); galat request/skrip tidak menjatuhkan aplikasi |
| Lokalisasi | ✅ Inggris dan Indonesia; teks UI lewat `tr()`/`trf()` dengan tabel `crates/merpati-app/i18n/id.tsv` (±1.670 entri), termasuk log koneksi tab protokol, label blok/port Flows dan ringkasan validasi Spec Hub. Tes memastikan setiap kunci punya terjemahan; `scripts/i18n-scan.py` melaporkan literal yang belum lewat sistem terjemahan (juga panggilan beberapa baris, `format!` langsung dan baris log protokol; sisa ±25 adalah contoh dan nama teknis seperti `*.example.com`, `Socket.IO v2`). |
| Tampilan | ✅ sistem desain sendiri (`docs/design.md`): font Inter dan JetBrains Mono serta ikon Phosphor dibundel (subset, ±0,6 MB, lisensi OFL/MIT ikut), token warna terang/gelap dengan aksen merah koral dari ikon aplikasi, komponen bersama (`kit.rs`). Semua layar didesain ulang dan diperiksa lewat tangkapan layar dalam mode terang, gelap dan jendela sempit 900×640: shell (bar atas, rel navigasi, tab ala browser, bar status), beranda, request HTTP/GraphQL, respons, autentikasi, environment, runner (fungsional dan performa), semua tab protokol, mock, capture, Flows, Spec Hub, dialog (pengaturan dua panel, palet perintah, impor, simpan, cookie, kode), konsol, vault dan sinkronisasi fork. Tidak ada emoji sebagai ikon. |
| Aksesibilitas | ✅ navigasi keyboard dan AccessKit; semua tombol ikon, tab, baris sidebar dan kolom isian punya nama untuk pembaca layar, tab terbaca sebagai tab terpilih. Diaudit dengan `scripts/ax-audit.swift` (membaca pohon aksesibilitas yang dibaca VoiceOver): 0 elemen interaktif tanpa nama. Belum dicoba langsung oleh pengguna VoiceOver. |
| Distribusi | 🟡 `scripts/package-macos.sh` membuat `Merpati.app` (universal arm64+x86_64, ikon, Info.plist), tanda tangan ad-hoc atau Developer ID, notarisasi opsional, DMG, tarball CLI, dan cask Homebrew; workflow rilis GitHub untuk macOS/Linux/Windows. Sudah dibangun dan dijalankan di Mac ini: DMG universal 32,8 MB (arm64 saja 14,7 MB), kedua slice terbuka (x86_64 lewat Rosetta), tanda tangan valid. Penandatanganan Developer ID dan notarisasi butuh akun Apple Developer; cask belum diterbitkan di tap. Pembaruan: saat dibuka aplikasi memeriksa rilis terbaru (atau feed lain lewat `settings.updates.feed_url`/`MERPATI_UPDATE_URL`) dan menampilkan pop-up berisi catatan rilis dengan pilihan Unduh dan pasang / Ingatkan nanti / Lewati versi ini; di macOS tombol **Pasang dan mulai ulang** memeriksa feed sekali lagi, mengunduh arsip aplikasi `Merpati-<versi>-macos-<arsitektur>.zip`, memverifikasi SHA-256 (digest yang diterbitkan GitHub atau `SHA256SUMS-*.txt`), bundle id, versi dan tanda tangan kode (Team ID harus sama), mengganti `Merpati.app` (yang lama dikembalikan bila gagal; meminta kata sandi admin bila folder tidak bisa ditulis) lalu membukanya kembali. Di luar bundle aplikasi, Linux dan Windows, installer untuk OS dan arsitektur ini diunduh ke folder Unduhan, diverifikasi, lalu dibuka. Unduhan tanpa checksum ditolak; feed dan unduhan wajib HTTPS (kecuali localhost untuk pengujian). Juga ada installer satu baris `scripts/install.sh` dengan pemeriksaan yang sama. |
| Kualitas kode | ✅ 1.079 uji otomatis lulus di macOS, 0 gagal; CI GitHub Actions menjalankan fmt, clippy `-D warnings` dan seluruh tes workspace di macOS dan Linux serta clippy di Windows, semuanya hijau pada commit rilis 0.1.0; 22 uji yang butuh server sungguhan atau jaringan (socket.io, Ollama, HashiCorp Vault, registry paket, berkas Smithy resmi) di-`#[ignore]` dan dijalankan manual; clippy macOS bersih, CI macOS + Linux (+ kompilasi Windows), benchmark |
