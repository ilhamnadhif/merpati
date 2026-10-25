# Merpati

**Klien API yang ringan, ditulis sepenuhnya dengan Rust.**

Merpati adalah alternatif native untuk Postman: satu binary kecil, tanpa Electron, tanpa Node.js,
tanpa akun. Frame pertamanya tampil dalam sekitar 40 ms, CPU 0 % saat diam, memakai kurang dari 90 MB
memori, dan respons 100 MB dialirkan ke disk, bukan ke memori. Koleksi berupa berkas JSON biasa di
folder yang bisa Anda commit ke Git, dan koleksi, environment, serta skrip `pm.*` dari Postman bisa
diimpor dan dijalankan apa adanya untuk pemakaian umum.

[English](README.md)

![Merpati dalam bahasa Indonesia: koleksi, request, dan hasil tesnya](docs/screenshots/indonesian.png)

## Sorotan

- **HTTP, GraphQL, gRPC, WebSocket, Socket.IO, MQTT, SOAP, MCP, dan request ke model AI** dalam satu
  aplikasi, masing-masing dengan tab, log pesan, dan skripnya sendiri.
- **Kompatibel dengan Postman**: impor dan ekspor koleksi v2.0/v2.1, environment, dan globals;
  skrip `pm.*` berjalan di mesin JavaScript tertanam dengan Chai, lodash, moment, crypto-js, dan tv4
  yang asli (korpus 50 skrip Postman realistis lulus 50/50).
- **Workspace ramah Git**: satu berkas rapi per request, urutan stabil, dan rahasia tidak pernah
  ditulis ke workspace; rahasia disimpan di Keychain.
- **Collection Runner dan CLI** dengan berkas data, laporan JUnit/HTML/JSON, dan **opsi yang
  kompatibel dengan newman**, jadi `merpati run` bisa menggantikan `newman run` di CI. Ada juga
  performance test dengan virtual user.
- **Flows**: kanvas visual yang merangkai request, kondisi, loop, cabang paralel, skrip, dan
  panggilan AI, lalu dijalankan dari aplikasi atau CLI.
- **Spec Hub** untuk berkas OpenAPI, AsyncAPI, Protobuf, GraphQL, dan Smithy dengan validasi
  langsung, pratinjau dokumentasi langsung, dan sinkronisasi spec ⇄ koleksi.
- **Mock server, listener webhook, dan proxy penangkap lalu lintas**, semuanya lokal.
- Dua belas jenis autentikasi, dari Basic dan OAuth 2.0 sampai AWS SigV4, NTLM, dan Akamai
  EdgeGrid.
- Antarmuka bahasa Inggris dan Indonesia, tema terang dan gelap, ramah keyboard dan pembaca layar,
  tanpa telemetri.
- **Pembaruan** dari rilis GitHub; di macOS cukup satu klik untuk memasang versi baru dan memulai
  ulang.

## Instalasi

