# PRD: Klien API Ringan (Full Rust) setara Postman

Oct 23, 2026 · @Tommy Lumos Empire

## 1. Ringkasan

Produk ini adalah aplikasi desktop klien API yang ditulis 100% Rust, termasuk UI dan semua klien protokol, dengan paritas fitur terhadap Postman untuk sisi klien. Tujuannya menggantikan Postman bagi pengguna yang terganggu oleh pemakaian memori, waktu start, dan ketergantungan pada akun dan cloud.

Nama kerja sementara: **Merpati**.

**Masalah.** Postman adalah platform besar dengan 18 area fitur, dari request builder sampai governance dan AI. Aplikasinya berat, dan banyak fitur terikat ke akun dan layanan cloud Postman.

**Peluang.** Satu binary native, cepat, tanpa akun, yang menyimpan data sebagai berkas lokal dan bisa mengimpor koleksi Postman yang sudah ada.

**Asumsi cakupan.** PRD ini mengasumsikan "100 persen" berarti seluruh fitur klien yang berdiri sendiri (kelompok A dan B di [peta fitur Postman](https://claude.ai/artifact/Rg41Eq7dGQ4fEKAs6bzLE5)). Fitur yang butuh server Postman (sinkronisasi tim, monitor cloud, webhook publik, API Network, governance) tidak dibuat pada fase awal; padanan lokalnya disebut di bagian 4, dan versi server menjadi fase opsional. Asumsi ini bisa diubah di bagian pertanyaan terbuka.

**Dasar paritas.** Daftar fitur diambil dari dokumentasi resmi Postman dan dipetakan di bagian 4 sampai 7. Aplikasi ini tidak memakai nama, logo, atau aset Postman.

## 2. Tujuan dan target terukur

Target utama: aplikasi dengan 5 tab request terbuka memakai kurang dari 150 MB RAM dan siap dipakai kurang dari 1 detik setelah dibuka. Angka di bawah adalah target desain awal, bukan hasil pengukuran, dan diverifikasi lewat benchmark otomatis.

| Metrik | Target | Cara ukur |
| --- | --- | --- |
| Memori idle, 1 workspace | < 100 MB | Memori fisik di Activity Monitor / `ps` |
| Memori, 5 tab request aktif | < 150 MB | Skenario benchmark tetap |
| Waktu start dingin | < 1 detik | Sampai jendela bisa diketik |
| Ukuran binary | < 40 MB | Build rilis, termasuk mesin skrip |
| Membuka workspace dengan 10.000 request | < 1 detik untuk daftar koleksi tampil | Data uji sintetis |
| Respons 100 MB | UI tetap responsif; isi di-stream ke penyimpanan sementara, hanya bagian yang terlihat yang dimuat | Server uji lokal |
| Collection Runner 1.000 iterasi | Tidak ada pertumbuhan memori linear | Pemantauan memori selama run |
| Overhead aplikasi per request | < 5 ms di luar waktu jaringan | Server lokal tanpa latensi |
| CPU idle | \~0% | Render hanya saat ada perubahan |

**Tujuan fungsional**

- Setiap fitur di kelompok A dan B pada peta fitur tersedia dengan perilaku yang setara.
- Koleksi, environment, dan variabel Postman bisa diimpor, dan skrip di dalamnya berjalan tanpa perubahan untuk kasus pemakaian umum.
- Seluruh aplikasi berjalan tanpa akun, tanpa koneksi ke server pihak lain, dan tanpa Node.js atau runtime tambahan.
- Data pengguna disimpan sebagai berkas teks yang rapi untuk Git.

**Non-tujuan**

- Tidak membangun sinkronisasi cloud, workspace tim, monitor cloud, API Network, governance, audit log, atau SSO pada fase awal.
- Tidak membangun Agent Mode atau layanan AI milik sendiri; fitur AI memakai kunci penyedia milik pengguna.
- Tidak menjamin kompatibilitas dengan semua paket npm yang dipakai skrip Postman; cakupannya dijelaskan di bagian 8.
- Tidak memakai nama, logo, atau aset Postman.

## 3. Target pengguna

Pengguna utama adalah developer yang membangun atau memakai API setiap hari dan ingin klien yang cepat, ringan, dan tidak memaksa akun.

| Persona | Kebutuhan utama | Skenario khas |
| --- | --- | --- |
| Backend developer | Menguji endpoint sendiri, menyimpan request, memakai environment untuk lokal, staging, dan produksi | Mengirim request dengan token dari environment, memeriksa respons, lalu menyimpannya ke koleksi di repo Git |
| QA / SDET | Tes otomatis, data-driven, dan dijalankan di CI | Menulis tes di skrip, menjalankan koleksi dengan file CSV, mengekspor laporan JUnit di pipeline |
| Frontend dan mobile developer | Mock server saat backend belum siap, mencoba GraphQL, WebSocket, dan gRPC | Membuat mock dari example respons dan menyambungkan klien ke mock tersebut |
| Pengguna Postman yang ingin beralih | Memindahkan koleksi dan environment tanpa mengulang | Mengimpor koleksi dan menjalankan tes yang sudah ada |
| Pengguna yang menghindari akun dan cloud | Privasi dan kontrol data | Menyimpan semua data lokal dan memakai Vault lokal untuk rahasia |

**Skenario utama**

1. Pengguna membuka aplikasi, membuat request, memilih environment, mengirim, dan melihat respons dalam hitungan detik.
2. Pengguna menyimpan request ke koleksi, menambahkan tes pada respons, lalu menjalankan seluruh koleksi dengan Collection Runner.
3. Pengguna mengimpor koleksi Postman, menjalankannya, dan mendapat hasil tes yang sama.
4. Pengguna menjalankan koleksi yang sama dari terminal atau CI lewat CLI dan mendapat laporan.

## 4. Lingkup paritas per fase

Semua fitur kelompok A dan B di peta fitur menjadi target, dibagi tiga fase. Fase 1 sudah cukup untuk pemakaian harian sebagai klien HTTP; fase 2 menambah otomasi dan protokol lain; fase 3 menutup sisa paritas. Fitur yang butuh server menjadi fase 4 yang opsional.

| Area | Fase 1: inti | Fase 2: lanjutan | Fase 3: paritas penuh |
| --- | --- | --- | --- |
| Request HTTP | Semua metode, params, header, body (form-data, urlencoded, raw, binary), kirim dan batalkan, penampil respons | Pengaturan request lanjutan, cookie jar, sertifikat klien dan CA, proxy, riwayat | Penangkapan lalu lintas lewat proxy |
| Autentikasi | No Auth, API key, Bearer, Basic, pewarisan dari koleksi dan folder | JWT bearer, Digest, OAuth 2.0 (semua grant type), OAuth 1.0 | Hawk, AWS Signature, NTLM, Akamai EdgeGrid, ASAP |
| Koleksi | Koleksi, folder bertingkat, simpan dan buka, impor koleksi dan environment Postman | Impor cURL dan OpenAPI, ekspor, example respons, duplikasi dan fork lokal | Impor dari SoapUI, Insomnia, Hoppscotch, Thunder Client |
| Variabel | Lima cakupan, environment, variabel dinamis | Vault lokal dan tipe secret | Integrasi 1Password, AWS Secrets Manager, Azure Key Vault, HashiCorp Vault |
| Skrip dan tes |  | Pre-request dan post-response, `pm.test`, `pm.expect`, console | Pustaka lengkap, paket npm dan JSR, Package Library lokal |
| Runner dan CLI |  | Collection Runner, file data CSV dan JSON, CLI dengan reporter JUnit | Performance test, urutan run kustom, simulasi kegagalan, penjadwal lokal |
| Mock |  | Mock server lokal dari example | Respons dinamis, algoritma pencocokan lanjutan, simulasi kondisi jaringan |
| Protokol lain |  | GraphQL, WebSocket | gRPC (empat pola), Socket.IO, MQTT, SOAP, unix socket, request MCP dan AI |
| Desain API dan dokumentasi |  | Impor OpenAPI | Spec Hub lokal (OpenAPI, AsyncAPI, protobuf, GraphQL, Smithy), dokumentasi statis, Flows lokal |
| Antarmuka | Tema terang dan gelap, pintasan keyboard, banyak tab | Palet perintah, kustomisasi | Lokalisasi, aksesibilitas lengkap |

**Fase 4 (opsional, butuh server).** Sinkronisasi dan workspace tim, monitor cloud, webhook publik, flows ter-deploy, API Network, governance, dan SSO. Penjelasan dan padanannya ada di bagian 4 peta fitur.

## 5. Persyaratan fungsional: request, koleksi, dan data

Fase penempatan mengikuti bagian 4.

### 5.1 Request builder HTTP

- **FR-R1** Pengguna membuat request dengan metode standar (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS) dan metode kustom; URL tanpa protokol otomatis diberi `http://`.
- **FR-R2** Parameter query dan path diedit di tab Params dan tersinkron dengan URL; setiap baris bisa dinonaktifkan.
- **FR-R3** Header diedit sebagai tabel dengan autocomplete nama header; header otomatis (Host, Content-Length, dan sejenisnya) bisa dilihat.
- **FR-R4** Body: none, form-data (teks dan berkas), x-www-form-urlencoded, raw (JSON, XML, teks, JavaScript, HTML), binary, dan GraphQL; editor menyorot sintaks dan memformat JSON.
- **FR-R5** Kirim, batalkan request yang berjalan, dan simpan otomatis perubahan request; setiap request menampilkan status, waktu, dan ukuran respons.
- **FR-R6** Pengaturan per request: ikuti redirect, verifikasi sertifikat SSL, encoding URL, timeout, dan versi HTTP (HTTP/1.1 dan HTTP/2).
- **FR-R7** Cookie jar per domain yang bisa dilihat dan diubah; cookie dikirim dan disimpan otomatis sesuai aturan standar.
- **FR-R8** Sertifikat CA kustom dan sertifikat klien per domain; proxy sistem dan proxy kustom (HTTP, HTTPS, SOCKS).
- **FR-R9** Riwayat request yang bisa dicari dan dipulihkan.
- **FR-R10** Menghasilkan potongan kode dari request ke cURL, Rust, JavaScript, Python, Go, dan bahasa lain.

### 5.2 Autentikasi

- **FR-A1** Tipe auth: No Auth, API key (header atau query), Bearer, JWT bearer, Basic, Digest, OAuth 1.0, OAuth 2.0, Hawk, AWS Signature, NTLM, Akamai EdgeGrid, dan ASAP.
- **FR-A2** OAuth 2.0 mendukung semua grant type umum (authorization code dengan PKCE, client credentials, password, implicit, refresh token), mengambil dan menyimpan token, dan menyegarkannya otomatis.
- **FR-A3** Auth bisa diatur di level koleksi, folder, atau request dan diwarisi ke bawah; nilai auth boleh memakai variabel.
- **FR-A4** Detail auth yang dihitung (header, query) tidak disimpan ke berkas agar rahasia tidak bocor.

### 5.3 Respons

- **FR-P1** Tab respons: body (Pretty, Raw, Preview), header, cookie, dan hasil tes; pencarian di dalam respons.
- **FR-P2** Pretty menampilkan JSON, XML, dan HTML dengan penyorotan, pelipatan, dan pencarian; respons besar di-stream dan ditampilkan secara virtual.
- **FR-P3** Penyimpanan respons ke berkas, serta example (pasangan request dan respons) yang disimpan di koleksi.
- **FR-P4** Visualizer HTML sederhana yang dikendalikan skrip (fase 3).

### 5.4 Koleksi dan organisasi

- **FR-C1** Koleksi berisi folder bertingkat dan request; semuanya bisa dipindah, diurutkan, diduplikasi, diganti nama, dan dihapus lewat drag-and-drop.
- **FR-C2** Level koleksi dan folder memiliki auth, skrip, variabel, dan deskripsi sendiri.
- **FR-C3** Penyimpanan koleksi sebagai pohon berkas teks di folder workspace (satu berkas per request), supaya diff Git terbaca.
- **FR-C4** Impor dan ekspor koleksi dan environment Postman (format Collection v2.1), cURL, dan OpenAPI; fase 3 menambah SoapUI, Insomnia, Hoppscotch, dan Thunder Client.
- **FR-C5** Pencarian di seluruh workspace (nama, URL, isi).

### 5.5 Variabel, environment, dan Vault

- **FR-V1** Variabel kunci-nilai dengan lima cakupan: global, koleksi, environment, data, dan lokal; jika nama sama, cakupan paling sempit yang dipakai.
- **FR-V2** Referensi `{{nama}}` di URL, header, body, auth, dan skrip, dengan penyorotan dan tooltip nilai saat diarahkan kursor.
- **FR-V3** Environment yang bisa diganti cepat; satu environment aktif per waktu; nilai default dan nilai lokal yang tidak ikut ke Git.
- **FR-V4** Variabel bertipe secret yang disembunyikan di UI dan tidak ditulis ke berkas yang di-commit.
- **FR-V5** Variabel dinamis (misalnya `{{$guid}}`, `{{$timestamp}}`, `{{$randomInt}}`, data acak mirip Faker).
- **FR-V6** Vault lokal di Keychain untuk rahasia, dirujuk lewat `{{vault:nama}}`; tidak pernah ditulis ke berkas koleksi.

## 6. Persyaratan fungsional: skrip, otomasi, dan mock

### 6.1 Skrip dan tes

- **FR-S1** Skrip pre-request dan post-response di level request, folder, dan koleksi; urutan eksekusi: koleksi, lalu folder, lalu request, baik untuk pre-request maupun post-response.
- **FR-S2** Skrip memakai bahasa JavaScript dengan objek `pm` yang kompatibel dengan Postman (rincian di bagian 8); skrip berjalan terisolasi, asinkron, dan tidak bisa mengakses sistem berkas atau jaringan selain lewat API yang disediakan.
- **FR-S3** Tes ditulis dengan `pm.test` dan `pm.expect` bergaya Chai BDD; hasil tampil di tab Test Results dengan status lulus atau gagal per tes.
- **FR-S4** Skrip bisa membaca dan mengubah variabel di semua cakupan, mengubah request sebelum dikirim, dan memakai `pm.sendRequest` untuk request tambahan.
- **FR-S5** Console menampilkan log skrip, request, dan respons untuk debugging, lengkap dengan urutan eksekusi.
- **FR-S6** Snippet siap pakai dan autocomplete untuk API `pm`; skrip bisa dijalankan ulang terhadap respons yang sudah ada tanpa mengirim ulang request.
- **FR-S7** Skrip untuk protokol lain mengikuti titik eksekusinya (GraphQL: sebelum query dan setelah respons; gRPC: sebelum invoke, saat pesan, setelah respons).

### 6.2 Collection Runner dan uji data

- **FR-U1** Menjalankan koleksi atau folder berurutan dengan pilihan environment, jumlah iterasi, jeda antar request, dan penyimpanan cookie.
- **FR-U2** File data CSV dan JSON menjadi variabel data tiap iterasi; satu iterasi per baris atau objek.
- **FR-U3** Skrip dapat mengatur request berikutnya, melompati, atau menghentikan run (setara `setNextRequest`).
- **FR-U4** Hasil run menampilkan tes lulus dan gagal per request dan per iterasi, waktu, dan ukuran respons, serta bisa diekspor sebagai JSON atau JUnit.
- **FR-U5** Run dapat dijalankan di latar belakang dan dibatalkan; penyimpanan hasil tidak menumbuhkan memori secara linear.
- **FR-U6** Opsi run dengan urutan kustom dan simulasi kegagalan jaringan (fase 3).
- **FR-U7** Performance test: menjalankan request dengan jumlah pengguna virtual dan durasi tertentu, lalu menampilkan latensi, throughput, dan tingkat galat (fase 3).

### 6.3 CLI dan CI

- **FR-L1** Binary CLI terpisah menjalankan koleksi dengan opsi environment, file data, jumlah iterasi, timeout, dan variabel dari baris perintah.
- **FR-L2** Reporter CLI, JSON, JUnit, dan HTML; kode keluar bukan nol saat ada tes gagal agar bisa menggagalkan pipeline.
- **FR-L3** CLI bisa membuka, memvalidasi, dan mengonversi koleksi, serta dipaketkan sebagai Docker image dan GitHub Action (fase 3).
- **FR-L4** Penjadwal lokal menjalankan koleksi pada jadwal tertentu dan menyimpan hasilnya (fase 3); sebagai ganti monitor cloud, bisa dipanggil juga dari cron dan CI.

### 6.4 Mock server

- **FR-M1** Mock server lokal dibuat dari koleksi atau example; request yang masuk dicocokkan dengan metode, path, dan parameter, lalu dibalas dengan example yang sesuai.
- **FR-M2** Respons dinamis dengan variabel dan templat, jeda buatan, dan simulasi kegagalan (fase 3).
- **FR-M3** Mock server bisa dijalankan dari UI dan dari CLI; semua request yang diterima dicatat dan bisa dilihat.
- **FR-M4** Pengguna dapat mengganti base URL request ke mock server dengan satu aksi.

## 7. Persyaratan fungsional: protokol lain dan desain API

### 7.1 Protokol

- **FR-X1 GraphQL** Penjelajah skema lewat introspeksi, editor query dengan autocomplete dan variabel, serta pengiriman operasi query, mutation, dan subscription.
- **FR-X2 gRPC** Memuat definisi `.proto` atau memakai server reflection, menjelajahi layanan dan method, serta memanggil keempat pola: unary, client streaming, server streaming, dan dua arah; metadata, TLS, dan pesan dalam JSON.
- **FR-X3 WebSocket** Koneksi persisten dengan header kustom, kirim dan terima pesan (teks dan biner), log pesan berstempel waktu, dan penyimpanan pesan sebagai bagian dari koleksi.
- **FR-X4 Socket.IO** Koneksi dengan event dan namespace, mendengarkan beberapa event, dan mengirim event dengan argumen.
- **FR-X5 MQTT** Koneksi ke broker (versi 3.1.1 dan 5), subscribe dengan QoS, publish pesan, dan log pesan.
- **FR-X6 SOAP** Pengiriman lewat HTTP dengan templat amplop XML dan impor WSDL untuk membuat operasi.
- **FR-X7 Unix socket dan named pipe** Mengirim request HTTP ke socket lokal (macOS dan Linux) atau named pipe (Windows).
- **FR-X8 MCP** Klien MCP yang terhubung ke server lokal (stdio) maupun jarak jauh, menjelajahi tool, resource, dan prompt, memanggilnya, dan mengekspor konfigurasi server untuk host MCP lain.
- **FR-X9 Request AI** Memanggil model dari penyedia yang kompatibel dengan OpenAI, Anthropic, dan Google, termasuk model lokal, dengan kunci milik pengguna yang disimpan di Vault; bisa melampirkan server MCP; hasilnya bisa dijalankan di Collection Runner.
- **FR-X10 Webhook (lokal)** Listener lokal yang mencatat event masuk (header, body, waktu), bisa diputar ulang; URL publik hanya tersedia lewat tunnel yang disiapkan pengguna.

### 7.2 Desain API dan dokumentasi (fase 3)

- **FR-D1** Editor spesifikasi OpenAPI, AsyncAPI, protobuf, GraphQL, dan Smithy dengan pemeriksaan sintaks dan validasi aturan, serta pratinjau dokumentasi langsung.
- **FR-D2** Membuat koleksi dari spesifikasi (folder, request, dan example) dan membuat spesifikasi OpenAPI dari koleksi, dengan sinkronisasi dua arah yang bisa dipicu manual.
- **FR-D3** Dokumentasi otomatis untuk setiap koleksi dengan deskripsi, contoh, dan potongan kode, diekspor sebagai situs statis HTML atau Markdown.
- **FR-D4** Flows lokal: kanvas visual untuk menyusun alur dari blok (request, kondisi, perulangan, skrip, dan blok AI), yang dijalankan dari aplikasi atau CLI.

## 8. Mesin skrip dan kompatibilitas Postman

Koleksi Postman menyimpan logika di skrip JavaScript yang berjalan di Postman Sandbox, runtime berbasis Node.js dengan objek `pm`. Agar koleksi yang diimpor tetap berjalan, klon membutuhkan mesin JavaScript tertanam dan lapisan kompatibilitas di atasnya. Ini keputusan teknis paling berpengaruh di seluruh proyek.

**Permukaan API yang harus ada.** Objek `pm` dengan `request`, `response`, `environment`, `globals`, `collectionVariables`, `variables`, `iterationData`, `test`, `expect`, `sendRequest`, `cookies`, `info`, dan kontrol alur run; objek `console`; serta pustaka bawaan Postman Sandbox seperti Chai untuk asersi dan generator data Faker. Daftar pasti API dan pustaka bawaan harus dicocokkan dengan dokumentasi Postman saat implementasi; daftar di sini berdasarkan pengetahuan umum dan belum diverifikasi sepenuhnya.

**Pilihan mesin JavaScript (kandidat, perlu dievaluasi)**

| Opsi | Rust murni | Pertimbangan |
| --- | --- | --- |
| `boa_engine` | Ya | Cocok dengan prinsip full Rust; cakupan standar ECMAScript dan kecepatan lebih terbatas, jadi harus diuji terhadap koleksi nyata |
| QuickJS lewat `rquickjs` | Tidak (pustaka C yang dibundel) | Kecil, cepat dimulai, cakupan ES modern baik; melanggar kemurnian Rust tetapi ukuran kecil |
| V8 lewat `deno_core` | Tidak (C++ besar) | Kompatibilitas dan kecepatan terbaik; menambah banyak ukuran binary dan waktu build, sulit memenuhi target ukuran |

**Keputusan dan cara memutuskannya**

- Antarmuka `ScriptEngine` dibuat sebagai trait sehingga mesin bisa diganti tanpa mengubah aplikasi.
- Prototipe pertama memakai `boa_engine`; jika korpus uji kompatibilitas (lihat di bawah) tidak lulus pada ambang yang ditetapkan, beralih ke QuickJS.
- Korpus uji berisi koleksi Postman publik yang representatif; hasil tes tiap koleksi dibandingkan dengan hasil yang diharapkan dari Postman.

**Batasan dan keamanan**

- Skrip tidak boleh mengakses sistem berkas, proses, atau jaringan, kecuali lewat `pm.sendRequest` dan API yang disediakan aplikasi.
- Setiap eksekusi punya batas waktu dan batas memori yang bisa diatur dan bisa dibatalkan.
- Modul bawaan Node.js (`fs`, `child_process`, dan sejenisnya) tidak didukung; skrip yang memakainya menampilkan galat yang jelas.
- Paket pihak ketiga: fase 2 hanya pustaka bawaan; fase 3 menambah muat paket JavaScript murni dari npm dan JSR yang di-bundle, tanpa modul native.

## 9. Persyaratan non-fungsional

| Kategori | Persyaratan |
| --- | --- |
| Platform | Prioritas macOS 12 ke atas (Apple Silicon dan Intel). Kode dijaga lintas platform agar Windows dan Linux bisa menyusul |
| Performa | Render berbasis perubahan; request dan skrip berjalan di luar thread UI; respons besar di-stream, bukan dimuat penuh ke memori |
| Memori | Isi respons besar disimpan di berkas sementara dan dimuat per bagian; riwayat dan hasil run memakai penyimpanan dengan batas ukuran |
| Penyimpanan data | Koleksi, environment, dan pengaturan sebagai berkas teks (JSON atau YAML) di folder workspace yang bisa di-commit ke Git; rahasia tidak pernah ikut di berkas tersebut |
| Keamanan | Rahasia dan token di Keychain; TLS terverifikasi secara default dengan pilihan eksplisit untuk mematikannya per request; skrip berjalan dalam sandbox; tidak ada kredensial di log atau berkas ekspor |
| Privasi | Tanpa akun, tanpa telemetri; satu-satunya koneksi keluar adalah ke server yang dituju pengguna dan, jika diaktifkan, penyedia AI pilihan pengguna |
| Keandalan | Kegagalan satu request atau skrip tidak menjatuhkan aplikasi; perubahan yang belum disimpan dipulihkan setelah penutupan paksa |
| Kompatibilitas | Mengimpor format Collection v2.1 dan environment Postman; mengekspor kembali dalam format yang sama |
| Kualitas kode | Unit test untuk parser dan penyusun request; uji integrasi terhadap server HTTP, gRPC, WebSocket, dan MQTT lokal; uji kompatibilitas skrip terhadap korpus koleksi; benchmark memori dan waktu start di CI |
| Distribusi | `.app` bertanda tangan dan ternotarisasi dalam DMG; instalasi lewat Homebrew cask; CLI sebagai binary terpisah |
| Lokalisasi | Antarmuka Inggris dan Indonesia; seluruh teks lewat sistem terjemahan |
| Aksesibilitas | Navigasi keyboard penuh untuk tab, tabel, dan editor; dukungan pembaca layar sebagai target fase 3 |

## 10. Arsitektur dan tech stack

Satu proses Rust: thread UI hanya menggambar state dan mengirim perintah, sedangkan request, skrip, dan akses berkas berjalan di runtime async terpisah. Pemisahan ini menjaga UI tetap responsif saat respons besar mengalir atau skrip berjalan lama.

&#91;embedded content: arsitektur modul · 3 lapisan, 9 komponen\]

Penyusun request menerapkan variabel dan auth, lalu mengirim lewat klien protokol; mesin skrip memodifikasi request sebelum dikirim dan menilai respons sesudahnya. Runner dan CLI memakai jalur yang sama dengan UI, sehingga hasil run di terminal sama dengan di aplikasi.

| Fungsi | Kandidat crate | Catatan |
| --- | --- | --- |
| Async runtime | `tokio` | Pemisahan I/O dari thread UI |
| HTTP, HTTP/2 | `hyper`, `reqwest` | Dengan `rustls` untuk TLS; kontrol penuh atas redirect, cookie, dan proxy |
| Cookie | `cookie_store` | Cookie jar per domain |
| gRPC | `tonic`, `prost-reflect` | Pemanggilan dinamis dari berkas `.proto` atau reflection |
| WebSocket | `tokio-tungstenite` | Teks dan biner |
| Socket.IO | `rust_socketio` | Event dan namespace |
| MQTT | `rumqttc` | Versi 3.1.1 dan 5 |
| GraphQL | `graphql-parser` | Introspeksi dan penyorotan; transport lewat HTTP dan WebSocket |
| XML dan SOAP | `quick-xml` | Amplop XML dan WSDL |
| Autentikasi | `oauth2`, `jsonwebtoken`, `aws-sigv4`, `hawk` | Tipe lain (Digest, NTLM, EdgeGrid, ASAP) kemungkinan ditulis sendiri |
| Mesin skrip | `boa_engine` atau `rquickjs` | Lihat bagian 8 |
| Mock server lokal | `axum` | Berbasis `hyper` |
| Penyimpanan | `serde`, `serde_json` | Berkas koleksi dan environment |
| Spesifikasi API | `openapiv3`, `protobuf-parse` | Impor dan pembuatan koleksi |
| Rahasia | `keyring` | macOS Keychain |
| CLI | `clap` | Biner CLI terpisah |
| Laporan | `quick-junit` | Keluaran JUnit |
| Editor teks | `ropey`, `syntect` | Buffer dan penyorotan sintaks |
| Templat kode | `minijinja` | Pembuat potongan kode |
| Logging | `tracing` | Tanpa kredensial di log |
| UI | Belum diputuskan | Lihat pertanyaan terbuka di bagian 15 |

## 11. Desain UX

Tata letak mengikuti pola yang sudah dikenal pengguna Postman: sidebar koleksi di kiri, tab request di tengah, dan respons di bawah request. Tujuannya agar pengguna yang beralih tidak perlu belajar ulang.

**Layout**

- Sidebar kiri: tab Koleksi (pohon dengan pencarian), Environment, dan Riwayat; bisa disembunyikan.
- Area tengah: tab untuk request dari semua protokol, runner, dan editor spesifikasi; tab bisa dipindah dan dibagi.
- Tab request: bar URL dengan pemilih metode dan tombol Kirim di atas, lalu tab Params, Auth, Headers, Body, Skrip, dan Pengaturan; respons di bawah (atau di samping, bisa dialihkan) dengan tab Body, Header, Cookie, dan Hasil Tes.
- Pemilih environment di pojok kanan atas, dan tombol Console di bar status.
- Mengikuti mode terang dan gelap macOS.

**Pintasan utama (usulan bawaan, dapat diubah)**

| Aksi | Pintasan |
| --- | --- |
| Kirim request | `⌘↩` |
| Request baru | `⌘N` |
| Tab baru / tutup tab | `⌘T` / `⌘W` |
| Cari di koleksi | `⌘K` |
| Tampilkan atau sembunyikan sidebar | `⌘B` |
| Ganti environment | `⌘E` |
| Buka Console | `⌥⌘C` |
| Simpan | `⌘S` |
| Palet perintah | `⇧⌘P` |

**Prinsip**

- Alur terpendek: dari buka aplikasi sampai respons tampil harus bisa dilakukan hanya dengan mengetik URL dan menekan Enter.
- Variabel yang tidak terdefinisi ditandai merah dan variabel yang terdefinisi menampilkan nilainya saat kursor diarahkan.
- Operasi yang berjalan lama (run, performance test, mock server) tampil di panel tugas latar belakang dan bisa dihentikan.
- Galat jaringan menyebut penyebab sebenarnya: DNS gagal, koneksi ditolak, sertifikat tidak valid, atau timeout.

## 12. Tahapan pengerjaan

Pengerjaan dibagi empat tahap berurutan, dan tiap tahap selesai hanya jika gerbang kualitasnya lolos. Tahap 0 membuktikan dua hal paling berisiko, yaitu kompatibilitas mesin skrip dan penanganan respons besar, sebelum fitur lain dibangun.

&#91;embedded content: roadmap · 4 tahap, 3 gerbang\]

Gerbang fase 3 adalah rilis dengan paritas penuh pada sisi klien; kriterianya ada di bagian 14. Fase 4 (fitur yang butuh server) dikerjakan hanya jika diputuskan di bagian pertanyaan terbuka. Jika Gate A gagal, keputusan mengganti mesin skrip atau toolkit UI diambil sebelum fase 1 dimulai.

## 13. Risiko dan mitigasi

Risiko terbesar ada pada kompatibilitas skrip dan pada kualitas komponen UI (editor kode dan penampil respons), bukan pada pengiriman request HTTP. Karena itu keduanya dibuktikan lebih dulu di tahap 0.

| Risiko | Dampak | Mitigasi |
| --- | --- | --- |
| Mesin JavaScript Rust murni tidak cukup lengkap atau cepat untuk skrip Postman nyata | Koleksi impor gagal atau berperilaku beda | Antarmuka `ScriptEngine` yang bisa diganti; korpus uji kompatibilitas; ambang lulus yang jelas di Gate A; cadangan QuickJS |
| Daftar API `pm` dan pustaka bawaan Postman belum diverifikasi penuh | Fitur yang hilang baru ketahuan belakangan | Susun daftar dari dokumentasi resmi di awal tahap 0 dan perbarui lewat hasil korpus uji |
| Paket npm yang dipakai skrip pengguna tidak bisa dimuat | Sebagian koleksi tidak berjalan | Dukung hanya pustaka JavaScript murni yang di-bundle; beri galat yang jelas dan daftar paket yang didukung |
| Penampil respons dan editor kode di UI Rust kurang matang | Pengalaman mengetik dan membaca respons buruk | Evaluasi komponen yang ada; cadangan: buffer `ropey` dengan render sendiri; uji IME sejak awal |
| Respons sangat besar menghabiskan memori | Gagal memenuhi alasan produk dibuat | Stream ke berkas sementara, tampilan virtual, dan benchmark 100 MB di Gate A |
| Klien protokol beragam (gRPC dinamis, Socket.IO, MQTT 5) punya perilaku halus | Bug khusus protokol | Server uji lokal per protokol di CI; implementasi bertahap sesuai fase |
| Format koleksi Postman berubah atau punya bagian tidak terdokumentasi | Impor tidak lengkap | Versi pembaca impor; laporan bagian yang tidak diimpor; fallback ke impor manual |
| OAuth 2.0 dan tipe auth khusus punya banyak variasi penyedia | Gagal autentikasi di layanan tertentu | Uji terhadap penyedia umum; opsi menempel token manual |
| "100 persen" meluas tanpa batas | Proyek tidak selesai | Peta fitur sebagai batas tetap; fitur di luar peta masuk PRD terpisah |
| Kemiripan dengan produk yang sudah ada | Risiko hukum atau merek | Tidak memakai nama, logo, atau aset Postman; implementasi dari perilaku yang terdokumentasi |

## 14. Kriteria keberhasilan dan rilis

Setiap fase dianggap selesai hanya jika semua kriteria di bawahnya lolos lewat uji otomatis atau benchmark yang bisa diulang.

**Gate A (tahap 0: risiko)**

- [ ] Respons 100 MB ditampilkan tanpa UI membeku dan dengan memori sesuai target di bagian 2
- [ ] Mesin skrip menjalankan korpus koleksi uji dengan tingkat lulus di atas ambang yang ditetapkan sebelum evaluasi dimulai
- [ ] Koleksi dan environment Postman v2.1 diimpor tanpa kehilangan field utama

**Gate B (fase 1: inti)**

- [ ] Target memori, waktu start, dan overhead per request di bagian 2 tercapai pada benchmark tetap
- [ ] Semua persyaratan fase 1 di bagian 5 lolos uji fungsional
- [ ] Ekspor lalu impor koleksi menghasilkan koleksi identik (uji round-trip)
- [ ] Tidak ada token, password, atau rahasia tersimpan polos di berkas koleksi atau log

**Gate C (fase 2: lanjutan)**

- [ ] Skrip pre-request, post-response, dan tes berjalan sama dengan Postman pada korpus uji
- [ ] Collection Runner dengan 1.000 iterasi tidak menumbuhkan memori secara linear
- [ ] CLI menghasilkan laporan JUnit dan kode keluar yang benar di pipeline contoh
- [ ] OAuth 2.0 bekerja untuk dua penyedia umum dan lewat token manual
- [ ] GraphQL, WebSocket, dan mock server lokal lolos uji integrasi

**Gate fase 3 (paritas penuh sisi klien)**

- [ ] Setiap fitur kelompok A dan B di peta fitur punya implementasi dan uji otomatis, atau tercatat alasan pengecualiannya
- [ ] gRPC (empat pola), MQTT, SOAP, dan MCP lolos uji terhadap server uji lokal
- [ ] Spec Hub lokal membuat koleksi dari OpenAPI dan sebaliknya

**Metrik setelah rilis**

- Jumlah isu terbuka berlabel `crash` dan `data-loss`; targetnya nol sebelum rilis stabil.
- Perbandingan memori dan waktu start terhadap Postman pada skenario benchmark yang sama, diterbitkan bersama setiap rilis.

## 15. Pertanyaan terbuka

Lima keputusan ini menentukan arah teknis dan perlu dijawab sebelum pengerjaan dimulai.

- [ ] **Arti "100 persen"**: PRD ini mengasumsikan seluruh fitur klien (kelompok A dan B). Apakah fitur server (sinkronisasi, monitor cloud, webhook publik, API Network) perlu dibangun juga sebagai fase 4 dengan backend sendiri?
- [ ] **Mesin JavaScript**: Rust murni (`boa_engine`) dengan risiko kompatibilitas, atau QuickJS dengan pustaka C yang dibundel? Ini menentukan makna "full Rust" dan seberapa mulus koleksi lama berjalan.
- [ ] **Framework UI**: toolkit siap pakai (`egui`, `iced`, `slint`) atau renderer kustom untuk editor kode dan penampil respons? Rekomendasi awal: toolkit untuk kerangka, komponen editor dan penampil respons ditulis sendiri.
- [ ] **Platform**: macOS dulu atau lintas platform sejak fase 1?
- [ ] **Format penyimpanan dan nama**: JSON atau YAML untuk berkas koleksi di disk (selain impor Postman v2.1), lisensi proyek, dan nama final selain nama kerja Merpati; periksa ketersediaan nama di crates.io, GitHub, dan domain.

## 16. Sumber

PRD ini disusun dari [Peta Fitur Postman](https://claude.ai/artifact/Rg41Eq7dGQ4fEKAs6bzLE5), dokumen terpisah yang memuat inventaris fitur dan sumbernya. Data per 23 Oktober 2026.

Halaman dokumentasi resmi yang menjadi dasar (dibaca dari cuplikan hasil pencarian, bukan dibuka penuh):

- [Other API protocols in Postman](https://learning.postman.com/docs/use/send-requests/protocols/protocols)
- [Send a request with the Postman API client](https://learning.postman.com/docs/use/send-requests/create-requests/request-basics)
- [Authorization types](https://learning.postman.com/docs/use/send-requests/authorization/authorization-types/)
- [Store and reuse values using variables](https://learning.postman.com/docs/use/send-requests/variables/variables/)
- [Use scripts to add logic and tests](https://learning.postman.com/docs/postman/scripts/intro_to_scripts/)
- [Write scripts to test API response data](https://learning.postman.com/docs/postman/scripts/test-scripts/)
- [Set up a monitor in Postman](https://learning.postman.com/docs/postman/monitors/setting_up_monitor)
- [Design and build your APIs in Postman](https://learning.postman.com/docs/design-apis/overview)

Nama crate Rust di bagian 8 dan 10, daftar API objek `pm`, dan beberapa detail request (misalnya tipe body dan kode generator) berasal dari pengetahuan umum penulis dan belum diverifikasi ke registri atau dokumentasi. Verifikasinya menjadi bagian dari tahap 0.