Aplikasi berjalan di macOS 12 Monterey atau lebih baru (Apple Silicon dan Intel, satu build
universal), Linux x86_64, dan Windows x86_64. macOS adalah platform utama; lihat
[Batasan](#batasan).

### Installer satu baris (macOS, disarankan)

```sh
curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/merpati/master/scripts/install.sh | bash
```

Skrip ini mengunduh rilis terbaru, memeriksanya (SHA-256 yang diterbitkan GitHub, bundle id, versi,
dan tanda tangan kode), menutup Merpati yang sedang berjalan, memasang `Merpati.app` ke
`/Applications`, lalu membukanya. Kalau ada yang gagal, salinan lama tetap di tempatnya. Jalankan
lagi untuk memperbarui. Untuk versi tertentu, berikan versinya sebagai argumen:
`… | bash -s -- v0.1.0`. Untuk memasang di folder lain, atur `MERPATI_APP_DIR` untuk bash:
`… | MERPATI_APP_DIR=~/Applications bash`.

### Disk image

Unduh **`Merpati-<versi>-universal.dmg`** dari
[rilis terbaru](https://github.com/ilhamnadhif/merpati/releases/latest), buka, lalu seret
**Merpati** ke **Applications**.

> **Muncul “Merpati” Not Opened / “Apple could not verify…”?** Build rilis belum dinotarisasi Apple
> (hanya ditandatangani ad hoc), jadi macOS memblokir aplikasi yang diunduh lewat browser. Sejak
> macOS 15, cara lama klik kanan → Open sudah tidak bisa. Pilih salah satu cara berikut:
>
> - **Pakai installer satu baris** di atas. Berkas yang diunduh lewat `curl` tidak diblokir.
> - **Izinkan sekali lewat Settings**: klik **Done** di peringatannya, buka **System Settings →
>   Privacy & Security**, gulir ke bagian **Security**, lalu klik **Open Anyway** di samping
>   “Merpati was blocked…”. Konfirmasi dengan kata sandi, lalu buka Merpati lagi.
> - **Atau lewat Terminal**, setelah aplikasi diseret ke Applications:
>   `xattr -dr com.apple.quarantine /Applications/Merpati.app`

### Homebrew

Setiap rilis menyertakan berkas cask `merpati.rb`:

```sh
curl -fsSLO https://github.com/ilhamnadhif/merpati/releases/latest/download/merpati.rb
brew install --cask ./merpati.rb
xattr -dr com.apple.quarantine /Applications/Merpati.app   # selama rilis belum dinotarisasi
```

Cask ini juga menautkan `merpati-app` ke folder `bin` milik Homebrew.

### Linux dan Windows

Unduh dari [rilis terbaru](https://github.com/ilhamnadhif/merpati/releases/latest):

- **`merpati-<versi>-linux-x86_64.tar.gz`**: aplikasi (`merpati-app`), CLI (`merpati`), berkas
  `.desktop`, dan ikon. Ekstrak lalu jalankan `./merpati-app`, atau salin binary-nya ke folder di
  `PATH` (misalnya `~/.local/bin`) dan berkas `.desktop` ke `~/.local/share/applications`.
- **`merpati-<versi>-windows-x86_64.zip`**: `merpati-app.exe` dan `merpati.exe`. Ekstrak lalu
  jalankan `merpati-app.exe`.

Keduanya dibangun oleh workflow rilis di GitHub Actions; setiap rilis juga menyertakan
`SHA256SUMS-linux.txt` dan `SHA256SUMS-windows.txt` untuk memeriksanya.

### Hanya command line

CLI juga berupa satu binary. Untuk macOS, ambil `merpati-cli-<versi>-macos-universal.tar.gz` dari
rilis (arsip Linux dan Windows di atas juga berisi CLI):

```sh
curl -fsSL https://github.com/ilhamnadhif/merpati/releases/download/v0.1.0/merpati-cli-0.1.0-macos-universal.tar.gz | tar -xz
sudo mv merpati /usr/local/bin/
merpati --version
```

Atau bangun dengan Cargo (Rust 1.88 atau lebih baru), atau pakai image Docker:

```sh
cargo install --locked --git https://github.com/ilhamnadhif/merpati merpati-cli

docker build -f docker/Dockerfile -t merpati .
docker run --rm -v "$PWD:/work" merpati run collection.json -e env.json -r cli,junit
```

### Pembaruan

Saat dibuka, Merpati menanyakan rilis terbaru ke GitHub. Permintaan itu hanya mengirim versi
aplikasi sebagai User-Agent; matikan di **Settings › General › Updates** (Pengaturan › Umum ›
Pembaruan), tempat **Check now** juga bisa langsung memeriksa. Kalau ada versi yang lebih baru,
sebuah jendela menampilkan catatan rilisnya dengan pilihan untuk memasangnya, diingatkan nanti,
atau melewati versi itu.

- **Di macOS**, **Install and restart** mengunduh arsip aplikasi
  (`Merpati-<versi>-macos-universal.zip`), memverifikasinya (SHA-256 yang diterbitkan GitHub,
  bundle id, versi, dan tanda tangan kode), mengganti `Merpati.app`, lalu membuka versi baru.
- **Di Linux dan Windows**, Merpati mengunduh arsip untuk sistem Anda ke folder Unduhan,
  memeriksanya dengan `SHA256SUMS-*.txt` dari rilis (unduhan tanpa checksum ditolak), lalu
  membukanya; penggantian berkasnya Anda lakukan sendiri.

Installer satu baris di atas juga bisa memperbarui.

### Uninstall

```sh
rm -rf /Applications/Merpati.app
rm -rf ~/Library/Application\ Support/Merpati   # pengaturan, workspace bawaan, cache paket
```

Workspace yang Anda buat di tempat lain tetap di tempatnya. Rahasia yang tersimpan adalah item
Keychain dengan nama service `dev.merpati.workspace`; hapus lewat Keychain Access.

## Mulai

1. Buka Merpati. Workspace terakhir Anda dibuka, atau **My Workspace** dibuat di folder
   pengaturannya. Pakai menu workspace (kiri atas) untuk membuat atau membuka workspace di mana
   saja, misalnya di dalam repositori Git.
2. Klik **Import** (Impor) untuk memasukkan koleksi Postman, berkas OpenAPI, perintah cURL, berkas
   HAR, dan lainnya, atau **New** (Baru) untuk request protokol apa pun.
3. Pilih environment (`⌘E`), ketik URL, lalu tekan `⌘↩`. `{{variabel}}` disorot, dan mengarahkan
   pointer ke variabel menampilkan nilainya.
4. Klik kanan koleksi atau folder lalu pilih **Run collection** (Jalankan koleksi) atau **Run
   folder** untuk menjalankan request dan tesnya, atau jalankan koleksi yang sama di terminal dengan
   `merpati run`.

## Fitur

### Request dan respons

- Semua metode HTTP, juga metode kustom. Parameter query dan variabel path `:id` tersinkron dengan
  URL; header punya autocomplete, dan header otomatis bisa dilihat serta dimatikan.
- Body: none, form-data (teks dan berkas), x-www-form-urlencoded, raw (JSON, XML, teks, JavaScript,
  HTML), binary, dan GraphQL. **Beautify** tidak merusak `{{variabel}}`.
- Per request: redirect, verifikasi TLS, encoding URL, timeout, HTTP/1.1 atau HTTP/2.
- Respons sebagai Pretty, Raw, Preview, atau Tree, dengan header, cookie, hasil tes, dan waktu
  (TTFB dan total), pencarian, simpan ke berkas atau sebagai example. Respons besar dialirkan ke
  berkas sementara dan ditampilkan secara virtual, jadi respons 100 MB berisi setengah juta baris
  tidak membuat antarmuka macet.
- Templat `pm.visualizer` dirender di dalam aplikasi (Handlebars, HTML, dan subset CSS termasuk
  grid, flexbox, float, dan positioning).
- Cookie jar per domain (RFC 6265) yang bisa dilihat dan diubah, sertifikat CA tambahan, sertifikat
  klien per host (PEM atau PKCS#12), proxy (HTTP, HTTPS, SOCKS4/5, pengaturan sistem, skrip PAC, dan
  WPAD), Unix socket (`http://unix:/var/run/docker.sock:/info`), dan named pipe Windows.
- Riwayat yang bisa dicari, dengan nilai rahasia disamarkan.
- Cuplikan kode dalam 18 varian: cURL, HTTP mentah, HTTPie, wget, fetch, axios, Python, Go, Rust,
  Java (dua), Kotlin, C#, PHP, Swift, Ruby, Dart, dan PowerShell.

### Autentikasi

API key, Basic, Bearer, Digest, JWT, OAuth 1.0, OAuth 2.0 (authorization code dengan PKCE, implicit,
password, client credentials, refresh otomatis), Hawk, AWS Signature v4, NTLM (dengan channel
binding), Akamai EdgeGrid, dan Atlassian ASAP, diperiksa dengan test vector resmi bila tersedia.
Autentikasi diwariskan dari koleksi dan folder; token OAuth disimpan di Keychain.

### Koleksi, environment, dan variabel

- Folder bertingkat, drag and drop (juga antarkoleksi), duplikasi, ganti nama; autentikasi, skrip,
  variabel, dan deskripsi di setiap koleksi dan folder; pencarian berdasarkan nama, URL, atau isi.
- **Impor**: koleksi Postman (v2.0, v2.1), environment dan globals, cURL, OpenAPI 3 dan Swagger 2,
  Insomnia, HAR, Hoppscotch, Thunder Client, WSDL (SOAP), dan Smithy. **Ekspor**: Postman v2.1
  (impor lalu ekspor menghasilkan berkas yang sama) dan OpenAPI.
- Lima cakupan variabel dengan prioritas seperti Postman, nilai awal (di workspace) dan nilai saat
  ini (hanya lokal), tipe secret yang disamarkan dan tidak pernah ditulis ke berkas, 118 variabel
  dinamis seperti `{{$guid}}` dan `{{$randomEmail}}`, serta vault: `{{vault:nama}}` membaca nilai
  dari Keychain atau dari HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, atau 1Password.
- **Fork lokal**: fork sebuah koleksi, tarik perubahan dari induknya atau gabungkan kembali, dengan
  diff tiga arah dan pilihan untuk setiap konflik.

### Skrip dan tes

- Skrip pre-request dan post-response di koleksi, folder, dan request, dengan urutan Postman,
  lengkap dengan `pm.test`/`pm.expect`, `pm.sendRequest` (callback atau `await`), `pm.environment`,
  `pm.collectionVariables`, dan bagian `pm` lainnya, serta `tests[]` dan `postman.*` versi lama.
- Skrip untuk protokol lain juga: sebelum panggilan gRPC dan di setiap pesan, di setiap pesan
  WebSocket, Socket.IO, dan MQTT, di sekitar panggilan MCP dan request AI.
- Paket npm dan JSR lewat `pm.require('npm:zod@3')`, di-cache untuk pemakaian offline dan dikunci
  dengan SHA-256 di berkas lock; Package Library di workspace untuk modul Anda sendiri; `Intl` dan
  Web Streams.
- Skrip berjalan di proses pekerja terpisah dengan batas waktu dan batas memori, jadi skrip yang
  lepas kendali tidak bisa membekukan atau menjatuhkan aplikasi.
- Konsol berisi log skrip, baris jaringan, dan galat; snippet dan autocomplete `pm.`; jalankan
  ulang tes tanpa mengirim ulang request.

Detail: [docs/scripting.md](docs/scripting.md) (bahasa Inggris).

### Collection Runner dan performance test

![Collection Runner: 10 request, 20 tes lulus](docs/screenshots/runner.png)

- Jalankan koleksi atau folder dengan environment, iterasi, jeda, berkas data CSV atau JSON,
  `pm.execution.setNextRequest`/`skipRequest`, urutan kustom, dan simulasi gangguan jaringan.
- Hasil per request dan per iterasi; ekspor sebagai JSON, JUnit, atau HTML. Run tetap berjalan di
  latar belakang, bisa dibatalkan, dan memorinya datar sampai ribuan iterasi.
- Semua protokol bisa dijalankan di Runner: panggilan gRPC, AI, dan MCP, serta WebSocket, Socket.IO,
  dan MQTT sebagai sesi berskrip (connect, kirim pesan tersimpan, berhenti setelah N pesan, sebuah
  event, atau saat hening).
- Performance test dengan virtual user dan ramp-up: persentil latensi (p50 sampai p99), throughput,
  dan galat, di aplikasi atau lewat `merpati perf`.

### Protokol

![Sesi Socket.IO: terhubung, event tick, dan pesan yang dipantulkan di log](docs/screenshots/socketio.png)

- **GraphQL**: query, mutation, dan subscription (graphql-transport-ws), variabel, introspeksi,
  penjelajah skema, dan autocomplete di editor.
- **gRPC**: skema dari berkas `.proto` (dikompilasi di dalam proses, dengan import path) atau server
  reflection; panggilan unary, client streaming, server streaming, dan dua arah; metadata,
  deadline, TLS, dan mTLS.
- **WebSocket**: header, subprotocol, pesan teks dan biner, pesan tersimpan, log berstempel waktu.
- **Socket.IO** v2 sampai v4: long-polling dengan upgrade atau WebSocket saja, namespace, payload
  `auth`, event, acknowledgement, dan lampiran biner.
- **MQTT** 3.1.1 dan 5 lewat TCP, TLS, WebSocket, dan WebSocket aman: subscription per QoS,
  publish dengan QoS dan retain, last will, user properties MQTT 5.
- **SOAP**: request dengan envelope dan SOAPAction dari WSDL.
- **Server MCP** (Model Context Protocol) lewat stdio, Streamable HTTP, atau transport SSE lama:
  tools, resources, dan prompts, OAuth, sampling, elicitation, roots, dan ekspor konfigurasi.
- **Request AI** ke OpenAI, Anthropic, Google Gemini, dan server yang kompatibel dengan OpenAI
  (Ollama, LM Studio, vLLM): streaming dengan waktu token pertama dan jumlah token, percakapan
  beberapa giliran, dan tools dari request MCP Anda.

Log pesan tervirtualisasi, jadi puluhan ribu pesan tetap lancar.

### Flows

![Flow dengan tiga request paralel, sebuah join, dan ringkasan AI](docs/screenshots/flows.png)

Flows merangkai blok di kanvas: Start, Request (tersimpan atau inline), Condition, Loop, Parallel,
Join, Script, AI, Delay, Set Variable, Template, Output, dan Log. Cabang paralel benar-benar berjalan
bersamaan dan Join menunggu cabang yang bisa mencapainya. Setiap blok menampilkan status dan
waktunya selama flow berjalan, jawaban AI mengalir langsung di kanvas, dan masalah ditandai sebelum
Anda menjalankannya. Flow disimpan sebagai `flows/*.flow.json` di workspace dan bisa dijalankan dari
CLI dengan `merpati flow run`.

### Desain API: Spec Hub

![Spec Hub: berkas OpenAPI di samping dokumentasinya yang langsung diperbarui](docs/screenshots/spec-hub.png)

- Spec disimpan di folder `specs/` dalam workspace: OpenAPI 3 dan Swagger 2, AsyncAPI, Protobuf,
  GraphQL SDL, dan Smithy (IDL 2.0 dan JSON AST, model dari banyak berkas, `smithy-build.json`).
- Editor dengan validasi langsung dan daftar masalah, templat, serta pratinjau dokumentasi di
  samping editor.
- **Generate collection** (Buat koleksi) dari spec OpenAPI atau Smithy, atau perbarui koleksi yang
  sudah ada (request ditambah dan diubah, skrip dan tes Anda tetap); ekspor koleksi sebagai OpenAPI.
- Dokumentasi koleksi sebagai Markdown atau situs HTML statis dengan contoh kode, dari aplikasi
  atau `merpati docs`.

### Mock server, webhook, dan penangkapan lalu lintas

- **Mock server** dari example di koleksi, dengan pencocokan gaya Postman, templat dinamis,
  simulasi jaringan (latensi, jitter, bandwidth, preset 4G/3G/satelit), dan simulasi kegagalan
  (status galat, koneksi putus, timeout). Satu klik mengarahkan `{{baseUrl}}` ke mock itu.
- **Listener webhook**: endpoint HTTP lokal yang mencatat setiap request (metode, path, header,
  body), menjawab dengan respons yang Anda atur, dan bisa me-replay event ke URL lain.
- **Proxy penangkap**: proxy HTTP/HTTPS dengan CA lokal sendiri (kuncinya di Keychain) untuk
  merekam lalu lintas aplikasi lain, mendekripsi HTTPS per host, menyaring, menyimpan request ke
  koleksi, dan mengekspor HAR (termasuk pesan WebSocket).

### Aplikasinya

![Tema gelap](docs/screenshots/dark.png)

- Sistem desain sendiri: Inter dan JetBrains Mono, ikon Phosphor, tema terang dan gelap dengan
  aksen merah koral, tab ala browser, palet perintah (`⇧⌘P`), dan pencarian cepat (`⌘K`).
- Tab yang terbuka, termasuk yang belum disimpan, dipulihkan setelah aplikasi dibuka ulang atau
  crash. Menutup request baru atau spec yang sudah diubah selalu ditanyakan dulu.
- Bahasa Inggris dan Indonesia (Settings › General › Language).
- Aksesibel: navigasi keyboard, dan setiap tombol, tab, baris sidebar, serta kolom isian punya nama
  untuk pembaca layar (lewat AccessKit).

![Antarmuka bahasa Inggris: request dan responsnya](docs/screenshots/hero.png)

## Pintasan keyboard

| Aksi | Pintasan |
| --- | --- |
| Kirim request | `⌘↩` |
| Request baru / tab baru / tutup tab | `⌘N` / `⌘T` / `⌘W` |
| Tab berikutnya / sebelumnya | `⇧⌘]` / `⇧⌘[` |
| Simpan | `⌘S` |
| Cari request | `⌘K` |
| Palet perintah | `⇧⌘P` |
| Ganti environment | `⌘E` |
| Tampilkan/sembunyikan sidebar | `⌘B` |
| Konsol | `⌥⌘C` |
| Pengaturan | `⌘,` |
| Terima saran variabel | `Tab` |

`⌘` menjadi Ctrl di Windows dan Linux.

## Command line

```text
merpati run       Jalankan koleksi (opsi kompatibel newman)
merpati schedule  Jalankan koleksi berulang (--every 5m), hasil tiap run disimpan
merpati mock      Mulai mock server dari example sebuah koleksi
merpati import    Impor Postman, OpenAPI, Insomnia, HAR, cURL, … ke workspace
merpati export    Ekspor koleksi, environment, atau globals
merpati convert   Ubah berkas yang bisa diimpor menjadi Postman atau OpenAPI
merpati validate  Periksa berkas atau workspace dari galat dan variabel yang tidak terdefinisi
merpati codegen   Buat cuplikan kode untuk sebuah request
merpati perf      Uji beban request dengan virtual user
merpati docs      Buat dokumentasi (situs HTML statis atau Markdown)
merpati flow      Jalankan, daftarkan, dan validasi Flows
merpati init      Buat workspace kosong
```

`merpati run` menerima berkas koleksi Postman, workspace Merpati, atau folder koleksi, dengan opsi
yang Anda kenal dari newman: `-e`, `-g`, `-d`, `-n`, `--folder`, `--env-var`, `--global-var`,
`--delay-request`, `--timeout`, `--timeout-request`, `--timeout-script`, `-k`, `--bail`, `-x`,
`-r cli,json,junit,html` dengan `--reporter-*-export`, `--export-environment`, `--export-globals`,
`--export-collection`, `--cookie-jar`, `--export-cookie-jar`, `--ssl-client-cert`/`-key`/`-passphrase`,
`--ssl-client-cert-list`, `--ssl-extra-ca-certs`, `--color`, `--disable-unicode`, `--silent`, dan
`--verbose`. Kode keluarnya 0 bila semua lulus, 1 bila ada request, tes, atau skrip yang gagal, dan 2
untuk galat pemakaian atau pemuatan, jadi bisa dipakai sebagai gerbang di CI. Kredensial (header
Authorization dan Cookie, API key, kata sandi di URL, token di query string) disamarkan di laporan
dan keluaran terminal kecuali Anda memberikan `--show-secrets`.

```sh
merpati run postman_collection.json -e staging.postman_environment.json -d users.csv \
  -r cli,junit --reporter-junit-export report.xml
merpati init my-api && merpati import postman_collection.json -w my-api
merpati run my-api -e Staging --folder "Smoke tests" --bail
merpati mock my-api --port 3000 --network slow-3g --fail-rate 0.1 --fail-with timeout
merpati perf my-api --vus 20 --duration 30s
merpati flow run my-api "Catalog summary" -e Staging
merpati docs my-api -o site/ --languages curl,python    # atau --markdown -o API.md
```

`merpati <perintah> --help` menampilkan semua opsi.

## GitHub Action

Repositori ini juga sebuah GitHub Action ([`action.yml`](action.yml)) yang membangun CLI dan
menjalankan koleksi, lalu menulis laporan JUnit:

```yaml
- uses: actions/checkout@v4
- uses: ilhamnadhif/merpati@v0.1.0
  with:
    collection: api/                 # berkas koleksi Postman, workspace, atau folder koleksi
    environment: Staging             # berkas environment Postman atau nama environment di workspace
    reporters: cli,junit
    args: --bail --env-var baseUrl=https://staging.example.com
- uses: actions/upload-artifact@v4
  if: always()
  with:
    name: api-tests
    path: merpati-reports/junit.xml
```

Input lainnya: `collection-name`, `iteration-data`, `junit-export`, dan `version` (ref Git Merpati
yang dibangun). Action ini mengompilasi CLI dengan Cargo dan menyimpan build-nya di cache antar-run.
Action ini belum pernah dicoba di runner milik GitHub, jadi laporkan bila ada masalah.

## Format workspace

```text
my-api/
  merpati.json                      manifest
  .gitignore                        mengabaikan .merpati/
  globals.json
  environments/staging.json         hanya nilai awal; rahasia dikosongkan
  collections/bookstore-api/
    collection.json                 auth, skrip, variabel, urutan isi
    books/folder.json
    books/list-books.request.json   satu berkas rapi per request
  flows/catalog-summary.flow.json
  specs/bookstore.yaml
  packages/                         modul skrip Anda sendiri
  .merpati/                         hanya lokal: nilai saat ini, cookie, riwayat, tab terbuka
```

## Privasi dan keamanan

- Tanpa akun dan tanpa telemetri. Merpati hanya terhubung ke server yang Anda kirimi request dan,
  bila pemeriksaan pembaruan tetap menyala, ke GitHub sekali saat dibuka (hanya dengan versi
  aplikasi sebagai User-Agent).
- Variabel bertipe secret, isian autentikasi yang sensitif (kata sandi, token, client secret,
  kunci privat), token OAuth, kata sandi proxy, dan frasa sandi sertifikat disimpan di Keychain
  macOS (satu item per workspace), tidak pernah di berkas workspace, ekspor, riwayat, atau laporan
  run. Di sistem tanpa keychain (CI, container) rahasia disimpan di `.merpati/secrets.json`, yang
  dijauhkan dari Git oleh `.gitignore` workspace.
- Sertifikat TLS diverifikasi secara bawaan. Skrip berjalan di sandbox tanpa akses berkas atau
  jaringan kecuali lewat `pm`, di proses terpisah dengan batas waktu dan memori.
- Tautan di keluaran visualizer, dokumentasi spec, dan alur OAuth/MCP hanya membuka `http(s):` dan
  `mailto:`, tidak pernah berkas atau aplikasi lokal.

| Apa | Lokasi (macOS) |
| --- | --- |
| Pengaturan | `~/Library/Application Support/Merpati/settings.json` |
| Workspace bawaan | `~/Library/Application Support/Merpati/workspaces/My Workspace` |
| Cache paket npm/JSR | `~/Library/Application Support/Merpati/package-cache` |
| Nilai saat ini, cookie, riwayat, tab terbuka | `.merpati/` di dalam setiap workspace |
| Rahasia | Keychain macOS, service `dev.merpati.workspace` |

Di Linux folder pengaturannya adalah folder konfigurasi XDG, di Windows `%APPDATA%`.

## Performa

Diukur dengan `scripts/bench.sh` di MacBook Apple Silicon (macOS 14, layar Retina, jendela
1280×820) dengan build `dist`. Memori adalah physical footprint seperti yang ditampilkan Activity
Monitor.

| | Target | Hasil |
| --- | --- | --- |
| Memori, diam, satu workspace | < 100 MB | 72–88 MB |
| Memori, lima tab request aktif | < 150 MB | 89–93 MB, ditambah 33 MB untuk proses pekerja skrip |
| Start dingin sampai frame pertama | < 1 detik | ≈ 40 ms |
| CPU saat diam | ≈ 0 % | 0,01 detik CPU dalam 15 detik |
| Binary aplikasi (arm64) | < 40 MB | 34,9 MB (33,3 MiB) |
| Binary CLI (arm64) | | 20,9 MB |
| Disk image | | 17,6 MB khusus Apple Silicon, ≈ 33 MB universal |
| Workspace dengan 10.000 request | < 1 detik | 276–315 ms sampai frame pertama |
| Respons 100 MB | antarmuka tetap responsif | diterima dalam 73 ms (lokal), 550.741 baris tampil virtual, 82–99 MB |
| Runner, 1.000 iterasi | tanpa pertumbuhan linear | RSS puncak CLI ≈ 21 MB pada 100, 1.000, dan 3.000 iterasi |
| Overhead per request (CLI, dengan tes JS) | < 5 ms | 1,7–2,0 ms |
| Impor 10.000 request | | 1,5–1,9 detik |

Antarmuka hanya digambar ulang saat ada perubahan, karena itu CPU saat diam nol. Angka lain dan
cara pengukurannya: [docs/STATUS.md](docs/STATUS.md).

## Membangun dari source

Butuh toolchain Rust stable (1.88 atau lebih baru, lewat [rustup](https://rustup.rs)); di macOS
Xcode Command Line Tools, di Linux paket pengembangan `libxkbcommon`, `wayland`, `gtk3`, dan `dbus`.

```sh
git clone https://github.com/ilhamnadhif/merpati.git && cd merpati
cargo run --release -p merpati-app            # aplikasi desktop
cargo run --release -p merpati-cli -- --help  # CLI
cargo build --profile dist -p merpati-app     # binary terkecil, seperti yang dirilis (fat LTO, opt-level s)
scripts/package-macos.sh                      # Merpati.app universal, DMG, zip, tarball CLI, cask Homebrew
scripts/package-macos.sh --native             # hanya arsitektur Mac ini (lebih cepat)
```

Penandatanganan dan notarisasi butuh akun Apple Developer: atur `MERPATI_SIGN_IDENTITY` (identitas
Developer ID Application) dan `MERPATI_NOTARY_PROFILE` (dibuat sekali dengan
`xcrun notarytool store-credentials`) sebelum menjalankan `scripts/package-macos.sh`. Tanpa itu
aplikasi ditandatangani ad hoc. Push tag `v*` menjalankan workflow rilis yang membangun macOS,
Linux, dan Windows lalu menerbitkan berkasnya beserta checksum.

## Struktur proyek

| Crate | Fungsi |
| --- | --- |
| `merpati-core` | Model data, variabel (5 cakupan, variabel dinamis, vault), format workspace, rahasia, riwayat, impor/ekspor Postman |
| `merpati-http` | Mesin HTTP di atas reqwest/hyper/rustls: redirect, cookie, proxy (PAC, WPAD), sertifikat CA dan klien, HTTP/1.1 dan HTTP/2, Unix socket dan named pipe, body besar dialirkan ke disk; klien WebSocket |
| `merpati-auth` | Semua jenis autentikasi, dari API key sampai OAuth 2.0, AWS SigV4, NTLM, EdgeGrid, dan ASAP |
| `merpati-script` | Mesin skrip di atas boa (JavaScript Rust murni): API `pm`, Chai, `pm.sendRequest`, paket, sandbox, dan proses pekerja |
| `merpati-runner` | Alur request yang dipakai bersama oleh aplikasi, Runner, dan CLI; Collection Runner, performance test, reporter |
| `merpati-convert` | Importer cURL, OpenAPI/Swagger, Insomnia, HAR, Hoppscotch, Thunder Client, WSDL, dan Smithy; ekspor OpenAPI; generator kode; skema GraphQL; dokumentasi |
| `merpati-mock` | Mock server dari example, simulasi jaringan dan kegagalan; listener webhook |
| `merpati-grpc` | Klien gRPC dinamis: kompilasi `.proto` atau reflection, keempat jenis panggilan, mTLS |
| `merpati-mqtt` | MQTT 3.1.1/5 lewat TCP, TLS, dan WebSocket |
| `merpati-socketio` | Klien Socket.IO v2–v4 (Engine.IO 3/4, polling dan WebSocket) |
| `merpati-mcp` | Klien Model Context Protocol (stdio, Streamable HTTP, SSE) dengan OAuth, sampling, dan elicitation |
| `merpati-ai` | Request chat ke OpenAI, Anthropic, Gemini, dan server kompatibel OpenAI, streaming, tools MCP |
| `merpati-flows` | Flows: model, penyimpanan, validasi, dan eksekutor |
| `merpati-secrets` | HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, dan 1Password di balik `{{vault:nama}}` |
| `merpati-proxy` | Proxy penangkap: intersepsi HTTP/HTTPS dengan CA lokal, ekspor HAR |
| `merpati-html` | Perender subset HTML/CSS untuk egui (visualizer, pratinjau dokumentasi) |
| `merpati-cli` | Binary `merpati` |
| `merpati-app` | Aplikasi desktop (egui/eframe di atas wgpu) |

Alurnya sama di mana pun: skrip pre-request (koleksi → folder → request), resolusi variabel,
autentikasi, kirim (dengan pengulangan challenge Digest dan NTLM), lalu skrip post-response dan
tes. Run di terminal berperilaku sama seperti run di aplikasi.

Juga di repositori: `docs/` (status setiap persyaratan, [panduan desain](docs/design.md),
[scripting](docs/scripting.md)), `scripts/` (packaging, installer, benchmark, pemeriksaan i18n dan
aksesibilitas), `packaging/` (Info.plist, ikon, cask Homebrew, `.desktop` Linux), `docker/`, dan
`action.yml`.

## Pengembangan

```sh
cargo test --workspace                       # sekitar 1.065 tes, dengan server uji HTTP/WS/gRPC/MQTT/Socket.IO/MCP/proxy lokal
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
scripts/bench.sh                             # target performa di atas (macOS)
python3 scripts/i18n-scan.py                 # teks UI yang tidak lewat sistem terjemahan
swift scripts/ax-audit.swift <pid>           # pohon aksesibilitas seperti yang dibaca VoiceOver
```

CI menjalankan pemeriksaan format, clippy, dan tes di macOS dan Linux, serta clippy di Windows. Tes
yang butuh layanan sungguhan (socket.io, mosquitto, HashiCorp Vault, Ollama, registry npm) diberi
`#[ignore]`; jalankan dengan `-- --ignored` setelah layanan yang disebutkan menyala.

- **UI**: baca [docs/design.md](docs/design.md) dulu; semua layar memakai token dan komponen
  bersama.
- **Terjemahan**: teks yang dibaca pengguna lewat `tr()`/`trf()` dengan entri bahasa Indonesia di
  `crates/merpati-app/i18n/id.tsv` (sekitar 1.670 entri); sebuah tes gagal bila ada yang belum
  diterjemahkan.
- **Variabel lingkungan**: `MERPATI_CONFIG_DIR` (folder pengaturan lain),
  `MERPATI_SECRETS=file|memory` (tanpa prompt Keychain untuk build yang tidak ditandatangani), dan
  `MERPATI_AUTOMATION=steps.json` (menggerakkan aplikasi dari daftar langkah dan mengambil tangkapan
  layar; lihat `crates/merpati-app/src/automation.rs`).
- **Server uji** untuk tab protokol: `cargo run -p merpati-grpc --example greeter_server`,
  `cargo run -p merpati-socketio --example echo_server -- 3000`, dan `target/debug/fake_mcp_server`
  (server MCP stdio, dibangun dengan `cargo build -p merpati-mcp`).

Issue dan pull request dipersilakan.

## Batasan

- Build rilis belum ditandatangani dengan Apple Developer ID dan belum dinotarisasi, jadi macOS
  memberi peringatan bila diunduh lewat browser; lihat catatan di bagian [Instalasi](#disk-image).
- macOS adalah tempat Merpati dibangun, diukur, dan diuji langsung. Build Linux dan Windows berasal
  dari CI dan kurang teruji: Linux menjalankan rangkaian tes (kecuali GUI) di CI, sedangkan build
  Windows baru dikompilasi dan diperiksa clippy, belum pernah dijalankan di mesin Windows
  sungguhan.
- Diuji dengan layanan sungguhan: Ollama, HashiCorp Vault, socket.io, mosquitto, server referensi
  MCP, dan gRPC. OpenAI, Anthropic, dan Gemini dengan kunci cloud, AWS Secrets Manager, Azure Key
  Vault, 1Password, dan penyedia OAuth komersial untuk MCP baru diuji dengan mock.
- Proxy: di Linux hanya variabel lingkungan yang dibaca; request HTTP biasa lewat proxy yang
  mewajibkan autentikasi Digest gagal (tunnel untuk WebSocket, Socket.IO, MQTT, dan gRPC
  mendukungnya); autentikasi proxy NTLM/Negotiate tidak didukung.
- Proxy penangkap tidak menangani HTTP/3 atau aplikasi yang memakai certificate pinning.
- Visualizer mendukung subset HTML dan CSS; halaman dengan skrip (misalnya Chart.js) dibuka di
  browser.
- Dependensi Smithy hanya dibaca dari repositori Maven lokal (tidak diunduh), dan plugin serta
  projection `smithy-build.json` tidak dipakai.
- Fitur tim dari platform API berbasis cloud (sinkronisasi cloud, monitor, workspace publik) di luar
  cakupan: workspace adalah folder yang Anda bagikan lewat Git, `merpati schedule` menjalankan
  koleksi berkala, dan webhook diterima secara lokal.

## Lisensi

Merpati berlisensi ganda [MIT](LICENSE-MIT) atau [Apache-2.0](LICENSE-APACHE), sesuai pilihan Anda.

Pustaka JavaScript yang dibundel tetap memakai lisensinya sendiri, tercantum di
`crates/merpati-script/js/vendor/THIRD_PARTY_LICENSES.txt`. Font Inter dan JetBrains Mono yang
dibundel berlisensi SIL Open Font License 1.1 dan ikon Phosphor berlisensi MIT; berkas lisensinya ada
di `crates/merpati-app/assets/fonts`.

Postman adalah merek dagang Postman, Inc. Merpati tidak berafiliasi dengan Postman dan tidak
memakai nama, logo, atau aset Postman di dalam aplikasi.
